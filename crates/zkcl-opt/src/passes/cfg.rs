use std::collections::hash_map::Entry;

use rustc_hash::{FxHashMap, FxHashSet};

use super::rewrite::{resolve_rewrite_chains, rewrite_all_uses};
use crate::ir::{
    BasicBlock, BlockId, BlockParam, BranchTarget, SsaFunction, SsaOp, Terminator, ValueId,
};

/// Registered local unwind recovery states shared by JIT and AOT lowering.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UnwindTable {
    targets: Vec<BlockId>,
    state_by_target: FxHashMap<BlockId, usize>,
    first_state_by_tagbody: FxHashMap<(BlockId, usize), usize>,
}

impl UnwindTable {
    /// Recovery blocks indexed by the native return record's state field.
    pub fn targets(&self) -> &[BlockId] {
        &self.targets
    }

    pub fn state_for(&self, target: BlockId) -> usize {
        self.state_by_target[&target]
    }

    /// `GO` adds the label index to this base to select a recovery block.
    pub fn tagbody_base(&self, block: BlockId, instruction_index: usize) -> usize {
        self.first_state_by_tagbody[&(block, instruction_index)]
    }
}

/// Build the local unwind target table for `func`.
pub fn build_unwind_table(func: &SsaFunction, rpo: &[BlockId]) -> UnwindTable {
    unwind_table_with_indices(func, rpo, &func.block_index_map())
}

pub(crate) fn unwind_table_with_indices(
    func: &SsaFunction,
    rpo: &[BlockId],
    block_map: &FxHashMap<BlockId, usize>,
) -> UnwindTable {
    let mut targets: Vec<BlockId> = Vec::new();
    let mut state_by_target: FxHashMap<BlockId, usize> = FxHashMap::default();
    let mut first_state_by_tagbody: FxHashMap<(BlockId, usize), usize> = FxHashMap::default();

    for &bid in rpo {
        let Some(&bidx) = block_map.get(&bid) else {
            continue;
        };
        let block = &func.blocks[bidx];
        for (inst_idx, inst) in block.insts.iter().enumerate() {
            let tagbody = matches!(inst.op, SsaOp::EnterTagbody { .. });
            if tagbody {
                first_state_by_tagbody.insert((bid, inst_idx), targets.len());
            }
            for &target in registered_targets(&inst.op) {
                // GO indexes a contiguous slice, including labels already registered
                // by another scope. Other transfers use the first state for the target.
                match state_by_target.entry(target) {
                    Entry::Vacant(entry) => {
                        entry.insert(targets.len());
                        targets.push(target);
                    }
                    Entry::Occupied(_) if tagbody => targets.push(target),
                    _ => {}
                }
            }
        }
    }

    UnwindTable {
        targets,
        state_by_target,
        first_state_by_tagbody,
    }
}

fn registered_targets(op: &SsaOp) -> &[BlockId] {
    match op {
        SsaOp::EnterBlock { resume, .. } | SsaOp::EnterCatch { resume, .. } => {
            std::slice::from_ref(resume)
        }
        SsaOp::EnterProtect { cleanup, .. } => std::slice::from_ref(cleanup),
        SsaOp::EnterTagbody { targets, .. } | SsaOp::EnterHandlers { targets, .. } => targets,
        _ => &[],
    }
}

/// Visit implicit control entries, including targets needed by a registration table
/// even if a particular transfer (such as a local `GO`) became an ordinary jump.
pub(crate) fn visit_control_targets(block: &BasicBlock, mut visit: impl FnMut(BlockId)) {
    for &target in &block.unwind_succs {
        visit(target);
    }
    for inst in &block.insts {
        registered_targets(&inst.op)
            .iter()
            .copied()
            .for_each(&mut visit);
    }
}

pub(super) fn control_targets(func: &SsaFunction) -> FxHashSet<BlockId> {
    let mut targets = FxHashSet::default();
    for block in &func.blocks {
        visit_control_targets(block, |target| {
            targets.insert(target);
        });
    }
    targets
}

/// Reverse-postorder traversal over `func`, following normal, unwind, and control targets.
pub fn compute_rpo(func: &SsaFunction) -> Vec<BlockId> {
    if has_only_entry_block(func) {
        return vec![func.entry];
    }
    rpo_with_indices(func, &func.block_index_map())
}

fn has_only_entry_block(func: &SsaFunction) -> bool {
    matches!(func.blocks.as_slice(), [block] if block.id == func.entry)
}

pub(crate) fn rpo_with_indices(
    func: &SsaFunction,
    block_map: &FxHashMap<BlockId, usize>,
) -> Vec<BlockId> {
    if has_only_entry_block(func) {
        return vec![func.entry];
    }
    rpo_visiting_control_targets(func, block_map, |_| {})
}

fn rpo_visiting_control_targets(
    func: &SsaFunction,
    block_map: &FxHashMap<BlockId, usize>,
    mut visit: impl FnMut(BlockId),
) -> Vec<BlockId> {
    let mut visited = vec![false; func.blocks.len()];
    let mut postorder = Vec::with_capacity(func.blocks.len());
    let mut pending = vec![(func.entry, false)];
    let mut successors = Vec::new();

    while let Some((bid, expanded)) = pending.pop() {
        if expanded {
            postorder.push(bid);
            continue;
        }
        let Some(&idx) = block_map.get(&bid) else {
            continue;
        };
        if std::mem::replace(&mut visited[idx], true) {
            continue;
        }
        let block = &func.blocks[idx];
        block.terminator.visit_targets(|t| successors.push(t.block));
        visit_control_targets(block, |target| {
            successors.push(target);
            visit(target);
        });
        pending.push((bid, true));
        pending.extend(successors.drain(..).rev().map(|target| (target, false)));
    }

    postorder.reverse();
    postorder
}

/// Remove unreachable blocks, fold single-predecessor jump chains, and drop trivial block params.
pub(super) fn simplify_cfg(func: &mut SsaFunction) -> bool {
    if has_only_entry_block(func) {
        return false;
    }
    // 1. Prune unreachable blocks.
    let block_indices = func.block_index_map();
    let mut control_entries = FxHashSet::default();
    let rpo = rpo_visiting_control_targets(func, &block_indices, |target| {
        control_entries.insert(target);
    });
    let pruned = rpo.len() < func.blocks.len();
    if pruned {
        let reachable: FxHashSet<BlockId> = rpo.into_iter().collect();
        func.blocks.retain(|b| reachable.contains(&b.id));
        func.pc_to_block.retain(|_, bid| reachable.contains(bid));
    }

    // Pruning changes block positions. The other path can reuse the index when
    // merging chains, which preserves registrations and keeps control entries.
    let block_indices = (!pruned).then_some(block_indices);
    let merged = merge_jump_chains(func, &control_entries, block_indices);
    pruned | merged | eliminate_trivial_params(func, &control_entries)
}

fn merge_jump_chains(
    func: &mut SsaFunction,
    control_entries: &FxHashSet<BlockId>,
    block_indices: Option<FxHashMap<BlockId, usize>>,
) -> bool {
    let mut pred_counts: FxHashMap<BlockId, usize> = FxHashMap::default();
    for b in &func.blocks {
        b.terminator.visit_targets(|t| {
            *pred_counts.entry(t.block).or_default() += 1;
        });
    }

    let mut merged_into = FxHashMap::default();
    for block in &func.blocks {
        if let Terminator::Jump(target) = &block.terminator
            && target.block != block.id
            && target.block != func.entry
            && !control_entries.contains(&target.block)
            && pred_counts[&target.block] == 1
        {
            merged_into.insert(target.block, block.id);
        }
    }
    if merged_into.is_empty() {
        return false;
    }

    // Eligible edges form disjoint chains. A cycle would have no entry edge
    // and would already have been removed, or have a protected entry block.
    let block_indices = block_indices.unwrap_or_else(|| func.block_index_map());
    let mut rewrites = FxHashMap::default();
    let mut seen_unwinds = FxHashSet::default();
    for head in 0..func.blocks.len() {
        let head_id = func.blocks[head].id;
        if merged_into.contains_key(&head_id) {
            continue;
        }

        let mut tail = head;
        while let Terminator::Jump(target) = &func.blocks[tail].terminator {
            let Some(destination) = merged_into.get_mut(&target.block) else {
                break;
            };
            let next = block_indices[&target.block];
            for (param, &arg) in func.blocks[next].params.iter().zip(&target.args) {
                rewrites.insert(param.value, arg);
            }
            let mut insts = std::mem::take(&mut func.blocks[next].insts);
            let mut unwind_succs = std::mem::take(&mut func.blocks[next].unwind_succs);
            func.blocks[head].insts.append(&mut insts);
            func.blocks[head].unwind_succs.append(&mut unwind_succs);
            *destination = head_id;
            tail = next;
        }

        if tail != head {
            func.blocks[head].terminator_pc = func.blocks[tail].terminator_pc;
            func.blocks[head].terminator = std::mem::replace(
                &mut func.blocks[tail].terminator,
                Terminator::ReturnDynamicValues,
            );
            seen_unwinds.clear();
            func.blocks[head]
                .unwind_succs
                .retain(|&target| seen_unwinds.insert(target));
        }
    }

    func.blocks
        .retain(|block| !merged_into.contains_key(&block.id));
    if !rewrites.is_empty() {
        resolve_rewrite_chains(&mut rewrites);
        rewrite_all_uses(func, &rewrites);
    }
    for block in func.pc_to_block.values_mut() {
        if let Some(&head) = merged_into.get(block) {
            *block = head;
        }
    }
    true
}

pub(super) fn incoming_branches(func: &SsaFunction) -> FxHashMap<BlockId, Vec<&BranchTarget>> {
    let mut incoming: FxHashMap<BlockId, Vec<&BranchTarget>> = FxHashMap::default();
    for b in &func.blocks {
        b.terminator.visit_targets(|t| {
            incoming.entry(t.block).or_default().push(t);
        });
    }
    incoming
}

/// Fold parameters whose ordinary predecessors supply one value (apart from self edges).
/// A runtime control entry may supply inputs independently of those branch arguments.
pub(crate) fn eliminate_trivial_block_params(func: &mut SsaFunction) -> bool {
    if !has_nonentry_params(func) {
        return false;
    }
    eliminate_trivial_params(func, &control_targets(func))
}

pub(super) fn has_nonentry_params(func: &SsaFunction) -> bool {
    func.blocks
        .iter()
        .any(|b| b.id != func.entry && !b.params.is_empty())
}

fn eliminate_trivial_params(func: &mut SsaFunction, control_entries: &FxHashSet<BlockId>) -> bool {
    let mut changed = false;
    loop {
        if !has_nonentry_params(func) {
            break;
        }
        let incoming = incoming_branches(func);

        let mut rewrites: FxHashMap<ValueId, ValueId> = FxHashMap::default();

        for b in &func.blocks {
            if b.id == func.entry || b.params.is_empty() || control_entries.contains(&b.id) {
                continue;
            }
            let Some(edges) = incoming.get(&b.id) else {
                continue;
            };
            'param: for (pidx, param) in b.params.iter().enumerate() {
                let mut candidate = None;
                for edge in edges {
                    let Some(&arg) = edge.args.get(pidx) else {
                        continue 'param;
                    };
                    if arg == param.value {
                        continue;
                    }
                    if candidate.is_some_and(|prev| prev != arg) {
                        continue 'param;
                    }
                    candidate = Some(arg);
                }
                if let Some(replacement) = candidate {
                    rewrites.insert(param.value, replacement);
                }
            }
        }

        resolve_rewrite_chains(&mut rewrites);
        if rewrites.is_empty() {
            break;
        }
        rewrite_all_uses(func, &rewrites);
        retain_block_params(func, |param| !rewrites.contains_key(&param.value));
        changed = true;
    }

    changed
}

/// Retain parameters and the corresponding argument positions on every incoming edge.
pub(super) fn retain_block_params(
    func: &mut SsaFunction,
    mut keep: impl FnMut(&BlockParam) -> bool,
) -> bool {
    let mut removed: FxHashMap<BlockId, Vec<usize>> = FxHashMap::default();
    for block in &mut func.blocks {
        let mut index = 0;
        block.params.retain(|param| {
            let retain = keep(param);
            if !retain {
                removed.entry(block.id).or_default().push(index);
            }
            index += 1;
            retain
        });
    }
    if removed.is_empty() {
        return false;
    }
    for block in &mut func.blocks {
        block.terminator.visit_targets_mut(|target| {
            if let Some(positions) = removed.get(&target.block) {
                let mut positions = positions.iter().copied().peekable();
                let mut index = 0;
                target.args.retain(|_| {
                    let retain = positions.next_if_eq(&index).is_none();
                    index += 1;
                    retain
                });
            }
        });
    }
    true
}
