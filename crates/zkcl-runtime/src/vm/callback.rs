//! Synchronous calls into Lisp from a Rust operation in the current VM.
//! A nonlocal exit unwinds the Rust operation before the VM resumes its target.

use std::cell::Cell;
use std::mem::DropGuard;
use std::ptr::NonNull;
use std::sync::Arc;

use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::macros::{CallExpander, ExpansionKind};
use crate::native::{NativeContext, NativeReturn, native_invoke_at_scope};
use crate::{EvalError, Root, Runtime};

use super::control_stack::{DeferredExpansionError, ExpansionErrorHandling};
use super::{Continuation, ContinuationStep, ExecFrame, ExecState, Resume, UnwindTarget, Values};

pub(crate) enum Outcome<T> {
    Completed(T),
    Unwind(UnwindTarget),
    Unhandled(EvalError),
}

enum Exit {
    Unwind(UnwindTarget),
    Unhandled(EvalError),
}

enum LispCallReturn<T> {
    Value(T),
    DeferredExpansionError(DeferredExpansionError),
}

pub(crate) type CallValues<'a> =
    dyn Fn(&Root<'_>, &[&Root<'_>], usize) -> Result<Vec<RootSlot>, EvalError> + 'a;

pub(super) struct HostCall {
    callback: Arc<crate::NativeFunction>,
    arguments: RootFrame,
    resume: Resume,
    at: usize,
}

impl HostCall {
    pub(super) fn new(
        runtime: &Runtime,
        callback: Arc<crate::NativeFunction>,
        slots: &RootSlots,
        range: std::ops::Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Self {
        let arguments = runtime.root_frame(range.len());
        for (index, slot) in range.enumerate() {
            arguments.set(index, slots.get(slot));
        }
        Self {
            callback,
            arguments,
            resume,
            at,
        }
    }
}

impl Continuation for HostCall {
    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        _returned: bool,
    ) -> Outcome<Result<ContinuationStep, Fault>> {
        let args: smallvec::SmallVec<[_; 4]> = (0..self.arguments.len())
            .map(|index| runtime.root(self.arguments.get(index)))
            .collect();
        let result = if exec.control.debug_pause().is_some() {
            with_debug_calls(runtime, exec, || (self.callback)(runtime, &args, self.at))
        } else {
            let _pin = runtime.pin_rust_frame();
            Outcome::Completed((self.callback)(runtime, &args, self.at))
        };
        match result {
            Outcome::Completed(Ok(values)) => {
                exec.values.select_roots(&values);
                Outcome::Completed(Ok(ContinuationStep::KeepValues))
            }
            Outcome::Completed(Err(fault)) => Outcome::Completed(Err(fault)),
            Outcome::Unwind(target) => Outcome::Unwind(target),
            Outcome::Unhandled(error) => Outcome::Unhandled(error),
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

struct HostAccess<'rt> {
    access: CallbackAccess<'rt>,
    exit: CallbackExit,
}

pub(super) fn with_debug_calls<'rt, T>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    f: impl FnOnce() -> T,
) -> Outcome<T> {
    let calls = HostAccess {
        access: CallbackAccess::new(exec, true),
        exit: CallbackExit::default(),
    };
    let previous = runtime
        .debug_callback
        .replace(Some(NonNull::from(&calls).cast()));
    let restore = DropGuard::new(previous, |previous| runtime.debug_callback.set(previous));
    let _pin = runtime.pin_rust_frame();
    let result = f();
    drop(restore);
    calls.exit.finish(result)
}

pub(super) fn call_from_host(
    runtime: &Runtime,
    caller: NonNull<()>,
    function: &RootSlot,
    args: &RootSlots,
) -> Result<Values, EvalError> {
    // SAFETY: `with_debug_calls` installs this stack address only for its
    // synchronous callback on the runtime's owning thread. Disarm the entry
    // during dispatch so nested Rust code cannot borrow the same VM state.
    let calls = unsafe { caller.cast::<HostAccess<'_>>().as_ref() };
    runtime.debug_callback.set(None);
    let _restore = DropGuard::new(caller, |caller| runtime.debug_callback.set(Some(caller)));
    calls.exit.propagate(0, || {
        let result = invoke_lisp_slots(
            runtime,
            &calls.access,
            function,
            args,
            0,
            None,
            |single, values| {
                let mut result = Values::new(runtime);
                if let Some(word) = single {
                    result.one(word);
                } else {
                    result.select(&values.slots, 0..values.len);
                }
                result
            },
        )?;
        match result {
            LispCallReturn::Value(values) => Ok(Ok(values)),
            LispCallReturn::DeferredExpansionError(_) => unreachable!("ordinary Lisp call"),
        }
    })
}

/// Encapsulates temporary reentrant access to `ExecState` while synchronous callbacks run.
/// Access methods borrow `ExecState` only for the duration of a single non-reentrant read or binding.
pub(super) struct CallbackAccess<'rt> {
    exec: NonNull<ExecState<'rt>>,
    active_borrow: Cell<bool>,
    stop_at_scope: bool,
}

impl<'rt> CallbackAccess<'rt> {
    fn new(exec: &mut ExecState<'rt>, stop_at_scope: bool) -> Self {
        Self {
            exec: NonNull::from(exec),
            active_borrow: Cell::new(false),
            stop_at_scope,
        }
    }

    fn with_exec<R>(&self, f: impl FnOnce(&mut ExecState<'rt>) -> R) -> R {
        assert!(
            !self.active_borrow.replace(true),
            "reentrant ExecState access"
        );
        let guard = std::mem::DropGuard::new((), |()| self.active_borrow.set(false));
        let mut exec = self.exec;
        // SAFETY: CallbackAccess is only invoked synchronously from the callback body when no
        // reentrant Lisp invocation (`invoke_lisp`) currently holds an `&mut ExecState` borrow.
        let result = f(unsafe { exec.as_mut() });
        drop(guard);
        result
    }

    pub(super) fn values_first(&self) -> Word {
        self.with_exec(|exec| exec.values.get(0))
    }

    pub(super) fn set_one_value(&self, word: Word) {
        self.with_exec(|exec| exec.values.one(word));
    }

    pub(super) fn with_values<R>(&self, f: impl FnOnce(&Values) -> R) -> R {
        self.with_exec(|exec| f(&exec.values))
    }

    pub(super) fn bind(&self, runtime: &Runtime, bindings: Vec<(Word, Word)>) {
        self.with_exec(|exec| {
            exec.control.bind(
                exec.frames.len().saturating_sub(1),
                &runtime.heap,
                bindings.into_iter(),
            );
        });
    }

    pub(super) fn unbind(&self, depth: usize) {
        self.with_exec(|exec| exec.control.unbind(depth));
    }
}

pub(crate) fn run<'rt, T>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    operation: impl FnOnce(Option<&CallExpander<'_>>, Option<&CallValues<'_>>) -> T,
) -> Outcome<T> {
    run_with_access(runtime, exec, false, |_, expander, values| {
        operation(expander, values)
    })
}

pub(super) fn run_with_access<'rt, T>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    defer_unhandled_expansion_errors: bool,
    operation: impl FnOnce(
        &CallbackAccess<'rt>,
        Option<&CallExpander<'_>>,
        Option<&CallValues<'_>>,
    ) -> T,
) -> Outcome<T> {
    let empty_frames = exec.frames.is_empty();
    let access = CallbackAccess::new(exec, false);
    // Only Lisp EVAL can defer an expansion error until the form runs, even
    // when its caller has no Lisp frames or dynamic handlers.
    if !defer_unhandled_expansion_errors && empty_frames {
        return Outcome::Completed(operation(&access, None, None));
    }
    let exit = CallbackExit::default();
    let call_expander = |function: &Root<'_>, args: &[&Root<'_>], at, kind| {
        exit.propagate(at, || {
            let error_handling = match kind {
                ExpansionKind::Other => None,
                _ if !defer_unhandled_expansion_errors => Some(ExpansionErrorHandling::Propagate),
                ExpansionKind::MacroForm => {
                    Some(ExpansionErrorHandling::DeferUnhandledToEvaluation)
                }
                ExpansionKind::MacroFormInHandler => {
                    Some(ExpansionErrorHandling::DeferToFormHandlers)
                }
            };
            let returned = invoke_lisp(
                runtime,
                &access,
                function,
                args,
                at,
                error_handling,
                |single, values| {
                    single
                        .or_else(|| (values.len > 0).then(|| values.get(0)))
                        .unwrap_or(Word::NIL)
                },
            )?;
            Ok(match returned {
                LispCallReturn::Value(value) => Ok(value),
                LispCallReturn::DeferredExpansionError(failure) => {
                    crate::compile::residual_macro_error(
                        runtime,
                        &failure.condition,
                        &failure.error,
                        at,
                    )
                    .map(|value| value.get())
                }
            })
        })
    };
    let call_for_values = |function: &Root<'_>, args: &[&Root<'_>], at| {
        exit.propagate(at, || {
            let returned = invoke_lisp(
                runtime,
                &access,
                function,
                args,
                at,
                None,
                |single, values| {
                    if let Some(value) = single {
                        vec![runtime.root_slot(value)]
                    } else {
                        (0..values.len)
                            .map(|index| runtime.root_slot(values.get(index)))
                            .collect()
                    }
                },
            )?;
            match returned {
                LispCallReturn::Value(values) => Ok(Ok(values)),
                LispCallReturn::DeferredExpansionError(_) => {
                    unreachable!("only macro expander calls can return deferred errors")
                }
            }
        })
    };
    let _pin = runtime.pin_rust_frame();
    let result = operation(&access, Some(&call_expander), Some(&call_for_values));
    exit.finish(result)
}

#[derive(Default)]
struct CallbackExit(Cell<Option<Exit>>);

impl CallbackExit {
    fn propagate<T>(
        &self,
        at: usize,
        invoke: impl FnOnce() -> Result<Result<T, EvalError>, Exit>,
    ) -> Result<T, EvalError> {
        if let Some(transfer) = self.0.take() {
            self.0.set(Some(transfer));
            return Err(EvalError::callback_transfer(at));
        }
        match invoke() {
            Ok(result) => result,
            Err(transfer) => {
                self.0.set(Some(transfer));
                // Stop the current Rust operation. The VM still owns the
                // return values and the destination of the nonlocal exit.
                Err(EvalError::callback_transfer(at))
            }
        }
    }

    fn finish<T>(self, result: T) -> Outcome<T> {
        match self.0.into_inner() {
            Some(Exit::Unwind(target)) => Outcome::Unwind(target),
            Some(Exit::Unhandled(error)) => Outcome::Unhandled(error),
            None => Outcome::Completed(result),
        }
    }
}

fn invoke_lisp<'rt, T>(
    runtime: &'rt Runtime,
    access: &CallbackAccess<'rt>,
    function: &Root<'_>,
    args: &[&Root<'_>],
    at: usize,
    error_handling: Option<ExpansionErrorHandling>,
    select: impl FnOnce(Option<Word>, &Values) -> T,
) -> Result<LispCallReturn<T>, Exit> {
    let arguments = runtime.staging_stack.alloc(args.len());
    for (index, argument) in args.iter().enumerate() {
        arguments.set(index, argument.slot.get());
    }
    invoke_lisp_slots(
        runtime,
        access,
        &function.slot,
        &arguments,
        at,
        error_handling,
        select,
    )
}

fn invoke_lisp_slots<'rt, T>(
    runtime: &'rt Runtime,
    access: &CallbackAccess<'rt>,
    function: &RootSlot,
    args: &RootSlots,
    at: usize,
    error_handling: Option<ExpansionErrorHandling>,
    select: impl FnOnce(Option<Word>, &Values) -> T,
) -> Result<LispCallReturn<T>, Exit> {
    if let Err(error) = runtime.check_execution_safepoint(at) {
        return Err(access.with_exec(|state| {
            match super::unwind_execution_stop(state, error, None) {
                Ok(target) => Exit::Unwind(target),
                Err(error) => Exit::Unhandled(error),
            }
        }));
    }
    let (caller_values, min_scopes) = access.with_exec(|state| {
        let min_scopes = access.stop_at_scope.then(|| state.control.len());
        if let Some(error_handling) = error_handling {
            state
                .control
                .enter_macro_expansion_call(state.frames.len(), at, error_handling);
        }
        (state.values.take(), min_scopes)
    });
    let mut context = NativeContext::new(runtime, access.exec);
    let result = native_invoke_at_scope(
        &mut context,
        function.get(),
        args,
        0..args.len(),
        at,
        min_scopes,
    )
    .map_err(|fault| Exit::Unhandled(fault.host))?;
    access.with_exec(|state| {
        if error_handling.is_some_and(ExpansionErrorHandling::defers_to_evaluation)
            && let Some(failure) = state.control.take_deferred_expansion_error()
        {
            state.values = caller_values;
            return Ok(LispCallReturn::DeferredExpansionError(failure));
        }
        let value = match result {
            NativeReturn::One(value) => select(Some(value), &state.values),
            NativeReturn::Values => select(None, &state.values),
            NativeReturn::Unwind {
                target_frame,
                state: pc,
            } => {
                let target = match state.frames.get(target_frame) {
                    Some(ExecFrame::Native(_)) => UnwindTarget::Native {
                        frame: target_frame,
                        state: pc,
                    },
                    _ => UnwindTarget::Bytecode(target_frame),
                };
                return Err(Exit::Unwind(target));
            }
        };
        if error_handling.is_some() {
            state.control.leave_macro_expansion_call();
        }
        state.values = caller_values;
        Ok(LispCallReturn::Value(value))
    })
}
