use std::ops::Range;

use super::control_stack::ControlStack;
use super::{
    Continuation, ContinuationStep, ExecFrame, ExecState, Invocation, Primitive, Register,
    RestartTarget, Resume, StepControl, Values, accept, designator, dispatch_error, finish, invoke,
};
use crate::condition::{self, Class, Fault};
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::symbol::Name;
use crate::{EvalError, Runtime};

/// The restart and its continuation have the same dynamic extent as BODY.
/// Normal return resumes with BODY's values. Invoking the restart unwinds
/// through any cleanups and resumes the continuation without a return.
pub(super) fn call_with_simple_restart<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    restart: RootSlot,
    body: RootSlot,
    resume: Resume,
    at: usize,
) -> Result<StepControl<'rt>, EvalError> {
    let invocation = invoke(
        runtime,
        body.get(),
        &runtime.root_frame(0),
        0..0,
        &mut exec.values,
        Resume::Continuation,
        at,
    );
    let invocation = match invocation {
        Ok(invocation) => invocation,
        Err(error) => return dispatch_error(runtime, exec, error),
    };
    exec.control.enter_resumable_restart(
        runtime,
        exec.frames.len(),
        restart.get(),
        None,
        Box::new(SimpleRestartReturn(resume)),
    );
    accept(runtime, exec, invocation)
}

struct SimpleRestartReturn(Resume);

impl Continuation for SimpleRestartReturn {
    fn step(
        &mut self,
        _runtime: &Runtime,
        values: &Values,
        returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        let results = if returned {
            (0..values.len).map(|i| values.get(i)).collect()
        } else {
            vec![Word::NIL, Word::T]
        };
        Ok(ContinuationStep::DoneValues(results))
    }

    fn resume_to(&self) -> Resume {
        self.0
    }
}

pub(super) struct CorrectableError {
    phase: CorrectableErrorPhase,
    report: RootSlot,
    pub(super) enclosing: Option<usize>,
    resume: Resume,
    at: usize,
}

enum CorrectableErrorPhase {
    ReadyToSignal(RootSlot),
    MakeCondition(RootFrame),
    AwaitingCondition,
    Signaled,
}

impl CorrectableError {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let datum_args = args.start + 1..args.end;
        let datum = frame.get(datum_args.start);
        let phase = if condition::is_condition(runtime, datum) {
            CorrectableErrorPhase::ReadyToSignal(runtime.root_slot(datum))
        } else if runtime.heap.is_symbol(datum) || runtime.heap.class_info(datum).is_some() {
            let arguments = runtime.root_frame(datum_args.len());
            for (index, argument) in datum_args.clone().enumerate() {
                arguments.set(index, frame.get(argument));
            }
            CorrectableErrorPhase::MakeCondition(arguments)
        } else {
            let condition = condition::from_format_control(
                runtime,
                frame,
                datum_args.clone(),
                Class::SimpleError,
                at,
            )?;
            CorrectableErrorPhase::ReadyToSignal(runtime.root_slot(condition))
        };
        let report_args = super::methods::list_from_words(
            runtime,
            (datum_args.start + 1..args.end)
                .map(|index| frame.get(index))
                .collect(),
            at,
        )?;
        let report_args = runtime.root_slot(report_args);
        let report = runtime
            .heap
            .cons(|| (frame.get(args.start), report_args.get()))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        Ok(Self {
            phase,
            report: runtime.root_slot(report),
            enclosing: None,
            resume,
            at,
        })
    }
}

impl Continuation for CorrectableError {
    fn step(
        &mut self,
        _runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        let condition = match std::mem::replace(&mut self.phase, CorrectableErrorPhase::Signaled) {
            CorrectableErrorPhase::ReadyToSignal(condition) => condition.get(),
            CorrectableErrorPhase::MakeCondition(args) => {
                self.phase = CorrectableErrorPhase::AwaitingCondition;
                return Ok(ContinuationStep::Call {
                    target: Primitive::named("MAKE-CONDITION").expect("MAKE-CONDITION primitive"),
                    args,
                    at: self.at,
                });
            }
            CorrectableErrorPhase::AwaitingCondition => values.get(0),
            CorrectableErrorPhase::Signaled => return Ok(ContinuationStep::Done(Word::NIL)),
        };
        Ok(ContinuationStep::CorrectableError {
            condition,
            report: self.report.get(),
            enclosing: self.enclosing,
            at: self.at,
        })
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

#[derive(Clone, Copy)]
pub(super) enum RestartOperation {
    Find,
    Compute,
    Invoke,
    InvokeInteractively,
    Standard(StandardRestart),
}

#[derive(Clone, Copy)]
pub(crate) enum StandardRestart {
    Abort,
    Continue,
    MuffleWarning,
    StoreValue,
    UseValue,
}

impl StandardRestart {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Abort => "ABORT",
            Self::Continue => "CONTINUE",
            Self::MuffleWarning => "MUFFLE-WARNING",
            Self::StoreValue => "STORE-VALUE",
            Self::UseValue => "USE-VALUE",
        }
    }

    fn argument_count(self) -> usize {
        usize::from(matches!(self, Self::StoreValue | Self::UseValue))
    }

    pub(super) fn accepts(self, count: usize) -> bool {
        let required = self.argument_count();
        (required..=required + 1).contains(&count)
    }

    fn missing_returns_nil(self) -> bool {
        matches!(self, Self::Continue | Self::StoreValue | Self::UseValue)
    }
}

pub(super) struct RestartRequest {
    op: RestartOperation,
    args: RootFrame,
    at: usize,
    resume: Resume,
}

impl RestartRequest {
    pub(super) fn new(
        runtime: &Runtime,
        op: RestartOperation,
        caller_slots: &RootSlots,
        args: Range<Register>,
        at: usize,
        resume: Resume,
    ) -> Self {
        let roots = runtime.root_frame(args.len());
        for (dst, src) in args.enumerate() {
            roots.set(dst, caller_slots.get(src));
        }
        Self {
            op,
            args: roots,
            at,
            resume,
        }
    }

    fn condition_argument(&self) -> Word {
        let index = match self.op {
            RestartOperation::Find => 1,
            RestartOperation::Compute => 0,
            RestartOperation::Invoke | RestartOperation::InvokeInteractively => return Word::NIL,
            RestartOperation::Standard(restart) => restart.argument_count(),
        };
        if index < self.args.len() {
            self.args.get(index)
        } else {
            Word::NIL
        }
    }

    fn invocation_arguments(&self) -> Range<Register> {
        match self.op {
            RestartOperation::Standard(restart) => 0..restart.argument_count(),
            _ => 1..self.args.len(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ActiveRestart {
    pub(super) scope_index: usize,
    pub(super) binding: usize,
}

pub(super) struct RestartCursor {
    pub(super) end: usize,
    pub(super) binding: usize,
}

pub(super) struct InteractiveRestartInvocation {
    candidate: ActiveRestart,
    at: usize,
    resume: Resume,
}

impl ActiveRestart {
    pub(super) fn restart_object(
        self,
        control: &ControlStack<'_>,
        frames: &[ExecFrame<'_>],
    ) -> Word {
        match control.restart_binding(frames, self) {
            Some((frame, spec)) => frames[frame].slots().get(spec.object),
            None => control.resumable_restart_object(self),
        }
    }
}

enum RestartIdentifier {
    All,
    Name,
    Object,
    Standard(RootSlot),
}

pub(super) struct RestartSelection {
    request: RestartRequest,
    cursor: RestartCursor,
    identifier: RestartIdentifier,
    first: Option<ActiveRestart>,
    further: Vec<ActiveRestart>,
    testing: Option<ActiveRestart>,
}

impl RestartSelection {
    pub(super) fn new(
        runtime: &Runtime,
        control: &ControlStack<'_>,
        request: RestartRequest,
    ) -> Result<Self, Fault> {
        let condition = request.condition_argument();
        if condition != Word::NIL && !condition::is_condition(runtime, condition) {
            return Err(Fault::optional_condition(
                runtime,
                condition,
                EvalError::new(request.at, "expected a condition or NIL"),
            ));
        }
        let identifier = match request.op {
            RestartOperation::Compute => RestartIdentifier::All,
            RestartOperation::Standard(restart) => RestartIdentifier::Standard(
                runtime.root_slot(runtime.intern(&Name::common_lisp(restart.name()), request.at)?),
            ),
            _ => {
                let id = request.args.get(0);
                if runtime.heap.restart_name(id).is_some() {
                    RestartIdentifier::Object
                } else if id != Word::NIL && runtime.heap.is_symbol(id) {
                    RestartIdentifier::Name
                } else {
                    let error = EvalError::new(request.at, "expected a restart or non-NIL symbol");
                    return Err(if matches!(request.op, RestartOperation::Find) {
                        Fault::restart_designator(runtime, id, error)
                    } else {
                        Fault::plain(Class::ControlError, error)
                    });
                }
            }
        };
        Ok(Self {
            cursor: RestartCursor {
                end: control.len(),
                binding: 0,
            },
            identifier,
            first: None,
            further: Vec::new(),
            testing: None,
            request,
        })
    }

    fn select(&mut self, candidate: ActiveRestart) {
        if self.first.is_none() {
            self.first = Some(candidate);
        } else {
            self.further.push(candidate);
        }
    }

    pub(super) fn record_test_result(&mut self, passed: bool) {
        let candidate = self.testing.take().unwrap();
        if passed {
            self.select(candidate);
        }
    }

    pub(super) fn is_testing(&self, candidate: ActiveRestart) -> bool {
        self.testing == Some(candidate)
    }
}

pub(super) fn dispatch_restart_operation<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut selection: RestartSelection,
) -> Result<StepControl<'rt>, EvalError> {
    let at = selection.request.at;
    while matches!(selection.request.op, RestartOperation::Compute) || selection.first.is_none() {
        let Some(candidate) = exec
            .control
            .next_restart(&exec.frames, &mut selection.cursor)
        else {
            break;
        };
        let object = candidate.restart_object(&exec.control, &exec.frames);
        let matches = match &selection.identifier {
            RestartIdentifier::All => true,
            RestartIdentifier::Object => object == selection.request.args.get(0),
            RestartIdentifier::Name => {
                runtime.heap.restart_name(object) == Some(selection.request.args.get(0))
            }
            RestartIdentifier::Standard(name) => {
                runtime.heap.restart_name(object) == Some(name.get())
            }
        };
        if !matches
            || !exec.control.restart_applies(
                candidate,
                object,
                selection.request.condition_argument(),
            )
        {
            continue;
        }
        let invoking_object = matches!(selection.identifier, RestartIdentifier::Object)
            && matches!(
                selection.request.op,
                RestartOperation::Invoke | RestartOperation::InvokeInteractively
            );
        if !invoking_object
            && let Some((frame, test)) = exec
                .control
                .restart_binding(&exec.frames, candidate)
                .and_then(|(frame, spec)| spec.test.map(|test| (frame, test)))
        {
            // A nested search must be able to reach other restarts without running
            // this test again. Unwinding or returning from the selection scope clears the mask.
            if exec.control.restart_test_is_active(candidate) {
                continue;
            }
            let function = exec.frames[frame].slots().get(test);
            let argument = runtime.root_frame(1);
            argument.set(0, selection.request.condition_argument());
            selection.testing = Some(candidate);
            exec.control
                .pause_restart_selection(exec.frames.len() - 1, selection);
            let invocation = designator(runtime, function, at).and_then(|target| {
                invoke(
                    runtime,
                    target,
                    &argument,
                    0..1,
                    &mut exec.values,
                    Resume::RestartTest,
                    at,
                )
            });
            return route(runtime, exec, invocation);
        }
        selection.select(candidate);
    }
    match selection.request.op {
        RestartOperation::Find => {
            let found = selection.first.map_or(Word::NIL, |candidate| {
                candidate.restart_object(&exec.control, &exec.frames)
            });
            exec.values.one(found);
            Ok(StepControl::Pending(finish(
                selection.request.resume,
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            )))
        }
        RestartOperation::Compute => {
            exec.values.one(Word::NIL);
            for candidate in selection.first.into_iter().chain(selection.further).rev() {
                let pair = runtime.heap.cons(|| {
                    (
                        candidate.restart_object(&exec.control, &exec.frames),
                        exec.values.get(0),
                    )
                });
                let pair = match pair {
                    Ok(pair) => pair,
                    Err(error) => {
                        return dispatch_error(
                            runtime,
                            exec,
                            EvalError::out_of_memory(at, error).into(),
                        );
                    }
                };
                exec.values.slots.set(0, pair);
            }
            Ok(StepControl::Pending(finish(
                selection.request.resume,
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            )))
        }
        RestartOperation::Invoke
        | RestartOperation::InvokeInteractively
        | RestartOperation::Standard(_) => {
            let Some(candidate) = selection.first else {
                if matches!(selection.request.op, RestartOperation::Standard(restart) if restart.missing_returns_nil())
                {
                    exec.values.one(Word::NIL);
                    return Ok(StepControl::Pending(finish(
                        selection.request.resume,
                        &mut exec.frames,
                        &mut exec.values,
                        &mut exec.control,
                    )));
                }
                return dispatch_error(
                    runtime,
                    exec,
                    Fault::plain(
                        Class::ControlError,
                        EvalError::new(at, "restart is not active or applicable"),
                    ),
                );
            };
            if matches!(selection.request.op, RestartOperation::InvokeInteractively)
                && let Some((frame, reg)) = exec
                    .control
                    .restart_binding(&exec.frames, candidate)
                    .and_then(|(frame, spec)| spec.interactive.map(|reg| (frame, reg)))
            {
                let function = exec.frames[frame].slots().get(reg);
                let interactive_restart = InteractiveRestartInvocation {
                    candidate,
                    at,
                    resume: selection.request.resume,
                };
                exec.control
                    .pause_interactive_restart(exec.frames.len() - 1, interactive_restart);
                let invocation = designator(runtime, function, at).and_then(|target| {
                    invoke(
                        runtime,
                        target,
                        RootSlots::empty(),
                        0..0,
                        &mut exec.values,
                        Resume::RestartInteractive,
                        at,
                    )
                });
                return route(runtime, exec, invocation);
            }
            if matches!(selection.request.op, RestartOperation::InvokeInteractively)
                && exec
                    .control
                    .restart_binding(&exec.frames, candidate)
                    .is_none()
                && exec.control.resumable_restart_arity(candidate) == 1
            {
                let invocation = InteractiveRestartInvocation {
                    candidate,
                    at,
                    resume: selection.request.resume,
                };
                exec.control
                    .pause_interactive_restart(exec.frames.len().saturating_sub(1), invocation);
                return accept(
                    runtime,
                    exec,
                    Invocation::Continuation(Box::new(super::recovery::InteractiveValue::new(at))),
                );
            }
            let request = selection.request;
            let resume = if matches!(
                request.op,
                RestartOperation::Standard(StandardRestart::Abort)
            ) {
                Resume::AbortRestart(request.at)
            } else {
                request.resume
            };
            invoke_selected_restart(
                runtime,
                exec,
                candidate,
                &request.args,
                request.invocation_arguments(),
                resume,
                request.at,
            )
        }
    }
}

pub(super) fn invoke_with_interactive_arguments<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    invocation: InteractiveRestartInvocation,
) -> Result<StepControl<'rt>, EvalError> {
    let argument_list = exec.values.get(0);
    let args = match super::proper_list(runtime, argument_list, invocation.at) {
        Ok(args) => args,
        Err(error) => {
            return dispatch_error(
                runtime,
                exec,
                Fault::type_error(runtime, argument_list, "LIST", error),
            );
        }
    };
    let roots = runtime.staging_stack.alloc(args.len());
    for (i, word) in args.into_iter().enumerate() {
        roots.set(i, word);
    }
    invoke_selected_restart(
        runtime,
        exec,
        invocation.candidate,
        &roots,
        0..roots.len(),
        invocation.resume,
        invocation.at,
    )
}

fn invoke_selected_restart<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    candidate: ActiveRestart,
    arguments: &RootSlots,
    args: Range<Register>,
    resume: Resume,
    at: usize,
) -> Result<StepControl<'rt>, EvalError> {
    let Some((frame, target)) = exec
        .control
        .restart_binding(&exec.frames, candidate)
        .map(|(frame, spec)| (frame, spec.target))
    else {
        if args.len() != exec.control.resumable_restart_arity(candidate) {
            return dispatch_error(
                runtime,
                exec,
                Fault::plain(
                    Class::ProgramError,
                    EvalError::new(at, "wrong number of restart arguments"),
                ),
            );
        }
        exec.values.select(arguments, args);
        let target = exec.control.resume_restart(
            &mut exec.frames,
            &mut exec.values,
            candidate.scope_index,
            candidate.binding,
        )?;
        return Ok(StepControl::Unwind(target));
    };
    match target {
        RestartTarget::Function(reg) => {
            let function = exec.frames[frame].slots().get(reg);
            let invocation = designator(runtime, function, at).and_then(|target| {
                invoke(
                    runtime,
                    target,
                    arguments,
                    args,
                    &mut exec.values,
                    resume,
                    at,
                )
            });
            route(runtime, exec, invocation)
        }
        RestartTarget::Case { pc, clear } => {
            let pc = exec.frames[frame].bytecode().vm_code.vm_target(pc);
            exec.values.select(arguments, args);
            let target = exec.control.restart_case(
                &mut exec.frames,
                &mut exec.values,
                candidate.scope_index,
                pc,
                clear,
            )?;
            Ok(StepControl::Unwind(target))
        }
    }
}

fn route<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    invocation: Result<Invocation<'rt>, Fault>,
) -> Result<StepControl<'rt>, EvalError> {
    match invocation {
        Ok(invocation) => accept(runtime, exec, invocation),
        Err(fault) => dispatch_error(runtime, exec, fault),
    }
}

#[cfg(test)]
mod tests {
    use crate::{Runtime, heap::Heap};

    #[test]
    fn compute_restarts_preserves_identity_during_relocation() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        config.stress_bytes = Some(128 << 10);
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let result = runtime
            .eval(
                "(restart-bind ((recover (lambda () nil)))
                   (let ((restart (find-restart 'recover)))
                     (dotimes (i 16384 t)
                       (unless (eq restart (car (compute-restarts)))
                         (return nil)))))",
            )
            .unwrap();
        assert!(!result[0].is_nil());
    }
}
