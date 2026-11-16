//! Per-runtime self-hosted Immix mark-region + opportunistic-evacuation heap.
//!
//! Layout:
//! - Small/medium objects (`<= MAX_IMMIX_OBJECT_BYTES`) live in 32 KiB aligned blocks
//!   divided into 256 x 128-byte lines.
//! - Large objects (`> MAX_IMMIX_OBJECT_BYTES`) live in the Large Object Space (LOS)
//!   with 16-byte alignment and never move.
//! - Each `ImmixHeap` owns its virtual memory pages exclusively and releases all
//!   blocks and LOS allocations on `Drop`, allowing arbitrarily many isolated
//!   `Runtime` instances to run concurrently in the same process.

use std::alloc::{Layout, alloc, dealloc};
use std::cmp::Reverse;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustc_hash::FxHashMap;

use super::{Address, Header, OutOfMemory, ValueSlot, Word};
use lines::{FreeLines, LineMap};

mod lines;

pub(super) const BLOCK_SHIFT: usize = 15;
pub(super) const BLOCK_SIZE: usize = 1 << BLOCK_SHIFT; // 32 KiB
pub(super) const LINE_SHIFT: usize = 7;
pub(super) const LINE_SIZE: usize = 1 << LINE_SHIFT; // 128 B
pub(super) const LINES_PER_BLOCK: usize = BLOCK_SIZE / LINE_SIZE; // 256
pub(super) const MAX_IMMIX_OBJECT_BYTES: usize = BLOCK_SIZE / 4; // 8 KiB
pub(super) const OBJECT_ALIGN: usize = 8;
const LOS_ALIGN: usize = 16;
const MIN_COLLECTION_BUDGET: usize = 8 << 20;
const MINOR_COLLECTION_BUDGET: usize = 2 << 20;
const _: () = assert!(MAX_IMMIX_OBJECT_BYTES <= u16::MAX as usize);

const FORWARDED_BIT: usize = 1;
// Transient BUSY claim bit before `dst | FORWARDED_BIT` (`dst != 0`) or `mark` is published.
const FORWARDING_BUSY: usize = FORWARDED_BIT;
const MARKED_BIT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObjectClaim {
    AlreadyMarked,
    Forwarded(usize),
    ClaimedInPlace,
    ClaimedEvacuate,
}

#[inline]
pub(super) fn claim_object(header: &Header, mark: usize, evacuate: bool) -> ObjectClaim {
    let target = if evacuate { FORWARDING_BUSY } else { mark };
    loop {
        let current = header.forwarding.load(Ordering::Acquire);
        if current == mark {
            return ObjectClaim::AlreadyMarked;
        }
        if current == FORWARDING_BUSY {
            std::hint::spin_loop();
            continue;
        }
        if current & FORWARDED_BIT != 0 {
            return ObjectClaim::Forwarded(current & !FORWARDED_BIT);
        }
        if header
            .forwarding
            .compare_exchange_weak(current, target, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return if evacuate {
                ObjectClaim::ClaimedEvacuate
            } else {
                header.promote_mature();
                ObjectClaim::ClaimedInPlace
            };
        }
    }
}

/// Configuration for a per-runtime [`ImmixHeap`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct HeapConfig {
    pub(crate) max_heap_bytes: usize,
    pub(crate) always_defrag: bool,
    pub(crate) stress_bytes: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CollectionMode {
    Minor,
    Major,
    Compact,
}

impl Default for HeapConfig {
    fn default() -> Self {
        Self {
            max_heap_bytes: 64 << 20,
            always_defrag: false,
            stress_bytes: None,
        }
    }
}

impl HeapConfig {
    fn minor_budget(self, mature_live_bytes: usize) -> usize {
        if let Some(stress) = self.stress_bytes {
            return stress;
        }
        // Collect dead young allocations frequently on typical heaps while letting small
        // explicitly sized heaps grow up to their limit before collecting.
        if MINOR_COLLECTION_BUDGET < self.max_heap_bytes.saturating_sub(mature_live_bytes) {
            MINOR_COLLECTION_BUDGET
        } else {
            usize::MAX
        }
    }
}

struct ImmixBlock {
    base: *mut u8,
    line_marks: LineMap,
    live_lines: u16,
    holes: u16,
    evacuate: bool,
}

impl ImmixBlock {
    fn allocate() -> Result<Self, OutOfMemory> {
        let layout = Layout::from_size_align(BLOCK_SIZE, BLOCK_SIZE).expect("valid block layout");
        // SAFETY: `BLOCK_SIZE` is non-zero and power-of-two aligned.
        let base = unsafe { alloc(layout) };
        if base.is_null() {
            return Err(OutOfMemory);
        }
        Ok(Self {
            base,
            line_marks: LineMap::default(),
            live_lines: 0,
            holes: 1,
            evacuate: false,
        })
    }

    #[inline]
    fn base_addr(&self) -> usize {
        self.base as usize
    }
}

impl Drop for ImmixBlock {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(BLOCK_SIZE, BLOCK_SIZE).expect("valid block layout");
        // SAFETY: `self.base` was allocated with `layout` in `ImmixBlock::allocate`.
        unsafe { dealloc(self.base, layout) };
    }
}

struct LargeObjectAllocation {
    ptr: *mut u8,
    layout: Layout,
    marked: bool,
    mature: bool,
}

impl Drop for LargeObjectAllocation {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was allocated with `self.layout`.
        unsafe { dealloc(self.ptr, self.layout) };
    }
}

#[derive(Default)]
struct BumpCursor {
    cursor: usize,
    limit: usize,
}

impl BumpCursor {
    #[inline]
    fn allocate(&mut self, size: usize) -> Option<usize> {
        if size > self.limit - self.cursor {
            return None;
        }
        let start = self.cursor;
        self.cursor += size;
        Some(start)
    }

    fn release_lines(&self, free: &mut FreeLines) {
        let bytes = (self.limit - self.cursor) & !(LINE_SIZE - 1);
        if bytes != 0 {
            free.insert(self.limit - bytes, bytes);
        }
    }
}

pub(super) struct GcSnapshot {
    pub(super) live_bytes: usize,
    pub(super) allocated_bytes_since_collection: usize,
    pub(super) last_reclaimed_bytes: usize,
    pub(super) last_evacuated_bytes: usize,
    pub(super) last_evacuated_objects: usize,
    pub(super) block_count: usize,
    pub(super) last_evacuation_blocks: usize,
}

struct Evacuation {
    cursor: BumpCursor,
    block: usize,
    fresh_blocks_left: usize,
    fresh_blocks: usize,
}

/// A stop-the-world trace over initialized allocations. Heap integration
/// chooses which slots are roots and scans the newly marked objects' fields.
pub(super) struct Collection<'heap> {
    heap: &'heap mut ImmixHeap,
    evacuation: Evacuation,
    cached_block: (usize, usize),
    mark: usize,
    minor: bool,
    live_bytes: usize,
    moved_bytes: usize,
    moved_objects: usize,
}

pub(crate) struct ImmixHeap {
    config: HeapConfig,
    /// All live blocks owned by this heap.
    blocks: Vec<ImmixBlock>,
    /// Maps block numbers (addresses shifted past their alignment bits) to indices.
    block_index: FxHashMap<usize, usize>,
    /// Unreserved line runs, available to the mutator or the next evacuation.
    free_lines: FreeLines,
    candidates: Vec<(Reverse<u16>, u16, usize)>,
    /// Primary allocation cursor (recycled or fresh block).
    alloc: BumpCursor,
    /// Independent cursor for allocations too large for the primary gap.
    overflow: BumpCursor,
    /// Large Object Space entries keyed by start address.
    large_objects: FxHashMap<usize, LargeObjectAllocation>,
    large_object_bytes: usize,
    /// Bytes allocated since the previous collection (drives stress & auto-GC).
    allocated_since_gc: usize,
    collection_budget: usize,
    /// Successful mutator allocation bytes, never reset by collection.
    allocated_total: std::rc::Rc<std::cell::Cell<u64>>,
    /// Per-heap collection epoch (starts at 1; `Header::forwarding == 0` is never marked).
    gc_epoch: usize,
    mature_live_bytes: usize,
    last_major_live_bytes: usize,
    minors_since_major: u32,
    last_live_bytes: usize,
    last_reclaimed_bytes: usize,
    last_evacuated_bytes: usize,
    last_evacuated_objects: usize,
    last_evacuation_blocks: usize,
    #[cfg(test)]
    pub(super) relocations: usize,
}

impl ImmixHeap {
    pub(crate) fn new(config: HeapConfig) -> Self {
        let config = HeapConfig {
            max_heap_bytes: config.max_heap_bytes.max(1 << 20),
            ..config
        };
        Self {
            config,
            blocks: Vec::new(),
            block_index: FxHashMap::default(),
            free_lines: FreeLines::default(),
            candidates: Vec::new(),
            alloc: BumpCursor::default(),
            overflow: BumpCursor::default(),
            large_objects: FxHashMap::default(),
            large_object_bytes: 0,
            allocated_since_gc: 0,
            collection_budget: config.minor_budget(0),
            allocated_total: std::rc::Rc::default(),
            gc_epoch: 1,
            mature_live_bytes: 0,
            last_major_live_bytes: 0,
            minors_since_major: 0,
            last_live_bytes: 0,
            last_reclaimed_bytes: 0,
            last_evacuated_bytes: 0,
            last_evacuated_objects: 0,
            last_evacuation_blocks: 0,
            #[cfg(test)]
            relocations: 0,
        }
    }

    pub(crate) fn max_heap_bytes(&self) -> usize {
        self.config.max_heap_bytes
    }

    pub(super) fn evacuation_reserve_bytes(&self) -> usize {
        // Bound the temporary space independently of how often compaction spills.
        (self.config.max_heap_bytes / (40 * BLOCK_SIZE)).max(1) * BLOCK_SIZE
    }

    #[inline]
    pub(super) fn committed_bytes(&self) -> usize {
        self.blocks.len() * BLOCK_SIZE + self.large_object_bytes
    }

    pub(super) fn large_object_bytes(&self) -> usize {
        self.large_object_bytes
    }

    #[inline]
    pub(super) fn should_collect(&self) -> bool {
        self.allocated_since_gc >= self.collection_budget
    }

    pub(super) fn should_major_collect(&self) -> bool {
        self.config.always_defrag
            || self.minors_since_major >= 8
            || self.mature_live_bytes
                >= MIN_COLLECTION_BUDGET.max(self.last_major_live_bytes.saturating_mul(2))
            || self.committed_bytes() >= self.config.max_heap_bytes.saturating_mul(3) / 4
    }

    pub(super) fn gc_snapshot(&self) -> GcSnapshot {
        GcSnapshot {
            live_bytes: self.last_live_bytes,
            allocated_bytes_since_collection: self.allocated_since_gc,
            last_reclaimed_bytes: self.last_reclaimed_bytes,
            last_evacuated_bytes: self.last_evacuated_bytes,
            last_evacuated_objects: self.last_evacuated_objects,
            block_count: self.blocks.len(),
            last_evacuation_blocks: self.last_evacuation_blocks,
        }
    }

    pub(super) fn allocation_counter(&self) -> std::rc::Rc<std::cell::Cell<u64>> {
        self.allocated_total.clone()
    }

    /// A failed allocation immediately after sweep can need fully free blocks
    /// even when the live objects and the request fit within the byte limit.
    pub(super) fn can_compact_for(&self, size: usize) -> bool {
        size.next_multiple_of(OBJECT_ALIGN)
            <= self
                .config
                .max_heap_bytes
                .saturating_sub(self.last_live_bytes)
            && self.free_lines.available_bytes() >= BLOCK_SIZE
    }

    /// Try allocating `size` bytes without triggering a GC.
    /// Returns `Ok(Some(address))` on success, `Ok(None)` when a GC should be attempted,
    /// or `Err(OutOfMemory)` if the request is impossible even for an empty heap.
    #[inline]
    pub(super) fn try_allocate(&mut self, size: usize) -> Result<Option<Address>, OutOfMemory> {
        let aligned = size
            .max(size_of::<Header>())
            .checked_next_multiple_of(OBJECT_ALIGN)
            .ok_or(OutOfMemory)?;
        let allocated = self.try_allocate_aligned(aligned)?;
        if allocated.is_some() {
            self.allocated_since_gc = self.allocated_since_gc.saturating_add(aligned);
            self.allocated_total
                .set(self.allocated_total.get().saturating_add(aligned as u64));
        }
        Ok(allocated)
    }

    #[inline]
    fn try_allocate_aligned(&mut self, size: usize) -> Result<Option<Address>, OutOfMemory> {
        if size <= MAX_IMMIX_OBJECT_BYTES {
            if let Some(addr) = self.alloc.allocate(size) {
                return Ok(Some(Address::from_usize(addr)));
            }
            Ok(self.refill(size))
        } else {
            self.allocate_large_object(size)
        }
    }

    #[cold]
    fn refill(&mut self, size: usize) -> Option<Address> {
        let medium = size > LINE_SIZE;
        if medium && let Some(start) = self.overflow.allocate(size) {
            return Some(Address::from_usize(start));
        }
        let (start, end) = if let Some(run) = self.free_lines.take(size) {
            run
        } else {
            // Small allocations can consume an otherwise stranded overflow tail.
            if !medium && let Some(start) = self.overflow.allocate(size) {
                return Some(Address::from_usize(start));
            }
            if self.committed_bytes().saturating_add(BLOCK_SIZE) > self.config.max_heap_bytes {
                return None;
            }
            let index = self.acquire_block().ok()?;
            let start = self.blocks[index].base_addr();
            (start, start + BLOCK_SIZE)
        };

        let cursor = if medium {
            &mut self.overflow
        } else {
            &mut self.alloc
        };
        cursor.release_lines(&mut self.free_lines);
        *cursor = BumpCursor {
            cursor: start + size,
            limit: end,
        };
        Some(Address::from_usize(start))
    }

    fn acquire_block(&mut self) -> Result<usize, OutOfMemory> {
        let block = ImmixBlock::allocate()?;
        let base = block.base_addr();
        let idx = self.blocks.len();
        self.blocks.push(block);
        self.block_index.insert(base >> BLOCK_SHIFT, idx);
        Ok(idx)
    }

    #[cold]
    fn allocate_large_object(&mut self, aligned: usize) -> Result<Option<Address>, OutOfMemory> {
        if aligned > self.config.max_heap_bytes {
            return Err(OutOfMemory);
        }
        let layout = Layout::from_size_align(aligned, LOS_ALIGN).map_err(|_| OutOfMemory)?;
        if aligned
            > self
                .config
                .max_heap_bytes
                .saturating_sub(self.committed_bytes())
        {
            return Ok(None);
        }
        // SAFETY: `aligned` is non-zero and `LOS_ALIGN` is a power of two.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Ok(None);
        }
        let addr = ptr as usize;
        self.large_objects.insert(
            addr,
            LargeObjectAllocation {
                ptr,
                layout,
                marked: false,
                mature: false,
            },
        );
        self.large_object_bytes = self.large_object_bytes.saturating_add(aligned);
        Ok(Some(Address::from_usize(addr)))
    }

    fn prepare_evacuation(&mut self, mode: CollectionMode) -> Evacuation {
        if mode == CollectionMode::Minor {
            // Preserve existing line_marks for mature objects so sweep only reclaims lines
            // left unmarked by dead young objects.
            for block in &mut self.blocks {
                block.evacuate = false;
            }
            self.alloc = BumpCursor::default();
            self.overflow = BumpCursor::default();
            return Evacuation {
                cursor: BumpCursor::default(),
                block: 0,
                fresh_blocks_left: 0,
                fresh_blocks: 0,
            };
        }

        let compact = mode == CollectionMode::Compact;
        self.candidates.clear();
        for (index, block) in self.blocks.iter_mut().enumerate() {
            block.line_marks = LineMap::default();
            block.evacuate = self.config.always_defrag;
            if block.evacuate || block.holes == 0 {
                continue;
            }
            if compact
                || (block.holes >= 2 && usize::from(block.live_lines) < LINES_PER_BLOCK * 3 / 4)
            {
                // At the limit prioritize the cheapest whole blocks to free.
                let priority = if compact { 0 } else { block.holes };
                self.candidates
                    .push((Reverse(priority), block.live_lines, index));
            }
        }

        if self.config.always_defrag || !self.candidates.is_empty() {
            // These lines were reserved by the mutator but never written. They
            // are safe destinations if their block is not an evacuation source.
            self.alloc.release_lines(&mut self.free_lines);
            self.overflow.release_lines(&mut self.free_lines);
        }
        self.alloc = BumpCursor::default();
        self.overflow = BumpCursor::default();

        let maximum = self
            .config
            .max_heap_bytes
            .saturating_add(self.evacuation_reserve_bytes());
        let available = maximum.saturating_sub(self.committed_bytes()) / BLOCK_SIZE;
        let fresh_blocks_left = if self.config.always_defrag {
            available
        } else {
            available.min((self.blocks.len() / 40).max(1))
        };
        let candidate_limit = if compact {
            // Each source consumes destination capacity for its live lines and
            // withdraws its unoccupied lines from the pool, one block in total.
            self.free_lines.available_bytes() / BLOCK_SIZE + fresh_blocks_left
        } else {
            (self.blocks.len() / 4).max(1)
        };
        let count = candidate_limit.min(self.candidates.len());
        if count < self.candidates.len() {
            self.candidates.select_nth_unstable(count);
        }
        for &(_, _, index) in &self.candidates[..count] {
            self.blocks[index].evacuate = true;
        }
        Evacuation {
            cursor: BumpCursor::default(),
            block: 0,
            fresh_blocks_left,
            fresh_blocks: 0,
        }
    }

    /// Run a stop-the-world minor, major, or compacting Immix collection.
    pub(super) fn collect<T>(
        &mut self,
        mode: CollectionMode,
        trace: impl FnOnce(&mut Collection<'_>) -> T,
    ) -> T {
        let old_committed = self.committed_bytes();
        let minor = mode == CollectionMode::Minor;
        let evacuation = self.prepare_evacuation(mode);
        self.gc_epoch = (self.gc_epoch + 1) & (usize::MAX >> 2);

        let mut collection = Collection {
            evacuation,
            cached_block: (usize::MAX, 0),
            mark: (self.gc_epoch << 2) | MARKED_BIT,
            minor,
            live_bytes: 0,
            moved_bytes: 0,
            moved_objects: 0,
            heap: self,
        };
        let observed = trace(&mut collection);
        let Collection {
            live_bytes,
            moved_bytes,
            moved_objects,
            evacuation,
            ..
        } = collection;

        self.sweep_blocks();
        self.sweep_large_objects(minor);

        if minor {
            self.mature_live_bytes = self.mature_live_bytes.saturating_add(live_bytes);
            self.last_live_bytes = self.mature_live_bytes;
            self.minors_since_major += 1;
        } else {
            self.mature_live_bytes = live_bytes;
            self.last_major_live_bytes = live_bytes;
            self.last_live_bytes = live_bytes;
            self.minors_since_major = 0;
        }
        self.last_evacuated_bytes = moved_bytes;
        self.last_evacuated_objects = moved_objects;
        self.last_evacuation_blocks = evacuation.fresh_blocks;
        self.last_reclaimed_bytes = old_committed
            .saturating_add(evacuation.fresh_blocks * BLOCK_SIZE)
            .saturating_sub(self.committed_bytes());
        self.allocated_since_gc = 0;
        self.collection_budget = self.config.minor_budget(self.mature_live_bytes);
        #[cfg(test)]
        {
            self.relocations += moved_objects;
        }
        observed
    }

    fn alloc_evacuation(&mut self, evac: &mut Evacuation, size: usize) -> Option<usize> {
        if let Some(addr) = evac.cursor.allocate(size) {
            return Some(addr);
        }
        while let Some((start, end)) = self.free_lines.take(size) {
            let index = self.block_index[&(start >> BLOCK_SHIFT)];
            if !self.blocks[index].evacuate {
                evac.cursor.release_lines(&mut self.free_lines);
                evac.cursor = BumpCursor {
                    cursor: start + size,
                    limit: end,
                };
                evac.block = index;
                return Some(start);
            }
        }
        if evac.fresh_blocks_left == 0 {
            return None;
        }
        let Ok(index) = self.acquire_block() else {
            evac.fresh_blocks_left = 0;
            return None;
        };
        evac.fresh_blocks_left -= 1;
        evac.fresh_blocks += 1;
        let base = self.blocks[index].base_addr();
        evac.cursor.release_lines(&mut self.free_lines);
        evac.cursor = BumpCursor {
            cursor: base + size,
            limit: base + BLOCK_SIZE,
        };
        evac.block = index;
        Some(base)
    }

    fn sweep_blocks(&mut self) {
        self.free_lines.clear();
        let mut index = 0;
        let mut previous = 0;
        self.blocks.retain_mut(|block| {
            let base = block.base_addr();
            let number = base >> BLOCK_SHIFT;
            let old_index = previous;
            previous += 1;
            block.live_lines = block.line_marks.marked_count() as u16;
            if block.live_lines == 0 {
                self.block_index.remove(&number);
                return false;
            }
            block.evacuate = false;
            block.holes = 0;
            if usize::from(block.live_lines) < LINES_PER_BLOCK {
                for (offset, size) in block.line_marks.free_runs() {
                    self.free_lines.insert(base + offset, size);
                    block.holes += 1;
                }
            }
            if index != old_index {
                *self
                    .block_index
                    .get_mut(&number)
                    .expect("owned Immix block") = index;
            }
            index += 1;
            true
        });
    }

    fn sweep_large_objects(&mut self, minor: bool) {
        let mut live_bytes = 0usize;
        self.large_objects.retain(|_, entry| {
            let keep = if minor {
                entry.mature || entry.marked
            } else {
                entry.marked
            };
            if keep {
                if entry.marked {
                    entry.mature = true;
                    entry.marked = false;
                }
                live_bytes = live_bytes.saturating_add(entry.layout.size());
                true
            } else {
                false
            }
        });
        self.large_object_bytes = live_bytes;
    }
}

impl Collection<'_> {
    #[inline]
    pub(super) fn is_minor(&self) -> bool {
        self.minor
    }

    /// Before sweep, weak fields may name a source, a destination, or an unmarked object.
    #[inline]
    pub(super) fn live_word(&self, word: Word) -> Option<Word> {
        let Some(address) = word.object_address() else {
            return Some(word);
        };
        // SAFETY: these fields belong to this heap, and object headers are intact until sweep.
        let header = unsafe { &*address.to_ptr::<Header>() };
        if self.minor && header.is_mature() {
            return Some(word);
        }
        let forwarding = header.forwarding.load(Ordering::Acquire);
        if forwarding != FORWARDING_BUSY && forwarding & FORWARDED_BIT != 0 {
            Some(Word(forwarding & !FORWARDED_BIT))
        } else {
            (forwarding == self.mark).then_some(word)
        }
    }

    /// Forward a slot immediately. Report the original word and current address
    /// only on the first visit, so the integration can scan each object once.
    #[inline]
    pub(super) fn trace_slot(&mut self, slot: ValueSlot) -> Option<(Word, Address)> {
        // SAFETY: the visitor supplies initialized root slots or live object fields.
        let atomic = unsafe { &*slot.0.to_ptr::<AtomicUsize>() };
        let word = Word(atomic.load(Ordering::Relaxed));
        let address = word.object_address()?;
        let addr = address.as_usize();

        let header_ptr = address.to_mut_ptr::<Header>();
        // SAFETY: ownership was guaranteed or checked above. A back edge can
        // target the currently scanned object, so avoid an exclusive reference.
        let header = unsafe { &*header_ptr };
        if self.minor && header.is_mature() {
            return None;
        }
        let forwarding = header.forwarding.load(Ordering::Acquire);
        if forwarding == self.mark {
            return None;
        }
        if forwarding != FORWARDING_BUSY && forwarding & FORWARDED_BIT != 0 {
            atomic.store(forwarding & !FORWARDED_BIT, Ordering::Relaxed);
            return None;
        }

        let size = usize::from(header.immix_size);
        let block_index = if size > 0 {
            Some(self.block_for(addr).expect("owned Immix block"))
        } else {
            None
        };
        let evacuate =
            block_index.is_some_and(|index| self.heap.blocks[index].evacuate && !header.is_pinned());

        match claim_object(header, self.mark, evacuate) {
            ObjectClaim::AlreadyMarked => None,
            ObjectClaim::Forwarded(dst) => {
                atomic.store(dst, Ordering::Relaxed);
                None
            }
            ObjectClaim::ClaimedInPlace => {
                if let Some(index) = block_index {
                    let base = addr & !(BLOCK_SIZE - 1);
                    self.live_bytes += size;
                    self.heap.blocks[index]
                        .line_marks
                        .mark_object(addr - base, size);
                } else {
                    let large = self
                        .heap
                        .large_objects
                        .get_mut(&addr)
                        .expect("owned large object");
                    large.marked = true;
                    self.live_bytes += large.layout.size();
                }
                Some((word, address))
            }
            ObjectClaim::ClaimedEvacuate => {
                let index = block_index.expect("evacuated object belongs to a block");
                let base = addr & !(BLOCK_SIZE - 1);
                self.live_bytes += size;
                if let Some(dst) = self.heap.alloc_evacuation(&mut self.evacuation, size) {
                    // SAFETY: the reserved destination is disjoint and has room for the object.
                    unsafe {
                        std::ptr::copy_nonoverlapping(header_ptr.cast::<u8>(), dst as *mut u8, size);
                        let dst_header = &*(dst as *const Header);
                        dst_header.forwarding.store(self.mark, Ordering::Release);
                        dst_header.promote_mature();
                        header
                            .forwarding
                            .store(dst | FORWARDED_BIT, Ordering::Release);
                    }
                    let block = &mut self.heap.blocks[self.evacuation.block];
                    block.line_marks.mark_object(dst - block.base_addr(), size);
                    self.moved_bytes += size;
                    self.moved_objects += 1;
                    atomic.store(dst, Ordering::Relaxed);
                    Some((word, Address::from_usize(dst)))
                } else {
                    self.heap.blocks[index]
                        .line_marks
                        .mark_object(addr - base, size);
                    header.promote_mature();
                    header.forwarding.store(self.mark, Ordering::Release);
                    Some((word, address))
                }
            }
        }
    }

    #[inline]
    fn block_for(&mut self, addr: usize) -> Option<usize> {
        let number = addr >> BLOCK_SHIFT;
        let (cached, index) = self.cached_block;
        if number == cached {
            return Some(index);
        }
        let index = *self.heap.block_index.get(&number)?;
        self.cached_block = (number, index);
        Some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeBuilder;
    use crate::heap::string::LispString;
    use crate::heap::Heap;

    #[test]
    fn committed_heap_bytes_fall_to_zero_after_the_last_roots_are_dropped() {
        let runtime = RuntimeBuilder::new().heap_size_bytes(1).start().unwrap();
        let initial = runtime.heap_stats();
        assert_eq!(initial.allocation_limit_bytes, 1 << 20);
        assert_eq!(initial.committed_bytes, 0);
        assert_eq!(initial.collections, 0);
        assert_eq!(initial.last_collection_duration, None);

        let small = runtime.make_string("small").unwrap();
        let text = "a".repeat(MAX_IMMIX_OBJECT_BYTES + 1);
        let large = runtime.make_string(&text).unwrap();
        let allocated = runtime.heap_stats();
        let large_bytes = (text.len() + size_of::<LispString>()).next_multiple_of(OBJECT_ALIGN);
        assert_eq!(allocated.large_object_bytes, large_bytes);
        assert_eq!(allocated.committed_bytes, BLOCK_SIZE + large_bytes);

        runtime.collect();
        let live = runtime.heap_stats();
        assert_eq!(live.collections, 1);
        assert!(live.last_collection_duration.is_some());
        assert_eq!(live.large_object_bytes, large_bytes);
        assert_eq!(live.committed_bytes, allocated.committed_bytes);

        drop(large);
        drop(small);
        runtime.collect();
        let reclaimed = runtime.heap_stats();
        assert_eq!(reclaimed.collections, 2);
        assert_eq!(reclaimed.large_object_bytes, 0);
        assert_eq!(reclaimed.committed_bytes, 0);
    }

    #[test]
    fn automatic_collections_are_counted_only_for_the_owning_runtime() {
        let runtime = RuntimeBuilder::new()
            .heap_size_bytes(1 << 20)
            .start()
            .unwrap();
        let other = RuntimeBuilder::new().start().unwrap();
        let other_initial = other.heap_stats();
        let text = "a".repeat(1 << 19);

        drop(runtime.make_string(&text).unwrap());
        assert_eq!(runtime.heap_stats().collections, 0);
        let large = runtime.make_string(&text).unwrap();

        let collected = runtime.heap_stats();
        assert_eq!(collected.collections, 1);
        assert!(collected.last_collection_duration.is_some());
        assert_eq!(
            collected.large_object_bytes,
            text.len() + size_of::<LispString>()
        );
        assert_eq!(collected.committed_bytes, collected.large_object_bytes);
        assert_eq!(other.heap_stats(), other_initial);
        assert_eq!(runtime.heap.gc_epoch(), 1);
        assert_eq!(other.heap.gc_epoch(), 0);
        drop(large);
    }

    #[test]
    fn low_survival_allocations_collect_before_the_heap_limit() {
        let runtime = RuntimeBuilder::new().start().unwrap();
        let retained = runtime.make_string("retained").unwrap();
        let text = "a".repeat(512 << 10);
        for _ in 0..40 {
            drop(runtime.make_string(&text).unwrap());
            assert!(runtime.heap_stats().committed_bytes < 10 << 20);
        }
        assert!(runtime.heap_stats().collections >= 2);
        assert_eq!(
            runtime.heap.string_contents(retained.slot.get()).as_deref(),
            Some("retained")
        );
    }

    #[test]
    fn medium_objects_reuse_free_lines_at_the_heap_limit() {
        let heap = Heap::start(Heap::builder_with_heap_size(1 << 20)).unwrap();
        let length = MAX_IMMIX_OBJECT_BYTES - size_of::<LispString>();
        let live_text = "a".repeat(length);
        let dead_text = "b".repeat(length);
        let block_count = heap.max_heap_bytes() / BLOCK_SIZE;
        let live: Vec<_> = (0..block_count)
            .map(|_| {
                let live = heap.root_slot(heap.string(&live_text).unwrap());
                for _ in 1..BLOCK_SIZE / MAX_IMMIX_OBJECT_BYTES {
                    heap.string(&dead_text).unwrap();
                }
                live
            })
            .collect();

        let recycled = heap
            .string(&dead_text)
            .expect("medium object fits in a recycled gap");
        assert_eq!(
            heap.string_contents(recycled).as_deref(),
            Some(dead_text.as_str())
        );
        for root in live {
            assert_eq!(
                heap.string_contents(root.get()).as_deref(),
                Some(live_text.as_str())
            );
        }
    }

    #[test]
    fn allocation_can_compact_partly_live_blocks_to_make_room_for_a_large_object() {
        let runtime = RuntimeBuilder::new()
            .heap_size_bytes(1 << 20)
            .start()
            .unwrap();
        let live_text = "a".repeat(MAX_IMMIX_OBJECT_BYTES - size_of::<LispString>());
        let dead_text = "b".repeat(live_text.len());
        let roots: Vec<_> = (0..(1 << 20) / BLOCK_SIZE)
            .map(|_| {
                let root = runtime.make_string(&live_text).unwrap();
                for _ in 1..BLOCK_SIZE / MAX_IMMIX_OBJECT_BYTES {
                    drop(runtime.make_string(&dead_text).unwrap());
                }
                root
            })
            .collect();
        assert_eq!(runtime.heap_stats().committed_bytes, 1 << 20);

        let large_text = "l".repeat(1 << 19);
        let large = runtime.make_string(&large_text).unwrap();
        let stats = runtime.heap_stats();
        assert_eq!(stats.collections, 2);
        assert_eq!(stats.compaction_retries, 1);
        assert_eq!(stats.live_bytes, roots.len() * MAX_IMMIX_OBJECT_BYTES);
        assert_eq!(
            stats.allocated_bytes_since_collection,
            stats.large_object_bytes
        );
        assert!(stats.last_reclaimed_bytes >= 1 << 19);
        assert!(stats.last_evacuated_objects > 0);
        assert!(stats.committed_bytes <= stats.allocation_limit_bytes);
        assert_eq!(
            runtime.heap.string_contents(large.slot.get()).as_deref(),
            Some(large_text.as_str())
        );
        for root in roots {
            assert_eq!(
                runtime.heap.string_contents(root.slot.get()).as_deref(),
                Some(live_text.as_str())
            );
        }
    }

    #[test]
    fn unrepresentable_aligned_size_is_out_of_memory() {
        let mut heap = ImmixHeap::new(HeapConfig::default());
        assert!(matches!(heap.try_allocate(usize::MAX), Err(OutOfMemory)));
    }

    #[test]
    fn header_keeps_three_word_layout_and_tracks_flags() {
        use crate::heap::word::{GC_FLAG_PINNED, GC_FLAG_UNLOGGED};

        assert_eq!(size_of::<Header>(), 3 * size_of::<usize>());
        let heap = Heap::start(HeapConfig::default()).unwrap();
        let word = heap.root_slot(heap.string("flagged").unwrap());
        let header = unsafe { &*word.get().object_address().unwrap().to_ptr::<Header>() };
        assert_eq!(header.gc_flags(), GC_FLAG_UNLOGGED);
        assert!(!header.is_pinned());

        header.set_pinned(true);
        assert!(header.is_pinned());
        assert_eq!(
            header.gc_flags() & (GC_FLAG_PINNED | GC_FLAG_UNLOGGED),
            GC_FLAG_PINNED | GC_FLAG_UNLOGGED
        );

        header.set_pinned(false);
        assert!(!header.is_pinned());
        assert_eq!(header.gc_flags(), GC_FLAG_UNLOGGED);
    }

    #[test]
    fn pinned_objects_resist_evacuation_until_unpinned() {
        let heap = Heap::start(HeapConfig {
            always_defrag: true,
            ..HeapConfig::default()
        })
        .unwrap();
        let pinned = heap.root_slot(heap.string("pinned-payload").unwrap());
        let movable = heap.root_slot(heap.string("movable-payload").unwrap());
        let pinned_before = pinned.get();
        let movable_before = movable.get();
        assert_eq!(
            pinned_before.raw() >> BLOCK_SHIFT,
            movable_before.raw() >> BLOCK_SHIFT
        );

        let header = unsafe { &*pinned_before.object_address().unwrap().to_ptr::<Header>() };
        header.set_pinned(true);

        heap.collect();
        assert_eq!(pinned.get(), pinned_before);
        assert_ne!(movable.get(), movable_before);
        assert_eq!(heap.string_contents(pinned.get()).as_deref(), Some("pinned-payload"));
        assert_eq!(heap.string_contents(movable.get()).as_deref(), Some("movable-payload"));

        let header = unsafe { &*pinned.get().object_address().unwrap().to_ptr::<Header>() };
        header.set_pinned(false);
        heap.collect();
        assert_ne!(pinned.get(), pinned_before);
        assert_eq!(heap.string_contents(pinned.get()).as_deref(), Some("pinned-payload"));
    }

    fn is_mature(word: Word) -> bool {
        let address = word.object_address().expect("heap object");
        unsafe { (*address.to_ptr::<Header>()).is_mature() }
    }

    #[test]
    fn minor_collection_reclaims_dead_young_objects_without_major_gc() {
        let runtime = RuntimeBuilder::new()
            .heap_size_bytes(32 << 20)
            .start()
            .unwrap();
        let retained = runtime.make_string("mature-retained").unwrap();
        runtime.collect();
        assert!(is_mature(retained.slot.get()));

        let churn = "x".repeat(4000);
        for _ in 0..2500 {
            drop(runtime.make_string(&churn).unwrap());
        }

        let stats = runtime.heap_stats();
        assert!(stats.minor_collections >= 3, "expected minor GCs, got {stats:?}");
        assert_eq!(stats.major_collections, 1);
        assert!(stats.committed_bytes < 4 << 20);
        assert_eq!(
            runtime.heap.string_contents(retained.slot.get()).as_deref(),
            Some("mature-retained")
        );
    }

    #[test]
    fn write_barrier_preserves_young_objects_in_mature_containers() {
        use crate::heap::{ArrayElement, HashTest};
        use crate::symbol::SymbolAttributes;
        use zkcl_bytecode::RehashHint;

        let heap = Heap::start(HeapConfig::default()).unwrap();
        let pair = heap.root_slot(heap.cons(|| (Word::NIL, Word::NIL)).unwrap());
        let array = heap.root_slot(
            heap.array(&[2], ArrayElement::General, false, None, |_| Word::NIL)
                .unwrap(),
        );
        let structure = heap.root_slot(heap.structure(1, || Word::NIL, |_| Word::NIL).unwrap());
        let symbol = heap.root_slot(
            heap.symbol("GEN-BARRIER-SYM", SymbolAttributes::new(9999, None, false, false))
                .unwrap(),
        );
        let table = heap.root_slot(
            heap.hash_table(
                HashTest::Eq,
                None,
                8,
                RehashHint::integer(2),
                RehashHint::integer(1),
            )
            .unwrap(),
        );

        heap.collect();
        assert!(is_mature(pair.get()));
        assert!(is_mature(array.get()));
        assert!(is_mature(structure.get()));
        assert!(is_mature(symbol.get()));
        assert!(is_mature(table.get()));

        let s_cons = heap.string("young-cons").unwrap();
        let s_array = heap.string("young-array").unwrap();
        let s_struct = heap.string("young-struct").unwrap();
        let s_sym = heap.string("young-sym").unwrap();
        let k_ht = heap.string("young-ht-key").unwrap();
        let v_ht = heap.string("young-ht-val").unwrap();
        for young in [s_cons, s_array, s_struct, s_sym, k_ht, v_ht] {
            assert!(!is_mature(young));
        }

        heap.set_cons_car(pair.get(), s_cons);
        assert!(heap.set_array_slot(array.get(), 1, s_array).unwrap());
        assert!(heap.set_structure_slot(structure.get(), 0, s_struct).is_some());
        heap.set_symbol_cell(symbol.get(), false, s_sym);
        let ht_hash = heap.identity_hash(k_ht);
        heap.hash_table_set(table.get(), k_ht, v_ht, ht_hash, |l, r| l == r)
            .unwrap();

        heap.collect_minor();
        let stats = heap.stats();
        assert_eq!(stats.minor_collections, 1);
        assert_eq!(stats.major_collections, 1);

        let cons_val = heap.cons_fields(pair.get()).unwrap().0;
        let array_val = heap.array_slot(array.get(), 1).unwrap().unwrap();
        let struct_val = heap.structure_slot(structure.get(), 0).unwrap();
        let sym_val = heap.symbol_cell(symbol.get(), false).unwrap();
        let mut cursor = 0;
        let (ht_key, ht_val) = heap.hash_table_next_entry(table.get(), &mut cursor).unwrap();

        assert!(is_mature(cons_val));
        assert!(is_mature(array_val));
        assert!(is_mature(struct_val));
        assert!(is_mature(sym_val));
        assert!(is_mature(ht_key));
        assert!(is_mature(ht_val));
        assert_eq!(heap.string_contents(cons_val).as_deref(), Some("young-cons"));
        assert_eq!(heap.string_contents(array_val).as_deref(), Some("young-array"));
        assert_eq!(heap.string_contents(struct_val).as_deref(), Some("young-struct"));
        assert_eq!(heap.string_contents(sym_val).as_deref(), Some("young-sym"));
        assert_eq!(heap.string_contents(ht_key).as_deref(), Some("young-ht-key"));
        assert_eq!(heap.string_contents(ht_val).as_deref(), Some("young-ht-val"));
    }

    #[test]
    fn minor_collection_handles_mature_weak_hash_table_with_young_entries() {
        use crate::heap::HashTest;
        use zkcl_bytecode::{HashTableWeakness, RehashHint};

        let heap = Heap::start(HeapConfig::default()).unwrap();
        let table = heap.root_slot(
            heap.hash_table(
                HashTest::Eq,
                Some(HashTableWeakness::Key),
                8,
                RehashHint::integer(2),
                RehashHint::integer(1),
            )
            .unwrap(),
        );
        let mature_key = heap.root_slot(heap.string("mature-key").unwrap());
        heap.collect();

        let young_live_val = heap.string("kept-young-value").unwrap();
        let dead_young_key = heap.string("dead-young-key").unwrap();
        let dead_young_val = heap.string("dead-young-val").unwrap();
        let mature_hash = heap.identity_hash(mature_key.get());
        let dead_hash = heap.identity_hash(dead_young_key);
        heap.hash_table_set(
            table.get(),
            mature_key.get(),
            young_live_val,
            mature_hash,
            |l, r| l == r,
        )
        .unwrap();
        heap.hash_table_set(
            table.get(),
            dead_young_key,
            dead_young_val,
            dead_hash,
            |l, r| l == r,
        )
        .unwrap();

        heap.collect_minor();
        let stats = heap.stats();
        assert_eq!(stats.minor_collections, 1);
        assert_eq!(stats.major_collections, 1);
        assert_eq!(heap.hash_table_info(table.get()).unwrap().count, 1);

        let retained_val = heap
            .hash_table_get(table.get(), mature_key.get(), mature_hash, |l, r| l == r)
            .expect("live mature key preserves young value across minor GC");
        assert!(is_mature(retained_val));
        assert_eq!(
            heap.string_contents(retained_val).as_deref(),
            Some("kept-young-value")
        );
    }

    #[test]
    fn parallel_claim_object_evacuates_once_and_forwards_competitors() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let mark = (7usize << 2) | MARKED_BIT;
        let dst = BLOCK_SIZE * 4;
        let header = Arc::new(Header::new(crate::heap::word::ObjectKind::Cons, 24));
        let start = Arc::new(Barrier::new(4));

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let header = Arc::clone(&header);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    start.wait();
                    match claim_object(&header, mark, true) {
                        ObjectClaim::ClaimedEvacuate => {
                            header
                                .forwarding
                                .store(dst | FORWARDED_BIT, Ordering::Release);
                            true
                        }
                        ObjectClaim::Forwarded(observed) => {
                            assert_eq!(observed, dst);
                            false
                        }
                        other => panic!("unexpected claim outcome under evacuation race: {other:?}"),
                    }
                })
            })
            .collect();

        let winners: usize = handles
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum();
        assert_eq!(winners, 1);
    }
}
