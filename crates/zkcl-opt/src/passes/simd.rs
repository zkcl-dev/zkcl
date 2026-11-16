use rustc_hash::FxHashMap;
use zkcl_bytecode::native_abi::I32x4Operation;

use super::rewrite::rewrite_all_uses;
use crate::ir::{
    DeoptRegisterValue, DeoptState, GuardedPrimitive, InstBuilder, SsaFunction, SsaOp, TypeTag,
    ValueId,
};

pub(super) fn lower_i32x4_call(
    emit: &mut InstBuilder<'_>,
    result: ValueId,
    symbol: usize,
    op: I32x4Operation,
    [left, right]: [ValueId; 2],
    deopt: &DeoptState,
) {
    emit.push(
        None,
        SsaOp::GuardPrimitiveBinding {
            symbol,
            primitive: GuardedPrimitive::I32x4(op),
        },
        TypeTag::Top,
        Some(deopt.clone()),
    );
    let lhs = emit.guarded_value(SsaOp::UnboxI32x4Guard(left), TypeTag::I32x4, deopt.clone());
    let rhs = emit.guarded_value(SsaOp::UnboxI32x4Guard(right), TypeTag::I32x4, deopt.clone());
    let value = emit.value(SsaOp::I32x4Binary { op, lhs, rhs }, TypeTag::I32x4);
    emit.push(Some(result), SsaOp::BoxI32x4(value), TypeTag::I32x4, None);
}

/// Pass vector values directly between specialized calls. Keep a box for each
/// result with an ordinary tagged use. Other results can be materialized on a
/// cold exit, where the producing box identifies the logical object.
pub(super) fn keep_i32x4_unboxed(func: &mut SsaFunction) -> bool {
    let mut boxes: FxHashMap<_, _> = func
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter_map(|inst| match (inst.id, &inst.op) {
            (Some(id), SsaOp::BoxI32x4(raw)) => Some((id, *raw)),
            _ => None,
        })
        .collect();
    if boxes.is_empty() {
        return false;
    }

    let mut rewrites = FxHashMap::default();
    for block in &mut func.blocks {
        let mut block_unboxes = FxHashMap::default();
        block.insts.retain(|inst| {
            let (Some(id), SsaOp::UnboxI32x4Guard(value)) = (inst.id, &inst.op) else {
                return true;
            };
            if let Some(&raw) = boxes.get(value).or_else(|| block_unboxes.get(value)) {
                rewrites.insert(id, raw);
                return false;
            }
            block_unboxes.insert(*value, id);
            true
        });
    }
    rewrite_all_uses(func, &rewrites);

    for block in &func.blocks {
        for inst in &block.insts {
            inst.op.visit_inputs(|value| {
                boxes.remove(&value);
            });
        }
        block.terminator.visit_inputs(|value| {
            boxes.remove(&value);
        });
    }
    if boxes.is_empty() {
        return !rewrites.is_empty();
    }

    for block in &mut func.blocks {
        block.insts.retain_mut(|inst| {
            if inst.id.is_some_and(|id| boxes.contains_key(&id)) {
                return false;
            }
            if let Some(deopt) = &mut inst.deopt {
                deopt.rewrite_values(|kind| match kind {
                    DeoptRegisterValue::TaggedWord(identity) => {
                        boxes.get(&identity).map_or(kind, |&value| {
                            DeoptRegisterValue::UnboxedI32x4 { identity, value }
                        })
                    }
                    _ => kind,
                });
            }
            true
        });
    }
    true
}
