use std::ffi::c_void;
use std::io::{self, BufReader, Read, Write};
use std::mem::MaybeUninit;
use std::ptr;
use std::time::Duration;

use zkcl::Runtime;

use crate::abi::{Error, boundary, require_pointer, write_owned_handle};

type ReadCallback = unsafe extern "C" fn(*mut c_void, *mut u8, usize) -> isize;
type WriteCallback = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> bool;

#[repr(C)]
#[derive(Default)]
struct HostInput {
    read: Option<ReadCallback>,
    userdata: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct HostOutput {
    write: Option<WriteCallback>,
    userdata: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct RuntimeConfig<'a> {
    heap_size_bytes: usize,
    step_budget: Option<&'a u64>,
    input: HostInput,
    output: HostOutput,
    error_output: HostOutput,
    allow_foreign_calls: bool,
}

unsafe fn enable_foreign_calls(
    builder: zkcl::RuntimeBuilder,
) -> Result<zkcl::RuntimeBuilder, Error> {
    #[cfg(feature = "ffi")]
    {
        // SAFETY: the C caller explicitly accepts the foreign call contract.
        Ok(unsafe { builder.allow_foreign_calls() })
    }
    #[cfg(not(feature = "ffi"))]
    {
        let _ = builder;
        Err(Error::invalid(
            "zkcl-capi was built without the ffi feature",
        ))
    }
}

impl RuntimeConfig<'_> {
    fn start(&self) -> Result<Runtime, Error> {
        let mut builder = Runtime::builder().step_budget(self.step_budget.copied());
        if self.heap_size_bytes != 0 {
            builder = builder.heap_size_bytes(self.heap_size_bytes);
        }
        if self.allow_foreign_calls {
            // SAFETY: caller opted into foreign calls in RuntimeConfig.
            builder = unsafe { enable_foreign_calls(builder)? };
        }
        let mut runtime = builder.start().map_err(|source| Error::RuntimeStart {
            message: source.to_string(),
            source,
        })?;
        if let Some(read) = self.input.read {
            runtime.set_standard_input(BufReader::new(CallbackReader {
                read,
                userdata: self.input.userdata,
            }));
        }
        if let Some(write) = self.output.write {
            runtime.set_standard_output(CallbackWriter {
                write,
                userdata: self.output.userdata,
            });
        }
        if let Some(write) = self.error_output.write {
            runtime.set_error_output(CallbackWriter {
                write,
                userdata: self.error_output.userdata,
            });
        }
        Ok(runtime)
    }
}

struct CallbackReader {
    read: ReadCallback,
    userdata: *mut c_void,
}

impl Read for CallbackReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        // SAFETY: startup delegates callback and userdata lifetimes to the C caller.
        let amount = unsafe { (self.read)(self.userdata, buffer.as_mut_ptr(), buffer.len()) };
        let amount = usize::try_from(amount).map_err(|_| io::Error::other("host input failed"))?;
        if amount > buffer.len() {
            return Err(io::Error::other("host input exceeded buffer capacity"));
        }
        Ok(amount)
    }
}

struct CallbackWriter {
    write: WriteCallback,
    userdata: *mut c_void,
}

impl Write for CallbackWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        // SAFETY: startup delegates callback and userdata lifetimes to the C caller.
        if !unsafe { (self.write)(self.userdata, buffer.as_ptr(), buffer.len()) } {
            return Err(io::Error::other("host output failed"));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: an empty buffer is a documented flush request.
        if !unsafe { (self.write)(self.userdata, ptr::null(), 0) } {
            return Err(io::Error::other("host output flush failed"));
        }
        Ok(())
    }
}

#[repr(C)]
struct HeapStats {
    allocation_limit_bytes: usize,
    committed_bytes: usize,
    large_object_bytes: usize,
    collections: u64,
    last_collection_ns: u64,
}

#[repr(C)]
struct GcStats {
    live_bytes: usize,
    allocated_bytes_since_collection: usize,
    last_reclaimed_bytes: usize,
    last_evacuated_bytes: usize,
    last_evacuated_objects: usize,
    block_count: usize,
    last_evacuation_blocks: usize,
    evacuation_reserve_bytes: usize,
    compaction_retries: u64,
    total_collection_ns: u64,
}

fn duration_ns(duration: Option<Duration>) -> u64 {
    duration.map_or(0, |value| {
        u64::try_from(value.as_nanos()).unwrap_or(u64::MAX)
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_runtime_new(
    allow_foreign_calls: bool,
    out: Option<&mut *mut Runtime>,
) -> *mut Error {
    zkcl_runtime_new_with_config(
        Some(&RuntimeConfig {
            allow_foreign_calls,
            ..RuntimeConfig::default()
        }),
        out,
    )
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_runtime_new_with_config(
    config: Option<&RuntimeConfig<'_>>,
    out: Option<&mut *mut Runtime>,
) -> *mut Error {
    write_owned_handle(out, || match config {
        Some(config) => config.start(),
        None => RuntimeConfig::default().start(),
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_get_heap_stats(
    rt: Option<&Runtime>,
    out: Option<&mut MaybeUninit<HeapStats>>,
) -> *mut Error {
    boundary(|| {
        let out = require_pointer(out)?;
        let stats = require_pointer(rt)?.heap_stats();
        out.write(HeapStats {
            allocation_limit_bytes: stats.allocation_limit_bytes,
            committed_bytes: stats.committed_bytes,
            large_object_bytes: stats.large_object_bytes,
            collections: stats.collections,
            last_collection_ns: duration_ns(stats.last_collection_duration),
        });
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_get_gc_stats(
    rt: Option<&Runtime>,
    out: Option<&mut MaybeUninit<GcStats>>,
) -> *mut Error {
    boundary(|| {
        let out = require_pointer(out)?;
        let stats = require_pointer(rt)?.heap_stats();
        out.write(GcStats {
            live_bytes: stats.live_bytes,
            allocated_bytes_since_collection: stats.allocated_bytes_since_collection,
            last_reclaimed_bytes: stats.last_reclaimed_bytes,
            last_evacuated_bytes: stats.last_evacuated_bytes,
            last_evacuated_objects: stats.last_evacuated_objects,
            block_count: stats.block_count,
            last_evacuation_blocks: stats.last_evacuation_blocks,
            evacuation_reserve_bytes: stats.evacuation_reserve_bytes,
            compaction_retries: stats.compaction_retries,
            total_collection_ns: duration_ns(stats.total_collection_duration),
        });
        Ok(())
    })
}
