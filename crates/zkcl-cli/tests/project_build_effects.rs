use std::fs;
use std::path::Path;
use std::process::Command;

fn put(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn run(root: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .current_dir(root)
        .env("ZKCL_CACHE_DIR", root.join("cache"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn generator(root: &Path, answer: u32) {
    put(
        root,
        "generator/generator.lisp",
        &format!(
            r#"
(defpackage #:build.effects (:use #:cl) (:export #:answer))
(in-package #:build.effects)
(eval-when (:compile-toplevel) (setf (get 'answer 'value) {answer}))
(defmacro answer () (or (get 'answer 'value) (error "Missing compile-only effect")))
"#
        ),
    );
}

#[test]
fn build_worker_replays_compile_effects_and_source_changes_invalidate_task_cache() {
    let scratch =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/build-tasks");
    fs::create_dir_all(&scratch).unwrap();
    let temporary = tempfile::tempdir_in(scratch).unwrap();
    let root = temporary.path();
    put(
        root,
        "zkcl.sexp",
        r#"(workspace (members "app" "generator"))"#,
    );
    put(
        root,
        "generator/zkcl.sexp",
        r#"
(project "effects/generator" (version "1.0.0")
 (library (files "generator.lisp")))"#,
    );
    generator(root, 42);
    put(
        root,
        "app/zkcl.sexp",
        r#"
(project "effects/app" (version "1.0.0")
 (dependency "generator" (module "effects/generator") (version "^1"))
 (build "build.lisp" (input) (dependency "generator") (output "value.lisp"))
 (binary "app" (files "value.lisp" "main.lisp") (entry "CL-USER::MAIN")))"#,
    );
    put(
        root,
        "app/build.lisp",
        r#"
(with-open-file (calls "calls.txt" :direction :output :if-exists :append
                                   :if-does-not-exist :create)
 (write-line "ran" calls))
(with-open-file (output (merge-pathnames "value.lisp" (zk:getenv "ZKCL_OUT_DIR"))
                       :direction :output :if-exists :supersede)
 (format output "(defun generated-value () ~D)~%" (build.effects:answer)))"#,
    );
    put(
        root,
        "app/main.lisp",
        r#"
(defun main ()
 (assert (null (find-package "BUILD.EFFECTS")))
 (format t "~D~%" (generated-value)))"#,
    );
    run(root, &["lock", "--member", "app"]);
    let args = ["run", "--member", "app", "--locked"];
    assert_eq!(run(root, &args), "42\n");
    assert_eq!(run(root, &args), "42\n");
    assert_eq!(
        fs::read_to_string(root.join("app/calls.txt")).unwrap(),
        "ran\n"
    );
    let task_cache = root.join("cache/build-tasks");
    let entries = || {
        fs::read_dir(&task_cache).unwrap().filter_map(|entry| {
            let entry = entry.unwrap();
            entry.file_type().unwrap().is_dir().then(|| entry.path())
        })
    };
    let original_entry = entries().next().unwrap();
    // Only compile-time state changes. The macro definition and saved load forms
    // are unchanged, so a key based only on FASL bytes would reuse stale output.
    generator(root, 43);
    assert_eq!(run(root, &args), "43\n");
    assert_eq!(run(root, &args), "43\n");
    assert_eq!(
        fs::read_to_string(root.join("app/calls.txt")).unwrap(),
        "ran\nran\n"
    );

    // A complete entry under the wrong key has correct output digests and the
    // same output filenames, but belongs to the generator that returned 42.
    let changed_entry = entries().find(|entry| *entry != original_entry).unwrap();
    fs::remove_dir_all(&changed_entry).unwrap();
    fs::rename(original_entry, changed_entry).unwrap();
    assert_eq!(run(root, &args), "43\n");
    assert_eq!(run(root, &args), "43\n");
    assert_eq!(
        fs::read_to_string(root.join("app/calls.txt")).unwrap(),
        "ran\nran\nran\n"
    );
}
