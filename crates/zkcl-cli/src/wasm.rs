use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use thiserror::Error;
use zkcl_wasm::abi::{HostFunction, HostValueType};
use zkcl_wasm::{
    Caller, FuncImport, Host, HostCall, Instance, Limits, Module, Trap, Value, ValueType,
};

#[derive(Debug, Error)]
pub(crate) enum RunError {
    #[error("failed to read Wasm module {path}: {source}", path = .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Engine(#[from] zkcl_wasm::Error),
    #[error("Wasm exit status must be between 0 and 255")]
    ExitStatus,
    #[error("Wasm entry must return no values or an i32 exit status")]
    EntryResult,
    #[error("Wasm module has no run or _start function")]
    MissingEntry,
}

pub(crate) fn run(
    path: &Path,
    args: &[String],
    memory_bytes: Option<usize>,
) -> Result<u8, RunError> {
    let bytes = std::fs::read(path).map_err(|source| RunError::Read {
        path: path.to_owned(),
        source,
    })?;
    let module = Module::parse(&bytes)?;
    let mut limits = Limits::default();
    if let Some(bytes) = memory_bytes {
        limits.memory_bytes = bytes;
    }
    let deadline = Cell::new(None);
    let host = CliHost {
        args,
        started: Instant::now(),
        deadline: &deadline,
    };
    drive(&deadline, async {
        let mut instance = Instance::instantiate(&module, host, limits).await?;
        let entry = ["run", "_start"]
            .into_iter()
            .find(|name| module.exported_function(name).is_some());
        match entry {
            Some(entry) => match instance.invoke(entry, &[]).await?.as_slice() {
                [] => Ok(0),
                [Value::I32(status)] => u8::try_from(*status).map_err(|_| RunError::ExitStatus),
                _ => Err(RunError::EntryResult),
            },
            None if module.has_start() => Ok(0),
            None => Err(RunError::MissingEntry),
        }
    })
}

struct CliHost<'args> {
    args: &'args [String],
    started: Instant,
    deadline: &'args Cell<Option<Instant>>,
}

impl Host for CliHost<'_> {
    type Func = HostFunction;

    fn link(&mut self, import: FuncImport<'_>) -> Result<Self::Func, zkcl_wasm::Error> {
        let function = HostFunction::lookup(import.module, import.name).ok_or_else(|| {
            zkcl_wasm::Error::Import {
                module: import.module.into(),
                name: import.name.into(),
            }
        })?;
        let (params, result) = function.signature();
        let value_type = |value| match value {
            HostValueType::I32 => ValueType::I32,
            HostValueType::I64 => ValueType::I64,
        };
        if import
            .ty
            .params
            .iter()
            .copied()
            .eq(params.iter().copied().map(value_type))
            && import.ty.results.as_ref() == [value_type(result)]
        {
            Ok(function)
        } else {
            Err(zkcl_wasm::Error::Signature(format!(
                "{}.{}",
                import.module, import.name
            )))
        }
    }

    fn call<'a>(
        &'a mut self,
        function: &HostFunction,
        arguments: &[Value],
        mut caller: Caller<'a>,
    ) -> Result<HostCall<'a>, Trap> {
        use HostFunction::*;

        let value = match (function, arguments) {
            (Write, [Value::I32(channel), Value::I32(offset), Value::I32(len)]) => {
                let bytes = caller.read_memory(0, *offset as u32, *len as u32 as usize)?;
                let result = match channel {
                    1 => io::stdout().lock().write_all(bytes),
                    2 => io::stderr().lock().write_all(bytes),
                    _ => return Ok(HostCall::Ready(vec![Value::I32(-1)])),
                };
                Value::I32(result.map_or(-1, |()| *len))
            }
            (Flush, [Value::I32(channel)]) => {
                let result = match channel {
                    1 => io::stdout().lock().flush(),
                    2 => io::stderr().lock().flush(),
                    _ => return Ok(HostCall::Ready(vec![Value::I32(-1)])),
                };
                Value::I32(result.map_or(-1, |()| 0))
            }
            (ArgCount, []) => Value::I32(self.args.len() as i32),
            (ArgLen, [Value::I32(index)]) => Value::I32(
                self.args
                    .get(*index as usize)
                    .map_or(-1, |arg| arg.len() as i32),
            ),
            (ArgRead, [Value::I32(index), Value::I32(offset), Value::I32(capacity)]) => {
                let Some(arg) = self
                    .args
                    .get(*index as usize)
                    .filter(|arg| arg.len() <= *capacity as u32 as usize)
                else {
                    return Ok(HostCall::Ready(vec![Value::I32(-1)]));
                };
                caller.write_memory(0, *offset as u32, arg.as_bytes())?;
                Value::I32(arg.len() as i32)
            }
            (WallTime, []) => {
                let seconds = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs() as i64)
                    .unwrap_or_else(|error| -(error.duration().as_secs() as i64));
                Value::I64(seconds)
            }
            (MonotonicTime, []) => {
                Value::I64(self.started.elapsed().as_nanos().min(i64::MAX as u128) as i64)
            }
            (FillEntropy, [Value::I32(offset), Value::I32(len)]) => {
                let bytes = caller.write_buffer(0, *offset as u32, *len as u32 as usize)?;
                Value::I32(if getrandom::fill(bytes).is_ok() {
                    0
                } else {
                    -1
                })
            }
            (Sleep, [Value::I64(0)]) => Value::I32(0),
            (Sleep, [Value::I64(nanos)]) => {
                let until = u64::try_from(*nanos)
                    .ok()
                    .and_then(|nanos| Instant::now().checked_add(Duration::from_nanos(nanos)));
                let Some(until) = until else {
                    return Ok(HostCall::Ready(vec![Value::I32(-1)]));
                };
                let deadline = self.deadline;
                return Ok(HostCall::Pending(Box::pin(poll_fn(move |_| {
                    if Instant::now() >= until {
                        Poll::Ready(Ok(vec![Value::I32(0)]))
                    } else {
                        deadline.set(Some(until));
                        Poll::Pending
                    }
                }))));
            }
            _ => unreachable!(),
        };
        Ok(HostCall::Ready(vec![value]))
    }
}

struct ThreadWake(Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn drive<F: Future>(deadline: &Cell<Option<Instant>>, future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        deadline.set(None);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(result) => return result,
            Poll::Pending => match deadline.get() {
                Some(until) => {
                    thread::park_timeout(until.saturating_duration_since(Instant::now()))
                }
                None => thread::park(),
            },
        }
    }
}
