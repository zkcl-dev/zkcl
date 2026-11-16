//! Specialize named calls using operand evidence and guarded Lisp bindings.

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::native_abi::{FixnumPrimitive, IntegerBitPrimitive};
use zkcl_bytecode::{AliasDomain, FixnumArithmetic, Immediate};

use super::{float, float_evidence::StaticFloatFormats, simd};
use crate::ir::{
    ArithOp, BasicBlock, BitwiseOp, CallSiteProfile, DeoptState, GuardedPrimitive, InstBuilder,
    SpecializationInputs, SsaFunction, SsaInst, SsaOp, Terminator, TypeTag, ValueId, ValueRepr,
};
use crate::native::{empty_return_blocks, returns_dynamic_values};

fn identify_fixnum_op(
    symbol: usize,
    pc: Option<usize>,
    specialization: &dyn SpecializationInputs,
) -> Option<FixnumPrimitive> {
    if let Some(pc) = pc {
        if let Some(operation) = specialization.fixnum_arith_op(pc) {
            return Some(FixnumPrimitive::Arithmetic(match operation {
                ArithOp::Add => FixnumArithmetic::Add,
                ArithOp::Sub => FixnumArithmetic::Sub,
                ArithOp::Mul => FixnumArithmetic::Mul,
            }));
        }
        if let Some(comparison) = specialization.fixnum_cmp_op(pc) {
            return Some(FixnumPrimitive::Compare(comparison));
        }
    }
    let ("COMMON-LISP" | "CL", name) = specialization.named_symbol(symbol)? else {
        return None;
    };
    FixnumPrimitive::named(name)
}

/// Lower supported primitive calls into SSA operations with binding and operand guards.
///
/// Calls marked `notinline` are never specialized.
pub(super) fn specialize_named_calls(
    func: &mut SsaFunction,
    specialization: &dyn SpecializationInputs,
    allow_speculative_deopt: bool,
    allow_static_native: bool,
) -> bool {
    let has_eligible_call = |block: &BasicBlock| {
        block.insts.iter().any(|inst| {
            matches!(
                inst.op,
                SsaOp::CallNamed {
                    notinline: false,
                    ..
                }
            )
        })
    };
    if !func.blocks.iter().any(has_eligible_call) {
        return false;
    }
    let value_types = func.value_metadata();
    let return_blocks = empty_return_blocks(&func.blocks);
    let float_formats = StaticFloatFormats::analyze(func, specialization);
    let setf_aref_values: FxHashSet<_> = func
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter_map(|inst| {
            let (Some(id), &SsaOp::LoadConst(index)) = (inst.id, &inst.op) else {
                return None;
            };
            specialization.is_setf_aref_literal(index).then_some(id)
        })
        .collect();

    let mut changed = false;
    for block in func
        .blocks
        .iter_mut()
        .filter(|block| has_eligible_call(block))
    {
        let returns_values =
            returns_dynamic_values(block, |target| return_blocks.contains(&target));
        let old_insts = std::mem::take(&mut block.insts);
        let old_len = old_insts.len();
        let mut new_insts = Vec::with_capacity(old_len);

        for (inst_idx, mut inst) in old_insts.into_iter().enumerate() {
            let SsaOp::CallNamed {
                symbol,
                ref args,
                has_result,
                at,
                associate,
                notinline,
                profile,
            } = inst.op
            else {
                new_insts.push(inst);
                continue;
            };

            if notinline || specialization.is_notinline(symbol) {
                new_insts.push(inst);
                continue;
            }

            let is_last_tail = !has_result && inst_idx + 1 == old_len && returns_values;
            let uses_result = has_result || is_last_tail;
            let name = specialization.named_symbol(symbol);
            let effective_profile = inst
                .pc
                .map(|pc| specialization.call_site_profile(pc))
                .filter(|&profile| profile != CallSiteProfile::Unprofiled)
                .unwrap_or(profile);
            let permits_fixnum = || {
                (allow_speculative_deopt
                    && permits_fixnum_specialization(effective_profile, args, &value_types))
                    || (allow_static_native
                        && args.iter().all(|arg| {
                            value_types
                                .get(arg)
                                .is_some_and(|(_, ty)| *ty == TypeTag::Fixnum)
                        }))
            };
            let mut emit = InstBuilder {
                next_value_id: &mut func.next_value_id,
                insts: &mut new_insts,
                pc: inst.pc,
            };

            if (allow_speculative_deopt || allow_static_native)
                && let (Some(("COMMON-LISP" | "CL", "FUNCALL")), [callee, rest @ ..]) =
                    (name, args.as_slice())
            {
                let Some(deopt) = inst.deopt.as_ref().filter(|_| !associate) else {
                    new_insts.push(inst);
                    continue;
                };
                if let &[value, array, index] = rest
                    && let Some(format) = match effective_profile {
                        CallSiteProfile::FloatArrayWriter { format } if allow_speculative_deopt => {
                            Some(format)
                        }
                        _ if allow_static_native && setf_aref_values.contains(callee) => {
                            float_formats
                                .scalar(value)
                                .filter(|&format| float_formats.vector(array) == Some(format))
                        }
                        _ => None,
                    }
                    && let Some(result) = inst.id
                {
                    let raw = emit.guarded_value(
                        SsaOp::UnboxFloatGuard { format, value },
                        TypeTag::Float,
                        deopt.clone(),
                    );
                    emit.push(
                        None,
                        SsaOp::WriteFloatVector {
                            symbol,
                            format,
                            setter: *callee,
                            array,
                            index,
                            value: raw,
                        },
                        TypeTag::Top,
                        Some(deopt.clone()),
                    );
                    emit.push(Some(result), SsaOp::Copy(value), TypeTag::Float, None);
                    changed = true;
                    continue;
                }
                if !allow_speculative_deopt {
                    new_insts.push(inst);
                    continue;
                }
                emit.push(
                    None,
                    SsaOp::GuardPrimitiveBinding {
                        symbol,
                        primitive: GuardedPrimitive::Funcall,
                    },
                    TypeTag::Top,
                    Some(deopt.clone()),
                );
                inst.op = SsaOp::CallValue {
                    callee: *callee,
                    args: rest.to_vec(),
                    has_result,
                    at,
                    associate,
                    notinline: false,
                    profile: CallSiteProfile::Unprofiled,
                };
                new_insts.push(inst);
                changed = true;
                continue;
            }

            if !associate
                && uses_result
                && let Some(("COMMON-LISP" | "CL", "AREF")) = name
                && let &[array, index] = args.as_slice()
                && let Some(format) = match effective_profile {
                    CallSiteProfile::FloatArrayReader { format } if allow_speculative_deopt => {
                        Some(format)
                    }
                    _ => float_formats.vector(array),
                }
                && let Some(deopt) = &inst.deopt
            {
                let raw = emit.guarded_value(
                    SsaOp::ReadFloatVector {
                        symbol,
                        format,
                        array,
                        index,
                    },
                    TypeTag::Float,
                    deopt.clone(),
                );
                let result = inst.id.unwrap_or_else(|| emit.fresh_value());
                emit.push(
                    Some(result),
                    SsaOp::BoxFloat { format, value: raw },
                    TypeTag::Float,
                    None,
                );
                if is_last_tail {
                    block.terminator = Terminator::ReturnOne(result);
                }
                changed = true;
                continue;
            }
            if !associate
                && uses_result
                && let &[lhs, rhs] = args.as_slice()
                && let Some(op) = float::named_arithmetic(name)
                && let Some(format) = match (allow_speculative_deopt, effective_profile) {
                    (true, CallSiteProfile::FloatArithmetic { .. }) => {
                        float::profiled_format(effective_profile)
                    }
                    _ => float_formats.arithmetic(lhs, rhs),
                }
                && let Some(deopt) = &inst.deopt
            {
                let result = inst.id.unwrap_or_else(|| emit.fresh_value());
                float::lower_call(&mut emit, result, symbol, format, op, [lhs, rhs], deopt);
                if is_last_tail {
                    block.terminator = Terminator::ReturnOne(result);
                }
                changed = true;
                continue;
            }
            if allow_speculative_deopt
                && !associate
                && uses_result
                && let CallSiteProfile::SimdI32x4 { operation } = effective_profile
                && let &[lhs, rhs] = args.as_slice()
                && let Some(deopt) = &inst.deopt
            {
                let result = inst.id.unwrap_or_else(|| emit.fresh_value());
                simd::lower_i32x4_call(&mut emit, result, symbol, operation, [lhs, rhs], deopt);
                if is_last_tail {
                    block.terminator = Terminator::ReturnOne(result);
                }
                changed = true;
                continue;
            }
            if !associate
                && uses_result
                && let Some(deopt) = &inst.deopt
                && let Some((primitive, op)) = identify_fixnum_bits(specialization, symbol)
                && let [lhs, rhs] = args.as_slice()
                && permits_fixnum()
            {
                let result = inst.id.unwrap_or_else(|| emit.fresh_value());
                emit.push(
                    None,
                    SsaOp::GuardPrimitiveBinding {
                        symbol,
                        primitive: GuardedPrimitive::IntegerBits(primitive),
                    },
                    TypeTag::Top,
                    Some(deopt.clone()),
                );
                let lhs = emit.guarded_value(
                    SsaOp::UnboxFixnumGuard(*lhs),
                    TypeTag::Fixnum,
                    deopt.clone(),
                );
                let rhs = emit.guarded_value(
                    SsaOp::UnboxFixnumGuard(*rhs),
                    TypeTag::Fixnum,
                    deopt.clone(),
                );
                let value = emit.value(SsaOp::BitwiseI64 { op, lhs, rhs }, TypeTag::Fixnum);
                emit.push(Some(result), SsaOp::BoxFixnum(value), TypeTag::Fixnum, None);
                if is_last_tail {
                    block.terminator = Terminator::ReturnOne(result);
                }
                changed = true;
                continue;
            }
            if !associate
                && uses_result
                && let Some(kind) = identify_fixnum_op(symbol, inst.pc, specialization)
                && permits_fixnum()
                && let Some(deopt) = &inst.deopt
            {
                let result = if has_result {
                    inst.id
                } else {
                    Some(emit.fresh_value())
                };
                if let Some(result) = result
                    && lower_fixnum_call(&mut emit, result, kind, args, deopt)
                {
                    // Arithmetic above has no observable effects. A replaced
                    // primitive must receive the original operands via deopt.
                    emit.push(
                        None,
                        SsaOp::GuardPrimitiveBinding {
                            symbol,
                            primitive: match kind {
                                FixnumPrimitive::Arithmetic(op) => {
                                    GuardedPrimitive::FixnumArithmetic(op)
                                }
                                FixnumPrimitive::Compare(op) => GuardedPrimitive::Comparison(op),
                            },
                        },
                        TypeTag::Top,
                        Some(deopt.clone()),
                    );
                    if is_last_tail {
                        block.terminator = Terminator::ReturnOne(result);
                    }
                    changed = true;
                    continue;
                }
            }

            new_insts.push(inst);
        }

        block.insts = new_insts;
    }

    changed
}

fn identify_fixnum_bits(
    specialization: &dyn SpecializationInputs,
    symbol: usize,
) -> Option<(IntegerBitPrimitive, BitwiseOp)> {
    let ("COMMON-LISP" | "CL", name) = specialization.named_symbol(symbol)? else {
        return None;
    };
    let primitive = IntegerBitPrimitive::named(name)?;
    let op = match primitive {
        IntegerBitPrimitive::And => BitwiseOp::And,
        IntegerBitPrimitive::Or => BitwiseOp::Or,
        IntegerBitPrimitive::Xor => BitwiseOp::Xor,
        _ => return None,
    };
    Some((primitive, op))
}

pub(super) fn share_binding_guards(func: &mut SsaFunction) {
    for block in &mut func.blocks {
        let mut guards = block
            .insts
            .iter()
            .filter_map(|inst| fixnum_binding_guard(&inst.op));
        if guards.nth(1).is_some() {
            block.insts = group_fixnum_binding_guards(std::mem::take(&mut block.insts));
        }
    }
}

fn fixnum_binding_guard(op: &SsaOp) -> Option<(usize, GuardedPrimitive)> {
    match *op {
        SsaOp::GuardPrimitiveBinding {
            symbol,
            primitive:
                primitive @ (GuardedPrimitive::FixnumArithmetic(_)
                | GuardedPrimitive::Comparison(_)
                | GuardedPrimitive::IntegerBits(_)),
        } => Some((symbol, primitive)),
        _ => None,
    }
}

/// Check the required function cells at the first call of a straight-line
/// arithmetic sequence. A miss resumes at that call, before any intermediate
/// result is needed. Only instructions that cannot change a binding are crossed.
fn group_fixnum_binding_guards(insts: Vec<SsaInst>) -> Vec<SsaInst> {
    let mut result: Vec<SsaInst> = Vec::with_capacity(insts.len());
    let mut arithmetic = Vec::new();
    let mut checked = FxHashSet::default();
    let mut first_guard: Option<usize> = None;
    for mut inst in insts {
        if let Some(binding) = fixnum_binding_guard(&inst.op) {
            if checked.insert(binding) {
                if let Some(first) = first_guard {
                    inst.pc = result[first].pc;
                    inst.deopt = result[first].deopt.clone();
                } else {
                    first_guard = Some(result.len());
                }
                result.push(inst);
            }
        } else if first_guard.is_some() && can_cross_binding_guard(&inst.op) {
            arithmetic.push(inst);
        } else {
            result.append(&mut arithmetic);
            checked.clear();
            first_guard = None;
            result.push(inst);
        }
    }
    result.append(&mut arithmetic);
    result
}

fn can_cross_binding_guard(op: &SsaOp) -> bool {
    let eff = op.effects();
    !matches!(op, SsaOp::GuardPrimitiveBinding { .. })
        && (eff.writes.disjoint(AliasDomain::SYMBOL_FN)
            && (eff.writes.is_empty() || matches!(op, SsaOp::SetValues(_))))
        && !eff.may_unwind()
        && !eff.may_suspend()
        && !eff.may_diverge()
}

fn permits_fixnum_specialization(
    profile: CallSiteProfile,
    args: &[ValueId],
    value_types: &FxHashMap<ValueId, (ValueRepr, TypeTag)>,
) -> bool {
    match profile {
        CallSiteProfile::FixnumArithmetic => true,
        CallSiteProfile::Megamorphic
        | CallSiteProfile::FloatArithmetic { .. }
        | CallSiteProfile::PolymorphicArithmetic => false,
        _ => {
            !args.is_empty()
                && args.iter().all(|arg| {
                    matches!(
                        value_types.get(arg),
                        Some((
                            _,
                            TypeTag::Top | TypeTag::Any | TypeTag::Fixnum | TypeTag::Bottom
                        ))
                    )
                })
        }
    }
}

fn emit_lisp_boolean(emit: &mut InstBuilder<'_>, result: ValueId, cond: ValueId) {
    emit.push(
        Some(result),
        SsaOp::SelectBool {
            cond,
            when_true: Immediate::True,
            when_false: Immediate::Nil,
        },
        TypeTag::Any,
        None,
    );
}

fn checked_arithmetic(op: FixnumArithmetic, lhs: ValueId, rhs: ValueId) -> SsaOp {
    match op {
        FixnumArithmetic::Add | FixnumArithmetic::Inc => SsaOp::AddI64Checked(lhs, rhs),
        FixnumArithmetic::Sub | FixnumArithmetic::Dec => SsaOp::SubI64Checked(lhs, rhs),
        FixnumArithmetic::Mul => SsaOp::MulI64Checked(lhs, rhs),
    }
}

fn lower_fixnum_call(
    emit: &mut InstBuilder<'_>,
    result: ValueId,
    kind: FixnumPrimitive,
    args: &[ValueId],
    deopt: &DeoptState,
) -> bool {
    let guard =
        |emit: &mut InstBuilder<'_>, op| emit.guarded_value(op, TypeTag::Fixnum, deopt.clone());
    let unbox = |emit: &mut InstBuilder<'_>, arg| guard(emit, SsaOp::UnboxFixnumGuard(arg));
    let unboxed_result = match (kind, args) {
        (FixnumPrimitive::Compare(op), &[left, right]) => {
            let lhs = unbox(emit, left);
            let rhs = unbox(emit, right);
            let cond = emit.value(SsaOp::CmpI64 { op, lhs, rhs }, TypeTag::Any);
            emit_lisp_boolean(emit, result, cond);
            return true;
        }
        (
            FixnumPrimitive::Arithmetic(op @ (FixnumArithmetic::Inc | FixnumArithmetic::Dec)),
            &[arg],
        ) => {
            let value = unbox(emit, arg);
            let one = emit.value(SsaOp::ConstI64(1), TypeTag::Fixnum);
            guard(emit, checked_arithmetic(op, value, one))
        }
        (FixnumPrimitive::Arithmetic(FixnumArithmetic::Sub), &[arg]) => {
            let value = unbox(emit, arg);
            let zero = emit.value(SsaOp::ConstI64(0), TypeTag::Fixnum);
            guard(emit, SsaOp::SubI64Checked(zero, value))
        }
        (FixnumPrimitive::Arithmetic(op), [first, rest @ ..])
            if op.arity() == 2 && !rest.is_empty() =>
        {
            // Check every argument before emitting the first arithmetic operation.
            let first = unbox(emit, *first);
            let rest: Vec<_> = rest.iter().map(|&arg| unbox(emit, arg)).collect();
            rest.into_iter().fold(first, |acc, rhs| {
                guard(emit, checked_arithmetic(op, acc, rhs))
            })
        }
        _ => return false,
    };
    emit.push(
        Some(result),
        SsaOp::BoxFixnum(unboxed_result),
        TypeTag::Fixnum,
        None,
    );
    true
}
