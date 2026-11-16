#![cfg(all(
    target_endian = "little",
    any(target_arch = "aarch64", target_arch = "x86_64")
))]

use std::sync::{Arc, Mutex};

use zkcl_bytecode::native_abi::I32x4Operation::{Add, BitAnd, BitOr, BitXor, Sub};
use zkcl_jit::CraneliftJitBackend;
use zkcl_runtime::native::{JitBackend, JitCompilationRequest, JitFunction, JitSiteKind};
use zkcl_runtime::{Runtime, metrics};

struct SynchronousBackend {
    compiler: CraneliftJitBackend,
    completed: Mutex<Vec<JitFunction>>,
}

// SAFETY: The adapter checks observed SIMD operations and publishes Cranelift's
// owned completion from the original snapshot.
unsafe impl JitBackend for SynchronousBackend {
    unsafe fn submit(&self, request: JitCompilationRequest) {
        let operations: Vec<_> = request
            .site_feedback
            .iter()
            .filter_map(|site| match site.kind {
                JitSiteKind::SimdI32x4 { operation } => Some(operation),
                _ => None,
            })
            .collect();
        assert_eq!(operations, [Add, Add, BitAnd, BitOr, BitAnd, BitXor, Sub]);
        let compiled = self.compiler.compile_now(&request).unwrap();
        self.completed.lock().unwrap().push(compiled);
    }

    fn poll_completions(&self) -> Vec<JitFunction> {
        std::mem::take(&mut *self.completed.lock().unwrap())
    }
}

#[test]
fn i32x4_chain_executes_natively_and_restores_distinct_pack_identities() {
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    runtime
        .register_function("simd-test-collect", |rt, _| {
            rt.collect();
            Ok(vec![rt.nil()])
        })
        .unwrap();
    runtime
        .load_str(
            "(defparameter *simd-a* (zk-simd:make-pack 'zk-simd:i32x4 2147483647 -2147483648 42 -1))
             (defparameter *simd-b* (zk-simd:make-pack 'zk-simd:i32x4 1 -1 7 2))
             (defun simd-chain (a b another delta)
               (let* ((sum (zk-simd:add a b))
                      (other (zk-simd:add a another))
                      (same (zk-simd:bit-and sum sum))
                      (different (zk-simd:bit-or same other))
                      (masked (zk-simd:bit-and different b)))
                 (zk-simd:sub (zk-simd:bit-xor masked a) delta)))",
        )
        .unwrap();
    let function = runtime.eval("#'simd-chain").unwrap().pop().unwrap();
    let a = runtime.eval("*simd-a*").unwrap().pop().unwrap();
    let b = runtime.eval("*simd-b*").unwrap().pop().unwrap();

    let before_vm = metrics::snapshot();
    let reference = runtime.call(&function, &[&a, &b, &b, &b]).unwrap();
    let after_vm = metrics::snapshot();
    runtime
        .load_str("(defun unrelated-simd-profile-definition (x) x)")
        .unwrap();
    let backend = Arc::new(SynchronousBackend {
        compiler: CraneliftJitBackend::new().unwrap(),
        completed: Mutex::new(Vec::new()),
    });
    runtime.set_jit_backend(backend.clone());
    runtime.eval("(compile nil #'simd-chain)").unwrap();
    let code_id = backend.completed.lock().unwrap()[0].code_id;
    assert_eq!(runtime.poll_jit_completions(), 1);

    if cfg!(all(target_arch = "aarch64", target_endian = "little")) {
        let installed = backend.compiler.code_cache().get(code_id).unwrap();
        let code = &installed.executable_code;
        // SAFETY: The owner keeps the full machine-code slice mapped and immutable.
        let bytes = unsafe { std::slice::from_raw_parts(code.as_ptr(), code.len()) };
        for expected in [
            0x4ea0_8400,
            0x6ea0_8400,
            0x4e20_1c00,
            0x4ea0_1c00,
            0x6e20_1c00,
        ] {
            assert!(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|&word| { u32::from_le_bytes(word) & 0xffe0_fc00 == expected }),
                "missing packed instruction {expected:08x}"
            );
        }
    }

    let before_native = metrics::snapshot();
    let result = runtime.call(&function, &[&a, &b, &b, &b]).unwrap();
    let after_native = metrics::snapshot();
    if after_vm.primitive_calls != before_vm.primitive_calls {
        assert_eq!(after_native.primitive_calls, before_native.primitive_calls);
        assert_eq!(after_native.heap_allocs - before_native.heap_allocs, 1);
    }
    let same_bits = runtime
        .eval("#'zk-simd:same-bits-p")
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        !runtime
            .call(&same_bits, &[&reference[0], &result[0]])
            .unwrap()[0]
            .is_nil()
    );

    runtime
        .eval(
            "(let ((expected (zk-simd:make-pack 'zk-simd:i32x4 2147483646 0 36 -3)))
               (assert (zk-simd:same-bits-p (simd-chain *simd-a* *simd-b* *simd-b* *simd-b*) expected))
               (assert (handler-case (simd-chain *simd-a* *simd-b* *simd-b* :bad)
                         (type-error () t)))
               (loop for (name same-object) in '((zk-simd:bit-and t) (zk-simd:bit-or nil)) do
                 (let ((primitive (symbol-function name))
                       (seen nil))
                   (unwind-protect
                       (progn
                         (setf (symbol-function name)
                               (lambda (x y)
                                 (simd-test-collect)
                                 (unless seen
                                   (assert (eq (eq x y) same-object))
                                   (assert (zk-simd:same-bits-p x y))
                                   (setf seen t))
                                 (funcall primitive x y)))
                         (assert (zk-simd:same-bits-p (simd-chain *simd-a* *simd-b* *simd-b* *simd-b*) expected))
                         (assert seen))
                     (setf (symbol-function name) primitive)))))",
        )
        .unwrap();
}
