//! Arity and keyword contracts for direct calls and statically named functions.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::format::TextEdit;
use crate::navigation::index::{CallableSignature, closed_list, lambda_list_signature, symbol};
use crate::navigation::queries::{check_callable_arity, find_local_fn};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SourceReadtable, SyntaxKind, TextRange, TextSize, lexer::lex};

use super::super::context::{
    FormContext, LintDocument, LintEmitter, is_char_literal_el, is_function_quote, is_nil_element,
    is_nil_syntax, is_number_literal_el, is_string_literal_el, unwrap_quote,
};
use super::super::{FixApplicability, LintFix, LintRelated};
use super::{CallSite, ScopeContext};

// ANSI specifies an exclusive upper bound with a minimum value of 50.
const MIN_CALL_ARGUMENTS_LIMIT: usize = 50;

pub(super) fn check_arity(
    signature: &CallableSignature<'_>,
    actual: usize,
    exact: bool,
    range: TextRange,
    emit: &mut LintEmitter<'_>,
) -> bool {
    let Some(message) = check_callable_arity(signature, actual, exact) else {
        return true;
    };
    if let Some(diag) = emit.diagnostic(
        "correctness/arity-mismatch",
        range,
        message,
        None,
        Vec::new(),
        None,
    ) && let Some((document, range)) = signature.definition
    {
        diag.related.push(LintRelated {
            document: document.map(str::to_owned),
            range,
            message: "the callable is defined here".into(),
        });
    }
    false
}

pub(super) fn check_keywords(
    ctx: &LintDocument<'_>,
    sig: &CallableSignature<'_>,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if !sig.evaluates_arguments
        || !sig
            .lambda_list()
            .any(|param| param.eq_ignore_ascii_case("&KEY"))
    {
        return;
    }
    let mut params = sig.lambda_list();
    let mut positional = 0;
    for param in params.by_ref() {
        if param.eq_ignore_ascii_case("&KEY") {
            break;
        }
        if param.eq_ignore_ascii_case("&REST") {
            // The following name receives the same tail processed by &KEY.
            params.next();
            if !params
                .next()
                .is_some_and(|next| next.eq_ignore_ascii_case("&KEY"))
            {
                return;
            }
            break;
        }
        if !param.eq_ignore_ascii_case("&OPTIONAL") {
            positional += 1;
        }
    }
    let Some(tail) = args.get(positional..).filter(|tail| !tail.is_empty()) else {
        return;
    };
    let (pairs, trailing) = tail.as_chunks::<2>();
    if let [last] = trailing {
        emit.emit(
            "correctness/odd-key-value-args",
            last.text_range(),
            format!(
                "`{}` received a keyword without its value",
                sig.name.to_ascii_lowercase()
            ),
            Some("check each name/value pair after the positional arguments".into()),
            Vec::new(),
            None,
        );
        return;
    }
    let mut accepted = FxHashSet::default();
    let mut allow_unknown = false;
    let mut keys_known = true;
    for param in params {
        if param.eq_ignore_ascii_case("&AUX") {
            break;
        }
        if param.eq_ignore_ascii_case("&ALLOW-OTHER-KEYS") {
            allow_unknown = true;
            break;
        }
        let Some(name) = parameter_keyword(param) else {
            keys_known = false;
            continue;
        };
        if name.prefix == Prefix::Keyword {
            accepted.insert(name.name);
        } else {
            // An explicit key name can be inherited or imported in the
            // defining package. The spelling alone does not resolve it.
            keys_known = false;
        }
    }
    let literal_names: Vec<_> = pairs
        .iter()
        .map(|[key, _]| {
            let quoted = unwrap_quote(key, package, ctx.workspace);
            let name = symbol(quoted.as_ref().unwrap_or(key))?;
            (name.prefix == Prefix::Keyword
                || ctx.workspace.symbol_home_package(&name, package) == Some("KEYWORD"))
            .then_some(name.name)
        })
        .collect();
    if !allow_unknown {
        // Only the leftmost :ALLOW-OTHER-KEYS value is consulted. A computed
        // key can itself name that keyword, so leave that case uncertain.
        for (literal, [key, value]) in literal_names.iter().zip(pairs) {
            match literal.as_deref() {
                Some("ALLOW-OTHER-KEYS") => {
                    allow_unknown = !is_nil_element(value, package, ctx.workspace);
                    break;
                }
                None if certainly_non_symbol(ctx, key, package)
                    || unwrap_quote(key, package, ctx.workspace).is_some()
                    || is_nil_syntax(key, package, ctx.workspace) => {}
                None => {
                    allow_unknown = true;
                    break;
                }
                Some(_) => {}
            }
        }
    }
    let mut seen = FxHashMap::default();
    for (literal, [key, _]) in literal_names.iter().zip(pairs) {
        let Some(name) = literal else {
            if !allow_unknown && certainly_non_symbol(ctx, key, package) {
                super::grammar::report(
                    emit,
                    "correctness/non-symbol-keyword-arg",
                    key,
                    "keyword argument names must evaluate to symbols",
                );
            }
            continue;
        };
        match seen.entry(name) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                if let Some(diag) = emit.diagnostic("suspicious/duplicate-keyword-arg", key.text_range(),
                    format!("repeated keyword `{}` uses the value from its first occurrence", ctx.text_of(key.text_range())),
                    Some("each argument expression is still evaluated; preserve any intended effects when editing the call".into()), Vec::new(), None) {
                    diag.note(*entry.get(), "this occurrence supplies the parameter value");
                }
                continue;
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(key.text_range());
            }
        }
        if keys_known && !allow_unknown && name != "ALLOW-OTHER-KEYS" && !accepted.contains(name) {
            let mut choices = accepted
                .iter()
                .map(|name| display_keyword(name))
                .collect::<Vec<_>>();
            choices.sort_unstable();
            let fix = symbol(key)
                .filter(|name| name.prefix == Prefix::Keyword)
                .and_then(|_| {
                    let spelling =
                        super::names::unique_nearby(name, accepted.iter().map(String::as_str))?;
                    if literal_names
                        .iter()
                        .flatten()
                        .any(|existing| existing == spelling)
                    {
                        return None;
                    }
                    let replacement = display_keyword(spelling);
                    Some(LintFix {
                        title: format!("Replace with `{replacement}`"),
                        applicability: FixApplicability::MaybeIncorrect,
                        edits: vec![TextEdit {
                            range: key.text_range(),
                            replacement,
                        }],
                    })
                });
            if let Some(diag) = emit.diagnostic(
                "correctness/unknown-keyword-arg",
                key.text_range(),
                format!(
                    "unknown keyword `{}` for `{}`",
                    ctx.text_of(key.text_range()),
                    sig.name.to_ascii_lowercase()
                ),
                (!choices.is_empty())
                    .then(|| format!("accepted keywords are {}", choices.join(", "))),
                Vec::new(),
                fix,
            ) && let Some((document, range)) = sig.definition
            {
                diag.related.push(LintRelated {
                    document: document.map(str::to_owned),
                    range,
                    message: "this lambda list declares the accepted keywords".into(),
                });
            }
        }
    }
}

fn parameter_keyword(param: &str) -> Option<SymbolName> {
    if !param.starts_with('(') {
        if !param.is_empty()
            && param.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'-' | b'_' | b'+' | b'*' | b'/' | b'<' | b'>' | b'=' | b'?' | b'!' | b'.'
                    )
            })
        {
            return Some(SymbolName {
                prefix: Prefix::Keyword,
                name: param.to_ascii_uppercase(),
            });
        }
    } else if !param.starts_with("((") {
        let rest = param[1..].trim_start_matches(|c: char| c.is_ascii_whitespace());
        let end = rest
            .find(|c: char| c.is_ascii_whitespace() || c == ')')
            .unwrap_or(rest.len());
        let token = &rest[..end];
        if !token.is_empty()
            && token.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'-' | b'_' | b'+' | b'*' | b'/' | b'<' | b'>' | b'=' | b'?' | b'!' | b'.'
                    )
            })
        {
            return Some(SymbolName {
                prefix: Prefix::Keyword,
                name: token.to_ascii_uppercase(),
            });
        }
    }
    let mut tokens = lex(param, SourceReadtable::Standard).filter(|token| !token.kind.is_trivia());
    let first = tokens.next()?;
    let (name, explicit) = if first.kind == SyntaxKind::OpenParen {
        let second = tokens.next()?;
        if second.kind == SyntaxKind::OpenParen {
            (tokens.next()?, true)
        } else {
            (second, false)
        }
    } else {
        (first, false)
    };
    if name.kind != SyntaxKind::Atom || name.diagnostic.is_some() {
        return None;
    }
    let mut name = SymbolName::parse(name.text)?;
    if !explicit {
        name.prefix = Prefix::Keyword;
    }
    Some(name)
}

fn display_keyword(name: &str) -> String {
    if !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return format!(":{}", name.to_ascii_lowercase());
    }
    let escaped = name.replace('\\', "\\\\").replace('|', "\\|");
    format!(":|{escaped}|")
}

fn certainly_non_symbol(ctx: &LintDocument<'_>, el: &Element, package: Option<&str>) -> bool {
    let literal = unwrap_quote(el, package, ctx.workspace);
    let value = literal.as_ref().unwrap_or(el);
    is_number_literal_el(value)
        || is_string_literal_el(value)
        || is_char_literal_el(value)
        || value.kind() == SyntaxKind::Vector
        || (literal.is_some()
            && value.kind() == SyntaxKind::List
            && !is_nil_syntax(value, package, ctx.workspace))
}

pub(super) fn check_indirect_call(
    scope: ScopeContext<'_, '_>,
    op: &str,
    call: CallSite<'_>,
    emit: &mut LintEmitter<'_>,
) {
    let ScopeContext {
        doc: ctx, package, ..
    } = scope;
    let Some((target, actuals)) = call.args.split_first() else {
        return;
    };
    let range = call.list.text_range();
    if op == "FUNCALL" {
        check_indirect_target(
            scope,
            target,
            actuals.len(),
            true,
            Some(actuals),
            range,
            emit,
        );
        return;
    }

    let Some((tail, fixed)) = actuals.split_last() else {
        return;
    };
    if let Some(tail_list) = tail.as_node()
        && scope.known_package(range)
        && let Some((constructor, suffix)) = scope.standard_call(tail_list)
        && constructor == "LIST"
    {
        let actual = fixed.len() + suffix.len();
        if actual + 1 < MIN_CALL_ARGUMENTS_LIMIT {
            if emit.is_enabled("perf/apply-fresh-list") {
                emit.emit(
                    "perf/apply-fresh-list",
                    tail.text_range(),
                    "`apply` immediately unpacks a freshly constructed argument list",
                    Some("pass the arguments directly to `funcall` in the same order".into()),
                    Vec::new(),
                    apply_list_fix(scope, call, tail, &suffix),
                );
            }
        } else if actual >= MIN_CALL_ARGUMENTS_LIMIT {
            check_apply_limit(scope, target, range, emit);
        }
        let mut arguments = fixed.to_vec();
        arguments.extend(suffix);
        check_indirect_target(scope, target, actual, true, Some(&arguments), range, emit);
        return;
    }
    let (actual, exact) = if is_nil_element(tail, package, ctx.workspace) {
        (fixed.len(), true)
    } else if let Some(quoted) = unwrap_quote(tail, package, ctx.workspace)
        && let Some(list) = quoted.as_node().filter(|node| closed_list(node))
        && let Some(forms) = ctx.read_elements(elements(list))
    {
        (fixed.len() + forms.len(), true)
    } else {
        if is_number_literal_el(tail)
            || is_string_literal_el(tail)
            || is_char_literal_el(tail)
            || tail.kind() == SyntaxKind::Vector
            || symbol(tail).is_some_and(|name| name.prefix == Prefix::Keyword)
        {
            super::grammar::report(
                emit,
                "correctness/invalid-apply-tail",
                tail,
                "the last argument to `apply` must be a proper list",
            );
            return;
        }
        (fixed.len(), false)
    };
    if !exact || actual >= MIN_CALL_ARGUMENTS_LIMIT {
        check_apply_limit(scope, target, range, emit);
    }
    check_indirect_target(scope, target, actual, exact, None, range, emit);
}

fn check_apply_limit(
    scope: ScopeContext<'_, '_>,
    target: &Element,
    range: TextRange,
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("perf/apply-call-arguments-limit") {
        return;
    }
    let Some(name) = is_function_quote(target, scope.package, scope.doc.workspace)
        .and_then(|element| symbol(&element))
    else {
        return;
    };
    let Some(operator) = scope
        .doc
        .std_op(&name, scope.package, scope.lexicals, scope.env)
    else {
        return;
    };
    let suggestion = match operator {
        "APPEND" => "use `loop for group in groups append group` to concatenate in one pass",
        "NCONC" => {
            "use `loop for group in groups nconc group` if destructively joining the groups is intended"
        }
        "CONCATENATE" => {
            "use `with-output-to-string` or `apply`/`reduce` carefully on bounded inputs"
        }
        "+" | "*" | "MAX" | "MIN" => "reduce the sequence directly to avoid `call-arguments-limit`",
        _ => return,
    };
    emit.emit(
        "perf/apply-call-arguments-limit",
        range,
        format!(
            "`apply` can pass too many arguments to `{}` for `call-arguments-limit`",
            operator.to_ascii_lowercase()
        ),
        Some(suggestion.into()),
        Vec::new(),
        None,
    );
}

fn apply_list_fix(
    scope: ScopeContext<'_, '_>,
    call: CallSite<'_>,
    tail: &Element,
    suffix: &[Element],
) -> Option<LintFix> {
    let operator = scope.standard_spelling("funcall")?;
    let head = elements(call.list).next()?;
    let list_head = elements(tail.as_node()?).next()?;
    let tail_range = tail.text_range();
    let closing = tail_range.end() - TextSize::from(1);
    let previous = call
        .args
        .get(call.args.len().checked_sub(2)?)?
        .text_range()
        .end();
    let gap = scope
        .doc
        .text_of(TextRange::new(previous, tail_range.start()));
    let contents = scope
        .doc
        .text_of(TextRange::new(list_head.text_range().end(), closing));
    let mut edits = vec![TextEdit {
        range: head.text_range(),
        replacement: operator.into_owned(),
    }];
    if suffix.is_empty() && gap.trim_ascii().is_empty() && contents.trim_ascii().is_empty() {
        edits.push(TextEdit {
            range: TextRange::new(previous, tail_range.end()),
            replacement: String::new(),
        });
    } else {
        let end = suffix
            .first()
            .map_or(list_head.text_range().end(), |first| {
                let first_start = first.text_range().start();
                let leading = scope
                    .doc
                    .text_of(TextRange::new(list_head.text_range().end(), first_start));
                if !gap.is_empty() && leading.trim_ascii().is_empty() {
                    first_start
                } else {
                    list_head.text_range().end()
                }
            });
        edits.extend([
            TextEdit {
                range: TextRange::new(tail_range.start(), end),
                replacement: String::new(),
            },
            TextEdit {
                range: TextRange::new(closing, tail_range.end()),
                replacement: String::new(),
            },
        ]);
    }
    Some(LintFix {
        title: "Pass the arguments directly with `funcall`".into(),
        applicability: FixApplicability::MachineApplicable,
        edits,
    })
}

pub(super) fn check_indirect_target(
    scope: ScopeContext<'_, '_>,
    target: &Element,
    actual: usize,
    exact: bool,
    written_arguments: Option<&[Element]>,
    range: TextRange,
    emit: &mut LintEmitter<'_>,
) {
    let ScopeContext {
        doc: ctx,
        package,
        lexicals,
        env,
    } = scope;
    let function = is_function_quote(target, package, ctx.workspace);
    let quoted = unwrap_quote(target, package, ctx.workspace);
    let value = function.as_ref().or(quoted.as_ref()).unwrap_or(target);
    if quoted.is_none() && super::is_lambda_expression(ctx, value) {
        check_lambda_arity(ctx, value, actual, exact, range, emit);
        return;
    }
    if let Some(name) = symbol(value).filter(|_| function.is_some() || quoted.is_some()) {
        let local = function
            .is_some()
            .then(|| find_local_fn(lexicals, &name, package, ctx.workspace))
            .flatten();
        if local.is_none() && function.is_some() && env.is_fn_shadowed(&name) {
            return;
        }
        let spec = ctx.workspace.resolve_callable(&name, package, local);
        if let Some(signature) = spec
            .as_ref()
            .filter(|signature| signature.evaluates_arguments)
        {
            if check_arity(signature, actual, exact, range, emit)
                && let Some(args) = written_arguments
            {
                check_keywords(ctx, signature, args, package, emit);
            }
        } else if quoted.is_some() {
            super::correctness::check_function_of_macro(
                ctx,
                value,
                package,
                &[],
                &FormContext::default(),
                emit,
            );
        }
        return;
    }
    if function.is_none() && certainly_non_symbol(ctx, target, package) {
        super::grammar::report(
            emit,
            "correctness/invalid-function-designator",
            target,
            "this value cannot designate a function; use a function object or a function name symbol",
        );
    }
}

pub(super) fn check_callback(
    scope: ScopeContext<'_, '_>,
    op: &str,
    args: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    let (position, actual) = match op {
        "MAPCAR" | "MAPC" | "MAPCAN" | "MAPLIST" | "MAPL" | "MAPCON" | "EVERY" | "SOME"
        | "NOTANY" | "NOTEVERY"
            if args.len() >= 2 =>
        {
            (0, args.len() - 1)
        }
        "MAP" | "MAP-INTO" if args.len() >= 2 => (1, args.len() - 2),
        "MAPHASH" => (0, 2),
        "SORT" | "STABLE-SORT" => (1, 2),
        "MERGE" => (3, 2),
        "REMOVE-IF" | "DELETE-IF" | "FIND-IF" | "POSITION-IF" | "COUNT-IF" | "MEMBER-IF"
        | "REMOVE-IF-NOT" | "DELETE-IF-NOT" | "FIND-IF-NOT" | "POSITION-IF-NOT"
        | "COUNT-IF-NOT" | "MEMBER-IF-NOT" => (0, 1),
        "SUBSTITUTE-IF" | "NSUBSTITUTE-IF" | "SUBSTITUTE-IF-NOT" | "NSUBSTITUTE-IF-NOT" => (1, 1),
        _ => return,
    };
    if let Some(callback) = args.get(position) {
        check_indirect_target(
            scope,
            callback,
            actual,
            true,
            None,
            callback.text_range(),
            emit,
        );
    }
}

pub(super) fn check_lambda_arity(
    ctx: &LintDocument<'_>,
    lambda: &Element,
    actual: usize,
    exact: bool,
    range: TextRange,
    emit: &mut LintEmitter<'_>,
) {
    if let Some(node) = lambda.as_node()
        && let Some(parts) = ctx.read_elements(elements(node))
        && let Some(params) = parts.get(1).and_then(lambda_list_signature)
    {
        check_arity(
            &CallableSignature::anonymous(&params),
            actual,
            exact,
            range,
            emit,
        );
    }
}
