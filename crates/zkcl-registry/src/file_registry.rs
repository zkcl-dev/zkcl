use super::{ContentStore, Digest, MAX_MANIFEST_BYTES, RegistryError, Result, open_file};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use std::error::Error as StdError;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const IDENTITY_FILE: &str = "registry-id";
const WRITE_LOCK: &str = "registry.lock";
const IDENTITY_MAGIC: &str = "zkcl-file-registry\n";
const RECORD_MAGIC: &str = "zkcl-release";
const MAX_RECORD_BYTES: usize = MAX_MANIFEST_BYTES + 512;
const MAX_RELEASES: usize = 16_384;

/// Immutable source metadata and current withdrawal state for one release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseRecord {
    pub version: String,
    pub checksum: Digest,
    pub manifest: String,
    /// Withdrawn releases remain fetchable by checksum but are excluded from new solves.
    pub yanked: bool,
}

/// A sparse, filesystem-backed development or private registry.
///
/// Selecting this local directory establishes trust in its release metadata.
/// The identity marker and release records are immutable through this API.
/// Archives live in a shared content-addressed store below `content/`.
/// This is a trusted local administrator API: possession of this handle permits
/// publication under every namespace, including `zk`. Its identity string is
/// an identifier, never evidence of official status or publisher authorization.
/// Remote services must enforce publisher authority before exposing mutations.
#[derive(Clone, Debug)]
pub struct FileRegistry {
    root: PathBuf,
    id: String,
    store: ContentStore,
}

impl FileRegistry {
    /// Initialize a registry, or reopen it if its identity is exactly `id`.
    pub fn init(root: impl Into<PathBuf>, id: impl Into<String>) -> Result<Self> {
        let root = root.into();
        let id = id.into();
        validate_identity(&id)?;
        fs::create_dir_all(&root)?;
        let root = fs::canonicalize(root)?;
        let mut marker = tempfile::Builder::new()
            .prefix(".pending-")
            .tempfile_in(&root)?;
        writeln!(marker, "{IDENTITY_MAGIC}{id}")?;
        marker.as_file_mut().sync_all()?;
        match marker.persist_noclobber(root.join(IDENTITY_FILE)) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.error.into()),
        }
        let registry = Self::open(root)?;
        if registry.id != id {
            return Err(RegistryError::RegistryIdentity {
                expected: id,
                actual: registry.id,
            });
        }
        Ok(registry)
    }

    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        let root = fs::canonicalize(&root)
            .map_err(|source| RegistryError::path_io("cannot open registry", &root, source))?;
        let directory = Dir::open_ambient_dir(&root, cap_std::ambient_authority())?;
        let id = at_metadata_file(root.join(IDENTITY_FILE), || {
            let bytes = read_limited(open_file(&directory, IDENTITY_FILE)?, 2048)?;
            let text = std::str::from_utf8(&bytes)?;
            let id = text
                .strip_prefix(IDENTITY_MAGIC)
                .and_then(|text| text.strip_suffix('\n'))
                .ok_or_else(|| registry_error("invalid identity marker"))?;
            validate_identity(id)?;
            Ok(id.to_owned())
        })?;
        Ok(Self {
            store: ContentStore::new(root.join("content")),
            root,
            id,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> &ContentStore {
        &self.store
    }

    /// Read only this module's metadata. No source archives are unpacked.
    pub fn records(&self, module: &str) -> Result<Vec<ReleaseRecord>> {
        self.records_bounded(module, usize::MAX)
    }

    fn records_bounded(
        &self,
        module: &str,
        mut remaining_manifest_bytes: usize,
    ) -> Result<Vec<ReleaseRecord>> {
        let Some(directory) = self.module_directory(module, false)? else {
            return Ok(Vec::new());
        };
        let path = self.root.join("index").join(module);
        let state_directory = self.directory("withdrawals", module, false)?;
        let mut records = Vec::new();
        let entries = directory.entries().map_err(|source| {
            RegistryError::path_io("cannot read registry index", &path, source)
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| {
                RegistryError::path_io("cannot read registry index", &path, source)
            })?;
            if records.len() >= MAX_RELEASES {
                return Err(registry_error("module release count limit exceeded"));
            }
            let mut record = read_record(&directory, &path, &entry.file_name())?;
            remaining_manifest_bytes = remaining_manifest_bytes
                .checked_sub(record.manifest.len())
                .ok_or_else(|| registry_error("module manifest byte limit exceeded"))?;
            record.yanked = self.read_yanked(state_directory.as_ref(), module, &record)?;
            records.push(record);
        }
        records.sort_by(|left, right| left.version.cmp(&right.version));
        Ok(records)
    }

    #[cfg(feature = "signed")]
    pub(crate) fn visit_snapshot<E>(
        &self,
        max_manifest_bytes: usize,
        mut visit: impl FnMut(String, Vec<ReleaseRecord>) -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E>
    where
        E: From<RegistryError>,
    {
        let lock = self.open_write_lock()?;
        lock.lock_shared().map_err(RegistryError::from)?;
        let root = Dir::open_ambient_dir(&self.root, cap_std::ambient_authority())
            .map_err(RegistryError::from)?;
        let index = match root.open_dir_nofollow("index") {
            Ok(index) => index,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(RegistryError::from(error).into()),
        };
        for namespace in index.entries().map_err(RegistryError::from)? {
            let namespace = namespace.map_err(RegistryError::from)?.file_name();
            let namespace = namespace
                .to_str()
                .ok_or_else(|| registry_error("non-UTF-8 registry namespace"))?;
            validate_module(&format!("{namespace}/x"))?;
            let directory = index
                .open_dir_nofollow(namespace)
                .map_err(RegistryError::from)?;
            for name in directory.entries().map_err(RegistryError::from)? {
                let name = name.map_err(RegistryError::from)?.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| registry_error("non-UTF-8 registry module name"))?;
                let module = format!("{namespace}/{name}");
                let records = self.records_bounded(&module, max_manifest_bytes)?;
                if !records.is_empty() {
                    visit(module, records)?;
                }
            }
        }
        Ok(())
    }

    fn open_write_lock(&self) -> Result<File> {
        let directory = Dir::open_ambient_dir(&self.root, cap_std::ambient_authority())?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No)
            .nonblock(true);
        let lock = directory.open_with(WRITE_LOCK, &options)?.into_std();
        if !lock.metadata()?.is_file() {
            return Err(registry_error("registry write lock is not a regular file"));
        }
        Ok(lock)
    }

    /// Publish selected sources and their exact root `zkcl.sexp` contents.
    /// Repeating the same release succeeds only when checksum and metadata match.
    pub fn publish<P: AsRef<Path>>(
        &self,
        module: &str,
        version: &str,
        source_root: impl AsRef<Path>,
        files: &[P],
        manifest: &str,
    ) -> Result<Digest> {
        validate_module(module)?;
        validate_version(version)?;
        if manifest.len() > MAX_MANIFEST_BYTES {
            return Err(registry_error("manifest byte limit exceeded"));
        }
        let checksum = self.store.publish(source_root, files)?;
        if self.store.manifest(checksum)? != manifest {
            return Err(registry_error(
                "release metadata differs from archived zkcl.sexp",
            ));
        }
        self.record_archive(module, version, checksum, manifest)
    }

    /// Trusted administrator import of an already canonical archive. The bytes
    /// and manifest are verified before immutable release metadata is installed.
    pub fn import_archive(
        &self,
        module: &str,
        version: &str,
        checksum: Digest,
        archive: impl Read,
    ) -> Result<Digest> {
        validate_module(module)?;
        validate_version(version)?;
        self.store.import(archive, checksum)?;
        let manifest = self.store.manifest(checksum)?;
        self.record_archive(module, version, checksum, &manifest)
    }

    fn record_archive(
        &self,
        module: &str,
        version: &str,
        checksum: Digest,
        manifest: &str,
    ) -> Result<Digest> {
        let record = ReleaseRecord {
            version: version.to_owned(),
            checksum,
            manifest: manifest.to_owned(),
            yanked: false,
        };
        let lock = self.open_write_lock()?;
        lock.lock()?;
        let directory = self
            .module_directory(module, true)?
            .expect("created module directory");
        let filename = format!("{}.release", Digest::of_bytes(version.as_bytes()));
        let mut temporary = tempfile::Builder::new()
            .prefix(".pending-")
            .tempfile_in(&self.root)?;
        write!(
            temporary,
            "{RECORD_MAGIC}\n{version}\n{checksum}\n{}\n{manifest}",
            manifest.len()
        )?;
        temporary.as_file_mut().sync_all()?;
        let index_path = self.root.join("index").join(module);
        match temporary.persist_noclobber(index_path.join(&filename)) {
            Ok(_) => Ok(checksum),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = read_record(&directory, &index_path, OsStr::new(&filename))?;
                if existing == record {
                    Ok(checksum)
                } else {
                    Err(RegistryError::ImmutableRelease {
                        module: module.to_owned(),
                        version: version.to_owned(),
                    })
                }
            }
            Err(error) => Err(error.error.into()),
        }
    }

    /// Change only the withdrawal state of an already published release.
    ///
    /// The immutable metadata and source content are never rewritten. Each update
    /// atomically replaces one complete state file; concurrent setters use the
    /// last replacement, and readers observe either complete state. Reads across
    /// different versions do not form a registry-wide transactional snapshot.
    pub fn set_yanked(&self, module: &str, version: &str, yanked: bool) -> Result<ReleaseRecord> {
        validate_module(module)?;
        validate_version(version)?;
        let lock = self.open_write_lock()?;
        lock.lock()?;
        let directory = self
            .module_directory(module, false)?
            .ok_or_else(|| registry_error("cannot withdraw an unpublished release"))?;
        let key = Digest::of_bytes(version.as_bytes());
        let mut record = read_record(
            &directory,
            &self.root.join("index").join(module),
            OsStr::new(&format!("{key}.release")),
        )?;
        let state_directory = self
            .directory("withdrawals", module, true)?
            .expect("created withdrawal state directory");
        let mut temporary = tempfile::Builder::new()
            .prefix(".pending-")
            .tempfile_in(&self.root)?;
        write!(
            temporary,
            "zkcl-withdrawal\n{}\n{}\n{yanked}\n",
            record.version, record.checksum
        )?;
        temporary.as_file_mut().sync_all()?;
        temporary
            .persist(
                self.root
                    .join("withdrawals")
                    .join(module)
                    .join(format!("{key}.state")),
            )
            .map_err(|error| RegistryError::Io(error.error))?;
        state_directory.into_std_file().sync_all()?;
        record.yanked = yanked;
        Ok(record)
    }

    fn read_yanked(
        &self,
        directory: Option<&Dir>,
        module: &str,
        record: &ReleaseRecord,
    ) -> Result<bool> {
        let Some(directory) = directory else {
            return Ok(false);
        };
        let filename = format!("{}.state", Digest::of_bytes(record.version.as_bytes()));
        let file = match open_file(directory, &filename) {
            Ok(file) => file,
            Err(RegistryError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        at_metadata_file(
            self.root.join("withdrawals").join(module).join(filename),
            || {
                let bytes = read_limited(file, 512)?;
                let text = std::str::from_utf8(&bytes)?;
                let prefix = format!("zkcl-withdrawal\n{}\n{}\n", record.version, record.checksum);
                match text.strip_prefix(&prefix) {
                    Some("true\n") => Ok(true),
                    Some("false\n") => Ok(false),
                    _ => Err(registry_error("invalid withdrawal state or release identity").into()),
                }
            },
        )
    }

    fn module_directory(&self, module: &str, create: bool) -> Result<Option<Dir>> {
        self.directory("index", module, create)
    }

    fn directory(&self, tree: &str, module: &str, create: bool) -> Result<Option<Dir>> {
        validate_module(module)?;
        let mut directory = Dir::open_ambient_dir(&self.root, cap_std::ambient_authority())?;
        for component in std::iter::once(tree).chain(module.split('/')) {
            if create {
                match directory.create_dir(component) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            directory = match directory.open_dir_nofollow(component) {
                Ok(directory) => directory,
                Err(error) if !create && error.kind() == io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            };
        }
        Ok(Some(directory))
    }
}

pub(crate) fn validate_identity(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 1024 || id.trim() != id || id.chars().any(char::is_control) {
        return Err(registry_error(
            "registry identity must contain 1 to 1024 non-control UTF-8 bytes",
        ));
    }
    Ok(())
}

pub(crate) fn validate_module(module: &str) -> Result<()> {
    let parts = module.split('/').collect::<Vec<_>>();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 128
                || part.split('-').any(|segment| {
                    segment.is_empty()
                        || !segment
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                })
        })
    {
        return Err(registry_error("module must be a canonical namespace/name"));
    }
    Ok(())
}

pub(crate) fn validate_version(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 128
        || !version.as_bytes()[0].is_ascii_alphanumeric()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    {
        return Err(registry_error("invalid release version key"));
    }
    Ok(())
}

fn read_limited(mut file: File, limit: usize) -> Result<Vec<u8>> {
    if file.metadata()?.len() > limit as u64 {
        return Err(registry_error("registry metadata byte limit exceeded"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(registry_error("registry metadata byte limit exceeded"));
    }
    Ok(bytes)
}

fn at_metadata_file<T>(
    path: PathBuf,
    read: impl FnOnce() -> std::result::Result<T, Box<dyn StdError + Send + Sync>>,
) -> Result<T> {
    read().map_err(|source| match source.downcast::<RegistryError>() {
        Ok(error) => match *error {
            RegistryError::Io(source) => {
                RegistryError::path_io("cannot read registry metadata", &path, source)
            }
            _ => RegistryError::RegistryFile {
                path,
                source: error,
            },
        },
        Err(source) => RegistryError::RegistryFile { path, source },
    })
}

fn read_record(directory: &Dir, index_path: &Path, filename: &OsStr) -> Result<ReleaseRecord> {
    at_metadata_file(index_path.join(filename), || {
        let filename = filename
            .to_str()
            .ok_or_else(|| registry_error("non-UTF-8 release filename"))?;
        let key: Digest = filename
            .strip_suffix(".release")
            .ok_or_else(|| registry_error("unexpected module index entry"))?
            .parse()?;
        let record = decode_record(&read_limited(
            open_file(directory, filename)?,
            MAX_RECORD_BYTES,
        )?)?;
        if Digest::of_bytes(record.version.as_bytes()) != key {
            return Err(registry_error("release filename does not match its version").into());
        }
        Ok(record)
    })
}

fn decode_record(
    bytes: &[u8],
) -> std::result::Result<ReleaseRecord, Box<dyn StdError + Send + Sync>> {
    let text = std::str::from_utf8(bytes)?;
    let mut fields = text.splitn(5, '\n');
    if fields.next() != Some(RECORD_MAGIC) {
        return Err(registry_error("invalid release metadata marker").into());
    }
    let version = fields
        .next()
        .ok_or_else(|| registry_error("missing release version"))?;
    validate_version(version)?;
    let checksum = fields
        .next()
        .ok_or_else(|| registry_error("missing source checksum"))?
        .parse()?;
    let length_text = fields
        .next()
        .ok_or_else(|| registry_error("missing manifest length"))?;
    let length: usize = length_text.parse().map_err(InvalidManifestLength)?;
    let manifest = fields
        .next()
        .ok_or_else(|| registry_error("missing release manifest"))?;
    if length > MAX_MANIFEST_BYTES || length != manifest.len() || length.to_string() != length_text
    {
        return Err(registry_error("release manifest length mismatch").into());
    }
    Ok(ReleaseRecord {
        version: version.to_owned(),
        checksum,
        manifest: manifest.to_owned(),
        yanked: false,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("invalid manifest length: {0}")]
struct InvalidManifestLength(#[source] std::num::ParseIntError);

fn registry_error(message: &str) -> RegistryError {
    RegistryError::Registry(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::temporary;
    use std::sync::{Arc, Barrier};

    fn source(root: &Path, module: &str, version: &str) -> String {
        fs::create_dir_all(root).unwrap();
        let manifest = format!("; UTF-8 元数据\n(project \"{module}\" (version \"{version}\"))\n");
        fs::write(root.join("zkcl.sexp"), &manifest).unwrap();
        fs::write(root.join("main.lisp"), "(print :registry)\n").unwrap();
        manifest
    }

    #[test]
    fn identity_is_immutable_and_checked_when_opening() {
        let temporary = temporary();
        let root = temporary.path().join("registry");
        let registry = FileRegistry::init(&root, "dev-local").unwrap();
        assert_eq!(registry.id(), "dev-local");
        assert_eq!(FileRegistry::open(&root).unwrap().id(), "dev-local");
        FileRegistry::init(&root, "dev-local").unwrap();
        assert!(matches!(
            FileRegistry::init(&root, "other"),
            Err(RegistryError::RegistryIdentity { .. })
        ));
        assert!(FileRegistry::init(temporary.path().join("bad"), "line\nbreak").is_err());
        fs::write(root.join(IDENTITY_FILE), "invalid").unwrap();
        assert!(FileRegistry::open(root).is_err());
    }

    #[test]
    fn sparse_records_keep_namespaces_and_manifest_bytes() {
        let temporary = temporary();
        let registry =
            FileRegistry::init(temporary.path().join("registry"), "private/acme").unwrap();
        assert!(registry.records("acme/library").unwrap().is_empty());
        for module in ["acme/library", "alice/library"] {
            let root = temporary.path().join(module);
            let manifest = source(&root, module, "1.0.0");
            let checksum = registry
                .publish(
                    module,
                    "1.0.0",
                    &root,
                    &["zkcl.sexp", "main.lisp"],
                    &manifest,
                )
                .unwrap();
            assert_eq!(
                registry.records(module).unwrap(),
                [ReleaseRecord {
                    version: "1.0.0".to_owned(),
                    checksum,
                    manifest,
                    yanked: false,
                }]
            );
        }
        assert!(registry.records("other/library").unwrap().is_empty());
    }

    #[test]
    fn published_versions_are_immutable_and_retries_compare_contents() {
        let temporary = temporary();
        let registry = FileRegistry::init(temporary.path().join("registry"), "local").unwrap();
        let root = temporary.path().join("source");
        let manifest = source(&root, "acme/library", "1.0.0");
        let publish = || {
            registry.publish(
                "acme/library",
                "1.0.0",
                &root,
                &["zkcl.sexp", "main.lisp"],
                &manifest,
            )
        };
        let checksum = publish().unwrap();
        assert_eq!(publish().unwrap(), checksum);
        fs::write(root.join("main.lisp"), "different").unwrap();
        assert!(matches!(
            publish(),
            Err(RegistryError::ImmutableRelease { .. })
        ));
        assert_eq!(
            registry.records("acme/library").unwrap()[0].checksum,
            checksum
        );
        let destination = temporary.path().join("offline-source");
        fs::remove_dir_all(root).unwrap();
        FileRegistry::open(registry.root())
            .unwrap()
            .store()
            .materialize(checksum, &destination)
            .unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("main.lisp")).unwrap(),
            "(print :registry)\n"
        );
    }

    #[test]
    fn withdrawal_and_undo_preserve_immutable_release_and_locked_fetch() {
        let temporary = temporary();
        let registry = FileRegistry::init(temporary.path().join("registry"), "local").unwrap();
        let root = temporary.path().join("source");
        let manifest = source(&root, "acme/library", "1.0.0");
        let publish = || {
            registry.publish(
                "acme/library",
                "1.0.0",
                &root,
                &["zkcl.sexp", "main.lisp"],
                &manifest,
            )
        };
        let checksum = publish().unwrap();
        let record_path = registry
            .root()
            .join("index/acme/library")
            .join(format!("{}.release", Digest::of_bytes(b"1.0.0")));
        let immutable_bytes = fs::read(&record_path).unwrap();
        let original = registry.records("acme/library").unwrap().remove(0);
        assert!(!original.yanked);
        assert!(registry.set_yanked("acme/missing", "1.0.0", true).is_err());
        assert!(registry.set_yanked("acme/library", "2.0.0", false).is_err());
        assert!(
            registry
                .set_yanked("acme/library", "1.0.0", true)
                .unwrap()
                .yanked
        );
        assert_eq!(publish().unwrap(), checksum);
        let reopened = FileRegistry::open(registry.root()).unwrap();
        let withdrawn = reopened.records("acme/library").unwrap().remove(0);
        assert!(withdrawn.yanked);
        assert_eq!(withdrawn.checksum, original.checksum);
        assert_eq!(withdrawn.manifest, original.manifest);
        assert_eq!(fs::read(&record_path).unwrap(), immutable_bytes);
        let destination = temporary.path().join("locked-source");
        reopened
            .store()
            .materialize(checksum, &destination)
            .unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("zkcl.sexp")).unwrap(),
            manifest
        );
        assert_eq!(
            reopened.set_yanked("acme/library", "1.0.0", false).unwrap(),
            original
        );
        assert_eq!(
            FileRegistry::open(registry.root())
                .unwrap()
                .records("acme/library")
                .unwrap(),
            [original]
        );
        assert_eq!(fs::read(&record_path).unwrap(), immutable_bytes);
    }

    #[test]
    fn concurrent_withdrawals_expose_complete_states() {
        let temporary = temporary();
        let registry = FileRegistry::init(temporary.path().join("registry"), "local").unwrap();
        let root = temporary.path().join("source");
        let manifest = source(&root, "acme/library", "1.0.0");
        registry
            .publish("acme/library", "1.0.0", &root, &["zkcl.sexp"], &manifest)
            .unwrap();
        std::thread::scope(|scope| {
            for yanked in [true, false, true, false] {
                let registry = &registry;
                scope.spawn(move || {
                    for _ in 0..20 {
                        registry
                            .set_yanked("acme/library", "1.0.0", yanked)
                            .unwrap();
                        assert_eq!(registry.records("acme/library").unwrap().len(), 1);
                    }
                });
            }
        });
        registry.set_yanked("acme/library", "1.0.0", true).unwrap();
        assert!(registry.records("acme/library").unwrap()[0].yanked);
        let state = registry
            .root()
            .join("withdrawals/acme/library")
            .join(format!("{}.state", Digest::of_bytes(b"1.0.0")));
        fs::write(&state, "invalid").unwrap();
        assert!(
            matches!(registry.records("acme/library"), Err(RegistryError::RegistryFile { path, .. }) if path == state)
        );
        #[cfg(unix)]
        {
            fs::remove_file(&state).unwrap();
            std::os::unix::fs::symlink(root.join("zkcl.sexp"), state).unwrap();
            assert!(registry.records("acme/library").is_err());
        }
    }

    #[test]
    fn rejects_invalid_coordinates_and_inconsistent_manifest_metadata() {
        let temporary = temporary();
        let registry = FileRegistry::init(temporary.path().join("registry"), "local").unwrap();
        for module in [
            "one",
            "a/b/c",
            "../b",
            "a/..",
            "Upper/lower",
            "a-/b",
            "a/b\\c",
        ] {
            assert!(registry.records(module).is_err());
        }
        let root = temporary.path().join("source");
        let manifest = source(&root, "acme/library", "1.0.0");
        for version in ["", "../one", "/root", "one\ntwo", ".", "version:name"] {
            assert!(
                registry
                    .publish("acme/library", version, &root, &["zkcl.sexp"], &manifest)
                    .is_err()
            );
        }
        assert!(
            registry
                .publish("acme/library", "1.0.0", &root, &["zkcl.sexp"], "different")
                .is_err()
        );
        assert!(
            registry
                .publish("acme/library", "1.0.0", &root, &["main.lisp"], &manifest)
                .is_err()
        );
        assert!(registry.records("acme/library").unwrap().is_empty());
    }

    #[test]
    fn concurrent_publish_exposes_one_complete_release() {
        let temporary = temporary();
        let registry =
            Arc::new(FileRegistry::init(temporary.path().join("registry"), "local").unwrap());
        let root = temporary.path().join("source");
        let manifest = source(&root, "acme/library", "1.0.0");
        let barrier = Arc::new(Barrier::new(6));
        std::thread::scope(|scope| {
            let handles = (0..6)
                .map(|_| {
                    let registry = registry.clone();
                    let barrier = barrier.clone();
                    let root = &root;
                    let manifest = &manifest;
                    scope.spawn(move || {
                        barrier.wait();
                        registry
                            .publish(
                                "acme/library",
                                "1.0.0",
                                root,
                                &["zkcl.sexp", "main.lisp"],
                                manifest,
                            )
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            let checksums = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>();
            assert!(checksums.iter().all(|checksum| *checksum == checksums[0]));
        });
        assert_eq!(registry.records("acme/library").unwrap().len(), 1);
    }

    #[test]
    fn rejects_corrupt_records_and_symlinked_index_entries() {
        let temporary = temporary();
        let registry = FileRegistry::init(temporary.path().join("registry"), "local").unwrap();
        let root = temporary.path().join("source");
        let manifest = source(&root, "acme/library", "1.0.0");
        registry
            .publish("acme/library", "1.0.0", &root, &["zkcl.sexp"], &manifest)
            .unwrap();
        let record = registry
            .root()
            .join("index/acme/library")
            .join(format!("{}.release", Digest::of_bytes(b"1.0.0")));
        fs::write(&record, b"zkcl-release\n1.0.0\ninvalid\n0\n").unwrap();
        let error = registry.records("acme/library").unwrap_err();
        assert!(matches!(&error, RegistryError::RegistryFile { path, .. } if path == &record));
        assert!(matches!(
            error.source().unwrap().downcast_ref::<RegistryError>(),
            Some(RegistryError::InvalidDigest)
        ));
        fs::write(&record, [0xff]).unwrap();
        let error = registry.records("acme/library").unwrap_err();
        assert!(error.source().unwrap().is::<std::str::Utf8Error>());
        #[cfg(unix)]
        {
            fs::remove_file(&record).unwrap();
            std::os::unix::fs::symlink(root.join("zkcl.sexp"), &record).unwrap();
            assert!(registry.records("acme/library").is_err());
        }
    }
}
