//! Performance and security lint checks.

use crate::format::TextEdit;
use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::syntax::SyntaxKind;

use super::super::context::{LintDocument, LintEmitter, is_string_literal_el, unwrap_quote};
use super::super::{FixApplicability, LintFix};
use super::{CallSite, ScopeContext, collect, indexed_loop, sequence, values};

pub(crate) fn check_call_perf_and_security(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(op) = call.std_op else {
        return;
    };
    let CallSite { list, args, .. } = call;
    let ScopeContext {
        doc: ctx,
        package,
        env,
        ..
    } = scope;

    // ── Security ─────────────────────────────────────────────────────────────
    match op {
        "READ" | "READ-FROM-STRING" | "READ-PRESERVING-WHITESPACE" => {
            if !env.read_eval_disabled {
                emit.emit(
                    "security/unsafe-read-eval",
                    list.text_range(),
                    format!(
                        "`{}` called without binding `*read-eval*` to `nil`",
                        op.to_ascii_lowercase()
                    ),
                    Some(
                        "wrap in `(let ((*read-eval* nil)) ...)` when reading untrusted input"
                            .into(),
                    ),
                    Vec::new(),
                    None,
                );
            }
        }
        "EVAL" => {
            emit.emit(
                "security/eval-usage",
                list.text_range(),
                "runtime call to `eval` bypasses lexical scoping and introduces injection risk",
                Some(
                    "prefer closures, `funcall`/`apply`, hash tables, or compile-time macros"
                        .into(),
                ),
                Vec::new(),
                None,
            );
        }
        "FORMAT" => {
            if args.len() >= 2
                && !is_string_literal_el(&args[1])
                && !is_formatter_call(&args[1], package, ctx)
            {
                let ctrl_src = ctx.text_of(args[1].text_range());
                let dest_src = ctx.text_of(args[0].text_range());
                let replacement = if args.len() == 2 {
                    Some(format!("(format {dest_src} \"~A\" {ctrl_src})"))
                } else {
                    None
                };
                emit.emit(
                    "security/format-non-constant-control",
                    args[1].text_range(),
                    "`format` control string is not a string literal",
                    Some("pass a literal control string like `\"~A\"` and pass dynamic text as an argument".into()),
                    Vec::new(),
                    replacement.map(|rep| LintFix {
                        title: "Pass dynamic string via `\"~A\"`".into(),
                        applicability: FixApplicability::MaybeIncorrect,
                        edits: vec![TextEdit {
                            range: list.text_range(),
                            replacement: rep,
                        }],
                    }),
                );
            }
        }
        "INTERN" if args.len() == 1 && !env.in_defmacro => {
            emit.emit(
                "security/intern-without-package",
                list.text_range(),
                "`intern` called without an explicit package argument uses dynamic `*package*`",
                Some("pass a target package as the second argument or use `find-symbol`".into()),
                Vec::new(),
                None,
            );
        }
        _ => {}
    }

    // ── Performance ──────────────────────────────────────────────────────────
    match op {
        "=" | "EQL" | "<=" | ">" | "<" | "ZEROP" | "PLUSP" => {
            sequence::check_length_emptiness(scope, call, op, emit);
        }
        "SETF" | "SETQ" => {
            if env.in_loop {
                for pair in args.as_chunks::<2>().0 {
                    check_loop_accumulation_anti_pattern(scope, call, pair, emit);
                }
            }
        }
        "MAPCAR" | "MAPLIST" | "MAP" | "LENGTH" | "STRING=" | "STRING/=" | "STRING-EQUAL"
        | "STRING-NOT-EQUAL" | "STRING<" | "STRING<=" | "STRING>" | "STRING>=" | "EQUAL" => {
            sequence::check(scope, call, op, emit);
        }
        "CAR" | "FIRST" => {
            sequence::check(scope, call, op, emit);
            values::check_list_consumer(scope, call, op, emit);
        }
        "NTH" => {
            if env.in_loop {
                indexed_loop::check(scope, call, emit);
            }
            values::check_list_consumer(scope, call, op, emit);
        }
        "SECOND" | "THIRD" | "FOURTH" | "FIFTH" | "SIXTH" | "SEVENTH" | "EIGHTH" | "NINTH"
        | "TENTH" | "CADR" | "CADDR" | "CADDDR" | "VALUES-LIST" => {
            values::check_list_consumer(scope, call, op, emit);
        }
        _ => {}
    }
}

fn is_formatter_call(el: &Element, package: Option<&str>, ctx: &LintDocument<'_>) -> bool {
    el.as_node().is_some_and(|n| {
        closed_list(n)
            && elements(n)
                .next()
                .and_then(|e| symbol(&e))
                .is_some_and(|s| ctx.workspace.standard_symbol(&s, package) == Some("FORMATTER"))
    })
}

fn check_loop_accumulation_anti_pattern(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    [place, val]: &[Element; 2],
    emit: &mut LintEmitter<'_>,
) {
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    let Some(val_node) = val
        .as_node()
        .filter(|n| n.kind() == SyntaxKind::List && closed_list(n))
    else {
        return;
    };
    let mut parts = elements(val_node);
    let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
        return;
    };
    let Some(std_op) = ctx.std_op(&head, package, lexicals, env) else {
        return;
    };
    let Some(target) = symbol(place) else { return };
    if matches!(std_op, "APPEND" | "NCONC") {
        if let Some(first_arg) = parts.next()
            && symbol(&first_arg).as_ref() == Some(&target)
            && parts.next().is_some()
        {
            let fix = (call.args.len() == 2 && emit.is_enabled("perf/append-in-loop"))
                .then(|| collect::collect_instead_of_append(scope, call.list, val_node, &target))
                .flatten();
            emit.emit(
                "perf/append-in-loop",
                val_node.text_range(),
                format!("`{}` repeatedly traverses the same accumulator; growing it this way can take quadratic time", std_op.to_ascii_lowercase()),
                Some("use `(push item acc)` and `(nreverse acc)`, or `loop ... collect`".into()),
                Vec::new(),
                fix,
            );
        }
    } else if std_op == "CONCATENATE"
        && let Some(type_arg) = parts.next()
        && let Some(ty) = unwrap_quote(&type_arg, package, ctx.workspace)
        && symbol(&ty).is_some_and(|s| ctx.workspace.standard_symbol(&s, package) == Some("STRING"))
        && let Some(first_str) = parts.next()
        && symbol(&first_str) == Some(target)
        && parts.next().is_some()
    {
        emit.emit(
            "perf/string-concat-in-loop",
            val_node.text_range(),
            "repeated `(concatenate 'string ...)` in a loop allocates O(n^2) bytes",
            Some("use `with-output-to-string` and `write-string`".into()),
            Vec::new(),
            None,
        );
    }
}
