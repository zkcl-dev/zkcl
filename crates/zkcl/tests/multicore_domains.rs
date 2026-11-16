use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Barrier};
use std::thread;

use zkcl::{Domain, PersistentRoot, Vm};

#[test]
fn vm_and_persistent_root_are_send_sync_for_multithreaded_hosts() {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Vm>();
    assert_send_sync::<Arc<Vm>>();
    assert_send_sync::<PersistentRoot>();

    let vm = Vm::start().unwrap();
    let root_from_domain = {
        let domain: Domain = vm.attach_current_thread();
        assert_eq!(vm.active_domain_count(), 1);
        domain.eval("(list 1 2 3)").unwrap().pop().unwrap()
    };
    assert_eq!(vm.active_domain_count(), 0);

    // Send PersistentRoot + Arc<Vm> to another OS thread and inspect it there.
    let vm_worker = Arc::clone(&vm);
    let text = thread::spawn(move || {
        let domain = vm_worker.attach_named_thread("inspector-worker");
        assert_eq!(domain.name(), "inspector-worker");
        assert_eq!(vm_worker.active_domain_count(), 1);
        domain.repr(&root_from_domain).unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(text, "(1 2 3)");
    assert_eq!(vm.active_domain_count(), 0);
}

#[test]
fn multiple_os_threads_attach_domains_and_coordinate_with_zk_atomic_and_gc() {
    const THREADS: usize = 4;
    const ITERS_PER_THREAD: usize = 64;

    let vm = Vm::builder().heap_size_bytes(8 * 1024 * 1024).start().unwrap();
    {
        let init = vm.attach_named_thread("bootstrap");
        init.load_str("(defvar *atomic-cell* (cons 0 :tag))").unwrap();
    }
    assert_eq!(vm.active_domain_count(), 0);

    let start = Arc::new(Barrier::new(THREADS));
    thread::scope(|scope| {
        for tid in 0..THREADS {
            let vm = Arc::clone(&vm);
            let start = Arc::clone(&start);
            scope.spawn(move || {
                let domain = vm.attach_named_thread(format!("worker-{tid}"));
                start.wait();
                for iter in 0..ITERS_PER_THREAD {
                    domain
                        .eval(
                            "(loop
                               (let ((cur (zk-atomic:atomic-load-car *atomic-cell* :acquire)))
                                 (multiple-value-bind (_ swapped)
                                     (zk-atomic:cas-car *atomic-cell* cur (1+ cur) :acq-rel)
                                   (declare (ignore _))
                                   (when swapped (return cur)))))",
                        )
                        .unwrap();
                    if iter % 16 == 0 {
                        domain.collect();
                    }
                }
            });
        }
    });

    assert_eq!(vm.active_domain_count(), 0);
    let check = vm.attach_current_thread();
    let final_val = check
        .eval("(zk-atomic:atomic-load-car *atomic-cell* :seq-cst)")
        .unwrap();
    assert_eq!(
        check.repr(&final_val[0]).unwrap(),
        (THREADS * ITERS_PER_THREAD).to_string()
    );
}

#[test]
fn domains_isolate_dynamic_special_bindings_and_host_scopes_across_threads() {
    let vm = Vm::start().unwrap();
    vm.register_function("record-local-and-global", |rt, args| {
        let local_val = args[0].as_i64().unwrap();
        let global_ok = args[1].as_string(rt).as_deref() == Some("GLOBAL");
        rt.try_with_host_mut::<Rc<RefCell<Vec<(i64, bool)>>>, _>(|sink| {
            sink.borrow_mut().push((local_val, global_ok));
        })?;
        Ok(vec![args[0].clone()])
    })
    .unwrap();

    {
        let init = vm.attach_current_thread();
        init.load_str("(defparameter *shared-dyn* \"GLOBAL\")").unwrap();
    }

    let start = Arc::new(Barrier::new(2));
    let (res_a, res_b) = thread::scope(|scope| {
        let spawn_worker = |tag_base: i64| {
            let vm = Arc::clone(&vm);
            let start = Arc::clone(&start);
            scope.spawn(move || {
                let domain = vm.attach_named_thread(format!("tls-{tag_base}"));
                // Intentionally !Send local host state kept strictly on this OS thread.
                let mut local_log: Rc<RefCell<Vec<(i64, bool)>>> = Rc::new(RefCell::new(Vec::new()));
                start.wait();
                domain.with_host_mut(&mut local_log, |d| {
                    for step in 0..8 {
                        let expected = tag_base + step;
                        d.eval(&format!(
                            "(let ((*shared-dyn* {expected}))
                               (zk:gc)
                               (record-local-and-global
                                 *shared-dyn*
                                 (zk-atomic:atomic-load-symbol-value '*shared-dyn*)))"
                        ))
                        .unwrap();
                    }
                });
                let entries = local_log.borrow().clone();
                entries
            })
        };

        let a = spawn_worker(100);
        let b = spawn_worker(200);
        (a.join().unwrap(), b.join().unwrap())
    });

    let expected_a: Vec<(i64, bool)> = (0..8).map(|s| (100 + s, true)).collect();
    let expected_b: Vec<(i64, bool)> = (0..8).map(|s| (200 + s, true)).collect();
    assert_eq!(res_a, expected_a);
    assert_eq!(res_b, expected_b);

    let verify = vm.attach_current_thread();
    let global = verify
        .eval("(list *shared-dyn* (zk-atomic:atomic-load-symbol-value '*shared-dyn*))")
        .unwrap();
    assert_eq!(verify.repr(&global[0]).unwrap(), "(\"GLOBAL\" \"GLOBAL\")");
    drop(verify);
    assert_eq!(vm.active_domain_count(), 0);
}

#[test]
fn native_parked_domain_does_not_block_gc_on_other_domains() {
    let vm = Arc::new(Vm::start().unwrap());
    let attached = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));

    let vm_idle = Arc::clone(&vm);
    let attached_idle = Arc::clone(&attached);
    let release_idle = Arc::clone(&release);
    let idle_thread = thread::spawn(move || {
        // Holding an attached Domain while blocked in native Rust keeps its safepoint `InNative`.
        let _idle_domain = vm_idle.attach_named_thread("idle-native");
        attached_idle.wait();
        release_idle.wait();
    });

    attached.wait();
    assert_eq!(vm.active_domain_count(), 1);
    let active = vm.attach_named_thread("collector-worker");
    assert_eq!(vm.active_domain_count(), 2);

    for _ in 0..8 {
        active.eval("(list 1 2 3)").unwrap();
        active.collect();
        vm.collect();
    }
    drop(active);

    release.wait();
    idle_thread.join().unwrap();
    assert_eq!(vm.active_domain_count(), 0);
}
