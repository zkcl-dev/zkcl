//! Checks for statically known names and spelling mistakes.

use crate::cl_builtins::{BuiltinKind, EXTERNAL_NAMES, builtin_kind, is_external_cl_symbol};
use crate::format::TextEdit;
use crate::outline::{Element, Token};
use crate::reader_tokens::{Prefix, SymbolName};

use super::super::context::{LintDocument, LintEmitter};
use super::super::{FixApplicability, LintFix};

pub(super) fn check_common_lisp_package_reference(
    ctx: &LintDocument<'_>,
    token: &Token,
    name: &SymbolName,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Prefix::Package {
        name: prefix,
        external: true,
    } = &name.prefix
    else {
        return;
    };
    if is_external_cl_symbol(&name.name)
        || ctx.workspace.resolve_package_prefix(package, prefix) != Some("COMMON-LISP")
    {
        return;
    }
    let suggestion = unique_nearby(&name.name, EXTERNAL_NAMES.iter().copied());
    let fix = suggestion.and_then(|replacement| {
        // A leading escape or vertical bars can conceal the package marker.
        // For straightforward spellings, change just the symbol component.
        let (package, _) = token.text().split_once(':')?;
        if token.text().contains(['|', '\\']) {
            return None;
        }
        let replacement = format!("{package}:{}", replacement.to_ascii_lowercase());
        Some(LintFix {
            title: format!("Replace with `{replacement}`"),
            applicability: FixApplicability::MaybeIncorrect,
            edits: vec![TextEdit {
                range: token.text_range(),
                replacement,
            }],
        })
    });
    emit.emit(
        "correctness/unknown-common-lisp-symbol",
        token.text_range(),
        format!("`{}` is not exported by `common-lisp`", token.text()),
        suggestion.map(|spelling| format!("did you mean `{}`?", spelling.to_ascii_lowercase())),
        Vec::new(),
        fix,
    );
}

pub(super) fn check_unresolved_operator(
    ctx: &LintDocument<'_>,
    head: &Element,
    name: &SymbolName,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    if !emit.is_enabled("suspicious/likely-misspelled-operator") || name.prefix != Prefix::None {
        return;
    }
    let Some(package) = package else { return };
    if !ctx.workspace.can_resolve_package(package) {
        return;
    }
    if ctx.workspace.may_define_callable(name, package) {
        return;
    }
    let suggestions = EXTERNAL_NAMES.iter().copied().filter(|candidate| {
        one_edit_apart(name.name.as_bytes(), candidate.as_bytes())
            && matches!(
                builtin_kind(candidate),
                Some(BuiltinKind::Macro | BuiltinKind::Function | BuiltinKind::SpecialOperator)
            )
    });
    if let Some(suggestion) = single_candidate(suggestions)
        && let Some(replacement) = SymbolName::parse(suggestion)
        && ctx.workspace.standard_symbol(&replacement, Some(package)) == Some(suggestion)
    {
        let replacement = suggestion.to_ascii_lowercase();
        emit.emit(
            "suspicious/likely-misspelled-operator",
            head.text_range(),
            format!(
                "unresolved operator `{}` looks like `{replacement}`",
                ctx.text_of(head.text_range())
            ),
            Some("check the spelling or bring the defining package into scope".into()),
            Vec::new(),
            Some(LintFix {
                title: format!("Replace with `{replacement}`"),
                applicability: FixApplicability::MaybeIncorrect,
                edits: vec![TextEdit {
                    range: head.text_range(),
                    replacement,
                }],
            }),
        );
    }
}

pub(super) fn unique_nearby<'a>(
    name: &str,
    candidates: impl Iterator<Item = &'a str>,
) -> Option<&'a str> {
    single_candidate(
        candidates.filter(|candidate| one_edit_apart(name.as_bytes(), candidate.as_bytes())),
    )
}

fn single_candidate<'a>(mut matches: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let candidate = matches.next()?;
    matches.next().is_none().then_some(candidate)
}

fn one_edit_apart(left: &[u8], right: &[u8]) -> bool {
    if left.len().min(right.len()) < 3
        || !left.is_ascii()
        || !right.is_ascii()
        || left.len().abs_diff(right.len()) > 1
    {
        return false;
    }
    let prefix = left.iter().zip(right).take_while(|(a, b)| a == b).count();
    let left = &left[prefix..];
    let right = &right[prefix..];
    if left.len() > right.len() {
        return left[1..] == *right;
    }
    if right.len() > left.len() {
        return right[1..] == *left;
    }
    if left.is_empty() {
        return false;
    }
    left[1..] == right[1..]
        || (left.len() >= 2
            && left[0] == right[1]
            && left[1] == right[0]
            && left[2..] == right[2..])
}
