use std::{collections::HashMap, sync::Arc};

use lsp_server::{ErrorCode, Request, Response};
use lsp_types::{
    CodeAction, CodeActionKind as LspCodeActionKind, CodeActionOrCommand, CodeActionParams,
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse,
    DocumentFormattingParams, DocumentHighlight, DocumentHighlightParams, DocumentSymbol,
    DocumentSymbolParams, DocumentSymbolResponse, Documentation, FoldingRange, FoldingRangeParams,
    GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverContents, HoverParams, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintParams, Location, MarkupContent, MarkupKind,
    ParameterInformation, ParameterLabel, PrepareRenameResponse, ReferenceParams, RenameParams,
    SelectionRange, SelectionRangeParams, SemanticToken, SemanticTokens, SemanticTokensParams,
    SemanticTokensResult, SignatureHelp, SignatureHelpParams, SignatureInformation,
    SymbolInformation, SymbolKind, TextDocumentPositionParams, TextEdit, Uri, WorkspaceEdit,
    WorkspaceSymbolParams, WorkspaceSymbolResponse,
    request::{
        CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest,
        FoldingRangeRequest, Formatting, GotoDefinition, HoverRequest, InlayHintRequest,
        PrepareRenameRequest, References, Rename, Request as LspRequest, SelectionRangeRequest,
        SemanticTokensFullRequest, SignatureHelpRequest, WorkspaceSymbolRequest,
    },
};
use thiserror::Error;
use zkcl_syntax::{
    CodeActionKind, CompletionKind, DocumentHighlightKind, OutlineKind, SourceLocation,
    SourceSnapshot, syntax::TextRange,
};

use crate::Session;

#[derive(Debug, Error)]
enum RequestError {
    #[error("{0}")]
    DecodeParams(#[from] serde_json::Error),
    #[error("Cannot locate the requested source position")]
    InvalidRenamePosition,
    #[error("Cannot rename this symbol to the requested name")]
    InvalidRename,
    #[error("Cannot locate all documents affected by the rename")]
    MissingRenameDocuments,
}

pub(super) fn respond<R: LspRequest>(
    request: Request,
    handle: impl FnOnce(R::Params) -> R::Result,
) -> Response {
    try_respond::<R>(request, |params| Ok(handle(params)))
}

fn try_respond<R: LspRequest>(
    request: Request,
    handle: impl FnOnce(R::Params) -> Result<R::Result, RequestError>,
) -> Response {
    match serde_json::from_value(request.params)
        .map_err(RequestError::from)
        .and_then(handle)
    {
        Ok(result) => Response::new_ok(request.id, result),
        Err(error) => Response::new_err(
            request.id,
            ErrorCode::InvalidParams as i32,
            error.to_string(),
        ),
    }
}

struct SourceCursor<'a> {
    document: &'a str,
    snapshot: Arc<SourceSnapshot>,
    byte_offset: usize,
}

impl Session {
    pub(super) fn request(&self, request: Request) -> Response {
        match request.method.as_str() {
            DocumentSymbolRequest::METHOD => {
                respond::<DocumentSymbolRequest>(request, |params| self.document_symbols(params))
            }
            GotoDefinition::METHOD => {
                respond::<GotoDefinition>(request, |params| self.definition(params))
            }
            References::METHOD => respond::<References>(request, |params| self.references(params)),
            PrepareRenameRequest::METHOD => {
                respond::<PrepareRenameRequest>(request, |params| self.prepare_rename(params))
            }
            Rename::METHOD => try_respond::<Rename>(request, |params| self.rename(params)),
            Formatting::METHOD => respond::<Formatting>(request, |params| self.formatting(params)),
            HoverRequest::METHOD => respond::<HoverRequest>(request, |params| self.hover(params)),
            SignatureHelpRequest::METHOD => {
                respond::<SignatureHelpRequest>(request, |params| self.signature_help(params))
            }
            Completion::METHOD => respond::<Completion>(request, |params| self.completion(params)),
            DocumentHighlightRequest::METHOD => {
                respond::<DocumentHighlightRequest>(request, |params| {
                    self.document_highlight(params)
                })
            }
            FoldingRangeRequest::METHOD => {
                respond::<FoldingRangeRequest>(request, |params| self.folding_ranges(params))
            }
            SelectionRangeRequest::METHOD => {
                respond::<SelectionRangeRequest>(request, |params| self.selection_ranges(params))
            }
            SemanticTokensFullRequest::METHOD => {
                respond::<SemanticTokensFullRequest>(request, |params| self.semantic_tokens(params))
            }
            WorkspaceSymbolRequest::METHOD => {
                respond::<WorkspaceSymbolRequest>(request, |params| {
                    Some(self.workspace_symbols(params))
                })
            }
            InlayHintRequest::METHOD => {
                respond::<InlayHintRequest>(request, |params| self.inlay_hints(params))
            }
            CodeActionRequest::METHOD => {
                respond::<CodeActionRequest>(request, |params| self.code_actions(params))
            }
            method => Response::new_err(
                request.id,
                ErrorCode::MethodNotFound as i32,
                format!("Unsupported method {method}"),
            ),
        }
    }

    fn source_cursor<'a>(
        &self,
        params: &'a TextDocumentPositionParams,
    ) -> Option<SourceCursor<'a>> {
        let document = params.text_document.uri.as_str();
        let snapshot = self.workspace.snapshot(document)?;
        let byte_offset = self.coordinates(&snapshot).byte_offset(params.position)?;
        Some(SourceCursor {
            document,
            snapshot,
            byte_offset,
        })
    }

    /// Reference and outline results are grouped by document. Keep the URI and
    /// snapshot together while converting successive ranges in that document.
    fn location_mapper(&self) -> impl FnMut(&str, TextRange) -> Option<Location> + '_ {
        let mut current: Option<(Uri, Arc<SourceSnapshot>)> = None;
        move |document, range| {
            if current
                .as_ref()
                .is_none_or(|(uri, _)| uri.as_str() != document)
            {
                current = Some((document.parse().ok()?, self.workspace.snapshot(document)?));
            }
            let (uri, snapshot) = current.as_ref()?;
            Some(Location::new(
                uri.clone(),
                self.coordinates(snapshot).range(range),
            ))
        }
    }

    fn definition(&self, params: GotoDefinitionParams) -> Option<GotoDefinitionResponse> {
        let cursor = self.source_cursor(&params.text_document_position_params)?;
        let definition = self
            .workspace
            .definition(cursor.document, cursor.byte_offset)?;
        self.location_mapper()(&definition.document, definition.selection_range)
            .map(GotoDefinitionResponse::Scalar)
    }

    fn references(&self, params: ReferenceParams) -> Option<Vec<Location>> {
        let cursor = self.source_cursor(&params.text_document_position)?;
        let mut location = self.location_mapper();
        self.workspace
            .references(
                cursor.document,
                cursor.byte_offset,
                params.context.include_declaration,
            )
            .into_iter()
            .map(|loc| location(&loc.document, loc.range))
            .collect()
    }

    fn prepare_rename(&self, params: TextDocumentPositionParams) -> Option<PrepareRenameResponse> {
        let cursor = self.source_cursor(&params)?;
        let (range, placeholder) = self
            .workspace
            .prepare_rename(cursor.document, cursor.byte_offset)?;
        Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: self.coordinates(&cursor.snapshot).range(range),
            placeholder,
        })
    }

    fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>, RequestError> {
        let cursor = self
            .source_cursor(&params.text_document_position)
            .ok_or(RequestError::InvalidRenamePosition)?;
        let edits = self
            .workspace
            .rename(cursor.document, cursor.byte_offset, &params.new_name)
            .ok_or(RequestError::InvalidRename)?;
        if edits.is_empty() {
            return Ok(None);
        }
        self.workspace_edit(edits)
            .map(Some)
            .ok_or(RequestError::MissingRenameDocuments)
    }

    #[expect(
        clippy::mutable_key_type,
        reason = "lsp_types::Uri uses interior mutability only in cached fields, not Hash/Eq"
    )]
    fn workspace_edit(&self, edits: Vec<(SourceLocation, String)>) -> Option<WorkspaceEdit> {
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        let mut location = self.location_mapper();
        for (loc, new_text) in edits {
            let Location { uri, range } = location(&loc.document, loc.range)?;
            changes
                .entry(uri)
                .or_default()
                .push(TextEdit { range, new_text });
        }
        Some(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
    }

    fn formatting(&self, params: DocumentFormattingParams) -> Option<Vec<TextEdit>> {
        self.workspace
            .snapshot(params.text_document.uri.as_str())
            .map(|snapshot| {
                let coordinates = self.coordinates(&snapshot);
                snapshot
                    .format_edits()
                    .into_iter()
                    .map(|edit| TextEdit {
                        range: coordinates.range(edit.range),
                        new_text: edit.replacement,
                    })
                    .collect::<Vec<_>>()
            })
    }

    fn hover(&self, params: HoverParams) -> Option<Hover> {
        let cursor = self.source_cursor(&params.text_document_position_params)?;
        let hover = self.workspace.hover(cursor.document, cursor.byte_offset)?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: hover.contents,
            }),
            range: Some(self.coordinates(&cursor.snapshot).range(hover.range)),
        })
    }

    fn signature_help(&self, params: SignatureHelpParams) -> Option<SignatureHelp> {
        let cursor = self.source_cursor(&params.text_document_position_params)?;
        let signature = self
            .workspace
            .signature_help(cursor.document, cursor.byte_offset)?;
        let active_parameter = signature.active_parameter.map(|idx| idx as u32);
        let parameters = signature
            .parameters
            .into_iter()
            .map(|param| ParameterInformation {
                label: ParameterLabel::Simple(param),
                documentation: None,
            })
            .collect();
        Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label: signature.label,
                documentation: signature.documentation.map(Documentation::String),
                parameters: Some(parameters),
                active_parameter,
            }],
            active_signature: Some(0),
            active_parameter,
        })
    }

    fn completion(&self, params: CompletionParams) -> Option<CompletionResponse> {
        let cursor = self.source_cursor(&params.text_document_position)?;
        let Some((replace, candidates)) = self
            .workspace
            .completions_with_range(cursor.document, cursor.byte_offset)
        else {
            return Some(CompletionResponse::Array(Vec::new()));
        };
        let replace = self.coordinates(&cursor.snapshot).range(replace);
        // LSP completion edits must be on a single line.
        if replace.start.line != replace.end.line {
            return Some(CompletionResponse::Array(Vec::new()));
        }
        let items = candidates
            .into_iter()
            .map(|cand| CompletionItem {
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: replace,
                    new_text: cand.label.clone(),
                })),
                label: cand.label,
                kind: Some(completion_kind(cand.kind)),
                detail: cand.detail,
                documentation: cand.documentation.map(Documentation::String),
                ..CompletionItem::default()
            })
            .collect();
        Some(CompletionResponse::Array(items))
    }

    fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Option<Vec<DocumentHighlight>> {
        let cursor = self.source_cursor(&params.text_document_position_params)?;
        let coordinates = self.coordinates(&cursor.snapshot);
        let highlights = self
            .workspace
            .document_highlights(cursor.document, cursor.byte_offset)
            .into_iter()
            .map(|hl| DocumentHighlight {
                range: coordinates.range(hl.range),
                kind: Some(match hl.kind {
                    DocumentHighlightKind::Text => lsp_types::DocumentHighlightKind::TEXT,
                    DocumentHighlightKind::Read => lsp_types::DocumentHighlightKind::READ,
                    DocumentHighlightKind::Write => lsp_types::DocumentHighlightKind::WRITE,
                }),
            })
            .collect();
        Some(highlights)
    }

    fn folding_ranges(&self, params: FoldingRangeParams) -> Option<Vec<FoldingRange>> {
        self.workspace
            .snapshot(params.text_document.uri.as_str())
            .map(|snapshot| {
                let coordinates = self.coordinates(&snapshot);
                snapshot
                    .folding_ranges()
                    .into_iter()
                    .map(|text_range| {
                        let r = coordinates.range(text_range);
                        FoldingRange {
                            start_line: r.start.line,
                            start_character: Some(r.start.character),
                            end_line: r.end.line,
                            end_character: Some(r.end.character),
                            kind: None,
                            collapsed_text: None,
                        }
                    })
                    .collect::<Vec<_>>()
            })
    }

    fn selection_ranges(&self, params: SelectionRangeParams) -> Option<Vec<SelectionRange>> {
        let document = params.text_document.uri.as_str();
        let snapshot = self.workspace.snapshot(document)?;
        let coordinates = self.coordinates(&snapshot);
        let mut results = Vec::with_capacity(params.positions.len());
        for pos in params.positions {
            let byte_offset = coordinates.byte_offset(pos)?;
            let chain = snapshot.selection_ranges(byte_offset)?;
            let mut current: Option<SelectionRange> = None;
            for text_range in chain.into_iter().rev() {
                current = Some(SelectionRange {
                    range: coordinates.range(text_range),
                    parent: current.map(Box::new),
                });
            }
            results.push(current?);
        }
        Some(results)
    }

    fn semantic_tokens(&self, params: SemanticTokensParams) -> Option<SemanticTokensResult> {
        let document = params.text_document.uri.as_str();
        self.workspace.snapshot(document).map(|snapshot| {
            let coordinates = self.coordinates(&snapshot);
            let source_tokens = self.workspace.semantic_tokens(document);
            let mut delta_tokens = Vec::with_capacity(source_tokens.len());
            let mut previous_line = 0u32;
            let mut previous_column = 0u32;
            for token in source_tokens {
                let range = coordinates.range(token.range);
                for line in range.start.line..=range.end.line {
                    let start = if line == range.start.line {
                        range.start.character
                    } else {
                        0
                    };
                    let end = if line == range.end.line {
                        range.end.character
                    } else {
                        let text = snapshot
                            .line_text(line as usize)
                            .expect("token's line exists");
                        self.encoding.code_unit_count(text) as u32
                    };
                    if start == end {
                        continue;
                    }
                    let delta_line = line - previous_line;
                    let delta_start = if delta_line == 0 {
                        start - previous_column
                    } else {
                        start
                    };
                    previous_line = line;
                    previous_column = start;
                    delta_tokens.push(SemanticToken {
                        delta_line,
                        delta_start,
                        length: end - start,
                        token_type: token.kind as u32,
                        token_modifiers_bitset: u32::from(token.is_definition),
                    });
                }
            }
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: delta_tokens,
            })
        })
    }

    #[expect(deprecated, reason = "lsp-types retains the omitted deprecated field")]
    fn document_symbols(&self, params: DocumentSymbolParams) -> Option<DocumentSymbolResponse> {
        let uri = params.text_document.uri;
        self.workspace.snapshot(uri.as_str()).map(|snapshot| {
            let coordinates = self.coordinates(&snapshot);
            let entries = snapshot.outline();
            if self.hierarchical_symbols {
                DocumentSymbolResponse::Nested(
                    entries
                        .iter()
                        .map(|entry| DocumentSymbol {
                            name: entry.name.clone(),
                            detail: None,
                            kind: symbol_kind(entry.kind),
                            tags: None,
                            deprecated: None,
                            range: coordinates.range(entry.range),
                            selection_range: coordinates.range(entry.selection_range),
                            children: None,
                        })
                        .collect(),
                )
            } else {
                DocumentSymbolResponse::Flat(
                    entries
                        .iter()
                        .map(|entry| SymbolInformation {
                            name: entry.name.clone(),
                            kind: symbol_kind(entry.kind),
                            tags: None,
                            deprecated: None,
                            location: Location::new(uri.clone(), coordinates.range(entry.range)),
                            container_name: None,
                        })
                        .collect(),
                )
            }
        })
    }

    #[expect(deprecated, reason = "lsp-types retains the omitted deprecated field")]
    fn workspace_symbols(&self, params: WorkspaceSymbolParams) -> WorkspaceSymbolResponse {
        let matches = self.workspace.workspace_symbols(&params.query);
        let mut symbols = Vec::with_capacity(matches.len());
        let mut location = self.location_mapper();
        for item in matches {
            let Some(location) = location(&item.document, item.range) else {
                continue;
            };
            symbols.push(SymbolInformation {
                name: item.name,
                kind: symbol_kind(item.kind),
                tags: None,
                deprecated: None,
                location,
                container_name: None,
            });
        }
        WorkspaceSymbolResponse::Flat(symbols)
    }

    fn inlay_hints(&self, params: InlayHintParams) -> Option<Vec<InlayHint>> {
        let document = params.text_document.uri.as_str();
        let snapshot = self.workspace.snapshot(document)?;
        let coordinates = self.coordinates(&snapshot);
        let text_range = coordinates.text_range(params.range)?;
        let hints = self.workspace.inlay_hints(document, text_range);
        Some(
            hints
                .into_iter()
                .map(|h| InlayHint {
                    position: coordinates.position(h.offset),
                    label: InlayHintLabel::String(h.label),
                    kind: Some(InlayHintKind::PARAMETER),
                    text_edits: None,
                    tooltip: None,
                    padding_left: None,
                    padding_right: Some(true),
                    data: None,
                })
                .collect::<Vec<_>>(),
        )
    }

    #[expect(clippy::mutable_key_type, reason = "Uri caches do not affect equality")]
    fn suggestion_edit(&self, selected: &[&zkcl_diagnostics::Suggestion]) -> Option<WorkspaceEdit> {
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        for edit in selected.iter().flat_map(|suggestion| suggestion.edits()) {
            let uri: Uri = edit.source().uri()?.parse().ok()?;
            let range = crate::diagnostics::source_range(
                edit.source(),
                edit.span().range(),
                self.encoding,
            )?;
            changes.entry(uri).or_default().push(TextEdit {
                range,
                new_text: edit.replacement().to_owned(),
            });
        }
        for edits in changes.values_mut() {
            edits.sort_by_key(|edit| (edit.range.start.line, edit.range.start.character));
        }
        Some(WorkspaceEdit {
            changes: Some(changes),
            ..WorkspaceEdit::default()
        })
    }

    fn code_actions(&self, params: CodeActionParams) -> Option<Vec<CodeActionOrCommand>> {
        let uri = &params.text_document.uri;
        let document = uri.as_str();
        let snapshot = self.workspace.snapshot(document)?;
        let text_range = self.coordinates(&snapshot).text_range(params.range)?;
        let only = params.context.only.as_deref();
        let requested = |kind: &LspCodeActionKind| {
            only.is_none_or(|parents| {
                parents.iter().any(|parent| {
                    kind == parent
                        || kind
                            .as_str()
                            .strip_prefix(parent.as_str())
                            .is_some_and(|suffix| suffix.starts_with('.'))
                })
            })
        };
        let lint = self
            .open
            .get(uri)
            .and_then(|open| open.lint.as_ref().map(|reports| reports.as_slice()))
            .filter(|_| self.workspace_warnings_current(uri));
        let mut lsp_actions = Vec::new();
        if requested(&LspCodeActionKind::QUICKFIX)
            && let Some(lint) = lint
        {
            let requested_range = usize::from(text_range.start())..usize::from(text_range.end());
            for report in lint {
                let Some(primary) = report
                    .labels
                    .iter()
                    .find(|label| label.role == zkcl_diagnostics::LabelRole::Primary)
                else {
                    continue;
                };
                let range = primary.span.range();
                if range.start > requested_range.end || range.end < requested_range.start {
                    continue;
                }
                for suggestion in &report.suggestions {
                    let Some(edit) = self.suggestion_edit(&[suggestion]) else {
                        continue;
                    };
                    lsp_actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: suggestion.message.clone(),
                        kind: Some(LspCodeActionKind::QUICKFIX),
                        is_preferred: Some(
                            suggestion.applicability
                                == zkcl_diagnostics::Applicability::MachineApplicable,
                        ),
                        edit: Some(edit),
                        ..CodeAction::default()
                    }));
                }
            }
        }
        let mut actions = Vec::new();
        if requested(&LspCodeActionKind::REFACTOR) && !self.is_workspace_lint_config(uri) {
            actions.extend(self.workspace.refactor_actions(document, text_range));
        }
        lsp_actions.reserve(actions.len());
        for act in actions {
            let kind = match act.kind {
                CodeActionKind::QuickFix => LspCodeActionKind::QUICKFIX,
                CodeActionKind::Refactor => LspCodeActionKind::REFACTOR,
            };
            let Some(edit) = self.workspace_edit(act.edits) else {
                continue;
            };
            lsp_actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: act.title,
                kind: Some(kind),
                is_preferred: Some(act.preferred),
                edit: Some(edit),
                ..CodeAction::default()
            }));
        }
        if only.is_some()
            && requested(&LspCodeActionKind::SOURCE_FIX_ALL)
            && let Some(lint) = lint
        {
            // Keep each suggestion atomic and select only compatible proven fixes.
            let candidates: Vec<_> = lint
                .iter()
                .filter_map(|report| {
                    // Suggestions on one report are alternatives.
                    report.suggestions.iter().find(|suggestion| {
                        suggestion.applicability
                            == zkcl_diagnostics::Applicability::MachineApplicable
                    })
                })
                .collect();
            let selected = zkcl_diagnostics::select_suggestions(candidates);
            if !selected.is_empty()
                && let Some(edit) = self.suggestion_edit(&selected)
            {
                lsp_actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Apply all safe zkcl fixes".into(),
                    kind: Some(LspCodeActionKind::SOURCE_FIX_ALL),
                    edit: Some(edit),
                    ..CodeAction::default()
                }));
            }
        }
        Some(lsp_actions)
    }
}

fn symbol_kind(kind: OutlineKind) -> SymbolKind {
    match kind {
        OutlineKind::Function | OutlineKind::Macro => SymbolKind::FUNCTION,
        OutlineKind::Variable => SymbolKind::VARIABLE,
        OutlineKind::Constant => SymbolKind::CONSTANT,
        OutlineKind::Package => SymbolKind::PACKAGE,
        OutlineKind::System => SymbolKind::MODULE,
        OutlineKind::Type => SymbolKind::CLASS,
    }
}

#[cfg(test)]
mod diagnostic_edit_tests {
    use super::*;
    use zkcl_diagnostics::{Applicability, SourceDocument, SourceEdit, Suggestion};

    #[test]
    fn suggestion_edits_use_the_negotiated_position_encoding() {
        for (encoding, start, end) in [
            (zkcl_syntax::SourcePositionEncoding::Utf8, 7, 10),
            (zkcl_syntax::SourcePositionEncoding::Utf16, 3, 6),
            (zkcl_syntax::SourcePositionEncoding::Utf32, 2, 5),
        ] {
            let (mut session, _) = Session::new(
                serde_json::from_value(serde_json::json!({"capabilities": {}})).unwrap(),
            );
            session.encoding = encoding;
            let uri: Uri = "untitled:unicode-edits.lisp".parse().unwrap();
            let source = SourceDocument::input("雪😀old\r\n").with_uri(uri.as_str());
            let suggestion = Suggestion::new(
                "replace",
                Applicability::MachineApplicable,
                vec![SourceEdit::new(source, 7..10, "new").unwrap()],
            )
            .unwrap();
            let edit =
                serde_json::to_value(session.suggestion_edit(&[&suggestion]).unwrap()).unwrap();
            let edit = &edit["changes"][uri.as_str()][0];
            assert_eq!(edit["range"]["start"]["character"], start);
            assert_eq!(edit["range"]["end"]["character"], end);
        }
    }
}

fn completion_kind(kind: CompletionKind) -> CompletionItemKind {
    match kind {
        CompletionKind::Function => CompletionItemKind::FUNCTION,
        CompletionKind::Macro => CompletionItemKind::SNIPPET,
        CompletionKind::SpecialOperator | CompletionKind::Keyword => CompletionItemKind::KEYWORD,
        CompletionKind::Variable => CompletionItemKind::VARIABLE,
        CompletionKind::Constant => CompletionItemKind::CONSTANT,
        CompletionKind::Type => CompletionItemKind::CLASS,
        CompletionKind::Package => CompletionItemKind::MODULE,
    }
}
