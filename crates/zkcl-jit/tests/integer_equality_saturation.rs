use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zkcl_jit::CraneliftJitBackend;
use zkcl_opt::FIXNUM_MAX;
use zkcl_runtime::Runtime;
use zkcl_runtime::native::{JitBackend, JitCompilationRequest, JitCompletions, JitFunction};

// The factored coefficient is 8, a pure shift: `3x + 5x` becomes `x << 3`, so the IR
// that shrank must show up as fewer machine-code bytes. Sizes are written in the order
// the test prints them, `baseline -> optimized`, never as an unlabelled pair:
//
//   `2x + 3x`  (5x)  3884 -> 3884  unchanged: the factor fits shift/add sized forms
//   `7x + 11x` (18x) 3880 -> 3884  grew 4 bytes, cause not identified yet
//
// The `18x` row stays out of this assertion, but not because the rewrite is wrong. `apply`
// builds `x * (7 + 11)` (observed in the stage-two trace), and
// `optimize_with_integer_equality_saturation` always runs `propagate_constants_and_copies`
// after it, which folds a sum of two `ConstI64` operands into one constant. The coefficient
// is therefore compile-time and the optimized IR keeps a single multiply; an earlier note
// here claimed the sum was computed at run time, and that claim was wrong. The 4 bytes come
// from lowering, register allocation or jump encoding instead, which is a separate question
// from this property. Asserting on that row would measure an unidentified lowering
// difference, so this test keeps a coefficient that needs no multiply at all.
const SOURCE: &str = "
(defun integer-equality-saturation (x)
  (if (<= -16 x)
      (if (<= x 16)
          (+ (+ (* 3 x) 7) (* 5 x))
          (+ (+ (* 3 x) 8) (* 5 x)))
      (+ (+ (* 3 x) 9) (* 5 x))))
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
        assert_eq!(result[0].as_fixnum(), Some(8 * x + constant));
    }
    let big = runtime
        .eval(&format!("(integer-equality-saturation {FIXNUM_MAX})"))
        .unwrap();
    assert_eq!(
        runtime.repr(&big[0]).unwrap(),
        (i128::from(FIXNUM_MAX) * 8 + 8).to_string()
    );
    runtime
        .eval("(assert (eql (integer-equality-saturation 0.5d0) 11.0d0))")
        .unwrap();
    assert!(runtime.eval("(integer-equality-saturation :bad)").is_err());
    machine.executable_code.len()
}

#[test]
fn explicit_integer_equality_saturation_shrinks_polynomial_machine_code() {
    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    let original = check_installed_polynomial(&runtime, false);
    let optimized = check_installed_polynomial(&runtime, true);
    // Printed unconditionally so a passing run still reports both sizes. The labels are
    // part of the output on purpose: an unlabelled pair was read in both directions
    // during stage two, once as `-4` and once as `+4` for the same two numbers.
    let delta = optimized as i64 - original as i64;
    println!(
        "eqsat machine code: baseline {original} bytes -> optimized {optimized} bytes (delta {delta:+})"
    );
    assert!(
        optimized < original,
        "baseline {original} bytes -> optimized {optimized} bytes (delta {delta:+})"
    );
}
