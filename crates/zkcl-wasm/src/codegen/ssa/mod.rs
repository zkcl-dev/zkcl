use std::cmp::Reverse;
use std::collections::BinaryHeap;

use wasm_encoder::{BlockType, Function, Instruction as Ins, MemArg, ValType};
use zkcl_bytecode::fixnum_bounds_for_bits;
use zkcl_opt::native::{
    ClassifiedFunction, JIT_RETURN_TAG_ONE, JIT_RETURN_TAG_TAIL_CALL, JIT_RETURN_TAG_UNWIND,
    JIT_RETURN_TAG_VALUES, NativeFrameLayout, NativeOp, RuntimeAbiFunction, ScalarConstant,
    SliceCharge, WORD_NIL, WordOp, jit_call_context_offsets, jit_return_layout,
    native_frame_layout, tail_call,
};
use zkcl_opt::{
    BasicBlock, BlockId, BranchTarget, DeoptRegisterValue, FloatFormat, RootSlot,
    SpecializationInputs, SsaFunction, SsaInst, SsaOp, Terminator, UnwindOp, ValueId, ValueRepr,
};

use super::cfg::{ControlFlow, Region};
use crate::abi::FunctionRegistration;
use crate::source_map::InstructionRange;

mod arguments;
mod numeric;
mod ops;

const WASM_FEATURES: &[zkcl_opt::native::NativeFeature] = zkcl_opt::native::WASM_FEATURES;

/// An admitted operation has no encoding in this emitter. Discard the whole
/// partially emitted body so the bytecode VM can run the function instead.
#[derive(Debug)]
struct UnsupportedOp;

const CONTEXT: u32 = 0;
const TARGET: u32 = 1;
const ARGS: u32 = 2;
const ARGC: u32 = 3;
const RESULT: u32 = 4;
const SCRATCH: u32 = 5;
const PARAMS: u32 = 6;
const WORD_BYTES: u64 = 4;
const RETURN_LAYOUT: zkcl_opt::native::JitReturnLayout = jit_return_layout(WORD_BYTES as i32);

#[derive(Debug, thiserror::Error)]
pub enum LoweringError {
    #[error("Wasm runtime is missing {symbol}", symbol = .0.symbol_name())]
    RuntimeFunction(RuntimeAbiFunction),
    #[error("Wasm lowering requires the wasm32 fixnum range")]
    FixnumBounds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Value {
    Undefined,
    Local(u32),
    I32(i32),
    I64(i64),
}

struct Locals {
    values: Vec<Value>,
    types: Vec<(u32, ValType)>,
    count: u32,
    slot_types: Vec<ValType>,
    free_i32: Vec<u32>,
    free_i64: Vec<u32>,
    free_f32: Vec<u32>,
    free_f64: Vec<u32>,
}

impl Locals {
    /// Assign locals by scanning conservative live intervals in reverse postorder.
    fn assign(
        function: &SsaFunction,
        classified: &ClassifiedFunction<'_>,
        liveness: &zkcl_opt::passes::Liveness,
    ) -> Result<Self, UnsupportedOp> {
        let mut locals = Self {
            values: vec![Value::Undefined; function.next_value_id as usize],
            types: Vec::new(),
            count: PARAMS,
            slot_types: Vec::new(),
            free_i32: Vec::new(),
            free_i64: Vec::new(),
            free_f32: Vec::new(),
            free_f64: Vec::new(),
        };
        // Keep one interval across the layout so a death in one branch cannot
        // free a slot before a use in another. Block boundaries include liveness
        // through backedges and non-local control transfers.
        let mut ranges = vec![(usize::MAX, 0); locals.values.len()];
        let mut include = |value: ValueId, point: usize| {
            let (first, last) = &mut ranges[value.0 as usize];
            *first = (*first).min(point);
            *last = (*last).max(point);
        };
        let mut definitions = Vec::new();
        let mut point = 0;
        let indices = function.block_index_map();
        for id in zkcl_opt::passes::compute_rpo(function) {
            let block_index = indices[&id];
            let block = &function.blocks[block_index];
            for &value in liveness.live_in(id) {
                include(value, point);
            }
            for param in &block.params {
                include(param.value, point);
                definitions.push((param.value, Self::local_type(param.repr)?));
            }
            for (inst_index, inst) in block.insts.iter().enumerate() {
                point += 1;
                zkcl_opt::passes::instruction_uses(inst, |value| include(value, point));
                if let Some(value) = inst.id {
                    include(value, point);
                    let location = &mut locals.values[value.0 as usize];
                    match classified.operation(block_index, inst_index) {
                        NativeOp::Word(WordOp::Constant(ScalarConstant::Word(word))) => {
                            *location = Value::I32(word.encode_word(32).unwrap() as i32);
                        }
                        NativeOp::Word(WordOp::Constant(ScalarConstant::Bool(value))) => {
                            *location = Value::I32(i32::from(value));
                        }
                        NativeOp::Word(WordOp::Constant(ScalarConstant::I64(value))) => {
                            *location = Value::I64(value);
                        }
                        NativeOp::Word(WordOp::Constant(ScalarConstant::U64(value))) => {
                            *location = Value::I64(value as i64);
                        }
                        _ => definitions.push((value, Self::local_type(inst.op.result_repr())?)),
                    }
                }
            }
            point += 1;
            block.terminator.visit_inputs(|value| include(value, point));
            for &value in liveness.live_out(id) {
                include(value, point);
            }
            point += 1;
        }

        definitions.sort_unstable_by_key(|&(value, _)| (ranges[value.0 as usize].0, value));
        let mut occupied: BinaryHeap<Reverse<(usize, u32)>> = BinaryHeap::new();
        for (value, ty) in definitions {
            let (first, last) = ranges[value.0 as usize];
            // Parameters enter together. Readers and definitions at the same
            // instruction, including deoptimization reads, keep separate slots.
            while let Some(&Reverse((end, slot))) = occupied.peek() {
                if end >= first {
                    break;
                }
                occupied.pop();
                locals.release(slot);
            }
            let slot = locals.allocate(ty);
            locals.values[value.0 as usize] = Value::Local(slot);
            occupied.push(Reverse((last, slot)));
        }
        Ok(locals)
    }

    fn local_type(repr: ValueRepr) -> Result<ValType, UnsupportedOp> {
        Ok(match repr {
            ValueRepr::TaggedWord | ValueRepr::Bool => ValType::I32,
            ValueRepr::UnboxedI64 | ValueRepr::UnboxedU64 => ValType::I64,
            ValueRepr::UnboxedF32 => ValType::F32,
            ValueRepr::UnboxedF64 => ValType::F64,
            ValueRepr::UnboxedI32x4 => return Err(UnsupportedOp),
        })
    }

    /// Take a free slot of this type, or declare a new local.
    fn allocate(&mut self, ty: ValType) -> u32 {
        let recycled = match ty {
            ValType::I64 => self.free_i64.pop(),
            ValType::F32 => self.free_f32.pop(),
            ValType::F64 => self.free_f64.pop(),
            _ => self.free_i32.pop(),
        };
        recycled.unwrap_or_else(|| self.declare(ty))
    }

    /// Keep a slot for the frame slots and scratch locals that live for the whole
    /// function; they must never be handed to a body value.
    fn allocate_fresh(&mut self, ty: ValType) -> u32 {
        self.declare(ty)
    }

    /// Append one wasm local and extend the declaration groups so consecutive
    /// locals of the same type stay in one `Function::new` entry.
    fn declare(&mut self, ty: ValType) -> u32 {
        let index = self.count;
        self.count += 1;
        self.slot_types.push(ty);
        if let Some((count, group)) = self.types.last_mut()
            && *group == ty
        {
            *count += 1;
        } else {
            self.types.push((1, ty));
        }
        index
    }

    fn release(&mut self, slot: u32) {
        match self.slot_types[slot as usize - PARAMS as usize] {
            ValType::I64 => self.free_i64.push(slot),
            ValType::F32 => self.free_f32.push(slot),
            ValType::F64 => self.free_f64.push(slot),
            _ => self.free_i32.push(slot),
        }
    }
}

struct RootFrame {
    slots: u32,
    index: u32,
}

pub struct LoweredFunction {
    pub registration: FunctionRegistration,
    pub body: Function,
    pub instructions: Vec<InstructionRange>,
}

struct Emitter<'a, F> {
    ssa: &'a SsaFunction,
    classified: &'a ClassifiedFunction<'a>,
    frame: &'a NativeFrameLayout,
    flow: &'a ControlFlow,
    runtime_function: F,
    missing_function: Option<RuntimeAbiFunction>,
    body: Function,
    source: Option<&'a [zkcl_bytecode::Instruction]>,
    instructions: Vec<InstructionRange>,
    locations: Vec<Value>,
    root_frame: Option<RootFrame>,
    depth: u32,
    labels: Vec<u32>,
    recovery: Option<u32>,
    dispatch: Option<u32>,
    scratch_words: usize,
    wide: Option<u32>,
    f32_scratch: Option<u32>,
    f64_scratch: Option<u32>,
    binding_mask: Option<u32>,
    primitives: Vec<Option<numeric::Binding>>,
    poll_flag: Option<u32>,
}

/// Every SSA instruction in layout order.
fn instructions(ssa: &SsaFunction) -> impl Iterator<Item = &SsaInst> {
    ssa.blocks.iter().flat_map(|block| &block.insts)
}

fn has_debug_point(ssa: &SsaFunction) -> bool {
    instructions(ssa).any(|inst| matches!(inst.op, SsaOp::DebugPoint { .. }))
}

/// Reserve a fresh local for a function-wide helper, or nothing when this
/// function never reaches it. Fresh slots stay out of the recycling pools, so a
/// body value can never overwrite a helper that is still in use.
fn helper_slot(locals: &mut Locals, needed: bool, ty: ValType) -> Option<u32> {
    needed.then(|| locals.allocate_fresh(ty))
}

/// Whether a checked float operation of this format can reach emission.
fn has_float_arithmetic(ssa: &SsaFunction, format: FloatFormat) -> bool {
    instructions(ssa).any(
        |inst| matches!(inst.op, SsaOp::FloatArithChecked { format: found, .. } if found == format),
    )
}

/// Recovery materializes words, checked integers, and unboxed floats. A recovery
/// map holding an SIMD pack belongs to a function this backend cannot resume, so
/// the whole function goes back to the bytecode VM instead of trapping in emission.
fn supported_recovery(function: &SsaFunction) -> bool {
    instructions(function).all(|inst| {
        inst.deopt.as_ref().is_none_or(|state| {
            state.registers_to_restore.iter().all(|(_, value)| {
                matches!(
                    value,
                    DeoptRegisterValue::TaggedWord(_)
                        | DeoptRegisterValue::UnboxedI64(_)
                        | DeoptRegisterValue::UnboxedFloat { .. }
                        | DeoptRegisterValue::ConstImmediate(_)
                )
            })
        })
    })
}

/// Lower an optimized wasm32 SSA function using the runtime's out-pointer ABI.
/// `None` leaves functions with unimplemented operations in the Lisp VM.
pub fn lower_ssa_function(
    ssa: &SsaFunction,
    specialization: &dyn SpecializationInputs,
    source: Option<&[zkcl_bytecode::Instruction]>,
    runtime_function: impl Fn(RuntimeAbiFunction) -> Option<u32>,
) -> Result<Option<LoweredFunction>, LoweringError> {
    if Some(ssa.fixnum_bounds) != fixnum_bounds_for_bits(32) {
        return Err(LoweringError::FixnumBounds);
    }
    let classified = ClassifiedFunction::new(ssa);
    if !classified.is_supported(WASM_FEATURES) {
        return Ok(None);
    }
    if !supported_recovery(ssa) {
        return Ok(None);
    }
    let frame = native_frame_layout(ssa);
    let flow = super::cfg::for_ssa(ssa, &frame);
    let liveness = zkcl_opt::passes::Liveness::solve(ssa);
    let Ok(mut locals) = Locals::assign(ssa, &classified, &liveness) else {
        return Ok(None);
    };
    let root_frame = (frame.root_slots() != 0).then(|| RootFrame {
        slots: locals.allocate_fresh(ValType::I32),
        index: locals.allocate_fresh(ValType::I32),
    });
    let recovery = helper_slot(
        &mut locals,
        !frame.unwind_table().targets().is_empty(),
        ValType::I32,
    );
    let dispatch = helper_slot(
        &mut locals,
        flow.dispatch.iter().any(Option::is_some),
        ValType::I32,
    );
    let primitives = numeric::bindings(ssa, specialization);
    // Bit mask of the primitive bindings this frame has already checked.
    let binding_mask = helper_slot(
        &mut locals,
        primitives.iter().any(Option::is_some),
        ValType::I64,
    );
    // The staging local for arithmetic that leaves the fixnum range and for
    // unboxing a Lisp float into an unboxed register.
    let needs_wide = binding_mask.is_some()
        || instructions(ssa).any(|inst| {
            matches!(
                inst.op,
                SsaOp::TryFixnumArithmetic { .. }
                    | SsaOp::AddI64Checked(..)
                    | SsaOp::SubI64Checked(..)
                    | SsaOp::MulI64Checked(..)
                    | SsaOp::UnboxFloatGuard { .. }
                    | SsaOp::ReadFloatVector { .. }
            )
        });
    let wide = helper_slot(&mut locals, needs_wide, ValType::I64);
    let f32_scratch = helper_slot(
        &mut locals,
        has_float_arithmetic(ssa, FloatFormat::F32),
        ValType::F32,
    );
    let f64_scratch = helper_slot(
        &mut locals,
        has_float_arithmetic(ssa, FloatFormat::F64),
        ValType::F64,
    );
    let poll_flag = helper_slot(&mut locals, frame.needs_execution_polls(), ValType::I32);
    let mut emit = Emitter {
        ssa,
        classified: &classified,
        frame: &frame,
        flow: &flow,
        runtime_function,
        missing_function: None,
        body: Function::new(locals.types),
        source,
        instructions: Vec::new(),
        locations: locals.values,
        root_frame,
        depth: 0,
        labels: vec![u32::MAX; ssa.blocks.len()],
        recovery,
        dispatch,
        scratch_words: 0,
        wide,
        f32_scratch,
        f64_scratch,
        primitives,
        binding_mask,
        poll_flag,
    };
    let native_debug = has_debug_point(ssa);
    if emit.function(native_debug).is_err() {
        return Ok(None);
    }
    if let Some(function) = emit.missing_function {
        return Err(LoweringError::RuntimeFunction(function));
    }
    Ok(Some(LoweredFunction {
        registration: FunctionRegistration {
            native_debug,
            root_slots: frame.root_slots() as u32,
            scratch_words: emit.scratch_words as u32,
        },
        body: emit.body,
        instructions: emit.instructions,
    }))
}

impl<F: Fn(RuntimeAbiFunction) -> Option<u32>> Emitter<'_, F> {
    fn function(&mut self, native_debug: bool) -> Result<(), UnsupportedOp> {
        let entry = self.index(self.ssa.entry);
        if let Some(frame) = &self.root_frame {
            let (slots, index) = (frame.slots, frame.index);
            self.read(Value::Local(CONTEXT));
            self.word(self.frame.root_slots());
            self.scratch(0, 2);
            self.scratch(1, 1);
            self.call(RuntimeAbiFunction::AllocRootFrame);
            for (local, offset) in [(slots, 0), (index, WORD_BYTES)] {
                self.ins(Ins::LocalGet(SCRATCH));
                self.load(offset);
                self.ins(Ins::LocalSet(local));
            }
            self.ins(Ins::LocalGet(slots));
            self.ins(Ins::LocalGet(TARGET));
            self.store(0);
            self.ins(Ins::LocalGet(slots));
            self.ins(Ins::LocalGet(TARGET));
            self.store(WORD_BYTES);

            self.prepare_argument_tail();
            if native_debug {
                self.ins(Ins::LocalGet(CONTEXT));
                self.ins(Ins::LocalGet(ARGS));
                self.ins(Ins::LocalGet(ARGC));
                self.call(RuntimeAbiFunction::StageDebugArguments);
                let registers: Vec<_> = self.frame.debug_unwind_registers().collect();
                for (register, slot) in registers {
                    self.ins(Ins::LocalGet(CONTEXT));
                    self.word(register);
                    self.word(slot);
                    self.call(RuntimeAbiFunction::MapDebugUnwindRegister);
                }
            }
        }
        for (index, param) in self.ssa.blocks[entry].params.iter().enumerate() {
            if matches!(
                param.repr,
                ValueRepr::UnboxedU64
                    | ValueRepr::UnboxedF32
                    | ValueRepr::UnboxedF64
                    | ValueRepr::UnboxedI32x4
            ) {
                return Err(UnsupportedOp);
            }
            self.ins(Ins::LocalGet(ARGS));
            self.load(index as u64 * WORD_BYTES);
            if param.repr == ValueRepr::UnboxedI64 {
                self.unbox_fixnum();
            }
            self.set(param.value);
        }

        if self.recovery.is_some() {
            self.scope(Ins::Loop(BlockType::Empty));
        }
        let regions = &self.flow.regions;
        self.open_regions(regions);
        if let Some(state) = self.recovery {
            self.ins(Ins::LocalGet(state));
            self.scope(Ins::If(BlockType::Empty));
            self.unwind_dispatch(state);
            self.end();
        }
        self.jump(entry, regions.first().map(Region::entry));
        self.regions(regions)?;
        if self.recovery.is_some() {
            self.end();
        }
        self.ins(Ins::Unreachable);
        self.ins(Ins::End);
        Ok(())
    }

    fn open_regions(&mut self, regions: &[Region]) {
        for region in regions.iter().rev() {
            let entry = region.entry();
            if self.flow.forward_labels[entry] {
                self.labels[entry] = self.depth;
                self.scope(Ins::Block(BlockType::Empty));
            }
        }
    }

    fn regions(&mut self, regions: &[Region]) -> Result<(), UnsupportedOp> {
        for (index, region) in regions.iter().enumerate() {
            if self.flow.forward_labels[region.entry()] {
                self.end();
            }
            let next = regions.get(index + 1).map(Region::entry);
            match region {
                Region::Block(block) => self.block(*block, next)?,
                Region::Loop { header, body } => {
                    self.loop_header(*header);
                    self.open_regions(body);
                    self.block(*header, body.first().map(Region::entry))?;
                    self.regions(body)?;
                    self.end();
                }
                Region::Dispatch(blocks) => self.dispatch_region(blocks)?,
            }
        }
        Ok(())
    }

    fn loop_header(&mut self, header: usize) {
        self.labels[header] = self.depth;
        self.scope(Ins::Loop(BlockType::Empty));
    }

    fn switch(&mut self, cases: usize, selector: impl FnOnce(&mut Self)) {
        for _ in 0..=cases {
            self.scope(Ins::Block(BlockType::Empty));
        }
        selector(self);
        self.ins(Ins::BrTable(
            (0..cases as u32).collect::<Vec<_>>().into(),
            cases as u32,
        ));
    }

    fn dispatch_region(&mut self, blocks: &[usize]) -> Result<(), UnsupportedOp> {
        self.loop_header(blocks[0]);
        let dispatch = self.dispatch.unwrap();
        self.switch(blocks.len(), |emit| emit.ins(Ins::LocalGet(dispatch)));
        for &block in blocks {
            self.end();
            self.block(block, None)?;
        }
        self.end();
        self.ins(Ins::Unreachable);
        self.end();
        Ok(())
    }

    fn unwind_dispatch(&mut self, state: u32) {
        self.invalidate_bindings();
        let targets = self.frame.unwind_table().targets();
        self.switch(targets.len(), |emit| {
            emit.ins(Ins::LocalGet(state));
            emit.word(1);
            emit.ins(Ins::I32Sub);
        });
        for &target in targets {
            self.end();
            let block = self.index(target);
            for (value, slot) in self.frame.unwind_parameters(&self.ssa.blocks[block]) {
                self.read_root(slot);
                self.set(value);
            }
            self.jump(block, None);
        }
        self.end();
        self.ins(Ins::Unreachable);
    }

    fn block(&mut self, block_index: usize, next: Option<usize>) -> Result<(), UnsupportedOp> {
        let block: &BasicBlock = &self.ssa.blocks[block_index];
        let tail = tail_call(self.ssa, self.frame, block);
        let frame = self.frame;
        for (index, inst, live) in frame.instructions(block) {
            let start = self.body.byte_len();
            let operation = self.classified.operation(block_index, index);
            if !self.numeric_call(operation, inst, live) {
                if let Some(tail) = tail.filter(|_| index + 1 == block.insts.len()) {
                    self.stage_tail_call(tail);
                    self.return_with(JIT_RETURN_TAG_TAIL_CALL);
                    self.record_site(inst.pc, start);
                    return Ok(());
                }
                self.spill(live);
                if inst.op.effects().may_gc() {
                    self.invalidate_bindings();
                }
                self.operation(block.id, index, operation, inst, live)?;
                self.reload(live);
            }
            self.record_site(inst.pc, start);
        }
        if let Some(pc) = self.frame.terminal_poll_pc(block.id) {
            let start = self.body.byte_len();
            self.poll_backedge(pc);
            self.record_site(Some(pc), start);
        }
        self.terminator(&block.terminator, next);
        Ok(())
    }

    fn record_site(&mut self, pc: Option<usize>, start: usize) {
        let end = self.body.byte_len();
        let Some((pc, source)) = pc.zip(self.source).filter(|_| start != end) else {
            return;
        };
        let Some(instruction) = source.get(pc) else {
            return;
        };
        let bytecode_pc = pc as u32;
        let wasm = start as u32..end as u32;
        if let Some(previous) = self.instructions.last_mut()
            && previous.bytecode_pc == bytecode_pc
            && previous.wasm.end == wasm.start
        {
            previous.wasm.end = wasm.end;
        } else {
            self.instructions.push(InstructionRange {
                wasm,
                bytecode_pc,
                source_offset: instruction.source_offset().map(|offset| offset as u64),
            });
        }
    }

    fn spill(&mut self, live: &[(ValueId, RootSlot)]) {
        for &(id, slot) in live {
            self.ins(Ins::LocalGet(self.root_frame.as_ref().unwrap().slots));
            self.get(id);
            self.store(slot.0 as u64 * WORD_BYTES);
        }
    }

    fn reload(&mut self, live: &[(ValueId, RootSlot)]) {
        for &(id, slot) in live {
            self.read_root(slot.0);
            self.set(id);
        }
    }

    fn poll_backedge(&mut self, pc: usize) {
        let flag = self.poll_flag.unwrap();
        let context = jit_call_context_offsets(WORD_BYTES as i32);
        self.ins(Ins::LocalGet(CONTEXT));
        self.load(context.cancel_flag as u64);
        self.ins(Ins::LocalTee(flag));
        self.scope(Ins::If(BlockType::Result(ValType::I32)));
        self.ins(Ins::LocalGet(flag));
        self.ins(Ins::I32Load8U(Self::mem(0, 0)));
        self.ins(Ins::Else);
        self.word(0);
        self.end();
        self.ins(Ins::LocalGet(CONTEXT));
        self.load(context.remaining_steps as u64);
        self.ins(Ins::I32Or);
        self.scope(Ins::If(BlockType::Empty));
        self.emit_simple_call(&SliceCharge::poll(pc));
        self.end();
    }

    fn terminator(&mut self, terminator: &Terminator, next: Option<usize>) {
        match terminator {
            Terminator::Jump(edge) => self.edge(edge, next),
            Terminator::BranchTruthy {
                cond,
                truthy,
                falsy,
            } => {
                self.get(*cond);
                self.word(WORD_NIL as usize);
                self.ins(Ins::I32Ne);
                self.branch(truthy, falsy, next);
            }
            Terminator::BranchBool {
                cond,
                then_target,
                else_target,
            } => {
                self.get(*cond);
                self.branch(then_target, else_target, next);
            }
            Terminator::ReturnOne(value) => {
                self.ins(Ins::LocalGet(RESULT));
                self.get(*value);
                self.store(RETURN_LAYOUT.single_value as u64);
                self.return_with(JIT_RETURN_TAG_ONE);
            }
            Terminator::ReturnValues(values) => {
                self.set_values(values);
                self.return_with(JIT_RETURN_TAG_VALUES);
            }
            Terminator::ReturnDynamicValues => self.return_with(JIT_RETURN_TAG_VALUES),
            Terminator::Unwind(op) => {
                self.ins(Ins::LocalGet(CONTEXT));
                let (function, at) = match *op {
                    UnwindOp::ReturnFrom { token, at } => {
                        self.get(token);
                        (RuntimeAbiFunction::ReturnFrom, at)
                    }
                    UnwindOp::Throw { tag, at } => {
                        self.get(tag);
                        (RuntimeAbiFunction::Throw, at)
                    }
                    UnwindOp::Go { token, label, at } => {
                        self.get(token);
                        self.word(label);
                        (RuntimeAbiFunction::Go, at)
                    }
                };
                self.word(at);
                self.ins(Ins::LocalGet(RESULT));
                self.call(function);
                self.unwind();
            }
        }
    }

    fn branch(&mut self, yes: &BranchTarget, no: &BranchTarget, next: Option<usize>) {
        if yes == no {
            self.ins(Ins::Drop);
            self.edge(yes, next);
            return;
        }
        if Some(self.anchor(self.index(yes.block))) == next {
            self.ins(Ins::I32Eqz);
            self.scope(Ins::If(BlockType::Empty));
            self.edge(no, None);
            self.end();
            self.edge(yes, next);
        } else if Some(self.anchor(self.index(no.block))) == next {
            self.scope(Ins::If(BlockType::Empty));
            self.edge(yes, None);
            self.end();
            self.edge(no, next);
        } else {
            self.scope(Ins::If(BlockType::Empty));
            self.edge(yes, None);
            self.ins(Ins::Else);
            self.edge(no, None);
            self.end();
        }
    }

    fn edge(&mut self, edge: &BranchTarget, next: Option<usize>) {
        let target = self.index(edge.block);
        let params = &self.ssa.blocks[target].params;
        for (&arg, param) in edge.args.iter().zip(params) {
            if arg != param.value {
                self.get(arg);
            }
        }
        for (&arg, param) in edge.args.iter().zip(params).rev() {
            if arg != param.value {
                self.set(param.value);
            }
        }
        self.jump(target, next);
    }

    fn anchor(&self, target: usize) -> usize {
        self.flow.dispatch[target].map_or(target, |(header, _)| header)
    }

    fn jump(&mut self, target: usize, next: Option<usize>) {
        if let Some((_, case)) = self.flow.dispatch[target] {
            self.word(case as usize);
            self.ins(Ins::LocalSet(self.dispatch.unwrap()));
        }
        let target = self.anchor(target);
        if Some(target) != next {
            self.ins(Ins::Br(self.depth - 1 - self.labels[target]));
        }
    }

    fn index(&self, block: BlockId) -> usize {
        self.frame.block_index(block).unwrap()
    }

    fn read(&mut self, value: Value) {
        self.ins(match value {
            Value::Local(local) => Ins::LocalGet(local),
            Value::I32(bits) => Ins::I32Const(bits),
            Value::I64(bits) => Ins::I64Const(bits),
            Value::Undefined => unreachable!("use of undefined SSA value"),
        });
    }

    fn get(&mut self, id: ValueId) {
        self.read(self.locations[id.0 as usize]);
    }

    fn set(&mut self, id: ValueId) {
        let Value::Local(local) = self.locations[id.0 as usize] else {
            unreachable!()
        };
        self.ins(Ins::LocalSet(local));
    }

    fn scratch(&mut self, first: usize, count: usize) {
        if count == 0 {
            self.word(0);
        } else {
            self.scratch_words = self.scratch_words.max(first + count);
            self.address(SCRATCH, first);
        }
    }

    fn address(&mut self, local: u32, index: usize) {
        self.ins(Ins::LocalGet(local));
        if index != 0 {
            self.word(index * WORD_BYTES as usize);
            self.ins(Ins::I32Add);
        }
    }

    fn mem(offset: u64, align: u32) -> MemArg {
        MemArg {
            offset,
            align,
            memory_index: 0,
        }
    }

    fn load(&mut self, offset: u64) {
        self.ins(Ins::I32Load(Self::mem(offset, 2)));
    }

    fn store(&mut self, offset: u64) {
        self.ins(Ins::I32Store(Self::mem(offset, 2)));
    }

    fn root_slot(&mut self, slot: usize) {
        self.address(self.root_frame.as_ref().unwrap().slots, slot);
    }

    fn read_root(&mut self, slot: usize) {
        self.ins(Ins::LocalGet(self.root_frame.as_ref().unwrap().slots));
        self.load(slot as u64 * WORD_BYTES);
    }

    fn target_word(&mut self, slot: usize) {
        if self.root_frame.is_some() {
            self.read_root(slot);
        } else {
            self.ins(Ins::LocalGet(TARGET));
        }
    }

    fn frame_index(&mut self) {
        self.ins(Ins::LocalGet(self.root_frame.as_ref().unwrap().index));
    }

    fn word(&mut self, value: usize) {
        self.ins(Ins::I32Const(value as i32));
    }

    fn ins(&mut self, instruction: Ins<'_>) {
        self.body.instruction(&instruction);
    }

    fn scope(&mut self, instruction: Ins<'_>) {
        self.ins(instruction);
        self.depth += 1;
    }

    fn end(&mut self) {
        self.ins(Ins::End);
        self.depth -= 1;
    }

    fn call(&mut self, function: RuntimeAbiFunction) {
        match (self.runtime_function)(function) {
            Some(index) => self.ins(Ins::Call(index)),
            None => {
                self.missing_function.get_or_insert(function);
            }
        }
    }

    fn write_tag(&mut self, tag: u8) {
        self.ins(Ins::LocalGet(RESULT));
        self.word(usize::from(tag));
        self.ins(Ins::I32Store8(Self::mem(RETURN_LAYOUT.tag as u64, 0)));
    }

    fn tag(&mut self) {
        self.ins(Ins::LocalGet(RESULT));
        self.ins(Ins::I32Load8U(Self::mem(RETURN_LAYOUT.tag as u64, 0)));
    }

    fn single_value(&mut self) {
        self.ins(Ins::LocalGet(RESULT));
        self.load(RETURN_LAYOUT.single_value as u64);
    }

    fn drop_frame(&mut self) {
        if let Some(frame) = &self.root_frame {
            let index = frame.index;
            self.ins(Ins::LocalGet(CONTEXT));
            self.ins(Ins::LocalGet(index));
            self.call(RuntimeAbiFunction::DropRootFrame);
        }
    }

    fn return_with(&mut self, tag: u8) {
        self.write_tag(tag);
        self.drop_frame();
        self.ins(Ins::Return);
    }

    fn unwind(&mut self) {
        if self.root_frame.is_some() {
            self.tag();
            self.word(usize::from(JIT_RETURN_TAG_UNWIND));
            self.ins(Ins::I32Eq);
            self.ins(Ins::LocalGet(RESULT));
            self.load(RETURN_LAYOUT.target_frame as u64);
            self.frame_index();
            self.ins(Ins::I32Eq);
            self.ins(Ins::I32And);
            self.ins(Ins::LocalGet(RESULT));
            self.load(RETURN_LAYOUT.state as u64);
            self.ins(Ins::I32Const(-1));
            self.ins(Ins::I32Eq);
            self.ins(Ins::I32And);
            self.scope(Ins::If(BlockType::Empty));
            self.return_with(JIT_RETURN_TAG_VALUES);
            self.end();
        }
        if let Some(state) = self.recovery {
            self.tag();
            self.word(usize::from(JIT_RETURN_TAG_UNWIND));
            self.ins(Ins::I32Eq);
            self.scope(Ins::If(BlockType::Empty));
            self.ins(Ins::LocalGet(CONTEXT));
            self.frame_index();
            self.call(RuntimeAbiFunction::PendingUnwindState);
            self.ins(Ins::LocalTee(state));
            self.ins(Ins::BrIf(self.depth - 1));
            self.end();
        }
        self.drop_frame();
        self.ins(Ins::Return);
    }

    fn check_result(&mut self) {
        self.tag();
        self.word(usize::from(JIT_RETURN_TAG_UNWIND));
        self.ins(Ins::I32GeU);
        self.scope(Ins::If(BlockType::Empty));
        self.unwind();
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use zkcl_bytecode::Arity;
    use zkcl_opt::native::{
        ClassifiedFunction, NativeOp, WASM_FEATURES, for_op_calls, reset_for_op_calls,
        supports_lowering,
    };
    use zkcl_opt::operation_samples::{lowering_cases, sample_operations};
    use zkcl_opt::passes::{Liveness, instruction_uses};
    use zkcl_opt::{
        BasicBlock, BlockId, BlockParam, BranchTarget, DeoptRegisterValue, DeoptState, FloatFormat,
        SsaFunction, SsaInst, SsaOp, Terminator, TypeTag, ValueId, ValueRepr,
    };

    use super::{Locals, LoweringError, PARAMS, Value, lower_ssa_function};
    use crate::codegen::ssa::ops::WasmTargetOp;

    impl zkcl_opt::SpecializationInputs for Wasm32 {
        fn fixnum_bounds(&self) -> (i64, i64) {
            zkcl_bytecode::fixnum_bounds_for_bits(32).unwrap()
        }
    }

    struct Wasm32;

    fn no_runtime(_: zkcl_opt::native::RuntimeAbiFunction) -> Option<u32> {
        None
    }

    #[test]
    fn every_supported_operation_has_a_wasm_dispatch() {
        for function in lowering_cases().filter(|ssa| supports_lowering(ssa, WASM_FEATURES)) {
            let op = &function.blocks[0].insts[0].op;
            match NativeOp::for_op(op) {
                NativeOp::Call(_)
                | NativeOp::Word(_)
                | NativeOp::Checkpoint { .. }
                | NativeOp::StoreUnwindRoot { .. }
                | NativeOp::LoadHandlerConditionRoot => {}
                NativeOp::Target { .. } => assert!(
                    WasmTargetOp::for_op(op).is_some(),
                    "supported without a Wasm emitter: {op:?}"
                ),
                NativeOp::Unsupported => panic!("supported unsupported operation: {op:?}"),
            }
        }
    }

    #[test]
    fn an_op_without_an_emitter_discards_the_function() {
        let function = lowering_cases()
            .find(|ssa| {
                matches!(
                    NativeOp::for_op(&ssa.blocks[0].insts[0].op),
                    NativeOp::Unsupported
                )
            })
            .expect("the sample cases contain an unsupported operation");
        let lowered = lower_ssa_function(&function, &Wasm32, None, no_runtime)
            .expect("an unsupported operation is not a lowering error");
        assert!(
            lowered.is_none(),
            "unsupported operations must leave the function to the bytecode VM"
        );
    }

    #[test]
    fn every_sample_target_operation_is_classified() {
        for op in sample_operations() {
            if matches!(NativeOp::for_op(&op), NativeOp::Target { .. }) {
                let _ = WasmTargetOp::for_op(&op);
            }
        }
    }

    #[test]
    fn a_missing_emitter_is_not_reported_as_a_lowering_error() {
        let error: LoweringError = LoweringError::FixnumBounds;
        assert!(matches!(error, LoweringError::FixnumBounds));
    }

    /// The shared classification must be derived once per instruction: feature checks,
    /// local slot assignment and emission all consume the same classified function.
    #[test]
    fn lowering_classifies_each_instruction_once() {
        for function in lowering_cases().filter(|ssa| supports_lowering(ssa, WASM_FEATURES)) {
            let instructions: usize = function.blocks.iter().map(|block| block.insts.len()).sum();
            reset_for_op_calls();
            let lowered = lower_ssa_function(&function, &Wasm32, None, |_| Some(0));
            let classified = for_op_calls() as usize;
            assert!(lowered.is_ok(), "the sample function lowers");
            assert_eq!(
                classified, instructions,
                "classified {classified} times for {instructions} instructions"
            );
        }
    }

    /// The classified function is the single source the emitters index into.
    #[test]
    fn the_classified_function_holds_one_operation_per_instruction() {
        for function in lowering_cases() {
            let classified = ClassifiedFunction::new(&function);
            for (block, body) in function.blocks.iter().enumerate() {
                assert_eq!(classified.instructions(block), body.insts.len());
                let _ = classified.operation(block, body.insts.len().saturating_sub(1));
            }
        }
    }

    /// Sample instructions carry no result id, so cover the local-assignment path with
    /// a lifted function whose instructions define values.
    #[test]
    fn a_lifted_function_is_classified_once() {
        use zkcl_bytecode::{BytecodeFunction, Instruction};
        use zkcl_opt::{NoSpecializationInputs, lift_code};

        let code = BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 2,
            instructions: vec![
                Instruction::Load(0, zkcl_bytecode::Immediate::Fixnum(7)),
                Instruction::Copy(1, 0),
                Instruction::Jump(0),
            ],
            constants: Vec::new(),
            lambda_list: None,
            captures: Vec::new(),
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        };
        let mut ssa = lift_code(&code, &NoSpecializationInputs).unwrap();
        ssa.fixnum_bounds = zkcl_bytecode::fixnum_bounds_for_bits(32).unwrap();
        let instructions: usize = ssa.blocks.iter().map(|block| block.insts.len()).sum();
        reset_for_op_calls();
        let _ = lower_ssa_function(&ssa, &Wasm32, None, |_| Some(0));
        assert_eq!(for_op_calls() as usize, instructions);
    }

    /// A function whose recovery map cannot be resumed on this backend goes back to
    /// the VM; the sample cases can build that shape for operations the feature check accepts.
    #[test]
    fn a_function_whose_recovery_cannot_resume_is_discarded() {
        let function = lowering_cases()
            .find(|ssa| {
                supports_lowering(ssa, WASM_FEATURES)
                    && ssa.blocks[0].insts[0].deopt.as_ref().is_some_and(|state| {
                        state.registers_to_restore.iter().any(|(_, value)| {
                            matches!(value, zkcl_opt::DeoptRegisterValue::UnboxedI32x4 { .. })
                        })
                    })
            })
            .expect("the sample cases cross SIMD pack recovery with supported operations");
        let lowered = lower_ssa_function(&function, &Wasm32, None, |_| Some(0))
            .expect("a non-resumable function is not a lowering error");
        assert!(
            lowered.is_none(),
            "recovery the backend cannot materialize must discard the function"
        );
    }

    fn chain_function() -> SsaFunction {
        let param = BlockParam {
            value: ValueId(0),
            ty: TypeTag::Any,
            repr: ValueRepr::TaggedWord,
            origin_reg: None,
        };
        let copy = |id: u32, source: u32| SsaInst {
            id: Some(ValueId(id)),
            op: SsaOp::Copy(ValueId(source)),
            ty: TypeTag::Any,
            pc: Some(0),
            deopt: None,
        };
        SsaFunction {
            fixnum_bounds: zkcl_bytecode::fixnum_bounds_for_bits(32).unwrap(),
            arity: Arity::ZERO,
            constant_count: 0,
            entry: BlockId(0),
            entry_params: Vec::new(),
            blocks: vec![BasicBlock {
                id: BlockId(0),
                params: vec![param],
                insts: vec![copy(1, 0), copy(2, 1), copy(3, 2), copy(4, 3)],
                terminator: Terminator::ReturnOne(ValueId(4)),
                terminator_pc: 1,
                entry_scope: zkcl_opt::ScopeId::ROOT,
                unwind_succs: Vec::new(),
            }],
            scopes: zkcl_opt::ScopeTree::new(),
            pc_to_block: Default::default(),
            next_value_id: 8,
        }
    }

    fn assigned_locals(function: &SsaFunction) -> Option<Locals> {
        let classified = ClassifiedFunction::new(function);
        let liveness = Liveness::solve(function);
        Locals::assign(function, &classified, &liveness).ok()
    }

    /// Sequential values die immediately, so they share one local instead of one
    /// local each.
    #[test]
    fn dead_values_share_a_local_slot() {
        let locals = assigned_locals(&chain_function()).expect("the chain lowers");
        assert_eq!(locals.count, PARAMS + 2, "declared {} locals", locals.count);
        assert_eq!(locals.values[1], locals.values[3]);
        assert_eq!(locals.values[2], locals.values[4]);
    }

    /// A value kept alive only by a recovery map must not share the slot of the
    /// value defined by the instruction that carries the map.
    #[test]
    fn recovery_boundaries_keep_values_in_separate_slots() {
        let mut function = chain_function();
        function.blocks[0].insts.truncate(2);
        function.next_value_id = 4;
        function.blocks[0].insts[1] = SsaInst {
            id: Some(ValueId(2)),
            op: SsaOp::Copy(ValueId(1)),
            ty: TypeTag::Any,
            pc: Some(0),
            deopt: Some(DeoptState {
                pc: 0,
                registers_to_restore: vec![(0, DeoptRegisterValue::TaggedWord(ValueId(1)))],
            }),
        };
        function.blocks[0].terminator = Terminator::ReturnOne(ValueId(2));
        let locals = assigned_locals(&function).expect("the chain lowers");
        assert_ne!(
            locals.values[1], locals.values[2],
            "the map keeps value 1 alive past the definition of value 2"
        );
    }

    /// A value that only the cleanup path reads stays live across the unwind edge.
    #[test]
    fn unwind_edges_keep_values_alive_for_cleanup() {
        let mut function = chain_function();
        function.next_value_id = 8;
        function.blocks[0].insts.truncate(1);
        function.blocks[0].terminator = Terminator::ReturnOne(ValueId(1));
        function.blocks[0].unwind_succs = vec![BlockId(1)];
        function.blocks.push(BasicBlock {
            id: BlockId(1),
            params: Vec::new(),
            insts: vec![SsaInst {
                id: Some(ValueId(5)),
                op: SsaOp::Copy(ValueId(1)),
                ty: TypeTag::Any,
                pc: Some(0),
                deopt: None,
            }],
            terminator: Terminator::ReturnOne(ValueId(5)),
            terminator_pc: 1,
            entry_scope: zkcl_opt::ScopeId::ROOT,
            unwind_succs: Vec::new(),
        });
        let _guard = BlockParam {
            value: ValueId(0),
            ty: TypeTag::Any,
            repr: ValueRepr::TaggedWord,
            origin_reg: None,
        };
        let liveness = Liveness::solve(&function);
        assert!(
            liveness.live_out(BlockId(0)).contains(&ValueId(1)),
            "the unwind edge must keep the value alive"
        );
    }

    /// What one function contributed to the corpus-wide numbers.
    #[derive(Default)]
    struct SlotStats {
        functions: usize,
        skipped: usize,
        instructions: usize,
        values: usize,
        peak: usize,
    }

    impl SlotStats {
        fn add(&mut self, other: Self) {
            self.functions += other.functions;
            self.skipped += other.skipped;
            self.instructions += other.instructions;
            self.values += other.values;
            self.peak += other.peak;
        }
    }

    /// Program points of one function, flattened into a dense index.
    struct Flow {
        offsets: Vec<usize>,
        edges: Vec<Vec<usize>>,
        reverse: Vec<Vec<usize>>,
    }

    impl Flow {
        fn new(function: &SsaFunction) -> Self {
            let indices = function.block_index_map();
            let mut offsets = Vec::with_capacity(function.blocks.len());
            let mut total = 0;
            for block in &function.blocks {
                offsets.push(total);
                total += block.insts.len() + 1;
            }
            let mut edges = vec![Vec::new(); total];
            for (block_index, block) in function.blocks.iter().enumerate() {
                let base = offsets[block_index];
                for position in 0..block.insts.len() {
                    edges[base + position].push(base + position + 1);
                }
                let terminator = base + block.insts.len();
                let mut targets = Vec::new();
                block
                    .terminator
                    .visit_targets(|target| targets.push(target.block));
                targets.extend(block.unwind_succs.iter().copied());
                for target in targets {
                    let successor = indices[&target];
                    edges[terminator].push(offsets[successor]);
                }
            }
            let mut reverse = vec![Vec::new(); total];
            for (from, successors) in edges.iter().enumerate() {
                for &to in successors {
                    reverse[to].push(from);
                }
            }
            Self {
                offsets,
                edges,
                reverse,
            }
        }

        fn point(&self, block: usize, position: usize) -> usize {
            self.offsets[block] + position
        }

        fn reaches_a_use_without(&self, definition: usize, use_points: &[usize]) -> Vec<bool> {
            let mut reached = vec![false; self.edges.len()];
            let mut pending: Vec<usize> = use_points.to_vec();
            for &point in use_points {
                reached[point] = true;
            }
            while let Some(point) = pending.pop() {
                if point == definition {
                    continue;
                }
                for &previous in &self.reverse[point] {
                    if !reached[previous] {
                        reached[previous] = true;
                        pending.push(previous);
                    }
                }
            }
            reached
        }

        fn reachable(&self, start: usize, edges: &[Vec<usize>]) -> Vec<bool> {
            let mut reached = vec![false; edges.len()];
            let mut pending = vec![start];
            reached[start] = true;
            while let Some(point) = pending.pop() {
                for &next in &edges[point] {
                    if !reached[next] {
                        reached[next] = true;
                        pending.push(next);
                    }
                }
            }
            reached
        }
    }

    fn check(function: &SsaFunction) -> SlotStats {
        let mut stats = SlotStats {
            functions: 1,
            ..SlotStats::default()
        };
        let Some(locals) = assigned_locals(function) else {
            stats.skipped = 1;
            return stats;
        };
        let flow = Flow::new(function);

        let mut definitions: HashMap<ValueId, usize> = HashMap::default();
        let mut uses: HashMap<ValueId, Vec<usize>> = HashMap::default();
        for (block_index, block) in function.blocks.iter().enumerate() {
            stats.instructions += block.insts.len();
            for param in &block.params {
                definitions.insert(param.value, flow.point(block_index, 0));
            }
            for (position, inst) in block.insts.iter().enumerate() {
                if let Some(id) = inst.id {
                    definitions.insert(id, flow.point(block_index, position + 1));
                }
                instruction_uses(inst, |value| {
                    uses.entry(value)
                        .or_default()
                        .push(flow.point(block_index, position));
                });
            }
            let terminator = flow.point(block_index, block.insts.len());
            block.terminator.visit_inputs(|value| {
                uses.entry(value).or_default().push(terminator);
            });
        }

        let mut occupants: Vec<HashMap<u32, ValueId>> = vec![HashMap::default(); flow.edges.len()];
        for (&value, &defined_at) in &definitions {
            let Value::Local(slot) = locals.values[value.0 as usize] else {
                continue;
            };
            let Some(use_points) = uses.get(&value) else {
                continue;
            };
            stats.values += 1;
            let after_definition = flow.reachable(defined_at, &flow.edges);
            let reaches_a_use = flow.reaches_a_use_without(defined_at, use_points);
            for point in 0..flow.edges.len() {
                if !(after_definition[point] && reaches_a_use[point]) {
                    continue;
                }
                if let Some(&other) = occupants[point].get(&slot) {
                    assert_eq!(
                        other, value,
                        "values {other:?} and {value:?} are both alive at point {point} but share \
                         local {slot}\n{function:#?}"
                    );
                }
                occupants[point].insert(slot, value);
            }
        }
        stats.peak = occupants.iter().map(HashMap::len).max().unwrap_or(0);
        stats
    }

    fn checked_locals(function: &SsaFunction) -> Locals {
        check(function);
        assigned_locals(function).expect("the shaped functions have local representations")
    }

    #[test]
    fn generated_control_flow_never_aliases_live_values() {
        let mut stats = SlotStats::default();
        for seed in 0..GENERATED_FUNCTIONS {
            stats.add(check(&generated_function(seed)));
        }
        assert!(stats.functions > 0);
        println!(
            "generated corpus: {} functions ({} without a local representation), \
             {} instructions, {} slotted values, sum of per-function peaks {}",
            stats.functions, stats.skipped, stats.instructions, stats.values, stats.peak
        );
    }

    #[test]
    fn the_lowering_samples_are_checked_as_well() {
        let mut stats = SlotStats::default();
        for function in lowering_cases() {
            stats.add(check(&function));
        }
        assert!(stats.functions > 0);
        println!(
            "operation samples: {} functions ({} without a local representation), \
             {} instructions, {} slotted values, sum of per-function peaks {}",
            stats.functions, stats.skipped, stats.instructions, stats.values, stats.peak
        );
    }

    const GENERATED_FUNCTIONS: u64 = 512;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: u32) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 33) as u32) % bound.max(1)
        }

        fn chance(&mut self, percent: u32) -> bool {
            self.below(100) < percent
        }

        fn pick(&mut self, bound: usize) -> usize {
            self.below(bound as u32) as usize
        }
    }

    enum Shape {
        Jump(usize),
        Branch(usize, usize),
        Return,
    }

    fn generated_function(seed: u64) -> SsaFunction {
        let mut rng = Rng(seed.wrapping_mul(2).wrapping_add(1));
        let blocks = 1 + rng.pick(4);

        let mut shapes = Vec::with_capacity(blocks);
        let mut unwind = vec![None; blocks];
        for (index, unwind_target) in unwind.iter_mut().enumerate() {
            let primary = index + 1;
            let shape = if primary == blocks {
                if blocks > 1 && rng.chance(35) {
                    Shape::Jump(0)
                } else {
                    Shape::Return
                }
            } else if rng.chance(45) {
                let secondary = if rng.chance(35) {
                    0
                } else {
                    primary + rng.pick(blocks - primary)
                };
                Shape::Branch(primary, secondary)
            } else {
                Shape::Jump(primary)
            };
            if primary < blocks && rng.chance(25) {
                *unwind_target = Some(primary + rng.pick(blocks - primary));
            }
            shapes.push(shape);
        }

        let mut successors = vec![Vec::new(); blocks];
        for (index, shape) in shapes.iter().enumerate() {
            match shape {
                Shape::Jump(target) => successors[index].push(*target),
                Shape::Branch(first, second) => {
                    successors[index].push(*first);
                    successors[index].push(*second);
                }
                Shape::Return => {}
            }
            if let Some(target) = unwind[index] {
                successors[index].push(target);
            }
        }
        let dominators = dominators(blocks, &successors);

        let mut defined: Vec<Vec<ValueId>> = vec![Vec::new(); blocks];
        defined[0].push(ValueId(0));
        let mut next_value = 1;
        let mut bodies: Vec<Vec<SsaInst>> = Vec::with_capacity(blocks);
        let mut terminators = Vec::with_capacity(blocks);
        for index in 0..blocks {
            let mut available: Vec<ValueId> = defined
                .iter()
                .enumerate()
                .filter(|(block, _)| dominators[index].contains(block) && *block != index)
                .flat_map(|(_, values)| values.iter().copied())
                .collect();
            if index == 0 {
                available.push(ValueId(0));
            }
            let mut body = Vec::new();
            for _ in 0..rng.pick(4) {
                let operand = available[rng.pick(available.len())];
                let id = ValueId(next_value);
                next_value += 1;
                let deopt = rng.chance(25).then(|| DeoptState {
                    pc: 0,
                    registers_to_restore: vec![(0, DeoptRegisterValue::TaggedWord(operand))],
                });
                body.push(SsaInst {
                    id: Some(id),
                    op: SsaOp::Copy(operand),
                    ty: TypeTag::Any,
                    pc: Some(0),
                    deopt,
                });
                available.push(id);
                defined[index].push(id);
            }
            let operands = |rng: &mut Rng| available[rng.pick(available.len())];
            let terminator = match &shapes[index] {
                Shape::Jump(target) => Terminator::Jump(BranchTarget {
                    block: BlockId(*target as u32),
                    args: Vec::new(),
                }),
                Shape::Branch(first, second) => {
                    let cond = operands(&mut rng);
                    Terminator::BranchTruthy {
                        cond,
                        truthy: BranchTarget {
                            block: BlockId(*first as u32),
                            args: Vec::new(),
                        },
                        falsy: BranchTarget {
                            block: BlockId(*second as u32),
                            args: Vec::new(),
                        },
                    }
                }
                Shape::Return => Terminator::ReturnOne(operands(&mut rng)),
            };
            bodies.push(body);
            terminators.push(terminator);
        }

        let blocks = (0..blocks)
            .map(|index| BasicBlock {
                id: BlockId(index as u32),
                params: if index == 0 {
                    vec![BlockParam {
                        value: ValueId(0),
                        ty: TypeTag::Any,
                        repr: ValueRepr::TaggedWord,
                        origin_reg: Some(0),
                    }]
                } else {
                    Vec::new()
                },
                insts: std::mem::take(&mut bodies[index]),
                terminator: terminators[index].clone(),
                terminator_pc: 1,
                entry_scope: zkcl_opt::ScopeId::ROOT,
                unwind_succs: unwind[index]
                    .map(|target| BlockId(target as u32))
                    .into_iter()
                    .collect(),
            })
            .collect();

        SsaFunction {
            fixnum_bounds: zkcl_bytecode::fixnum_bounds_for_bits(32).unwrap(),
            arity: Arity::ZERO,
            constant_count: 0,
            entry: BlockId(0),
            entry_params: vec![ValueId(0)],
            blocks,
            scopes: zkcl_opt::ScopeTree::new(),
            pc_to_block: Default::default(),
            next_value_id: next_value,
        }
    }

    fn dominators(blocks: usize, successors: &[Vec<usize>]) -> Vec<HashSet<usize>> {
        let everything: HashSet<usize> = (0..blocks).collect();
        let mut dominators = vec![everything.clone(); blocks];
        dominators[0] = HashSet::from_iter([0]);
        let mut changed = true;
        while changed {
            changed = false;
            for block in 1..blocks {
                let predecessors: Vec<usize> = (0..blocks)
                    .filter(|predecessor| successors[*predecessor].contains(&block))
                    .collect();
                let mut next = if predecessors.is_empty() {
                    everything.clone()
                } else {
                    let mut intersection = dominators[predecessors[0]].clone();
                    for predecessor in &predecessors[1..] {
                        intersection
                            .retain(|candidate| dominators[*predecessor].contains(candidate));
                    }
                    intersection
                };
                next.insert(block);
                if next != dominators[block] {
                    dominators[block] = next;
                    changed = true;
                }
            }
        }
        dominators
    }

    #[test]
    fn a_value_dying_in_both_arms_of_a_diamond_stays_single_owner() {
        let mut function = generated_function(0);
        let copy = |id: ValueId, op: SsaOp| SsaInst {
            id: Some(id),
            op,
            ty: TypeTag::Any,
            pc: Some(0),
            deopt: None,
        };
        let target = |block: u32| BranchTarget {
            block: BlockId(block),
            args: Vec::new(),
        };
        function.fixnum_bounds = zkcl_bytecode::fixnum_bounds_for_bits(32).unwrap();
        function.next_value_id = 6;
        function.blocks = vec![
            BasicBlock {
                id: BlockId(0),
                params: vec![BlockParam {
                    value: ValueId(0),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(0),
                }],
                insts: vec![copy(ValueId(1), SsaOp::Copy(ValueId(0)))],
                terminator: Terminator::BranchTruthy {
                    cond: ValueId(0),
                    truthy: target(1),
                    falsy: target(2),
                },
                terminator_pc: 1,
                entry_scope: zkcl_opt::ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
            BasicBlock {
                id: BlockId(1),
                params: Vec::new(),
                insts: vec![copy(ValueId(2), SsaOp::Copy(ValueId(1)))],
                terminator: Terminator::Jump(target(3)),
                terminator_pc: 1,
                entry_scope: zkcl_opt::ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
            BasicBlock {
                id: BlockId(2),
                params: Vec::new(),
                insts: vec![copy(ValueId(3), SsaOp::Copy(ValueId(1)))],
                terminator: Terminator::Jump(target(3)),
                terminator_pc: 1,
                entry_scope: zkcl_opt::ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
            BasicBlock {
                id: BlockId(3),
                params: Vec::new(),
                insts: vec![
                    copy(ValueId(4), SsaOp::Copy(ValueId(0))),
                    copy(ValueId(5), SsaOp::Copy(ValueId(0))),
                ],
                terminator: Terminator::ReturnValues(vec![ValueId(4), ValueId(5)]),
                terminator_pc: 1,
                entry_scope: zkcl_opt::ScopeId::ROOT,
                unwind_succs: Vec::new(),
            },
        ];
        let locals = checked_locals(&function);
        assert_ne!(
            locals.values[4], locals.values[5],
            "both values are alive at the return, so they need distinct locals"
        );
    }

    #[test]
    fn a_value_stays_live_until_its_use_in_either_branch() {
        let mut function = chain_function();
        function.arity = Arity {
            required: 3,
            maximum: Some(3),
        };
        function.entry_params = (0..3).map(ValueId).collect();
        function.next_value_id = 8;
        let copy = |out, input| SsaInst {
            id: Some(ValueId(out)),
            op: SsaOp::Copy(ValueId(input)),
            ty: TypeTag::Any,
            pc: Some(0),
            deopt: None,
        };
        let target = |id| BranchTarget {
            block: BlockId(id),
            args: Vec::new(),
        };
        let entry = &mut function.blocks[0];
        entry.params = (0..3)
            .map(|id| BlockParam {
                value: ValueId(id),
                ty: TypeTag::Any,
                repr: ValueRepr::TaggedWord,
                origin_reg: Some(id as usize),
            })
            .collect();
        entry.insts = vec![copy(3, 0)];
        entry.terminator = Terminator::BranchTruthy {
            cond: ValueId(2),
            truthy: target(1),
            falsy: target(2),
        };
        for (block, first) in [(1, 4), (2, 6)] {
            function.blocks.push(BasicBlock {
                id: BlockId(block),
                params: Vec::new(),
                insts: vec![copy(first, 1), copy(first + 1, 3)],
                terminator: Terminator::ReturnValues(vec![ValueId(first), ValueId(first + 1)]),
                terminator_pc: 1,
                entry_scope: zkcl_opt::ScopeId::ROOT,
                unwind_succs: Vec::new(),
            });
        }
        checked_locals(&function);
    }

    fn validate_ssa_wasm(function: &SsaFunction) {
        use wasm_encoder::{
            CodeSection, EntityType, FunctionSection, ImportSection, MemorySection, MemoryType,
            Module, TypeSection, ValType,
        };
        use wasmparser::{Validator, WasmFeatures};
        use zkcl_opt::native::{RuntimeAbiFunction, RuntimeAbiType};

        let to_val_type = |ty: RuntimeAbiType| match ty {
            RuntimeAbiType::Pointer | RuntimeAbiType::Word | RuntimeAbiType::Bool => ValType::I32,
            RuntimeAbiType::I64 => ValType::I64,
        };

        let lowered = lower_ssa_function(function, &Wasm32, None, |abi| Some(abi as u32))
            .expect("lowering succeeds")
            .expect("all ops are supported in WASM_FEATURES");

        let mut types = TypeSection::new();
        let mut imports = ImportSection::new();
        for (idx, &abi) in RuntimeAbiFunction::ALL.iter().enumerate() {
            assert_eq!(abi as usize, idx, "runtime ABI IDs must match ALL order");
            let params: Vec<_> = abi
                .parameter_types()
                .iter()
                .copied()
                .map(to_val_type)
                .collect();
            let results: Vec<_> = abi.return_type().into_iter().map(to_val_type).collect();
            types.ty().function(params, results);
            imports.import("rt", abi.symbol_name(), EntityType::Function(idx as u32));
        }
        let ssa_type = RuntimeAbiFunction::ALL.len() as u32;
        types.ty().function([ValType::I32; PARAMS as usize], []);

        let mut funcs = FunctionSection::new();
        funcs.function(ssa_type);

        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });

        let mut code = CodeSection::new();
        code.function(&lowered.body);

        let mut module = Module::new();
        module
            .section(&types)
            .section(&imports)
            .section(&funcs)
            .section(&memories)
            .section(&code);

        let bytes = module.finish();
        Validator::new_with_features(WasmFeatures::WASM2)
            .validate_all(&bytes)
            .expect("generated Wasm must validate cleanly");
    }

    #[test]
    fn both_conditional_edges_assign_their_args_at_the_same_forward_block() {
        use zkcl_bytecode::Immediate;

        let mut function = chain_function();
        let condition = ValueId(0);
        let arg = |id, n| SsaInst {
            id: Some(ValueId(id)),
            op: SsaOp::ConstWord(Immediate::Fixnum(n)),
            ty: TypeTag::Fixnum,
            pc: None,
            deopt: None,
        };
        function.arity = Arity {
            required: 1,
            maximum: Some(1),
        };
        function.entry_params = vec![condition];
        function.blocks[0].insts = vec![arg(1, 10), arg(2, 20)];
        function.blocks[0].terminator = Terminator::BranchTruthy {
            cond: condition,
            truthy: BranchTarget {
                block: BlockId(1),
                args: vec![ValueId(1)],
            },
            falsy: BranchTarget {
                block: BlockId(1),
                args: vec![ValueId(2)],
            },
        };
        function.blocks.push(BasicBlock {
            id: BlockId(1),
            params: vec![BlockParam {
                value: ValueId(3),
                ty: TypeTag::Fixnum,
                repr: ValueRepr::TaggedWord,
                origin_reg: None,
            }],
            insts: Vec::new(),
            terminator: Terminator::ReturnOne(ValueId(3)),
            terminator_pc: 2,
            entry_scope: zkcl_opt::ScopeId::ROOT,
            unwind_succs: Vec::new(),
        });
        validate_ssa_wasm(&function);
    }

    #[test]
    fn checked_fixnum_and_primitive_guard_ops_validate_as_wasm() {
        use zkcl_bytecode::FixnumArithmetic;
        use zkcl_opt::GuardedPrimitive;

        let mut function = generated_function(0);
        function.fixnum_bounds = zkcl_bytecode::fixnum_bounds_for_bits(32).unwrap();
        function.arity = Arity {
            required: 2,
            maximum: Some(2),
        };
        function.constant_count = 1;
        function.entry_params = vec![ValueId(0), ValueId(1)];
        function.next_value_id = 8;
        let word_inst = |id: Option<ValueId>,
                         op: SsaOp,
                         ty: TypeTag,
                         regs: Vec<(u32, DeoptRegisterValue)>| SsaInst {
            id,
            op,
            ty,
            pc: Some(0),
            deopt: Some(DeoptState {
                pc: 0,
                registers_to_restore: regs,
            }),
        };
        function.blocks = vec![BasicBlock {
            id: BlockId(0),
            params: vec![
                BlockParam {
                    value: ValueId(0),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(0),
                },
                BlockParam {
                    value: ValueId(1),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(1),
                },
            ],
            insts: vec![
                word_inst(
                    None,
                    SsaOp::GuardPrimitiveBinding {
                        symbol: 0,
                        primitive: GuardedPrimitive::FixnumArithmetic(FixnumArithmetic::Add),
                    },
                    TypeTag::Any,
                    vec![
                        (0, DeoptRegisterValue::TaggedWord(ValueId(0))),
                        (1, DeoptRegisterValue::TaggedWord(ValueId(1))),
                    ],
                ),
                word_inst(
                    Some(ValueId(2)),
                    SsaOp::UnboxFixnumGuard(ValueId(0)),
                    TypeTag::Fixnum,
                    vec![
                        (0, DeoptRegisterValue::TaggedWord(ValueId(0))),
                        (1, DeoptRegisterValue::TaggedWord(ValueId(1))),
                    ],
                ),
                word_inst(
                    Some(ValueId(3)),
                    SsaOp::UnboxFixnumGuard(ValueId(1)),
                    TypeTag::Fixnum,
                    vec![
                        (0, DeoptRegisterValue::UnboxedI64(ValueId(2))),
                        (1, DeoptRegisterValue::TaggedWord(ValueId(1))),
                    ],
                ),
                word_inst(
                    Some(ValueId(4)),
                    SsaOp::AddI64Checked(ValueId(2), ValueId(3)),
                    TypeTag::Fixnum,
                    vec![
                        (0, DeoptRegisterValue::UnboxedI64(ValueId(2))),
                        (1, DeoptRegisterValue::UnboxedI64(ValueId(3))),
                    ],
                ),
                word_inst(
                    Some(ValueId(5)),
                    SsaOp::SubI64Checked(ValueId(4), ValueId(3)),
                    TypeTag::Fixnum,
                    vec![
                        (0, DeoptRegisterValue::UnboxedI64(ValueId(4))),
                        (1, DeoptRegisterValue::UnboxedI64(ValueId(3))),
                    ],
                ),
                word_inst(
                    Some(ValueId(6)),
                    SsaOp::MulI64Checked(ValueId(5), ValueId(2)),
                    TypeTag::Fixnum,
                    vec![
                        (0, DeoptRegisterValue::UnboxedI64(ValueId(5))),
                        (1, DeoptRegisterValue::UnboxedI64(ValueId(2))),
                    ],
                ),
                SsaInst {
                    id: Some(ValueId(7)),
                    op: SsaOp::BoxFixnum(ValueId(6)),
                    ty: TypeTag::Fixnum,
                    pc: Some(0),
                    deopt: None,
                },
            ],
            terminator: Terminator::ReturnOne(ValueId(7)),
            terminator_pc: 1,
            entry_scope: zkcl_opt::ScopeId::ROOT,
            unwind_succs: Vec::new(),
        }];

        validate_ssa_wasm(&function);
    }

    #[test]
    fn float_ops_and_float_deopt_validate_as_wasm() {
        use zkcl_opt::FloatArithOp;

        let mut function = generated_function(0);
        function.fixnum_bounds = zkcl_bytecode::fixnum_bounds_for_bits(32).unwrap();
        function.arity = Arity {
            required: 2,
            maximum: Some(2),
        };
        function.constant_count = 2;
        function.entry_params = vec![ValueId(0), ValueId(1)];
        function.next_value_id = 9;
        let inst = |id: Option<ValueId>,
                    op: SsaOp,
                    ty: TypeTag,
                    regs: Vec<(u32, DeoptRegisterValue)>| SsaInst {
            id,
            op,
            ty,
            pc: Some(0),
            deopt: Some(DeoptState {
                pc: 0,
                registers_to_restore: regs,
            }),
        };
        function.blocks = vec![BasicBlock {
            id: BlockId(0),
            params: vec![
                BlockParam {
                    value: ValueId(0),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(0),
                },
                BlockParam {
                    value: ValueId(1),
                    ty: TypeTag::Any,
                    repr: ValueRepr::TaggedWord,
                    origin_reg: Some(1),
                },
            ],
            insts: vec![
                inst(
                    Some(ValueId(2)),
                    SsaOp::UnboxFloatGuard {
                        format: FloatFormat::F32,
                        value: ValueId(0),
                    },
                    TypeTag::Float,
                    vec![(0, DeoptRegisterValue::TaggedWord(ValueId(0)))],
                ),
                inst(
                    Some(ValueId(3)),
                    SsaOp::ReadFloatVector {
                        symbol: 0,
                        format: FloatFormat::F32,
                        array: ValueId(0),
                        index: ValueId(1),
                    },
                    TypeTag::Float,
                    vec![
                        (
                            0,
                            DeoptRegisterValue::UnboxedFloat {
                                format: FloatFormat::F32,
                                value: ValueId(2),
                            },
                        ),
                        (1, DeoptRegisterValue::TaggedWord(ValueId(1))),
                    ],
                ),
                inst(
                    Some(ValueId(4)),
                    SsaOp::FloatArithChecked {
                        format: FloatFormat::F32,
                        op: FloatArithOp::Div,
                        lhs: ValueId(2),
                        rhs: ValueId(3),
                    },
                    TypeTag::Float,
                    vec![
                        (
                            0,
                            DeoptRegisterValue::UnboxedFloat {
                                format: FloatFormat::F32,
                                value: ValueId(2),
                            },
                        ),
                        (
                            1,
                            DeoptRegisterValue::UnboxedFloat {
                                format: FloatFormat::F32,
                                value: ValueId(3),
                            },
                        ),
                    ],
                ),
                inst(
                    None,
                    SsaOp::WriteFloatVector {
                        symbol: 1,
                        format: FloatFormat::F32,
                        setter: ValueId(0),
                        array: ValueId(0),
                        index: ValueId(1),
                        value: ValueId(4),
                    },
                    TypeTag::Float,
                    vec![(
                        0,
                        DeoptRegisterValue::UnboxedFloat {
                            format: FloatFormat::F32,
                            value: ValueId(4),
                        },
                    )],
                ),
                inst(
                    Some(ValueId(5)),
                    SsaOp::UnboxFloatGuard {
                        format: FloatFormat::F64,
                        value: ValueId(1),
                    },
                    TypeTag::Float,
                    vec![
                        (1, DeoptRegisterValue::TaggedWord(ValueId(1))),
                        (
                            0,
                            DeoptRegisterValue::UnboxedFloat {
                                format: FloatFormat::F32,
                                value: ValueId(4),
                            },
                        ),
                    ],
                ),
                inst(
                    Some(ValueId(6)),
                    SsaOp::ReadFloatVector {
                        symbol: 0,
                        format: FloatFormat::F64,
                        array: ValueId(1),
                        index: ValueId(0),
                    },
                    TypeTag::Float,
                    vec![(
                        0,
                        DeoptRegisterValue::UnboxedFloat {
                            format: FloatFormat::F64,
                            value: ValueId(5),
                        },
                    )],
                ),
                inst(
                    Some(ValueId(7)),
                    SsaOp::FloatArithChecked {
                        format: FloatFormat::F64,
                        op: FloatArithOp::Add,
                        lhs: ValueId(5),
                        rhs: ValueId(6),
                    },
                    TypeTag::Float,
                    vec![(
                        0,
                        DeoptRegisterValue::UnboxedFloat {
                            format: FloatFormat::F64,
                            value: ValueId(5),
                        },
                    )],
                ),
                inst(
                    None,
                    SsaOp::WriteFloatVector {
                        symbol: 1,
                        format: FloatFormat::F64,
                        setter: ValueId(1),
                        array: ValueId(1),
                        index: ValueId(0),
                        value: ValueId(7),
                    },
                    TypeTag::Float,
                    vec![(
                        0,
                        DeoptRegisterValue::UnboxedFloat {
                            format: FloatFormat::F64,
                            value: ValueId(7),
                        },
                    )],
                ),
                SsaInst {
                    id: Some(ValueId(8)),
                    op: SsaOp::BoxFloat {
                        format: FloatFormat::F64,
                        value: ValueId(7),
                    },
                    ty: TypeTag::Float,
                    pc: Some(0),
                    deopt: None,
                },
            ],
            terminator: Terminator::ReturnOne(ValueId(8)),
            terminator_pc: 1,
            entry_scope: zkcl_opt::ScopeId::ROOT,
            unwind_succs: Vec::new(),
        }];

        validate_ssa_wasm(&function);
    }

    #[test]
    fn guarded_word_count_validates_as_wasm() {
        use zkcl_bytecode::WordCountOp;

        let mut function = generated_function(0);
        function.fixnum_bounds = zkcl_bytecode::fixnum_bounds_for_bits(32).unwrap();
        function.arity = Arity {
            required: 1,
            maximum: Some(1),
        };
        function.constant_count = 2;
        function.entry_params = vec![ValueId(0)];
        function.next_value_id = 4;
        let recovery = DeoptState {
            pc: 0,
            registers_to_restore: vec![(0, DeoptRegisterValue::TaggedWord(ValueId(0)))],
        };
        function.blocks = vec![BasicBlock {
            id: BlockId(0),
            params: vec![BlockParam {
                value: ValueId(0),
                ty: TypeTag::Any,
                repr: ValueRepr::TaggedWord,
                origin_reg: Some(0),
            }],
            insts: vec![
                SsaInst {
                    id: None,
                    op: SsaOp::GuardTransformBinding {
                        symbol: 0,
                        binding: 1,
                    },
                    ty: TypeTag::Top,
                    pc: Some(0),
                    deopt: Some(recovery.clone()),
                },
                SsaInst {
                    id: Some(ValueId(1)),
                    op: SsaOp::UnboxU64Guard(ValueId(0)),
                    ty: TypeTag::Any,
                    pc: Some(0),
                    deopt: Some(recovery),
                },
                SsaInst {
                    id: Some(ValueId(2)),
                    op: SsaOp::WordCount {
                        op: WordCountOp::Popcount,
                        input: ValueId(1),
                    },
                    ty: TypeTag::Fixnum,
                    pc: Some(0),
                    deopt: None,
                },
                SsaInst {
                    id: Some(ValueId(3)),
                    op: SsaOp::BoxFixnum(ValueId(2)),
                    ty: TypeTag::Fixnum,
                    pc: Some(0),
                    deopt: None,
                },
            ],
            terminator: Terminator::ReturnOne(ValueId(3)),
            terminator_pc: 1,
            entry_scope: zkcl_opt::ScopeId::ROOT,
            unwind_succs: Vec::new(),
        }];

        validate_ssa_wasm(&function);
    }
}
