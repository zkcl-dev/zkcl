//! Run a Wasm module against a stub `zkcl.host` and report how many
//! instructions the engine retired.
//!
//! This is the measurement side of the `step-counter` instrument described in
//! `designs/wasm-backend.md`: the feature makes `WasmFrame::execute` bump one
//! counter per dispatched instruction, and this runner prints that tally next
//! to the elapsed time, so the cost per instruction is measured instead of
//! inferred from wall time.
//!
//! ```sh
//! cargo run -p zkcl-wasm --features step-counter,abi \
//!     --example run_module -- path/to/module.wasm
//! ```

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use zkcl_wasm::abi::{HostFunction, HostValueType};
use zkcl_wasm::{
    Caller, Error, FuncImport, Host, HostCall, Instance, Limits, Module, Trap, Value, ValueType,
};

/// Host that satisfies every `zkcl.host` import without I/O, so the tally and
/// the elapsed time cover engine dispatch only.
struct StubHost;

impl Host for StubHost {
    type Func = HostFunction;

    fn link(&mut self, import: FuncImport<'_>) -> Result<Self::Func, Error> {
        let function =
            HostFunction::lookup(import.module, import.name).ok_or_else(|| Error::Import {
                module: import.module.into(),
                name: import.name.into(),
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
            Err(Error::Signature(format!(
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
            (Write, [Value::I32(_), _, Value::I32(len)]) => Value::I32(*len),
            (Flush, [_]) => Value::I32(0),
            (FillEntropy, [Value::I32(offset), Value::I32(len)]) => {
                caller
                    .write_buffer(0, *offset as u32, *len as u32 as usize)?
                    .fill(0);
                Value::I32(0)
            }
            (ArgCount, []) => Value::I32(0),
            (ArgLen, [_]) | (ArgRead, [..]) => Value::I32(-1),
            (WallTime, []) | (MonotonicTime, []) => Value::I64(0),
            (Sleep, [_]) => Value::I32(0),
            _ => unreachable!("linked host function called with unexpected arguments"),
        };
        Ok(HostCall::Ready(vec![value]))
    }
}

/// Poll a future that never suspends on this host: every import answers
/// `HostCall::Ready`.
fn drive<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: run_module <module.wasm>")?;
    let bytes = std::fs::read(&path)?;
    let module = Module::parse(&bytes)?;

    let started = Instant::now();
    let mut instance = drive(Instance::instantiate(&module, StubHost, Limits::default()))?;
    let status = drive(instance.invoke("run", &[]))?;
    let seconds = started.elapsed().as_secs_f64();

    #[cfg(feature = "step-counter")]
    {
        let retired = instance.executed_instructions();
        println!(
            "status={status:?} retired={retired} elapsed_s={seconds:.3} ns_per_instruction={:.3}",
            seconds * 1e9 / retired.max(1) as f64
        );
    }
    #[cfg(not(feature = "step-counter"))]
    println!(
        "status={status:?} elapsed_s={seconds:.3} (rebuild with --features step-counter for the tally)"
    );
    Ok(())
}
