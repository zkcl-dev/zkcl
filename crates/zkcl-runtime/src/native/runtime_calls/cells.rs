use zkcl_bytecode::Class;

use super::super::context::{raw_slice, with_native_ctx, write_abi_result, write_native_result};
use super::super::{
    Fault, JitCallContext, JitReturn, NativeReturn, Word, native_function_constant,
    native_read_box, native_read_cell, native_write_box, native_write_cell,
};
use crate::core_ir::InlineDeclaration;
use crate::{EvalError, vm};

/// Read a constant from the function underlying `fn_word`.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `fn_word` must be a function or closure, and
/// `index` must be within its constant pool.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_function_constant(
    ctx: *mut JitCallContext,
    fn_word: usize,
    index: usize,
) -> usize {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_function_constant(native_ctx.runtime, Word::from_raw(fn_word), index).raw()
        })
    }
}

/// Read the function or value binding of a symbol.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_read_cell(
    ctx: *mut JitCallContext,
    symbol: usize,
    function: bool,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_abi_result(ctx, out_ret, |scope| {
            let symbol = Word::from_raw(symbol);
            match native_read_cell(scope.native.runtime, symbol, function, at) {
                Ok(value) => Ok(JitReturn::from_native(NativeReturn::One(value))),
                // Looking up a cell has no Lisp effect until a condition is signalled.
                // Its handlers can call Lisp and wait just like any other open call.
                Err(_) if scope.allow_handoff => Ok(JitReturn::control(JitReturn::TAG_VM_CALL)),
                Err(_) => vm::recovery::read_from_native(
                    scope.native.runtime,
                    scope.native.exec_mut(),
                    symbol,
                    function,
                    at,
                )
                .map(JitReturn::from_native),
            }
        });
    }
}

/// Store a symbol's value binding.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_write_cell(ctx: *mut JitCallContext, symbol: usize, value: usize) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_write_cell(
                native_ctx.runtime,
                Word::from_raw(symbol),
                Word::from_raw(value),
            );
        })
    }
}

/// Proclaim a variable special and deliver declaration errors through Lisp control flow.
///
/// # Safety
/// Follow the [`JitCallContext`] call contract and pass a live Lisp symbol.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_proclaim_special(
    ctx: *mut JitCallContext,
    symbol: usize,
    at: usize,
    out: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out` contracts.
    unsafe {
        write_native_result(ctx, out, |ctx| {
            match ctx.runtime.proclaim_special(Word::from_raw(symbol), at) {
                Ok(()) => Ok(NativeReturn::One(Word::NIL)),
                Err(fault) => super::native_error(ctx, fault),
            }
        });
    }
}

/// Record an inline or notinline proclamation for a symbol's function name.
///
/// # Safety
/// Follow the [`JitCallContext`] call contract and pass a live Lisp symbol.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_proclaim_inline(
    ctx: *mut JitCallContext,
    symbol: usize,
    setf: bool,
    notinline: bool,
) {
    let declaration = if notinline {
        InlineDeclaration::NotInline
    } else {
        InlineDeclaration::Inline
    };
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx
                .runtime
                .proclaim_inline(Word::from_raw(symbol), setf, declaration);
        });
    }
}

/// Set a symbol's variable documentation.
///
/// # Safety
/// Follow the [`JitCallContext`] call contract. Pass a live symbol and documentation word.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_set_variable_documentation(
    ctx: *mut JitCallContext,
    symbol: usize,
    doc: usize,
) {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx
                .runtime
                .heap
                .set_variable_documentation(Word::from_raw(symbol), Word::from_raw(doc));
        });
    }
}

/// Test whether a symbol's current value cell is bound.
///
/// # Safety
/// Follow the [`JitCallContext`] call contract and pass a live Lisp symbol.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_is_symbol_bound(ctx: *mut JitCallContext, symbol: usize) -> bool {
    // SAFETY: Caller upholds the `JitCallContext` contract.
    unsafe {
        with_native_ctx(ctx, |native_ctx| {
            native_ctx
                .runtime
                .heap
                .symbol_cell(Word::from_raw(symbol), false)
                .is_some()
        })
    }
}

/// Read a capture, reporting a fault for an invalid closure or index.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_read_capture(
    ctx: *mut JitCallContext,
    closure_word: usize,
    index: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let word = native_ctx
                .runtime
                .heap
                .capture(Word::from_raw(closure_word), index)
                .ok_or_else(|| {
                    Fault::plain(
                        Class::ProgramError,
                        EvalError::new(at, "closure capture is out of bounds"),
                    )
                })?;
            Ok(NativeReturn::One(word))
        });
    }
}

/// Allocate a closure over a function constant and native captures.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract. `fn_word` must be a function or closure whose
/// `const_idx` constant is the compiled closure body. Use its capture count. For non-null
/// `captures_ptr`, supply `count` Lisp words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_alloc_closure(
    ctx: *mut JitCallContext,
    fn_word: usize,
    const_idx: usize,
    captures_ptr: *const usize,
    count: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller guarantees `captures_ptr` holds `count` words when non-null.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let code_word =
                native_function_constant(native_ctx.runtime, Word::from_raw(fn_word), const_idx);
            let staged = native_ctx.runtime.staging_stack.alloc(1 + count);
            staged.set(0, code_word);
            for (i, &w) in raw_slice(captures_ptr, count).iter().enumerate() {
                staged.set(1 + i, Word::from_raw(w));
            }
            let closure = native_ctx
                .runtime
                .heap
                .closure(count, || staged.get(0), |i| staged.get(1 + i))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            Ok(NativeReturn::One(closure))
        });
    }
}

/// Allocate a lexical cell, rooting its initial value across allocation.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_box_local(
    ctx: *mut JitCallContext,
    value: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let value = native_ctx.runtime.root_slot(Word::from_raw(value));
            let cell = native_ctx
                .runtime
                .heap
                .cell(|| value.get())
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            Ok(NativeReturn::One(cell))
        });
    }
}

/// Read a lexical cell, reporting a fault for a different object kind.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_read_box(
    ctx: *mut JitCallContext,
    cell: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            native_read_box(native_ctx.runtime, Word::from_raw(cell), at).map(NativeReturn::One)
        });
    }
}

/// Store into a lexical cell, reporting a fault for a different object kind.
///
/// # Safety
///
/// Follow the [`JitCallContext`] call contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zkcl_rt_write_box(
    ctx: *mut JitCallContext,
    cell: usize,
    value: usize,
    at: usize,
    out_ret: *mut JitReturn,
) {
    // SAFETY: Caller upholds the `JitCallContext` and `out_ret` contracts.
    unsafe {
        write_native_result(ctx, out_ret, |native_ctx| {
            let value = Word::from_raw(value);
            native_write_box(native_ctx.runtime, Word::from_raw(cell), value, at)?;
            Ok(NativeReturn::One(value))
        });
    }
}
