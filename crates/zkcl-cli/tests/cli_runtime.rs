mod common;

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio, id};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{ReplSession, zkcl};

fn error_source<'a>(
    error: &'a serde_json::Value,
    location: &serde_json::Value,
) -> &'a serde_json::Value {
    let id = &location["snapshot_id"];
    assert!(id.is_u64(), "{location}");
    error["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|source| source["snapshot_id"] == *id)
        .expect("captured source")
}

struct JsonClient {
    session: ReplSession,
    input: std::process::ChildStdin,
    messages: mpsc::Receiver<serde_json::Value>,
}

impl JsonClient {
    fn new() -> Self {
        let mut session = ReplSession::spawn(zkcl().args(["repl", "--json", "--no-init"]));
        let (input, output) = session.pipes();
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let response = serde_json::from_str(&line.unwrap()).expect("JSON REPL response");
                if sender.send(response).is_err() {
                    break;
                }
            }
        });
        Self {
            session,
            input,
            messages,
        }
    }

    fn send(&mut self, value: serde_json::Value) {
        writeln!(self.input, "{value}").unwrap();
        self.input.flush().unwrap();
    }

    fn recv(&self) -> serde_json::Value {
        self.messages
            .recv_timeout(Duration::from_secs(5))
            .expect("JSON REPL response")
    }

    fn reply(&mut self, prompt: &serde_json::Value, text: &str) {
        use serde_json::json;
        self.send(json!({"id": "reply", "reply": {"request": prompt["id"], "token": prompt["token"], "text": text}}));
    }

    fn finish(mut self) {
        drop(self.input);
        assert!(self.session.wait().success());
    }
}

#[test]
fn json_repl_debugs_live_frames_and_mutates_a_lexical_before_restart() {
    use serde_json::json;

    let mut repl = JsonClient::new();
    repl.send(json!({"id": 1, "source": "(defun audit-restart (n) (declare (optimize (debug 3))) (let ((sum (+ n 10))) (restart-case (error \"audit stop\") (recover () :report \"Recover audit\" nil)) sum))"}));
    assert!(repl.recv()["error"].is_null());
    repl.send(json!({"id": 2, "source": "(audit-restart 32)", "interactive": true}));
    let first = repl.recv();
    assert_eq!(first["event"], "debugger", "{first}");
    assert_eq!(first["data"]["restarts"][0], "Recover audit");
    assert_eq!(first["data"]["restartNames"], json!(["RECOVER", "ABORT"]));
    let locals = first["data"]["locals"].as_array().unwrap();
    assert!(
        locals.contains(&json!({"name":"SUM", "value":"42"})),
        "{first}"
    );
    assert_eq!(first["data"]["frames"][0]["function"], "AUDIT-RESTART");
    assert!(
        first["data"]["frames"][0]["text"]
            .as_str()
            .unwrap()
            .contains("defun audit-restart")
    );
    repl.reply(&first, "p (setq sum 83)");
    let second = loop {
        let response = repl.recv();
        if response["event"] != "reply" {
            break response;
        }
        assert_eq!(response["accepted"], true);
    };
    assert_eq!(second["event"], "debugger", "{second}");
    assert!(
        second["data"]["locals"]
            .as_array()
            .unwrap()
            .contains(&json!({"name":"SUM", "value":"83"})),
        "{second}"
    );
    repl.reply(&second, "recover");
    loop {
        let response = repl.recv();
        if response["id"] == 2 {
            assert!(response["error"].is_null(), "{response}");
            assert_eq!(response["values"], json!([["83"]]));
            break;
        }
    }
    repl.finish();
}

#[test]
fn json_repl_prompts_for_input_streams_output_and_cancels_a_debugger_wait() {
    use serde_json::json;

    let mut repl = JsonClient::new();
    repl.send(json!({"id": 1, "source": "(progn (write-line \"雪\") (read-line))", "interactive": true, "stream": true}));
    let mut emitted = String::new();
    let prompt = loop {
        let response = repl.recv();
        match response["event"].as_str() {
            Some("output") => emitted.push_str(response["text"].as_str().unwrap()),
            Some("input") => break response,
            _ => panic!("unexpected response {response}"),
        }
    };
    assert_eq!(emitted, "雪\n");
    repl.reply(&prompt, "input answer\n");
    loop {
        let response = repl.recv();
        if response["id"] == 1 {
            assert_eq!(response["values"], json!([["\"input answer\"", "NIL"]]));
            assert_eq!(response["stdout"], "雪\n");
            break;
        }
    }

    repl.send(json!({"id": 2, "source": "missing-input", "interactive": true}));
    let prompt = repl.recv();
    assert_eq!(prompt["event"], "debugger", "{prompt}");
    assert_eq!(
        prompt["data"]["condition"],
        "unbound variable MISSING-INPUT"
    );
    repl.send(json!({"id": 3, "cancel": 2}));
    let result = repl.recv();
    assert_eq!(result["id"], 2, "{result}");
    assert_eq!(result["error"]["kind"], "cancelled", "{result}");
    assert_eq!(repl.recv()["cancelled"], true);
    repl.send(json!({"id": 4, "source": "(+ 20 22)"}));
    assert_eq!(repl.recv()["values"], json!([["42"]]));
    repl.finish();
}

#[test]
fn json_repl_inspector_preserves_object_identity() {
    use serde_json::json;

    let mut repl = JsonClient::new();
    repl.send(json!({"id": 1, "source": "(defparameter *object* (cons (cons :a 42) nil)) (inspect *object*) (cdar *object*)", "interactive": true}));
    let mut page = repl.recv();
    assert_eq!(page["event"], "inspector", "{page}");
    for command in ["0", "set 1 73", "q"] {
        repl.reply(&page, command);
        loop {
            page = repl.recv();
            if page["event"] != "reply" {
                break;
            }
        }
    }
    assert_eq!(page["values"], json!([["*OBJECT*"], [], ["73"]]), "{page}");
    repl.finish();
}

#[test]
fn json_repl_identifies_the_submission_that_defined_an_erroring_function() {
    let result = with_input(
        zkcl().args(["repl", "--json"]),
        br#"{"id":1,"source":"(defun old-error () (error \"old\"))"}
{"id":2,"source":"(list :long-marker :caller (old-error))"}
"#,
    );
    assert!(result.status.success(), "{:?}", result);
    let response: serde_json::Value =
        serde_json::from_slice(result.stdout.split(|&b| b == b'\n').nth(1).unwrap()).unwrap();
    let error = &response["error"];
    assert_eq!(error["frames"][0]["function"], "OLD-ERROR", "{response}");
    let label = &error["labels"][0];
    let frame = &error["frames"][0];
    assert_eq!(label["role"], "primary", "{response}");
    assert_eq!(label["snapshot_id"], frame["snapshot_id"], "{response}");
    assert_eq!(
        error_source(error, frame)["text"],
        "(defun old-error () (error \"old\"))",
    );
}

#[test]
fn json_repl_sets_a_breakpoint_and_resumes_the_same_invocation() {
    use serde_json::json;
    let mut repl = JsonClient::new();
    repl.send(json!({"id": 1, "source": "(defun stopped (x)\n  ;; 雪\n  (+ x 2))"}));
    assert!(repl.recv()["error"].is_null());
    repl.send(
        json!({"id": "comment", "breakpoint":{"action":"add", "function":"stopped", "line":2}}),
    );
    let comment = repl.recv();
    assert_eq!(
        comment["error"]["message"],
        "no executable location on this source line"
    );
    repl.send(json!({"id": 2, "breakpoint":{"action":"add", "function":"stopped", "line":3}}));
    let result = repl.recv();
    assert_eq!(result["breakpoint"]["number"], 1, "{result}");
    repl.send(json!({"id": 3, "source":"(stopped 40)", "interactive": true}));
    let event = repl.recv();
    assert_eq!(event["data"]["condition"], "Breakpoint 1", "{event}");
    assert_eq!(event["data"]["frames"][0]["line"], 3, "{event}");
    repl.reply(&event, "continue");
    loop {
        let response = repl.recv();
        if response["id"] == 3 {
            assert_eq!(response["values"], json!([["42"]]), "{response}");
            break;
        }
    }
    repl.send(json!({"id": 4, "breakpoint":{"action":"delete", "number":1}}));
    assert!(repl.recv()["error"].is_null());
    repl.finish();
}

#[test]
fn repl_init_keeps_package_and_reader_settings_for_subsequent_input() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/repl-regression");
    fs::create_dir_all(&directory).unwrap();
    let temp = tempfile::tempdir_in(directory).unwrap();
    let init = temp.path().join("init.lisp");
    fs::write(
        &init,
        "(defpackage :audit-repl (:use :cl)) (in-package :audit-repl) (defparameter value 42)",
    )
    .unwrap();
    let result = with_input(
        zkcl().args(["repl", "--init"]).arg(&init),
        b"(values (package-name *package*) value)",
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"\"AUDIT-REPL\"\n42\n");
}

fn repl(source: &str) -> Output {
    with_input(zkcl().arg("repl"), source.as_bytes())
}

fn with_input(command: &mut Command, input: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn wasm_run_reports_callers_through_its_error_source() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/cli-embedding");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("nested-trap-{}.wasm", id()));
    let _file = std::mem::DropGuard::new(&path, |path| {
        let _ = fs::remove_file(path);
    });
    // run calls function 1, whose body traps on UNREACHABLE.
    let bytes = [
        0, 97, 115, 109, 1, 0, 0, 0, // module header
        1, 4, 1, 0x60, 0, 0, // one () -> () type
        3, 3, 2, 0, 0, // two functions of that type
        7, 7, 1, 3, b'r', b'u', b'n', 0, 0, // export function 0 as run
        10, 10, 2, 4, 0, 0x10, 1, 0x0b, 3, 0, 0, 0x0b, // function bodies
    ];
    fs::write(&path, bytes).unwrap();
    let output = zkcl().arg("run").arg(&path).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostic.contains("error[wasm/execution]: unreachable instruction"),
        "{diagnostic}"
    );
    let callee = diagnostic.find("in function 1").expect(&diagnostic);
    let caller = diagnostic
        .find("in function 0 (\"run\")")
        .expect(&diagnostic);
    assert!(callee < caller, "{diagnostic}");
    assert_eq!(diagnostic.matches("function 1").count(), 1, "{diagnostic}");
}

#[test]
fn piped_repl_prints_only_returned_values() {
    let output = repl(
        "(defun twice (x)\n  (+ x x))\n(values (twice 21) nil \"雪😀\" '(1 . 2))\n(values)\n(+ 2 3)",
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "TWICE\n42\nNIL\n\"雪😀\"\n(1 . 2)\n5\n"
    );
    assert!(output.stderr.is_empty());

    let no_values = repl("; comment\n(values)");
    assert!(no_values.status.success());
    assert!(no_values.stdout.is_empty());
    assert!(no_values.stderr.is_empty());
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn lisp_internal_clocks_distinguish_sleep_from_cpu_work() {
    // Run in its own process so unrelated tests do not add to process CPU time.
    let output = repl(
        "(let* ((units internal-time-units-per-second)
                 (real-start (get-internal-real-time))
                 (run-start (get-internal-run-time)))
           (assert (= units 1000000000))
           (assert (typep real-start '(integer 0 *)))
           (assert (typep run-start '(integer 0 *)))
           (sleep 1/5)
           (let ((idle-run (- (get-internal-run-time) run-start))
                 (idle-real (- (get-internal-real-time) real-start)))
             (assert (>= idle-real (/ (* 19 units) 100)))
             (assert (<= 0 idle-run (/ idle-real 2))))
           (let ((work-start (get-internal-run-time))
                 (work-target (/ units 20)))
             (loop repeat 200
                   do (let ((sum 0))
                        (dotimes (i 10000) (incf sum i))
                        (assert (= sum 49995000)))
                   until (>= (- (get-internal-run-time) work-start) work-target))
             (assert (>= (- (get-internal-run-time) work-start) work-target)))
           t)",
    );
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"T\n");
}

#[test]
fn time_traces_both_clocks_without_altering_values() {
    let output = repl(
        "(let ((result nil))
           (let ((report
                   (with-output-to-string (*trace-output*)
                     (let ((count 0))
                       (setf result
                             (multiple-value-list
                               (time (values (incf count) :second))))
                       (assert (null (multiple-value-list (time (values)))))
                       (assert (= count 1))))))
             (assert (equal result '(1 :second)))
             (assert (search \"Elapsed time:\" report))
             (assert (search \"Run time:\" report))
             t))",
    );
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(output.stdout, b"T\n");
}

#[test]
fn repl_preserves_earlier_values_before_errors() {
    let output = repl("(values 1 2)\n(list 3\n");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"1\n2\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("unterminated list"));
}

#[test]
fn repl_history_shifts_after_a_zero_value_form() {
    let output = repl(
        "(+ 1 2) (values 7 8) (values) (values (list - + ++ +++) (list * ** ***) (list / // ///))",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"3\n7\n8\n((VALUES (LIST - + ++ +++) (LIST * ** ***) (LIST / // ///)) (VALUES) (VALUES 7 8) (+ 1 2))\n(NIL 7 3)\n(NIL (7 8) (3))\n"
    );
}

#[test]
fn jit_repl_redefinition_preserves_saved_function_identity() {
    if !cfg!(any(feature = "cranelift-jit", feature = "direct-jit")) {
        let output = zkcl().args(["repl", "--jit"]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("--jit requires"));
        return;
    }
    let output = with_input(
        zkcl().args(["repl", "--jit"]),
        b"(defun step-value (x) (+ x 1))
          (defun existing-caller (x) (step-value x))
          (defparameter *original* #'step-value)
          (dotimes (i 1000) (assert (= (existing-caller i) (+ i 1))))
          (defun step-value (x) (+ x 2))
          (values (existing-caller 40) (funcall *original* 40))",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.ends_with(b"42\n41\n"));
}

#[test]
fn run_uses_the_jit_backend_named_on_the_command_line() {
    check_requested_jit_backend("run", |backend, script| {
        zkcl()
            .args(["run", "--jit-backend", backend])
            .arg(script)
            .output()
            .unwrap()
    });
}

#[test]
fn repl_uses_the_requested_jit_backend_for_startup_files() {
    check_requested_jit_backend("repl", |backend, script| {
        zkcl()
            .args(["repl", "--jit-backend", backend, "--load"])
            .arg(script)
            .output()
            .unwrap()
    });
}

fn check_requested_jit_backend(
    command: &str,
    run_script: impl Fn(&str, &std::path::Path) -> std::process::Output,
) {
    let help = zkcl().args([command, "--help"]).output().unwrap();
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        words.contains("--jit-backend") && words.contains("[possible values: direct, cranelift]"),
        "{text}"
    );
    let script = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("select-jit-{command}-{}.lisp", id()));
    let source = b"(defun answer (x) (+ x 1))
                   (compile 'answer)
                   (dotimes (i 128) (assert (= (answer i) (1+ i))))";
    fs::write(&script, source).unwrap();
    for (backend, enabled) in [
        ("direct", cfg!(feature = "direct-jit")),
        ("cranelift", cfg!(feature = "cranelift-jit")),
    ] {
        let result = run_script(backend, &script);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert_eq!(result.status.success(), enabled, "{backend}: {stderr}");
        if !enabled {
            assert!(
                stderr.contains(&format!("{backend}-jit Cargo feature")),
                "{stderr}"
            );
        }
    }
    fs::remove_file(script).unwrap();
}

#[test]
fn piped_repl_reserves_stdin_for_submitted_source_during_startup() {
    let startup = std::env::temp_dir().join(format!("zkcl-piped-repl-{}.lisp", id()));
    fs::write(
        &startup,
        "(assert (eq (read-line *standard-input* nil :eof) :eof))
         (assert (eq (read-line *terminal-io* nil :eof) :eof))",
    )
    .unwrap();
    let output = with_input(zkcl().args(["repl", "--load"]).arg(&startup), b"(+ 21 21)");
    fs::remove_file(startup).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"42\n");
}

#[test]
fn run_expands_macros_defined_earlier_in_script() {
    let directory = std::env::temp_dir().join(format!("zkcl-cli-runtime-{}", id()));
    fs::create_dir(&directory).unwrap();
    let script = directory.join("source with space.lisp");
    fs::write(
        &script,
        "(defmacro add-two (x) (list '+ x 2))\n(if (= (add-two 40) 42) t (error \"bad result\"))",
    )
    .unwrap();
    let success = zkcl().arg("run").arg(&script).output();
    fs::remove_dir_all(&directory).unwrap();

    let success = success.unwrap();
    assert!(
        success.status.success(),
        "{}",
        String::from_utf8_lossy(&success.stderr)
    );
    assert!(success.stdout.is_empty());
    assert!(success.stderr.is_empty());
}

#[test]
fn run_reports_source_location_of_script_failure() {
    let directory = std::env::temp_dir().join(format!("zkcl-cli-runtime-failure-{}", id()));
    fs::create_dir(&directory).unwrap();
    let bad = directory.join("bad.lisp");
    let bad_source = "(list (error \"script failed\"))";
    fs::write(&bad, bad_source).unwrap();
    let failure = zkcl()
        .current_dir(&directory)
        .args(["run", "bad.lisp"])
        .output();
    fs::remove_dir_all(&directory).unwrap();
    let failure = failure.unwrap();
    assert_eq!(failure.status.code(), Some(1));
    assert!(failure.stdout.is_empty());
    let message = String::from_utf8(failure.stderr).unwrap();
    for expected in [
        "error[runtime/condition]: script failed",
        "bad.lisp:1:",
        bad_source,
        "^",
    ] {
        assert!(message.contains(expected), "{message}");
    }
    assert!(
        !message.contains(&*directory.to_string_lossy()),
        "{message}"
    );
}

#[test]
fn run_returns_exit_status_computed_by_script() {
    let directory = std::env::temp_dir().join(format!("zkcl-cli-arguments-{}", id()));
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("worker.lisp"),
        "(defun answer () 23) (provide :worker)",
    )
    .unwrap();
    let script = directory.join("main.lisp");
    fs::write(
        &script,
        r#"(require :worker)
           (assert (equal (zk:command-line-arguments) '("two words" "雪" "--flag")))
           (zk:set-exit-code (answer))"#,
    )
    .unwrap();
    let result = zkcl()
        .args(["run", "-I"])
        .arg(&directory)
        .arg(&script)
        .args(["--", "two words", "雪", "--flag"])
        .output()
        .unwrap();
    fs::remove_dir_all(&directory).unwrap();
    assert_eq!(
        result.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn json_repl_keeps_the_old_session_when_reload_fails() {
    let directory = std::env::temp_dir().join(format!("zkcl-json-repl-{}", id()));
    fs::create_dir_all(&directory).unwrap();
    let startup = directory.join("startup.lisp");
    fs::write(&startup, "(defun answer () 10)").unwrap();
    let mut command = zkcl();
    command.args(["repl", "--json"]);
    if cfg!(any(feature = "cranelift-jit", feature = "direct-jit")) {
        command.arg("--jit");
    }
    command.arg("--load").arg(&startup);
    let mut session = ReplSession::spawn(&mut command);
    let (mut input, stdout) = session.pipes();
    let mut output = BufReader::new(stdout);
    let mut request = |value: serde_json::Value| {
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        assert!(output.read_line(&mut line).unwrap() > 0);
        serde_json::from_str::<serde_json::Value>(&line).unwrap()
    };
    use serde_json::json;
    let first = request(json!({
        "id": "first",
        "source": "(progn (format t \"雪\") (format *error-output* \"notice\") (values (answer) nil)) (values)"
    }));
    assert_eq!(first["id"], "first");
    assert_eq!(first["values"], json!([["10", "NIL"], []]));
    assert_eq!(first["stdout"], "雪");
    assert_eq!(first["stderr"], "notice");
    assert!(first["error"].is_null());
    let prepared = request(json!({
        "id": "prepare",
        "source": "(defparameter *saved-answer* #'answer)
                   (defparameter *session-state* 7)
                   (defun answer () 11)
                   (trace answer)"
    }));
    assert!(prepared["error"].is_null(), "{prepared}");
    fs::write(
        &startup,
        "(format t \"loading\") (format *error-output* \"startup context\")
         (defun answer () 20) (compile 'answer) (error \"startup failed\")",
    )
    .unwrap();
    let failed = request(json!({"id": 2, "reload": true}));
    assert_eq!(failed["stdout"], "loading");
    assert_eq!(failed["stderr"], "startup context");
    assert!(
        failed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("startup failed")
    );
    let retained = request(json!({
        "id": 3,
        "source": "(values (answer) (funcall *saved-answer*) *session-state*)"
    }));
    assert_eq!(retained["values"], json!([["11", "10", "7"]]));
    assert!(
        retained["stdout"].as_str().unwrap().contains("ANSWER"),
        "{retained}"
    );
    assert_eq!(retained["stderr"], "");
    fs::write(&startup, "(defun answer () 30) (compile 'answer)").unwrap();
    assert!(request(json!({"id": 4, "reload": true}))["error"].is_null());
    let refreshed = request(json!({
        "id": 5,
        "source": "(values (answer) (boundp '*saved-answer*) (boundp '*session-state*))"
    }));
    assert_eq!(refreshed["values"], json!([["30", "NIL", "NIL"]]));
    assert_eq!(refreshed["stdout"], "");
    drop(input);
    assert!(session.wait().success());
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn json_repl_preserves_session_across_failed_requests() {
    use serde_json::{Value, json};

    let input = b"{\"id\":1,\"source\":\"(values 7 8) (error \\\"stop\\\") (values 99)\"}\n\
                  {\"source\":\"\xff\"}\ntrue\n\
                  {\"id\":{\"client\":4},\"source\":\"(list + * /)\"}";
    let output = with_input(zkcl().args(["repl", "--json"]), input);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["values"], json!([["7", "8"]]));
    assert_eq!(responses[0]["error"]["message"], "stop");
    for response in &responses[1..3] {
        assert!(response["id"].is_null());
        assert!(response["error"]["message"].is_string());
        assert_eq!(response["values"], json!([]));
    }
    assert_eq!(responses[3]["id"], json!({"client": 4}));
    assert!(responses[3]["error"].is_null());
    assert_eq!(
        responses[3]["values"],
        json!([["((ERROR \"stop\") 7 (7 8))"]])
    );
}

#[test]
fn json_repl_cancels_evaluations_without_ending_session() {
    use serde_json::{Value, json};

    let marker = std::env::temp_dir().join(format!("zkcl-cancelling-{}", id()));
    let _ = fs::remove_file(&marker);
    let mut command = zkcl();
    command.args(["repl", "--json"]);
    let mut session = ReplSession::spawn(&mut command);
    let (mut input, stdout) = session.pipes();
    let output = BufReader::new(stdout);
    let (send, receive) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in output.lines() {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let exchange = (|| -> Result<Vec<Value>, Box<dyn std::error::Error>> {
        let path = serde_json::to_string(&marker)?;
        let source = format!(
            "(defparameter *kept* 7) (format t \"started\") \
             (with-open-file (file {path} :direction :output :if-exists :supersede) (values)) \
             (loop)"
        );
        writeln!(input, "{}", json!({"id": "long", "source": source}))?;
        input.flush()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            if let Ok(line) = receive.try_recv() {
                return Err(format!("evaluation returned before cancellation: {}", line?).into());
            }
            if Instant::now() >= deadline {
                return Err("evaluation did not reach its marker".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        for request in [
            json!({"id": "queued", "source": "(setf *kept* 99)"}),
            json!({"id": "queued", "source": "(setf *kept* 100)"}),
            json!({"id": 1, "cancel": "queued"}),
            json!({"id": 2, "cancel": "long"}),
            json!({"id": "next", "source": "(+ *kept* 35)"}),
        ] {
            writeln!(input, "{request}")?;
        }
        input.flush()?;
        let mut responses = (0..6)
            .map(|_| {
                let line = receive.recv_timeout(Duration::from_secs(10))??;
                Ok(serde_json::from_str(&line)?)
            })
            .collect::<Result<Vec<Value>, Box<dyn std::error::Error>>>()?;
        writeln!(input, "{}", json!({"id": 3, "cancel": "long"}))?;
        input.flush()?;
        let line = receive.recv_timeout(Duration::from_secs(10))??;
        responses.push(serde_json::from_str(&line)?);
        Ok(responses)
    })();
    drop(input);
    let status = session.wait();
    reader.join().unwrap();
    let _ = fs::remove_file(marker);
    let responses = exchange.unwrap();
    assert!(status.success());

    assert_eq!(responses[0]["id"], "long");
    assert_eq!(responses[0]["error"]["kind"], "cancelled");
    assert_eq!(responses[0]["stdout"], "started");
    assert_eq!(responses[0]["values"][0], json!(["*KEPT*"]));
    assert_eq!(responses[1]["id"], "queued");
    assert_eq!(responses[1]["error"]["kind"], "cancelled");
    assert_eq!(responses[1]["values"], json!([]));
    assert_eq!(responses[2]["id"], "queued");
    assert_eq!(
        responses[2]["error"]["message"],
        "an evaluation with this id is already pending"
    );
    for (index, id) in [(3, 1), (4, 2), (6, 3)] {
        assert_eq!(responses[index]["id"], id);
        assert_eq!(responses[index]["cancelled"], id != 3);
        assert!(responses[index]["error"].is_null());
        assert_eq!(responses[index]["stdout"], "");
    }
    assert_eq!(responses[5]["id"], "next");
    assert_eq!(responses[5]["values"], json!([["42"]]));
    assert!(responses[5]["error"].is_null());
}

#[test]
fn json_repl_cancels_a_reload_and_keeps_the_active_runtime() {
    use serde_json::{Value, json};

    let directory = tempfile::tempdir().unwrap();
    let startup = directory.path().join("startup.lisp");
    let marker = directory.path().join("candidate-started");
    fs::write(&startup, "(defparameter *answer* 10)").unwrap();
    let mut command = zkcl();
    command.args(["repl", "--json", "--load"]).arg(&startup);
    let mut session = ReplSession::spawn(&mut command);
    let (mut input, stdout) = session.pipes();
    let output = BufReader::new(stdout);
    let (send, receive) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in output.lines() {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let exercise = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut send_request = |request: Value| -> io::Result<()> {
            writeln!(input, "{request}")?;
            input.flush()
        };
        let reply = || -> Result<Value, Box<dyn std::error::Error>> {
            Ok(serde_json::from_str(
                &receive.recv_timeout(Duration::from_secs(10))??,
            )?)
        };
        send_request(json!({"source": "(setf *answer* 42)"}))?;
        let initial = reply()?;
        if initial["values"] != json!([["42"]]) {
            return Err(format!("unexpected startup response: {initial}").into());
        }
        let marker_path = serde_json::to_string(&marker)?;
        fs::write(
            &startup,
            format!(
                "(defparameter *answer* 99) (write-string \"candidate\")
                     (with-open-file (out {marker_path} :direction :output) (close out)) (loop)"
            ),
        )?;
        send_request(json!({"id": "reload", "reload": true}))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            if let Ok(line) = receive.try_recv() {
                return Err(format!("reload returned before cancellation: {}", line?).into());
            }
            if Instant::now() >= deadline {
                return Err("reload did not reach its marker".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        for request in [
            json!({"id": "queued", "reload": true}),
            json!({"id": 1, "cancel": "queued"}),
            json!({"id": 2, "cancel": "reload"}),
            json!({"source": "*answer*"}),
        ] {
            send_request(request)?;
        }
        let [cancelled, queued, ack_queued, ack_running, active] =
            [reply()?, reply()?, reply()?, reply()?, reply()?];
        if cancelled["id"] != "reload"
            || cancelled["error"]["kind"] != "cancelled"
            || cancelled["stdout"] != "candidate"
            || queued["id"] != "queued"
            || queued["error"]["kind"] != "cancelled"
            || queued["stdout"] != ""
            || ack_queued["cancelled"] != true
            || ack_running["cancelled"] != true
            || active["values"] != json!([["42"]])
        {
            return Err(format!("unexpected cancellation responses: {cancelled}, {queued}, {ack_queued}, {ack_running}, {active}").into());
        }

        fs::write(&startup, "(defparameter *answer* 21)")?;
        send_request(json!({"id": "reload", "reload": true}))?;
        let retried = reply()?;
        send_request(json!({"source": "*answer*"}))?;
        let refreshed = reply()?;
        if !retried["error"].is_null() || refreshed["values"] != json!([["21"]]) {
            return Err(format!("unexpected reload retry: {retried}, {refreshed}").into());
        }
        Ok(())
    })();
    drop(input);
    let status = session.wait();
    reader.join().unwrap();
    exercise.unwrap();
    assert!(status.success());
}

#[test]
fn cli_reports_named_lisp_frames_in_unhandled_errors() {
    const SOURCE: &str = "(defun trace-leaf () (+ 1 (error \"trace failure\")))
                           (defun trace-caller () (+ 1 (trace-leaf)))
                           (list (trace-caller))";
    let text_output = repl(SOURCE);
    assert!(!text_output.status.success());
    let stderr = String::from_utf8(text_output.stderr).unwrap();
    assert!(stderr.contains("trace failure"), "{stderr}");
    let leaf = stderr.find("TRACE-LEAF").expect("leaf frame");
    let caller = stderr.find("TRACE-CALLER").expect("caller frame");
    assert!(leaf < caller, "{stderr}");

    let request = format!("{}\n", serde_json::json!({"id": 1, "source": SOURCE}));
    let output = with_input(zkcl().args(["repl", "--json"]), request.as_bytes());
    assert!(output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let error = &response["error"];
    assert!(error["message"].as_str().unwrap().contains("trace failure"));
    let frames = error["frames"].as_array().unwrap();
    let names: Vec<_> = frames
        .iter()
        .filter_map(|frame| frame["function"].as_str())
        .collect();
    assert!(names[0].ends_with("TRACE-LEAF"), "{names:?}");
    assert!(names[1].ends_with("TRACE-CALLER"), "{names:?}");
    let caller = frames
        .iter()
        .find(|frame| frame["function"].as_str() == Some(names[1]))
        .unwrap();
    assert!(caller["bytecodePc"].is_u64());
    assert!(caller["byte_range"]["start"].is_u64());
    assert!(error_source(error, caller)["path"].is_null());
}

#[test]
fn json_repl_reports_the_source_path_for_a_loaded_function() {
    let directory = std::env::temp_dir().join(format!("zkcl-json-frame-path-{}", id()));
    fs::create_dir(&directory).unwrap();
    let startup = directory.join("source with space.lisp");
    let source = "; 雪\n(defun cli-file-leaf () (+ 1 (error \"file trace failure\")))";
    fs::write(&startup, source).unwrap();
    let path = startup.canonicalize().unwrap();
    let request = format!(
        "{}\n",
        serde_json::json!({"id": 1, "source": "(list (cli-file-leaf))"})
    );
    let output = with_input(
        zkcl().args(["repl", "--json", "--load"]).arg(&startup),
        request.as_bytes(),
    );
    fs::remove_dir_all(&directory).unwrap();

    assert!(output.status.success(), "{output:?}");
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let error = &response["error"];
    let frames = error["frames"].as_array().unwrap();
    assert_eq!(frames[0]["function"], "CLI-FILE-LEAF");
    assert_eq!(
        error_source(error, &frames[0])["path"],
        path.to_str().unwrap()
    );
    assert_eq!(
        frames[0]["byte_range"]["start"],
        source.find("(error").unwrap() + 1
    );
    assert!(error_source(error, &frames[1])["path"].is_null());
}

#[test]
fn heap_size_option_limits_lisp_array_allocation() {
    // The general vector exceeds 4 MiB on both supported word widths.
    let source = b"(let ((v (make-array 2000000 :initial-element 9))) (aref v 1999999))";
    let small = with_input(zkcl().args(["--heap-size-mib", "4", "repl"]), source);
    assert!(!small.status.success());
    assert!(
        String::from_utf8_lossy(&small.stderr).contains("heap"),
        "{small:?}"
    );
    let large = with_input(zkcl().args(["--heap-size-mib", "32", "repl"]), source);
    assert!(
        large.status.success(),
        "{}",
        String::from_utf8_lossy(&large.stderr)
    );
    assert_eq!(large.stdout, b"9\n");
}

#[test]
fn heap_size_option_rejects_invalid_mib_values() {
    for invalid in ["0", "-1", "large", "18446744073709551616"] {
        let result = zkcl()
            .args(["--heap-size-mib", invalid, "repl"])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains("--heap-size-mib") && stderr.contains("positive whole number"),
            "{invalid}: {stderr}"
        );
    }
}

/// Closing input retires the request stream; a quiet session ends rather than
/// parking itself on a stream nobody will write to again.
#[test]
fn a_quiet_json_session_exits_when_its_input_closes() {
    let mut command = zkcl();
    command.args(["repl", "--json"]);
    let mut session = ReplSession::spawn(&mut command);
    let (mut input, stdout) = session.pipes();
    let mut output = BufReader::new(stdout);
    writeln!(
        input,
        "{}",
        serde_json::json!({"id": 1, "source": "(+ 1 2)"})
    )
    .unwrap();
    input.flush().unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert!(line.contains("\"values\""), "{line}");
    drop(input);
    assert!(
        session.exited_within(Duration::from_secs(10)),
        "the session outlived its input"
    );
}

/// A client that vanished must not turn the session's last write into a failure.
#[test]
fn a_json_session_exits_cleanly_when_its_client_is_gone() {
    let mut command = zkcl();
    command.args(["repl", "--json"]);
    let mut session = ReplSession::spawn(&mut command);
    let (mut input, output) = session.pipes();
    writeln!(
        input,
        "{}",
        serde_json::json!({"id": 1, "source": "(+ 1 2)"})
    )
    .unwrap();
    input.flush().unwrap();
    // The client stops reading and sends nothing more before the answer exists.
    drop(output);
    drop(input);
    assert!(
        session.exited_within(Duration::from_secs(10)),
        "the session outlived its client"
    );
    assert!(
        session.wait().success(),
        "a vanished client is not an error"
    );
}

/// Tests hold their own pipes, so a panic cannot close the session's input for
/// it. The guard must still reclaim the process instead of leaving it behind.
#[cfg(unix)]
#[test]
fn dropping_a_session_reclaims_a_process_that_ignores_its_client() {
    let mut command = zkcl();
    command.args(["repl", "--json"]);
    let mut session = ReplSession::spawn(&mut command);
    let (mut input, _output) = session.pipes();
    writeln!(
        input,
        "{}",
        serde_json::json!({"id": 1, "source": "(loop)"})
    )
    .unwrap();
    input.flush().unwrap();
    thread::sleep(Duration::from_millis(300));
    let pid = session.pid();
    // No explicit kill anywhere: the session ends with the test that owns it.
    drop(session);
    let status = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "session {pid} survived the test that owned it"
    );
}
