//! Export a coherent sparse registry snapshot for offline signing or file hosting.

use crate::signed::{
    SPARSE_INDEX_MAX_BYTES, SPARSE_SHARD_MAX_MODULES, SPARSE_V1_MARKER_TARGET, SignedModuleIndex,
    SignedRegistryError, SignedReleaseRecord, SparseModule, SparseShard, sparse_index_path,
    sparse_marker_bytes, sparse_shard_prefix, sparse_shard_target,
};
use crate::{ContentStore, Digest, FileRegistry, RegistryError, platform};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::TempDir;
use tough::TargetName;
use tough::editor::{RepositoryEditor, signed::PathExists};
use tough::key_source::{KeySource, LocalKeySource};
use tough::schema::{Root, Signed};

type Result<T> = std::result::Result<T, SignedRegistryError>;

/// The number of modules, nonempty shards, and distinct archives exported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SparseExport {
    pub modules: usize,
    pub shards: usize,
    pub sources: usize,
}

#[derive(Clone, Debug)]
pub struct SparseSigningOptions {
    /// A self-signed TUF root with consistent snapshots enabled.
    pub trusted_root: PathBuf,
    /// Local PEM keys sufficient to sign targets, snapshot, and timestamp.
    pub signing_keys: Vec<PathBuf>,
    /// The new version of all three signing roles. Advance it for each export.
    pub version: NonZeroU64,
    /// An RFC 3339 expiration for the three signing roles.
    pub expires: String,
}

/// Prepare unsigned marker and shard inputs in `targets/`, alongside raw
/// module indexes and archives. The destination must not already exist.
pub fn export_sparse(registry: &FileRegistry, output: &Path) -> Result<SparseExport> {
    PreparedExport::new(registry, output)?.install()
}

/// Produce a self-contained TUF metadata and target directory for a single
/// snapshot. The supplied root's public bytes are copied, and the private keys
/// are only read from their supplied paths. The destination must be new.
pub async fn export_signed_sparse(
    registry: &FileRegistry,
    output: &Path,
    options: SparseSigningOptions,
) -> Result<SparseExport> {
    let root_bytes = read_trusted_root(&options.trusted_root)?;
    let root: Signed<Root> = serde_json::from_slice(&root_bytes)
        .map_err(|error| invalid(format!("invalid TUF root: {error}")))?;
    if !root.signed.consistent_snapshot {
        return Err(invalid("TUF root must enable consistent snapshots"));
    }
    root.signed
        .verify_role(&root)
        .map_err(|error| invalid(format!("invalid TUF root signatures: {error}")))?;
    let now = SystemTime::now()
        .try_into()
        .map_err(|_| invalid("system clock is outside the TUF timestamp range"))?;
    if root.signed.expires <= now {
        return Err(invalid("TUF root has expired"));
    }
    let expires = options
        .expires
        .parse()
        .map_err(|error| invalid(format!("invalid TUF expiration: {error}")))?;
    if expires <= now {
        return Err(invalid("TUF expiration must be in the future"));
    }
    if options.signing_keys.is_empty() {
        return Err(invalid("no TUF signing keys provided"));
    }

    let registry = registry.clone();
    let output = output.to_owned();
    let prepared = tokio::task::spawn_blocking(move || PreparedExport::new(&registry, &output))
        .await
        .map_err(|error| invalid(format!("export sparse registry: {error}")))??;
    let seed = prepared.staging.path().join("root.json");
    fs::write(&seed, root_bytes)?;
    let mut editor = RepositoryEditor::new(seed).await?;
    editor
        .targets_version(options.version)?
        .targets_expires(expires)?;
    editor
        .snapshot_version(options.version)
        .snapshot_expires(expires);
    editor
        .timestamp_version(options.version)
        .timestamp_expires(expires);

    let targets = prepared.repository.join("targets");
    let mut inputs = Vec::with_capacity(prepared.tuf_targets.len());
    for name in &prepared.tuf_targets {
        let path = targets.join(name);
        let name = TargetName::new(name)?;
        let (_, target) = RepositoryEditor::build_target(&path).await?;
        editor.add_target(name.clone(), target)?;
        inputs.push((name, path));
    }
    let keys: Vec<Box<dyn KeySource>> = options
        .signing_keys
        .into_iter()
        .map(|path| Box::new(LocalKeySource { path }) as Box<dyn KeySource>)
        .collect();
    let signed = editor.sign(&keys).await?;
    for (name, path) in inputs {
        signed
            .copy_target(&path, &targets, PathExists::Fail, Some(&name))
            .await?;
        fs::remove_file(path)?;
    }
    fs::remove_dir(targets.join("v1/shards"))?;
    signed.write(prepared.repository.join("metadata")).await?;
    // A long export must still be fresh by the time it is ready to install.
    let now = SystemTime::now()
        .try_into()
        .map_err(|_| invalid("system clock is outside the TUF timestamp range"))?;
    if expires <= now || root.signed.expires <= now {
        return Err(invalid("TUF metadata expired during export"));
    }
    prepared.install()
}

struct PreparedExport {
    staging: TempDir,
    repository: PathBuf,
    destination: PathBuf,
    tuf_targets: Vec<String>,
    summary: SparseExport,
}

impl PreparedExport {
    fn new(registry: &FileRegistry, output: &Path) -> Result<Self> {
        if !platform::HAS_ATOMIC_NOREPLACE_RENAME {
            return Err(invalid(
                "atomic registry export is unsupported on this platform",
            ));
        }
        let name = output
            .file_name()
            .ok_or_else(|| invalid("registry export requires a destination directory name"))?;
        match fs::symlink_metadata(output) {
            Ok(_) => {
                return Err(invalid(format!(
                    "export destination already exists: {}",
                    output.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let parent = output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let parent = fs::canonicalize(parent)?;
        let destination = parent.join(name);
        let staging = tempfile::Builder::new()
            .prefix(".zkcl-registry-export-")
            .tempdir_in(parent)?;
        let repository = staging.path().join("repository");
        let targets = repository.join("targets");
        let sources = ContentStore::new(targets.join("sources"));
        fs::create_dir_all(sources.root())?;
        fs::create_dir(targets.join("v1"))?;
        fs::create_dir(targets.join("v1/index"))?;
        fs::create_dir(targets.join("v1/shards"))?;
        fs::write(
            targets.join(SPARSE_V1_MARKER_TARGET),
            sparse_marker_bytes(registry.id())?,
        )?;

        let mut shards = BTreeMap::<String, SparseShard>::new();
        let mut archived_manifests = BTreeMap::<Digest, Digest>::new();
        let mut modules = 0;
        registry.visit_snapshot(SPARSE_INDEX_MAX_BYTES as usize, |module, records| {
            let prefix = sparse_shard_prefix(&module);
            let shard = shards.entry(prefix).or_insert_with(|| SparseShard {
                modules: Vec::new(),
            });
            if shard.modules.len() == SPARSE_SHARD_MAX_MODULES {
                return Err(invalid("sparse shard module count limit exceeded"));
            }
            let releases = records
                .into_iter()
                .map(|record| {
                    let checksum = record.checksum;
                    let manifest = match archived_manifests.entry(checksum) {
                        std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            sources.import(registry.store().open_archive(checksum)?, checksum)?;
                            let manifest =
                                ContentStore::archive_manifest(sources.open_archive(checksum)?)?;
                            *entry.insert(Digest::of_bytes(manifest.as_bytes()))
                        }
                    };
                    if Digest::of_bytes(record.manifest.as_bytes()) != manifest {
                        return Err(invalid(format!(
                            "release {module}@{} manifest differs from archived zkcl.sexp",
                            record.version,
                        )));
                    }
                    Ok(SignedReleaseRecord {
                        version: record.version,
                        checksum: checksum.to_string(),
                        manifest: record.manifest,
                        yanked: record.yanked,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let index = SignedModuleIndex { module, releases };
            let bytes = index.to_sparse_bytes()?;
            let checksum = Digest::of_bytes(&bytes);
            fs::write(targets.join(sparse_index_path(checksum)), &bytes)?;
            shard.modules.push(SparseModule {
                module: index.module,
                length: bytes.len() as u64,
                sha256: checksum.to_string(),
            });
            modules += 1;
            Ok(())
        })?;

        let summary = SparseExport {
            modules,
            shards: shards.len(),
            sources: archived_manifests.len(),
        };
        let mut tuf_targets = vec![SPARSE_V1_MARKER_TARGET.to_owned()];
        for (prefix, shard) in shards {
            let name = sparse_shard_target(&prefix);
            fs::write(targets.join(&name), shard.to_bytes(&prefix)?)?;
            tuf_targets.push(name);
        }
        Ok(Self {
            staging,
            repository,
            destination,
            tuf_targets,
            summary,
        })
    }

    fn install(self) -> Result<SparseExport> {
        platform::install_directory(&self.repository, &self.destination).map_err(|source| {
            RegistryError::path_io("install sparse registry export", &self.destination, source)
        })?;
        Ok(self.summary)
    }
}

fn read_trusted_root(path: &Path) -> Result<Vec<u8>> {
    const MAX_ROOT_BYTES: u64 = 1024 * 1024;
    if !fs::metadata(path)
        .map_err(|source| RegistryError::path_io("inspect TUF root", path, source))?
        .is_file()
    {
        return Err(invalid("TUF root must be a regular file"));
    }
    let file =
        File::open(path).map_err(|source| RegistryError::path_io("open TUF root", path, source))?;
    let mut bytes = Vec::new();
    file.take(MAX_ROOT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_ROOT_BYTES {
        return Err(invalid("TUF root exceeds its byte limit"));
    }
    Ok(bytes)
}

fn invalid(message: impl Into<String>) -> SignedRegistryError {
    SignedRegistryError::Invalid(message.into())
}
