use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;

use crate::{SnapshotId, SourceDocument, SourceSpan, SpanError};

/// Origin of a source range after reader, macro, or compiler transformation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceOrigin {
    Original,
    Reader,
    Macro,
    Compiler,
}

/// Source expansion history retained alongside a diagnostic label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProvenance {
    pub origin: SourceOrigin,
    pub expansion: Vec<SourceSpan>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Note,
    Help,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelRole {
    Primary,
    Secondary,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    pub span: SourceSpan,
    pub role: LabelRole,
    pub message: String,
    pub provenance: Option<SourceProvenance>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    Note,
    Help,
}

/// Presentation semantics understood by editors as well as terminal adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticTag {
    Unnecessary,
    Deprecated,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub kind: NoteKind,
    pub message: String,
}

/// A contextual operation and its optional resource. Paths stay structured so
/// terminals can choose display paths without changing machine identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticContext {
    pub message: String,
    pub path: Option<PathBuf>,
}

/// A report owns the immutable text needed to explain each source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: Option<String>,
    pub severity: Severity,
    pub message: String,
    pub tags: Vec<DiagnosticTag>,
    /// Rule reasoning and observed evidence retain their machine-readable roles.
    pub rationale: Option<String>,
    pub evidence: Option<String>,
    pub labels: Vec<Label>,
    pub notes: Vec<Note>,
    pub contexts: Vec<DiagnosticContext>,
    pub suggestions: Vec<Suggestion>,
    pub sources: Vec<SourceDocument>,
}
impl Diagnostic {
    pub fn new(severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code: None,
            severity,
            message: message.into(),
            tags: Vec::new(),
            rationale: None,
            evidence: None,
            labels: Vec::new(),
            notes: Vec::new(),
            contexts: Vec::new(),
            suggestions: Vec::new(),
            sources: Vec::new(),
        }
    }
    pub fn source(&self, id: SnapshotId) -> Option<&SourceDocument> {
        self.sources
            .iter()
            .find(|source| source.snapshot_id() == id)
    }
    pub fn add_source(&mut self, source: SourceDocument) {
        if self.source(source.snapshot_id()).is_none() {
            self.sources.push(source);
        }
    }
    pub fn add_label(
        &mut self,
        source: &SourceDocument,
        range: Range<usize>,
        role: LabelRole,
        message: impl Into<String>,
    ) -> Result<(), SpanError> {
        let span = source.span(range)?;
        self.add_source(source.clone());
        self.labels.push(Label {
            span,
            role,
            message: message.into(),
            provenance: None,
        });
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applicability {
    MachineApplicable,
    MaybeIncorrect,
    HasPlaceholders,
    Unspecified,
}

/// One atomic edit group. Separate suggestions are alternatives, not edits to
/// apply together. Callers select alternatives before [`apply_suggestions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub message: String,
    pub applicability: Applicability,
    edits: Vec<SourceEdit>,
}
impl Suggestion {
    pub fn new(
        message: impl Into<String>,
        applicability: Applicability,
        edits: Vec<SourceEdit>,
    ) -> Result<Self, EditError> {
        validate_conflicts(edits.iter())?;
        Ok(Self {
            message: message.into(),
            applicability,
            edits,
        })
    }

    pub fn edits(&self) -> &[SourceEdit] {
        &self.edits
    }
}

/// A validated change with the exact snapshot and original text it replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEdit {
    source: SourceDocument,
    span: SourceSpan,
    replacement: String,
}
impl SourceEdit {
    pub fn new(
        source: SourceDocument,
        range: Range<usize>,
        replacement: impl Into<String>,
    ) -> Result<Self, EditError> {
        let span = source.span(range)?;
        if source.text().is_none() {
            return Err(SpanError::MissingText.into());
        }
        Ok(Self {
            source,
            span,
            replacement: replacement.into(),
        })
    }
    pub fn source(&self) -> &SourceDocument {
        &self.source
    }
    pub fn span(&self) -> SourceSpan {
        self.span
    }
    pub fn expected(&self) -> &str {
        &self.source.text().expect("an edit retains its source text")[self.span.range()]
    }
    pub fn replacement(&self) -> &str {
        &self.replacement
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditError {
    Span(SpanError),
    Conflict,
}
impl From<SpanError> for EditError {
    fn from(value: SpanError) -> Self {
        Self::Span(value)
    }
}
impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Span(error) => error.fmt(f),
            Self::Conflict => {
                f.write_str("selected source edits overlap or have ambiguous insertion order")
            }
        }
    }
}
impl std::error::Error for EditError {}

type EditGroups<'a> = BTreeMap<SnapshotId, Vec<&'a SourceEdit>>;

fn grouped_edits<'a>(
    edits: impl IntoIterator<Item = &'a SourceEdit>,
) -> Result<EditGroups<'a>, EditError> {
    let mut grouped = EditGroups::new();
    for edit in edits {
        grouped.entry(edit.span.snapshot()).or_default().push(edit);
    }
    for edits in grouped.values_mut() {
        edits.sort_by_key(|edit| (edit.span.range().start, edit.span.range().end));
        for pair in edits.windows(2) {
            let a = pair[0].span.range();
            let b = pair[1].span.range();
            if a.end > b.start || a.start == b.start {
                return Err(EditError::Conflict);
            }
        }
    }
    Ok(grouped)
}

fn validate_conflicts<'a>(
    edits: impl IntoIterator<Item = &'a SourceEdit>,
) -> Result<(), EditError> {
    grouped_edits(edits).map(|_| ())
}

/// Select whole, non-overlapping groups in candidate order.
/// Callers choose the eligible applicability before passing candidates.
pub fn select_suggestions<'a>(
    candidates: impl IntoIterator<Item = &'a Suggestion>,
) -> Vec<&'a Suggestion> {
    let mut occupied = BTreeMap::<SnapshotId, BTreeMap<usize, usize>>::new();
    let mut selected = Vec::new();
    for candidate in candidates {
        let conflict = candidate.edits.iter().any(|edit| {
            let Some(ranges) = occupied.get(&edit.span.snapshot()) else {
                return false;
            };
            let range = edit.span.range();
            ranges
                .range(..=range.start)
                .next_back()
                .is_some_and(|(&start, &end)| start == range.start || end > range.start)
                || ranges
                    .range(range.start..)
                    .next()
                    .is_some_and(|(&start, _)| start < range.end || start == range.start)
        });
        if conflict {
            continue;
        }
        for edit in &candidate.edits {
            let range = edit.span.range();
            occupied
                .entry(edit.span.snapshot())
                .or_default()
                .insert(range.start, range.end);
        }
        selected.push(candidate);
    }
    selected
}

/// Apply a set of non-overlapping edits to the source each edit carries.
pub fn apply_suggestions<'a>(
    suggestions: impl IntoIterator<Item = &'a Suggestion>,
) -> Result<Vec<(SourceDocument, String)>, EditError> {
    let grouped = grouped_edits(suggestions.into_iter().flat_map(Suggestion::edits))?;
    let mut result = Vec::with_capacity(grouped.len());
    for edits in grouped.values() {
        let source = edits[0].source();
        let mut text = source
            .text()
            .expect("an edit retains its source text")
            .to_owned();
        for edit in edits.iter().rev() {
            text.replace_range(edit.span.range(), &edit.replacement);
        }
        result.push((source.clone(), text));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn suggestion(source: &SourceDocument, range: Range<usize>, value: &str) -> Suggestion {
        Suggestion::new(
            "replace",
            Applicability::MachineApplicable,
            vec![SourceEdit::new(source.clone(), range, value).unwrap()],
        )
        .unwrap()
    }
    #[test]
    fn applies_one_group_across_its_source_documents() {
        let first = SourceDocument::input("雪abcdef");
        let second = SourceDocument::input("xyz");
        let group = Suggestion::new(
            "two files",
            Applicability::MachineApplicable,
            vec![
                SourceEdit::new(first.clone(), 0..3, "犬").unwrap(),
                SourceEdit::new(first.clone(), 5..7, "!").unwrap(),
                SourceEdit::new(second.clone(), 1..2, "😀").unwrap(),
            ],
        )
        .unwrap();
        let applied = apply_suggestions([&group]).unwrap();
        assert_eq!(applied[0].1, "犬ab!ef");
        assert_eq!(applied[1].1, "x😀z");
        assert_eq!(first.text(), Some("雪abcdef"));
    }
    #[test]
    fn invalid_boundaries_and_conflicting_edits_are_rejected() {
        let source = SourceDocument::input("雪abc");
        assert!(SourceEdit::new(source.clone(), 1..3, "x").is_err());
        let a = suggestion(&source, 3..5, "x");
        let b = suggestion(&source, 4..6, "y");
        assert_eq!(apply_suggestions([&a, &b]), Err(EditError::Conflict));
        let a = suggestion(&source, 3..3, "x");
        let b = suggestion(&source, 3..3, "y");
        assert_eq!(apply_suggestions([&a, &b]), Err(EditError::Conflict));
        let left = suggestion(&source, 0..3, "犬");
        let right = suggestion(&source, 3..4, "b");
        assert_eq!(apply_suggestions([&left, &right]).unwrap()[0].1, "犬bbc");
    }

    #[test]
    fn selection_keeps_groups_atomic_and_agrees_with_application() {
        let source = SourceDocument::input("abcdef");
        let other = SourceDocument::input("xyz");
        let first = suggestion(&source, 1..3, "!");
        let rejected = Suggestion::new(
            "atomic conflicting group",
            Applicability::MachineApplicable,
            vec![
                SourceEdit::new(source.clone(), 2..4, "?").unwrap(),
                SourceEdit::new(other.clone(), 0..1, "!").unwrap(),
            ],
        )
        .unwrap();
        let independent = suggestion(&other, 0..1, "雪");
        let adjacent = suggestion(&source, 3..4, "😀");
        let duplicate_insertion = suggestion(&source, 3..3, "+");
        let candidates = [first, rejected, independent, adjacent, duplicate_insertion];
        let selected = select_suggestions(&candidates);
        assert_eq!(selected, [&candidates[0], &candidates[2], &candidates[3]]);
        let applied = apply_suggestions(selected).unwrap();
        assert_eq!(applied[0].1, "a!😀ef");
        assert_eq!(applied[1].1, "雪yz");
        assert_eq!(
            apply_suggestions(&candidates[..2]),
            Err(EditError::Conflict)
        );
    }

    #[test]
    fn interval_selection_matches_atomic_greedy_validation() {
        let source = SourceDocument::input("abcdef");
        let mut candidates = Vec::new();
        // Include replacements, empty insertions, containment and every boundary.
        for start in 0..=6 {
            for end in start..=6 {
                candidates.push(suggestion(&source, start..end, "!"));
            }
        }
        for candidates in [candidates.clone(), candidates.into_iter().rev().collect()] {
            let mut expected = Vec::new();
            for candidate in &candidates {
                let mut attempt = expected.clone();
                attempt.push(candidate.clone());
                if apply_suggestions(&attempt).is_ok() {
                    expected = attempt;
                }
            }
            let selected = select_suggestions(&candidates);
            assert_eq!(selected.into_iter().cloned().collect::<Vec<_>>(), expected);
        }
    }
}
