use std::rc::Rc;

use zkcl_bytecode::native_abi::{JIT_CALL_PRIMARY_SLOW, JIT_CALL_PRIMARY_VALUE};

use super::super::call_cache::{
    CachedCall, CallArguments, CallCacheEntry, NAMED_CALL_CACHE, VALUE_CALL_CACHE, call_cache_slot,
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
    if let Some(result) = vm::Primitive::try_native_word_call(ctx.runtime, target, words, || true) {
        // No fast operation can collect while using the raw argument words.
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
    // The fast helpers cannot collect, invoke user code, or signal for these operands.
    // SAFETY: Caller guarantees `args` came from the active native call frame.
    let runtime = unsafe { context.runtime_ref() };
    if let Some(result) = vm::Primitive::try_native_word_call(runtime, target, args, || unsafe {
        context.charge_slice()
    }) {
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
    args: &[usize],
    at: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    let context = unsafe { &mut *ctx };
    let runtime = unsafe { context.runtime_ref() };
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

#[inline]
unsafe fn write_call_return<const PRIMARY: bool>(out: *mut JitReturn, ret: JitReturn) -> usize {
    if PRIMARY
        && ret.tag == JitReturn::TAG_ONE
        && ret.single_value != JIT_CALL_PRIMARY_SLOW as usize
    {
        return ret.single_value;
    }
    unsafe { write_return(out, ret) };
    JIT_CALL_PRIMARY_SLOW as usize
}

/// Read only the initialized success fields after a call that writes its own return record.
#[inline]
unsafe fn call_return<const PRIMARY: bool>(out: *mut JitReturn) -> usize {
    if PRIMARY && unsafe { std::ptr::addr_of!((*out).tag).read() } == JitReturn::TAG_ONE {
        return unsafe { std::ptr::addr_of!((*out).single_value).read() };
    }
    JIT_CALL_PRIMARY_SLOW as usize
}

unsafe fn try_standard_object_call(
    ctx: *mut JitCallContext,
    runtime: &Runtime,
    target: Word,
    args: &[usize],
) -> Option<JitReturn> {
    let argument_at = |index| Word::from_raw(args[index]);
    if let Some(value) = vm::instances::try_standard_slot_primitive(
        runtime,
        target,
        args.len(),
        argument_at,
        || unsafe { (&mut *ctx).charge_slice() },
    ) {
        crate::metrics::record_primitive_call();
        unsafe { (&mut *ctx).exec_mut() }.values.resize(0);
        return Some(JitReturn::from_native(NativeReturn::One(value)));
    }
    let primary =
        vm::method_combination::cached_direct_primary(runtime, target, args.len(), argument_at)?;
    // The method arguments below live in a local array. Only a register leaf
    // may inspect them because it cannot allocate or enter Lisp. A general
    // method must receive a registered argument frame before any safepoint.
    if args.len() > 2 {
        return None;
    }
    let arity = 2 + args.len();
    let (executable, _) = direct_entry(runtime, primary, arity)?;
    let leaf = executable.leaf.as_ref()?;
    if !unsafe { (&*ctx).charge_slice() } {
        return None;
    }
    let mut method_args = [Word::NIL.raw(); 4];
    method_args[2..arity].copy_from_slice(args);
    // SAFETY: We checked the method's ABI and arity. The leading NILs are
    // the two unused next-method arguments of the standard method convention.
    let result = leaf.call(runtime, primary, |index| Word::from_raw(method_args[index]));
    result.map(|word| {
        crate::metrics::record_primitive_call();
        unsafe { (&mut *ctx).exec_mut() }.values.resize(0);
        JitReturn::from_native(NativeReturn::One(word))
    })
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
    let args = CallArguments::Slice(unsafe { raw_slice(args_ptr, argc) });
    unsafe { call_value::<false>(ctx, target, args, at, out_ret) };
}

#[inline]
unsafe fn call_value<const PRIMARY: bool>(
    ctx: *mut JitCallContext,
    target: usize,
    args: CallArguments<'_>,
    at: usize,
    out_ret: *mut JitReturn,
) -> usize {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    if let Some(frame) = unsafe { (&mut *ctx).exec_mut() }.frames.last_mut() {
        frame.record_native_offset(at);
    }
    let runtime = unsafe { (&*ctx).runtime_ref() };
    let epochs = CallCacheEntry::epochs(runtime);
    let target_word = Word::from_raw(target);
    let slot = call_cache_slot(target, 0);
    let key = [target, 0];
    let argc = args.len();
    let cached =
        VALUE_CALL_CACHE.with(|cache| cache[slot].borrow().resolve(epochs, key, argc, None));
    if let Some(CachedCall::Native { target, executable }) = cached {
        // SAFETY: Cache hit verified compatible epochs, key, and arity.
        let ret = unsafe { call_direct_entry(ctx, executable, target, args) };
        return unsafe { write_call_return::<PRIMARY>(out_ret, ret) };
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
        // SAFETY: Direct entry verified compatible arity.
        let ret = unsafe { call_direct_entry(ctx, executable, target, args) };
        return unsafe { write_call_return::<PRIMARY>(out_ret, ret) };
    }
    args.with_slice(|args| unsafe {
        if let Some(ret) = try_standard_object_call(ctx, runtime, target_word, args) {
            return write_call_return::<PRIMARY>(out_ret, ret);
        }
        finish_fallback_call(ctx, out_ret, Ok(target_word), args, at);
        call_return::<PRIMARY>(out_ret)
    })
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
    let args = CallArguments::Slice(unsafe { raw_slice(args_ptr, argc) });
    unsafe { call_named::<false>(ctx, fn_word, sym_idx, args, at, out_ret) };
}

#[inline]
unsafe fn call_named<const PRIMARY: bool>(
    ctx: *mut JitCallContext,
    fn_word: usize,
    sym_idx: usize,
    args: CallArguments<'_>,
    at: usize,
    out_ret: *mut JitReturn,
) -> usize {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    let context = unsafe { &mut *ctx };
    if let Some(frame) = unsafe { context.exec_mut() }.frames.last_mut() {
        frame.record_native_offset(at);
    }
    let runtime = unsafe { context.runtime_ref() };
    let epochs = CallCacheEntry::epochs(runtime);
    let key = [fn_word, sym_idx];
    let argc = args.len();
    let current_entry = context.current_entry;
    if let Some(entry) = unsafe { try_cache_self_reentry(ctx, key, epochs, argc) } {
        return unsafe { call_current_entry::<PRIMARY>(ctx, entry, fn_word, args, out_ret) };
    }
    let slot = call_cache_slot(fn_word, sym_idx);
    let direct_self = current_entry.map(|_| fn_word);
    let cached =
        NAMED_CALL_CACHE.with(|cache| cache[slot].borrow().resolve(epochs, key, argc, direct_self));
    match cached {
        Some(CachedCall::CurrentEntry) => {
            let entry = current_entry.expect("current entry selected by cache guard");
            unsafe { record_self_call(ctx, key, epochs, argc) };
            return unsafe { call_current_entry::<PRIMARY>(ctx, entry, fn_word, args, out_ret) };
        }
        Some(CachedCall::Native { target, executable }) => {
            let ret = unsafe { call_direct_entry(ctx, executable, target, args) };
            return unsafe { write_call_return::<PRIMARY>(out_ret, ret) };
        }
        None => {}
    }
    let sym = native_function_constant(runtime, Word::from_raw(fn_word), sym_idx);
    let callee = vm::function_cell(runtime, sym, at);
    if callee.is_err() && !context.allow_handoff {
        let arguments = runtime.root_frame(argc);
        args.with_slice(|args| {
            for (index, &bits) in args.iter().enumerate() {
                arguments.set(index, Word::from_raw(bits));
            }
        });
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
        return unsafe { call_return::<PRIMARY>(out_ret) };
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
            return unsafe { call_current_entry::<PRIMARY>(ctx, entry, fn_word, args, out_ret) };
        }
        let ret = unsafe { call_direct_entry(ctx, executable, callee.raw(), args) };
        return unsafe { write_call_return::<PRIMARY>(out_ret, ret) };
    }
    args.with_slice(|args| unsafe {
        if let Ok(callee) = callee
            && let Some(ret) = try_standard_object_call(ctx, runtime, callee, args)
        {
            return write_call_return::<PRIMARY>(out_ret, ret);
        }
        finish_fallback_call(ctx, out_ret, callee, args, at);
        call_return::<PRIMARY>(out_ret)
    })
}

/// Invoke a call with at most two scalar arguments and return its primary word in a register.
/// `JIT_CALL_PRIMARY_SLOW` indicates that the complete structured result is in `out`.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `argc` must be 0–2, with the unused arguments
/// padded with NIL. For a named call, `target` is the caller and `symbol` is its constant-pool
/// index for the callee's symbol. Otherwise `symbol` is `JIT_CALL_PRIMARY_VALUE`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_call_primary(
    ctx: *mut JitCallContext,
    target: usize,
    symbol: usize,
    at: usize,
    argc: usize,
    a: usize,
    b: usize,
    out: *mut JitReturn,
) -> usize {
    if argc > 2 {
        unsafe { write_return(out, JitReturn::control(JitReturn::TAG_VM_CALL)) };
        return JIT_CALL_PRIMARY_SLOW as usize;
    }
    let args = CallArguments::Pair { a, b, len: argc };
    if symbol == JIT_CALL_PRIMARY_VALUE as usize {
        unsafe { call_value::<true>(ctx, target, args, at, out) }
    } else {
        unsafe { call_named::<true>(ctx, target, symbol, args, at, out) }
    }
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

/// Verify the function identity captured by a call transform.
///
/// # Safety
///
/// Follow the [`JitCallContext`] contract. `function` must be a live Lisp
/// function or closure and `symbol` and `binding` are its constant indices.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_check_transform_binding(
    ctx: *mut JitCallContext,
    function: usize,
    symbol: usize,
    binding: usize,
) -> bool {
    // SAFETY: The caller provides a live native context.
    let Some(runtime) = (unsafe { JitCallContext::try_runtime(ctx) }) else {
        return false;
    };
    let function = Word::from_raw(function);
    let marker = native_function_constant(runtime, function, binding);
    let symbol = native_function_constant(runtime, function, symbol);
    let expected = if let Some(descriptor) = marker.as_fixnum() {
        runtime.vops.binding(runtime, symbol, descriptor)
    } else {
        runtime.heap.cons_car(marker)
    };
    let Some(expected) = expected.filter(|&function| function != Word::NIL) else {
        return false;
    };
    runtime.heap.symbol_cell(symbol, true) == Some(expected)
}

/// Read a Lisp integer in the unsigned 64-bit range without allocating.
///
/// # Safety
///
/// Follow the [`JitCallContext`] contract. `input` is a live Lisp word and
/// `out` is an aligned, writable `u64` slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_unbox_u64(
    ctx: *mut JitCallContext,
    input: usize,
    out: *mut u64,
) -> bool {
    // SAFETY: The caller provides a live native context.
    let Some(runtime) = (unsafe { JitCallContext::try_runtime(ctx) }) else {
        return false;
    };
    let Some(bits) = runtime.heap.unsigned_integer64(Word::from_raw(input)) else {
        return false;
    };
    // SAFETY: The caller supplies a writable, aligned u64 slot.
    unsafe { out.write(bits) };
    true
}
