//! Static project data and dependency resolution shared by zkcl tools.

use std::error::Error;
use std::ops::Range;

#[cfg(feature = "workspace")]
pub mod cache;
#[cfg(feature = "fetch")]
pub mod git;
pub mod lockfile;
pub mod manifest;
#[cfg(feature = "fetch")]
pub mod registries;
mod sexp;
pub mod solver;
pub mod sources;
#[cfg(feature = "workspace")]
pub mod workspace;

/// A diagnostic for the manifest or lockfile, located in the original UTF-8 text.
#[derive(Debug, thiserror::Error)]
#[error("{message} at bytes {}..{}", .span.start, .span.end)]
pub struct ProjectMetadataError {
    pub message: String,
    pub span: Range<usize>,
    #[source]
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProjectMetadataError {
    pub(crate) fn new(message: impl Into<String>, span: Range<usize>) -> Self {
        Self {
            message: message.into(),
            span,
            source: None,
        }
    }

    pub(crate) fn with_source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }
}
