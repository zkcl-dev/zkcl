#![cfg(not(target_family = "wasm"))]

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Wake, Waker};
use std::thread;
use std::time::Duration;

use zkcl::{
    BlockingPool, ConditionClass, EvalError, EvalErrorKind, Evaluation, EvaluationPoll,
    ExecutionStop, Runtime,
};

#[path = "common/files.rs"]
mod files;

const WAIT: Duration = Duration::from_secs(10);

fn pool(workers: usize, queued: usize) -> BlockingPool {
    BlockingPool::new(
        NonZeroUsize::new(workers).unwrap(),
        NonZeroUsize::new(queued).unwrap(),
    )
    .unwrap()
}

fn slice<'rt>(eval: &mut Evaluation<'rt>, wake: &Waker) -> Result<EvaluationPoll<'rt>, EvalError> {
    for _ in 0..1000 {
        match eval.resume(Some(wake))? {
            EvaluationPoll::Runnable => {}
            other => return Ok(other),
        }
    }
    panic!("evaluation did not wait or complete")
}

fn notifications() -> (Waker, mpsc::Receiver<()>) {
    struct Notify(mpsc::Sender<()>);
    impl Wake for Notify {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(());
        }
    }
    let (send, recv) = mpsc::channel();
    (Waker::from(Arc::new(Notify(send))), recv)
}

#[test]
fn one_lisp_group_can_run_two_isolated_runtimes_in_parallel() {
    let runtime = Runtime::start().unwrap();
    let pool = pool(2, 4);
    let (entered, workers) = mpsc::channel();
    let owner = thread::current().id();
    runtime
        .register_async_function("cpu-job", move |_, args, complete| {
            let input = args[0].as_i64().unwrap();
            let entered = entered.clone();
            pool.try_submit_runtime(
                complete,
                Runtime::builder(),
                move |worker| {
                    worker.register_function("collect-and-wait", move |runtime, _| {
                        let (release, gate) = mpsc::channel();
                        entered.send((thread::current().id(), release))?;
                        gate.recv_timeout(WAIT)?;
                        runtime.collect();
                        Ok(Vec::new())
                    })?;
                    let compute = worker
                        .eval(
                            "(lambda (x) (let ((value (list (* x x))))
                                             (collect-and-wait) (car value)))",
                        )?
                        .remove(0);
                    let argument = worker.integer_from(input)?;
                    Ok(worker.call(&compute, &[&argument])?[0].as_i64().unwrap())
                },
                move |rt, result| {
                    assert_eq!(thread::current().id(), owner);
                    Ok(vec![rt.integer_from(result)?])
                },
            )?;
            Ok(())
        })
        .unwrap();
    let (waker, woke) = notifications();
    let mut eval = runtime
        .start_eval(
            "(zk-async:with-task-group (group)
            (let ((x (zk-async:spawn-task (group) (cpu-job 2)))
                  (y (zk-async:spawn-task (group) (cpu-job 7))))
              (values (zk-async:await x) (zk-async:await y))))",
        )
        .unwrap();
    assert!(matches!(
        slice(&mut eval, &waker).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    let (first, release_first) = workers.recv_timeout(WAIT).unwrap();
    let (second, release_second) = workers.recv_timeout(WAIT).unwrap();
    assert_ne!(first, second);
    assert_ne!(first, owner);
    assert_ne!(second, owner);
    runtime.collect();
    release_first.send(()).unwrap();
    release_second.send(()).unwrap();
    loop {
        match slice(&mut eval, &waker).unwrap() {
            EvaluationPoll::Ready(values) => {
                assert_eq!(values[0].as_i64(), Some(4));
                assert_eq!(values[1].as_i64(), Some(49));
                break;
            }
            EvaluationPoll::Waiting(None) => woke.recv_timeout(WAIT).unwrap(),
            _ => panic!("unexpected evaluation state"),
        }
    }
}

#[test]
fn cancellation_joins_an_isolated_runtime_after_its_lisp_and_rust_cleanup() {
    let runtime = Runtime::start().unwrap();
    let pool = pool(1, 1);
    let (entered, running) = mpsc::channel();
    let cleanups = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let worker_cleanups = Arc::clone(&cleanups);
    runtime
        .register_async_function("run-isolated", move |_, _, completion| {
            let entered = entered.clone();
            let worker_cleanups = Arc::clone(&worker_cleanups);
            pool.try_submit_runtime(
                completion,
                Runtime::builder(),
                move |worker| {
                    let on_drop = Arc::clone(&worker_cleanups);
                    let _release_worker_data = std::mem::DropGuard::new(on_drop, |count| {
                        count.fetch_add(1, Ordering::AcqRel);
                    });
                    worker.register_function("entered", move |_, _| {
                        entered.send(())?;
                        Ok(Vec::new())
                    })?;
                    worker.register_function("worker-cleanup", move |_, _| {
                        assert_eq!(worker_cleanups.fetch_add(1, Ordering::AcqRel), 0);
                        Ok(Vec::new())
                    })?;
                    let error = worker
                        .eval("(unwind-protect (progn (entered) (loop)) (worker-cleanup))")
                        .err()
                        .expect("isolated evaluation must unwind");
                    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
                    Err(error.into())
                },
                |_, ()| panic!("cancelled work must not materialize a result"),
            )?;
            Ok(())
        })
        .unwrap();
    runtime
        .register_function("check-worker-cleanup", move |_, _| {
            assert_eq!(cleanups.fetch_add(1, Ordering::AcqRel), 2);
            Ok(Vec::new())
        })
        .unwrap();
    let (waker, woke) = notifications();
    let mut eval = runtime
        .start_eval("(unwind-protect (run-isolated) (check-worker-cleanup))")
        .unwrap();
    assert!(matches!(
        slice(&mut eval, &waker).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    running.recv_timeout(WAIT).unwrap();
    runtime.cancellation_handle().cancel_with_cleanup();
    loop {
        match slice(&mut eval, &waker) {
            Ok(EvaluationPoll::Waiting(None)) => woke.recv_timeout(WAIT).unwrap(),
            Err(error) => {
                assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
                break;
            }
            _ => panic!("expected cooperative cancellation"),
        }
    }
}

#[test]
fn an_isolated_step_budget_failure_is_an_error_in_the_parent() {
    let runtime = Runtime::start().unwrap();
    let pool = pool(1, 1);
    runtime
        .register_async_function("bounded-isolate", move |_, _, completion| {
            pool.try_submit_runtime(
                completion,
                Runtime::builder().step_budget(Some(256)),
                |runtime| {
                    runtime.eval("(loop)")?;
                    Ok(())
                },
                |_, ()| Ok(Vec::new()),
            )?;
            Ok(())
        })
        .unwrap();
    let error = runtime.eval("(bounded-isolate)").err().unwrap();
    assert_eq!(error.kind(), EvalErrorKind::HostCallback);
    assert!(error.message().contains("step budget exceeded"));
    assert!(runtime.eval("t").unwrap()[0].is_t());
}

#[test]
fn cancelled_queued_work_releases_its_resources_while_the_only_worker_is_busy() {
    struct Lease(Arc<AtomicBool>);
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let pool = pool(1, 1);
    let busy = Runtime::start().unwrap();
    let (entered, started) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let gate = Mutex::new(Some(gate));
    let worker = pool.clone();
    busy.register_async_function("occupy-worker", move |_, _, completion| {
        let entered = entered.clone();
        let gate = gate.lock().unwrap().take().unwrap();
        worker.try_submit(
            completion,
            move |_| {
                entered.send(()).unwrap();
                gate.recv_timeout(WAIT).unwrap();
            },
            |_, ()| Ok(Vec::new()),
        )?;
        Ok(())
    })
    .unwrap();
    let mut busy_eval = busy.start_eval("(occupy-worker)").unwrap();
    let (busy_waker, busy_woke) = notifications();
    assert!(matches!(
        slice(&mut busy_eval, &busy_waker).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    started.recv_timeout(WAIT).unwrap();

    let waiting = Runtime::start().unwrap();
    let released = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&released);
    let worker = pool.clone();
    waiting
        .register_async_function("queued-job", move |_, _, completion| {
            let lease = Lease(Arc::clone(&observed));
            worker.try_submit(
                completion,
                move |_| -> () {
                    drop(lease);
                    panic!("cancelled work started");
                },
                |_, ()| Ok(Vec::new()),
            )?;
            Ok(())
        })
        .unwrap();
    let observed = Arc::clone(&released);
    waiting
        .register_function("check-release", move |_, _| {
            assert!(observed.load(Ordering::Acquire));
            Ok(Vec::new())
        })
        .unwrap();
    let mut queued_eval = waiting
        .start_eval("(unwind-protect (queued-job) (check-release))")
        .unwrap();
    let waker = Waker::noop();
    assert!(matches!(
        slice(&mut queued_eval, waker).unwrap(),
        EvaluationPoll::Waiting(None)
    ));

    let overflow = Runtime::start().unwrap();
    overflow
        .register_async_function("one-too-many", move |_, _, completion| {
            pool.try_submit(completion, |_| (), |_, ()| Ok(Vec::new()))?;
            Ok(())
        })
        .unwrap();
    let error = overflow
        .eval("(one-too-many)")
        .err()
        .expect("queue is full");
    assert_eq!(error.kind(), EvalErrorKind::HostCallback);
    assert!(error.message().contains("blocking work queue is full"));

    let cancel = waiting.cancellation_handle();
    cancel.cancel_with_cleanup();
    let error = slice(&mut queued_eval, waker)
        .err()
        .expect("queued cancellation must finish before a worker is free");
    assert_eq!(error.execution_stop(), Some(ExecutionStop::Cancelled));
    assert!(released.load(Ordering::Acquire));

    let mut admitted = overflow.start_eval("(one-too-many)").unwrap();
    assert!(matches!(
        slice(&mut admitted, waker).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    overflow.cancellation_handle().cancel();
    assert_eq!(
        slice(&mut admitted, waker).err().unwrap().execution_stop(),
        Some(ExecutionStop::Aborted)
    );

    release.send(()).unwrap();
    busy_woke.recv_timeout(WAIT).unwrap();
    assert!(
        matches!(slice(&mut busy_eval, &busy_waker).unwrap(), EvaluationPoll::Ready(values) if values.is_empty())
    );
}

#[test]
fn a_start_error_waits_for_a_provider_that_has_already_started() {
    let runtime = Runtime::start().unwrap();
    let (send, provider) = mpsc::channel();
    runtime
        .register_async_function("start-then-fail", move |_, _, completion| {
            send.send(completion)?;
            Err(std::io::Error::other("startup failed after submission").into())
        })
        .unwrap();
    runtime
        .eval("(defparameter *cleaned-after-start-error* nil)")
        .unwrap();
    let mut eval = runtime
        .start_eval("(unwind-protect (start-then-fail) (setq *cleaned-after-start-error* t))")
        .unwrap();
    let (waker, woke) = notifications();
    assert!(matches!(
        slice(&mut eval, &waker).unwrap(),
        EvaluationPoll::Waiting(None)
    ));
    let complete = provider.try_recv().unwrap();
    assert!(complete.is_cancelled());
    assert!(complete.complete(|_| panic!("startup failed, so its reply must be discarded")));
    woke.recv_timeout(WAIT).unwrap();
    let error = slice(&mut eval, &waker)
        .err()
        .expect("the error from startup must be preserved");
    assert_eq!(error.kind(), EvalErrorKind::HostCallback);
    assert!(error.message().contains("startup failed after submission"));
    assert!(runtime.eval("*cleaned-after-start-error*").unwrap()[0].is_t());
}

#[test]
fn async_file_reads_ranges_and_reports_a_file_condition() {
    let dir = files::TempDir::new("async-octets");
    let bytes: Vec<u8> = (0..=255).cycle().take(512 * 1024).collect();
    let path = dir.write("input.bin", &bytes);
    let runtime = Runtime::start().unwrap();
    let reader = runtime
        .eval("#'zk-async:read-file-octets")
        .unwrap()
        .remove(0);
    let name = runtime.make_string(path.to_str().unwrap()).unwrap();
    let keywords = runtime.eval("(values :start :end)").unwrap();
    let start = runtime.integer_from(12345).unwrap();
    let end = runtime.integer_from(321234).unwrap();
    let result = runtime
        .call(&reader, &[&name, &keywords[0], &start, &keywords[1], &end])
        .unwrap();
    assert_eq!(runtime.octets(&result[0]).unwrap(), bytes[12345..321234]);

    let result = runtime.call(&reader, &[&name]).unwrap();
    assert_eq!(runtime.octets(&result[0]).unwrap(), bytes);
    let empty = runtime
        .call(&reader, &[&name, &keywords[0], &end, &keywords[1], &end])
        .unwrap();
    assert!(runtime.octets(&empty[0]).unwrap().is_empty());

    let error = runtime
        .call(&reader, &[&name, &keywords[0], &end, &keywords[1], &start])
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ProgramError)
    );
    let missing = dir.path().join("missing.bin");
    let name = runtime.make_string(missing.to_str().unwrap()).unwrap();
    let handler = runtime
        .eval(
            "(lambda (path)
           (handler-case (zk-async:read-file-octets path)
             (file-error (condition) (namestring (file-error-pathname condition)))))",
        )
        .unwrap()
        .remove(0);
    let result = runtime.call(&handler, &[&name]).unwrap();
    assert_eq!(runtime.string(&result[0]).as_deref(), missing.to_str());
}
