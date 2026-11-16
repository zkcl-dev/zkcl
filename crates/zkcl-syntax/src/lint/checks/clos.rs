//! Class, generic function, and method syntax and their evaluated positions.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::SymbolName;
use crate::syntax::{SyntaxKind, TextRange, list_has_dot};

use super::super::context::{FormContext, LintDocument, LintEmitter, is_nil_syntax};
use super::grammar::{LambdaListKind, has_unknown_reader_value};
use super::{BodyKind, Walk, WalkContext, correctness, grammar, push_definition_block};

const CLASS_OPTION: &str = "correctness/invalid-class-option";
const DUPLICATE_CLASS_OPTION: &str = "correctness/duplicate-class-option";
const GENERIC_OPTION: &str = "correctness/invalid-defgeneric-option";

pub(super) fn check(
    ctx: &LintDocument<'_>,
    op: &str,
    args: &[Element],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
    work: &mut Walk,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    let package = args
        .first()
        .and_then(|name| ctx.source.package_at(name.text_range().start()));
    let init_env = env.nested_expr(false, false);
    let options = args.get(3..).unwrap_or_default();
    let condition = op == "DEFINE-CONDITION";
    let standard_allocation = condition
        || options.iter().all(|option| {
            let Some(node) = option
                .as_node()
                .filter(|node| node.kind() == SyntaxKind::List)
            else {
                return !has_unknown_reader_value(option);
            };
            let Some(parts) = ctx.read_elements(elements(node)) else {
                return false;
            };
            match parts.as_slice() {
                [key, values @ ..]
                    if keyword(ctx, key, package).is_some_and(|k| k.name == "METACLASS") =>
                {
                    matches!(values, [value] if symbol(value).is_some_and(|name| {
                        ctx.workspace.standard_symbol(&name, package) == Some("STANDARD-CLASS")
                    }))
                }
                _ => true,
            }
        });
    if let Some(name) = args.first() {
        check_class_value(
            name,
            non_nil_symbol(ctx, name, package),
            "a class name must be a non-nil symbol",
            emit,
        );
    }
    if let Some(superclasses) = args.get(1)
        && let Some(names) = grammar::proper_list(
            ctx,
            superclasses,
            package,
            CLASS_OPTION,
            "expected a list of superclass names",
            emit,
        )
    {
        for class in names {
            check_class_value(
                &class,
                non_nil_symbol(ctx, &class, package),
                "a superclass name must be a non-nil symbol",
                emit,
            );
        }
    }
    if let Some(slots) = args.get(2)
        && let Some(slots) = grammar::proper_list(
            ctx,
            slots,
            package,
            CLASS_OPTION,
            "expected a list of slot definitions",
            emit,
        )
    {
        for slot in slots {
            if cancelled() {
                return None;
            }
            if symbol(&slot).is_some() {
                check_slot_name(ctx, package, &slot, emit);
                continue;
            }
            let Some(node) = slot.as_node().filter(|node| closed_list(node)) else {
                if !has_unknown_reader_value(&slot) {
                    grammar::report(
                        emit,
                        CLASS_OPTION,
                        &slot,
                        "expected a slot name or a proper slot definition",
                    );
                }
                continue;
            };
            let Some(parts) = ctx.read_elements(elements(node)) else {
                continue;
            };
            let Some((name, options)) = parts.split_first() else {
                grammar::report(
                    emit,
                    CLASS_OPTION,
                    &slot,
                    "the slot definition needs a name",
                );
                continue;
            };
            check_slot_name(ctx, package, name, emit);
            let mut seen = FxHashMap::default();
            let (pairs, rest) = options.as_chunks::<2>();
            if let [trailing] = rest {
                grammar::report(
                    emit,
                    CLASS_OPTION,
                    trailing,
                    "this slot option is missing its value",
                );
            }
            for [key, value] in pairs {
                let Some(tag) = keyword(ctx, key, package) else {
                    if condition || !is_symbol(ctx, key, package) {
                        check_class_value(key, false, "expected a slot option name", emit);
                    }
                    continue;
                };
                let name = tag.name.as_str();
                if matches!(name, "ALLOCATION" | "INITFORM" | "TYPE" | "DOCUMENTATION") {
                    duplicate(
                        &mut seen,
                        name.to_owned(),
                        key.text_range(),
                        DUPLICATE_CLASS_OPTION,
                        "option",
                        emit,
                    );
                }
                let (valid, message) = match name {
                    "INITFORM" => {
                        work.enqueue(value, &init_env);
                        continue;
                    }
                    "TYPE" => continue,
                    "READER" | "ACCESSOR" => (
                        non_nil_symbol(ctx, value, package),
                        "expected a non-nil reader function name",
                    ),
                    "WRITER" => (
                        non_nil_symbol(ctx, value, package)
                            || ctx.setf_name(value, package).is_some_and(|name| {
                                ctx.workspace.standard_symbol(&name, package) != Some("NIL")
                            }),
                        "expected a writer function name",
                    ),
                    "INITARG" => (
                        is_symbol(ctx, value, package),
                        "an initarg name must be a symbol",
                    ),
                    "DOCUMENTATION" => (is_string(value), "slot documentation must be a string"),
                    "ALLOCATION" if standard_allocation => (
                        keyword(ctx, value, package)
                            .is_some_and(|name| matches!(name.name.as_str(), "INSTANCE" | "CLASS")),
                        "standard slot allocation is `:instance` or `:class`",
                    ),
                    "ALLOCATION" => (
                        non_nil_symbol(ctx, value, package),
                        "slot allocation must be a non-nil symbol",
                    ),
                    _ => {
                        if condition {
                            grammar::report(
                                emit,
                                CLASS_OPTION,
                                key,
                                "unknown condition slot option",
                            );
                        }
                        continue;
                    }
                };
                check_class_value(value, valid, message, emit);
            }
        }
    }
    let mut seen = FxHashMap::default();
    for option in options {
        if cancelled() {
            return None;
        }
        let Some(node) = option
            .as_node()
            .filter(|node| node.kind() == SyntaxKind::List)
        else {
            if !has_unknown_reader_value(option) {
                grammar::report(emit, CLASS_OPTION, option, "expected a class option list");
            }
            continue;
        };
        let Some(parts) = ctx.read_elements(elements(node)) else {
            continue;
        };
        let Some((key, values)) = parts.split_first() else {
            grammar::report(
                emit,
                CLASS_OPTION,
                option,
                "the class option list needs an option name",
            );
            continue;
        };
        if !is_symbol(ctx, key, package) {
            check_class_value(key, false, "a class option name must be a symbol", emit);
            continue;
        }
        if let Some(identity) = ctx.control_block(key, package) {
            duplicate(
                &mut seen,
                identity,
                key.text_range(),
                DUPLICATE_CLASS_OPTION,
                "option",
                emit,
            );
        }
        let name = keyword(ctx, key, package);
        let name = name.as_ref().map(|name| name.name.as_str());
        match name {
            Some("DEFAULT-INITARGS" | "DOCUMENTATION") => {}
            Some("METACLASS") if !condition => {}
            Some("REPORT") if condition => {}
            _ => {
                // AMOP passes an extension option's cdr through as data, including a dotted tail.
                if condition {
                    grammar::report(emit, CLASS_OPTION, key, "unknown condition class option");
                }
                continue;
            }
        }
        if list_has_dot(node) {
            if values.last().is_some_and(|tail| {
                tail.kind() != SyntaxKind::List
                    && !is_nil_syntax(tail, package, ctx.workspace)
                    && !has_unknown_reader_value(tail)
            }) {
                grammar::report(
                    emit,
                    CLASS_OPTION,
                    option,
                    "expected a proper list for this class option",
                );
            }
            continue;
        }
        if name == Some("DEFAULT-INITARGS") {
            let mut defaulted = FxHashMap::default();
            let (pairs, rest) = values.as_chunks::<2>();
            if let [trailing] = rest {
                grammar::report(
                    emit,
                    CLASS_OPTION,
                    trailing,
                    "`:default-initargs` needs a value for this initarg",
                );
            }
            for [key, value] in pairs {
                if is_symbol(ctx, key, package) {
                    if let Some(identity) = ctx.control_block(key, package) {
                        duplicate(
                            &mut defaulted,
                            identity,
                            key.text_range(),
                            DUPLICATE_CLASS_OPTION,
                            "default initarg",
                            emit,
                        );
                    }
                } else {
                    check_class_value(key, false, "a default initarg name must be a symbol", emit);
                }
                work.enqueue(value, &init_env);
            }
        } else if let [value] = values {
            let (valid, message) = match name {
                Some("DOCUMENTATION") => (is_string(value), "class documentation must be a string"),
                Some("METACLASS") => (
                    non_nil_symbol(ctx, value, package),
                    "a metaclass name must be a non-nil symbol",
                ),
                _ if super::is_lambda_expression(ctx, value) => {
                    work.enqueue(value, &init_env);
                    continue;
                }
                _ => (
                    is_string(value) || is_symbol(ctx, value, package),
                    "expected a report string, function name, or lambda expression",
                ),
            };
            check_class_value(value, valid, message, emit);
        } else {
            grammar::report(
                emit,
                CLASS_OPTION,
                option,
                "this class option takes exactly one value",
            );
        }
    }
    Some(())
}

pub(super) fn check_generic<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    name: &Element,
    params: &Element,
    options: &[Element],
    env: &FormContext,
) -> Option<()> {
    let ctx = walk.doc;
    let package = walk.package;
    let lambda = grammar::analyze_lambda_list(
        ctx,
        params,
        LambdaListKind::Generic,
        walk.emit,
        walk.cancelled,
    )?;
    let mut seen = FxHashMap::default();
    for option in options {
        if walk.is_cancelled() {
            return None;
        }
        if has_unknown_reader_value(option) {
            continue;
        }
        let Some(parts) = grammar::proper_list(
            ctx,
            option,
            package,
            GENERIC_OPTION,
            "expected a generic function option list",
            walk.emit,
        ) else {
            continue;
        };
        let Some((key, values)) = parts.split_first() else {
            grammar::report(
                walk.emit,
                GENERIC_OPTION,
                option,
                "the option list needs an option name",
            );
            continue;
        };
        if symbol(key)
            .is_some_and(|name| ctx.workspace.standard_symbol(&name, package) == Some("DECLARE"))
        {
            check_generic_declarations(ctx, package, option, values, walk.emit);
            continue;
        }
        let Some(tag) = keyword(ctx, key, package) else {
            if !has_unknown_reader_value(key) {
                grammar::report(
                    walk.emit,
                    GENERIC_OPTION,
                    key,
                    "expected a keyword option or `declare`",
                );
            }
            continue;
        };
        if tag.name == "METHOD" {
            walk_method(walk, name, values, option.text_range(), env)?;
            continue;
        }
        duplicate(
            &mut seen,
            tag.name.clone(),
            key.text_range(),
            GENERIC_OPTION,
            "option",
            walk.emit,
        );
        match (tag.name.as_str(), values) {
            ("ARGUMENT-PRECEDENCE-ORDER", _) => check_precedence(
                ctx,
                package,
                option,
                values,
                lambda.required.as_deref(),
                walk.emit,
            ),
            ("DOCUMENTATION", [value]) if is_string(value) || has_unknown_reader_value(value) => {}
            ("GENERIC-FUNCTION-CLASS" | "METHOD-CLASS", [value])
                if non_nil_symbol(ctx, value, package) || has_unknown_reader_value(value) => {}
            ("METHOD-COMBINATION", [name, args @ ..])
                if non_nil_symbol(ctx, name, package) || has_unknown_reader_value(name) =>
            {
                if !args.is_empty()
                    && symbol(name).is_some_and(|name| {
                        ctx.workspace.standard_symbol(&name, package) == Some("STANDARD")
                    })
                {
                    grammar::report(
                        walk.emit,
                        GENERIC_OPTION,
                        &args[0],
                        "the standard method combination takes no arguments",
                    );
                }
            }
            ("DOCUMENTATION", _) => grammar::report(
                walk.emit,
                GENERIC_OPTION,
                option,
                "`:documentation` takes exactly one string",
            ),
            ("GENERIC-FUNCTION-CLASS" | "METHOD-CLASS", _) => grammar::report(
                walk.emit,
                GENERIC_OPTION,
                option,
                "this class option takes one non-nil symbol",
            ),
            ("METHOD-COMBINATION", _) => grammar::report(
                walk.emit,
                GENERIC_OPTION,
                option,
                "`:method-combination` needs a method combination name",
            ),
            _ => grammar::report(
                walk.emit,
                GENERIC_OPTION,
                key,
                "unknown generic function option",
            ),
        }
    }
    Some(())
}

fn check_generic_declarations(
    ctx: &LintDocument<'_>,
    package: Option<&str>,
    option: &Element,
    values: &[Element],
    emit: &mut LintEmitter<'_>,
) {
    if values.is_empty() {
        grammar::report(
            emit,
            GENERIC_OPTION,
            option,
            "this `declare` needs an `optimize` declaration specifier",
        );
    }
    for value in values {
        if has_unknown_reader_value(value) {
            continue;
        }
        let Some(parts) = grammar::proper_list(
            ctx,
            value,
            package,
            GENERIC_OPTION,
            "expected an `optimize` declaration specifier",
            emit,
        ) else {
            continue;
        };
        if parts
            .first()
            .and_then(symbol)
            .is_some_and(|name| ctx.workspace.standard_symbol(&name, package) == Some("OPTIMIZE"))
        {
            correctness::check_declaration_specs(ctx, std::slice::from_ref(value), package, emit);
        } else {
            grammar::report(
                emit,
                GENERIC_OPTION,
                value,
                "`defgeneric` accepts `optimize` declaration specifiers",
            );
        }
    }
}

fn check_precedence(
    ctx: &LintDocument<'_>,
    package: Option<&str>,
    option: &Element,
    order: &[Element],
    required: Option<&[Element]>,
    emit: &mut LintEmitter<'_>,
) {
    if order.is_empty() {
        grammar::report(
            emit,
            GENERIC_OPTION,
            option,
            "`:argument-precedence-order` needs at least one parameter name",
        );
        return;
    }
    let mut valid = true;
    for item in order {
        if !is_symbol(ctx, item, package) && !has_unknown_reader_value(item) {
            grammar::report(
                emit,
                GENERIC_OPTION,
                item,
                "expected a required parameter name",
            );
            valid = false;
        }
    }
    let Some(required) = required.filter(|_| valid) else {
        return;
    };
    let Some(mut remaining) = required
        .iter()
        .map(|item| ctx.control_name(symbol(item)?, package))
        .collect::<Option<FxHashSet<_>>>()
    else {
        return;
    };
    if remaining.len() != required.len() {
        return;
    }
    let Some(order) = order
        .iter()
        .map(|item| Some((item, ctx.control_block(item, package)?)))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    for (item, identity) in order {
        if !remaining.remove(&identity) {
            grammar::report(
                emit,
                GENERIC_OPTION,
                item,
                "expected a required parameter that has not already been listed",
            );
            valid = false;
        }
    }
    if valid && !remaining.is_empty() {
        grammar::report(
            emit,
            GENERIC_OPTION,
            option,
            "the precedence order must list every required parameter",
        );
    }
}

pub(super) fn walk_method<C: Fn() -> bool>(
    walk: &mut WalkContext<'_, '_, '_, C>,
    name: &Element,
    fields: &[Element],
    range: TextRange,
    env: &FormContext,
) -> Option<()> {
    let Some(index) = fields.iter().position(|item| {
        matches!(
            item.kind(),
            SyntaxKind::List
                | SyntaxKind::ReaderForm
                | SyntaxKind::UnknownDispatch
                | SyntaxKind::Error
        ) || is_nil_syntax(item, walk.package, walk.doc.workspace)
    }) else {
        walk.emit.emit(
            "correctness/invalid-lambda-list",
            range,
            "expected a specialized lambda list",
            None,
            Vec::new(),
            None,
        );
        return Some(());
    };
    let params = &fields[index];
    if params.kind() == SyntaxKind::ReaderForm || has_unknown_reader_value(params) {
        return Some(());
    }
    let lambda = grammar::analyze_lambda_list(
        walk.doc,
        params,
        LambdaListKind::Specialized,
        walk.emit,
        walk.cancelled,
    )?;
    let definition_env = env.nested_expr(false, false);
    for form in lambda.eql_specializers {
        walk.enqueue(&form, &definition_env);
    }
    let mut body_env = env.function_body();
    body_env.in_defmacro = false;
    for form in lambda.initforms {
        walk.enqueue(&form, &body_env);
    }
    super::style_pedantic::check_lambda_list_style(params, walk.emit);
    push_definition_block(walk.doc, name, walk.package, &mut body_env);
    walk.walk_body(&fields[index + 1..], &body_env, BodyKind::Documented)
}

fn keyword(ctx: &LintDocument<'_>, el: &Element, package: Option<&str>) -> Option<SymbolName> {
    symbol(el).filter(|name| ctx.workspace.symbol_home_package(name, package) == Some("KEYWORD"))
}

fn is_symbol(ctx: &LintDocument<'_>, value: &Element, package: Option<&str>) -> bool {
    symbol(value).is_some() || is_nil_syntax(value, package, ctx.workspace)
}

fn non_nil_symbol(ctx: &LintDocument<'_>, value: &Element, package: Option<&str>) -> bool {
    symbol(value).is_some_and(|name| ctx.workspace.standard_symbol(&name, package) != Some("NIL"))
}

fn is_string(value: &Element) -> bool {
    matches!(value.kind(), SyntaxKind::String | SyntaxKind::RawString)
}

fn check_slot_name(
    ctx: &LintDocument<'_>,
    package: Option<&str>,
    name: &Element,
    emit: &mut LintEmitter<'_>,
) {
    let valid = symbol(name).is_some_and(|name| {
        !matches!(
            ctx.workspace.standard_symbol(&name, package),
            Some("NIL" | "T")
        ) && ctx.workspace.symbol_home_package(&name, package) != Some("KEYWORD")
    });
    check_class_value(
        name,
        valid,
        "a slot name must be usable as a variable name",
        emit,
    );
}

fn check_class_value(value: &Element, valid: bool, message: &str, emit: &mut LintEmitter<'_>) {
    if !valid && !has_unknown_reader_value(value) {
        grammar::report(emit, CLASS_OPTION, value, message);
    }
}

fn duplicate<Key: std::hash::Hash + Eq>(
    seen: &mut FxHashMap<Key, TextRange>,
    key: Key,
    range: TextRange,
    rule: &'static str,
    kind: &str,
    emit: &mut LintEmitter<'_>,
) {
    match seen.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(range);
        }
        std::collections::hash_map::Entry::Occupied(entry) => {
            if let Some(diag) = emit.diagnostic(
                rule,
                range,
                format!("this {kind} can appear only once"),
                None,
                Vec::new(),
                None,
            ) {
                diag.note(*entry.get(), format!("first {kind} is here"));
            }
        }
    }
}
