use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};

use zkcl_bytecode::dispatch::DispatchOutcome;

use super::{
    ControlStack, ExecFrame, ExecState, Frame, InstructionAction, Invocation, NativeInvocation,
    Pending, PendingContinuation, StepControl, TransferOutcome, UnwindTarget, Values, accept,
    accept_with_pc, dispatch_continuation, dispatch_error, dispatch_error_with_pc,
    dispatch_macroexpand, dispatch_signal, evaluate, finish, function_definitions, instances,
    invoke, ops, restart, task_scheduler, unwind_execution_stop,
};
use crate::code_store::NativeCode;
use crate::condition::{Class, Fault};
use crate::execution::{HOST_CANCEL, HOST_RUNNING, cancellation_stop};
use crate::native::NativeReturn;
use crate::{EvalError, ExecutionStop, Runtime};

pub(super) const CHECKPOINT_QUANTUM: u16 = 64;
// Use a prime cadence so a loop with a power-of-two padded stride does not
// repeatedly align the sampler with the same backedge.
pub(super) const OSR_BACKEDGE_SAMPLE: u8 = 61;

pub(super) enum TaskStepOutcome<'rt> {
    Completed,
    Failed(EvalError),
    QuantumYield(Option<PendingContinuation<'rt>>),
    Suspend(task_scheduler::Suspension),
    Paused(super::debugging::PausedEvent),
}

#[inline]
pub(super) fn poll<'rt>(
    runtime: &Runtime,
    exec: &mut ExecState<'rt>,
    top_vm_pc: Option<usize>,
) -> Result<Option<StepControl<'rt>>, EvalError> {
    let scheduled = task_scheduler::in_scheduler(runtime) && !runtime.has_pinned_rust_frame();
    poll_step(runtime, exec, top_vm_pc, scheduled, false)
}

#[inline]
fn host_stop(
    runtime: &Runtime,
    control: &ControlStack<'_>,
    request: u8,
    scheduled: bool,
) -> Option<ExecutionStop> {
    if request == HOST_CANCEL {
        let active_root = task_scheduler::observe_host_cancel(runtime);
        if scheduled || active_root == Some(false) {
            return None;
        }
    }
    cancellation_stop(
        request,
        control.defers_host_cancellation() || runtime.defers_host_cancellation(),
    )
}

fn poll_step<'rt>(
    runtime: &Runtime,
    exec: &mut ExecState<'rt>,
    top_vm_pc: Option<usize>,
    scheduled: bool,
    ignores_cancel: bool,
) -> Result<Option<StepControl<'rt>>, EvalError> {
    let request = runtime.host_cancel_request();
    if request != HOST_RUNNING
        && let Some(stop) = host_stop(runtime, &exec.control, request, scheduled)
    {
        let error = EvalError::execution_stopped(0, stop);
        return unwind_execution_stop(exec, error, top_vm_pc)
            .map(|target| Some(StepControl::Unwind(target)));
    }
    runtime.charge_execution_step(0)?;
    if scheduled && !ignores_cancel && exec.control.can_deliver_task_cancel() {
        return cancel_task(exec, top_vm_pc).map(Some);
    }
    Ok(None)
}

#[cold]
fn cancel_task<'rt>(
    exec: &mut ExecState<'rt>,
    top_vm_pc: Option<usize>,
) -> Result<StepControl<'rt>, EvalError> {
    let mut error = EvalError::execution_stopped(0, ExecutionStop::Cancelled);
    super::capture_error_stack(&exec.frames, &mut error, top_vm_pc);
    exec.control
        .cancel_task(&mut exec.frames, &mut exec.values, error)
        .map(StepControl::Unwind)
}

pub(super) fn execute_task_step<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut pending: Option<PendingContinuation<'rt>>,
) -> TaskStepOutcome<'rt> {
    debug_assert!(!runtime.has_pinned_rust_frame() && exec.jit_frames.is_empty());
    let mut quantum = CHECKPOINT_QUANTUM;
    loop {
        if let Some(event) = exec.host_debug_event.take() {
            debug_assert!(pending.is_none());
            return TaskStepOutcome::Paused(event);
        }
        if exec.frames.is_empty() && pending.is_none() {
            // The root of a native invocation can have completed its own unwind.
            // Check for a stop before committing the execution's result.
            return match poll_step(runtime, exec, None, true, false) {
                Ok(None) => TaskStepOutcome::Completed,
                Ok(Some(_)) => unreachable!("a frameless task cannot unwind to a frame"),
                Err(error) => TaskStepOutcome::Failed(error),
            };
        }
        if quantum == 0 || runtime.native_slice_steps.get() == 0 {
            quantum = CHECKPOINT_QUANTUM;
            if runtime.evaluation_active.get() || task_scheduler::poll_ready(runtime) {
                return TaskStepOutcome::QuantumYield(pending);
            }
            runtime.native_slice_steps.set(CHECKPOINT_QUANTUM);
        }
        let ignores_cancel = matches!(
            pending.as_ref(),
            Some(PendingContinuation(Pending::TaskCall(call))) if call.ignores_task_cancellation()
        );
        let next = match poll_step(runtime, exec, None, true, ignores_cancel) {
            Ok(Some(step)) => {
                pending = None;
                Ok(Some(step))
            }
            Ok(None) => match pending.take() {
                Some(PendingContinuation(action)) => {
                    quantum -= 1;
                    dispatch_pending(runtime, exec, action).map(Some)
                }
                None => run_bytecode_frame(runtime, exec, 0, true, &mut quantum),
            },
            Err(error) => Err(error),
        };
        let step = match next {
            Ok(Some(step)) => step,
            Ok(None) => return TaskStepOutcome::Completed,
            Err(error) => return TaskStepOutcome::Failed(error),
        };
        match step {
            StepControl::Pending(next) => pending = next,
            StepControl::QuantumYield => return TaskStepOutcome::QuantumYield(pending),
            StepControl::Suspend { suspension } => return TaskStepOutcome::Suspend(suspension),
            StepControl::Unwind(target) => {
                if let Some(continuation) = exec.control.take_resumed_restart() {
                    pending = Some(PendingContinuation(Pending::Continuation {
                        continuation,
                        returned: false,
                    }));
                } else if !matches!(target, UnwindTarget::Bytecode(_)) {
                    unreachable!("scheduled task has no enclosing native frames");
                }
            }
        }
    }
}

pub(crate) fn execute_until<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    min_frames: usize,
    pending: Option<PendingContinuation<'rt>>,
) -> Result<NativeReturn, EvalError> {
    execute_until_at_scope(runtime, exec, min_frames, pending, None)
}

pub(crate) fn execute_until_at_scope<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    min_frames: usize,
    mut pending: Option<PendingContinuation<'rt>>,
    min_scopes: Option<usize>,
) -> Result<NativeReturn, EvalError> {
    let mut quantum = CHECKPOINT_QUANTUM;
    loop {
        // A native call or a control transfer can consume the last step of a
        // quantum without reaching a backedge, so grant the next one here.
        if quantum == 0 {
            quantum = CHECKPOINT_QUANTUM;
        }
        let step = if let Some(step) = poll(runtime, exec, None)? {
            pending = None;
            step
        } else if let Some(PendingContinuation(action)) = pending.take() {
            dispatch_pending(runtime, exec, action)?
        } else {
            let Some(step) = run_bytecode_frame(runtime, exec, min_frames, false, &mut quantum)?
            else {
                break;
            };
            step
        };
        match step {
            StepControl::Pending(next) => pending = next,
            StepControl::QuantumYield | StepControl::Suspend { .. } => {
                unreachable!("synchronous execution cannot suspend");
            }
            StepControl::Unwind(target) => {
                // Restart continuations can live outside the debugger's call
                // even when the caller's Lisp frame count has not changed.
                if min_scopes.is_some_and(|min| exec.control.len() < min) {
                    return Ok(target.into());
                }
                // A cleanup can finish in native code. Wait for all intervening
                // native invocations to return before running the restart's work.
                if exec.frames.len() >= min_frames
                    && let Some(continuation) = exec.control.take_resumed_restart()
                {
                    pending = Some(PendingContinuation(Pending::Continuation {
                        continuation,
                        returned: false,
                    }));
                } else if !matches!(target, UnwindTarget::Bytecode(frame) if frame >= min_frames) {
                    return Ok(target.into());
                }
            }
        }
    }
    Ok(exec.values.take_native_return())
}

fn run_bytecode_frame<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    min_frames: usize,
    in_scheduler: bool,
    quantum: &mut u16,
) -> Result<Option<StepControl<'rt>>, EvalError> {
    let cancel_flag = runtime.cancellation_flag_ptr();
    if exec.frames.len() <= min_frames {
        return Ok(None);
    }
    let index = exec.frames.len() - 1;
    let ExecFrame::Bytecode(frame) = &mut exec.frames[index] else {
        return Ok(None);
    };
    let _shield =
        super::inherit_host_cancel_shield(runtime, exec.control.defers_host_cancellation());
    let host_debugging = exec.host_debugging && !runtime.has_pinned_rust_frame();
    let osr_enabled = runtime.heap.code_store.has_osr_backend()
        && frame.osr_countdown != 0
        && frame.vm_code.arity.maximum == Some(frame.vm_code.arity.required)
        && !frame.vm_code.debug.retain_frame
        && !exec.host_debugging
        && exec.stepper.is_none()
        // The VM and native tier account taken branches at different points.
        && runtime.remaining_steps_ptr().is_null()
        && (!in_scheduler || runtime.native_slice_steps.get() != 0)
        && !runtime.has_pinned_rust_frame();
    let mut dispatch_frame = BytecodeFrame {
        runtime,
        index,
        frame,
        values: &mut exec.values,
        control: &mut exec.control,
        cancel_flag,
        quantum: *quantum,
        in_scheduler,
        host_debugging,
        osr_enabled,
    };
    let debugging = runtime.debugger.get().filter(|debugger| {
        debugger.observing_execution(runtime, host_debugging)
            && dispatch_frame.control.debug_pause().is_none()
    });
    let result = if let Some(debugger) = debugging {
        zkcl_bytecode::dispatch::run_frame(&mut DebugBytecodeFrame {
            inner: &mut dispatch_frame,
            controls: &debugger.controls,
            stepper: exec.stepper.as_deref(),
            can_pause: host_debugging || debugger.has_frontend(),
        })
    } else {
        // A halted frame remembers the PC it must resume without re-triggering
        // its breakpoint. With no observer present that skip has no consumer,
        // so drop it here rather than let a later attach inherit a stale PC.
        dispatch_frame.frame.skip_debug_pc = None;
        zkcl_bytecode::dispatch::run_frame(&mut dispatch_frame)
    };
    *quantum = dispatch_frame.quantum;
    match result {
        DispatchOutcome::EndOfFunction => {
            let ExecFrame::Bytecode(returned) = exec.frames.pop().unwrap() else {
                unreachable!();
            };
            Ok(Some(StepControl::Pending(finish(
                returned.resume,
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            ))))
        }
        DispatchOutcome::Exit(FrameExit::Action { pc, action }) => {
            *quantum -= 1;
            dispatch_action(runtime, exec, pc, action).map(Some)
        }
        DispatchOutcome::Exit(FrameExit::Stop { pc, error }) => {
            unwind_execution_stop(exec, error, Some(pc))
                .map(|target| Some(StepControl::Unwind(target)))
        }
        DispatchOutcome::Exit(FrameExit::TaskCancel { pc }) => {
            cancel_task(exec, Some(pc)).map(Some)
        }
        DispatchOutcome::Exit(FrameExit::QuantumYield) => Ok(Some(StepControl::QuantumYield)),
        DispatchOutcome::Exit(FrameExit::Debugger { pc, breakpoint }) => {
            super::signaling::pause_at_instruction(runtime, exec, pc, breakpoint).map(Some)
        }
        DispatchOutcome::Exit(FrameExit::Osr(executable)) => {
            let invocation = NativeInvocation::from_osr(executable, exec.frames[index].bytecode());
            accept(runtime, exec, Invocation::Native(invocation)).map(Some)
        }
    }
}

struct BytecodeFrame<'frame, 'rt> {
    runtime: &'rt Runtime,
    index: usize,
    frame: &'frame mut Frame<'rt>,
    values: &'frame mut Values,
    control: &'frame mut ControlStack<'rt>,
    cancel_flag: *const AtomicU8,
    quantum: u16,
    in_scheduler: bool,
    host_debugging: bool,
    osr_enabled: bool,
}

enum FrameExit<'rt> {
    Action {
        pc: usize,
        action: Result<InstructionAction<'rt>, Fault>,
    },
    Stop {
        pc: usize,
        error: EvalError,
    },
    TaskCancel {
        pc: usize,
    },
    QuantumYield,
    Debugger {
        pc: usize,
        breakpoint: Option<u64>,
    },
    Osr(Rc<NativeCode>),
}

struct DebugBytecodeFrame<'a, 'frame, 'rt> {
    inner: &'a mut BytecodeFrame<'frame, 'rt>,
    controls: &'a super::debugging::Controls,
    stepper: Option<&'a super::debugging::Stepper>,
    can_pause: bool,
}

impl<'rt> zkcl_bytecode::dispatch::DispatchFrame for DebugBytecodeFrame<'_, '_, 'rt> {
    type Instruction = ops::VmOp;
    type Exit = FrameExit<'rt>;

    fn instructions(&self) -> &[Self::Instruction] {
        self.inner.instructions()
    }
    fn pc(&mut self) -> &mut usize {
        self.inner.pc()
    }
    fn checkpoint(&mut self) -> ControlFlow<Self::Exit> {
        self.inner
            .checkpoint_safepoint(self.inner.frame.pc.saturating_sub(1))
    }

    fn execute(&mut self, pc: usize, instruction: Self::Instruction) -> ControlFlow<Self::Exit> {
        let frame = &mut self.inner.frame;
        if frame.skip_debug_pc.take() != Some(pc) {
            let breakpoint =
                self.controls
                    .hit(self.inner.runtime, frame.vm_code.id, pc, self.can_pause);
            let stepping = self
                .stepper
                .is_some_and(|step| step.stops_at(self.inner.index + 1))
                && super::debugging::source_boundary(&frame.vm_code, pc);
            if breakpoint.is_some() || stepping {
                frame.pc = pc;
                frame.skip_debug_pc = Some(pc);
                return ControlFlow::Break(FrameExit::Debugger { pc, breakpoint });
            }
        }
        if self
            .stepper
            .is_some_and(super::debugging::Stepper::is_active)
            && matches!(
                instruction,
                ops::VmOp::TailCallCanonical { .. }
                    | ops::VmOp::TailCallNamed { .. }
                    | ops::VmOp::TailCallValue { .. }
            )
        {
            let pc_in_source = frame
                .vm_code
                .canonical_pc(pc)
                .expect("tail call has canonical bytecode");
            return self.inner.execute_op(
                pc,
                ops::VmOp::Slow {
                    pc: pc_in_source as u32,
                },
            );
        }
        self.inner.execute_op(pc, instruction.unfuse_clear_pair())
    }
}

impl<'rt> BytecodeFrame<'_, 'rt> {
    #[inline(always)]
    fn execute_op(&mut self, pc: usize, instruction: ops::VmOp) -> ControlFlow<FrameExit<'rt>> {
        crate::metrics::record_dispatch();
        match ops::dispatch_vm_op(
            self.runtime,
            self.frame,
            self.index,
            pc,
            instruction,
            self.values,
            self.control,
        ) {
            Ok(InstructionAction::Continue) => ControlFlow::Continue(()),
            action => ControlFlow::Break(FrameExit::Action { pc, action }),
        }
    }

    #[cold]
    #[inline(never)]
    fn sample_osr(&mut self, from: usize) -> ControlFlow<FrameExit<'rt>> {
        self.frame.osr_countdown = OSR_BACKEDGE_SAMPLE;
        if self.control.len() != self.frame.control_base || self.control.debug_pause().is_some() {
            return ControlFlow::Continue(());
        }
        let code = &self.frame.vm_code;
        let Some(pc) = code.osr_target(from, self.frame.pc) else {
            return ControlFlow::Continue(());
        };
        let entry = self.runtime.heap.code_store.osr_entry_at(
            code,
            pc,
            // Use the call tier's threshold in backedge samples, with a floor
            // to amortize a snapshot over at least four groups of backedges.
            self.runtime.jit_threshold.get().max(4),
            self.runtime.heap.function_epoch(),
        );
        match entry {
            Some(entry) => ControlFlow::Break(FrameExit::Osr(entry)),
            None => ControlFlow::Continue(()),
        }
    }

    #[cold]
    #[inline(never)]
    fn checkpoint_safepoint(&mut self, pc: usize) -> ControlFlow<FrameExit<'rt>> {
        if let Err(error) = self.runtime.charge_execution_step(0) {
            return ControlFlow::Break(FrameExit::Stop { pc, error });
        }
        self.quantum -= 1;
        if self.quantum != 0 {
            return ControlFlow::Continue(());
        }
        self.quantum = CHECKPOINT_QUANTUM;
        self.runtime.profile_sample();
        if self.control.debug_pause().is_none()
            && let Some(debugger) = self
                .runtime
                .debugger
                .get()
                .filter(|debugger| self.host_debugging || debugger.has_frontend())
            && debugger.controls.take_pause(self.runtime)
        {
            let resume_pc = self.frame.pc;
            self.frame.skip_debug_pc = Some(resume_pc);
            return ControlFlow::Break(FrameExit::Debugger {
                pc: resume_pc,
                breakpoint: Some(0),
            });
        }
        if !self.cancel_flag.is_null() {
            // SAFETY: `cancel_flag` points into `Runtime::cancellation`'s `Arc`.
            let request = unsafe { (*self.cancel_flag).load(Ordering::Relaxed) };
            if request != HOST_RUNNING
                && let Some(stop) =
                    host_stop(self.runtime, self.control, request, self.in_scheduler)
            {
                return ControlFlow::Break(FrameExit::Stop {
                    pc,
                    error: EvalError::execution_stopped(0, stop),
                });
            }
        }
        if self.in_scheduler {
            if self.runtime.evaluation_active.get() {
                return ControlFlow::Break(FrameExit::QuantumYield);
            }
            let ready = task_scheduler::poll_ready(self.runtime);
            if self.control.can_deliver_task_cancel() {
                return ControlFlow::Break(FrameExit::TaskCancel { pc });
            }
            if ready {
                return ControlFlow::Break(FrameExit::QuantumYield);
            }
        }
        ControlFlow::Continue(())
    }
}

impl<'rt> zkcl_bytecode::dispatch::DispatchFrame for BytecodeFrame<'_, 'rt> {
    type Instruction = ops::VmOp;
    type Exit = FrameExit<'rt>;

    #[inline]
    fn instructions(&self) -> &[Self::Instruction] {
        &self.frame.vm_code.vm_ops
    }

    #[inline]
    fn pc(&mut self) -> &mut usize {
        &mut self.frame.pc
    }

    #[inline]
    fn execute(&mut self, pc: usize, instruction: Self::Instruction) -> ControlFlow<Self::Exit> {
        self.execute_op(pc, instruction)?;
        // Check budgets, host cancellation, and scheduler yields at backedges
        // and calls (calls exit via `FrameExit::Action`) rather than every op.
        if self.frame.pc <= pc {
            self.checkpoint_safepoint(pc)?;
            if self.osr_enabled {
                self.frame.osr_countdown -= 1;
                if self.frame.osr_countdown == 0 {
                    return self.sample_osr(pc);
                }
            }
        }
        ControlFlow::Continue(())
    }

    #[inline(always)]
    fn checkpoint(&mut self) -> ControlFlow<Self::Exit> {
        ControlFlow::Continue(())
    }
}

fn dispatch_pending<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    action: Pending<'rt>,
) -> Result<StepControl<'rt>, EvalError> {
    let _shield =
        super::inherit_host_cancel_shield(runtime, exec.control.defers_host_cancellation());
    match action {
        Pending::TaskCall(call) => call.dispatch(runtime, exec),
        Pending::StartCall { args, resume, at } => {
            let target = args.get(0);
            let invocation = if runtime.heap.is_symbol(target) {
                super::recovery::call(
                    runtime,
                    target,
                    &args,
                    1..args.len(),
                    &mut exec.values,
                    resume,
                    at,
                )
            } else {
                super::designator(runtime, target, at).and_then(|target| {
                    invoke(
                        runtime,
                        target,
                        &args,
                        1..args.len(),
                        &mut exec.values,
                        resume,
                        at,
                    )
                })
            };
            match invocation {
                Ok(invocation) => accept(runtime, exec, invocation),
                Err(fault) => dispatch_error(runtime, exec, fault),
            }
        }
        Pending::TimerFault { at, message } => {
            dispatch_error(runtime, exec, task_scheduler::timer_fault(at, &message))
        }
        Pending::DebuggerFailure(error) => Err(error),
        Pending::Continuation {
            continuation,
            returned,
        } => dispatch_continuation(runtime, exec, continuation, returned),
        Pending::Signal(search) => dispatch_signal(runtime, exec, search),
        Pending::Restart(selection) => {
            restart::dispatch_restart_operation(runtime, exec, selection)
        }
        Pending::RestartInteractive(interactive_restart) => {
            restart::invoke_with_interactive_arguments(runtime, exec, interactive_restart)
        }
        Pending::AbortRestartFailed(at) => dispatch_error(
            runtime,
            exec,
            Fault::plain(
                Class::ControlError,
                EvalError::new(at, "ABORT restart returned without transferring control"),
            ),
        ),
        Pending::Macroexpand(expansion) => dispatch_macroexpand(runtime, exec, expansion),
        Pending::Compile(definition) => function_definitions::dispatch(runtime, exec, *definition),
        Pending::Evaluate(evaluation) => evaluate::dispatch(runtime, exec, evaluation),
        Pending::Instance {
            operation,
            returned,
        } => instances::dispatch(runtime, exec, operation, returned),
    }
}

#[cfg(test)]
mod tests {
    use super::OSR_BACKEDGE_SAMPLE;

    #[test]
    fn osr_sampling_rotates_across_power_of_two_backedge_cycles() {
        for cycle in [8usize, 16, 32, 64, 128] {
            let mut phases = 0u128;
            for sample in 1..=cycle {
                let edge = sample * usize::from(OSR_BACKEDGE_SAMPLE) % cycle;
                phases |= 1 << edge;
            }
            assert_eq!(
                phases.count_ones(),
                cycle as u32,
                "a {OSR_BACKEDGE_SAMPLE}-backedge cadence aliases a {cycle}-edge cycle"
            );
        }
    }
}

fn dispatch_action<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    vm_pc: usize,
    action: Result<InstructionAction<'rt>, Fault>,
) -> Result<StepControl<'rt>, EvalError> {
    match action {
        Ok(InstructionAction::Continue) => unreachable!("handled in bytecode dispatch"),
        Ok(InstructionAction::Invoke(invocation)) => {
            accept_with_pc(runtime, exec, invocation, Some(vm_pc))
        }
        Ok(InstructionAction::TailInvoke(invocation)) => {
            exec.frames.pop();
            accept(runtime, exec, invocation)
        }
        Ok(InstructionAction::TailCall { target, args, at }) => {
            // Keep the call site of a directly signaled condition live for the
            // debugger and unwinder, including when the signaling call is last.
            let preserve = matches!(
                super::Primitive::decode(target),
                Some(
                    super::Primitive::Error
                        | super::Primitive::Cerror
                        | super::Primitive::InvokeDebugger
                )
            );
            let pc = preserve.then_some(vm_pc);
            match super::call_site::dispatch_tail_call(runtime, exec, target, args, at, preserve) {
                Ok(invocation) => accept_with_pc(runtime, exec, invocation, pc),
                Err(error) => dispatch_error_with_pc(runtime, exec, error, pc),
            }
        }
        Ok(InstructionAction::BeginCleanup) => {
            exec.control
                .begin_cleanup(&mut exec.frames, &mut exec.values);
            Ok(StepControl::Pending(None))
        }
        Ok(InstructionAction::EndCleanup) => {
            let target = exec
                .control
                .end_cleanup(&mut exec.frames, &mut exec.values)?;
            Ok(target.map_or(StepControl::Pending(None), StepControl::Unwind))
        }
        Ok(InstructionAction::Transfer(transfer, at)) => {
            match exec
                .control
                .transfer_control(&mut exec.frames, &mut exec.values, transfer, at)?
            {
                TransferOutcome::ResumeFrame(target) => Ok(StepControl::Unwind(target)),
                TransferOutcome::InvalidExit(error) => {
                    dispatch_error(runtime, exec, Fault::plain(Class::ControlError, error))
                }
            }
        }
        Err(error) => dispatch_error_with_pc(runtime, exec, error, Some(vm_pc)),
    }
}
