//! Check literal ZK-FFI declarations without evaluating source.

use crate::foreign::{ForeignScalar, MAX_STRUCT_DEPTH};
use crate::navigation::ScopeBinding;
use crate::navigation::index::{closed_list, symbol};
use crate::navigation::queries::find_local_fn;
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName, integer_token};
use crate::syntax::SyntaxKind;

use super::super::context::{
    FormContext, LintDocument, LintEmitter, is_char_literal_el, is_nil_element,
    is_number_literal_el, is_string_literal_el, string_literal_value, unwrap_quote,
};

const RULE: &str = "correctness/ffi-declaration";

pub(super) fn is_operator(
    ctx: &LintDocument<'_>,
    head: &SymbolName,
    package: Option<&str>,
) -> bool {
    crate::foreign::signature(&head.name).is_some()
        && ctx.workspace.symbol_home_package(head, package) == Some("ZK-FFI")
}

pub(super) fn check_declaration(
    ctx: &LintDocument<'_>,
    head: &SymbolName,
    args: &[Element],
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if !is_operator(ctx, head, package) || !emit.is_enabled(RULE) {
        return;
    }
    let check = |argument: &Element, usage, emit: &mut LintEmitter<'_>| {
        let datum = unwrap_quote(argument, package, ctx.workspace);
        check_type(
            ctx,
            datum.as_ref().unwrap_or(argument),
            package,
            usage,
            datum.is_some(),
            0,
            emit,
        )
    };

    if let Some((result_index, variadic)) = binding_signature(&head.name)
        .or_else(|| (head.name == "MAKE-CALLBACK").then_some((1, false)))
    {
        let Some((result, params)) = args.get(result_index..).and_then(<[_]>::split_first) else {
            return;
        };
        check(result, TypeUse::Result, emit);
        for param in params {
            check(param, TypeUse::Object, emit);
        }
        if variadic {
            let fixed = &args[result_index - 1];
            if let Some(count) = literal_integer(ctx, fixed, package)
                && !count
                    .parse::<usize>()
                    .is_ok_and(|count| (1..=params.len()).contains(&count))
            {
                report(
                    fixed,
                    "fixed parameter count must be positive and at most the total parameter count",
                    emit,
                );
            }
        }
        return;
    }

    let (type_index, usage) = match head.name.as_str() {
        "TYPE-SIZE" | "TYPE-ALIGNMENT" => (0, TypeUse::Object),
        "FIELD-OFFSET" => (0, TypeUse::Structure),
        "MEM-REF" | "MEM-SET" => (1, TypeUse::Scalar),
        _ => return,
    };
    if let Some(ty) = args.get(type_index)
        && let Some(field_count) = check(ty, usage, emit)
        && matches!(usage, TypeUse::Structure)
        && let Some(index) = args.get(1)
        && let Some(index_text) = literal_integer(ctx, index, package)
        && !index_text
            .parse::<usize>()
            .is_ok_and(|index| index < field_count)
    {
        report(
            index,
            format!("C structure field index is out of bounds for {field_count} fields"),
            emit,
        );
    }
}

fn literal_integer(ctx: &LintDocument<'_>, el: &Element, package: Option<&str>) -> Option<String> {
    let quoted = unwrap_quote(el, package, ctx.workspace);
    let token = quoted.as_ref().unwrap_or(el).as_token()?;
    if token.kind() == SyntaxKind::Atom {
        integer_token(token.text())
    } else {
        None
    }
}

pub(super) fn check_inline_call(
    ctx: &LintDocument<'_>,
    args: &[Element],
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
    emit: &mut LintEmitter<'_>,
) {
    let Some((target, actual)) = args.split_first() else {
        return;
    };
    let Some(list) = target.as_node() else {
        return;
    };
    let Some((head, binding_args, _)) = ctx.resolve_head(list, package, lexicals, env) else {
        return;
    };
    let Some((result_index, _)) = binding_signature(&head.name) else {
        return;
    };
    if !is_operator(ctx, &head, package)
        || env.is_fn_shadowed(&head)
        || find_local_fn(lexicals, &head, package, ctx.workspace).is_some()
    {
        return;
    }
    if let Some(expected) = binding_args.len().checked_sub(result_index + 1)
        && expected != actual.len()
    {
        emit.emit(
            "correctness/arity-mismatch",
            target.text_range(),
            format!(
                "foreign binding declares {expected} arguments, but `funcall` passes {}",
                actual.len()
            ),
            None,
            Vec::new(),
            None,
        );
        return;
    }

    if env.in_loop
        && head.name == "BIND-FUNCTION"
        && let [library, name, result, params @ ..] = binding_args.as_slice()
        && keyword(ctx, library, package).as_deref() == Some("HOST")
        && string_literal_value(name).is_some_and(|name| !name.is_empty() && !name.contains('\0'))
        && keyword(ctx, result, package)
            .and_then(|ty| ForeignScalar::from_keyword(&ty))
            .is_some()
        && params.iter().all(|param| {
            keyword(ctx, param, package)
                .and_then(|ty| ForeignScalar::from_keyword(&ty))
                .is_some_and(|ty| ty != ForeignScalar::Void)
        })
    {
        emit.emit(
            "perf/foreign-binding-in-loop",
            target.text_range(),
            "this call site prepares the same `:host` foreign binding on each iteration",
            Some("prepare the binding before the loop and reuse it with `funcall`".into()),
            Vec::new(),
            None,
        );
    }
}

fn binding_signature(name: &str) -> Option<(usize, bool)> {
    Some(match name {
        "BIND-FUNCTION" => (2, false),
        "BIND-POINTER" => (1, false),
        "BIND-VARIADIC-FUNCTION" => (3, true),
        "BIND-VARIADIC-POINTER" => (2, true),
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum TypeUse {
    Result,
    Object,
    Scalar,
    Structure,
}

fn keyword(ctx: &LintDocument<'_>, el: &Element, package: Option<&str>) -> Option<String> {
    let spelling = symbol(el)?;
    match &spelling.prefix {
        Prefix::Keyword => Some(spelling.name),
        Prefix::Package { name, .. }
            if ctx.workspace.resolve_package_prefix(package, name) == Some("KEYWORD") =>
        {
            Some(spelling.name)
        }
        _ => None,
    }
}

fn check_type(
    ctx: &LintDocument<'_>,
    el: &Element,
    package: Option<&str>,
    usage: TypeUse,
    quoted: bool,
    depth: usize,
    emit: &mut LintEmitter<'_>,
) -> Option<usize> {
    if let Some(name) = keyword(ctx, el, package) {
        match ForeignScalar::from_keyword(&name) {
            None => report(el, format!("unknown ZK-FFI type keyword `:{name}`"), emit),
            Some(ForeignScalar::Void) if !matches!(usage, TypeUse::Result) => {
                report(el, "`:void` is only valid as a result type", emit);
            }
            Some(_) if matches!(usage, TypeUse::Structure) => {
                report(el, "expected a C structure type", emit);
            }
            _ => {}
        }
        return None;
    }

    if quoted && let Some(list) = el.as_node().filter(|node| closed_list(node)) {
        let fields = ctx.read_elements(elements(list))?;
        let Some((head, fields)) = fields.split_first() else {
            report(el, "a C type cannot be an empty list", emit);
            return None;
        };
        // Reader macros such as #. may produce an arbitrary keyword.
        if head.kind() == SyntaxKind::ReaderForm {
            return None;
        }
        if keyword(ctx, head, package).as_deref() != Some("STRUCT") {
            report(head, "a compound C type must start with `:struct`", emit);
        } else if fields.is_empty() {
            report(el, "a C structure requires at least one field", emit);
        } else if matches!(usage, TypeUse::Scalar) {
            report(el, "this memory operation requires a scalar C type", emit);
        } else if depth >= MAX_STRUCT_DEPTH {
            report(
                el,
                format!("C structure nesting exceeds {MAX_STRUCT_DEPTH} levels"),
                emit,
            );
        } else {
            for field in fields {
                check_type(ctx, field, package, TypeUse::Object, true, depth + 1, emit);
            }
            return Some(fields.len());
        }
        return None;
    }

    if (quoted && symbol(el).is_some())
        || is_nil_element(el, package, ctx.workspace)
        || is_number_literal_el(el)
        || is_char_literal_el(el)
        || is_string_literal_el(el)
        || el.kind() == SyntaxKind::Vector
    {
        report(el, "expected a ZK-FFI type designator", emit);
    }
    None
}

fn report(el: &Element, message: impl Into<String>, emit: &mut LintEmitter<'_>) {
    emit.emit(RULE, el.text_range(), message, None, Vec::new(), None);
}
