//! Runtime module for `zkcl build --wasm-runtime`.

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

use std::cell::RefCell;
use std::io::{self, Write};
use std::mem::DropGuard;
use std::sync::Once;
use std::time::Duration;

use zkcl_runtime::{CancellationHandle, EvalError, HostServices, Runtime};
use zkcl_wasm::abi::{CallFunction, CallLeaf};

unsafe extern "C" {
    fn __wasm_call_ctors();
}

#[link(wasm_import_module = "zkcl.host")]
unsafe extern "C" {
    fn write(channel: i32, offset: *const u8, len: i32) -> i32;
    fn flush(channel: i32) -> i32;
    fn arg_count() -> i32;
    fn arg_len(index: i32) -> i32;
    fn arg_read(index: i32, offset: *mut u8, capacity: i32) -> i32;
    fn wall_time_secs() -> i64;
    fn monotonic_nanos() -> i64;
    fn fill_entropy(offset: *mut u8, len: i32) -> i32;
    fn sleep_ns(nanos: i64) -> i32;
}

struct WasmHost;

impl WasmHost {
    fn wait(duration: Duration) -> io::Result<()> {
        if duration.is_zero() {
            return Ok(());
        }
        let nanos = i64::try_from(duration.as_nanos()).map_err(io::Error::other)?;
        // SAFETY: this import has no memory arguments.
        match unsafe { sleep_ns(nanos) } {
            0 => Ok(()),
            _ => Err(io::Error::other("Wasm host could not schedule SLEEP")),
        }
    }
}

impl HostServices for WasmHost {
    fn wall_time_secs(&self) -> io::Result<i64> {
        // SAFETY: this import has no memory arguments.
        Ok(unsafe { wall_time_secs() })
    }

    fn monotonic_nanos(&self) -> io::Result<u64> {
        // SAFETY: this import has no memory arguments.
        Ok(unsafe { monotonic_nanos() } as u64)
    }

    fn fill_entropy(&self, bytes: &mut [u8]) -> io::Result<()> {
        let len = i32::try_from(bytes.len()).map_err(io::Error::other)?;
        // SAFETY: the import may write within this buffer for this call.
        match unsafe { fill_entropy(bytes.as_mut_ptr(), len) } {
            0 => Ok(()),
            _ => Err(io::Error::other("host entropy request failed")),
        }
    }

    fn sleep(&self, duration: Duration) -> io::Result<()> {
        if duration.is_zero() {
            return Ok(());
        }
        // A synchronous frame cannot hand off. Recompute the remaining time
        // against the original deadline after every successful host wake.
        let started = self.monotonic_nanos()?;
        let mut remaining = duration;
        while !remaining.is_zero() {
            Self::wait(remaining)?;
            let elapsed = self.monotonic_nanos()?.saturating_sub(started);
            remaining = duration.saturating_sub(Duration::from_nanos(elapsed));
        }
        Ok(())
    }

    fn park(&self, duration: Duration) -> io::Result<()> {
        Self::wait(duration)
    }
}

struct Output(i32);

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let len = i32::try_from(bytes.len().min(i32::MAX as usize)).unwrap();
        // SAFETY: the import may read within this buffer for this call.
        let written = unsafe { write(self.0, bytes.as_ptr(), len) };
        if !(0..=len).contains(&written) {
            return Err(io::Error::other("host output failed"));
        }
        Ok(written as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        match unsafe { flush(self.0) } {
            0 => Ok(()),
            _ => Err(io::Error::other("host output flush failed")),
        }
    }
}

thread_local! {
    static LOAD_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static ACTIVE_CANCELLATION: RefCell<Option<CancellationHandle>> = const { RefCell::new(None) };
}

#[unsafe(no_mangle)]
extern "C" fn cancel() -> i32 {
    let Some(active) = ACTIVE_CANCELLATION.with_borrow(Clone::clone) else {
        return 0;
    };
    if !active.is_cancelled() {
        active.cancel_with_cleanup();
    }
    1
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_reserve_fasl(len: i32) -> *mut u8 {
    let Ok(len) = usize::try_from(len) else {
        return std::ptr::null_mut();
    };
    // Taking responsibility for the constructors keeps wasm-ld from wrapping
    // the exports. The linker patches the same leaf bridge the guest calls.
    static INITIALIZE: Once = Once::new();
    INITIALIZE.call_once(|| {
        // SAFETY: wasm-ld defines this function for the current module.
        unsafe { __wasm_call_ctors() };
    });
    LOAD_BUFFER.with_borrow_mut(|bytes| {
        bytes.clear();
        if bytes.try_reserve_exact(len).is_err() {
            return std::ptr::null_mut();
        }
        bytes.resize(len, 0);
        bytes.as_mut_ptr()
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_load_fasl(fasl_len: usize) -> i32 {
    let bytes = LOAD_BUFFER.take();
    match run(&bytes, fasl_len) {
        Ok(status) => i32::from(status),
        Err(error) => {
            let mut output = Output(2);
            let _ = writeln!(output, "zkcl: {error}");
            if let Some(error) = error.downcast_ref::<EvalError>() {
                for frame in error.stack_frames() {
                    let _ = writeln!(output, "  in {frame}");
                }
            }
            1
        }
    }
}

/// Separate export makes stale runtime templates fail at project link time.
#[unsafe(no_mangle)]
extern "C" fn zkcl_load_project(_reserved: usize) -> i32 {
    zkcl_load_fasl(0)
}

#[unsafe(no_mangle)]
#[inline(never)]
extern "C" fn zkcl_call_leaf_with_ssa(
    _slot: u32,
    _first: usize,
    _second: usize,
    _third: usize,
    _fourth: usize,
) -> usize {
    // The application linker replaces this body with the leaf table dispatch.
    core::arch::wasm32::unreachable()
}

#[unsafe(no_mangle)]
#[inline(never)]
extern "C" fn zkcl_call_function(
    _slot: u32,
    _context: *mut u8,
    _target: usize,
    _arguments: *const usize,
    _argc: usize,
    _result: *mut u8,
    _scratch: *mut usize,
) {
    core::arch::wasm32::unreachable()
}

fn run(bytes: &[u8], fasl_len: usize) -> Result<u8, Box<dyn std::error::Error>> {
    let mut runtime = Runtime::builder().start()?;
    let cancellation = runtime.cancellation_handle();
    runtime.set_host_services(WasmHost);
    runtime.set_standard_output(Output(1));
    runtime.set_error_output(Output(2));
    runtime.set_command_line_arguments(arguments()?);
    ACTIVE_CANCELLATION.with_borrow_mut(|active| {
        if active.is_some() {
            return Err("Wasm application is already running");
        }
        *active = Some(cancellation);
        Ok(())
    })?;
    let _clear = DropGuard::new((), |()| drop(ACTIVE_CANCELLATION.take()));
    let call_leaf = std::hint::black_box(zkcl_call_leaf_with_ssa as CallLeaf);
    let call_function = std::hint::black_box(zkcl_call_function as CallFunction);
    if fasl_len == 0 {
        use zkcl_wasm::abi::decode_project;
        let project = decode_project(bytes)?;
        let mut leaf_base = 0u32;
        let mut function_base = 0u32;
        for unit in project.units {
            // SAFETY: the project linker concatenates each unit's entries in
            // registration order in the two shared function tables.
            (leaf_base, function_base) = unsafe {
                runtime.load_linked_fasl_at(
                    &unit.fasl,
                    &unit.registrations,
                    call_leaf,
                    call_function,
                    leaf_base,
                    function_base,
                )
            }?;
        }
        let (package, name) = project
            .entry
            .split_once(':')
            .ok_or("expected a package-qualified entry")?;
        let name = name.strip_prefix(':').unwrap_or(name);
        let entry = runtime
            .lookup_symbol(package, name)?
            .ok_or_else(|| format!("cannot find {}", project.entry))?;
        runtime.call(&entry, &[])?;
    } else {
        let (fasl, registrations) = bytes
            .split_at_checked(fasl_len)
            .ok_or("invalid FASL length")?;
        unsafe { runtime.load_linked_fasl(fasl, registrations, call_leaf, call_function) }?;
    }
    Ok(runtime.requested_exit_code())
}

fn arguments() -> io::Result<Vec<String>> {
    // SAFETY: these imports do not access memory.
    let count = unsafe { arg_count() };
    if count < 0 {
        return Err(io::Error::other("host argument count failed"));
    }
    (0..count)
        .map(|index| {
            let len = unsafe { arg_len(index) };
            if len < 0 {
                return Err(io::Error::other("host argument length failed"));
            }
            let mut bytes = vec![0; len as usize];
            // SAFETY: the import may write within this buffer for this call.
            if unsafe { arg_read(index, bytes.as_mut_ptr(), len) } != len {
                return Err(io::Error::other("host argument read failed"));
            }
            String::from_utf8(bytes).map_err(io::Error::other)
        })
        .collect()
}
