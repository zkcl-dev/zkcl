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
    /// Bundled Lisp sources loaded by the one-time package bootstrap.
    pub bootstrap_source_loads: u64,
    /// Time spent reading and evaluating those sources.
    pub bootstrap_source_nanos: u64,

    // Automatic JIT admissions. The seven exits below are mutually exclusive and
    // exhaustive, so their sum is the number of admissions attempted.
    /// Admissions answered by an already-recorded compilation outcome.
    ///
    /// `CodeStore::finished` records successes and failures alike, and a success stops
    /// re-entering admission because its native entry short-circuits the call, so a hit
    /// here is in practice a function whose compilation failed and is never retried.
    pub jit_finished_hits: u64,
    /// Admissions skipped because a request for the same code was already in flight.
    pub jit_pending_hits: u64,
    /// Admissions that found no active backend or no installed code to snapshot.
    pub jit_unavailable: u64,
    /// Admissions the backend declined to queue before a snapshot was built.
    pub jit_admission_gated: u64,
    /// Admissions whose bytecode snapshot was built and then refused by the queue.
    pub jit_rejected_snapshots: u64,
    /// Admissions that queued a compilation request.
    pub jit_accepted: u64,
    /// Admissions the backend queued but whose store-side lease lost the race.
    pub jit_superseded: u64,
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
    static BOOTSTRAP_SOURCE_LOADS: AtomicU64 = AtomicU64::new(0);
    static BOOTSTRAP_SOURCE_NANOS: AtomicU64 = AtomicU64::new(0);
    static JIT_FINISHED_HITS: AtomicU64 = AtomicU64::new(0);
    static JIT_PENDING_HITS: AtomicU64 = AtomicU64::new(0);
    static JIT_UNAVAILABLE: AtomicU64 = AtomicU64::new(0);
    static JIT_ADMISSION_GATED: AtomicU64 = AtomicU64::new(0);
    static JIT_REJECTED_SNAPSHOTS: AtomicU64 = AtomicU64::new(0);
    static JIT_ACCEPTED: AtomicU64 = AtomicU64::new(0);
    static JIT_SUPERSEDED: AtomicU64 = AtomicU64::new(0);

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
            bootstrap_source_loads: BOOTSTRAP_SOURCE_LOADS.load(Ordering::Relaxed),
            bootstrap_source_nanos: BOOTSTRAP_SOURCE_NANOS.load(Ordering::Relaxed),
            jit_finished_hits: JIT_FINISHED_HITS.load(Ordering::Relaxed),
            jit_pending_hits: JIT_PENDING_HITS.load(Ordering::Relaxed),
            jit_unavailable: JIT_UNAVAILABLE.load(Ordering::Relaxed),
            jit_admission_gated: JIT_ADMISSION_GATED.load(Ordering::Relaxed),
            jit_rejected_snapshots: JIT_REJECTED_SNAPSHOTS.load(Ordering::Relaxed),
            jit_accepted: JIT_ACCEPTED.load(Ordering::Relaxed),
            jit_superseded: JIT_SUPERSEDED.load(Ordering::Relaxed),
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
        BOOTSTRAP_SOURCE_LOADS.store(0, Ordering::Relaxed);
        BOOTSTRAP_SOURCE_NANOS.store(0, Ordering::Relaxed);
        JIT_FINISHED_HITS.store(0, Ordering::Relaxed);
        JIT_PENDING_HITS.store(0, Ordering::Relaxed);
        JIT_UNAVAILABLE.store(0, Ordering::Relaxed);
        JIT_ADMISSION_GATED.store(0, Ordering::Relaxed);
        JIT_REJECTED_SNAPSHOTS.store(0, Ordering::Relaxed);
        JIT_ACCEPTED.store(0, Ordering::Relaxed);
        JIT_SUPERSEDED.store(0, Ordering::Relaxed);
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

    /// One bundled Lisp source finished loading during package bootstrap.
    #[inline(always)]
    pub(crate) fn record_bootstrap_source(elapsed: std::time::Duration) {
        BOOTSTRAP_SOURCE_LOADS.fetch_add(1, Ordering::Relaxed);
        BOOTSTRAP_SOURCE_NANOS.fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    }

    /// One automatic JIT admission exited through an already-recorded outcome.
    #[inline(always)]
    pub(crate) fn record_jit_finished_hit() {
        JIT_FINISHED_HITS.fetch_add(1, Ordering::Relaxed);
    }

    /// One automatic JIT admission exited through an in-flight request.
    #[inline(always)]
    pub(crate) fn record_jit_pending_hit() {
        JIT_PENDING_HITS.fetch_add(1, Ordering::Relaxed);
    }

    /// One automatic JIT admission found nothing to submit to.
    #[inline(always)]
    pub(crate) fn record_jit_unavailable() {
        JIT_UNAVAILABLE.fetch_add(1, Ordering::Relaxed);
    }

    /// One automatic JIT admission was refused before its bytecode was snapshotted.
    #[inline(always)]
    pub(crate) fn record_jit_admission_gated() {
        JIT_ADMISSION_GATED.fetch_add(1, Ordering::Relaxed);
    }

    /// One automatic JIT admission snapshotted bytecode the queue then refused.
    #[inline(always)]
    pub(crate) fn record_jit_rejected_snapshot() {
        JIT_REJECTED_SNAPSHOTS.fetch_add(1, Ordering::Relaxed);
    }

    /// One automatic JIT admission queued a compilation request.
    #[inline(always)]
    pub(crate) fn record_jit_accepted() {
        JIT_ACCEPTED.fetch_add(1, Ordering::Relaxed);
    }

    /// One automatic JIT admission queued a request but lost its store-side lease.
    #[inline(always)]
    pub(crate) fn record_jit_superseded() {
        JIT_SUPERSEDED.fetch_add(1, Ordering::Relaxed);
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

    #[inline(always)]
    pub(crate) fn record_jit_finished_hit() {}

    #[inline(always)]
    pub(crate) fn record_jit_pending_hit() {}

    #[inline(always)]
    pub(crate) fn record_jit_unavailable() {}

    #[inline(always)]
    pub(crate) fn record_jit_admission_gated() {}

    #[inline(always)]
    pub(crate) fn record_jit_rejected_snapshot() {}

    #[inline(always)]
    pub(crate) fn record_jit_accepted() {}

    #[inline(always)]
    pub(crate) fn record_jit_superseded() {}
}

#[cfg(feature = "vm-metrics")]
pub(crate) use inner::record_bootstrap_source;
#[allow(unused_imports)]
pub(crate) use inner::{
    record_canonical_dispatch, record_dispatch, record_frame, record_gc_collection,
    record_gc_relocations, record_heap_alloc, record_jit_accepted, record_jit_admission_gated,
    record_jit_finished_hit, record_jit_pending_hit,
    record_jit_rejected_snapshot, record_jit_superseded, record_jit_unavailable,
    record_native_checkpoint, record_primitive_call, record_root_frame, record_root_registration,
    record_slow_path,
};
pub use inner::{reset, snapshot};
