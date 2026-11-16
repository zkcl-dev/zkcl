use std::sync::Arc;
use std::time::{Duration, Instant};

use zkcl_bytecode::{Arity, BytecodeFunction, Callee, Immediate, Instruction};
use zkcl_jit::CraneliftJitBackend;
use zkcl_opt::{FIXNUM_MAX, NoSpecializationInputs, lift_code, optimize};
use zkcl_runtime::Runtime;
use zkcl_runtime::native::{
    JitBackend, JitCallContext, JitCompilationRequest, JitReturn, WordLayout,
};

const PORTABLE_VOP: &str = "
    (defun word-count (word) (declare (ignore word)) (values 99 :lisp))
    (defun second-word-count (word) (declare (ignore word)) (values 88 :lisp))
    (zk-sys:defknown word-count (integer) (integer 0 64))
    (zk-sys:defknown second-word-count (integer) (integer 0 64))
    (zk-sys:deftransform word-count (word) (:ctz word))
    (zk-sys:deftransform second-word-count (word) (:ctz word))";

#[test]
fn native_transforms_preserve_values_and_arguments_on_binding_or_type_miss() {
    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.load_str(PORTABLE_VOP).unwrap();
    rt.load_str(
        "(defvar *first-word-arguments* 0)
         (defvar *second-word-arguments* 0)
         (defun count-after (word)
           (word-count (progn (incf *first-word-arguments*) word)))
         (defun second-count-after (word)
           (second-word-count (progn (incf *second-word-arguments*) word)))
         (defun ordinary-count (word)
           (declare (notinline word-count)) (word-count word))
         (defun lexical-count (word)
           (flet ((word-count (x) (declare (ignore x)) 73))
             (declare (notinline word-count))
             (word-count word)))",
    )
    .unwrap();
    rt.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));
    let count_after = rt.eval("#'count-after").unwrap().pop().unwrap();
    let second_count_after = rt.eval("#'second-count-after").unwrap().pop().unwrap();
    rt.eval("(compile nil #'count-after)").unwrap();
    rt.eval("(compile nil #'second-count-after)").unwrap();
    wait_for_native_functions(&rt, &[&count_after, &second_count_after]);

    rt.load_str(
        r#"
        (assert (equal (multiple-value-list (count-after 0)) '(64)))
        (assert (equal (multiple-value-list (count-after #x8000000000000000)) '(63)))
        (assert (equal (multiple-value-list (count-after #xffffffffffffffff)) '(0)))
        (assert (equal (multiple-value-list (second-count-after 16)) '(4)))
        (assert (equal (multiple-value-list (count-after -1)) '(99 :lisp)))
        (assert (equal (multiple-value-list (count-after #x10000000000000000)) '(99 :lisp)))
        (assert (equal (multiple-value-list (count-after :bad-type)) '(99 :lisp)))
        (assert (equal (multiple-value-list (second-count-after -1)) '(88 :lisp)))
        (assert (equal (multiple-value-list (second-count-after :bad-type)) '(88 :lisp)))
        (assert (eq 99 (ordinary-count 8)))
        (assert (eq 73 (lexical-count 8)))
        (let ((old (symbol-function 'word-count))
              (second-old (symbol-function 'second-word-count))
              (*trace-output* (make-string-output-stream)))
          (unwind-protect
              (progn
                (trace word-count)
                (assert (eq 99 (count-after 8)))
                (assert (search "WORD-COUNT" (get-output-stream-string *trace-output*)))
                (untrace word-count)
                (setf (symbol-function 'word-count) (lambda (word) (1+ word)))
                (setf (symbol-function 'second-word-count) (lambda (word) (+ word 2)))
                (assert (eq 9 (count-after 8)))
                (assert (eq 10 (second-count-after 8))))
            (untrace word-count)
            (setf (symbol-function 'word-count) old)
            (setf (symbol-function 'second-word-count) second-old)))
        (assert (eq 3 (count-after 8)))
        (assert (eq 4 (second-count-after 16)))
        (assert (eq 6 *first-word-arguments*))
        (assert (eq 2 *second-word-arguments*))"#,
    )
    .unwrap();
}

#[test]
fn native_transform_registration_and_calls_survive_separate_fasl_loads() {
    use zkcl_runtime::SourceDocument;

    let compiler = Runtime::start().unwrap();
    let definitions = compiler
        .compile_document_to_bytecode(&SourceDocument::input(PORTABLE_VOP))
        .unwrap()
        .encode()
        .unwrap();
    compiler.load_fasl(&definitions).unwrap();
    let consumer = compiler
        .compile_document_to_bytecode(&SourceDocument::input(
            "(defun count-in-other-file (word) (word-count word))
             (defun second-count-in-other-file (word) (second-word-count word))",
        ))
        .unwrap();
    for name in ["COUNT-IN-OTHER-FILE", "SECOND-COUNT-IN-OTHER-FILE"] {
        let code = consumer
            .code_entries()
            .find(|entry| entry.name.as_deref() == Some(name))
            .unwrap();
        assert_eq!(code.vop_calls.len(), 1, "{name}");
    }
    let consumer = consumer.encode().unwrap();

    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.load_fasl(&consumer).unwrap();
    rt.load_str(
        "(defun word-count (word) (declare (ignore word)) 7)
         (defun second-word-count (word) (declare (ignore word)) 11)",
    )
    .unwrap();
    rt.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));
    let first = rt.eval("#'count-in-other-file").unwrap().pop().unwrap();
    let second = rt.eval("#'second-count-in-other-file").unwrap().pop().unwrap();
    rt.eval("(compile nil #'count-in-other-file)").unwrap();
    rt.eval("(compile nil #'second-count-in-other-file)")
        .unwrap();
    wait_for_native_functions(&rt, &[&first, &second]);
    assert_eq!(
        rt.eval("(count-in-other-file 8)").unwrap()[0].as_fixnum(),
        Some(7)
    );
    assert_eq!(
        rt.eval("(second-count-in-other-file 8)").unwrap()[0].as_fixnum(),
        Some(11)
    );
    rt.eval("(trace word-count)").unwrap();
    rt.load_fasl(&definitions).unwrap();
    rt.eval(
        "(let ((*trace-output* (make-string-output-stream)))
           (assert (eq 99 (count-in-other-file 8)))
           (assert (search \"WORD-COUNT\" (get-output-stream-string *trace-output*))))",
    )
    .unwrap();
    rt.eval("(untrace word-count)").unwrap();
    assert_eq!(
        rt.eval("(count-in-other-file 8)").unwrap()[0].as_fixnum(),
        Some(3)
    );
    assert_eq!(
        rt.eval("(second-count-in-other-file 16)").unwrap()[0].as_fixnum(),
        Some(4)
    );
    rt.load_str(
        "(defun word-count (word) (declare (ignore word)) 13)
         (defun second-word-count (word) (declare (ignore word)) 17)",
    )
    .unwrap();
    assert_eq!(
        rt.eval("(count-in-other-file 8)").unwrap()[0].as_fixnum(),
        Some(13)
    );
    assert_eq!(
        rt.eval("(second-count-in-other-file 8)").unwrap()[0].as_fixnum(),
        Some(17)
    );
}

fn tag_fixnum(n: isize) -> usize {
    (((n) << WordLayout::FIXNUM_SHIFT) as usize) | WordLayout::FIXNUM_TAG
}

fn untag_fixnum(raw: usize) -> isize {
    (raw as isize) >> WordLayout::FIXNUM_SHIFT
}

fn literal_bytecode(value: i64) -> BytecodeFunction {
    BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity: Arity::ZERO,
        registers: 1,
        instructions: vec![
            Instruction::Load(0, Immediate::Fixnum(value)),
            Instruction::SetValues(0..1),
        ],
        constants: Vec::new(),
        lambda_list: None,
        captures: Vec::new(),
        notinline: Vec::new(),
        vop_calls: Vec::new(),
        handlers: Vec::new(),
        restarts: Vec::new(),
        keywords: Vec::new(),
        jump_tables: Vec::new(),
        facts: Default::default(),
    }
}

#[test]
fn jit_lowering_exposes_the_out_of_range_immediate() {
    use std::error::Error as _;
    use zkcl_cranelift::LoweringError;
    use zkcl_jit::compile_ssa_function_for_host;
    use zkcl_opt::SsaOp;

    let mut function = lift_code(&literal_bytecode(77), &NoSpecializationInputs).unwrap();
    let literal = function
        .blocks
        .iter_mut()
        .flat_map(|block| &mut block.insts)
        .find(|inst| inst.op == SsaOp::ConstWord(Immediate::Fixnum(77)))
        .expect("lifted literal");
    literal.op = SsaOp::ConstWord(Immediate::Fixnum(i64::MAX));

    let error = compile_ssa_function_for_host(&function).unwrap_err();
    assert!(matches!(
        error.source().and_then(|source| source.downcast_ref()),
        Some(LoweringError::ImmediateOutOfRange(i64::MAX))
    ));
}

#[test]
fn native_fixnum_loop_checks_live_bindings_and_preserves_arity_and_type_errors() {
    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.load_str(
        "(defun sum-integers-below (n)
        (do ((i 0 (1+ i)) (sum 0 (+ sum i)))
            ((not (< i n)) sum)))",
    )
    .unwrap();
    let function = rt.eval("#'sum-integers-below").unwrap().pop().unwrap();
    rt.call(&function, &[&rt.fixnum(10).unwrap()]).unwrap();
    let backend = Arc::new(CraneliftJitBackend::new().unwrap());
    rt.set_jit_backend(backend.clone());
    let compile = rt.eval("#'compile").unwrap().pop().unwrap();
    rt.call(&compile, &[&rt.nil(), &function]).unwrap();
    wait_for_native_installation(&rt);

    for n in [0, 1, 10, 100, 1000] {
        let result = rt.call(&function, &[&rt.fixnum(n).unwrap()]).unwrap();
        assert_eq!(result[0].as_fixnum(), Some(n * (n - 1) / 2));
    }
    assert!(rt.call(&function, &[]).is_err());
    assert!(rt.call(&function, &[&rt.nil()]).is_err());
    for name in ["<", "+", "1+"] {
        let form = format!(
            r#"
            (let ((*trace-output* (make-string-output-stream)))
              (trace {name})
              (unwind-protect
                  (let ((result (sum-integers-below 3))
                        (text (get-output-stream-string *trace-output*)))
                    (values result (search "TRACE {name}: enter" text)))
                (untrace {name})))"#
        );
        let traced = rt.eval(&form).unwrap();
        assert_eq!(traced[0].as_fixnum(), Some(3));
        assert!(traced[1].as_fixnum().is_some(), "{name}");
    }
}

#[test]
fn fixnum_calls_check_a_binding_changed_by_an_intervening_lisp_call() {
    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.load_str(
        "(defun increment-around-call (x change)
                  (let ((first (1+ x)))
                    (funcall change)
                    (values first (1+ first))))",
    )
    .unwrap();
    let function = rt.eval("#'increment-around-call").unwrap().pop().unwrap();
    rt.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));
    let compile = rt.eval("#'compile").unwrap().pop().unwrap();
    rt.call(&compile, &[&rt.nil(), &function]).unwrap();
    wait_for_native_installation(&rt);
    let values = rt
        .eval(
            r#"
        (let ((*trace-output* (make-string-output-stream)))
          (unwind-protect
              (multiple-value-bind (a b)
                  (increment-around-call 7 (lambda () (trace 1+)))
                (values a b (get-output-stream-string *trace-output*)))
            (untrace 1+)))"#,
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(8));
    assert_eq!(values[1].as_fixnum(), Some(9));
    assert_eq!(
        rt.string(&values[2])
            .unwrap()
            .matches("TRACE 1+: enter")
            .count(),
        1
    );
}

#[test]
fn detached_fixnum_calls_require_a_live_primitive_binding() {
    use zkcl_runtime::native::{JitSiteHint, JitSiteKind};
    let request = JitCompilationRequest {
        native_debug: false,
        code_id: 2,
        revision: 1,
        arity: Arity {
            required: 1,
            maximum: Some(1),
        },
        registers: 3,
        canonical: Box::new([
            Instruction::Load(1, Immediate::Fixnum(1)),
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(2),
                args: 0..2,
                at: 1,
                associate: false,
                notinline: false,
            },
            Instruction::SetValues(2..3),
        ]),
        captures: Box::new([]),
        notinline: Box::new([]),
        vop_calls: Box::new([]),
        handlers: Box::new([]),
        restarts: Box::new([]),
        keywords: Box::new([]),
        jump_tables: Box::new([]),
        param_facts: Box::new([]),
        site_feedback: vec![JitSiteHint {
            canonical_pc: 1,
            kind: JitSiteKind::KnownFixnumArith,
        }],
        cl_const_names: Box::new([Some("+")]),
    };
    let backend = CraneliftJitBackend::new().expect("host ISA supported");
    let jit_function = backend.compile_now(&request).expect("compiles checked add");
    let c_entry = jit_function.c_entry.unwrap();

    let mut ctx = JitCallContext::detached();

    let argument = tag_fixnum(41);
    let mut result = JitReturn::zeroed();
    unsafe { c_entry(&mut ctx, 0, &argument, 1, &mut result) };
    assert_eq!(result.tag, JitReturn::TAG_FAULT);

    let max_arg = tag_fixnum(FIXNUM_MAX as isize);
    let mut overflow_return = JitReturn::zeroed();
    unsafe { c_entry(&mut ctx, 0, &max_arg, 1, &mut overflow_return) };
    assert_eq!(overflow_return.tag, JitReturn::TAG_FAULT);

    let missing_constants = JitCompilationRequest {
        cl_const_names: Box::new([]),
        ..request
    };
    assert!(matches!(
        backend.compile_now(&missing_constants),
        Err(zkcl_jit::JitError::Lift(
            zkcl_opt::BytecodeLiftError::ConstantOutOfBounds {
                pc: Some(1),
                index: 0,
                len: 0,
            }
        ))
    ));
}

#[test]
fn jit_compiles_a_balanced_progv_scope() {
    let request = JitCompilationRequest {
        native_debug: false,
        code_id: 99,
        revision: 1,
        arity: Arity::ZERO,
        registers: 2,
        canonical: vec![
            Instruction::Load(0, Immediate::Nil),
            Instruction::Load(1, Immediate::Nil),
            Instruction::BindProgv {
                symbols: 0,
                values: 1,
                at: 0,
            },
            Instruction::SetValues(0..1),
            Instruction::Unbind(1),
        ]
        .into_boxed_slice(),
        captures: Box::new([]),
        notinline: Box::new([]),
        vop_calls: Box::new([]),
        handlers: Box::new([]),
        restarts: Box::new([]),
        keywords: Box::new([]),
        jump_tables: Box::new([]),
        param_facts: Box::new([]),
        site_feedback: Vec::new(),
        cl_const_names: Box::new([]),
    };

    let backend = CraneliftJitBackend::new().expect("host ISA supported");
    let compiled = backend.compile_now(&request).expect("compile PROGV");
    assert_eq!(compiled.code_id, request.code_id);
    assert!(compiled.c_entry.is_some());
}

#[test]
fn delivered_completion_owns_a_released_background_entry() {
    let request = JitCompilationRequest {
        native_debug: false,
        code_id: 7,
        revision: 3,
        arity: Arity {
            required: 1,
            maximum: Some(1),
        },
        registers: 2,
        canonical: vec![
            Instruction::Copy(1, 0),
            Instruction::Unless(1, 4),
            Instruction::Load(0, Immediate::Fixnum(111)),
            Instruction::Jump(5),
            Instruction::Load(0, Immediate::Fixnum(222)),
            Instruction::SetValues(0..1),
        ]
        .into_boxed_slice(),
        captures: Box::new([]),
        notinline: Box::new([]),
        vop_calls: Box::new([]),
        handlers: Box::new([]),
        restarts: Box::new([]),
        keywords: Box::new([]),
        jump_tables: Box::new([]),
        param_facts: Box::new([]),
        site_feedback: Vec::new(),
        cl_const_names: Box::new([]),
    };

    let backend = CraneliftJitBackend::new().expect("host ISA supported");
    // SAFETY: This backend serves only the standalone bytecode fixture.
    unsafe { backend.submit(request) };

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut completed = Vec::new();
    while Instant::now() < deadline {
        completed.extend(backend.poll_completions());
        if !completed.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(completed.len(), 1);
    let jit_function = completed.pop().unwrap();
    assert_eq!(jit_function.code_id, 7);
    assert_eq!(jit_function.entry.revision, 3);
    let c_entry = jit_function.c_entry.expect("c_entry produced");
    let mapping = backend.code_cache().get(7).expect("published mapping");
    assert!(std::ptr::fn_addr_eq(c_entry, mapping.c_entry));
    let executable = Arc::downgrade(&mapping.executable_code);
    drop(mapping);
    backend.release(7, 3);
    assert!(backend.code_cache().get(7).is_none());
    drop(backend);
    assert!(executable.upgrade().is_some());

    // The completion keeps both the code and its backing slab alive.
    let mut ctx = JitCallContext::detached();

    let t_arg = WordLayout::T;
    let mut t_ret = JitReturn::zeroed();
    unsafe { c_entry(&mut ctx, 0, &t_arg, 1, &mut t_ret) };
    assert_eq!(t_ret.tag, JitReturn::TAG_ONE);
    assert_eq!(untag_fixnum(t_ret.single_value), 111);

    let nil_arg = WordLayout::NIL;
    let mut nil_ret = JitReturn::zeroed();
    unsafe { c_entry(&mut ctx, 0, &nil_arg, 1, &mut nil_ret) };
    assert_eq!(nil_ret.tag, JitReturn::TAG_ONE);
    assert_eq!(untag_fixnum(nil_ret.single_value), 222);
    drop(jit_function);
    assert!(executable.upgrade().is_none());
}

#[test]
fn synchronous_ssa_entry_returns_a_literal_without_site_feedback() {
    let backend = CraneliftJitBackend::new().expect("host ISA supported");
    let mut ctx = JitCallContext::detached();
    let ssa_function = lift_code(&literal_bytecode(77), &NoSpecializationInputs).unwrap();
    let direct_jit_function = backend.compile_ssa_function(8, 1, &ssa_function).unwrap();
    let mut direct_return = JitReturn::zeroed();
    unsafe {
        (direct_jit_function.c_entry.unwrap())(&mut ctx, 0, std::ptr::null(), 0, &mut direct_return)
    };
    assert_eq!(direct_return.tag, JitReturn::TAG_ONE);
    assert_eq!(untag_fixnum(direct_return.single_value), 77);
}

fn runtime_with_gc_callback() -> Runtime {
    let rt = Runtime::start().expect("start runtime");
    rt.register_function("jit-force-gc", |rt, _| {
        rt.collect();
        Ok(vec![rt.nil()])
    })
    .expect("register jit-force-gc");
    rt
}

#[test]
fn runtime_installs_hot_functions_for_native_execution() {
    let rt = runtime_with_gc_callback();
    rt.load_str(
        "(defun jit-factorial (n)
           (if (<= n 1)
               1
               (* n (jit-factorial (- n 1)))))
         (defun jit-gc-preserve (a b)
           (let ((pair (cons a b)))
             (jit-force-gc)
             (values (car pair) (cdr pair))))
         (defun jit-gc-car (a b)
           (let ((pair (cons a b)))
             (jit-force-gc)
             (car pair)))",
    )
    .expect("load Lisp test functions");

    // Baseline VM results before enabling JIT.
    let vm_factorial = rt.eval("(jit-factorial 10)").expect("VM jit-factorial");
    assert_eq!(vm_factorial[0].as_fixnum(), Some(3_628_800));

    let vm_car = rt
        .eval("(car (jit-gc-car (list 42) (list 99)))")
        .expect("VM jit-gc-car");
    assert_eq!(vm_car[0].as_fixnum(), Some(42));

    // Attach CraneliftJitBackend to Runtime.
    let backend = Arc::new(CraneliftJitBackend::new().expect("create JIT backend"));
    rt.set_jit_backend(Arc::clone(&backend) as Arc<dyn JitBackend>);

    // First calls submit functions to background JIT compilation.
    let warmup_factorial = rt.eval("(jit-factorial 10)").expect("warmup jit-factorial");
    assert_eq!(warmup_factorial[0].as_fixnum(), Some(3_628_800));

    let warmup_car = rt
        .eval("(car (jit-gc-car (list 42) (list 99)))")
        .expect("warmup jit-gc-car");
    assert_eq!(warmup_car[0].as_fixnum(), Some(42));

    // Wait for background worker to finish compiling and install native entries into CodeStore.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut installed = 0usize;
    while Instant::now() < deadline {
        installed += rt.poll_jit_completions();
        if installed >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        installed >= 2,
        "expected at least 2 native entries installed, got {installed}"
    );
    assert!(backend.code_cache().len() >= 2);

    // Subsequent calls execute JIT-compiled machine code and preserve roots across GC!
    let jit_factorial = rt.eval("(jit-factorial 10)").expect("JIT jit-factorial");
    assert_eq!(jit_factorial[0].as_fixnum(), Some(3_628_800));

    let jit_car = rt
        .eval("(car (jit-gc-car (list 123) (list 456)))")
        .expect("JIT jit-gc-car");
    assert_eq!(jit_car[0].as_fixnum(), Some(123));

    // Multi-value function (`jit-gc-preserve` uses `ReturnDynamicValues` or `ReturnValues(2)`):
    // either JIT-compiled or seamlessly executed by VM without regression.
    let multi = rt
        .eval("(multiple-value-list (jit-gc-preserve (list 77) (list 88)))")
        .expect("jit-gc-preserve");
    let check_multi = rt
        .eval("(let ((vals (multiple-value-list (jit-gc-preserve (list 77) (list 88))))) (list (caar vals) (caadr vals)))")
        .expect("check multi");
    let _ = multi;
    let (first, rest) = rt.cons_fields(&check_multi[0]).unwrap();
    let (second, _) = rt.cons_fields(&rest).unwrap();
    assert_eq!(first.as_fixnum(), Some(77));
    assert_eq!(second.as_fixnum(), Some(88));
}

#[test]
fn profiled_fixnum_addition_resumes_in_the_vm_on_guard_failure() {
    let rt = Runtime::start().expect("start runtime");
    let backend = Arc::new(CraneliftJitBackend::new().expect("create JIT backend"));
    rt.set_jit_backend(Arc::clone(&backend) as Arc<dyn JitBackend>);
    // Warm up `(jit-add a b)` with fixnums BEFORE JIT submission so SiteFeedback records
    // ArithPrimitive and JitSpecializationInputs specializes it to UnboxFixnumGuard + AddI64Checked.
    rt.set_jit_threshold(3);
    rt.load_str("(defun jit-add (a b) (+ a b))").unwrap();
    assert_eq!(rt.eval("(jit-add 20 22)").unwrap()[0].as_fixnum(), Some(42));
    assert_eq!(rt.eval("(jit-add 1 2)").unwrap()[0].as_fixnum(), Some(3));
    let before_add_cache = backend.code_cache().len();
    assert_eq!(rt.eval("(jit-add 5 6)").unwrap()[0].as_fixnum(), Some(11));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && backend.code_cache().len() == before_add_cache {
        rt.poll_jit_completions();
        std::thread::sleep(Duration::from_millis(2));
    }
    rt.poll_jit_completions();
    assert!(
        backend.code_cache().len() > before_add_cache,
        "expected jit-add to compile with fixnum specialization"
    );

    // Fixnum fast-path still works, AND bignum overflow + float inputs cleanly deopt to VM!
    assert_eq!(
        rt.eval("(jit-add 100 23)").unwrap()[0].as_fixnum(),
        Some(123)
    );
    assert!(
        rt.eval("(typep (jit-add most-positive-fixnum 1) 'bignum)")
            .unwrap()[0]
            .is_t()
    );
    let big_res = rt.eval("(jit-add 4611686018427387903 10)").unwrap();
    assert_eq!(rt.repr(&big_res[0]).unwrap(), "4611686018427387913");
    let flt_res = rt.eval("(jit-add 1.5 2.5)").unwrap();
    assert_eq!(rt.repr(&flt_res[0]).unwrap(), "4.0");
}

#[test]
fn native_nonlocal_exits_run_unwind_protect_cleanup() {
    let rt = runtime_with_gc_callback();
    let backend = Arc::new(CraneliftJitBackend::new().expect("create JIT backend"));
    rt.set_jit_backend(Arc::clone(&backend) as Arc<dyn JitBackend>);
    rt.set_jit_threshold(1);
    let cache_before_expanded = backend.code_cache().len();
    rt.load_str(
        "(defvar *jit-dyn* 100)
         (defvar *jit-uwp-flag* nil)
         (defun jit-make-adder (k) (lambda (x) (+ x k)))
         (defun jit-multi (a b) (values (+ a 1) (+ b 2) (+ a b)))
         (defun jit-use-multi (a b)
           (multiple-value-bind (x y z) (jit-multi a b) (+ (* x 100) (* y 10) z)))
         (defun jit-read-dyn () *jit-dyn*)
         (defun jit-with-dyn (v)
           (let ((*jit-dyn* v)) (jit-read-dyn)))
         (defun jit-uwp-catch ()
           (setq *jit-uwp-flag* nil)
           (catch 'bail
             (unwind-protect
                 (throw 'bail 77)
               (setq *jit-uwp-flag* :cleaned))))
         (defun jit-block-ret (x)
           (block done
             (unwind-protect
                 (if (> x 0) (return-from done (+ x 10)) 0)
               (jit-force-gc))
             -1))",
    )
    .expect("load expanded JIT feature functions");

    // Trigger warmup calls (which also submit JIT requests) then poll completions.
    assert_eq!(
        rt.eval("(funcall (jit-make-adder 10) 32)").unwrap()[0].as_fixnum(),
        Some(42)
    );
    assert_eq!(
        rt.eval("(jit-use-multi 3 4)").unwrap()[0].as_fixnum(),
        Some(467)
    );
    assert_eq!(
        rt.eval("(jit-with-dyn 999)").unwrap()[0].as_fixnum(),
        Some(999)
    );
    assert_eq!(rt.eval("(jit-read-dyn)").unwrap()[0].as_fixnum(), Some(100));
    assert_eq!(rt.eval("(jit-uwp-catch)").unwrap()[0].as_fixnum(), Some(77));
    assert_eq!(rt.eval("*jit-uwp-flag*").unwrap()[0].as_fixnum(), None);
    assert_eq!(
        rt.eval("(jit-block-ret 5)").unwrap()[0].as_fixnum(),
        Some(15)
    );
    assert_eq!(
        rt.eval("(jit-block-ret 0)").unwrap()[0].as_fixnum(),
        Some(-1)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && backend.code_cache().len() < cache_before_expanded + 5 {
        rt.poll_jit_completions();
        std::thread::sleep(Duration::from_millis(2));
    }
    rt.poll_jit_completions();
    assert!(
        backend.code_cache().len() >= cache_before_expanded + 5,
        "expected closure, multi-value, special, uwp-catch, and block functions in JIT cache (before={cache_before_expanded}, after={})",
        backend.code_cache().len()
    );

    // Re-execute on installed JIT native code!
    assert_eq!(
        rt.eval("(funcall (jit-make-adder 100) 23)").unwrap()[0].as_fixnum(),
        Some(123)
    );
    assert_eq!(
        rt.eval("(jit-use-multi 5 6)").unwrap()[0].as_fixnum(),
        Some(691)
    );
    assert_eq!(
        rt.eval("(jit-with-dyn 555)").unwrap()[0].as_fixnum(),
        Some(555)
    );
    assert_eq!(rt.eval("(jit-read-dyn)").unwrap()[0].as_fixnum(), Some(100));
    assert_eq!(rt.eval("(jit-uwp-catch)").unwrap()[0].as_fixnum(), Some(77));
    assert_eq!(
        rt.repr(&rt.eval("*jit-uwp-flag*").unwrap()[0]).unwrap(),
        ":CLEANED"
    );
    assert_eq!(
        rt.eval("(jit-block-ret 32)").unwrap()[0].as_fixnum(),
        Some(42)
    );
    assert_eq!(
        rt.eval("(jit-block-ret -1)").unwrap()[0].as_fixnum(),
        Some(-1)
    );
}

#[test]
fn code_cache_maps_1500_callable_entries() {
    use zkcl_jit::{CodeCache, compile_ssa_function_for_host};

    let mut ssa_function = lift_code(&literal_bytecode(99), &NoSpecializationInputs).unwrap();
    optimize(&mut ssa_function, &NoSpecializationInputs);
    let compiled = compile_ssa_function_for_host(&ssa_function).expect("host Cranelift compiles");

    let cache = CodeCache::new();
    let mut ctx = JitCallContext::detached();
    for code_id in 0..1500usize {
        let entry = cache
            .insert(code_id, 1, &compiled.bytes)
            .expect("slab allocator inserts beyond 1024 functions");
        let mut ret = JitReturn::zeroed();
        unsafe { (entry.c_entry)(&mut ctx, 0, std::ptr::null(), 0, &mut ret) };
        assert_eq!(ret.tag, JitReturn::TAG_ONE);
        assert_eq!(untag_fixnum(ret.single_value), 99);
    }
    assert_eq!(cache.len(), 1500);
}

#[test]
fn jit_backend_swap_defers_reclamation_until_native_calls_return() {
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    let original = Arc::new(CraneliftJitBackend::new().unwrap());
    let replacement = Arc::new(CraneliftJitBackend::new().unwrap());
    runtime.set_jit_backend(original.clone());

    use std::sync::atomic::{AtomicBool, Ordering};
    let swap_next_call = AtomicBool::new(false);
    runtime
        .register_function("swap-backend", {
            let original = original.clone();
            let replacement = replacement.clone();
            move |runtime, _| {
                if swap_next_call.swap(true, Ordering::Relaxed) && original.code_cache().len() == 2
                {
                    runtime.set_jit_backend(replacement.clone());
                    runtime.load_str("(compile 'jit-live (symbol-function 'jit-live))")?;
                    wait_for_native_installation(runtime);
                    runtime.collect();
                    assert_eq!(
                        original.code_cache().len(),
                        2,
                        "active revision is still executing"
                    );
                }
                Ok(vec![runtime.nil()])
            }
        })
        .unwrap();
    runtime
        .load_str(
            "(defun jit-live () (swap-backend) 73)
         (defun jit-driver () (+ 1 (jit-live)))",
        )
        .unwrap();
    let driver = runtime.eval("#'jit-driver").unwrap().pop().unwrap();
    let saved = runtime.eval("#'jit-live").unwrap().pop().unwrap();
    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    for function in [&saved, &driver] {
        runtime.call(&compile, &[&runtime.nil(), function]).unwrap();
        wait_for_native_installation(&runtime);
    }
    assert_eq!(runtime.call(&driver, &[]).unwrap()[0].as_fixnum(), Some(74));
    assert_eq!(runtime.call(&driver, &[]).unwrap()[0].as_fixnum(), Some(74));
    assert_eq!(
        original.code_cache().len(),
        1,
        "old active revision was released"
    );

    for _ in 0..100 {
        runtime
            .load_str("(defun jit-live () 13) (compile 'jit-live (symbol-function 'jit-live))")
            .unwrap();
    }
    runtime.collect();
    assert!((1..=2).contains(&replacement.code_cache().len()));
    assert_eq!(runtime.call(&driver, &[]).unwrap()[0].as_fixnum(), Some(14));
    assert_eq!(runtime.call(&saved, &[]).unwrap()[0].as_fixnum(), Some(73));

    drop(saved);
    drop(driver);
    runtime
        .load_str("(fmakunbound 'jit-live) (fmakunbound 'jit-driver)")
        .unwrap();
    runtime.collect();
    assert!(original.code_cache().is_empty());
    assert!(replacement.code_cache().is_empty());
    assert_eq!(runtime.poll_jit_completions(), 0);
}

fn start_runtime_with_immediate_jit() -> Runtime {
    let rt = Runtime::start().expect("start runtime");
    let backend = Arc::new(CraneliftJitBackend::new().expect("create JIT backend"));
    rt.set_jit_backend(backend as Arc<dyn JitBackend>);
    rt.set_jit_threshold(1);
    rt
}

fn wait_for_native_installation(runtime: &Runtime) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.poll_jit_completions() == 0 {
        assert!(Instant::now() < deadline, "JIT compilation did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wait_for_native_functions(runtime: &Runtime, functions: &[&zkcl_runtime::Root<'_>]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while functions
        .iter()
        .any(|function| runtime.machine_code(function).unwrap().is_none())
    {
        assert!(Instant::now() < deadline, "JIT compilation did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn native_primary_calls_keep_binding_values_and_nonlocal_control() {
    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.register_function("scalar-collect", |rt, _| {
        rt.collect();
        Ok(vec![])
    })
    .unwrap();
    rt.load_str(
        "(defun scalar-child (x y) (+ x y))
         (defun scalar-parent (x y) (list (scalar-child x y)))
         (defun scalar-value-parent (f x) (list (funcall f x)))
         (defun scalar-move (x y) (scalar-collect) (values y x))
         (defun scalar-live-argument (x y) (list (scalar-move x y) x))
         (defun scalar-self (n) (if (zerop n) 7 (1+ (scalar-self (1- n)))))",
    )
    .unwrap();
    rt.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));
    for name in [
        "scalar-child",
        "scalar-parent",
        "scalar-value-parent",
        "scalar-move",
        "scalar-live-argument",
        "scalar-self",
    ] {
        rt.eval(&format!("(compile nil #'{name})")).unwrap();
        wait_for_native_installation(&rt);
    }
    rt.load_str(
        "(defparameter *scalar-saved-child* #'scalar-child)
         (defparameter *scalar-saved-self* #'scalar-self)
         (assert (equal (scalar-parent 2 3) '(5)))
         (assert (= (scalar-self 5) 12))
         (assert (equal (scalar-value-parent #'identity :live) '(:live)))
         (let* ((x (list 'first)) (y (list 'second)) (result (scalar-live-argument x y)))
           (assert (eq y (first result)))
           (assert (eq x (second result))))
         (let ((*trace-output* (make-string-output-stream)))
           (unwind-protect
               (progn (trace scalar-child)
                      (assert (equal (scalar-parent 3 4) '(7)))
                      (assert (search \"SCALAR-CHILD\" (get-output-stream-string *trace-output*))))
             (untrace scalar-child)))
         (defun scalar-child (x y) (values y x))
         (assert (equal (scalar-parent 2 3) '(3)))
         (assert (= (funcall *scalar-saved-child* 2 3) 5))
         (defun scalar-child (x y) (declare (ignore x y)) (values))
         (assert (equal (scalar-parent 2 3) '(nil)))
         (defun scalar-child (x) x)
         (assert (eq :arity (handler-case (scalar-parent 2 3) (program-error () :arity))))
         (defun scalar-child (x y) (declare (ignore x)) (throw 'scalar-throw y))
         (assert (= (catch 'scalar-throw (scalar-parent 2 3)) 3))
         (assert (equal (scalar-value-parent (lambda (x) (values x :ignored)) :live) '(:live)))
         (defun scalar-self (n) (declare (ignore n)) 30)
         (assert (= (funcall *scalar-saved-self* 1) 31))
         (assert (= (funcall *scalar-saved-self* 0) 7))",
    )
    .unwrap();
}

#[cfg(feature = "direct")]
#[test]
fn cranelift_worker_installs_a_register_leaf_alongside_the_general_entry() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use zkcl_runtime::native::JitFunction;

    struct InspectingBackend {
        backend: CraneliftJitBackend,
        leaf_installed: AtomicBool,
    }

    // SAFETY: The production backend validates and owns both entries. This
    // adapter inspects completion metadata without modifying it.
    unsafe impl JitBackend for InspectingBackend {
        unsafe fn submit(&self, request: JitCompilationRequest) {
            unsafe { self.backend.submit(request) };
        }

        fn poll_completions(&self) -> Vec<JitFunction> {
            let ready = self.backend.poll_completions();
            for entry in &ready {
                if entry.leaf_entry.is_some() {
                    assert!(entry.c_entry.is_some());
                    assert!(entry.resumable);
                    self.leaf_installed.store(true, Ordering::Relaxed);
                }
            }
            ready
        }
    }

    let rt = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    rt.load_str(
        "(defun hybrid-sum (x y) (+ x y))
         (defun hybrid-caller (x y) (cons (hybrid-sum x y) nil))",
    )
    .unwrap();
    let backend = Arc::new(InspectingBackend {
        backend: CraneliftJitBackend::new().unwrap(),
        leaf_installed: AtomicBool::new(false),
    });
    rt.set_jit_backend(backend.clone());
    rt.eval("(compile nil #'hybrid-sum)").unwrap();
    wait_for_native_installation(&rt);
    assert!(backend.leaf_installed.load(Ordering::Relaxed));
    rt.eval("(compile nil #'hybrid-caller)").unwrap();
    wait_for_native_installation(&rt);
    rt.eval("(assert (equal (hybrid-caller 2 3) '(5)))")
        .unwrap();
    rt.eval(&format!(
        "(assert (equal (hybrid-caller {FIXNUM_MAX} 1) '({})))",
        FIXNUM_MAX + 1
    ))
    .unwrap();
    rt.set_step_budget(Some(10_000));
    rt.eval("(assert (equal (hybrid-caller 2 3) '(5)))")
        .unwrap();
}

#[test]
fn saved_jit_function_uses_the_current_binding_for_self_calls() {
    let rt = start_runtime_with_immediate_jit();
    rt.load_str(
        "(defun native-fastpath-self (n)
           (if (= n 0) 10 (native-fastpath-self (- n 1))))",
    )
    .unwrap();

    let three = rt.fixnum(3).unwrap();
    let zero = rt.fixnum(0).unwrap();
    assert_eq!(
        rt.eval("(native-fastpath-self 1)").unwrap()[0].as_fixnum(),
        Some(10)
    );
    wait_for_native_installation(&rt);

    let saved = rt.eval("#'native-fastpath-self").unwrap().pop().unwrap();
    rt.eval("(defun native-fastpath-self (n) 99)").unwrap();
    assert_eq!(
        rt.call(&saved, &[&three]).unwrap()[0].as_fixnum(),
        Some(99),
        "a saved old function must resolve a rebound self name dynamically"
    );
    assert_eq!(
        rt.call(&saved, &[&zero]).unwrap()[0].as_fixnum(),
        Some(10),
        "the saved function object must keep executing its old body"
    );
    rt.collect();
    assert_eq!(rt.call(&saved, &[&three]).unwrap()[0].as_fixnum(), Some(99));
}

#[test]
fn native_self_calls_recurse_after_collection() {
    let rt = start_runtime_with_immediate_jit();
    rt.register_function("native-fastpath-force-gc", |rt, _| {
        rt.collect();
        Ok(vec![rt.nil()])
    })
    .expect("register native-fastpath-force-gc");
    rt.load_str(
        "(defun native-fastpath-gc (n)
           (if (= n 0) 42
               (progn
                 (native-fastpath-force-gc)
                 (native-fastpath-gc (- n 1)))))",
    )
    .unwrap();

    assert_eq!(
        rt.eval("(native-fastpath-gc 1)").unwrap()[0].as_fixnum(),
        Some(42)
    );
    wait_for_native_installation(&rt);
    assert_eq!(
        rt.eval("(native-fastpath-gc 3)").unwrap()[0].as_fixnum(),
        Some(42)
    );
}

#[test]
fn native_recursive_addition_promotes_a_result_past_the_fixnum_limit() {
    let rt = start_runtime_with_immediate_jit();
    rt.load_str(&format!(
        "(defun native-fastpath-overflow (n)
           (if (= n 0)
               {FIXNUM_MAX}
               (+ (native-fastpath-overflow (- n 1)) 1)))",
    ))
    .unwrap();

    assert_eq!(
        rt.eval("(native-fastpath-overflow 0)").unwrap()[0].as_fixnum(),
        Some(FIXNUM_MAX as isize)
    );
    wait_for_native_installation(&rt);
    let overflow = rt.eval("(native-fastpath-overflow 1)").unwrap();
    assert_eq!(rt.repr(&overflow[0]).unwrap(), (FIXNUM_MAX + 1).to_string());
}

#[test]
fn native_float_recurrences_keep_the_accumulator_unboxed() {
    use zkcl_runtime::ProfilerConfig;

    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    runtime
        .load_str(
            "(defun checked-double-loop (n x scale offset)
               (declare (type (integer 0 1000000) n)
                        (type double-float x scale offset)
                        (optimize (speed 3) (safety 1) (debug 0)))
               (dotimes (i n x) (setf x (+ (* scale x) offset))))
             (defun observed-double-loop (n x scale offset)
               (dotimes (i n x) (setf x (+ (* scale x) offset))))",
        )
        .unwrap();
    assert_eq!(
        runtime
            .eval("(observed-double-loop 4 1d0 1d0 0.25d0)")
            .unwrap()[0]
            .as_double_float(),
        Some(2.0)
    );
    runtime.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));
    for name in ["checked-double-loop", "observed-double-loop"] {
        runtime.eval(&format!("(compile nil #'{name})")).unwrap();
        wait_for_native_installation(&runtime);
    }

    runtime.start_profiling(ProfilerConfig::default()).unwrap();
    for name in ["checked-double-loop", "observed-double-loop"] {
        assert_eq!(
            runtime
                .eval(&format!("({name} 1000 1d0 1d0 0.25d0)"))
                .unwrap()[0]
                .as_double_float(),
            Some(251.0)
        );
    }
    let report = runtime.stop_profiling();
    for name in ["CHECKED-DOUBLE-LOOP", "OBSERVED-DOUBLE-LOOP"] {
        let row = report
            .functions
            .iter()
            .find(|row| row.name == name)
            .unwrap();
        assert_eq!(row.native_calls, 1, "{row:?}");
        assert!(row.self_allocated_bytes <= 64, "{row:?}");
    }
    let result = runtime
        .eval(
            "(let ((epsilon (scale-float 1d0 -27)))
               (checked-double-loop 1 (+ 1d0 epsilon) (- 1d0 epsilon) -1d0))",
        )
        .unwrap();
    assert_eq!(result[0].as_double_float().unwrap().to_bits(), 0);
    let result = runtime
        .eval("(observed-double-loop 0 :unchanged 0 0)")
        .unwrap();
    assert_eq!(runtime.repr(&result[0]).unwrap(), ":UNCHANGED");
    let result = runtime
        .eval(
            "(handler-case (checked-double-loop -1 1 1 0)
               (type-error (c) (type-error-datum c)))",
        )
        .unwrap();
    assert_eq!(result[0].as_fixnum(), Some(-1));
}

#[test]
fn live_float_intermediates_survive_gc_before_arithmetic_resumes() {
    use std::sync::Mutex;
    use zkcl_runtime::native::JitFunction;

    struct SynchronousBackend {
        backend: CraneliftJitBackend,
        completed: Mutex<Vec<JitFunction>>,
    }

    // SAFETY: The adapter publishes Cranelift's owned completion from the original snapshot.
    unsafe impl JitBackend for SynchronousBackend {
        unsafe fn submit(&self, request: JitCompilationRequest) {
            let compiled = self.backend.compile_now(&request).unwrap();
            self.completed.lock().unwrap().push(compiled);
        }

        fn poll_completions(&self) -> Vec<JitFunction> {
            std::mem::take(&mut *self.completed.lock().unwrap())
        }
    }

    use std::sync::atomic::{AtomicI32, Ordering};
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    let collections = Arc::new(AtomicI32::new(0));
    let calls = Arc::clone(&collections);
    runtime
        .register_function("profiled-float-collect", move |rt, _| {
            rt.collect();
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![rt.nil()])
        })
        .unwrap();
    runtime
        .load_str(
            "(defun profiled-f32 (a b c marker)
               (let ((product (* a b)))
                 (profiled-float-collect)
                 (values (+ product c) marker)))
             (defun profiled-f64 (a b c marker)
               (let ((difference (- a b)))
                 (profiled-float-collect)
                 (values (/ difference c) marker)))",
        )
        .unwrap();
    for warm in [
        "(profiled-f32 1f0 2f0 3f0 nil)",
        "(profiled-f64 7d0 1d0 2d0 nil)",
    ] {
        runtime.eval(warm).unwrap();
    }

    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    let backend = Arc::new(SynchronousBackend {
        backend: CraneliftJitBackend::new().unwrap(),
        completed: Mutex::new(Vec::new()),
    });
    runtime.set_jit_backend(backend.clone());
    for (name, instructions) in [
        ("#'profiled-f32", [0x1e20_0800, 0x1e20_2800]), // FMUL S, FADD S
        ("#'profiled-f64", [0x1e60_3800, 0x1e60_1800]), // FSUB D, FDIV D
    ] {
        let function = runtime.eval(name).unwrap().pop().unwrap();
        runtime
            .call(&compile, &[&runtime.nil(), &function])
            .unwrap();
        let code_id = backend.completed.lock().unwrap()[0].code_id;
        assert_eq!(runtime.poll_jit_completions(), 1);
        let machine = backend.backend.code_cache().get(code_id).unwrap();
        if cfg!(all(target_arch = "aarch64", target_endian = "little")) {
            let code = &machine.executable_code;
            // SAFETY: The owner keeps the entire immutable machine-code slice mapped.
            let bytes = unsafe { std::slice::from_raw_parts(code.as_ptr(), code.len()) };
            for expected in instructions {
                assert!(
                    bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|&word| { u32::from_le_bytes(word) & 0xffe0_fc00 == expected }),
                    "{name} is missing scalar instruction {expected:08x}"
                );
            }
        }
    }

    let before = collections.load(std::sync::atomic::Ordering::Relaxed);
    let rounded = runtime
        .eval(
            "(let ((epsilon (scale-float 1f0 -13)) (marker (list :kept)))
               (multiple-value-bind (answer saved)
                   (profiled-f32 (+ 1f0 epsilon) (- 1f0 epsilon) -1f0 marker)
                 (assert (eq marker saved)) answer))",
        )
        .unwrap();
    assert_eq!(rounded[0].as_single_float().unwrap().to_bits(), 0);

    let negative_zero = runtime.eval("(profiled-f64 -0d0 0d0 2d0 nil)").unwrap();
    assert_eq!(
        negative_zero[0].as_double_float().unwrap().to_bits(),
        (-0.0_f64).to_bits()
    );

    let mixed = runtime
        .eval(
            "(let ((marker (list :kept)))
               (multiple-value-bind (answer saved) (profiled-f32 2f0 3f0 1d0 marker)
                 (assert (eq marker saved)) answer))",
        )
        .unwrap();
    assert_eq!(mixed[0].as_double_float(), Some(7.0));

    let conditions = runtime
        .eval(
            "(values
               (handler-case
                   (profiled-f32 most-positive-single-float 1f0 most-positive-single-float nil)
                 (floating-point-overflow (c)
                   (and (eq '+ (arithmetic-error-operation c))
                        (equal (list most-positive-single-float most-positive-single-float)
                               (arithmetic-error-operands c)))))
               (handler-case (profiled-f64 7d0 1d0 -0d0 nil)
                 (division-by-zero (c)
                   (and (eq '/ (arithmetic-error-operation c))
                        (equal '(6d0 -0d0) (arithmetic-error-operands c))))))",
        )
        .unwrap();
    assert_eq!(conditions.len(), 2);
    assert!(conditions.iter().all(|condition| condition.is_t()));
    assert_eq!(
        collections.load(std::sync::atomic::Ordering::Relaxed) - before,
        5
    );
}
