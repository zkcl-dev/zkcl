//! OS-specific executable memory allocation, W^X transitions, and instruction cache invalidation.

use std::ptr::NonNull;

use super::{ExecutableCode, ExecutableMemoryError};

/// Backing executable memory slab shared by one or more [`ExecutableCode`] slices.
pub(super) struct ExecutableSlab {
    pub(super) base: NonNull<u8>,
    pub(super) len: usize,
}

// SAFETY: Published code ranges are immutable. The cache lock serializes allocation and writes
// to unused slab ranges, and the last Arc owns unmapping.
unsafe impl Send for ExecutableSlab {}
unsafe impl Sync for ExecutableSlab {}

impl ExecutableSlab {
    pub(super) fn allocate(min_len: usize) -> Result<Self, ExecutableMemoryError> {
        os::allocate(min_len)
    }

    pub(super) fn publish(&self, code: &[u8]) -> Result<(), ExecutableMemoryError> {
        os::publish(self, code)
    }
}

impl Drop for ExecutableSlab {
    fn drop(&mut self) {
        // SAFETY: `self.base` and `self.len` came from `os::allocate`.
        unsafe { os::deallocate(self.base, self.len) };
    }
}

/// Allocates standalone or slab-suballocated [`ExecutableCode`] according to target W^X rules.
#[derive(Default)]
pub(super) struct SlabAllocator {
    inner: slab_alloc::State,
}

impl SlabAllocator {
    pub(super) fn allocate(
        &mut self,
        code: &[u8],
    ) -> Result<ExecutableCode, ExecutableMemoryError> {
        slab_alloc::allocate(&mut self.inner, code)
    }
}

#[cfg(unix)]
mod os {
    use std::ptr::NonNull;

    use super::ExecutableSlab;
    use crate::code_cache::ExecutableMemoryError;

    fn page_size() -> usize {
        // SAFETY: `sysconf(_SC_PAGESIZE)` has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 { 4096 } else { page as usize }
    }

    pub(super) fn allocate(min_len: usize) -> Result<ExecutableSlab, ExecutableMemoryError> {
        let page = page_size();
        let alloc_len = min_len.div_ceil(page) * page;
        let (prot, flags) = wx::mapping_flags();

        // SAFETY: Anonymous private mapping with no fixed address, file, or offset.
        let raw = unsafe { libc::mmap(std::ptr::null_mut(), alloc_len, prot, flags, -1, 0) };
        if raw == libc::MAP_FAILED {
            return Err(ExecutableMemoryError::Allocation(
                std::io::Error::last_os_error(),
            ));
        }
        let Some(base) = NonNull::new(raw.cast::<u8>()) else {
            // SAFETY: This address and length came from the successful mmap above.
            unsafe { libc::munmap(raw, alloc_len) };
            return Err(ExecutableMemoryError::Allocation(std::io::Error::other(
                "mmap returned null",
            )));
        };
        Ok(ExecutableSlab {
            base,
            len: alloc_len,
        })
    }

    pub(super) unsafe fn deallocate(base: NonNull<u8>, len: usize) {
        // SAFETY: Caller passes an active slab allocation from `allocate`.
        unsafe {
            libc::munmap(base.as_ptr().cast::<libc::c_void>(), len);
        }
    }

    pub(super) fn publish(slab: &ExecutableSlab, code: &[u8]) -> Result<(), ExecutableMemoryError> {
        wx::publish(slab, code)
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    pub(super) mod wx {
        use std::mem::DropGuard;

        use super::ExecutableSlab;
        use crate::code_cache::ExecutableMemoryError;

        pub(super) fn mapping_flags() -> (libc::c_int, libc::c_int) {
            (
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_JIT,
            )
        }

        pub(in crate::code_cache::platform) unsafe fn write_jit_and_flush(
            dst: *mut u8,
            code: &[u8],
        ) {
            unsafe extern "C" {
                fn sys_icache_invalidate(start: *mut libc::c_void, len: libc::size_t);
            }
            {
                // SAFETY: The caller supplies writable space in a MAP_JIT mapping.
                unsafe { libc::pthread_jit_write_protect_np(0) };
                let _write_guard = DropGuard::new((), |()| {
                    // SAFETY: Restoring execute protection before returning.
                    unsafe { libc::pthread_jit_write_protect_np(1) };
                });
                // SAFETY: The caller ensures the destination is disjoint and holds `code.len()` bytes.
                unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), dst, code.len()) };
            }
            // SAFETY: The destination range remains mapped after restoring execute protection.
            unsafe { sys_icache_invalidate(dst.cast(), code.len()) };
        }

        pub(super) fn publish(
            slab: &ExecutableSlab,
            code: &[u8],
        ) -> Result<(), ExecutableMemoryError> {
            // SAFETY: The freshly allocated MAP_JIT range is large enough for `code`.
            unsafe { write_jit_and_flush(slab.base.as_ptr(), code) };
            Ok(())
        }
    }

    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    pub(super) mod wx {
        use super::ExecutableSlab;
        use crate::code_cache::ExecutableMemoryError;

        pub(super) fn mapping_flags() -> (libc::c_int, libc::c_int) {
            (
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANON | libc::MAP_PRIVATE,
            )
        }

        pub(super) fn publish(
            slab: &ExecutableSlab,
            code: &[u8],
        ) -> Result<(), ExecutableMemoryError> {
            let base = slab.base.as_ptr();
            // SAFETY: The newly mapped code is exclusively owned and writable until mprotect.
            unsafe {
                std::ptr::copy_nonoverlapping(code.as_ptr(), base, code.len());
                if libc::mprotect(base.cast(), slab.len, libc::PROT_READ | libc::PROT_EXEC) != 0 {
                    return Err(ExecutableMemoryError::Protection(
                        std::io::Error::last_os_error(),
                    ));
                }
                flush_icache(base, code.len())
            }
        }

        unsafe fn flush_icache(base: *mut u8, len: usize) -> Result<(), ExecutableMemoryError> {
            #[cfg(all(
                any(target_arch = "arm", target_arch = "aarch64"),
                not(target_vendor = "apple")
            ))]
            {
                unsafe extern "C" {
                    fn __clear_cache(begin: *mut u8, end: *mut u8);
                }
                // SAFETY: `base..base + len` is the mapped executable slice.
                unsafe { __clear_cache(base, base.add(len)) };
            }
            #[cfg(all(target_os = "linux", target_arch = "riscv64", target_env = "gnu"))]
            {
                unsafe extern "C" {
                    fn __riscv_flush_icache(
                        begin: *mut libc::c_void,
                        end: *mut libc::c_void,
                        flags: libc::c_ulong,
                    ) -> libc::c_int;
                }
                // Zero publishes the new instructions to all threads in this process.
                // SAFETY: `base..base + len` is the mapped executable slice.
                if unsafe { __riscv_flush_icache(base.cast(), base.add(len).cast(), 0) } != 0 {
                    return Err(ExecutableMemoryError::FlushInstructionCache(
                        std::io::Error::last_os_error(),
                    ));
                }
            }
            let _ = (base, len);
            Ok(())
        }
    }
}

#[cfg(windows)]
mod os {
    use std::ptr::NonNull;

    use windows_sys::Win32::System::{
        Diagnostics::Debug::FlushInstructionCache,
        Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc,
            VirtualFree, VirtualProtect,
        },
        SystemInformation::{GetSystemInfo, SYSTEM_INFO},
        Threading::GetCurrentProcess,
    };

    use super::ExecutableSlab;
    use crate::code_cache::ExecutableMemoryError;

    pub(super) fn allocate(min_len: usize) -> Result<ExecutableSlab, ExecutableMemoryError> {
        let mut info = SYSTEM_INFO::default();
        // SAFETY: GetSystemInfo initializes the supplied system information structure.
        unsafe { GetSystemInfo(&mut info) };
        let len = min_len.next_multiple_of(info.dwPageSize as usize);
        // SAFETY: Reserve and commit a fresh private region with writable pages.
        let raw = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                len,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };
        let base = NonNull::new(raw.cast())
            .ok_or_else(|| ExecutableMemoryError::Allocation(std::io::Error::last_os_error()))?;
        Ok(ExecutableSlab { base, len })
    }

    pub(super) fn publish(slab: &ExecutableSlab, code: &[u8]) -> Result<(), ExecutableMemoryError> {
        let base = slab.base.as_ptr();
        let mut old_protection = 0;
        // SAFETY: This slab is private until publication and covers the entire code slice.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), base, code.len());
            if VirtualProtect(
                base.cast(),
                slab.len,
                PAGE_EXECUTE_READ,
                &mut old_protection,
            ) == 0
            {
                return Err(ExecutableMemoryError::Protection(
                    std::io::Error::last_os_error(),
                ));
            }
            if FlushInstructionCache(GetCurrentProcess(), base.cast(), code.len()) == 0 {
                return Err(ExecutableMemoryError::FlushInstructionCache(
                    std::io::Error::last_os_error(),
                ));
            }
        }
        Ok(())
    }

    pub(super) unsafe fn deallocate(base: NonNull<u8>, _len: usize) {
        // SAFETY: Release the whole VirtualAlloc reservation from its original base.
        unsafe { VirtualFree(base.as_ptr().cast(), 0, MEM_RELEASE) };
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod slab_alloc {
    use std::sync::Arc;

    use super::{ExecutableSlab, os};
    use crate::code_cache::{ExecutableCode, ExecutableMemoryError};

    const DEFAULT_SLAB_BYTES: usize = 256 * 1024;
    const CODE_ALIGN: usize = 16;

    #[derive(Default)]
    pub(super) struct State {
        active_slab: Option<SlabAllocationCursor>,
    }

    struct SlabAllocationCursor {
        slab: Arc<ExecutableSlab>,
        offset: usize,
    }

    pub(super) fn allocate(
        state: &mut State,
        code: &[u8],
    ) -> Result<ExecutableCode, ExecutableMemoryError> {
        if code.is_empty() {
            return Err(ExecutableMemoryError::EmptyCode);
        }
        if code.len() > DEFAULT_SLAB_BYTES / 2 {
            return ExecutableCode::new(code);
        }

        if let Some(cursor) = &mut state.active_slab {
            if Arc::strong_count(&cursor.slab) == 1 {
                cursor.offset = 0;
            }
            let aligned = cursor.offset.next_multiple_of(CODE_ALIGN);
            if let Some(end) = aligned.checked_add(code.len())
                && end <= cursor.slab.len
            {
                // SAFETY: The cursor selects unused space entirely within this slab.
                let ptr = unsafe { cursor.slab.base.add(aligned) };
                // SAFETY: The unused slab range has `code.len()` writable bytes under MAP_JIT.
                unsafe { os::wx::write_jit_and_flush(ptr.as_ptr(), code) };
                cursor.offset = end;
                return Ok(ExecutableCode {
                    ptr,
                    code_len: code.len(),
                    slab: Arc::clone(&cursor.slab),
                });
            }
        }

        let slab = Arc::new(ExecutableSlab::allocate(DEFAULT_SLAB_BYTES)?);
        // SAFETY: Fresh MAP_JIT slab holds at least `DEFAULT_SLAB_BYTES > code.len()`.
        unsafe { os::wx::write_jit_and_flush(slab.base.as_ptr(), code) };
        state.active_slab = Some(SlabAllocationCursor {
            slab: Arc::clone(&slab),
            offset: code.len(),
        });
        Ok(ExecutableCode {
            ptr: slab.base,
            code_len: code.len(),
            slab,
        })
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
mod slab_alloc {
    use crate::code_cache::{ExecutableCode, ExecutableMemoryError};

    #[derive(Default)]
    pub(super) struct State;

    pub(super) fn allocate(
        _state: &mut State,
        code: &[u8],
    ) -> Result<ExecutableCode, ExecutableMemoryError> {
        ExecutableCode::new(code)
    }
}
