//! Character names used by the standard readtable and character printing.

use std::borrow::Cow;

type Names = (char, Option<&'static str>, &'static [&'static str]);

// RFC 20 mnemonics, Unicode control aliases, and Unicode separator names.
// The second column preserves zkcl's printed names; other names are input aliases.
const NAMES: &[Names] = &[
    ('\0', Some("Nul"), &["NULL"]),
    ('\x01', None, &["SOH", "START_OF_HEADING"]),
    ('\x02', None, &["STX", "START_OF_TEXT"]),
    ('\x03', None, &["ETX", "END_OF_TEXT"]),
    ('\x04', None, &["EOT", "END_OF_TRANSMISSION"]),
    ('\x05', None, &["ENQ", "ENQUIRY"]),
    ('\x06', None, &["ACK", "ACKNOWLEDGE"]),
    ('\x07', None, &["BEL"]),
    ('\x08', Some("Backspace"), &["BS"]),
    ('\t', Some("Tab"), &["HT", "CHARACTER_TABULATION"]),
    ('\n', Some("Newline"), &["LF", "NL", "LINEFEED"]),
    ('\x0b', None, &["VT", "LINE_TABULATION"]),
    ('\x0c', Some("Page"), &["FF", "FORMFEED"]),
    ('\r', Some("Return"), &["CR"]),
    ('\x0e', None, &["SO", "SHIFT_OUT"]),
    ('\x0f', None, &["SI", "SHIFT_IN"]),
    ('\x10', None, &["DLE", "DATA_LINK_ESCAPE"]),
    ('\x11', None, &["DC1", "DEVICE_CONTROL_ONE"]),
    ('\x12', None, &["DC2", "DEVICE_CONTROL_TWO"]),
    ('\x13', None, &["DC3", "DEVICE_CONTROL_THREE"]),
    ('\x14', None, &["DC4", "DEVICE_CONTROL_FOUR"]),
    ('\x15', None, &["NAK", "NEGATIVE_ACKNOWLEDGE"]),
    ('\x16', None, &["SYN", "SYNCHRONOUS_IDLE"]),
    ('\x17', None, &["ETB", "END_OF_TRANSMISSION_BLOCK"]),
    ('\x18', None, &["CAN", "CANCEL"]),
    ('\x19', None, &["EM", "END_OF_MEDIUM"]),
    ('\x1a', None, &["SUB", "SUBSTITUTE"]),
    ('\x1b', None, &["ESC", "ESCAPE"]),
    ('\x1c', None, &["FS", "INFORMATION_SEPARATOR_FOUR"]),
    ('\x1d', None, &["GS", "INFORMATION_SEPARATOR_THREE"]),
    ('\x1e', None, &["RS", "INFORMATION_SEPARATOR_TWO"]),
    ('\x1f', None, &["US", "INFORMATION_SEPARATOR_ONE"]),
    (' ', Some("Space"), &["SP"]),
    ('\x7f', Some("Rubout"), &["DEL", "DELETE"]),
    // Unicode general categories Zs, Zl, and Zp, with spaces written as underscores.
    ('\u{00a0}', None, &["NO-BREAK_SPACE"]),
    ('\u{1680}', None, &["OGHAM_SPACE_MARK"]),
    ('\u{2000}', None, &["EN_QUAD"]),
    ('\u{2001}', None, &["EM_QUAD"]),
    ('\u{2002}', None, &["EN_SPACE"]),
    ('\u{2003}', None, &["EM_SPACE"]),
    ('\u{2004}', None, &["THREE-PER-EM_SPACE"]),
    ('\u{2005}', None, &["FOUR-PER-EM_SPACE"]),
    ('\u{2006}', None, &["SIX-PER-EM_SPACE"]),
    ('\u{2007}', None, &["FIGURE_SPACE"]),
    ('\u{2008}', None, &["PUNCTUATION_SPACE"]),
    ('\u{2009}', None, &["THIN_SPACE"]),
    ('\u{200a}', None, &["HAIR_SPACE"]),
    ('\u{2028}', None, &["LINE_SEPARATOR"]),
    ('\u{2029}', None, &["PARAGRAPH_SEPARATOR"]),
    ('\u{202f}', None, &["NARROW_NO-BREAK_SPACE"]),
    ('\u{205f}', None, &["MEDIUM_MATHEMATICAL_SPACE"]),
    ('\u{3000}', None, &["IDEOGRAPHIC_SPACE"]),
    ('\u{fffd}', Some("REPLACEMENT_CHARACTER"), &[]),
];

/// Return the character's canonical name, using its scalar code as a fallback.
pub fn character_name(character: char) -> Cow<'static, str> {
    NAMES
        .iter()
        .find(|&&(value, _, _)| value == character)
        .and_then(|&(_, name, _)| name)
        .map_or_else(
            || Cow::Owned(format!("U+{:04X}", u32::from(character))),
            Cow::Borrowed,
        )
}

/// Resolve a character name or `U+` followed by a Unicode scalar in hex.
pub fn named_character(name: &str) -> Option<char> {
    let matches = |candidate: &str| name.eq_ignore_ascii_case(candidate);
    NAMES
        .iter()
        .find(|&&(_, canonical, aliases)| {
            canonical.is_some_and(matches) || aliases.iter().any(|alias| matches(alias))
        })
        .map(|&(character, _, _)| character)
        .or_else(|| {
            let code = name
                .strip_prefix("U+")
                .or_else(|| name.strip_prefix("u+"))?;
            if code.is_empty() || !code.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            u32::from_str_radix(code, 16).ok().and_then(char::from_u32)
        })
}

/// Interpret the spelling after `#\`. The first character is single-escaped.
pub fn decode_character_payload(payload: &str) -> Option<char> {
    let mut input = payload.chars();
    let first = input.next()?;
    if input.as_str().is_empty() {
        return Some(first);
    }
    let mut name = String::from(first);
    let mut escaped = false;
    while let Some(character) = input.next() {
        match character {
            '\\' => name.push(input.next()?),
            '|' => escaped = !escaped,
            ':' if !escaped => return None,
            character => name.push(character),
        }
    }
    if escaped {
        return None;
    }
    let mut decoded = name.chars();
    let first = decoded.next()?;
    if decoded.as_str().is_empty() {
        Some(first)
    } else {
        named_character(&name)
    }
}

/// Lowercase a character using the reversible one-to-one case pairs of CLHS 13.1.4.3.4.
pub fn simple_lowercase(character: char) -> char {
    let mut mapping = character.to_lowercase();
    let lower = mapping.next().expect("a lowercase mapping is nonempty");
    if lower != character
        && mapping.next().is_none()
        && lower.to_uppercase().eq(std::iter::once(character))
    {
        lower
    } else {
        character
    }
}

/// Uppercase a character using the reversible one-to-one case pairs of CLHS 13.1.4.3.4.
pub fn simple_uppercase(character: char) -> char {
    let mut mapping = character.to_uppercase();
    let upper = mapping.next().expect("an uppercase mapping is nonempty");
    if upper != character && mapping.next().is_none() && simple_lowercase(upper) == character {
        upper
    } else {
        character
    }
}
