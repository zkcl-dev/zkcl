#[path = "embedding/hot_calls.rs"]
mod hot_calls;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use zkcl::{EvalErrorKind, ForeignBorrowError, HostBorrowError, Runtime};

struct CountedForeignResource {
    id: usize,
    drops: Arc<AtomicUsize>,
}

impl Drop for CountedForeignResource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn a_first_call_budget_exhaustion_leaves_builtins_installed() {
    let rt = Runtime::builder().step_budget(Some(5)).start().unwrap();
    let error = rt.eval("(loop)").err().unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);
    rt.set_step_budget(None);
    assert_eq!(
        rt.eval("(zk:match 42 (n n))").unwrap()[0].as_fixnum(),
        Some(42)
    );
}

#[test]
fn persistent_function_handles_remain_bound_to_their_original_runtime() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defpackage :service (:use :cl) (:nicknames :svc)) (defun service::|Mixed name| () 1)",
    )
    .unwrap();
    let symbol = rt
        .lookup_symbol("SVC", "Mixed name")
        .unwrap()
        .unwrap()
        .into_persistent();
    let original = rt
        .resolve_function(&symbol.bind(&rt).unwrap())
        .unwrap()
        .into_persistent();
    let immediate = rt.nil().into_persistent();
    let rt = Box::new(rt);
    rt.collect();
    assert!(rt.lookup_symbol("SVC", "mixed name").unwrap().is_none());
    assert!(rt.lookup_symbol("unknown-package", "X").is_err());
    assert!(rt.lookup_symbol("SVC", "NIL").unwrap().unwrap().is_nil());
    rt.load_str("(defun service::|Mixed name| () 2)").unwrap();
    assert_eq!(
        rt.call(&symbol.bind(&rt).unwrap(), &[]).unwrap()[0].as_fixnum(),
        Some(2)
    );
    assert_eq!(
        rt.call(&original.bind(&rt).unwrap(), &[]).unwrap()[0].as_fixnum(),
        Some(1)
    );
    let other = Runtime::start().unwrap();
    assert!(symbol.bind(&other).is_none());
    drop(rt);
    assert!(original.bind(&other).is_none());
    assert!(immediate.bind(&Runtime::start().unwrap()).is_none());
}

#[test]
fn negative_zero_round_trips_in_its_original_float_format() {
    let rt = Runtime::start().unwrap();
    let single = rt.make_single_float(-0.0).unwrap();
    let double = rt.make_double_float(-0.0).unwrap();
    rt.collect();
    assert_eq!(
        single.as_single_float().unwrap().to_bits(),
        (-0.0f32).to_bits()
    );
    assert_eq!(
        double.as_double_float().unwrap().to_bits(),
        (-0.0f64).to_bits()
    );
    assert!(single.as_double_float().is_none());
    assert!(double.as_single_float().is_none());
}

#[test]
fn reentrant_host_calls_return_the_original_rooted_argument() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defun lisp-step (x) (values (+ 1 (car x)) x))")
        .unwrap();
    rt.register_function("rust-step", |rt, args| {
        let [arg] = args else {
            return Err("rust-step expects one argument".into());
        };
        rt.collect();
        let lisp = rt.eval("#'lisp-step")?.pop().unwrap();
        let mut results = rt.call(&lisp, &[arg])?;
        results.push(arg.clone());
        Ok(results)
    })
    .unwrap();

    let results = rt.eval("(rust-step (list 41))").unwrap();
    assert_eq!(results[0].as_fixnum(), Some(42));
    assert!(results[1].same_identity(&results[2]));
    assert_eq!(
        rt.eval("(+ 2 (rust-step (list 39)))").unwrap()[0].as_fixnum(),
        Some(42)
    );
    let error = rt.eval("(progn (values) (rust-step))").err().unwrap();
    assert!(error.to_string().contains("rust-step expects one argument"));
    assert_eq!(error.offset(), 17);

    let retained = rt.eval("#'rust-step").unwrap().pop().unwrap();
    rt.register_function("rust-step", |rt, _| Ok(vec![rt.fixnum(99).unwrap()]))
        .unwrap();
    assert_eq!(
        rt.eval("(funcall 'rust-step nil)").unwrap()[0].as_fixnum(),
        Some(99)
    );
    assert_eq!(
        rt.call(&retained, &[&results[1]]).unwrap()[0].as_fixnum(),
        Some(42)
    );
}

#[test]
fn call_into_appends_all_values_and_preserves_existing_results_on_error() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        r#"
        (defun state (x) (values x nil 42))
        (defun ignore-state () (values))
        (defun bad-state () (error "failed"))
    "#,
    )
    .unwrap();
    let target = |name| rt.lookup_symbol("CL-USER", name).unwrap().unwrap();
    let input = rt.make_string("rooted").unwrap();
    let mut out = Vec::with_capacity(8);
    out.push(rt.t());
    rt.call_into(&target("STATE"), &[&input], &mut out).unwrap();
    rt.call_into(&target("IGNORE-STATE"), &[], &mut out)
        .unwrap();
    assert!(rt.call_into(&target("BAD-STATE"), &[], &mut out).is_err());
    rt.collect();
    assert_eq!(out.len(), 4);
    assert!(out[0].is_t());
    assert!(out[1].same_identity(&input));
    assert!(out[2].is_nil());
    assert_eq!(out[3].as_i64(), Some(42));
}

#[test]
fn a_cancelled_host_call_never_enters_its_callback() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = Runtime::start().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    runtime
        .register_function("count-host-calls", {
            let calls = calls.clone();
            move |runtime, _| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(vec![runtime.t()])
            }
        })
        .unwrap();
    let callback = runtime.eval("#'count-host-calls").unwrap().pop().unwrap();

    let cancellation = runtime.cancellation_handle();
    cancellation.cancel();
    assert_eq!(
        runtime.call(&callback, &[]).err().unwrap().kind(),
        EvalErrorKind::Cancelled
    );
    cancellation.reset();
    runtime.set_step_budget(Some(0));
    assert_eq!(
        runtime.call(&callback, &[]).err().unwrap().kind(),
        EvalErrorKind::Cancelled
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    runtime.set_step_budget(None);
    assert!(runtime.call(&callback, &[]).unwrap()[0].is_t());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[test]
fn cancellation_during_callback_reentry_reaches_the_host() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("cancel-nested-eval", |runtime, _| {
            let cancellation = runtime.cancellation_handle();
            cancellation.cancel();
            let result = runtime.eval("42");
            cancellation.reset();
            Ok(result?)
        })
        .unwrap();
    runtime
        .load_str(
            "(defparameter *host-cancel-state* :outside)
             (defparameter *host-cancel-cleanup* nil)
             (defmacro cancel-in-expander () (cancel-nested-eval))",
        )
        .unwrap();
    let error = runtime
        .eval(
            "(let ((*host-cancel-state* :inside))
               (ignore-errors
                 (unwind-protect (cancel-nested-eval)
                   (setq *host-cancel-cleanup* t))))",
        )
        .err()
        .unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);
    for source in [
        "(cancel-in-expander)",
        "(ignore-errors (read-from-string \"#.(cancel-nested-eval)\"))",
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(error.kind(), EvalErrorKind::Cancelled, "{source}: {error}");
    }
    assert!(
        runtime
            .eval("(and (eq *host-cancel-state* :outside) (not *host-cancel-cleanup*))")
            .unwrap()[0]
            .is_t()
    );
}

#[test]
fn host_redefinition_replaces_the_inline_body_for_future_compilation() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(declaim (inline next-number)) (defun next-number (x) (+ x 1))")
        .unwrap();
    runtime
        .register_function("next-number", |runtime, args| {
            Ok(vec![
                runtime.fixnum(args[0].as_fixnum().unwrap() + 10).unwrap(),
            ])
        })
        .unwrap();
    runtime
        .load_str("(defun after-redefinition (x) (next-number x))")
        .unwrap();
    assert_eq!(
        runtime.eval("(after-redefinition 3)").unwrap()[0].as_fixnum(),
        Some(13)
    );
}

#[test]
fn failed_registration_does_not_remove_a_reentrant_registration() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("register-while-reading", |runtime, _| {
            runtime.register_function("installed-during-read", |runtime, _| {
                Ok(vec![runtime.fixnum(42).unwrap()])
            })?;
            Ok(vec![runtime.nil()])
        })
        .unwrap();
    assert!(
        runtime
            .register_function("(invalid-name #.(register-while-reading))", |_, _| Ok(
                vec![]
            ))
            .is_err()
    );
    assert_eq!(
        runtime.eval("(installed-during-read)").unwrap()[0].as_fixnum(),
        Some(42)
    );
}

#[test]
fn room_reports_this_runtimes_heap_at_each_verbosity() {
    let runtime = Runtime::start().unwrap();
    runtime.eval("(zk:gc)").unwrap();
    for (argument, block_info, evacuation_info) in [
        ("nil", false, false),
        ("", true, false),
        (":default", true, false),
        ("t", true, true),
    ] {
        let values = runtime
            .eval(&format!(
                "(with-output-to-string (*standard-output*)
               (assert (null (multiple-value-list (room {argument})))))"
            ))
            .unwrap();
        let text = runtime.repr(&values[0]).unwrap();
        assert!(text.contains("Managed heap has"));
        assert_eq!(text.contains("Immix blocks"), block_info);
        assert_eq!(text.contains("Evacuation reserve"), evacuation_info);
    }
}

#[test]
fn roots_do_not_cross_runtime_heaps_or_alias_foreign_ids() {
    let first = Runtime::start().unwrap();
    let second = Runtime::start().unwrap();
    let value = first.foreign(10_i32).unwrap();
    let other = second.foreign(20_i32).unwrap();
    assert_eq!(value.try_with_foreign_ref(&first, |n: &i32| *n), Ok(10));
    assert_eq!(other.try_with_foreign_ref(&second, |n: &i32| *n), Ok(20));
    assert!(value.try_with_foreign_ref(&second, |n: &i32| *n).is_err());

    let list = first.cons(&value, &first.nil()).unwrap();
    assert!(second.cons_fields(&list).is_none());
    let identity = second.eval("#'identity").unwrap().pop().unwrap();
    assert!(second.call(&identity, &[&list]).is_err());
    assert!(first.call(&identity, &[]).is_err());
    let cons = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = second.cons(&value, &second.nil());
    }));
    assert!(cons.is_err());
}

#[test]
fn foreign_borrows_allow_reentry_into_the_runtime_but_do_not_alias() {
    let runtime = Runtime::start().unwrap();
    let resource = runtime.foreign(String::from("live")).unwrap();
    let other = runtime.foreign(0_u32).unwrap();
    let alias = resource.clone();

    resource
        .try_with_foreign_mut(&runtime, |text: &mut String| {
            let busy = Err(ForeignBorrowError::AlreadyBorrowed(std::any::type_name::<
                String,
            >()));
            assert_eq!(
                alias.try_with_foreign_ref::<String, _>(&runtime, |_| ()),
                busy
            );
            assert_eq!(
                alias.try_with_foreign_mut::<String, _>(&runtime, |_| ()),
                busy
            );
            assert!(matches!(
                alias.try_with_foreign_mut::<u32, _>(&runtime, |_| ()),
                Err(ForeignBorrowError::TypeMismatch { .. })
            ));
            let entries: Vec<_> = (0..64).map(|n| runtime.foreign(n).unwrap()).collect();
            runtime.collect();
            other
                .try_with_foreign_mut(&runtime, |value: &mut u32| *value = entries.len() as u32)
                .unwrap();
            text.push_str(" through GC");
        })
        .unwrap();
    resource
        .try_with_foreign_ref(&runtime, |text: &String| {
            alias
                .try_with_foreign_ref(&runtime, |other: &String| {
                    assert_eq!(text, "live through GC");
                    assert!(std::ptr::eq(text, other));
                    assert!(matches!(
                        alias.try_with_foreign_mut::<String, _>(&runtime, |_| ()),
                        Err(ForeignBorrowError::AlreadyBorrowed(_))
                    ));
                })
                .unwrap();
        })
        .unwrap();
    assert_eq!(
        other.try_with_foreign_ref(&runtime, |value: &u32| *value),
        Ok(64)
    );
}

#[cfg(feature = "ffi")]
#[test]
fn compiled_foreign_bindings_resolve_in_the_loading_runtime() {
    use std::ffi::c_void;
    use std::path::Path;
    use std::ptr::NonNull;

    extern "C" fn add_one(n: i32) -> i32 {
        n + 1
    }

    extern "C" fn add_two(n: i32) -> i32 {
        n + 2
    }

    fn address(function: extern "C" fn(i32) -> i32) -> NonNull<c_void> {
        NonNull::new(function as *mut c_void).unwrap()
    }

    // SAFETY: the test supplies process-lifetime extern C functions with the
    // signatures declared in the test's Lisp source.
    let new_runtime = || {
        unsafe { Runtime::builder().allow_foreign_calls() }
            .start()
            .unwrap()
    };
    let compiler = new_runtime();
    unsafe { compiler.register_foreign_symbol("only_here", address(add_one)) }.unwrap();
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let input = directory.join("host-binding.lisp");
    let output = directory.join("host-binding.zkclfasl");
    std::fs::write(
        &input,
        r#"(defparameter *host-step* (zk-ffi:bind-function :host "only_here" :int32 :int32))
           (defun run-host-step (n) (funcall *host-step* n))"#,
    )
    .unwrap();
    compiler.compile_file(&input, Some(&output)).unwrap();
    std::fs::remove_file(input).unwrap();
    drop(compiler);

    let missing = new_runtime();
    let error = missing.load_file(&output).unwrap_err();
    assert!(error.to_string().contains("only_here"));
    let host = new_runtime();
    unsafe { host.register_foreign_symbol("only_here", address(add_two)) }.unwrap();
    host.load_file(&output).unwrap();
    std::fs::remove_file(output).unwrap();
    host.collect();
    assert_eq!(
        host.eval("(run-host-step 40)").unwrap()[0].as_fixnum(),
        Some(42)
    );
}

#[test]
fn scoped_host_binding_shadows_an_active_mutable_borrow() {
    let rt = Runtime::builder().start().unwrap();
    let mut outer = 1_u32;
    rt.with_host_mut(&mut outer, || {
        rt.try_with_host_mut::<u32, _>(|outer| {
            rt.with_host_ref(&2_u32, || {
                rt.try_with_host_ref::<u32, _>(|inner| {
                    assert_eq!(*inner, 2);
                    assert_eq!(
                        rt.try_with_host_mut::<u32, _>(|_| ()),
                        Err(HostBorrowError::MutableRequired("u32"))
                    );
                })
                .unwrap();
            });
            assert_eq!(
                rt.try_with_host_ref::<u32, _>(|_| ()),
                Err(HostBorrowError::AlreadyBorrowed("u32"))
            );
            *outer += 10;
        })
        .unwrap();
        assert_eq!(rt.try_with_host_ref::<u32, _>(|value| *value), Ok(11));
    });
}

#[test]
fn scoped_host_borrow_unwind_restores_the_enclosing_borrow() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let rt = Runtime::builder().start().unwrap();
    rt.with_host_mut(&mut 0_u32, || {
        rt.try_with_host_ref::<u32, _>(|_| {
            let nested = catch_unwind(AssertUnwindSafe(|| {
                rt.try_with_host_ref::<u32, _>(|_| panic!("nested shared borrow"))
            }));
            assert_eq!(
                nested.unwrap_err().downcast_ref::<&str>(),
                Some(&"nested shared borrow")
            );
            assert_eq!(
                rt.try_with_host_mut::<u32, _>(|_| ()),
                Err(HostBorrowError::AlreadyBorrowed("u32"))
            );
        })
        .unwrap();

        let exclusive = catch_unwind(AssertUnwindSafe(|| {
            rt.try_with_host_mut::<u32, _>(|value| {
                *value = 42;
                panic!("exclusive borrow");
            })
        }));
        assert!(exclusive.is_err());
        assert_eq!(rt.try_with_host_mut::<u32, _>(|value| *value), Ok(42));
    });
}

#[test]
fn collection_in_one_thread_leaves_other_runtimes_values_intact() {
    let handles: Vec<_> = (0..4usize)
        .map(|worker_id| {
            std::thread::spawn(move || {
                let drops = Arc::new(AtomicUsize::new(0));
                let rt = Runtime::builder()
                    .heap_size_bytes(16 << 20)
                    .start()
                    .expect("start independent per-thread runtime");
                rt.load_str(&format!(
                    "(defparameter *worker-id* {worker_id})
                     (defun make-chain (n acc)
                       (if (= n 0)
                           acc
                           (make-chain (- n 1) (cons (+ n *worker-id*) acc))))"
                ))
                .unwrap();

                let f = rt
                    .foreign(CountedForeignResource {
                        id: worker_id,
                        drops: drops.clone(),
                    })
                    .unwrap();
                assert_eq!(
                    f.try_with_foreign_ref::<CountedForeignResource, _>(&rt, |r| r.id)
                        .unwrap(),
                    worker_id
                );
                drop(f);

                for _ in 0..8 {
                    let res = rt.eval("(length (make-chain 500 nil))").unwrap();
                    assert_eq!(res[0].as_fixnum(), Some(500));
                    rt.collect();
                }
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                let id_val = rt.eval("*worker-id*").unwrap();
                assert_eq!(id_val[0].as_fixnum(), Some(worker_id as isize));
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("worker runtime thread must succeed");
    }
}
