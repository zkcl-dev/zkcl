use std::fs;

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use zkcl_runtime::{EvalError, Runtime, SourceDocument};

use super::asdf::AsdfDelivery;

#[derive(Debug)]
pub struct FaslUnit {
    pub source: PathBuf,
    pub fasl: PathBuf,
}

impl FaslUnit {
    /// Compile and load this unit again, reusing its output pathname.
    pub fn reload(&self, runtime: &Runtime) -> Result<SourceDocument, DeliveryError> {
        compile_and_load(runtime, &self.source, &self.fasl).map(|(_, source)| source)
    }
}

#[derive(Debug, Default)]
pub struct Delivery {
    pub units: Vec<FaslUnit>,
    pub steps: Vec<StartupStep>,
    pub asdf: Option<AsdfDelivery>,
    pub link_libraries: Vec<String>,
    pub link_search: Vec<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum StartupStep {
    LoadFasl(usize),
    LoadAsdf(String),
    ReadyAsdfAlias(Vec<String>),
}

/// Observe the environment before an ASDF or native load and the source used
/// by a native component that finished loading.
pub enum LoadEvent<'a> {
    Begin(&'a Runtime),
    LoadedSource(&'a SourceDocument),
}

pub fn stage_delivery_assets(
    delivery: &Delivery,
    destination: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(asdf) = &delivery.asdf {
        super::asdf::stage_delivery(asdf, destination)?;
    }
    Ok(())
}

/// Initialize the delivered image in a fresh instance before publishing it.
/// This intentionally repeats top-level load effects in a separate build
/// instance, without calling the application entry. It catches dependencies
/// on the compiler session and missing assets observed during initialization.
pub fn validate_delivery(
    delivery: &Delivery,
    assets: &Path,
    runtime: &Runtime,
) -> Result<(), Box<dyn std::error::Error>> {
    let output = tempfile::Builder::new()
        .prefix("zkcl-asdf-validate-")
        .tempdir_in(assets)?;
    if delivery.asdf.is_some() {
        super::asdf::initialize_delivery(runtime, assets, output.path())?;
    }
    for step in &delivery.steps {
        match step {
            StartupStep::LoadFasl(index) => {
                runtime.load_file(&assets.join(format!("{index:04}.zkclfasl")))?
            }
            StartupStep::LoadAsdf(name) => super::asdf::load(runtime, name)?,
            StartupStep::ReadyAsdfAlias(names) => {
                let names = names.iter().map(String::as_str).collect::<Vec<_>>();
                super::asdf::mark_native_ready(runtime, &names)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error("failed to {action} {path}: {source}", path = .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to compile project component {path}: {source}", path = .path.display())]
    Compile {
        path: PathBuf,
        #[source]
        source: zkcl_runtime::EvalError,
    },
    #[error("failed to load compiled project component {path}: {source}", path = .path.display())]
    Load {
        path: PathBuf,
        #[source]
        source: zkcl_runtime::EvalError,
    },
    #[cfg(feature = "aot")]
    #[error("failed to emit AOT project component {path}: {source}", path = .path.display())]
    Aot {
        path: PathBuf,
        #[source]
        source: zkcl_aot::AotError,
    },
}

impl DeliveryError {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

/// Compile and load each source in dependency order on the caller's build runtime.
///
/// The caller assigns a distinct output directory to each component group. It
/// can interleave these groups with ASDF actions on the same runtime. Replaying
/// both compilation and loading retains compile-time effects needed by later
/// files, including effects absent from the saved FASL.
///
/// Each LOAD binds its pathname and truename to the actual output FASL. A
/// delivery that needs adjacent resources must provide them at that location.
/// On error, the caller discards this build session, which can already contain
/// compilation and initialization effects from the failed or preceding files.
pub fn compile_files(
    runtime: &Runtime,
    sources: &[PathBuf],
    output_dir: &Path,
) -> Result<Vec<FaslUnit>, DeliveryError> {
    compile_files_observing(runtime, sources, output_dir, &mut |_| Ok(()))
}

pub(super) fn compile_files_observing(
    runtime: &Runtime,
    sources: &[PathBuf],
    output_dir: &Path,
    observe: &mut dyn FnMut(LoadEvent<'_>) -> Result<(), EvalError>,
) -> Result<Vec<FaslUnit>, DeliveryError> {
    fs::create_dir_all(output_dir)
        .map_err(|source| DeliveryError::io("create", output_dir, source))?;
    let mut components = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let output = output_dir.join(format!("{index:04}.zkclfasl"));
        observe(LoadEvent::Begin(runtime)).map_err(|error| DeliveryError::Compile {
            path: source.clone(),
            source: error,
        })?;
        let (fasl, revision) = compile_and_load(runtime, source, &output)?;
        observe(LoadEvent::LoadedSource(&revision)).map_err(|error| DeliveryError::Load {
            path: fasl.clone(),
            source: error,
        })?;
        components.push(FaslUnit {
            source: source.clone(),
            fasl,
        });
    }
    Ok(components)
}

fn compile_and_load(
    runtime: &Runtime,
    path: &Path,
    output: &Path,
) -> Result<(PathBuf, SourceDocument), DeliveryError> {
    let (fasl, revision) = runtime
        .compile_file_with_source(path, Some(output))
        .map_err(|source| DeliveryError::Compile { path: path.to_owned(), source })?;
    runtime.load_file(&fasl).map_err(|source| DeliveryError::Load {
        path: fasl.clone(),
        source,
    })?;
    Ok((fasl, revision))
}

#[cfg(feature = "aot")]
mod aot;
#[cfg(feature = "aot")]
pub use aot::{AotUnit, emit_aot_units};

impl Delivery {
    /// Append ordered compile/load actions on the same compilation runtime.
    pub fn append_files(
        &mut self,
        runtime: &Runtime,
        files: &[PathBuf],
        output: &Path,
    ) -> Result<(), DeliveryError> {
        self.append_files_observing(runtime, files, output, &mut |_| Ok(()))
    }

    pub(super) fn append_files_observing(
        &mut self,
        runtime: &Runtime,
        files: &[PathBuf],
        output: &Path,
        observe: &mut dyn FnMut(LoadEvent<'_>) -> Result<(), EvalError>,
    ) -> Result<(), DeliveryError> {
        let compiled = compile_files_observing(runtime, files, output, observe)?;
        for component in compiled {
            self.steps.push(StartupStep::LoadFasl(self.units.len()));
            self.units.push(component);
        }
        Ok(())
    }
}
