use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use zkcl_bytecode::{Callee, Instruction};
use zkcl_jit::CraneliftJitBackend;
use zkcl_runtime::Runtime;
use zkcl_runtime::native::{
    JitBackend, JitCompilationRequest, JitFloatFormat, JitFunction, JitSiteKind,
};

struct RecordingBackend {
    backend: CraneliftJitBackend,
    compiled: Mutex<Vec<JitFunction>>,
    observed: Mutex<Vec<JitFloatFormat>>,
}

// SAFETY: The adapter records feedback and publishes Cranelift's owned completion
// from the original snapshot.
unsafe impl JitBackend for RecordingBackend {
    unsafe fn submit(&self, request: JitCompilationRequest) {
        self.observed
            .lock()
            .unwrap()
            .extend(
                request
                    .site_feedback
                    .iter()
                    .filter_map(|hint| match hint.kind {
                        JitSiteKind::FloatArrayReader { format } => {
                            let Instruction::Call {
                                callee: Callee::Named(symbol),
                                ..
                            } = request.canonical[hint.canonical_pc]
                            else {
                                panic!("float vector profile did not identify a named call");
                            };
                            assert_eq!(request.cl_const_names[symbol], Some("AREF"));
                            Some(format)
                        }
                        _ => None,
                    }),
            );
        self.compiled
            .lock()
            .unwrap()
            .push(self.backend.compile_now(&request).unwrap());
    }

    fn poll_completions(&self) -> Vec<JitFunction> {
        std::mem::take(&mut *self.compiled.lock().unwrap())
    }
}

#[test]
fn profiled_float_reads_keep_pre_mutation_values_on_vm_resume() {
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    runtime
        .load_str(
            "(defun float-array-mutate (a) (setf (aref a 0) 9d0))
             (let ((index (identity 0)))
               (setf (fdefinition 'float-array-ordered)
                     (lambda (a b)
                       (let ((x (aref a index)))
                         (float-array-write-and-collect a)
                         (* x (aref b index))))))
             (defun float-array-dot (a b n)
               (let ((sum 0f0))
                 (dotimes (i n sum)
                   (setq sum (+ sum (* (aref a i) (aref b i)))))))",
        )
        .unwrap();
    let writes = Arc::new(AtomicI32::new(0));
    let write_count = Arc::clone(&writes);
    runtime
        .register_function("float-array-write-and-collect", move |rt, args| {
            rt.collect();
            write_count.fetch_add(1, Ordering::Relaxed);
            let mutation = rt.eval("#'float-array-mutate")?.pop().unwrap();
            Ok(rt.call(&mutation, &[&args[0]])?)
        })
        .unwrap();
    let warm_dot = "(float-array-dot
                      (make-array 4 :element-type 'single-float :initial-element 2f0)
                      (make-array 4 :element-type 'single-float :initial-element 3f0) 4)";
    let alias = "(let ((a (make-array 1 :element-type 'double-float :initial-element 2d0)))
                   (float-array-ordered a a))";
    assert_eq!(
        runtime.eval(warm_dot).unwrap()[0].as_single_float(),
        Some(24.0)
    );
    assert_eq!(
        runtime.eval(alias).unwrap()[0].as_double_float(),
        Some(18.0)
    );

    runtime
        .load_str("(defun unrelated-array-profile-definition (x) x)")
        .unwrap();

    let backend = Arc::new(RecordingBackend {
        backend: CraneliftJitBackend::new().unwrap(),
        compiled: Mutex::new(Vec::new()),
        observed: Mutex::new(Vec::new()),
    });
    runtime.set_jit_backend(backend.clone());
    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    for (name, format) in [
        ("float-array-dot", JitFloatFormat::F32),
        ("float-array-ordered", JitFloatFormat::F64),
    ] {
        let function = runtime.eval(&format!("#'{name}")).unwrap().pop().unwrap();
        runtime
            .call(&compile, &[&runtime.nil(), &function])
            .unwrap();
        assert_eq!(*backend.observed.lock().unwrap(), [format; 2]);
        backend.observed.lock().unwrap().clear();
        assert_eq!(runtime.poll_jit_completions(), 1);
    }

    assert_eq!(
        runtime.eval(warm_dot).unwrap()[0].as_single_float(),
        Some(24.0)
    );
    let before = writes.load(Ordering::Relaxed);
    assert_eq!(
        runtime.eval(alias).unwrap()[0].as_double_float(),
        Some(18.0)
    );
    let resumed = runtime
        .eval(
            "(float-array-ordered
               (make-array 1 :element-type 'double-float :initial-element 2d0)
               (vector 3d0))",
        )
        .unwrap();
    assert_eq!(resumed[0].as_double_float(), Some(6.0));
    assert_eq!(writes.load(Ordering::Relaxed) - before, 2);

    let displaced = runtime
        .eval(
            "(let* ((a (make-array 4 :element-type 'single-float :initial-element 2f0))
                    (base (make-array 6 :element-type 'single-float :initial-element 3f0))
                    (b (make-array 4 :element-type 'single-float
                                  :displaced-to base :displaced-index-offset 2)))
               (float-array-dot a b 4))",
        )
        .unwrap();
    assert_eq!(displaced[0].as_single_float(), Some(24.0));
    let out_of_bounds = runtime
        .eval(
            "(handler-case
                 (float-array-ordered
                   (make-array 1 :element-type 'double-float :initial-element 2d0)
                   (make-array 0 :element-type 'double-float))
               (error () t))",
        )
        .unwrap();
    assert!(out_of_bounds[0].is_t());
    assert_eq!(writes.load(Ordering::Relaxed) - before, 3);

    let traced = runtime
        .eval(&format!(
            "(let ((*trace-output* (make-string-output-stream)))
               (trace aref)
               (unwind-protect
                   (progn (assert (eql {warm_dot} 24f0))
                          (not (null (search \"AREF\" (get-output-stream-string *trace-output*)))))
                 (untrace aref)))"
        ))
        .unwrap();
    assert!(traced[0].is_t());
}
