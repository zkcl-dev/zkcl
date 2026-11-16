use rustc_hash::FxHashMap;
use zkcl_bytecode::Aarch64Popcount;
use zkcl_bytecode::native_abi::{FixnumPrimitive, I32x4Operation};
use zkcl_bytecode::{Arity, Immediate, Register};

pub use zkcl_bytecode::native_abi::FixnumComparison as CmpOp;
pub use zkcl_bytecode::{AliasDomain, BarrierEffect, ControlEffect, OpEffects};

const HOST_FIXNUM_BOUNDS: (i64, i64) =
    zkcl_bytecode::fixnum_bounds_for_bits(usize::BITS as u8).expect("supported host word width");

/// Minimum signed integer representable as a tagged fixnum in the host VM.
pub const FIXNUM_MIN: i64 = HOST_FIXNUM_BOUNDS.0;

/// Maximum signed integer representable as a tagged fixnum in the host VM.
pub const FIXNUM_MAX: i64 = HOST_FIXNUM_BOUNDS.1;

/// Returns whether `value` fits in the host VM's tagged fixnum range.
#[inline]
pub const fn is_valid_fixnum(value: i64) -> bool {
    value >= FIXNUM_MIN && value <= FIXNUM_MAX
}

/// Identifier of a basic block in an [`SsaFunction`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BlockId(pub u32);

/// Identifier of an SSA value in an [`SsaFunction`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ValueId(pub u32);

/// Physical representation of an SSA value in the optimizer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ValueRepr {
    /// Tagged VM `Word` (immediate tag or GC-tracked heap object reference).
    TaggedWord,
    /// Unboxed 64-bit signed integer (never a GC root).
    UnboxedI64,
    /// Unboxed IEEE binary32 (never a GC root).
    UnboxedF32,
    /// Unboxed IEEE binary64 (never a GC root).
    UnboxedF64,
    /// Four unboxed 32-bit signed integer lanes (never a GC root).
    UnboxedI32x4,
    /// Unboxed boolean condition produced by comparisons/predicates (never a GC root).
    Bool,
}

/// Static type lattice for SSA values (`Top > Any > concrete tags > Bottom`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum TypeTag {
    /// Completely unknown / unconstrained top element.
    Top,
    /// Unreachable / uninhabited / conflicting bottom element.
    Bottom,
    /// `NIL` (`Word::NIL`).
    Nil,
    /// `T` (`Word::T`).
    True,
    /// Tagged or unboxed fixnum in the execution target's integer range.
    Fixnum,
    /// Single- or double-float value.
    Float,
    /// Signed integer pack with four 32-bit lanes.
    I32x4,
    /// Character immediate.
    Char,
    /// Cons cell.
    Cons,
    /// Heap symbol.
    Symbol,
    /// Function or closure object.
    Function,
    /// Any valid runtime Lisp `Word`.
    Any,
}

impl TypeTag {
    /// Least upper bound (`a ⊔ b`) in the type lattice.
    #[inline]
    pub const fn join(self, other: Self) -> Self {
        if matches!((self, other), (Self::Top, _) | (_, Self::Top)) {
            return Self::Top;
        }
        match (self, other) {
            (Self::Bottom, x) | (x, Self::Bottom) => x,
            (Self::Nil, Self::Nil) => Self::Nil,
            (Self::True, Self::True) => Self::True,
            (Self::Fixnum, Self::Fixnum) => Self::Fixnum,
            (Self::Float, Self::Float) => Self::Float,
            (Self::I32x4, Self::I32x4) => Self::I32x4,
            (Self::Char, Self::Char) => Self::Char,
            (Self::Cons, Self::Cons) => Self::Cons,
            (Self::Symbol, Self::Symbol) => Self::Symbol,
            (Self::Function, Self::Function) => Self::Function,
            _ => Self::Any,
        }
    }

    /// Greatest lower bound (`a ⊓ b`) in the type lattice.
    #[inline]
    pub const fn meet(self, other: Self) -> Self {
        if matches!((self, other), (Self::Bottom, _) | (_, Self::Bottom)) {
            return Self::Bottom;
        }
        match (self, other) {
            (Self::Top, x) | (x, Self::Top) | (Self::Any, x) | (x, Self::Any) => x,
            (Self::Nil, Self::Nil) => Self::Nil,
            (Self::True, Self::True) => Self::True,
            (Self::Fixnum, Self::Fixnum) => Self::Fixnum,
            (Self::Float, Self::Float) => Self::Float,
            (Self::I32x4, Self::I32x4) => Self::I32x4,
            (Self::Char, Self::Char) => Self::Char,
            (Self::Cons, Self::Cons) => Self::Cons,
            (Self::Symbol, Self::Symbol) => Self::Symbol,
            (Self::Function, Self::Function) => Self::Function,
            _ => Self::Bottom,
        }
    }

    /// Returns `true` if `self` is a subtype of `other` (`self ⊔ other == other`).
    #[inline]
    pub const fn is_subtype_of(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Bottom, _)
                | (_, Self::Top)
                | (
                    Self::Nil
                        | Self::True
                        | Self::Fixnum
                        | Self::Float
                        | Self::I32x4
                        | Self::Char
                        | Self::Cons
                        | Self::Symbol
                        | Self::Function
                        | Self::Any,
                    Self::Any
                )
                | (Self::Nil, Self::Nil)
                | (Self::True, Self::True)
                | (Self::Fixnum, Self::Fixnum)
                | (Self::Float, Self::Float)
                | (Self::I32x4, Self::I32x4)
                | (Self::Char, Self::Char)
                | (Self::Cons, Self::Cons)
                | (Self::Symbol, Self::Symbol)
                | (Self::Function, Self::Function)
        )
    }

    /// Returns `true` if values of this type are guaranteed non-`NIL`.
    #[inline]
    pub const fn is_definitely_truthy(self) -> bool {
        matches!(
            self,
            Self::True
                | Self::Fixnum
                | Self::Float
                | Self::I32x4
                | Self::Char
                | Self::Cons
                | Self::Symbol
                | Self::Function
        )
    }

    /// Returns `true` if values of this type are guaranteed `NIL`.
    #[inline]
    pub const fn is_definitely_nil(self) -> bool {
        matches!(self, Self::Nil)
    }

    /// Type tag implied by a bytecode [`Immediate`].
    #[inline]
    pub const fn from_immediate(imm: Immediate) -> Self {
        match imm {
            Immediate::Nil => Self::Nil,
            Immediate::True => Self::True,
            Immediate::Fixnum(_) => Self::Fixnum,
            Immediate::Character(_) => Self::Char,
        }
    }
}

/// How a bytecode register value is reconstructed upon deoptimization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeoptRegisterValue {
    /// Already stored as a tagged VM `Word` (must be rooted across GC safepoints).
    TaggedWord(ValueId),
    /// Held as an unboxed `i64`; deoptimization re-boxes it as a fixnum `Word`.
    UnboxedI64(ValueId),
    /// Held as a raw float; deoptimization materializes a Lisp float of the same format.
    UnboxedFloat { format: FloatFormat, value: ValueId },
    /// Held as a raw pack. The original box identifies the Lisp object even
    /// when two distinct boxes hold the same raw SSA value.
    UnboxedI32x4 { identity: ValueId, value: ValueId },
    /// Known immediate constant; deoptimization materializes it without a live register.
    ConstImmediate(Immediate),
}

/// Deoptimization state captured at a speculative guard or call boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeoptState {
    /// Bytecode instruction index to resume in the interpreter on deoptimization.
    pub pc: usize,
    /// Mapping `(bytecode_register, value_representation)` for registers live at `pc`.
    pub registers_to_restore: Vec<(u32, DeoptRegisterValue)>,
}

impl DeoptState {
    /// Visit all [`ValueId`]s referenced by this deoptimization state.
    pub fn visit_values(&self, mut f: impl FnMut(ValueId)) {
        for &(_, kind) in &self.registers_to_restore {
            match kind {
                DeoptRegisterValue::TaggedWord(v)
                | DeoptRegisterValue::UnboxedI64(v)
                | DeoptRegisterValue::UnboxedI32x4 { value: v, .. }
                | DeoptRegisterValue::UnboxedFloat { value: v, .. } => f(v),
                DeoptRegisterValue::ConstImmediate(_) => {}
            }
        }
    }

    /// Rewrite value references inside this deoptimization state.
    pub fn rewrite_values(&mut self, mut f: impl FnMut(DeoptRegisterValue) -> DeoptRegisterValue) {
        for (_, kind) in &mut self.registers_to_restore {
            *kind = f(*kind);
        }
    }
}

/// Target cell for boxed lexical variable operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum CellTarget {
    Local(ValueId),
    Capture(usize),
}

/// Checked unboxed fixnum arithmetic operators.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
}

/// IEEE format of an unboxed Lisp float.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum FloatFormat {
    F32 = 32,
    F64 = 64,
}

impl FloatFormat {
    pub const fn repr(self) -> ValueRepr {
        match self {
            Self::F32 => ValueRepr::UnboxedF32,
            Self::F64 => ValueRepr::UnboxedF64,
        }
    }
}

/// One rounded, same-format floating-point operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u8)]
pub enum FloatArithOp {
    Add = 0,
    Sub = 1,
    Mul = 2,
    Div = 3,
}

/// Runtime function binding checked before specializing a named call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GuardedPrimitive {
    Arithmetic(FloatArithOp),
    FixnumArithmetic(zkcl_bytecode::FixnumArithmetic),
    Comparison(CmpOp),
    Typep,
    Funcall,
    I32x4(I32x4Operation),
}

impl GuardedPrimitive {
    pub const fn abi_index(self) -> u8 {
        match self {
            Self::Arithmetic(op) => op as u8,
            Self::FixnumArithmetic(op) => op.binding_index(),
            Self::Comparison(op) => FixnumPrimitive::Compare(op).binding_index(),
            Self::Typep => 4,
            Self::Funcall => 5,
            Self::I32x4(op) => op.binding_index(),
        }
    }
}

/// Operand representation observed in the VM before a numeric primitive call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum NumericOperand {
    Fixnum,
    F32,
    F64,
}

/// Runtime observations available at a bytecode call site.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Default)]
pub enum CallSiteProfile {
    #[default]
    Unprofiled,
    FixnumArithmetic,
    FloatArithmetic {
        lhs: NumericOperand,
        rhs: NumericOperand,
    },
    FloatArrayReader {
        format: FloatFormat,
    },
    FloatArrayWriter {
        format: FloatFormat,
    },
    SimdI32x4 {
        operation: I32x4Operation,
    },
    PolymorphicArithmetic,
    Primitive,
    Megamorphic,
}

/// Supplies constant-pool types and symbol names, call-site profiles, and
/// NOTINLINE declarations for bytecode lifting and SSA optimization.
pub trait SpecializationInputs {
    /// Preserve canonical register locations and source safepoints in native code.
    fn native_debug(&self) -> bool {
        false
    }

    /// Inclusive fixnum limits of the runtime that will execute this function.
    fn fixnum_bounds(&self) -> (i64, i64) {
        (FIXNUM_MIN, FIXNUM_MAX)
    }

    /// Returns the site profile recorded for bytecode PC `pc`.
    fn call_site_profile(&self, pc: usize) -> CallSiteProfile {
        let _ = pc;
        CallSiteProfile::Unprofiled
    }

    /// Returns `(package, symbol_name)` for constant-pool index `const_idx`, if known.
    fn named_symbol(&self, const_idx: usize) -> Option<(&str, &str)> {
        let _ = const_idx;
        None
    }

    /// Optional direct override identifying a fixnum arithmetic operator at `pc`.
    fn fixnum_arith_op(&self, pc: usize) -> Option<ArithOp> {
        let _ = pc;
        None
    }

    /// Optional direct override identifying a fixnum comparison operator at `pc`.
    fn fixnum_cmp_op(&self, pc: usize) -> Option<CmpOp> {
        let _ = pc;
        None
    }

    /// Optional static type tag for constant-pool entry `const_idx`.
    fn constant_type(&self, const_idx: usize) -> TypeTag {
        let _ = const_idx;
        TypeTag::Any
    }

    /// Whether constant-pool symbol `const_idx` is proclaimed `NOTINLINE`.
    fn is_notinline(&self, const_idx: usize) -> bool {
        let _ = const_idx;
        false
    }

    /// An explicitly declared POPCOUNT lowering and its binding constant at this call site.
    fn popcount_vop(&self, _pc: usize) -> Option<(usize, PopcountLowering)> {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopcountLowering {
    Portable,
    Aarch64(Aarch64Popcount),
}

/// Use the information available in the bytecode and SSA alone.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSpecializationInputs;

impl SpecializationInputs for NoSpecializationInputs {}

/// SSA instruction payload covering `zkcl_bytecode::Instruction` semantics and
/// lowered/specialized optimizer operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SsaOp {
    // --- Constants, copies, and boxing/unboxing ---
    ConstWord(Immediate),
    ConstI64(i64),
    ConstBool(bool),
    LoadConst(usize),
    Copy(ValueId),
    SelectBool {
        cond: ValueId,
        when_true: Immediate,
        when_false: Immediate,
    },
    BoxFixnum(ValueId),
    /// Tagged arithmetic with a live primitive-binding check. NIL denotes a
    /// miss. For Inc and Dec the lifter supplies a tagged one as rhs.
    TryFixnumArithmetic {
        op: zkcl_bytecode::FixnumArithmetic,
        symbol: usize,
        lhs: ValueId,
        rhs: ValueId,
    },
    /// A guarded call whose successful path counts the bits of an unboxed u64.
    /// The output is a tagged fixnum. A miss replays the canonical named call.
    PopcountU64 {
        input: ValueId,
        symbol: usize,
        binding: usize,
        lowering: PopcountLowering,
    },
    UnboxFixnumGuard(ValueId),
    BoxFloat {
        format: FloatFormat,
        value: ValueId,
    },
    UnboxFloatGuard {
        format: FloatFormat,
        value: ValueId,
    },
    BoxI32x4(ValueId),
    UnboxI32x4Guard(ValueId),
    I32x4Binary {
        op: I32x4Operation,
        lhs: ValueId,
        rhs: ValueId,
    },
    /// Deoptimizes if the named function has been replaced, including by TRACE.
    GuardPrimitiveBinding {
        symbol: usize,
        primitive: GuardedPrimitive,
    },
    /// Deoptimizes on division by signed zero or a non-finite result.
    FloatArithChecked {
        format: FloatFormat,
        op: FloatArithOp,
        lhs: ValueId,
        rhs: ValueId,
    },
    /// Guards the AREF binding, array layout, element format, and subscript.
    ReadFloatVector {
        symbol: usize,
        format: FloatFormat,
        array: ValueId,
        index: ValueId,
    },
    /// Guards the FUNCALL binding, setter, array layout, format, and subscript before writing.
    WriteFloatVector {
        symbol: usize,
        format: FloatFormat,
        setter: ValueId,
        array: ValueId,
        index: ValueId,
        value: ValueId,
    },

    // --- Unboxed fixnum arithmetic & comparisons ---
    AddI64Checked(ValueId, ValueId),
    SubI64Checked(ValueId, ValueId),
    MulI64Checked(ValueId, ValueId),
    AddI64Unchecked(ValueId, ValueId),
    SubI64Unchecked(ValueId, ValueId),
    MulI64Unchecked(ValueId, ValueId),
    CmpI64 {
        op: CmpOp,
        lhs: ValueId,
        rhs: ValueId,
    },

    // --- Argument & multiple-value operations ---
    IsPresent(ValueId),
    IsNil(ValueId),
    IsFixnum(ValueId),
    IsFixnumInRange {
        value: ValueId,
        low: i32,
        high: i32,
    },
    NotBool(ValueId),
    SetValues(Vec<ValueId>),
    ReadValue(usize),
    /// A field of a destructuring result.
    ExtractValue {
        source: ValueId,
        index: usize,
    },
    SaveValues,
    AppendValues,
    RestoreValues,
    OptionalArgument(usize),
    KeywordArgument(usize),
    RestArguments {
        start: usize,
        at: usize,
    },
    CheckKeywords {
        start: usize,
        table: usize,
    },
    DestructureNext {
        cursor: ValueId,
        required: bool,
        macro_call: bool,
        at: usize,
    },
    DestructureEnd {
        cursor: ValueId,
        macro_call: bool,
        at: usize,
    },
    DestructureKeywords {
        cursor: ValueId,
        table: usize,
        width: usize,
        macro_call: bool,
        at: usize,
    },
    ReleaseArguments,

    // --- Symbol cells, globals, dynamic bindings, and closures ---
    ReadCell {
        symbol: usize,
        function: bool,
        at: usize,
    },
    WriteCell {
        symbol: usize,
        src: ValueId,
    },
    DefineFunction {
        symbol: usize,
        definition: ValueId,
        is_macro: bool,
        at: usize,
    },
    ProclaimSpecial {
        symbol: usize,
        at: usize,
    },
    ProclaimInline {
        symbol: usize,
        setf: bool,
        notinline: bool,
    },
    SetVariableDocumentation {
        symbol: usize,
        doc: usize,
    },
    IsSymbolBound {
        symbol: usize,
    },
    BindSpecial {
        symbol: usize,
        value: ValueId,
    },
    BindProgv {
        symbols: ValueId,
        values: ValueId,
        at: usize,
    },
    Unbind(usize),
    BoxLocal {
        value: ValueId,
        at: usize,
    },
    ReadBox(CellTarget),
    WriteBox {
        cell: CellTarget,
        src: ValueId,
    },
    ReadCapture(usize),
    Closure {
        code: usize,
        cells: Vec<ValueId>,
        at: usize,
    },

    // --- Calls ---
    CallNamed {
        symbol: usize,
        args: Vec<ValueId>,
        has_result: bool,
        at: usize,
        associate: bool,
        notinline: bool,
        profile: CallSiteProfile,
    },
    CallValue {
        callee: ValueId,
        args: Vec<ValueId>,
        has_result: bool,
        at: usize,
        associate: bool,
        notinline: bool,
        profile: CallSiteProfile,
    },
    CallValues {
        callee: ValueId,
        at: usize,
    },

    /// A bounded execution checkpoint before a bytecode control transfer.
    /// When values are static, publish them only on the exit to bytecode.
    Checkpoint {
        values: Option<Vec<ValueId>>,
    },

    /// Publish canonical registers before executing this bytecode instruction.
    DebugPoint {
        pc: usize,
        registers: Vec<ValueId>,
    },
    /// Reload a register after the debugger may have changed it.
    DebugRegister(usize),

    // --- Control scopes (handlers, restarts, blocks, catches, tagbody, protect) ---
    /// Keep the current bytecode register in a root slot for non-local entries.
    StoreUnwindRegister {
        reg: Register,
        value: ValueId,
    },
    ExitToken(usize),
    EnterBlock {
        token: ValueId,
        resume: BlockId,
        clear: Register,
    },
    EnterCatch {
        tag: ValueId,
        resume: BlockId,
        clear: Register,
    },
    EnterTagbody {
        token: ValueId,
        jumps: usize,
        targets: Vec<BlockId>,
        clear: Register,
    },
    LeaveTarget,
    EnterProtect {
        cleanup: BlockId,
        clear: Register,
    },
    BeginCleanup,
    EndCleanup,
    EnterHandlers {
        table: usize,
        /// Handler `case` targets in table order (function handlers excluded).
        targets: Vec<BlockId>,
        function_targets: bool,
    },
    LeaveHandlers,
    EnterRestarts {
        table: usize,
        at: usize,
    },
    LeaveRestarts,
    AssociateRestarts {
        condition: ValueId,
        list: ValueId,
        at: usize,
    },
    LeaveRestartAssociation,
    /// Reads the signaled condition object bound upon entering a `HandlerTarget::Case` block.
    HandlerCondition,
}

// Matching a shared or mutable SsaOp yields shared or mutable operand references.
// Both traversals use the same exhaustive match and preserve operand order.
macro_rules! for_each_operand {
    ($op:expr, $visit:ident) => {
        match $op {
            Self::Copy(v)
            | Self::BoxFixnum(v)
            | Self::PopcountU64 { input: v, .. }
            | Self::UnboxFixnumGuard(v)
            | Self::BoxFloat { value: v, .. }
            | Self::UnboxFloatGuard { value: v, .. }
            | Self::BoxI32x4(v)
            | Self::UnboxI32x4Guard(v)
            | Self::IsPresent(v)
            | Self::IsNil(v)
            | Self::IsFixnum(v)
            | Self::IsFixnumInRange { value: v, .. }
            | Self::NotBool(v)
            | Self::ExtractValue { source: v, .. }
            | Self::DestructureNext { cursor: v, .. }
            | Self::DestructureEnd { cursor: v, .. }
            | Self::DestructureKeywords { cursor: v, .. }
            | Self::StoreUnwindRegister { value: v, .. }
            | Self::WriteCell { src: v, .. }
            | Self::DefineFunction { definition: v, .. }
            | Self::BindSpecial { value: v, .. }
            | Self::BoxLocal { value: v, .. }
            | Self::WriteBox {
                cell: CellTarget::Capture(_),
                src: v,
            }
            | Self::ReadBox(CellTarget::Local(v))
            | Self::CallValues { callee: v, .. }
            | Self::EnterBlock { token: v, .. }
            | Self::EnterCatch { tag: v, .. }
            | Self::EnterTagbody { token: v, .. } => $visit(v),
            Self::SelectBool { cond, .. } => $visit(cond),
            Self::WriteFloatVector {
                setter,
                array,
                index,
                value,
                ..
            } => {
                $visit(setter);
                $visit(array);
                $visit(index);
                $visit(value);
            }
            Self::AddI64Checked(a, b)
            | Self::SubI64Checked(a, b)
            | Self::MulI64Checked(a, b)
            | Self::AddI64Unchecked(a, b)
            | Self::SubI64Unchecked(a, b)
            | Self::MulI64Unchecked(a, b)
            | Self::CmpI64 { lhs: a, rhs: b, .. }
            | Self::TryFixnumArithmetic { lhs: a, rhs: b, .. }
            | Self::FloatArithChecked { lhs: a, rhs: b, .. }
            | Self::I32x4Binary { lhs: a, rhs: b, .. }
            | Self::ReadFloatVector {
                array: a, index: b, ..
            }
            | Self::BindProgv {
                symbols: a,
                values: b,
                ..
            }
            | Self::WriteBox {
                cell: CellTarget::Local(a),
                src: b,
            }
            | Self::AssociateRestarts {
                condition: a,
                list: b,
                ..
            } => {
                $visit(a);
                $visit(b);
            }
            Self::DebugPoint {
                registers: vals, ..
            }
            | Self::SetValues(vals)
            | Self::Closure { cells: vals, .. }
            | Self::CallNamed { args: vals, .. } => {
                for v in vals {
                    $visit(v);
                }
            }
            Self::CallValue { callee, args, .. } => {
                $visit(callee);
                for v in args {
                    $visit(v);
                }
            }
            Self::Checkpoint { values } => {
                if let Some(values) = values {
                    for value in values {
                        $visit(value);
                    }
                }
            }
            Self::ConstWord(_)
            | Self::ConstI64(_)
            | Self::ConstBool(_)
            | Self::LoadConst(_)
            | Self::DebugRegister(_)
            | Self::ReadValue(_)
            | Self::SaveValues
            | Self::AppendValues
            | Self::RestoreValues
            | Self::OptionalArgument(_)
            | Self::KeywordArgument(_)
            | Self::RestArguments { .. }
            | Self::CheckKeywords { .. }
            | Self::ReleaseArguments
            | Self::ReadCell { .. }
            | Self::ProclaimSpecial { .. }
            | Self::ProclaimInline { .. }
            | Self::SetVariableDocumentation { .. }
            | Self::IsSymbolBound { .. }
            | Self::Unbind(_)
            | Self::ReadBox(CellTarget::Capture(_))
            | Self::ReadCapture(_)
            | Self::GuardPrimitiveBinding { .. }
            | Self::ExitToken(_)
            | Self::LeaveTarget
            | Self::EnterProtect { .. }
            | Self::BeginCleanup
            | Self::EndCleanup
            | Self::EnterHandlers { .. }
            | Self::LeaveHandlers
            | Self::EnterRestarts { .. }
            | Self::LeaveRestarts
            | Self::LeaveRestartAssociation
            | Self::HandlerCondition => {}
        }
    };
}

impl SsaOp {
    /// Conservative [`OpEffects`] for this operation.
    pub fn effects(&self) -> OpEffects {
        match self {
            Self::ConstWord(_)
            | Self::ConstI64(_)
            | Self::ConstBool(_)
            | Self::LoadConst(_)
            | Self::Copy(_)
            | Self::SelectBool { .. }
            | Self::BoxFixnum(_)
            | Self::I32x4Binary { .. }
            | Self::CmpI64 { .. }
            | Self::IsPresent(_)
            | Self::IsNil(_)
            | Self::IsFixnum(_)
            | Self::IsFixnumInRange { .. }
            | Self::NotBool(_)
            | Self::ExtractValue { .. } => OpEffects::PURE,

            Self::AddI64Unchecked(_, _)
            | Self::SubI64Unchecked(_, _)
            | Self::MulI64Unchecked(_, _) => OpEffects {
                barrier: BarrierEffect::DEPENDS_ON_CHECKS,
                ..OpEffects::PURE
            },

            Self::UnboxFixnumGuard(_)
            | Self::UnboxFloatGuard { .. }
            | Self::UnboxI32x4Guard(_)
            | Self::FloatArithChecked { .. }
            | Self::AddI64Checked(_, _)
            | Self::SubI64Checked(_, _)
            | Self::MulI64Checked(_, _) => OpEffects {
                control: ControlEffect::MAY_DEOPT,
                ..OpEffects::PURE
            },

            Self::GuardPrimitiveBinding { .. } => OpEffects {
                reads: AliasDomain::SYMBOL_FN,
                writes: AliasDomain::EMPTY,
                control: ControlEffect::MAY_DEOPT,
                barrier: BarrierEffect::REQUIRED_WHEN_UNUSED,
            },
            Self::PopcountU64 { .. } => OpEffects {
                reads: AliasDomain::SYMBOL_FN,
                writes: AliasDomain::EMPTY,
                control: ControlEffect::MAY_DEOPT,
                barrier: BarrierEffect::EMPTY,
            },

            Self::ReadFloatVector { .. } => OpEffects {
                reads: AliasDomain::SYMBOL_FN.union(AliasDomain::ARRAY_ELEM),
                writes: AliasDomain::EMPTY,
                control: ControlEffect::MAY_DEOPT,
                barrier: BarrierEffect::DEPENDS_ON_CHECKS,
            },
            Self::WriteFloatVector { .. } => OpEffects {
                reads: AliasDomain::SYMBOL_FN.union(AliasDomain::ARRAY_ELEM),
                writes: AliasDomain::ARRAY_ELEM,
                control: ControlEffect::MAY_DEOPT,
                barrier: BarrierEffect::DEPENDS_ON_CHECKS,
            },

            Self::Checkpoint { .. } => OpEffects::CHECKPOINT,
            Self::DebugPoint { .. } => OpEffects {
                reads: AliasDomain::ALL_MEM,
                writes: AliasDomain::ALL_MEM,
                control: ControlEffect::MAY_UNWIND.union(ControlEffect::MAY_SUSPEND),
                barrier: BarrierEffect::ALLOC_FRESH
                    .union(BarrierEffect::MAY_GC)
                    .union(BarrierEffect::REQUIRED_WHEN_UNUSED),
            },
            Self::DebugRegister(_) => OpEffects {
                reads: AliasDomain::ALL_MEM,
                ..OpEffects::PURE
            },
            Self::BoxFloat { .. } | Self::BoxI32x4(_) => OpEffects {
                reads: AliasDomain::EMPTY,
                writes: AliasDomain::EMPTY,
                control: ControlEffect::MAY_UNWIND,
                barrier: BarrierEffect::ALLOC_FRESH.union(BarrierEffect::MAY_GC),
            },

            Self::ReadBox(_) | Self::ReadCapture(_) => OpEffects {
                reads: AliasDomain::BOX_CELL,
                ..OpEffects::PURE
            },
            Self::ReadValue(_) | Self::OptionalArgument(_) => OpEffects {
                reads: AliasDomain::VALUES_BUF,
                ..OpEffects::PURE
            },
            Self::HandlerCondition => OpEffects {
                reads: AliasDomain::DYNAMIC_STACK,
                ..OpEffects::PURE
            },
            Self::IsSymbolBound { .. } => OpEffects {
                reads: AliasDomain::SYMBOL_VALUE,
                ..OpEffects::PURE
            },
            Self::TryFixnumArithmetic { .. } => OpEffects {
                reads: AliasDomain::SYMBOL_FN,
                ..OpEffects::PURE
            },
            Self::KeywordArgument(_) => OpEffects {
                reads: AliasDomain::VALUES_BUF,
                writes: AliasDomain::EMPTY,
                control: ControlEffect::MAY_UNWIND,
                barrier: BarrierEffect::EMPTY,
            },
            // Failed reads can run Lisp handlers and resume through a restart.
            Self::ReadCell { .. } => OpEffects::CL_SIGNAL,

            Self::WriteCell { .. } => OpEffects {
                writes: AliasDomain::SYMBOL_VALUE,
                control: ControlEffect::MAY_UNWIND,
                ..OpEffects::PURE
            },

            Self::DefineFunction { .. } | Self::ProclaimInline { .. } => OpEffects {
                reads: AliasDomain::EMPTY,
                writes: AliasDomain::SYMBOL_FN,
                control: ControlEffect::MAY_UNWIND,
                barrier: BarrierEffect::ALLOC_FRESH.union(BarrierEffect::MAY_GC),
            },
            Self::ProclaimSpecial { .. } | Self::SetVariableDocumentation { .. } => OpEffects {
                reads: AliasDomain::EMPTY,
                writes: AliasDomain::SYMBOL_VALUE,
                control: ControlEffect::MAY_UNWIND,
                barrier: BarrierEffect::ALLOC_FRESH.union(BarrierEffect::MAY_GC),
            },

            Self::WriteBox { .. } => OpEffects {
                writes: AliasDomain::BOX_CELL,
                ..OpEffects::PURE
            },
            Self::StoreUnwindRegister { .. } => OpEffects {
                writes: AliasDomain::DYNAMIC_STACK,
                barrier: BarrierEffect::REQUIRED_WHEN_UNUSED,
                ..OpEffects::PURE
            },

            Self::SaveValues | Self::AppendValues | Self::RestoreValues => OpEffects {
                reads: AliasDomain::VALUES_BUF.union(AliasDomain::DYNAMIC_STACK),
                writes: AliasDomain::VALUES_BUF.union(AliasDomain::DYNAMIC_STACK),
                control: ControlEffect::EMPTY,
                barrier: BarrierEffect::ALLOC_FRESH
                    .union(BarrierEffect::MAY_GC)
                    .union(BarrierEffect::REQUIRED_WHEN_UNUSED),
            },

            Self::SetValues(_) | Self::ReleaseArguments => OpEffects {
                writes: AliasDomain::VALUES_BUF,
                barrier: BarrierEffect::REQUIRED_WHEN_UNUSED,
                ..OpEffects::PURE
            },

            Self::BoxLocal { .. } | Self::Closure { .. } | Self::ExitToken(_) => OpEffects {
                barrier: BarrierEffect::ALLOC_FRESH.union(BarrierEffect::MAY_GC),
                ..OpEffects::PURE
            },
            Self::RestArguments { .. } => OpEffects {
                reads: AliasDomain::VALUES_BUF,
                barrier: BarrierEffect::ALLOC_FRESH.union(BarrierEffect::MAY_GC),
                ..OpEffects::PURE
            },

            Self::CheckKeywords { .. }
            | Self::DestructureNext { .. }
            | Self::DestructureEnd { .. }
            | Self::DestructureKeywords { .. } => OpEffects {
                reads: AliasDomain::CONS_FIELD.union(AliasDomain::VALUES_BUF),
                writes: AliasDomain::EMPTY,
                control: ControlEffect::MAY_UNWIND,
                barrier: BarrierEffect::ALLOC_FRESH.union(BarrierEffect::MAY_GC),
            },

            Self::BindSpecial { .. } | Self::BindProgv { .. } | Self::Unbind(_) => OpEffects {
                reads: AliasDomain::DYNAMIC_STACK,
                writes: AliasDomain::DYNAMIC_STACK.union(AliasDomain::SYMBOL_VALUE),
                control: ControlEffect::MAY_UNWIND,
                barrier: BarrierEffect::MAY_GC.union(BarrierEffect::REQUIRED_WHEN_UNUSED),
            },
            Self::EnterBlock { .. }
            | Self::EnterCatch { .. }
            | Self::EnterTagbody { .. }
            | Self::LeaveTarget
            | Self::EnterProtect { .. }
            | Self::BeginCleanup
            | Self::EndCleanup
            | Self::EnterHandlers { .. }
            | Self::LeaveHandlers
            | Self::EnterRestarts { .. }
            | Self::LeaveRestarts
            | Self::AssociateRestarts { .. }
            | Self::LeaveRestartAssociation => OpEffects {
                reads: AliasDomain::DYNAMIC_STACK,
                writes: AliasDomain::DYNAMIC_STACK,
                control: ControlEffect::MAY_UNWIND,
                barrier: BarrierEffect::MAY_GC.union(BarrierEffect::REQUIRED_WHEN_UNUSED),
            },

            Self::CallNamed { .. } | Self::CallValue { .. } | Self::CallValues { .. } => {
                OpEffects::UNKNOWN_CALL
            }
        }
    }

    /// Physical representation produced by this operation (if it defines a result).
    pub fn result_repr(&self) -> ValueRepr {
        match self {
            Self::UnboxFloatGuard { format, .. }
            | Self::FloatArithChecked { format, .. }
            | Self::ReadFloatVector { format, .. } => format.repr(),
            Self::UnboxI32x4Guard(_) | Self::I32x4Binary { .. } => ValueRepr::UnboxedI32x4,
            Self::ConstI64(_)
            | Self::UnboxFixnumGuard(_)
            | Self::AddI64Checked(_, _)
            | Self::SubI64Checked(_, _)
            | Self::MulI64Checked(_, _)
            | Self::AddI64Unchecked(_, _)
            | Self::SubI64Unchecked(_, _)
            | Self::MulI64Unchecked(_, _) => ValueRepr::UnboxedI64,
            Self::ConstBool(_)
            | Self::CmpI64 { .. }
            | Self::IsPresent(_)
            | Self::IsNil(_)
            | Self::IsFixnum(_)
            | Self::IsFixnumInRange { .. }
            | Self::NotBool(_)
            | Self::IsSymbolBound { .. } => ValueRepr::Bool,
            _ => ValueRepr::TaggedWord,
        }
    }

    /// Visit all input [`ValueId`]s used by this operation.
    pub fn visit_inputs(&self, mut f: impl FnMut(ValueId)) {
        let mut visit = |value: &ValueId| f(*value);
        for_each_operand!(self, visit);
    }

    /// Rewrite input [`ValueId`]s using `f`.
    pub fn rewrite_inputs(&mut self, mut f: impl FnMut(ValueId) -> ValueId) {
        let mut rewrite = |value: &mut ValueId| *value = f(*value);
        for_each_operand!(self, rewrite);
    }
}

/// One SSA instruction inside a [`BasicBlock`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SsaInst {
    pub id: Option<ValueId>,
    pub op: SsaOp,
    pub ty: TypeTag,

    pub pc: Option<usize>,
    pub deopt: Option<DeoptState>,
}

impl SsaInst {
    /// Visit operands needed by the instruction or by recovery from this instruction.
    pub(crate) fn visit_inputs(&self, mut visit: impl FnMut(ValueId)) {
        self.op.visit_inputs(&mut visit);
        if let Some(deopt) = &self.deopt {
            deopt.visit_values(visit);
        }
    }
}

/// Emits instructions for a single bytecode location during lifting or specialization.
pub(crate) struct InstBuilder<'a> {
    pub next_value_id: &'a mut u32,
    pub insts: &'a mut Vec<SsaInst>,
    pub pc: Option<usize>,
}

impl InstBuilder<'_> {
    pub fn fresh_value(&mut self) -> ValueId {
        fresh_value(self.next_value_id)
    }

    pub fn push(&mut self, id: Option<ValueId>, op: SsaOp, ty: TypeTag, deopt: Option<DeoptState>) {
        self.insts.push(SsaInst {
            id,

            op,
            ty,
            pc: self.pc,
            deopt,
        });
    }

    pub fn effect(&mut self, op: SsaOp) {
        self.push(None, op, TypeTag::Top, None);
    }

    pub fn value(&mut self, op: SsaOp, ty: TypeTag) -> ValueId {
        let id = self.fresh_value();
        self.push(Some(id), op, ty, None);
        id
    }

    pub fn guarded_value(&mut self, op: SsaOp, ty: TypeTag, deopt: DeoptState) -> ValueId {
        let id = self.fresh_value();
        self.push(Some(id), op, ty, Some(deopt));
        id
    }
}

/// Branch destination with block-parameter arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchTarget {
    pub block: BlockId,
    pub args: Vec<ValueId>,
}

/// Non-local control transfer terminators (`RETURN-FROM`, `THROW`, `GO`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum UnwindOp {
    ReturnFrom {
        token: ValueId,
        at: usize,
    },
    Throw {
        tag: ValueId,
        at: usize,
    },
    Go {
        token: ValueId,
        label: usize,
        at: usize,
    },
}

/// Block terminator instruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Terminator {
    Jump(BranchTarget),
    BranchTruthy {
        cond: ValueId,
        truthy: BranchTarget,
        falsy: BranchTarget,
    },
    BranchBool {
        cond: ValueId,
        then_target: BranchTarget,
        else_target: BranchTarget,
    },
    ReturnOne(ValueId),
    ReturnValues(Vec<ValueId>),
    /// Returns whatever values are currently held in the runtime `Values` buffer.
    ReturnDynamicValues,
    Unwind(UnwindOp),
}

// Match by reference so read-only and in-place traversals see the same edges.
macro_rules! for_each_branch_target {
    ($term:expr, $visit:ident) => {
        match $term {
            Terminator::Jump(target) => $visit(target),
            Terminator::BranchTruthy {
                truthy: first,
                falsy: second,
                ..
            }
            | Terminator::BranchBool {
                then_target: first,
                else_target: second,
                ..
            } => {
                $visit(first);
                $visit(second);
            }
            Terminator::ReturnOne(_)
            | Terminator::ReturnValues(_)
            | Terminator::ReturnDynamicValues
            | Terminator::Unwind(_) => {}
        }
    };
}

// Direct operands precede outgoing edge arguments. Each variant holds either
// a single operand, a contiguous list of return values, or no direct operands.
macro_rules! direct_operands {
    ($term:expr, $singleton:path, $empty:expr) => {
        match $term {
            Terminator::BranchTruthy { cond: value, .. }
            | Terminator::BranchBool { cond: value, .. }
            | Terminator::ReturnOne(value)
            | Terminator::Unwind(
                UnwindOp::ReturnFrom { token: value, .. }
                | UnwindOp::Throw { tag: value, .. }
                | UnwindOp::Go { token: value, .. },
            ) => $singleton(value),
            Terminator::ReturnValues(values) => values,
            Terminator::Jump(_) | Terminator::ReturnDynamicValues => $empty,
        }
    };
}

impl Terminator {
    /// Visit all outgoing branch targets.
    pub fn visit_targets<'a>(&'a self, mut f: impl FnMut(&'a BranchTarget)) {
        for_each_branch_target!(self, f);
    }

    /// Mutably visit all outgoing branch targets.
    pub fn visit_targets_mut(&mut self, mut f: impl FnMut(&mut BranchTarget)) {
        for_each_branch_target!(self, f);
    }

    pub(crate) fn direct_inputs(&self) -> &[ValueId] {
        direct_operands!(self, std::slice::from_ref, &[])
    }

    pub(crate) fn direct_inputs_mut(&mut self) -> &mut [ValueId] {
        direct_operands!(self, std::slice::from_mut, &mut [])
    }

    /// Visit all SSA values consumed by this terminator (condition, return values, branch args).
    pub fn visit_inputs(&self, mut f: impl FnMut(ValueId)) {
        self.direct_inputs().iter().copied().for_each(&mut f);
        self.visit_targets(|target| target.args.iter().copied().for_each(&mut f));
    }

    /// Rewrite all SSA values consumed by this terminator.
    pub fn rewrite_inputs(&mut self, mut f: impl FnMut(ValueId) -> ValueId) {
        let mut rewrite = |value: &mut ValueId| *value = f(*value);
        self.direct_inputs_mut().iter_mut().for_each(&mut rewrite);
        self.visit_targets_mut(|target| target.args.iter_mut().for_each(&mut rewrite));
    }
}

/// Identifier of a dynamic control scope inside an [`SsaFunction`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ScopeId(pub u32);

impl ScopeId {
    /// Root dynamic scope at function entry.
    pub const ROOT: Self = Self(0);

    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Kind of dynamic control scope established by Lisp control or binding forms.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ControlScopeKind {
    Root,
    Target,
    Protect,
    Cleanup,
    Handler,
    Restart,
    Association,
    Dynamic,
    Values,
}

/// One node in an [`SsaFunction`]'s dynamic control scope tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ControlScopeNode {
    pub kind: ControlScopeKind,
    pub parent: Option<ScopeId>,
}

/// Dynamic control scope hierarchy preserved from bytecode lifting.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopeTree {
    nodes: Vec<ControlScopeNode>,
}

impl Default for ScopeTree {
    fn default() -> Self {
        Self {
            nodes: vec![ControlScopeNode {
                kind: ControlScopeKind::Root,
                parent: None,
            }],
        }
    }
}

impl ScopeTree {
    /// Construct a [`ScopeTree`] containing only [`ScopeId::ROOT`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Root scope identifier (`ScopeId::ROOT`).
    #[inline]
    pub const fn root(&self) -> ScopeId {
        ScopeId::ROOT
    }

    /// Push a new child scope under `parent`.
    pub fn push(&mut self, kind: ControlScopeKind, parent: ScopeId) -> ScopeId {
        let id = ScopeId(u32::try_from(self.nodes.len()).expect("ScopeId overflow"));
        self.nodes.push(ControlScopeNode {
            kind,
            parent: Some(parent),
        });
        id
    }

    /// Look up a scope node by [`ScopeId`].
    #[inline]
    pub fn get(&self, id: ScopeId) -> &ControlScopeNode {
        &self.nodes[id.index()]
    }

    /// Immediate enclosing parent of `id`, or `None` for the root scope.
    #[inline]
    pub fn parent(&self, id: ScopeId) -> Option<ScopeId> {
        self.nodes[id.index()].parent
    }

    /// Total number of scope nodes including the root scope.
    #[inline]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the internal node list is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns `true` if at least one non-root scope exists in this function.
    #[inline]
    pub fn has_non_root_scopes(&self) -> bool {
        self.nodes.len() > 1
    }

    /// Nesting depth of `id` from [`ScopeId::ROOT`] (depth 0).
    pub fn depth(&self, mut id: ScopeId) -> usize {
        let mut depth = 0;
        while let Some(parent) = self.parent(id) {
            depth += 1;
            id = parent;
        }
        depth
    }

    /// Slice of all scope nodes indexed by [`ScopeId::index`].
    #[inline]
    pub fn nodes(&self) -> &[ControlScopeNode] {
        &self.nodes
    }
}

/// Incoming block parameter (`phi` equivalent).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockParam {
    pub value: ValueId,
    pub ty: TypeTag,
    pub repr: ValueRepr,
    pub origin_reg: Option<Register>,
}

/// One SSA basic block in an [`SsaFunction`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BasicBlock {
    pub id: BlockId,
    pub params: Vec<BlockParam>,
    pub insts: Vec<SsaInst>,
    pub terminator: Terminator,
    /// Bytecode location of the control transfer, or the end PC for an empty return block.
    pub terminator_pc: usize,
    /// Dynamic control scope active upon entering this basic block.
    pub entry_scope: ScopeId,
    /// Non-local unwind / handler / restart / protect / tagbody targets reachable from this block.
    pub unwind_succs: Vec<BlockId>,
}

/// A function in SSA form, from bytecode lifting through native lowering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SsaFunction {
    /// Inclusive integer range available to tagged fixnums on the execution target.
    pub fixnum_bounds: (i64, i64),
    pub arity: Arity,
    /// SSA constant operands refer to the original bytecode's pool.
    pub constant_count: usize,
    pub entry: BlockId,
    pub entry_params: Vec<ValueId>,
    pub blocks: Vec<BasicBlock>,
    /// Dynamic control scope tree preserved from bytecode lifting.
    pub scopes: ScopeTree,
    /// Mapping from bytecode PC to lifted basic block (for side-table targets and diagnostics).
    pub pc_to_block: FxHashMap<usize, BlockId>,
    pub next_value_id: u32,
}

impl SsaFunction {
    /// Allocate a fresh [`ValueId`].
    #[inline]
    pub fn fresh_value(&mut self) -> ValueId {
        fresh_value(&mut self.next_value_id)
    }

    /// Look up a [`BasicBlock`] by [`BlockId`].
    #[inline]
    pub fn block(&self, id: BlockId) -> Option<&BasicBlock> {
        self.blocks.iter().find(|b| b.id == id)
    }

    /// Mutably look up a [`BasicBlock`] by [`BlockId`].
    #[inline]
    pub fn block_mut(&mut self, id: BlockId) -> Option<&mut BasicBlock> {
        self.blocks.iter_mut().find(|b| b.id == id)
    }

    /// Index lookup map `BlockId -> index in self.blocks`.
    pub fn block_index_map(&self) -> FxHashMap<BlockId, usize> {
        self.blocks
            .iter()
            .enumerate()
            .map(|(idx, b)| (b.id, idx))
            .collect()
    }

    /// Compute `(repr, ty)` for every currently defined [`ValueId`].
    pub fn value_metadata(&self) -> FxHashMap<ValueId, (ValueRepr, TypeTag)> {
        let mut by_value = FxHashMap::default();
        for block in &self.blocks {
            for param in &block.params {
                by_value.insert(param.value, (param.repr, param.ty));
            }
            for inst in &block.insts {
                if let Some(id) = inst.id {
                    by_value.insert(id, (inst.op.result_repr(), inst.ty));
                }
            }
        }
        by_value
    }
}

fn fresh_value(next: &mut u32) -> ValueId {
    let id = ValueId(*next);
    *next = next.checked_add(1).expect("ValueId overflow");
    id
}
