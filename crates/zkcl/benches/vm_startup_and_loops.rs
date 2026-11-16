use std::time::Instant;

use zkcl::Runtime;

mod common;

use common::Workload;

fn main() {
    let runtime_start = Instant::now();
    let runtime = Runtime::start().expect("runtime start");
    let startup_duration = runtime_start.elapsed();

    let first_eval_start = Instant::now();
    let first_eval_values = runtime.eval("(+ 40 2)").expect("first eval");
    let first_eval_duration = first_eval_start.elapsed();
    assert_eq!(first_eval_values[0].as_fixnum(), Some(42));

    let sum_loop = Workload {
        source: "(lambda (n)
      (let ((i 0) (s 0))
        (tagbody
          again
            (if (= i n) (go done))
            (setq s (+ s i))
            (setq i (+ i 1))
            (go again)
          done)
        s))",
        input: 100_000,
        expected: 4_999_950_000,
    };

    runtime.load_str("(defun bench-step (n) (+ n 1))").unwrap();
    let call_loop = Workload {
        source: "(lambda (n)
      (let ((i 0))
        (tagbody
          again
            (if (= i n) (go done))
            (setq i (bench-step i))
            (go again)
          done)
        i))",
        input: 100_000,
        expected: 100_000,
    };

    let cons_loop = Workload {
        source: "(lambda (n)
      (let ((i 0) (xs nil) (s 0))
        (tagbody
          build
            (if (= i n) (go walk))
            (setq xs (cons i xs))
            (setq i (+ i 1))
            (go build)
          walk
            (if (null xs) (go done))
            (setq s (+ s (car xs)))
            (setq xs (cdr xs))
            (go walk)
          done)
        s))",
        input: 10_000,
        expected: 49_995_000,
    };

    let add = Workload {
        source: "(lambda (x) (+ x 1))",
        input: 41,
        expected: 42,
    };

    let (compile_add_median, compile_add_max) = add.measure_compilation(&runtime, 10);
    let (compile_sum_median, compile_sum_max) = sum_loop.measure_compilation(&runtime, 10);
    let (compile_call_median, compile_call_max) = call_loop.measure_compilation(&runtime, 10);
    let (compile_cons_median, compile_cons_max) = cons_loop.measure_compilation(&runtime, 10);

    let (sum_median, sum_max) = sum_loop.measure_execution(&runtime, 1);
    let (call_median, call_max) = call_loop.measure_execution(&runtime, 1);
    let (cons_median, cons_max) = cons_loop.measure_execution(&runtime, 1);

    println!("=== zkcl VM startup, compilation, and loops ===");
    println!(
        "Runtime::start:        {:>10.3} ms",
        startup_duration.as_secs_f64() * 1e3
    );
    println!(
        "First eval (+ 40 2):   {:>10.3} ms",
        first_eval_duration.as_secs_f64() * 1e3
    );
    println!(
        "Compile add lambda:    {:>10.3} us (max {:>8.3} us)",
        compile_add_median.as_secs_f64() * 1e6,
        compile_add_max.as_secs_f64() * 1e6
    );
    println!(
        "Compile sum loop:      {:>10.3} us (max {:>8.3} us)",
        compile_sum_median.as_secs_f64() * 1e6,
        compile_sum_max.as_secs_f64() * 1e6
    );
    println!(
        "Compile call loop:     {:>10.3} us (max {:>8.3} us)",
        compile_call_median.as_secs_f64() * 1e6,
        compile_call_max.as_secs_f64() * 1e6
    );
    println!(
        "Compile cons loop:     {:>10.3} us (max {:>8.3} us)",
        compile_cons_median.as_secs_f64() * 1e6,
        compile_cons_max.as_secs_f64() * 1e6
    );
    println!(
        "Run sum 100k:          {:>10.3} ms (max {:>8.3} ms)",
        sum_median.as_secs_f64() * 1e3,
        sum_max.as_secs_f64() * 1e3
    );
    println!(
        "Run call 100k:         {:>10.3} ms (max {:>8.3} ms)",
        call_median.as_secs_f64() * 1e3,
        call_max.as_secs_f64() * 1e3
    );
    println!(
        "Run cons 10k:          {:>10.3} ms (max {:>8.3} ms)",
        cons_median.as_secs_f64() * 1e3,
        cons_max.as_secs_f64() * 1e3
    );

    // Keep the coefficients as function arguments so the compiled recurrence
    // runs with both float formats. The truncation is outside the loop.
    runtime
        .load_str(
            "(defun bench-float-loop (n a b x)
           (dotimes (i n x) (setq x (+ (* a x) b))))
         (defun bench-single-loop (n a b x)
           (declare (type fixnum n) (type single-float a b x)
                    (optimize (speed 3) (safety 1)))
           (dotimes (i n x) (setq x (+ (* a x) b))))
         (defun bench-double-loop (n a b x)
           (declare (type fixnum n) (type double-float a b x)
                    (optimize (speed 3) (safety 1)))
           (dotimes (i n x) (setq x (+ (* a x) b))))
         (defun bench-count-below (n a b)
           (let ((count 0))
             (dotimes (i n count) (when (< a b) (incf count)))))",
        )
        .unwrap();
    for (label, source, expected) in [
        (
            "single-float",
            "(lambda (n) (truncate (bench-float-loop n 1f0 0.25f0 -1f0)))",
            3999,
        ),
        (
            "double-float",
            "(lambda (n) (truncate (bench-float-loop n 1d0 0.25d0 -1d0)))",
            3999,
        ),
        (
            "declared single-float",
            "(lambda (n) (if (eql (bench-single-loop n 1f0 0.25f0 -1f0) 3999f0) 1 0))",
            1,
        ),
        (
            "declared double-float",
            "(lambda (n) (if (eql (bench-double-loop n 1d0 0.25d0 -1d0) 3999d0) 1 0))",
            1,
        ),
        (
            "mixed float comparison",
            "(lambda (n) (bench-count-below n 1f0 1.0000000000000002d0))",
            16000,
        ),
    ] {
        let workload = Workload {
            source,
            input: 16_000,
            expected,
        };
        let (median, max) = workload.measure_execution(&runtime, 1);
        println!(
            "Run {label} 16k: {:.3} ms (max {:.3} ms)",
            median.as_secs_f64() * 1e3,
            max.as_secs_f64() * 1e3
        );
    }

    runtime
        .load_str(
            "(defun bench-make-dot-vector (modulus offset zero)
               (let ((vector (make-array 8192 :element-type (type-of zero) :initial-element zero)))
                 (dotimes (i (length vector) vector)
                   (setf (aref vector i) (float (- (mod i modulus) offset) zero)))))
             (defparameter *bench-single-left* (bench-make-dot-vector 7 3 0f0))
             (defparameter *bench-single-right* (bench-make-dot-vector 11 5 0f0))
             (defparameter *bench-double-left* (bench-make-dot-vector 7 3 0d0))
             (defparameter *bench-double-right* (bench-make-dot-vector 11 5 0d0))
             (defun bench-single-dot (n a b)
               (declare (type fixnum n) (type (simple-array single-float (*)) a b)
                        (optimize (speed 3) (safety 1)))
               (let ((sum 0f0))
                 (declare (type single-float sum))
                 (dotimes (i n sum) (incf sum (* (aref a i) (aref b i))))))
             (defun bench-double-dot (n a b)
               (declare (type fixnum n) (type (simple-array double-float (*)) a b)
                        (optimize (speed 3) (safety 1)))
               (let ((sum 0d0))
                 (declare (type double-float sum))
                 (dotimes (i n sum) (incf sum (* (aref a i) (aref b i))))))",
        )
        .unwrap();
    let n = 8192;
    let exact: isize = (0..n).map(|i| (i % 7 - 3) * (i % 11 - 5)).sum();
    for (label, kernel, expected) in [
        (
            "single-float dot",
            "(bench-single-dot n *bench-single-left* *bench-single-right*)",
            format!("{exact}f0"),
        ),
        (
            "double-float dot",
            "(bench-double-dot n *bench-double-left* *bench-double-right*)",
            format!("{exact}d0"),
        ),
    ] {
        let source = format!("(lambda (n) (if (eql {kernel} {expected}) 1 0))");
        let workload = Workload {
            source: &source,
            input: n,
            expected: 1,
        };
        let (median, max) = workload.measure_execution(&runtime, 1);
        println!(
            "Run {label} {n}: {:.3} ms (max {:.3} ms)",
            median.as_secs_f64() * 1e3,
            max.as_secs_f64() * 1e3
        );
    }

    for degree in [4, 8, 10] {
        let mut expr = String::from("x");
        for coefficient in 1..=degree {
            expr = format!("(+ (* {expr} x) {coefficient})");
        }
        let source = format!("(lambda (x) (if (realp {expr}) 1 0))");
        let workload = Workload {
            source: &source,
            input: 2,
            expected: 1,
        };
        let (median, max) = workload.measure_compilation(&runtime, 10);
        println!(
            "Compile Horner degree {degree}: {:.3} us (max {:.3} us)",
            median.as_secs_f64() * 1e6,
            max.as_secs_f64() * 1e6
        );
    }

    if cfg!(feature = "vm-metrics") {
        println!(
            "Metrics (sum 100k):    {:?}",
            sum_loop.execution_metrics(&runtime)
        );
        println!(
            "Metrics (call 100k):   {:?}",
            call_loop.execution_metrics(&runtime)
        );
    }
}
