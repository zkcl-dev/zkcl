use std::time::{Duration, Instant};

use zkcl::{EvalErrorKind, ExecutionStop, Runtime};

fn eval_true(runtime: &Runtime, form: &str) {
    let values = runtime.eval(form).unwrap_or_else(|error| {
        panic!("expected form to succeed:\n{form}\nerror: {error:?}");
    });
    assert!(
        !values.is_empty() && values[0].is_t(),
        "expected T for form:\n{form}"
    );
}

struct TestClock<F>(F);

impl<F: Fn() -> std::io::Result<u64> + Send + Sync> zkcl::HostServices for TestClock<F> {
    fn wall_time_secs(&self) -> std::io::Result<i64> {
        Ok(0)
    }

    fn monotonic_nanos(&self) -> std::io::Result<u64> {
        (self.0)()
    }

    fn fill_entropy(&self, bytes: &mut [u8]) -> std::io::Result<()> {
        bytes.fill(0);
        Ok(())
    }
}

fn use_manual_clock(runtime: &mut Runtime) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let clock = Arc::new(AtomicU64::new(0));
    let reader = Arc::clone(&clock);
    runtime.set_host_services(TestClock(move || Ok(reader.load(Ordering::Relaxed))));
    runtime
        .register_function("advance-clock", move |_, args| {
            let seconds = args[0].as_i64().expect("seconds as an integer") as u64;
            clock.fetch_add(
                Duration::from_secs(seconds).as_nanos() as u64,
                Ordering::Relaxed,
            );
            Ok(Vec::new())
        })
        .unwrap();
}

#[test]
fn task_identity_and_terminal_results_survive_a_later_evaluation() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defvar *saved-root* nil)
             (defvar *saved-child* nil)",
        )
        .unwrap();
    let result = runtime
        .eval(
            "(progn
           (setq *saved-root* (zk-async:current-task))
           (assert (eq *saved-root* (zk-async:current-task)))
           (zk-async:with-task-group (group)
             (setq *saved-child*
                   (zk-async:spawn-task (group) (zk-async:current-task))))
           (values :root (list 42)))",
        )
        .unwrap();
    assert_eq!(result.len(), 2);
    drop(result);
    runtime.collect();
    eval_true(
        &runtime,
        "(and (zk-async:task-done-p *saved-root*)
              (equal (multiple-value-list (zk-async:await *saved-root*)) '(:root (42)))
              (eq (zk-async:await *saved-child*) *saved-child*)
              (zk-async:with-task-group (group)
                (let ((live (zk-async:spawn-task (group) :live)))
                  (and (null (zk-async:cancel-task *saved-child*))
                       (null (zk-async:cancel-task *saved-root*))
                       (eq (zk-async:await live) :live)))))",
    );
}

#[test]
fn a_task_results_cycle_is_collectable() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *weak-task*
           (zk-async:with-task-group (group)
             (let ((task (zk-async:spawn-task (group)
                           (values (zk-async:current-task) (list :kept)))))
               (assert (eq task (zk-async:await task)))
               (zk:make-weak-pointer task))))",
        )
        .unwrap();
    runtime.collect();
    eval_true(&runtime, "(null (zk:weak-pointer-value *weak-task*))");
}

#[test]
fn queued_closures_and_multiple_awaiters_survive_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-queued-tasks", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    eval_true(
        &runtime,
        "(let ((ready nil) (waiters nil))
           (zk-async:with-task-group (group)
             (let* ((payload (list 'kept (make-string 20 :initial-element #\\x)))
                    (task (zk-async:spawn-task (group)
                            (loop until ready do (zk-async:yield))
                            (values payload :second))))
               (dotimes (i 16)
                 (push (zk-async:spawn-task (group)
                         (multiple-value-list (zk-async:await task)))
                       waiters))
               (collect-queued-tasks)
               (setq ready t)
               (zk-async:await task)
               (collect-queued-tasks)
               (every (lambda (waiter)
                        (equal (zk-async:await waiter) (list payload :second)))
                      waiters))))",
    );
}

#[test]
fn failed_await_and_group_exit_raise_the_error_even_without_handlers() {
    let runtime = runtime_with_worker_failure_condition();
    for form in [
        "(zk-async:with-task-group (group)
           (zk-async:spawn-task (group) (error 'custom-worker-failure :code 13))
           :normal-result)",
        "(zk-async:with-task-group (group)
           (zk-async:await (zk-async:spawn-task (group :allow-failure t)
                             (error 'custom-worker-failure :code 13))))",
    ] {
        let error = runtime
            .eval(form)
            .err()
            .expect("child failure must propagate to Rust");
        assert!(
            error
                .message()
                .to_ascii_uppercase()
                .contains("CUSTOM-WORKER-FAILURE"),
            "{error:?}"
        );
    }
}

#[test]
fn group_cleanup_uses_the_original_group_after_rebinding_and_nonlocal_exit() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((cleaned nil) (started nil))
           (and (eq (block escape
                      (zk-async:with-task-group (group)
                        (declare (ignorable group))
                        (zk-async:spawn-task (group)
                          (unwind-protect
                               (progn (setq started t) (loop (zk-async:yield)))
                            (setq cleaned t)))
                        (loop until started do (zk-async:yield))
                        (setq group nil)
                        (return-from escape :escaped)))
                    :escaped)
                cleaned))",
    );
}

#[test]
fn wait_cycles_are_lisp_errors_and_the_group_remains_usable() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)) (a nil) (b nil))
           (zk-async:with-task-group (group)
             (setq a (zk-async:spawn-task (group)
                       (loop until b do (zk-async:yield))
                       (handler-case (zk-async:await b)
                         (zk-async:async-error () :cycle))))
             (setq b (zk-async:spawn-task (group)
                       (handler-case (zk-async:await a)
                         (zk-async:async-error () :cycle))))
             (let ((ancestor (zk-async:spawn-task (group)
                               (handler-case (zk-async:await owner)
                                 (zk-async:async-error () :ancestor-cycle)))))
               (and (eq (zk-async:await a) :cycle)
                    (eq (zk-async:await b) :cycle)
                    (eq (zk-async:await ancestor) :ancestor-cycle)
                    (eq (zk-async:await (zk-async:spawn-task (group) :ok)) :ok)))))",
    );
}

#[test]
fn delegated_spawn_depends_on_the_owner_of_its_group() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)) (cycle-caught nil))
           (zk-async:with-task-group (outer)
             (zk-async:spawn-task (outer)
               (zk-async:with-task-group (inner)
                 (zk-async:spawn-task (inner)
                   (zk-async:spawn-task (outer)
                     (handler-case (zk-async:await owner)
                       (zk-async:async-error () (setq cycle-caught t)))))))
             (loop until cycle-caught do (zk-async:yield)))
           cycle-caught)",
    );
}

#[test]
fn no_wait_can_await_its_owner_until_group_exit() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)) (started nil) (cleaned nil))
           (and (eq (zk-async:with-task-group (group)
                      (let ((child (zk-async:spawn-task (group :no-wait t)
                                     (unwind-protect
                                          (progn (setq started t) (zk-async:await owner))
                                       (setq cleaned t)))))
                        (loop until started do (zk-async:yield))
                        (assert (not (zk-async:task-done-p child)))
                        (handler-case (zk-async:await child)
                          (zk-async:async-error () :owner-cannot-await-child))))
                    :owner-cannot-await-child)
                cleaned))",
    );
}

#[test]
fn waiting_for_the_owner_from_shielded_cleanup_is_a_cycle() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)) (started nil) (caught nil))
           (zk-async:with-task-group (group)
             (zk-async:spawn-task (group :no-wait t)
               (unwind-protect
                    (progn (setq started t) (zk-async:await owner))
                 (handler-case (zk-async:await owner)
                   (zk-async:async-error () (setq caught t)))))
             (loop until started do (zk-async:yield))
             (assert (not caught)))
           caught)",
    );
}

#[test]
fn no_wait_does_not_discard_dependencies_of_nested_groups() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)) (caught nil))
           (zk-async:with-task-group (outer)
             (zk-async:spawn-task (outer :no-wait t)
               (zk-async:protect-from-cancel
                 (zk-async:with-task-group (inner)
                   (zk-async:spawn-task (inner)
                     (handler-case (zk-async:await owner)
                       (zk-async:async-error () (setq caught t)))))))
             (loop until caught do (zk-async:yield)))
           caught)",
    );
}

#[test]
fn no_wait_cancellation_releases_an_await_in_a_nested_group() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)) (started nil) (cleaned 0))
           (zk-async:with-task-group (outer)
             (zk-async:spawn-task (outer :no-wait t)
               (unwind-protect
                    (zk-async:with-task-group (inner)
                      (zk-async:spawn-task (inner)
                        (unwind-protect
                             (progn (setq started t) (zk-async:await owner))
                          (incf cleaned))))
                 (incf cleaned)))
             (loop until started do (zk-async:yield))
             (assert (zerop cleaned)))
           (= cleaned 2))",
    );
}

#[test]
fn an_await_cycle_can_be_released_by_a_deadline_inside_a_shield() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)) (started nil) (cleaned nil))
           (and (eq (zk-async:with-task-group (group)
                      (let ((child (zk-async:spawn-task (group)
                                     (zk-async:protect-from-cancel
                                       (zk-async:with-timeout (5 :on-timeout :elapsed)
                                         (unwind-protect
                                              (progn (setq started t) (zk-async:await owner))
                                           (setq cleaned t)))))))
                        (zk-async:spawn-task (group)
                          (loop until started do (zk-async:yield))
                          (advance-clock 5))
                        (zk-async:await child)))
                    :elapsed)
                cleaned))",
    );
}

#[test]
fn a_deadline_outside_a_shield_cannot_release_an_await_cycle() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)) (started nil) (caught nil))
           (zk-async:with-task-group (group)
             (zk-async:spawn-task (group)
               (zk-async:with-timeout (5 :on-timeout nil)
                 (zk-async:protect-from-cancel
                   (setq started t)
                   (handler-case (zk-async:await owner)
                     (zk-async:async-error () (setq caught t))))))
             (zk-async:spawn-task (group)
               (loop until started do (zk-async:yield))
               (advance-clock 5)))
           caught)",
    );
}

#[test]
fn an_exited_timeout_does_not_make_an_untimed_wait_breakable() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(let ((task (zk-async:current-task)))
           (zk-async:with-timeout (5) (values))
           (catch 'leave (zk-async:with-timeout (5) (throw 'leave nil)))
           (assert (eq (zk-async:with-timeout (5 :on-timeout :elapsed)
                         (advance-clock 5)
                         (zk-async:await task))
                       :elapsed))
           (handler-case (zk-async:await task)
             (zk-async:async-error () t)))",
    );
}

#[test]
fn cancellation_at_a_clock_fault_precedes_the_sleep_result() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut runtime = Runtime::start().unwrap();
    let failed = Arc::new(AtomicBool::new(false));
    let reader = Arc::clone(&failed);
    let cancellation = runtime.cancellation_handle();
    runtime.set_host_services(TestClock(move || {
        if reader.load(Ordering::Relaxed) {
            cancellation.cancel_with_cleanup();
            Err(std::io::Error::other("clock failed at wakeup"))
        } else {
            Ok(0)
        }
    }));
    runtime
        .register_function("fail-clock-at-wakeup", move |_, _| {
            failed.store(true, Ordering::Relaxed);
            Ok(Vec::new())
        })
        .unwrap();
    runtime.load_str("(defvar *cleanup-ran* nil)").unwrap();
    let error = runtime
        .eval(
            "(unwind-protect
              (zk-async:with-task-group (group)
                (zk-async:spawn-task (group) (fail-clock-at-wakeup))
                (sleep 10))
           (setq *cleanup-ran* t))",
        )
        .err()
        .expect("the host requested cancellation during the clock read");
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
    runtime.cancellation_handle().reset();
    eval_true(&runtime, "*cleanup-ran*");
}

#[test]
fn host_cancellation_wakes_the_parked_root_without_a_ready_child() {
    use std::sync::{Arc, Mutex};

    let mut runtime = Runtime::start().unwrap();
    let reads_since_sleep = Arc::new(Mutex::new(None::<u8>));
    let reads = Arc::clone(&reads_since_sleep);
    let cancellation = runtime.cancellation_handle();
    runtime.set_host_services(TestClock(move || {
        let count = {
            let mut guard = reads.lock().unwrap();
            let Some(count) = *guard else {
                return Ok(0);
            };
            *guard = Some(count + 1);
            count
        };
        match count {
            0 => Ok(0),
            1 => {
                // The scheduler is about to park. No other task can make the
                // root ready and neither timer has reached its deadline.
                cancellation.cancel_with_cleanup();
                Ok(0)
            }
            _ => Ok(60_000_000_000),
        }
    }));
    let read_counter = Arc::clone(&reads_since_sleep);
    runtime
        .register_function("prepare-root-sleep", move |rt, _| {
            *read_counter.lock().unwrap() = Some(0);
            Ok(vec![rt.fixnum(1).expect("1 is a fixnum")])
        })
        .unwrap();
    runtime.load_str("(defvar *root-cleaned* nil)").unwrap();

    let error = runtime
        .eval(
            "(zk-async:with-timeout (5)
                  (unwind-protect (sleep (prepare-root-sleep)) (setq *root-cleaned* t)))",
        )
        .err()
        .expect("host cancellation must stop the root task");
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
    assert_eq!(
        *reads_since_sleep.lock().unwrap(),
        Some(2),
        "no timer had to expire to wake the root"
    );
    runtime.cancellation_handle().reset();
    eval_true(&runtime, "*root-cleaned*");
}

#[test]
fn cancellation_keeps_shielded_sleep_and_await_registered_until_ready() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((ready nil) (slept nil) (finished nil) (release nil) (result nil))
           (zk-async:with-task-group (group)
             (let* ((provider (zk-async:spawn-task (group)
                                (loop until release do (zk-async:yield))
                                :provided))
                    (worker (zk-async:spawn-task (group)
                              (zk-async:protect-from-cancel
                                (setq ready t)
                                (let ((start (get-internal-real-time)))
                                  (sleep 0.02)
                                  (setq finished
                                        (>= (- (get-internal-real-time) start)
                                            (floor internal-time-units-per-second 50))
                                        slept t))
                                (setq result (zk-async:await provider))))))
               (loop until ready do (zk-async:yield))
               (and (zk-async:cancel-task worker)
                    (not (zk-async:cancel-task worker))
                    (progn
                      (loop until slept do (zk-async:yield))
                      (zk-async:yield)
                      (assert (not (zk-async:task-done-p worker)))
                      (setq release t))
                    (handler-case (zk-async:await worker)
                      (zk-async:waited-task-cancelled ()
                        (and finished (eq result :provided))))))))",
    );
}

#[test]
fn zero_timeout_skips_the_body_and_nested_deadlines_still_work_in_a_shield() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(and (equal (multiple-value-list
                       (zk-async:with-timeout (0 :on-timeout (values :zero :fallback))
                         (error \"body must not start\")))
                    '(:zero :fallback))
              (eq (zk-async:protect-from-cancel
                    (zk-async:with-timeout (0.01 :on-timeout :timed-out)
                      (sleep 1)))
                  :timed-out))",
    );
}

#[test]
fn completed_timeouts_leave_no_clock_work_for_later_bytecode() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    let mut runtime = Runtime::start().unwrap();
    let reads = Arc::new(AtomicU64::new(0));
    let reader = Arc::clone(&reads);
    runtime.set_host_services(TestClock(move || {
        Ok(reader.fetch_add(1, Ordering::Relaxed) + 1)
    }));
    runtime
        .register_function("clock-reads", move |runtime, _| {
            Ok(vec![runtime.integer_from(reads.load(Ordering::Relaxed))?])
        })
        .unwrap();
    eval_true(
        &runtime,
        "(progn
           (dotimes (i 512) (zk-async:with-timeout (3600) i))
           (let ((before (clock-reads)) (sum 0))
             (dotimes (i 4000) (incf sum i))
             (and (= sum 7998000) (= before (clock-reads)))))",
    );
}

#[test]
fn losing_the_host_clock_unwinds_timed_work_and_drains_the_group() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut runtime = Runtime::start().unwrap();
    let failed = Arc::new(AtomicBool::new(false));
    let reader = Arc::clone(&failed);
    runtime.set_host_services(TestClock(move || {
        if reader.load(Ordering::Relaxed) {
            Err(std::io::Error::other("clock failed"))
        } else {
            Ok(0)
        }
    }));
    runtime
        .register_function("break-clock", move |_, _| {
            failed.store(true, Ordering::Relaxed);
            Ok(Vec::new())
        })
        .unwrap();
    eval_true(
        &runtime,
        "(let ((started nil) (child-cleaned nil) (parent-cleaned nil) (fallback nil))
           (handler-case
               (zk-async:with-timeout (10 :on-timeout (setq fallback t))
                 (unwind-protect
                      (zk-async:with-task-group (group)
                        (zk-async:spawn-task (group)
                          (unwind-protect
                               (progn (setq started t) (sleep 5))
                            (setq child-cleaned t)))
                        (loop until started do (zk-async:yield))
                        (break-clock)
                        (loop (zk-async:yield)))
                   (setq parent-cleaned t)))
             (error (condition)
               (and (search \"clock failed\" (princ-to-string condition))
                    child-cleaned parent-cleaned (not fallback)))))",
    );
}

#[test]
fn host_cancellation_is_deferred_through_a_shielded_rust_callback() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("cancel-and-call", |runtime, arguments| {
            runtime.cancellation_handle().cancel_with_cleanup();
            Ok(runtime.call(&arguments[0], &[])?)
        })
        .unwrap();
    runtime.load_str("(defvar *shield-log* nil)").unwrap();
    let result = runtime.eval(
        "(unwind-protect
             (zk-async:protect-from-cancel
               (cancel-and-call (lambda () (push :callback *shield-log*)))
               (push :inside-shield *shield-log*))
           (push :cleanup *shield-log*))",
    );
    assert_eq!(
        result.err().unwrap().execution_stop(),
        Some(ExecutionStop::Cancelled)
    );
    runtime.cancellation_handle().reset();
    eval_true(
        &runtime,
        "(equal *shield-log* '(:cleanup :inside-shield :callback))",
    );
}

#[cfg(feature = "jit")]
#[test]
fn an_empty_group_keeps_dynamic_calls_suspendable() {
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    let backend = zkcl::enable_jit(&runtime).unwrap();
    runtime
        .load_str(
            "(defun call-operation (fn)
               (multiple-value-list (funcall fn)))
             (compile 'call-operation #'call-operation)",
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.poll_jit_completions() == 0 {
        assert!(Instant::now() < deadline, "operation did not compile");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!backend.code_cache().is_empty());
    eval_true(
        &runtime,
        "(zk-async:with-task-group (group)
           (let ((first (car (call-operation (lambda () (zk-async:spawn-task (group) :first))))))
             (assert (eq (zk-async:await first) :first))
             (let ((second (car (call-operation (lambda () (zk-async:spawn-task (group) :second))))))
               (eq (zk-async:await second) :second))))",
    );
    eval_true(
        &runtime,
        "(equal (call-operation (lambda () (values 1 2))) '(1 2))",
    );
}

#[cfg(feature = "jit")]
#[test]
fn a_precompiled_cpu_loop_yields_to_its_task_group() {
    let runtime = Runtime::start().unwrap();
    let backend = zkcl::enable_jit(&runtime).unwrap();
    runtime.set_jit_threshold(u32::MAX);
    runtime
        .load_str(
            "(defvar *stop-loop* nil)
         (defun cpu-loop ()
           (let ((counter 0))
             (loop until *stop-loop* do (incf counter))
             counter))
         (defun run-concurrently ()
           (zk-async:with-task-group (group)
             (let ((task (zk-async:spawn-task (group) (cpu-loop))))
               (zk-async:yield)
               (setq *stop-loop* t)
               (plusp (zk-async:await task)))))
         (compile 'cpu-loop #'cpu-loop)
         (compile 'run-concurrently #'run-concurrently)",
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.poll_jit_completions() == 0 {
        assert!(Instant::now() < deadline, "JIT compilation did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!backend.code_cache().is_empty());
    runtime.set_step_budget(Some(100_000));
    eval_true(&runtime, "(run-concurrently)");
}

#[test]
fn await_preserves_each_tasks_values_on_repeated_calls() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((root (zk-async:current-task)))
           (and (zk-async:taskp root)
                (string= (zk-async:task-name root) \"main\")
                (not (zk-async:task-done-p root))
                (zk-async:with-task-group (g)
                  (let ((t0 (zk-async:spawn g (lambda () (values)) :name \"zero\"))
                        (t1 (zk-async:spawn-task (g :name \"one\") (+ 20 22)))
                        (tm (zk-async:spawn-task (g :name \"multi\")
                              (zk-async:yield)
                              (values 1 :two \"three\"))))
                    (and (zk-async:taskp t0)
                         (zk-async:taskp t1)
                         (string= (zk-async:task-name t1) \"one\")
                         (null (multiple-value-list (zk-async:await t0)))
                         (eql (zk-async:await t1) 42)
                         (zk-async:task-done-p t1)
                         (equal (multiple-value-list (zk-async:await tm))
                                '(1 :two \"three\"))
                         (equal (multiple-value-list (zk-async:await tm))
                                '(1 :two \"three\")))))))",
    );
}

#[test]
fn sleeping_tasks_resume_in_deadline_order() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    eval_true(
        &runtime,
        "(let ((log nil) (started 0))
           (zk-async:with-task-group (g)
             (zk-async:spawn-task (g :name \"slower\")
               (incf started)
               (sleep 40)
               (push :slower log))
             (zk-async:spawn-task (g :name \"faster\")
               (incf started)
               (sleep 10)
               (push :faster log))
             (loop until (= started 2) do (zk-async:yield))
             (zk-async:yield)
             (advance-clock 50))
           (equal (reverse log) '(:faster :slower)))",
    );
}

#[test]
fn cpu_bound_siblings_complete_without_explicit_yield() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((turns nil))
           (zk-async:with-task-group (g)
             (let ((a (zk-async:spawn-task (g :name \"cpu-a\")
                        (dotimes (i 250)
                          (when (= (mod i 100) 0)
                            (push :a turns)))
                        :done-a))
                   (b (zk-async:spawn-task (g :name \"cpu-b\")
                        (dotimes (i 250)
                          (when (= (mod i 100) 0)
                            (push :b turns)))
                        :done-b)))
               (and (eq (zk-async:await a) :done-a)
                    (eq (zk-async:await b) :done-b)
                    (not (null (member :a turns)))
                    (not (null (member :b turns)))
                    (> (loop for x in (cdr turns)
                             for prev in turns
                             count (not (eq x prev)))
                       0)))))",
    );
}

#[test]
fn short_calls_do_not_starve_ready_siblings() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defun repeat-short-calls (step iterations)
               (dotimes (i iterations)
                 (multiple-value-bind (first second) (funcall step i)
                   (unless (= first second)
                     (error \"Unexpected return values\")))))",
        )
        .unwrap();
    eval_true(
        &runtime,
        "(let ((ready nil) (progress 0) (observed nil) (iterations 2000))
           (zk-async:with-task-group (group)
             (let ((cpu (zk-async:spawn-task (group)
                          (loop until ready do (zk-async:yield))
                          (repeat-short-calls
                           (lambda (x) (incf progress) (values x x))
                           iterations))))
               (zk-async:spawn-task (group)
                 (loop until (plusp progress) do (zk-async:yield))
                 (setq observed progress))
               (setq ready t)
               (zk-async:await cpu)))
           (and (= progress iterations) (< 0 observed iterations)))",
    );
}

#[test]
fn suspended_tasks_retain_their_special_bindings_and_live_objects_during_gc() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("trigger-gc", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime.load_str("(defvar *dyn* :global)").unwrap();
    eval_true(
        &runtime,
        "(let ((*dyn* :parent))
           (zk-async:with-task-group (g)
             (let ((a (zk-async:spawn-task (g :name \"bind-a\")
                        (let ((inherited *dyn*)
                              (kept (list 'payload-a 1)))
                          (let ((*dyn* :child-a))
                            (zk-async:yield)
                            (setq *dyn* :mutated-a)
                            (trigger-gc)
                            (zk-async:yield)
                            (list inherited *dyn* kept)))))
                   (b (zk-async:spawn-task (g :name \"bind-b\")
                        (let ((inherited *dyn*)
                              (kept (list 'payload-b 2)))
                          (progv '(*dyn*) '(:progv-b)
                            (trigger-gc)
                            (zk-async:yield)
                            (list inherited *dyn* kept))))))
               (trigger-gc)
               (let ((res-a (zk-async:await a))
                     (res-b (zk-async:await b)))
                 (and (eq *dyn* :parent)
                      (equal res-a '(:parent :mutated-a (payload-a 1)))
                      (equal res-b '(:parent :progv-b (payload-b 2))))))))",
    );
    eval_true(&runtime, "(eq *dyn* :global)");
}

#[test]
fn a_suspended_task_observes_redefinition_without_replaying_effects() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    runtime
        .load_str(
            "(defvar *task-phase* :outside)
             (defun task-target (payload) (values :original payload *task-phase*))",
        )
        .unwrap();
    runtime
        .register_function("redefine-task-target", |rt, _| {
            rt.load_str("(defun task-target (payload) (values :new payload nil *task-phase*))")?;
            rt.collect();
            Ok(Vec::new())
        })
        .unwrap();
    eval_true(
        &runtime,
        "(let ((old #'task-target) (started nil) (released nil) (log nil))
           (zk-async:with-task-group (group)
             (let ((task (zk-async:spawn-task (group)
                           (let ((*task-phase* :inside) (payload (list 42)))
                             (unwind-protect
                                  (progn
                                    (push :entered log)
                                    (setq started t)
                                    (loop until released do (zk-async:yield))
                                    (values (multiple-value-list (task-target payload))
                                            (multiple-value-list (funcall old payload))))
                               (push *task-phase* log))))))
               (loop until started do (zk-async:yield))
               (redefine-task-target)
               (setq released t)
               (multiple-value-bind (new previous) (zk-async:await task)
                 (and (equal new '(:new (42) nil :inside))
                      (equal previous '(:original (42) :inside))
                      (eq (second new) (second previous))
                      (equal log '(:inside :entered))
                      (eq *task-phase* :outside))))))",
    );
}

#[test]
fn cancellation_bypasses_error_handlers_and_runs_unwind_protect() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((log nil)
               (cancelled-during-cleanup nil)
               (shielded-finished nil))
           (zk-async:with-task-group (g)
             (let ((worker
                     (zk-async:spawn-task (g :name \"worker\")
                       (unwind-protect
                            (handler-case
                                (ignore-errors
                                  (loop (zk-async:yield)))
                              (error () (push :caught-inside log)))
                         (setq cancelled-during-cleanup (zk-async:task-cancelled-p))
                         (zk-async:protect-from-cancel
                           (sleep 0.01)
                           (setq shielded-finished t))
                         (push :cleaned log)))))
               (zk-async:yield)
               (zk-async:cancel-task worker)
               (let ((caught
                       (handler-case (zk-async:await worker)
                         (zk-async:waited-task-cancelled (c)
                           (list :cancelled (eq (zk-async:waited-task-cancelled-task c)
                                                worker))))))
                 (and cancelled-during-cleanup
                      shielded-finished
                      (zk-async:task-done-p worker)
                      (equal log '(:cleaned))
                      (equal caught '(:cancelled t)))))))",
    );
}

fn runtime_with_worker_failure_condition() -> Runtime {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(define-condition custom-worker-failure (error)
               ((code :initarg :code :reader custom-worker-failure-code)))",
        )
        .unwrap();
    runtime
}

#[test]
fn task_group_cancels_siblings_and_resignals_the_child_condition() {
    let runtime = runtime_with_worker_failure_condition();
    eval_true(
        &runtime,
        "(let ((sibling-cleaned nil))
           (let ((outcome
                   (handler-case
                       (zk-async:with-task-group (g)
                         (zk-async:spawn-task (g :name \"sibling\")
                           (unwind-protect
                                (loop (zk-async:yield))
                             (setq sibling-cleaned t)))
                         (zk-async:spawn-task (g :name \"failing\")
                           (zk-async:yield)
                           (error 'custom-worker-failure :code 77)))
                     (custom-worker-failure (c)
                       (list :failed (custom-worker-failure-code c))))))
             (and sibling-cleaned
                  (equal outcome '(:failed 77)))))",
    );
}

#[test]
fn allowing_failure_leaves_siblings_running_and_await_resignals_the_condition() {
    let runtime = runtime_with_worker_failure_condition();
    eval_true(
        &runtime,
        "(let ((sibling-result nil)
               (awaited-error nil))
           (zk-async:with-task-group (g)
             (let ((bad (zk-async:spawn-task (g :allow-failure t :name \"bad\")
                          (error 'custom-worker-failure :code 91)))
                   (good (zk-async:spawn-task (g :name \"good\")
                           (zk-async:yield)
                           :ok)))
               (setq sibling-result (zk-async:await good))
               (setq awaited-error
                     (handler-case (zk-async:await bad)
                       (custom-worker-failure (c)
                         (custom-worker-failure-code c))))))
           (and (eq sibling-result :ok)
                (eql awaited-error 91)))",
    );
}

#[test]
fn normal_group_exit_cancels_no_wait_tasks_and_waits_for_cleanup() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((bg-cleaned nil))
           (let ((result
                   (zk-async:with-task-group (g)
                     (zk-async:spawn-task (g :no-wait t :name \"background\")
                       (unwind-protect
                            (loop (zk-async:yield))
                         (zk-async:protect-from-cancel
                           (sleep 0.01)
                           (setq bg-cleaned t))))
                     (zk-async:yield)
                     :group-value)))
             (and bg-cleaned
                  (eq result :group-value))))",
    );
}

#[test]
fn timeout_unwinds_a_sleeping_body_before_signaling_timeout_error() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((cleaned nil) (start (get-internal-real-time)))
           (let ((res
                   (handler-case
                       (zk-async:with-timeout (0.01)
                         (unwind-protect
                              (sleep 5)
                           (setq cleaned t)))
                     (zk-async:timeout-error (c)
                       (list :timed-out (zk-async:timeout-error-seconds c))))))
             (and cleaned
                  (equal res '(:timed-out 0.01))
                  (< (- (get-internal-real-time) start)
                     (* 4 internal-time-units-per-second)))))",
    );
}

#[test]
fn timeout_options_return_the_fallback_or_a_completion_flag() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(and (eq (zk-async:with-timeout (0.01 :on-timeout :fallback)
                    (sleep 5))
                  :fallback)
              (equal (multiple-value-list
                       (zk-async:with-timeout-opt (0.01)
                         (sleep 5)))
                     '(nil nil))
              (equal (multiple-value-list
                       (zk-async:with-timeout-opt (1.0)
                         99))
                     '(99 t)))",
    );
}

#[test]
fn task_cancellation_skips_the_inner_timeout_handler() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((inner-fallback-ran nil)
               (outer-caught nil))
           (zk-async:with-task-group (g)
             (let ((victim
                     (zk-async:spawn-task (g :name \"victim\")
                       (zk-async:with-timeout (5.0 :on-timeout (progn
                                                                 (setq inner-fallback-ran t)
                                                                 :swallowed))
                         (loop (zk-async:yield))))))
               (zk-async:yield)
               (zk-async:cancel-task victim)
               (setq outer-caught
                     (handler-case (zk-async:await victim)
                       (zk-async:waited-task-cancelled () :outer-won)))))
           (and (not inner-fallback-ran)
                (eq outer-caught :outer-won)))",
    );
}

#[test]
fn outer_timeout_unwinds_the_inner_body_without_running_its_timeout_handler() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(let ((inner-cleaned nil)
               (inner-on-timeout-ran nil))
           (let ((res
                   (handler-case
                       (zk-async:with-timeout (5)
                         (zk-async:with-timeout (10 :on-timeout (progn
                                                                   (setq inner-on-timeout-ran t)
                                                                   :bad))
                           (unwind-protect
                                (progn (advance-clock 20) (loop (zk-async:yield)))
                             (setq inner-cleaned t))))
                     (zk-async:timeout-error (c)
                       (zk-async:timeout-error-seconds c)))))
             (and inner-cleaned
                  (not inner-on-timeout-ran)
                  (eql res 5))))",
    );
}

#[test]
fn nonlocal_exits_cannot_target_another_task() {
    let runtime = Runtime::start().unwrap();

    eval_true(
        &runtime,
        "(handler-case
             (zk-async:with-task-group (g)
               (block escape
                 (zk-async:spawn-task (g :name \"bad-return-from\")
                   (return-from escape :escaped))))
           (control-error () t))",
    );

    eval_true(
        &runtime,
        "(handler-case
             (zk-async:with-task-group (g)
               (catch 'escape-tag
                 (zk-async:spawn-task (g :name \"bad-throw\")
                   (throw 'escape-tag :escaped))))
           (control-error () t))",
    );
}

#[test]
fn a_rust_callback_cannot_suspend_its_task() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("call-lisp-through-rust", |runtime, arguments| {
            Ok(runtime.call(&arguments[0], &[])?)
        })
        .unwrap();
    eval_true(
        &runtime,
        "(handler-case
             (zk-async:with-task-group (g)
               (zk-async:spawn-task (g :name \"other\") (zk-async:yield))
               (call-lisp-through-rust #'zk-async:yield)
               nil)
           (error (c)
             (not (null (search \"cannot yield across a native or Rust callback frame\"
                                (princ-to-string c))))))",
    );
}

#[test]
fn bordeaux_threads_thread_yield_cooperates_with_task_scheduler() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defpackage #:bordeaux-threads
               (:nicknames #:bt #:bt2)
               (:use #:cl)
               (:export #:current-thread #:all-threads #:thread-name #:thread-alive-p))
             (in-package #:bt2)
             (defun current-thread () :main)
             (defun all-threads () '(:main))
             (defun thread-name (th) (declare (ignore th)) \"main\")
             (defun thread-alive-p (th) (declare (ignore th)) t)
             (defun signal-not-implemented (op) (error \"not implemented: ~S\" op))
             (defun bt-error (fmt &rest args) (apply #'error fmt args))
             (defun mark-not-implemented (op &rest keys) (declare (ignore op keys)) nil)",
        )
        .unwrap();
    let impl_source = runtime
        .eval("(zk::%asdf-port-source \"bordeaux-threads/impl-zkcl.lisp\")")
        .unwrap();
    let impl_source = runtime.string(&impl_source[0]).unwrap();
    runtime.load_str(&impl_source).unwrap();
    eval_true(
        &runtime,
        "(let ((trace nil)
               (yield-values nil))
           (zk-async:with-task-group (g)
             (let ((a (zk-async:spawn-task (g :name \"bt-a\")
                        (push :a1 trace)
                        (setq yield-values (multiple-value-list (bt2::%thread-yield)))
                        (push :a2 trace)))
                   (b (zk-async:spawn-task (g :name \"bt-b\")
                        (push :b1 trace)
                        (bt2::%thread-yield)
                        (push :b2 trace))))
               (zk-async:await a)
               (zk-async:await b)))
           (and (null yield-values)
                (equal (reverse trace) '(:a1 :b1 :a2 :b2))))",
    );
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn host_cancellation_wakes_and_unwinds_active_task_group() {
    let runtime = Runtime::start().unwrap();
    let cancellation = runtime.cancellation_handle();
    let (started, waiting) = std::sync::mpsc::channel();
    runtime
        .register_function("notify-waiting", move |_, _| {
            started.send(())?;
            Ok(Vec::new())
        })
        .unwrap();

    let worker = std::thread::spawn(move || {
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(25));
        cancellation.cancel_with_cleanup();
    });

    runtime.load_str("(defvar *host-cancel-log* nil)").unwrap();
    let start = Instant::now();
    let error = runtime
        .eval(
            "(unwind-protect
                  (zk-async:with-task-group (g)
                    (zk-async:spawn-task (g :name \"child-sleep\")
                      (unwind-protect
                           (progn (notify-waiting) (sleep 5))
                        (push :child *host-cancel-log*)))
                    (sleep 5))
               (push :parent *host-cancel-log*))",
        )
        .err()
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    worker.join().unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));

    runtime.cancellation_handle().reset();
    eval_true(
        &runtime,
        "(and (not (null (member :child *host-cancel-log*)))
              (not (null (member :parent *host-cancel-log*))))",
    );
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn host_hard_abort_stops_active_task_group_immediately() {
    let runtime = Runtime::start().unwrap();
    let cancellation = runtime.cancellation_handle();
    let (started, waiting) = std::sync::mpsc::channel();
    runtime
        .register_function("notify-abort-ready", move |_, _| {
            started.send(())?;
            Ok(Vec::new())
        })
        .unwrap();

    let worker = std::thread::spawn(move || {
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        cancellation.cancel();
    });

    runtime.load_str("(defvar *aborted-cleanups* nil)").unwrap();
    let error = runtime
        .eval(
            "(unwind-protect
                  (zk-async:with-task-group (g)
                    (zk-async:spawn-task (g :name \"child\")
                      (unwind-protect
                           (progn (notify-abort-ready) (sleep 5))
                        (push :child *aborted-cleanups*)))
                    (sleep 5))
               (push :parent *aborted-cleanups*))",
        )
        .err()
        .unwrap();
    worker.join().unwrap();
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Aborted));
    runtime.cancellation_handle().reset();
    eval_true(&runtime, "(null *aborted-cleanups*)");
}

#[test]
fn shielded_timeout_completion_preserves_values_and_propagates_cleanup_errors() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime
        .load_str(
            "(define-condition custom-timeout-cleanup-failure (error)
               ((code :initarg :code :reader custom-timeout-cleanup-failure-code)))",
        )
        .unwrap();
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(and (equal (multiple-value-list
                       (zk-async:with-timeout (5 :on-timeout :timed-out)
                         (zk-async:protect-from-cancel
                           (advance-clock 10)
                           (values :shielded :ok))))
                    '(:shielded :ok))
              (equal (multiple-value-list
                       (zk-async:with-timeout-opt (5)
                         (zk-async:protect-from-cancel
                           (advance-clock 10)
                           :kept)))
                    '(:kept t))
              (eql (handler-case
                       (zk-async:with-timeout (5 :on-timeout :timed-out)
                         (unwind-protect
                              (progn (advance-clock 10) (sleep 5))
                           (error 'custom-timeout-cleanup-failure :code 55)))
                     (custom-timeout-cleanup-failure (c)
                       (custom-timeout-cleanup-failure-code c)))
                   55))",
    );
}

#[test]
fn nonlocal_exit_from_task_group_propagates_child_failure() {
    let runtime = runtime_with_worker_failure_condition();
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(and (eql (handler-case
                       (block out
                         (zk-async:with-task-group (g)
                           (let ((started (zk-async:make-event)))
                             (zk-async:spawn-task (g)
                               (unwind-protect
                                    (progn (zk-async:set-event started)
                                           (loop (zk-async:yield)))
                                 (error 'custom-worker-failure :code 31)))
                             (zk-async:wait-event started)
                             (return-from out :escaped))))
                     (custom-worker-failure (c)
                       (custom-worker-failure-code c)))
                   31)
              (eql (handler-case
                       (catch 'out-tag
                         (zk-async:with-task-group (g)
                           (let ((failed (zk-async:make-event)))
                             (zk-async:spawn-task (g)
                               (zk-async:set-event failed)
                               (error 'custom-worker-failure :code 42))
                             (zk-async:protect-from-cancel
                               (zk-async:wait-event failed)
                               (zk-async:yield)
                               (throw 'out-tag :escaped)))))
                     (custom-worker-failure (c)
                       (custom-worker-failure-code c)))
                   42)
              (eql (handler-case
                       (zk-async:with-timeout (0.01 :on-timeout :timed-out)
                         (zk-async:with-task-group (g)
                           (let ((started (zk-async:make-event)))
                             (zk-async:spawn-task (g)
                               (unwind-protect
                                    (progn (zk-async:set-event started) (sleep 5))
                                 (error 'custom-worker-failure :code 63)))
                             (zk-async:wait-event started)
                             (sleep 5))))
                     (custom-worker-failure (c)
                       (custom-worker-failure-code c)))
                   63))",
    );
}

#[test]
fn zero_sleep_wakes_due_timers_and_yields_to_ready_siblings() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(let ((started (zk-async:make-event))
               (woke nil))
           (zk-async:with-task-group (g)
             (zk-async:spawn-task (g)
               (zk-async:set-event started)
               (sleep 2)
               (setq woke t))
             (zk-async:wait-event started)
             (zk-async:yield)
             (advance-clock 2)
             (sleep 0)
             woke))",
    );
}

#[test]
fn task_group_p_and_async_error_message_inspect_handles_and_errors() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((msg (handler-case (zk-async:await-any nil)
                      (zk-async:async-error (c)
                        (zk-async:async-error-message c)))))
           (and (stringp msg)
                (> (length msg) 0)
                (zk-async:with-task-group (g)
                  (and (zk-async:task-group-p g)
                       (not (zk-async:task-group-p (zk-async:current-task)))
                       (not (zk-async:task-group-p 42))))))",
    );
}

#[test]
fn await_any_races_tasks_and_detects_cycles() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(let ((slow-cleaned nil))
           (and (zk-async:with-task-group (g)
                  (let* ((slow-started (zk-async:make-event))
                         (fast-started (zk-async:make-event))
                         (slow (zk-async:spawn-task (g :no-wait t :name \"slow\")
                                 (unwind-protect
                                      (progn (zk-async:set-event slow-started)
                                             (sleep 10)
                                             :slow)
                                   (setq slow-cleaned t))))
                         (fast (zk-async:spawn-task (g :no-wait t :name \"fast\")
                                 (zk-async:set-event fast-started)
                                 (sleep 2)
                                 (values :fast 2))))
                    (zk-async:wait-event slow-started)
                    (zk-async:wait-event fast-started)
                    (zk-async:spawn-task (g) (advance-clock 2))
                    (let ((winner (zk-async:await-any (list slow fast))))
                      (and (eq winner fast)
                           (equal (multiple-value-list (zk-async:await winner))
                                  '(:fast 2))))))
                slow-cleaned))",
    );

    eval_true(
        &runtime,
        "(let ((owner (zk-async:current-task)))
           (and (zk-async:with-task-group (g)
                  (let* ((done (zk-async:spawn-task (g) :done))
                         (self-waiter (zk-async:spawn-task (g :no-wait t) (zk-async:await owner)))
                         (sleeper (zk-async:spawn-task (g) (sleep 1) :slept)))
                    (zk-async:await done)
                    (and (eq (zk-async:await-any (list done self-waiter)) done)
                         (progn
                           (advance-clock 1)
                           (eq (zk-async:await-any (list self-waiter sleeper)) sleeper)))))
                (eq (handler-case
                        (zk-async:with-task-group (g)
                          (let ((a (zk-async:spawn-task (g :no-wait t) (zk-async:await owner)))
                                (b (zk-async:spawn-task (g :no-wait t) (zk-async:await owner))))
                            (zk-async:await-any (list a b))))
                      (zk-async:async-error () :cycle))
                    :cycle)
                (eq (handler-case
                        (zk-async:with-task-group (g)
                          (let* ((d (zk-async:spawn-task (g :no-wait t) (zk-async:await owner)))
                                 (c (zk-async:spawn-task (g :no-wait t)
                                      (zk-async:await-any (list owner d)))))
                            (zk-async:yield)
                            (zk-async:await c)))
                      (zk-async:async-error () :indirect-cycle))
                    :indirect-cycle)
                (eq (handler-case (zk-async:await-any (list owner :bad))
                      (type-error () :type-error))
                    :type-error)))",
    );
}

#[test]
fn await_any_keeps_the_completion_that_woke_it() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((gate (zk-async:make-event)))
           (zk-async:with-task-group (group)
             (let* ((winner (zk-async:spawn-task (group) (zk-async:wait-event gate) :winner))
                    (later (zk-async:spawn-task (group) (zk-async:wait-event gate) :later))
                    (waiter (zk-async:spawn-task (group) (zk-async:await-any (list later winner winner)))))
               (zk-async:yield)
               (zk-async:set-event gate)
               (and (eq winner (zk-async:await waiter))
                    (eq later (zk-async:await-any (list later winner)))))))",
    );
}

#[test]
fn events_coordinate_multiple_waiters_and_respect_cancellation_and_reset() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(let ((ev (zk-async:make-event))
               (log nil))
           (assert (zk-async:eventp ev))
           (assert (not (zk-async:event-set-p ev)))
           (zk-async:with-task-group (g)
             (let ((w1 (zk-async:spawn-task (g) (zk-async:wait-event ev) (push :w1 log)))
                   (w2 (zk-async:spawn-task (g) (zk-async:wait-event ev) (push :w2 log)))
                   (w3 (zk-async:spawn-task (g) (zk-async:wait-event ev) (push :w3 log))))
               (zk-async:yield)
               (zk-async:cancel-task w2)
               (zk-async:yield)
               (assert (zk-async:set-event ev))
               (assert (not (zk-async:set-event ev)))
               (assert (zk-async:reset-event ev))
               (assert (not (zk-async:reset-event ev)))
               (zk-async:await w1)
               (zk-async:await w3)
               (assert (eq (handler-case (zk-async:await w2)
                             (zk-async:waited-task-cancelled () :cancelled))
                           :cancelled))))
           (and (equal (reverse log) '(:w1 :w3))
                (eq (zk-async:with-timeout (3 :on-timeout :timed-out)
                      (zk-async:with-task-group (g)
                        (zk-async:spawn-task (g) (advance-clock 3))
                        (zk-async:wait-event ev)))
                    :timed-out)))",
    );
}

#[test]
fn channels_support_unbounded_bounded_and_rendezvous_transfer_with_gc_and_cancellation() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-channel-heap", |rt, _| {
            rt.collect();
            Ok(Vec::new())
        })
        .unwrap();
    runtime.set_step_budget(Some(100_000));
    eval_true(
        &runtime,
        "(let ((unbounded (zk-async:make-channel)))
           (assert (zk-async:channelp unbounded))
           (assert (not (zk-async:channel-closed-p unbounded)))
           (zk-async:channel-send unbounded (list :one))
           (zk-async:channel-send unbounded (list :two))
           (assert (zk-async:close-channel unbounded))
           (assert (not (zk-async:close-channel unbounded)))
           (assert (zk-async:channel-closed-p unbounded))
           (assert (eq (handler-case (zk-async:channel-send unbounded :three)
                         (zk-async:async-error () :closed))
                       :closed))
           (assert (equal (multiple-value-list (zk-async:channel-recv unbounded)) '((:one) t)))
           (assert (equal (multiple-value-list (zk-async:channel-try-recv unbounded)) '((:two) t)))
           (assert (equal (multiple-value-list (zk-async:channel-recv unbounded)) '(nil nil)))
           (assert (equal (multiple-value-list (zk-async:channel-try-recv unbounded)) '(nil nil)))
           t)",
    );

    eval_true(
        &runtime,
        "(let ((bounded (zk-async:make-channel :capacity 2))
               (received nil))
           (assert (zk-async:channel-try-send bounded (list :a)))
           (assert (zk-async:channel-try-send bounded (list :b)))
           (assert (not (zk-async:channel-try-send bounded (list :c))))
           (zk-async:with-task-group (g)
             (let ((s1 (zk-async:spawn-task (g)
                         (zk-async:channel-send bounded (list :c))))
                   (cancelled-sender (zk-async:spawn-task (g)
                                       (zk-async:channel-send bounded (list :drop-me))))
                   (s2 (zk-async:spawn-task (g)
                         (zk-async:channel-send bounded (list :d)))))
               (zk-async:yield)
               (zk-async:cancel-task cancelled-sender)
               (collect-channel-heap)
               (zk-async:yield)
               (dotimes (_ 4)
                 (multiple-value-bind (val ok) (zk-async:channel-recv bounded)
                   (assert ok)
                   (push val received)))
               (collect-channel-heap)
               (zk-async:await s1)
               (zk-async:await s2)
               (assert (eq (handler-case (zk-async:await cancelled-sender)
                             (zk-async:waited-task-cancelled () :cancelled))
                           :cancelled))))
           (equal (reverse received) '((:a) (:b) (:c) (:d))))",
    );

    eval_true(
        &runtime,
        "(let ((rdv (zk-async:make-channel :capacity 0)))
           (assert (not (zk-async:channel-try-send rdv :early)))
           (zk-async:with-task-group (g)
             (let ((r (zk-async:spawn-task (g)
                        (multiple-value-list (zk-async:channel-recv rdv)))))
               (zk-async:yield)
               (assert (zk-async:channel-try-send rdv (list :handoff)))
               (collect-channel-heap)
               (assert (equal (zk-async:await r) '((:handoff) t)))
               (let ((s (zk-async:spawn-task (g) (zk-async:channel-send rdv (list :sync)))))
                 (assert (equal (multiple-value-list (zk-async:channel-recv rdv)) '((:sync) t)))
                 (assert (equal (zk-async:await s) '(:sync))))
               (let ((blocked-s (zk-async:spawn-task (g)
                                  (handler-case (zk-async:channel-send rdv (list :never))
                                    (zk-async:async-error () :closed)))))
                 (zk-async:yield)
                 (zk-async:close-channel rdv)
                 (assert (eq (zk-async:await blocked-s) :closed)))))
           t)",
    );
}

#[test]
fn buffered_channel_reserves_delivery_for_the_oldest_live_receiver() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((channel (zk-async:make-channel :capacity 1)))
           (zk-async:with-task-group (g)
             (let ((cancelled (zk-async:spawn-task (g) (zk-async:channel-recv channel)))
                   (first (zk-async:spawn-task (g) (zk-async:channel-recv channel)))
                   (second (zk-async:spawn-task (g) (zk-async:channel-recv channel))))
               (zk-async:yield)
               (assert (zk-async:cancel-task cancelled))
               (assert (zk-async:channel-try-send channel (list :one)))
               (assert (zk-async:channel-try-send channel nil))
               (assert (equal (multiple-value-list (zk-async:channel-try-recv channel)) '(nil nil)))
               (zk:gc)
               (and (equal (multiple-value-list (zk-async:await first)) '((:one) t))
                    (equal (multiple-value-list (zk-async:await second)) '(nil t))
                    (handler-case (progn (zk-async:await cancelled) nil)
                      (zk-async:waited-task-cancelled () t))))))",
    );
}

#[test]
fn buffered_messages_follow_gc_without_rooting_unreachable_channel_cycles() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(defparameter *weak-channel*
               (let ((left (zk-async:make-channel)) (right (zk-async:make-channel)))
                 (zk-async:channel-send left (list right))
                 (zk-async:channel-send right (list left))
                 (zk:make-weak-pointer left)))",
        )
        .unwrap();
    runtime.collect();
    eval_true(&runtime, "(null (zk:weak-pointer-value *weak-channel*))");
    eval_true(
        &runtime,
        "(let ((channel (zk-async:make-channel :capacity 32)))
           (dotimes (i 32) (zk-async:channel-send channel (list i)))
           (dotimes (i 16) (assert (equal (zk-async:channel-recv channel) (list i))))
           (dotimes (i 16) (zk-async:channel-send channel (list (+ i 32))))
           (zk:gc)
           (loop for i from 16 below 48
                 always (equal (multiple-value-list (zk-async:channel-recv channel))
                               (list (list i) t))))",
    );
}

#[test]
fn a_synchronous_rust_callback_can_use_a_ready_channel() {
    let runtime = Runtime::start().unwrap();
    let body = runtime
        .eval(
            "(lambda () (let ((channel (zk-async:make-channel :capacity 1)))
               (zk-async:channel-send channel (list 42))
               (zk-async:channel-recv channel)))",
        )
        .unwrap()
        .remove(0)
        .into_persistent();
    runtime
        .register_function("read-ready-channel", move |rt, _| {
            Ok(rt.call(&body.bind(rt).unwrap(), &[])?)
        })
        .unwrap();
    let callback = runtime.eval("#'read-ready-channel").unwrap().remove(0);
    let values = runtime.call(&callback, &[]).unwrap();
    assert_eq!(runtime.repr(&values[0]).unwrap(), "(42)");
    assert!(values[1].is_t());
}

#[test]
fn select_coordinates_recv_send_event_task_timeout_and_default_atomically() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(200_000));
    eval_true(
        &runtime,
        "(let ((ch1 (zk-async:make-channel :capacity 0))
               (ch2 (zk-async:make-channel :capacity 1))
               (ev (zk-async:make-event)))
           ;; Non-blocking default branch when nothing is ready
           (assert (eq (zk-async:select
                         ((:recv ch1 msg) msg)
                         ((:send ch1 :never) :sent)
                         ((:event ev) :event)
                         (otherwise :idle))
                       :idle))
           ;; Duplicate default or empty clauses fail cleanly via program-error at macroexpansion
           (assert (eq (handler-case (macroexpand-1 '(zk-async:select))
                         (program-error () :prog-err))
                       :prog-err))
           (assert (eq (handler-case
                           (macroexpand-1 '(zk-async:select (otherwise 1) (:default 2)))
                         (program-error () :dup-default))
                       :dup-default))
           ;; Multiple channels become ready in the same turn: only the winning branch
           ;; consumes from its channel; the second channel retains its buffered value.
           (zk-async:with-task-group (g)
             (let ((selector
                     (zk-async:spawn-task (g)
                       (zk-async:select
                         ((:recv ch1 msg ok) (list :ch1 msg ok))
                         ((:recv ch2 msg ok) (list :ch2 msg ok))))))
               (zk-async:yield)
               (assert (zk-async:channel-try-send ch1 :first))
               (assert (zk-async:channel-try-send ch2 :kept-in-ch2))
               (assert (equal (zk-async:await selector) '(:ch1 :first t)))
               (assert (equal (multiple-value-list (zk-async:channel-try-recv ch2))
                              '(:kept-in-ch2 t)))))
           ;; Send + timeout + event + closed channel + task branches
           (zk-async:with-task-group (g)
             (let* ((rdv (zk-async:make-channel :capacity 0))
                    (worker (zk-async:spawn-task (g)
                              (zk-async:select
                                ((:send rdv :payload val) (list :sent val))
                                ((:event ev) :ev)
                                ((:timeout 5) :timeout))))
                    (recv-task (zk-async:spawn-task (g)
                                 (zk-async:channel-recv rdv))))
               (assert (equal (zk-async:await worker) '(:sent :payload)))
               (assert (equal (multiple-value-list (zk-async:await recv-task)) '(:payload t)))))
           ;; Timeout branch wins when clock advances
           (assert (eq (zk-async:with-task-group (g)
                         (zk-async:spawn-task (g) (advance-clock 4))
                         (zk-async:select
                           ((:recv ch1 msg) msg)
                           ((:timeout 3) :timed-out)))
                       :timed-out))
           ;; Task branch wins when spawned task finishes, and pure-task cycles are detected
           (zk-async:with-task-group (g)
             (let* ((slow (zk-async:spawn-task (g) (zk-async:wait-event ev) :slow))
                    (fast (zk-async:spawn-task (g) :fast))
                    (winner (zk-async:select
                              ((:task slow tk) (list :slow tk))
                              ((:task fast tk) (list :fast tk)))))
               (assert (eq (first winner) :fast))
               (assert (eq (second winner) fast))
               (zk-async:set-event ev)
               (assert (eq (zk-async:await slow) :slow))))
           ;; Closed recv yields (NIL NIL); closed send signals async-error
           (let ((closed (zk-async:make-channel)))
             (zk-async:close-channel closed)
             (assert (equal (zk-async:select
                              ((:recv closed v ok) (list :closed v ok)))
                            '(:closed nil nil)))
             (assert (eq (handler-case
                             (zk-async:select
                               ((:send closed :nope) :sent))
                           (zk-async:async-error () :send-closed))
                         :send-closed)))
           ;; Cycle detection for task-only select
           (let ((owner (zk-async:current-task)))
             (assert (eq (handler-case
                             (zk-async:with-task-group (g)
                               (let ((a (zk-async:spawn-task (g :no-wait t) (zk-async:await owner)))
                                     (b (zk-async:spawn-task (g :no-wait t) (zk-async:await owner))))
                                 (zk-async:select
                                   ((:task a) :a)
                                   ((:task b) :b))))
                           (zk-async:async-error () :cycle))
                         :cycle)))
           t)",
    );
}

#[test]
fn cancel_task_group_cancels_children_and_joins_cleanly() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((cleanups 0)
               (ev (zk-async:make-event))
               (owner-continued nil))
           (zk-async:with-task-group (g)
             (assert (zk-async:task-group-p g))
             (let ((w1 (zk-async:spawn-task (g)
                         (unwind-protect
                             (zk-async:wait-event ev)
                           (incf cleanups))))
                   (w2 (zk-async:spawn-task (g :no-wait t)
                         (unwind-protect
                             (zk-async:wait-event ev)
                           (incf cleanups)))))
               (zk-async:yield)
               (assert (zk-async:cancel-task-group g))
               (assert (not (zk-async:cancel-task-group g)))
               (assert (eq (handler-case
                               (zk-async:spawn-task (g) :late)
                             (zk-async:async-error () :closed))
                           :closed))
               (setq owner-continued t)
               (assert (eq (handler-case (zk-async:await w1)
                             (zk-async:waited-task-cancelled () :w1-cancelled))
                           :w1-cancelled))
               (assert (eq (handler-case (zk-async:await w2)
                             (zk-async:waited-task-cancelled () :w2-cancelled))
                           :w2-cancelled))))
           (and owner-continued (= cleanups 2)))",
    );
}

#[test]
fn incremental_evaluation_drives_task_group_select_timeout_and_host_operation() {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::task::{Wake, Waker};

    struct FlagWake(std::sync::mpsc::Sender<()>);
    impl Wake for FlagWake {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    let runtime = Runtime::start().unwrap();
    let (send, recv) = mpsc::channel::<zkcl::HostCompletion>();
    runtime
        .register_async_function("async-fetch", move |_, args, complete| {
            let tag = args[0].as_i64().unwrap();
            send.send(complete).unwrap();
            let _ = tag;
            Ok(())
        })
        .unwrap();

    let (wake_tx, wake_rx) = mpsc::channel();
    let waker = Waker::from(Arc::new(FlagWake(wake_tx)));

    let mut evaluation = runtime
        .start_eval(
            "(let ((ch (zk-async:make-channel :capacity 1)))
               (zk-async:with-task-group (g)
                 (zk-async:spawn-task (g)
                   (zk-async:channel-send ch (list :host (async-fetch 10))))
                 (let ((slow (zk-async:spawn-task (g :no-wait t)
                               (zk-async:with-timeout (60 :on-timeout :timeout)
                                 (async-fetch 20)))))
                   (prog1
                       (zk-async:select
                         ((:recv ch msg ok) (list msg ok))
                         ((:task slow) :slow-won))
                     (zk-async:cancel-task-group g)))))",
        )
        .unwrap();

    loop {
        match evaluation.resume(Some(&waker)).unwrap() {
            zkcl::EvaluationPoll::Runnable => continue,
            zkcl::EvaluationPoll::Waiting(_) => break,
            zkcl::EvaluationPoll::Paused(_) => panic!("debugging was not enabled"),
            zkcl::EvaluationPoll::Ready(vals) => {
                panic!("unexpected early completion with {} values", vals.len())
            }
        }
    }

    let first = recv.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = recv.recv_timeout(Duration::from_secs(2)).unwrap();

    let worker = std::thread::spawn(move || {
        assert!(first.complete(|rt| Ok(vec![rt.integer_from(99)?])));
    });
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();

    // Cooperative cancellation waits for `second` to stop and release its completion.
    loop {
        match evaluation.resume(Some(&waker)).unwrap() {
            zkcl::EvaluationPoll::Runnable => continue,
            zkcl::EvaluationPoll::Waiting(_) => break,
            zkcl::EvaluationPoll::Paused(_) => panic!("debugging was not enabled"),
            zkcl::EvaluationPoll::Ready(vals) => panic!(
                "task group exited before cancelled host completion finished ({} values)",
                vals.len()
            ),
        }
    }
    assert!(second.is_cancelled());
    drop(second);
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let values = loop {
        match evaluation.resume(Some(&waker)).unwrap() {
            zkcl::EvaluationPoll::Runnable => continue,
            zkcl::EvaluationPoll::Waiting(_) => panic!("unexpected wait after completion dropped"),
            zkcl::EvaluationPoll::Paused(_) => panic!("debugging was not enabled"),
            zkcl::EvaluationPoll::Ready(values) => break values,
        }
    };
    assert_eq!(runtime.repr(&values[0]).unwrap(), "((:HOST 99) T)");
}

#[test]
fn cancellation_between_semaphore_handoff_and_dispatch_returns_the_permit() {
    let runtime = Runtime::builder()
        .step_budget(Some(100_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((semaphore (zk-async:make-semaphore 0)) (entered nil))
           (zk-async:with-task-group (group)
             (let ((task (zk-async:spawn-task (group)
                           (zk-async:with-semaphore (semaphore) (setq entered t)))))
               (zk-async:yield)
               (assert (zerop (zk-async:release-semaphore semaphore)))
               (assert (zk-async:cancel-task task))
               (assert (handler-case (progn (zk-async:await task) nil)
                         (zk-async:waited-task-cancelled () t)))))
           (and (not entered) (= (zk-async:semaphore-count semaphore) 1)))",
    );
}

#[test]
fn semaphore_bounds_concurrency_with_cancellation_safe_with_semaphore() {
    let runtime = Runtime::builder()
        .step_budget(Some(200_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((sem (zk-async:make-semaphore 2))
               (in-flight 0)
               (peak 0)
               (completed 0))
           (assert (zk-async:semaphorep sem))
           (assert (= (zk-async:semaphore-count sem) 2))
           (assert (zk-async:try-acquire-semaphore sem))
           (assert (zk-async:try-acquire-semaphore sem))
           (assert (not (zk-async:try-acquire-semaphore sem)))
           (assert (= (zk-async:semaphore-count sem) 0))
           (assert (= (zk-async:release-semaphore sem) 1))
           (assert (= (zk-async:release-semaphore sem) 2))
           ;; Concurrent bounded critical section + cancelled waiter leaves permits exact
           (zk-async:with-task-group (g)
             (dotimes (_ 6)
               (zk-async:spawn-task (g)
                 (zk-async:with-semaphore (sem)
                   (incf in-flight)
                   (when (> in-flight peak)
                     (setq peak in-flight))
                   (zk-async:yield)
                   (decf in-flight)
                   (incf completed))))
             ;; Waiter cancelled while waiting or right as permit releases never leaks permits
             (let ((blocked (zk-async:spawn-task (g)
                              (zk-async:with-semaphore (sem)
                                (zk-async:yield)))))
               (zk-async:yield)
               (zk-async:cancel-task blocked)
               (assert (eq (handler-case (zk-async:await blocked)
                             (zk-async:waited-task-cancelled () :cancelled))
                           :cancelled))))
           (and (= in-flight 0)
                (= peak 2)
                (= completed 6)
                (= (zk-async:semaphore-count sem) 2)))",
    );
}

#[cfg(feature = "jit")]
#[test]
fn jit_compiled_wrapper_runs_task_group_select_and_semaphore() {
    let runtime = Runtime::builder()
        .jit_threshold(1)
        .step_budget(Some(200_000))
        .start()
        .unwrap();
    let function = runtime
        .eval(
            "(defun jit-async-orchestrator (val)
               (let ((sem (zk-async:make-semaphore 1))
                     (ch (zk-async:make-channel :capacity 1)))
                 (zk-async:with-task-group (g)
                   (zk-async:spawn-task (g)
                     (zk-async:with-semaphore (sem)
                       (zk-async:yield)
                       (zk-async:channel-send ch (list :done val))))
                   (zk-async:spawn-task (g :no-wait t)
                     (zk-async:with-timeout (10 :on-timeout :timed-out)
                       (loop (zk-async:yield))))
                   (prog1
                       (zk-async:select
                         ((:recv ch msg ok) (list msg ok (zk-async:semaphore-count sem)))
                         ((:timeout 5) :timeout))
                     (zk-async:cancel-task-group g)))))",
        )
        .unwrap();
    let target = runtime
        .eval("#'jit-async-orchestrator")
        .unwrap()
        .pop()
        .unwrap();
    let backend = zkcl::enable_jit(&runtime).unwrap();
    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    runtime.call(&compile, &[&runtime.nil(), &target]).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.poll_jit_completions() == 0 {
        assert!(Instant::now() < deadline, "JIT compilation did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!backend.code_cache().is_empty());
    let _ = function;
    eval_true(
        &runtime,
        "(equal (jit-async-orchestrator 77) '((:done 77) t 1))",
    );
}

#[test]
fn cancel_scope_supports_self_cross_task_cancellation_shielding_and_inner_groups() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(200_000));
    eval_true(
        &runtime,
        "(let (stale-scope
               (cleanup-log nil))
           ;; Normal exit keeps multiple values and leaves stale cancel-scope a no-op
           (assert (equal (multiple-value-list
                            (zk-async:with-cancel-scope (s :on-cancel (values :cancelled))
                              (assert (zk-async:cancel-scope-p s))
                              (assert (not (zk-async:cancel-scope-cancelled-p s)))
                              (setq stale-scope s)
                              (values :ok 42)))
                          '(:ok 42)))
           (assert (not (zk-async:cancel-scope-cancelled-p stale-scope)))
           (assert (not (zk-async:cancel-scope stale-scope)))

           ;; Self-cancellation respects protect-from-cancel, cleans inner groups, and runs on-cancel
           (let (caught-scope)
             (assert (eq (zk-async:with-cancel-scope (s :on-cancel :scope-caught)
                           (setq caught-scope s)
                           (unwind-protect
                               (zk-async:with-task-group (g)
                                 (zk-async:spawn-task (g :no-wait t)
                                   (unwind-protect
                                       (loop (zk-async:yield))
                                     (push :inner-child cleanup-log)))
                                 (zk-async:yield)
                                 (zk-async:protect-from-cancel
                                   (assert (zk-async:cancel-scope s))
                                   (assert (zk-async:cancel-scope-cancelled-p s))
                                   (push :shielded cleanup-log))
                                 (push :unreachable cleanup-log))
                             (push :outer-cleanup cleanup-log)))
                         :scope-caught))
             (assert (equal (reverse cleanup-log) '(:shielded :inner-child :outer-cleanup)))
             (assert (zk-async:cancel-scope-cancelled-p caught-scope))
             (assert (not (zk-async:cancel-scope caught-scope))))

           ;; Cross-task cancellation wakes suspended owner cleanly with default NIL fallback
           (let ((never (zk-async:make-event)))
             (assert (null (zk-async:with-task-group (outer)
                             (zk-async:with-cancel-scope (s)
                               (zk-async:spawn-task (outer)
                                 (zk-async:yield)
                                 (zk-async:cancel-scope s))
                               (zk-async:wait-event never))))))

           ;; Outer timeout is never swallowed by an inner cancel scope
           (assert (eq (zk-async:with-timeout (2 :on-timeout :outer-timeout)
                         (zk-async:with-cancel-scope (inner :on-cancel :swallowed)
                           (zk-async:protect-from-cancel
                             (zk-async:cancel-scope inner)
                             (zk-async:with-task-group (g)
                               (zk-async:spawn-task (g) (advance-clock 2))
                               (zk-async:yield)))))
                       :outer-timeout))
           t)",
    );
}

#[test]
fn task_and_group_status_predicates_await_all_and_race_compose_cleanly() {
    let runtime = Runtime::builder()
        .step_budget(Some(200_000))
        .start()
        .unwrap();
    runtime
        .load_str("(define-condition custom-race-failure (error) ())")
        .unwrap();
    eval_true(
        &runtime,
        "(let ((gate (zk-async:make-event))
               (loser-cleaned nil))
           (zk-async:with-task-group (g)
             (let* ((running (zk-async:spawn-task (g) (zk-async:wait-event gate) :done))
                    (ok-task (zk-async:spawn-task (g) 10))
                    (fail-task (zk-async:spawn-task (g :allow-failure t)
                                 (error 'custom-race-failure))))
               (zk-async:yield)
               (assert (zk-async:task-running-p running))
               (assert (not (zk-async:task-done-p running)))
               (assert (zk-async:task-completed-p ok-task))
               (assert (zk-async:task-failed-p fail-task))
               (assert (not (zk-async:task-group-closed-p g)))
               (assert (not (zk-async:task-group-cancelled-p g)))
               (zk-async:set-event gate)
               (assert (equal (zk-async:await-all (list ok-task running)) '(10 :done)))
               (assert (null (zk-async:await-all nil)))
               (zk-async:cancel-task-group g)
               (assert (zk-async:task-group-closed-p g))
               (assert (zk-async:task-group-cancelled-p g))))

           ;; race preserves multiple values from winner and cancels & joins losers
           (assert (equal (multiple-value-list
                            (zk-async:race
                              (progn (zk-async:yield) (values :fast 99))
                              (unwind-protect
                                  (loop (zk-async:yield))
                                (setq loser-cleaned t))))
                          '(:fast 99)))
           (assert loser-cleaned)

           ;; race propagates error when fastest branch fails
           (assert (eq (handler-case
                           (zk-async:race
                             (error 'custom-race-failure)
                             (loop (zk-async:yield)))
                         (custom-race-failure () :failed))
                       :failed))
           t)",
    );
}

#[test]
fn promises_propagate_multivalues_errors_gc_cycles_and_integrate_with_select() {
    let runtime = Runtime::builder()
        .step_budget(Some(200_000))
        .start()
        .unwrap();
    runtime
        .load_str(
            "(define-condition custom-promise-error (error)
               ((code :initarg :code :reader custom-promise-error-code)))",
        )
        .unwrap();
    eval_true(
        &runtime,
        "(progn
         ;; Multi-value resolution, multiple waiters, duplicate settlement ignored
         (let ((p (zk-async:make-promise)))
           (assert (zk-async:promisep p))
           (assert (not (zk-async:promise-done-p p)))
           (zk-async:with-task-group (g)
             (let ((w1 (zk-async:spawn-task (g) (multiple-value-list (zk-async:await-promise p))))
                   (w2 (zk-async:spawn-task (g) (multiple-value-list (zk-async:await-promise p)))))
               (zk-async:yield)
               (assert (zk-async:fulfill-promise p :alpha :beta))
               (assert (not (zk-async:fulfill-promise p :late)))
               (assert (not (zk-async:fail-promise p \"ignored\")))
               (assert (zk-async:promise-done-p p))
               (assert (equal (zk-async:await w1) '(:alpha :beta)))
               (assert (equal (zk-async:await w2) '(:alpha :beta))))))

         ;; Condition & string failures re-signal faithfully
         (let ((p-cond (zk-async:make-promise))
               (p-msg (zk-async:make-promise)))
           (zk-async:fail-promise p-cond 'custom-promise-error :code 503)
           (zk-async:fail-promise p-msg \"promise failed: ~D\" 7)
           (assert (= (handler-case (zk-async:await-promise p-cond)
                        (custom-promise-error (c) (custom-promise-error-code c)))
                      503))
           (assert (equal (handler-case (zk-async:await-promise p-msg)
                            (zk-async:async-error (c) (zk-async:async-error-message c)))
                          \"promise failed: 7\")))

         ;; Select waits atomically across promise + channel without losing buffered messages
         (let ((p (zk-async:make-promise))
               (ch (zk-async:make-channel :capacity 1)))
           (zk-async:with-task-group (g)
             (let ((selector (zk-async:spawn-task (g)
                               (zk-async:select
                                 ((:promise p resolved)
                                  (assert (eq resolved p))
                                  (multiple-value-list (zk-async:await-promise resolved)))
                                 ((:recv ch v) (list :channel v))))))
               (zk-async:yield)
               (assert (zk-async:fulfill-promise p :won 1))
               (assert (zk-async:channel-try-send ch :buffered))
               (assert (equal (zk-async:await selector) '(:won 1)))
               (assert (equal (multiple-value-list (zk-async:channel-try-recv ch))
                              '(:buffered t))))))

         ;; GC collects promise reference cycles cleanly
         (let ((weak (let ((p (zk-async:make-promise)))
                       (zk-async:fulfill-promise p (list p))
                       (zk:make-weak-pointer p))))
           (zk:gc)
           (assert (null (zk:weak-pointer-value weak))))
         t)",
    );
}

#[test]
fn select_rotates_ready_branches_fairly_and_supports_biased_priority() {
    let runtime = Runtime::builder()
        .step_budget(Some(200_000))
        .start()
        .unwrap();
    eval_true(
        &runtime,
        "(let ((ch1 (zk-async:make-channel :capacity 10))
               (ch2 (zk-async:make-channel :capacity 10))
               (c1 0)
               (c2 0))
           (dotimes (i 10)
             (zk-async:channel-try-send ch1 i)
             (zk-async:channel-try-send ch2 i))
           (dotimes (_ 10)
             (zk-async:select
               ((:recv ch1) (incf c1))
               ((:recv ch2) (incf c2))))
           (assert (and (> c1 0) (> c2 0) (= (+ c1 c2) 10)))

           ;; Biased select strictly prioritizes earlier ready branches
           (let ((b1 (zk-async:make-channel :capacity 4))
                 (b2 (zk-async:make-channel :capacity 4))
                 (picked nil))
             (dotimes (i 4)
               (zk-async:channel-try-send b1 i)
               (zk-async:channel-try-send b2 i))
             (dotimes (_ 4)
               (zk-async:select (:biased t)
                 ((:recv b1) (push :first picked))
                 ((:recv b2) (push :second picked))))
             (assert (equal picked '(:first :first :first :first)))
             (assert (equal (multiple-value-list (zk-async:channel-try-recv b2)) '(0 t))))
           t)",
    );
}

#[test]
fn bordeaux_threads_locks_and_condition_variables_coordinate_async_tasks() {
    let mut runtime = Runtime::start().unwrap();
    use_manual_clock(&mut runtime);
    runtime.set_step_budget(Some(200_000));
    runtime
        .load_str(
            "(defpackage #:bordeaux-threads
               (:nicknames #:bt #:bt2)
               (:use #:cl)
               (:export #:current-thread #:all-threads #:thread-name #:thread-alive-p))
             (in-package #:bt2)
             (defun current-thread () (if (zk::%in-scheduler-p) (zk-async:current-task) :main))
             (defun all-threads () (list (current-thread)))
             (defun thread-name (th) (declare (ignore th)) \"task\")
             (defun thread-alive-p (th) (declare (ignore th)) t)
             (defun signal-not-implemented (op) (error \"not implemented: ~S\" op))
             (defun bt-error (fmt &rest args) (apply #'error fmt args))
             (defun mark-not-implemented (op &rest keys) (declare (ignore op keys)) nil)",
        )
        .unwrap();
    let impl_source = runtime
        .eval("(zk::%asdf-port-source \"bordeaux-threads/impl-zkcl.lisp\")")
        .unwrap();
    let impl_source = runtime.string(&impl_source[0]).unwrap();
    runtime.load_str(&impl_source).unwrap();
    runtime.load_str("(in-package #:cl-user)").unwrap();
    eval_true(
        &runtime,
        "(let ((lock (bt2::%make-lock \"m\"))
               (rlock (bt2::%make-recursive-lock \"rm\"))
               (cv (bt2::%make-condition-variable \"cv\"))
               (ready nil)
               (woke 0)
               (timed-out nil))
           ;; Non-recursive lock mutex + timeout failure + condition-wait/notify/broadcast
           (zk-async:with-task-group (g)
             (let* ((waiter1 (zk-async:spawn-task (g)
                              (bt2::%with-lock (lock nil)
                                (loop until ready do (bt2::%condition-wait cv lock nil))
                                (incf woke))))
                   (waiter2 (zk-async:spawn-task (g)
                              (bt2::%with-lock (lock nil)
                                (loop until ready do (bt2::%condition-wait cv lock nil))
                                (incf woke))))
                   (waiter-to (zk-async:spawn-task (g)
                                (bt2::%with-lock (lock nil)
                                  (setq timed-out (not (bt2::%condition-wait cv lock 2))))))
                   (clock-driver (zk-async:spawn-task (g :no-wait t)
                                   (loop until (zk-async:task-done-p waiter-to)
                                         do (advance-clock 1) (zk-async:yield)))))
               (declare (ignore clock-driver))
               ;; Timeout waiter expires cleanly via manual clock and reacquires lock
               (zk-async:await waiter-to)
               (assert timed-out)
               ;; Ensure waiter1 and waiter2 are waiting before broadcast
               (loop until (= (length (bt2::condition-variable-waiters cv)) 2)
                     do (zk-async:yield))
               ;; Broadcast wakes both condition waiters
               (bt2::%with-lock (lock nil)
                 (setq ready t)
                 (bt2::%condition-broadcast cv))
               (zk-async:await waiter1)
               (zk-async:await waiter2)
               (assert (= woke 2))))

           ;; Recursive lock allows same task re-entry, blocks other task, forbids alien unlock
           (zk-async:with-task-group (g)
             (let ((order nil)
                   (inside (zk-async:make-event))
                   (checked (zk-async:make-event)))
               (let ((t1 (zk-async:spawn-task (g)
                           (bt2::%with-recursive-lock (rlock nil)
                             (bt2::%with-recursive-lock (rlock nil)
                               (push :t1-inner order)
                               (zk-async:set-event inside)
                               (zk-async:wait-event checked))
                             (push :t1-outer order))))
                     (t2 (zk-async:spawn-task (g)
                           (zk-async:wait-event inside)
                           (assert (not (bt2::%acquire-recursive-lock rlock nil nil)))
                           (assert (handler-case (progn (bt2::%release-recursive-lock rlock) nil)
                                     (error () t)))
                           (zk-async:set-event checked)
                           (bt2::%with-recursive-lock (rlock nil)
                             (push :t2 order)))))
                 (zk-async:await t1)
                 (zk-async:await t2)
                 (assert (equal (reverse order) '(:t1-inner :t1-outer :t2))))))

           ;; Cancelled condition-wait still cleanly reacquires lock during cleanup before exit
           (zk-async:with-task-group (g)
             (let ((victim (zk-async:spawn-task (g)
                             (bt2::%with-lock (lock nil)
                               (bt2::%condition-wait cv lock nil)))))
               (loop until (= (length (bt2::condition-variable-waiters cv)) 1)
                     do (zk-async:yield))
               (zk-async:cancel-task victim)
               (assert (eq (handler-case (zk-async:await victim)
                             (zk-async:waited-task-cancelled () :cancelled))
                           :cancelled))))
           ;; Lock is free afterwards
           (bt2::%with-lock (lock nil)
             t))",
    );
    runtime
        .load_str("(defparameter *main-lock* (bt2::%make-lock \"main\"))")
        .unwrap();
    eval_true(&runtime, "(bt2::%acquire-lock *main-lock* nil nil)");
    runtime.eval("(bt2::%release-lock *main-lock*)").unwrap();
    eval_true(&runtime, "(bt2::%with-lock (*main-lock* 0) t)");
}

#[test]
fn packed_select_and_sync_fast_paths_preserve_semantics_errors_and_cancellation() {
    let runtime = Runtime::start().unwrap();
    // The expansion contains the packed selector and its literal opcode vector.
    eval_true(
        &runtime,
        "(let ((expanded (macroexpand-1 '(zk-async:select ((:recv ch v) v) ((:timeout 0) :t) (otherwise :d)))))
           (not (null (search \"%SELECT-PACKED\" (prin1-to-string expanded)))))",
    );

    // A ready operation can commit without parking a task.
    eval_true(
        &runtime,
        "(let ((ch (zk-async:make-channel :capacity 2))
               (ev (zk-async:make-event :signaled t))
               (p (zk-async:make-promise)))
           (zk-async:fulfill-promise p :ok 99)
           (assert (eql (zk-async:channel-send ch :a) :a))
           (assert (eql (zk-async:select ((:recv ch v) v) ((:timeout 0) :to)) :a))
           (assert (eql (zk-async:select ((:recv ch v) v) (otherwise :default)) :default))
           (assert (eq (zk-async:select ((:timeout 0) :zero)) :zero))
           (assert (eq (zk-async:select ((:event ev) :ev-hit)) :ev-hit))
           (assert (eq (zk-async:select ((:promise p pv) (assert (zk-async:promise-done-p pv)) t)) t))
           (assert (= (length (multiple-value-list (zk-async:await-promise p))) 2))
           (zk-async:close-channel ch)
           (multiple-value-bind (v ok) (zk-async:channel-recv ch)
             (and (null v) (null ok))))",
    );

    // Packed select preserves full argument validation even when an earlier branch is ready,
    // and respects self/scope cancellation on ready channel/select fast paths.
    eval_true(
        &runtime,
        "(let ((ch (zk-async:make-channel :capacity 4)))
           (zk-async:channel-send ch :ready)
           (assert (eq (handler-case
                           (zk-async:select
                             ((:recv ch v) v)
                             ((:timeout -1) :bad))
                         (error () :caught))
                       :caught))
           (assert (eql (zk-async:channel-recv ch) :ready))
           (zk-async:channel-send ch :item)
           (assert (eq (zk-async:with-cancel-scope (scope :on-cancel :cancelled)
                         (zk-async:cancel-scope scope)
                         (zk-async:channel-recv ch))
                       :cancelled))
           (assert (eq (zk-async:with-cancel-scope (scope :on-cancel :cancelled)
                         (zk-async:cancel-scope scope)
                         (zk-async:select ((:recv ch value) value)))
                       :cancelled))
           (eq (zk-async:channel-recv ch) :item))",
    );
}

#[test]
fn deadlocked_waits_signal_in_lisp_and_drain_child_cleanups() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((cleaned nil)
               (channel (zk-async:make-channel :capacity 0)))
           (assert (eq (handler-case
                           (unwind-protect (zk-async:channel-recv channel)
                             (push :root cleaned))
                         (zk-async:async-error () :deadlocked))
                       :deadlocked))
           (assert (eq (handler-case
                           (zk-async:with-task-group (group)
                             (zk-async:spawn-task (group)
                               (unwind-protect (zk-async:channel-recv channel)
                                 (push :receiver cleaned)))
                             (zk-async:spawn-task (group)
                               (unwind-protect (zk-async:wait-event (zk-async:make-event))
                                 (push :sibling cleaned))))
                         (zk-async:async-error () :deadlocked))
                       :deadlocked))
           (assert (equal cleaned '(:sibling :receiver :root)))
           (zk-async:with-task-group (group)
             (let ((sender (zk-async:spawn-task (group)
                             (zk-async:channel-send channel :reused))))
               (assert (eq (zk-async:channel-recv channel) :reused))
               (zk-async:await sender)
               t)))",
    );
}

#[test]
fn semaphore_fixnum_and_high_throughput_channels_compose_cleanly() {
    let runtime = Runtime::start().unwrap();
    eval_true(
        &runtime,
        "(let ((sem (zk-async:make-semaphore 2))
               (buf (zk-async:make-channel :capacity 64))
               (unbuf (zk-async:make-channel :capacity 0))
               (sum 0))
           (assert (= (zk-async:semaphore-count sem) 2))
           (assert (eq (zk-async:acquire-semaphore sem) t))
           (assert (= (zk-async:semaphore-count sem) 1))
           (assert (= (zk-async:release-semaphore sem) 2))

           ;; Tight buffered + packed select fast-path loop inside a task group.
           (zk-async:with-task-group (g)
             (dotimes (i 500)
               (zk-async:channel-send buf i)
               (zk-async:select
                 ((:recv buf v ok) (when ok (incf sum v)))
                 (otherwise (error \"unexpected miss\"))))
             ;; Unbuffered rendezvous across tasks (WaitQueue + timer heap fairness).
             (let ((prod (zk-async:spawn-task (g)
                           (dotimes (i 200)
                             (zk-async:channel-send unbuf i))
                           (zk-async:close-channel unbuf))))
               (declare (ignore prod))
               (loop
                 (zk-async:select
                   ((:recv unbuf v ok)
                     (unless ok (return))
                     (incf sum v))))))
           (= sum (+ (/ (* 499 500) 2) (/ (* 199 200) 2))))",
    );
}
