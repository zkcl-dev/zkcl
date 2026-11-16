//! Scoped type and constant facts, folding and dead-expression elimination.

mod effects;
mod inference;

use rustc_hash::FxHashMap;

use super::type_predicates::{TypeTest, branch_type_test, stable_value_type};
use super::{
    BlockExitTarget, CoreBindingTarget, CoreCallee, CoreConst, CoreExpr, CoreFn, CoreKind,
    CoreType, InlineDeclaration, IntegerRange, LocalFunction, VarId, VariableBinding,
    fn_has_raw_special, has_raw_special, parse_the_type, restore_map,
};
use crate::heap::Word;
use crate::symbol::Name;
use effects::is_pure_expr;
#[cfg(test)]
pub(super) use inference::builtin_return_type;
pub(crate) use inference::is_single_value_expr;
use inference::{
    arithmetic_range, comparison_branch_ranges, const_type, infer_expr_type, infer_range,
    is_the_proven,
};

#[derive(Clone, Copy, PartialEq, Eq)]
struct TypeConstraints {
    upper_bound: CoreType,
    integer_range: Option<IntegerRange>,
    // Head of this binding's exclusions in the current lexical path.
    exclusion_head: Option<usize>,
}

impl TypeConstraints {
    const UNCONSTRAINED: Self = Self {
        upper_bound: CoreType::Top,
        integer_range: None,
        exclusion_head: None,
    };
}

struct PropagationEnv<'a> {
    fixnums: IntegerRange,
    vars: &'a [VariableBinding],
    blocks: &'a [BlockExitTarget],

    var_const: FxHashMap<VarId, CoreConst>,
    var_copy: FxHashMap<VarId, VarId>,
    variable_types: FxHashMap<VarId, TypeConstraints>,
    exclusions: Vec<(CoreType, Option<usize>)>,
}

struct PropagationUndo {
    exclusion_count: usize,
    consts: Vec<(VarId, Option<CoreConst>)>,
    copies: Vec<(VarId, Option<VarId>)>,
    type_constraints: Vec<(VarId, Option<TypeConstraints>)>,
}

impl<'a> PropagationEnv<'a> {
    fn new(
        fixnums: IntegerRange,
        vars: &'a [VariableBinding],
        blocks: &'a [BlockExitTarget],
    ) -> Self {
        let mut variable_types = FxHashMap::default();
        for (idx, info) in vars.iter().enumerate() {
            if !info.special && info.trusted_type != CoreType::Top {
                variable_types.insert(
                    VarId(idx as u32),
                    TypeConstraints {
                        upper_bound: stable_value_type(info.trusted_type),
                        ..TypeConstraints::UNCONSTRAINED
                    },
                );
            }
        }
        Self {
            fixnums,
            vars,
            blocks,
            var_const: FxHashMap::default(),
            var_copy: FxHashMap::default(),
            variable_types,
            exclusions: Vec::new(),
        }
    }

    fn begin_scope(&self) -> PropagationUndo {
        PropagationUndo {
            exclusion_count: self.exclusions.len(),
            consts: Vec::new(),
            copies: Vec::new(),
            type_constraints: Vec::new(),
        }
    }

    fn set_const(&mut self, undo: &mut PropagationUndo, var: VarId, c: CoreConst) {
        undo.consts.push((var, self.var_const.insert(var, c)));
    }

    fn set_copy(&mut self, undo: &mut PropagationUndo, var: VarId, src: VarId) {
        undo.copies.push((var, self.var_copy.insert(var, src)));
    }

    fn type_constraints(&self, var: VarId) -> TypeConstraints {
        self.variable_types
            .get(&var)
            .copied()
            .unwrap_or(TypeConstraints::UNCONSTRAINED)
    }

    fn update_type_constraints(
        &mut self,
        undo: &mut PropagationUndo,
        var: VarId,
        update: impl FnOnce(&mut TypeConstraints),
    ) {
        let previous = self.variable_types.get(&var).copied();
        let mut next = previous.unwrap_or(TypeConstraints::UNCONSTRAINED);
        update(&mut next);
        if next != previous.unwrap_or(TypeConstraints::UNCONSTRAINED) {
            undo.type_constraints.push((var, previous));
            self.variable_types.insert(var, next);
        }
    }

    fn narrow_type(&mut self, undo: &mut PropagationUndo, var: VarId, ty: CoreType) {
        self.update_type_constraints(undo, var, |constraints| {
            constraints.upper_bound = constraints.upper_bound.meet(stable_value_type(ty))
        });
    }

    fn intersect_integer_range(
        &mut self,
        undo: &mut PropagationUndo,
        var: VarId,
        range: IntegerRange,
    ) {
        self.update_type_constraints(undo, var, |constraints| {
            constraints.integer_range = constraints
                .integer_range
                .map_or(Some(range), |old| range.intersection(old));
            constraints.upper_bound = if constraints.integer_range.is_some() {
                constraints.upper_bound.meet(CoreType::Fixnum)
            } else {
                CoreType::Empty
            };
        });
    }

    fn exclude_type(&mut self, undo: &mut PropagationUndo, var: VarId, ty: CoreType) {
        let current = self.type_constraints(var);
        let upper_bound = current.upper_bound.excluding(ty);
        let exclusion_head = if upper_bound.is_disjoint(ty) {
            current.exclusion_head
        } else {
            let head = self.exclusions.len();
            self.exclusions.push((ty, current.exclusion_head));
            Some(head)
        };
        self.update_type_constraints(undo, var, |constraints| {
            constraints.upper_bound = upper_bound;
            constraints.exclusion_head = exclusion_head;
        });
    }

    fn excludes_type(&self, var: VarId, ty: CoreType) -> bool {
        let mut head = self.type_constraints(var).exclusion_head;
        while let Some(index) = head {
            let (excluded, previous) = self.exclusions[index];
            if ty.is_subtype(excluded) {
                return true;
            }
            head = previous;
        }
        false
    }

    fn end_scope(&mut self, undo: PropagationUndo) {
        self.exclusions.truncate(undo.exclusion_count);
        for (k, prev) in undo.consts.into_iter().rev() {
            restore_map(&mut self.var_const, k, prev);
        }
        for (k, prev) in undo.copies.into_iter().rev() {
            restore_map(&mut self.var_copy, k, prev);
        }
        for (k, prev) in undo.type_constraints.into_iter().rev() {
            restore_map(&mut self.variable_types, k, prev);
        }
    }

    fn resolve_copy(&self, mut var: VarId) -> VarId {
        let mut steps = 0;
        while let Some(&next) = self.var_copy.get(&var) {
            if next == var || steps >= 16 {
                break;
            }
            var = next;
            steps += 1;
        }
        var
    }
}

fn is_duplicable_const(c: &CoreConst) -> bool {
    matches!(
        c,
        CoreConst::Nil
            | CoreConst::True
            | CoreConst::Fixnum(_)
            | CoreConst::Char(_)
            | CoreConst::Symbol(_)
    )
}

pub(super) fn fold_and_simplify(fn_ir: &mut CoreFn) -> bool {
    let mut env = PropagationEnv::new(fn_ir.fixnums, &fn_ir.vars, &fn_ir.blocks);
    let locals = &mut fn_ir.local_functions;
    let mut changed = false;
    fn_ir.params.for_each_init_mut(&mut |init| {
        changed |= fold_expr(init, &mut env, locals);
    });
    for index in 0..locals.len() {
        let binding = &locals[index].binding;
        if binding.call_count == 0 && !binding.escapes {
            continue;
        }
        // A definition can contain nested FLETs. Keep their entries accessible
        // while folding this body's bindings and default arguments in place.
        let (mut params, mut body) = locals[index].take_definition();
        params.for_each_init_mut(&mut |init| {
            changed |= fold_expr(init, &mut env, locals);
        });
        changed |= fold_expr(&mut body, &mut env, locals);
        locals[index].params = params;
        locals[index].body = body;
    }
    changed |= fold_expr(&mut fn_ir.body, &mut env, locals);
    changed
}

fn subst_const_in_func_expr(
    expr: &mut CoreExpr,
    local_functions: &mut [LocalFunction],
    target: VarId,
    c: &CoreConst,
) -> bool {
    if matches!(expr.kind, CoreKind::ReadVar(var) if var == target) {
        expr.kind = CoreKind::Const(c.clone());
        return true;
    }

    let mut changed = false;
    if let CoreKind::Flet { fns, .. } = &expr.kind {
        for &id in fns {
            let (mut params, mut body) = local_functions[id.index()].take_definition();
            params.for_each_init_mut(&mut |init| {
                changed |= subst_const_in_func_expr(init, local_functions, target, c);
            });
            changed |= subst_const_in_func_expr(&mut body, local_functions, target, c);
            let local = &mut local_functions[id.index()];
            local.params = params;
            local.body = body;
        }
    }
    expr.for_each_child_mut(|child| {
        changed |= subst_const_in_func_expr(child, local_functions, target, c);
    });
    changed
}

fn fold_if_branches(
    test: &CoreExpr,
    then_: &mut CoreExpr,
    else_: &mut CoreExpr,
    env: &mut PropagationEnv<'_>,
    locals: &[LocalFunction],
) -> bool {
    let mut changed = false;
    let type_test = branch_type_test(test, env.fixnums).filter(|(var, _)| {
        let info = &env.vars[var.index()];
        !info.special && !info.assigned
    });
    let integer_ranges = comparison_branch_ranges(test, env);
    if type_test.is_none() && integer_ranges.is_none() {
        changed |= fold_expr(then_, env, locals);
        changed |= fold_expr(else_, env, locals);
        return changed;
    }
    for (branch, taken) in [(then_, true), (else_, false)] {
        if matches!(branch.kind, CoreKind::Const(_)) {
            continue;
        }
        let mut undo = env.begin_scope();
        if let Some((var, test)) = type_test {
            if taken != test.negated {
                if let Some(range) = test.range {
                    env.intersect_integer_range(&mut undo, var, range);
                } else {
                    env.narrow_type(&mut undo, var, test.ty);
                }
            } else if test.range.is_none() && test.ty != CoreType::Keyword {
                env.exclude_type(&mut undo, var, test.ty);
            }
            if env.type_constraints(var).upper_bound == CoreType::Null {
                env.set_const(&mut undo, var, CoreConst::Nil);
            }
        }
        if let Some((var, then_range, else_range)) = integer_ranges {
            env.intersect_integer_range(
                &mut undo,
                var,
                if taken { then_range } else { else_range },
            );
        }
        changed |= fold_expr(branch, env, locals);
        env.end_scope(undo);
    }
    changed
}

fn fold_expr(expr: &mut CoreExpr, env: &mut PropagationEnv<'_>, locals: &[LocalFunction]) -> bool {
    let mut changed = false;
    match &mut expr.kind {
        CoreKind::Const(_)
        | CoreKind::ReadGlobal(_)
        | CoreKind::ReadGlobalFn { .. }
        | CoreKind::ReadLocalFn(_)
        | CoreKind::Go { .. }
        | CoreKind::LoadTimeValue { .. }
        | CoreKind::RawSpecial(..) => return false,
        CoreKind::Lambda { func } => {
            if !fn_has_raw_special(func) {
                let mut sub_changed = false;
                for &(child_v, parent_v) in &func.parent_var_captures {
                    if let Some(c) = env.var_const.get(&parent_v)
                        && !env.vars[parent_v.index()].assigned
                        && !func.vars[child_v.index()].assigned
                        && !func.vars[child_v.index()].special
                        && !func.vars[child_v.index()].captured
                    {
                        let mut lfs = std::mem::take(&mut func.local_functions);
                        func.params.for_each_init_mut(&mut |init| {
                            sub_changed |= subst_const_in_func_expr(init, &mut lfs, child_v, c);
                        });
                        sub_changed |=
                            subst_const_in_func_expr(&mut func.body, &mut lfs, child_v, c);
                        func.local_functions = lfs;
                    }
                }
                return sub_changed;
            }
            return false;
        }
        CoreKind::ReadVar(v) => {
            if let Some(c) = env.var_const.get(v) {
                expr.kind = CoreKind::Const(c.clone());
                return true;
            }
            let copy_source = env.resolve_copy(*v);
            if copy_source != *v {
                *v = copy_source;
                return true;
            }
            return false;
        }
        CoreKind::WriteVar { value, .. }
        | CoreKind::WriteGlobal { value, .. }
        | CoreKind::ReturnFrom { value, .. } => {
            changed |= fold_expr(value, env, locals);
        }
        CoreKind::The {
            type_spec,
            expr: inner,
            safety,
        } => {
            changed |= fold_expr(inner, env, locals);
            if is_the_proven(type_spec, inner, env) {
                *expr = std::mem::replace(&mut **inner, CoreExpr::nil(0));
                return true;
            }
            if *safety == 0
                && let CoreKind::The {
                    type_spec: inner_spec,
                    expr: inner2,
                    safety: inner_safety,
                } = &mut inner.kind
                && *inner_safety == 0
                && parse_the_type(type_spec).primary_type()
                    == parse_the_type(inner_spec).primary_type()
            {
                let taken = std::mem::replace(&mut **inner2, CoreExpr::nil(0));
                **inner = taken;
                return true;
            }
        }
        CoreKind::Block { id, body } => {
            changed |= fold_expr(body, env, locals);
            if !env.blocks[id.index()].used && !has_raw_special(body, locals) {
                *expr = std::mem::replace(&mut **body, CoreExpr::nil(0));
                return true;
            }
        }
        CoreKind::Progn(exprs) => {
            let mut undo = env.begin_scope();
            let mut flat = Vec::with_capacity(exprs.len());
            for mut e in std::mem::take(exprs) {
                changed |= fold_expr(&mut e, env, locals);
                if let CoreKind::The {
                    type_spec,
                    expr: inner,
                    ..
                } = &e.kind
                    && let CoreKind::ReadVar(v) = inner.kind
                    && !env.vars[v.index()].special
                    && !env.vars[v.index()].assigned
                {
                    let ty = parse_the_type(type_spec).primary_type();
                    if let Some(range) = env.fixnums.spec(type_spec) {
                        env.intersect_integer_range(&mut undo, v, range);
                    } else if ty != CoreType::Top {
                        env.narrow_type(&mut undo, v, ty);
                        if ty == CoreType::Null {
                            env.set_const(&mut undo, v, CoreConst::Nil);
                        }
                    }
                }
                match e.kind {
                    CoreKind::Progn(inner) => {
                        changed = true;
                        flat.extend(inner);
                    }
                    _ => flat.push(e),
                }
            }
            env.end_scope(undo);
            if flat.len() > 1 {
                let last = flat.len() - 1;
                let mut kept = Vec::with_capacity(flat.len());
                for (i, e) in flat.into_iter().enumerate() {
                    if i < last && is_pure_expr(&e, env.vars) {
                        changed = true;
                    } else {
                        kept.push(e);
                    }
                }
                flat = kept;
            }
            match flat.len() {
                0 => {
                    expr.kind = CoreKind::Const(CoreConst::Nil);
                    return true;
                }
                1 => {
                    *expr = flat.pop().unwrap();
                    return true;
                }
                _ => *exprs = flat,
            }
        }
        CoreKind::If { test, then_, else_ } => {
            changed |= fold_expr(test, env, locals);
            let test_ty = infer_expr_type(test, env);
            let selected = if test_ty == CoreType::Null {
                Some(&mut **else_)
            } else if test_ty.is_definitely_truthy() {
                Some(&mut **then_)
            } else {
                None
            };
            if let Some(branch) = selected {
                fold_expr(branch, env, locals);
                let result = std::mem::replace(branch, CoreExpr::nil(0));
                if is_pure_expr(test, env.vars) {
                    *expr = result;
                } else {
                    let taken_test = std::mem::replace(&mut **test, CoreExpr::nil(0));
                    expr.kind = CoreKind::Progn(vec![taken_test, result]);
                }
                return true;
            }
            changed |= fold_if_branches(test, then_, else_, env, locals);
        }
        CoreKind::Let {
            sequential,
            bindings,
            body,
        } => {
            let mut undo = env.begin_scope();
            let record_binding = |target: &CoreBindingTarget,
                                  rhs: &CoreExpr,
                                  env: &mut PropagationEnv<'_>,
                                  undo: &mut PropagationUndo| {
                if let CoreBindingTarget::Lexical(v) = target {
                    let info = &env.vars[v.index()];
                    if !info.special && !info.assigned {
                        if let Some(range) = infer_range(rhs, env) {
                            env.intersect_integer_range(undo, *v, range);
                        } else {
                            let rhs_ty = infer_expr_type(rhs, env);
                            if rhs_ty != CoreType::Top {
                                env.narrow_type(undo, *v, rhs_ty);
                            }
                        }
                        if let CoreKind::Const(c) = &rhs.kind
                            && is_duplicable_const(c)
                        {
                            env.set_const(undo, *v, c.clone());
                        } else if let CoreKind::ReadVar(src) = &rhs.kind {
                            let sinfo = &env.vars[src.index()];
                            if !sinfo.special && !sinfo.assigned {
                                let root = env.resolve_copy(*src);
                                env.set_copy(undo, *v, root);
                            }
                        }
                    }
                }
            };
            if *sequential {
                for (target, rhs) in bindings.iter_mut() {
                    changed |= fold_expr(rhs, env, locals);
                    record_binding(target, rhs, env, &mut undo);
                }
            } else {
                for (_, rhs) in bindings.iter_mut() {
                    changed |= fold_expr(rhs, env, locals);
                }
                for (target, rhs) in bindings.iter() {
                    record_binding(target, rhs, env, &mut undo);
                }
            }
            changed |= fold_expr(body, env, locals);
            env.end_scope(undo);

            let before = bindings.len();
            bindings.retain(|(target, rhs)| match target {
                CoreBindingTarget::Lexical(v) => {
                    let info = &env.vars[v.index()];
                    !(!info.special
                        && info.read_count == 0
                        && !info.assigned
                        && !info.captured
                        && is_pure_expr(rhs, env.vars))
                }
                CoreBindingTarget::Special(_) => true,
            });
            if bindings.len() != before {
                changed = true;
            }
            if bindings.is_empty() {
                *expr = std::mem::replace(&mut **body, CoreExpr::nil(0));
                return true;
            }
        }
        CoreKind::Flet { fns, body, .. } => {
            changed |= fold_expr(body, env, locals);
            let before = fns.len();
            fns.retain(|id| {
                let info = &locals[id.index()].binding;
                info.call_count > 0 || info.escapes
            });
            if fns.len() != before {
                changed = true;
            }
            if fns.is_empty() {
                *expr = std::mem::replace(&mut **body, CoreExpr::nil(0));
                return true;
            }
        }
        CoreKind::Tagbody { segments, .. } => {
            for (_, es) in segments {
                for e in es {
                    changed |= fold_expr(e, env, locals);
                }
            }
        }
        CoreKind::Call {
            callee,
            args,
            inline_declaration,
            fixnum_arithmetic,
            ..
        } => {
            for arg in args.iter_mut() {
                changed |= fold_expr(arg, env, locals);
            }
            if let CoreCallee::Expr(ce) = callee {
                changed |= fold_expr(ce, env, locals);
                if let CoreKind::ReadLocalFn(id) = ce.kind {
                    *callee = CoreCallee::Local(id);
                    changed = true;
                }
            }
            if *inline_declaration != Some(InlineDeclaration::NotInline)
                && let CoreCallee::Named(name) = callee
            {
                if let Some(const_args) = args
                    .iter()
                    .map(|a| match &a.kind {
                        CoreKind::Const(c) => Some(c),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()
                    && let Some(folded) = fold_builtin_call(name, &const_args, env.fixnums)
                {
                    expr.kind = CoreKind::Const(folded);
                    return true;
                }
                if let Some(folded) = fold_typed_call(name, args, expr.at, env) {
                    *expr = folded;
                    return true;
                }
                *fixnum_arithmetic = arithmetic_range(name.operator(), args, env).map(|(op, _)| op);
            }
        }
        CoreKind::CallValues { callee, forms } => {
            changed |= fold_expr(callee, env, locals);
            for form in forms {
                changed |= fold_expr(form, env, locals);
            }
        }
    }
    changed
}

fn const_truthiness(c: &CoreConst) -> Option<bool> {
    let ty = const_type(c);
    if ty.is_definitely_nil() {
        Some(false)
    } else if ty.is_definitely_truthy() {
        Some(true)
    } else {
        None
    }
}

fn const_is_nil(c: &CoreConst) -> Option<bool> {
    const_truthiness(c).map(|t| !t)
}

fn const_is_t(c: &CoreConst) -> bool {
    matches!(c, CoreConst::True) || matches!(c, CoreConst::Symbol(n) if n.operator() == "T")
}

fn eval_type_pred_for_type(op: &str, infer: impl FnOnce() -> CoreType) -> Option<bool> {
    TypeTest::unary(op)?.evaluate(infer())
}

#[test]
fn arithmetic_calls_skip_predicate_type_inference() {
    assert_eq!(
        eval_type_pred_for_type("1+", || panic!("unexpected type query")),
        None
    );
    assert_eq!(
        eval_type_pred_for_type("NUMBERP", || CoreType::Fixnum),
        Some(true)
    );
}

fn fold_typed_call(
    name: &Name,
    args: &[CoreExpr],
    at: usize,
    env: &PropagationEnv<'_>,
) -> Option<CoreExpr> {
    let op = name.operator();
    if op.is_empty() {
        return None;
    }
    if let ("IDENTITY", [arg]) = (op, args)
        && is_single_value_expr(arg)
    {
        return Some(arg.clone());
    }
    let known = if let Some((arg, test)) = TypeTest::call(op, args, env.fixnums) {
        if matches!(arg.kind, CoreKind::ReadVar(var)
            if env.excludes_type(var, test.ty))
        {
            Some(test.negated)
        } else {
            let from_range = test.range.and_then(|required| {
                let actual = infer_range(arg, env)?;
                if required.contains(actual) {
                    Some(true)
                } else if required.disjoint(actual) {
                    Some(false)
                } else {
                    None
                }
            });
            from_range.or_else(|| test.evaluate(infer_expr_type(arg, env)))
        }
    } else if let [lhs, rhs] = args {
        match op {
            "<" | "<=" | ">" | ">=" => {
                infer_range(lhs, env)?.comparison(op, infer_range(rhs, env)?)
            }
            "EQ" | "EQL" => {
                let left = infer_expr_type(lhs, env);
                let right = infer_expr_type(rhs, env);
                if left.is_disjoint(right) {
                    Some(false)
                } else if left == right && matches!(left, CoreType::True | CoreType::Null) {
                    Some(true)
                } else {
                    None
                }
            }
            _ => None,
        }
    } else {
        None
    }?;
    let result = CoreExpr::new(
        at,
        CoreKind::Const(if known {
            CoreConst::True
        } else {
            CoreConst::Nil
        }),
    );
    let mut effects: Vec<_> = args
        .iter()
        .filter(|arg| !is_pure_expr(arg, env.vars))
        .cloned()
        .collect();
    if effects.is_empty() {
        Some(result)
    } else {
        effects.push(result);
        Some(CoreExpr::new(at, CoreKind::Progn(effects)))
    }
}

fn fold_builtin_call(name: &Name, args: &[&CoreConst], fixnums: IntegerRange) -> Option<CoreConst> {
    let (min, max) = fixnums.bounds();
    // A folded immediate may also execute in the compiling VM during macroexpansion.
    let fixnum_in_range = |n: i64| {
        ((min..=max).contains(&n)
            && (Word::FIXNUM_MIN as i64..=Word::FIXNUM_MAX as i64).contains(&n))
        .then_some(n)
    };
    let op = name.operator();
    if op.is_empty() {
        return None;
    }
    match op {
        "+" | "*" => {
            let mut acc: i64 = if op == "+" { 0 } else { 1 };
            for arg in args {
                let CoreConst::Fixnum(v) = arg else {
                    return None;
                };
                acc = if op == "+" {
                    acc.checked_add(*v)?
                } else {
                    acc.checked_mul(*v)?
                };
            }
            fixnum_in_range(acc).map(CoreConst::Fixnum)
        }
        "-" => {
            let (&first, rest) = args.split_first()?;
            let &CoreConst::Fixnum(mut acc) = first else {
                return None;
            };
            if rest.is_empty() {
                acc = acc.checked_neg()?;
            } else {
                for arg in rest {
                    let CoreConst::Fixnum(v) = arg else {
                        return None;
                    };
                    acc = acc.checked_sub(*v)?;
                }
            }
            fixnum_in_range(acc).map(CoreConst::Fixnum)
        }
        "1+" | "1-" => {
            let [CoreConst::Fixnum(v)] = args else {
                return None;
            };
            let res = if op == "1+" {
                v.checked_add(1)?
            } else {
                v.checked_sub(1)?
            };
            fixnum_in_range(res).map(CoreConst::Fixnum)
        }
        "ABS" => {
            let [CoreConst::Fixnum(v)] = args else {
                return None;
            };
            fixnum_in_range(v.checked_abs()?).map(CoreConst::Fixnum)
        }
        "SIGNUM" => {
            let [CoreConst::Fixnum(v)] = args else {
                return None;
            };
            Some(CoreConst::Fixnum(v.signum()))
        }
        "MIN" | "MAX" => {
            let (&first, rest) = args.split_first()?;
            let &CoreConst::Fixnum(mut acc) = first else {
                return None;
            };
            for arg in rest {
                let CoreConst::Fixnum(v) = arg else {
                    return None;
                };
                acc = if op == "MIN" {
                    acc.min(*v)
                } else {
                    acc.max(*v)
                };
            }
            fixnum_in_range(acc).map(CoreConst::Fixnum)
        }
        "ASH" => {
            let [CoreConst::Fixnum(v), CoreConst::Fixnum(shift)] = args else {
                return None;
            };
            let res = if *shift >= 0 {
                let s = u32::try_from(*shift).ok()?;
                if s >= 63 {
                    if *v == 0 { 0 } else { return None }
                } else {
                    v.checked_mul(1_i64 << s)?
                }
            } else {
                let s = (-*shift).min(63) as u32;
                v >> s
            };
            fixnum_in_range(res).map(CoreConst::Fixnum)
        }
        "MOD" | "REM" => {
            let [CoreConst::Fixnum(a), CoreConst::Fixnum(b)] = args else {
                return None;
            };
            if *b == 0 {
                return None;
            }
            let rem = a.checked_rem(*b)?;
            let res = if op == "MOD" && rem != 0 && ((rem < 0) != (*b < 0)) {
                rem.checked_add(*b)?
            } else {
                rem
            };
            fixnum_in_range(res).map(CoreConst::Fixnum)
        }
        "=" | "<" | "<=" | ">" | ">=" => {
            if args.is_empty() {
                return None;
            }
            let nums: Vec<i64> = args
                .iter()
                .map(|a| match a {
                    CoreConst::Fixnum(v) => Some(*v),
                    _ => None,
                })
                .collect::<Option<_>>()?;
            let ok = nums.windows(2).all(|w| match op {
                "=" => w[0] == w[1],
                "<" => w[0] < w[1],
                "<=" => w[0] <= w[1],
                ">" => w[0] > w[1],
                ">=" => w[0] >= w[1],
                _ => unreachable!(),
            });
            Some(if ok { CoreConst::True } else { CoreConst::Nil })
        }
        "/=" => {
            if args.is_empty() {
                return None;
            }
            let nums: Vec<i64> = args
                .iter()
                .map(|a| match a {
                    CoreConst::Fixnum(v) => Some(*v),
                    _ => None,
                })
                .collect::<Option<_>>()?;
            let mut ok = true;
            for i in 0..nums.len() {
                for j in (i + 1)..nums.len() {
                    if nums[i] == nums[j] {
                        ok = false;
                        break;
                    }
                }
                if !ok {
                    break;
                }
            }
            Some(if ok { CoreConst::True } else { CoreConst::Nil })
        }
        "ZEROP" | "PLUSP" | "MINUSP" | "EVENP" | "ODDP" => {
            let [CoreConst::Fixnum(v)] = args else {
                return None;
            };
            let ok = match op {
                "ZEROP" => *v == 0,
                "PLUSP" => *v > 0,
                "MINUSP" => *v < 0,
                "EVENP" => v % 2 == 0,
                "ODDP" => v % 2 != 0,
                _ => unreachable!(),
            };
            Some(if ok { CoreConst::True } else { CoreConst::Nil })
        }
        "NULL" | "NOT" => {
            let [arg] = args else { return None };
            const_is_nil(arg).map(|b| if b { CoreConst::True } else { CoreConst::Nil })
        }
        "IDENTITY" => {
            let [arg] = args else { return None };
            Some((*arg).clone())
        }
        _ if args.len() == 1 => eval_type_pred_for_type(op, || const_type(args[0]))
            .map(|b| if b { CoreConst::True } else { CoreConst::Nil }),
        "EQ" | "EQL" => {
            let [a, b] = args else { return None };
            let res = match (a, b) {
                (CoreConst::LiteralForm(_), _) | (_, CoreConst::LiteralForm(_)) => return None,
                (CoreConst::String(_), CoreConst::String(_)) => return None,
                (x, y) if const_is_nil(x) == Some(true) && const_is_nil(y) == Some(true) => true,
                (x, y) if const_is_t(x) && const_is_t(y) => true,
                (CoreConst::Fixnum(x), CoreConst::Fixnum(y)) => x == y,
                (CoreConst::Char(x), CoreConst::Char(y)) => x == y,
                (CoreConst::Symbol(x), CoreConst::Symbol(y)) => x == y,
                _ => false,
            };
            Some(if res { CoreConst::True } else { CoreConst::Nil })
        }
        "EQUAL" => {
            let [a, b] = args else { return None };
            let res = match (a, b) {
                (CoreConst::LiteralForm(_), _) | (_, CoreConst::LiteralForm(_)) => return None,
                (CoreConst::String(x), CoreConst::String(y)) => x == y,
                (x, y) if const_is_nil(x) == Some(true) && const_is_nil(y) == Some(true) => true,
                (x, y) if const_is_t(x) && const_is_t(y) => true,
                (CoreConst::Fixnum(x), CoreConst::Fixnum(y)) => x == y,
                (CoreConst::Char(x), CoreConst::Char(y)) => x == y,
                (CoreConst::Symbol(x), CoreConst::Symbol(y)) => x == y,
                _ => false,
            };
            Some(if res { CoreConst::True } else { CoreConst::Nil })
        }
        _ => None,
    }
}
