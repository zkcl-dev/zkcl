#![cfg(feature = "asdf")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zkcl::Runtime;

use zkcl::project::asdf;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempProject(PathBuf);

impl TempProject {
    fn new() -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let path = workspace.join(".agents/tmp/zkcl/mixed-asdf").join(format!(
            "{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, source).unwrap();
        path
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[test]
fn project_asdf_runs_real_definitions_with_selected_sources_and_operations() {
    let project = TempProject::new();
    let support = project.write(
        "support.asd",
        r#"(asdf:defsystem "support" :components ((:file "support")))"#,
    );
    project.write(
        "support.lisp",
        r#"(defpackage :project-asdf-support (:use :cl))
           (defclass project-asdf-support::custom-file (asdf:cl-source-file) ())
           (defmethod asdf:perform :after
               ((operation asdf:load-op) (component project-asdf-support::custom-file))
             (incf cl-user::*custom-loads*))"#,
    );
    let selected = project.write(
        "bundle.asd",
        r#"(asdf:defsystem "bundle" :defsystem-depends-on ("support"))
           (asdf:defsystem "bundle/client"
             :defsystem-depends-on ("support")
             :depends-on ("uiop")
             :components ((project-asdf-support::custom-file "client")))
           (asdf:defsystem "unselected-sibling" :components ((:file "client")))"#,
    );
    project.write(
        "client.lisp",
        "(incf cl-user::*client-loads*) (defun cl-user::project-asdf-answer () 42)",
    );
    let ambient = project.write(
        "ambient.asd",
        "(asdf:defsystem \"ambient\") (error \"ambient definition was evaluated\")",
    );
    let output = project.0.join("output");
    for iteration in 1..=2 {
        let runtime = Runtime::start().unwrap();
        let caller_package = runtime.current_package();
        runtime.eval("(require :asdf)").unwrap();
        runtime
            .eval(&format!(
                "(setf asdf:*central-registry* (list (pathname {})))",
                string(&format!("{}/", ambient.parent().unwrap().display()))
            ))
            .unwrap();
        runtime
            .load_str("(defvar *custom-loads* 0) (defvar *client-loads* 0)")
            .unwrap();
        asdf::configure(
            &runtime,
            &[
                asdf::AsdfSystem {
                    name: "support",
                    definition: &support,
                },
                asdf::AsdfSystem {
                    name: "bundle/client",
                    definition: &selected,
                },
            ],
            &[],
            &output,
        )
        .unwrap();
        assert_eq!(runtime.current_package(), caller_package);
        asdf::load(&runtime, "bundle/client").unwrap();
        asdf::load(&runtime, "bundle/client").unwrap();
        let values = runtime
            .eval("(values (project-asdf-answer) *custom-loads* *client-loads*)")
            .unwrap();
        assert_eq!(values[0].as_fixnum(), Some(42));
        assert_eq!(values[1].as_fixnum(), Some(1));
        assert_eq!(values[2].as_fixnum(), Some(1));
        for missing in ["ambient", "unselected-sibling", "trivial-features"] {
            let error = asdf::load(&runtime, missing).unwrap_err().to_string();
            assert!(error.contains(missing), "{iteration}: {error}");
            assert!(error.contains("not selected"), "{iteration}: {error}");
        }
        assert!(runtime.eval("(asdf:find-system \"ambient\" nil)").unwrap()[0].is_nil());
    }
    assert!(!project.0.join("client.zkclfasl").exists());
    assert!(output.is_dir());
}

#[test]
fn project_asdf_rejects_a_registered_system_from_another_source() {
    let project = TempProject::new();
    let selected = project.write("selected.asd", "(asdf:defsystem \"collision\")");
    let wrong = project.write("wrong.asd", "(asdf:defsystem \"collision\")");
    let runtime = Runtime::start().unwrap();
    runtime.eval("(require :asdf)").unwrap();
    runtime
        .eval(&format!(
            "(asdf:load-asd (pathname {}) :name \"collision\")",
            string(wrong.to_str().unwrap())
        ))
        .unwrap();
    let error = asdf::configure(
        &runtime,
        &[asdf::AsdfSystem {
            name: "collision",
            definition: &selected,
        }],
        &[],
        &project.0.join("output"),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("collision"), "{error}");
    assert!(error.contains("selected.asd"), "{error}");
    assert!(error.contains("wrong.asd"), "{error}");
}

#[test]
fn native_alias_delegates_standard_operations_to_completed_native_library() {
    let project = TempProject::new();
    let runtime = Runtime::start().unwrap();
    let consumer = project.write(
        "consumer.asd",
        r#"(asdf:defsystem "consumer"
       :depends-on ((:version "old/existing" "1.2.0"))
       :components ((:file "consumer")))"#,
    );
    project.write(
        "consumer.lisp",
        r#"(defpackage :native-alias-consumer (:use :cl))
(in-package :native-alias-consumer)
(defparameter *answer* (native-alias-test:answer))
(defparameter *symbol* 'native-alias-test:answer)"#,
    );
    asdf::configure(
        &runtime,
        &[asdf::AsdfSystem {
            name: "consumer",
            definition: &consumer,
        }],
        &[asdf::AsdfAlias {
            name: "old/existing",
            version: "1.2.3",
        }],
        &project.0.join("output"),
    )
    .unwrap();
    let premature = asdf::load(&runtime, "old/existing")
        .unwrap_err()
        .to_string();
    assert!(
        premature.contains("before its locked native"),
        "{premature}"
    );
    let native = project.write(
        "native.lisp",
        r#"(defpackage :native-alias-test (:use :cl) (:export :answer))
(in-package :native-alias-test)
(eval-when (:compile-toplevel) (setf (get 'answer 'compile-value) 42))
(defmacro answer () (get 'answer 'compile-value 0))
(defvar *loads* 0)
(incf *loads*)"#,
    );
    let compiled = runtime
        .compile_file(&native, Some(&project.0.join("native.zkclfasl")))
        .unwrap();
    runtime.load_file(&compiled).unwrap();
    asdf::mark_native_ready(&runtime, &["old/existing"]).unwrap();
    runtime
        .eval("(asdf:compile-system \"old/existing\")")
        .unwrap();
    asdf::load(&runtime, "old/existing").unwrap();
    asdf::load(&runtime, "old/existing").unwrap();
    asdf::load(&runtime, "consumer").unwrap();
    let Err(unsupported) = runtime.eval("(asdf:test-system \"old/existing\")") else {
        panic!("a native alias has no ASDF test operation");
    };
    let unsupported = unsupported.to_string();
    assert!(
        unsupported.contains("no published native alias contract"),
        "{unsupported}"
    );
    let values = runtime
        .eval(
            r#"(values native-alias-test::*loads* (native-alias-test:answer)
      (asdf:component-version (asdf:find-system "old/existing"))
      native-alias-consumer::*answer*
      (eq native-alias-consumer::*symbol* 'native-alias-test:answer))"#,
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(1));
    assert_eq!(values[1].as_fixnum(), Some(42));
    assert_eq!(runtime.repr(&values[2]).unwrap(), "\"1.2.3\"");
    assert_eq!(values[3].as_fixnum(), Some(42));
    assert_eq!(runtime.repr(&values[4]).unwrap(), "T");
}

#[test]
fn native_alias_cannot_replace_a_real_locked_system_definition() {
    let project = TempProject::new();
    let definition = project.write("collision.asd", "(asdf:defsystem \"collision\")");
    let runtime = Runtime::start().unwrap();
    let error = asdf::configure(
        &runtime,
        &[asdf::AsdfSystem {
            name: "collision",
            definition: &definition,
        }],
        &[asdf::AsdfAlias {
            name: "collision",
            version: "1.0",
        }],
        &project.0.join("output"),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("collision"), "{error}");
    assert!(error.contains("conflicts"), "{error}");
}

#[test]
fn standalone_asdf_preserves_the_explicit_definition_and_adjacent_dependencies() {
    let project = TempProject::new();
    let sources = project.0.join("sources");
    let later_directory = project.0.join("other");
    fs::create_dir_all(&sources).unwrap();
    fs::create_dir_all(&later_directory).unwrap();
    let definition = project.write(
        "sources/definitions.asd",
        r#"(incf cl-user::*definition-loads*)
           (asdf:defsystem "definitions")
           (asdf:defsystem "standalone-alias")
           (asdf:defsystem "standalone-alias/tools"
             :depends-on ("standalone-nearby" "standalone-startup")
             :components ((:file "client")))"#,
    );
    project.write(
        "sources/standalone-nearby.asd",
        r#"(asdf:defsystem "standalone-nearby" :components ((:file "dependency")))"#,
    );
    project.write(
        "sources/dependency.lisp",
        "(defparameter cl-user::*standalone-nearby* 40)",
    );
    project.write(
        "standalone-startup.asd",
        r#"(asdf:defsystem "standalone-startup" :components ((:file "startup")))"#,
    );
    project.write("startup.lisp", "(defparameter cl-user::*standalone-startup* 2)");
    project.write(
        "sources/client.lisp",
        r#"(incf cl-user::*component-loads*)
           (defun cl-user::standalone-answer ()
             (+ cl-user::*standalone-nearby* cl-user::*standalone-startup*))"#,
    );
    project.write("standalone-nearby.asd", "(error \"startup directory shadows adjacent dependency\")");
    project.write("other/standalone-nearby.asd", "(error \"adjacent dependency was displaced\")");
    project.write("other/standalone-alias.asd", "(error \"selected definition was displaced\")");
    project.write("other/standalone-other.asd", "(asdf:defsystem \"standalone-other\")");

    let runtime = Runtime::start().unwrap();
    runtime.require_module("asdf").unwrap();
    runtime
        .load_str(&format!(
            "(asdf:initialize-source-registry '(:source-registry :ignore-inherited-configuration))
             (asdf:initialize-output-translations '(:output-translations (t {}) :ignore-inherited-configuration))
             (defvar *definition-loads* 0)
             (defvar *component-loads* 0)",
            string(&format!("{}/cache/", project.0.display()))
        ))
        .unwrap();

    let system = "standalone-alias/tools";
    let selected = asdf::load_from_directory(&runtime, system, &project.0, Some(&definition))
        .unwrap()
        .unwrap();
    assert_eq!(PathBuf::from(&selected), definition.canonicalize().unwrap());
    asdf::load_from_directory(&runtime, "standalone-other", &later_directory, None).unwrap();
    asdf::load(&runtime, system).unwrap();
    assert_eq!(
        asdf::registered_system_source(&runtime, system).unwrap(),
        Some(selected)
    );
    let values = runtime
        .eval("(values (standalone-answer) *definition-loads* *component-loads*)")
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(42));
    assert_eq!(values[1].as_fixnum(), Some(1));
    assert_eq!(values[2].as_fixnum(), Some(1));
}

#[test]
fn an_explicit_asdf_file_must_define_the_requested_system() {
    let project = TempProject::new();
    project.write("prior.asd", "(asdf:defsystem \"prior\")");
    let definition = project.write(
        "replacement.asd",
        "(asdf:defsystem \"replacement\")",
    );
    let runtime = Runtime::start().unwrap();
    asdf::load_from_directory(&runtime, "prior", &project.0, None).unwrap();
    let error = asdf::load_from_directory(&runtime, "prior", &project.0, Some(&definition))
        .unwrap_err();
    assert!(error.downcast_ref::<asdf::AsdfError>().is_some());
    let error = error.to_string();
    assert!(error.contains("replacement.asd"), "{error}");
    assert!(error.contains("does not define system \"prior\""), "{error}");
}

