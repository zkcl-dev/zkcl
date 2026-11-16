use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path};
use std::process::{Command, Output, Stdio};

use crate::solver::{ModuleId, SourceId};
use zkcl_registry::{ContentStore, Digest};

/// Obtain a commit's canonical source archive. The lock digest is authoritative
/// whenever supplied; an offline request never consults Git metadata or a remote.
pub fn read(
    id: &ModuleId,
    store: &ContentStore,
    expected: Option<Digest>,
    offline: bool,
) -> io::Result<(Digest, String)> {
    let SourceId::Git { url, rev } = &id.source else {
        return Err(io::Error::other("expected a Git source"));
    };
    validate_source(url, rev)?;
    if let Some(digest) = expected {
        match store.manifest(digest) {
            Ok(text) => return Ok((digest, text)),
            Err(error) if offline || store.archive_path(digest).try_exists()? => {
                return Err(io::Error::other(format!(
                    "cannot read locked Git source {id}: {error}; run `zkcl fetch`"
                )));
            }
            Err(_) => {}
        }
    }
    if offline {
        return Err(io::Error::other(format!(
            "offline Git source {id} needs a locked checksum and cached archive; run `zkcl fetch`"
        )));
    }
    let cache = store
        .root()
        .join("git")
        .join(Digest::of_bytes(url.as_bytes()).to_string());
    fs::create_dir_all(&cache)?;
    let guard = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(cache.join("lock"))?;
    guard.lock()?;
    let format = if rev.len() == 64 { "sha256" } else { "sha1" };
    let objects = cache.join(format!("objects-{format}.git"));
    if !objects.join("HEAD").is_file() {
        run(git(&cache)
            .args(["init", "--bare", "--template="])
            .arg(format!("--object-format={format}"))
            .arg(&objects))?;
    }
    let commit = format!("{rev}^{{commit}}");
    if !git(&objects)
        .args(["cat-file", "-e", &commit])
        .output()?
        .status
        .success()
    {
        run(git(&objects).args(["fetch", "--no-tags", "--depth=1", "--", url, rev]))?;
    }
    let actual = run(git(&objects).args(["rev-parse", "--verify", &commit]))?;
    if actual.stdout.strip_suffix(b"\n").unwrap_or(&actual.stdout) != rev.as_bytes() {
        return Err(io::Error::other(format!(
            "Git source {id} did not resolve to its requested commit"
        )));
    }
    let digest = archive(&objects, rev, store)?;
    if expected.is_some_and(|expected| expected != digest) {
        return Err(io::Error::other(format!(
            "Git source {id} differs from its locked SHA-256 checksum"
        )));
    }
    let manifest = store.manifest(digest).map_err(io::Error::other)?;
    Ok((digest, manifest))
}

fn validate_source(url: &str, rev: &str) -> io::Result<()> {
    if !matches!(rev.len(), 40 | 64)
        || !rev
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::other(
            "Git revision must be a full lowercase commit object ID (40 or 64 hexadecimal characters)",
        ));
    }
    let address = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("ssh://"));
    if address.is_none_or(|address| address.is_empty() || address.starts_with('/'))
        || url
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(io::Error::other(
            "Git sources require an explicit https:// or ssh:// URL",
        ));
    }
    Ok(())
}

fn git(directory: &Path) -> Command {
    let mut command = Command::new("git");
    // Ignore inherited repository selectors, configuration injection, transport
    // helpers and hooks. Only the two explicitly supported transports may run.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "https:ssh")
        .args([
            "-c",
            "core.hooksPath=",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.file.allow=never",
        ]);
    command
}

fn run(command: &mut Command) -> io::Result<Output> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "Git source command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output)
}

fn archive(objects: &Path, rev: &str, store: &ContentStore) -> io::Result<Digest> {
    let entries = run(git(objects).args(["ls-tree", "-rz", "--full-tree", rev]))?;
    // Export blobs ourselves: Git checkout filters, attributes, hooks, symlinks
    // and submodules cannot change the locked source content.
    let source = tempfile::Builder::new()
        .prefix("git-source-")
        .tempdir_in(store.root())?;
    let mut files = Vec::new();
    for entry in entries
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let entry = std::str::from_utf8(entry).map_err(io::Error::other)?;
        let (header, path) = entry
            .split_once('\t')
            .ok_or_else(|| io::Error::other("invalid Git tree entry"))?;
        let fields: Vec<_> = header.split(' ').collect();
        if fields.len() != 3 || fields[1] != "blob" || !matches!(fields[0], "100644" | "100755") {
            return Err(io::Error::other(format!(
                "Git source entry {path:?} must be a regular file; symlinks and submodules are unsupported"
            )));
        }
        let relative = Path::new(path);
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(io::Error::other(
                "Git source path must remain inside its archive",
            ));
        }
        let destination = source.path().join(relative);
        fs::create_dir_all(destination.parent().expect("source file has a parent"))?;
        let file = File::create(&destination)?;
        let output = git(objects)
            .args(["cat-file", "blob", fields[2]])
            .stdout(Stdio::from(file))
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "cannot export Git blob {path:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        files.push((relative.to_owned(), fields[0] == "100755"));
    }
    store
        .publish_with_modes(source.path(), &files)
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_git_source() -> (tempfile::TempDir, ContentStore, ModuleId) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.agents/tmp/zkcl/project-dependencies");
        fs::create_dir_all(&root).unwrap();
        let temp = tempfile::tempdir_in(root).unwrap();
        let store = ContentStore::new(temp.path().join("store"));
        let url = "https://example.invalid/sample.git";
        let objects = store
            .root()
            .join("git")
            .join(Digest::of_bytes(url.as_bytes()).to_string())
            .join("objects-sha1.git");
        fs::create_dir_all(&objects).unwrap();
        run(git(&objects).args(["init", "--bare", "--template=", "."])).unwrap();
        let manifest = temp.path().join("zkcl.sexp");
        fs::write(&manifest, "(project \"test/lib\" (version \"1.0.0\"))").unwrap();
        let blob = run(git(&objects).args(["hash-object", "-w"]).arg(&manifest)).unwrap();
        let blob = std::str::from_utf8(&blob.stdout).unwrap().trim();
        let tree_input = format!("100755 blob {blob}\trun.lisp\n100644 blob {blob}\tzkcl.sexp\n");
        let mut tree = git(&objects)
            .arg("mktree")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            tree.stdin
                .take()
                .unwrap()
                .write_all(tree_input.as_bytes())
                .unwrap();
        }
        let tree = tree.wait_with_output().unwrap();
        assert!(tree.status.success());
        let commit = run(git(&objects)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .args([
                "commit-tree",
                std::str::from_utf8(&tree.stdout).unwrap().trim(),
                "-m",
                "sample",
            ]))
        .unwrap();
        let rev = String::from_utf8(commit.stdout).unwrap().trim().to_owned();
        (
            temp,
            store,
            ModuleId {
                source: SourceId::Git {
                    url: url.into(),
                    rev,
                },
                name: "test/lib".parse().unwrap(),
            },
        )
    }

    #[test]
    fn exports_fixed_commit_and_reads_locked_archive_offline_without_git_metadata() {
        let (_temp, store, id) = temp_git_source();
        let (digest, text) = read(&id, &store, None, false).unwrap();
        assert!(text.contains("test/lib"));
        // The source declaration, including Git's executable bit, determines
        // the archive even though the blob-export staging file is nonexecutable.
        fs::write(_temp.path().join("run.lisp"), &text).unwrap();
        assert_eq!(
            digest,
            store
                .publish_with_modes(_temp.path(), &[("zkcl.sexp", false), ("run.lisp", true)])
                .unwrap()
        );
        fs::remove_dir_all(store.root().join("git")).unwrap();
        assert_eq!(
            read(&id, &store, Some(digest), true).unwrap(),
            (digest, text)
        );
        assert!(!store.root().join("git").exists());
        assert!(
            read(&id, &store, None, true)
                .unwrap_err()
                .to_string()
                .contains("locked checksum")
        );
        fs::write(store.archive_path(digest), b"tampered").unwrap();
        assert!(
            read(&id, &store, Some(digest), true)
                .unwrap_err()
                .to_string()
                .contains("locked Git source")
        );
    }

    #[test]
    fn rejects_a_changed_locked_digest_and_non_commit_revisions() {
        let (_temp, store, id) = temp_git_source();
        let wrong = Digest::of_bytes(b"other archive");
        assert!(
            read(&id, &store, Some(wrong), false)
                .unwrap_err()
                .to_string()
                .contains("locked SHA-256")
        );
        for rev in [
            "main",
            "abc123",
            "--upload-pack=anything",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(validate_source("https://example.invalid/repo", rev).is_err());
        }
        let rev = "0123456789abcdef0123456789abcdef01234567";
        for url in [
            "file:///tmp/repo",
            "/tmp/repo",
            "ext::helper",
            "git@example.invalid:repo",
            "https:///tmp/repo",
            "https://example.invalid/\nrepo",
        ] {
            assert!(validate_source(url, rev).is_err(), "accepted {url:?}");
        }
    }
}
