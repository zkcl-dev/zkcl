pub(crate) mod index;
pub(crate) mod queries;
mod references;
mod rename;
pub(crate) mod scope;

use std::sync::Arc;

use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::TextRange;
pub(crate) use crate::syntax::TextSize;

pub(crate) use index::DocumentIndex;
pub(crate) use queries::{
    CallQuery, call_query, completions, definition, hover, inlay_hints, refactor_actions,
    semantic_tokens,
};
pub(crate) use references::{document_highlights, references};
pub(crate) use rename::{prepare_rename, rename};

/// A definition in an indexed document and its UTF-8 name range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionLocation {
    pub document: String,
    pub selection_range: TextRange,
}

/// A source range in a named document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    pub document: String,
    pub range: TextRange,
}

/// Hover documentation and signature information for a symbol at an offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    /// UTF-8 byte range of the occurrence in the document snapshot.
    pub range: TextRange,
    /// Documentation and signature formatted as Markdown.
    pub contents: String,
}

/// Signature of the call at the cursor, including its active parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHelp {
    pub label: String,
    pub documentation: Option<String>,
    pub parameters: Vec<String>,
    pub active_parameter: Option<usize>,
}

/// Feedback for the call enclosing the cursor. An arity error takes precedence
/// over signature help, including errors in an enclosing call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallFeedback {
    ArityError(String),
    Signature(SignatureHelp),
}

/// Kind of completion candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Function,
    Macro,
    SpecialOperator,
    Variable,
    Constant,
    Type,
    Package,
    Keyword,
}

impl From<crate::cl_builtins::BuiltinKind> for CompletionKind {
    fn from(kind: crate::cl_builtins::BuiltinKind) -> Self {
        use crate::cl_builtins::BuiltinKind;
        match kind {
            BuiltinKind::Function => Self::Function,
            BuiltinKind::Macro => Self::Macro,
            BuiltinKind::SpecialOperator => Self::SpecialOperator,
            BuiltinKind::Variable => Self::Variable,
            BuiltinKind::Constant => Self::Constant,
            BuiltinKind::Type => Self::Type,
            BuiltinKind::Declaration | BuiltinKind::LambdaListKeyword => Self::Keyword,
        }
    }
}

/// A completion candidate at a cursor position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: Option<String>,
    pub documentation: Option<String>,
}

/// Kind of document highlight for an occurrence in the same document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentHighlightKind {
    Text,
    Read,
    Write,
}

/// A highlighted occurrence in a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentHighlight {
    /// UTF-8 byte range in the document snapshot.
    pub range: TextRange,
    pub kind: DocumentHighlightKind,
}

/// Semantic token classification for syntax highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SemanticTokenKind {
    Keyword = 0,
    Function = 1,
    Macro = 2,
    Variable = 3,
    Parameter = 4,
    Type = 5,
    String = 6,
    Number = 7,
    Comment = 8,
    Operator = 9,
}

/// A semantic token in a document snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticToken {
    /// UTF-8 byte range in the document snapshot.
    pub range: TextRange,
    pub kind: SemanticTokenKind,
    pub is_definition: bool,
}

/// A static semantic warning in a document snapshot (such as an unused lexical variable or call arity mismatch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticWarning {
    pub range: TextRange,
    pub message: String,
}

/// A workspace-wide symbol entry matching a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbol {
    pub document: String,
    pub name: String,
    pub kind: crate::outline::OutlineKind,
    /// UTF-8 byte range of the declaration.
    pub range: TextRange,
    /// UTF-8 byte range of the name within the declaration.
    pub selection_range: TextRange,
}

/// A parameter inlay hint at a source byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlayHint {
    pub offset: TextSize,
    pub label: String,
}

/// Kind of a code action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeActionKind {
    QuickFix,
    Refactor,
}

/// A source code action (quick fix or refactoring) with its workspace edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeAction {
    pub title: String,
    pub kind: CodeActionKind,
    pub preferred: bool,
    pub edits: Vec<(SourceLocation, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Namespace {
    Pattern,
    Function,
    SetfFunction,
    Value,
    SpecialValue,
    Type,
    Block,
    ImplicitBlock,
    Tag,
    IntegerTag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Name {
    pub(crate) namespace: Namespace,
    pub(crate) spelling: SymbolName,
    pub(crate) package: Option<String>,
}

impl Name {
    pub(crate) fn package(&self) -> Option<&str> {
        match &self.spelling.prefix {
            Prefix::None => self.package.as_deref(),
            Prefix::Package { name, .. } => Some(name),
            Prefix::Keyword | Prefix::Uninterned => None,
        }
    }
}

/// A local binding or special declaration visible in the source scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeBinding {
    pub(crate) namespace: Namespace,
    pub(crate) spelling: SymbolName,
    pub(crate) package: Option<String>,
    pub(crate) def_range: Option<TextRange>,
    pub(crate) params: Option<Arc<[String]>>,
    pub(crate) evaluates_arguments: bool,
}

/// Binding of a name occurrence in its Lisp namespace and lexical scope.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(crate) enum OccurrenceBinding {
    /// Local binding or declaration with a source position in the same document.
    LocalSite {
        def_range: TextRange,
        namespace: Namespace,
        spelling: SymbolName,
        package: Option<String>,
        params: Option<Arc<[String]>>,
        occurrence_range: TextRange,
    },
    /// Local scope blocks global lookup, but there is no static definition site.
    LocalWithoutSite,
    /// Global name reference or declaration. Its definition may be outside the index.
    Global {
        name: Name,
        occurrence_range: TextRange,
        is_declaration: bool,
    },
}
