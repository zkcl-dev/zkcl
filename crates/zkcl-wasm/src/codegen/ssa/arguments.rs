//! Argument storage and binding for the wasm32 entry ABI.

use zkcl_opt::native::{NativeArguments, RuntimeAbiFunction, WORD_UNBOUND};

use super::{ARGC, ARGS, BlockType, Emitter, Ins, ValType, WORD_BYTES};

impl<F: Fn(RuntimeAbiFunction) -> Option<u32>> Emitter<'_, F> {
    pub(super) fn prepare_argument_tail(&mut self) {
        match self.frame.arguments() {
            NativeArguments::RootedOptionals(slots) => {
                for (index, slot) in slots.iter() {
                    self.root_slot(slot);
                    self.load_optional_argument(index);
                    self.store(0);
                }
            }
            NativeArguments::RuntimeTail => {
                let required = self.ssa.arity.required;
                self.ctx();
                self.frame_index();
                self.address(ARGS, required);
                self.ins(Ins::LocalGet(ARGC));
                self.word(required);
                self.ins(Ins::I32Sub);
                self.call(RuntimeAbiFunction::StageArguments);
            }
            NativeArguments::Unused | NativeArguments::UnrootedOptionals => {}
        }
    }

    /// Returns false when the optional argument is stored by the runtime.
    pub(super) fn local_optional_argument(&mut self, index: usize) -> bool {
        match self.frame.arguments() {
            NativeArguments::Unused => unreachable!("no optional argument instructions"),
            NativeArguments::UnrootedOptionals => self.load_optional_argument(index),
            NativeArguments::RootedOptionals(slots) => self.read_root(slots.slot_for(index)),
            NativeArguments::RuntimeTail => return false,
        }
        true
    }

    /// Returns false when the shared runtime release call must be emitted.
    pub(super) fn release_local_arguments(&mut self) -> bool {
        match self.frame.arguments() {
            NativeArguments::Unused => {}
            NativeArguments::UnrootedOptionals => {
                self.word(0);
                self.ins(Ins::LocalSet(ARGC));
            }
            NativeArguments::RootedOptionals(slots) => {
                for slot in slots.slots() {
                    self.root_slot(slot);
                    self.word(WORD_UNBOUND as usize);
                    self.store(0);
                }
            }
            NativeArguments::RuntimeTail => return false,
        }
        true
    }

    fn load_optional_argument(&mut self, index: usize) {
        let position = self.ssa.arity.required + index;
        self.word(position);
        self.ins(Ins::LocalGet(ARGC));
        self.ins(Ins::I32LtU);
        self.scope(Ins::If(BlockType::Result(ValType::I32)));
        self.ins(Ins::LocalGet(ARGS));
        self.load(position as u64 * WORD_BYTES);
        self.ins(Ins::Else);
        self.word(WORD_UNBOUND as usize);
        self.end();
    }
}
