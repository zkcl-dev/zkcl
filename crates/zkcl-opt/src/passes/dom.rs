//! Shared dominator tree and natural-loop infrastructure for SSA passes.

use std::ops::Range;

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::OpEffects;

use super::cfg::{control_targets, rpo_with_indices, visit_control_targets};
use crate::ir::{BlockId, SsaFunction, Terminator};

/// Record one predecessor, at most once.
///
/// Predecessor tables list blocks, so a branch whose two arms target the same
/// block records one predecessor. Every push for one source block happens in
/// one visit, which puts any duplicate next to the entry already recorded.
fn push_predecessor(predecessors: &mut Vec<usize>, predecessor: usize) {
    if predecessors.last() != Some(&predecessor) {
        predecessors.push(predecessor);
    }
}

/// Cooper–Harvey–Kennedy dominance for a dense graph. The synthetic root has
/// an edge to each entry and is the only predecessor from outside the graph.
pub(super) struct IndexedDomTree {
    pub(super) children: Vec<Vec<usize>>,
    intervals: Vec<Range<usize>>,
}

impl IndexedDomTree {
    pub(super) fn compute(
        predecessors: &[Vec<usize>],
        rpo: &[usize],
        entries: impl IntoIterator<Item = usize>,
    ) -> Self {
        let root = predecessors.len();
        let mut rank = vec![usize::MAX; root + 1];
        rank[root] = 0;
        for (index, &block) in rpo.iter().enumerate() {
            rank[block] = index + 1;
        }

        let intersect = |mut first: usize, mut second: usize, idom: &[usize]| -> usize {
            while first != second {
                while rank[first] > rank[second] {
                    first = idom[first];
                }
                while rank[second] > rank[first] {
                    second = idom[second];
                }
            }
            first
        };

        let mut idom = vec![usize::MAX; root + 1];
        idom[root] = root;
        for entry in entries {
            idom[entry] = root;
        }
        let mut changed = true;
        while changed {
            changed = false;
            for &block in rpo {
                // A direct entry or a join already dominated only by the
                // synthetic root cannot acquire a closer dominator.
                if idom[block] == root {
                    continue;
                }
                let new_idom = predecessors[block]
                    .iter()
                    .copied()
                    .filter(|&pred| idom[pred] != usize::MAX)
                    .reduce(|first, second| intersect(first, second, &idom));
                if let Some(parent) = new_idom
                    && idom[block] != parent
                {
                    idom[block] = parent;
                    changed = true;
                }
            }
        }

        let mut children = vec![Vec::new(); root + 1];
        for &block in rpo {
            let parent = idom[block];
            debug_assert_ne!(parent, usize::MAX, "RPO includes an unreachable block");
            children[parent].push(block);
        }

        let mut intervals = vec![0..0; root + 1];
        let mut stack = vec![(root, false)];
        let mut next = 0;
        while let Some((block, exit)) = stack.pop() {
            if exit {
                intervals[block].end = next;
            } else {
                intervals[block].start = next;
                next += 1;
                stack.push((block, true));
                stack.extend(children[block].iter().rev().map(|&child| (child, false)));
            }
        }
        Self { children, intervals }
    }

    fn roots(&self) -> &[usize] {
        self.children.last().unwrap()
    }

    #[inline]
    pub(super) fn dominates(&self, a: usize, b: usize) -> bool {
        self.intervals[a].contains(&self.intervals[b].start)
    }
}

#[derive(Default)]
struct BlockDom {
    interval: Range<usize>,
    children: Vec<BlockId>,
    cfg_preds: Vec<BlockId>,
}

/// Dominance for whole-block optimizations. A non-local transfer can leave a
/// block before its last instruction, so each resume target is an independent
/// entry. No value or predicate from the source block is assumed at that entry.
pub(crate) struct DomTree {
    rpo: Vec<BlockId>,
    roots: Vec<BlockId>,
    blocks: FxHashMap<BlockId, BlockDom>,
}

impl DomTree {
    pub(crate) fn compute(func: &SsaFunction, block_map: &FxHashMap<BlockId, usize>) -> Self {
        let rpo = rpo_with_indices(func, block_map);
        let order: Vec<_> = rpo.iter().map(|bid| block_map[bid]).collect();
        let mut cfg_preds = vec![Vec::new(); func.blocks.len()];
        let mut entries = vec![false; func.blocks.len()];
        entries[block_map[&func.entry]] = true;
        for &index in &order {
            let block = &func.blocks[index];
            block.terminator.visit_targets(|target| {
                push_predecessor(&mut cfg_preds[block_map[&target.block]], index);
            });
            visit_control_targets(block, |target| entries[block_map[&target]] = true);
        }
        // The SSA reverse postorder also follows recovery edges. Its DFS tree
        // therefore visits each non-entry block after a normal predecessor.
        let entries = entries.iter().enumerate().filter_map(|(i, &entry)| entry.then_some(i));
        let dom = IndexedDomTree::compute(&cfg_preds, &order, entries);
        let block_id = |index: usize| func.blocks[index].id;
        let roots = dom.roots().iter().copied().map(block_id).collect();
        let blocks = order
            .into_iter()
            .map(|index| {
                (
                    block_id(index),
                    BlockDom {
                        interval: dom.intervals[index].clone(),
                        children: dom.children[index].iter().copied().map(block_id).collect(),
                        cfg_preds: cfg_preds[index].iter().copied().map(block_id).collect(),
                    },
                )
            })
            .collect();
        Self { rpo, roots, blocks }
    }

    #[inline]
    pub(crate) fn roots(&self) -> &[BlockId] {
        &self.roots
    }

    #[inline]
    pub(crate) fn rpo(&self) -> &[BlockId] {
        &self.rpo
    }

    #[inline]
    pub(crate) fn dominates(&self, a: BlockId, b: BlockId) -> bool {
        self.blocks.get(&a).zip(self.blocks.get(&b)).is_some_and(|(a, b)| {
            a.interval.contains(&b.interval.start)
        })
    }

    #[inline]
    pub(crate) fn strictly_dominates(&self, a: BlockId, b: BlockId) -> bool {
        a != b && self.dominates(a, b)
    }

    #[inline]
    pub(crate) fn children(&self, bid: BlockId) -> &[BlockId] {
        self.blocks.get(&bid).map_or(&[], |block| block.children.as_slice())
    }

    #[inline]
    pub(crate) fn cfg_preds(&self, bid: BlockId) -> &[BlockId] {
        self.blocks.get(&bid).map_or(&[], |block| block.cfg_preds.as_slice())
    }
}

/// One natural loop identified by a header and its dominating backedges.
pub(crate) struct NaturalLoop {
    pub header: BlockId,
    pub preheader: Option<BlockId>,
    pub body: FxHashSet<BlockId>,
    pub effects: OpEffects,
}

/// Natural loops discovered in an [`SsaFunction`], ordered by header RPO.
pub(crate) struct LoopInfo {
    pub loops: Vec<NaturalLoop>,
}

impl LoopInfo {
    pub(crate) fn compute(
        func: &SsaFunction,
        block_map: &FxHashMap<BlockId, usize>,
        dom: &DomTree,
    ) -> Self {
        let control_entries = control_targets(func);
        let mut backedges_by_header: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
        for &bid in dom.rpo() {
            let block = &func.blocks[block_map[&bid]];
            block.terminator.visit_targets(|t| {
                if dom.dominates(t.block, bid) {
                    backedges_by_header.entry(t.block).or_default().push(bid);
                }
            });
        }

        let mut loops = Vec::new();
        for &header in dom.rpo() {
            let Some(backedges) = backedges_by_header.remove(&header) else {
                continue;
            };
            let mut body = FxHashSet::default();
            body.insert(header);
            let mut stack = backedges.clone();
            while let Some(node) = stack.pop() {
                if body.insert(node) {
                    for &pred in dom.cfg_preds(node) {
                        if !body.contains(&pred) {
                            stack.push(pred);
                        }
                    }
                }
            }

            let mut effects = OpEffects::PURE;
            for &bid in &body {
                for inst in &func.blocks[block_map[&bid]].insts {
                    effects |= inst.op.effects();
                }
            }

            let preheader = if header != func.entry && !control_entries.contains(&header) {
                let outside: Vec<BlockId> = dom
                    .cfg_preds(header)
                    .iter()
                    .copied()
                    .filter(|pred| !dom.dominates(header, *pred))
                    .collect();
                if let [pre] = outside.as_slice() {
                    let pre_block = &func.blocks[block_map[pre]];
                    matches!(
                        &pre_block.terminator,
                        Terminator::Jump(target) if target.block == header
                    )
                    .then_some(*pre)
                } else {
                    None
                }
            } else {
                None
            };

            loops.push(NaturalLoop {
                header,
                preheader,
                body,
                effects,
            });
        }

        Self { loops }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BasicBlock, BranchTarget, ScopeId, ScopeTree, ValueId};
    use zkcl_bytecode::Arity;

    fn target(block: u32) -> BranchTarget {
        BranchTarget {
            block: BlockId(block),
            args: Vec::new(),
        }
    }

    fn block(id: u32, terminator: Terminator) -> BasicBlock {
        BasicBlock {
            id: BlockId(id),
            params: Vec::new(),
            insts: Vec::new(),
            terminator,
            terminator_pc: 0,
            entry_scope: ScopeId::ROOT,
            unwind_succs: Vec::new(),
        }
    }

    fn function(blocks: Vec<BasicBlock>) -> SsaFunction {
        SsaFunction {
            fixnum_bounds: (-1024, 1023),
            arity: Arity::ZERO,
            constant_count: 0,
            entry: BlockId(0),
            entry_params: Vec::new(),
            blocks,
            scopes: ScopeTree::new(),
            pc_to_block: Default::default(),
            next_value_id: 0,
        }
    }

    fn tree(func: &SsaFunction) -> DomTree {
        DomTree::compute(func, &func.block_index_map())
    }

    fn idom(dom: &DomTree, child: BlockId) -> BlockId {
        if dom.roots().contains(&child) {
            return child;
        }
        dom.rpo()
            .iter()
            .copied()
            .find(|&parent| dom.children(parent).contains(&child))
            .expect("reachable dominator tree node")
    }

    fn return_one() -> Terminator {
        Terminator::ReturnOne(ValueId(0))
    }

    fn branch(then_target: u32, else_target: u32) -> Terminator {
        Terminator::BranchBool {
            cond: ValueId(0),
            then_target: target(then_target),
            else_target: target(else_target),
        }
    }

    /// Predecessor blocks, read straight from the IR.
    fn predecessors(func: &SsaFunction) -> FxHashMap<BlockId, Vec<BlockId>> {
        let mut predecessors: FxHashMap<BlockId, Vec<BlockId>> = func
            .blocks
            .iter()
            .map(|block| (block.id, Vec::new()))
            .collect();
        for block in &func.blocks {
            for successor in func_successors(block) {
                predecessors.entry(successor).or_default().push(block.id);
            }
        }
        predecessors
    }

    fn func_successors(block: &BasicBlock) -> Vec<BlockId> {
        let mut successors = Vec::new();
        block
            .terminator
            .visit_targets(|target| successors.push(target.block));
        successors.extend(block.unwind_succs.iter().copied());
        successors
    }

    /// Dominators computed by intersecting predecessor sets, independently of the
    /// idom chain the production tree walks.
    fn dominators(func: &SsaFunction) -> FxHashMap<BlockId, FxHashSet<BlockId>> {
        let ids: Vec<BlockId> = func.blocks.iter().map(|block| block.id).collect();
        let everything: FxHashSet<BlockId> = ids.iter().copied().collect();
        let predecessors = predecessors(func);
        let mut dominators: FxHashMap<BlockId, FxHashSet<BlockId>> = ids
            .iter()
            .map(|&bid| {
                let initial = if bid == func.entry {
                    FxHashSet::from_iter([bid])
                } else {
                    everything.clone()
                };
                (bid, initial)
            })
            .collect();
        let mut changed = true;
        while changed {
            changed = false;
            for &bid in &ids {
                if bid == func.entry {
                    continue;
                }
                let mut next = everything.clone();
                for predecessor in &predecessors[&bid] {
                    next.retain(|candidate| dominators[predecessor].contains(candidate));
                }
                next.insert(bid);
                if dominators[&bid] != next {
                    dominators.insert(bid, next);
                    changed = true;
                }
            }
        }
        dominators
    }

    /// The deepest strict dominator: it dominates every other strict dominator.
    fn expected_idom(
        dominators: &FxHashMap<BlockId, FxHashSet<BlockId>>,
        bid: BlockId,
    ) -> BlockId {
        dominators[&bid]
            .iter()
            .copied()
            .filter(|&candidate| candidate != bid)
            .max_by_key(|&candidate| dominators[&candidate].len())
            .expect("a reachable block has a dominator")
    }

    #[test]
    fn the_entry_is_its_own_immediate_dominator() {
        let func = function(vec![block(0, return_one())]);
        let dom = tree(&func);
        assert_eq!(idom(&dom, BlockId(0)), BlockId(0));
    }

    #[test]
    fn a_self_loop_keeps_the_entry_as_its_own_immediate_dominator() {
        let func = function(vec![block(0, Terminator::Jump(target(0)))]);
        let dom = tree(&func);
        assert_eq!(dom.rpo().len(), 1);
        assert_eq!(dom.rpo()[0], BlockId(0));
        assert_eq!(idom(&dom, BlockId(0)), BlockId(0));
    }

    #[test]
    fn an_immediate_dominator_strictly_dominates_its_block() {
        let func = function(vec![
            block(0, branch(1, 2)),
            block(1, Terminator::Jump(target(3))),
            block(2, Terminator::Jump(target(3))),
            block(3, return_one()),
        ]);
        let dom = tree(&func);
        let dominators = dominators(&func);
        for &bid in dom.rpo() {
            let immediate = idom(&dom, bid);
            assert!(
                dominators[&bid].contains(&immediate),
                "block {bid:?} is not dominated by its immediate dominator {immediate:?}"
            );
            if bid == func.entry {
                continue;
            }
            assert!(
                dom.strictly_dominates(immediate, bid),
                "{immediate:?} must strictly dominate {bid:?}"
            );
        }
    }

    #[test]
    fn the_immediate_dominator_is_the_deepest_common_dominator() {
        let func = function(vec![
            block(0, branch(1, 2)),
            block(1, Terminator::Jump(target(3))),
            block(2, Terminator::Jump(target(3))),
            block(3, return_one()),
        ]);
        let dom = tree(&func);
        let sets = dominators(&func);
        for &bid in dom.rpo() {
            if bid == func.entry {
                continue;
            }
            assert_eq!(
                idom(&dom, bid),
                expected_idom(&sets, bid),
                "the immediate dominator of {bid:?} disagrees with the dominator sets"
            );
        }
    }

    #[test]
    fn a_loop_header_is_the_dominator_of_its_body() {
        let func = function(vec![
            block(0, Terminator::Jump(target(1))),
            block(1, branch(2, 3)),
            block(2, Terminator::Jump(target(1))),
            block(3, return_one()),
        ]);
        let dom = tree(&func);
        let sets = dominators(&func);
        assert_eq!(idom(&dom, BlockId(1)), BlockId(0));
        assert_eq!(idom(&dom, BlockId(2)), BlockId(1));
        assert_eq!(idom(&dom, BlockId(3)), BlockId(1));
        for &bid in dom.rpo() {
            if bid == func.entry {
                continue;
            }
            for &candidate in dom.rpo() {
                assert_eq!(dom.dominates(candidate, bid), sets[&bid].contains(&candidate));
            }
            assert_eq!(idom(&dom, bid), expected_idom(&sets, bid));
        }
    }

    #[test]
    fn a_resume_target_has_an_independent_dominator_subtree() {
        // Normal path 0 -> 1 -> 3, resume path 2 -> 3. Facts from the
        // terminator of 0 are unavailable on the resume path and at the join.
        let mut entry = block(0, Terminator::Jump(target(1)));
        entry.unwind_succs = vec![BlockId(2)];
        let func = function(vec![
            entry,
            block(1, Terminator::Jump(target(3))),
            block(2, Terminator::Jump(target(3))),
            block(3, Terminator::Jump(target(4))),
            block(4, return_one()),
            block(5, return_one()),
        ]);
        let dom = tree(&func);
        assert!(dom.dominates(BlockId(0), BlockId(1)));
        assert!(!dom.dominates(BlockId(0), BlockId(2)));
        assert!(!dom.dominates(BlockId(0), BlockId(3)));
        assert!(!dom.dominates(BlockId(2), BlockId(3)));
        assert!(dom.dominates(BlockId(3), BlockId(4)));
        assert!(!dom.dominates(BlockId(5), BlockId(5)));
        assert!(dom.cfg_preds(BlockId(2)).is_empty());
        assert_eq!(dom.roots().len(), 3);
    }

    #[test]
    fn a_branch_into_the_same_block_twice_is_one_predecessor() {
        let func = function(vec![block(0, branch(1, 1)), block(1, return_one())]);
        let dom = tree(&func);
        assert_eq!(dom.cfg_preds(BlockId(1)).len(), 1);
    }

    #[test]
    fn distinct_predecessors_are_kept() {
        // The other side of the dedup: collapsing equal neighbours must not
        // collapse two different predecessors.
        let func = function(vec![
            block(0, branch(1, 2)),
            block(1, Terminator::Jump(target(3))),
            block(2, Terminator::Jump(target(3))),
            block(3, return_one()),
        ]);
        let dom = tree(&func);
        // Predecessor order follows the DFS behind the RPO and is not a contract;
        // the passes that read this table only count it or iterate it.
        let mut predecessors = dom.cfg_preds(BlockId(3)).to_vec();
        predecessors.sort_unstable();
        assert_eq!(predecessors, vec![BlockId(1), BlockId(2)]);
    }
}
