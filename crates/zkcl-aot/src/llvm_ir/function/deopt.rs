//! Reconstruct a bytecode frame at a failed guard or native call handoff.

use rustc_hash::FxHashMap;
use zkcl_bytecode::native_abi::DEOPT_I32X4_FORMAT;
use zkcl_opt::{DeoptRegisterValue, DeoptState, RootSlot, ValueId};

use super::{Function, Result, RuntimeAbiFunction};

impl Function<'_, '_> {
    pub(super) fn emit_deopt(
        &self,
        state: &DeoptState,
        live: &[(ValueId, RootSlot)],
    ) -> Result<()> {
        let restore = &state.registers_to_restore;
        let has_packs = restore
            .iter()
            .any(|(_, kind)| matches!(kind, DeoptRegisterValue::UnboxedI32x4 { .. }));
        let needs_formats = has_packs
            || restore
                .iter()
                .any(|(_, kind)| matches!(kind, DeoptRegisterValue::UnboxedFloat { .. }));
        let register_type = self.ir.context.i32_type();
        let format_type = self.ir.context.i8_type();
        let scalar_type = if needs_formats {
            self.ir.context.i64_type()
        } else {
            self.ir.word
        };
        let mut registers = Vec::with_capacity(restore.len());
        let mut scalars = Vec::with_capacity(restore.len());
        let mut formats = Vec::new();
        let mut packs = Vec::new();
        let mut pack_identities = FxHashMap::default();
        for (index, &(reg, kind)) in restore.iter().enumerate() {
            registers.push(register_type.const_int(reg.into(), false));
            let (raw, format) = match kind {
                DeoptRegisterValue::TaggedWord(value) => {
                    let word = if let Some((_, slot)) = live.iter().find(|(id, _)| *id == value) {
                        self.load_word(self.root(slot.0)?)?
                    } else {
                        self.value(value)?
                    };
                    (word, 0)
                }
                DeoptRegisterValue::UnboxedI64(value) => (self.box_fixnum(self.value(value)?)?, 0),
                DeoptRegisterValue::ConstImmediate(imm) => (self.immediate(imm)?, 0),
                DeoptRegisterValue::UnboxedFloat { format, value } => {
                    let bits =
                        self.float_bits(format, self.basic_value(value)?.into_float_value())?;
                    (bits, format as u8)
                }
                DeoptRegisterValue::UnboxedI32x4 { identity, value } => {
                    // The runtime allocates one pack per identity even if several
                    // VM registers alias it. Each alias points at the first slot.
                    let first = *pack_identities.entry(identity).or_insert(index);
                    if first == index {
                        packs.push((index, self.basic_value(value)?.into_vector_value()));
                    }
                    (
                        scalar_type.const_int(first as u64, false),
                        DEOPT_I32X4_FORMAT,
                    )
                }
            };
            scalars.push(
                self.builder
                    .build_int_z_extend_or_bit_cast(raw, scalar_type, "")?,
            );
            if needs_formats {
                formats.push(format_type.const_int(format.into(), false));
            }
        }
        for (reg, slot) in self.layout.unwind_restore(state) {
            registers.push(register_type.const_int(reg as u64, false));
            let word = self.load_word(self.root(slot)?)?;
            scalars.push(
                self.builder
                    .build_int_z_extend_or_bit_cast(word, scalar_type, "")?,
            );
            if needs_formats {
                formats.push(format_type.const_zero());
            }
        }

        let scalar_scratch = if needs_formats {
            &self.scratch.restore_scalars
        } else {
            &self.scratch.words
        };
        let mut args = vec![
            self.ctx.into(),
            self.target_word(0)?.into(),
            self.word(state.pc).into(),
            self.scratch.registers.write(self, &registers)?.into(),
            scalar_scratch.write(self, &scalars)?.into(),
        ];
        if needs_formats {
            args.push(self.scratch.restore_formats.write(self, &formats)?.into());
        }
        if has_packs {
            let buffer = self.scratch.packs.reserve(self, registers.len())?;
            for (index, pack) in packs {
                let slot = self.element(self.ir.pack_type(), buffer, index)?;
                self.builder.build_store(slot, pack)?;
            }
            args.push(buffer.into());
        }
        args.push(self.word(registers.len()).into());
        args.push(self.output.into());
        let runtime_function = if has_packs {
            RuntimeAbiFunction::DeoptSimdToVm
        } else if needs_formats {
            RuntimeAbiFunction::DeoptFloatToVm
        } else {
            RuntimeAbiFunction::DeoptToVm
        };
        self.call(runtime_function, &args)?;
        self.drop_frame()?;
        self.builder.build_return(None)?;
        Ok(())
    }
}
