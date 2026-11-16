//! Incremental syntax analysis for named source buffers.
//!
//! Each open document has its own Salsa input. Single-range edits can reuse
//! unchanged syntax, and other documents keep their cached snapshots.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use salsa::{Database, DatabaseImpl, Setter};
use zkcl_diagnostics::{SourceDocument, SourcePosition, SourcePositionEncoding};
use zkcl_target::CompilationTarget;

use crate::syntax::{
    ParsedSource, SourceReadtable, SyntaxDiagnostic, SyntaxKind, TextRange, TextSize,
    parse_source_with_readtable,
};
use crate::{OutlineEntry, TextEdit, format, outline};
use crate::{documentation::DocumentationTests, navigation};

mod analysis;
mod lint_dependencies;
mod position;

pub use analysis::{DiagnosticSnapshot, ExpressionFeedback};
pub use lint_dependencies::WorkspaceLintChanges;

#[salsa::input]
struct Document {
    // Vacant Salsa slots hold no syntax. Every entry in `SourceWorkspace::documents` is live.
    syntax: Option<Arc<DocumentSyntax>>,
    initial_package: Option<Arc<String>>,
    features: Option<Arc<[String]>>,
}

impl Document {
    fn live_syntax(self, db: &dyn Database) -> &DocumentSyntax {
        self.syntax(db).as_deref().expect("live source document")
    }
}

#[salsa::tracked(returns(clone), no_eq)]
fn document_snapshot(db: &dyn Database, document: Document) -> Option<Arc<SourceSnapshot>> {
    Some(SourceSnapshot::new(
        Arc::clone(document.syntax(db).as_ref()?),
        document.initial_package(db).clone(),
        document.features(db).clone(),
    ))
}

#[derive(Debug)]
struct DocumentSyntax {
    source: SourceDocument,
    initial_readtable: SourceReadtable,
    edit: Mutex<Option<PendingReparse>>,
    parsed: OnceLock<Arc<ParsedSource>>,
    diagnostics: OnceLock<Arc<[SyntaxDiagnostic]>>,
}

#[derive(Debug)]
struct PendingReparse {
    before: Arc<ParsedSource>,
    replaced: TextRange,
    inserted: usize,
}

impl DocumentSyntax {
    fn new(source: SourceDocument, initial_readtable: SourceReadtable) -> Self {
        debug_assert!(source.byte_len().is_some());
        Self {
            source,
            initial_readtable,
            edit: Mutex::new(None),
            parsed: OnceLock::new(),
            diagnostics: OnceLock::new(),
        }
    }

    fn text(&self) -> &str {
        self.source.text().expect("syntax source always has text")
    }

    fn with_text(&self, text: String) -> Self {
        let (replaced, replacement) = changed_ranges(self.text(), &text);
        self.edited(replaced, &text[replacement])
            .expect("validated edit")
            .expect("different text")
    }

    fn edited(
        &self,
        range: Range<usize>,
        replacement: &str,
    ) -> Result<Option<Self>, SourceEditError> {
        let source = self
            .source
            .edited(range.clone(), replacement)
            .map_err(|_| SourceEditError::InvalidRange)?;
        if source == self.source {
            return Ok(None);
        }
        let edit = self.pending_reparse(range, replacement.len());
        Ok(Some(Self {
            edit: Mutex::new(edit),
            ..Self::new(source, self.initial_readtable)
        }))
    }

    fn parsed(&self) -> &Arc<ParsedSource> {
        self.parsed.get_or_init(|| {
            let edit = self.edit.lock().unwrap().take();
            let parsed = edit.and_then(|edit| {
                edit.before
                    .reparse(&self.source, edit.replaced, edit.inserted)
            });
            Arc::new(parsed.unwrap_or_else(|| {
                parse_source_with_readtable(self.text(), self.initial_readtable)
            }))
        })
    }

    fn diagnostics(&self) -> &Arc<[SyntaxDiagnostic]> {
        self.diagnostics
            .get_or_init(|| Arc::from(self.parsed().diagnostics()))
    }

    fn line_starts(&self) -> &[usize] {
        self.source
            .line_starts()
            .expect("syntax source always has text")
    }

    fn pending_reparse(&self, range: Range<usize>, inserted: usize) -> Option<PendingReparse> {
        let (before, range, inserted) = if let Some(parsed) = self.parsed.get() {
            (Arc::clone(parsed), range, inserted)
        } else {
            let edit = self.edit.lock().unwrap();
            let previous = edit.as_ref()?;
            let previous_range =
                usize::from(previous.replaced.start())..usize::from(previous.replaced.end());
            let (range, width) = combine_edits(previous_range, previous.inserted, range, inserted);
            (Arc::clone(&previous.before), range, width)
        };
        Some(PendingReparse {
            before,
            replaced: TextRange::new(
                TextSize::try_from(range.start).ok()?,
                TextSize::try_from(range.end).ok()?,
            ),
            inserted,
        })
    }
}

/// Describe two successive edits as one replacement against the original text.
/// The second range uses coordinates after the first replacement.
fn combine_edits(
    before: Range<usize>,
    before_inserted: usize,
    after: Range<usize>,
    after_inserted: usize,
) -> (Range<usize>, usize) {
    let intermediate_end = before.start + before_inserted;
    let start = before.start.min(after.start);
    let end = before.end + after.end.saturating_sub(intermediate_end);
    let width = after.end.max(intermediate_end) - start - after.len() + after_inserted;
    (start..end, width)
}

/// Find the unchanged edges of a replacement. Both ranges follow UTF-8 scalar
/// boundaries, and the replaced range never splits an original CRLF terminator.
pub(crate) fn changed_ranges(before: &str, after: &str) -> (Range<usize>, Range<usize>) {
    let old = before.as_bytes();
    let new = after.as_bytes();
    let mut start = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    while !before.is_char_boundary(start) || !after.is_char_boundary(start) {
        start -= 1;
    }
    if start > 0 && old.get(start - 1..start + 1) == Some(b"\r\n") {
        start -= 1;
    }

    let mut suffix = old[start..]
        .iter()
        .rev()
        .zip(new[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    while !before.is_char_boundary(old.len() - suffix)
        || !after.is_char_boundary(new.len() - suffix)
    {
        suffix -= 1;
    }
    let end = old.len() - suffix;
    if end > 0 && old.get(end - 1..end + 1) == Some(b"\r\n") {
        suffix -= 1;
    }
    (start..old.len() - suffix, start..new.len() - suffix)
}

/// Text, syntax and locations from a single version of a document.
///
/// A snapshot remains usable after the workspace edits or removes its document.
#[derive(Debug)]
pub struct SourceSnapshot {
    syntax: Arc<DocumentSyntax>,
    initial_package: Option<Arc<String>>,
    features: Option<Arc<[String]>>,
    outline: OnceLock<Vec<OutlineEntry>>,
    index: OnceLock<navigation::DocumentIndex>,
    documentation_tests: OnceLock<DocumentationTests>,
    lint_spellings: OnceLock<rustc_hash::FxHashSet<String>>,
}

impl SourceSnapshot {
    fn new(
        syntax: Arc<DocumentSyntax>,
        initial_package: Option<Arc<String>>,
        features: Option<Arc<[String]>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            syntax,
            initial_package,
            features,
            outline: OnceLock::new(),
            index: OnceLock::new(),
            documentation_tests: OnceLock::new(),
            lint_spellings: OnceLock::new(),
        })
    }

    /// Take an immutable UTF-8 snapshot of text owned by the caller's editor.
    /// No workspace or document registration is required.
    pub fn from_text(text: impl Into<String>, readtable: SourceReadtable) -> Arc<Self> {
        let source = SourceDocument::new(None, Some(text.into()));
        Self::new(Arc::new(DocumentSyntax::new(source, readtable)), None, None)
    }

    pub(crate) fn editable_reader_syntax_at(&self, offset: TextSize) -> bool {
        self.index().editable_reader_syntax_at(offset)
    }

    /// Shared immutable source, suitable for runtime execution and diagnostics.
    pub fn source(&self) -> &SourceDocument {
        &self.syntax.source
    }

    pub fn text(&self) -> &str {
        self.syntax.text()
    }

    /// Read the original text in a half-open UTF-8 byte range of this snapshot.
    /// Returns `None` if the range is outside the source or splits a character.
    pub fn text_at(&self, range: TextRange) -> Option<&str> {
        self.text()
            .get(usize::from(range.start())..usize::from(range.end()))
    }

    pub fn parsed(&self) -> &ParsedSource {
        self.syntax.parsed()
    }

    pub fn diagnostics(&self) -> &[SyntaxDiagnostic] {
        self.parsed().diagnostics()
    }

    /// Whether the source position can accept paired punctuation. Uses the
    /// current token and readtable from this snapshot.
    pub fn should_auto_pair_at(&self, byte: usize) -> bool {
        if !self.text().is_char_boundary(byte) {
            return false;
        }
        let Ok(offset) = TextSize::try_from(byte) else {
            return false;
        };
        let parsed = self.parsed();
        let root = parsed.root();
        if offset > root.text_range().end() {
            return false;
        }
        let Some(token) = crate::tokens_at_offset(&root, offset).left_biased() else {
            return true;
        };
        let start = token.text_range().start();
        if start == offset {
            return true;
        }
        token.kind() != SyntaxKind::RawString
            && crate::ast::can_pair_after(
                &self.text()[usize::from(start)..byte],
                parsed.readtable_at(start),
            )
    }

    /// Discover declarations under standard reader syntax in source order.
    ///
    /// This outline recognizes standard definition forms and top-level
    /// `PROGN`. It follows reader conditionals with known active branches and
    /// stops at unknown reader boundaries. Entries describe source syntax
    /// without resolving packages or expanding macros.
    pub fn outline(&self) -> &[OutlineEntry] {
        self.outline.get_or_init(|| {
            outline::collect_with_features(&self.parsed().root(), self.features.as_deref())
        })
    }

    /// Format the snapshot using its parsed syntax. Returns `None` if formatting
    /// cannot preserve the source's token boundaries or it has syntax diagnostics.
    pub fn formatted_text(&self) -> Option<String> {
        format::format_parsed(self.text(), self.parsed())
    }

    /// Compute lossless formatting edits for this snapshot.
    pub fn format_edits(&self) -> Vec<TextEdit> {
        format::format_edits(self)
    }

    /// Return the enclosing syntax ranges from innermost to outermost at `offset`.
    pub fn selection_ranges(&self, offset: usize) -> Option<Vec<TextRange>> {
        if !self.text().is_char_boundary(offset) {
            return None;
        }
        let pos = TextSize::try_from(offset).ok()?;
        let root = self.parsed().root();
        if pos > root.text_range().end() {
            return None;
        }
        let mut ranges = Vec::new();
        let mut node = if let Some(token) = crate::ast::token_at_offset(&root, pos) {
            ranges.push(token.text_range());
            token.parent()
        } else {
            crate::tokens_at_offset(&root, pos)
                .left_biased()
                .and_then(|token| token.parent())
        };
        while let Some(parent) = node {
            if ranges.last() != Some(&parent.text_range()) {
                ranges.push(parent.text_range());
            }
            node = parent.parent();
        }
        if ranges.is_empty() {
            ranges.push(root.text_range());
        }
        Some(ranges)
    }

    /// Discover multiline structural folding ranges (`List`, `Vector`, `BlockComment`).
    pub fn folding_ranges(&self) -> Vec<TextRange> {
        let mut ranges = Vec::new();
        let root = self.parsed().root();
        let line_starts = self.syntax.line_starts();
        for element in root.descendants_with_tokens() {
            if !matches!(
                element.kind(),
                SyntaxKind::List
                    | SyntaxKind::Vector
                    | SyntaxKind::BlockComment
                    | SyntaxKind::RawString
            ) {
                continue;
            }
            let range = element.text_range();
            let next_line = line_starts.partition_point(|&at| at <= usize::from(range.start()));
            if line_starts
                .get(next_line)
                .is_some_and(|&at| at <= usize::from(range.end()))
            {
                ranges.push(range);
            }
        }
        ranges
    }
}

/// An edit that could not be applied to a named source buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SourceEditError {
    #[error("unknown source document")]
    UnknownDocument,
    #[error("edit range is outside the source or splits a UTF-8 character")]
    InvalidRange,
    #[error("source snapshot has no text to analyze")]
    MissingText,
}

/// An incremental database of source buffers used for structural analysis.
#[derive(Default)]
pub struct SourceWorkspace {
    database: DatabaseImpl,
    documents: BTreeMap<String, Document>,
    vacant_documents: Vec<Document>,
    visible_sources: BTreeMap<String, Arc<[String]>>,
    features: Option<Arc<[String]>>,
    readtable: SourceReadtable,
}

impl SourceWorkspace {
    /// Creates an empty source workspace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Configure the known `*FEATURES*` list for reader conditional (`#+` / `#-`) evaluation.
    /// Passing `None` leaves reader conditionals in an unknown state.
    /// Returns whether the configured list changed.
    pub fn set_features(&mut self, features: Option<Vec<String>>) -> bool {
        let next: Option<Arc<[String]>> = features.map(|list| {
            list.into_iter()
                .map(|s| s.trim_prefix(':').to_ascii_uppercase())
                .collect::<Vec<_>>()
                .into()
        });
        if self.features == next {
            return false;
        }
        self.features = next.clone();
        for &doc in self.documents.values() {
            doc.set_features(&mut self.database).to(next.clone());
        }
        true
    }

    /// Configure reader conditionals using the initial features of a compilation target.
    /// Use [Self::set_features] for a project with its own feature list.
    /// Passing `None` leaves reader conditionals in an unknown state.
    /// Returns whether the configured list changed.
    pub fn set_target(&mut self, target: Option<&CompilationTarget>) -> bool {
        self.set_features(target.map(|target| target.features().map(str::to_owned).collect()))
    }

    /// Select the initial syntax for documents in the workspace. A top-level
    /// readtable selection in a source file can override this setting.
    pub fn set_readtable(&mut self, readtable: SourceReadtable) -> bool {
        if readtable == self.readtable {
            return false;
        }
        self.readtable = readtable;
        for &document in self.documents.values() {
            set_document_readtable(&mut self.database, document, readtable);
        }
        true
    }

    /// Select the initial syntax for one document, e.g. a REPL buffer.
    pub fn set_initial_readtable(&mut self, name: &str, readtable: SourceReadtable) -> bool {
        self.documents
            .get(name)
            .copied()
            .is_some_and(|document| set_document_readtable(&mut self.database, document, readtable))
    }

    /// Configure the initial package for a document before any top-level `IN-PACKAGE` form.
    pub fn set_initial_package(&mut self, name: &str, package: Option<String>) -> bool {
        let Some(&document) = self.documents.get(name) else {
            return false;
        };
        let next = package.map(|p| Arc::new(p.to_ascii_uppercase()));
        if *document.initial_package(&self.database) != next {
            document.set_initial_package(&mut self.database).to(next);
        }
        true
    }

    /// Adds a document or replaces its text. A byte-identical update keeps the
    /// current revision. A replacement can reuse syntax outside the edit.
    pub fn set_document(&mut self, name: impl Into<String>, text: impl Into<String>) {
        let text = text.into();
        match self.documents.entry(name.into()) {
            Entry::Occupied(entry) => {
                let document = *entry.get();
                let existing = document.live_syntax(&self.database);
                if existing.text() != text {
                    let syntax = existing.with_text(text);
                    document
                        .set_syntax(&mut self.database)
                        .to(Some(Arc::new(syntax)));
                }
            }
            Entry::Vacant(entry) => {
                let source = SourceDocument::new(None, Some(text)).with_uri(entry.key().clone());
                let syntax = Some(Arc::new(DocumentSyntax::new(source, self.readtable)));
                let features = self.features.clone();
                let document = match self.vacant_documents.pop() {
                    Some(document) => {
                        document.set_syntax(&mut self.database).to(syntax);
                        document.set_initial_package(&mut self.database).to(None);
                        document.set_features(&mut self.database).to(features);
                        document
                    }
                    None => Document::new(&self.database, syntax, None, features),
                };
                entry.insert(document);
            }
        }
    }

    /// Analyze a host's exact retained snapshot, sharing text and identity with
    /// execution, diagnostics and code actions. Unlike `set_document`, this
    /// replaces identity even when a new host revision has byte-identical text.
    pub fn set_document_source(
        &mut self,
        name: impl Into<String>,
        source: SourceDocument,
    ) -> Result<(), SourceEditError> {
        source.byte_len().ok_or(SourceEditError::MissingText)?;
        let name = name.into();
        if let Some(&document) = self.documents.get(&name) {
            let existing = document.live_syntax(&self.database);
            if existing.source == source {
                return Ok(());
            }
            let (range, replacement) = changed_ranges(existing.text(), source.text().unwrap());
            let edit = existing.pending_reparse(range, replacement.len());
            let syntax = DocumentSyntax {
                edit: Mutex::new(edit),
                ..DocumentSyntax::new(source, existing.initial_readtable)
            };
            document
                .set_syntax(&mut self.database)
                .to(Some(Arc::new(syntax)));
        } else {
            let syntax = Some(Arc::new(DocumentSyntax::new(source, self.readtable)));
            let features = self.features.clone();
            let document = match self.vacant_documents.pop() {
                Some(document) => {
                    document.set_syntax(&mut self.database).to(syntax);
                    document.set_initial_package(&mut self.database).to(None);
                    document.set_features(&mut self.database).to(features);
                    document
                }
                None => Document::new(&self.database, syntax, None, features),
            };
            self.documents.insert(name, document);
        }
        Ok(())
    }

    /// Removes a buffer. Snapshots already held by callers stay intact.
    pub fn remove_document(&mut self, name: &str) -> bool {
        self.visible_sources.remove(name);
        let Some(document) = self.documents.remove(name) else {
            return false;
        };
        document.set_syntax(&mut self.database).to(None);
        document.set_initial_package(&mut self.database).to(None);
        document.set_features(&mut self.database).to(None);
        // Replace the memo now so Salsa can reclaim it on the next revision,
        // even when nobody asks for another snapshot of this slot.
        document_snapshot(&self.database, document);
        self.vacant_documents.push(document);
        true
    }

    /// Replaces a half-open byte range in an open buffer. Both endpoints must
    /// be on UTF-8 character boundaries. An identical edit keeps the revision.
    pub fn apply_edit(
        &mut self,
        name: &str,
        range: Range<usize>,
        replacement: &str,
    ) -> Result<(), SourceEditError> {
        let document = *self
            .documents
            .get(name)
            .ok_or(SourceEditError::UnknownDocument)?;
        let syntax = document.live_syntax(&self.database);
        if let Some(edited) = syntax.edited(range, replacement)? {
            document
                .set_syntax(&mut self.database)
                .to(Some(Arc::new(edited)));
        }
        Ok(())
    }

    /// Apply a document edit batch atomically. Ranges use the text after the
    /// preceding change. `None` replaces the full buffer. Out-of-line columns
    /// clamp to line end, but positions inside a character are invalid.
    pub fn apply_position_edits<'a>(
        &mut self,
        name: &str,
        encoding: SourcePositionEncoding,
        edits: impl IntoIterator<Item = (Option<Range<SourcePosition>>, &'a str)>,
    ) -> Result<(), SourceEditError> {
        let document = *self
            .documents
            .get(name)
            .ok_or(SourceEditError::UnknownDocument)?;
        let before = document.live_syntax(&self.database);
        let mut edited = None;
        let mut delta: Option<(Range<usize>, usize)> = None;
        for (range, text) in edits {
            let current = edited.as_ref().unwrap_or(before);
            let (range, replacement) = if let Some(range) = range {
                let byte = |position| {
                    current
                        .source
                        .clamped_byte_offset_with_encoding(position, encoding)
                        .ok_or(SourceEditError::InvalidRange)
                };
                (byte(range.start)?..byte(range.end)?, text)
            } else {
                let (range, inserted) = changed_ranges(current.text(), text);
                (range, &text[inserted])
            };
            if let Some(next) = current.edited(range.clone(), replacement)? {
                let inserted = replacement.len();
                delta = Some(match delta.take() {
                    Some((old, size)) => combine_edits(old, size, range, inserted),
                    None => (range, inserted),
                });
                edited = Some(next);
            }
        }
        if let (Some(edited), Some((range, inserted))) = (edited, delta)
            && (range.len() != inserted || before.text()[range.clone()] != edited.text()[range])
        {
            document
                .set_syntax(&mut self.database)
                .to(Some(Arc::new(edited)));
        }
        Ok(())
    }

    /// Returns the current immutable version of a buffer and its analysis.
    pub fn snapshot(&self, name: &str) -> Option<Arc<SourceSnapshot>> {
        let document = *self.documents.get(name)?;
        document_snapshot(&self.database, document)
    }

    /// Returns the current syntax tree and diagnostics for a known document.
    pub fn parse(&self, name: &str) -> Option<Arc<ParsedSource>> {
        let document = *self.documents.get(name)?;
        Some(Arc::clone(document.live_syntax(&self.database).parsed()))
    }

    /// Returns the structural diagnostics for a known document.
    pub fn diagnostics(&self, name: &str) -> Option<Arc<[SyntaxDiagnostic]>> {
        let document = *self.documents.get(name)?;
        Some(Arc::clone(
            document.live_syntax(&self.database).diagnostics(),
        ))
    }
}

fn set_document_readtable(
    db: &mut impl Database,
    document: Document,
    readtable: SourceReadtable,
) -> bool {
    let existing = document.live_syntax(db);
    if existing.initial_readtable == readtable {
        return false;
    }
    let syntax = DocumentSyntax::new(existing.source.clone(), readtable);
    document.set_syntax(db).to(Some(Arc::new(syntax)));
    true
}

#[cfg(all(test, feature = "analysis"))]
mod tests {
    use super::*;

    #[test]
    fn shared_host_snapshot_survives_edits_and_analysis_context_changes() {
        let document = SourceDocument::input("(list 雪 😀)\r\n")
            .with_uri("untitled:shared")
            .with_version(7);
        let mut workspace = SourceWorkspace::new();
        workspace
            .set_document_source("host", document.clone())
            .unwrap();
        let before = workspace.snapshot("host").unwrap();
        assert_eq!(before.source(), &document);
        assert_eq!(before.source().version(), Some(7));
        assert_eq!(before.position(6), document.position(6));
        workspace.set_initial_package("host", Some("SHARED".into()));
        workspace.set_initial_readtable("host", SourceReadtable::RawStrings);
        assert_eq!(workspace.snapshot("host").unwrap().source(), &document);
        let snow = before.text().find('雪').unwrap();
        workspace.apply_edit("host", snow..snow + 3, "犬").unwrap();
        let after = workspace.snapshot("host").unwrap();
        assert_ne!(before.source(), after.source());
        assert_eq!(before.source(), &document);
        assert!(before.text().contains('雪'));
        assert!(after.text().contains('犬'));
        assert_eq!(after.source().uri(), Some("untitled:shared"));
        let new_host_version = document.clone().with_version(8);
        workspace
            .set_document_source("host", new_host_version.clone())
            .unwrap();
        assert_eq!(
            workspace.snapshot("host").unwrap().source(),
            &new_host_version
        );
        assert_eq!(before.source().version(), Some(7));
    }

    #[test]
    fn workspace_warnings_invalidate_on_signature_package_or_location_changes() {
        let mut workspace = SourceWorkspace::new();
        let file = "library.lisp";
        workspace.set_document(
            file,
            "(defpackage :demo (:export :f))\n(in-package :demo)\n(defun f (x) x)",
        );
        let original = workspace.snapshot(file).unwrap();
        for (text, changes_warnings) in [
            (
                "(defpackage :demo (:export :f))\n(in-package :demo)\n(defun f (x) \"documentation\" (list x))",
                false,
            ),
            (
                "; Shift declaration ranges\n(defpackage :demo (:export :f))\n(in-package :demo)\n(defun f (x) x)",
                true,
            ),
            (
                "(defpackage :demo (:export :f))\n(in-package :demo)\n(defun f (x y) (list x y))",
                true,
            ),
            (
                "(defpackage :demo (:import-from :other :f) (:export :f))\n(in-package :demo)\n(defun f (x) x)",
                true,
            ),
        ] {
            workspace.set_document(file, text);
            assert_eq!(
                SourceSnapshot::changes_workspace_warnings(
                    Some(&original),
                    workspace.snapshot(file).as_deref()
                ),
                changes_warnings,
                "{text}",
            );
        }
    }

    const METHOD_SCOPE_SOURCE: &str = "\
(in-package #:demo)
(defclass box-item () ())
(defun item-val (b) b)
(defun (setf item-val) (new-v b)
  (return-from item-val new-v))
(defmethod update-box :around ((b box-item) &optional (delta 1))
  (funcall #'(setf item-val) delta b)
  (with-open-file (stream \"out.txt\")
    (with-slots ((v val)) b
      (with-accessors ((acc item-val)) b
        (restart-case (list stream v acc)
          (retry (arg) arg))))))
";

    #[test]
    fn setf_function_reference_jumps_to_writer_definition() {
        let mut ws = SourceWorkspace::new();
        let src = METHOD_SCOPE_SOURCE;
        ws.set_document("a.lisp", src);
        let setf_ref = src.find("#'(setf item-val)").unwrap() + "#'(setf ".len();
        let setf_def = src.find("(defun (setf item-val)").unwrap() + "(defun (setf ".len();
        let def_loc = ws.definition("a.lisp", setf_ref).expect("setf def");
        assert_eq!(usize::from(def_loc.selection_range.start()), setf_def);
    }

    #[test]
    fn return_from_setf_function_jumps_to_its_implicit_block() {
        let mut ws = SourceWorkspace::new();
        let src = METHOD_SCOPE_SOURCE;
        ws.set_document("a.lisp", src);
        let setf_def = src.find("(defun (setf item-val)").unwrap() + "(defun (setf ".len();
        let ret_ref = src.find("(return-from item-val").unwrap() + "(return-from ".len();
        let ret_def = ws.definition("a.lisp", ret_ref).expect("setf block def");
        assert_eq!(usize::from(ret_def.selection_range.start()), setf_def);
    }

    #[test]
    fn defmethod_specializer_jumps_to_class_definition() {
        let mut ws = SourceWorkspace::new();
        let src = METHOD_SCOPE_SOURCE;
        ws.set_document("a.lisp", src);
        let spec_ref = src.find("((b box-item)").unwrap() + "((b ".len();
        let class_def = src.find("(defclass box-item").unwrap() + "(defclass ".len();
        let spec_loc = ws
            .definition("a.lisp", spec_ref)
            .expect("class specializer def");
        assert_eq!(usize::from(spec_loc.selection_range.start()), class_def);
    }

    #[test]
    fn nested_binding_macros_resolve_names_in_method_body() {
        let mut ws = SourceWorkspace::new();
        let src = METHOD_SCOPE_SOURCE;
        ws.set_document("a.lisp", src);
        let stream_ref = src.find("(list stream").unwrap() + "(list ".len();
        let stream_def = src.find("(with-open-file (stream").unwrap() + "(with-open-file (".len();
        assert_eq!(
            usize::from(
                ws.definition("a.lisp", stream_ref)
                    .unwrap()
                    .selection_range
                    .start()
            ),
            stream_def
        );

        let v_ref = src.find("stream v acc").unwrap() + "stream ".len();
        let v_def = src.find("((v val))").unwrap() + "((".len();
        assert_eq!(
            usize::from(
                ws.definition("a.lisp", v_ref)
                    .unwrap()
                    .selection_range
                    .start()
            ),
            v_def
        );

        let acc_fn_ref = src.find("((acc item-val))").unwrap() + "((acc ".len();
        let item_val_def = src.find("(defun item-val").unwrap() + "(defun ".len();
        assert_eq!(
            usize::from(
                ws.definition("a.lisp", acc_fn_ref)
                    .unwrap()
                    .selection_range
                    .start()
            ),
            item_val_def
        );

        let retry_arg_ref = src.find("(retry (arg) arg)").unwrap() + "(retry (arg) ".len();
        let retry_arg_def = src.find("(retry (arg)").unwrap() + "(retry (".len();
        assert_eq!(
            usize::from(
                ws.definition("a.lisp", retry_arg_ref)
                    .unwrap()
                    .selection_range
                    .start()
            ),
            retry_arg_def
        );
    }

    const UNUSED_PARAMETER_SOURCE: &str = "\
(in-package #:demo)
(defun sample (used ignored unused)
  (declare (ignore ignored))
  (cons used 1 2))
";

    #[test]
    fn warnings_distinguish_unused_parameter_from_declared_ignore() {
        let mut ws = SourceWorkspace::new();
        let src = UNUSED_PARAMETER_SOURCE;
        ws.set_document("a.lisp", src);
        let warnings = ws.semantic_warnings("a.lisp");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .any(|w| w.message == "unused lexical variable `unused`")
        );
    }

    #[test]
    fn semantic_warnings_report_too_many_arguments_to_cons() {
        let mut ws = SourceWorkspace::new();
        let src = UNUSED_PARAMETER_SOURCE;
        ws.set_document("a.lisp", src);
        let warnings = ws.semantic_warnings("a.lisp");
        assert!(
            warnings
                .iter()
                .any(|w| w.message == "cons takes at most 2 arguments, found 3")
        );
    }

    #[test]
    fn workspace_symbol_search_finds_a_partial_name() {
        let mut ws = SourceWorkspace::new();
        let src = UNUSED_PARAMETER_SOURCE;
        ws.set_document("a.lisp", src);
        let syms = ws.workspace_symbols("samp");
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].name, "sample");
    }

    #[test]
    fn inlay_hints_label_subseq_parameters_in_order() {
        let mut ws = SourceWorkspace::new();
        let hint_src = "(in-package #:demo)\n(subseq \"hello\" 1 3)\n";
        ws.set_document("b.lisp", hint_src);
        let full_range = TextRange::new(0.into(), (hint_src.len() as u32).into());
        let hints = ws.inlay_hints("b.lisp", full_range);
        assert_eq!(hints.len(), 3, "{hints:?}");
        assert_eq!(hints[0].label, "sequence:");
        assert_eq!(hints[1].label, "start:");
        assert_eq!(hints[2].label, "end:");
    }

    #[test]
    fn unused_parameter_offers_ignore_quick_fix() {
        let mut ws = SourceWorkspace::new();
        let src = UNUSED_PARAMETER_SOURCE;
        ws.set_document("a.lisp", src);
        let unused_pos = src.find("unused)").unwrap() as u32;
        let actions = ws.code_actions(
            "a.lisp",
            TextRange::new(unused_pos.into(), (unused_pos + 6).into()),
        );
        assert!(
            actions
                .iter()
                .any(|a| a.title == "Declare `(ignore unused)`"),
            "{actions:?}"
        );
    }

    #[test]
    fn unexported_function_offers_package_export_refactoring() {
        let mut ws = SourceWorkspace::new();
        let src = UNUSED_PARAMETER_SOURCE;
        ws.set_document("a.lisp", src);
        ws.set_document(
            "pkg.lisp",
            "(defpackage #:demo (:use #:cl) (:export #:existing))\n",
        );
        let unused_pos = src.find("unused)").unwrap() as u32;
        let actions = ws.code_actions(
            "a.lisp",
            TextRange::new(unused_pos.into(), (unused_pos + 6).into()),
        );
        assert!(
            actions
                .iter()
                .any(|a| a.title == "Export `sample` from package `DEMO`"),
            "{actions:?}"
        );
    }

    #[test]
    fn flet_call_uses_local_lambda_list() {
        let mut ws = SourceWorkspace::new();
        let flet_src = "\
(in-package #:demo)
(defun outer (a b)
  (flet ((pair-sum (left right) (+ left right)))
    (declare (ignore a b))
    (pair-sum 10 20 30)))
";
        ws.set_document("flet.lisp", flet_src);
        let call_pos = flet_src.find("(pair-sum 10 20").unwrap() + "(pair-sum 10 ".len();
        let sig = ws.signature_help("flet.lisp", call_pos).expect("flet sig");
        assert_eq!(sig.label, "(PAIR-SUM left right)");
        assert_eq!(sig.active_parameter, Some(1));

        let err = ws
            .call_arity_error("flet.lisp", call_pos)
            .expect("flet arity");
        assert_eq!(err, "pair-sum takes at most 2 arguments, found 3");

        let hints = ws.inlay_hints(
            "flet.lisp",
            TextRange::new(0.into(), (flet_src.len() as u32).into()),
        );
        assert!(hints.iter().any(|h| h.label == "left:"));
        assert!(hints.iter().any(|h| h.label == "right:"));
    }

    #[test]
    fn quoted_cons_form_emits_no_arity_warning() {
        let mut ws = SourceWorkspace::new();
        let safe_src = "\
(in-package #:demo)
(defun my-fn (cons extra)
  (list cons extra '(cons 1 2 3)))
";
        ws.set_document("safe.lisp", safe_src);
        assert!(ws.semantic_warnings("safe.lisp").is_empty());
    }

    #[test]
    fn unfinished_call_completes_a_multiple_value_binding() {
        let mut ws = SourceWorkspace::new();
        let typing_src = "\
(in-package #:demo)
(defun typing (alpha-arg)
  (multiple-value-bind (first-val second-val) (values 1 2)
    (list fir";
        ws.set_document("typing.lisp", typing_src);
        let cands = ws.completions("typing.lisp", typing_src.len());
        assert!(cands.iter().any(|c| c.label == "first-val"), "{cands:?}");
    }

    const EVAL_WHEN_SOURCE: &str = "\
(eval-when (:compile-toplevel :load-toplevel :execute)
  (defpackage #:ew-pkg (:use #:cl) (:export #:ew-fn))
  (in-package #:ew-pkg)
  (defun ew-fn (a b &key (scale 1))
    (+ a (* b scale))))
(eval-when (compile)
  (defun legacy-fn (x)
    (ew-fn x)))
(eval-when ()
  (defun dead-fn () 1))
";

    #[test]
    fn eval_when_outline_excludes_definitions_in_an_empty_situation_list() {
        let mut ws = SourceWorkspace::new();
        ws.set_document("ew.lisp", EVAL_WHEN_SOURCE);
        let snap = ws.snapshot("ew.lisp").unwrap();
        let outline_names: Vec<_> = snap.outline().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(outline_names, ["#:ew-pkg", "ew-fn", "legacy-fn"]);
    }

    #[test]
    fn in_package_inside_eval_when_applies_to_later_calls() {
        let mut ws = SourceWorkspace::new();
        let src = EVAL_WHEN_SOURCE;
        ws.set_document("ew.lisp", src);
        let call_offset = src.find("(ew-fn x)").unwrap() + 1;
        let def_loc = ws.definition("ew.lisp", call_offset).expect("ew-fn def");
        assert_eq!(
            usize::from(def_loc.selection_range.start()),
            src.find("ew-fn (a b").unwrap()
        );
    }

    #[test]
    fn eval_when_situation_hover_explains_compile_toplevel() {
        let mut ws = SourceWorkspace::new();
        let src = EVAL_WHEN_SOURCE;
        ws.set_document("ew.lisp", src);
        let sit_offset = src.find(":compile-toplevel").unwrap() + 2;
        let sit_hover = ws.hover("ew.lisp", sit_offset).expect("situation hover");
        assert!(
            sit_hover
                .contents
                .contains("eval-when-situation :compile-toplevel"),
            "{}",
            sit_hover.contents
        );
    }

    #[test]
    fn eval_when_warnings_include_the_selected_body_call() {
        let mut ws = SourceWorkspace::new();
        ws.set_document("ew.lisp", EVAL_WHEN_SOURCE);
        let warnings = ws.semantic_warnings("ew.lisp");
        assert!(
            warnings.iter().any(|w| {
                w.message == "deprecated `eval-when` situation `compile`; use `:compile-toplevel`"
            }),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.message == "ew-fn requires at least 2 arguments, found 1"),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.message == "empty `eval-when` situation list discards all body forms"),
            "{warnings:?}"
        );
    }

    #[test]
    fn deprecated_eval_when_compile_situation_offers_modern_spelling() {
        let mut ws = SourceWorkspace::new();
        let src = EVAL_WHEN_SOURCE;
        ws.set_document("ew.lisp", src);
        let compile_sit = src.find("(compile)").unwrap() + 1;
        let actions = ws.code_actions(
            "ew.lisp",
            TextRange::new(
                (compile_sit as u32).into(),
                ((compile_sit + "compile".len()) as u32).into(),
            ),
        );
        assert!(
            actions
                .iter()
                .any(|a| a.title == "Replace deprecated `compile` with `:compile-toplevel`"),
            "{actions:?}"
        );
    }

    #[test]
    fn eval_when_situation_completes_to_compile_toplevel() {
        let mut ws = SourceWorkspace::new();
        let comp_src = "(in-package #:ew-pkg)\n(eval-when (:c))\n(ew-fn 1 2 :s)\n";
        ws.set_document("comp.lisp", comp_src);
        let sit_comp_pos = comp_src.find(":c))").unwrap() + 2;
        let sit_cands = ws.completions("comp.lisp", sit_comp_pos);
        assert_eq!(sit_cands.len(), 1);
        assert_eq!(sit_cands[0].label, ":compile-toplevel");
    }

    #[test]
    fn call_completes_keyword_parameter_from_eval_when_definition() {
        let mut ws = SourceWorkspace::new();
        ws.set_document("ew.lisp", EVAL_WHEN_SOURCE);
        let comp_src = "(in-package #:ew-pkg)\n(eval-when (:c))\n(ew-fn 1 2 :s)\n";
        ws.set_document("comp.lisp", comp_src);
        let kw_comp_pos = comp_src.find(":s)").unwrap() + 2;
        let kw_cands = ws.completions("comp.lisp", kw_comp_pos);
        assert!(
            kw_cands
                .iter()
                .any(|c| c.label == ":scale"
                    && c.detail.as_deref() == Some("&key parameter of ew-fn")),
            "{kw_cands:?}"
        );
    }

    const LOCAL_MACRO_SOURCE: &str = "\
(in-package #:demo)
(defclass account ()
  ((balance :initarg :balance :accessor account-balance :initform (+ 10 20))))
(defmacro with-tag (x)
  `(list :tag ,x))
(defun run-account (acc)
  (macrolet ((double-it (form) (let ((tmp (gensym))) `(let ((,tmp ,form)) (+ ,tmp ,tmp)))))
    (prog ((cur (account-balance acc)))
     retry
      (when (< cur 0)
        (setf (account-balance acc) 0)
        (go retry))
      (return (double-it 1)))))
";

    #[test]
    fn unquote_counts_as_a_reference_to_a_macro_parameter() {
        let mut ws = SourceWorkspace::new();
        let src = LOCAL_MACRO_SOURCE;
        ws.set_document("clos.lisp", src);
        let warnings = ws.semantic_warnings("clos.lisp");
        assert!(warnings.is_empty(), "{warnings:?}");
        let x_ref = src.find(",x))").unwrap() + 1;
        let x_def = src.find("with-tag (x)").unwrap() + "with-tag (".len();
        assert_eq!(
            usize::from(
                ws.definition("clos.lisp", x_ref)
                    .unwrap()
                    .selection_range
                    .start()
            ),
            x_def
        );
    }

    #[test]
    fn accessor_call_jumps_to_the_defclass_slot_option() {
        let mut ws = SourceWorkspace::new();
        let src = LOCAL_MACRO_SOURCE;
        ws.set_document("clos.lisp", src);
        let acc_def = src.find(":accessor account-balance").unwrap() + ":accessor ".len();
        let reader_call = src.find("(account-balance acc)))").unwrap() + 1;
        assert_eq!(
            usize::from(
                ws.definition("clos.lisp", reader_call)
                    .unwrap()
                    .selection_range
                    .start()
            ),
            acc_def
        );
    }

    #[test]
    fn macro_call_inside_prog_jumps_to_the_macrolet_definition() {
        let mut ws = SourceWorkspace::new();
        let src = LOCAL_MACRO_SOURCE;
        ws.set_document("clos.lisp", src);
        let mac_call = src.find("(double-it 1)").unwrap() + 1;
        let mac_def = src.find("((double-it (form)").unwrap() + 2;
        assert_eq!(
            usize::from(
                ws.definition("clos.lisp", mac_call)
                    .unwrap()
                    .selection_range
                    .start()
            ),
            mac_def
        );
    }

    #[test]
    fn go_inside_prog_jumps_to_the_local_tag() {
        let mut ws = SourceWorkspace::new();
        let src = LOCAL_MACRO_SOURCE;
        ws.set_document("clos.lisp", src);
        let go_ref = src.find("(go retry)").unwrap() + "(go ".len();
        let tag_def = src.find("retry\n").unwrap();
        assert_eq!(
            usize::from(
                ws.definition("clos.lisp", go_ref)
                    .unwrap()
                    .selection_range
                    .start()
            ),
            tag_def
        );
    }

    fn resolves(ws: &mut SourceWorkspace, reference: &str, name: &str) -> bool {
        ws.set_document("caller.lisp", reference);
        let offset = reference
            .find(name)
            .unwrap_or_else(|| panic!("missing {name}"));
        ws.definition("caller.lisp", offset).is_some()
    }

    #[test]
    fn nested_top_level_wrappers_preserve_package_and_lexical_resolution() {
        let mut ws = SourceWorkspace::new();
        let src = "\
(in-package #:nested-pkg)
(defun default-nested-value () 0)
(progn
  (locally
    (eval-when (:compile-toplevel :load-toplevel :execute)
      (defclass nested-item ()
        ((value :initarg :value :accessor nested-value)))
      (defstruct nested-struct
        (slot (default-nested-value) :type nested-item))
      (defun nested-read (item)
        (nested-value item)))))
(defun nested-caller (item)
  (list (nested-read item) (nested-value item)))
";
        ws.set_document("nested.lisp", src);
        let snapshot = ws.snapshot("nested.lisp").unwrap();
        let outline: Vec<_> = snapshot
            .outline()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            outline,
            [
                "default-nested-value",
                "nested-item",
                "nested-struct",
                "nested-read",
                "nested-caller"
            ]
        );

        // Index and scope agree on a declaration inside all three wrappers.
        let read_def = src.find("nested-read (item)").unwrap();
        let read_call = src.find("(nested-read item)").unwrap() + 1;
        let definition = ws
            .definition("nested.lisp", read_call)
            .expect("nested-read definition");
        assert_eq!(usize::from(definition.selection_range.start()), read_def);
        let hover = ws
            .hover("nested.lisp", read_call)
            .expect("nested-read hover");
        assert!(
            hover.contents.contains("NESTED-PKG::NESTED-READ"),
            "{}",
            hover.contents
        );
        let references = ws.references("nested.lisp", read_def, false);
        assert_eq!(references.len(), 1, "{references:?}");
        assert_eq!(
            usize::from(references[0].range.start()),
            src.find("(nested-read item)").unwrap() + 1
        );

        // A defclass accessor declared inside the wrappers still resolves both
        // inside the wrapper and from the following top-level definition.
        let accessor_def = src.find(":accessor nested-value").unwrap() + ":accessor ".len();
        let nested_accessor_ref = src.find("(nested-value item)").unwrap() + 1;
        for accessor_ref in [
            nested_accessor_ref,
            src.rfind("(nested-value item)").unwrap() + 1,
        ] {
            assert_eq!(
                usize::from(
                    ws.definition("nested.lisp", accessor_ref)
                        .expect("accessor definition")
                        .selection_range
                        .start()
                ),
                accessor_def
            );
        }

        // A defstruct slot's :type and initform resolve inside the wrappers too.
        let slot_type = src.find(":type nested-item").unwrap() + ":type ".len();
        assert_eq!(
            usize::from(
                ws.definition("nested.lisp", slot_type)
                    .expect("slot type")
                    .selection_range
                    .start()
            ),
            src.find("nested-item ()").unwrap()
        );
        let initform = src.find("(default-nested-value)").unwrap() + 1;
        assert_eq!(
            usize::from(
                ws.definition("nested.lisp", initform)
                    .expect("slot initform")
                    .selection_range
                    .start()
            ),
            src.find("default-nested-value ()").unwrap()
        );
        assert!(ws.semantic_warnings("nested.lisp").is_empty());
    }

    #[test]
    fn definitions_remain_navigable_inside_unclosed_top_level_wrappers() {
        for (wrapper, open) in [
            ("progn", "(progn\n"),
            ("locally", "(locally\n"),
            ("eval-when", "(eval-when (:execute)\n"),
        ] {
            let mut ws = SourceWorkspace::new();
            let src = format!(
                "(in-package #:partial-pkg)\n{open}  (defun partial-target (arg)\n    (list arg))\n  (partial-target 1)\n"
            );
            ws.set_document("partial.lisp", &src);
            let snapshot = ws.snapshot("partial.lisp").unwrap();
            let outline: Vec<_> = snapshot
                .outline()
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(outline, ["partial-target"], "{wrapper}");

            let target = src.find("partial-target (arg)").unwrap();
            let hover = ws
                .hover("partial.lisp", target)
                .unwrap_or_else(|| panic!("{wrapper} hover"));
            assert!(
                hover.contents.contains("PARTIAL-PKG::PARTIAL-TARGET"),
                "{wrapper}: {}",
                hover.contents
            );

            let arg_ref = src.find("(list arg)").unwrap() + "(list ".len();
            let arg_def = target + "partial-target (".len();
            assert_eq!(
                usize::from(
                    ws.definition("partial.lisp", arg_ref)
                        .unwrap_or_else(|| panic!("{wrapper} lexical"))
                        .selection_range
                        .start()
                ),
                arg_def,
                "{wrapper}"
            );

            let call = src.find("(partial-target 1)").unwrap() + 1;
            assert_eq!(
                usize::from(
                    ws.definition("partial.lisp", call)
                        .unwrap_or_else(|| panic!("{wrapper} call"))
                        .selection_range
                        .start()
                ),
                target,
                "{wrapper}"
            );

            assert!(
                resolves(&mut ws, "(partial-pkg::partial-target)", "partial-target"),
                "{wrapper}"
            );
            assert!(
                !resolves(&mut ws, "(other-pkg::partial-target)", "partial-target"),
                "{wrapper}"
            );
        }
    }

    #[test]
    fn feature_conditionals_leave_package_unknown_only_when_they_may_change_it() {
        let src = "\
(in-package #:known-pkg)
(progn #+optional-pkg (locally (in-package #:maybe-pkg)))
(defun wrapper-target () 2)
#+optional-pkg (in-package #:root-maybe)
(defun root-target () 3)
";
        let mut ws = SourceWorkspace::new();
        ws.set_document("pkgs.lisp", src);
        let snapshot = ws.snapshot("pkgs.lisp").unwrap();
        let outline: Vec<_> = snapshot
            .outline()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(outline, ["wrapper-target", "root-target"]);

        // Unknown features: both conditionals may change the package, so later
        // declarations cannot be attributed to a definite package.
        for (reference, name) in [
            ("(known-pkg::wrapper-target)", "wrapper-target"),
            ("(maybe-pkg::wrapper-target)", "wrapper-target"),
            ("(known-pkg::root-target)", "root-target"),
            ("(root-maybe::root-target)", "root-target"),
        ] {
            assert!(!resolves(&mut ws, reference, name), "{reference}");
        }
        ws.set_document(
            "control.lisp",
            "(in-package #:known-pkg)\n(defun control-target () 1)\n",
        );
        assert!(resolves(
            &mut ws,
            "(known-pkg::control-target)",
            "control-target"
        ));

        // A known feature list makes the same source definite again.
        ws.set_features(Some(vec!["unrelated".into()]));
        assert!(resolves(
            &mut ws,
            "(known-pkg::wrapper-target)",
            "wrapper-target"
        ));
        assert!(resolves(&mut ws, "(known-pkg::root-target)", "root-target"));
        assert!(!resolves(
            &mut ws,
            "(maybe-pkg::wrapper-target)",
            "wrapper-target"
        ));

        ws.set_features(Some(vec!["optional-pkg".into()]));
        assert!(resolves(
            &mut ws,
            "(maybe-pkg::wrapper-target)",
            "wrapper-target"
        ));
        assert!(resolves(
            &mut ws,
            "(root-maybe::root-target)",
            "root-target"
        ));
        assert!(!resolves(
            &mut ws,
            "(known-pkg::root-target)",
            "root-target"
        ));
    }

    #[test]
    fn unevaluated_feature_conditionals_preserve_unknown_reader_state() {
        let src = "\
(in-package #:reader-pkg)
#+optional-pkg (set-macro-character)
(defun after-reader () 1)
";
        let mut ws = SourceWorkspace::new();
        ws.set_document("reader.lisp", src);
        assert!(!resolves(
            &mut ws,
            "(reader-pkg::after-reader)",
            "after-reader"
        ));

        ws.set_features(Some(vec!["unrelated".into()]));
        assert!(resolves(
            &mut ws,
            "(reader-pkg::after-reader)",
            "after-reader"
        ));
    }

    #[test]
    fn completions_inside_a_wrapper_use_the_package_from_a_nested_in_package() {
        let mut ws = SourceWorkspace::new();
        let src = "\
(in-package #:start-pkg)
(progn
  (in-package #:demo-pkg)
  (defun wrapped-fn (x) x)

)
";
        ws.set_document("gap.lisp", src);
        let gap = src.find("\n\n)").unwrap() + 1;
        let candidates = ws.completions("gap.lisp", gap);
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.label == "wrapped-fn"),
            "{candidates:?}"
        );

        let before_in_package = src.find("(in-package #:demo-pkg)").unwrap();
        let candidates = ws.completions("gap.lisp", before_in_package);
        assert!(
            !candidates
                .iter()
                .any(|candidate| candidate.label == "wrapped-fn"),
            "{candidates:?}"
        );

        // The same package context is available at the end of an unclosed wrapper.
        let unclosed = "\
(in-package #:start-pkg)
(progn
  (in-package #:demo-pkg)
  (defun wrapped-fn (x) x)
  ";
        ws.set_document("unclosed-gap.lisp", unclosed);
        let candidates = ws.completions("unclosed-gap.lisp", unclosed.len());
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.label == "wrapped-fn"),
            "{candidates:?}"
        );

        // A new top-level form typed after the last definition keeps the package.
        let eof = "(in-package #:demo-pkg)\n(defun eof-fn (x) x)\n";
        ws.set_document("eof.lisp", eof);
        let candidates = ws.completions("eof.lisp", eof.len());
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.label == "eof-fn"),
            "{candidates:?}"
        );
    }

    const WRAPPED_EVAL_WHEN_SOURCE: &str = "\
(in-package #:demo)
(progn
  (locally
    (eval-when (compile)
      (defun legacy-wrapped () 1))))
(progn (locally (eval-when (:c))))
";

    #[test]
    fn hover_marks_nested_eval_when_compile_situation_as_deprecated() {
        let mut ws = SourceWorkspace::new();
        let src = WRAPPED_EVAL_WHEN_SOURCE;
        ws.set_document("wrapped-ew.lisp", src);
        let situation = src.find("(compile)").unwrap() + 1;
        let hover = ws
            .hover("wrapped-ew.lisp", situation)
            .expect("situation hover");
        assert!(
            hover
                .contents
                .contains("eval-when-situation compile (deprecated)"),
            "{}",
            hover.contents
        );
    }

    #[test]
    fn nested_eval_when_compile_situation_offers_keyword_replacement() {
        let mut ws = SourceWorkspace::new();
        let src = WRAPPED_EVAL_WHEN_SOURCE;
        ws.set_document("wrapped-ew.lisp", src);
        let situation = src.find("(compile)").unwrap() + 1;
        let actions = ws.code_actions(
            "wrapped-ew.lisp",
            TextRange::new(
                (situation as u32).into(),
                ((situation + "compile".len()) as u32).into(),
            ),
        );
        assert!(
            actions
                .iter()
                .any(|action| action.title
                    == "Replace deprecated `compile` with `:compile-toplevel`"),
            "{actions:?}"
        );
    }

    #[test]
    fn nested_eval_when_completes_a_partial_situation_keyword() {
        let mut ws = SourceWorkspace::new();
        let src = WRAPPED_EVAL_WHEN_SOURCE;
        ws.set_document("wrapped-ew.lisp", src);
        let completion = src.find(":c)").unwrap() + 2;
        let candidates = ws.completions("wrapped-ew.lisp", completion);
        assert_eq!(candidates.len(), 1, "{candidates:?}");
        assert_eq!(candidates[0].label, ":compile-toplevel");
    }

    #[test]
    fn invalid_eval_when_bodies_are_diagnosed_without_indexing_definitions() {
        let mut ws = SourceWorkspace::new();
        let src = "\
(in-package #:demo)
(progn)
(locally)
(progn (eval-when (bogus) (defun dead-fn () 1)))
(eval-when :execute (defun not-a-list () 1))
(defun live-fn () 2)
";
        ws.set_document("invalid.lisp", src);
        let snapshot = ws.snapshot("invalid.lisp").unwrap();
        let outline: Vec<_> = snapshot
            .outline()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(outline, ["live-fn"]);

        let messages: Vec<_> = ws
            .semantic_warnings("invalid.lisp")
            .into_iter()
            .map(|warning| warning.message)
            .collect();
        assert_eq!(
            messages,
            [
                "unknown `eval-when` situation `bogus`; expected `:compile-toplevel`, `:load-toplevel`, or `:execute`",
                "expected a list of `eval-when` situations (`:compile-toplevel`, `:load-toplevel`, `:execute`)",
            ]
        );

        for dead in ["(dead-fn)", "(not-a-list)"] {
            ws.set_document("caller.lisp", dead);
            assert!(
                ws.definition(
                    "caller.lisp",
                    dead.find(|c: char| c.is_alphabetic()).unwrap()
                )
                .is_none(),
                "{dead}"
            );
        }
    }
}
