use std::cmp::Reverse;
use std::collections::BinaryHeap;

use rustc_hash::{FxHashMap, FxHashSet};

use super::cfg::{rpo_with_indices, unwind_table_with_indices};
use crate::ir::{BasicBlock, BlockId, SsaFunction, SsaInst, SsaOp, ValueId, ValueRepr};

/// A slot index in the runtime `RootStack` frame (`slot[0] = code`, `slot[1] = closure`,
/// `slot[2..] = args + boxed values live across safepoints`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct RootSlot(pub usize);

/// Tagged values that must remain valid in `RootSlot`s across one safepoint instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafepointRoots {
    pub block: BlockId,
    pub inst_index: usize,
    pub pc: Option<usize>,
    pub live_slots: Vec<(ValueId, RootSlot)>,
}

/// Frame root-slot assignment produced by [`compute_root_layout`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootLayout {
    pub code_slot: RootSlot,
    pub closure_slot: RootSlot,
    pub arg_slots: Vec<(ValueId, RootSlot)>,
    /// Temporaries with disjoint safepoint lifetimes may share a physical slot.
    pub value_slots: FxHashMap<ValueId, RootSlot>,
    pub total_slots: usize,
    /// Sites in reverse postorder by block and in instruction order within each block.
    pub safepoint_roots: Vec<SafepointRoots>,
}

/// Slots 0 and 1 hold the code and closure, followed by incoming arguments and heap
/// references that can remain live across a safepoint.
pub fn compute_root_layout(func: &SsaFunction) -> RootLayout {
    let block_map = func.block_index_map();
    let rpo = rpo_with_indices(func, &block_map);
    let unwind = unwind_table_with_indices(func, &rpo, &block_map);
    root_layout_with_indices(func, &block_map, &rpo, unwind.targets())
}

#[derive(Clone, Copy)]
enum RootCandidate {
    None,
    Instruction,
    Parameter(BlockId),
}

pub(crate) fn root_layout_with_indices(
    func: &SsaFunction,
    block_map: &FxHashMap<BlockId, usize>,
    rpo: &[BlockId],
    unwind_targets: &[BlockId],
) -> RootLayout {
    let mut candidates = vec![RootCandidate::None; func.next_value_id as usize];
    let mut has_safepoint = false;
    for block in &func.blocks {
        for param in &block.params {
            if param.repr == ValueRepr::TaggedWord {
                candidates[param.value.0 as usize] = RootCandidate::Parameter(block.id);
            }
        }
        for inst in &block.insts {
            has_safepoint |= is_safepoint(inst);
            if let Some(id) = inst.id
                && inst.op.result_repr() == ValueRepr::TaggedWord
                && !matches!(
                    inst.op,
                    SsaOp::ConstWord(_) | SsaOp::BoxFixnum(_) | SsaOp::SelectBool { .. }
                )
            {
                candidates[id.0 as usize] = RootCandidate::Instruction;
            }
        }
    }
    let may_reference_heap =
        |value: ValueId| !matches!(candidates[value.0 as usize], RootCandidate::None);
    let mut layout = RootLayout {
        code_slot: RootSlot(0),
        closure_slot: RootSlot(1),
        arg_slots: Vec::with_capacity(func.entry_params.len()),
        value_slots: FxHashMap::default(),
        total_slots: 2,
        safepoint_roots: Vec::new(),
    };
    for &arg in &func.entry_params {
        if may_reference_heap(arg) {
            let slot = RootSlot(layout.total_slots);
            layout.total_slots += 1;
            layout.value_slots.insert(arg, slot);
            layout.arg_slots.push((arg, slot));
        }
    }
    if !has_safepoint {
        return layout;
    }

    let add_use = |value: ValueId, live: &mut FxHashSet<ValueId>| {
        if may_reference_heap(value) {
            live.insert(value);
        }
    };
    let collect_live_out =
        |block: &BasicBlock, live_in: &[FxHashSet<ValueId>], live: &mut FxHashSet<ValueId>| {
            live.clear();
            let mut add_successor = |target: BlockId| {
                let Some(&index) = block_map.get(&target) else {
                    return;
                };
                live.extend(live_in[index].iter().copied().filter(|value| {
                    match candidates[value.0 as usize] {
                        RootCandidate::Parameter(owner) => owner != target,
                        _ => true,
                    }
                }));
            };
            block.terminator.visit_targets(|target| {
                add_successor(target.block);
            });
            for &target in &block.unwind_succs {
                add_successor(target);
            }
            // Lowerers read all ordinary edge arguments. Unwind dispatch
            // supplies its parameters separately from the runtime frame.
            block.terminator.visit_inputs(|value| add_use(value, live));
        };

    let mut live_in = vec![FxHashSet::default(); func.blocks.len()];
    let mut live = FxHashSet::default();
    loop {
        let mut changed = false;
        for &bid in rpo.iter().rev() {
            let index = block_map[&bid];
            let block = &func.blocks[index];
            collect_live_out(block, &live_in, &mut live);
            for inst in block.insts.iter().rev() {
                if let Some(def) = inst.id.filter(|&value| may_reference_heap(value)) {
                    live.remove(&def);
                }
                inst.visit_inputs(|value| add_use(value, &mut live));
            }
            if live != live_in[index] {
                std::mem::swap(&mut live, &mut live_in[index]);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Collect the values that must survive each safepoint before adding the
    // operands needed to execute the current instruction.
    let mut safepoint_live_values = Vec::new();
    for &bid in rpo {
        let b = &func.blocks[block_map[&bid]];
        collect_live_out(b, &live_in, &mut live);

        let mut block_safepoints = Vec::new();
        for (inst_idx, inst) in b.insts.iter().enumerate().rev() {
            if let Some(def) = inst.id.filter(|&value| may_reference_heap(value)) {
                live.remove(&def);
            }
            if is_safepoint(inst) {
                if matches!(
                    inst.op,
                    SsaOp::CallNamed { .. } | SsaOp::CallValue { .. } | SsaOp::CallValues { .. }
                ) && let Some(state) = &inst.deopt
                {
                    // A child can collect before it returns a continuation to the
                    // caller. The caller's recovery operands still need valid roots.
                    state.visit_values(|value| add_use(value, &mut live));
                }
                let mut tagged_live: Vec<ValueId> = live.iter().copied().collect();
                tagged_live.sort();
                block_safepoints.push((bid, inst_idx, inst.pc, tagged_live));
            }
            inst.visit_inputs(|value| add_use(value, &mut live));
        }
        block_safepoints.reverse();
        safepoint_live_values.extend(block_safepoints);
    }

    let mut lifetimes: FxHashMap<ValueId, (usize, usize)> = FxHashMap::default();
    for (site, (_, _, _, values)) in safepoint_live_values.iter().enumerate() {
        for &value in values {
            lifetimes
                .entry(value)
                .and_modify(|(_, last)| *last = site)
                .or_insert((site, site));
        }
    }
    let mut unwind_live = FxHashSet::default();
    for &target in unwind_targets {
        if let Some(&tidx) = block_map.get(&target) {
            for &value in &live_in[tidx] {
                if !matches!(
                    candidates[value.0 as usize],
                    RootCandidate::Parameter(owner) if owner == target
                ) {
                    unwind_live.insert(value);
                }
            }
        }
    }

    let mut pinned = Vec::new();
    let mut reusable = Vec::new();
    for (value, interval) in lifetimes {
        if layout.value_slots.contains_key(&value) {
            continue;
        }
        if unwind_live.contains(&value) {
            pinned.push(value);
        } else {
            reusable.push((value, interval));
        }
    }

    // Values live upon entering an unwind target are reloaded together by
    // local unwind dispatch and keep dedicated root slots.
    pinned.sort_unstable();
    for value in pinned {
        layout
            .value_slots
            .insert(value, RootSlot(layout.total_slots));
        layout.total_slots += 1;
    }

    // Non-unwind temporaries share physical slots across disjoint safepoint lifetimes.
    reusable.sort_unstable_by_key(|&(value, (first, _))| (first, value));
    let mut occupied: BinaryHeap<Reverse<(usize, RootSlot)>> = BinaryHeap::new();
    for (value, (first, last)) in reusable {
        let slot = if let Some(mut next) = occupied.peek_mut().filter(|next| next.0.0 < first) {
            let (_, slot) = next.0;
            *next = Reverse((last, slot));
            slot
        } else {
            let slot = RootSlot(layout.total_slots);
            layout.total_slots += 1;
            occupied.push(Reverse((last, slot)));
            slot
        };
        layout.value_slots.insert(value, slot);
    }

    layout.safepoint_roots = safepoint_live_values
        .into_iter()
        .map(|(block, inst_index, pc, vals)| SafepointRoots {
            block,
            inst_index,
            pc,
            live_slots: vals
                .into_iter()
                .map(|v| (v, layout.value_slots[&v]))
                .collect(),
        })
        .collect();

    layout
}

fn is_safepoint(inst: &SsaInst) -> bool {
    inst.op.effects().may_gc()
}
