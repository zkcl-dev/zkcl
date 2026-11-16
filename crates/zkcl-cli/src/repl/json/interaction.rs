use std::cell::RefCell;
use std::io::{self, BufRead, Cursor, Read, Write};
use std::mem::DropGuard;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};
use zkcl::{CancellationHandle, EvalError, Root, Runtime};

use super::super::{ResultText, classify_result_text};
use super::objects::ObjectError;

#[derive(Clone, Default)]
pub(super) struct Protocol(Arc<Mutex<()>>);

impl Protocol {
    pub(super) fn send(&self, message: &Value) -> io::Result<()> {
        let _serialized = self.0.lock().unwrap();
        super::write_response(&mut io::stdout().lock(), message)
    }
}

#[derive(Default)]
pub(super) struct Interaction(Mutex<State>);

#[derive(Default)]
struct State {
    closed: bool,
    next_token: u64,
    waiting: Option<Waiter>,
}

struct Waiter {
    request: Value,
    token: u64,
    reply: mpsc::Sender<Option<Answer>>,
}

#[derive(Clone)]
pub(super) enum Answer {
    Text(String),
    Action { id: Value, action: Value },
}

impl Interaction {
    pub(super) fn reply(&self, request: &Value, token: u64, answer: Option<Answer>) -> bool {
        let mut state = self.0.lock().unwrap();
        if state
            .waiting
            .as_ref()
            .is_some_and(|waiter| waiter.request == *request && waiter.token == token)
        {
            return state.waiting.take().unwrap().reply.send(answer).is_ok();
        }
        false
    }

    pub(super) fn cancel(&self, request: &Value) {
        let mut state = self.0.lock().unwrap();
        if state
            .waiting
            .as_ref()
            .is_some_and(|waiter| waiter.request == *request)
        {
            state.waiting.take();
        }
    }

    pub(super) fn close(&self) {
        let mut state = self.0.lock().unwrap();
        state.closed = true;
        state.waiting.take();
    }

    fn prompt(
        &self,
        output: &Protocol,
        active: &Active,
        event: &str,
        body: &Value,
    ) -> io::Result<Option<Answer>> {
        let (token, answer) = {
            let mut state = self.0.lock().unwrap();
            if state.closed || active.cancellation.is_cancelled() {
                return Ok(None);
            }
            let (reply, answer) = mpsc::channel();
            state.next_token = state
                .next_token
                .checked_add(1)
                .ok_or_else(|| io::Error::other("interaction token space exhausted"))?;
            let token = state.next_token;
            debug_assert!(state.waiting.is_none());
            state.waiting = Some(Waiter {
                request: active.id.clone(),
                token,
                reply,
            });
            (token, answer)
        };
        if let Err(error) =
            output.send(&json!({"id": active.id, "event": event, "token": token, "data": body}))
        {
            self.cancel(&active.id);
            return Err(error);
        }
        Ok(answer.recv().ok().flatten())
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Mode {
    pub interactive: bool,
    pub stream: bool,
}

#[derive(Clone)]
struct Active {
    id: Value,
    mode: Mode,
    cancellation: CancellationHandle,
}

#[derive(Clone)]
pub(super) struct SessionIo {
    active: Arc<Mutex<Option<Active>>>,
    pub interaction: Arc<Interaction>,
    pub protocol: Protocol,
    pub objects: super::objects::Objects,
    condition: Arc<Mutex<Option<Value>>>,
    printing_values: Arc<AtomicBool>,
}

pub(super) struct PresentedValues {
    pub objects: Vec<Value>,
    pub text: Vec<String>,
    pub print_errors: Vec<Value>,
}

impl SessionIo {
    pub(super) fn new() -> Self {
        Self {
            active: Arc::default(),
            interaction: Arc::default(),
            protocol: Protocol::default(),
            objects: super::objects::Objects::default(),
            condition: Arc::default(),
            printing_values: Arc::default(),
        }
    }

    pub(super) fn activate(&self, id: Value, mode: Mode, cancellation: CancellationHandle) {
        self.condition.lock().unwrap().take();
        *self.active.lock().unwrap() = Some(Active {
            id,
            mode,
            cancellation,
        });
    }

    pub(super) fn deactivate(&self) {
        self.active.lock().unwrap().take();
    }

    pub(super) fn is_interactive(&self) -> bool {
        self.active
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|active| active.mode.interactive)
    }

    pub(super) fn is_printing_values(&self) -> bool {
        self.printing_values.load(Ordering::Relaxed)
    }

    pub(super) fn present_values(
        &self,
        runtime: &Runtime,
        values: &[Root<'_>],
        print: Option<impl Fn(&Root<'_>) -> Result<String, zkcl::EvalError>>,
    ) -> Result<PresentedValues, ObjectError> {
        let mut rendered = Vec::new();
        let mut print_errors = Vec::new();
        if let Some(print) = print {
            let _restore = DropGuard::new(
                self.printing_values.swap(true, Ordering::Relaxed),
                |previous| {
                    self.printing_values.store(previous, Ordering::Relaxed);
                },
            );
            for (index, value) in values.iter().enumerate() {
                match classify_result_text(print(value))? {
                    ResultText::Printed(text) => rendered.push(Some(text)),
                    ResultText::PrinterError(error) => {
                        rendered.push(None);
                        print_errors
                            .push(json!({"index":index,"error":super::boxed_error(&error)}));
                    }
                }
            }
        }
        let objects = self.objects.retain_all(runtime, values)?;
        let mut rendered = rendered.into_iter();
        let text = objects
            .iter()
            .map(|object| {
                rendered.next().flatten().unwrap_or_else(|| {
                    object["preview"]
                        .as_str()
                        .expect("value preview")
                        .to_owned()
                })
            })
            .collect();
        Ok(PresentedValues {
            objects,
            text,
            print_errors,
        })
    }

    pub(super) fn record_condition(&self, condition: Value) {
        *self.condition.lock().unwrap() = Some(condition);
    }
    pub(super) fn condition(&self) -> Option<Value> {
        self.condition.lock().unwrap().clone()
    }

    pub(super) fn input(&self) -> ProtocolInput {
        ProtocolInput {
            io: self.clone(),
            buffer: Cursor::default(),
            closed: false,
        }
    }

    pub(super) fn output(&self, name: &'static str) -> CapturedOutput {
        CapturedOutput {
            io: self.clone(),
            bytes: Rc::default(),
            name,
        }
    }

    pub(super) fn prompt(&self, kind: &str, payload: Value) -> io::Result<Option<String>> {
        Ok(match self.prompt_action(kind, &payload)? {
            Some(Answer::Text(text)) => Some(text),
            _ => None,
        })
    }

    pub(super) fn prompt_action(&self, kind: &str, payload: &Value) -> io::Result<Option<Answer>> {
        let active = self.active.lock().unwrap().clone();
        let Some(active) = active.filter(|active| active.mode.interactive) else {
            return Ok(None);
        };
        self.interaction
            .prompt(&self.protocol, &active, kind, payload)
    }

    pub(super) fn install_tools(&self, runtime: &Runtime) -> Result<(), EvalError> {
        let io = self.clone();
        runtime.register_function("ZK::%JSON-DEBUGGER", move |runtime, args| {
            let [condition] = args else {
                return Err("the debugger expects one condition".into());
            };
            super::debugger::run(&io, runtime, condition)
        })?;
        let io = self.clone();
        runtime.register_function("ZK::%JSON-INSPECT", move |runtime, args| {
            let [object] = args else { return Err("the inspector expects one object".into()); };
            crate::repl::inspector::inspect_session(runtime, object, |page, offset, depth, notice| {
                let parts = page.parts.iter().map(|part| {
                    let value = part.value.as_ref().map(|value| runtime.preview(value)).transpose()?;
                    Ok::<_, EvalError>(json!({"index": part.index, "label": part.label, "value": value, "writable": part.writable}))
                }).collect::<Result<Vec<_>, _>>()?;
                Ok(io.prompt("inspector", json!({
                    "summary": page.summary, "offset": offset, "depth": depth,
                    "total": page.total, "parts": parts, "notice": notice,
                }))?)
            })
        })?;
        Ok(())
    }
}

pub(super) struct ProtocolInput {
    io: SessionIo,
    buffer: Cursor<Vec<u8>>,
    closed: bool,
}

impl Read for ProtocolInput {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let size = self.fill_buf()?.read(bytes)?;
        self.consume(size);
        Ok(size)
    }
}

impl BufRead for ProtocolInput {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if !self.closed && self.buffer.position() as usize == self.buffer.get_ref().len() {
            if let Some(input) = self
                .io
                .prompt("input", Value::Null)?
                .filter(|text| !text.is_empty())
            {
                self.buffer = Cursor::new(input.into_bytes());
            } else {
                self.closed = true;
            }
        }
        self.buffer.fill_buf()
    }

    fn consume(&mut self, count: usize) {
        self.buffer.consume(count);
    }
}

#[derive(Default)]
struct OutputBytes {
    all: Vec<u8>,
    streamed: usize,
}

#[derive(Clone)]
pub(super) struct CapturedOutput {
    io: SessionIo,
    bytes: Rc<RefCell<OutputBytes>>,
    name: &'static str,
}

impl CapturedOutput {
    pub(super) fn take_text(&self) -> String {
        let _ = self.emit(true);
        let bytes = std::mem::take(&mut *self.bytes.borrow_mut()).all;
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn emit(&self, finish: bool) -> io::Result<()> {
        let active = self.io.active.lock().unwrap().clone();
        let Some(active) = active.filter(|active| active.mode.stream) else {
            return Ok(());
        };
        let mut bytes = self.bytes.borrow_mut();
        let pending = &bytes.all[bytes.streamed..];
        let upto = match std::str::from_utf8(pending) {
            Ok(_) => pending.len(),
            Err(error) if !finish && error.error_len().is_none() => error.valid_up_to(),
            Err(_) => pending.len(),
        };
        if upto != 0 {
            let text = String::from_utf8_lossy(&pending[..upto]);
            self.io.protocol.send(
                &json!({"id": active.id, "event": "output", "stream": self.name, "text": text}),
            )?;
            bytes.streamed += upto;
        }
        Ok(())
    }
}

impl Write for CapturedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.borrow_mut().all.extend_from_slice(bytes);
        self.emit(false)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.emit(false)
    }
}
