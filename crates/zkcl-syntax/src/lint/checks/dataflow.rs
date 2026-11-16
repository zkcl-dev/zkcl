//! Binding uses, assignments, overwrite checks, and package-use diagnostics.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::format::TextEdit;
use crate::navigation::index::{
    OccurrenceResolver, closed_list, find_top_level_form_near, operator, symbol,
};
use crate::navigation::queries::candidate_symbol_tokens;
use crate::navigation::{Namespace, OccurrenceBinding, ScopeBinding};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

use super::super::context::{
    FormContext, LintDocument, LintEmitter, is_nil_element, repeatable_test,
};
use super::super::{FixApplicability, LintFix, LintTag};
use super::{BodyKind, ScopeContext};

struct IgnoreDeclaration {
    location: TextRange,
    scope: TextRange,
    first_read: Option<TextRange>,
}

pub(crate) fn run_dataflow_checks(
    ctx: &LintDocument<'_>,
    writes: &FxHashMap<TextRange, bool>,
    emit: &mut LintEmitter<'_>,
    cancelled: &impl Fn() -> bool,
    other_forms: Option<&[Element]>,
) -> Option<()> {
    let track_lexicals = lexical_checks_enabled(emit);
    let check_package_references =
        emit.is_enabled("correctness/unknown-common-lisp-symbol") && ctx.text.contains(':');
    let check_package_imports =
        !ctx.source.packages.is_empty() && emit.is_enabled("style/unused-package-import");
    if !track_lexicals && !check_package_references {
        if check_package_imports {
            check_unused_package_imports(ctx, emit, None);
        }
        return Some(());
    }
    let all = || candidate_symbol_tokens(ctx.root);
    let checks = ScanChecks {
        lexicals: track_lexicals,
        package_references: check_package_references,
        package_imports: check_package_imports,
    };
    let Some(rest) = other_forms else {
        return scan_dataflow(ctx, writes, emit, cancelled, all(), checks);
    };
    if track_lexicals {
        scan_dataflow(
            ctx,
            writes,
            emit,
            cancelled,
            element_tokens(rest),
            ScanChecks::LEXICAL,
        )?;
    }
    if check_package_imports || check_package_references {
        let package_elements: Vec<_> = ctx
            .root
            .children_with_tokens()
            .filter(|element| {
                check_package_imports || ctx.text_of(element.text_range()).contains(':')
            })
            .collect();
        scan_dataflow(
            ctx,
            writes,
            emit,
            cancelled,
            element_tokens(&package_elements),
            ScanChecks {
                lexicals: false,
                ..checks
            },
        )?;
    }
    Some(())
}

pub(super) fn check_form_bindings(
    ctx: &LintDocument<'_>,
    form: &SyntaxNode,
    writes: &FxHashMap<TextRange, bool>,
    emit: &mut LintEmitter<'_>,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    if lexical_checks_enabled(emit) {
        scan_dataflow(
            ctx,
            writes,
            emit,
            cancelled,
            candidate_symbol_tokens(form),
            ScanChecks::LEXICAL,
        )?;
    }
    Some(())
}

fn lexical_checks_enabled(emit: &LintEmitter<'_>) -> bool {
    [
        "style/unused-variable",
        "style/lexical-earmuffs",
        "style/unused-local-function",
        "style/unused-block-label",
        "style/unused-tag",
        "suspicious/write-only-variable",
        "suspicious/used-ignored-binding",
    ]
    .into_iter()
    .any(|rule| emit.is_enabled(rule))
}

fn element_tokens(elements: &[Element]) -> impl Iterator<Item = crate::outline::Token> + '_ {
    elements.iter().flat_map(|element| {
        element
            .as_node()
            .map(candidate_symbol_tokens)
            .into_iter()
            .flatten()
            .chain(element.as_token().cloned())
    })
}

#[derive(Clone, Copy)]
struct ScanChecks {
    lexicals: bool,
    package_references: bool,
    package_imports: bool,
}

impl ScanChecks {
    const LEXICAL: Self = Self {
        lexicals: true,
        package_references: false,
        package_imports: false,
    };
}

fn scan_dataflow(
    ctx: &LintDocument<'_>,
    writes: &FxHashMap<TextRange, bool>,
    emit: &mut LintEmitter<'_>,
    cancelled: &impl Fn() -> bool,
    tokens: impl Iterator<Item = crate::outline::Token>,
    checks: ScanChecks,
) -> Option<()> {
    let track_blocks = checks.lexicals && emit.is_enabled("style/unused-block-label");
    let track_tags = checks.lexicals && emit.is_enabled("style/unused-tag");
    let track_ignored_reads = checks.lexicals && emit.is_enabled("suspicious/used-ignored-binding");
    let check_package_imports = checks.package_imports;
    let entered_packages = if check_package_imports {
        entered_packages_in_doc(ctx)
    } else {
        FxHashSet::default()
    };
    let track_package_uses = check_package_imports && !entered_packages.is_empty();
    let mut pkg_used_unqualified: FxHashMap<&str, FxHashSet<String>> = FxHashMap::default();
    let mut pkg_used_prefixes: FxHashMap<&str, FxHashSet<String>> = FxHashMap::default();

    let mut defined_locals: FxHashMap<TextRange, (String, bool, TextRange)> = FxHashMap::default();
    let mut read_locals: FxHashSet<TextRange> = FxHashSet::default();
    let mut write_locals: FxHashSet<TextRange> = FxHashSet::default();
    let mut ignored_locals: FxHashSet<TextRange> = FxHashSet::default();
    let mut ignore_sites: FxHashMap<TextRange, Vec<IgnoreDeclaration>> = FxHashMap::default();
    let mut reads = Vec::new();
    let mut unresolved_mentions: FxHashMap<String, Vec<TextRange>> = FxHashMap::default();

    let mut defined_local_fns: FxHashMap<TextRange, (String, TextRange)> = FxHashMap::default();
    let mut used_local_fns: FxHashSet<TextRange> = FxHashSet::default();
    let mut defined_blocks: FxHashMap<TextRange, (String, TextRange)> = FxHashMap::default();
    let mut used_blocks: FxHashSet<TextRange> = FxHashSet::default();
    let mut defined_tags: FxHashMap<TextRange, (String, TextRange)> = FxHashMap::default();
    let mut used_tags: FxHashSet<TextRange> = FxHashSet::default();
    let macro_operands = ctx.macro_operands.borrow();
    let mut occurrences = OccurrenceResolver::new(
        ctx.source,
        ctx.root,
        ctx.workspace,
        (!macro_operands.is_empty()).then_some(&*macro_operands),
    );

    for token in tokens.filter(|token| token.kind() == SyntaxKind::Atom) {
        if cancelled() {
            return None;
        }
        let tok_range = token.text_range();
        if !ctx.source.reader_syntax_known_at(tok_range.start()) {
            break;
        }
        let spelling = token.text();
        let mut parsed_sym: Option<SymbolName> = None;
        if checks.package_references && spelling.contains(':') {
            parsed_sym = SymbolName::parse(spelling);
            if let Some(name) = parsed_sym.as_ref()
                && matches!(name.prefix, Prefix::Package { external: true, .. })
                && !crate::cl_builtins::is_external_cl_symbol(&name.name)
            {
                // Guarded source is read only for the selected feature set.
                let active = token.parent_ancestors().all(|node| {
                    crate::ast::FeatureConditional::from_node(&node).is_none_or(|feature| {
                        !feature.guarded.text_range().contains_range(tok_range)
                            || feature.is_active(ctx.source.known_features()) == Some(true)
                    })
                });
                if active {
                    let package = ctx.source.package_at(tok_range.start());
                    super::names::check_common_lisp_package_reference(
                        ctx, &token, name, package, emit,
                    );
                }
            }
        }
        if track_package_uses
            && let Some(pkg) = ctx.source.package_at(tok_range.start())
            && entered_packages.contains(pkg)
        {
            if parsed_sym.is_none() {
                parsed_sym = SymbolName::parse(spelling);
            }
            if let Some(sym) = parsed_sym.as_ref() {
                match &sym.prefix {
                    Prefix::None => {
                        pkg_used_unqualified
                            .entry(pkg)
                            .or_default()
                            .insert(sym.name.clone());
                    }
                    Prefix::Package { name, .. } => {
                        pkg_used_prefixes
                            .entry(pkg)
                            .or_default()
                            .insert(name.clone());
                    }
                    Prefix::Keyword | Prefix::Uninterned => {}
                }
            }
        }
        if !checks.lexicals || spelling.starts_with(':') {
            continue;
        }
        let is_number =
            spelling.as_bytes().first().is_some_and(|byte| {
                byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'#')
            }) && crate::reader_tokens::is_number_literal_token(spelling);
        if is_number && !(track_tags && is_integer_tag_position(&token)) {
            continue;
        }
        let resolved = occurrences.resolve(&token);
        let Some(binding) = resolved else {
            if !is_number
                && let Some(symbol) = parsed_sym.or_else(|| SymbolName::parse(spelling))
                && symbol.prefix == Prefix::None
                && !is_inside_quote_or_declare(&token)
            {
                unresolved_mentions
                    .entry(symbol.name)
                    .or_default()
                    .push(tok_range);
            }
            continue;
        };
        match binding {
            OccurrenceBinding::LocalSite {
                def_range,
                namespace: Namespace::Value,
                spelling,
                occurrence_range,
                ..
            } => {
                if def_range == occurrence_range {
                    let has_earmuffs = spelling.name.len() > 2
                        && spelling.name.starts_with('*')
                        && spelling.name.ends_with('*');
                    let scope_range = enclosing_binding_scope_range(&token).unwrap_or(def_range);
                    defined_locals.entry(def_range).or_insert((
                        spelling.name,
                        has_earmuffs,
                        scope_range,
                    ));
                } else if let Some((declaration, scope)) =
                    declaration_occurrence(&token, ctx, false, def_range)
                {
                    record_ignore_declaration(
                        &mut ignored_locals,
                        &mut ignore_sites,
                        def_range,
                        declaration,
                        tok_range,
                        scope,
                    );
                } else if writes.get(&tok_range) == Some(&false) {
                    write_locals.insert(def_range);
                } else {
                    read_locals.insert(def_range);
                    if track_ignored_reads {
                        reads.push((def_range, tok_range));
                    }
                }
            }
            OccurrenceBinding::LocalSite {
                def_range,
                namespace: Namespace::Function | Namespace::SetfFunction,
                spelling,
                occurrence_range,
                ..
            } => {
                if def_range == occurrence_range {
                    let scope = enclosing_binding_scope_range(&token).unwrap_or(def_range);
                    defined_local_fns
                        .entry(def_range)
                        .or_insert((spelling.name, scope));
                } else if let Some((declaration, scope)) =
                    declaration_occurrence(&token, ctx, true, def_range)
                {
                    record_ignore_declaration(
                        &mut ignored_locals,
                        &mut ignore_sites,
                        def_range,
                        declaration,
                        tok_range,
                        scope,
                    );
                } else {
                    used_local_fns.insert(def_range);
                    if track_ignored_reads {
                        reads.push((def_range, tok_range));
                    }
                }
            }
            OccurrenceBinding::LocalSite {
                def_range,
                namespace: Namespace::Block,
                spelling,
                occurrence_range,
                ..
            } if track_blocks => {
                if def_range == occurrence_range {
                    if spelling.name != "NIL"
                        && is_explicit_block_definition(&token, ctx)
                        && let Some(block_node) = token.parent()
                    {
                        defined_blocks
                            .entry(def_range)
                            .or_insert((spelling.name, block_node.text_range()));
                    }
                } else {
                    used_blocks.insert(def_range);
                }
            }
            OccurrenceBinding::LocalSite {
                def_range,
                namespace: Namespace::Tag | Namespace::IntegerTag,
                spelling,
                occurrence_range,
                ..
            } if track_tags => {
                if def_range == occurrence_range {
                    if let Some(tagbody_node) = token.parent() {
                        defined_tags
                            .entry(def_range)
                            .or_insert((spelling.name, tagbody_node.text_range()));
                    }
                } else {
                    used_tags.insert(def_range);
                }
            }
            _ => {}
        }
    }

    for (binding, used) in reads {
        if let Some(declaration) = ignore_sites.get_mut(&binding).and_then(|sites| {
            sites
                .iter_mut()
                .rev()
                .find(|site| site.scope.contains_range(used))
        }) {
            declaration.first_read.get_or_insert(used);
        }
    }
    for declaration in ignore_sites.into_values().flatten() {
        if let Some(used) = declaration.first_read
            && let Some(diag) = emit.diagnostic(
                "suspicious/used-ignored-binding",
                used,
                "a binding declared `ignore` is used",
                Some("use `ignorable` if this binding may be used".into()),
                Vec::new(),
                None,
            )
        {
            diag.note(declaration.location, "the `ignore` declaration is here");
        }
    }

    for (def_range, (name, has_earmuffs, scope_range)) in &defined_locals {
        if *has_earmuffs {
            emit.emit(
                "style/lexical-earmuffs",
                *def_range,
                format!(
                    "lexical variable `{}` uses `*earmuffs*` without `special` declaration",
                    name.to_ascii_lowercase()
                ),
                Some("remove `*...*` or declare `(declare (special ...))`".into()),
                Vec::new(),
                None,
            );
            if ctx.source.package_at(def_range.start()).is_none() {
                // In package-less files, `*earmuffs*` may bind a special declared in another file.
                continue;
            }
        }
        if name.starts_with('_') || ignored_locals.contains(def_range) {
            continue;
        }
        let has_opaque_mention =
            has_opaque_mention_in_scope(&unresolved_mentions, name, *def_range, *scope_range);
        let is_read = read_locals.contains(def_range) || has_opaque_mention;
        let is_written = write_locals.contains(def_range);
        if !is_read && !is_written {
            emit.emit(
                "style/unused-variable",
                *def_range,
                format!("unused lexical variable `{}`", name.to_ascii_lowercase()),
                None,
                vec![LintTag::Unnecessary],
                unused_binding_fix(ctx, *def_range),
            );
        } else if !is_read && is_written {
            emit.emit(
                "suspicious/write-only-variable",
                *def_range,
                format!(
                    "variable `{}` is mutated or assigned but never read",
                    name.to_ascii_lowercase()
                ),
                Some("return the updated value or remove the unused accumulator".into()),
                vec![LintTag::Unnecessary],
                None,
            );
        }
    }

    for (def_range, (name, scope_range)) in &defined_blocks {
        if !used_blocks.contains(def_range)
            && !name.starts_with('_')
            && !has_opaque_mention_in_scope(&unresolved_mentions, name, *def_range, *scope_range)
        {
            emit.emit(
                "style/unused-block-label",
                *def_range,
                format!(
                    "block `{}` is never targeted by `return-from`",
                    name.to_ascii_lowercase()
                ),
                Some("replace with `progn` if no early exit is needed".into()),
                vec![LintTag::Unnecessary],
                None,
            );
        }
    }

    for (def_range, (name, scope_range)) in &defined_tags {
        if !used_tags.contains(def_range)
            && !name.starts_with('_')
            && !has_opaque_mention_in_scope(&unresolved_mentions, name, *def_range, *scope_range)
        {
            emit.emit(
                "style/unused-tag",
                *def_range,
                format!(
                    "tag `{}` is never targeted by `go`",
                    name.to_ascii_lowercase()
                ),
                Some("remove the unused tag or verify control flow".into()),
                vec![LintTag::Unnecessary],
                None,
            );
        }
    }

    for (def_range, (name, scope)) in defined_local_fns {
        if !used_local_fns.contains(&def_range)
            && !name.starts_with('_')
            && !ignored_locals.contains(&def_range)
            && !has_opaque_mention_in_scope(&unresolved_mentions, &name, def_range, scope)
        {
            emit.emit(
                "style/unused-local-function",
                def_range,
                format!("unused local function `{}`", name.to_ascii_lowercase()),
                Some("remove the unused local function or declare `(ignore #'...)`".into()),
                vec![LintTag::Unnecessary],
                None,
            );
        }
    }

    if track_package_uses {
        check_unused_package_imports(
            ctx,
            emit,
            Some((&entered_packages, &pkg_used_unqualified, &pkg_used_prefixes)),
        );
    }
    Some(())
}

pub(super) fn record_writes(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    env: &FormContext,
    writes: &mut FxHashMap<TextRange, bool>,
) {
    let result_used = !env.result_discarded;
    let mut mark = |place: &Element, read| {
        if place.kind() == SyntaxKind::Atom {
            writes.insert(place.text_range(), read);
        }
    };
    match op {
        "SETF" | "SETQ" | "PSETF" | "PSETQ" => {
            let (pairs, _) = args.as_chunks::<2>();
            for (i, [place, _]) in pairs.iter().enumerate() {
                mark(
                    place,
                    result_used && matches!(op, "SETF" | "SETQ") && i + 1 == pairs.len(),
                );
            }
        }
        "INCF" | "DECF" | "POP" => {
            if let Some(place) = args.first() {
                mark(place, result_used);
            }
        }
        "PUSH" | "PUSHNEW" | "REMF" => {
            if let Some(place) = args.get(if op == "REMF" { 0 } else { 1 }) {
                mark(place, result_used);
            }
        }
        "MULTIPLE-VALUE-SETQ" => {
            if let Some(names) = args
                .first()
                .and_then(Element::as_node)
                .filter(|node| closed_list(node))
                && let Some(names) = ctx.read_elements(elements(names))
            {
                for place in names {
                    mark(&place, result_used);
                }
            }
        }
        _ => {}
    }
}

fn declaration_occurrence(
    token: &crate::outline::Token,
    ctx: &LintDocument<'_>,
    function: bool,
    binding: TextRange,
) -> Option<(&'static str, TextRange)> {
    // Fast-path: token must live inside a declaration spec whose parent is a `(declare ...)` list.
    let mut spec = token.parent()?;
    for _ in 0..if function { 3 } else { 1 } {
        let parent = spec.parent()?;
        if parent.kind() == SyntaxKind::List
            && closed_list(&parent)
            && parent_might_be_head(&parent, "DECLARE")
        {
            let package = ctx.source.package_at(token.text_range().start());
            if super::is_declaration(ctx, &parent.clone().into(), package) {
                let head = symbol(&elements(&spec).next()?)?;
                let declaration = match ctx.workspace.standard_symbol(&head, package) {
                    Some("IGNORE") => "IGNORE",
                    Some("IGNORABLE") => "IGNORABLE",
                    _ => "OTHER",
                };
                let owner = parent.parent()?.text_range();
                let scope = if owner.contains_range(binding) {
                    owner
                } else {
                    TextRange::new(parent.text_range().end(), owner.end())
                };
                return Some((declaration, scope));
            }
        }
        spec = parent;
    }
    None
}

fn record_ignore_declaration(
    ignored: &mut FxHashSet<TextRange>,
    sites: &mut FxHashMap<TextRange, Vec<IgnoreDeclaration>>,
    binding: TextRange,
    declaration: &str,
    location: TextRange,
    scope: TextRange,
) {
    if matches!(declaration, "IGNORE" | "IGNORABLE") {
        ignored.insert(binding);
    }
    if declaration == "IGNORE" {
        sites.entry(binding).or_default().push(IgnoreDeclaration {
            location,
            scope,
            first_read: None,
        });
    }
}

fn parent_might_be_head(node: &SyntaxNode, target: &str) -> bool {
    let Some(first) = elements(node).next() else {
        return false;
    };
    let Some(tok) = first.as_token() else {
        return false;
    };
    let text = tok.text();
    text.contains(':')
        || text.contains('|')
        || text.contains('\\')
        || text.eq_ignore_ascii_case(target)
}

fn has_opaque_mention_in_scope(
    unresolved_mentions: &FxHashMap<String, Vec<TextRange>>,
    name: &str,
    def_range: TextRange,
    scope_range: TextRange,
) -> bool {
    unresolved_mentions.get(name).is_some_and(|ranges| {
        let start = ranges.partition_point(|range| range.start() < scope_range.start());
        ranges[start..]
            .iter()
            .take(2)
            .any(|r| *r != def_range && scope_range.contains_range(*r))
    })
}

fn is_integer_tag_position(token: &crate::outline::Token) -> bool {
    let Some(parent) = token.parent() else {
        return false;
    };
    if parent.kind() != SyntaxKind::List || !closed_list(&parent) {
        return false;
    }
    let Some(head_el) = elements(&parent).next() else {
        return false;
    };
    let Some(head_tok) = head_el.as_token() else {
        return false;
    };
    let text = head_tok.text();
    let tail = text.rsplit(':').next().unwrap_or(text);
    tail.eq_ignore_ascii_case("TAGBODY")
        || tail.eq_ignore_ascii_case("GO")
        || tail.eq_ignore_ascii_case("PROG")
        || tail.eq_ignore_ascii_case("PROG*")
        || tail.eq_ignore_ascii_case("DO")
        || tail.eq_ignore_ascii_case("DO*")
        || tail.eq_ignore_ascii_case("DOLIST")
        || tail.eq_ignore_ascii_case("DOTIMES")
}

fn is_explicit_block_definition(token: &crate::outline::Token, ctx: &LintDocument<'_>) -> bool {
    let Some(parent) = token.parent() else {
        return false;
    };
    if parent.kind() != SyntaxKind::List || !closed_list(&parent) {
        return false;
    }
    if !parent_might_be_head(&parent, "BLOCK") {
        return false;
    }
    let package = ctx.source.package_at(parent.text_range().start());
    elements(&parent)
        .next()
        .and_then(|e| symbol(&e))
        .is_some_and(|s| ctx.workspace.standard_symbol(&s, package) == Some("BLOCK"))
}

fn unused_binding_fix(ctx: &LintDocument<'_>, binding: TextRange) -> Option<LintFix> {
    use crate::ast::tokens_at_offset;
    let token = tokens_at_offset(ctx.root, binding.start()).right_biased()?;
    if token.text_range() != binding {
        return None;
    }
    let package = ctx.source.package_at(binding.start());
    for node in token.parent_ancestors() {
        if !closed_list(&node) {
            continue;
        }
        let parts = ctx.read_elements(elements(&node))?;
        let Some(head) = parts.first().and_then(symbol) else {
            continue;
        };
        let Some(op) = ctx.workspace.standard_symbol(&head, package) else {
            continue;
        };
        let index = match op {
            "DEFUN" | "DEFMACRO" | "DEFINE-COMPILER-MACRO" | "DEFTYPE" => 2,
            "LAMBDA"
            | "LET"
            | "LET*"
            | "DOLIST"
            | "DOTIMES"
            | "DO"
            | "DO*"
            | "DESTRUCTURING-BIND"
            | "MULTIPLE-VALUE-BIND" => 1,
            "DEFMETHOD" => {
                parts
                    .iter()
                    .enumerate()
                    .skip(2)
                    .find(|(_, el)| {
                        el.as_node().is_some()
                            || super::super::context::is_nil_syntax(el, package, ctx.workspace)
                    })?
                    .0
            }
            _ => continue,
        };
        if !parts
            .get(index)
            .is_some_and(|parameters| parameters.text_range().contains_range(binding))
        {
            continue;
        }
        // Declarations may precede a docstring. Keep the original string in
        // place so it remains documentation after inserting the declaration.
        let start = index
            + if matches!(
                op,
                "DO" | "DO*" | "DESTRUCTURING-BIND" | "MULTIPLE-VALUE-BIND"
            ) {
                2
            } else {
                1
            };
        let body = parts.get(start..)?;
        let documented = matches!(
            op,
            "DEFUN" | "DEFMACRO" | "DEFINE-COMPILER-MACRO" | "DEFTYPE" | "LAMBDA" | "DEFMETHOD"
        );
        let after_doc =
            documented && body.len() > 1 && super::super::context::is_string_literal_el(&body[0]);
        let offset = body.get(usize::from(after_doc))?.text_range().start();
        let candidate = SymbolName::parse("declare")?;
        let declare = if ctx.workspace.standard_symbol(&candidate, package) == Some("DECLARE") {
            "declare"
        } else {
            "common-lisp:declare"
        };
        let candidate = SymbolName::parse("ignore")?;
        let ignore = if ctx.workspace.standard_symbol(&candidate, package) == Some("IGNORE") {
            "ignore"
        } else {
            "common-lisp:ignore"
        };
        let name = token.text();
        return Some(LintFix {
            title: format!("Declare `(ignore {name})`"),
            applicability: FixApplicability::MaybeIncorrect,
            edits: vec![TextEdit {
                range: TextRange::empty(offset),
                replacement: format!("({declare} ({ignore} {name})) "),
            }],
        });
    }
    None
}

fn is_inside_quote_or_declare(token: &crate::outline::Token) -> bool {
    let mut node = token.parent();
    while let Some(cur) = node {
        match cur.kind() {
            SyntaxKind::ReaderForm => match cur.first_token().map(|t| t.kind()) {
                Some(SyntaxKind::Quote) => return true,
                Some(
                    SyntaxKind::Unquote
                    | SyntaxKind::UnquoteSplicing
                    | SyntaxKind::UnquoteDestructive,
                ) => {
                    // Inside an unquote inside a backquote, code is evaluated at runtime/expansion.
                    if let Some(after_bq) = cur.parent().and_then(|p| {
                        let mut anc = Some(p);
                        while let Some(a) = anc {
                            if a.kind() == SyntaxKind::ReaderForm
                                && a.first_token()
                                    .is_some_and(|t| t.kind() == SyntaxKind::Backquote)
                            {
                                return a.parent();
                            }
                            anc = a.parent();
                        }
                        None
                    }) {
                        node = Some(after_bq);
                        continue;
                    }
                }
                Some(SyntaxKind::Backquote) => return true,
                _ => {}
            },
            SyntaxKind::List => {
                if matches!(operator(&cur).as_deref(), Some("QUOTE" | "DECLARE")) {
                    return true;
                }
            }
            _ => {}
        }
        node = cur.parent();
    }
    false
}

fn enclosing_binding_scope_range(token: &crate::outline::Token) -> Option<TextRange> {
    let mut node = token.parent();
    while let Some(cur) = node {
        if cur.kind() == SyntaxKind::List
            && let Some(head_el) = elements(&cur).next()
            && let Some(head_tok) = head_el.as_token()
        {
            let text = head_tok.text();
            let raw = text.rsplit(':').next().unwrap_or(text);
            let upper = raw.to_ascii_uppercase();
            let op = upper.as_str();
            if matches!(
                op,
                "LET"
                    | "LET*"
                    | "DEFUN"
                    | "DEFMACRO"
                    | "DEFMETHOD"
                    | "LAMBDA"
                    | "FLET"
                    | "LABELS"
                    | "MACROLET"
                    | "DOLIST"
                    | "DOTIMES"
                    | "DO"
                    | "DO*"
                    | "PROG"
                    | "PROG*"
                    | "LOOP"
                    | "MULTIPLE-VALUE-BIND"
                    | "DESTRUCTURING-BIND"
                    | "HANDLER-CASE"
                    | "RESTART-CASE"
            ) || op.starts_with("WITH-")
                || op.ends_with("-MATCH")
                || op.ends_with("-EMATCH")
                || op.ends_with("-CMATCH")
            {
                return Some(cur.text_range());
            }
        }
        node = cur.parent();
    }
    None
}

pub(super) fn is_direct_write_target(
    token: &crate::outline::Token,
    ctx: &LintDocument<'_>,
) -> bool {
    let Some(parent) = token.parent() else {
        return false;
    };
    if parent.kind() != SyntaxKind::List || !closed_list(&parent) {
        return false;
    }
    let tok_range = token.text_range();
    // A variable in MULTIPLE-VALUE-SETQ is inside the target list, not
    // an immediate child of the assignment form.
    if parent.parent().is_some_and(|form| {
        if !closed_list(&form) {
            return false;
        }
        let package = ctx.source.package_at(form.text_range().start());
        let mut parts = elements(&form);
        parts
            .next()
            .and_then(|head| symbol(&head))
            .is_some_and(|head| {
                ctx.workspace.standard_symbol(&head, package) == Some("MULTIPLE-VALUE-SETQ")
            })
            && parts.next().as_ref().and_then(Element::as_node) == Some(&parent)
    }) {
        return true;
    }
    let package = ctx.source.package_at(parent.text_range().start());
    let mut parts = elements(&parent);
    let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
        return false;
    };
    let Some(std_op) = ctx.workspace.standard_symbol(&head, package) else {
        return false;
    };
    let args: Vec<_> = parts.collect();
    match std_op {
        "SETF" | "PSETF" | "SETQ" | "PSETQ" => args
            .as_chunks::<2>()
            .0
            .iter()
            .any(|pair| pair[0].text_range() == tok_range),
        "INCF" | "DECF" | "POP" => args.first().is_some_and(|p| p.text_range() == tok_range),
        "PUSH" | "PUSHNEW" => args.get(1).is_some_and(|p| p.text_range() == tok_range),
        _ => false,
    }
}

pub(crate) fn check_body_overwritten_bindings(
    scope: ScopeContext<'_, '_>,
    forms: &[Element],
    kind: BodyKind,
    emit: &mut LintEmitter<'_>,
) {
    if forms.len() < 2 || !emit.is_enabled("suspicious/overwritten-binding") {
        return;
    }
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    // Removing the first expression can promote a later string to a docstring.
    let can_remove = scope.known_package(forms[0].text_range())
        && (kind != BodyKind::Documented
            || !forms
                .iter()
                .any(|form| matches!(form.kind(), SyntaxKind::String | SyntaxKind::RawString)));
    let mut pending: FxHashMap<TextRange, (Element, &Element, Element)> = FxHashMap::default();
    let macro_operands = ctx.macro_operands.borrow();
    let mut resolver: Option<OccurrenceResolver<'_, '_>> = None;
    let mut resolve_lexical = |place: &Element| -> Option<TextRange> {
        let token = place.as_token()?;
        let res = resolver.get_or_insert_with(|| {
            OccurrenceResolver::new(
                ctx.source,
                ctx.root,
                ctx.workspace,
                (!macro_operands.is_empty()).then_some(&*macro_operands),
            )
        });
        match res.resolve(token)? {
            OccurrenceBinding::LocalSite {
                namespace: Namespace::Value,
                def_range,
                ..
            } => Some(def_range),
            _ => None,
        }
    };
    for form in forms {
        if let Some((target, value)) = single_var_assignment(form, package, ctx, lexicals, env) {
            if !pending.is_empty() {
                match plain_read_with(ctx, &value, package, env, &mut resolve_lexical) {
                    Some(Some(binding)) => {
                        pending.remove(&binding);
                    }
                    Some(None) => {}
                    None => pending.clear(),
                }
            }
            // A dynamic or generalized assignment can interact with the
            // environment in ways unavailable to source-level analysis.
            let Some(binding) = resolve_lexical(&target).filter(|_| !env.symbol_macros) else {
                pending.clear();
                continue;
            };
            if let Some((earlier, assignment, rhs)) =
                pending.insert(binding, (target.clone(), form, value))
                && let Some(name) = symbol(&target)
            {
                let fix = (can_remove
                    && matches!(rhs.kind(), SyntaxKind::Atom | SyntaxKind::Character)
                    && plain_read_with(ctx, &rhs, package, env, &mut resolve_lexical)
                        == Some(None))
                .then(|| remove_overwritten_assignment(ctx, assignment.text_range()));
                report_overwrite(emit, &earlier, &target, &name.name, fix);
            }
        } else if !pending.is_empty() {
            match plain_read_with(ctx, form, package, env, &mut resolve_lexical) {
                Some(Some(binding)) => {
                    pending.remove(&binding);
                }
                Some(None) => {}
                None => pending.clear(),
            }
        }
    }
}

fn remove_overwritten_assignment(ctx: &LintDocument<'_>, range: TextRange) -> LintFix {
    let start = usize::from(range.start());
    let end = usize::from(range.end());
    let leading = ctx.text[..start].rsplit('\n').next().unwrap_or_default();
    let trailing = ctx.text[end..].split_once('\n');
    let edit = if leading.trim_ascii().is_empty()
        && let Some((trailing, _)) = trailing
        && trailing.trim_ascii().is_empty()
    {
        TextEdit {
            range: TextRange::new(
                ((start - leading.len()) as u32).into(),
                ((end + trailing.len() + 1) as u32).into(),
            ),
            replacement: String::new(),
        }
    } else {
        TextEdit {
            range,
            replacement: " ".into(),
        }
    };
    LintFix {
        title: "Remove the overwritten assignment".into(),
        applicability: FixApplicability::MachineApplicable,
        edits: vec![edit],
    }
}

pub(crate) fn check_let_overwritten_binding(
    ctx: &LintDocument<'_>,
    bindings_el: &Element,
    body: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("suspicious/overwritten-binding") {
        return;
    }
    let Some(bnode) = bindings_el.as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let Some(bindings) = ctx.read_elements(elements(bnode)) else {
        return;
    };
    // With several LET* bindings, a later initializer can observe the first.
    let [spec] = bindings.as_slice() else { return };
    let Some(snode) = spec.as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let Some(parts) = ctx.read_elements(elements(snode)) else {
        return;
    };
    let [var, initializer] = parts.as_slice() else {
        return;
    };
    if is_nil_element(initializer, package, ctx.workspace) {
        return;
    }
    let Some(first_body) = super::body_after_declarations(ctx, body, package).first() else {
        return;
    };
    let Some((target, value)) = single_var_assignment(first_body, package, ctx, lexicals, env)
    else {
        return;
    };
    if let Some(name) = symbol(var)
        && lexical_definition(ctx, &target) == Some(var.text_range())
        && repeatable_test(ctx, &value, package, lexicals, env).is_some()
        && !element_mentions_symbol(&value, &name.name)
    {
        report_overwrite(emit, initializer, &target, &name.name, None);
    }
}

// The outer option is proof that evaluation returns without invoking Lisp
// code. The inner range identifies the lexical value that is observed.
fn plain_read_with(
    ctx: &LintDocument<'_>,
    value: &Element,
    package: Option<&str>,
    env: &FormContext,
    resolve: &mut impl FnMut(&Element) -> Option<TextRange>,
) -> Option<Option<TextRange>> {
    use super::super::context::{is_nil_syntax, is_number_literal_el, is_t_element};
    if is_nil_syntax(value, package, ctx.workspace)
        || is_t_element(value, package, ctx.workspace)
        || is_number_literal_el(value)
        || matches!(
            value.kind(),
            SyntaxKind::String | SyntaxKind::RawString | SyntaxKind::Character | SyntaxKind::Vector
        )
        || value.as_node().is_some_and(|node| {
            node.kind() == SyntaxKind::ReaderForm
                && node
                    .first_token()
                    .is_some_and(|token| token.kind() == SyntaxKind::Quote)
        })
    {
        return Some(None);
    }
    let name = symbol(value)?;
    if name.prefix == Prefix::Keyword {
        return Some(None);
    }
    if env.symbol_macros || ctx.workspace.is_symbol_macro(&name, package) {
        return None;
    }
    resolve(value).map(Some)
}

pub(super) fn lexical_definition(ctx: &LintDocument<'_>, place: &Element) -> Option<TextRange> {
    let operands = ctx.macro_operands.borrow();
    match ctx.source.resolve_occurrence_with_operands(
        place.text_range().start(),
        ctx.workspace,
        (!operands.is_empty()).then_some(&*operands),
        |range| find_top_level_form_near(ctx.root, range, place.clone()),
    )? {
        OccurrenceBinding::LocalSite {
            namespace: Namespace::Value,
            def_range,
            ..
        } => Some(def_range),
        _ => None,
    }
}

fn report_overwrite(
    emit: &mut LintEmitter<'_>,
    earlier: &Element,
    later: &Element,
    name: &str,
    fix: Option<LintFix>,
) {
    if let Some(diag) = emit.diagnostic(
        "suspicious/overwritten-binding",
        earlier.text_range(),
        format!(
            "the value assigned to `{}` is replaced before a read",
            name.to_ascii_lowercase()
        ),
        Some(if fix.is_some() {
            "remove the unused assignment to this lexical variable".into()
        } else {
            "preserve any required evaluation of the earlier value expression".into()
        }),
        Vec::new(),
        fix,
    ) {
        diag.note(later.text_range(), "this assignment replaces the value");
    }
}

fn single_var_assignment(
    el: &Element,
    package: Option<&str>,
    ctx: &LintDocument<'_>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> Option<(Element, Element)> {
    let node = el.as_node().filter(|n| closed_list(n))?;
    let mut parts = elements(node);
    let head = symbol(&parts.next()?)?;
    if !matches!(
        ctx.std_op(&head, package, lexicals, env),
        Some("SETF" | "SETQ")
    ) {
        return None;
    }
    let place = parts.next()?;
    let val = parts.next()?;
    if parts.next().is_some() || !symbol(&place).is_some_and(|name| name.prefix == Prefix::None) {
        return None;
    }
    Some((place, val))
}

pub(crate) fn check_loop_var_mutation(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    if env.loop_bindings.is_empty() || !emit.is_enabled("suspicious/mutated-loop-variable") {
        return;
    }
    let places: Vec<&Element> = match op {
        "SETF" | "PSETF" | "SETQ" | "PSETQ" => {
            args.as_chunks::<2>().0.iter().map(|p| &p[0]).collect()
        }
        "INCF" | "DECF" | "POP" => args.first().into_iter().collect(),
        "PUSH" | "PUSHNEW" => args.get(1).into_iter().collect(),
        _ => Vec::new(),
    };
    for place in places {
        if let Some(sym) = symbol(place)
            && env
                .loop_bindings
                .iter()
                .any(|binding| binding.name.name == sym.name)
            && let Some(definition) = lexical_definition(ctx, place)
            && env
                .loop_bindings
                .iter()
                .any(|binding| binding.definition == definition)
        {
            emit.emit(
                "suspicious/mutated-loop-variable",
                place.text_range(),
                format!(
                    "mutating loop iteration variable `{}` inside loop body",
                    sym.name.to_ascii_lowercase()
                ),
                Some("bind a separate local variable or use `loop`".into()),
                Vec::new(),
                None,
            );
        }
    }
}

fn entered_packages_in_doc(ctx: &LintDocument<'_>) -> FxHashSet<String> {
    let mut entered = FxHashSet::default();
    for el in elements(ctx.root) {
        if let Some(node) = el.as_node()
            && crate::outline::with_list_operator(node, |op| op == "IN-PACKAGE").unwrap_or(false)
            && let Some(arg) = elements(node).nth(1)
            && let Some(pkg) = crate::reader_tokens::string_designator(&arg)
        {
            entered.insert(pkg);
        }
    }
    entered
}

type CollectedPackageUses<'a> = (
    &'a FxHashSet<String>,
    &'a FxHashMap<&'a str, FxHashSet<String>>,
    &'a FxHashMap<&'a str, FxHashSet<String>>,
);

fn check_unused_package_imports(
    ctx: &LintDocument<'_>,
    emit: &mut LintEmitter<'_>,
    precollected: Option<CollectedPackageUses<'_>>,
) {
    if ctx.source.packages.is_empty() {
        return;
    }
    let owned_entered;
    let entered = match precollected {
        Some((entered, _, _)) => entered,
        None => {
            owned_entered = entered_packages_in_doc(ctx);
            &owned_entered
        }
    };
    if entered.is_empty() {
        return;
    }

    let mut owned_unqualified: FxHashMap<&str, FxHashSet<String>> = FxHashMap::default();
    let mut owned_prefixes: FxHashMap<&str, FxHashSet<String>> = FxHashMap::default();
    let (used_unqualified_map, used_prefixes_map) = match precollected {
        Some((_, uq, px)) => (uq, px),
        None => {
            for tok in candidate_symbol_tokens(ctx.root) {
                if tok.kind() != SyntaxKind::Atom {
                    continue;
                }
                let offset = tok.text_range().start();
                let Some(pkg_name) = ctx.source.package_at(offset) else {
                    continue;
                };
                if !entered.contains(pkg_name) {
                    continue;
                }
                if let Some(sym) = SymbolName::parse(tok.text()) {
                    match sym.prefix {
                        Prefix::None => {
                            owned_unqualified
                                .entry(pkg_name)
                                .or_default()
                                .insert(sym.name);
                        }
                        Prefix::Package { name, .. } => {
                            owned_prefixes.entry(pkg_name).or_default().insert(name);
                        }
                        Prefix::Keyword | Prefix::Uninterned => {}
                    }
                }
            }
            (&owned_unqualified, &owned_prefixes)
        }
    };

    for pkg in &ctx.source.packages {
        if !entered.contains(&pkg.name) {
            continue;
        }
        let used_unqualified = used_unqualified_map.get(pkg.name.as_str());
        let used_prefixes = used_prefixes_map.get(pkg.name.as_str());

        for des in &pkg.symbol_designators {
            let Some(_source_pkg) = &des.import_source else {
                continue;
            };
            if pkg.exports.iter().any(|e| e == &des.name) {
                continue;
            }
            if !used_unqualified.is_some_and(|set| set.contains(&des.name)) {
                emit.emit(
                    "style/unused-package-import",
                    des.range,
                    format!(
                        "imported symbol `{}` is never referenced in package `{}`",
                        des.name.to_ascii_lowercase(),
                        pkg.name.to_ascii_lowercase()
                    ),
                    None,
                    vec![LintTag::Unnecessary],
                    None,
                );
            }
        }

        for alias in pkg.local_nicknames.keys() {
            if !used_prefixes.is_some_and(|set| set.contains(alias))
                && let Some(alias_range) = find_local_nickname_range(ctx.root, &pkg.name, alias)
            {
                emit.emit(
                    "style/unused-package-import",
                    alias_range,
                    format!(
                        "local package nickname `{}` is never used in package `{}`",
                        alias.to_ascii_lowercase(),
                        pkg.name.to_ascii_lowercase()
                    ),
                    None,
                    vec![LintTag::Unnecessary],
                    None,
                );
            }
        }
    }
}

fn find_local_nickname_range(root: &SyntaxNode, pkg_name: &str, alias: &str) -> Option<TextRange> {
    for el in elements(root) {
        let Some(node) = el.as_node() else {
            continue;
        };
        if operator(node).as_deref() != Some("DEFPACKAGE") || !closed_list(node) {
            continue;
        }
        let mut parts = elements(node).skip(1);
        let Some(name_el) = parts.next() else {
            continue;
        };
        if crate::reader_tokens::string_designator(&name_el).as_deref() != Some(pkg_name) {
            continue;
        }
        for opt in parts {
            let Some(opt_node) = opt.as_node().filter(|n| closed_list(n)) else {
                continue;
            };
            let mut opt_parts = elements(opt_node);
            if opt_parts
                .next()
                .and_then(|e| symbol(&e))
                .is_some_and(|s| s.prefix == Prefix::Keyword && s.name == "LOCAL-NICKNAMES")
            {
                for pair in opt_parts {
                    if let Some(pnode) = pair.as_node().filter(|n| closed_list(n))
                        && let Some(alias_el) = elements(pnode).next()
                        && crate::reader_tokens::string_designator(&alias_el).as_deref()
                            == Some(alias)
                    {
                        return Some(alias_el.text_range());
                    }
                }
            }
        }
    }
    None
}

pub(super) fn element_mentions_symbol(el: &Element, target_upper: &str) -> bool {
    match el {
        Element::Token(_) => symbol(el).is_some_and(|name| name.name == target_upper),
        Element::Node(node) => node.descendants_with_tokens().any(|part| {
            part.into_token().is_some_and(|token| {
                symbol(&token.into()).is_some_and(|name| name.name == target_upper)
            })
        }),
    }
}
