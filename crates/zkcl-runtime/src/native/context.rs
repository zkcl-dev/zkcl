use std::cell::Cell;
use std::marker::PhantomData;
use std::mem::DropGuard;
use std::ops::Deref;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::AtomicU8;

use zkcl_bytecode::Class;

use super::{Fault, JitReturn, NativeReturn, NativeTailCall};
use crate::heap::{RootSlots, RootStackFrame, Word};
use crate::vm::{self, ExecFrame, ExecState, NativeFrameAnchor};
use crate::{EvalError, Runtime};

/// Execution context passed across native AOT/JIT frames on a single `Runtime` thread.
pub struct NativeContext<'rt, 'exec> {
    pub(crate) runtime: &'rt Runtime,
    pub(crate) exec: NonNull<ExecState<'rt>>,
    pub(crate) code: Option<Rc<crate::code_store::VmCode>>,
    pub(crate) _marker: PhantomData<&'exec mut ExecState<'rt>>,
}

impl<'rt, 'exec> NativeContext<'rt, 'exec> {
    pub(crate) fn new(runtime: &'rt Runtime, exec: NonNull<ExecState<'rt>>) -> Self {
        Self {
            runtime,
            exec,
            code: None,
            _marker: PhantomData,
        }
    }

    pub fn runtime(&self) -> &'rt Runtime {
        self.runtime
    }

    pub(crate) fn exec_mut(&mut self) -> &mut ExecState<'rt> {
        // SAFETY: `&mut self` grants exclusive access to `ExecState` for this call.
        unsafe { self.exec.as_mut() }
    }

    pub fn values_len(&self) -> usize {
        // SAFETY: No other mutable borrow to `ExecState` is active while `&self` is held.
        unsafe { self.exec.as_ref() }.values.len
    }

    pub fn value(&self, index: usize) -> Word {
        // SAFETY: No other mutable borrow to `ExecState` is active while `&self` is held.
        unsafe { self.exec.as_ref() }.values.get(index)
    }

    pub fn set_one_value(&mut self, word: Word) {
        self.exec_mut().values.one(word);
    }

    pub fn set_values(&mut self, words: &[Word]) {
        let values = &mut self.exec_mut().values;
        values.resize(words.len());
        for (i, &word) in words.iter().enumerate() {
            values.slots.set(i, word);
        }
    }

    pub fn clear_values(&mut self) {
        self.exec_mut().values.resize(0);
    }

    /// Materialize a `NativeReturn` into `ExecState::values` (no-op for `Values` or `Unwind`).
    pub fn store_return(&mut self, ret: NativeReturn) {
        if let NativeReturn::One(word) = ret {
            self.set_one_value(word);
        }
    }

    /// Snapshot all currently active return values as `Word`s (without clearing them).
    pub fn read_values(&self) -> Vec<Word> {
        // SAFETY: No other mutable borrow to `ExecState` is active while `&self` is held.
        let exec = unsafe { self.exec.as_ref() };
        (0..exec.values.len).map(|i| exec.values.get(i)).collect()
    }

    /// Read all values from a `NativeReturn::One` or `NativeReturn::Values` into a `Vec<Word>`.
    pub fn collect_return_values(&self, ret: NativeReturn) -> Vec<Word> {
        match ret {
            NativeReturn::One(word) => vec![word],
            NativeReturn::Values => self.read_values(),
            NativeReturn::Unwind { .. } => Vec::new(),
        }
    }

    pub fn root_stack_top(&self) -> usize {
        self.runtime.root_stack.top()
    }
}

/// RAII root frame for a native Lisp activation record.
///
/// Allocates `RootStackFrame<'rt>` on `runtime.root_stack`, registers an `ExecFrame::Native`
/// anchor in `ExecState::frames`, and releases both in strict LIFO order on `Drop`.
pub struct NativeRootFrame<'rt> {
    frame_index: usize,
    control_base: usize,
    slots: RootStackFrame<'rt>,
    exec: NonNull<ExecState<'rt>>,
}

impl<'rt> NativeRootFrame<'rt> {
    pub fn new(ctx: &mut NativeContext<'rt, '_>, slots_len: usize) -> Self {
        let slots = ctx.runtime.root_stack.alloc(slots_len);
        let root_slots = slots.slots();
        let slots_ptr = root_slots.as_ptr();
        // SAFETY: No other mutable reference to `ExecState` is active during `new`.
        let exec = unsafe { ctx.exec.as_mut() };
        let frame_index = exec.frames.len();
        let control_base = exec.control.len();
        exec.frames.push(ExecFrame::Native(NativeFrameAnchor {
            slots_ptr,
            slots_len,
            pending_state: None,
            backing: None,
            arguments: vm::ArgumentTail::default(),
            code: ctx.code.clone(),
            source_offset: None,
            debug: None,
            debug_arguments: None,
            profile: ctx
                .code
                .as_deref()
                .and_then(|code| ctx.runtime.profile_native_activation(code)),
        }));
        Self {
            frame_index,
            control_base,
            slots,
            exec: ctx.exec,
        }
    }

    #[inline(always)]
    pub fn frame_index(&self) -> usize {
        self.frame_index
    }

    #[inline(always)]
    pub fn slots(&self) -> &RootSlots {
        self.slots.slots()
    }

    #[inline(always)]
    pub fn get(&self, index: usize) -> Word {
        self.slots.slots().get(index)
    }

    #[inline(always)]
    pub fn set(&self, index: usize, word: Word) {
        self.slots.slots().set(index, word);
    }

    /// Read and clear the pending control-recovery state recorded on this frame's anchor.
    pub fn take_pending_state(&self, ctx: &mut NativeContext<'rt, '_>) -> Option<usize> {
        let exec = ctx.exec_mut();
        match exec.frames.get_mut(self.frame_index) {
            Some(ExecFrame::Native(anchor)) => anchor.pending_state.take(),
            _ => None,
        }
    }

    /// Check whether a `NativeReturn::Unwind` targets this native frame, and if so,
    /// consume its `pending_state` and return `Some(state)`.
    pub fn catch_unwind(
        &self,
        ctx: &mut NativeContext<'rt, '_>,
        ret: NativeReturn,
    ) -> Option<usize> {
        if let NativeReturn::Unwind {
            target_frame,
            state,
        } = ret
            && target_frame == self.frame_index
        {
            let _ = self.take_pending_state(ctx);
            Some(state)
        } else {
            None
        }
    }
}

impl Deref for NativeRootFrame<'_> {
    type Target = RootSlots;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.slots()
    }
}

impl Drop for NativeRootFrame<'_> {
    fn drop(&mut self) {
        // SAFETY: `ExecState` outlives this stack frame and no `&mut ExecState` is held across drop.
        let exec = unsafe { self.exec.as_mut() };
        if self.frame_index < exec.frames.len() {
            exec.control
                .truncate_frame_scopes(self.frame_index, self.control_base);
            exec.frames.truncate(self.frame_index);
        }
        // `self.slots` (`RootStackFrame<'rt>`) drops right after this body in strict LIFO order.
    }
}

#[derive(Clone, Copy)]
pub(super) struct CachedSelfCall {
    pub(super) epochs: [usize; 3],
    pub(super) key: [usize; 2],
    pub(super) argc: usize,
}

/// Per-call context passed by pointer to native entries and runtime calls.
///
/// # Runtime call contract
///
/// Pass the context of the current native invocation on the runtime's owning thread.
/// Keep it exclusively available to the call and its nested invocations. Its runtime,
/// execution state, and native entry must remain valid for the call. Runtime functions that
/// accept a detached context document that exception.
///
/// Every raw Lisp word must encode an immediate or a live object in this runtime.
/// Keep heap values needed across a collecting call in registered root slots and
/// reload them afterwards. This also applies to a heap value returned in a register
/// or in the `single_value` field of a [`JitReturn`].
///
/// An `out_ret` argument must point to aligned, writable storage for one [`JitReturn`]
/// that remains available for the entire call. Input arrays must be aligned, readable
/// as a single allocation of the stated length, and stable while read. Keep output
/// storage disjoint from live inputs and runtime state.
///
/// A `frame_index` used to install a scope identifies the calling activation's live
/// native frame. A resume or cleanup state belongs to that activation's generated
/// code. The `clear` boundary may discard only root slots dead at its landing point.
#[repr(C)]
pub struct JitCallContext {
    /// Nonzero requests a runtime safepoint check.
    pub cancel_flag: *const AtomicU8,
    pub remaining_steps: *mut u64,
    pub runtime: *const Runtime,
    pub(super) slice_steps: *const Cell<u16>,
    pub(crate) exec: *mut ExecState<'static>,
    pub(crate) current_entry: Option<JitCEntry>,
    pub pending_fault: Option<Fault>,
    pub(super) self_call: Option<CachedSelfCall>,
    pub(super) frame_depth: *const Cell<usize>,
    pub(super) pending_tail_call: Option<NativeTailCall>,
    pub(super) resumable: bool,
    pub(super) allow_handoff: bool,
    pub(super) resume_after_call: bool,
    pub(super) code: Option<Rc<crate::code_store::VmCode>>,
    pub(super) osr: Option<vm::OsrActivation>,
}

impl JitCallContext {
    /// Detached null context for standalone leaf JIT tests without a live `Runtime`.
    pub const fn detached() -> Self {
        Self {
            cancel_flag: std::ptr::null(),
            remaining_steps: std::ptr::null_mut(),
            runtime: std::ptr::null(),
            slice_steps: std::ptr::null(),
            exec: std::ptr::null_mut(),
            current_entry: None,
            pending_fault: None,
            self_call: None,
            frame_depth: std::ptr::null(),
            pending_tail_call: None,
            resumable: false,
            allow_handoff: false,
            resume_after_call: false,
            code: None,
            osr: None,
        }
    }

    /// Validate and borrow a mutable [`JitCallContext`] pointer received across the C ABI.
    ///
    /// # Safety
    ///
    /// When `ctx` is non-null, it must point to a live, uniquely accessible `JitCallContext`
    /// with valid `runtime` and `exec` pointers when both are non-null.
    pub(super) unsafe fn from_raw<'a>(ctx: *mut Self) -> Option<&'a mut Self> {
        let context = unsafe { ctx.as_mut() }?;
        if context.runtime.is_null() || context.exec.is_null() {
            return None;
        }
        Some(context)
    }

    /// Borrow a live `Runtime` from a potentially detached `JitCallContext` pointer.
    ///
    /// # Safety
    /// `ctx` must be null or point to a readable `JitCallContext` whose `runtime` pointer,
    /// if non-null, points to a live `Runtime`.
    #[inline]
    pub(super) unsafe fn try_runtime<'a>(ctx: *mut Self) -> Option<&'a Runtime> {
        let context = unsafe { ctx.as_ref() }?;
        unsafe { context.runtime.as_ref() }
    }

    /// Borrow the runtime backing this native activation.
    ///
    /// # Safety
    ///
    /// `self.runtime` must be non-null and valid for the returned borrow.
    #[inline]
    pub(super) unsafe fn runtime_ref(&self) -> &Runtime {
        // SAFETY: Caller guarantees `self.runtime` points to a live `Runtime`.
        unsafe { &*self.runtime }
    }

    /// Mutably borrow the execution state backing this native activation.
    ///
    /// # Safety
    ///
    /// `self.exec` must be non-null and exclusively accessible for the duration of `&mut self`.
    #[inline]
    pub(super) unsafe fn exec_mut(&mut self) -> &mut ExecState<'static> {
        // SAFETY: `&mut self` grants exclusive access to the active `ExecState`.
        unsafe { &mut *self.exec }
    }

    /// Borrow both the runtime and execution state without tying the shared runtime borrow to `&mut self`.
    ///
    /// # Safety
    ///
    /// Both `self.runtime` and `self.exec` must be non-null and valid for the call.
    #[inline]
    pub(super) unsafe fn runtime_and_exec_mut(&mut self) -> (&Runtime, &mut ExecState<'static>) {
        // SAFETY: `self.runtime` is immutable while `&mut self` exclusively borrows `self.exec`.
        (unsafe { &*self.runtime }, unsafe { &mut *self.exec })
    }

    pub(super) fn child_context(
        &self,
        entry: JitCEntry,
        resumable: bool,
        code: Option<Rc<crate::code_store::VmCode>>,
    ) -> Self {
        Self {
            cancel_flag: self.cancel_flag,
            remaining_steps: self.remaining_steps,
            runtime: self.runtime,
            slice_steps: self.slice_steps,
            exec: self.exec,
            current_entry: Some(entry),
            pending_fault: None,
            self_call: None,
            frame_depth: self.frame_depth,
            pending_tail_call: None,
            resumable,
            allow_handoff: self.allow_handoff && resumable,
            resume_after_call: false,
            code,
            osr: None,
        }
    }

    pub(super) unsafe fn charge_slice(&self) -> bool {
        let Some(budget) = (unsafe { self.slice_steps.as_ref() }) else {
            return true;
        };
        let remaining = budget.get();
        if remaining == 0 {
            return unsafe { self.refill_slice() };
        }
        budget.set(remaining - 1);
        true
    }

    pub(super) unsafe fn refill_slice(&self) -> bool {
        let runtime = unsafe { &*self.runtime };
        let exec = unsafe { &*self.exec };
        vm::task_scheduler::refill_native_slice(runtime, exec)
    }

    pub(super) fn set_fault(&mut self, fault: Fault) -> JitReturn {
        self.pending_fault = Some(fault);
        JitReturn {
            tag: JitReturn::TAG_FAULT,
            single_value: 0,
            target_frame: 0,
            state: 0,
        }
    }

    pub(super) fn take_fault(&mut self) -> Fault {
        self.pending_fault
            .take()
            .unwrap_or_else(|| Fault::plain(Class::Error, EvalError::new(0, "JIT execution fault")))
    }

    pub(super) fn take_tail_call(&mut self) -> Result<NativeTailCall, Fault> {
        self.pending_tail_call.take().ok_or_else(|| {
            Fault::plain(
                Class::ProgramError,
                EvalError::new(0, "native tail call has no arguments"),
            )
        })
    }

    /// The unwind is a restart only after the selected activation has returned.
    ///
    /// # Safety
    /// The context must point to its live runtime and exclusively accessible executor.
    #[cold]
    pub(super) unsafe fn take_debug_restart(
        &mut self,
        frame: usize,
    ) -> Option<Result<NativeTailCall, Fault>> {
        let (runtime, exec) = unsafe { self.runtime_and_exec_mut() };
        if exec.frames.len() != frame {
            return None;
        }
        exec.control.take_native_debug_restart(runtime, frame)
    }
}

/// Safe view over a live [`JitCallContext`] inside C-ABI boundary wrappers.
pub(super) struct AbiScope<'rt, 'exec> {
    pub(super) native: NativeContext<'rt, 'exec>,
    pub(super) allow_handoff: bool,
    pub(super) resumable: bool,
    resume_after_call: &'exec mut bool,
}

impl AbiScope<'_, '_> {
    pub(super) fn take_resume_after_call(&mut self) -> bool {
        std::mem::take(self.resume_after_call)
    }
}

/// Out-pointer C-ABI signature for JIT- and AOT-compiled functions.
pub type JitCEntryFn = unsafe extern "C" fn(
    ctx: *mut JitCallContext,
    target: usize,
    args_ptr: *const usize,
    argc: usize,
    out_ret: *mut JitReturn,
);

#[derive(Clone, Copy)]
pub(crate) enum JitCEntry {
    Native(JitCEntryFn),
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    Wasm {
        slot: u32,
        scratch_words: u32,
        call: zkcl_wasm::abi::CallFunction,
    },
}

impl JitCEntry {
    #[inline]
    pub(super) unsafe fn invoke(
        self,
        context: *mut JitCallContext,
        target: usize,
        arguments: *const usize,
        argc: usize,
        result: *mut JitReturn,
    ) {
        match self {
            Self::Native(call) => unsafe { call(context, target, arguments, argc, result) },
            #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
            Self::Wasm {
                slot,
                scratch_words,
                call,
            } => {
                use std::mem::MaybeUninit;

                let mut stack = [MaybeUninit::<u64>::uninit(); 4];
                let mut heap = Vec::<MaybeUninit<u64>>::new();
                let u64_count = (scratch_words as usize).div_ceil(2);
                let scratch = if u64_count <= stack.len() {
                    stack.as_mut_ptr()
                } else {
                    heap.resize(u64_count, MaybeUninit::uninit());
                    heap.as_mut_ptr()
                };
                unsafe {
                    call(
                        slot,
                        context.cast(),
                        target,
                        arguments,
                        argc,
                        result.cast(),
                        scratch.cast(),
                    )
                };
            }
        }
    }
}

thread_local! {
    pub(super) static JIT_FRAME_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Upper bound on nested native frames. The VM recurses on heap frames, so
/// only JIT/AOT entry points need this guard to turn a runaway recursion into
/// a Lisp condition instead of a native stack overflow.
const JIT_FRAME_LIMIT: usize = 4096;

pub(super) fn enter_jit_frame(depth: &Cell<usize>) -> Option<impl Drop + '_> {
    let next = depth.get() + 1;
    if next > JIT_FRAME_LIMIT {
        return None;
    }
    depth.set(next);
    Some(DropGuard::new(depth, |depth| depth.update(|n| n - 1)))
}

pub(super) fn jit_stack_exhausted() -> Fault {
    Fault::plain(
        Class::ControlError,
        EvalError::new(0, "JIT control stack exhausted"),
    )
}

/// # Safety
///
/// When `len > 0`, `ptr` must be non-null and valid for `len` initialized `T` elements.
#[inline]
pub(super) unsafe fn raw_slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if len == 0 || ptr.is_null() {
        &[]
    } else {
        // SAFETY: Caller guarantees `[ptr .. ptr + len]` is valid and initialized for `'a`.
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// Enter safe Rust with a [`NativeContext`] borrowed from a validated `JitCallContext`.
///
/// # Safety
/// Caller upholds the [`JitCallContext`] contract for `ctx`.
#[inline]
pub(super) unsafe fn with_native_ctx<T>(
    ctx: *mut JitCallContext,
    operation: impl for<'rt, 'exec> FnOnce(&mut NativeContext<'rt, 'exec>) -> T,
) -> T {
    // SAFETY: The trampoline keeps `runtime` and `exec` alive across each runtime call.
    let context = unsafe { &*ctx };
    let runtime = unsafe { context.runtime_ref() };
    let exec = NonNull::new(context.exec).expect("non-null ExecState in JitCallContext");
    operation(&mut NativeContext::new(runtime, exec))
}

/// Run `operation` with a [`NativeContext`] and write its [`NativeReturn`] or fault to `out_ret`.
///
/// # Safety
/// Caller upholds the [`JitCallContext`] and `out_ret` contracts.
#[inline]
pub(super) unsafe fn write_native_result(
    ctx: *mut JitCallContext,
    out_ret: *mut JitReturn,
    operation: impl for<'rt, 'exec> FnOnce(
        &mut NativeContext<'rt, 'exec>,
    ) -> Result<NativeReturn, Fault>,
) {
    // SAFETY: Caller guarantees `ctx` and `out_ret` obey the C-ABI boundary contract.
    unsafe {
        write_jit_result(ctx, out_ret, |context| {
            operation(context).map(JitReturn::from_native)
        });
    }
}

/// Run `operation` with a [`NativeContext`] and write its [`JitReturn`] or fault to `out_ret`.
///
/// # Safety
/// Caller upholds the [`JitCallContext`] and `out_ret` contracts.
#[inline]
pub(super) unsafe fn write_jit_result(
    ctx: *mut JitCallContext,
    out_ret: *mut JitReturn,
    operation: impl for<'rt, 'exec> FnOnce(&mut NativeContext<'rt, 'exec>) -> Result<JitReturn, Fault>,
) {
    // SAFETY: Caller guarantees `ctx` and `out_ret` are valid for this call.
    let result = unsafe { with_native_ctx(ctx, operation) };
    let ret = match result {
        Ok(ret) => ret,
        Err(fault) => unsafe { &mut *ctx }.set_fault(fault),
    };
    unsafe { out_ret.write(ret) };
}

/// Run `operation` with an [`AbiScope`] (including handoff/resumable flags) and write `out_ret`.
///
/// # Safety
/// Caller upholds the [`JitCallContext`] and `out_ret` contracts.
#[inline]
pub(super) unsafe fn write_abi_result(
    ctx: *mut JitCallContext,
    out_ret: *mut JitReturn,
    operation: impl for<'rt, 'exec> FnOnce(&mut AbiScope<'rt, 'exec>) -> Result<JitReturn, Fault>,
) {
    // SAFETY: Caller guarantees `ctx` and `out_ret` are valid and uniquely accessed here.
    let context = unsafe { &mut *ctx };
    // SAFETY: `context.runtime` points to a live `Runtime` outliving `context` and distinct from `resume_after_call`.
    let runtime = unsafe { &*context.runtime };
    let exec = NonNull::new(context.exec).expect("non-null ExecState in JitCallContext");
    let mut scope = AbiScope {
        native: NativeContext::new(runtime, exec),
        allow_handoff: context.allow_handoff,
        resumable: context.resumable,
        resume_after_call: &mut context.resume_after_call,
    };
    let ret = match operation(&mut scope) {
        Ok(ret) => ret,
        Err(fault) => context.set_fault(fault),
    };
    unsafe { out_ret.write(ret) };
}

/// Write a `TAG_FAULT` status to `out_ret` when `ctx` is null or detached.
///
/// # Safety
/// `out_ret` must point to writable `JitReturn` storage.
#[inline]
pub(super) unsafe fn write_fault_return(out_ret: *mut JitReturn) {
    let mut ret = JitReturn::zeroed();
    ret.tag = JitReturn::TAG_FAULT;
    // SAFETY: Caller guarantees `out_ret` points to writable `JitReturn` storage.
    unsafe { out_ret.write(ret) };
}

/// Write a `JitReturn` value to `out_ret`.
///
/// # Safety
/// `out_ret` must point to writable `JitReturn` storage.
#[inline]
pub(super) unsafe fn write_return(out_ret: *mut JitReturn, ret: JitReturn) {
    // SAFETY: Caller guarantees `out_ret` points to writable `JitReturn` storage.
    unsafe { out_ret.write(ret) };
}
