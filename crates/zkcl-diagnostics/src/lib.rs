//! Source identity and diagnostic facts shared by language and host adapters.
//!
//! This crate does not read files or depend on syntax trees, runtime values,
//! terminal libraries, or protocol types. Positions always refer to retained
//! immutable text, never to a later disk revision.

mod diagnostic;
mod position;
mod source;

pub use diagnostic::*;
pub use position::{SourcePosition, SourcePositionEncoding};
pub use source::{ByteRange, SnapshotId, SourceDocument, SourceSpan, SpanError};
