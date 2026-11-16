use std::any::TypeId;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::mem::DropGuard;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use zkcl_target::CompilationTarget;

use crate::heap::{DomainSafepoint, SafepointCoordinator, Word};
use crate::host::{HostCompletion, HostError, HostServices};
use crate::{
    AlreadyStarted, CancellationHandle, EvalError, HeapStats, NativeResult, PersistentRoot, Root,
    Runtime, RuntimeBuilder, ScopedHostBinding, ScopedHostValue,
};

struct SendRuntime(Runtime);

// SAFETY: `Vm` owns the sole `Runtime` behind `Mutex<SendRuntime>`. All thread-local globals
// have been removed, host callbacks/services and `PersistentRoot` are `Send + Sync`, and any
// `!Send` scoped host borrows remain confined to the active `Domain` stack frame and are swapped
// back out via `DropGuard` before unlocking or returning to native state.
unsafe impl Send for SendRuntime {}

/// Builder for configuring and starting a thread-safe [`Vm`].
#[derive(Default)]
pub struct VmBuilder {
    runtime: RuntimeBuilder,
    host_services: Option<Box<dyn HostServices>>,
}

impl VmBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn compilation_target(mut self, target: CompilationTarget) -> Self {
        self.runtime = self.runtime.compilation_target(target);
        self
    }

    pub fn heap_size_bytes(mut self, bytes: usize) -> Self {
        self.runtime = self.runtime.heap_size_bytes(bytes);
        self
    }

    pub fn jit_threshold(mut self, threshold: u32) -> Self {
        self.runtime = self.runtime.jit_threshold(threshold);
        self
    }

    pub fn step_budget(mut self, budget: Option<u64>) -> Self {
        self.runtime = self.runtime.step_budget(budget);
        self
    }

    pub fn cancellation(mut self, handle: CancellationHandle) -> Self {
        self.runtime = self.runtime.cancellation(handle);
        self
    }

    pub fn host_services(mut self, services: impl HostServices + 'static) -> Self {
        self.host_services = Some(Box::new(services));
        self
    }

    pub fn start(self) -> Result<Arc<Vm>, AlreadyStarted> {
        let mut runtime = self.runtime.start()?;
        if let Some(services) = self.host_services {
            runtime.host_services = services;
        }
        runtime.heap.detach_main_safepoint();
        let safepoints = Arc::clone(runtime.heap.safepoint_coordinator());
        let cancellation = runtime.cancellation_handle();
        Ok(Arc::new(Vm {
            safepoints,
            cancellation,
            runtime: Mutex::new(SendRuntime(runtime)),
            next_domain_id: AtomicU64::new(1),
        }))
    }
}

/// Shared Common Lisp virtual machine (`Send + Sync`), serving as the single Lisp world
/// across one or many attached OS execution [`Domain`]s.
pub struct Vm {
    safepoints: Arc<SafepointCoordinator>,
    cancellation: CancellationHandle,
    runtime: Mutex<SendRuntime>,
    next_domain_id: AtomicU64,
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Vm>();
    assert_send_sync::<Arc<Vm>>();
    assert_send_sync::<PersistentRoot>();
};

impl Vm {
    pub fn start() -> Result<Arc<Self>, AlreadyStarted> {
        VmBuilder::new().start()
    }

    pub fn builder() -> VmBuilder {
        VmBuilder::new()
    }

    /// Attach the calling OS thread as a Lisp execution [`Domain`].
    /// Newly attached domains start in the `InNative` safepoint state until entering Lisp.
    pub fn attach_current_thread(self: &Arc<Self>) -> Domain {
        let id = self.next_domain_id.fetch_add(1, Ordering::Relaxed);
        self.attach_with_id(id, format!("zkcl-domain-{id}"))
    }

    /// Attach the calling OS thread with a human-readable domain name.
    pub fn attach_named_thread(self: &Arc<Self>, name: impl Into<String>) -> Domain {
        let id = self.next_domain_id.fetch_add(1, Ordering::Relaxed);
        self.attach_with_id(id, name.into())
    }

    fn attach_with_id(self: &Arc<Self>, id: u64, name: String) -> Domain {
        let safepoint = self.safepoints.register_native_domain();
        Domain {
            vm: Arc::clone(self),
            safepoint,
            id,
            name,
            tls_values: RefCell::new(vec![Word::NO_TLS_VALUE]),
            host_scopes: RefCell::new(Vec::new()),
            _not_send: PhantomData,
        }
    }

    pub fn register_function<F>(&self, name: &str, function: F) -> Result<(), EvalError>
    where
        F: for<'rt> Fn(&'rt Runtime, &[Root<'rt>]) -> NativeResult<'rt> + Send + Sync + 'static,
    {
        self.runtime
            .lock()
            .unwrap()
            .0
            .register_function(name, function)
    }

    pub fn register_async_function<F>(&self, name: &str, start: F) -> Result<(), EvalError>
    where
        F: for<'rt> Fn(&'rt Runtime, &[Root<'rt>], HostCompletion) -> Result<(), HostError>
            + Send
            + Sync
            + 'static,
    {
        self.runtime
            .lock()
            .unwrap()
            .0
            .register_async_function(name, start)
    }

    pub fn collect(&self) {
        self.runtime.lock().unwrap().0.collect();
    }

    pub fn heap_stats(&self) -> HeapStats {
        self.runtime.lock().unwrap().0.heap_stats()
    }

    pub fn cancellation_handle(&self) -> CancellationHandle {
        self.cancellation.clone()
    }

    pub fn active_domain_count(&self) -> usize {
        self.safepoints.registered_domain_count()
    }
}

/// Thread-affine (`!Send + !Sync`) execution context bound to a single OS thread.
/// Holds per-domain special-variable TLS state, safepoint registration, and thread-local
/// host scope borrows (`with_host_ref` / `with_host_mut`).
pub struct Domain {
    vm: Arc<Vm>,
    safepoint: Arc<DomainSafepoint>,
    id: u64,
    name: String,
    tls_values: RefCell<Vec<Word>>,
    host_scopes: RefCell<Vec<ScopedHostBinding>>,
    _not_send: PhantomData<*const ()>,
}

impl Drop for Domain {
    fn drop(&mut self) {
        self.vm.safepoints.unregister_domain(&self.safepoint);
    }
}

impl Domain {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn vm(&self) -> &Arc<Vm> {
        &self.vm
    }

    fn with_locked_runtime<R>(&self, f: impl FnOnce(&Runtime) -> R) -> R {
        // Acquire the VM runtime lock while in `InNative` so we never block an active
        // stop-the-world coordinator, then transition into `InLisp` for execution.
        let guard = self.vm.runtime.lock().unwrap();
        guard
            .0
            .heap
            .swap_tls_values(&mut self.tls_values.borrow_mut());
        std::mem::swap(
            &mut *guard.0.host_scopes.borrow_mut(),
            &mut *self.host_scopes.borrow_mut(),
        );
        let prev_safepoint = guard
            .0
            .heap
            .replace_active_safepoint(Some(Arc::clone(&self.safepoint)));
        self.safepoint.leave_native();
        let _restore = DropGuard::new((), |_| {
            self.safepoint.enter_native();
            guard.0.heap.replace_active_safepoint(prev_safepoint);
            std::mem::swap(
                &mut *guard.0.host_scopes.borrow_mut(),
                &mut *self.host_scopes.borrow_mut(),
            );
            guard
                .0
                .heap
                .swap_tls_values(&mut self.tls_values.borrow_mut());
        });
        f(&guard.0)
    }

    /// Execute an arbitrary operation with temporary access to the underlying [`Runtime`].
    pub fn with_runtime<R>(&self, f: impl FnOnce(&Runtime) -> R) -> R {
        self.with_locked_runtime(f)
    }

    /// Evaluate Lisp source in this domain and return persistent handles owned by its VM.
    pub fn eval(&self, source: &str) -> Result<Vec<PersistentRoot>, EvalError> {
        self.with_locked_runtime(|runtime| {
            Ok(runtime
                .eval(source)?
                .into_iter()
                .map(Root::into_persistent)
                .collect())
        })
    }

    /// Load and evaluate Lisp forms from `source` inside this domain.
    pub fn load_str(&self, source: &str) -> Result<(), EvalError> {
        self.with_locked_runtime(|runtime| runtime.load_str(source))
    }

    /// Format a [`PersistentRoot`] using the Lisp printer.
    pub fn repr(&self, value: &PersistentRoot) -> Result<String, EvalError> {
        self.with_locked_runtime(|runtime| {
            let bound = value
                .bind(runtime)
                .ok_or_else(|| EvalError::new(0, "persistent root does not belong to this VM"))?;
            runtime.repr(&bound)
        })
    }

    /// Trigger stop-the-world garbage collection on the shared VM heap.
    pub fn collect(&self) {
        self.with_locked_runtime(|runtime| runtime.collect());
    }

    /// Expose a thread-affine `&mut T` (including `!Send` types) to callbacks invoked
    /// during `f` on this domain's thread.
    pub fn with_host_mut<T: 'static, R>(&self, host: &mut T, f: impl FnOnce(&Domain) -> R) -> R {
        self.push_host_scope(ScopedHostValue::Mutable(RefCell::new(host)), f)
    }

    /// Expose a thread-affine `&T` (including `!Send` types) to callbacks invoked
    /// during `f` on this domain's thread.
    pub fn with_host_ref<T: 'static, R>(&self, host: &T, f: impl FnOnce(&Domain) -> R) -> R {
        self.push_host_scope(ScopedHostValue::Shared(host), f)
    }

    fn push_host_scope<T: 'static, R>(
        &self,
        value: ScopedHostValue<'_, T>,
        f: impl FnOnce(&Domain) -> R,
    ) -> R {
        self.host_scopes.borrow_mut().push(ScopedHostBinding {
            type_id: TypeId::of::<T>(),
            value: std::ptr::from_ref(&value).cast(),
        });
        let _guard = DropGuard::new(&self.host_scopes, |scopes| {
            scopes.borrow_mut().pop();
        });
        f(self)
    }
}
