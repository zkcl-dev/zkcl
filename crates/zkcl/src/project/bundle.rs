use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use zkcl_registry::Digest;
use zkcl_runtime::{Root, Runtime};

use super::delivery::{Delivery, StartupStep};

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("project bundle: {0}")]
    Invalid(String),
    #[error("project bundle I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid project bundle manifest: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("project bundle initializer: {0}")]
    Lisp(#[from] zkcl_runtime::EvalError),
    #[error("project bundle initialization: {0}")]
    Initialization(#[source] Box<dyn std::error::Error>),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    toolchain: String,
    target: String,
    entry: Option<String>,
    asdf: bool,
    steps: Vec<StartupStep>,
    files: BTreeMap<PathBuf, String>,
}

/// Prepared FASLs, ordered initialization and resources. Loading needs no source
/// project, dependency solver, registry access, or command-line executable.
pub struct ProjectBundle {
    root: PathBuf,
    manifest: BundleManifest,
}

fn io<T>(path: &Path, result: std::io::Result<T>) -> Result<T, BundleError> {
    result.map_err(|source| BundleError::Io {
        path: path.to_owned(),
        source,
    })
}

fn relative(path: &Path) -> Result<(), BundleError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(BundleError::Invalid(format!(
            "invalid asset path {}",
            path.display()
        )));
    }
    Ok(())
}

fn collect(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<PathBuf, String>,
) -> Result<(), BundleError> {
    for entry in io(directory, fs::read_dir(directory))? {
        let path = io(directory, entry)?.path();
        let metadata = io(&path, fs::symlink_metadata(&path))?;
        if metadata.file_type().is_symlink() {
            return Err(BundleError::Invalid(format!(
                "symbolic link in bundle: {}",
                path.display()
            )));
        }
        if metadata.is_dir() {
            collect(root, &path, files)?;
        } else if metadata.is_file() {
            files.insert(
                path.strip_prefix(root)
                    .expect("bundle descendant")
                    .to_owned(),
                Digest::of_bytes(&io(&path, fs::read(&path))?).to_string(),
            );
        }
    }
    Ok(())
}

impl ProjectBundle {
    pub fn write(
        delivery: &Delivery,
        destination: &Path,
        target: &str,
        entry: Option<String>,
    ) -> Result<Self, BundleError> {
        if destination.exists() {
            return Err(BundleError::Invalid(format!(
                "destination already exists: {}",
                destination.display()
            )));
        }
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        io(parent, fs::create_dir_all(parent))?;
        let stage = io(
            parent,
            tempfile::Builder::new()
                .prefix(".zkcl-bundle-")
                .tempdir_in(parent),
        )?;
        for (index, unit) in delivery.units.iter().enumerate() {
            let path = stage.path().join(format!("{index:04}.zkclfasl"));
            io(&unit.fasl, fs::copy(&unit.fasl, path))?;
        }
        super::delivery::stage_delivery_assets(delivery, stage.path())
            .map_err(BundleError::Initialization)?;
        let mut manifest = BundleManifest {
            toolchain: env!("CARGO_PKG_VERSION").to_owned(),
            target: target.to_owned(),
            entry,
            asdf: delivery.asdf.is_some(),
            steps: delivery.steps.clone(),
            files: BTreeMap::new(),
        };
        collect(stage.path(), stage.path(), &mut manifest.files)?;
        let path = stage.path().join("project.json");
        io(
            &path,
            fs::write(&path, serde_json::to_vec_pretty(&manifest)?),
        )?;
        io(destination, fs::rename(stage.path(), destination))?;
        Self::open(destination)
    }

    /// Read metadata only. Every asset is verified before executing any initializer.
    pub fn open(path: &Path) -> Result<Self, BundleError> {
        let root = io(path, path.canonicalize())?;
        let path = root.join("project.json");
        let bytes = io(&path, fs::read(&path))?;
        let manifest: BundleManifest = serde_json::from_slice(&bytes)?;
        if manifest.toolchain != env!("CARGO_PKG_VERSION") {
            return Err(BundleError::Invalid(format!(
                "toolchain {} differs from {}",
                manifest.toolchain,
                env!("CARGO_PKG_VERSION")
            )));
        }
        for path in manifest.files.keys() {
            relative(path)?;
        }
        for step in &manifest.steps {
            if let StartupStep::LoadFasl(index) = step {
                let path = PathBuf::from(format!("{index:04}.zkclfasl"));
                if !manifest.files.contains_key(&path) {
                    return Err(BundleError::Invalid(format!(
                        "missing initializer {}",
                        path.display()
                    )));
                }
            }
        }
        if manifest.asdf
            && !manifest
                .files
                .contains_key(Path::new("asdf-bootstrap.lisp"))
        {
            return Err(BundleError::Invalid("missing ASDF bootstrap".into()));
        }
        Ok(Self { root, manifest })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn target(&self) -> &str {
        &self.manifest.target
    }
    pub fn entry(&self) -> Option<&str> {
        self.manifest.entry.as_deref()
    }

    pub fn load(&self, runtime: &Runtime, asdf_output: &Path) -> Result<(), BundleError> {
        if self.target() != runtime.compilation_target().to_string() {
            return Err(BundleError::Invalid(format!(
                "target {} differs from runtime {}",
                self.target(),
                runtime.compilation_target()
            )));
        }
        for (relative, digest) in &self.manifest.files {
            let path = self.root.join(relative);
            let canonical = io(&path, path.canonicalize())?;
            if !canonical.starts_with(&self.root)
                || Digest::of_bytes(&io(&path, fs::read(&path))?).to_string() != *digest
            {
                return Err(BundleError::Invalid(format!(
                    "asset failed verification: {}",
                    relative.display()
                )));
            }
        }
        if self.manifest.asdf {
            io(asdf_output, fs::create_dir_all(asdf_output))?;
            super::asdf::initialize_delivery(runtime, &self.root, asdf_output)
                .map_err(BundleError::Initialization)?;
        }
        for step in &self.manifest.steps {
            match step {
                StartupStep::LoadFasl(index) => {
                    runtime.load_file(&self.root.join(format!("{index:04}.zkclfasl")))?
                }
                StartupStep::LoadAsdf(name) => {
                    super::asdf::load(runtime, name).map_err(BundleError::Initialization)?
                }
                StartupStep::ReadyAsdfAlias(names) => super::asdf::mark_native_ready(
                    runtime,
                    &names.iter().map(String::as_str).collect::<Vec<_>>(),
                )
                .map_err(BundleError::Initialization)?,
            }
        }
        Ok(())
    }

    pub fn call_entry<'rt>(&self, runtime: &'rt Runtime) -> Result<Vec<Root<'rt>>, BundleError> {
        let name = self
            .entry()
            .ok_or_else(|| BundleError::Invalid("bundle has no binary entry".into()))?;
        let (package, name) = name
            .split_once(':')
            .ok_or_else(|| BundleError::Invalid("entry must be package-qualified".into()))?;
        let symbol = runtime
            .lookup_symbol(package, name.strip_prefix(':').unwrap_or(name))?
            .ok_or_else(|| BundleError::Invalid("bundle entry is undefined".into()))?;
        Ok(runtime.call(&symbol, &[])?)
    }
}
