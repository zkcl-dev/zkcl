mod breakpoints;
pub(super) use breakpoints::breakpoint_entry;
pub use breakpoints::{
    Breakpoint, BreakpointFollow, BreakpointLocation, BreakpointOptions, BreakpointStatus,
};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use zkcl_bytecode::{CaptureKind, DebugVariableLocation as Location};

use super::callback::Outcome;
use super::{
    Continuation, ContinuationStep, ExecFrame, ExecState, Frame, Primitive, Resume, UnhandledSignal,
};
use crate::code_store::VmCode;
use crate::condition::{Class, Fault};
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::package::Package;
use crate::{EvalError, Root, Runtime, SourceDocument};

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("ZK:%DEBUG-FRAMES", Op::Frames),
    ("ZK:%DEBUG-SNAPSHOT", Op::Snapshot),
    ("ZK:%DEBUG-FAULT", Op::Fault),
    ("ZK:%DEBUG-CONDITION", Op::Condition),
    ("ZK:%DEBUG-LOCALS", Op::Locals),
    ("ZK:%DEBUG-LOCAL", Op::Local),
    ("ZK:%SET-DEBUG-LOCAL", Op::SetLocal),
    ("ZK:%DEBUG-MOTION", Op::Motion),
    ("ZK:%DEBUG-STEP", Op::Step),
    ("ZK:%DEBUG-RETURN", Op::Return),
    ("ZK:%DEBUG-RESTART-FRAME", Op::RestartFrame),
    ("ZK:%DEBUG-VARIABLES", Op::Variables),
    ("ZK:%DEBUG-EVAL-PACKAGE", Op::EvalPackage),
];

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Frames,
    Snapshot,
    Fault,
    Condition,
    Locals,
    Local,
    SetLocal,
    Motion,
    Step,
    Return,
    RestartFrame,
    Variables,
    EvalPackage,
}

impl Op {
    pub(super) fn arity(self) -> usize {
        match self {
            Self::Frames | Self::Snapshot | Self::Fault | Self::Condition => 0,
            Self::Locals
            | Self::Motion
            | Self::Step
            | Self::RestartFrame
            | Self::Variables
            | Self::EvalPackage => 1,
            Self::Local | Self::Return => 2,
            Self::SetLocal => 3,
        }
    }
}

pub(super) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    for &(name, op) in PRIMITIVES {
        let symbol = runtime
            .intern_package(Package::ZK, name.trim_start_matches("ZK:"), 0)?
            .0;
        let function = Primitive::named(name).expect("debug primitive registered");
        if matches!(op, Op::SetLocal) {
            let reader = runtime.intern_package(Package::ZK, "%DEBUG-LOCAL", 0)?.0;
            runtime.heap.set_symbol_setter(reader, function);
        }
        runtime.heap.set_symbol_cell(symbol, true, function);
    }
    Ok(())
}

/// The stack boundary recorded before the debugger adds its own Lisp frames.
#[derive(Clone, Copy)]
pub(super) struct Pause {
    pub frame_count: usize,
    pub pc: Option<usize>,
    pub resumable: bool,
    pub abort: bool,
    pub resume_requested: bool,
    pub from_native: bool,
}

#[derive(Clone, Copy)]
enum StepMode {
    Into,
    Next(usize),
    Out(usize),
}

pub(super) struct Stepper {
    mode: Cell<Option<StepMode>>,
    active_steppers: Rc<Cell<usize>>,
}

impl Stepper {
    pub(super) fn is_active(&self) -> bool {
        self.mode.get().is_some()
    }

    fn set(&self, mode: Option<StepMode>) {
        let active = self.active_steppers.get();
        match (self.mode.replace(mode).is_some(), mode.is_some()) {
            (false, true) => self.active_steppers.set(active + 1),
            (true, false) => self.active_steppers.set(active - 1),
            _ => {}
        }
    }

    pub(super) fn stops_at(&self, depth: usize) -> bool {
        match self.mode.get() {
            Some(StepMode::Into) => true,
            Some(StepMode::Next(previous)) => depth <= previous,
            Some(StepMode::Out(previous)) => depth < previous,
            None => false,
        }
    }
}

impl Drop for Stepper {
    fn drop(&mut self) {
        self.set(None);
    }
}

struct StepScope {
    stepper: Rc<Stepper>,
    previous: Option<StepMode>,
}

impl Drop for StepScope {
    fn drop(&mut self) {
        self.stepper.set(self.previous);
    }
}

#[derive(Default)]
pub(super) struct Controls {
    active_steppers: Rc<Cell<usize>>,
    breakpoints: breakpoints::Registry,
    frame_snapshot: RefCell<Vec<DebugFrame>>,
}

impl Controls {
    fn stepper(&self) -> Rc<Stepper> {
        Rc::new(Stepper {
            mode: Cell::new(None),
            active_steppers: Rc::clone(&self.active_steppers),
        })
    }

    pub(super) fn observing(&self, runtime: &Runtime) -> bool {
        runtime.pause_requested.load(Ordering::Acquire)
            || self.active_steppers.get() != 0
            || self.breakpoints.observing()
    }

    pub(super) fn has_logpoints(&self) -> bool {
        self.breakpoints.has_logpoints()
    }

    pub(super) fn take_pause(&self, runtime: &Runtime) -> bool {
        if runtime.cancellation.is_cancelled() {
            return false;
        }
        let requested = runtime.pause_requested.swap(false, Ordering::AcqRel);
        if requested {
            runtime.cancellation.clear_pause();
        }
        requested
    }

    pub(super) fn hit(
        &self,
        runtime: &Runtime,
        code: crate::code_store::CodeId,
        pc: usize,
        can_pause: bool,
    ) -> Option<u64> {
        if can_pause && self.take_pause(runtime) {
            return Some(0);
        }
        self.breakpoints.hit(runtime, code, pc)
    }
}

pub(super) fn source_boundary(code: &VmCode, pc: usize) -> bool {
    code.source.is_some()
        && (pc == 0
            || code
                .canonical_pc(pc)
                .is_some_and(|pc| code.canonical[pc].source_offset().is_some()))
}

pub(super) struct Request {
    op: Op,
    arguments: RootFrame,
    resume: Resume,
    at: usize,
    step: Option<StepScope>,
}

impl Request {
    pub(super) fn new(
        runtime: &Runtime,
        op: Op,
        slots: &RootSlots,
        range: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Self {
        let arguments = runtime.root_frame(range.len());
        for (index, argument) in range.enumerate() {
            arguments.set(index, slots.get(argument));
        }
        Self {
            op,
            arguments,
            resume,
            at,
            step: None,
        }
    }

    fn result<'rt>(
        &self,
        runtime: &'rt Runtime,
        exec: &ExecState<'rt>,
    ) -> Result<Root<'rt>, Fault> {
        let (signal, pause) = exec
            .control
            .debug_signal()
            .ok_or_else(|| invalid(self.at, "there is no suspended debugger frame"))?;
        let frames = &exec.frames[..pause.frame_count];
        if matches!(self.op, Op::Condition) {
            return Ok(runtime.root(signal.condition.get(0)));
        }
        if matches!(self.op, Op::Fault) {
            let Some(UnhandledSignal::Fault(error)) = &signal.unhandled else {
                return Ok(runtime.nil());
            };
            let top_level_input = matches!(frames, [ExecFrame::Bytecode(frame)]
                if frame.vm_code.name.is_none()
                    && frame.vm_code.source.as_ref().is_some_and(|source| source.path().is_none()));
            return list(
                runtime,
                vec![
                    string(runtime, error.message(), self.at)?,
                    if top_level_input {
                        runtime.t()
                    } else {
                        runtime.nil()
                    },
                ],
                self.at,
            );
        }
        if matches!(self.op, Op::Snapshot) {
            let snapshot = snapshots(frames, pause);
            *runtime
                .debugger
                .get()
                .expect("initialized debugger")
                .controls
                .frame_snapshot
                .borrow_mut() = snapshot;
            return Ok(runtime.nil());
        }
        if matches!(self.op, Op::Frames) {
            let mut results = Vec::with_capacity(frames.len().min(super::MAX_ERROR_STACK_FRAMES));
            for (number, frame) in frames
                .iter()
                .rev()
                .take(super::MAX_ERROR_STACK_FRAMES)
                .enumerate()
            {
                let result = frame_description(
                    runtime,
                    frame,
                    (number == 0).then_some(pause.pc).flatten(),
                    self.at,
                )?;
                results.push(result);
            }
            return list(runtime, results, self.at);
        }

        let frame_argument = usize::from(matches!(self.op, Op::SetLocal));
        let number = self
            .arguments
            .get(frame_argument)
            .as_fixnum()
            .and_then(|number| usize::try_from(number).ok())
            .filter(|&number| number < frames.len())
            .ok_or_else(|| invalid(self.at, "debugger frame number is out of range"))?;
        let selected = &frames[frames.len() - number - 1];
        if matches!(self.op, Op::EvalPackage) {
            let code = match selected {
                ExecFrame::Bytecode(frame) => Some(frame.vm_code.as_ref()),
                ExecFrame::Native(frame) => frame.code.as_deref(),
            };
            return Ok(code
                .and_then(|code| code.debug.reader_package)
                .map(|package| runtime.root(selected.constant(runtime, package as usize)))
                .unwrap_or_else(|| runtime.nil()));
        }
        if let ExecFrame::Native(frame) = selected {
            return self.native_result(runtime, selected, frame, frame_argument);
        }
        let ExecFrame::Bytecode(frame) = selected else {
            unreachable!()
        };
        if matches!(self.op, Op::Variables) {
            let pc = current_pc(frame, if number == 0 { pause.pc } else { None });
            let bindings = locals(runtime, frame, pc).into_iter().map(|(name, location)| {
                let value = local_value(runtime, frame, location)?;
                let writable = frame.vm_code.debug.retain_frame && !matches!(location, Location::Capture(i) if frame.vm_code.captures[i] != CaptureKind::MutableCell);
                Ok((runtime.root(name),runtime.root(value),writable))
            }).collect::<Result<Vec<_>,Fault>>()?;
            let entries = bindings
                .into_iter()
                .map(|(name, value, writable)| {
                    list(
                        runtime,
                        vec![
                            name,
                            value,
                            if writable { runtime.t() } else { runtime.nil() },
                        ],
                        self.at,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            return list(runtime, entries, self.at);
        }
        let pc = current_pc(frame, if number == 0 { pause.pc } else { None });
        let bindings = locals(runtime, frame, pc);
        if matches!(self.op, Op::Locals) {
            // Root every name and value before allocating any Lisp lists.
            let rooted = bindings
                .into_iter()
                .map(|(name, location)| {
                    local_value(runtime, frame, location)
                        .map(|value| (runtime.root(name), runtime.root(value)))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let entries = rooted
                .into_iter()
                .map(|(name, value)| list(runtime, vec![name, value], self.at))
                .collect::<Result<Vec<_>, _>>()?;
            return list(runtime, entries, self.at);
        }
        let name = self.arguments.get(frame_argument + 1);
        let location = bindings
            .into_iter()
            .find_map(|(symbol, location)| (symbol == name).then_some(location))
            .ok_or_else(|| {
                invalid(
                    self.at,
                    "lexical binding is unavailable in the selected frame",
                )
            })?;
        let value = if matches!(self.op, Op::SetLocal) {
            let value = self.arguments.get(0);
            set_local(runtime, frame, location, value, self.at)?;
            value
        } else {
            local_value(runtime, frame, location)?
        };
        Ok(runtime.root(value))
    }
}

impl Continuation for Request {
    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        returned: bool,
    ) -> Outcome<Result<ContinuationStep, Fault>> {
        let result = match self.op {
            Op::Step if returned => {
                self.step.take();
                Ok(ContinuationStep::KeepValues)
            }
            Op::Step => {
                if let Some(debugger) = runtime
                    .debugger
                    .get()
                    .filter(|debugger| exec.host_debugging || debugger.has_frontend())
                {
                    runtime.heap.code_store.invalidate_native_calls();
                    let stepper = exec
                        .stepper
                        .get_or_insert_with(|| debugger.controls.stepper())
                        .clone();
                    let previous = stepper.mode.get();
                    stepper.set(Some(StepMode::Into));
                    self.step = Some(StepScope { stepper, previous });
                }
                Ok(ContinuationStep::Call {
                    target: self.arguments.get(0),
                    args: runtime.root_frame(0),
                    at: self.at,
                })
            }
            Op::Return | Op::RestartFrame => return self.transfer(runtime, exec),
            Op::Motion => self.motion(runtime, exec),
            _ => self
                .result(runtime, exec)
                .map(|value| ContinuationStep::Done(value.slot.get())),
        };
        Outcome::Completed(result)
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

impl Request {
    fn motion(
        &self,
        runtime: &Runtime,
        exec: &mut ExecState<'_>,
    ) -> Result<ContinuationStep, Fault> {
        let command = runtime
            .heap
            .symbol_name(self.arguments.get(0))
            .ok_or_else(|| invalid(self.at, "expected a debugger motion keyword"))?;
        let pause = exec
            .control
            .debug_pause_mut()
            .ok_or_else(|| invalid(self.at, "the debugger is not paused"))?;
        let mode = match command.as_str() {
            "INTO" => Some(StepMode::Into),
            "NEXT" => Some(StepMode::Next(pause.frame_count)),
            "OUT" => Some(StepMode::Out(pause.frame_count)),
            "ABORT" => {
                pause.abort = true;
                None
            }
            "CONTINUE" => None,
            _ => return Err(invalid(self.at, "unknown debugger motion")),
        };
        pause.resume_requested = pause.resumable || pause.abort;
        if mode.is_some() || exec.stepper.is_some() {
            runtime.heap.code_store.invalidate_native_calls();
            let controls = &runtime
                .debugger
                .get()
                .expect("initialized debugger")
                .controls;
            exec.stepper
                .get_or_insert_with(|| controls.stepper())
                .set(mode);
        }
        Ok(ContinuationStep::Done(if pause.resumable {
            Word::T
        } else {
            Word::NIL
        }))
    }
}

pub(super) fn current_pc(frame: &Frame<'_>, stopped_pc: Option<usize>) -> Option<usize> {
    stopped_pc
        .or_else(|| frame.pc.checked_sub(1))
        .and_then(|pc| frame.vm_code.canonical_pc(pc))
}

fn frame_description<'rt>(
    runtime: &'rt Runtime,
    frame: &ExecFrame<'_>,
    stopped_pc: Option<usize>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let trace = frame.diagnostic(stopped_pc);
    let pc = trace.bytecode_pc;
    let code = match frame {
        ExecFrame::Bytecode(frame) => Some(frame.vm_code.as_ref()),
        ExecFrame::Native(frame) => frame.code.as_deref(),
    };
    let offset = trace
        .source_range
        .as_ref()
        .map(|range| range.start)
        .or_else(|| code.and_then(|code| code.debug.entry));
    let name = trace.function.as_deref().unwrap_or(if trace.native {
        "<native frame>"
    } else {
        "<top level or anonymous function>"
    });
    let location = trace.source.as_ref().map(|source| {
        source.path().map_or_else(
            || format!("repl submission {}", source.id()),
            |path| path.display().to_string(),
        )
    });
    let text = trace.source.as_ref().and_then(SourceDocument::text);
    let (line, column, excerpt) = text
        .zip(offset)
        .and_then(|(text, offset)| source_line(text, offset))
        .map_or((None, None, None), |(line, column, text)| {
            (Some(line), Some(column), Some(text))
        });
    list(
        runtime,
        vec![
            string(runtime, name, at)?,
            location
                .as_deref()
                .map(|text| string(runtime, text, at))
                .transpose()?
                .unwrap_or_else(|| runtime.nil()),
            number(runtime, line, at)?,
            number(runtime, column, at)?,
            excerpt
                .map(|text| string(runtime, text, at))
                .transpose()?
                .unwrap_or_else(|| runtime.nil()),
            number(runtime, pc, at)?,
            if trace.native {
                runtime.t()
            } else {
                runtime.nil()
            },
            if matches!(frame, ExecFrame::Bytecode(_))
                || matches!(frame,ExecFrame::Native(f) if f.debug_pc().is_some())
            {
                runtime.t()
            } else {
                runtime.nil()
            },
            if matches!(frame, ExecFrame::Bytecode(f) if f.debug_arguments.is_some())
                || matches!(frame,ExecFrame::Native(f) if f.debug_arguments.is_some())
            {
                runtime.t()
            } else {
                runtime.nil()
            },
            if matches!(frame,ExecFrame::Native(f) if f.debug_pc().is_some()) {
                runtime.t()
            } else {
                runtime.nil()
            },
        ],
        at,
    )
}

fn source_line(text: &str, offset: usize) -> Option<(usize, usize, &str)> {
    if !text.is_char_boundary(offset) {
        return None;
    }
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let line = before.bytes().filter(|&byte| byte == b'\n').count() + 1;
    let column = text[line_start..offset].chars().count() + 1;
    let excerpt = text[line_start..]
        .split('\n')
        .next()?
        .trim_end_matches('\r');
    Some((line, column, excerpt))
}

fn locals(runtime: &Runtime, frame: &Frame<'_>, pc: Option<usize>) -> Vec<(Word, Location)> {
    let Some(pc) = pc else {
        return Vec::new();
    };
    let mut names = HashSet::new();
    let mut bindings = Vec::new();
    let variables = frame
        .vm_code
        .debug
        .variables
        .iter()
        .rev()
        .filter(|variable| variable.live.contains(&pc));
    for variable in variables
        .clone()
        .filter(|var| matches!(var.location, Location::Register { .. }))
        .chain(variables.filter(|var| matches!(var.location, Location::Capture(_))))
    {
        let name = frame.constant(runtime, variable.name as usize);
        if names.insert(name) {
            bindings.push((name, variable.location));
        }
    }
    bindings
}

fn local_value(runtime: &Runtime, frame: &Frame<'_>, location: Location) -> Result<Word, Fault> {
    match location {
        Location::Register {
            index,
            boxed: false,
        } => Ok(frame.slots.get(index)),
        Location::Register { index, boxed: true } => {
            frame.read_box(runtime, super::CellLocation::Local(index))
        }
        Location::Capture(index) => frame.read_box(runtime, super::CellLocation::Capture(index)),
    }
}

fn set_local(
    runtime: &Runtime,
    frame: &Frame<'_>,
    location: Location,
    value: Word,
    at: usize,
) -> Result<(), Fault> {
    if !frame.vm_code.debug.retain_frame {
        return Err(invalid(
            at,
            "changing a frame variable requires (OPTIMIZE (DEBUG 3))",
        ));
    }
    let cell = match location {
        Location::Register {
            index,
            boxed: false,
        } => {
            frame.slots.set(index, value);
            return Ok(());
        }
        Location::Register { index, boxed: true } => frame.slots.get(index),
        Location::Capture(index) if frame.vm_code.captures[index] == CaptureKind::MutableCell => {
            frame.cell(runtime, super::CellLocation::Capture(index))?
        }
        Location::Capture(_) => return Err(invalid(at, "this capture is stored by value")),
    };
    runtime
        .heap
        .write_cell(cell, value)
        .ok_or_else(|| invalid(at, "lexical cell is unavailable"))
}

fn list<'rt>(
    runtime: &'rt Runtime,
    entries: Vec<Root<'rt>>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let mut tail = runtime.nil();
    for root in entries.into_iter().rev() {
        tail = runtime
            .cons(&root, &tail)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    Ok(tail)
}

fn number(runtime: &Runtime, number: Option<usize>, at: usize) -> Result<Root<'_>, Fault> {
    number.map_or_else(
        || Ok(runtime.nil()),
        |value| {
            runtime
                .integer_from(value)
                .map_err(|error| EvalError::out_of_memory(at, error).into())
        },
    )
}

fn string<'rt>(runtime: &'rt Runtime, text: &str, at: usize) -> Result<Root<'rt>, Fault> {
    runtime
        .make_string(text)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn invalid(at: usize, message: &'static str) -> Fault {
    Fault::plain(Class::ControlError, EvalError::new(at, message))
}

/// A separate, thread-safe pause request delivered to the installed debugger.
/// Resuming does not cancel the invocation. Blocking host callbacks must return
/// before an execution checkpoint can deliver the request.
#[derive(Clone, Default)]
pub struct PauseHandle(
    Arc<AtomicBool>,
    Option<Arc<crate::execution::ExecutionWake>>,
    Option<crate::CancellationHandle>,
);
impl std::fmt::Debug for PauseHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PauseHandle")
            .field("pending", &self.is_pending())
            .finish()
    }
}
impl PauseHandle {
    pub fn pause(&self) {
        if let Some(cancellation) = &self.2 {
            cancellation.request_pause();
        }
        self.0.store(true, Ordering::Release);
        if let Some(wake) = &self.1 {
            wake.notify();
        }
    }
    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
        if let Some(cancellation) = &self.2 {
            cancellation.clear_pause();
        }
    }
    pub fn is_pending(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugMotion {
    Continue,
    Into,
    Next,
    Out,
    Abort,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugLocalsAvailability {
    Available,
    Unavailable(String),
}
/// An owned source snapshot of a paused activation. Its index is valid only
/// until the debugger resumes, transfers control, or enters another pause.
#[derive(Clone, Debug)]
pub struct DebugFrame {
    pub index: usize,
    pub function: String,
    pub source_path: Option<String>,
    pub source: Option<SourceDocument>,
    pub source_range: Option<Range<usize>>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub source_text: Option<String>,
    pub bytecode_pc: Option<usize>,
    pub native: bool,
    pub locals: DebugLocalsAvailability,
    pub can_return: bool,
    pub can_restart: bool,
}
/// Values are GC roots owned by the caller; the frame index lasts only for this pause.
pub struct DebugVariable<'rt> {
    pub name: Root<'rt>,
    pub value: Option<Root<'rt>>,
    pub unavailable: Option<String>,
    pub writable: bool,
}

/// A stopped invocation's condition and stack, owned independently of the pause.
pub struct DebugEvent<'rt> {
    pub condition: Root<'rt>,
    pub condition_type: String,
    pub message: String,
    pub frames: Vec<DebugFrame>,
}

pub(crate) struct PausedEvent {
    condition: RootSlot,
    condition_type: String,
    message: String,
    frames: Vec<DebugFrame>,
}

impl PausedEvent {
    pub(super) fn new(
        runtime: &Runtime,
        exec: &ExecState<'_>,
        search: &super::HandlerSearch,
    ) -> Self {
        let condition = runtime.root_slot(search.signal.condition.get(0));
        let condition_type = runtime
            .heap
            .condition_class(condition.get())
            .map(|class| class.name().to_owned())
            .or_else(|| {
                crate::condition::instance_name(runtime, condition.get())
                    .and_then(|name| runtime.heap.symbol_name(name))
            })
            .unwrap_or_else(|| "CONDITION".into());
        let message = match &search.signal.unhandled {
            Some(UnhandledSignal::Fault(error)) => error.message().to_owned(),
            _ => crate::condition::report(runtime, condition.get(), search.signal.at)
                .ok()
                .flatten()
                .unwrap_or_else(|| condition_type.clone()),
        };
        let pause = search.debug_pause.expect("host debugger pause");
        let frames = snapshots(&exec.frames[..pause.frame_count], pause);
        Self {
            condition,
            condition_type,
            message,
            frames,
        }
    }

    pub(crate) fn event<'rt>(&self, runtime: &'rt Runtime) -> DebugEvent<'rt> {
        DebugEvent {
            condition: runtime.root(self.condition.get()),
            condition_type: self.condition_type.clone(),
            message: self.message.clone(),
            frames: self.frames.clone(),
        }
    }
}
impl Runtime {
    pub fn pause_handle(&self) -> PauseHandle {
        PauseHandle(
            Arc::clone(&self.pause_requested),
            Some(Arc::clone(self.execution_wake())),
            Some(self.cancellation.clone()),
        )
    }
    fn debug_call<'rt>(
        &'rt self,
        name: &str,
        args: &[&Root<'_>],
    ) -> Result<Vec<Root<'rt>>, EvalError> {
        let symbol = self.root(self.intern_package(Package::ZK, name, 0)?.0);
        self.check_call_roots(&symbol, args)?;
        let mut arguments = self.nil();
        for value in args.iter().rev() {
            arguments = self
                .cons(value, &arguments)
                .map_err(|e| EvalError::out_of_memory(0, e))?;
        }
        let result = self.debug_lambda(
            r#"(cl:lambda (function args)
            (cl:handler-case
                (cl:cons cl:t (cl:multiple-value-list (cl:apply function args)))
              (cl:error (condition) (cl:list cl:nil condition))))"#,
            &[&symbol, &arguments],
        )?;
        self.debug_operation_values(&result[0])
    }
    fn debug_operation_values<'rt>(
        &'rt self,
        result: &Root<'_>,
    ) -> Result<Vec<Root<'rt>>, EvalError> {
        let (success, tail) = self
            .heap
            .cons_fields(result.slot.get())
            .expect("debug operation result");
        if success == Word::NIL {
            let condition = self.heap.cons_car(tail).expect("debug operation condition");
            return Err(crate::condition::to_eval_error(self, condition, 0));
        }
        Ok(self
            .debug_list(tail)
            .into_iter()
            .map(|value| self.root(value))
            .collect())
    }
    fn debug_list(&self, mut list: Word) -> Vec<Word> {
        let mut words = Vec::new();
        while let Some((head, tail)) = self.heap.cons_fields(list) {
            words.push(head);
            list = tail;
        }
        words
    }
    pub fn debug_frames(&self) -> Result<Vec<DebugFrame>, EvalError> {
        let result = self.debug_call("%DEBUG-SNAPSHOT", &[])?;
        drop(result);
        Ok(std::mem::take(
            &mut *self
                .debugger
                .get()
                .expect("initialized debugger")
                .controls
                .frame_snapshot
                .borrow_mut(),
        ))
    }

    pub fn debug_variables(&self, frame: usize) -> Result<Vec<DebugVariable<'_>>, EvalError> {
        let frame = self
            .integer_from(frame)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        let result = self.debug_call("%DEBUG-VARIABLES", &[&frame])?;
        let rows = self.debug_list(result[0].slot.get());
        if let Some(&first) = rows.first() {
            if let Some(reason) = self.root(first).as_string(self) {
                return Ok(vec![DebugVariable {
                    name: self.nil(),
                    value: None,
                    unavailable: Some(reason),
                    writable: false,
                }]);
            }
        }
        Ok(rows
            .into_iter()
            .map(|row| {
                let f = self.debug_list(row);
                DebugVariable {
                    name: self.root(f[0]),
                    value: Some(self.root(f[1])),
                    unavailable: None,
                    writable: f[2] != Word::NIL,
                }
            })
            .collect())
    }
    pub fn debug_local(&self, frame: usize, name: &Root<'_>) -> Result<Root<'_>, EvalError> {
        let index = self
            .integer_from(frame)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        Ok(self.debug_call("%DEBUG-LOCAL", &[&index, name])?.remove(0))
    }

    /// Look up a bare symbol among the frame's lexical bindings. This only
    /// handles standard reader syntax and never reads or evaluates source.
    pub fn debug_find_local(
        &self,
        frame: usize,
        text: &str,
    ) -> Result<Option<Root<'_>>, EvalError> {
        let token = text.trim_matches(|ch: char| ch.is_ascii_whitespace());
        if token.starts_with('#')
            || token.bytes().all(|byte| byte == b'.')
            || !token.chars().all(crate::symbol::is_symbol_constituent)
            || self.custom_readtable()
        {
            return Ok(None);
        }
        let base = self
            .packages
            .borrow()
            .present(Package::COMMON_LISP, "*READ-BASE*")
            .and_then(|(symbol, _)| self.heap.symbol_cell(symbol, false))
            .and_then(Word::as_fixnum)
            .filter(|base| (2..=36).contains(base));
        if base.is_none_or(|base| crate::read::numeric_or_reserved(token, base as u32)) {
            return Ok(None);
        }
        let index = self
            .integer_from(frame)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        let package = self.debug_call("%DEBUG-EVAL-PACKAGE", &[&index])?;
        let package = Package::from_word(package[0].slot.get()).or_else(|| self.bound_package());
        let name = token.to_ascii_uppercase();
        let symbol = {
            let packages = self.packages.borrow();
            package.and_then(|package| {
                packages.name(package)?;
                packages
                    .find(package, &name)
                    .map(|(symbol, _)| self.root(symbol))
            })
        };
        let Some(symbol) = symbol else {
            return Ok(None);
        };
        Ok(self
            .debug_variables(frame)?
            .into_iter()
            .find_map(|variable| {
                if variable.name.slot.get() == symbol.slot.get() {
                    variable.value
                } else {
                    None
                }
            }))
    }

    pub fn debug_set_local(
        &self,
        frame: usize,
        name: &Root<'_>,
        value: &Root<'_>,
    ) -> Result<(), EvalError> {
        let index = self
            .integer_from(frame)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        self.debug_call("%SET-DEBUG-LOCAL", &[value, &index, name])?;
        Ok(())
    }
    pub fn debug_motion(&self, motion: DebugMotion) -> Result<bool, EvalError> {
        let name = match motion {
            DebugMotion::Continue => "CONTINUE",
            DebugMotion::Into => "INTO",
            DebugMotion::Next => "NEXT",
            DebugMotion::Out => "OUT",
            DebugMotion::Abort => "ABORT",
        };
        let symbol = self.root(self.intern_package(Package::KEYWORD, name, 0)?.0);
        Ok(self.debug_call("%DEBUG-MOTION", &[&symbol])?[0].slot.get() != Word::NIL)
    }
    pub fn debug_return_frame(&self, frame: usize, values: &[&Root<'_>]) -> Result<(), EvalError> {
        let index = self
            .integer_from(frame)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        self.check_call_roots(&index, values)?;
        let mut list = self.nil();
        for value in values.iter().rev() {
            list = self
                .cons(value, &list)
                .map_err(|e| EvalError::out_of_memory(0, e))?;
        }
        self.debug_call("%DEBUG-RETURN", &[&index, &list])?;
        Ok(())
    }
    pub fn debug_restart_frame(&self, frame: usize) -> Result<(), EvalError> {
        let index = self
            .integer_from(frame)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        self.debug_call("%DEBUG-RESTART-FRAME", &[&index])?;
        Ok(())
    }
    fn debug_lambda<'rt>(
        &'rt self,
        source: &str,
        args: &[&Root<'_>],
    ) -> Result<Vec<Root<'rt>>, EvalError> {
        let function = super::instances::compile_builtin_lambda(self, source, &[])?;
        self.call(&self.root(function), args)
    }
    pub fn debug_eval(&self, frame: usize, source: &str) -> Result<Vec<Root<'_>>, EvalError> {
        let index = self
            .integer_from(frame)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        let text = self
            .make_string(source)
            .map_err(|e| EvalError::out_of_memory(0, e))?;
        let result=self.debug_lambda(r#"(cl:lambda (frame text)
          (cl:handler-case
              (cl:cons cl:t
                (cl:multiple-value-list
                  (cl:let* ((cl:*package* (cl:or (zk::%debug-eval-package frame) cl:*package*))
                            (form (cl:read-from-string text))
                            (bindings
                              (cl:mapcar
                                (cl:lambda (entry)
                                  (cl:list (cl:car entry)
                                    (cl:list 'zk::%debug-local frame (cl:list 'cl:quote (cl:car entry)))))
                                (zk::%debug-locals frame))))
                    (cl:eval (cl:list 'cl:symbol-macrolet bindings form)))))
            (cl:error (condition) (cl:list cl:nil condition))))"#,&[&index,&text])?;
        self.debug_operation_values(&result[0])
    }

    /// Print a committed result and return printer conditions to the caller.
    pub fn print_result_handling_errors(
        &self,
        value: &Root<'_>,
        pretty: Option<bool>,
    ) -> Result<String, EvalError> {
        let printer = self.root(Primitive::named("WRITE-TO-STRING").expect("standard printer"));
        self.check_call_roots(&printer, &[value])?;
        let table = self.root_slot(
            self.heap
                .readtable(crate::heap::ReadtableCase::Upcase)
                .map_err(|error| EvalError::out_of_memory(0, error))?,
        );
        let binding = |name, value| Ok::<_, EvalError>((self.intern_common_lisp(name, 0)?, value));
        let function = super::with_special_bindings(
            self,
            [
                binding("*READTABLE*", table.get())?,
                binding("*READ-SUPPRESS*", Word::NIL)?,
                binding(
                    "*READ-BASE*",
                    Word::fixnum(10).expect("decimal is a fixnum"),
                )?,
                binding("*PACKAGE*", Package::USER.word())?,
            ],
            || {
                super::instances::compile_builtin_lambda(
                    self,
                    r#"(cl:lambda (printer value pretty override)
                 (cl:handler-case
                     (cl:list cl:t (cl:if override
                       (cl:funcall printer value :pretty pretty)
                       (cl:funcall printer value)))
                   (cl:error (condition) (cl:list cl:nil condition))))"#,
                    &[],
                )
            },
        )?;
        let enabled = if pretty == Some(true) {
            self.t()
        } else {
            self.nil()
        };
        let override_pretty = if pretty.is_some() {
            self.t()
        } else {
            self.nil()
        };
        let result = self.call(
            &self.root(function),
            &[&printer, value, &enabled, &override_pretty],
        )?;
        let values = self.debug_operation_values(&result[0])?;
        values
            .first()
            .and_then(|text| self.string(text))
            .ok_or_else(|| EvalError::new(0, "printer did not return a string"))
    }

    pub fn debug_restarts(&self) -> Result<Vec<Root<'_>>, EvalError> {
        let result = self.debug_lambda(
            r#"(cl:lambda ()
          (cl:handler-case
              (cl:cons cl:t (cl:compute-restarts (zk::%debug-condition)))
            (cl:error (condition) (cl:list cl:nil condition))))"#,
            &[],
        )?;
        self.debug_operation_values(&result[0])
    }

    pub fn debug_invoke_restart(
        &self,
        restart: &Root<'_>,
        args: &[&Root<'_>],
    ) -> Result<Vec<Root<'_>>, EvalError> {
        self.check_call_roots(restart, args)?;
        let mut list = self.nil();
        for value in args.iter().rev() {
            list = self
                .cons(value, &list)
                .map_err(|e| EvalError::out_of_memory(0, e))?;
        }
        let result = self.debug_lambda(
            r#"(cl:lambda (restart args)
          (cl:handler-case
              (cl:progn (zk::%debug-fault)
                (cl:cons cl:t (cl:multiple-value-list (cl:apply #'cl:invoke-restart restart args))))
            (cl:error (condition) (cl:list cl:nil condition))))"#,
            &[restart, &list],
        )?;
        self.debug_operation_values(&result[0])
    }
}

struct FrameTransfer {
    target: Option<RootSlot>,
    global_name: Option<RootSlot>,
    arguments: RootFrame,
    resume: Resume,
    at: usize,
    native_frame: Option<usize>,
}
impl Continuation for FrameTransfer {
    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        returned: bool,
    ) -> Outcome<Result<ContinuationStep, Fault>> {
        if (returned || self.target.is_none())
            && let Some(frame) = self.native_frame
        {
            return Outcome::Unwind(super::control_stack::UnwindTarget::Native {
                frame,
                state: usize::MAX,
            });
        }
        Outcome::Completed(self.step(runtime, &exec.values, returned))
    }
    fn step(
        &mut self,
        runtime: &Runtime,
        _values: &super::Values,
        returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if returned || self.target.is_none() {
            return Ok(ContinuationStep::KeepValues);
        }
        if let Some(name) = self.global_name.take() {
            let target =
                super::function_definitions::FunctionName::parse(runtime, name.get(), self.at)?
                    .get(runtime)
                    .ok_or_else(|| invalid(self.at, "the global function is no longer defined"))?;
            self.target = Some(runtime.root_slot(target));
        }
        let args = runtime.root_frame(self.arguments.len());
        for i in 0..args.len() {
            args.set(i, self.arguments.get(i));
        }
        Ok(ContinuationStep::Call {
            target: self.target.as_ref().unwrap().get(),
            args,
            at: self.at,
        })
    }
    fn resume_to(&self) -> Resume {
        self.resume
    }
}
impl Request {
    fn transfer<'rt>(
        &self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
    ) -> Outcome<Result<ContinuationStep, Fault>> {
        let result = (|| {
            let pause = exec
                .control
                .debug_pause()
                .ok_or_else(|| invalid(self.at, "the debugger is not paused"))?;
            let number = self
                .arguments
                .get(0)
                .as_fixnum()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|&n| n < pause.frame_count)
                .ok_or_else(|| invalid(self.at, "debugger frame number is out of range"))?;
            let index = pause.frame_count - number - 1;
            if let ExecFrame::Native(frame) = &exec.frames[index] {
                let scope_boundary = exec
                    .jit_frames
                    .iter()
                    .rev()
                    .find(|entry| entry.frame_index == index)
                    .map(|entry| entry.control_base)
                    .ok_or_else(|| {
                        invalid(
                            self.at,
                            "the native activation has no recoverable control boundary",
                        )
                    })?;
                if frame.debug_pc().is_none() {
                    return Err(invalid(
                        self.at,
                        "this optimized native activation has no resumable debug map",
                    ));
                }
                if matches!(self.op, Op::RestartFrame) {
                    let saved = frame.debug_arguments.as_ref().ok_or_else(|| {
                        invalid(
                            self.at,
                            "original arguments were not retained for this native activation",
                        )
                    })?;
                    let args = runtime.root_frame(saved.len());
                    for i in 0..args.len() {
                        args.set(i, saved.get(i));
                    }
                    let old = frame.slots().get(1);
                    let global_name = frame
                        .code
                        .as_ref()
                        .filter(|code| code.debug.global_function)
                        .and_then(|code| code.debug.function_name)
                        .map(|name| {
                            runtime.root_slot(exec.frames[index].constant(runtime, name as usize))
                        });
                    let continuation = Box::new(FrameTransfer {
                        target: Some(runtime.root_slot(old)),
                        global_name,
                        arguments: args,
                        resume: Resume::Call(None),
                        at: self.at,
                        native_frame: Some(index),
                    });
                    return exec
                        .control
                        .debug_native_restart(
                            &mut exec.frames,
                            &mut exec.values,
                            index,
                            scope_boundary,
                            continuation,
                        )
                        .map_err(Fault::from);
                }
                let mut words = Vec::new();
                super::visit_proper_list(&runtime.heap, self.arguments.get(1), self.at, |word| {
                    words.push(word)
                })?;
                exec.values.resize(words.len());
                for (i, word) in words.into_iter().enumerate() {
                    exec.values.slots.set(i, word);
                }
                return exec
                    .control
                    .native_debug_return(&mut exec.frames, &mut exec.values, index, scope_boundary)
                    .map_err(Fault::from);
            }
            let ExecFrame::Bytecode(frame) = &exec.frames[index] else {
                unreachable!()
            };
            let resume = frame.resume;
            let at = frame.call_at;
            let scope_boundary = frame.control_base;
            let (target, global_name, args) = if matches!(self.op, Op::RestartFrame) {
                let saved = frame.debug_arguments.as_ref().ok_or_else(|| {
                    invalid(
                        self.at,
                        "original arguments were not retained; restarting requires DEBUG 3",
                    )
                })?;
                let args = runtime.root_frame(saved.len());
                for i in 0..args.len() {
                    args.set(i, saved.get(i));
                }
                let old = frame.slots.get(frame.closure);
                let global_name = frame
                    .vm_code
                    .debug
                    .function_name
                    .filter(|_| frame.vm_code.debug.global_function)
                    .map(|name| runtime.root_slot(frame.constant(runtime, name as usize)));
                (Some(runtime.root_slot(old)), global_name, args)
            } else {
                let mut words = Vec::new();
                super::visit_proper_list(&runtime.heap, self.arguments.get(1), self.at, |word| {
                    words.push(word)
                })?;
                exec.values.resize(words.len());
                for (i, word) in words.into_iter().enumerate() {
                    exec.values.slots.set(i, word);
                }
                (None, None, runtime.root_frame(0))
            };
            let continuation = Box::new(FrameTransfer {
                target,
                global_name,
                arguments: args,
                resume,
                at,
                native_frame: None,
            });
            exec.control
                .debug_frame_transfer(
                    &mut exec.frames,
                    &mut exec.values,
                    index,
                    scope_boundary,
                    continuation,
                )
                .map_err(Fault::from)
        })();
        match result {
            Ok(target) => Outcome::Unwind(target),
            Err(error) => Outcome::Completed(Err(error)),
        }
    }
}

impl Request {
    fn native_result<'rt>(
        &self,
        runtime: &'rt Runtime,
        selected: &ExecFrame<'_>,
        frame: &super::NativeFrameAnchor,
        frame_argument: usize,
    ) -> Result<Root<'rt>, Fault> {
        let Some(pc) = frame.debug_pc() else {
            if matches!(self.op, Op::Variables) {
                return list(
                    runtime,
                    vec![string(
                        runtime,
                        "native frame has no live variable map at this location",
                        self.at,
                    )?],
                    self.at,
                );
            }
            return Err(invalid(
                self.at,
                "native frame has no live variable map at this location",
            ));
        };
        let Some(code) = frame.code.as_ref() else {
            return Err(invalid(
                self.at,
                "native code debug information is unavailable",
            ));
        };
        let mut names = HashSet::new();
        let variables = code
            .debug
            .variables
            .iter()
            .rev()
            .filter(|v| v.live.contains(&pc));
        let bindings = variables
            .clone()
            .filter(|v| matches!(v.location, Location::Register { .. }))
            .chain(variables.filter(|v| matches!(v.location, Location::Capture(_))))
            .filter_map(|v| {
                let name = selected.constant(runtime, v.name as usize);
                names.insert(name).then_some((name, v.location))
            })
            .collect::<Vec<_>>();
        let read =
            |location| -> Result<Word, Fault> {
                match location {
                    Location::Register { index, boxed } => {
                        let value = frame.debug_register(index).ok_or_else(|| {
                            invalid(self.at, "native register is unavailable at this location")
                        })?;
                        if boxed {
                            runtime.heap.read_cell(value).ok_or_else(|| {
                                invalid(self.at, "native lexical cell is unavailable")
                            })
                        } else {
                            Ok(value)
                        }
                    }
                    Location::Capture(index) => {
                        let closure = frame.slots().get(1);
                        let value = runtime.heap.capture(closure, index).ok_or_else(|| {
                            invalid(self.at, "native closure capture is unavailable")
                        })?;
                        if code.captures[index] == CaptureKind::MutableCell {
                            runtime.heap.read_cell(value).ok_or_else(|| {
                                invalid(self.at, "native lexical cell is unavailable")
                            })
                        } else {
                            Ok(value)
                        }
                    }
                }
            };
        if matches!(self.op, Op::Locals | Op::Variables) {
            let rooted=bindings.into_iter().map(|(name,location)|{
                let value=read(location)?;
                let writable=!matches!(location,Location::Capture(i) if code.captures[i]!=CaptureKind::MutableCell);
                Ok((runtime.root(name),runtime.root(value),writable))
            }).collect::<Result<Vec<_>,Fault>>()?;
            let entries = rooted
                .into_iter()
                .map(|(name, value, writable)| {
                    let mut fields = vec![name, value];
                    if matches!(self.op, Op::Variables) {
                        fields.push(if writable { runtime.t() } else { runtime.nil() });
                    }
                    list(runtime, fields, self.at)
                })
                .collect::<Result<Vec<_>, _>>()?;
            return list(runtime, entries, self.at);
        }
        let name = self.arguments.get(frame_argument + 1);
        let location = bindings
            .into_iter()
            .find_map(|(candidate, location)| (candidate == name).then_some(location))
            .ok_or_else(|| {
                invalid(
                    self.at,
                    "lexical binding is unavailable in the selected native frame",
                )
            })?;
        if !matches!(self.op, Op::SetLocal) {
            return Ok(runtime.root(read(location)?));
        }
        let value = self.arguments.get(0);
        match location {
            Location::Register {
                index,
                boxed: false,
            } => {
                if !frame.set_debug_register(index, value) {
                    return Err(invalid(
                        self.at,
                        "native register is unavailable at this location",
                    ));
                }
            }
            Location::Register { index, boxed: true } => {
                let cell = frame.debug_register(index).ok_or_else(|| {
                    invalid(self.at, "native register is unavailable at this location")
                })?;
                runtime
                    .heap
                    .write_cell(cell, value)
                    .ok_or_else(|| invalid(self.at, "native lexical cell is unavailable"))?;
            }
            Location::Capture(index) => {
                if code.captures[index] != CaptureKind::MutableCell {
                    return Err(invalid(self.at, "this capture is stored by value"));
                }
                let cell = runtime
                    .heap
                    .capture(frame.slots().get(1), index)
                    .ok_or_else(|| invalid(self.at, "native closure capture is unavailable"))?;
                runtime
                    .heap
                    .write_cell(cell, value)
                    .ok_or_else(|| invalid(self.at, "native lexical cell is unavailable"))?;
            }
        }
        Ok(runtime.root(value))
    }
}

pub(crate) fn native_debug_stop(
    runtime: &Runtime,
    exec: &ExecState<'_>,
    canonical_pc: usize,
    host_debugging: bool,
) -> Option<(Option<u64>, usize)> {
    let Some(debugger) = runtime
        .debugger
        .get()
        .filter(|d| host_debugging || d.has_frontend() || d.controls.has_logpoints())
    else {
        return None;
    };
    if exec.control.debug_pause().is_some() {
        return None;
    }
    let frame = exec
        .frames
        .last()
        .expect("native debugpoint has an activation");
    let ExecFrame::Native(anchor) = frame else {
        return None;
    };
    let code = anchor.code.as_ref()?;
    let vm_pc = code.vm_pc(canonical_pc).unwrap_or(0);
    let breakpoint = if canonical_pc == 0 || code.canonical_pc(vm_pc) == Some(canonical_pc) {
        debugger.controls.hit(
            runtime,
            code.id,
            vm_pc,
            host_debugging || debugger.has_frontend(),
        )
    } else {
        None
    };
    let stepping = exec
        .stepper
        .as_ref()
        .is_some_and(|step| step.stops_at(exec.frames.len()))
        && code.source.is_some()
        && (canonical_pc == 0
            || code
                .canonical
                .get(canonical_pc)
                .is_some_and(|op| op.source_offset().is_some()));
    if breakpoint.is_none() && !stepping {
        return None;
    }
    let at = code
        .canonical
        .get(canonical_pc)
        .and_then(|op| op.source_offset())
        .or(code.debug.entry)
        .unwrap_or(0);
    Some((breakpoint, at))
}

pub(crate) fn native_poll_pause<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    at: usize,
) -> Result<crate::native::NativeReturn, Fault> {
    let Some(debugger) = runtime.debugger.get().filter(|d| d.has_frontend()) else {
        return Ok(crate::native::NativeReturn::One(Word::NIL));
    };
    if exec.control.debug_pause().is_some() || !debugger.controls.take_pause(runtime) {
        return Ok(crate::native::NativeReturn::One(Word::NIL));
    }
    if let Some(frame) = exec.frames.last_mut() {
        frame.record_native_offset(at);
    }
    drive_native_pause(runtime, exec, at, Some(0))
}
pub(crate) fn drive_native_pause<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    at: usize,
    breakpoint: Option<u64>,
) -> Result<crate::native::NativeReturn, Fault> {
    let depth = exec.frames.len();
    let step = super::signaling::pause_debug_event(runtime, exec, None, at, breakpoint)
        .map_err(Fault::from)?;
    match step {
        super::StepControl::Pending(pending) => {
            let returned = super::dispatch::execute_until(runtime, exec, depth, pending)
                .map_err(Fault::dispatched)?;
            match returned {
                crate::native::NativeReturn::One(value) => {
                    exec.values.one(value);
                    Ok(crate::native::NativeReturn::One(Word::NIL))
                }
                crate::native::NativeReturn::Values => {
                    Ok(crate::native::NativeReturn::One(Word::NIL))
                }
                transfer => Ok(transfer),
            }
        }
        super::StepControl::Unwind(target) => Ok(target.into()),
        _ => unreachable!("a synchronous debugger cannot suspend"),
    }
}

fn snapshots(frames: &[ExecFrame<'_>], pause: Pause) -> Vec<DebugFrame> {
    frames
        .iter()
        .rev()
        .take(super::MAX_ERROR_STACK_FRAMES)
        .enumerate()
        .map(|(index, frame)| {
            let mut snapshot =
                debug_frame_snapshot(frame, index, (index == 0).then_some(pause.pc).flatten());
            if index == 0 && pause.from_native {
                snapshot.native = true;
            }
            snapshot
        })
        .collect()
}

fn debug_frame_snapshot(
    frame: &ExecFrame<'_>,
    index: usize,
    stopped_pc: Option<usize>,
) -> DebugFrame {
    let trace = frame.diagnostic(stopped_pc);
    let source = trace.source;
    let (code, can_return, can_restart, native_map) = match frame {
        ExecFrame::Bytecode(f) => (
            Some(f.vm_code.as_ref()),
            true,
            f.debug_arguments.is_some(),
            true,
        ),
        ExecFrame::Native(f) => (
            f.code.as_deref(),
            f.debug_pc().is_some(),
            f.debug_arguments.is_some(),
            f.debug_pc().is_some(),
        ),
    };
    let offset = trace
        .source_range
        .as_ref()
        .map(|r| r.start)
        .or_else(|| code.and_then(|c| c.debug.entry));
    let position = source
        .as_ref()
        .and_then(SourceDocument::text)
        .zip(offset)
        .and_then(|(text, at)| source_line(text, at));
    DebugFrame {
        index,
        function: trace.function.unwrap_or_else(|| {
            if trace.native {
                "<native frame>".into()
            } else {
                "<top level or anonymous function>".into()
            }
        }),
        source_path: source
            .as_ref()
            .and_then(SourceDocument::path)
            .map(|p| p.display().to_string()),
        line: position.map(|(l, _, _)| l),
        column: position.map(|(_, c, _)| c),
        source_text: position.map(|(_, _, t)| t.to_owned()),
        source,
        source_range: trace.source_range,
        bytecode_pc: trace.bytecode_pc,
        native: trace.native,
        locals: if native_map {
            DebugLocalsAvailability::Available
        } else {
            DebugLocalsAvailability::Unavailable(
                "no live native variable map at this location".into(),
            )
        },
        can_return,
        can_restart,
    }
}

impl Controls {
    pub(super) fn write_image(
        &self,
        image: &mut crate::image::ImageWriter<'_>,
    ) -> Result<(), EvalError> {
        if self.active_steppers.get() != 0 {
            return Err(EvalError::new(
                0,
                "runtime image requires debugger stepping and pending pause requests to be cleared",
            ));
        }
        self.breakpoints.write_image(image)
    }
    pub(super) fn read_image(image: &mut crate::image::ImageReader<'_>) -> Result<Self, EvalError> {
        Ok(Self {
            breakpoints: breakpoints::Registry::read_image(image)?,
            ..Self::default()
        })
    }
}
