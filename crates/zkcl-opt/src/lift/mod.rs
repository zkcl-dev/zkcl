mod cfg;
mod fasl;
mod liveness;
mod validate;

use std::fmt;

pub use fasl::{FaslSpecializationInputs, VopTarget};

use cfg::BytecodeCfg;
use liveness::register_liveness;
use validate::validate_code_bounds;

use rustc_hash::{FxHashMap, FxHashSet};
use thiserror::Error;
use zkcl_bytecode::{
    BytecodeBody, BytecodeFunction, Callee, CaptureKind, CellLocation, HandlerTarget, Immediate,
    Instruction, LiveIns, ParamFact, ParamTypeFact, Register,
};

use crate::ir::{
    BasicBlock, BlockId, BlockParam, BranchTarget, CellTarget, DeoptRegisterValue, DeoptState,
    InstBuilder, ScopeId, ScopeTree, SpecializationInputs, SsaFunction, SsaOp, Terminator, TypeTag,
    UnwindOp, ValueId, ValueRepr,
};
use crate::passes::eliminate_trivial_block_params;

/// Errors produced when lifting bytecode to SSA IR.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BytecodeLiftError {
    #[error("register {reg} out of bounds for a {registers}-register frame{}", bytecode_location(*.pc))]
    RegisterOutOfBounds {
        pc: Option<usize>,
        reg: Register,
        registers: usize,
    },
    #[error("invalid register range {start}..{end} for a {registers}-register frame{}", bytecode_location(*.pc))]
    InvalidRegisterRange {
        pc: Option<usize>,
        start: usize,
        end: usize,
        registers: usize,
    },
    #[error("jump target {target} exceeds the bytecode length {len}{}", bytecode_location(*.pc))]
    JumpOutOfBounds {
        pc: Option<usize>,
        target: usize,
        len: usize,
    },
    #[error("constant index {index} out of bounds for a pool of {len} constants{}", bytecode_location(*.pc))]
    ConstantOutOfBounds {
        pc: Option<usize>,
        index: usize,
        len: usize,
    },
    #[error("capture index {index} out of bounds for {len} closure captures{}", bytecode_location(*.pc))]
    CaptureOutOfBounds {
        pc: Option<usize>,
        index: usize,
        len: usize,
    },
    #[error("cannot write to immutable closure capture {index} at bytecode PC {pc}")]
    ImmutableCapture { pc: usize, index: usize },
    #[error("index {index} out of bounds for {table} ({len} entries){}", bytecode_location(*.pc))]
    TableOutOfBounds {
        pc: Option<usize>,
        table: &'static str,
        index: usize,
        len: usize,
    },
    #[error("fixnum literal {value} out of tagged range at bytecode PC {pc}")]
    InvalidFixnum { pc: usize, value: i64 },
    #[error("invalid fixnum range {low}..={high} at bytecode PC {pc}")]
    InvalidFixnumRange { pc: usize, low: i32, high: i32 },
    #[error("wrong number of operands ({count}) for {op:?} fixnum arithmetic at bytecode PC {pc}")]
    InvalidFixnumArity {
        pc: usize,
        op: zkcl_bytecode::FixnumArithmetic,
        count: usize,
    },
    #[error("invalid arity for a {registers}-register frame ({required} required, {})", maximum_arity(*.maximum))]
    InvalidArity {
        required: usize,
        maximum: Option<usize>,
        registers: usize,
    },
    #[error(
        "optional argument index {index} is out of bounds for the function arity at bytecode PC {pc}"
    )]
    InvalidOptionalArgument { pc: usize, index: usize },
    #[error("invalid control flow at bytecode PC {pc}: {reason}")]
    InvalidControlFlow { pc: usize, reason: &'static str },
}

fn bytecode_location(pc: Option<usize>) -> impl fmt::Display {
    fmt::from_fn(move |f| match pc {
        Some(pc) => write!(f, " at bytecode PC {pc}"),
        None => f.write_str(" in bytecode metadata"),
    })
}

fn maximum_arity(maximum: Option<usize>) -> impl fmt::Display {
    fmt::from_fn(move |f| match maximum {
        Some(maximum) => write!(f, "maximum {maximum}"),
        None => f.write_str("no maximum"),
    })
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct UnwindEntryRegisters {
    clear_from: Register,
    condition_register: Option<Register>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RecoveryEntry {
    target: BlockId,
    registers: UnwindEntryRegisters,
}

#[derive(Clone, Debug)]
struct BytecodeBlock {
    id: BlockId,
    start_pc: usize,
    end_pc: usize,
    entry_scope: ScopeId,
    succs: Vec<BlockId>,
    unwind_succs: Vec<RecoveryEntry>,
}

/// Lift a compiled-file function into SSA form.
pub fn lift_code(
    code: &BytecodeFunction,
    specialization: &dyn SpecializationInputs,
) -> Result<SsaFunction, BytecodeLiftError> {
    lift_bytecode(code.body(), specialization)
}

/// Lift a function body from either a compiled file or a JIT snapshot.
/// Performs the structural checks needed to build the SSA graph.
pub fn lift_bytecode(
    body: BytecodeBody<'_>,
    specialization: &dyn SpecializationInputs,
) -> Result<SsaFunction, BytecodeLiftError> {
    let code = &body;
    let fixnum_bounds = specialization.fixnum_bounds();
    validate_code_bounds(code, fixnum_bounds)?;

    let cfg = BytecodeCfg::analyze(code)?;
    let (live_in, deopt_registers) = register_liveness(code, &cfg, specialization.native_debug());
    // A reentry at PC 0 receives the current registers. Only the initial ABI
    // entry initializes nonargument registers to NIL.
    let separate_entry = cfg
        .blocks
        .iter()
        .any(|block| block.succs.contains(&BlockId(0)))
        || cfg
            .recovery_entries
            .iter()
            .any(|entry| entry.target == BlockId(0));
    let value_counts = static_value_counts(code, &cfg.blocks);
    let mut func = SsaFunction {
        fixnum_bounds,
        arity: code.arity,
        constant_count: code.constant_count,
        entry: if separate_entry {
            BlockId((cfg.blocks.len() + cfg.recovery_entries.len()) as u32)
        } else {
            BlockId(0)
        },
        entry_params: Vec::new(),
        blocks: Vec::with_capacity(cfg.blocks.len()),
        scopes: ScopeTree::new(),
        pc_to_block: Default::default(),
        next_value_id: 0,
    };
    let block_params = allocate_block_params(
        &mut func,
        &cfg,
        &live_in,
        value_counts.as_deref(),
        code.param_facts,
    );
    let unwind_registers = cfg
        .recovery_entries
        .iter()
        .flat_map(|entry| {
            block_params[entry.target.0 as usize]
                .iter()
                .filter_map(move |param| {
                    param.origin_reg.filter(|&reg| {
                        reg < entry.registers.clear_from
                            && entry.registers.condition_register != Some(reg)
                    })
                })
        })
        .collect();
    let context = BytecodeLifter {
        code,
        cfg: &cfg,
        specialization,
        block_params,
        unwind_registers,
        deopt_registers,
        uses_dynamic_values: value_counts.is_none(),
        entry: func.entry,
    };

    for block in &cfg.blocks {
        func.blocks
            .push(context.lift_block(block, &mut func.next_value_id));
    }
    for &entry in &cfg.recovery_entries {
        func.blocks
            .push(context.lift_recovery(entry, &mut func.next_value_id));
    }
    if separate_entry {
        let entry = context.lift_entry(&mut func.next_value_id);
        func.entry_params
            .extend(entry.params.iter().map(|param| param.value));
        func.blocks.push(entry);
    }
    for (block, params) in func.blocks.iter_mut().zip(context.block_params) {
        block.params = params;
    }
    func.scopes = cfg.scope_tree;
    func.pc_to_block = cfg.pc_to_block;
    eliminate_trivial_block_params(&mut func);
    Ok(func)
}

/// Function-wide information that every block uses while translating register operands.
struct BytecodeLifter<'a> {
    code: &'a BytecodeBody<'a>,
    cfg: &'a BytecodeCfg,
    specialization: &'a dyn SpecializationInputs,
    block_params: Vec<Vec<BlockParam>>,
    unwind_registers: FxHashSet<Register>,
    deopt_registers: FxHashMap<usize, Vec<Register>>,
    uses_dynamic_values: bool,
    entry: BlockId,
}

impl BytecodeLifter<'_> {
    fn lift_block(&self, block: &BytecodeBlock, next_value_id: &mut u32) -> BasicBlock {
        let mut insts = Vec::new();
        let mut lift = BlockLifter {
            context: self,
            emit: InstBuilder {
                next_value_id,
                insts: &mut insts,
                pc: None,
            },
            registers: vec![UNINITIALIZED_REGISTER; self.code.registers],
            values: (!self.uses_dynamic_values || block.id == self.entry).then(Vec::new),
            nil: None,
        };
        lift.initialize(block);

        let mut terminator = None;
        for (pc, instruction) in
            (block.start_pc..).zip(&self.code.instructions[block.start_pc..block.end_pc])
        {
            terminator = lift.lower(pc, instruction);
            lift.sync_register_writes(instruction);
            if terminator.is_some() {
                debug_assert_eq!(pc + 1, block.end_pc);
                break;
            }
        }
        let terminator = terminator.unwrap_or_else(|| {
            if block.start_pc == self.code.instructions.len() {
                match lift.values.take() {
                    Some(values) if values.len() == 1 => Terminator::ReturnOne(values[0]),
                    Some(values) => Terminator::ReturnValues(values),
                    None => Terminator::ReturnDynamicValues,
                }
            } else {
                Terminator::Jump(lift.target(block.end_pc))
            }
        });
        BasicBlock {
            id: block.id,
            params: Vec::new(),
            insts,
            terminator,
            terminator_pc: if block.start_pc == block.end_pc {
                block.end_pc
            } else {
                block.end_pc - 1
            },
            entry_scope: block.entry_scope,
            unwind_succs: block
                .unwind_succs
                .iter()
                .map(|entry| self.cfg.recovery_blocks[entry])
                .collect(),
        }
    }

    fn lift_recovery(&self, entry: RecoveryEntry, next_value_id: &mut u32) -> BasicBlock {
        let mut insts = Vec::new();
        let mut lift = BlockLifter {
            context: self,
            emit: InstBuilder {
                next_value_id,
                insts: &mut insts,
                pc: None,
            },
            registers: vec![UNINITIALIZED_REGISTER; self.code.registers],
            values: None,
            nil: None,
        };
        let target = &self.cfg.blocks[entry.target.0 as usize];
        let mut params = Vec::new();
        for param in &self.block_params[entry.target.0 as usize] {
            let reg = param
                .origin_reg
                .expect("recovery target register parameter");
            if reg < entry.registers.clear_from && entry.registers.condition_register != Some(reg) {
                let mut param = param.clone();
                param.value = lift.emit.fresh_value();
                lift.registers[reg] = param.value;
                params.push(param);
            }
        }
        lift.recover_registers(entry.registers, target.start_pc);
        let terminator = Terminator::Jump(lift.target(target.start_pc));
        BasicBlock {
            id: self.cfg.recovery_blocks[&entry],
            params,
            insts,
            terminator,
            terminator_pc: target.start_pc,
            entry_scope: target.entry_scope,
            unwind_succs: Vec::new(),
        }
    }

    fn lift_entry(&self, next_value_id: &mut u32) -> BasicBlock {
        let mut insts = Vec::new();
        let mut lift = BlockLifter {
            context: self,
            emit: InstBuilder {
                next_value_id,
                insts: &mut insts,
                pc: None,
            },
            registers: vec![UNINITIALIZED_REGISTER; self.code.registers],
            values: Some(Vec::new()),
            nil: None,
        };
        let mut params = Vec::new();
        for reg in 0..self.code.arity.required {
            let value = lift.emit.fresh_value();
            params.push(BlockParam {
                value,
                ty: param_type_from_facts(self.code.param_facts, reg),
                repr: ValueRepr::TaggedWord,
                origin_reg: Some(reg),
            });
            lift.registers[reg] = value;
            lift.sync_unwind_register(reg);
        }
        lift.initialize_unset_registers();
        let terminator = Terminator::Jump(lift.target(0));
        BasicBlock {
            id: self.entry,
            params,
            insts,
            terminator,
            terminator_pc: 0,
            entry_scope: ScopeId::ROOT,
            unwind_succs: Vec::new(),
        }
    }

    fn deopt_state(&self, pc: usize, registers: &[ValueId]) -> Option<DeoptState> {
        let live = self.deopt_registers.get(&pc)?;
        let registers_to_restore = live
            .iter()
            .map(|&reg| (reg as u32, DeoptRegisterValue::TaggedWord(registers[reg])))
            .collect();
        Some(DeoptState {
            pc,
            registers_to_restore,
        })
    }
}

const UNINITIALIZED_REGISTER: ValueId = ValueId(u32::MAX);

/// Register and multiple-value state valid for one SSA block.
struct BlockLifter<'a> {
    context: &'a BytecodeLifter<'a>,
    emit: InstBuilder<'a>,
    registers: Vec<ValueId>,
    /// `None` means the current values are only available in the runtime buffer.
    values: Option<Vec<ValueId>>,
    nil: Option<ValueId>,
}

impl BlockLifter<'_> {
    fn initialize(&mut self, block: &BytecodeBlock) {
        let entry = block.id == self.context.entry;
        for param in &self.context.block_params[block.id.0 as usize] {
            match param.origin_reg {
                Some(reg) => {
                    self.registers[reg] = param.value;
                    if entry {
                        self.sync_unwind_register(reg);
                    }
                }
                None => self
                    .values
                    .as_mut()
                    .expect("static multiple-value parameter")
                    .push(param.value),
            }
        }
        self.initialize_unset_registers();
    }

    // Uninitialized bytecode registers read as NIL.
    fn initialize_unset_registers(&mut self) {
        if self.registers.contains(&UNINITIALIZED_REGISTER) {
            let nil = self.nil();
            for value in &mut self.registers {
                if *value == UNINITIALIZED_REGISTER {
                    *value = nil;
                }
            }
        }
    }

    fn recover_registers(&mut self, recovery: UnwindEntryRegisters, pc: usize) {
        for reg in recovery.clear_from..self.registers.len() {
            self.registers[reg] = self.nil();
            self.sync_unwind_register(reg);
        }
        if let Some(reg) = recovery.condition_register {
            self.emit.pc = Some(pc);
            self.registers[reg] = self.emit.value(SsaOp::HandlerCondition, TypeTag::Any);
            self.emit.pc = None;
            self.sync_unwind_register(reg);
        }
    }

    fn nil(&mut self) -> ValueId {
        if let Some(value) = self.nil {
            return value;
        }
        let pc = self.emit.pc.take();
        let value = self
            .emit
            .value(SsaOp::ConstWord(Immediate::Nil), TypeTag::Nil);
        self.emit.pc = pc;
        self.nil = Some(value);
        value
    }

    fn sync_unwind_register(&mut self, reg: Register) {
        if self.context.unwind_registers.contains(&reg) {
            self.emit.effect(SsaOp::StoreUnwindRegister {
                reg,
                value: self.registers[reg],
            });
        }
    }

    fn sync_register_writes(&mut self, instruction: &Instruction) {
        if !self.context.unwind_registers.is_empty() {
            self.emit.pc = None;
            instruction
                .register_operands()
                .writes
                .for_each(|reg| self.sync_unwind_register(reg));
        }
    }

    fn set_values(&mut self, values: Vec<ValueId>) {
        // Keep the runtime buffer current when this function has dynamic value uses.
        if self.context.uses_dynamic_values {
            self.emit.effect(SsaOp::SetValues(values.clone()));
        }
        self.values = Some(values);
    }

    fn target(&self, pc: usize) -> BranchTarget {
        let block = self.context.cfg.pc_to_block[&pc];
        let params = &self.context.block_params[block.0 as usize];
        let mut args = Vec::with_capacity(params.len());
        for param in params {
            if let Some(reg) = param.origin_reg {
                args.push(self.registers[reg]);
            } else {
                // The remaining block parameters carry the statically known values.
                args.extend_from_slice(self.values.as_deref().expect("static multiple-value edge"));
                break;
            }
        }
        BranchTarget { block, args }
    }

    fn branch(&mut self, op: SsaOp, truthy: usize, falsy: usize) -> Terminator {
        let cond = self.emit.value(op, TypeTag::Any);
        Terminator::BranchBool {
            cond,
            then_target: self.target(truthy),
            else_target: self.target(falsy),
        }
    }

    fn lower(&mut self, pc: usize, instruction: &Instruction) -> Option<Terminator> {
        self.emit.pc = Some(pc);
        let context = self.context;
        let code = context.code;
        let specialization = context.specialization;
        let regs = &mut self.registers;
        let emit = &mut self.emit;
        if specialization.native_debug() {
            // A source safepoint can materialize this activation into the VM.
            // Publish static multiple values as well as canonical registers.
            if let Some(values) = self.values.clone() {
                emit.effect(SsaOp::SetValues(values));
            }
            emit.effect(SsaOp::DebugPoint {
                pc,
                registers: regs.clone(),
            });
            for (index, value) in regs.iter_mut().enumerate() {
                *value = emit.value(SsaOp::DebugRegister(index), TypeTag::Any);
            }
        }
        if context.cfg.execution_checkpoint(pc, instruction) {
            emit.push(
                None,
                SsaOp::Checkpoint {
                    values: self.values.clone(),
                },
                TypeTag::Top,
                context.deopt_state(pc, regs),
            );
        }
        match instruction {
            Instruction::Load(out, imm) => {
                regs[*out] = emit.value(SsaOp::ConstWord(*imm), TypeTag::from_immediate(*imm));
            }
            Instruction::Constant(out, idx) => {
                regs[*out] = emit.value(SsaOp::LoadConst(*idx), specialization.constant_type(*idx));
            }
            Instruction::Copy(out, src) => {
                regs[*out] = regs[*src];
            }
            Instruction::Clear(range) if !range.is_empty() => {
                let nil = self.nil();
                self.registers[range.clone()].fill(nil);
            }
            Instruction::Clear(_) => {}
            Instruction::Jump(target) => {
                return Some(Terminator::Jump(self.target(*target)));
            }
            Instruction::Unless(cond_reg, target) => {
                return Some(Terminator::BranchTruthy {
                    cond: regs[*cond_reg],
                    truthy: self.target(pc + 1),
                    falsy: self.target(*target),
                });
            }
            Instruction::IfPresent(reg, target) => {
                let condition = SsaOp::IsPresent(regs[*reg]);
                return Some(self.branch(condition, *target, pc + 1));
            }
            Instruction::UnlessFixnum(reg, target) => {
                let condition = SsaOp::IsFixnum(regs[*reg]);
                return Some(self.branch(condition, pc + 1, *target));
            }
            Instruction::UnlessFixnumInRange(reg, low, high, target) => {
                let condition = SsaOp::IsFixnumInRange {
                    value: regs[*reg],
                    low: *low,
                    high: *high,
                };
                return Some(self.branch(condition, pc + 1, *target));
            }
            Instruction::WhenBound { symbol, jump } => {
                let condition = SsaOp::IsSymbolBound { symbol: *symbol };
                return Some(self.branch(condition, *jump, pc + 1));
            }
            Instruction::SetValues(range) => {
                let values = range.clone().map(|reg| regs[reg]).collect();
                self.set_values(values);
            }
            Instruction::ReadValue(out, index) => {
                regs[*out] = emit.value(SsaOp::ReadValue(*index), TypeTag::Any);
            }
            Instruction::BindValues(range) => {
                // The runtime returns NIL for a missing value. Read every
                // bound position before clearing the buffer.
                for (index, reg) in range.clone().enumerate() {
                    regs[reg] = emit.value(SsaOp::ReadValue(index), TypeTag::Any);
                }
                self.set_values(Vec::new());
            }
            Instruction::SaveValues => {
                emit.effect(SsaOp::SaveValues);
                self.values = Some(Vec::new());
            }
            Instruction::AppendValues => {
                emit.effect(SsaOp::AppendValues);
                self.values = Some(Vec::new());
            }
            Instruction::RestoreValues => {
                emit.effect(SsaOp::RestoreValues);
                self.values = None;
            }
            Instruction::ReadCell {
                out,
                symbol,
                function,
                at,
            } => {
                let op = SsaOp::ReadCell {
                    symbol: *symbol,
                    function: *function,
                    at: *at,
                };
                let result = emit.fresh_value();
                emit.push(
                    Some(result),
                    op,
                    if *function {
                        TypeTag::Function
                    } else {
                        TypeTag::Any
                    },
                    context.deopt_state(pc, regs),
                );
                regs[*out] = result;
            }
            Instruction::WriteCell { symbol, src } => {
                emit.effect(SsaOp::WriteCell {
                    symbol: *symbol,
                    src: regs[*src],
                });
            }
            Instruction::DefineFunction {
                symbol,
                definition,
                is_macro,
                at,
            } => {
                emit.effect(SsaOp::DefineFunction {
                    symbol: *symbol,
                    definition: regs[*definition],
                    is_macro: *is_macro,
                    at: *at,
                });
            }
            Instruction::ProclaimSpecial { symbol, at } => {
                emit.effect(SsaOp::ProclaimSpecial {
                    symbol: *symbol,
                    at: *at,
                });
            }
            Instruction::ProclaimInline {
                symbol,
                setf,
                notinline,
            } => {
                emit.effect(SsaOp::ProclaimInline {
                    symbol: *symbol,
                    setf: *setf,
                    notinline: *notinline,
                });
            }
            Instruction::SetVariableDocumentation { symbol, doc } => {
                emit.effect(SsaOp::SetVariableDocumentation {
                    symbol: *symbol,
                    doc: *doc,
                });
            }
            Instruction::BindSpecial { symbol, value } => {
                emit.effect(SsaOp::BindSpecial {
                    symbol: *symbol,
                    value: regs[*value],
                });
            }
            Instruction::BindProgv {
                symbols,
                values,
                at,
            } => {
                emit.effect(SsaOp::BindProgv {
                    symbols: regs[*symbols],
                    values: regs[*values],
                    at: *at,
                });
            }
            Instruction::Unbind(count) => {
                emit.effect(SsaOp::Unbind(*count));
            }
            Instruction::BoxLocal { reg, at } => {
                let op = SsaOp::BoxLocal {
                    value: regs[*reg],
                    at: *at,
                };
                regs[*reg] = emit.value(op, TypeTag::Any);
            }
            Instruction::ReadBox { out, cell } => {
                let op = match cell {
                    CellLocation::Capture(idx)
                        if code.captures[*idx] != CaptureKind::MutableCell =>
                    {
                        SsaOp::ReadCapture(*idx)
                    }
                    CellLocation::Local(r) => SsaOp::ReadBox(CellTarget::Local(regs[*r])),
                    CellLocation::Capture(idx) => SsaOp::ReadBox(CellTarget::Capture(*idx)),
                };
                regs[*out] = emit.value(op, TypeTag::Any);
            }
            Instruction::WriteBox { cell, src } => {
                let target = match cell {
                    CellLocation::Local(r) => CellTarget::Local(regs[*r]),
                    CellLocation::Capture(idx) => CellTarget::Capture(*idx),
                };
                emit.effect(SsaOp::WriteBox {
                    cell: target,
                    src: regs[*src],
                });
            }
            Instruction::Capture(out, idx) => {
                regs[*out] = emit.value(SsaOp::ReadCapture(*idx), TypeTag::Any);
            }
            Instruction::Closure {
                out,
                code: code_idx,
                cells,
                at,
            } => {
                let cell_vals = cells.clone().map(|r| regs[r]).collect();
                let op = SsaOp::Closure {
                    code: *code_idx,
                    cells: cell_vals,
                    at: *at,
                };
                regs[*out] = emit.value(op, TypeTag::Function);
            }
            Instruction::ExitToken(out, idx) => {
                let op = SsaOp::ExitToken(*idx);
                regs[*out] = emit.value(op, TypeTag::Any);
            }
            Instruction::EnterBlock {
                token,
                resume,
                clear,
            } => {
                emit.effect(SsaOp::EnterBlock {
                    token: regs[*token],
                    resume: context.cfg.recovery_target(*resume, *clear, None),
                    clear: *clear,
                });
            }
            Instruction::EnterCatch { tag, resume, clear } => {
                emit.effect(SsaOp::EnterCatch {
                    tag: regs[*tag],
                    resume: context.cfg.recovery_target(*resume, *clear, None),
                    clear: *clear,
                });
            }
            Instruction::EnterTagbody {
                token,
                jumps,
                clear,
            } => {
                let targets = code.jump_tables[*jumps]
                    .targets
                    .iter()
                    .map(|&pc| context.cfg.recovery_target(pc, *clear, None))
                    .collect();
                emit.effect(SsaOp::EnterTagbody {
                    token: regs[*token],
                    jumps: *jumps,
                    targets,
                    clear: *clear,
                });
            }
            Instruction::LeaveTarget => {
                emit.effect(SsaOp::LeaveTarget);
            }
            Instruction::EnterProtect { cleanup, clear } => {
                emit.effect(SsaOp::EnterProtect {
                    cleanup: context.cfg.recovery_target(*cleanup, *clear, None),
                    clear: *clear,
                });
            }
            Instruction::BeginCleanup => {
                emit.effect(SsaOp::BeginCleanup);
                self.values = Some(Vec::new());
                let cleanup = context.cfg.begin_cleanup_target[&pc];
                self.recover_registers(context.cfg.normal_recoveries[&pc], pc);
                return Some(Terminator::Jump(self.target(cleanup)));
            }
            Instruction::EndCleanup => {
                emit.effect(SsaOp::EndCleanup);
                self.values = None;
            }
            Instruction::ReturnFrom { token, at } => {
                return Some(Terminator::Unwind(UnwindOp::ReturnFrom {
                    token: regs[*token],
                    at: *at,
                }));
            }
            Instruction::Throw { tag, at } => {
                return Some(Terminator::Unwind(UnwindOp::Throw {
                    tag: regs[*tag],
                    at: *at,
                }));
            }
            Instruction::Go { token, label, at } => {
                if let Some(&target_pc) = context.cfg.local_go_target.get(&pc) {
                    self.set_values(Vec::new());
                    self.recover_registers(context.cfg.normal_recoveries[&pc], pc);
                    return Some(Terminator::Jump(self.target(target_pc)));
                }
                return Some(Terminator::Unwind(UnwindOp::Go {
                    token: regs[*token],
                    label: *label,
                    at: *at,
                }));
            }
            Instruction::EnterHandlers { table } => {
                let entries = &code.handlers[*table];
                let mut targets = Vec::new();
                let mut function_targets = false;
                for handler in &entries.handlers {
                    match handler.target {
                        HandlerTarget::Case {
                            pc,
                            clear,
                            condition,
                        } => {
                            targets.push(context.cfg.recovery_target(pc, clear, Some(condition)));
                        }
                        HandlerTarget::Function(_) => function_targets = true,
                    }
                }
                emit.effect(SsaOp::EnterHandlers {
                    table: *table,
                    targets,
                    function_targets,
                });
            }
            Instruction::LeaveHandlers => {
                emit.effect(SsaOp::LeaveHandlers);
            }
            Instruction::EnterRestarts { table, at } => {
                emit.effect(SsaOp::EnterRestarts {
                    table: *table,
                    at: *at,
                });
            }
            Instruction::LeaveRestarts => {
                emit.effect(SsaOp::LeaveRestarts);
            }
            Instruction::AssociateRestarts {
                condition,
                list,
                at,
            } => {
                emit.effect(SsaOp::AssociateRestarts {
                    condition: regs[*condition],
                    list: regs[*list],
                    at: *at,
                });
            }
            Instruction::LeaveRestartAssociation => {
                emit.effect(SsaOp::LeaveRestartAssociation);
            }
            Instruction::OptionalArgument(index, out) => {
                regs[*out] = emit.value(SsaOp::OptionalArgument(*index), TypeTag::Any);
            }
            Instruction::KeywordArgument(index, out) => {
                regs[*out] = emit.value(SsaOp::KeywordArgument(*index), TypeTag::Any);
            }
            Instruction::RestArguments { start, out, at } => {
                let op = SsaOp::RestArguments {
                    start: *start,
                    at: *at,
                };
                regs[*out] = emit.value(op, TypeTag::Any);
            }
            Instruction::CheckKeywords { start, table } => {
                emit.effect(SsaOp::CheckKeywords {
                    start: *start,
                    table: *table,
                });
            }
            Instruction::DestructureNext {
                cursor,
                out,
                required,
                macro_call,
                at,
            } => {
                let op = SsaOp::DestructureNext {
                    cursor: regs[*cursor],
                    required: *required,
                    macro_call: *macro_call,
                    at: *at,
                };
                let tuple = emit.value(op, TypeTag::Any);
                unpack_tuple(emit, tuple, regs, [*out, *cursor]);
            }
            Instruction::DestructureEnd {
                cursor,
                macro_call,
                at,
            } => {
                emit.effect(SsaOp::DestructureEnd {
                    cursor: regs[*cursor],
                    macro_call: *macro_call,
                    at: *at,
                });
            }
            Instruction::DestructureKeywords {
                cursor,
                table,
                out,
                macro_call,
                at,
            } => {
                let width = out.len();
                let op = SsaOp::DestructureKeywords {
                    cursor: regs[*cursor],
                    table: *table,
                    width,
                    macro_call: *macro_call,
                    at: *at,
                };
                let tuple = emit.value(op, TypeTag::Any);
                unpack_tuple(emit, tuple, regs, out.clone());
            }
            Instruction::ReleaseArguments => {
                emit.effect(SsaOp::ReleaseArguments);
            }
            Instruction::TryFixnumArithmetic {
                op,
                symbol,
                out,
                args,
            } => {
                let lhs = regs[args.start];
                let rhs = if args.len() == 1 {
                    emit.value(SsaOp::ConstWord(Immediate::Fixnum(1)), TypeTag::Fixnum)
                } else {
                    regs[args.start + 1]
                };
                regs[*out] = emit.value(
                    SsaOp::TryFixnumArithmetic {
                        op: *op,
                        symbol: *symbol,
                        lhs,
                        rhs,
                    },
                    TypeTag::Any,
                );
            }
            Instruction::CallValues { callee, at } => {
                let deopt = context.deopt_state(pc, regs);
                let op = SsaOp::CallValues {
                    callee: regs[*callee],
                    at: *at,
                };
                emit.push(None, op, TypeTag::Top, deopt);
                self.values = None;
            }
            Instruction::Call {
                callee,
                out,
                args,
                at,
                associate,
                notinline,
            } => {
                let deopt = context.deopt_state(pc, regs);
                if let Callee::Named(symbol) = *callee
                    && args.len() == 1
                    && !associate
                    && !notinline
                    && !code.notinline.contains(&(symbol as u32))
                    && !specialization.is_notinline(symbol)
                    && !specialization.native_debug()
                    && let Some(state) = deopt.as_ref()
                    && let Some((binding, lowering)) = specialization.popcount_vop(pc)
                {
                    let op = SsaOp::PopcountU64 {
                        input: regs[args.start],
                        symbol,
                        binding,
                        lowering,
                    };
                    let result = emit.guarded_value(op, TypeTag::Fixnum, state.clone());
                    if let Some(out) = *out {
                        regs[out] = result;
                        self.set_values(Vec::new());
                    } else {
                        self.set_values(vec![result]);
                    }
                    return None;
                }
                let arg_vals: Vec<ValueId> = args.clone().map(|r| regs[r]).collect();
                let profile = specialization.call_site_profile(pc);
                let op = match callee {
                    Callee::Named(symbol) => {
                        let effective_notinline = *notinline
                            || code.notinline.contains(&(*symbol as u32))
                            || specialization.is_notinline(*symbol);
                        SsaOp::CallNamed {
                            symbol: *symbol,
                            args: arg_vals,
                            has_result: out.is_some(),
                            at: *at,
                            associate: *associate,
                            notinline: effective_notinline,
                            profile,
                        }
                    }
                    Callee::Value(reg) => SsaOp::CallValue {
                        callee: regs[*reg],
                        args: arg_vals,
                        has_result: out.is_some(),
                        at: *at,
                        associate: *associate,
                        notinline: *notinline,
                        profile,
                    },
                };
                if let Some(out_reg) = out {
                    let result = emit.fresh_value();
                    emit.push(Some(result), op, TypeTag::Any, deopt);
                    regs[*out_reg] = result;
                    self.set_values(Vec::new());
                } else {
                    emit.push(None, op, TypeTag::Top, deopt);
                    self.values = None;
                }
            }
        }
        None
    }
}

/// Extract tuple fields in order, including when output registers overlap.
fn unpack_tuple(
    emit: &mut InstBuilder<'_>,
    source: ValueId,
    regs: &mut [ValueId],
    outputs: impl IntoIterator<Item = Register>,
) {
    for (index, reg) in outputs.into_iter().enumerate() {
        regs[reg] = emit.value(SsaOp::ExtractValue { source, index }, TypeTag::Any);
    }
}

fn param_type_from_facts(param_facts: &[ParamFact], index: usize) -> TypeTag {
    match param_facts.get(index).map(|fact| fact.ty) {
        Some(ParamTypeFact::Fixnum) => TypeTag::Fixnum,
        Some(ParamTypeFact::Float) => TypeTag::Float,
        Some(ParamTypeFact::Char) => TypeTag::Char,
        Some(ParamTypeFact::Cons) => TypeTag::Cons,
        Some(ParamTypeFact::Symbol) => TypeTag::Symbol,
        Some(ParamTypeFact::Function) => TypeTag::Function,
        Some(ParamTypeFact::Nil) => TypeTag::Nil,
        Some(ParamTypeFact::Any) | None => TypeTag::Any,
    }
}

/// Allocate register parameters first and multiple-value parameters last, in the
/// order expected by native entries and by ordinary SSA branches.
fn allocate_block_params(
    func: &mut SsaFunction,
    cfg: &BytecodeCfg,
    live_in: &LiveIns,
    incoming_value_counts: Option<&[usize]>,
    param_facts: &[ParamFact],
) -> Vec<Vec<BlockParam>> {
    let entry_arity = func.arity.required;
    let fixed_entry = func.entry == BlockId(0);
    let mut blocks = Vec::with_capacity(cfg.blocks.len());
    for index in 0..cfg.blocks.len() {
        let mut params = Vec::new();
        let mut add = |origin_reg, ty| {
            let value = func.fresh_value();
            params.push(BlockParam {
                value,
                ty,
                repr: ValueRepr::TaggedWord,
                origin_reg,
            });
        };
        if index == 0 && fixed_entry {
            for reg in 0..entry_arity {
                add(Some(reg), param_type_from_facts(param_facts, reg));
            }
            func.entry_params
                .extend(params.iter().map(|param| param.value));
        } else {
            for reg in live_in.registers(index) {
                add(Some(reg), TypeTag::Any);
            }
            if let Some(counts) = incoming_value_counts {
                for _ in 0..counts[index] {
                    add(None, TypeTag::Any);
                }
            }
        }
        blocks.push(params);
    }
    blocks
}

/// Count the values to pass as block parameters when ordinary SSA edges can carry them.
/// Runtime buffer operations, unwind edges, and joins with incompatible counts need the buffer.
fn static_value_counts(code: &BytecodeBody<'_>, blocks: &[BytecodeBlock]) -> Option<Vec<usize>> {
    let mut overwrites = Vec::with_capacity(blocks.len());
    for block in blocks {
        if !block.unwind_succs.is_empty() {
            return None;
        }
        let mut last_write = None;
        for inst in &code.instructions[block.start_pc..block.end_pc] {
            match inst {
                Instruction::SetValues(range) => last_write = Some(range.len()),
                Instruction::Call { out: Some(_), .. } | Instruction::Go { .. } => {
                    last_write = Some(0);
                }
                Instruction::SaveValues
                | Instruction::AppendValues
                | Instruction::RestoreValues
                | Instruction::BindValues(_)
                | Instruction::ReadValue(..)
                | Instruction::CallValues { .. }
                | Instruction::Call { out: None, .. }
                | Instruction::BeginCleanup
                | Instruction::EndCleanup
                | Instruction::ReturnFrom { .. }
                | Instruction::Throw { .. } => return None,
                _ => {}
            }
        }
        overwrites.push(last_write);
    }

    let mut incoming = vec![None; blocks.len()];
    incoming[0] = Some(0);
    let mut pending = vec![0];
    while let Some(index) = pending.pop() {
        let outgoing = overwrites[index].unwrap_or_else(|| incoming[index].unwrap());
        for successor in &blocks[index].succs {
            let target = successor.0 as usize;
            // This target overwrites the values before reading or forwarding them.
            let needed = if overwrites[target].is_some() {
                0
            } else {
                outgoing
            };
            match incoming[target] {
                Some(previous) if previous != needed => return None,
                Some(_) => {}
                None => {
                    incoming[target] = Some(needed);
                    pending.push(target);
                }
            }
        }
    }
    incoming.into_iter().collect()
}
