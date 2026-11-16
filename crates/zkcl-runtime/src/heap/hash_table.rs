use std::mem::offset_of;
use std::sync::atomic::{AtomicUsize, Ordering};

use zkcl_bytecode::{HashTableWeakness, RehashHint};

use crate::OutOfMemory;

use super::{
    Address, Header, Heap, HeapObject, ObjectKind, SlotVisitor, ValueSlot, Word, WordSlot,
    object_address, trailing_base,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum HashTest {
    Eq,
    Eql,
    Equal,
    Equalp,
}

#[derive(Clone, Copy)]
pub(crate) struct HashTableLayout {
    pub(crate) test: HashTest,
    pub(crate) weakness: Option<HashTableWeakness>,
    pub(crate) count: usize,
    pub(crate) size: usize,
    pub(crate) rehash_size: RehashHint,
    pub(crate) rehash_threshold: RehashHint,
}

#[repr(C)]
struct HashTable {
    header: Header,
    buckets: WordSlot,
    count: AtomicUsize,
    used: AtomicUsize,
    test: HashTest,
    weakness: Option<HashTableWeakness>,
    rehash_size: RehashHint,
    rehash_threshold: RehashHint,
}

// SAFETY: `HashTable` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::HashTable`.
unsafe impl HeapObject for HashTable {
    const KIND: ObjectKind = ObjectKind::HashTable;
}

#[repr(C)]
struct HashBuckets {
    header: Header,
    capacity: usize,
    weak: bool,
    // Followed by capacity inline buckets.
}

// SAFETY: `HashBuckets` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::HashBuckets`.
unsafe impl HeapObject for HashBuckets {
    const KIND: ObjectKind = ObjectKind::HashBuckets;
}

#[repr(C)]
struct Bucket {
    key: WordSlot,
    value: WordSlot,
    hash: AtomicUsize,
}

impl Bucket {
    fn empty() -> Self {
        Self {
            key: WordSlot::unbound(),
            value: WordSlot::unbound(),
            hash: AtomicUsize::new(0),
        }
    }

    fn key(&self) -> Word {
        self.key.get()
    }

    fn value(&self) -> Word {
        self.value.get()
    }

    fn put_unbarriered(&self, key: Word, value: Word, hash: usize) {
        self.key.set_unbarriered(key);
        self.value.set_unbarriered(value);
        self.hash.store(hash, Ordering::Relaxed);
    }
}

impl HashTable {
    fn buckets(&self) -> &[Bucket] {
        // SAFETY: this live table owns the initialized buckets, and their
        // callers finish using the slice before reaching any safepoint.
        unsafe { &*bucket_slice(self.buckets.get()) }
    }
}

#[derive(Clone, Copy)]
enum Position {
    Present(usize),
    Vacant { index: usize, empty: bool },
}

const MIN_BUCKETS: usize = 8;

/// The load factor used for sizing. Thresholds below a quarter would make
/// every insertion rebuild the table (and a zero threshold can never be
/// satisfied), so the stored value is reported unchanged while growth uses
/// this floor. Capacity and usable slots must share it or growth never catches
/// up with the entry count.
const MIN_REHASH_THRESHOLD: f64 = 0.25;

fn threshold_fraction(threshold: RehashHint) -> f64 {
    threshold.as_f64().clamp(MIN_REHASH_THRESHOLD, 1.0)
}

fn capacity_for(size: usize, threshold: RehashHint) -> Result<usize, OutOfMemory> {
    let fraction = threshold_fraction(threshold);
    let requested = (size as f64 / fraction).ceil();
    if !requested.is_finite() || requested > usize::MAX as f64 {
        return Err(OutOfMemory);
    }
    // The occupied probe chains must always leave at least one empty slot.
    (requested as usize)
        .max(MIN_BUCKETS)
        .checked_next_power_of_two()
        .ok_or(OutOfMemory)
}

fn usable(capacity: usize, threshold: RehashHint) -> usize {
    let slots = (capacity as f64 * threshold_fraction(threshold)).floor();
    if !slots.is_finite() {
        return capacity - 1;
    }
    (slots as usize).clamp(1, capacity - 1)
}

fn rehash_target(rehash_size: RehashHint, count: usize) -> Result<usize, OutOfMemory> {
    if let Some(step) = rehash_size.as_integer() {
        return count
            .checked_add(usize::try_from(step).map_err(|_| OutOfMemory)?)
            .ok_or(OutOfMemory);
    }
    let scale = rehash_size.as_f64();
    let requested = (count as f64 * scale).ceil();
    if !requested.is_finite() || requested > usize::MAX as f64 {
        return Err(OutOfMemory);
    }
    Ok(requested as usize)
}

fn bucket_slice(word: Word) -> *const [Bucket] {
    let address = object_address(word, ObjectKind::HashBuckets).expect("hash buckets");
    // SAFETY: object_address checked the live object's kind.
    let count = unsafe { (*address.to_ptr::<HashBuckets>()).capacity };
    let base = address + size_of::<HashBuckets>();
    std::ptr::slice_from_raw_parts(base.to_ptr::<Bucket>(), count)
}

fn position(
    buckets: &[Bucket],
    key: Word,
    hash: usize,
    mut same: impl FnMut(Word, Word) -> bool,
) -> Position {
    let mask = buckets.len() - 1;
    let mut index = hash & mask;
    let mut tombstone = None;
    loop {
        let bucket = &buckets[index];
        let candidate = bucket.key();
        if candidate == Word::UNBOUND {
            // An unbound key with a NIL value is a deleted bucket. Retaining
            // it in the probe chain lets searches reach entries beyond it.
            if bucket.value() == Word::UNBOUND {
                return match tombstone {
                    Some(index) => Position::Vacant {
                        index,
                        empty: false,
                    },
                    None => Position::Vacant { index, empty: true },
                };
            }
            tombstone.get_or_insert(index);
        } else if bucket.hash.load(Ordering::Relaxed) == hash && same(candidate, key) {
            return Position::Present(index);
        }
        index = (index + 1) & mask;
    }
}

fn insert_fresh(buckets: &[Bucket], key: Word, value: Word, hash: usize) {
    let mask = buckets.len() - 1;
    let mut index = hash & mask;
    while buckets[index].key() != Word::UNBOUND {
        index = (index + 1) & mask;
    }
    buckets[index].put_unbarriered(key, value, hash);
}

impl Heap {
    fn hash_table_ref(&self, word: Word) -> Option<&HashTable> {
        self.object_ref::<HashTable>(word)
    }

    pub(crate) fn word_identity_hash(word: Word) -> usize {
        let Some(address) = word.object_address() else {
            return word.0;
        };
        static NEXT_IDENTITY: AtomicUsize = AtomicUsize::new(1);
        // SAFETY: this header is live and identity assignment cannot reach a safepoint.
        let header = unsafe { &*address.to_ptr::<Header>() };
        let identity = header.identity.load(Ordering::Relaxed);
        if identity != 0 {
            return identity;
        }
        let identity = NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed).max(1);
        header.identity.store(identity, Ordering::Relaxed);
        identity
    }

    pub(crate) fn identity_hash(&self, word: Word) -> usize {
        Self::word_identity_hash(word)
    }

    pub(crate) fn hash_table(
        &self,
        test: HashTest,
        weakness: Option<HashTableWeakness>,
        size: usize,
        rehash_size: RehashHint,
        rehash_threshold: RehashHint,
    ) -> Result<Word, OutOfMemory> {
        let buckets = self.root_slot(
            self.allocate_buckets(capacity_for(size, rehash_threshold)?, weakness.is_some())?,
        );
        self.alloc_object(|header| HashTable {
            header,
            buckets: WordSlot::new(buckets.get()),
            count: AtomicUsize::new(0),
            used: AtomicUsize::new(0),
            test,
            weakness,
            rehash_size,
            rehash_threshold,
        })
    }

    pub(crate) fn hash_table_info(&self, word: Word) -> Option<HashTableLayout> {
        let table = self.hash_table_ref(word)?;
        Some(HashTableLayout {
            test: table.test,
            weakness: table.weakness,
            count: table.count.load(Ordering::Relaxed),
            size: usable(table.buckets().len(), table.rehash_threshold),
            rehash_size: table.rehash_size,
            rehash_threshold: table.rehash_threshold,
        })
    }

    /// Look up a key using a comparison that does not allocate Lisp objects.
    pub(crate) fn hash_table_get(
        &self,
        word: Word,
        key: Word,
        hash: usize,
        same: impl FnMut(Word, Word) -> bool,
    ) -> Option<Word> {
        let buckets = self.hash_table_ref(word).expect("hash table").buckets();
        match position(buckets, key, hash, same) {
            Position::Present(index) => Some(buckets[index].value()),
            Position::Vacant { .. } => None,
        }
    }

    fn hash_table_barrier_owner(&self, table_word: Word, table: &HashTable) -> Address {
        if table.weakness.is_some() {
            // Weak HashBuckets are skipped by scan_buckets; logging the owning HashTable ensures
            // scan_table re-registers the table in weak.tables during minor GC.
            table_word.object_address().expect("hash table")
        } else {
            table.buckets.get().object_address().expect("hash buckets")
        }
    }

    /// Update a table using a comparison that does not allocate Lisp objects.
    pub(crate) fn hash_table_set(
        &self,
        word: Word,
        key: Word,
        value: Word,
        hash: usize,
        same: impl FnMut(Word, Word) -> bool,
    ) -> Result<(), OutOfMemory> {
        let (capacity, weak) = {
            let table = self.hash_table_ref(word).expect("hash table");
            let buckets = table.buckets();
            match position(buckets, key, hash, same) {
                Position::Present(index) => {
                    let owner = self.hash_table_barrier_owner(word, table);
                    self.write_slot(owner, &buckets[index].value, value);
                    return Ok(());
                }
                Position::Vacant { index, empty } => {
                    let used = table.used.load(Ordering::Relaxed);
                    let limit = usable(buckets.len(), table.rehash_threshold);
                    if !empty || used < limit {
                        let owner = self.hash_table_barrier_owner(word, table);
                        let bucket = &buckets[index];
                        self.write_slot(owner, &bucket.key, key);
                        self.write_slot(owner, &bucket.value, value);
                        bucket.hash.store(hash, Ordering::Relaxed);
                        table.count.fetch_add(1, Ordering::Relaxed);
                        if empty {
                            table.used.fetch_add(1, Ordering::Relaxed);
                        }
                        return Ok(());
                    }
                    let count = table.count.load(Ordering::Relaxed);
                    let capacity = if count >= limit {
                        let requested = rehash_target(table.rehash_size, count)?;
                        let mut capacity = capacity_for(requested, table.rehash_threshold)?;
                        while capacity <= buckets.len()
                            || usable(capacity, table.rehash_threshold) <= count
                        {
                            capacity = capacity.checked_mul(2).ok_or(OutOfMemory)?;
                        }
                        capacity
                    } else {
                        buckets.len()
                    };
                    (capacity, table.weakness.is_some())
                }
            }
        };

        let roots = self.root_frame(3);
        roots.set(0, word);
        roots.set(1, key);
        roots.set(2, value);
        let replacement = self.allocate_buckets(capacity, weak)?;
        let table = self.hash_table_ref(roots.get(0)).expect("hash table");
        // SAFETY: allocate_buckets published this slice, and reinserting
        // entries and linking the table do not reach a safepoint.
        let new_buckets = unsafe { &*bucket_slice(replacement) };
        for old in table.buckets() {
            let key = old.key();
            if key != Word::UNBOUND {
                insert_fresh(
                    new_buckets,
                    key,
                    old.value(),
                    old.hash.load(Ordering::Relaxed),
                );
            }
        }
        insert_fresh(new_buckets, roots.get(1), roots.get(2), hash);
        self.write_slot_object(table, &table.buckets, replacement);
        let count = table.count.fetch_add(1, Ordering::Relaxed) + 1;
        table.used.store(count, Ordering::Relaxed);
        Ok(())
    }

    /// Delete the matching entry, preserving the rest of the probe chain.
    pub(crate) fn hash_table_remove(
        &self,
        word: Word,
        key: Word,
        hash: usize,
        same: impl FnMut(Word, Word) -> bool,
    ) -> bool {
        let table = self.hash_table_ref(word).expect("hash table");
        let buckets = table.buckets();
        let Position::Present(index) = position(buckets, key, hash, same) else {
            return false;
        };
        buckets[index].put_unbarriered(Word::UNBOUND, Word::NIL, 0);
        table.count.fetch_sub(1, Ordering::Relaxed);
        true
    }

    pub(crate) fn hash_table_clear(&self, word: Word) {
        let table = self.hash_table_ref(word).expect("hash table");
        for bucket in table.buckets() {
            bucket.put_unbarriered(Word::UNBOUND, Word::UNBOUND, 0);
        }
        table.count.store(0, Ordering::Relaxed);
        table.used.store(0, Ordering::Relaxed);
    }

    /// Fetch the next live entry. A callback must root both words before
    /// allocating. Reload the rooted table word before continuing.
    pub(crate) fn hash_table_next_entry(
        &self,
        word: Word,
        cursor: &mut usize,
    ) -> Option<(Word, Word)> {
        let buckets = self.hash_table_ref(word).expect("hash table").buckets();
        while let Some(bucket) = buckets.get(*cursor) {
            *cursor += 1;
            let key = bucket.key();
            if key != Word::UNBOUND {
                return Some((key, bucket.value()));
            }
        }
        None
    }

    /// Walk matching hash candidates along one probe chain from cursor zero.
    pub(crate) fn hash_table_next_candidate(
        &self,
        word: Word,
        hash: usize,
        cursor: &mut usize,
    ) -> Option<(Word, Word)> {
        let buckets = self.hash_table_ref(word).expect("hash table").buckets();
        while *cursor < buckets.len() {
            let index = hash.wrapping_add(*cursor) & (buckets.len() - 1);
            *cursor += 1;
            let bucket = &buckets[index];
            let key = bucket.key();
            let value = bucket.value();
            if key == Word::UNBOUND {
                if value == Word::UNBOUND {
                    *cursor = buckets.len();
                    return None;
                }
            } else if bucket.hash.load(Ordering::Relaxed) == hash {
                return Some((key, value));
            }
        }
        None
    }

    fn allocate_buckets(&self, capacity: usize, weak: bool) -> Result<Word, OutOfMemory> {
        self.alloc_with_trailing(
            capacity,
            |header| HashBuckets {
                header,
                capacity,
                weak,
            },
            |_| Bucket::empty(),
        )
    }
}

pub(super) fn scan_table(base: Address, visitor: &mut impl SlotVisitor) {
    visitor.visit_slot(ValueSlot(base + offset_of!(HashTable, buckets)));
}

pub(super) fn scan_buckets(base: Address, visitor: &mut impl SlotVisitor) {
    // SAFETY: the collector is scanning an initialized HashBuckets object.
    let buckets = unsafe { &*base.to_ptr::<HashBuckets>() };
    if buckets.weak {
        return;
    }
    let capacity = buckets.capacity;
    let start = trailing_base::<HashBuckets>(base);
    for index in 0..capacity {
        let bucket = start + index * size_of::<Bucket>();
        visitor.visit_slot(ValueSlot(bucket + offset_of!(Bucket, key)));
        visitor.visit_slot(ValueSlot(bucket + offset_of!(Bucket, value)));
    }
}

pub(super) fn has_weak_entries(base: Address) -> bool {
    // SAFETY: the collector has identified this object as a hash table.
    let table = unsafe { &*base.to_ptr::<HashTable>() };
    table.weakness.is_some() && table.count.load(Ordering::Relaxed) != 0
}

pub(super) fn register_weak_entries(
    base: Address,
    live: impl Fn(Word) -> bool,
    ephemerons: &mut super::weak::Ephemerons,
) {
    // SAFETY: ordinary tracing has forwarded the table and its buckets.
    let table = unsafe { &*base.to_ptr::<HashTable>() };
    let weakness = table.weakness.expect("weak table");
    if weakness == HashTableWeakness::KeyAndValue {
        return; // Both fields need independent reachability.
    }
    for bucket in table.buckets() {
        let key = bucket.key();
        if key == Word::UNBOUND {
            continue;
        }
        let value = bucket.value();
        let key_live = live(key);
        let value_live = live(value);
        if !value_live
            && matches!(
                weakness,
                HashTableWeakness::Key | HashTableWeakness::KeyOrValue
            )
        {
            ephemerons.register(key, key_live, ValueSlot(Address::from_ptr(&bucket.value)));
        }
        if !key_live
            && matches!(
                weakness,
                HashTableWeakness::Value | HashTableWeakness::KeyOrValue
            )
        {
            ephemerons.register(value, value_live, ValueSlot(Address::from_ptr(&bucket.key)));
        }
    }
}

pub(super) fn sweep_weak_entries(base: Address, live: impl Fn(Word) -> Option<Word>) {
    // SAFETY: ordinary tracing has forwarded the table and its buckets.
    let table = unsafe { &*base.to_ptr::<HashTable>() };
    let mut count = 0;
    for bucket in table.buckets() {
        let key = bucket.key();
        if key == Word::UNBOUND {
            continue;
        }
        // At the ephemeron fixed point both halves of every surviving entry
        // are reachable, including the dependent halves retained above.
        if let (Some(key), Some(value)) = (live(key), live(bucket.value())) {
            bucket.key.set_unbarriered(key);
            bucket.value.set_unbarriered(value);
            count += 1;
        } else {
            bucket.put_unbarriered(Word::UNBOUND, Word::NIL, 0);
        }
    }
    table.count.store(count, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn check_hash_table_storage(heap: &Heap) {
    let same = |left, right| left == right;
    let table = heap.root_slot(
        heap.hash_table(
            HashTest::Eq,
            None,
            0,
            RehashHint::integer(1),
            RehashHint::integer(1),
        )
        .unwrap(),
    );
    let key = heap.root_slot(heap.cons(|| (Word::NIL, Word::NIL)).unwrap());
    let key_hash = heap.identity_hash(key.get());
    let value = heap.cons(|| (Word::T, Word::NIL)).unwrap();
    heap.hash_table_set(table.get(), key.get(), value, key_hash, same)
        .unwrap();

    let previous_key = key.get();
    heap.collect();
    assert_ne!(key.get(), previous_key);
    assert_eq!(heap.identity_hash(key.get()), key_hash);
    let current_value = heap
        .hash_table_get(table.get(), key.get(), key_hash, same)
        .unwrap();
    assert_ne!(current_value, value);
    assert_eq!(heap.cons_fields(current_value), Some((Word::T, Word::NIL)));

    // Exercise collisions, tombstone reuse, and growth without moving the
    // stored key identity or losing the key's probe chain.
    for number in 0..32 {
        let key = Word::fixnum(number).unwrap();
        heap.hash_table_set(table.get(), key, Word::NIL, key_hash, same)
            .unwrap();
    }
    for number in 0..16 {
        let key = Word::fixnum(number).unwrap();
        assert!(heap.hash_table_remove(table.get(), key, key_hash, same));
    }
    for number in 16..48 {
        let key = Word::fixnum(number).unwrap();
        heap.hash_table_set(table.get(), key, Word::T, key_hash, same)
            .unwrap();
    }
    let info = heap.hash_table_info(table.get()).unwrap();
    assert_eq!(info.count, 33);
    let mut cursor = 0;
    let mut entries = 0;
    while let Some((key, value)) = heap.hash_table_next_entry(table.get(), &mut cursor) {
        assert_eq!(
            heap.hash_table_get(table.get(), key, key_hash, same),
            Some(value)
        );
        entries += 1;
    }
    assert_eq!(entries, info.count);
    heap.hash_table_clear(table.get());
    assert_eq!(heap.hash_table_info(table.get()).unwrap().count, 0);
    assert_eq!(
        heap.hash_table_get(table.get(), key.get(), key_hash, same),
        None
    );

    let weak_table = heap.root_slot(
        heap.hash_table(
            HashTest::Eq,
            Some(HashTableWeakness::Key),
            0,
            RehashHint::integer(1),
            RehashHint::integer(1),
        )
        .unwrap(),
    );
    // The first entry dies in front of a colliding, surviving entry. Its
    // tombstone must keep the latter reachable both by lookup and insertion.
    let dead = heap.cons(|| (Word::T, Word::T)).unwrap();
    heap.hash_table_set(weak_table.get(), dead, dead, 0, same)
        .unwrap();
    let payload = heap.cons(|| (key.get(), Word::T)).unwrap();
    heap.hash_table_set(weak_table.get(), key.get(), payload, 0, same)
        .unwrap();
    let previous_key = key.get();
    heap.collect();
    assert_ne!(key.get(), previous_key);
    let moved = heap
        .hash_table_get(weak_table.get(), key.get(), 0, same)
        .unwrap();
    assert_ne!(moved, payload);
    assert_eq!(heap.cons_fields(moved), Some((key.get(), Word::T)));
    assert_eq!(heap.hash_table_info(weak_table.get()).unwrap().count, 1);
    heap.hash_table_set(weak_table.get(), Word::NIL, Word::T, 0, same)
        .unwrap();
    heap.collect();
    assert_eq!(
        heap.hash_table_get(weak_table.get(), Word::NIL, 0, same),
        Some(Word::T)
    );
    assert_eq!(heap.hash_table_info(weak_table.get()).unwrap().count, 2);
}
