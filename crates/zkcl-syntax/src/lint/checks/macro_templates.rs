//! Audits evaluated positions and binding capture in macro backquote templates.

use rowan::WalkEvent;
use rustc_hash::FxHashMap;

use crate::cl_builtins::evaluates_all_arguments;
use crate::navigation::Namespace;
use crate::navigation::index::{closed_list, possibly_read, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

use super::super::MacroShape;
use super::super::context::{
    LintDocument, LintEmitter, is_nil_element, is_t_element, unwrap_quote,
};
use super::super::macro_template::{let_binding, reader_operand};

const MAX_AUDIT_DEPTH: usize = 80;
const MAX_AUDIT_PARTS: usize = 8192;

type ExpansionBindings = FxHashMap<String, usize>;

pub(super) fn check(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    if ![
        "suspicious/macro-multiple-evaluation",
        "suspicious/macro-unhygienic-binding",
        "suspicious/macro-evaluation-in-loop",
    ]
    .into_iter()
    .any(|rule| emit.is_enabled(rule))
    {
        return Some(());
    }
    let [_, params_el, body @ ..] = args else {
        return Some(());
    };
    if !within_audit_budget(std::iter::once(params_el).chain(body), cancelled)? {
        return Some(());
    }
    let mut scalar_params: Vec<(String, TextRange)> = Vec::new();
    collect_scalar_macro_params(params_el, &mut scalar_params);
    let bindings = scalar_params
        .iter()
        .enumerate()
        .map(|(slot, (name, _))| (name.clone(), slot))
        .collect();
    ExpansionAudit {
        ctx,
        package,
        scalar_params: &scalar_params,
        emit,
        cancelled,
    }
    .sequence(body.iter().cloned(), bindings)
}

fn within_audit_budget<'a>(
    forms: impl IntoIterator<Item = &'a Element>,
    cancelled: &impl Fn() -> bool,
) -> Option<bool> {
    let mut parts = 0;
    for form in forms {
        let Some(node) = form.as_node() else {
            parts += 1;
            if parts > MAX_AUDIT_PARTS {
                return Some(false);
            }
            if parts % 64 == 0 && cancelled() {
                return None;
            }
            continue;
        };
        let mut depth = 0;
        for event in node.preorder_with_tokens() {
            match event {
                WalkEvent::Enter(_) => {
                    if parts % 64 == 0 && cancelled() {
                        return None;
                    }
                    parts += 1;
                    depth += 1;
                    if parts > MAX_AUDIT_PARTS || depth > MAX_AUDIT_DEPTH {
                        return Some(false);
                    }
                }
                WalkEvent::Leave(_) => depth -= 1,
            }
        }
    }
    Some(true)
}

fn collect_scalar_macro_params(el: &Element, out: &mut Vec<(String, TextRange)>) {
    let Some(node) = el.as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let mut skip_next = false;
    let mut in_key_or_opt = false;
    for item in elements(node) {
        if let Some(sym) = symbol(&item) {
            if sym.name == "&AUX" {
                break;
            }
            if matches!(
                sym.name.as_str(),
                "&BODY" | "&REST" | "&WHOLE" | "&ENVIRONMENT"
            ) {
                skip_next = true;
                continue;
            }
            if sym.name.starts_with('&') {
                in_key_or_opt = matches!(sym.name.as_str(), "&OPTIONAL" | "&KEY");
                continue;
            }
            if skip_next {
                skip_next = false;
                continue;
            }
            if !sym.name.starts_with('_') {
                out.push((sym.name, item.text_range()));
            }
        } else {
            skip_next = false;
            if let Some(inner) = item.as_node().filter(|n| closed_list(n)) {
                if in_key_or_opt {
                    if let Some(first) = elements(inner).next() {
                        if let Some(sym) = symbol(&first) {
                            if !sym.name.starts_with('_') {
                                out.push((sym.name, first.text_range()));
                            }
                        } else if let Some(pair) = first.as_node().filter(|n| closed_list(n))
                            && let Some(var) = elements(pair).nth(1)
                            && let Some(sym) = symbol(&var)
                            && !sym.name.starts_with('_')
                        {
                            out.push((sym.name, var.text_range()));
                        }
                    }
                } else {
                    collect_scalar_macro_params(&item, out);
                }
            }
        }
    }
}

struct ExpansionAudit<'a, 'doc, 'emit, C> {
    ctx: &'a LintDocument<'doc>,
    package: Option<&'a str>,
    scalar_params: &'a [(String, TextRange)],
    emit: &'a mut LintEmitter<'emit>,
    cancelled: &'a C,
}

impl<C: Fn() -> bool> ExpansionAudit<'_, '_, '_, C> {
    fn sequence(
        &mut self,
        forms: impl IntoIterator<Item = Element>,
        mut bindings: ExpansionBindings,
    ) -> Option<()> {
        for form in forms {
            self.inspect(&form, &bindings)?;
            if self.may_assign(&form) {
                bindings.clear();
            }
        }
        Some(())
    }

    fn may_assign(&self, form: &Element) -> bool {
        form.as_node().is_some_and(|node| {
            unwrap_quote(form, self.package, self.ctx.workspace).is_none()
                && !elements(node)
                    .next()
                    .and_then(|el| symbol(&el))
                    .is_some_and(|name| {
                        matches!(
                            self.ctx.workspace.standard_symbol(&name, self.package),
                            Some("DECLARE" | "GENSYM" | "MAKE-SYMBOL")
                        )
                    })
        })
    }

    fn inspect(&mut self, form: &Element, bindings: &ExpansionBindings) -> Option<()> {
        if (self.cancelled)() {
            return None;
        }
        let Some((form, true)) = possibly_read(form.clone(), self.ctx.source.known_features())
        else {
            return Some(());
        };
        let Some(node) = form.as_node() else {
            return Some(());
        };
        if unwrap_quote(&form, self.package, self.ctx.workspace).is_some() {
            return Some(());
        }
        if let Some(template) = reader_operand(&form, SyntaxKind::Backquote) {
            self.check_template(&template, bindings);
            return Some(());
        }
        if !closed_list(node) {
            return Some(());
        }
        let mut parts = elements(node);
        let Some(head) = parts.next().and_then(|head| symbol(&head)) else {
            return Some(());
        };
        let std = self.ctx.workspace.standard_symbol(&head, self.package);
        match std {
            Some("QUOTE" | "DECLARE" | "FUNCTION" | "SYMBOL-MACROLET") => return Some(()),
            Some("LET" | "LET*") => {
                let Some(specs) = parts
                    .next()
                    .and_then(Element::into_node)
                    .filter(closed_list)
                else {
                    return Some(());
                };
                let mut initial = bindings.clone();
                let sequential = std == Some("LET*");
                let mut parallel = Vec::new();
                let install = |bindings: &mut ExpansionBindings, name, slot| {
                    if let Some(slot) = slot {
                        bindings.insert(name, slot);
                    } else {
                        bindings.remove(&name);
                    }
                };
                for binding in elements(&specs) {
                    let Some((var, init)) = let_binding(&binding) else {
                        return Some(());
                    };
                    let Some(var) = bare_name(&var) else {
                        return Some(());
                    };
                    if let Some(init) = &init {
                        self.inspect(init, &initial)?;
                        if self.may_assign(init) {
                            initial.clear();
                        }
                    }
                    let slot = init
                        .as_ref()
                        .and_then(bare_name)
                        .and_then(|name| initial.get(&name).copied());
                    if sequential {
                        install(&mut initial, var, slot);
                    } else {
                        parallel.push((var, slot));
                    }
                }
                for (var, slot) in parallel {
                    install(&mut initial, var, slot);
                }
                return self.sequence(parts, initial);
            }
            Some("DESTRUCTURING-BIND" | "MULTIPLE-VALUE-BIND" | "LAMBDA") => {
                let Some(params) = parts.next() else {
                    return Some(());
                };
                let mut local = bindings.clone();
                if std != Some("LAMBDA")
                    && let Some(init) = parts.next()
                {
                    self.inspect(&init, bindings)?;
                    if self.may_assign(&init) {
                        local.clear();
                    }
                }
                let mut names = Vec::new();
                collect_scalar_macro_params(&params, &mut names);
                for (name, _) in names {
                    local.remove(&name);
                }
                return self.sequence(parts, local);
            }
            Some("FLET" | "LABELS" | "MACROLET") => {
                // Nested definitions have their own parameters and expansion environment.
                parts.next();
                return self.sequence(parts, bindings.clone());
            }
            Some("PROGN" | "LOCALLY") => return self.sequence(parts, bindings.clone()),
            _ => {}
        }
        if matches!(
            head.name.as_str(),
            "ONCE-ONLY" | "REBINDING" | "WITH-GENSYMS" | "WITH-UNIQUE-NAMES"
        ) && let Some(vars) = parts
            .next()
            .and_then(Element::into_node)
            .filter(closed_list)
        {
            let mut local = bindings.clone();
            for var in elements(&vars) {
                let name = bare_name(&var).or_else(|| bare_name(&elements(var.as_node()?).next()?));
                if let Some(name) = name {
                    local.remove(&name);
                }
            }
            return self.sequence(parts, local);
        }
        for child in parts {
            self.inspect(&child, bindings)?;
        }
        Some(())
    }

    fn check_template(&mut self, template: &Element, bindings: &ExpansionBindings) {
        let emit = &mut *self.emit;
        let mut counts = UnquoteCounts::default();
        let mut loop_uses = FxHashMap::default();
        MacroTemplateAnalysis {
            ctx: self.ctx,
            package: self.package,
            bindings,
            bound: Vec::new(),
            loop_depth: 0,
            loop_uses: &mut loop_uses,
            emit,
        }
        .inspect(template, &mut counts, true);
        for (slot, (param, definition)) in self.scalar_params.iter().enumerate() {
            if let Some(uses) = counts.get(&slot)
                    && let Some(second) = uses.second
                    && let Some(diag) = emit.diagnostic(
                        "suspicious/macro-multiple-evaluation",
                        second,
                        format!(
                            "the expression supplied for `{}` can be evaluated {} times in the expansion",
                            param.to_ascii_lowercase(),
                            uses.count
                        ),
                        Some("bind the value to a fresh temporary in the expansion, or use `once-only`".into()),
                        Vec::new(),
                        None,
                    )
                {
                    diag.note(uses.first, "another evaluated position on this expansion path");
                    diag.note(*definition, "macro parameter is introduced here");
                }
            if let Some(first) = loop_uses.get(&slot)
                    && let Some(diag) = emit.diagnostic("suspicious/macro-evaluation-in-loop", *first,
                        format!("macro parameter `{}` may be evaluated once per generated iteration", param.to_ascii_lowercase()),
                        Some("evaluate the argument before the loop and bind its value to a fresh temporary if it should run once".into()), Vec::new(), None)
                {
                    diag.note(*definition, "the caller supplies this expression");
                }
        }
    }
}

fn bare_name(el: &Element) -> Option<String> {
    symbol(el)
        .filter(|name| name.prefix == Prefix::None)
        .map(|name| name.name)
}

#[derive(Clone, Copy)]
struct UnquoteTrace {
    count: usize,
    first: TextRange,
    second: Option<TextRange>,
}

type UnquoteCounts = FxHashMap<usize, UnquoteTrace>;

struct MacroTemplateAnalysis<'a, 'src, 'emit> {
    ctx: &'a LintDocument<'src>,
    package: Option<&'a str>,
    bindings: &'a ExpansionBindings,
    bound: Vec<usize>,
    loop_depth: usize,
    loop_uses: &'a mut FxHashMap<usize, TextRange>,
    emit: &'a mut LintEmitter<'emit>,
}

impl MacroTemplateAnalysis<'_, '_, '_> {
    fn standard_symbol<'a>(&self, sym: &'a SymbolName) -> Option<&'a str> {
        self.ctx.workspace.standard_symbol(sym, self.package)
    }

    fn inspect(&mut self, el: &Element, counts: &mut UnquoteCounts, is_form: bool) {
        let checkpoint = self.bound.len();
        self.inspect_form(el, counts, is_form);
        self.bound.truncate(checkpoint);
    }

    fn binding_slot(&self, name: &Element) -> Option<usize> {
        let name = bare_name(&reader_operand(name, SyntaxKind::Unquote)?)?;
        self.bindings.get(&name).copied()
    }

    fn bind_variable(&mut self, name: &Element, check_capture: bool) {
        if check_capture {
            check_template_bound_var(name, self.emit);
        }
        self.bound.extend(self.binding_slot(name));
    }

    fn inspect_let_binding(
        &mut self,
        binding: &Element,
        counts: &mut UnquoteCounts,
        check_capture: bool,
    ) -> Option<usize> {
        let (name, init) = let_binding(binding)?;
        if check_capture {
            check_template_bound_var(&name, self.emit);
        }
        self.inspect_elements(init, counts, true);
        // A whole binding may interpolate a (name initializer) pair.
        binding.as_node().filter(|node| closed_list(node))?;
        self.binding_slot(&name)
    }

    fn inspect_form(&mut self, el: &Element, counts: &mut UnquoteCounts, is_form: bool) {
        let Some((el, true)) =
            crate::navigation::index::possibly_read(el.clone(), self.ctx.source.known_features())
        else {
            return;
        };
        let Some(node) = el.as_node() else {
            return;
        };
        if node.kind() == SyntaxKind::ReaderForm {
            match node.first_token().map(|t| t.kind()) {
                Some(SyntaxKind::Quote | SyntaxKind::Backquote) => return,
                Some(SyntaxKind::FunctionQuote) => {
                    self.inspect_function(elements(node).nth(1), counts);
                    return;
                }
                Some(
                    SyntaxKind::Unquote
                    | SyntaxKind::UnquoteSplicing
                    | SyntaxKind::UnquoteDestructive,
                ) => {
                    if let Some(expr) = elements(node).nth(1)
                        && let Some(slot) =
                            bare_name(&expr).and_then(|name| self.bindings.get(&name).copied())
                        && !self.bound.contains(&slot)
                    {
                        let range = expr.text_range();
                        if self.loop_depth != 0 {
                            self.loop_uses.entry(slot).or_insert(range);
                        }
                        counts
                            .entry(slot)
                            .and_modify(|trace| {
                                trace.count += 1;
                                trace.second.get_or_insert(range);
                            })
                            .or_insert(UnquoteTrace {
                                count: 1,
                                first: range,
                                second: None,
                            });
                    }
                    return;
                }
                _ => {}
            }
        }

        if node.kind() != SyntaxKind::List || !closed_list(node) {
            self.inspect_elements(elements(node), counts, false);
            return;
        }
        let mut parts = elements(node).peekable();
        let Some(head) = parts.next() else {
            return;
        };
        let head_symbol = symbol(&head);
        let std_op = head_symbol.as_ref().and_then(|sym| {
            self.standard_symbol(sym).or_else(|| {
                self.emit
                    .config
                    .macro_shape_for(self.ctx.workspace, sym, self.package)
                    .map(|shape| match shape {
                        MacroShape::Let => "LET",
                        MacroShape::Defun => "DEFUN",
                        MacroShape::Defmacro => "DEFMACRO",
                        MacroShape::When => "WHEN",
                        MacroShape::Unless => "UNLESS",
                        MacroShape::Dolist => "DOLIST",
                        MacroShape::Progn => "PROGN",
                        MacroShape::Ignore => "QUOTE",
                    })
            })
        });
        if std_op.is_none()
            && let Some(head) = &head_symbol
            && self
                .ctx
                .workspace
                .lookup_unique_declaration_parts(
                    Namespace::Function,
                    head,
                    self.package.or(Some("COMMON-LISP-USER")),
                )
                .is_some_and(|(_, definition)| definition.macro_definition.is_some())
        {
            return;
        }
        match std_op {
            Some("QUOTE" | "DECLARE") => return,
            Some("FUNCTION") => {
                self.inspect_function(parts.next(), counts);
                return;
            }
            Some("IF") => {
                if let Some(cond) = parts.next() {
                    self.inspect(&cond, counts, true);
                }
                if let Some(then) = parts.next() {
                    let mut else_counts = counts.clone();
                    self.inspect(&then, counts, true);
                    if let Some(otherwise) = parts.next() {
                        self.inspect(&otherwise, &mut else_counts, true);
                    }
                    // THEN and ELSE are exclusive: keep the longer trace for each parameter.
                    merge_longest_path(counts, else_counts);
                }
                return;
            }
            Some("COND") => {
                // Failure falls through to the next test. A successful
                // branch's body and any later clauses are mutually exclusive.
                let mut fallthrough = counts.clone();
                for clause in parts {
                    let Some(node) = clause.as_node().filter(|node| closed_list(node)) else {
                        continue;
                    };
                    let mut fields = elements(node);
                    let Some(test) = fields.next() else { continue };
                    self.inspect(&test, &mut fallthrough, true);
                    if is_nil_element(&test, self.package, self.ctx.workspace) {
                        continue;
                    }
                    let mut success = fallthrough.clone();
                    self.inspect_elements(fields, &mut success, true);
                    merge_longest_path(counts, success);
                    if is_t_element(&test, self.package, self.ctx.workspace) {
                        break;
                    }
                }
                return;
            }
            Some("CASE" | "ECASE" | "CCASE" | "TYPECASE" | "ETYPECASE" | "CTYPECASE") => {
                if let Some(scrutinee) = parts.next() {
                    self.inspect(&scrutinee, counts, true);
                }
                let before_clauses = counts.clone();
                for clause in parts {
                    if let Some(node) = clause.as_node().filter(|node| closed_list(node)) {
                        let mut branch = before_clauses.clone();
                        self.inspect_elements(elements(node).skip(1), &mut branch, true);
                        merge_longest_path(counts, branch);
                    }
                }
                return;
            }
            Some("LET" | "LET*" | "DOLIST" | "DOTIMES") => {
                if let Some(bindings) = parts
                    .next()
                    .and_then(Element::into_node)
                    .filter(closed_list)
                {
                    let body: Vec<_> = parts.collect();
                    let body_has_unquote = body.iter().any(element_contains_runtime_unquote);
                    if std_op == Some("LET*") {
                        let binding_items: Vec<_> = elements(&bindings).collect();
                        let mut any_later_unquote = body_has_unquote;
                        // Compute suffix unquote flags from right to left so LET*
                        // variable capture in subsequent initializers is also caught.
                        let mut check_flags = vec![false; binding_items.len()];
                        for i in (0..binding_items.len()).rev() {
                            check_flags[i] = any_later_unquote;
                            if element_contains_runtime_unquote(&binding_items[i]) {
                                any_later_unquote = true;
                            }
                        }
                        for (binding, check_capture) in binding_items.into_iter().zip(check_flags) {
                            if let Some(slot) =
                                self.inspect_let_binding(&binding, counts, check_capture)
                            {
                                self.bound.push(slot);
                            }
                        }
                    } else if std_op == Some("LET") {
                        let slots: Vec<_> = elements(&bindings)
                            .filter_map(|binding| {
                                self.inspect_let_binding(&binding, counts, body_has_unquote)
                            })
                            .collect();
                        self.bound.extend(slots);
                    } else {
                        let mut slots = elements(&bindings);
                        let name = slots.next();
                        if let Some(name) = &name
                            && body_has_unquote
                        {
                            check_template_bound_var(name, self.emit);
                        }
                        self.inspect_elements(slots.next(), counts, true);
                        if let Some(slot) = name.as_ref().and_then(|name| self.binding_slot(name)) {
                            self.bound.push(slot);
                        }
                        self.inspect_elements(slots.next(), counts, true);
                    }
                    let is_loop = matches!(std_op, Some("DOLIST" | "DOTIMES"));
                    self.loop_depth += usize::from(is_loop);
                    self.inspect_elements(body, counts, true);
                    self.loop_depth -= usize::from(is_loop);
                } else {
                    self.inspect_elements(parts, counts, true);
                }
                return;
            }
            Some("HANDLER-CASE") => {
                if let Some(expr) = parts.next() {
                    self.inspect(&expr, counts, true);
                }
                let before_clauses = counts.clone();
                for clause in parts {
                    if let Some(node) = clause.as_node().filter(|n| closed_list(n)) {
                        let mut fields = elements(node);
                        let Some(ty) = fields.next() else { continue };
                        let var_list = fields.next();
                        let body: Vec<_> = fields.collect();
                        if symbol(&ty)
                            .is_some_and(|s| s.prefix == Prefix::Keyword && s.name == "NO-ERROR")
                            && let Some(params) = var_list
                                .clone()
                                .and_then(Element::into_node)
                                .filter(closed_list)
                        {
                            let check_capture = body.iter().any(element_contains_runtime_unquote);
                            let mut branch = before_clauses.clone();
                            let checkpoint = self.bound.len();
                            self.inspect_lambda_list(&params, &mut branch, false, check_capture);
                            self.inspect_elements(body, &mut branch, true);
                            self.bound.truncate(checkpoint);
                            merge_longest_path(counts, branch);
                        } else {
                            if let Some(vars) =
                                var_list.and_then(Element::into_node).filter(closed_list)
                                && body.iter().any(element_contains_runtime_unquote)
                            {
                                for var in elements(&vars) {
                                    check_template_bound_var(&var, self.emit);
                                }
                            }
                            let mut branch = before_clauses.clone();
                            self.inspect_elements(body, &mut branch, true);
                            merge_longest_path(counts, branch);
                        }
                    }
                }
                return;
            }
            Some("RESTART-CASE") => {
                if let Some(expr) = parts.next() {
                    self.inspect(&expr, counts, true);
                }
                let before_clauses = counts.clone();
                for clause in parts {
                    if let Some(node) = clause.as_node().filter(|n| closed_list(n)) {
                        let fields: Vec<_> = elements(node).collect();
                        if fields.len() < 2 {
                            continue;
                        }
                        let mut branch = before_clauses.clone();
                        let mut idx = 2usize;
                        while idx + 1 < fields.len()
                            && crate::navigation::scope::restart_case_option(&fields[idx])
                        {
                            if symbol(&fields[idx + 1]).is_none() {
                                self.inspect(&fields[idx + 1], &mut branch, true);
                            }
                            idx += 2;
                        }
                        let body = &fields[idx..];
                        let checkpoint = self.bound.len();
                        if let Some(params) = fields[1].as_node().filter(|n| closed_list(n)) {
                            let check_capture = body.iter().any(element_contains_runtime_unquote);
                            self.inspect_lambda_list(params, &mut branch, false, check_capture);
                        }
                        self.inspect_elements(body.iter().cloned(), &mut branch, true);
                        self.bound.truncate(checkpoint);
                        merge_longest_path(counts, branch);
                    }
                }
                return;
            }
            Some("FLET" | "LABELS" | "MACROLET") => {
                if let Some(defs) = parts
                    .next()
                    .and_then(Element::into_node)
                    .filter(closed_list)
                {
                    let body: Vec<_> = parts.collect();
                    let check_capture = body.iter().any(element_contains_spliced_unquote);
                    for def in elements(&defs) {
                        let Some(def_node) = def.as_node().filter(|n| closed_list(n)) else {
                            continue;
                        };
                        let mut dparts = elements(def_node);
                        if let Some(fn_name) = dparts.next()
                            && check_capture
                        {
                            check_template_bound_var(&fn_name, self.emit);
                        }
                        if let Some(params) = dparts.next() {
                            let fn_body: Vec<_> = dparts.collect();
                            let checkpoint = self.bound.len();
                            if std_op != Some("MACROLET")
                                && let Some(params_node) =
                                    params.as_node().filter(|n| closed_list(n))
                            {
                                let fn_capture =
                                    fn_body.iter().any(element_contains_runtime_unquote);
                                self.inspect_lambda_list(params_node, counts, false, fn_capture);
                            }
                            self.inspect_elements(fn_body, counts, true);
                            self.bound.truncate(checkpoint);
                        }
                    }
                    self.inspect_elements(body, counts, true);
                } else {
                    self.inspect_elements(parts, counts, true);
                }
                return;
            }
            Some("LOOP") => {
                let items: Vec<_> = parts.collect();
                let simple = items.iter().all(|el| el.as_node().is_some());
                if simple {
                    self.loop_depth += 1;
                    self.inspect_elements(items, counts, true);
                    self.loop_depth -= 1;
                } else {
                    let mut per_iteration = false;
                    let mut skip_non_expr = false;
                    let mut in_binding_header = false;
                    for item in items {
                        if let Some(kw) = symbol(&item) {
                            match kw.name.as_str() {
                                "FOR" | "AS" | "WITH" => {
                                    per_iteration = false;
                                    skip_non_expr = true;
                                    in_binding_header = true;
                                }
                                "AND" if in_binding_header => {
                                    per_iteration = false;
                                    skip_non_expr = true;
                                }
                                "OF-TYPE" | "USING" | "NAMED" | "INTO" => {
                                    skip_non_expr = true;
                                }
                                "INITIALLY" | "FINALLY" | "REPEAT" | "FROM" | "UPFROM"
                                | "DOWNFROM" | "TO" | "UPTO" | "DOWNTO" | "BELOW" | "ABOVE"
                                | "BY" | "IN" | "ON" | "ACROSS" | "BEING" => {
                                    per_iteration = false;
                                    skip_non_expr = false;
                                }
                                "=" | "THEN" | "WHILE" | "UNTIL" | "DO" | "DOING" | "COLLECT"
                                | "COLLECTING" | "APPEND" | "APPENDING" | "NCONC" | "NCONCING"
                                | "COUNT" | "COUNTING" | "SUM" | "SUMMING" | "MAXIMIZE"
                                | "MAXIMIZING" | "MINIMIZE" | "MINIMIZING" | "ALWAYS" | "NEVER"
                                | "THEREIS" | "WHEN" | "UNLESS" | "IF" | "RETURN" => {
                                    per_iteration = true;
                                    skip_non_expr = false;
                                    if !matches!(kw.name.as_str(), "=" | "THEN") {
                                        in_binding_header = false;
                                    }
                                }
                                _ => {
                                    skip_non_expr = false;
                                }
                            }
                            continue;
                        }
                        if std::mem::take(&mut skip_non_expr) {
                            continue;
                        }
                        if per_iteration {
                            self.loop_depth += 1;
                            self.inspect(&item, counts, true);
                            self.loop_depth -= 1;
                        } else {
                            self.inspect(&item, counts, true);
                        }
                    }
                }
                return;
            }
            Some(
                "WITH-OPEN-FILE"
                | "WITH-OPEN-STREAM"
                | "WITH-INPUT-FROM-STRING"
                | "WITH-OUTPUT-TO-STRING",
            ) => {
                if let Some(spec) = parts
                    .next()
                    .and_then(Element::into_node)
                    .filter(closed_list)
                {
                    let mut fields = elements(&spec);
                    let name = fields.next();
                    if let Some(name) = &name
                        && parts
                            .clone()
                            .any(|form| element_contains_runtime_unquote(&form))
                    {
                        check_template_bound_var(name, self.emit);
                    }
                    if std_op == Some("WITH-INPUT-FROM-STRING") {
                        self.inspect_elements(fields.next(), counts, true);
                        while let (Some(key), Some(operand)) = (fields.next(), fields.next()) {
                            if symbol(&key).is_some_and(|key| {
                                key.prefix == Prefix::Keyword
                                    && matches!(key.name.as_str(), "START" | "END")
                            }) {
                                self.inspect(&operand, counts, true);
                            }
                        }
                    } else {
                        self.inspect_elements(fields, counts, true);
                    }
                    if let Some(slot) = name.as_ref().and_then(|name| self.binding_slot(name)) {
                        self.bound.push(slot);
                    }
                }
                self.inspect_elements(parts, counts, true);
                return;
            }
            Some("MULTIPLE-VALUE-BIND") => {
                let vars_el = parts.next();
                let expr_el = parts.next();
                let body: Vec<_> = parts.collect();
                self.inspect_elements(expr_el, counts, true);
                if let Some(vars_node) = vars_el.and_then(Element::into_node).filter(closed_list) {
                    let check_capture = body.iter().any(element_contains_runtime_unquote);
                    for var in elements(&vars_node) {
                        self.bind_variable(&var, check_capture);
                    }
                }
                self.inspect_elements(body, counts, true);
                return;
            }
            Some("DESTRUCTURING-BIND") => {
                let lambda_el = parts.next();
                let expr_el = parts.next();
                let body: Vec<_> = parts.collect();
                if let Some(expr) = expr_el {
                    self.inspect(&expr, counts, true);
                }
                if let Some(params) = lambda_el.and_then(Element::into_node).filter(closed_list) {
                    let check_capture = body.iter().any(element_contains_runtime_unquote);
                    self.inspect_lambda_list(&params, counts, false, check_capture);
                }
                self.inspect_elements(body, counts, true);
                return;
            }
            Some("THE" | "CHECK-TYPE" | "BLOCK" | "RETURN-FROM" | "LOAD-TIME-VALUE") => {
                match std_op {
                    Some("THE" | "BLOCK" | "RETURN-FROM") => {
                        self.inspect_elements(parts.skip(1), counts, true)
                    }
                    _ => self.inspect_elements(parts.take(1), counts, true),
                }
                return;
            }
            Some("LAMBDA" | "DEFUN" | "DEFMETHOD") => {
                let is_lambda = std_op == Some("LAMBDA");
                let is_method = std_op == Some("DEFMETHOD");
                if !is_lambda {
                    parts.next(); // Definition name, not a value expression.
                }
                if is_method {
                    // Stop before the specialized lambda list (including NIL). Interpolated
                    // qualifiers and whole lists have no statically known boundary.
                    while parts.peek().is_some_and(|part| {
                        part.as_token().is_some()
                            && !is_nil_element(part, self.package, self.ctx.workspace)
                    }) {
                        parts.next();
                    }
                    if parts.peek().is_some_and(|part| {
                        !part.as_node().is_some_and(closed_list)
                            && !is_nil_element(part, self.package, self.ctx.workspace)
                    }) {
                        self.inspect_elements(parts, counts, false);
                        return;
                    }
                }
                if let Some(params) = parts.next()
                    && let Some(params) = params.as_node().filter(|n| closed_list(n))
                {
                    let check_capture = is_lambda
                        && parts
                            .clone()
                            .any(|el| element_contains_runtime_unquote(&el));
                    self.inspect_lambda_list(params, counts, is_method, check_capture);
                }
                self.inspect_elements(parts, counts, true);
                return;
            }
            _ => {}
        }

        // A single interpolation at a compound form's head supplies an operator,
        // not a value expression. Splices can also insert operands, so keep them.
        let interpolated_operator = is_form
            && head.as_node().is_some_and(|head| {
                head.kind() == SyntaxKind::ReaderForm
                    && head
                        .first_token()
                        .is_some_and(|t| t.kind() == SyntaxKind::Unquote)
            });
        if !interpolated_operator {
            self.inspect(&head, counts, false);
        }
        // The operator of a nested unknown shape may belong to its data.
        self.inspect_elements(parts, counts, std_op.is_some_and(evaluates_all_arguments));
    }

    fn inspect_elements(
        &mut self,
        elements: impl IntoIterator<Item = Element>,
        counts: &mut UnquoteCounts,
        are_forms: bool,
    ) {
        for el in elements {
            self.inspect(&el, counts, are_forms);
        }
    }

    fn inspect_function(&mut self, target: Option<Element>, counts: &mut UnquoteCounts) {
        if let Some(target) = target
            && let Some(list) = target.as_node().filter(|n| closed_list(n))
            && let Some(head) = elements(list).next().and_then(|el| symbol(&el))
            && self.standard_symbol(&head) == Some("LAMBDA")
        {
            self.inspect(&target, counts, true);
        }
    }

    fn inspect_lambda_list(
        &mut self,
        params: &SyntaxNode,
        counts: &mut UnquoteCounts,
        specialized: bool,
        check_capture: bool,
    ) {
        let mut required = true;
        let mut has_initforms = false;
        let mut specialized_slots = Vec::new();
        for param in elements(params) {
            if let Some(sym) = symbol(&param)
                && sym.name.starts_with('&')
            {
                required = false;
                has_initforms = matches!(
                    self.standard_symbol(&sym),
                    Some("&OPTIONAL" | "&KEY" | "&AUX")
                );
                self.bound.append(&mut specialized_slots);
                continue;
            }
            let Some(spec) = param.as_node().filter(|n| closed_list(n)) else {
                if specialized && required {
                    if check_capture {
                        check_template_bound_var(&param, self.emit);
                    }
                    specialized_slots.extend(self.binding_slot(&param));
                } else {
                    self.bind_variable(&param, check_capture);
                }
                continue;
            };
            let mut fields = elements(spec);
            let Some(name) = fields.next() else { continue };
            if has_initforms {
                self.inspect_elements(fields.next(), counts, true);
                let explicit = name
                    .as_node()
                    .filter(|node| closed_list(node))
                    .and_then(|pair| elements(pair).nth(1));
                self.bind_variable(explicit.as_ref().unwrap_or(&name), check_capture);
                if let Some(supplied) = fields.next() {
                    self.bind_variable(&supplied, check_capture);
                }
            } else if specialized && required {
                if let Some(specializer) = fields
                    .next()
                    .and_then(Element::into_node)
                    .filter(closed_list)
                {
                    let mut slots = elements(&specializer);
                    if slots
                        .next()
                        .and_then(|el| symbol(&el))
                        .is_some_and(|op| self.standard_symbol(&op) == Some("EQL"))
                    {
                        // An EQL specializer runs before the method parameters are bound.
                        self.inspect_elements(slots.next(), counts, true);
                    }
                }
                if check_capture {
                    check_template_bound_var(&name, self.emit);
                }
                specialized_slots.extend(self.binding_slot(&name));
            }
        }
        self.bound.extend(specialized_slots);
    }
}

fn merge_longest_path(counts: &mut UnquoteCounts, alternative: UnquoteCounts) {
    for (name, uses) in alternative {
        if uses.count > counts.get(&name).map_or(0, |trace| trace.count) {
            counts.insert(name, uses);
        }
    }
}

fn check_template_bound_var(var_el: &Element, emit: &mut LintEmitter<'_>) {
    // Only flag plain interned symbols (`Prefix::None`, non-earmuff, non-`_`).
    let Some(sym) = symbol(var_el) else {
        return;
    };
    if !matches!(sym.prefix, Prefix::None)
        || sym.name.starts_with('&')
        || sym.name.starts_with('_')
        || (sym.name.starts_with('*') && sym.name.ends_with('*'))
        || sym.name == "IT"
    {
        return;
    }
    emit.emit(
        "suspicious/macro-unhygienic-binding",
        var_el.text_range(),
        format!(
            "macro template binds plain symbol `{}` without `gensym`, risking variable capture",
            sym.name.to_ascii_lowercase()
        ),
        Some("generate a fresh symbol with `(gensym ...)` or `with-gensyms`".into()),
        Vec::new(),
        None,
    );
}

fn element_contains_runtime_unquote(el: &Element) -> bool {
    let Some(node) = el.as_node() else {
        return false;
    };
    if node.kind() == SyntaxKind::ReaderForm {
        if node
            .first_token()
            .is_some_and(|t| matches!(t.kind(), SyntaxKind::Quote | SyntaxKind::FunctionQuote))
        {
            return false;
        }
        if node.first_token().is_some_and(|t| {
            matches!(
                t.kind(),
                SyntaxKind::Unquote | SyntaxKind::UnquoteSplicing | SyntaxKind::UnquoteDestructive
            )
        }) {
            return true;
        }
    }
    elements(node).any(|child| element_contains_runtime_unquote(&child))
}

fn element_contains_spliced_unquote(el: &Element) -> bool {
    let Some(node) = el.as_node() else {
        return false;
    };
    if node.kind() == SyntaxKind::ReaderForm {
        if node
            .first_token()
            .is_some_and(|t| matches!(t.kind(), SyntaxKind::Quote | SyntaxKind::FunctionQuote))
        {
            return false;
        }
        if node.first_token().is_some_and(|t| {
            matches!(
                t.kind(),
                SyntaxKind::UnquoteSplicing | SyntaxKind::UnquoteDestructive
            )
        }) {
            return true;
        }
    }
    elements(node).any(|child| element_contains_spliced_unquote(&child))
}
