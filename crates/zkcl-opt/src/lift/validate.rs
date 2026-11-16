use std::ops::{Range, RangeInclusive};

use zkcl_bytecode::{
    BytecodeBody, Callee, CaptureKind, CellLocation, HandlerTarget, Immediate, Instruction,
    Register, RegisterSet, RestartTarget, Test,
};

use super::BytecodeLiftError;

type Result = std::result::Result<(), BytecodeLiftError>;

pub(super) fn validate_code_bounds(code: &BytecodeBody<'_>, (low, high): (i64, i64)) -> Result {
    let arity = code.arity;
    if arity.required > code.registers || arity.maximum.is_some_and(|max| max < arity.required) {
        return Err(BytecodeLiftError::InvalidArity {
            required: arity.required,
            maximum: arity.maximum,
            registers: code.registers,
        });
    }

    let bounds = Bounds {
        code: *code,
        fixnums: low..=high,
    };
    bounds.check_tables()?;
    for (pc, instruction) in code.instructions.iter().enumerate() {
        bounds.check_instruction(pc, instruction)?;
    }
    Ok(())
}

struct Bounds<'a> {
    code: BytecodeBody<'a>,
    fixnums: RangeInclusive<i64>,
}

impl Bounds<'_> {
    fn check_register(&self, pc: Option<usize>, reg: Register) -> Result {
        self.check_register_bound(pc, reg, reg < self.code.registers)
    }

    fn check_register_bound(&self, pc: Option<usize>, reg: Register, in_bounds: bool) -> Result {
        in_bounds
            .then_some(())
            .ok_or(BytecodeLiftError::RegisterOutOfBounds {
                pc,
                reg,
                registers: self.code.registers,
            })
    }

    fn check_span(&self, pc: Option<usize>, span: &Range<Register>) -> Result {
        (span.start <= span.end && span.end <= self.code.registers)
            .then_some(())
            .ok_or(BytecodeLiftError::InvalidRegisterRange {
                pc,
                start: span.start,
                end: span.end,
                registers: self.code.registers,
            })
    }

    fn check_registers(&self, pc: Option<usize>, operands: RegisterSet<'_>) -> Result {
        match operands {
            RegisterSet::Empty => Ok(()),
            RegisterSet::One(reg) => self.check_register(pc, reg),
            RegisterSet::Two(first, second) => {
                self.check_register(pc, first)?;
                self.check_register(pc, second)
            }
            RegisterSet::Span(span) => self.check_span(pc, span),
            RegisterSet::OneAndSpan(reg, span) => {
                self.check_register(pc, reg)?;
                self.check_span(pc, span)
            }
        }
    }

    fn check_clear_boundary(&self, pc: Option<usize>, reg: Register) -> Result {
        self.check_register_bound(pc, reg, reg <= self.code.registers)
    }

    fn check_jump(&self, pc: Option<usize>, target: usize) -> Result {
        let len = self.code.instructions.len();
        (target <= len)
            .then_some(())
            .ok_or(BytecodeLiftError::JumpOutOfBounds { pc, target, len })
    }

    fn check_constant(&self, pc: Option<usize>, index: usize) -> Result {
        let len = self.code.constant_count;
        (index < len)
            .then_some(())
            .ok_or(BytecodeLiftError::ConstantOutOfBounds { pc, index, len })
    }

    fn capture(
        &self,
        pc: Option<usize>,
        index: usize,
    ) -> std::result::Result<CaptureKind, BytecodeLiftError> {
        let captures = self.code.captures;
        captures
            .get(index)
            .copied()
            .ok_or(BytecodeLiftError::CaptureOutOfBounds {
                pc,
                index,
                len: captures.len(),
            })
    }

    fn check_tables(&self) -> Result {
        for &index in self.code.notinline {
            self.check_constant(None, index as usize)?;
        }
        for table in self.code.handlers {
            for handler in &table.handlers {
                if let Test::Named(index) = handler.test {
                    self.check_constant(None, index)?;
                }
                match handler.target {
                    HandlerTarget::Function(reg) => self.check_register(None, reg)?,
                    HandlerTarget::Case {
                        pc,
                        condition,
                        clear,
                    } => {
                        self.check_jump(None, pc)?;
                        self.check_register(None, condition)?;
                        self.check_clear_boundary(None, clear)?;
                    }
                }
            }
        }
        for table in self.code.restarts {
            for spec in &table.restarts {
                self.check_constant(None, spec.name)?;
                for reg in spec.frame_registers() {
                    self.check_register(None, reg)?;
                }
                if let RestartTarget::Case { pc, clear } = spec.target {
                    self.check_jump(None, pc)?;
                    self.check_clear_boundary(None, clear)?;
                }
            }
        }
        for table in self.code.keywords {
            for index in std::iter::once(table.override_key).chain(table.names.iter().copied()) {
                self.check_constant(None, index)?;
            }
        }
        for table in self.code.jump_tables {
            for &target in &table.targets {
                self.check_jump(None, target)?;
            }
        }
        Ok(())
    }

    fn check_instruction(&self, pc: usize, inst: &Instruction) -> Result {
        use Instruction::*;

        let at = Some(pc);
        let registers = inst.register_operands();
        self.check_registers(at, registers.reads)?;
        self.check_registers(at, registers.writes)?;

        let check_table = |table, index, len| {
            (index < len)
                .then_some(())
                .ok_or(BytecodeLiftError::TableOutOfBounds {
                    pc: at,
                    table,
                    index,
                    len,
                })
        };
        match inst {
            Load(_, Immediate::Fixnum(value)) if !self.fixnums.contains(value) => {
                Err(BytecodeLiftError::InvalidFixnum { pc, value: *value })
            }
            OptionalArgument(index, _) => {
                let position = self.code.arity.required.checked_add(*index);
                if position.is_none_or(|position| {
                    self.code.arity.maximum.is_some_and(|max| position >= max)
                }) {
                    return Err(BytecodeLiftError::InvalidOptionalArgument { pc, index: *index });
                }
                Ok(())
            }
            TryFixnumArithmetic {
                op, symbol, args, ..
            } => {
                self.check_constant(at, *symbol)?;
                if args.len() != op.arity() {
                    return Err(BytecodeLiftError::InvalidFixnumArity {
                        pc,
                        op: *op,
                        count: args.len(),
                    });
                }
                Ok(())
            }
            Capture(_, index) => self.capture(at, *index).map(|_| ()),
            ReadBox { cell, .. } | WriteBox { cell, .. } => {
                if let CellLocation::Capture(index) = *cell {
                    let kind = self.capture(at, index)?;
                    if matches!(inst, WriteBox { .. }) && kind != CaptureKind::MutableCell {
                        return Err(BytecodeLiftError::ImmutableCapture { pc, index });
                    }
                }
                Ok(())
            }
            Constant(_, index)
            | ReadCell { symbol: index, .. }
            | WriteCell { symbol: index, .. }
            | DefineFunction { symbol: index, .. }
            | BindSpecial { symbol: index, .. }
            | ProclaimSpecial { symbol: index, .. }
            | ProclaimInline { symbol: index, .. }
            | Closure { code: index, .. }
            | Call {
                callee: Callee::Named(index),
                ..
            } => self.check_constant(at, *index),
            SetVariableDocumentation { symbol, doc } => {
                self.check_constant(at, *symbol)?;
                self.check_constant(at, *doc)
            }
            WhenBound { symbol, jump } => {
                self.check_constant(at, *symbol)?;
                self.check_jump(at, *jump)
            }
            Jump(target) | Unless(_, target) | IfPresent(_, target) | UnlessFixnum(_, target) => {
                self.check_jump(at, *target)
            }
            UnlessFixnumInRange(_, low, high, target) => {
                if low > high
                    || !self.fixnums.contains(&i64::from(*low))
                    || !self.fixnums.contains(&i64::from(*high))
                {
                    return Err(BytecodeLiftError::InvalidFixnumRange {
                        pc,
                        low: *low,
                        high: *high,
                    });
                }
                self.check_jump(at, *target)
            }
            EnterBlock { resume, clear, .. }
            | EnterCatch { resume, clear, .. }
            | EnterProtect {
                cleanup: resume,
                clear,
            } => {
                self.check_jump(at, *resume)?;
                self.check_clear_boundary(at, *clear)
            }
            EnterTagbody { jumps, clear, .. } => {
                self.check_clear_boundary(at, *clear)?;
                check_table("jump_tables", *jumps, self.code.jump_tables.len())
            }
            EnterHandlers { table } => check_table("handlers", *table, self.code.handlers.len()),
            EnterRestarts { table, .. } => {
                check_table("restarts", *table, self.code.restarts.len())
            }
            CheckKeywords { start, table } => {
                self.check_clear_boundary(at, *start)?;
                check_table("keywords", *table, self.code.keywords.len())
            }
            DestructureKeywords { table, .. } => {
                check_table("keywords", *table, self.code.keywords.len())
            }
            Load(..)
            | Copy(..)
            | Clear(..)
            | SetValues(..)
            | BindValues(..)
            | ReadValue(..)
            | SaveValues
            | AppendValues
            | RestoreValues
            | Unbind(..)
            | BindProgv { .. }
            | BoxLocal { .. }
            | ExitToken(..)
            | LeaveTarget
            | BeginCleanup
            | EndCleanup
            | ReturnFrom { .. }
            | Throw { .. }
            | Go { .. }
            | LeaveHandlers
            | LeaveRestarts
            | AssociateRestarts { .. }
            | LeaveRestartAssociation
            | RestArguments { .. }
            | KeywordArgument(..)
            | DestructureNext { .. }
            | DestructureEnd { .. }
            | ReleaseArguments
            | CallValues { .. }
            | Call {
                callee: Callee::Value(_),
                ..
            } => Ok(()),
        }
    }
}
