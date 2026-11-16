#![cfg(feature = "direct-jit")]

use std::time::{Duration, Instant};

use zkcl::Runtime;
use zkcl::jit::{DirectJitBackend, JitError};

fn direct_backend() -> Option<DirectJitBackend> {
    match DirectJitBackend::new() {
        Ok(backend) => Some(backend),
        Err(JitError::UnsupportedTarget(_)) => None,
        Err(error) => panic!("starting Direct JIT: {error}"),
    }
}

fn direct_runtime() -> Option<Runtime> {
    let backend = direct_backend()?;
    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.set_jit_backend(std::sync::Arc::new(backend));
    Some(rt)
}

#[test]
fn published_leaf_joins_values_through_both_calling_conventions() {
    use zkcl::native::{JitBackend, JitCallContext, JitCompilationRequest, JitReturn, Word};
    use zkcl_bytecode::{Arity, Instruction::*};

    let Some(backend) = direct_backend() else {
        return;
    };
    let request = JitCompilationRequest {
        native_debug: false,
        code_id: 1,
        revision: 1,
        arity: Arity {
            required: 2,
            maximum: Some(2),
        },
        registers: 2,
        canonical: [
            Unless(0, 4),
            SetValues(0..2),
            Clear(0..2),
            Jump(5),
            SetValues(0..1),
            BindValues(0..2),
            SetValues(1..2),
        ]
        .into(),
        captures: Box::default(),
        notinline: Box::default(),
        handlers: Box::default(),
        restarts: Box::default(),
        keywords: Box::default(),
        jump_tables: Box::default(),
        param_facts: Box::default(),
        site_feedback: Vec::new(),
        cl_const_names: Box::default(),
    };
    // SAFETY: The backend has no attached runtime and serves only this detached leaf.
    assert!(unsafe { backend.submit_explicit(request) });
    let deadline = Instant::now() + Duration::from_secs(5);
    let compiled = loop {
        if let Some((mut compiled, failed)) = backend.poll_for(&mut |id, rev| id == 1 && rev == 1) {
            assert!(failed.is_empty(), "leaf was rejected");
            if let Some(function) = compiled.pop() {
                break function;
            }
        }
        assert!(Instant::now() < deadline, "expected a published leaf");
        std::thread::yield_now();
    };
    assert!(compiled.leaf_guards.is_empty());
    let scalar = compiled.leaf_entry.unwrap();
    let generic = compiled.c_entry.unwrap();
    let nil = Word::NIL.raw();
    let value = Word::fixnum(-17).unwrap().raw();
    for (flag, expected) in [(nil, nil), (Word::T.raw(), value)] {
        let args = [flag, value];
        let mut ctx = JitCallContext::detached();
        let mut result = JitReturn::zeroed();
        // The backend keeps this published revision mapped. This guardless
        // entry takes two tagged arguments and needs no runtime context.
        unsafe {
            assert_eq!(scalar(flag, value, nil, nil), expected);
            generic(&mut ctx, nil, args.as_ptr(), args.len(), &mut result);
        }
        assert_eq!(
            (result.tag, result.single_value),
            (JitReturn::TAG_ONE, expected)
        );
    }
}

fn install(rt: &Runtime, name: &str) {
    rt.eval(&format!("(compile nil #'{name})")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while rt.poll_jit_completions() == 0 {
        assert!(
            Instant::now() < deadline,
            "expected the leaf to be installed"
        );
        std::thread::yield_now();
    }
}

#[test]
fn installed_leaf_checks_chained_arithmetic_and_live_comparison_binding() {
    let Some(rt) = direct_runtime() else { return };
    rt.load_str("(defun vop-chain (a b) (if (< (1+ a) b) (* (- b a) (1+ a)) (1- b)))")
        .unwrap();
    install(&rt, "vop-chain");

    assert_eq!(rt.eval("(vop-chain 2 8)").unwrap()[0].as_fixnum(), Some(18));
    assert_eq!(rt.eval("(vop-chain 5 2)").unwrap()[0].as_fixnum(), Some(1));
    assert_eq!(
        rt.eval("(vop-chain most-positive-fixnum 7)").unwrap()[0].as_fixnum(),
        Some(6)
    );
    let error = rt
        .eval("(handler-case (vop-chain :bad 7) (type-error (c) (type-error-datum c)))")
        .unwrap();
    assert_eq!(rt.repr(&error[0]).unwrap(), ":BAD");

    let traced = rt.eval(r#"(let ((*trace-output* (make-string-output-stream)))
                          (trace <)
                          (unwind-protect
                              (values (vop-chain 2 8)
                                      (search "TRACE <: enter" (get-output-stream-string *trace-output*)))
                            (untrace <)))"#).unwrap();
    assert_eq!(traced[0].as_fixnum(), Some(18));
    assert!(traced[1].as_fixnum().is_some());
}

#[test]
fn installed_leaf_shares_the_continuation_of_successive_branches() {
    let Some(rt) = direct_runtime() else { return };
    let mut definition = String::from("(defun joined-leaf (flag value)");
    for _ in 0..18 {
        definition.push_str(" (setq value (if flag (1+ value) (1- value)))");
    }
    definition.push_str(" (+ value 1 -1))");
    rt.load_str(&definition).unwrap();
    install(&rt, "joined-leaf");

    assert_eq!(
        rt.eval("(joined-leaf t 42)").unwrap()[0].as_fixnum(),
        Some(60)
    );
    assert_eq!(
        rt.eval("(joined-leaf nil 42)").unwrap()[0].as_fixnum(),
        Some(24)
    );
    let promoted = rt
        .eval(
            "(let ((result (joined-leaf t most-positive-fixnum)))
        (and (typep result 'bignum) (= (- result most-positive-fixnum) 18)))",
        )
        .unwrap();
    assert!(promoted[0].is_t());
}

#[test]
fn installed_leaf_executes_integer_bits_and_checks_live_bindings() {
    let Some(rt) = direct_runtime() else { return };
    rt.load_str(
        "(defun vop-bits (left right)
        (if (logtest left right)
            (logxor (logand left 255) (logior right 128))
            (lognot left)))",
    )
    .unwrap();
    install(&rt, "vop-bits");

    for (form, expected) in [("(vop-bits -1 2)", Some(125)), ("(vop-bits 4 2)", Some(-5))] {
        assert_eq!(rt.eval(form).unwrap()[0].as_fixnum(), expected);
    }
    assert!(
        rt.eval("(= (vop-bits -1 (ash 1 100)) (+ (ash 1 100) 127))")
            .unwrap()[0]
            .is_t()
    );
    assert!(
        rt.eval(
            "(eq (handler-case (vop-bits :bad 2)
        (type-error (condition) (type-error-datum condition))) :bad)"
        )
        .unwrap()[0]
            .is_t()
    );
    let traced = rt
        .eval(
            r#"(let ((*trace-output* (make-string-output-stream)))
          (trace logxor)
          (unwind-protect
              (values (vop-bits -1 2)
                      (search "TRACE LOGXOR: enter" (get-output-stream-string *trace-output*)))
            (untrace logxor)))"#,
        )
        .unwrap();
    assert_eq!(traced[0].as_fixnum(), Some(125));
    assert!(traced[1].as_fixnum().is_some());
}

#[test]
fn installed_leaf_runs_dotimes_and_respects_the_host_step_budget() {
    let Some(rt) = direct_runtime() else { return };
    rt.load_str(
        "(defun vop-sum (count) (let ((total 0))
                  (dotimes (i count total) (incf total i))))",
    )
    .unwrap();
    install(&rt, "vop-sum");

    for (count, expected) in [(-5, 0), (0, 0), (16, 120), (300, 44850)] {
        assert_eq!(
            rt.eval(&format!("(vop-sum {count})")).unwrap()[0].as_fixnum(),
            Some(expected)
        );
    }
    let function = rt.eval("#'vop-sum").unwrap().remove(0);
    let count = rt.fixnum(16).unwrap();
    rt.set_step_budget(Some(8));
    let error = rt
        .call(&function, &[&count])
        .err()
        .expect("budget must stop the call");
    assert_eq!(
        error.execution_stop(),
        Some(zkcl::ExecutionStop::BudgetExhausted)
    );
}

#[test]
fn direct_debug_three_uses_native_source_maps_and_writable_locals() {
    let Some(rt) = direct_runtime() else { return };
    rt.load_str(
        r#"(defvar *direct-debug-native* nil)
(defun direct-debug-target (x)
  (declare (optimize (debug 3)))
  (let ((y 3))
    (+ x y)))
(defun direct-debug-frontend (condition)
  (declare (ignore condition))
  (setq *direct-debug-native* (seventh (first (zk::%debug-frames))))
  (zk::%set-debug-local 40 0 'y)
  (zk:gc)
  (zk::%debug-motion :continue))"#,
    )
    .unwrap();
    install(&rt, "direct-debug-target");
    let function = rt.eval("#'direct-debug-target").unwrap().remove(0);
    rt.add_source_breakpoint(&function, 5).unwrap();
    let frontend = rt.eval("#'direct-debug-frontend").unwrap().remove(0);
    rt.set_debugger(Some(&frontend)).unwrap();
    assert_eq!(
        rt.call(&function, &[&rt.fixnum(2).unwrap()]).unwrap()[0].as_fixnum(),
        Some(42)
    );
    assert_eq!(
        rt.repr(&rt.eval("*direct-debug-native*").unwrap()[0])
            .unwrap(),
        "T"
    );
}
