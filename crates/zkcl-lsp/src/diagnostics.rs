use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, SyncSender},
};

use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag, Location,
    NumberOrString, Uri,
};
use zkcl_syntax::{
    DiagnosticSnapshot, LintConfig, SourcePositionEncoding, SourceSnapshot, WorkspaceLintChanges,
    lint::FormLintCache, syntax::parse_source,
};

use crate::ServerEvent;

/// Convert positions using the retained report source, including related files.
pub(crate) fn source_range(
    source: &zkcl_diagnostics::SourceDocument,
    range: std::ops::Range<usize>,
    encoding: SourcePositionEncoding,
) -> Option<lsp_types::Range> {
    let position = |offset| {
        let position = source.position_with_encoding(offset, encoding)?;
        Some(lsp_types::Position::new(
            position.line.try_into().ok()?,
            position.column.try_into().ok()?,
        ))
    };
    Some(lsp_types::Range::new(
        position(range.start)?,
        position(range.end)?,
    ))
}

fn lsp_diagnostic(
    report: &zkcl_diagnostics::Diagnostic,
    primary_uri: &Uri,
    encoding: SourcePositionEncoding,
) -> Option<Diagnostic> {
    use zkcl_diagnostics::{DiagnosticTag as Tag, LabelRole, NoteKind, Severity};
    let primary = report
        .labels
        .iter()
        .find(|label| label.role == LabelRole::Primary)?;
    let primary_source = report.source(primary.span.snapshot())?;
    let related_information: Vec<_> = report
        .labels
        .iter()
        .filter(|label| label.role == LabelRole::Secondary)
        .filter_map(|label| {
            let source = report.source(label.span.snapshot())?;
            let uri = if source.snapshot_id() == primary_source.snapshot_id() {
                primary_uri.clone()
            } else {
                source.uri()?.parse().ok()?
            };
            Some(DiagnosticRelatedInformation {
                location: Location {
                    uri,
                    range: source_range(source, label.span.range(), encoding)?,
                },
                message: label.message.clone(),
            })
        })
        .collect();
    let tags: Vec<_> = report
        .tags
        .iter()
        .map(|tag| match tag {
            Tag::Unnecessary => DiagnosticTag::UNNECESSARY,
            Tag::Deprecated => DiagnosticTag::DEPRECATED,
        })
        .collect();
    let mut data = serde_json::Map::new();
    if !primary.message.is_empty() {
        data.insert("label".into(), primary.message.clone().into());
    }
    if let Some(help) = report.notes.iter().find(|note| note.kind == NoteKind::Help) {
        data.insert("help".into(), help.message.clone().into());
    }
    if let Some(rationale) = &report.rationale {
        data.insert("rationale".into(), rationale.clone().into());
    }
    if let Some(evidence) = &report.evidence {
        data.insert("evidence".into(), evidence.clone().into());
    }
    if !report.notes.is_empty() {
        data.insert("notes".into(), report.notes.iter().map(|note| serde_json::json!({
            "kind": match note.kind { NoteKind::Note => "note", NoteKind::Help => "help" },
            "message": note.message,
        })).collect());
    }
    if !report.contexts.is_empty() {
        data.insert(
            "contexts".into(),
            report
                .contexts
                .iter()
                .map(|context| {
                    serde_json::json!({
                        "message": context.message,
                        "path": context.path,
                    })
                })
                .collect(),
        );
    }
    Some(Diagnostic {
        range: source_range(primary_source, primary.span.range(), encoding)?,
        severity: Some(match report.severity {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Note => DiagnosticSeverity::INFORMATION,
            Severity::Help => DiagnosticSeverity::HINT,
        }),
        code: report.code.clone().map(NumberOrString::String),
        source: Some("zkcl".into()),
        message: report.message.clone(),
        tags: (!tags.is_empty()).then_some(tags),
        related_information: (!related_information.is_empty()).then_some(related_information),
        data: (!data.is_empty()).then(|| data.into()),
        ..Diagnostic::default()
    })
}

pub(crate) enum DiagnosticEvent {
    SyntaxDiagnostics(Option<Vec<Diagnostic>>),
    DocumentAnalyzed(Option<AnalyzedDocument>),
    WorkspaceWarningsChecked(Vec<WarningCandidate>),
}

#[derive(Default)]
pub(crate) struct AnalyzedDocument {
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) lint: Vec<zkcl_diagnostics::Diagnostic>,
}

pub(crate) struct DocumentChange {
    pub(crate) name: String,
    pub(crate) before: Option<Arc<SourceSnapshot>>,
    pub(crate) after: Option<Arc<SourceSnapshot>>,
}

pub(crate) struct WorkspaceChanges {
    pub(crate) edits: Vec<DocumentChange>,
    pub(crate) candidates: Vec<WarningCandidate>,
}

pub(crate) struct WarningCandidate {
    pub(crate) uri: Uri,
    pub(crate) source: Arc<SourceSnapshot>,
    pub(crate) visible_sources: Option<Arc<[String]>>,
    pub(crate) lint: Weak<Vec<zkcl_diagnostics::Diagnostic>>,
    pub(crate) form_lint_cache: Arc<Mutex<FormLintCache>>,
}

impl WarningCandidate {
    fn sees_change(&self, name: &str) -> bool {
        name != self.uri.as_str() && sees_source(self.visible_sources.as_deref(), name)
    }

    fn affected_by(&self, name: &str, inputs: &WorkspaceLintChanges) -> bool {
        if !self.sees_change(name) {
            return false;
        }
        if inputs.affects(&self.source) {
            return true;
        }
        // If another analysis owns the cache, let the publication check
        // reject a stale candidate instead of waiting for that analysis.
        self.form_lint_cache.try_lock().map_or(true, |cache| {
            cache
                .source_macro_dependencies()
                .is_none_or(|dependencies| inputs.affects_macro_dependencies(dependencies))
        })
    }
}

pub(crate) fn sees_source(visible: Option<&[String]>, name: &str) -> bool {
    visible.is_none_or(|sources| sources.iter().any(|source| source == name))
}

pub(crate) struct DocumentAnalysis {
    pub(crate) uri: Uri,
    pub(crate) snapshot: Option<DiagnosticSnapshot>,
    pub(crate) syntax: Arc<[Diagnostic]>,
    pub(crate) lint_config: Arc<LintConfig>,
    pub(crate) form_lint_cache: Arc<Mutex<FormLintCache>>,
    pub(crate) encoding: SourcePositionEncoding,
    pub(crate) cancelled: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
pub(crate) enum ProjectMetadataKind {
    Manifest,
    Lockfile,
}

pub(crate) struct SyntaxAnalysis {
    pub(crate) project_metadata: Option<ProjectMetadataKind>,
    pub(crate) snapshot: Arc<SourceSnapshot>,
    pub(crate) encoding: SourcePositionEncoding,
    pub(crate) cancelled: Arc<AtomicBool>,
}

pub(crate) fn run_syntax_worker(jobs: Receiver<SyntaxAnalysis>, events: SyncSender<ServerEvent>) {
    for job in jobs {
        let event = DiagnosticEvent::SyntaxDiagnostics(job.diagnostics());
        if events.send(ServerEvent::Diagnostics(event)).is_err() {
            break;
        }
    }
}

pub(crate) fn run_worker(jobs: Receiver<DocumentAnalysis>, events: SyncSender<ServerEvent>) {
    for job in jobs {
        let event = DiagnosticEvent::DocumentAnalyzed(job.analyze());
        if events.send(ServerEvent::Diagnostics(event)).is_err() {
            break;
        }
    }
}

pub(crate) fn run_workspace_warning_worker(
    jobs: Receiver<WorkspaceChanges>,
    events: SyncSender<ServerEvent>,
) {
    for changes in jobs {
        let event = DiagnosticEvent::WorkspaceWarningsChecked(check_workspace_warnings(changes));
        if events.send(ServerEvent::Diagnostics(event)).is_err() {
            break;
        }
    }
}

fn check_workspace_warnings(changes: WorkspaceChanges) -> Vec<WarningCandidate> {
    let WorkspaceChanges {
        edits,
        mut candidates,
    } = changes;
    candidates.retain(|candidate| candidate.lint.strong_count() != 0);
    let edits: Vec<_> = edits
        .into_iter()
        .filter_map(|edit| {
            if !candidates.iter().any(|candidate| candidate.sees_change(&edit.name)) {
                return None;
            }
            let inputs =
                WorkspaceLintChanges::between(edit.before.as_deref(), edit.after.as_deref());
            (!inputs.is_empty()).then_some((edit.name, inputs))
        })
        .collect();
    candidates
        .into_iter()
        .filter(|candidate| {
            candidate.lint.strong_count() != 0
                && edits.iter().any(|(name, inputs)| candidate.affected_by(name, inputs))
        })
        .collect()
}

impl SyntaxAnalysis {
    fn diagnostics(&self) -> Option<Vec<Diagnostic>> {
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let primary_uri: Uri = self.snapshot.source().uri()?.parse().ok()?;
        if let Some(kind) = self.project_metadata {
            let (result, code) = match kind {
                ProjectMetadataKind::Manifest => (
                    zkcl_project::manifest::parse_manifest(self.snapshot.text()).map(|_| ()),
                    "project/manifest",
                ),
                ProjectMetadataKind::Lockfile => (
                    zkcl_project::lockfile::parse_lockfile(self.snapshot.text()).map(|_| ()),
                    "project/lockfile",
                ),
            };
            return Some(
                result
                    .err()
                    .and_then(|error| {
                        let source = self.snapshot.source();
                        // Project metadata uses the standard initial readtable.
                        let syntax = parse_source(self.snapshot.text());
                        let report = match syntax.primary_diagnostic() {
                            Some(diagnostic) => {
                                let mut report = diagnostic.to_diagnostic(source);
                                report.code = Some(code.into());
                                report
                            }
                            None => report_at(source, code, error.message, error.span),
                        };
                        lsp_diagnostic(&report, &primary_uri, self.encoding)
                    })
                    .into_iter()
                    .collect(),
            );
        }
        let mut diagnostics = self
            .snapshot
            .diagnostics()
            .iter()
            .filter_map(|diagnostic| {
                lsp_diagnostic(
                    &diagnostic.to_diagnostic(self.snapshot.source()),
                    &primary_uri,
                    self.encoding,
                )
            })
            .collect::<Vec<_>>();
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        diagnostics.extend(
            self.snapshot
                .documentation_tests()
                .diagnostics
                .iter()
                .filter_map(|diagnostic| {
                    let report = diagnostic.to_diagnostic(self.snapshot.source());
                    lsp_diagnostic(&report, &primary_uri, self.encoding)
                }),
        );

        Some(diagnostics)
    }
}

impl DocumentAnalysis {
    fn analyze(&self) -> Option<AnalyzedDocument> {
        let cancelled = || self.cancelled.load(Ordering::Relaxed);
        if cancelled() {
            return None;
        }
        let Some(snapshot) = &self.snapshot else {
            return Some(AnalyzedDocument {
                diagnostics: self.syntax.to_vec(),
                lint: Vec::new(),
            });
        };
        let lint_diagnostics = snapshot.lint_with_cache(
            &self.lint_config,
            &cancelled,
            Some(&mut self.form_lint_cache.lock().unwrap()),
        )?;
        if cancelled() {
            return None;
        }
        let mut diagnostics = self.syntax.to_vec();
        let mut lint = Vec::new();
        let source = snapshot.source().source();
        for diagnostic in lint_diagnostics {
            if cancelled() {
                return None;
            }
            let Some(report) = diagnostic.to_diagnostic(source, |name| {
                snapshot.document(name).map(SourceSnapshot::source)
            }) else {
                continue;
            };
            diagnostics.extend(lsp_diagnostic(&report, &self.uri, self.encoding));
            lint.push(report);
        }
        Some(AnalyzedDocument { diagnostics, lint })
    }
}

fn report_at(
    source: &zkcl_diagnostics::SourceDocument,
    code: &str,
    message: impl Into<String>,
    range: std::ops::Range<usize>,
) -> zkcl_diagnostics::Diagnostic {
    let mut report = zkcl_diagnostics::Diagnostic::new(zkcl_diagnostics::Severity::Error, message);
    report.code = Some(code.into());
    report
        .add_label(source, range, zkcl_diagnostics::LabelRole::Primary, "")
        .expect("syntax analysis error belongs to its retained source");
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use zkcl_syntax::SourceWorkspace;

    #[test]
    fn incomplete_project_metadata_keeps_parser_help() {
        let mut workspace = SourceWorkspace::new();
        for (name, text, kind, code) in [
            (
                "file:///project/zkcl.sexp",
                "(project",
                ProjectMetadataKind::Manifest,
                "project/manifest",
            ),
            (
                "file:///project/zkcl.lock",
                "(lockfile",
                ProjectMetadataKind::Lockfile,
                "project/lockfile",
            ),
        ] {
            workspace.set_document(name, text);
            let diagnostics = SyntaxAnalysis {
                project_metadata: Some(kind),
                snapshot: workspace.snapshot(name).unwrap(),
                encoding: SourcePositionEncoding::Utf16,
                cancelled: Arc::new(AtomicBool::new(false)),
            }
            .diagnostics()
            .unwrap();
            let [diagnostic] = diagnostics.as_slice() else {
                panic!("expected one diagnostic, got {diagnostics:?}");
            };
            assert_eq!(diagnostic.code, Some(NumberOrString::String(code.into())));
            let data = diagnostic.data.as_ref().unwrap();
            assert_eq!(data["label"], "this form starts here");
            assert_eq!(data["help"], "add a closing `)` to finish the form");
        }
    }

    #[test]
    fn related_definition_positions_use_captured_sources_after_definition_moves() {
        let mut workspace = SourceWorkspace::new();
        let caller = "untitled:caller.lisp";
        let definition = "untitled:definition.lisp";
        workspace.set_document(
            definition,
            "(defun count-items (first second) (list first second))",
        );
        workspace.set_document(caller, "(list \"😀\" (count-items 1))");
        let captured = workspace.diagnostic_snapshot(caller).unwrap();
        workspace.set_document(
            definition,
            "\n\n(defun count-items (first second) (list first second))",
        );
        let caller_source = captured.source().source();
        let definition_source = captured.document(definition).unwrap().source();
        let mut report =
            zkcl_diagnostics::Diagnostic::new(zkcl_diagnostics::Severity::Error, "arity mismatch");
        report
            .add_label(
                caller_source,
                13..14,
                zkcl_diagnostics::LabelRole::Primary,
                "call site",
            )
            .unwrap();
        report
            .add_label(
                definition_source,
                0..8,
                zkcl_diagnostics::LabelRole::Secondary,
                "definition",
            )
            .unwrap();
        let published = lsp_diagnostic(
            &report,
            &caller.parse().unwrap(),
            SourcePositionEncoding::Utf16,
        )
        .unwrap();
        assert_eq!(published.range.start.line, 0);
        let related = published.related_information.unwrap();
        assert_eq!(related[0].location.uri.as_str(), definition);
        assert_eq!(related[0].location.range.start.line, 0);
        assert_eq!(related[0].location.range.start.character, 0);
    }

    #[test]
    fn absent_form_cache_dependencies_are_unknown_not_verified_empty() {
        let mut workspace = SourceWorkspace::new();
        let caller = "untitled:caller.lisp";
        let dependency = "untitled:dependency.lisp";
        workspace.set_document(caller, "(defun caller () 1)");
        workspace.set_document(dependency, "");
        let before_dependency = workspace.snapshot(dependency).unwrap();
        let mut cache = FormLintCache::default();
        workspace
            .diagnostic_snapshot(caller)
            .unwrap()
            .lint_with_cache(&LintConfig::default(), || false, Some(&mut cache))
            .unwrap();

        workspace.set_document(dependency, "(defun unrelated () 1)");
        let after_dependency = workspace.snapshot(dependency).unwrap();
        let caller = workspace.snapshot(caller).unwrap();
        let changes =
            WorkspaceLintChanges::between(Some(&before_dependency), Some(&after_dependency));
        assert!(!changes.affects(&caller));
        let published_lint = Arc::new(Vec::new());
        let request = |form_lint_cache| WorkspaceChanges {
            edits: vec![DocumentChange {
                name: "untitled:dependency.lisp".into(),
                before: Some(before_dependency.clone()),
                after: Some(after_dependency.clone()),
            }],
            candidates: vec![WarningCandidate {
                uri: caller.source().uri().unwrap().parse().unwrap(),
                source: Arc::clone(&caller),
                visible_sources: None,
                lint: Arc::downgrade(&published_lint),
                form_lint_cache,
            }],
        };

        let cache = Arc::new(Mutex::new(cache));
        assert!(
            check_workspace_warnings(request(Arc::clone(&cache))).is_empty(),
            "a populated cache with no macro dependencies is an exact negative"
        );
        let _busy = cache.lock().unwrap();
        assert_eq!(
            check_workspace_warnings(request(Arc::clone(&cache))).len(),
            1,
            "a background check can proceed while the cache is busy"
        );
        assert_eq!(
            check_workspace_warnings(request(Arc::default())).len(),
            1,
            "a missing cache must fall back to conservative invalidation"
        );
    }

    #[test]
    fn macro_helper_changes_requeue_callers_that_only_name_the_macro() {
        let mut workspace = SourceWorkspace::new();
        let macros = "untitled:macros.lisp";
        let dependency = "untitled:dependency.lisp";
        let caller = "untitled:caller.lisp";
        workspace.set_document(
            "untitled:packages.lisp",
            "(defpackage :macro-helper-probe (:use :cl) (:export :checked))",
        );
        workspace.set_document(
            macros,
            "(in-package :macro-helper-probe)\n\
             (defmacro checked (form) `(progn ,form :done))",
        );
        workspace.set_document(
            dependency,
            "(in-package :macro-helper-probe)\n\
             (defun progn (form) form)",
        );
        workspace.set_document(
            caller,
            "(in-package :macro-helper-probe)\n\
             (defun caller (items) (checked (mapcar #'print items)))",
        );

        let config = LintConfig::default();
        let mut form_lint_cache = FormLintCache::default();
        let before = workspace
            .diagnostic_snapshot(caller)
            .unwrap()
            .lint_with_cache(&config, || false, Some(&mut form_lint_cache))
            .unwrap();
        assert!(
            before
                .iter()
                .all(|diagnostic| diagnostic.rule_id != "perf/mapcar-result-discarded")
        );

        let before_dependency = workspace.snapshot(dependency).unwrap();
        workspace.set_document(dependency, "");
        let after_dependency = workspace.snapshot(dependency).unwrap();
        let after_caller = workspace.snapshot(caller).unwrap();
        let changes =
            WorkspaceLintChanges::between(Some(&before_dependency), Some(&after_dependency));
        assert!(!changes.affects(&after_caller));

        let form_lint_cache = Arc::new(Mutex::new(form_lint_cache));
        let published_lint = Arc::new(Vec::new());
        let request = WorkspaceChanges {
            edits: vec![DocumentChange {
                name: dependency.to_owned(),
                before: Some(before_dependency),
                after: Some(after_dependency),
            }],
            candidates: vec![WarningCandidate {
                uri: caller.parse().unwrap(),
                source: Arc::clone(&after_caller),
                visible_sources: None,
                lint: Arc::downgrade(&published_lint),
                form_lint_cache: Arc::clone(&form_lint_cache),
            }],
        };
        let affected = check_workspace_warnings(request);
        assert_eq!(affected.len(), 1);
        assert_eq!(affected[0].uri.as_str(), caller);

        let after = workspace
            .diagnostic_snapshot(caller)
            .unwrap()
            .lint_with_cache(
                &config,
                || false,
                Some(&mut form_lint_cache.lock().unwrap()),
            )
            .unwrap();
        assert!(
            after
                .iter()
                .any(|diagnostic| diagnostic.rule_id == "perf/mapcar-result-discarded")
        );
    }
}
