//! Higher-order free-variable sets and Nesting Tree analysis for [`CoreFn`]
//! (Leißa & Griebler, *SSA without Dominance for Higher-Order Programs*, PLDI 2026).
//!
//! Macro expansion in Common Lisp often produces deep syntactic `let` / `flet`
//! / `labels` nesting even when an inner local function only captures a
//! binding from a much outer scope (or captures nothing at all).
//!
//! [`CoreNestingTree::nesting_parent`]: the deepest enclosing local function that
//! defines any of its transitive free dependencies (or `None` when hoistable to
//! the enclosing [`CoreFn`] root).

use rustc_hash::FxHashSet;

use super::{
    BlockId, CoreBindingTarget, CoreCallee, CoreExpr, CoreFn, CoreKind, CoreParameterBindings,
    LocalFunctionId, TagbodyId, VarId, fn_has_raw_special,
};

/// Free-variable sets and Nesting Tree hierarchy for all local functions in a [`CoreFn`].
#[derive(Clone, Debug)]
pub struct CoreNestingTree {
    /// Lexical variables referenced free by each `LocalFunctionId` (transitive).
    free_vars: Vec<FxHashSet<VarId>>,
    /// Lexical `BLOCK` targets referenced non-locally by each `LocalFunctionId` (transitive).
    free_blocks: Vec<FxHashSet<BlockId>>,
    /// Lexical `TAGBODY` targets referenced non-locally by each `LocalFunctionId` (transitive).
    free_tagbodies: Vec<FxHashSet<TagbodyId>>,
    /// Local or captured functions defined outside `lf` and referenced within `lf` (transitive).
    free_local_fns: Vec<FxHashSet<LocalFunctionId>>,
    /// Syntactic enclosing `LocalFunctionId` in source IR (for conservative fallback).
    syntactic_parent: Vec<Option<LocalFunctionId>>,
    /// Nesting depth of each `LocalFunctionId` from the root `CoreFn`.
    fn_depth: Vec<usize>,
    /// Tightest enclosing `LocalFunctionId` required by free dependencies, or `None` for root.
    nesting_parent: Vec<Option<LocalFunctionId>>,
    /// True if `lf` (or a callee inside `lf`) contains an unexpanded `RawSpecial`.
    has_raw_special: Vec<bool>,
    /// True if this slot represents a local `FLET`/`LABELS` definition (not a parent capture stub).
    is_locally_defined: Vec<bool>,
}

impl CoreNestingTree {
    /// Compute the free-variable sets and Nesting Tree for `fn_ir`.
    pub fn compute(fn_ir: &CoreFn) -> Self {
        let num_fns = fn_ir.local_functions.len();
        let mut builder = NestingBuilder {
            fn_ir,
            var_owner: vec![None; fn_ir.vars.len()],
            block_owner: vec![None; fn_ir.blocks.len()],
            tagbody_owner: vec![None; fn_ir.tagbodies.len()],
            local_fn_owner: vec![None; num_fns],
            syntactic_parent: vec![None; num_fns],
            fn_depth: vec![0; num_fns],
            is_locally_defined: vec![false; num_fns],
            bound_vars: vec![FxHashSet::default(); num_fns],
            direct_fn_edges: vec![Vec::new(); num_fns],
            free_vars: vec![FxHashSet::default(); num_fns],
            free_blocks: vec![FxHashSet::default(); num_fns],
            free_tagbodies: vec![FxHashSet::default(); num_fns],
            free_local_fns: vec![FxHashSet::default(); num_fns],
            has_raw_special: vec![false; num_fns],
        };

        builder.walk_params(&fn_ir.params, None);
        builder.walk_expr(&fn_ir.body, None);

        // Fixed-point propagation across call/reference edges (handles mutual
        // recursion in LABELS as well as inner functions referencing outer siblings).
        loop {
            let mut changed = false;
            for caller_idx in 0..num_fns {
                if !builder.is_locally_defined[caller_idx] {
                    continue;
                }
                let caller = LocalFunctionId(caller_idx as u32);
                let edges = builder.direct_fn_edges[caller_idx].clone();
                for callee in edges {
                    let callee_idx = callee.index();
                    if builder.has_raw_special[callee_idx] && !builder.has_raw_special[caller_idx] {
                        builder.has_raw_special[caller_idx] = true;
                        changed = true;
                    }

                    let callee_vars: Vec<VarId> = builder.free_vars[callee_idx].iter().copied().collect();
                    for v in callee_vars {
                        if !builder.bound_vars[caller_idx].contains(&v)
                            && builder.free_vars[caller_idx].insert(v)
                        {
                            changed = true;
                        }
                    }

                    let callee_blocks: Vec<BlockId> =
                        builder.free_blocks[callee_idx].iter().copied().collect();
                    for b in callee_blocks {
                        if builder.block_owner[b.index()] != Some(caller)
                            && builder.free_blocks[caller_idx].insert(b)
                        {
                            changed = true;
                        }
                    }

                    let callee_tbs: Vec<TagbodyId> =
                        builder.free_tagbodies[callee_idx].iter().copied().collect();
                    for tb in callee_tbs {
                        if builder.tagbody_owner[tb.index()] != Some(caller)
                            && builder.free_tagbodies[caller_idx].insert(tb)
                        {
                            changed = true;
                        }
                    }

                    let callee_fns: Vec<LocalFunctionId> =
                        builder.free_local_fns[callee_idx].iter().copied().collect();
                    for f2 in callee_fns {
                        if f2 != caller
                            && builder.local_fn_owner[f2.index()] != Some(caller)
                            && builder.free_local_fns[caller_idx].insert(f2)
                        {
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // Compute tightest `nesting_parent` (fixed point so a caller inheriting a
        // callee's `RawSpecial` or sibling local-fn constraint converges as well).
        let mut nesting_parent = vec![None; num_fns];
        for idx in 0..num_fns {
            if !builder.is_locally_defined[idx] {
                continue;
            }
            if builder.has_raw_special[idx] {
                nesting_parent[idx] = builder.syntactic_parent[idx];
                continue;
            }
            let mut best: Option<LocalFunctionId> = None;
            let mut consider = |candidate: Option<LocalFunctionId>| {
                if let Some(cand) = candidate
                    && best.is_none_or(|cur| builder.fn_depth[cand.index()] > builder.fn_depth[cur.index()])
                {
                    best = Some(cand);
                }
            };
            for &v in &builder.free_vars[idx] {
                consider(builder.var_owner[v.index()]);
            }
            for &b in &builder.free_blocks[idx] {
                consider(builder.block_owner[b.index()]);
            }
            for &tb in &builder.free_tagbodies[idx] {
                consider(builder.tagbody_owner[tb.index()]);
            }
            for &f2 in &builder.free_local_fns[idx] {
                // Calling/referencing an enclosing local function `f2` only pins `idx`
                // under an owner if `f2` itself lives at `Some(owner)` and cannot float
                // all the way to root without capturing something from `owner`.
                if !builder.is_locally_defined[f2.index()] {
                    consider(builder.local_fn_owner[f2.index()]);
                }
            }
            nesting_parent[idx] = best;
        }

        loop {
            let mut changed = false;
            for idx in 0..num_fns {
                if !builder.is_locally_defined[idx] || builder.has_raw_special[idx] {
                    continue;
                }
                for &f2 in &builder.free_local_fns[idx] {
                    if let Some(p) = nesting_parent[f2.index()] {
                        // Only constrain `idx` if `p` is an ancestor of `idx` (always true
                        // when `p` is in scope at `f2`'s definition inside `idx`'s ancestor chain,
                        // unless `p` == `LocalFunctionId(idx)` which is excluded by `free_local_fns`).
                        let better = nesting_parent[idx]
                            .is_none_or(|cur| builder.fn_depth[p.index()] > builder.fn_depth[cur.index()]);
                        if better {
                            nesting_parent[idx] = Some(p);
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }

        Self {
            free_vars: builder.free_vars,
            free_blocks: builder.free_blocks,
            free_tagbodies: builder.free_tagbodies,
            free_local_fns: builder.free_local_fns,
            syntactic_parent: builder.syntactic_parent,
            fn_depth: builder.fn_depth,
            nesting_parent,
            has_raw_special: builder.has_raw_special,
            is_locally_defined: builder.is_locally_defined,
        }
    }

    /// Tightest enclosing `LocalFunctionId` needed by `lf`, or `None` if hoistable
    /// directly to the enclosing [`CoreFn`] root.
    #[cfg_attr(not(test), allow(dead_code))]
    #[inline]
    pub fn nesting_parent(&self, lf: LocalFunctionId) -> Option<LocalFunctionId> {
        self.nesting_parent[lf.index()]
    }

    /// Syntactic enclosing `LocalFunctionId` in the source IR.
    #[inline]
    pub fn syntactic_parent(&self, lf: LocalFunctionId) -> Option<LocalFunctionId> {
        self.syntactic_parent[lf.index()]
    }

    /// Returns `true` when `lf` has no free dependencies bound inside any enclosing
    /// local function (and is a local definition).
    #[cfg_attr(not(test), allow(dead_code))]
    #[inline]
    pub fn is_hoistable_to_root(&self, lf: LocalFunctionId) -> bool {
        self.is_locally_defined[lf.index()] && self.nesting_parent[lf.index()].is_none()
    }

    /// Returns `true` when `lf` captures zero variables, blocks, tagbodies,
    /// external/free local functions, and contains no unexpanded `RawSpecial`.
    pub fn has_no_captures(&self, lf: LocalFunctionId) -> bool {
        let idx = lf.index();
        self.is_locally_defined[idx]
            && !self.has_raw_special[idx]
            && self.free_vars[idx].is_empty()
            && self.free_blocks[idx].is_empty()
            && self.free_tagbodies[idx].is_empty()
            && self.free_local_fns[idx].is_empty()
    }

    /// Transitive set of lexical `VarId`s appearing free inside `lf`.
    #[cfg_attr(not(test), allow(dead_code))]
    #[inline]
    pub fn minimal_free_vars(&self, lf: LocalFunctionId) -> &FxHashSet<VarId> {
        &self.free_vars[lf.index()]
    }
}

/// Hoist closed non-recursive `FLET` local functions nested inside other local
/// functions directly to the root of `fn_ir.body`.
pub(super) fn hoist_closed_local_functions(fn_ir: &mut CoreFn) -> bool {
    if fn_ir.local_functions.is_empty() || fn_has_raw_special(fn_ir) {
        return false;
    }

    let tree = CoreNestingTree::compute(fn_ir);
    let candidates: FxHashSet<LocalFunctionId> = (0..fn_ir.local_functions.len())
        .map(|idx| LocalFunctionId(idx as u32))
        .filter(|&id| tree.has_no_captures(id) && tree.syntactic_parent(id).is_some())
        .collect();
    if candidates.is_empty() {
        return false;
    }

    fn strip_in_expr(
        expr: &mut CoreExpr,
        candidates: &FxHashSet<LocalFunctionId>,
        tree: &CoreNestingTree,
        extracted: &mut Vec<(usize, LocalFunctionId)>,
    ) {
        expr.for_each_child_mut(|child| strip_in_expr(child, candidates, tree, extracted));
        if let CoreKind::Flet {
            recursive: false,
            fns,
            body,
        } = &mut expr.kind
        {
            let mut kept = Vec::with_capacity(fns.len());
            for &id in fns.iter() {
                if candidates.contains(&id) {
                    extracted.push((tree.fn_depth[id.index()], id));
                } else {
                    kept.push(id);
                }
            }
            if kept.len() != fns.len() {
                if kept.is_empty() {
                    let inner = std::mem::replace(&mut **body, CoreExpr::nil(expr.at));
                    *expr = inner;
                } else {
                    *fns = kept;
                }
            }
        }
    }

    let mut extracted = Vec::new();
    fn_ir
        .params
        .for_each_init_mut(&mut |init| strip_in_expr(init, &candidates, &tree, &mut extracted));
    strip_in_expr(&mut fn_ir.body, &candidates, &tree, &mut extracted);
    for local in &mut fn_ir.local_functions {
        local
            .params
            .for_each_init_mut(&mut |init| strip_in_expr(init, &candidates, &tree, &mut extracted));
        strip_in_expr(&mut local.body, &candidates, &tree, &mut extracted);
    }

    if extracted.is_empty() {
        return false;
    }

    // Wrap shallowest depth first so deeper nested helper functions end up
    // outermost in `fn_ir.body` and are already bound when shallower ones emit.
    extracted.sort_by_key(|&(depth, id)| (depth, id));
    let mut wrapped = std::mem::replace(&mut *fn_ir.body, CoreExpr::nil(fn_ir.at));
    let mut idx = 0;
    while idx < extracted.len() {
        let depth = extracted[idx].0;
        let mut group = Vec::new();
        while idx < extracted.len() && extracted[idx].0 == depth {
            group.push(extracted[idx].1);
            idx += 1;
        }
        let at = wrapped.at;
        wrapped = CoreExpr::new(
            at,
            CoreKind::Flet {
                recursive: false,
                fns: group,
                body: Box::new(wrapped),
            },
        );
    }
    *fn_ir.body = wrapped;
    true
}

struct NestingBuilder<'a> {
    fn_ir: &'a CoreFn,
    var_owner: Vec<Option<LocalFunctionId>>,
    block_owner: Vec<Option<LocalFunctionId>>,
    tagbody_owner: Vec<Option<LocalFunctionId>>,
    local_fn_owner: Vec<Option<LocalFunctionId>>,
    syntactic_parent: Vec<Option<LocalFunctionId>>,
    fn_depth: Vec<usize>,
    is_locally_defined: Vec<bool>,
    bound_vars: Vec<FxHashSet<VarId>>,
    direct_fn_edges: Vec<Vec<LocalFunctionId>>,
    free_vars: Vec<FxHashSet<VarId>>,
    free_blocks: Vec<FxHashSet<BlockId>>,
    free_tagbodies: Vec<FxHashSet<TagbodyId>>,
    free_local_fns: Vec<FxHashSet<LocalFunctionId>>,
    has_raw_special: Vec<bool>,
}

impl NestingBuilder<'_> {
    fn bind_var(&mut self, var: VarId, owner: Option<LocalFunctionId>) {
        self.var_owner[var.index()] = owner;
        if let Some(lf) = owner {
            self.bound_vars[lf.index()].insert(var);
        }
    }

    fn walk_params(&mut self, params: &CoreParameterBindings, owner: Option<LocalFunctionId>) {
        for &var in &params.required {
            self.bind_var(var, owner);
        }
        for opt in &params.optional {
            if let Some(init) = &opt.init {
                self.walk_expr(init, owner);
            }
            self.bind_var(opt.var, owner);
            if let Some(supplied) = opt.supplied {
                self.bind_var(supplied, owner);
            }
        }
        if let Some(rest) = params.rest {
            self.bind_var(rest, owner);
        }
        if let Some(kws) = &params.keywords {
            for kw in &kws.params {
                if let Some(init) = &kw.param.init {
                    self.walk_expr(init, owner);
                }
                self.bind_var(kw.param.var, owner);
                if let Some(supplied) = kw.param.supplied {
                    self.bind_var(supplied, owner);
                }
            }
        }
        for (var, init) in &params.aux {
            if let Some(init) = init {
                self.walk_expr(init, owner);
            }
            self.bind_var(*var, owner);
        }
    }

    fn note_var_use(&mut self, var: VarId, current: Option<LocalFunctionId>) {
        if let Some(lf) = current
            && !self.bound_vars[lf.index()].contains(&var)
        {
            self.free_vars[lf.index()].insert(var);
        }
    }

    fn note_fn_use(&mut self, target: LocalFunctionId, current: Option<LocalFunctionId>) {
        if let Some(lf) = current
            && target != lf
        {
            self.direct_fn_edges[lf.index()].push(target);
            if self.local_fn_owner[target.index()] != Some(lf) {
                self.free_local_fns[lf.index()].insert(target);
            }
        }
    }

    fn walk_expr(&mut self, expr: &CoreExpr, current: Option<LocalFunctionId>) {
        match &expr.kind {
            CoreKind::Const(_)
            | CoreKind::ReadGlobal(_)
            | CoreKind::ReadGlobalFn { .. }
            | CoreKind::LoadTimeValue { .. } => {}
            CoreKind::ReadVar(var) => self.note_var_use(*var, current),
            CoreKind::WriteVar { var, value } => {
                self.note_var_use(*var, current);
                self.walk_expr(value, current);
            }
            CoreKind::WriteGlobal { value, .. } | CoreKind::The { expr: value, .. } => {
                self.walk_expr(value, current);
            }
            CoreKind::ReadLocalFn(id) => self.note_fn_use(*id, current),
            CoreKind::Progn(es) => {
                for e in es {
                    self.walk_expr(e, current);
                }
            }
            CoreKind::If { test, then_, else_ } => {
                self.walk_expr(test, current);
                self.walk_expr(then_, current);
                self.walk_expr(else_, current);
            }
            CoreKind::Let {
                sequential,
                bindings,
                body,
            } => {
                if *sequential {
                    for (target, rhs) in bindings {
                        self.walk_expr(rhs, current);
                        if let CoreBindingTarget::Lexical(v) = target {
                            self.bind_var(*v, current);
                        }
                    }
                } else {
                    for (_, rhs) in bindings {
                        self.walk_expr(rhs, current);
                    }
                    for (target, _) in bindings {
                        if let CoreBindingTarget::Lexical(v) = target {
                            self.bind_var(*v, current);
                        }
                    }
                }
                self.walk_expr(body, current);
            }
            CoreKind::Flet { fns, body, .. } => {
                let parent_depth = current.map_or(0, |p| self.fn_depth[p.index()]);
                for &lf_id in fns {
                    let idx = lf_id.index();
                    self.is_locally_defined[idx] = true;
                    self.local_fn_owner[idx] = current;
                    self.syntactic_parent[idx] = current;
                    self.fn_depth[idx] = parent_depth + 1;
                }
                for &lf_id in fns {
                    let local = &self.fn_ir.local_functions[lf_id.index()];
                    self.walk_params(&local.params, Some(lf_id));
                    self.walk_expr(&local.body, Some(lf_id));
                }
                self.walk_expr(body, current);
            }
            CoreKind::Block { id, body } => {
                self.block_owner[id.index()] = current;
                self.walk_expr(body, current);
            }
            CoreKind::ReturnFrom { id, value } => {
                self.walk_expr(value, current);
                if let Some(lf) = current
                    && self.block_owner[id.index()] != Some(lf)
                {
                    self.free_blocks[lf.index()].insert(*id);
                }
            }
            CoreKind::Tagbody { id, segments } => {
                self.tagbody_owner[id.index()] = current;
                for (_, es) in segments {
                    for e in es {
                        self.walk_expr(e, current);
                    }
                }
            }
            CoreKind::Go { id, .. } => {
                if let Some(lf) = current
                    && self.tagbody_owner[id.index()] != Some(lf)
                {
                    self.free_tagbodies[lf.index()].insert(*id);
                }
            }
            CoreKind::Call { callee, args, .. } => {
                match callee {
                    CoreCallee::Local(id) => self.note_fn_use(*id, current),
                    CoreCallee::Expr(e) => self.walk_expr(e, current),
                    CoreCallee::Named(_) => {}
                }
                for arg in args {
                    self.walk_expr(arg, current);
                }
            }
            CoreKind::CallValues { callee, forms } => {
                self.walk_expr(callee, current);
                for f in forms {
                    self.walk_expr(f, current);
                }
            }
            CoreKind::Lambda { func } => {
                if let Some(lf) = current {
                    for &(_, outer_v) in &func.parent_var_captures {
                        self.note_var_use(outer_v, current);
                    }
                    for &(_, outer_b) in &func.parent_block_captures {
                        if self.block_owner[outer_b.index()] != Some(lf) {
                            self.free_blocks[lf.index()].insert(outer_b);
                        }
                    }
                    for &(_, outer_tb) in &func.parent_tagbody_captures {
                        if self.tagbody_owner[outer_tb.index()] != Some(lf) {
                            self.free_tagbodies[lf.index()].insert(outer_tb);
                        }
                    }
                    for &(_, outer_fn) in &func.parent_function_captures {
                        self.note_fn_use(outer_fn, current);
                    }
                    if fn_has_raw_special(func) {
                        self.has_raw_special[lf.index()] = true;
                    }
                }
            }
            CoreKind::RawSpecial(..) => {
                if let Some(lf) = current {
                    self.has_raw_special[lf.index()] = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core_ir::{BlockExitTarget, CoreConst, LocalFunction, LocalFunctionBinding, VariableBinding};
    use crate::symbol::Name;
    use zkcl_target::CompilationTarget;

    fn var_binding(name: &str) -> VariableBinding {
        VariableBinding {
            name: Name::common_lisp(name),
            special: false,
            assigned: false,
            captured: false,
            read_count: 0,
            trusted_type: crate::core_ir::CoreType::Top,
            dynamic_extent: false,
        }
    }

    fn fn_binding(name: &str) -> LocalFunctionBinding {
        LocalFunctionBinding {
            name: Name::common_lisp(name),
            setf: false,
            call_count: 0,
            escapes: false,
            recursive: false,
            has_notinline_call: false,
            dynamic_extent: false,
        }
    }

    fn fixed_params(vars: Vec<VarId>) -> CoreParameterBindings {
        CoreParameterBindings::new(vars.clone(), false, vars, Vec::new(), None, None, Vec::new())
    }

    fn empty_fn() -> CoreFn {
        CoreFn::new(
            &CompilationTarget::native(),
            None,
            CoreParameterBindings::empty(),
            Box::new(CoreExpr::nil(0)),
            0,
        )
    }

    #[test]
    fn closed_nested_flet_is_hoistable_with_zero_captures() {
        let mut fn_ir = empty_fn();
        let outer_x = fn_ir.alloc_var(var_binding("X"));
        fn_ir.params = fixed_params(vec![outer_x]);

        let mid_y = fn_ir.alloc_var(var_binding("Y"));
        let leaf_z = fn_ir.alloc_var(var_binding("Z"));
        let leaf_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("LEAF"),
            params: fixed_params(vec![leaf_z]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(0, CoreKind::ReadVar(leaf_z))),
            at: 0,
        });
        let mid_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("MID"),
            params: fixed_params(vec![mid_y]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(
                0,
                CoreKind::Flet {
                    recursive: false,
                    fns: vec![leaf_id],
                    body: Box::new(CoreExpr::new(0, CoreKind::ReadVar(outer_x))),
                },
            )),
            at: 0,
        });
        fn_ir.body = Box::new(CoreExpr::new(
            0,
            CoreKind::Flet {
                recursive: false,
                fns: vec![mid_id],
                body: Box::new(CoreExpr::nil(0)),
            },
        ));

        let tree = CoreNestingTree::compute(&fn_ir);
        assert!(tree.has_no_captures(leaf_id));
        assert!(tree.is_hoistable_to_root(leaf_id));
        assert_eq!(tree.syntactic_parent(leaf_id), Some(mid_id));
        assert_eq!(tree.nesting_parent(leaf_id), None);
        assert!(!tree.has_no_captures(mid_id));
        assert_eq!(tree.minimal_free_vars(mid_id).iter().copied().collect::<Vec<_>>(), vec![outer_x]);
        assert!(tree.is_hoistable_to_root(mid_id));
    }

    #[test]
    fn nesting_tree_skips_unused_intermediate_local_function() {
        let mut fn_ir = empty_fn();
        let f_var = fn_ir.alloc_var(var_binding("F-VAR"));
        let g_var = fn_ir.alloc_var(var_binding("G-VAR"));
        let h_var = fn_ir.alloc_var(var_binding("H-VAR"));

        // Leaf H inside G (inside F): reads F-VAR, ignores G-VAR!
        let h_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("H"),
            params: fixed_params(vec![h_var]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(0, CoreKind::ReadVar(f_var))),
            at: 0,
        });
        let g_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("G"),
            params: fixed_params(vec![g_var]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(
                0,
                CoreKind::Flet {
                    recursive: false,
                    fns: vec![h_id],
                    body: Box::new(CoreExpr::new(0, CoreKind::ReadVar(g_var))),
                },
            )),
            at: 0,
        });
        let f_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("F"),
            params: fixed_params(vec![f_var]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(
                0,
                CoreKind::Flet {
                    recursive: false,
                    fns: vec![g_id],
                    body: Box::new(CoreExpr::nil(0)),
                },
            )),
            at: 0,
        });
        fn_ir.body = Box::new(CoreExpr::new(
            0,
            CoreKind::Flet {
                recursive: false,
                fns: vec![f_id],
                body: Box::new(CoreExpr::nil(0)),
            },
        ));

        let tree = CoreNestingTree::compute(&fn_ir);
        assert_eq!(tree.syntactic_parent(h_id), Some(g_id));
        assert_eq!(tree.nesting_parent(h_id), Some(f_id));
        assert_eq!(tree.nesting_parent(g_id), None);
    }

    #[test]
    fn mutually_recursive_labels_propagate_transitive_free_vars() {
        let mut fn_ir = empty_fn();
        let outer_k = fn_ir.alloc_var(var_binding("K"));
        let outer_fn_id;
        let even_id;
        let odd_id;

        // Allocate stubs first so mutual calls have valid IDs.
        let e_param = fn_ir.alloc_var(var_binding("N"));
        let o_param = fn_ir.alloc_var(var_binding("M"));
        let outer_k_dummy = outer_k;

        even_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("EVEN"),
            params: fixed_params(vec![e_param]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::nil(0)),
            at: 0,
        });
        odd_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("ODD"),
            params: fixed_params(vec![o_param]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::nil(0)),
            at: 0,
        });
        outer_fn_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("OUTER"),
            params: fixed_params(vec![outer_k_dummy]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(
                0,
                CoreKind::Flet {
                    recursive: true,
                    fns: vec![even_id, odd_id],
                    body: Box::new(CoreExpr::nil(0)),
                },
            )),
            at: 0,
        });

        let call_local = |callee, arg| {
            CoreExpr::new(
                0,
                CoreKind::Call {
                    callee: CoreCallee::Local(callee),
                    args: vec![arg],
                    qualities: crate::core_ir::OptimizationQualities::default(),
                    inline_declaration: None,
                    ftype_ret: crate::core_ir::CoreType::Top,
                    fixnum_arithmetic: None,
                },
            )
        };
        // EVEN calls ODD (no direct read of K).
        *fn_ir.local_functions[even_id.index()].body =
            call_local(odd_id, CoreExpr::new(0, CoreKind::ReadVar(e_param)));
        // ODD reads OUTER's parameter K and calls EVEN.
        *fn_ir.local_functions[odd_id.index()].body = CoreExpr::new(
            0,
            CoreKind::Progn(vec![
                CoreExpr::new(0, CoreKind::ReadVar(outer_k)),
                call_local(even_id, CoreExpr::new(0, CoreKind::ReadVar(o_param))),
            ]),
        );

        fn_ir.body = Box::new(CoreExpr::new(
            0,
            CoreKind::Flet {
                recursive: false,
                fns: vec![outer_fn_id],
                body: Box::new(CoreExpr::nil(0)),
            },
        ));

        let tree = CoreNestingTree::compute(&fn_ir);
        assert!(tree.minimal_free_vars(even_id).contains(&outer_k));
        assert!(tree.minimal_free_vars(odd_id).contains(&outer_k));
        assert_eq!(tree.nesting_parent(even_id), Some(outer_fn_id));
        assert_eq!(tree.nesting_parent(odd_id), Some(outer_fn_id));
    }

    #[test]
    fn non_local_return_from_pins_nesting_parent_to_block_owner() {
        let mut fn_ir = empty_fn();
        let blk = fn_ir.alloc_block(BlockExitTarget {
            name: Name::common_lisp("STOP"),
            used: true,
            escapes: true,
            crosses_dynamic_scope: false,
        });
        let inner_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("INNER"),
            params: CoreParameterBindings::empty(),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(
                0,
                CoreKind::ReturnFrom {
                    id: blk,
                    value: Box::new(CoreExpr::new(0, CoreKind::Const(CoreConst::Fixnum(42)))),
                },
            )),
            at: 0,
        });
        let owner_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("OWNER"),
            params: CoreParameterBindings::empty(),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(
                0,
                CoreKind::Block {
                    id: blk,
                    body: Box::new(CoreExpr::new(
                        0,
                        CoreKind::Flet {
                            recursive: false,
                            fns: vec![inner_id],
                            body: Box::new(CoreExpr::nil(0)),
                        },
                    )),
                },
            )),
            at: 0,
        });
        fn_ir.body = Box::new(CoreExpr::new(
            0,
            CoreKind::Flet {
                recursive: false,
                fns: vec![owner_id],
                body: Box::new(CoreExpr::nil(0)),
            },
        ));

        let tree = CoreNestingTree::compute(&fn_ir);
        assert!(!tree.has_no_captures(inner_id));
        assert_eq!(tree.nesting_parent(inner_id), Some(owner_id));
    }

    #[test]
    fn hoist_closed_local_functions_lifts_closed_flet_to_root_and_preserves_capturing_flet() {
        let mut fn_ir = empty_fn();
        let outer_x = fn_ir.alloc_var(var_binding("X"));
        fn_ir.params = fixed_params(vec![outer_x]);

        let mid_y = fn_ir.alloc_var(var_binding("Y"));
        let leaf_z = fn_ir.alloc_var(var_binding("Z"));
        let cap_w = fn_ir.alloc_var(var_binding("W"));

        // Closed LEAF nested inside MID: must be hoisted to root.
        let leaf_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("LEAF"),
            params: fixed_params(vec![leaf_z]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(0, CoreKind::ReadVar(leaf_z))),
            at: 0,
        });
        // CAP nested inside MID capturing MID's Y: must stay inside MID!
        let cap_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("CAP"),
            params: fixed_params(vec![cap_w]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(0, CoreKind::ReadVar(mid_y))),
            at: 0,
        });
        let mid_id = fn_ir.alloc_local_fn(LocalFunction {
            binding: fn_binding("MID"),
            params: fixed_params(vec![mid_y]),
            source_lambda_list: None,
            source_expression: None,
            body: Box::new(CoreExpr::new(
                0,
                CoreKind::Flet {
                    recursive: false,
                    fns: vec![leaf_id, cap_id],
                    body: Box::new(CoreExpr::new(0, CoreKind::ReadVar(outer_x))),
                },
            )),
            at: 0,
        });
        fn_ir.body = Box::new(CoreExpr::new(
            0,
            CoreKind::Flet {
                recursive: false,
                fns: vec![mid_id],
                body: Box::new(CoreExpr::new(0, CoreKind::ReadLocalFn(mid_id))),
            },
        ));

        assert!(hoist_closed_local_functions(&mut fn_ir));
        assert!(!hoist_closed_local_functions(&mut fn_ir));

        // Root body should now wrap old body with `Flet([leaf_id], ...)`.
        let CoreKind::Flet { fns, .. } = &fn_ir.body.kind else {
            panic!("expected root body wrapped in hoisted Flet");
        };
        assert_eq!(fns, &[leaf_id]);

        // MID's body should retain only `cap_id`.
        let CoreKind::Flet { fns: mid_fns, .. } = &fn_ir.local_functions[mid_id.index()].body.kind
        else {
            panic!("expected MID to keep inner Flet for capturing CAP");
        };
        assert_eq!(mid_fns, &[cap_id]);
    }
}
