//! Rewrites for sequence consumers that do not need an intermediate result.

use crate::format::TextEdit;
use crate::navigation::index::{non_dotted_list, symbol};
use crate::navigation::{Namespace, OccurrenceBinding};
use crate::outline::{Element, elements};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

use super::super::context::{
    FormContext, LintDocument, LintEmitter, integer_literal_value, is_function_quote,
    is_nil_element, is_nil_syntax, is_pure_atom, numeric_literal_is_zero, string_literal_value,
    unwrap_quote,
};
use super::super::{FixApplicability, LintFix};
use super::{CallSite, ScopeContext, replace_around};

pub(super) fn check_length_emptiness(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    op: &str,
    emit: &mut LintEmitter<'_>,
) {
    const RULE: &str = "perf/length-zero-comparison";
    if !emit.is_enabled(RULE) {
        return;
    }
    let zero = |el: &Element| {
        if op == "EQL" {
            integer_literal_value(el) == Some(0)
        } else {
            numeric_literal_is_zero(el)
        }
    };
    let length = |el: &Element| {
        super::complexity::unwrap_unshadowed_unary_call(
            scope.doc,
            el,
            "LENGTH",
            scope.package,
            scope.lexicals,
            scope.env,
        )
    };
    let input = match (op, call.args) {
        ("ZEROP" | "PLUSP", [arg]) => length(arg),
        ("=" | "EQL" | "<=" | ">", [left, right]) if zero(right) => length(left),
        ("=" | "EQL" | "<", [left, right]) if zero(left) => length(right),
        _ => None,
    };
    if let Some(input) = input
        && !is_nil_element(&input, scope.package, scope.doc.workspace)
        && (known_list(scope, &input) || has_list_type(scope, &input))
    {
        emit.emit(
            RULE,
            call.list.text_range(),
            "testing list emptiness with `length` can walk the entire list",
            Some("use `null` or `consp` for a proper list".into()),
            Vec::new(),
            None,
        );
    }
}

pub(super) fn record_list_declarations(
    doc: &LintDocument<'_>,
    declare: &Element,
    package: Option<&str>,
    env: &FormContext,
    body_env: &mut Option<FormContext>,
) {
    let Some(declare) = declare.as_node() else {
        return;
    };
    for spec in elements(declare).skip(1) {
        let Some(spec) = spec.as_node().filter(|node| non_dotted_list(node)) else {
            continue;
        };
        let Some(fields) = doc.read_elements(elements(spec)) else {
            continue;
        };
        let [head, rest @ ..] = fields.as_slice() else {
            continue;
        };
        let standard = |el: &Element| {
            symbol(el).and_then(|name| {
                doc.workspace
                    .standard_symbol(&name, package)
                    .map(str::to_owned)
            })
        };
        let vars = match (standard(head).as_deref(), rest) {
            (Some("TYPE"), [ty, vars @ ..])
                if matches!(standard(ty).as_deref(), Some("LIST" | "CONS")) =>
            {
                vars
            }
            (Some("LIST" | "CONS"), vars) => vars,
            _ => continue,
        };
        for var in vars {
            if let Some(OccurrenceBinding::LocalSite {
                namespace: Namespace::Value,
                spelling,
                def_range,
                ..
            }) =
                doc.source
                    .resolve_occurrence_at(doc.root, var.text_range().start(), doc.workspace)
            {
                let body_env = body_env.get_or_insert_with(|| env.clone());
                std::rc::Rc::make_mut(&mut body_env.list_declarations)
                    .push((spelling.name, def_range));
            }
        }
    }
}

fn has_list_type(scope: ScopeContext<'_, '_>, input: &Element) -> bool {
    if let Some(node) = input.as_node()
        && let Some((op, args)) = scope.standard_call(node)
        && op == "THE"
        && let [ty, _] = args.as_slice()
        && symbol(ty).is_some_and(|name| {
            matches!(
                scope.doc.workspace.standard_symbol(&name, scope.package),
                Some("LIST" | "CONS")
            )
        })
    {
        return true;
    }
    let declarations = &scope.env.list_declarations;
    if scope.env.symbol_macros || declarations.is_empty() {
        return false;
    }
    let Some(name) = symbol(input) else {
        return false;
    };
    declarations
        .iter()
        .any(|(spelling, _)| *spelling == name.name)
        && matches!(
            scope.doc.source.resolve_occurrence_at(scope.doc.root, input.text_range().start(), scope.doc.workspace),
            Some(OccurrenceBinding::LocalSite { namespace: Namespace::Value, def_range, .. })
                if declarations.iter().any(|(_, declaration)| *declaration == def_range)
        )
}

pub(super) fn check(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    op: &str,
    emit: &mut LintEmitter<'_>,
) {
    if !scope.known_package(call.list.text_range()) {
        return;
    }
    match (op, call.args) {
        ("LENGTH", [input]) => check_length(scope, call.list, input, emit),
        ("CAR" | "FIRST", [input]) => check_first(scope, call.list, input, emit),
        (
            "STRING=" | "STRING/=" | "STRING-EQUAL" | "STRING-NOT-EQUAL" | "STRING<" | "STRING<="
            | "STRING>" | "STRING>=",
            args,
        ) => {
            check_string_comparison_subseq(scope, op, call.list, args, emit);
            if matches!(op, "STRING=" | "STRING/=") {
                check_string_case_fold(scope, op, call.list, args, emit);
            }
        }
        ("EQUAL", args) => check_string_case_fold(scope, op, call.list, args, emit),
        ("MAPCAR" | "MAPLIST", [_, _, ..]) if scope.env.result_discarded => {
            let (from, to) = if op == "MAPCAR" {
                ("mapcar", "mapc")
            } else {
                ("maplist", "mapl")
            };
            let fix = elements(call.list).next().and_then(|head| {
                let to = scope.standard_spelling(to)?;
                Some(LintFix {
                    title: format!("Replace `{from}` with `{to}`"),
                    applicability: FixApplicability::MachineApplicable,
                    edits: vec![TextEdit {
                        range: head.text_range(),
                        replacement: to.into_owned(),
                    }],
                })
            });
            emit.emit(
                "perf/mapcar-result-discarded",
                call.list.text_range(),
                format!("`{from}` builds a list that this expression discards"),
                Some(format!(
                    "use `{to}` to keep the calls without collecting their results"
                )),
                Vec::new(),
                fix,
            );
        }
        ("MAP", [result_type, _, _, ..]) if scope.env.result_discarded => {
            let workspace = scope.doc.workspace;
            if unwrap_quote(result_type, scope.package, workspace)
                .and_then(|ty| symbol(&ty))
                .is_some_and(|ty| {
                    matches!(
                        workspace.standard_symbol(&ty, scope.package),
                        Some("LIST" | "VECTOR")
                    )
                })
            {
                let fix = scope.standard_spelling("nil").map(|nil| LintFix {
                    title: "Use `map` for side effects only".into(),
                    applicability: FixApplicability::MachineApplicable,
                    edits: vec![TextEdit {
                        range: result_type.text_range(),
                        replacement: nil.into_owned(),
                    }],
                });
                emit.emit(
                    "perf/map-result-discarded",
                    result_type.text_range(),
                    "`map` collects return values into a sequence that is discarded",
                    Some("pass `nil` as the result type to keep the calls without collecting results".into()),
                    Vec::new(),
                    fix,
                );
            }
        }
        _ => {}
    }
}

fn check_length(
    scope: ScopeContext<'_, '_>,
    list: &SyntaxNode,
    input: &Element,
    emit: &mut LintEmitter<'_>,
) {
    let Some(input_list) = input.as_node() else {
        return;
    };
    let Some((op, args)) = scope.standard_call(input_list) else {
        return;
    };
    match (op.as_str(), args.as_slice()) {
        ("REMOVE-IF-NOT", [predicate, _]) => {
            let applicability = if total_predicate(scope, predicate) {
                FixApplicability::MachineApplicable
            } else {
                FixApplicability::MaybeIncorrect
            };
            emit.emit(
                "perf/count-filtered-sequence",
                list.text_range(),
                "filtering this sequence just to count its elements can allocate an intermediate sequence",
                Some("`count-if` counts directly. Review predicates with side effects before changing the traversal.".into()),
                Vec::new(),
                fuse_call(scope, list, input, "count-if", applicability),
            );
        }
        ("COPY-SEQ" | "REVERSE", [source]) => {
            // All of these operations accept the same proper sequences and
            // preserve their lengths. The discarded copies have no consumer.
            let mut source = source.clone();
            while let Some((op, operands)) =
                source.as_node().and_then(|node| scope.standard_call(node))
                && matches!(op.as_str(), "COPY-SEQ" | "REVERSE")
                && let Ok([next]) = <[Element; 1]>::try_from(operands)
            {
                source = next;
            }
            if compares_original_length(scope, list, &source) {
                return;
            }
            let inner = source.text_range();
            emit.emit(
                "perf/length-of-sequence-copy",
                input.text_range(),
                "the copied sequence is used only for its length",
                Some("take the length of the original sequence directly".into()),
                Vec::new(),
                Some(LintFix {
                    title: "Take the length without copying the sequence".into(),
                    applicability: FixApplicability::MachineApplicable,
                    edits: replace_around(input_list, inner, String::new(), String::new()),
                }),
            );
        }
        _ => {}
    }
}

fn compares_original_length(
    scope: ScopeContext<'_, '_>,
    length: &SyntaxNode,
    source: &Element,
) -> bool {
    let Some(parent) = length.parent() else {
        return false;
    };
    let mut parts = elements(&parent);
    let Some(head) = parts.next().and_then(|el| symbol(&el)) else {
        return false;
    };
    if !matches!(
        scope
            .doc
            .std_op(&head, scope.package, scope.lexicals, scope.env),
        Some("=" | "EQL" | "EQUAL" | "EQUALP")
    ) {
        return false;
    }
    let other = match (parts.next(), parts.next(), parts.next()) {
        (Some(left), Some(right), None) if left.text_range() == length.text_range() => right,
        (Some(left), Some(right), None) if right.text_range() == length.text_range() => left,
        _ => return false,
    };
    let Some((op, args)) = other.as_node().and_then(|node| scope.standard_call(node)) else {
        return false;
    };
    // A property check can ask whether an operation preserves the length.
    // Collapsing both sides would leave a tautology and stop testing it.
    op == "LENGTH"
        && matches!(args.as_slice(), [original] if scope.doc.text_of(original.text_range()) == scope.doc.text_of(source.text_range()))
}

fn check_first(
    scope: ScopeContext<'_, '_>,
    list: &SyntaxNode,
    input: &Element,
    emit: &mut LintEmitter<'_>,
) {
    let Some((op, args)) = input.as_node().and_then(|node| scope.standard_call(node)) else {
        return;
    };
    if args.len() < 2 {
        return;
    }
    match op.as_str() {
        "REMOVE-IF-NOT" | "REMOVE-IF" => {
            let fix = if op == "REMOVE-IF-NOT" && let [predicate, source] = args.as_slice() {
                let applicability = if total_predicate(scope, predicate) && known_list(scope, source) {
                    FixApplicability::MachineApplicable
                } else {
                    FixApplicability::MaybeIncorrect
                };
                fuse_call(scope, list, input, "find-if", applicability)
            } else {
                None
            };
            emit.emit(
                "perf/car-of-remove-if-not",
                list.text_range(),
                "only the first element of a filtered list is used",
                Some(if op == "REMOVE-IF-NOT" {
                    "`find-if` can stop at the first match. Check filter options and any predicate effects.".into()
                } else {
                    "`find-if` with a complemented predicate can stop at the first retained element".into()
                }),
                Vec::new(),
                fix,
            );
        }
        "SORT" | "STABLE-SORT" => emit.emit(
            "perf/first-of-sort",
            list.text_range(),
            "the list is sorted when only its first element is used",
            Some("select the extremum in one pass if the reordering and comparator calls are unnecessary".into()),
            Vec::new(),
            None,
        ),
        _ => {}
    }
}

fn fuse_call(
    scope: ScopeContext<'_, '_>,
    outer: &SyntaxNode,
    inner: &Element,
    replacement: &'static str,
    applicability: FixApplicability,
) -> Option<LintFix> {
    let name = scope.standard_spelling(replacement)?;
    let head = elements(inner.as_node()?).next()?;
    Some(LintFix {
        title: format!("Use `{name}` directly"),
        applicability,
        // Leave the operands in place, including their formatting and comments.
        edits: vec![
            TextEdit {
                range: TextRange::new(outer.text_range().start(), head.text_range().end()),
                replacement: format!("({name}"),
            },
            TextEdit {
                range: TextRange::new(inner.text_range().end(), outer.text_range().end()),
                replacement: String::new(),
            },
        ],
    })
}

fn total_predicate(scope: ScopeContext<'_, '_>, predicate: &Element) -> bool {
    let name = if predicate.kind() == SyntaxKind::ReaderForm {
        is_function_quote(predicate, scope.package, scope.doc.workspace).and_then(|el| symbol(&el))
    } else {
        predicate
            .as_node()
            .and_then(|node| scope.standard_call(node))
            .filter(|(op, _)| op == "FUNCTION")
            .and_then(|(_, args)| <[Element; 1]>::try_from(args).ok())
            .and_then(|[el]| symbol(&el))
    };
    // These ANSI predicates cannot call user code or reject an object based on its type.
    name.and_then(|name| {
        scope
            .doc
            .std_op(&name, scope.package, scope.lexicals, scope.env)
            .map(|op| {
                matches!(
                    op,
                    "ATOM"
                        | "CONSP"
                        | "LISTP"
                        | "NULL"
                        | "IDENTITY"
                        | "SYMBOLP"
                        | "KEYWORDP"
                        | "NUMBERP"
                        | "INTEGERP"
                        | "RATIONALP"
                        | "FLOATP"
                        | "REALP"
                        | "COMPLEXP"
                        | "CHARACTERP"
                        | "STRINGP"
                        | "VECTORP"
                        | "ARRAYP"
                        | "BIT-VECTOR-P"
                        | "SIMPLE-VECTOR-P"
                        | "HASH-TABLE-P"
                        | "FUNCTIONP"
                        | "PACKAGEP"
                        | "PATHNAMEP"
                        | "STREAMP"
                )
            })
    }) == Some(true)
}

fn known_list(scope: ScopeContext<'_, '_>, source: &Element) -> bool {
    let workspace = scope.doc.workspace;
    let package = scope.package;
    if is_nil_syntax(source, package, workspace) {
        return true;
    }
    if let Some(value) = unwrap_quote(source, package, workspace) {
        return is_nil_syntax(&value, package, workspace)
            || value.as_node().is_some_and(non_dotted_list);
    }
    source
        .as_node()
        .and_then(|node| scope.standard_call(node))
        .is_some_and(|(op, args)| match (op.as_str(), args.as_slice()) {
            ("THE", [ty, _]) => symbol(ty).is_some_and(|ty| {
                matches!(
                    workspace.standard_symbol(&ty, package),
                    Some("LIST" | "NULL")
                )
            }),
            ("LIST", _) => true,
            ("MAPCAR" | "MAPLIST", [_, _, ..]) => true,
            _ => false,
        })
}

fn check_string_comparison_subseq(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("perf/string-comparison-subseq") || args.len() < 2 {
        return;
    }
    let subseq_call = |el: &Element| -> Option<(Element, Element, Option<Element>)> {
        let node = el.as_node()?;
        let (call_op, call_args) = scope.standard_call(node)?;
        if call_op != "SUBSEQ" || !(2..=3).contains(&call_args.len()) {
            return None;
        }
        Some((
            call_args[0].clone(),
            call_args[1].clone(),
            call_args.get(2).cloned(),
        ))
    };

    let s1 = subseq_call(&args[0]);
    let s2 = subseq_call(&args[1]);
    if s1.is_none() && s2.is_none() {
        return;
    }

    let (target_range, slot_num) = if s1.is_some() {
        (args[0].text_range(), 1)
    } else {
        (args[1].text_range(), 2)
    };

    let fix = if args.len() == 2 && !(s1.is_some() && s2.is_some()) {
        let (other, seq_el, start_el, end_el) = if let Some((seq, start, end)) = s1 {
            (&args[1], seq, start, end)
        } else if let Some((seq, start, end)) = s2 {
            (&args[0], seq, start, end)
        } else {
            return;
        };
        let order_safe = is_pure_atom(other)
            && is_pure_atom(&start_el)
            && end_el.as_ref().is_none_or(is_pure_atom);
        let head_kw = elements(list)
            .next()
            .map(|h| scope.doc.text_of(h.text_range()));
        let preserves_index = slot_num == 2
            || scope.env.in_boolean_test
            || scope.env.result_discarded
            || matches!(op, "STRING=" | "STRING-EQUAL")
            || integer_literal_value(&start_el) == Some(0);
        if order_safe
            && preserves_index
            && let Some(head_kw) = head_kw
        {
            let mut bounds = String::new();
            if integer_literal_value(&start_el) != Some(0) {
                bounds.push_str(&format!(
                    " :start{slot_num} {}",
                    scope.doc.text_of(start_el.text_range())
                ));
            }
            if let Some(end_el) = end_el {
                bounds.push_str(&format!(
                    " :end{slot_num} {}",
                    scope.doc.text_of(end_el.text_range())
                ));
            }
            let seq_src = scope.doc.text_of(seq_el.text_range());
            let other_src = scope.doc.text_of(other.text_range());
            let replacement = if slot_num == 1 {
                format!("({head_kw} {seq_src} {other_src}{bounds})")
            } else {
                format!("({head_kw} {other_src} {seq_src}{bounds})")
            };
            Some(LintFix {
                title: format!(
                    "Use `:start{slot_num}`/`:end{slot_num}` on `{}` instead of `subseq`",
                    op.to_ascii_lowercase()
                ),
                // Removing the copy can change when argument values are checked.
                applicability: FixApplicability::MaybeIncorrect,
                edits: vec![TextEdit {
                    range: list.text_range(),
                    replacement,
                }],
            })
        } else {
            None
        }
    } else {
        None
    };

    emit.emit(
        "perf/string-comparison-subseq",
        target_range,
        format!(
            "`subseq` allocates a temporary string before passing it to `{}`",
            op.to_ascii_lowercase()
        ),
        Some(
            "consider bounds keywords. A mismatch index is relative to the first original string"
                .into(),
        ),
        Vec::new(),
        fix,
    );
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum CaseFoldKind {
    Upper,
    Lower,
}

fn unwrap_case_fold(scope: ScopeContext<'_, '_>, el: &Element) -> Option<(CaseFoldKind, Element)> {
    let node = el.as_node()?;
    let (op, args) = scope.standard_call(node)?;
    if args.len() != 1 {
        return None;
    }
    match op.as_str() {
        "STRING-UPCASE" => Some((CaseFoldKind::Upper, args[0].clone())),
        "STRING-DOWNCASE" => Some((CaseFoldKind::Lower, args[0].clone())),
        _ => None,
    }
}

fn literal_matches_case(el: &Element, kind: CaseFoldKind) -> bool {
    let Some(text) = string_literal_value(el) else {
        return false;
    };
    match kind {
        CaseFoldKind::Upper => text.chars().all(|c| !c.is_lowercase()),
        CaseFoldKind::Lower => text.chars().all(|c| !c.is_uppercase()),
    }
}

fn check_string_case_fold(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("perf/string-equal-case-fold") || args.len() != 2 {
        return;
    }
    let f0 = unwrap_case_fold(scope, &args[0]);
    let f1 = unwrap_case_fold(scope, &args[1]);
    let pair = match (f0, f1) {
        (Some((k0, inner0)), Some((k1, inner1))) if k0 == k1 => Some((inner0, inner1)),
        (Some((k0, inner0)), None) if literal_matches_case(&args[1], k0) => {
            Some((inner0, args[1].clone()))
        }
        (None, Some((k1, inner1))) if literal_matches_case(&args[0], k1) => {
            Some((args[0].clone(), inner1))
        }
        _ => None,
    };
    let Some((lhs, rhs)) = pair else {
        return;
    };
    let target_op = if op == "STRING/=" {
        "string-not-equal"
    } else {
        "string-equal"
    };
    let fix = if (op != "EQUAL" || scope.env.in_boolean_test)
        && let Some(kw) = scope.standard_spelling(target_op)
    {
        let replacement = format!(
            "({kw} {} {})",
            scope.doc.text_of(lhs.text_range()),
            scope.doc.text_of(rhs.text_range())
        );
        Some(LintFix {
            title: format!("Replace with `{target_op}`"),
            // A later operand can mutate the original string after case conversion
            // has already produced a separate value for the first operand.
            applicability: FixApplicability::MaybeIncorrect,
            edits: vec![TextEdit {
                range: list.text_range(),
                replacement,
            }],
        })
    } else {
        None
    };
    emit.emit(
        "perf/string-equal-case-fold",
        list.text_range(),
        format!(
            "`{}` after `string-upcase`/`string-downcase` allocates temporary strings",
            op.to_ascii_lowercase()
        ),
        Some(format!("use `{target_op}` directly")),
        Vec::new(),
        fix,
    );
}
