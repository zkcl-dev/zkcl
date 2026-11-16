use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, SyncSender},
};

use lsp_types::{
    Diagnostic, DiagnosticRelatedInformation, DiagnosticSeverity, DiagnosticTag, Location,
    NumberOrString, Uri,
};
use zkcl_syntax::{
    DiagnosticSnapshot, LintConfig, SourcePositionEncoding, SourceSnapshot, WorkspaceLintChanges,
    lint::FormLintCache,
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
        data: Some(data.into()),
        ..Diagnostic::default()
    })
}

pub(crate) enum DiagnosticEvent {
    SyntaxDiagnostics(Option<Vec<Diagnostic>>),
    DocumentAnalyzed(Option<AnalyzedDocument>),
    WorkspaceWarningsChecked(Vec<Uri>),
}

#[derive(Default)]
pub(crate) struct AnalyzedDocument {
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) lint: Vec<zkcl_diagnostics::Diagnostic>,
}

pub(crate) enum DiagnosticJob {
    Analyze(DocumentAnalysis),
    CheckWorkspaceWarnings(WorkspaceChanges),
}

pub(crate) struct DocumentChange {
    pub(crate) before: Option<Arc<SourceSnapshot>>,
    pub(crate) after: Option<Arc<SourceSnapshot>>,
}

#[derive(Default)]
pub(crate) struct WorkspaceChanges {
    pub(crate) edits: Vec<DocumentChange>,
    pub(crate) candidates: Vec<(Uri, Arc<SourceSnapshot>)>,
}

pub(crate) struct DocumentAnalysis {
    pub(crate) uri: Uri,
    pub(crate) snapshot: Option<DiagnosticSnapshot>,
    pub(crate) syntax: Arc<[Diagnostic]>,
    pub(crate) lint_config: Arc<LintConfig>,
    pub(crate) form_lint_cache: Arc<Mutex<FormLintCache>>,
    pub(crate) document_changes: WorkspaceChanges,
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

pub(crate) fn run_worker(jobs: Receiver<DiagnosticJob>, events: SyncSender<ServerEvent>) {
    for job in jobs {
        let event = match job {
            DiagnosticJob::Analyze(job) => {
                // A newer edit can cancel this document job while its context
                // changes still need to invalidate other documents' warnings.
                if !job.document_changes.edits.is_empty()
                    && events
                        .send(ServerEvent::Diagnostics(check_workspace_warnings(
                            &job.document_changes,
                        )))
                        .is_err()
                {
                    break;
                }
                DiagnosticEvent::DocumentAnalyzed(job.analyze())
            }
            DiagnosticJob::CheckWorkspaceWarnings(changes) => check_workspace_warnings(&changes),
        };
        if events.send(ServerEvent::Diagnostics(event)).is_err() {
            break;
        }
    }
}

fn check_workspace_warnings(changes: &WorkspaceChanges) -> DiagnosticEvent {
    let mut inputs = WorkspaceLintChanges::default();
    for edit in &changes.edits {
        inputs.merge(WorkspaceLintChanges::between(
            edit.before.as_deref(),
            edit.after.as_deref(),
        ));
    }
    let affected = if inputs.is_empty() {
        Vec::new()
    } else {
        changes
            .candidates
            .iter()
            .filter(|(_, source)| inputs.affects(source))
            .map(|(uri, _)| uri.clone())
            .collect()
    };
    DiagnosticEvent::WorkspaceWarningsChecked(affected)
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
                        let report =
                            report_at(self.snapshot.source(), code, error.message, error.span);
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
                    let report = report_at(
                        self.snapshot.source(),
                        "doc/example",
                        diagnostic.message,
                        usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end()),
                    );
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
        let mut diagnostics = self.syntax.to_vec();
        let Some(snapshot) = &self.snapshot else {
            return Some(AnalyzedDocument {
                diagnostics,
                lint: Vec::new(),
            });
        };
        let lint_diagnostics = snapshot.lint_with_cache(
            &self.lint_config,
            cancelled,
            Some(&mut self.form_lint_cache.lock().unwrap()),
        )?;
        let lint_diagnostics: Vec<_> = lint_diagnostics
            .iter()
            .filter_map(|diagnostic| {
                diagnostic.to_diagnostic(snapshot.source().source(), |name| {
                    snapshot.document(name).map(SourceSnapshot::source)
                })
            })
            .collect();
        diagnostics.extend(
            lint_diagnostics
                .iter()
                .filter_map(|report| lsp_diagnostic(report, &self.uri, self.encoding)),
        );
        Some(AnalyzedDocument {
            diagnostics,
            lint: lint_diagnostics,
        })
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
}
