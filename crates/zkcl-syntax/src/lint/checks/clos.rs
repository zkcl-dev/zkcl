//! Standard class/condition option contracts and their evaluated positions.

use rustc_hash::FxHashMap;

use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::Prefix;
use crate::syntax::{SyntaxKind, TextRange};

use super::super::context::{FormContext, LintDocument, LintEmitter};
use super::{Walk, grammar};

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
    let standard_allocation = op == "DEFCLASS"
        && !options.iter().any(|option| {
            option
                .as_node()
                .and_then(|node| elements(node).next())
                .as_ref()
                .and_then(keyword)
                .is_some_and(|name| name.name == "METACLASS")
        });
    if let Some(superclasses) = args.get(1)
        && let Some(names) = grammar::proper_list(
            ctx,
            superclasses,
            package,
            "correctness/invalid-class-option",
            "expected a list of superclass names",
            emit,
        )
    {
        for class in names {
            if symbol(&class).is_none() && class.kind() != SyntaxKind::ReaderForm {
                grammar::report(
                    emit,
                    "correctness/invalid-class-option",
                    &class,
                    "a superclass name must be a symbol",
                );
            }
        }
    }
    if let Some(slots) = args.get(2)
        && let Some(slots) = grammar::proper_list(
            ctx,
            slots,
            package,
            "correctness/invalid-class-option",
            "expected a list of slot definitions",
            emit,
        )
    {
        for slot in slots {
            if cancelled() {
                return None;
            }
            if symbol(&slot).is_some() {
                continue;
            }
            let Some(node) = slot.as_node().filter(|node| closed_list(node)) else {
                continue;
            };
            let Some(parts) = ctx.read_elements(elements(node)) else {
                continue;
            };
            let Some((name, options)) = parts.split_first() else {
                grammar::report(
                    emit,
                    "correctness/invalid-class-option",
                    &slot,
                    "the slot definition needs a name",
                );
                continue;
            };
            if symbol(name).is_none() && name.kind() != SyntaxKind::ReaderForm {
                grammar::report(
                    emit,
                    "correctness/invalid-class-option",
                    name,
                    "a slot name must be a symbol",
                );
            }
            let mut seen = FxHashMap::default();
            let (pairs, rest) = options.as_chunks::<2>();
            if let [trailing] = rest {
                grammar::report(
                    emit,
                    "correctness/invalid-class-option",
                    trailing,
                    "this slot option is missing its value",
                );
            }
            for [key, value] in pairs {
                let Some(keyword) = keyword(key) else {
                    continue;
                };
                let name = keyword.name.as_str();
                if matches!(name, "ALLOCATION" | "INITFORM" | "TYPE" | "DOCUMENTATION") {
                    record(&mut seen, name, key.text_range(), emit);
                }
                if name == "INITFORM" {
                    work.enqueue(value, &init_env);
                }
                if name == "ALLOCATION"
                    && standard_allocation
                    && symbol(value).is_some_and(|name| {
                        name.prefix == Prefix::Keyword
                            && !matches!(name.name.as_str(), "INSTANCE" | "CLASS")
                    })
                {
                    grammar::report(
                        emit,
                        "correctness/invalid-class-option",
                        value,
                        "standard slot allocation is `:instance` or `:class`",
                    );
                }
            }
        }
    }
    let mut seen = FxHashMap::default();
    for option in options {
        if cancelled() {
            return None;
        }
        let Some(node) = option.as_node().filter(|node| closed_list(node)) else {
            continue;
        };
        let Some(parts) = ctx.read_elements(elements(node)) else {
            continue;
        };
        let Some((key, values)) = parts.split_first() else {
            continue;
        };
        let Some(keyword) = keyword(key) else {
            continue;
        };
        let name = keyword.name.as_str();
        if name == "DEFAULT-INITARGS" {
            record(&mut seen, name, key.text_range(), emit);
            let mut defaulted = FxHashMap::default();
            let (pairs, rest) = values.as_chunks::<2>();
            if let [trailing] = rest {
                grammar::report(
                    emit,
                    "correctness/invalid-class-option",
                    trailing,
                    "`:default-initargs` needs a value for this initarg",
                );
            }
            for [key, value] in pairs {
                if let Some(symbol) = symbol(key) {
                    if let Some(identity) = ctx.control_name(symbol, package) {
                        duplicate(
                            &mut defaulted,
                            identity,
                            key.text_range(),
                            "default initarg",
                            emit,
                        );
                    }
                } else if key.kind() != SyntaxKind::ReaderForm {
                    grammar::report(
                        emit,
                        "correctness/invalid-class-option",
                        key,
                        "a default initarg name must be a symbol",
                    );
                }
                work.enqueue(value, &init_env);
            }
        } else if name == "DOCUMENTATION"
            || (op == "DEFCLASS" && name == "METACLASS")
            || (op == "DEFINE-CONDITION" && name == "REPORT")
        {
            record(&mut seen, name, key.text_range(), emit);
            if let [value] = values {
                if name == "REPORT" && super::is_lambda_expression(ctx, value) {
                    work.enqueue(value, &init_env);
                }
            } else {
                grammar::report(
                    emit,
                    "correctness/invalid-class-option",
                    option,
                    "this class option takes exactly one value",
                );
            }
        }
    }
    Some(())
}

fn keyword(el: &Element) -> Option<crate::reader_tokens::SymbolName> {
    symbol(el).filter(|name| name.prefix == Prefix::Keyword)
}

fn record(
    seen: &mut FxHashMap<String, TextRange>,
    name: &str,
    range: TextRange,
    emit: &mut LintEmitter<'_>,
) {
    duplicate(seen, name.to_owned(), range, "option", emit);
}

fn duplicate<Key: std::hash::Hash + Eq>(
    seen: &mut FxHashMap<Key, TextRange>,
    key: Key,
    range: TextRange,
    kind: &str,
    emit: &mut LintEmitter<'_>,
) {
    match seen.entry(key) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(range);
        }
        std::collections::hash_map::Entry::Occupied(entry) => {
            if let Some(diag) = emit.diagnostic(
                "correctness/duplicate-class-option",
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
