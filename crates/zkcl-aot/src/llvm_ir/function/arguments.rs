//! Argument storage and binding for the native entry ABI.

use inkwell::IntPredicate;
use inkwell::values::IntValue;
use zkcl_opt::native::{NativeArguments, RuntimeAbiFunction, WORD_UNBOUND};

use super::{Function, Result};

impl<'ctx> Function<'_, 'ctx> {
    pub(super) fn prepare_argument_tail(&mut self) -> Result<()> {
        match self.layout.arguments() {
            NativeArguments::Unused => {}
            NativeArguments::UnrootedOptionals => {
                let slot = self.allocas.build_alloca(self.ir.word, "optional_argc")?;
                self.builder.build_store(slot, self.argc)?;
                self.unrooted_argc = Some(slot);
            }
            NativeArguments::RootedOptionals(slots) => {
                for (index, slot) in slots.iter() {
                    let argument = self.load_optional_argument(self.argc, index)?;
                    self.builder.build_store(self.root(slot)?, argument)?;
                }
            }
            NativeArguments::RuntimeTail => {
                let required = self.ssa.arity.required;
                let tail = if required == 0 {
                    self.args
                } else {
                    self.element(self.ir.word, self.args, required)?
                };
                let count =
                    self.builder
                        .build_int_sub(self.argc, self.word(required), "tail_len")?;
                self.call(
                    RuntimeAbiFunction::StageArguments,
                    &[
                        self.ctx.into(),
                        self.argument_frame().into(),
                        tail.into(),
                        count.into(),
                    ],
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn local_optional_argument(&self, index: usize) -> Result<Option<IntValue<'ctx>>> {
        match self.layout.arguments() {
            NativeArguments::Unused => unreachable!("no optional argument instructions"),
            NativeArguments::UnrootedOptionals => {
                let argc = self.load_word(self.unrooted_argc.expect("live argument count"))?;
                self.load_optional_argument(argc, index).map(Some)
            }
            NativeArguments::RootedOptionals(slots) => {
                self.load_word(self.root(slots.slot_for(index))?).map(Some)
            }
            NativeArguments::RuntimeTail => Ok(None),
        }
    }

    /// Returns false when the shared runtime release call must be emitted.
    pub(super) fn release_local_arguments(&self) -> Result<bool> {
        match self.layout.arguments() {
            NativeArguments::Unused => {}
            NativeArguments::UnrootedOptionals => {
                let count = self.unrooted_argc.expect("live argument count");
                self.builder.build_store(count, self.word(0))?;
            }
            NativeArguments::RootedOptionals(slots) => {
                let unbound = self.ir.word.const_int(WORD_UNBOUND, false);
                for slot in slots.slots() {
                    self.builder.build_store(self.root(slot)?, unbound)?;
                }
            }
            NativeArguments::RuntimeTail => return Ok(false),
        }
        Ok(true)
    }

    fn argument_frame(&self) -> IntValue<'ctx> {
        self.frame.expect("runtime argument tail").index
    }

    fn load_optional_argument(&self, argc: IntValue<'ctx>, index: usize) -> Result<IntValue<'ctx>> {
        let position = self.ssa.arity.required + index;
        let present =
            self.builder
                .build_int_compare(IntPredicate::ULT, self.word(position), argc, "")?;
        self.choose_int(
            present,
            || self.load_word(self.element(self.ir.word, self.args, position)?),
            || Ok(self.ir.word.const_int(WORD_UNBOUND, false)),
        )
    }
}
