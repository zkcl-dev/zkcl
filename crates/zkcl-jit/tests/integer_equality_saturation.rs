use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zkcl_jit::CraneliftJitBackend;
use zkcl_opt::FIXNUM_MAX;
use zkcl_runtime::Runtime;
use zkcl_runtime::native::{JitBackend, JitCompilationRequest, JitCompletions, JitFunction};

const SOURCE: &str = "
(defun integer-equality-saturation (x)
  (if (<= -16 x)
      (if (<= x 16)
          (+ (+ (* 2 x) 7) (* 3 x))
          (+ (+ (* 2 x) 8) (* 3 x)))
      (+ (+ (* 2 x) 9) (* 3 x))))
";

struct RecordingBackend {
    backend: CraneliftJitBackend,
    submitted_id: Mutex<Option<usize>>,
}

// SAFETY: The adapter passes each snapshot and the corresponding completion
// through Cranelift, retaining only a code ID for the test assertions.
unsafe impl JitBackend for RecordingBackend {
    unsafe fn submit(&self, request: JitCompilationRequest) {
        // SAFETY: Forward the caller's submitted snapshot unchanged.
        unsafe { self.backend.submit(request) };
    }

    unsafe fn submit_explicit(&self, request: JitCompilationRequest) -> bool {
        *self.submitted_id.lock().unwrap() = Some(request.code_id);
        // SAFETY: Forward the caller's submitted snapshot unchanged.
        unsafe { self.backend.submit_explicit(request) }
    }

    fn poll_completions(&self) -> Vec<JitFunction> {
        self.backend.poll_completions()
    }

    fn poll_for(
        &self,
        belongs_to_runtime: &mut dyn FnMut(usize, u64) -> bool,
    ) -> Option<JitCompletions> {
        self.backend.poll_for(belongs_to_runtime)
    }

    fn release(&self, code_id: usize, revision: u64) {
        self.backend.release(code_id, revision);
    }
}

fn check_installed_polynomial(runtime: &Runtime, enabled: bool) -> usize {
    let backend = Arc::new(RecordingBackend {
        backend: CraneliftJitBackend::with_integer_equality_saturation(enabled).unwrap(),
        submitted_id: Mutex::new(None),
    });
    runtime.set_jit_backend(backend.clone());
    runtime.load_str(SOURCE).unwrap();
    runtime.eval("(integer-equality-saturation 2)").unwrap();
    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    let function = runtime
        .eval("#'integer-equality-saturation")
        .unwrap()
        .pop()
        .unwrap();
    runtime
        .call(&compile, &[&runtime.nil(), &function])
        .unwrap();
    let code_id = backend.submitted_id.lock().unwrap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while runtime.poll_jit_completions() == 0 {
        assert!(Instant::now() < deadline, "JIT compilation did not finish");
        std::thread::sleep(Duration::from_millis(1));
    }
    let machine = backend.backend.code_cache().get(code_id).unwrap();
    for x in -18isize..=18 {
        let result = runtime
            .eval(&format!("(integer-equality-saturation {x})"))
            .unwrap();
        let constant = if x < -16 {
            9
        } else if x > 16 {
            8
        } else {
            7
        };
        assert_eq!(result[0].as_fixnum(), Some(5 * x + constant));
    }
    let big = runtime
        .eval(&format!("(integer-equality-saturation {FIXNUM_MAX})"))
        .unwrap();
    assert_eq!(
        runtime.repr(&big[0]).unwrap(),
        (i128::from(FIXNUM_MAX) * 5 + 8).to_string()
    );
    runtime
        .eval("(assert (eql (integer-equality-saturation 0.5d0) 9.5d0))")
        .unwrap();
    assert!(runtime.eval("(integer-equality-saturation :bad)").is_err());
    machine.executable_code.len()
}

#[test]
fn explicit_integer_equality_saturation_shrinks_polynomial_machine_code() {
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    let original = check_installed_polynomial(&runtime, false);
    let optimized = check_installed_polynomial(&runtime, true);
    assert!(
        optimized < original,
        "{optimized} optimized bytes, {original} baseline bytes"
    );
}
