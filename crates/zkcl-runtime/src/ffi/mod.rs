//! Native C ABI operations available to a runtime that opts into foreign code.
mod aggregate;
mod call;
mod callback;
mod memory;
mod types;

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::c_void;
use std::ptr::NonNull;

use crate::condition::{Class, Fault};
use crate::{EvalError, Root, Runtime};
use types::{Argument, CType, integer};

#[derive(Default)]
pub(crate) struct ForeignSymbolTable(RefCell<HashMap<String, NonNull<c_void>>>);

impl ForeignSymbolTable {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }

    pub(super) fn borrow(&self) -> std::cell::Ref<'_, HashMap<String, NonNull<c_void>>> {
        self.0.borrow()
    }
}

// libffi widens narrow integral return values to the target ABI's argument
// register. x32, arm64_32 and MIPS N32 have 64-bit registers and 32-bit pointers.
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "mips64",
    target_arch = "mips64r6"
))]
type FfiWord = u64;
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "mips64",
    target_arch = "mips64r6"
)))]
type FfiWord = usize;

type CallResult<'rt> = Result<Vec<Root<'rt>>, Fault>;
#[derive(Debug, thiserror::Error)]
enum ForeignCallError {
    #[error("cannot open {path:?}: {source}")]
    Open {
        path: String,
        source: libloading::Error,
    },
    #[error("cannot bind {name:?}: {source}")]
    Bind {
        name: String,
        source: libloading::Error,
    },
    #[error("host has not registered C symbol {0:?}")]
    MissingHostSymbol(String),
    #[error("invalid C signature for {name:?}: {reason:?}")]
    Signature {
        name: String,
        reason: libffi::low::Error,
    },
    #[error("invalid C structure layout: {0:?}")]
    StructLayout(libffi::low::Error),
    #[error("C structure does not fit in addressable memory")]
    StructAddressSpace(#[source] std::alloc::LayoutError),
    #[error("C structure return buffer is too large")]
    StructReturnLayout(#[source] std::alloc::LayoutError),
    #[error("foreign allocation is too large")]
    AllocationLayout(#[source] std::alloc::LayoutError),
    #[error("a C {argument} cannot contain NUL")]
    InteriorNul {
        argument: &'static str,
        source: std::ffi::NulError,
    },
    #[error("returned C string is not UTF-8")]
    ReturnedString(#[source] std::str::Utf8Error),
}

pub(crate) fn initialize(rt: &Runtime) -> Result<(), EvalError> {
    rt.load_builtin_source(include_str!("../../lisp/runtime/ffi/ffi.lisp"))?;
    memory::install(rt)?;
    aggregate::initialize(rt)?;
    callback::install(rt)?;
    call::install(rt)
}

fn arity(at: usize, message: impl Into<String>) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

fn string(rt: &Runtime, value: &Root<'_>, at: usize) -> Result<String, Fault> {
    rt.string(value).ok_or_else(|| {
        Fault::type_error(rt, 
            value.slot.get(),
            "STRING",
            EvalError::new(at, "expected a Lisp string"),
        )
    })
}

impl Runtime {
    /// Publish an address under an exact, case-sensitive name in this instance's
    /// `:HOST` library. Registration is idempotent for the same name and address.
    /// A name cannot be assigned to a different address.
    ///
    /// # Safety
    ///
    /// The host must keep the function or data valid throughout use by this
    /// runtime. Lisp must use its actual ABI, type, and access permissions.
    pub unsafe fn register_foreign_symbol(
        &self,
        name: &str,
        address: NonNull<c_void>,
    ) -> Result<(), EvalError> {
        self.require_foreign_calls()?;
        if name.is_empty() || name.contains('\0') {
            return Err(EvalError::new(
                0,
                "expected a nonempty C symbol name without NUL",
            ));
        }
        match self.foreign_symbols.0.borrow_mut().entry(name.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(address);
                Ok(())
            }
            Entry::Occupied(entry) if *entry.get() == address => Ok(()),
            Entry::Occupied(_) => Err(EvalError::new(
                0,
                format!("host C symbol {name:?} already has a different address"),
            )),
        }
    }

    /// Wrap an address supplied by the host as a ZK-FFI pointer. A null address
    /// becomes Lisp NIL. The resulting value borrows the native storage.
    ///
    /// # Safety
    ///
    /// The host must keep the address valid for every native use through this
    /// value and its aliases. Lisp must use the actual bounds, access permissions,
    /// and ABI of the address.
    pub unsafe fn make_foreign_pointer(&self, address: *mut c_void) -> Result<Root<'_>, EvalError> {
        self.require_foreign_calls()?;
        memory::from_raw(self, address, None, 0).map_err(Fault::into_eval_error)
    }

    /// Read a foreign pointer (or Lisp NIL, which represents NULL). Keep its
    /// Lisp handle alive and its storage valid for as long as the address is used.
    pub fn foreign_pointer_address(&self, value: &Root<'_>) -> Result<*mut c_void, EvalError> {
        self.require_foreign_calls()?;
        if !value.belongs_to(self) {
            return Err(EvalError::new(0, "value belongs to a different runtime"));
        }
        memory::address(self, value, 0).map_err(Fault::into_eval_error)
    }

    /// Run host code that may synchronously invoke Lisp's C callbacks on this
    /// runtime's owning thread. Retain callback handles while C can invoke them.
    /// A callback failure returns an error after the host function has returned.
    /// Returned C string storage is borrowed until the scope ends.
    /// All entries share this invocation's execution budget.
    pub fn with_foreign_callbacks<T>(&self, run: impl FnOnce() -> T) -> Result<T, EvalError> {
        self.require_foreign_calls()?;
        let _execution = self.enter_host_execution()?;
        callback::with_call(self, 0, run, |result, _| Ok(result)).map_err(Fault::into_eval_error)
    }

    fn require_foreign_calls(&self) -> Result<(), EvalError> {
        if self.foreign_calls {
            Ok(())
        } else {
            Err(EvalError::new(
                0,
                "foreign calls are disabled for this runtime",
            ))
        }
    }
}
