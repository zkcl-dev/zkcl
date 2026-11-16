use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::error::Error;
use std::io::{self, BufRead, Write};
use std::mem::DropGuard;
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};
use thiserror::Error;
use zkcl::{
    CancellationHandle, EvalError, EvalErrorKind, PauseHandle, Root, Runtime, SourceDocument,
};
use zkcl_diagnostics::{Diagnostic, Severity};

use super::ReplStartup;
use crate::diagnostics;
mod breakpoints;
mod debugger;
use breakpoints::BreakpointRequest;
mod interaction;
mod objects;
mod profile;
use interaction::{Answer, Interaction, Mode, Protocol, SessionIo};
use objects::ObjectRequest;

enum ReplInput {
    Evaluate(EvaluationRequest),
    Refresh,
    Reload,
    Reply(Value),
    Breakpoint(BreakpointRequest),
    CancelAcknowledgement(bool),
    PauseAcknowledgement(bool),
    Object(ObjectRequest),
    Profile(profile::ProfileRequest),
    Image(ImageRequest),
    Invalid(RequestError),
}

struct EvaluationRequest {
    source: String,
    origin: Option<SourceOrigin>,
    mode: Mode,
    render: bool,
}

#[derive(Default)]
struct EvaluationResults {
    text: Vec<Vec<String>>,
    objects: Vec<Vec<Value>>,
    print_errors: Vec<Value>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "lowercase", deny_unknown_fields)]
enum ImageRequest {
    Save { path: std::path::PathBuf },
    Restore { path: std::path::PathBuf },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceOrigin {
    path: Option<std::path::PathBuf>,
    uri: Option<String>,
    version: Option<i64>,
    #[serde(default)]
    base_offset: usize,
}

impl SourceOrigin {
    fn document(self, source: &str) -> SourceDocument {
        let mut document = SourceDocument::new(self.path, Some(source.to_owned()))
            .with_base_offset(self.base_offset);
        if let Some(uri) = self.uri {
            document = document.with_uri(uri);
        }
        if let Some(version) = self.version {
            document = document.with_version(version);
        }
        document
    }
}

#[derive(Debug, Error)]
enum RequestError {
    #[error("{0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error(
        "expected source text, refresh, reload, cancel, pause, reply, breakpoint, object, profile, or image operation"
    )]
    InvalidCommand,
    #[error("{operation} with this id is already pending")]
    DuplicateRequest { operation: &'static str },
    #[error(
        "expected boolean evaluation options and a non-null id for interactive or streaming evaluation"
    )]
    InvalidMode,
}

enum RequestState {
    Queued,
    CancelledBeforeStart,
    Running(CancellationHandle, PauseHandle),
}

#[derive(Default)]
struct PendingRequests(Mutex<HashMap<Value, RequestState>>);

impl PendingRequests {
    fn register_queued(&self, request_id: &Value) -> bool {
        if request_id.is_null() {
            return true;
        }
        if let Entry::Vacant(entry) = self.0.lock().unwrap().entry(request_id.clone()) {
            entry.insert(RequestState::Queued);
            true
        } else {
            false
        }
    }

    fn request_cancellation(&self, target_id: &Value) -> bool {
        match self.0.lock().unwrap().get_mut(target_id) {
            Some(state @ RequestState::Queued) => *state = RequestState::CancelledBeforeStart,
            Some(RequestState::CancelledBeforeStart) => {}
            Some(RequestState::Running(handle, _)) => handle.cancel(),
            None => return false,
        }
        true
    }

    fn request_pause(&self, target_id: &Value) -> bool {
        match self.0.lock().unwrap().get(target_id) {
            Some(RequestState::Running(_, handle)) => {
                handle.pause();
                true
            }
            _ => false,
        }
    }

    fn retire_cancelled_before_start(&self, request_id: &Value) -> bool {
        if request_id.is_null() {
            return false;
        }
        let mut pending = self.0.lock().unwrap();
        if matches!(
            pending.get(request_id),
            Some(RequestState::CancelledBeforeStart)
        ) {
            pending.remove(request_id);
            true
        } else {
            false
        }
    }

    fn begin(&self, request_id: &Value, runtime: &Runtime) -> bool {
        if request_id.is_null() {
            return true;
        }
        let mut pending = self.0.lock().unwrap();
        let state = pending.get_mut(request_id).expect("registered request");
        if matches!(state, RequestState::CancelledBeforeStart) {
            return false;
        }
        *state = RequestState::Running(runtime.cancellation_handle(), runtime.pause_handle());
        true
    }

    fn finish(&self, request_id: &Value) -> bool {
        if request_id.is_null() {
            return false;
        }
        let mut pending = self.0.lock().unwrap();
        // Retire the request ID and reset together so a late cancel cannot reach the next evaluation.
        match pending.remove(request_id).expect("registered request") {
            RequestState::Queued => false,
            RequestState::CancelledBeforeStart => true,
            RequestState::Running(handle, pause) => {
                pause.reset();
                let cancelled = handle.is_cancelled();
                handle.reset();
                cancelled
            }
        }
    }
}

fn read_requests(
    pending: &PendingRequests,
    interaction: &Interaction,
    protocol: &Protocol,
    sender: mpsc::Sender<io::Result<(Value, ReplInput)>>,
) {
    let mut input = io::stdin().lock();
    let mut line = Vec::new();
    loop {
        match input.read_until(b'\n', &mut line) {
            // Closing input only retires the request stream: a batch client can
            // deliver every request up front and keep reading the answers.
            Ok(0) => break,
            Ok(_) => {
                match parse_request(&line, pending, interaction) {
                    (id, ReplInput::Reply(reply)) => {
                        let accepted = reply
                            .get("request")
                            .zip(reply.get("token").and_then(Value::as_u64))
                            .is_some_and(|(request, token)| {
                                let answer = match (reply.get("text"), reply.get("action")) {
                                    (Some(Value::String(text)), None) => {
                                        Some(Some(Answer::Text(text.clone())))
                                    }
                                    (Some(Value::Null), None) => Some(None),
                                    (None, Some(action)) if action.is_object() => {
                                        Some(Some(Answer::Action {
                                            id: id.clone(),
                                            action: action.clone(),
                                        }))
                                    }
                                    _ => None,
                                };
                                answer
                                    .is_some_and(|answer| interaction.reply(request, token, answer))
                            });
                        if protocol
                            .send(&json!({"id": id, "event": "reply", "accepted": accepted}))
                            .is_err()
                        {
                            break;
                        }
                    }
                    (id, ReplInput::PauseAcknowledgement(accepted)) => {
                        if protocol.send(&json!({"id":id,"paused":accepted,"values":[],"objects":[],"error":null,"stdout":"","stderr":""})).is_err() { break; }
                    }
                    request => {
                        if sender.send(Ok(request)).is_err() {
                            break;
                        }
                    }
                }
                line.clear();
            }
            Err(error) => {
                let _ = sender.send(Err(error));
                break;
            }
        }
    }
    interaction.close();
}

fn parse_request(
    line: &[u8],
    pending: &PendingRequests,
    interaction: &Interaction,
) -> (Value, ReplInput) {
    let mut request = match serde_json::from_slice::<Value>(line) {
        Ok(Value::Object(request)) => request,
        Ok(_) => {
            return (
                Value::Null,
                ReplInput::Invalid(RequestError::InvalidCommand),
            );
        }
        Err(error) => return (Value::Null, ReplInput::Invalid(error.into())),
    };
    let id = request.remove("id").unwrap_or(Value::Null);
    let pause = request.remove("pause");
    let object = request.remove("object");
    let profile = request.remove("profile");
    let image = request.remove("image");
    let refresh = request.remove("refresh");
    let origin = request.remove("sourceOrigin");
    let command = if let Some(refresh) = refresh {
        if refresh == Value::Bool(true)
            && request.is_empty()
            && pause.is_none()
            && object.is_none()
            && profile.is_none()
            && image.is_none()
            && origin.is_none()
        {
            ReplInput::Refresh
        } else {
            ReplInput::Invalid(RequestError::InvalidCommand)
        }
    } else if let Some(target) = pause {
        if !target.is_null()
            && request.is_empty()
            && object.is_none()
            && profile.is_none()
            && image.is_none()
            && origin.is_none()
        {
            ReplInput::PauseAcknowledgement(pending.request_pause(&target))
        } else {
            ReplInput::Invalid(RequestError::InvalidCommand)
        }
    } else if let Some(image) = image {
        if request.is_empty() && object.is_none() && profile.is_none() && origin.is_none() {
            serde_json::from_value(image)
                .map(ReplInput::Image)
                .unwrap_or_else(|error| ReplInput::Invalid(error.into()))
        } else {
            ReplInput::Invalid(RequestError::InvalidCommand)
        }
    } else if let Some(profile) = profile {
        if request.is_empty() && object.is_none() && origin.is_none() {
            serde_json::from_value(profile)
                .map(ReplInput::Profile)
                .unwrap_or_else(|error| ReplInput::Invalid(error.into()))
        } else {
            ReplInput::Invalid(RequestError::InvalidCommand)
        }
    } else if let Some(object) = object {
        if request.is_empty() && origin.is_none() {
            serde_json::from_value(object)
                .map(ReplInput::Object)
                .unwrap_or_else(|error| ReplInput::Invalid(error.into()))
        } else {
            ReplInput::Invalid(RequestError::InvalidCommand)
        }
    } else {
        match (
            request.remove("source"),
            request.remove("reload"),
            request.remove("cancel"),
            request.remove("reply"),
            request.remove("breakpoint"),
        ) {
            (Some(Value::String(source)), None, None, None, None) => {
                let interactive = request
                    .get("interactive")
                    .map_or(Some(false), Value::as_bool);
                let stream = request.get("stream").map_or(Some(false), Value::as_bool);
                let render = request.get("render").map_or(Some(true), Value::as_bool);
                match (interactive, stream, render) {
                    (Some(interactive), Some(stream), Some(render))
                        if (!interactive && !stream) || !id.is_null() =>
                    {
                        let origin = match origin
                            .map(serde_json::from_value::<SourceOrigin>)
                            .transpose()
                        {
                            Ok(origin) => origin,
                            Err(error) => return (id, ReplInput::Invalid(error.into())),
                        };
                        ReplInput::Evaluate(EvaluationRequest {
                            source,
                            origin,
                            mode: Mode {
                                interactive,
                                stream,
                            },
                            render,
                        })
                    }
                    _ => ReplInput::Invalid(RequestError::InvalidMode),
                }
            }
            (None, Some(Value::Bool(true)), None, None, None) => ReplInput::Reload,
            (None, None, Some(target_id), None, None) if !target_id.is_null() => {
                let accepted = pending.request_cancellation(&target_id);
                if accepted {
                    interaction.cancel(&target_id);
                }
                ReplInput::CancelAcknowledgement(accepted)
            }
            (None, None, None, Some(reply), None) => ReplInput::Reply(reply),
            (None, None, None, None, Some(operation)) => {
                serde_json::from_value::<BreakpointRequest>(operation)
                    .map(ReplInput::Breakpoint)
                    .unwrap_or_else(|error| ReplInput::Invalid(error.into()))
            }
            _ => ReplInput::Invalid(RequestError::InvalidCommand),
        }
    };
    let operation = match &command {
        ReplInput::Evaluate(_) => Some("an evaluation"),
        ReplInput::Refresh => Some("a refresh request"),
        ReplInput::Reload => Some("a reload request"),
        _ => None,
    };
    let command = if let Some(operation) = operation
        && !pending.register_queued(&id)
    {
        ReplInput::Invalid(RequestError::DuplicateRequest { operation })
    } else {
        command
    };
    (id, command)
}

pub(super) fn run(startup: &ReplStartup<'_>) -> Result<(), Box<dyn Error>> {
    let io = SessionIo::new();
    let captured_stdout = io.output("stdout");
    let captured_stderr = io.output("stderr");
    let configure_io = |runtime: &mut Runtime| {
        runtime.set_standard_input(io::empty());
        runtime.set_standard_output(captured_stdout.clone());
        runtime.set_error_output(captured_stderr.clone());
        runtime.set_interactive_compiler_feedback(true);
        Ok(())
    };
    let mut runtime = startup.start(configure_io)?;

    // Captures belong to the connection so a failed reload can return output
    // from the candidate while evaluation continues with the active runtime.
    let pending = Arc::new(PendingRequests::default());
    // Keep reading on a separate thread so cancellation can reach a busy runtime.
    // Evaluations, reloads, and all responses stay on this thread in input order.
    let (sender, requests) = mpsc::channel();
    let input_pending = pending.clone();
    let input_interaction = io.interaction.clone();
    let protocol = io.protocol.clone();
    std::thread::Builder::new()
        .name("zkcl-repl-input".into())
        .spawn(move || read_requests(&input_pending, &input_interaction, &protocol, sender))?;
    for request in requests {
        let (id, command) = request?;
        let mut results = EvaluationResults::default();
        let mut object = None;
        let mut profile = None;
        let mut image = None;
        let mut refresh = None;
        let mut paused = None;
        let mut cancelled = None;
        let mut breakpoint = None;
        let error = match command {
            ReplInput::Evaluate(request) => {
                io.activate(id.clone(), request.mode, runtime.cancellation_handle());
                runtime.set_standard_input(io.input());
                evaluate_request(&runtime, &pending, &io, &id, request, &mut results)
            }
            ReplInput::Refresh => {
                let (report, error) = refresh_request(startup, &runtime, &pending, &id);
                refresh = Some(report);
                error
            }
            ReplInput::Reload => {
                let error = reload_request(startup, &mut runtime, &pending, &id, configure_io);
                if error.is_none() {
                    io.objects.clear();
                }
                error
            }
            ReplInput::CancelAcknowledgement(accepted) => {
                cancelled = Some(accepted);
                None
            }
            ReplInput::PauseAcknowledgement(accepted) => {
                paused = Some(accepted);
                None
            }
            ReplInput::Image(operation) => {
                let hooks = zkcl::NamedImageHooks::new(&[
                    ("ZK", "%JSON-DEBUGGER"),
                    ("ZK", "%JSON-INSPECT"),
                    ("ZK", "%REPL-INSPECT"),
                ]);
                let result = match operation {
                    ImageRequest::Save { path } => runtime
                        .save_image_with_hooks(&path, &hooks)
                        .map(|()| json!({"saved":path}))
                        .map_err(|error| Box::new(error) as objects::ObjectError),
                    ImageRequest::Restore { path } => {
                        (|| -> Result<Value, objects::ObjectError> {
                            let mut candidate = startup
                                .start_bare(configure_io)
                                .map_err(|error| error.to_string())?;
                            io.install_tools(&candidate)?;
                            candidate.restore_image_with_hooks(&path, &hooks)?;
                            io.install_tools(&candidate)?;
                            io.objects.clear();
                            startup.replace_with_image(&mut runtime, candidate);
                            Ok(json!({"restored":path,"handlesInvalidated":true}))
                        })()
                    }
                };
                match result {
                    Ok(value) => {
                        image = Some(value);
                        None
                    }
                    Err(error) => Some(boxed_error(&*error)),
                }
            }
            ReplInput::Profile(operation) => match operation.execute(&runtime) {
                Ok(value) => {
                    profile = Some(value);
                    None
                }
                Err(error) => Some(boxed_error(&error)),
            },
            ReplInput::Object(operation) => match io.objects.execute(&runtime, operation) {
                Ok(value) => {
                    object = Some(value);
                    None
                }
                Err(error) => Some(boxed_error(&*error)),
            },
            ReplInput::Invalid(error) => Some(protocol_error(error.to_string())),
            ReplInput::Breakpoint(operation) => match operation.execute(&runtime) {
                Ok(result) => {
                    breakpoint = Some(result);
                    None
                }
                Err(error) => Some(protocol_error(error)),
            },
            ReplInput::Reply(_) => unreachable!("the reader routes replies directly"),
        };
        let mut response = json!({
            "id": id,
            "values": results.text,
            "objects": results.objects,
            "error": error,
            "stdout": captured_stdout.take_text(),
            "stderr": captured_stderr.take_text(),
        });
        io.deactivate();
        if !results.print_errors.is_empty() {
            response["printErrors"] = json!(results.print_errors);
        }
        if let Some(image) = image {
            response["image"] = image;
        }
        if let Some(refresh) = refresh {
            response["refresh"] = json!(refresh);
        }
        if let Some(profile) = profile {
            response["profile"] = profile;
        }
        if let Some(object) = object {
            response["object"] = object;
        }
        if let Some(accepted) = paused {
            response["paused"] = json!(accepted);
        }
        if let Some(accepted) = cancelled {
            response["cancelled"] = json!(accepted);
        }
        if let Some(breakpoint) = breakpoint {
            response["breakpoint"] = breakpoint;
        }
        if let Err(error) = io.protocol.send(&response) {
            // Nobody is reading any more: the session is over, not broken.
            if error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.into());
        }
    }
    Ok(())
}

fn write_response(stdout: &mut impl Write, response: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, response).map_err(io::Error::other)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn refresh_request(
    startup: &ReplStartup<'_>,
    runtime: &Runtime,
    pending: &PendingRequests,
    id: &Value,
) -> (super::RefreshReport, Option<Value>) {
    let mut report = super::RefreshReport::default();
    if !pending.begin(id, runtime) {
        pending.finish(id);
        return (report, Some(cancelled_error("refresh cancelled")));
    }
    let result = startup.refresh(runtime, &mut report);
    let cancelled = pending.finish(id);
    let error = if cancelled && result.is_err() {
        Some(cancelled_error("refresh cancelled"))
    } else {
        result.err().map(|error| boxed_error(error.as_ref()))
    };
    (report, error)
}

fn reload_request(
    startup: &ReplStartup<'_>,
    runtime: &mut Runtime,
    pending: &PendingRequests,
    request_id: &Value,
    configure_io: impl FnOnce(&mut Runtime) -> Result<(), EvalError>,
) -> Option<Value> {
    if pending.retire_cancelled_before_start(request_id) {
        return Some(cancelled_error("reload cancelled"));
    }
    // Build and load the candidate without touching the active session, so a
    // cancelled request publishes none of its work.
    let candidate = startup.prepare(configure_io).and_then(|session| {
        if !pending.begin(request_id, &session.runtime) {
            return Ok(None);
        }
        session.load(startup, &[]).map(Some)
    });
    // Finish under the request lock before committing the session. A cancel
    // accepted before this point can still discard a fully loaded candidate.
    if pending.finish(request_id) {
        return Some(cancelled_error("reload cancelled"));
    }
    match candidate {
        Ok(Some(session)) => {
            // The JSON surface has no interactive report; stderr is how a driver
            // attributes the reload it just triggered.
            eprintln!("[reload] {}", session.timings.report());
            startup.swap_in(runtime, session);
            None
        }
        Err(error) => Some(boxed_error(error.as_ref())),
        Ok(None) => unreachable!("skipped reload was cancelled"),
    }
}

fn cancelled_error(message: &str) -> Value {
    message_error("cancelled", "runtime/cancelled", message)
}

fn protocol_error(message: impl Into<String>) -> Value {
    message_error("protocol", "repl/protocol", message)
}

fn message_error(kind: &str, code: &str, message: impl Into<String>) -> Value {
    let mut report = Diagnostic::new(Severity::Error, message);
    report.code = Some(code.into());
    let mut value = diagnostics::report_json(&report, []);
    value["kind"] = json!(kind);
    value
}

fn evaluate_request(
    runtime: &Runtime,
    pending: &PendingRequests,
    io: &SessionIo,
    request_id: &Value,
    request: EvaluationRequest,
    results: &mut EvaluationResults,
) -> Option<Value> {
    let _finish = DropGuard::new((), |()| {
        pending.finish(request_id);
    });
    if !pending.begin(request_id, runtime) {
        return Some(cancelled_error("evaluation cancelled"));
    }
    let EvaluationRequest {
        source,
        origin,
        mode,
        render,
    } = request;
    let result = (|| {
        let _restore = DropGuard::new((), |()| {
            let _ = runtime.set_debugger(None);
            if mode.interactive {
                let _ = runtime.set_inspector(None);
            }
        });
        {
            let name = match runtime.lookup_symbol("ZK", "%JSON-DEBUGGER")? {
                Some(name) => name,
                None => {
                    io.install_tools(runtime)?;
                    runtime
                        .lookup_symbol("ZK", "%JSON-DEBUGGER")?
                        .expect("JSON debugger installed")
                }
            };
            runtime.set_debugger(Some(&runtime.resolve_function(&name)?))?;
            if mode.interactive {
                let inspector = runtime
                    .lookup_symbol("ZK", "%JSON-INSPECT")?
                    .expect("JSON inspector installed");
                runtime.set_inspector(Some(&inspector))?;
            }
        }
        let document = origin
            .map(|origin| origin.document(&source))
            .unwrap_or_else(|| SourceDocument::input(&source));
        runtime.eval_document_with_offsets::<objects::ObjectError>(&document, |_, form_values| {
            let print = |value: &Root<'_>| runtime.print_result(value, None);
            let values = io.present_values(runtime, form_values, render.then_some(print))?;
            let form = results.text.len();
            results
                .print_errors
                .extend(values.print_errors.into_iter().map(|mut error| {
                    error["form"] = json!(form);
                    error
                }));
            results.text.push(values.text);
            results.objects.push(values.objects);
            Ok(())
        })
    })();
    result.err().map(|error| {
        let mut error = boxed_error(&*error);
        if let Some(condition) = io.condition() {
            error["conditionType"] = condition["type"].clone();
            error["conditionObject"] = condition;
        }
        error
    })
}

fn evaluation_error(report: &Diagnostic, error: &EvalError) -> Value {
    let frames = error.stack_frames();
    let mut error_json = diagnostics::report_json(
        report,
        frames.iter().filter_map(|frame| frame.source.as_ref()),
    );
    let (kind, condition) = match error.kind() {
        EvalErrorKind::Read => ("read", None),
        EvalErrorKind::Compile => ("compile", None),
        EvalErrorKind::MacroExpansion => ("macro-expansion", None),
        EvalErrorKind::LispCondition(class) => ("condition", Some(class.name())),
        EvalErrorKind::HostCallback => ("host-callback", None),
        EvalErrorKind::OutOfMemory => ("out-of-memory", None),
        EvalErrorKind::Cancelled => ("cancelled", None),
        _ => ("runtime", None),
    };
    error_json["kind"] = json!(kind);
    error_json["conditionType"] = json!(condition);
    error_json["debuggerAborted"] = json!(error.debugger_aborted());
    error_json["executionStop"] = json!(error.execution_stop().map(|stop| format!("{stop:?}")));

    if !frames.is_empty() {
        let frames: Vec<_> = frames
            .iter()
            .map(|frame| {
                let range = frame
                    .source_range
                    .as_ref()
                    .map(|range| json!({"start": range.start, "end": range.end}));
                json!({
                    "function": frame.function,
                    "bytecodePc": frame.bytecode_pc,
                    "byte_range": range,
                    "snapshot_id": frame.source.as_ref().map(SourceDocument::id),
                    "native": frame.native,
                })
            })
            .collect();
        error_json["frames"] = json!(frames);
    }
    error_json
}

fn boxed_error(error: &(dyn Error + 'static)) -> Value {
    let report = diagnostics::from_error("", error);
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(error) = error.downcast_ref::<EvalError>() {
            return evaluation_error(&report, error);
        }
        cause = error.source();
    }
    let mut value = diagnostics::report_json(&report, []);
    value["kind"] = json!("protocol");
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_queued_reload_skips_candidate_preparation() {
        let startup = ReplStartup {
            project: None,
            asdf: None,
            preload_asdf: false,
            package: None,
            active: Default::default(),
            module_directories: &[],
            startup_files: &[],
            init_file: None,
            heap_size: None,
            jit_backend: None,
            jit_name: None,
        };
        let mut runtime = Runtime::start().unwrap();
        let pending = PendingRequests::default();
        let id = json!("reload");
        assert!(pending.register_queued(&id));
        assert!(pending.request_cancellation(&id));

        let error = reload_request(&startup, &mut runtime, &pending, &id, |_| {
            panic!("cancelled reload prepared a candidate runtime")
        })
        .expect("a cancelled response");
        assert_eq!(error["kind"], "cancelled");
        assert!(pending.register_queued(&id));
    }
}
