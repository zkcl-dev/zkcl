use std::ops::Range;

use crate::{Callee, CellLocation, Instruction, Register, RestartSpec, RestartTarget};

/// Explicit frame-register operands of one bytecode instruction.
pub struct RegisterOperands<'a> {
    pub reads: RegisterSet<'a>,
    pub writes: RegisterSet<'a>,
}

/// The scalar and contiguous operands of a canonical instruction.
#[derive(Clone, Copy)]
pub enum RegisterSet<'a> {
    Empty,
    One(Register),
    Two(Register, Register),
    Span(&'a Range<Register>),
    OneAndSpan(Register, &'a Range<Register>),
}

impl RegisterSet<'_> {
    #[inline]
    pub fn contains(self, reg: Register) -> bool {
        match self {
            Self::Empty => false,
            Self::One(first) => first == reg,
            Self::Two(first, second) => first == reg || second == reg,
            Self::Span(range) => range.contains(&reg),
            Self::OneAndSpan(first, range) => first == reg || range.contains(&reg),
        }
    }

    /// Visit each operand, including repeated operands for aliased registers.
    #[inline]
    pub fn for_each(self, mut visit: impl FnMut(Register)) {
        match self {
            Self::Empty => {}
            Self::One(first) => visit(first),
            Self::Two(first, second) => {
                visit(first);
                visit(second);
            }
            Self::Span(range) => range.clone().for_each(visit),
            Self::OneAndSpan(first, range) => {
                visit(first);
                range.clone().for_each(visit);
            }
        }
    }
}

impl Instruction {
    /// Direct register reads and writes along the instruction's ordinary path.
    /// A register that is both read and written is an input, even when its write
    /// is conditional. Control transfers and side-table accesses are separate.
    #[inline]
    pub fn register_operands(&self) -> RegisterOperands<'_> {
        use Instruction::*;
        use RegisterSet::{Empty, One, OneAndSpan, Span, Two};

        let (reads, writes) = match self {
            Load(out, _)
            | Constant(out, _)
            | ReadValue(out, _)
            | ReadCell { out, .. }
            | Capture(out, _)
            | ExitToken(out, _)
            | OptionalArgument(_, out)
            | KeywordArgument(_, out)
            | RestArguments { out, .. } => (Empty, One(*out)),
            Copy(out, src) => (One(*src), One(*out)),
            TryFixnumArithmetic { out, args, .. } => (Span(args), One(*out)),
            Clear(range) | BindValues(range) => (Empty, Span(range)),
            SetValues(range) => (Span(range), Empty),
            Unless(src, _)
            | IfPresent(src, _)
            | UnlessFixnum(src, _)
            | UnlessFixnumInRange(src, _, _, _)
            | WriteCell { src, .. }
            | DefineFunction {
                definition: src, ..
            }
            | BindSpecial { value: src, .. }
            | WriteBox {
                cell: CellLocation::Capture(_),
                src,
            }
            | EnterBlock { token: src, .. }
            | EnterCatch { tag: src, .. }
            | EnterTagbody { token: src, .. }
            | ReturnFrom { token: src, .. }
            | Throw { tag: src, .. }
            | Go { token: src, .. }
            | DestructureEnd { cursor: src, .. }
            | CallValues { callee: src, .. } => (One(*src), Empty),
            BindProgv {
                symbols, values, ..
            } => (Two(*symbols, *values), Empty),
            AssociateRestarts {
                condition, list, ..
            } => (Two(*condition, *list), Empty),
            BoxLocal { reg, .. } => (One(*reg), One(*reg)),
            ReadBox { out, cell } => {
                let reads = match cell {
                    CellLocation::Local(reg) => One(*reg),
                    CellLocation::Capture(_) => Empty,
                };
                (reads, One(*out))
            }
            WriteBox {
                cell: CellLocation::Local(reg),
                src,
            } => (Two(*reg, *src), Empty),
            Closure { out, cells, .. } => (Span(cells), One(*out)),
            DestructureNext { cursor, out, .. } => (One(*cursor), Two(*out, *cursor)),
            DestructureKeywords { cursor, out, .. } => (One(*cursor), Span(out)),
            Call {
                callee, out, args, ..
            } => {
                let reads = match callee {
                    Callee::Named(_) => Span(args),
                    Callee::Value(reg) => OneAndSpan(*reg, args),
                };
                (reads, out.map_or(Empty, One))
            }
            Jump(_)
            | SaveValues
            | AppendValues
            | RestoreValues
            | ProclaimSpecial { .. }
            | ProclaimInline { .. }
            | SetVariableDocumentation { .. }
            | WhenBound { .. }
            | Unbind(_)
            | LeaveTarget
            | EnterProtect { .. }
            | BeginCleanup
            | EndCleanup
            | EnterHandlers { .. }
            | LeaveHandlers
            | EnterRestarts { .. }
            | LeaveRestarts
            | LeaveRestartAssociation
            | CheckKeywords { .. }
            | ReleaseArguments => (Empty, Empty),
        };
        RegisterOperands { reads, writes }
    }
}

impl RestartSpec {
    /// The restart's object slot, its report input at scope entry, and slots
    /// that later restart operations can read while the scope is active.
    #[inline]
    pub fn frame_registers(self) -> impl Iterator<Item = Register> {
        let function = match self.target {
            RestartTarget::Function(reg) => Some(reg),
            RestartTarget::Case { .. } => None,
        };
        std::iter::once(self.object).chain(
            [self.test, self.interactive, self.report, function]
                .into_iter()
                .flatten(),
        )
    }
}
