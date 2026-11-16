use std::time::Duration;

use zkcl::{FunctionProfile, ProfileReport, ProfilerConfig, Runtime};

fn row<'a>(report: &'a ProfileReport, name: &str) -> &'a FunctionProfile {
    report
        .functions
        .iter()
        .find(|row| row.name.contains(name))
        .unwrap_or_else(|| panic!("missing {name} in {report:?}"))
}

#[test]
fn recursive_calls_have_self_and_inclusive_time_and_observed_samples() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defun profile-rec (n) (if (zerop n) 0 (1+ (profile-rec (1- n)))))")
        .unwrap();
    runtime
        .start_profiling(ProfilerConfig {
            sample_interval: Duration::from_nanos(1),
        })
        .unwrap();
    assert_eq!(
        runtime.eval("(profile-rec 8)").unwrap()[0].as_fixnum(),
        Some(8)
    );
    let report = runtime.stop_profiling();
    let recursive = row(&report, "PROFILE-REC");
    assert_eq!(recursive.calls, 9);
    assert!(recursive.total_time >= recursive.self_time);
    assert!(!recursive.self_time.is_zero());
    assert!(recursive.samples > 0);
    assert!(report.samples >= recursive.samples);
    assert_eq!(report.sampling_mode, "safe-point wall-clock");
    assert!(!report.running);
    let stopped = runtime.profile_report();
    runtime.eval("(profile-rec 2)").unwrap();
    assert_eq!(runtime.profile_report().elapsed, stopped.elapsed);
    assert_eq!(row(&runtime.profile_report(), "PROFILE-REC").calls, 9);
}

#[test]
fn allocation_hotspots_use_successful_allocation_bytes_across_gc() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("profile-collect", |runtime, _| {
            runtime.collect();
            Ok(vec![runtime.nil()])
        })
        .unwrap();
    runtime
        .load_str(
            "(defun profile-allocate (n) (loop repeat n collect (cons n n)))
                      (defun profile-parent (n) (length (profile-allocate n)))",
        )
        .unwrap();
    runtime.start_profiling(ProfilerConfig::default()).unwrap();
    runtime.eval("(profile-parent 200)").unwrap();
    let before_gc = runtime.profile_report();
    runtime.eval("(profile-collect)").unwrap();
    runtime.eval("(profile-parent 200)").unwrap();
    let report = runtime.stop_profiling();
    let allocate = row(&report, "PROFILE-ALLOCATE");
    let parent = row(&report, "PROFILE-PARENT");
    assert_eq!(allocate.calls, 2);
    assert!(allocate.self_allocated_bytes >= 400 * 2 * std::mem::size_of::<usize>() as u64);
    assert!(allocate.allocated_bytes >= allocate.self_allocated_bytes);
    assert!(parent.allocated_bytes >= allocate.allocated_bytes);
    assert!(report.allocated_bytes > before_gc.allocated_bytes);
    assert!(report.collections >= 1);
    assert!(!report.gc_time.is_zero());
}

#[test]
fn redefinition_and_saved_old_functions_keep_distinct_profile_ids() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str("(defun profile-version () 1)").unwrap();
    let old = runtime.eval("#'profile-version").unwrap().pop().unwrap();
    runtime.start_profiling(ProfilerConfig::default()).unwrap();
    runtime.call(&old, &[]).unwrap();
    runtime.load_str("(defun profile-version () 2)").unwrap();
    runtime.eval("(profile-version)").unwrap();
    runtime.call(&old, &[]).unwrap();
    let report = runtime.stop_profiling();
    let versions = report
        .functions
        .iter()
        .filter(|row| row.name.contains("PROFILE-VERSION"))
        .collect::<Vec<_>>();
    assert_eq!(versions.len(), 2, "{report:?}");
    assert_ne!(versions[0].id, versions[1].id);
    let mut counts = versions.iter().map(|row| row.calls).collect::<Vec<_>>();
    counts.sort_unstable();
    assert_eq!(counts, [1, 2]);
}

#[test]
fn nonlocal_exit_cleanup_and_error_close_profile_activations() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str("(defvar *profile-cleaned* 0)
                      (defun profile-cleanup () (incf *profile-cleaned*))
                      (defun profile-escape () (unwind-protect (throw 'profile-out 42) (profile-cleanup)))
                      (defun profile-fail () (error \"expected profiler failure\"))
                      (defun profile-after () 9)").unwrap();
    runtime.start_profiling(ProfilerConfig::default()).unwrap();
    assert_eq!(
        runtime
            .eval("(catch 'profile-out (profile-escape))")
            .unwrap()[0]
            .as_fixnum(),
        Some(42)
    );
    assert!(runtime.eval("(profile-fail)").is_err());
    let before = runtime.profile_report();
    runtime.eval("(profile-after)").unwrap();
    let report = runtime.stop_profiling();
    assert_eq!(row(&report, "PROFILE-ESCAPE").calls, 1);
    assert_eq!(row(&report, "PROFILE-CLEANUP").calls, 1);
    assert_eq!(row(&report, "PROFILE-FAIL").calls, 1);
    assert_eq!(row(&report, "PROFILE-AFTER").calls, 1);
    // A failed activation must not keep accumulating after its unwind.
    assert_eq!(
        row(&before, "PROFILE-FAIL").total_time,
        row(&report, "PROFILE-FAIL").total_time
    );
    assert_eq!(
        runtime.eval("*profile-cleaned*").unwrap()[0].as_fixnum(),
        Some(1)
    );
}

#[test]
fn lisp_entry_points_return_structured_reports_and_reset() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defun profile-lisp () (cons 1 2))")
        .unwrap();
    runtime.eval("(zk:start-profiling 1)").unwrap();
    runtime.eval("(profile-lisp)").unwrap();
    assert_eq!(
        runtime.eval("(getf (zk:profile-report) :running)").unwrap()[0].is_t(),
        true
    );
    assert!(
        !runtime
            .eval("(getf (zk:stop-profiling) :functions)")
            .unwrap()[0]
            .is_nil()
    );
    assert_eq!(row(&runtime.profile_report(), "PROFILE-LISP").calls, 1);
    runtime.eval("(zk:reset-profiling)").unwrap();
    assert!(runtime.profile_report().functions.is_empty());
    assert!(
        runtime
            .start_profiling(ProfilerConfig {
                sample_interval: Duration::ZERO
            })
            .is_err()
    );
}

#[test]
fn yielded_tasks_do_not_inherit_another_tasks_allocations() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str("(defun profile-task-wait () (zk-async:yield) 0)
                      (defun profile-task-allocate () (length (loop repeat 500 collect (cons 1 2))))").unwrap();
    runtime.start_profiling(ProfilerConfig::default()).unwrap();
    runtime
        .eval(
            "(zk-async:with-task-group (group)
                    (let ((waiting (zk-async:spawn-task (group) (profile-task-wait)))
                          (allocating (zk-async:spawn-task (group) (profile-task-allocate))))
                      (zk-async:await waiting)
                      (zk-async:await allocating)))",
        )
        .unwrap();
    let report = runtime.stop_profiling();
    let waiting = row(&report, "PROFILE-TASK-WAIT");
    let allocating = row(&report, "PROFILE-TASK-ALLOCATE");
    assert_eq!(waiting.calls, 1);
    assert_eq!(allocating.calls, 1);
    assert!(allocating.allocated_bytes > 8_000, "{report:?}");
    assert!(
        waiting.allocated_bytes < allocating.allocated_bytes / 4,
        "{report:?}"
    );
}

#[test]
fn waiting_evaluations_exclude_host_idle_time_and_host_allocations() {
    let runtime = Runtime::start().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    runtime
        .register_async_function("profile-host-wait", move |_, _, complete| {
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    runtime
        .load_str("(defun profile-paused () (profile-host-wait) 42)")
        .unwrap();
    runtime.start_profiling(ProfilerConfig::default()).unwrap();
    let mut evaluation = runtime.start_eval("(profile-paused)").unwrap();
    loop {
        match evaluation.resume(None).unwrap() {
            zkcl::EvaluationPoll::Runnable => {}
            zkcl::EvaluationPoll::Waiting(None) => break,
            _ => panic!("expected host wait"),
        }
    }
    let before = runtime.profile_report();
    let _host_value = runtime
        .make_string(&"host allocation".repeat(5_000))
        .unwrap();
    let after = runtime.profile_report();
    assert!(after.allocated_bytes > before.allocated_bytes);
    assert_eq!(
        row(&before, "PROFILE-PAUSED").total_time,
        row(&after, "PROFILE-PAUSED").total_time
    );
    assert_eq!(
        row(&before, "PROFILE-PAUSED").allocated_bytes,
        row(&after, "PROFILE-PAUSED").allocated_bytes
    );
    receive.recv().unwrap().complete(|_| Ok(Vec::new()));
    loop {
        match evaluation.resume(None).unwrap() {
            zkcl::EvaluationPoll::Runnable => {}
            zkcl::EvaluationPoll::Ready(values) => {
                assert_eq!(values[0].as_fixnum(), Some(42));
                break;
            }
            _ => panic!("expected resumed completion"),
        }
    }
    assert_eq!(row(&runtime.stop_profiling(), "PROFILE-PAUSED").calls, 1);
}

#[cfg(feature = "jit")]
#[test]
fn installed_native_function_contributes_calls_time_and_allocation() {
    use std::time::Instant;

    let runtime = Runtime::start().unwrap();
    runtime
        // Returning a second value keeps the allocation inside this activation.
        .load_str("(defun profile-native (n) (values (cons n n) n))")
        .unwrap();
    let function = runtime.eval("#'profile-native").unwrap().pop().unwrap();
    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    let backend = zkcl::enable_jit(&runtime).unwrap();
    runtime
        .call(&compile, &[&runtime.nil(), &function])
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.machine_code(&function).unwrap().is_none() {
        assert!(Instant::now() < deadline, "JIT did not install native code");
        std::thread::yield_now();
    }
    assert!(!backend.code_cache().is_empty());
    runtime
        .start_profiling(ProfilerConfig {
            sample_interval: Duration::from_nanos(1),
        })
        .unwrap();
    let argument = runtime.fixnum(7).unwrap();
    for _ in 0..5 {
        runtime.call(&function, &[&argument]).unwrap();
    }
    let report = runtime.stop_profiling();
    let native = row(&report, "PROFILE-NATIVE");
    assert_eq!(native.calls, 5, "{report:?}");
    assert_eq!(
        native.native_calls, 5,
        "expected native entry, got {report:?}"
    );
    assert!(native.self_allocated_bytes > 0);
    assert!(!native.total_time.is_zero());
    assert!(runtime.machine_code(&function).unwrap().is_some());
}
