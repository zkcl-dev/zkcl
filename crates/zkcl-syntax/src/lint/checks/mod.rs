//! CST lint driver. Pending forms carry their lexical context in a work stack.

use std::borrow::Cow;
use std::rc::Rc;

use rustc_hash::FxHashMap;

mod call_arguments;
mod clos;
mod collect;
mod complexity;
mod correctness;
mod dataflow;
mod foreign;
mod format_control;
mod grammar;
mod indexed_loop;
mod literal_arguments;
mod macro_calls;
mod macro_templates;
mod names;
mod perf_security;
mod sequence;
mod standard_forms;
mod style_pedantic;
mod suspicious;
mod values;

use crate::ast::FeatureConditional;
use crate::format::TextEdit;
use crate::navigation::ScopeBinding;
use crate::navigation::index::{
    closed_list, find_top_level_form_near, non_dotted_list, possibly_read, symbol,
};
use crate::navigation::queries::{candidate_symbol_tokens, collect_scope_with, find_local_fn};
use crate::navigation::scope::ScopeAtOffset;
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

use super::MacroShape;
use super::context::{FormContext, LintDocument, LintEmitter, is_nil_element};
use grammar::LambdaListKind;

#[derive(Clone, Copy)]
pub(in crate::lint::checks) struct ScopeContext<'a, 'doc> {
    pub(super) doc: &'a LintDocument<'doc>,
    pub(super) package: Option<&'a str>,
    pub(super) lexicals: &'a [ScopeBinding],
    pub(super) env: &'a FormContext,
}

impl ScopeContext<'_, '_> {
    fn known_package(self, range: TextRange) -> bool {
        match self.package {
            Some(package) => self.doc.workspace.can_resolve_package(package),
            None => self.doc.source.package_known_at(range.start()),
        }
    }

    fn standard_call(self, list: &SyntaxNode) -> Option<(String, Vec<Element>)> {
        let (_, args, op) = self
            .doc
            .resolve_head(list, self.package, self.lexicals, self.env)?;
        Some((op?, args))
    }

    fn standard_spelling(self, name: &'static str) -> Option<Cow<'static, str>> {
        if self
            .doc
            .unqualified_standard(name, self.package, self.lexicals, self.env)
        {
            return Some(Cow::Borrowed(name));
        }
        let qualified = SymbolName {
            prefix: Prefix::Package {
                name: "COMMON-LISP".into(),
                external: true,
            },
            name: name.to_ascii_uppercase(),
        };
        self.doc
            .std_op(&qualified, self.package, self.lexicals, self.env)?;
        Some(Cow::Owned(format!("common-lisp:{name}")))
    }

    fn continuation(self, list: &SyntaxNode, indent: usize) -> String {
        let start = usize::from(list.text_range().start());
        let column = self.doc.text[..start]
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count();
        let newline = if self.doc.text_of(list.text_range()).contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        format!("{newline}{:width$}", "", width = column.min(80) + indent)
    }
}

fn replace_around(
    outer: &SyntaxNode,
    retained: TextRange,
    mut prefix: String,
    mut suffix: String,
) -> Vec<TextEdit> {
    let needs_separator = |kind| {
        !matches!(
            kind,
            SyntaxKind::Whitespace | SyntaxKind::OpenParen | SyntaxKind::CloseParen
        )
    };
    // A neighboring atom may contain escaped delimiters or whitespace.
    if prefix.is_empty()
        && outer
            .first_token()
            .and_then(|token| token.prev_token())
            .is_some_and(|token| needs_separator(token.kind()))
    {
        prefix.push(' ');
    }
    if suffix.is_empty()
        && outer
            .last_token()
            .and_then(|token| token.next_token())
            .is_some_and(|token| needs_separator(token.kind()))
    {
        suffix.push(' ');
    }
    let outer = outer.text_range();
    vec![
        TextEdit {
            range: TextRange::new(outer.start(), retained.start()),
            replacement: prefix,
        },
        TextEdit {
            range: TextRange::new(retained.end(), outer.end()),
            replacement: suffix,
        },
    ]
}

#[derive(Clone, Copy)]
pub(in crate::lint::checks) struct CallSite<'a> {
    pub(super) list: &'a SyntaxNode,
    pub(super) head: &'a SymbolName,
    pub(super) args: &'a [Element],
    pub(super) std_op: Option<&'a str>,
}

pub(in crate::lint::checks) struct WalkContext<'a, 'doc, 'emit, C> {
    pub(super) doc: &'a LintDocument<'doc>,
    pub(super) package: Option<&'a str>,
    pub(super) lexicals: &'a [ScopeBinding],
    pub(super) emit: &'a mut LintEmitter<'emit>,
    pub(super) work: &'a mut Walk,
    pub(super) cancelled: &'a C,
}

impl<C: Fn() -> bool> WalkContext<'_, '_, '_, C> {
    pub(in crate::lint::checks) fn is_cancelled(&self) -> bool {
        (self.cancelled)()
    }

    pub(in crate::lint::checks) fn enqueue(&mut self, element: &Element, env: &FormContext) {
        self.work.enqueue(element, env);
    }

    pub(in crate::lint::checks) fn enqueue_place(&mut self, element: &Element, env: &FormContext) {
        self.work.enqueue_place(element, env);
    }

    pub(in crate::lint::checks) fn walk_body(
        &mut self,
        body: &[Element],
        env: &FormContext,
        kind: BodyKind,
    ) -> Option<()> {
        walk_body_sequence(self, body, env, kind)
    }

    pub(in crate::lint::checks) fn walk_lambda_initforms(
        &mut self,
        lambda_list: &Element,
        kind: LambdaListKind,
        env: &FormContext,
    ) -> Option<()> {
        walk_lambda_list_initforms(
            self.doc,
            lambda_list,
            kind,
            env,
            self.emit,
            self.work,
            self.cancelled,
        )
    }

    pub(in crate::lint::checks) fn next_fn_scope_id(&mut self) -> u32 {
        self.work.fn_scope_counter = self.work.fn_scope_counter.wrapping_add(1).max(1);
        self.work.fn_scope_counter
    }

    pub(in crate::lint::checks) fn walk_tagbody(
        &mut self,
        body: &[Element],
        env: &FormContext,
    ) -> Option<()> {
        walk_tagbody(
            self.doc,
            body,
            self.package,
            env,
            self.emit,
            self.work,
            self.cancelled,
        )
    }
}

pub(crate) fn run_all(
    ctx: &LintDocument<'_>,
    emit: &mut LintEmitter<'_>,
    cancelled: &impl Fn() -> bool,
    cache: Option<&mut super::FormLintCache>,
) -> Option<()> {
    let reader_checks = ReaderChecks {
        eval: emit.is_enabled("security/read-time-eval")
            && memchr::memmem::find(ctx.text.as_bytes(), b"#.").is_some(),
        comma: (emit.is_enabled("correctness/unquote-outside-backquote")
            || emit.is_enabled("correctness/invalid-unquote-splicing"))
            && memchr::memchr(b',', ctx.text.as_bytes()).is_some(),
    };
    if reader_checks.eval || reader_checks.comma {
        for form in elements(ctx.root) {
            scan_reader_forms(ctx, &form, 0, reader_checks, emit, cancelled)?;
        }
    }
    let env = FormContext::top_level();
    let mut work = Walk::default();
    let remaining = if let Some(cache) = cache {
        let mut isolated = Walk::default();
        cache.check(
            ctx,
            emit,
            |form, buffer| {
                walk_form(
                    ctx,
                    form.clone().into(),
                    &env,
                    buffer,
                    &mut isolated,
                    cancelled,
                )?;
                dataflow::check_form_bindings(ctx, form, &isolated.writes, buffer, cancelled)?;
                // These facts describe occurrences within this form only.
                isolated.writes.clear();
                ctx.macro_operands.borrow_mut().clear();
                Some(())
            },
            cancelled,
        )?
    } else {
        None
    };
    let forms = remaining
        .iter()
        .flatten()
        .filter(|part| crate::outline::is_element_kind(part.kind()))
        .cloned()
        .chain(
            remaining
                .is_none()
                .then(|| elements(ctx.root))
                .into_iter()
                .flatten(),
        );
    for form in forms {
        if cancelled() {
            return None;
        }
        if !ctx.source.reader_syntax_known_at(form.text_range().start()) {
            break;
        }
        walk_form(ctx, form, &env, emit, &mut work, cancelled)?;
    }
    dataflow::run_dataflow_checks(ctx, &work.writes, emit, cancelled, remaining.as_deref())?;
    Some(())
}

fn walk_form(
    ctx: &LintDocument<'_>,
    form: Element,
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    work.pending
        .push((form, env.clone(), ExpressionRole::Value));
    while let Some((element, env, role)) = work.pending.pop() {
        let siblings = work.pending.len();
        let first_diagnostic = emit.diagnostics.len();
        walk_element(ctx, &element, &env, role, emit, work, cancelled)?;
        if let Some(origin) = &env.macro_origin {
            macro_calls::add_origin(emit, first_diagnostic, origin);
        }
        // Earlier siblings are next to be popped.
        work.pending[siblings..].reverse();
    }
    Some(())
}

#[derive(Clone, Copy)]
struct ReaderChecks {
    eval: bool,
    comma: bool,
}

fn scan_reader_forms(
    ctx: &LintDocument<'_>,
    el: &Element,
    mut depth: usize,
    checks: ReaderChecks,
    emit: &mut LintEmitter<'_>,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    if cancelled() {
        return None;
    }
    let Some(node) = el.as_node() else {
        return Some(());
    };
    if let Some(feature) = FeatureConditional::from_node(node) {
        let active = feature.is_active(ctx.source.known_features());
        scan_reader_forms(
            ctx,
            &feature.feature.into_element(),
            depth,
            checks,
            emit,
            cancelled,
        )?;
        if active != Some(false) {
            scan_reader_forms(
                ctx,
                &feature.guarded.into_element(),
                depth,
                ReaderChecks {
                    comma: checks.comma && active == Some(true),
                    ..checks
                },
                emit,
                cancelled,
            )?;
        }
        return Some(());
    }
    if node.kind() == SyntaxKind::List && crate::syntax::list_has_dot(node) && checks.comma {
        let mut saw_dot = false;
        for child in elements(node) {
            if child.kind() == SyntaxKind::Dot {
                saw_dot = true;
                continue;
            }
            if saw_dot {
                saw_dot = false;
                if let Some(child_node) = child.as_node()
                    && child_node.kind() == SyntaxKind::ReaderForm
                    && child_node.first_token().is_some_and(|t| {
                        matches!(
                            t.kind(),
                            SyntaxKind::UnquoteSplicing | SyntaxKind::UnquoteDestructive
                        )
                    })
                    && ctx
                        .source
                        .reader_syntax_known_at(child_node.text_range().start())
                {
                    emit.emit(
                        "correctness/invalid-unquote-splicing",
                        child_node.text_range(),
                        "splicing unquote (`,@` / `,.`) cannot appear as a dotted list tail",
                        Some(
                            "use `,` for a dotted tail or remove the dot to splice into the list"
                                .into(),
                        ),
                        Vec::new(),
                        None,
                    );
                }
            }
        }
    }
    if node.kind() == SyntaxKind::ReaderForm
        && let Some(prefix) = node.first_token()
    {
        match prefix.kind() {
            SyntaxKind::Backquote => {
                if checks.comma
                    && let Some(operand) = elements(node).nth(1).and_then(Element::into_node)
                    && operand.kind() == SyntaxKind::ReaderForm
                    && operand.first_token().is_some_and(|t| {
                        matches!(
                            t.kind(),
                            SyntaxKind::UnquoteSplicing | SyntaxKind::UnquoteDestructive
                        )
                    })
                    && ctx
                        .source
                        .reader_syntax_known_at(operand.text_range().start())
                {
                    emit.emit(
                        "correctness/invalid-unquote-splicing",
                        operand.text_range(),
                        "splicing unquote (`,@` / `,.`) requires an enclosing list or vector in the backquote template",
                        None,
                        Vec::new(),
                        None,
                    );
                }
                depth += 1;
            }
            SyntaxKind::Quote | SyntaxKind::FunctionQuote => {
                if checks.comma
                    && let Some(operand) = elements(node).nth(1).and_then(Element::into_node)
                    && operand.kind() == SyntaxKind::ReaderForm
                    && operand.first_token().is_some_and(|t| {
                        matches!(
                            t.kind(),
                            SyntaxKind::UnquoteSplicing | SyntaxKind::UnquoteDestructive
                        )
                    })
                    && ctx
                        .source
                        .reader_syntax_known_at(operand.text_range().start())
                {
                    emit.emit(
                        "correctness/invalid-unquote-splicing",
                        operand.text_range(),
                        "splicing unquote (`,@` / `,.`) cannot appear directly under `'` or `#'`",
                        None,
                        Vec::new(),
                        None,
                    );
                }
            }
            SyntaxKind::Unquote | SyntaxKind::UnquoteSplicing | SyntaxKind::UnquoteDestructive => {
                if checks.comma
                    && depth == 0
                    && ctx
                        .source
                        .reader_syntax_known_at(prefix.text_range().start())
                {
                    emit.emit(
                        "correctness/unquote-outside-backquote",
                        prefix.text_range(),
                        "comma requires an enclosing backquote",
                        None,
                        Vec::new(),
                        None,
                    );
                }
                depth = depth.saturating_sub(1);
            }
            SyntaxKind::DispatchPrefix if checks.eval && prefix.text().starts_with("#.") => {
                emit.emit("security/read-time-eval", prefix.text_range(),
                    "`#.` can execute code while reading this form",
                    Some("use a load-time or runtime expression when the value need not be computed by the reader".into()),
                    Vec::new(), None);
            }
            _ => {}
        }
    }
    for child in node.children() {
        scan_reader_forms(ctx, &child.into(), depth, checks, emit, cancelled)?;
    }
    Some(())
}

#[derive(Default)]
struct Walk {
    pending: Vec<(Element, FormContext, ExpressionRole)>,
    // Reused scratch buffer for flattened list form elements during traversal.
    parts_buf: Vec<Element>,
    // Direct assignments that also expose their value count as uses.
    writes: FxHashMap<TextRange, bool>,
    fn_scope_counter: u32,
    cached_lexicals: Option<(u32, Rc<[ScopeBinding]>)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpressionRole {
    Value,
    Place,
}

impl Walk {
    fn enqueue(&mut self, element: &Element, env: &FormContext) {
        self.pending
            .push((element.clone(), env.clone(), ExpressionRole::Value));
    }

    fn enqueue_place(&mut self, element: &Element, env: &FormContext) {
        self.pending
            .push((element.clone(), env.clone(), ExpressionRole::Place));
    }
}

fn walk_element(
    ctx: &LintDocument<'_>,
    el: &Element,
    env: &FormContext,
    role: ExpressionRole,
    emit: &mut LintEmitter<'_>,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    if cancelled() {
        return None;
    }
    if !ctx.source.reader_syntax_known_at(el.text_range().start()) {
        return Some(());
    }
    let Some((el, known)) = possibly_read(el.clone(), ctx.source.known_features()) else {
        return Some(());
    };
    if !known {
        return Some(());
    }

    if role == ExpressionRole::Place {
        if let Some(list) = el.as_node().filter(|node| node.kind() == SyntaxKind::List) {
            walk_place(ctx, list, env, work);
        }
        return Some(());
    }

    match el {
        Element::Token(tok) => {
            if emit.is_enabled("style/double-colon-internal-access") && tok.text().contains(':') {
                let pkg = ctx.source.package_at(tok.text_range().start());
                style_pedantic::check_double_colon_token(ctx, &tok, pkg, emit);
            }
            Some(())
        }
        Element::Node(node) => match node.kind() {
            SyntaxKind::ReaderForm => walk_reader_form(ctx, &node, env, emit, work, cancelled),
            SyntaxKind::List => walk_list(ctx, &node, env, emit, work, cancelled),
            SyntaxKind::Vector => Some(()),
            _ => {
                for child in elements(&node) {
                    work.enqueue(&child, env);
                }
                Some(())
            }
        },
    }
}

fn walk_place(ctx: &LintDocument<'_>, list: &SyntaxNode, env: &FormContext, work: &mut Walk) {
    use crate::cl_builtins::{BuiltinKind, builtin_kind};

    let package = ctx.source.package_at(list.text_range().start());
    let lexicals = lint_scope(ctx, list, env, work);
    let Some((_, args, Some(op))) =
        ctx.resolve_head(list, package, lexicals.as_deref().unwrap_or_default(), env)
    else {
        // A user-defined place expander determines which operands are evaluated.
        return;
    };
    match (op.as_str(), args.as_slice()) {
        ("THE", [_, place]) => work.enqueue_place(place, env),
        ("VALUES", places) => {
            for place in places {
                work.enqueue_place(place, env);
            }
        }
        ("GETF", [place, remaining @ ..]) => {
            work.enqueue_place(place, env);
            for value in remaining {
                work.enqueue(value, env);
            }
        }
        ("LDB" | "MASK-FIELD", [specifier, place]) => {
            work.enqueue(specifier, env);
            work.enqueue_place(place, env);
        }
        (op, args) if builtin_kind(op) == Some(BuiltinKind::Function) => {
            for value in args {
                work.enqueue(value, env);
            }
        }
        _ => {}
    }
}

fn walk_lambda_list_initforms(
    ctx: &LintDocument<'_>,
    lambda_list: &Element,
    kind: LambdaListKind,
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    let init_env = env.nested_expr(false, false);
    for form in grammar::lambda_initforms(ctx, lambda_list, kind, emit, cancelled)? {
        work.enqueue(&form, &init_env);
    }
    Some(())
}

fn walk_reader_form(
    ctx: &LintDocument<'_>,
    node: &SyntaxNode,
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    let first_kind = node.first_token().map(|t| t.kind());
    match first_kind {
        Some(SyntaxKind::Quote) => Some(()),
        Some(SyntaxKind::FunctionQuote) => {
            let pkg = ctx.source.package_at(node.text_range().start());
            if let Some(target) = elements(node).nth(1) {
                if is_lambda_expression(ctx, &target) {
                    work.enqueue(&target, &env.nested_expr(false, false));
                    return Some(());
                }
                check_function_name(ctx, &target, pkg, emit);
            }
            let lexicals = lint_scope(ctx, node, env, work);
            correctness::check_function_quote_form(
                ctx,
                node,
                pkg,
                lexicals.as_deref().unwrap_or_default(),
                env,
                emit,
            );
            Some(())
        }
        Some(SyntaxKind::Backquote) => {
            for child in elements(node).skip(1) {
                walk_backquote_unquotes(ctx, &child, 1, env, work, cancelled)?;
            }
            Some(())
        }
        Some(
            SyntaxKind::Unquote
            | SyntaxKind::UnquoteSplicing
            | SyntaxKind::UnquoteDestructive
            | SyntaxKind::DispatchPrefix,
        ) => Some(()),
        _ => {
            for child in elements(node).skip(1) {
                work.enqueue(&child, env);
            }
            Some(())
        }
    }
}

fn walk_backquote_unquotes(
    ctx: &LintDocument<'_>,
    el: &Element,
    depth: usize,
    env: &FormContext,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    if cancelled() {
        return None;
    }
    let Some((el, true)) = possibly_read(el.clone(), ctx.source.known_features()) else {
        return Some(());
    };
    let Some(node) = el.as_node() else {
        return Some(());
    };
    let reader_form = node.kind() == SyntaxKind::ReaderForm;
    let depth = if reader_form {
        match node.first_token().map(|token| token.kind()) {
            Some(SyntaxKind::Backquote) => depth + 1,
            Some(
                SyntaxKind::Unquote | SyntaxKind::UnquoteSplicing | SyntaxKind::UnquoteDestructive,
            ) => depth - 1,
            Some(SyntaxKind::DispatchPrefix) => return Some(()),
            _ => depth,
        }
    } else {
        depth
    };
    for child in elements(node).skip(usize::from(reader_form)) {
        if depth == 0 {
            work.enqueue(&child, &env.nested_expr(false, false));
        } else {
            walk_backquote_unquotes(ctx, &child, depth, env, work, cancelled)?;
        }
    }
    Some(())
}

fn is_lambda_expression(ctx: &LintDocument<'_>, el: &Element) -> bool {
    let Some(node) = el.as_node().filter(|node| non_dotted_list(node)) else {
        return false;
    };
    let Some((head, true)) =
        elements(node).find_map(|el| possibly_read(el, ctx.source.known_features()))
    else {
        return false;
    };
    symbol(&head).is_some_and(|name| {
        ctx.workspace
            .standard_symbol(&name, ctx.source.package_at(el.text_range().start()))
            == Some("LAMBDA")
    })
}

fn check_function_name(
    ctx: &LintDocument<'_>,
    element: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if symbol(element).is_none()
        && ctx.setf_name(element, package).is_none()
        && element.kind() != SyntaxKind::ReaderForm
    {
        grammar::report(
            emit,
            "correctness/invalid-function-name",
            element,
            "expected a symbol or a `(setf name)` function name",
        );
    }
}

fn push_definition_block(
    ctx: &LintDocument<'_>,
    name: &Element,
    package: Option<&str>,
    env: &mut FormContext,
) {
    if let Some(target) = symbol(name)
        .or_else(|| ctx.setf_name(name, package))
        .and_then(|name| ctx.control_name(name, package))
    {
        env.push_block(target);
    }
}

fn is_declaration(ctx: &LintDocument<'_>, el: &Element, package: Option<&str>) -> bool {
    el.as_node()
        .filter(|node| closed_list(node))
        .is_some_and(|node| {
            elements(node)
                .next()
                .and_then(|el| symbol(&el))
                .is_some_and(|name| {
                    ctx.workspace.standard_symbol(&name, package) == Some("DECLARE")
                })
        })
}

fn body_after_declarations<'a>(
    ctx: &LintDocument<'_>,
    body: &'a [Element],
    package: Option<&str>,
) -> &'a [Element] {
    let count = body
        .iter()
        .take_while(|el| is_declaration(ctx, el, package))
        .count();
    &body[count..]
}

fn walk_tagbody(
    ctx: &LintDocument<'_>,
    body: &[Element],
    package: Option<&str>,
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    use rustc_hash::FxHashMap;
    let mut tags = FxHashMap::default();
    for stmt in body {
        if cancelled() {
            return None;
        }
        if let Some(tag) = ctx.control_tag(stmt, package) {
            match tags.entry(tag) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(stmt.text_range());
                }
                std::collections::hash_map::Entry::Occupied(entry) => {
                    if let Some(diag) = emit.diagnostic(
                        "correctness/duplicate-tagbody-tag",
                        stmt.text_range(),
                        "duplicate tag in the same tagbody",
                        None,
                        Vec::new(),
                        None,
                    ) {
                        diag.note(*entry.get(), "first tag definition is here");
                    }
                }
            }
        } else if stmt.as_token().is_some() && symbol(stmt).is_none() {
            grammar::report(
                emit,
                "correctness/invalid-control-target",
                stmt,
                "a tag must be a symbol or an integer; use a compound form for a statement",
            );
        }
    }
    let mut body_env = env.clone();
    body_env.in_tagbody = true;
    Rc::make_mut(&mut body_env.tagbody_tags).extend(tags.into_keys());
    for stmt in body.iter().filter(|stmt| stmt.as_node().is_some()) {
        work.enqueue(stmt, &body_env);
    }
    Some(())
}

fn clause_fields(
    ctx: &LintDocument<'_>,
    clause: &Element,
    package: Option<&str>,
    min: usize,
    emit: &mut LintEmitter<'_>,
) -> Option<Vec<Element>> {
    let fields = grammar::proper_list(
        ctx,
        clause,
        package,
        "correctness/malformed-clause",
        "expected a clause list",
        emit,
    )?;
    if fields.len() < min {
        grammar::report(
            emit,
            "correctness/malformed-clause",
            clause,
            if min == 1 {
                "the clause is missing its first field"
            } else {
                "the clause requires at least two fields"
            },
        );
        return None;
    }
    Some(fields)
}

fn lint_scope(
    ctx: &LintDocument<'_>,
    node: &SyntaxNode,
    env: &FormContext,
    work: &mut Walk,
) -> Option<Rc<[ScopeBinding]>> {
    // The walker knows which ancestors establish lexical callable bindings.
    // Avoid rebuilding a navigation scope for unrelated calls, and reuse it
    // within the same flet/labels/macrolet callable scope.
    if env.local_fns.is_empty() {
        return None;
    }
    if env.fn_scope_id != 0
        && let Some((cached_id, cached)) = &work.cached_lexicals
        && *cached_id == env.fn_scope_id
    {
        return Some(Rc::clone(cached));
    }
    let collected: Rc<[ScopeBinding]> = collect_scope_with(
        ctx.source,
        node.text_range().start(),
        ctx.workspace,
        |range| find_top_level_form_near(ctx.root, range, node.clone().into()),
    )
    .into();
    if env.fn_scope_id != 0 {
        work.cached_lexicals = Some((env.fn_scope_id, Rc::clone(&collected)));
    }
    Some(collected)
}

fn walk_list(
    ctx: &LintDocument<'_>,
    list: &SyntaxNode,
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    if !non_dotted_list(list) {
        // Macro calls can legally be dotted. Ordinary calls and special
        // operators require a proper argument list.
        if let Some(head) = elements(list).next().and_then(|el| symbol(&el)) {
            let package = ctx.source.package_at(list.text_range().start());
            if let Some(operator) = ctx.workspace.standard_symbol(&head, package)
                && !env.is_fn_shadowed(&head)
                && matches!(
                    crate::cl_builtins::builtin_kind(operator),
                    Some(
                        crate::cl_builtins::BuiltinKind::Function
                            | crate::cl_builtins::BuiltinKind::SpecialOperator
                    )
                )
                && let Some(dot) = elements(list).find(|part| part.kind() == SyntaxKind::Dot)
            {
                grammar::report(
                    emit,
                    "correctness/dotted-call",
                    &dot,
                    "this call requires a proper argument list",
                );
            }
        }
        return Some(());
    }
    let complete = closed_list(list);
    let call_offset = list.text_range().start();
    let package = ctx.source.package_at(call_offset);
    let mut parts = std::mem::take(&mut work.parts_buf);
    if !ctx.read_elements_into(elements(list), &mut parts) {
        parts.clear();
        work.parts_buf = parts;
        let mut read =
            elements(list).filter_map(|part| possibly_read(part, ctx.source.known_features()));
        if let Some((head, true)) = read.next()
            && let Some(name) = symbol(&head)
        {
            let lexicals = lint_scope(ctx, list, env, work);
            if let Some(op) =
                ctx.std_op(&name, package, lexicals.as_deref().unwrap_or_default(), env)
                && crate::cl_builtins::builtin_kind(op)
                    == Some(crate::cl_builtins::BuiltinKind::Function)
            {
                for (arg, known) in read {
                    if known {
                        work.enqueue(&arg, &env.nested_expr(false, false));
                    }
                }
            }
        }
        return Some(());
    }
    let result = walk_list_parts(
        ctx, list, package, complete, &parts, env, emit, work, cancelled,
    );
    parts.clear();
    work.parts_buf = parts;
    result
}

#[allow(clippy::too_many_arguments)]
fn walk_list_parts(
    ctx: &LintDocument<'_>,
    list: &SyntaxNode,
    package: Option<&str>,
    complete: bool,
    parts: &[Element],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    let call_offset = list.text_range().start();
    let Some((head_element, args)) = parts.split_first() else {
        return Some(());
    };
    let Some(head_sym) = symbol(head_element) else {
        if is_lambda_expression(ctx, head_element) {
            if complete {
                call_arguments::check_lambda_arity(
                    ctx,
                    head_element,
                    args.len(),
                    true,
                    list.text_range(),
                    emit,
                );
                emit.emit(
                    "style/redundant-lambda", list.text_range(),
                    "immediately invoked `lambda` can be written directly with `let`",
                    Some("replace `((lambda (p...) body...) arg...)` with `(let ((p arg)...) body...)`".into()),
                    Vec::new(), None,
                );
            }
            for child in parts {
                work.enqueue(child, &env.nested_expr(false, false));
            }
        } else if complete
            && matches!(
                head_element.kind(),
                SyntaxKind::List
                    | SyntaxKind::Vector
                    | SyntaxKind::Atom
                    | SyntaxKind::String
                    | SyntaxKind::RawString
                    | SyntaxKind::Character
            )
        {
            emit.emit(
                "correctness/invalid-call-operator",
                head_element.text_range(),
                "the operator position expects a symbol or a lambda expression",
                Some("use `funcall` to call the function computed by an expression".into()),
                Vec::new(),
                None,
            );
        }
        return Some(());
    };
    let lexicals = lint_scope(ctx, list, env, work);
    let lexicals = lexicals.as_deref().unwrap_or_default();
    let fn_shadowed = !env.local_fns.is_empty() && env.is_fn_shadowed(&head_sym);
    let local_fn = find_local_fn(lexicals, &head_sym, package, ctx.workspace);
    let std_ref = if local_fn.is_some() || fn_shadowed {
        None
    } else {
        ctx.workspace.standard_symbol(&head_sym, package)
    };

    let mut walk = WalkContext {
        doc: ctx,
        package,
        lexicals,
        emit,
        work,
        cancelled,
    };

    // Check ZK / ZK-LEX pattern-matching special forms/macros before general callable checks.
    if local_fn.is_none() && std_ref.is_none() {
        let scope_helper = ScopeAtOffset {
            package,
            offset: call_offset,
            workspace: ctx.workspace,
            known_features: ctx.source.known_features(),
            macro_operands: None,
        };
        if matches!(head_sym.name.as_str(), "LEXMATCH" | "LEXSCAN")
            && scope_helper.package_symbol(&head_sym, "ZK-LEX")
        {
            use crate::navigation::scope::{
                lexical_call_parts, lexical_input_parts, lexical_regex_warnings,
            };
            for (range, message) in lexical_regex_warnings(args) {
                walk.emit.emit(
                    "correctness/zk-match-mismatch",
                    range,
                    message,
                    None,
                    Vec::new(),
                    None,
                );
            }
            let Some((input, rows)) = lexical_call_parts(args) else {
                if complete {
                    walk.emit.emit(
                        "correctness/zk-match-mismatch",
                        list.text_range(),
                        "expected a lexical input followed by pattern clauses",
                        None,
                        Vec::new(),
                        None,
                    );
                }
                return Some(());
            };
            if let Some((_, expressions)) = lexical_input_parts(input) {
                for expression in expressions {
                    walk.enqueue(&expression, &env.nested_expr(false, false));
                }
            } else {
                walk.enqueue(input, &env.nested_expr(false, false));
            }
            return walk_zk_match_clauses(&mut walk, rows, env);
        }
        if matches!(
            head_sym.name.as_str(),
            "MATCH"
                | "EMATCH"
                | "CMATCH"
                | "IF-MATCH"
                | "WHEN-MATCH"
                | "UNLESS-MATCH"
                | "LET-MATCH"
                | "LET-MATCH*"
        ) && scope_helper.zk_symbol(&head_sym)
        {
            match head_sym.name.as_str() {
                "MATCH" | "EMATCH" | "CMATCH" => {
                    correctness::check_zk_match_form(ctx, call_offset, args, package, walk.emit);
                    if let Some(scrutinee) = args.first() {
                        walk.enqueue(scrutinee, &env.nested_expr(false, false));
                    }
                    return walk_zk_match_clauses(&mut walk, args.get(1..).unwrap_or(&[]), env);
                }
                "IF-MATCH" | "WHEN-MATCH" | "UNLESS-MATCH" => {
                    if let Some(scrutinee) = args.get(1) {
                        walk.enqueue(scrutinee, &env.nested_expr(false, false));
                    }
                    let rest = args.get(2..).unwrap_or(&[]);
                    let body = if rest
                        .first()
                        .and_then(symbol)
                        .is_some_and(|kw| kw.prefix == Prefix::Keyword && kw.name == "WHEN")
                    {
                        if let Some(guard) = rest.get(1) {
                            walk.enqueue(guard, &env.nested_expr(true, false));
                        }
                        rest.get(2..).unwrap_or(&[])
                    } else {
                        rest
                    };
                    return walk.walk_body(
                        body,
                        &env.nested_expr(env.in_boolean_test, env.result_discarded),
                        BodyKind::Declared,
                    );
                }
                "LET-MATCH" | "LET-MATCH*" => {
                    if let Some(bindings_el) = args.first()
                        && let Some(bindings_node) =
                            bindings_el.as_node().filter(|n| closed_list(n))
                    {
                        for pair_el in elements(bindings_node) {
                            if let Some(pair) = pair_el.as_node().filter(|n| closed_list(n))
                                && let Some(source) = elements(pair).nth(1)
                            {
                                walk.enqueue(&source, &env.nested_expr(false, false));
                            }
                        }
                    }
                    let body = args.get(1..).unwrap_or(&[]);
                    return walk.walk_body(
                        body,
                        &env.nested_expr(env.in_boolean_test, env.result_discarded),
                        BodyKind::Declared,
                    );
                }
                _ => {}
            }
        }
    }

    if std_ref.is_none()
        && local_fn.is_none()
        && !fn_shadowed
        && let Some(shape) = walk
            .emit
            .config
            .macro_shape_for(ctx.workspace, &head_sym, package)
    {
        return walk_custom_macro_shape(&mut walk, shape, args, env);
    }

    let callable = if fn_shadowed && local_fn.is_none() {
        None
    } else {
        ctx.workspace
            .resolve_callable_with_standard(&head_sym, package, local_fn, std_ref)
    };
    if complete && callable.is_none() && std_ref.is_none() && local_fn.is_none() {
        names::check_unresolved_operator(ctx, head_element, &head_sym, package, walk.emit);
    }
    if complete {
        let scope = ScopeContext {
            doc: ctx,
            package,
            lexicals,
            env,
        };
        let call = CallSite {
            list,
            head: &head_sym,
            args,
            std_op: std_ref,
        };
        correctness::check_call_correctness(scope, call, callable.as_ref(), walk.emit);
        suspicious::check_call_suspicious(scope, call, walk.emit);
        complexity::check_call_complexity(scope, call, walk.emit);
        perf_security::check_call_perf_and_security(scope, call, walk.emit);
        style_pedantic::check_call_style_and_pedantic(scope, call, callable.is_some(), walk.emit);
        if let Some(op) = std_ref {
            dataflow::record_writes(ctx, op, args, env, &mut walk.work.writes);
            dataflow::check_loop_var_mutation(ctx, op, args, env, walk.emit);
        }
    }

    if head_sym.name == "WITH-FOREIGN-MEMORY"
        && local_fn.is_none()
        && !fn_shadowed
        && foreign::is_operator(ctx, &head_sym, package)
    {
        if let Some(spec) = args
            .first()
            .and_then(Element::as_node)
            .filter(|node| closed_list(node))
            && let Some(size) = elements(spec).nth(1)
        {
            walk.enqueue(&size, &env.nested_expr(false, false));
        }
        return walk.walk_body(
            args.get(1..).unwrap_or(&[]),
            &env.nested_expr(env.in_boolean_test, env.result_discarded),
            BodyKind::Declared,
        );
    }

    if matches!(head_sym.name.as_str(), "WITH-TRANSIENT" | "DO-ELEMENTS")
        && local_fn.is_none()
        && !fn_shadowed
        && ctx.workspace.symbol_home_package(&head_sym, package)
            == Some(crate::treelist_builtins::PACKAGE)
    {
        if let Some(spec) = args
            .first()
            .and_then(Element::as_node)
            .filter(|node| closed_list(node))
        {
            for expr in elements(spec).skip(1) {
                walk.enqueue(&expr, &env.nested_expr(false, false));
            }
        }
        let body_env = if head_sym.name == "DO-ELEMENTS" {
            env.nested_expr(false, true)
        } else {
            env.nested_expr(false, false)
        };
        return walk.walk_body(args.get(1..).unwrap_or(&[]), &body_env, BodyKind::Declared);
    }

    if let Some(op) = std_ref {
        return standard_forms::walk(&mut walk, list, op, complete, args, env);
    }
    let evaluated = local_fn
        .map(|binding| binding.evaluates_arguments)
        .or_else(|| {
            callable
                .as_ref()
                .map(|signature| signature.evaluates_arguments)
        });
    // An unresolved operator may be a library macro or DSL. Only a resolved
    // ordinary callable justifies treating all its operands as expressions.
    if evaluated == Some(true) {
        for arg in args {
            walk.enqueue(arg, &env.nested_expr(false, false));
        }
    } else {
        macro_calls::walk(&mut walk, env, &head_sym, list.text_range(), local_fn, args)?;
    }
    Some(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::lint::checks) enum BodyKind {
    Forms,
    Declared,
    Documented,
}

fn walk_body_sequence<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    body: &[Element],
    env: &FormContext,
    kind: BodyKind,
) -> Option<()> {
    let mut skip = 0usize;
    let mut doc_allowed = kind == BodyKind::Documented;
    let mut body_env = None;
    while let Some(el) = body.get(skip) {
        if doc_allowed
            && el
                .as_token()
                .is_some_and(|t| matches!(t.kind(), SyntaxKind::String | SyntaxKind::RawString))
            && skip + 1 < body.len()
        {
            doc_allowed = false;
            skip += 1;
            continue;
        }
        if kind != BodyKind::Forms && is_declaration(walk.doc, el, walk.package) {
            correctness::check_declaration_form(walk.doc, el, walk.package, walk.emit);
            if walk.emit.is_enabled("perf/length-zero-comparison") {
                sequence::record_list_declarations(walk.doc, el, walk.package, env, &mut body_env);
            }
            skip += 1;
            continue;
        }
        break;
    }

    let exprs = &body[skip..];
    let env = body_env.as_ref().unwrap_or(env);
    suspicious::check_sequence_unreachable(
        walk.doc,
        exprs,
        walk.package,
        walk.lexicals,
        env,
        walk.emit,
    );
    dataflow::check_body_overwritten_bindings(
        ScopeContext {
            doc: walk.doc,
            package: walk.package,
            lexicals: walk.lexicals,
            env,
        },
        exprs,
        kind,
        walk.emit,
    );

    for (idx, form) in exprs.iter().enumerate() {
        if walk.is_cancelled() {
            return None;
        }
        let is_last = idx + 1 == exprs.len();
        let mut child_env = if is_last {
            env.nested_expr(env.in_boolean_test, env.result_discarded)
        } else {
            env.nested_expr(false, true)
        };
        if idx == 0 {
            child_env.entry_test = env.entry_test.clone();
        }
        walk.enqueue(form, &child_env);
    }
    Some(())
}

fn walk_zk_match_clauses<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    clauses: &[Element],
    env: &FormContext,
) -> Option<()> {
    for clause_el in clauses {
        let Some(clause) = clause_el.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let items: Vec<_> = elements(clause).collect();
        let mut body_start = 1usize;
        if let Some(kw) = items.get(1).and_then(symbol)
            && kw.prefix == Prefix::Keyword
            && kw.name == "WHEN"
            && let Some(guard) = items.get(2)
        {
            walk.enqueue(guard, &env.nested_expr(true, false));
            body_start = 3;
        }
        let body = items.get(body_start..).unwrap_or(&[]);
        walk.walk_body(
            body,
            &env.nested_expr(env.in_boolean_test, env.result_discarded),
            BodyKind::Declared,
        )?;
    }
    Some(())
}

pub(in crate::lint::checks) fn walk_loop_form<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    let ctx = walk.doc;
    let package = walk.package;
    let lexicals = walk.lexicals;
    let mut base_env = env.nested_expr(false, false);
    if args
        .first()
        .and_then(symbol)
        .is_some_and(|name| name.name == "NAMED")
    {
        if let Some(name) = args.get(1) {
            if super::context::is_nil_syntax(name, package, ctx.workspace) {
                base_env.push_nil_block();
            } else if let Some(target) = ctx.control_block(name, package) {
                base_env.push_block(target);
            }
        }
    } else {
        base_env.push_nil_block();
    }
    base_env.in_loop = true;

    let is_simple_loop = args.iter().all(|el| el.as_node().is_some());
    if is_simple_loop {
        let stmt_env = base_env.nested_expr(false, true);
        for arg in args {
            if walk.is_cancelled() {
                return None;
            }
            walk.enqueue(arg, &stmt_env);
        }
        return Some(());
    }

    let mut in_do_clause = false;
    let mut in_test_clause = false;
    let mut skip_non_expr = false;
    let mut in_binding_header = false;
    // Conditional clauses can pass the actual test value to an IT operand.
    let has_it_operand = args
        .iter()
        .any(|arg| symbol(arg).is_some_and(|name| name.name == "IT"));
    for (index, arg) in args.iter().enumerate() {
        if walk.is_cancelled() {
            return None;
        }
        if let Some(kw) = symbol(arg) {
            match kw.name.as_str() {
                "FOR" | "AS" | "WITH" => {
                    in_do_clause = false;
                    in_test_clause = false;
                    skip_non_expr = true;
                    in_binding_header = true;
                }
                "AND" if in_binding_header => {
                    in_do_clause = false;
                    in_test_clause = false;
                    skip_non_expr = true;
                }
                "OF-TYPE" | "USING" | "NAMED" | "INTO" => {
                    skip_non_expr = true;
                }
                "DO" | "DOING" | "INITIALLY" | "FINALLY" => {
                    in_do_clause = true;
                    in_test_clause = false;
                    skip_non_expr = false;
                    in_binding_header = false;
                }
                "WHILE" | "UNTIL" | "WHEN" | "UNLESS" | "IF" | "ALWAYS" | "NEVER" | "THEREIS" => {
                    in_do_clause = false;
                    in_test_clause = match kw.name.as_str() {
                        "THEREIS" => false,
                        "WHEN" | "UNLESS" | "IF" => !has_it_operand,
                        _ => true,
                    };
                    skip_non_expr = false;
                    in_binding_header = false;
                }
                _ if kw.as_token_keyword_like() => {
                    in_do_clause = false;
                    in_test_clause = false;
                    skip_non_expr = false;
                    in_binding_header = false;
                }
                _ => {
                    skip_non_expr = false;
                    in_test_clause = false;
                }
            }
            continue;
        }
        if std::mem::take(&mut skip_non_expr) {
            continue;
        }
        if arg.as_node().is_some() {
            let mut child_env = if in_do_clause {
                base_env.nested_expr(false, true)
            } else if std::mem::take(&mut in_test_clause) {
                base_env.nested_expr(true, false)
            } else {
                base_env.clone()
            };
            if let Some(window) = index
                .checked_sub(3)
                .and_then(|start| args.get(start..start + 8))
                && loop_open_has_cleanup(ctx, window, package, lexicals, env, walk.cancelled)
                    .is_some()
            {
                child_env.protected_loop_open = Some(arg.text_range());
            }
            walk.enqueue(arg, &child_env);
        }
    }
    Some(())
}

/// Recognize an immediately protected acquisition, not arbitrary LOOP control flow.
fn loop_open_has_cleanup(
    ctx: &LintDocument<'_>,
    window: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    let [for_kw, var, equals, open, when_kw, test, do_kw, body] = window else {
        return None;
    };
    let keyword = |el: &Element, candidates: &[&str]| {
        symbol(el).is_some_and(|sym| candidates.contains(&sym.name.as_str()))
    };
    if !keyword(for_kw, &["FOR", "AS"])
        || !keyword(equals, &["="])
        || !keyword(when_kw, &["WHEN", "IF"])
        || !keyword(do_kw, &["DO", "DOING"])
    {
        return None;
    }
    let stream = symbol(var)?;
    let same_var = |el: &Element| {
        symbol(el).is_some_and(|sym| sym.prefix == Prefix::None && sym.name == stream.name)
    };
    if stream.prefix != Prefix::None || !same_var(test) {
        return None;
    }
    let standard_call = |el: &Element| {
        let node = el.as_node()?;
        if !ctx.source.reader_syntax_known_at(node.text_range().start()) {
            return None;
        }
        let (_, args, op) = ctx.resolve_head(node, package, lexicals, env)?;
        Some((op?, args))
    };
    if standard_call(open)?.0 != "OPEN" {
        return None;
    }

    let is_close = |el: &Element| {
        standard_call(el).is_some_and(|(op, args)| {
            op == "CLOSE" && matches!(args.as_slice(), [target] if same_var(target))
        })
    };
    let mut body = body.clone();
    loop {
        if cancelled() {
            return None;
        }
        let (op, args) = standard_call(&body)?;
        match (op.as_str(), args.as_slice()) {
            ("RETURN", [value]) => body = value.clone(),
            ("LET" | "LET*", [bindings, value]) => {
                // No executable initializer or rebinding may intervene before the guard.
                if !is_nil_element(bindings, package, ctx.workspace) {
                    let bindings = bindings.as_node().filter(|node| closed_list(node))?;
                    for binding in elements(bindings) {
                        let name = if let Some(pair) = binding.as_node().filter(|n| closed_list(n))
                        {
                            let mut parts = elements(pair);
                            let name = parts.next()?;
                            if let Some(init) = parts.next()
                                && !symbol(&init).is_some_and(|sym| {
                                    matches!(
                                        ctx.workspace.standard_symbol(&sym, package),
                                        Some("NIL" | "T")
                                    )
                                })
                            {
                                return None;
                            }
                            if parts.next().is_some() {
                                return None;
                            }
                            name
                        } else {
                            binding
                        };
                        let name = symbol(&name)?;
                        if name.prefix != Prefix::None || name.name == stream.name {
                            return None;
                        }
                    }
                }
                body = value.clone();
            }
            ("UNWIND-PROTECT", [protected, cleanup, ..]) => {
                // Do not accept a CLOSE of a variable visibly reassigned in the body.
                if protected.as_node().is_some_and(|node| {
                    candidate_symbol_tokens(node).any(|token| {
                        cancelled()
                            || SymbolName::parse(token.text())
                                .is_some_and(|sym| sym.name == stream.name)
                                && dataflow::is_direct_write_target(&token, ctx)
                    })
                }) {
                    return None;
                }
                if is_close(cleanup) {
                    return Some(());
                }
                let (op, args) = standard_call(cleanup)?;
                let [check, close] = args.as_slice() else {
                    return None;
                };
                let (check_op, check_args) = standard_call(check)?;
                return (op == "WHEN"
                    && check_op == "OPEN-STREAM-P"
                    && matches!(check_args.as_slice(), [target] if same_var(target))
                    && is_close(close))
                .then_some(());
            }
            _ => return None,
        }
    }
}

trait LoopKwExt {
    fn as_token_keyword_like(&self) -> bool;
}

impl LoopKwExt for crate::reader_tokens::SymbolName {
    fn as_token_keyword_like(&self) -> bool {
        matches!(
            self.name.as_str(),
            "FOR"
                | "AS"
                | "WITH"
                | "REPEAT"
                | "WHILE"
                | "UNTIL"
                | "ALWAYS"
                | "NEVER"
                | "THEREIS"
                | "COLLECT"
                | "COLLECTING"
                | "APPEND"
                | "APPENDING"
                | "NCONC"
                | "NCONCING"
                | "COUNT"
                | "COUNTING"
                | "SUM"
                | "SUMMING"
                | "MAXIMIZE"
                | "MAXIMIZING"
                | "MINIMIZE"
                | "MINIMIZING"
                | "IF"
                | "WHEN"
                | "UNLESS"
                | "RETURN"
                | "NAMED"
                | "INTO"
        )
    }
}

fn walk_custom_macro_shape<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    shape: MacroShape,
    args: &[Element],
    env: &FormContext,
) -> Option<()> {
    match shape {
        MacroShape::Ignore => Some(()),
        MacroShape::Progn => walk.walk_body(
            args,
            &env.nested_expr(env.in_boolean_test, env.result_discarded),
            BodyKind::Declared,
        ),
        MacroShape::When | MacroShape::Unless => {
            if let Some(cond) = args.first() {
                walk.enqueue(cond, &env.nested_expr(true, false));
            }
            walk.walk_body(
                args.get(1..).unwrap_or(&[]),
                &env.nested_expr(env.in_boolean_test, env.result_discarded),
                BodyKind::Declared,
            )
        }
        MacroShape::Dolist => {
            let mut body_env = env.nested_expr(false, true);
            body_env.push_nil_block();
            body_env.in_loop = true;
            if let Some(spec) = args.first()
                && let Some(snode) = spec.as_node().filter(|n| closed_list(n))
            {
                let mut parts = elements(snode).skip(1);
                if let Some(bound) = parts.next() {
                    let mut bound_env = env.nested_expr(false, false);
                    bound_env.push_nil_block();
                    walk.enqueue(&bound, &bound_env);
                }
                if let Some(result) = parts.next() {
                    walk.enqueue(&result, &body_env.nested_expr(false, env.result_discarded));
                }
            }
            let body =
                body_after_declarations(walk.doc, args.get(1..).unwrap_or(&[]), walk.package);
            walk.walk_tagbody(body, &body_env)
        }
        MacroShape::Let => {
            if let Some(bindings_el) = args.first()
                && let Some(bnode) = bindings_el.as_node().filter(|n| closed_list(n))
            {
                for spec in elements(bnode) {
                    if let Some(snode) = spec.as_node().filter(|n| closed_list(n))
                        && let Some(init) = elements(snode).nth(1)
                    {
                        walk.enqueue(&init, &env.nested_expr(false, false));
                    }
                }
            }
            walk.walk_body(
                args.get(1..).unwrap_or(&[]),
                &env.nested_expr(env.in_boolean_test, env.result_discarded),
                BodyKind::Declared,
            )
        }
        MacroShape::Defun | MacroShape::Defmacro => {
            let mut body_env = env.function_body();
            body_env.in_defmacro = shape == MacroShape::Defmacro;
            if let Some(name) = args.first() {
                push_definition_block(walk.doc, name, walk.package, &mut body_env);
            }
            walk.walk_body(
                args.get(2..).unwrap_or(&[]),
                &body_env,
                BodyKind::Documented,
            )
        }
    }
}

fn check_declaim_special_earmuffs(
    args: &[Element],
    package: Option<&str>,
    ctx: &LintDocument<'_>,
    emit: &mut LintEmitter<'_>,
) {
    for spec in args {
        let Some(node) = spec.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let mut parts = elements(node);
        let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
            continue;
        };
        if ctx.workspace.standard_symbol(&head, package) == Some("SPECIAL") {
            for var_el in parts {
                if let Some(sym) = symbol(&var_el)
                    && !(sym.name.len() > 2 && sym.name.starts_with('*') && sym.name.ends_with('*'))
                {
                    emit.emit(
                        "style/special-earmuffs",
                        var_el.text_range(),
                        format!(
                            "proclaimed special variable `{}` should use `*earmuffs*`",
                            sym.name.to_ascii_lowercase()
                        ),
                        None,
                        Vec::new(),
                        None,
                    );
                }
            }
        }
    }
}
