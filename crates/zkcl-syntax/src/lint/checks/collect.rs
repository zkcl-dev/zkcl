//! Recognize a complete list-building loop behind an append-in-loop warning.

use crate::format::TextEdit;
use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::SyntaxNode;

use super::super::context::is_nil_element;
use super::super::{FixApplicability, LintFix};
use super::{ScopeContext, dataflow::element_mentions_symbol};

pub(super) fn collect_instead_of_append(
    scope: ScopeContext<'_, '_>,
    assignment: &SyntaxNode,
    accumulation: &SyntaxNode,
    target: &SymbolName,
) -> Option<LintFix> {
    let doc = scope.doc;
    let workspace = doc.workspace;
    let package = scope.package;
    if target.prefix != Prefix::None
        || scope.env.symbol_macros
        || !scope.known_package(assignment.text_range())
        || workspace.is_proclaimed_special(target, package)
    {
        return None;
    }

    let (op, accumulation_args) = scope.standard_call(accumulation)?;
    let [previous, singleton] = accumulation_args.as_slice() else {
        return None;
    };
    if !matches!(op.as_str(), "APPEND" | "NCONC") || symbol(previous).as_ref() != Some(target) {
        return None;
    }
    let (op, singleton_args) = scope.standard_call(singleton.as_node()?)?;
    let [value] = singleton_args.as_slice() else {
        return None;
    };
    if op != "LIST" || element_mentions_symbol(value, &target.name) {
        return None;
    }

    let iteration = assignment.parent()?;
    let (op, iteration_args) = scope.standard_call(&iteration)?;
    let [spec, statement] = iteration_args.as_slice() else {
        return None;
    };
    if op != "DOLIST" || statement.text_range() != assignment.text_range() {
        return None;
    }
    let spec = doc.read_elements(elements(spec.as_node().filter(|node| closed_list(node))?))?;
    let [variable, source, result @ ..] = spec.as_slice() else {
        return None;
    };
    let item = symbol(variable)?;
    if item.prefix != Prefix::None
        || item == *target
        || workspace.is_proclaimed_special(&item, package)
        || element_mentions_symbol(source, &target.name)
        || element_mentions_symbol(source, &item.name)
    {
        return None;
    }

    let binding_form = iteration.parent()?;
    let (op, binding_args) = scope.standard_call(&binding_form)?;
    if !matches!(op.as_str(), "LET" | "LET*") {
        return None;
    }
    let [bindings, loop_form, tail @ ..] = binding_args.as_slice() else {
        return None;
    };
    if loop_form.text_range() != iteration.text_range() {
        return None;
    }
    let has_returned_accumulator = match (result, tail) {
        ([], [last]) => symbol(last).as_ref() == Some(target),
        ([result], []) => symbol(result).as_ref() == Some(target),
        ([result], [last]) => {
            is_nil_element(result, package, workspace) && symbol(last).as_ref() == Some(target)
        }
        _ => false,
    };
    if !has_returned_accumulator {
        return None;
    }

    let bindings = bindings.as_node().filter(|node| closed_list(node))?;
    let [binding] = <[Element; 1]>::try_from(doc.read_elements(elements(bindings))?).ok()?;
    let initialized_to_nil = if let Some(pair) = binding.as_node().filter(|node| closed_list(node))
    {
        let parts = doc.read_elements(elements(pair))?;
        match parts.as_slice() {
            [name] => symbol(name).as_ref() == Some(target),
            [name, initial] => {
                symbol(name).as_ref() == Some(target) && is_nil_element(initial, package, workspace)
            }
            _ => false,
        }
    } else {
        symbol(&binding).as_ref() == Some(target)
    };
    if !initialized_to_nil {
        return None;
    }

    let loop_op = scope.standard_spelling("loop")?;
    let continuation = scope.continuation(&binding_form, loop_op.len() + 2);
    let replacement = format!(
        "({loop_op} for {} in {}{continuation}collect {})",
        doc.text_of(variable.text_range()),
        doc.text_of(source.text_range()),
        doc.text_of(value.text_range()),
    );
    Some(LintFix {
        title: "Collect results with `loop` in a single pass".into(),
        // DOLIST permits different iteration-variable capture behavior, and
        // calls in either operand may observe unseen special proclamations.
        applicability: FixApplicability::MaybeIncorrect,
        edits: vec![TextEdit {
            range: binding_form.text_range(),
            replacement,
        }],
    })
}
