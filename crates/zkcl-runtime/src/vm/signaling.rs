use super::control_stack::{HandlerSearch, MatchingHandler, SignalPhase};
use super::{
    Continuation, ContinuationStep, ExecState, HandlerTarget, Invocation, Pending, Primitive,
    Resume, Signal, StepControl, UnhandledSignal, Values, accept, capture_error_stack, designator,
    dispatch_error, finish, invoke, symbol_value, unwind_execution_stop,
};
use crate::condition::{self, Class, Fault};
use crate::heap::{RootFrame, RootSlot, Word};
use crate::symbol::Name;
use crate::{EvalError, EvalErrorKind, Runtime};

/// Keep the symbols rooted, and read their dynamic values at each signal.
pub(crate) struct Debugger {
    hook: RootSlot,
    break_on_signals: RootSlot,
    before_handlers: RootSlot,
    enter: RootSlot,
    frontend: RootFrame,
    pub(super) controls: super::debugging::Controls,
}

impl Debugger {
    pub(crate) fn observing_execution(&self, runtime: &Runtime, host_debugging: bool) -> bool {
        (host_debugging || self.has_frontend() || self.controls.has_logpoints())
            && self.controls.observing(runtime)
    }

    pub(super) fn has_frontend(&self) -> bool {
        self.frontend.get(0) != Word::NIL
    }

    fn value(&self, runtime: &Runtime, symbol: &RootSlot) -> Word {
        runtime
            .heap
            .symbol_cell(symbol.get(), false)
            .filter(|&value| value != Word::UNBOUND)
            .unwrap_or(Word::NIL)
    }

    pub(super) fn observes_faults(&self, runtime: &Runtime) -> bool {
        self.frontend.get(0) != Word::NIL
            || self.value(runtime, &self.hook) != Word::NIL
            || self.value(runtime, &self.break_on_signals) != Word::NIL
    }

    pub(crate) fn set_frontend(&self, function: Word) {
        self.frontend.set(0, function);
    }
}

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    super::debugging::initialize(runtime)?;
    let symbol = |name| {
        runtime
            .intern(&Name::common_lisp(name), 0)
            .map(|word| runtime.root_slot(word))
    };
    let function = |source| {
        super::instances::compile_builtin_lambda(runtime, source, &[])
            .map(|word| runtime.root_slot(word))
    };
    let debugger = Debugger {
        hook: symbol("*DEBUGGER-HOOK*")?,
        break_on_signals: symbol("*BREAK-ON-SIGNALS*")?,
        before_handlers: function(
            r#"(lambda (condition typespec)
          (let ((*break-on-signals* nil))
            (when (typep condition typespec)
              (restart-case
                  (with-condition-restarts condition (list (find-restart 'continue))
                    (invoke-debugger condition))
                (continue () :report "Continue signaling." nil)))))"#,
        )?,
        enter: function(
            "(lambda (condition hook debugger)
          (when hook
            (let ((*debugger-hook* nil)) (funcall hook condition hook)))
          (when debugger (funcall debugger condition)))",
        )?,
        frontend: runtime.root_frame(1),
        controls: super::debugging::Controls::default(),
    };
    runtime
        .debugger
        .set(debugger)
        .ok()
        .expect("debugger initialized once");
    Ok(())
}

pub(super) fn dispatch_signal<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut search: HandlerSearch,
) -> Result<StepControl<'rt>, EvalError> {
    let condition = search.signal.condition.get(0);
    match std::mem::replace(&mut search.phase, SignalPhase::Searching) {
        SignalPhase::BeforeHandlers => {
            if let Some(debugger) = runtime.debugger.get() {
                let specifier = debugger.value(runtime, &debugger.break_on_signals);
                if specifier != Word::NIL {
                    return call_while_signaling(
                        runtime,
                        exec,
                        search,
                        None,
                        debugger.before_handlers.get(),
                        &[condition, specifier],
                    );
                }
            }
        }
        SignalPhase::Tested if exec.values.get(0) != Word::NIL => {
            let handler = exec.control.tested_handler(&exec.frames, &search);
            return run_handler(runtime, exec, search, handler);
        }
        SignalPhase::HostDebuggerEntry { check_stop } => {
            if !check_stop || exec.values.get(0) != Word::NIL {
                return expose_host_pause(runtime, exec, search);
            }
            exec.values = *search
                .paused_values
                .take()
                .expect("a breakpoint can resume");
            return Ok(StepControl::Pending(None));
        }
        SignalPhase::DebuggerReturned => {
            if let Some(saved) = search.paused_values.take() {
                exec.values = *saved;
                if search.debug_pause.is_some_and(|pause| pause.abort) {
                    let error = EvalError::execution_stopped(
                        search.signal.at,
                        crate::ExecutionStop::Cancelled,
                    )
                    .after_debugger_abort();
                    return unwind_execution_stop(exec, error, search.signal.vm_pc)
                        .map(StepControl::Unwind);
                }
                return Ok(StepControl::Pending(None));
            }
            return unwind_unhandled_signal(runtime, exec, search, false);
        }
        SignalPhase::Searching | SignalPhase::Tested => {}
    }
    if let Some((handler, specifier)) =
        exec.control
            .next_handler(runtime, &exec.frames, &mut search)
    {
        if let Some(specifier) = specifier {
            let cluster = handler.cluster_scope;
            search.phase = SignalPhase::Tested;
            return call_while_signaling(
                runtime,
                exec,
                search,
                Some(cluster),
                Primitive::named("TYPEP").expect("TYPEP primitive"),
                &[condition, specifier],
            );
        }
        return run_handler(runtime, exec, search, handler);
    }

    let defer_expansion_error = match &search.signal.unhandled {
        Some(UnhandledSignal::LispError) => true,
        Some(UnhandledSignal::Fault(error)) => {
            matches!(error.kind(), EvalErrorKind::LispCondition(_))
        }
        None => {
            exec.values.one(Word::NIL);
            return Ok(StepControl::Pending(finish(
                search.signal.resume,
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            )));
        }
    } && exec.control.can_defer_expansion_error();
    if defer_expansion_error {
        unwind_unhandled_signal(runtime, exec, search, defer_expansion_error)
    } else {
        enter_debugger(runtime, exec, search)
    }
}

pub(super) fn enter_debugger<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    search: HandlerSearch,
) -> Result<StepControl<'rt>, EvalError> {
    enter_debugger_event(runtime, exec, search, None, false)
}

fn enter_debugger_event<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut search: HandlerSearch,
    breakpoint: Option<u64>,
    from_native: bool,
) -> Result<StepControl<'rt>, EvalError> {
    if let Some(debugger) = runtime.debugger.get() {
        let host_debugging = exec.host_debugging && !runtime.has_pinned_rust_frame();
        let hook = debugger.value(runtime, &debugger.hook);
        let nested_callback_fault = exec.control.debug_pause().is_some()
            && matches!(&search.signal.unhandled, Some(UnhandledSignal::Fault(error))
                if error.kind() == EvalErrorKind::HostCallback);
        let frontend = if host_debugging {
            Word::NIL
        } else {
            debugger.frontend.get(0)
        };
        if !nested_callback_fault
            && (host_debugging
                || hook != Word::NIL
                || frontend != Word::NIL
                || breakpoint.is_some_and(|id| id != 0))
        {
            if (host_debugging || frontend != Word::NIL)
                && let Some(UnhandledSignal::Fault(error)) = &mut search.signal.unhandled
                && error.kind() == EvalErrorKind::LispCondition(Class::TypeError)
                && let Ok(datum) = condition::builtin_slot(
                    runtime,
                    search.signal.condition.get(0),
                    Class::TypeError,
                    false,
                    error.offset(),
                )
                && let Ok(text) = runtime.preview(&runtime.root(datum))
            {
                error.append_detail(format_args!("got {text}"));
            }
            let condition = search.signal.condition.get(0);
            search.phase = SignalPhase::DebuggerReturned;
            search.debug_pause = Some(super::debugging::Pause {
                frame_count: exec.frames.len(),
                pc: search.signal.vm_pc,
                resumable: search.paused_values.is_some(),
                abort: false,
                resume_requested: false,
                from_native,
            });
            if let Some(id) = breakpoint
                && let Some((function, arguments)) =
                    super::debugging::breakpoint_entry(runtime, id, condition, hook, frontend)?
            {
                if host_debugging {
                    search.phase = SignalPhase::HostDebuggerEntry { check_stop: true };
                }
                let words = (0..arguments.len())
                    .map(|index| arguments.get(index))
                    .collect::<Vec<_>>();
                return call_while_signaling(runtime, exec, search, None, function.get(), &words);
            }
            if host_debugging {
                if hook == Word::NIL {
                    return expose_host_pause(runtime, exec, search);
                }
                search.phase = SignalPhase::HostDebuggerEntry { check_stop: false };
            }
            return call_while_signaling(
                runtime,
                exec,
                search,
                None,
                debugger.enter.get(),
                &[condition, hook, frontend],
            );
        }
    }
    if let Some(saved) = search.paused_values {
        exec.values = *saved;
        return Ok(StepControl::Pending(None));
    }
    unwind_unhandled_signal(runtime, exec, search, false)
}

fn expose_host_pause<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut search: HandlerSearch,
) -> Result<StepControl<'rt>, EvalError> {
    search.phase = SignalPhase::DebuggerReturned;
    exec.host_debug_event = Some(super::debugging::PausedEvent::new(runtime, exec, &search));
    exec.control
        .pause_signal(exec.frames.len().saturating_sub(1), search, None);
    Ok(StepControl::Pending(None))
}

pub(super) fn pause_at_instruction<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    pc: usize,
    breakpoint: Option<u64>,
) -> Result<StepControl<'rt>, EvalError> {
    let frame = exec
        .frames
        .last()
        .expect("debugger stopped in a bytecode frame")
        .bytecode();
    let at = frame.logical_at(pc);
    pause_instruction(runtime, exec, Some(pc), at, breakpoint, false)
}

pub(super) fn pause_recovered_instruction<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    pc: usize,
    breakpoint: Option<u64>,
) -> Result<StepControl<'rt>, EvalError> {
    let at = exec.frames.last().unwrap().bytecode().logical_at(pc);
    pause_instruction(runtime, exec, Some(pc), at, breakpoint, true)
}

pub(super) fn pause_debug_event<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    pc: Option<usize>,
    at: usize,
    breakpoint: Option<u64>,
) -> Result<StepControl<'rt>, EvalError> {
    pause_instruction(runtime, exec, pc, at, breakpoint, false)
}

fn pause_instruction<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    pc: Option<usize>,
    at: usize,
    breakpoint: Option<u64>,
    from_native: bool,
) -> Result<StepControl<'rt>, EvalError> {
    let message = breakpoint.map_or_else(
        || "Single step".into(),
        |id| {
            if id == 0 {
                "Pause requested".into()
            } else {
                format!("Breakpoint {id}")
            }
        },
    );
    let text = runtime
        .make_string(&message)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let args = runtime.root_frame(1);
    args.set(0, text.slot.get());
    let condition =
        condition::from_format_control(runtime, &args, 0..1, Class::SimpleCondition, at)
            .map_err(|fault| fault.host)?;
    let mut signal = Signal::from_lisp(runtime, condition, false, at, Resume::Call(None));
    signal.vm_pc = pc;
    let mut search = HandlerSearch::for_debugger(signal);
    search.paused_values = Some(Box::new(exec.values.take()));
    enter_debugger_event(runtime, exec, search, breakpoint, from_native)
}

fn unwind_unhandled_signal<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    search: HandlerSearch,
    defer_expansion_error: bool,
) -> Result<StepControl<'rt>, EvalError> {
    let signal = search.signal;
    let mut error = match signal
        .unhandled
        .expect("debugger has a condition to propagate")
    {
        UnhandledSignal::LispError => {
            let error = condition::to_eval_error(runtime, signal.condition.get(0), signal.at);
            if error.execution_stop().is_some() {
                return unwind_execution_stop(exec, error, signal.vm_pc).map(StepControl::Unwind);
            }
            error
        }
        UnhandledSignal::Fault(error) => error,
    };
    if search.debug_pause.is_some_and(|pause| pause.abort) {
        error = error.after_debugger_abort();
    }
    capture_error_stack(&exec.frames, &mut error, signal.vm_pc);
    let cond_slot = runtime.root_slot(signal.condition.get(0));
    let target = exec.control.error(
        &mut exec.frames,
        &mut exec.values,
        error,
        Some(cond_slot),
        defer_expansion_error,
    )?;
    Ok(StepControl::Unwind(target))
}

fn run_handler<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    search: HandlerSearch,
    handler: MatchingHandler,
) -> Result<StepControl<'rt>, EvalError> {
    let condition = search.signal.condition.get(0);
    if let HandlerTarget::Function(reg) = handler.target {
        let function = exec.frames[handler.frame].slots().get(reg);
        call_while_signaling(
            runtime,
            exec,
            search,
            Some(handler.cluster_scope),
            function,
            &[condition],
        )
    } else {
        let target =
            exec.control
                .handler_case(&mut exec.frames, &mut exec.values, handler, condition)?;
        Ok(StepControl::Unwind(target))
    }
}

fn call_while_signaling<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    search: HandlerSearch,
    masked_from: Option<usize>,
    function: Word,
    arguments: &[Word],
) -> Result<StepControl<'rt>, EvalError> {
    let args = runtime.staging_stack.alloc(arguments.len());
    for (i, &word) in arguments.iter().enumerate() {
        args.set(i, word);
    }
    let at = search.signal.at;
    exec.control
        .pause_signal(exec.frames.len().saturating_sub(1), search, masked_from);
    match designator(runtime, function, at).and_then(|target| {
        invoke(
            runtime,
            target,
            &args,
            0..arguments.len(),
            &mut exec.values,
            Resume::SignalHandler,
            at,
        )
    }) {
        Ok(invocation) => accept(runtime, exec, invocation),
        Err(error) => dispatch_error(runtime, exec, error),
    }
}

pub(super) fn start_restartable_signal<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    continuation: Box<dyn Continuation + 'rt>,
    mut signal: Signal,
    restart_name: &'static str,
    report: Option<Word>,
    enclosing: Option<usize>,
) -> Result<StepControl<'rt>, EvalError> {
    let report = report.map(|word| runtime.root_slot(word));
    let at = signal.at;
    let restart = runtime
        .intern(&Name::common_lisp(restart_name), at)
        .and_then(|name| {
            let name = runtime.root_slot(name);
            runtime
                .heap
                .restart(|| name.get())
                .map_err(|error| EvalError::out_of_memory(at, error))
        });
    let restart = match restart {
        Ok(restart) => restart,
        Err(error) => return dispatch_error(runtime, exec, error.into()),
    };
    if let Some(report) = report {
        runtime.heap.set_restart_report(restart, report.get());
    }
    signal.associated_restart_scope = Some(exec.control.enter_resumable_restart(
        runtime,
        exec.frames.len(),
        restart,
        enclosing,
        continuation,
    ));
    Ok(StepControl::pending(Pending::Signal(
        exec.control.search_handlers(signal),
    )))
}

enum WarningPhase {
    Signal,
    Report,
    Done,
}

pub(in crate::vm) struct WarnContinuation {
    condition: RootSlot,
    phase: WarningPhase,
    pub(super) enclosing: Option<usize>,
    at: usize,
    resume: Resume,
}

impl Continuation for WarnContinuation {
    fn step(
        &mut self,
        runtime: &Runtime,
        _values: &Values,
        returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        match self.phase {
            WarningPhase::Signal => {
                self.phase = WarningPhase::Report;
                crate::fasl::note_signalled_warning(runtime, self.condition.get());
                Ok(ContinuationStep::Warning {
                    condition: self.condition.get(),
                    enclosing: self.enclosing,
                    at: self.at,
                })
            }
            WarningPhase::Report if returned => {
                self.phase = WarningPhase::Done;
                let args = runtime.root_frame(3);
                let stream = runtime.intern(&Name::common_lisp("*ERROR-OUTPUT*"), self.at)?;
                args.set(0, symbol_value(runtime, stream, self.at)?);
                args.set(
                    1,
                    runtime
                        .heap
                        .string("~&; Warning: ~A~%")
                        .map_err(|e| EvalError::out_of_memory(self.at, e))?,
                );
                args.set(2, self.condition.get());
                Ok(ContinuationStep::Call {
                    target: Primitive::named("FORMAT").expect("FORMAT primitive"),
                    args,
                    at: self.at,
                })
            }
            WarningPhase::Report | WarningPhase::Done => Ok(ContinuationStep::Done(Word::NIL)),
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

pub(in crate::vm) fn warn_invocation<'rt>(
    runtime: &Runtime,
    condition: Word,
    resume: Resume,
    at: usize,
    enclosing: Option<usize>,
) -> Result<Invocation<'rt>, Fault> {
    if !condition::matches_builtin(runtime, condition, Class::Warning) {
        return Err(Fault::type_error(
            runtime,
            condition,
            "WARNING",
            EvalError::new(at, "WARN expects a warning condition or type"),
        ));
    }
    Ok(Invocation::Warn(Box::new(WarnContinuation {
        condition: runtime.root_slot(condition),
        phase: WarningPhase::Signal,
        enclosing,
        at,
        resume,
    })))
}

impl Debugger {
    pub(crate) fn write_image(
        &self,
        image: &mut crate::image::ImageWriter<'_>,
    ) -> Result<(), EvalError> {
        for word in [
            self.hook.get(),
            self.break_on_signals.get(),
            self.before_handlers.get(),
            self.enter.get(),
            self.frontend.get(0),
        ] {
            image.word(word)?;
        }
        self.controls.write_image(image)
    }
    pub(crate) fn read_image(image: &mut crate::image::ImageReader<'_>) -> Result<Self, EvalError> {
        let hook = image.runtime.root_slot(image.word()?);
        let break_on_signals = image.runtime.root_slot(image.word()?);
        let before_handlers = image.runtime.root_slot(image.word()?);
        let enter = image.runtime.root_slot(image.word()?);
        let frontend = image.runtime.root_frame(1);
        frontend.set(0, image.word()?);
        Ok(Self {
            hook,
            break_on_signals,
            before_handlers,
            enter,
            frontend,
            controls: super::debugging::Controls::read_image(image)?,
        })
    }
}
