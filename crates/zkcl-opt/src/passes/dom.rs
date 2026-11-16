//! Shared dominator tree and natural-loop infrastructure for SSA passes.

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::OpEffects;

use super::cfg::{control_targets, rpo_with_indices};
use crate::ir::{BlockId, SsaFunction, Terminator};

/// Dominator tree and CFG predecessor tables for an [`SsaFunction`].
pub(crate) struct DomTree {
    rpo: Vec<BlockId>,
    idom: FxHashMap<BlockId, BlockId>,
    children: FxHashMap<BlockId, Vec<BlockId>>,
    cfg_preds: FxHashMap<BlockId, Vec<BlockId>>,
}

impl DomTree {
    /// Compute the dominator tree using Cooper-Harvey-Kennedy iterative dataflow over RPO.
    pub(crate) fn compute(func: &SsaFunction, block_map: &FxHashMap<BlockId, usize>) -> Self {
        let rpo = rpo_with_indices(func, block_map);
        let mut rpo_index = FxHashMap::default();
        for (idx, &bid) in rpo.iter().enumerate() {
            rpo_index.insert(bid, idx);
        }

        let mut cfg_preds: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
        let mut all_preds: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
        for &bid in &rpo {
            cfg_preds.entry(bid).or_default();
            all_preds.entry(bid).or_default();
        }
        for b in &func.blocks {
            if !rpo_index.contains_key(&b.id) {
                continue;
            }
            b.terminator.visit_targets(|t| {
                if rpo_index.contains_key(&t.block) {
                    cfg_preds.entry(t.block).or_default().push(b.id);
                    all_preds.entry(t.block).or_default().push(b.id);
                }
            });
            for &u in &b.unwind_succs {
                if rpo_index.contains_key(&u) {
                    all_preds.entry(u).or_default().push(b.id);
                }
            }
        }

        let intersect =
            |mut b1: BlockId, mut b2: BlockId, idom: &FxHashMap<BlockId, BlockId>| -> BlockId {
                while b1 != b2 {
                    while rpo_index[&b1] > rpo_index[&b2] {
                        b1 = idom[&b1];
                    }
                    while rpo_index[&b2] > rpo_index[&b1] {
                        b2 = idom[&b2];
                    }
                }
                b1
            };

        let mut idom = FxHashMap::default();
        idom.insert(func.entry, func.entry);

        let mut changed = true;
        while changed {
            changed = false;
            for &bid in rpo.iter().skip(1) {
                let Some(p_list) = all_preds.get(&bid) else {
                    continue;
                };
                let mut new_idom: Option<BlockId> = None;
                for &p in p_list {
                    if !idom.contains_key(&p) {
                        continue;
                    }
                    new_idom = Some(match new_idom {
                        None => p,
                        Some(cur) => intersect(p, cur, &idom),
                    });
                }
                if let Some(d) = new_idom
                    && idom.get(&bid).copied() != Some(d)
                {
                    idom.insert(bid, d);
                    changed = true;
                }
            }
        }

        let mut children: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
        for &bid in &rpo {
            if let Some(&parent) = idom.get(&bid)
                && parent != bid
            {
                children.entry(parent).or_default().push(bid);
            }
        }

        Self {
            rpo,
            idom,
            children,
            cfg_preds,
        }
    }

    #[inline]
    pub(crate) fn rpo(&self) -> &[BlockId] {
        &self.rpo
    }

    #[inline]
    pub(crate) fn dominates(&self, a: BlockId, mut b: BlockId) -> bool {
        loop {
            if a == b {
                return true;
            }
            let Some(&parent) = self.idom.get(&b) else {
                return false;
            };
            if parent == b {
                return false;
            }
            b = parent;
        }
    }

    #[inline]
    pub(crate) fn strictly_dominates(&self, a: BlockId, b: BlockId) -> bool {
        a != b && self.dominates(a, b)
    }

    #[inline]
    pub(crate) fn children(&self, bid: BlockId) -> &[BlockId] {
        self.children.get(&bid).map_or(&[], Vec::as_slice)
    }

    #[inline]
    pub(crate) fn cfg_preds(&self, bid: BlockId) -> &[BlockId] {
        self.cfg_preds.get(&bid).map_or(&[], Vec::as_slice)
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
