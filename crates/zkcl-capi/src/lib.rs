//! The exported interface and its ownership rules are in include/zkcl.h.

mod abi;
mod breakpoints;
mod evaluation;
mod foreign;
mod native;
mod project;
mod runtime;
mod tooling;

use std::ffi::c_void;
use std::ptr;

use smallvec::SmallVec;
use zkcl::{CancellationHandle, PersistentRoot, Root, Runtime};

use abi::{
    Error, Utf8View, boundary, free_owned_handle, input_slice, require_pointer, write_owned_handle,
};

#[derive(Clone, Copy)]
pub(crate) struct CUserdata(*mut c_void);

// SAFETY: C callers explicitly opt into host callbacks being callable from runtime threads with their userdata pointer.
unsafe impl Send for CUserdata {}
// SAFETY: C callers synchronize any shared access to `userdata` across invocations.
unsafe impl Sync for CUserdata {}

impl CUserdata {
    pub(crate) fn new(ptr: *mut c_void) -> Self {
        Self(ptr)
    }

    pub(crate) fn as_ptr(self) -> *mut c_void {
        self.0
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_runtime_free(rt: *mut Runtime) {
    unsafe { free_owned_handle(rt) }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_collect(rt: Option<&Runtime>) -> *mut Error {
    boundary(|| {
        require_pointer(rt)?.collect();
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_set_step_budget(rt: Option<&Runtime>, budget: Option<&u64>) -> *mut Error {
    boundary(|| {
        require_pointer(rt)?.set_step_budget(budget.copied());
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_cancellation_new(
    rt: Option<&Runtime>,
    out: Option<&mut *mut CancellationHandle>,
) -> *mut Error {
    write_owned_handle(out, || Ok(require_pointer(rt)?.cancellation_handle()))
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_cancel(handle: Option<&CancellationHandle>) {
    if let Some(handle) = handle {
        handle.cancel();
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_cancel_with_cleanup(handle: Option<&CancellationHandle>) {
    if let Some(handle) = handle {
        handle.cancel_with_cleanup();
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_cancel_reset(handle: Option<&CancellationHandle>) {
    if let Some(handle) = handle {
        handle.reset();
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_cancellation_free(handle: *mut CancellationHandle) {
    unsafe { free_owned_handle(handle) }
}

struct Values(SmallVec<[PersistentRoot; 2]>);

impl Values {
    fn from_roots<'rt>(roots: impl IntoIterator<Item = Root<'rt>>) -> Self {
        Self(roots.into_iter().map(Root::into_persistent).collect())
    }
}

impl<'rt> Extend<Root<'rt>> for Values {
    fn extend<T: IntoIterator<Item = Root<'rt>>>(&mut self, values: T) {
        self.0.extend(values.into_iter().map(Root::into_persistent));
    }
}

fn bind_value_to_runtime<'rt>(
    rt: &'rt Runtime,
    value: Option<&PersistentRoot>,
) -> Result<Root<'rt>, Error> {
    require_pointer(value)?
        .bind(rt)
        .ok_or_else(|| Error::invalid("value belongs to a different runtime"))
}

unsafe fn bind_call_arguments<'rt>(
    rt: &'rt Runtime,
    argv: *const *const PersistentRoot,
    argc: usize,
) -> Result<SmallVec<[Root<'rt>; 4]>, Error> {
    unsafe { input_slice(argv, argc)? }
        .iter()
        .map(|p| bind_value_to_runtime(rt, unsafe { p.as_ref() }))
        .collect()
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_eval(
    rt: Option<&Runtime>,
    source: Utf8View,
    out: Option<&mut *mut Values>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(Values::from_roots(
            require_pointer(rt)?.eval(unsafe { source.read_utf8()? })?,
        ))
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_load(rt: Option<&Runtime>, source: Utf8View) -> *mut Error {
    boundary(|| Ok(require_pointer(rt)?.load_str(unsafe { source.read_utf8()? })?))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_load_file(rt: Option<&Runtime>, path: Utf8View) -> *mut Error {
    boundary(|| {
        Ok(require_pointer(rt)?.load_file(std::path::Path::new(unsafe { path.read_utf8()? }))?)
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_load_fasl(
    rt: Option<&Runtime>,
    bytes: *const u8,
    len: usize,
) -> *mut Error {
    boundary(|| Ok(require_pointer(rt)?.load_fasl(unsafe { input_slice(bytes, len)? })?))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_lookup_symbol(
    rt: Option<&Runtime>,
    package: Utf8View,
    name: Utf8View,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    write_owned_handle(out, || {
        let name = unsafe { name.read_utf8()? };
        require_pointer(rt)?
            .lookup_symbol(unsafe { package.read_utf8()? }, name)?
            .map(Root::into_persistent)
            .ok_or_else(|| Error::Invalid(format!("symbol {name:?} not found")))
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_resolve_function(
    rt: Option<&Runtime>,
    target: Option<&PersistentRoot>,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        Ok(rt
            .resolve_function(&bind_value_to_runtime(rt, target)?)?
            .into_persistent())
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_call(
    rt: Option<&Runtime>,
    target: Option<&PersistentRoot>,
    argv: *const *const PersistentRoot,
    argc: usize,
    out: Option<&mut *mut Values>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        let function = bind_value_to_runtime(rt, target)?;
        let args = unsafe { bind_call_arguments(rt, argv, argc)? };
        let args = args.iter().collect::<SmallVec<[_; 4]>>();
        let mut results = Values(SmallVec::new());
        rt.call_into(&function, &args, &mut results)?;
        Ok(results)
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_values_len(values: Option<&Values>) -> usize {
    values.map_or(0, |v| v.0.len())
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_values_get(values: Option<&Values>, index: usize) -> *const PersistentRoot {
    values
        .and_then(|v| v.0.get(index))
        .map_or(ptr::null(), ptr::from_ref)
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_values_free(values: *mut Values) {
    unsafe { free_owned_handle(values) }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_value_retain(
    rt: Option<&Runtime>,
    value: Option<&PersistentRoot>,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(bind_value_to_runtime(require_pointer(rt)?, value)?.into_persistent())
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_value_free(value: *mut PersistentRoot) {
    unsafe { free_owned_handle(value) }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_make_i64(
    rt: Option<&Runtime>,
    n: i64,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(require_pointer(rt)?.integer_from(n)?.into_persistent())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_get_i64(
    rt: Option<&Runtime>,
    value: Option<&PersistentRoot>,
    out: Option<&mut i64>,
) -> *mut Error {
    boundary(|| {
        let rt = require_pointer(rt)?;
        let value = bind_value_to_runtime(rt, value)?;
        *require_pointer(out)? = value
            .as_i64()
            .ok_or_else(|| Error::invalid("expected an integer representable as int64_t"))?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_make_double(
    rt: Option<&Runtime>,
    n: f64,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(require_pointer(rt)?.make_double_float(n)?.into_persistent())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_get_double(
    rt: Option<&Runtime>,
    value: Option<&PersistentRoot>,
    out: Option<&mut f64>,
) -> *mut Error {
    boundary(|| {
        *require_pointer(out)? = bind_value_to_runtime(require_pointer(rt)?, value)?
            .as_double_float()
            .ok_or_else(|| Error::invalid("expected a DOUBLE-FLOAT"))?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_make_string(
    rt: Option<&Runtime>,
    text: Utf8View,
    out: Option<&mut *mut PersistentRoot>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(require_pointer(rt)?
            .make_string(unsafe { text.read_utf8()? })?
            .into_persistent())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_get_string(
    rt: Option<&Runtime>,
    value: Option<&PersistentRoot>,
    out: Option<&mut *mut String>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        rt.string(&bind_value_to_runtime(rt, value)?)
            .ok_or_else(|| Error::invalid("expected a Lisp string"))
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_repr(
    rt: Option<&Runtime>,
    value: Option<&PersistentRoot>,
    out: Option<&mut *mut String>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        Ok(rt.repr(&bind_value_to_runtime(rt, value)?)?)
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_text_view(text: Option<&String>) -> Utf8View {
    Utf8View::from_str(text.map_or("", String::as_str))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_text_free(text: *mut String) {
    unsafe { free_owned_handle(text) }
}

struct CallbackReturns<'rt> {
    runtime: &'rt Runtime,
    roots: Vec<Root<'rt>>,
}

type Callback =
    unsafe extern "C" fn(&Runtime, &Values, *mut CallbackReturns<'_>, *mut c_void) -> *mut Error;

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_register_function(
    rt: Option<&Runtime>,
    name: Utf8View,
    callback: Option<Callback>,
    userdata: *mut c_void,
) -> *mut Error {
    boundary(|| {
        let callback = require_pointer(callback)?;
        let userdata = CUserdata::new(userdata);
        require_pointer(rt)?.register_function(unsafe { name.read_utf8()? }, move |rt, args| {
            let args = Values::from_roots(args.iter().cloned());
            let mut results = CallbackReturns {
                runtime: rt,
                roots: Vec::new(),
            };
            // No mutable borrow of the runtime spans the callback. It may reenter Lisp.
            let error = unsafe { callback(rt, &args, &raw mut results, userdata.as_ptr()) };
            unsafe { abi::check_foreign_error(error)? };
            Ok(results.roots)
        })?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_return_value(
    results: Option<&mut CallbackReturns<'_>>,
    value: Option<&PersistentRoot>,
) -> *mut Error {
    boundary(|| {
        let results = require_pointer(results)?;
        results
            .roots
            .push(bind_value_to_runtime(results.runtime, value)?);
        Ok(())
    })
}
