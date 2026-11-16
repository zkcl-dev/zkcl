//! Type and range proofs over the current lexical path.

use super::super::types::{type_of_literal_form, type_of_symbol_name};
use super::super::{
    CoreCallee, CoreConst, CoreExpr, CoreKind, CoreType, CoreValuesSpec, InlineDeclaration,
    IntegerRange, ParsedTheType, VarId, parse_the_type,
};
use super::PropagationEnv;
use crate::read::Form;
use crate::symbol::Name;
use zkcl_bytecode::FixnumArithmetic;

pub(super) fn const_type(c: &CoreConst) -> CoreType {
    match c {
        CoreConst::Nil => CoreType::Null,
        CoreConst::True => CoreType::True,
        CoreConst::Fixnum(_) => CoreType::Fixnum,
        CoreConst::Char(_) => CoreType::Character,
        CoreConst::String(_) => CoreType::String,
        CoreConst::Symbol(n) => type_of_symbol_name(n),
        CoreConst::LiteralForm(f) => type_of_literal_form(f),
    }
}

pub(super) fn infer_expr_type(expr: &CoreExpr, env: &PropagationEnv<'_>) -> CoreType {
    infer_type(expr, Some(env))
}

fn infer_type(expr: &CoreExpr, env: Option<&PropagationEnv<'_>>) -> CoreType {
    let infer = |expr| infer_type(expr, env);
    match &expr.kind {
        CoreKind::Const(c) => const_type(c),
        CoreKind::ReadVar(var) => env.map_or(CoreType::Top, |env| {
            if let Some(constant) = env.var_const.get(var) {
                const_type(constant)
            } else {
                env.type_constraints(*var).upper_bound
            }
        }),
        CoreKind::Lambda { .. } | CoreKind::ReadLocalFn(_) => CoreType::Function,
        CoreKind::WriteVar { value, .. } | CoreKind::WriteGlobal { value, .. } => infer(value),
        CoreKind::Progn(es) => es.last().map_or(CoreType::Null, infer),
        CoreKind::If { then_, else_, .. } => infer(then_).join(infer(else_)),
        CoreKind::Let { body, .. } | CoreKind::Flet { body, .. } => infer(body),
        CoreKind::Block { id, body } => {
            if env.is_some_and(|env| !env.blocks[id.index()].used) {
                infer(body)
            } else {
                CoreType::Top
            }
        }
        CoreKind::Tagbody { .. } | CoreKind::Go { .. } => CoreType::Null,
        CoreKind::ReturnFrom { .. } => CoreType::Empty,
        CoreKind::The {
            type_spec, expr, ..
        } => infer(expr).meet(parse_the_type(type_spec).primary_type()),
        CoreKind::Call {
            callee,
            args,
            inline_declaration,
            ftype_ret,
            ..
        } => {
            let declared = env.map_or(CoreType::Top, |_| *ftype_ret);
            match callee {
                CoreCallee::Named(name)
                    if *inline_declaration != Some(InlineDeclaration::NotInline) =>
                {
                    builtin_return_type(name, args, infer).meet(declared)
                }
                _ => declared,
            }
        }

        CoreKind::ReadGlobal(_)
        | CoreKind::ReadGlobalFn { .. }
        | CoreKind::CallValues { .. }
        | CoreKind::LoadTimeValue { .. }
        | CoreKind::RawSpecial(..) => CoreType::Top,
    }
}

pub(in crate::core_ir) fn builtin_return_type<'a>(
    name: &Name,
    args: &'a [CoreExpr],
    infer: impl Fn(&'a CoreExpr) -> CoreType,
) -> CoreType {
    let op = name.operator();
    match op {
        "CONS" if args.len() == 2 => CoreType::Cons,
        "LIST" => {
            if args.is_empty() {
                CoreType::Null
            } else {
                CoreType::Cons
            }
        }
        "LIST*" if !args.is_empty() => {
            if args.len() == 1 {
                infer(&args[0])
            } else {
                CoreType::Cons
            }
        }
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
        | "EQ"
        | "EQL"
        | "EQUAL"
        | "EQUALP"
        | "="
        | "/="
        | "<"
        | "<="
        | ">"
        | ">="
        | "ZEROP"
        | "PLUSP"
        | "MINUSP"
        | "EVENP"
        | "ODDP" => CoreType::Boolean,
        "IDENTITY" if args.len() == 1 => infer(&args[0]),
        "VALUES" => args.first().map_or(CoreType::Null, infer),
        "+" | "-" | "*" | "1+" | "1-" => {
            numeric_return_type(args.iter().map(infer), CoreType::Integer, CoreType::Number)
        }
        "/" => numeric_return_type(args.iter().map(infer), CoreType::Rational, CoreType::Number),
        "MOD" | "REM" => {
            numeric_return_type(args.iter().map(infer), CoreType::Integer, CoreType::Real)
        }
        "ASH" | "FLOOR" | "CEILING" | "TRUNCATE" | "ROUND" => CoreType::Integer,
        "FFLOOR" | "FCEILING" | "FTRUNCATE" | "FROUND" => CoreType::Float,
        "ABS" if args.len() == 1 => match infer(&args[0]) {
            CoreType::Fixnum | CoreType::Bignum | CoreType::Integer => CoreType::Integer,
            ty @ (CoreType::Ratio | CoreType::Rational | CoreType::Float) => ty,
            _ => CoreType::Real,
        },
        "SIGNUM" if args.len() == 1 => match infer(&args[0]) {
            CoreType::Fixnum
            | CoreType::Bignum
            | CoreType::Integer
            | CoreType::Ratio
            | CoreType::Rational => CoreType::Fixnum,
            ty @ (CoreType::Float | CoreType::Real | CoreType::Complex) => ty,
            _ => CoreType::Number,
        },
        "MIN" | "MAX" if !args.is_empty() => {
            // The VM returns one of its real arguments without converting it.
            let mut bound = CoreType::Empty;
            for arg in args {
                bound = bound.join(infer(arg).meet(CoreType::Real));
                if bound == CoreType::Real {
                    break;
                }
            }
            bound
        }
        _ => CoreType::Top,
    }
}

fn numeric_return_type(
    operands: impl Iterator<Item = CoreType>,
    mut result: CoreType,
    unknown: CoreType,
) -> CoreType {
    let mut has_float = false;
    for ty in operands {
        // Inference recurses into the operand. Visit it once even when a
        // nested numeric expression has an unknown input type.
        match ty {
            CoreType::Empty | CoreType::Fixnum | CoreType::Bignum | CoreType::Integer => {}
            CoreType::Ratio | CoreType::Rational => result = result.join(CoreType::Rational),
            CoreType::Float | CoreType::Real => {
                has_float |= ty == CoreType::Float;
                result = CoreType::Real;
            }
            _ => return unknown,
        }
    }
    if has_float { CoreType::Float } else { result }
}

#[cfg(test)]
#[test]
fn numeric_return_type_visits_each_operand_at_most_once() {
    use std::cell::Cell;
    let op = Name::common_lisp("+");
    let args = [CoreExpr::nil(0), CoreExpr::nil(1)];
    for (rhs, expected) in [
        (CoreType::Fixnum, CoreType::Integer),
        (CoreType::Ratio, CoreType::Rational),
        (CoreType::Float, CoreType::Float),
        (CoreType::Real, CoreType::Real),
        (CoreType::Top, CoreType::Number),
    ] {
        let calls = [Cell::new(0), Cell::new(0)];
        let result = builtin_return_type(&op, &args, |arg| {
            calls[arg.at].update(|count| count + 1);
            [CoreType::Fixnum, rhs][arg.at]
        });
        assert_eq!(result, expected);
        assert_eq!(calls.map(Cell::into_inner), [1, 1]);
    }
}

pub(crate) fn is_single_value_expr(expr: &CoreExpr) -> bool {
    match &expr.kind {
        CoreKind::Const(_)
        | CoreKind::ReadVar(_)
        | CoreKind::ReadGlobal(_)
        | CoreKind::ReadGlobalFn { .. }
        | CoreKind::ReadLocalFn(_)
        | CoreKind::WriteVar { .. }
        | CoreKind::WriteGlobal { .. }
        | CoreKind::Lambda { .. }
        | CoreKind::LoadTimeValue { .. }
        | CoreKind::Tagbody { .. } => true,
        CoreKind::Progn(es) => es.last().is_none_or(is_single_value_expr),
        CoreKind::If { then_, else_, .. } => {
            is_single_value_expr(then_) && is_single_value_expr(else_)
        }
        CoreKind::Let { body, .. } | CoreKind::Flet { body, .. } => is_single_value_expr(body),
        CoreKind::The { expr, .. } => is_single_value_expr(expr),
        CoreKind::Call {
            callee: CoreCallee::Named(name),
            args,
            inline_declaration: None | Some(InlineDeclaration::Inline),
            ..
        } => match name.operator() {
            "VALUES" => args.len() == 1,
            "+" | "-" | "*" | "1+" | "1-" | "=" | "/=" | "<" | ">" | "<=" | ">=" | "NULL"
            | "NOT" | "EQ" | "EQL" | "EQUAL" | "EQUALP" | "CONS" | "LIST" | "LIST*" | "CAR"
            | "CDR" | "IDENTITY" | "ATOM" | "CONSP" | "LISTP" | "SYMBOLP" | "KEYWORDP"
            | "NUMBERP" | "INTEGERP" | "STRINGP" | "CHARACTERP" | "FUNCTIONP" => args.len() <= 8,
            _ => false,
        },
        _ => false,
    }
}

fn values_spec_is_proven(
    vspec: &CoreValuesSpec,
    expr: &CoreExpr,
    env: Option<&PropagationEnv<'_>>,
) -> bool {
    if vspec
        .required
        .iter()
        .chain(&vspec.optional)
        .chain(vspec.rest.as_ref())
        .all(|(_, ty, exact)| (*ty, *exact) == (CoreType::Top, true))
    {
        return true;
    }
    let values = match &expr.kind {
        CoreKind::Call {
            callee: CoreCallee::Named(n),
            args,
            inline_declaration: None | Some(InlineDeclaration::Inline),
            ..
        } if n.operator() == "VALUES" => args.as_slice(),
        _ if is_single_value_expr(expr) => std::slice::from_ref(expr),
        _ => return false,
    };
    let proves_type = |(_, ty, exact): &(Form, CoreType, bool), value: Option<&CoreExpr>| {
        *exact
            && (*ty == CoreType::Top
                || value
                    .map(|arg| infer_type(arg, env))
                    .unwrap_or(CoreType::Null)
                    .is_subtype(*ty))
    };
    if !vspec
        .required
        .iter()
        .enumerate()
        .all(|(i, spec)| proves_type(spec, values.get(i)))
    {
        return false;
    }
    let remaining = values.get(vspec.required.len()..).unwrap_or_default();
    vspec
        .optional
        .iter()
        .zip(remaining)
        .all(|(spec, arg)| proves_type(spec, Some(arg)))
        && vspec.rest.as_ref().is_none_or(|spec| {
            remaining
                .iter()
                .skip(vspec.optional.len())
                .all(|arg| proves_type(spec, Some(arg)))
        })
}

pub(super) fn is_the_trivially_proven(type_spec: &Form, expr: &CoreExpr) -> bool {
    prove_the_type(type_spec, expr, None)
}

pub(super) fn is_the_proven(type_spec: &Form, expr: &CoreExpr, env: &PropagationEnv<'_>) -> bool {
    if let Some(required) = env.fixnums.spec(type_spec) {
        if let Some(actual) = infer_range(expr, env) {
            return required.contains(actual);
        }
        return required == env.fixnums && infer_expr_type(expr, env).is_subtype(CoreType::Fixnum);
    }
    prove_the_type(type_spec, expr, Some(env))
}

fn prove_the_type(type_spec: &Form, expr: &CoreExpr, env: Option<&PropagationEnv<'_>>) -> bool {
    match parse_the_type(type_spec) {
        ParsedTheType::Single { ty, exact } => {
            exact && (ty == CoreType::Top || infer_type(expr, env).is_subtype(ty))
        }
        ParsedTheType::Values(spec) => values_spec_is_proven(&spec, expr, env),
    }
}

pub(super) fn infer_range(expr: &CoreExpr, env: &PropagationEnv<'_>) -> Option<IntegerRange> {
    match &expr.kind {
        CoreKind::Const(CoreConst::Fixnum(n)) => Some(IntegerRange::singleton(*n)),
        CoreKind::ReadVar(var) => {
            let constraints = env.type_constraints(*var);
            constraints
                .integer_range
                .or_else(|| (constraints.upper_bound == CoreType::Fixnum).then_some(env.fixnums))
        }
        CoreKind::The { type_spec, .. } => env.fixnums.spec(type_spec),
        CoreKind::Call {
            callee: CoreCallee::Named(name),
            args,
            inline_declaration: None | Some(InlineDeclaration::Inline),
            ..
        } => arithmetic_range(name.operator(), args, env).map(|(_, range)| range),
        _ => None,
    }
}

pub(super) fn arithmetic_range(
    name: &str,
    args: &[CoreExpr],
    env: &PropagationEnv<'_>,
) -> Option<(FixnumArithmetic, IntegerRange)> {
    use FixnumArithmetic::{Add, Dec, Inc, Mul, Sub};
    let (op, lhs, rhs) = match (name, args) {
        ("1+", [lhs]) => (Inc, lhs, IntegerRange::singleton(1)),
        ("1-", [lhs]) => (Dec, lhs, IntegerRange::singleton(1)),
        ("+", [lhs, rhs]) => (Add, lhs, infer_range(rhs, env)?),
        ("-", [lhs, rhs]) => (Sub, lhs, infer_range(rhs, env)?),
        ("*", [lhs, rhs]) => (Mul, lhs, infer_range(rhs, env)?),
        _ => return None,
    };
    Some((op, infer_range(lhs, env)?.arithmetic(op, rhs, env.fixnums)?))
}

/// Intersect the incoming fixnum interval with each edge of an integer comparison.
/// An unconstrained real operand might be a float or a ratio.
pub(super) fn comparison_branch_ranges(
    test: &CoreExpr,
    env: &PropagationEnv<'_>,
) -> Option<(VarId, IntegerRange, IntegerRange)> {
    let CoreKind::Call {
        callee: CoreCallee::Named(name),
        args,
        inline_declaration: None | Some(InlineDeclaration::Inline),
        ..
    } = &test.kind
    else {
        return None;
    };
    let (less, inclusive) = match name.operator() {
        "<" => (true, false),
        "<=" => (true, true),
        ">" => (false, false),
        ">=" => (false, true),
        _ => return None,
    };
    let [lhs, rhs] = args.as_slice() else {
        return None;
    };
    let (operand, limit, less) = match (&lhs.kind, &rhs.kind) {
        (CoreKind::ReadVar(_), CoreKind::Const(CoreConst::Fixnum(limit))) => (lhs, *limit, less),
        (CoreKind::Const(CoreConst::Fixnum(limit)), CoreKind::ReadVar(_)) => (rhs, *limit, !less),
        _ => return None,
    };
    let CoreKind::ReadVar(var) = operand.kind else {
        return None;
    };
    let binding = &env.vars[var.index()];
    if binding.assigned || binding.special {
        return None;
    }
    let domain = infer_range(operand, env)?;
    // cut is the first integer in the upper half, whichever edge reaches it.
    let cut = i128::from(limit) + i128::from(inclusive == less);
    let [low, high] = domain.split_at(cut)?;
    let (yes, no) = if less { (low, high) } else { (high, low) };
    Some((var, yes, no))
}
