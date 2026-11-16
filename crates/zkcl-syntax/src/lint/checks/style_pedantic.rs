//! Style and pedantic lint checks: idiomatic CL, naming conventions,
//! package hygiene, deprecated features, CLOS style, docstrings, complexity.

use crate::cl_builtins::is_external_cl_symbol;
use crate::format::TextEdit;
use crate::navigation::ScopeBinding;
use crate::navigation::index::{closed_list, symbol};
use crate::navigation::queries::find_local_fn;
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SyntaxKind, SyntaxNode};

use super::super::context::{
    FormContext, LintDocument, LintEmitter, integer_literal_value, is_function_quote,
    is_nil_element, is_pure_atom, is_string_literal_el, is_t_element, normalized_sexpr,
    string_literal_value, unwrap_quote,
};
use super::super::{FixApplicability, LintFix, LintTag};
use super::complexity::unwrap_unshadowed_unary_call;

use super::{CallSite, ScopeContext};

pub(crate) fn check_call_style_and_pedantic(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    has_callable: bool,
    emit: &mut LintEmitter<'_>,
) {
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    let CallSite {
        list,
        head: head_sym,
        args,
        std_op,
    } = call;
    // Check cross-dialect hallucinations only when the call doesn't resolve anywhere
    // and the current package doesn't `:use` unresolved external packages.
    if std_op.is_none()
        && matches!(head_sym.prefix, Prefix::None)
        && !env.is_fn_shadowed(head_sym)
        && find_local_fn(lexicals, head_sym, package, ctx.workspace).is_none()
        && !has_callable
        && !package_uses_unresolved_external(ctx, package)
        && let Some(suggestion) = cross_dialect_suggestion(&head_sym.name)
    {
        emit.emit(
            "style/cross-dialect-hallucination",
            elements(list)
                .next()
                .map_or(list.text_range(), |e| e.text_range()),
            format!(
                "`{}` is not an ANSI Common Lisp operator",
                head_sym.name.to_ascii_lowercase()
            ),
            Some(suggestion.to_owned()),
            Vec::new(),
            None,
        );
    }

    let Some(op) = std_op else {
        return;
    };

    match op {
        "IF" => check_if_to_when_unless(ctx, list, args, package, lexicals, env, emit),
        "AND" => check_and_progn(ctx, args, package, lexicals, env, emit),
        "WHEN" | "UNLESS" => check_when_unless_not(scope, op, list, args, emit),
        "COND" => check_cond_simplification(scope, list, args, emit),
        "EQ" | "EQL" | "EQUAL" => check_null_equality_comparison(scope, op, list, args, emit),
        "FUNCALL" => check_unnecessary_funcall(ctx, list, args, package, lexicals, env, emit),
        "FORMAT" => check_useless_format_nil(ctx, list, args, package, emit),
        "SETF" | "SETQ" => check_incf_decf(scope, op, list, args, emit),
        "LAMBDA" => check_redundant_lambda(ctx, list, args, package, lexicals, env, emit),
        "=" | ">" | "<" => check_numeric_sign_predicate(scope, op, list, args, emit),
        "GENTEMP" | "SET" | "RPLACA" | "RPLACD" | "PROG" | "PROG*" => {
            let suggestion = match op {
                "GENTEMP" => "use `gensym` to avoid interning leaked symbols",
                "SET" => "use `(setf (symbol-value ...) ...)` or lexical bindings",
                "RPLACA" => "use `(setf (car ...) ...)`",
                "RPLACD" => "use `(setf (cdr ...) ...)`",
                _ => "use `let` / `block` / `tagbody` / `loop` instead of `prog`",
            };
            emit.emit(
                "style/deprecated-cl-feature",
                elements(list)
                    .next()
                    .map_or(list.text_range(), |e| e.text_range()),
                format!(
                    "`{}` is deprecated or obsolete in modern Common Lisp",
                    op.to_ascii_lowercase()
                ),
                Some(suggestion.into()),
                vec![LintTag::Deprecated],
                None,
            );
        }
        "REMOVE-IF-NOT" | "FIND-IF-NOT" | "COUNT-IF-NOT" | "POSITION-IF-NOT" | "DELETE-IF-NOT"
        | "MEMBER-IF-NOT" | "ASSOC-IF-NOT" | "RASSOC-IF-NOT" | "SUBSTITUTE-IF-NOT"
        | "NSUBSTITUTE-IF-NOT" | "SUBST-IF-NOT" | "NSUBST-IF-NOT" => {
            let positive = op.trim_end_matches("-NOT").to_ascii_lowercase();
            emit.emit(
                "pedantic/deprecated-if-not",
                elements(list)
                    .next()
                    .map_or(list.text_range(), |e| e.text_range()),
                format!(
                    "`{}` is deprecated in ANSI CLHS §1.8.1",
                    op.to_ascii_lowercase()
                ),
                Some(format!("use `({positive} (complement pred) ...)`")),
                vec![LintTag::Deprecated],
                None,
            );
        }
        "+" | "-" => check_plus_minus_one(scope, op, list, args, emit),
        _ => {}
    }

    // `:test-not` keyword argument (CLHS §1.8.2 deprecated)
    if let Some(test_not_kw) = args.iter().find(|arg| {
        symbol(arg).is_some_and(|s| s.prefix == Prefix::Keyword && s.name == "TEST-NOT")
    }) {
        emit.emit(
            "style/deprecated-cl-feature",
            test_not_kw.text_range(),
            "`:test-not` keyword argument is deprecated in ANSI CLHS §1.8.2",
            Some("use `:test (complement pred)` instead".into()),
            vec![LintTag::Deprecated],
            None,
        );
    }

    // Deep CAR/CDR chains
    if is_deep_car_cdr(op) {
        emit.emit(
            "pedantic/deep-car-cdr",
            elements(list)
                .next()
                .map_or(list.text_range(), |e| e.text_range()),
            format!(
                "deep accessor `{}` obscures data shape",
                op.to_ascii_lowercase()
            ),
            Some("prefer `second`/`third`/`fourth`, `nth`, or `destructuring-bind`".into()),
            Vec::new(),
            None,
        );
    }
}

fn cross_dialect_suggestion(name: &str) -> Option<&'static str> {
    match name {
        "DEFINE" => Some("use `defun`, `defvar`, or `defparameter` in Common Lisp"),
        "SET!" => Some("use `setf` or `setq` in Common Lisp"),
        "BEGIN" => Some("use `progn` in Common Lisp"),
        "DISPLAY" | "PRINTLN" => Some("use `princ` or `format` in Common Lisp"),
        "NEWLINE" => Some("use `terpri` in Common Lisp"),
        "NULL?" | "EMPTY?" => Some("use `null` (lists) or `(zerop (length ...))` in Common Lisp"),
        "PAIR?" => Some("use `consp` in Common Lisp"),
        "LIST?" => Some("use `listp` in Common Lisp"),
        "NUMBER?" => Some("use `numberp` in Common Lisp"),
        "STRING?" => Some("use `stringp` in Common Lisp"),
        "SYMBOL?" => Some("use `symbolp` in Common Lisp"),
        "EQ?" => Some("use `eq` or `eql` in Common Lisp"),
        "EQUAL?" => Some("use `equal` in Common Lisp"),
        "EQV?" => Some("use `eql` in Common Lisp"),
        "STRING-APPEND" => Some("use `(concatenate 'string ...)` in Common Lisp"),
        "STRING-SPLIT" => Some("use `uiop:split-string` or a helper function in Common Lisp"),
        "FILTER" => Some("use `remove-if-not` (or `remove-if`) in Common Lisp"),
        "FOLDL" | "FOLDR" => Some("use `reduce` in Common Lisp"),
        "HASH-REF" | "HASH-SET!" => {
            Some("use `gethash` / `(setf (gethash ...) ...)` in Common Lisp")
        }
        "DEFN" => Some("use `defun` in Common Lisp"),
        "LETFN" => Some("use `flet` or `labels` in Common Lisp"),
        "FOR-EACH" => Some("use `mapc` or `dolist` in Common Lisp"),
        "CALL/CC" => Some("use `block`/`return-from` or `catch`/`throw` in Common Lisp"),
        _ => None,
    }
}

fn check_if_to_when_unless(
    ctx: &LintDocument<'_>,
    list: &SyntaxNode,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    if args.len() < 2 || args.len() > 3 {
        return;
    }
    let test = &args[0];
    let then_el = &args[1];
    let else_el = args.get(2);

    let one_armed_or_discarded_nil = else_el.is_none()
        || (env.result_discarded
            && else_el.is_some_and(|e| is_nil_element(e, package, ctx.workspace)));
    if one_armed_or_discarded_nil {
        if let Some(progn_body) = unwrap_std_progn_body(ctx, then_el, package, lexicals, env) {
            let body_src: String = progn_body
                .iter()
                .map(|el| ctx.text_of(el.text_range()))
                .collect::<Vec<_>>()
                .join(" ");
            let (kw, cond_src) = if let Some(inner) =
                super::complexity::unwrap_unshadowed_unary_call(
                    ctx, test, "NOT", package, lexicals, env,
                ) {
                ("unless", ctx.text_of(inner.text_range()))
            } else {
                ("when", ctx.text_of(test.text_range()))
            };
            let replacement = if body_src.is_empty() {
                format!("({kw} {cond_src})")
            } else {
                format!("({kw} {cond_src} {body_src})")
            };
            emit.emit(
                "style/progn-in-conditional",
                list.text_range(),
                format!("`if` wrapping `progn` without an else value should use `{kw}`"),
                Some(format!("replace with `{replacement}`")),
                vec![LintTag::Unnecessary],
                ctx.unqualified_standard(kw, package, lexicals, env)
                    .then(|| LintFix {
                        title: format!("Replace with `{kw}`"),
                        applicability: FixApplicability::MachineApplicable,
                        edits: vec![TextEdit {
                            range: list.text_range(),
                            replacement,
                        }],
                    }),
            );
            return;
        }
        if let Some(inner) = super::complexity::unwrap_unshadowed_unary_call(
            ctx, test, "NOT", package, lexicals, env,
        ) {
            let replacement = format!(
                "(unless {} {})",
                ctx.text_of(inner.text_range()),
                ctx.text_of(then_el.text_range())
            );
            emit.emit(
                "style/if-to-unless",
                list.text_range(),
                "`(if (not cond) ...)` without an else branch should be `unless`",
                Some(format!("replace with `{replacement}`")),
                Vec::new(),
                ctx.unqualified_standard("unless", package, lexicals, env)
                    .then(|| LintFix {
                        title: "Replace with `unless`".into(),
                        applicability: FixApplicability::MachineApplicable,
                        edits: vec![TextEdit {
                            range: list.text_range(),
                            replacement,
                        }],
                    }),
            );
        } else {
            let replacement = format!(
                "(when {} {})",
                ctx.text_of(test.text_range()),
                ctx.text_of(then_el.text_range())
            );
            emit.emit(
                "style/if-to-when",
                list.text_range(),
                if else_el.is_none() {
                    "one-armed `if` should be written with `when`"
                } else {
                    "`(if cond expr nil)` in void context should be written with `when`"
                },
                Some(format!("replace with `{replacement}`")),
                Vec::new(),
                ctx.unqualified_standard("when", package, lexicals, env)
                    .then(|| LintFix {
                        title: "Replace with `when`".into(),
                        applicability: FixApplicability::MachineApplicable,
                        edits: vec![TextEdit {
                            range: list.text_range(),
                            replacement,
                        }],
                    }),
            );
        }
    } else if env.result_discarded
        && let Some(else_el) = else_el
        && is_nil_element(then_el, package, ctx.workspace)
    {
        let replacement = format!(
            "(unless {} {})",
            ctx.text_of(test.text_range()),
            ctx.text_of(else_el.text_range())
        );
        emit.emit(
            "style/if-to-unless",
            list.text_range(),
            "`(if cond nil expr)` in void context should be written with `unless`",
            Some(format!("replace with `{replacement}`")),
            Vec::new(),
            ctx.unqualified_standard("unless", package, lexicals, env)
                .then(|| LintFix {
                    title: "Replace with `unless`".into(),
                    applicability: FixApplicability::MachineApplicable,
                    edits: vec![TextEdit {
                        range: list.text_range(),
                        replacement,
                    }],
                }),
        );
    }
}

fn unwrap_std_progn_body(
    ctx: &LintDocument<'_>,
    el: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> Option<Vec<Element>> {
    let node = el.as_node().filter(|n| closed_list(n))?;
    let mut items = elements(node);
    let head = symbol(&items.next()?)?;
    if ctx.std_op(&head, package, lexicals, env) != Some("PROGN") {
        return None;
    }
    Some(items.collect())
}

fn check_and_progn(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    for arg in args {
        if let Some(body) = unwrap_std_progn_body(ctx, arg, package, lexicals, env)
            && body.len() >= 2
        {
            emit.emit(
                "style/progn-in-conditional",
                arg.text_range(),
                "`progn` inside `and` obscures sequential control flow",
                Some("prefer `when` or factor sequential effects out of boolean `and`".into()),
                Vec::new(),
                None,
            );
        }
    }
}

fn check_when_unless_not(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if args.len() < 2 {
        return;
    }
    let Some(inner) = unwrap_unshadowed_unary_call(
        scope.doc,
        &args[0],
        "NOT",
        scope.package,
        scope.lexicals,
        scope.env,
    ) else {
        return;
    };
    let flipped = if op == "WHEN" { "unless" } else { "when" };
    let fix = elements(list)
        .next()
        .zip(scope.standard_spelling(flipped))
        .map(|(head, flipped_spelling)| LintFix {
            title: format!(
                "Replace `({} (not ...))` with `{flipped}`",
                op.to_ascii_lowercase()
            ),
            applicability: FixApplicability::MachineApplicable,
            edits: vec![
                TextEdit {
                    range: head.text_range(),
                    replacement: flipped_spelling.into_owned(),
                },
                TextEdit {
                    range: args[0].text_range(),
                    replacement: scope.doc.text_of(inner.text_range()).to_owned(),
                },
            ],
        });
    emit.emit(
        "style/when-or-unless-not",
        list.text_range(),
        format!(
            "`({} (not ...))` can be written as `{flipped}`",
            op.to_ascii_lowercase()
        ),
        None,
        Vec::new(),
        fix,
    );
}

fn check_cond_simplification(
    scope: ScopeContext<'_, '_>,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    let ctx = scope.doc;
    let package = scope.package;
    if args.len() == 1
        && let Some(cnode) = args[0].as_node().filter(|n| closed_list(n))
    {
        let cparts: Vec<_> = elements(cnode).collect();
        if cparts.len() >= 2 {
            let test = &cparts[0];
            if is_t_element(test, package, ctx.workspace) {
                emit.emit(
                    "style/cond-simplification",
                    list.text_range(),
                    "single unconditional `(cond (t ...))` clause does not need `cond`",
                    None,
                    vec![LintTag::Unnecessary],
                    None,
                );
            } else {
                let body_src = cparts[1..]
                    .iter()
                    .map(|e| ctx.text_of(e.text_range()))
                    .collect::<Vec<_>>()
                    .join(" ");
                let test_src = ctx.text_of(test.text_range());
                let fix = scope.standard_spelling("when").map(|when_kw| LintFix {
                    title: "Replace single-clause `cond` with `when`".into(),
                    applicability: FixApplicability::MachineApplicable,
                    edits: vec![TextEdit {
                        range: list.text_range(),
                        replacement: format!("({when_kw} {test_src} {body_src})"),
                    }],
                });
                emit.emit(
                    "style/cond-simplification",
                    list.text_range(),
                    "single-clause `cond` can be written as `when`",
                    Some(format!("replace with `(when {test_src} {body_src})`")),
                    Vec::new(),
                    fix,
                );
            }
        }
        return;
    }

    if args.len() == 2
        && let Some(c1) = args[0].as_node().filter(|n| closed_list(n))
        && let Some(c2) = args[1].as_node().filter(|n| closed_list(n))
    {
        let p1: Vec<_> = elements(c1).collect();
        let p2: Vec<_> = elements(c2).collect();
        if p1.len() == 2 && p2.len() == 2 && is_t_element(&p2[0], package, ctx.workspace) {
            if is_nil_element(&p2[1], package, ctx.workspace) {
                let test_src = ctx.text_of(p1[0].text_range());
                let body_src = ctx.text_of(p1[1].text_range());
                let fix = scope.standard_spelling("when").map(|when_kw| LintFix {
                    title: "Replace `cond` with `when`".into(),
                    applicability: FixApplicability::MachineApplicable,
                    edits: vec![TextEdit {
                        range: list.text_range(),
                        replacement: format!("({when_kw} {test_src} {body_src})"),
                    }],
                });
                emit.emit(
                    "style/cond-simplification",
                    list.text_range(),
                    "two-clause `cond` with `(t nil)` fallback is equivalent to `when`",
                    None,
                    Vec::new(),
                    fix,
                );
            } else {
                let test_src = ctx.text_of(p1[0].text_range());
                let then_src = ctx.text_of(p1[1].text_range());
                let else_src = ctx.text_of(p2[1].text_range());
                let fix = scope.standard_spelling("if").map(|if_kw| LintFix {
                    title: "Replace two-clause `cond` with `if`".into(),
                    applicability: FixApplicability::MachineApplicable,
                    edits: vec![TextEdit {
                        range: list.text_range(),
                        replacement: format!("({if_kw} {test_src} {then_src} {else_src})"),
                    }],
                });
                emit.emit(
                    "style/cond-simplification",
                    list.text_range(),
                    "two-clause `cond` with `t` fallback can be written as `if`",
                    None,
                    Vec::new(),
                    fix,
                );
            }
            return;
        }
    }

    if let Some(last) = args.last()
        && let Some(last_node) = last.as_node().filter(|n| closed_list(n))
    {
        let last_parts: Vec<_> = elements(last_node).collect();
        if last_parts.len() == 2
            && is_t_element(&last_parts[0], package, ctx.workspace)
            && is_nil_element(&last_parts[1], package, ctx.workspace)
        {
            emit.emit(
                "style/cond-simplification",
                last_node.text_range(),
                "trailing `(t nil)` clause in `cond` is redundant (`cond` returns `nil` by default)",
                None,
                vec![LintTag::Unnecessary],
                None,
            );
        }
    }
}

fn check_null_equality_comparison(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if args.len() != 2 {
        return;
    }
    let other = if is_nil_element(&args[0], scope.package, scope.doc.workspace) {
        &args[1]
    } else if is_nil_element(&args[1], scope.package, scope.doc.workspace) {
        &args[0]
    } else {
        return;
    };
    let preferred = if scope.env.in_boolean_test {
        "not"
    } else {
        "null"
    };
    let replacement = scope
        .standard_spelling(preferred)
        .map(|spelling| format!("({spelling} {})", scope.doc.text_of(other.text_range())));
    let summary_target = match &replacement {
        Some(rep) if !rep.contains('\n') => format!("`{rep}`"),
        _ => format!("`{preferred}`"),
    };
    let fix = replacement.map(|replacement| LintFix {
        title: format!("Replace with `({preferred} ...)`"),
        applicability: FixApplicability::MachineApplicable,
        edits: vec![TextEdit {
            range: list.text_range(),
            replacement,
        }],
    });
    emit.emit(
        "style/null-equality-comparison",
        list.text_range(),
        format!(
            "`({} ... nil)` is unidiomatic; prefer {summary_target}",
            op.to_ascii_lowercase()
        ),
        Some(format!("replace with {summary_target}")),
        Vec::new(),
        fix,
    );
}

fn check_unnecessary_funcall(
    ctx: &LintDocument<'_>,
    list: &SyntaxNode,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    let Some(first) = args.first() else {
        return;
    };
    let by_object = is_function_quote(first, package, ctx.workspace);
    let by_symbol = by_object.is_none();
    let target = by_object.or_else(|| unwrap_quote(first, package, ctx.workspace));
    let Some(target) = target else {
        return;
    };
    let Some(sym) = symbol(&target) else {
        return;
    };
    // Keep `(funcall #'lambda ...)` excluded and keep macro/special-op errors handled by correctness.
    if ctx
        .workspace
        .standard_symbol(&sym, package)
        .is_some_and(super::super::context::is_macro_or_special_builtin)
    {
        return;
    }
    let local = find_local_fn(lexicals, &sym, package, ctx.workspace);
    if (by_symbol && (local.is_some() || env.is_fn_shadowed(&sym)))
        || !ctx
            .workspace
            .resolve_callable(&sym, package, local)
            .is_some_and(|sig| sig.evaluates_arguments)
    {
        return;
    }
    let rest_src = args[1..]
        .iter()
        .map(|a| ctx.text_of(a.text_range()))
        .collect::<Vec<_>>()
        .join(" ");
    let fn_src = ctx.text_of(target.text_range());
    let replacement = if rest_src.is_empty() {
        format!("({fn_src})")
    } else {
        format!("({fn_src} {rest_src})")
    };
    emit.emit(
        "style/unnecessary-funcall",
        list.text_range(),
        format!("direct call `{replacement}` avoids unnecessary `funcall`"),
        None,
        vec![LintTag::Unnecessary],
        Some(LintFix {
            title: format!("Replace with direct call `({fn_src} ...)`"),
            applicability: FixApplicability::MaybeIncorrect,
            edits: vec![TextEdit {
                range: list.text_range(),
                replacement,
            }],
        }),
    );
}

fn check_useless_format_nil(
    ctx: &LintDocument<'_>,
    list: &SyntaxNode,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if args.len() != 2 || !is_nil_element(&args[0], package, ctx.workspace) {
        return;
    }
    let Some(ctrl) = string_literal_value(&args[1]) else {
        return;
    };
    if !ctrl.contains('~') {
        let replacement = ctx.text_of(args[1].text_range()).to_owned();
        emit.emit(
            "style/useless-format-nil",
            list.text_range(),
            "`format nil` on a string literal without `~` directives is redundant",
            Some("use the literal if callers do not need a freshly allocated string".into()),
            vec![LintTag::Unnecessary],
            Some(LintFix {
                title: "Use literal string directly".into(),
                applicability: FixApplicability::MaybeIncorrect,
                edits: vec![TextEdit {
                    range: list.text_range(),
                    replacement,
                }],
            }),
        );
    }
}

fn check_incf_decf(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if args.len() != 2 {
        return;
    }
    let ctx = scope.doc;
    let place = &args[0];
    // Only suggest auto-rewrite when `place` is a pure symbol to avoid double-evaluating side-effectful places.
    if symbol(place).is_none() {
        return;
    }
    let Some(val_node) = args[1]
        .as_node()
        .filter(|n| n.kind() == SyntaxKind::List && closed_list(n))
    else {
        return;
    };
    let mut parts = elements(val_node);
    let Some(arith_head) = parts.next().and_then(|e| symbol(&e)) else {
        return;
    };
    let Some(std_arith) = ctx.std_op(&arith_head, scope.package, scope.lexicals, scope.env) else {
        return;
    };
    if !matches!(std_arith, "+" | "-") {
        return;
    }
    let arith_args: Vec<_> = parts.collect();
    if arith_args.len() != 2 {
        return;
    }
    let place_norm = normalized_sexpr(place);
    let place_src = ctx.text_of(place.text_range());
    let delta = if normalized_sexpr(&arith_args[0]) == place_norm {
        &arith_args[1]
    } else if std_arith == "+" && normalized_sexpr(&arith_args[1]) == place_norm {
        &arith_args[0]
    } else {
        return;
    };

    let macro_name = if std_arith == "+" { "incf" } else { "decf" };
    let delta_src = ctx.text_of(delta.text_range());
    let display = if delta_src == "1" {
        format!("({macro_name} {place_src})")
    } else {
        format!("({macro_name} {place_src} {delta_src})")
    };
    let fix = scope.standard_spelling(macro_name).map(|macro_kw| {
        let replacement = if delta_src == "1" {
            format!("({macro_kw} {place_src})")
        } else {
            format!("({macro_kw} {place_src} {delta_src})")
        };
        LintFix {
            title: format!("Replace with `{display}`"),
            applicability: FixApplicability::MachineApplicable,
            edits: vec![TextEdit {
                range: list.text_range(),
                replacement,
            }],
        }
    });
    emit.emit(
        "style/incf-decf",
        list.text_range(),
        format!(
            "`({} {place_src} ({std_arith} ...))` can be written as `{display}`",
            op.to_ascii_lowercase()
        ),
        None,
        Vec::new(),
        fix,
    );
}

fn check_redundant_lambda(
    ctx: &LintDocument<'_>,
    list: &SyntaxNode,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    if args.len() != 2 {
        return;
    }
    let Some(params_node) = args[0].as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let params: Vec<_> = elements(params_node).collect();
    if params
        .iter()
        .any(|p| symbol(p).is_none_or(|s| s.name.starts_with('&')))
    {
        return;
    }

    let body = &args[1];
    // `(lambda (x) x)` -> `#'identity`
    if params.len() == 1
        && let Some(p_sym) = symbol(&params[0])
        && p_sym.prefix != Prefix::Uninterned
        && let Some(b_sym) = symbol(body)
        && p_sym == b_sym
    {
        emit.emit(
            "style/redundant-lambda",
            list.text_range(),
            "`(lambda (x) x)` is equivalent to `#'identity`",
            None,
            Vec::new(),
            Some(LintFix {
                title: "Replace with `#'identity`".into(),
                applicability: FixApplicability::MaybeIncorrect,
                edits: vec![TextEdit {
                    range: list.text_range(),
                    replacement: "#'identity".into(),
                }],
            }),
        );
        return;
    }

    // `(lambda (x) (fn x))` -> `#'fn` (only when `fn` is a known Function, not Macro/SpecialOp)
    if params.len() == 1
        && let Some(p_sym) = symbol(&params[0])
        && p_sym.prefix != Prefix::Uninterned
        && let Some(b_node) = body
            .as_node()
            .filter(|n| n.kind() == SyntaxKind::List && closed_list(n))
    {
        let b_parts: Vec<_> = elements(b_node).collect();
        if b_parts.len() == 2
            && let Some(fn_sym) = symbol(&b_parts[0])
            && let Some(arg_sym) = symbol(&b_parts[1])
            && arg_sym.name == p_sym.name
            && arg_sym.prefix == p_sym.prefix
            && is_eta_safe_function(ctx, &fn_sym, package, lexicals, env)
        {
            let fn_src = ctx.text_of(b_parts[0].text_range());
            let replacement = format!("#'{fn_src}");
            emit.emit(
                "style/redundant-lambda",
                list.text_range(),
                format!("eta-reducible lambda can be simplified to `{replacement}`"),
                None,
                Vec::new(),
                Some(LintFix {
                    title: format!("Eta-reduce to `{replacement}`"),
                    applicability: FixApplicability::MaybeIncorrect,
                    edits: vec![TextEdit {
                        range: list.text_range(),
                        replacement,
                    }],
                }),
            );
            return;
        }
    }

    // Constant lambda `(lambda (...) literal)` -> `(constantly literal)`
    if params.len() <= 1
        && is_pure_atom(body)
        && symbol(body).is_none_or(|s| {
            s.prefix == Prefix::Keyword
                || matches!(
                    ctx.workspace.standard_symbol(&s, package),
                    Some("T" | "NIL")
                )
        })
    {
        let val_src = ctx.text_of(body.text_range());
        let replacement = format!("(constantly {val_src})");
        emit.emit(
            "style/redundant-lambda",
            list.text_range(),
            format!("constant lambda can be written `{replacement}`"),
            None,
            Vec::new(),
            None,
        );
    }
}

fn is_eta_safe_function(
    ctx: &LintDocument<'_>,
    sym: &SymbolName,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> bool {
    if let Some(local) = find_local_fn(lexicals, sym, package, ctx.workspace) {
        return local.evaluates_arguments;
    }
    if env.is_fn_shadowed(sym) {
        return false;
    }
    if let Some(std_name) = ctx.workspace.standard_symbol(sym, package) {
        return !super::super::context::is_macro_or_special_builtin(std_name);
    }
    ctx.workspace
        .lookup_unique_declaration_parts(crate::navigation::Namespace::Function, sym, package)
        .is_some_and(|(_, decl)| decl.evaluates_arguments)
}

fn check_numeric_sign_predicate(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("style/numeric-sign-predicate") || args.len() != 2 {
        return;
    }
    let left_zero = integer_literal_value(&args[0]) == Some(0);
    let right_zero = integer_literal_value(&args[1]) == Some(0);
    if left_zero == right_zero {
        return;
    }
    let (other, pred_name) = match (op, left_zero, right_zero) {
        ("=", _, _) => (if right_zero { &args[0] } else { &args[1] }, "zerop"),
        (">", false, true) | ("<", true, false) => {
            (if right_zero { &args[0] } else { &args[1] }, "plusp")
        }
        ("<", false, true) | (">", true, false) => {
            (if right_zero { &args[0] } else { &args[1] }, "minusp")
        }
        _ => return,
    };
    let other_src = scope.doc.text_of(other.text_range());
    let display = format!("({pred_name} {other_src})");
    let fix = scope.standard_spelling(pred_name).map(|pred_kw| LintFix {
        title: format!("Replace with `{display}`"),
        applicability: FixApplicability::MachineApplicable,
        edits: vec![TextEdit {
            range: list.text_range(),
            replacement: format!("({pred_kw} {other_src})"),
        }],
    });
    emit.emit(
        "style/numeric-sign-predicate",
        list.text_range(),
        format!("`({op} ...)` comparison with `0` can be written as `{display}`"),
        None,
        Vec::new(),
        fix,
    );
}

fn check_plus_minus_one(
    scope: ScopeContext<'_, '_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if args.len() != 2 {
        return;
    }
    let one_on_right = scope.doc.text_of(args[1].text_range()) == "1";
    let one_on_left = op == "+" && scope.doc.text_of(args[0].text_range()) == "1";
    if !one_on_right && !one_on_left {
        return;
    }
    let other = if one_on_right { &args[0] } else { &args[1] };
    let fn_name = if op == "+" { "1+" } else { "1-" };
    let fix_spelling = scope.standard_spelling(fn_name);
    let display = format!("({fn_name} {})", scope.doc.text_of(other.text_range()));
    let fix = fix_spelling.map(|spelling| LintFix {
        title: format!("Replace with `{display}`"),
        applicability: FixApplicability::MachineApplicable,
        edits: vec![TextEdit {
            range: list.text_range(),
            replacement: format!("({spelling} {})", scope.doc.text_of(other.text_range())),
        }],
    });
    emit.emit(
        "pedantic/plus-one-minus-one",
        list.text_range(),
        format!("`({op} ... 1)` can be written as `{display}`"),
        None,
        Vec::new(),
        fix,
    );
}

fn is_deep_car_cdr(op: &str) -> bool {
    let bytes = op.as_bytes();
    bytes.len() >= 5
        && bytes.len() <= 6
        && bytes[0] == b'C'
        && bytes[bytes.len() - 1] == b'R'
        && bytes[1..bytes.len() - 1]
            .iter()
            .all(|&b| b == b'A' || b == b'D')
}

pub(crate) fn check_lambda_list_style(params_el: &Element, emit: &mut LintEmitter<'_>) {
    let Some(node) = params_el.as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let mut has_optional = false;
    let mut has_key = false;
    let mut aux_range = None;
    let mut allow_other_keys_range = None;

    for item in elements(node) {
        if let Some(sym) = symbol(&item) {
            match sym.name.as_str() {
                "&OPTIONAL" => has_optional = true,
                "&KEY" => has_key = true,
                "&AUX" => aux_range = Some(item.text_range()),
                "&ALLOW-OTHER-KEYS" => allow_other_keys_range = Some(item.text_range()),
                _ => {}
            }
        }
    }

    if has_optional && has_key {
        emit.emit(
            "style/mixed-optional-and-key",
            params_el.text_range(),
            "lambda list mixes `&optional` and `&key` parameters",
            Some("prefer using `&key` parameters with defaults instead of mixing both".into()),
            Vec::new(),
            None,
        );
    }
    if let Some(range) = aux_range {
        emit.emit(
            "style/obsolete-aux",
            range,
            "`&aux` in lambda list is obsolete; bind local helpers with `let*` in the body",
            None,
            Vec::new(),
            None,
        );
    }
    if let Some(range) = allow_other_keys_range {
        emit.emit(
            "style/no-allow-other-keys",
            range,
            "`&allow-other-keys` disables static keyword argument checking",
            Some("list accepted keyword parameters explicitly or accept `&rest` when forwarding options".into()),
            Vec::new(),
            None,
        );
    }
}

pub(crate) fn check_top_level_definition_style(
    ctx: &LintDocument<'_>,
    op: &str,
    list: &SyntaxNode,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(name_el) = args.first() else {
        return;
    };
    if let Some(name_sym) = symbol(name_el) {
        match op {
            "DEFVAR" | "DEFPARAMETER" => {
                if !(name_sym.name.len() > 2
                    && name_sym.name.starts_with('*')
                    && name_sym.name.ends_with('*'))
                {
                    emit.emit(
                        "style/special-earmuffs",
                        name_el.text_range(),
                        format!(
                            "special variable `{}` should use `*earmuffs*`",
                            name_sym.name.to_ascii_lowercase()
                        ),
                        Some("wrap special variable names in asterisks `*...*`".into()),
                        Vec::new(),
                        None,
                    );
                }
            }
            "DEFCONSTANT" => {
                if !(name_sym.name.len() > 2
                    && name_sym.name.starts_with('+')
                    && name_sym.name.ends_with('+'))
                {
                    emit.emit(
                        "style/constant-pluses",
                        name_el.text_range(),
                        format!(
                            "constant `{}` should follow the `+name+` convention",
                            name_sym.name.to_ascii_lowercase()
                        ),
                        None,
                        Vec::new(),
                        None,
                    );
                }
            }
            "DEFUN" if name_sym.name.starts_with("IS-") && name_sym.name.len() > 3 => {
                let stem = name_sym.name[3..].to_ascii_lowercase();
                emit.emit(
                    "style/is-prefix-predicate",
                    name_el.text_range(),
                    format!(
                        "predicate name `{}` uses `is-` prefix instead of Common Lisp `-p` suffix",
                        name_sym.name.to_ascii_lowercase()
                    ),
                    Some(format!("consider naming it `{stem}-p`")),
                    Vec::new(),
                    None,
                );
            }
            _ => {}
        }

        // Shadowed external CL symbol check (pedantic)
        // `defmethod` specializes an existing generic function rather than shadowing it.
        if op != "DEFMETHOD"
            && matches!(name_sym.prefix, Prefix::None)
            && is_external_cl_symbol(&name_sym.name)
            && !is_shadowed_in_package(ctx, package, &name_sym.name)
        {
            emit.emit(
                "pedantic/shadowed-cl-symbol",
                name_el.text_range(),
                format!(
                    "definition `{}` shadows standard `COMMON-LISP` symbol",
                    name_sym.name.to_ascii_lowercase()
                ),
                Some("choose a distinct name or declare `:shadow` in `defpackage`".into()),
                Vec::new(),
                None,
            );
        }
    }

    // Missing docstring check (pedantic)
    if emit.is_enabled("pedantic/missing-docstring")
        && !has_definition_docstring(ctx, op, args, package)
    {
        emit.emit(
            "pedantic/missing-docstring",
            name_el.text_range(),
            format!(
                "`{}` `{}` is missing a docstring",
                op.to_ascii_lowercase(),
                ctx.text_of(name_el.text_range())
            ),
            None,
            Vec::new(),
            None,
        );
    }

    // `define-condition` missing `:report`
    if op == "DEFINE-CONDITION" && args.len() >= 3 {
        let has_report = args[3..].iter().any(|opt| {
            opt.as_node().is_some_and(|n| {
                closed_list(n)
                    && elements(n)
                        .next()
                        .and_then(|e| symbol(&e))
                        .is_some_and(|s| s.prefix == Prefix::Keyword && s.name == "REPORT")
            })
        });
        if !has_report {
            emit.emit(
                "style/condition-missing-report",
                name_el.text_range(),
                format!(
                    "`define-condition` `{}` has no `:report` option",
                    ctx.text_of(name_el.text_range())
                ),
                Some("add `(:report ...)` so printed conditions are human-readable".into()),
                Vec::new(),
                None,
            );
        }
    }

    // `defclass` inaccessible slot check
    if op == "DEFCLASS"
        && args.len() >= 3
        && let Some(slots_node) = args[2].as_node().filter(|n| closed_list(n))
    {
        for slot in elements(slots_node) {
            check_defclass_slot_accessibility(&slot, emit);
        }
    }

    // High cyclomatic/branch complexity & nesting check (pedantic)
    if matches!(op, "DEFUN" | "DEFMACRO" | "DEFMETHOD")
        && emit.is_enabled("pedantic/high-complexity")
    {
        let (branches, max_depth) = measure_form_complexity(list, package, ctx.workspace, 0);
        if branches > emit.config.max_complexity || max_depth > emit.config.max_nesting {
            emit.emit(
                "pedantic/high-complexity",
                name_el.text_range(),
                format!(
                    "`{}` exceeds complexity thresholds (branches: {branches}, max nesting: {max_depth})",
                    ctx.text_of(name_el.text_range())
                ),
                Some("split deeply nested branches into smaller helper functions".into()),
                Vec::new(),
                None,
            );
        }
    }
}

fn check_defclass_slot_accessibility(slot: &Element, emit: &mut LintEmitter<'_>) {
    let Some(slot_node) = slot.as_node().filter(|n| closed_list(n)) else {
        emit.emit(
            "style/defclass-inaccessible-slot",
            slot.text_range(),
            "bare `defclass` slot has no `:initarg`, `:accessor`, `:reader`, or `:writer`",
            None,
            Vec::new(),
            None,
        );
        return;
    };
    let mut parts = elements(slot_node);
    let Some(slot_name) = parts.next() else {
        return;
    };
    let opts: Vec<_> = parts.collect();
    let has_iface = opts.as_chunks::<2>().0.iter().any(|pair| {
        symbol(&pair[0]).is_some_and(|s| {
            s.prefix == Prefix::Keyword
                && matches!(
                    s.name.as_str(),
                    "INITARG" | "ACCESSOR" | "READER" | "WRITER"
                )
        })
    });
    if !has_iface {
        emit.emit(
            "style/defclass-inaccessible-slot",
            slot_name.text_range(),
            "slot defines neither `:initarg` nor `:accessor`/`:reader`/`:writer`",
            Some("add `:initarg` and/or `:accessor` to avoid raw `slot-value`".into()),
            Vec::new(),
            None,
        );
    }
}

fn has_definition_docstring(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
) -> bool {
    match op {
        "DEFVAR" | "DEFPARAMETER" | "DEFCONSTANT" => args.get(2).is_some_and(is_string_literal_el),
        "DEFUN" | "DEFMACRO" | "DEFTYPE" => {
            let rest = args.get(2..).unwrap_or(&[]);
            for (idx, el) in rest.iter().enumerate() {
                if idx + 1 >= rest.len() {
                    break;
                }
                if is_string_literal_el(el) {
                    return true;
                }
                if super::is_declaration(ctx, el, package) {
                    continue;
                }
                break;
            }
            false
        }
        "DEFGENERIC" | "DEFCLASS" | "DEFINE-CONDITION" => args.iter().any(|arg| {
            arg.as_node().is_some_and(|n| {
                closed_list(n)
                    && elements(n)
                        .next()
                        .and_then(|e| symbol(&e))
                        .is_some_and(|s| s.prefix == Prefix::Keyword && s.name == "DOCUMENTATION")
            })
        }),
        "DEFSTRUCT" => args.get(1).is_some_and(is_string_literal_el),
        _ => true,
    }
}

fn measure_form_complexity(
    node: &SyntaxNode,
    package: Option<&str>,
    workspace: &crate::navigation::index::WorkspaceIndex<'_>,
    depth: usize,
) -> (usize, usize) {
    let mut branches = 0usize;
    let mut max_depth = depth;
    for child in elements(node) {
        let Some(cnode) = child.as_node() else {
            continue;
        };
        if cnode.kind() == SyntaxKind::List && closed_list(cnode) {
            let mut next_depth = depth;
            if let Some(head_el) = elements(cnode).next()
                && maybe_complexity_branch_head(&head_el)
                && let Some(head) = symbol(&head_el)
                && let Some(std_op) = workspace.standard_symbol(&head, package)
                && is_complexity_branch_op(std_op)
            {
                branches += 1;
                next_depth += 1;
            }
            let (cb, cd) = measure_form_complexity(cnode, package, workspace, next_depth);
            branches += cb;
            max_depth = max_depth.max(cd);
        }
    }
    (branches, max_depth)
}

fn maybe_complexity_branch_head(head_el: &Element) -> bool {
    let Some(tok) = head_el.as_token() else {
        return false;
    };
    let text = tok.text();
    if text.contains(':') || text.contains('|') || text.contains('\\') {
        return true;
    }
    matches!(text.len(), 2 | 3 | 4 | 6 | 7 | 8 | 9 | 12)
        && is_complexity_branch_op_ignore_case(text)
}

fn is_complexity_branch_op_ignore_case(name: &str) -> bool {
    name.eq_ignore_ascii_case("IF")
        || name.eq_ignore_ascii_case("WHEN")
        || name.eq_ignore_ascii_case("UNLESS")
        || name.eq_ignore_ascii_case("COND")
        || name.eq_ignore_ascii_case("CASE")
        || name.eq_ignore_ascii_case("ECASE")
        || name.eq_ignore_ascii_case("CCASE")
        || name.eq_ignore_ascii_case("TYPECASE")
        || name.eq_ignore_ascii_case("ETYPECASE")
        || name.eq_ignore_ascii_case("CTYPECASE")
        || name.eq_ignore_ascii_case("HANDLER-CASE")
        || name.eq_ignore_ascii_case("AND")
        || name.eq_ignore_ascii_case("OR")
        || name.eq_ignore_ascii_case("LOOP")
        || name.eq_ignore_ascii_case("DO")
        || name.eq_ignore_ascii_case("DO*")
        || name.eq_ignore_ascii_case("DOLIST")
        || name.eq_ignore_ascii_case("DOTIMES")
}

fn is_complexity_branch_op(std_op: &str) -> bool {
    matches!(
        std_op,
        "IF" | "WHEN"
            | "UNLESS"
            | "COND"
            | "CASE"
            | "ECASE"
            | "CCASE"
            | "TYPECASE"
            | "ETYPECASE"
            | "CTYPECASE"
            | "HANDLER-CASE"
            | "AND"
            | "OR"
            | "LOOP"
            | "DO"
            | "DO*"
            | "DOLIST"
            | "DOTIMES"
    )
}

fn is_shadowed_in_package(ctx: &LintDocument<'_>, package: Option<&str>, sym_name: &str) -> bool {
    let Some(pkg) = package.and_then(|p| ctx.workspace.canonical_global_package(p)) else {
        return false;
    };
    let Some(info) = ctx.workspace.package(pkg) else {
        return false;
    };
    let uses_cl = info.uses.iter().any(|u| {
        ctx.workspace
            .canonical_global_package(u)
            .unwrap_or(u.as_str())
            == "COMMON-LISP"
    });
    !uses_cl
        || info.shadows.iter().any(|s| s == sym_name)
        || info.imports.iter().any(|i| i.name == sym_name)
}

fn package_uses_unresolved_external(ctx: &LintDocument<'_>, package: Option<&str>) -> bool {
    let Some(pkg) = package.and_then(|p| ctx.workspace.canonical_global_package(p)) else {
        return false;
    };
    let Some(info) = ctx.workspace.package(pkg) else {
        return false;
    };
    info.uses.iter().any(|u| {
        let canon = ctx
            .workspace
            .canonical_global_package(u)
            .unwrap_or(u.as_str());
        canon != "COMMON-LISP" && ctx.workspace.package(canon).is_none()
    })
}

fn check_defpackage_designator(
    ctx: &LintDocument<'_>,
    el: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let target = unwrap_quote(el, package, ctx.workspace).unwrap_or_else(|| el.clone());
    if let Some(sym) = symbol(&target)
        && matches!(sym.prefix, Prefix::None)
    {
        let replacement = format!(":{}", ctx.text_of(target.text_range()));
        emit.emit(
            "style/defpackage-interned-symbol",
            el.text_range(),
            format!(
                "`defpackage` designator `{}` interns a symbol in the current package",
                ctx.text_of(el.text_range())
            ),
            Some(format!(
                "use keyword `:{0}` or uninterned `#:{0}`",
                sym.name.to_ascii_lowercase(),
            )),
            Vec::new(),
            Some(LintFix {
                title: format!("Use keyword `:{}`", sym.name.to_ascii_lowercase()),
                applicability: FixApplicability::MachineApplicable,
                edits: vec![TextEdit {
                    range: el.text_range(),
                    replacement,
                }],
            }),
        );
    }
}

pub(crate) fn check_defpackage_style(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if let Some(name_el) = args.first() {
        check_defpackage_designator(ctx, name_el, package, emit);
    }
    for opt in args.iter().skip(1) {
        let Some(opt_node) = opt.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let mut parts = elements(opt_node);
        let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
            continue;
        };
        if head.prefix != Prefix::Keyword {
            continue;
        }
        match head.name.as_str() {
            "USE" => {
                for used in parts {
                    check_defpackage_designator(ctx, &used, package, emit);
                    if let Some(used_name) = crate::reader_tokens::string_designator(&used) {
                        let canon = ctx
                            .workspace
                            .canonical_global_package(&used_name)
                            .unwrap_or(&used_name);
                        if canon != "COMMON-LISP" {
                            emit.emit(
                                "style/defpackage-use-non-cl",
                                used.text_range(),
                                format!(
                                    "`:use` of non-standard package `{}` risks symbol clashes",
                                    used_name.to_ascii_lowercase()
                                ),
                                Some("prefer `:import-from` or `:local-nicknames`".into()),
                                Vec::new(),
                                None,
                            );
                        }
                    }
                }
            }
            "EXPORT"
            | "SHADOW"
            | "INTERN"
            | "NICKNAMES"
            | "IMPORT-FROM"
            | "SHADOWING-IMPORT-FROM" => {
                for designator in parts {
                    check_defpackage_designator(ctx, &designator, package, emit);
                }
            }
            "LOCAL-NICKNAMES" => {
                for pair in parts {
                    if let Some(pnode) = pair.as_node().filter(|n| closed_list(n)) {
                        for item in elements(pnode) {
                            check_defpackage_designator(ctx, &item, package, emit);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn check_double_colon_token(
    ctx: &LintDocument<'_>,
    tok: &crate::outline::Token,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if tok.kind() != SyntaxKind::Atom {
        return;
    }
    let Some(sym) = SymbolName::parse(tok.text()) else {
        return;
    };
    let Prefix::Package {
        name,
        external: false,
    } = &sym.prefix
    else {
        return;
    };
    let target_pkg = ctx
        .workspace
        .resolve_package_prefix(package, name)
        .unwrap_or(name.as_str());
    let current_pkg = package.and_then(|p| ctx.workspace.canonical_global_package(p));
    if current_pkg == Some(target_pkg) || target_pkg == "COMMON-LISP" {
        return;
    }
    emit.emit(
        "style/double-colon-internal-access",
        tok.text_range(),
        format!(
            "internal symbol access `{}` reaches into package `{}`",
            tok.text(),
            target_pkg.to_ascii_lowercase()
        ),
        Some("prefer exported symbols (`pkg:sym`) or export the symbol".into()),
        Vec::new(),
        None,
    );
}
