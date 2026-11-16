use num_bigint::BigInt;

use crate::ast::{Element, Token};
use crate::character_names::decode_character_payload;
use crate::syntax::SyntaxKind;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Prefix {
    None,
    Keyword,
    Uninterned,
    Package { name: String, external: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SymbolName {
    pub prefix: Prefix,
    pub name: String,
}

const SYMBOL_CONSTITUENT_BYTES: [bool; 256] = {
    let mut table = [false; 256];
    let mut i = 0usize;
    while i < 256 {
        let b = i as u8;
        table[i] = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'/'
                    | b'<'
                    | b'='
                    | b'>'
                    | b'?'
                    | b'@'
                    | b'['
                    | b']'
                    | b'^'
                    | b'_'
                    | b'{'
                    | b'}'
                    | b'~'
            );
        i += 1;
    }
    table
};

// Maximum unescaped name length normalized on the stack for operator lookup.
const STACK_UPPER_CAP: usize = 96;

#[inline]
fn ascii_upper_into<'a>(slice: &str, buf: &'a mut [u8; STACK_UPPER_CAP]) -> Option<&'a str> {
    if slice.len() > STACK_UPPER_CAP {
        return None;
    }
    for (dst, &src) in buf.iter_mut().zip(slice.as_bytes()) {
        *dst = src.to_ascii_uppercase();
    }
    // SAFETY: uppercasing ASCII leaves every non-ASCII UTF-8 byte intact.
    Some(unsafe { std::str::from_utf8_unchecked(&buf[..slice.len()]) })
}

/// A borrowed symbol spelling, invalid syntax, or a request to parse escapes.
enum PlainScan<'a> {
    Invalid,
    NeedsSlowPath,
    Parsed {
        prefix: PlainPrefix<'a>,
        name: &'a str,
    },
}

#[derive(Clone, Copy)]
enum PlainPrefix<'a> {
    None,
    Keyword,
    Uninterned,
    Package { name: &'a str, external: bool },
}

fn scan_plain_symbol(text: &str) -> PlainScan<'_> {
    let (uninterned, body) = if let Some(rest) = text.strip_prefix("#:") {
        (true, rest)
    } else {
        (false, text)
    };
    let bytes = body.as_bytes();
    if bytes.is_empty() || bytes[0] == b'#' {
        return PlainScan::Invalid;
    }
    let mut colon_idx: Option<usize> = None;
    let mut all_dots = true;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\\' || b == b'|' {
            return PlainScan::NeedsSlowPath;
        }
        if b == b':' {
            if uninterned {
                return PlainScan::Invalid;
            }
            if let Some(first) = colon_idx {
                // Only one `:` or `::` separator is valid.
                if i == first + 1 && first > 0 && bytes.get(i + 1) != Some(&b':') {
                    continue;
                }
                return PlainScan::Invalid;
            }
            colon_idx = Some(i);
            all_dots = false;
            continue;
        }
        if !SYMBOL_CONSTITUENT_BYTES[usize::from(b)] {
            return PlainScan::Invalid;
        }
        if b != b'.' {
            all_dots = false;
        }
    }
    if all_dots {
        return PlainScan::Invalid;
    }

    let (prefix, tail) = match colon_idx {
        None => {
            // The scan already ruled out escapes, package markers, and all-dot names.
            if potential_number(body) || is_float_token(body) {
                return PlainScan::Invalid;
            }
            let prefix = if uninterned {
                PlainPrefix::Uninterned
            } else {
                PlainPrefix::None
            };
            (prefix, body)
        }
        Some(0) => {
            if bytes.get(1) == Some(&b':') {
                return PlainScan::Invalid;
            }
            (PlainPrefix::Keyword, &body[1..])
        }
        Some(idx) => {
            let pkg = &body[..idx];
            let double = bytes.get(idx + 1) == Some(&b':');
            let tail = &body[idx + 1 + usize::from(double)..];
            (
                PlainPrefix::Package {
                    name: pkg,
                    external: !double,
                },
                tail,
            )
        }
    };

    if tail.is_empty() {
        return PlainScan::Invalid;
    }
    PlainScan::Parsed { prefix, name: tail }
}

impl SymbolName {
    /// Interpret the spelling with the standard readtable (`:UPCASE`) and decimal read base.
    pub fn parse(text: &str) -> Option<Self> {
        match scan_plain_symbol(text) {
            PlainScan::Invalid => None,
            PlainScan::Parsed { prefix, name } => Some(Self {
                prefix: match prefix {
                    PlainPrefix::None => Prefix::None,
                    PlainPrefix::Keyword => Prefix::Keyword,
                    PlainPrefix::Uninterned => Prefix::Uninterned,
                    PlainPrefix::Package {
                        name: pkg,
                        external,
                    } => Prefix::Package {
                        name: pkg.to_ascii_uppercase(),
                        external,
                    },
                },
                name: name.to_ascii_uppercase(),
            }),
            PlainScan::NeedsSlowPath => Self::parse_escaped(text),
        }
    }

    /// Inspect the standard-operator spelling (unqualified or `CL:` / `COMMON-LISP:` qualified)
    /// without heap allocation in the common unescaped ASCII case.
    pub(crate) fn with_standard_operator<R>(text: &str, f: impl FnOnce(&str) -> R) -> Option<R> {
        match scan_plain_symbol(text) {
            PlainScan::Invalid => None,
            PlainScan::Parsed { prefix, name } => {
                match prefix {
                    PlainPrefix::None => {}
                    PlainPrefix::Package { name: pkg, .. }
                        if pkg.eq_ignore_ascii_case("CL")
                            || pkg.eq_ignore_ascii_case("COMMON-LISP") => {}
                    _ => return None,
                }
                let mut stack = [0u8; STACK_UPPER_CAP];
                if let Some(upper) = ascii_upper_into(name, &mut stack) {
                    Some(f(upper))
                } else {
                    Some(f(&name.to_ascii_uppercase()))
                }
            }
            PlainScan::NeedsSlowPath => {
                let sym = Self::parse_escaped(text)?;
                sym.standard_operator().map(f)
            }
        }
    }

    /// Check whether `text` names the given uppercase `COMMON-LISP` operator.
    /// Short unescaped spellings are checked without allocation.
    pub(crate) fn is_standard_operator_ci(text: &str, expected_upper: &str) -> bool {
        Self::with_standard_operator(text, |op| op == expected_upper).unwrap_or(false)
    }

    /// Decode the spelling while preserving case inside escapes.
    fn parse_escaped(text: &str) -> Option<Self> {
        let (mut prefix, text) = if let Some(rest) = text.strip_prefix("#:") {
            (Prefix::Uninterned, rest)
        } else {
            (Prefix::None, text)
        };
        if text.is_empty() || text.starts_with('#') || text.bytes().all(|byte| byte == b'.') {
            return None;
        }
        let mut chars = text.char_indices().peekable();
        let mut escaped = false;
        let mut has_escape_in_segment = false;
        let mut has_char_in_segment = false;
        let mut name = String::with_capacity(text.len());
        let mut start = 0;
        while let Some((index, ch)) = chars.next() {
            match ch {
                '\\' => {
                    name.push(chars.next()?.1);
                    has_escape_in_segment = true;
                    has_char_in_segment = true;
                }
                '|' => {
                    escaped = !escaped;
                    has_escape_in_segment = true;
                }
                ':' if !escaped => {
                    if prefix != Prefix::None {
                        return None;
                    }
                    let internal = chars.next_if(|&(_, next)| next == ':').is_some();
                    prefix = if index == 0 {
                        if internal {
                            return None;
                        }
                        Prefix::Keyword
                    } else {
                        if !has_char_in_segment && !has_escape_in_segment {
                            return None;
                        }
                        Prefix::Package {
                            name: std::mem::take(&mut name),
                            external: !internal,
                        }
                    };
                    start = index + 1 + usize::from(internal);
                    has_escape_in_segment = false;
                    has_char_in_segment = false;
                }
                ch if escaped
                    && (!ch.is_ascii_control() || matches!(ch, '\t' | '\n' | '\r' | '\x0c')) =>
                {
                    name.push(ch);
                    has_char_in_segment = true;
                }
                ch if is_symbol_constituent(ch) => {
                    name.push(ch.to_ascii_uppercase());
                    has_char_in_segment = true;
                }
                _ => return None,
            }
        }
        if escaped || (!has_char_in_segment && !has_escape_in_segment) {
            return None;
        }
        let tail = &text[start..];
        if !has_escape_in_segment
            && ((prefix == Prefix::None && is_numeric_token(tail))
                || (prefix == Prefix::Uninterned
                    && (tail.bytes().all(|byte| byte == b'.')
                        || potential_number(tail)
                        || is_float_token(tail))))
        {
            return None;
        }
        Some(Self { prefix, name })
    }

    pub(crate) fn zk_definition(&self) -> Option<&str> {
        if !matches!(&self.prefix, Prefix::Package { name, .. } if name == "ZK") {
            return None;
        }
        matches!(self.name.as_str(), "DEFVARIANT" | "DEFPATTERN").then_some(self.name.as_str())
    }

    pub(crate) fn standard_operator(&self) -> Option<&str> {
        match &self.prefix {
            Prefix::None => Some(&self.name),
            Prefix::Package { name, .. } if canonical_package(name) == "COMMON-LISP" => {
                Some(&self.name)
            }
            _ => None,
        }
    }
}

#[inline]
pub(crate) fn is_symbol_constituent(ch: char) -> bool {
    ch.is_ascii() && SYMBOL_CONSTITUENT_BYTES[ch as usize]
}

pub(crate) fn canonical_package(name: &str) -> &str {
    match name {
        "CL" => "COMMON-LISP",
        "CL-USER" => "COMMON-LISP-USER",
        _ => name,
    }
}

pub(crate) fn string_designator(element: &Element) -> Option<String> {
    let token = element.as_token()?;
    match token.kind() {
        SyntaxKind::Atom => SymbolName::parse(token.text()).map(|symbol| symbol.name),
        SyntaxKind::String | SyntaxKind::RawString => string_literal(token),
        SyntaxKind::Character => decode_character_token(token.text()).map(|ch| ch.to_string()),
        _ => None,
    }
}

pub(crate) fn string_literal(token: &Token) -> Option<String> {
    match token.kind() {
        SyntaxKind::String => decode_string_token(token.text()),
        SyntaxKind::RawString => {
            let mut value = String::with_capacity(token.text().len());
            let mut lines = token.text().split('\n').peekable();
            while let Some(line) = lines.next() {
                let payload = line.trim_start_matches([' ', '\t']).strip_prefix("#>")?;
                if lines.peek().is_some() {
                    value.push_str(payload.trim_suffix('\r'));
                    value.push('\n');
                } else {
                    value.push_str(payload);
                }
            }
            Some(value)
        }
        _ => None,
    }
}

pub(crate) fn decode_string_token(text: &str) -> Option<String> {
    let body = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = body.chars();
    let mut decoded = String::with_capacity(body.len());
    while let Some(ch) = chars.next() {
        decoded.push(if ch == '\\' { chars.next()? } else { ch });
    }
    Some(decoded)
}

/// Map a byte range in a decoded, ordinary string back through reader escapes.
/// A position at or beyond the decoded end points at the closing quote.
pub(crate) fn string_token_source_range(
    literal: &str,
    decoded: std::ops::Range<usize>,
) -> (usize, usize) {
    let closing = literal.len() - 1;
    let mut source = 1;
    let mut value_end = 0;
    let mut range_start = None;
    while source < closing {
        let start = source;
        if literal.as_bytes()[source] == b'\\' {
            source += 1;
        }
        let width = literal[source..]
            .chars()
            .next()
            .expect("complete string token")
            .len_utf8();
        source += width;
        value_end += width;
        if decoded.start < value_end && range_start.is_none() {
            range_start = Some(start);
        }
        if decoded.end <= value_end
            && let Some(range_start) = range_start
        {
            return (range_start, source);
        }
    }
    (
        range_start.unwrap_or(closing),
        if range_start.is_some() {
            closing
        } else {
            closing + 1
        },
    )
}

pub(crate) fn decode_character_token(text: &str) -> Option<char> {
    decode_character_payload(text.strip_prefix("#\\")?)
}

pub(crate) fn is_numeric_token(text: &str) -> bool {
    if text.contains(['\\', '|', ':']) {
        return false;
    }
    potential_number(text) || is_float_token(text)
}

/// Tests for a recognized numeric grammar. Some potential numbers (2^N,
/// for example) are accepted as symbols by implementations in practice.
pub(crate) fn is_number_literal_token(text: &str) -> bool {
    number_literal_kind(text).is_some()
}

/// The written form of a numeric literal under the standard readtable and decimal read base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberLiteralKind {
    Integer,
    Ratio,
    Float,
}

/// Classify a numeric token by spelling. A ratio may reduce to an integer or have a zero
/// denominator. An exponent may be outside the runtime's floating-point range.
pub fn number_literal_kind(text: &str) -> Option<NumberLiteralKind> {
    if let Some(parts) = exact_number_parts(text) {
        Some(if parts.denominator.is_some() {
            NumberLiteralKind::Ratio
        } else {
            NumberLiteralKind::Integer
        })
    } else {
        is_float_token(text).then_some(NumberLiteralKind::Float)
    }
}

/// Returns `true` for standard CL `#b`, `#o`, `#x`, or `#nR` integer/ratio tokens.
pub(crate) fn is_radix_number_token(text: &str) -> bool {
    text.starts_with('#') && exact_number_parts(text).is_some()
}

fn radix_number_parts(text: &str) -> Option<(u32, &str)> {
    let rest = text.strip_prefix('#')?;
    let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    let after_digits = &rest[digits_len..];
    let mut chars = after_digits.chars();
    let marker = chars.next()?;
    let radix = match marker.to_ascii_uppercase() {
        'B' if digits_len == 0 => 2,
        'O' if digits_len == 0 => 8,
        'X' if digits_len == 0 => 16,
        'R' if digits_len > 0 => rest[..digits_len].parse().ok()?,
        _ => return None,
    };
    (2..=36).contains(&radix).then_some((radix, chars.as_str()))
}

fn radix_digits(text: &str, radix: u32) -> bool {
    !text.is_empty() && text.chars().all(|digit| digit.is_digit(radix))
}

struct ExactNumberParts<'a> {
    radix: u32,
    numerator: &'a str,
    denominator: Option<&'a str>,
}

fn exact_number_parts(text: &str) -> Option<ExactNumberParts<'_>> {
    let (radix, body) = if text.starts_with('#') {
        radix_number_parts(text)?
    } else {
        (10, text)
    };
    let (radix, numerator, denominator) =
        if let Some((numerator, denominator)) = body.split_once('/') {
            if !radix_digits(denominator, radix) {
                return None;
            }
            (radix, numerator, Some(denominator))
        } else if let Some(integer) = body.strip_suffix('.') {
            (10, integer, None)
        } else {
            (radix, body, None)
        };
    radix_digits(numerator.trim_prefix(['+', '-']), radix).then_some(ExactNumberParts {
        radix,
        numerator,
        denominator,
    })
}

/// Parse an exact token into its integer numerator and optional nonzero denominator.
pub(crate) fn exact_number_token(text: &str) -> Option<(BigInt, Option<BigInt>)> {
    let parts = exact_number_parts(text)?;
    let numerator = BigInt::parse_bytes(parts.numerator.as_bytes(), parts.radix)?;
    let denominator = match parts.denominator {
        Some(digits) => {
            let denominator = BigInt::parse_bytes(digits.as_bytes(), parts.radix)?;
            if denominator == BigInt::ZERO {
                return None;
            }
            Some(denominator)
        }
        None => None,
    };
    Some((numerator, denominator))
}

/// Canonical integer value of a token read with the standard readtable and read base.
pub(crate) fn integer_token(text: &str) -> Option<String> {
    let (numerator, denominator) = exact_number_token(text)?;
    let value = match denominator {
        Some(denominator) if &numerator % &denominator == BigInt::ZERO => numerator / denominator,
        Some(_) => return None,
        None => numerator,
    };
    Some(value.to_str_radix(10))
}

/// Returns `true` for standard CL `#*...` bit-vector literals.
pub(crate) fn is_bit_vector_token(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('#') else {
        return false;
    };
    let after_digits = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let Some(bits) = after_digits.strip_prefix('*') else {
        return false;
    };
    bits.bytes().all(|b| b == b'0' || b == b'1')
}

fn is_float_token(text: &str) -> bool {
    let unsigned = text.trim_prefix(['+', '-']);
    let whole = unsigned.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &unsigned[whole..];
    let (fraction, rest) = if let Some(fractional) = rest.strip_prefix('.') {
        let fraction = fractional.bytes().take_while(u8::is_ascii_digit).count();
        (fraction, &fractional[fraction..])
    } else {
        (0, rest)
    };
    if whole + fraction == 0 {
        return false;
    }
    if rest.is_empty() {
        return fraction > 0;
    }
    let Some((&marker, _)) = rest.as_bytes().split_first() else {
        return false;
    };
    if !matches!(
        marker.to_ascii_uppercase(),
        b'E' | b'S' | b'F' | b'D' | b'L'
    ) {
        return false;
    }
    let exponent = &rest[1..];
    let exp_digits = exponent.trim_prefix(['+', '-']);
    !exp_digits.is_empty() && exp_digits.bytes().all(|b| b.is_ascii_digit())
}

// Under decimal read syntax, adjacent letter markers disqualify a potential number.
pub(crate) fn potential_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_digit() || b"+-.^".contains(byte))
        && !text.ends_with(['+', '-'])
        && bytes.iter().any(u8::is_ascii_digit)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+-/.^_".contains(byte))
        && !bytes
            .windows(2)
            .any(|pair| pair.iter().all(u8::is_ascii_alphabetic))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_name_parsing_respects_reader_escape_and_package_syntax() {
        assert_eq!(
            SymbolName::parse("||"),
            Some(SymbolName {
                prefix: Prefix::None,
                name: String::new(),
            })
        );
        assert_eq!(
            SymbolName::parse(":||"),
            Some(SymbolName {
                prefix: Prefix::Keyword,
                name: String::new(),
            })
        );
        assert_eq!(
            SymbolName::parse("||::||"),
            Some(SymbolName {
                prefix: Prefix::Package {
                    name: String::new(),
                    external: false,
                },
                name: String::new(),
            })
        );
        assert_eq!(
            SymbolName::parse("|123|"),
            Some(SymbolName {
                prefix: Prefix::None,
                name: "123".into(),
            })
        );
        assert_eq!(
            SymbolName::parse(r"+\1"),
            Some(SymbolName {
                prefix: Prefix::None,
                name: "+1".into(),
            })
        );
        assert_eq!(
            SymbolName::parse(":15"),
            Some(SymbolName {
                prefix: Prefix::Keyword,
                name: "15".into(),
            })
        );
        assert_eq!(
            SymbolName::parse(":..."),
            Some(SymbolName {
                prefix: Prefix::Keyword,
                name: "...".into(),
            })
        );
        assert_eq!(
            SymbolName::parse("1+"),
            Some(SymbolName {
                prefix: Prefix::None,
                name: "1+".into(),
            })
        );
        assert_eq!(
            SymbolName::parse("25-dec-83"),
            Some(SymbolName {
                prefix: Prefix::None,
                name: "25-DEC-83".into(),
            })
        );

        for invalid in [
            "123", "+1", "-.5", "1.0e6", ":", "::a", "pkg:", "pkg:::a", "#:123", "...",
        ] {
            assert_eq!(SymbolName::parse(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn character_tokens_decode_reader_names_to_scalars() {
        for (token, expected) in [
            (r"#\A", Some('A')),
            (r"#\Space", Some(' ')),
            (r"#\N\ewline", Some('\n')),
            (r"#\N|ewline|", Some('\n')),
            (r"#\Rubout", Some('\x7f')),
            (r"#\Backspace", Some('\x08')),
            (r"#\Nul", Some('\0')),
            (r"#\Null", Some('\0')),
            (r"#\bEl", Some('\x07')),
            (r"#\V|t|", Some('\x0b')),
            (r"#\LINE_TABULATION", Some('\x0b')),
            (r"#\ST\X", Some('\x02')),
            (r"#\No-break_space", Some('\u{00a0}')),
            (r"#\Narrow_No-break_space", Some('\u{202f}')),
            (r"#\Line_Separator", Some('\u{2028}')),
            (r"#\Paragraph_Separator", Some('\u{2029}')),
            (r"#\replacement_character", Some('\u{fffd}')),
            (r"#\U+0041", Some('A')),
            (r"#\U+1F600", Some('😀')),
            (r"#2\A", None),
            (r"#\Bel|", None),
            (r"#\Vt\", None),
            (r"#\Bel:Vt", None),
            (r"#\No_break_space", None),
            (r"#\No:Package", None),
            (r"#\UnknownName", None),
        ] {
            assert_eq!(decode_character_token(token), expected, "{token}");
        }
    }

    #[test]
    fn radix_number_tokens_require_a_valid_base_and_digits() {
        for valid in [
            "#b101",
            "#B-10/11",
            "#o755",
            "#xdeadBEEF",
            "#16rFF",
            "#2r1010",
        ] {
            assert!(is_radix_number_token(valid), "{valid}");
        }
        for invalid in ["#b2", "#xZZ", "#1r0", "#37rA", "#r10", "#*101"] {
            assert!(!is_radix_number_token(invalid), "{invalid}");
        }
    }

    #[test]
    fn number_literal_kinds_describe_written_syntax() {
        for (text, kind) in [
            ("+18446744073709551617", NumberLiteralKind::Integer),
            ("-123.", NumberLiteralKind::Integer),
            ("#x10.", NumberLiteralKind::Integer),
            ("#36rZ", NumberLiteralKind::Integer),
            ("#2R-100/11", NumberLiteralKind::Ratio),
            ("8/2", NumberLiteralKind::Ratio),
            ("0/0", NumberLiteralKind::Ratio),
            ("-.5", NumberLiteralKind::Float),
            ("1.e+2", NumberLiteralKind::Float),
            ("1f0", NumberLiteralKind::Float),
            ("1d0", NumberLiteralKind::Float),
            ("1L0", NumberLiteralKind::Float),
            ("1s0", NumberLiteralKind::Float),
        ] {
            assert_eq!(number_literal_kind(text), Some(kind), "{text}");
        }
        for text in [
            "", "-", ".", "2^N", "7/+2", "|1|", "#:1", "#xA.", "#2r3", "1e",
        ] {
            assert_eq!(number_literal_kind(text), None, "{text}");
        }
        assert_eq!(integer_token("#16rA/2").as_deref(), Some("5"));
        assert_eq!(integer_token("#2r1/0"), None);
        assert_eq!(integer_token("1/2"), None);
    }

    #[test]
    fn bit_vector_tokens_only_accept_binary_digits() {
        assert!(is_bit_vector_token("#*"));
        assert!(is_bit_vector_token("#4*10"));
        assert!(!is_bit_vector_token("#*102"));
    }

    #[test]
    fn fast_plain_symbol_scan_matches_escaped_parser_and_zero_alloc_operator_checks() {
        let long_name = "a".repeat(STACK_UPPER_CAP + 16);
        let long_expected = "A".repeat(STACK_UPPER_CAP + 16);
        let cases = [
            "defun",
            "cl:let",
            "common-lisp:progn",
            "CL::IF",
            ":key",
            ":123",
            "#:uninterned",
            "pkg:123",
            "pkg:...",
            "...",
            "#:...",
            ":",
            "::a",
            "pkg:",
            "pkg::",
            "pkg:::a",
            "a::b:c",
            "123",
            "#:123",
            "#:2^N",
            "1+",
            "|a|:b",
            "pkg:|123|",
            r"+\1",
            "",
            "#b10",
            "#36rZ",
            long_name.as_str(),
        ];
        for text in cases {
            assert_eq!(
                SymbolName::parse(text),
                SymbolName::parse_escaped(text),
                "fast/slow mismatch for {text:?}"
            );
            let via_struct = SymbolName::parse_escaped(text)
                .and_then(|s| s.standard_operator().map(str::to_owned));
            let via_fast = SymbolName::with_standard_operator(text, str::to_owned);
            assert_eq!(
                via_fast, via_struct,
                "with_standard_operator mismatch for {text:?}"
            );
        }
        assert!(SymbolName::is_standard_operator_ci("cl:defun", "DEFUN"));
        assert!(SymbolName::is_standard_operator_ci("common-lisp::if", "IF"));
        assert!(SymbolName::is_standard_operator_ci(
            &long_name,
            &long_expected
        ));
        assert!(!SymbolName::is_standard_operator_ci(":defun", "DEFUN"));
    }
}
