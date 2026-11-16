use std::fs;

/// Look up the OS file owner name when supported by the target platform.
pub(crate) fn file_owner_name(metadata: &fs::Metadata) -> Option<String> {
    owner_imp::file_owner_name(metadata)
}

#[cfg(unix)]
mod owner_imp {
    use std::ffi::CStr;
    use std::fs;
    use std::mem::MaybeUninit;
    use std::os::unix::fs::MetadataExt;
    use std::ptr;

    pub(super) fn file_owner_name(metadata: &fs::Metadata) -> Option<String> {
        let mut storage = MaybeUninit::<libc::passwd>::uninit();
        let mut buffer = vec![0u8; 1024];
        loop {
            let mut entry = ptr::null_mut();
            // SAFETY: storage, buffer, and entry are writable for this call.
            let status = unsafe {
                libc::getpwuid_r(
                    metadata.uid(),
                    storage.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut entry,
                )
            };
            if status == libc::ERANGE {
                buffer.try_reserve_exact(buffer.len()).ok()?;
                buffer.resize(buffer.capacity(), 0);
                continue;
            }
            if status != 0 || entry.is_null() {
                return None;
            }
            // SAFETY: On success the entry is initialized, and its name is a
            // NUL-terminated string in the caller's buffer.
            let name = unsafe { (*entry).pw_name };
            return (!name.is_null())
                .then(|| unsafe { CStr::from_ptr(name).to_string_lossy().into_owned() });
        }
    }
}

#[cfg(not(unix))]
mod owner_imp {
    use std::fs;

    pub(super) fn file_owner_name(_metadata: &fs::Metadata) -> Option<String> {
        None
    }
}

/// Open a regular file for reading without blocking on special files like FIFOs.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn open_regular_file_nonblocking(
    path: &std::path::Path,
) -> std::io::Result<(fs::File, fs::Metadata)> {
    use std::fs::OpenOptions;
    use std::io;

    let mut options = OpenOptions::new();
    options.read(true);
    open_imp::prepare_regular_open(&mut options, path)?;
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ));
    }
    Ok((file, metadata))
}

#[cfg(all(not(target_family = "wasm"), unix))]
mod open_imp {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub(super) fn prepare_regular_open(options: &mut OpenOptions, _path: &Path) -> io::Result<()> {
        // Opening a FIFO must not park a worker indefinitely. The opened
        // descriptor is checked even if the pathname was changed by another process.
        options.custom_flags(libc::O_NONBLOCK);
        Ok(())
    }
}

#[cfg(all(not(target_family = "wasm"), windows))]
mod open_imp {
    use std::fs::{self, OpenOptions};
    use std::io;
    use std::path::Path;

    pub(super) fn prepare_regular_open(_options: &mut OpenOptions, path: &Path) -> io::Result<()> {
        if !fs::metadata(path)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a regular file",
            ));
        }
        Ok(())
    }
}

#[cfg(all(not(target_family = "wasm"), not(unix), not(windows)))]
mod open_imp {
    use std::fs::OpenOptions;
    use std::io;
    use std::path::Path;

    pub(super) fn prepare_regular_open(_options: &mut OpenOptions, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}
