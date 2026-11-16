//! Native closures enter their owning runtime within a synchronous callback scope.
use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::c_void;
use std::mem::DropGuard;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::rc::{Rc, Weak};
use std::thread::{self, ThreadId};

use libffi::middle::Cif;
use libffi::{low, raw};
use smallvec::SmallVec;

use super::aggregate::ForeignType;
use super::{Argument, CType, CallResult, FfiWord, ForeignCallError, arity, memory};
use crate::condition::{self, Fault};
use crate::heap::{ForeignEdge, RootSlots, Word};
use crate::{EvalError, PersistentRoot, Root, Runtime, vm};

pub(super) fn install(rt: &Runtime) -> Result<(), EvalError> {
    vm::install_host_function(rt, "ZK-FFI::%MAKE-CALLBACK", std::sync::Arc::new(make_callback))
}

struct CallbackClosure {
    allocation: *mut low::ffi_closure,
    cif: Cif,
    thread: ThreadId,
    result: ForeignType,
    result_bytes: usize,
    params: Vec<ForeignType>,
    target: ForeignEdge,
    weak_self: Weak<CallbackClosure>,
    active_pin: RefCell<Weak<PinnedCallback>>,
}

impl Drop for CallbackClosure {
    fn drop(&mut self) {
        // SAFETY: this is the unique owner of the executable allocation. A
        // caller holding a registered C pointer must retain its Lisp handle.
        unsafe { low::closure_free(self.allocation) }
    }
}

struct PinnedCallback {
    _closure: Rc<CallbackClosure>,
    target: PersistentRoot,
}

fn make_callback<'rt>(rt: &'rt Runtime, args: &[Root<'rt>], at: usize) -> CallResult<'rt> {
    let [target, result, params @ ..] = args else {
        return Err(arity(
            at,
            "MAKE-CALLBACK expects a function and a result type",
        ));
    };
    let result = ForeignType::parse(rt, result, at)?;
    // libffi closure result storage has exactly the declared size, except for
    // narrow integers widened to argument words. A void result is unspecified.
    let result_bytes = match &result {
        ForeignType::Struct(ty) => ty.size(),
        ForeignType::Scalar(CType::Void) => 0,
        ForeignType::Scalar(CType::Float) => size_of::<f32>(),
        ForeignType::Scalar(CType::Double | CType::I64 | CType::U64) => 8,
        ForeignType::Scalar(_) => size_of::<FfiWord>(),
    };
    let params = ForeignType::parse_parameters(rt, params, at)?;
    let cif = Cif::try_new(params.iter().map(|ty| ty.abi()), result.abi()).map_err(|reason| {
        EvalError::from_error(
            at,
            ForeignCallError::Signature {
                name: "callback".into(),
                reason,
            },
        )
    })?;
    let edge = ForeignEdge::empty(rt);
    let (allocation, code) = low::try_closure_alloc()
        .ok_or_else(|| EvalError::new(at, "cannot allocate executable memory for a callback"))?;
    let callback = Rc::new_cyclic(|weak_self| CallbackClosure {
        allocation,
        cif,
        thread: thread::current().id(),
        result,
        result_bytes,
        params,
        target: edge.clone(),
        weak_self: weak_self.clone(),
        active_pin: RefCell::default(),
    });
    // SAFETY: the Rc gives the CIF and userdata stable addresses until the
    // executable is freed. The raw closure interface permits a null void result.
    let status = unsafe {
        raw::ffi_prep_closure_loc(
            allocation,
            callback.cif.as_raw_ptr(),
            Some(trampoline),
            Rc::as_ptr(&callback).cast_mut().cast(),
            code.0,
        )
    };
    if status != raw::ffi_status_FFI_OK {
        return Err(EvalError::new(
            at,
            format!("cannot prepare C callback (libffi status {status})"),
        )
        .into());
    }
    let pointer = memory::from_callback(rt, code.0, callback, &edge, at)?;
    // Allocation can collect before the new wrapper exists. Read the updated
    // root only after the wrapper and its traced edge have been installed.
    edge.set(target);
    Ok(vec![pointer])
}

thread_local! {
    static ACTIVE: Cell<*const ()> = const { Cell::new(ptr::null()) };
}

pub(super) struct CallInputs<'a> {
    pub(super) slots: &'a RootSlots,
    pub(super) pointer_arguments: &'a [usize],
    pub(super) strings: &'a [memory::StringBuffer],
}

pub(super) struct CallFrame<'rt, 'args> {
    rt: &'rt Runtime,
    at: usize,
    previous: *const (),
    inputs: Option<&'args CallInputs<'args>>,
    callbacks: OnceCell<CallbackState<'rt>>,
}

#[derive(Default)]
struct CallbackState<'rt> {
    failures: RefCell<Vec<Failure>>,
    callbacks: RefCell<Vec<Rc<PinnedCallback>>>,
    /// Keep callback result pointers alive through native result conversion.
    pointers: RefCell<Vec<Root<'rt>>>,
}

impl<'rt> CallbackState<'rt> {
    fn retain_pointer(&self, pointer: Root<'rt>) {
        let mut pointers = self.pointers.borrow_mut();
        if pointers
            .last()
            .is_none_or(|last| !last.same_identity(&pointer))
        {
            pointers.push(pointer);
        }
    }
}

impl<'rt> CallFrame<'rt, '_> {
    fn visit_scopes(
        &self,
        mut visit: impl FnMut(&Self) -> Result<bool, Fault>,
    ) -> Result<(), Fault> {
        let mut current = self;
        loop {
            if ptr::eq(self.rt, current.rt) && visit(current)? {
                return Ok(());
            }
            // SAFETY: each parent is a synchronous native frame still on this
            // thread's stack. Its borrowed slots live through this visit.
            let Some(previous) = (unsafe { current.previous.cast::<Self>().as_ref() }) else {
                return Ok(());
            };
            current = previous;
        }
    }

    fn search_pointer(&self, address: *mut c_void) -> Result<memory::AliasSearch<'rt>, Fault> {
        let mut search = memory::AliasSearch::new(address);
        self.visit_scopes(|frame| {
            if let Some(inputs) = frame.inputs {
                for &index in inputs.pointer_arguments {
                    let word = inputs.slots.get(index);
                    if word != Word::NIL {
                        search.consider(self.rt, &self.rt.root(word), self.at)?;
                        if search.found_allocation() {
                            return Ok(true);
                        }
                    }
                }
                search.consider_strings(inputs.strings);
                if search.found_allocation() {
                    return Ok(true);
                }
            }
            if let Some(callbacks) = frame.callbacks.get() {
                // Comparing owners can register roots but cannot collect. The
                // selected owner is turned into a new Lisp object after this visit.
                for pointer in callbacks.pointers.borrow().iter().rev() {
                    search.consider(self.rt, pointer, self.at)?;
                    if search.found_allocation() {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        })?;
        Ok(search)
    }

    pub(super) fn find_pointer(&self, address: *mut c_void) -> Result<Option<Root<'rt>>, Fault> {
        self.search_pointer(address)?.finish(self.rt, self.at)
    }

    pub(super) fn read_string(&self, address: *mut c_void) -> Result<Root<'rt>, Fault> {
        self.search_pointer(address)?.read_string(self.rt, self.at)
    }

    /// Keep potential field owners without examining an aggregate's native bytes.
    pub(super) fn retain_pointers(&self) -> Result<Vec<Root<'rt>>, Fault> {
        let mut owners = Vec::new();
        self.visit_scopes(|frame| {
            if let Some(callbacks) = frame.callbacks.get() {
                owners.extend(callbacks.pointers.borrow().iter().cloned());
            }
            if let Some(inputs) = frame.inputs {
                for &index in inputs.pointer_arguments {
                    let word = inputs.slots.get(index);
                    if word != Word::NIL {
                        owners.push(self.rt.root(word));
                    }
                }
                for buffer in inputs.strings {
                    owners.push(buffer.pointer(self.rt, buffer.as_ptr(), self.at)?);
                }
            }
            Ok(false)
        })?;
        Ok(owners)
    }
}

enum Failure {
    Fault(Fault),
    Condition(PersistentRoot),
    Panic(Box<dyn std::any::Any + Send>),
}

impl From<Fault> for Failure {
    fn from(fault: Fault) -> Self {
        Self::Fault(fault)
    }
}

impl From<EvalError> for Failure {
    fn from(error: EvalError) -> Self {
        Self::Fault(error.into())
    }
}

pub(super) fn with_call<'rt, T, U>(
    rt: &'rt Runtime,
    at: usize,
    native: impl FnOnce() -> T,
    convert: impl FnOnce(T, &CallFrame<'rt, '_>) -> Result<U, Fault>,
) -> Result<U, Fault> {
    with_call_inputs(rt, at, None, native, convert)
}

pub(super) fn with_call_inputs<'rt, T, U>(
    rt: &'rt Runtime,
    at: usize,
    inputs: Option<&CallInputs<'_>>,
    native: impl FnOnce() -> T,
    convert: impl FnOnce(T, &CallFrame<'rt, '_>) -> Result<U, Fault>,
) -> Result<U, Fault> {
    // Install the TLS frame for every call, including C functions which
    // register and use a callback before they return. Build the registries
    // lazily because most calls never enter the trampoline.
    ACTIVE.with(|active| {
        let previous = active.get();
        let frame = CallFrame {
            rt,
            at,
            previous,
            inputs,
            callbacks: OnceCell::new(),
        };
        let returned = {
            active.set(ptr::from_ref(&frame).cast());
            let _restore = DropGuard::new(previous, |previous| active.set(previous));
            native()
        };
        let failure = frame
            .callbacks
            .get()
            .and_then(|callbacks| callbacks.failures.take().into_iter().next());
        match failure {
            None => convert(returned, &frame),
            Some(Failure::Fault(fault)) => Err(fault),
            Some(Failure::Condition(condition)) => {
                let c = condition
                    .bind(rt)
                    .expect("callback condition belongs to this frame");
                Err(Fault::existing_condition(
                    &c,
                    condition::to_eval_error(rt, c.slot.get(), at),
                ))
            }
            Some(Failure::Panic(payload)) => {
                let text = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                Err(EvalError::new(at, format!("panic in foreign callback: {text}")).into())
            }
        }
    })
}

unsafe extern "C" fn trampoline(
    _cif: *mut raw::ffi_cif,
    result: *mut c_void,
    args: *mut *mut c_void,
    data: *mut c_void,
) {
    let data = data.cast::<CallbackClosure>();
    // SAFETY: the caller retains the callback for as long as C may use it.
    // Read only immutable Copy fields before checking thread identity. No Rc,
    // runtime state, or Lisp word may be touched on a foreign thread.
    let zero_result = || {
        let size = unsafe { ptr::addr_of!((*data).result_bytes).read() };
        if size != 0 {
            unsafe { result.write_bytes(0, size) };
        }
    };
    let Ok(active) = ACTIVE.try_with(Cell::get) else {
        zero_result();
        return;
    };
    if active.is_null() {
        zero_result();
        return;
    }
    // SAFETY: with_call installs a thread-local stack address for the duration
    // of native() and restores the previous address on every exit path.
    let frame = unsafe { &*active.cast::<CallFrame<'_, '_>>() };
    let callbacks = frame.callbacks.get_or_init(CallbackState::default);
    if !callbacks.failures.borrow().is_empty() {
        zero_result();
        return;
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if unsafe { ptr::addr_of!((*data).thread).read() } != thread::current().id() {
            return Err(EvalError::new(frame.at, "callback belongs to a different thread").into());
        }
        // SAFETY: only the executable's owning thread reaches its Lisp state.
        unsafe { invoke(frame, callbacks, &*data, args, result) }
    }));
    let failure = match outcome {
        Ok(Ok(())) => return,
        Ok(Err(failure)) => failure,
        Err(panic) => Failure::Panic(panic),
    };
    zero_result();
    // Defer destruction of all failure payloads until after the C call. A
    // second failure is possible if a host function directly reentered this frame.
    callbacks.failures.borrow_mut().push(failure);
}

unsafe fn invoke<'rt>(
    frame: &CallFrame<'rt, '_>,
    callbacks: &CallbackState<'rt>,
    callback: &CallbackClosure,
    args: *mut *mut c_void,
    result: *mut c_void,
) -> Result<(), Failure> {
    let rt = frame.rt;
    let at = frame.at;
    let wrong_runtime = || EvalError::new(at, "callback belongs to a different runtime");
    let pin = callback.active_pin.borrow().upgrade();
    let pin = match pin {
        Some(pin) => pin,
        None => {
            let target = callback.target.bind(rt).ok_or_else(wrong_runtime)?;
            // Retain code and target independently of Lisp handles while native
            // callers are active. Nested frames can share this same host root.
            let pin = Rc::new(PinnedCallback {
                _closure: callback
                    .weak_self
                    .upgrade()
                    .expect("C holds a live callback"),
                target: target.into_persistent(),
            });
            *callback.active_pin.borrow_mut() = Rc::downgrade(&pin);
            pin
        }
    };
    let function = pin.target.slot_for(rt).ok_or_else(wrong_runtime)?;
    {
        let mut active = callbacks.callbacks.borrow_mut();
        if !active.iter().any(|held| Rc::ptr_eq(held, &pin)) {
            active.push(Rc::clone(&pin));
        }
    }
    let values = callback
        .params
        .iter()
        .enumerate()
        .map(|(index, ty)| {
            // SAFETY: libffi supplies a pointer to the declared value for each parameter.
            let argument = unsafe { *args.add(index) };
            match ty {
                ForeignType::Scalar(ty) => unsafe { read_argument(frame, *ty, argument) },
                ForeignType::Struct(ty) => unsafe {
                    ty.copy_callback_argument(rt, argument, at, frame)
                },
            }
        })
        .collect::<Result<SmallVec<[_; 4]>, _>>()?;
    let returned = rt.call_slots(function, values.iter().map(|value| value.slot.get()))?;
    let condition = returned.get(1);
    if condition != Word::NIL {
        if !condition::is_condition(rt, condition) {
            return Err(Fault::type_error(rt, 
                condition,
                "CONDITION",
                EvalError::new(at, "callback error value must be a condition"),
            )
            .into());
        }
        return Err(Failure::Condition(rt.root(condition).into_persistent()));
    }
    let ty = match &callback.result {
        ForeignType::Scalar(CType::Void) => return Ok(()),
        ForeignType::Scalar(ty) => *ty,
        ForeignType::Struct(ty) => {
            let primary = rt.root(returned.get(0));
            // SAFETY: this C result slot matches the prepared structure layout.
            unsafe { ty.write_callback_result(rt, &primary, result, at)? };
            // A pointer-valued member can refer back into the storage from
            // which this by-value result was copied.
            if ty.has_pointer_fields() {
                callbacks.retain_pointer(primary);
            }
            return Ok(());
        }
    };
    let mut strings = memory::StringBuffers::new();
    let value = Argument::from_slot(rt, returned.get(0), ty, &mut strings, at)?;
    if let Argument::Pointer(address) = value
        && !address.is_null()
    {
        let owner = if ty == CType::CString {
            let text = strings
                .pop()
                .expect("a non-null string has backing storage");
            text.pointer(rt, address, at)?
        } else {
            rt.root(returned.get(0))
        };
        callbacks.retain_pointer(owner);
    }
    // SAFETY: the result slot is sized for this prepared CIF.
    unsafe { write_result(value, result) };
    Ok(())
}

unsafe fn read_argument<'rt>(
    frame: &CallFrame<'rt, '_>,
    ty: CType,
    address: *mut c_void,
) -> Result<Root<'rt>, Fault> {
    let rt = frame.rt;
    let at = frame.at;
    // SAFETY: the trampoline receives the addresses of the declared ABI values.
    // A C stack slot can be less aligned than the corresponding Rust type.
    // Declared C strings must remain NUL-terminated.
    unsafe {
        match ty {
            CType::I8 => rt.integer_from(address.cast::<i8>().read_unaligned()),
            CType::U8 => rt.integer_from(address.cast::<u8>().read_unaligned()),
            CType::I16 => rt.integer_from(address.cast::<i16>().read_unaligned()),
            CType::U16 => rt.integer_from(address.cast::<u16>().read_unaligned()),
            CType::I32 => rt.integer_from(address.cast::<i32>().read_unaligned()),
            CType::U32 => rt.integer_from(address.cast::<u32>().read_unaligned()),
            CType::I64 => rt.integer_from(address.cast::<i64>().read_unaligned()),
            CType::U64 => rt.integer_from(address.cast::<u64>().read_unaligned()),
            CType::Float => rt.make_single_float(address.cast::<f32>().read_unaligned()),
            CType::Double => rt.make_double_float(address.cast::<f64>().read_unaligned()),
            CType::Pointer => {
                let pointer = address.cast::<*mut c_void>().read_unaligned();
                if pointer.is_null() {
                    return Ok(rt.nil());
                }
                if let Some(owner) = frame.find_pointer(pointer)? {
                    return Ok(owner);
                }
                return memory::from_raw(rt, pointer, None, at);
            }
            CType::CString => {
                let pointer = address.cast::<*mut c_void>().read_unaligned();
                if pointer.is_null() {
                    return Ok(rt.nil());
                }
                return frame.read_string(pointer);
            }
            CType::Void => unreachable!("void argument rejected at declaration"),
        }
    }
    .map_err(|error| EvalError::out_of_memory(at, error).into())
}

unsafe fn write_result(value: Argument, out: *mut c_void) {
    // MIPS N32/N64 assembly loads narrow integer results directly at the
    // start of its scratch area. Other targets read the widened argument word
    // or its low bytes. The difference is observable on big-endian MIPS64.
    const MIPS_NARROW: bool = cfg!(all(
        target_endian = "big",
        any(target_arch = "mips64", target_arch = "mips64r6")
    ));
    macro_rules! integer {
        ($n:expr) => {
            if MIPS_NARROW {
                ptr::write_unaligned(out.cast(), $n);
            } else {
                out.cast::<FfiWord>().write_unaligned($n as FfiWord);
            }
        };
    }
    unsafe {
        match value {
            Argument::I8(n) => integer!(n),
            Argument::I16(n) => integer!(n),
            Argument::I32(n) => integer!(n),
            Argument::U8(n) => integer!(n),
            Argument::U16(n) => integer!(n),
            Argument::U32(n) => integer!(n),
            Argument::I64(n) => out.cast::<i64>().write_unaligned(n),
            Argument::U64(n) => out.cast::<u64>().write_unaligned(n),
            Argument::Float(n) => out.cast::<f32>().write_unaligned(n),
            Argument::Double(n) => out.cast::<f64>().write_unaligned(n),
            Argument::Pointer(p) => {
                let extra = size_of::<FfiWord>() - size_of::<*mut c_void>();
                if extra != 0 {
                    out.cast::<FfiWord>().write_unaligned(0);
                }
                let offset = if cfg!(target_endian = "big") && !MIPS_NARROW {
                    extra
                } else {
                    0
                };
                out.byte_add(offset)
                    .cast::<*mut c_void>()
                    .write_unaligned(p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EvalErrorKind;
    use std::ffi::CStr;

    fn runtime() -> Runtime {
        // SAFETY: each test calls its own correctly declared native function pointer.
        unsafe { Runtime::builder().allow_foreign_calls() }
            .start()
            .unwrap()
    }

    fn lisp<'rt>(rt: &'rt Runtime, source: &str) -> Root<'rt> {
        rt.eval(source).unwrap().remove(0)
    }

    #[test]
    fn native_scalar_arguments_round_trip_through_identity_callbacks() {
        let rt = runtime();
        macro_rules! round_trip {
            ($tag:expr, $ty:ty, $value:expr) => {{
                let handle = lisp(
                    &rt,
                    &format!("(zk-ffi:make-callback #'identity {0} {0})", $tag),
                );
                let address = memory::address(&rt, &handle, 0).unwrap();
                // SAFETY: the declared parameter and result match this function type.
                let native: extern "C" fn($ty) -> $ty = unsafe { std::mem::transmute(address) };
                assert_eq!(
                    with_call(&rt, 0, || native($value), |value, _| Ok(value)).unwrap(),
                    $value
                );
            }};
        }
        round_trip!(":int8", i8, -127);
        round_trip!(":uint8", u8, 253);
        round_trip!(":int16", i16, -31799);
        round_trip!(":uint16", u16, 65001);
        round_trip!(":int32", i32, -2023123456);
        round_trip!(":uint32", u32, 4098765432);
        round_trip!(":int64", i64, i64::MIN + 1);
        round_trip!(":uint64", u64, u64::MAX - 1);
        round_trip!(":float", f32, -19.375);
        round_trip!(":double", f64, 1234.12345);
        round_trip!(":pointer", *mut c_void, ptr::null_mut());
        let mut byte = 0u8;
        round_trip!(
            ":pointer",
            *mut c_void,
            ptr::from_mut(&mut byte).cast::<c_void>()
        );
    }

    #[test]
    fn callbacks_with_more_than_four_parameters_receive_all_arguments_in_order() {
        let rt = runtime();
        let handle = lisp(
            &rt,
            r#"
            (zk-ffi:make-callback
              (lambda (a b c d e) (+ (* 10000 a) (* 1000 b) (* 100 c) (* 10 d) e))
              :int :int :int :int :int :int)
        "#,
        );
        let address = rt.foreign_pointer_address(&handle).unwrap();
        // SAFETY: this exactly matches the closure's declared C signature.
        let callback: extern "C" fn(i32, i32, i32, i32, i32) -> i32 =
            unsafe { std::mem::transmute(address) };
        assert_eq!(
            rt.with_foreign_callbacks(|| callback(1, 2, 3, 4, 5))
                .unwrap(),
            12345
        );
    }

    #[test]
    fn callback_cstring_results_remain_readable_during_collection() {
        let rt = runtime();
        let handle = lisp(&rt, "(zk-ffi:make-callback #'identity :cstring :cstring)");
        let address = memory::address(&rt, &handle, 0).unwrap();
        let native: extern "C" fn(*const std::ffi::c_char) -> *const std::ffi::c_char =
            unsafe { std::mem::transmute(address) };
        let text = with_call(
            &rt,
            0,
            || native(c"héllo".as_ptr()),
            |p, _| {
                rt.collect();
                Ok(unsafe { CStr::from_ptr(p) }.to_owned())
            },
        )
        .unwrap();
        assert_eq!(text.as_c_str(), c"héllo");
        assert!(
            with_call(&rt, 0, || native(ptr::null()), |value, _| Ok(value))
                .unwrap()
                .is_null()
        );
    }

    #[test]
    fn a_void_callback_runs_its_lisp_function() {
        let rt = runtime();
        rt.load_str("(defparameter *visits* 0)").unwrap();
        let handle = lisp(
            &rt,
            "(zk-ffi:make-callback (lambda () (incf *visits*)) :void)",
        );
        let native: extern "C" fn() =
            unsafe { std::mem::transmute(memory::address(&rt, &handle, 0).unwrap()) };
        with_call(&rt, 0, || native(), |value, _| Ok(value)).unwrap();
        assert_eq!(lisp(&rt, "*visits*").as_i64(), Some(1));
    }

    #[test]
    fn a_callback_error_blocks_reentry_only_in_the_failing_foreign_call() {
        let rt = runtime();
        let visits = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let counter = visits.clone();
        rt.register_function("CL-USER::CALLBACK-VISIT", move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(vec![])
        })
        .unwrap();
        let failure = lisp(
            &rt,
            "(zk-ffi:make-callback (lambda () (callback-visit) (error \"callback failed\")) :int)",
        );
        let failed: extern "C" fn() -> i32 =
            unsafe { std::mem::transmute(memory::address(&rt, &failure, 0).unwrap()) };
        let valid = lisp(&rt, "(zk-ffi:make-callback (lambda () 17) :int)");
        let ok: extern "C" fn() -> i32 =
            unsafe { std::mem::transmute(memory::address(&rt, &valid, 0).unwrap()) };
        with_call(
            &rt,
            0,
            || {
                let fault = with_call(
                    &rt,
                    0,
                    || {
                        assert_eq!(failed(), 0);
                        assert_eq!(failed(), 0);
                        assert_eq!(ok(), 0);
                        assert_eq!(visits.load(std::sync::atomic::Ordering::Relaxed), 1);
                    },
                    |(), _| -> Result<(), Fault> {
                        panic!("must inspect the pending error before converting results")
                    },
                )
                .unwrap_err();
                assert!(fault.host.to_string().contains("callback failed"));
                assert_eq!(ok(), 17);
            },
            |value, _| Ok(value),
        )
        .unwrap();
        assert_eq!(ok(), 0);
    }

    #[test]
    fn a_panic_in_a_callback_surfaces_as_a_foreign_call_error() {
        let rt = runtime();
        rt.register_function("CL-USER::PANIC-IN-CALLBACK", |_, _| {
            panic!("deliberate native panic")
        })
        .unwrap();
        let panic_handle = lisp(&rt, "(zk-ffi:make-callback #'panic-in-callback :void)");
        let panics: extern "C" fn() =
            unsafe { std::mem::transmute(memory::address(&rt, &panic_handle, 0).unwrap()) };
        let fault = with_call(&rt, 0, || panics(), |value, _| Ok(value)).unwrap_err();
        assert!(fault.host.to_string().contains("deliberate native panic"));
    }

    #[test]
    fn a_cancelled_callback_runs_again_after_reset() {
        let rt = runtime();
        let valid = lisp(&rt, "(zk-ffi:make-callback (lambda () 17) :int)");
        let ok: extern "C" fn() -> i32 =
            unsafe { std::mem::transmute(memory::address(&rt, &valid, 0).unwrap()) };
        let cancellation = rt.cancellation_handle();
        cancellation.cancel();
        let fault = with_call(&rt, 0, || ok(), |value, _| Ok(value)).unwrap_err();
        assert_eq!(fault.host.kind(), EvalErrorKind::Cancelled);
        cancellation.reset();
        assert_eq!(
            with_call(&rt, 0, || ok(), |value, _| Ok(value)).unwrap(),
            17
        );
    }

    #[test]
    fn a_c_callback_cannot_spawn_into_its_callers_group() {
        extern "C" fn call(callback: extern "C" fn() -> i32) -> i32 {
            callback()
        }

        let rt = runtime();
        let address = ptr::NonNull::new((call as *const ()).cast_mut().cast()).unwrap();
        // SAFETY: the registered C entry has static storage and accepts the
        // function pointer and result type declared below.
        unsafe { rt.register_foreign_symbol("async-test-call", address) }.unwrap();
        assert!(
            lisp(
                &rt,
                "(let ((invoke (zk-ffi:bind-function :host \"async-test-call\" :int :pointer))
                       (cleanups 0) (child-ran nil))
                   (zk-async:with-task-group (group)
                     (let* ((bad (zk-ffi:make-callback
                                   (lambda ()
                                     (unwind-protect
                                          (progn (zk-async:spawn-task (group) (setq child-ran t)) 1)
                                       (incf cleanups))) :int))
                            (good (zk-ffi:make-callback (lambda () 42) :int)))
                       (and (handler-case (progn (funcall invoke bad) nil)
                              (control-error () t))
                            (= cleanups 1)
                            (not child-ran)
                            (= (funcall invoke good) 42)
                            (zk-async:await (zk-async:spawn-task (group) t))))))",
            )
            .is_t()
        );
    }

    #[test]
    fn a_pointer_alias_remains_bound_to_the_runtime_that_owns_its_callback() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        struct Sentinel(Arc<AtomicBool>);
        impl Drop for Sentinel {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let rt = runtime();
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = dropped.clone();
        rt.register_function("CL-USER::SENTINEL", move |rt, _| {
            Ok(vec![rt.foreign(Sentinel(flag.clone()))?])
        })
        .unwrap();
        let source = "(let ((owner (sentinel)) (self nil))
                        (setq self (zk-ffi:make-callback
                                     (lambda () (if (and owner self) 42 0)) :int))
                        (zk-ffi:inc-pointer self 0))";
        let alias = lisp(&rt, source).into_persistent();
        let previous_address = ptr::from_ref(&rt).addr();
        let rt = Box::new(rt);
        assert_ne!(previous_address, ptr::from_ref(rt.as_ref()).addr());
        rt.collect();
        let handle = alias.bind(&rt).unwrap();
        let code = memory::address(&rt, &handle, 0).unwrap();
        let native: extern "C" fn() -> i32 = unsafe { std::mem::transmute(code) };
        assert_eq!(
            with_call(&rt, 0, || native(), |value, _| Ok(value)).unwrap(),
            42
        );
        // No runtime state or Rc may be consulted from an unregistered worker.
        thread::scope(|scope| {
            let worker = scope.spawn(move || {
                assert_eq!(native(), 0);
                let different = runtime();
                let fault =
                    with_call(&different, 0, || native(), |value, _| Ok(value)).unwrap_err();
                assert!(fault.host.to_string().contains("different thread"));
            });
            worker.join().unwrap();
        });
        let other = runtime();
        let mismatch = with_call(&other, 0, || native(), |value, _| Ok(value)).unwrap_err();
        assert!(mismatch.host.to_string().contains("different runtime"));
        drop(handle);
        drop(alias);
        rt.collect();
        assert!(
            dropped.load(Ordering::Relaxed),
            "a closure capturing its own pointer must be collectible"
        );

        dropped.store(false, Ordering::Relaxed);
        let live = lisp(&rt, source).into_persistent();
        drop(rt);
        assert!(
            dropped.load(Ordering::Relaxed),
            "runtime teardown releases a still-rooted callback"
        );
        assert!(live.bind(&other).is_none());
    }

    #[test]
    fn active_callback_survives_last_lisp_handle_being_released() {
        let rt = runtime();
        rt.register_function("CL-USER::COLLECT-NOW", |rt, _| {
            rt.collect();
            Ok(vec![])
        })
        .unwrap();
        let handle = lisp(
            &rt,
            "(defparameter *self-releasing*
            (zk-ffi:make-callback (let ((n 0))
              (lambda () (setq *self-releasing* nil) (collect-now) (incf n))) :int))",
        );
        drop(handle);
        let address = memory::address(&rt, &lisp(&rt, "*self-releasing*"), 0).unwrap();
        let native: extern "C" fn() -> i32 = unsafe { std::mem::transmute(address) };
        with_call(
            &rt,
            0,
            || {
                assert_eq!(native(), 1);
                rt.collect();
                assert_eq!(native(), 2);
            },
            |value, _| Ok(value),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn libc_qsort_calls_a_lisp_comparator_that_reenters_another_callback() {
        let rt = runtime();
        rt.register_function("CL-USER::COLLECT-NOW", |rt, _| {
            rt.collect();
            Ok(vec![])
        })
        .unwrap();
        let mut data = [19i32, -7, 21, 0, -9, 21];
        let qsort = libc::qsort as *const () as usize;
        let base = data.as_mut_ptr().addr();
        rt.load_str(&format!(
            "
            (let* ((sorter (zk-ffi:bind-pointer (zk-ffi:make-pointer {qsort})
                             :void :pointer :size :size :pointer))
                   (sign (zk-ffi:bind-pointer (zk-ffi:make-callback #'signum :int :int) :int :int))
                   (cmp (zk-ffi:make-callback
                          (lambda (a b)
                            (collect-now)
                            (funcall sign (- (zk-ffi:mem-ref a :int) (zk-ffi:mem-ref b :int))))
                          :int :pointer :pointer)))
              (funcall sorter (zk-ffi:make-pointer {base}) 6 4 cmp))"
        ))
        .unwrap();
        assert_eq!(data, [-9, -7, 0, 19, 21, 21]);
    }

    #[test]
    fn calling_a_bound_callback_resignals_the_original_condition() {
        let rt = runtime();
        rt.load_str(
            "(define-condition callback-test-error (error)
                        ((payload :initarg :payload :reader callback-test-payload)))",
        )
        .unwrap();
        assert!(lisp(&rt, "(let* ((condition (make-condition 'callback-test-error :payload '(42)))
                                  (target (zk-ffi:make-callback (lambda () (error condition)) :int))
                                  (invoke (zk-ffi:bind-pointer target :int)))
                            (handler-case (funcall invoke)
                              (callback-test-error (seen)
                                (and (eq seen condition) (= 42 (first (callback-test-payload seen)))))))").is_t());
    }

    #[test]
    fn pointers_returned_from_callbacks_survive_collection() {
        let rt = runtime();
        rt.register_function("CL-USER::COLLECT-NOW", |rt, _| {
            rt.collect();
            Ok(vec![])
        })
        .unwrap();
        let result = lisp(
            &rt,
            "
            (flet ((invoke (fn declared-type)
                     (funcall (zk-ffi:bind-pointer (zk-ffi:make-callback fn declared-type) :pointer))))
              (let ((memory (invoke (lambda () (let ((p (zk-ffi:allocate 4)))
                                               (setf (zk-ffi:mem-ref p :int) -42) p)) :pointer))
                    (function (invoke (lambda () (zk-ffi:make-callback (lambda () 99) :int)) :pointer))
                    (text (invoke (lambda () (copy-seq \"owned\")) :cstring)))
                (collect-now)
                (unwind-protect
                  (and (= -42 (zk-ffi:mem-ref memory :int))
                       (= 99 (funcall (zk-ffi:bind-pointer function :int)))
                       (string= \"owned\" (zk-ffi:read-c-string text)))
                  (zk-ffi:free memory))))",
        );
        assert!(result.is_t());
    }

    #[test]
    fn callback_marshalling_accepts_unaligned_abi_slots() {
        let rt = runtime();
        let frame = CallFrame {
            rt: &rt,
            at: 0,
            previous: ptr::null(),
            inputs: None,
            callbacks: OnceCell::new(),
        };
        #[repr(align(8))]
        struct Scratch([u8; 12]);
        let mut storage = Scratch([0x7e; 12]);
        let slot = unsafe { storage.0.as_mut_ptr().add(1) }.cast::<c_void>();
        unsafe { write_result(Argument::Double(-19.25), slot) };
        assert_eq!(storage.0[0], 0x7e);
        assert_eq!(&storage.0[9..], &[0x7e; 3]);
        let input = unsafe { read_argument(&frame, CType::Double, slot) }.unwrap();
        assert_eq!(input.as_double_float(), Some(-19.25));
        storage.0.fill(0x7e);
        unsafe { write_result(Argument::Float(3.125), slot) };
        assert_eq!(storage.0[0], 0x7e);
        assert_eq!(&storage.0[5..], &[0x7e; 7]);
        let input = unsafe { read_argument(&frame, CType::Float, slot) }.unwrap();
        assert_eq!(input.as_single_float(), Some(3.125));
    }

    #[test]
    fn native_code_can_forward_an_owned_pointer_between_callbacks() {
        extern "C" fn forward(
            produce: extern "C" fn() -> *mut c_void,
            consume: extern "C" fn(*mut c_void) -> *mut c_void,
        ) -> *mut c_void {
            consume(produce())
        }
        let rt = runtime();
        let function = forward as *const () as usize;
        rt.load_str("(defparameter *forwarded* nil)").unwrap();
        let result = lisp(&rt, &format!("
            (let ((call (zk-ffi:bind-pointer (zk-ffi:make-pointer {function}) :pointer :pointer :pointer))
                  (produce (zk-ffi:make-callback (lambda () (zk-ffi:allocate 8)) :pointer))
                  (consume (zk-ffi:make-callback (lambda (pointer) (setq *forwarded* pointer))
                                                 :pointer :pointer)))
              (funcall call produce consume))"));
        drop(result);
        rt.collect();
        assert_eq!(
            lisp(
                &rt,
                "(unwind-protect
                                (setf (zk-ffi:mem-ref *forwarded* :int) 17)
                                (zk-ffi:free *forwarded*))"
            )
            .as_i64(),
            Some(17)
        );
    }

    #[test]
    fn callback_can_retain_a_native_argument_from_an_outer_scope() {
        thread_local! {
            static STORED: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
        }
        extern "C" fn outer(callback: extern "C" fn(), pointer: *mut c_void) {
            let previous = STORED.replace(pointer);
            callback();
            STORED.set(previous);
        }
        extern "C" fn inner(callback: extern "C" fn(*mut c_void)) {
            callback(STORED.get());
        }
        let rt = runtime();
        for (name, address) in [("outer", outer as *const ()), ("inner", inner as *const ())] {
            // SAFETY: both functions have static storage and are declared below.
            unsafe {
                rt.register_foreign_symbol(
                    name,
                    ptr::NonNull::new(address.cast_mut().cast()).unwrap(),
                )
            }
            .unwrap();
        }
        rt.load_str(
            r#"
            (defparameter *saved* nil)
            (defparameter *consumer*
              (zk-ffi:make-callback (lambda (p) (setf *saved* p)) :void :pointer))
            (defparameter *bridge*
              (let ((inner (zk-ffi:bind-function :host "inner" :void :pointer)))
                (zk-ffi:make-callback (lambda () (funcall inner *consumer*)) :void)))
            (let ((outer (zk-ffi:bind-function :host "outer" :void :pointer :pointer))
                  (buffer (zk-ffi:allocate 8)))
              (funcall outer *bridge* buffer)
              (assert (eq *saved* buffer))
              (zk-ffi:free buffer)
              (assert (handler-case (progn (zk-ffi:pointer-address *saved*) nil)
                        (error () t))))
            (funcall (zk-ffi:bind-function :host "outer" :void :pointer :cstring)
                     *bridge* "雪")
        "#,
        )
        .unwrap();
        rt.collect();
        assert_eq!(
            lisp(&rt, "(zk-ffi:read-c-string *saved*)")
                .as_string(&rt)
                .as_deref(),
            Some("雪")
        );
    }

    #[test]
    fn released_managed_storage_cannot_be_used_as_a_prepared_call_target() {
        let rt = runtime();
        let error = rt
            .eval(
                r#"
            (let* ((storage (zk-ffi:allocate 8))
                   (call (zk-ffi:bind-pointer storage :int)))
              (zk-ffi:free storage)
              (funcall call))
        "#,
            )
            .err()
            .expect("call must fail before entering native code");
        assert!(error.to_string().contains("has been freed"));
    }

    #[test]
    fn native_scope_propagates_an_error_from_an_entry_finalizer() {
        let rt = runtime();
        rt.load_str(r#"(zk:finalize (list 1) (lambda () (error "entry finalizer")))"#)
            .unwrap();
        rt.collect();
        let ran = Cell::new(false);
        let error = rt.with_foreign_callbacks(|| ran.set(true)).unwrap_err();
        assert!(error.to_string().contains("entry finalizer"));
        assert!(!ran.get());
        rt.with_foreign_callbacks(|| ran.set(true)).unwrap();
        assert!(ran.get());
    }
}
