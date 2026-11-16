use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use flate2::bufread::GzDecoder;
use reqwest::blocking::Client;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::selector::{Channel, ReleaseId, Selector};

pub(crate) const DEFAULT_SOURCE: &str = "https://github.com/zkcl-dev/zkcl/releases";

const POINTER_NAME: &str = "zkcl-channel-v1.txt";
const MANIFEST_NAME: &str = "zkcl-toolchain-v1.json";
const MAX_POINTER_BYTES: u64 = 256;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_TAR_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_TAR_PATH_BYTES: u64 = 256;
const EXECUTABLE: &str = "bin/zkcl";
const RUNTIME_FILES: &[&str] = &[EXECUTABLE, "LICENSE", "LICENSE-ASDF"];
const LAUNCHER_FILES: &[&str] = &[EXECUTABLE, "LICENSE"];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub(crate) id: ReleaseId,
    pub(crate) version: Version,
    pub(crate) commit: String,
    pub(crate) runtime: Archive,
    pub(crate) launcher: Archive,
    #[serde(default)]
    pub(crate) sdk: BTreeMap<String, Archive>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Archive {
    pub(crate) sha256: String,
    pub(crate) size: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    toolchain: ReleaseId,
    version: Version,
    commit: String,
    targets: BTreeMap<String, Target>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    runtime: Archive,
    launcher: Archive,
    #[serde(default)]
    sdk: BTreeMap<String, Archive>,
}

enum Source {
    Directory(String),
    Https { root: Url, client: Client },
}

pub(crate) struct Distribution {
    source: Source,
}

impl Distribution {
    pub(crate) fn new(source: &str) -> Result<Self> {
        let source = if let Some((_, location)) = source.split_once("://") {
            let authority = location
                .split(['/', '\\', '?', '#'])
                .next()
                .unwrap_or_default();
            let root =
                Url::parse(source.trim_end_matches('/')).context("invalid toolchain source URL")?;
            ensure!(
                root.scheme() == "https"
                    && root.host().is_some()
                    && !authority.contains('@')
                    && root.username().is_empty()
                    && root.password().is_none()
                    && root.query().is_none()
                    && root.fragment().is_none(),
                "toolchain source URL must use HTTPS and omit credentials, query, and fragment"
            );
            let client = Client::builder()
                .https_only(true)
                .redirect(reqwest::redirect::Policy::limited(5))
                .connect_timeout(Duration::from_secs(10))
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .build()
                .context("initialize toolchain download client")?;
            Source::Https { root, client }
        } else {
            let directory = fs::canonicalize(source)
                .with_context(|| format!("open toolchain source directory {source:?}"))?;
            ensure!(
                directory.is_dir(),
                "toolchain source {source:?} must be a directory"
            );
            Source::Directory(
                directory
                    .into_os_string()
                    .into_string()
                    .map_err(|path| anyhow!("toolchain source directory {path:?} must be UTF-8"))?,
            )
        };
        Ok(Self { source })
    }

    pub(crate) fn source(&self) -> &str {
        match &self.source {
            Source::Directory(path) => path,
            Source::Https { root, .. } => root.as_str(),
        }
    }

    pub(crate) fn resolve(&self, selector: &Selector, host: &str) -> Result<Artifact> {
        let id = match selector {
            Selector::Channel(channel) => self.resolve_channel(*channel)?,
            Selector::Exact(id) => id.clone(),
        };
        let tag = id.tag();
        let bytes = self.read_asset(&["download", &tag, MANIFEST_NAME], MAX_MANIFEST_BYTES)?;
        let mut manifest: Manifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("read release manifest for {id}"))?;
        ensure!(
            manifest.format == 1,
            "unsupported release manifest format {} for {id}",
            manifest.format
        );
        ensure!(
            manifest.toolchain == id,
            "release {id} contains the manifest for {}",
            manifest.toolchain
        );
        if let ReleaseId::Version(version) = &id {
            ensure!(
                &manifest.version == version,
                "release {id} declares version {}",
                manifest.version
            );
        }
        ensure!(
            is_hex(&manifest.commit, 40),
            "invalid commit in manifest for {id}"
        );
        let Target {
            mut runtime,
            mut launcher,
            mut sdk,
        } = manifest
            .targets
            .remove(host)
            .with_context(|| format!("release {id} has no toolchain for {host}"))?;
        runtime
            .validate()
            .with_context(|| format!("invalid runtime archive in manifest for {id} on {host}"))?;
        launcher
            .validate()
            .with_context(|| format!("invalid launcher archive in manifest for {id} on {host}"))?;
        for (target, archive) in &mut sdk {
            validate_name(target)?;
            archive.validate()?;
        }
        manifest.commit.make_ascii_lowercase();
        Ok(Artifact {
            id,
            version: manifest.version,
            commit: manifest.commit,
            runtime,
            launcher,
            sdk,
        })
    }

    /// Stage the runtime for an artifact resolved for this host in a fresh directory.
    pub(crate) fn install_runtime(
        &self,
        artifact: &Artifact,
        host: &str,
        staging: &Path,
    ) -> Result<()> {
        let stem = format!("zkcl-{}-{host}", artifact.id);
        self.install_archive(
            &artifact.id,
            &stem,
            &artifact.runtime,
            RUNTIME_FILES,
            staging,
        )?;
        if staging.join("sdk/wasm32-unknown-unknown").is_dir() {
            validate_wasm_sdk(staging, "wasm32-unknown-unknown", &artifact.version)?;
        }
        Ok(())
    }

    /// Stage the launcher for an artifact resolved for this host in a fresh directory.
    pub(crate) fn install_launcher(
        &self,
        artifact: &Artifact,
        host: &str,
        staging: &Path,
    ) -> Result<PathBuf> {
        let stem = format!("zkcl-launcher-{host}");
        self.install_archive(
            &artifact.id,
            &stem,
            &artifact.launcher,
            LAUNCHER_FILES,
            staging,
        )?;
        Ok(staging.join(EXECUTABLE))
    }

    pub(crate) fn install_component(
        &self,
        artifact: &Artifact,
        component: &str,
        target: &str,
        staging: &Path,
    ) -> Result<()> {
        let (stem, spec, required) = match component {
            "sdk" => (
                format!("zkcl-sdk-{}-{target}", artifact.id),
                artifact
                    .sdk
                    .get(target)
                    .context("release does not provide this SDK target")?,
                if target == "wasm32-unknown-unknown" {
                    vec![
                        format!("sdk/{target}/runtime.wasm"),
                        format!("sdk/{target}/runtime.json"),
                    ]
                } else {
                    vec![
                        format!("sdk/{target}/lib/libzkcl_capi.a"),
                        format!("sdk/{target}/include/zkcl.h"),
                        format!("sdk/{target}/runtime.json"),
                    ]
                },
            ),
            _ => anyhow::bail!("unknown component {component}"),
        };
        let required: Vec<_> = required.iter().map(String::as_str).collect();
        self.install_archive(&artifact.id, &stem, spec, &required, staging)?;
        if component == "sdk" && target == "wasm32-unknown-unknown" {
            validate_wasm_sdk(staging, target, &artifact.version)?;
        }
        Ok(())
    }

    fn resolve_channel(&self, channel: Channel) -> Result<ReleaseId> {
        let name = channel.to_string();
        let path = match channel {
            Channel::Stable => ["latest", "download", POINTER_NAME],
            Channel::Beta | Channel::Nightly => ["download", &name, POINTER_NAME],
        };
        let bytes = self.read_asset(&path, MAX_POINTER_BYTES)?;
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("read {channel} channel pointer"))?;
        let id: ReleaseId = text
            .strip_suffix('\n')
            .with_context(|| format!("{channel} channel pointer must end with a newline"))?
            .parse()
            .with_context(|| format!("read {channel} channel pointer"))?;
        ensure!(
            channel.accepts(&id),
            "{channel} channel points to incompatible toolchain {id}"
        );
        Ok(id)
    }

    fn read_asset(&self, path: &[&str], limit: u64) -> Result<Vec<u8>> {
        let name = path.join("/");
        let mut data = Vec::new();
        self.open_asset(path, limit, Duration::from_secs(30))?
            .take(limit + 1)
            .read_to_end(&mut data)
            .with_context(|| format!("read release asset {name}"))?;
        ensure!(
            data.len() as u64 <= limit,
            "release asset {name} exceeds {limit} bytes"
        );
        Ok(data)
    }

    fn open_asset(&self, path: &[&str], limit: u64, timeout: Duration) -> Result<Box<dyn Read>> {
        match &self.source {
            Source::Directory(root) => {
                let path = Path::new(root).join(path.join("/"));
                let metadata = fs::metadata(&path)
                    .with_context(|| format!("open release asset {}", path.display()))?;
                ensure!(
                    metadata.is_file(),
                    "release asset {} must be a regular file",
                    path.display()
                );
                ensure!(
                    metadata.len() <= limit,
                    "release asset {} exceeds {limit} bytes",
                    path.display()
                );
                let file = File::open(&path)
                    .with_context(|| format!("open release asset {}", path.display()))?;
                Ok(Box::new(file))
            }
            Source::Https { root, client } => {
                let mut url = root.clone();
                url.path_segments_mut()
                    .map_err(|()| anyhow!("toolchain source URL cannot contain an asset path"))?
                    .pop_if_empty()
                    .extend(path.iter().copied());
                let response = client
                    .get(url.clone())
                    .timeout(timeout)
                    .header(reqwest::header::ACCEPT_ENCODING, "identity")
                    .send()
                    .map_err(reqwest::Error::without_url)
                    .with_context(|| format!("get release asset {url}"))?;
                ensure!(
                    response.status().is_success(),
                    "release asset {url} returned HTTP {}",
                    response.status()
                );
                ensure!(
                    response.content_length().is_none_or(|size| size <= limit),
                    "release asset {url} exceeds {limit} bytes"
                );
                Ok(Box::new(response))
            }
        }
    }

    fn install_archive(
        &self,
        id: &ReleaseId,
        stem: &str,
        spec: &Archive,
        files: &[&str],
        staging: &Path,
    ) -> Result<()> {
        let filename = format!("{stem}.tar.gz");
        let tag = id.tag();
        let file = self
            .download(&["download", &tag, &filename], spec, staging)
            .with_context(|| format!("download {filename}"))?;
        unpack(file, stem, files, staging).with_context(|| format!("unpack {filename}"))
    }

    fn download(&self, path: &[&str], spec: &Archive, staging: &Path) -> Result<File> {
        let mut source = self
            .open_asset(path, spec.size, Duration::from_secs(180))?
            .take(spec.size + 1);
        let mut file = tempfile::tempfile_in(staging).context("create archive download file")?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        let mut size = 0;
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            file.write_all(&buffer[..count])?;
            size += count as u64;
        }
        ensure!(
            size == spec.size,
            "archive has {size} bytes, expected {}",
            spec.size
        );
        let digest = format!("{:x}", hash.finalize());
        ensure!(
            digest == spec.sha256,
            "archive SHA-256 is {digest}, expected {}",
            spec.sha256
        );
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }
}

impl Archive {
    fn validate(&mut self) -> Result<()> {
        ensure!(
            (1..=MAX_ARCHIVE_BYTES).contains(&self.size),
            "invalid archive size {}",
            self.size
        );
        ensure!(is_hex(&self.sha256, 64), "invalid archive SHA-256");
        self.sha256.make_ascii_lowercase();
        Ok(())
    }
}

fn is_hex(text: &str, length: usize) -> bool {
    text.len() == length && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unpack(file: File, stem: &str, files: &[&str], staging: &Path) -> Result<()> {
    fs::create_dir_all(staging.join("bin")).context("create staged bin directory")?;
    let gzip = GzDecoder::new(BufReader::new(file));
    let mut archive = tar::Archive::new(gzip.take(MAX_TAR_BYTES + 1));
    let mut seen = HashSet::new();
    let mut long_name = None;
    // Read GNU long-name records explicitly so their size is bounded before allocation.
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() {
            ensure!(
                long_name.is_none() && entry.size() <= MAX_TAR_PATH_BYTES,
                "invalid GNU long path record"
            );
            let mut name = Vec::new();
            entry.read_to_end(&mut name)?;
            ensure!(
                name.len() as u64 == entry.size() && name.pop() == Some(0),
                "invalid GNU long path record"
            );
            long_name = Some(name);
            continue;
        }

        let path = long_name
            .take()
            .map_or_else(|| Cow::Owned(entry.path_bytes().into_owned()), Cow::Owned);
        let path = std::str::from_utf8(&path).context("archive path must be UTF-8")?;
        let suffix = path
            .strip_prefix(stem)
            .with_context(|| format!("unexpected path {path:?} in archive"))?;
        ensure!(
            suffix.is_empty() || suffix.starts_with('/'),
            "archive path is outside its root"
        );
        if kind.is_dir() {
            let directory = suffix.trim_start_matches('/').trim_end_matches('/');
            validate_relative(directory)?;
            fs::create_dir_all(staging.join(directory))?;
            ensure!(
                entry.size() == 0,
                "archive directory {path:?} contains file data"
            );
            ensure!(
                seen.insert(directory.to_owned()),
                "duplicate archive directory {path:?}"
            );
            continue;
        }

        ensure!(
            kind.is_file(),
            "archive entry {path:?} must be a regular file"
        );
        let file = suffix
            .strip_prefix('/')
            .context("archive file is outside its root")?;
        validate_relative(file)?;
        ensure!(
            files.contains(&file) || file.starts_with("sdk/"),
            "unexpected file {path:?} in archive"
        );
        ensure!(
            seen.insert(file.to_owned()),
            "duplicate archive file {path:?}"
        );
        let destination = staging.join(file);
        fs::create_dir_all(destination.parent().context("archive file has no parent")?)?;
        ensure!(
            entry.size() <= MAX_TAR_BYTES,
            "archive file {path:?} exceeds the unpacked size limit"
        );
        extract_file(&mut entry, &staging.join(file), file == EXECUTABLE)
            .with_context(|| format!("extract {file}"))?;
    }
    ensure!(
        long_name.is_none(),
        "archive ends with an unused GNU long path record"
    );
    for file in files {
        ensure!(seen.contains(*file), "archive is missing {file}");
    }

    // Finish decoding so the gzip trailer and all remaining tar padding are checked.
    let mut tail = archive.into_inner();
    let mut buffer = [0; 8192];
    loop {
        let count = tail.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        ensure!(
            buffer[..count].iter().all(|byte| *byte == 0),
            "unexpected data after the tar entries"
        );
    }
    ensure!(tail.limit() != 0, "archive exceeds the unpacked size limit");
    let mut input = tail.into_inner().into_inner();
    ensure!(
        input.fill_buf()?.is_empty(),
        "unexpected data after the gzip stream"
    );
    Ok(())
}

fn extract_file(
    entry: &mut tar::Entry<'_, impl Read>,
    path: &Path,
    executable: bool,
) -> Result<()> {
    let mode = entry.header().mode()? & 0o777;
    ensure!(
        !executable || mode & 0o100 != 0,
        "the archive executable is missing its owner execute permission"
    );
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let size = io::copy(entry, &mut file)?;
    ensure!(size == entry.size(), "archive file is truncated");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    file.sync_all()?;
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid target {name:?}"
    );
    Ok(())
}

fn validate_relative(path: &str) -> Result<()> {
    ensure!(
        path.len() <= MAX_TAR_PATH_BYTES as usize
            && !path.contains('\\')
            && !path.contains(':')
            && !path.starts_with('/'),
        "unsafe archive path {path:?}"
    );
    ensure!(
        path.is_empty()
            || path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."),
        "unsafe archive path {path:?}"
    );
    Ok(())
}

fn validate_wasm_sdk(staging: &Path, target: &str, version: &Version) -> Result<()> {
    let root = staging.join("sdk").join(target);
    let mut magic = [0; 8];
    File::open(root.join("runtime.wasm"))?.read_exact(&mut magic)?;
    ensure!(
        magic == *b"\0asm\x01\0\0\0",
        "invalid Wasm SDK runtime header"
    );
    #[derive(Deserialize)]
    struct Metadata {
        kind: String,
        target: String,
        version: Version,
    }
    let mut bytes = Vec::new();
    File::open(root.join("runtime.json"))?
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_MANIFEST_BYTES,
        "Wasm SDK metadata is too large"
    );
    let metadata: Metadata = serde_json::from_slice(&bytes).context("read Wasm SDK metadata")?;
    ensure!(
        metadata.kind == "wasm" && metadata.target == target && &metadata.version == version,
        "Wasm SDK metadata does not match release target and version"
    );
    Ok(())
}
