use zkcl::Runtime;

#[test]
fn special_bindings_restore_previous_dynamic_values() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defvar *mode* 1)
         (defvar *missing*)
         (setq local 10)
         (defun read-mode () *mode*)
         (defun read-local () local)
         (defun parameter-example (local)
           (declare (special local))
           (let ((local 8)) (values local (locally (declare (special local)) local))))
         (defun dynamic-reader (local)
           (declare (special local))
           (lambda () local))
         (setq saved-reader (dynamic-reader 88))",
    )
    .unwrap();
    let numbers = |source| {
        rt.eval(source)
            .unwrap()
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>()
    };
    assert!(rt.eval("(boundp '*missing*)").unwrap()[0].is_nil());
    rt.eval("(defvar *mode* (car 99))").unwrap(); // The initializer must not run.
    assert_eq!(numbers("*mode*"), [Some(1)]);
    rt.eval("(defparameter *mode* 2)").unwrap();
    assert_eq!(numbers("*mode*"), [Some(2)]);

    assert_eq!(
        numbers(
            "(progn (defvar *same-form* 4)
        (defun read-same-form () *same-form*)
        (let ((*same-form* 9)) (read-same-form)))"
        ),
        [Some(9)]
    );
    assert_eq!(
        numbers(
            "(locally (declaim (special *declaimed*))
        (setq *declaimed* 1)
        (let ((*declaimed* 6)) (symbol-value '*declaimed*)))"
        ),
        [Some(6)]
    );
    assert_eq!(numbers("(parameter-example 21)"), [Some(8), Some(21)]);
    assert_eq!(numbers("(funcall saved-reader)"), [Some(10)]);
    assert_eq!(
        numbers("(let ((local 22)) (declare (special local)) (funcall saved-reader))"),
        [Some(22)]
    );
    assert_eq!(
        numbers(
            "(let ((local 5) (old local))
        (declare (special local)) (values local old (read-local)))"
        ),
        [Some(5), Some(10), Some(5)]
    );
    assert_eq!(
        numbers(
            "(let* ((local 5) (old local))
        (declare (special local)) (values local old (read-local)))"
        ),
        [Some(5), Some(5), Some(5)]
    );
    assert_eq!(
        numbers(
            "(multiple-value-bind (local x) (values 19 20)
        (declare (special local)) (values (read-local) x))"
        ),
        [Some(19), Some(20)]
    );

    assert_eq!(
        numbers(
            "(let ((local 3))
        (progv '(local) '(4) (values local (symbol-value 'local))))"
        ),
        [Some(3), Some(4)]
    );
    assert!(rt.eval("(progv '(*mode*) nil (boundp '*mode*))").unwrap()[0].is_nil());
    assert!(
        !rt.eval("(progv '(*mode*) '(nil) (boundp '*mode*))")
            .unwrap()[0]
            .is_nil()
    );
    assert!(rt.eval("(progv '(nil) '(11) nil)").is_err());
    assert!(rt.eval("(progv '(*mode*) '(1 . 2) nil)").is_err());
    assert_eq!(numbers("*mode*"), [Some(2)]);
    assert_eq!(
        numbers("(let ((*mode* 10)) (makunbound '*mode*) (set '*mode* 11) (read-mode))"),
        [Some(11)]
    );
    assert_eq!(numbers("*mode*"), [Some(2)]);

    assert_eq!(
        numbers(
            "(catch :leave
        (let ((*mode* 3))
          (unwind-protect
              (let ((*mode* 4)) (throw :leave (values (read-mode) 77)))
            (setq observed (read-mode)))))"
        ),
        [Some(4), Some(77)]
    );
    assert_eq!(numbers("(values observed *mode*)"), [Some(3), Some(2)]);
    assert!(rt.eval("(let ((*mode* 100)) (car 1))").is_err());
    assert_eq!(numbers("*mode*"), [Some(2)]);

    rt.register_function("read-through-rust", |rt, _| Ok(rt.eval("(read-mode)")?))
        .unwrap();
    assert_eq!(
        numbers("(let ((*mode* 42)) (read-through-rust))"),
        [Some(42)]
    );
    assert_eq!(numbers("*mode*"), [Some(2)]);

    rt.register_function("rust-panics", |_, _| panic!("host panic"))
        .unwrap();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.eval("(let ((*mode* 19)) (let ((*mode* 92)) (rust-panics)))")
    }));
    assert!(panicked.is_err());
    assert_eq!(numbers("*mode*"), [Some(2)]);
}

#[test]
fn variable_documentation_survives_collection() {
    let rt = Runtime::start().unwrap();
    let numbers = |source| {
        rt.eval(source)
            .unwrap()
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>()
    };
    rt.load_str(
        r#"(defvar *doc-value* 42 "initial doc")
           (defvar *doc-value* (error "initializer should be skipped") "updated doc")
           (defparameter *other-doc-value* 5 "parameter doc")"#,
    )
    .unwrap();
    rt.collect();
    assert_eq!(
        numbers("(values *doc-value* *other-doc-value*)"),
        [Some(42), Some(5)]
    );
    let docs = rt
        .eval(
            "(values (documentation '*doc-value* 'variable)
                       (documentation '*other-doc-value* 'variable))",
        )
        .unwrap();
    assert_eq!(rt.string(&docs[0]).as_deref(), Some("updated doc"));
    assert_eq!(rt.string(&docs[1]).as_deref(), Some("parameter doc"));
}

#[test]
fn special_bindings_use_tls_without_clobbering_global_cell_and_survive_gc() {
    let rt = Runtime::builder()
        .heap_size_bytes(8 * 1024 * 1024)
        .start()
        .unwrap();
    rt.load_str("(defparameter *tls-var* :global)").unwrap();
    rt.register_function("collect-now", |runtime, _| {
        runtime.collect();
        Ok(Vec::new())
    })
    .unwrap();

    // Dynamic binding leaves global symbol.value untouched and restores it cleanly.
    let observed = rt
        .eval(
            "(let ((*tls-var* :dynamic))
               (list *tls-var*
                     (zk-atomic:atomic-load-symbol-value '*tls-var*)
                     (progn (setf *tls-var* :mutated-tls)
                            *tls-var*)
                     (zk-atomic:atomic-load-symbol-value '*tls-var*)))",
        )
        .unwrap();
    assert_eq!(
        rt.repr(&observed[0]).unwrap(),
        "(:DYNAMIC :GLOBAL :MUTATED-TLS :GLOBAL)"
    );
    assert_eq!(
        rt.repr(&rt.eval("*tls-var*").unwrap()[0]).unwrap(),
        ":GLOBAL"
    );

    // PROGV unbound shadows global binding while preserving the global value.
    let progv_unbound = rt
        .eval(
            "(progv '(*tls-var*) '()
               (list (boundp '*tls-var*)
                     (zk-atomic:atomic-load-symbol-value '*tls-var*)))",
        )
        .unwrap();
    assert_eq!(rt.repr(&progv_unbound[0]).unwrap(), "(NIL :GLOBAL)");
    assert_eq!(
        rt.repr(&rt.eval("*tls-var*").unwrap()[0]).unwrap(),
        ":GLOBAL"
    );

    // Dynamically bound young objects stay rooted and relocate safely under always_defrag GC.
    let relocated = rt
        .eval(
            "(let ((*tls-var* (list 'young 1 2 3)))
               (collect-now)
               (cons *tls-var* (zk-atomic:atomic-load-symbol-value '*tls-var*)))",
        )
        .unwrap();
    assert_eq!(
        rt.repr(&relocated[0]).unwrap(),
        "((YOUNG 1 2 3) . :GLOBAL)"
    );
}
