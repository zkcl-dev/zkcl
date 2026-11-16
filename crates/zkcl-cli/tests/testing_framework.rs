use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Project(PathBuf);

impl Project {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zkcl-test-project-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.0.join(name), text).unwrap();
    }

    fn invoke(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_zkcl"))
            .current_dir(&self.0)
            .env("XDG_CACHE_HOME", self.0.join("cache"))
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_status(result: Output, success: bool) -> String {
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.status.success(), success, "{log}");
    log
}

#[test]
fn run_loads_test_modules_for_contract_suite() {
    let p = Project::new();
    p.write(
        "contracts.lisp",
        include_str!("../../../tests/lisp/zk-pbt/test-runner.lisp"),
    );
    p.write(
        "run.lisp",
        r#"
        (assert (not (find-package :zk-test)))
        (require :zk-test)
        (assert (not (find-package :zk-pbt)))
        (require :zk-pbt)
        (load "contracts.lisp")
    "#,
    );
    assert!(assert_status(p.invoke(&["run", "run.lisp"]), true).contains("PBT contracts passed"));
}

#[test]
fn counterexample_is_replayed_in_another_process_after_seed_changes() {
    let p = Project::new();
    p.write(
        "property.lisp",
        r#"
        (require "zk-pbt")
        (zk-pbt:defproperty boundary (n)
          (:generate (zk-pbt:tuple (zk-pbt:integers :min 0 :max 100)))
          (zk-pbt:claim :boundary (< n 7)))
    "#,
    );
    p.write(
        "save.lisp",
        r#"
        (load "property.lisp")
        (let ((r (zk-pbt:run 'boundary :seed (parse-integer (first (zk:command-line-arguments))))))
          (assert (equal (getf (zk-pbt:report-data r) :reduced) '(7)))
          (zk-pbt:save-regression r #p"cases/")
          (zk-pbt:save-regression r #p"cases/"))
    "#,
    );
    std::thread::scope(|scope| {
        let concurrent = scope.spawn(|| p.invoke(&["run", "save.lisp", "--", "91"]));
        assert_status(p.invoke(&["run", "save.lisp", "--", "42"]), true);
        assert_status(concurrent.join().unwrap(), true);
    });
    assert_eq!(fs::read_dir(p.0.join("cases")).unwrap().count(), 1);
    p.write(
        "replay.lisp",
        r#"
        (load "property.lisp")
        (let* ((file (first (directory #p"cases/*.zkcase")))
               (r (zk-pbt:run 'boundary :corpus #p"cases/" :trials 0 :seed 91)))
          (assert (eq :regression (getf (zk-pbt:report-data r) :source)))
          (assert (eq :reproduced (zk-pbt:replay file)))
          (zk-pbt:defproperty boundary (n) (:examples (7)) (zk-pbt:claim :changed (< n 1)))
          (assert (eq :changed (zk-pbt:replay file)))
          (zk-pbt:defproperty boundary (n) (:examples (7)) (<= n 100))
          (assert (eq :fixed (zk-pbt:replay file))))
    "#,
    );
    assert_status(p.invoke(&["run", "replay.lisp"]), true);
    p.write("bad.zkcase", "#.42");
    p.write(
        "bad.lisp",
        "(require :zk-pbt) (assert (eq :error (zk-pbt:replay #p\"bad.zkcase\")))",
    );
    assert_status(p.invoke(&["run", "bad.lisp"]), true);
}

#[test]
fn asdf_test_command_reports_suite_outcomes() {
    let p = Project::new();
    p.write(
        "suite-demo.asd",
        r#"
        (asdf:defsystem "suite-demo"
          :depends-on ("zk-pbt") :components ((:file "cases"))
          :perform (asdf:test-op (o c) (uiop:symbol-call :zk-test :check :green)))
        (asdf:defsystem "suite-demo/red" :depends-on ("suite-demo")
          :perform (asdf:test-op (o c) (uiop:symbol-call :zk-test :check :red)))
    "#,
    );
    p.write(
        "cases.lisp",
        r#"
        (zk-test:deftest example () (zk-test:is (= 42 (+ 40 2))))
        (zk-pbt:defproperty law (n) (:generate (zk-pbt:tuple (zk-pbt:integers))) (integerp n))
        (zk-test:defsuite :green example law)
        (zk-test:deftest intentional () (zk-test:is (= 1 2)))
        (zk-test:defsuite :red intentional)
    "#,
    );
    let green = assert_status(
        p.invoke(&["test", "--seed", "42", "--trials", "4", "suite-demo"]),
        true,
    );
    assert!(
        green.contains("GREEN")
            && green.contains("LAW")
            && green.contains("seed=42")
            && green.contains(":RANDOM 4"),
        "{green}"
    );
    let red = assert_status(p.invoke(&["test", "suite-demo/red"]), false);
    assert!(
        red.contains("INTENTIONAL") && red.contains("FALSIFIED"),
        "{red}"
    );
    assert_status(p.invoke(&["test", "--seed", "-1", "suite-demo"]), false);
}

#[test]
fn documentation_tests_use_asdf_components_and_report_original_positions() {
    let p = Project::new();
    fs::create_dir(p.0.join("src")).unwrap();
    p.write(
        "greetings.asd",
        r#"(asdf:defsystem "greetings"
          :around-compile "zk:call-with-raw-strings"
          :components ((:module "src" :components ((:file "greetings"))))
          :perform (asdf:test-op (op component) (error "unexpected test-op")))"#,
    );
    p.write(
        "src/greetings.lisp",
        r#"(defpackage :greetings (:use :cl) (:export :greet))
(in-package :greetings)
(defparameter *source*
  #>(greet #\A)
)
;;! Greet `name`.
;;! ```lisp
;;! (error "display only") )
;;! ```
;;! ```lisp test
;;! (assert (string= (greet #\A) "Hi, A"))
;;! (in-package :cl-user)
;;! (zk:in-readtable :standard)
;;! ```
#+zkcl (defun greet (name) (format nil "Hi, ~A" name))
;;! ```lisp test
;;! (assert (string= (greet #\B) "Hi, B"))
;;! (let ((source
;;!         #>(greet #\A)
;;!       ))
;;!   (assert (string= source *source*)))
;;! ```
(defun sample () *source*)
"#,
    );
    p.write(
        "README.md",
        "# Greetings\n\n```lisp test\n(assert (eq *package* (find-package :cl-user)))\n(assert (string= (greetings:greet \"Ada\") \"Hi, Ada\"))\n```\n",
    );
    let passed = assert_status(p.invoke(&["test", "--doc", "greetings"]), true);
    assert!(passed.contains("3 passed, 0 failed"), "{passed}");

    p.write(
        "README.md",
        "# Greetings\r\n\r\n```lisp test\r\n(values \"😀\")\r\n(assert nil)\r\n```\r\n",
    );
    assert_status(p.invoke(&["check", "README.md"]), true);
    let failed = assert_status(p.invoke(&["test", "--doc", "greetings"]), false);
    assert!(failed.contains("README.md:5:"), "{failed}");
    assert!(failed.contains("2 passed, 1 failed"), "{failed}");
}

#[test]
fn check_reports_documentation_syntax_and_keeps_reader_macros_inert() {
    let p = Project::new();
    p.write(
        "README.md",
        "# Heading\r\n\r\n```lisp test\r\n#. (error \"inert\")\r\n(\"😀\"))\r\n```\r\n",
    );
    let output = p.invoke(&["check", "--format", "json", "README.md"]);
    assert_eq!(output.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = result["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{result}");
    let error = &diagnostics[0];
    assert_eq!(error["rule"], "doc/example");
    assert_eq!(error["line"], 5);
    assert_eq!(error["column"], 6);
    assert_eq!(error["message"], "unexpected closing parenthesis");
}

#[test]
fn check_applies_lint_rules_to_source_and_readme_examples() {
    let p = Project::new();
    p.write(
        "greetings.lisp",
        "(defpackage :greetings (:use :cl) (:export :greet))\r\n\
         (in-package :greetings)\r\n\
         ;;! ```lisp test\r\n\
         ;;! (list \"雪😀\" (greet))\r\n\
         ;;! ```\r\n\
         (defun greet (name) name)\r\n",
    );
    p.write(
        "README.md",
        "# Greetings\r\n\r\n```lisp test\r\n(assert (string= (greetings:greet) \"Hi, A\"))\r\n```\r\n",
    );

    let check = |flag| {
        p.invoke(&[
            "check",
            "--format",
            "json",
            flag,
            "arity-mismatch",
            "greetings.lisp",
            "README.md",
        ])
    };
    let output = check("--deny");
    assert_eq!(output.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = result["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 2, "{result}");
    for (diagnostic, (file, column)) in diagnostics
        .iter()
        .zip([("greetings.lisp", 16), ("README.md", 18)])
    {
        assert_eq!(diagnostic["file"], file);
        assert_eq!(diagnostic["rule"], "correctness/arity-mismatch");
        assert_eq!(diagnostic["severity"], "error");
        assert_eq!(diagnostic["line"], 4);
        assert_eq!(diagnostic["column"], column);
        assert_eq!(diagnostic["fixable"], false);
    }

    let output = check("--disable");
    assert!(output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["diagnostics"], serde_json::json!([]));
}

#[test]
fn cli_test_flags_filter_and_budget_options() {
    let p = Project::new();
    p.write(
        "flags-demo.asd",
        r#"
        (asdf:defsystem "flags-demo"
          :depends-on ("zk-pbt")
          :components ((:file "suite"))
          :perform (asdf:test-op (o c) (uiop:symbol-call :zk-test :check :all)))
    "#,
    );
    p.write(
        "suite.lisp",
        r#"
        (zk-test:deftest passing-a () (zk-test:is= 4 (+ 2 2)))
        (zk-pbt:defproperty small-ints ((n (zk-pbt:integers :min 0 :max 100)))
          (<= 0 n 100))
        (zk-pbt:defproperty fail-large ((n (zk-pbt:integers :min 0 :max 100)))
          (< n 10))
        (zk-test:defsuite :all passing-a small-ints fail-large)
    "#,
    );
    let filtered = assert_status(
        p.invoke(&[
            "test",
            "--seed",
            "42",
            "--trials",
            "10",
            "--max-size",
            "20",
            "--max-discards",
            "50",
            "--max-enumerated",
            "16",
            "--filter",
            "PASSING-A",
            "flags-demo",
        ]),
        true,
    );
    assert!(filtered.contains("PASSING-A"), "{filtered}");
    assert!(!filtered.contains("FAIL-LARGE"), "{filtered}");

    let pbt_filtered = assert_status(
        p.invoke(&[
            "test",
            "--seed",
            "42",
            "--trials",
            "12",
            "--filter",
            "SMALL-INTS",
            "flags-demo",
        ]),
        true,
    );
    assert!(pbt_filtered.contains("SMALL-INTS"), "{pbt_filtered}");
    assert!(pbt_filtered.contains(":RANDOM 12"), "{pbt_filtered}");

    let zero_shrinks = assert_status(
        p.invoke(&[
            "test",
            "--seed",
            "42",
            "--trials",
            "40",
            "--max-shrinks",
            "0",
            "--filter",
            "FAIL-LARGE",
            "flags-demo",
        ]),
        false,
    );
    assert!(
        zero_shrinks.contains("FAIL-LARGE") && zero_shrinks.contains("shrinks: 0"),
        "{zero_shrinks}"
    );
    assert_status(
        p.invoke(&["test", "--max-shrinks", "-1", "flags-demo"]),
        false,
    );
}

#[test]
fn cli_test_corpus_flag_replays_saved_regressions() {
    let p = Project::new();
    p.write(
        "corpus-demo.asd",
        r#"
        (asdf:defsystem "corpus-demo"
          :depends-on ("zk-pbt")
          :components ((:file "prop"))
          :perform (asdf:test-op (o c)
                     (uiop:symbol-call :zk-test :check
                                       (uiop:find-symbol* :prop :cl-user))))
    "#,
    );
    p.write(
        "prop.lisp",
        r#"
        (zk-pbt:defproperty prop ((n (zk-pbt:integers :min 0 :max 100)))
          (zk-pbt:claim :limit (< n 7)))
    "#,
    );
    p.write(
        "save.lisp",
        r#"
        (require :zk-pbt)
        (load "prop.lisp")
        (let ((r (zk-pbt:run 'prop :seed 42)))
          (assert (equal '(7) (getf (zk-pbt:report-data r) :reduced)))
          (zk-pbt:save-regression r "regressions"))
    "#,
    );
    assert_status(p.invoke(&["run", "save.lisp"]), true);

    let replayed = assert_status(
        p.invoke(&[
            "test",
            "--corpus",
            "regressions",
            "--trials",
            "0",
            "corpus-demo",
        ]),
        false,
    );
    assert!(
        replayed.contains("FALSIFIED PROP") && replayed.contains(":REGRESSION 1"),
        "{replayed}"
    );

    let _ = fs::remove_dir_all(p.0.join("cache"));
    p.write(
        "prop.lisp",
        r#"
        (zk-pbt:defproperty prop ((n (zk-pbt:integers :min 0 :max 100)))
          (<= n 100))
    "#,
    );
    let fixed = assert_status(
        p.invoke(&[
            "test",
            "--corpus",
            "regressions",
            "--trials",
            "0",
            "corpus-demo",
        ]),
        true,
    );
    assert!(
        fixed.contains("PASSED PROP") && fixed.contains(":REGRESSION 1"),
        "{fixed}"
    );
}

#[test]
fn cli_runs_zkcl_stdlib_tests_suite() {
    let cache = Project::new();
    let lisp_tests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/lisp");
    let output = Command::new(env!("CARGO_BIN_EXE_zkcl"))
        .current_dir(&lisp_tests_dir)
        .env("XDG_CACHE_HOME", cache.0.join("cache"))
        .args([
            "test",
            "--seed",
            "42",
            "--trials",
            "20",
            "zkcl-stdlib-tests",
        ])
        .output()
        .unwrap();
    let log = assert_status(output, true);
    assert!(log.contains("PASSED ZKCL-STDLIB-TESTS:STDLIB-ALL"), "{log}");
    assert!(log.contains("summary: 33/33 passed"), "{log}");
}

#[test]
fn cli_parallel_jobs_and_structured_formats_preserve_determinism() {
    let p = Project::new();
    p.write(
        "parallel-demo.asd",
        r#"
        (asdf:defsystem "parallel-demo"
          :depends-on ("zk-pbt")
          :components ((:file "suite"))
          :perform (asdf:test-op (o c) (uiop:symbol-call :zk-test :check :all)))
        (asdf:defsystem "parallel-demo/red"
          :depends-on ("parallel-demo")
          :perform (asdf:test-op (o c) (uiop:symbol-call :zk-test :check :red)))
    "#,
    );
    p.write(
        "suite.lisp",
        r#"
        (zk-test:deftest unit-ok () (zk-test:is= 7 (+ 3 4)))
        (zk-pbt:defproperty prop-a ((n (zk-pbt:integers :min 0 :max 50))) (<= 0 n 50))
        (zk-pbt:defproperty prop-b ((n (zk-pbt:such-that #'evenp (zk-pbt:integers :min 0 :max 20) :attempts 1)))
          (evenp n))
        (zk-test:defsuite :all unit-ok prop-a prop-b)
        (zk-pbt:defproperty prop-fail ((n (zk-pbt:integers :min 0 :max 100))) (< n 7))
        (zk-test:defsuite :red unit-ok prop-fail)
    "#,
    );

    let json_green = assert_status(
        p.invoke(&[
            "test",
            "--jobs",
            "2",
            "--format",
            "json",
            "--seed",
            "42",
            "--trials",
            "10",
            "--max-discard-ratio",
            "5",
            "parallel-demo",
        ]),
        true,
    );
    let parsed: serde_json::Value = serde_json::from_str(&json_green).unwrap();
    assert_eq!(parsed["status"], "passed", "{parsed}");
    assert_eq!(parsed["results"].as_array().unwrap().len(), 3, "{parsed}");

    let junit_red = assert_status(
        p.invoke(&[
            "test",
            "-j",
            "2",
            "--format",
            "junit",
            "--seed",
            "42",
            "--trials",
            "25",
            "parallel-demo/red",
        ]),
        false,
    );
    assert!(
        junit_red.contains("<testsuite ")
            && junit_red.contains("failures=\"1\"")
            && junit_red.contains("<failure ")
            && junit_red.contains("input (7)"),
        "{junit_red}"
    );

    let serial_json = assert_status(
        p.invoke(&[
            "test",
            "--format",
            "json",
            "--seed",
            "42",
            "--trials",
            "25",
            "parallel-demo/red",
        ]),
        false,
    );
    let serial_val: serde_json::Value = serde_json::from_str(&serial_json).unwrap();
    assert_eq!(serial_val["status"], "falsified", "{serial_val}");
    assert_eq!(serial_val["results"][1]["reduced"], "(7)", "{serial_val}");

    // Verify :in-order-to ((asdf:test-op (asdf:test-op "parallel-demo/tests"))) works with --jobs.
    p.write(
        "umbrella.asd",
        r#"
        (asdf:defsystem "umbrella"
          :in-order-to ((asdf:test-op (asdf:test-op "umbrella/tests"))))
        (asdf:defsystem "umbrella/tests"
          :depends-on ("zk-pbt")
          :components ((:file "umbrella-tests"))
          :perform (asdf:test-op (o c) (uiop:symbol-call :zk-test :check :umbrella-suite)))
    "#,
    );
    p.write(
        "umbrella-tests.lisp",
        r#"
        (zk-test:deftest umb-a () (zk-test:is= 2 (+ 1 1)))
        (zk-pbt:defproperty umb-b ((x (zk-pbt:integers :min 0 :max 20))) (<= 0 x 20))
        (zk-test:defsuite :umbrella-suite umb-a umb-b)
    "#,
    );
    let umb_out = assert_status(
        p.invoke(&["test", "--jobs", "2", "--trials", "5", "umbrella"]),
        true,
    );
    assert!(umb_out.contains("PASSED :UMBRELLA-SUITE"), "{umb_out}");
}
