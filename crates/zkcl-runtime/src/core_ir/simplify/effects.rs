//! Conservative effects derived from `zkcl_bytecode::OpEffects` when discarding Core expressions.

use zkcl_bytecode::{AliasDomain, BarrierEffect, ControlEffect, OpEffects};

use super::super::{
    CoreBindingTarget, CoreCallee, CoreExpr, CoreKind, InlineDeclaration, VariableBinding,
};
use super::inference::is_the_trivially_proven;
use crate::symbol::Name;

fn is_pure_total_call(name: &Name, args: &[CoreExpr]) -> bool {
    let op = name.operator();
    if op.is_empty() {
        return false;
    }
    match args.len() {
        1 => matches!(
            op,
            "NULL"
                | "NOT"
                | "ATOM"
                | "CONSP"
                | "LISTP"
                | "SYMBOLP"
                | "KEYWORDP"
                | "NUMBERP"
                | "REALP"
                | "RATIONALP"
                | "INTEGERP"
                | "FLOATP"
                | "COMPLEXP"
                | "CHARACTERP"
                | "STRINGP"
                | "VECTORP"
                | "ARRAYP"
                | "FUNCTIONP"
                | "COMPILED-FUNCTION-P"
                | "IDENTITY"
                | "VALUES"
        ),
        2 => matches!(op, "EQ" | "EQL" | "VALUES"),
        _ => op == "VALUES",
    }
}

fn is_diverging_structural_predicate(name: &Name, args: &[CoreExpr]) -> bool {
    args.len() == 2 && matches!(name.operator(), "EQUAL" | "EQUALP")
}

fn is_pure_alloc_call(name: &Name, args: &[CoreExpr]) -> bool {
    let op = name.operator();
    match op {
        "CONS" => args.len() == 2,
        "LIST" => true,
        "LIST*" => !args.is_empty(),
        _ => false,
    }
}

/// Computes the conservative [`OpEffects`] of `expr`.
fn expr_effect(expr: &CoreExpr, vars: &[VariableBinding]) -> OpEffects {
    const READ_SPECIAL: OpEffects = OpEffects {
        reads: AliasDomain::SYMBOL_VALUE,
        writes: AliasDomain::EMPTY,
        control: ControlEffect::MAY_UNWIND,
        barrier: BarrierEffect::MAY_GC,
    };
    const READ_GLOBAL_FN: OpEffects = OpEffects {
        reads: AliasDomain::SYMBOL_FN,
        writes: AliasDomain::EMPTY,
        control: ControlEffect::MAY_UNWIND,
        barrier: BarrierEffect::MAY_GC,
    };
    const FRESH_ALLOC: OpEffects = OpEffects {
        reads: AliasDomain::EMPTY,
        writes: AliasDomain::EMPTY,
        control: ControlEffect::EMPTY,
        barrier: BarrierEffect::ALLOC_FRESH.union(BarrierEffect::MAY_GC),
    };

    match &expr.kind {
        CoreKind::Const(_) | CoreKind::LoadTimeValue { .. } => OpEffects::PURE,
        CoreKind::ReadVar(v) => {
            if vars.get(v.index()).is_some_and(|info| info.special) {
                READ_SPECIAL
            } else {
                OpEffects::PURE
            }
        }
        CoreKind::ReadGlobal(_) => READ_SPECIAL,
        CoreKind::ReadGlobalFn { .. } => READ_GLOBAL_FN,
        CoreKind::ReadLocalFn(_) => FRESH_ALLOC,
        CoreKind::WriteVar { var, value } => {
            let domain = if vars.get(var.index()).is_some_and(|info| info.special) {
                AliasDomain::SYMBOL_VALUE
            } else {
                AliasDomain::BOX_CELL
            };
            expr_effect(value, vars)
                | OpEffects {
                    writes: domain,
                    ..OpEffects::PURE
                }
        }
        CoreKind::WriteGlobal { value, .. } => {
            expr_effect(value, vars)
                | OpEffects {
                    writes: AliasDomain::SYMBOL_VALUE,
                    ..OpEffects::PURE
                }
        }
        CoreKind::Progn(es) => es
            .iter()
            .fold(OpEffects::PURE, |a, e| a | expr_effect(e, vars)),
        CoreKind::If { test, then_, else_ } => {
            expr_effect(test, vars) | expr_effect(then_, vars) | expr_effect(else_, vars)
        }
        CoreKind::Let { bindings, body, .. } => {
            let mut eff = expr_effect(body, vars);
            for (target, rhs) in bindings {
                eff |= expr_effect(rhs, vars);
                let special = match target {
                    CoreBindingTarget::Special(_) => true,
                    CoreBindingTarget::Lexical(v) => vars.get(v.index()).is_some_and(|i| i.special),
                };
                if special {
                    eff |= OpEffects {
                        reads: AliasDomain::DYNAMIC_STACK,
                        writes: AliasDomain::DYNAMIC_STACK.union(AliasDomain::SYMBOL_VALUE),
                        control: ControlEffect::EMPTY,
                        barrier: BarrierEffect::REQUIRED_WHEN_UNUSED,
                    };
                }
            }
            eff
        }
        CoreKind::Flet { body, .. } | CoreKind::Block { body, .. } => expr_effect(body, vars),
        CoreKind::ReturnFrom { value, .. } => {
            expr_effect(value, vars)
                | OpEffects {
                    control: ControlEffect::MAY_UNWIND,
                    ..OpEffects::PURE
                }
        }
        CoreKind::Tagbody { segments, .. } => segments
            .iter()
            .flat_map(|(_, es)| es)
            .fold(OpEffects::PURE, |a, e| a | expr_effect(e, vars)),
        CoreKind::Go { .. } => OpEffects {
            control: ControlEffect::MAY_UNWIND.union(ControlEffect::MAY_DIVERGE),
            ..OpEffects::PURE
        },
        CoreKind::Call {
            callee,
            args,
            inline_declaration,
            ..
        } => {
            let mut eff = args
                .iter()
                .fold(OpEffects::PURE, |a, e| a | expr_effect(e, vars));
            if *inline_declaration != Some(InlineDeclaration::NotInline)
                && let CoreCallee::Named(n) = callee
            {
                if is_pure_total_call(n, args) {
                    return eff;
                }
                if is_diverging_structural_predicate(n, args) {
                    // EQUAL/EQUALP may diverge on circular structures.
                    return eff
                        | OpEffects {
                            reads: AliasDomain::ALL_MEM,
                            writes: AliasDomain::EMPTY,
                            control: ControlEffect::MAY_DIVERGE,
                            barrier: BarrierEffect::EMPTY,
                        };
                }
                if is_pure_alloc_call(n, args) {
                    return eff | FRESH_ALLOC;
                }
            }
            if let CoreCallee::Expr(e) = callee {
                eff |= expr_effect(e, vars);
            }
            eff | OpEffects::UNKNOWN_CALL
        }
        CoreKind::Lambda { func } => {
            if func.has_captures() {
                FRESH_ALLOC
            } else {
                OpEffects::PURE
            }
        }
        CoreKind::The {
            type_spec,
            expr,
            safety,
        } => {
            let inner = expr_effect(expr, vars);
            if *safety == 0 || is_the_trivially_proven(type_spec, expr) {
                inner
            } else {
                inner | OpEffects::CL_SIGNAL
            }
        }
        CoreKind::RawSpecial(..) | CoreKind::CallValues { .. } => OpEffects::UNKNOWN_CALL,
    }
}

/// Returns `true` if `expr` can be discarded when its value is unused.
pub(super) fn is_pure_expr(expr: &CoreExpr, vars: &[VariableBinding]) -> bool {
    expr_effect(expr, vars).can_eliminate_if_unused()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named_call(op: &str, args: Vec<CoreExpr>) -> CoreExpr {
        CoreExpr::new(
            0,
            CoreKind::Call {
                callee: CoreCallee::Named(Name::common_lisp(op)),
                args,
                qualities: crate::core_ir::OptimizationQualities::default(),
                inline_declaration: None,
                ftype_ret: crate::core_ir::CoreType::Top,
                fixnum_arithmetic: None,
            },
        )
    }

    #[test]
    fn diverging_structural_equality_is_not_discardable() {
        let a = CoreExpr::nil(0);
        let b = CoreExpr::nil(0);
        assert!(is_pure_expr(
            &named_call("EQ", vec![a.clone(), b.clone()]),
            &[]
        ));
        assert!(is_pure_expr(
            &named_call("EQL", vec![a.clone(), b.clone()]),
            &[]
        ));
        assert!(is_pure_expr(
            &named_call("CONS", vec![a.clone(), b.clone()]),
            &[]
        ));
        assert!(!is_pure_expr(
            &named_call("EQUAL", vec![a.clone(), b.clone()]),
            &[]
        ));
        assert!(!is_pure_expr(&named_call("EQUALP", vec![a, b]), &[]));
    }
}
