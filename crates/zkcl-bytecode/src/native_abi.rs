//! C ABI shared by native entry points and the runtime.

/// `JitReturn::tag` discriminants matching `zkcl-runtime::native::JitReturn`.
pub const JIT_RETURN_TAG_ONE: u8 = 0;
pub const JIT_RETURN_TAG_VALUES: u8 = 1;
pub const JIT_RETURN_TAG_UNWIND: u8 = 2;
pub const JIT_RETURN_TAG_FAULT: u8 = 3;
pub const JIT_RETURN_TAG_TAIL_CALL: u8 = 4;
pub const NATIVE_CALL_ABI_REVISION: u32 = 7;

/// The call has left no effects. Retry it from the caller's bytecode checkpoint.
pub const JIT_RETURN_TAG_VM_CALL: u8 = 5;
/// A compiled activation has handed its continuation back to the VM.
pub const JIT_RETURN_TAG_VM_RESUME: u8 = 6;

/// Offsets in the codegen-visible prefix of `#[repr(C)] JitCallContext`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JitCallContextOffsets {
    pub cancel_flag: i32,
    pub remaining_steps: i32,
    pub runtime: i32,
    /// Native execution quantum pointer. A null pointer disables time slicing.
    pub slice_steps: i32,
}

pub const fn jit_call_context_offsets(word_bytes: i32) -> JitCallContextOffsets {
    JitCallContextOffsets {
        cancel_flag: 0,
        remaining_steps: word_bytes,
        runtime: 2 * word_bytes,
        slice_steps: 3 * word_bytes,
    }
}

/// The layout of `#[repr(C)] JitReturn` on the compilation target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JitReturnLayout {
    pub tag: i32,
    pub single_value: i32,
    pub target_frame: i32,
    pub state: i32,
    pub size: u32,
}

pub const fn jit_return_layout(word_bytes: i32) -> JitReturnLayout {
    JitReturnLayout {
        tag: 0,
        single_value: word_bytes,
        target_frame: 2 * word_bytes,
        state: 3 * word_bytes,
        size: 4 * word_bytes as u32,
    }
}

/// The scalar leaf entry always takes this many word arguments, including padding.
pub const LEAF_ARGUMENTS: usize = 4;

/// A leaf with an operand or overflow miss returns the internal unbound word.
/// It cannot be the result of a successful Lisp call.
pub const LEAF_MISS: u64 = crate::WORD_UNBOUND;

/// Check the current, untraced binding of a function's constant-pool symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeafGuard {
    pub symbol: usize,
    pub primitive: LeafPrimitive,
}

/// The original binding required by a selected scalar leaf operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LeafPrimitive {
    Arithmetic(crate::FixnumArithmetic),
    Compare(FixnumComparison),
    IntegerBits(IntegerBitPrimitive),
}

impl LeafPrimitive {
    /// Use only for a symbol resolved to the COMMON-LISP package.
    pub fn named(name: &str) -> Option<Self> {
        FixnumPrimitive::named(name)
            .map(Self::from)
            .or_else(|| IntegerBitPrimitive::named(name).map(Self::IntegerBits))
    }

    pub fn numeric(self) -> Option<FixnumPrimitive> {
        match self {
            Self::Arithmetic(op) => Some(FixnumPrimitive::Arithmetic(op)),
            Self::Compare(op) => Some(FixnumPrimitive::Compare(op)),
            Self::IntegerBits(_) => None,
        }
    }
}

impl From<FixnumPrimitive> for LeafPrimitive {
    fn from(primitive: FixnumPrimitive) -> Self {
        match primitive {
            FixnumPrimitive::Arithmetic(op) => Self::Arithmetic(op),
            FixnumPrimitive::Compare(op) => Self::Compare(op),
        }
    }
}

macro_rules! integer_bit_primitives {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        /// The integer bit operations recognized by the scalar leaf selector.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        #[repr(u8)]
        pub enum IntegerBitPrimitive { $($variant),+ }

        impl IntegerBitPrimitive {
            pub const COUNT: usize = [$(Self::$variant),+].len();
            pub const ALL: [Self; Self::COUNT] = [$(Self::$variant),+];

            pub const fn name(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }

            pub fn named(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

integer_bit_primitives! {
    And => "LOGAND",
    Or => "LOGIOR",
    Xor => "LOGXOR",
    Eqv => "LOGEQV",
    Nand => "LOGNAND",
    Nor => "LOGNOR",
    AndC1 => "LOGANDC1",
    AndC2 => "LOGANDC2",
    OrC1 => "LOGORC1",
    OrC2 => "LOGORC2",
    Not => "LOGNOT",
    Test => "LOGTEST",
    Bitp => "LOGBITP",
    Oddp => "ODDP",
    Evenp => "EVENP",
}

/// A Common Lisp primitive with a fixed-arity fixnum specialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FixnumPrimitive {
    Arithmetic(crate::FixnumArithmetic),
    Compare(FixnumComparison),
}

/// Numeric comparison of two signed fixnums.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum FixnumComparison {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl FixnumPrimitive {
    /// Use only after resolving the symbol's identity in the COMMON-LISP package.
    pub fn named(name: &str) -> Option<Self> {
        use crate::FixnumArithmetic::*;
        use FixnumComparison::*;
        Some(match name {
            "+" => Self::Arithmetic(Add),
            "-" => Self::Arithmetic(Sub),
            "*" => Self::Arithmetic(Mul),
            "1+" => Self::Arithmetic(Inc),
            "1-" => Self::Arithmetic(Dec),
            "=" => Self::Compare(Eq),
            "/=" => Self::Compare(Ne),
            "<" => Self::Compare(Lt),
            "<=" => Self::Compare(Le),
            ">" => Self::Compare(Gt),
            ">=" => Self::Compare(Ge),
            _ => return None,
        })
    }

    pub const fn arity(self) -> usize {
        match self {
            Self::Arithmetic(op) => op.arity(),
            Self::Compare(_) => 2,
        }
    }

    pub const fn binding_index(self) -> u8 {
        match self {
            Self::Arithmetic(op) => op.binding_index(),
            Self::Compare(FixnumComparison::Eq) => 8,
            Self::Compare(FixnumComparison::Ne) => 9,
            Self::Compare(FixnumComparison::Lt) => 10,
            Self::Compare(FixnumComparison::Le) => 11,
            Self::Compare(FixnumComparison::Gt) => 12,
            Self::Compare(FixnumComparison::Ge) => 13,
        }
    }
}

impl FixnumComparison {
    pub const fn test(self, lhs: i64, rhs: i64) -> bool {
        match self {
            Self::Eq => lhs == rhs,
            Self::Ne => lhs != rhs,
            Self::Lt => lhs < rhs,
            Self::Le => lhs <= rhs,
            Self::Gt => lhs > rhs,
            Self::Ge => lhs >= rhs,
        }
    }
}

/// Binary integer pack operations that the native compiler can specialize.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum I32x4Operation {
    Add,
    Sub,
    BitAnd,
    BitOr,
    BitXor,
}

impl I32x4Operation {
    pub const fn binding_index(self) -> u8 {
        match self {
            Self::Add => 14,
            Self::Sub => 15,
            Self::BitAnd => 16,
            Self::BitOr => 17,
            Self::BitXor => 18,
        }
    }
}

/// Deoptimization format byte for an i32x4 pack. The scalar entry indexes the
/// first register holding this identity and the 16-byte payload at that index.
pub const DEOPT_I32X4_FORMAT: u8 = 128;

/// C-ABI types used by native runtime functions.
#[derive(Clone, Copy)]
pub enum RuntimeAbiType {
    Pointer,
    Word,
    Bool,
    I64,
}

/// Parameters of the out-pointer C entry: context, tagged function, arguments, count, result.
pub const NATIVE_ENTRY_PARAMS: [RuntimeAbiType; 5] = [
    RuntimeAbiType::Pointer,
    RuntimeAbiType::Word,
    RuntimeAbiType::Pointer,
    RuntimeAbiType::Word,
    RuntimeAbiType::Pointer,
];

// Each row supplies the instruction selector, the name of the typed runtime
// pointer, the exported symbol, and the Rust/target signature. Rows follow
// the C layout of the typed pointer table. The IDs index codegen tables.
#[doc(hidden)]
#[macro_export]
macro_rules! native_runtime_functions {
    ($declare:ident) => {
        $declare! {
            MarkScope = 23, mark_scope => zkcl_rt_mark_scope(ctx: Context, pc: Word);
            RefillSlice = 24, refill_slice => zkcl_rt_refill_slice(ctx: Context) -> Bool;
            Invoke = 0, invoke => zkcl_rt_invoke(
                ctx: Context, target: Word, args_ptr: Words, argc: Word, at: Word,
                out_ret: ResultOut,
            );
            InvokeValues = 1, invoke_values => zkcl_rt_invoke_values(
                ctx: Context, callee: Word, at: Word, out_ret: ResultOut,
            );
            FunctionConstant = 2, function_constant => zkcl_rt_function_constant(
                ctx: Context, fn_word: Word, index: Word,
            ) -> Word;
            ReadCell = 4, read_cell => zkcl_rt_read_cell(
                ctx: Context, symbol: Word, function: Bool, at: Word, out_ret: ResultOut,
            );
            WriteCell = 5, write_cell => zkcl_rt_write_cell(
                ctx: Context, symbol: Word, value: Word,
            );
            BindSpecial = 6, bind_special => zkcl_rt_bind_special(
                ctx: Context, frame_index: Word, symbol: Word, value: Word,
            );
            Unbind = 7, unbind => zkcl_rt_unbind(ctx: Context, count: Word);
            Protect = 8, protect => zkcl_rt_protect(
                ctx: Context, frame_index: Word, cleanup_state: Word, clear: Word,
            );
            BeginCleanup = 9, begin_cleanup => zkcl_rt_begin_cleanup(ctx: Context);
            EndCleanup = 10, end_cleanup => zkcl_rt_end_cleanup(
                ctx: Context, out_ret: ResultOut,
            );
            Throw = 11, throw => zkcl_rt_throw(
                ctx: Context, tag: Word, at: Word, out_ret: ResultOut,
            );
            ReturnFrom = 12, return_from => zkcl_rt_return_from(
                ctx: Context, token: Word, at: Word, out_ret: ResultOut,
            );
            AllocRootFrame = 13, alloc_root_frame => zkcl_rt_alloc_root_frame(
                ctx: Context, slots_len: Word, out_slots_ptr: RootSlotsOut,
                out_frame_index: WordOut,
            );
            DropRootFrame = 14, drop_root_frame => zkcl_rt_drop_root_frame(
                ctx: Context, frame_index: Word,
            );
            SetOneValue = 15, set_one_value => zkcl_rt_set_one_value(ctx: Context, word: Word);
            GetValue = 17, get_value => zkcl_rt_get_value(ctx: Context, index: Word) -> Word;
            SetValues = 16, set_values => zkcl_rt_set_values(
                ctx: Context, words_ptr: Words, count: Word,
            );
            SaveValues = 19, save_values => zkcl_rt_save_values(
                ctx: Context, frame_index: Word,
            );
            AppendValues = 20, append_values => zkcl_rt_append_values(ctx: Context);
            RestoreValues = 21, restore_values => zkcl_rt_restore_values(ctx: Context);
            ReadCapture = 31, read_capture => zkcl_rt_read_capture(
                ctx: Context, closure_word: Word, index: Word, at: Word, out_ret: ResultOut,
            );
            AllocClosure = 32, alloc_closure => zkcl_rt_alloc_closure(
                ctx: Context, fn_word: Word, const_idx: Word, captures_ptr: Words, count: Word,
                at: Word, out_ret: ResultOut,
            );
            EnterCatch = 33, enter_catch => zkcl_rt_enter_catch(
                ctx: Context, frame_index: Word, tag_slot: Word, resume_state: Word, clear: Word,
            );
            EnterBlock = 34, enter_block => zkcl_rt_enter_block(
                ctx: Context, frame_index: Word, token: Word, resume_state: Word, clear: Word,
            );
            LeaveTarget = 36, leave_target => zkcl_rt_leave_target(ctx: Context);
            ExitToken = 37, exit_token => zkcl_rt_exit_token(
                ctx: Context, at: Word, out_ret: ResultOut,
            );
            PendingUnwindState = 38, pending_unwind_state => zkcl_rt_pending_unwind_state(
                ctx: Context, frame_index: Word,
            ) -> Word;
            DeoptToVm = 26, deopt_to_vm => zkcl_rt_deopt_to_vm(
                ctx: Context, target_word: Word, pc: Word, regs_ptr: Registers, vals_ptr: Words,
                reg_count: Word, out_ret: ResultOut,
            );
            CallNamed = 27, call_named => zkcl_rt_call_named(
                ctx: Context, fn_word: Word, sym_idx: Word, args_ptr: Words, argc: Word, at: Word,
                out_ret: ResultOut,
            );
            BoxLocal = 28, box_local => zkcl_rt_box_local(
                ctx: Context, value: Word, at: Word, out_ret: ResultOut,
            );
            ReadBox = 29, read_box => zkcl_rt_read_box(
                ctx: Context, cell: Word, at: Word, out_ret: ResultOut,
            );
            WriteBox = 30, write_box => zkcl_rt_write_box(
                ctx: Context, cell: Word, value: Word, at: Word, out_ret: ResultOut,
            );
            EnterTagbody = 35, enter_tagbody => zkcl_rt_enter_tagbody(
                ctx: Context, frame_index: Word, token: Word, base_state: Word, count: Word,
                clear: Word,
            );
            Go = 39, go => zkcl_rt_go(
                ctx: Context, token: Word, label: Word, at: Word, out_ret: ResultOut,
            );
            DefineFunction = 25, define_function => zkcl_rt_define_function(
                ctx: Context, symbol: Word, definition: Word, is_macro: Bool, at: Word,
                out_ret: ResultOut,
            );
            EnterHandlers = 40, enter_handlers => zkcl_rt_enter_handlers(
                ctx: Context, frame_index: Word, target: Word, table: Word, states: Words,
                condition_slots: Words, count: Word,
            );
            LeaveHandlers = 41, leave_handlers => zkcl_rt_leave_handlers(ctx: Context);
            MapDebugUnwindRegister = 67, map_debug_unwind_register => zkcl_rt_map_debug_unwind_register(
                ctx: Context, register: Word, root_slot: Word,
            );
            StageDebugArguments = 66, stage_debug_arguments => zkcl_rt_stage_debug_arguments(
                ctx: Context, arguments: Words, count: Word,
            );
            DebugPoint = 64, debug_point => zkcl_rt_debug_point(
                ctx: Context, pc: Word, registers: Words, count: Word, out: ResultOut,
            );
            DebugRegister = 65, debug_register => zkcl_rt_debug_register(
                ctx: Context, index: Word,
            ) -> Word;
            PollExecution = 42, poll_execution => zkcl_rt_poll_execution(
                ctx: Context, at: Word, out_ret: ResultOut,
            );
            UnboxF32 = 43, unbox_f32 => zkcl_rt_unbox_f32(ctx: Context, value: Word) -> I64;
            UnboxF64 = 44, unbox_f64 => zkcl_rt_unbox_f64(ctx: Context, value: Word) -> I64;
            BoxF32 = 45, box_f32 => zkcl_rt_box_f32(
                ctx: Context, bits: I64, at: Word, out_ret: ResultOut,
            );
            BoxF64 = 46, box_f64 => zkcl_rt_box_f64(
                ctx: Context, bits: I64, at: Word, out_ret: ResultOut,
            );
            DeoptFloatToVm = 47, deopt_float_to_vm => zkcl_rt_deopt_float_to_vm(
                ctx: Context, target_word: Word, pc: Word, regs_ptr: Registers,
                vals_ptr: WideWords, formats_ptr: Formats, reg_count: Word, out_ret: ResultOut,
            );
            UnboxI32x4 = 3, unbox_i32x4 => zkcl_rt_unbox_i32x4(
                ctx: Context, value: Word, out: PackOut,
            ) -> Bool;
            BoxI32x4 = 18, box_i32x4 => zkcl_rt_box_i32x4(
                ctx: Context, bits: Packs, at: Word, ret: ResultOut,
            );
            DeoptSimdToVm = 22, deopt_simd_to_vm => zkcl_rt_deopt_simd_to_vm(
                ctx: Context, target: Word, pc: Word, registers: Registers, scalars: WideWords,
                formats: Formats, packs: Packs, count: Word, ret: ResultOut,
            );
            IsPrimitiveBinding = 49, is_primitive_binding => zkcl_rt_is_primitive_binding(
                ctx: Context, target: Word, symbol: Word, op: Word,
            ) -> Bool;
            ReadFloatVector = 48, read_float_vector => zkcl_rt_read_float_vector(
                ctx: Context, function: Word, symbol: Word, array: Word, index: Word,
                double: Bool,
            ) -> I64;
            WriteFloatVector = 50, write_float_vector => zkcl_rt_write_float_vector(
                ctx: Context, function: Word, symbol: Word, setter: Word, array: Word,
                index: Word, bits: I64, double: Bool,
            ) -> Bool;
            BindProgv = 51, bind_progv => zkcl_rt_bind_progv(
                ctx: Context, frame_index: Word, symbols: Word, values: Word, at: Word,
                out_ret: ResultOut,
            );
            StageArguments = 52, stage_arguments => zkcl_stage_arguments(
                ctx: Context, frame: Word, args: Words, count: Word,
            );
            OptionalArgument = 53, optional_argument => zkcl_optional_argument(
                ctx: Context, frame: Word, index: Word,
            ) -> Word;
            RestArguments = 54, rest_arguments => zkcl_rest_arguments(
                ctx: Context, frame: Word, start: Word, at: Word, result: ResultOut,
            );
            CheckKeywords = 55, check_keywords => zkcl_check_keywords(
                ctx: Context, frame: Word, target: Word, start: Word, table: Word,
                result: ResultOut,
            );
            KeywordArgument = 56, keyword_argument => zkcl_keyword_argument(
                ctx: Context, frame: Word, index: Word, result: ResultOut,
            );
            ReleaseArguments = 57, release_arguments => zkcl_release_arguments(
                ctx: Context, frame: Word,
            );
            ProclaimSpecial = 58, proclaim_special => zkcl_proclaim_special(
                ctx: Context, symbol: Word, at: Word, out: ResultOut,
            );
            ProclaimInline = 59, proclaim_inline => zkcl_proclaim_inline(
                ctx: Context, symbol: Word, setf: Bool, notinline: Bool,
            );
            SetVariableDocumentation = 60, set_variable_documentation => zkcl_set_variable_documentation(
                ctx: Context, symbol: Word, doc: Word,
            );
            IsSymbolBound = 61, is_symbol_bound => zkcl_is_symbol_bound(
                ctx: Context, symbol: Word,
            ) -> Bool;
            StageTailCall = 62, stage_tail_call => zkcl_rt_stage_tail_call(
                ctx: Context, callee: Word, args: Words, argc: Word, at: Word, use_values: Bool,
            );
            IsSelfCall = 63, is_self_call => zkcl_rt_is_self_call(
                ctx: Context, function: Word, symbol: Word,
            ) -> Bool;
            PrepareVopU64 = 68, prepare_vop_u64 => zkcl_rt_prepare_vop_u64(
                ctx: Context, function: Word, symbol: Word, binding: Word, input: Word, out: WideWordOut,
            ) -> Bool;
        }
    };
}

macro_rules! abi_type {
    (Word) => {
        RuntimeAbiType::Word
    };
    (Bool) => {
        RuntimeAbiType::Bool
    };
    (I64) => {
        RuntimeAbiType::I64
    };
    (Context) => {
        RuntimeAbiType::Pointer
    };
    (ResultOut) => {
        RuntimeAbiType::Pointer
    };
    (Words) => {
        RuntimeAbiType::Pointer
    };
    (WordOut) => {
        RuntimeAbiType::Pointer
    };
    (RootSlotsOut) => {
        RuntimeAbiType::Pointer
    };
    (Registers) => {
        RuntimeAbiType::Pointer
    };
    (WideWords) => {
        RuntimeAbiType::Pointer
    };
    (WideWordOut) => {
        RuntimeAbiType::Pointer
    };
    (Formats) => {
        RuntimeAbiType::Pointer
    };
    (Packs) => {
        RuntimeAbiType::Pointer
    };
    (PackOut) => {
        RuntimeAbiType::Pointer
    };
}

macro_rules! declare_runtime_abi_functions {
    ($($name:ident = $id:literal, $field:ident => $symbol:ident(
        $($param:ident: $kind:ident),* $(,)?
    ) $(-> $result:ident)?;)*) => {
        /// Runtime entry points shared by native code generators.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u32)]
        pub enum RuntimeAbiFunction {
            $($name = $id,)*
        }

        impl RuntimeAbiFunction {
            const COUNT: usize = [$(Self::$name,)*].len();

            /// Runtime functions in ID order. The IDs are unique and dense.
            pub const ALL: [Self; Self::COUNT] = {
                let mut functions = [Self::Invoke; Self::COUNT];
                $(functions[$id] = Self::$name;)*
                functions
            };

            /// C-ABI external symbol exported by zkcl-runtime.
            pub const fn symbol_name(self) -> &'static str {
                match self {
                    $(Self::$name => stringify!($symbol),)*
                }
            }

            /// Recognize an exported runtime symbol without consulting the runtime.
            pub fn from_symbol_name(symbol: &str) -> Option<Self> {
                match symbol {
                    $(stringify!($symbol) => Some(Self::$name),)*
                    _ => None,
                }
            }

            pub const fn parameter_types(self) -> &'static [RuntimeAbiType] {
                match self {
                    $(Self::$name => &[$(abi_type!($kind),)*],)*
                }
            }

            pub const fn return_type(self) -> Option<RuntimeAbiType> {
                match self {
                    $(Self::$name => declare_runtime_abi_functions!(@result $($result)?),)*
                }
            }
        }
    };
    (@result $kind:ident) => { Some(abi_type!($kind)) };
    (@result) => { None };
}

native_runtime_functions!(declare_runtime_abi_functions);
