use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/workspace-integration");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn put(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .current_dir(root)
        .env("ZKCL_CACHE_DIR", root.join("cache"))
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

fn workspace(root: &Path) {
    put(root, "zkcl.sexp", r#"(workspace (members "lib" "app"))"#);
    put(
        root,
        "lib/zkcl.sexp",
        r#"(project "example/lib" (version "1.0.0")
             (library (files "lib.lisp")))"#,
    );
    put(
        root,
        "lib/lib.lisp",
        r#"(defpackage #:example.lib (:use #:cl) (:export #:answer))
           (in-package #:example.lib)
           (defun answer () 42)"#,
    );
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "example/app" (version "1.0.0")
             (dependency "lib" (module "example/lib") (version "^1"))
             (binary "app" (files "main.lisp") (entry "CL-USER::MAIN")))"#,
    );
    put(
        root,
        "app/main.lisp",
        r#"(defun main () (format t "~D~%" (example.lib:answer)))"#,
    );
}

#[test]
fn new_creates_a_runnable_project_and_preserves_existing_directories() {
    let temp = fixture();
    let root = temp.path();
    success(run(root, &["new", "demo", "--name", "example/demo"]));
    assert_eq!(
        success(run(root, &["-C", "demo", "run"])),
        "Hello from example/demo.\n"
    );
    let source = fs::read_to_string(root.join("demo/src/main.lisp")).unwrap();
    assert!(
        !run(root, &["new", "demo", "--name", "example/other"])
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(root.join("demo/src/main.lisp")).unwrap(),
        source
    );
}

#[test]
fn root_commands_share_locks_select_members_and_follow_directory() {
    let temp = fixture();
    let root = temp.path();
    workspace(root);
    success(run(root, &["lock"]));
    assert!(root.join("zkcl.lock").is_file());
    assert!(!root.join("lib/zkcl.lock").exists());
    assert!(!root.join("app/zkcl.lock").exists());
    let lock = fs::read_to_string(root.join("zkcl.lock")).unwrap();
    let parsed = zkcl_project::lockfile::parse_lockfile(&lock).unwrap();
    assert_eq!(parsed.graphs().count(), 2);
    assert!(lock.contains(r#""lib""#), "{lock}");
    assert_eq!(
        success(run(root, &["run", "--member", "example/app", "--locked"])),
        "42\n"
    );
    assert_eq!(
        success(run(root, &["-C", "app", "run", "--locked"])),
        "42\n"
    );
    success(run(root, &["check"]));
    let workspace_info: serde_json::Value =
        serde_json::from_str(&success(run(root, &["info", "--json"]))).unwrap();
    assert_eq!(workspace_info.as_array().unwrap().len(), 2);
    let trees: serde_json::Value =
        serde_json::from_str(&success(run(root, &["tree", "--json", "--locked"]))).unwrap();
    assert_eq!(trees.as_array().unwrap().len(), 2);
    assert!(
        trees
            .as_array()
            .unwrap()
            .iter()
            .all(|tree| tree["modules"].is_array())
    );
    let metadata = success(run(root, &["info", "--member", "app", "--json"]));
    let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
    assert_eq!(metadata["module"], "example/app");
    assert_eq!(
        PathBuf::from(metadata["lockfile"].as_str().unwrap()),
        root.join("zkcl.lock").canonicalize().unwrap()
    );
    let ambiguous = run(root, &["run"]);
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("--member"));
}

#[test]
fn host_build_dependencies_generate_cached_sources_without_entering_program_state() {
    let temp = fixture();
    let root = temp.path();
    put(
        root,
        "zkcl.sexp",
        r#"(workspace (members "generator" "app"))"#,
    );
    put(
        root,
        "generator/zkcl.sexp",
        r#"(project "example/generator" (version "1.0.0")
             (library (files "generator.lisp")))"#,
    );
    put(
        root,
        "generator/generator.lisp",
        r#"(defpackage #:example.generator (:use #:cl) (:export #:generate))
           (in-package #:example.generator)
           (defun generate (input output)
             (with-open-file (source input)
               (with-open-file (destination output :direction :output :if-exists :supersede)
                 (format destination "(defun generated-value () ~D)~%" (read source)))))"#,
    );
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "example/app" (version "1.0.0")
             (dependency "generator" (module "example/generator") (version "^1"))
             (build "generate.lisp"
               (input "number.txt")
               (dependency "generator")
               (output "generated.lisp"))
             (binary "app" (files "generated.lisp" "main.lisp") (entry "CL-USER::MAIN")))"#,
    );
    put(root, "app/number.txt", "42\n");
    put(
        root,
        "app/generate.lisp",
        r#"(with-open-file (calls "calls.txt" :direction :output
                                :if-exists :append :if-does-not-exist :create)
             (write-line "ran" calls))
           (example.generator:generate
             "number.txt" (merge-pathnames "generated.lisp" (zk:getenv "ZKCL_OUT_DIR")))"#,
    );
    put(
        root,
        "app/main.lisp",
        r#"(defun main ()
             (assert (null (find-package "EXAMPLE.GENERATOR")))
             (format t "~D~%" (generated-value)))"#,
    );
    success(run(root, &["lock", "--member", "app"]));
    let lock = zkcl_project::lockfile::parse_lockfile(
        &fs::read_to_string(root.join("zkcl.lock")).unwrap(),
    )
    .unwrap();
    assert_eq!(lock.graphs().count(), 2);
    let args = ["run", "--member", "app", "--locked"];
    assert_eq!(success(run(root, &args)), "42\n");
    assert_eq!(success(run(root, &args)), "42\n");
    assert_eq!(
        fs::read_to_string(root.join("app/calls.txt")).unwrap(),
        "ran\n"
    );
    put(root, "app/number.txt", "43\n");
    assert_eq!(success(run(root, &args)), "43\n");
    assert_eq!(
        fs::read_to_string(root.join("app/calls.txt")).unwrap(),
        "ran\nran\n"
    );
    assert!(!root.join("app/generated.lisp").exists());
}

#[test]
fn clean_removes_only_generated_project_directories() {
    let temp = fixture();
    let root = temp.path();
    workspace(root);
    let target = target_lexicon::Triple::host().to_string();
    put(
        root,
        &format!("lib/target/zkcl/{target}/debug/value"),
        "generated",
    );
    put(
        root,
        &format!("app/target/zkcl/{target}/debug/value"),
        "generated",
    );
    put(root, "app/target/zkcl/session-pending/value", "session");
    put(root, "app/target/zkcl/manifest.lock", "");
    put(root, "app/target/keep", "keep");
    success(run(root, &["clean"]));
    assert!(!root.join("lib/target/zkcl").join(&target).exists());
    assert!(!root.join("app/target/zkcl").join(&target).exists());
    assert!(root.join("app/target/zkcl/manifest.lock").is_file());
    assert!(root.join("app/target/zkcl/session-pending/value").is_file());
    assert_eq!(
        fs::read_to_string(root.join("app/target/keep")).unwrap(),
        "keep"
    );
    assert!(root.join("app/main.lisp").exists());
}
