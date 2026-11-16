mod common;

use common::{ReplSession, zkcl};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc;
use std::time::Duration;

struct Client {
    session: ReplSession,
    input: std::process::ChildStdin,
    messages: mpsc::Receiver<Value>,
}
impl Client {
    fn new() -> Self {
        let mut session = ReplSession::spawn(zkcl().args(["repl", "--json", "--no-init"]));
        let (input, output) = session.pipes();
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if sender
                    .send(serde_json::from_str(&line.unwrap()).unwrap())
                    .is_err()
                {
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
    fn send(&mut self, value: Value) {
        writeln!(self.input, "{value}").unwrap();
        self.input.flush().unwrap();
    }
    fn recv(&self) -> Value {
        self.messages
            .recv_timeout(Duration::from_secs(15))
            .expect("JSON protocol response")
    }
    fn response(&self, id: &Value) -> Value {
        loop {
            let value = self.recv();
            if value["id"] == *id && value.get("event").is_none() {
                return value;
            }
        }
    }
    fn action(&mut self, id: Value, prompt: &Value, action: Value) -> (Value, Value) {
        self.send(json!({"id":id,"reply":{"request":prompt["id"],"token":prompt["token"],"action":action}}));
        let mut response = None;
        let mut prompt = None;
        while response.is_none() || prompt.is_none() {
            let value = self.recv();
            if value["event"] == "debugger-action" {
                response = Some(value);
            } else if value["event"] == "debugger" {
                prompt = Some(value);
            }
        }
        (response.unwrap(), prompt.unwrap())
    }
    fn finish(mut self) {
        drop(self.input);
        assert!(self.session.wait().success());
    }
}

#[test]
fn json_source_origin_and_error_kind_survive_later_calls() {
    let mut repl = Client::new();
    let source = "(defun origin-json () (error \"origin failure\"))";
    repl.send(json!({"id":1,"source":source,"sourceOrigin":{"path":"/tmp/editor-origin.lisp","uri":"file:///tmp/editor-origin.lisp","version":17,"baseOffset":120}}));
    assert!(repl.response(&json!(1))["error"].is_null());
    repl.send(json!({"id":2,"source":"(origin-json)"}));
    let response = repl.response(&json!(2));
    let error = &response["error"];
    assert_eq!(error["kind"], "condition", "{response}");
    assert_eq!(error["conditionType"], "SIMPLE-ERROR", "{response}");
    let label = &error["labels"][0];
    assert_eq!(label["role"], "primary", "{response}");
    let snapshot = error["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|snapshot| snapshot["snapshot_id"] == label["snapshot_id"])
        .unwrap();
    assert_eq!(snapshot["uri"], "file:///tmp/editor-origin.lisp");
    assert_eq!(snapshot["version"], 17);
    assert_eq!(snapshot["base_offset"], 120);
    assert_eq!(snapshot["text"], source);
    let start = label["byte_range"]["start"].as_u64().unwrap() as usize;
    let end = label["byte_range"]["end"].as_u64().unwrap() as usize;
    assert!(source[start..end].starts_with("error"), "{response}");
    assert!(error["conditionObject"]["handle"].is_u64(), "{response}");
    repl.finish();
}

#[test]
fn json_handles_keep_identity_across_gc_support_modify_and_release() {
    let mut repl = Client::new();
    repl.send(json!({"id":1,"source":"(cons 41 nil)"}));
    let result = repl.response(&json!(1));
    let handle = result["objects"][0][0]["handle"].clone();
    assert!(handle.is_u64(), "{result}");
    repl.send(json!({"id":2,"source":"(zk:gc)"}));
    let gc = repl.response(&json!(2));
    assert!(gc["error"].is_null(), "{gc}");
    repl.send(json!({"id":3,"object":{"action":"modify","handle":handle,"index":0,"value":{"source":"42"}}}));
    assert!(repl.response(&json!(3))["error"].is_null());
    repl.send(json!({"id":4,"object":{"action":"inspect","handle":handle}}));
    assert_eq!(
        repl.response(&json!(4))["object"]["parts"][0]["value"]["preview"],
        "42"
    );
    repl.send(json!({"id":5,"object":{"action":"release","handles":[handle]}}));
    assert_eq!(
        repl.response(&json!(5))["object"]["released"],
        json!([handle])
    );
    repl.send(json!({"id":6,"object":{"action":"inspect","handle":handle}}));
    assert!(!repl.response(&json!(6))["error"].is_null());
    repl.send(json!({"id":7,"source":"(cons :new nil)"}));
    let new = repl.response(&json!(7))["objects"][0][0]["handle"]
        .as_u64()
        .unwrap();
    assert!(new > handle.as_u64().unwrap());
    repl.send(json!({"id":8,"reload":true}));
    assert!(repl.response(&json!(8))["error"].is_null());
    repl.send(json!({"id":9,"object":{"action":"inspect","handle":new}}));
    assert!(!repl.response(&json!(9))["error"].is_null());
    repl.finish();
}

#[test]
fn json_evaluation_keeps_rendered_values_and_object_previews_separate() {
    let mut repl = Client::new();
    let source = r#"(values (make-string 240 :initial-element #\α)
                           (list 1 2 3 4 5 6 7 8 9 10)
                           (make-array 240 :element-type 'bit :initial-element 1))
                    (values)"#;
    repl.send(json!({"id":1,"source":source}));
    let result = repl.response(&json!(1));
    assert!(result["error"].is_null(), "{result}");
    assert_eq!(
        result["values"],
        json!([
            [
                format!("\"{}\"", "α".repeat(240)),
                "(1 2 3 4 5 6 7 8 9 10)".to_owned(),
                format!("#*{}", "1".repeat(240))
            ],
            []
        ])
    );
    assert_eq!(
        result["objects"][0][0]["preview"],
        format!("\"{}…", "α".repeat(199))
    );
    assert_eq!(result["objects"][0][1]["preview"], "(1 2 3 4 5 6 7 8 ...)");
    assert_eq!(
        result["objects"][0][2]["preview"],
        format!("#*{}…", "1".repeat(198))
    );
    assert_eq!(result["objects"][1], json!([]));
    repl.send(json!({"id":2,"source":source,"render":false}));
    let previews = repl.response(&json!(2));
    assert!(previews["error"].is_null(), "{previews}");
    for index in 0..3 {
        assert_eq!(
            previews["values"][0][index],
            result["objects"][0][index]["preview"]
        );
    }
    repl.send(json!({
        "id":3,
        "source":"(let ((large (ash 1 8192))) (values (expt 10 249) large (/ 1 large) (complex large 1)))"
    }));
    let numbers = repl.response(&json!(3));
    assert!(numbers["error"].is_null(), "{numbers}");
    assert_eq!(numbers["values"][0][0], format!("1{}", "0".repeat(249)));
    assert_eq!(
        numbers["objects"][0][0]["preview"],
        format!("1{}…", "0".repeat(199))
    );
    for (index, kind) in ["BIGNUM", "RATIO", "COMPLEX"].into_iter().enumerate() {
        assert!(numbers["values"][0][index + 1].as_str().unwrap().len() > 2000);
        assert_eq!(
            numbers["objects"][0][index + 1]["preview"],
            format!("#<{kind} …>")
        );
    }
    repl.send(json!({
        "id":4,
        "source":"(let ((items (list :item))) (setf (cdr items) items) items)",
        "render": false
    }));
    let cycle = repl.response(&json!(4));
    assert_eq!(cycle["values"], json!([["(:ITEM . #<CIRCULAR-CONS>)"]]));
    assert!(cycle["objects"][0][0]["handle"].is_u64());
    repl.finish();
}

#[test]
fn json_printer_error_keeps_committed_objects_and_later_values() {
    let mut repl = Client::new();
    let setup = r#"(defclass json-bad-printer () ((item :initform 5)))
                   (defmethod print-object ((object json-bad-printer) stream)
                     (declare (ignore object stream))
                     (error "cannot render"))"#;
    repl.send(json!({"id":1,"source":setup}));
    assert!(repl.response(&json!(1))["error"].is_null());

    let source = "(values (make-instance 'json-bad-printer) 42) (values 73 (type-of *))";
    repl.send(json!({"id":2,"source":source,"interactive":true}));
    let result = repl.recv();
    assert!(result.get("event").is_none(), "{result}");
    assert!(result["error"].is_null(), "{result}");
    assert_eq!(result["values"][0][0], result["objects"][0][0]["preview"]);
    assert_eq!(result["values"][0][1], "42");
    assert_eq!(result["values"][1], json!(["73", "JSON-BAD-PRINTER"]));
    assert_eq!(result["printErrors"][0]["form"], 0);
    assert_eq!(result["printErrors"][0]["index"], 0);
    assert_eq!(
        result["printErrors"][0]["error"]["message"],
        "cannot render"
    );

    let handle = &result["objects"][0][0]["handle"];
    repl.send(json!({"id":3,"object":{"action":"inspect","handle":handle}}));
    assert_eq!(
        repl.response(&json!(3))["object"]["parts"][0]["value"]["preview"],
        "5"
    );
    repl.finish();
}

#[test]
fn json_typed_debugger_mutates_evaluates_inspects_and_invokes_restart() {
    let mut repl = Client::new();
    repl.send(json!({"id":1,"source":"(defun typed-json (n) (declare (optimize (debug 3))) (let ((value (list n))) (restart-case (error \"typed stop\") (recover () nil)) (car value)))"}));
    assert!(repl.response(&json!(1))["error"].is_null());
    repl.send(json!({"id":2,"source":"(typed-json 40)","interactive":true}));
    let prompt = repl.recv();
    assert_eq!(prompt["event"], "debugger", "{prompt}");
    let (result, prompt) = repl.action(
        json!(3),
        &prompt,
        json!({"kind":"set","frame":0,"name":"value","source":"(list (+ n 2))"}),
    );
    assert!(result["result"]["error"].is_null(), "{result}");
    let handle = result["result"]["value"]["handle"].clone();
    let (result, prompt) = repl.action(
        json!(4),
        &prompt,
        json!({"kind":"object","object":{"action":"inspect","handle":handle}}),
    );
    assert_eq!(
        result["result"]["value"]["parts"][0]["value"]["preview"], "42",
        "{result}"
    );
    let (result, prompt) = repl.action(
        json!(5),
        &prompt,
        json!({"kind":"eval","frame":0,"source":"(values n (car value))"}),
    );
    assert_eq!(
        result["result"]["value"]["values"][1]["preview"], "42",
        "{result}"
    );
    assert_eq!(
        result["result"]["value"]["renderedValues"],
        json!(["40", "42"])
    );
    let (result, prompt) = repl.action(json!(6), &prompt, json!({"kind":"frame","frame":999}));
    assert!(!result["result"]["error"].is_null(), "{result}");
    repl.send(json!({"id":7,"reply":{"request":2,"token":prompt["token"],"action":{"kind":"restart","name":"recover"}}}));
    let result = repl.response(&json!(2));
    assert_eq!(result["values"], json!([["42"]]), "{result}");
    repl.finish();
}

#[test]
fn json_paused_lookups_and_preview_requests_do_not_call_the_lisp_printer() {
    let mut repl = Client::new();
    let source = r#"(defpackage "JSON-PREVIEW" (:use :cl))
                    (in-package "JSON-PREVIEW")
                    (defparameter *printer-calls* 0)
                    (defparameter *printer-fails* nil)
                    (defparameter *report-calls* 0)
                    (defclass inspected () ())
                    (defmethod print-object ((object inspected) stream)
                      (declare (ignore object))
                      (incf *printer-calls*)
                      (if *printer-fails* (error "printer failure")
                          (write-string "<printed by method>" stream)))
                    (defun stop-preview (value)
                      (declare (optimize (debug 3)))
                      (restart-case (error "stop")
                        (recover () :report (lambda (stream)
                                              (incf *report-calls*)
                                              (write-string "Recover" stream))
                          nil))
                      (type-of value))
                    (in-package :cl-user)
                    (defparameter value 99)"#;
    repl.send(json!({"id":1,"source":source}));
    assert!(repl.response(&json!(1))["error"].is_null());
    repl.send(json!({"id":"watch","source":"(values (make-instance 'json-preview::inspected) json-preview::*printer-calls*)","render":false}));
    assert_eq!(
        repl.response(&json!("watch"))["values"],
        json!([["#<JSON-PREVIEW::INSPECTED>", "0"]])
    );
    repl.send(json!({"id":2,"source":"(json-preview::stop-preview (make-instance 'json-preview::inspected))","interactive":true}));
    let prompt = repl.recv();
    assert_eq!(prompt["event"], "debugger", "{prompt}");

    let (found, prompt) = repl.action(
        json!(3),
        &prompt,
        json!({"kind":"local","frame":0,"expression":" value "}),
    );
    let found = &found["result"]["value"];
    assert_eq!(found, &json!({"preview":"#<JSON-PREVIEW::INSPECTED>"}));

    let (unsupported, prompt) = repl.action(
        json!(4),
        &prompt,
        json!({"kind":"local","frame":0,"expression":"#.(incf json-preview::*printer-calls*)"}),
    );
    assert!(unsupported["result"]["value"].is_null(), "{unsupported}");

    let (preview, prompt) = repl.action(
        json!(5),
        &prompt,
        json!({"kind":"eval","frame":0,"source":"(values value *printer-calls* *report-calls*)","render":false}),
    );
    let preview = &preview["result"]["value"];
    assert!(preview.get("renderedValues").is_none(), "{preview}");
    assert_eq!(preview["values"][0]["preview"], found["preview"]);
    assert!(preview["values"][0]["handle"].is_u64());
    assert_eq!(preview["values"][1]["preview"], "0");
    assert_eq!(preview["values"][2]["preview"], "1");

    let (rendered, prompt) = repl.action(
        json!(6),
        &prompt,
        json!({"kind":"eval","frame":0,"source":"(values value *printer-calls*)"}),
    );
    assert_eq!(
        rendered["result"]["value"]["renderedValues"],
        json!(["<printed by method>", "0"])
    );
    let (failed, prompt) = repl.action(
        json!("failed-print"),
        &prompt,
        json!({"kind":"eval","frame":0,"source":"(values (progn (setf *printer-fails* t) value) 42)"}),
    );
    assert!(failed["result"]["error"].is_null(), "{failed}");
    let failed = &failed["result"]["value"];
    assert_eq!(failed["renderedValues"], json!([found["preview"], "42"]));
    assert!(failed["values"][0]["handle"].is_u64());
    assert_eq!(failed["printErrors"][0]["index"], 0);
    assert_eq!(
        failed["printErrors"][0]["error"]["message"],
        "printer failure"
    );
    let (empty, prompt) = repl.action(
        json!(7),
        &prompt,
        json!({"kind":"eval","frame":0,"source":"(values)"}),
    );
    assert_eq!(
        empty["result"]["value"],
        json!({"values":[], "renderedValues":[]})
    );
    repl.send(json!({"id":8,"reply":{"request":2,"token":prompt["token"],"action":{"kind":"restart","name":"recover"}}}));
    assert!(repl.response(&json!(2))["error"].is_null());

    repl.send(json!({"id":9,"source":"json-preview::*printer-calls*"}));
    assert_eq!(repl.response(&json!(9))["values"], json!([["2"]]));
    repl.finish();
}

#[test]
fn json_resume_releases_handles_only_when_the_pause_ends() {
    let mut repl = Client::new();
    repl.send(json!({"id":1,"source":"(restart-case (error \"stop\") (recover () 42))","interactive":true}));
    let prompt = repl.recv();
    assert_eq!(prompt["event"], "debugger", "{prompt}");
    let (objects, prompt) = repl.action(
        json!(2),
        &prompt,
        json!({"kind":"eval","source":"(list 7)"}),
    );
    let handle = &objects["result"]["value"]["values"][0]["handle"];
    assert!(handle.is_u64(), "{objects}");
    let (failure, prompt) = repl.action(
        json!(3),
        &prompt,
        json!({"kind":"motion","motion":"continue","release":[handle]}),
    );
    assert_eq!(failure["resumed"], false, "{failure}");
    assert!(!failure["result"]["error"].is_null(), "{failure}");
    let (inspected, prompt) = repl.action(
        json!(4),
        &prompt,
        json!({"kind":"object","object":{"action":"inspect","handle":handle}}),
    );
    assert_eq!(
        inspected["result"]["value"]["parts"][0]["value"]["preview"],
        "7"
    );

    repl.send(json!({"id":5,"reply":{"request":1,"token":prompt["token"],"action":{"kind":"motion","motion":"abort","release":[handle]}}}));
    repl.response(&json!(1));
    repl.send(json!({"id":6,"object":{"action":"inspect","handle":handle}}));
    assert!(!repl.response(&json!(6))["error"].is_null());
    repl.finish();
}

#[test]
fn json_pause_reaches_busy_evaluation_and_continues_it() {
    let mut repl = Client::new();
    repl.send(json!({"id":1,"source":"(defparameter *json-stop* nil)"}));
    assert!(repl.response(&json!(1))["error"].is_null());
    repl.send(
        json!({"id":2,"source":"(loop until *json-stop* finally (return 42))","interactive":true}),
    );
    std::thread::sleep(Duration::from_millis(50));
    repl.send(json!({"id":3,"pause":2}));
    let prompt = loop {
        let event = repl.recv();
        if event["event"] == "debugger" {
            break event;
        }
    };
    let (result, prompt) = repl.action(
        json!(4),
        &prompt,
        json!({"kind":"eval","frame":0,"source":"(setq *json-stop* t)"}),
    );
    assert!(result["result"]["error"].is_null(), "{result}");
    repl.send(json!({"id":5,"reply":{"request":2,"token":prompt["token"],"action":{"kind":"motion","motion":"continue"}}}));
    let result = repl.response(&json!(2));
    assert_eq!(result["values"], json!([["42"]]), "{result}");
    repl.finish();
}

#[test]
fn json_profile_reports_actual_function_calls() {
    let mut repl = Client::new();
    repl.send(
        json!({"id":1,"source":"(defun json-profiled (n) (loop repeat n collect (cons n n)))"}),
    );
    assert!(repl.response(&json!(1))["error"].is_null());
    repl.send(json!({"id":2,"profile":{"action":"start","sampleIntervalMicros":10}}));
    assert!(repl.response(&json!(2))["error"].is_null());
    repl.send(json!({"id":3,"source":"(json-profiled 10)"}));
    assert!(repl.response(&json!(3))["error"].is_null());
    repl.send(json!({"id":4,"profile":{"action":"stop"}}));
    let report = repl.response(&json!(4));
    assert_eq!(report["profile"]["running"], false, "{report}");
    let rows = report["profile"]["functions"].as_array().unwrap();
    assert!(
        rows.iter().any(|row| row["name"] == "JSON-PROFILED"
            && row["calls"].as_u64().is_some_and(|calls| calls > 0)
            && row["nativeCalls"].is_u64()),
        "{report}"
    );
    repl.finish();
}

#[test]
fn json_image_restore_replaces_state_and_invalidates_handles() {
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/repl-completion/json");
    std::fs::create_dir_all(&directory).unwrap();
    let temp = tempfile::tempdir_in(directory).unwrap();
    let path = temp.path().join("session.image");
    let mut repl = Client::new();
    repl.send(
        json!({"id":1,"source":"(defparameter *json-image-state* (list 42)) *json-image-state*"}),
    );
    let handle = repl.response(&json!(1))["objects"][1][0]["handle"].clone();
    repl.send(json!({"id":"unavailable","image":{"action":"restore","path":temp.path().join("missing.image")}}));
    assert!(!repl.response(&json!("unavailable"))["error"].is_null());
    repl.send(json!({"id":"preserved","object":{"action":"inspect","handle":handle}}));
    assert_eq!(
        repl.response(&json!("preserved"))["object"]["parts"][0]["value"]["preview"],
        "42"
    );
    repl.send(json!({"id":2,"image":{"action":"save","path":path}}));
    let saved = repl.response(&json!(2));
    assert!(saved["error"].is_null(), "{saved}");
    repl.send(json!({"id":3,"source":"(setf (car *json-image-state*) 73)"}));
    assert!(repl.response(&json!(3))["error"].is_null());
    repl.send(json!({"id":4,"image":{"action":"restore","path":path}}));
    let restored = repl.response(&json!(4));
    assert!(restored["error"].is_null(), "{restored}");
    assert_eq!(restored["image"]["handlesInvalidated"], true);
    repl.send(json!({"id":5,"source":"(car *json-image-state*)"}));
    assert_eq!(repl.response(&json!(5))["values"], json!([["42"]]));
    repl.send(json!({"id":6,"object":{"action":"inspect","handle":handle}}));
    assert!(!repl.response(&json!(6))["error"].is_null());
    repl.finish();
}
