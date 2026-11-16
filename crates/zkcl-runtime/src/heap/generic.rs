use std::sync::atomic::AtomicBool;

use super::{
    Address, AtomicUsize, Header, Heap, HeapObject, ObjectKind, Ordering, OutOfMemory, SlotVisitor,
    Word, WordSlot, object_address, offset_of, visit_offsets,
};

#[derive(Clone, Copy)]
pub(crate) struct GenericFields {
    pub(crate) name: Word,
    pub(crate) lambda_list: Word,
    pub(crate) call_signature: GenericCallSignature,
    pub(crate) precedence: Word,
    pub(crate) combination: Word,
    pub(crate) documentation: Word,
    pub(crate) methods: Word,
}

#[derive(Clone, Copy)]
/// Calling requirements derived from the lambda list and argument precedence order.
pub(crate) struct GenericCallSignature {
    pub(crate) required: usize,
    pub(crate) positional: usize,
    pub(crate) accepts_extra: bool,
    pub(crate) allow_other_keys: bool,
    pub(crate) keywords: Option<Word>,
    pub(crate) precedence_positions: Word,
}

#[repr(C)]
struct StoredGenericCallSignature {
    required: AtomicUsize,
    positional: AtomicUsize,
    accepts_extra: AtomicBool,
    allow_other_keys: AtomicBool,
    keywords: WordSlot,
    precedence_positions: WordSlot,
}

impl StoredGenericCallSignature {
    fn new(call_signature: GenericCallSignature) -> Self {
        Self {
            required: AtomicUsize::new(call_signature.required),
            positional: AtomicUsize::new(call_signature.positional),
            accepts_extra: AtomicBool::new(call_signature.accepts_extra),
            allow_other_keys: AtomicBool::new(call_signature.allow_other_keys),
            keywords: WordSlot::new(call_signature.keywords.unwrap_or(Word::UNBOUND)),
            precedence_positions: WordSlot::new(call_signature.precedence_positions),
        }
    }

    fn set(&self, heap: &Heap, owner: Address, call_signature: GenericCallSignature) {
        self.required
            .store(call_signature.required, Ordering::Relaxed);
        self.positional
            .store(call_signature.positional, Ordering::Relaxed);
        self.accepts_extra
            .store(call_signature.accepts_extra, Ordering::Relaxed);
        self.allow_other_keys
            .store(call_signature.allow_other_keys, Ordering::Relaxed);
        heap.write_slot(
            owner,
            &self.keywords,
            call_signature.keywords.unwrap_or(Word::UNBOUND),
        );
        heap.write_slot(
            owner,
            &self.precedence_positions,
            call_signature.precedence_positions,
        );
    }

    fn get(&self) -> GenericCallSignature {
        GenericCallSignature {
            required: self.required.load(Ordering::Relaxed),
            positional: self.positional.load(Ordering::Relaxed),
            accepts_extra: self.accepts_extra.load(Ordering::Relaxed),
            allow_other_keys: self.allow_other_keys.load(Ordering::Relaxed),
            keywords: self.keywords.get_bound(),
            precedence_positions: self.precedence_positions.get(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MethodFields {
    pub(crate) qualifiers: Word,
    pub(crate) specializers: Word,
    pub(crate) lambda_list: Word,
    pub(crate) function: Word,
    pub(crate) documentation: Word,
    pub(crate) owner: Word,
    pub(crate) accessor_slot: Word,
    pub(crate) inline_definition: Word,
}

impl GenericFields {
    pub(crate) fn uninitialized(combination: Word) -> Self {
        Self {
            name: Word::NIL,
            lambda_list: Word::UNBOUND,
            call_signature: GenericCallSignature {
                required: 0,
                positional: 0,
                accepts_extra: true,
                allow_other_keys: true,
                keywords: None,
                precedence_positions: Word::NIL,
            },
            precedence: Word::NIL,
            combination,
            documentation: Word::NIL,
            methods: Word::NIL,
        }
    }
}

impl MethodFields {
    pub(crate) fn uninitialized() -> Self {
        Self {
            qualifiers: Word::NIL,
            specializers: Word::NIL,
            lambda_list: Word::NIL,
            function: Word::UNBOUND,
            documentation: Word::NIL,
            owner: Word::NIL,
            accessor_slot: Word::NIL,
            inline_definition: Word::NIL,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StandardDispatchCacheKey {
    pub(crate) argument: usize,
    pub(crate) class_identity: Word,
    pub(crate) lambda_list: Word,
    pub(crate) gc_epoch: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct CachedStandardMethods {
    pub(crate) lists: [Word; 5],
    pub(crate) direct_primary: Word,
}

const CACHED_METHOD_LIST_COUNT: usize = 5;
const STANDARD_DISPATCH_CACHE_CAPACITY: usize = 4;

#[repr(C)]
struct StandardDispatchEntry {
    // Compare the non-owning layout or built-in class identity only within
    // the collection epoch of the cache.
    class_identity: AtomicUsize,
    direct_primary: WordSlot,
    // The applicable method list followed by the four standard method groups.
    lists: [WordSlot; CACHED_METHOD_LIST_COUNT],
}

impl StandardDispatchEntry {
    fn empty() -> Self {
        Self {
            class_identity: AtomicUsize::new(0),
            direct_primary: WordSlot::nil(),
            lists: std::array::from_fn(|_| WordSlot::nil()),
        }
    }

    fn clear(&self) {
        self.class_identity.store(0, Ordering::Relaxed);
        self.direct_primary.set_unbarriered(Word::NIL);
        for slot in &self.lists {
            slot.set_unbarriered(Word::NIL);
        }
    }

    fn store(
        &self,
        heap: &Heap,
        owner: Address,
        class_identity: usize,
        methods: CachedStandardMethods,
    ) {
        self.class_identity.store(class_identity, Ordering::Relaxed);
        heap.write_slot(owner, &self.direct_primary, methods.direct_primary);
        for (slot, list) in self.lists.iter().zip(methods.lists) {
            heap.write_slot(owner, slot, list);
        }
    }

    fn methods(&self) -> CachedStandardMethods {
        CachedStandardMethods {
            lists: std::array::from_fn(|index| self.lists[index].get()),
            direct_primary: self.direct_primary.get(),
        }
    }
}

#[repr(C)]
struct LispGenericFunction {
    header: Header,
    name: WordSlot,
    lambda_list: WordSlot,
    call_signature: StoredGenericCallSignature,
    precedence: WordSlot,
    combination: WordSlot,
    documentation: WordSlot,
    methods: WordSlot,
    function_target: WordSlot,
    meta_instance: WordSlot,
    method_class: WordSlot,
    combination_object: WordSlot,
    declarations: WordSlot,
    dependents: WordSlot,
    inline_definition: WordSlot,
    dispatch_argument: AtomicUsize,
    dispatch_lambda_list: WordSlot,
    dispatch_epoch: AtomicUsize,
    // Replace in insertion order. Cache hits leave the table untouched.
    dispatch_next_entry: AtomicUsize,
    dispatch_entries: [StandardDispatchEntry; STANDARD_DISPATCH_CACHE_CAPACITY],
}

// SAFETY: `LispGenericFunction` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::GenericFunction`.
unsafe impl HeapObject for LispGenericFunction {
    const KIND: ObjectKind = ObjectKind::GenericFunction;
}

#[repr(C)]
struct LispMethod {
    header: Header,
    qualifiers: WordSlot,
    specializers: WordSlot,
    lambda_list: WordSlot,
    function: WordSlot,
    documentation: WordSlot,
    owner: WordSlot,
    accessor_slot: WordSlot,
    meta_instance: WordSlot,
    portable_function: WordSlot,
    inline_definition: WordSlot,
}

// SAFETY: `LispMethod` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::Method`.
unsafe impl HeapObject for LispMethod {
    const KIND: ObjectKind = ObjectKind::Method;
}

impl LispGenericFunction {
    fn clear_dispatch_cache(&self) {
        self.dispatch_lambda_list.set_unbarriered(Word::NIL);
        self.dispatch_next_entry.store(0, Ordering::Relaxed);
        for entry in &self.dispatch_entries {
            entry.clear();
        }
    }
}

impl Heap {
    /// Reload rooted fields after allocation. The callback must not collect.
    pub(crate) fn generic_function(
        &self,
        fields: impl FnOnce() -> GenericFields,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| {
            let info = fields();
            LispGenericFunction {
                header,
                name: WordSlot::new(info.name),
                lambda_list: WordSlot::new(info.lambda_list),
                call_signature: StoredGenericCallSignature::new(info.call_signature),
                precedence: WordSlot::new(info.precedence),
                combination: WordSlot::new(info.combination),
                documentation: WordSlot::new(info.documentation),
                methods: WordSlot::new(info.methods),
                function_target: WordSlot::nil(),
                meta_instance: WordSlot::nil(),
                method_class: WordSlot::nil(),
                combination_object: WordSlot::nil(),
                declarations: WordSlot::nil(),
                dependents: WordSlot::nil(),
                inline_definition: WordSlot::nil(),
                dispatch_argument: AtomicUsize::new(0),
                dispatch_lambda_list: WordSlot::nil(),
                dispatch_epoch: AtomicUsize::new(0),
                dispatch_next_entry: AtomicUsize::new(0),
                dispatch_entries: std::array::from_fn(|_| StandardDispatchEntry::empty()),
            }
        })
    }

    pub(crate) fn generic_function_target(&self, word: Word) -> Option<Word> {
        Some(
            self.object_ref::<LispGenericFunction>(word)?
                .function_target
                .get(),
        )
    }

    pub(crate) fn set_generic_function_target(&self, word: Word, function: Word) {
        let generic = self.expect_object::<LispGenericFunction>(word, "generic function");
        self.write_slot_object(generic, &generic.function_target, function);
    }

    pub(crate) fn generic_info(&self, word: Word) -> Option<GenericFields> {
        let generic = self.object_ref::<LispGenericFunction>(word)?;
        Some(GenericFields {
            name: generic.name.get(),
            lambda_list: generic.lambda_list.get(),
            call_signature: generic.call_signature.get(),
            precedence: generic.precedence.get(),
            combination: generic.combination.get(),
            documentation: generic.documentation.get(),
            methods: generic.methods.get(),
        })
    }

    pub(crate) fn generic_call_signature(&self, word: Word) -> Option<GenericCallSignature> {
        Some(
            self.object_ref::<LispGenericFunction>(word)?
                .call_signature
                .get(),
        )
    }

    /// `class_at` reads the identity without reaching a Lisp safepoint.
    pub(crate) fn cached_standard_dispatch(
        &self,
        word: Word,
        lambda_list: Word,
        class_at: impl FnOnce(usize) -> Option<Word>,
    ) -> Option<CachedStandardMethods> {
        let generic = self.expect_object::<LispGenericFunction>(word, "generic function");
        if generic.dispatch_lambda_list.get() != lambda_list
            || generic.dispatch_epoch.load(Ordering::Relaxed) != self.gc_epoch()
        {
            return None;
        }
        let argument = generic.dispatch_argument.load(Ordering::Relaxed);
        // Class identity lookup reads committed layout or package state without
        // collecting. A class redefinition supplies a fresh committed layout.
        let identity = class_at(argument)?;
        if identity == Word::UNBOUND {
            return None;
        }
        generic
            .dispatch_entries
            .iter()
            .find(|entry| entry.class_identity.load(Ordering::Relaxed) == identity.0)
            .map(StandardDispatchEntry::methods)
    }

    pub(crate) fn cache_standard_dispatch(
        &self,
        word: Word,
        key: StandardDispatchCacheKey,
        cached_methods: CachedStandardMethods,
    ) {
        let owner = word.object_address().expect("generic function");
        let generic = self.expect_object::<LispGenericFunction>(word, "generic function");
        if generic.dispatch_epoch.load(Ordering::Relaxed) != key.gc_epoch
            || generic.dispatch_lambda_list.get() != key.lambda_list
            || generic.dispatch_argument.load(Ordering::Relaxed) != key.argument
        {
            generic.clear_dispatch_cache();
            self.write_slot(owner, &generic.dispatch_lambda_list, key.lambda_list);
            generic
                .dispatch_epoch
                .store(key.gc_epoch, Ordering::Relaxed);
            generic
                .dispatch_argument
                .store(key.argument, Ordering::Relaxed);
        }
        let entry = generic.dispatch_next_entry.load(Ordering::Relaxed);
        generic.dispatch_entries[entry].store(self, owner, key.class_identity.0, cached_methods);
        generic.dispatch_next_entry.store(
            (entry + 1) % STANDARD_DISPATCH_CACHE_CAPACITY,
            Ordering::Relaxed,
        );
    }

    pub(crate) fn invalidate_standard_dispatch(&self, word: Word) {
        self.expect_object::<LispGenericFunction>(word, "generic function")
            .clear_dispatch_cache();
    }

    pub(crate) fn set_generic_definition(
        &self,
        word: Word,
        lambda_list: Word,
        call_signature: GenericCallSignature,
        precedence: Word,
        combination: Word,
        documentation: Word,
    ) {
        let generic = self.expect_object::<LispGenericFunction>(word, "generic function");
        let owner = word.object_address().expect("generic function");
        self.write_slot_object(generic, &generic.lambda_list, lambda_list);
        generic.call_signature.set(self, owner, call_signature);
        self.write_slot_object(generic, &generic.precedence, precedence);
        self.write_slot_object(generic, &generic.combination, combination);
        self.write_slot_object(generic, &generic.documentation, documentation);
        generic.clear_dispatch_cache();
    }

    pub(crate) fn set_generic_documentation(&self, word: Word, documentation: Word) {
        let generic = self.expect_object::<LispGenericFunction>(word, "generic function");
        self.write_slot_object(generic, &generic.documentation, documentation);
    }

    pub(crate) fn set_generic_methods(&self, word: Word, methods: Word) {
        let generic = self.expect_object::<LispGenericFunction>(word, "generic function");
        self.write_slot_object(generic, &generic.methods, methods);
        generic.clear_dispatch_cache();
    }

    /// Reload rooted fields after allocation. The callback must not collect.
    pub(crate) fn method(
        &self,
        fields: impl FnOnce() -> MethodFields,
    ) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| {
            let info = fields();
            LispMethod {
                header,
                qualifiers: WordSlot::new(info.qualifiers),
                specializers: WordSlot::new(info.specializers),
                lambda_list: WordSlot::new(info.lambda_list),
                function: WordSlot::new(info.function),
                documentation: WordSlot::new(info.documentation),
                owner: WordSlot::new(info.owner),
                accessor_slot: WordSlot::new(info.accessor_slot),
                meta_instance: WordSlot::nil(),
                portable_function: WordSlot::nil(),
                inline_definition: WordSlot::new(info.inline_definition),
            }
        })
    }

    pub(crate) fn method_info(&self, word: Word) -> Option<MethodFields> {
        let method = self.object_ref::<LispMethod>(word)?;
        Some(MethodFields {
            qualifiers: method.qualifiers.get(),
            specializers: method.specializers.get(),
            lambda_list: method.lambda_list.get(),
            function: method.function.get(),
            documentation: method.documentation.get(),
            owner: method.owner.get(),
            accessor_slot: method.accessor_slot.get(),
            inline_definition: method.inline_definition.get(),
        })
    }

    pub(crate) fn generic_inline_definition(
        &self,
        generic: Word,
        new: Option<Word>,
    ) -> Option<Word> {
        let object = self.object_ref::<LispGenericFunction>(generic)?;
        if let Some(value) = new {
            self.write_slot_object(object, &object.inline_definition, value);
            Some(value)
        } else {
            Some(object.inline_definition.get())
        }
    }

    pub(crate) fn generic_options(&self, word: Word) -> Option<[Word; 3]> {
        let gf = self.object_ref::<LispGenericFunction>(word)?;
        Some([&gf.method_class, &gf.combination_object, &gf.declarations].map(WordSlot::get))
    }

    pub(crate) fn remember_combination(&self, word: Word, combination: Word) -> Option<Word> {
        let gf = self.object_ref::<LispGenericFunction>(word)?;
        Some(
            match self.compare_exchange_slot(gf, &gf.combination_object, Word::NIL, combination) {
                Ok(_) => combination,
                Err(installed) => installed,
            },
        )
    }

    pub(crate) fn set_generic_options(
        &self,
        word: Word,
        name: Word,
        method_class: Word,
        combination: Word,
        declarations: Word,
    ) {
        let gf = self.expect_object::<LispGenericFunction>(word, "generic function");
        for (field, value) in [
            (&gf.name, name),
            (&gf.method_class, method_class),
            (&gf.combination_object, combination),
            (&gf.declarations, declarations),
        ] {
            self.write_slot_object(gf, field, value);
        }
    }

    pub(crate) fn initialize_method(&self, word: Word, info: MethodFields, portable: Word) {
        let m = self.expect_object::<LispMethod>(word, "method");
        for (field, value) in [
            (&m.qualifiers, info.qualifiers),
            (&m.specializers, info.specializers),
            (&m.lambda_list, info.lambda_list),
            (&m.function, info.function),
            (&m.documentation, info.documentation),
            (&m.accessor_slot, info.accessor_slot),
            (&m.portable_function, portable),
            (&m.inline_definition, info.inline_definition),
        ] {
            self.write_slot_object(m, field, value);
        }
    }

    pub(crate) fn remember_method_function(&self, word: Word, function: Word) -> Option<Word> {
        let method = self.object_ref::<LispMethod>(word)?;
        Some(
            match self.compare_exchange_slot(method, &method.portable_function, Word::NIL, function)
            {
                Ok(_) => function,
                Err(installed) => installed,
            },
        )
    }

    pub(crate) fn portable_method_function(&self, word: Word) -> Option<Word> {
        Some(self.object_ref::<LispMethod>(word)?.portable_function.get())
    }

    pub(super) fn generic_meta_slot(&self, word: Word) -> Option<Address> {
        if let Some(base) = object_address(word, ObjectKind::GenericFunction) {
            return Some(base + offset_of!(LispGenericFunction, meta_instance));
        }
        Some(object_address(word, ObjectKind::Method)? + offset_of!(LispMethod, meta_instance))
    }

    pub(super) fn gf_dependents_slot(&self, word: Word) -> Option<Address> {
        Some(
            object_address(word, ObjectKind::GenericFunction)?
                + offset_of!(LispGenericFunction, dependents),
        )
    }

    pub(crate) fn set_method_documentation(&self, word: Word, documentation: Word) {
        let method = self.expect_object::<LispMethod>(word, "method");
        self.write_slot_object(method, &method.documentation, documentation);
    }

    pub(crate) fn set_method_owner(&self, word: Word, owner: Word) {
        let method = self.expect_object::<LispMethod>(word, "method");
        self.write_slot_object(method, &method.owner, owner);
    }
}

/// Persist the generic function's definitions and metadata. Dispatch entries
/// are derived for the current heap and start empty after restoring an image.
pub(super) fn scan_generic_state<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(LispGenericFunction, name),
            offset_of!(LispGenericFunction, lambda_list),
            offset_of!(LispGenericFunction, call_signature.keywords),
            offset_of!(LispGenericFunction, call_signature.precedence_positions),
            offset_of!(LispGenericFunction, precedence),
            offset_of!(LispGenericFunction, combination),
            offset_of!(LispGenericFunction, documentation),
            offset_of!(LispGenericFunction, methods),
            offset_of!(LispGenericFunction, function_target),
            offset_of!(LispGenericFunction, meta_instance),
            offset_of!(LispGenericFunction, method_class),
            offset_of!(LispGenericFunction, combination_object),
            offset_of!(LispGenericFunction, declarations),
            offset_of!(LispGenericFunction, dependents),
            offset_of!(LispGenericFunction, inline_definition),
        ],
    );
}

pub(super) fn scan_generic<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    scan_generic_state(base, visitor);
    let cache = base + offset_of!(LispGenericFunction, dispatch_entries);
    for index in 0..STANDARD_DISPATCH_CACHE_CAPACITY {
        let entry = cache + index * size_of::<StandardDispatchEntry>();
        visit_offsets(
            entry,
            visitor,
            &[offset_of!(StandardDispatchEntry, direct_primary)],
        );
        visitor.visit_array(
            entry + offset_of!(StandardDispatchEntry, lists),
            CACHED_METHOD_LIST_COUNT,
        );
    }
}

pub(super) fn scan_method<SV: SlotVisitor>(base: Address, visitor: &mut SV) {
    visit_offsets(
        base,
        visitor,
        &[
            offset_of!(LispMethod, qualifiers),
            offset_of!(LispMethod, specializers),
            offset_of!(LispMethod, lambda_list),
            offset_of!(LispMethod, function),
            offset_of!(LispMethod, documentation),
            offset_of!(LispMethod, owner),
            offset_of!(LispMethod, accessor_slot),
            offset_of!(LispMethod, portable_function),
            offset_of!(LispMethod, meta_instance),
            offset_of!(LispMethod, inline_definition),
        ],
    );
}

#[cfg(test)]
pub(crate) fn check_generic_storage(heap: &Heap) {
    let fields = heap.root_frame(10);
    for marker in 0..10 {
        let number = Word::fixnum(marker).unwrap();
        fields.set(marker as usize, heap.cons(|| (number, Word::NIL)).unwrap());
    }

    let generic = heap.root_slot(
        heap.generic_function(|| GenericFields {
            name: fields.get(0),
            lambda_list: Word::NIL,
            call_signature: GenericCallSignature {
                required: 0,
                positional: 0,
                accepts_extra: false,
                allow_other_keys: false,
                keywords: None,
                precedence_positions: Word::NIL,
            },
            precedence: Word::NIL,
            combination: Word::NIL,
            documentation: Word::NIL,
            methods: Word::NIL,
        })
        .unwrap(),
    );
    let method = heap.root_slot(
        heap.method(|| MethodFields {
            qualifiers: fields.get(5),
            specializers: fields.get(6),
            lambda_list: fields.get(7),
            function: fields.get(8),
            documentation: fields.get(9),
            owner: Word::NIL,
            accessor_slot: Word::NIL,
            inline_definition: fields.get(5),
        })
        .unwrap(),
    );
    let methods = heap.cons(|| (method.get(), Word::NIL)).unwrap();
    heap.set_generic_methods(generic.get(), methods);
    heap.set_generic_definition(
        generic.get(),
        fields.get(1),
        GenericCallSignature {
            required: 1,
            positional: 2,
            accepts_extra: true,
            allow_other_keys: false,
            keywords: Some(fields.get(6)),
            precedence_positions: fields.get(8),
        },
        fields.get(2),
        fields.get(3),
        fields.get(4),
    );
    heap.set_method_owner(method.get(), generic.get());
    heap.generic_inline_definition(generic.get(), Some(fields.get(5)))
        .unwrap();

    let (old_generic, old_method) = (generic.get(), method.get());
    drop(fields);
    drop(generic);
    // Only the method root keeps the generic, cycle, and field objects live.
    heap.collect();
    let method_info = heap.method_info(method.get()).unwrap();
    let generic_info = heap.generic_info(method_info.owner).unwrap();
    assert_ne!(method.get(), old_method);
    assert_ne!(method_info.owner, old_generic);
    assert_eq!(method_info.inline_definition, method_info.qualifiers);
    assert_eq!(
        heap.generic_inline_definition(method_info.owner, None),
        Some(method_info.inline_definition)
    );
    assert_eq!(generic_info.call_signature.required, 1);
    assert_eq!(generic_info.call_signature.positional, 2);
    assert!(generic_info.call_signature.accepts_extra);
    assert!(!generic_info.call_signature.allow_other_keys);
    assert_eq!(
        generic_info.call_signature.keywords,
        Some(method_info.specializers)
    );
    assert_eq!(
        generic_info.call_signature.precedence_positions,
        method_info.function
    );
    assert_eq!(
        heap.cons_fields(generic_info.methods),
        Some((method.get(), Word::NIL))
    );
    for (marker, field) in [
        generic_info.name,
        generic_info.lambda_list,
        generic_info.precedence,
        generic_info.combination,
        generic_info.documentation,
        method_info.qualifiers,
        method_info.specializers,
        method_info.lambda_list,
        method_info.function,
        method_info.documentation,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            heap.cons_fields(field),
            Some((Word::fixnum(marker as isize).unwrap(), Word::NIL))
        );
    }
}

pub(super) fn image_shape(heap: &Heap, word: Word) -> Option<(&'static str, Vec<u64>)> {
    if heap.object_ref::<LispMethod>(word).is_some() {
        return Some(("method", vec![]));
    }
    let value = heap.object_ref::<LispGenericFunction>(word)?;
    let sig = &value.call_signature;
    Some((
        "generic-function",
        vec![
            sig.required.load(Ordering::Relaxed) as u64,
            sig.positional.load(Ordering::Relaxed) as u64,
            sig.accepts_extra.load(Ordering::Relaxed) as u64,
            sig.allow_other_keys.load(Ordering::Relaxed) as u64,
        ],
    ))
}
pub(super) fn image_allocate(
    heap: &Heap,
    kind: &str,
    data: &[u64],
) -> Result<Option<Word>, crate::EvalError> {
    let mut d = super::image::Data(data);
    let word = match kind {
        "method" => {
            d.done()?;
            heap.method(MethodFields::uninitialized)
        }
        "generic-function" => {
            let required = d.number()?;
            let positional = d.number()?;
            let accepts_extra = d.flag()?;
            let allow_other_keys = d.flag()?;
            d.done()?;
            if required > positional {
                return Err(super::image::invalid());
            }
            heap.generic_function(|| {
                let mut fields = GenericFields::uninitialized(Word::NIL);
                fields.call_signature.required = required;
                fields.call_signature.positional = positional;
                fields.call_signature.accepts_extra = accepts_extra;
                fields.call_signature.allow_other_keys = allow_other_keys;
                fields
            })
        }
        _ => return Ok(None),
    };
    word.map(Some)
        .map_err(|e| crate::EvalError::out_of_memory(0, e))
}
