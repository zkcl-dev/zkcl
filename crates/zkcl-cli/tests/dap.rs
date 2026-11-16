mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::ChildStdin;
use std::sync::mpsc;
use std::time::Duration;

use common::{ReplSession, zkcl};
use serde_json::{Value, json};

struct Client {
    session: ReplSession,
    input: Option<ChildStdin>,
    messages: mpsc::Receiver<Value>,
    saved: Vec<Value>,
    seq: u64,
}

impl Client {
    fn new() -> Self {
        let mut session = ReplSession::spawn(zkcl().arg("dap"));
        let (input, output) = session.pipes();
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            let mut input = BufReader::new(output);
            loop {
                let mut length = None;
                loop {
                    let mut header = String::new();
                    if input.read_line(&mut header).unwrap() == 0 {
                        return;
                    }
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(value) = header.strip_prefix("Content-Length:") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
                let mut body = vec![0; length.expect("DAP Content-Length")];
                input.read_exact(&mut body).unwrap();
                if sender.send(serde_json::from_slice(&body).unwrap()).is_err() {
                    return;
                }
            }
        });
        Self {
            session,
            input: Some(input),
            messages,
            saved: Vec::new(),
            seq: 0,
        }
    }

    fn send(&mut self, command: &str, arguments: Value) -> u64 {
        self.seq += 1;
        let message =
            json!({"seq":self.seq,"type":"request","command":command,"arguments":arguments});
        let bytes = serde_json::to_vec(&message).unwrap();
        let input = self.input.as_mut().unwrap();
        write!(input, "Content-Length: {}\r\n\r\n", bytes.len()).unwrap();
        input.write_all(&bytes).unwrap();
        input.flush().unwrap();
        self.seq
    }

    fn matching(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.saved.iter().position(&predicate) {
            return self.saved.remove(index);
        }
        loop {
            let message = self
                .messages
                .recv_timeout(Duration::from_secs(10))
                .expect("DAP response before timeout");
            assert!(message["seq"].is_u64(), "{message}");
            if predicate(&message) {
                return message;
            }
            self.saved.push(message);
        }
    }

    fn response(&mut self, id: u64) -> Value {
        self.matching(|message| message["type"] == "response" && message["request_seq"] == id)
    }

    fn request(&mut self, command: &str, args: Value) -> Value {
        let id = self.send(command, args);
        let response = self.response(id);
        assert_eq!(response["success"], true, "{response}");
        response["body"].clone()
    }

    fn event(&mut self, name: &str) -> Value {
        self.matching(|message| message["type"] == "event" && message["event"] == name)
    }

    fn launch(&mut self, path: &std::path::Path, entry: Option<&str>) {
        let capabilities=self.request("initialize",json!({"adapterID":"zkcl","linesStartAt1":true,"columnsStartAt1":true,"pathFormat":"path"}));
        assert_eq!(capabilities["supportsConfigurationDoneRequest"], true);
        self.request(
            "launch",
            json!({"program":path,"cwd":path.parent().unwrap(),"entry":entry}),
        );
        self.event("initialized");
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(input) = self.input.as_mut() {
            let message = json!({"seq":self.seq+1,"type":"request","command":"disconnect","arguments":{"terminateDebuggee":true}});
            if let Ok(bytes) = serde_json::to_vec(&message) {
                let _ = write!(input, "Content-Length: {}\r\n\r\n", bytes.len());
                let _ = input.write_all(&bytes);
                let _ = input.flush();
            }
        }
        self.input.take();
        self.session.wait();
    }
}

fn directory() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.agents/tmp/zkcl/repl-completion/dap");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

#[test]
fn standard_dap_client_inspects_edits_and_resumes_a_live_lisp_frame() {
    let directory = directory();
    let path = directory.path().join("source with space.lisp");
    std::fs::write(&path,"(defun dap-probe (n)\n  (declare (optimize (debug 3)))\n  (let ((sum (+ n 2)))\n    (break \"DAP 雪\")\n    (write-line (princ-to-string sum))\n    sum))\n(dap-probe 40)\n").unwrap();
    let mut client = Client::new();
    client.launch(&path, None);
    client.request("configurationDone", json!({}));
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["threadId"], 1);
    assert_eq!(
        client.request("threads", json!({}))["threads"],
        json!([{"id":1,"name":"Lisp runtime"}])
    );
    let stack = client.request("stackTrace", json!({"threadId":1}));
    let frames = stack["stackFrames"].as_array().unwrap();
    let frame = frames
        .iter()
        .find(|frame| frame["name"] == "DAP-PROBE")
        .expect("user Lisp frame");
    assert_eq!(frame["source"]["path"], json!(path.canonicalize().unwrap()));
    assert_eq!(frame["line"], 4);
    let source_reference = frame["source"]["sourceReference"].clone();
    assert!(
        source_reference.as_u64().is_some_and(|id| id > 0),
        "{frame}"
    );
    std::fs::write(
        &path,
        "; changed on disk after this definition was compiled\n",
    )
    .unwrap();
    let source = client.request("source", json!({"sourceReference":source_reference}));
    assert!(
        source["content"].as_str().unwrap().contains("DAP 雪"),
        "{source}"
    );
    let scopes = client.request("scopes", json!({"frameId":frame["id"]}));
    let reference = scopes["scopes"][0]["variablesReference"].clone();
    let variables = client.request("variables", json!({"variablesReference":reference}));
    assert!(
        variables["variables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "SUM" && entry["value"] == "42"),
        "{variables}"
    );
    assert_eq!(
        client.request(
            "evaluate",
            json!({"frameId":frame["id"],"expression":"sum","context":"watch"})
        )["result"],
        "42"
    );
    assert_eq!(
        client.request(
            "setVariable",
            json!({"variablesReference":reference,"name":"SUM","value":"73"})
        )["value"],
        "73"
    );
    assert_eq!(
        client.request(
            "evaluate",
            json!({"frameId":frame["id"],"expression":"sum"})
        )["result"],
        "73"
    );
    client.request("continue", json!({"threadId":1}));
    client.event("continued");
    let output = client.matching(|message| {
        message["event"] == "output"
            && message["body"]["output"]
                .as_str()
                .is_some_and(|text| text.contains("73"))
    });
    assert_eq!(output["body"]["category"], "stdout");
    client.event("terminated");
    let stale = client.send("variables", json!({"variablesReference":reference}));
    assert_eq!(client.response(stale)["success"], false);
}

#[test]
fn dap_console_and_watch_present_live_values_and_hover_only_reads_locals() {
    let directory = directory();
    let path = directory.path().join("evaluation.lisp");
    std::fs::write(
        &path,
        r#"
        (defparameter *dap-counter* 0)
        (defclass dap-printed () ())
        (defmethod print-object ((sample dap-printed) stream)
          (incf *dap-counter*)
          (write-string "printed-by-user" stream)
          sample)
        (defclass dap-unprintable () ((payload :initform 23)))
        (defmethod print-object ((sample dap-unprintable) stream)
          (declare (ignore sample stream))
          (error "dap printer failure"))
        (defun dap-evaluation (sample)
          (declare (optimize (debug 3)))
          (break "presentation probe")
          sample)
        (dap-evaluation (make-instance 'dap-printed))
        "#,
    )
    .unwrap();
    let mut client = Client::new();
    client.launch(&path, None);
    client.request("configurationDone", json!({}));
    client.event("stopped");
    let stack = client.request("stackTrace", json!({"threadId":1}));
    let frame_id = stack["stackFrames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["name"] == "DAP-EVALUATION")
        .unwrap()["id"]
        .as_u64()
        .unwrap();

    let hovered = client.request(
        "evaluate",
        json!({"context":"hover","expression":"sample","frameId":frame_id}),
    );
    assert_eq!(hovered["variablesReference"], 0);
    let watched = client.request(
        "evaluate",
        json!({"context":"watch","expression":"sample","frameId":frame_id}),
    );
    assert_eq!(watched["result"], hovered["result"]);
    assert!(watched["variablesReference"].as_u64().unwrap() > 0);
    for (context, expression, frame_id) in [
        ("hover", "(incf *dap-counter*)", frame_id),
        ("hover", "#.(incf *dap-counter*)", frame_id),
        ("repl", "(incf *dap-counter*)", frame_id + 100),
    ] {
        let rejected = client.send(
            "evaluate",
            json!({"context":context,"expression":expression,"frameId":frame_id}),
        );
        assert_eq!(client.response(rejected)["success"], false);
    }
    let counter = json!({"context":"watch","expression":"*dap-counter*","frameId":frame_id});
    assert_eq!(client.request("evaluate", counter.clone())["result"], "0");
    assert_eq!(
        client.request(
            "evaluate",
            json!({"context":"repl","expression":"sample","frameId":frame_id}),
        )["result"],
        "printed-by-user"
    );
    assert_eq!(client.request("evaluate", counter)["result"], "1");
    let unprintable = client.request(
        "evaluate",
        json!({"context":"repl","expression":"(make-instance 'dap-unprintable)","frameId":frame_id}),
    );
    let printer_error = client.matching(|message| {
        message["event"] == "output"
            && message["body"]["output"]
                .as_str()
                .is_some_and(|text| text.contains("dap printer failure"))
    });
    assert_eq!(printer_error["body"]["category"], "stderr");
    let slots = client.request(
        "variables",
        json!({"variablesReference":unprintable["variablesReference"]}),
    );
    assert_eq!(slots["variables"][0]["name"], "PAYLOAD");
    assert_eq!(slots["variables"][0]["value"], "23");

    let integer = format!("1{}3", "2".repeat(249));
    let result = client.request(
        "evaluate",
        json!({"context":"repl","expression":format!("(values {integer} (list 2 3))"),"frameId":frame_id}),
    );
    assert_eq!(result["result"], format!("{integer}; (2 3)"));
    assert_eq!(result["indexedVariables"], 2);
    let reference = &result["variablesReference"];
    let first = client.request(
        "variables",
        json!({"variablesReference":reference,"filter":"indexed","start":0,"count":1}),
    );
    let preview = first["variables"][0]["value"].as_str().unwrap();
    assert!(preview.ends_with('…') && integer.starts_with(preview.trim_end_matches('…')));
    let second = client.request(
        "variables",
        json!({"variablesReference":reference,"filter":"indexed","start":1,"count":1}),
    );
    assert_eq!(second["variables"][0]["name"], "[1]");
    assert_eq!(second["variables"][0]["value"], "(2 3)");
    let cons = client.request(
        "variables",
        json!({"variablesReference":second["variables"][0]["variablesReference"]}),
    );
    assert_eq!(cons["variables"][0]["value"], "2");
    assert_eq!(
        client.request(
            "evaluate",
            json!({"context":"repl","expression":"(values)","frameId":frame_id}),
        ),
        json!({"result":"<no values>","variablesReference":0})
    );
    client.request("continue", json!({"threadId":1}));
    client.event("terminated");
    let stale = client.send("variables", json!({"variablesReference":reference}));
    assert_eq!(client.response(stale)["success"], false);
}

#[test]
fn dap_console_keeps_results_from_each_top_level_form() {
    let directory = directory();
    let path = directory.path().join("empty.lisp");
    std::fs::write(&path, "").unwrap();
    let mut client = Client::new();
    client.launch(&path, Some("(break \"entry probe\")"));

    let integer = format!("1{}3", "2".repeat(249));
    let result = client.request(
        "evaluate",
        json!({"context":"repl","expression":format!("(values {integer} 3)\n(values)\n(list 4 5)")}),
    );
    assert_eq!(
        result["result"],
        format!("{integer}; 3\n<no values>\n(4 5)")
    );
    assert_eq!(result["indexedVariables"], 3);
    let watch = client.request("evaluate", json!({"context":"watch","expression":integer}));
    let preview = watch["result"].as_str().unwrap();
    assert!(preview.ends_with('…') && integer.starts_with(preview.trim_end_matches('…')));

    client.request("configurationDone", json!({}));
    let stop = client.event("stopped");
    assert!(
        stop["body"]["description"]
            .as_str()
            .is_some_and(|description| description.contains("entry probe")),
        "{stop}"
    );
    let evaluated = client.request("evaluate", json!({"expression":"(list 45)"}));
    let stack = client.request("stackTrace", json!({"threadId":1}));
    let frame = stack["stackFrames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["canRestart"] == false)
        .unwrap();
    let restart = client.send("restartFrame", json!({"frameId":frame["id"]}));
    assert_eq!(client.response(restart)["success"], false);
    let parts = client.request(
        "variables",
        json!({"variablesReference":evaluated["variablesReference"]}),
    );
    assert_eq!(parts["variables"][0]["value"], "45");
    client.request("continue", json!({"threadId":1}));
    client.event("terminated");
}

#[test]
fn dap_pause_reaches_a_busy_runtime_without_waiting_for_evaluation() {
    let directory = directory();
    let path = directory.path().join("busy.lisp");
    std::fs::write(&path,"(defun dap-busy () (declare (optimize (debug 3))) (write-line \"ready-for-pause\") (let ((n 0)) (loop (incf n)))) (dap-busy)").unwrap();
    let mut client = Client::new();
    client.launch(&path, None);
    client.request("configurationDone", json!({}));
    client.matching(|message| {
        message["event"] == "output"
            && message["body"]["output"]
                .as_str()
                .is_some_and(|text| text.contains("ready-for-pause"))
    });
    client.request("pause", json!({"threadId":1}));
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "pause", "{stopped}");
    let stack = client.request("stackTrace", json!({"threadId":1}));
    assert!(
        stack["stackFrames"]
            .as_array()
            .unwrap()
            .iter()
            .any(|frame| frame["name"] == "DAP-BUSY"),
        "{stack}"
    );
    client.request("terminate", json!({}));
    client.input.take();
}

#[test]
fn dap_installs_source_breakpoints_before_load_and_restarts_and_steps_frames() {
    let directory = directory();
    let path = directory.path().join("steps.lisp");
    std::fs::write(&path,"(defun dap-steps (n)\n  (declare (optimize (debug 3)))\n  (let ((sum (+ n 2)))\n    (write-line \"probe-start\")\n    (write-line (princ-to-string sum))\n    sum))\n(dap-steps 40)\n").unwrap();
    let mut client = Client::new();
    client.launch(&path, None);
    let points = client.request(
        "setBreakpoints",
        json!({"source":{"path":path},"breakpoints":[{"line":4}]}),
    );
    assert_eq!(points["breakpoints"].as_array().unwrap().len(), 1);
    assert!(points["breakpoints"][0]["id"].is_u64(), "{points}");
    client.request("configurationDone", json!({}));
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "breakpoint", "{stopped}");
    client.request(
        "setBreakpoints",
        json!({"source":{"path":path},"breakpoints":[]}),
    );
    client.request("next", json!({"threadId":1}));
    client.event("continued");
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "step", "{stopped}");
    let stack = client.request("stackTrace", json!({"threadId":1}));
    let frame = stack["stackFrames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["name"] == "DAP-STEPS")
        .unwrap();
    client.request("restartFrame", json!({"frameId":frame["id"]}));
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "restart", "{stopped}");
    let stack = client.request("stackTrace", json!({"threadId":1}));
    let frame = stack["stackFrames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|frame| frame["name"] == "DAP-STEPS")
        .unwrap();
    assert_eq!(
        client.request("evaluate", json!({"frameId":frame["id"],"expression":"n"}))["result"],
        "40"
    );
    client.request("stepIn", json!({"threadId":1}));
    let stopped = client.event("stopped");
    assert_eq!(stopped["body"]["reason"], "step", "{stopped}");
    client.request("stepOut", json!({"threadId":1}));
    let result = client
        .matching(|message| message["event"] == "stopped" || message["event"] == "terminated");
    if result["event"] == "stopped" {
        client.request("continue", json!({"threadId":1}));
        client.event("terminated");
    }
}
