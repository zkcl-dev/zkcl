//! Suspicious semantic checks: control/type subsumption, condition swallowing,
//! pure discarded expressions, unreachable code, CLOS/resource/multi-value traps.

use rustc_hash::FxHashMap;

use crate::navigation::ScopeBinding;

use crate::navigation::index::{closed_list, symbol};
use crate::navigation::queries::find_local_fn;
use crate::outline::{Element, elements};
use crate::reader_tokens::Prefix;
use crate::syntax::{SyntaxKind, SyntaxNode};

use super::super::LintTag;
use super::super::context::{
    FormContext, LintDocument, LintEmitter, cl_predicates_disjoint, is_cl_type_subsumed,
    is_literal_compound, is_nil_element, is_pure_expr, is_string_literal_el, is_t_element,
    normalized_sexpr, repeatable_test, unwrap_quote,
};

use super::{CallSite, ScopeContext, body_after_declarations};

pub(crate) fn check_call_suspicious(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    emit: &mut LintEmitter<'_>,
) {
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    let CallSite {
        list, args, std_op, ..
    } = call;
    let Some(op) = std_op else {
        return;
    };

    match op {
        "MEMBER" | "ASSOC" | "RASSOC" | "FIND" | "POSITION" | "COUNT" | "REMOVE" | "DELETE"
        | "ADJOIN" => {
            check_default_eql_on_string(ctx, op, args, package, emit);
        }
        "SETF" | "PSETF" | "SETQ" | "PSETQ" => {
            for pair in args.as_chunks::<2>().0 {
                check_place_validity(ctx, op, &pair[0], package, emit);
                if matches!(op, "SETF" | "SETQ")
                    && symbol(&pair[0]).is_some()
                    && repeatable_test(ctx, &pair[0], package, lexicals, env).is_some_and(|key| {
                        repeatable_test(ctx, &pair[1], package, lexicals, env).as_ref()
                            == Some(&key)
                    })
                {
                    emit.emit(
                        "suspicious/self-assignment",
                        pair[0].text_range().cover(pair[1].text_range()),
                        format!(
                            "self-assignment `({} {} ...)` has no effect",
                            op.to_ascii_lowercase(),
                            ctx.text_of(pair[0].text_range())
                        ),
                        None,
                        Vec::new(),
                        None,
                    );
                }
            }
        }
        "INCF" | "DECF" | "POP" => {
            if let Some(place) = args.first() {
                check_place_validity(ctx, op, place, package, emit);
            }
        }
        "PUSH" | "PUSHNEW" => {
            if let Some(place) = args.get(1) {
                check_place_validity(ctx, op, place, package, emit);
            }
        }
        "WHEN" | "UNLESS" => {
            if args.len() == 1 {
                emit.emit(
                    "suspicious/empty-body",
                    list.text_range(),
                    format!(
                        "`{}` has an empty body and always returns `nil`",
                        op.to_ascii_lowercase()
                    ),
                    Some("add body forms or check closing parentheses".into()),
                    Vec::new(),
                    None,
                );
            }
            if let Some(test) = args.first() {
                check_constant_test(ctx, op, test, package, emit);
                check_enclosing_condition_redundancy(ctx, test, package, lexicals, env, emit);
            }
        }
        "IF" => {
            if let Some(test) = args.first() {
                check_constant_test(ctx, op, test, package, emit);
                check_enclosing_condition_redundancy(ctx, test, package, lexicals, env, emit);
            }
            if args.len() == 3
                && is_pure_expr(&args[1], package, ctx.workspace)
                && normalized_sexpr(&args[1]) == normalized_sexpr(&args[2])
            {
                emit.emit(
                    "suspicious/identical-branches",
                    list.text_range(),
                    "`if` has identical `then` and `else` branches",
                    None,
                    Vec::new(),
                    None,
                );
            }
        }
        "EQ" | "EQL" | "EQUAL" | "EQUALP" | "=" | "/=" | "<" | "<=" | ">" | ">=" | "STRING="
        | "STRING/=" | "STRING-EQUAL" | "CHAR=" | "CHAR/=" => {
            if args.len() == 2
                && repeatable_test(ctx, &args[0], package, lexicals, env).is_some_and(|key| {
                    repeatable_test(ctx, &args[1], package, lexicals, env).as_ref() == Some(&key)
                })
            {
                emit.emit(
                    "suspicious/self-comparison",
                    list.text_range(),
                    format!(
                        "`{}` compares `{}` against itself",
                        op.to_ascii_lowercase(),
                        ctx.text_of(args[0].text_range())
                    ),
                    None,
                    Vec::new(),
                    None,
                );
            }
        }
        "IGNORE-ERRORS" => {
            if !env.in_defmacro && env.result_discarded {
                emit.emit(
                    "suspicious/swallowed-condition",
                    list.text_range(),
                    "`ignore-errors` suppresses errors and its result is discarded",
                    Some("handle the returned condition or narrow the handler to the failure you expect".into()),
                    Vec::new(),
                    None,
                );
            }
        }
        "AND" | "OR" => {
            check_duplicate_operands(ctx, op, args, package, lexicals, env, emit);
            check_and_or_suspicious(ctx, op, args, package, lexicals, env, emit);
        }
        "-" => {
            check_duplicate_operands(ctx, op, args, package, lexicals, env, emit);
        }
        "HANDLER-CASE" => check_handler_case(ctx, args, package, emit),
        "TYPECASE" | "ETYPECASE" | "CTYPECASE" => check_typecase(ctx, op, args, package, emit),
        "COND" => check_cond_clauses(ctx, args, package, lexicals, env, emit),
        "MULTIPLE-VALUE-BIND"
        | "MULTIPLE-VALUE-SETQ"
        | "NTH-VALUE"
        | "PROG1"
        | "MULTIPLE-VALUE-CALL" => {
            super::values::check(scope, list, op, args, emit);
        }
        "OPEN" if !env.in_open_guard && env.protected_loop_open != Some(list.text_range()) => {
            emit.emit(
                "suspicious/resource-leak-open",
                list.text_range(),
                "`open` called outside `with-open-file` or `unwind-protect`",
                Some("prefer `with-open-file` so the stream is closed on non-local exit".into()),
                Vec::new(),
                None,
            );
        }
        _ => {}
    }

    if env.result_discarded
        && emit.is_enabled("suspicious/discarded-pure-expression")
        && is_discardable_pure_call(op)
        && is_pure_expr(&Element::Node(list.clone()), package, ctx.workspace)
    {
        emit.emit(
            "suspicious/discarded-pure-expression",
            list.text_range(),
            format!(
                "result of pure `{}` expression is discarded",
                op.to_ascii_lowercase()
            ),
            Some("bind the result, return it, or remove the dead expression".into()),
            vec![LintTag::Unnecessary],
            None,
        );
    }
}

fn check_duplicate_operands(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("suspicious/duplicate-operand") || (op == "-" && args.len() != 2) {
        return;
    }
    let mut seen = FxHashMap::default();
    for arg in args {
        let Some(key) = repeatable_test(ctx, arg, package, lexicals, env) else {
            seen.clear();
            continue;
        };
        if let Some(previous) = seen.insert(key, arg.text_range())
            && let Some(diag) = emit.diagnostic(
                "suspicious/duplicate-operand",
                arg.text_range(),
                format!(
                    "duplicate operand `{}` in `{}`",
                    ctx.text_of(arg.text_range()),
                    op.to_ascii_lowercase()
                ),
                None,
                Vec::new(),
                None,
            )
        {
            diag.note(previous, "same operand evaluated earlier here");
        }
    }
}

fn is_discardable_pure_call(op: &str) -> bool {
    matches!(
        op,
        "+" | "-"
            | "*"
            | "/"
            | "1+"
            | "1-"
            | "ABS"
            | "MIN"
            | "MAX"
            | "MOD"
            | "REM"
            | "FLOOR"
            | "CEILING"
            | "TRUNCATE"
            | "ROUND"
            | "NOT"
            | "NULL"
            | "EQ"
            | "EQL"
            | "EQUAL"
            | "EQUALP"
            | "="
            | "/="
            | "<"
            | "<="
            | ">"
            | ">="
            | "ZEROP"
            | "PLUSP"
            | "MINUSP"
            | "EVENP"
            | "ODDP"
            | "CONSP"
            | "ATOM"
            | "LISTP"
            | "NUMBERP"
            | "STRINGP"
            | "SYMBOLP"
            | "CHARACTERP"
            | "VECTORP"
            | "ARRAYP"
            | "TYPEP"
            | "CAR"
            | "CDR"
            | "FIRST"
            | "SECOND"
            | "THIRD"
            | "FOURTH"
            | "REST"
            | "CONS"
            | "LIST"
            | "LIST*"
            | "VECTOR"
            | "LENGTH"
            | "APPEND"
            | "REVERSE"
            | "COPY-LIST"
            | "COPY-SEQ"
            | "SUBSEQ"
            | "LAST"
            | "BUTLAST"
            | "STRING="
            | "STRING-EQUAL"
            | "CHAR="
            | "CHAR-EQUAL"
    )
}

fn check_default_eql_on_string(
    _ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if args.len() < 2 || !is_string_literal_el(&args[0]) {
        return;
    }
    let has_test_kw = args[2..].iter().any(|arg| {
        symbol(arg).is_some_and(|s| {
            s.prefix == Prefix::Keyword && matches!(s.name.as_str(), "TEST" | "TEST-NOT")
        })
    });
    if !has_test_kw {
        emit.emit(
            "suspicious/default-eql-on-string",
            args[0].text_range(),
            format!(
                "`{}` defaults to `:test #'eql`, which does not match distinct string values",
                op.to_ascii_lowercase()
            ),
            Some("pass `:test #'string=` or `:test #'equal`".into()),
            Vec::new(),
            None,
        );
    }
    let _ = package;
}

fn check_place_validity(
    ctx: &LintDocument<'_>,
    op: &str,
    place: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if let Some(name) = symbol(place)
        && let Some((document, definition)) = ctx.workspace.lookup_unique_declaration_parts(
            crate::navigation::Namespace::Value,
            &name,
            package,
        )
        && definition.operator.name == "DEFCONSTANT"
    {
        if let Some(diag) = emit.diagnostic(
            "correctness/assign-to-constant",
            place.text_range(),
            "this place names a constant variable",
            None,
            Vec::new(),
            None,
        ) {
            diag.related.push(super::super::LintRelated {
                document: Some(document.to_owned()),
                range: definition.range,
                message: "the constant is defined here".into(),
            });
        }
        return;
    }
    if is_string_literal_el(place)
        || place.as_token().is_some_and(|t| {
            matches!(t.kind(), SyntaxKind::Character)
                || super::super::context::is_number_literal_el(place)
        })
        || unwrap_quote(place, package, ctx.workspace).is_some()
        || is_t_element(place, package, ctx.workspace)
        || is_nil_element(place, package, ctx.workspace)
        || symbol(place).is_some_and(|name| name.prefix == Prefix::Keyword)
        || place
            .as_node()
            .is_some_and(|n| n.kind() == SyntaxKind::Vector)
    {
        emit.emit(
            "suspicious/setf-non-place",
            place.text_range(),
            format!(
                "`{}` target `{}` is a literal/constant rather than a writable place",
                op.to_ascii_lowercase(),
                ctx.text_of(place.text_range())
            ),
            None,
            Vec::new(),
            None,
        );
    } else if matches!(op, "SETQ" | "PSETQ") && place.kind() == SyntaxKind::List {
        emit.emit(
            "correctness/invalid-setq-target",
            place.text_range(),
            format!(
                "`{}` requires a variable name in this position",
                op.to_ascii_lowercase()
            ),
            Some(format!(
                "use `{}` when assigning through an accessor",
                if op == "SETQ" { "setf" } else { "psetf" }
            )),
            Vec::new(),
            None,
        );
    }
}

fn check_constant_test(
    ctx: &LintDocument<'_>,
    op: &str,
    test: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if is_t_element(test, package, ctx.workspace) {
        emit.emit(
            "suspicious/constant-condition",
            test.text_range(),
            format!(
                "condition in `{}` is always true (`t`)",
                op.to_ascii_lowercase()
            ),
            None,
            Vec::new(),
            None,
        );
    } else if is_nil_element(test, package, ctx.workspace) {
        emit.emit(
            "suspicious/constant-condition",
            test.text_range(),
            format!(
                "condition in `{}` is always false (`nil`)",
                op.to_ascii_lowercase()
            ),
            None,
            Vec::new(),
            None,
        );
    }
}

fn unwrap_negated(
    ctx: &LintDocument<'_>,
    el: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> (Element, bool) {
    let mut current = el.clone();
    let mut negated = false;
    while let Some(node) = current.as_node().filter(|n| closed_list(n)) {
        let mut parts = elements(node);
        let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
            break;
        };
        let Some(arg) = parts.next() else {
            break;
        };
        if parts.next().is_some()
            || !matches!(
                ctx.std_op(&head, package, lexicals, env),
                Some("NOT" | "NULL")
            )
        {
            break;
        }
        negated = !negated;
        current = arg;
    }
    (current, negated)
}

fn check_and_or_suspicious(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    let check_contradict = emit.is_enabled("suspicious/contradictory-boolean");
    let check_const = emit.is_enabled("suspicious/constant-condition");
    let check_unreach = emit.is_enabled("suspicious/unreachable-code");
    if !check_contradict && !check_const && !check_unreach {
        return;
    }

    let mut seen_polarity: FxHashMap<String, (bool, crate::syntax::TextRange)> =
        FxHashMap::default();
    let mut seen_preds: FxHashMap<String, Vec<(&'static str, crate::syntax::TextRange)>> =
        FxHashMap::default();
    let mut short_circuited = false;

    for (idx, arg) in args.iter().enumerate() {
        let is_last = idx + 1 == args.len();
        let is_nil = is_nil_element(arg, package, ctx.workspace);
        let is_t = is_t_element(arg, package, ctx.workspace);

        if short_circuited {
            if check_unreach {
                emit.emit(
                    "suspicious/unreachable-code",
                    arg.text_range(),
                    format!(
                        "unreachable operand in `{}` after constant short-circuit",
                        op.to_ascii_lowercase()
                    ),
                    Some("remove unreachable operands or fix the preceding constant".into()),
                    vec![LintTag::Unnecessary],
                    None,
                );
            }
            break;
        }

        if op == "AND" {
            if is_nil && !is_last {
                short_circuited = true;
                continue;
            }
            if is_t && !is_last && check_const {
                emit.emit(
                    "suspicious/constant-condition",
                    arg.text_range(),
                    "`t` is always truthy in `and` and can be omitted here",
                    None,
                    vec![LintTag::Unnecessary],
                    None,
                );
            }
        } else {
            if is_t && !is_last {
                short_circuited = true;
                continue;
            }
            if is_nil && !is_last && check_const {
                emit.emit(
                    "suspicious/constant-condition",
                    arg.text_range(),
                    "`nil` is always false in `or` and can be omitted",
                    None,
                    vec![LintTag::Unnecessary],
                    None,
                );
            }
        }

        if !check_contradict {
            continue;
        }
        let (inner, negated) = unwrap_negated(ctx, arg, package, lexicals, env);
        let Some(key) = repeatable_test(ctx, &inner, package, lexicals, env) else {
            // Reset tracked facts across any non-repeatable / side-effecting operand.
            seen_polarity.clear();
            seen_preds.clear();
            continue;
        };

        if let Some(&(prev_neg, prev_range)) = seen_polarity.get(&key) {
            if prev_neg != negated {
                let msg = if op == "AND" {
                    "contradictory operands in `and`: condition and its negation cannot both hold"
                } else {
                    "tautological operands in `or`: condition or its negation always holds"
                };
                if let Some(diag) = emit.diagnostic(
                    "suspicious/contradictory-boolean",
                    arg.text_range(),
                    msg,
                    None,
                    Vec::new(),
                    None,
                ) {
                    diag.note(prev_range, "opposite condition checked earlier here");
                }
            }
        } else {
            seen_polarity.insert(key, (negated, arg.text_range()));
        }

        if op == "AND"
            && !negated
            && let Some(node) = inner.as_node().filter(|n| closed_list(n))
        {
            let mut parts = elements(node);
            if let Some(head_el) = parts.next()
                && let Some(arg_el) = parts.next()
                && parts.next().is_none()
                && let Some(pred_sym) = symbol(&head_el)
                && find_local_fn(lexicals, &pred_sym, package, ctx.workspace).is_none()
                && !env.is_fn_shadowed(&pred_sym)
                && let Some(std_pred) = ctx.workspace.standard_symbol(&pred_sym, package)
                && let Some(target_key) = repeatable_test(ctx, &arg_el, package, lexicals, env)
            {
                let static_pred = canon_disjoint_pred(std_pred);
                if let Some(pred_name) = static_pred {
                    let entries = seen_preds.entry(target_key).or_default();
                    if let Some(&(prev_pred, prev_range)) = entries
                        .iter()
                        .find(|&&(prev, _)| cl_predicates_disjoint(prev, pred_name))
                        && let Some(diag) = emit.diagnostic(
                            "suspicious/contradictory-boolean",
                            arg.text_range(),
                            format!(
                                "disjoint predicates `{}` and `{}` cannot both hold on `{}`",
                                prev_pred.to_ascii_lowercase(),
                                pred_name.to_ascii_lowercase(),
                                ctx.text_of(arg_el.text_range())
                            ),
                            None,
                            Vec::new(),
                            None,
                        )
                    {
                        diag.note(prev_range, "disjoint predicate checked earlier here");
                    }
                    entries.push((pred_name, arg.text_range()));
                }
            }
        }
    }
}

fn canon_disjoint_pred(op: &str) -> Option<&'static str> {
    match op {
        "ATOM" => Some("ATOM"),
        "CONSP" => Some("CONSP"),
        "NULL" => Some("NULL"),
        "LISTP" => Some("LISTP"),
        "SYMBOLP" => Some("SYMBOLP"),
        "KEYWORDP" => Some("KEYWORDP"),
        "NUMBERP" => Some("NUMBERP"),
        "INTEGERP" => Some("INTEGERP"),
        "REALP" => Some("REALP"),
        "CHARACTERP" => Some("CHARACTERP"),
        "STRINGP" => Some("STRINGP"),
        "VECTORP" => Some("VECTORP"),
        "ARRAYP" => Some("ARRAYP"),
        "FUNCTIONP" => Some("FUNCTIONP"),
        "HASH-TABLE-P" => Some("HASH-TABLE-P"),
        "PACKAGEP" => Some("PACKAGEP"),
        "STREAMP" => Some("STREAMP"),
        "PATHNAMEP" => Some("PATHNAMEP"),
        "EVENP" => Some("EVENP"),
        "ODDP" => Some("ODDP"),
        "MINUSP" => Some("MINUSP"),
        "PLUSP" => Some("PLUSP"),
        "ZEROP" => Some("ZEROP"),
        _ => None,
    }
}

fn check_enclosing_condition_redundancy(
    ctx: &LintDocument<'_>,
    test: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    let Some(fact) = &env.entry_test else { return };
    let (inner, negated) = unwrap_negated(ctx, test, package, lexicals, env);
    let known_truth = if negated { !fact.truth } else { fact.truth };
    if repeatable_test(ctx, &inner, package, lexicals, env).as_ref() == Some(&fact.test)
        && let Some(diag) = emit.diagnostic(
            "suspicious/redundant-enclosing-condition",
            test.text_range(),
            format!(
                "this test is already known to be {} on entry to this branch",
                known_truth
            ),
            None,
            Vec::new(),
            None,
        )
    {
        diag.note(fact.range, "the enclosing test established this result");
    }
}

fn check_handler_case(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let clauses = args.get(1..).unwrap_or(&[]);
    let mut seen_types: Vec<Element> = Vec::new();
    for clause in clauses {
        let Some(cnode) = clause.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let mut parts = elements(cnode);
        let Some(typespec_el) = parts.next() else {
            continue;
        };
        if let Some(ty_sym) = symbol(&typespec_el)
            && ty_sym.prefix == Prefix::Keyword
            && ty_sym.name == "NO-ERROR"
        {
            continue;
        }

        if let Some(prev_sup) = seen_types
            .iter()
            .find(|prev| is_cl_type_subsumed(&typespec_el, prev, package, ctx.workspace))
        {
            emit.emit(
                "suspicious/shadowed-handler-clause",
                typespec_el.text_range(),
                format!(
                    "`handler-case` clause for `{}` is shadowed by earlier `{}` handler",
                    ctx.text_of(typespec_el.text_range()),
                    ctx.text_of(prev_sup.text_range())
                ),
                Some("order specific condition clauses before general supertypes".into()),
                vec![LintTag::Unnecessary],
                None,
            );
        }
        seen_types.push(typespec_el.clone());

        // Check swallowed condition: `(error (e) nil)` or `(condition (e) nil)` or `(t () nil)`.
        if let Some(ty_sym) = symbol(&typespec_el)
            && let Some(std_ty) = ctx.workspace.standard_symbol(&ty_sym, package)
            && matches!(std_ty, "ERROR" | "SERIOUS-CONDITION" | "CONDITION" | "T")
        {
            let _var_list = parts.next();
            let raw_body: Vec<_> = parts.collect();
            let body = body_after_declarations(ctx, &raw_body, package);
            let returns_nil_only = body.is_empty()
                || (body.len() == 1 && is_nil_element(&body[0], package, ctx.workspace));
            if returns_nil_only {
                emit.emit(
                    "suspicious/swallowed-condition",
                    cnode.text_range(),
                    format!(
                        "`handler-case` catches broad `{}` and silently discards it",
                        std_ty.to_ascii_lowercase()
                    ),
                    Some("handle a specific condition type or log/re-signal the error".into()),
                    Vec::new(),
                    None,
                );
            }
        }
    }
}

fn check_typecase(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let clauses = args.get(1..).unwrap_or(&[]);
    let mut seen_types: Vec<Element> = Vec::new();
    for (idx, clause) in clauses.iter().enumerate() {
        let Some(cnode) = clause.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let Some(typespec_el) = elements(cnode).next() else {
            continue;
        };
        let is_last = idx + 1 == clauses.len();
        if let Some(ty_sym) = symbol(&typespec_el)
            && (ctx.workspace.standard_symbol(&ty_sym, package) == Some("T")
                || (matches!(ty_sym.prefix, Prefix::None) && ty_sym.name == "OTHERWISE"))
            && !is_last
        {
            emit.emit(
                "correctness/case-non-final-default",
                clause.text_range(),
                format!(
                    "`{}` catch-all `{}` clause before the end makes following clauses unreachable",
                    op.to_ascii_lowercase(),
                    ty_sym.name.to_ascii_lowercase()
                ),
                None,
                Vec::new(),
                None,
            );
            continue;
        }
        if let Some(prev_sup) = seen_types
            .iter()
            .find(|prev| is_cl_type_subsumed(&typespec_el, prev, package, ctx.workspace))
        {
            emit.emit(
                "suspicious/shadowed-typecase-clause",
                typespec_el.text_range(),
                format!(
                    "`{}` branch `{}` is shadowed by earlier `{}` branch",
                    op.to_ascii_lowercase(),
                    ctx.text_of(typespec_el.text_range()),
                    ctx.text_of(prev_sup.text_range())
                ),
                Some("place subtype branches before their supertypes".into()),
                vec![LintTag::Unnecessary],
                None,
            );
        }
        seen_types.push(typespec_el);
    }
}

fn check_cond_clauses(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    let mut seen_tests = FxHashMap::default();
    for (idx, clause) in args.iter().enumerate() {
        let Some(cnode) = clause.as_node().filter(|n| closed_list(n)) else {
            seen_tests.clear();
            continue;
        };
        let Some(parts) = ctx.read_elements(elements(cnode)) else {
            seen_tests.clear();
            continue;
        };
        let Some(test_el) = parts.first() else {
            continue;
        };
        if is_t_element(test_el, package, ctx.workspace) && idx + 1 < args.len() {
            emit.emit(
                "correctness/case-non-final-default",
                clause.text_range(),
                "`cond` clause with `t` before the end makes following clauses unreachable",
                None,
                Vec::new(),
                None,
            );
            continue;
        }
        if is_nil_element(test_el, package, ctx.workspace) {
            emit.emit(
                "suspicious/constant-condition",
                test_el.text_range(),
                "`cond` clause condition is always `nil`",
                None,
                vec![LintTag::Unnecessary],
                None,
            );
            continue;
        }
        let Some(key) = repeatable_test(ctx, test_el, package, lexicals, env) else {
            seen_tests.clear();
            continue;
        };
        if let Some(previous) = seen_tests.insert(key, test_el.text_range())
            && let Some(diag) = emit.diagnostic(
                "suspicious/duplicate-cond-test",
                test_el.text_range(),
                format!(
                    "duplicate `cond` test `{}`",
                    ctx.text_of(test_el.text_range())
                ),
                None,
                vec![LintTag::Unnecessary],
                None,
            )
        {
            diag.note(
                previous,
                "if this test succeeded, the earlier clause would have been taken",
            );
        }
    }
}

pub(crate) fn check_sequence_unreachable(
    ctx: &LintDocument<'_>,
    forms: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("suspicious/unreachable-code") {
        return;
    }
    for [previous, next] in forms.windows(2).map(|pair| [&pair[0], &pair[1]]) {
        if never_returns(ctx, previous, package, lexicals, env, &mut 64) {
            if let Some(diag) = emit.diagnostic(
                "suspicious/unreachable-code",
                next.text_range(),
                "unreachable expression after unconditional exit",
                None,
                vec![LintTag::Unnecessary],
                None,
            ) {
                diag.note(previous.text_range(), "control leaves this sequence here");
            }
            break;
        }
    }
}

fn never_returns(
    ctx: &LintDocument<'_>,
    form: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    budget: &mut usize,
) -> bool {
    let Some(left) = budget.checked_sub(1) else {
        return false;
    };
    *budget = left;
    let Some(node) = form.as_node().filter(|node| closed_list(node)) else {
        return false;
    };
    let Some(parts) = ctx.read_elements(elements(node)) else {
        return false;
    };
    let Some((head, args)) = parts.split_first() else {
        return false;
    };
    let Some(name) = symbol(head) else {
        return false;
    };
    let Some(operator) = ctx.std_op(&name, package, lexicals, env) else {
        return false;
    };
    let mut leaves = |form| never_returns(ctx, form, package, lexicals, env, budget);
    match (operator, args) {
        ("RETURN" | "RETURN-FROM" | "GO" | "THROW" | "ERROR" | "ABORT", _) => true,
        ("IF", [test, yes, rest @ ..]) => {
            if leaves(test) {
                return true;
            }
            if is_t_element(test, package, ctx.workspace) {
                return leaves(yes);
            }
            if is_nil_element(test, package, ctx.workspace) {
                return rest.first().is_some_and(leaves);
            }
            leaves(yes) && rest.first().is_some_and(leaves)
        }
        ("PROGN" | "PROG1" | "PROG2" | "MULTIPLE-VALUE-PROG1" | "UNWIND-PROTECT", body) => {
            body.iter().any(leaves)
        }
        ("LOCALLY", body) | ("LET" | "LET*" | "FLET" | "LABELS", [_, body @ ..]) => {
            super::body_after_declarations(ctx, body, package)
                .iter()
                .any(leaves)
        }
        ("MULTIPLE-VALUE-BIND", [_, values_form, body @ ..]) => {
            leaves(values_form)
                || super::body_after_declarations(ctx, body, package)
                    .iter()
                    .any(leaves)
        }
        ("WHEN", [test, body @ ..]) => {
            leaves(test) || (is_t_element(test, package, ctx.workspace) && body.iter().any(leaves))
        }
        ("UNLESS", [test, body @ ..]) => {
            leaves(test)
                || (is_nil_element(test, package, ctx.workspace) && body.iter().any(leaves))
        }
        ("COND", clauses) => {
            for clause in clauses {
                let Some(cnode) = clause.as_node().filter(|node| closed_list(node)) else {
                    return false;
                };
                let Some(parts) = ctx.read_elements(elements(cnode)) else {
                    return false;
                };
                let Some((test, body)) = parts.split_first() else {
                    return false;
                };
                if never_returns(ctx, test, package, lexicals, env, budget) {
                    return true;
                }
                if is_nil_element(test, package, ctx.workspace) {
                    continue;
                }
                let clause_leaves = body
                    .iter()
                    .any(|expr| never_returns(ctx, expr, package, lexicals, env, budget));
                if !clause_leaves {
                    return false;
                }
                if is_t_element(test, package, ctx.workspace) {
                    return true;
                }
            }
            false
        }
        _ => false,
    }
}

pub(crate) fn check_defclass_suspicious(
    ctx: &LintDocument<'_>,
    slots_el: &Element,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(node) = slots_el.as_node().filter(|n| closed_list(n)) else {
        return;
    };
    for slot in elements(node) {
        let Some(slot_node) = slot.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let parts: Vec<_> = elements(slot_node).skip(1).collect();
        for pair in parts.as_chunks::<2>().0 {
            if let Some(kw) = symbol(&pair[0])
                && kw.prefix == Prefix::Keyword
                && kw.name == "INITFORM"
                && is_literal_compound(&pair[1], package, ctx.workspace)
                && !is_string_literal_el(&pair[1])
                && !is_nil_element(&pair[1], package, ctx.workspace)
            {
                emit.emit(
                    "suspicious/defclass-literal-initform",
                    pair[1].text_range(),
                    "`:initform` uses a quoted compound literal shared across all instances",
                    Some("construct a fresh collection with `(list ...)` or `(vector ...)`".into()),
                    Vec::new(),
                    None,
                );
            }
        }
    }
}

pub(crate) fn check_defmethod_around(
    ctx: &LintDocument<'_>,
    list: &SyntaxNode,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let lambda_idx = crate::outline::defmethod_lambda_list_index(args).unwrap_or(args.len());
    let is_around = args[1..lambda_idx].iter().any(|el| {
        symbol(el).is_some_and(|sym| sym.prefix == Prefix::Keyword && sym.name == "AROUND")
    });
    if !is_around {
        return;
    }
    let body_start = (lambda_idx + 1).min(args.len());
    let body = args.get(body_start..).unwrap_or(&[]);
    let calls_next = body
        .iter()
        .any(|el| contains_symbol_named(el, "CALL-NEXT-METHOD", package, ctx.workspace));
    if !calls_next {
        emit.emit(
            "suspicious/around-missing-call-next-method",
            list.text_range(),
            "`:around` method never references `call-next-method`",
            Some("call `(call-next-method)` or convert this into a primary method".into()),
            Vec::new(),
            None,
        );
    }
}

fn contains_symbol_named(
    el: &Element,
    target: &str,
    package: Option<&str>,
    workspace: &crate::navigation::index::WorkspaceIndex<'_>,
) -> bool {
    if let Some(sym) = symbol(el) {
        return workspace.standard_symbol(&sym, package) == Some(target);
    }
    if let Some(node) = el.as_node() {
        return elements(node)
            .any(|child| contains_symbol_named(&child, target, package, workspace));
    }
    false
}

pub(crate) fn check_defstruct_suspicious(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("suspicious/defclass-literal-initform") {
        return;
    }
    for slot in args.iter().skip(1) {
        let Some(slot_node) = slot.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let parts: Vec<_> = elements(slot_node).collect();
        let Some(initform) = parts.get(1) else {
            continue;
        };
        let is_read_only = parts[2..].as_chunks::<2>().0.iter().any(|pair| {
            symbol(&pair[0])
                .is_some_and(|kw| kw.prefix == Prefix::Keyword && kw.name == "READ-ONLY")
                && !is_nil_element(&pair[1], package, ctx.workspace)
        });
        if !is_read_only
            && is_literal_compound(initform, package, ctx.workspace)
            && !is_string_literal_el(initform)
            && !is_nil_element(initform, package, ctx.workspace)
        {
            emit.emit(
                "suspicious/defclass-literal-initform",
                initform.text_range(),
                "`defstruct` slot default uses a quoted compound literal shared across all instances",
                Some("construct a fresh collection with `(list ...)` or `(vector ...)`".into()),
                Vec::new(),
                None,
            );
        }
    }
}
