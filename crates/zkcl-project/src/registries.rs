mod config;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};
use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::lockfile::{LockedGraph, LockedModule, Lockfile};
use crate::manifest::{DependencySource, MANIFEST_NAME, Manifest, ManifestFile, parse_manifest};
use crate::solver::{ModuleId, PackageIndex, Release, ReleaseId, SourceId};
use zkcl_registry::{ContentStore, Digest, FileRegistry};

use self::config::{ArchiveSource, RegistryRecord, RegistrySources};
use crate::workspace::{
    LocalProvider, Project, RegistryDeclarations, dependency_request, system_definitions,
};
use crate::{cache, git};

pub struct RegistryCache {
    registries: Rc<RegistrySources>,
    store: ContentStore,
    previous: Option<LockedGraph>,
    locked: bool,
    offline: bool,
    snapshots: RefCell<BTreeMap<ModuleId, Vec<IndexedRelease>>>,
}

#[derive(Clone)]
pub struct IndexedRelease {
    pub release: Release,
    pub checksum: Digest,
    pub systems: BTreeMap<String, PathBuf>,
    pub asdf_aliases: BTreeMap<String, String>,
    pub has_build_dependencies: bool,
    manifest_checksum: Digest,
    archive_source: Option<ArchiveSource>,
}

#[derive(Debug, thiserror::Error)]
enum CachedDependencyError {
    #[error(
        "cannot read the cached archive for {module}: {source}. Run `zkcl fetch` with its registry"
    )]
    ReadArchive {
        module: ModuleId,
        source: zkcl_registry::RegistryError,
    },
    #[error("cannot open cached source for {module}: {source}. Run `zkcl fetch` with its registry")]
    Materialize {
        module: ModuleId,
        source: zkcl_registry::RegistryError,
    },
}

impl RegistryCache {
    pub fn new(
        paths: &[PathBuf],
        previous: Option<LockedGraph>,
        locked: bool,
        offline: bool,
    ) -> Result<Self, Box<dyn Error>> {
        Self::with_declarations(
            paths,
            RegistryDeclarations::default(),
            previous,
            locked,
            offline,
        )
    }

    pub fn with_declarations(
        paths: &[PathBuf],
        declarations: RegistryDeclarations,
        previous: Option<LockedGraph>,
        locked: bool,
        offline: bool,
    ) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            registries: Rc::new(RegistrySources::new(
                paths,
                declarations,
                cache::cache_root()?.join("registries"),
            )),
            store: cache::source_store()?,
            previous,
            locked: locked || offline,
            offline,
            snapshots: RefCell::default(),
        })
    }

    /// Use the same registry snapshots and fetch policy for an associated graph.
    pub fn for_graph(&self, previous: Option<LockedGraph>) -> Self {
        Self {
            registries: self.registries.clone(),
            store: self.store.clone(),
            previous,
            locked: self.locked,
            offline: self.offline,
            snapshots: RefCell::default(),
        }
    }

    pub fn selection(&self, id: &ModuleId, release: &ReleaseId) -> Option<IndexedRelease> {
        self.snapshots
            .borrow()
            .get(id)?
            .iter()
            .find(|entry| entry.release.id == *release)
            .cloned()
    }

    fn read(&self, id: &ModuleId) -> io::Result<Vec<IndexedRelease>> {
        let previous = self.previous.as_ref().and_then(|lock| lock.modules.get(id));
        if self.locked && previous.is_none() {
            return Ok(Vec::new());
        }
        let expected = previous
            .map(|module| locked_digest(id, module))
            .transpose()?;
        if matches!(id.source, SourceId::Git { .. }) {
            let (checksum, text) = git::read(id, &self.store, expected, self.offline)?;
            let manifest = project_manifest(id, &text)?;
            let release = ReleaseId::Version(manifest.version.clone());
            return Ok(vec![indexed(
                id, release, checksum, &text, manifest, false,
            )?]);
        }
        if let (true, Some(module), Some(checksum)) = (self.locked, previous, expected) {
            if self.offline || self.store.archive_path(checksum).try_exists()? {
                let text = self.store.manifest(checksum).map_err(|source| {
                    io::Error::other(CachedDependencyError::ReadArchive {
                        module: id.clone(),
                        source,
                    })
                })?;
                let manifest = project_manifest(id, &text)?;
                return Ok(vec![indexed(
                    id,
                    module.release.clone(),
                    checksum,
                    &text,
                    manifest,
                    false,
                )?]);
            }
            let records = self.registries.records(id)?;
            return Ok(vec![locked_registry_release(
                id, module, checksum, &records,
            )?]);
        }
        self.registries
            .records(id)?
            .iter()
            .map(|record| {
                let indexed = indexed_registry_release(id, record)?;
                if let (Some(module), Some(digest)) = (previous, expected)
                    && indexed.release.id == module.release
                {
                    verify_locked_digest(id, &module.release, indexed.checksum, digest)?;
                }
                Ok(indexed)
            })
            .collect()
    }

    pub fn fetch(&self, id: &ModuleId, release: &IndexedRelease) -> Result<(), Box<dyn Error>> {
        let digest = release.checksum;
        if !self.store.archive_path(digest).try_exists()? {
            let Some(origin) = &release.archive_source else {
                return Err(format!("source archive for {id} disappeared during fetching").into());
            };
            self.registries.fetch(id, origin, digest, &self.store)?;
        }
        let manifest = self.store.manifest(digest)?;
        release.verify_manifest(id, manifest.as_bytes())
    }

    pub fn fetch_all_configurations(&self, lockfiles: &Lockfile) -> Result<(), Box<dyn Error>> {
        let mut records = BTreeMap::new();
        for lock in lockfiles.graphs() {
            for (id, module) in &lock.modules {
                match &id.source {
                    SourceId::Path(_) => {}
                    SourceId::PathRequest { .. } => {
                        unreachable!("unactivated path request cannot appear in a lockfile")
                    }
                    SourceId::Git { .. } => {
                        let checksum = locked_digest(id, module)?;
                        let (_, manifest) =
                            git::read(id, &self.store, Some(checksum), self.offline)?;
                        verify_locked_manifest(id, &module.release, &manifest)?;
                    }
                    SourceId::Registry(_) => {
                        let checksum = locked_digest(id, module)?;
                        if self.store.archive_path(checksum).try_exists()? {
                            let manifest = self.store.manifest(checksum)?;
                            verify_locked_manifest(id, &module.release, &manifest)?;
                        } else if self.offline {
                            return Err(format!(
                                "offline source archive for {id} is not cached; run `zkcl fetch --all-configurations` online"
                            )
                            .into());
                        } else {
                            let index = match records.entry(id) {
                                Entry::Occupied(entry) => entry.into_mut(),
                                Entry::Vacant(entry) => entry.insert(self.registries.records(id)?),
                            };
                            let release = locked_registry_release(id, module, checksum, index)?;
                            self.fetch(id, &release)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn materialize(
        &self,
        id: &ModuleId,
        release: &IndexedRelease,
    ) -> Result<Project, Box<dyn Error>> {
        let digest = release.checksum;
        let destination = self.store.root().join("sources").join(digest.to_string());
        let path = self
            .store
            .materialize(digest, destination)
            .map_err(|source| CachedDependencyError::Materialize {
                module: id.clone(),
                source,
            })?;
        release.verify_manifest(id, &fs::read(path.join(MANIFEST_NAME))?)?;
        Project::open(&path)
    }
}

fn locked_digest(id: &ModuleId, module: &LockedModule) -> io::Result<Digest> {
    module
        .checksum
        .as_deref()
        .ok_or_else(|| io::Error::other(format!("no digest for {id} in the lockfile")))?
        .parse()
        .map_err(io::Error::other)
}

fn verify_locked_digest(
    id: &ModuleId,
    release: &ReleaseId,
    actual: Digest,
    expected: Digest,
) -> io::Result<()> {
    if actual != expected {
        return Err(io::Error::other(format!(
            "registry record for {id} {release} differs from its locked digest"
        )));
    }
    Ok(())
}

fn locked_registry_release(
    id: &ModuleId,
    module: &LockedModule,
    digest: Digest,
    records: &[RegistryRecord],
) -> io::Result<IndexedRelease> {
    let version = module.release.to_string();
    let record = records
        .iter()
        .find(|record| record.metadata.version == version)
        .ok_or_else(|| {
            io::Error::other(format!(
                "registry no longer lists locked release {id} {version}"
            ))
        })?;
    verify_locked_digest(id, &module.release, record.metadata.checksum, digest)?;
    indexed_registry_release(id, record)
}

fn indexed_registry_release(id: &ModuleId, record: &RegistryRecord) -> io::Result<IndexedRelease> {
    let metadata = &record.metadata;
    let release = ReleaseId::Version(metadata.version.parse().map_err(io::Error::other)?);
    let manifest = project_manifest(id, &metadata.manifest)?;
    let mut indexed = indexed(
        id,
        release,
        metadata.checksum,
        &metadata.manifest,
        manifest,
        metadata.yanked,
    )?;
    indexed.archive_source = Some(record.source.clone());
    Ok(indexed)
}

fn verify_locked_manifest(
    id: &ModuleId,
    release: &ReleaseId,
    text: &str,
) -> Result<(), Box<dyn Error>> {
    let manifest = project_manifest(id, text)?;
    if manifest.name != id.name || ReleaseId::Version(manifest.version) != *release {
        return Err(
            format!("source archive for {id} differs from its locked release {release}").into(),
        );
    }
    Ok(())
}

impl IndexedRelease {
    fn verify_manifest(&self, id: &ModuleId, manifest: &[u8]) -> Result<(), Box<dyn Error>> {
        if Digest::of_bytes(manifest) != self.manifest_checksum {
            return Err(format!("{id} index metadata differs from its source archive").into());
        }
        Ok(())
    }
}

pub struct CombinedIndex<'a> {
    pub local: &'a LocalProvider,
    pub registries: &'a RegistryCache,
}

impl PackageIndex for CombinedIndex<'_> {
    type Error = io::Error;

    fn begin_graph(&self) -> Result<(), Self::Error> {
        self.local.begin_graph()
    }

    fn activate(
        &self,
        dependency: &crate::solver::Dependency,
    ) -> Result<crate::solver::Dependency, Self::Error> {
        if matches!(
            dependency.module.source,
            SourceId::Path(_) | SourceId::PathRequest { .. }
        ) {
            self.local.activate(dependency)
        } else {
            Ok(dependency.clone())
        }
    }

    fn refresh(&self, release: &Release) -> Result<Release, Self::Error> {
        if matches!(release.module.source, SourceId::Path(_)) {
            self.local.refresh(release)
        } else {
            Ok(release.clone())
        }
    }

    fn releases(&self, id: &ModuleId) -> Result<Vec<Release>, Self::Error> {
        if matches!(id.source, SourceId::Path(_)) {
            return self.local.releases(id);
        }
        let mut snapshots = self.registries.snapshots.borrow_mut();
        if !snapshots.contains_key(id) {
            snapshots.insert(id.clone(), self.registries.read(id)?);
        }
        Ok(snapshots[id]
            .iter()
            .map(|entry| entry.release.clone())
            .collect())
    }
}

fn project_manifest(id: &ModuleId, text: &str) -> io::Result<Manifest> {
    let ManifestFile::Project(manifest) = parse_manifest(text).map_err(io::Error::other)? else {
        return Err(io::Error::other(format!(
            "{id} contains a workspace instead of a project"
        )));
    };
    Ok(manifest)
}

fn indexed(
    id: &ModuleId,
    release: ReleaseId,
    checksum: Digest,
    text: &str,
    manifest: Manifest,
    yanked: bool,
) -> io::Result<IndexedRelease> {
    if manifest.name != id.name || ReleaseId::Version(manifest.version.clone()) != release {
        return Err(io::Error::other(format!(
            "index entry {id} {release} and its manifest disagree"
        )));
    }
    let has_build_dependencies = manifest
        .build
        .as_ref()
        .is_some_and(|build| !build.dependencies.is_empty());
    let dependencies = manifest
        .dependencies
        .iter()
        .filter(|dependency| {
            !manifest
                .build
                .as_ref()
                .is_some_and(|build| build.dependencies.contains(&dependency.alias))
        })
        .map(|dependency| match &dependency.source {
            DependencySource::Registry { registry } => Ok(dependency_request(
                dependency,
                SourceId::Registry(registry.clone().unwrap_or_else(|| "zkcl".into())),
            )),
            DependencySource::Git { url, rev } => Ok(dependency_request(
                dependency,
                SourceId::Git {
                    url: url.clone(),
                    rev: rev.clone(),
                },
            )),
            DependencySource::Path { .. } => Err(io::Error::other(format!(
                "published dependency {:?} of {id} needs an immutable source",
                dependency.alias
            ))),
        })
        .collect::<io::Result<_>>()?;
    let systems = system_definitions(&manifest)?;
    Ok(IndexedRelease {
        release: Release {
            module: id.clone(),
            id: release,
            toolchain: manifest.toolchain,
            dependencies,
            packages: manifest.packages.into_iter().collect(),
            nicknames: BTreeSet::new(),
            systems: systems
                .keys()
                .chain(manifest.asdf_aliases.keys())
                .cloned()
                .collect(),
            features: manifest.features,
            exclusive_features: manifest.exclusive_features,
            yanked,
            available: true,
        },
        checksum,
        systems,
        asdf_aliases: manifest.asdf_aliases,
        has_build_dependencies,
        manifest_checksum: Digest::of_bytes(text.as_bytes()),
        archive_source: None,
    })
}

pub fn configured_identity(path: &Path) -> Result<String, Box<dyn Error>> {
    Ok(config::configured_identity(path)?)
}

pub fn publish(project: &Project, dir: &Path) -> Result<Digest, Box<dyn Error>> {
    let registry = FileRegistry::open(dir.canonicalize()?)?;
    let files = archive_files(project)?;
    let manifest = &project.manifest;
    let text = fs::read_to_string(project.source_file(Path::new(MANIFEST_NAME))?)?;
    Ok(registry.publish(
        manifest.name.as_str(),
        &manifest.version.to_string(),
        &project.root,
        &files,
        &text,
    )?)
}

pub fn archive_files(project: &Project) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let manifest = &project.manifest;
    if manifest
        .dependencies
        .iter()
        .any(|dependency| matches!(dependency.source, DependencySource::Path { .. }))
    {
        return Err("published dependencies must use immutable registry or Git sources".into());
    }
    if manifest.license.is_none() {
        return Err("a published project needs a license declaration".into());
    }
    let text = fs::read_to_string(project.source_file(Path::new(MANIFEST_NAME))?)?;
    if parse_manifest(&text)? != ManifestFile::Project(manifest.clone()) {
        return Err("the selected manifest differs from the published zkcl.sexp".into());
    }
    let mut files = BTreeSet::from([PathBuf::from(MANIFEST_NAME)]);
    files.extend(project.authored_target_files().cloned());
    if let Some(build) = &manifest.build {
        files.insert(build.path.clone());
        files.extend(build.inputs.iter().flatten().cloned());
    }
    files.extend(manifest.systems.values().cloned());
    files.extend(manifest.include.iter().cloned());
    include_license(&project.root, manifest, &mut files)?;
    Ok(files.into_iter().collect())
}

fn include_license(
    root: &Path,
    manifest: &Manifest,
    files: &mut BTreeSet<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    for name in ["LICENSE", "LICENSE.md", "LICENSE.txt", "COPYING"] {
        if root.join(name).is_file() {
            files.insert(PathBuf::from(name));
            return Ok(());
        }
    }
    Err(format!(
        "{} needs a LICENSE or COPYING file for its published archive",
        manifest.name
    )
    .into())
}
