use std::{
    collections::HashMap,
    io::{self, BufReader},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::Duration,
};

use lsp_server::{ErrorCode, Message, Notification, Request, Response};
use lsp_types::PublishDiagnosticsParams;
use serde_json::{Value, json};

const URI: &str = "untitled:zkcl-protocol-test.lisp";

#[test]
fn stdio_validates_unsaved_lockfile_data_and_clears_repaired_errors() {
    const LOCK_URI: &str = "file:///zkcl-protocol-unsaved/zkcl.lock";
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"capabilities": {
            "textDocument": {"publishDiagnostics": {"versionSupport": true}},
        }}),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": LOCK_URI, "languageId": "common-lisp", "version": 1,
            "text": "(lockfile (format 3))",
        }}),
    );
    let invalid = server.diagnostics_for(LOCK_URI);
    assert_eq!(invalid.version, Some(1));
    let [diagnostic] = invalid.diagnostics.as_slice() else {
        panic!("{invalid:?}")
    };
    assert_eq!(diagnostic.message, "unsupported lockfile format version");
    assert_eq!(
        diagnostic.code,
        Some(lsp_types::NumberOrString::String("project/lockfile".into()))
    );
    assert_eq!(
        diagnostic.range,
        lsp_types::Range::new(
            lsp_types::Position::new(0, 18),
            lsp_types::Position::new(0, 19)
        )
    );
    server.notify("textDocument/didChange", json!({
        "textDocument": {"uri": LOCK_URI, "version": 2},
        "contentChanges": [{"text": "(lockfile (format 2) (configuration (project \"acme/example\") (target \"library\") (roots)))"}],
    }));
    let repaired = server.diagnostics_for(LOCK_URI);
    assert_eq!(repaired.version, Some(2));
    assert!(repaired.diagnostics.is_empty());
    server.request(2, "shutdown", Value::Null);
    server.response(2).response_result.unwrap();
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

#[test]
fn stdio_validates_unsaved_project_manifests_with_snapshot_coordinates() {
    const MANIFEST_URI: &str = "file:///zkcl-protocol-unsaved/zkcl.sexp";
    let prefix = "(project \"acme/example\" (version \"1.0.0\") (license \"😀\") ";
    for (encoding, column) in [
        ("utf-16", prefix.encode_utf16().count()),
        ("utf-8", prefix.len()),
    ] {
        let mut server = Server::start();
        server.request(
            1,
            "initialize",
            json!({"capabilities": {
                "general": {"positionEncodings": [encoding]},
                "textDocument": {"publishDiagnostics": {"versionSupport": true}},
            }}),
        );
        server.response(1).response_result.unwrap();
        server.notify("initialized", json!({}));
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": MANIFEST_URI, "languageId": "common-lisp", "version": 1,
                "text": format!("{prefix}(mystery \"x\"))"),
            }}),
        );
        let invalid = server.diagnostics_for(MANIFEST_URI);
        assert_eq!(invalid.version, Some(1));
        let [diagnostic] = invalid.diagnostics.as_slice() else {
            panic!("{invalid:?}")
        };
        assert_eq!(diagnostic.message, "unknown project field \"mystery\"");
        assert_eq!(
            diagnostic.code,
            Some(lsp_types::NumberOrString::String("project/manifest".into()))
        );
        assert_eq!(
            diagnostic.range.start,
            lsp_types::Position::new(0, column as u32)
        );
        assert_eq!(diagnostic.range.end.character, column as u32 + 13);

        // Repair only the erroneous range in the unsaved snapshot.
        server.notify("textDocument/didChange", json!({
            "textDocument": {"uri": MANIFEST_URI, "version": 2},
            "contentChanges": [{"range": diagnostic.range, "text": "(library (files \"src/main.lisp\"))"}],
        }));
        let repaired = server.diagnostics_for(MANIFEST_URI);
        assert_eq!(repaired.version, Some(2));
        assert!(repaired.diagnostics.is_empty());

        // Project data rejects reader forms without invoking the Lisp reader.
        server.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": MANIFEST_URI, "version": 3},
                "contentChanges": [{"text": "#.(error \"must stay inert\")"}],
            }),
        );
        let reader_form = server.diagnostics_for(MANIFEST_URI);
        assert_eq!(reader_form.version, Some(3));
        assert_eq!(reader_form.diagnostics.len(), 1);
        assert!(reader_form.diagnostics[0].message.contains("reader forms"));

        server.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": MANIFEST_URI, "version": 4},
                "contentChanges": [{"text": "(project"}],
            }),
        );
        let incomplete = server.diagnostics_for(MANIFEST_URI);
        assert_eq!(incomplete.version, Some(4));
        assert_eq!(incomplete.diagnostics.len(), 1);
        assert_eq!(
            incomplete.diagnostics[0].message,
            "missing closing parenthesis"
        );

        server.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": MANIFEST_URI}}),
        );
        let closed = server.diagnostics_for(MANIFEST_URI);
        assert!(closed.diagnostics.is_empty());
        assert_eq!(closed.version, None);

        server.request(2, "shutdown", Value::Null);
        server.response(2).response_result.unwrap();
        server.notify("exit", Value::Null);
        assert!(server.wait());
    }
}

#[test]
fn stdio_publishes_diagnostics_for_the_open_version_in_the_negotiated_encoding() {
    for (encodings, expected, column) in [
        (None, "utf-16", 6),
        (Some(&["utf-8", "utf-16"][..]), "utf-8", 8),
        (Some(&["utf-32"][..]), "utf-32", 5),
    ] {
        let mut server = Server::start();
        server.request(0, "textDocument/hover", Value::Null);
        assert_eq!(
            server.response(0).response_result.unwrap_err().code,
            ErrorCode::ServerNotInitialized as i32,
        );

        let mut capabilities = json!({});
        if let Some(encodings) = encodings {
            capabilities["general"] = json!({"positionEncodings": encodings});
        }
        server.request(
            1,
            "initialize",
            json!({"processId": null, "capabilities": capabilities}),
        );
        let result = server.response(1).response_result.unwrap();
        assert_eq!(result["capabilities"]["positionEncoding"], expected);
        assert_eq!(result["capabilities"]["renameProvider"], true);
        assert_eq!(
            result["capabilities"]["textDocumentSync"],
            json!({"openClose": true, "change": 2, "save": true}),
        );
        server.notify("initialized", json!({}));

        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": URI, "languageId": "common-lisp", "version": 1,
                "text": "\"😀\"  )\r\n)",
            }}),
        );
        let open = server.diagnostics();
        assert_eq!(open.version, Some(1));
        assert_eq!(open.diagnostics.len(), 2);
        let first = &open.diagnostics[0];
        assert_eq!(first.message, "unexpected closing parenthesis");
        assert_eq!(first.range.start.line, 0);
        assert_eq!(first.range.start.character, column);
        assert_eq!(first.range.end.character, column + 1);
        assert_eq!(open.diagnostics[1].range.start.line, 1);
        assert_eq!(open.diagnostics[1].range.start.character, 0);

        server.change(6, "#.(error \"must stay inert\")");
        let repaired = server.diagnostics();
        assert_eq!(repaired.version, Some(6));
        assert!(repaired.diagnostics.is_empty());

        server.change(2, ")");
        server.request(2, "textDocument/unknownMethod", Value::Null);
        assert_eq!(
            server.response(2).response_result.unwrap_err().code,
            ErrorCode::MethodNotFound as i32,
        );

        server.change(8, ")");
        let invalid = server.diagnostics();
        assert_eq!(invalid.version, Some(8));
        assert_eq!(invalid.diagnostics.len(), 1);
        server.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": URI}}),
        );
        let closed = server.diagnostics();
        assert!(closed.diagnostics.is_empty());
        assert_eq!(closed.version, None);

        server.change(9, ")");
        server.request(3, "shutdown", Value::Null);
        assert_eq!(server.response(3).response_result.unwrap(), Value::Null);

        server.request(4, "textDocument/hover", Value::Null);
        assert_eq!(
            server.response(4).response_result.unwrap_err().code,
            ErrorCode::InvalidRequest as i32,
        );
        server.notify("exit", Value::Null);
        assert!(server.wait());
    }
}

#[test]
fn stdio_reports_structural_errors_before_the_combined_diagnostics() {
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"capabilities": {
            "textDocument": {"publishDiagnostics": {"versionSupport": true}},
        }}),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 1,
            "text": "(defun sample (unused) 1)\r\n\"😀\"  )",
        }}),
    );

    let structural = server.diagnostics();
    assert_eq!(structural.version, Some(1));
    let [error] = structural.diagnostics.as_slice() else {
        panic!(
            "expected one structural error, got {:?}",
            structural.diagnostics
        );
    };
    assert_eq!(error.message, "unexpected closing parenthesis");
    assert_eq!(error.range.start, lsp_types::Position::new(1, 6));

    let combined = server.diagnostics();
    assert_eq!(combined.version, Some(1));
    let [repeated_error, warning] = combined.diagnostics.as_slice() else {
        panic!(
            "expected one error and one warning, got {:?}",
            combined.diagnostics
        );
    };
    assert_eq!(repeated_error, error);
    assert_eq!(warning.message, "unused lexical variable `unused`");

    server.change(2, "(defun sample (used) used)\r\n\"😀\"");
    let repaired = server.diagnostics();
    assert_eq!(repaired.version, Some(2));
    assert!(repaired.diagnostics.is_empty());
    server.request(2, "shutdown", Value::Null);
    assert_eq!(server.response(2).response_result.unwrap(), Value::Null);
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

#[test]
fn stdio_indexes_generated_structure_functions_after_an_edit() {
    const LIBRARY: &str = "untitled:zkcl-protocol-structure.lisp";
    const PREDICATE: &str = "(defpackage #:demo (:use #:cl))\n(in-package :demo)\n(defstruct (record (:predicate record-check)) payload)";
    const CONSTRUCTOR: &str = "(defpackage #:demo (:use #:cl))\n(in-package :demo)\n; 🧪\n(defstruct (record (:constructor record-check ())) payload)";
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"capabilities": {
            "textDocument": {"publishDiagnostics": {"versionSupport": true}},
        }}),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    for (uri, text) in [
        (LIBRARY, PREDICATE),
        (URI, "(in-package :demo)\n(record-check)"),
    ] {
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "text": text, "languageId": "common-lisp", "version": 1,
            }}),
        );
        let diagnostics = server.diagnostics_for(uri);
        if uri == URI {
            let [diagnostic] = diagnostics.diagnostics.as_slice() else {
                panic!("{diagnostics:?}");
            };
            assert_eq!(
                diagnostic.code,
                Some(lsp_types::NumberOrString::String(
                    "correctness/arity-mismatch".into()
                ))
            );
        } else {
            assert!(diagnostics.diagnostics.is_empty(), "{diagnostics:?}");
        }
    }
    let definition = json!({"textDocument": {"uri": URI}, "position": {"line": 1, "character": 3}});
    server.request(2, "textDocument/definition", definition.clone());
    let before = server.response(2).response_result.unwrap();
    assert_eq!(before["uri"], LIBRARY);
    assert_eq!(
        before["range"]["start"],
        json!({"line": 2, "character": 31})
    );

    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": LIBRARY, "version": 2},
            "contentChanges": [{"text": CONSTRUCTOR}],
        }),
    );
    assert_eq!(server.diagnostics_for(LIBRARY).version, Some(2));
    let caller = server.diagnostics();
    assert_eq!(caller.version, Some(1));
    assert!(caller.diagnostics.is_empty(), "{caller:?}");
    server.request(3, "textDocument/definition", definition);
    let after = server.response(3).response_result.unwrap();
    assert_eq!(after["uri"], LIBRARY);
    assert_eq!(after["range"]["start"], json!({"line": 3, "character": 33}));
    server.request(4, "shutdown", Value::Null);
    server.response(4).response_result.unwrap();
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

#[test]
fn stdio_refreshes_caller_diagnostics_when_an_open_definition_changes() {
    const LIBRARY: &str = "untitled:zkcl-protocol-library.lisp";
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"capabilities": {
            "textDocument": {"publishDiagnostics": {"versionSupport": true}},
        }}),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 7,
            "text": "(in-package :demo)\n(dependency 42)",
        }}),
    );
    assert!(server.diagnostics().diagnostics.is_empty());

    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": LIBRARY, "languageId": "common-lisp", "version": 1,
            "text": "(in-package :demo)\n(defun dependency () nil)",
        }}),
    );
    assert!(server.diagnostics_for(LIBRARY).diagnostics.is_empty());
    let caller = server.diagnostics();
    assert_eq!(caller.version, Some(7));
    assert_eq!(caller.diagnostics.len(), 1);
    assert_eq!(
        caller.diagnostics[0].message,
        "dependency takes at most 0 arguments, found 1"
    );
    assert_eq!(caller.diagnostics[0].range.start.line, 1);
    let definition = &caller.diagnostics[0].related_information.as_ref().unwrap()[0].location;
    assert_eq!(definition.uri.as_str(), LIBRARY);
    assert_eq!(definition.range.start, lsp_types::Position::new(1, 7));

    for (version, definition, caller_warning) in [
        (2, "(defun dependency (x) x)", false),
        (3, "(defun dependency () nil)", true),
        (4, "; Shift the definition\n(defun dependency () nil)", true),
    ] {
        server.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": LIBRARY, "version": version},
                "contentChanges": [{"text": format!("(in-package :demo)\n{definition}")}],
            }),
        );
        assert_eq!(server.diagnostics_for(LIBRARY).version, Some(version));
        let caller = server.diagnostics();
        assert_eq!(caller.version, Some(7));
        assert_eq!(caller.diagnostics.len(), usize::from(caller_warning));
        if caller_warning {
            let location = &caller.diagnostics[0].related_information.as_ref().unwrap()[0].location;
            assert_eq!(location.uri.as_str(), LIBRARY);
            assert_eq!(
                location.range.start,
                lsp_types::Position::new(if version == 4 { 2 } else { 1 }, 7)
            );
        }
    }

    server.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": LIBRARY}}),
    );
    assert_eq!(server.diagnostics_for(LIBRARY).version, None);
    let caller = server.diagnostics();
    assert_eq!(caller.version, Some(7));
    assert!(caller.diagnostics.is_empty());

    server.request(2, "shutdown", Value::Null);
    assert_eq!(server.response(2).response_result.unwrap(), Value::Null);
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

#[test]
fn stdio_rechecks_macro_callers_after_a_template_only_edit() {
    const MACROS: &str = "untitled:zkcl-protocol-macros.lisp";
    const BEFORE: &str = "(defmacro finish-report (&body forms) `(progn ,@forms :done))";
    const AFTER: &str = "(defmacro finish-report (&body forms) `(progn :done ,@forms))";
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"capabilities": {
            "textDocument": {"publishDiagnostics": {"versionSupport":true}}
        }}),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri": URI, "languageId":"common-lisp", "version":7,
            "text":"(defun run (rows) (finish-report (mapcar #'print rows)))"
        }}),
    );
    assert!(server.diagnostics().diagnostics.is_empty());
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri": MACROS, "languageId":"common-lisp", "version":1, "text":BEFORE
        }}),
    );
    assert!(server.diagnostics_for(MACROS).diagnostics.is_empty());
    let before = server.diagnostics();
    assert_eq!(before.version, Some(7));
    let [diagnostic] = before.diagnostics.as_slice() else {
        panic!("{before:?}")
    };
    assert_eq!(
        diagnostic.code,
        Some(lsp_types::NumberOrString::String(
            "perf/mapcar-result-discarded".into()
        ))
    );
    let location = &diagnostic.related_information.as_ref().unwrap()[0].location;
    assert_eq!(location.uri.as_str(), MACROS);
    assert_eq!(
        location.range.start.character as usize,
        BEFORE.find(",@forms").unwrap()
    );

    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument":{"uri":MACROS, "version":2}, "contentChanges":[{"text":AFTER}]
        }),
    );
    assert_eq!(server.diagnostics_for(MACROS).version, Some(2));
    let after = server.diagnostics();
    assert_eq!(after.version, Some(7));
    assert!(after.diagnostics.is_empty(), "{after:?}");
    finish_editor_session(server, 2);
}

#[test]
fn stdio_outlines_the_current_buffer_in_the_client_symbol_shape() {
    for (hierarchical, encoding, start, name_start, end) in
        [(true, "utf-16", 14, 21, 34), (false, "utf-8", 16, 23, 36)]
    {
        let mut server = Server::start();
        server.request(
            1,
            "initialize",
            json!({"processId": null, "capabilities": {
                "general": {"positionEncodings": [encoding]},
                "textDocument": {"documentSymbol": {"hierarchicalDocumentSymbolSupport": hierarchical}},
            }}),
        );
        let result = server.response(1).response_result.unwrap();
        assert_eq!(result["capabilities"]["documentSymbolProvider"], true);
        server.notify("initialized", json!({}));
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": URI, "languageId": "common-lisp", "version": 1,
                "text": "  (progn \"😀\" (defun visible () 1))\r\n\
                         '(defun quoted () 2)\r\n\
                         #+unknown (defun conditional () 3)\r\n\
                         (defvar *flag* 4)",
            }}),
        );
        assert!(server.diagnostics().diagnostics.is_empty());

        let params = json!({"textDocument": {"uri": URI}});
        server.request(2, "textDocument/documentSymbol", params.clone());
        let symbols = server.response(2).response_result.unwrap();
        let symbols = symbols.as_array().unwrap();
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0]["name"], "visible");
        assert_eq!(symbols[0]["kind"], 12);
        assert_eq!(symbols[1]["name"], "*flag*");
        assert_eq!(symbols[1]["kind"], 13);
        let range = json!({
            "start": {"line": 0, "character": start},
            "end": {"line": 0, "character": end},
        });
        if hierarchical {
            assert_eq!(symbols[0]["range"], range);
            assert_eq!(
                symbols[0]["selectionRange"],
                json!({
                    "start": {"line": 0, "character": name_start},
                    "end": {"line": 0, "character": name_start + 7},
                }),
            );
        } else {
            assert_eq!(symbols[0]["location"], json!({"uri": URI, "range": range}));
            assert!(symbols[0].get("selectionRange").is_none());
        }

        server.change(3, "(defparameter updated 5)");
        assert!(server.diagnostics().diagnostics.is_empty());
        server.request(3, "textDocument/documentSymbol", params.clone());
        let symbols = server.response(3).response_result.unwrap();
        assert_eq!(symbols.as_array().unwrap().len(), 1);
        assert_eq!(symbols[0]["name"], "updated");

        server.request(4, "textDocument/documentSymbol", Value::Null);
        assert_eq!(
            server.response(4).response_result.unwrap_err().code,
            ErrorCode::InvalidParams as i32,
        );

        server.notify("textDocument/didClose", params.clone());
        assert!(server.diagnostics().diagnostics.is_empty());
        server.request(5, "textDocument/documentSymbol", params);
        assert_eq!(server.response(5).response_result.unwrap(), Value::Null);

        server.request(6, "shutdown", Value::Null);
        assert_eq!(server.response(6).response_result.unwrap(), Value::Null);
        server.notify("exit", Value::Null);
        assert!(server.wait());
    }
}

#[test]
fn stdio_applies_incremental_batches_before_publishing_the_new_snapshot() {
    for (encoding, name_start, invalid_position) in [
        ("utf-8", 21, (0, 9)),
        ("utf-16", 19, (0, 9)),
        ("utf-32", 18, (50, 0)),
    ] {
        let mut server = Server::start();
        server.request(
            1,
            "initialize",
            json!({"processId": null, "capabilities": {
                "general": {"positionEncodings": [encoding]},
                "textDocument": {
                    "publishDiagnostics": {"versionSupport": true},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                },
            }}),
        );
        let initialized = server.response(1).response_result.unwrap();
        assert_eq!(initialized["capabilities"]["positionEncoding"], encoding);
        server.notify("initialized", json!({}));
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": URI, "languageId": "common-lisp", "version": 1,
                "text": "(progn \"😀\" (defun old () 1))\r\n(defun stale () 2)",
            }}),
        );
        assert!(server.diagnostics().diagnostics.is_empty());

        let line_end = name_start + "old () 1))".len() as u32;
        server.request(
            10,
            "textDocument/selectionRange",
            json!({
                "textDocument": {"uri": URI},
                "positions": [
                    {"line": 0, "character": line_end},
                    {"line": 0, "character": 1000},
                ],
            }),
        );
        let result = server.response(10).response_result.unwrap();
        let [at_end, past_end] = result.as_array().unwrap().as_slice() else {
            panic!("expected one selection range per position, got {result}");
        };
        assert_eq!(at_end, past_end);

        server.changes(
            5,
            &[
                ranged_change((0, name_start), (0, name_start + 3), "renamed"),
                ranged_change((0, 1000), (0, 1000), ")"),
                ranged_change((0, 1000), (1, 1000), "\r(defun second () 2)\n"),
                ranged_change((2, 0), (2, 0), "(defvar *third* 3)"),
            ],
        );
        let diagnostics = server.diagnostics();
        assert_eq!(diagnostics.version, Some(5));
        assert_eq!(diagnostics.diagnostics.len(), 1);
        let diagnostic = &diagnostics.diagnostics[0];
        let extra_close = name_start + 14;
        assert_eq!(diagnostic.message, "unexpected closing parenthesis");
        assert_eq!(
            diagnostic.range.start,
            lsp_types::Position::new(0, extra_close)
        );
        assert_eq!(
            diagnostic.range.end,
            lsp_types::Position::new(0, extra_close + 1)
        );

        let params = json!({"textDocument": {"uri": URI}});
        server.request(2, "textDocument/documentSymbol", params.clone());
        let symbols = server.response(2).response_result.unwrap();
        assert_eq!(symbols.as_array().unwrap().len(), 3);
        assert_eq!(symbols[0]["name"], "renamed");
        assert_eq!(
            symbols[0]["selectionRange"],
            json!({
                "start": {"line": 0, "character": name_start},
                "end": {"line": 0, "character": name_start + 7},
            })
        );
        assert_eq!(symbols[1]["name"], "second");
        assert_eq!(symbols[1]["selectionRange"]["start"]["line"], 1);
        assert_eq!(symbols[2]["name"], "*third*");
        assert_eq!(symbols[2]["selectionRange"]["start"]["line"], 2);

        server.changes(4, &[ranged_change((0, 0), (0, 8), "")]);
        server.change(5, "(defun duplicate () 0)");
        server.changes(
            6,
            &[
                ranged_change((0, name_start), (0, name_start + 7), "partial"),
                ranged_change(invalid_position, invalid_position, "unusable"),
            ],
        );
        server.request(3, "textDocument/documentSymbol", params.clone());
        assert_eq!(server.response(3).response_result.unwrap(), symbols);

        server.changes(
            6,
            &[
                ranged_change((0, name_start), (0, name_start + 7), "superseded"),
                json!({"text": "\r\n(defun base () 1)\n)"}),
                ranged_change((1, 7), (1, 11), "final"),
                ranged_change((2, 0), (2, 1), ""),
            ],
        );
        let diagnostics = server.diagnostics();
        assert_eq!(diagnostics.version, Some(6));
        assert!(diagnostics.diagnostics.is_empty());
        server.request(4, "textDocument/documentSymbol", params);
        let symbols = server.response(4).response_result.unwrap();
        assert_eq!(symbols.as_array().unwrap().len(), 1);
        assert_eq!(symbols[0]["name"], "final");
        assert_eq!(symbols[0]["selectionRange"]["start"]["line"], 1);

        server.request(5, "shutdown", Value::Null);
        assert_eq!(server.response(5).response_result.unwrap(), Value::Null);
        server.notify("exit", Value::Null);
        assert!(server.wait());
    }
}

#[test]
fn stdio_definition_tracks_the_open_library_revision() {
    const LIB: &str = "file:///workspace/library.lisp";
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"processId": null, "capabilities": {}}),
    );
    let initialized = server.response(1).response_result.unwrap();
    assert_eq!(initialized["capabilities"]["definitionProvider"], true);
    assert_eq!(initialized["capabilities"]["positionEncoding"], "utf-16");
    server.notify("initialized", json!({}));

    for (uri, text) in [
        (
            LIB,
            "(defpackage :demo (:use :cl) (:export #:|work:it|))\r\n\
               (in-package :demo)\r\n\
               (progn \"😀\" (defun |work:it| () nil))",
        ),
        (URI, "(in-package :local)\n(values \"😀\" (demo:|work:it|))"),
    ] {
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": "common-lisp", "version": 1, "text": text,
            }}),
        );
        assert!(server.diagnostics_for(uri).diagnostics.is_empty());
    }
    let destination = |character| {
        json!({
            "uri": LIB,
            "range": {
                "start": {"line": 2, "character": character},
                "end": {"line": 2, "character": character + 9},
            },
        })
    };
    let params = json!({
        "textDocument": {"uri": URI}, "position": {"line": 1, "character": 21},
    });
    server.request(2, "textDocument/definition", params.clone());
    assert_eq!(server.response(2).response_result.unwrap(), destination(19));

    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": LIB, "version": 2},
            "contentChanges": [ranged_change((2, 19), (2, 19), "  ")],
        }),
    );
    assert!(server.diagnostics_for(LIB).diagnostics.is_empty());
    server.request(3, "textDocument/definition", params.clone());
    assert_eq!(server.response(3).response_result.unwrap(), destination(21));

    server.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": LIB}}),
    );
    assert!(server.diagnostics_for(LIB).diagnostics.is_empty());
    server.request(4, "textDocument/definition", params);
    assert_eq!(server.response(4).response_result.unwrap(), Value::Null);
    server.request(5, "textDocument/definition", Value::Null);
    assert_eq!(
        server.response(5).response_result.unwrap_err().code,
        ErrorCode::InvalidParams as i32
    );

    server.request(6, "shutdown", Value::Null);
    assert_eq!(server.response(6).response_result.unwrap(), Value::Null);
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

#[test]
fn stdio_exit_without_shutdown_uses_a_failure_status() {
    let mut server = Server::start();
    server.notify("exit", Value::Null);
    assert!(!server.wait());
}

#[test]
fn stdio_qualified_rename_succeeds_after_invalid_requests() {
    const LIBRARY: &str = "file:///rename-library.lisp";
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"processId": null, "capabilities": {
            "textDocument": {"rename": {"prepareSupport": true}}
        }}),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));

    let library = "(defpackage :lib (:use :cl) (:export :old-name))\n\
                   (in-package :lib)\n(defun old-name () 1)";
    let caller = "(in-package :lib)\n(flet ((taken () 2)) (list \"😀\" (lib:old-name)))";
    for (uri, text) in [(LIBRARY, library), (URI, caller)] {
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": "common-lisp", "version": 1, "text": text,
            }}),
        );
        assert!(server.diagnostics_for(uri).diagnostics.is_empty());
    }

    let stem = caller
        .split_once('\n')
        .unwrap()
        .1
        .split(":old-name")
        .next()
        .unwrap();
    let start = stem.encode_utf16().count() + 1;
    let position = json!({"line": 1, "character": start + 1});
    server.request(
        2,
        "textDocument/prepareRename",
        json!({"textDocument": {"uri": URI}, "position": position}),
    );
    assert_eq!(
        server.response(2).response_result.unwrap(),
        json!({
            "range": {
                "start": {"line": 1, "character": start},
                "end": {"line": 1, "character": start + "old-name".len()},
            },
            "placeholder": "old-name",
        })
    );
    for (id, name) in [(3, "taken"), (4, ":not-a-stem"), (5, "two words")] {
        server.request(
            id,
            "textDocument/rename",
            json!({"textDocument": {"uri": URI}, "position": position, "newName": name}),
        );
        assert_eq!(
            server.response(id).response_result.unwrap_err().code,
            ErrorCode::InvalidParams as i32,
        );
    }
    server.request(
        6,
        "textDocument/rename",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 100, "character": 1},
            "newName": "new-name",
        }),
    );
    assert_eq!(
        server.response(6).response_result.unwrap_err().code,
        ErrorCode::InvalidParams as i32,
    );
    server.request(
        7,
        "textDocument/rename",
        json!({"textDocument": {"uri": URI}, "position": position, "newName": "new-name"}),
    );
    let result = server.response(7).response_result.unwrap();
    let library_edits = result["changes"][LIBRARY].as_array().unwrap();
    assert_eq!(library_edits.len(), 2);
    assert!(
        library_edits
            .iter()
            .any(|edit| edit["newText"] == ":new-name")
    );
    assert!(
        library_edits
            .iter()
            .any(|edit| edit["newText"] == "new-name")
    );
    assert_eq!(
        result["changes"][URI],
        json!([{
            "range": {
                "start": {"line": 1, "character": start - "lib:".len()},
                "end": {"line": 1, "character": start + "old-name".len()},
            },
            "newText": "lib:new-name",
        }])
    );

    server.request(8, "shutdown", Value::Null);
    server.response(8).response_result.unwrap();
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

#[test]
fn stdio_control_target_ranges_use_utf16_columns() {
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"processId": null, "capabilities": {}}),
    );
    server.response(1);
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 1,
            "text": "(in-package :cl-user)\n(tagbody (list \"😀\") #x11 (go 17))\n(block nil (return 2))",
        }}),
    );
    assert!(server.diagnostics().diagnostics.is_empty());

    let selection = |line, start, end| {
        json!({
            "uri": URI,
            "range": {"start": {"line": line, "character": start},
                      "end": {"line": line, "character": end}}
        })
    };
    for (id, line, character, expected) in [
        (2, 1, 30, selection(1, 21, 25)),
        (3, 2, 12, selection(2, 7, 10)),
    ] {
        server.request(
            id,
            "textDocument/definition",
            json!({"textDocument": {"uri": URI}, "position": {"line": line, "character": character}}),
        );
        assert_eq!(server.response(id).response_result.unwrap(), expected);
    }
    server.request(
        4,
        "textDocument/references",
        json!({"textDocument": {"uri": URI}, "position": {"line": 1, "character": 21},
               "context": {"includeDeclaration": true}}),
    );
    assert_eq!(
        server.response(4).response_result.unwrap(),
        json!([selection(1, 21, 25), selection(1, 30, 32)])
    );
    server.request(5, "shutdown", Value::Null);
    assert_eq!(server.response(5).response_result.unwrap(), Value::Null);
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

fn initialize_editor_session() -> (Server, Value) {
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"processId": null, "capabilities": {
            "textDocument": {"rename": {"prepareSupport": true}}
        }}),
    );
    let init = server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    (server, init)
}

fn open_greeting_document() -> Server {
    let (mut server, _) = initialize_editor_session();
    let text = "(in-package :demo)\n(defun greet (who)\n  \"Greet someone.\"\n  (list who))\n(  greet   \"world\" )\n";
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 1, "text": text,
        }}),
    );
    assert!(server.diagnostics().diagnostics.is_empty());
    server
}

fn finish_editor_session(mut server: Server, id: i32) {
    server.request(id, "shutdown", Value::Null);
    assert_eq!(server.response(id).response_result.unwrap(), Value::Null);
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

#[test]
fn stdio_initialize_advertises_document_providers() {
    let (server, init) = initialize_editor_session();
    assert_eq!(init["capabilities"]["documentFormattingProvider"], true);
    assert_eq!(init["capabilities"]["referencesProvider"], true);
    assert_eq!(init["capabilities"]["hoverProvider"], true);
    assert!(init["capabilities"]["signatureHelpProvider"].is_object());
    assert!(init["capabilities"]["completionProvider"].is_object());
    assert_eq!(
        init["capabilities"]["renameProvider"]["prepareProvider"],
        true
    );
    assert_eq!(init["capabilities"]["documentHighlightProvider"], true);
    assert_eq!(init["capabilities"]["foldingRangeProvider"], true);
    assert_eq!(init["capabilities"]["selectionRangeProvider"], true);
    assert!(init["capabilities"]["semanticTokensProvider"].is_object());
    finish_editor_session(server, 14);
}

#[test]
fn stdio_hover_at_call_shows_qualified_signature() {
    let mut server = open_greeting_document();
    server.request(
        2,
        "textDocument/hover",
        json!({"textDocument": {"uri": URI}, "position": {"line": 4, "character": 4}}),
    );
    let hover = server.response(2).response_result.unwrap();
    let hover_val = hover["contents"]["value"].as_str().unwrap();
    assert!(hover_val.contains("DEMO::GREET (who)"), "{hover_val}");
    assert!(hover_val.contains("Greet someone."), "{hover_val}");
    finish_editor_session(server, 3);
}

#[test]
fn stdio_hover_shows_static_constant_without_evaluating_lisp() {
    let (mut server, _) = initialize_editor_session();
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 1,
            "text": "(defun example ()\n  (+ 9223372036854775808 1)\n  (princ \"hi\"))",
        }}),
    );
    server.diagnostics();
    server.request(
        2,
        "textDocument/hover",
        json!({"textDocument": {"uri": URI}, "position": {"line": 1, "character": 3}}),
    );
    let constant = server.response(2).response_result.unwrap();
    let text = constant["contents"]["value"].as_str().unwrap();
    assert!(
        text.contains("Constant preview\n\n    9223372036854775809"),
        "{constant}"
    );

    server.request(
        3,
        "textDocument/hover",
        json!({"textDocument": {"uri": URI}, "position": {"line": 2, "character": 4}}),
    );
    let effectful = server.response(3).response_result.unwrap();
    let text = effectful["contents"]["value"].as_str().unwrap();
    assert!(!text.contains("Constant preview"), "{effectful}");
    finish_editor_session(server, 4);
}

#[test]
fn stdio_updates_hover_and_documentation_diagnostics_at_original_positions() {
    let (mut server, _) = initialize_editor_session();
    let text = "(in-package :demo)\r\n  ;;! Greet someone.\r\n  ;;! ```lisp test\r\n  ;;! (list \"😀\"))\r\n  ;;! ```\r\n(defun greet (who) who)\r\n(greet #\\A)";
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 1, "text": text,
        }}),
    );
    let initial = server.diagnostics();
    let [error] = initial.diagnostics.as_slice() else {
        panic!("{initial:?}")
    };
    assert_eq!(
        error.code,
        Some(lsp_types::NumberOrString::String("doc/example".into()))
    );
    assert_eq!(error.range.start, lsp_types::Position::new(3, 17));
    assert_eq!(error.range.end, lsp_types::Position::new(3, 18));

    let hover_params =
        json!({"textDocument": {"uri": URI}, "position": {"line": 6, "character": 3}});
    server.request(2, "textDocument/hover", hover_params.clone());
    let hover = server.response(2).response_result.unwrap();
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("Greet someone.")
    );

    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": URI, "version": 2},
            "contentChanges": [
                {"range": error.range, "text": ""},
                {"range": {"start": {"line": 1, "character": 6}, "end": {"line": 1, "character": 20}}, "text": "Return the name."},
            ],
        }),
    );
    assert!(server.diagnostics().diagnostics.is_empty());
    server.request(3, "textDocument/hover", hover_params);
    let hover = server.response(3).response_result.unwrap();
    assert!(
        hover["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("Return the name.")
    );
    finish_editor_session(server, 4);
}

#[test]
fn stdio_rechecks_example_calls_when_a_signature_changes() {
    const LIBRARY: &str = "untitled:zkcl-doc-example-library.lisp";
    let text = "(in-package :demo)\r\n;;! ```lisp test\r\n;;! (list \"雪😀\" (greet))\r\n;;! ```\r\n(defun demo () nil)";
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({"capabilities": {
            "textDocument": {"publishDiagnostics": {"versionSupport": true}},
        }}),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 7,
            "text": text,
        }}),
    );
    assert!(server.diagnostics().diagnostics.is_empty());
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": LIBRARY, "languageId": "common-lisp", "version": 1,
            "text": "(in-package :demo)\n(defun greet (name) name)",
        }}),
    );
    assert!(server.diagnostics_for(LIBRARY).diagnostics.is_empty());
    let initial = server.diagnostics();
    assert_eq!(initial.version, Some(7));
    let [warning] = initial.diagnostics.as_slice() else {
        panic!("{initial:?}")
    };
    assert_eq!(
        warning.code,
        Some(lsp_types::NumberOrString::String(
            "correctness/arity-mismatch".into()
        ))
    );
    assert_eq!(warning.range.start, lsp_types::Position::new(2, 16));
    assert_eq!(warning.range.end, lsp_types::Position::new(2, 23));
    let definition = &warning.related_information.as_ref().unwrap()[0].location;
    assert_eq!(definition.uri.as_str(), LIBRARY);
    assert_eq!(definition.range.start, lsp_types::Position::new(1, 7));

    server.change(8, &text.replace("(greet)", "(greet \"Ada\")"));
    let edited = server.diagnostics();
    assert_eq!(edited.version, Some(8));
    assert!(edited.diagnostics.is_empty());
    server.change(9, text);
    let restored = server.diagnostics();
    assert_eq!(restored.version, Some(9));
    assert_eq!(restored.diagnostics, initial.diagnostics);

    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": LIBRARY, "version": 2},
            "contentChanges": [{"text": "(in-package :demo)\n\n(defun greet (name) name)"}],
        }),
    );
    assert!(server.diagnostics_for(LIBRARY).diagnostics.is_empty());
    let shifted = server.diagnostics();
    assert_eq!(shifted.version, Some(9));
    let [warning] = shifted.diagnostics.as_slice() else {
        panic!("{shifted:?}");
    };
    let definition = &warning.related_information.as_ref().unwrap()[0].location;
    assert_eq!(definition.range.start, lsp_types::Position::new(2, 7));

    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": LIBRARY, "version": 3},
            "contentChanges": [{"text": "(in-package :demo)\n(defun greet (&optional name) name)"}],
        }),
    );
    assert!(server.diagnostics_for(LIBRARY).diagnostics.is_empty());
    let repaired = server.diagnostics();
    assert_eq!(repaired.version, Some(9));
    assert!(repaired.diagnostics.is_empty());
    finish_editor_session(server, 2);
}

#[test]
fn stdio_signature_help_marks_the_current_argument() {
    let mut server = open_greeting_document();
    server.request(
        3,
        "textDocument/signatureHelp",
        json!({"textDocument": {"uri": URI}, "position": {"line": 4, "character": 12}}),
    );
    let sig = server.response(3).response_result.unwrap();
    assert_eq!(sig["signatures"][0]["label"], "(GREET who)");
    assert_eq!(sig["activeParameter"], 0);
    finish_editor_session(server, 4);
}

#[test]
fn stdio_references_can_include_the_function_declaration() {
    let mut server = open_greeting_document();
    server.request(
        4,
        "textDocument/references",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 4, "character": 4},
            "context": {"includeDeclaration": true},
        }),
    );
    let refs = server.response(4).response_result.unwrap();
    assert_eq!(refs.as_array().unwrap().len(), 2);
    finish_editor_session(server, 5);
}

#[test]
fn stdio_definition_at_variable_use_jumps_to_parameter() {
    let mut server = open_greeting_document();
    server.request(
        5,
        "textDocument/definition",
        json!({"textDocument": {"uri": URI}, "position": {"line": 3, "character": 8}}),
    );
    let def = server.response(5).response_result.unwrap();
    assert_eq!(def["range"]["start"], json!({"line": 1, "character": 14}));
    finish_editor_session(server, 6);
}

#[test]
fn stdio_prepared_parameter_rename_returns_edits_for_both_occurrences() {
    let mut server = open_greeting_document();
    server.request(
        6,
        "textDocument/prepareRename",
        json!({"textDocument": {"uri": URI}, "position": {"line": 3, "character": 8}}),
    );
    let prep = server.response(6).response_result.unwrap();
    assert_eq!(prep["placeholder"], "who");

    server.request(
        7,
        "textDocument/rename",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 3, "character": 8},
            "newName": "person",
        }),
    );
    let rename_edit = server.response(7).response_result.unwrap();
    assert_eq!(rename_edit["changes"][URI].as_array().unwrap().len(), 2);
    finish_editor_session(server, 8);
}

#[test]
fn stdio_rename_returns_one_edit_map_for_open_and_disk_documents() {
    let dir = std::env::temp_dir().join(format!("zkcl-lsp-rename-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("library.lisp");
    std::fs::write(
        &path,
        "(defpackage :rename-lib (:use :cl) (:export :library-value))\n\
         (in-package :rename-lib)\n(defun library-value () 42)",
    )
    .unwrap();
    let library_uri = format!("file://{}", path.display());

    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({
            "workspaceFolders": [{"uri": format!("file://{}", dir.display()), "name": "rename"}],
            "capabilities": {},
        }),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 7,
            "text": "(in-package :cl-user)\n(rename-lib:library-value)"
        }}),
    );
    assert!(server.diagnostics().diagnostics.is_empty());
    server.request(
        2,
        "textDocument/rename",
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 1, "character": 14},
            "newName": "fresh-value",
        }),
    );
    let result = server.response(2).response_result.unwrap();
    finish_editor_session(server, 3);
    std::fs::remove_dir_all(&dir).unwrap();

    let changes = result["changes"].as_object().unwrap();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[&library_uri][0]["newText"], "fresh-value");
    assert_eq!(changes[URI][0]["newText"], "rename-lib:fresh-value");
}

#[test]
fn stdio_formatting_returns_edits_for_unformatted_source() {
    let mut server = open_greeting_document();
    server.request(
        8,
        "textDocument/formatting",
        json!({
            "textDocument": {"uri": URI},
            "options": {"tabSize": 2, "insertSpaces": true},
        }),
    );
    let edits = server.response(8).response_result.unwrap();
    assert!(!edits.as_array().unwrap().is_empty());
    finish_editor_session(server, 9);
}

#[test]
fn stdio_completion_offers_parameter_matching_typed_prefix() {
    let mut server = open_greeting_document();
    server.request(
        9,
        "textDocument/completion",
        json!({"textDocument": {"uri": URI}, "position": {"line": 3, "character": 9}}),
    );
    let completions = server.response(9).response_result.unwrap();
    assert!(
        completions
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["label"] == "who")
    );
    finish_editor_session(server, 10);
}

#[test]
fn stdio_document_highlight_marks_both_parameter_occurrences() {
    let mut server = open_greeting_document();
    server.request(
        10,
        "textDocument/documentHighlight",
        json!({"textDocument": {"uri": URI}, "position": {"line": 3, "character": 8}}),
    );
    assert_eq!(
        server
            .response(10)
            .response_result
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    finish_editor_session(server, 11);
}

#[test]
fn stdio_folding_range_covers_the_multiline_definition() {
    let mut server = open_greeting_document();
    server.request(
        11,
        "textDocument/foldingRange",
        json!({"textDocument": {"uri": URI}}),
    );
    assert_eq!(
        server
            .response(11)
            .response_result
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    finish_editor_session(server, 12);
}

#[test]
fn stdio_selection_range_returns_one_chain_per_requested_position() {
    let mut server = open_greeting_document();
    server.request(
        12,
        "textDocument/selectionRange",
        json!({
            "textDocument": {"uri": URI},
            "positions": [{"line": 3, "character": 8}],
        }),
    );
    assert_eq!(
        server
            .response(12)
            .response_result
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    finish_editor_session(server, 13);
}

#[test]
fn stdio_full_semantic_tokens_encodes_the_open_document() {
    let mut server = open_greeting_document();
    server.request(
        13,
        "textDocument/semanticTokens/full",
        json!({"textDocument": {"uri": URI}}),
    );
    assert!(
        !server.response(13).response_result.unwrap()["data"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    finish_editor_session(server, 14);
}

#[test]
fn stdio_multiline_semantic_tokens_use_negotiated_code_unit_lengths() {
    let string = zkcl_syntax::SemanticTokenKind::String as u32;
    for (encoding, first_length, second_length) in
        [("utf-8", 6, 7), ("utf-16", 4, 5), ("utf-32", 3, 5)]
    {
        let mut server = Server::start();
        server.request(
            1,
            "initialize",
            json!({
                "capabilities": {"general": {"positionEncodings": [encoding]}},
                "initializationOptions": {"readtable": "raw-strings"},
            }),
        );
        server.response(1).response_result.unwrap();
        server.notify("initialized", json!({}));
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": URI, "languageId": "common-lisp", "version": 1,
                "text": "#>😀\r\n  #>雪\r\n",
            }}),
        );
        assert!(server.diagnostics().diagnostics.is_empty());
        server.request(
            2,
            "textDocument/semanticTokens/full",
            json!({"textDocument": {"uri": URI}}),
        );
        let result = server.response(2).response_result.unwrap();
        let data = serde_json::from_value::<Vec<u32>>(result["data"].clone()).unwrap();
        let expected = [
            0,
            0,
            first_length,
            string,
            0,
            1,
            0,
            second_length,
            string,
            0,
        ];
        assert_eq!(data, expected, "{encoding}");
        finish_editor_session(server, 3);
    }
}

#[test]
fn stdio_closing_unsaved_document_restores_indexed_disk_definition() {
    let dir = std::env::temp_dir().join(format!("zkcl-lsp-ws-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let lib_path = dir.join("lib.lisp");
    std::fs::write(
        &lib_path,
        "(in-package :demo)\n#+zkcl-lsp-feature\n(defun disk-fn (x) (+ x 1))\n",
    )
    .unwrap();
    let dir_uri = format!("file://{}", dir.to_str().unwrap());
    let lib_uri = format!("file://{}", lib_path.to_str().unwrap());

    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({
            "processId": null,
            "workspaceFolders": [{"uri": dir_uri, "name": "ws"}],
            "initializationOptions": {"features": ["ZKCL-LSP-FEATURE"]},
            "capabilities": {},
        }),
    );
    let _ = server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));

    // Open caller buffer without opening `lib.lisp`; definition should resolve to disk-indexed `lib.lisp`!
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 1,
            "text": "(in-package :demo)\n(disk-fn)\n",
        }}),
    );
    assert_eq!(server.diagnostics_for(URI).diagnostics.len(), 1);

    server.request(
        2,
        "textDocument/definition",
        json!({"textDocument": {"uri": URI}, "position": {"line": 1, "character": 2}}),
    );
    let def = server.response(2).response_result.unwrap();
    assert_eq!(def["uri"], lib_uri);
    assert_eq!(def["range"]["start"]["line"], 2);

    // Open `lib.lisp` with unsaved changes that rename `disk-fn` -> `temp-fn`, then close it without saving.
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": lib_uri, "languageId": "common-lisp", "version": 1,
            "text": "(in-package :demo)\n(defun temp-fn (x) (+ x 1))\n",
        }}),
    );
    assert!(server.diagnostics_for(&lib_uri).diagnostics.is_empty());
    assert!(server.diagnostics_for(URI).diagnostics.is_empty());
    server.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": lib_uri}}),
    );
    assert!(server.diagnostics_for(&lib_uri).diagnostics.is_empty());
    assert_eq!(server.diagnostics_for(URI).diagnostics.len(), 1);

    // After closing unsaved `lib.lisp`, disk content `(defun disk-fn ...)` is restored!
    server.request(
        3,
        "textDocument/definition",
        json!({"textDocument": {"uri": URI}, "position": {"line": 1, "character": 2}}),
    );
    let restored_def = server.response(3).response_result.unwrap();
    assert_eq!(restored_def["uri"], lib_uri);

    // Workspace symbol search finds `disk-fn` on disk!
    server.request(4, "workspace/symbol", json!({"query": "disk-"}));
    let ws_syms = server.response(4).response_result.unwrap();
    let ws_syms = ws_syms.as_array().unwrap();
    assert_eq!(ws_syms.len(), 1);
    assert_eq!(ws_syms[0]["name"], "disk-fn");
    assert_eq!(ws_syms[0]["location"]["uri"], lib_uri);

    finish_editor_session(server, 7);
    let _ = std::fs::remove_dir_all(&dir);
}

fn open_document_with_unused_parameter() -> (Server, Vec<lsp_types::Diagnostic>) {
    let (mut server, _) = initialize_editor_session();
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": "untitled:actions.lisp",
            "languageId": "common-lisp",
            "version": 1,
            "text": "(in-package :demo)\n(defun prefix (used unused)\n  (subseq used 0 2))\n",
        }}),
    );
    let diagnostics = server.diagnostics_for("untitled:actions.lisp").diagnostics;
    (server, diagnostics)
}

#[test]
fn stdio_inlay_hints_label_all_subseq_arguments() {
    let (mut server, _) = open_document_with_unused_parameter();
    server.request(
        5,
        "textDocument/inlayHint",
        json!({
            "textDocument": {"uri": "untitled:actions.lisp"},
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 3, "character": 0},
            },
        }),
    );
    let hints = server.response(5).response_result.unwrap();
    let hints = hints.as_array().unwrap();
    assert_eq!(hints.len(), 3);
    assert_eq!(hints[0]["label"], "sequence:");
    assert_eq!(hints[1]["label"], "start:");
    assert_eq!(hints[2]["label"], "end:");
    finish_editor_session(server, 6);
}

#[test]
fn stdio_unused_parameter_warning_has_a_quick_fix() {
    let (mut server, action_diags) = open_document_with_unused_parameter();
    assert_eq!(action_diags.len(), 1, "{action_diags:?}");
    assert_eq!(
        action_diags[0].severity,
        Some(lsp_types::DiagnosticSeverity::WARNING)
    );
    assert_eq!(
        action_diags[0].tags,
        Some(vec![lsp_types::DiagnosticTag::UNNECESSARY])
    );
    assert_eq!(action_diags[0].message, "unused lexical variable `unused`");

    server.request(
        6,
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": "untitled:actions.lisp"},
            "range": {
                "start": {"line": 1, "character": 20},
                "end": {"line": 1, "character": 26},
            },
            "context": {"diagnostics": []},
        }),
    );
    let actions = server.response(6).response_result.unwrap();
    let actions = actions.as_array().unwrap();
    assert!(
        actions
            .iter()
            .any(|a| a["title"] == "Declare `(ignore unused)`"),
        "{actions:?}"
    );
    finish_editor_session(server, 7);
}

#[test]
fn stdio_completion_replaces_a_qualified_symbol_at_the_negotiated_position() {
    for (encoding, token_start) in [("utf-8", 14), ("utf-16", 12), ("utf-32", 11)] {
        let mut server = Server::start();
        server.request(
            1,
            "initialize",
            json!({"processId": null, "capabilities": {
                "general": {"positionEncodings": [encoding]},
                "textDocument": {"publishDiagnostics": {"versionSupport": true}},
            }}),
        );
        let _ = server.response(1).response_result.unwrap();
        server.notify("initialized", json!({}));
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": URI, "languageId": "common-lisp", "version": 1,
                "text": "(list \"😀\" (cl:cax '(1))\r\n",
            }}),
        );
        let diagnostics = server.diagnostics();
        assert_eq!(diagnostics.diagnostics.len(), 1);
        assert_eq!(
            diagnostics.diagnostics[0].message,
            "missing closing parenthesis"
        );
        let combined = server.diagnostics();
        assert_eq!(combined.diagnostics.len(), 2);
        assert_eq!(
            combined.diagnostics[1].message,
            "`cl:cax` is not exported by `common-lisp`"
        );

        let position = json!({
            "textDocument": {"uri": URI},
            "position": {"line": 0, "character": token_start + 5},
        });
        server.request(2, "textDocument/completion", position.clone());
        let result = server.response(2).response_result.unwrap();
        let item = result
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["label"] == "cl:car")
            .unwrap();
        let edit = &item["textEdit"];
        assert_eq!(
            *edit,
            json!({
                "range": {
                    "start": {"line": 0, "character": token_start},
                    "end": {"line": 0, "character": token_start + 6},
                },
                "newText": "cl:car",
            }),
            "{encoding}"
        );

        server.changes(
            2,
            &[
                json!({"range": edit["range"], "text": edit["newText"]}),
                ranged_change((1, 0), (1, 0), ")"),
            ],
        );
        let diagnostics = server.diagnostics();
        assert_eq!(diagnostics.version, Some(2));
        assert!(diagnostics.diagnostics.is_empty());
        server.request(3, "textDocument/hover", position);
        let hover = server.response(3).response_result.unwrap();
        assert_eq!(hover["range"], edit["range"]);
        assert!(
            hover["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("cl:CAR")
        );

        server.request(4, "shutdown", Value::Null);
        assert_eq!(server.response(4).response_result.unwrap(), Value::Null);
        server.notify("exit", Value::Null);
        assert!(server.wait());
    }
}

#[test]
fn stdio_document_symbol_respects_initialization_options_features() {
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({
            "processId": null,
            "initializationOptions": { "features": ["zkcl-lsp-feature"] },
            "capabilities": {
                "textDocument": {
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
                }
            }
        }),
    );
    let _ = server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));

    server.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": URI,
                "languageId": "common-lisp",
                "version": 1,
                "text": "\
#+zkcl-lsp-feature (defun enabled-fn () 1)
#+other-feature (defun skipped-fn () 2)
#-other-feature (defun fallback-fn () 3)
(defun always-fn () 4)
"
            }
        }),
    );
    assert!(server.diagnostics().diagnostics.is_empty());

    server.request(
        2,
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": URI } }),
    );
    let syms = server.response(2).response_result.unwrap();
    let names: Vec<_> = syms
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["enabled-fn", "fallback-fn", "always-fn"]);

    server.request(3, "shutdown", Value::Null);
    assert_eq!(server.response(3).response_result.unwrap(), Value::Null);
    server.notify("exit", Value::Null);
    assert!(server.wait());
}

#[test]
fn stdio_target_changes_refresh_outline_and_warnings_for_open_documents() {
    const SECOND: &str = "untitled:target-second.lisp";
    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({
            "initializationOptions": { "target": "aarch64-apple-darwin" },
            "capabilities": {
                "textDocument": {
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "publishDiagnostics": { "versionSupport": true },
                },
            },
        }),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": URI, "languageId": "common-lisp", "version": 4,
            "text": "\
#+(and macos arm64) (defun mac-only (mac) nil)
#+(and windows x86-64) (defun windows-only (win) nil)
#+(and android linux) (defun android-only (phone) nil)
#+custom-feat (defun custom (option) nil)
#+windows (set-macro-character #\\! (function read-it))
(defun trailing (later) nil)
",
        }}),
    );
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": SECOND, "languageId": "common-lisp", "version": 12,
            "text": "#+windows (defun second-window (other) nil)",
        }}),
    );

    let changes: [(Value, &[&str], &[&str], bool); 7] = [
        (
            Value::Null,
            &["mac-only", "trailing"],
            &["mac", "later"],
            false,
        ),
        (
            json!({"zkcl": {"target": "x86_64-pc-windows-msvc"}}),
            &["windows-only"],
            &["win"],
            true,
        ),
        (
            json!({"target": "aarch64-linux-android"}),
            &["android-only", "trailing"],
            &["phone", "later"],
            false,
        ),
        (json!({"features": []}), &["trailing"], &["later"], false),
        (
            json!({"target": "x86_64-pc-windows-msvc", "features": ["custom-feat"]}),
            &["custom", "trailing"],
            &["option", "later"],
            false,
        ),
        (json!({"features": null}), &["windows-only"], &["win"], true),
        (json!({"target": null}), &[], &[], false),
    ];
    let mut previous_second_warning = None;
    for (step, (settings, symbols, unused_variables, second_warning)) in
        changes.into_iter().enumerate()
    {
        if !settings.is_null() {
            server.notify(
                "workspace/didChangeConfiguration",
                json!({"settings": settings}),
            );
        }
        let mut expected = HashMap::from([(URI, (4, unused_variables))]);
        if previous_second_warning != Some(second_warning) {
            let unused: &[&str] = if second_warning { &["other"] } else { &[] };
            expected.insert(SECOND, (12, unused));
            previous_second_warning = Some(second_warning);
        }
        for _ in 0..expected.len() {
            let diagnostics = server.receive_diagnostics();
            let (version, unused_variables) = expected.remove(diagnostics.uri.as_str()).unwrap();
            assert_eq!(diagnostics.version, Some(version));
            assert_eq!(
                diagnostics
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>(),
                unused_variables
                    .iter()
                    .map(|name| format!("unused lexical variable `{name}`"))
                    .collect::<Vec<_>>(),
            );
        }

        let id = 2 + step as i32;
        server.request(
            id,
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": URI}}),
        );
        let response = server.response(id).response_result.unwrap();
        let names: Vec<_> = response
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, symbols);
    }
    finish_editor_session(server, 9);
}

fn ranged_change(start: (u32, u32), end: (u32, u32), text: &str) -> Value {
    json!({
        "range": {
            "start": {"line": start.0, "character": start.1},
            "end": {"line": end.0, "character": end.1},
        },
        "text": text,
    })
}

#[test]
fn stdio_lint_config_reloads_buffers_and_restores_disk_settings_on_close() {
    let dir = std::env::temp_dir().join(format!("zkcl-lsp-lint-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join(".zkcl-lint.lisp");
    let disk_config = "(:preset :strict :rules ((style/if-to-when :deny)))\n";
    std::fs::write(&config_path, disk_config).unwrap();
    std::fs::write(dir.join(".zkcl-lint.toml"), "preset = \"default\"\n").unwrap();
    let config_uri = format!("file://{}", config_path.display());
    let file_path = dir.join("sample.lisp");
    let source = "(defun demo (c x)\n  (if c (print x)))\n";
    std::fs::write(&file_path, source).unwrap();
    let root_uri = format!("file://{}", dir.display());
    let file_uri = format!("file://{}", file_path.display());

    let mut server = Server::start();
    server.request(
        1,
        "initialize",
        json!({
            "processId": null,
            "workspaceFolders": [{"uri": root_uri, "name": "lint-ws"}],
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": {"versionSupport": true}
                }
            }
        }),
    );
    server.response(1).response_result.unwrap();
    server.notify("initialized", json!({}));
    server.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": file_uri,
                "languageId": "common-lisp",
                "version": 1,
                "text": source
            }
        }),
    );

    let published = server.diagnostics_for(&file_uri);
    let diag = published
        .diagnostics
        .iter()
        .find(|d| {
            d.code
                .as_ref()
                .is_some_and(|c| *c == lsp_types::NumberOrString::String("style/if-to-when".into()))
        })
        .unwrap_or_else(|| {
            panic!(
                "expected style/if-to-when diagnostic, got {:?}",
                published.diagnostics
            )
        });
    assert_eq!(diag.severity, Some(lsp_types::DiagnosticSeverity::ERROR));
    let data = diag.data.as_ref().unwrap();
    assert_eq!(data["help"], "replace with `(when c (print x))`");
    assert_eq!(
        data["rationale"],
        zkcl_syntax::find_rule("style/if-to-when")
            .unwrap()
            .rationale
    );

    server.request(
        2,
        "textDocument/codeAction",
        json!({
            "textDocument": {"uri": file_uri},
            "range": diag.range,
            "context": {"diagnostics": []}
        }),
    );
    let actions = server.response(2).response_result.unwrap();
    let has_when_fix = actions
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["title"] == "Replace with `when`");
    assert!(
        has_when_fix,
        "expected `Replace with when` quickfix in {actions}"
    );

    let open_config = |server: &mut Server, text: &str| {
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": config_uri, "languageId": "common-lisp", "version": 1, "text": text,
            }}),
        );
    };
    let close_config = |server: &mut Server| {
        server.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": config_uri}}),
        );
    };
    let source_diagnostics = |server: &Server| loop {
        let published = server.receive_diagnostics();
        if published.uri.as_str() == file_uri {
            assert_eq!(published.version, Some(1));
            break published.diagnostics;
        }
        assert_eq!(published.uri.as_str(), config_uri);
    };
    let rule_severity = |diagnostics: Vec<lsp_types::Diagnostic>| {
        diagnostics.into_iter().find_map(|diag| {
            (diag.code == Some(lsp_types::NumberOrString::String("style/if-to-when".into())))
                .then_some(diag.severity)
                .flatten()
        })
    };

    open_config(&mut server, disk_config);
    assert!(server.diagnostics_for(&config_uri).diagnostics.is_empty());
    server.notify(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": config_uri, "version": 2},
            "contentChanges": [{"text": "(:rules ((style/if-to-when :allow)))"}],
        }),
    );
    assert_eq!(rule_severity(source_diagnostics(&server)), None);
    close_config(&mut server);
    assert_eq!(
        rule_severity(source_diagnostics(&server)),
        Some(lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), disk_config);

    // Client settings survive configuration reloads and can be cleared explicitly.
    server.notify(
        "workspace/didChangeConfiguration",
        json!({"settings": {"zkcl": {"lint": {"rules": {"all": "allow"}}}}}),
    );
    assert!(source_diagnostics(&server).is_empty());
    let malformed_uri = "untitled:lint-disabled.lisp";
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": malformed_uri, "languageId": "common-lisp", "version": 1, "text": ")",
        }}),
    );
    let structural = server.diagnostics_for(malformed_uri);
    assert_eq!(structural.version, Some(1));
    assert_eq!(structural.diagnostics[0].message, "unexpected closing parenthesis");
    server.notify(
        "textDocument/didClose",
        json!({"textDocument": {"uri": malformed_uri}}),
    );
    assert!(server.diagnostics_for(malformed_uri).diagnostics.is_empty());

    server.notify(
        "workspace/didChangeConfiguration",
        json!({"settings": {"zkcl": {"lint": {"rules": {"style/if-to-when": "warn"}}}}}),
    );
    assert_eq!(
        rule_severity(source_diagnostics(&server)),
        Some(lsp_types::DiagnosticSeverity::WARNING),
    );
    open_config(&mut server, "(:rules ((style/if-to-when :allow)))");
    assert!(server.diagnostics_for(&config_uri).diagnostics.is_empty());
    close_config(&mut server);
    assert!(server.diagnostics_for(&config_uri).diagnostics.is_empty());
    server.notify(
        "workspace/didChangeConfiguration",
        json!({"settings": {"zkcl": {"lint": null}}}),
    );
    assert_eq!(
        rule_severity(source_diagnostics(&server)),
        Some(lsp_types::DiagnosticSeverity::ERROR),
    );

    // When the preferred config no longer exists, reload the other format.
    open_config(&mut server, disk_config);
    assert!(server.diagnostics_for(&config_uri).diagnostics.is_empty());
    std::fs::remove_file(&config_path).unwrap();
    close_config(&mut server);
    assert_eq!(
        rule_severity(source_diagnostics(&server)),
        Some(lsp_types::DiagnosticSeverity::WARNING),
    );

    server.request(3, "shutdown", Value::Null);
    server.response(3);
    server.notify("exit", Value::Null);
    assert!(server.wait());
    let _ = std::fs::remove_dir_all(&dir);
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    output: Receiver<io::Result<Option<Message>>>,
    reader: Option<JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zkcl-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            loop {
                let result = Message::read(&mut stdout);
                let done = !matches!(result, Ok(Some(_)));
                if sender.send(result).is_err() || done {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            output,
            reader: Some(reader),
        }
    }

    fn request(&mut self, id: i32, method: &str, params: Value) {
        Message::Request(Request::new(id.into(), method.into(), params))
            .write(&mut self.stdin)
            .unwrap();
    }

    fn notify(&mut self, method: &str, params: Value) {
        Message::Notification(Notification::new(method.into(), params))
            .write(&mut self.stdin)
            .unwrap();
    }

    fn change(&mut self, version: i32, text: &str) {
        self.changes(version, &[json!({"text": text})]);
    }

    fn changes(&mut self, version: i32, changes: &[Value]) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": URI, "version": version},
                "contentChanges": changes,
            }),
        );
    }

    fn receive(&self) -> Option<Message> {
        self.output
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap()
    }

    fn response(&self, id: i32) -> Response {
        let message = self.receive();
        let Some(Message::Response(response)) = message else {
            panic!("expected response {id}, received {message:?}");
        };
        assert_eq!(response.id, id.into());
        response
    }

    fn diagnostics(&self) -> PublishDiagnosticsParams {
        self.diagnostics_for(URI)
    }

    fn diagnostics_for(&self, uri: &str) -> PublishDiagnosticsParams {
        let diagnostics = self.receive_diagnostics();
        assert_eq!(diagnostics.uri.as_str(), uri);
        diagnostics
    }

    fn receive_diagnostics(&self) -> PublishDiagnosticsParams {
        let message = self.receive();
        let Some(Message::Notification(notification)) = message else {
            panic!("expected diagnostics, received {message:?}");
        };
        notification
            .extract::<PublishDiagnosticsParams>("textDocument/publishDiagnostics")
            .unwrap()
    }

    fn wait(mut self) -> bool {
        assert!(self.receive().is_none(), "output continued after exit");
        self.child.wait().unwrap().success()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reader.take().unwrap().join().unwrap();
    }
}

#[test]
fn stdio_fix_all_uses_only_machine_applicable_edits() {
    let mut server = Server::start();
    server.request(1, "initialize", json!({"capabilities":{}}));
    assert!(server.response(1).response_result.unwrap()["capabilities"]["codeActionProvider"]["codeActionKinds"]
        .as_array().unwrap().iter().any(|kind| kind == "source.fixAll"));
    server.notify("initialized", json!({}));
    let source = concat!(
        "(defun sample (unused rows key cache)\n",
        "  (dotimes (i (length rows)) (let ((row (nth i rows))) (print row)))\n",
        "  (list (and rows) (second (multiple-value-list (gethash key cache))) (make-array 3 :initial-eleemnt 0)))",
    );
    server.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri": URI, "languageId":"common-lisp", "version": 3,
            "text":source
        }}),
    );
    let diagnostics = server.diagnostics();
    assert_eq!(diagnostics.version, Some(3));
    let whole = json!({"start":{"line":0,"character":0},"end":{"line":2,"character":200}});
    let request = |server: &mut Server, id, kind: &str, range: &serde_json::Value| {
        server.request(
            id,
            "textDocument/codeAction",
            json!({
                "textDocument":{"uri":URI}, "range":range,
                "context":{"diagnostics":[], "only":[kind]},
            }),
        );
        server.response(id).response_result.unwrap()
    };
    let quick = request(&mut server, 2, "quickfix", &whole);
    let typo = quick
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Replace with `:initial-element`")
        .unwrap();
    assert_eq!(typo["isPreferred"], false);
    let traversal = quick
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["title"] == "Walk the list directly with `dolist`")
        .unwrap();
    assert_eq!(traversal["isPreferred"], false);
    let all = request(&mut server, 3, "source.fixAll", &whole);
    let [all] = all.as_array().unwrap().as_slice() else {
        panic!("{all}")
    };
    assert_eq!(all["kind"], "source.fixAll");
    let edits: Vec<_> = all["edit"]["changes"][URI]
        .as_array()
        .unwrap()
        .iter()
        .map(|edit| edit["newText"].as_str().unwrap())
        .collect();
    assert_eq!(edits, ["rows", "(nth-value 1 ", ")"]);
    server.change(4, "(list :done)");
    assert!(server.diagnostics().diagnostics.is_empty());
    let range = json!({"start":{"line":0,"character":0},"end":{"line":0,"character":12}});
    assert_eq!(request(&mut server, 4, "source.fixAll", &range), json!([]));
    finish_editor_session(server, 5);
}

#[test]
fn stdio_shared_suggestions_use_unicode_ranges() {
    for (encoding, start) in [("utf-8", 16), ("utf-16", 12), ("utf-32", 11)] {
        let mut server = Server::start();
        server.request(
            1,
            "initialize",
            json!({"capabilities": {
                "general": {"positionEncodings": [encoding]},
            }}),
        );
        server.response(1).response_result.unwrap();
        server.notify("initialized", json!({}));
        server.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": URI, "languageId": "common-lisp", "version": 7,
                "text": "(list \"雪😀\" (and :value))\r\n",
            }}),
        );
        let diagnostics = server.diagnostics();
        assert_eq!(diagnostics.version, Some(7));
        let diagnostic = diagnostics
            .diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic.code
                    == Some(lsp_types::NumberOrString::String(
                        "complexity/nested-and-or".into(),
                    ))
            })
            .unwrap_or_else(|| panic!("{diagnostics:?}"));
        assert_eq!(diagnostic.range.start.character, start);
        assert!(diagnostic.data.as_ref().unwrap()["rationale"].is_string());
        assert!(diagnostic.data.as_ref().unwrap()["evidence"].is_string());
        server.request(
            2,
            "textDocument/codeAction",
            json!({
                "textDocument": {"uri": URI},
                "range": diagnostic.range,
                "context": {"diagnostics": [diagnostic], "only": ["quickfix"]},
            }),
        );
        let actions = server.response(2).response_result.unwrap();
        let edit = actions
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|action| action["edit"]["changes"][URI].as_array())
            .flatten()
            .find(|edit| edit["newText"] == ":value")
            .unwrap_or_else(|| panic!("{actions}"));
        assert_eq!(
            edit["range"],
            serde_json::to_value(diagnostic.range).unwrap()
        );
        finish_editor_session(server, 3);
    }
}
