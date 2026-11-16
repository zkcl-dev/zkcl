//! Unified Cranelift SSA-to-IR lowering for `zkcl`.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    Block, BlockArg, Inst, InstBuilder, MemFlags, StackSlot, StackSlotData, StackSlotKind, Type,
    UserFuncName, Value, types,
};
use cranelift_frontend::{FunctionBuilder, Variable};
use rustc_hash::FxHashMap;
use thiserror::Error;
use zkcl_bytecode::native_abi::I32x4Operation;
use zkcl_opt::{
    BlockId, CellTarget, CmpOp, FloatArithOp, FloatFormat, RootSlot, SsaFunction, SsaInst, SsaOp,
    Terminator, UnwindOp, ValueId, ValueRepr,
};

use zkcl_opt::native::{
    CRANELIFT_FEATURES, CallOperand, CallReturn, CallWhen, ClassifiedFunction, FIXNUM_SHIFT,
    JIT_RETURN_TAG_ONE, JIT_RETURN_TAG_TAIL_CALL, JIT_RETURN_TAG_VALUES, JitReturnLayout,
    NativeFrameLayout, RuntimeAbiFunction, RuntimeCall, SliceCharge, TAG_FIXNUM, TAG_MASK,
    TailCall, TailCallee, WORD_NIL, native_frame_layout,
};

mod arguments;
mod arithmetic;
mod calls;
mod deopt;
mod function;

use arithmetic::{CheckedFixnumArithmetic, try_fixnum_arithmetic};
use deopt::Deoptimization;
pub use function::{lower_ssa_function, lower_ssa_function_into};

/// Errors reported by shared Cranelift IR lowering.
#[derive(Debug, Error)]
pub enum LoweringError {
    #[error("unsupported SSA operation in Cranelift lowering: {0}")]
    UnsupportedOp(String),
    #[error("missing deoptimization state on speculative instruction")]
    MissingDeoptState,
    #[error("SSA fixnum bounds {actual:?} differ from the target's bounds {expected:?}")]
    FixnumBounds {
        expected: (i64, i64),
        actual: (i64, i64),
    },
    #[error("immediate value out of fixnum range: {0}")]
    ImmediateOutOfRange(i64),
    #[error("integer overflow converting {0}")]
    IntegerOverflow(&'static str),
}

/// Strategy for emitting a call to a [`RuntimeAbiFunction`] (`call` for AOT, `call_indirect` for JIT).
pub trait RuntimeCallEmitter {
    fn emit_runtime_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        runtime_function: RuntimeAbiFunction,
        args: &[Value],
    ) -> Result<Inst, LoweringError>;
}

/// An SSA function, its classified operations and its frame layout.
pub struct LoweringConfig<'a> {
    ssa_function: &'a SsaFunction,
    classified: ClassifiedFunction<'a>,
    frame_layout: NativeFrameLayout,
    func_name: UserFuncName,
}

impl<'a> LoweringConfig<'a> {
    /// Classify the function once and prepare the frame the backend can emit.
    pub fn new(ssa_function: &'a SsaFunction, func_name: UserFuncName) -> Option<Self> {
        let classified = ClassifiedFunction::new(ssa_function);
        if !classified.is_supported(CRANELIFT_FEATURES) {
            return None;
        }
        Some(Self {
            ssa_function,
            classified,
            frame_layout: native_frame_layout(ssa_function),
            func_name,
        })
    }

    pub fn root_slots(&self) -> usize {
        self.frame_layout.root_slots()
    }
}

#[inline]
pub fn repr_to_clif_type(repr: ValueRepr, ptr_ty: Type) -> Type {
    match repr {
        ValueRepr::TaggedWord => ptr_ty,
        ValueRepr::UnboxedI64 => types::I64,
        ValueRepr::UnboxedF32 => types::F32,
        ValueRepr::UnboxedF64 => types::F64,
        ValueRepr::UnboxedI32x4 => types::I32X4,
        ValueRepr::Bool => types::I8,
    }
}

fn float_bits(builder: &mut FunctionBuilder<'_>, format: FloatFormat, value: Value) -> Value {
    match format {
        FloatFormat::F32 => {
            let bits = builder.ins().bitcast(types::I32, MemFlags::new(), value);
            builder.ins().uextend(types::I64, bits)
        }
        FloatFormat::F64 => builder.ins().bitcast(types::I64, MemFlags::new(), value),
    }
}

fn float_from_bits(builder: &mut FunctionBuilder<'_>, format: FloatFormat, raw: Value) -> Value {
    match format {
        FloatFormat::F32 => {
            let raw = builder.ins().ireduce(types::I32, raw);
            builder.ins().bitcast(types::F32, MemFlags::new(), raw)
        }
        FloatFormat::F64 => builder.ins().bitcast(types::F64, MemFlags::new(), raw),
    }
}

#[inline]
fn word_const(builder: &mut FunctionBuilder<'_>, ty: Type, raw: u64) -> Value {
    builder.ins().iconst(ty, raw as i64)
}

fn is_fixnum(builder: &mut FunctionBuilder<'_>, word: Value) -> Value {
    let tag = builder.ins().band_imm(word, TAG_MASK as i64);
    builder.ins().icmp_imm(IntCC::Equal, tag, TAG_FIXNUM as i64)
}

#[derive(Clone, Copy)]
struct RootFrameHandle {
    slots_ptr: Value,
    frame_index: Value,
}

#[derive(Clone, Copy)]
struct BackedgePollState {
    cancel_flag: Value,
    remaining_steps: Value,
    slice_steps: Value,
    has_budget: Value,
}

#[derive(Clone, Copy)]
enum ValueBinding {
    Undefined,
    Direct(Value),
    Rooted(Variable),
}

struct LoweringContext<'a, E: RuntimeCallEmitter> {
    ssa_function: &'a SsaFunction,
    classified: &'a ClassifiedFunction<'a>,
    frame_layout: &'a NativeFrameLayout,
    emitter: &'a mut E,
    ptr_ty: Type,
    ptr_bytes: i32,
    portable_signed_overflow: bool,

    ctx: Value,
    target: Value,
    args_ptr: Value,
    argc: Value,
    unrooted_argc: Option<Variable>,
    out_ret: Value,
    values: Vec<ValueBinding>,

    root_frame: Option<RootFrameHandle>,
    ret_slot: StackSlot,
    return_layout: JitReturnLayout,
    fault_block: Block,
    unwind_block: Block,
    block_map: FxHashMap<BlockId, Block>,
    backedge_polls: Option<BackedgePollState>,
    deoptimization: Deoptimization<'a>,
    pack_bits_slot: Option<StackSlot>,
    word_slice_slot: Option<StackSlot>,
}

impl<'a, E: RuntimeCallEmitter> LoweringContext<'a, E> {
    fn ptr_bytes_usize(&self) -> usize {
        self.ptr_bytes as usize
    }

    fn ret_ptr(&self, builder: &mut FunctionBuilder<'_>) -> Value {
        builder.ins().stack_addr(self.ptr_ty, self.ret_slot, 0)
    }

    fn store_return_record(&self, builder: &mut FunctionBuilder<'_>, fields: [Value; 4]) {
        let JitReturnLayout {
            tag,
            single_value,
            target_frame,
            state,
            ..
        } = self.return_layout;
        for (value, offset) in fields
            .into_iter()
            .zip([tag, single_value, target_frame, state])
        {
            builder
                .ins()
                .store(MemFlags::trusted(), value, self.out_ret, offset);
        }
    }

    fn emit_return(&self, builder: &mut FunctionBuilder<'_>, tag: u8, single_value: Option<Value>) {
        let tag = builder.ins().iconst(types::I8, i64::from(tag));
        let zero = builder.ins().iconst(self.ptr_ty, 0);
        self.store_return_record(builder, [tag, single_value.unwrap_or(zero), zero, zero]);
        builder.ins().return_(&[]);
    }

    fn emit_copy_ret_slot_to_out(&self, builder: &mut FunctionBuilder<'_>) {
        let tag = self.load_return_tag(builder);
        let single_value = self.load_return_single_value(builder);
        let (target_frame, state) = self.load_unwind_target(builder);
        self.store_return_record(builder, [tag, single_value, target_frame, state]);
    }

    fn emit_drop_root_frame(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
    ) -> Result<(), LoweringError> {
        if let Some(root_frame) = self.root_frame {
            let ctx = self.ctx;
            self.emitter.emit_runtime_call(
                builder,
                RuntimeAbiFunction::DropRootFrame,
                &[ctx, root_frame.frame_index],
            )?;
        }
        Ok(())
    }

    fn read_value(&self, builder: &mut FunctionBuilder<'_>, id: ValueId) -> Value {
        match self.values[id.0 as usize] {
            ValueBinding::Rooted(var) => builder.use_var(var),
            ValueBinding::Direct(value) => value,
            ValueBinding::Undefined => unreachable!("use of undefined SSA value {id:?}"),
        }
    }

    fn def_val(&mut self, builder: &mut FunctionBuilder<'_>, id: Option<ValueId>, val: Value) {
        if let Some(id) = id {
            let binding = &mut self.values[id.0 as usize];
            if let ValueBinding::Rooted(var) = *binding {
                builder.def_var(var, val);
            } else {
                *binding = ValueBinding::Direct(val);
            }
        }
    }

    /// Current function word reloaded from `slots[0]` when a root frame is active.
    fn emit_current_func_word(&self, builder: &mut FunctionBuilder<'_>) -> Value {
        if let Some(root_frame) = self.root_frame {
            builder
                .ins()
                .load(self.ptr_ty, MemFlags::trusted(), root_frame.slots_ptr, 0)
        } else {
            self.target
        }
    }

    /// Current closure word reloaded from `slots[1]` when a root frame is active.
    fn emit_current_closure_word(&self, builder: &mut FunctionBuilder<'_>) -> Value {
        if let Some(root_frame) = self.root_frame {
            builder.ins().load(
                self.ptr_ty,
                MemFlags::trusted(),
                root_frame.slots_ptr,
                self.ptr_bytes,
            )
        } else {
            self.target
        }
    }

    fn emit_function_constant(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        index: usize,
    ) -> Result<Value, LoweringError> {
        let call = self.emit_call(builder, &RuntimeCall::constant(index), None)?;
        Ok(builder.inst_results(call)[0])
    }

    fn spill_root(&self, builder: &mut FunctionBuilder<'_>, id: ValueId, slot: RootSlot) {
        let frame = self.root_frame.expect("live root slots require a frame");
        let value = self.read_value(builder, id);
        let offset = (slot.0 * self.ptr_bytes_usize()) as i32;
        builder
            .ins()
            .store(MemFlags::trusted(), value, frame.slots_ptr, offset);
    }

    fn reload_root(&self, builder: &mut FunctionBuilder<'_>, id: ValueId, slot: RootSlot) {
        let frame = self.root_frame.expect("live root slots require a frame");
        let ValueBinding::Rooted(var) = self.values[id.0 as usize] else {
            unreachable!("every root layout entry has a Cranelift variable");
        };
        let offset = (slot.0 * self.ptr_bytes_usize()) as i32;
        let value = builder
            .ins()
            .load(self.ptr_ty, MemFlags::trusted(), frame.slots_ptr, offset);
        builder.def_var(var, value);
    }

    fn pack_word_slice(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        vals: &[ValueId],
    ) -> (Value, Value) {
        if vals.is_empty() {
            let zero = builder.ins().iconst(self.ptr_ty, 0);
            return (zero, zero);
        }
        let bytes =
            u32::try_from(vals.len() * self.ptr_bytes_usize()).expect("stack slot size fits u32");
        let slot = if let Some(slot) = self.word_slice_slot {
            let data = &mut builder.func.sized_stack_slots[slot];
            data.size = data.size.max(bytes);
            slot
        } else {
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                bytes,
                3,
            ));
            self.word_slice_slot = Some(slot);
            slot
        };
        for (i, &vid) in vals.iter().enumerate() {
            let v = self.read_value(builder, vid);
            let off = (i * self.ptr_bytes_usize()) as i32;
            builder.ins().stack_store(v, slot, off);
        }
        let ptr = builder.ins().stack_addr(self.ptr_ty, slot, 0);
        let len = builder.ins().iconst(self.ptr_ty, vals.len() as i64);
        (ptr, len)
    }

    fn emit_set_values(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        values: &[ValueId],
    ) -> Result<(), LoweringError> {
        self.emit_call(builder, &RuntimeCall::set_values(values), None)?;
        Ok(())
    }

    fn emit_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        call: &RuntimeCall<'_>,
        site: Option<(BlockId, usize, Option<usize>)>,
    ) -> Result<Inst, LoweringError> {
        use CallOperand::*;

        let mut args = [self.ctx; RuntimeCall::MAX_OPERANDS];
        let ptr_ty = self.ptr_ty;
        let word =
            |builder: &mut FunctionBuilder<'_>, n: usize| builder.ins().iconst(ptr_ty, n as i64);
        let mut words_len = None;
        let mut handler_slots = None;
        for (arg, &operand) in args.iter_mut().zip(call.operands()) {
            *arg = match operand {
                Context => self.ctx,
                Frame => {
                    self.root_frame
                        .expect("runtime call requires a root frame")
                        .frame_index
                }
                Code => self.emit_current_func_word(builder),
                Word(n) => word(builder, n),
                Bool(b) => builder.ins().iconst(types::I8, i64::from(b)),
                Value(value) => self.read_value(builder, value),
                SingleValue => self.load_return_single_value(builder),
                Constant(index) => self.emit_function_constant(builder, index)?,
                Cell(cell) => self.resolve_cell_target(builder, &cell)?,
                BoxLocation => word(builder, site.and_then(|(_, _, pc)| pc).unwrap_or(0)),
                Words(values) => {
                    let (ptr, len) = self.pack_word_slice(builder, values);
                    words_len = Some(len);
                    ptr
                }
                Length(_) => words_len.take().expect("word slice precedes its length"),
                CatchTag(value) => {
                    let (block, index, _) = site.expect("catch call belongs to a block");
                    let slot = self.frame_layout.catch_tag_slot(block, index);
                    let offset = i32::try_from(slot * self.ptr_bytes_usize())
                        .map_err(|_| LoweringError::IntegerOverflow("catch tag slot offset"))?;
                    let frame = self.root_frame.expect("catch requires a root frame");
                    let value = self.read_value(builder, value);
                    builder
                        .ins()
                        .store(MemFlags::trusted(), value, frame.slots_ptr, offset);
                    word(builder, slot)
                }
                UnwindState(target) => {
                    word(builder, self.frame_layout.unwind_table().state_for(target))
                }
                TagbodyBase => {
                    let (block, index, _) = site.expect("tagbody call belongs to a block");
                    word(
                        builder,
                        self.frame_layout.unwind_table().tagbody_base(block, index),
                    )
                }
                HandlerStates(targets) => {
                    let (states, slots) = self.pack_handler_tables(builder, targets)?;
                    handler_slots = Some(slots);
                    states
                }
                HandlerSlots(_) => handler_slots
                    .take()
                    .expect("handler state table precedes slots"),
                RootCount => word(builder, self.frame_layout.root_slots()),
                ResultOut => self.ret_ptr(builder),
            };
        }
        self.emitter
            .emit_runtime_call(builder, call.function, &args[..call.operands().len()])
    }

    fn emit_ssa_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        block: BlockId,
        index: usize,
        inst: &SsaInst,
        live: &[(ValueId, RootSlot)],
        call: &RuntimeCall<'_>,
    ) -> Result<(), LoweringError> {
        match call.when {
            CallWhen::SaveValues if self.root_frame.is_none() => return Ok(()),
            CallWhen::ReleaseArguments if self.release_local_arguments(builder)? => return Ok(()),
            CallWhen::OptionalArgument(index) => {
                if let Some(value) = self.local_optional_argument(builder, index)? {
                    self.def_val(builder, inst.id, value);
                    return Ok(());
                }
            }
            _ => {}
        }
        let site = Some((block, index, inst.pc));
        let emitted = self.emit_call(builder, call, site)?;
        let result = match call.result {
            CallReturn::Void => None,
            CallReturn::Word => Some(builder.inst_results(emitted)[0]),
            CallReturn::One | CallReturn::Effect => {
                self.finish_runtime_call(builder, inst.id.is_some())?
            }
            CallReturn::ReadCell => {
                self.finish_mapped_call(builder, true, inst.deopt.as_ref(), live)?
            }
            CallReturn::LispCall { primary } => {
                self.finish_call_preserving_values(builder, primary, inst.deopt.as_ref(), live)?
            }
            CallReturn::DebugPoint => {
                self.finish_debug_point(builder)?;
                None
            }
            CallReturn::Cleanup => self.finish_call_preserving_values(builder, false, None, &[])?,
        };
        if let Some(value) = result {
            self.def_val(builder, inst.id, value);
        }
        if let Some(pc) = inst.pc.filter(|_| call.mark_scope) {
            self.emit_call(builder, &RuntimeCall::mark_scope(pc), None)?;
        }
        Ok(())
    }

    fn emit_word_op(
        &self,
        builder: &mut FunctionBuilder<'_>,
        op: zkcl_opt::native::WordOp,
    ) -> Result<Value, LoweringError> {
        use zkcl_opt::native::{I64Arithmetic, ScalarConstant, WordOp};

        let value = match op {
            WordOp::Constant(ScalarConstant::Word(imm)) => {
                word_const(builder, self.ptr_ty, self.immediate(imm)?)
            }
            WordOp::Constant(ScalarConstant::I64(n)) => builder.ins().iconst(types::I64, n),
            WordOp::Constant(ScalarConstant::Bool(b)) => {
                builder.ins().iconst(types::I8, i64::from(b))
            }
            WordOp::Copy(value) => self.read_value(builder, value),
            WordOp::Select(cond, when_true, when_false) => {
                let cond = self.read_value(builder, cond);
                let yes = word_const(builder, self.ptr_ty, self.immediate(when_true)?);
                let no = word_const(builder, self.ptr_ty, self.immediate(when_false)?);
                builder.ins().select(cond, yes, no)
            }
            WordOp::BoxFixnum(value) => {
                let value = self.read_value(builder, value);
                let shifted = builder.ins().ishl_imm(value, i64::from(FIXNUM_SHIFT));
                builder.ins().bor_imm(shifted, TAG_FIXNUM as i64)
            }
            WordOp::Test(value, test) => {
                let mut value = self.read_value(builder, value);
                if let Some(mask) = test.mask {
                    value = builder.ins().band_imm(value, mask as i64);
                }
                let cmp = if test.equal {
                    IntCC::Equal
                } else {
                    IntCC::NotEqual
                };
                builder.ins().icmp_imm(cmp, value, test.bits as i64)
            }
            WordOp::FixnumRange { value, low, span } => {
                let word = self.read_value(builder, value);
                let fixnum = is_fixnum(builder, word);
                let value = builder.ins().sshr_imm(word, i64::from(FIXNUM_SHIFT));
                let bias = builder.ins().iadd_imm(value, -low);
                let in_range = builder
                    .ins()
                    .icmp_imm(IntCC::UnsignedLessThanOrEqual, bias, span);
                builder.ins().band(fixnum, in_range)
            }
            WordOp::I64(op, lhs, rhs) => {
                let a = self.read_value(builder, lhs);
                let b = self.read_value(builder, rhs);
                match op {
                    I64Arithmetic::Add => builder.ins().iadd(a, b),
                    I64Arithmetic::Sub => builder.ins().isub(a, b),
                    I64Arithmetic::Mul => builder.ins().imul(a, b),
                }
            }
            WordOp::CompareI64(op, lhs, rhs) => {
                let a = self.read_value(builder, lhs);
                let b = self.read_value(builder, rhs);
                let cc = match op {
                    CmpOp::Eq => IntCC::Equal,
                    CmpOp::Ne => IntCC::NotEqual,
                    CmpOp::Lt => IntCC::SignedLessThan,
                    CmpOp::Le => IntCC::SignedLessThanOrEqual,
                    CmpOp::Gt => IntCC::SignedGreaterThan,
                    CmpOp::Ge => IntCC::SignedGreaterThanOrEqual,
                };
                builder.ins().icmp(cc, a, b)
            }
            WordOp::Not(value) => {
                let value = self.read_value(builder, value);
                builder.ins().bxor_imm(value, 1)
            }
        };
        Ok(value)
    }

    fn emit_inst(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        block_index: usize,
        bid: BlockId,
        inst_idx: usize,
        inst: &'a SsaInst,
        live: &[(ValueId, RootSlot)],
    ) -> Result<(), LoweringError> {
        use zkcl_opt::native::NativeOp;

        let ptr_ty = self.ptr_ty;
        match self.classified.operation(block_index, inst_idx) {
            NativeOp::Call(call) => {
                return self.emit_ssa_call(builder, bid, inst_idx, inst, live, &call);
            }
            NativeOp::Word(op) => {
                let value = self.emit_word_op(builder, op)?;
                self.def_val(builder, inst.id, value);
                return Ok(());
            }
            NativeOp::Checkpoint { values, charge } => {
                return self.emit_checkpoint(builder, inst, values, charge);
            }
            NativeOp::StoreUnwindRoot { reg, value } => {
                if let Some(slot) = self.frame_layout.unwind_register_slot(reg) {
                    let offset = i32::try_from(slot * self.ptr_bytes_usize())
                        .map_err(|_| LoweringError::IntegerOverflow("unwind register offset"))?;
                    let value = self.read_value(builder, value);
                    let frame = self
                        .root_frame
                        .expect("unwind register requires a root frame");
                    builder
                        .ins()
                        .store(MemFlags::trusted(), value, frame.slots_ptr, offset);
                }
                return Ok(());
            }
            NativeOp::LoadHandlerConditionRoot => {
                let frame = self
                    .root_frame
                    .expect("handler condition requires a root frame");
                let slot = self.frame_layout.handler_condition_slot(bid);
                let offset = i32::try_from(slot * self.ptr_bytes_usize())
                    .map_err(|_| LoweringError::IntegerOverflow("handler condition offset"))?;
                let value =
                    builder
                        .ins()
                        .load(ptr_ty, MemFlags::trusted(), frame.slots_ptr, offset);
                self.def_val(builder, inst.id, value);
                return Ok(());
            }
            NativeOp::Target { .. } | NativeOp::Unsupported => {}
        }
        match &inst.op {
            SsaOp::UnboxFixnumGuard(src) => {
                let deopt = inst
                    .deopt
                    .as_ref()
                    .ok_or(LoweringError::MissingDeoptState)?;
                let word = self.read_value(builder, *src);
                let tag = builder.ins().band_imm(word, TAG_MASK as i64);
                let failed = builder
                    .ins()
                    .icmp_imm(IntCC::NotEqual, tag, TAG_FIXNUM as i64);
                self.guard(builder, failed, deopt)?;
                let unboxed = builder.ins().sshr_imm(word, i64::from(FIXNUM_SHIFT));
                self.def_val(builder, inst.id, unboxed);
            }
            SsaOp::GuardPrimitiveBinding { symbol, primitive } => {
                let deopt = inst
                    .deopt
                    .as_ref()
                    .ok_or(LoweringError::MissingDeoptState)?;
                let valid = self.is_primitive_binding(builder, *symbol, primitive.abi_index())?;
                let failed = builder.ins().icmp_imm(IntCC::Equal, valid, 0);
                self.guard(builder, failed, deopt)?;
            }
            SsaOp::UnboxFloatGuard { format, value } => {
                let deopt = inst
                    .deopt
                    .as_ref()
                    .ok_or(LoweringError::MissingDeoptState)?;
                let word = self.read_value(builder, *value);
                let runtime_function = match format {
                    FloatFormat::F32 => RuntimeAbiFunction::UnboxF32,
                    FloatFormat::F64 => RuntimeAbiFunction::UnboxF64,
                };
                let call =
                    self.emitter
                        .emit_runtime_call(builder, runtime_function, &[self.ctx, word])?;
                let raw = builder.inst_results(call)[0];
                let failed = builder.ins().icmp_imm(IntCC::Equal, raw, -1);
                self.guard(builder, failed, deopt)?;
                let unboxed = float_from_bits(builder, *format, raw);
                self.def_val(builder, inst.id, unboxed);
            }
            SsaOp::FloatArithChecked {
                format,
                op,
                lhs,
                rhs,
            } => {
                let deopt = inst
                    .deopt
                    .as_ref()
                    .ok_or(LoweringError::MissingDeoptState)?;
                let lhs = self.read_value(builder, *lhs);
                let rhs = self.read_value(builder, *rhs);
                if *op == FloatArithOp::Div {
                    let zero = match format {
                        FloatFormat::F32 => builder.ins().f32const(0.0_f32),
                        FloatFormat::F64 => builder.ins().f64const(0.0_f64),
                    };
                    let zero_divisor = builder.ins().fcmp(FloatCC::Equal, rhs, zero);
                    self.guard(builder, zero_divisor, deopt)?;
                }
                // Each operation rounds at its own width, including before a subsequent add.
                let result = match op {
                    FloatArithOp::Add => builder.ins().fadd(lhs, rhs),
                    FloatArithOp::Sub => builder.ins().fsub(lhs, rhs),
                    FloatArithOp::Mul => builder.ins().fmul(lhs, rhs),
                    FloatArithOp::Div => builder.ins().fdiv(lhs, rhs),
                };
                let bits = float_bits(builder, *format, result);
                let exponent_mask = match format {
                    FloatFormat::F32 => 0x7f80_0000,
                    FloatFormat::F64 => 0x7ff0_0000_0000_0000,
                };
                let exponent = builder.ins().band_imm(bits, exponent_mask);
                let nonfinite = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, exponent, exponent_mask);
                self.guard(builder, nonfinite, deopt)?;
                self.def_val(builder, inst.id, result);
            }
            SsaOp::BoxFloat { format, value } => {
                let value = self.read_value(builder, *value);
                let bits = float_bits(builder, *format, value);
                let at = builder.ins().iconst(ptr_ty, inst.pc.unwrap_or(0) as i64);
                let ret_ptr = self.ret_ptr(builder);
                let runtime_function = match format {
                    FloatFormat::F32 => RuntimeAbiFunction::BoxF32,
                    FloatFormat::F64 => RuntimeAbiFunction::BoxF64,
                };
                self.emitter.emit_runtime_call(
                    builder,
                    runtime_function,
                    &[self.ctx, bits, at, ret_ptr],
                )?;
                self.bind_runtime_call_result(builder, inst.id)?;
            }
            SsaOp::UnboxI32x4Guard(value) => {
                let deopt = inst
                    .deopt
                    .as_ref()
                    .ok_or(LoweringError::MissingDeoptState)?;
                let word = self.read_value(builder, *value);
                let slot = self.pack_bits_slot(builder);
                let bits = builder.ins().stack_addr(ptr_ty, slot, 0);
                let call = self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::UnboxI32x4,
                    &[self.ctx, word, bits],
                )?;
                let success = builder.inst_results(call)[0];
                let failed = builder.ins().icmp_imm(IntCC::Equal, success, 0);
                self.guard(builder, failed, deopt)?;
                let value = builder.ins().stack_load(types::I32X4, slot, 0);
                self.def_val(builder, inst.id, value);
            }
            SsaOp::I32x4Binary { op, lhs, rhs } => {
                let lhs = self.read_value(builder, *lhs);
                let rhs = self.read_value(builder, *rhs);
                let result = match op {
                    I32x4Operation::Add => builder.ins().iadd(lhs, rhs),
                    I32x4Operation::Sub => builder.ins().isub(lhs, rhs),
                    I32x4Operation::BitAnd => builder.ins().band(lhs, rhs),
                    I32x4Operation::BitOr => builder.ins().bor(lhs, rhs),
                    I32x4Operation::BitXor => builder.ins().bxor(lhs, rhs),
                };
                self.def_val(builder, inst.id, result);
            }
            SsaOp::BoxI32x4(value) => {
                let value = self.read_value(builder, *value);
                let slot = self.pack_bits_slot(builder);
                builder.ins().stack_store(value, slot, 0);
                let bits = builder.ins().stack_addr(ptr_ty, slot, 0);
                let at = builder.ins().iconst(ptr_ty, inst.pc.unwrap_or(0) as i64);
                let ret_ptr = self.ret_ptr(builder);
                self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::BoxI32x4,
                    &[self.ctx, bits, at, ret_ptr],
                )?;
                self.bind_runtime_call_result(builder, inst.id)?;
            }
            SsaOp::TryFixnumArithmetic {
                op,
                symbol,
                lhs,
                rhs,
            } => {
                let bound = self.is_primitive_binding(builder, *symbol, op.binding_index())?;
                let lhs = self.read_value(builder, *lhs);
                let rhs = self.read_value(builder, *rhs);
                let value = try_fixnum_arithmetic(
                    builder,
                    *op,
                    lhs,
                    rhs,
                    bound,
                    ptr_ty,
                    self.portable_signed_overflow,
                );
                self.def_val(builder, inst.id, value);
            }
            SsaOp::AddI64Checked(lhs, rhs) => {
                self.emit_checked_arith(builder, inst, *lhs, *rhs, CheckedFixnumArithmetic::Add)?;
            }
            SsaOp::SubI64Checked(lhs, rhs) => {
                self.emit_checked_arith(builder, inst, *lhs, *rhs, CheckedFixnumArithmetic::Sub)?;
            }
            SsaOp::MulI64Checked(lhs, rhs) => {
                self.emit_checked_arith(builder, inst, *lhs, *rhs, CheckedFixnumArithmetic::Mul)?;
            }
            SsaOp::ReadFloatVector {
                symbol,
                format,
                array,
                index,
            } => {
                let deopt = inst
                    .deopt
                    .as_ref()
                    .ok_or(LoweringError::MissingDeoptState)?;
                let function = self.emit_current_func_word(builder);
                let symbol = builder.ins().iconst(ptr_ty, *symbol as i64);
                let array = self.read_value(builder, *array);
                let index = self.read_value(builder, *index);
                let double = builder
                    .ins()
                    .iconst(types::I8, i64::from(*format == FloatFormat::F64));
                let call = self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::ReadFloatVector,
                    &[self.ctx, function, symbol, array, index, double],
                )?;
                let raw = builder.inst_results(call)[0];
                let failed = builder.ins().icmp_imm(IntCC::Equal, raw, -1);
                self.guard(builder, failed, deopt)?;
                let unboxed = float_from_bits(builder, *format, raw);
                self.def_val(builder, inst.id, unboxed);
            }
            SsaOp::WriteFloatVector {
                symbol,
                format,
                setter,
                array,
                index,
                value,
            } => {
                let deopt = inst
                    .deopt
                    .as_ref()
                    .ok_or(LoweringError::MissingDeoptState)?;
                let function = self.emit_current_func_word(builder);
                let symbol = builder.ins().iconst(ptr_ty, *symbol as i64);
                let setter = self.read_value(builder, *setter);
                let array = self.read_value(builder, *array);
                let index = self.read_value(builder, *index);
                let value = self.read_value(builder, *value);
                let bits = float_bits(builder, *format, value);
                let double = builder
                    .ins()
                    .iconst(types::I8, i64::from(*format == FloatFormat::F64));
                let call = self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::WriteFloatVector,
                    &[
                        self.ctx, function, symbol, setter, array, index, bits, double,
                    ],
                )?;
                let success = builder.inst_results(call)[0];
                let failed = builder.ins().icmp_imm(IntCC::Equal, success, 0);
                self.guard(builder, failed, deopt)?;
            }
            SsaOp::ReadCapture(cap_idx) => {
                let closure_word = self.emit_current_closure_word(builder);
                let ctx = self.ctx;
                let idx_val = builder.ins().iconst(ptr_ty, *cap_idx as i64);
                let at_val = builder.ins().iconst(ptr_ty, inst.pc.unwrap_or(0) as i64);
                let ret_ptr = self.ret_ptr(builder);
                self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::ReadCapture,
                    &[ctx, closure_word, idx_val, at_val, ret_ptr],
                )?;
                self.bind_runtime_call_result(builder, inst.id)?;
            }
            other => {
                return Err(LoweringError::UnsupportedOp(format!("{other:?}")));
            }
        }
        Ok(())
    }

    fn pack_handler_tables(
        &self,
        builder: &mut FunctionBuilder<'_>,
        targets: &[BlockId],
    ) -> Result<(Value, Value), LoweringError> {
        let ptr_ty = self.ptr_ty;
        Ok(if targets.is_empty() {
            let null = builder.ins().iconst(ptr_ty, 0);
            (null, null)
        } else {
            let bytes = u32::try_from(targets.len() * self.ptr_bytes_usize())
                .map_err(|_| LoweringError::IntegerOverflow("handler table bytes"))?;
            let table_slot = StackSlotData::new(StackSlotKind::ExplicitSlot, bytes, 3);
            let states = builder.create_sized_stack_slot(table_slot.clone());
            let slots = builder.create_sized_stack_slot(table_slot);
            for (index, target) in targets.iter().enumerate() {
                let offset = (index * self.ptr_bytes_usize()) as i32;
                let state = builder.ins().iconst(
                    ptr_ty,
                    self.frame_layout.unwind_table().state_for(*target) as i64,
                );
                builder.ins().stack_store(state, states, offset);
                let slot = builder.ins().iconst(
                    ptr_ty,
                    self.frame_layout.handler_condition_slot(*target) as i64,
                );
                builder.ins().stack_store(slot, slots, offset);
            }
            (
                builder.ins().stack_addr(ptr_ty, states, 0),
                builder.ins().stack_addr(ptr_ty, slots, 0),
            )
        })
    }

    fn resolve_cell_target(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        target: &CellTarget,
    ) -> Result<Value, LoweringError> {
        match target {
            CellTarget::Local(v) => Ok(self.read_value(builder, *v)),
            CellTarget::Capture(cap_idx) => {
                let closure_word = self.emit_current_closure_word(builder);
                let ctx = self.ctx;
                let idx_val = builder.ins().iconst(self.ptr_ty, *cap_idx as i64);
                let zero = builder.ins().iconst(self.ptr_ty, 0);
                let ret_ptr = self.ret_ptr(builder);
                self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::ReadCapture,
                    &[ctx, closure_word, idx_val, zero, ret_ptr],
                )?;
                let tag = self.load_return_tag(builder);
                let ok_blk = builder.create_block();
                self.branch_on_return_fault(builder, tag, ok_blk);
                builder.switch_to_block(ok_blk);
                Ok(self.load_return_single_value(builder))
            }
        }
    }

    fn immediate(&self, imm: zkcl_bytecode::Immediate) -> Result<u64, LoweringError> {
        imm.encode_word(self.ptr_ty.bits() as u8)
            .ok_or_else(|| match imm {
                zkcl_bytecode::Immediate::Fixnum(n) => LoweringError::ImmediateOutOfRange(n),
                _ => unreachable!("native targets use 32-bit or 64-bit words"),
            })
    }

    fn pack_bits_slot(&mut self, builder: &mut FunctionBuilder<'_>) -> StackSlot {
        *self.pack_bits_slot.get_or_insert_with(|| {
            builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                types::I32X4.bytes(),
                4,
            ))
        })
    }

    fn is_primitive_binding(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        symbol: usize,
        operation: u8,
    ) -> Result<Value, LoweringError> {
        let target = self.emit_current_func_word(builder);
        let symbol = builder.ins().iconst(self.ptr_ty, symbol as i64);
        let operation = builder.ins().iconst(self.ptr_ty, i64::from(operation));
        let call = self.emitter.emit_runtime_call(
            builder,
            RuntimeAbiFunction::IsPrimitiveBinding,
            &[self.ctx, target, symbol, operation],
        )?;
        Ok(builder.inst_results(call)[0])
    }

    fn emit_checked_arith(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        inst: &'a SsaInst,
        lhs: ValueId,
        rhs: ValueId,
        op: CheckedFixnumArithmetic,
    ) -> Result<(), LoweringError> {
        let deopt = inst
            .deopt
            .as_ref()
            .ok_or(LoweringError::MissingDeoptState)?;
        let a = self.read_value(builder, lhs);
        let b = self.read_value(builder, rhs);

        let (res, ov) = op.emit(builder, a, b, self.portable_signed_overflow);
        // Translating the signed interval to start at zero makes one unsigned bounds test sufficient.
        let (min, max) = self.ssa_function.fixnum_bounds;
        let biased = builder.ins().iadd_imm(res, -min);
        let out_of_range = builder
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThan, biased, max - min);
        let bad = builder.ins().bor(ov, out_of_range);
        self.guard(builder, bad, deopt)?;
        self.def_val(builder, inst.id, res);
        Ok(())
    }

    fn branch_arguments(
        &self,
        builder: &mut FunctionBuilder<'_>,
        args: &[ValueId],
    ) -> Vec<BlockArg> {
        args.iter()
            .map(|&v| BlockArg::Value(self.read_value(builder, v)))
            .collect()
    }

    fn sync_all_rooted_slots(&self, builder: &mut FunctionBuilder<'_>) {
        for (&vid, &slot) in &self.frame_layout.roots().value_slots {
            self.spill_root(builder, vid, slot);
        }
    }

    fn branch_on_slice(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        slice_steps: Value,
        ready: Block,
        handoff: Block,
        protocol: SliceCharge,
    ) -> Result<(), LoweringError> {
        let charge = builder.create_block();
        let decrement = builder.create_block();
        let refill = builder.create_block();
        builder.set_cold_block(refill);
        let enabled = builder.ins().icmp_imm(IntCC::NotEqual, slice_steps, 0);
        builder.ins().brif(enabled, charge, &[], ready, &[]);
        builder.switch_to_block(charge);
        let steps = builder
            .ins()
            .load(types::I16, MemFlags::trusted(), slice_steps, 0);
        let exhausted = builder.ins().icmp_imm(IntCC::Equal, steps, 0);
        builder.ins().brif(exhausted, refill, &[], decrement, &[]);
        builder.switch_to_block(decrement);
        let next = builder.ins().iadd_imm(steps, -i64::from(protocol.cost()));
        builder
            .ins()
            .store(MemFlags::trusted(), next, slice_steps, 0);
        builder.ins().jump(ready, &[]);
        builder.switch_to_block(refill);
        let check = self.emit_call(builder, &protocol.refill(), None)?;
        let refilled = builder.inst_results(check)[0];
        builder.ins().brif(refilled, ready, &[], handoff, &[]);
        Ok(())
    }

    fn emit_backedge_safepoint_poll(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        at: usize,
        polls: BackedgePollState,
        handoff: Option<(Block, SliceCharge)>,
    ) -> Result<(), LoweringError> {
        let BackedgePollState {
            cancel_flag,
            remaining_steps,
            slice_steps,
            has_budget,
        } = polls;
        let has_cancel = builder.ins().icmp_imm(IntCC::NotEqual, cancel_flag, 0);

        let check_cancel_blk = builder.create_block();
        let check_budget_blk = builder.create_block();
        let safepoint_hit_blk = builder.create_block();
        builder.set_cold_block(safepoint_hit_blk);
        let continue_blk = builder.create_block();
        let slice_check = handoff.map(|_| builder.create_block());
        let after_budget = slice_check.unwrap_or(continue_blk);

        builder
            .ins()
            .brif(has_cancel, check_cancel_blk, &[], check_budget_blk, &[]);

        builder.switch_to_block(check_cancel_blk);
        // A host thread can update this state while the loop is running.
        let flag = builder
            .ins()
            .atomic_load(types::I8, MemFlags::trusted(), cancel_flag);
        let is_cancelled = builder.ins().icmp_imm(IntCC::NotEqual, flag, 0);
        builder
            .ins()
            .brif(is_cancelled, safepoint_hit_blk, &[], check_budget_blk, &[]);

        builder.switch_to_block(check_budget_blk);
        // The normal synchronous path checks one flag for both budgets. Test
        // individual budgets only when at least one of them is active.
        if handoff.is_some() {
            let active = builder.create_block();
            builder
                .ins()
                .brif(has_budget, active, &[], continue_blk, &[]);
            builder.switch_to_block(active);
        }
        Self::charge_execution_step(builder, remaining_steps, safepoint_hit_blk, after_budget);

        builder.switch_to_block(safepoint_hit_blk);
        self.emit_call(builder, &SliceCharge::poll(at), None)?;
        self.finish_runtime_call(builder, false)?;
        builder.ins().jump(after_budget, &[]);

        if let Some((check, (handoff, protocol))) = slice_check.zip(handoff) {
            builder.switch_to_block(check);
            self.branch_on_slice(builder, slice_steps, continue_blk, handoff, protocol)?;
        }
        builder.switch_to_block(continue_blk);
        Ok(())
    }

    fn charge_execution_step(
        builder: &mut FunctionBuilder<'_>,
        remaining_steps: Value,
        exhausted: Block,
        ready: Block,
    ) {
        let has_steps = builder.ins().icmp_imm(IntCC::NotEqual, remaining_steps, 0);
        let dec_steps_blk = builder.create_block();
        builder
            .ins()
            .brif(has_steps, dec_steps_blk, &[], ready, &[]);

        builder.switch_to_block(dec_steps_blk);
        let rem = builder
            .ins()
            .load(types::I64, MemFlags::trusted(), remaining_steps, 0);
        let out_of_steps = builder.ins().icmp_imm(IntCC::Equal, rem, 0);
        let store_dec_blk = builder.create_block();
        builder
            .ins()
            .brif(out_of_steps, exhausted, &[], store_dec_blk, &[]);

        builder.switch_to_block(store_dec_blk);
        let next_rem = builder.ins().iadd_imm(rem, -1);
        builder
            .ins()
            .store(MemFlags::trusted(), next_rem, remaining_steps, 0);
        builder.ins().jump(ready, &[]);
    }

    fn emit_terminator(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        term: &Terminator,
    ) -> Result<(), LoweringError> {
        match term {
            Terminator::Jump(target) => {
                let args = self.branch_arguments(builder, &target.args);
                builder.ins().jump(self.block_map[&target.block], &args);
            }
            Terminator::BranchTruthy {
                cond,
                truthy: yes,
                falsy: no,
            }
            | Terminator::BranchBool {
                cond,
                then_target: yes,
                else_target: no,
            } => {
                let value = self.read_value(builder, *cond);
                let condition = if matches!(term, Terminator::BranchTruthy { .. }) {
                    builder
                        .ins()
                        .icmp_imm(IntCC::NotEqual, value, WORD_NIL as i64)
                } else {
                    value
                };
                let yes_args = self.branch_arguments(builder, &yes.args);
                let no_args = self.branch_arguments(builder, &no.args);
                builder.ins().brif(
                    condition,
                    self.block_map[&yes.block],
                    &yes_args,
                    self.block_map[&no.block],
                    &no_args,
                );
            }
            Terminator::ReturnOne(value) => self.emit_return_one(builder, *value)?,
            Terminator::ReturnValues(values) => {
                if let [value] = values.as_slice() {
                    self.emit_return_one(builder, *value)?;
                } else {
                    self.emit_set_values(builder, values)?;
                    self.emit_drop_root_frame(builder)?;
                    self.emit_return(builder, JIT_RETURN_TAG_VALUES, None);
                }
            }
            Terminator::ReturnDynamicValues => {
                self.emit_drop_root_frame(builder)?;
                self.emit_return(builder, JIT_RETURN_TAG_VALUES, None);
            }
            Terminator::Unwind(op) => self.emit_unwind(builder, op)?,
        }
        Ok(())
    }

    fn emit_tail_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        call: TailCall<'_>,
    ) -> Result<(), LoweringError> {
        if let Some(args) = call.fixed_arguments(self.ssa_function.arity) {
            self.emit_self_tail_backedge(builder, call, args)?;
        }
        let callee = match call.callee {
            TailCallee::Named(symbol) => self.emit_function_constant(builder, symbol)?,
            TailCallee::Value(value) => self.read_value(builder, value),
        };
        let (args, argc) = self.pack_word_slice(builder, call.args.unwrap_or_default());
        let at = builder.ins().iconst(self.ptr_ty, call.at as i64);
        let use_values = builder
            .ins()
            .iconst(types::I8, i64::from(call.args.is_none()));
        self.emitter.emit_runtime_call(
            builder,
            RuntimeAbiFunction::StageTailCall,
            &[self.ctx, callee, args, argc, at, use_values],
        )?;
        self.emit_drop_root_frame(builder)?;
        self.emit_return(builder, JIT_RETURN_TAG_TAIL_CALL, None);
        Ok(())
    }

    fn emit_self_tail_backedge(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        call: TailCall<'_>,
        args: &[ValueId],
    ) -> Result<(), LoweringError> {
        let current = self.emit_current_closure_word(builder);
        let same_function = match call.callee {
            TailCallee::Named(symbol) => {
                let symbol = builder.ins().iconst(self.ptr_ty, symbol as i64);
                let check = self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::IsSelfCall,
                    &[self.ctx, current, symbol],
                )?;
                builder.inst_results(check)[0]
            }
            TailCallee::Value(callee) => {
                let callee = self.read_value(builder, callee);
                builder.ins().icmp(IntCC::Equal, callee, current)
            }
        };
        let restart = builder.create_block();
        let transfer = builder.create_block();
        builder
            .ins()
            .brif(same_function, restart, &[], transfer, &[]);

        builder.switch_to_block(restart);
        let polls = self
            .backedge_polls
            .expect("self tail call has a backedge poll");
        self.emit_backedge_safepoint_poll(
            builder,
            call.at,
            polls,
            Some((transfer, SliceCharge::CHECKPOINT)),
        )?;
        if let Some(frame) = self.root_frame {
            let nil = word_const(builder, self.ptr_ty, WORD_NIL);
            // Preserve the function and closure slots. The next iteration
            // publishes its own parameters before an allocating call.
            for slot in 2..self.frame_layout.root_slots() {
                let offset = (slot * self.ptr_bytes_usize()) as i32;
                builder
                    .ins()
                    .store(MemFlags::trusted(), nil, frame.slots_ptr, offset);
            }
        }
        let entry = self.ssa_function.entry;
        let args = self.branch_arguments(builder, args);
        builder.ins().jump(self.block_map[&entry], &args);
        builder.switch_to_block(transfer);
        Ok(())
    }

    fn emit_return_one(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        value: ValueId,
    ) -> Result<(), LoweringError> {
        let value = self.read_value(builder, value);
        self.emit_drop_root_frame(builder)?;
        self.emit_return(builder, JIT_RETURN_TAG_ONE, Some(value));
        Ok(())
    }

    fn emit_unwind(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        op: &UnwindOp,
    ) -> Result<(), LoweringError> {
        if !self.frame_layout.unwind_table().targets().is_empty() {
            self.sync_all_rooted_slots(builder);
        }
        let ctx = self.ctx;
        let ptr_ty = self.ptr_ty;
        let ret_ptr = self.ret_ptr(builder);
        match op {
            UnwindOp::ReturnFrom { token, at } => {
                let tok = self.read_value(builder, *token);
                let at_val = builder.ins().iconst(ptr_ty, *at as i64);
                self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::ReturnFrom,
                    &[ctx, tok, at_val, ret_ptr],
                )?;
            }
            UnwindOp::Throw { tag, at } => {
                let t = self.read_value(builder, *tag);
                let at_val = builder.ins().iconst(ptr_ty, *at as i64);
                self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::Throw,
                    &[ctx, t, at_val, ret_ptr],
                )?;
            }
            UnwindOp::Go { token, label, at } => {
                let tok = self.read_value(builder, *token);
                let lbl = builder.ins().iconst(ptr_ty, *label as i64);
                let at_val = builder.ins().iconst(ptr_ty, *at as i64);
                self.emitter.emit_runtime_call(
                    builder,
                    RuntimeAbiFunction::Go,
                    &[ctx, tok, lbl, at_val, ret_ptr],
                )?;
            }
        }
        let tag = self.load_return_tag(builder);
        self.branch_on_return_fault(builder, tag, self.unwind_block);
        Ok(())
    }
}
