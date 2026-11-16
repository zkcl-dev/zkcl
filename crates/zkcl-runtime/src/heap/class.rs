use std::sync::atomic::AtomicU8;

use super::{
    Address, Header, Heap, HeapObject, ObjectKind, Ordering, OutOfMemory, SlotVisitor, Word,
    WordSlot, object_address, offset_of, visit_offsets,
};

#[derive(Clone, Copy)]
#[repr(u8)]
pub(crate) enum ClassKind {
    BuiltIn,
    Standard,
    Structure,
    ForwardReferenced,
}

impl ClassKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::BuiltIn => "BUILT-IN-CLASS",
            Self::Standard => "STANDARD-CLASS",
            Self::Structure => "STRUCTURE-CLASS",
            Self::ForwardReferenced => "FORWARD-REFERENCED-CLASS",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ClassFields {
    pub(crate) name: Word,
    pub(crate) origin_name: Word,
    pub(crate) kind: ClassKind,
    pub(crate) serial: usize,
}

#[repr(C)]
struct LispClass {
    header: Header,
    name: WordSlot,
    origin_name: WordSlot,
    // A standard class metaobject uses precisely the same GC-traced slot storage
    // and obsolete-instance updates as other instances of its metaclass.
    meta_instance: WordSlot,
    previous_subclasses: WordSlot,
    direct_methods: WordSlot,
    dependents: WordSlot,
    kind: AtomicU8,
    serial: usize,
}

// SAFETY: `LispClass` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Class`.
unsafe impl HeapObject for LispClass {
    const KIND: ObjectKind = ObjectKind::Class;
}

impl Heap {
    pub(crate) fn class(
        &self,
        kind: ClassKind,
        serial: usize,
        name: impl FnOnce() -> Word,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| {
            let name = name();
            LispClass {
                header,
                name: WordSlot::new(name),
                origin_name: WordSlot::new(name),
                meta_instance: WordSlot::nil(),
                previous_subclasses: WordSlot::nil(),
                direct_methods: WordSlot::nil(),
                dependents: WordSlot::nil(),
                kind: AtomicU8::new(kind as u8),
                serial,
            }
        })
    }

    pub(crate) fn class_info(&self, word: Word) -> Option<ClassFields> {
        let class = self.object_ref::<LispClass>(word)?;
        Some(ClassFields {
            name: class.name.get(),
            origin_name: class.origin_name.get(),
            kind: match class.kind.load(Ordering::Relaxed) {
                x if x == ClassKind::BuiltIn as u8 => ClassKind::BuiltIn,
                x if x == ClassKind::Standard as u8 => ClassKind::Standard,
                x if x == ClassKind::Structure as u8 => ClassKind::Structure,
                x if x == ClassKind::ForwardReferenced as u8 => ClassKind::ForwardReferenced,
                _ => unreachable!("class kind"),
            },
            serial: class.serial,
        })
    }

    pub(crate) fn previous_class_subclasses(&self, class: Word) -> Word {
        self.expect_object::<LispClass>(class, "class metaobject")
            .previous_subclasses
            .get()
    }

    pub(crate) fn set_previous_class_subclasses(&self, previous: Word, subclasses: Word) {
        let class = self.expect_object::<LispClass>(previous, "previous class");
        self.write_slot_object(class, &class.previous_subclasses, subclasses);
    }

    pub(super) fn class_methods_slot(&self, word: Word) -> Option<Address> {
        Some(object_address(word, ObjectKind::Class)? + offset_of!(LispClass, direct_methods))
    }

    pub(super) fn class_dependents_slot(&self, word: Word) -> Option<Address> {
        Some(object_address(word, ObjectKind::Class)? + offset_of!(LispClass, dependents))
    }

    pub(crate) fn class_instance(&self, word: Word) -> Option<Word> {
        self.object_ref::<LispClass>(word)?
            .meta_instance
            .get_non_nil()
    }

    pub(crate) fn set_class_instance(&self, class: Word, instance: Word) {
        let class = self.expect_object::<LispClass>(class, "class metaobject");
        self.write_slot_object(class, &class.meta_instance, instance);
    }

    pub(crate) fn set_class_name(&self, class: Word, name: Word) -> Option<()> {
        let class = self.object_ref::<LispClass>(class)?;
        self.write_slot_object(class, &class.name, name);
        Some(())
    }

    pub(crate) fn resolve_forward_class(&self, class: Word, kind: ClassKind) {
        let class = self.expect_object::<LispClass>(class, "class object");
        debug_assert_eq!(
            class.kind.load(Ordering::Relaxed),
            ClassKind::ForwardReferenced as u8
        );
        class.kind.store(kind as u8, Ordering::Relaxed);
    }
}

pub(super) fn scan<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(LispClass, name),
            offset_of!(LispClass, origin_name),
            offset_of!(LispClass, meta_instance),
            offset_of!(LispClass, previous_subclasses),
            offset_of!(LispClass, direct_methods),
            offset_of!(LispClass, dependents),
        ],
    );
}

pub(super) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    let value = heap.object_ref::<LispClass>(word)?;
    Some((
        "class",
        vec![
            value.kind.load(Ordering::Relaxed) as u64,
            value.serial as u64,
        ],
    ))
}
pub(super) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    if kind != "class" {
        return Ok(None);
    }
    let mut d = super::image::Data(data);
    let kind = match d.number()? {
        0 => ClassKind::BuiltIn,
        1 => ClassKind::Standard,
        2 => ClassKind::Structure,
        3 => ClassKind::ForwardReferenced,
        _ => return Err(super::image::invalid()),
    };
    let serial = d.number()?;
    d.done()?;
    heap.class(kind, serial, || Word::NIL)
        .map(Some)
        .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
