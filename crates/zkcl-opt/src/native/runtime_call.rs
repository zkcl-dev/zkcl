//! Runtime calls described in SSA terms. Backends materialize these operands
//! in ABI order and interpret the return record in their own control flow.

use zkcl_bytecode::native_abi::{JIT_CALL_PRIMARY_VALUE, RuntimeAbiFunction as Abi};

use crate::{BlockId, CellTarget, SsaOp, ValueId};
use zkcl_bytecode::{Register, WORD_NIL, WORD_UNBOUND};

use super::NativeFeature;
use super::word_ops::{I64Arithmetic, ScalarConstant, WordOp, WordPredicate};

#[derive(Clone, Copy)]
pub enum CallOperand<'a> {
    Context,
    Frame,
    Code,
    Word(usize),
    Bool(bool),
    Value(ValueId),
    /// A computed primary value, normally read from the runtime return record.
    SingleValue,
    Constant(usize),
    Cell(CellTarget),
    /// Cranelift reports the bytecode PC for lexical-box errors; the portable
    /// emitters currently pass zero. This is diagnostic metadata, not an operand.
    BoxLocation,
    Words(&'a [ValueId]),
    Length(&'a [ValueId]),
    CatchTag(ValueId),
    UnwindState(BlockId),
    TagbodyBase,
    HandlerStates(&'a [BlockId]),
    HandlerSlots(&'a [BlockId]),
    RootCount,
    ResultOut,
}

use CallOperand::*;

#[derive(Clone, Copy)]
pub enum CallReturn {
    Void,
    Word,
    /// Check the return record and use the direct single-value payload.
    One,
    /// Check the return record without reading its payload.
    Effect,
    /// A source point can hand an already materialized activation to the VM.
    DebugPoint,
    /// Read the cell result or hand the activation back to the VM.
    ReadCell,
    /// Preserve the callee's values unless the caller requests only the primary.
    LispCall {
        primary: bool,
    },
    /// A cleanup can transfer control; it doesn't have a bytecode handoff map.
    Cleanup,
}

#[derive(Clone, Copy)]
pub enum CallWhen {
    Always,
    SaveValues,
    ReleaseArguments,
    OptionalArgument(usize),
}

#[derive(Clone, Copy)]
pub(super) enum CallRequirement {
    Always,
    Unassociated { associate: bool },
    CaseHandlers { function_targets: bool },
    Requires(NativeFeature),
}

/// Poll for cancellation and the execution step limit before touching the
/// scheduler's slice. A missing slice needs no charge; a positive counter is
/// decremented by one. At zero, refill without decrementing: a false refill
/// result hands off to bytecode. The execution step limit is a separate budget.
#[derive(Clone, Copy)]
pub struct SliceCharge {
    cost: u16,
}

impl SliceCharge {
    pub const CHECKPOINT: Self = Self { cost: 1 };

    pub const fn cost(self) -> u16 {
        self.cost
    }

    pub fn poll(at: usize) -> RuntimeCall<'static> {
        RuntimeCall::new(
            Abi::PollExecution,
            CallReturn::Effect,
            [Context, Word(at), ResultOut],
        )
    }

    pub fn refill(self) -> RuntimeCall<'static> {
        RuntimeCall::new(Abi::RefillSlice, CallReturn::Word, [Context])
    }
}

/// The exhaustive native lowering table, including operations whose encoding
/// and feature requirements are specific to one or more targets.
#[derive(Clone, Copy)]
pub enum NativeOp<'a> {
    Call(RuntimeCall<'a>),
    Word(WordOp),
    Checkpoint {
        values: Option<&'a [ValueId]>,
        charge: SliceCharge,
    },
    /// Publish a value to the unwind root, if this frame has allocated it.
    StoreUnwindRoot {
        reg: Register,
        value: ValueId,
    },
    /// Read the condition from the current handler's root.
    LoadHandlerConditionRoot,
    Target {
        feature: Option<NativeFeature>,
        checks_recovery: bool,
    },
    Unsupported,
}

#[derive(Clone, Copy)]
pub struct RuntimeCall<'a> {
    pub function: Abi,
    pub result: CallReturn,
    pub when: CallWhen,
    pub(super) requirement: CallRequirement,
    /// Attach the bytecode location after the scope has been entered.
    pub mark_scope: bool,
    operands: [CallOperand<'a>; RuntimeCall::MAX_OPERANDS],
    len: usize,
}

impl<'a> RuntimeCall<'a> {
    pub const MAX_OPERANDS: usize = 8;
    fn new<const N: usize>(
        function: Abi,
        result: CallReturn,
        operands: [CallOperand<'a>; N],
    ) -> Self {
        let mut call = Self {
            function,
            result,
            when: CallWhen::Always,
            requirement: CallRequirement::Always,
            mark_scope: false,
            operands: [Context; Self::MAX_OPERANDS],
            len: N,
        };
        debug_assert_eq!(function.parameter_types().len(), N);
        call.operands[..N].copy_from_slice(&operands);
        call
    }

    pub fn operands(&self) -> &[CallOperand<'a>] {
        &self.operands[..self.len]
    }

    /// Use the register call ABI for primary-value sites with at most two arguments.
    /// The caller must decode a structured result when the scalar result escapes.
    pub fn primary_register_call(op: &'a SsaOp) -> Option<Self> {
        let (target, symbol, args, at) = match op {
            SsaOp::CallNamed {
                symbol,
                args,
                at,
                has_result: true,
                associate: false,
                notinline: false,
                ..
            } => (Code, *symbol, args, *at),
            SsaOp::CallValue {
                callee,
                args,
                at,
                has_result: true,
                associate: false,
                notinline: false,
                ..
            } => (Value(*callee), JIT_CALL_PRIMARY_VALUE as usize, args, *at),
            _ => return None,
        };
        let nil = Word(WORD_NIL as usize);
        let [a, b] = match args.as_slice() {
            [] => [nil, nil],
            &[a] => [Value(a), nil],
            &[a, b] => [Value(a), Value(b)],
            _ => return None,
        };
        Some(Self::new(
            Abi::CallPrimary,
            CallReturn::Word,
            [
                Context,
                target,
                Word(symbol),
                Word(at),
                Word(args.len()),
                a,
                b,
                ResultOut,
            ],
        ))
    }

    fn scope<const N: usize>(function: Abi, operands: [CallOperand<'a>; N]) -> Self {
        Self {
            mark_scope: true,
            ..RuntimeCall::new(function, CallReturn::Void, operands)
        }
    }

    pub fn constant(index: usize) -> Self {
        RuntimeCall::new(
            Abi::FunctionConstant,
            CallReturn::Word,
            [Context, Code, Word(index)],
        )
    }

    pub fn set_values(values: &'a [ValueId]) -> Self {
        if let [value] = values {
            Self::set_one_value(Value(*value))
        } else {
            RuntimeCall::new(
                Abi::SetValues,
                CallReturn::Void,
                [Context, Words(values), Length(values)],
            )
        }
    }

    pub fn read_value(index: usize) -> Self {
        RuntimeCall::new(Abi::GetValue, CallReturn::Word, [Context, Word(index)])
    }

    fn set_one_value(value: CallOperand<'a>) -> Self {
        Self::new(Abi::SetOneValue, CallReturn::Void, [Context, value])
    }

    pub fn preserve_single_value() -> Self {
        Self::set_one_value(SingleValue)
    }

    pub fn mark_scope(pc: usize) -> Self {
        RuntimeCall::new(Abi::MarkScope, CallReturn::Void, [Context, Word(pc)])
    }

    fn require(mut self, requirement: CallRequirement) -> Self {
        self.requirement = requirement;
        self
    }
}

impl<'a> NativeOp<'a> {
    pub fn for_op(op: &'a SsaOp) -> Self {
        #[cfg(feature = "counters")]
        super::counters::record();
        use CallRequirement::{CaseHandlers, Requires, Unassociated};
        use CallReturn::{Cleanup, Effect, LispCall, One, Void};
        use NativeFeature::*;
        use RuntimeCall as Call;
        use SsaOp as Op;

        let scalar = |op| Self::Word(WordOp::Constant(op));
        let arithmetic = |op, lhs, rhs| Self::Word(WordOp::I64(op, lhs, rhs));
        let target = |feature, checks_recovery| Self::Target {
            feature: Some(feature),
            checks_recovery,
        };

        Self::Call(match op {
            Op::ConstWord(value) => return scalar(ScalarConstant::Word(*value)),
            Op::ConstI64(value) => return scalar(ScalarConstant::I64(*value)),
            Op::ConstU64(value) => return scalar(ScalarConstant::U64(*value)),
            Op::ConstBool(value) => return scalar(ScalarConstant::Bool(*value)),
            Op::Copy(value) => return Self::Word(WordOp::Copy(*value)),
            Op::SelectBool {
                cond,
                when_true,
                when_false,
            } => {
                return Self::Word(WordOp::Select(*cond, *when_true, *when_false));
            }
            Op::BoxFixnum(value) => return Self::Word(WordOp::BoxFixnum(*value)),
            Op::IsPresent(value) | Op::IsNil(value) => {
                let (bits, equal) = if matches!(op, Op::IsNil(_)) {
                    (WORD_NIL, true)
                } else {
                    (WORD_UNBOUND, false)
                };
                return Self::Word(WordOp::Test(
                    *value,
                    WordPredicate {
                        mask: None,
                        bits,
                        equal,
                    },
                ));
            }
            Op::IsFixnum(value) => return Self::Word(WordOp::Test(*value, WordPredicate::FIXNUM)),
            Op::IsFixnumInRange { value, low, high } => {
                let low = i64::from(*low);
                return Self::Word(WordOp::FixnumRange {
                    value: *value,
                    low,
                    span: i64::from(*high) - low,
                });
            }
            Op::AddI64Unchecked(lhs, rhs) => return arithmetic(I64Arithmetic::Add, *lhs, *rhs),
            Op::SubI64Unchecked(lhs, rhs) => return arithmetic(I64Arithmetic::Sub, *lhs, *rhs),
            Op::MulI64Unchecked(lhs, rhs) => return arithmetic(I64Arithmetic::Mul, *lhs, *rhs),
            Op::BitwiseI64 { op, lhs, rhs } => {
                return Self::Word(WordOp::BitwiseI64(*op, *lhs, *rhs));
            }
            Op::CmpI64 { op, lhs, rhs } => return Self::Word(WordOp::CompareI64(*op, *lhs, *rhs)),
            Op::NotBool(value) => return Self::Word(WordOp::Not(*value)),
            Op::Checkpoint { values } => {
                return Self::Checkpoint {
                    values: values.as_deref(),
                    charge: SliceCharge::CHECKPOINT,
                };
            }
            Op::StoreUnwindRegister { reg, value } => {
                return Self::StoreUnwindRoot {
                    reg: *reg,
                    value: *value,
                };
            }
            Op::HandlerCondition => return Self::LoadHandlerConditionRoot,
            Op::TryFixnumArithmetic { .. } | Op::ReadCapture(_) => {
                return Self::Target {
                    feature: None,
                    checks_recovery: false,
                };
            }
            Op::UnboxFixnumGuard(_)
            | Op::AddI64Checked(..)
            | Op::SubI64Checked(..)
            | Op::MulI64Checked(..) => return target(CheckedFixnums, true),
            Op::GuardPrimitiveBinding { .. } => return target(PrimitiveGuards, true),
            Op::GuardTransformBinding { .. } | Op::UnboxU64Guard(_) => {
                return Self::Target {
                    feature: None,
                    checks_recovery: true,
                };
            }
            Op::WordCount { .. } => {
                return Self::Target {
                    feature: None,
                    checks_recovery: false,
                };
            }
            Op::UnboxFloatGuard { .. }
            | Op::FloatArithChecked { .. }
            | Op::ReadFloatVector { .. }
            | Op::WriteFloatVector { .. } => return target(Float, true),
            Op::BoxFloat { .. } => return target(Float, false),
            Op::UnboxI32x4Guard(_) => return target(I32x4, true),
            Op::BoxI32x4(_) | Op::I32x4Binary { .. } => return target(I32x4, false),
            Op::ExtractValue { .. }
            | Op::DestructureNext { .. }
            | Op::DestructureEnd { .. }
            | Op::DestructureKeywords { .. }
            | Op::EnterRestarts { .. }
            | Op::LeaveRestarts
            | Op::AssociateRestarts { .. }
            | Op::LeaveRestartAssociation => return Self::Unsupported,
            Op::DebugPoint { pc, registers } => RuntimeCall::new(
                Abi::DebugPoint,
                CallReturn::DebugPoint,
                [
                    Context,
                    Word(*pc),
                    Words(registers),
                    Length(registers),
                    ResultOut,
                ],
            ),
            Op::DebugRegister(index) => RuntimeCall::new(
                Abi::DebugRegister,
                CallReturn::Word,
                [Context, Word(*index)],
            ),
            Op::LoadConst(index) => Call::constant(*index),
            Op::SetValues(values) => Call::set_values(values),
            Op::ReadValue(index) => Call::read_value(*index),
            Op::SaveValues => Call {
                when: CallWhen::SaveValues,
                ..RuntimeCall::new(Abi::SaveValues, Void, [Context, Frame])
            },
            Op::AppendValues => RuntimeCall::new(Abi::AppendValues, Void, [Context]),
            Op::RestoreValues => RuntimeCall::new(Abi::RestoreValues, Void, [Context]),
            Op::ReleaseArguments => Call {
                when: CallWhen::ReleaseArguments,
                ..RuntimeCall::new(Abi::ReleaseArguments, Void, [Context, Frame])
            },
            Op::OptionalArgument(index) => Call {
                when: CallWhen::OptionalArgument(*index),
                ..RuntimeCall::new(
                    Abi::OptionalArgument,
                    CallReturn::Word,
                    [Context, Frame, Word(*index)],
                )
            },
            Op::KeywordArgument(index) => RuntimeCall::new(
                Abi::KeywordArgument,
                One,
                [Context, Frame, Word(*index), ResultOut],
            ),
            Op::RestArguments { start, at } => RuntimeCall::new(
                Abi::RestArguments,
                One,
                [Context, Frame, Word(*start), Word(*at), ResultOut],
            ),
            Op::CheckKeywords { start, table } => RuntimeCall::new(
                Abi::CheckKeywords,
                Effect,
                [Context, Frame, Code, Word(*start), Word(*table), ResultOut],
            ),
            Op::ReadCell {
                symbol,
                function,
                at,
            } => RuntimeCall::new(
                Abi::ReadCell,
                CallReturn::ReadCell,
                [
                    Context,
                    Constant(*symbol),
                    Bool(*function),
                    Word(*at),
                    ResultOut,
                ],
            ),
            Op::WriteCell { symbol, src } => RuntimeCall::new(
                Abi::WriteCell,
                Void,
                [Context, Constant(*symbol), Value(*src)],
            ),
            Op::DefineFunction {
                symbol,
                definition,
                is_macro,
                at,
            } => RuntimeCall::new(
                Abi::DefineFunction,
                Effect,
                [
                    Context,
                    Constant(*symbol),
                    Value(*definition),
                    Bool(*is_macro),
                    Word(*at),
                    ResultOut,
                ],
            ),
            Op::BindSpecial { symbol, value } => RuntimeCall::new(
                Abi::BindSpecial,
                Void,
                [Context, Frame, Constant(*symbol), Value(*value)],
            ),
            Op::BindProgv {
                symbols,
                values,
                at,
            } => RuntimeCall::new(
                Abi::BindProgv,
                Effect,
                [
                    Context,
                    Frame,
                    Value(*symbols),
                    Value(*values),
                    Word(*at),
                    ResultOut,
                ],
            ),
            Op::Unbind(count) => RuntimeCall::new(Abi::Unbind, Void, [Context, Word(*count)]),
            Op::BoxLocal { value, at } => RuntimeCall::new(
                Abi::BoxLocal,
                One,
                [Context, Value(*value), Word(*at), ResultOut],
            ),
            Op::ReadBox(cell) => RuntimeCall::new(
                Abi::ReadBox,
                One,
                [Context, Cell(*cell), BoxLocation, ResultOut],
            ),
            Op::WriteBox { cell, src } => RuntimeCall::new(
                Abi::WriteBox,
                Effect,
                [Context, Cell(*cell), Value(*src), BoxLocation, ResultOut],
            ),
            Op::Closure { code, cells, at } => RuntimeCall::new(
                Abi::AllocClosure,
                One,
                [
                    Context,
                    Code,
                    Word(*code),
                    Words(cells),
                    Length(cells),
                    Word(*at),
                    ResultOut,
                ],
            ),
            Op::CallNamed {
                symbol,
                args,
                has_result,
                at,
                associate,
                ..
            } => RuntimeCall::new(
                Abi::CallNamed,
                LispCall {
                    primary: *has_result,
                },
                [
                    Context,
                    Code,
                    Word(*symbol),
                    Words(args),
                    Length(args),
                    Word(*at),
                    ResultOut,
                ],
            )
            .require(Unassociated {
                associate: *associate,
            }),
            Op::CallValue {
                callee,
                args,
                has_result,
                at,
                associate,
                ..
            } => RuntimeCall::new(
                Abi::Invoke,
                LispCall {
                    primary: *has_result,
                },
                [
                    Context,
                    Value(*callee),
                    Words(args),
                    Length(args),
                    Word(*at),
                    ResultOut,
                ],
            )
            .require(Unassociated {
                associate: *associate,
            }),
            Op::CallValues { callee, at } => RuntimeCall::new(
                Abi::InvokeValues,
                LispCall { primary: false },
                [Context, Value(*callee), Word(*at), ResultOut],
            ),
            Op::ExitToken(at) => {
                RuntimeCall::new(Abi::ExitToken, One, [Context, Word(*at), ResultOut])
            }
            Op::EnterBlock { token, resume, .. } => RuntimeCall::scope(
                Abi::EnterBlock,
                [
                    Context,
                    Frame,
                    Value(*token),
                    UnwindState(*resume),
                    RootCount,
                ],
            ),
            Op::EnterCatch { tag, resume, .. } => RuntimeCall::scope(
                Abi::EnterCatch,
                [
                    Context,
                    Frame,
                    CatchTag(*tag),
                    UnwindState(*resume),
                    RootCount,
                ],
            ),
            Op::EnterTagbody { token, targets, .. } => RuntimeCall::scope(
                Abi::EnterTagbody,
                [
                    Context,
                    Frame,
                    Value(*token),
                    TagbodyBase,
                    Word(targets.len()),
                    RootCount,
                ],
            ),
            Op::EnterProtect { cleanup, .. } => RuntimeCall::scope(
                Abi::Protect,
                [Context, Frame, UnwindState(*cleanup), RootCount],
            ),
            Op::LeaveTarget => RuntimeCall::new(Abi::LeaveTarget, Void, [Context]),
            Op::BeginCleanup => RuntimeCall::new(Abi::BeginCleanup, Void, [Context]),
            Op::EndCleanup => RuntimeCall::new(Abi::EndCleanup, Cleanup, [Context, ResultOut]),
            Op::EnterHandlers {
                table,
                targets,
                function_targets,
            } => RuntimeCall::new(
                Abi::EnterHandlers,
                Void,
                [
                    Context,
                    Frame,
                    Code,
                    Word(*table),
                    HandlerStates(targets),
                    HandlerSlots(targets),
                    Word(targets.len()),
                ],
            )
            .require(CaseHandlers {
                function_targets: *function_targets,
            }),
            Op::LeaveHandlers => RuntimeCall::new(Abi::LeaveHandlers, Void, [Context]),
            Op::ProclaimSpecial { symbol, at } => Call::new(
                Abi::ProclaimSpecial,
                Effect,
                [Context, Constant(*symbol), Word(*at), ResultOut],
            )
            .require(Requires(Declarations)),
            Op::ProclaimInline {
                symbol,
                setf,
                notinline,
            } => Call::new(
                Abi::ProclaimInline,
                Void,
                [Context, Constant(*symbol), Bool(*setf), Bool(*notinline)],
            )
            .require(Requires(Declarations)),
            Op::SetVariableDocumentation { symbol, doc } => Call::new(
                Abi::SetVariableDocumentation,
                Void,
                [Context, Constant(*symbol), Constant(*doc)],
            )
            .require(Requires(Declarations)),
            Op::IsSymbolBound { symbol } => Call::new(
                Abi::IsSymbolBound,
                CallReturn::Word,
                [Context, Constant(*symbol)],
            )
            .require(Requires(Declarations)),
        })
    }
}
