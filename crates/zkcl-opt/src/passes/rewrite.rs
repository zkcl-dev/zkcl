//! Value substitution and elimination of dead instructions and block parameters.

use rustc_hash::FxHashMap;

use super::cfg::{control_targets, has_nonentry_params, incoming_branches, retain_block_params};
use crate::ir::{BlockId, DeoptRegisterValue, SsaFunction, SsaInst, ValueId};

/// Mark values demanded by control, observable operations, or the frame ABI.
/// A branch argument is a dependency of the destination parameter.
pub(super) fn eliminate_dead_code(func: &mut SsaFunction) -> bool {
    #[derive(Clone, Copy)]
    enum Definition<'a> {
        Leaf,
        Instruction(&'a SsaInst),
        Parameter(BlockId, usize),
    }

    let incoming = incoming_branches(func);
    let control_entries = if has_nonentry_params(func) {
        control_targets(func)
    } else {
        Default::default()
    };
    let mut definitions = vec![Definition::Leaf; func.next_value_id as usize];
    let mut live = vec![false; definitions.len()];
    let mut pending = Vec::new();
    let mark = |value: ValueId, live: &mut [bool], pending: &mut Vec<ValueId>| {
        if !std::mem::replace(&mut live[value.0 as usize], true) {
            pending.push(value);
        }
    };
    for block in &func.blocks {
        let mut mark_input = |value| mark(value, &mut live, &mut pending);
        // Function entry and runtime control entries have a frame contract
        // beyond the ordinary branch arguments present in the SSA.
        let fixed_params = block.id == func.entry || control_entries.contains(&block.id);
        for (index, param) in block.params.iter().enumerate() {
            definitions[param.value.0 as usize] = Definition::Parameter(block.id, index);
            if fixed_params {
                mark_input(param.value);
            }
        }
        block
            .terminator
            .direct_inputs()
            .iter()
            .copied()
            .for_each(&mut mark_input);
        for inst in &block.insts {
            if inst.op.effects().can_eliminate_if_unused() {
                if let Some(id) = inst.id {
                    definitions[id.0 as usize] = Definition::Instruction(inst);
                }
            } else {
                inst.visit_inputs(&mut mark_input);
            }
        }
    }
    // A cycle of loop-carried values without an observable use stays unmarked.
    while let Some(value) = pending.pop() {
        let mut mark_input = |input| mark(input, &mut live, &mut pending);
        match definitions[value.0 as usize] {
            Definition::Leaf => {}
            Definition::Instruction(inst) => inst.visit_inputs(mark_input),
            Definition::Parameter(block, index) => {
                if let Some(edges) = incoming.get(&block) {
                    for edge in edges {
                        mark_input(edge.args[index]);
                    }
                }
            }
        }
    }
    let mut changed = retain_block_params(func, |param| live[param.value.0 as usize]);
    for block in &mut func.blocks {
        let before = block.insts.len();
        block.insts.retain(|inst| {
            !inst.op.effects().can_eliminate_if_unused()
                || inst.id.is_some_and(|id| live[id.0 as usize])
        });
        changed |= block.insts.len() != before;
    }
    changed
}

pub(super) fn resolve_rewrite_chains(rewrites: &mut FxHashMap<ValueId, ValueId>) {
    let starts: Vec<_> = rewrites.keys().copied().collect();
    let mut path = Vec::new();
    for start in starts {
        let mut root = start;
        // Remove pending links while following them. Later paths traverse at
        // most one finalized link because it points directly to a survivor.
        while let Some(next) = rewrites.remove(&root) {
            path.push(root);
            root = next;
        }
        // Leave cyclic aliases untouched. Their definitions cannot all be
        // removed because there is no surviving replacement for them.
        if path.contains(&root) {
            path.clear();
        } else {
            for value in path.drain(..) {
                rewrites.insert(value, root);
            }
        }
    }
}

pub(super) fn rewrite_deopt_register_value(
    kind: DeoptRegisterValue,
    rewrites: &FxHashMap<ValueId, ValueId>,
) -> DeoptRegisterValue {
    match kind {
        DeoptRegisterValue::TaggedWord(v) => {
            DeoptRegisterValue::TaggedWord(*rewrites.get(&v).unwrap_or(&v))
        }
        DeoptRegisterValue::UnboxedI64(v) => {
            DeoptRegisterValue::UnboxedI64(*rewrites.get(&v).unwrap_or(&v))
        }
        DeoptRegisterValue::UnboxedI32x4 { identity, value } => DeoptRegisterValue::UnboxedI32x4 {
            identity,
            value: *rewrites.get(&value).unwrap_or(&value),
        },
        DeoptRegisterValue::UnboxedFloat { format, value } => DeoptRegisterValue::UnboxedFloat {
            format,
            value: *rewrites.get(&value).unwrap_or(&value),
        },
        DeoptRegisterValue::ConstImmediate(imm) => DeoptRegisterValue::ConstImmediate(imm),
    }
}

pub(super) fn rewrite_all_uses(func: &mut SsaFunction, rewrites: &FxHashMap<ValueId, ValueId>) {
    for b in &mut func.blocks {
        for inst in &mut b.insts {
            inst.op.rewrite_inputs(|v| *rewrites.get(&v).unwrap_or(&v));
            if let Some(deopt) = &mut inst.deopt {
                deopt.rewrite_values(|k| rewrite_deopt_register_value(k, rewrites));
            }
        }
        b.terminator
            .rewrite_inputs(|v| *rewrites.get(&v).unwrap_or(&v));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_long_alias_chain_before_removing_definitions() {
        let survivor = ValueId(0);
        let mut rewrites: FxHashMap<_, _> = (1..2048).map(|id| (ValueId(id), survivor)).collect();
        // Follow hash iteration order so the first walk traverses the entire
        // chain before any intermediate link can be shortened.
        let order: Vec<_> = rewrites.keys().copied().collect();
        for pair in order.windows(2) {
            *rewrites.get_mut(&pair[0]).unwrap() = pair[1];
        }

        resolve_rewrite_chains(&mut rewrites);

        assert_eq!(rewrites.len(), order.len());
        assert!(rewrites.values().all(|&value| value == survivor));
    }

    #[test]
    fn cyclic_aliases_keep_a_defined_replacement() {
        let a = ValueId(0);
        let b = ValueId(1);
        let c = ValueId(2);
        let mut rewrites = FxHashMap::from_iter([(a, b), (b, a), (c, a)]);

        resolve_rewrite_chains(&mut rewrites);

        assert!(!rewrites.contains_key(&a) && !rewrites.contains_key(&b));
        assert!(rewrites.values().all(|value| !rewrites.contains_key(value)));
    }
}
