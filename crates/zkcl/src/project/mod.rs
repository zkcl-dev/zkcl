//! Project execution shared by command-line tools and embedded hosts.
//!
//! `project` enables source projects and dependency acquisition. Without it,
//! hosts can load prepared artifacts without registry clients or a terminal.
//! All execution uses a caller-owned [`zkcl_runtime::Runtime`]. Initializers
//! may have effects before failing; discard a candidate runtime on failure.

pub mod asdf;
#[cfg(feature = "project")]
pub mod build_script;
mod bundle;
pub mod delivery;
#[cfg(feature = "project")]
pub mod resolved;
#[cfg(feature = "project")]
mod session;

pub use bundle::{BundleError, ProjectBundle};
#[cfg(feature = "project")]
pub use resolved::{ResolvedProject, Update};
#[cfg(feature = "project")]
pub use session::{LoadOptions, LoadedProject, ProjectSession, ResolveOptions, lookup_entry};
pub use zkcl_project as metadata;
#[cfg(feature = "project")]
pub use zkcl_project::registries;
pub use zkcl_project::workspace::{Project, ProjectMetadataFileError, TargetSelection, Workspace};
pub use zkcl_project::{cache, workspace};
