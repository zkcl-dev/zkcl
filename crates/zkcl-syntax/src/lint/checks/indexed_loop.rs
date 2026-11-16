//! List traversal driven by a lexical DOTIMES counter.

use crate::format::TextEdit;
use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

use super::super::context::{IndexedListLoop, LintEmitter, is_nil_element};
use super::super::{FixApplicability, LintFix};
use super::dataflow::{element_mentions_symbol, lexical_definition};
use super::{CallSite, ScopeContext, is_declaration};

pub(super) fn check(scope: ScopeContext<'_, '_>, call: CallSite<'_>, emit: &mut LintEmitter<'_>) {
    if scope.env.loop_bindings.is_empty() || scope.env.symbol_macros {
        return;
    }
    let [index, input] = call.args else {
        return;
    };
    let (Some(index_name), Some(input_name)) = (symbol(index), symbol(input)) else {
        return;
    };
    let doc = scope.doc;
    for binding in scope.env.loop_bindings.iter().rev() {
        let Some(candidate) = binding.indexed_list.as_deref() else {
            continue;
        };
        // Equal spellings are insufficient inside nested LETs and loops.
        if binding.name.name != index_name.name
            || candidate.source.name != input_name.name
            || lexical_definition(doc, index) != Some(binding.definition)
            || lexical_definition(doc, input) != Some(candidate.source_definition)
        {
            continue;
        }
        let range = call.list.text_range();
        let fix = candidate
            .rewrite
            .as_ref()
            .filter(|(initializer, _)| *initializer == range)
            .map(|(_, fix)| fix.clone());
        if let Some(diag) = emit.diagnostic(
            "perf/indexed-list-loop",
            range,
            "`nth` walks from the head on every iteration, which can make this loop quadratic",
            Some("walk the list directly with `dolist` or `loop ... for ... in` when the list stays stable".into()),
            Vec::new(),
            fix,
        ) {
            diag.note(
                candidate.length,
                "this list's length bounds the indexed loop",
            );
        }
        return;
    }
}

pub(super) fn describe(
    scope: ScopeContext<'_, '_>,
    iteration: &SyntaxNode,
    bounds: &[Element],
    body: &[Element],
    counter: &SymbolName,
) -> Option<IndexedListLoop> {
    let [length, result @ ..] = bounds else {
        return None;
    };
    let (op, args) = scope.standard_call(length.as_node()?)?;
    let [source] = args.as_slice() else {
        return None;
    };
    if op != "LENGTH" || scope.env.symbol_macros || !scope.known_package(iteration.text_range()) {
        return None;
    }
    let source_name = symbol(source)?;
    let source_definition = lexical_definition(scope.doc, source)?;
    let rewrite = loop_fix(
        scope,
        iteration,
        body,
        result,
        counter,
        source,
        &source_name,
    );
    Some(IndexedListLoop {
        source: source_name,
        source_definition,
        length: length.text_range(),
        rewrite,
    })
}

fn list_fields(scope: ScopeContext<'_, '_>, element: &Element) -> Option<Vec<Element>> {
    let node = element.as_node().filter(|node| closed_list(node))?;
    scope.doc.read_elements(elements(node))
}

fn loop_fix(
    scope: ScopeContext<'_, '_>,
    iteration: &SyntaxNode,
    body: &[Element],
    result: &[Element],
    counter: &SymbolName,
    source: &Element,
    source_name: &SymbolName,
) -> Option<(TextRange, LintFix)> {
    let doc = scope.doc;
    let package = scope.package;
    if result
        .first()
        .is_some_and(|result| !is_nil_element(result, package, doc.workspace))
    {
        return None;
    }
    let [statement] = body else {
        return None;
    };
    let (op, let_args) = scope.standard_call(statement.as_node()?)?;
    let [bindings, statements @ ..] = let_args.as_slice() else {
        return None;
    };
    if !matches!(op.as_str(), "LET" | "LET*") || statements.is_empty() {
        return None;
    }
    let [binding] = <[Element; 1]>::try_from(list_fields(scope, bindings)?).ok()?;
    let [item, initializer] = <[Element; 2]>::try_from(list_fields(scope, &binding)?).ok()?;
    let (op, operands) = scope.standard_call(initializer.as_node()?)?;
    let [index, input] = operands.as_slice() else {
        return None;
    };
    if op != "NTH"
        || symbol(index).as_ref() != Some(counter)
        || symbol(input).as_ref() != Some(source_name)
    {
        return None;
    }
    let item_name = symbol(&item)?;
    if item_name.prefix != Prefix::None
        || item_name == *counter
        || doc.workspace.is_proclaimed_special(&item_name, package)
        || statements.iter().any(|statement| {
            statement.kind() != SyntaxKind::List
                || is_declaration(doc, statement, package)
                || element_mentions_symbol(statement, &counter.name)
                || element_mentions_symbol(statement, &source_name.name)
        })
    {
        return None;
    }

    let dolist = scope.standard_spelling("dolist")?;
    let indent = scope.continuation(iteration, 2);
    let mut replacement = format!(
        "({dolist} ({} {})",
        doc.text_of(item.text_range()),
        doc.text_of(source.text_range())
    );
    for statement in statements {
        replacement.push_str(&indent);
        replacement.push_str(doc.text_of(statement.text_range()));
    }
    replacement.push(')');
    Some((
        initializer.text_range(),
        LintFix {
            title: "Walk the list directly with `dolist`".into(),
            // The body may mutate the list or capture the iteration binding.
            applicability: FixApplicability::MaybeIncorrect,
            edits: vec![TextEdit {
                range: iteration.text_range(),
                replacement,
            }],
        },
    ))
}
