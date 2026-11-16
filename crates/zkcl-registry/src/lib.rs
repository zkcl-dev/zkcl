//! Deterministic source archives and an offline, SHA-256-addressed content store.
//!
//! Archives contain sorted regular files at project-relative UTF-8 paths. They
//! use USTAR headers with zero owner IDs and timestamps, and modes 0644 or 0755.
//! Links, extension headers, duplicate paths, and platform-dependent paths are
//! rejected. Source selection belongs to the project manifest's caller.

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tempfile::NamedTempFile;

pub mod authority;
mod file_registry;
#[cfg(feature = "remote")]
pub mod remote;
#[cfg(feature = "signed")]
pub mod signed;
#[cfg(feature = "signed")]
pub mod sparse_export;
pub use file_registry::{FileRegistry, ReleaseRecord};

const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;

/// A SHA-256 digest, displayed as exactly 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest([u8; 32]);

impl Digest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Hash a stream without retaining its contents in memory.
    pub fn of_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut hash = Sha256::new();
        io::copy(&mut reader, &mut HashSink(&mut hash, io::sink()))?;
        Ok(Self(hash.finalize().into()))
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for Digest {
    type Err = RegistryError;

    fn from_str(text: &str) -> Result<Self> {
        if text.len() != 64 {
            return Err(RegistryError::InvalidDigest);
        }
        let mut bytes = [0; 32];
        let (chunks, _) = text.as_bytes().as_chunks::<2>();
        for (destination, pair) in bytes.iter_mut().zip(chunks) {
            let digit = |byte| match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err(RegistryError::InvalidDigest),
            };
            *destination = digit(pair[0])? * 16 + digit(pair[1])?;
        }
        Ok(Self(bytes))
    }
}

/// Bounds applied before file contents are copied or extraction directories grow.
#[derive(Clone, Copy, Debug)]
pub struct ArchiveLimits {
    pub max_archive_bytes: u64,
    pub max_file_bytes: u64,
    pub max_total_file_bytes: u64,
    pub max_files: usize,
    pub max_path_bytes: usize,
    pub max_path_depth: usize,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 512 * 1024 * 1024,
            max_file_bytes: 128 * 1024 * 1024,
            max_total_file_bytes: 256 * 1024 * 1024,
            max_files: 16_384,
            max_path_bytes: 255,
            max_path_depth: 32,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("{0}")]
    Io(#[source] io::Error),
    #[error("{action} {}: {source}", path.display())]
    PathIo {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("expected a 64-character lowercase SHA-256 digest")]
    InvalidDigest,
    #[error("invalid source archive: {0}")]
    InvalidArchive(String),
    #[error("invalid source archive: source manifest is not UTF-8")]
    ManifestEncoding(#[source] std::str::Utf8Error),
    #[error("source archive exceeds {0}")]
    Limit(ArchiveLimit),
    #[error("source digest mismatch: expected {expected}, found {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },
    #[error("source destination is not the expected immutable tree: {0}")]
    ExistingDestination(PathBuf),
    #[error("invalid filesystem registry: {0}")]
    Registry(String),
    #[error("invalid registry metadata at {path}: {source}")]
    RegistryFile {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("registry identity mismatch: expected {expected:?}, found {actual:?}")]
    RegistryIdentity { expected: String, actual: String },
    #[error("release {module}@{version} is already published with different contents")]
    ImmutableRelease { module: String, version: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ArchiveLimit {
    #[error("archive byte limit")]
    ArchiveBytes,
    #[error("manifest byte limit")]
    ManifestBytes,
    #[error("individual file byte limit")]
    FileBytes,
    #[error("total file byte limit")]
    TotalFileBytes,
    #[error("file count limit")]
    Files,
    #[error("path byte limit")]
    PathBytes,
    #[error("path depth limit")]
    PathDepth,
    #[error("source tree entry count limit")]
    TreeEntries,
}

impl From<io::Error> for RegistryError {
    fn from(error: io::Error) -> Self {
        if let Some(&limit) = error.get_ref().and_then(|cause| cause.downcast_ref()) {
            Self::Limit(limit)
        } else {
            Self::Io(error)
        }
    }
}

impl RegistryError {
    fn path_io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::PathIo {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, RegistryError>;

/// Write selected project files in canonical order. File paths must be relative
/// to `root`, and directories or symbolic links cannot be selected.
pub fn write_source_archive<P: AsRef<Path>>(
    root: impl AsRef<Path>,
    files: &[P],
    writer: impl Write,
) -> Result<Digest> {
    write_archive(root.as_ref(), files, writer, ArchiveLimits::default())
}

fn write_archive<P: AsRef<Path>>(
    root: &Path,
    files: &[P],
    writer: impl Write,
    limits: ArchiveLimits,
) -> Result<Digest> {
    write_archive_with_modes(root, files, writer, limits, |_, metadata| {
        Ok(normalized_mode(metadata))
    })
}

fn write_archive_with_modes<P: AsRef<Path>>(
    root: &Path,
    files: &[P],
    writer: impl Write,
    limits: ArchiveLimits,
    mode: impl Fn(&str, &fs::Metadata) -> Result<u32>,
) -> Result<Digest> {
    if files.len() > limits.max_files {
        return Err(RegistryError::Limit(ArchiveLimit::Files));
    }
    let mut paths = BTreeSet::new();
    let mut portable_paths = BTreeSet::new();
    let mut path_cases = BTreeMap::new();
    for file in files {
        let path = file
            .as_ref()
            .to_str()
            .ok_or_else(|| invalid("non-UTF-8 path"))?;
        validate_path(path, limits)?;
        check_path_case(path, &mut path_cases)?;
        if !paths.insert(path.to_owned()) || !portable_paths.insert(path.to_lowercase()) {
            return Err(invalid("duplicate or case-colliding source path"));
        }
    }
    for path in &paths {
        reject_file_parent(path, &portable_paths)?;
    }
    let root = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    let hashing = HashWriter {
        inner: writer,
        hash: Sha256::new(),
        count: 0,
        limit: limits.max_archive_bytes,
    };
    let mut archive = tar::Builder::new(hashing);
    let mut total = 0u64;
    for path in paths {
        let mut file = open_file(&root, &path)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        check_size(size, &mut total, limits)?;
        let header = canonical_header(&path, size, mode(&path, &metadata)?)?;
        archive.append(&header, (&mut file).take(size))?;
        if file.stream_position()? != size || file.read(&mut [0u8; 1])? != 0 {
            return Err(invalid(format!("source changed while archiving: {path}")));
        }
    }
    let mut hashing = archive.into_inner()?;
    hashing.flush()?;
    Ok(Digest(hashing.hash.finalize().into()))
}

/// Verify both the entire byte stream's digest and its source archive structure.
pub fn verify_archive(reader: impl Read, expected: Digest, limits: ArchiveLimits) -> Result<()> {
    read_archive(reader, expected, limits, None)
}

/// Local archive storage. Each digest names one immutable `.tar` object.
/// Existing objects are checked again on every access, including offline access.
#[derive(Clone, Debug)]
pub struct ContentStore {
    root: PathBuf,
    limits: ArchiveLimits,
}

impl ContentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_limits(root, ArchiveLimits::default())
    }

    pub fn with_limits(root: impl Into<PathBuf>, limits: ArchiveLimits) -> Self {
        Self {
            root: root.into(),
            limits,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn archive_path(&self, digest: Digest) -> PathBuf {
        self.root.join(format!("{digest}.tar"))
    }

    /// Prepare and atomically publish source into this local store.
    pub fn publish<P: AsRef<Path>>(&self, root: impl AsRef<Path>, files: &[P]) -> Result<Digest> {
        fs::create_dir_all(&self.root)?;
        let mut temporary = NamedTempFile::new_in(&self.root)?;
        let digest = write_archive(root.as_ref(), files, temporary.as_file_mut(), self.limits)?;
        temporary.as_file_mut().sync_all()?;
        self.commit(temporary, digest)?;
        Ok(digest)
    }

    /// Publish regular files using source-declared executable flags rather than
    /// the host filesystem's permission representation. All path, file-type and
    /// resource checks are the same as for [`Self::publish`].
    pub fn publish_with_modes<P: AsRef<Path>>(
        &self,
        root: impl AsRef<Path>,
        files: &[(P, bool)],
    ) -> Result<Digest> {
        let paths: Vec<_> = files.iter().map(|(path, _)| path.as_ref()).collect();
        let modes: BTreeMap<_, _> = files
            .iter()
            .map(|(path, executable)| (path.as_ref(), *executable))
            .collect();
        fs::create_dir_all(&self.root)?;
        let mut temporary = NamedTempFile::new_in(&self.root)?;
        let digest = write_archive_with_modes(
            root.as_ref(),
            &paths,
            temporary.as_file_mut(),
            self.limits,
            |path, _| Ok(if modes[Path::new(path)] { 0o755 } else { 0o644 }),
        )?;
        temporary.as_file_mut().sync_all()?;
        self.commit(temporary, digest)?;
        Ok(digest)
    }

    /// Stream, verify, and atomically install an archive. A concurrent install
    /// reuses the winner only after checking its actual contents.
    pub fn import(&self, reader: impl Read, expected: Digest) -> Result<PathBuf> {
        fs::create_dir_all(&self.root)?;
        let mut temporary = NamedTempFile::new_in(&self.root)?;
        let copied = io::copy(
            &mut reader.take(self.limits.max_archive_bytes.saturating_add(1)),
            temporary.as_file_mut(),
        )?;
        if copied > self.limits.max_archive_bytes {
            return Err(RegistryError::Limit(ArchiveLimit::ArchiveBytes));
        }
        temporary.as_file_mut().rewind()?;
        verify_archive(temporary.as_file_mut(), expected, self.limits)?;
        temporary.as_file_mut().sync_all()?;
        self.commit(temporary, expected)
    }

    fn commit(&self, temporary: NamedTempFile, digest: Digest) -> Result<PathBuf> {
        let destination = self.archive_path(digest);
        match temporary.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                self.archive(digest)?;
            }
            Err(error) => return Err(error.error.into()),
        }
        Ok(destination)
    }

    /// Return a verified archive positioned at its beginning.
    pub fn archive(&self, digest: Digest) -> Result<File> {
        let mut archive = self.open_archive(digest)?;
        verify_archive(&mut archive, digest, self.limits)?;
        archive.rewind()?;
        Ok(archive)
    }

    fn open_archive(&self, digest: Digest) -> Result<File> {
        let directory = Dir::open_ambient_dir(&self.root, cap_std::ambient_authority())
            .map_err(|source| RegistryError::path_io("open source store", &self.root, source))?;
        let name = format!("{digest}.tar");
        open_file(&directory, &name).map_err(|error| match error {
            RegistryError::Io(source) => {
                RegistryError::path_io("open source archive", &self.root.join(name), source)
            }
            error => error,
        })
    }

    /// Read root `zkcl.sexp` from a verified local archive, without unpacking it
    /// or consulting a registry index. Manifest text is limited to 4 MiB.
    pub fn manifest(&self, digest: Digest) -> Result<String> {
        Self::archive_manifest(self.archive(digest)?)
    }

    fn archive_manifest(verified_archive: File) -> Result<String> {
        let mut archive = tar::Archive::new(verified_archive);
        for entry in archive.entries()?.raw(true) {
            let entry = entry?;
            if &*entry.path_bytes() == b"zkcl.sexp" {
                let size = entry.size();
                if size > MAX_MANIFEST_BYTES as u64 {
                    return Err(RegistryError::Limit(ArchiveLimit::ManifestBytes));
                }
                let mut bytes = Vec::new();
                let count = entry
                    .take(MAX_MANIFEST_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                if count as u64 != size {
                    return Err(invalid("truncated source manifest"));
                }
                return String::from_utf8(bytes)
                    .map_err(|error| RegistryError::ManifestEncoding(error.utf8_error()));
            }
        }
        Err(invalid("source archive must contain root zkcl.sexp"))
    }

    /// Extract into a sibling temporary directory and install the whole tree.
    /// Repeated calls verify an existing destination against the archive digest.
    pub fn materialize(&self, digest: Digest, destination: impl AsRef<Path>) -> Result<PathBuf> {
        let destination = destination.as_ref();
        let archive = self.open_archive(digest)?;
        let _lock = self.lock_materialization_if_needed(digest)?;
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                verify_archive(archive, digest, self.limits)?;
                return self.verify_destination(digest, destination);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(RegistryError::path_io(
                    "inspect source destination",
                    destination,
                    source,
                ));
            }
        }
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let temporary = tempfile::Builder::new()
            .prefix(".zkcl-source-")
            .tempdir_in(parent)?;
        read_archive(archive, digest, self.limits, Some(temporary.path()))?;
        match platform::install_directory(temporary.path(), destination) {
            Ok(()) => {
                let _ = temporary.keep();
                Ok(destination.to_owned())
            }
            Err(_) if fs::symlink_metadata(destination).is_ok() => {
                self.verify_destination(digest, destination)
            }
            Err(source) => Err(RegistryError::path_io(
                "install source tree",
                destination,
                source,
            )),
        }
    }

    fn lock_materialization_if_needed(&self, digest: Digest) -> Result<Option<File>> {
        if platform::HAS_ATOMIC_NOREPLACE_RENAME {
            return Ok(None);
        }
        let directory = Dir::open_ambient_dir(&self.root, cap_std::ambient_authority())?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No)
            .nonblock(true);
        let name = format!("{digest}.lock");
        let lock_error =
            |source| RegistryError::path_io("lock source archive", &self.root.join(&name), source);
        let lock = directory
            .open_with(&name, &options)
            .map_err(lock_error)?
            .into_std();
        if !lock.metadata().map_err(lock_error)?.is_file() {
            return Err(invalid("content lock is not a regular file"));
        }
        lock.lock().map_err(lock_error)?;
        Ok(Some(lock))
    }

    fn verify_destination(&self, expected: Digest, destination: &Path) -> Result<PathBuf> {
        let metadata = fs::symlink_metadata(destination).map_err(|source| {
            RegistryError::path_io("inspect source destination", destination, source)
        })?;
        if !metadata.is_dir() || metadata.is_symlink() {
            return Err(RegistryError::ExistingDestination(destination.to_owned()));
        }
        let root = Dir::open_ambient_dir(destination, cap_std::ambient_authority())?;
        let mut files = Vec::new();
        if collect_files(&root, "", &mut files, &mut 0, self.limits)? {
            return Err(RegistryError::ExistingDestination(destination.to_owned()));
        }
        let modes = self.archive_modes(expected)?;
        let actual = write_archive_with_modes(
            destination,
            &files,
            io::sink(),
            self.limits,
            |path, metadata| {
                let mode = *modes
                    .get(path)
                    .ok_or_else(|| RegistryError::ExistingDestination(destination.to_owned()))?;
                if !platform::mode_matches(metadata, mode) {
                    return Err(RegistryError::ExistingDestination(destination.to_owned()));
                }
                Ok(mode)
            },
        )?;
        if actual != expected {
            return Err(RegistryError::ExistingDestination(destination.to_owned()));
        }
        Ok(destination.to_owned())
    }

    fn archive_modes(&self, digest: Digest) -> Result<BTreeMap<String, u32>> {
        let mut archive = tar::Archive::new(self.archive(digest)?);
        archive
            .entries()?
            .raw(true)
            .map(|entry| {
                let entry = entry?;
                let path = String::from_utf8(entry.path_bytes().into_owned())
                    .map_err(|_| invalid("non-UTF-8 source path"))?;
                Ok((path, entry.header().mode()?))
            })
            .collect()
    }
}

mod platform {
    use std::{fs, io, path::Path};

    pub(super) const HAS_ATOMIC_NOREPLACE_RENAME: bool = cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        windows
    ));

    pub(super) fn install_directory(staging: &Path, destination: &Path) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        {
            use rustix::fs::{CWD, RenameFlags, renameat_with};

            // A competing writer may have created an empty directory since the check.
            renameat_with(CWD, staging, CWD, destination, RenameFlags::NOREPLACE)
                .map_err(io::Error::from)
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
        {
            fs::rename(staging, destination)
        }
    }

    pub(super) fn normalized_mode(metadata: &fs::Metadata) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 != 0 {
                0o755
            } else {
                0o644
            }
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            0o644
        }
    }

    pub(super) fn mode_matches(metadata: &fs::Metadata, expected_mode: u32) -> bool {
        #[cfg(unix)]
        {
            normalized_mode(metadata) == expected_mode
        }
        #[cfg(not(unix))]
        {
            let _ = (metadata, expected_mode);
            true
        }
    }

    pub(super) fn apply_file_mode(file: &fs::File, mode: u32) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))
        }
        #[cfg(not(unix))]
        {
            let _ = (file, mode);
            Ok(())
        }
    }
}

fn normalized_mode(metadata: &fs::Metadata) -> u32 {
    platform::normalized_mode(metadata)
}

fn collect_files(
    root: &Dir,
    prefix: &str,
    files: &mut Vec<String>,
    visited: &mut usize,
    limits: ArchiveLimits,
) -> Result<bool> {
    let mut has_empty_directory = false;
    for entry in root.entries()? {
        *visited += 1;
        if *visited
            > limits
                .max_files
                .saturating_mul(limits.max_path_depth.saturating_add(1))
        {
            return Err(RegistryError::Limit(ArchiveLimit::TreeEntries));
        }
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("non-UTF-8 cached path"))?;
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        validate_path(&path, limits)?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            let first_file = files.len();
            has_empty_directory |= collect_files(
                &root.open_dir_nofollow(name)?,
                &path,
                files,
                visited,
                limits,
            )? || files.len() == first_file;
        } else if kind.is_file() {
            files.push(path);
            if files.len() > limits.max_files {
                return Err(RegistryError::Limit(ArchiveLimit::Files));
            }
        } else {
            return Err(invalid("cached source contains a link or special file"));
        }
    }
    Ok(has_empty_directory)
}

fn open_file(root: &Dir, path: &str) -> Result<File> {
    let mut components = path.split('/').peekable();
    let mut directory = root.try_clone()?;
    while let Some(component) = components.next() {
        if components.peek().is_some() {
            directory = directory.open_dir_nofollow(component)?;
        } else {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No).nonblock(true);
            let file = directory.open_with(component, &options)?.into_std();
            if !file.metadata()?.is_file() {
                return Err(invalid("source is not a regular file"));
            }
            return Ok(file);
        }
    }
    Err(invalid("empty source path"))
}

fn validate_path(path: &str, limits: ArchiveLimits) -> Result<()> {
    if path.len() > limits.max_path_bytes {
        return Err(RegistryError::Limit(ArchiveLimit::PathBytes));
    }
    if path.split('/').count() > limits.max_path_depth {
        return Err(RegistryError::Limit(ArchiveLimit::PathDepth));
    }
    for component in path.split('/') {
        let stem = component
            .split('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let reserved = matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
            || (stem.len() == 4
                && (stem.starts_with("com") || stem.starts_with("lpt"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'));
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.ends_with(['.', ' '])
            || reserved
            || component.chars().any(|character| {
                character.is_control()
                    || matches!(character, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
            })
        {
            return Err(invalid(format!("invalid relative source path: {path:?}")));
        }
    }
    Ok(())
}

fn canonical_header(path: &str, size: u64, mode: u32) -> Result<tar::Header> {
    let mut header = tar::Header::new_ustar();
    header.set_path(path)?;
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(size);
    header.set_mode(mode);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    Ok(header)
}

fn check_size(size: u64, total: &mut u64, limits: ArchiveLimits) -> Result<()> {
    if size > limits.max_file_bytes {
        return Err(RegistryError::Limit(ArchiveLimit::FileBytes));
    }
    *total = total
        .checked_add(size)
        .ok_or(RegistryError::Limit(ArchiveLimit::TotalFileBytes))?;
    if *total > limits.max_total_file_bytes {
        return Err(RegistryError::Limit(ArchiveLimit::TotalFileBytes));
    }
    Ok(())
}

fn read_archive(
    reader: impl Read,
    expected: Digest,
    limits: ArchiveLimits,
    destination: Option<&Path>,
) -> Result<()> {
    let hashing = HashReader {
        inner: reader,
        hash: Sha256::new(),
        count: 0,
        limit: limits.max_archive_bytes,
    };
    let mut archive = tar::Archive::new(hashing);
    let mut previous = String::new();
    let mut portable_paths = BTreeSet::new();
    let mut path_cases = BTreeMap::new();
    let mut total = 0u64;
    let mut canonical_hash = Sha256::new();
    for (index, entry) in archive.entries()?.raw(true).enumerate() {
        if index >= limits.max_files {
            return Err(RegistryError::Limit(ArchiveLimit::Files));
        }
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() || entry.header().as_ustar().is_none() {
            return Err(invalid("only USTAR regular file entries are allowed"));
        }
        let path = std::str::from_utf8(&entry.path_bytes())
            .map_err(|_| invalid("non-UTF-8 archive path"))?
            .to_owned();
        validate_path(&path, limits)?;
        check_path_case(&path, &mut path_cases)?;
        if path <= previous || !portable_paths.insert(path.to_lowercase()) {
            return Err(invalid("archive paths must be unique and sorted"));
        }
        reject_file_parent(&path, &portable_paths)?;
        previous = path.clone();
        let size = entry.size();
        check_size(size, &mut total, limits)?;
        let mode = entry.header().mode()?;
        if !matches!(mode, 0o644 | 0o755)
            || canonical_header(&path, size, mode)?.as_bytes() != entry.header().as_bytes()
        {
            return Err(invalid("noncanonical source archive header"));
        }
        canonical_hash.update(entry.header().as_bytes());
        let copied = if let Some(destination) = destination {
            let path = destination.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            let copied = io::copy(&mut entry, &mut HashSink(&mut canonical_hash, &mut file))?;
            platform::apply_file_mode(&file, mode)?;
            copied
        } else {
            io::copy(&mut entry, &mut HashSink(&mut canonical_hash, io::sink()))?
        };
        if copied != size {
            return Err(invalid("truncated source file"));
        }
        let padding = ((512 - size % 512) % 512) as usize;
        canonical_hash.update(&[0u8; 512][..padding]);
    }
    let mut hashing = archive.into_inner();
    let mut trailer = [0u8; 512];
    hashing.read_exact(&mut trailer)?;
    if trailer != [0; 512] || hashing.read(&mut [0u8; 1])? != 0 {
        return Err(invalid("source archive must end with two zero blocks"));
    }
    let actual = Digest(hashing.hash.finalize().into());
    if actual != expected {
        return Err(RegistryError::DigestMismatch { expected, actual });
    }
    canonical_hash.update([0u8; 1024]);
    if Digest(canonical_hash.finalize().into()) != actual {
        return Err(invalid("noncanonical source archive padding"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> RegistryError {
    RegistryError::InvalidArchive(message.into())
}

fn reject_file_parent(path: &str, files: &BTreeSet<String>) -> Result<()> {
    for (index, _) in path.match_indices('/') {
        if files.contains(&path[..index].to_lowercase()) {
            return Err(invalid("archive path has a regular file as its parent"));
        }
    }
    Ok(())
}

fn check_path_case(path: &str, paths: &mut BTreeMap<String, String>) -> Result<()> {
    for end in path
        .match_indices('/')
        .map(|(index, _)| index)
        .chain(std::iter::once(path.len()))
    {
        let prefix = &path[..end];
        if let Some(previous) = paths.insert(prefix.to_lowercase(), prefix.to_owned())
            && previous != prefix
        {
            return Err(invalid(
                "source paths contain case-colliding parent directories",
            ));
        }
    }
    Ok(())
}

struct HashSink<'a, W>(&'a mut Sha256, W);

impl<W: Write> Write for HashSink<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.1.write(bytes)?;
        self.0.update(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.1.flush()
    }
}

struct HashWriter<W> {
    inner: W,
    hash: Sha256,
    count: u64,
    limit: u64,
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.limit.saturating_sub(self.count) {
            return Err(io::Error::other(ArchiveLimit::ArchiveBytes));
        }
        let count = self.inner.write(bytes)?;
        self.hash.update(&bytes[..count]);
        self.count += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct HashReader<R> {
    inner: R,
    hash: Sha256,
    count: u64,
    limit: u64,
}

impl<R: Read> Read for HashReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let permitted = (bytes.len() as u64)
            .min(self.limit.saturating_sub(self.count).saturating_add(1))
            as usize;
        let count = self.inner.read(&mut bytes[..permitted])?;
        self.count += count as u64;
        if self.count > self.limit {
            return Err(io::Error::other(ArchiveLimit::ArchiveBytes));
        }
        self.hash.update(&bytes[..count]);
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Barrier};

    pub(crate) fn temporary() -> tempfile::TempDir {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let root = workspace.join(".agents/tmp/zkcl/project-archive");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    fn project(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("zkcl.sexp"),
            "(project \"acme/sample\" (version \"1.0.0\"))\n",
        )
        .unwrap();
        fs::write(root.join("src/main.lisp"), "(print :hello)\n").unwrap();
    }

    fn source_bytes(root: &Path) -> (Vec<u8>, Digest) {
        let mut bytes = Vec::new();
        let digest =
            write_source_archive(root, &["zkcl.sexp", "src/main.lisp"], &mut bytes).unwrap();
        (bytes, digest)
    }

    fn one_entry(header: tar::Header, contents: &[u8]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, contents).unwrap();
        builder.into_inner().unwrap()
    }

    fn verify(bytes: &[u8]) -> Result<()> {
        verify_archive(bytes, Digest::of_bytes(bytes), ArchiveLimits::default())
    }

    #[test]
    fn digest_has_stable_sha256_representation() {
        let text = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let digest = Digest::of_bytes(b"abc");
        assert_eq!(digest.to_string(), text);
        assert_eq!(text.parse::<Digest>().unwrap(), digest);
        assert_eq!(Digest::of_reader(Cursor::new(b"abc")).unwrap(), digest);
        for invalid in [
            "",
            "sha256:123",
            &text[..63],
            &text.to_uppercase(),
            &"z".repeat(64),
        ] {
            assert!(invalid.parse::<Digest>().is_err());
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn install_preserves_an_empty_directory_created_after_the_existence_check() {
        let temp = temporary();
        let staging = temp.path().join("staging");
        let destination = temp.path().join("destination");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("source.lisp"), b"42").unwrap();
        fs::create_dir(&destination).unwrap();

        let error = platform::install_directory(&staging, &destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(staging.join("source.lisp")).unwrap(), b"42");
        assert!(fs::read_dir(destination).unwrap().next().is_none());
    }

    #[test]
    fn cached_manifest_errors_distinguish_size_from_encoding() {
        let temp = temporary();
        let manifest = temp.path().join("zkcl.sexp");
        let store = ContentStore::new(temp.path().join("store"));
        fs::write(&manifest, [b'(', 0xff]).unwrap();
        let digest = store.publish(temp.path(), &["zkcl.sexp"]).unwrap();
        let error = store.manifest(digest).unwrap_err();
        let source = std::error::Error::source(&error)
            .and_then(|cause| cause.downcast_ref::<std::str::Utf8Error>())
            .expect("manifest decoder error");
        assert_eq!(source.valid_up_to(), 1);

        fs::write(manifest, vec![b' '; MAX_MANIFEST_BYTES + 1]).unwrap();
        let digest = store.publish(temp.path(), &["zkcl.sexp"]).unwrap();
        assert!(matches!(
            store.manifest(digest),
            Err(RegistryError::Limit(ArchiveLimit::ManifestBytes))
        ));
    }

    #[test]
    fn archive_is_deterministic_and_contains_project_relative_files() {
        let temporary = temporary();
        project(temporary.path());
        let (first, digest) = source_bytes(temporary.path());
        let file = File::open(temporary.path().join("zkcl.sexp")).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let mut second = Vec::new();
        let second_digest = write_source_archive(
            temporary.path(),
            &["src/main.lisp", "zkcl.sexp"],
            &mut second,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(digest, second_digest);
        assert_eq!(digest, Digest::of_bytes(&first));
        verify(&first).unwrap();
        let mut archive = tar::Archive::new(first.as_slice());
        let paths = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [PathBuf::from("src/main.lisp"), PathBuf::from("zkcl.sexp")]
        );
    }

    #[test]
    fn explicit_executable_flags_define_portable_headers_and_survive_materialization() {
        let temporary = temporary();
        project(temporary.path());
        let store = ContentStore::new(temporary.path().join("store"));
        let files = [("zkcl.sexp", false), ("src/main.lisp", true)];
        let digest = store.publish_with_modes(temporary.path(), &files).unwrap();
        let mut expected = tar::Builder::new(Vec::new());
        for (path, mode) in [("src/main.lisp", 0o755), ("zkcl.sexp", 0o644)] {
            let bytes = fs::read(temporary.path().join(path)).unwrap();
            expected
                .append(
                    &canonical_header(path, bytes.len() as u64, mode).unwrap(),
                    bytes.as_slice(),
                )
                .unwrap();
        }
        let expected = expected.into_inner().unwrap();
        assert_eq!(digest, Digest::of_bytes(&expected));
        assert_eq!(fs::read(store.archive_path(digest)).unwrap(), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Input permissions deliberately disagree with both explicit flags.
            fs::set_permissions(
                temporary.path().join("zkcl.sexp"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            fs::set_permissions(
                temporary.path().join("src/main.lisp"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        assert_eq!(
            store.publish_with_modes(temporary.path(), &files).unwrap(),
            digest
        );
        let destination = temporary.path().join("installed");
        store.materialize(digest, &destination).unwrap();
        store.materialize(digest, &destination).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                destination.join("src/main.lisp"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            assert!(matches!(
                store.materialize(digest, &destination),
                Err(RegistryError::ExistingDestination(_))
            ));
        }
    }

    #[test]
    fn explicit_modes_retain_source_boundary_and_resource_checks() {
        let temporary = temporary();
        project(temporary.path());
        let store = ContentStore::new(temporary.path().join("store"));
        for files in [
            vec![("../outside", false)],
            vec![("src", false)],
            vec![("zkcl.sexp", false), ("zkcl.sexp", true)],
        ] {
            assert!(store.publish_with_modes(temporary.path(), &files).is_err());
        }
        let store = ContentStore::with_limits(
            temporary.path().join("limited"),
            ArchiveLimits {
                max_files: 0,
                ..ArchiveLimits::default()
            },
        );
        assert!(matches!(
            store.publish_with_modes(temporary.path(), &[("zkcl.sexp", false)]),
            Err(RegistryError::Limit(ArchiveLimit::Files))
        ));
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_source_selection() {
        let temporary = temporary();
        project(temporary.path());
        for path in [
            "../outside",
            "/absolute",
            "./zkcl.sexp",
            "src//main.lisp",
            "src/../zkcl.sexp",
            "C:/source",
            "src\\main.lisp",
            "con",
            "dir/file.",
            "dir/file ",
            "src",
        ] {
            assert!(
                write_source_archive(temporary.path(), &[path], io::sink()).is_err(),
                "accepted {path}"
            );
        }
        assert!(
            write_source_archive(temporary.path(), &["zkcl.sexp", "zkcl.sexp"], io::sink())
                .is_err()
        );
        assert!(
            write_source_archive(temporary.path(), &["zkcl.sexp", "ZKCL.sexp"], io::sink())
                .is_err()
        );
    }

    #[test]
    fn rejects_traversal_absolute_and_platform_paths_in_received_archives() {
        for path in [
            "../outside",
            "/outside",
            "C:/outside",
            "a\\outside",
            "a/../outside",
        ] {
            let mut header = canonical_header("safe", 0, 0o644).unwrap();
            header.as_mut_bytes()[..100].fill(0);
            header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
            header.set_cksum();
            assert!(verify(&one_entry(header, b"")).is_err(), "accepted {path}");
        }
    }

    #[test]
    fn rejects_links_special_entries_and_extension_headers() {
        for kind in [
            tar::EntryType::Symlink,
            tar::EntryType::Link,
            tar::EntryType::Directory,
            tar::EntryType::Fifo,
            tar::EntryType::Char,
            tar::EntryType::Block,
            tar::EntryType::GNULongName,
            tar::EntryType::XHeader,
        ] {
            let mut header = canonical_header("entry", 0, 0o644).unwrap();
            header.set_entry_type(kind);
            header.set_cksum();
            assert!(
                verify(&one_entry(header, b"")).is_err(),
                "accepted {kind:?}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_unsorted_and_conflicting_archive_paths() {
        for paths in [["same", "same"], ["z", "a"], ["A", "a"], ["A/x", "a/y"]] {
            let mut builder = tar::Builder::new(Vec::new());
            for path in paths {
                builder
                    .append(&canonical_header(path, 0, 0o644).unwrap(), io::empty())
                    .unwrap();
            }
            assert!(verify(&builder.into_inner().unwrap()).is_err());
        }
        let temporary = temporary();
        let mut builder = tar::Builder::new(Vec::new());
        for path in ["a", "a/b"] {
            builder
                .append(&canonical_header(path, 0, 0o644).unwrap(), io::empty())
                .unwrap();
        }
        let bytes = builder.into_inner().unwrap();
        let digest = Digest::of_bytes(&bytes);
        let store = ContentStore::new(temporary.path().join("store"));
        // A path cannot simultaneously name a file and a parent directory.
        assert!(store.import(bytes.as_slice(), digest).is_err());
    }

    #[test]
    fn rejects_noncanonical_headers_truncation_trailers_and_digest_changes() {
        let mut header = canonical_header("source", 1, 0o644).unwrap();
        header.set_mtime(1);
        header.set_cksum();
        assert!(verify(&one_entry(header, b"x")).is_err());
        let bytes = one_entry(canonical_header("source", 1, 0o644).unwrap(), b"x");
        verify(&bytes).unwrap();
        for end in [0, 100, 512, 513, bytes.len() - 1, bytes.len() - 512] {
            assert!(
                verify(&bytes[..end]).is_err(),
                "accepted truncation at {end}"
            );
        }
        let mut appended = bytes.clone();
        appended.extend_from_slice(b"trailing bytes");
        assert!(verify(&appended).is_err());
        let mut nonzero_padding = bytes.clone();
        nonzero_padding[513] = 1;
        assert!(verify(&nonzero_padding).is_err());
        let mut changed = bytes.clone();
        changed[512] = b'y';
        assert!(matches!(
            verify_archive(
                changed.as_slice(),
                Digest::of_bytes(&bytes),
                ArchiveLimits::default()
            ),
            Err(RegistryError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn bounds_received_files_paths_and_archive_bytes() {
        let bytes = one_entry(canonical_header("nested/file", 4, 0o644).unwrap(), b"data");
        let defaults = ArchiveLimits::default();
        let limits = [
            ArchiveLimits {
                max_files: 0,
                ..defaults
            },
            ArchiveLimits {
                max_file_bytes: 3,
                ..defaults
            },
            ArchiveLimits {
                max_total_file_bytes: 3,
                ..defaults
            },
            ArchiveLimits {
                max_path_bytes: 5,
                ..defaults
            },
            ArchiveLimits {
                max_path_depth: 1,
                ..defaults
            },
        ];
        for limits in limits {
            assert!(verify_archive(bytes.as_slice(), Digest::of_bytes(&bytes), limits).is_err());
        }
        let limits = ArchiveLimits {
            max_archive_bytes: 1024,
            ..defaults
        };
        assert!(matches!(
            verify_archive(bytes.as_slice(), Digest::of_bytes(&bytes), limits),
            Err(RegistryError::Limit(ArchiveLimit::ArchiveBytes))
        ));
        let temporary = temporary();
        project(temporary.path());
        let store = ContentStore::with_limits(temporary.path().join("store"), limits);
        assert!(matches!(
            store.import(io::repeat(0), Digest::of_bytes(b"")),
            Err(RegistryError::Limit(ArchiveLimit::ArchiveBytes))
        ));
        assert!(matches!(
            store.publish(temporary.path(), &["zkcl.sexp"]),
            Err(RegistryError::Limit(ArchiveLimit::ArchiveBytes))
        ));
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
    }

    #[test]
    fn publishes_imports_and_materializes_offline() {
        let temporary = temporary();
        let source = temporary.path().join("project");
        project(&source);
        let publisher = ContentStore::new(temporary.path().join("publisher"));
        let digest = publisher
            .publish(&source, &["zkcl.sexp", "src/main.lisp"])
            .unwrap();
        let consumer = ContentStore::new(temporary.path().join("consumer"));
        consumer
            .import(publisher.archive(digest).unwrap(), digest)
            .unwrap();
        fs::remove_dir_all(&source).unwrap();
        fs::remove_dir_all(publisher.root()).unwrap();
        assert_eq!(
            consumer.manifest(digest).unwrap(),
            "(project \"acme/sample\" (version \"1.0.0\"))\n"
        );
        let destination = temporary.path().join("installed");
        assert_eq!(
            consumer.materialize(digest, &destination).unwrap(),
            destination
        );
        assert_eq!(
            fs::read_to_string(destination.join("src/main.lisp")).unwrap(),
            "(print :hello)\n"
        );
        consumer.materialize(digest, &destination).unwrap();
        fs::create_dir(destination.join("src/extra")).unwrap();
        assert!(matches!(
            consumer.materialize(digest, &destination),
            Err(RegistryError::ExistingDestination(_))
        ));
        fs::remove_dir(destination.join("src/extra")).unwrap();
        fs::write(destination.join("extra"), "modified").unwrap();
        assert!(matches!(
            consumer.materialize(digest, &destination),
            Err(RegistryError::ExistingDestination(_))
        ));
    }

    #[test]
    fn corrupted_cache_is_rejected_without_overwriting_it() {
        let temporary = temporary();
        project(temporary.path());
        let (bytes, digest) = source_bytes(temporary.path());
        let store = ContentStore::new(temporary.path().join("store"));
        store.import(bytes.as_slice(), digest).unwrap();
        fs::write(store.archive_path(digest), "corruption").unwrap();
        assert!(store.archive(digest).is_err());
        assert!(store.import(bytes.as_slice(), digest).is_err());
        assert_eq!(fs::read(store.archive_path(digest)).unwrap(), b"corruption");
        assert!(!temporary.path().join("installed").exists());
        assert!(
            store
                .materialize(digest, temporary.path().join("installed"))
                .is_err()
        );
    }

    #[test]
    fn failed_import_or_extract_never_exposes_a_partial_destination() {
        let temporary = temporary();
        project(temporary.path());
        let (bytes, digest) = source_bytes(temporary.path());
        let store = ContentStore::new(temporary.path().join("store"));
        assert!(store.import(&bytes[..bytes.len() - 1], digest).is_err());
        assert!(!store.archive_path(digest).exists());
        assert_eq!(fs::read_dir(store.root()).unwrap().count(), 0);
        store.import(bytes.as_slice(), digest).unwrap();
        let destination = temporary.path().join("existing");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("user-file"), "preserve").unwrap();
        assert!(store.materialize(digest, &destination).is_err());
        assert_eq!(
            fs::read(destination.join("user-file")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn concurrent_import_and_install_reuse_only_complete_verified_contents() {
        let temporary = temporary();
        project(temporary.path());
        let (bytes, digest) = source_bytes(temporary.path());
        let bytes = Arc::new(bytes);
        let store = Arc::new(ContentStore::new(temporary.path().join("store")));
        let destination = temporary.path().join("installed");
        let barrier = Arc::new(Barrier::new(6));
        std::thread::scope(|scope| {
            let handles = (0..6)
                .map(|_| {
                    let store = store.clone();
                    let bytes = bytes.clone();
                    let barrier = barrier.clone();
                    let destination = &destination;
                    scope.spawn(move || {
                        barrier.wait();
                        store.import(bytes.as_slice(), digest).unwrap();
                        store.materialize(digest, destination).unwrap();
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        store.archive(digest).unwrap();
        store.materialize(digest, &destination).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_source_cache_and_destination_symlinks_and_preserves_executable_mode() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temporary = temporary();
        project(temporary.path());
        symlink("src/main.lisp", temporary.path().join("linked")).unwrap();
        symlink("src", temporary.path().join("linked-dir")).unwrap();
        for path in ["linked", "linked-dir/main.lisp"] {
            assert!(write_source_archive(temporary.path(), &[path], io::sink()).is_err());
        }
        fs::set_permissions(
            temporary.path().join("src/main.lisp"),
            fs::Permissions::from_mode(0o711),
        )
        .unwrap();
        let (bytes, digest) = source_bytes(temporary.path());
        let store = ContentStore::new(temporary.path().join("store"));
        store.import(bytes.as_slice(), digest).unwrap();
        let destination = temporary.path().join("installed");
        store.materialize(digest, &destination).unwrap();
        assert_eq!(
            fs::metadata(destination.join("src/main.lisp"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        let linked = temporary.path().join("linked-install");
        symlink(&destination, &linked).unwrap();
        assert!(store.materialize(digest, linked).is_err());
        let real = temporary.path().join("real.tar");
        fs::rename(store.archive_path(digest), &real).unwrap();
        symlink(real, store.archive_path(digest)).unwrap();
        assert!(store.archive(digest).is_err());
        assert!(store.import(bytes.as_slice(), digest).is_err());
    }
}
