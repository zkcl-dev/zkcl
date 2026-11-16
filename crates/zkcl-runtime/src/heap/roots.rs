//! GC root storage and the live slots traced by each heap.

use std::cell::{Cell as LocalCell, RefCell, RefMut};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak as SyncWeak};

use rustc_hash::FxHashSet;

use super::{Address, Heap, ValueSlot, Word};

pub(super) struct HeapRoots {
    host: Arc<ScalarRoots>,
    stacks: RefCell<Vec<Weak<RootStackInner>>>,
    stack_ranges: Rc<RootRangeRegistry>,
    singles: Arc<ScalarRoots>,
    frames: Rc<RootFrameAllocator>,
}

impl HeapRoots {
    pub(super) fn new() -> Self {
        Self {
            host: Arc::default(),
            stacks: RefCell::default(),
            stack_ranges: Rc::default(),
            singles: Arc::default(),
            frames: Rc::default(),
        }
    }

    /// Visit all live root slots owned by this heap while the mutator is stopped.
    pub(super) fn visit_slots(&self, mut visit: impl FnMut(ValueSlot)) {
        for slot in self.host.lock().unwrap().iter() {
            visit(ValueSlot(Address::from_ptr(&slot.value)));
        }
        for slot in self.singles.lock().unwrap().iter() {
            visit(ValueSlot(Address::from_ptr(&slot.value)));
        }
        for &range in self.frames.ranges.borrow().iter() {
            range.visit_slots(&mut visit);
        }
        for &range in self.stack_ranges.borrow().iter() {
            range.visit_slots(&mut visit);
        }
        self.stacks.borrow_mut().retain(|weak| {
            let Some(inner) = weak.upgrade() else {
                return false;
            };
            let mut state = inner.state.borrow_mut();
            let top = inner.top.get();
            let mut remaining = top;
            for segment in &state.segments {
                let count = remaining.min(STACK_SEGMENT_SLOTS);
                if count == 0 {
                    break;
                }
                RootSlotRange::new(&segment[..count]).visit_slots(&mut visit);
                remaining -= count;
            }
            // Frames above top have ended. Keep a spare segment for the next call
            // and release the rest of a deep stack's high-water allocation.
            state
                .segments
                .truncate(top.div_ceil(STACK_SEGMENT_SLOTS) + 1);
            let retained_records = state.allocations.len().saturating_mul(2).max(128);
            if state.allocations.capacity() > retained_records.saturating_mul(2) {
                state.allocations.shrink_to(retained_records);
            }
            true
        });
        for page in &self.frames.slab_pool.borrow().pages {
            RootSlotRange::new(&page.slots[..page.used.get()]).visit_slots(&mut visit);
        }
    }

    #[cfg(test)]
    fn slot_addresses(&self) -> Vec<ValueSlot> {
        let mut slots = Vec::new();
        self.visit_slots(|slot| slots.push(slot));
        slots
    }
}

impl Heap {
    pub(crate) fn root_slot(&self, word: Word) -> RootSlot {
        RootSlot::register_if_object(word, &self.roots.singles)
    }

    pub(crate) fn root_frame(&self, len: usize) -> RootFrame {
        self.roots.frames.alloc(len)
    }

    pub(crate) fn root_frame_allocator(&self) -> Rc<RootFrameAllocator> {
        Rc::clone(&self.roots.frames)
    }

    pub(crate) fn root_for_host(&self, word: Word) -> RootSlot {
        // A Rust handle's storage can survive the runtime, for instance if
        // the caller forgets it. Only this heap may consider its slot a root.
        RootSlot::register_if_object(word, &self.roots.host)
    }

    pub(crate) fn root_registry(&self) -> HeapRootRegistry {
        HeapRootRegistry(Arc::clone(&self.roots.host))
    }

    pub(crate) fn root_stack(&self) -> RootStack {
        RootStack::new(&self.roots)
    }
}

type ScalarRoots = Mutex<Vec<Arc<ScalarRoot>>>;

pub(crate) struct HeapRootRegistry(Arc<ScalarRoots>);

impl HeapRootRegistry {
    pub(crate) fn root(&self, word: Word) -> RootSlot {
        RootSlot::register_if_object(word, &self.0)
    }
}

/// A Lisp value retained by a host between calls, independent of a Rust borrow.
/// It stays rooted until dropped or until its runtime is destroyed.
pub struct PersistentRoot {
    slot: RootSlot,
    owner: SyncWeak<ScalarRoots>,
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PersistentRoot>();
    assert_send_sync::<RootSlot>();
};

impl crate::Root<'_> {
    /// Transfer this root into a handle that can be stored independently of the borrow.
    pub fn into_persistent(self) -> PersistentRoot {
        PersistentRoot {
            slot: self.slot,
            owner: Arc::downgrade(&self.owner.heap.roots.host),
        }
    }
}

impl PersistentRoot {
    /// Borrow the retained value through its original, live runtime.
    pub fn bind<'rt>(&self, runtime: &'rt crate::Runtime) -> Option<crate::Root<'rt>> {
        self.slot_for(runtime).map(|slot| crate::Root {
            slot: slot.clone(),
            owner: runtime,
        })
    }

    pub(crate) fn slot_for(&self, runtime: &crate::Runtime) -> Option<&RootSlot> {
        // The weak reference keeps the registry identity unique even after teardown.
        std::ptr::eq(self.owner.as_ptr(), Arc::as_ptr(&runtime.heap.roots.host))
            .then_some(&self.slot)
    }
}

/// Slots visited only through a reachable Lisp foreign object.
#[derive(Clone)]
pub(crate) struct ForeignEdge {
    values: Rc<RefCell<ForeignSlots>>,
    owner: SyncWeak<ScalarRoots>,
}

pub(super) enum ForeignSlots {
    Empty,
    One(AtomicUsize),
    Many(Box<[AtomicUsize]>),
    Queue(VecDeque<AtomicUsize>),
}

impl ForeignSlots {
    pub(super) fn slices(&self) -> [&[AtomicUsize]; 2] {
        let first = match self {
            Self::Empty => &[],
            Self::One(slot) => std::slice::from_ref(slot),
            Self::Many(slots) => slots,
            Self::Queue(queue) => {
                let (front, back) = queue.as_slices();
                return [front, back];
            }
        };
        [first, &[]]
    }
}

impl ForeignEdge {
    pub(crate) fn empty(rt: &crate::Runtime) -> Self {
        Self::new(rt, ForeignSlots::Empty)
    }

    fn new(rt: &crate::Runtime, values: ForeignSlots) -> Self {
        Self {
            values: Rc::new(RefCell::new(values)),
            owner: Arc::downgrade(&rt.heap.roots.host),
        }
    }

    pub(super) fn slot_storage(&self) -> std::ptr::NonNull<RefCell<ForeignSlots>> {
        std::ptr::NonNull::from(self.values.as_ref())
    }

    pub(crate) fn belongs_to(&self, heap: &Heap) -> bool {
        std::ptr::eq(self.owner.as_ptr(), Arc::as_ptr(&heap.roots.host))
    }

    pub(crate) fn set_words(&self, mut values: impl ExactSizeIterator<Item = Word>) {
        *self.values.borrow_mut() = match values.len() {
            0 => ForeignSlots::Empty,
            1 => ForeignSlots::One(AtomicUsize::new(values.next().unwrap().0)),
            _ => ForeignSlots::Many(values.map(|word| AtomicUsize::new(word.0)).collect()),
        };
    }

    pub(crate) fn with_slots<R>(&self, read: impl FnOnce(&RootSlots) -> R) -> R {
        let values = self.values.borrow();
        debug_assert!(!matches!(*values, ForeignSlots::Queue(_)));
        read(RootSlots::from_slice(values.slices()[0]))
    }

    #[allow(dead_code)]
    pub(crate) fn set(&self, value: &crate::Root<'_>) {
        assert!(self.belongs_to(&value.owner.heap));
        self.set_words(std::iter::once(value.slot.get()));
    }

    #[allow(dead_code)]
    pub(crate) fn bind<'rt>(&self, rt: &'rt crate::Runtime) -> Option<crate::Root<'rt>> {
        self.belongs_to(&rt.heap).then(|| {
            self.with_slots(|slots| {
                rt.root(if slots.is_empty() {
                    Word::NIL
                } else {
                    slots.get(0)
                })
            })
        })
    }
}

/// A FIFO of Lisp values traced through its owning foreign object. Neither
/// growing the queue nor popping a value can trigger a Lisp collection.
pub(crate) struct ForeignQueue(ForeignEdge);

impl ForeignQueue {
    pub(crate) fn new(runtime: &crate::Runtime) -> Self {
        Self(ForeignEdge::new(
            runtime,
            ForeignSlots::Queue(VecDeque::new()),
        ))
    }

    pub(crate) fn edge(&self) -> ForeignEdge {
        self.0.clone()
    }

    pub(crate) fn try_push(&self, word: Word, limit: Option<usize>) -> bool {
        let mut queue = self.queue_mut();
        if limit.is_some_and(|limit| queue.len() == limit) {
            return false;
        }
        queue.push_back(AtomicUsize::new(word.0));
        true
    }

    pub(crate) fn pop(&self) -> Option<Word> {
        self.queue_mut()
            .pop_front()
            .map(|slot| Word(slot.into_inner()))
    }

    fn queue_mut(&self) -> RefMut<'_, VecDeque<AtomicUsize>> {
        RefMut::map(self.0.values.borrow_mut(), |values| match values {
            ForeignSlots::Queue(queue) => queue,
            _ => unreachable!("queue edge holds queue slots"),
        })
    }
}

pub(crate) struct ScalarRoot {
    value: AtomicUsize,
    index: AtomicUsize,
}

pub(crate) enum RootSlot {
    Immediate(Word),
    Object {
        owner: SyncWeak<ScalarRoots>,
        slot: ManuallyDrop<Arc<ScalarRoot>>,
    },
}

impl Clone for RootSlot {
    fn clone(&self) -> Self {
        match self {
            Self::Immediate(word) => Self::Immediate(*word),
            Self::Object { owner, slot } => Self::Object {
                owner: owner.clone(),
                slot: ManuallyDrop::new(Arc::clone(slot)),
            },
        }
    }
}

impl RootSlot {
    pub(crate) const fn nil() -> Self {
        Self::Immediate(Word::NIL)
    }

    pub(crate) fn immediate(word: Word) -> Self {
        debug_assert!(word.is_immediate());
        Self::Immediate(word)
    }

    fn register_if_object(word: Word, owner: &Arc<ScalarRoots>) -> Self {
        if word.is_immediate() {
            Self::Immediate(word)
        } else {
            Self::register(word, owner)
        }
    }

    fn register(word: Word, owner: &Arc<ScalarRoots>) -> Self {
        let mut roots = owner.lock().unwrap();
        let slot = Arc::new(ScalarRoot {
            value: AtomicUsize::new(word.0),
            index: AtomicUsize::new(roots.len()),
        });
        crate::metrics::record_root_registration();
        roots.push(Arc::clone(&slot));
        Self::Object {
            owner: Arc::downgrade(owner),
            slot: ManuallyDrop::new(slot),
        }
    }

    pub(crate) fn get(&self) -> Word {
        match self {
            Self::Immediate(word) => *word,
            Self::Object { slot, .. } => Word(slot.value.load(Ordering::Relaxed)),
        }
    }
}

impl Drop for RootSlot {
    fn drop(&mut self) {
        let Self::Object { owner, slot } = self else {
            return;
        };
        if let Some(owner) = owner.upgrade() {
            let mut roots = owner.lock().unwrap();
            // Serialize the final external refcount check, swap_remove, and handle drop under `owner`
            // so concurrent drops of the last two handles never race and leak the registry entry.
            if Arc::strong_count(slot) == 2 {
                let index = slot.index.load(Ordering::Relaxed);
                roots.swap_remove(index);
                // The value cells stay at stable addresses during a collection.
                // Only the index used to release the moved handle changes.
                if let Some(moved) = roots.get(index) {
                    moved.index.store(index, Ordering::Relaxed);
                }
            }
            // SAFETY: `self` is being dropped once and `slot` is never accessed again.
            unsafe { ManuallyDrop::drop(slot) };
        } else {
            // SAFETY: `self` is being dropped once and `slot` is never accessed again.
            unsafe { ManuallyDrop::drop(slot) };
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RootSlotRange {
    start: Address,
    len: usize,
}

impl RootSlotRange {
    fn new(slots: &[AtomicUsize]) -> Self {
        Self {
            start: Address::from_ptr(slots.as_ptr()),
            len: slots.len(),
        }
    }

    fn visit_slots(self, mut visit: impl FnMut(ValueSlot)) {
        for i in 0..self.len {
            visit(ValueSlot(self.start + i * size_of::<AtomicUsize>()));
        }
    }
}

type RootRangeRegistry = RefCell<FxHashSet<RootSlotRange>>;

struct RegisteredSlots {
    slots: Box<[AtomicUsize]>,
    owner: Weak<RootRangeRegistry>,
}

impl RegisteredSlots {
    fn new(len: usize, owner: &Rc<RootRangeRegistry>) -> Self {
        crate::metrics::record_root_frame();
        let slots: Box<[_]> = (0..len).map(|_| AtomicUsize::new(Word::NIL.0)).collect();
        crate::metrics::record_root_registration();
        owner.borrow_mut().insert(RootSlotRange::new(&slots));
        Self {
            slots,
            owner: Rc::downgrade(owner),
        }
    }
}

impl Drop for RegisteredSlots {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.borrow_mut().remove(&RootSlotRange::new(&self.slots));
        }
    }
}

/// A borrowed contiguous slice of GC-traced value slots.
#[repr(transparent)]
pub struct RootSlots([AtomicUsize]);

impl RootSlots {
    pub fn empty() -> &'static Self {
        static EMPTY: [AtomicUsize; 0] = [];
        Self::from_slice(&EMPTY)
    }

    pub(crate) fn from_slice(slots: &[AtomicUsize]) -> &Self {
        // SAFETY: `RootSlots` is `#[repr(transparent)]` over `[AtomicUsize]`.
        unsafe { &*(slots as *const [AtomicUsize] as *const Self) }
    }

    #[inline(always)]
    pub fn get(&self, index: usize) -> Word {
        Word(self.0[index].load(Ordering::Relaxed))
    }

    #[inline(always)]
    pub fn set(&self, index: usize, word: Word) {
        self.0[index].store(word.0, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn clear(&self, range: std::ops::Range<usize>) {
        for slot in &self.0[range] {
            slot.store(Word::NIL.0, Ordering::Relaxed);
        }
    }

    #[inline]
    pub(crate) fn move_to_start(&self, source: std::ops::Range<usize>) {
        if source.start == 0 {
            return;
        }
        // Copying toward a lower index also handles overlapping ranges.
        for (dst, src) in self.0.iter().zip(&self.0[source]) {
            dst.store(src.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn slice(&self, range: std::ops::Range<usize>) -> &Self {
        Self::from_slice(&self.0[range])
    }

    #[inline(always)]
    pub(crate) fn as_ptr(&self) -> *const AtomicUsize {
        self.0.as_ptr()
    }
}

const SLAB_PAGE_SLOTS: usize = 4096;
const SLAB_CLASSES: [usize; 4] = [8, 16, 32, 64];

struct SlabPage {
    used: LocalCell<usize>,
    slots: Box<[AtomicUsize]>,
}

struct SlabBlock {
    page: Rc<SlabPage>,
    offset: usize,
}

impl SlabBlock {
    fn slots(&self, len: usize) -> &[AtomicUsize] {
        &self.page.slots[self.offset..self.offset + len]
    }
}

#[derive(Default)]
struct RootSlabPool {
    free: [Vec<SlabBlock>; 4],
    pages: Vec<Rc<SlabPage>>,
}

impl RootSlabPool {
    fn alloc(&mut self, class: usize) -> SlabBlock {
        if let Some(block) = self.free[class].pop() {
            return block;
        }
        let block_size = SLAB_CLASSES[class];
        if self
            .pages
            .last()
            .is_none_or(|page| page.used.get() + block_size > SLAB_PAGE_SLOTS)
        {
            if let Some(page) = self.pages.last() {
                let mut offset = page.used.get();
                for (class, size) in SLAB_CLASSES.into_iter().enumerate().rev() {
                    while offset + size <= SLAB_PAGE_SLOTS {
                        self.free[class].push(SlabBlock {
                            page: Rc::clone(page),
                            offset,
                        });
                        offset += size;
                    }
                }
                page.used.set(offset);
            }
            let page = Rc::new(SlabPage {
                used: LocalCell::new(0),
                // Root frames are also created deep in macro expansion. Build
                // the page on the heap to avoid a large temporary stack array.
                slots: (0..SLAB_PAGE_SLOTS)
                    .map(|_| AtomicUsize::new(Word::NIL.0))
                    .collect(),
            });
            crate::metrics::record_root_registration();
            self.pages.push(page);
        }
        let page = self.pages.last().unwrap();
        let offset = page.used.get();
        page.used.set(offset + block_size);
        SlabBlock {
            page: Rc::clone(page),
            offset,
        }
    }
}

#[inline(always)]
fn slab_class_for_len(len: usize) -> Option<usize> {
    match len {
        1..=8 => Some(0),
        9..=16 => Some(1),
        17..=32 => Some(2),
        33..=64 => Some(3),
        _ => None,
    }
}

enum RootFrameStorage {
    Empty,
    Slab {
        block: SlabBlock,
        pool: Weak<RefCell<RootSlabPool>>,
        len: u16,
        class: u8,
    },
    Dedicated(RegisteredSlots),
}

/// The collector can rewrite every active slot at an allocation safepoint.
pub(crate) struct RootFrame(RootFrameStorage);

/// Allocates independently owned root frames registered with one heap.
#[derive(Default)]
pub(crate) struct RootFrameAllocator {
    ranges: Rc<RootRangeRegistry>,
    slab_pool: Rc<RefCell<RootSlabPool>>,
}

impl RootFrameAllocator {
    pub(crate) fn alloc(&self, len: usize) -> RootFrame {
        if len == 0 {
            return RootFrame::empty();
        }
        if let Some(class) = slab_class_for_len(len) {
            let block = self.slab_pool.borrow_mut().alloc(class);
            return RootFrame(RootFrameStorage::Slab {
                block,
                pool: Rc::downgrade(&self.slab_pool),
                len: len as u16,
                class: class as u8,
            });
        }
        RootFrame(RootFrameStorage::Dedicated(RegisteredSlots::new(
            len,
            &self.ranges,
        )))
    }
}

impl RootFrame {
    pub(crate) const fn empty() -> Self {
        Self(RootFrameStorage::Empty)
    }

    #[inline(always)]
    pub(crate) fn get(&self, index: usize) -> Word {
        RootSlots::get(self, index)
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        RootSlots::len(self)
    }

    #[inline(always)]
    pub(crate) fn set(&self, index: usize, word: Word) {
        RootSlots::set(self, index, word)
    }
}

impl Drop for RootFrame {
    fn drop(&mut self) {
        if let RootFrameStorage::Slab {
            block,
            pool,
            len,
            class,
        } = std::mem::replace(&mut self.0, RootFrameStorage::Empty)
        {
            for slot in block.slots(len as usize) {
                slot.store(Word::NIL.0, Ordering::Relaxed);
            }
            if let Some(pool) = pool.upgrade() {
                pool.borrow_mut().free[class as usize].push(block);
            }
        }
    }
}

impl std::ops::Deref for RootFrame {
    type Target = RootSlots;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        match &self.0 {
            RootFrameStorage::Empty => RootSlots::empty(),
            RootFrameStorage::Slab { block, len, .. } => {
                RootSlots::from_slice(block.slots(*len as usize))
            }
            RootFrameStorage::Dedicated(buffer) => RootSlots::from_slice(&buffer.slots),
        }
    }
}

const STACK_SEGMENT_SLOTS: usize = 4096;

struct RootStackInner {
    top: LocalCell<usize>,
    state: RefCell<RootStackState>,
}

struct StackAllocation {
    previous_top: usize,
    released: bool,
}

struct RootStackState {
    segments: Vec<Box<[AtomicUsize]>>,
    allocations: Vec<StackAllocation>,
}

/// Segmented root stack with a bump pointer for VM call frames.
pub(crate) struct RootStack {
    inner: Rc<RootStackInner>,
    ranges: Rc<RootRangeRegistry>,
}

impl RootStack {
    fn new(roots: &HeapRoots) -> Self {
        let inner = Rc::new(RootStackInner {
            top: LocalCell::new(0),
            state: RefCell::new(RootStackState {
                segments: Vec::new(),
                allocations: Vec::new(),
            }),
        });
        roots.stacks.borrow_mut().push(Rc::downgrade(&inner));
        Self {
            inner,
            ranges: Rc::clone(&roots.stack_ranges),
        }
    }

    pub(crate) fn top(&self) -> usize {
        self.inner.top.get()
    }

    #[cold]
    #[inline(never)]
    fn segment() -> Box<[AtomicUsize]> {
        (0..STACK_SEGMENT_SLOTS)
            .map(|_| AtomicUsize::new(Word::NIL.0))
            .collect()
    }

    pub(crate) fn alloc(&self, len: usize) -> RootStackFrame<'_> {
        if len == 0 {
            return RootStackFrame(StackFrameStorage::Empty);
        }
        if len > STACK_SEGMENT_SLOTS {
            return RootStackFrame(StackFrameStorage::Overflow(RegisteredSlots::new(
                len,
                &self.ranges,
            )));
        }
        let previous_top = self.inner.top.get();
        let segment_index = previous_top / STACK_SEGMENT_SLOTS;
        let slot_offset = previous_top % STACK_SEGMENT_SLOTS;
        let mut state = self.inner.state.borrow_mut();
        let (segment_index, slot_offset) = if slot_offset + len <= STACK_SEGMENT_SLOTS {
            (segment_index, slot_offset)
        } else {
            // Clear the skipped tail so the collector sees no stale pointers below `new_top`.
            for slot in &state.segments[segment_index][slot_offset..] {
                slot.store(Word::NIL.0, Ordering::Relaxed);
            }
            (segment_index + 1, 0)
        };
        if segment_index == state.segments.len() {
            state.segments.push(Self::segment());
        }
        let slots = &state.segments[segment_index][slot_offset..slot_offset + len];
        for slot in slots {
            slot.store(Word::NIL.0, Ordering::Relaxed);
        }
        let slots = NonNull::from(RootSlots::from_slice(slots));
        let allocation = state.allocations.len();
        state.allocations.push(StackAllocation {
            previous_top,
            released: false,
        });
        let new_top = segment_index * STACK_SEGMENT_SLOTS + slot_offset + len;
        self.inner.top.set(new_top);
        RootStackFrame(StackFrameStorage::Segment(SegmentFrame {
            stack: self,
            allocation,
            slots,
        }))
    }

    fn release(&self, index: usize, slots: &RootSlots) {
        let mut state = self.inner.state.borrow_mut();
        let allocations = &mut state.allocations;
        if index + 1 < allocations.len() {
            // Replacements can allocate before dropping the old slice. Leave the
            // newer slices in the scanned prefix and clear the dead slots below them.
            allocations[index].released = true;
            for slot in &slots.0 {
                slot.store(Word::NIL.0, Ordering::Relaxed);
            }
            return;
        }
        let mut top = allocations
            .pop()
            .expect("live stack allocation")
            .previous_top;
        while allocations.last().is_some_and(|frame| frame.released) {
            top = allocations.pop().unwrap().previous_top;
        }
        self.inner.top.set(top);
    }
}

enum StackFrameStorage<'rt> {
    Empty,
    Segment(SegmentFrame<'rt>),
    Overflow(RegisteredSlots),
    Independent(RootFrame),
}

struct SegmentFrame<'rt> {
    stack: &'rt RootStack,
    allocation: usize,
    slots: NonNull<RootSlots>,
}

impl SegmentFrame<'_> {
    #[inline(always)]
    fn slots(&self) -> &RootSlots {
        // SAFETY: the stack owns the segment's boxed slots for the life of
        // this frame. It cannot reuse this allocation until release.
        unsafe { self.slots.as_ref() }
    }
}

impl Drop for SegmentFrame<'_> {
    fn drop(&mut self) {
        self.stack.release(self.allocation, self.slots());
    }
}

pub(crate) struct RootStackFrame<'rt>(StackFrameStorage<'rt>);

impl RootStackFrame<'_> {
    /// Storage for VM frames that can be released in a different task's stack order.
    pub(crate) fn independent(heap: &Heap, len: usize) -> Self {
        Self(StackFrameStorage::Independent(heap.root_frame(len)))
    }

    #[inline(always)]
    pub(crate) fn slots(&self) -> &RootSlots {
        match &self.0 {
            StackFrameStorage::Empty => RootSlots::empty(),
            StackFrameStorage::Segment(frame) => frame.slots(),
            StackFrameStorage::Overflow(buffer) => RootSlots::from_slice(&buffer.slots),
            StackFrameStorage::Independent(frame) => frame,
        }
    }
}

impl std::ops::Deref for RootStackFrame<'_> {
    type Target = RootSlots;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.slots()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::HeapConfig;

    #[test]
    fn root_snapshot_excludes_other_runtime_threads() {
        use std::sync::mpsc;

        let heap = Heap::start(HeapConfig::default()).unwrap();
        let frame = heap.root_frame(1);
        let local = heap.roots.slot_addresses();
        assert!(local.contains(&ValueSlot(Address::from_ptr(frame.as_ptr()))));

        let (ready, remote_slots) = mpsc::channel();
        let (release, done) = mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            let heap = Heap::start(HeapConfig::default()).unwrap();
            let object = heap.allocate_foreign(42).unwrap();
            let single = heap.root_slot(object);
            let small = heap.root_frame(1);
            let large = heap.root_frame(SLAB_CLASSES[3] + 1);
            let stack = heap.root_stack();
            let slice = stack.alloc(1);
            small.set(0, object);
            large.set(0, object);
            slice.set(0, object);
            ready.send(heap.roots.slot_addresses()).unwrap();
            let _ = done.recv();
            drop((single, small, large, slice));
        });

        let remote = remote_slots.recv().unwrap();
        assert!(!remote.is_empty());
        assert_eq!(heap.roots.slot_addresses(), local);
        assert!(remote.iter().all(|slot| !local.contains(slot)));
        drop(release);
        other.join().unwrap();
    }

    #[test]
    fn same_thread_heaps_isolate_roots_and_survive_drop_order() {
        let heap_a = Heap::start(HeapConfig {
            always_defrag: true,
            ..HeapConfig::default()
        })
        .unwrap();
        let heap_b = Heap::start(HeapConfig {
            always_defrag: true,
            ..HeapConfig::default()
        })
        .unwrap();

        let slot_b = heap_b.root_slot(heap_b.allocate_foreign(7).unwrap());
        let frame_b = heap_b.root_frame(1);
        frame_b.set(0, heap_b.allocate_foreign(8).unwrap());
        let before_b_slot = slot_b.get();
        let before_b_frame = frame_b.get(0);

        let slot_a = heap_a.root_slot(heap_a.allocate_foreign(1).unwrap());
        let frame_a = heap_a.root_frame(1);
        frame_a.set(0, heap_a.allocate_foreign(2).unwrap());

        let addrs_a = heap_a.roots.slot_addresses();
        let addrs_b = heap_b.roots.slot_addresses();
        assert!(addrs_a.iter().all(|slot| !addrs_b.contains(slot)));

        heap_a.collect();
        assert_eq!(slot_b.get(), before_b_slot);
        assert_eq!(frame_b.get(0), before_b_frame);
        assert_eq!(heap_a.foreign_id(slot_a.get()), Some(1));
        assert_eq!(heap_a.foreign_id(frame_a.get(0)), Some(2));

        drop(heap_b);
        drop((slot_b, frame_b));
    }

    #[test]
    fn owned_stack_segments_and_overflow_are_excluded_from_neighboring_heaps() {
        let heap = Heap::start(HeapConfig {
            always_defrag: true,
            ..HeapConfig::default()
        })
        .unwrap();
        let other = Heap::start(HeapConfig {
            always_defrag: true,
            ..HeapConfig::default()
        })
        .unwrap();
        let stack = heap.root_stack();
        let frame = stack.alloc(1);
        let overflow = stack.alloc(STACK_SEGMENT_SLOTS + 1);
        frame.set(0, heap.allocate_foreign(1).unwrap());
        overflow.set(0, heap.allocate_foreign(2).unwrap());

        let local = heap.roots.slot_addresses();
        let remote = other.roots.slot_addresses();
        for address in [frame.as_ptr(), overflow.as_ptr()] {
            let slot = ValueSlot(Address::from_ptr(address));
            assert!(local.contains(&slot));
            assert!(!remote.contains(&slot));
        }
        let before = frame.get(0);
        other.collect();
        assert_eq!(frame.get(0), before);
        heap.collect();
        assert_ne!(frame.get(0), before);
        assert_eq!(heap.foreign_id(frame.get(0)), Some(1));
        assert_eq!(heap.foreign_id(overflow.get(0)), Some(2));
    }

    #[test]
    fn heap_collects_from_a_thread_local_destructor() {
        use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
        use std::sync::mpsc;

        struct CollectOnDrop {
            heap: Heap,
            single: RootSlot,
            slab: RootFrame,
            owned: Option<RootFrame>,
            result: mpsc::Sender<std::thread::Result<()>>,
        }

        impl Drop for CollectOnDrop {
            fn drop(&mut self) {
                // Catch assertions here so a regression reports a normal test failure.
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let owned = self.owned.take().unwrap();
                    let released_slot = ValueSlot(Address::from_ptr(owned.as_ptr()));
                    drop(owned);
                    assert!(!self.heap.roots.slot_addresses().contains(&released_slot));

                    let host = self
                        .heap
                        .root_for_host(self.heap.allocate_foreign(4).unwrap());
                    self.heap.collect();
                    let (_, live) = self.heap.snapshot_live_foreigns(0).unwrap();
                    assert_eq!(live, [1, 2, 4].into_iter().collect());
                    assert_eq!(self.heap.foreign_id(self.single.get()), Some(1));
                    assert_eq!(self.heap.foreign_id(self.slab.get(0)), Some(2));
                    assert_eq!(self.heap.foreign_id(host.get()), Some(4));
                }));
                self.result.send(result).unwrap();
            }
        }

        thread_local! {
            static HOLDER: RefCell<Option<CollectOnDrop>> = const { RefCell::new(None) };
        }

        let (result, outcome) = mpsc::channel();
        std::thread::spawn(move || {
            HOLDER.with_borrow_mut(|holder| {
                let heap = Heap::start(HeapConfig {
                    always_defrag: true,
                    ..HeapConfig::default()
                })
                .unwrap();
                let single = heap.root_slot(heap.allocate_foreign(1).unwrap());
                let slab = heap.root_frame(1);
                slab.set(0, heap.allocate_foreign(2).unwrap());
                let owned = heap.root_frame(SLAB_CLASSES[3] + 1);
                owned.set(0, heap.allocate_foreign(3).unwrap());
                *holder = Some(CollectOnDrop {
                    heap,
                    single,
                    slab,
                    owned: Some(owned),
                    result,
                });
            });
        })
        .join()
        .unwrap();
        if let Err(panic) = outcome.recv().unwrap() {
            resume_unwind(panic);
        }
    }

    #[test]
    fn scalar_root_release_updates_the_handle_moved_into_its_slot() {
        let heap = Heap::start(HeapConfig {
            always_defrag: true,
            ..HeapConfig::default()
        })
        .unwrap();
        let mut roots: Vec<_> = (0..32)
            .map(|id| (id, heap.root_slot(heap.allocate_foreign(id).unwrap())))
            .collect();
        roots.retain(|(id, _)| id.is_multiple_of(3));
        heap.collect();
        assert_eq!(
            heap.snapshot_live_foreigns(0).unwrap().1,
            roots.iter().map(|(id, _)| *id).collect()
        );
        for (id, root) in roots {
            assert_eq!(heap.foreign_id(root.get()), Some(id));
        }
    }

    #[test]
    fn host_root_registry_belongs_to_its_heap() {
        let heap = Heap::start(HeapConfig::default()).unwrap();
        let root = heap.root_for_host(heap.allocate_foreign(1).unwrap());
        let RootSlot::Object { owner, slot } = &root else {
            panic!("expected a rooted object");
        };
        let owner = owner.clone();
        assert!(Arc::ptr_eq(
            &heap.roots.host.lock().unwrap()[slot.index.load(Ordering::Relaxed)],
            slot
        ));
        assert!(
            !heap
                .roots
                .singles
                .lock()
                .unwrap()
                .iter()
                .any(|entry| Arc::ptr_eq(entry, slot))
        );

        let retained = root.clone();
        assert_eq!(heap.roots.host.lock().unwrap().len(), 1);
        drop(root);
        heap.collect();
        assert!(heap.snapshot_live_foreigns(0).unwrap().1.contains(&1));
        assert_eq!(heap.foreign_id(retained.get()), Some(1));
        drop(retained);
        assert!(heap.roots.host.lock().unwrap().is_empty());

        let surviving_handle = heap.root_for_host(heap.allocate_foreign(2).unwrap());
        let clone = surviving_handle.clone();
        drop(heap);
        assert!(owner.upgrade().is_none());
        drop((surviving_handle, clone));
    }

    #[test]
    fn concurrent_root_slot_clone_and_drop_leaks_zero_entries() {
        use std::sync::Barrier;

        let heap = Heap::start(HeapConfig::default()).unwrap();
        let initial: Vec<_> = (0..64)
            .map(|i| heap.root_for_host(heap.allocate_foreign(i + 1).unwrap()))
            .collect();
        let barrier = Arc::new(Barrier::new(4));

        std::thread::scope(|scope| {
            for shard in 0..4 {
                let barrier = &barrier;
                let shared: Vec<_> = initial.iter().skip(shard).step_by(4).cloned().collect();
                scope.spawn(move || {
                    barrier.wait();
                    let mut copies = Vec::with_capacity(shared.len() * 16);
                    for handle in &shared {
                        for _ in 0..16 {
                            copies.push(handle.clone());
                        }
                    }
                    drop(copies);
                    drop(shared);
                });
            }
        });

        drop(initial);
        assert!(heap.roots.host.lock().unwrap().is_empty());
        heap.collect();
        assert!(heap.snapshot_live_foreigns(0).unwrap().1.is_empty());
    }

    #[test]
    fn dropping_an_older_stack_slice_preserves_younger_roots() {
        let heap = Heap::start(HeapConfig::default()).unwrap();
        let stack = heap.root_stack();
        let older = stack.alloc(STACK_SEGMENT_SLOTS - 1);
        older.set(0, heap.allocate_foreign(1).unwrap());
        let younger = stack.alloc(2);
        younger.set(0, heap.allocate_foreign(2).unwrap());

        drop(older);
        let newest = stack.alloc(1);
        newest.set(0, heap.allocate_foreign(3).unwrap());
        heap.collect();

        let (epoch, live) = heap.snapshot_live_foreigns(0).unwrap();
        assert_eq!(live, [2, 3].into_iter().collect());
        assert_eq!(heap.foreign_id(younger.get(0)), Some(2));

        drop(newest);
        drop(younger);
        assert_eq!(stack.top(), 0);
        heap.collect();
        assert!(heap.snapshot_live_foreigns(epoch).unwrap().1.is_empty());
    }

    #[test]
    fn oversized_stack_frames_keep_their_slots_registered_until_drop() {
        let heap = Heap::start(HeapConfig::default()).unwrap();
        let stack = heap.root_stack();
        let mut frames = [stack.alloc(1), stack.alloc(STACK_SEGMENT_SLOTS + 1)];
        frames[1].set(STACK_SEGMENT_SLOTS, heap.allocate_foreign(4).unwrap());
        let address = frames[1].as_ptr();
        frames.swap(0, 1);
        heap.collect();
        let [wide, _neighbor] = frames;
        assert_eq!(wide.as_ptr(), address);
        assert_eq!(heap.foreign_id(wide.get(STACK_SEGMENT_SLOTS)), Some(4));
        let (epoch, live) = heap.snapshot_live_foreigns(0).unwrap();
        assert!(live.contains(&4));
        drop(wide);
        heap.collect();
        assert!(heap.snapshot_live_foreigns(epoch).unwrap().1.is_empty());
    }

    #[test]
    fn collection_releases_idle_root_stack_segments() {
        let heap = Heap::start(HeapConfig::default()).unwrap();
        let stack = heap.root_stack();
        let bottom = stack.alloc(1);
        bottom.set(0, heap.string("live").unwrap());
        let deep = (0..8)
            .map(|_| stack.alloc(STACK_SEGMENT_SLOTS))
            .collect::<Vec<_>>();
        assert_eq!(stack.inner.state.borrow().segments.len(), 9);
        drop(deep);
        heap.collect();
        assert_eq!(stack.inner.state.borrow().segments.len(), 2);
        assert_eq!(heap.string_contents(bottom.get(0)).as_deref(), Some("live"));
        let reused = stack.alloc(STACK_SEGMENT_SLOTS);
        assert_eq!(reused.get(0), Word::NIL);
        drop((reused, bottom));
        heap.collect();
        assert_eq!(stack.inner.state.borrow().segments.len(), 1);
    }

    #[test]
    fn slab_tail_is_traced_at_first_reuse() {
        let heap = Heap::start(HeapConfig::default()).unwrap();
        let foreign = heap.allocate_foreign(42).unwrap();
        let fill: Vec<_> = (0..SLAB_PAGE_SLOTS / SLAB_CLASSES[0] - 1)
            .map(|_| heap.root_frame(1))
            .collect();
        let large = heap.root_frame(SLAB_CLASSES[3]);
        let tail = heap.root_frame(1);
        tail.set(0, foreign);
        {
            let pool = heap.roots.frames.slab_pool.borrow();
            assert_eq!(pool.pages.len(), 2);
            assert_eq!(
                tail.as_ptr(),
                pool.pages[0].slots[SLAB_PAGE_SLOTS - SLAB_CLASSES[0]..].as_ptr()
            );
        }
        heap.collect();
        assert!(heap.snapshot_live_foreigns(0).unwrap().1.contains(&42));
        drop((fill, large, tail));
    }
}
