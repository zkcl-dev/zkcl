use super::{
    Address, Header, Heap, HeapObject, ObjectKind, OutOfMemory, RootSlot, SlotVisitor, ValueSlot,
    Word, WordSlot, offset_of, trailing_base, visit_offsets,
};

#[repr(C)]
struct Instance {
    header: Header,
    class: WordSlot,
    storage: WordSlot,
}

// SAFETY: `Instance` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Instance`.
unsafe impl HeapObject for Instance {
    const KIND: ObjectKind = ObjectKind::Instance;
}

// Only funcallable instances carry callable state. Both layouts share the
// instance header so class and slot access use the same offsets.
#[repr(C)]
struct FuncallableInstance {
    base: Instance,
    function: WordSlot,
    documentation: WordSlot,
}

// SAFETY: `FuncallableInstance` is `#[repr(C)]` with `base.header` at offset 0 and tagged `ObjectKind::FuncallableInstance`.
unsafe impl HeapObject for FuncallableInstance {
    const KIND: ObjectKind = ObjectKind::FuncallableInstance;
}

#[repr(C)]
struct InstanceStorage {
    header: Header,
    layout: WordSlot,
    count: usize,
}

// SAFETY: `InstanceStorage` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::InstanceStorage`.
unsafe impl HeapObject for InstanceStorage {
    const KIND: ObjectKind = ObjectKind::InstanceStorage;
}

#[repr(C)]
struct InstanceLayout {
    header: Header,
    total: usize,
    local: usize,
}

// SAFETY: `InstanceLayout` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::InstanceLayout`.
unsafe impl HeapObject for InstanceLayout {
    const KIND: ObjectKind = ObjectKind::InstanceLayout;
}

#[repr(C)]
struct LayoutSlot {
    name: WordSlot,
    cell: WordSlot,
}

impl Heap {
    /// The first `local` entries define the storage indexes of local slots.
    pub(crate) fn instance_layout(
        &self,
        total: usize,
        local: usize,
        entry: impl Fn(usize) -> (Word, Option<Word>),
    ) -> Result<Word, OutOfMemory> {
        self.alloc_with_trailing(
            total,
            |header| InstanceLayout {
                header,
                total,
                local,
            },
            |index| {
                let (name, cell) = entry(index);
                LayoutSlot {
                    name: WordSlot::new(name),
                    cell: WordSlot::new(cell.unwrap_or(Word::UNBOUND)),
                }
            },
        )
    }

    pub(crate) fn instance(
        &self,
        class: &RootSlot,
        layout: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let storage = self.root_slot(self.new_instance_storage(layout, |_| Word::UNBOUND)?);
        self.new_instance(class, &storage, false)
    }

    pub(crate) fn funcallable_instance(
        &self,
        class: &RootSlot,
        layout: &RootSlot,
    ) -> Result<Word, OutOfMemory> {
        let storage = self.root_slot(self.new_instance_storage(layout, |_| Word::UNBOUND)?);
        self.new_instance(class, &storage, true)
    }

    fn new_instance(
        &self,
        class: &RootSlot,
        storage: &RootSlot,
        funcallable: bool,
    ) -> Result<Word, OutOfMemory> {
        let make_base = |header| Instance {
            header,
            class: WordSlot::new(class.get()),
            storage: WordSlot::new(storage.get()),
        };
        if funcallable {
            self.alloc_object(|header| FuncallableInstance {
                base: make_base(header),
                function: WordSlot::unbound(),
                documentation: WordSlot::nil(),
            })
        } else {
            self.alloc_object(make_base)
        }
    }

    pub(super) fn funcallable_doc_slot(&self, word: Word) -> Option<&WordSlot> {
        Some(&self.object_ref::<FuncallableInstance>(word)?.documentation)
    }

    pub(crate) fn funcallable_target(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<FuncallableInstance>(word)?.function.get())
    }

    pub(crate) fn set_funcallable_target(&self, word: Word, function: Word) -> Option<()> {
        let instance = self.object_ref::<FuncallableInstance>(word)?;
        self.write_slot_object(instance, &instance.function, function);
        Some(())
    }

    fn instance_header(&self, word: Word) -> Option<&Instance> {
        let direct = |word| {
            let address = Word::object_address(word)?;
            // SAFETY: the live heap word starts with a header for either layout.
            let kind = unsafe { (*address.to_ptr::<Header>()).kind };
            matches!(kind, ObjectKind::Instance | ObjectKind::FuncallableInstance)
                // SAFETY: `Instance` is the common prefix of both `Instance` and `FuncallableInstance`.
                .then(|| unsafe { &*address.to_ptr::<Instance>() })
        };
        direct(word).or_else(|| {
            let backing = self
                .class_instance(word)
                .or_else(|| self.slot_meta_instance(word))
                .or_else(|| self.additional_meta_instance(word))?;
            direct(backing)
        })
    }

    pub(crate) fn instance_class(&self, word: Word) -> Option<Word> {
        Some(self.instance_header(word)?.class.get())
    }

    pub(crate) fn instance_layout_of(&self, word: Word) -> Option<Word> {
        let storage = self.instance_header(word)?.storage.get();
        Some(self.object_ref::<InstanceStorage>(storage)?.layout.get())
    }

    pub(crate) fn instance_layout_entries(&self, word: Word) -> Option<Vec<(Word, Option<Word>)>> {
        let slots = self.trailing_slice::<InstanceLayout, LayoutSlot>(word, |l| l.total)?;
        Some(
            slots
                .iter()
                .map(|slot| (slot.name.get(), slot.cell.get_bound()))
                .collect(),
        )
    }

    pub(crate) fn instance_slot(&self, word: Word, index: usize) -> Option<Word> {
        self.instance_slot_cell(word, index)
            .map(|(_, slot)| slot.get())
    }

    pub(crate) fn set_instance_slot(&self, word: Word, index: usize, value: Word) -> Option<()> {
        let (owner, slot) = self.instance_slot_cell(word, index)?;
        self.write_slot(owner, slot, value);
        Some(())
    }

    pub(crate) fn replace_instance_storage(
        &self,
        instance: &RootSlot,
        layout: &RootSlot,
        slot: impl Fn(usize) -> Word,
    ) -> Result<(), OutOfMemory> {
        let storage = self.new_instance_storage(layout, slot)?;
        let header = self
            .instance_header(instance.get())
            .expect("rooted instance");
        self.write_slot_object(header, &header.storage, storage);
        Ok(())
    }

    pub(crate) fn replace_instance_class_and_storage(
        &self,
        instance: &RootSlot,
        class: &RootSlot,
        layout: &RootSlot,
        slot: impl Fn(usize) -> Word,
    ) -> Result<(), OutOfMemory> {
        let storage = self.new_instance_storage(layout, slot)?;
        let header = self
            .instance_header(instance.get())
            .expect("rooted instance");
        self.write_slot_object(header, &header.class, class.get());
        self.write_slot_object(header, &header.storage, storage);
        Ok(())
    }

    fn new_instance_storage(
        &self,
        layout: &RootSlot,
        slot: impl Fn(usize) -> Word,
    ) -> Result<Word, OutOfMemory> {
        let count = self
            .expect_object::<InstanceLayout>(layout.get(), "instance layout")
            .local;
        self.alloc_with_trailing(
            count,
            |header| InstanceStorage {
                header,
                layout: WordSlot::new(layout.get()),
                count,
            },
            |index| WordSlot::new(slot(index)),
        )
    }

    fn instance_slot_cell(&self, word: Word, index: usize) -> Option<(Address, &WordSlot)> {
        let storage = self.instance_header(word)?.storage.get();
        let owner = storage.object_address()?;
        let slot = self.trailing_ref::<InstanceStorage, WordSlot>(storage, |s| s.count, index)?;
        Some((owner, slot))
    }
}

pub(super) fn scan_instance<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visit_offsets(
        base,
        visitor,
        &[offset_of!(Instance, class), offset_of!(Instance, storage)],
    );
}

pub(super) fn scan_funcallable_instance<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    scan_instance(base, visitor);
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(FuncallableInstance, function),
            offset_of!(FuncallableInstance, documentation),
        ],
    );
}

pub(super) fn scan_storage<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visitor.visit_slot(ValueSlot(base + offset_of!(InstanceStorage, layout)));
    // SAFETY: The collector supplies live initialized storage and finds its local slots here.
    let count = unsafe { (*base.to_ptr::<InstanceStorage>()).count };
    visitor.visit_array(trailing_base::<InstanceStorage>(base), count);
}

pub(super) fn scan_layout<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    // SAFETY: The collector supplies a live layout with all its entries initialized.
    let count = unsafe { (*base.to_ptr::<InstanceLayout>()).total };
    let entries = trailing_base::<InstanceLayout>(base);
    for index in 0..count {
        let entry = entries + index * size_of::<LayoutSlot>();
        visitor.visit_slot(ValueSlot(entry + offset_of!(LayoutSlot, name)));
        visitor.visit_slot(ValueSlot(entry + offset_of!(LayoutSlot, cell)));
    }
}

pub(super) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    if heap.object_ref::<Instance>(word).is_some() {
        return Some(("instance", vec![]));
    }
    if heap.object_ref::<FuncallableInstance>(word).is_some() {
        return Some(("funcallable-instance", vec![]));
    }
    if let Some(value) = heap.object_ref::<InstanceStorage>(word) {
        return Some(("instance-storage", vec![value.count as u64]));
    }
    let value = heap.object_ref::<InstanceLayout>(word)?;
    Some((
        "instance-layout",
        vec![value.total as u64, value.local as u64],
    ))
}
pub(super) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    let mut d = super::image::Data(data);
    let word = match kind {
        "instance" => {
            d.done()?;
            heap.alloc_object(|header| Instance {
                header,
                class: WordSlot::nil(),
                storage: WordSlot::nil(),
            })
        }
        "funcallable-instance" => {
            d.done()?;
            heap.alloc_object(|header| FuncallableInstance {
                base: Instance {
                    header,
                    class: WordSlot::nil(),
                    storage: WordSlot::nil(),
                },
                function: WordSlot::nil(),
                documentation: WordSlot::nil(),
            })
        }
        "instance-storage" => {
            let count = d.number()?;
            d.done()?;
            heap.alloc_with_trailing(
                count,
                |header| InstanceStorage {
                    header,
                    layout: WordSlot::nil(),
                    count,
                },
                |_| WordSlot::nil(),
            )
        }
        "instance-layout" => {
            let total = d.number()?;
            let local = d.number()?;
            d.done()?;
            if local > total {
                return Err(super::image::invalid());
            }
            heap.instance_layout(total, local, |_| (Word::NIL, None))
        }
        _ => return Ok(None),
    };
    word.map(Some)
        .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
