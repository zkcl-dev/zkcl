use super::{
    Address, Header, Heap, HeapObject, ObjectKind, OutOfMemory, SlotVisitor, Word, WordSlot,
    object_address, offset_of, visit_offsets,
};

#[repr(C)]
struct Specializer {
    header: Header,
    object: WordSlot,
    direct_methods: WordSlot,
    meta_instance: WordSlot,
}

// SAFETY: `Specializer` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Specializer`.
unsafe impl HeapObject for Specializer {
    const KIND: ObjectKind = ObjectKind::Specializer;
}

#[derive(Clone, Copy)]
pub(crate) struct SlotFields {
    pub(crate) name: Word,
    pub(crate) initargs: Word,
    pub(crate) initform: Word,
    pub(crate) initfunction: Word,
    pub(crate) allocation: Word,
    pub(crate) location: Word,
    pub(crate) type_specifier: Word,
    pub(crate) readers: Word,
    pub(crate) writers: Word,
    pub(crate) documentation: Word,
    pub(crate) direct: bool,
}

impl SlotFields {
    pub(crate) fn empty(direct: bool, allocation: Word) -> Self {
        Self {
            direct,
            allocation,
            name: Word::NIL,
            initargs: Word::NIL,
            initform: Word::NIL,
            initfunction: Word::NIL,
            type_specifier: Word::T,
            location: Word::NIL,
            readers: Word::NIL,
            writers: Word::NIL,
            documentation: Word::NIL,
        }
    }

    fn fields(self) -> [Word; 10] {
        [
            self.name,
            self.initargs,
            self.initform,
            self.initfunction,
            self.allocation,
            self.location,
            self.type_specifier,
            self.readers,
            self.writers,
            self.documentation,
        ]
    }
}

#[repr(C)]
struct SlotDefinition {
    header: Header,
    fields: [WordSlot; 10],
    meta_instance: WordSlot,
    location_owner: WordSlot,
    direct: bool,
}

// SAFETY: `SlotDefinition` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::SlotDefinition`.
unsafe impl HeapObject for SlotDefinition {
    const KIND: ObjectKind = ObjectKind::SlotDefinition;
}

impl Heap {
    pub(crate) fn mop_relations(&self, word: Word, dependents: bool) -> Option<Word> {
        let field = self.relation_slot(word, dependents)?;
        // SAFETY: The returned address denotes a traced word slot of this object.
        Some(unsafe { Self::load_word_slot(field) })
    }

    pub(crate) fn set_mop_relations(&self, word: Word, dependents: bool, list: Word) -> Option<()> {
        let field = self.relation_slot(word, dependents)?;
        let owner = word.object_address().expect("metaobject");
        // SAFETY: The live field is traced and belongs to `owner`; write barrier tracks young `list`.
        unsafe { self.write_word_slot(owner, field, list) };
        Some(())
    }

    fn relation_slot(&self, word: Word, dependents: bool) -> Option<Address> {
        if dependents {
            self.class_dependents_slot(word)
                .or_else(|| self.gf_dependents_slot(word))
        } else {
            self.class_methods_slot(word).or_else(|| {
                Some(
                    object_address(word, ObjectKind::Specializer)?
                        + offset_of!(Specializer, direct_methods),
                )
            })
        }
    }

    fn callable_meta_slot(&self, word: Word) -> Option<Address> {
        self.generic_meta_slot(word).or_else(|| {
            Some(
                object_address(word, ObjectKind::Specializer)?
                    + offset_of!(Specializer, meta_instance),
            )
        })
    }

    pub(super) fn additional_meta_instance(&self, word: Word) -> Option<Word> {
        let field = self.callable_meta_slot(word)?;
        // SAFETY: The field belongs to the checked object.
        let value = unsafe { Self::load_word_slot(field) };
        (value != Word::NIL).then_some(value)
    }

    pub(crate) fn set_additional_meta_instance(&self, word: Word, instance: Word) {
        let field = self
            .callable_meta_slot(word)
            .expect("metaobject with instance storage");
        let owner = word.object_address().expect("metaobject");
        // SAFETY: This field is visited by `owner`'s scanner.
        unsafe { self.write_word_slot(owner, field, instance) };
    }

    pub(crate) fn specializer(&self, object: impl FnOnce() -> Word) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| Specializer {
            header,
            object: WordSlot::new(object()),
            direct_methods: WordSlot::nil(),
            meta_instance: WordSlot::nil(),
        })
    }

    pub(crate) fn is_specializer(&self, word: Word) -> bool {
        self.object_ref::<Specializer>(word).is_some()
            || object_address(word, ObjectKind::Class).is_some()
    }

    pub(crate) fn initialize_eql_specializer(&self, word: Word, object: Word) -> bool {
        let Some(spec) = self.object_ref::<Specializer>(word) else {
            return false;
        };
        self.compare_exchange_slot(spec, &spec.object, Word::UNBOUND, object)
            .is_ok()
    }

    pub(crate) fn eql_specializer_object(&self, word: Word) -> Option<Word> {
        self.object_ref::<Specializer>(word)?.object.get_bound()
    }

    pub(crate) fn slot_definition(
        &self,
        info: impl FnOnce() -> SlotFields,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| {
            let info = info();
            SlotDefinition {
                header,
                fields: info.fields().map(WordSlot::new),
                meta_instance: WordSlot::nil(),
                location_owner: WordSlot::nil(),
                direct: info.direct,
            }
        })
    }

    pub(super) fn slot_meta_instance(&self, word: Word) -> Option<Word> {
        self.object_ref::<SlotDefinition>(word)?
            .meta_instance
            .get_non_nil()
    }

    pub(crate) fn set_slot_meta_instance(&self, word: Word, instance: Word) {
        let slot = self.expect_object::<SlotDefinition>(word, "slot metaobject");
        self.write_slot_object(slot, &slot.meta_instance, instance);
    }

    pub(crate) fn update_slot_definition(&self, word: Word, info: SlotFields) {
        let definition = self.expect_object::<SlotDefinition>(word, "slot definition");
        for (field, value) in definition.fields.iter().zip(info.fields()) {
            self.write_slot_object(definition, field, value);
        }
    }

    pub(crate) fn slot_location_owner(&self, word: Word) -> Option<Word> {
        Some(
            self.object_ref::<SlotDefinition>(word)?
                .location_owner
                .get(),
        )
    }

    pub(crate) fn set_slot_location(&self, word: Word, owner: Word, location: Word) {
        let slot = self.expect_object::<SlotDefinition>(word, "effective slot definition");
        self.write_slot_object(slot, &slot.location_owner, owner);
        self.write_slot_object(slot, &slot.fields[5], location);
    }

    pub(crate) fn slot_definition_info(&self, word: Word) -> Option<SlotFields> {
        let slot = self.object_ref::<SlotDefinition>(word)?;
        let [
            name,
            initargs,
            initform,
            initfunction,
            allocation,
            location,
            type_specifier,
            readers,
            writers,
            documentation,
        ] = std::array::from_fn(|index| slot.fields[index].get());
        Some(SlotFields {
            name,
            initargs,
            initform,
            initfunction,
            allocation,
            location,
            type_specifier,
            readers,
            writers,
            documentation,
            direct: slot.direct,
        })
    }
}

pub(super) fn scan_specializer<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(Specializer, object),
            offset_of!(Specializer, direct_methods),
            offset_of!(Specializer, meta_instance),
        ],
    );
}

pub(super) fn scan_slot_definition<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(SlotDefinition, meta_instance),
            offset_of!(SlotDefinition, location_owner),
        ],
    );
    visitor.visit_array(base + offset_of!(SlotDefinition, fields), 10);
}

pub(super) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    if heap.object_ref::<Specializer>(word).is_some() {
        return Some(("specializer", vec![]));
    }
    Some((
        "slot-definition",
        vec![heap.object_ref::<SlotDefinition>(word)?.direct as u64],
    ))
}
pub(super) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    let mut d = super::image::Data(data);
    let word = match kind {
        "specializer" => {
            d.done()?;
            heap.specializer(|| Word::NIL)
        }
        "slot-definition" => {
            let direct = d.flag()?;
            d.done()?;
            heap.slot_definition(|| SlotFields::empty(direct, Word::NIL))
        }
        _ => return Ok(None),
    };
    word.map(Some)
        .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
