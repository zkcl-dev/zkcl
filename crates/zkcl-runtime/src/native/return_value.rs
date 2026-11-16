use super::WordLayout;
use crate::heap::{RootFrame, Word};

/// Structured return status from a native (AOT or JIT) Lisp function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeReturn {
    /// Fast single-value return without touching `ExecState::values`.
    One(Word),
    /// Zero, one, or multiple return values stored in `ExecState::values`.
    Values,
    /// Structured non-local unwind targeting `ExecState::frames[target_frame]` at `state`.
    Unwind { target_frame: usize, state: usize },
}

/// A native activation can return a rooted call for its caller to dispatch.
/// These slots can outlive the staging-stack allocation used to enter it.
pub(crate) struct NativeTailCall {
    pub(crate) slots: RootFrame,
    pub(crate) at: usize,
}

pub(crate) enum NativeOutcome {
    Returned(NativeReturn),
    TailCall(NativeTailCall),
    ResumeVm,
}

/// Return descriptor passed across the C ABI by native code and the runtime.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitReturn {
    /// Return status, including a tail call staged in the active `JitCallContext`.
    pub tag: u8,
    /// The tagged Lisp word for `TAG_ONE`.
    pub single_value: usize,
    pub target_frame: usize,
    pub state: usize,
}

impl JitReturn {
    pub const TAG_ONE: u8 = zkcl_bytecode::native_abi::JIT_RETURN_TAG_ONE;
    pub const TAG_VALUES: u8 = zkcl_bytecode::native_abi::JIT_RETURN_TAG_VALUES;
    pub const TAG_UNWIND: u8 = zkcl_bytecode::native_abi::JIT_RETURN_TAG_UNWIND;
    pub const TAG_FAULT: u8 = zkcl_bytecode::native_abi::JIT_RETURN_TAG_FAULT;
    pub const TAG_TAIL_CALL: u8 = zkcl_bytecode::native_abi::JIT_RETURN_TAG_TAIL_CALL;
    pub const TAG_VM_CALL: u8 = zkcl_bytecode::native_abi::JIT_RETURN_TAG_VM_CALL;
    pub const TAG_VM_RESUME: u8 = zkcl_bytecode::native_abi::JIT_RETURN_TAG_VM_RESUME;

    pub(super) fn control(tag: u8) -> Self {
        Self {
            tag,
            ..Self::zeroed()
        }
    }

    pub const fn zeroed() -> Self {
        Self {
            tag: Self::TAG_ONE,
            single_value: WordLayout::NIL,
            target_frame: 0,
            state: 0,
        }
    }

    pub(super) fn from_native(ret: NativeReturn) -> Self {
        match ret {
            NativeReturn::One(word) => Self {
                tag: Self::TAG_ONE,
                single_value: word.raw(),
                target_frame: 0,
                state: 0,
            },
            NativeReturn::Values => Self {
                tag: Self::TAG_VALUES,
                single_value: 0,
                target_frame: 0,
                state: 0,
            },
            NativeReturn::Unwind {
                target_frame,
                state,
            } => Self {
                tag: Self::TAG_UNWIND,
                single_value: 0,
                target_frame,
                state,
            },
        }
    }
}
