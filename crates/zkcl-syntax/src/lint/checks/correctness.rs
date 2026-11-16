//! Correctness lint checks: definite bugs, ANSI CL undefined behavior, and invalid forms.

use std::collections::HashSet;

use crate::format::TextEdit;
use crate::navigation::ScopeBinding;
use crate::navigation::index::{CallableSignature, closed_list, operator, symbol};
use crate::navigation::queries::{check_eval_when_warnings, find_local_fn};
use crate::navigation::scope::ScopeAtOffset;
use crate::outline::{Element, elements};
use crate::reader_tokens::Prefix;
use crate::syntax::{SyntaxKind, SyntaxNode, TextSize};

use super::super::context::{
    FormContext, LintDocument, LintEmitter, integer_literal_value, is_char_literal_el,
    is_destructive_sequence_op, is_literal_compound, is_literal_mutating_op,
    is_macro_or_special_builtin, is_nil_element, is_number_literal_el, is_string_literal_el,
    normalized_sexpr, numeric_literal_is_zero, unwrap_quote,
};
use super::super::{FixApplicability, LintFix, LintTag};

use super::{CallSite, ScopeContext};

pub(crate) fn check_call_correctness(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    callable: Option<&CallableSignature<'_>>,
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
    if let Some(spec) = callable {
        if !super::call_arguments::check_arity(spec, args.len(), true, list.text_range(), emit) {
            return;
        }
        super::call_arguments::check_keywords(ctx, spec, args, package, emit);
    }

    let Some(op) = std_op else {
        if find_local_fn(lexicals, head_sym, package, ctx.workspace).is_none()
            && !env.is_fn_shadowed(head_sym)
        {
            super::foreign::check_declaration(ctx, head_sym, args, package, emit);
        }
        return;
    };

    super::literal_arguments::check(ctx, op, args, package, emit);
    super::call_arguments::check_callback(scope, op, args, emit);
    match op {
        "SETF" | "PSETF" | "SETQ" | "PSETQ" => {
            if !args.len().is_multiple_of(2) {
                emit.emit(
                    "correctness/odd-key-value-args",
                    list.text_range(),
                    format!(
                        "`{}` requires an even number of place/value arguments, found {}",
                        op.to_ascii_lowercase(),
                        args.len()
                    ),
                    Some("add the missing value expression or remove the trailing place".into()),
                    Vec::new(),
                    None,
                );
            }
        }
        "FORMAT" => super::format_control::check(list, args, emit),
        "/" | "MOD" | "REM" | "FLOOR" | "CEILING" | "TRUNCATE" | "ROUND" => {
            check_division_by_zero(op, args, emit);
        }
        "CHAR" | "SCHAR" | "ELT" | "AREF" | "SVREF" | "ROW-MAJOR-AREF" | "SUBSEQ" => {
            check_literal_sequence_bounds(ctx, op, args, package, emit);
        }
        "EQ" => check_eq_usage(ctx, args, package, emit),
        "EQL" => check_eql_usage(ctx, args, package, emit),
        "FUNCALL" | "APPLY" => {
            super::call_arguments::check_indirect_call(scope, op, call, emit);
            if op == "FUNCALL" {
                super::foreign::check_inline_call(ctx, args, package, lexicals, env, emit);
            }
        }
        "FUNCTION" => {
            if args.len() == 1 {
                check_function_of_macro(ctx, &args[0], package, lexicals, env, emit);
            }
        }
        "RETURN" => {
            if env.nil_blocks == 0 {
                emit.emit(
                    "correctness/return-outside-nil-block",
                    list.text_range(),
                    "`return` used outside any `nil` block",
                    Some(
                        "use `(return-from <function-name> ...)` or return the final expression directly"
                            .into(),
                    ),
                    Vec::new(),
                    None,
                );
            }
        }
        "RETURN-FROM" | "GO" => {
            if let Some(target) = args.first() {
                let (kind, resolved, known) = if op == "GO" {
                    ("tag", ctx.control_tag(target, package), &env.tagbody_tags)
                } else {
                    ("block", ctx.control_block(target, package), &env.blocks)
                };
                if let Some(name) = resolved {
                    if !known.contains(&name) {
                        emit.emit(
                            "correctness/unknown-control-target",
                            target.text_range(),
                            format!(
                                "`{}` targets unknown {kind} `{}`",
                                op.to_ascii_lowercase(),
                                ctx.text_of(target.text_range())
                            ),
                            Some(format!("use a {kind} visible in this lexical scope")),
                            Vec::new(),
                            None,
                        );
                    }
                } else if target.kind() != SyntaxKind::ReaderForm && symbol(target).is_none() {
                    super::grammar::report(
                        emit,
                        "correctness/invalid-control-target",
                        target,
                        if op == "GO" {
                            "a `go` tag must be a symbol or an integer"
                        } else {
                            "a block name must be a symbol"
                        },
                    );
                }
            }
        }
        "DEFVAR" | "DEFPARAMETER" | "DEFCONSTANT" | "DEFPACKAGE" | "IN-PACKAGE" => {
            if !env.in_top_level {
                emit.emit(
                    "correctness/non-toplevel-definition",
                    list.text_range(),
                    format!(
                        "`{}` should appear at top level, not inside a local form",
                        op.to_ascii_lowercase()
                    ),
                    Some("move this definition to top level".into()),
                    Vec::new(),
                    None,
                );
            }
            if op == "DEFCONSTANT" && args.len() >= 2 {
                check_defconstant_value(ctx, &args[1], package, emit);
            }
        }
        "MAPCAN" | "MAPCON" => {
            check_mapcan_literal(ctx, op, args, package, emit);
        }
        _ => {}
    }

    if is_literal_mutating_op(op) {
        check_literal_mutation(ctx, op, args, package, emit);
    }
    if is_destructive_sequence_op(op) && env.result_discarded {
        emit.emit(
            "correctness/discarded-destructive-result",
            list.text_range(),
            format!(
                "return value of destructive `{}` is discarded",
                op.to_ascii_lowercase()
            ),
            Some(format!(
                "assign the result back (e.g. `(setf seq ({} seq ...))`)",
                op.to_ascii_lowercase()
            )),
            Vec::new(),
            None,
        );
    }
}

pub(crate) fn check_function_quote_form(
    ctx: &LintDocument<'_>,
    reader_node: &SyntaxNode,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    if let Some(target) = elements(reader_node).nth(1) {
        check_function_of_macro(ctx, &target, package, lexicals, env, emit);
    }
}

pub(super) fn check_function_of_macro(
    ctx: &LintDocument<'_>,
    target: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    let Some(sym) = symbol(target) else {
        return;
    };
    if let Some(local) = find_local_fn(lexicals, &sym, package, ctx.workspace) {
        if !local.evaluates_arguments
            && let Some(diag) = emit.diagnostic(
                "correctness/function-of-macro-or-special",
                target.text_range(),
                "a local macro does not denote a function",
                Some("wrap the macro call in `lambda`".into()),
                Vec::new(),
                None,
            )
            && let Some(range) = local.def_range
        {
            diag.note(range, "the local macro is defined here");
        }
        return;
    }
    if env.is_fn_shadowed(&sym) {
        return;
    }
    if let Some(std_name) = ctx.workspace.standard_symbol(&sym, package)
        && is_macro_or_special_builtin(std_name)
    {
        emit.emit(
            "correctness/function-of-macro-or-special",
            target.text_range(),
            format!(
                "cannot take `function` (`#'`) of macro or special operator `{}`",
                std_name.to_ascii_lowercase()
            ),
            Some("wrap it in a `(lambda (...) ...)` or use a standard function".into()),
            Vec::new(),
            None,
        );
        return;
    }
    if let Some((_, decl)) = ctx.workspace.lookup_unique_declaration_parts(
        crate::navigation::Namespace::Function,
        &sym,
        package,
    ) && decl.operator.name == "DEFMACRO"
    {
        emit.emit(
            "correctness/function-of-macro-or-special",
            target.text_range(),
            format!(
                "cannot take `function` (`#'`) of macro `{}`",
                decl.name.spelling.name.to_ascii_lowercase()
            ),
            Some("wrap the macro call in a `(lambda (...) ...)`".into()),
            Vec::new(),
            None,
        );
    }
}

fn check_division_by_zero(op: &str, args: &[Element], emit: &mut LintEmitter<'_>) {
    match op {
        "/" => {
            if args.len() == 1 && numeric_literal_is_zero(&args[0]) {
                emit.emit(
                    "correctness/division-by-zero",
                    args[0].text_range(),
                    "division by literal zero in reciprocal `(/ 0)`",
                    None,
                    Vec::new(),
                    None,
                );
            } else if args.len() >= 2 {
                for arg in &args[1..] {
                    if numeric_literal_is_zero(arg) {
                        emit.emit(
                            "correctness/division-by-zero",
                            arg.text_range(),
                            "division by literal zero",
                            None,
                            Vec::new(),
                            None,
                        );
                    }
                }
            }
        }
        "MOD" | "REM" | "FLOOR" | "CEILING" | "TRUNCATE" | "ROUND"
            if args.len() >= 2 && numeric_literal_is_zero(&args[1]) =>
        {
            emit.emit(
                "correctness/division-by-zero",
                args[1].text_range(),
                format!("`{}` divisor is literal zero", op.to_ascii_lowercase()),
                None,
                Vec::new(),
                None,
            );
        }
        _ => {}
    }
}

fn check_literal_sequence_bounds(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(seq_el) = args.first() else {
        return;
    };
    let Some(len) = super::literal_arguments::sequence_len(ctx, seq_el, package) else {
        return;
    };
    match op {
        "CHAR" | "SCHAR" | "ELT" | "AREF" | "SVREF" | "ROW-MAJOR-AREF" => {
            if args.len() == 2
                && let Some(idx) = integer_literal_value(&args[1])
                && (idx >= 0 && idx as usize >= len)
            {
                emit.emit(
                    "correctness/literal-sequence-bounds",
                    args[1].text_range(),
                    format!("index {idx} is out of bounds for literal sequence of length {len}"),
                    None,
                    Vec::new(),
                    None,
                );
            }
        }
        "SUBSEQ" => {
            if args.len() >= 2
                && let Some(start) = integer_literal_value(&args[1])
            {
                if start >= 0 && start as usize > len {
                    emit.emit(
                        "correctness/literal-sequence-bounds",
                        args[1].text_range(),
                        format!("`subseq` start index {start} is out of bounds for length {len}"),
                        None,
                        Vec::new(),
                        None,
                    );
                } else if args.len() == 3
                    && !is_nil_element(&args[2], package, ctx.workspace)
                    && let Some(end) = integer_literal_value(&args[2])
                    && end >= 0
                    && (end as usize > len || end < start)
                {
                    emit.emit(
                        "correctness/literal-sequence-bounds",
                        args[2].text_range(),
                        format!(
                            "`subseq` end index {end} is invalid for start {start} and length {len}"
                        ),
                        None,
                        Vec::new(),
                        None,
                    );
                }
            }
        }
        _ => {}
    }
}

fn check_eq_usage(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if args.len() != 2 {
        return;
    }
    let has_number = args.iter().any(is_number_literal_el);
    let has_char = args.iter().any(is_char_literal_el);
    let has_string = args.iter().any(is_string_literal_el);
    let has_compound = args
        .iter()
        .any(|a| !is_string_literal_el(a) && is_literal_compound(a, package, ctx.workspace));

    let range = args[0].text_range().cover(args[1].text_range());
    if has_string {
        emit.emit(
            "correctness/eq-on-non-identity",
            range,
            "`eq` compares string object identity, not string contents",
            Some("use `string=` or `equal` to compare strings".into()),
            Vec::new(),
            None,
        );
    } else if has_compound {
        emit.emit(
            "correctness/eq-on-non-identity",
            range,
            "`eq` compares object identity rather than structural equality",
            Some("use `equal` for lists and vectors".into()),
            Vec::new(),
            None,
        );
    } else if has_number || has_char {
        emit.emit(
            "correctness/eq-on-non-identity",
            range,
            "`eq` on numbers or characters is implementation-dependent in ANSI Common Lisp",
            Some(if has_number {
                "use `=` or `eql` for numbers".into()
            } else {
                "use `char=` or `eql` for characters".into()
            }),
            Vec::new(),
            None,
        );
    }
}

fn check_eql_usage(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if args.len() != 2 {
        return;
    }
    if args.iter().any(is_string_literal_el) {
        emit.emit(
            "correctness/eql-on-compound",
            args[0].text_range().cover(args[1].text_range()),
            "`eql` compares string identity, not contents",
            Some("use `string=` or `equal` to compare strings".into()),
            Vec::new(),
            None,
        );
    } else if args
        .iter()
        .any(|a| is_literal_compound(a, package, ctx.workspace))
    {
        emit.emit(
            "correctness/eql-on-compound",
            args[0].text_range().cover(args[1].text_range()),
            "`eql` does not compare contents of lists or vectors",
            Some("use `equal` or `equalp`".into()),
            Vec::new(),
            None,
        );
    }
}

fn check_literal_mutation(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if op == "NCONC" {
        if args.len() >= 2 {
            for arg in &args[..args.len() - 1] {
                if is_literal_compound(arg, package, ctx.workspace) {
                    emit.emit(
                        "correctness/literal-mutation",
                        arg.text_range(),
                        "mutating a literal list with `nconc` has undefined consequences",
                        Some(
                            "construct a fresh list with `(list ...)` or `copy-list`, or use `append`"
                                .into(),
                        ),
                        Vec::new(),
                        None,
                    );
                }
            }
        }
        return;
    }

    let target_indices: &[usize] = match op {
        "DELETE" | "DELETE-IF" | "DELETE-IF-NOT" => &[1],
        "NSUBST" | "NSUBST-IF" | "NSUBST-IF-NOT" | "NSUBSTITUTE" | "NSUBSTITUTE-IF"
        | "NSUBSTITUTE-IF-NOT" => &[2],
        "NUNION" | "NINTERSECTION" | "NSET-DIFFERENCE" | "NSET-EXCLUSIVE-OR" => &[0, 1],
        _ => &[0],
    };

    for &idx in target_indices {
        if let Some(arg) = args.get(idx)
            && is_literal_compound(arg, package, ctx.workspace)
        {
            emit.emit(
                "correctness/literal-mutation",
                arg.text_range(),
                format!(
                    "passing a literal datum to destructive `{}` has undefined consequences",
                    op.to_ascii_lowercase()
                ),
                Some("construct a fresh copy with `list`, `copy-list`, or `copy-seq`".into()),
                Vec::new(),
                None,
            );
        }
    }
}

pub(crate) fn check_setf_literal_place(
    ctx: &LintDocument<'_>,
    place: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(node) = place.as_node() else {
        return;
    };
    if node.kind() != SyntaxKind::List || !closed_list(node) {
        return;
    }
    let Some(op) = operator(node) else {
        return;
    };
    if matches!(
        op.as_str(),
        "CAR"
            | "CDR"
            | "FIRST"
            | "SECOND"
            | "THIRD"
            | "REST"
            | "NTH"
            | "AREF"
            | "SVREF"
            | "CHAR"
            | "SCHAR"
            | "ELT"
    ) {
        let target_arg = if op == "NTH" {
            elements(node).nth(2)
        } else {
            elements(node).nth(1)
        };
        if let Some(arg) = target_arg
            && is_literal_compound(&arg, package, ctx.workspace)
        {
            emit.emit(
                "correctness/literal-mutation",
                arg.text_range(),
                format!(
                    "mutating a literal via `(setf ({} ...) ...)` has undefined consequences",
                    op.to_ascii_lowercase()
                ),
                Some("mutate a freshly allocated object instead of a quoted literal".into()),
                Vec::new(),
                None,
            );
        }
    }
}

fn check_defconstant_value(
    ctx: &LintDocument<'_>,
    val_el: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let is_non_eql_literal = is_literal_compound(val_el, package, ctx.workspace)
        || val_el.as_node().is_some_and(|n| {
            closed_list(n)
                && elements(n)
                    .next()
                    .and_then(|e| symbol(&e))
                    .is_some_and(|s| {
                        ctx.workspace
                            .standard_symbol(&s, package)
                            .is_some_and(|op| {
                                matches!(
                                    op,
                                    "LIST"
                                        | "CONS"
                                        | "VECTOR"
                                        | "MAKE-ARRAY"
                                        | "MAKE-HASH-TABLE"
                                        | "MAKE-LIST"
                                        | "COPY-LIST"
                                        | "APPEND"
                                )
                            })
                    })
        });
    if is_non_eql_literal {
        emit.emit(
            "correctness/defconstant-non-eql-literal",
            val_el.text_range(),
            "`defconstant` initialized with a fresh or compound object will fail EQL redefinition checks on reload",
            Some(
                "use `defparameter` (or `alexandria:define-constant`) for non-EQL constants"
                    .into(),
            ),
            Vec::new(),
            None,
        );
    }
}

pub(crate) fn check_case_form(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let is_exhaustive = matches!(op, "ECASE" | "CCASE");
    let clauses = args.get(1..).unwrap_or(&[]);
    let mut seen_keys: HashSet<String> = HashSet::new();

    for (idx, clause) in clauses.iter().enumerate() {
        let Some(clause_node) = clause.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let Some(key_el) = elements(clause_node).next() else {
            continue;
        };
        let is_last = idx + 1 == clauses.len();

        if let Some(sym) = symbol(&key_el) {
            let std = ctx.workspace.standard_symbol(&sym, package);
            if matches!(std, Some("T"))
                || (matches!(sym.prefix, Prefix::None) && sym.name == "OTHERWISE")
            {
                if is_exhaustive {
                    emit.emit(
                        "correctness/case-non-final-default",
                        key_el.text_range(),
                        format!(
                            "`{}` does not allow a default `{}` clause; write `(({}) ...)` to match the symbol",
                            op.to_ascii_lowercase(),
                            sym.name.to_ascii_lowercase(),
                            sym.name.to_ascii_lowercase()
                        ),
                        None,
                        Vec::new(),
                        None,
                    );
                } else if !is_last {
                    emit.emit(
                        "correctness/case-non-final-default",
                        clause.text_range(),
                        format!(
                            "catch-all `{}` clause before the end of `{}` makes following clauses unreachable",
                            sym.name.to_ascii_lowercase(),
                            op.to_ascii_lowercase()
                        ),
                        Some(
                            "move this clause to the end or wrap the symbol in parens `((t) ...)`"
                                .into(),
                        ),
                        Vec::new(),
                        None,
                    );
                }
                continue;
            }
        }

        if let Some(inner) = unwrap_quote(&key_el, package, ctx.workspace) {
            let inner_text = ctx.text_of(inner.text_range());
            let fix = inner.as_token().is_some().then(|| LintFix {
                title: format!(
                    "Remove redundant quote in `{}` key",
                    op.to_ascii_lowercase()
                ),
                applicability: FixApplicability::MaybeIncorrect,
                edits: vec![TextEdit {
                    range: key_el.text_range(),
                    replacement: inner_text.to_owned(),
                }],
            });
            emit.emit(
                "correctness/case-quoted-key",
                key_el.text_range(),
                format!(
                    "`{}` does not evaluate its keys; `'...` actually expands to `(quote ...)`",
                    op.to_ascii_lowercase()
                ),
                Some(format!("write `{inner_text}` without a quote")),
                Vec::new(),
                fix,
            );
            continue;
        }

        if let Some(key_list) = key_el
            .as_node()
            .filter(|n| n.kind() == SyntaxKind::List && closed_list(n))
        {
            let mut items = elements(key_list).peekable();
            if items.peek().is_none() {
                emit.emit(
                    "correctness/case-nil-key",
                    key_el.text_range(),
                    format!(
                        "empty key list `()` in `{}` never matches any object",
                        op.to_ascii_lowercase()
                    ),
                    Some("add keys or write `((nil) ...)` to match the symbol `nil`".into()),
                    Vec::new(),
                    None,
                );
                continue;
            }
            for item in items {
                check_single_case_key(op, &item, &mut seen_keys, emit);
            }
        } else if key_el.as_token().is_some() && is_nil_element(&key_el, package, ctx.workspace) {
            emit.emit(
                "correctness/case-nil-key",
                key_el.text_range(),
                format!(
                    "`{}` key `nil` means an empty key list `()` and never matches the symbol `nil`",
                    op.to_ascii_lowercase()
                ),
                Some("write `((nil) ...)` to match the symbol `nil`".into()),
                Vec::new(),
                Some(LintFix {
                    title: "Wrap `nil` key in parentheses `((nil) ...)`".into(),
                    applicability: FixApplicability::MaybeIncorrect,
                    edits: vec![TextEdit {
                        range: key_el.text_range(),
                        replacement: format!("({})", ctx.text_of(key_el.text_range())),
                    }],
                }),
            );
        } else {
            check_single_case_key(op, &key_el, &mut seen_keys, emit);
        }
    }
}

fn check_single_case_key(
    op: &str,
    key: &Element,
    seen_keys: &mut HashSet<String>,
    emit: &mut LintEmitter<'_>,
) {
    if is_string_literal_el(key) || key.as_node().is_some() {
        emit.emit(
            "correctness/case-string-or-list-key",
            key.text_range(),
            format!(
                "`{}` compares keys with `eql`, so strings or lists will not match by content",
                op.to_ascii_lowercase()
            ),
            Some("use `cond` with `string=` / `equal` instead".into()),
            Vec::new(),
            None,
        );
        return;
    }
    let norm = normalized_sexpr(key);
    if !norm.is_empty() && !seen_keys.insert(norm.clone()) {
        emit.emit(
            "correctness/duplicate-case-key",
            key.text_range(),
            format!(
                "duplicate key `{}` in `{}`",
                norm.to_ascii_lowercase(),
                op.to_ascii_lowercase()
            ),
            None,
            Vec::new(),
            None,
        );
    }
}

pub(crate) fn check_eval_when_form(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(sit_el) = args.first() else {
        return;
    };
    let mut legacy = Vec::new();
    check_eval_when_warnings(sit_el.clone(), package, ctx.workspace, &mut legacy);
    for w in legacy {
        let mut tags = Vec::new();
        let mut fix = None;
        if let Some(rest) = w.message.strip_prefix("deprecated `eval-when` situation `")
            && let Some((old, modern_part)) = rest.split_once("`; use `")
            && let Some(modern) = modern_part.strip_suffix('`')
        {
            tags.push(LintTag::Deprecated);
            fix = Some(LintFix {
                title: format!("Replace deprecated `{old}` with `{modern}`"),
                applicability: FixApplicability::MachineApplicable,
                edits: vec![TextEdit {
                    range: w.range,
                    replacement: modern.to_owned(),
                }],
            });
        }
        emit.emit(
            "correctness/eval-when-invalid",
            w.range,
            w.message,
            None,
            tags,
            fix,
        );
    }
}

pub(crate) fn check_zk_match_form(
    ctx: &LintDocument<'_>,
    offset: TextSize,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let query = ScopeAtOffset {
        package,
        offset,
        workspace: ctx.workspace,
        known_features: ctx.source.known_features(),
        macro_operands: None,
    };
    let mut legacy = Vec::new();
    query.match_warnings(args, &mut legacy);
    for w in legacy {
        emit.emit(
            "correctness/zk-match-mismatch",
            w.range,
            w.message,
            None,
            Vec::new(),
            None,
        );
    }
}

fn slot_name_element(slot: Element) -> Option<Element> {
    if let Some(inner) = slot.as_node().filter(|n| closed_list(n)) {
        elements(inner).next()
    } else {
        Some(slot)
    }
}

pub(crate) fn check_duplicate_defclass_slots(
    ctx: &LintDocument<'_>,
    package: Option<&str>,
    slots_el: &Element,
    emit: &mut LintEmitter<'_>,
) {
    let Some(node) = slots_el.as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let mut seen: HashSet<(Option<String>, String)> = HashSet::new();
    for slot in elements(node) {
        let Some(name_el) = slot_name_element(slot) else {
            continue;
        };
        let Some(sym) = symbol(&name_el) else {
            continue;
        };
        if sym.prefix == Prefix::Uninterned {
            continue;
        }
        let home = ctx
            .workspace
            .symbol_home_package(&sym, package)
            .or(match &sym.prefix {
                Prefix::Package { name, .. } => Some(name.as_str()),
                Prefix::Keyword => Some("KEYWORD"),
                Prefix::None => package,
                Prefix::Uninterned => None,
            })
            .map(str::to_owned);
        let display_name = sym.name.to_ascii_lowercase();
        if !seen.insert((home, sym.name)) {
            emit.emit(
                "correctness/duplicate-binding",
                name_el.text_range(),
                format!("duplicate slot name `{display_name}`"),
                None,
                Vec::new(),
                None,
            );
        }
    }
}

pub(crate) fn check_duplicate_defstruct_slots(args: &[Element], emit: &mut LintEmitter<'_>) {
    let mut seen: HashSet<String> = HashSet::new();
    for slot in args.iter().skip(1) {
        if slot
            .as_token()
            .is_some_and(|t| t.kind() != SyntaxKind::Atom)
        {
            continue;
        }
        let Some(name_el) = slot_name_element(slot.clone()) else {
            continue;
        };
        if let Some(sym) = symbol(&name_el)
            && !seen.insert(sym.name.clone())
        {
            emit.emit(
                "correctness/duplicate-binding",
                name_el.text_range(),
                format!(
                    "duplicate slot name `{}` in `defstruct`",
                    sym.name.to_ascii_lowercase()
                ),
                None,
                Vec::new(),
                None,
            );
        }
    }
}

pub(crate) fn check_quoted_type_specifier(
    ctx: &LintDocument<'_>,
    type_spec: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("correctness/quoted-type-specifier") {
        return;
    }
    let Some(inner) = unwrap_quote(type_spec, package, ctx.workspace) else {
        return;
    };
    let inner_text = ctx.text_of(inner.text_range());
    let fix = (!inner_text.is_empty()).then(|| LintFix {
        title: "Remove unnecessary quote on type specifier".into(),
        applicability: FixApplicability::MaybeIncorrect,
        edits: vec![TextEdit {
            range: type_spec.text_range(),
            replacement: inner_text.to_owned(),
        }],
    });
    emit.emit(
        "correctness/quoted-type-specifier",
        type_spec.text_range(),
        "type specifier is not evaluated and should not be quoted",
        Some(format!("write `{inner_text}` without a quote")),
        Vec::new(),
        fix,
    );
}

pub(crate) fn check_declaration_form(
    ctx: &LintDocument<'_>,
    decl: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(node) = decl.as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let Some(parts) = ctx.read_elements(elements(node)) else {
        return;
    };
    if parts.len() > 1 {
        check_declaration_specs(ctx, &parts[1..], package, emit);
    }
}

pub(crate) fn check_declaration_specs(
    ctx: &LintDocument<'_>,
    specs: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let enabled = emit.is_enabled("correctness/invalid-declaration")
        || emit.is_enabled("correctness/quoted-type-specifier");
    if !enabled {
        return;
    }
    for spec in specs {
        let Some(node) = spec.as_node() else {
            if spec
                .as_token()
                .is_some_and(|t| t.kind() == SyntaxKind::Atom)
            {
                emit.emit(
                    "correctness/invalid-declaration",
                    spec.text_range(),
                    "declaration specifier must be a parenthesized list",
                    Some("wrap the declaration in parentheses, e.g. `(ignore x)`".into()),
                    Vec::new(),
                    None,
                );
            }
            continue;
        };
        if !closed_list(node) {
            continue;
        }
        let Some(parts) = ctx.read_elements(elements(node)) else {
            continue;
        };
        let Some(head) = parts.first().and_then(symbol) else {
            continue;
        };
        let Some(std_decl) = ctx.workspace.standard_symbol(&head, package) else {
            continue;
        };
        match std_decl {
            "OPTIMIZE" => {
                for clause in &parts[1..] {
                    if let Some(cnode) = clause.as_node() {
                        if !closed_list(cnode) {
                            continue;
                        }
                        let Some(cparts) = ctx.read_elements(elements(cnode)) else {
                            continue;
                        };
                        if cparts.len() != 2 || cparts.first().and_then(symbol).is_none() {
                            emit.emit(
                                "correctness/invalid-declaration",
                                clause.text_range(),
                                "`optimize` entry must be a quality symbol or `(quality level)`",
                                Some(
                                    "use an integer optimization level between `0` and `3`".into(),
                                ),
                                Vec::new(),
                                None,
                            );
                            continue;
                        }
                        let level_ok = integer_literal_value(&cparts[1])
                            .is_some_and(|level| (0..=3).contains(&level));
                        if !level_ok {
                            emit.emit(
                                "correctness/invalid-declaration",
                                cparts[1].text_range(),
                                "`optimize` level must be an integer between `0` and `3`",
                                Some("replace with `0`, `1`, `2`, or `3`".into()),
                                Vec::new(),
                                None,
                            );
                        }
                    } else if symbol(clause).is_none() {
                        emit.emit(
                            "correctness/invalid-declaration",
                            clause.text_range(),
                            "`optimize` entry must be a quality symbol or `(quality level)`",
                            None,
                            Vec::new(),
                            None,
                        );
                    }
                }
            }
            "TYPE" | "FTYPE" => {
                if let Some(type_spec) = parts.get(1) {
                    check_quoted_type_specifier(ctx, type_spec, package, emit);
                } else {
                    emit.emit(
                        "correctness/invalid-declaration",
                        spec.text_range(),
                        format!(
                            "`{}` declaration requires a type specifier",
                            std_decl.to_ascii_lowercase()
                        ),
                        None,
                        Vec::new(),
                        None,
                    );
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn check_defpackage_options(
    ctx: &LintDocument<'_>,
    args: &[Element],
    _package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("correctness/invalid-defpackage-option") {
        return;
    }
    let mut seen_doc = false;
    let mut seen_size = false;
    for opt in args.iter().skip(1) {
        let Some(opt_node) = opt.as_node() else {
            if opt.as_token().is_some_and(|t| t.kind() == SyntaxKind::Atom) {
                emit.emit(
                    "correctness/invalid-defpackage-option",
                    opt.text_range(),
                    "`defpackage` option must be a parenthesized list",
                    None,
                    Vec::new(),
                    None,
                );
            }
            continue;
        };
        if !closed_list(opt_node) {
            continue;
        }
        let Some(parts) = ctx.read_elements(elements(opt_node)) else {
            continue;
        };
        let Some(head) = parts.first().and_then(symbol) else {
            emit.emit(
                "correctness/invalid-defpackage-option",
                opt.text_range(),
                "`defpackage` option must start with an option keyword",
                None,
                Vec::new(),
                None,
            );
            continue;
        };
        if head.prefix != Prefix::Keyword {
            emit.emit(
                "correctness/invalid-defpackage-option",
                parts[0].text_range(),
                format!(
                    "`defpackage` option `{}` should be a keyword",
                    ctx.text_of(parts[0].text_range())
                ),
                Some(format!("use `:{}`", head.name.to_ascii_lowercase())),
                Vec::new(),
                None,
            );
            continue;
        }
        match head.name.as_str() {
            "NICKNAMES" | "USE" | "SHADOW" | "EXPORT" | "INTERN" | "LOCK" | "IMPLEMENT" => {}
            "IMPORT-FROM" | "SHADOWING-IMPORT-FROM" => {
                if parts.len() < 2 {
                    emit.emit(
                        "correctness/invalid-defpackage-option",
                        opt.text_range(),
                        format!(
                            "`:{}` option requires a source package designator",
                            head.name.to_ascii_lowercase()
                        ),
                        None,
                        Vec::new(),
                        None,
                    );
                }
            }
            "DOCUMENTATION" => {
                if seen_doc {
                    emit.emit(
                        "correctness/invalid-defpackage-option",
                        opt.text_range(),
                        "duplicate `:documentation` option in `defpackage`",
                        None,
                        Vec::new(),
                        None,
                    );
                } else if parts.len() != 2 || !is_string_literal_el(&parts[1]) {
                    emit.emit(
                        "correctness/invalid-defpackage-option",
                        opt.text_range(),
                        "`:documentation` in `defpackage` expects a single string literal",
                        None,
                        Vec::new(),
                        None,
                    );
                }
                seen_doc = true;
            }
            "SIZE" => {
                if seen_size {
                    emit.emit(
                        "correctness/invalid-defpackage-option",
                        opt.text_range(),
                        "duplicate `:size` option in `defpackage`",
                        None,
                        Vec::new(),
                        None,
                    );
                } else if parts.len() != 2
                    || integer_literal_value(&parts[1]).is_none_or(|size| size < 0)
                {
                    emit.emit(
                        "correctness/invalid-defpackage-option",
                        opt.text_range(),
                        "`:size` in `defpackage` expects a single non-negative integer",
                        None,
                        Vec::new(),
                        None,
                    );
                }
                seen_size = true;
            }
            "LOCAL-NICKNAMES" => {
                for entry in &parts[1..] {
                    let valid = entry
                        .as_node()
                        .filter(|n| closed_list(n))
                        .and_then(|n| ctx.read_elements(elements(n)))
                        .is_some_and(|pair| pair.len() == 2);
                    if !valid {
                        emit.emit(
                            "correctness/invalid-defpackage-option",
                            entry.text_range(),
                            "`:local-nicknames` entries must have the form `(nickname package)`",
                            None,
                            Vec::new(),
                            None,
                        );
                    }
                }
            }
            _ => {
                emit.emit(
                    "correctness/invalid-defpackage-option",
                    parts[0].text_range(),
                    format!(
                        "unknown `defpackage` option `:{}`",
                        head.name.to_ascii_lowercase()
                    ),
                    None,
                    Vec::new(),
                    None,
                );
            }
        }
    }
}

fn check_mapcan_literal(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(first) = args.first() else {
        return;
    };
    let lambda_node = if let Some(node) = first.as_node()
        && node.kind() == SyntaxKind::ReaderForm
    {
        if !ctx.text_of(node.text_range()).starts_with("#'") {
            return;
        }
        elements(node).nth(1).and_then(|e| e.into_node())
    } else if let Some(node) = first.as_node()
        && closed_list(node)
    {
        let elems: Vec<_> = elements(node).collect();
        let Some(head) = elems.first().and_then(symbol) else {
            return;
        };
        if ctx.workspace.standard_symbol(&head, package) == Some("FUNCTION") && elems.len() == 2 {
            elems[1].as_node().cloned()
        } else {
            Some(node.clone())
        }
    } else {
        None
    };
    let Some(lambda) = lambda_node.filter(closed_list) else {
        return;
    };
    let elems: Vec<_> = elements(&lambda).collect();
    let Some(head) = elems.first().and_then(symbol) else {
        return;
    };
    if ctx.workspace.standard_symbol(&head, package) != Some("LAMBDA") || elems.len() < 3 {
        return;
    }
    let Some(last) = elems.last() else {
        return;
    };
    check_returned_quoted_list(ctx, op, last, package, emit, 0);
}

fn check_returned_quoted_list(
    ctx: &LintDocument<'_>,
    op: &str,
    expr: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    if let Some(quoted) = unwrap_quote(expr, package, ctx.workspace) {
        if quoted
            .as_node()
            .is_some_and(|n| n.kind() == SyntaxKind::List && elements(n).next().is_some())
        {
            emit.emit(
                "correctness/literal-mutation",
                expr.text_range(),
                format!(
                    "`{}` destructively concatenates return values with `nconc`, mutating this quoted list literal",
                    op.to_ascii_lowercase()
                ),
                Some("return a freshly allocated list via `list` or `copy-list`".into()),
                Vec::new(),
                None,
            );
        }
        return;
    }
    let Some(node) = expr.as_node().filter(|n| closed_list(n)) else {
        return;
    };
    let elems: Vec<_> = elements(node).collect();
    let Some(head) = elems.first().and_then(symbol) else {
        return;
    };
    match ctx.workspace.standard_symbol(&head, package) {
        Some("IF") => {
            if let Some(t) = elems.get(2) {
                check_returned_quoted_list(ctx, op, t, package, emit, depth + 1);
            }
            if let Some(f) = elems.get(3) {
                check_returned_quoted_list(ctx, op, f, package, emit, depth + 1);
            }
        }
        Some("WHEN" | "UNLESS" | "PROGN") => {
            if let Some(last) = elems.last().filter(|_| elems.len() > 1) {
                check_returned_quoted_list(ctx, op, last, package, emit, depth + 1);
            }
        }
        _ => {}
    }
}
