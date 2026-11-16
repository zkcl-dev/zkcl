use std::{collections::BTreeMap, error::Error, path::PathBuf};

use serde_json::{Value, json};
use zkcl::project::{ProjectMetadataFileError, delivery::DeliveryError};
use zkcl_diagnostics::{
    Applicability, Diagnostic, DiagnosticContext, DiagnosticTag, LabelRole, Note, NoteKind,
    Severity, SourceDocument, SourceOrigin, SourceSpan, Suggestion,
};

pub(crate) fn from_error(prefix: &str, error: &(dyn Error + 'static)) -> Diagnostic {
    let mut report = error_report(error);
    if !prefix.is_empty() {
        report.contexts.insert(
            0,
            DiagnosticContext {
                message: prefix.trim().trim_end_matches(':').into(),
                path: None,
            },
        );
    }
    report
}

fn error_report(error: &(dyn Error + 'static)) -> Diagnostic {
    if let Some(error) = error.downcast_ref::<crate::project::TargetSelectionError>() {
        return error.to_diagnostic();
    }
    if let Some(error) = error.downcast_ref::<zkcl::EvalError>() {
        return evaluation_report(error);
    }
    if let Some(error) = error.downcast_ref::<ProjectMetadataFileError>() {
        return match error {
            ProjectMetadataFileError::Parse { document, source } => {
                let mut report = error_with_code("project/metadata", &source.message);
                report
                    .add_label(document, source.span.clone(), LabelRole::Primary, "")
                    .expect("project parser locates errors in the parsed text");
                report
            }
            ProjectMetadataFileError::Read { path, source } => {
                let mut report = error_with_code("project/read", "cannot read project metadata");
                context(&mut report, source.to_string(), Some(path.clone()));
                report
            }
        };
    }
    if let Some(error) = error.downcast_ref::<DeliveryError>() {
        return match error {
            DeliveryError::Compile { path, source } => {
                let mut report = evaluation_report(source);
                context(
                    &mut report,
                    "while compiling project source",
                    Some(path.clone()),
                );
                report
            }
            DeliveryError::Load { path, source } => {
                let mut report = evaluation_report(source);
                if report
                    .labels
                    .iter()
                    .any(|label| label.role == LabelRole::Primary)
                {
                    // The source location is more useful than the compiled artifact here.
                    report
                        .contexts
                        .retain(|entry| entry.path.as_ref() != Some(path));
                } else if !report
                    .contexts
                    .iter()
                    .any(|entry| entry.path.as_ref() == Some(path))
                {
                    context(
                        &mut report,
                        "while loading compiled project component",
                        Some(path.clone()),
                    );
                }
                report
            }
            DeliveryError::Io {
                action,
                path,
                source,
            } => {
                let mut report =
                    error_with_code("project/io", format!("cannot {action} project file"));
                context(&mut report, source.to_string(), Some(path.clone()));
                report
            }
            DeliveryError::Aot { path, source } => {
                let mut report = error_report(source);
                context(
                    &mut report,
                    "while emitting native project code",
                    Some(path.clone()),
                );
                report
            }
        };
    }
    if let Some(error) = error.downcast_ref::<crate::wasm::RunError>() {
        return match error {
            crate::wasm::RunError::Engine(source) => error_report(source),
            crate::wasm::RunError::Read { path, source } => {
                let mut report = error_with_code("wasm/read", "cannot read Wasm module");
                context(&mut report, source.to_string(), Some(path.clone()));
                report
            }
            _ => error_with_code("wasm/run", error.to_string()),
        };
    }
    if let Some(zkcl_wasm::Error::Execution {
        frame,
        callers,
        source,
    }) = error.downcast_ref()
    {
        let mut report = error_with_code("wasm/execution", source.to_string());
        for frame in std::iter::once(frame).chain(callers.iter()) {
            context(&mut report, format!("in {frame}"), None);
        }
        return report;
    }
    let message = error.to_string();
    let Some(cause) = error.source() else {
        return error_with_code("cli/error", message);
    };
    let mut report = error_report(cause);
    if message != report.message && !report.notes.iter().any(|note| note.message == message) {
        context(&mut report, message, None);
    }
    report
}

fn error_with_code(code: &str, message: impl Into<String>) -> Diagnostic {
    let mut report = Diagnostic::new(Severity::Error, message);
    report.code = Some(code.into());
    report
}

fn context(report: &mut Diagnostic, message: impl Into<String>, path: Option<PathBuf>) {
    report.contexts.push(DiagnosticContext {
        message: message.into(),
        path,
    });
}

fn evaluation_report(error: &zkcl::EvalError) -> Diagnostic {
    let mut report = error.to_diagnostic();
    let mut frames = error
        .stack_frames()
        .iter()
        .filter(|frame| frame.function.is_some());
    for frame in frames.by_ref().take(8) {
        let function = frame.function.as_deref().expect("named stack frame");
        if let (Some(source), Some(range)) = (&frame.source, &frame.source_range)
            && let Ok(span) = source.span(range.clone())
        {
            if let Some(label) = report.labels.iter_mut().find(|label| label.span == span) {
                if label.role == LabelRole::Primary && label.message.is_empty() {
                    label.message = format!("in {function}");
                }
            } else {
                report
                    .add_label(
                        source,
                        range.clone(),
                        LabelRole::Secondary,
                        format!("in {function}"),
                    )
                    .expect("frame location belongs to its source");
            }
        } else {
            context(
                &mut report,
                format!("in {function}"),
                frame.source_path.clone(),
            );
        }
    }
    let remaining = frames.count();
    if remaining > 0 {
        report.notes.push(Note {
            kind: NoteKind::Note,
            message: format!("{remaining} more Lisp frames"),
        });
    }
    report
}

fn span_json(span: SourceSpan, source: Option<&SourceDocument>) -> Value {
    let range = span.range();
    let position = |offset| {
        source
            .and_then(|source| source.position(offset))
            .map(|position| serde_json::json!({"line": position.line, "column": position.column}))
    };
    let start = position(range.start);
    let end = position(range.end);
    let positions = start
        .zip(end)
        .map(|(start, end)| serde_json::json!({"start": start, "end": end}));
    serde_json::json!({
        "snapshot_id": span.snapshot().get(),
        "byte_range": {"start": range.start, "end": range.end},
        "range": positions,
    })
}

/// Positions use zero-based Unicode scalar columns and UTF-8 byte offsets.
/// Each report batch carries one copy of each captured source.
pub(crate) fn batch_json(diagnostics: &[Diagnostic]) -> Value {
    json!({
        "position_encoding": POSITION_ENCODING,
        "sources": sources_json(diagnostics, []),
        "diagnostics": diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
    })
}

pub(crate) fn report_json<'a>(
    diagnostic: &'a Diagnostic,
    extra_sources: impl IntoIterator<Item = &'a SourceDocument>,
) -> Value {
    let mut report = diagnostic_json(diagnostic);
    report["position_encoding"] = json!(POSITION_ENCODING);
    report["sources"] = sources_json(std::slice::from_ref(diagnostic), extra_sources);
    report
}

const POSITION_ENCODING: &str = "utf-32";

fn sources_json<'a>(
    diagnostics: &'a [Diagnostic],
    extra_sources: impl IntoIterator<Item = &'a SourceDocument>,
) -> Value {
    let sources = diagnostics
        .iter()
        .flat_map(|diagnostic| {
            diagnostic.sources.iter().chain(
                diagnostic
                    .suggestions
                    .iter()
                    .flat_map(Suggestion::edits)
                    .map(|edit| edit.source()),
            )
        })
        .chain(extra_sources)
        .map(|source| (source.snapshot_id(), source))
        .collect::<BTreeMap<_, _>>();
    Value::Array(
        sources
            .into_values()
            .map(|source| {
                let mut value = source_metadata_json(source);
                value["text"] = json!(source.text());
                value
            })
            .collect(),
    )
}

pub(crate) fn source_metadata_json(source: &SourceDocument) -> Value {
    json!({
        "snapshot_id": source.snapshot_id().get(),
        "path": source.path().map(|path| path.to_string_lossy()),
        "uri": source.uri(),
        "version": source.version(),
        "base_offset": source.base_offset(),
    })
}

fn diagnostic_json(diagnostic: &Diagnostic) -> Value {
    let labels: Vec<_> = diagnostic
        .labels
        .iter()
        .map(|label| {
            let mut value = span_json(label.span, diagnostic.source(label.span.snapshot()));
            value["role"] = json!(match label.role {
                LabelRole::Primary => "primary",
                LabelRole::Secondary => "secondary",
            });
            value["message"] = json!(label.message);
            if let Some(provenance) = &label.provenance {
                value["provenance"] = json!({
                    "origin": match provenance.origin {
                        SourceOrigin::Original => "original",
                        SourceOrigin::Reader => "reader",
                        SourceOrigin::Macro => "macro",
                        SourceOrigin::Compiler => "compiler",
                    },
                    "expansion": provenance.expansion.iter().map(|&span| {
                        span_json(span, diagnostic.source(span.snapshot()))
                    }).collect::<Vec<_>>(),
                });
            }
            value
        })
        .collect();
    let notes: Vec<_> = diagnostic
        .notes
        .iter()
        .map(|note| {
            json!({
                "kind": match note.kind { NoteKind::Note => "note", NoteKind::Help => "help" },
                "message": note.message,
            })
        })
        .collect();
    let suggestions: Vec<_> = diagnostic
        .suggestions
        .iter()
        .map(|suggestion| {
            let edits: Vec<_> = suggestion
                .edits()
                .iter()
                .map(|edit| {
                    let mut value = span_json(edit.span(), Some(edit.source()));
                    value["expected"] = json!(edit.expected());
                    value["replacement"] = json!(edit.replacement());
                    value
                })
                .collect();
            json!({
                "message": suggestion.message,
                "applicability": match suggestion.applicability {
                    Applicability::MachineApplicable => "machine-applicable",
                    Applicability::MaybeIncorrect => "maybe-incorrect",
                    Applicability::HasPlaceholders => "has-placeholders",
                    Applicability::Unspecified => "unspecified",
                },
                "edits": edits,
            })
        })
        .collect();
    json!({
        "code": diagnostic.code,
        "severity": match diagnostic.severity {
            Severity::Error => "error", Severity::Warning => "warning",
            Severity::Note => "note", Severity::Help => "help",
        },
        "message": diagnostic.message,
        "tags": diagnostic.tags.iter().map(|tag| match tag {
            DiagnosticTag::Unnecessary => "unnecessary",
            DiagnosticTag::Deprecated => "deprecated",
        }).collect::<Vec<_>>(),
        "rationale": diagnostic.rationale,
        "evidence": diagnostic.evidence,
        "labels": labels,
        "notes": notes,
        "contexts": diagnostic.contexts.iter().map(|context| json!({
            "message": context.message,
            "path": context.path.as_ref().map(|path| path.to_string_lossy()),
        })).collect::<Vec<_>>(),
        "suggestions": suggestions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zkcl_diagnostics::SourceEdit;

    #[test]
    fn json_preserves_snapshot_text_and_edit_preimages_without_terminal_styling() {
        let source = SourceDocument::input("雪😀\r\nold").with_version(7);
        let mut diagnostic = Diagnostic::new(Severity::Warning, "change");
        diagnostic
            .add_label(&source, 3..7, LabelRole::Primary, "emoji")
            .unwrap();
        diagnostic.suggestions.push(
            Suggestion::new(
                "replace",
                Applicability::MachineApplicable,
                vec![SourceEdit::new(source.clone(), 9..12, "new").unwrap()],
            )
            .unwrap(),
        );
        let batch = batch_json(&[diagnostic.clone(), diagnostic]);
        assert_eq!(batch["sources"].as_array().unwrap().len(), 1);
        assert_eq!(batch["sources"][0]["text"], source.text().unwrap());
        assert_eq!(batch["sources"][0]["version"], 7);
        let value = &batch["diagnostics"][0];
        assert_eq!(value["labels"][0]["byte_range"]["start"], 3);
        assert_eq!(value["labels"][0]["range"]["start"]["column"], 1);
        assert_eq!(value["labels"][0]["range"]["end"]["column"], 2);
        assert_eq!(value["suggestions"][0]["edits"][0]["expected"], "old");
        assert_eq!(value["suggestions"][0]["edits"][0]["replacement"], "new");
        assert_eq!(
            value["suggestions"][0]["applicability"],
            "machine-applicable"
        );
    }
}
