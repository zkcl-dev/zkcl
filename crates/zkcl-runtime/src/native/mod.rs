//! Native interop, AOT module registration, and JIT ABI/compilation substrate.

mod argument_binding;
mod array;
mod array_write;
mod backend;
mod call_cache;
mod context;
mod debugging;
mod layout;
mod return_value;
mod runtime_calls;
mod simd;
mod trampoline;

pub use argument_binding::*;
pub use backend::*;
pub(crate) use context::JitCEntry;
pub use context::{JitCEntryFn, JitCallContext, NativeContext, NativeRootFrame};
pub use debugging::*;
pub use layout::{AbiFingerprint, WordLayout};
pub use return_value::{JitReturn, NativeReturn};
pub(crate) use return_value::{NativeOutcome, NativeTailCall};
pub use runtime_calls::*;
pub use simd::{zkcl_rt_box_i32x4, zkcl_rt_deopt_simd_to_vm, zkcl_rt_unbox_i32x4};
pub use trampoline::*;

use zkcl_bytecode::Class;
pub use zkcl_bytecode::native_abi::{
    FixnumComparison, FixnumPrimitive, IntegerBitPrimitive, LeafGuard, LeafPrimitive,
};
pub use zkcl_bytecode::{Arity, FixnumArithmetic, Handler, HandlerTarget, Test};

pub use crate::condition::Fault;
pub use crate::heap::{RootSlots, Word};
use crate::{EvalError, Runtime, vm};

/// Load a constant from a `Function` (or `Closure`'s underlying `Function`) constant pool.
#[inline]
pub(crate) fn native_function_constant(rt: &Runtime, function: Word, index: usize) -> Word {
    let code = rt.heap.function_code(function).unwrap_or(function);
    rt.heap.function_constant(code, index)
}

/// Read a symbol's value or function cell.
pub(crate) fn native_read_cell(
    rt: &Runtime,
    symbol: Word,
    function: bool,
    at: usize,
) -> Result<Word, Fault> {
    if function {
        vm::function_cell(rt, symbol, at)
    } else {
        vm::symbol_value(rt, symbol, at)
    }
}

/// Write a symbol's dynamic value cell.
pub(crate) fn native_write_cell(rt: &Runtime, symbol: Word, value: Word) {
    rt.heap.set_symbol_cell(symbol, false, value);
}

/// Read a boxed lexical cell (`ObjectKind::Cell`).
pub(crate) fn native_read_box(rt: &Runtime, cell: Word, at: usize) -> Result<Word, Fault> {
    rt.heap.read_cell(cell).ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected a lexical cell"),
        )
    })
}

/// Write a boxed lexical cell (`ObjectKind::Cell`).
pub(crate) fn native_write_box(
    rt: &Runtime,
    cell: Word,
    value: Word,
    at: usize,
) -> Result<(), Fault> {
    rt.heap.write_cell(cell, value).ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected a lexical cell"),
        )
    })
}

/// Install a function or macro definition on `symbol`.
pub(crate) fn native_define_function(
    rt: &Runtime,
    symbol: Word,
    definition: Word,
    is_macro: bool,
    at: usize,
) -> Result<(), Fault> {
    vm::check_definable_name(rt, symbol, at)?;
    if is_macro {
        rt.heap.set_macro_expander(symbol, definition);
    } else {
        rt.heap.set_symbol_cell(symbol, true, definition);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use super::*;
    use crate::vm::ExecState;

    #[test]
    fn native_entry_offsets_match_the_runtime_c_layout() {
        use std::mem::{offset_of, size_of};
        use zkcl_bytecode::native_abi::{
            JitCallContextOffsets, JitReturnLayout, jit_call_context_offsets, jit_return_layout,
        };

        let width = size_of::<usize>() as i32;
        assert_eq!(
            jit_call_context_offsets(width),
            JitCallContextOffsets {
                cancel_flag: offset_of!(JitCallContext, cancel_flag) as i32,
                remaining_steps: offset_of!(JitCallContext, remaining_steps) as i32,
                runtime: offset_of!(JitCallContext, runtime) as i32,
                slice_steps: offset_of!(JitCallContext, slice_steps) as i32,
            }
        );
        assert_eq!(
            jit_return_layout(width),
            JitReturnLayout {
                tag: offset_of!(JitReturn, tag) as i32,
                single_value: offset_of!(JitReturn, single_value) as i32,
                target_frame: offset_of!(JitReturn, target_frame) as i32,
                state: offset_of!(JitReturn, state) as i32,
                size: size_of::<JitReturn>() as u32,
            }
        );
        assert_eq!(WordLayout::NIL as u64, zkcl_bytecode::WORD_NIL);
        assert_eq!(WordLayout::UNBOUND as u64, zkcl_bytecode::WORD_UNBOUND);
        assert_eq!(WordLayout::T as u64, zkcl_bytecode::WORD_T);
        assert_eq!(WordLayout::TAG_FIXNUM as u64, zkcl_bytecode::TAG_FIXNUM);
        assert_eq!(WordLayout::TAG_MASK as u64, zkcl_bytecode::TAG_MASK);
        assert_eq!(WordLayout::FIXNUM_SHIFT, zkcl_bytecode::FIXNUM_SHIFT);
    }

    #[test]
    fn native_host_call_returns_a_relocated_argument_after_unbinding_itself() {
        use crate::heap::{Heap, HeapConfig};

        let rt = Runtime::with_heap(
            Heap::start(HeapConfig {
                always_defrag: true,
                ..HeapConfig::default()
            })
            .unwrap(),
        );
        let name = rt.eval("'native-host").unwrap().pop().unwrap();
        let capture = std::sync::Arc::new(());
        let weak = std::sync::Arc::downgrade(&capture);
        let owner = rt.foreign(capture).unwrap();
        let function = rt
            .host_function(&owner, move |rt, args, _| {
                let before = args.get(0);
                rt.eval("(fmakunbound 'native-host)")?;
                rt.collect();
                assert!(weak.upgrade().is_some());
                assert_ne!(args.get(0), before);
                Ok(Some(args.get(0)))
            })
            .unwrap();
        rt.heap
            .set_symbol_cell(name.slot.get(), true, function.slot.get());
        drop(owner);
        drop(function);

        let mut exec = ExecState::new(vm::Values::new(&rt));
        let mut native = NativeContext::new(&rt, NonNull::from(&mut exec));
        let mut context = JitCallContext::detached();
        context.runtime = &rt;
        context.exec = native.exec.as_ptr().cast();
        native.set_values(&[Word::T, Word::T]);
        let argument = rt.heap.cons(|| (Word::T, Word::NIL)).unwrap().raw();
        let callee = rt.heap.symbol_cell(name.slot.get(), true).unwrap().raw();
        let mut result = JitReturn::zeroed();
        unsafe { zkcl_rt_invoke(&mut context, callee, &argument, 1, 0, &mut result) };
        assert_eq!(result.tag, JitReturn::TAG_ONE);
        assert_eq!(native.values_len(), 0);
        assert_eq!(
            rt.heap.cons_fields(Word::from_raw(result.single_value)),
            Some((Word::T, Word::NIL))
        );
    }

    #[test]
    fn jit_box_local_keeps_the_input_rooted_during_collection() {
        use crate::heap::{Heap, HeapConfig};

        let runtime = Runtime::with_heap(
            Heap::start(HeapConfig {
                always_defrag: true,
                stress_bytes: Some(1),
                ..HeapConfig::default()
            })
            .unwrap(),
        );
        let pair = runtime.root_slot(runtime.heap.cons(|| (Word::T, Word::NIL)).unwrap());
        let before = pair.get();
        let mut exec = ExecState::new(vm::Values::new(&runtime));
        let mut context = JitCallContext::detached();
        context.runtime = &runtime;
        context.exec = (&mut exec as *mut ExecState<'_>).cast();
        let mut result = JitReturn::zeroed();
        unsafe { zkcl_rt_box_local(&mut context, before.raw(), 0, &mut result) };
        assert_eq!(result.tag, JitReturn::TAG_ONE);
        assert_ne!(pair.get(), before, "the allocation should move the cons");
        assert_eq!(
            runtime.heap.read_cell(Word::from_raw(result.single_value)),
            Some(pair.get())
        );
    }
}

#[cfg(test)]
mod offline_abi_tests {
    #[test]
    fn offline_fingerprints_require_machine_layout_and_track_word_width() {
        assert!(crate::CompilationTarget::javascript().machine().is_none());
        let narrow = "wasm32-unknown-unknown"
            .parse::<crate::CompilationTarget>()
            .unwrap();
        let wide = "x86_64-unknown-linux-gnu"
            .parse::<crate::CompilationTarget>()
            .unwrap();
        let narrow = super::AbiFingerprint::for_target(narrow.machine().unwrap());
        let wide = super::AbiFingerprint::for_target(wide.machine().unwrap());
        assert_eq!(narrow.pointer_width, 32);
        assert_eq!(wide.pointer_width, 64);
        assert_ne!(
            narrow.bytecode_contract_version,
            wide.bytecode_contract_version
        );
    }

    #[test]
    fn target_fingerprint_matches_native_heap_layout() {
        assert_eq!(
            super::AbiFingerprint::for_target(
                crate::CompilationTarget::native()
                    .machine()
                    .expect("native target has a machine ABI"),
            ),
            super::AbiFingerprint::CURRENT,
        );
    }
}
