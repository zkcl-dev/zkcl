use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio, id};

fn task_tmp() -> std::path::PathBuf {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/diagnostics-rework/commands");
    fs::create_dir_all(&root).unwrap();
    root
}

fn source_snapshot<'a>(
    report: &'a serde_json::Value,
    id: &serde_json::Value,
) -> &'a serde_json::Value {
    report["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| &source["snapshot_id"] == id)
        .unwrap()
}

fn zkcl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
}

fn check_stdin(source: &str) -> Output {
    check_stdin_with_args(source, [])
}

fn check_stdin_with_args<'a>(source: &str, args: impl IntoIterator<Item = &'a str>) -> Output {
    let mut child = zkcl()
        .arg("check")
        .args(args)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn check_stdin_skips_read_time_eval() {
    let valid = check_stdin("#.(error \"must not run\")\r\n(list \"雪😀\")");
    assert!(valid.status.success());
    assert!(valid.stdout.is_empty());
    assert!(valid.stderr.is_empty());
}

#[test]
fn check_json_includes_secondary_locations_and_original_byte_offsets() {
    let source = "(tagbody |雪|\n  |雪|)";
    let output = check_stdin_with_args(source, ["--format", "json"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = report["diagnostics"].as_array().unwrap();
    let [diagnostic] = diagnostics.as_slice() else {
        panic!("{diagnostics:?}")
    };
    assert_eq!(diagnostic["code"], "correctness/duplicate-tagbody-tag");
    assert_eq!(
        diagnostic["labels"][0]["byte_range"]["start"],
        source.rfind("|雪|").unwrap()
    );
    assert_eq!(diagnostic["labels"][0]["range"]["start"]["line"], 1);
    assert_eq!(diagnostic["labels"][1]["range"]["start"]["line"], 0);
    assert_eq!(diagnostic["labels"][1]["range"]["start"]["column"], 9);
}

#[test]
fn check_reads_raw_strings_only_when_selected() {
    let source = "(list\n  #>unbalanced quoted Lisp: ( \"\n  #>  )\n)\n";
    let with_header = format!("(zk:in-readtable :raw-strings)\n{source}");
    for output in [
        check_stdin(&with_header),
        check_stdin_with_args(source, ["--readtable", "raw-strings"]),
    ] {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(check_stdin(source).status.code(), Some(1));
}

#[test]
fn check_stdin_reports_parenthesis_errors_at_character_columns() {
    let invalid = check_stdin("\"雪😀\")\r\n(");
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    let diagnostics = String::from_utf8(invalid.stderr).unwrap();
    for expected in [
        "error[syntax/read]",
        "unexpected closing parenthesis",
        "missing closing parenthesis",
        "雪😀",
        "<stdin>",
        "2 errors",
    ] {
        assert!(diagnostics.contains(expected), "{diagnostics}");
    }
    assert!(!diagnostics.contains('\u{1b}'));
}

#[test]
fn check_keeps_going_after_an_input_error() {
    let directory = task_tmp().join(format!("zkcl-cli-check-{}", id()));
    fs::create_dir(&directory).unwrap();
    let malformed = directory.join("bad source.lisp");
    let missing = directory.join("missing.lisp");
    let valid = directory.join("valid.lisp");
    fs::write(&malformed, ")").unwrap();
    fs::write(&valid, "(quote checked)").unwrap();
    let valid_output = zkcl().arg("check").arg(&valid).output();
    let mut paths = vec![&valid; 8];
    paths.extend([&missing, &malformed]);
    let batch_output = zkcl().arg("check").args(paths).output();
    fs::remove_dir_all(&directory).unwrap();

    let valid_output = valid_output.unwrap();
    assert!(valid_output.status.success());
    assert!(valid_output.stderr.is_empty());

    let batch_output = batch_output.unwrap();
    assert_eq!(batch_output.status.code(), Some(2));
    assert!(batch_output.stdout.is_empty());
    let errors = String::from_utf8(batch_output.stderr).unwrap();
    assert!(errors.contains("cannot read"), "{errors}");
    assert!(errors.contains("missing.lisp"));
    assert!(errors.contains("bad source.lisp"));
    assert!(errors.contains("unexpected closing parenthesis"));
    assert!(errors.contains("2 errors"));
}

#[test]
fn parallel_analysis_preserves_json_reports_and_fixes() {
    let directory = tempfile::tempdir_in(task_tmp()).unwrap();
    let library = directory.path().join("library.lisp");
    let caller = directory.path().join("caller.lisp");
    let prefix = "; Large enough to check the inputs on separate workers\n".repeat(240);
    fs::write(
        &library,
        format!("{prefix}(in-package :cl-user) (defun combine (a b) (list a b))\n"),
    )
    .unwrap();
    fs::write(
        &caller,
        format!("{prefix}(in-package :cl-user) (defun caller () (progn (combine :one)))\n"),
    )
    .unwrap();
    let analyze = |jobs| {
        zkcl()
            .args(["lint", "--format", "json", "--fix-dry-run", "-j", jobs])
            .arg(&caller)
            .arg(&library)
            .output()
            .unwrap()
    };
    let serial = analyze("1");
    let parallel = analyze("4");
    assert_eq!(serial.status, parallel.status);
    assert_eq!(serial.stderr, parallel.stderr);
    assert_eq!(serial.stdout, parallel.stdout);
    let report: serde_json::Value = serde_json::from_slice(&parallel.stdout).unwrap();
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| {
                diagnostic["code"] == "correctness/arity-mismatch"
                    && diagnostic["labels"]
                        .as_array()
                        .is_some_and(|labels| labels.len() > 1)
            })
    );
    assert!(
        report["fix_diff"]
            .as_str()
            .is_some_and(|diff| !diff.is_empty())
    );
}

#[test]
fn source_analysis_requires_an_input_path_outside_a_project() {
    for command in ["check", "fmt", "lint"] {
        let output = zkcl().arg(command).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{command}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("source path"));
    }
}

#[test]
fn fmt_stdin_writes_formatted_source_to_stdout() {
    let mut child = zkcl()
        .args(["fmt", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(UNFORMATTED_SAMPLE.as_bytes())
        .unwrap();
    let formatted_out = child.wait_with_output().unwrap();
    assert!(formatted_out.status.success());
    assert_eq!(
        String::from_utf8(formatted_out.stdout).unwrap(),
        FORMATTED_SAMPLE
    );
}

const UNFORMATTED_SAMPLE: &str = "(  defun   sample ( x   y )\n   ( +  x   y ) )\n";
const FORMATTED_SAMPLE: &str = "(defun sample (x y)\n  (+ x y))\n";

#[test]
fn fmt_check_accepts_the_rewritten_file() {
    let directory = task_tmp().join(format!("zkcl-cli-fmt-{}", id()));
    fs::create_dir(&directory).unwrap();
    let file = directory.join("sample.lisp");
    fs::write(&file, UNFORMATTED_SAMPLE).unwrap();

    let check_before = zkcl().args(["fmt", "--check"]).arg(&file).output().unwrap();
    assert_eq!(check_before.status.code(), Some(1));

    let write_fmt = zkcl().arg("fmt").arg(&file).output().unwrap();
    assert!(write_fmt.status.success());
    assert_eq!(fs::read_to_string(&file).unwrap(), FORMATTED_SAMPLE);

    let check_after = zkcl().args(["fmt", "--check"]).arg(&file).output().unwrap();
    assert!(check_after.status.success());
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn check_prints_semantic_warnings_to_stderr() {
    let output = check_stdin("(defun demo (used unused)\n  (cons used 1 2))\n");
    assert!(
        output.status.success(),
        "warnings alone should not fail check: {:?}",
        output.status
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for expected in [
        "warning[style/unused-variable]",
        "warning[correctness/arity-mismatch]",
        "unused lexical variable `unused`",
        "cons takes at most 2 arguments, found 3",
        "(defun demo",
        "2 warnings",
    ] {
        assert!(stderr.contains(expected), "{stderr}");
    }
}

#[test]
fn check_target_selects_reader_branches_for_semantic_warnings() {
    let source = "
#+(and macos arm64) (defun demo (mac) nil)
#+(and windows x86-64) (defun demo (win) nil)
#+(and android linux) (defun demo (phone) nil)
(cons 1 2 #+windows 3)
#+android (car 1 2)
";
    assert!(check_stdin(source).stderr.is_empty());
    for (triple, variable, arity) in [
        ("aarch64-apple-darwin", "mac", None),
        (
            "x86_64-pc-windows-msvc",
            "win",
            Some("cons takes at most 2 arguments, found 3"),
        ),
        (
            "aarch64-linux-android",
            "phone",
            Some("car takes at most 1 argument, found 2"),
        ),
    ] {
        let output = check_stdin_with_args(source, ["--target", triple, "--format", "json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let messages: Vec<_> = report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|diagnostic| diagnostic["message"].as_str().unwrap())
            .collect();
        let unused = format!("unused lexical variable `{variable}`");
        let expected: Vec<_> = std::iter::once(unused.as_str()).chain(arity).collect();
        assert_eq!(messages, expected);
    }
}

#[test]
fn explain_resolves_a_rule_by_its_short_name() {
    let explain = zkcl().args(["explain", "if-to-when"]).output().unwrap();
    assert!(explain.status.success());
    let explain_out = String::from_utf8(explain.stdout).unwrap();
    assert!(explain_out.contains("style/if-to-when"));
    assert!(explain_out.contains("Why it matters:"));
}

#[test]
fn explain_list_prints_qualified_rule_ids() {
    let list_rules = zkcl().args(["explain", "--list"]).output().unwrap();
    assert!(list_rules.status.success());
    let list_out = String::from_utf8(list_rules.stdout).unwrap();
    assert!(list_out.contains("security/read-time-eval"));
    assert!(list_out.contains("style/defpackage-interned-symbol"));
}

#[test]
fn lint_directory_config_omits_ignored_macro_bodies_from_diagnostics() {
    let dir = task_tmp().join(format!("zkcl-cli-lint-{}", id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(".zkcl-lint.lisp"),
        "(:preset :strict\n :macros ((custom-dsl :lint-as :ignore)))\n",
    )
    .unwrap();
    let file = dir.join("sample.lisp");
    fs::write(
        &file,
        "(defun demo (flag x)\n  (custom-dsl (/ x 0))\n  (if flag (print x)))\n",
    )
    .unwrap();

    let json_out = zkcl()
        .args(["lint", "--format", "json"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(json_out.status.success());
    let json_text = String::from_utf8(json_out.stdout).unwrap();
    assert!(
        json_text.contains("\"code\":\"style/if-to-when\""),
        "{json_text}"
    );
    assert!(
        !json_text.contains("division-by-zero"),
        "macro :ignore should apply: {json_text}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn lint_deny_warnings_rejects_a_conditional_that_fix_rewrites() {
    let dir = task_tmp().join(format!("zkcl-cli-lint-fix-{}", id()));
    fs::create_dir(&dir).unwrap();
    let file = dir.join("conditional.lisp");
    fs::write(&file, "(defun demo (flag x) (if flag (print x)))\n").unwrap();

    let denied = zkcl()
        .args(["lint", "--preset", "strict", "--deny-warnings"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(denied.status.code(), Some(1));
    let denied_err = String::from_utf8(denied.stderr).unwrap();
    assert!(
        denied_err.contains("warning[style/if-to-when]:"),
        "{denied_err}"
    );

    let fixed = zkcl()
        .args(["lint", "--preset", "strict", "--fix"])
        .arg(&file)
        .output()
        .unwrap();
    assert!(fixed.status.success());
    let updated = fs::read_to_string(&file).unwrap();
    assert!(updated.contains("(when flag (print x))"), "{updated}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn check_and_lint_preview_the_same_safe_and_unsafe_fixes_they_apply() {
    let directory = task_tmp().join(format!("zkcl-cli-fix-preview-{}", id()));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("preview.lisp");
    let original = concat!(
        "(defun demo (items predicate)\n",
        "  (progn (if items nil t))\n",
        "  (length (remove-if-not predicate items)))\n",
    );
    let safe = "(defun demo (items predicate)\n  (if items nil t)\n  (length (remove-if-not predicate items)))\n";
    let unsafe_fixed =
        "(defun demo (items predicate)\n  (if items nil t)\n  (count-if predicate items))\n";
    let label = zkcl_terminal::display_path(&path);
    let header = format!("--- {label}\n+++ {label}\n@@ -1,3 +1,3 @@\n");
    let safe_diff = format!(
        "{header} (defun demo (items predicate)\n-  (progn (if items nil t))\n+  (if items nil t)\n   (length (remove-if-not predicate items)))\n"
    );
    let unsafe_diff = format!(
        "{header} (defun demo (items predicate)\n-  (progn (if items nil t))\n-  (length (remove-if-not predicate items)))\n+  (if items nil t)\n+  (count-if predicate items))\n"
    );

    // The PROGN and nested IF fixes overlap. Only the outer fix wins in this pass.
    for (command, preset) in [("check", &["--preset", "strict"][..]), ("lint", &[][..])] {
        for (flags, fixed, expected_diff) in [
            (&[][..], safe, &safe_diff),
            (&["--unsafe-fixes"][..], unsafe_fixed, &unsafe_diff),
        ] {
            fs::write(&path, original).unwrap();
            let preview = zkcl()
                .args([command, "--fix-dry-run"])
                .args(preset)
                .args(flags)
                .arg(&path)
                .output()
                .unwrap();
            let diagnostics = String::from_utf8(preview.stderr).unwrap();
            assert!(preview.status.success(), "{diagnostics}");
            assert_eq!(String::from_utf8(preview.stdout).unwrap(), *expected_diff);
            assert!(diagnostics.contains("warning[complexity/redundant-progn]:"));
            assert!(diagnostics.contains("warning[complexity/if-boolean-branch]:"));
            assert!(diagnostics.contains("warning[perf/count-filtered-sequence]:"));
            assert!(!diagnostics.contains("--- ") && !diagnostics.contains("applied fixes"));
            assert_eq!(fs::read_to_string(&path).unwrap(), original);

            let apply = zkcl()
                .args([command, "--fix", "--format", "json"])
                .args(preset)
                .args(flags)
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                apply.status.success(),
                "{}",
                String::from_utf8_lossy(&apply.stderr)
            );
            assert!(apply.stderr.is_empty());
            let payload: serde_json::Value = serde_json::from_slice(&apply.stdout).unwrap();
            assert_eq!(payload["fixed_files"], 1);
            assert!(payload.get("fix_diff").is_none());
            assert_eq!(fs::read_to_string(&path).unwrap(), fixed);
        }
    }
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn check_stdin_preview_keeps_json_clean_and_reports_original_diagnostics() {
    let source = "(defun flag (x) (if x t nil))";
    let flags = ["--format", "json", "--deny-warnings"];
    let original = check_stdin_with_args(source, flags);
    let safe = check_stdin_with_args(source, flags.into_iter().chain(["--fix-dry-run"]));
    let unsafe_preview = check_stdin_with_args(
        source,
        flags.into_iter().chain(["--fix-dry-run", "--unsafe-fixes"]),
    );
    for output in [&original, &safe, &unsafe_preview] {
        assert_eq!(output.status.code(), Some(1));
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let original: serde_json::Value = serde_json::from_slice(&original.stdout).unwrap();
    let safe: serde_json::Value = serde_json::from_slice(&safe.stdout).unwrap();
    let unsafe_preview: serde_json::Value = serde_json::from_slice(&unsafe_preview.stdout).unwrap();
    assert!(original.get("fix_diff").is_none());
    assert_eq!(safe["fix_diff"], "");
    assert_eq!(safe["fixed_files"], 0);
    assert_eq!(unsafe_preview["fixed_files"], 0);
    assert_eq!(safe["diagnostics"], original["diagnostics"]);
    assert_eq!(unsafe_preview["diagnostics"], original["diagnostics"]);
    let diagnostics = original["diagnostics"].as_array().unwrap();
    assert!(diagnostics.iter().any(|d| {
        d["code"] == "complexity/if-boolean-branch"
            && d["suggestions"][0]["applicability"] == "maybe-incorrect"
    }));
    assert_eq!(
        unsafe_preview["fix_diff"].as_str().unwrap(),
        concat!(
            "--- <stdin>\n+++ <stdin>\n@@ -1,1 +1,1 @@\n",
            "-(defun flag (x) (if x t nil))\n\\ No newline at end of file\n",
            "+(defun flag (x) x)\n\\ No newline at end of file\n",
        ),
    );
}

#[test]
fn check_rejects_incompatible_fix_options() {
    for (flags, expected) in [
        (&["--fix", "--fix-dry-run", "-"][..], "cannot be used with"),
        (&["--fix-dry-run", "--fix", "-"][..], "cannot be used with"),
        (&["--unsafe-fixes", "-"][..], "--fix"),
        (&["--fix", "-"][..], "--fix requires files"),
        (
            &["--list-rules", "--fix-dry-run"][..],
            "cannot be used with",
        ),
        (
            &["--explain", "redundant-let", "--fix"][..],
            "cannot be used with",
        ),
    ] {
        let output = zkcl().arg("check").args(flags).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(expected), "{stderr}");
    }
}

#[test]
fn check_and_lint_keep_the_same_shared_facts_and_actual_edit_preimages() {
    let source = "(defun demo (flag x) (if flag (print x)))";
    let mut reports = Vec::new();
    for command in ["check", "lint"] {
        let mut child = zkcl()
            .args([command, "--preset", "strict", "--format", "json", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        reports.push(serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap());
    }
    assert_eq!(reports[0], reports[1]);
    let diagnostic = reports[0]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|diagnostic| diagnostic["code"] == "style/if-to-when")
        .unwrap();
    let captured = source_snapshot(&reports[0], &diagnostic["labels"][0]["snapshot_id"]);
    assert_eq!(captured["text"], source);
    assert_eq!(reports[0]["sources"].as_array().unwrap().len(), 1);
    assert_eq!(diagnostic["labels"][0]["role"], "primary");
    assert!(
        diagnostic["rationale"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    let edits = diagnostic["suggestions"][0]["edits"].as_array().unwrap();
    assert!(!edits.is_empty());
    for edit in edits {
        let start = edit["byte_range"]["start"].as_u64().unwrap() as usize;
        let end = edit["byte_range"]["end"].as_u64().unwrap() as usize;
        assert_eq!(edit["expected"], &source[start..end]);
        assert!(edit["replacement"].is_string());
    }
}

#[test]
fn json_input_errors_are_machine_reports_without_stderr_noise() {
    let directory = task_tmp();
    let path = directory.join(format!("missing-{}.lisp", id()));
    let output = zkcl()
        .args(["check", "--format", "json", "--color", "always"])
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["errors"], 1);
    assert_eq!(report["diagnostics"][0]["code"], "cli/input");
    assert!(
        report["diagnostics"][0]["labels"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(!output.stdout.contains(&0x1b));
}

#[test]
fn rule_queries_respect_json_output() {
    for args in [
        &["check", "--list-rules", "--format", "json"][..],
        &["lint", "--explain", "redundant-let", "--format", "json"][..],
    ] {
        let output = zkcl().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(report["rules"].is_array() || report["rule"]["id"] == "complexity/redundant-let");
    }
}

// Darwin file systems reject non-UTF-8 file names at creation.
#[cfg(all(unix, not(target_vendor = "apple")))]
#[test]
fn distinct_native_paths_keep_distinct_analysis_snapshots() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let directory = tempfile::tempdir_in(task_tmp()).unwrap();
    let first = directory
        .path()
        .join(OsString::from_vec(b"name-\xff.lisp".to_vec()));
    let second = directory
        .path()
        .join(OsString::from_vec(b"name-\xfe.lisp".to_vec()));
    fs::write(&first, "(car 1 2)").unwrap();
    fs::write(&second, "(cons 1 2 3)").unwrap();
    let output = zkcl()
        .args(["check", "--format", "json"])
        .args([first, second])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let diagnostics = report["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 2);
    let first = source_snapshot(&report, &diagnostics[0]["labels"][0]["snapshot_id"]);
    let second = source_snapshot(&report, &diagnostics[1]["labels"][0]["snapshot_id"]);
    assert_ne!(first["uri"], second["uri"]);
    assert_ne!(first["snapshot_id"], second["snapshot_id"]);
    assert_eq!(first["text"], "(car 1 2)");
    assert_eq!(second["text"], "(cons 1 2 3)");
}
