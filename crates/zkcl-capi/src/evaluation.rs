//! Owning-thread evaluation handles and thread-safe host notifications.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::Arc;
use std::task::{Context, Wake, Waker};

use smallvec::SmallVec;
use zkcl::{Evaluation, EvaluationPoll, HostCompletion, PersistentRoot, Runtime};

use super::abi::{
    Error, Utf8View, boundary, free_owned_handle, require_pointer, write_owned_handle,
};
use super::{CallbackReturns, Values, bind_call_arguments, bind_value_to_runtime};

struct EvaluationHandle<'rt>(RefCell<Evaluation<'rt>>);

struct DebugEvent {
    condition: PersistentRoot,
    frames: super::tooling::DebugFrames,
    message: String,
    condition_type: String,
}

#[repr(u32)]
enum EvaluationState {
    Ready = 0,
    Runnable = 1,
    Waiting = 2,
    WaitingTimer = 3,
    Paused = 4,
}

#[repr(C)]
struct EvaluationResult {
    state: EvaluationState,
    delay_ns: u64,
    values: *mut Values,
    debug_event: *mut DebugEvent,
}

impl Default for EvaluationResult {
    fn default() -> Self {
        Self {
            state: EvaluationState::Ready,
            delay_ns: 0,
            values: ptr::null_mut(),
            debug_event: ptr::null_mut(),
        }
    }
}

// C keeps the owning runtime alive until it releases the evaluation handle.
#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_start_eval<'rt>(
    rt: Option<&'rt Runtime>,
    source: Utf8View,
    out: Option<&mut *mut EvaluationHandle<'rt>>,
) -> *mut Error {
    write_owned_handle(out, || {
        let invocation = require_pointer(rt)?.start_eval(unsafe { source.read_utf8()? })?;
        Ok(EvaluationHandle(RefCell::new(invocation)))
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_start_eval_document<'rt>(
    rt: Option<&'rt Runtime>,
    source: Option<&zkcl::SourceDocument>,
    out: Option<&mut *mut EvaluationHandle<'rt>>,
) -> *mut Error {
    write_owned_handle(out, || {
        Ok(EvaluationHandle(RefCell::new(
            require_pointer(rt)?.start_eval_document(require_pointer(source)?)?,
        )))
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_start_call<'rt>(
    rt: Option<&'rt Runtime>,
    target: Option<&PersistentRoot>,
    argv: *const *const PersistentRoot,
    argc: usize,
    out: Option<&mut *mut EvaluationHandle<'rt>>,
) -> *mut Error {
    write_owned_handle(out, || {
        let rt = require_pointer(rt)?;
        let function = bind_value_to_runtime(rt, target)?;
        let args = unsafe { bind_call_arguments(rt, argv, argc)? };
        let args = args.iter().collect::<SmallVec<[_; 4]>>();
        Ok(EvaluationHandle(RefCell::new(
            rt.start_call(&function, &args)?,
        )))
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_evaluation_resume(
    evaluation: Option<&EvaluationHandle<'_>>,
    waker: Option<&Waker>,
    out: Option<&mut MaybeUninit<EvaluationResult>>,
) -> *mut Error {
    boundary(|| {
        let out = require_pointer(out)?.write(EvaluationResult::default());
        let mut evaluation = require_pointer(evaluation)?
            .0
            .try_borrow_mut()
            .map_err(|_| Error::invalid("evaluation is already running"))?;
        match evaluation.resume(waker)? {
            EvaluationPoll::Ready(values) => {
                out.values = Box::into_raw(Box::new(Values::from_roots(values)));
            }
            EvaluationPoll::Runnable => out.state = EvaluationState::Runnable,
            EvaluationPoll::Waiting(None) => out.state = EvaluationState::Waiting,
            EvaluationPoll::Waiting(Some(delay)) => {
                out.state = EvaluationState::WaitingTimer;
                out.delay_ns = u64::try_from(delay.as_nanos()).unwrap_or(u64::MAX);
            }
            EvaluationPoll::Paused(event) => {
                out.state = EvaluationState::Paused;
                out.debug_event = Box::into_raw(Box::new(DebugEvent {
                    condition: event.condition.into_persistent(),
                    frames: super::tooling::DebugFrames(event.frames),
                    message: event.message,
                    condition_type: event.condition_type,
                }));
            }
        }
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_evaluation_enable_debugging(
    evaluation: Option<&EvaluationHandle<'_>>,
) -> *mut Error {
    boundary(|| {
        require_pointer(evaluation)?
            .0
            .try_borrow_mut()
            .map_err(|_| Error::invalid("evaluation is already running"))?
            .enable_debugging();
        Ok(())
    })
}

type DebugOperation = unsafe extern "C" fn(&Runtime, *mut c_void) -> *mut Error;

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_evaluation_with_debugger(
    evaluation: Option<&EvaluationHandle<'_>>,
    callback: Option<DebugOperation>,
    userdata: *mut c_void,
) -> *mut Error {
    boundary(|| {
        let callback = require_pointer(callback)?;
        let mut evaluation = require_pointer(evaluation)?
            .0
            .try_borrow_mut()
            .map_err(|_| Error::invalid("evaluation is already running"))?;
        let result = evaluation.with_debugger(|rt| {
            // Runtime lends only a shared reference. No Evaluation reference is
            // passed to C; reentry is rejected by the outstanding RefCell borrow.
            let error = unsafe { callback(rt, userdata) };
            if error.is_null() {
                Ok(())
            } else {
                // C transfers the error, including internal control transfers.
                // Keep it owned even if with_debugger consumes that transfer.
                Err(*unsafe { Box::from_raw(error) })
            }
        })?;
        result.unwrap_or(Ok(()))
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_event_condition(event: Option<&DebugEvent>) -> *const PersistentRoot {
    event.map_or(ptr::null(), |event| ptr::from_ref(&event.condition))
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_event_frames(
    event: Option<&DebugEvent>,
) -> *const super::tooling::DebugFrames {
    event.map_or(ptr::null(), |event| ptr::from_ref(&event.frames))
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_event_message(event: Option<&DebugEvent>) -> Utf8View {
    Utf8View::from_str(event.map_or("", |event| &event.message))
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_debug_event_condition_type(event: Option<&DebugEvent>) -> Utf8View {
    Utf8View::from_str(event.map_or("", |event| &event.condition_type))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_debug_event_free(event: *mut DebugEvent) {
    unsafe { free_owned_handle(event) }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_evaluation_free(evaluation: *mut EvaluationHandle<'_>) {
    unsafe { free_owned_handle(evaluation) }
}

type Notify = unsafe extern "C" fn(*mut c_void);
type Release = unsafe extern "C" fn(*mut c_void);

struct Userdata {
    pointer: *mut c_void,
    release: Option<Release>,
}

// The C caller transfers owned userdata whose release may run on any thread.
unsafe impl Send for Userdata {}

impl Userdata {
    fn pointer(&self) -> *mut c_void {
        self.pointer
    }
}

impl Drop for Userdata {
    fn drop(&mut self) {
        if let Some(release) = self.release {
            // SAFETY: the caller keeps the transferred userdata valid until release.
            unsafe { release(self.pointer) };
        }
    }
}

struct WakeCallback {
    notify: Notify,
    data: Userdata,
}

// The waker's C contract permits concurrent notifications. Arc keeps the
// callback data alive until all notification attempts have finished.
unsafe impl Sync for WakeCallback {}

impl Wake for WakeCallback {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // SAFETY: the caller supplies a notification callable from any thread.
        unsafe { (self.notify)(self.data.pointer()) };
    }
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_waker_new(
    notify: Option<Notify>,
    userdata: *mut c_void,
    release: Option<Release>,
    out: Option<&mut *mut Waker>,
) -> *mut Error {
    write_owned_handle(out, || {
        let notify = require_pointer(notify)?;
        let data = Userdata {
            pointer: userdata,
            release,
        };
        Ok(Waker::from(Arc::new(WakeCallback { notify, data })))
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_waker_free(waker: *mut Waker) {
    unsafe { free_owned_handle(waker) }
}

type Start =
    unsafe extern "C" fn(&Runtime, &Values, *mut HostCompletion, *mut c_void) -> *mut Error;
type Materialize =
    unsafe extern "C" fn(&Runtime, *mut CallbackReturns<'_>, *mut c_void) -> *mut Error;

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_register_async_function(
    rt: Option<&Runtime>,
    name: Utf8View,
    start: Option<Start>,
    userdata: *mut c_void,
) -> *mut Error {
    boundary(|| {
        let start = require_pointer(start)?;
        let userdata = super::CUserdata::new(userdata);
        require_pointer(rt)?.register_async_function(
            unsafe { name.read_utf8()? },
            move |rt, args, completion| {
                let args = Values::from_roots(args.iter().cloned());
                let completion = Box::into_raw(Box::new(completion));
                // SAFETY: start borrows the arguments during the call and owns the completion.
                let error = unsafe { start(rt, &args, completion, userdata.as_ptr()) };
                unsafe { crate::abi::check_foreign_error(error) }
            },
        )?;
        Ok(())
    })
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_completion_is_cancelled(completion: Option<&HostCompletion>) -> bool {
    completion.is_some_and(HostCompletion::is_cancelled)
}

#[unsafe(no_mangle)]
extern "C" fn zkcl_completion_poll_cancelled(
    completion: Option<&HostCompletion>,
    waker: Option<&Waker>,
) -> bool {
    match (completion, waker) {
        (Some(completion), Some(waker)) => completion
            .poll_cancelled(&mut Context::from_waker(waker))
            .is_ready(),
        (Some(completion), None) => completion.is_cancelled(),
        (None, _) => false,
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_completion_finish(
    completion: *mut HostCompletion,
    materialize: Option<Materialize>,
    userdata: *mut c_void,
    release: Option<Release>,
) -> bool {
    let data = Userdata {
        pointer: userdata,
        release,
    };
    if completion.is_null() {
        return false;
    }
    let completion = unsafe { Box::from_raw(completion) };
    completion.complete(move |rt| {
        let mut results = CallbackReturns {
            runtime: rt,
            roots: Vec::new(),
        };
        if let Some(materialize) = materialize {
            // SAFETY: the completion retains userdata and invokes this callback on rt's thread.
            let error = unsafe { materialize(rt, &raw mut results, data.pointer()) };
            unsafe { crate::abi::check_foreign_error(error)? };
        }
        Ok(results.roots)
    })
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_completion_free(completion: *mut HostCompletion) {
    unsafe { free_owned_handle(completion) }
}
