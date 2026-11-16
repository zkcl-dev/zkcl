use std::ops::Range;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, atomic::AtomicUsize};

use crate::code_store::{ArithOperands, NativeCode, SiteFeedback, VmCode};
use crate::native::{NativeContext, NativeOutcome, NativeReturn, invoke_c_entry, invoke_osr_entry};

use crate::condition::{self, Class, Fault};
use crate::heap::{
    ListSpine, RootFrame, RootFrameAllocator, RootSlot, RootSlots, RootStackFrame, Word,
};
use crate::macros;
use crate::package::Package;
use crate::symbol::Name;
use crate::{EvalError, EvalErrorKind, EvalStackFrame, Root, Runtime, SourceDocument};

mod adjust_array;
mod arguments;
pub(crate) mod array_access;
pub(crate) mod array_write;
pub(crate) mod arrays;
mod association;
pub(crate) mod atomic;
mod call_site;
pub(crate) mod callback;
mod characters;
pub(crate) mod class_definition;
pub(crate) mod classes;
mod coerce;
mod condition_type;
mod function_definitions;
pub(crate) mod function_mop;
pub(crate) mod generic;
pub(crate) mod method_combination;
pub(crate) use function_definitions::{FunctionName, lambda_list_word};
pub(crate) mod control_stack;
pub(crate) mod debugging;
mod deduplicate;
mod dispatch;
pub(crate) mod environment;
pub(crate) mod equality;
mod invocation;
mod keywords;
pub(crate) mod signaling;
pub(crate) use dispatch::{execute_until, execute_until_at_scope};
pub(crate) use environment::initialize_interaction;
pub(crate) mod evaluate;
#[cfg(test)]
mod execution_probe;
mod filesystem;
mod find_position_count;
pub(crate) mod format;
pub(crate) mod hash_tables;
pub(crate) mod instances;
mod list;
mod mapping;
pub(crate) mod methods;
pub(crate) mod modules;
pub(crate) mod mop;
mod multiple_values;
pub(crate) mod numbers;
mod numeric_feedback;
pub(crate) mod ops;
pub(crate) mod packages;
mod pathnames;
pub(crate) mod plists;
mod predicate_remove;
mod primitives;
pub(crate) mod printer;
mod random;
mod read_string;
mod readtables;
pub(crate) mod recovery;
mod reduce;
mod restart;
mod search_mismatch;
mod sequence;
mod sequence_map;
mod sequence_types;
mod sets;
pub(crate) mod simd;
mod sort;
pub(crate) mod streams;
mod strings;
pub(crate) mod structures;
mod symbols;
pub(crate) mod task_scheduler;
mod tree;
pub(crate) mod treelist;
pub(crate) mod type_check;
pub(crate) mod types;

pub(crate) use arguments::ArgumentTail;
use call_site::CallSite;
use call_site::{call_named_range, stage_call};
pub(crate) use control_stack::{ControlStack, TransferOutcome, UnwindTarget};
use control_stack::{ExitPoint, HandlerSearch, NonLocalTransfer, SpecialBindings};
use evaluate::Evaluation;
pub(crate) use evaluate::{eval_host, load_builtin_source, load_host, prepare_eval};
pub(crate) use function_definitions::initialize_function_introspection;
use invocation::invoke;
use keywords::keyword_arguments;
pub(crate) use keywords::named_keyword_arguments;
use numeric_feedback::{exec_numeric_primitive, is_binary_numeric};
pub(crate) use primitives::{Primitive, setf_primitive};
use restart::{CorrectableError, InteractiveRestartInvocation, RestartRequest, RestartSelection};
use signaling::{WarnContinuation, dispatch_signal, start_restartable_signal, warn_invocation};
pub(crate) use zkcl_bytecode::{
    Arity, Callee, CaptureKind, CellLocation, Handler, HandlerTable, HandlerTarget, Immediate,
    Instruction, JumpTable, KeywordTable, Register, RestartSpec, RestartTable, RestartTarget,
};

#[derive(Clone)]
pub(crate) enum ConstantTemplate {
    Immediate(Word),
    String(String, usize),
    Rooted(Rc<RootSlot>),
    LoadTimeCell(Rc<RootSlot>),
    Symbol(Name, usize),
    Pair(usize, usize),
    Function(Box<FunctionTemplate>),
}

/// Compiler output before its constants are instantiated in the heap or encoded in a FASL.
#[derive(Clone)]
pub(crate) struct FunctionTemplate {
    pub(crate) name: Option<String>,
    pub(crate) debug: zkcl_bytecode::FunctionDebug,
    pub(crate) instructions: Vec<Instruction>,
    pub(crate) constants: Vec<ConstantTemplate>,
    pub(crate) lambda_list: Option<u32>,
    pub(crate) registers: usize,
    pub(crate) arity: Arity,
    pub(crate) captures: Vec<CaptureKind>,
    pub(crate) notinline: Vec<u32>,
    pub(crate) vop_calls: Vec<zkcl_bytecode::VopCall>,
    pub(crate) handlers: Vec<HandlerTable>,
    pub(crate) restarts: Vec<RestartTable>,
    pub(crate) keywords: Vec<KeywordTable>,
    pub(crate) jump_tables: Vec<JumpTable>,
    pub(crate) facts: zkcl_bytecode::FunctionFacts,
}

pub(in crate::vm) struct Signal {
    condition: RootFrame,
    unhandled: Option<UnhandledSignal>,
    at: usize,
    vm_pc: Option<usize>,
    resume: Resume,
    associated_restart_scope: Option<usize>,
}

enum UnhandledSignal {
    LispError,
    Fault(EvalError),
}

impl Signal {
    fn new(
        runtime: &Runtime,
        word: Word,
        unhandled: Option<EvalError>,
        at: usize,
        resume: Resume,
    ) -> Self {
        // A condition is always a heap object. The root frame pool reuses
        // registered cells across signals, including a handler's nested signals.
        let condition = runtime.root_frame(1);
        condition.set(0, word);
        Self {
            condition,
            unhandled: unhandled.map(UnhandledSignal::Fault),
            at,
            vm_pc: None,
            resume,
            associated_restart_scope: None,
        }
    }

    fn from_lisp(runtime: &Runtime, word: Word, error: bool, at: usize, resume: Resume) -> Self {
        let mut signal = Self::new(runtime, word, None, at, resume);
        signal.unhandled = error.then_some(UnhandledSignal::LispError);
        signal
    }
}

pub(crate) enum ContinuationStep {
    Done(Word),
    DoneValues(Vec<Word>),
    KeepValues,
    Call {
        target: Word,
        args: RootFrame,
        at: usize,
    },
    Signal {
        condition: Word,
        at: usize,
        unhandled: Option<EvalError>,
    },
    CellError {
        fault: Fault,
    },
    CorrectableError {
        condition: Word,
        report: Word,
        enclosing: Option<usize>,
        at: usize,
    },
    Warning {
        condition: Word,
        enclosing: Option<usize>,
        at: usize,
    },
}

pub(crate) trait Continuation {
    fn step(
        &mut self,
        _runtime: &Runtime,
        _values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        unreachable!("continuation implements step_in_vm")
    }

    /// Most continuations only inspect their previous call's values. File
    /// compilation can call Lisp synchronously while emitting an entry; it
    /// must release every borrow of the VM before entering the dispatcher.
    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        returned: bool,
    ) -> callback::Outcome<Result<ContinuationStep, Fault>> {
        callback::Outcome::Completed(self.step(runtime, &exec.values, returned))
    }

    fn on_start<'a>(
        &mut self,
        _runtime: &'a Runtime,
        _frame: usize,
        _control: &mut ControlStack<'a>,
    ) -> Result<(), Fault> {
        Ok(())
    }

    fn on_done(&mut self, _control: &mut ControlStack<'_>) {}

    fn restart_invoked(&mut self, _binding: usize) {}

    fn signal_pc(&self) -> Option<usize> {
        None
    }

    fn continuation_scope(&self) -> Option<control_stack::ContinuationScope> {
        None
    }

    fn resume_to(&self) -> Resume;
}

pub(crate) struct NativeFrameAnchor {
    pub(crate) slots_ptr: *const AtomicUsize,
    pub(crate) slots_len: usize,
    pub(crate) pending_state: Option<usize>,
    pub(crate) backing: Option<crate::heap::RootStackFrame<'static>>,
    pub(crate) arguments: ArgumentTail<'static>,
    pub(crate) code: Option<Rc<VmCode>>,
    pub(crate) source_offset: Option<usize>,
    pub(crate) debug: Option<crate::native::NativeDebugState>,
    pub(crate) debug_arguments: Option<RootFrame>,
    pub(crate) profile: Option<crate::profiling::ProfileActivation>,
}

/// Metadata of the bytecode activation that the next native frame replaces.
pub(crate) struct OsrActivation {
    pub(crate) call_at: usize,
    pub(crate) profile: Option<crate::profiling::ProfileActivation>,
}

impl NativeFrameAnchor {
    pub(crate) fn slots(&self) -> &RootSlots {
        if let Some(backing) = &self.backing {
            return backing.slots();
        }
        if self.slots_len == 0 {
            RootSlots::empty()
        } else {
            // SAFETY: `NativeRootFrame` keeps the `RootStackFrame` alive until after
            // this anchor is removed from `ExecState::frames`.
            RootSlots::from_slice(unsafe {
                std::slice::from_raw_parts(self.slots_ptr, self.slots_len)
            })
        }
    }
}

pub(crate) enum ExecFrame<'rt> {
    Bytecode(Frame<'rt>),
    Native(NativeFrameAnchor),
}

impl<'rt> ExecFrame<'rt> {
    fn bytecode(&self) -> &Frame<'rt> {
        let Self::Bytecode(frame) = self else {
            unreachable!("bytecode control table belongs to a bytecode frame")
        };
        frame
    }

    pub(crate) fn slots(&self) -> &RootSlots {
        match self {
            Self::Bytecode(frame) => &frame.slots,
            Self::Native(anchor) => anchor.slots(),
        }
    }

    pub(crate) fn constant(&self, runtime: &Runtime, index: usize) -> Word {
        match self {
            Self::Bytecode(frame) => frame.constant(runtime, index),
            Self::Native(anchor) => {
                let slots = anchor.slots();
                if !slots.is_empty() {
                    let target_word = slots.get(0);
                    let code_word = runtime
                        .heap
                        .function_code(target_word)
                        .unwrap_or(target_word);
                    if runtime.heap.function_info(code_word).is_some() {
                        return runtime.heap.function_constant(code_word, index);
                    }
                }
                if index < slots.len() {
                    slots.get(index)
                } else {
                    Word::NIL
                }
            }
        }
    }
}

pub(crate) struct JitFrameEntry {
    pub(crate) frame_index: usize,
    pub(crate) control_base: usize,
    pub(crate) osr_call_at: Option<usize>,
}

pub(crate) struct ExecState<'rt> {
    pub(crate) jit_frames: Vec<JitFrameEntry>,
    pub(crate) frames: Vec<ExecFrame<'rt>>,
    pub(crate) values: Values,
    pub(crate) control: ControlStack<'rt>,
    pub(crate) native_tail: Option<(crate::native::NativeTailCall, Option<Resume>)>,
    pub(crate) host_debugging: bool,
    pub(crate) native_debug_stop: Option<Option<u64>>,
    host_debug_event: Option<debugging::PausedEvent>,
    stepper: Option<Rc<debugging::Stepper>>,
}

impl<'rt> ExecState<'rt> {
    pub(crate) fn new(values: Values) -> Self {
        Self {
            jit_frames: Vec::new(),
            frames: Vec::new(),
            values,
            control: ControlStack::default(),
            native_tail: None,
            host_debugging: false,
            native_debug_stop: None,
            host_debug_event: None,
            stepper: None,
        }
    }
}

const MAX_ERROR_STACK_FRAMES: usize = 64;

fn capture_error_stack(frames: &[ExecFrame<'_>], error: &mut EvalError, top_vm_pc: Option<usize>) {
    if !error.stack_frames().is_empty() || error.kind() == EvalErrorKind::OutOfMemory {
        return;
    }
    let mut stack = Vec::with_capacity(frames.len().min(MAX_ERROR_STACK_FRAMES));
    for (index, frame) in frames.iter().enumerate().rev() {
        let top = index + 1 == frames.len();
        let vm_pc = if top { top_vm_pc } else { None };
        let (code, _, offset, _) = frame.execution_location(vm_pc);
        if top
            && offset == Some(error.offset())
            && let Some(code) = code
            && let Some(source) = &code.source
        {
            error.attach_source_location(source, code.debug.source_at(error.offset()));
        }
        stack.push(frame.diagnostic(vm_pc));
        if stack.len() == MAX_ERROR_STACK_FRAMES {
            break;
        }
    }
    error.attach_stack_frames(stack);
}

impl ExecFrame<'_> {
    fn execution_location(
        &self,
        vm_pc: Option<usize>,
    ) -> (Option<&VmCode>, Option<usize>, Option<usize>, bool) {
        match self {
            Self::Bytecode(frame) => {
                let pc = debugging::current_pc(frame, vm_pc);
                let offset = pc.and_then(|pc| frame.vm_code.canonical[pc].source_offset());
                (Some(frame.vm_code.as_ref()), pc, offset, false)
            }
            Self::Native(frame) => (frame.code.as_deref(), None, frame.source_offset, true),
        }
    }

    fn diagnostic(&self, vm_pc: Option<usize>) -> EvalStackFrame {
        let (code, pc, offset, native) = self.execution_location(vm_pc);
        let source = code.and_then(|code| code.source.clone());
        EvalStackFrame {
            function: code.and_then(|code| code.name.clone()),
            source_path: source
                .as_ref()
                .and_then(SourceDocument::path)
                .map(ToOwned::to_owned),
            source,
            source_range: offset.and_then(|at| code?.debug.source_at(at)?.range.clone()),
            bytecode_pc: pc,
            native,
        }
    }

    pub(crate) fn record_native_offset(&mut self, at: usize) {
        if let Self::Native(frame) = self {
            frame.source_offset = Some(at);
        }
    }
}

/// Opaque pending VM continuation/signal step handed across [`StepControl`].
pub(crate) struct PendingContinuation<'rt>(pub(in crate::vm) Pending<'rt>);

pub(crate) enum StepControl<'rt> {
    Pending(Option<PendingContinuation<'rt>>),
    Unwind(UnwindTarget),
    QuantumYield,
    Suspend {
        suspension: task_scheduler::Suspension,
    },
}

impl<'rt> StepControl<'rt> {
    #[inline]
    pub(in crate::vm) fn pending(p: Pending<'rt>) -> Self {
        Self::Pending(Some(PendingContinuation(p)))
    }
}

/// The caller has resolved the function designator and checked the source arity. When the
/// installed code is a register leaf, guard failure resumes its original bytecode
/// at entry, with all incoming arguments still in the caller's registered slots.
#[expect(
    clippy::too_many_arguments,
    reason = "VM dispatch helper forwards native call site and unwind state directly"
)]
fn try_native_call<'rt>(
    runtime: &'rt Runtime,
    code_id: crate::code_store::CodeId,
    vm_code: &VmCode,
    target: Word,
    slots: &RootSlots,
    args: Range<Register>,
    values: &mut Values,
    resume: Resume,
    fixed_site: bool,
) -> Option<Invocation<'rt>> {
    // A deopt exit is still on the native stack until this chain returns.
    // Tail calls inherit its continuation, so keep the chain in bytecode.
    if matches!(resume, Resume::Deoptimized) {
        return None;
    }
    let executable = runtime.native_entry_for_call(code_id, vm_code)?;
    if let Some(leaf) = &executable.leaf {
        if let Some(word) = leaf.call(runtime, target, |i| slots.get(args.start + i)) {
            values.one(word);
            return Some(Invocation::Done(resume));
        }
        if !executable.has_leaf_fallback() || !runtime.remaining_steps_ptr().is_null() {
            return None;
        }
    }
    Some(Invocation::Native(NativeInvocation::from_slots(
        runtime, executable, target, slots, args, resume, fixed_site,
    )))
}

pub(in crate::vm) enum NativeCallArgs<'rt> {
    /// `slots[0]` is `target`; `slots[1..1 + argc]` are arguments.
    Slice(RootStackFrame<'rt>),
    Fast {
        target: Word,
        args: [Word; 4],
        len: u8,
    },
    Osr,
}

pub(in crate::vm) struct NativeInvocation<'rt> {
    executable: Rc<NativeCode>,
    args: NativeCallArgs<'rt>,
    argc: usize,
    resume: Resume,
}

impl<'rt> NativeInvocation<'rt> {
    fn from_osr(executable: Rc<NativeCode>, frame: &Frame<'rt>) -> Self {
        Self {
            executable,
            args: NativeCallArgs::Osr,
            argc: frame.vm_code.registers,
            resume: frame.resume,
        }
    }

    fn from_slots(
        runtime: &'rt Runtime,
        executable: Rc<NativeCode>,
        target: Word,
        frame: &RootSlots,
        args: Range<Register>,
        resume: Resume,
        fixed_exact_arity: bool,
    ) -> Self {
        let argc = args.len();
        if executable.entry.fast_entry.is_some() && fixed_exact_arity && argc <= 4 {
            let mut fast = [Word::NIL; 4];
            for (i, reg) in args.enumerate() {
                fast[i] = frame.get(reg);
            }
            return Self {
                executable,
                args: NativeCallArgs::Fast {
                    target,
                    args: fast,
                    len: argc as u8,
                },
                argc,
                resume,
            };
        }
        let staged = stage_call(runtime, target, frame, args);
        Self {
            executable,
            args: NativeCallArgs::Slice(staged),
            argc,
            resume,
        }
    }
}

pub(in crate::vm) enum Invocation<'rt> {
    Done(Resume),
    StartCall(RootFrame),
    Lisp(Frame<'rt>),
    Native(NativeInvocation<'rt>),
    Continuation(Box<dyn Continuation + 'rt>),
    TaskCall(task_scheduler::TaskCall),
    CorrectableError(Box<CorrectableError>),
    Warn(Box<WarnContinuation>),
    SimpleRestart {
        restart: RootSlot,
        body: RootSlot,
        resume: Resume,
        at: usize,
    },
    Signal(Signal),
    InvokeDebugger(Signal),
    ConstructCondition(Box<condition_type::Construction>),
    Restart(RestartRequest),
    Macroexpand(Macroexpansion),
    Compile(Box<function_definitions::CompileDefinition>),
    Evaluate(Box<Evaluation>),
    Initialization(Box<instances::Initialization>),
    AccessInstance(Box<instances::Access>),
}

impl Invocation<'_> {
    fn associate_restart(&mut self, group: usize) {
        match self {
            Self::Signal(signal) => signal.associated_restart_scope = Some(group),
            Self::Initialization(init) => init.associate(group),
            Self::CorrectableError(error) => error.enclosing = Some(group),
            Self::Warn(warning) => warning.enclosing = Some(group),
            Self::ConstructCondition(construction) => construction.enclosing = Some(group),
            _ => {}
        }
    }
}

pub(in crate::vm) struct Macroexpansion {
    form: RootFrame,
    environment: RootSlot,
    repeat: bool,
    steps: usize,
    resume: Resume,
    at: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum Resume {
    Call(Option<Register>),
    Deoptimized,
    BranchUnless(FusedBranch),
    Continuation,
    SignalHandler,
    RestartTest,
    RestartInteractive,
    AbortRestart(usize),
    Macroexpander,
    Evaluate,
    Instance,
}

#[derive(Clone, Copy)]
pub(crate) struct FusedBranch {
    target: u32,
    out: u16,
    clear_start: u16,
    clear_len: u16,
}

impl FusedBranch {
    #[inline(always)]
    fn finish(self, frame: &mut Frame<'_>, result: Word, values: &mut Values) {
        frame.slots.set(self.out as usize, result);
        values.resize(0);
        let start = self.clear_start as usize;
        frame.clear(start..start + self.clear_len as usize);
        if result == Word::NIL {
            frame.pc = self.target as usize;
        }
    }
}

pub(in crate::vm) enum Pending<'rt> {
    TaskCall(task_scheduler::TaskCall),
    StartCall {
        args: RootFrame,
        resume: Resume,
        at: usize,
    },
    TimerFault {
        at: usize,
        message: Rc<str>,
    },
    DebuggerFailure(EvalError),
    Continuation {
        continuation: Box<dyn Continuation + 'rt>,
        returned: bool,
    },
    Signal(HandlerSearch),
    Restart(RestartSelection),
    RestartInteractive(InteractiveRestartInvocation),
    AbortRestartFailed(usize),
    Macroexpand(Macroexpansion),
    Compile(Box<function_definitions::CompileDefinition>),
    Evaluate(Box<Evaluation>),
    Instance {
        operation: instances::Operation,
        returned: bool,
    },
}

impl FunctionTemplate {
    pub(crate) fn instantiate(self, runtime: &Runtime) -> Result<Word, EvalError> {
        self.instantiate_with_source(runtime, None)
    }

    pub(crate) fn instantiate_with_source(
        self,
        runtime: &Runtime,
        source: Option<&SourceDocument>,
    ) -> Result<Word, EvalError> {
        let constants = runtime.root_frame(self.constants.len());
        let mut symbols = Vec::with_capacity(self.constants.len());
        for (i, constant) in self.constants.into_iter().enumerate() {
            let word = match constant {
                ConstantTemplate::Immediate(word) => word,
                ConstantTemplate::Rooted(root) | ConstantTemplate::LoadTimeCell(root) => root.get(),
                ConstantTemplate::String(text, at) => runtime
                    .heap
                    .string(&text)
                    .map_err(|error| EvalError::out_of_memory(at, error))?,
                ConstantTemplate::Symbol(name, at) => runtime.intern(&name, at)?,
                ConstantTemplate::Pair(car, cdr) => runtime
                    .heap
                    .cons(|| (constants.get(car), constants.get(cdr)))
                    .map_err(|error| EvalError::out_of_memory(0, error))?,
                ConstantTemplate::Function(code) => {
                    code.instantiate_with_source(runtime, source)?
                }
            };
            constants.set(i, word);
            symbols.push(canonical_symbol_for_word(runtime, word));
        }
        let code_id = runtime
            .heap
            .code_store
            .insert(crate::code_store::CodeToInstall {
                name: self.name,
                source: source.cloned(),
                debug: self.debug,
                arity: self.arity,
                lambda_list: self.lambda_list,
                registers: self.registers,
                instructions: self.instructions,
                captures: self.captures,
                notinline: self.notinline,
                vop_calls: self.vop_calls,
                tables: crate::code_store::CodeSideTables {
                    handlers: self.handlers,
                    restarts: self.restarts,
                    keywords: self.keywords,
                    jump_tables: self.jump_tables,
                    facts: self.facts,
                },
                canonical_symbols: symbols,
                native: None,
            });
        runtime
            .heap
            .function(code_id, &constants)
            .map_err(|error| EvalError::out_of_memory(0, error))
    }
}

pub(crate) fn call_host<'rt>(
    runtime: &'rt Runtime,
    target: &Root<'_>,
    args: &[&Root<'_>],
) -> Result<Vec<Root<'rt>>, EvalError> {
    call_host_values(
        runtime,
        &target.slot,
        args.iter().map(|value| value.slot.get()),
    )
    .map(|values| values.roots(runtime).collect())
}

pub(crate) fn call_host_values(
    runtime: &Runtime,
    target: &RootSlot,
    args: impl ExactSizeIterator<Item = Word>,
) -> Result<Values, EvalError> {
    // A direct call to a primitive or host function can finish before the VM
    // dispatch loop. Check the limit before it can produce side effects.
    runtime.check_execution_safepoint(0)?;
    let argc = args.len();
    let frame = runtime.staging_stack.alloc(argc);
    for (i, word) in args.enumerate() {
        frame.set(i, word);
    }
    if let Some(callback) = runtime.debug_callback.get() {
        return callback::call_from_host(runtime, callback, target, &frame);
    }
    run_values(runtime, target.get(), &frame, 0..argc)
}

pub(crate) fn prepare_call<'rt>(
    runtime: &'rt Runtime,
    target: &Root<'_>,
    args: &[&Root<'_>],
) -> task_scheduler::TaskExecutor<'rt> {
    let slots = runtime.root_frame(args.len() + 1);
    slots.set(0, target.slot.get());
    for (index, arg) in args.iter().enumerate() {
        slots.set(index + 1, arg.slot.get());
    }
    task_scheduler::TaskExecutor::new(Values::new(runtime), Invocation::StartCall(slots))
}

pub(crate) struct Frame<'rt> {
    slots: RootStackFrame<'rt>,
    vm_code: Rc<VmCode>,
    code: Register,
    closure: Register,
    pc: usize,
    resume: Resume,
    arguments: ArgumentTail<'rt>,
    call_at: usize,
    skip_debug_pc: Option<usize>,
    pub(super) debug_arguments: Option<RootFrame>,
    /// Dynamic scopes below this activation, including frameless callers.
    pub(super) control_base: usize,
    profile: Option<crate::profiling::ProfileActivation>,
    osr_countdown: u8,
}

impl Runtime {
    fn alloc_vm_slots(&self, len: usize) -> RootStackFrame<'_> {
        if self.child_task_running.get() {
            RootStackFrame::independent(&self.heap, len)
        } else {
            self.root_stack.alloc(len)
        }
    }
}

impl<'rt> Frame<'rt> {
    #[expect(
        clippy::too_many_arguments,
        reason = "VM frame constructor passes target, code, slots, and resume directly"
    )]
    fn new_with_code(
        runtime: &'rt Runtime,
        target: Word,
        code: Word,
        vm_code: Rc<VmCode>,
        args: &RootSlots,
        range: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Self {
        let argc = range.len();
        let debug_arguments = vm_code.debug.retain_frame.then(|| {
            let saved = runtime.root_frame(argc);
            for (index, source) in range.clone().enumerate() {
                saved.set(index, args.get(source));
            }
            saved
        });
        let profile = runtime.profile_activation(&vm_code);
        let closure = vm_code.registers + usize::from(target != code);
        crate::metrics::record_frame();
        let slots = runtime.alloc_vm_slots(closure + 1);
        let required = vm_code.arity.required;
        for i in 0..required {
            slots.set(i, args.get(range.start + i));
        }
        let arguments = ArgumentTail::new(runtime, argc - required, |index| {
            args.get(range.start + required + index)
        });
        let code_reg = vm_code.registers;
        slots.set(code_reg, code);
        slots.set(closure, target);
        Self {
            slots,
            vm_code,
            code: code_reg,
            closure,
            pc: 0,
            resume,
            arguments,
            call_at: at,
            skip_debug_pc: None,
            debug_arguments,
            control_base: 0,
            profile,
            osr_countdown: dispatch::OSR_BACKEDGE_SAMPLE,
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "deopt frame constructor passes register map, spilled values, and resume directly"
    )]
    fn new_deopt(
        runtime: &'rt Runtime,
        target: Word,
        code: Word,
        vm_code: Rc<VmCode>,
        canon_pc: usize,
        regs: &[u32],
        vals: &[usize],
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let vm_pc = vm_code.vm_pc(canon_pc).ok_or_else(|| {
            Fault::plain(
                Class::ProgramError,
                EvalError::new(at, format!("deopt pc {canon_pc} out of bounds")),
            )
        })?;
        let closure = vm_code.registers + usize::from(target != code);
        crate::metrics::record_frame();
        let slots = runtime.alloc_vm_slots(closure + 1);
        let code_reg = vm_code.registers;
        slots.set(code_reg, code);
        slots.set(closure, target);
        for (&reg, &val) in regs.iter().zip(vals.iter()) {
            let reg_idx = reg as usize;
            if reg_idx < code_reg {
                slots.set(reg_idx, Word::from_raw(val));
            }
        }
        Ok(Self {
            slots,
            vm_code,
            code: code_reg,
            closure,
            pc: vm_pc,
            resume,
            arguments: ArgumentTail::default(),
            call_at: at,
            skip_debug_pc: None,
            debug_arguments: None,
            control_base: 0,
            profile: None,
            // A transferred activation stays in bytecode after native recovery.
            osr_countdown: 0,
        })
    }

    fn new_cached(
        runtime: &'rt Runtime,
        code: Word,
        vm_code: Rc<VmCode>,
        caller: &RootSlots,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Self {
        crate::metrics::record_frame();
        let registers = vm_code.registers;
        let debug_arguments = vm_code.debug.retain_frame.then(|| {
            let saved = runtime.root_frame(args.len());
            for (index, source) in args.clone().enumerate() {
                saved.set(index, caller.get(source));
            }
            saved
        });
        let profile = runtime.profile_activation(&vm_code);
        let slots = runtime.alloc_vm_slots(registers + 1);
        for (i, reg) in args.enumerate() {
            slots.set(i, caller.get(reg));
        }
        slots.set(registers, code);
        Self {
            slots,
            vm_code,
            code: registers,
            closure: registers,
            pc: 0,
            resume,
            arguments: ArgumentTail::default(),
            call_at: at,
            skip_debug_pc: None,
            debug_arguments,
            control_base: 0,
            profile,
            osr_countdown: dispatch::OSR_BACKEDGE_SAMPLE,
        }
    }

    fn constant(&self, runtime: &Runtime, index: usize) -> Word {
        runtime
            .heap
            .function_constant(self.slots.get(self.code), index)
    }

    fn bind_special(
        &self,
        runtime: &'rt Runtime,
        control: &mut ControlStack<'rt>,
        frame_index: usize,
        symbol: usize,
        value: Register,
    ) {
        let binding = (self.constant(runtime, symbol), self.slots.get(value));
        control.bind(frame_index, &runtime.heap, std::iter::once(binding));
    }

    fn cell(&self, runtime: &Runtime, location: CellLocation) -> Result<Word, Fault> {
        match location {
            CellLocation::Local(reg) => Ok(self.slots.get(reg)),
            CellLocation::Capture(index) => runtime
                .heap
                .capture(self.slots.get(self.closure), index)
                .ok_or_else(|| self.invalid_operand("closure capture is out of bounds")),
        }
    }

    fn read_box(&self, runtime: &Runtime, location: CellLocation) -> Result<Word, Fault> {
        let word = self.cell(runtime, location)?;
        if matches!(location, CellLocation::Capture(index)
            if self.vm_code.captures[index] != CaptureKind::MutableCell)
        {
            return Ok(word);
        }
        runtime
            .heap
            .read_cell(word)
            .ok_or_else(|| self.invalid_operand("expected a lexical cell"))
    }

    fn box_local(&self, runtime: &Runtime, reg: Register) -> Result<(), crate::OutOfMemory> {
        let cell = runtime.heap.cell(|| self.slots.get(reg))?;
        self.slots.set(reg, cell);
        Ok(())
    }

    fn make_closure(
        &self,
        runtime: &Runtime,
        code: usize,
        first: Register,
        count: usize,
    ) -> Result<Word, crate::OutOfMemory> {
        runtime.heap.closure(
            count,
            || self.constant(runtime, code),
            |i| self.slots.get(first + i),
        )
    }

    fn invalid_operand(&self, message: &'static str) -> Fault {
        Fault::plain(Class::ProgramError, EvalError::new(self.call_at, message))
    }

    fn clear(&self, regs: Range<Register>) {
        self.slots.clear(regs);
    }

    fn check_keywords(
        &mut self,
        runtime: &Runtime,
        start: usize,
        table: usize,
    ) -> Result<(), Fault> {
        let code = self.slots.get(self.code);
        self.arguments.check_keywords(
            runtime,
            &self.vm_code.keywords[table],
            |index| runtime.heap.function_constant(code, index),
            start,
            self.call_at,
        )
    }

    fn keyword_names(&self, runtime: &Runtime, table: &KeywordTable) -> Vec<Word> {
        table
            .names
            .iter()
            .map(|&index| self.constant(runtime, index))
            .collect()
    }

    #[cold]
    #[inline(never)]
    fn logical_at(&self, vm_pc: usize) -> usize {
        let Some(canon_pc) = self.vm_code.canonical_pc(vm_pc) else {
            return self.call_at;
        };
        self.vm_code.canonical[canon_pc]
            .source_offset()
            .unwrap_or(self.call_at)
    }
}

pub(crate) fn canonical_symbol_for_word(
    runtime: &Runtime,
    word: Word,
) -> Option<ops::CanonicalSymbol> {
    let data = runtime.heap.symbol_attributes(word)?;
    let spelling = runtime.heap.symbol_name_word(word)?;
    runtime.heap.with_string_slice(spelling, |name| {
        data.canonical
            .then(|| ops::CanonicalSymbol::from_cl_name(name))?
    })?
}

pub(crate) fn with_special_bindings<const N: usize, T>(
    runtime: &Runtime,
    bindings: [(Word, Word); N],
    f: impl FnOnce() -> T,
) -> T {
    let _scope = SpecialBindings::new(&runtime.heap, bindings.into_iter());
    f()
}

fn destructuring_fault(
    runtime: &Runtime,
    value: Word,
    expected: &'static str,
    macro_call: bool,
    error: EvalError,
) -> Fault {
    if macro_call {
        Fault::plain(Class::ProgramError, error)
    } else {
        Fault::type_error(runtime, value, expected, error)
    }
}

pub(crate) struct Values {
    allocator: Rc<RootFrameAllocator>,
    pub(crate) slots: RootFrame,
    capacity: usize,
    pub(crate) len: usize,
}

impl Values {
    pub(crate) fn new(runtime: &Runtime) -> Self {
        Self::empty(runtime.heap.root_frame_allocator())
    }

    fn empty(allocator: Rc<RootFrameAllocator>) -> Self {
        Self {
            allocator,
            slots: RootFrame::empty(),
            capacity: 0,
            len: 0,
        }
    }

    pub(crate) fn take(&mut self) -> Self {
        let empty = Self::empty(Rc::clone(&self.allocator));
        std::mem::replace(self, empty)
    }

    /// Reset the buffer to `len` values. Growing past the capacity allocates a
    /// fresh frame, so every caller must overwrite each position; shrinking
    /// clears the abandoned slots because they are GC roots.
    #[inline]
    pub(crate) fn resize(&mut self, len: usize) {
        if len == self.len {
            return;
        }
        if len > self.capacity {
            self.grow(len);
        } else {
            for i in len..self.len {
                self.slots.set(i, Word::NIL);
            }
        }
        self.len = len;
    }

    #[cold]
    fn grow(&mut self, len: usize) {
        let capacity = len.max(self.capacity.saturating_mul(2)).max(8);
        self.slots = self.allocator.alloc(capacity);
        self.capacity = capacity;
    }

    pub(crate) fn get(&self, i: usize) -> Word {
        if i < self.len {
            self.slots.get(i)
        } else {
            Word::NIL
        }
    }

    pub(crate) fn take_native_return(&mut self) -> NativeReturn {
        if self.len == 1 {
            let word = self.get(0);
            self.resize(0);
            NativeReturn::One(word)
        } else {
            NativeReturn::Values
        }
    }

    fn select(&mut self, frame: &RootSlots, regs: Range<Register>) {
        self.resize(regs.len());
        for (i, reg) in regs.enumerate() {
            self.slots.set(i, frame.get(reg));
        }
    }

    fn bind(&mut self, frame: &RootSlots, regs: Range<Register>) {
        for (i, reg) in regs.enumerate() {
            frame.set(reg, self.get(i));
        }
        self.resize(0);
    }

    pub(crate) fn one(&mut self, word: Word) {
        self.resize(1);
        self.slots.set(0, word);
    }

    pub(crate) fn roots<'rt>(
        &self,
        runtime: &'rt Runtime,
    ) -> impl ExactSizeIterator<Item = Root<'rt>> {
        (0..self.len).map(move |i| runtime.root(self.get(i)))
    }

    fn select_roots(&mut self, roots: &[Root<'_>]) {
        self.resize(roots.len());
        for (index, root) in roots.iter().enumerate() {
            self.slots.set(index, root.slot.get());
        }
    }
}

pub(crate) fn check_definable_name(
    runtime: &Runtime,
    symbol: Word,
    at: usize,
) -> Result<(), Fault> {
    if runtime
        .heap
        .symbol_attributes(symbol)
        .expect("function name symbol")
        .canonical
    {
        Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "cannot redefine a COMMON-LISP operator"),
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn function_cell(runtime: &Runtime, symbol: Word, at: usize) -> Result<Word, Fault> {
    read_symbol_cell(runtime, symbol, true, || at)
}

fn undefined_function(runtime: &Runtime, symbol: Word, at: usize) -> Fault {
    let name = runtime.heap.symbol_name(symbol).unwrap();
    Fault::cell(
        runtime,
        Class::UndefinedFunction,
        symbol,
        EvalError::new(at, format!("undefined function {name}")),
    )
}

pub(crate) fn symbol_value(runtime: &Runtime, symbol: Word, at: usize) -> Result<Word, Fault> {
    read_symbol_cell(runtime, symbol, false, || at)
}

#[inline]
fn read_symbol_cell(
    runtime: &Runtime,
    symbol: Word,
    function: bool,
    at: impl FnOnce() -> usize,
) -> Result<Word, Fault> {
    runtime.heap.symbol_cell(symbol, function).ok_or_else(|| {
        let at = at();
        if function {
            undefined_function(runtime, symbol, at)
        } else {
            let name = runtime.heap.symbol_name(symbol).unwrap();
            Fault::cell(
                runtime,
                Class::UnboundVariable,
                symbol,
                EvalError::new(at, format!("unbound variable {name}")),
            )
        }
    })
}

pub(crate) fn is_function(runtime: &Runtime, value: Word) -> bool {
    Primitive::decode(value).is_some()
        || value.native_index().is_some()
        || runtime.heap.is_callable_object(value)
}

pub(crate) fn is_funcall(word: Word) -> bool {
    matches!(Primitive::decode(word), Some(Primitive::Funcall))
}

pub(crate) fn designator(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    if is_function(runtime, value) {
        Ok(value)
    } else if runtime.heap.is_symbol(value) {
        function_cell(runtime, value, at)
    } else {
        Err(Fault::function_designator(
            runtime,
            value,
            EvalError::new(at, "expected a function or symbol"),
        ))
    }
}

pub(crate) fn run<'rt>(
    runtime: &'rt Runtime,
    target: Word,
    host: &RootSlots,
    args: Range<Register>,
) -> Result<Vec<Root<'rt>>, EvalError> {
    run_values(runtime, target, host, args).map(|values| values.roots(runtime).collect())
}

fn run_values(
    runtime: &Runtime,
    target: Word,
    host: &RootSlots,
    args: Range<Register>,
) -> Result<Values, EvalError> {
    let mut values = Values::new(runtime);
    let initial = if runtime.heap.is_symbol(target) {
        recovery::call(
            runtime,
            target,
            host,
            args,
            &mut values,
            Resume::Call(None),
            0,
        )
    } else {
        designator(runtime, target, 0).and_then(|target| {
            invoke(
                runtime,
                target,
                host,
                args,
                &mut values,
                Resume::Call(None),
                0,
            )
        })
    }
    .map_err(|error| error.host)?;
    execute_values(runtime, values, initial)
}

fn execute<'rt>(
    runtime: &'rt Runtime,
    values: Values,
    initial: Invocation<'rt>,
) -> Result<Vec<Root<'rt>>, EvalError> {
    execute_values(runtime, values, initial).map(|values| values.roots(runtime).collect())
}

fn execute_values<'rt>(
    runtime: &'rt Runtime,
    values: Values,
    initial: Invocation<'rt>,
) -> Result<Values, EvalError> {
    if matches!(initial, Invocation::Done(Resume::Call(None))) {
        // A completed primitive or host function has no frames to suspend.
        // Preserve the final stop check before making its values visible.
        runtime.check_execution_safepoint(0)?;
        return Ok(values);
    }
    if !runtime.has_pinned_rust_frame() && !task_scheduler::in_scheduler(runtime) {
        return task_scheduler::execute_scheduled(runtime, values, initial);
    }
    let _pin = runtime.pin_rust_frame();
    let _class_updates = crate::classes::inline_update_scope(runtime);
    let mut exec = ExecState::new(values);
    let initial_step = accept(runtime, &mut exec, initial)?;
    if let StepControl::Pending(pending) = initial_step
        && let NativeReturn::One(word) = execute_until(runtime, &mut exec, 0, pending)?
    {
        exec.values.one(word);
    }
    Ok(exec.values)
}

impl From<UnwindTarget> for NativeReturn {
    fn from(target: UnwindTarget) -> Self {
        let (target_frame, state) = match target {
            UnwindTarget::Bytecode(frame) => (frame, 0),
            UnwindTarget::Native { frame, state } => (frame, state),
        };
        Self::Unwind {
            target_frame,
            state,
        }
    }
}

pub(crate) fn invoke_and_accept<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
    at: usize,
) -> Result<StepControl<'rt>, EvalError> {
    if runtime.heap.is_symbol(target) && runtime.heap.symbol_cell(target, true).is_none() {
        return accept(
            runtime,
            exec,
            Invocation::Continuation(Box::new(recovery::CellRecovery::call(
                runtime,
                target,
                args,
                range,
                Resume::Call(None),
                at,
            ))),
        );
    }
    let callee = match designator(runtime, target, at) {
        Ok(callee) => callee,
        Err(fault) => {
            return dispatch_fault_from_native(runtime, exec, fault)
                .map(|ret| match ret {
                    NativeReturn::Unwind {
                        target_frame,
                        state,
                    } => {
                        let target = match exec.frames.get(target_frame) {
                            Some(ExecFrame::Native(_)) => UnwindTarget::Native {
                                frame: target_frame,
                                state,
                            },
                            _ => UnwindTarget::Bytecode(target_frame),
                        };
                        StepControl::Unwind(target)
                    }
                    _ => unreachable!("dispatch_fault_from_native only returns Unwind or Err"),
                })
                .map_err(|fault| fault.host);
        }
    };
    let invocation = invoke(
        runtime,
        callee,
        args,
        range,
        &mut exec.values,
        Resume::Call(None),
        at,
    );
    let invocation = match invocation {
        Ok(invocation) => invocation,
        Err(fault) => return dispatch_error(runtime, exec, fault),
    };
    accept(runtime, exec, invocation)
}

enum TailArguments {
    Registers(Range<Register>),
    Values,
}

enum InstructionAction<'rt> {
    Continue,
    BeginCleanup,
    EndCleanup,
    Invoke(Invocation<'rt>),
    TailInvoke(Invocation<'rt>),
    TailCall {
        target: Word,
        args: TailArguments,
        at: usize,
    },
    Transfer(NonLocalTransfer, usize),
}

#[inline(always)]
fn exec_primitive_pair(
    runtime: &Runtime,
    frame: &Frame<'_>,
    site: CallSite<'_>,
    prim: Primitive,
    first: Register,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let slots = &frame.slots;
    let args = first..first + 2;
    if is_binary_numeric(prim) {
        exec_numeric_primitive(runtime, site, prim, slots, args, values, at)
    } else {
        primitive(runtime, slots, prim, args, values, at)
    }
}

#[inline(always)]
fn cached_primitive(site: CallSite<'_>) -> Option<(Primitive, Option<ArithOperands>)> {
    match site.observed {
        SiteFeedback::ArithPrimitive { prim, operands } => Some((prim, Some(operands))),
        SiteFeedback::CallPrimitive { prim } => Some((prim, None)),
        _ => None,
    }
}

#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "VM cached primitive pair dispatch passes frame, pc, operands, and values directly"
)]
fn exec_cached_primitive_pair(
    runtime: &Runtime,
    frame: &Frame<'_>,
    site: CallSite<'_>,
    prim: Primitive,
    previous: Option<ArithOperands>,
    first: Register,
    values: &mut Values,
    at: impl FnOnce() -> usize,
) -> Result<Word, Fault> {
    let slots = &frame.slots;
    let args = first..first + 2;
    if matches!(previous, Some(ArithOperands::Pair(..))) {
        numeric_feedback::profiled_binary_result(runtime, site, prim, previous, slots, args, at)
    } else {
        primitive(runtime, slots, prim, args, values, at())?;
        Ok(values.get(0))
    }
}

#[inline(always)]
fn finish_single_result_call<'rt>(
    frame: &Frame<'rt>,
    invocation: Invocation<'rt>,
    values: &mut Values,
) -> Option<Invocation<'rt>> {
    if let Invocation::Done(Resume::Call(Some(dest))) = invocation {
        values.bind(&frame.slots, dest..dest + 1);
        return None;
    }
    Some(invocation)
}

#[inline(always)]
fn finish_checked_single_result_call<'rt>(
    runtime: &Runtime,
    frame: &Frame<'rt>,
    invocation: Invocation<'rt>,
    values: &mut Values,
) -> Option<Invocation<'rt>> {
    if let Invocation::Done(Resume::Call(Some(dest))) = invocation
        && runtime.host_cancel_request() == crate::execution::HOST_RUNNING
        && runtime.step_budget.get().is_none()
    {
        values.bind(&frame.slots, dest..dest + 1);
        return None;
    }
    Some(invocation)
}

fn call_pair_to_register<'rt>(
    runtime: &'rt Runtime,
    frame: &Frame<'rt>,
    target: Word,
    first: Register,
    out: Register,
    values: &mut Values,
    at: usize,
) -> Result<Option<Invocation<'rt>>, Fault> {
    let invocation = invoke(
        runtime,
        target,
        &frame.slots,
        first..first + 2,
        values,
        Resume::Call(Some(out)),
        at,
    )?;
    Ok(finish_single_result_call(frame, invocation, values))
}

#[cold]
#[inline(never)]
fn slow_arith2<'rt>(
    runtime: &'rt Runtime,
    frame: &Frame<'rt>,
    vm_pc: usize,
    sym: usize,
    first: Register,
    out: Register,
    values: &mut Values,
) -> Result<Option<Invocation<'rt>>, Fault> {
    let at = frame.logical_at(vm_pc);
    let target = function_cell(runtime, frame.constant(runtime, sym), at)?;
    if let Some(prim) = Primitive::decode(target)
        && prim.is_direct_leaf()
        && prim.accepts(2)
    {
        crate::metrics::record_primitive_call();
        let site = CallSite::lookup(runtime, frame, vm_pc, sym);
        if !is_binary_numeric(prim) {
            site.record(SiteFeedback::CallPrimitive { prim });
        }
        exec_primitive_pair(runtime, frame, site, prim, first, values, at)?;
        values.bind(&frame.slots, out..out + 1);
        return Ok(None);
    }
    call_pair_to_register(runtime, frame, target, first, out, values, at)
}

#[derive(Clone, Copy)]
enum UnaryBuiltin {
    Car,
    Cdr,
    Null,
}

impl UnaryBuiltin {
    fn matches_primitive(self, primitive: Primitive) -> bool {
        matches!(
            (self, primitive),
            (Self::Car, Primitive::List(list::Op::Access("CAR")))
                | (Self::Cdr, Primitive::List(list::Op::Access("CDR")))
                | (Self::Null, Primitive::List(list::Op::Null))
        )
    }
}

#[inline(always)]
fn cached_unary_builtin(runtime: &Runtime, frame: &Frame<'_>, vm_pc: usize, sym: usize) -> bool {
    // This site's slow path caches a primitive only after checking its semantics.
    matches!(
        CallSite::lookup(runtime, frame, vm_pc, sym).observed,
        SiteFeedback::CallPrimitive { .. }
    )
}

#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "VM unary builtin slow path forwards runtime, frame, pc, and values directly"
)]
fn slow_unary_builtin<'rt>(
    runtime: &'rt Runtime,
    frame: &Frame<'rt>,
    vm_pc: usize,
    sym: usize,
    src: Register,
    builtin: UnaryBuiltin,
    values: &mut Values,
    resume: Resume,
) -> Result<Invocation<'rt>, Fault> {
    let at = frame.logical_at(vm_pc);
    let target = function_cell(runtime, frame.constant(runtime, sym), at)?;
    if let Some(prim) = Primitive::decode(target)
        && builtin.matches_primitive(prim)
    {
        CallSite::lookup(runtime, frame, vm_pc, sym).record(SiteFeedback::CallPrimitive { prim });
    }
    invoke(
        runtime,
        target,
        &frame.slots,
        src..src + 1,
        values,
        resume,
        at,
    )
}

#[cold]
#[inline(never)]
fn slow_cons<'rt>(
    runtime: &'rt Runtime,
    frame: &Frame<'rt>,
    vm_pc: usize,
    sym: usize,
    first: Register,
    out: Register,
    values: &mut Values,
) -> Result<Option<Invocation<'rt>>, Fault> {
    let at = frame.logical_at(vm_pc);
    let target = function_cell(runtime, frame.constant(runtime, sym), at)?;
    let site = CallSite::lookup(runtime, frame, vm_pc, sym);
    if let Some(prim @ Primitive::List(list::Op::Cons)) = Primitive::decode(target) {
        site.record(SiteFeedback::CallPrimitive { prim });
        let pair = runtime
            .heap
            .cons(|| (frame.slots.get(first), frame.slots.get(first + 1)))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        frame.slots.set(out, pair);
        values.resize(0);
        return Ok(None);
    }
    site.record(SiteFeedback::Megamorphic);
    call_pair_to_register(runtime, frame, target, first, out, values, at)
}

#[cold]
#[inline(never)]
fn slow_cmp2<'rt>(
    runtime: &'rt Runtime,
    frame: &mut Frame<'rt>,
    vm_pc: usize,
    sym: usize,
    first: Register,
    branch: FusedBranch,
    values: &mut Values,
) -> Result<InstructionAction<'rt>, Fault> {
    let at = frame.logical_at(vm_pc);
    let target = function_cell(runtime, frame.constant(runtime, sym), at)?;
    if let Some(op) = Primitive::decode(target)
        && op.is_direct_leaf()
        && op.accepts(2)
    {
        crate::metrics::record_primitive_call();
        let site = CallSite::lookup(runtime, frame, vm_pc, sym);
        if !is_binary_numeric(op) {
            site.record(SiteFeedback::CallPrimitive { prim: op });
        }
        exec_primitive_pair(runtime, frame, site, op, first, values, at)?;
        branch.finish(frame, values.get(0), values);
        return Ok(InstructionAction::Continue);
    }
    let invocation = invoke(
        runtime,
        target,
        &frame.slots,
        first..first + 2,
        values,
        Resume::BranchUnless(branch),
        at,
    )?;
    Ok(InstructionAction::Invoke(invocation))
}

#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "VM comparison-branch fast path forwards operands, target pc, and values directly"
)]
fn exec_cmp_branch<'rt>(
    runtime: &'rt Runtime,
    frame: &mut Frame<'rt>,
    vm_pc: usize,
    kind: ops::CmpKind,
    sym: usize,
    first: Register,
    branch: FusedBranch,
    values: &mut Values,
) -> Result<InstructionAction<'rt>, Fault> {
    let site = CallSite::lookup(runtime, frame, vm_pc, sym);
    let Some((prim, previous)) = cached_primitive(site) else {
        crate::metrics::record_slow_path();
        return slow_cmp2(runtime, frame, vm_pc, sym, first, branch, values);
    };
    let left = frame.slots.get(first);
    let right = frame.slots.get(first + 1);
    let truth = |cond| if cond { Word::T } else { Word::NIL };
    let result = if kind.matches_primitive(prim)
        && let Some(cond) = ops::exec_cmp_branch_fast(kind, left, right)
    {
        if previous.is_some() {
            numeric_feedback::record_fixnums(site, prim, previous);
        }
        truth(cond)
    } else {
        crate::metrics::record_primitive_call();
        if let Some(cond) = numeric_feedback::compare_machine_reals(
            &runtime.heap,
            site,
            prim,
            previous,
            [left, right],
        ) {
            truth(cond)
        } else {
            exec_cached_primitive_pair(runtime, frame, site, prim, previous, first, values, || {
                frame.logical_at(vm_pc)
            })?
        }
    };
    branch.finish(frame, result, values);
    Ok(InstructionAction::Continue)
}

#[cold]
#[inline(never)]
fn exec_slow<'rt>(
    runtime: &'rt Runtime,
    frame: &mut Frame<'rt>,
    frame_index: usize,
    vm_pc: usize,
    canonical_pc: usize,
    values: &mut Values,
    control: &mut ControlStack<'rt>,
) -> Result<InstructionAction<'rt>, Fault> {
    crate::metrics::record_canonical_dispatch();
    let vm_target = |pc: usize| frame.vm_code.vm_target(pc);
    let slots = &frame.slots;
    match frame.vm_code.canonical[canonical_pc] {
        Instruction::Load(out, immediate) => {
            let word = Word::from_immediate(immediate).ok_or_else(|| {
                EvalError::new(
                    frame.call_at,
                    "immediate fixnum is outside the supported range",
                )
            })?;
            slots.set(out, word);
        }
        Instruction::Constant(out, index) => slots.set(out, frame.constant(runtime, index)),
        Instruction::Copy(out, src) => slots.set(out, slots.get(src)),
        Instruction::Clear(ref regs) => frame.clear(regs.clone()),
        Instruction::Jump(pc) => frame.pc = vm_target(pc),
        Instruction::Unless(reg, pc) => {
            if slots.get(reg) == Word::NIL {
                frame.pc = vm_target(pc);
            }
        }
        Instruction::IfPresent(reg, pc) => {
            if slots.get(reg) != Word::UNBOUND {
                frame.pc = vm_target(pc);
            }
        }
        Instruction::UnlessFixnum(reg, pc) => {
            if runtime.target_fixnum_value(slots.get(reg)).is_none() {
                frame.pc = vm_target(pc);
            }
        }
        Instruction::UnlessFixnumInRange(reg, low, high, pc) => {
            if !runtime
                .target_fixnum_value(slots.get(reg))
                .is_some_and(|n| (i64::from(low)..=i64::from(high)).contains(&n))
            {
                frame.pc = vm_target(pc);
            }
        }
        Instruction::TryFixnumArithmetic {
            op,
            symbol,
            out,
            ref args,
        } => {
            let result = try_fixnum_arithmetic(runtime, frame, op, symbol, args.start);
            slots.set(out, result.unwrap_or(Word::NIL));
        }
        Instruction::SetValues(ref regs) => values.select(slots, regs.clone()),
        Instruction::BindValues(ref regs) => values.bind(slots, regs.clone()),
        Instruction::ReadValue(out, index) => slots.set(out, values.get(index)),
        Instruction::SaveValues => control.save_values(frame_index, values),
        Instruction::AppendValues => control.append_values(values),
        Instruction::RestoreValues => control.restore_values(values),
        Instruction::OptionalArgument(index, out) => slots.set(out, frame.arguments.get(index)),
        Instruction::KeywordArgument(index, out) => {
            slots.set(out, frame.arguments.keyword(index, frame.call_at)?)
        }
        Instruction::RestArguments { start, out, at } => {
            frame.arguments.rest(runtime, start, slots, out, at)?;
        }
        Instruction::CheckKeywords { start, table } => {
            frame.check_keywords(runtime, start, table)?
        }
        Instruction::DestructureNext {
            cursor,
            out,
            required,
            macro_call,
            at,
        } => {
            let value = slots.get(cursor);
            if let Some((head, tail)) = runtime.heap.cons_fields(value) {
                slots.set(out, head);
                slots.set(cursor, tail);
            } else if value == Word::NIL && !required {
                slots.set(out, Word::UNBOUND);
            } else {
                let message = if value == Word::NIL {
                    "not enough elements for the destructuring pattern"
                } else {
                    "expected a cons while destructuring"
                };
                return Err(destructuring_fault(
                    runtime,
                    value,
                    "CONS",
                    macro_call,
                    EvalError::new(at, message),
                ));
            }
        }
        Instruction::DestructureEnd {
            cursor,
            macro_call,
            at,
        } => {
            let value = slots.get(cursor);
            if value != Word::NIL {
                return Err(destructuring_fault(
                    runtime,
                    value,
                    "NULL",
                    macro_call,
                    EvalError::new(at, "unexpected tail after destructuring"),
                ));
            }
        }
        Instruction::DestructureKeywords {
            cursor,
            table,
            ref out,
            macro_call,
            at,
        } => {
            let value = slots.get(cursor);
            let arguments = proper_list(runtime, value, at)
                .map_err(|error| destructuring_fault(runtime, value, "LIST", macro_call, error))?;
            let descriptor = &frame.vm_code.keywords[table];
            let names = frame.keyword_names(runtime, descriptor);
            let matches = keyword_arguments(
                runtime,
                &names,
                0..arguments.len(),
                |index| arguments[index],
                descriptor.allow_other_keys,
                frame.constant(runtime, descriptor.override_key),
            )
            .map_err(|error| {
                destructuring_fault(runtime, value, "LIST", macro_call, error.at(at))
            })?;
            for (reg, argument) in out.clone().zip(matches) {
                slots.set(
                    reg,
                    argument.map_or(Word::UNBOUND, |index| arguments[index]),
                );
            }
        }
        Instruction::ReleaseArguments => frame.arguments = ArgumentTail::default(),
        Instruction::CallValues { callee, at } => {
            let target = designator(runtime, slots.get(callee), at)?;
            let argc = values.len;
            let arguments = runtime.staging_stack.alloc(argc);
            for i in 0..argc {
                arguments.set(i, values.get(i));
            }
            values.resize(0);
            return Ok(InstructionAction::Invoke(invoke(
                runtime,
                target,
                &arguments,
                0..argc,
                values,
                Resume::Call(None),
                at,
            )?));
        }
        Instruction::ReadCell {
            out,
            symbol,
            function,
            at,
        } => {
            let symbol = frame.constant(runtime, symbol);
            let value = read_symbol_cell(runtime, symbol, function, || at)?;
            slots.set(out, value);
        }
        Instruction::WriteCell { symbol, src } => {
            let symbol = frame.constant(runtime, symbol);
            runtime.heap.set_symbol_cell(symbol, false, slots.get(src));
        }
        Instruction::DefineFunction {
            symbol,
            definition,
            is_macro,
            at,
        } => {
            let symbol = runtime.root_slot(frame.constant(runtime, symbol));
            let definition = runtime.root_slot(slots.get(definition));
            check_definable_name(runtime, symbol.get(), at)?;
            if is_macro {
                runtime
                    .heap
                    .set_macro_expander(symbol.get(), definition.get());
            } else {
                let function = environment::reinstall_trace(
                    runtime,
                    symbol.get(),
                    false,
                    definition.get(),
                    at,
                )?;
                runtime.heap.set_symbol_cell(symbol.get(), true, function);
            }
        }
        Instruction::ProclaimSpecial { symbol, at } => {
            runtime.proclaim_special(frame.constant(runtime, symbol), at)?;
        }
        Instruction::ProclaimInline {
            symbol,
            setf,
            notinline,
        } => {
            let word = frame.constant(runtime, symbol);
            let declaration = if notinline {
                crate::core_ir::InlineDeclaration::NotInline
            } else {
                crate::core_ir::InlineDeclaration::Inline
            };
            runtime.proclaim_inline(word, setf, declaration);
        }
        Instruction::SetVariableDocumentation { symbol, doc } => {
            runtime.heap.set_variable_documentation(
                frame.constant(runtime, symbol),
                frame.constant(runtime, doc),
            );
        }
        Instruction::WhenBound { symbol, jump } => {
            if runtime
                .heap
                .symbol_cell(frame.constant(runtime, symbol), false)
                .is_some()
            {
                frame.pc = vm_target(jump);
            }
        }
        Instruction::BindSpecial { symbol, value } => {
            frame.bind_special(runtime, control, frame_index, symbol, value);
        }
        Instruction::BindProgv {
            symbols,
            values,
            at,
        } => {
            control.bind_progv(
                runtime,
                frame_index,
                slots.get(symbols),
                slots.get(values),
                at,
            )?;
        }
        Instruction::Unbind(count) => control.unbind(count),
        Instruction::BoxLocal { reg, at } => {
            frame
                .box_local(runtime, reg)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
        }
        Instruction::ReadBox { out, cell } => slots.set(out, frame.read_box(runtime, cell)?),
        Instruction::WriteBox { cell, src } => {
            let cell = frame.cell(runtime, cell)?;
            runtime
                .heap
                .write_cell(cell, slots.get(src))
                .ok_or_else(|| frame.invalid_operand("expected a lexical cell"))?;
        }
        Instruction::Capture(out, i) => {
            slots.set(out, frame.cell(runtime, CellLocation::Capture(i))?);
        }
        Instruction::Closure {
            out,
            code,
            ref cells,
            at,
        } => {
            let closure = frame
                .make_closure(runtime, code, cells.start, cells.len())
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            slots.set(out, closure);
        }
        Instruction::ExitToken(reg, at) => {
            let id = runtime.next_exit.get();
            let token = Word::exit_token(id)
                .ok_or_else(|| EvalError::new(at, "too many control targets"))?;
            runtime.next_exit.set(id + 1);
            slots.set(reg, token);
        }
        Instruction::EnterBlock {
            token,
            resume,
            clear,
        } => {
            let token = slots.get(token);
            if !token.is_exit_token() {
                return Err(frame.invalid_operand("expected a BLOCK token"));
            }
            control.enter_exit_point(
                frame_index,
                ExitPoint::Block(token),
                vm_target(resume),
                clear,
            );
        }
        Instruction::EnterCatch { tag, resume, clear } => {
            control.enter_exit_point(frame_index, ExitPoint::Catch(tag), vm_target(resume), clear);
        }
        Instruction::EnterTagbody {
            token,
            jumps,
            clear,
        } => {
            let token = slots.get(token);
            if !token.is_exit_token() {
                return Err(frame.invalid_operand("expected a TAGBODY token"));
            }
            let count = frame.vm_code.jump_tables[jumps].targets.len();
            control.enter_exit_point(frame_index, ExitPoint::Tagbody(token, count), jumps, clear);
        }
        Instruction::LeaveTarget => control.leave_exit_point(),
        Instruction::EnterProtect { cleanup, clear } => {
            control.protect(runtime, frame_index, vm_target(cleanup), clear)
        }
        Instruction::EnterHandlers { table } => control.enter_bytecode_handlers(frame_index, table),
        Instruction::LeaveHandlers => control.leave_handlers(),
        Instruction::EnterRestarts { table, at } => {
            for spec in &frame.vm_code.restarts[table].restarts {
                let object = runtime
                    .heap
                    .restart(|| frame.constant(runtime, spec.name))
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
                if let Some(report) = spec.report {
                    let report = slots.get(report);
                    runtime.heap.set_restart_report(object, report);
                }
                slots.set(spec.object, object);
            }
            control.enter_restarts(frame_index, table);
        }
        Instruction::LeaveRestarts => control.leave_restarts(),
        Instruction::AssociateRestarts {
            condition,
            list,
            at,
        } => {
            let condition = slots.get(condition);
            if !condition::is_condition(runtime, condition) {
                return Err(Fault::type_error(
                    runtime,
                    condition,
                    "CONDITION",
                    EvalError::new(at, "expected a condition object"),
                ));
            }
            let restarts = proper_list(runtime, slots.get(list), at)
                .map_err(|error| Fault::type_error(runtime, slots.get(list), "LIST", error))?;
            if let Some(&word) = restarts
                .iter()
                .find(|&&word| runtime.heap.restart_name(word).is_none())
            {
                return Err(Fault::type_error(
                    runtime,
                    word,
                    "RESTART",
                    EvalError::new(at, "expected a list of restart objects"),
                ));
            }
            control.associate_restarts(runtime, frame_index, condition, &restarts);
        }
        Instruction::LeaveRestartAssociation => control.leave_restart_association(),
        Instruction::BeginCleanup => return Ok(InstructionAction::BeginCleanup),
        Instruction::EndCleanup => return Ok(InstructionAction::EndCleanup),
        Instruction::ReturnFrom { token, at } => {
            return Ok(InstructionAction::Transfer(
                NonLocalTransfer::ReturnFrom(slots.get(token)),
                at,
            ));
        }
        Instruction::Throw { tag, at } => {
            return Ok(InstructionAction::Transfer(
                NonLocalTransfer::Throw(slots.get(tag)),
                at,
            ));
        }
        Instruction::Go { token, label, at } => {
            return Ok(InstructionAction::Transfer(
                NonLocalTransfer::Go(slots.get(token), label),
                at,
            ));
        }
        Instruction::Call {
            callee,
            out,
            ref args,
            at,
            associate,
            notinline,
        } => {
            let args = args.clone();
            let target = match callee {
                Callee::Named(index) => function_cell(runtime, frame.constant(runtime, index), at)?,
                Callee::Value(reg) => designator(runtime, slots.get(reg), at)?,
            };
            if let Callee::Named(symbol) = callee
                && args.len() == 2
                && let Some(prim) = Primitive::decode(target)
            {
                match prim {
                    Primitive::Array(arrays::Op::Aref) => {
                        array_access::exec_named_aref(
                            runtime,
                            frame,
                            CallSite::lookup(runtime, frame, vm_pc, symbol),
                            args.start,
                            values,
                            || at,
                        )?;
                        return Ok(InstructionAction::Invoke(Invocation::Done(Resume::Call(
                            out,
                        ))));
                    }
                    prim if is_binary_numeric(prim) => {
                        crate::metrics::record_primitive_call();
                        exec_numeric_primitive(
                            runtime,
                            CallSite::lookup(runtime, frame, vm_pc, symbol),
                            prim,
                            slots,
                            args,
                            values,
                            at,
                        )?;
                        return Ok(InstructionAction::Invoke(Invocation::Done(Resume::Call(
                            out,
                        ))));
                    }
                    prim @ Primitive::Simd(_) => {
                        let site = CallSite::lookup(runtime, frame, vm_pc, symbol);
                        call_site::observe_i32x4(runtime, slots, site, prim, args.start);
                    }
                    _ => {}
                }
            }
            if !associate
                && !notinline
                && args.len() == 4
                && let Callee::Named(symbol) = callee
                && frame.vm_code.canonical_symbols[symbol] == Some(ops::CanonicalSymbol::Funcall)
                && is_funcall(target)
                && array_write::try_setf_aref(
                    runtime,
                    frame,
                    CallSite::lookup(runtime, frame, vm_pc, symbol),
                    args.start,
                    values,
                )
            {
                return Ok(InstructionAction::Invoke(Invocation::Done(Resume::Call(
                    out,
                ))));
            }
            let mut invocation =
                invoke(runtime, target, slots, args, values, Resume::Call(out), at)?;
            if associate && let Some(group) = control.innermost_restart_cluster() {
                invocation.associate_restart(group);
            }
            return Ok(InstructionAction::Invoke(invocation));
        }
    }
    Ok(InstructionAction::Continue)
}

fn try_fixnum_arithmetic(
    runtime: &Runtime,
    frame: &Frame<'_>,
    op: zkcl_bytecode::FixnumArithmetic,
    symbol: usize,
    start: Register,
) -> Option<Word> {
    use zkcl_bytecode::FixnumArithmetic::{Add, Dec, Inc, Mul, Sub};
    let actual = runtime
        .heap
        .symbol_cell(frame.constant(runtime, symbol), true)?;
    if actual != Primitive::guarded_binding(usize::from(op.binding_index()))? {
        return None;
    }
    let lhs = runtime.target_fixnum_value(frame.slots.get(start))?;
    let rhs = || runtime.target_fixnum_value(frame.slots.get(start + 1));
    let result = match op {
        Inc => lhs.checked_add(1),
        Dec => lhs.checked_sub(1),
        Add => lhs.checked_add(rhs()?),
        Sub => lhs.checked_sub(rhs()?),
        Mul => lhs.checked_mul(rhs()?),
    }?;
    if !runtime.native_word_width() {
        let (low, high) = runtime.compilation_target().fixnum_bounds();
        if !(low..=high).contains(&result) {
            return None;
        }
    }
    Word::fixnum(result.try_into().ok()?)
}

fn run_native_invocation<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    invocation: NativeInvocation<'rt>,
) -> Result<NativeOutcome, Fault> {
    let _shield = inherit_host_cancel_shield(runtime, exec.control.defers_host_cancellation());
    let entry = invocation.executable.entry;
    let argc = invocation.argc;
    let mut ctx = NativeContext::new(runtime, NonNull::from(exec));
    ctx.code = runtime.heap.code_store.find(invocation.executable.code_id);
    match invocation.args {
        NativeCallArgs::Osr => {
            let ExecFrame::Bytecode(mut frame) = ctx.exec_mut().frames.pop().unwrap() else {
                unreachable!("OSR replaces the current bytecode frame");
            };
            debug_assert_eq!(ctx.exec_mut().control.len(), frame.control_base);
            let osr = OsrActivation {
                call_at: frame.call_at,
                profile: frame.profile.take(),
            };
            let entry = invocation
                .executable
                .c_entry
                .expect("OSR is installed as a C entry");
            invoke_osr_entry(
                &mut ctx,
                entry,
                frame.slots.get(frame.closure),
                &frame.slots,
                argc,
                osr,
            )
        }
        NativeCallArgs::Fast { target, args, len } => {
            let _pin = runtime.pin_rust_frame();
            let fast = entry
                .fast_entry
                .expect("Fast args constructed only when fast_entry is Some");
            fast(runtime, &mut ctx, target, &args[..len as usize]).map(NativeOutcome::Returned)
        }
        NativeCallArgs::Slice(slice) => {
            let target = slice.get(0);
            if let Some(c_entry) = invocation.executable.c_entry {
                invoke_c_entry(
                    &mut ctx,
                    c_entry,
                    invocation.executable.resumable,
                    target,
                    &slice,
                    1..1 + argc,
                )
            } else {
                let _pin = runtime.pin_rust_frame();
                (entry.generic_entry)(runtime, &mut ctx, target, &slice, 1..1 + argc)
                    .map(NativeOutcome::Returned)
            }
        }
    }
}

fn inherit_host_cancel_shield(runtime: &Runtime, shielded: bool) -> impl Drop + use<'_> {
    let previous = shielded.then(|| task_scheduler::set_host_cancel_shield(runtime, true));
    std::mem::DropGuard::new(previous, |previous| {
        if let Some(previous) = previous {
            task_scheduler::set_host_cancel_shield(runtime, previous);
        }
    })
}

pub(in crate::vm) fn accept<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    invocation: Invocation<'rt>,
) -> Result<StepControl<'rt>, EvalError> {
    accept_with_pc(runtime, exec, invocation, None)
}

fn accept_with_pc<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut invocation: Invocation<'rt>,
    mut top_vm_pc: Option<usize>,
) -> Result<StepControl<'rt>, EvalError> {
    if exec.frames.is_empty() && exec.control.len() == 0 {
        recovery::enter_toplevel(runtime, exec)?;
    }
    while let Invocation::Native(native_inv) = invocation {
        let resume = native_inv.resume;
        let frame_base =
            exec.frames.len() - usize::from(matches!(native_inv.args, NativeCallArgs::Osr));
        match run_native_invocation(runtime, exec, native_inv) {
            Ok(NativeOutcome::ResumeVm) => {
                let ExecFrame::Bytecode(frame) = &mut exec.frames[frame_base] else {
                    unreachable!("handoff preserves the outermost activation")
                };
                frame.resume = resume;
                if let Some(breakpoint) = exec.native_debug_stop.take() {
                    let ExecFrame::Bytecode(stopped) = exec.frames.last_mut().unwrap() else {
                        unreachable!("the stopped activation has recovered its VM state")
                    };
                    let pc = stopped.pc;
                    stopped.skip_debug_pc = Some(pc);
                    return signaling::pause_recovered_instruction(runtime, exec, pc, breakpoint);
                }
                let pending = exec.native_tail.take().map(|(call, resume)| {
                    PendingContinuation(Pending::StartCall {
                        args: call.slots,
                        resume: resume.expect("native caller supplies the tail continuation"),
                        at: call.at,
                    })
                });
                return Ok(StepControl::Pending(pending));
            }
            Ok(NativeOutcome::TailCall(call)) => {
                // Native code has returned. The next call inherits the same
                // continuation, even when it crosses back into bytecode.
                if runtime.native_slice_steps.get() == 0
                    && task_scheduler::in_scheduler(runtime)
                    && !runtime.has_pinned_rust_frame()
                {
                    return Ok(StepControl::pending(Pending::StartCall {
                        args: call.slots,
                        resume,
                        at: call.at,
                    }));
                }
                top_vm_pc = None;
                let next = runtime
                    .check_execution_safepoint(call.at)
                    .map_err(Fault::from)
                    .and_then(|()| designator(runtime, call.slots.get(0), call.at))
                    .and_then(|target| {
                        invoke(
                            runtime,
                            target,
                            &call.slots,
                            1..call.slots.len(),
                            &mut exec.values,
                            resume,
                            call.at,
                        )
                    });
                match next {
                    Ok(next) => invocation = next,
                    Err(fault) => return dispatch_error(runtime, exec, fault),
                }
            }
            Ok(NativeOutcome::Returned(NativeReturn::Unwind {
                target_frame,
                state,
            })) => {
                let target = match exec.frames.get(target_frame) {
                    Some(ExecFrame::Native(_)) => UnwindTarget::Native {
                        frame: target_frame,
                        state,
                    },
                    _ => UnwindTarget::Bytecode(target_frame),
                };
                return Ok(StepControl::Unwind(target));
            }
            Ok(NativeOutcome::Returned(ret)) => {
                if let NativeReturn::One(word) = ret {
                    exec.values.one(word);
                }
                return Ok(StepControl::Pending(finish(
                    resume,
                    &mut exec.frames,
                    &mut exec.values,
                    &mut exec.control,
                )));
            }
            Err(fault) => return dispatch_error_with_pc(runtime, exec, fault, top_vm_pc),
        }
    }
    match invocation {
        Invocation::StartCall(args) => Ok(StepControl::pending(Pending::StartCall {
            args,
            resume: Resume::Call(None),
            at: 0,
        })),
        Invocation::Done(resume) => Ok(StepControl::Pending(finish(
            resume,
            &mut exec.frames,
            &mut exec.values,
            &mut exec.control,
        ))),
        Invocation::Lisp(mut frame) => {
            frame.control_base = exec.control.len();
            exec.frames.push(ExecFrame::Bytecode(frame));
            Ok(StepControl::Pending(None))
        }
        Invocation::Native(_) => unreachable!("native invocations are driven above"),
        Invocation::TaskCall(call) => Ok(StepControl::pending(Pending::TaskCall(call))),
        Invocation::Continuation(mut continuation) => {
            let frame_index = exec.frames.len().saturating_sub(1);
            if let Err(error) = continuation.on_start(runtime, frame_index, &mut exec.control) {
                return dispatch_error(runtime, exec, error);
            }
            Ok(StepControl::pending(Pending::Continuation {
                continuation,
                returned: false,
            }))
        }
        Invocation::CorrectableError(error) => Ok(StepControl::pending(Pending::Continuation {
            continuation: error,
            returned: false,
        })),
        Invocation::SimpleRestart {
            restart,
            body,
            resume,
            at,
        } => restart::call_with_simple_restart(runtime, exec, restart, body, resume, at),
        Invocation::Warn(warning) => Ok(StepControl::pending(Pending::Continuation {
            continuation: warning,
            returned: false,
        })),
        Invocation::ConstructCondition(construction) => {
            construction.dispatch(runtime, exec, top_vm_pc)
        }
        Invocation::InvokeDebugger(mut signal) => {
            signal.vm_pc = top_vm_pc;
            signaling::enter_debugger(runtime, exec, HandlerSearch::for_debugger(signal))
        }
        Invocation::Signal(mut signal) => {
            signal.vm_pc = top_vm_pc;
            let search = exec.control.search_handlers(signal);
            if top_vm_pc.is_some() {
                // A bytecode call can start dispatch here. Calls from Lisp callbacks
                // still trampoline so nested primitive handlers cannot recurse in Rust.
                match dispatch::poll(runtime, exec, top_vm_pc)? {
                    Some(step) => Ok(step),
                    None => dispatch_signal(runtime, exec, search),
                }
            } else {
                Ok(StepControl::pending(Pending::Signal(search)))
            }
        }
        Invocation::Macroexpand(expansion) => {
            Ok(StepControl::pending(Pending::Macroexpand(expansion)))
        }
        Invocation::Compile(definition) => Ok(StepControl::pending(Pending::Compile(definition))),
        Invocation::Evaluate(evaluation) => Ok(StepControl::pending(Pending::Evaluate(evaluation))),
        Invocation::Initialization(initialization) => Ok(StepControl::pending(Pending::Instance {
            operation: instances::Operation::Initialize(initialization),
            returned: false,
        })),
        Invocation::AccessInstance(access) => Ok(StepControl::pending(Pending::Instance {
            operation: instances::Operation::Access(access),
            returned: false,
        })),
        Invocation::Restart(request) => {
            match RestartSelection::new(runtime, &exec.control, request) {
                Ok(selection) => Ok(StepControl::pending(Pending::Restart(selection))),
                Err(error) => dispatch_error(runtime, exec, error),
            }
        }
    }
}

pub(in crate::vm) fn finish<'rt>(
    resume: Resume,
    frames: &mut [ExecFrame<'_>],
    values: &mut Values,
    control: &mut ControlStack<'rt>,
) -> Option<PendingContinuation<'rt>> {
    let p = match resume {
        Resume::Call(Some(out)) => {
            values.bind(frames.last().unwrap().slots(), out..out + 1);
            return None;
        }
        Resume::Call(None) | Resume::Deoptimized => return None,
        Resume::BranchUnless(branch) => {
            let Some(ExecFrame::Bytecode(frame)) = frames.last_mut() else {
                unreachable!("fused branch resumes its bytecode caller");
            };
            branch.finish(frame, values.get(0), values);
            return None;
        }
        Resume::Continuation => Pending::Continuation {
            continuation: control.resume_continuation(),
            returned: true,
        },
        Resume::SignalHandler => Pending::Signal(control.resume_signal()),
        Resume::RestartTest => {
            Pending::Restart(control.resume_restart_selection(values.get(0) != Word::NIL))
        }
        Resume::RestartInteractive => {
            Pending::RestartInteractive(control.resume_interactive_restart())
        }
        Resume::AbortRestart(at) => Pending::AbortRestartFailed(at),
        Resume::Evaluate => Pending::Evaluate(control.resume_evaluation()),
        Resume::Instance => Pending::Instance {
            operation: control.resume_instance(),
            returned: true,
        },
        Resume::Macroexpander => {
            let mut expansion = control.resume_macroexpansion();
            expansion.form.set(0, values.get(0));
            expansion.steps += 1;
            Pending::Macroexpand(expansion)
        }
    };
    Some(PendingContinuation(p))
}

fn dispatch_continuation<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut continuation: Box<dyn Continuation + 'rt>,
    returned: bool,
) -> Result<StepControl<'rt>, EvalError> {
    let step = match continuation.step_in_vm(runtime, exec, returned) {
        callback::Outcome::Completed(step) => step,
        callback::Outcome::Unwind(target) => return Ok(StepControl::Unwind(target)),
        callback::Outcome::Unhandled(error) => return Err(error),
    };
    match step {
        Ok(ContinuationStep::Done(result)) => {
            continuation.on_done(&mut exec.control);
            exec.values.one(result);
            Ok(StepControl::Pending(finish(
                continuation.resume_to(),
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            )))
        }
        Ok(ContinuationStep::DoneValues(results)) => {
            continuation.on_done(&mut exec.control);
            exec.values.resize(results.len());
            for (index, word) in results.into_iter().enumerate() {
                exec.values.slots.set(index, word);
            }
            Ok(StepControl::Pending(finish(
                continuation.resume_to(),
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            )))
        }
        Ok(ContinuationStep::KeepValues) => {
            continuation.on_done(&mut exec.control);
            Ok(StepControl::Pending(finish(
                continuation.resume_to(),
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            )))
        }
        Ok(ContinuationStep::Call { target, args, at }) => {
            let saved_frames = exec.frames.len();
            let frame_index = saved_frames.saturating_sub(1);
            let continuation_scope = continuation.continuation_scope();
            exec.control.pause_continuation(frame_index, continuation);
            if let Some(scope) = continuation_scope {
                exec.control.push_continuation_scope(saved_frames, scope);
            }
            match invoke(
                runtime,
                target,
                &args,
                0..args.len(),
                &mut exec.values,
                Resume::Continuation,
                at,
            ) {
                Ok(invocation) => accept(runtime, exec, invocation),
                Err(error) => dispatch_error(runtime, exec, error),
            }
        }
        Ok(ContinuationStep::Signal {
            condition,
            at,
            unhandled,
        }) => start_restartable_signal(
            runtime,
            exec,
            continuation,
            Signal::new(runtime, condition, unhandled, at, Resume::Continuation),
            "CONTINUE",
            None,
            None,
        ),
        Ok(ContinuationStep::CellError { fault }) => {
            recovery::start(runtime, exec, continuation, fault)
        }
        Ok(ContinuationStep::CorrectableError {
            condition,
            report,
            enclosing,
            at,
        }) => start_restartable_signal(
            runtime,
            exec,
            continuation,
            Signal::from_lisp(runtime, condition, true, at, Resume::Continuation),
            "CONTINUE",
            Some(report),
            enclosing,
        ),
        Ok(ContinuationStep::Warning {
            condition,
            enclosing,
            at,
        }) => start_restartable_signal(
            runtime,
            exec,
            continuation,
            Signal::new(runtime, condition, None, at, Resume::Continuation),
            "MUFFLE-WARNING",
            None,
            enclosing,
        ),
        Err(error) => dispatch_error(runtime, exec, error),
    }
}

fn dispatch_macroexpand<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    expansion: Macroexpansion,
) -> Result<StepControl<'rt>, EvalError> {
    let at = expansion.at;
    let form = runtime.root(expansion.form.get(0));
    let call = if expansion.repeat || expansion.steps == 0 {
        match macros::prepare(runtime, &form, expansion.environment.get(), at) {
            Ok(call) => call,
            Err(error) => return dispatch_error(runtime, exec, error.into()),
        }
    } else {
        None
    };
    let Some(call) = call else {
        exec.values.resize(2);
        exec.values.slots.set(0, form.slot.get());
        exec.values.slots.set(
            1,
            if expansion.steps == 0 {
                Word::NIL
            } else {
                Word::T
            },
        );
        return Ok(StepControl::Pending(finish(
            expansion.resume,
            &mut exec.frames,
            &mut exec.values,
            &mut exec.control,
        )));
    };
    if expansion.steps >= macros::EXPANSION_LIMIT {
        let error = EvalError::macro_expansion(at, "macro expansion limit exceeded");
        return dispatch_error(runtime, exec, error.into());
    }
    let arguments = runtime.staging_stack.alloc(3);
    arguments.set(0, call.expander.slot.get());
    arguments.set(1, form.slot.get());
    arguments.set(2, expansion.environment.get());
    exec.control
        .pause_macroexpansion(exec.frames.len().saturating_sub(1), expansion);
    let invoked = designator(runtime, call.hook.slot.get(), at).and_then(|target| {
        invoke(
            runtime,
            target,
            &arguments,
            0..3,
            &mut exec.values,
            Resume::Macroexpander,
            at,
        )
    });
    match invoked {
        Ok(invocation) => accept(runtime, exec, invocation),
        Err(error) => dispatch_error(runtime, exec, error),
    }
}

pub(crate) fn dispatch_error<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    error: Fault,
) -> Result<StepControl<'rt>, EvalError> {
    dispatch_error_with_pc(runtime, exec, error, None)
}

#[cold]
fn unwind_execution_stop(
    exec: &mut ExecState<'_>,
    mut error: EvalError,
    top_vm_pc: Option<usize>,
) -> Result<UnwindTarget, EvalError> {
    capture_error_stack(&exec.frames, &mut error, top_vm_pc);
    if error.execution_stop() != Some(crate::ExecutionStop::Cancelled) {
        return Err(error);
    }
    exec.control
        .error(&mut exec.frames, &mut exec.values, error, None, false)
}

fn dispatch_error_with_pc<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut error: Fault,
    top_vm_pc: Option<usize>,
) -> Result<StepControl<'rt>, EvalError> {
    if error.already_dispatched() {
        return Err(error.host);
    }
    if let Some(pc) = top_vm_pc
        && let Some(recovery) = recovery::for_instruction(runtime, exec, &error, pc)
    {
        return dispatch_continuation(runtime, exec, Box::new(recovery), false);
    }
    if error.host.kind() == EvalErrorKind::Cancelled {
        return unwind_execution_stop(exec, error.host, top_vm_pc).map(StepControl::Unwind);
    }
    let debug = (exec.host_debugging && !runtime.has_pinned_rust_frame())
        || runtime
            .debugger
            .get()
            .is_some_and(|debugger| debugger.observes_faults(runtime));
    let may_defer = matches!(error.host.kind(), EvalErrorKind::LispCondition(_))
        && exec.control.can_defer_expansion_error();
    if (exec.control.has_handlers() || debug || may_defer)
        && let Some(word) = error.make_condition(runtime)
    {
        let at = error.host.offset();
        let mut signal = Signal::new(runtime, word, Some(error.host), at, Resume::Call(None));
        // A declining handler returns to these same frames. Keep the original
        // faulting PC and capture the host backtrace only when unwinding out.
        signal.vm_pc = top_vm_pc;
        let search = exec.control.search_handlers(signal);
        return Ok(StepControl::pending(Pending::Signal(search)));
    }
    let cond_slot = if task_scheduler::retains_task_result(runtime) {
        error
            .make_condition(runtime)
            .map(|word| runtime.root_slot(word))
    } else {
        None
    };
    capture_error_stack(&exec.frames, &mut error.host, top_vm_pc);
    let target = exec.control.error(
        &mut exec.frames,
        &mut exec.values,
        error.host,
        cond_slot,
        false,
    )?;
    Ok(StepControl::Unwind(target))
}

pub(crate) fn install_host_function(
    runtime: &Runtime,
    source: &str,
    callback: Arc<crate::NativeFunction>,
) -> Result<(), EvalError> {
    install_registered_function(runtime, source, crate::RegisteredFunction::Host(callback))
}

pub(crate) fn install_registered_function(
    runtime: &Runtime,
    source: &str,
    callback: crate::RegisteredFunction,
) -> Result<(), EvalError> {
    let source = source.trim();
    let (name, at) = if source.starts_with('(') {
        let form = crate::read::read(runtime, source)?;
        let at = form.at;
        let value = crate::form_value::to_value(runtime, &form)?;
        let name =
            FunctionName::parse(runtime, value.slot.get(), at).map_err(|fault| fault.host)?;
        (name, at)
    } else {
        let (name, at) = crate::read::read_symbol_name(runtime, source)?;
        (FunctionName::Symbol(runtime.intern(&name, at)?), at)
    };
    name.check_definable(runtime, at)
        .map_err(|fault| fault.host)?;

    // Reading a name can run Lisp (including another registration). Add the
    // callback only after that work succeeds; registry indices never move.
    let mut natives = runtime.natives.borrow_mut();
    let function = Word::native(natives.len());
    natives.0.push(callback);
    drop(natives);
    name.install(runtime, function, at)
        .map_err(|fault| fault.host)?;
    Ok(())
}

pub(crate) fn deopt_into_vm<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    target: Word,
    canon_pc: usize,
    regs: &[u32],
    vals: &[usize],
) -> Result<NativeReturn, Fault> {
    let mut frame = deopt_frame(runtime, target, canon_pc, regs, vals)?;
    if let Some(ExecFrame::Native(anchor)) = exec.frames.last_mut()
        && anchor
            .code
            .as_ref()
            .is_some_and(|code| code.id == frame.vm_code.id)
    {
        frame.profile = anchor.profile.take();
    }
    let base_len = exec.frames.len();
    frame.control_base = exec.control.len();
    exec.frames.push(ExecFrame::Bytecode(frame));
    execute_until(runtime, exec, base_len, None).map_err(Fault::dispatched)
}

fn deopt_frame<'rt>(
    runtime: &'rt Runtime,
    target: Word,
    canon_pc: usize,
    regs: &[u32],
    vals: &[usize],
) -> Result<Frame<'rt>, Fault> {
    let code = runtime.heap.function_code(target).ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(0, "deopt called on non-function"),
        )
    })?;
    let info = runtime.heap.function_info(code).ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(0, "deopt missing FunctionFields"),
        )
    })?;
    let vm_code = runtime.heap.code_store.get(info.code_id);
    Frame::new_deopt(
        runtime,
        target,
        code,
        vm_code,
        canon_pc,
        regs,
        vals,
        Resume::Deoptimized,
        0,
    )
}

pub(crate) fn recover_native_frame<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    target: Word,
    canon_pc: usize,
    regs: &[u32],
    vals: &[usize],
    after_call: bool,
) -> Result<usize, Fault> {
    let mut frame = deopt_frame(runtime, target, canon_pc, regs, vals)?;
    let entry = exec
        .jit_frames
        .last()
        .expect("recovery belongs to an active compiled frame");
    let index = entry.frame_index;
    frame.control_base = entry.control_base;
    let ExecFrame::Native(anchor) = &mut exec.frames[index] else {
        unreachable!("each native activation is recovered once");
    };
    exec.control.recover_native_scopes(entry, anchor, &frame);
    if let Some(call_at) = entry.osr_call_at {
        frame.call_at = call_at;
    }
    frame.arguments = std::mem::take(&mut anchor.arguments);
    frame.debug_arguments = anchor.debug_arguments.take();
    frame.profile = anchor.profile.take();
    if after_call {
        let resume = ops::resume_native_call(&frame.vm_code, canon_pc);
        let call_pc = frame.pc;
        frame.pc = frame.vm_code.vm_target(canon_pc + 1);
        if let Some(ExecFrame::Bytecode(callee)) = exec.frames.get_mut(index + 1) {
            callee.resume = resume;
            callee.call_at = frame.logical_at(call_pc);
        } else {
            let (_, continuation) = exec.native_tail.as_mut().expect("a call is in progress");
            *continuation = Some(resume);
        }
    } else if let Some(&Instruction::Unless(register, target)) =
        frame.vm_code.canonical.get(canon_pc)
        && frame.vm_code.canonical_pc(frame.pc) != Some(canon_pc)
    {
        // A generated VM comparison may fuse this branch with the preceding
        // call. The native result is already in the register. Replaying the
        // whole fused instruction would call Lisp twice.
        let target = if frame.slots.get(register) == Word::NIL {
            target
        } else {
            canon_pc + 1
        };
        frame.pc = frame.vm_code.vm_target(target);
    }
    exec.frames[index] = ExecFrame::Bytecode(frame);
    Ok(index)
}

pub(crate) fn dispatch_fault_from_native<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    fault: Fault,
) -> Result<NativeReturn, Fault> {
    if fault.host.source_document().is_none()
        && let Some(frame) = exec.frames.last_mut()
    {
        frame.record_native_offset(fault.host.offset());
    }
    let base_len = exec.frames.len();
    match dispatch_error(runtime, exec, fault).map_err(Fault::dispatched)? {
        StepControl::Unwind(target) => Ok(target.into()),
        StepControl::Pending(None) => Ok(exec.values.take_native_return()),
        StepControl::Pending(Some(pending)) => {
            execute_until(runtime, exec, base_len, Some(pending)).map_err(Fault::dispatched)
        }
        StepControl::QuantumYield | StepControl::Suspend { .. } => {
            unreachable!("synchronous native fault dispatch cannot suspend")
        }
    }
}

/// Resolve a read-only call that can complete without starting a continuation.
/// A fault can be retried at the original call in the VM to run Lisp handlers.
pub(crate) fn immediate_native_primitive(
    runtime: &Runtime,
    target: Word,
    arguments: &[usize],
) -> Option<Primitive> {
    let op = Primitive::decode(target)?;
    match (op, arguments) {
        (Primitive::String(strings::Op::Length), [_])
        | (Primitive::Array(arrays::Op::Aref), [_, ..])
        | (Primitive::Number(numbers::Op::Add), _) => Some(op),
        (Primitive::Typep, &[_, spec])
            if type_check::is_immediate_array_check(runtime, Word::from_raw(spec)) =>
        {
            Some(op)
        }
        _ => None,
    }
}

pub(crate) fn primitive(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Primitive,
    args: Range<Register>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let truth = |condition| if condition { Word::T } else { Word::NIL };

    let word = match op {
        Primitive::Printer(_) | Primitive::Format(_) => unreachable!("printer continuation"),
        Primitive::Environment(op) => {
            return environment::run(runtime, frame, op, args, values, at);
        }
        Primitive::Array(op) => return arrays::run(runtime, frame, op, args, values, at),
        Primitive::Treelist(op) => return treelist::run(runtime, frame, op, args, values, at),
        Primitive::Simd(op) => simd::invoke(runtime, op, frame.slice(args), at)?,
        Primitive::Sequence(op) => sequence::run(runtime, frame, op, args, at)?,
        Primitive::Coerce(op) => coerce::run(runtime, frame, op, args, at)?,
        Primitive::String(op) => strings::run(runtime, frame, op, args, at)?,
        Primitive::Character(op) => characters::run(runtime, frame, op, args, at)?,
        Primitive::Readtable(op) => return readtables::run(runtime, frame, op, args, values, at),
        Primitive::FindRestart
        | Primitive::ComputeRestarts
        | Primitive::InvokeRestart
        | Primitive::InvokeRestartInteractively
        | Primitive::StandardRestart(_) => {
            unreachable!("restart queries dispatch through the VM")
        }
        Primitive::RestartName => {
            let word = frame.get(args.start);
            runtime.heap.restart_name(word).ok_or_else(|| {
                Fault::type_error(
                    runtime,
                    word,
                    "RESTART",
                    EvalError::new(at, "expected a restart object"),
                )
            })?
        }
        Primitive::Signal
        | Primitive::Error
        | Primitive::Cerror
        | Primitive::CallWithSimpleRestart
        | Primitive::Warn
        | Primitive::InvokeDebugger => {
            unreachable!("signaling dispatches through the VM")
        }
        Primitive::Eval
        | Primitive::Load
        | Primitive::LoadLogicalPathnameTranslations
        | Primitive::Require
        | Primitive::Compile
        | Primitive::CompileFile => {
            unreachable!("source evaluation and compilation dispatch through the VM")
        }
        Primitive::Provide => modules::provide(runtime, frame.get(args.start), at)?,
        Primitive::LispImplementationType => modules::implementation_type(runtime, at)?,
        Primitive::LispImplementationVersion => modules::implementation_version(runtime, at)?,
        Primitive::Mapping(_) | Primitive::Tree(_) | Primitive::SubsequenceComparison(_) => {
            unreachable!("continuation dispatches through the VM")
        }
        Primitive::Instance(_) => unreachable!("instance operations dispatch through the VM"),
        Primitive::Async(_) => unreachable!("async operations dispatch through the VM"),
        Primitive::SequenceMap(_) => unreachable!("sequence mapping dispatches through the VM"),
        Primitive::HashTable(op) => return hash_tables::run(runtime, frame, op, args, values, at),
        Primitive::MakeCondition => {
            unreachable!("condition construction dispatches through the VM")
        }
        Primitive::FunctionDefinition(op) => {
            function_definitions::inspect(runtime, op, frame.get(args.start), at)?
        }
        Primitive::SetFdefinition => {
            let function = frame.get(args.start);
            let name = FunctionName::parse(runtime, frame.get(args.start + 1), at)?;
            name.install(runtime, function, at)?;
            function
        }
        Primitive::SetSymbolFunction => {
            let function = frame.get(args.start);
            let symbol = frame.get(args.start + 1);
            if !runtime.heap.is_symbol(symbol) {
                return Err(Fault::type_error(
                    runtime,
                    symbol,
                    "SYMBOL",
                    EvalError::new(at, "expected a symbol"),
                ));
            }
            FunctionName::Symbol(symbol).install(runtime, function, at)?;
            function
        }
        Primitive::Fmakunbound => {
            function_definitions::fmakunbound(runtime, frame.get(args.start), at)?
        }
        Primitive::Constantp => {
            let env = if args.len() == 2 {
                frame.get(args.start + 1)
            } else {
                Word::NIL
            };
            truth(crate::compile::constantp(
                runtime,
                frame.get(args.start),
                env,
                at,
            )?)
        }
        Primitive::Proclaim => {
            crate::compile::proclaim(runtime, frame.get(args.start), at)?;
            Word::NIL
        }
        Primitive::FunctionLambdaExpression => {
            function_definitions::function_lambda_expression(
                runtime,
                frame.get(args.start),
                values,
                at,
            )?;
            return Ok(());
        }
        Primitive::RegisterDeclarationHandler => crate::compile::declarations::set_handler(
            runtime,
            frame.get(args.start),
            frame.get(args.start + 1),
            at,
        )?,
        Primitive::DeclarationInformation => crate::compile::declarations::information(
            runtime,
            frame.get(args.start),
            if args.len() == 2 {
                frame.get(args.start + 1)
            } else {
                Word::NIL
            },
            at,
        )?,
        Primitive::FunctionLambdaList => {
            function_definitions::function_lambda_list(runtime, frame.get(args.start), values, at)?;
            return Ok(());
        }
        Primitive::Constantly => constantly_function(runtime, frame.get(args.start), at)?,
        Primitive::Complement => complement_function(runtime, frame.get(args.start), at)?,
        Primitive::FunctionP => truth(is_function(runtime, frame.get(args.start))),
        Primitive::ReadFromString => return read_string::run(runtime, frame, args, values, at),
        Primitive::Stream(op) => return streams::run(runtime, frame, op, args, values, at),
        Primitive::SymbolName => symbols::symbol_name(runtime, frame.get(args.start), at)?,
        Primitive::SymbolP => truth(runtime.heap.is_symbol(frame.get(args.start))),
        Primitive::KeywordP => {
            truth(runtime.heap.symbol_home(frame.get(args.start)) == Some(Package::KEYWORD))
        }
        Primitive::SymbolPackage => symbols::symbol_package(runtime, frame.get(args.start), at)?,
        Primitive::MakeSymbol => symbols::make_symbol(runtime, frame.get(args.start), at)?,
        Primitive::CopySymbol => symbols::copy_symbol(runtime, frame, args, at)?,
        Primitive::Gensym => symbols::gensym(
            runtime,
            (!args.is_empty()).then(|| frame.get(args.start)),
            at,
        )?,
        Primitive::Gentemp => symbols::gentemp(runtime, frame, args, at)?,
        Primitive::PatternTag => runtime
            .heap
            .structure_layout(frame.get(args.start))
            .map_or(Word::NIL, |(name, _)| name),
        Primitive::PatternField => {
            let field = runtime
                .variant_field(frame.get(args.start), frame.get(args.start + 1))
                .ok_or_else(|| EvalError::new(at, "field is absent from this variant layout"))?;
            if field == Word::UNBOUND {
                return Err(EvalError::new(at, "unbound variant field").into());
            }
            field
        }
        Primitive::TypeOf => types::type_of(runtime, frame.get(args.start), at)?,
        Primitive::ClassOf => classes::class_of(runtime, frame.get(args.start), at)?,
        Primitive::ClassName => classes::class_name(runtime, frame.get(args.start), at)?,
        Primitive::FindClass => classes::find_class(runtime, frame, args, at)?,
        Primitive::DefineNamedClass => unreachable!("class definitions dispatch through the VM"),
        Primitive::CopyStructure => structures::copy_structure(runtime, frame.get(args.start), at)?,
        Primitive::Documentation => {
            let object = frame.get(args.start);
            let kind = frame.get(args.start + 1);
            if kind == Word::T
                && let Some(package) = Package::from_word(object)
            {
                runtime.packages.borrow().documentation(package)
            } else if kind == Word::T
                && let Some(slot) = runtime.heap.slot_definition_info(object)
            {
                slot.documentation
            } else if kind == Word::T && runtime.heap.class_info(object).is_some() {
                classes::class_definition(runtime, object)
                    .and_then(|definition| definition.documentation.clone())
                    .map_or(Word::NIL, |documentation| documentation.get())
            } else if kind == Word::T
                && let Some(info) = runtime.heap.method_info(object)
            {
                info.documentation
            } else if kind == Word::T
                && let Some(info) = runtime.heap.generic_info(object)
            {
                info.documentation
            } else if kind == Word::T {
                match runtime.heap.function_documentation(object) {
                    Some(documentation) => documentation,
                    None => simd::documentation(runtime, object, at)?,
                }
            } else if runtime.find_named_symbol(&Name::common_lisp("FUNCTION")) == Some(kind) {
                let function = if is_function(runtime, object) {
                    Some(object)
                } else {
                    FunctionName::parse(runtime, object, at)?.get(runtime)
                };
                if let Some(function) = function {
                    let documentation =
                        runtime.heap.function_documentation(function).or_else(|| {
                            runtime
                                .heap
                                .generic_info(function)
                                .map(|info| info.documentation)
                        });
                    match documentation {
                        Some(documentation) => documentation,
                        None => simd::documentation(runtime, function, at)?,
                    }
                } else {
                    Word::NIL
                }
            } else if runtime.find_named_symbol(&Name::common_lisp("VARIABLE")) == Some(kind) {
                runtime.heap.variable_documentation(object)
            } else if runtime.find_named_symbol(&Name::common_lisp("STRUCTURE")) == Some(kind)
                || runtime.find_named_symbol(&Name::common_lisp("TYPE")) == Some(kind)
            {
                runtime
                    .structure_definition(object)
                    .map(|definition| definition.documentation.get())
                    .or_else(|| {
                        (runtime.find_named_symbol(&Name::common_lisp("TYPE")) == Some(kind))
                            .then(|| crate::classes::named_definition(runtime, object))
                            .flatten()
                            .and_then(|definition| definition.documentation.clone())
                            .map(|documentation| documentation.get())
                    })
                    .unwrap_or(Word::NIL)
            } else if runtime.find_named_symbol(&Name::common_lisp("SETF")) == Some(kind) {
                runtime.setf_documentation(object)
            } else {
                Word::NIL
            }
        }
        Primitive::SetDocumentation => {
            let new_doc = frame.get(args.start);
            let object = frame.get(args.start + 1);
            let kind = frame.get(args.start + 2);
            method_combination::store_documentation(runtime, object, kind, new_doc, at)?;
            new_doc
        }
        Primitive::Subtypep => {
            let (subtype, valid) =
                types::subtypep(runtime, frame.get(args.start), frame.get(args.start + 1));
            values.resize(2);
            values.slots.set(0, truth(subtype));
            values.slots.set(1, truth(valid));
            return Ok(());
        }
        Primitive::Typep => {
            if args.len() == 3 && frame.get(args.start + 2) != Word::NIL {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(
                        at,
                        "TYPEP currently supports only the null lexical environment",
                    ),
                ));
            }
            truth(types::typep(
                runtime,
                frame.get(args.start),
                frame.get(args.start + 1),
                at,
            )?)
        }
        Primitive::FormatControl
        | Primitive::FormatArguments
        | Primitive::TypeErrorDatum
        | Primitive::TypeErrorExpectedType
        | Primitive::CellErrorName
        | Primitive::PackageErrorPackage
        | Primitive::PrintNotReadableObject
        | Primitive::FileErrorPathname
        | Primitive::StreamErrorStream
        | Primitive::ArithmeticErrorOperation
        | Primitive::ArithmeticErrorOperands => {
            let (expected, second) = match op {
                Primitive::FormatControl => (Class::SimpleCondition, false),
                Primitive::FormatArguments => (Class::SimpleCondition, true),
                Primitive::TypeErrorDatum => (Class::TypeError, false),
                Primitive::TypeErrorExpectedType => (Class::TypeError, true),
                Primitive::CellErrorName => (Class::CellError, false),
                Primitive::PackageErrorPackage => (Class::PackageError, false),
                Primitive::PrintNotReadableObject => (Class::PrintNotReadable, false),
                Primitive::FileErrorPathname => (Class::FileError, false),
                Primitive::StreamErrorStream => (Class::StreamError, false),
                Primitive::ArithmeticErrorOperation => (Class::ArithmeticError, false),
                Primitive::ArithmeticErrorOperands => (Class::ArithmeticError, true),
                _ => unreachable!("only condition accessors dispatch here"),
            };
            let datum = frame.get(args.start);
            condition::builtin_slot(runtime, datum, expected, second, at)?
        }
        Primitive::SymbolValue | Primitive::Boundp | Primitive::Makunbound | Primitive::Set => {
            let symbol = frame.get(args.start);
            if !runtime.heap.is_symbol(symbol) {
                return Err(Fault::type_error(
                    runtime,
                    symbol,
                    "SYMBOL",
                    EvalError::new(at, "expected a symbol"),
                ));
            }
            match op {
                Primitive::Boundp => truth(runtime.heap.symbol_cell(symbol, false).is_some()),
                Primitive::SymbolValue => symbol_value(runtime, symbol, at)?,
                Primitive::Set | Primitive::Makunbound => {
                    if !runtime.heap.is_variable(symbol) {
                        return Err(Fault::plain(
                            Class::ProgramError,
                            EvalError::new(at, "cannot change a constant variable"),
                        ));
                    }
                    if matches!(op, Primitive::Set) {
                        let value = frame.get(args.start + 1);
                        runtime.heap.set_symbol_cell(symbol, false, value);
                        value
                    } else {
                        runtime.heap.set_symbol_cell(symbol, false, Word::UNBOUND);
                        symbol
                    }
                }
                _ => unreachable!(),
            }
        }
        Primitive::Values => {
            values.select(frame, args);
            return Ok(());
        }
        Primitive::ValuesList => {
            let list = frame.get(args.start);
            let items = proper_list(runtime, list, at).map_err(|error| {
                Fault::type_error(runtime, list::list_tail(runtime, list), "LIST", error)
            })?;
            values.resize(items.len());
            for (index, item) in items.into_iter().enumerate() {
                values.slots.set(index, item);
            }
            return Ok(());
        }
        Primitive::Funcall
        | Primitive::Apply
        | Primitive::Macroexpand
        | Primitive::MacroexpandOne => {
            unreachable!("call primitives resolve their arguments before dispatch")
        }
        Primitive::GetSetfExpansion => {
            let environment = if args.len() == 2 {
                frame.get(args.start + 1)
            } else {
                Word::NIL
            };
            let expansion =
                macros::get_setf_expansion(runtime, frame.get(args.start), environment, at)?;
            values.select_roots(&expansion);
            return Ok(());
        }
        Primitive::MacroFunction => {
            let symbol = frame.get(args.start);
            if !runtime.heap.is_symbol(symbol) {
                return Err(Fault::type_error(
                    runtime,
                    symbol,
                    "SYMBOL",
                    EvalError::new(at, "expected a symbol as a macro name"),
                ));
            }
            let environment = if args.len() == 2 {
                frame.get(args.start + 1)
            } else {
                Word::NIL
            };
            runtime
                .macro_function(symbol, environment, at)?
                .map_or(Word::NIL, |function| function.slot.get())
        }
        Primitive::CompilerMacroFunction => {
            let fn_name = FunctionName::parse(runtime, frame.get(args.start), at)?;
            let environment = if args.len() == 2 {
                frame.get(args.start + 1)
            } else {
                Word::NIL
            };
            let (symbol, is_setf) = match fn_name {
                FunctionName::Symbol(symbol) => (symbol, false),
                FunctionName::Setf(accessor) => (accessor, true),
            };
            runtime
                .compiler_macro_function(symbol, is_setf, environment, at)?
                .map_or(Word::NIL, |function| function.slot.get())
        }
        Primitive::SetCompilerMacroFunction => {
            let new_fn = frame.get(args.start);
            let fn_name = FunctionName::parse(runtime, frame.get(args.start + 1), at)?;
            let environment = if args.len() == 3 {
                frame.get(args.start + 2)
            } else {
                Word::NIL
            };
            macros::validate_environment(runtime, environment, at)?;
            let (symbol, is_setf) = match fn_name {
                FunctionName::Symbol(symbol) => (symbol, false),
                FunctionName::Setf(accessor) => (accessor, true),
            };
            check_definable_name(runtime, symbol, at)?;
            if new_fn != Word::NIL && !is_function(runtime, new_fn) {
                return Err(Fault::type_error(
                    runtime,
                    new_fn,
                    "FUNCTION",
                    EvalError::new(at, "expected a function or NIL"),
                ));
            }
            runtime.set_compiler_macro_function(
                symbol,
                is_setf,
                (new_fn != Word::NIL).then_some(new_fn),
            );
            new_fn
        }
        Primitive::List(op) => return list::run(runtime, frame, op, args, values, at),
        Primitive::Package(op) => return packages::run(runtime, frame, op, args, values, at),
        Primitive::Pathname(op) => return pathnames::run(runtime, frame, op, args, values, at),
        Primitive::Plist(op) => return plists::run(runtime, frame, op, args, values, at),
        Primitive::Truename
        | Primitive::ProbeFile
        | Primitive::Directory
        | Primitive::DeleteFile
        | Primitive::FileWriteDate
        | Primitive::FileAuthor => filesystem::run(runtime, frame, op, args, at)?,
        Primitive::RenameFile => {
            let renamed = filesystem::rename(runtime, frame, args, at)?;
            values.resize(3);
            for (index, word) in renamed.into_iter().enumerate() {
                values.slots.set(index, word);
            }
            return Ok(());
        }
        Primitive::EnsureDirectoriesExist => {
            let (path, created) = filesystem::ensure_directories(runtime, frame, args, at)?;
            values.resize(2);
            values.slots.set(0, path);
            values
                .slots
                .set(1, if created { Word::T } else { Word::NIL });
            return Ok(());
        }
        Primitive::Association(_) => unreachable!("association callbacks are handled by the VM"),
        Primitive::Debug(_) => unreachable!("debugger commands dispatch through the VM"),
        Primitive::Deduplicate(_) => unreachable!("deduplication dispatches through the VM"),
        Primitive::ElementSearch(_) => unreachable!("sequence searches dispatch through the VM"),
        Primitive::Sets(_) => unreachable!("set operations dispatch through the VM"),
        Primitive::Sort(_) => unreachable!("sort operations dispatch through the VM"),
        Primitive::Reduce(_) => unreachable!("reduction dispatches through the VM"),
        Primitive::PredicateRemove(_) => {
            unreachable!("predicate removal dispatches through the VM")
        }
        Primitive::Identity => frame.get(args.start),
        Primitive::Eq => truth(frame.get(args.start) == frame.get(args.start + 1)),
        Primitive::Eql => truth(equality::eql(
            &runtime.heap,
            frame.get(args.start),
            frame.get(args.start + 1),
        )),
        Primitive::Equal => truth(equality::equal(
            &runtime.heap,
            frame.get(args.start),
            frame.get(args.start + 1),
        )),
        Primitive::Equalp => truth(equality::equalp(
            &runtime.heap,
            frame.get(args.start),
            frame.get(args.start + 1),
        )),
        Primitive::Sxhash => equality::sxhash(&runtime.heap, frame.get(args.start)),
        Primitive::Number(op) => return numbers::run(runtime, frame, op, args, values, at),
        Primitive::Random(op) => random::run(runtime, frame, op, args, at)?,
    };
    values.one(word);
    Ok(())
}

pub(crate) fn proper_list(
    runtime: &Runtime,
    list: Word,
    at: usize,
) -> Result<Vec<Word>, EvalError> {
    let mut result = Vec::new();
    visit_proper_list(&runtime.heap, list, at, |word| result.push(word))?;
    Ok(result)
}

/// The visitor must not enter Lisp or collect: the spine holds unrooted cons words.
fn visit_proper_list(
    heap: &crate::heap::Heap,
    list: Word,
    at: usize,
    mut visit: impl FnMut(Word),
) -> Result<(), EvalError> {
    let invalid = || EvalError::new(at, "expected a proper list");
    let mut spine = ListSpine::new(list);
    while let Some((first, _)) = spine.next(heap).map_err(|_| invalid())? {
        visit(first);
    }
    if spine.tail() != Word::NIL {
        return Err(invalid());
    }
    Ok(())
}

fn closure_from_template(
    runtime: &Runtime,
    cell: &std::cell::OnceCell<RootSlot>,
    build: impl FnOnce() -> FunctionTemplate,
    capture: Word,
    at: usize,
) -> Result<Word, Fault> {
    let capture = runtime.root(capture);
    if cell.get().is_none() {
        let slot = runtime.root_slot(build().instantiate(runtime)?);
        let _ = cell.set(slot);
    }
    runtime
        .heap
        .closure(
            1,
            || cell.get().expect("closure template initialized").get(),
            |_| capture.slot.get(),
        )
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn constantly_function(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    closure_from_template(
        runtime,
        &runtime.constantly_code,
        || FunctionTemplate {
            name: None,
            debug: Default::default(),
            instructions: vec![
                Instruction::Capture(0, 0),
                Instruction::ReleaseArguments,
                Instruction::SetValues(0..1),
            ],
            constants: Vec::new(),
            lambda_list: None,
            registers: 1,
            arity: Arity {
                required: 0,
                maximum: None,
            },
            captures: vec![CaptureKind::Value],
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        },
        value,
        at,
    )
}

fn complement_function(runtime: &Runtime, function: Word, at: usize) -> Result<Word, Fault> {
    if !is_function(runtime, function) {
        return Err(Fault::type_error(
            runtime,
            function,
            "FUNCTION",
            EvalError::new(at, "expected a function"),
        ));
    }
    closure_from_template(
        runtime,
        &runtime.complement_code,
        || FunctionTemplate {
            name: None,
            debug: Default::default(),
            instructions: vec![
                Instruction::Capture(0, 0),
                Instruction::RestArguments {
                    start: 0,
                    out: 1,
                    at: 0,
                },
                Instruction::ReleaseArguments,
                Instruction::Constant(2, 0),
                Instruction::Call {
                    callee: Callee::Value(2),
                    out: Some(0),
                    args: 0..2,
                    at: 0,
                    associate: false,
                    notinline: false,
                },
                Instruction::Unless(0, 8),
                Instruction::Load(0, Immediate::Nil),
                Instruction::Jump(9),
                Instruction::Load(0, Immediate::True),
                Instruction::SetValues(0..1),
            ],
            constants: vec![ConstantTemplate::Immediate(
                Primitive::named("APPLY").expect("APPLY primitive registered"),
            )],
            lambda_list: None,
            registers: 3,
            arity: Arity {
                required: 0,
                maximum: None,
            },
            captures: vec![CaptureKind::Value],
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        },
        function,
        at,
    )
}

#[cfg(test)]
mod frame_pinning_tests {
    use super::*;
    use crate::code_store::{CodeSideTables, CodeToInstall};
    use crate::native::{AotCodeDescriptor, JitCallContext, JitReturn, NativeEntry};

    #[test]
    fn start_evaluation_rejects_a_pinned_frame_even_at_zero_eval_depth() {
        let runtime = Runtime::start().unwrap();
        assert_eq!(runtime.eval_depth.get(), 0);
        let pin = runtime.pin_rust_frame();
        let Err(error) = runtime.start_eval("42") else {
            panic!("an incremental evaluation must not borrow a pinned frame");
        };
        assert_eq!(
            error.message(),
            "incremental execution requires a top-level invocation"
        );
        drop(pin);
        assert!(runtime.start_eval("42").is_ok());
    }

    #[test]
    fn a_pinned_native_tail_call_is_dispatched_without_handoff() {
        unsafe extern "C" fn tail_call(
            ctx: *mut JitCallContext,
            _: usize,
            args: *const usize,
            _: usize,
            out: *mut JitReturn,
        ) {
            let value = Word::fixnum(42).unwrap().raw();
            let mut result = JitReturn::zeroed();
            result.tag = zkcl_bytecode::native_abi::JIT_RETURN_TAG_TAIL_CALL;
            // SAFETY: the test passes one rooted Lisp callable, and the
            // trampoline supplies live context and result pointers.
            unsafe {
                crate::native::zkcl_rt_stage_tail_call(ctx, args.read(), &value, 1, 0, false);
                out.write(result);
            }
        }

        let runtime = Runtime::start().unwrap();
        runtime
            .register_function("probe-pinned-tail-call", |runtime, args| {
                assert!(task_scheduler::in_scheduler(runtime));
                assert!(runtime.has_pinned_rust_frame());
                runtime.native_slice_steps.set(0);
                let arity = Arity {
                    required: 1,
                    maximum: Some(1),
                };
                let store = &runtime.heap.code_store;
                let code = store.get(store.insert(CodeToInstall {
                    name: None,
                    source: None,
                    debug: Default::default(),
                    arity,
                    lambda_list: None,
                    registers: 1,
                    instructions: vec![Instruction::SetValues(0..1)],
                    captures: vec![],
                    notinline: vec![],
                    vop_calls: vec![],
                    tables: CodeSideTables {
                        handlers: vec![],
                        restarts: vec![],
                        keywords: vec![],
                        jump_tables: vec![],
                        facts: Default::default(),
                    },
                    canonical_symbols: vec![],
                    native: Some(AotCodeDescriptor {
                        native_debug: false,
                        entry_index: 0,
                        arity,
                        root_slots: 0,
                        fast_entry: None,
                        generic_entry: NativeEntry::from_jit_c(0, 0).generic_entry,
                        c_entry: Some(tail_call),
                        continuation_abi: None,
                        leaf_entry: None,
                        leaf_guards: &[],
                    }),
                }));
                let slots = runtime.staging_stack.alloc(1);
                slots.set(0, args[0].slot.get());
                let invocation = NativeInvocation::from_slots(
                    runtime,
                    code.native_entry().unwrap(),
                    Word::NIL,
                    &slots,
                    0..1,
                    Resume::Call(None),
                    true,
                );
                let mut exec = ExecState::new(Values::new(runtime));
                let step = accept(runtime, &mut exec, Invocation::Native(invocation)).unwrap();
                assert!(
                    matches!(step, StepControl::Pending(None)),
                    "pinned tail call must not hand off"
                );
                assert_eq!(exec.values.len, 1);
                assert_eq!(exec.values.get(0), Word::fixnum(42).unwrap());
                Ok(vec![runtime.root(exec.values.get(0))])
            })
            .unwrap();
        let values = runtime.eval("(probe-pinned-tail-call #'values)").unwrap();
        assert_eq!(values[0].as_fixnum(), Some(42));
    }
}
