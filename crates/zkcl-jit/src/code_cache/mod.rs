mod platform;

use std::ptr::NonNull;
use std::sync::{Arc, Mutex, Weak};

use rustc_hash::FxHashMap;
use thiserror::Error;
use zkcl_runtime::native::JitCEntryFn;

use self::platform::{ExecutableSlab, SlabAllocator};

/// Errors raised by JIT executable code allocation.
#[derive(Debug, Error)]
pub enum ExecutableMemoryError {
    #[error("cannot map empty JIT code")]
    EmptyCode,
    #[error("executable memory allocation failed: {0}")]
    Allocation(#[source] std::io::Error),
    #[error("executable memory protection update failed: {0}")]
    Protection(#[source] std::io::Error),
    #[error("could not synchronize the instruction cache: {0}")]
    FlushInstructionCache(#[source] std::io::Error),
}

/// Machine code in a W^X-protected mapping kept alive by its backing slab.
pub struct ExecutableCode {
    ptr: NonNull<u8>,
    code_len: usize,
    slab: Arc<ExecutableSlab>,
}

// SAFETY: Executable code bytes are immutable once initialized; `ExecutableSlab` is thread-safe.
unsafe impl Send for ExecutableCode {}
unsafe impl Sync for ExecutableCode {}

impl ExecutableCode {
    /// Allocate standalone executable memory, copy `code` into it, enforce W^X, and flush icache.
    pub fn new(code: &[u8]) -> Result<Self, ExecutableMemoryError> {
        if code.is_empty() {
            return Err(ExecutableMemoryError::EmptyCode);
        }
        let slab = ExecutableSlab::allocate(code.len())?;
        slab.publish(code)?;
        Ok(Self {
            ptr: slab.base,
            code_len: code.len(),
            slab: Arc::new(slab),
        })
    }

    /// Start address of the mapped machine code.
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Byte length of the copied machine code.
    #[inline]
    pub fn len(&self) -> usize {
        self.code_len
    }

    /// Immutable bytes in the executable mapping, excluding slab padding.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the owning slab keeps exactly this published range live.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.code_len) }
    }

    /// Returns `true` if `len() == 0`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.code_len == 0
    }

    /// Mapped capacity of the backing slab in bytes.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.slab.len
    }

    /// Cast the executable code entry point directly to the out-pointer [`JitCEntryFn`] ABI.
    ///
    /// # Safety
    /// The caller must ensure the executable code contains valid machine code matching [`JitCEntryFn`].
    #[inline]
    pub unsafe fn as_c_entry(&self) -> JitCEntryFn {
        // SAFETY: Caller guarantees the executable code holds valid executable `JitCEntryFn` code.
        unsafe { std::mem::transmute::<*const u8, JitCEntryFn>(self.ptr.as_ptr()) }
    }
}

/// Owns a mapped function revision and its callable C entry.
#[derive(Clone)]
pub struct MappedJitFunction {
    pub code_id: usize,
    pub revision: u64,
    pub c_entry: JitCEntryFn,
    pub executable_code: Arc<ExecutableCode>,
}

#[derive(Default)]
struct CacheStorage {
    revisions_by_code_id: FxHashMap<usize, Vec<CachedRevision>>,
    allocator: SlabAllocator,
}

struct CachedRevision {
    revision: u64,
    c_entry: JitCEntryFn,
    executable_code: CachedExecutableCode,
}

enum CachedExecutableCode {
    Standalone(Arc<ExecutableCode>),
    Runtime(Weak<ExecutableCode>),
}

/// Indexes mapped JIT revisions by code ID. Standalone insertions are owned by
/// the cache, and runtime-owned revisions are held weakly.
#[derive(Default)]
pub struct CodeCache {
    inner: Mutex<CacheStorage>,
}

impl CodeCache {
    /// Create an empty code cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Map `code` into an executable slab allocation, bind its [`JitCEntryFn`] entry, and retain it.
    pub fn insert(
        &self,
        code_id: usize,
        revision: u64,
        code: &[u8],
    ) -> Result<MappedJitFunction, ExecutableMemoryError> {
        let mut guard = self.inner.lock().unwrap();
        let cached = Self::allocate_in(&mut guard, code_id, revision, code, None)?;
        Self::track(
            &mut guard,
            &cached,
            CachedExecutableCode::Standalone(Arc::clone(&cached.executable_code)),
        );
        Ok(cached)
    }

    pub(crate) fn allocate(
        &self,
        code_id: usize,
        revision: u64,
        code: &[u8],
        c_entry: Option<JitCEntryFn>,
    ) -> Result<MappedJitFunction, ExecutableMemoryError> {
        Self::allocate_in(
            &mut self.inner.lock().unwrap(),
            code_id,
            revision,
            code,
            c_entry,
        )
    }

    pub(crate) fn track_runtime(&self, code: &MappedJitFunction) {
        Self::track(
            &mut self.inner.lock().unwrap(),
            code,
            CachedExecutableCode::Runtime(Arc::downgrade(&code.executable_code)),
        );
    }

    fn allocate_in(
        cache: &mut CacheStorage,
        code_id: usize,
        revision: u64,
        code: &[u8],
        c_entry: Option<JitCEntryFn>,
    ) -> Result<MappedJitFunction, ExecutableMemoryError> {
        let executable_code = Arc::new(cache.allocator.allocate(code)?);
        // The direct backend supplies the runtime dispatcher when the mapping contains
        // only a guarded register entry. Other images begin with a C ABI entry.
        let c_entry = c_entry.unwrap_or_else(|| unsafe { executable_code.as_c_entry() });
        Ok(MappedJitFunction {
            code_id,
            revision,
            c_entry,
            executable_code,
        })
    }

    fn track(
        cache: &mut CacheStorage,
        code: &MappedJitFunction,
        executable_code: CachedExecutableCode,
    ) {
        cache
            .revisions_by_code_id
            .entry(code.code_id)
            .or_default()
            .push(CachedRevision {
                revision: code.revision,
                c_entry: code.c_entry,
                executable_code,
            });
    }

    pub(crate) fn remove_runtime(&self, code: &MappedJitFunction) {
        let mut guard = self.inner.lock().unwrap();
        let revisions = guard
            .revisions_by_code_id
            .get_mut(&code.code_id)
            .expect("cached revision list");
        let executable_code = Arc::as_ptr(&code.executable_code);
        revisions.retain(|entry| {
            !matches!(
                &entry.executable_code,
                CachedExecutableCode::Runtime(cached) if cached.as_ptr() == executable_code
            )
        });
        if revisions.is_empty() {
            guard.revisions_by_code_id.remove(&code.code_id);
        }
    }

    /// Look up the active compiled entry for `code_id`.
    pub fn get(&self, code_id: usize) -> Option<MappedJitFunction> {
        let guard = self.inner.lock().unwrap();
        let entry = guard.revisions_by_code_id.get(&code_id)?.last()?;
        Self::mapped(code_id, entry)
    }

    pub(crate) fn get_revision(&self, code_id: usize, revision: u64) -> Option<MappedJitFunction> {
        let guard = self.inner.lock().unwrap();
        let entry = guard
            .revisions_by_code_id
            .get(&code_id)?
            .iter()
            .find(|entry| entry.revision == revision)?;
        Self::mapped(code_id, entry)
    }

    fn mapped(code_id: usize, entry: &CachedRevision) -> Option<MappedJitFunction> {
        let executable_code = match &entry.executable_code {
            CachedExecutableCode::Standalone(code) => Arc::clone(code),
            CachedExecutableCode::Runtime(code) => code.upgrade()?,
        };
        Some(MappedJitFunction {
            code_id,
            revision: entry.revision,
            c_entry: entry.c_entry,
            executable_code,
        })
    }

    /// Number of active compiled functions in the cache.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().revisions_by_code_id.len()
    }

    /// Returns `true` when no active compiled functions are cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
