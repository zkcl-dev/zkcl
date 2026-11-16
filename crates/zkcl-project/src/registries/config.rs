use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::solver::{ModuleId, SourceId};
use serde::Deserialize;
use tokio::runtime::{Builder, Runtime};
use url::Url;
use zkcl_registry::signed::{SignedRegistry, SignedRegistryConfig, TrustedRelease};
use zkcl_registry::{ContentStore, Digest, FileRegistry, ReleaseRecord};

const DESCRIPTOR_LIMIT: u64 = 16 * 1024;
const TRUST_ROOT_LIMIT: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    format: u64,
    identity: String,
    metadata_url: String,
    targets_url: String,
    trusted_root: PathBuf,
}

#[derive(PartialEq, Eq)]
struct SignedConfig {
    identity: String,
    metadata_url: Url,
    targets_url: Url,
    trusted_root: PathBuf,
}

impl SignedConfig {
    fn read(path: &Path) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let descriptor: Descriptor =
            serde_json::from_slice(&read_limited(path, DESCRIPTOR_LIMIT)?)?;
        if descriptor.format != 1 {
            return Err(format!(
                "unsupported registry descriptor format {}",
                descriptor.format
            )
            .into());
        }
        let identity = descriptor.identity;
        if identity.is_empty()
            || identity.len() > 1024
            || identity.trim() != identity
            || identity.chars().any(char::is_control)
        {
            return Err("registry identity must contain 1 to 1024 non-control UTF-8 bytes".into());
        }
        if descriptor.trusted_root.as_os_str().is_empty() {
            return Err("trusted_root must name a file".into());
        }
        let metadata_url = Url::parse(&descriptor.metadata_url)
            .map_err(|error| format!("invalid metadata_url: {error}"))?;
        let targets_url = Url::parse(&descriptor.targets_url)
            .map_err(|error| format!("invalid targets_url: {error}"))?;
        let trusted_root = path.with_file_name(descriptor.trusted_root);
        Ok(Self {
            identity,
            metadata_url,
            targets_url,
            trusted_root,
        })
    }

    fn load(&self, state_root: &Path, runtime: &Runtime) -> io::Result<SignedRegistry> {
        let trusted_root = read_limited(&self.trusted_root, TRUST_ROOT_LIMIT).map_err(|error| {
            io::Error::other(format!(
                "cannot read registry trust root {}: {error}",
                self.trusted_root.display()
            ))
        })?;
        let identity_digest = Digest::of_bytes(self.identity.as_bytes());
        runtime
            .block_on(SignedRegistry::load(SignedRegistryConfig {
                identity: self.identity.clone(),
                trusted_root,
                metadata_url: self.metadata_url.clone(),
                targets_url: self.targets_url.clone(),
                state_directory: state_root.join(identity_digest.to_string()),
            }))
            .map_err(|error| {
                io::Error::other(format!("signed registry {:?}: {error}", self.identity))
            })
    }
}

fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::other("expected a regular file"));
    }
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(format!("file exceeds {limit} bytes")));
    }
    Ok(bytes)
}

enum RegistrySource {
    File(FileRegistry),
    Signed(Box<SignedSource>),
}

struct SignedSource {
    config: SignedConfig,
    snapshot: OnceCell<SignedRegistry>,
}

impl RegistrySource {
    fn read(path: &Path) -> io::Result<Self> {
        let path = path.canonicalize().map_err(|error| {
            io::Error::other(format!("cannot open registry {}: {error}", path.display()))
        })?;
        if path.is_dir() {
            return FileRegistry::open(path)
                .map(Self::File)
                .map_err(io::Error::other);
        }
        let config = SignedConfig::read(&path).map_err(|error| {
            io::Error::other(format!(
                "invalid registry descriptor {}: {error}",
                path.display()
            ))
        })?;
        Ok(Self::Signed(Box::new(SignedSource {
            config,
            snapshot: OnceCell::new(),
        })))
    }

    fn identity(&self) -> &str {
        match self {
            Self::File(registry) => registry.id(),
            Self::Signed(source) => &source.config.identity,
        }
    }

    fn same_configuration(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::File(left), Self::File(right)) => left.root() == right.root(),
            (Self::Signed(left), Self::Signed(right)) => left.config == right.config,
            _ => false,
        }
    }
}

pub(super) fn configured_identity(path: &Path) -> io::Result<String> {
    Ok(RegistrySource::read(path)?.identity().to_owned())
}

pub(super) struct RegistrySources {
    paths: Vec<PathBuf>,
    configured: OnceCell<BTreeMap<String, RegistrySource>>,
    runtime: OnceCell<Runtime>,
    state_root: PathBuf,
}

#[derive(Clone)]
pub(super) enum ArchiveSource {
    File,
    Signed(Rc<TrustedRelease>),
}

pub(super) struct RegistryRecord {
    pub metadata: ReleaseRecord,
    pub source: ArchiveSource,
}

impl RegistrySources {
    pub fn new(paths: &[PathBuf], state_root: PathBuf) -> Self {
        Self {
            paths: paths.to_vec(),
            configured: OnceCell::new(),
            runtime: OnceCell::new(),
            state_root,
        }
    }

    fn registry(&self, id: &ModuleId) -> io::Result<&RegistrySource> {
        let SourceId::Registry(identity) = &id.source else {
            unreachable!("expected a registry module");
        };
        if self.configured.get().is_none() {
            let mut configured = BTreeMap::new();
            for path in &self.paths {
                let source = RegistrySource::read(path)?;
                let identity = source.identity().to_owned();
                match configured.entry(identity) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(source);
                    }
                    std::collections::btree_map::Entry::Occupied(entry)
                        if !source.same_configuration(entry.get()) =>
                    {
                        return Err(io::Error::other(format!(
                            "conflicting configurations for registry {:?}: {}",
                            entry.key(),
                            path.display()
                        )));
                    }
                    _ => {}
                }
            }
            self.configured.get_or_init(|| configured);
        }
        self.configured
            .get()
            .expect("registry configurations were initialized")
            .get(identity)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "registry {identity:?} is needed for {id}; supply it with --registry"
                ))
            })
    }

    fn signed<'a>(
        &'a self,
        source: &'a SignedSource,
    ) -> io::Result<(&'a Runtime, &'a SignedRegistry)> {
        if self.runtime.get().is_none() {
            let runtime = Builder::new_current_thread().enable_all().build()?;
            self.runtime.get_or_init(|| runtime);
        }
        let runtime = self
            .runtime
            .get()
            .expect("registry runtime was initialized");
        if source.snapshot.get().is_none() {
            let snapshot = source.config.load(&self.state_root, runtime)?;
            source.snapshot.get_or_init(|| snapshot);
        }
        Ok((
            runtime,
            source.snapshot.get().expect("registry snapshot was loaded"),
        ))
    }

    pub fn records(&self, id: &ModuleId) -> io::Result<Vec<RegistryRecord>> {
        match self.registry(id)? {
            RegistrySource::File(registry) => Ok(registry
                .records(id.name.as_str())
                .map_err(io::Error::other)?
                .into_iter()
                .map(|metadata| RegistryRecord {
                    metadata,
                    source: ArchiveSource::File,
                })
                .collect()),
            RegistrySource::Signed(source) => {
                let (runtime, registry) = self.signed(source)?;
                runtime
                    .block_on(registry.records(id.name.as_str()))
                    .map_err(io::Error::other)?
                    .into_iter()
                    .map(|release| {
                        Ok(RegistryRecord {
                            metadata: release
                                .record()
                                .release_record()
                                .map_err(io::Error::other)?,
                            source: ArchiveSource::Signed(Rc::new(release)),
                        })
                    })
                    .collect()
            }
        }
    }

    pub fn fetch(
        &self,
        id: &ModuleId,
        origin: &ArchiveSource,
        digest: Digest,
        store: &ContentStore,
    ) -> Result<(), Box<dyn Error>> {
        match (self.registry(id)?, origin) {
            (RegistrySource::File(registry), ArchiveSource::File) => {
                store.import(registry.store().archive(digest)?, digest)?;
            }
            (RegistrySource::Signed(source), ArchiveSource::Signed(release)) => {
                let (runtime, registry) = self.signed(source)?;
                runtime.block_on(registry.fetch(release, store))?;
            }
            _ => unreachable!("release came from the configured registry"),
        }
        Ok(())
    }
}
