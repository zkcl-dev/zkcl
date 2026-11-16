//! Debug Adapter Protocol transport and mapping to the persistent JSON runtime.
//!
//! The runtime owns the live stack and all object roots. DAP references below
//! are connection-local aliases, invalidated whenever execution resumes.
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::sync::mpsc;

use serde_json::{Value, json};

pub(crate) fn command(mut args: impl Iterator<Item = OsString>, heap: Option<usize>) -> ExitCode {
    if let Some(arg) = args.next() {
        if (arg == "--help" || arg == "-h") && args.next().is_none() {
            println!("Usage: zkcl dap\nServe the Debug Adapter Protocol over stdio.");
            return ExitCode::SUCCESS;
        }
        eprintln!("Usage: zkcl dap");
        return ExitCode::from(2);
    }
    match run(heap) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("zkcl dap: {error}");
            ExitCode::FAILURE
        }
    }
}

enum Incoming {
    Client(io::Result<Option<Value>>),
    Runtime(io::Result<Value>),
    RuntimeClosed,
    Stderr(String),
}

/// A DAP frame is an ASCII header followed by exactly Content-Length UTF-8 bytes.
fn read_message(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    const MAX_MESSAGE: usize = 16 * 1024 * 1024;
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            return if length.is_none() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete DAP header",
                ))
            };
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid DAP header",
            ));
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate Content-Length",
                ));
            }
            length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
        }
    }
    let length = length.filter(|size| *size <= MAX_MESSAGE).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing or excessive DAP Content-Length",
        )
    })?;
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

fn write_message(output: &mut impl Write, message: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(output, "Content-Length: {}\r\n\r\n", bytes.len())?;
    output.write_all(&bytes)?;
    output.flush()
}

struct Process {
    child: Child,
    input: ChildStdin,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone)]
enum Reference {
    Locals(usize),
    Object(u64),
    Values(Vec<Value>),
}

#[derive(Clone, Copy)]
enum EvaluationContext {
    Console,
    Watch,
    Local,
}

struct Pending {
    request: Value,
    kind: PendingKind,
}

enum PendingKind {
    Action,
    Evaluate,
    Object,
    Breakpoints { path: String, lines: Vec<Value> },
    Pause,
}

struct Adapter {
    seq: u64,
    next_runtime: u64,
    process: Option<Process>,
    pending: HashMap<u64, Pending>,
    streamed: HashMap<u64, [usize; 2]>,
    handles: BTreeSet<u64>,
    action_queue: VecDeque<(Value, Value)>,
    action: Option<u64>,
    awaiting_prompt: bool,
    stopped: Option<Value>,
    references: HashMap<u64, Reference>,
    components: HashMap<(u64, String), usize>,
    next_reference: u64,
    next_frame: u64,
    frames: HashMap<u64, usize>,
    sources: HashMap<u64, String>,
    source_references: HashMap<u64, u64>,
    next_source: u64,
    program: Option<PathBuf>,
    entry: Option<String>,
    eval: Option<u64>,
    configured: bool,
    lines_start_at_one: bool,
    columns_start_at_one: bool,
    exception: Option<Value>,
    next_stop_reason: Option<&'static str>,
    heap: Option<usize>,
    sender: mpsc::Sender<Incoming>,
}

fn run(heap: Option<usize>) -> io::Result<()> {
    let (sender, incoming) = mpsc::channel();
    let client_sender = sender.clone();
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            let message = read_message(&mut input);
            let done = !matches!(message, Ok(Some(_)));
            if client_sender.send(Incoming::Client(message)).is_err() || done {
                break;
            }
        }
    });
    let mut adapter = Adapter {
        seq: 0,
        next_runtime: 0,
        process: None,
        pending: HashMap::new(),
        streamed: HashMap::new(),
        handles: BTreeSet::new(),
        action_queue: VecDeque::new(),
        action: None,
        awaiting_prompt: false,
        stopped: None,
        references: HashMap::new(),
        components: HashMap::new(),
        next_reference: 0,
        next_frame: 0,
        frames: HashMap::new(),
        sources: HashMap::new(),
        source_references: HashMap::new(),
        next_source: 0,
        program: None,
        entry: None,
        eval: None,
        configured: false,
        lines_start_at_one: true,
        columns_start_at_one: true,
        exception: None,
        next_stop_reason: None,
        heap,
        sender,
    };
    let mut output = io::stdout().lock();
    for incoming in incoming {
        match incoming {
            Incoming::Client(Ok(Some(request))) => {
                if adapter.request(request, &mut output)? {
                    break;
                }
            }
            Incoming::Client(Ok(None)) => break,
            Incoming::Client(Err(error)) => return Err(error),
            Incoming::Runtime(Ok(message)) => adapter.runtime(message, &mut output)?,
            Incoming::Runtime(Err(error)) => return Err(error),
            Incoming::Stderr(text) => adapter.event(
                "output",
                json!({"category":"stderr","output":text}),
                &mut output,
            )?,
            Incoming::RuntimeClosed => {
                adapter.event("terminated", json!({}), &mut output)?;
                break;
            }
        }
    }
    Ok(())
}

impl Adapter {
    fn send(&mut self, mut message: Value, output: &mut impl Write) -> io::Result<()> {
        self.seq += 1;
        message["seq"] = json!(self.seq);
        write_message(output, &message)
    }

    fn response(
        &mut self,
        request: &Value,
        body: Value,
        error: Option<&str>,
        output: &mut impl Write,
    ) -> io::Result<()> {
        let mut response = json!({"type":"response","request_seq":request["seq"],"command":request["command"],"success":error.is_none(),"body":body});
        if let Some(error) = error {
            response["message"] = json!(error);
        }
        self.send(response, output)
    }

    fn event(&mut self, event: &str, body: Value, output: &mut impl Write) -> io::Result<()> {
        self.send(json!({"type":"event","event":event,"body":body}), output)
    }

    fn runtime_send(&mut self, mut message: Value, pending: Option<Pending>) -> io::Result<u64> {
        self.next_runtime += 1;
        let id = self.next_runtime;
        message["id"] = json!(id);
        let process = self
            .process
            .as_mut()
            .ok_or_else(|| io::Error::other("debug session has not launched"))?;
        serde_json::to_writer(&mut process.input, &message).map_err(io::Error::other)?;
        process.input.write_all(b"\n")?;
        process.input.flush()?;
        if let Some(pending) = pending {
            self.pending.insert(id, pending);
        }
        Ok(id)
    }

    fn request(&mut self, request: Value, output: &mut impl Write) -> io::Result<bool> {
        if request["type"] != "request" || request["seq"].as_u64().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a DAP request",
            ));
        }
        let args = &request["arguments"];
        match request["command"].as_str().unwrap_or("") {
            "initialize" => {
                if args["pathFormat"]
                    .as_str()
                    .is_some_and(|format| format != "path")
                {
                    self.response(
                        &request,
                        json!({}),
                        Some("zkcl requires pathFormat: path"),
                        output,
                    )?;
                    return Ok(false);
                }
                self.lines_start_at_one = args["linesStartAt1"].as_bool().unwrap_or(true);
                self.columns_start_at_one = args["columnsStartAt1"].as_bool().unwrap_or(true);
                self.response(&request, json!({
                    "supportsConfigurationDoneRequest":true,"supportsEvaluateForHovers":true,
                    "supportsSetVariable":true,"supportsRestartFrame":true,
                    "supportsTerminateRequest":true,"supportsExceptionInfoRequest":true,
                    "supportsConditionalBreakpoints":true,"supportsHitConditionalBreakpoints":true,"supportsLogPoints":true,
                    "supportsValueFormattingOptions":false,"supportsDelayedStackTraceLoading":true,
                    "supportsLoadedSourcesRequest":false,
                    "exceptionBreakpointFilters":[{"filter":"all","label":"Lisp conditions entering the debugger","default":true}]
                }), None, output)?;
            }
            "launch" => {
                if self.process.is_some() {
                    self.response(
                        &request,
                        json!({}),
                        Some("session already launched"),
                        output,
                    )?;
                } else if let Err(error) = self.launch(args) {
                    self.response(&request, json!({}), Some(&error.to_string()), output)?;
                } else {
                    self.response(&request, json!({}), None, output)?;
                    self.event("initialized", json!({}), output)?;
                }
            }
            "configurationDone" => {
                if self.configured || self.process.is_none() {
                    self.response(
                        &request,
                        json!({}),
                        Some("session is not awaiting configuration"),
                        output,
                    )?;
                } else {
                    self.configured = true;
                    self.response(&request, json!({}), None, output)?;
                    let path = self.program.as_ref().unwrap().to_string_lossy();
                    let mut source = format!("(load {})", lisp_string(&path));
                    if let Some(entry) = &self.entry {
                        source.push('\n');
                        source.push_str(entry);
                    }
                    self.eval = Some(self.runtime_send(
                        json!({"source":source,"interactive":true,"stream":true,"render":false}),
                        None,
                    )?);
                }
            }
            "disconnect" | "terminate" => {
                self.process.take();
                self.response(&request, json!({}), None, output)?;
                self.event("terminated", json!({}), output)?;
                return Ok(true);
            }
            "threads" => {
                let threads = if self.process.is_some() {
                    json!([{"id":1,"name":"Lisp runtime"}])
                } else {
                    json!([])
                };
                self.response(&request, json!({"threads":threads}), None, output)?;
            }
            "stackTrace" => self.stack_trace(&request, output)?,
            "source" => {
                if let Some(text) = args["sourceReference"]
                    .as_u64()
                    .and_then(|id| self.sources.get(&id))
                    .cloned()
                {
                    self.response(
                        &request,
                        json!({"content":text,"mimeType":"text/x-common-lisp"}),
                        None,
                        output,
                    )?;
                } else {
                    self.response(
                        &request,
                        json!({}),
                        Some("source snapshot is unavailable"),
                        output,
                    )?;
                }
            }
            "scopes" => {
                if let Some(index) = args["frameId"]
                    .as_u64()
                    .and_then(|id| self.frames.get(&id))
                    .copied()
                {
                    let unavailable = self
                        .stopped
                        .as_ref()
                        .and_then(|stopped| {
                            stopped["data"]["frames"][index]["localsAvailability"].as_str()
                        })
                        .map(str::to_owned);
                    let (name, reference) = if let Some(reason) = unavailable {
                        (format!("Locals unavailable: {reason}"), 0)
                    } else {
                        (
                            "Locals".to_owned(),
                            self.reference(Reference::Locals(index)),
                        )
                    };
                    self.response(&request, json!({"scopes":[{"name":name,"presentationHint":"locals","variablesReference":reference,"expensive":false}]}), None, output)?;
                } else {
                    self.response(
                        &request,
                        json!({}),
                        Some("frame is no longer active"),
                        output,
                    )?;
                }
            }
            "variables" => self.variables(request.clone(), output)?,
            "evaluate" => self.evaluate(request, output)?,
            "setVariable" => self.set_variable(request.clone(), output)?,
            "continue" | "next" | "stepIn" | "stepOut" | "restartFrame" => {
                let action = match request["command"].as_str().unwrap() {
                    "continue" => json!({"kind":"motion","motion":"continue"}),
                    "next" => json!({"kind":"motion","motion":"next"}),
                    "stepIn" => json!({"kind":"motion","motion":"into"}),
                    "stepOut" => json!({"kind":"motion","motion":"out"}),
                    _ => match self.frame_argument(args) {
                        Some(frame) => json!({"kind":"frame-restart","frame":frame,"stop":true}),
                        None => {
                            self.response(
                                &request,
                                json!({}),
                                Some("frame is no longer active"),
                                output,
                            )?;
                            return Ok(false);
                        }
                    },
                };
                self.queue_action(request.clone(), action, output)?;
            }
            "pause" => {
                if let Some(eval) = self.eval {
                    self.runtime_send(
                        json!({"pause":eval}),
                        Some(Pending {
                            request,
                            kind: PendingKind::Pause,
                        }),
                    )?;
                } else {
                    self.response(
                        &request,
                        json!({}),
                        Some("runtime is not executing"),
                        output,
                    )?;
                }
            }
            "setBreakpoints" => self.breakpoints(request.clone(), output)?,
            "setFunctionBreakpoints" => {
                self.response(
                    &request,
                    json!({}),
                    Some(
                        "use source breakpoints; function breakpoint replacement is not supported",
                    ),
                    output,
                )?;
            }
            "setExceptionBreakpoints" => {
                let enabled = args["filters"]
                    .as_array()
                    .is_none_or(|filters| filters.iter().any(|filter| filter == "all"));
                if enabled {
                    self.response(&request, json!({"breakpoints":[]}), None, output)?;
                } else {
                    self.response(&request,json!({}),Some("Lisp debugger conditions cannot be disabled for an interactive session"),output)?;
                }
            }
            "exceptionInfo" => {
                if let Some(exception) = self.exception.clone() {
                    self.response(&request, exception, None, output)?;
                } else {
                    self.response(
                        &request,
                        json!({}),
                        Some("runtime is not stopped on an exception"),
                        output,
                    )?;
                }
            }
            _ => self.response(&request, json!({}), Some("unsupported DAP request"), output)?,
        }
        Ok(false)
    }

    fn launch(&mut self, args: &Value) -> io::Result<()> {
        let program = args["program"]
            .as_str()
            .ok_or_else(|| io::Error::other("launch requires a Lisp program path"))?;
        let cwd = args["cwd"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or(std::env::current_dir()?);
        let program = if Path::new(program).is_absolute() {
            PathBuf::from(program)
        } else {
            cwd.join(program)
        };
        let program = program.canonicalize()?;
        let mut command = Command::new(std::env::current_exe()?);
        if let Some(bytes) = self.heap {
            command.args(["--heap-size-mib", &(bytes / 1024 / 1024).to_string()]);
        }
        command
            .args(["repl", "--json", "--no-init"])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dirs) = args["moduleDirectories"].as_array() {
            for dir in dirs {
                command.arg("-I").arg(
                    dir.as_str()
                        .ok_or_else(|| io::Error::other("moduleDirectories must contain paths"))?,
                );
            }
        }
        let mut child = command.spawn()?;
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let message =
                    line.and_then(|line| serde_json::from_str(&line).map_err(io::Error::other));
                if sender.send(Incoming::Runtime(message)).is_err() {
                    return;
                }
            }
            let _ = sender.send(Incoming::RuntimeClosed);
        });
        let sender = self.sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if sender.send(Incoming::Stderr(format!("{line}\n"))).is_err() {
                    break;
                }
            }
        });
        self.program = Some(program);
        self.entry = args["entry"].as_str().map(str::to_owned);
        self.process = Some(Process { child, input });
        Ok(())
    }

    fn frame_argument(&self, args: &Value) -> Option<usize> {
        args["frameId"]
            .as_u64()
            .and_then(|id| self.frames.get(&id))
            .copied()
    }

    fn evaluate(&mut self, request: Value, output: &mut impl Write) -> io::Result<()> {
        let args = &request["arguments"];
        let Some(source) = args["expression"].as_str() else {
            return self.response(&request, json!({}), Some("expression is required"), output);
        };
        let context = match args.get("context") {
            None => EvaluationContext::Console,
            Some(context) => match context.as_str() {
                Some("repl") => EvaluationContext::Console,
                Some("watch") => EvaluationContext::Watch,
                Some("hover" | "variables") => EvaluationContext::Local,
                _ => {
                    return self.response(
                        &request,
                        json!({}),
                        Some("unsupported evaluation context"),
                        output,
                    );
                }
            },
        };
        let frame = if args.get("frameId").is_some() {
            let Some(frame) = self.frame_argument(args) else {
                return self.response(
                    &request,
                    json!({}),
                    Some("frame is no longer active"),
                    output,
                );
            };
            frame
        } else {
            0
        };
        let render = matches!(context, EvaluationContext::Console);
        if self.stopped.is_some() {
            let action = match context {
                EvaluationContext::Local => {
                    json!({"kind":"local","frame":frame,"expression":source})
                }
                _ => json!({"kind":"eval","frame":frame,"source":source,"render":render}),
            };
            return self.queue_action(request, action, output);
        }
        let error = if matches!(context, EvaluationContext::Local) {
            "local variables are available while paused"
        } else if self.eval.is_some() {
            "pause execution before evaluating"
        } else if self.process.is_none() {
            "session has not launched"
        } else {
            self.runtime_send(
                json!({"source":source,"stream":true,"render":render}),
                Some(Pending {
                    request,
                    kind: PendingKind::Evaluate,
                }),
            )?;
            return Ok(());
        };
        self.response(&request, json!({}), Some(error), output)
    }

    fn reference(&mut self, reference: Reference) -> u64 {
        self.next_reference += 1;
        self.references.insert(self.next_reference, reference);
        self.next_reference
    }

    fn object_value(&mut self, value: &Value) -> Value {
        let reference = value["handle"]
            .as_u64()
            .map(|handle| self.reference(Reference::Object(handle)))
            .unwrap_or(0);
        let mut result = json!({"value":value["preview"].as_str().unwrap_or("<unavailable>"),"variablesReference":reference});
        if let Some(kind) = value["type"].as_str() {
            result["type"] = json!(kind);
        }
        result
    }

    fn stack_trace(&mut self, request: &Value, output: &mut impl Write) -> io::Result<()> {
        let Some(stopped) = self.stopped.as_ref() else {
            return self.response(request, json!({}), Some("runtime is not paused"), output);
        };
        let frames = stopped["data"]["frames"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let total = frames.len();
        let start = request["arguments"]["startFrame"].as_u64().unwrap_or(0) as usize;
        let levels = request["arguments"]["levels"]
            .as_u64()
            .filter(|n| *n != 0)
            .unwrap_or(total as u64) as usize;
        let mut stack = Vec::new();
        for (index, frame) in frames.iter().enumerate().skip(start).take(levels) {
            let id = self
                .frames
                .iter()
                .find_map(|(id, old)| (*old == index).then_some(*id))
                .unwrap_or_else(|| {
                    self.next_frame += 1;
                    self.frames.insert(self.next_frame, index);
                    self.next_frame
                });
            let path = frame["sourcePath"]
                .as_str()
                .or_else(|| frame["source"].as_str());
            let line =
                frame["line"].as_u64().unwrap_or(1).max(1) - u64::from(!self.lines_start_at_one);
            let character_column = frame["column"].as_u64().unwrap_or(1).max(1) as usize;
            let utf16_column = frame["text"]
                .as_str()
                .map(|text| {
                    text.chars()
                        .take(character_column - 1)
                        .map(char::len_utf16)
                        .sum::<usize>()
                        + 1
                })
                .unwrap_or(character_column);
            let column = utf16_column as u64 - u64::from(!self.columns_start_at_one);
            let mut entry = json!({"id":id,"name":frame["function"].as_str().unwrap_or("<anonymous>"),"line":line,"column":column,"canRestart":frame["canRestart"].as_bool().unwrap_or(false)});
            if let Some(path) = path.filter(|path| Path::new(path).is_absolute()) {
                entry["source"] = json!({"name":Path::new(path).file_name().map(|name| name.to_string_lossy()),"path":path});
            } else {
                entry["presentationHint"] = json!("subtle");
                entry["line"] = json!(0);
                entry["column"] = json!(0);
            }
            if let (Some(snapshot), Some(text)) =
                (frame["sourceId"].as_u64(), frame["sourceText"].as_str())
            {
                let reference = if let Some(reference) = self.source_references.get(&snapshot) {
                    *reference
                } else {
                    self.next_source += 1;
                    self.source_references.insert(snapshot, self.next_source);
                    self.sources.insert(self.next_source, text.to_owned());
                    self.next_source
                };
                if entry["source"].is_null() {
                    entry["source"] = json!({"name":format!("Lisp submission {snapshot}")});
                }
                entry["source"]["sourceReference"] = json!(reference);
                entry["line"] = json!(line);
                entry["column"] = json!(column);
            }
            stack.push(entry);
        }
        self.response(
            request,
            json!({"stackFrames":stack,"totalFrames":total}),
            None,
            output,
        )
    }

    fn variables(&mut self, request: Value, output: &mut impl Write) -> io::Result<()> {
        let reference = request["arguments"]["variablesReference"]
            .as_u64()
            .and_then(|id| self.references.get(&id))
            .cloned();
        match reference {
            Some(Reference::Locals(frame)) => {
                self.queue_action(request, json!({"kind":"locals","frame":frame}), output)
            }
            Some(Reference::Values(values)) => {
                let args = &request["arguments"];
                let start = args["start"].as_u64().unwrap_or(0) as usize;
                let count = args["count"]
                    .as_u64()
                    .filter(|count| *count != 0)
                    .unwrap_or(values.len() as u64) as usize;
                let variables: Vec<_> = values
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(count)
                    .filter(|_| args["filter"] != "named")
                    .map(|(index, value)| {
                        let mut variable = self.object_value(value);
                        variable["name"] = json!(format!("[{index}]"));
                        variable["presentationHint"] = json!({"attributes":["readOnly"]});
                        variable
                    })
                    .collect();
                self.response(&request, json!({"variables":variables}), None, output)
            }
            Some(Reference::Object(handle)) => {
                let args = &request["arguments"];
                let object = json!({"action":"inspect","handle":handle,"start":args["start"].as_u64().unwrap_or(0),"count":args["count"].as_u64().filter(|count| *count != 0).unwrap_or(100)});
                if self.stopped.is_some() {
                    self.queue_action(request, json!({"kind":"object","object":object}), output)
                } else {
                    self.runtime_send(
                        json!({"object":object}),
                        Some(Pending {
                            request,
                            kind: PendingKind::Object,
                        }),
                    )?;
                    Ok(())
                }
            }
            None => self.response(
                &request,
                json!({}),
                Some("variable reference has expired"),
                output,
            ),
        }
    }

    fn set_variable(&mut self, request: Value, output: &mut impl Write) -> io::Result<()> {
        let args = &request["arguments"];
        let reference = args["variablesReference"]
            .as_u64()
            .and_then(|id| self.references.get(&id))
            .cloned();
        match reference {
            Some(Reference::Locals(frame)) => {
                let action =
                    json!({"kind":"set","frame":frame,"name":args["name"],"source":args["value"]});
                self.queue_action(request, action, output)
            }
            Some(Reference::Values(_)) => self.response(
                &request,
                json!({}),
                Some("evaluation results are read-only"),
                output,
            ),
            Some(Reference::Object(handle)) => {
                let name = args["name"].as_str().unwrap_or("");
                if let Some(index) = self.components.get(&(handle, name.to_owned())).copied() {
                    let action = json!({"kind":"object","object":{"action":"modify","handle":handle,"index":index,"value":{"source":args["value"]}}});
                    if self.stopped.is_some() {
                        self.queue_action(request, action, output)
                    } else {
                        self.runtime_send(
                            json!({"object":action["object"]}),
                            Some(Pending {
                                request,
                                kind: PendingKind::Object,
                            }),
                        )?;
                        Ok(())
                    }
                } else {
                    self.response(
                        &request,
                        json!({}),
                        Some("component is not available in the current variables page"),
                        output,
                    )
                }
            }
            None => self.response(
                &request,
                json!({}),
                Some("variable reference has expired"),
                output,
            ),
        }
    }

    fn queue_action(
        &mut self,
        request: Value,
        action: Value,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if self.stopped.is_none() {
            return self.response(&request, json!({}), Some("runtime is not paused"), output);
        }
        self.action_queue.push_back((request, action));
        self.next_action()
    }

    fn next_action(&mut self) -> io::Result<()> {
        if self.action.is_some() || self.awaiting_prompt {
            return Ok(());
        }
        let Some((request, mut action)) = self.action_queue.pop_front() else {
            return Ok(());
        };
        if matches!(action["kind"].as_str(), Some("motion" | "frame-restart")) {
            action["release"] = json!(&self.handles);
        }
        let stopped = self.stopped.as_ref().unwrap();
        let message =
            json!({"reply":{"request":stopped["id"],"token":stopped["token"],"action":action}});
        let id = self.runtime_send(
            message,
            Some(Pending {
                request,
                kind: PendingKind::Action,
            }),
        )?;
        self.action = Some(id);
        self.awaiting_prompt = true;
        Ok(())
    }

    fn breakpoints(&mut self, request: Value, output: &mut impl Write) -> io::Result<()> {
        let Some(path) = request["arguments"]["source"]["path"].as_str() else {
            return self.response(&request, json!({}), Some("source.path is required"), output);
        };
        if self.eval.is_some() && self.stopped.is_none() {
            return self.response(
                &request,
                json!({}),
                Some("pause execution before changing breakpoints"),
                output,
            );
        }
        let path = path.to_owned();
        let lines = request["arguments"]["breakpoints"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut breakpoints = Vec::new();
        for point in &lines {
            let mut options = json!({});
            if let Some(condition) = point["condition"].as_str() {
                options["condition"] = json!(condition);
            }
            if let Some(hit) = point["hitCondition"].as_str() {
                let Some(hit) = hit.parse::<u64>().ok().filter(|count| *count > 0) else {
                    return self.response(&request,json!({"breakpoints":lines.iter().map(|point|json!({"verified":false,"line":point["line"],"message":"hitCondition must be a positive integer"})).collect::<Vec<_>>()}),None,output);
                };
                options["hitCount"] = json!(hit);
                options["hitEvery"] = json!(1);
            }
            if let Some(log) = point["logMessage"].as_str() {
                match log_expression(log){
                    Ok(expression)=>{options["log"]=json!(expression);options["logOnly"]=json!(true);}
                    Err(error)=>return self.response(&request,json!({"breakpoints":lines.iter().map(|point|json!({"verified":false,"line":point["line"],"message":error})).collect::<Vec<_>>()}),None,output),
                }
            }
            breakpoints.push(json!({"line":point["line"].as_u64().unwrap_or(1)+u64::from(!self.lines_start_at_one),"options":options}));
        }
        let operation = json!({"action":"source-set","path":path,"breakpoints":breakpoints});
        if self.stopped.is_some() {
            return self.queue_action(
                request,
                json!({"kind":"breakpoint","breakpoint":operation}),
                output,
            );
        }
        self.runtime_send(
            json!({"breakpoint":operation}),
            Some(Pending {
                request,
                kind: PendingKind::Breakpoints { path, lines },
            }),
        )?;
        Ok(())
    }

    fn runtime(&mut self, message: Value, output: &mut impl Write) -> io::Result<()> {
        collect_handles(&message, &mut self.handles);
        match message["event"].as_str() {
            Some("output") => {
                if let (Some(id), Some(text)) = (message["id"].as_u64(), message["text"].as_str()) {
                    let stream = usize::from(message["stream"] == "stderr");
                    self.streamed.entry(id).or_default()[stream] += text.len();
                }
                return self.event(
                    "output",
                    json!({"category":message["stream"],"output":message["text"]}),
                    output,
                );
            }
            Some("input") => {
                self.event("output",json!({"category":"console","output":"Lisp is waiting for standard input. Use the debug console expression (e.g. redefine the input source), or terminate the session.\n"}),output)?;
                // DAP has no generic stdin request. EOF is explicit and must not
                // leave the runtime waiting on an unanswerable protocol prompt.
                self.runtime_send(
                    json!({"reply":{"request":message["id"],"token":message["token"],"text":null}}),
                    None,
                )?;
                return Ok(());
            }
            Some("debugger") => {
                self.awaiting_prompt = false;
                let result = message["data"]["result"].clone();
                let was_stopped = self.stopped.is_some();
                self.stopped = Some(message.clone());
                if let Some(id) = result["id"].as_u64().or(self.action) {
                    if let Some(pending) = self.pending.remove(&id) {
                        self.action = None;
                        self.action_result(pending, &message, &result, output)?;
                    }
                }
                if !was_stopped {
                    let condition = message["data"]["condition"]
                        .as_str()
                        .unwrap_or("Lisp debugger");
                    let pending_reason = self.next_stop_reason.take();
                    let reason = if condition.starts_with("Breakpoint") {
                        "breakpoint"
                    } else if let Some(reason) = pending_reason {
                        reason
                    } else if condition.to_ascii_lowercase().contains("pause")
                        || condition.to_ascii_lowercase().contains("interrupt")
                    {
                        "pause"
                    } else if condition.to_ascii_lowercase().contains("step") {
                        "step"
                    } else {
                        "exception"
                    };
                    self.exception = (reason == "exception").then(|| json!({"exceptionId":message["data"]["conditionType"].as_str().unwrap_or("Lisp condition"),"description":condition,"breakMode":"always"}));
                    let mut body = json!({"reason":reason,"description":condition,"threadId":1,"allThreadsStopped":true});
                    if let Some(number) = condition
                        .strip_prefix("Breakpoint ")
                        .and_then(|text| text.split_whitespace().next())
                        .and_then(|number| number.parse::<u64>().ok())
                    {
                        body["hitBreakpointIds"] = json!([number]);
                        self.event(
                            "breakpoint",
                            json!({"reason":"changed","breakpoint":{"id":number,"verified":true}}),
                            output,
                        )?;
                    }
                    self.event("stopped", body, output)?;
                }
                return self.next_action();
            }
            Some("debugger-action") => {
                let Some(id) = message["id"].as_u64() else {
                    return Ok(());
                };
                let Some(pending) = self.pending.remove(&id) else {
                    return Ok(());
                };
                self.action = None;
                if message["resumed"] == true && message["result"]["error"].is_null() {
                    self.awaiting_prompt = false;
                    self.clear_pause();
                    self.handles.clear();
                    self.next_stop_reason = match pending.request["command"].as_str() {
                        Some("restartFrame") => Some("restart"),
                        Some("next" | "stepIn" | "stepOut") => Some("step"),
                        _ => None,
                    };
                    let body = if pending.request["command"] == "continue" {
                        json!({"allThreadsContinued":true})
                    } else {
                        json!({})
                    };
                    self.response(&pending.request, body, None, output)?;
                    self.event(
                        "continued",
                        json!({"threadId":1,"allThreadsContinued":true}),
                        output,
                    )?;
                    while let Some((request, _)) = self.action_queue.pop_front() {
                        self.response(
                            &request,
                            json!({}),
                            Some("execution resumed before this request could run"),
                            output,
                        )?;
                    }
                } else {
                    let snapshot = self.stopped.clone().unwrap_or(Value::Null);
                    self.action_result(pending, &snapshot, &message["result"], output)?;
                }
                return Ok(());
            }
            Some("reply") => {
                let Some(id) = message["id"].as_u64() else {
                    return Ok(());
                };
                if message["accepted"] == false {
                    if let Some(pending) = self.pending.remove(&id) {
                        self.action = None;
                        self.response(
                            &pending.request,
                            json!({}),
                            Some("debugger action was rejected"),
                            output,
                        )?;
                    }
                }
                return Ok(());
            }
            Some(_) => return Ok(()),
            None => {}
        }
        let Some(id) = message["id"].as_u64() else {
            return Ok(());
        };
        let streamed = self.streamed.remove(&id).unwrap_or_default();
        for (index, name) in ["stdout", "stderr"].into_iter().enumerate() {
            if let Some(text) = message[name]
                .as_str()
                .and_then(|text| text.get(streamed[index]..))
                .filter(|text| !text.is_empty())
            {
                self.event("output", json!({"category":name,"output":text}), output)?;
            }
        }
        if self.eval == Some(id) {
            self.eval = None;
            self.clear_pause();
            if !self.handles.is_empty() {
                let handles = std::mem::take(&mut self.handles);
                self.runtime_send(
                    json!({"object":{"action":"release","handles":handles}}),
                    None,
                )?;
            }
            if !message["error"].is_null() {
                self.event("output",json!({"category":"stderr","output":format!("{}\n",message["error"]["message"].as_str().unwrap_or("evaluation failed"))}),output)?;
            }
            self.event(
                "exited",
                json!({"exitCode":if message["error"].is_null(){0}else{1}}),
                output,
            )?;
            self.event("terminated", json!({}), output)?;
            // Keep process ownership until disconnect, allowing inspect/evaluate
            // responses already in flight to drain in wire order.
            return Ok(());
        }
        let Some(pending) = self.pending.remove(&id) else {
            return Ok(());
        };
        self.report_print_errors(&message["printErrors"], output)?;
        if !message["error"].is_null() {
            return self.response(
                &pending.request,
                json!({}),
                Some(
                    message["error"]["message"]
                        .as_str()
                        .unwrap_or("runtime request failed"),
                ),
                output,
            );
        }
        match pending.kind {
            PendingKind::Pause => self.response(
                &pending.request,
                json!({}),
                if message["paused"] == false {
                    Some("runtime could not be paused")
                } else {
                    None
                },
                output,
            ),
            PendingKind::Evaluate => {
                let rows = message["objects"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let body = self.evaluation_body(
                    rows.iter()
                        .enumerate()
                        .map(|(index, objects)| (objects, &message["values"][index])),
                );
                self.response(&pending.request, body, None, output)
            }
            PendingKind::Object => {
                if pending.request["command"] == "setVariable" {
                    let body = self.object_value(&message["object"]);
                    return self.response(&pending.request, body, None, output);
                }
                let parts = message["object"]["parts"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                self.remember_components(&pending.request, &parts);
                let variables: Vec<_> = parts
                    .iter()
                    .map(|part| {
                        let mut value = self.object_value(&part["value"]);
                        value["name"] = part["label"].clone();
                        value
                    })
                    .collect();
                self.response(
                    &pending.request,
                    json!({"variables":variables}),
                    None,
                    output,
                )
            }
            PendingKind::Breakpoints { path, lines } => {
                let resolved = message["breakpoint"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let points = self.dap_breakpoints(&resolved, &path, &lines);
                self.response(
                    &pending.request,
                    json!({"breakpoints":points}),
                    None,
                    output,
                )
            }
            PendingKind::Action => self.response(
                &pending.request,
                json!({}),
                Some("debugger action ended without a result"),
                output,
            ),
        }
    }

    fn action_result(
        &mut self,
        pending: Pending,
        message: &Value,
        result: &Value,
        output: &mut impl Write,
    ) -> io::Result<()> {
        if !result["error"].is_null() {
            let error = result["error"]["message"]
                .as_str()
                .or_else(|| result["error"].as_str())
                .unwrap_or("debugger operation failed");
            return self.response(&pending.request, json!({}), Some(error), output);
        }
        match pending.request["command"].as_str() {
            Some("setBreakpoints") => {
                let path = pending.request["arguments"]["source"]["path"]
                    .as_str()
                    .unwrap_or("");
                let lines = pending.request["arguments"]["breakpoints"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let points = self.dap_breakpoints(
                    result["value"].as_array().map(Vec::as_slice).unwrap_or(&[]),
                    path,
                    &lines,
                );
                self.response(
                    &pending.request,
                    json!({"breakpoints":points}),
                    None,
                    output,
                )
            }
            Some("variables") => {
                if let Some(parts) = result["value"]["parts"].as_array() {
                    self.remember_components(&pending.request, parts);
                    let variables: Vec<_> = parts
                        .iter()
                        .map(|part| {
                            let mut value = self.object_value(&part["value"]);
                            value["name"] = part["label"].clone();
                            value
                        })
                        .collect();
                    return self.response(
                        &pending.request,
                        json!({"variables":variables}),
                        None,
                        output,
                    );
                }
                let locals = result["value"]["variables"]
                    .as_array()
                    .or_else(|| message["data"]["variables"].as_array())
                    .cloned()
                    .unwrap_or_default();
                let variables: Vec<_> = locals
                    .iter()
                    .map(|local| {
                        let mut value = self.object_value(&local["value"]);
                        value["name"] = local["name"].clone();
                        if local["writable"] == false {
                            value["presentationHint"] = json!({"attributes":["readOnly"]});
                        }
                        if let Some(reason) = local["unavailable"].as_str() {
                            value["value"] = json!(format!("<unavailable: {reason}>"));
                        }
                        value
                    })
                    .collect();
                self.response(
                    &pending.request,
                    json!({"variables":variables}),
                    None,
                    output,
                )
            }
            Some("evaluate") => {
                let value = &result["value"];
                self.report_print_errors(&value["printErrors"], output)?;
                let body = if matches!(
                    pending.request["arguments"]["context"].as_str(),
                    Some("hover" | "variables")
                ) {
                    let Some(preview) = value["preview"].as_str() else {
                        return self.response(
                            &pending.request,
                            json!({}),
                            Some("no matching local variable"),
                            output,
                        );
                    };
                    json!({"result":preview,"variablesReference":0})
                } else {
                    self.evaluation_body([(&value["values"], &value["renderedValues"])])
                };
                self.response(&pending.request, body, None, output)
            }
            Some("setVariable") => {
                let value = if result["value"]["values"].is_array() {
                    &result["value"]["values"][0]
                } else {
                    &result["value"]
                };
                let body = self.object_value(value);
                self.response(&pending.request, body, None, output)
            }
            _ => self.response(&pending.request, json!({}), None, output),
        }
    }

    fn remember_components(&mut self, request: &Value, parts: &[Value]) {
        let handle = request["arguments"]["variablesReference"]
            .as_u64()
            .and_then(|id| self.references.get(&id))
            .and_then(|reference| match reference {
                Reference::Object(handle) => Some(*handle),
                _ => None,
            });
        if let Some(handle) = handle {
            for part in parts {
                if let (Some(label), Some(index)) = (part["label"].as_str(), part["index"].as_u64())
                {
                    self.components
                        .insert((handle, label.to_owned()), index as usize);
                }
            }
        }
    }

    fn clear_pause(&mut self) {
        self.stopped = None;
        self.references.clear();
        self.components.clear();
        self.frames.clear();
        self.exception = None;
    }

    fn report_print_errors(&mut self, errors: &Value, output: &mut impl Write) -> io::Result<()> {
        if let Some(errors) = errors.as_array() {
            for error in errors {
                if let Some(message) = error["error"]["message"].as_str() {
                    self.event(
                        "output",
                        json!({"category":"stderr","output":format!("Error printing result ({message})\n")}),
                        output,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn evaluation_body<'a>(
        &mut self,
        rows: impl IntoIterator<Item = (&'a Value, &'a Value)>,
    ) -> Value {
        let mut lines = Vec::new();
        let mut values = Vec::new();
        for (objects, rendered) in rows {
            let objects = objects.as_array().map(Vec::as_slice).unwrap_or(&[]);
            if objects.is_empty() {
                lines.push("<no values>".to_owned());
                continue;
            }
            let texts: Vec<_> = objects
                .iter()
                .enumerate()
                .map(|(index, object)| {
                    rendered[index]
                        .as_str()
                        .or_else(|| object["preview"].as_str())
                        .unwrap_or("<unavailable>")
                })
                .collect();
            lines.push(texts.join("; "));
            values.extend(objects.iter().cloned());
        }
        let mut body = json!({"result":lines.join("\n"),"variablesReference":0});
        match values.as_slice() {
            [] => {}
            [value] => {
                let object = self.object_value(value);
                body["variablesReference"] = object["variablesReference"].clone();
                if let Some(kind) = object.get("type") {
                    body["type"] = kind.clone();
                }
            }
            _ => {
                body["indexedVariables"] = json!(values.len());
                body["variablesReference"] = json!(self.reference(Reference::Values(values)));
            }
        }
        body
    }

    fn dap_breakpoints(&self, resolved: &[Value], path: &str, lines: &[Value]) -> Vec<Value> {
        lines.iter().enumerate().map(|(index,line)|{
            let result=resolved.get(index).unwrap_or(&Value::Null);
            let mut point=json!({"verified":result["verified"].as_bool().unwrap_or(false),"line":result["line"].as_u64().map(|line|line.saturating_sub(u64::from(!self.lines_start_at_one))).unwrap_or_else(||line["line"].as_u64().unwrap_or(1)),"source":{"path":path}});
            if result["number"].is_u64(){point["id"]=result["number"].clone();}
            if result["message"].is_string(){point["message"]=result["message"].clone();}
            point
        }).collect()
    }
}

fn lisp_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// DAP logpoints interpolate Lisp forms inside braces; FORMAT's own marker is escaped.
fn log_expression(template: &str) -> Result<String, &'static str> {
    let mut format = String::new();
    let mut forms = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        format.push_str(&rest[..start].replace('~', "~~"));
        let tail = &rest[start + 1..];
        let end = tail.find('}').ok_or("unclosed expression in logMessage")?;
        let form = tail[..end].trim();
        if form.is_empty() {
            return Err("empty expression in logMessage");
        }
        forms.push(form);
        format.push_str("~A");
        rest = &tail[end + 1..];
    }
    format.push_str(&rest.replace('~', "~~"));
    Ok(format!(
        "(cl:format cl:nil {} {})",
        lisp_string(&format),
        forms.join(" ")
    ))
}

fn collect_handles(value: &Value, handles: &mut BTreeSet<u64>) {
    match value {
        Value::Object(fields) => {
            if let Some(handle) = fields.get("handle").and_then(Value::as_u64) {
                handles.insert(handle);
            }
            for value in fields.values() {
                collect_handles(value, handles);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_handles(value, handles);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dap_frames_count_utf8_bytes_and_preserve_back_to_back_messages() {
        let first = json!({"type":"event","event":"output","body":{"output":"雪"}});
        let second = json!({"type":"request","seq":2,"command":"threads"});
        let mut bytes = Vec::new();
        write_message(&mut bytes, &first).unwrap();
        write_message(&mut bytes, &second).unwrap();
        let mut input = io::Cursor::new(bytes);
        assert_eq!(read_message(&mut input).unwrap(), Some(first));
        assert_eq!(read_message(&mut input).unwrap(), Some(second));
        assert!(read_message(&mut input).unwrap().is_none());
    }
    #[test]
    fn malformed_dap_lengths_do_not_allocate_unbounded_buffers() {
        for header in [
            "Content-Length: 99999999999\r\n\r\n",
            "Content-Length: 1\r\nContent-Length: 1\r\n\r\n0",
        ] {
            assert!(read_message(&mut io::Cursor::new(header)).is_err());
        }
    }
}
