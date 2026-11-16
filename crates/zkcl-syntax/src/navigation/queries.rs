use std::collections::HashSet;
use std::sync::Arc;

use crate::ast::{Token, token_at_offset, tokens_at_offset};
use crate::cl_builtins::{BuiltinKind, builtin_kind, builtin_signature};
use crate::outline::{Element, elements};
use crate::reader_tokens::{
    Prefix, SymbolName, is_bit_vector_token, is_numeric_token, is_radix_number_token,
    string_designator,
};
use crate::simd_builtins;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};
use crate::treelist_builtins;

use super::index::{
    CallableSignature, DocumentIndex, OccurrenceResolver, WorkspaceDeclarations, WorkspaceIndex,
    closed_list, find_top_level_form, non_dotted_list, operator, possibly_read, symbol,
};
use super::{
    CallFeedback, CodeAction, CodeActionKind, CompletionCandidate, CompletionKind,
    DefinitionLocation, Hover, InlayHint, Namespace, OccurrenceBinding, ScopeBinding,
    SemanticToken, SemanticTokenKind, SemanticWarning, SignatureHelp, SourceLocation,
};
use crate::navigation::scope::{ScopeAtOffset, is_evaluated_call_position};

pub(crate) fn definition<'a>(
    source_doc: &str,
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    documents: impl IntoIterator<Item = (&'a str, &'a DocumentIndex)>,
) -> Option<DefinitionLocation> {
    let workspace = WorkspaceIndex::new(documents);
    match source.resolve_occurrence_at(root, offset, &workspace)? {
        OccurrenceBinding::LocalSite {
            def_range,
            occurrence_range,
            ..
        } if def_range != occurrence_range => Some(DefinitionLocation {
            document: source_doc.to_owned(),
            selection_range: def_range,
        }),
        OccurrenceBinding::LocalSite { .. } | OccurrenceBinding::LocalWithoutSite => None,
        OccurrenceBinding::Global {
            name,
            is_declaration: false,
            ..
        } => {
            let (document, declaration) = workspace.lookup_unique_declaration(&name)?;
            Some(DefinitionLocation {
                document: document.to_owned(),
                selection_range: declaration.range,
            })
        }
        OccurrenceBinding::Global {
            is_declaration: true,
            ..
        } => None,
    }
}

pub(crate) fn hover(
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    workspace: &WorkspaceIndex<'_>,
) -> Option<Hover> {
    if let Some(binding) = source.resolve_occurrence_at(root, offset, workspace) {
        match binding {
            OccurrenceBinding::LocalSite {
                def_range,
                namespace,
                spelling,
                params,
                occurrence_range,
                ..
            } if namespace != Namespace::ImplicitBlock || def_range != occurrence_range => {
                let contents = match namespace {
                    Namespace::Function | Namespace::SetfFunction => {
                        let sig = format_param_list(params.as_deref().unwrap_or(&[]));
                        let fn_name = if namespace == Namespace::SetfFunction {
                            format!("(setf {})", spelling.name)
                        } else {
                            spelling.name
                        };
                        format!("```common-lisp\n(local-function {fn_name} {sig})\n```")
                    }
                    Namespace::Value => {
                        format!("```common-lisp\n(lexical-variable {})\n```", spelling.name)
                    }
                    Namespace::SpecialValue => {
                        format!("```common-lisp\n(special {})\n```", spelling.name)
                    }
                    Namespace::Block | Namespace::ImplicitBlock => {
                        format!("```common-lisp\n(block {})\n```", spelling.name)
                    }
                    Namespace::Pattern => {
                        format!("```common-lisp\n(pattern {})\n```", spelling.name)
                    }
                    Namespace::Tag | Namespace::IntegerTag => {
                        format!("```common-lisp\n(tagbody-tag {})\n```", spelling.name)
                    }
                    Namespace::Type => {
                        format!("```common-lisp\n(type {})\n```", spelling.name)
                    }
                };
                return Some(Hover {
                    range: occurrence_range,
                    contents,
                });
            }
            OccurrenceBinding::Global {
                name,
                occurrence_range,
                ..
            } => {
                if let Some((_, decl)) = workspace.lookup_unique_declaration(&name) {
                    let pkg = workspace.declaration_package(decl).unwrap_or("CL-USER");
                    let op = decl.operator.name.to_ascii_lowercase();
                    let full_name = if decl.name.namespace == Namespace::SetfFunction {
                        format!("(setf {pkg}::{})", decl.name.spelling.name)
                    } else {
                        format!("{pkg}::{}", decl.name.spelling.name)
                    };
                    let header = if let Some(params) = &decl.params {
                        format!("({op} {full_name} {})", format_param_list(params))
                    } else {
                        format!("({op} {full_name})")
                    };
                    let mut contents = format!("```common-lisp\n{header}\n```");
                    if let Some(doc) = &decl.documentation {
                        contents.push_str("\n\n");
                        contents.push_str(doc);
                    }
                    return Some(Hover {
                        range: occurrence_range,
                        contents,
                    });
                }
                if let Some(std_name) =
                    workspace.standard_symbol(&name.spelling, name.package.as_deref())
                    && let Some(sig) = builtin_signature(std_name)
                {
                    let kind_label = match sig.kind {
                        BuiltinKind::SpecialOperator => "special-operator",
                        BuiltinKind::Macro => "macro",
                        BuiltinKind::Function => "defun",
                        BuiltinKind::Variable => "defvar",
                        BuiltinKind::Constant => "defconstant",
                        BuiltinKind::Type => "deftype",
                        BuiltinKind::Declaration => "declaration",
                        BuiltinKind::LambdaListKeyword => "lambda-list-keyword",
                    };
                    let header = if matches!(
                        sig.kind,
                        BuiltinKind::Function | BuiltinKind::Macro | BuiltinKind::SpecialOperator
                    ) {
                        format!(
                            "({kind_label} cl:{} {})",
                            sig.name,
                            format_param_list(sig.params)
                        )
                    } else {
                        format!("({kind_label} cl:{})", sig.name)
                    };
                    let mut contents = format!("```common-lisp\n{header}\n```");
                    if !sig.summary.is_empty() {
                        contents.push_str("\n\n");
                        contents.push_str(sig.summary);
                    }
                    return Some(Hover {
                        range: occurrence_range,
                        contents,
                    });
                }
            }
            OccurrenceBinding::LocalSite { .. } | OccurrenceBinding::LocalWithoutSite => {}
        }
    }

    let tok = token_at_offset(root, offset)?;
    if tok.kind() != SyntaxKind::Atom {
        return None;
    }
    let sym = SymbolName::parse(tok.text())?;
    let pkg = source.package_at(offset);
    if let Some(sit_hover) = eval_when_situation_hover(&tok, &sym, pkg, workspace) {
        return Some(sit_hover);
    }
    if source.reader_syntax_known_at(offset)
        && let Some(hover) = simd_hover(&sym, pkg, tok.text_range(), workspace)
    {
        return Some(hover);
    }
    let std_name = workspace.standard_symbol(&sym, pkg)?;
    let sig = builtin_signature(std_name)?;
    let header = if sig.params.is_empty() {
        format!("(cl:{})", sig.name)
    } else {
        format!("(cl:{} {})", sig.name, sig.params.join(" "))
    };
    let mut contents = format!("```common-lisp\n{header}\n```");
    if !sig.summary.is_empty() {
        contents.push_str("\n\n");
        contents.push_str(sig.summary);
    }
    Some(Hover {
        range: tok.text_range(),
        contents,
    })
}

fn simd_hover(
    symbol: &SymbolName,
    package: Option<&str>,
    range: TextRange,
    workspace: &WorkspaceIndex<'_>,
) -> Option<Hover> {
    let function = simd_builtins::function(&symbol.name);
    let doc = match function {
        Some(function) => function.documentation,
        None => simd_builtins::type_documentation(&symbol.name)?,
    };
    if workspace.symbol_home_package(symbol, package) != Some(simd_builtins::PACKAGE) {
        return None;
    }
    let name = symbol.name.to_ascii_lowercase();
    let header = if let Some(function) = function {
        format!("(defun zk-simd:{name} ({}))", function.lambda_list)
    } else {
        format!("(deftype zk-simd:{name})")
    };
    Some(Hover {
        range,
        contents: format!("```common-lisp\n{header}\n```\n\n{doc}"),
    })
}

fn format_param_list<S: AsRef<str>>(params: &[S]) -> String {
    if params.is_empty() {
        "()".to_owned()
    } else {
        let joined: Vec<&str> = params.iter().map(AsRef::as_ref).collect();
        format!("({})", joined.join(" "))
    }
}

fn eval_when_situation_hover(
    tok: &crate::outline::Token,
    sym: &SymbolName,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> Option<Hover> {
    let sit_list = tok.parent()?;
    if sit_list.kind() != SyntaxKind::List {
        return None;
    }
    let eval_when_list = sit_list.parent()?;
    if eval_when_list.kind() != SyntaxKind::List {
        return None;
    }
    let mut ew_elems = elements(&eval_when_list);
    let head = ew_elems.next()?;
    let head_sym = symbol(&head)?;
    if workspace.standard_symbol(&head_sym, package) != Some("EVAL-WHEN") {
        return None;
    }
    let second = ew_elems.next()?;
    if second.text_range() != sit_list.text_range() {
        return None;
    }
    let (header, doc) = match (&sym.prefix, sym.name.as_str()) {
        (Prefix::Keyword, "COMPILE-TOPLEVEL") => (
            ":compile-toplevel",
            "Evaluate body forms at compile time when processed as a top-level form by `COMPILE-FILE` (CLHS §3.8.5).",
        ),
        (Prefix::Keyword, "LOAD-TOPLEVEL") => (
            ":load-toplevel",
            "Compile body forms as top-level forms to be executed when the compiled file is loaded (CLHS §3.8.5).",
        ),
        (Prefix::Keyword, "EXECUTE") => (
            ":execute",
            "Evaluate body forms when `EVAL-WHEN` is executed as code (or at top-level in `EVAL` / `LOAD` / `:compile-toplevel` context) (CLHS §3.8.5).",
        ),
        (Prefix::None, "COMPILE") => (
            "compile (deprecated)",
            "Deprecated CLtL1 `EVAL-WHEN` situation; prefer `:compile-toplevel`.",
        ),
        (Prefix::None, "LOAD") => (
            "load (deprecated)",
            "Deprecated CLtL1 `EVAL-WHEN` situation; prefer `:load-toplevel`.",
        ),
        (Prefix::None, "EVAL") => (
            "eval (deprecated)",
            "Deprecated CLtL1 `EVAL-WHEN` situation; prefer `:execute`.",
        ),
        _ => return None,
    };
    Some(Hover {
        range: tok.text_range(),
        contents: format!("```common-lisp\n(eval-when-situation {header})\n```\n\n{doc}"),
    })
}

#[derive(PartialEq, Eq)]
pub(crate) enum CallQuery {
    Signature,
    Arity,
    Feedback,
}

pub(crate) fn classify_first_call_argument<'a, T>(
    source: &DocumentIndex,
    root: &SyntaxNode,
    argument: &Token,
    documents: impl IntoIterator<Item = (&'a str, &'a DocumentIndex)>,
    classify: impl FnOnce(&SymbolName, Option<&str>) -> Option<T>,
) -> Option<T> {
    let list = argument.parent()?;
    if list.kind() != SyntaxKind::List
        || list.ancestors().last().as_ref() != Some(root)
        || !non_dotted_list(&list)
        || !source.reader_syntax_known_at(argument.text_range().start())
    {
        return None;
    }
    let (head, range, mut args) = call_head(&list, source.known_features())?;
    if !source.package_known_at(range.start()) || args.next()? != (argument.clone().into(), true) {
        return None;
    }
    let package = source.package_at(range.start());
    let result = classify(&head, package)?;
    let workspace = WorkspaceIndex::new(documents);
    let lexicals = collect_scope_at(source, root, range.start(), &workspace);
    find_local_fn(&lexicals, &head, package, &workspace)
        .is_none()
        .then_some(result)
}

pub(crate) fn call_query<'a>(
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    documents: impl IntoIterator<Item = (&'a str, &'a DocumentIndex)>,
    query: CallQuery,
    live_standard: &dyn Fn(&SymbolName) -> Option<bool>,
) -> Option<CallFeedback> {
    if !source.reader_syntax_known_at(offset) {
        return None;
    }
    let wants_arity = query != CallQuery::Signature;
    let mut node = Some(find_enclosing_list(root, offset, wants_arity)?);
    let workspace = WorkspaceIndex::new(documents);
    let mut signature = None;
    'calls: while let Some(list) = node {
        node = list.parent();
        let closed = list
            .last_child_or_token()
            .is_some_and(|last| last.kind() == SyntaxKind::CloseParen);
        let wants_signature = query != CallQuery::Arity
            && signature.is_none()
            && offset > list.text_range().start()
            && (!closed || offset < list.text_range().end());
        if !wants_signature && !wants_arity {
            continue;
        }
        let Some((head, head_range, args)) = call_head(&list, source.known_features()) else {
            continue;
        };
        let head_at = head_range.start();
        let lexicals = collect_scope_at(source, root, head_at, &workspace);
        let Some(spec) = resolve_call(
            &list,
            &head,
            source.package_at(head_at),
            &workspace,
            &lexicals,
            live_standard,
        ) else {
            continue;
        };
        let mut arg_index = 0;
        let mut actual = 0;
        for (arg, known) in args {
            if !known {
                continue 'calls;
            }
            if wants_signature && arg.text_range().end() < offset {
                arg_index += 1;
            } else if !wants_arity {
                break;
            }
            actual += 1;
        }
        if wants_arity && let Some(message) = check_callable_arity(&spec, actual, closed) {
            return Some(CallFeedback::ArityError {
                range: head_range,
                message,
            });
        }
        if wants_signature {
            signature = Some(signature_help(spec, arg_index));
            if !wants_arity {
                break;
            }
        }
    }
    signature.map(CallFeedback::Signature)
}

fn find_enclosing_list(
    root: &SyntaxNode,
    offset: TextSize,
    include_after_closing: bool,
) -> Option<SyntaxNode> {
    let clamped = offset.min(root.text_range().end());
    let mut best: Option<SyntaxNode> = None;
    for tok in tokens_at_offset(root, clamped) {
        if !include_after_closing
            && tok.kind() == SyntaxKind::CloseParen
            && tok.text_range().end() == clamped
        {
            continue;
        }
        let mut anc = tok.parent();
        while let Some(n) = anc {
            if n.kind() == SyntaxKind::List && offset > n.text_range().start() {
                if best
                    .as_ref()
                    .is_none_or(|b| n.text_range().len() < b.text_range().len())
                {
                    best = Some(n);
                }
                break;
            }
            anc = n.parent();
        }
    }
    if best.is_none()
        && include_after_closing
        && let Some(previous) = root
            .children_with_tokens()
            .filter(|element| element.text_range().end() <= offset && !element.kind().is_trivia())
            .last()
        && let Some(list) = previous
            .into_node()
            .filter(|node| node.kind() == SyntaxKind::List)
    {
        return Some(list);
    }
    best
}

fn resolve_call<'scope>(
    list: &SyntaxNode,
    head: &SymbolName,
    package: Option<&str>,
    workspace: &'scope WorkspaceIndex<'_>,
    lexicals: &'scope [ScopeBinding],
    live_standard: &dyn Fn(&SymbolName) -> Option<bool>,
) -> Option<CallableSignature<'scope>> {
    if !non_dotted_list(list) {
        return None;
    }
    let local_fn = find_local_fn(lexicals, head, package, workspace);
    let live = live_standard(head);
    if local_fn.is_none() && live == Some(true) {
        return CallableSignature::standard(&head.name);
    }
    let spec = workspace.resolve_callable(head, package, local_fn)?;
    if spec.standard_name.is_some() && live == Some(false) {
        return None;
    }
    Some(spec)
}

fn call_head<'a, 'features>(
    list: &'a SyntaxNode,
    known_features: Option<&'features [String]>,
) -> Option<(
    SymbolName,
    TextRange,
    impl Iterator<Item = (Element, bool)> + use<'a, 'features>,
)> {
    let mut args = elements(list).filter_map(move |element| possibly_read(element, known_features));
    let (head, known) = args.next()?;
    let name = symbol(&head)?;
    (known && is_evaluated_call_position(list, known_features)).then_some((
        name,
        head.text_range(),
        args,
    ))
}

fn signature_help(spec: CallableSignature<'_>, arg_index: usize) -> SignatureHelp {
    let mut label = format!("({}", spec.name);
    for param in spec.lambda_list() {
        label.push(' ');
        label.push_str(param);
    }
    label.push(')');
    SignatureHelp {
        label,
        documentation: spec.documentation.map(str::to_owned),
        parameters: spec.lambda_list().map(str::to_owned).collect(),
        active_parameter: active_param_index(spec.lambda_list(), arg_index),
    }
}

fn active_param_index<'a>(
    params: impl IntoIterator<Item = &'a str>,
    arg_index: usize,
) -> Option<usize> {
    let mut remaining = arg_index;
    let mut params = params.into_iter().enumerate();
    while let Some((idx, param)) = params.next() {
        if param.eq_ignore_ascii_case("&AUX") {
            break;
        }
        if ["&OPTIONAL", "&WHOLE", "&ENVIRONMENT", "&ALLOW-OTHER-KEYS"]
            .iter()
            .any(|marker| param.eq_ignore_ascii_case(marker))
        {
            continue;
        }
        if ["&REST", "&BODY", "&KEY"]
            .iter()
            .any(|marker| param.eq_ignore_ascii_case(marker))
        {
            return Some(params.next().map_or(idx, |(next, _)| next));
        }
        if remaining == 0 {
            return Some(idx);
        }
        remaining -= 1;
    }
    None
}

pub(crate) fn check_callable_arity(
    spec: &CallableSignature<'_>,
    actual: usize,
    closed: bool,
) -> Option<String> {
    let (min, max) = arity_bounds(spec);
    if closed && actual < min {
        let name = spec.name.to_ascii_lowercase();
        return Some(format!(
            "{name} requires at least {min} argument{}, found {actual}",
            if min == 1 { "" } else { "s" }
        ));
    }
    if let Some(max) = max
        && actual > max
    {
        let name = spec.name.to_ascii_lowercase();
        return Some(format!(
            "{name} takes at most {max} argument{}, found {actual}",
            if max == 1 { "" } else { "s" }
        ));
    }
    None
}

pub(crate) fn check_eval_when_warnings(
    sit_el: Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
    warnings: &mut Vec<SemanticWarning>,
) {
    let Some(sit_node) = sit_el.as_node() else {
        if let Some(sym) = symbol(&sit_el)
            && workspace.standard_symbol(&sym, package) == Some("NIL")
        {
            warnings.push(SemanticWarning {
                range: sit_el.text_range(),
                message: "empty `eval-when` situation list discards all body forms".into(),
            });
        } else {
            warnings.push(SemanticWarning {
                range: sit_el.text_range(),
                message: "expected a list of `eval-when` situations (`:compile-toplevel`, `:load-toplevel`, `:execute`)".into(),
            });
        }
        return;
    };
    if !closed_list(sit_node) {
        return;
    }
    let mut items = elements(sit_node).peekable();
    if items.peek().is_none() {
        warnings.push(SemanticWarning {
            range: sit_node.text_range(),
            message: "empty `eval-when` situation list discards all body forms".into(),
        });
        return;
    }
    for item in items {
        let Some(sym) = symbol(&item) else {
            warnings.push(SemanticWarning {
                range: item.text_range(),
                message: "invalid `eval-when` situation: expected `:compile-toplevel`, `:load-toplevel`, or `:execute`".into(),
            });
            continue;
        };
        if sym.prefix == Prefix::Keyword {
            if !matches!(
                sym.name.as_str(),
                "COMPILE-TOPLEVEL" | "LOAD-TOPLEVEL" | "EXECUTE"
            ) {
                warnings.push(SemanticWarning {
                    range: item.text_range(),
                    message: format!(
                        "unknown `eval-when` situation `:{}`; expected `:compile-toplevel`, `:load-toplevel`, or `:execute`",
                        sym.name.to_ascii_lowercase()
                    ),
                });
            }
        } else if matches!(
            workspace.standard_symbol(&sym, package),
            Some("COMPILE" | "LOAD" | "EVAL")
        ) {
            let modern = match sym.name.as_str() {
                "COMPILE" => ":compile-toplevel",
                "LOAD" => ":load-toplevel",
                _ => ":execute",
            };
            warnings.push(SemanticWarning {
                range: item.text_range(),
                message: format!(
                    "deprecated `eval-when` situation `{}`; use `{modern}`",
                    sym.name.to_ascii_lowercase()
                ),
            });
        } else {
            warnings.push(SemanticWarning {
                range: item.text_range(),
                message: format!(
                    "unknown `eval-when` situation `{}`; expected `:compile-toplevel`, `:load-toplevel`, or `:execute`",
                    sym.name.to_ascii_lowercase()
                ),
            });
        }
    }
}

pub(crate) fn inlay_hints<'a>(
    source: &DocumentIndex,
    root: &SyntaxNode,
    range: TextRange,
    documents: impl IntoIterator<Item = (&'a str, &'a DocumentIndex)>,
) -> Vec<InlayHint> {
    let documents: Vec<_> = documents.into_iter().collect();
    let declarations = Arc::new(
        WorkspaceDeclarations::collect(&documents, &|| false)
            .expect("synchronous navigation query cannot be cancelled"),
    );
    let workspace = WorkspaceIndex::new(documents).with_declarations(declarations);
    let mut hints = Vec::new();
    for node in root.descendants() {
        let call_offset = node.text_range().start();
        if !source.reader_syntax_known_at(call_offset) {
            break;
        }
        if node.kind() != SyntaxKind::List
            || !closed_list(&node)
            || node.text_range().intersect(range).is_none()
        {
            continue;
        }
        let pkg = source.package_at(call_offset);
        let Some((head_sym, _, args)) = call_head(&node, source.known_features()) else {
            continue;
        };
        let lexicals = collect_scope_at(source, root, call_offset, &workspace);
        let local_fn = find_local_fn(&lexicals, &head_sym, pkg, &workspace);
        let Some(spec) = workspace.resolve_callable(&head_sym, pkg, local_fn) else {
            continue;
        };
        if !spec.evaluates_arguments {
            continue;
        }
        if let Some(std_name) = spec.standard_name
            && matches!(
                std_name,
                "+" | "-"
                    | "*"
                    | "/"
                    | "="
                    | "/="
                    | "<"
                    | "<="
                    | ">"
                    | ">="
                    | "EQ"
                    | "EQL"
                    | "EQUAL"
                    | "EQUALP"
                    | "CONS"
                    | "LIST"
                    | "VALUES"
                    | "APPEND"
                    | "FUNCALL"
                    | "APPLY"
            )
        {
            continue;
        }

        let mut positional = Vec::new();
        for p in spec.lambda_list() {
            if p.eq_ignore_ascii_case("&OPTIONAL") {
                continue;
            }
            if p.starts_with('&') || p.ends_with('*') || p.ends_with("...") {
                break;
            }
            let clean = p
                .trim_start_matches('(')
                .split_whitespace()
                .next()
                .unwrap_or(p)
                .trim_end_matches(')');
            if !clean.is_empty() {
                positional.push(clean.to_ascii_lowercase());
            }
        }
        if positional.len() < 2 {
            continue;
        }
        let args: Vec<_> = args
            .map_while(|(arg, known)| known.then_some(arg))
            .collect();
        if args.len() < 2 {
            continue;
        }
        for (arg, param_name) in args.iter().zip(positional.iter()) {
            if let Some(tok) = arg.as_token()
                && tok.text().eq_ignore_ascii_case(param_name)
            {
                continue;
            }
            hints.push(InlayHint {
                offset: arg.text_range().start(),
                label: format!("{param_name}:"),
            });
        }
    }
    hints.sort_by_key(|h| h.offset);
    hints
}

pub(crate) fn refactor_actions<'a>(
    source: &DocumentIndex,
    range: TextRange,
    all_docs: &[(&'a str, &'a SyntaxNode, &'a DocumentIndex)],
) -> Vec<CodeAction> {
    if !source.reader_syntax_known_at(range.start()) {
        return Vec::new();
    }
    let mut actions = Vec::new();
    let workspace = WorkspaceIndex::new(all_docs.iter().map(|(n, _, i)| (*n, *i)));
    for decl in &source.declarations {
        let enclosing_top = source
            .top_level_forms
            .iter()
            .find(|f| f.range.contains_range(decl.range))
            .map(|f| f.range)
            .unwrap_or(decl.range);
        if enclosing_top.intersect(range).is_none()
            && !enclosing_top.contains_inclusive(range.start())
        {
            continue;
        }
        let Some(pkg_name) = workspace.declaration_package(decl) else {
            continue;
        };
        let sym_name = &decl.name.spelling.name;
        if let Some((pkg_doc, insert_range, new_text)) =
            find_defpackage_export_edit(all_docs, pkg_name, sym_name)
        {
            let lower_sym = sym_name.to_ascii_lowercase();
            actions.push(CodeAction {
                title: format!("Export `{lower_sym}` from package `{pkg_name}`"),
                kind: CodeActionKind::Refactor,
                preferred: false,
                edits: vec![(
                    SourceLocation {
                        document: pkg_doc.to_owned(),
                        range: insert_range,
                    },
                    new_text,
                )],
            });
        }
    }

    actions
}

fn find_defpackage_export_edit<'a>(
    all_docs: &[(&'a str, &'a SyntaxNode, &'a DocumentIndex)],
    pkg_name: &str,
    sym_name: &str,
) -> Option<(&'a str, TextRange, String)> {
    let lower_sym = sym_name.to_ascii_lowercase();
    for &(doc_name, doc_root, doc_idx) in all_docs {
        let Some(pkg) = doc_idx.packages.iter().find(|p| p.name == pkg_name) else {
            continue;
        };
        if pkg.exports.iter().any(|e| e == sym_name) {
            return None;
        }
        for el in elements(doc_root) {
            let Some(list) = el.as_node() else {
                continue;
            };
            if operator(list).as_deref() != Some("DEFPACKAGE") || !closed_list(list) {
                continue;
            }
            let mut forms = elements(list).skip(1);
            let Some(name_el) = forms.next() else {
                continue;
            };
            if string_designator(&name_el).as_deref() != Some(pkg_name) {
                continue;
            }
            for opt in forms {
                if let Some(opt_node) = opt.as_node()
                    && closed_list(opt_node)
                    && let Some(head) = elements(opt_node).next()
                    && let Some(sym) = symbol(&head)
                    && sym.prefix == Prefix::Keyword
                    && sym.name == "EXPORT"
                    && let Some(close_tok) = opt_node.last_token()
                {
                    let pos = close_tok.text_range().start();
                    return Some((doc_name, TextRange::empty(pos), format!(" #:{lower_sym}")));
                }
            }
            if let Some(close_tok) = list.last_token() {
                let pos = close_tok.text_range().start();
                return Some((
                    doc_name,
                    TextRange::empty(pos),
                    format!(" (:export #:{lower_sym})"),
                ));
            }
        }
    }
    None
}

fn arity_bounds(spec: &CallableSignature<'_>) -> (usize, Option<usize>) {
    let mut min_args = 0usize;
    let mut max_args = 0usize;
    let mut in_optional = false;
    let mut params = spec.lambda_list();
    while let Some(param) = params.next() {
        if param.eq_ignore_ascii_case("&AUX") {
            break;
        }
        if param.eq_ignore_ascii_case("&OPTIONAL") {
            in_optional = true;
            continue;
        }
        if param.eq_ignore_ascii_case("&WHOLE") || param.eq_ignore_ascii_case("&ENVIRONMENT") {
            params.next();
            continue;
        }
        if ["&REST", "&BODY", "&KEY", "&ALLOW-OTHER-KEYS"]
            .iter()
            .any(|marker| param.eq_ignore_ascii_case(marker))
            || (!spec.is_source() && (param.ends_with('*') || param.ends_with("...")))
        {
            return (min_args, None);
        }
        if !in_optional {
            min_args += 1;
        }
        max_args += 1;
    }
    (min_args, Some(max_args))
}

pub(crate) fn completions<'a>(
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    documents: impl IntoIterator<Item = (&'a str, &'a DocumentIndex)>,
) -> (TextRange, Vec<CompletionCandidate>) {
    let documents: Vec<_> = documents.into_iter().collect();
    let declarations = Arc::new(
        WorkspaceDeclarations::collect(&documents, &|| false)
            .expect("synchronous navigation query cannot be cancelled"),
    );
    let workspace = WorkspaceIndex::new(documents).with_declarations(declarations);
    let token = token_at_offset(root, offset).filter(|t| t.kind() == SyntaxKind::Atom);
    let (replacement, prefix) = match &token {
        Some(token) => {
            let range = token.text_range();
            (range, &token.text()[..usize::from(offset - range.start())])
        }
        None => (TextRange::empty(offset), ""),
    };
    (
        replacement,
        completion_candidates(source, root, offset, prefix, &workspace),
    )
}

fn completion_candidates(
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    prefix_text: &str,
    workspace: &WorkspaceIndex<'_>,
) -> Vec<CompletionCandidate> {
    let current_pkg = source.package_at(offset);
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    if let Some(sit_cands) =
        eval_when_situation_completions(root, offset, prefix_text, current_pkg, workspace)
    {
        return sit_cands;
    }

    if let Some((pkg_prefix, rest)) = prefix_text.rsplit_once(':') {
        let internal = pkg_prefix.ends_with(':');
        let pkg_raw = pkg_prefix.trim_end_matches(':');
        if pkg_raw.is_empty() && !internal {
            let kw_prefix = rest.to_ascii_uppercase();
            let mut kw_seen = HashSet::new();
            for (kw, detail, doc) in
                call_keyword_param_completions(source, root, offset, current_pkg, workspace)
            {
                if kw.starts_with(&kw_prefix) && kw_seen.insert(kw.clone()) {
                    candidates.push(CompletionCandidate {
                        label: format!(":{}", kw.to_ascii_lowercase()),
                        kind: CompletionKind::Keyword,
                        detail: Some(detail),
                        documentation: doc,
                    });
                }
            }
            for kw in [
                "COMPILE-TOPLEVEL",
                "LOAD-TOPLEVEL",
                "EXECUTE",
                "USE",
                "EXPORT",
                "NICKNAMES",
                "LOCAL-NICKNAMES",
                "SHADOW",
                "IMPORT-FROM",
                "SHADOWING-IMPORT-FROM",
                "DOCUMENTATION",
                "TEST",
                "TEST-NOT",
                "KEY",
                "START",
                "END",
                "START1",
                "END1",
                "START2",
                "END2",
                "COUNT",
                "FROM-END",
                "INITIAL-VALUE",
                "INITIAL-ELEMENT",
                "INITIAL-CONTENTS",
                "ELEMENT-TYPE",
                "FILL-POINTER",
                "ADJUSTABLE",
                "DISPLACED-TO",
                "DISPLACED-INDEX-OFFSET",
                "INITARG",
                "INITFORM",
                "DEFAULT-INITARGS",
                "ACCESSOR",
                "READER",
                "WRITER",
                "ALLOCATION",
                "TYPE",
                "CONSTRUCTOR",
                "PREDICATE",
                "COPIER",
                "CONC-NAME",
                "INCLUDE",
                "PRINT-OBJECT",
                "PRINT-FUNCTION",
                "REPORT",
                "INTERACTIVE",
                "DIRECTION",
                "IF-EXISTS",
                "IF-DOES-NOT-EXIST",
                "EXTERNAL-FORMAT",
                "STREAM",
                "BASE",
                "RADIX",
                "CASE",
                "CIRCLE",
                "ESCAPE",
                "GENSYM",
                "LENGTH",
                "LEVEL",
                "LINES",
                "MISER-WIDTH",
                "PRETTY",
                "READABLY",
                "RIGHT-MARGIN",
            ] {
                if kw.starts_with(&kw_prefix) && kw_seen.insert(kw.to_owned()) {
                    candidates.push(CompletionCandidate {
                        label: format!(":{kw}"),
                        kind: CompletionKind::Keyword,
                        detail: Some("keyword".into()),
                        documentation: None,
                    });
                }
            }
            return candidates;
        }
        let package_name = pkg_raw.to_ascii_uppercase();
        let target_pkg = workspace.resolve_package_prefix(current_pkg, &package_name);
        let stem_upper = rest.to_ascii_uppercase();
        if let Some(target_pkg) = target_pkg {
            let sep = if internal { "::" } else { ":" };
            if target_pkg == "COMMON-LISP" {
                push_cl_completion_candidates(
                    &mut candidates,
                    &stem_upper,
                    Some((pkg_raw, sep)),
                    None,
                    workspace,
                );
                return candidates;
            }
            for (_, doc_idx) in &workspace.documents {
                for decl in &doc_idx.declarations {
                    if !decl.name.spelling.name.starts_with(&stem_upper) {
                        continue;
                    }
                    let Some(package) = workspace.resolve_symbol_home_in_package(
                        target_pkg,
                        &decl.name.spelling.name,
                        !internal,
                    ) else {
                        continue;
                    };
                    if !workspace.declaration_matches(package, decl) {
                        continue;
                    }
                    if seen.insert((decl.name.namespace, decl.name.spelling.name.clone())) {
                        candidates.push(CompletionCandidate {
                            label: format!(
                                "{pkg_raw}{sep}{}",
                                decl.name.spelling.name.to_ascii_lowercase()
                            ),
                            kind: match decl.name.namespace {
                                Namespace::Function | Namespace::SetfFunction => {
                                    CompletionKind::Function
                                }
                                Namespace::Value | Namespace::SpecialValue => {
                                    CompletionKind::Variable
                                }
                                Namespace::Type => CompletionKind::Type,
                                Namespace::Pattern => CompletionKind::Macro,
                                Namespace::Block
                                | Namespace::ImplicitBlock
                                | Namespace::Tag
                                | Namespace::IntegerTag => CompletionKind::Keyword,
                            },
                            detail: Some(target_pkg.to_owned()),
                            documentation: decl.documentation.clone(),
                        });
                    }
                }
            }
            if source.reader_syntax_known_at(offset) {
                append_simd_completions(
                    &mut candidates,
                    &mut seen,
                    workspace,
                    target_pkg,
                    !internal,
                    &format!("{pkg_raw}{sep}"),
                    &stem_upper,
                );
                append_treelist_completions(
                    &mut candidates,
                    &mut seen,
                    workspace,
                    target_pkg,
                    !internal,
                    &format!("{pkg_raw}{sep}"),
                    &stem_upper,
                );
            }
        }
        return candidates;
    }

    let upper_prefix = prefix_text.to_ascii_uppercase();

    // 1. Collect in-scope lexical bindings at `offset` (works inside unclosed forms too).
    for binding in collect_scope_at(source, root, offset, workspace)
        .into_iter()
        .rev()
    {
        let namespace = match binding.namespace {
            Namespace::SpecialValue => Namespace::Value,
            namespace => namespace,
        };
        if binding.spelling.name.starts_with(&upper_prefix)
            && seen.insert((namespace, binding.spelling.name.clone()))
        {
            let detail = match (&binding.namespace, &binding.params) {
                (Namespace::Function | Namespace::SetfFunction, Some(params)) => Some(format!(
                    "({} {})",
                    binding.spelling.name.to_ascii_lowercase(),
                    params.join(" ")
                )),
                (Namespace::SpecialValue, _) => Some("special".into()),
                _ => Some("lexical".into()),
            };
            candidates.push(CompletionCandidate {
                label: binding.spelling.name.to_ascii_lowercase(),
                kind: match binding.namespace {
                    Namespace::Function | Namespace::SetfFunction => CompletionKind::Function,
                    Namespace::Value | Namespace::SpecialValue => CompletionKind::Variable,
                    Namespace::Pattern => CompletionKind::Macro,
                    Namespace::Block
                    | Namespace::ImplicitBlock
                    | Namespace::Tag
                    | Namespace::IntegerTag => CompletionKind::Keyword,
                    Namespace::Type => CompletionKind::Type,
                },
                detail,
                documentation: None,
            });
        }
    }

    // 2. Collect workspace declarations accessible in `current_pkg`.
    let current_canonical = current_pkg.and_then(|p| workspace.canonical_global_package(p));
    for (_, doc_idx) in &workspace.documents {
        for decl in &doc_idx.declarations {
            if !decl.name.spelling.name.starts_with(&upper_prefix) {
                continue;
            }
            let Some(decl_pkg) = workspace.declaration_package(decl) else {
                continue;
            };
            let accessible = current_canonical.is_some_and(|cur| {
                workspace
                    .resolve_symbol_home_in_package(cur, &decl.name.spelling.name, false)
                    .is_some_and(|package| workspace.declaration_matches(package, decl))
            });
            if accessible && seen.insert((decl.name.namespace, decl.name.spelling.name.clone())) {
                let detail = decl.params.as_ref().map_or_else(
                    || Some(decl_pkg.to_owned()),
                    |p| {
                        Some(format!(
                            "({pkg}::{name} {plist})",
                            pkg = decl_pkg,
                            name = decl.name.spelling.name.to_ascii_lowercase(),
                            plist = p.join(" ")
                        ))
                    },
                );
                candidates.push(CompletionCandidate {
                    label: decl.name.spelling.name.to_ascii_lowercase(),
                    kind: match decl.name.namespace {
                        Namespace::Function | Namespace::SetfFunction => {
                            if decl.operator.name == "DEFMACRO" {
                                CompletionKind::Macro
                            } else {
                                CompletionKind::Function
                            }
                        }
                        Namespace::Value | Namespace::SpecialValue => {
                            if decl.operator.name == "DEFCONSTANT" {
                                CompletionKind::Constant
                            } else {
                                CompletionKind::Variable
                            }
                        }
                        Namespace::Type => CompletionKind::Type,
                        Namespace::Pattern => CompletionKind::Macro,
                        Namespace::Block
                        | Namespace::ImplicitBlock
                        | Namespace::Tag
                        | Namespace::IntegerTag => CompletionKind::Keyword,
                    },
                    detail,
                    documentation: decl.documentation.clone(),
                });
            }
        }
    }

    // 3. Collect workspace package names & nicknames.
    let mut seen_pkgs = HashSet::new();
    let local = current_canonical
        .and_then(|owner| workspace.package(owner))
        .into_iter()
        .flat_map(|pkg| pkg.local_nicknames.keys());
    let global = workspace
        .packages()
        .flat_map(|pkg| std::iter::once(&pkg.name).chain(&pkg.nicknames));
    for name in local
        .chain(global)
        .map(String::as_str)
        .chain([simd_builtins::PACKAGE, treelist_builtins::PACKAGE])
    {
        if name.starts_with(&upper_prefix)
            && seen_pkgs.insert(name)
            && let Some(target) = workspace.resolve_package_prefix(current_pkg, name)
        {
            candidates.push(CompletionCandidate {
                label: name.to_ascii_lowercase(),
                kind: CompletionKind::Package,
                detail: Some(format!("package {target}")),
                documentation: None,
            });
        }
    }

    if let Some(package) = current_canonical
        && source.reader_syntax_known_at(offset)
    {
        append_simd_completions(
            &mut candidates,
            &mut seen,
            workspace,
            package,
            false,
            "",
            &upper_prefix,
        );
        append_treelist_completions(
            &mut candidates,
            &mut seen,
            workspace,
            package,
            false,
            "",
            &upper_prefix,
        );
    }

    // 4. Collect ANSI Common Lisp external symbols.
    let known_package = current_canonical.filter(|pkg| workspace.can_resolve_package(pkg));
    push_cl_completion_candidates(
        &mut candidates,
        &upper_prefix,
        None,
        known_package,
        workspace,
    );

    candidates
}

fn push_cl_completion_candidates(
    candidates: &mut Vec<CompletionCandidate>,
    upper_prefix: &str,
    qualifier: Option<(&str, &str)>,
    known_package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) {
    for &cl_sym in crate::cl_builtins::external_names_with_prefix(upper_prefix) {
        if let Some(package) = known_package
            && simd_builtins::function(cl_sym).is_some()
            && workspace.resolve_symbol_home_in_package(package, cl_sym, false)
                != Some("COMMON-LISP")
        {
            continue;
        }
        let sig = builtin_signature(cl_sym);
        let builtin = sig.map(|s| s.kind);
        if builtin == Some(BuiltinKind::LambdaListKeyword) && upper_prefix.is_empty() {
            continue;
        }
        let kind = match builtin {
            Some(BuiltinKind::SpecialOperator) => CompletionKind::SpecialOperator,
            Some(BuiltinKind::Macro) => CompletionKind::Macro,
            Some(BuiltinKind::Function) => CompletionKind::Function,
            Some(BuiltinKind::Variable) => CompletionKind::Variable,
            Some(BuiltinKind::Constant) => CompletionKind::Constant,
            Some(BuiltinKind::Type) => CompletionKind::Type,
            Some(BuiltinKind::Declaration | BuiltinKind::LambdaListKeyword) => {
                CompletionKind::Keyword
            }
            None => CompletionKind::Function,
        };
        let lower = cl_sym.to_ascii_lowercase();
        let (label, detail) = match qualifier {
            Some((pkg_raw, sep)) => (
                format!("{pkg_raw}{sep}{lower}"),
                Some("COMMON-LISP".to_owned()),
            ),
            None => {
                let detail = sig.map(|s| {
                    if s.params.is_empty() {
                        format!("(cl:{lower})")
                    } else {
                        format!("(cl:{lower} {})", s.params.join(" "))
                    }
                });
                (lower, detail)
            }
        };
        let documentation = sig.and_then(|s| (!s.summary.is_empty()).then(|| s.summary.to_owned()));
        candidates.push(CompletionCandidate {
            label,
            kind,
            detail,
            documentation,
        });
    }
}

fn append_simd_completions(
    candidates: &mut Vec<CompletionCandidate>,
    seen: &mut HashSet<(Namespace, String)>,
    workspace: &WorkspaceIndex<'_>,
    package: &str,
    external: bool,
    qualifier: &str,
    stem: &str,
) {
    let functions = simd_builtins::FUNCTIONS.iter().map(|function| {
        (
            function.name,
            Some(function.lambda_list),
            function.documentation,
        )
    });
    let types = simd_builtins::types()
        .filter_map(|name| simd_builtins::type_documentation(name).map(|doc| (name, None, doc)));
    for (name, lambda_list, doc) in functions.chain(types) {
        if !name.starts_with(stem)
            || workspace.resolve_symbol_home_in_package(package, name, external)
                != Some(simd_builtins::PACKAGE)
        {
            continue;
        }
        let (namespace, kind) = if lambda_list.is_some() {
            (Namespace::Function, CompletionKind::Function)
        } else {
            (Namespace::Type, CompletionKind::Type)
        };
        if seen.insert((namespace, name.to_owned())) {
            let name = name.to_ascii_lowercase();
            let detail = match lambda_list {
                Some(params) => format!("(zk-simd:{name} {params})"),
                None => format!("zk-simd:{name}"),
            };
            candidates.push(CompletionCandidate {
                label: format!("{qualifier}{name}"),
                kind,
                detail: Some(detail),
                documentation: Some(doc.to_owned()),
            });
        }
    }
}

fn append_treelist_completions(
    candidates: &mut Vec<CompletionCandidate>,
    seen: &mut HashSet<(Namespace, String)>,
    workspace: &WorkspaceIndex<'_>,
    package: &str,
    external: bool,
    qualifier: &str,
    stem: &str,
) {
    for entry in treelist_builtins::CALLABLES {
        if !entry.name.starts_with(stem) {
            continue;
        }
        let home = workspace.resolve_symbol_home_in_package(package, entry.name, external);
        let in_treelist = home == Some(treelist_builtins::PACKAGE);
        let in_zk = home == Some("ZK")
            && matches!(
                entry.name,
                "TREELIST" | "MUTABLE-TREELIST" | "TREELIST-P" | "MUTABLE-TREELIST-P"
            );
        if !in_treelist && !in_zk {
            continue;
        }
        if seen.insert((Namespace::Function, entry.name.to_owned())) {
            let lower = entry.name.to_ascii_lowercase();
            let detail = if entry.lambda_list.is_empty() {
                format!("(zk-treelist:{lower})")
            } else {
                format!("(zk-treelist:{lower} {})", entry.lambda_list)
            };
            candidates.push(CompletionCandidate {
                label: format!("{qualifier}{lower}"),
                kind: entry.kind.into(),
                detail: Some(detail),
                documentation: Some(entry.documentation.to_owned()),
            });
        }
    }
}

fn eval_when_situation_completions(
    root: &SyntaxNode,
    offset: TextSize,
    prefix_text: &str,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> Option<Vec<CompletionCandidate>> {
    let sit_list = find_enclosing_list(root, offset, false)?;
    let eval_when_list = sit_list.parent()?;
    if eval_when_list.kind() != SyntaxKind::List {
        return None;
    }
    let mut ew_elems = elements(&eval_when_list);
    let head = ew_elems.next()?;
    let head_sym = symbol(&head)?;
    if workspace.standard_symbol(&head_sym, package) != Some("EVAL-WHEN") {
        return None;
    }
    let second = ew_elems.next()?;
    if second.text_range() != sit_list.text_range() {
        return None;
    }
    let filter = prefix_text.trim_start_matches(':').to_ascii_uppercase();
    let mut items = Vec::new();
    for (label, summary) in [
        (
            ":compile-toplevel",
            "Evaluate body forms at compile time when processed as a top-level form by COMPILE-FILE.",
        ),
        (
            ":load-toplevel",
            "Compile body forms as top-level forms to be executed when the compiled file is loaded.",
        ),
        (
            ":execute",
            "Evaluate body forms when EVAL-WHEN is executed as code.",
        ),
    ] {
        let stem = label.trim_start_matches(':').to_ascii_uppercase();
        if stem.starts_with(&filter) {
            items.push(CompletionCandidate {
                label: label.to_owned(),
                kind: CompletionKind::Keyword,
                detail: Some("eval-when situation".into()),
                documentation: Some(summary.to_owned()),
            });
        }
    }
    Some(items)
}

fn call_keyword_param_completions(
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> Vec<(String, String, Option<String>)> {
    let Some(call_list) = find_enclosing_list(root, offset, false) else {
        return Vec::new();
    };
    if !non_dotted_list(&call_list) {
        return Vec::new();
    }
    let Some((head_sym, _, _)) = call_head(&call_list, source.known_features()) else {
        return Vec::new();
    };
    let lexicals = collect_scope_at(source, root, offset, workspace);
    let local_fn = find_local_fn(&lexicals, &head_sym, package, workspace);
    let Some(spec) = workspace.resolve_callable(&head_sym, package, local_fn) else {
        return Vec::new();
    };
    let mut in_key = false;
    let mut out = Vec::new();
    for param in spec.lambda_list() {
        let upper = param.to_ascii_uppercase();
        if upper == "&KEY" {
            in_key = true;
            continue;
        }
        if upper.starts_with('&') {
            if upper != "&ALLOW-OTHER-KEYS" {
                in_key = false;
            }
            continue;
        }
        if !in_key {
            continue;
        }
        let clean = param
            .trim_start_matches('(')
            .split_whitespace()
            .next()
            .unwrap_or(param)
            .trim_end_matches(')')
            .trim_start_matches(':')
            .to_ascii_uppercase();
        if !clean.is_empty() {
            out.push((
                clean,
                format!("&key parameter of {}", spec.name.to_ascii_lowercase()),
                spec.documentation.map(str::to_owned),
            ));
        }
    }
    out
}

pub(crate) fn semantic_tokens<'a>(
    source_doc: &str,
    source: &DocumentIndex,
    root: &SyntaxNode,
    documents: impl IntoIterator<Item = (&'a str, &'a DocumentIndex)>,
) -> Vec<SemanticToken> {
    let documents: Vec<_> = documents.into_iter().collect();
    let declarations = Arc::new(
        WorkspaceDeclarations::collect(&documents, &|| false)
            .expect("synchronous navigation query cannot be cancelled"),
    );
    let workspace = WorkspaceIndex::new(documents).with_declarations(declarations);
    let mut resolver = OccurrenceResolver::new(source, root, &workspace, None);
    let mut tokens = Vec::new();
    for item in root.descendants_with_tokens() {
        let Some(token) = item.into_token() else {
            continue;
        };
        let range = token.text_range();
        if !source.reader_syntax_known_at(range.start()) {
            break;
        }
        let kind = match token.kind() {
            SyntaxKind::LineComment | SyntaxKind::BlockComment => {
                Some((SemanticTokenKind::Comment, false))
            }
            SyntaxKind::String | SyntaxKind::RawString | SyntaxKind::Character => {
                Some((SemanticTokenKind::String, false))
            }
            SyntaxKind::Quote
            | SyntaxKind::Backquote
            | SyntaxKind::Unquote
            | SyntaxKind::UnquoteSplicing
            | SyntaxKind::UnquoteDestructive
            | SyntaxKind::FunctionQuote
            | SyntaxKind::DispatchPrefix
            | SyntaxKind::ReaderConditional => Some((SemanticTokenKind::Operator, false)),
            SyntaxKind::Atom => classify_atom_token(source_doc, &token, &workspace, &mut resolver),
            _ => None,
        };
        if let Some((kind, is_definition)) = kind {
            tokens.push(SemanticToken {
                range,
                kind,
                is_definition,
            });
        }
    }
    tokens
}

fn classify_atom_token(
    source_doc: &str,
    token: &crate::outline::Token,
    workspace: &WorkspaceIndex<'_>,
    resolver: &mut OccurrenceResolver<'_, '_>,
) -> Option<(SemanticTokenKind, bool)> {
    let text = token.text();
    if is_numeric_token(text) || is_radix_number_token(text) || is_bit_vector_token(text) {
        return Some((SemanticTokenKind::Number, false));
    }
    let sym = SymbolName::parse(text)?;
    if sym.prefix == Prefix::Keyword || sym.name.starts_with('&') {
        return Some((SemanticTokenKind::Keyword, false));
    }
    if let Some(binding) = resolver.resolve(token) {
        match binding {
            OccurrenceBinding::LocalSite {
                def_range,
                namespace,
                ..
            } => {
                let is_def = def_range == token.text_range();
                let kind = match namespace {
                    Namespace::Function | Namespace::SetfFunction => SemanticTokenKind::Function,
                    Namespace::Value => SemanticTokenKind::Parameter,
                    Namespace::SpecialValue => SemanticTokenKind::Variable,
                    Namespace::Type => SemanticTokenKind::Type,
                    Namespace::Pattern => SemanticTokenKind::Macro,
                    Namespace::ImplicitBlock if is_def => SemanticTokenKind::Macro,
                    Namespace::Block | Namespace::ImplicitBlock | Namespace::Tag => {
                        SemanticTokenKind::Keyword
                    }
                    Namespace::IntegerTag => SemanticTokenKind::Number,
                };
                return Some((kind, is_def));
            }
            OccurrenceBinding::Global { name, .. } => {
                if let Some((document, decl)) = workspace.lookup_unique_declaration(&name) {
                    let is_def = document == source_doc && decl.range == token.text_range();
                    let kind = match decl.name.namespace {
                        Namespace::Function | Namespace::SetfFunction => {
                            if decl.operator.name == "DEFMACRO" {
                                SemanticTokenKind::Macro
                            } else {
                                SemanticTokenKind::Function
                            }
                        }
                        Namespace::Value | Namespace::SpecialValue => SemanticTokenKind::Variable,
                        Namespace::Type => SemanticTokenKind::Type,
                        Namespace::Pattern => SemanticTokenKind::Macro,
                        Namespace::Block | Namespace::ImplicitBlock | Namespace::Tag => {
                            SemanticTokenKind::Variable
                        }
                        Namespace::IntegerTag => SemanticTokenKind::Number,
                    };
                    return Some((kind, is_def));
                }
            }
            OccurrenceBinding::LocalWithoutSite => {}
        }
    }
    if let Some(std_op) = sym.standard_operator() {
        return match builtin_kind(std_op) {
            Some(
                BuiltinKind::SpecialOperator
                | BuiltinKind::Declaration
                | BuiltinKind::LambdaListKeyword,
            ) => Some((SemanticTokenKind::Keyword, false)),
            Some(BuiltinKind::Macro) => Some((SemanticTokenKind::Macro, false)),
            Some(BuiltinKind::Function) => Some((SemanticTokenKind::Function, false)),
            Some(BuiltinKind::Type) => Some((SemanticTokenKind::Type, false)),
            Some(BuiltinKind::Variable | BuiltinKind::Constant) => {
                Some((SemanticTokenKind::Variable, false))
            }
            None => None,
        };
    }
    None
}

fn collect_scope_at(
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    workspace: &WorkspaceIndex<'_>,
) -> Vec<ScopeBinding> {
    collect_scope_with(source, offset, workspace, |range| {
        find_top_level_form(root, range)
    })
}

pub(crate) fn collect_scope_with(
    source: &DocumentIndex,
    offset: TextSize,
    workspace: &WorkspaceIndex<'_>,
    find_form: impl FnOnce(TextRange) -> Option<Element>,
) -> Vec<ScopeBinding> {
    let mut lexicals = Vec::new();
    if let Some(form) = source.top_level_form_at(offset)
        && let Some(element) = find_form(form.range)
    {
        let context = ScopeAtOffset {
            package: source.package_at(offset),
            offset,
            workspace,
            known_features: source.known_features(),
            macro_operands: None,
        };
        lexicals = context.top_level_specials(&element);
        context.collect_lexical_bindings(element, &mut lexicals);
    }
    lexicals
}

pub(crate) fn find_local_fn<'a>(
    lexicals: &'a [ScopeBinding],
    head_sym: &SymbolName,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> Option<&'a ScopeBinding> {
    let ctx = ScopeAtOffset {
        package,
        offset: 0.into(),
        workspace,
        known_features: None,
        macro_operands: None,
    };
    lexicals
        .iter()
        .rev()
        .find(|b| b.namespace == Namespace::Function && ctx.same_symbol(b, head_sym))
}

pub(crate) fn candidate_symbol_tokens(
    root: &SyntaxNode,
) -> impl Iterator<Item = crate::outline::Token> {
    root.descendants_with_tokens().filter_map(|el| {
        let tok = el.into_token()?;
        matches!(
            tok.kind(),
            SyntaxKind::Atom | SyntaxKind::String | SyntaxKind::RawString
        )
        .then_some(tok)
    })
}
