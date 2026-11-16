use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Execution state of a domain visible to the safepoint coordinator.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DomainState {
    InLisp = 0,
    InNative = 1,
    Parked = 2,
}

#[derive(Default)]
struct CoordinatorState {
    stw_active: bool,
    domains: Vec<Arc<DomainSafepoint>>,
}

/// Coordinates stop-the-world pauses across active Lisp domains using a 3-state
/// (`InLisp` / `InNative` / `Parked`) safepoint protocol.
pub(crate) struct SafepointCoordinator {
    gc_requested: AtomicBool,
    state: Mutex<CoordinatorState>,
    coordinator_cv: Condvar,
    resume_cv: Condvar,
}

impl Default for SafepointCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-domain safepoint handle registered with a [`SafepointCoordinator`].
pub(crate) struct DomainSafepoint {
    state: AtomicU8,
    coordinator: Arc<SafepointCoordinator>,
}

struct StwReleaseGuard<'a> {
    coordinator: &'a SafepointCoordinator,
}

impl Drop for StwReleaseGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.coordinator.state.lock().unwrap();
        self.coordinator
            .gc_requested
            .store(false, Ordering::Release);
        state.stw_active = false;
        self.coordinator.resume_cv.notify_all();
    }
}

impl SafepointCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            gc_requested: AtomicBool::new(false),
            state: Mutex::new(CoordinatorState::default()),
            coordinator_cv: Condvar::new(),
            resume_cv: Condvar::new(),
        }
    }

    pub(crate) fn register_domain(self: &Arc<Self>) -> Arc<DomainSafepoint> {
        self.register_domain_with_state(DomainState::InLisp)
    }

    pub(crate) fn register_native_domain(self: &Arc<Self>) -> Arc<DomainSafepoint> {
        self.register_domain_with_state(DomainState::InNative)
    }

    fn register_domain_with_state(self: &Arc<Self>, initial: DomainState) -> Arc<DomainSafepoint> {
        let mut state = self.state.lock().unwrap();
        while state.stw_active {
            state = self.resume_cv.wait(state).unwrap();
        }
        let domain = Arc::new(DomainSafepoint {
            state: AtomicU8::new(initial as u8),
            coordinator: Arc::clone(self),
        });
        state.domains.push(Arc::clone(&domain));
        domain
    }

    pub(crate) fn unregister_domain(&self, domain: &Arc<DomainSafepoint>) {
        let mut state = self.state.lock().unwrap();
        state.domains.retain(|entry| !Arc::ptr_eq(entry, domain));
        self.coordinator_cv.notify_all();
    }

    #[inline(always)]
    pub(crate) fn is_gc_requested(&self) -> bool {
        self.gc_requested.load(Ordering::Relaxed)
    }

    pub(crate) fn registered_domain_count(&self) -> usize {
        self.state.lock().unwrap().domains.len()
    }

    pub(crate) fn stop_the_world<R>(
        &self,
        requester: Option<&DomainSafepoint>,
        f: impl FnOnce() -> R,
    ) -> R {
        let mut state = self.state.lock().unwrap();
        while state.stw_active {
            if let Some(domain) = requester {
                domain
                    .state
                    .store(DomainState::Parked as u8, Ordering::SeqCst);
                self.coordinator_cv.notify_all();
            }
            state = self.resume_cv.wait(state).unwrap();
            if let Some(domain) = requester {
                domain
                    .state
                    .store(DomainState::InLisp as u8, Ordering::SeqCst);
            }
        }

        state.stw_active = true;
        self.gc_requested.store(true, Ordering::SeqCst);

        while !Self::all_other_domains_safe(&state.domains, requester) {
            state = self.coordinator_cv.wait(state).unwrap();
        }
        drop(state);

        let _release = StwReleaseGuard { coordinator: self };
        f()
    }

    fn all_other_domains_safe(
        domains: &[Arc<DomainSafepoint>],
        requester: Option<&DomainSafepoint>,
    ) -> bool {
        domains.iter().all(|domain| {
            requester.is_some_and(|req| std::ptr::eq(Arc::as_ptr(domain), req))
                || domain.state.load(Ordering::SeqCst) != DomainState::InLisp as u8
        })
    }
}

impl DomainSafepoint {
    #[cfg(test)]
    #[inline(always)]
    pub(crate) fn poll(&self) {
        if !self.coordinator.gc_requested.load(Ordering::Relaxed) {
            return;
        }
        self.park_for_safepoint();
    }

    #[cold]
    pub(crate) fn park_for_safepoint(&self) {
        let mut state = self.coordinator.state.lock().unwrap();
        if !state.stw_active {
            self.state.store(DomainState::InLisp as u8, Ordering::SeqCst);
            return;
        }
        self.state.store(DomainState::Parked as u8, Ordering::SeqCst);
        self.coordinator.coordinator_cv.notify_all();
        while state.stw_active {
            state = self.coordinator.resume_cv.wait(state).unwrap();
        }
        self.state.store(DomainState::InLisp as u8, Ordering::SeqCst);
    }

    pub(crate) fn enter_native(&self) {
        self.state
            .store(DomainState::InNative as u8, Ordering::SeqCst);
        if self.coordinator.gc_requested.load(Ordering::SeqCst) {
            let _state = self.coordinator.state.lock().unwrap();
            self.coordinator.coordinator_cv.notify_all();
        }
    }

    pub(crate) fn leave_native(&self) {
        self.state.store(DomainState::InLisp as u8, Ordering::SeqCst);
        if self.coordinator.gc_requested.load(Ordering::SeqCst) {
            self.park_for_safepoint();
        }
    }

    #[cfg(test)]
    pub(crate) fn current_state(&self) -> DomainState {
        match self.state.load(Ordering::SeqCst) {
            0 => DomainState::InLisp,
            1 => DomainState::InNative,
            2 => DomainState::Parked,
            other => panic!("unexpected domain state {other}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::{DomainState, SafepointCoordinator};

    #[test]
    fn stop_the_world_parks_in_lisp_domains_and_resumes_all() {
        let coordinator = Arc::new(SafepointCoordinator::new());
        let domain_a = coordinator.register_domain();
        let domain_b = coordinator.register_domain();
        let domain_c = coordinator.register_domain();

        let stop = Arc::new(AtomicBool::new(false));
        let stw_done = Arc::new(AtomicBool::new(false));
        let start = Arc::new(Barrier::new(3));

        let spawn_worker = |domain: Arc<super::DomainSafepoint>,
                            start: Arc<Barrier>,
                            stop: Arc<AtomicBool>| {
            thread::spawn(move || {
                start.wait();
                while !stop.load(Ordering::Relaxed) {
                    domain.poll();
                    thread::yield_now();
                }
            })
        };

        let w_b = spawn_worker(Arc::clone(&domain_b), Arc::clone(&start), Arc::clone(&stop));
        let w_c = spawn_worker(Arc::clone(&domain_c), Arc::clone(&start), Arc::clone(&stop));
        start.wait();

        coordinator.stop_the_world(Some(&domain_a), || {
            assert_eq!(domain_a.current_state(), DomainState::InLisp);
            assert_eq!(domain_b.current_state(), DomainState::Parked);
            assert_eq!(domain_c.current_state(), DomainState::Parked);
            stw_done.store(true, Ordering::Release);
        });
        assert!(stw_done.load(Ordering::Acquire));

        stop.store(true, Ordering::Release);
        w_b.join().unwrap();
        w_c.join().unwrap();

        coordinator.unregister_domain(&domain_c);
        coordinator.unregister_domain(&domain_b);
        coordinator.unregister_domain(&domain_a);
    }

    #[test]
    fn in_native_domain_does_not_block_stop_the_world_and_parks_on_leave_native() {
        let coordinator = Arc::new(SafepointCoordinator::new());
        let requester = coordinator.register_domain();
        let native_domain = coordinator.register_domain();

        let entered_native = Arc::new(Barrier::new(2));
        let inside_stw = Arc::new(AtomicBool::new(false));
        let wait_stw = Arc::new(Barrier::new(2));
        let resumed_after_stw = Arc::new(AtomicBool::new(false));

        let worker_domain = Arc::clone(&native_domain);
        let worker_entered = Arc::clone(&entered_native);
        let worker_wait_stw = Arc::clone(&wait_stw);
        let worker_inside_stw = Arc::clone(&inside_stw);
        let worker_resumed = Arc::clone(&resumed_after_stw);

        let worker = thread::spawn(move || {
            worker_domain.enter_native();
            worker_entered.wait();
            worker_wait_stw.wait();
            worker_domain.leave_native();
            assert!(!worker_inside_stw.load(Ordering::Acquire));
            worker_resumed.store(true, Ordering::Release);
        });

        entered_native.wait();
        coordinator.stop_the_world(Some(&requester), || {
            assert_eq!(native_domain.current_state(), DomainState::InNative);
            inside_stw.store(true, Ordering::Release);
            wait_stw.wait();
            // Wait until the worker hits leave_native and parks instead of racing past STW.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while native_domain.current_state() != DomainState::Parked {
                assert!(
                    std::time::Instant::now() < deadline,
                    "worker did not park on leave_native"
                );
                thread::yield_now();
            }
            assert!(!resumed_after_stw.load(Ordering::Acquire));
            inside_stw.store(false, Ordering::Release);
        });

        worker.join().unwrap();
        assert!(resumed_after_stw.load(Ordering::Acquire));

        coordinator.unregister_domain(&native_domain);
        coordinator.unregister_domain(&requester);
    }

    #[test]
    fn concurrent_register_and_unregister_with_stop_the_world() {
        let coordinator = Arc::new(SafepointCoordinator::new());
        let requester = coordinator.register_domain();
        let churn_count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::new();
        for _ in 0..4 {
            let coord = Arc::clone(&coordinator);
            let stop_flag = Arc::clone(&stop);
            let count = Arc::clone(&churn_count);
            workers.push(thread::spawn(move || {
                while !stop_flag.load(Ordering::Relaxed) {
                    let domain = coord.register_domain();
                    domain.poll();
                    domain.enter_native();
                    domain.leave_native();
                    domain.poll();
                    coord.unregister_domain(&domain);
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        // Also stress simultaneous STW requests from two registered domains.
        let coord_two = Arc::clone(&coordinator);
        let stop_two = Arc::clone(&stop);
        let domain_two = coordinator.register_domain();
        let domain_two_handle = Arc::clone(&domain_two);
        workers.push(thread::spawn(move || {
            while !stop_two.load(Ordering::Relaxed) {
                coord_two.stop_the_world(Some(&domain_two_handle), || {});
                domain_two_handle.poll();
            }
        }));

        for _ in 0..40 {
            coordinator.stop_the_world(Some(&requester), || {});
            thread::yield_now();
        }

        stop.store(true, Ordering::Release);
        // Entering native releases the safepoint coordinator while the main thread blocks in `join()`.
        requester.enter_native();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(churn_count.load(Ordering::Relaxed) > 0);
        coordinator.unregister_domain(&domain_two);
        coordinator.unregister_domain(&requester);
    }
}
