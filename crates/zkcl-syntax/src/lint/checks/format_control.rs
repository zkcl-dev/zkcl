//! Validate literal FORMAT control syntax and count a guaranteed argument prefix.

use crate::outline::Element;
use crate::reader_tokens::string_literal;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

use super::super::context::LintEmitter;

pub(super) fn check(list: &SyntaxNode, args: &[Element], emit: &mut LintEmitter<'_>) {
    let Some(control) = args.get(1).and_then(Element::as_token) else {
        return;
    };
    let Some(text) = string_literal(control) else {
        return;
    };
    let checking_arity = emit.is_enabled("correctness/format-arity-mismatch");
    let checking_syntax = emit.is_enabled("correctness/invalid-format-control");
    if !checking_arity && !checking_syntax {
        return;
    }
    let mut chars = text.char_indices().peekable();
    let mut open_groups = Vec::new();
    let mut prefix = Some(0usize);
    let mut needed = 0;
    let mut first_missing = None;
    let actual = args.len() - 2;
    let mut error = None;
    while let Some((start, ch)) = chars.next() {
        if ch != '~' {
            continue;
        }
        let mut variables = 0;
        while let Some((_, param)) = chars.peek().copied() {
            match param {
                'v' | 'V' => {
                    chars.next();
                    variables += 1;
                }
                '0'..='9' | ',' | '#' => {
                    chars.next();
                }
                '+' | '-'
                    if text
                        .as_bytes()
                        .get(chars.peek().unwrap().0 + 1)
                        .is_some_and(u8::is_ascii_digit) =>
                {
                    chars.next();
                }
                '\'' => {
                    chars.next();
                    chars.next();
                }
                _ => break,
            }
        }
        let mut colon = false;
        let mut at_sign = false;
        while let Some((offset, modifier @ (':' | '@'))) = chars.peek().copied() {
            chars.next();
            let flag = if modifier == ':' {
                &mut colon
            } else {
                &mut at_sign
            };
            if std::mem::replace(flag, true) {
                error = Some((
                    start..offset + 1,
                    "duplicate FORMAT directive modifier",
                    None,
                ));
                break;
            }
        }
        if error.is_some() {
            break;
        }
        let Some((offset, op)) = chars.next() else {
            error = Some((
                start..text.len(),
                "incomplete FORMAT directive after `~`",
                None,
            ));
            break;
        };
        let directive = start..offset + op.len_utf8();
        let consumption = match op.to_ascii_uppercase() {
            '%' | '&' | '|' | '~' | '\n' | '\r' | 'T' => Some(0),
            'A' | 'S' | 'W' | 'D' | 'X' | 'O' | 'B' | 'F' | 'E' | 'G' | 'C' | '$' | 'R' => Some(1),
            'P' if !colon => Some(1),
            '(' | '[' | '{' | '<' => {
                open_groups.push((op, directive.clone()));
                None
            }
            ')' | ']' | '}' | '>' => {
                let matching = match op {
                    ')' => '(',
                    ']' => '[',
                    '}' => '{',
                    _ => '<',
                };
                let open = open_groups.pop();
                if !open.as_ref().is_some_and(|(op, _)| *op == matching) {
                    error = Some((
                        directive,
                        "unmatched FORMAT closing directive",
                        open.map(|(_, range)| range),
                    ));
                    break;
                }
                None
            }
            ';' => {
                if !open_groups
                    .last()
                    .is_some_and(|(op, _)| matches!(op, '[' | '<'))
                {
                    error = Some((
                        directive,
                        "`~;` requires a conditional or justification group",
                        None,
                    ));
                    break;
                }
                None
            }
            '/' => {
                if !chars.by_ref().any(|(_, ch)| ch == '/') {
                    error = Some((
                        start..text.len(),
                        "`~/name/` is missing its final slash",
                        None,
                    ));
                    break;
                }
                Some(1)
            }
            '?' | '*' | '^' | '_' | 'I' | 'P' => None,
            _ => {
                error = Some((directive, "unrecognized ANSI FORMAT directive", None));
                break;
            }
        };
        if let Some(processed) = prefix {
            needed = processed + variables + consumption.unwrap_or(0);
            if needed > actual && first_missing.is_none() {
                first_missing = Some(directive);
            }
            prefix = consumption.map(|_| needed);
        }
    }
    if let Some((_, range)) = open_groups.pop().filter(|_| error.is_none()) {
        error = Some((range, "FORMAT group is missing its closing directive", None));
    }
    if let Some((range, message, opening)) = error {
        if let Some(diag) = emit.diagnostic(
            "correctness/invalid-format-control",
            source_range(control, range),
            message,
            None,
            Vec::new(),
            None,
        ) && let Some(opening) = opening
        {
            diag.note(source_range(control, opening), "this group opened here");
        }
        return;
    }
    if let Some(missing) = first_missing
        && let Some(diag) = emit.diagnostic("correctness/format-arity-mismatch", list.text_range(),
            format!("`format` control string needs at least {needed} argument{}, only {actual} supplied",
                if needed == 1 { "" } else { "s" }), None, Vec::new(), None)
    {
        diag.note(source_range(control, missing), "this directive consumes an argument that was not supplied");
    }
}

fn source_range(token: &crate::outline::Token, decoded: std::ops::Range<usize>) -> TextRange {
    if token.kind() != SyntaxKind::String {
        return token.text_range();
    }
    let (start, end) = crate::reader_tokens::string_token_source_range(token.text(), decoded);
    let base = token.text_range().start();
    TextRange::new(
        base + TextSize::from(start as u32),
        base + TextSize::from(end as u32),
    )
}
