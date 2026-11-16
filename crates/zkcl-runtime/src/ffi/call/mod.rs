//! Prepared library/function bindings. Argument roots come directly from the VM call.
mod direct;

use std::ffi::{CStr, CString, c_void};
use std::rc::Rc;

use libffi::middle::{Cif, CodePtr};
use libffi::raw;
use libloading::Library;
use smallvec::SmallVec;

use super::aggregate::{ForeignType, StructBuffer};
use super::{
    Argument, CType, CallResult, ForeignCallError, arity, callback, integer, memory, string,
};
use crate::condition::{Fault, named_word};
use crate::heap::RootSlots;
use crate::package::Package;
use crate::{EvalError, Root, Runtime, vm};

pub(super) fn install(rt: &Runtime) -> Result<(), EvalError> {
    type Native = for<'rt> fn(&'rt Runtime, &[Root<'rt>], usize) -> CallResult<'rt>;
    let operations: &[(&str, Native)] = &[
        ("ZK-FFI:OPEN-LIBRARY", open_library),
        ("ZK-FFI:FIND-SYMBOL-POINTER", find_symbol_pointer),
        ("ZK-FFI:BIND-FUNCTION", bind_function),
        ("ZK-FFI:BIND-POINTER", bind_pointer),
        ("ZK-FFI:BIND-VARIADIC-FUNCTION", bind_variadic_function),
        ("ZK-FFI:BIND-VARIADIC-POINTER", bind_variadic_pointer),
    ];
    for &(name, op) in operations {
        vm::install_host_function(rt, name, std::sync::Arc::new(op))?;
    }
    Ok(())
}

fn open_library<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [path] = args else {
        return Err(arity(at, "OPEN-LIBRARY expects a library path"));
    };
    let path = string(rt, path, at)?;
    // SAFETY: the opted-in host accepts native library constructors and destructors.
    let library = unsafe { Library::new(path.as_str()) }
        .map_err(|source| EvalError::from_error(at, ForeignCallError::Open { path, source }))?;
    Ok(vec![
        rt.foreign_owned(Rc::new(library))
            .map_err(|e| EvalError::out_of_memory(at, e))?,
    ])
}

enum LibraryHandle {
    Host,
    Dynamic(Rc<Library>),
}

fn current_process_library(at: usize) -> Result<LibraryHandle, Fault> {
    #[cfg(unix)]
    let current = {
        let _ = at;
        libloading::os::unix::Library::this()
    };
    #[cfg(windows)]
    let current = libloading::os::windows::Library::this()
        .map_err(|error| EvalError::from_error(at, error))?;
    Ok(LibraryHandle::Dynamic(Rc::new(current.into())))
}

fn library_handle(rt: &Runtime, value: &Root<'_>, at: usize) -> Result<LibraryHandle, Fault> {
    if let Ok(lib) = value.with_owned_foreign_ref::<Rc<Library>, _>(rt, Rc::clone) {
        return Ok(LibraryHandle::Dynamic(lib));
    }
    match named_word(rt, value.slot.get(), Package::KEYWORD).as_deref() {
        Some("HOST") => Ok(LibraryHandle::Host),
        Some("DEFAULT") => current_process_library(at),
        _ => Err(EvalError::new(
            at,
            "expected a library from OPEN-LIBRARY, :HOST or :DEFAULT",
        )
        .into()),
    }
}

fn symbol_name(rt: &Runtime, value: &Root<'_>, at: usize) -> Result<CString, Fault> {
    Ok(CString::new(string(rt, value, at)?).map_err(|source| {
        EvalError::from_error(
            at,
            ForeignCallError::InteriorNul {
                argument: "symbol name",
                source,
            },
        )
    })?)
}

impl LibraryHandle {
    fn lookup(
        self,
        rt: &Runtime,
        name: &CStr,
    ) -> Result<(*mut c_void, memory::Origin), ForeignCallError> {
        let text = name.to_str().expect("Lisp names are UTF-8");
        match self {
            Self::Host => rt
                .foreign_symbols
                .borrow()
                .get(text)
                .map(|address| (address.as_ptr(), memory::Origin::default()))
                .ok_or_else(|| ForeignCallError::MissingHostSymbol(text.to_owned())),
            Self::Dynamic(library) => {
                // SAFETY: request the symbol address. The binding prepares
                // the declared signature and retains its originating library.
                let address = unsafe { library.get::<*mut c_void>(name.to_bytes_with_nul()) }
                    .map(|symbol| *symbol)
                    .map_err(|source| ForeignCallError::Bind {
                        name: text.to_owned(),
                        source,
                    })?;
                Ok((address, memory::Origin::owned(library)))
            }
        }
    }
}

fn find_symbol_pointer<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [library, name] = args else {
        return Err(arity(
            at,
            "FIND-SYMBOL-POINTER expects a library and C symbol name",
        ));
    };
    let library = library_handle(rt, library, at)?;
    let name = symbol_name(rt, name, at)?;
    let value = match library.lookup(rt, &name) {
        Ok((ptr, origin)) => origin.wrap(rt, ptr, at)?,
        Err(_) => rt.nil(),
    };
    Ok(vec![value])
}

fn bind_function<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [lib, symbol, result, params @ ..] = args else {
        return Err(arity(
            at,
            "BIND-FUNCTION expects a library, name, result and parameter types",
        ));
    };
    bind_named(rt, lib, symbol, None, result, params, at)
}

fn bind_variadic_function<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [lib, symbol, fixed, result, params @ ..] = args else {
        return Err(arity(
            at,
            "BIND-VARIADIC-FUNCTION expects a library, name, fixed parameter count, result and parameter types",
        ));
    };
    bind_named(
        rt,
        lib,
        symbol,
        Some(integer(rt, fixed, at)?),
        result,
        params,
        at,
    )
}

fn bind_named<'rt>(
    rt: &'rt Runtime,
    lib: &Root<'_>,
    symbol: &Root<'_>,
    fixed: Option<usize>,
    result: &Root<'_>,
    params: &[Root<'_>],
    at: usize,
) -> CallResult<'rt> {
    let library = library_handle(rt, lib, at)?;
    let name = symbol_name(rt, symbol, at)?;
    let (address, owner) = library
        .lookup(rt, &name)
        .map_err(|error| EvalError::from_error(at, error))?;
    let target = ForeignCallTarget {
        pointer: memory::Pointer::external(address, owner),
        name: name.into_string().expect("Lisp names are UTF-8"),
    };
    target.prepare(rt, lib, fixed, result, params, at)
}

fn bind_pointer<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [ptr, result, params @ ..] = args else {
        return Err(arity(
            at,
            "BIND-POINTER expects a function pointer, result and parameter types",
        ));
    };
    ForeignCallTarget::from_pointer(rt, ptr, at)?.prepare(rt, ptr, None, result, params, at)
}

fn bind_variadic_pointer<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [ptr, fixed, result, params @ ..] = args else {
        return Err(arity(
            at,
            "BIND-VARIADIC-POINTER expects a function pointer, fixed parameter count, result and parameter types",
        ));
    };
    ForeignCallTarget::from_pointer(rt, ptr, at)?.prepare(
        rt,
        ptr,
        Some(integer(rt, fixed, at)?),
        result,
        params,
        at,
    )
}

struct ForeignCallTarget {
    pointer: memory::Pointer,
    name: String,
}

impl ForeignCallTarget {
    fn check_arity(&self, expected: usize, actual: usize, at: usize) -> Result<(), Fault> {
        if expected == actual {
            return Ok(());
        }
        Err(self.wrong_arity(expected, actual, at))
    }

    #[cold]
    fn wrong_arity(&self, expected: usize, actual: usize, at: usize) -> Fault {
        arity(
            at,
            format!("{} expects {expected} arguments, got {actual}", self.name),
        )
    }

    fn from_pointer(rt: &Runtime, ptr: &Root<'_>, at: usize) -> Result<Self, Fault> {
        let pointer = memory::Pointer::get(rt, ptr, at)?;
        pointer.check_function_address(at)?;
        let address = pointer.raw();
        Ok(Self {
            pointer,
            name: format!("native function {address:p}"),
        })
    }

    fn call_address(&self, at: usize) -> Result<CodePtr, Fault> {
        self.pointer.check_live(at)?;
        Ok(CodePtr(self.pointer.raw()))
    }

    fn prepare<'rt>(
        self,
        rt: &'rt Runtime,
        capture: &Root<'_>,
        fixed: Option<usize>,
        result: &Root<'_>,
        params: &[Root<'_>],
        at: usize,
    ) -> CallResult<'rt> {
        if self.pointer.raw().is_null() {
            return Err(
                EvalError::new(at, format!("null function pointer for {}", self.name)).into(),
            );
        }
        if fixed.is_some_and(|n| n == 0 || n > params.len()) {
            return Err(EvalError::new(
                at,
                "variadic fixed parameter count must be between one and the total parameter count",
            )
            .into());
        }
        let result = ForeignType::parse(rt, result, at)?;
        let params = ForeignType::parse_parameters(rt, params, at)?;
        if fixed.is_none()
            && params.len() <= 2
            && let ForeignType::Scalar(ty) = &result
            && direct::supports(*ty)
            && params
                .iter()
                .all(|p| matches!(p, ForeignType::Scalar(t) if t == ty))
        {
            let function = direct::bind(*ty, params.len(), self, rt, capture)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            return Ok(vec![function]);
        }
        let fixed_count = fixed.unwrap_or(params.len());
        let abi_params = params.iter().enumerate().map(|(i, ty)| match ty {
            ForeignType::Scalar(s) if i >= fixed_count => s.promoted().abi(),
            _ => ty.abi(),
        });
        let cif = match fixed {
            Some(count) => Cif::try_new_variadic(abi_params, count, result.abi()),
            None => Cif::try_new(abi_params, result.abi()),
        }
        .map_err(|reason| {
            EvalError::from_error(
                at,
                ForeignCallError::Signature {
                    name: self.name.clone(),
                    reason,
                },
            )
        })?;
        if fixed.is_none()
            && params.len() <= MAX_STACK_ARGS
            && let ForeignType::Scalar(ty) = &result
            && (*ty == CType::Void || direct::supports(*ty))
            && let Some(encoders) = params
                .iter()
                .map(|param| match param {
                    ForeignType::Scalar(ty) => direct::encoder(*ty),
                    ForeignType::Struct(_) => None,
                })
                .collect::<Option<Vec<_>>>()
        {
            let binding = NumericBinding {
                target: self,
                cif,
                encoders,
                result: *ty,
            };
            let function = rt
                .host_function(capture, move |rt, slots, at| binding.call(rt, slots, at))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            return Ok(vec![function]);
        }
        let pointer_arguments = params
            .iter()
            .enumerate()
            .filter_map(|(index, ty)| {
                matches!(
                    ty,
                    ForeignType::Scalar(CType::Pointer) | ForeignType::Struct(_)
                )
                .then_some(index)
            })
            .collect();
        let binding = Binding {
            target: self,
            cif,
            params,
            pointer_arguments,
            fixed_count,
            result,
        };
        let function = rt
            .host_function(capture, move |rt, slots, at| {
                Ok(binding.call(rt, slots, at)?.map(|root| root.slot.get()))
            })
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        Ok(vec![function])
    }
}

const MAX_STACK_ARGS: usize = 8;

struct NumericBinding {
    target: ForeignCallTarget,
    cif: Cif,
    encoders: Vec<direct::Encode>,
    result: CType,
}

impl NumericBinding {
    fn call(
        &self,
        rt: &Runtime,
        slots: &RootSlots,
        at: usize,
    ) -> Result<Option<crate::heap::Word>, Fault> {
        self.target
            .check_arity(self.encoders.len(), slots.len(), at)?;
        let mut storage = [direct::NumericArgument::UNINIT; MAX_STACK_ARGS];
        let mut args = [std::ptr::null_mut(); MAX_STACK_ARGS];
        for (index, encode) in self.encoders.iter().enumerate() {
            encode(rt, slots.get(index), &mut storage[index], at)?;
            args[index] = storage[index].as_mut_ptr();
        }
        let address = self.target.call_address(at)?;
        let value = callback::with_call(
            rt,
            at,
            // SAFETY: each encoder initialized the exact C parameter type,
            // and binding validated the non-variadic numeric signature.
            || unsafe { invoke_scalar(&self.cif, address, self.result, &mut args) },
            |out, _| out.root_number(rt, at),
        )?;
        Ok(value.map(|root| root.slot.get()))
    }
}

struct Binding {
    target: ForeignCallTarget,
    cif: Cif,
    params: Vec<ForeignType>,
    pointer_arguments: Vec<usize>,
    fixed_count: usize,
    result: ForeignType,
}

enum NativeArgument {
    Scalar(Argument),
    Struct(StructBuffer),
}

impl NativeArgument {
    fn as_mut_ptr(&mut self) -> *mut c_void {
        match self {
            Self::Scalar(v) => v.as_mut_ptr(),
            Self::Struct(v) => v.as_mut_ptr(),
        }
    }
}

impl Binding {
    fn call<'rt>(
        &self,
        rt: &'rt Runtime,
        slots: &RootSlots,
        at: usize,
    ) -> Result<Option<Root<'rt>>, Fault> {
        self.target
            .check_arity(self.params.len(), slots.len(), at)?;
        // The VM keeps all slots live across GC and callback reentry. C receives
        // our own scalar/array storage, never an address inside the Lisp heap.
        let mut strings = memory::StringBuffers::new();
        let mut storage = SmallVec::<[NativeArgument; MAX_STACK_ARGS]>::with_capacity(slots.len());
        for (i, ty) in self.params.iter().enumerate() {
            let word = slots.get(i);
            let value = match ty {
                ForeignType::Scalar(ty) => {
                    let v = Argument::from_slot(rt, word, *ty, &mut strings, at)?;
                    NativeArgument::Scalar(if i < self.fixed_count { v } else { v.promote() })
                }
                ForeignType::Struct(s) => {
                    NativeArgument::Struct(s.argument(rt, &rt.root(word), at)?)
                }
            };
            storage.push(value);
        }
        // Some ABIs rewrite elements of avalue for indirectly passed structures.
        // Each call owns this writable array and the values it points into.
        let mut args: SmallVec<[*mut c_void; MAX_STACK_ARGS]> =
            storage.iter_mut().map(NativeArgument::as_mut_ptr).collect();
        let address = self.target.call_address(at)?;
        let inputs = callback::CallInputs {
            slots,
            pointer_arguments: &self.pointer_arguments,
            strings: &strings,
        };
        let inputs =
            (!inputs.pointer_arguments.is_empty() || !inputs.strings.is_empty()).then_some(&inputs);
        let ty = match &self.result {
            ForeignType::Scalar(ty) => *ty,
            ForeignType::Struct(s) => {
                // SAFETY: the prepared CIF describes the marshalled arguments and result.
                return unsafe {
                    s.call_result(
                        rt,
                        &self.cif,
                        address,
                        &mut args,
                        self.target.pointer.origin().clone(),
                        at,
                        inputs,
                    )
                }
                .map(Some);
            }
        };
        callback::with_call_inputs(
            rt,
            at,
            inputs,
            || {
                // SAFETY: the opted-in host supplies the C prototype. The CIF and
                // argument storage agree, and reentrant calls have independent buffers.
                unsafe { invoke_scalar(&self.cif, address, ty, &mut args) }
            },
            |out, frame| {
                let RawResult::Pointer(ptr) = out else {
                    return out.root_number(rt, at);
                };
                if ptr.is_null() {
                    return Ok(Some(rt.nil()));
                }
                if ty == CType::CString {
                    return frame.read_string(ptr).map(Some);
                }
                match frame.find_pointer(ptr)? {
                    Some(pointer) => Ok(Some(pointer)),
                    None => self.target.pointer.origin().wrap(rt, ptr, at).map(Some),
                }
            },
        )
    }
}

unsafe fn invoke_scalar(
    cif: &Cif,
    address: CodePtr,
    ty: CType,
    args: &mut [*mut c_void],
) -> RawResult {
    // ffi_call may widen an integer to ffi_arg before storing it. Initialize
    // a full-width buffer and read the corresponding native-endian member;
    // this also covers a 64-bit result on a 32-bit target.
    let mut result = ScalarStorage { wide: 0 };
    let destination = if ty == CType::Void {
        std::ptr::null_mut()
    } else {
        std::ptr::from_mut(&mut result).cast()
    };
    unsafe {
        raw::ffi_call(
            cif.as_raw_ptr(),
            Some(*address.as_fun()),
            destination,
            args.as_mut_ptr(),
        );
        match ty {
            CType::I8 => RawResult::Signed((result.register as i8).into()),
            CType::U8 => RawResult::Unsigned((result.register as u8).into()),
            CType::I16 => RawResult::Signed((result.register as i16).into()),
            CType::U16 => RawResult::Unsigned((result.register as u16).into()),
            CType::I32 => RawResult::Signed((result.register as i32).into()),
            CType::U32 => RawResult::Unsigned((result.register as u32).into()),
            CType::I64 => RawResult::Signed(result.wide as i64),
            CType::U64 => RawResult::Unsigned(result.wide),
            CType::Float => RawResult::Float(result.single),
            CType::Double => RawResult::Double(result.double),
            CType::CString | CType::Pointer => RawResult::Pointer(result.pointer_value()),
            CType::Void => RawResult::Void,
        }
    }
}

#[repr(C, align(8))]
union ScalarStorage {
    register: super::FfiWord,
    wide: u64,
    single: f32,
    double: f64,
    pointer: *mut c_void,
}

impl ScalarStorage {
    unsafe fn pointer_value(&self) -> *mut c_void {
        // Some ILP32 ABIs widen a pointer result to a whole register. Preserve
        // its pointer representation at the low end of that register.
        if cfg!(target_endian = "big") && size_of::<super::FfiWord>() > size_of::<*mut c_void>() {
            let offset = size_of::<super::FfiWord>() - size_of::<*mut c_void>();
            unsafe {
                std::ptr::from_ref(self)
                    .cast::<u8>()
                    .add(offset)
                    .cast::<*mut c_void>()
                    .read_unaligned()
            }
        } else {
            unsafe { self.pointer }
        }
    }
}

enum RawResult {
    Void,
    Signed(i64),
    Unsigned(u64),
    Float(f32),
    Double(f64),
    Pointer(*mut c_void),
}

impl RawResult {
    fn root_number(self, rt: &Runtime, at: usize) -> Result<Option<Root<'_>>, Fault> {
        let number = match self {
            Self::Signed(n) => rt.integer_from(n),
            Self::Unsigned(n) => rt.integer_from(n),
            Self::Float(n) => rt.make_single_float(n),
            Self::Double(n) => rt.make_double_float(n),
            Self::Void => return Ok(None),
            Self::Pointer(_) => unreachable!("expected a numeric or void result"),
        };
        Ok(Some(number.map_err(|e| EvalError::out_of_memory(at, e))?))
    }
}
