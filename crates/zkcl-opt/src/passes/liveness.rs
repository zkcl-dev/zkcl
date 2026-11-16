//! Value liveness over the SSA control-flow graph.
//!
//! Backends that keep SSA values in a fixed set of machine locations reuse a
//! location once its value is dead. Deopt recovery maps count as uses here: a
//! handoff reads the values a map names, so they outlive their last regular use.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::passes::rpo_with_indices;
use crate::{BlockId, DeoptRegisterValue, SsaFunction, SsaInst, ValueId};

/// Values live on entry to and after each block.
pub struct Liveness {
    live_in: FxHashMap<BlockId, FxHashSet<ValueId>>,
    live_out: FxHashMap<BlockId, FxHashSet<ValueId>>,
}

impl Liveness {
    pub fn solve(function: &SsaFunction) -> Self {
        let indices = function.block_index_map();
        let rpo = rpo_with_indices(function, &indices);
        let mut live_in = FxHashMap::default();
        let mut live_out = FxHashMap::default();
        let mut uses = Vec::with_capacity(function.blocks.len());
        let mut defs = Vec::with_capacity(function.blocks.len());
        let mut successors = Vec::with_capacity(function.blocks.len());
        for block in &function.blocks {
            let mut defined: FxHashSet<ValueId> =
                block.params.iter().map(|param| param.value).collect();
            let mut used = FxHashSet::default();
            for inst in &block.insts {
                record_uses(inst, &defined, &mut used);
                if let Some(id) = inst.id {
                    defined.insert(id);
                }
            }
            block.terminator.visit_inputs(|value| {
                if !defined.contains(&value) {
                    used.insert(value);
                }
            });
            uses.push(used);
            defs.push(defined);
            let mut targets = Vec::new();
            block
                .terminator
                .visit_targets(|target| targets.push(target.block));
            targets.extend(block.unwind_succs.iter().copied());
            successors.push(targets);
            live_in.insert(block.id, FxHashSet::default());
            live_out.insert(block.id, FxHashSet::default());
        }

        let mut changed = true;
        while changed {
            changed = false;
            for &id in rpo.iter().rev() {
                let index = indices[&id];
                let mut out: FxHashSet<ValueId> = FxHashSet::default();
                for successor in &successors[index] {
                    if let Some(successor_in) = live_in.get(successor) {
                        out.extend(successor_in.iter().copied());
                    }
                }
                let mut entry = uses[index].clone();
                entry.extend(out.difference(&defs[index]).copied());
                changed |= *live_out.get(&id).unwrap() != out;
                changed |= *live_in.get(&id).unwrap() != entry;
                live_out.insert(id, out);
                live_in.insert(id, entry);
            }
        }
        Self { live_in, live_out }
    }

    pub fn live_in(&self, block: BlockId) -> &FxHashSet<ValueId> {
        &self.live_in[&block]
    }

    pub fn live_out(&self, block: BlockId) -> &FxHashSet<ValueId> {
        &self.live_out[&block]
    }
}

/// Values read by an instruction, including the ones a recovery map restores.
pub fn instruction_uses(inst: &SsaInst, mut visit: impl FnMut(ValueId)) {
    inst.op.visit_inputs(&mut visit);
    if let Some(state) = &inst.deopt {
        for (_, value) in &state.registers_to_restore {
            if let Some(value) = restored_value(value) {
                visit(value);
            }
        }
    }
}

pub fn restored_value(value: &DeoptRegisterValue) -> Option<ValueId> {
    match *value {
        DeoptRegisterValue::TaggedWord(value) | DeoptRegisterValue::UnboxedI64(value) => {
            Some(value)
        }
        DeoptRegisterValue::UnboxedI32x4 { value, .. }
        | DeoptRegisterValue::UnboxedFloat { value, .. } => Some(value),
        DeoptRegisterValue::ConstImmediate(_) => None,
    }
}

fn record_uses(inst: &SsaInst, defined: &FxHashSet<ValueId>, used: &mut FxHashSet<ValueId>) {
    instruction_uses(inst, |value| {
        if !defined.contains(&value) {
            used.insert(value);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BasicBlock, BlockParam, BranchTarget, ScopeId, ScopeTree, SsaFunction, SsaInst, SsaOp,
        Terminator, TypeTag, ValueRepr,
    };
    use zkcl_bytecode::Arity;

    fn parameter(value: u32) -> BlockParam {
        BlockParam {
            value: ValueId(value),
            ty: TypeTag::Any,
            repr: ValueRepr::TaggedWord,
            origin_reg: None,
        }
    }

    fn copy(id: u32, source: u32) -> SsaInst {
        SsaInst {
            id: Some(ValueId(id)),
            op: SsaOp::Copy(ValueId(source)),
            ty: TypeTag::Any,
            pc: Some(0),
            deopt: None,
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
            next_value_id: 8,
        }
    }

    fn block(
        id: u32,
        params: Vec<BlockParam>,
        insts: Vec<SsaInst>,
        terminator: Terminator,
    ) -> BasicBlock {
        BasicBlock {
            id: BlockId(id),
            params,
            insts,
            terminator,
            terminator_pc: 0,
            entry_scope: ScopeId::ROOT,
            unwind_succs: Vec::new(),
        }
    }

    #[test]
    fn a_value_used_later_in_the_block_is_not_live_in() {
        let ssa = function(vec![block(
            0,
            vec![parameter(0)],
            vec![copy(1, 0), copy(2, 1)],
            Terminator::ReturnOne(ValueId(2)),
        )]);
        let liveness = Liveness::solve(&ssa);
        assert!(liveness.live_in(BlockId(0)).is_empty());
    }

    #[test]
    fn loop_carried_values_stay_live_across_the_backedge() {
        let ssa = function(vec![
            block(
                0,
                vec![parameter(0)],
                vec![copy(1, 0)],
                Terminator::Jump(BranchTarget {
                    block: BlockId(1),
                    args: Vec::new(),
                }),
            ),
            block(
                1,
                Vec::new(),
                vec![copy(2, 1)],
                Terminator::Jump(BranchTarget {
                    block: BlockId(1),
                    args: Vec::new(),
                }),
            ),
        ]);
        let liveness = Liveness::solve(&ssa);
        assert!(liveness.live_out(BlockId(1)).contains(&ValueId(1)));
        assert!(liveness.live_in(BlockId(1)).contains(&ValueId(1)));
        assert!(!liveness.live_in(BlockId(1)).contains(&ValueId(2)));
    }

    #[test]
    fn recovery_maps_count_as_uses() {
        let mut inst = copy(1, 0);
        inst.deopt = Some(crate::DeoptState {
            pc: 0,
            registers_to_restore: vec![(0, DeoptRegisterValue::TaggedWord(ValueId(5)))],
        });
        let ssa = function(vec![block(
            0,
            Vec::new(),
            vec![inst],
            Terminator::ReturnOne(ValueId(1)),
        )]);
        let liveness = Liveness::solve(&ssa);
        assert!(liveness.live_in(BlockId(0)).contains(&ValueId(5)));
    }
}
