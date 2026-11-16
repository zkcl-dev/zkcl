use std::sync::{Arc, Mutex};

use zkcl_bytecode::{Callee, Instruction};
use zkcl_jit::CraneliftJitBackend;
use zkcl_runtime::Runtime;
use zkcl_runtime::native::{
    JitBackend, JitCompilationRequest, JitFloatFormat, JitFunction, JitSiteKind,
};

struct StoreBackend {
    compiler: CraneliftJitBackend,
    submitted: Mutex<Vec<JitFunction>>,
    formats: Mutex<Vec<JitFloatFormat>>,
}

// SAFETY: The adapter inspects feedback and publishes Cranelift's owned completion
// from the original snapshot.
unsafe impl JitBackend for StoreBackend {
    unsafe fn submit(&self, request: JitCompilationRequest) {
        for hint in &request.site_feedback {
            if let JitSiteKind::FloatArrayWriter { format } = hint.kind {
                let Instruction::Call {
                    callee: Callee::Named(symbol),
                    notinline: false,
                    ref args,
                    ..
                } = request.canonical[hint.canonical_pc]
                else {
                    panic!("float store feedback did not identify an eligible named call");
                };
                assert_eq!(request.cl_const_names[symbol], Some("FUNCALL"));
                assert_eq!(args.len(), 4);
                self.formats.lock().unwrap().push(format);
            }
        }
        self.submitted
            .lock()
            .unwrap()
            .push(self.compiler.compile_now(&request).unwrap());
    }

    fn poll_completions(&self) -> Vec<JitFunction> {
        std::mem::take(&mut *self.submitted.lock().unwrap())
    }
}

#[test]
fn profiled_float_stores_keep_setf_operand_evaluation_order() {
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    runtime
        .register_function("float-write-collect", |rt, _| {
            rt.collect();
            Ok(vec![rt.nil()])
        })
        .unwrap();
    runtime
        .load_str(
            "(defvar *write-order* nil)
             (defvar *write-f64* (make-array 1 :element-type 'double-float :initial-element 0d0))
             (defvar *write-f32* (make-array 2 :element-type 'single-float
                                             :initial-contents '(0f0 2f0)))
             (defun write-arg (tag value)
               (push tag *write-order*)
               (when (eq tag :value) (float-write-collect))
               value)
             (defun write-ordered (a i rhs)
               (let ((stored (setf (aref (write-arg :array a) (write-arg :index i))
                                   (values (write-arg :value rhs) :discarded))))
                 (values stored (eq stored rhs))))
             (defun write-alias (a b)
               (let ((stored (setf (aref a 0) (+ (aref b 1) 1f0))))
                 (values stored (aref b 0))))
             (defun write-notinline (a rhs)
               (declare (notinline (setf aref)))
               (let ((stored (setf (aref a 0) rhs))) (eq stored rhs)))
             (defun write-when-traced (a rhs)
               (let ((stored (setf (aref a 0) rhs))) (eq stored rhs)))",
        )
        .unwrap();
    let check_ordered = || {
        let values = runtime
            .eval(
                "(let ((*write-order* nil))
                   (setf (aref *write-f64* 0) 4d0)
                   (multiple-value-bind (stored same) (write-ordered *write-f64* 0 -0d0)
                     (assert (equal *write-order* '(:value :index :array)))
                     (assert same)
                     (values stored (aref *write-f64* 0))))",
            )
            .unwrap();
        assert_eq!(values.len(), 2);
        for value in values {
            assert_eq!(
                value.as_double_float().unwrap().to_bits(),
                (-0.0f64).to_bits()
            );
        }
    };
    let check_alias = || {
        let values = runtime
            .eval("(progn (setf (aref *write-f32* 0) 4f0) (write-alias *write-f32* *write-f32*))")
            .unwrap();
        assert_eq!(values.len(), 2);
        for value in values {
            assert_eq!(value.as_single_float(), Some(3.0));
        }
    };
    check_ordered();
    check_alias();
    runtime
        .eval("(assert (write-notinline *write-f64* 0d0))")
        .unwrap();
    runtime
        .load_str("(defun unrelated-store-profile-definition (x) x)")
        .unwrap();

    let backend = Arc::new(StoreBackend {
        compiler: CraneliftJitBackend::new().unwrap(),
        submitted: Mutex::new(Vec::new()),
        formats: Mutex::new(Vec::new()),
    });
    runtime.set_jit_backend(backend.clone());
    let install = |name: &str, formats: &[JitFloatFormat]| {
        runtime.eval(&format!("(compile nil #'{name})")).unwrap();
        assert_eq!(*backend.formats.lock().unwrap(), formats);
        backend.formats.lock().unwrap().clear();
        assert_eq!(runtime.poll_jit_completions(), 1);
    };
    install("write-ordered", &[JitFloatFormat::F64]);
    install("write-alias", &[JitFloatFormat::F32]);
    install("write-notinline", &[]);
    check_ordered();
    check_alias();

    runtime
        .eval(
            "(let ((*write-order* nil))
               (assert (handler-case (write-ordered *write-f64* 1 9d0)
                         (error () t)))
               (assert (equal *write-order* '(:value :index :array)))
               (setq *write-order* nil)
               (assert (handler-case (write-ordered *write-f64* 0 9f0)
                         (type-error (c) (eql (type-error-datum c) 9f0))))
               (assert (equal *write-order* '(:value :index :array)))
               (assert (zerop (aref *write-f64* 0)))
               (let* ((base (make-array 2 :element-type 'double-float :initial-element 0d0))
                      (view (make-array 1 :element-type 'double-float :displaced-to base
                                          :displaced-index-offset 1)))
                 (setq *write-order* nil)
                 (assert (equal (multiple-value-list (write-ordered view 0 7d0)) '(7d0 t)))
                 (assert (equal *write-order* '(:value :index :array)))
                 (assert (eql (aref base 1) 7d0))))",
        )
        .unwrap();

    let call_traced = |expression| {
        let shown = runtime
            .eval(&format!(
                "(let ((*trace-output* (make-string-output-stream)) (*write-order* nil))
                   {expression}
                   (search \"TRACE FUNCALL: enter\" (get-output-stream-string *trace-output*)))"
            ))
            .unwrap();
        assert!(shown[0].as_fixnum().is_some(), "{expression}");
    };
    runtime.eval("(trace cl:funcall)").unwrap();
    call_traced("(assert (write-when-traced *write-f64* 4d0))");
    install("write-when-traced", &[]);
    call_traced("(assert (write-when-traced *write-f64* 4d0))");
    call_traced(
        "(progn
           (assert (equal (multiple-value-list (write-ordered *write-f64* 0 5d0)) '(5d0 t)))
           (assert (equal *write-order* '(:value :index :array))))",
    );
    runtime.eval("(untrace cl:funcall)").unwrap();
}
