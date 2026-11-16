use std::ops::Range;
use std::sync::Arc;

use crate::SourceDocument;

/// A zero-based source position. By default, columns count Unicode scalar values.
/// The encoded conversion methods count code units in the chosen encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourcePosition {
    pub line: usize,
    pub column: usize,
}

impl SourcePosition {
    /// Maps a half-open range in source text to UTF-8 byte offsets without
    /// parsing. Columns past the end of a line clamp to its end. Both
    /// lines must exist and both positions must be on character boundaries.
    pub fn clamped_byte_range(
        text: &str,
        range: Range<Self>,
        encoding: SourcePositionEncoding,
    ) -> Option<Range<usize>> {
        let Range { start, end } = range;
        let line_delta = end.line.checked_sub(start.line)?;
        let mut starts = line_starts(text).skip(start.line).peekable();
        let start_offset = starts.next()?;
        let start_text = line_text(text, start_offset, starts.peek().copied());
        let (end_offset, end_text) = if line_delta == 0 {
            (start_offset, start_text)
        } else {
            let offset = starts.nth(line_delta - 1)?;
            (offset, line_text(text, offset, starts.peek().copied()))
        };
        let start = start_offset + column_to_byte_offset(start_text, start.column, encoding, true)?;
        let end = end_offset + column_to_byte_offset(end_text, end.column, encoding, true)?;
        (start <= end).then_some(start..end)
    }
}

/// The units used to count columns in a source position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourcePositionEncoding {
    /// UTF-8 code units (bytes).
    Utf8,
    /// UTF-16 code units.
    Utf16,
    /// UTF-32 code units (Unicode scalar values).
    Utf32,
}

impl SourcePositionEncoding {
    /// Counts the code units of a UTF-8 string in this encoding.
    pub fn code_unit_count(self, text: &str) -> usize {
        match self {
            Self::Utf8 => text.len(),
            Self::Utf16 => text.encode_utf16().count(),
            Self::Utf32 => text.chars().count(),
        }
    }
}

impl SourceDocument {
    /// Counts lines including the empty line after a trailing line break.
    pub fn line_count(&self) -> usize {
        self.line_starts().map_or(0, <[usize]>::len)
    }

    /// Returns a zero-based line without its LF, CRLF or CR terminator.
    pub fn line_text(&self, line: usize) -> Option<&str> {
        self.line_at(line).map(|(_, text)| text)
    }

    /// Maps a UTF-8 byte offset to a source position in Unicode scalar values.
    /// Either byte in a CRLF terminator maps to the end of the preceding line.
    pub fn position(&self, offset: usize) -> Option<SourcePosition> {
        self.position_with_encoding(offset, SourcePositionEncoding::Utf32)
    }

    /// Maps a UTF-8 byte offset to a position in the given encoding.
    /// The offset must be a character boundary. Either byte in a CRLF
    /// terminator maps to the end of the preceding line.
    pub fn position_with_encoding(
        &self,
        offset: usize,
        encoding: SourcePositionEncoding,
    ) -> Option<SourcePosition> {
        if !self.is_char_boundary(offset) {
            return None;
        }
        let starts = self.line_starts()?;
        let line = starts.partition_point(|&start| start <= offset) - 1;
        let start = starts[line];
        let text = line_text(self.text()?, start, starts.get(line + 1).copied());
        let column = encoding.code_unit_count(&text[..(offset - start).min(text.len())]);
        Some(SourcePosition { line, column })
    }

    /// Maps a source position in Unicode scalar values to a UTF-8 byte offset.
    /// A line-end position maps to the start of its line terminator, or to EOF.
    pub fn byte_offset(&self, position: SourcePosition) -> Option<usize> {
        self.byte_offset_with_encoding(position, SourcePositionEncoding::Utf32)
    }

    /// Maps a position in the given encoding to a UTF-8 byte offset.
    /// The column must be at a character boundary within the line. A line-end
    /// position maps to the start of its line terminator, or to EOF.
    pub fn byte_offset_with_encoding(
        &self,
        position: SourcePosition,
        encoding: SourcePositionEncoding,
    ) -> Option<usize> {
        self.position_to_byte_offset(position, encoding, false)
    }

    /// Like [`Self::byte_offset_with_encoding`], with columns past the end of
    /// a line clamped to its end. Positions within a character remain invalid.
    pub fn clamped_byte_offset_with_encoding(
        &self,
        position: SourcePosition,
        encoding: SourcePositionEncoding,
    ) -> Option<usize> {
        self.position_to_byte_offset(position, encoding, true)
    }

    fn position_to_byte_offset(
        &self,
        position: SourcePosition,
        encoding: SourcePositionEncoding,
        clamp: bool,
    ) -> Option<usize> {
        let SourcePosition { line, column } = position;
        let (start, text) = self.line_at(line)?;
        let column_offset = column_to_byte_offset(text, column, encoding, clamp)?;
        Some(start + column_offset)
    }

    fn line_at(&self, line: usize) -> Option<(usize, &str)> {
        let starts = self.line_starts()?;
        let &start = starts.get(line)?;
        Some((
            start,
            line_text(self.text()?, start, starts.get(line + 1).copied()),
        ))
    }
}

pub(crate) fn line_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(line_break_ends(text.as_bytes(), 0..text.len()))
}

pub(crate) fn line_starts_after_edit(
    before: &Arc<[usize]>,
    text: &str,
    replaced: Range<usize>,
    inserted: usize,
) -> Arc<[usize]> {
    // An unchanged CR just before the edit may gain or lose a following LF.
    // Line breaks starting in the unchanged suffix keep their classification.
    let scan_start = replaced.start.saturating_sub(1);
    let scan_end = replaced.start + inserted;
    let prefix = before.partition_point(|&offset| offset <= scan_start);
    let suffix = before.partition_point(|&offset| offset <= replaced.end);
    let scan = || line_break_ends(text.as_bytes(), scan_start..scan_end);
    if replaced.len() == inserted && scan().eq(before[prefix..suffix].iter().copied()) {
        return Arc::clone(before);
    }

    let mut starts = Vec::with_capacity(before.len());
    starts.extend_from_slice(&before[..prefix]);
    starts.extend(scan());
    starts.extend(
        before[suffix..]
            .iter()
            .map(|&offset| offset - replaced.end + scan_end),
    );
    starts.into()
}

fn line_break_ends(bytes: &[u8], range: Range<usize>) -> impl Iterator<Item = usize> + '_ {
    let start = range.start;
    memchr::memchr2_iter(b'\r', b'\n', &bytes[range]).filter_map(move |relative| {
        let offset = start + relative;
        (bytes[offset] == b'\n' || bytes.get(offset + 1) != Some(&b'\n')).then_some(offset + 1)
    })
}

fn line_text(text: &str, start: usize, next: Option<usize>) -> &str {
    text[start..next.unwrap_or(text.len())].trim_end_matches(['\r', '\n'])
}

fn column_to_byte_offset(
    text: &str,
    mut column: usize,
    encoding: SourcePositionEncoding,
    clamp: bool,
) -> Option<usize> {
    match encoding {
        SourcePositionEncoding::Utf8 => {
            if clamp {
                column = column.min(text.len());
            }
            text.is_char_boundary(column).then_some(column)
        }
        SourcePositionEncoding::Utf16 => utf16_byte_offset(text, column, clamp),
        SourcePositionEncoding::Utf32 => text
            .char_indices()
            .map(|(offset, _)| offset)
            .chain(std::iter::once(text.len()))
            .nth(column)
            .or_else(|| clamp.then_some(text.len())),
    }
}

fn utf16_byte_offset(text: &str, mut units: usize, clamp: bool) -> Option<usize> {
    for (offset, character) in text.char_indices() {
        if units == 0 {
            return Some(offset);
        }
        units = units.checked_sub(character.len_utf16())?;
    }
    (units == 0 || clamp).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_columns_and_crlf_use_the_retained_snapshot() {
        let source = SourceDocument::input("雪😀e\u{301}\t\r\n犬\r終\n");
        assert_eq!(source.line_count(), 4);
        assert_eq!(source.line_text(0), Some("雪😀e\u{301}\t"));
        assert_eq!(source.line_text(1), Some("犬"));
        assert_eq!(source.line_text(2), Some("終"));
        assert_eq!(source.line_text(3), Some(""));
        let position = SourcePosition { line: 0, column: 3 };
        assert_eq!(
            source.byte_offset_with_encoding(position, SourcePositionEncoding::Utf16),
            Some(7)
        );
        assert_eq!(
            source.byte_offset_with_encoding(
                SourcePosition { line: 0, column: 2 },
                SourcePositionEncoding::Utf16
            ),
            None
        );
        assert_eq!(
            source.position_with_encoding(7, SourcePositionEncoding::Utf8),
            Some(SourcePosition { line: 0, column: 7 })
        );
        assert_eq!(
            source.position_with_encoding(7, SourcePositionEncoding::Utf16),
            Some(position)
        );
        assert_eq!(
            source.position(7),
            Some(SourcePosition { line: 0, column: 2 })
        );
        assert_eq!(source.position(11), source.position(12));
        assert_eq!(
            source.position(13),
            Some(SourcePosition { line: 1, column: 0 })
        );
        assert_eq!(source.position(4), None);
        assert_eq!(
            source.byte_offset_with_encoding(
                SourcePosition { line: 0, column: 2 },
                SourcePositionEncoding::Utf8
            ),
            None
        );
        assert_eq!(
            source.clamped_byte_offset_with_encoding(
                SourcePosition {
                    line: 0,
                    column: 999
                },
                SourcePositionEncoding::Utf16
            ),
            Some(11)
        );
        let changed = source.edited(0..3, "a").unwrap();
        assert_eq!(
            changed.position(5),
            Some(SourcePosition { line: 0, column: 2 })
        );
        assert_eq!(
            source.position(7),
            Some(SourcePosition { line: 0, column: 2 })
        );
    }

    #[test]
    fn large_edited_source_positions_match_a_fresh_index() {
        let mut flat = "雪😀\u{2028}x\r\n犬\r終\n".repeat(1200);
        let old = SourceDocument::input(&flat);
        assert_eq!(old.line_count(), 3601);
        let edit = flat.find('\r').unwrap()..flat.find('\r').unwrap() + 1;
        flat.replace_range(edit.clone(), "\nα");
        let edited = old.edited(edit, "\nα").unwrap();
        let reference = SourceDocument::input(&flat);
        assert_eq!(edited.line_count(), reference.line_count());
        for encoding in [
            SourcePositionEncoding::Utf8,
            SourcePositionEncoding::Utf16,
            SourcePositionEncoding::Utf32,
        ] {
            for offset in 0..=flat.len() {
                let expected = reference.position_with_encoding(offset, encoding);
                assert_eq!(edited.position_with_encoding(offset, encoding), expected);
                if let Some(position) = expected {
                    assert_eq!(
                        edited.byte_offset_with_encoding(position, encoding),
                        reference.byte_offset_with_encoding(position, encoding)
                    );
                }
            }
        }
        assert_eq!(edited.line_starts(), reference.line_starts());
        assert_eq!(old.text().unwrap().lines().next(), Some("雪😀\u{2028}x"));
    }

    #[test]
    fn edited_line_starts_match_a_fresh_index() {
        for original in ["", "a\r\nb\rc\n", "\r\r\n\n", "😀雪\r\n犬"] {
            let before = line_starts(original).collect::<Arc<[_]>>();
            let boundaries: Vec<_> = original
                .char_indices()
                .map(|(offset, _)| offset)
                .chain([original.len()])
                .collect();
            for (index, &start) in boundaries.iter().enumerate() {
                for &end in &boundaries[index..] {
                    for replacement in ["", "b", "雪", "\r", "\n", "\r\n", "\n\r"] {
                        let mut edited = original.to_owned();
                        edited.replace_range(start..end, replacement);
                        let updated =
                            line_starts_after_edit(&before, &edited, start..end, replacement.len());
                        let expected = line_starts(&edited).collect::<Vec<_>>();
                        assert_eq!(
                            &*updated, expected,
                            "{original:?} {start}..{end} {replacement:?}"
                        );
                        if end - start == replacement.len() && *before == expected {
                            assert!(Arc::ptr_eq(&before, &updated));
                        }
                    }
                }
            }
        }
    }
}
