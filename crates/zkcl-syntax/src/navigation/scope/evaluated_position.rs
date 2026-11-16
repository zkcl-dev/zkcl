use rowan::Language;

use crate::navigation::index::{ReaderConditional, reader_conditional};
use crate::outline::{elements, is_element_kind};
use crate::syntax::{LispLanguage, SyntaxKind, SyntaxNode};

use super::{Prefix, operator, symbol};

// Only count as many preceding operands as the operator rule distinguishes.
fn element_position(parent: &SyntaxNode, child_index: usize, limit: usize) -> usize {
    parent
        .green()
        .children()
        .take(child_index)
        .filter(|element| is_element_kind(LispLanguage::kind_from_raw(element.kind())))
        .take(limit)
        .count()
}

/// Returns whether `list` occurs where the enclosing forms evaluate a call.
pub(crate) fn is_evaluated_call_position(
    list: &SyntaxNode,
    known_features: Option<&[String]>,
) -> bool {
    let mut chain = Vec::new();
    let mut cur = list.clone();
    while let Some(parent) = cur.parent() {
        chain.push((parent.clone(), cur.index()));
        if parent.kind() == SyntaxKind::SourceFile {
            break;
        }
        cur = parent;
    }

    // Walk top-down from SourceFile to `list`, verifying every step permits expression evaluation.
    let mut quoted = 0usize;
    let mut idx = chain.len();
    while idx > 0 {
        idx -= 1;
        let (ref parent, child_index) = chain[idx];
        match parent.kind() {
            SyntaxKind::SourceFile => {}
            SyntaxKind::Vector => return false,
            SyntaxKind::ReaderForm => match parent.first_token().map(|t| t.kind()) {
                Some(SyntaxKind::ReaderConditional) => {
                    let Some(ReaderConditional::Active(guarded)) =
                        reader_conditional(&parent.clone().into(), known_features)
                    else {
                        return false;
                    };
                    if guarded.index() != child_index {
                        return false;
                    }
                }
                Some(
                    SyntaxKind::Quote | SyntaxKind::FunctionQuote | SyntaxKind::DispatchPrefix,
                ) => {
                    return false;
                }
                Some(SyntaxKind::Backquote) => quoted += 1,
                Some(
                    SyntaxKind::Unquote
                    | SyntaxKind::UnquoteSplicing
                    | SyntaxKind::UnquoteDestructive,
                ) => {
                    quoted = quoted.saturating_sub(1);
                }
                _ => {}
            },
            SyntaxKind::List => {
                if quoted > 0 {
                    // An unquote farther down the path can restore evaluation.
                    continue;
                }
                let position = |limit| element_position(parent, child_index, limit);
                if let Some(op) = operator(parent) {
                    match op.as_str() {
                        "QUOTE" | "DECLARE" | "DECLAIM" | "PROCLAIM" | "DEFPACKAGE"
                        | "DEFGENERIC" | "LOOP" => return false,
                        "DEFCLASS" | "DEFINE-CONDITION" => {
                            // Only `:initform <expr>` inside a slot spec is an evaluated call context
                            if position(4) != 3 || idx <= 1 {
                                return false;
                            }
                            idx -= 2;
                            let (ref slot_node, slot_child_index) = chain[idx];
                            if slot_node.kind() != SyntaxKind::List {
                                return false;
                            }
                            let slot_parts: Vec<_> = elements(slot_node).collect();
                            let Some(slot_pos) = slot_parts
                                .iter()
                                .position(|el| el.index() == slot_child_index)
                            else {
                                return false;
                            };
                            if slot_pos < 2 || !slot_pos.is_multiple_of(2) {
                                return false;
                            }
                            let Some(kw) = symbol(&slot_parts[slot_pos - 1]) else {
                                return false;
                            };
                            if kw.prefix != Prefix::Keyword || kw.name != "INITFORM" {
                                return false;
                            }
                        }
                        "DEFSTRUCT" => {
                            // In `(defstruct name (slot initform :type ...))` only `initform` (pos 1 of slot spec) is evaluated
                            if position(2) <= 1 || idx == 0 {
                                return false;
                            }
                            idx -= 1;
                            let (ref slot_node, slot_child_index) = chain[idx];
                            if slot_node.kind() != SyntaxKind::List {
                                return false;
                            }
                            let slot_pos = element_position(slot_node, slot_child_index, 2);
                            if slot_pos != 1 {
                                return false;
                            }
                        }
                        "DEFUN"
                        | "DEFMACRO"
                        | "DEFTYPE"
                        | "DEFINE-COMPILER-MACRO"
                        | "DEFINE-SETF-EXPANDER" => {
                            if position(3) <= 2 {
                                return false;
                            }
                        }
                        "DEFMETHOD" => {
                            // Name (pos 1), qualifiers, and specialized lambda list are non-calls.
                            let lambda_pos = 2 + elements(parent)
                                .skip(2)
                                .take_while(crate::outline::is_defmethod_qualifier)
                                .count();
                            if position(lambda_pos + 1) <= lambda_pos {
                                return false;
                            }
                        }
                        "EVAL-WHEN" => {
                            if position(2) <= 1 {
                                return false;
                            }
                        }
                        "LAMBDA"
                        | "DESTRUCTURING-BIND"
                        | "MULTIPLE-VALUE-BIND"
                        | "MULTIPLE-VALUE-SETQ" => {
                            if position(2) <= 1 {
                                return false;
                            }
                        }
                        "CHECK-TYPE" => {
                            if position(3) == 2 {
                                return false;
                            }
                        }
                        "THE" => {
                            if position(2) == 1 {
                                return false;
                            }
                        }
                        "LET" | "LET*" | "DO" | "DO*" | "PROG" | "PROG*" | "FLET" | "LABELS"
                        | "MACROLET" | "SYMBOL-MACROLET" | "HANDLER-BIND" | "RESTART-BIND" => {
                            if position(2) == 1 {
                                // `child` is the bindings/defs list itself; `chain[idx - 1]` is `(bindings, pair)`.
                                if idx <= 1 {
                                    return false;
                                }
                                idx -= 2;
                                let (ref binding_pair, pair_child_index) = chain[idx];
                                if binding_pair.kind() != SyntaxKind::List {
                                    return false;
                                }
                                let pair_pos = element_position(binding_pair, pair_child_index, 2);
                                match op.as_str() {
                                    "LET" | "LET*" | "DO" | "DO*" | "PROG" | "PROG*"
                                    | "HANDLER-BIND" | "RESTART-BIND" | "SYMBOL-MACROLET"
                                        if pair_pos == 0 =>
                                    {
                                        return false;
                                    }
                                    "FLET" | "LABELS" | "MACROLET" if pair_pos <= 1 => {
                                        return false;
                                    }
                                    _ => {}
                                }
                            } else if matches!(op.as_str(), "DO" | "DO*")
                                && idx == 0
                                && position(3) == 2
                            {
                                return false;
                            }
                        }
                        "DOLIST"
                        | "DOTIMES"
                        | "DO-SYMBOLS"
                        | "DO-EXTERNAL-SYMBOLS"
                        | "DO-ALL-SYMBOLS"
                        | "WITH-OPEN-FILE"
                        | "WITH-OPEN-STREAM"
                        | "WITH-INPUT-FROM-STRING"
                        | "WITH-OUTPUT-TO-STRING"
                        | "WITH-PACKAGE-ITERATOR"
                        | "WITH-HASH-TABLE-ITERATOR"
                        | "WITH-SIMPLE-RESTART" => {
                            if position(2) == 1 {
                                if idx == 0 {
                                    return false;
                                }
                                // Inside `(var expr ...)`, pos 0 is variable/restart/iterator name
                                let (ref clause, child_index) = chain[idx - 1];
                                let inner_pos = element_position(clause, child_index, 1);
                                if inner_pos == 0 {
                                    return false;
                                }
                            }
                        }
                        "PRINT-UNREADABLE-OBJECT"
                        | "PPRINT-LOGICAL-BLOCK"
                        | "WITH-COMPILATION-UNIT" => {
                            if position(2) == 1 && idx == 0 {
                                return false;
                            }
                        }
                        "WITH-SLOTS" | "WITH-ACCESSORS" => {
                            if position(2) == 1 {
                                return false;
                            }
                        }
                        "COND" => {
                            if idx == 0 && position(1) >= 1 {
                                return false;
                            }
                        }
                        "CASE" | "ECASE" | "CCASE" | "TYPECASE" | "ETYPECASE" | "CTYPECASE" => {
                            if position(2) >= 2 {
                                if idx == 0 {
                                    return false;
                                }
                                let (ref clause, child_index) = chain[idx - 1];
                                let inner_pos = element_position(clause, child_index, 1);
                                if inner_pos == 0 {
                                    return false;
                                }
                            }
                        }
                        "HANDLER-CASE" | "RESTART-CASE" if position(2) >= 2 => {
                            if idx == 0 {
                                return false;
                            }
                            let (ref clause, child_index) = chain[idx - 1];
                            let inner_pos = element_position(clause, child_index, 2);
                            if inner_pos <= 1 {
                                return false;
                            }
                        }
                        _ => {}
                    }
                } else if idx == 0 && position(1) == 0 {
                    // `((lambda (x) ...) arg)`: check the lambda's body only.
                    return false;
                }
            }
            _ => {}
        }
    }
    quoted == 0
}
