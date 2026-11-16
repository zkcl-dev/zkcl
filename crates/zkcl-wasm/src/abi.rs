//! Host imports and registration data for linked Lisp applications.

pub use postcard::Error as RegistrationError;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zkcl_bytecode::native_abi::LeafGuard;

pub const HOST_MODULE: &str = "zkcl.host";
pub const RESERVE_FASL: &str = "zkcl_reserve_fasl";
pub const LOAD_FASL: &str = "zkcl_load_fasl";
pub const LOAD_PROJECT: &str = "zkcl_load_project";
pub const CALL_LEAF: &str = "zkcl_call_leaf_with_ssa";
pub const CALL_FUNCTION: &str = "zkcl_call_function";
pub const CANCEL_RUN: &str = "cancel";

pub type CallLeaf = unsafe extern "C" fn(u32, usize, usize, usize, usize) -> usize;
pub type CallFunction = unsafe extern "C" fn(
    slot: u32,
    context: *mut u8,
    target: usize,
    arguments: *const usize,
    argc: usize,
    result: *mut u8,
    scratch: *mut usize,
);

#[derive(Clone, Copy)]
pub enum HostFunction {
    Write,
    Flush,
    ArgRead,
    ArgLen,
    ArgCount,
    WallTime,
    MonotonicTime,
    FillEntropy,
    /// Wait for the given nanoseconds or until the host requests cooperative cancellation.
    /// Return zero to wake the guest or a nonzero error status.
    Sleep,
}

#[derive(Clone, Copy)]
pub enum HostValueType {
    I32,
    I64,
}

impl HostFunction {
    pub fn lookup(module: &str, name: &str) -> Option<Self> {
        if module != HOST_MODULE {
            return None;
        }
        Some(match name {
            "write" => Self::Write,
            "flush" => Self::Flush,
            "arg_read" => Self::ArgRead,
            "arg_len" => Self::ArgLen,
            "arg_count" => Self::ArgCount,
            "wall_time_secs" => Self::WallTime,
            "monotonic_nanos" => Self::MonotonicTime,
            "fill_entropy" => Self::FillEntropy,
            "sleep_ns" => Self::Sleep,
            _ => return None,
        })
    }

    pub fn signature(self) -> (&'static [HostValueType], HostValueType) {
        use HostValueType::{I32, I64};

        match self {
            Self::Write | Self::ArgRead => (&[I32, I32, I32], I32),
            Self::Flush | Self::ArgLen => (&[I32], I32),
            Self::ArgCount => (&[], I32),
            Self::WallTime | Self::MonotonicTime => (&[], I64),
            Self::FillEntropy => (&[I32, I32], I32),
            Self::Sleep => (&[I64], I32),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Registration {
    pub entry_index: u32,
    pub entry: RegisteredEntry,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RegisteredEntry {
    Leaf {
        guards: Box<[LeafGuard]>,
        // Both entry bodies belong to this same FASL index. Their slots are
        // assigned in registration order in two separate function tables.
        fallback: Option<FunctionRegistration>,
    },
    Function(FunctionRegistration),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct FunctionRegistration {
    pub native_debug: bool,
    pub root_slots: u32,
    pub scratch_words: u32,
}

pub fn encode_registrations(entries: &[Registration]) -> Result<Vec<u8>, RegistrationError> {
    postcard::to_allocvec(entries)
}

pub fn decode_registrations(bytes: &[u8]) -> Result<Vec<Registration>, RegistrationError> {
    decode_exact(bytes)
}

/// Ordered, independently indexed FASLs in a linked application.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectImage {
    pub units: Vec<ProjectUnit>,
    pub entry: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectUnit {
    pub fasl: Vec<u8>,
    pub registrations: Vec<u8>,
}

pub fn encode_project(image: &ProjectImage) -> Result<Vec<u8>, RegistrationError> {
    postcard::to_allocvec(image)
}

pub fn decode_project(bytes: &[u8]) -> Result<ProjectImage, RegistrationError> {
    decode_exact(bytes)
}

fn decode_exact<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, RegistrationError> {
    let (value, remaining) = postcard::take_from_bytes(bytes)?;
    if !remaining.is_empty() {
        return Err(RegistrationError::DeserializeBadEncoding);
    }
    Ok(value)
}
