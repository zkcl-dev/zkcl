use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::task::{Wake, Waker};
use std::time::Duration;

use zkcl::{EvalError, Evaluation, EvaluationPoll, ExecutionStop, HostServices, Root, Runtime};

#[derive(Clone, Default)]
struct Clock(Arc<AtomicU64>);

impl HostServices for Clock {
    fn monotonic_nanos(&self) -> io::Result<u64> {
        Ok(self.0.load(Ordering::Relaxed))
    }

    fn wall_time_secs(&self) -> io::Result<i64> {
        Ok(0)
    }

    fn fill_entropy(&self, bytes: &mut [u8]) -> io::Result<()> {
        bytes.fill(0);
        Ok(())
    }

    fn sleep(&self, _: Duration) -> io::Result<()> {
        panic!("incremental evaluation must return to the host")
    }
}

fn next<'rt>(
    evaluation: &mut Evaluation<'rt>,
    waker: Option<&Waker>,
) -> Result<EvaluationPoll<'rt>, EvalError> {
    for _ in 0..1_000 {
        match evaluation.resume(waker)? {
            EvaluationPoll::Runnable => {}
            other => return Ok(other),
        }
    }
    panic!("evaluation did not reach a wait or finish")
}

fn ready<'rt>(result: Result<EvaluationPoll<'rt>, EvalError>) -> Vec<Root<'rt>> {
    match result.unwrap() {
        EvaluationPoll::Ready(values) => values,
        _ => panic!("expected completed evaluation"),
    }
}

fn wake_channel() -> (Waker, Receiver<()>) {
    struct Signal(Sender<()>);

    impl Wake for Signal {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }
    }

    let (send, recv) = mpsc::channel();
    (Waker::from(Arc::new(Signal(send))), recv)
}

#[test]
fn a_timer_can_be_driven_with_gc_between_slices() {
    let clock = Clock::default();
    let mut runtime = Runtime::start().unwrap();
    runtime.set_host_services(clock.clone());
    runtime.eval("(defparameter *scope* :outside)").unwrap();
    let mut evaluation = runtime
        .start_eval("(let ((*scope* (list 41))) (sleep 5) (values (car *scope*) 42))")
        .unwrap();
    assert!(
        matches!(next(&mut evaluation, None).unwrap(), EvaluationPoll::Waiting(Some(delay)) if delay == Duration::from_secs(5))
    );
    assert!(runtime.eval("*scope*").is_err());
    assert!(runtime.start_eval("*scope*").is_err());
    runtime.collect();
    clock.0.store(4_000_000_000, Ordering::Relaxed);
    assert!(
        matches!(next(&mut evaluation, None).unwrap(), EvaluationPoll::Waiting(Some(delay)) if delay == Duration::from_secs(1))
    );
    clock.0.store(5_000_000_000, Ordering::Relaxed);
    let values = ready(next(&mut evaluation, None));
    assert_eq!(values.len(), 2);
    assert_eq!(values[0].as_fixnum(), Some(41));
    assert_eq!(values[1].as_fixnum(), Some(42));
    assert!(runtime.eval("(eq *scope* :outside)").unwrap()[0].is_t());
}

#[test]
fn one_cpu_task_returns_to_the_host_and_keeps_its_budget() {
    let runtime = Runtime::start().unwrap();
    runtime.eval("(values)").unwrap();
    runtime.set_step_budget(Some(256));
    let mut evaluation = runtime.start_eval("(loop)").unwrap();
    assert!(matches!(
        evaluation.resume(None).unwrap(),
        EvaluationPoll::Runnable
    ));
    let Err(error) = next(&mut evaluation, None) else {
        panic!("expected to exhaust the invocation budget");
    };
    assert_eq!(error.execution_stop(), Some(ExecutionStop::BudgetExhausted));
    assert!(runtime.eval("t").unwrap()[0].is_t());
}

#[cfg(feature = "jit")]
#[test]
fn a_precompiled_cpu_loop_returns_to_the_incremental_host() {
    use std::time::Instant;

    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    let backend = zkcl::enable_jit(&runtime).unwrap();
    runtime
        .load_str(
            "(defun host-count ()
               (let ((total 0))
                 (dotimes (i 2000 total) (incf total i))))
             (compile 'host-count #'host-count)",
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.poll_jit_completions() == 0 {
        assert!(Instant::now() < deadline, "counting loop did not compile");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!backend.code_cache().is_empty());
    let function = runtime.eval("#'host-count").unwrap().remove(0);
    let mut evaluation = runtime.start_call(&function, &[]).unwrap();
    assert!(matches!(
        evaluation.resume(None).unwrap(),
        EvaluationPoll::Runnable
    ));
    assert_eq!(
        ready(next(&mut evaluation, None))[0].as_fixnum(),
        Some(1_999_000)
    );
}

#[cfg(feature = "jit")]
#[test]
fn an_infinite_native_loop_hands_off_before_the_execution_budget_is_exhausted() {
    use std::time::Instant;

    let clock = Clock::default();
    let mut runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    runtime.set_host_services(clock.clone());
    let backend = zkcl::enable_jit(&runtime).unwrap();
    runtime
        .load_str(
            "(defun spin-native (finish) (if finish (progn (sleep 1) t) (loop)))
                   (defun spin-tail () (spin-tail))
                   (compile 'spin-native #'spin-native)
                   (compile 'spin-tail #'spin-tail)",
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while backend.code_cache().len() < 2 {
        runtime.poll_jit_completions();
        assert!(Instant::now() < deadline, "infinite loop did not compile");
        std::thread::sleep(Duration::from_millis(2));
    }
    runtime.poll_jit_completions();
    {
        let function = runtime.eval("#'spin-native").unwrap().remove(0);
        let truth = runtime.eval("t").unwrap().remove(0);
        let mut evaluation = runtime.start_call(&function, &[&truth]).unwrap();
        assert!(
            matches!(next(&mut evaluation, None).unwrap(), EvaluationPoll::Waiting(Some(delay)) if delay == Duration::from_secs(1))
        );
        clock.0.store(1_000_000_000, Ordering::Relaxed);
        assert!(ready(next(&mut evaluation, None))[0].is_t());
    }
    for name in ["spin-native", "spin-tail"] {
        let function = runtime.eval(&format!("#'{name}")).unwrap().remove(0);
        runtime.set_step_budget(Some(512));
        let nil = runtime.nil();
        let arguments: &[&Root<'_>] = if name == "spin-native" { &[&nil] } else { &[] };
        let mut evaluation = runtime.start_call(&function, arguments).unwrap();
        #[cfg(feature = "vm-metrics")]
        let before = zkcl::metrics::snapshot().native_checkpoints;
        assert!(
            matches!(evaluation.resume(None).unwrap(), EvaluationPoll::Runnable),
            "{name}"
        );
        #[cfg(feature = "vm-metrics")]
        assert!(
            zkcl::metrics::snapshot().native_checkpoints > before,
            "{name}"
        );
        let Err(error) = next(&mut evaluation, None) else {
            panic!("expected a budget stop in {name}")
        };
        assert_eq!(error.execution_stop(), Some(ExecutionStop::BudgetExhausted));
    }
}

#[test]
fn early_completion_uses_owned_call_arguments_and_preserves_value_counts() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_async_function("host-echo", |runtime, args, complete| {
            let owned = runtime.string(&args[0]).unwrap();
            assert!(complete.complete(move |runtime| {
                Ok(vec![
                    runtime.make_string(&owned)?,
                    runtime.integer_from(42)?,
                ])
            }));
            Ok(())
        })
        .unwrap();
    let target = runtime
        .lookup_symbol("CL-USER", "HOST-ECHO")
        .unwrap()
        .unwrap();
    let argument = runtime.make_string("owned").unwrap();
    let mut evaluation = runtime.start_call(&target, &[&argument]).unwrap();
    drop(argument);
    drop(target);
    runtime.collect();
    let result = ready(next(&mut evaluation, None));
    assert_eq!(runtime.string(&result[0]).as_deref(), Some("owned"));
    assert_eq!(result[1].as_fixnum(), Some(42));
    runtime
        .register_async_function("host-no-values", |_, _, complete| {
            complete.complete(|_| Ok(Vec::new()));
            Ok(())
        })
        .unwrap();
    assert!(runtime.eval("(host-no-values)").unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn a_socket_completion_wakes_the_owning_event_loop() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let runtime = Runtime::start().unwrap();
    let (mut read, mut write) = UnixStream::pair().unwrap();
    let (send, recv) = mpsc::channel::<zkcl::HostCompletion>();
    runtime
        .register_async_function("host-read", move |_, _, complete| {
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    let owner = std::thread::current().id();
    let worker = std::thread::spawn(move || {
        let complete = recv.recv().unwrap();
        let mut byte = [0];
        read.read_exact(&mut byte).unwrap();
        assert!(complete.complete(move |runtime| {
            assert_eq!(owner, std::thread::current().id());
            Ok(vec![runtime.integer_from(byte[0])?])
        }));
    });
    let (waker, woke) = wake_channel();
    let mut evaluation = runtime.start_eval("(+ 1 (host-read))").unwrap();
    assert!(matches!(
        next(&mut evaluation, Some(&waker)).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    write.write_all(&[41]).unwrap();
    woke.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        ready(next(&mut evaluation, Some(&waker)))[0].as_fixnum(),
        Some(42)
    );
    worker.join().unwrap();
}

#[test]
fn cancel_awaits_the_provider_before_cleanup_can_start_another_operation() {
    let runtime = Runtime::start().unwrap();
    let (send, recv) = mpsc::channel();
    runtime
        .register_async_function("host-request", move |_, _, complete| {
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    runtime.eval("(defparameter *cleaned* nil)").unwrap();
    let (waker, woke) = wake_channel();
    let mut evaluation = runtime
        .start_eval("(unwind-protect (host-request) (setq *cleaned* (host-request)))")
        .unwrap();
    assert!(matches!(
        next(&mut evaluation, Some(&waker)).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    let old = recv.try_recv().unwrap();
    let cancel = runtime.cancellation_handle();
    cancel.cancel_with_cleanup();
    woke.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
        next(&mut evaluation, None).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    assert!(old.is_cancelled());
    assert!(
        recv.try_recv().is_err(),
        "cleanup must wait for the provider"
    );
    assert!(old.complete(|_| panic!("a cancelled operation's result was materialized")));
    assert!(matches!(
        next(&mut evaluation, None).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    let cleanup = recv.try_recv().unwrap();
    assert!(cleanup.complete(|runtime| Ok(vec![runtime.integer_from(42)?])));
    let Err(error) = next(&mut evaluation, None) else {
        panic!("expected cancellation")
    };
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
    cancel.reset();
    assert_eq!(runtime.eval("*cleaned*").unwrap()[0].as_fixnum(), Some(42));
}

#[test]
fn a_task_group_waits_for_cancelled_host_work_to_finish() {
    let runtime = Runtime::start().unwrap();
    let (send, recv) = mpsc::channel();
    runtime
        .register_async_function("group-host-work", move |_, _, complete| {
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    runtime.eval("(defvar *group-host-cleaned* nil)").unwrap();
    let mut evaluation = runtime
        .start_eval(
            "(zk-async:with-task-group (group)
           (zk-async:spawn-task (group :no-wait t)
             (unwind-protect (group-host-work) (setq *group-host-cleaned* t)))
           (zk-async:yield)
           :drained)",
        )
        .unwrap();
    assert!(matches!(
        next(&mut evaluation, None).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    let completion = recv.try_recv().unwrap();
    assert!(completion.is_cancelled());
    drop(completion);
    let values = ready(next(&mut evaluation, None));
    assert_eq!(runtime.repr(&values[0]).unwrap(), ":DRAINED");
    assert!(runtime.eval("*group-host-cleaned*").unwrap()[0].is_t());
}

#[test]
fn cancellation_requested_in_provider_start_reaches_the_wait_registration() {
    let runtime = Runtime::start().unwrap();
    let (send, recv) = mpsc::channel();
    runtime
        .register_async_function("cancel-before-park", move |rt, _, complete| {
            assert!(rt.eval("(zk-async:cancel-task (zk-async:current-task))")?[0].is_t());
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    let mut evaluation = runtime.start_eval("(cancel-before-park)").unwrap();
    assert!(matches!(
        next(&mut evaluation, None).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    let completion = recv.try_recv().unwrap();
    assert!(completion.is_cancelled());
    drop(completion);
    let Err(error) = next(&mut evaluation, None) else {
        panic!("expected cancellation after provider finished");
    };
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
}

#[test]
fn hard_abort_releases_a_host_wait_without_materializing_a_late_reply() {
    let runtime = Runtime::start().unwrap();
    let (send, recv) = mpsc::channel();
    runtime
        .register_async_function("pending-host-work", move |_, _, complete| {
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    let mut evaluation = runtime.start_eval("(pending-host-work)").unwrap();
    assert!(matches!(
        next(&mut evaluation, None).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    let completion = recv.try_recv().unwrap();
    runtime.cancellation_handle().cancel();
    let Err(error) = next(&mut evaluation, None) else {
        panic!("expected a hard abort without provider acknowledgement");
    };
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Aborted));
    assert!(completion.is_cancelled());
    assert!(!completion.complete(|_| panic!("an aborted operation's reply ran")));
}

#[test]
fn blocking_calls_wait_for_the_same_completion_protocol() {
    let runtime = Runtime::start().unwrap();
    let (send, recv) = mpsc::channel::<zkcl::HostCompletion>();
    let worker = std::thread::spawn(move || {
        let complete = recv.recv().unwrap();
        std::thread::sleep(Duration::from_millis(10));
        assert!(complete.complete(|runtime| Ok(vec![runtime.integer_from(42)?])));
    });
    runtime
        .register_async_function("host-reply", move |_, _, complete| {
            send.send(complete)?;
            Ok(())
        })
        .unwrap();
    let target = runtime
        .lookup_symbol("CL-USER", "HOST-REPLY")
        .unwrap()
        .unwrap();
    assert_eq!(runtime.call(&target, &[]).unwrap()[0].as_fixnum(), Some(42));
    worker.join().unwrap();
}
