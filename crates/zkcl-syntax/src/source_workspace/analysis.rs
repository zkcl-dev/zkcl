//! Workspace queries that require static language analysis.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::thread;

use super::{SourceSnapshot, SourceWorkspace, WorkspaceLintChanges, document_snapshot};
use crate::ast::{
    AstAtom, AstList, PreviewIdentity, eval_const_form_preview_with, eval_const_preview_with,
};
use crate::cl_builtins::{BuiltinKind, builtin_kind};
use crate::documentation::{DocumentationComment, DocumentationTests};
use crate::expression_feedback::{
    ExpressionTypeError, expression_type_error, infer_expression_type,
};
use crate::lint::{FormLintCache, LintConfig, LintDiagnostic, lint_with_form_cache};
use crate::navigation::index::{DocumentIndex, OccurrenceResolver, WorkspaceIndex};
use crate::navigation::scope::is_evaluated_call_position;
use crate::navigation::{Namespace, OccurrenceBinding};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};
use crate::{
    CallFeedback, CompletionCandidate, DefinitionLocation, DocumentHighlight, Hover, SemanticToken,
    SignatureHelp, SourceLocation, SymbolName, TextEdit, navigation,
};

/// Static feedback for a source buffer in its current context.
#[derive(Debug, Default)]
pub struct ExpressionFeedback {
    /// The known result type of a single-value expression.
    pub output_type: Option<&'static str>,
    /// A source range whose known type is incompatible with the call.
    pub type_error: Option<ExpressionTypeError>,
    /// A constant value in standard print settings, ready for display-side clipping.
    pub preview: Option<String>,
}

impl SourceSnapshot {
    /// Whether replacing a document can change semantic warnings in other documents.
    pub fn changes_workspace_warnings(before: Option<&Self>, after: Option<&Self>) -> bool {
        !WorkspaceLintChanges::between(before, after).is_empty()
    }

    /// Turn fixes from this snapshot's diagnostics into actions at `range`.
    pub fn quick_fixes(
        &self,
        document: &str,
        range: TextRange,
        diagnostics: &[LintDiagnostic],
    ) -> Vec<navigation::CodeAction> {
        let mut actions = Vec::new();
        for diag in diagnostics {
            if diag.range.intersect(range).is_none()
                && !diag.range.contains_inclusive(range.start())
            {
                continue;
            }
            if let Some(fix) = &diag.fix {
                actions.push(navigation::CodeAction {
                    title: fix.title.clone(),
                    kind: navigation::CodeActionKind::QuickFix,
                    preferred: fix.applicability
                        == crate::lint::FixApplicability::MachineApplicable,
                    edits: fix
                        .edits
                        .iter()
                        .map(|edit| {
                            (
                                SourceLocation {
                                    document: document.to_owned(),
                                    range: edit.range,
                                },
                                edit.replacement.clone(),
                            )
                        })
                        .collect(),
                });
            }
        }
        actions
    }

    /// Ordered, non-overlapping machine-applicable edits for this snapshot.
    pub fn safe_lint_fix_edits(&self, diagnostics: &[LintDiagnostic]) -> Vec<TextEdit> {
        crate::lint::select_fixes(self.text(), diagnostics, false)
            .0
            .into_iter()
            .cloned()
            .collect()
    }

    /// Documentation comments attached to definitions in this snapshot.
    pub fn documentation(&self) -> &[DocumentationComment] {
        &self.index().documentation
    }

    /// Extract and structurally check the documentation examples in this snapshot.
    pub fn documentation_tests(&self) -> &DocumentationTests {
        self.documentation_tests.get_or_init(|| {
            let mut tests = DocumentationTests::default();
            if !self.text().contains(";;!") {
                return tests;
            }
            let index = self.index();
            let parsed = self.parsed();
            for comment in &index.documentation {
                let at = comment.definition.start();
                comment.collect_tests(&mut tests, index.package_at(at), parsed.readtable_at(at));
            }
            tests
        })
    }

    fn lint(
        &self,
        workspace: &WorkspaceIndex<'_>,
        config: &LintConfig,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Vec<LintDiagnostic>> {
        self.lint_with_cache(workspace, config, cancelled, None)
    }

    fn lint_with_cache(
        &self,
        workspace: &WorkspaceIndex<'_>,
        config: &LintConfig,
        cancelled: &impl Fn() -> bool,
        form_cache: Option<&mut FormLintCache>,
    ) -> Option<Vec<LintDiagnostic>> {
        let mut diagnostics = lint_with_form_cache(
            self.text(),
            self.index(),
            &self.parsed().root(),
            workspace,
            config,
            cancelled,
            form_cache,
        )?;
        let examples = self.documentation_tests().lint(
            self.features.as_deref(),
            workspace,
            config,
            cancelled,
        )?;
        if !examples.is_empty() {
            diagnostics.extend(examples);
            diagnostics.sort_by_key(|d| (d.range.start(), d.range.end(), d.rule_id));
        }
        Some(diagnostics)
    }

    pub(crate) fn index(&self) -> &navigation::DocumentIndex {
        self.index.get_or_init(|| {
            navigation::DocumentIndex::collect(
                &self.parsed().root(),
                self.initial_package.as_deref().map(String::as_str),
                self.features.as_deref(),
                self.syntax.initial_readtable,
            )
        })
    }
}

/// The source snapshot and workspace context needed to compute diagnostics.
/// Later workspace edits do not change the inputs held by this snapshot.
pub struct DiagnosticSnapshot {
    source: Arc<SourceSnapshot>,
    documents: Vec<(String, Arc<SourceSnapshot>)>,
}

impl DiagnosticSnapshot {
    pub fn source(&self) -> &SourceSnapshot {
        &self.source
    }

    /// The captured version of another workspace document used by diagnostics.
    pub fn document(&self, name: &str) -> Option<&SourceSnapshot> {
        self.documents
            .iter()
            .find_map(|(key, source)| (key == name).then_some(source.as_ref()))
    }

    /// Returns lint diagnostics for `config`, or `None` when cancelled.
    pub fn lint(
        &self,
        config: &crate::lint::LintConfig,
        cancelled: impl Fn() -> bool,
    ) -> Option<Vec<crate::lint::LintDiagnostic>> {
        self.lint_with_cache(config, cancelled, None)
    }

    /// Check with form results retained from earlier versions of this document.
    pub fn lint_with_cache(
        &self,
        config: &LintConfig,
        cancelled: impl Fn() -> bool,
        form_cache: Option<&mut FormLintCache>,
    ) -> Option<Vec<LintDiagnostic>> {
        let mut indices = Vec::with_capacity(self.documents.len());
        for (name, snapshot) in &self.documents {
            if cancelled() {
                return None;
            }
            indices.push((name.as_str(), snapshot.index()));
        }
        let workspace = WorkspaceIndex::new(indices).index_workspace_declarations(&cancelled)?;
        self.source
            .lint_with_cache(&workspace, config, &cancelled, form_cache)
    }

    /// Returns semantic warnings, or `None` when the analysis was cancelled.
    pub fn semantic_warnings(
        &self,
        cancelled: impl Fn() -> bool,
    ) -> Option<Vec<navigation::SemanticWarning>> {
        let config = crate::lint::LintConfig::default();
        self.lint(&config, cancelled).map(|diags| {
            diags
                .into_iter()
                .map(|d| navigation::SemanticWarning {
                    range: d.range,
                    message: d.message,
                })
                .collect()
        })
    }
}

impl SourceWorkspace {
    /// Captures the current document and workspace inputs without parsing them.
    pub fn diagnostic_snapshot(&self, name: &str) -> Option<DiagnosticSnapshot> {
        let source = self.snapshot(name)?;
        let documents = self.snapshots(self.visible_sources.get(name).map(AsRef::as_ref));
        Some(DiagnosticSnapshot { source, documents })
    }

    /// Resolve a reference at a UTF-8 byte offset using currently open source.
    ///
    /// Resolves both local lexical bindings (`DEFUN`, `LAMBDA`, `LET`/`LET*`, `FLET`/`LABELS`,
    /// `MULTIPLE-VALUE-BIND`, `DESTRUCTURING-BIND`, `DO`/`DO*`, `DOLIST`/`DOTIMES`,
    /// `BLOCK`/`RETURN-FROM`, `TAGBODY`/`GO`) and unique global declarations across open documents.
    pub fn definition(&self, document: &str, byte: usize) -> Option<DefinitionLocation> {
        let (source, offset) = self.snapshot_at(document, byte)?;
        let documents = self.visible_snapshots(document);
        navigation::definition(
            document,
            source.index(),
            &source.parsed().root(),
            offset,
            Self::indices(&documents),
        )
    }

    /// Find all statically provable references to the symbol at `byte`.
    pub fn references(
        &self,
        document: &str,
        byte: usize,
        include_declaration: bool,
    ) -> Vec<SourceLocation> {
        let Some((source, offset)) = self.snapshot_at(document, byte) else {
            return Vec::new();
        };
        let documents = self.visible_snapshots(document);
        let roots = Self::collect_roots(&documents);
        navigation::references(
            document,
            source.index(),
            &source.parsed().root(),
            offset,
            include_declaration,
            roots.iter().map(|(n, r, i)| (*n, r, *i)),
        )
    }

    /// Check whether the symbol at `byte` can be safely renamed and return its range and name.
    pub fn prepare_rename(&self, document: &str, byte: usize) -> Option<(TextRange, String)> {
        let (source, offset) = self.snapshot_at(document, byte)?;
        let documents = self.visible_snapshots(document);
        navigation::prepare_rename(
            source.index(),
            &source.parsed().root(),
            offset,
            Self::indices(&documents),
        )
    }

    /// Compute workspace-wide rename edits for the symbol at `byte`.
    pub fn rename(
        &self,
        document: &str,
        byte: usize,
        new_name: &str,
    ) -> Option<Vec<(SourceLocation, String)>> {
        let (source, offset) = self.snapshot_at(document, byte)?;
        let documents = self.visible_snapshots(document);
        let roots = Self::collect_roots(&documents);
        navigation::rename(
            document,
            source.index(),
            &source.parsed().root(),
            offset,
            new_name,
            source.features.as_deref(),
            roots.iter().map(|(n, r, i)| (*n, r, *i)),
        )
    }

    /// Highlight all occurrences of the symbol at `byte` within `document`.
    pub fn document_highlights(&self, document: &str, byte: usize) -> Vec<DocumentHighlight> {
        let Some((source, offset)) = self.snapshot_at(document, byte) else {
            return Vec::new();
        };
        let documents = self.visible_snapshots(document);
        navigation::document_highlights(
            document,
            source.index(),
            &source.parsed().root(),
            offset,
            Self::indices(&documents),
        )
    }

    /// Return hover documentation and a known constant value for a complete call.
    pub fn hover(&self, document: &str, byte: usize) -> Option<Hover> {
        let (source, offset) = self.snapshot_at(document, byte)?;
        let documents = self.visible_snapshots(document);
        let workspace = WorkspaceIndex::new(Self::indices(&documents));
        let index = source.index();
        let root = source.parsed().root();
        let mut hover = navigation::hover(index, &root, offset, &workspace)?;
        let resolve = RefCell::new(OccurrenceResolver::new(index, &root, &workspace, None));
        let is_standard = |atom: &AstAtom, name: &str| {
            standard_symbol_at(index, &workspace, &resolve, atom, name, &|_| None)
        };
        if let Some(value) = hover_constant_value(&root, hover.range, &source, &is_standard) {
            hover.contents.push_str("\n\nConstant preview\n\n");
            for line in value.split('\n') {
                hover.contents.push_str("    ");
                hover.contents.push_str(line);
                hover.contents.push('\n');
            }
        }
        Some(hover)
    }

    /// Return active function/macro call signature help at `byte`.
    pub fn signature_help(&self, document: &str, byte: usize) -> Option<SignatureHelp> {
        match self.call_query(document, byte, navigation::CallQuery::Signature, &|_| None)? {
            CallFeedback::Signature(signature) => Some(signature),
            CallFeedback::ArityError(_) => None,
        }
    }

    /// Return an arity error in a call enclosing `byte`, when its argument count is known.
    pub fn call_arity_error(&self, document: &str, byte: usize) -> Option<String> {
        match self.call_query(document, byte, navigation::CallQuery::Arity, &|_| None)? {
            CallFeedback::ArityError(error) => Some(error),
            CallFeedback::Signature(_) => None,
        }
    }

    /// Return an arity error or active signature help using a single call lookup.
    pub fn call_feedback(&self, document: &str, byte: usize) -> Option<CallFeedback> {
        self.call_feedback_with(document, byte, |_| None)
    }

    /// Return call feedback using the live builtin identity when available.
    pub fn call_feedback_with(
        &self,
        document: &str,
        byte: usize,
        live_standard: impl Fn(&SymbolName) -> Option<bool>,
    ) -> Option<CallFeedback> {
        self.call_query(
            document,
            byte,
            navigation::CallQuery::Feedback,
            &live_standard,
        )
    }

    fn call_query(
        &self,
        document: &str,
        byte: usize,
        query: navigation::CallQuery,
        live_standard: &impl Fn(&SymbolName) -> Option<bool>,
    ) -> Option<CallFeedback> {
        let (source, offset) = self.snapshot_at(document, byte)?;
        let documents = self.visible_snapshots(document);
        navigation::call_query(
            source.index(),
            &source.parsed().root(),
            offset,
            Self::indices(&documents),
            query,
            live_standard,
        )
    }

    /// Preview a side-effect-free constant expression in the document's static context.
    pub fn const_preview(&self, document: &str) -> Option<String> {
        self.expression_feedback(document, true, &|_| None, PreviewIdentity::Portable)?
            .preview
    }

    /// Analyze one source buffer using static lexical bindings and optional live
    /// evidence about whether a name still identifies a standard Lisp binding.
    /// This query follows native value identity. `None` asks the source index to resolve a name.
    pub fn expression_feedback_with(
        &self,
        document: &str,
        preview: bool,
        live_standard: impl Fn(&SymbolName) -> Option<bool>,
    ) -> ExpressionFeedback {
        self.expression_feedback(document, preview, &live_standard, PreviewIdentity::Native)
            .unwrap_or_default()
    }

    fn expression_feedback(
        &self,
        document: &str,
        preview: bool,
        live_standard: &impl Fn(&SymbolName) -> Option<bool>,
        identity: PreviewIdentity,
    ) -> Option<ExpressionFeedback> {
        let source = self.snapshot(document)?;
        let root = source.parsed().root();
        let index = source.index();
        let documents = self.visible_snapshots(document);
        let workspace = WorkspaceIndex::new(Self::indices(&documents));
        let resolve = RefCell::new(OccurrenceResolver::new(index, &root, &workspace, None));
        let is_standard = |atom: &AstAtom, name: &str| {
            standard_symbol_at(index, &workspace, &resolve, atom, name, live_standard)
        };
        let type_error = expression_type_error(&root, &is_standard);
        if type_error.is_some() || !source.diagnostics().is_empty() {
            return Some(ExpressionFeedback {
                type_error,
                ..ExpressionFeedback::default()
            });
        }
        Some(ExpressionFeedback {
            output_type: infer_expression_type(&root, &is_standard),
            type_error: None,
            preview: preview
                .then(|| eval_const_preview_with(&root, &is_standard, identity))
                .flatten(),
        })
    }

    /// Return completion candidates at `byte`.
    pub fn completions(&self, document: &str, byte: usize) -> Vec<CompletionCandidate> {
        self.completions_with_range(document, byte)
            .map(|(_, candidates)| candidates)
            .unwrap_or_default()
    }

    /// Return candidates and the UTF-8 byte range to replace with the chosen label.
    /// The range covers the entire symbol token, including any package prefix or
    /// text after `byte`. At an insertion point with no symbol, it is empty.
    pub fn completions_with_range(
        &self,
        document: &str,
        byte: usize,
    ) -> Option<(TextRange, Vec<CompletionCandidate>)> {
        let (source, offset) = self.snapshot_at(document, byte)?;
        let documents = self.visible_snapshots(document);
        Some(navigation::completions(
            source.index(),
            &source.parsed().root(),
            offset,
            Self::indices(&documents),
        ))
    }

    /// Return semantic tokens for `document`.
    pub fn semantic_tokens(&self, document: &str) -> Vec<SemanticToken> {
        let Some(source) = self.snapshot(document) else {
            return Vec::new();
        };
        let documents = self.visible_snapshots(document);
        navigation::semantic_tokens(
            document,
            source.index(),
            &source.parsed().root(),
            Self::indices(&documents),
        )
    }

    /// Return built-in lint diagnostics for `document` and its documentation examples.
    pub fn lint_document(
        &self,
        document: &str,
        config: &crate::lint::LintConfig,
    ) -> Vec<crate::lint::LintDiagnostic> {
        self.lint_documents([document], config)
            .pop()
            .unwrap_or_default()
    }

    /// Analyze the requested buffers in input order, sharing one workspace index.
    /// An unknown document produces an empty result at its input position.
    pub fn lint_documents<'a>(
        &self,
        requested: impl IntoIterator<Item = &'a str>,
        config: &crate::lint::LintConfig,
    ) -> Vec<Vec<crate::lint::LintDiagnostic>> {
        self.lint_documents_with_jobs(requested, config, NonZeroUsize::MIN)
    }

    /// Analyze buffers with at most `jobs` workers, using one shared index per
    /// visibility group. Results, including unknown documents and duplicates,
    /// retain their input positions.
    pub fn lint_documents_with_jobs<'a>(
        &self,
        requested: impl IntoIterator<Item = &'a str>,
        config: &LintConfig,
        jobs: NonZeroUsize,
    ) -> Vec<Vec<LintDiagnostic>> {
        let requested: Vec<_> = requested.into_iter().collect();
        let mut results = vec![Vec::new(); requested.len()];
        if requested
            .iter()
            .any(|name| self.visible_sources.contains_key(*name))
        {
            let mut groups = BTreeMap::<_, Vec<_>>::new();
            for (name, result) in requested.into_iter().zip(&mut results) {
                groups
                    .entry(self.visible_sources.get(name).map(AsRef::as_ref))
                    .or_default()
                    .push((name, result));
            }
            for (visible, members) in groups {
                self.lint_group(self.snapshots(visible), members, config, jobs);
            }
        } else {
            self.lint_group(
                self.all_snapshots(),
                requested.into_iter().zip(&mut results),
                config,
                jobs,
            );
        }
        results
    }

    fn lint_group<'a>(
        &self,
        documents: Vec<(&str, Arc<SourceSnapshot>)>,
        requested: impl IntoIterator<Item = (&'a str, &'a mut Vec<LintDiagnostic>)>,
        config: &LintConfig,
        jobs: NonZeroUsize,
    ) {
        let work: Vec<_> = requested
            .into_iter()
            .filter_map(|(name, result)| Some((self.snapshot(name)?, result)))
            .collect();
        if work.is_empty() {
            return;
        }
        if jobs.get() > 1 {
            let cold: Vec<_> = documents
                .iter()
                .filter(|(_, source)| source.index.get().is_none())
                .collect();
            for_each_document(
                cold,
                jobs,
                |(_, source)| source.source().byte_len().expect("snapshot has text"),
                |(_, source)| {
                    source.index();
                },
            );
        }
        let workspace = WorkspaceIndex::new(Self::indices(&documents))
            .index_workspace_declarations(&|| false)
            .expect("synchronous lint queries cannot be cancelled");
        for_each_document(
            work,
            jobs,
            |(source, _)| source.source().byte_len().expect("snapshot has text"),
            |(source, result)| {
                *result = source
                    .lint(&workspace, config, &|| false)
                    .expect("synchronous lint queries cannot be cancelled");
            },
        );
    }

    /// Check examples extracted from Markdown against this workspace's declarations and features.
    /// Diagnostic ranges refer to the original Markdown text.
    pub fn lint_documentation(
        &self,
        tests: &DocumentationTests,
        config: &LintConfig,
    ) -> Vec<LintDiagnostic> {
        if tests.examples.is_empty() {
            return Vec::new();
        }
        let documents = self.all_snapshots();
        let workspace = WorkspaceIndex::new(Self::indices(&documents))
            .index_workspace_declarations(&|| false)
            .expect("synchronous lint queries cannot be cancelled");
        tests
            .lint(self.features.as_deref(), &workspace, config, &|| false)
            .expect("synchronous lint queries cannot be cancelled")
    }

    /// Return static semantic warnings (using default lint preset) for `document`.
    pub fn semantic_warnings(&self, document: &str) -> Vec<navigation::SemanticWarning> {
        self.lint_document(document, &crate::lint::LintConfig::default())
            .into_iter()
            .map(|d| navigation::SemanticWarning {
                range: d.range,
                message: d.message,
            })
            .collect()
    }

    /// Search top-level symbol declarations across all indexed documents in the workspace.
    pub fn workspace_symbols(&self, query: &str) -> Vec<navigation::WorkspaceSymbol> {
        let q = query.trim().to_ascii_uppercase();
        let mut results = Vec::new();
        for (doc_name, snapshot) in self.all_snapshots() {
            for entry in snapshot.outline() {
                if q.is_empty() || entry.name.to_ascii_uppercase().contains(&q) {
                    results.push(navigation::WorkspaceSymbol {
                        document: doc_name.to_owned(),
                        name: entry.name.clone(),
                        kind: entry.kind,
                        range: entry.range,
                        selection_range: entry.selection_range,
                    });
                }
            }
        }
        results
    }

    /// Return parameter inlay hints for calls overlapping `range` in `document`.
    pub fn inlay_hints(&self, document: &str, range: TextRange) -> Vec<navigation::InlayHint> {
        let Some(source) = self.snapshot(document) else {
            return Vec::new();
        };
        let documents = self.visible_snapshots(document);
        navigation::inlay_hints(
            source.index(),
            &source.parsed().root(),
            range,
            Self::indices(&documents),
        )
    }

    /// Return quick fixes and refactoring code actions for `range` in `document`.
    pub fn code_actions(&self, document: &str, range: TextRange) -> Vec<navigation::CodeAction> {
        let config = LintConfig::from_preset(crate::lint::LintPreset::Strict);
        self.code_actions_with_config(document, range, &config)
    }

    /// Return quick fixes and refactoring code actions using `config`.
    pub fn code_actions_with_config(
        &self,
        document: &str,
        range: TextRange,
        config: &LintConfig,
    ) -> Vec<navigation::CodeAction> {
        let Some(source) = self.snapshot(document) else {
            return Vec::new();
        };
        let diagnostics = self.lint_document(document, config);
        let mut actions = source.quick_fixes(document, range, &diagnostics);
        actions.extend(self.refactor_actions(document, range));
        actions
    }

    /// Compute source refactorings without running lint rules.
    pub fn refactor_actions(
        &self,
        document: &str,
        range: TextRange,
    ) -> Vec<navigation::CodeAction> {
        let Some(source) = self.snapshot(document) else {
            return Vec::new();
        };
        let documents = self.visible_snapshots(document);
        let roots = Self::collect_roots(&documents);
        let roots: Vec<_> = roots
            .iter()
            .map(|(name, root, index)| (*name, root, *index))
            .collect();
        navigation::refactor_actions(source.index(), range, &roots)
    }

    fn snapshot_at(&self, document: &str, byte: usize) -> Option<(Arc<SourceSnapshot>, TextSize)> {
        let source = self.snapshot(document)?;
        if !source.text().is_char_boundary(byte) {
            return None;
        }
        let offset = TextSize::try_from(byte).ok()?;
        if !source.index().reader_syntax_known_at(offset) {
            return None;
        }
        Some((source, offset))
    }

    fn indices<'a>(
        documents: &'a [(&'a str, Arc<SourceSnapshot>)],
    ) -> impl Iterator<Item = (&'a str, &'a navigation::DocumentIndex)> {
        documents
            .iter()
            .map(|(name, snapshot)| (*name, snapshot.index()))
    }

    fn collect_roots<'a>(
        documents: &'a [(&'a str, Arc<SourceSnapshot>)],
    ) -> Vec<(
        &'a str,
        crate::syntax::SyntaxNode,
        &'a navigation::DocumentIndex,
    )> {
        documents
            .iter()
            .map(|(name, snap)| (*name, snap.parsed().root(), snap.index()))
            .collect()
    }

    /// Restrict cross-source queries for one document to its declared project closure.
    /// An absent restriction preserves host-managed workspace behavior.
    pub fn set_visible_sources(&mut self, document: String, sources: impl Into<Arc<[String]>>) {
        self.visible_sources.insert(document, sources.into());
    }

    fn visible_snapshots(&self, document: &str) -> Vec<(&str, Arc<SourceSnapshot>)> {
        self.snapshots(self.visible_sources.get(document).map(AsRef::as_ref))
    }

    fn snapshots<'a, Name: From<&'a str>>(
        &'a self,
        visible: Option<&'a [String]>,
    ) -> Vec<(Name, Arc<SourceSnapshot>)> {
        if let Some(visible) = visible {
            visible
                .iter()
                .filter_map(|name| {
                    let snapshot = self.snapshot(name)?;
                    Some((Name::from(name.as_str()), snapshot))
                })
                .collect()
        } else {
            self.documents
                .iter()
                .filter_map(|(name, &document)| {
                    let snapshot = document_snapshot(&self.database, document)?;
                    Some((Name::from(name.as_str()), snapshot))
                })
                .collect()
        }
    }

    fn all_snapshots(&self) -> Vec<(&str, Arc<SourceSnapshot>)> {
        self.snapshots(None)
    }
}

fn standard_symbol_at(
    source: &DocumentIndex,
    workspace: &WorkspaceIndex<'_>,
    resolver: &RefCell<OccurrenceResolver<'_, '_>>,
    atom: &AstAtom,
    name: &str,
    live_standard: &impl Fn(&SymbolName) -> Option<bool>,
) -> bool {
    let offset = atom.text_range().start();
    if !source.reader_syntax_known_at(offset) || !source.package_known_at(offset) {
        return false;
    }
    let Some(spelling) = atom.symbol().filter(|spelling| spelling.name == name) else {
        return false;
    };
    let package = source.package_at(offset).unwrap_or("COMMON-LISP-USER");
    let live = live_standard(&spelling);
    let standard = live.unwrap_or_else(|| {
        workspace.symbol_home_package(&spelling, Some(package)) == Some("COMMON-LISP")
    });
    if builtin_kind(name) == Some(BuiltinKind::Constant) {
        return standard;
    }
    if !standard {
        return false;
    }
    let may_redefine = if live.is_some() {
        source.declarations_named(name).any(|declaration| {
            declaration.name.namespace == Namespace::Function
                && live_standard(&declaration.name.spelling) == Some(true)
        })
    } else {
        workspace.may_define_callable(&spelling, package)
    };
    if may_redefine {
        return false;
    }
    !matches!(
        resolver.borrow_mut().resolve(atom.token()),
        Some(OccurrenceBinding::LocalSite { .. } | OccurrenceBinding::LocalWithoutSite)
    )
}

fn hover_constant_value(
    root: &SyntaxNode,
    range: TextRange,
    source: &SourceSnapshot,
    is_standard: &impl Fn(&AstAtom, &str) -> bool,
) -> Option<String> {
    let token = crate::ast::tokens_at_offset(root, range.start())
        .into_iter()
        .find(|token| token.text_range() == range)?;
    let form = AstList::cast(token.parent()?)?;
    if form.head()?.text_range() != range
        || !form.is_proper_closed()
        || source.diagnostics().iter().any(|diagnostic| {
            form.text_range()
                .contains_inclusive(diagnostic.range().start())
        })
        || !is_evaluated_call_position(form.syntax(), source.index().known_features())
    {
        return None;
    }
    // Decline inside an unknown macro or reader form. A list there could be
    // syntax owned by that macro rather than an evaluated subexpression.
    for ancestor in form.syntax().ancestors().skip(1) {
        if ancestor.kind() == SyntaxKind::SourceFile {
            break;
        }
        let (head, symbol) = AstList::cast(ancestor)?.head_symbol()?;
        if !is_standard(&head, &symbol.name)
            || !(builtin_kind(&symbol.name) == Some(BuiltinKind::Function)
                || matches!(
                    symbol.name.as_str(),
                    "DEFUN"
                        | "LAMBDA"
                        | "LET"
                        | "LET*"
                        | "PROGN"
                        | "IF"
                        | "WHEN"
                        | "UNLESS"
                        | "AND"
                        | "OR"
                        | "BLOCK"
                        | "LOCALLY"
                ))
        {
            return None;
        }
    }
    eval_const_form_preview_with(&form, is_standard)
}

#[cfg(test)]
mod expression_feedback_tests {
    use super::*;

    #[test]
    fn expression_feedback_uses_one_resolver_for_types_errors_and_values() {
        let mut workspace = SourceWorkspace::new();
        workspace.set_document("input", "(+ 9223372036854775808\n   1)");
        let feedback = workspace.expression_feedback_with("input", true, |_| None);
        assert_eq!(feedback.output_type, Some("integer"));
        assert_eq!(feedback.preview.as_deref(), Some("9223372036854775809"));

        let feedback = workspace.expression_feedback_with("input", false, |_| None);
        assert_eq!(feedback.output_type, Some("integer"));
        assert_eq!(feedback.preview, None);

        let feedback = workspace.expression_feedback_with("input", true, |_| Some(false));
        assert_eq!(feedback.output_type, None);
        assert_eq!(feedback.preview, None);

        workspace.set_document("input", "(+ 9223372036854775808 \"bad\")");
        let feedback = workspace.expression_feedback_with("input", true, |_| None);
        assert!(feedback.type_error.is_some());
        assert_eq!(feedback.preview, None);
    }

    #[test]
    fn live_bindings_take_precedence_over_older_source_definitions() {
        let mut workspace = SourceWorkspace::new();
        workspace.set_document("older", "(defun cl:subseq (a b c d) nil)");
        let input = "(subseq \"hello\" 1)";
        workspace.set_document("input", input);
        let live = |name: &SymbolName| Some(name.name == "SUBSEQ");
        let feedback = workspace.expression_feedback_with("input", true, live);
        assert_eq!(feedback.preview.as_deref(), Some("\"ello\""));
        let call = workspace.call_feedback_with("input", input.len() - 1, live);
        assert!(matches!(call, Some(CallFeedback::Signature(_))), "{call:?}");

        let invalid = "(cons 1 2 3)";
        workspace.set_document("input", invalid);
        let feedback = workspace.call_feedback("input", invalid.len());
        assert!(
            matches!(feedback, Some(CallFeedback::ArityError(_))),
            "{feedback:?}"
        );
    }

    #[test]
    fn constant_hover_uses_only_calls_in_known_source_contexts() {
        let mut workspace = SourceWorkspace::new();
        for (text, expected) in [
            (
                "(defun f () (+ 9223372036854775808 1))",
                Some("9223372036854775809"),
            ),
            ("(flet ((+ (a b) (list a b))) (+ 1 2))", None),
            ("(unknown-macro (+ 1 2))", None),
            ("'(+ 1 2)", None),
            ("(setq *read-base* 16)\n(+ 10 1)", None),
            ("(setq *read-suppress* t)\n(+ 10 1)", None),
            (
                "(defpackage :shadowed (:use :cl) (:shadow #:+))\n(in-package :shadowed)\n(+ 1 2)",
                None,
            ),
            (
                "(defpackage :inherited (:use :cl))\n(in-package :inherited)\n(+ 1 2)",
                Some("3"),
            ),
        ] {
            workspace.set_document("input", text);
            let hover = workspace.hover("input", text.rfind('+').unwrap());
            let contents = hover.map(|value| value.contents).unwrap_or_default();
            match expected {
                Some(result) => assert!(
                    contents.contains(&format!("Constant preview\n\n    {result}\n")),
                    "{text}: {contents}",
                ),
                None => assert!(!contents.contains("Constant preview"), "{text}: {contents}"),
            }
        }
    }
}

fn for_each_document<T: Send>(
    mut work: Vec<T>,
    limit: NonZeroUsize,
    bytes: impl Fn(&T) -> usize,
    run: impl Fn(T) + Sync,
) {
    let jobs = limit.get().min(work.len());
    if jobs <= 1 || cfg!(target_family = "wasm") {
        work.into_iter().for_each(run);
        return;
    }
    let jobs = jobs.min(work.iter().map(&bytes).sum::<usize>().div_ceil(8 * 1024));
    if jobs <= 1 {
        work.into_iter().for_each(run);
        return;
    }

    // Expensive files start early. Each request already owns its result slot,
    // so workers can finish in any order without changing the report.
    work.sort_by_cached_key(|task| std::cmp::Reverse(bytes(task)));
    let queue = Mutex::new(work.into_iter());
    let perform = || {
        let tasks = std::iter::from_fn(|| queue.lock().unwrap().next());
        tasks.for_each(&run);
    };
    thread::scope(|scope| {
        for _ in 1..jobs {
            if thread::Builder::new().spawn_scoped(scope, perform).is_err() {
                break;
            }
        }
        perform();
    });
}
