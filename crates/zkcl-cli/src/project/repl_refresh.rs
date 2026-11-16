use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use zkcl::{FileReaderBindings, Runtime, SourceDocument};
use zkcl::project::asdf;
use zkcl::project::delivery::{Delivery, DeliveryError, StartupStep};
use zkcl_project::lockfile::{LOCKFILE_NAME, LockKey, LockedGraph, Lockfile, parse_lockfile};
use zkcl_project::manifest::{BuildTarget, MANIFEST_NAME, ManifestFile, parse_manifest};
use zkcl_project::solver::SourceId;
use zkcl_registry::Digest;

use super::{ProjectMetadataFileError, ResolvedProject};

#[derive(Default, serde::Serialize)]
pub(crate) struct RefreshReport {
    pub files: Vec<PathBuf>,
    pub systems: Vec<String>,
    pub remaining: usize,
}

pub(crate) struct ActiveProject {
    inputs: ProjectInputs,
    reader: FileReaderBindings,
    delivery: Delivery,
    revisions: Vec<SourceRevision>,
    resume_at: Option<usize>,
    pub package: Option<String>,
    // Lisp code can retain paths inside the session's output tree.
    _output: tempfile::TempDir,
}

impl ActiveProject {
    pub(super) fn load(
        project: &ResolvedProject,
        runtime: &Runtime,
        output: tempfile::TempDir,
        target: Option<&BuildTarget>,
    ) -> Result<Self, Box<dyn Error>> {
        let inputs = ProjectInputs::capture(project)?;
        let reader = runtime.file_reader_bindings()?;
        let mut revisions = Vec::new();
        let delivery = project.load_target_observing(
            runtime,
            output.path(),
            &project.project,
            target,
            &mut |_, source| revisions.push(SourceRevision::from_document(source)),
        )?;
        Ok(Self {
            inputs,
            reader,
            delivery,
            revisions,
            resume_at: None,
            package: project.project.manifest.repl_package.clone(),
            _output: output,
        })
    }

    pub(crate) fn sources(&self) -> impl Iterator<Item = &Path> {
        self.delivery.units.iter().map(|unit| unit.source.as_path())
    }

    pub(crate) fn refresh(
        &mut self,
        runtime: &Runtime,
        report: &mut RefreshReport,
    ) -> Result<(), Box<dyn Error>> {
        self.inputs.verify()?;
        let steps = &self.delivery.steps;
        let mut start = self.resume_at.unwrap_or(steps.len());
        for (position, step) in steps.iter().enumerate() {
            match step {
                StartupStep::LoadFasl(index) => {
                    let path = &self.delivery.units[*index].source;
                    let Some(revision) = SourceRevision::read(path)? else {
                        return Err(format!("{} was removed. Use :reload to rebuild the project.", path.display()).into());
                    };
                    if revision.path != self.revisions[*index].path {
                        return Err(changed_input(path));
                    }
                    if revision != self.revisions[*index] {
                        start = start.min(position);
                    }
                }
                StartupStep::LoadAsdf(_) => {
                    // Only ASDF knows its dirty component graph. Run its LOAD-OP and
                    // replay any following native files that could use changed macros.
                    start = start.min(position);
                }
                StartupStep::ReadyAsdfAlias(_) => {}
            }
        }
        if start == steps.len() {
            return Ok(());
        }

        report.remaining = steps[start..]
            .iter()
            .filter(|step| !matches!(step, StartupStep::ReadyAsdfAlias(_)))
            .count();
        self.resume_at = Some(start);
        let result = runtime.with_file_reader_bindings(&self.reader, || -> Result<(), Box<dyn Error>> {
            for (offset, step) in steps[start..].iter().enumerate() {
                self.resume_at = Some(start + offset);
                match step {
                    StartupStep::LoadFasl(index) => {
                        let unit = &self.delivery.units[*index];
                        let source = unit.reload(runtime)?;
                        self.revisions[*index] = SourceRevision::from_document(&source);
                        report.files.push(unit.source.clone());
                    }
                    StartupStep::LoadAsdf(name) => {
                        asdf::load(runtime, name)?;
                        report.systems.push(name.clone());
                    }
                    StartupStep::ReadyAsdfAlias(_) => continue,
                }
                report.remaining -= 1;
            }
            self.resume_at = None;
            Ok(())
        })?;
        result
    }
}

struct ProjectInputs {
    manifests: BTreeMap<PathBuf, Option<ManifestFile>>,
    build_files: BTreeMap<PathBuf, Option<Digest>>,
    environment: BTreeMap<String, Option<OsString>>,
    lock_path: PathBuf,
    locks: BTreeMap<LockKey, Option<LockedGraph>>,
}

impl ProjectInputs {
    fn capture(resolved: &ResolvedProject) -> Result<Self, Box<dyn Error>> {
        let project = &resolved.project;
        let lock_root = project.lock_root()?;
        let lock_path = lock_root.join(LOCKFILE_NAME);
        let lock = read_metadata(&lock_path, parse_lockfile)?;
        let graphs = resolved.configuration_graphs();
        let mut sources = BTreeSet::from([project.root.clone()]);
        let mut locks = BTreeMap::new();
        for graph in graphs {
            sources.extend(graph.modules.keys().filter_map(|module| match &module.source {
                SourceId::Path(path) => Some(lock_root.join(path)),
                _ => None,
            }));
            let key = graph.key();
            locks.insert(key.clone(), find_graph(lock.as_ref(), &key).cloned());
        }
        let mut inputs = Self {
            manifests: BTreeMap::new(),
            build_files: BTreeMap::new(),
            environment: BTreeMap::new(),
            lock_path,
            locks,
        };
        // A workspace declaration can affect the selected project and registry inputs.
        if lock_root != project.root {
            let path = lock_root.join(MANIFEST_NAME);
            let manifest = read_metadata(&path, parse_manifest)?;
            inputs.manifests.insert(path, manifest);
        }
        for root in sources {
            let path = if root == project.root {
                project.manifest_path.clone()
            } else {
                root.join(MANIFEST_NAME)
            };
            let manifest = read_metadata(&path, parse_manifest)?;
            if root == project.root
                && manifest.as_ref() != Some(&ManifestFile::Project(project.manifest.clone()))
            {
                return Err(changed_input(&path));
            }
            if let Some(ManifestFile::Project(project)) = &manifest
                && let Some(build) = &project.build
            {
                for input in std::iter::once(&build.path).chain(build.inputs.iter().flatten()) {
                    let path = root.join(input);
                    let digest = file_digest(&path)?;
                    inputs.build_files.insert(path, digest);
                }
                for name in &build.env {
                    inputs.environment.insert(name.clone(), env::var_os(name));
                }
            }
            inputs.manifests.insert(path, manifest);
        }
        Ok(inputs)
    }

    fn verify(&self) -> Result<(), Box<dyn Error>> {
        for (path, previous) in &self.manifests {
            if read_metadata(path, parse_manifest)? != *previous {
                return Err(changed_input(path));
            }
        }
        let lock = read_metadata(&self.lock_path, parse_lockfile)?;
        for (key, previous) in &self.locks {
            if find_graph(lock.as_ref(), key) != previous.as_ref() {
                return Err(changed_input(&self.lock_path));
            }
        }
        for (path, previous) in &self.build_files {
            if file_digest(path)? != *previous {
                return Err(changed_input(path));
            }
        }
        for (name, previous) in &self.environment {
            if env::var_os(name) != *previous {
                return Err(format!("Build input {name} changed. Use :reload to rebuild the project.").into());
            }
        }
        Ok(())
    }
}

fn find_graph<'a>(lock: Option<&'a Lockfile>, key: &LockKey) -> Option<&'a LockedGraph> {
    lock?.selected_for_scope(&key.project, &key.target_triple, &key.features, key.scope)
}

fn changed_input(path: &Path) -> Box<dyn Error> {
    format!("{} changed. Use :reload to rebuild the project.", path.display()).into()
}

fn read_metadata<T>(
    path: &Path,
    parse: impl FnOnce(&str) -> Result<T, zkcl_project::ProjectMetadataError>,
) -> Result<Option<T>, ProjectMetadataFileError> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(ProjectMetadataFileError::Read { path: path.to_owned(), source }),
    };
    parse(&source).map(Some).map_err(|error| ProjectMetadataFileError::Parse {
        document: SourceDocument::file(path.to_owned(), Some(source)),
        source: error,
    })
}

fn file_digest(path: &Path) -> Result<Option<Digest>, Box<dyn Error>> {
    match File::open(path).and_then(Digest::of_reader) {
        Ok(digest) => Ok(Some(digest)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DeliveryError::Io {
            action: "read", path: path.to_owned(), source: error,
        }.into()),
    }
}

#[derive(PartialEq, Eq)]
struct SourceRevision {
    path: PathBuf,
    digest: Digest,
}

impl SourceRevision {
    fn from_document(document: &SourceDocument) -> Self {
        Self {
            path: document.path().expect("compiled file pathname").to_owned(),
            digest: Digest::of_bytes(document.text().expect("compiled file source text").as_bytes()),
        }
    }

    fn read(path: &Path) -> Result<Option<Self>, Box<dyn Error>> {
        let Some(digest) = file_digest(path)? else {
            return Ok(None);
        };
        Ok(Some(Self { path: path.canonicalize()?, digest }))
    }
}
