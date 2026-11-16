use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn temp_project() -> tempfile::TempDir {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/project-delivery");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn put(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn project(root: &Path) -> PathBuf {
    put(
        root,
        "shared/zkcl.sexp",
        r#"(project "example/shared" (version "1.0.0")
          (packages "EXAMPLE.SHARED") (library (files "shared.lisp")))"#,
    );
    put(
        root,
        "shared/shared.lisp",
        r#"(defpackage #:example.shared (:use #:cl) (:export #:answer #:literal))
        (in-package #:example.shared)
        (eval-when (:compile-toplevel) (setf (get 'answer 'compile-value) 42))
        (defmacro answer () (get 'answer 'compile-value 0))
        (defun literal () '(12 34))"#,
    );
    put(
        root,
        "app/zkcl.sexp",
        r#"(project "example/app" (version "1.0.0") (packages "EXAMPLE.APP")
          (dependency "shared" (module "example/shared") (path "../shared") (version "^1"))
          (library (files "package.lisp"))
          (binary "app" (files "main.lisp") (entry "EXAMPLE.APP:MAIN")))"#,
    );
    put(
        root,
        "app/package.lisp",
        r#"(defpackage #:example.app (:use #:cl) (:export #:main))
        (in-package #:example.app)
        (defparameter *package-load* *load-truename*)"#,
    );
    put(
        root,
        "app/main.lisp",
        r#"(in-package #:example.app)
        (defparameter *main-load* *load-truename*)
        (defun main ()
          (format t "~D|~A|~A|~A|~A~%"
            (example.shared:answer)
            (first (zk:command-line-arguments))
            (equal (example.shared:literal) '(12 34))
            (not (equal *package-load* *main-load*))
            (and (probe-file *package-load*) (probe-file *main-load*) t))
          (zk:set-exit-code 7))"#,
    );
    root.join("app")
}

#[test]
fn project_binary_loads_independent_fasls_after_installing_away_from_sources() {
    let temp = temp_project();
    let root = temp.path();
    let app = project(&root.join("source"));
    let executable_name = if cfg!(windows) { "app.exe" } else { "app" };
    let output = root.join("built").join(executable_name);
    success(
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .current_dir(&app)
            .args(["build", "--bin", "app", "--backend", "direct", "-o"])
            .arg(&output)
            .output()
            .unwrap(),
    );
    assert!(output.is_file());
    let sidecar_name = format!("{executable_name}.zkcl");
    let sidecar = output.with_file_name(&sidecar_name);
    let versions = fs::read_dir(&sidecar)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(versions.len(), 1);
    assert_eq!(fs::read_dir(&versions[0]).unwrap().count(), 3);

    let installed = root.join("installed");
    fs::create_dir(&installed).unwrap();
    fs::rename(&output, installed.join(executable_name)).unwrap();
    fs::rename(&sidecar, installed.join(sidecar_name)).unwrap();
    fs::remove_dir_all(root.join("source")).unwrap();
    let output = Command::new(installed.join(executable_name))
        .current_dir(root)
        .arg("hello")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "42|hello|T|T|T\n"
    );
}

#[test]
fn project_emit_directory_contains_each_object_and_registration_module() {
    let temp = temp_project();
    let root = temp.path();
    let app = project(&root.join("source"));
    let emitted = root.join("emitted");
    success(
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .current_dir(&app)
            .args(["build", "--bin", "app", "--backend", "direct", "--emit-dir"])
            .arg(&emitted)
            .output()
            .unwrap(),
    );
    for index in 0..3 {
        assert!(emitted.join(format!("modules/{index:04}.o")).is_file());
        assert!(
            emitted
                .join(format!("modules/{index:04}.zkclfasl"))
                .is_file()
        );
        assert!(
            emitted
                .join(format!("modules/component_{index:04}.rs"))
                .is_file()
        );
    }
    let source = fs::read_to_string(emitted.join("src/main.rs")).unwrap();
    assert_eq!(source.matches("runtime.register_aot_module(").count(), 3);
    assert_eq!(source.matches("runtime.load_file(").count(), 3);
    assert!(source.contains("std::env::current_exe()"));
    assert!(source.contains("runtime.lookup_symbol(\"EXAMPLE.APP\", \"MAIN\")"));
    let script = fs::read_to_string(emitted.join("build.rs")).unwrap();
    assert!(script.contains("ZKCL_AOT_LINK_OBJECT_2"));
    assert!(emitted.join("Cargo.toml").is_file());
}

#[test]
fn native_library_rebuild_publishes_a_distinct_complete_directory() {
    let temp = temp_project();
    let root = temp.path();
    let source = root.join("source");
    let emitted = root.join("emitted");
    put(&source, "first.lisp", "(defun first-function () 42)");
    put(&source, "second.lisp", "(defun second-function () 43)");
    let build = |files: &str| {
        put(
            &source,
            "zkcl.sexp",
            &format!(r#"(project "example/library" (version "1.0.0") (library (files {files})))"#),
        );
        let output = Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .current_dir(&source)
            .env("ZKCL_CACHE_DIR", root.join("cache"))
            .args(["build", "--backend", "direct", "--emit-dir"])
            .arg(&emitted)
            .output()
            .unwrap();
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        success(output);
        assert!(path.starts_with(emitted.canonicalize().unwrap()));
        path
    };

    let first = build(r#""first.lisp" "second.lisp""#);
    let second = build(r#""first.lisp""#);
    assert_ne!(first, second);
    for path in [&first, &second] {
        for file in ["0000.o", "0000.zkclfasl", "component_0000.rs"] {
            assert!(path.join(file).is_file());
        }
    }
    for file in ["0001.o", "0001.zkclfasl", "component_0001.rs"] {
        assert!(first.join(file).is_file());
        assert!(!second.join(file).exists());
    }
}

#[test]
fn aot_cache_hits_preserve_ordered_compile_time_effects() {
    let temp = temp_project();
    let root = temp.path();
    let source = root.join("source");
    let app = project(&source);
    let dependency = source.join("shared/shared.lisp");
    let original = fs::read_to_string(&dependency).unwrap();
    fs::write(
        &dependency,
        format!(
            "{original}\n(eval-when (:compile-toplevel)
               (with-open-file (s \"compile-effects.txt\" :direction :output
                                  :if-does-not-exist :create :if-exists :append)
                 (write-line \"compile\" s)))"
        ),
    )
    .unwrap();

    let cache = root.join("cache");
    let build = |name, jobs| {
        let output = root.join(name);
        success(
            Command::new(env!("CARGO_BIN_EXE_zkcl"))
                .current_dir(&app)
                .env("ZKCL_CACHE_DIR", &cache)
                .args(["build", "--bin", "app", "--jobs", jobs, "--emit-dir"])
                .arg(&output)
                .output()
                .unwrap(),
        );
        output
    };
    let first = build("first", "1");
    let artifacts: Vec<_> = fs::read_dir(cache.join("aot"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("artifact.json").is_file())
        .collect();
    assert_eq!(artifacts.len(), 3);
    for artifact in &artifacts {
        fs::write(artifact.join("cache-hit"), b"").unwrap();
    }
    let second = build("second", "4");
    assert_eq!(
        fs::read_dir(cache.join("aot"))
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .join("artifact.json")
                .is_file())
            .count(),
        artifacts.len()
    );
    assert_eq!(
        fs::read_to_string(app.join("compile-effects.txt")).unwrap(),
        "compile\ncompile\n"
    );
    for (index, artifact) in artifacts.iter().enumerate() {
        assert!(artifact.join("cache-hit").is_file());
        for filename in [
            format!("modules/{index:04}.o"),
            format!("modules/{index:04}.zkclfasl"),
            format!("modules/component_{index:04}.rs"),
        ] {
            assert_eq!(
                fs::read(first.join(&filename)).unwrap(),
                fs::read(second.join(&filename)).unwrap(),
                "{filename} should be identical across worker counts"
            );
        }
    }
}

#[test]
fn mixed_project_installs_asdf_resources_and_custom_load_operations() {
    let temp = temp_project();
    let root = temp.path();
    let source = root.join("source");
    put(
        &source,
        "resource/zkcl.sexp",
        r#"(project "example/resource"
      (version "1.0.0") (packages "EXAMPLE.RESOURCE")
      (asdf-system "resource" "resource.asd")
      (include "library.lisp" "data.txt"))"#,
    );
    put(
        &source,
        "resource/resource.asd",
        r#"(asdf:defsystem "resource"
      :components ((:file "library"))
      :perform (asdf:load-op :after (operation system)
        (declare (ignore operation system))
        (setf (get 'cl-user::resource-loaded 'cl-user::custom-operation) 99)))"#,
    );
    put(
        &source,
        "resource/library.lisp",
        r#"(defpackage #:example.resource (:use #:cl) (:export #:resource))
      (in-package #:example.resource)
      (defun resource ()
        (with-open-file (input (asdf:system-relative-pathname "resource" "data.txt"))
          (read-line input)))"#,
    );
    put(&source, "resource/data.txt", "installed resource");
    put(
        &source,
        "app/zkcl.sexp",
        r#"(project "example/app" (version "1.0.0")
      (packages "EXAMPLE.APP")
      (dependency "resource" (module "example/resource") (path "../resource") (system "resource") (version "^1"))
      (library (files "package.lisp"))
      (binary "app" (files "main.lisp") (entry "EXAMPLE.APP:MAIN")))"#,
    );
    put(
        &source,
        "app/package.lisp",
        r#"(defpackage #:example.app (:use #:cl) (:export #:main))
      (in-package #:example.app)
      (defparameter *observed-custom-operation*
        (get 'cl-user::resource-loaded 'cl-user::custom-operation))"#,
    );
    put(
        &source,
        "app/main.lisp",
        r#"(in-package #:example.app)
      (defun main ()
        (format t "~A|~D|~A~%" (example.resource:resource)
          *observed-custom-operation* (first (zk:command-line-arguments))))"#,
    );
    let executable_name = if cfg!(windows) { "app.exe" } else { "app" };
    let built = root.join("built").join(executable_name);
    success(
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .current_dir(source.join("app"))
            .args(["build", "--bin", "app", "--backend", "direct", "-o"])
            .arg(&built)
            .output()
            .unwrap(),
    );
    let sidecar_name = format!("{executable_name}.zkcl");
    let installed = root.join("installed");
    fs::create_dir_all(&installed).unwrap();
    fs::rename(&built, installed.join(executable_name)).unwrap();
    fs::rename(
        built.with_file_name(&sidecar_name),
        installed.join(&sidecar_name),
    )
    .unwrap();
    fs::remove_dir_all(source).unwrap();
    let output = Command::new(installed.join(executable_name))
        .current_dir(root)
        .arg("hello")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.ends_with("installed resource|99|hello\n"),
        "{stdout}"
    );
}
