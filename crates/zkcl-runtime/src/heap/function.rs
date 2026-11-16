use std::mem::offset_of;

use crate::OutOfMemory;

use super::object::{HeapObject, WordSlot};
use super::word::{Header, ObjectKind, object_address};
use super::{Heap, RootSlot, RootSlots, Word};

#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) struct FunctionFields {
    pub(crate) code_id: usize,
    pub(crate) constants: usize,
}

#[repr(C)]
pub(super) struct Function {
    header: Header,
    pub(super) info: FunctionFields,
    // Followed by the constant slots and one documentation slot.
}

// SAFETY: `Function` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Function`.
unsafe impl HeapObject for Function {
    const KIND: ObjectKind = ObjectKind::Function;
}

#[repr(C)]
pub(super) struct Closure {
    header: Header,
    pub(super) code: WordSlot,
    pub(super) count: usize,
    // Followed by the capture slots and one documentation slot.
}

// SAFETY: `Closure` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Closure`.
unsafe impl HeapObject for Closure {
    const KIND: ObjectKind = ObjectKind::Closure;
}

#[repr(C)]
pub(super) struct HostFunction {
    header: Header,
    pub(super) id: u64,
    pub(super) capture: WordSlot,
    callback: *const crate::HostCallback,
}

// SAFETY: `HostFunction` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::HostFunction`.
unsafe impl HeapObject for HostFunction {
    const KIND: ObjectKind = ObjectKind::HostFunction;
}

impl FunctionFields {
    pub(super) const fn constants_offset(self) -> usize {
        size_of::<Function>()
    }
}

pub(crate) const FUNCTION_INFO_OFFSET: usize = offset_of!(Function, info);
pub(crate) const FUNCTION_CONSTANTS_OFFSET: usize = size_of::<Function>();
pub(crate) const CLOSURE_CODE_OFFSET: usize = offset_of!(Closure, code);
pub(crate) const CLOSURE_CAPTURES_OFFSET: usize = size_of::<Closure>();

impl Heap {
    pub(crate) fn function(
        &self,
        code_id: usize,
        constants: &RootSlots,
    ) -> Result<Word, OutOfMemory> {
        self.allocate_function(code_id, constants.len(), |index| constants.get(index))
    }

    pub(crate) fn function_unfilled(
        &self,
        code_id: usize,
        constants: usize,
    ) -> Result<Word, OutOfMemory> {
        self.allocate_function(code_id, constants, |_| Word::NIL)
    }

    pub(crate) fn allocate_function(
        &self,
        code_id: usize,
        constants: usize,
        constant: impl Fn(usize) -> Word,
    ) -> Result<Word, OutOfMemory> {
        // A collection during allocation cannot discover the unpublished function.
        let _code = self.code_store.get(code_id);
        if self.code_store.should_collect() {
            self.collect();
        }
        let info = FunctionFields { code_id, constants };
        self.alloc_with_trailing(
            constants + 1,
            |header| Function { header, info },
            |i| {
                WordSlot::new(if i < constants {
                    constant(i)
                } else {
                    Word::NIL
                })
            },
        )
    }

    pub(crate) fn closure(
        &self,
        count: usize,
        code: impl FnOnce() -> Word,
        capture: impl Fn(usize) -> Word,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_with_trailing(
            count + 1,
            |header| Closure {
                header,
                code: WordSlot::new(code()),
                count,
            },
            |i| WordSlot::new(if i < count { capture(i) } else { Word::NIL }),
        )
    }

    pub(crate) fn allocate_host_function(
        &self,
        id: u64,
        capture: &RootSlot,
        callback: *const crate::HostCallback,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| HostFunction {
            header,
            id,
            capture: WordSlot::new(capture.get()),
            callback,
        })
    }

    pub(crate) fn is_host_function(&self, word: Word) -> bool {
        object_address(word, ObjectKind::HostFunction).is_some()
    }

    pub(crate) fn host_callback(&self, word: Word) -> Option<*const crate::HostCallback> {
        Some(self.object_ref::<HostFunction>(word)?.callback)
    }

    pub(crate) fn function_info(&self, word: Word) -> Option<FunctionFields> {
        Some(self.object_ref::<Function>(word)?.info)
    }

    pub(crate) fn is_callable_object(&self, word: Word) -> bool {
        let Some(address) = word.object_address() else {
            return false;
        };
        // SAFETY: the live word identifies an object starting with a header.
        match unsafe { (*address.to_ptr::<Header>()).kind } {
            ObjectKind::Function
            | ObjectKind::Closure
            | ObjectKind::GenericFunction
            | ObjectKind::FuncallableInstance
            | ObjectKind::HostFunction => true,
            _ => false,
        }
    }

    pub(crate) fn function_code(&self, word: Word) -> Option<Word> {
        if let Some(closure) = self.object_ref::<Closure>(word) {
            Some(closure.code.get())
        } else {
            object_address(word, ObjectKind::Function).map(|_| word)
        }
    }

    pub(crate) fn capture(&self, word: Word, index: usize) -> Option<Word> {
        self.trailing_ref::<Closure, WordSlot>(word, |c| c.count, index)
            .map(WordSlot::get)
    }

    pub(crate) fn initialize_closure(
        &self,
        word: Word,
        code: Word,
        captures: impl ExactSizeIterator<Item = Word>,
    ) {
        let owner = word.object_address().expect("closure object");
        let closure = self.expect_object::<Closure>(word, "closure object");
        assert_eq!(captures.len(), closure.count);
        self.write_slot(owner, &closure.code, code);
        let slots = self
            .trailing_slice::<Closure, WordSlot>(word, |c| c.count)
            .expect("closure object");
        for (slot, value) in slots.iter().zip(captures) {
            self.write_slot(owner, slot, value);
        }
    }

    /// Documentation belongs to the individual function, including closures of the
    /// same code. Its final slot follows constants or captures to preserve their ABI.
    fn function_doc_slot(&self, word: Word) -> Option<&WordSlot> {
        if let Some(function) = self.object_ref::<Function>(word) {
            return self.trailing_ref::<Function, WordSlot>(
                word,
                |f| f.info.constants + 1,
                function.info.constants,
            );
        }
        if let Some(closure) = self.object_ref::<Closure>(word) {
            return self.trailing_ref::<Closure, WordSlot>(word, |c| c.count + 1, closure.count);
        }
        self.funcallable_doc_slot(word)
    }

    pub(crate) fn function_documentation(&self, word: Word) -> Option<Word> {
        self.function_doc_slot(word).map(WordSlot::get)
    }

    pub(crate) fn set_function_documentation(&self, word: Word, doc: Word) {
        if let Some(slot) = self.function_doc_slot(word) {
            self.write_slot(word.object_address().expect("function object"), slot, doc);
            self.namespace_changed();
        }
    }

    pub(crate) fn function_constant(&self, word: Word, index: usize) -> Word {
        self.trailing_ref::<Function, WordSlot>(word, |f| f.info.constants, index)
            .expect("function constant slot")
            .get()
    }

    pub(crate) fn set_function_constant(&self, word: Word, index: usize, value: Word) {
        let slot = self
            .trailing_ref::<Function, WordSlot>(word, |f| f.info.constants, index)
            .expect("function constant slot");
        self.write_slot(word.object_address().expect("function object"), slot, value);
    }
}
