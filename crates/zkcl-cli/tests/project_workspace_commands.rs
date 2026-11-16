use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn fixture() -> tempfile::TempDir {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/work-commands");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn put(root: &Path, name: &str, source: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}
fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn initializes_adds_and_lists_members_preserving_workspace_comments() {
    let temp = fixture();
    let root = temp.path();
    put(
        root,
        "core/zkcl.sexp",
        r#"(project "test/core" (version "2.3.4"))"#,
    );
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "test/app" (version "1.0.0"))"#,
    );
    success(run(root, &["work", "init", "core"]));
    put(
        root,
        "zkcl.sexp",
        "; preserved\n(workspace (members \"core\" ; core module\n))\n",
    );
    success(run(root, &["work", "use", "app"]));
    let source = fs::read_to_string(root.join("zkcl.sexp")).unwrap();
    assert!(source.starts_with("; preserved\n"));
    assert!(source.contains("; core module\n"));
    let list = success(run(&root.join("app"), &["work", "list", "--json"]));
    let list: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(list[0]["module"], "test/core");
    assert_eq!(list[1]["path"], "app");
    success(run(root, &["lock"]));
    success(run(root, &["work", "remove", "test/app"]));
    assert!(root.join("app/zkcl.sexp").is_file());
    let lock = fs::read_to_string(root.join("zkcl.lock")).unwrap();
    let lock = zkcl_project::lockfile::parse_lockfile(&lock).unwrap();
    assert_eq!(lock.graphs().count(), 1);
    assert_eq!(lock.graphs().next().unwrap().project.as_str(), "test/core");
    let source = fs::read_to_string(root.join("zkcl.sexp")).unwrap();
    assert!(source.contains("; core module\n"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&success(run(root, &["work", "list", "--json"])))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn sync_changes_only_implicit_member_version_strings() {
    let temp = fixture();
    let root = temp.path();
    put(root, "zkcl.sexp", r#"(workspace (members "core" "app"))"#);
    put(
        root,
        "core/zkcl.sexp",
        r#"(project "test/core" (version "2.3.4"))"#,
    );
    let source = r#"; 中文 is preserved
(project "test/app" (version "1.0.0")
  (dependency "core" (module "test/core") (version ; comment
    "^1"))
  (dependency "external" (module "test/core") (version "^1") (registry "private"))
  (dependency "explicit" (module "test/core") (version "^1") (registry "zkcl"))
  (dependency "path" (module "test/core") (version "^1") (path "../core"))
  (dependency "remote" (module "test/remote") (version "^1")))
"#;
    put(root, "app/zkcl.sexp", source);
    success(run(&root.join("app"), &["work", "sync"]));
    assert_eq!(
        fs::read_to_string(root.join("app/zkcl.sexp")).unwrap(),
        source.replacen("\"^1\"", "\"2.3.4\"", 1)
    );
}

#[test]
fn invalid_init_does_not_create_manifest_or_overwrite_projects() {
    let temp = fixture();
    let root = temp.path();
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "test/app" (version "1.0.0"))"#,
    );
    assert!(
        !run(root, &["work", "init", "app", "missing"])
            .status
            .success()
    );
    assert!(!root.join("zkcl.sexp").exists());
    let before = fs::read_to_string(root.join("app/zkcl.sexp")).unwrap();
    assert!(
        !run(&root.join("app"), &["work", "init", "."])
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(root.join("app/zkcl.sexp")).unwrap(),
        before
    );
}

#[test]
fn concurrent_use_commands_wait_for_lock_and_preserve_both_additions() {
    let temp = fixture();
    let root = temp.path();
    for member in ["core", "first", "second"] {
        put(
            root,
            &format!("{member}/zkcl.sexp"),
            &format!("(project \"test/{member}\" (version \"1.0.0\"))"),
        );
    }
    success(run(root, &["work", "init", "core"]));
    fs::create_dir_all(root.join("target/zkcl")).unwrap();
    let guard = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("target/zkcl/manifest.lock"))
        .unwrap();
    guard.lock().unwrap();
    let spawn = |member| {
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .current_dir(root)
            .args(["work", "use", member])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut first = spawn("first");
    let mut second = spawn("second");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        first.try_wait().unwrap().is_none(),
        "first command bypassed the manifest lock"
    );
    assert!(
        second.try_wait().unwrap().is_none(),
        "second command bypassed the manifest lock"
    );
    drop(guard);
    success(first.wait_with_output().unwrap());
    success(second.wait_with_output().unwrap());
    let list: serde_json::Value =
        serde_json::from_str(&success(run(root, &["work", "list", "--json"]))).unwrap();
    let mut modules: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["module"].as_str().unwrap())
        .collect();
    modules.sort();
    assert_eq!(modules, ["test/core", "test/first", "test/second"]);
}
