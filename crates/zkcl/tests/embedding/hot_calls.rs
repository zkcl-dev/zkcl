use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "jit")]
use std::time::Duration;

use super::CountedForeignResource;
use zkcl::{CancellationHandle, EvalErrorKind, ForeignBorrowError, HostBorrowError, Runtime};

struct WorldState {
    score: isize,
    log: Vec<String>,
}

fn runtime_with_hot_calls() -> (Runtime, CancellationHandle) {
    let cancel = CancellationHandle::new();
    let runtime = Runtime::builder()
        .heap_size_bytes(32 << 20)
        .jit_threshold(1)
        .cancellation(cancel.clone())
        .start()
        .expect("start configured runtime");

    #[cfg(feature = "jit")]
    zkcl::enable_jit(&runtime).expect("enable Cranelift JIT");

    (runtime, cancel)
}

#[test]
fn lisp_callbacks_can_modify_host_state_only_in_a_mutable_scope() {
    let (runtime, _) = runtime_with_hot_calls();
    runtime
        .register_function("world-score", |rt, _args| {
            let score = rt
                .try_with_host_ref::<WorldState, _>(|w| w.score)
                .map_err(|e| format!("host ref failed: {e}"))?;
            Ok(vec![rt.fixnum(score).ok_or("score out of fixnum range")?])
        })
        .unwrap();

    runtime
        .register_function("(setf world-score)", |rt, args| {
            let new_val = args
                .first()
                .and_then(|v| v.as_fixnum())
                .ok_or("expected fixnum score")?;
            rt.try_with_host_mut::<WorldState, _>(|w| {
                w.score = new_val;
                w.log.push(format!("score={new_val}"));
            })
            .map_err(|e| format!("host mut failed: {e}"))?;
            Ok(vec![args[0].clone()])
        })
        .unwrap();

    runtime
        .register_function("check-reentrant-host-borrow", |rt, _args| {
            rt.try_with_host_mut::<WorldState, _>(|_| {
                let nested = rt.try_with_host_mut::<WorldState, _>(|_| ());
                assert_eq!(
                    nested,
                    Err(HostBorrowError::AlreadyBorrowed(std::any::type_name::<
                        WorldState,
                    >()))
                );
            })
            .map_err(|e| format!("{e}"))?;
            Ok(vec![rt.t()])
        })
        .unwrap();

    // A missing dynamic host binding reaches Lisp as a callback error.
    let err = runtime.eval("(world-score)").err().unwrap();
    assert_eq!(err.kind(), EvalErrorKind::HostCallback);

    let mut world = WorldState {
        score: 10,
        log: Vec::new(),
    };
    runtime.with_host_mut(&mut world, || {
        let updated = runtime
            .eval("(progn (setf (world-score) (+ (world-score) 32)) (world-score))")
            .unwrap();
        assert_eq!(updated[0].as_fixnum(), Some(42));
        let reentrant = runtime.eval("(check-reentrant-host-borrow)").unwrap();
        assert!(reentrant[0].is_t());
    });
    assert_eq!(world.score, 42);
    assert_eq!(world.log, vec!["score=42"]);
    assert!(matches!(
        runtime.try_with_host_mut::<WorldState, _>(|_| ()),
        Err(HostBorrowError::Missing(_))
    ));

    // Shared immutable scope rejects `try_with_host_mut`.
    runtime.with_host_ref(&world, || {
        assert!(matches!(
            runtime.try_with_host_mut::<WorldState, _>(|_| ()),
            Err(HostBorrowError::MutableRequired(_))
        ));
        let v = runtime.eval("(world-score)").unwrap();
        assert_eq!(v[0].as_fixnum(), Some(42));
    });

    // Panic inside `with_host_mut` still pops scope cleanly via `DropGuard`.
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.with_host_mut(&mut world, || {
            panic!("simulated host panic");
        });
    }));
    assert!(unwind.is_err());
    assert!(matches!(
        runtime.try_with_host_ref::<WorldState, _>(|_| ()),
        Err(HostBorrowError::Missing(_))
    ));

    assert!(
        runtime
            .register_function("(not-setf foo)", |_rt, _args| Ok(vec![]))
            .is_err()
    );
}

#[test]
fn foreign_storage_is_released_after_its_last_lisp_reference_dies() {
    let (runtime, _) = runtime_with_hot_calls();
    let drops = Arc::new(AtomicUsize::new(0));
    let foreign_obj = runtime
        .foreign(CountedForeignResource {
            id: 77,
            drops: drops.clone(),
        })
        .unwrap();
    assert!(foreign_obj.is_foreign(&runtime));
    assert!(
        runtime
            .repr(&foreign_obj)
            .unwrap()
            .contains("FOREIGN-OBJECT")
    );
    assert_eq!(
        foreign_obj
            .try_with_foreign_ref::<CountedForeignResource, _>(&runtime, |r| r.id)
            .unwrap(),
        77
    );
    assert!(matches!(
        foreign_obj.try_with_foreign_ref::<WorldState, _>(&runtime, |_| ()),
        Err(ForeignBorrowError::TypeMismatch { .. })
    ));
    foreign_obj
        .try_with_foreign_mut::<CountedForeignResource, _>(&runtime, |r| {
            let nested =
                foreign_obj.try_with_foreign_mut::<CountedForeignResource, _>(&runtime, |_| ());
            assert!(matches!(
                nested,
                Err(ForeignBorrowError::AlreadyBorrowed(_))
            ));
            r.id = 99;
        })
        .unwrap();
    assert_eq!(
        foreign_obj
            .try_with_foreign_ref::<CountedForeignResource, _>(&runtime, |r| r.id)
            .unwrap(),
        99
    );

    runtime
        .load_str("(defparameter *held-foreign* nil)")
        .unwrap();
    runtime
        .load_str("(defun stash-foreign (x) (setq *held-foreign* x) x)")
        .unwrap();
    let stash = runtime.eval("#'stash-foreign").unwrap().pop().unwrap();
    let kept = runtime.call(&stash, &[&foreign_obj]).unwrap();
    assert!(kept[0].same_identity(&foreign_obj));
    drop(kept);
    drop(foreign_obj);

    // Still reachable from `*held-foreign*`: GC must keep it alive.
    runtime.collect();
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    // Clear global binding -> GC reclaims and mutator drops the Rust value cleanly.
    runtime.load_str("(setq *held-foreign* nil)").unwrap();
    runtime.collect();
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    // Non-foreign value returns `NotForeign`, and `as_string` convenience works.
    let s = runtime.eval("\"hello-zkcl\"").unwrap().pop().unwrap();
    assert_eq!(s.as_string(&runtime).as_deref(), Some("hello-zkcl"));
    assert!(matches!(
        s.try_with_foreign_ref::<CountedForeignResource, _>(&runtime, |_| ()),
        Err(ForeignBorrowError::NotForeign)
    ));
}

#[test]
fn step_budget_interrupts_an_infinite_loop_inside_ignore_errors() {
    let (runtime, _) = runtime_with_hot_calls();
    runtime
        .load_str(
            "(defun hot-infinite ()
               (ignore-errors
                 (let ((i 0))
                   (loop (setq i (1+ i))))))",
        )
        .unwrap();
    runtime.set_step_budget(Some(200));
    for _ in 0..4 {
        let loop_fn = runtime.eval("#'hot-infinite").unwrap().pop().unwrap();
        let err = runtime.call(&loop_fn, &[]).err().unwrap();
        assert_eq!(
            err.kind(),
            EvalErrorKind::Cancelled,
            "unexpected loop abort kind: {err}"
        );
    }
    runtime.set_step_budget(None);
}

#[cfg(feature = "jit")]
#[test]
fn hot_function_keeps_multiple_values_through_cleanup_collection() {
    let (runtime, _) = runtime_with_hot_calls();
    runtime
        .register_function("collect-garbage-now", |rt, _args| {
            rt.collect();
            Ok(vec![rt.t()])
        })
        .unwrap();

    runtime
        .load_str(
            "(defun jit-complex (a &optional (b (progn (collect-garbage-now) 10)))
               (let ((cleanup-ran nil)
                     (caught nil))
                 (setq caught
                       (handler-case
                           (multiple-value-call
                               (lambda (x y z) (+ x y z))
                             (unwind-protect
                                 (values a b 5)
                               (collect-garbage-now)
                               (setq cleanup-ran t)))
                         (error () -1)))
                 (list caught cleanup-ran (+ most-positive-fixnum a))))",
        )
        .unwrap();
    let f = runtime.eval("#'jit-complex").unwrap().pop().unwrap();
    // Run multiple times so `jit_threshold = 1` compiles and executes via Cranelift JIT.
    for _ in 0..4 {
        let arg = runtime.fixnum(7).unwrap();
        let res = runtime.call(&f, &[&arg]).unwrap();
        assert_eq!(
            runtime.repr(&res[0]).unwrap(),
            format!("(22 T {})", (isize::MAX >> 3) + 7)
        );
    }
}

#[cfg(feature = "jit")]
#[test]
fn a_hot_loop_can_be_called_again_after_cancellation() {
    let (runtime, cancel) = runtime_with_hot_calls();
    runtime
        .load_str(
            "(defun jit-count-until-cancel (n)
               (let ((i 0))
                 (loop
                   (if (= i n) (return i))
                   (setq i (+ i 1)))))",
        )
        .unwrap();
    let bounded = runtime.fixnum(5).unwrap();
    let count_fn = runtime
        .eval("#'jit-count-until-cancel")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        runtime.call(&count_fn, &[&bounded]).unwrap()[0].as_fixnum(),
        Some(5)
    );
    // Pump JIT compilation completion so `count_fn` is running native Cranelift machine code.
    for _ in 0..50 {
        if runtime.poll_jit_completions() > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let bg_cancel = cancel.clone();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(15));
        bg_cancel.cancel();
    });
    let huge = runtime.fixnum(isize::MAX >> 4).unwrap();
    let cancelled = runtime.call(&count_fn, &[&huge]).err().unwrap();
    handle.join().unwrap();
    assert_eq!(cancelled.kind(), EvalErrorKind::Cancelled);
    cancel.reset();
    assert_eq!(
        runtime.call(&count_fn, &[&bounded]).unwrap()[0].as_fixnum(),
        Some(5)
    );
}
