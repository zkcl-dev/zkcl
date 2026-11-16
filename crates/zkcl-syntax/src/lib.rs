//! Lossless Common Lisp syntax and incremental source snapshots.

pub mod ast;
pub mod character_names;
#[cfg_attr(not(feature = "analysis"), allow(dead_code))]
pub mod cl_builtins;
#[cfg(feature = "analysis")]
pub mod documentation;
#[cfg(feature = "analysis")]
pub mod editing;
#[cfg(feature = "analysis")]
mod expression_feedback;
#[cfg_attr(not(feature = "analysis"), allow(dead_code))]
pub mod foreign;
#[cfg(feature = "analysis")]
pub mod format;
#[cfg(feature = "analysis")]
pub mod lint;
#[cfg(feature = "analysis")]
mod navigation;
#[cfg_attr(not(feature = "analysis"), allow(dead_code))]
mod outline;
#[cfg_attr(not(feature = "analysis"), allow(dead_code))]
mod reader_tokens;
pub mod simd_builtins;
#[cfg(feature = "analysis")]
mod source_workspace;
pub mod syntax;
pub mod treelist_builtins;

pub use ast::{
    eval_const_preview, matching_parens_at, should_auto_pair_at, token_at_offset, tokens_at_offset,
};
#[cfg(feature = "analysis")]
pub use expression_feedback::ExpressionTypeError;
#[cfg(feature = "analysis")]
pub use format::{TextEdit, format_edits, format_source};
#[cfg(feature = "analysis")]
pub use lint::{
    FixApplicability, LintCategory, LintConfig, LintConfigError, LintDiagnostic, LintFix,
    LintLevel, LintPreset, LintRelated, LintTag, MacroShape, RuleDescriptor, RuleEvidence,
    all_rules, apply_fixes, find_rule,
};
#[cfg(feature = "analysis")]
pub use navigation::{
    CallFeedback, CodeAction, CodeActionKind, CompletionCandidate, CompletionKind,
    DefinitionLocation, DocumentHighlight, DocumentHighlightKind, Hover, InlayHint, SemanticToken,
    SemanticTokenKind, SemanticWarning, SignatureHelp, SourceLocation, WorkspaceSymbol,
};
pub use outline::{OutlineEntry, OutlineKind};
pub use reader_tokens::{NumberLiteralKind, Prefix, SymbolName, number_literal_kind};
#[cfg(feature = "analysis")]
pub use source_workspace::ExpressionFeedback;
#[cfg(feature = "analysis")]
pub use source_workspace::{DiagnosticSnapshot, WorkspaceLintChanges};
#[cfg(feature = "analysis")]
pub use source_workspace::WorkspaceRevision;
#[cfg(feature = "analysis")]
pub use source_workspace::{SourceEditError, SourceSnapshot, SourceWorkspace};
pub use zkcl_diagnostics::{SourcePosition, SourcePositionEncoding};
