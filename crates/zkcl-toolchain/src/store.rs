use std::collections::BTreeMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tempfile::{Builder, NamedTempFile};

use crate::dist::{Artifact, DEFAULT_SOURCE, Distribution};
use crate::selector::{Channel, ReleaseId, Selector};

pub(crate) struct Store {
    pub(crate) root: PathBuf,
    pub(crate) host: &'static str,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct State {
    pub(crate) source: String,
    pub(crate) default: Option<Selector>,
    pub(crate) channels: BTreeMap<Channel, ReleaseId>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            source: DEFAULT_SOURCE.to_owned(),
            default: None,
            channels: BTreeMap::new(),
        }
    }
}

impl State {
    pub(crate) fn resolve(&self, selector: &Selector) -> Option<ReleaseId> {
        match selector {
            Selector::Exact(id) => Some(id.clone()),
            Selector::Channel(channel) => self.channels.get(channel).cloned(),
        }
    }
}

impl Store {
    pub(crate) fn new(root: PathBuf, host: &'static str) -> Self {
        Self { root, host }
    }

    pub(crate) fn read(&self) -> Result<State> {
        Ok(read_json(&self.root.join("state.json"))?.unwrap_or_default())
    }

    pub(crate) fn read_lock(&self) -> Result<File> {
        let lock = self.open_lock(false)?;
        lock.lock_shared()
            .context("lock toolchain store for reading")?;
        Ok(lock)
    }

    pub(crate) fn lock(&self) -> Result<Transaction<'_>> {
        let mut directories = fs::DirBuilder::new();
        directories.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directories.mode(0o700);
        }
        directories
            .create(&self.root)
            .with_context(|| format!("create toolchain store {}", self.root.display()))?;
        let lock = self.open_lock(true)?;
        lock.lock().context("lock toolchain store for writing")?;
        let state = self.read()?;
        Ok(Transaction {
            store: self,
            state,
            _lock: lock,
        })
    }

    fn open_lock(&self, create: bool) -> Result<File> {
        let path = self.root.join("lock");
        if let Some(metadata) = metadata(&path)? {
            ensure!(
                metadata.is_file(),
                "invalid toolchain lock {}",
                path.display()
            );
        }
        OpenOptions::new()
            .read(true)
            .write(create)
            .create(create)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("open toolchain lock {}", path.display()))
    }

    fn toolchains(&self) -> PathBuf {
        self.root.join("toolchains")
    }

    pub(crate) fn release_dir(&self, id: &ReleaseId) -> PathBuf {
        let id = id.to_string();
        let mut component = String::with_capacity(id.len());
        // SemVer identifiers are case-sensitive, including on case-insensitive volumes.
        // The release ID grammar reserves `~` for this escape.
        for byte in id.bytes() {
            if byte.is_ascii_uppercase() {
                component.push('~');
                component.push(char::from(byte.to_ascii_lowercase()));
            } else {
                component.push(char::from(byte));
            }
        }
        self.toolchains().join(component)
    }

    pub(crate) fn binary(&self, id: &ReleaseId) -> PathBuf {
        self.release_dir(id).join("bin/zkcl")
    }

    pub(crate) fn artifact(&self, id: &ReleaseId) -> Result<Option<Artifact>> {
        let directory = self.release_dir(id);
        let Some(directory_metadata) = metadata(&directory)? else {
            return Ok(None);
        };
        ensure!(
            directory_metadata.is_dir(),
            "invalid toolchain directory {}",
            directory.display()
        );
        self.read_artifact(&directory)?
            .map(Some)
            .with_context(|| format!("missing metadata for toolchain {id}"))
    }

    fn read_artifact(&self, directory: &Path) -> Result<Option<Artifact>> {
        let Some(artifact) = read_json::<Artifact>(&directory.join("release.json"))? else {
            return Ok(None);
        };
        ensure!(
            self.release_dir(&artifact.id) == directory,
            "toolchain metadata does not match directory {}",
            directory.display()
        );
        let binary = directory.join("bin/zkcl");
        let binary_metadata = fs::symlink_metadata(&binary)
            .with_context(|| format!("inspect toolchain executable {}", binary.display()))?;
        ensure!(
            binary_metadata.is_file(),
            "invalid toolchain executable {}",
            binary.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                binary_metadata.permissions().mode() & 0o111 != 0,
                "toolchain file {} is not executable",
                binary.display()
            );
        }
        Ok(Some(artifact))
    }

    pub(crate) fn installed(&self, id: &ReleaseId) -> Result<bool> {
        Ok(self.artifact(id)?.is_some())
    }

    pub(crate) fn installed_ids(&self) -> Result<Vec<ReleaseId>> {
        let path = self.toolchains();
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).with_context(|| format!("list {}", path.display())),
        };
        let mut releases = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_name().as_encoded_bytes().starts_with(b".")
                || !entry.file_type()?.is_dir()
            {
                continue;
            }
            if let Some(artifact) = self.read_artifact(&entry.path())? {
                releases.push(artifact.id);
            }
        }
        releases.sort();
        Ok(releases)
    }
}

pub(crate) struct Transaction<'a> {
    store: &'a Store,
    pub(crate) state: State,
    _lock: File,
}

impl Transaction<'_> {
    pub(crate) fn commit(&self) -> Result<()> {
        write_json(&self.store.root.join("state.json"), &self.state)
    }

    pub(crate) fn install(&self, dist: &Distribution, artifact: &Artifact) -> Result<()> {
        let id = &artifact.id;
        if let Some(installed) = self.store.artifact(id)? {
            ensure!(
                installed == *artifact,
                "installed toolchain {id} has different release metadata"
            );
            return Ok(());
        }

        let toolchains = self.store.toolchains();
        create_directory(&toolchains)?;
        let staging = Builder::new().prefix(".install-").tempdir_in(&toolchains)?;
        dist.install_runtime(artifact, self.store.host, staging.path())?;
        write_json(&staging.path().join("release.json"), artifact)?;
        sync_directory(&staging.path().join("bin"))?;
        fs::rename(staging.path(), self.store.release_dir(id))
            .with_context(|| format!("publish toolchain {id}"))?;
        let _ = staging.keep();
        sync_directory(&toolchains)
    }

    pub(crate) fn component(
        &self,
        dist: &Distribution,
        artifact: &Artifact,
        component: &str,
        target: &str,
        remove: bool,
    ) -> Result<()> {
        ensure!(component == "sdk", "unknown component {component}");
        let relative = format!("sdk/{target}");
        let release = self.store.release_dir(&artifact.id);
        let destination = release.join(&relative);
        if remove {
            ensure!(destination.is_dir(), "component is not installed");
            let trash = Builder::new()
                .prefix(".component-remove-")
                .tempdir_in(&release)?;
            fs::rename(&destination, trash.path().join("component"))?;
            sync_directory(&release)?;
            return trash.close().context("remove component");
        }
        if destination.is_dir() {
            return Ok(());
        }
        let staging = Builder::new().prefix(".component-").tempdir_in(&release)?;
        dist.install_component(artifact, component, target, staging.path())?;
        let source = staging.path().join(&relative);
        ensure!(source.is_dir(), "component archive is missing {relative}");
        fs::create_dir_all(destination.parent().expect("component parent"))?;
        fs::rename(source, &destination)?;
        sync_directory(&release)
    }

    #[cfg(unix)]
    pub(crate) fn check_launcher(&self) -> Result<()> {
        self.launcher_status().map(|_| ())
    }

    #[cfg(not(unix))]
    pub(crate) fn check_launcher(&self) -> Result<()> {
        anyhow::bail!("managed launcher installation requires Unix")
    }

    #[cfg(unix)]
    fn launcher_status(&self) -> Result<(Option<String>, bool)> {
        let bin = self.store.root.join("bin");
        if let Some(bin_metadata) = metadata(&bin)? {
            ensure!(
                bin_metadata.is_dir(),
                "refusing to use unmanaged {}",
                bin.display()
            );
        }
        let launcher = bin.join("zkcl");
        let alias = bin.join("zkclx");
        let create_alias = match metadata(&alias)? {
            Some(entry) => {
                ensure!(
                    entry.is_symlink() && fs::read_link(&alias)? == Path::new("zkcl"),
                    "refusing to replace unmanaged {}",
                    alias.display()
                );
                false
            }
            None => true,
        };
        let ownership_path = self.store.root.join("launcher.json");
        let recorded: Option<LauncherDigests> = read_json(&ownership_path)?;
        let previous = if let Some(entry) = metadata(&launcher)? {
            ensure!(
                entry.is_file(),
                "refusing to replace unmanaged {}",
                launcher.display()
            );
            let recorded = recorded
                .with_context(|| format!("refusing to replace unmanaged {}", launcher.display()))?;
            let digest = launcher_digest(&launcher)?;
            ensure!(
                recorded.staged == digest || recorded.previous.as_ref() == Some(&digest),
                "refusing to replace unmanaged {}",
                launcher.display()
            );
            Some(digest)
        } else {
            None
        };
        Ok((previous, create_alias))
    }

    #[cfg(unix)]
    pub(crate) fn promote_launcher(&self, verified_binary: &Path) -> Result<()> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let (previous, create_alias) = self.launcher_status()?;
        let bin = self.store.root.join("bin");
        create_directory(&bin)?;
        let launcher = bin.join("zkcl");
        let alias = bin.join("zkclx");
        let ownership_path = self.store.root.join("launcher.json");

        let mut staging = NamedTempFile::new_in(&bin)?;
        let mut source = File::open(verified_binary)
            .with_context(|| format!("open verified launcher {}", verified_binary.display()))?;
        io::copy(&mut source, &mut staging).context("stage launcher executable")?;
        staging
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
        staging.as_file().sync_all()?;
        let staged = launcher_digest(staging.path())?;
        let staging = staging.into_temp_path();
        let replace = previous.is_some();

        // A retry can identify the managed launcher on either side of the rename.
        write_json(&ownership_path, &LauncherDigests { previous, staged })?;
        if create_alias {
            symlink("zkcl", &alias).with_context(|| format!("create {}", alias.display()))?;
        }
        let published = if replace {
            staging.persist(&launcher)
        } else {
            staging.persist_noclobber(&launcher)
        };
        if let Err(error) = published {
            if create_alias {
                let _ = fs::remove_file(&alias);
            }
            return Err(error).with_context(|| format!("publish launcher {}", launcher.display()));
        }
        sync_directory(&bin)
    }

    #[cfg(not(unix))]
    pub(crate) fn promote_launcher(&self, _verified_binary: &Path) -> Result<()> {
        anyhow::bail!("managed launcher installation requires Unix")
    }

    pub(crate) fn uninstall(&mut self, id: &ReleaseId) -> Result<()> {
        let selector = self
            .state
            .default
            .as_ref()
            .unwrap_or(&Selector::Channel(Channel::Stable));
        ensure!(
            self.state.resolve(selector).as_ref() != Some(id),
            "cannot uninstall the default toolchain {id}"
        );
        ensure!(self.store.installed(id)?, "toolchain {id} is not installed");

        let toolchains = self.store.toolchains();
        ensure!(
            fs::symlink_metadata(&toolchains)?.is_dir(),
            "expected directory at {}",
            toolchains.display()
        );
        let trash = Builder::new().prefix(".remove-").tempdir_in(&toolchains)?;
        let mut state = self.state.clone();
        state.channels.retain(|_, installed| installed != id);
        let original = self.store.release_dir(id);
        let moved = trash.path().join("release");
        let state_path = self.store.root.join("state.json");
        // Publish state first so no alias points at the release after it is moved.
        write_json(&state_path, &state)?;
        if let Err(error) = fs::rename(&original, &moved) {
            if let Err(rollback_error) = write_json(&state_path, &self.state) {
                self.state = state;
                return Err(error).context(format!(
                    "remove toolchain {id}; also failed to restore toolchain state: {rollback_error:#}"
                ));
            }
            return Err(error).with_context(|| format!("remove toolchain {id}"));
        }
        self.state = state;
        sync_directory(&toolchains)?;
        trash
            .close()
            .with_context(|| format!("clean up toolchain {id}"))
    }
}

#[cfg(unix)]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LauncherDigests {
    previous: Option<String>,
    staged: String,
}

#[cfg(unix)]
fn launcher_digest(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let mut input =
        File::open(path).with_context(|| format!("open launcher {}", path.display()))?;
    let mut digest = Sha256::new();
    io::copy(&mut input, &mut digest)
        .with_context(|| format!("hash launcher {}", path.display()))?;
    Ok(format!("{:x}", digest.finalize()))
}

fn metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn create_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            ensure!(
                fs::symlink_metadata(path)?.is_dir(),
                "expected directory at {}",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let Some(entry) = metadata(path)? else {
        return Ok(None);
    };
    ensure!(
        entry.is_file(),
        "expected regular file at {}",
        path.display()
    );
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    serde_json::from_reader(io::BufReader::new(file))
        .map(Some)
        .with_context(|| format!("read {}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let directory = path.parent().expect("store file has a parent");
    let mut staging = NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(&mut staging, value)
        .with_context(|| format!("write {}", path.display()))?;
    writeln!(staging)?;
    staging.as_file().sync_all()?;
    staging
        .persist(path)
        .with_context(|| format!("publish {}", path.display()))?;
    sync_directory(directory)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", path.display()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
