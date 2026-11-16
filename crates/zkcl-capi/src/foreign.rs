use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

use zkcl::{PersistentRoot, Runtime};

use crate::abi::{Error, Utf8View, boundary, require_pointer, write_owned_handle};

type ForeignFunctionPointer = unsafe extern "C" fn();
type ForeignScopeCallback = unsafe extern "C" fn(*mut c_void) -> *mut Error;

macro_rules! ffi_only {
    ($body:block, ($($used:ident),+ $(,)?)) => {{
        #[cfg(feature = "ffi")]
        $body
        #[cfg(not(feature = "ffi"))]
        {
            let _ = ($($used,)+);
            Err(Error::invalid("zkcl-capi was built without the ffi feature"))
        }
    }};
}

unsafe fn register_foreign_symbol(
    rt: Option<&Runtime>,
    name: Utf8View,
    address: Option<NonNull<c_void>>,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let address = require_pointer(address)?;
        let name = unsafe { name.read_utf8()? };
        ffi_only!(
            {
                // SAFETY: the C caller vouches for the address and its lifetime.
                unsafe { rt.register_foreign_symbol(name, address)? };
                Ok(())
            },
            (rt, address, name)
        )
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_register_foreign_function(
    rt: Option<&Runtime>,
    name: Utf8View,
    address: Option<ForeignFunctionPointer>,
) -> *mut Error {
    let address = address.and_then(|function| NonNull::new(function as *const () as *mut c_void));
    unsafe { register_foreign_symbol(rt, name, address) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_register_foreign_data(
    rt: Option<&Runtime>,
    name: Utf8View,
    address: Option<NonNull<c_void>>,
) -> *mut Error {
    unsafe { register_foreign_symbol(rt, name, address) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_make_foreign_pointer(
    rt: Option<&Runtime>,
    address: *mut c_void,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        ffi_only!(
            {
                // SAFETY: the caller supplies the address, bounds, and lifetime.
                Ok(unsafe { rt.make_foreign_pointer(address)? }.into_persistent())
            },
            (rt, address)
        )
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_get_foreign_pointer(
    rt: Option<&Runtime>,
    value: Option<&PersistentRoot>,
    out: Option<&mut MaybeUninit<usize>>,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let out = require_pointer(out)?;
        ffi_only!(
            {
                let value = crate::bind_value_to_runtime(rt, value)?;
                let address = rt.foreign_pointer_address(&value)?;
                out.write(address.expose_provenance());
                Ok(())
            },
            (rt, out, value)
        )
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_with_foreign_callbacks(
    rt: Option<&Runtime>,
    run: Option<ForeignScopeCallback>,
    userdata: *mut c_void,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let run = require_pointer(run)?;
        ffi_only!(
            {
                let error = rt.with_foreign_callbacks(|| {
                    // Adopt a returned error here so it is also dropped when a
                    // Lisp callback has failed and its error takes precedence.
                    let error = unsafe { run(userdata) };
                    NonNull::new(error).map(|error| unsafe { Box::from_raw(error.as_ptr()) })
                })?;
                match error {
                    Some(error) => Err(*error),
                    None => Ok(()),
                }
            },
            (rt, run, userdata)
        )
    })
}
