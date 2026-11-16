//! Constant folding and copy propagation.

use rustc_hash::FxHashMap;
use zkcl_bytecode::Immediate;

use super::cfg::rpo_with_indices;
use super::rewrite::{resolve_rewrite_chains, rewrite_all_uses, rewrite_deopt_register_value};
use crate::ir::{
    ArithOp, DeoptRegisterValue, SsaFunction, SsaOp, Terminator, TypeTag, ValueId,
};

enum FoldedFixnum {
    Constant(i64),
    Existing(ValueId),
}

fn fold_fixnum_arithmetic(
    op: ArithOp,
    lhs: ValueId,
    rhs: ValueId,
    constants: &FxHashMap<ValueId, i64>,
    bounds: (i64, i64),
) -> Option<FoldedFixnum> {
    let left = constants.get(&lhs).copied();
    let right = constants.get(&rhs).copied();
    if let (Some(a), Some(b)) = (left, right) {
        let result = match op {
            ArithOp::Add => a.checked_add(b),
            ArithOp::Sub => a.checked_sub(b),
            ArithOp::Mul => a.checked_mul(b),
        };
        if let Some(result) = result.filter(|&value| (bounds.0..=bounds.1).contains(&value)) {
            return Some(FoldedFixnum::Constant(result));
        }
    }
    match (op, left, right) {
        (ArithOp::Add | ArithOp::Sub, _, Some(0)) | (ArithOp::Mul, _, Some(1)) => {
            Some(FoldedFixnum::Existing(lhs))
        }
        (ArithOp::Add, Some(0), _) | (ArithOp::Mul, Some(1), _) => {
            Some(FoldedFixnum::Existing(rhs))
        }
        (ArithOp::Mul, Some(0), _) | (ArithOp::Mul, _, Some(0)) => Some(FoldedFixnum::Constant(0)),
        _ => None,
    }
}

/// Constant/copy propagation, algebraic simplification, box/unbox cancellation, and known-branch folding.
pub(super) fn propagate_constants_and_copies(func: &mut SsaFunction) -> bool {
    let (min, max) = func.fixnum_bounds;
    let mut changed = false;
    let mut const_words: FxHashMap<ValueId, Immediate> = FxHashMap::default();
    let mut const_i64s: FxHashMap<ValueId, i64> = FxHashMap::default();
    let mut const_bools: FxHashMap<ValueId, bool> = FxHashMap::default();
    let mut box_sources: FxHashMap<ValueId, ValueId> = FxHashMap::default();
    let mut select_defs: FxHashMap<ValueId, (ValueId, Immediate, Immediate)> = FxHashMap::default();
    let mut not_bool_defs: FxHashMap<ValueId, ValueId> = FxHashMap::default();
    let mut copy_rewrites: FxHashMap<ValueId, ValueId> = FxHashMap::default();

    for b in &func.blocks {
        for inst in &b.insts {
            let Some(id) = inst.id else {
                continue;
            };
            match inst.op {
                SsaOp::ConstWord(imm) => {
                    const_words.insert(id, imm);
                }
                SsaOp::ConstI64(v) => {
                    const_i64s.insert(id, v);
                }
                SsaOp::ConstBool(v) => {
                    const_bools.insert(id, v);
                }
                SsaOp::BoxFixnum(u) => {
                    box_sources.insert(id, u);
                }
                SsaOp::SelectBool {
                    cond,
                    when_true,
                    when_false,
                } => {
                    select_defs.insert(id, (cond, when_true, when_false));
                }
                SsaOp::NotBool(inner) => {
                    not_bool_defs.insert(id, inner);
                }
                SsaOp::Copy(src) => {
                    copy_rewrites.insert(id, src);
                }
                _ => {}
            }
        }
    }

    if !copy_rewrites.is_empty() {
        resolve_rewrite_chains(&mut copy_rewrites);
        rewrite_all_uses(func, &copy_rewrites);
        for b in &mut func.blocks {
            b.insts
                .retain(|inst| inst.id.is_none_or(|id| !copy_rewrites.contains_key(&id)));
        }
        changed = true;
    }

    let block_map = func.block_index_map();
    let rpo = rpo_with_indices(func, &block_map);

    for bid in rpo {
        let bidx = block_map[&bid];
        let mut folded_to_value: FxHashMap<ValueId, ValueId> = FxHashMap::default();

        for inst in &mut func.blocks[bidx].insts {
            inst.op
                .rewrite_inputs(|v| *folded_to_value.get(&v).unwrap_or(&v));

            if let Some(deopt) = &mut inst.deopt {
                deopt.rewrite_values(|original| {
                    let remapped = rewrite_deopt_register_value(original, &folded_to_value);
                    let replacement = match remapped {
                        DeoptRegisterValue::TaggedWord(v) => {
                            if let Some(&imm) = const_words.get(&v) {
                                DeoptRegisterValue::ConstImmediate(imm)
                            } else if let Some(&src) = box_sources.get(&v) {
                                DeoptRegisterValue::UnboxedI64(src)
                            } else {
                                remapped
                            }
                        }
                        DeoptRegisterValue::UnboxedI64(v) => match const_i64s.get(&v) {
                            Some(&n) if (min..=max).contains(&n) => {
                                DeoptRegisterValue::ConstImmediate(Immediate::Fixnum(n))
                            }
                            _ => remapped,
                        },
                        DeoptRegisterValue::ConstImmediate(_)
                        | DeoptRegisterValue::UnboxedI32x4 { .. }
                        | DeoptRegisterValue::UnboxedFloat { .. } => remapped,
                    };
                    changed |= replacement != original;
                    replacement
                });
            }

            let Some(id) = inst.id else {
                continue;
            };

            match inst.op {
                SsaOp::UnboxFixnumGuard(src) => {
                    if let Some(&Immediate::Fixnum(n)) = const_words.get(&src) {
                        inst.op = SsaOp::ConstI64(n);

                        inst.ty = TypeTag::Fixnum;
                        inst.deopt = None;
                        const_i64s.insert(id, n);
                        changed = true;
                    } else if let Some(&unboxed) = box_sources.get(&src) {
                        folded_to_value.insert(id, unboxed);
                        if let Some(&n) = const_i64s.get(&unboxed) {
                            const_i64s.insert(id, n);
                        }
                        changed = true;
                    }
                }
                SsaOp::BoxFixnum(src) => {
                    if let Some(&n) = const_i64s.get(&src)
                        && (min..=max).contains(&n)
                    {
                        let imm = Immediate::Fixnum(n);
                        inst.op = SsaOp::ConstWord(imm);

                        inst.ty = TypeTag::Fixnum;
                        const_words.insert(id, imm);
                        changed = true;
                    }
                }
                SsaOp::IsFixnum(src) => {
                    if let Some(is_fixnum) = const_words
                        .get(&src)
                        .map(|imm| matches!(imm, Immediate::Fixnum(_)))
                        .or_else(|| box_sources.contains_key(&src).then_some(true))
                    {
                        inst.op = SsaOp::ConstBool(is_fixnum);
                        const_bools.insert(id, is_fixnum);
                        changed = true;
                    }
                }
                SsaOp::AddI64Checked(lhs, rhs)
                | SsaOp::AddI64Unchecked(lhs, rhs)
                | SsaOp::SubI64Checked(lhs, rhs)
                | SsaOp::SubI64Unchecked(lhs, rhs)
                | SsaOp::MulI64Checked(lhs, rhs)
                | SsaOp::MulI64Unchecked(lhs, rhs) => {
                    let op = match inst.op {
                        SsaOp::AddI64Checked(..) | SsaOp::AddI64Unchecked(..) => ArithOp::Add,
                        SsaOp::SubI64Checked(..) | SsaOp::SubI64Unchecked(..) => ArithOp::Sub,
                        _ => ArithOp::Mul,
                    };
                    match fold_fixnum_arithmetic(op, lhs, rhs, &const_i64s, (min, max)) {
                        Some(FoldedFixnum::Constant(n)) => {
                            inst.op = SsaOp::ConstI64(n);

                            inst.ty = TypeTag::Fixnum;
                            inst.deopt = None;
                            const_i64s.insert(id, n);
                        }
                        Some(FoldedFixnum::Existing(value)) => {
                            folded_to_value.insert(id, value);
                        }
                        None => continue,
                    }
                    changed = true;
                }
                SsaOp::CmpI64 { op, lhs, rhs } => {
                    if let (Some(&a), Some(&b)) = (const_i64s.get(&lhs), const_i64s.get(&rhs)) {
                        let res = op.test(a, b);
                        inst.op = SsaOp::ConstBool(res);

                        const_bools.insert(id, res);
                        changed = true;
                    }
                }
                SsaOp::SelectBool {
                    cond,
                    when_true,
                    when_false,
                } => {
                    if let Some(&b) = const_bools.get(&cond) {
                        let imm = if b { when_true } else { when_false };
                        inst.op = SsaOp::ConstWord(imm);
                        inst.ty = TypeTag::from_immediate(imm);
                        const_words.insert(id, imm);
                        changed = true;
                    }
                }
                SsaOp::IsPresent(src) => {
                    if const_words.contains_key(&src) {
                        inst.op = SsaOp::ConstBool(true);
                        const_bools.insert(id, true);
                        changed = true;
                    }
                }
                SsaOp::IsNil(src) => {
                    if let Some(&imm) = const_words.get(&src) {
                        let is_nil = imm == Immediate::Nil;
                        inst.op = SsaOp::ConstBool(is_nil);
                        const_bools.insert(id, is_nil);
                        changed = true;
                    } else if let Some(&(cond, wt, wf)) = select_defs.get(&src) {
                        if wt == Immediate::Nil && wf == Immediate::Nil {
                            inst.op = SsaOp::ConstBool(true);
                            const_bools.insert(id, true);
                            changed = true;
                        } else if wt != Immediate::Nil && wf != Immediate::Nil {
                            inst.op = SsaOp::ConstBool(false);
                            const_bools.insert(id, false);
                            changed = true;
                        } else if wt == Immediate::Nil {
                            folded_to_value.insert(id, cond);
                            changed = true;
                        } else {
                            inst.op = SsaOp::NotBool(cond);
                            not_bool_defs.insert(id, cond);
                            changed = true;
                        }
                    }
                }
                SsaOp::NotBool(src) => {
                    if let Some(&b) = const_bools.get(&src) {
                        inst.op = SsaOp::ConstBool(!b);
                        const_bools.insert(id, !b);
                        changed = true;
                    } else if let Some(&inner) = not_bool_defs.get(&src) {
                        folded_to_value.insert(id, inner);
                        changed = true;
                    }
                }
                _ => {}
            }
        }

        if !folded_to_value.is_empty() {
            resolve_rewrite_chains(&mut folded_to_value);
            rewrite_all_uses(func, &folded_to_value);
        }
    }

    changed | fold_known_branches(func, &const_words, &const_bools)
}

fn fold_known_branches(
    func: &mut SsaFunction,
    const_words: &FxHashMap<ValueId, Immediate>,
    const_bools: &FxHashMap<ValueId, bool>,
) -> bool {
    let mut changed = false;
    // Collect `SelectBool` & `NotBool` definitions to fold `BranchTruthy`/`BranchBool`.
    let mut bool_wrappers: FxHashMap<ValueId, (ValueId, bool)> = FxHashMap::default();
    let mut not_bool_map: FxHashMap<ValueId, ValueId> = FxHashMap::default();
    let mut value_types: FxHashMap<ValueId, TypeTag> = FxHashMap::default();
    for b in &func.blocks {
        for p in &b.params {
            value_types.insert(p.value, p.ty);
        }
        for inst in &b.insts {
            if let Some(id) = inst.id {
                value_types.insert(id, inst.ty);
                if let SsaOp::SelectBool {
                    cond,
                    when_true,
                    when_false,
                } = inst.op
                {
                    if when_true != Immediate::Nil && when_false == Immediate::Nil {
                        bool_wrappers.insert(id, (cond, false));
                    } else if when_true == Immediate::Nil && when_false != Immediate::Nil {
                        bool_wrappers.insert(id, (cond, true));
                    }
                } else if let SsaOp::NotBool(inner) = inst.op {
                    not_bool_map.insert(id, inner);
                }
            }
        }
    }

    for b in &mut func.blocks {
        let replacement = match &b.terminator {
            Terminator::BranchTruthy {
                cond,
                truthy,
                falsy,
            } => {
                if let Some(&imm) = const_words.get(cond) {
                    if imm == Immediate::Nil {
                        Some(Terminator::Jump(falsy.clone()))
                    } else {
                        Some(Terminator::Jump(truthy.clone()))
                    }
                } else if let Some(&ty) = value_types.get(cond)
                    && ty.is_definitely_nil()
                {
                    Some(Terminator::Jump(falsy.clone()))
                } else if let Some(&ty) = value_types.get(cond)
                    && ty.is_definitely_truthy()
                {
                    Some(Terminator::Jump(truthy.clone()))
                } else if let Some(&(mut bool_val, mut inverted)) = bool_wrappers.get(cond) {
                    while let Some(&inner) = not_bool_map.get(&bool_val) {
                        bool_val = inner;
                        inverted = !inverted;
                    }
                    if inverted {
                        Some(Terminator::BranchBool {
                            cond: bool_val,
                            then_target: falsy.clone(),
                            else_target: truthy.clone(),
                        })
                    } else {
                        Some(Terminator::BranchBool {
                            cond: bool_val,
                            then_target: truthy.clone(),
                            else_target: falsy.clone(),
                        })
                    }
                } else {
                    None
                }
            }
            Terminator::BranchBool {
                cond,
                then_target,
                else_target,
            } => {
                if let Some(&val) = const_bools.get(cond) {
                    Some(if val {
                        Terminator::Jump(then_target.clone())
                    } else {
                        Terminator::Jump(else_target.clone())
                    })
                } else if let Some(&inner) = not_bool_map.get(cond) {
                    Some(Terminator::BranchBool {
                        cond: inner,
                        then_target: else_target.clone(),
                        else_target: then_target.clone(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(new_term) = replacement {
            b.terminator = new_term;
            changed = true;
        }
    }

    changed
}
