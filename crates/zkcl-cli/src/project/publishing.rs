use super::{registries, workspace::Project};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zkcl_registry::{
    Digest,
    remote::{Control, RegistryClient},
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    tokens: BTreeMap<String, String>,
}
fn credentials_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ZKCL_CREDENTIALS_FILE") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err("ZKCL_CREDENTIALS_FILE must be absolute".into());
        }
        return Ok(path);
    }
    Ok(configuration_base(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )?
    .join("zkcl/credentials.json"))
}
fn configuration_base(xdg: Option<PathBuf>, home: Option<PathBuf>) -> Result<PathBuf> {
    xdg.filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or_else(|| "set absolute XDG_CONFIG_HOME or HOME for registry credentials".into())
}

fn read_credentials(path: &Path) -> Result<Credentials> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Credentials::default()),
        Err(e) => return Err(e.into()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata()?.permissions().mode() & 0o077 != 0 {
            return Err("registry credentials file must have mode 0600".into());
        }
    }
    if file.metadata()?.len() > 1024 * 1024 {
        return Err("registry credentials file exceeds limit".into());
    }
    Ok(serde_json::from_reader(file)?)
}
fn save_credentials(path: &Path, credentials: &Credentials) -> Result<()> {
    let parent = path
        .parent()
        .ok_or("credentials file needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(file.as_file_mut(), credentials)?;
    file.flush()?;
    if file.as_file().metadata()?.len() > 1024 * 1024 {
        return Err("registry credentials file exceeds limit".into());
    }
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}
fn edit_credentials(path: &Path, edit: impl FnOnce(&mut Credentials)) -> Result<()> {
    let parent = path
        .parent()
        .ok_or("credentials file needs a parent directory")?;
    fs::create_dir_all(parent)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(path.with_extension("lock"))?;
    lock.lock()?;
    let mut credentials = read_credentials(path)?;
    edit(&mut credentials);
    save_credentials(path, &credentials)
}
fn token(client: &RegistryClient) -> Result<String> {
    if let Ok(token) = std::env::var("ZKCL_REGISTRY_TOKEN") {
        zkcl_registry::remote::validate_token(&token)?;
        return Ok(token);
    }
    read_credentials(&credentials_path()?)?
        .tokens
        .remove(&client.identity())
        .ok_or_else(|| "registry token missing; use zkcl login or ZKCL_REGISTRY_TOKEN".into())
}
pub(super) fn login(url: &str) -> Result<()> {
    let client = RegistryClient::new(url)?;
    let token = if let Ok(token) = std::env::var("ZKCL_REGISTRY_TOKEN") {
        token
    } else {
        use std::io::IsTerminal;
        if std::io::stdin().is_terminal() {
            return Err("pipe a registry token on stdin or set ZKCL_REGISTRY_TOKEN".into());
        }
        let mut token = String::new();
        std::io::stdin().take(4098).read_to_string(&mut token)?;
        token.trim_end_matches(['\r', '\n']).to_owned()
    };
    client.control(&token, &Control::Authenticate)?;
    let path = credentials_path()?;
    edit_credentials(&path, |credentials| {
        credentials.tokens.insert(client.identity(), token);
    })?;
    println!("Logged in to {}", client.identity());
    Ok(())
}
pub(super) fn logout(url: &str) -> Result<()> {
    let client = RegistryClient::new(url)?;
    let path = credentials_path()?;
    edit_credentials(&path, |credentials| {
        credentials.tokens.remove(&client.identity());
    })?;
    println!("Removed saved credentials for {}", client.identity());
    Ok(())
}
fn archive(project: &Project, files: &[PathBuf]) -> Result<(Vec<u8>, Digest)> {
    let mut bytes = Vec::new();
    let digest = zkcl_registry::write_source_archive(&project.root, files, &mut bytes)?;
    Ok((bytes, digest))
}
pub(super) fn package(project: &Project, output: Option<&Path>, list: bool) -> Result<Digest> {
    let files = registries::archive_files(project)?;
    let (bytes, digest) = archive(project, &files)?;
    if list {
        for file in files {
            println!("{}", file.display());
        }
    }
    if let Some(path) = output {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut pending = tempfile::NamedTempFile::new_in(parent)?;
        pending.write_all(&bytes)?;
        pending.persist(path)?;
    }
    println!(
        "Packaged {} {} sha256:{digest} ({} bytes)",
        project.manifest.name,
        project.manifest.version,
        bytes.len()
    );
    Ok(digest)
}
pub(super) fn publish(project: &Project, url: &str, dry_run: bool) -> Result<Digest> {
    let files = registries::archive_files(project)?;
    let (bytes, digest) = archive(project, &files)?;
    if !dry_run {
        let client = RegistryClient::new(url)?;
        client.publish(
            &token(&client)?,
            project.manifest.name.as_str(),
            &project.manifest.version.to_string(),
            digest,
            bytes,
        )?;
    }
    println!(
        "{} {} {} sha256:{digest}",
        if dry_run {
            "Verified package"
        } else {
            "Published"
        },
        project.manifest.name,
        project.manifest.version
    );
    Ok(digest)
}
pub(super) fn control(url: &str, operation: Control) -> Result<()> {
    let client = RegistryClient::new(url)?;
    client.control(&token(&client)?, &operation)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_directory_uses_absolute_xdg_then_home() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"));
        let home = base.join("home");
        let config = base.join("config");
        assert_eq!(
            configuration_base(Some(config.clone()), Some(home.clone())).unwrap(),
            config
        );
        assert_eq!(
            configuration_base(Some("relative".into()), Some(home.clone())).unwrap(),
            home.join(".config")
        );
        assert!(configuration_base(None, Some("relative".into())).is_err());
    }
    #[test]
    fn concurrent_credential_edits_preserve_other_registries() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join(".agents/tmp/zkcl/registry-remote");
        fs::create_dir_all(&root).unwrap();
        let temporary = tempfile::tempdir_in(root).unwrap();
        let path = temporary.path().join("credentials.json");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    edit_credentials(&path, |credentials| {
                        credentials.tokens.insert(
                            format!("https://registry-{index}.example"),
                            "secret-token".into(),
                        );
                    })
                    .unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(read_credentials(&path).unwrap().tokens.len(), 8);
        edit_credentials(&path, |credentials| {
            credentials.tokens.remove("https://registry-0.example");
        })
        .unwrap();
        assert_eq!(read_credentials(&path).unwrap().tokens.len(), 7);
    }
    #[test]
    fn credentials_are_private_atomic_and_removable() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join(".agents/tmp/zkcl/registry-remote");
        fs::create_dir_all(&root).unwrap();
        let temporary = tempfile::tempdir_in(root).unwrap();
        let path = temporary.path().join("config/credentials.json");
        edit_credentials(&path, |credentials| {
            credentials
                .tokens
                .insert("https://registry.example".into(), "secret-token".into());
        })
        .unwrap();
        assert_eq!(
            read_credentials(&path).unwrap().tokens["https://registry.example"],
            "secret-token"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_credentials(&path).is_err());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        edit_credentials(&path, |credentials| credentials.tokens.clear()).unwrap();
        assert!(read_credentials(&path).unwrap().tokens.is_empty());
    }
}
