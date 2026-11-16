//! Cold exits that reconstruct a bytecode frame from live SSA values.

use cranelift_codegen::ir::{
    Block, InstBuilder, StackSlot, StackSlotData, StackSlotKind, Value, types,
};
use cranelift_frontend::FunctionBuilder;
use rustc_hash::FxHashMap;
use zkcl_bytecode::native_abi::DEOPT_I32X4_FORMAT;
use zkcl_opt::native::{FIXNUM_SHIFT, RuntimeAbiFunction, SliceCharge, TAG_FIXNUM};
use zkcl_opt::{DeoptRegisterValue, DeoptState, RootSlot, SsaInst, ValueId};

use super::{LoweringContext, LoweringError, RuntimeCallEmitter, float_bits, word_const};

#[derive(Default)]
pub(super) struct Deoptimization<'a> {
    exits: FxHashMap<usize, (&'a DeoptState, Block)>,
    registers: Option<StackSlot>,
    values: Option<StackSlot>,
    formats: Option<StackSlot>,
    packs: Option<StackSlot>,
}

impl<'a, E: RuntimeCallEmitter> LoweringContext<'a, E> {
    pub(super) fn guard(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        failed: Value,
        state: &'a DeoptState,
    ) -> Result<(), LoweringError> {
        let normal = builder.create_block();
        // Guards from one bytecode instruction usually reconstruct the same registers.
        let existing = self
            .deoptimization
            .exits
            .get(&state.pc)
            .filter(|(saved, _)| *saved == state)
            .map(|(_, block)| *block);
        let exit = existing.unwrap_or_else(|| builder.create_block());
        builder.ins().brif(failed, exit, &[], normal, &[]);
        if existing.is_none() {
            self.deoptimization.exits.insert(state.pc, (state, exit));
            self.emit_deopt_exit(builder, exit, state, &[], None)?;
        }
        builder.switch_to_block(normal);
        Ok(())
    }

    pub(super) fn call_handoff(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        tag: Value,
        state: Option<&DeoptState>,
        live: &[(ValueId, RootSlot)],
    ) -> Result<(), LoweringError> {
        let Some(state) = state else { return Ok(()) };
        let handoff = builder.ins().icmp_imm(
            cranelift_codegen::ir::condcodes::IntCC::UnsignedGreaterThanOrEqual,
            tag,
            i64::from(zkcl_bytecode::native_abi::JIT_RETURN_TAG_VM_CALL),
        );
        let exit = builder.create_block();
        let next = builder.create_block();
        builder.ins().brif(handoff, exit, &[], next, &[]);
        self.emit_deopt_exit(builder, exit, state, live, None)?;
        builder.switch_to_block(next);
        Ok(())
    }

    pub(super) fn emit_checkpoint(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        inst: &SsaInst,
        values: Option<&[ValueId]>,
        protocol: SliceCharge,
    ) -> Result<(), LoweringError> {
        let state = inst
            .deopt
            .as_ref()
            .ok_or(LoweringError::MissingDeoptState)?;
        let polls = self
            .backedge_polls
            .expect("execution checkpoint uses a quantum");
        let exit = builder.create_block();
        self.emit_backedge_safepoint_poll(builder, state.pc, polls, Some((exit, protocol)))?;
        let next = builder
            .current_block()
            .expect("checkpoint has a continuation block");
        self.emit_deopt_exit(builder, exit, state, &[], values)?;
        builder.switch_to_block(next);
        Ok(())
    }

    fn emit_deopt_exit(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        exit_block: Block,
        deopt: &DeoptState,
        live: &[(ValueId, RootSlot)],
        values: Option<&[ValueId]>,
    ) -> Result<(), LoweringError> {
        builder.set_cold_block(exit_block);
        builder.switch_to_block(exit_block);
        if let Some(values) = values {
            self.emit_set_values(builder, values)?;
        }
        for &(value, slot) in live {
            self.reload_root(builder, value, slot);
        }

        let unwind_registers: Vec<_> = self.frame_layout.unwind_restore(deopt).collect();
        let count = deopt.registers_to_restore.len() + unwind_registers.len();
        let mut needs_formats = false;
        let mut has_packs = false;
        for (_, kind) in &deopt.registers_to_restore {
            match kind {
                DeoptRegisterValue::UnboxedFloat { .. } => needs_formats = true,
                DeoptRegisterValue::UnboxedI32x4 { .. } => {
                    needs_formats = true;
                    has_packs = true;
                    break;
                }
                _ => {}
            }
        }
        let pack_bytes = types::I32X4.bytes();
        let packs_slot = if has_packs {
            Some(reserve_scratch(
                builder,
                &mut self.deoptimization.packs,
                count,
                pack_bytes,
                4,
            )?)
        } else {
            None
        };
        let formats_slot = if needs_formats {
            Some(reserve_scratch(
                builder,
                &mut self.deoptimization.formats,
                count,
                1,
                0,
            )?)
        } else {
            None
        };
        let value_ty = if formats_slot.is_some() {
            types::I64
        } else {
            self.ptr_ty
        };
        let value_bytes = value_ty.bytes() as usize;
        let (regs_ptr, vals_ptr) = if count == 0 {
            let z = builder.ins().iconst(self.ptr_ty, 0);
            (z, z)
        } else {
            let state = &mut self.deoptimization;
            let regs_ss = reserve_scratch(builder, &mut state.registers, count, 4, 2)?;
            let vals_ss =
                reserve_scratch(builder, &mut state.values, count, types::I64.bytes(), 3)?;
            let mut pack_identities = FxHashMap::default();
            for (idx, &(reg, kind)) in deopt.registers_to_restore.iter().enumerate() {
                let r_val = builder.ins().iconst(types::I32, i64::from(reg));
                builder.ins().stack_store(r_val, regs_ss, (idx * 4) as i32);

                let (word_val, format) = match kind {
                    DeoptRegisterValue::TaggedWord(v) => (self.read_value(builder, v), 0),
                    DeoptRegisterValue::UnboxedI64(v) => {
                        let raw_i64 = self.read_value(builder, v);
                        let shifted = builder.ins().ishl_imm(raw_i64, i64::from(FIXNUM_SHIFT));
                        let tagged = builder.ins().bor_imm(shifted, TAG_FIXNUM as i64);
                        (tagged, 0)
                    }
                    DeoptRegisterValue::UnboxedFloat { format, value } => {
                        let value = self.read_value(builder, value);
                        let raw = float_bits(builder, format, value);
                        (raw, i64::from(format as u8))
                    }
                    DeoptRegisterValue::UnboxedI32x4 { identity, value } => {
                        let first = *pack_identities.entry(identity).or_insert(idx);
                        if first == idx {
                            let offset = (idx * pack_bytes as usize) as i32;
                            let raw = self.read_value(builder, value);
                            let packs = packs_slot.expect("SIMD recovery uses the pack buffer");
                            builder.ins().stack_store(raw, packs, offset);
                        }
                        (
                            builder.ins().iconst(types::I64, first as i64),
                            i64::from(DEOPT_I32X4_FORMAT),
                        )
                    }
                    DeoptRegisterValue::ConstImmediate(imm) => {
                        let encoded = self.immediate(imm)?;
                        (word_const(builder, self.ptr_ty, encoded), 0)
                    }
                };
                if let Some(formats) = formats_slot {
                    let byte = builder.ins().iconst(types::I8, format);
                    builder.ins().stack_store(byte, formats, idx as i32);
                }
                let source_ty = builder.func.dfg.value_type(word_val);
                let word_val = match source_ty.bytes().cmp(&value_ty.bytes()) {
                    std::cmp::Ordering::Less => builder.ins().uextend(value_ty, word_val),
                    std::cmp::Ordering::Greater => builder.ins().ireduce(value_ty, word_val),
                    std::cmp::Ordering::Equal => word_val,
                };
                builder
                    .ins()
                    .stack_store(word_val, vals_ss, (idx * value_bytes) as i32);
            }
            for (i, (reg, slot)) in unwind_registers.into_iter().enumerate() {
                let index = deopt.registers_to_restore.len() + i;
                let register = builder.ins().iconst(types::I32, reg as i64);
                builder
                    .ins()
                    .stack_store(register, regs_ss, (index * 4) as i32);
                let roots = self.root_frame.expect("unwind registers are rooted");
                let offset = self.word_offset(slot, "unwind register offset")?;
                let word = builder.ins().load(
                    self.ptr_ty,
                    cranelift_codegen::ir::MemFlags::trusted(),
                    roots.slots_ptr,
                    offset,
                );
                let word = if self.ptr_ty == value_ty {
                    word
                } else {
                    builder.ins().uextend(value_ty, word)
                };
                builder
                    .ins()
                    .stack_store(word, vals_ss, (index * value_bytes) as i32);
                if let Some(formats) = formats_slot {
                    let tagged = builder.ins().iconst(types::I8, 0);
                    builder.ins().stack_store(tagged, formats, index as i32);
                }
            }
            (
                builder.ins().stack_addr(self.ptr_ty, regs_ss, 0),
                builder.ins().stack_addr(self.ptr_ty, vals_ss, 0),
            )
        };

        // Reload function word from `slots[0]` before calling `deopt_to_vm` so GC updates apply.
        let func_word = self.emit_current_func_word(builder);
        let ctx = self.ctx;
        let pc_val = builder.ins().iconst(self.ptr_ty, deopt.pc as i64);
        let count_val = builder.ins().iconst(self.ptr_ty, count as i64);
        let ret_ptr = self.ret_ptr(builder);
        if let (Some(formats), Some(packs)) = (formats_slot, packs_slot) {
            let formats_ptr = builder.ins().stack_addr(self.ptr_ty, formats, 0);
            let packs_ptr = builder.ins().stack_addr(self.ptr_ty, packs, 0);
            self.emitter.emit_runtime_call(
                builder,
                RuntimeAbiFunction::DeoptSimdToVm,
                &[
                    ctx,
                    func_word,
                    pc_val,
                    regs_ptr,
                    vals_ptr,
                    formats_ptr,
                    packs_ptr,
                    count_val,
                    ret_ptr,
                ],
            )?;
        } else if let Some(formats) = formats_slot {
            let formats_ptr = builder.ins().stack_addr(self.ptr_ty, formats, 0);
            self.emitter.emit_runtime_call(
                builder,
                RuntimeAbiFunction::DeoptFloatToVm,
                &[
                    ctx,
                    func_word,
                    pc_val,
                    regs_ptr,
                    vals_ptr,
                    formats_ptr,
                    count_val,
                    ret_ptr,
                ],
            )?;
        } else {
            self.emitter.emit_runtime_call(
                builder,
                RuntimeAbiFunction::DeoptToVm,
                &[
                    ctx, func_word, pc_val, regs_ptr, vals_ptr, count_val, ret_ptr,
                ],
            )?;
        }
        self.emit_drop_root_frame(builder)?;
        self.emit_copy_ret_slot_to_out(builder);
        builder.ins().return_(&[]);
        Ok(())
    }
}

/// Reuse a stack buffer across exits. Every element starts at a signed `i32` displacement.
fn reserve_scratch(
    builder: &mut FunctionBuilder<'_>,
    cached: &mut Option<StackSlot>,
    count: usize,
    element_bytes: u32,
    align_shift: u8,
) -> Result<StackSlot, LoweringError> {
    let bytes = count
        .checked_mul(element_bytes as usize)
        .and_then(|bytes| i32::try_from(bytes).ok())
        .ok_or(LoweringError::IntegerOverflow(
            "deoptimization scratch size",
        ))? as u32;
    let slot = *cached.get_or_insert_with(|| {
        builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            bytes,
            align_shift,
        ))
    });
    let data = &mut builder.func.sized_stack_slots[slot];
    data.size = data.size.max(bytes);
    Ok(slot)
}
