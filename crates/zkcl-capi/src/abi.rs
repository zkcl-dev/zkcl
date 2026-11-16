use std::panic::{AssertUnwindSafe, catch_unwind};
use std::{ptr, slice};

use zkcl::{EvalError, EvalErrorKind, OutOfMemory};

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Utf8View {
    data: *const u8,
    len: usize,
}

impl Utf8View {
    pub(super) fn from_str(text: &str) -> Self {
        Self {
            data: text.as_ptr(),
            len: text.len(),
        }
    }

    // The caller supplies readable bytes for the duration of the operation.
    pub(super) unsafe fn read_utf8(&self) -> Result<&str, Error> {
        let bytes = unsafe { input_slice(self.data, self.len)? };
        Ok(std::str::from_utf8(bytes)?)
    }
}

pub(super) unsafe fn input_slice<'a, T>(data: *const T, len: usize) -> Result<&'a [T], Error> {
    if len == 0 {
        return Ok(&[]);
    }
    if data.is_null() || len > (isize::MAX as usize) / size_of::<T>() {
        return Err(Error::invalid("invalid pointer or length"));
    }
    Ok(unsafe { slice::from_raw_parts(data, len) })
}

#[repr(C)]
pub(super) struct SourceByteRange {
    pub(super) start: usize,
    pub(super) end: usize,
}

#[derive(Debug, thiserror::Error)]
#[error("{}", self.message())]
pub(super) enum Error {
    Lisp(#[from] EvalError),
    Invalid(String),
    Utf8(#[from] std::str::Utf8Error),
    Host(String),
    #[cfg(feature = "delivery")]
    Project {
        message: String,
        details: String,
    },
    RuntimeStart {
        #[source]
        source: zkcl::AlreadyStarted,
        message: String,
    },
    Memory(#[from] OutOfMemory),
    Panic,
}

impl Error {
    pub(super) fn invalid(message: &str) -> Self {
        Self::Invalid(message.to_owned())
    }

    fn message(&self) -> &str {
        match self {
            Self::Lisp(e) => e.message(),
            Self::Invalid(s) | Self::Host(s) => s,
            #[cfg(feature = "delivery")]
            Self::Project { message, .. } => message,
            Self::Utf8(_) => "expected UTF-8 text",
            Self::RuntimeStart { message, .. } => message,
            Self::Memory(_) => "the Lisp heap cannot satisfy the allocation",
            Self::Panic => "panic in the zkcl runtime",
        }
    }

    pub(super) fn into_host_error(self) -> Box<dyn std::error::Error + Send + Sync> {
        match self {
            Self::Lisp(e) => Box::new(e),
            Self::Memory(e) => Box::new(e),
            other => Box::new(other),
        }
    }
}

pub(super) fn boundary(f: impl FnOnce() -> Result<(), Error>) -> *mut Error {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => ptr::null_mut(),
        Ok(Err(error)) => Box::into_raw(Box::new(error)),
        Err(_) => Box::into_raw(Box::new(Error::Panic)),
    }
}

pub(super) fn require_pointer<T>(value: Option<T>) -> Result<T, Error> {
    value.ok_or_else(|| Error::invalid("missing required pointer"))
}

/// The caller supplies a live, exclusively accessible handle for the borrow.
pub(super) unsafe fn require_mut_pointer<'a, T>(value: *mut T) -> Result<&'a mut T, Error> {
    unsafe { value.as_mut() }.ok_or_else(|| Error::invalid("missing required pointer"))
}

pub(super) fn write_owned_handle<T>(
    out: Option<&mut *mut T>,
    make: impl FnOnce() -> Result<T, Error>,
) -> *mut Error {
    boundary(|| {
        let out = require_pointer(out)?;
        *out = ptr::null_mut();
        *out = Box::into_raw(Box::new(make()?));
        Ok(())
    })
}

pub(super) unsafe fn free_owned_handle<T>(value: *mut T) {
    if !value.is_null() {
        // SAFETY: Caller passes either null or a live `Box::into_raw` handle.
        drop(unsafe { Box::from_raw(value) });
    }
}

/// Convert a C-callback error pointer into a host `EvalError`, taking ownership of `error`.
///
/// # Safety
///
/// When `error` is non-null, it must point to an error handle allocated by `Box::into_raw`
/// that has not already been freed.
pub(super) unsafe fn check_foreign_error(
    error: *mut Error,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if error.is_null() {
        Ok(())
    } else {
        // SAFETY: Caller transfers ownership of the heap-allocated `Error`.
        Err(unsafe { Box::from_raw(error) }.into_host_error())
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_kind(error: Option<&Error>) -> u32 {
    match error {
        None => 0,
        Some(Error::Invalid(_) | Error::Utf8(_)) => 1,
        Some(Error::Lisp(e)) => match e.kind() {
            EvalErrorKind::Read => 2,
            EvalErrorKind::Compile => 3,
            EvalErrorKind::MacroExpansion => 10,
            EvalErrorKind::LispCondition(_) => 4,
            EvalErrorKind::HostCallback => 5,
            EvalErrorKind::OutOfMemory => 6,
            EvalErrorKind::Cancelled => 7,
            _ => 8,
        },
        Some(Error::Host(_) | Error::RuntimeStart { .. }) => 5,
        Some(Error::Memory(_)) => 6,
        Some(Error::Panic) => 9,
        #[cfg(feature = "delivery")]
        Some(Error::Project { .. }) => 11,
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_message(error: Option<&Error>) -> Utf8View {
    Utf8View::from_str(error.map_or("", Error::message))
}

/// Additional structured project context; borrowed until error release.
#[cfg(feature = "delivery")]
#[unsafe(no_mangle)]
extern "C" fn zkcl_error_details(error: Option<&Error>) -> Utf8View {
    Utf8View::from_str(match error {
        Some(Error::Project { details, .. }) => details,
        _ => "",
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_range(error: Option<&Error>) -> SourceByteRange {
    let range = match error {
        Some(Error::Lisp(e)) => e.byte_range(),
        _ => 0..0,
    };
    SourceByteRange {
        start: range.start,
        end: range.end,
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_execution_stop(error: Option<&Error>) -> u32 {
    match error {
        Some(Error::Lisp(error)) => match error.execution_stop() {
            Some(zkcl::ExecutionStop::Cancelled) => 1,
            Some(zkcl::ExecutionStop::Aborted) => 2,
            Some(zkcl::ExecutionStop::BudgetExhausted) => 3,
            _ => 0,
        },
        _ => 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_condition_type(error: Option<&Error>) -> Utf8View {
    Utf8View::from_str(match error {
        Some(Error::Lisp(error)) => match error.kind() {
            EvalErrorKind::LispCondition(class) => class.name(),
            _ => "",
        },
        _ => "",
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_source(error: Option<&Error>) -> *const zkcl::SourceDocument {
    match error {
        Some(Error::Lisp(error)) => error.source_document().map_or(ptr::null(), ptr::from_ref),
        _ => ptr::null(),
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_stack_len(error: Option<&Error>) -> usize {
    match error {
        Some(Error::Lisp(error)) => error.stack_frames().len(),
        _ => 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_stack_get(
    error: Option<&Error>,
    index: usize,
) -> *const zkcl::EvalStackFrame {
    match error {
        Some(Error::Lisp(error)) => error
            .stack_frames()
            .get(index)
            .map_or(ptr::null(), ptr::from_ref),
        _ => ptr::null(),
    }
}

#[repr(C)]
pub(super) struct StackFrameView {
    function: Utf8View,
    source: *const zkcl::SourceDocument,
    range: SourceByteRange,
    bytecode_pc: usize,
    has_range: bool,
    has_bytecode_pc: bool,
    native: bool,
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_error_frame_view(
    frame: Option<&zkcl::EvalStackFrame>,
    out: Option<&mut std::mem::MaybeUninit<StackFrameView>>,
) -> *mut Error {
    boundary(|| {
        let frame = require_pointer(frame)?;
        let range = frame.source_range.clone().unwrap_or(0..0);
        require_pointer(out)?.write(StackFrameView {
            function: Utf8View::from_str(frame.function.as_deref().unwrap_or("")),
            source: frame.source.as_ref().map_or(ptr::null(), ptr::from_ref),
            range: SourceByteRange {
                start: range.start,
                end: range.end,
            },
            bytecode_pc: frame.bytecode_pc.unwrap_or(0),
            has_range: frame.source_range.is_some(),
            has_bytecode_pc: frame.bytecode_pc.is_some(),
            native: frame.native,
        });
        Ok(())
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_error_new(message: Utf8View) -> *mut Error {
    boundary(|| Err(Error::Host(unsafe { message.read_utf8()? }.to_owned())))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_error_free(error: *mut Error) {
    unsafe { free_owned_handle(error) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_retains_decoder_source_and_c_error_classification() {
        let bytes = [b'a', 0xff];
        let input = Utf8View {
            data: bytes.as_ptr(),
            len: bytes.len(),
        };
        let error = unsafe { input.read_utf8() }.unwrap_err();
        assert_eq!(zkcl_error_kind(Some(&error)), 1);
        assert_eq!(error.message(), "expected UTF-8 text");
        let source = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<std::str::Utf8Error>()
            .unwrap();
        assert_eq!(source.valid_up_to(), 1);
        assert_eq!(source.error_len(), Some(1));
    }
}
