use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::position::{line_starts, line_starts_after_edit};

static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

/// Process-local immutable snapshot identity; never serialized as file identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotId(u64);

impl SnapshotId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// An ordered, half-open UTF-8 byte range. Text-boundary validation is performed
/// by [`SourceDocument::span`], since it requires the corresponding snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteRange {
    start: usize,
    end: usize,
}

impl ByteRange {
    pub fn new(range: Range<usize>) -> Result<Self, SpanError> {
        if range.start > range.end {
            return Err(SpanError::InvalidRange);
        }
        Ok(Self {
            start: range.start,
            end: range.end,
        })
    }
    pub fn start(self) -> usize {
        self.start
    }
    pub fn end(self) -> usize {
        self.end
    }
    pub fn as_range(self) -> Range<usize> {
        self.start..self.end
    }
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// A range bound to exactly one immutable source snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    snapshot: SnapshotId,
    range: ByteRange,
}

impl SourceSpan {
    pub fn snapshot(self) -> SnapshotId {
        self.snapshot
    }
    pub fn range(self) -> Range<usize> {
        self.range.as_range()
    }
    pub fn byte_range(self) -> ByteRange {
        self.range
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanError {
    InvalidRange,
    WrongSnapshot,
    MissingText,
}

impl fmt::Display for SpanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRange => {
                "source range is outside the snapshot or splits a UTF-8 character"
            }
            Self::WrongSnapshot => "source range belongs to a different snapshot",
            Self::MissingText => "source text is unavailable",
        })
    }
}
impl std::error::Error for SpanError {}

/// Retained UTF-8 source and host metadata for one revision.
///
/// The host owns its editable buffer. It can supply a snapshot as text or
/// derive another immutable snapshot from a byte-range edit.
#[derive(Clone)]
pub struct SourceDocument(Arc<Document>);

#[derive(Clone)]
struct Document {
    id: SnapshotId,
    path: Option<PathBuf>,
    text: Option<Arc<String>>,
    uri: Option<String>,
    version: Option<i64>,
    base_offset: usize,
    line_starts: OnceLock<Arc<[usize]>>,
    edit: Option<EditOrigin>,
}

#[derive(Clone)]
struct EditOrigin {
    before: SnapshotId,
    replaced: Range<usize>,
    preserves_suffix_positions: bool,
}

fn fresh_id() -> SnapshotId {
    let id = NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
    assert!(id != u64::MAX, "source snapshot identity exhausted");
    SnapshotId(id)
}

impl SourceDocument {
    pub fn input(text: &str) -> Self {
        Self::new(None, Some(text.to_owned()))
    }
    pub fn file(path: PathBuf, text: Option<String>) -> Self {
        Self::new(Some(path), text)
    }
    pub fn new(path: Option<PathBuf>, text: Option<String>) -> Self {
        Self(Arc::new(Document {
            id: fresh_id(),
            path,
            text: text.map(Arc::new),
            uri: None,
            version: None,
            base_offset: 0,
            line_starts: OnceLock::new(),
            edit: None,
        }))
    }

    fn metadata_mut(&mut self) -> &mut Document {
        // A retained snapshot must not silently acquire another host version.
        // Assigning fresh identity also avoids equal IDs for diverged metadata.
        let document = Arc::make_mut(&mut self.0);
        document.id = fresh_id();
        document
    }
    pub fn with_uri(mut self, uri: impl Into<String>) -> Self {
        self.metadata_mut().uri = Some(uri.into());
        self
    }
    pub fn with_version(mut self, version: i64) -> Self {
        self.metadata_mut().version = Some(version);
        self
    }
    /// Snapshot offsets remain relative to its text; this records its host offset.
    pub fn with_base_offset(mut self, offset: usize) -> Self {
        self.metadata_mut().base_offset = offset;
        self
    }
    pub fn uri(&self) -> Option<&str> {
        self.0.uri.as_deref()
    }
    pub fn version(&self) -> Option<i64> {
        self.0.version
    }
    pub fn base_offset(&self) -> usize {
        self.0.base_offset
    }
    pub fn id(&self) -> u64 {
        self.0.id.get()
    }
    pub fn snapshot_id(&self) -> SnapshotId {
        self.0.id
    }
    pub fn path(&self) -> Option<&Path> {
        self.0.path.as_deref()
    }

    /// Borrow the text retained by this snapshot.
    pub fn text(&self) -> Option<&str> {
        self.0.text.as_deref().map(String::as_str)
    }

    pub fn byte_len(&self) -> Option<usize> {
        self.text().map(str::len)
    }

    pub fn is_char_boundary(&self, offset: usize) -> bool {
        self.text()
            .is_some_and(|text| text.is_char_boundary(offset))
    }

    /// Borrow a half-open UTF-8 byte range within the snapshot.
    pub fn text_slice(&self, range: Range<usize>) -> Option<&str> {
        self.text()?.get(range)
    }

    /// Whether the same byte offset is known to preserve its line and all
    /// column encodings across a direct edit. An unknown answer is false.
    pub fn preserves_position_from(&self, before: &Self, offset: usize) -> bool {
        self == before
            || self.0.edit.as_ref().is_some_and(|edit| {
                edit.before == before.snapshot_id()
                    && (offset < edit.replaced.start
                        || (edit.preserves_suffix_positions && offset >= edit.replaced.end))
            })
    }

    /// Missing text permits a known ordered range, but never a fabricated snippet.
    pub fn span(&self, range: Range<usize>) -> Result<SourceSpan, SpanError> {
        let range = ByteRange::new(range)?;
        if self.0.text.is_some()
            && ![range.start(), range.end()]
                .into_iter()
                .all(|at| self.is_char_boundary(at))
        {
            return Err(SpanError::InvalidRange);
        }
        Ok(SourceSpan {
            snapshot: self.snapshot_id(),
            range,
        })
    }

    pub fn text_at(&self, span: SourceSpan) -> Result<&str, SpanError> {
        if span.snapshot != self.snapshot_id() {
            return Err(SpanError::WrongSnapshot);
        }
        self.text()
            .ok_or(SpanError::MissingText)?
            .get(span.range())
            .ok_or(SpanError::InvalidRange)
    }

    pub fn absolute_range(&self, range: Range<usize>) -> Option<Range<usize>> {
        self.span(range.clone()).ok()?;
        Some(
            self.base_offset().checked_add(range.start)?
                ..self.base_offset().checked_add(range.end)?,
        )
    }

    /// Produce a new snapshot while reusing an already-built line index outside
    /// the edit. Byte-identical replacements preserve the current identity.
    pub fn edited(&self, range: Range<usize>, replacement: &str) -> Result<Self, SpanError> {
        let text = self.text().ok_or(SpanError::MissingText)?;
        let before = text.get(range.clone()).ok_or(SpanError::InvalidRange)?;
        if before == replacement {
            return Ok(self.clone());
        }
        let preserves_suffix_positions = before.len() == replacement.len()
            && replacement.len() <= 256
            && !replacement.contains(['\r', '\n'])
            && !before.contains(['\r', '\n'])
            && before.chars().count() == replacement.chars().count()
            && before.encode_utf16().count() == replacement.encode_utf16().count();
        let mut edited = String::with_capacity(text.len() - before.len() + replacement.len());
        edited.push_str(&text[..range.start]);
        edited.push_str(replacement);
        edited.push_str(&text[range.end..]);
        let lines = self.0.line_starts.get().map(|before| {
            line_starts_after_edit(before, &edited, range.clone(), replacement.len())
        });
        let mut document = self.0.as_ref().clone();
        document.id = fresh_id();
        document.text = Some(Arc::new(edited));
        document.line_starts = lines.map_or_else(OnceLock::new, OnceLock::from);
        document.edit = Some(EditOrigin {
            before: self.snapshot_id(),
            replaced: range,
            preserves_suffix_positions,
        });
        // An edit creates an unpublished revision. The host assigns its version.
        document.version = None;
        Ok(Self(Arc::new(document)))
    }

    pub fn line_starts(&self) -> Option<&[usize]> {
        let text = self.text()?;
        Some(
            self.0
                .line_starts
                .get_or_init(|| line_starts(text).collect()),
        )
    }
}

impl PartialEq for SourceDocument {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot_id() == other.snapshot_id()
    }
}
impl Eq for SourceDocument {}
impl fmt::Debug for SourceDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceDocument")
            .field("id", &self.id())
            .field("path", &self.path())
            .field("uri", &self.uri())
            .field("version", &self.version())
            .field("base_offset", &self.base_offset())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_reuse_requires_equal_columns_in_all_encodings() {
        let before = SourceDocument::input("雪😀z\r\nnext");
        let same = before.edited(3..7, "🦎").unwrap();
        assert!(same.preserves_position_from(&before, 7));
        assert!(same.preserves_position_from(&before, 0));
        assert!(!same.preserves_position_from(&before, 3));
        let changed = before.edited(3..7, "abcd").unwrap();
        assert!(!changed.preserves_position_from(&before, 7));
        let new_line = before.edited(7..8, "\n").unwrap();
        assert!(!new_line.preserves_position_from(&before, 8));
        assert!(!new_line.preserves_position_from(&same, 0));
    }

    #[test]
    fn revisions_keep_identity_and_previously_retained_text() {
        let old = SourceDocument::input("雪😀\r\n").with_version(4);
        let span = old.span(0..3).unwrap();
        assert!(old.span(1..3).is_err());
        assert!(old.span(3..1).is_err());
        assert_eq!(old.clone(), old);
        let new = old.edited(0..3, "犬").unwrap();
        assert_ne!(old, new);
        assert_eq!(old.text_at(span), Ok("雪"));
        assert_eq!(new.text_at(span), Err(SpanError::WrongSnapshot));
        assert_eq!(old.version(), Some(4));
        assert_eq!(new.version(), None);
        assert_eq!(old.edited(0..3, "雪").unwrap(), old);
        let host_revision = old.clone().with_version(5);
        assert_ne!(host_revision.snapshot_id(), old.snapshot_id());
        assert_eq!(old.version(), Some(4));
    }
    #[test]
    fn unavailable_text_and_host_offsets_are_explicit() {
        let source = SourceDocument::new(None, None).with_base_offset(10);
        assert_eq!(source.absolute_range(2..4), Some(12..14));
        assert_eq!(
            source.text_at(source.span(2..4).unwrap()),
            Err(SpanError::MissingText)
        );
        assert_eq!(
            source
                .clone()
                .with_base_offset(usize::MAX)
                .absolute_range(1..2),
            None
        );
    }
}
