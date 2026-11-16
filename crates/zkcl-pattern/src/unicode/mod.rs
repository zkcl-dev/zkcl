mod grapheme;
mod properties;

use std::ops::Range;

use icu_casemap::CaseMapper;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

pub use grapheme::Graphemes;
pub use properties::{Property, PropertyError};

#[derive(Clone, Copy)]
pub enum Normalization {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

pub fn normalize(text: &str, form: Normalization) -> String {
    match form {
        Normalization::Nfc => text.nfc().collect(),
        Normalization::Nfd => text.nfd().collect(),
        Normalization::Nfkc => text.nfkc().collect(),
        Normalization::Nfkd => text.nfkd().collect(),
    }
}

pub fn case_fold(text: &str) -> String {
    CaseMapper::new().fold_string(text).into_owned()
}

pub fn simple_fold(character: char) -> char {
    CaseMapper::new().simple_fold(character)
}

pub fn segment(text: &str, words: bool) -> Vec<usize> {
    let mut end = 0;
    let mut boundaries = vec![0];
    if words {
        for word in text.split_word_bounds() {
            end += word.chars().count();
            boundaries.push(end);
        }
    } else {
        let mut state = Graphemes::default();
        for (index, character) in text.chars().enumerate() {
            if state.push(character) && index != 0 {
                boundaries.push(index);
            }
            end = index + 1;
        }
        if end != 0 {
            boundaries.push(end);
        }
    }
    boundaries
}

/// UTF-8 text together with positions in the original byte input.
pub struct DecodedUtf8 {
    pub text: String,
    /// Original byte boundary for every decoded character boundary, beginning at zero.
    pub byte_boundaries: Vec<usize>,
    /// Byte length of the decoded prefix, excluding incomplete or rejected input.
    pub consumed_bytes: usize,
    /// First malformed byte sequence when replacement is disabled.
    pub invalid_byte_range: Option<Range<usize>>,
}

pub fn decode(input: &[u8], eof: bool, replace: bool) -> DecodedUtf8 {
    let mut decoded = DecodedUtf8 {
        text: String::new(),
        byte_boundaries: vec![0],
        consumed_bytes: 0,
        invalid_byte_range: None,
    };
    while decoded.consumed_bytes != input.len() {
        let rest = &input[decoded.consumed_bytes..];
        let fault = match std::str::from_utf8(rest) {
            Ok(text) => {
                append(&mut decoded, text);
                break;
            }
            Err(error) => error,
        };
        let valid =
            std::str::from_utf8(&rest[..fault.valid_up_to()]).expect("validated UTF-8 prefix");
        append(&mut decoded, valid);
        let width = match fault.error_len() {
            Some(width) => width,
            None if eof => input.len() - decoded.consumed_bytes,
            None => break,
        };
        let end = decoded.consumed_bytes + width;
        if !replace {
            decoded.invalid_byte_range = Some(decoded.consumed_bytes..end);
            break;
        }
        decoded.text.push('\u{fffd}');
        decoded.byte_boundaries.push(end);
        decoded.consumed_bytes = end;
    }
    decoded
}

fn append(decoded: &mut DecodedUtf8, text: &str) {
    decoded.text.push_str(text);
    decoded.byte_boundaries.extend(
        text.char_indices()
            .map(|(start, character)| decoded.consumed_bytes + start + character.len_utf8()),
    );
    decoded.consumed_bytes += text.len();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximal_subparts_preserve_original_byte_boundaries() {
        let incomplete = decode(&[0xE1, 0x80], false, true);
        assert_eq!(
            (incomplete.text.as_str(), incomplete.consumed_bytes),
            ("", 0)
        );
        let replaced = decode(&[0xE1, 0x80, b'A', 0xE0, 0x80, 0x80], true, true);
        assert_eq!(replaced.text, "�A���");
        assert_eq!(replaced.byte_boundaries, [0, 2, 3, 4, 5, 6]);
        assert_eq!(
            decode(&[0xE1, 0x80, b'A'], true, false).invalid_byte_range,
            Some(0..2)
        );
        assert_eq!(decode(&[0xEF, 0xBB, 0xBF], true, false).text, "\u{feff}");
    }

    #[test]
    fn normalization_properties_and_case_folds_use_scalar_input() {
        assert_eq!(normalize("e\u{301}", Normalization::Nfc), "é");
        assert_eq!(case_fold("Straße Σς"), "strasse σσ");
        assert_eq!(simple_fold('ς'), 'σ');
        assert!(Property::parse("XID_Start", None).unwrap().contains('中'));
        assert!(!Property::parse("XID_Start", None).unwrap().contains('2'));
        assert!(Property::parse("gc", Some("Lu")).unwrap().contains('Ś'));
        assert!(Property::parse("scx", Some("Hira")).unwrap().contains('ー'));
        assert!(Property::parse("ASCII", None).unwrap().contains('A'));
        assert!(!Property::parse("ASCII", None).unwrap().contains('中'));
        assert!(Property::parse("Any", None).unwrap().contains('中'));
        assert!(Property::parse("Assigned", None).unwrap().contains('中'));
        assert!(
            !Property::parse("Assigned", None)
                .unwrap()
                .contains('\u{10fffe}')
        );
        assert!(
            Property::parse("Assigned", None)
                .unwrap()
                .ranges()
                .iter()
                .any(|range| range.contains(&(b'A' as u32)))
        );
    }

    #[test]
    fn graphemes_and_words_use_character_positions() {
        assert_eq!(segment("A中😀e\u{301}", false), [0, 1, 2, 3, 5]);
        assert_eq!(segment("🇺🇸🇨🇳", false), [0, 2, 4]);
        assert_eq!(segment("👩\u{200d}💻", false), [0, 3]);
        assert_eq!(segment("\u{915}\u{94d}\u{924}", false), [0, 3]);
        assert_eq!(segment("a! b", true), [0, 1, 2, 3, 4]);
    }
}
