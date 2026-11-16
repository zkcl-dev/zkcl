use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use num_traits::ToPrimitive;

use super::{concrete_path, file_designator, file_fault};
use crate::condition::{Class, Fault};
use crate::heap::{RootSlots, Word};
use crate::host::HostOperation;
use crate::pathname::{self, PathnameError};
use crate::{BlockingPool, EvalError, HostCompletion, Runtime};

pub(in crate::vm) fn read_octets(
    runtime: &Runtime,
    arguments: &RootSlots,
    at: usize,
) -> Result<HostOperation, Fault> {
    let start = file_offset(runtime, arguments.get(1), at)?;
    let length = if arguments.get(2) == Word::NIL {
        u64::MAX
    } else {
        let end = file_offset(runtime, arguments.get(2), at)?;
        end.checked_sub(start).ok_or_else(|| {
            Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "file :END precedes :START"),
            )
        })?
    };
    let original = file_designator(runtime, arguments.get(0), at)?;
    let fail = |error| file_fault(runtime, &original, at, error);
    if original.is_wild() {
        return Err(fail(EvalError::from_error(at, PathnameError::Wild)));
    }
    let translated = pathname::translate(runtime, original.clone(), at)?;
    let path = concrete_path(&translated, at).map_err(fail)?;
    // Resolve relative defaults before the operation leaves the owning thread.
    let path =
        std::path::absolute(&path).map_err(|error| fail(EvalError::file_io(at, &path, error)))?;
    let pool = file_workers().map_err(|error| fail(EvalError::file_io(at, &path, error)))?;
    let heap_limit = runtime.heap_stats().allocation_limit_bytes as u64;
    let array_limit = runtime.compilation_target().fixnum_bounds().1 as u64;
    let limit = heap_limit.min(array_limit);
    let (operation, completion) = HostOperation::new();
    pool.try_submit_with_fault(
        completion,
        move |completion| {
            read_range(&path, start, length, limit, completion)
                .map_err(|error| EvalError::file_io(at, &path, error))
        },
        move |rt, result, at| {
            let bytes = result.map_err(|error| file_fault(rt, &original, at, error))?;
            let octets = rt
                .make_octets(&bytes)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            Ok(vec![octets])
        },
    )
    .map_err(|error| EvalError::host_callback(at, Box::new(error)))?;
    Ok(operation)
}

fn file_offset(runtime: &Runtime, word: Word, at: usize) -> Result<u64, Fault> {
    if let Some(fixnum) = word.as_fixnum() {
        if let Ok(offset) = u64::try_from(fixnum) {
            return Ok(offset);
        }
    } else if let Some(offset) = runtime
        .heap
        .integer(word)
        .and_then(|number| number.to_u64())
    {
        return Ok(offset);
    }
    Err(Fault::type_error(runtime, 
        word,
        "(UNSIGNED-BYTE 64)",
        EvalError::new(at, "file offset must fit in an unsigned 64-bit integer"),
    ))
}

fn file_workers() -> io::Result<&'static BlockingPool> {
    static POOL: OnceLock<BlockingPool> = OnceLock::new();
    static INIT: Mutex<()> = Mutex::new(());
    if let Some(pool) = POOL.get() {
        return Ok(pool);
    }
    let _lock = INIT.lock().unwrap();
    if let Some(pool) = POOL.get() {
        return Ok(pool);
    }
    let workers = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(2, 8);
    let pool = BlockingPool::new(
        NonZeroUsize::new(workers).unwrap(),
        NonZeroUsize::new(256).unwrap(),
    )?;
    Ok(POOL.get_or_init(|| pool))
}

fn read_range(
    path: &Path,
    start: u64,
    length: u64,
    limit: u64,
    completion: &HostCompletion,
) -> io::Result<Vec<u8>> {
    check_cancelled(completion)?;
    let (mut file, metadata) = crate::platform::open_regular_file_nonblocking(path)?;
    if start != 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let read_limit = length.min(limit.saturating_add(1));
    let mut bytes = Vec::new();
    let expected = metadata.len().saturating_sub(start).min(read_limit);
    bytes
        .try_reserve_exact(expected as usize)
        .map_err(io::Error::other)?;
    CancellableFile { file, completion }
        .take(read_limit)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "file range exceeds the runtime's octet vector limit",
        ));
    }
    Ok(bytes)
}

struct CancellableFile<'a> {
    file: File,
    completion: &'a HostCompletion,
}

impl Read for CancellableFile<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        check_cancelled(self.completion)?;
        // Limit each call so a cancelled large read can release its descriptor
        // before waiting for another large transfer.
        let len = bytes.len().min(128 * 1024);
        self.file.read(&mut bytes[..len])
    }
}

fn check_cancelled(completion: &HostCompletion) -> io::Result<()> {
    if completion.is_cancelled() {
        // Read::read_to_end would retry Interrupted and never acknowledge.
        return Err(io::Error::other("file read cancelled"));
    }
    Ok(())
}
