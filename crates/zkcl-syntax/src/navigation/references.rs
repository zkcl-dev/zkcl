use crate::reader_tokens::{SymbolName, integer_token};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

use super::index::{DocumentIndex, WorkspaceIndex, find_top_level_form_near};
use super::scope::is_empty_list;
use super::{
    DocumentHighlight, DocumentHighlightKind, Namespace, OccurrenceBinding, SourceLocation,
};

pub(super) type IndexedDocument<'a> = (&'a str, &'a SyntaxNode, &'a DocumentIndex);

/// A local binding or unique global declaration identified in this workspace.
pub(super) struct ReferenceTarget<'a> {
    definition_document: &'a str,
    definition_range: TextRange,
    spelling: String,
    namespace: Namespace,
    local: bool,
}

impl<'a> ReferenceTarget<'a> {
    pub(super) fn at(
        document: &'a str,
        source: &DocumentIndex,
        root: &SyntaxNode,
        offset: TextSize,
        workspace: &WorkspaceIndex<'a>,
    ) -> Option<Self> {
        match source.resolve_occurrence_at(root, offset, workspace)? {
            OccurrenceBinding::LocalSite {
                def_range,
                namespace,
                spelling,
                ..
            } => Some(Self {
                definition_document: document,
                definition_range: def_range,
                spelling: spelling.name,
                namespace,
                local: true,
            }),
            OccurrenceBinding::Global { name, .. } => {
                let (document, declaration) = workspace.lookup_unique_declaration(&name)?;
                Some(Self {
                    definition_document: document,
                    definition_range: declaration.range,
                    spelling: name.spelling.name,
                    namespace: name.namespace,
                    local: false,
                })
            }
            OccurrenceBinding::LocalWithoutSite => None,
        }
    }

    fn visit_references(
        &self,
        (document, root, index): IndexedDocument<'_>,
        workspace: &WorkspaceIndex<'_>,
        mut visit: impl FnMut(TextRange),
    ) {
        if self.local && document != self.definition_document {
            return;
        }
        let matches_nil = self.spelling == "NIL"
            && matches!(
                self.namespace,
                Namespace::Block | Namespace::ImplicitBlock | Namespace::Tag
            );
        for element in root.descendants_with_tokens().filter(|element| {
            matches!(
                element.kind(),
                SyntaxKind::Atom | SyntaxKind::String | SyntaxKind::RawString
            ) || matches_nil && is_empty_list(element)
        }) {
            let range = element.text_range();
            if !index.reader_syntax_known_at(range.start()) {
                break;
            }
            if document == self.definition_document && range == self.definition_range {
                continue;
            }
            if let Some(token) = element.as_token() {
                let spelling = if self.namespace == Namespace::IntegerTag {
                    integer_token(token.text())
                } else {
                    SymbolName::parse(token.text()).map(|name| name.name)
                };
                if !spelling.is_some_and(|name| {
                    name == self.spelling
                        || name == "RETURN"
                            && matches_nil
                            && matches!(self.namespace, Namespace::Block | Namespace::ImplicitBlock)
                }) {
                    continue;
                }
            }
            let occurrence = index.resolve_occurrence_with(range.start(), workspace, |form| {
                find_top_level_form_near(root, form, element)
            });
            let matches = match occurrence {
                Some(OccurrenceBinding::LocalSite {
                    def_range,
                    namespace,
                    occurrence_range,
                    ..
                }) => {
                    self.local
                        && occurrence_range == range
                        && def_range == self.definition_range
                        && (namespace == Namespace::IntegerTag)
                            == (self.namespace == Namespace::IntegerTag)
                }
                Some(OccurrenceBinding::Global {
                    name,
                    occurrence_range,
                    ..
                }) => {
                    !self.local
                        && occurrence_range == range
                        && workspace.lookup_unique_declaration(&name).is_some_and(
                            |(document, decl)| {
                                document == self.definition_document
                                    && decl.range == self.definition_range
                            },
                        )
                }
                _ => false,
            };
            if matches {
                visit(range);
            }
        }
    }

    pub(super) fn locations<'d>(
        &self,
        workspace: &WorkspaceIndex<'_>,
        documents: impl Iterator<Item = IndexedDocument<'d>>,
        include_declaration: bool,
    ) -> Vec<SourceLocation> {
        let mut locations = Vec::new();
        if include_declaration {
            locations.push(SourceLocation {
                document: self.definition_document.to_owned(),
                range: self.definition_range,
            });
        }
        for (document, root, index) in documents {
            self.visit_references((document, root, index), workspace, |range| {
                locations.push(SourceLocation {
                    document: document.to_owned(),
                    range,
                });
            });
        }
        locations
    }
}

pub(crate) fn references<'a>(
    document: &str,
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    include_declaration: bool,
    documents: impl Iterator<Item = IndexedDocument<'a>> + Clone,
) -> Vec<SourceLocation> {
    let workspace = WorkspaceIndex::new(documents.clone().map(|(name, _, index)| (name, index)))
        .index_workspace_declarations(&|| false)
        .expect("synchronous references query cannot be cancelled");
    let Some(target) = ReferenceTarget::at(document, source, root, offset, &workspace) else {
        return Vec::new();
    };
    target.locations(&workspace, documents, include_declaration)
}

pub(crate) fn document_highlights<'a>(
    document: &str,
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    documents: impl Iterator<Item = (&'a str, &'a DocumentIndex)>,
) -> Vec<DocumentHighlight> {
    let workspace = WorkspaceIndex::new(documents);
    let Some(target) = ReferenceTarget::at(document, source, root, offset, &workspace) else {
        return Vec::new();
    };
    let mut highlights = Vec::new();
    if document == target.definition_document {
        highlights.push(DocumentHighlight {
            range: target.definition_range,
            kind: DocumentHighlightKind::Write,
        });
    }
    target.visit_references((document, root, source), &workspace, |range| {
        highlights.push(DocumentHighlight {
            range,
            kind: DocumentHighlightKind::Read,
        });
    });
    highlights
}
