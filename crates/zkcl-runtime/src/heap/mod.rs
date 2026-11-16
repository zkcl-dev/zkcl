use std::cell::{Cell as LocalCell, RefCell};
use std::mem::offset_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use rustc_hash::FxHashSet;

use crate::condition::Class;
use crate::{AlreadyStarted, HeapStats, OutOfMemory};

mod array;
mod class;
mod condition_restart;
mod cons;
mod foreign;
mod function;
mod generic;
mod gray_packet;
mod hash_table;
mod image;
mod immix;
mod instance;
mod mop;
mod number;
mod object;
mod random_state;
mod readtable;
mod roots;
mod safepoint;
mod simd;
mod stream;
mod string;
mod structure;
mod symbol;
mod trace;
mod treelist;
mod weak;
mod word;

use condition_restart::{LispCondition, LispMacroEnvironment, LispRestart};
use immix::CollectionMode;
use object::{HeapObject, WordSlot, trailing_base, visit_offsets};
use symbol::Symbol;
use word::{
    Address, Header, ObjectKind, SlotVisitor, ValueSlot, object_address, scan_object_slots,
};

pub(crate) use array::{ArrayElement, ArrayValue, F32SpanError, SimpleVectorError};
pub(crate) use class::{ClassFields, ClassKind};
pub(crate) use cons::{ListSpine, SpineCycle};
pub(crate) use function::{
    CLOSURE_CAPTURES_OFFSET, CLOSURE_CODE_OFFSET, FUNCTION_CONSTANTS_OFFSET, FUNCTION_INFO_OFFSET,
};
#[cfg(test)]
pub(crate) use generic::check_generic_storage;
pub(crate) use generic::{
    CachedStandardMethods, GenericCallSignature, GenericFields, MethodFields,
    StandardDispatchCacheKey,
};
#[cfg(test)]
pub(crate) use hash_table::check_hash_table_storage;
pub(crate) use hash_table::{HashTableLayout, HashTest};
pub(crate) use immix::HeapConfig;
pub(crate) use mop::SlotFields;
#[cfg(test)]
pub(crate) use number::check_number_storage;
pub(crate) use random_state::RandomState;
pub(crate) use readtable::ReadtableCase;
pub(crate) use roots::{
    ForeignEdge, ForeignQueue, HeapRootRegistry, RootFrame, RootFrameAllocator, RootSlot, RootStack,
    RootStackFrame,
};
pub use roots::{PersistentRoot, RootSlots};
pub(crate) use safepoint::{DomainSafepoint, SafepointCoordinator};
pub(crate) use stream::{StreamElement, StreamKind, StreamState};
pub(crate) use symbol::CELL_VALUE_OFFSET;
pub(crate) use treelist::TreelistKind;
pub use word::Word;

pub(crate) struct Heap {
    pub(crate) code_store: crate::code_store::CodeStore,
    roots: roots::HeapRoots,
    immix: RefCell<immix::ImmixHeap>,
    trace: RefCell<trace::HeapTrace>,
    remembered_objects: RefCell<Vec<Address>>,
    collections: LocalCell<u64>,
    minor_collections: LocalCell<u64>,
    major_collections: LocalCell<u64>,
    gc_epoch: LocalCell<usize>,
    compaction_retries: LocalCell<u64>,
    last_collection_duration: LocalCell<Option<Duration>>,
    total_collection_duration: LocalCell<Option<Duration>>,
    function_epoch: std::cell::Cell<usize>,
    namespace_epoch: std::cell::Cell<u64>,
    no_gc_depth: LocalCell<usize>,
    next_treelist_owner: LocalCell<u64>,
    next_tls_index: AtomicU32,
    tls_values: RefCell<Vec<Word>>,
    safepoints: Arc<SafepointCoordinator>,
    main_domain_safepoint: Arc<DomainSafepoint>,
    active_safepoint: RefCell<Option<Arc<DomainSafepoint>>>,
}

impl Drop for Heap {
    fn drop(&mut self) {
        self.safepoints
            .unregister_domain(&self.main_domain_safepoint);
    }
}

impl Heap {
    pub(crate) fn builder() -> HeapConfig {
        HeapConfig::default()
    }

    pub(crate) fn builder_with_heap_size(heap_size_bytes: usize) -> HeapConfig {
        HeapConfig {
            max_heap_bytes: heap_size_bytes.max(1 << 20),
            ..HeapConfig::default()
        }
    }

    pub(crate) fn start(config: HeapConfig) -> Result<Self, AlreadyStarted> {
        let safepoints = Arc::new(SafepointCoordinator::new());
        let main_domain_safepoint = safepoints.register_domain();
        Ok(Self {
            code_store: crate::code_store::CodeStore::default(),
            roots: roots::HeapRoots::new(),
            immix: RefCell::new(immix::ImmixHeap::new(config)),
            trace: RefCell::default(),
            remembered_objects: RefCell::default(),
            collections: LocalCell::default(),
            minor_collections: LocalCell::default(),
            major_collections: LocalCell::default(),
            gc_epoch: LocalCell::default(),
            compaction_retries: LocalCell::default(),
            last_collection_duration: LocalCell::default(),
            total_collection_duration: LocalCell::default(),
            function_epoch: std::cell::Cell::new(1),
            namespace_epoch: std::cell::Cell::new(0),
            no_gc_depth: LocalCell::new(0),
            next_treelist_owner: LocalCell::new(1),
            next_tls_index: AtomicU32::new(1),
            tls_values: RefCell::new(vec![Word::NO_TLS_VALUE]),
            safepoints,
            active_safepoint: RefCell::new(Some(Arc::clone(&main_domain_safepoint))),
            main_domain_safepoint,
        })
    }

    pub(crate) fn safepoint_coordinator(&self) -> &Arc<SafepointCoordinator> {
        &self.safepoints
    }

    pub(crate) fn detach_main_safepoint(&self) {
        self.safepoints
            .unregister_domain(&self.main_domain_safepoint);
        *self.active_safepoint.borrow_mut() = None;
    }

    pub(crate) fn replace_active_safepoint(
        &self,
        next: Option<Arc<DomainSafepoint>>,
    ) -> Option<Arc<DomainSafepoint>> {
        std::mem::replace(&mut *self.active_safepoint.borrow_mut(), next)
    }

    #[inline(always)]
    pub(crate) fn poll_safepoint(&self) {
        if !self.safepoints.is_gc_requested() {
            return;
        }
        self.park_active_safepoint();
    }

    #[cold]
    fn park_active_safepoint(&self) {
        let domain = self.active_safepoint.borrow().clone();
        if let Some(domain) = domain {
            domain.park_for_safepoint();
        }
    }

    pub(crate) fn swap_tls_values(&self, other: &mut Vec<Word>) {
        std::mem::swap(&mut *self.tls_values.borrow_mut(), other);
    }

    pub(crate) fn max_heap_bytes(&self) -> usize {
        self.immix.borrow().max_heap_bytes()
    }

    pub(crate) fn stats(&self) -> HeapStats {
        let immix = self.immix.borrow();
        let gc = immix.gc_snapshot();
        HeapStats {
            allocation_limit_bytes: immix.max_heap_bytes(),
            committed_bytes: immix.committed_bytes(),
            large_object_bytes: immix.large_object_bytes(),
            collections: self.collections.get(),
            minor_collections: self.minor_collections.get(),
            major_collections: self.major_collections.get(),
            last_collection_duration: self.last_collection_duration.get(),
            total_collection_duration: self.total_collection_duration.get(),
            live_bytes: gc.live_bytes,
            allocated_bytes_since_collection: gc.allocated_bytes_since_collection,
            last_reclaimed_bytes: gc.last_reclaimed_bytes,
            last_evacuated_bytes: gc.last_evacuated_bytes,
            last_evacuated_objects: gc.last_evacuated_objects,
            block_count: gc.block_count,
            last_evacuation_blocks: gc.last_evacuation_blocks,
            evacuation_reserve_bytes: immix.evacuation_reserve_bytes(),
            compaction_retries: self.compaction_retries.get(),
        }
    }

    pub(crate) fn foreign_epoch(&self) -> u64 {
        self.trace.borrow().foreign_epoch
    }

    pub(crate) fn collections(&self) -> u64 {
        self.collections.get()
    }

    pub(crate) fn allocation_counter(&self) -> std::rc::Rc<std::cell::Cell<u64>> {
        self.immix.borrow().allocation_counter()
    }

    #[cfg(test)]
    pub(crate) fn relocations(&self) -> usize {
        self.immix.borrow().relocations
    }

    pub(crate) fn with_live_foreigns<T>(
        &self,
        last_epoch: u64,
        observe: impl FnOnce(u64, &FxHashSet<u64>) -> T,
    ) -> Option<T> {
        let trace = self.trace.borrow();
        (trace.foreign_epoch > last_epoch).then(|| observe(trace.foreign_epoch, &trace.foreign_ids))
    }

    #[cfg(test)]
    pub(crate) fn snapshot_live_foreigns(&self, last_epoch: u64) -> Option<(u64, FxHashSet<u64>)> {
        self.with_live_foreigns(last_epoch, |epoch, ids| (epoch, ids.clone()))
    }

    fn allocate(&self, size: usize) -> Result<Address, OutOfMemory> {
        self.assert_may_collect();
        crate::metrics::record_heap_alloc(size);
        {
            let mut immix = self.immix.borrow_mut();
            if !immix.should_collect()
                && let Some(allocated) = immix.try_allocate(size)?
            {
                return Ok(allocated);
            }
        }
        self.allocate_after_collection(size)
    }

    #[cold]
    fn allocate_after_collection(&self, size: usize) -> Result<Address, OutOfMemory> {
        let initial = if self.immix.borrow().should_major_collect() {
            CollectionMode::Major
        } else {
            CollectionMode::Minor
        };
        self.collect_with_mode(initial);
        if let Some(allocated) = self.immix.borrow_mut().try_allocate(size)? {
            return Ok(allocated);
        }
        if initial == CollectionMode::Minor {
            self.collect_with_mode(CollectionMode::Major);
            if let Some(allocated) = self.immix.borrow_mut().try_allocate(size)? {
                return Ok(allocated);
            }
        }
        if !self.immix.borrow().can_compact_for(size) {
            return Err(OutOfMemory);
        }
        self.collect_with_mode(CollectionMode::Compact);
        self.immix
            .borrow_mut()
            .try_allocate(size)?
            .ok_or(OutOfMemory)
    }

    pub(crate) fn collect(&self) {
        self.collect_with_mode(CollectionMode::Major);
    }

    #[cfg(test)]
    pub(crate) fn collect_minor(&self) {
        self.collect_with_mode(CollectionMode::Minor);
    }

    fn collect_with_mode(&self, mode: CollectionMode) {
        self.assert_may_collect();
        let timer = crate::platform::Timer::start();
        crate::metrics::record_gc_collection();
        // The trace runs synchronously without invoking host or Lisp code. The
        // owning thread therefore keeps every registered slot live until it ends.
        let requester = self.active_safepoint.borrow().clone();
        let live_code = self.safepoints.stop_the_world(requester.as_deref(), || {
            let mut remembered = self.remembered_objects.borrow_mut();
            let mut tls_values = self.tls_values.borrow_mut();
            self.immix.borrow_mut().collect(mode, |collection| {
                self.trace
                    .borrow_mut()
                    .run(collection, &self.roots, &mut tls_values, &mut remembered)
            })
        });
        crate::metrics::record_gc_relocations(
            self.immix.borrow().gc_snapshot().last_evacuated_objects,
        );
        self.gc_epoch.update(|epoch| epoch.wrapping_add(1));
        if let Some(live_code) = live_code {
            self.code_store.sweep(&live_code);
        }
        if let Some(elapsed) = timer.elapsed() {
            self.last_collection_duration.set(Some(elapsed));
            self.total_collection_duration
                .update(|total| Some(total.unwrap_or_default().saturating_add(elapsed)));
        }
        match mode {
            CollectionMode::Minor => {
                self.minor_collections
                    .update(|count| count.saturating_add(1));
            }
            CollectionMode::Major => {
                self.major_collections
                    .update(|count| count.saturating_add(1));
            }
            CollectionMode::Compact => {
                self.major_collections
                    .update(|count| count.saturating_add(1));
                self.compaction_retries
                    .update(|count| count.saturating_add(1));
            }
        }
        self.collections.update(|count| count.saturating_add(1));
    }

    /// Changes before the mutator resumes from a collection that may forward roots.
    #[inline]
    pub(crate) fn gc_epoch(&self) -> usize {
        self.gc_epoch.get()
    }
}
