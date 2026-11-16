use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use zkcl::Runtime;

#[test]
fn finalizers_run_once_when_the_target_loses_its_last_reference() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defvar *sum* 0)
         (defvar *captured* nil)
         (defun delta (value) (lambda () (incf *sum* (car value))))
         (defun install (object)
           (zk:finalize object (delta (list 2)))
           (zk:finalize object (delta (list 3))))
         (defun capture (object) (lambda () (setq *captured* object)))",
    )
    .unwrap();
    let retained = rt.eval("(install (list :live))").unwrap();
    rt.collect();
    assert_eq!(rt.eval("*sum*").unwrap()[0].as_fixnum(), Some(0));
    drop(retained);
    rt.collect();
    assert_eq!(rt.eval("*sum*").unwrap()[0].as_fixnum(), Some(5));
    rt.eval("(zk:gc)").unwrap();
    assert_eq!(rt.eval("*sum*").unwrap()[0].as_fixnum(), Some(5));

    rt.load_str(
        "(defvar *object* (list 1))
         (install *object*)
         (zk:cancel-finalization *object*)
         (zk:finalize *object* (delta (list 7)))
         (setq *object* nil)
         (zk:gc)",
    )
    .unwrap();
    assert_eq!(rt.eval("*sum*").unwrap()[0].as_fixnum(), Some(12));

    rt.load_str(
        "(defvar *weak* nil)
         (defun install-capture ()
           (let ((object (list :captured)))
             (setq *weak* (zk:make-weak-pointer object))
             (zk:finalize object (capture object)))
           nil)
         (install-capture)
         (zk:gc)",
    )
    .unwrap();
    assert!(
        !rt.eval("(and (zk:weak-pointer-value *weak*) (null *captured*))")
            .unwrap()[0]
            .is_nil()
    );
    rt.load_str("(zk:cancel-finalization (zk:weak-pointer-value *weak*)) (zk:gc)")
        .unwrap();
    assert!(rt.eval("(zk:weak-pointer-value *weak*)").unwrap()[0].is_nil());
}

#[test]
fn pending_finalizers_run_once_even_if_a_callback_fails() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defvar *runs* 0)
         (defun record-run () (incf *runs*) (zk:gc))
         (defun fail-once ()
           (unwind-protect (throw 'missing :value) (incf *runs* 10)))
         (defun install ()
           (let ((object (list :target)))
             (zk:finalize object #'record-run)
             (zk:finalize object #'fail-once))
           nil)
         (install)",
    )
    .unwrap();
    assert!(rt.eval("(zk:gc)").is_err());
    assert_eq!(rt.eval("*runs*").unwrap()[0].as_fixnum(), Some(11));
    rt.eval("(zk:gc)").unwrap();
    assert_eq!(rt.eval("*runs*").unwrap()[0].as_fixnum(), Some(11));
}

#[test]
fn failed_finalizers_cannot_starve_callbacks_from_an_earlier_collection() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defvar *events* nil)
         (defvar *target* (list :first))
         (defun fail-later () (push :new *events*) (error \"new failure\"))
         (defun arm-next () (zk:finalize (list :next) #'fail-later) nil)
         (defun fail-and-collect ()
           (push :first *events*)
           (arm-next)
           (zk:gc)
           (error \"first failure\"))
         (zk:finalize *target* (lambda () (push :older *events*)))
         (zk:finalize *target* #'fail-and-collect)",
    )
    .unwrap();
    assert!(rt.eval("(progn (setq *target* nil) (zk:gc))").is_err());
    rt.collect();
    assert!(rt.eval("nil").is_err());
    let events = rt.eval("(reverse *events*)").unwrap();
    assert_eq!(rt.repr(&events[0]).unwrap(), "(:FIRST :OLDER :NEW)");
}

#[test]
fn dropping_a_runtime_releases_registrations_without_entering_lisp() {
    let ran = Arc::new(AtomicBool::new(false));
    {
        let rt = Runtime::start().unwrap();
        let callback = ran.clone();
        rt.register_function("RECORD-FINALIZER", move |_, _| {
            callback.store(true, Ordering::Relaxed);
            Ok(vec![])
        })
        .unwrap();
        rt.load_str("(defvar *live* (list 1)) (zk:finalize *live* #'record-finalizer)")
            .unwrap();
    }
    assert!(!ran.load(Ordering::Relaxed));
    assert_eq!(Arc::strong_count(&ran), 1);
}

#[test]
fn an_allocation_triggered_collection_schedules_callbacks_at_host_entry() {
    let rt = Runtime::builder()
        .heap_size_bytes(16 * 1024 * 1024)
        .start()
        .unwrap();
    rt.load_str(
        "(defvar *calls* 0)
         (defvar *last* nil)
         (defun install () (zk:finalize (list 1) (lambda () (incf *calls*))) nil)
         (defun churn () (dotimes (i 30000) (setq *last* (cons i nil))))
         (install)",
    )
    .unwrap();
    let before = rt.heap_stats().collections;
    for _ in 0..64 {
        rt.eval("(churn)").unwrap();
        if rt.heap_stats().collections > before {
            break;
        }
    }
    assert!(rt.heap_stats().collections > before);
    assert_eq!(rt.eval("*calls*").unwrap()[0].as_fixnum(), Some(1));
}

#[test]
fn host_cancellation_before_delivery_preserves_pending_callbacks() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defvar *ran* 0)
         (defun install () (zk:finalize (list :dead) (lambda () (incf *ran*))) nil)
         (install)",
    )
    .unwrap();
    rt.collect();
    let stop = rt.cancellation_handle();
    stop.cancel();
    assert!(rt.eval("*ran*").is_err());
    stop.reset();
    assert_eq!(rt.eval("*ran*").unwrap()[0].as_fixnum(), Some(1));
}

#[test]
fn a_value_retained_by_a_live_weak_key_is_finalized_after_the_key_dies() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defvar *key* (list :keep))
         (defvar *table* (make-hash-table :weakness :key :test #'eq))
         (defvar *calls* 0)
         (defun install ()
           (setf (gethash *key* *table*)
                 (zk:finalize (list :value) (lambda () (incf *calls*))))
           nil)
         (install)
         (zk:gc)",
    )
    .unwrap();
    assert_eq!(rt.eval("*calls*").unwrap()[0].as_fixnum(), Some(0));
    rt.load_str("(setq *key* nil) (zk:gc)").unwrap();
    assert_eq!(
        rt.eval("(hash-table-count *table*)").unwrap()[0].as_fixnum(),
        Some(0)
    );
    assert_eq!(rt.eval("*calls*").unwrap()[0].as_fixnum(), Some(1));
}
