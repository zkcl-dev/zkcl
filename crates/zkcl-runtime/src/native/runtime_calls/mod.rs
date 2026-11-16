//! Runtime entry points called by JIT code through function pointers and by linked AOT objects by name.

mod calls;
mod cells;
mod control;
mod deopt_float;
mod frames;

pub use calls::*;
pub use cells::*;
pub use control::*;
pub use deopt_float::*;
pub use frames::*;

use std::sync::atomic::AtomicUsize;

use zkcl_bytecode::Class;
use zkcl_bytecode::native_abi::{LEAF_ARGUMENTS, NATIVE_CALL_ABI_REVISION, RuntimeAbiFunction};

use super::argument_binding::*;
use super::array::zkcl_rt_read_float_vector;
use super::array_write::zkcl_rt_write_float_vector;
use super::call_cache::call_leaf_or_bytecode;
use super::context::{
    raw_slice, write_abi_result, write_fault_return, write_native_result, write_return,
};
use super::debugging::*;
use super::simd::*;
use super::{
    Fault, JitCallContext, JitReturn, NativeReturn, NativeTailCall, Word, WordLayout,
    native_define_function, native_error, native_function_constant,
};
use crate::{EvalError, vm};

// A typed function pointer checks the runtime implementation against each
// signature before any backend can obtain an erased address for codegen.
macro_rules! rust_type {
    (Word) => { usize };
    (Bool) => { bool };
    (I64) => { u64 };
    (Context) => { *mut JitCallContext };
    (ResultOut) => { *mut JitReturn };
    (Words) => { *const usize };
    (WordOut) => { *mut usize };
    (RootSlotsOut) => { *mut *const AtomicUsize };
    (Registers) => { *const u32 };
    (WideWords) => { *const u64 };
    (WideWordOut) => { *mut u64 };
    (Formats) => { *const u8 };
    (Packs) => { *const [u32; 4] };
    (PackOut) => { *mut [u32; 4] };
}

macro_rules! declare_runtime_function_table {
    ($($function:ident = $id:literal, $field:ident => $symbol:ident(
        $($param:ident: $kind:ident),* $(,)?
    ) $(-> $result:ident)?;)*) => {
        /// Typed C-ABI entry points available to in-process native compilers.
        #[repr(C)]
        pub struct JitRuntimeFunctions {
            pub abi_version: u32,
            pub pointer_width_bits: u8,
            $(pub $field: unsafe extern "C" fn($($param: rust_type!($kind)),*) $(-> rust_type!($result))?,)*
        }

        impl JitRuntimeFunctions {
            /// Runtime address to embed in machine code using this function's ABI signature.
            pub fn address(&self, function: RuntimeAbiFunction) -> usize {
                match function {
                    $(RuntimeAbiFunction::$function => self.$field as usize,)*
                }
            }
        }

        static JIT_RUNTIME_FUNCTIONS: JitRuntimeFunctions = JitRuntimeFunctions {
            abi_version: NATIVE_CALL_ABI_REVISION,
            pointer_width_bits: WordLayout::POINTER_WIDTH_BITS,
            $($field: $symbol,)*
        };
    };
}

zkcl_bytecode::native_runtime_functions!(declare_runtime_function_table);

/// Return the runtime function pointers used by JIT backends.
pub fn jit_runtime_functions() -> &'static JitRuntimeFunctions {
    &JIT_RUNTIME_FUNCTIONS
}

/// Test the current function binding without calling Lisp or collecting.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `symbol` indexes a symbol in the
/// current function object's constant pool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_is_self_call(
    ctx: *mut JitCallContext,
    function: usize,
    symbol: usize,
) -> bool {
    // SAFETY: Caller upholds the `JitCallContext` contract and constant-pool index.
    let runtime = unsafe { (&*ctx).runtime_ref() };
    let function = Word::from_raw(function);
    let symbol = native_function_constant(runtime, function, symbol);
    runtime.heap.symbol_cell(symbol, true) == Some(function)
}

/// Preserve a callee and its arguments until the generated activation returns.
/// The caller then returns `TAG_TAIL_CALL`, after releasing its native roots.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. The caller has no active control
/// scopes of its own. When `use_values` is false, `args` must supply `argc` words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_stage_tail_call(
    ctx: *mut JitCallContext,
    callee: usize,
    args: *const usize,
    argc: usize,
    at: usize,
    use_values: bool,
) {
    // SAFETY: Caller guarantees an active `JitCallContext` and valid `args` slice when `!use_values`.
    let context = unsafe { &mut *ctx };
    let runtime_ptr = context.runtime;
    let slots = if use_values {
        // SAFETY: Caller passes control exclusively to this entry point, and `runtime_ptr`
        // points to the immutable `Runtime` outliving the call.
        let runtime = unsafe { &*runtime_ptr };
        let values = &mut unsafe { context.exec_mut() }.values;
        let slots = runtime.root_frame(values.len + 1);
        for index in 0..values.len {
            slots.set(index + 1, values.get(index));
        }
        values.resize(0);
        slots
    } else {
        // SAFETY: Caller guarantees `args` holds `argc` initialized words.
        let runtime = unsafe { &*runtime_ptr };
        let words = unsafe { raw_slice(args, argc) };
        let slots = runtime.root_frame(argc + 1);
        for (index, &word) in words.iter().enumerate() {
            slots.set(index + 1, Word::from_raw(word));
        }
        slots
    };
    slots.set(0, Word::from_raw(callee));
    context.pending_tail_call = Some(NativeTailCall { slots, at });
}

/// Generic ABI entry for a register leaf whose preconditions require a live runtime.
/// The VM dispatches the resolved function through its leaf guards and executes its
/// ordinary bytecode if any guard fails.
///
/// # Safety
///
/// Follow the [`JitCallContext`] and [`JitCEntryFn`] argument and output contracts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_register_leaf_dispatch(
    ctx: *mut JitCallContext,
    target: usize,
    args_ptr: *const usize,
    argc: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Validated before dereferencing `runtime` or `exec`.
    if unsafe { JitCallContext::from_raw(ctx) }.is_none() {
        unsafe { write_fault_return(out_ret) };
        return;
    }
    // SAFETY: `ctx` was validated above; caller guarantees `args_ptr` and `out_ret`.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let target = Word::from_raw(target);
            let heap = &native_ctx.runtime.heap;
            let info = heap
                .function_code(target)
                .and_then(|code| heap.function_info(code))
                .ok_or_else(|| {
                    Fault::plain(
                        Class::ProgramError,
                        EvalError::new(0, "register entry called on non-function"),
                    )
                })?;
            let vm_code = heap.code_store.get(info.code_id);
            if argc > LEAF_ARGUMENTS
                || vm_code.arity.required != argc
                || vm_code.arity.maximum != Some(argc)
            {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(0, "wrong number of arguments for register entry"),
                ));
            }
            let executable = vm_code.native_entry();
            let leaf = executable.as_ref().and_then(|code| code.leaf.as_ref());
            call_leaf_or_bytecode(native_ctx, leaf, target, args_ptr, argc)
        });
    }
}

/// Poll for an execution stop and return any resulting Lisp unwind target.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. A detached context is also accepted. `ctx` itself
/// must point to a live context, and `out_ret` must remain writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_poll_execution(
    ctx: *mut JitCallContext,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller guarantees `ctx` points to a live context and `out_ret` is writable.
    if let Some(runtime) = unsafe { JitCallContext::try_runtime(ctx) } {
        runtime.profile_sample();
        if let Err(error) = runtime.check_execution_safepoint(at) {
            unsafe {
                write_native_result(ctx, out_ret, |native_ctx| {
                    native_error(native_ctx, error.into())
                });
            }
            return;
        }
    }
    if unsafe { JitCallContext::from_raw(ctx) }.is_none() {
        unsafe {
            write_return(
                out_ret,
                JitReturn::from_native(NativeReturn::One(Word::NIL)),
            )
        };
        return;
    }
    // SAFETY: `ctx` was validated non-detached above.
    unsafe {
        write_abi_result(ctx, out_ret, |scope| {
            if scope.allow_handoff && scope.native.exec_mut().host_debugging {
                // Sliced native loops can return to the VM at a recoverable
                // point. Leave the request pending for that boundary.
                return Ok(JitReturn::from_native(NativeReturn::One(Word::NIL)));
            }
            let runtime = scope.native.runtime;
            vm::debugging::native_poll_pause(runtime, scope.native.exec_mut(), at)
                .map(JitReturn::from_native)
        });
    }
}

/// Renew the native quantum without invoking Lisp, a host callback or GC.
/// A false result requests a handoff to the scheduler.
///
/// # Safety
/// The context must belong to a scheduled native activation with a live slice counter.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_refill_slice(ctx: *mut JitCallContext) -> bool {
    // SAFETY: Caller guarantees `ctx` belongs to a scheduled native activation.
    unsafe { (&*ctx).refill_slice() }
}

/// Install a function or macro definition for the supplied name.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_define_function(
    ctx: *mut JitCallContext,
    symbol: usize,
    definition: usize,
    is_macro: bool,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let symbol = Word::from_raw(symbol);
            native_define_function(
                native_ctx.runtime,
                symbol,
                Word::from_raw(definition),
                is_macro,
                at,
            )?;
            Ok(NativeReturn::One(symbol))
        });
    }
}
