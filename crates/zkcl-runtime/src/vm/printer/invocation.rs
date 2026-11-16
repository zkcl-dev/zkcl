use std::cell::Cell;
use std::mem::DropGuard;
use std::ops::{Deref, Range};

use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlots, Word};
use crate::vm::{
    Continuation, ContinuationStep, ControlStack, ExecState, Primitive, Resume, Values, callback,
    format,
};
use crate::{Root, Runtime};

/// A synchronous printer borrows the active VM only for each Lisp call or
/// binding operation. Its Rust traversal owns no references into VM frames.
pub(crate) struct PrintContext<'a> {
    runtime: &'a Runtime,
    active: Option<VmPrintCallbacks<'a>>,
}

struct VmPrintCallbacks<'a> {
    call: &'a callback::CallValues<'a>,
    bind: &'a dyn Fn(Vec<(Word, Word)>),
    unbind: &'a dyn Fn(),
    transferred: Cell<bool>,
}

impl Deref for PrintContext<'_> {
    type Target = Runtime;

    fn deref(&self) -> &Runtime {
        self.runtime
    }
}

impl<'a> PrintContext<'a> {
    pub(crate) fn host(runtime: &'a Runtime) -> Self {
        Self {
            runtime,
            active: None,
        }
    }

    pub(crate) fn invoke(
        &self,
        function: &Root<'_>,
        args: &[&Root<'_>],
        at: usize,
    ) -> Result<Vec<Root<'a>>, Fault> {
        if let Some(active) = &self.active {
            return match (active.call)(function, args, at) {
                Ok(values) => Ok(values
                    .iter()
                    .map(|value| self.runtime.root(value.get()))
                    .collect()),
                Err(error) => {
                    active.transferred.set(true);
                    Err(error.into())
                }
            };
        }
        self.runtime.call(function, args).map_err(Fault::from)
    }

    pub(crate) fn bind_specials(&self, bindings: Vec<(Word, Word)>) -> impl Drop + use<'_> {
        let mut local = ControlStack::default();
        let active = if bindings.is_empty() {
            None
        } else if let Some(active) = &self.active {
            (active.bind)(bindings);
            Some(active)
        } else {
            local.bind(0, &self.runtime.heap, bindings.into_iter());
            None
        };
        DropGuard::new((local, active), |(_local, active)| {
            // Lisp has already unwound the bindings when a callback transfers control.
            if let Some(active) = active
                && !active.transferred.get()
            {
                (active.unbind)();
            }
        })
    }
}

pub(crate) struct PrintContinuation {
    primitive: Primitive,
    args: RootFrame,
    at: usize,
    resume: Resume,
}

impl PrintContinuation {
    pub(in crate::vm) fn new(
        runtime: &Runtime,
        primitive: Primitive,
        frame: &RootSlots,
        args: Range<usize>,
        at: usize,
        resume: Resume,
    ) -> Self {
        let roots = runtime.root_frame(args.len());
        for (index, slot) in args.enumerate() {
            roots.set(index, frame.get(slot));
        }
        Self {
            primitive,
            args: roots,
            at,
            resume,
        }
    }

    fn run(&self, print_context: &PrintContext<'_>) -> Result<ContinuationStep, Fault> {
        let range = 0..self.args.len();
        let value = match self.primitive {
            Primitive::Printer(op) => super::run(print_context, &self.args, op, range, self.at)?,
            Primitive::Format(op) => format::run(print_context, &self.args, op, range, self.at)?,
            _ => unreachable!("printer continuation"),
        };
        Ok(ContinuationStep::Done(value))
    }
}

impl Continuation for PrintContinuation {
    fn step(&mut self, rt: &Runtime, _: &Values, _: bool) -> Result<ContinuationStep, Fault> {
        self.run(&PrintContext::host(rt))
    }

    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        _: bool,
    ) -> callback::Outcome<Result<ContinuationStep, Fault>> {
        callback::run_with_access(runtime, exec, false, |access, _, call| {
            // Each borrow ends before a printer method can reenter the VM.
            let bind = |bindings: Vec<(Word, Word)>| access.bind(runtime, bindings);
            let unbind = || access.unbind(1);
            self.run(&PrintContext {
                runtime,
                active: call.map(|call| VmPrintCallbacks {
                    call,
                    bind: &bind,
                    unbind: &unbind,
                    transferred: Cell::new(false),
                }),
            })
        })
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
