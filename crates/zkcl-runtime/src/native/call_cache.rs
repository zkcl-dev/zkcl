use std::cell::RefCell;
use std::rc::{Rc, Weak};

use zkcl_bytecode::Arity;

use super::context::{
    CachedSelfCall, enter_jit_frame, jit_stack_exhausted, raw_slice, write_native_result,
    write_return,
};
use super::trampoline::invoke_native_tail;
use super::{
    Fault, JitCEntry, JitCallContext, JitReturn, NativeContext, NativeReturn, NativeTailCall,
    RegisterLeaf, Word,
};
use crate::Runtime;
use crate::code_store::NativeCode;
use crate::vm::{self, ExecState};

/// Resolved target of one named or value call site, valid within the code store,
/// function binding, and GC epochs in which it was found.
pub(super) struct CallCacheEntry {
    pub(super) epochs: [usize; 3],
    pub(super) key: [usize; 2],
    pub(super) target: usize,
    pub(super) executable: Weak<NativeCode>,
    pub(super) arity: Arity,
    pub(super) can_reenter: bool,
}

pub(super) enum CachedCall {
    CurrentEntry,
    Native {
        target: usize,
        executable: Rc<NativeCode>,
    },
}

impl CallCacheEntry {
    const EMPTY: Self = Self {
        epochs: [0; 3],
        key: [0; 2],
        target: 0,
        executable: Weak::new(),
        arity: Arity::ZERO,
        can_reenter: false,
    };

    pub(super) fn epochs(runtime: &Runtime) -> [usize; 3] {
        [
            runtime.heap.code_store.native_epoch(),
            runtime.heap.function_epoch(),
            runtime.heap.gc_epoch(),
        ]
    }

    pub(super) fn resolve(
        &self,
        epochs: [usize; 3],
        key: [usize; 2],
        argc: usize,
        prefer_current_entry: Option<usize>,
    ) -> Option<CachedCall> {
        if self.epochs != epochs || self.key != key || !self.arity.accepts(argc) {
            return None;
        }
        if self.can_reenter && prefer_current_entry == Some(self.target) {
            Some(CachedCall::CurrentEntry)
        } else {
            Some(CachedCall::Native {
                target: self.target,
                executable: self.executable.upgrade()?,
            })
        }
    }
}

thread_local! {
    pub(super) static NAMED_CALL_CACHE: [RefCell<CallCacheEntry>; 256] =
        const { [const { RefCell::new(CallCacheEntry::EMPTY) }; 256] };
    pub(super) static VALUE_CALL_CACHE: [RefCell<CallCacheEntry>; 256] =
        const { [const { RefCell::new(CallCacheEntry::EMPTY) }; 256] };
}

#[inline]
pub(super) fn call_cache_slot(function: usize, constant: usize) -> usize {
    // Mix the aligned function address into the high byte. Consecutive
    // constant indices in the same function then occupy distinct slots.
    let hash = (function as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    ((hash >> 56) as usize ^ constant) & 0xff
}

fn fault_to_jit_return(pending_fault: &mut Option<Fault>, fault: Fault) -> JitReturn {
    *pending_fault = Some(fault);
    JitReturn {
        tag: JitReturn::TAG_FAULT,
        single_value: 0,
        target_frame: 0,
        state: 0,
    }
}

pub(super) fn direct_entry(
    runtime: &Runtime,
    target: Word,
    argc: usize,
) -> Option<(Rc<NativeCode>, Arity)> {
    let code = runtime.heap.function_code(target)?;
    let info = runtime.heap.function_info(code)?;
    let vm_code = runtime.heap.code_store.get(info.code_id);
    if !vm_code.arity.accepts(argc) || runtime.use_debug_bytecode(&vm_code) {
        return None;
    }
    let executable = vm_code.native_entry()?;
    if executable.leaf.is_none() {
        executable.c_entry?;
    }
    Some((executable, vm_code.arity))
}

/// Enter from the raw C argument array once the actual fixed arity is known.
/// A register-guard miss precedes every Lisp effect. At bytecode PC zero only
/// the required argument registers are live, and the VM roots them on entry.
pub(super) unsafe fn call_leaf_or_bytecode(
    ctx: &mut NativeContext<'_, '_>,
    leaf: Option<&RegisterLeaf>,
    target: Word,
    args_ptr: *const usize,
    argc: usize,
) -> Result<NativeReturn, Fault> {
    // SAFETY: Caller guarantees `args_ptr` holds `argc` Lisp words when `argc > 0`.
    let args = unsafe { raw_slice(args_ptr, argc) };
    if let Some(word) =
        leaf.and_then(|leaf| leaf.call(ctx.runtime, target, |i| Word::from_raw(args[i])))
    {
        return Ok(NativeReturn::One(word));
    }
    let runtime = ctx.runtime;
    run_leaf_bytecode(runtime, ctx.exec_mut(), target, args)
}

fn run_leaf_bytecode<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    target: Word,
    args: &[usize],
) -> Result<NativeReturn, Fault> {
    vm::deopt_into_vm(runtime, exec, target, 0, &[0, 1, 2, 3][..args.len()], args)
}

/// Handle `TAG_FAULT`, `TAG_TAIL_CALL`, and `TAG_VM_RESUME` after a direct C-ABI call.
///
/// When calling a child frame, pass `Some(child_ctx)` to drain its pending fault/tail-call;
/// for validated self-reentry on the caller's own context, pass `None`.
#[inline]
unsafe fn finish_direct_call_return(
    parent: *mut JitCallContext,
    ret: JitReturn,
    callee: Option<&mut JitCallContext>,
) -> JitReturn {
    match ret.tag {
        JitReturn::TAG_FAULT => {
            let fault = callee
                .unwrap_or_else(|| unsafe { &mut *parent })
                .take_fault();
            unsafe { dispatch_jit_call_fault(parent, fault) }
        }
        JitReturn::TAG_TAIL_CALL => {
            let tail = callee
                .unwrap_or_else(|| unsafe { &mut *parent })
                .take_tail_call();
            unsafe { finish_direct_tail_call(parent, tail) }
        }
        JitReturn::TAG_VM_RESUME => {
            unsafe { (*parent).resume_after_call = true };
            ret
        }
        _ => ret,
    }
}

#[inline]
pub(super) unsafe fn call_direct_entry(
    ctx: *mut JitCallContext,
    executable: Rc<NativeCode>,
    target: usize,
    args_ptr: *const usize,
    argc: usize,
) -> JitReturn {
    let jit_ctx = unsafe { &mut *ctx };
    if !jit_ctx.slice_steps.is_null()
        && (!executable.fits_slice() || !unsafe { jit_ctx.charge_slice() })
    {
        return JitReturn::control(JitReturn::TAG_VM_CALL);
    }
    // SAFETY: The outer trampoline keeps the thread's depth counter borrowed.
    let depth = unsafe { &*jit_ctx.frame_depth };
    let Some(_guard) = enter_jit_frame(depth) else {
        return unsafe { dispatch_jit_call_fault(ctx, jit_stack_exhausted()) };
    };
    if let Some(leaf) = &executable.leaf {
        // SAFETY: Caller guarantees `args_ptr` holds `argc` words for `argc > 0`.
        let args = unsafe { raw_slice(args_ptr, argc) };
        let runtime = unsafe { jit_ctx.runtime_ref() };
        let target = Word::from_raw(target);
        if let Some(word) = leaf.call(runtime, target, |i| Word::from_raw(args[i])) {
            return JitReturn::from_native(NativeReturn::One(word));
        }
        if !executable.has_leaf_fallback()
            || !jit_ctx.remaining_steps.is_null()
            || runtime.profiling_enabled()
        {
            if jit_ctx.allow_handoff {
                return JitReturn::control(JitReturn::TAG_VM_CALL);
            }
            let (runtime, exec) = unsafe { jit_ctx.runtime_and_exec_mut() };
            return match run_leaf_bytecode(runtime, exec, target, args) {
                Ok(ret) => JitReturn::from_native(ret),
                Err(fault) => unsafe { dispatch_jit_call_fault(ctx, fault) },
            };
        }
    }
    let entry = executable.c_entry.expect("direct C-ABI call");
    let runtime = unsafe { jit_ctx.runtime_ref() };
    let code = runtime.heap.code_store.find(executable.code_id);
    let mut child_ctx = jit_ctx.child_context(entry, executable.resumable, code);
    let mut ret = JitReturn::zeroed();
    unsafe { entry.invoke(&mut child_ctx, target, args_ptr, argc, &mut ret) };
    unsafe { finish_direct_call_return(ctx, ret, Some(&mut child_ctx)) }
}

/// Re-enter the currently executing JIT function through a validated self-call cache hit.
///
/// The active native frame keeps the entry and function identity alive. Faults remain in `pending_fault`
/// until this call boundary, preserving the same unwind handoff as a child-context direct call.
#[inline]
pub(super) unsafe fn call_current_entry(
    ctx: *mut JitCallContext,
    entry: JitCEntry,
    target: usize,
    args_ptr: *const usize,
    argc: usize,
    out_ret: *mut JitReturn,
) {
    if !unsafe { (*ctx).charge_slice() } {
        unsafe { write_return(out_ret, JitReturn::control(JitReturn::TAG_VM_CALL)) };
        return;
    }
    // SAFETY: The outer trampoline keeps the thread's depth counter borrowed.
    let depth = unsafe { &*(*ctx).frame_depth };
    let Some(_guard) = enter_jit_frame(depth) else {
        let fault = unsafe { dispatch_jit_call_fault(ctx, jit_stack_exhausted()) };
        unsafe { write_return(out_ret, fault) };
        return;
    };
    unsafe { entry.invoke(ctx, target, args_ptr, argc, out_ret) };
    let ret = unsafe { out_ret.read() };
    let finished = unsafe { finish_direct_call_return(ctx, ret, None) };
    unsafe { write_return(out_ret, finished) };
}

#[cold]
unsafe fn finish_direct_tail_call(
    ctx: *mut JitCallContext,
    tail: Result<NativeTailCall, Fault>,
) -> JitReturn {
    if unsafe { (*ctx).allow_handoff } {
        return match tail {
            Ok(tail) => unsafe {
                let exec = &mut *(*ctx).exec;
                debug_assert!(exec.native_tail.is_none());
                exec.native_tail = Some((tail, None));
                (*ctx).resume_after_call = true;
                JitReturn::control(JitReturn::TAG_VM_RESUME)
            },
            Err(fault) => unsafe { dispatch_jit_call_fault(ctx, fault) },
        };
    }
    // A non-tail native caller is waiting for the result. Its callee has
    // returned and released its roots, so the VM can drive the tail chain.
    let mut result = JitReturn::zeroed();
    unsafe {
        write_native_result(ctx, &mut result, |context| {
            invoke_native_tail(context, tail?)
        });
    }
    result
}

#[cold]
unsafe fn dispatch_jit_call_fault(ctx: *mut JitCallContext, fault: Fault) -> JitReturn {
    let ctx = unsafe { &mut *ctx };
    if fault.host.kind() == crate::EvalErrorKind::Cancelled {
        return fault_to_jit_return(&mut ctx.pending_fault, fault);
    }
    let runtime = unsafe { &*ctx.runtime };
    let exec = unsafe { &mut *ctx.exec };
    match vm::dispatch_fault_from_native(runtime, exec, fault) {
        Ok(ret) => JitReturn::from_native(ret),
        Err(fault) => fault_to_jit_return(&mut ctx.pending_fault, fault),
    }
}

pub(super) unsafe fn try_cache_self_reentry(
    ctx: *mut JitCallContext,
    key: [usize; 2],
    epochs: [usize; 3],
    argc: usize,
) -> Option<JitCEntry> {
    // SAFETY: Caller guarantees `ctx` is uniquely borrowed at the call boundary.
    let context = unsafe { &*ctx };
    let entry = context.current_entry?;
    let cache = context.self_call?;
    (cache.key == key && cache.epochs == epochs && cache.argc == argc).then_some(entry)
}

pub(super) unsafe fn record_self_call(
    ctx: *mut JitCallContext,
    key: [usize; 2],
    epochs: [usize; 3],
    argc: usize,
) {
    // SAFETY: Caller guarantees `ctx` is uniquely borrowed at the call boundary.
    unsafe { &mut *ctx }.self_call = Some(CachedSelfCall { epochs, key, argc });
}
