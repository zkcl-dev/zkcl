use std::cell::OnceCell;
use std::collections::{BTreeMap, btree_map::Entry};
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::manifest::validate_registry_identity;
use crate::solver::{ModuleId, SourceId};
use crate::workspace::RegistryDeclarations;
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
    fn read(path: &Path, scope: Option<&Path>) -> Result<Self, Box<dyn Error + Send + Sync>> {
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
        validate_registry_identity(&identity)?;
        if descriptor.trusted_root.as_os_str().is_empty() {
            return Err("trusted_root must name a file".into());
        }
        let metadata_url = Url::parse(&descriptor.metadata_url)
            .map_err(|error| format!("invalid metadata_url: {error}"))?;
        let targets_url = Url::parse(&descriptor.targets_url)
            .map_err(|error| format!("invalid targets_url: {error}"))?;
        let trusted_root = path.with_file_name(descriptor.trusted_root);
        let trusted_root = if let Some(scope) = scope {
            registry_path(&trusted_root, Some(scope), "registry trust root")?
        } else {
            trusted_root
        };
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

fn registry_path(path: &Path, scope: Option<&Path>, kind: &str) -> io::Result<PathBuf> {
    let canonical = path.canonicalize().map_err(|error| {
        io::Error::other(format!("cannot open {kind} {}: {error}", path.display()))
    })?;
    if let Some(root) = scope
        && !canonical.starts_with(root)
    {
        return Err(io::Error::other(format!(
            "{kind} {} resolves outside {}",
            path.display(),
            root.display()
        )));
    }
    Ok(canonical)
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
    fn read(path: &Path, scope: Option<&Path>) -> io::Result<Self> {
        let path = registry_path(path, scope, "registry")?;
        if path.is_dir() {
            return FileRegistry::open(path)
                .map(Self::File)
                .map_err(io::Error::other);
        }
        let config = SignedConfig::read(&path, scope).map_err(|error| {
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
    Ok(RegistrySource::read(path, None)?.identity().to_owned())
}

pub(super) struct RegistrySources {
    explicit_paths: Vec<PathBuf>,
    explicit: OnceCell<BTreeMap<String, ExplicitRegistry>>,
    manifest: PathBuf,
    declared: BTreeMap<String, DeclaredRegistry>,
    runtime: OnceCell<Runtime>,
    state_root: PathBuf,
}

struct ExplicitRegistry {
    path: PathBuf,
    source: RegistrySource,
}

struct DeclaredRegistry {
    path: PathBuf,
    source: OnceCell<RegistrySource>,
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
    pub fn new(paths: &[PathBuf], declarations: RegistryDeclarations, state_root: PathBuf) -> Self {
        Self {
            explicit_paths: paths.to_vec(),
            explicit: OnceCell::new(),
            manifest: declarations.manifest,
            declared: declarations
                .paths
                .into_iter()
                .map(|(identity, path)| {
                    (
                        identity,
                        DeclaredRegistry {
                            path,
                            source: OnceCell::new(),
                        },
                    )
                })
                .collect(),
            runtime: OnceCell::new(),
            state_root,
        }
    }

    fn explicit_registry(&self, identity: &str) -> io::Result<Option<&RegistrySource>> {
        if self.explicit.get().is_none() {
            let mut explicit = BTreeMap::<_, ExplicitRegistry>::new();
            for path in &self.explicit_paths {
                let source = RegistrySource::read(path, None)?;
                let identity = source.identity().to_owned();
                match explicit.entry(identity) {
                    Entry::Vacant(entry) => {
                        entry.insert(ExplicitRegistry {
                            path: path.clone(),
                            source,
                        });
                    }
                    Entry::Occupied(entry) if !source.same_configuration(&entry.get().source) => {
                        return Err(io::Error::other(format!(
                            "conflicting --registry configurations for {:?}: {} and {}",
                            entry.key(),
                            entry.get().path.display(),
                            path.display()
                        )));
                    }
                    _ => {}
                }
            }
            self.explicit.get_or_init(|| explicit);
        }
        Ok(self
            .explicit
            .get()
            .expect("explicit registry configurations were initialized")
            .get(identity)
            .map(|entry| &entry.source))
    }

    fn registry(&self, id: &ModuleId) -> io::Result<&RegistrySource> {
        let SourceId::Registry(identity) = &id.source else {
            unreachable!("expected a registry module");
        };
        if let Some(source) = self.explicit_registry(identity)? {
            return Ok(source);
        }
        let declared = self.declared.get(identity).ok_or_else(|| {
            io::Error::other(format!(
                "registry {identity:?} is needed for {id}; declare it in zkcl.sexp or supply it with --registry"
            ))
        })?;
        if declared.source.get().is_none() {
            let root = self
                .manifest
                .parent()
                .expect("registry manifest has a parent");
            let path = root.join(&declared.path);
            let source = RegistrySource::read(&path, Some(root)).map_err(|error| {
                io::Error::other(format!(
                    "{}: registry {identity:?}: {error}",
                    self.manifest.display()
                ))
            })?;
            if source.identity() != identity {
                return Err(io::Error::other(format!(
                    "{}: registry {identity:?} points to {} with identity {:?}",
                    self.manifest.display(),
                    path.display(),
                    source.identity()
                )));
            }
            declared.source.get_or_init(|| source);
        }
        Ok(declared
            .source
            .get()
            .expect("declared registry was initialized"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> tempfile::TempDir {
        let directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/project-config");
        fs::create_dir_all(&directory).unwrap();
        tempfile::tempdir_in(directory.canonicalize().unwrap()).unwrap()
    }

    fn requested() -> ModuleId {
        ModuleId {
            name: "test/module".parse().unwrap(),
            source: SourceId::Registry("private".into()),
        }
    }

    #[test]
    fn loads_active_registry_declarations_and_checks_source_identity() {
        let temporary = scratch();
        let root = temporary.path();
        let declarations = RegistryDeclarations {
            manifest: root.join("zkcl.sexp"),
            paths: BTreeMap::from([
                ("private".into(), PathBuf::from("local")),
                ("unused".into(), PathBuf::from("missing")),
            ]),
        };
        let local = root.join("local");
        FileRegistry::init(&local, "private").unwrap();
        let configured = RegistrySources::new(&[], declarations.clone(), root.join("cache"));
        assert!(configured.records(&requested()).unwrap().is_empty());

        fs::remove_dir_all(&local).unwrap();
        FileRegistry::init(&local, "wrong").unwrap();
        let configured = RegistrySources::new(&[], declarations, root.join("cache"));
        let error = configured.records(&requested()).err().unwrap().to_string();
        assert!(error.contains("zkcl.sexp: registry \"private\" points to"));
        assert!(error.contains("with identity \"wrong\""));
    }

    #[test]
    fn explicit_registry_overrides_declared_path_and_conflicting_flags_fail() {
        let temporary = scratch();
        let root = temporary.path();
        let declarations = RegistryDeclarations {
            manifest: root.join("zkcl.sexp"),
            paths: BTreeMap::from([("private".into(), PathBuf::from("missing"))]),
        };
        let first = root.join("first");
        let second = root.join("second");
        FileRegistry::init(&first, "private").unwrap();
        FileRegistry::init(&second, "private").unwrap();
        let paths = [first.clone(), first.clone()];
        let configured = RegistrySources::new(&paths, declarations.clone(), root.join("cache"));
        assert!(configured.records(&requested()).unwrap().is_empty());

        let configured = RegistrySources::new(&[first, second], declarations, root.join("cache"));
        let error = configured.records(&requested()).err().unwrap().to_string();
        assert!(error.contains("conflicting --registry configurations for \"private\""));
        assert!(error.contains(&root.join("first").display().to_string()));
        assert!(error.contains(&root.join("second").display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn declared_registry_and_trust_root_cannot_escape_manifest_directory() {
        let temporary = scratch();
        let scratch = temporary.path();
        let root = scratch.join("project");
        fs::create_dir(&root).unwrap();
        let outside = scratch.join("outside");
        FileRegistry::init(&outside, "private").unwrap();
        let descriptor = root.join("registry.json");
        std::os::unix::fs::symlink(&outside, &descriptor).unwrap();
        let declarations = RegistryDeclarations {
            manifest: root.join("zkcl.sexp"),
            paths: BTreeMap::from([("private".into(), PathBuf::from("registry.json"))]),
        };
        let configured = RegistrySources::new(&[], declarations.clone(), scratch.join("cache"));
        let error = configured.records(&requested()).err().unwrap().to_string();
        assert!(error.contains("registry.json resolves outside"));

        fs::remove_file(&descriptor).unwrap();
        fs::write(outside.join("root.json"), "{}").unwrap();
        std::os::unix::fs::symlink(outside.join("root.json"), root.join("root.json")).unwrap();
        fs::write(
            &descriptor,
            r#"{"format":1,"identity":"private","metadata_url":"https://example.invalid/metadata/",
            "targets_url":"https://example.invalid/targets/","trusted_root":"root.json"}"#,
        )
        .unwrap();
        let configured = RegistrySources::new(&[], declarations, scratch.join("cache"));
        let error = configured.records(&requested()).err().unwrap().to_string();
        assert!(error.contains("root.json resolves outside"));
        assert_eq!(configured_identity(&descriptor).unwrap(), "private");
    }
}
