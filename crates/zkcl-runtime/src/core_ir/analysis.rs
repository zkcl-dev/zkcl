//! Lexical uses, captures, recursion and local control transfers.

use std::mem::DropGuard;

use super::{
    BlockId, CoreBindingTarget, CoreCallee, CoreExpr, CoreFn, CoreKind, CoreParameterBindings,
    InlineDeclaration, LocalFunctionId, TagbodyId, VarId, VariableBinding, fn_has_raw_special,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlTransferTarget {
    Block(BlockId),
    Tagbody(TagbodyId),
}

struct ControlTransferScope {
    target: ControlTransferTarget,
    transfers: Vec<ControlTransferTarget>,
}

#[derive(Default)]
pub(super) struct FunctionAnalysisContext {
    dynamic_depth: usize,
    block_dynamic_depths: Vec<usize>,
    tagbody_dynamic_depths: Vec<usize>,
    var_owner: Vec<Option<LocalFunctionId>>,
    block_owner: Vec<Option<LocalFunctionId>>,
    tagbody_owner: Vec<Option<LocalFunctionId>>,
    active_vars: Vec<VarId>,
    transfer_scopes: Vec<ControlTransferScope>,
    active_local_fns: Vec<LocalFunctionId>,
    current_local_function: Option<LocalFunctionId>,
    local_function_use_edges: Vec<Vec<LocalFunctionId>>,
    local_functions_used_by_body: Vec<LocalFunctionId>,
    reachable_local_functions: Vec<bool>,
}

impl FunctionAnalysisContext {
    fn bind_var(&mut self, var: VarId, vars: &[VariableBinding]) {
        self.var_owner[var.index()] = self.current_local_function;
        self.active_vars.push(var);
        if vars[var.index()].special {
            self.dynamic_depth += 1;
        }
    }

    fn bind_target(&mut self, target: &CoreBindingTarget, vars: &[VariableBinding]) {
        match target {
            CoreBindingTarget::Lexical(var) => self.bind_var(*var, vars),
            CoreBindingTarget::Special(_) => self.dynamic_depth += 1,
        }
    }

    fn local_function_uses(&mut self) -> &mut Vec<LocalFunctionId> {
        match self.current_local_function {
            Some(caller) => &mut self.local_function_use_edges[caller.index()],
            None => &mut self.local_functions_used_by_body,
        }
    }

    fn enter_transfer_scope(&mut self, target: ControlTransferTarget) {
        self.transfer_scopes.push(ControlTransferScope {
            target,
            transfers: Vec::new(),
        });
    }

    fn note_transfer(&mut self, target: ControlTransferTarget) {
        if let Some(scope) = self.transfer_scopes.last_mut()
            && scope.target != target
        {
            scope.transfers.push(target);
        }
    }

    fn leave_transfer_scope(&mut self, fn_ir: &mut CoreFn) {
        let scope = self.transfer_scopes.pop().expect("active exit scope");
        let needs_unwind = match scope.target {
            ControlTransferTarget::Block(id) => {
                let info = &fn_ir.blocks[id.index()];
                info.used && !info.is_local_jump()
            }
            ControlTransferTarget::Tagbody(id) => {
                let info = &fn_ir.tagbodies[id.index()];
                info.used && !info.is_local_jump()
            }
        };
        // All branches of this scope have been analyzed. If it gets a runtime
        // exit record, transfers past it must unwind that record. Otherwise
        // defer the transfers that also leave the next enclosing scope.
        if needs_unwind {
            for target in scope.transfers {
                match target {
                    ControlTransferTarget::Block(id) => {
                        fn_ir.blocks[id.index()].crosses_dynamic_scope = true
                    }
                    ControlTransferTarget::Tagbody(id) => {
                        fn_ir.tagbodies[id.index()].crosses_dynamic_scope = true
                    }
                }
            }
        } else if let Some(parent) = self.transfer_scopes.last_mut() {
            let parent_target = parent.target;
            parent.transfers.extend(
                scope
                    .transfers
                    .into_iter()
                    .filter(|&target| target != parent_target),
            );
        }
    }
}

fn analyze_definition(
    params: &mut CoreParameterBindings,
    body: &mut CoreExpr,
    fn_ir: &mut CoreFn,
    state: &mut FunctionAnalysisContext,
) {
    params.for_each_binding_mut(|var, init| {
        if let Some(init) = init {
            analyze_expr(init, fn_ir, state);
        }
        state.bind_var(*var, &fn_ir.vars);
    });
    analyze_expr(body, fn_ir, state);
}

/// Recomputes variable read/assign/capture, local fn call/escape/recursive,
/// and block/tagbody local-jump flags across `fn_ir` and nested lambdas.
pub(super) fn analyze_fn(fn_ir: &mut CoreFn, state: &mut FunctionAnalysisContext) {
    for v in &mut fn_ir.vars {
        v.assigned = false;
        v.captured = false;
        v.read_count = 0;
    }
    for lf in &mut fn_ir.local_functions {
        lf.binding.call_count = 0;
        lf.binding.escapes = false;
        lf.binding.recursive = false;
        lf.binding.has_notinline_call = false;
    }
    for b in &mut fn_ir.blocks {
        b.used = false;
        b.escapes = false;
        b.crosses_dynamic_scope = false;
    }
    for tb in &mut fn_ir.tagbodies {
        tb.used = false;
        tb.escapes = false;
        tb.crosses_dynamic_scope = false;
    }

    // Inlining can grow the binding tables between rounds. Reset the facts,
    // retaining storage already allocated by the preceding analysis.
    state.dynamic_depth = 0;
    state.current_local_function = None;
    state.block_dynamic_depths.clear();
    state.block_dynamic_depths.resize(fn_ir.blocks.len(), 0);
    state.tagbody_dynamic_depths.clear();
    state
        .tagbody_dynamic_depths
        .resize(fn_ir.tagbodies.len(), 0);
    state.var_owner.clear();
    state.var_owner.resize(fn_ir.vars.len(), None);
    state.block_owner.clear();
    state.block_owner.resize(fn_ir.blocks.len(), None);
    state.tagbody_owner.clear();
    state.tagbody_owner.resize(fn_ir.tagbodies.len(), None);
    state.active_vars.clear();
    state
        .active_vars
        .extend(fn_ir.parent_var_captures.iter().map(|&(child, _)| child));
    state.transfer_scopes.clear();
    state.active_local_fns.clear();
    state.active_local_fns.extend(
        fn_ir
            .parent_function_captures
            .iter()
            .map(|&(child, _)| child),
    );
    state
        .local_function_use_edges
        .resize_with(fn_ir.local_functions.len(), Vec::new);
    for edges in &mut state.local_function_use_edges {
        edges.clear();
    }
    state.local_functions_used_by_body.clear();
    state.reachable_local_functions.clear();
    state
        .reachable_local_functions
        .resize(fn_ir.local_functions.len(), false);

    let mut params = std::mem::replace(&mut fn_ir.params, CoreParameterBindings::empty());
    let mut body = std::mem::replace(&mut fn_ir.body, Box::new(CoreExpr::nil(0)));
    analyze_definition(&mut params, &mut body, fn_ir, state);
    fn_ir.params = params;
    fn_ir.body = body;

    // Prune self-call counts on local functions unreachable from `fn_ir.body`.
    if !fn_ir.local_functions.is_empty() {
        while let Some(id) = state.local_functions_used_by_body.pop() {
            let idx = id.index();
            if !state.reachable_local_functions[idx] {
                state.reachable_local_functions[idx] = true;
                state
                    .local_functions_used_by_body
                    .extend_from_slice(&state.local_function_use_edges[idx]);
            }
        }
        for (idx, lf) in fn_ir.local_functions.iter_mut().enumerate() {
            if !state.reachable_local_functions[idx] {
                lf.binding.call_count = 0;
                lf.binding.escapes = false;
            }
        }
    }
    compute_local_fn_recursion(fn_ir, &state.local_function_use_edges);
}

fn compute_local_fn_recursion(fn_ir: &mut CoreFn, edges: &[Vec<LocalFunctionId>]) {
    // Singletons recurse only through a self edge. For mutually recursive
    // LABELS functions, mark each strongly connected component in two walks.
    let mut has_cross_call = false;
    for (from, calls) in edges.iter().enumerate() {
        for to in calls {
            if from == to.index() {
                fn_ir.local_functions[from].binding.recursive = true;
            } else {
                has_cross_call = true;
            }
        }
    }
    if !has_cross_call {
        return;
    }
    let mut callers = vec![Vec::new(); edges.len()];
    for (from, calls) in edges.iter().enumerate() {
        for to in calls {
            callers[to.index()].push(from);
        }
    }
    let mut seen = vec![false; edges.len()];
    let mut order = Vec::with_capacity(edges.len());
    let mut stack = Vec::new();
    for start in 0..edges.len() {
        if seen[start] {
            continue;
        }
        stack.push((start, false));
        while let Some((node, finished)) = stack.pop() {
            if finished {
                order.push(node);
            } else if !std::mem::replace(&mut seen[node], true) {
                stack.push((node, true));
                stack.extend(edges[node].iter().map(|to| (to.index(), false)));
            }
        }
    }
    let mut component = Vec::new();
    for start in order.into_iter().rev() {
        if !std::mem::take(&mut seen[start]) {
            continue;
        }
        component.clear();
        component.push(start);
        let mut next = 0;
        while next < component.len() {
            let node = component[next];
            next += 1;
            for &caller in &callers[node] {
                if std::mem::take(&mut seen[caller]) {
                    component.push(caller);
                }
            }
        }
        if component.len() > 1 {
            for &node in &component {
                fn_ir.local_functions[node].binding.recursive = true;
            }
        }
    }
}

fn analyze_expr(expr: &mut CoreExpr, fn_ir: &mut CoreFn, state: &mut FunctionAnalysisContext) {
    match &mut expr.kind {
        CoreKind::Const(_)
        | CoreKind::ReadGlobal(_)
        | CoreKind::ReadGlobalFn { .. }
        | CoreKind::LoadTimeValue { .. } => {}
        CoreKind::ReadVar(var) => {
            let info = &mut fn_ir.vars[var.index()];
            info.read_count = info.read_count.saturating_add(1);
            if state.var_owner[var.index()] != state.current_local_function {
                info.captured = true;
            }
        }
        CoreKind::WriteVar { var, value } => {
            let info = &mut fn_ir.vars[var.index()];
            info.assigned = true;
            if state.var_owner[var.index()] != state.current_local_function {
                info.captured = true;
            }
            analyze_expr(value, fn_ir, state);
        }
        CoreKind::WriteGlobal { value, .. } | CoreKind::The { expr: value, .. } => {
            analyze_expr(value, fn_ir, state);
        }
        CoreKind::ReadLocalFn(id) => {
            fn_ir.local_functions[id.index()].binding.escapes = true;
            state.local_function_uses().push(*id);
        }
        CoreKind::Progn(es) => {
            for e in es {
                analyze_expr(e, fn_ir, state);
            }
        }
        CoreKind::If { test, then_, else_ } => {
            analyze_expr(test, fn_ir, state);
            analyze_expr(then_, fn_ir, state);
            analyze_expr(else_, fn_ir, state);
        }
        CoreKind::Let {
            sequential,
            bindings,
            body,
        } => {
            let saved_vars = state.active_vars.len();
            let saved_dyn = state.dynamic_depth;
            if *sequential {
                for (target, rhs) in bindings.iter_mut() {
                    analyze_expr(rhs, fn_ir, state);
                    state.bind_target(target, &fn_ir.vars);
                }
            } else {
                for (_, rhs) in bindings.iter_mut() {
                    analyze_expr(rhs, fn_ir, state);
                }
                for (target, _) in bindings.iter() {
                    state.bind_target(target, &fn_ir.vars);
                }
            }
            analyze_expr(body, fn_ir, state);
            state.dynamic_depth = saved_dyn;
            state.active_vars.truncate(saved_vars);
        }
        CoreKind::Flet {
            recursive,
            fns,
            body,
        } => {
            let saved_fns = state.active_local_fns.len();
            if *recursive {
                state.active_local_fns.extend_from_slice(fns);
            }
            for &lf_id in fns.iter() {
                let saved_vars = state.active_vars.len();
                let saved_dyn = state.dynamic_depth;
                let prev_fn = state.current_local_function.replace(lf_id);
                let mut guard = DropGuard::new(
                    (&mut *state, saved_vars, saved_dyn, prev_fn),
                    |(state, saved_vars, saved_dyn, prev_fn)| {
                        state.current_local_function = prev_fn;
                        state.dynamic_depth = saved_dyn;
                        state.active_vars.truncate(saved_vars);
                    },
                );
                let (mut params, mut definition) =
                    fn_ir.local_functions[lf_id.index()].take_definition();
                analyze_definition(&mut params, &mut definition, fn_ir, guard.0);
                let local = &mut fn_ir.local_functions[lf_id.index()];
                local.params = params;
                local.body = definition;
            }
            if !*recursive {
                state.active_local_fns.extend_from_slice(fns);
            }
            analyze_expr(body, fn_ir, state);
            state.active_local_fns.truncate(saved_fns);
        }
        CoreKind::Block { id, body } => {
            state.block_owner[id.index()] = state.current_local_function;
            state.block_dynamic_depths[id.index()] = state.dynamic_depth;
            state.enter_transfer_scope(ControlTransferTarget::Block(*id));
            analyze_expr(body, fn_ir, state);
            state.leave_transfer_scope(fn_ir);
        }
        CoreKind::ReturnFrom { id, value } => {
            analyze_expr(value, fn_ir, state);
            let b = &mut fn_ir.blocks[id.index()];
            b.used = true;
            if state.block_owner[id.index()] != state.current_local_function {
                b.escapes = true;
            }
            if state.dynamic_depth > state.block_dynamic_depths[id.index()] {
                b.crosses_dynamic_scope = true;
            }
            state.note_transfer(ControlTransferTarget::Block(*id));
        }
        CoreKind::Tagbody { id, segments } => {
            state.tagbody_owner[id.index()] = state.current_local_function;
            state.tagbody_dynamic_depths[id.index()] = state.dynamic_depth;
            state.enter_transfer_scope(ControlTransferTarget::Tagbody(*id));
            for (_, es) in segments {
                for e in es {
                    analyze_expr(e, fn_ir, state);
                }
            }
            state.leave_transfer_scope(fn_ir);
        }
        CoreKind::Go { id, .. } => {
            let tb = &mut fn_ir.tagbodies[id.index()];
            tb.used = true;
            if state.tagbody_owner[id.index()] != state.current_local_function {
                tb.escapes = true;
            }
            if state.dynamic_depth > state.tagbody_dynamic_depths[id.index()] {
                tb.crosses_dynamic_scope = true;
            }
            state.note_transfer(ControlTransferTarget::Tagbody(*id));
        }
        CoreKind::Call {
            callee,
            args,
            inline_declaration,
            ..
        } => {
            match callee {
                CoreCallee::Named(_) => {}
                CoreCallee::Local(id) => {
                    let info = &mut fn_ir.local_functions[id.index()].binding;
                    info.call_count = info.call_count.saturating_add(1);
                    if *inline_declaration == Some(InlineDeclaration::NotInline) {
                        info.has_notinline_call = true;
                    }
                    state.local_function_uses().push(*id);
                }
                CoreCallee::Expr(e) => analyze_expr(e, fn_ir, state),
            }
            for arg in args {
                analyze_expr(arg, fn_ir, state);
            }
        }
        CoreKind::CallValues { callee, forms } => {
            analyze_expr(callee, fn_ir, state);
            if let [first, rest @ ..] = forms.as_mut_slice() {
                analyze_expr(first, fn_ir, state);
                // Later operands execute under a saved-values scope. A
                // lexical transfer out of it must unwind that scope.
                state.dynamic_depth += 1;
                for form in rest {
                    analyze_expr(form, fn_ir, state);
                }
                state.dynamic_depth -= 1;
            }
        }
        CoreKind::Lambda { func } => analyze_lambda(func, fn_ir, state),
        CoreKind::RawSpecial(..) => preserve_raw_special_scope(fn_ir, state),
    }
}

fn analyze_lambda(child: &mut CoreFn, parent: &mut CoreFn, state: &mut FunctionAnalysisContext) {
    analyze_fn(child, &mut FunctionAnalysisContext::default());
    let keep_scope = fn_has_raw_special(child);
    if keep_scope {
        preserve_raw_special_scope(parent, state);
    }

    child.parent_var_captures.retain(|&(inner, outer)| {
        let binding = &child.vars[inner.index()];
        let used = binding.read_count > 0 || binding.assigned || binding.captured;
        if used {
            let enclosing = &mut parent.vars[outer.index()];
            enclosing.captured = true;
            enclosing.read_count = enclosing.read_count.saturating_add(binding.read_count);
            enclosing.assigned |= binding.assigned;
        }
        used || keep_scope
    });
    child.parent_block_captures.retain(|&(inner, outer)| {
        let used = child.blocks[inner.index()].used;
        if used {
            let enclosing = &mut parent.blocks[outer.index()];
            enclosing.used = true;
            enclosing.escapes = true;
        }
        used || keep_scope
    });
    child.parent_tagbody_captures.retain(|&(inner, outer)| {
        let used = child.tagbodies[inner.index()].used;
        if used {
            let enclosing = &mut parent.tagbodies[outer.index()];
            enclosing.used = true;
            enclosing.escapes = true;
        }
        used || keep_scope
    });
    child.parent_function_captures.retain(|&(inner, outer)| {
        let binding = &child.local_functions[inner.index()].binding;
        if binding.call_count == 0 && !binding.escapes && !keep_scope {
            return false;
        }
        let enclosing = &mut parent.local_functions[outer.index()].binding;
        enclosing.call_count = enclosing.call_count.saturating_add(binding.call_count);
        enclosing.escapes = true;
        enclosing.has_notinline_call |= binding.has_notinline_call;
        state.local_function_uses().push(outer);
        true
    });
}

/// The bytecode compiler will expand subforms in the lexical environment at this
/// point. A macro can introduce a reference, assignment, or closure over a name
/// absent from its source, so the Core IR cannot infer which bindings it will use.
fn preserve_raw_special_scope(fn_ir: &mut CoreFn, state: &mut FunctionAnalysisContext) {
    for &var in &state.active_vars {
        let info = &mut fn_ir.vars[var.index()];
        info.read_count = info.read_count.saturating_add(1);
        // Keep a writable binding. Bytecode emission records and boxes actual
        // captures when it compiles the expansion in this environment.
        info.assigned = true;
    }
    let targets = state
        .transfer_scopes
        .iter()
        .map(|scope| scope.target)
        .chain(
            fn_ir
                .parent_block_captures
                .iter()
                .map(|&(id, _)| ControlTransferTarget::Block(id)),
        )
        .chain(
            fn_ir
                .parent_tagbody_captures
                .iter()
                .map(|&(id, _)| ControlTransferTarget::Tagbody(id)),
        );
    for target in targets {
        match target {
            ControlTransferTarget::Block(id) => {
                let info = &mut fn_ir.blocks[id.index()];
                info.used = true;
                info.crosses_dynamic_scope = true;
                info.escapes = true;
            }
            ControlTransferTarget::Tagbody(id) => {
                let info = &mut fn_ir.tagbodies[id.index()];
                info.used = true;
                info.crosses_dynamic_scope = true;
                info.escapes = true;
            }
        }
    }
    let call_edges = match state.current_local_function {
        Some(caller) => &mut state.local_function_use_edges[caller.index()],
        None => &mut state.local_functions_used_by_body,
    };
    call_edges.extend_from_slice(&state.active_local_fns);
    for &lf in &state.active_local_fns {
        let info = &mut fn_ir.local_functions[lf.index()].binding;
        info.call_count = info.call_count.saturating_add(1);
        info.escapes = true;
    }
}
