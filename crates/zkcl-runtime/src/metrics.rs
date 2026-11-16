#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VmMetrics {
    pub dispatches: u64,
    /// Executions of a canonical instruction through the general interpreter.
    pub canonical_dispatches: u64,
    pub frames: u64,
    pub root_frames: u64,
    pub root_registrations: u64,
    pub primitive_calls: u64,
    pub slow_paths: u64,
    /// Exhausted native quanta serviced by the scheduler.
    pub native_checkpoints: u64,
    pub heap_allocs: u64,
    pub heap_bytes: u64,
    pub gc_collections: u64,
    pub gc_relocations: u64,
}

#[cfg(feature = "vm-metrics")]
mod inner {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::VmMetrics;

    static DISPATCHES: AtomicU64 = AtomicU64::new(0);
    static CANONICAL_DISPATCHES: AtomicU64 = AtomicU64::new(0);
    static FRAMES: AtomicU64 = AtomicU64::new(0);
    static ROOT_FRAMES: AtomicU64 = AtomicU64::new(0);
    static ROOT_REGISTRATIONS: AtomicU64 = AtomicU64::new(0);
    static PRIMITIVE_CALLS: AtomicU64 = AtomicU64::new(0);
    static SLOW_PATHS: AtomicU64 = AtomicU64::new(0);
    static NATIVE_CHECKPOINTS: AtomicU64 = AtomicU64::new(0);
    static HEAP_ALLOCS: AtomicU64 = AtomicU64::new(0);
    static HEAP_BYTES: AtomicU64 = AtomicU64::new(0);
    static GC_COLLECTIONS: AtomicU64 = AtomicU64::new(0);
    static GC_RELOCATIONS: AtomicU64 = AtomicU64::new(0);

    pub fn snapshot() -> VmMetrics {
        VmMetrics {
            dispatches: DISPATCHES.load(Ordering::Relaxed),
            canonical_dispatches: CANONICAL_DISPATCHES.load(Ordering::Relaxed),
            frames: FRAMES.load(Ordering::Relaxed),
            root_frames: ROOT_FRAMES.load(Ordering::Relaxed),
            root_registrations: ROOT_REGISTRATIONS.load(Ordering::Relaxed),
            primitive_calls: PRIMITIVE_CALLS.load(Ordering::Relaxed),
            slow_paths: SLOW_PATHS.load(Ordering::Relaxed),
            native_checkpoints: NATIVE_CHECKPOINTS.load(Ordering::Relaxed),
            heap_allocs: HEAP_ALLOCS.load(Ordering::Relaxed),
            heap_bytes: HEAP_BYTES.load(Ordering::Relaxed),
            gc_collections: GC_COLLECTIONS.load(Ordering::Relaxed),
            gc_relocations: GC_RELOCATIONS.load(Ordering::Relaxed),
        }
    }

    pub fn reset() {
        DISPATCHES.store(0, Ordering::Relaxed);
        CANONICAL_DISPATCHES.store(0, Ordering::Relaxed);
        FRAMES.store(0, Ordering::Relaxed);
        ROOT_FRAMES.store(0, Ordering::Relaxed);
        ROOT_REGISTRATIONS.store(0, Ordering::Relaxed);
        PRIMITIVE_CALLS.store(0, Ordering::Relaxed);
        SLOW_PATHS.store(0, Ordering::Relaxed);
        NATIVE_CHECKPOINTS.store(0, Ordering::Relaxed);
        HEAP_ALLOCS.store(0, Ordering::Relaxed);
        HEAP_BYTES.store(0, Ordering::Relaxed);
        GC_COLLECTIONS.store(0, Ordering::Relaxed);
        GC_RELOCATIONS.store(0, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_dispatch() {
        DISPATCHES.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_canonical_dispatch() {
        CANONICAL_DISPATCHES.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_frame() {
        FRAMES.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_root_frame() {
        ROOT_FRAMES.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_root_registration() {
        ROOT_REGISTRATIONS.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_primitive_call() {
        PRIMITIVE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_slow_path() {
        SLOW_PATHS.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_native_checkpoint() {
        NATIVE_CHECKPOINTS.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_heap_alloc(bytes: usize) {
        HEAP_ALLOCS.fetch_add(1, Ordering::Relaxed);
        HEAP_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_gc_collection() {
        GC_COLLECTIONS.fetch_add(1, Ordering::Relaxed);
    }

    #[inline(always)]
    pub(crate) fn record_gc_relocations(count: usize) {
        GC_RELOCATIONS.fetch_add(count as u64, Ordering::Relaxed);
    }
}

#[cfg(not(feature = "vm-metrics"))]
mod inner {
    use super::VmMetrics;

    pub fn snapshot() -> VmMetrics {
        VmMetrics::default()
    }

    pub fn reset() {}

    #[inline(always)]
    pub(crate) fn record_dispatch() {}

    #[inline(always)]
    pub(crate) fn record_canonical_dispatch() {}

    #[inline(always)]
    pub(crate) fn record_frame() {}

    #[inline(always)]
    pub(crate) fn record_root_frame() {}

    #[inline(always)]
    pub(crate) fn record_root_registration() {}

    #[inline(always)]
    pub(crate) fn record_primitive_call() {}

    #[inline(always)]
    pub(crate) fn record_slow_path() {}

    #[inline(always)]
    pub(crate) fn record_native_checkpoint() {}

    #[inline(always)]
    pub(crate) fn record_heap_alloc(_bytes: usize) {}

    #[inline(always)]
    pub(crate) fn record_gc_collection() {}

    #[inline(always)]
    pub(crate) fn record_gc_relocations(_count: usize) {}
}

#[allow(unused_imports)]
pub(crate) use inner::{
    record_canonical_dispatch, record_dispatch, record_frame, record_gc_collection,
    record_gc_relocations, record_heap_alloc, record_native_checkpoint, record_primitive_call,
    record_root_frame, record_root_registration, record_slow_path,
};
pub use inner::{reset, snapshot};
