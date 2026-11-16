use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use zkcl_jit::CraneliftJitBackend;
use zkcl_runtime::native::{
    JitBackend, JitCompilationRequest, JitCompletions, JitFunction, JitOsrRequest, MachineCode,
};
use zkcl_runtime::{ExecutionStop, ProfilerConfig, Runtime};

struct OsrOnly {
    backend: CraneliftJitBackend,
    completed: AtomicBool,
}

// SAFETY: The wrapper forwards OSR requests, completions and releases unchanged.
unsafe impl JitBackend for OsrOnly {
    unsafe fn submit(&self, _: JitCompilationRequest) {}

    unsafe fn try_submit(&self, _: JitCompilationRequest) -> bool {
        false
    }

    fn supports_osr(&self) -> bool {
        true
    }

    unsafe fn try_submit_osr(&self, request: JitOsrRequest) -> bool {
        unsafe { self.backend.try_submit_osr(request) }
    }

    fn poll_completions(&self) -> Vec<JitFunction> {
        self.poll_for(&mut |_, _| true)
            .map_or_else(Vec::new, |(completed, _)| completed)
    }

    fn poll_for(&self, belongs: &mut dyn FnMut(usize, u64) -> bool) -> Option<JitCompletions> {
        let (completed, failed) = self.backend.poll_for(belongs)?;
        if !completed.is_empty() {
            self.completed.store(true, Ordering::Relaxed);
        }
        Some((completed, failed))
    }

    fn release(&self, code: usize, revision: u64) {
        self.backend.release(code, revision);
    }

    fn machine_code(&self, code: usize, revision: u64) -> Option<MachineCode> {
        self.backend.machine_code(code, revision)
    }
}

fn install_osr(runtime: &Runtime) {
    let backend = Arc::new(OsrOnly {
        backend: CraneliftJitBackend::new().unwrap(),
        completed: AtomicBool::new(false),
    });
    let wait_backend = Arc::clone(&backend);
    runtime
        .register_function("wait-for-osr-entry", move |runtime, _| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !wait_backend.completed.load(Ordering::Relaxed) {
                if Instant::now() >= deadline {
                    return Err("loop entry compilation timed out".into());
                }
                runtime.poll_jit_completions();
                std::thread::yield_now();
            }
            Ok(Vec::new())
        })
        .unwrap();
    runtime.set_jit_backend(backend);
    runtime.start_profiling(ProfilerConfig::default()).unwrap();
}

#[test]
fn first_call_transfers_live_loop_state_and_recovers_on_overflow() {
    let runtime = Runtime::builder().jit_threshold(4).start().unwrap();
    runtime
        .register_function("collect-during-osr", |runtime, _| {
            let report = runtime.profile_report();
            if !report
                .functions
                .iter()
                .any(|row| row.name == "OSR-LOOP" && (row.calls, row.native_calls) == (1, 1))
            {
                return Err("the first loop activation did not reach native code".into());
            }
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            r#"
            (defvar *osr-prefix* 0)
            (defvar *osr-binding* :outside)
            (defvar *osr-cleanup* 0)
            (defun osr-loop (count)
              (incf *osr-prefix*)
              (let ((live (list :kept *osr-binding*))
                    (sum (- most-positive-fixnum 2048)))
                (dotimes (i count (values live sum *osr-binding*))
                  (incf sum)
                  (when (= i 512) (wait-for-osr-entry))
                  (when (= i 1024) (collect-during-osr)))))
            "#,
        )
        .unwrap();
    install_osr(&runtime);

    runtime
        .load_str(
            r#"
            (assert (equal
                      (let ((*osr-binding* :inside))
                        (unwind-protect
                            (multiple-value-list (osr-loop 4096))
                          (incf *osr-cleanup*)))
                      (list '(:kept :inside) (+ most-positive-fixnum 2048) :inside)))
            (assert (= *osr-prefix* 1))
            (assert (= *osr-cleanup* 1))
            (assert (eq *osr-binding* :outside))
            "#,
        )
        .unwrap();

    let report = runtime.profile_report();
    let row = report
        .functions
        .iter()
        .find(|row| row.name == "OSR-LOOP")
        .unwrap();
    assert_eq!((row.calls, row.native_calls), (1, 1));
    let function = runtime.eval("#'osr-loop").unwrap().pop().unwrap();
    assert!(runtime.machine_code(&function).unwrap().is_none());
}

#[test]
fn native_loop_cancellation_runs_the_bytecode_callers_cleanup() {
    let runtime = Runtime::builder().jit_threshold(4).start().unwrap();
    runtime
        .register_function("cancel-in-osr", |runtime, _| {
            let report = runtime.profile_report();
            if !report.functions.iter().any(|row| {
                row.name == "OSR-UNTIL-CANCELLED" && (row.calls, row.native_calls) == (1, 1)
            }) {
                return Err("cancellation did not reach a transferred loop".into());
            }
            runtime.cancellation_handle().cancel_with_cleanup();
            Ok(Vec::new())
        })
        .unwrap();
    runtime
        .load_str(
            r#"
            (defvar *osr-cleanups* 0)
            (defun osr-until-cancelled ()
              (dotimes (i 20000 :no-cancel)
                (when (= i 512) (wait-for-osr-entry))
                (when (= i 1024) (cancel-in-osr))))
            "#,
        )
        .unwrap();
    install_osr(&runtime);
    let result = runtime.eval("(unwind-protect (osr-until-cancelled) (incf *osr-cleanups*))");
    assert_eq!(
        result.err().unwrap().execution_stop(),
        Some(ExecutionStop::Cancelled)
    );
    runtime.cancellation_handle().reset();
    assert_eq!(
        runtime.eval("*osr-cleanups*").unwrap()[0].as_fixnum(),
        Some(1)
    );
}

#[test]
fn step_limited_call_counts_the_same_backedges_when_osr_is_installed() {
    const PROGRAM: &str = r#"
        (defvar *osr-budget-count* 0)
        (defun osr-budget-loop (warm)
          (dotimes (i 4096)
            (incf *osr-budget-count*)
            (when (and warm (= i 512)) (wait-for-osr-entry))))
    "#;
    fn budgeted_count(runtime: &Runtime) -> isize {
        runtime.eval("(setq *osr-budget-count* 0)").unwrap();
        runtime.set_step_budget(Some(1650));
        let error = runtime.eval("(osr-budget-loop nil)").err().unwrap();
        assert_eq!(error.execution_stop(), Some(ExecutionStop::BudgetExhausted));
        runtime.set_step_budget(None);
        runtime.eval("*osr-budget-count*").unwrap()[0]
            .as_fixnum()
            .unwrap()
    }

    let baseline = Runtime::start().unwrap();
    baseline.load_str(PROGRAM).unwrap();
    let expected = budgeted_count(&baseline);

    let runtime = Runtime::builder().jit_threshold(4).start().unwrap();
    runtime.load_str(PROGRAM).unwrap();
    install_osr(&runtime);
    runtime.eval("(osr-budget-loop t)").unwrap();
    assert_eq!(budgeted_count(&runtime), expected);
    let report = runtime.profile_report();
    let row = report
        .functions
        .iter()
        .find(|row| row.name == "OSR-BUDGET-LOOP")
        .unwrap();
    assert_eq!((row.calls, row.native_calls), (2, 1));
}
