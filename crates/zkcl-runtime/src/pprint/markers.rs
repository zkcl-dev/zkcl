//! Encode and decode the string representation used by FORMAT.
use super::{IndentKind, LayoutItem, LayoutToken, NewlineKind, TabKind};

pub(super) const FORMAT_MARK: char = '\u{e000}';

pub(super) fn encode_format_markers(items: &[LayoutItem]) -> String {
    let mut out = String::new();
    for item in items {
        match &item.token {
            LayoutToken::Text(text) => out.push_str(text),
            LayoutToken::Newline(NewlineKind::Literal) => out.push('\n'),
            LayoutToken::Newline(kind) => out.push_str(newline_marker_for(*kind)),
            LayoutToken::Indent(kind, amount) => {
                out.push_str(&indent_marker_for(
                    matches!(kind, IndentKind::Current),
                    *amount,
                ));
            }
            LayoutToken::Tab(kind, colnum, colinc) => {
                out.push_str(&tab_marker_for(*kind, *colnum, *colinc));
            }
            LayoutToken::BlockStart(per_line) => {
                out.push_str(&logical_block_marker(per_line.as_deref()));
            }
            LayoutToken::BlockEnd => out.push_str(&block_end_marker()),
        }
    }
    out
}

pub(super) fn parse_format_markers(mut text: &str) -> Vec<LayoutItem> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut plain = String::new();
    while let Some(index) = text.find(['\n', FORMAT_MARK]) {
        plain.push_str(&text[..index]);
        text = &text[index..];
        let token = if let Some(rest) = text.strip_prefix('\n') {
            text = rest;
            LayoutToken::Newline(NewlineKind::Literal)
        } else {
            text = &text[FORMAT_MARK.len_utf8()..];
            let Some((token, rest)) = decode_marker(text) else {
                plain.push(FORMAT_MARK);
                continue;
            };
            text = rest;
            token
        };
        if !plain.is_empty() {
            items.push(LayoutItem {
                depth,
                token: LayoutToken::Text(std::mem::take(&mut plain)),
            });
        }
        if matches!(token, LayoutToken::BlockStart(_)) {
            depth += 1;
        }
        let ends_block = matches!(token, LayoutToken::BlockEnd);
        items.push(LayoutItem { depth, token });
        if ends_block {
            depth = depth.saturating_sub(1);
        }
    }
    plain.push_str(text);
    if !plain.is_empty() {
        items.push(LayoutItem {
            depth,
            token: LayoutToken::Text(plain),
        });
    }
    items
}

/// A numeric field ends at the first non-digit. Its delimiter is optional.
fn numeric_field<'a>(text: &mut &'a str, delimiter: char, signed: bool) -> &'a str {
    let end = text
        .find(|ch: char| !(ch.is_ascii_digit() || signed && ch == '-'))
        .unwrap_or(text.len());
    let (digits, rest) = text.split_at(end);
    *text = rest.trim_prefix(delimiter);
    digits
}

fn decode_marker(text: &str) -> Option<(LayoutToken, &str)> {
    let mut chars = text.chars();
    let code = chars.next()?;
    let mut rest = chars.as_str();
    let token = match code {
        'l' => LayoutToken::Newline(NewlineKind::Linear),
        'f' => LayoutToken::Newline(NewlineKind::Fill),
        'm' => LayoutToken::Newline(NewlineKind::Miser),
        'n' => LayoutToken::Newline(NewlineKind::Mandatory),
        'N' => LayoutToken::Newline(NewlineKind::Literal),
        'I' => {
            let kind = match chars.next()? {
                'c' => IndentKind::Current,
                'b' => IndentKind::Block,
                _ => return None,
            };
            rest = chars.as_str();
            let amount = numeric_field(&mut rest, ';', true).parse().unwrap_or(0);
            LayoutToken::Indent(kind, amount)
        }
        'T' => {
            let kind = match chars.next()? {
                '0' => TabKind::Line,
                '1' => TabKind::Section,
                '2' => TabKind::LineRelative,
                '3' => TabKind::SectionRelative,
                _ => return None,
            };
            rest = chars.as_str();
            let column = numeric_field(&mut rest, ',', false).parse().unwrap_or(0);
            let increment = numeric_field(&mut rest, ';', false).parse().unwrap_or(1);
            LayoutToken::Tab(kind, column, increment)
        }
        'B' => {
            let digits = numeric_field(&mut rest, ':', false);
            let length = digits.parse().unwrap_or(0);
            let end = rest
                .char_indices()
                .nth(length)
                .map_or(rest.len(), |(byte, _)| byte);
            let (prefix, suffix) = rest.split_at(end);
            rest = suffix.trim_prefix(';');
            LayoutToken::BlockStart((!digits.is_empty()).then(|| prefix.to_owned()))
        }
        'E' => LayoutToken::BlockEnd,
        _ => return None,
    };
    Some((token, rest))
}

pub(crate) fn logical_block_marker(per_line_prefix: Option<&str>) -> String {
    match per_line_prefix {
        None => format!("{FORMAT_MARK}B;"),
        Some(prefix) => format!("{FORMAT_MARK}B{}:{prefix};", prefix.chars().count()),
    }
}

pub(crate) fn newline_marker_for(kind: NewlineKind) -> &'static str {
    match kind {
        NewlineKind::Linear => "\u{e000}l",
        NewlineKind::Fill => "\u{e000}f",
        NewlineKind::Miser => "\u{e000}m",
        NewlineKind::Mandatory => "\u{e000}n",
        NewlineKind::Literal => "\u{e000}N",
    }
}

pub(crate) fn indent_marker_for(current: bool, amount: i64) -> String {
    format!("{FORMAT_MARK}I{}{amount};", if current { 'c' } else { 'b' })
}

pub(crate) fn tab_marker_for(kind: TabKind, colnum: usize, colinc: usize) -> String {
    let digit = match kind {
        TabKind::Line => 0,
        TabKind::Section => 1,
        TabKind::LineRelative => 2,
        TabKind::SectionRelative => 3,
    };
    format!("{FORMAT_MARK}T{digit}{colnum},{colinc};")
}

pub(crate) fn block_end_marker() -> String {
    format!("{FORMAT_MARK}E")
}
