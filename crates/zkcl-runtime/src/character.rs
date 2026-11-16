//! Unicode scalar characters and their standard-readtable names.

pub(crate) use zkcl_syntax::character_names::{
    character_name as name, decode_character_payload as from_token, named_character as named,
    simple_lowercase, simple_uppercase,
};

pub(crate) const CODE_LIMIT: u32 = 0x11_0000;

pub(crate) fn is_standard(character: char) -> bool {
    character == '\n' || (' '..='~').contains(&character)
}

pub(crate) fn is_alphanumeric(character: char) -> bool {
    character.is_alphabetic() || character.is_ascii_digit()
}

/// Convert a character index (including the end) to a UTF-8 byte offset.
pub(crate) fn utf8_byte_index(text: &str, index: usize) -> Option<usize> {
    text.char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(text.len()))
        .nth(index)
}

/// Convert a half-open range of character indices, checking both bounds.
pub(crate) fn utf8_byte_range(
    text: &str,
    start: usize,
    end: Option<usize>,
) -> Option<std::ops::Range<usize>> {
    let from = utf8_byte_index(text, start)?;
    let to = match end {
        Some(end) => from + utf8_byte_index(&text[from..], end.checked_sub(start)?)?,
        None => text.len(),
    };
    Some(from..to)
}
