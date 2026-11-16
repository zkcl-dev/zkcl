use std::rc::Rc;

use super::super::call_cache::{
    CachedCall, CallCacheEntry, NAMED_CALL_CACHE, VALUE_CALL_CACHE, call_cache_slot,
    call_current_entry, call_direct_entry, direct_entry, record_self_call, try_cache_self_reentry,
};
use super::super::context::{raw_slice, write_abi_result, write_native_result, write_return};
use super::super::{
    Fault, JitCallContext, JitReturn, NativeContext, NativeReturn, Word, native_error,
    native_function_constant, native_invoke,
};
use crate::heap::RootStackFrame;
use crate::{Runtime, vm};

#[inline]
fn needs_vm_call(runtime: &Runtime, target: Word) -> bool {
    target.native_index().is_none_or(|index| {
        !matches!(
            &runtime.natives.borrow().0[index],
            crate::RegisteredFunction::Sync(_) | crate::RegisteredFunction::Host(_)
        )
    })
}

fn try_machine_comparison_words(runtime: &Runtime, target: Word, words: &[usize]) -> Option<Word> {
    let &[left, right] = words else {
        return None;
    };
    vm::numbers::try_machine_comparison(
        &runtime.heap,
        target,
        [Word::from_raw(left), Word::from_raw(right)],
    )
}

fn stage_arguments<'rt>(runtime: &'rt Runtime, words: &[usize]) -> RootStackFrame<'rt> {
    let staged = runtime.staging_stack.alloc(words.len());
    for (index, &raw) in words.iter().enumerate() {
        staged.set(index, Word::from_raw(raw));
    }
    staged
}

fn invoke_with_arg_slice(
    ctx: &mut NativeContext<'_, '_>,
    target: Word,
    words: &[usize],
    at: usize,
) -> Result<NativeReturn, Fault> {
    if let Some(result) = try_machine_comparison_words(ctx.runtime, target, words) {
        // Machine comparisons only read operands and cannot allocate or collect.
        crate::metrics::record_primitive_call();
        ctx.clear_values();
        return Ok(NativeReturn::One(result));
    }
    let staged = stage_arguments(ctx.runtime, words);
    native_invoke(ctx, target, &staged, 0..words.len(), at)
}

fn unresolved_call(
    context: &mut JitCallContext,
    target: Word,
    args: &[usize],
    at: usize,
) -> JitReturn {
    // The guard accepts only built-in comparisons that cannot allocate, invoke
    // user code or signal for these exact operands.
    // SAFETY: Caller guarantees `args` came from the active native call frame.
    let runtime = unsafe { context.runtime_ref() };
    if let Some(result) = try_machine_comparison_words(runtime, target, args) {
        crate::metrics::record_primitive_call();
        // SAFETY: `&mut JitCallContext` grants exclusive access before returning.
        unsafe { context.exec_mut() }.values.resize(0);
        return JitReturn::from_native(NativeReturn::One(result));
    }
    if let Some(op) = vm::immediate_native_primitive(runtime, target, args)
        && unsafe { context.charge_slice() }
    {
        // SAFETY: This immediate primitive cannot call back into the executor.
        let (runtime, exec) = unsafe { context.runtime_and_exec_mut() };
        let staged = stage_arguments(runtime, args);
        let values = &mut exec.values;
        if vm::primitive(runtime, &staged, op, 0..args.len(), values, at).is_ok() {
            crate::metrics::record_primitive_call();
            return JitReturn::from_native(values.take_native_return());
        }
    }
    JitReturn::control(JitReturn::TAG_VM_CALL)
}

/// Resolve a cache miss/handoff call through slice charging and `invoke_with_arg_slice`.
unsafe fn finish_fallback_call(
    ctx: *mut JitCallContext,
    out_ret: *mut JitReturn,
    callee: Result<Word, Fault>,
    args_ptr: *const usize,
    argc: usize,
    at: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `args_ptr`/`out_ret` contracts.
    let context = unsafe { &mut *ctx };
    let runtime = unsafe { context.runtime_ref() };
    let args = unsafe { raw_slice(args_ptr, argc) };
    if context.allow_handoff {
        match &callee {
            Ok(target) if needs_vm_call(runtime, *target) => {
                unsafe { write_return(out_ret, unresolved_call(context, *target, args, at)) };
                return;
            }
            Err(_) => {
                unsafe { write_return(out_ret, JitReturn::control(JitReturn::TAG_VM_CALL)) };
                return;
            }
            _ => {}
        }
    }
    if !unsafe { context.charge_slice() } {
        unsafe { write_return(out_ret, JitReturn::control(JitReturn::TAG_VM_CALL)) };
        return;
    }
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| match callee {
            Ok(target) => invoke_with_arg_slice(native_ctx, target, args, at),
            Err(fault) => native_error(native_ctx, fault),
        });
    }
}

/// Invoke a Lisp callable with arguments from native code.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. For positive `argc`, `args_ptr` must supply that
/// many Lisp words, including when the call goes directly to a compiled entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_invoke(
    ctx: *mut JitCallContext,
    target: usize,
    args_ptr: *const usize,
    argc: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    if let Some(frame) = unsafe { (&mut *ctx).exec_mut() }.frames.last_mut() {
        frame.record_native_offset(at);
    }
    let runtime = unsafe { (&*ctx).runtime_ref() };
    let epochs = CallCacheEntry::epochs(runtime);
    let target_word = Word::from_raw(target);
    let slot = call_cache_slot(target, 0);
    let key = [target, 0];
    let cached =
        VALUE_CALL_CACHE.with(|cache| cache[slot].borrow().resolve(epochs, key, argc, None));
    if let Some(CachedCall::Native { target, executable }) = cached {
        // SAFETY: Cache hit verified compatible epochs, key, and arity.
        let ret = unsafe { call_direct_entry(ctx, executable, target, args_ptr, argc) };
        unsafe { write_return(out_ret, ret) };
        return;
    }
    if let Some((executable, arity)) = direct_entry(runtime, target_word, argc) {
        VALUE_CALL_CACHE.with(|cache| {
            cache[slot].replace(CallCacheEntry {
                epochs,
                key,
                target,
                executable: Rc::downgrade(&executable),
                arity,
                can_reenter: executable.leaf.is_none(),
            });
        });
        // SAFETY: Direct entry verified compatible arity for `args_ptr[..argc]`.
        let ret = unsafe { call_direct_entry(ctx, executable, target, args_ptr, argc) };
        unsafe { write_return(out_ret, ret) };
        return;
    }
    unsafe { finish_fallback_call(ctx, out_ret, Ok(target_word), args_ptr, argc, at) };
}

/// Resolve a named call through a function constant and invoke its current binding.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `fn_word` must be the current function or closure,
/// and `sym_idx` must select its callee symbol constant. For positive `argc`, `args_ptr` must
/// supply that many Lisp words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_call_named(
    ctx: *mut JitCallContext,
    fn_word: usize,
    sym_idx: usize,
    args_ptr: *const usize,
    argc: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    let context = unsafe { &mut *ctx };
    if let Some(frame) = unsafe { context.exec_mut() }.frames.last_mut() {
        frame.record_native_offset(at);
    }
    let runtime = unsafe { context.runtime_ref() };
    let epochs = CallCacheEntry::epochs(runtime);
    let key = [fn_word, sym_idx];
    let current_entry = context.current_entry;
    if let Some(entry) = unsafe { try_cache_self_reentry(ctx, key, epochs, argc) } {
        unsafe { call_current_entry(ctx, entry, fn_word, args_ptr, argc, out_ret) };
        return;
    }
    let slot = call_cache_slot(fn_word, sym_idx);
    let direct_self = current_entry.map(|_| fn_word);
    let cached =
        NAMED_CALL_CACHE.with(|cache| cache[slot].borrow().resolve(epochs, key, argc, direct_self));
    match cached {
        Some(CachedCall::CurrentEntry) => {
            let entry = current_entry.expect("current entry selected by cache guard");
            unsafe { record_self_call(ctx, key, epochs, argc) };
            unsafe { call_current_entry(ctx, entry, fn_word, args_ptr, argc, out_ret) };
            return;
        }
        Some(CachedCall::Native { target, executable }) => {
            let ret = unsafe { call_direct_entry(ctx, executable, target, args_ptr, argc) };
            unsafe { write_return(out_ret, ret) };
            return;
        }
        None => {}
    }
    let sym = native_function_constant(runtime, Word::from_raw(fn_word), sym_idx);
    let callee = vm::function_cell(runtime, sym, at);
    if callee.is_err() && !context.allow_handoff {
        let arguments = runtime.root_frame(argc);
        for (index, &bits) in unsafe { raw_slice(args_ptr, argc) }.iter().enumerate() {
            arguments.set(index, Word::from_raw(bits));
        }
        unsafe {
            write_native_result(ctx, out_ret, |native| {
                vm::recovery::call_from_native(
                    native.runtime,
                    native.exec_mut(),
                    sym,
                    &arguments,
                    0..argc,
                    at,
                )
            });
        }
        return;
    }
    if let Ok(callee) = callee
        && let Some((executable, arity)) = direct_entry(runtime, callee, argc)
    {
        NAMED_CALL_CACHE.with(|cache| {
            cache[slot].replace(CallCacheEntry {
                epochs,
                key,
                target: callee.raw(),
                executable: Rc::downgrade(&executable),
                arity,
                can_reenter: executable.leaf.is_none(),
            });
        });
        if executable.leaf.is_none()
            && callee.raw() == fn_word
            && let Some(entry) = current_entry
        {
            unsafe { record_self_call(ctx, key, epochs, argc) };
            unsafe { call_current_entry(ctx, entry, fn_word, args_ptr, argc, out_ret) };
            return;
        }
        let ret = unsafe { call_direct_entry(ctx, executable, callee.raw(), args_ptr, argc) };
        unsafe { write_return(out_ret, ret) };
        return;
    }
    unsafe { finish_fallback_call(ctx, out_ret, callee, args_ptr, argc, at) };
}

/// Invoke a Lisp callable with the current multiple values as arguments.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_invoke_values(
    ctx: *mut JitCallContext,
    callee: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_abi_result(ctx, out_ret, |scope| {
            if scope.allow_handoff {
                return Ok(JitReturn::control(JitReturn::TAG_VM_CALL));
            }
            let argc = scope.native.values_len();
            let staged = scope.native.runtime.staging_stack.alloc(argc);
            for i in 0..argc {
                staged.set(i, scope.native.value(i));
            }
            scope.native.clear_values();
            native_invoke(
                &mut scope.native,
                Word::from_raw(callee),
                &staged,
                0..argc,
                at,
            )
            .map(JitReturn::from_native)
        });
    }
}

/// Verify a primitive binding before bypassing a named call.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `target` is the current function or closure,
/// and `symbol` is the name's constant-pool index. The operation identifies the
/// expected primitive using the optimizer's `GuardedPrimitive::abi_index`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_is_primitive_binding(
    ctx: *mut JitCallContext,
    target: usize,
    symbol: usize,
    op: usize,
) -> bool {
    // SAFETY: Caller guarantees `ctx` is live; null `runtime` returns `false`.
    let Some(runtime) = (unsafe { JitCallContext::try_runtime(ctx) }) else {
        return false;
    };
    let Some(expected) = vm::Primitive::guarded_binding(op) else {
        return false;
    };
    let symbol = native_function_constant(runtime, Word::from_raw(target), symbol);
    runtime.heap.symbol_cell(symbol, true) == Some(expected)
}

/// Verify the load-time function binding and borrow an integer's bits without allocation.
///
/// # Safety
///
/// Follow the [`JitCallContext`] contract. `function` and `input` must be live Lisp
/// words, `symbol` and `binding` are constant indices in `function`, and `out`
/// must accept a `u64` write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_prepare_vop_u64(
    ctx: *mut JitCallContext,
    function: usize,
    symbol: usize,
    binding: usize,
    input: usize,
    out: *mut u64,
) -> bool {
    // SAFETY: The caller provides a live native context.
    let Some(runtime) = (unsafe { JitCallContext::try_runtime(ctx) }) else {
        return false;
    };
    let function = Word::from_raw(function);
    let marker = native_function_constant(runtime, function, binding);
    let symbol = native_function_constant(runtime, function, symbol);
    let Some(expected) = runtime.heap.symbol_cell(marker, false) else {
        return false;
    };
    if runtime.heap.symbol_cell(symbol, true) != Some(expected) {
        return false;
    }
    let Some(bits) = runtime.heap.unsigned_integer64(Word::from_raw(input)) else {
        return false;
    };
    // SAFETY: The caller supplies a writable, aligned u64 slot.
    unsafe { out.write(bits) };
    true
}
