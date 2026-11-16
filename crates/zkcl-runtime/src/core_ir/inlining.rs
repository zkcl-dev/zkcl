//! Argument binding and alpha-renaming for local and named inlining.

use std::ops::ControlFlow;
use std::rc::Rc;

use rustc_hash::FxHashMap;

use super::{
    BlockId, CoreBindingTarget, CoreCallee, CoreExpr, CoreFn, CoreKind, CoreParameterBindings,
    InlineDeclaration, LocalFunction, LocalFunctionBinding, LocalFunctionId, TagbodyId, VarId,
    any_fn_node, has_raw_special, optimize_core_fn_inner, restore_map,
};
use crate::symbol::{Name, NameKey};

const MAX_INLINE_DEPTH: usize = 8;

/// Stop counting as soon as the inlining size limit is exceeded.
fn fits_inline_size_limit(expr: &CoreExpr, limit: usize) -> bool {
    fn count(expr: &CoreExpr, remaining: &mut usize) -> ControlFlow<()> {
        expr.walk_in_function(None, &mut |node| {
            let size = 1 + match &node.kind {
                CoreKind::Let { bindings, .. } => bindings.len(),
                CoreKind::Flet { fns, .. } => fns.len(),
                _ => 0,
            };
            let Some(left) = remaining.checked_sub(size) else {
                return ControlFlow::Break(());
            };
            *remaining = left;
            if let CoreKind::Lambda { func } = &node.kind {
                for expr in func.params.initializers_and_body(&func.body) {
                    count(expr, remaining)?;
                }
            }
            ControlFlow::Continue(())
        })
    }
    let mut remaining = limit;
    count(expr, &mut remaining).is_continue()
}

pub fn can_store_inline_candidate(fn_ir: &CoreFn, size_limit: usize) -> bool {
    fits_inline_size_limit(&fn_ir.body, size_limit)
        && !any_fn_node(fn_ir, &|expr| {
            matches!(
                expr.kind,
                CoreKind::RawSpecial(..) | CoreKind::LoadTimeValue { .. }
            )
        })
}

struct InliningContext<'a> {
    lookup: &'a dyn Fn(&Name) -> Option<Rc<CoreFn>>,
    local_stack: Vec<LocalFunctionId>,
    named_stack: Vec<NameKey>,
    depth: usize,
}

pub(super) fn inline_pass(
    fn_ir: &mut CoreFn,
    inline_lookup: &dyn Fn(&Name) -> Option<Rc<CoreFn>>,
) -> bool {
    let mut ctx = InliningContext {
        lookup: inline_lookup,
        local_stack: Vec::new(),
        named_stack: Vec::new(),
        depth: 0,
    };
    let mut changed = false;

    for idx in 0..fn_ir.local_functions.len() {
        let binding = &fn_ir.local_functions[idx].binding;
        // Stable IDs retain definitions after their FLET or all calls disappear.
        // Analysis also accounts for local names used by delayed raw expansion.
        if binding.call_count == 0 && !binding.escapes {
            continue;
        }
        let (mut lf_params, mut lf_body) = fn_ir.local_functions[idx].take_definition();
        ctx.local_stack.push(LocalFunctionId(idx as u32));
        lf_params.for_each_init_mut(&mut |init| {
            changed |= inline_expr(init, fn_ir, &mut ctx);
        });
        changed |= inline_expr(&mut lf_body, fn_ir, &mut ctx);
        ctx.local_stack.pop();
        fn_ir.local_functions[idx].params = lf_params;
        fn_ir.local_functions[idx].body = lf_body;
    }

    let mut params = std::mem::replace(&mut fn_ir.params, CoreParameterBindings::empty());
    let mut body = std::mem::replace(&mut fn_ir.body, Box::new(CoreExpr::nil(0)));
    params.for_each_init_mut(&mut |init| {
        changed |= inline_expr(init, fn_ir, &mut ctx);
    });
    changed |= inline_expr(&mut body, fn_ir, &mut ctx);
    fn_ir.params = params;
    fn_ir.body = body;
    changed
}

fn inline_expr(expr: &mut CoreExpr, fn_ir: &mut CoreFn, ctx: &mut InliningContext<'_>) -> bool {
    match &mut expr.kind {
        CoreKind::Lambda { func } => optimize_core_fn_inner(func, ctx.lookup),
        CoreKind::Call { .. } => try_inline_call(expr, fn_ir, ctx),
        _ => {
            let mut changed = false;
            expr.for_each_child_mut(|child| changed |= inline_expr(child, fn_ir, ctx));
            changed
        }
    }
}

fn try_inline_call(expr: &mut CoreExpr, fn_ir: &mut CoreFn, ctx: &mut InliningContext<'_>) -> bool {
    let call_at = expr.at;
    let CoreKind::Call {
        callee,
        args,
        qualities,
        inline_declaration,
        ..
    } = &mut expr.kind
    else {
        return false;
    };

    let mut changed = false;
    for arg in args.iter_mut() {
        changed |= inline_expr(arg, fn_ir, ctx);
    }
    if *inline_declaration == Some(InlineDeclaration::NotInline) || ctx.depth >= MAX_INLINE_DEPTH {
        if let CoreCallee::Expr(e) = callee {
            changed |= inline_expr(e, fn_ir, ctx);
        }
        return changed;
    }

    match callee {
        CoreCallee::Local(id) => {
            let explicitly_inline = *inline_declaration == Some(InlineDeclaration::Inline);
            if !explicitly_inline && !qualities.allows_automatic_inlining() {
                return changed;
            }
            let id = *id;
            let lf = &fn_ir.local_functions[id.index()];
            let size_limit = if lf.binding.dynamic_extent {
                qualities.local_inline_size_limit().saturating_mul(2)
            } else {
                qualities.local_inline_size_limit()
            };
            if !ctx.local_stack.contains(&id)
                && !lf.binding.recursive
                && let Some(params) = lf.params.inlineable_fixed_params()
                && params.len() == args.len()
                && (explicitly_inline
                    || (lf.binding.call_count == 1 && !lf.binding.escapes)
                    || fits_inline_size_limit(&lf.body, size_limit))
                && !has_raw_special(&lf.body, &fn_ir.local_functions)
            {
                let params = params.to_vec();
                let lf_body = (*lf.body).clone();
                let taken_args = std::mem::take(args);
                *expr = instantiate_local_fn(fn_ir, &params, lf_body, taken_args, call_at);
                ctx.local_stack.push(id);
                ctx.depth += 1;
                inline_expr(expr, fn_ir, ctx);
                ctx.depth -= 1;
                ctx.local_stack.pop();
                return true;
            }
        }
        CoreCallee::Expr(callee_expr) => {
            if qualities.allows_automatic_inlining()
                && let CoreKind::Lambda { func } = &mut callee_expr.kind
                && !func.has_external_captures()
                && let Some(params) = func.params.inlineable_fixed_params()
                && params.len() == args.len()
                && !has_raw_special(&func.body, &func.local_functions)
            {
                let taken_args = std::mem::take(args);
                let inlined = instantiate_core_fn(fn_ir, func, params, taken_args, call_at);
                *expr = inlined;
                ctx.depth += 1;
                inline_expr(expr, fn_ir, ctx);
                ctx.depth -= 1;
                return true;
            }
            changed |= inline_expr(callee_expr, fn_ir, ctx);
        }
        CoreCallee::Named(name) => {
            if *inline_declaration == Some(InlineDeclaration::Inline)
                && !ctx.named_stack.contains(name.key())
                && let Some(callee_fn) = (ctx.lookup)(name)
                && !callee_fn.has_captures()
                && let Some(params) = callee_fn.params.inlineable_fixed_params()
                && params.len() == args.len()
                && !has_raw_special(&callee_fn.body, &callee_fn.local_functions)
            {
                let key = name.key().clone();
                let taken_args = std::mem::take(args);
                *expr = instantiate_core_fn(fn_ir, &callee_fn, params, taken_args, call_at);
                ctx.named_stack.push(key);
                ctx.depth += 1;
                inline_expr(expr, fn_ir, ctx);
                ctx.depth -= 1;
                ctx.named_stack.pop();
                return true;
            }
        }
    }
    changed
}

#[derive(Default)]
struct InliningRenames {
    vars: FxHashMap<VarId, VarId>,
    blocks: FxHashMap<BlockId, BlockId>,
    tagbodies: FxHashMap<TagbodyId, TagbodyId>,
    local_functions: FxHashMap<LocalFunctionId, LocalFunctionId>,
}

impl InliningRenames {
    fn map_var(&self, v: VarId) -> VarId {
        self.vars.get(&v).copied().unwrap_or(v)
    }
    fn map_block(&self, b: BlockId) -> BlockId {
        self.blocks.get(&b).copied().unwrap_or(b)
    }
    fn map_tagbody(&self, tb: TagbodyId) -> TagbodyId {
        self.tagbodies.get(&tb).copied().unwrap_or(tb)
    }
    fn map_local_fn(&self, lf: LocalFunctionId) -> LocalFunctionId {
        self.local_functions.get(&lf).copied().unwrap_or(lf)
    }
}

fn bind_inline_args(params: &[VarId], args: Vec<CoreExpr>, body: CoreExpr, at: usize) -> CoreExpr {
    if params.is_empty() {
        return body;
    }
    let bindings = params
        .iter()
        .copied()
        .zip(args)
        .map(|(v, a)| (CoreBindingTarget::Lexical(v), a))
        .collect();
    CoreExpr::new(
        at,
        CoreKind::Let {
            sequential: true,
            bindings,
            body: Box::new(body),
        },
    )
}

fn instantiate_local_fn(
    caller: &mut CoreFn,
    params: &[VarId],
    mut lf_body: CoreExpr,
    args: Vec<CoreExpr>,
    call_at: usize,
) -> CoreExpr {
    let mut map = InliningRenames::default();
    let mut new_params = Vec::with_capacity(params.len());
    for &p in params {
        let fresh = caller.alloc_var(caller.vars[p.index()].fresh_binding());
        map.vars.insert(p, fresh);
        new_params.push(fresh);
    }
    rename_expr(&mut lf_body, caller, &mut map, true);
    bind_inline_args(&new_params, args, lf_body, call_at)
}

fn instantiate_core_fn(
    caller: &mut CoreFn,
    callee: &CoreFn,
    params: &[VarId],
    args: Vec<CoreExpr>,
    call_at: usize,
) -> CoreExpr {
    let mut map = InliningRenames::default();
    for &(child, parent) in &callee.parent_var_captures {
        map.vars.insert(child, parent);
    }
    for &(child, parent) in &callee.parent_block_captures {
        map.blocks.insert(child, parent);
    }
    for &(child, parent) in &callee.parent_tagbody_captures {
        map.tagbodies.insert(child, parent);
    }
    for &(child, parent) in &callee.parent_function_captures {
        map.local_functions.insert(child, parent);
    }

    for (idx, info) in callee.vars.iter().enumerate() {
        let old = VarId(idx as u32);
        map.vars
            .entry(old)
            .or_insert_with(|| caller.alloc_var(info.fresh_binding()));
    }
    for (idx, info) in callee.blocks.iter().enumerate() {
        let old = BlockId(idx as u32);
        map.blocks
            .entry(old)
            .or_insert_with(|| caller.alloc_block(info.fresh_binding()));
    }
    for (idx, info) in callee.tagbodies.iter().enumerate() {
        let old = TagbodyId(idx as u32);
        map.tagbodies
            .entry(old)
            .or_insert_with(|| caller.alloc_tagbody(info.fresh_binding()));
    }
    let mut new_lfs = Vec::new();
    for (idx, orig) in callee.local_functions.iter().enumerate() {
        let old = LocalFunctionId(idx as u32);
        if let std::collections::hash_map::Entry::Vacant(entry) = map.local_functions.entry(old) {
            let fresh = alloc_cloned_local_fn(caller, orig);
            entry.insert(fresh);
            new_lfs.push((idx, fresh));
        }
    }
    for (old_idx, fresh_id) in new_lfs {
        let orig = &callee.local_functions[old_idx];
        let mut params = orig.params.clone();
        let mut body = orig.body.clone();
        rename_params(&mut params, caller, &mut map, None);
        caller.local_functions[fresh_id.index()].params = params;
        rename_expr(&mut body, caller, &mut map, false);
        caller.local_functions[fresh_id.index()].body = body;
    }

    let params: Vec<_> = params.iter().map(|&var| map.map_var(var)).collect();
    let mut body = (*callee.body).clone();
    rename_expr(&mut body, caller, &mut map, false);
    bind_inline_args(&params, args, body, call_at)
}

fn alloc_cloned_local_fn(caller: &mut CoreFn, orig: &LocalFunction) -> LocalFunctionId {
    caller.alloc_local_fn(LocalFunction {
        binding: LocalFunctionBinding {
            name: orig.binding.name.clone(),
            setf: orig.binding.setf,
            call_count: 0,
            escapes: false,
            recursive: orig.binding.recursive,
            has_notinline_call: false,
            dynamic_extent: orig.binding.dynamic_extent,
        },
        params: orig.params.clone(),
        source_lambda_list: orig.source_lambda_list.clone(),
        source_expression: orig.source_expression.clone(),
        body: Box::new(CoreExpr::nil(orig.at)),
        at: orig.at,
    })
}

/// Rename an owned copy of an inlinee. Whole-function inlining has already
/// allocated its binding tables. A local inlinee shares the caller's tables
/// and needs fresh IDs only for the bindings introduced inside its definition.
fn rename_expr(
    expr: &mut CoreExpr,
    caller: &mut CoreFn,
    map: &mut InliningRenames,
    alloc_locals: bool,
) {
    match &mut expr.kind {
        CoreKind::ReadVar(var) | CoreKind::WriteVar { var, .. } => *var = map.map_var(*var),
        CoreKind::ReadLocalFn(id)
        | CoreKind::Call {
            callee: CoreCallee::Local(id),
            ..
        } => {
            *id = map.map_local_fn(*id);
        }
        CoreKind::ReturnFrom { id, .. } => *id = map.map_block(*id),
        CoreKind::Go { id, .. } => *id = map.map_tagbody(*id),
        CoreKind::Lambda { func } => {
            for (_, parent) in &mut func.parent_var_captures {
                *parent = map.map_var(*parent);
            }
            for (_, parent) in &mut func.parent_block_captures {
                *parent = map.map_block(*parent);
            }
            for (_, parent) in &mut func.parent_tagbody_captures {
                *parent = map.map_tagbody(*parent);
            }
            for (_, parent) in &mut func.parent_function_captures {
                *parent = map.map_local_fn(*parent);
            }
            return;
        }
        CoreKind::Let {
            sequential,
            bindings,
            body,
        } => {
            let mut saved = Vec::new();
            let mut staged = Vec::new();
            for (target, rhs) in bindings {
                rename_expr(rhs, caller, map, alloc_locals);
                if let CoreBindingTarget::Lexical(var) = target {
                    if alloc_locals {
                        let old = *var;
                        *var = caller.alloc_var(caller.vars[old.index()].fresh_binding());
                        if *sequential {
                            saved.push((old, map.vars.insert(old, *var)));
                        } else {
                            staged.push((old, *var));
                        }
                    } else {
                        *var = map.map_var(*var);
                    }
                }
            }
            for (old, fresh) in staged {
                saved.push((old, map.vars.insert(old, fresh)));
            }
            rename_expr(body, caller, map, alloc_locals);
            for (old, prev) in saved.into_iter().rev() {
                restore_map(&mut map.vars, old, prev);
            }
            return;
        }
        CoreKind::Flet {
            recursive,
            fns,
            body,
        } if alloc_locals => {
            rename_local_definitions(*recursive, fns, body, caller, map);
            return;
        }
        CoreKind::Flet { fns, .. } => {
            for id in fns {
                *id = map.map_local_fn(*id);
            }
        }
        CoreKind::Block { id, body } => {
            let old = *id;
            let prev = if alloc_locals {
                *id = caller.alloc_block(caller.blocks[old.index()].fresh_binding());
                Some(map.blocks.insert(old, *id))
            } else {
                *id = map.map_block(old);
                None
            };
            rename_expr(body, caller, map, alloc_locals);
            if let Some(prev) = prev {
                restore_map(&mut map.blocks, old, prev);
            }
            return;
        }
        CoreKind::Tagbody { id, segments } => {
            let old = *id;
            let prev = if alloc_locals {
                *id = caller.alloc_tagbody(caller.tagbodies[old.index()].fresh_binding());
                Some(map.tagbodies.insert(old, *id))
            } else {
                *id = map.map_tagbody(old);
                None
            };
            for (_, forms) in segments {
                for form in forms {
                    rename_expr(form, caller, map, alloc_locals);
                }
            }
            if let Some(prev) = prev {
                restore_map(&mut map.tagbodies, old, prev);
            }
            return;
        }
        CoreKind::Const(_)
        | CoreKind::ReadGlobal(_)
        | CoreKind::ReadGlobalFn { .. }
        | CoreKind::WriteGlobal { .. }
        | CoreKind::Progn(_)
        | CoreKind::If { .. }
        | CoreKind::Call { .. }
        | CoreKind::CallValues { .. }
        | CoreKind::LoadTimeValue { .. }
        | CoreKind::The { .. }
        | CoreKind::RawSpecial(..) => {}
    }
    expr.for_each_child_mut(|child| rename_expr(child, caller, map, alloc_locals));
}

fn rename_local_definitions(
    recursive: bool,
    fns: &mut [LocalFunctionId],
    body: &mut CoreExpr,
    caller: &mut CoreFn,
    map: &mut InliningRenames,
) {
    let mut definitions = Vec::with_capacity(fns.len());
    let mut ids = Vec::with_capacity(fns.len());
    for id in fns {
        let old = *id;
        let orig = caller.local_functions[old.index()].clone();
        *id = alloc_cloned_local_fn(caller, &orig);
        ids.push((old, *id));
        definitions.push((orig.params, orig.body));
    }
    let mut saved_fns = Vec::with_capacity(ids.len());
    let install_functions = |map: &mut InliningRenames, saved: &mut Vec<_>| {
        for &(old, fresh) in &ids {
            saved.push((old, map.local_functions.insert(old, fresh)));
        }
    };
    if recursive {
        install_functions(map, &mut saved_fns);
    }
    for ((_, fresh), (mut params, mut definition)) in ids.iter().zip(definitions) {
        let mut saved_vars = Vec::with_capacity(params.all_params.len());
        rename_params(&mut params, caller, map, Some(&mut saved_vars));
        caller.local_functions[fresh.index()].params = params;
        rename_expr(&mut definition, caller, map, true);
        for (old, prev) in saved_vars.into_iter().rev() {
            restore_map(&mut map.vars, old, prev);
        }
        caller.local_functions[fresh.index()].body = definition;
    }
    if !recursive {
        install_functions(map, &mut saved_fns);
    }
    rename_expr(body, caller, map, true);
    for (old, prev) in saved_fns.into_iter().rev() {
        restore_map(&mut map.local_functions, old, prev);
    }
}

fn rename_params(
    params: &mut CoreParameterBindings,
    caller: &mut CoreFn,
    map: &mut InliningRenames,
    mut saved: Option<&mut Vec<(VarId, Option<VarId>)>>,
) {
    params.for_each_binding_mut(|var, init| {
        // Defaults see earlier parameters before the new binding shadows them.
        if let Some(init) = init {
            rename_expr(init, caller, map, saved.is_some());
        }
        if let Some(saved) = &mut saved {
            let old = *var;
            *var = caller.alloc_var(caller.vars[old.index()].fresh_binding());
            saved.push((old, map.vars.insert(old, *var)));
        } else {
            *var = map.map_var(*var);
        }
    });
    for var in &mut params.all_params {
        *var = map.map_var(*var);
    }
}
