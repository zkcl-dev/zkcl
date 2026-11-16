//! Type-safe heap object descriptors, atomic word slots, and safepoint-checked borrowing.

use std::mem::DropGuard;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{
    Address, Header, Heap, ObjectKind, OutOfMemory, SlotVisitor, ValueSlot, Word, object_address,
};

/// A GC-managed heap object whose `#[repr(C)]` layout starts with [`Header`].
///
/// # Safety
/// - `Self` must be `#[repr(C)]` with `header: Header` at offset 0.
/// - Every allocated instance on the heap must carry `Self::KIND` in its header.
pub(super) unsafe trait HeapObject: Sized {
    const KIND: ObjectKind;
}

/// Atomic Lisp [`Word`] slot embedded in a GC-managed object.
#[derive(Default)]
#[repr(transparent)]
pub(super) struct WordSlot(AtomicUsize);

impl WordSlot {
    #[inline(always)]
    pub(super) const fn new(word: Word) -> Self {
        Self(AtomicUsize::new(word.0))
    }

    #[inline(always)]
    pub(super) const fn unbound() -> Self {
        Self(AtomicUsize::new(Word::UNBOUND.0))
    }

    #[inline(always)]
    pub(super) const fn nil() -> Self {
        Self(AtomicUsize::new(Word::NIL.0))
    }

    #[inline(always)]
    pub(super) fn get(&self) -> Word {
        Word(self.0.load(Ordering::Relaxed))
    }

    #[inline(always)]
    pub(super) fn get_bound(&self) -> Option<Word> {
        let value = self.get();
        (value != Word::UNBOUND).then_some(value)
    }

    #[inline(always)]
    pub(super) fn get_non_nil(&self) -> Option<Word> {
        let value = self.get();
        (value != Word::NIL).then_some(value)
    }

    #[inline(always)]
    pub(super) fn set_unbarriered(&self, word: Word) {
        self.0.store(word.0, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(super) fn compare_exchange_unbarriered(&self, expected: Word, new: Word) -> Result<Word, Word> {
        self.0
            .compare_exchange(expected.0, new.0, Ordering::Relaxed, Ordering::Relaxed)
            .map(Word)
            .map_err(Word)
    }

    #[inline(always)]
    pub(super) fn load_ordered(&self, order: Ordering) -> Word {
        Word(self.0.load(order))
    }

    #[inline(always)]
    pub(super) fn store_ordered_unbarriered(&self, word: Word, order: Ordering) {
        self.0.store(word.0, order);
    }

    #[inline(always)]
    pub(super) fn compare_exchange_ordered_unbarriered(
        &self,
        expected: Word,
        new: Word,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Word, Word> {
        self.0
            .compare_exchange(expected.0, new.0, success, failure)
            .map(Word)
            .map_err(Word)
    }
}

/// Compute the byte size of `T` followed by `count` trailing `E` elements, aligned to 8 bytes.
#[inline]
pub(super) fn trailing_object_size<T, E>(count: usize) -> Result<usize, OutOfMemory> {
    const {
        assert!(size_of::<T>().is_multiple_of(align_of::<E>()));
    }
    count
        .checked_mul(size_of::<E>())
        .and_then(|bytes| size_of::<T>().checked_add(bytes))
        .and_then(|size| size.checked_next_multiple_of(8))
        .ok_or(OutOfMemory)
}

/// Return the trailing payload address immediately following `T`.
#[inline(always)]
pub(super) fn trailing_base<T>(address: Address) -> Address {
    address + size_of::<T>()
}

/// Visit each word-slot byte offset on `base` during GC tracing.
#[inline(always)]
pub(super) fn visit_offsets(base: Address, visitor: &mut impl SlotVisitor, offsets: &[usize]) {
    for &offset in offsets {
        visitor.visit_slot(ValueSlot(base + offset));
    }
}

impl Heap {
    /// Validate `word`'s object kind and borrow `T` for a safepoint-free operation.
    #[inline(always)]
    pub(super) fn object_ref<T: HeapObject>(&self, word: Word) -> Option<&T> {
        let address = object_address(word, T::KIND)?;
        // SAFETY: `object_address` verified `T::KIND` and no safepoint occurs while `&T` lives.
        Some(unsafe { &*address.to_ptr::<T>() })
    }

    /// Borrow `word` as `T`, returning both its validated heap `Address` and `&T`.
    #[inline(always)]
    pub(super) fn object_with_address<T: HeapObject>(&self, word: Word) -> Option<(Address, &T)> {
        let address = object_address(word, T::KIND)?;
        // SAFETY: `object_address` verified `T::KIND` and no safepoint occurs while `&T` lives.
        Some((address, unsafe { &*address.to_ptr::<T>() }))
    }

    /// Borrow a validated heap object, panicking with `expectation` if `word` has another kind.
    #[inline(always)]
    pub(super) fn expect_object<T: HeapObject>(&self, word: Word, expectation: &str) -> &T {
        self.object_ref(word).expect(expectation)
    }

    /// Allocate and initialize a fixed-size [`HeapObject`].
    /// `build` runs after allocation (and any GC), so it may read freshly moved `RootSlot`s.
    #[inline]
    pub(super) fn alloc_object<T: HeapObject>(
        &self,
        build: impl FnOnce(Header) -> T,
    ) -> Result<Word, OutOfMemory> {
        let size = size_of::<T>();
        let address = self.allocate(size)?;
        let value = build(Header::new(T::KIND, size));
        // SAFETY: `self.allocate(size)` returns a fresh, aligned block of `size` bytes.
        unsafe { address.to_mut_ptr::<T>().write(value) };
        Ok(Word::from_object_address(address))
    }

    /// Allocate `T` followed by `count` inline trailing elements of type `E`.
    #[inline]
    pub(super) fn alloc_with_trailing<T: HeapObject, E>(
        &self,
        count: usize,
        build: impl FnOnce(Header) -> T,
        mut init_element: impl FnMut(usize) -> E,
    ) -> Result<Word, OutOfMemory> {
        let size = trailing_object_size::<T, E>(count)?;
        let address = self.allocate(size)?;
        let header = build(Header::new(T::KIND, size));
        // SAFETY: `size` holds `T` and `count` aligned elements of `E`.
        unsafe {
            address.to_mut_ptr::<T>().write(header);
            let slots = trailing_base::<T>(address).to_mut_ptr::<E>();
            for index in 0..count {
                slots.add(index).write(init_element(index));
            }
        }
        Ok(Word::from_object_address(address))
    }

    /// Borrow one element from a validated `T` object's trailing `E` array.
    #[inline(always)]
    pub(super) fn trailing_ref<T: HeapObject, E>(
        &self,
        word: Word,
        count: impl FnOnce(&T) -> usize,
        index: usize,
    ) -> Option<&E> {
        let (address, header) = self.object_with_address::<T>(word)?;
        if index >= count(header) {
            return None;
        }
        // SAFETY: `object_with_address` verified `T`, `count` bounds `index`, and `&self` forbids GC moves.
        Some(unsafe { &*(trailing_base::<T>(address) + index * size_of::<E>()).to_ptr::<E>() })
    }

    /// Borrow the trailing `E` slice of a validated `T` object for a safepoint-free access.
    #[inline(always)]
    pub(super) fn trailing_slice<T: HeapObject, E>(
        &self,
        word: Word,
        count: impl FnOnce(&T) -> usize,
    ) -> Option<&[E]> {
        let (address, header) = self.object_with_address::<T>(word)?;
        let len = count(header);
        // SAFETY: `alloc_with_trailing` guarantees `len` initialized `E` elements follow `T`.
        Some(unsafe { std::slice::from_raw_parts(trailing_base::<T>(address).to_ptr::<E>(), len) })
    }

    /// Read a traced word at a validated `Address` offset.
    ///
    /// # Safety
    /// `slot` must point to a live, initialized `AtomicUsize` word slot.
    #[inline(always)]
    pub(super) unsafe fn load_word_slot(slot: Address) -> Word {
        // SAFETY: Caller guarantees `slot` points to a live `AtomicUsize`.
        Word(unsafe { &*slot.to_ptr::<AtomicUsize>() }.load(Ordering::Relaxed))
    }

    /// Write a traced word at a validated `Address` offset without a write barrier.
    ///
    /// # Safety
    /// `slot` must point to a live, traced `AtomicUsize` word slot, and the caller must
    /// either keep the enclosing object young / freshly allocated or run [`Self::write_barrier`].
    #[inline(always)]
    pub(super) unsafe fn store_word_slot_unbarriered(slot: Address, value: Word) {
        // SAFETY: Caller guarantees `slot` points to a live, traced `AtomicUsize`.
        unsafe { &*slot.to_ptr::<AtomicUsize>() }.store(value.0, Ordering::Relaxed);
    }

    /// Trigger the object-remembering write barrier after storing `value` into `owner`.
    #[inline(always)]
    pub(super) fn write_barrier(&self, owner: Address, value: Word) {
        if !value.is_heap_object() {
            return;
        }
        // SAFETY: `owner` is a live heap object whose header remains valid until collection.
        let header = unsafe { &*owner.to_ptr::<Header>() };
        const MATURE_UNLOGGED: u8 = super::word::GC_FLAG_MATURE | super::word::GC_FLAG_UNLOGGED;
        if header.gc_flags() & MATURE_UNLOGGED == MATURE_UNLOGGED {
            self.record_remembered_object(owner, header);
        }
    }

    #[cold]
    #[inline(never)]
    fn record_remembered_object(&self, owner: Address, header: &Header) {
        if header.try_log() {
            self.remembered_objects.borrow_mut().push(owner);
        }
    }

    #[inline(always)]
    pub(super) fn write_slot(&self, owner: Address, slot: &WordSlot, value: Word) {
        slot.set_unbarriered(value);
        self.write_barrier(owner, value);
    }

    #[inline(always)]
    pub(super) fn write_slot_object<T: HeapObject>(&self, owner: &T, slot: &WordSlot, value: Word) {
        self.write_slot(Address::from_ptr(owner), slot, value);
    }

    /// Write `value` into raw traced `slot` belonging to heap object `owner`, with barrier.
    ///
    /// # Safety
    /// `owner` must be the enclosing heap object address and `slot` must belong to its traced slots.
    #[inline(always)]
    pub(super) unsafe fn write_word_slot(&self, owner: Address, slot: Address, value: Word) {
        // SAFETY: Caller guarantees `slot` is a live traced `AtomicUsize` owned by `owner`.
        unsafe { Self::store_word_slot_unbarriered(slot, value) };
        self.write_barrier(owner, value);
    }

    #[inline(always)]
    pub(super) fn compare_exchange_slot<T: HeapObject>(
        &self,
        owner: &T,
        slot: &WordSlot,
        expected: Word,
        new: Word,
    ) -> Result<Word, Word> {
        let result = slot.compare_exchange_unbarriered(expected, new);
        if result.is_ok() {
            self.write_barrier(Address::from_ptr(owner), new);
        }
        result
    }

    #[inline(always)]
    pub(super) fn atomic_store_slot<T: HeapObject>(
        &self,
        owner: &T,
        slot: &WordSlot,
        value: Word,
        order: Ordering,
    ) {
        slot.store_ordered_unbarriered(value, order);
        self.write_barrier(Address::from_ptr(owner), value);
    }

    #[inline(always)]
    pub(super) fn atomic_cas_slot<T: HeapObject>(
        &self,
        owner: &T,
        slot: &WordSlot,
        expected: Word,
        new: Word,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Word, Word> {
        let result = slot.compare_exchange_ordered_unbarriered(expected, new, success, failure);
        if result.is_ok() {
            self.write_barrier(Address::from_ptr(owner), new);
        }
        result
    }

    #[inline(always)]
    pub(super) fn enter_no_gc(&self) -> impl Drop + '_ {
        let previous = self.no_gc_depth.get();
        self.no_gc_depth.set(previous + 1);
        DropGuard::new(&self.no_gc_depth, move |depth| depth.set(previous))
    }

    #[inline(always)]
    pub(super) fn assert_may_collect(&self) {
        debug_assert_eq!(
            self.no_gc_depth.get(),
            0,
            "heap allocation/GC/mutation is forbidden while borrowing a heap slice"
        );
    }

    /// Borrow a Lisp string's UTF-8 contents for the duration of `read`.
    pub(crate) fn with_string_slice<R>(
        &self,
        word: Word,
        read: impl FnOnce(&str) -> R,
    ) -> Option<R> {
        let _guard = self.enter_no_gc();
        // SAFETY: `_guard` checks in debug builds that `read` never triggers GC or mutates strings.
        let text = unsafe { self.string_slice(word) }?;
        Some(read(text))
    }

    /// Borrow two Lisp strings' UTF-8 contents simultaneously for comparison or inspection.
    pub(crate) fn with_two_string_slices<R>(
        &self,
        left: Word,
        right: Word,
        read: impl FnOnce(&str, &str) -> R,
    ) -> Option<R> {
        let _guard = self.enter_no_gc();
        // SAFETY: `_guard` checks in debug builds that `read` never triggers GC or mutates strings.
        let left = unsafe { self.string_slice(left) }?;
        let right = unsafe { self.string_slice(right) }?;
        Some(read(left, right))
    }

    /// Borrow a row-major character range from a string or character array for `read`.
    pub(crate) fn with_array_character_slice<R>(
        &self,
        word: Word,
        start: usize,
        length: usize,
        read: impl FnOnce(&str) -> R,
    ) -> Option<R> {
        let _guard = self.enter_no_gc();
        // SAFETY: `_guard` checks in debug builds that `read` never triggers GC or mutates strings.
        let text = unsafe { self.array_character_slice(word, start, length) }?;
        Some(read(text))
    }

    /// Borrow a row-major octet range for the duration of `read`.
    pub(crate) fn with_array_octet_slice<R>(
        &self,
        word: Word,
        start: usize,
        length: usize,
        read: impl FnOnce(&[u8]) -> R,
    ) -> Option<R> {
        let _guard = self.enter_no_gc();
        let address = self.octet_array_address(word, start, length)?;
        // SAFETY: The range is checked, and `read` must not collect or modify the heap.
        let bytes = unsafe { std::slice::from_raw_parts(address, length) };
        Some(read(bytes))
    }
}
