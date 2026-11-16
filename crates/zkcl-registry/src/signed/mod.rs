//! TUF registry client and sparse v1 wire format. Trust roots are provisioned
//! separately from registry identities and mirror URLs.
mod sparse;

use crate::{ArchiveLimits, ContentStore, Digest, ReleaseRecord, file_registry::validate_identity};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;
use tough::{RepositoryLoader, Transport, TransportError, TransportErrorKind, TransportStream};
use url::Url;

use sparse::{MARKER_MAX_BYTES, Marker, ObjectReference, shard_target_prefix};
pub use sparse::{
    SPARSE_INDEX_MAX_BYTES, SPARSE_INDEX_MAX_RELEASES, SPARSE_SHARD_MAX_BYTES,
    SPARSE_SHARD_MAX_MODULES, SPARSE_V1_MARKER_TARGET, SparseModule, SparseShard,
    sparse_index_path, sparse_marker_bytes, sparse_shard_prefix, sparse_shard_target,
    sparse_source_path,
};

const METADATA_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SignedRegistryError {
    #[error("{0}")]
    Invalid(String),
    #[error("TUF verification: {0}")]
    Tuf(#[source] Box<tough::error::Error>),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("invalid signed index: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid archive: {0}")]
    Archive(#[from] crate::RegistryError),
    #[error("HTTP transport: {0}")]
    Http(#[from] reqwest::Error),
    #[error("registry transport: {0}")]
    Transport(#[from] TransportError),
}

impl From<tough::error::Error> for SignedRegistryError {
    fn from(error: tough::error::Error) -> Self {
        Self::Tuf(Box::new(error))
    }
}

type Result<T> = std::result::Result<T, SignedRegistryError>;

/// Explicit operator trust configuration. Keep the originally provisioned root
/// bytes across root rotation: TUF verifies the successive replacement roots.
#[derive(Clone, Debug)]
pub struct SignedRegistryConfig {
    pub identity: String,
    pub trusted_root: Vec<u8>,
    pub metadata_url: Url,
    pub targets_url: Url,
    /// Persistent private client state; never a temporary per-invocation folder.
    pub state_directory: PathBuf,
}

/// Wire schema carried by a module index object.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignedModuleIndex {
    pub module: String,
    pub releases: Vec<SignedReleaseRecord>,
}
impl SignedModuleIndex {
    fn validate(&self) -> Result<()> {
        if self.releases.len() > SPARSE_INDEX_MAX_RELEASES {
            return Err(invalid("signed release count limit"));
        }
        let mut versions = BTreeSet::new();
        for record in &self.releases {
            crate::file_registry::validate_version(&record.version)?;
            let _: Digest = record.checksum.parse()?;
            if record.manifest.len() > crate::MAX_MANIFEST_BYTES
                || !versions.insert(record.version.as_str())
            {
                return Err(invalid(
                    "duplicate version or oversized manifest in signed index",
                ));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SignedReleaseRecord {
    pub version: String,
    pub checksum: String,
    pub manifest: String,
    pub yanked: bool,
}
impl SignedReleaseRecord {
    pub fn release_record(&self) -> Result<ReleaseRecord> {
        Ok(ReleaseRecord {
            version: self.version.clone(),
            checksum: self.checksum.parse()?,
            manifest: self.manifest.clone(),
            yanked: self.yanked,
        })
    }
}

/// Release verified through this registry identity, trust root and snapshot.
#[derive(Clone, Debug)]
pub struct TrustedRelease {
    record: SignedReleaseRecord,
    module: String,
    identity: Arc<str>,
    root: Digest,
    snapshot: Digest,
    index: Digest,
}
impl TrustedRelease {
    pub fn record(&self) -> &SignedReleaseRecord {
        &self.record
    }
    pub fn index_digest(&self) -> Digest {
        self.index
    }
    pub fn module(&self) -> &str {
        &self.module
    }
}

#[derive(Debug)]
pub struct SignedRegistry {
    identity: Arc<str>,
    root: Digest,
    snapshot: Digest,
    staging: PathBuf,
    targets_url: Url,
    transport: RegistryTransport,
    objects: PathBuf,
    expires: SystemTime,
    shards: BTreeMap<String, ObjectReference>,
}

impl SignedRegistry {
    pub async fn load(config: SignedRegistryConfig) -> Result<Self> {
        validate_identity(&config.identity)?;
        if config.trusted_root.len() > 1024 * 1024 {
            return Err(invalid("trusted root exceeds limit"));
        }
        let scheme = config.metadata_url.scheme();
        if !matches!(scheme, "https" | "file") || config.targets_url.scheme() != scheme {
            return Err(invalid(
                "registry endpoints must both use HTTPS, or explicit file mirrors",
            ));
        }
        for url in [&config.metadata_url, &config.targets_url] {
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(invalid(
                    "registry endpoint must not contain credentials, query or fragment",
                ));
            }
        }
        let root = Digest::of_bytes(&config.trusted_root);
        let directory = config.state_directory.join(root.to_string());
        tokio::fs::create_dir_all(&directory).await?;
        let lock_path = directory.join("refresh.lock");
        let lock = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(lock_path)?;
            lock.lock()?;
            Ok(lock)
        })
        .await
        .map_err(|e| invalid(&format!("registry lock task: {e}")))??;
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        let transport = RegistryTransport {
            client,
            file: scheme == "file",
        };
        let targets_url = directory_url(config.targets_url)?;
        let gate = Arc::new(MetadataGate::default());
        let top_level = TopLevelTransport {
            inner: transport.clone(),
            gate: gate.clone(),
        };
        let repository = RepositoryLoader::new(
            &config.trusted_root,
            config.metadata_url,
            targets_url.clone(),
        )
        .transport(top_level)
        .datastore(&directory)
        .expiration_enforcement(tough::ExpirationEnforcement::Safe)
        .limits(tough::Limits {
            max_targets_size: METADATA_LIMIT,
            ..Default::default()
        })
        .load()
        .await?;
        gate.loaded.store(true, Ordering::Release);
        let snapshot_hash = &repository
            .timestamp()
            .signed
            .meta
            .get("snapshot.json")
            .and_then(|metadata| metadata.hashes.as_ref())
            .ok_or_else(|| invalid("signed registry snapshot must carry a SHA-256 hash"))?
            .sha256;
        let snapshot = Digest::from_bytes(
            snapshot_hash
                .as_ref()
                .try_into()
                .map_err(|_| invalid("invalid signed snapshot SHA-256 length"))?,
        );
        let targets = &repository.targets().signed;
        if targets
            .delegations
            .as_ref()
            .is_some_and(|delegations| !delegations.roles.is_empty())
        {
            return Err(invalid("signed registries cannot use TUF delegations"));
        }
        let snapshot_role = &repository.snapshot().signed;
        if !repository.root().signed.consistent_snapshot
            || snapshot_role.meta.len() != 1
            || !snapshot_role
                .meta
                .get("targets.json")
                .is_some_and(|metadata| metadata.hashes.is_some())
        {
            return Err(invalid(
                "sparse registry requires consistent TUF snapshots with hashed top-level targets",
            ));
        }
        let mut marker = None;
        let mut shards = BTreeMap::new();
        for (name, target) in &targets.targets {
            if name.raw() == SPARSE_V1_MARKER_TARGET {
                marker = Some(ObjectReference::from_target(target, MARKER_MAX_BYTES)?);
            } else {
                let prefix = shard_target_prefix(name.raw())
                    .ok_or_else(|| invalid("unexpected TUF target in sparse registry"))?;
                shards.insert(
                    prefix.to_owned(),
                    ObjectReference::from_target(target, SPARSE_SHARD_MAX_BYTES)?,
                );
            }
        }
        let marker = marker.ok_or_else(|| invalid("signed sparse registry marker missing"))?;
        let staging = directory.join("staging");
        let objects = directory.join("metadata-objects");
        tokio::fs::create_dir_all(&staging).await?;
        tokio::fs::create_dir_all(&objects).await?;
        let expires = [
            repository.root().signed.expires,
            repository.timestamp().signed.expires,
            snapshot_role.expires,
            targets.expires,
        ]
        .into_iter()
        .map(SystemTime::from)
        .min()
        .expect("all four TUF roles have expiry times");
        drop(lock);
        let registry = Self {
            identity: config.identity.into(),
            root,
            snapshot,
            staging,
            targets_url,
            transport,
            objects,
            expires,
            shards,
        };
        registry.ensure_fresh()?;
        let bytes = registry
            .signed_target(SPARSE_V1_MARKER_TARGET, marker)
            .await?;
        let marker: Marker = serde_json::from_slice(&bytes)?;
        if marker.version != 1 {
            return Err(invalid("unsupported signed registry protocol version"));
        }
        if marker.identity != registry.identity.as_ref() {
            return Err(crate::RegistryError::RegistryIdentity {
                expected: registry.identity.to_string(),
                actual: marker.identity,
            }
            .into());
        }
        Ok(registry)
    }

    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn trust_root_digest(&self) -> Digest {
        self.root
    }
    pub fn snapshot_digest(&self) -> Digest {
        self.snapshot
    }

    pub async fn records(&self, module: &str) -> Result<Vec<TrustedRelease>> {
        crate::file_registry::validate_module(module)?;
        self.ensure_fresh()?;
        let prefix = sparse_shard_prefix(module);
        let Some(&target) = self.shards.get(&prefix) else {
            return Ok(Vec::new());
        };
        let bytes = self
            .signed_target(&sparse_shard_target(&prefix), target)
            .await?;
        let shard: SparseShard = serde_json::from_slice(&bytes)?;
        let Some(reference) = shard.find(&prefix, Some(module))? else {
            return Ok(Vec::new());
        };
        let bytes = self
            .cached_object(&sparse_index_path(reference.digest), reference)
            .await?;
        let index: SignedModuleIndex = serde_json::from_slice(&bytes)?;
        if index.module != module {
            return Err(invalid("signed index module identity mismatch"));
        }
        index.validate()?;
        Ok(index
            .releases
            .into_iter()
            .map(|record| TrustedRelease {
                record,
                module: module.to_owned(),
                identity: self.identity.clone(),
                root: self.root,
                snapshot: self.snapshot,
                index: reference.digest,
            })
            .collect())
    }

    /// Import only a record verified under this exact repository snapshot.
    /// The caller also checks module/version/manifest language semantics.
    pub async fn fetch(&self, release: &TrustedRelease, store: &ContentStore) -> Result<Digest> {
        self.ensure_fresh()?;
        if release.identity != self.identity
            || release.root != self.root
            || release.snapshot != self.snapshot
        {
            return Err(invalid(
                "release belongs to a different trusted registry snapshot",
            ));
        }
        let digest: Digest = release.record.checksum.parse()?;
        if !path_exists(&store.archive_path(digest))? {
            let mut file = tempfile::NamedTempFile::new_in(&self.staging)?;
            let stream = self.download(&sparse_source_path(digest)).await?;
            copy_target(stream, file.as_file_mut(), store.limits.max_archive_bytes).await?;
            file.as_file_mut().rewind()?;
            store.import(file.as_file_mut(), digest)?;
        }
        if store.manifest(digest)? != release.record.manifest {
            return Err(invalid("signed manifest differs from archived zkcl.sexp"));
        }
        Ok(digest)
    }

    fn ensure_fresh(&self) -> Result<()> {
        if SystemTime::now() >= self.expires {
            return Err(invalid("signed registry metadata expired"));
        }
        Ok(())
    }

    async fn signed_target(&self, name: &str, reference: ObjectReference) -> Result<Vec<u8>> {
        // Match tough's consistent snapshot target filename prefix. The
        // reference comes from the TUF-verified top-level targets document.
        self.cached_object(&format!("{}.{name}", reference.digest), reference)
            .await
    }

    async fn cached_object(&self, path: &str, reference: ObjectReference) -> Result<Vec<u8>> {
        let name = reference.digest.to_string();
        let directory =
            cap_std::fs::Dir::open_ambient_dir(&self.objects, cap_std::ambient_authority())?;
        match crate::open_file(&directory, &name) {
            Ok(file) => return read_cached_object(file, reference),
            Err(crate::RegistryError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut bytes = Vec::new();
        copy_target(self.download(path).await?, &mut bytes, reference.length).await?;
        reference.verify(&bytes)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.objects)?;
        file.write_all(&bytes)?;
        file.as_file_mut().sync_all()?;
        match file.persist_noclobber(self.objects.join(&name)) {
            Ok(_) => Ok(bytes),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                read_cached_object(crate::open_file(&directory, &name)?, reference)
            }
            Err(error) => Err(error.error.into()),
        }
    }

    async fn download(&self, relative_path: &str) -> Result<TransportStream> {
        let url = self
            .targets_url
            .join(relative_path)
            .map_err(|error| invalid(&format!("invalid registry target URL: {error}")))?;
        Ok(self.transport.fetch(url).await?)
    }
}

fn read_cached_object(file: File, reference: ObjectReference) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(reference.length + 1).read_to_end(&mut bytes)?;
    reference.verify(&bytes)?;
    Ok(bytes)
}

async fn copy_target<B: AsRef<[u8]>, E: Into<SignedRegistryError>>(
    stream: impl futures::Stream<Item = std::result::Result<B, E>>,
    writer: &mut impl Write,
    limit: u64,
) -> Result<()> {
    let mut stream = std::pin::pin!(stream);
    let mut count = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Into::into)?;
        let bytes = chunk.as_ref();
        count = count.saturating_add(bytes.len() as u64);
        if count > limit {
            return Err(invalid("signed target byte limit exceeded"));
        }
        writer.write_all(bytes)?;
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn directory_url(mut url: Url) -> Result<Url> {
    if !url.path().ends_with('/') {
        url.path_segments_mut()
            .map_err(|_| invalid("registry endpoint must refer to a directory"))?
            .push("");
    }
    Ok(url)
}

#[derive(Debug, Default)]
struct MetadataGate {
    targets_requested: AtomicBool,
    loaded: AtomicBool,
}

#[derive(Clone, Debug)]
struct TopLevelTransport {
    inner: RegistryTransport,
    gate: Arc<MetadataGate>,
}

#[tough::async_trait]
impl Transport for TopLevelTransport {
    async fn fetch(&self, url: Url) -> std::result::Result<TransportStream, TransportError> {
        // RepositoryLoader fetches roots, timestamp, snapshot and finally
        // targets. It would eagerly fetch delegated roles after that point.
        if !self.gate.loaded.load(Ordering::Acquire) {
            if self.gate.targets_requested.load(Ordering::Acquire) {
                return Err(TransportError::new_with_cause(
                    TransportErrorKind::Other,
                    url,
                    io::Error::other("signed registries cannot use TUF delegations"),
                ));
            }
            let filename = url.path().rsplit('/').next().unwrap_or_default();
            if filename == "targets.json"
                || filename
                    .strip_suffix(".targets.json")
                    .is_some_and(|version| version.parse::<u64>().is_ok())
            {
                self.gate.targets_requested.store(true, Ordering::Release);
            }
        }
        self.inner.fetch(url).await
    }
}

#[derive(Clone, Debug)]
struct RegistryTransport {
    client: reqwest::Client,
    file: bool,
}
#[tough::async_trait]
impl Transport for RegistryTransport {
    async fn fetch(&self, url: Url) -> std::result::Result<TransportStream, TransportError> {
        if self.file && url.scheme() == "file" {
            let stream = tough::FilesystemTransport.fetch(url.clone()).await?;
            return Ok(bounded_transport(stream, url));
        }
        if self.file || url.scheme() != "https" {
            return Err(TransportError::new(TransportErrorKind::Other, url));
        }
        let response = self.client.get(url.clone()).send().await.map_err(|e| {
            TransportError::new_with_cause(TransportErrorKind::Other, url.clone(), e)
        })?;
        let response = response.error_for_status().map_err(|e| {
            let kind = if e
                .status()
                .is_some_and(|s| s.as_u16() == 404 || s.as_u16() == 410)
            {
                TransportErrorKind::FileNotFound
            } else {
                TransportErrorKind::Other
            };
            TransportError::new_with_cause(kind, url.clone(), e)
        })?;
        let error_url = url.clone();
        let stream: TransportStream = Box::pin(response.bytes_stream().map(move |chunk| {
            chunk.map_err(|e| {
                TransportError::new_with_cause(TransportErrorKind::Other, error_url.clone(), e)
            })
        }));
        Ok(bounded_transport(stream, url))
    }
}
fn bounded_transport(stream: TransportStream, url: Url) -> TransportStream {
    let limit = if url.path().ends_with(".tar") {
        ArchiveLimits::default().max_archive_bytes
    } else {
        METADATA_LIMIT
    };
    let mut count = 0u64;
    Box::pin(stream.map(move |chunk| {
        let bytes = chunk?;
        count = count.saturating_add(bytes.len() as u64);
        if count > limit {
            Err(TransportError::new_with_cause(
                TransportErrorKind::Other,
                url.clone(),
                std::io::Error::other("registry transport byte limit exceeded"),
            ))
        } else {
            Ok(bytes)
        }
    }))
}
fn invalid(message: &str) -> SignedRegistryError {
    SignedRegistryError::Invalid(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::num::NonZeroU64;
    use std::path::Path;
    use tough::TargetName;
    use tough::editor::{RepositoryEditor, signed::PathExists};
    use tough::key_source::LocalKeySource;
    use tough::schema::Target;

    const ROOT: &[u8] = include_bytes!("test_keys/root.json");
    const KEY: &[u8] = include_bytes!("test_keys/test-only-private.pem");

    fn config(root: &Path, state: &Path) -> SignedRegistryConfig {
        SignedRegistryConfig {
            identity: "test-registry".into(),
            trusted_root: ROOT.to_vec(),
            metadata_url: Url::from_directory_path(root.join("metadata")).unwrap(),
            targets_url: Url::from_directory_path(root.join("targets")).unwrap(),
            state_directory: state.to_owned(),
        }
    }

    async fn repository(root: &Path, version: u64, expires: &str, wrong_module: bool) -> Digest {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("root.json"), ROOT).unwrap();
        fs::write(root.join("key.pem"), KEY).unwrap();
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        let manifest = "(project \"zk/test\" (version \"1.0.0\"))";
        fs::write(source.join("zkcl.sexp"), manifest).unwrap();
        fs::write(source.join("main.lisp"), "(print :signed)").unwrap();
        let archive = root.join("source.tar");
        let digest = crate::write_source_archive(
            &source,
            &["zkcl.sexp", "main.lisp"],
            fs::File::create(&archive).unwrap(),
        )
        .unwrap();
        let targets = root.join("targets");
        fs::create_dir_all(targets.join("sources")).unwrap();
        fs::create_dir_all(targets.join("v1/index")).unwrap();
        fs::rename(archive, targets.join(sparse_source_path(digest))).unwrap();
        let module = "zk/test";
        let index = SignedModuleIndex {
            module: if wrong_module { "other/test" } else { module }.into(),
            releases: vec![SignedReleaseRecord {
                version: "1.0.0".into(),
                checksum: digest.to_string(),
                manifest: manifest.into(),
                yanked: false,
            }],
        };
        let bytes = index.to_sparse_bytes().unwrap();
        let index_digest = Digest::of_bytes(&bytes);
        fs::write(targets.join(sparse_index_path(index_digest)), &bytes).unwrap();
        let prefix = sparse_shard_prefix(module);
        let shard = SparseShard {
            modules: vec![SparseModule {
                module: module.into(),
                length: bytes.len() as u64,
                sha256: index_digest.to_string(),
            }],
        };
        let inputs = [
            (
                SPARSE_V1_MARKER_TARGET.to_owned(),
                sparse_marker_bytes("test-registry").unwrap(),
            ),
            (
                sparse_shard_target(&prefix),
                shard.to_bytes(&prefix).unwrap(),
            ),
        ];
        let mut editor = RepositoryEditor::new(root.join("root.json")).await.unwrap();
        let version = NonZeroU64::new(version).unwrap();
        editor
            .targets_version(version)
            .unwrap()
            .targets_expires(expires.parse().unwrap())
            .unwrap()
            .snapshot_version(version)
            .snapshot_expires(expires.parse().unwrap())
            .timestamp_version(version)
            .timestamp_expires(expires.parse().unwrap());
        let mut staged = Vec::new();
        for (index, (name, bytes)) in inputs.into_iter().enumerate() {
            let path = root.join(format!("target-{index}.json"));
            fs::write(&path, bytes).unwrap();
            let name = TargetName::new(name).unwrap();
            editor
                .add_target(name.clone(), Target::from_path(&path).await.unwrap())
                .unwrap();
            staged.push((name, path));
        }
        let signed = editor
            .sign(&[Box::new(LocalKeySource {
                path: root.join("key.pem"),
            })])
            .await
            .unwrap();
        signed.write(root.join("metadata")).await.unwrap();
        for (name, path) in staged {
            signed
                .copy_target(&path, &targets, PathExists::Fail, Some(&name))
                .await
                .unwrap();
        }
        digest
    }

    #[tokio::test(flavor = "current_thread")]
    async fn signed_index_and_archive_verify_end_to_end_and_reject_tampering() {
        let root = crate::tests::temporary();
        let mirror = root.path().join("mirror");
        let state = root.path().join("client");
        let digest = repository(&mirror, 1, "3000-01-01T00:00:00Z", false).await;
        let registry = SignedRegistry::load(config(&mirror, &state)).await.unwrap();
        let records = registry.records("zk/test").await.unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record().checksum, digest.to_string());
        let store = ContentStore::new(root.path().join("store"));
        assert_eq!(registry.fetch(&records[0], &store).await.unwrap(), digest);
        assert!(store.manifest(digest).unwrap().contains("zk/test"));
        assert!(registry.records("zk/missing").await.unwrap().is_empty());
        let targets = mirror.join("targets");
        let archive_path = targets.join(sparse_source_path(digest));
        fs::write(&archive_path, "tampered").unwrap();
        let rejected_store = ContentStore::new(root.path().join("rejected-store"));
        assert!(registry.fetch(&records[0], &rejected_store).await.is_err());
        assert!(!rejected_store.archive_path(digest).exists());
        let index = targets.join(sparse_index_path(records[0].index_digest()));
        fs::write(index, "{}").unwrap();
        assert_eq!(registry.records("zk/test").await.unwrap().len(), 1);
        let cold = SignedRegistry::load(config(&mirror, &root.path().join("cold")))
            .await
            .unwrap();
        assert!(cold.records("zk/test").await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rollback_state_survives_reopen_and_expiry_is_never_disabled() {
        let root = crate::tests::temporary();
        let old = root.path().join("old");
        let new = root.path().join("new");
        let state = root.path().join("client");
        repository(&old, 1, "3000-01-01T00:00:00Z", false).await;
        repository(&new, 2, "3000-01-01T00:00:00Z", false).await;
        SignedRegistry::load(config(&new, &state)).await.unwrap();
        // Mirror location changes preserve the same root's anti-rollback state.
        assert!(SignedRegistry::load(config(&old, &state)).await.is_err());
        let expired = root.path().join("expired");
        repository(&expired, 3, "2000-01-01T00:00:00Z", false).await;
        assert!(
            SignedRegistry::load(config(&expired, &state))
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_wrong_module_signature_and_insecure_endpoints() {
        let root = crate::tests::temporary();
        let mirror = root.path().join("mirror");
        let state = root.path().join("client");
        repository(&mirror, 1, "3000-01-01T00:00:00Z", true).await;
        let registry = SignedRegistry::load(config(&mirror, &state)).await.unwrap();
        assert!(registry.records("zk/test").await.is_err());
        let mut insecure = config(&mirror, &state);
        insecure.metadata_url = Url::parse("http://example.invalid/metadata/").unwrap();
        insecure.targets_url = Url::parse("http://example.invalid/targets/").unwrap();
        assert!(SignedRegistry::load(insecure).await.is_err());
        let timestamp = mirror.join("metadata/timestamp.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&timestamp).unwrap()).unwrap();
        value["signed"]["version"] = 9.into();
        fs::write(timestamp, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            SignedRegistry::load(config(&mirror, &root.path().join("fresh")))
                .await
                .is_err()
        );
    }
}
