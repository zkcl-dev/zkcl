use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::Immediate;

use super::cfg::{simplify_cfg, visit_control_targets};
use super::dom::DomTree;
use super::propagate::propagate_constants_and_copies;
use super::rewrite::rewrite_all_uses;
use crate::ir::{
    CallSiteProfile, DeoptRegisterValue, DeoptState, FloatArithOp, FloatFormat, GuardedPrimitive,
    InstBuilder, NumericOperand, SpecializationInputs, SsaFunction, SsaInst, SsaOp, Terminator,
    TypeTag, ValueId, ValueRepr,
};

pub(super) fn profiled_arithmetic(
    profile: CallSiteProfile,
    symbol: Option<(&str, &str)>,
) -> Option<(FloatFormat, FloatArithOp)> {
    let format = match profile {
        CallSiteProfile::FloatArithmetic {
            lhs: NumericOperand::F32,
            rhs: NumericOperand::F32,
        } => FloatFormat::F32,
        CallSiteProfile::FloatArithmetic {
            lhs: NumericOperand::F64,
            rhs: NumericOperand::F64,
        } => FloatFormat::F64,
        _ => return None,
    };
    // Production JIT metadata identifies the canonical call. The native guard checks
    // the current function cell, which privileged operators such as TRACE may change.
    let ("COMMON-LISP" | "CL", name) = symbol? else {
        return None;
    };
    let op = match name {
        "+" => FloatArithOp::Add,
        "-" => FloatArithOp::Sub,
        "*" => FloatArithOp::Mul,
        "/" => FloatArithOp::Div,
        _ => return None,
    };
    Some((format, op))
}

pub(super) fn lower_call(
    emit: &mut InstBuilder<'_>,
    result: ValueId,
    symbol: usize,
    format: FloatFormat,
    op: FloatArithOp,
    [left, right]: [ValueId; 2],
    deopt: &DeoptState,
) {
    emit.push(
        None,
        SsaOp::GuardPrimitiveBinding {
            symbol,
            primitive: GuardedPrimitive::Arithmetic(op),
        },
        TypeTag::Top,
        Some(deopt.clone()),
    );
    let lhs = emit.guarded_value(
        SsaOp::UnboxFloatGuard {
            format,
            value: left,
        },
        TypeTag::Float,
        deopt.clone(),
    );
    let rhs = emit.guarded_value(
        SsaOp::UnboxFloatGuard {
            format,
            value: right,
        },
        TypeTag::Float,
        deopt.clone(),
    );
    let value = emit.guarded_value(
        SsaOp::FloatArithChecked {
            format,
            op,
            lhs,
            rhs,
        },
        TypeTag::Float,
        deopt.clone(),
    );
    emit.push(
        Some(result),
        SsaOp::BoxFloat { format, value },
        TypeTag::Float,
        None,
    );
}

type UnboxedFloatSource = (FloatFormat, ValueId);

/// A declaration check on a known native result needs neither a box nor a type test.
/// Preserve the named call's semantics if TYPEP has been traced or rebound.
fn fold_float_typep_calls(
    func: &mut SsaFunction,
    specialization: &dyn SpecializationInputs,
    box_sources: &FxHashMap<ValueId, UnboxedFloatSource>,
) -> bool {
    let type_specifier_formats: FxHashMap<_, _> = func
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .filter_map(|inst| {
            let (Some(id), &SsaOp::LoadConst(index)) = (inst.id, &inst.op) else {
                return None;
            };
            let format = match specialization.named_symbol(index) {
                Some(("COMMON-LISP" | "CL", "SINGLE-FLOAT")) => FloatFormat::F32,
                Some(("COMMON-LISP" | "CL", "DOUBLE-FLOAT")) => FloatFormat::F64,
                _ => return None,
            };
            Some((id, format))
        })
        .collect();
    if type_specifier_formats.is_empty() {
        return false;
    }
    let mut changed = false;
    for block in &mut func.blocks {
        if !block.insts.iter().any(|inst| {
            matches!(&inst.op, SsaOp::CallNamed { symbol, .. }
                if matches!(specialization.named_symbol(*symbol), Some(("COMMON-LISP" | "CL", "TYPEP"))))
        }) {
            continue;
        }
        let old_insts = std::mem::take(&mut block.insts);
        block.insts.reserve(old_insts.len());
        for mut inst in old_insts {
            if let SsaOp::CallNamed {
                symbol,
                ref args,
                has_result: true,
                associate: false,
                notinline: false,
                ..
            } = inst.op
                && !specialization.is_notinline(symbol)
                && matches!(
                    specialization.named_symbol(symbol),
                    Some(("COMMON-LISP" | "CL", "TYPEP"))
                )
                && let &[value, type_specifier] = args.as_slice()
                && let Some(&(actual, _)) = box_sources.get(&value)
                && let Some(&expected) = type_specifier_formats.get(&type_specifier)
                && let Some(deopt) = inst.deopt.take()
            {
                let guard = SsaOp::GuardPrimitiveBinding {
                    symbol,
                    primitive: GuardedPrimitive::Typep,
                };
                block.insts.push(SsaInst {
                    id: None,

                    op: guard,
                    ty: TypeTag::Top,
                    pc: inst.pc,
                    deopt: Some(deopt),
                });
                let value = if actual == expected {
                    Immediate::True
                } else {
                    Immediate::Nil
                };
                inst.op = SsaOp::ConstWord(value);

                inst.ty = TypeTag::from_immediate(value);
                changed = true;
            }
            block.insts.push(inst);
        }
    }
    changed
}

/// Carry the checked result through same-format arithmetic and ordinary loop edges. Heap
/// allocations stay at tagged uses, and cold guard exits record how to restore raw floats.
pub(super) fn keep_unboxed(
    func: &mut SsaFunction,
    specialization: &dyn SpecializationInputs,
) -> bool {
    let mut box_sources = FxHashMap::default();
    let mut unbox_inputs = FxHashSet::default();
    for inst in func.blocks.iter().flat_map(|block| &block.insts) {
        match (inst.id, &inst.op) {
            (Some(id), SsaOp::BoxFloat { format, value }) => {
                box_sources.insert(id, (*format, *value));
            }
            (_, SsaOp::UnboxFloatGuard { format, value }) => {
                unbox_inputs.insert((*format, *value));
            }
            _ => {}
        }
    }
    if box_sources.is_empty() {
        return false;
    }
    let checks_folded = fold_float_typep_calls(func, specialization, &box_sources);
    if checks_folded {
        propagate_constants_and_copies(func);
        simplify_cfg(func);
    }
    let promoted = promote_loop_parameters(func, &box_sources, &unbox_inputs);
    let mut unboxed_sources = box_sources.clone();
    unboxed_sources.extend(
        promoted
            .iter()
            .map(|(&value, &format)| (value, (format, value))),
    );
    let mut rewrites = FxHashMap::default();
    for block in &mut func.blocks {
        let mut block_unbox_results = FxHashMap::default();
        block.insts.retain(|inst| {
            let (Some(id), &SsaOp::UnboxFloatGuard { format, value }) = (inst.id, &inst.op) else {
                return true;
            };
            let known = unboxed_sources
                .get(&value)
                .filter(|&&(source_format, _)| source_format == format)
                .map(|&(_, raw)| raw)
                .or_else(|| block_unbox_results.get(&(format, value)).copied());
            if let Some(raw) = known {
                rewrites.insert(id, raw);
                return false;
            }
            block_unbox_results.insert((format, value), id);
            true
        });
    }
    rewrite_all_uses(func, &rewrites);

    if !promoted.is_empty() {
        box_tagged_uses(func, &promoted);
    }
    // Keep the actual boxed object whenever any ordinary use could observe it.
    // Only the remaining deopt-only or dead boxes are replaced with lazy materialization.
    let mut deopt_sources = box_sources;
    for block in &func.blocks {
        for inst in &block.insts {
            inst.op.visit_inputs(|value| {
                deopt_sources.remove(&value);
            });
        }
        block.terminator.visit_inputs(|value| {
            deopt_sources.remove(&value);
        });
    }
    let changed =
        checks_folded || !promoted.is_empty() || !rewrites.is_empty() || !deopt_sources.is_empty();
    deopt_sources.extend(
        promoted
            .into_iter()
            .map(|(value, format)| (value, (format, value))),
    );
    if !deopt_sources.is_empty() {
        for block in &mut func.blocks {
            block.insts.retain_mut(|inst| {
                if inst
                    .id
                    .is_some_and(|value| deopt_sources.contains_key(&value))
                {
                    return false;
                }
                if let Some(deopt) = &mut inst.deopt {
                    deopt.rewrite_values(|kind| match kind {
                        DeoptRegisterValue::TaggedWord(value) => {
                            deopt_sources.get(&value).map_or(kind, |&(format, value)| {
                                DeoptRegisterValue::UnboxedFloat { format, value }
                            })
                        }
                        _ => kind,
                    });
                }
                true
            });
        }
    }
    changed
}

fn promote_loop_parameters(
    func: &mut SsaFunction,
    box_sources: &FxHashMap<ValueId, UnboxedFloatSource>,
    unbox_inputs: &FxHashSet<(FloatFormat, ValueId)>,
) -> FxHashMap<ValueId, FloatFormat> {
    if func.blocks.len() < 2
        || unbox_inputs.is_empty()
        || func.arity.maximum != Some(func.arity.required)
    {
        return FxHashMap::default();
    }
    let block_map = func.block_index_map();
    let dom = DomTree::compute(func, &block_map);
    let entry_values: FxHashSet<_> = func.entry_params.iter().copied().collect();
    let constants: FxHashMap<_, _> = func
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|inst| match (inst.id, &inst.op) {
            (Some(id), &SsaOp::LoadConst(index)) => Some((id, index)),
            _ => None,
        })
        .collect();
    let available_at_entry =
        |value: &ValueId| entry_values.contains(value) || constants.contains_key(value);
    let mut incoming = FxHashMap::<_, Vec<_>>::default();
    let mut control_targets = FxHashSet::default();
    for block in &func.blocks {
        visit_control_targets(block, |target| {
            control_targets.insert(target);
        });
        block.terminator.visit_targets(|target| {
            incoming
                .entry(target.block)
                .or_default()
                .push((block.id, &target.args));
        });
    }
    let mut params_to_unbox = FxHashMap::default();
    let mut promoted = FxHashMap::default();
    for block in &func.blocks {
        if block.id == func.entry || control_targets.contains(&block.id) {
            continue;
        }
        let Some(edges) = incoming.get(&block.id) else {
            continue;
        };
        for (index, param) in block.params.iter().enumerate() {
            if param.repr != ValueRepr::TaggedWord {
                continue;
            }
            let backedge_format = edges.iter().find_map(|&(from, args)| {
                let &(format, _) = box_sources.get(&args[index])?;
                dom.dominates(block.id, from).then_some(format)
            });
            let Some(format) = backedge_format else {
                continue;
            };
            if unbox_inputs.contains(&(format, param.value))
                && edges.iter().all(|&(_, args)| {
                    let arg = args[index];
                    arg == param.value
                        || available_at_entry(&arg)
                        || box_sources
                            .get(&arg)
                            .is_some_and(|&(found, _)| found == format)
                })
            {
                params_to_unbox.insert((block.id, index), (format, param.value));
                promoted.insert(param.value, format);
            }
        }
    }
    if params_to_unbox.is_empty() {
        return promoted;
    }

    // Guard required input values before any instruction has run. A failed guard may
    // resume at entry even when the loop would have had zero iterations.
    let entry_state = DeoptState {
        pc: 0,
        registers_to_restore: func
            .entry_params
            .iter()
            .enumerate()
            .map(|(reg, &value)| (reg as u32, DeoptRegisterValue::TaggedWord(value)))
            .collect(),
    };
    let mut entry_guards = Vec::new();
    let mut entry_unbox_results = FxHashMap::default();
    let mut loaded_constants = FxHashMap::default();
    for block in &mut func.blocks {
        for (position, param) in block.params.iter_mut().enumerate() {
            if let Some(&(format, _)) = params_to_unbox.get(&(block.id, position)) {
                param.repr = format.repr();
                param.ty = TypeTag::Float;
            }
        }
    }
    for index in 0..func.blocks.len() {
        let mut targets = Vec::new();
        func.blocks[index].terminator.visit_targets(|target| {
            for (position, &arg) in target.args.iter().enumerate() {
                if let Some(&(format, param)) = params_to_unbox.get(&(target.block, position))
                    && arg != param
                    && available_at_entry(&arg)
                {
                    targets.push((format, arg));
                }
            }
        });
        for key @ (format, value) in targets {
            entry_unbox_results.entry(key).or_insert_with(|| {
                let mut emit = InstBuilder {
                    next_value_id: &mut func.next_value_id,
                    insts: &mut entry_guards,
                    pc: Some(0),
                };
                let value = constants.get(&value).map_or(value, |&index| {
                    *loaded_constants
                        .entry(index)
                        .or_insert_with(|| emit.value(SsaOp::LoadConst(index), TypeTag::Any))
                });
                emit.guarded_value(
                    SsaOp::UnboxFloatGuard { format, value },
                    TypeTag::Float,
                    entry_state.clone(),
                )
            });
        }
        func.blocks[index].terminator.visit_targets_mut(|target| {
            for (position, arg) in target.args.iter_mut().enumerate() {
                if let Some(&(format, param)) = params_to_unbox.get(&(target.block, position))
                    && *arg != param
                {
                    *arg = box_sources
                        .get(arg)
                        .map_or_else(|| entry_unbox_results[&(format, *arg)], |&(_, raw)| raw);
                }
            }
        });
    }
    if !entry_guards.is_empty() {
        func.block_mut(func.entry)
            .expect("entry block")
            .insts
            .splice(..0, entry_guards);
    }
    promoted
}

fn box_tagged_uses(func: &mut SsaFunction, promoted: &FxHashMap<ValueId, FloatFormat>) {
    let target_reprs: FxHashMap<_, Vec<_>> = func
        .blocks
        .iter()
        .map(|block| {
            (
                block.id,
                block.params.iter().map(|param| param.repr).collect(),
            )
        })
        .collect();
    for index in 0..func.blocks.len() {
        let old_insts = std::mem::take(&mut func.blocks[index].insts);
        let mut new_insts = Vec::with_capacity(old_insts.len());
        let mut local_boxes = FxHashMap::default();
        let mut box_argument = |func: &mut SsaFunction, insts: &mut Vec<SsaInst>, pc, value| {
            let Some(&format) = promoted.get(&value) else {
                return value;
            };
            *local_boxes.entry(value).or_insert_with(|| {
                InstBuilder {
                    next_value_id: &mut func.next_value_id,
                    insts,
                    pc,
                }
                .value(SsaOp::BoxFloat { format, value }, TypeTag::Float)
            })
        };
        for mut inst in old_insts {
            let mut box_input = |value| box_argument(func, &mut new_insts, inst.pc, value);
            match &mut inst.op {
                SsaOp::BoxFloat { .. } | SsaOp::FloatArithChecked { .. } => {}
                SsaOp::WriteFloatVector {
                    setter,
                    array,
                    index,
                    ..
                } => {
                    *setter = box_input(*setter);
                    *array = box_input(*array);
                    *index = box_input(*index);
                }
                op => op.rewrite_inputs(box_input),
            }
            new_insts.push(inst);
        }

        let mut term = std::mem::replace(
            &mut func.blocks[index].terminator,
            Terminator::ReturnDynamicValues,
        );
        let pc = new_insts.last().and_then(|inst| inst.pc);
        let mut tagged = |value| box_argument(func, &mut new_insts, pc, value);
        match &mut term {
            Terminator::BranchTruthy { cond, .. } => *cond = tagged(*cond),
            Terminator::ReturnOne(_) | Terminator::ReturnValues(_) | Terminator::Unwind(_) => {
                term.rewrite_inputs(&mut tagged);
            }
            _ => {}
        }
        term.visit_targets_mut(|target| {
            for (arg, repr) in target.args.iter_mut().zip(&target_reprs[&target.block]) {
                if *repr == ValueRepr::TaggedWord {
                    *arg = tagged(*arg);
                }
            }
        });
        func.blocks[index].insts = new_insts;
        func.blocks[index].terminator = term;
    }
}
