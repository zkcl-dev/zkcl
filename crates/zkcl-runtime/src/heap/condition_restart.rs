use crate::OutOfMemory;
use crate::condition::Class;

use super::object::{HeapObject, WordSlot};
use super::word::{Header, ObjectKind};
use super::{Heap, Word};

#[repr(C)]
pub(super) struct LispCondition {
    pub(super) header: Header,
    pub(super) class: Class,
    pub(super) count: usize,
    // The Lisp slots follow this header as an inline array of WordSlot.
}

// SAFETY: `LispCondition` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Condition`.
unsafe impl HeapObject for LispCondition {
    const KIND: ObjectKind = ObjectKind::Condition;
}

#[repr(C)]
pub(super) struct LispRestart {
    header: Header,
    pub(super) name: WordSlot,
    pub(super) report: WordSlot,
}

// SAFETY: `LispRestart` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Restart`.
unsafe impl HeapObject for LispRestart {
    const KIND: ObjectKind = ObjectKind::Restart;
}

#[repr(C)]
pub(super) struct LispMacroEnvironment {
    header: Header,
    pub(super) id: usize,
}

// SAFETY: `LispMacroEnvironment` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::MacroEnvironment`.
unsafe impl HeapObject for LispMacroEnvironment {
    const KIND: ObjectKind = ObjectKind::MacroEnvironment;
}

#[repr(C)]
pub(super) struct LispPathname {
    header: Header,
    pub(super) components: [WordSlot; 6],
}

// SAFETY: `LispPathname` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Pathname`.
unsafe impl HeapObject for LispPathname {
    const KIND: ObjectKind = ObjectKind::Pathname;
}

impl Heap {
    pub(crate) fn pathname(
        &self,
        components: impl FnOnce() -> [Word; 6],
    ) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| LispPathname {
            header,
            components: components().map(WordSlot::new),
        })
    }

    pub(crate) fn pathname_fields(&self, word: Word) -> Option<[Word; 6]> {
        let pathname = self.object_ref::<LispPathname>(word)?;
        Some(std::array::from_fn(|index| {
            pathname.components[index].get()
        }))
    }

    pub(crate) fn macro_environment(&self, id: usize) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| LispMacroEnvironment { header, id })
    }

    pub(crate) fn macro_environment_id(&self, word: Word) -> Option<usize> {
        Some(self.object_ref::<LispMacroEnvironment>(word)?.id)
    }

    pub(crate) fn restart(&self, name: impl FnOnce() -> Word) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| LispRestart {
            header,
            name: WordSlot::new(name()),
            report: WordSlot::unbound(),
        })
    }

    pub(crate) fn restart_name(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<LispRestart>(word)?.name.get())
    }

    /// Attach the report value (a string or function designator) to a restart.
    /// `Word::UNBOUND` means the restart has no report.
    pub(crate) fn set_restart_report(&self, word: Word, report: Word) {
        if let Some(restart) = self.object_ref::<LispRestart>(word) {
            self.write_slot_object(restart, &restart.report, report);
        }
    }

    /// The report value attached to a restart, if it has one.
    pub(crate) fn restart_report(&self, word: Word) -> Option<Word> {
        self.object_ref::<LispRestart>(word)?.report.get_bound()
    }

    pub(crate) fn condition(
        &self,
        class: Class,
        count: usize,
        slot: impl Fn(usize) -> Word,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_with_trailing(
            count,
            |header| LispCondition {
                header,
                class,
                count,
            },
            |index| WordSlot::new(slot(index)),
        )
    }

    pub(crate) fn condition_class(&self, word: Word) -> Option<Class> {
        Some(self.object_ref::<LispCondition>(word)?.class)
    }

    pub(crate) fn condition_slot(&self, word: Word, index: usize) -> Option<Word> {
        self.trailing_ref::<LispCondition, WordSlot>(word, |c| c.count, index)
            .map(WordSlot::get)
    }

    pub(crate) fn set_condition_slot(&self, word: Word, index: usize, value: Word) -> Option<()> {
        let slot = self.trailing_ref::<LispCondition, WordSlot>(word, |c| c.count, index)?;
        self.write_slot(word.object_address().expect("condition"), slot, value);
        Some(())
    }
}
