use std::time::Instant;

use zkcl::Runtime;

mod common;

use common::Workload;

fn main() {
    let runtime_start = Instant::now();
    let runtime = Runtime::start().expect("runtime start");
    let startup_duration = runtime_start.elapsed();

    // 1. closures_loop: exercises CaptureKind::Value (immutable `step`)
    //    and CaptureKind::MutableCell (`acc` mutated via `setq`) inside a loop.
    let closure_loop = Workload {
        source: "(lambda (n)
      (let ((step 3)
            (acc 0))
        (let ((advance (lambda (k)
                         (setq acc (+ acc step k))
                         acc)))
          (let ((i 0))
            (tagbody
              again
                (if (= i n) (go done))
                (funcall advance i)
                (setq i (+ i 1))
                (go again)
              done)
            acc))))",
        input: 10_000,
        expected: 50_025_000,
    };

    // 2. clos_dispatch: CLOS defclass / defgeneric / defmethod polymorphic & monomorphic loops.
    runtime
        .load_str(
            "(defclass bench-shape () ((scale :initarg :scale :accessor bench-shape-scale)))
         (defclass bench-rect (bench-shape) ((w :initarg :w) (h :initarg :h)))
         (defclass bench-square (bench-shape) ((side :initarg :side)))
         (defgeneric bench-area (shape))
         (defmethod bench-area ((s bench-rect))
           (* (slot-value s 'scale) (* (slot-value s 'w) (slot-value s 'h))))
         (defmethod bench-area ((s bench-square))
           (let ((side (slot-value s 'side)))
             (* (slot-value s 'scale) (* side side))))",
        )
        .unwrap();

    let monomorphic_dispatch = Workload {
        source: "(lambda (n)
      (let ((r (make-instance 'bench-rect :scale 2 :w 3 :h 4))
            (i 0)
            (total 0))
        (tagbody
          again
            (if (= i n) (go done))
            (setq total (+ total (bench-area r)))
            (setq i (+ i 1))
            (go again)
          done)
        total))",
        input: 500,
        expected: 12_000,
    };

    let polymorphic_dispatch = Workload {
        source: "(lambda (n)
      (let ((r (make-instance 'bench-rect :scale 2 :w 3 :h 4))
            (sq (make-instance 'bench-square :scale 2 :side 5))
            (i 0)
            (total 0))
        (tagbody
          again
            (if (= i n) (go done))
            (setq total (+ total (bench-area (if (= (mod i 2) 0) r sq))))
            (setq i (+ i 1))
            (go again)
          done)
        total))",
        input: 500,
        expected: 18_500,
    };

    // 3. array_fill_sum: simple-vector fill & aref summation loop.
    let array_fill_and_sum = Workload {
        source: "(lambda (n)
      (let ((vec (make-array n :initial-element 0))
            (i 0)
            (sum 0))
        (tagbody
          fill
            (if (= i n) (go sum-loop))
            (setf (aref vec i) (+ i 1))
            (setq i (+ i 1))
            (go fill)
          sum-loop
            (setq i 0)
          sum-next
            (if (= i n) (go done))
            (setq sum (+ sum (aref vec i)))
            (setq i (+ i 1))
            (go sum-next)
          done)
        sum))",
        input: 10_000,
        expected: 50_005_000,
    };

    // 4. mv_bind_loop: multiple-value-bind + values loop (exercises Values root traffic).
    runtime
        .load_str("(defun bench-mv-step (a b) (values (+ a 1) (+ b a) (- b a)))")
        .unwrap();
    let multiple_value_binding = Workload {
        source: "(lambda (n)
      (let ((i 0)
            (a 1)
            (b 2)
            (acc 0))
        (tagbody
          again
            (if (= i n) (go done))
            (multiple-value-bind (x y z) (bench-mv-step a b)
              (setq a x)
              (setq b (+ y z))
              (setq acc (+ acc x)))
            (setq i (+ i 1))
            (go again)
          done)
        acc))",
        input: 10_000,
        expected: 50_015_000,
    };

    // 5. special_bind_loop: dynamic `let` binding on `defvar` across call depth.
    runtime
        .load_str(
            "(defvar *bench-special* 1)
         (defun bench-special-walk (depth)
           (if (= depth 0)
               *bench-special*
               (let ((*bench-special* (+ *bench-special* depth)))
                 (bench-special-walk (- depth 1)))))",
        )
        .unwrap();
    let special_binding = Workload {
        source: "(lambda (n)
      (let ((i 0)
            (sum 0))
        (tagbody
          again
            (if (= i n) (go done))
            (setq sum (+ sum (bench-special-walk 16)))
            (setq i (+ i 1))
            (go again)
          done)
        sum))",
        input: 500,
        expected: 68_500,
    };

    // 6. compile_medium: compiling a medium Lisp function with flet, the, tagbody, and closures.
    let compilation_workload = Workload {
        source: "(lambda (n)
      (flet ((mix (x y)
               (the fixnum (+ (the fixnum x) (the fixnum y)))))
        (let ((scale 3)
              (acc 0)
              (i 0))
          (let ((step-fn (lambda (k)
                           (setq acc (mix acc (* scale k))))))
            (tagbody
              loop-start
                (if (>= i n) (go loop-end))
                (funcall step-fn i)
                (setq i (mix i 1))
                (go loop-start)
              loop-end)
            acc))))",
        input: 100,
        expected: 14_850,
    };

    // Expected results:
    // closures_loop(10_000): sum_{i=0..9999} (3 + i) = 30_000 + 49_995_000 = 50_025_000
    let (closure_loop_median, closure_loop_max) = closure_loop.measure_execution(&runtime, 3);
    // clos_mono(500): 500 * (2 * 3 * 4) = 12_000
    let (monomorphic_dispatch_median, monomorphic_dispatch_max) =
        monomorphic_dispatch.measure_execution(&runtime, 3);
    // clos_poly(500): 250 * 24 + 250 * 50 = 6_000 + 12_500 = 18_500
    let (polymorphic_dispatch_median, polymorphic_dispatch_max) =
        polymorphic_dispatch.measure_execution(&runtime, 3);
    // array_fill_sum(10_000): sum_{i=1..10000} i = 50_005_000
    let (array_fill_and_sum_median, array_fill_and_sum_max) =
        array_fill_and_sum.measure_execution(&runtime, 3);
    // mv_bind_loop(10_000): a starts at 1, increments by 1 each iter -> sum_{k=2..10001} k = 50_015_000
    let (multiple_value_binding_median, multiple_value_binding_max) =
        multiple_value_binding.measure_execution(&runtime, 3);
    // special_bind_loop(500): each walk returns 1 + (16*17/2) = 137 -> 500 * 137 = 68_500
    let (special_binding_median, special_binding_max) =
        special_binding.measure_execution(&runtime, 3);
    // compile_medium: 3 * (0 + ... + 99) = 3 * 4950 = 14_850
    let (compilation_median, compilation_max) =
        compilation_workload.measure_compilation(&runtime, 5);

    println!("=== zkcl VM language workloads ===");
    println!(
        "Runtime::start:          {:>10.3} ms",
        startup_duration.as_secs_f64() * 1e3
    );
    println!(
        "closures_loop (10k):     {:>10.3} ms (max {:>8.3} ms)",
        closure_loop_median.as_secs_f64() * 1e3,
        closure_loop_max.as_secs_f64() * 1e3
    );
    println!(
        "clos_dispatch mono (500):{:>10.3} ms (max {:>8.3} ms)",
        monomorphic_dispatch_median.as_secs_f64() * 1e3,
        monomorphic_dispatch_max.as_secs_f64() * 1e3
    );
    println!(
        "clos_dispatch poly (500):{:>10.3} ms (max {:>8.3} ms)",
        polymorphic_dispatch_median.as_secs_f64() * 1e3,
        polymorphic_dispatch_max.as_secs_f64() * 1e3
    );
    println!(
        "array_fill_sum (10k):    {:>10.3} ms (max {:>8.3} ms)",
        array_fill_and_sum_median.as_secs_f64() * 1e3,
        array_fill_and_sum_max.as_secs_f64() * 1e3
    );
    println!(
        "mv_bind_loop (10k):      {:>10.3} ms (max {:>8.3} ms)",
        multiple_value_binding_median.as_secs_f64() * 1e3,
        multiple_value_binding_max.as_secs_f64() * 1e3
    );
    println!(
        "special_bind_loop (500): {:>10.3} ms (max {:>8.3} ms)",
        special_binding_median.as_secs_f64() * 1e3,
        special_binding_max.as_secs_f64() * 1e3
    );
    println!(
        "compile_medium:          {:>10.3} us (max {:>8.3} us)",
        compilation_median.as_secs_f64() * 1e6,
        compilation_max.as_secs_f64() * 1e6
    );

    if cfg!(feature = "vm-metrics") {
        for (name, workload) in [
            ("closures_loop", &closure_loop),
            ("clos_mono", &monomorphic_dispatch),
            ("clos_poly", &polymorphic_dispatch),
            ("array_fill_sum", &array_fill_and_sum),
            ("mv_bind_loop", &multiple_value_binding),
            ("special_bind_loop", &special_binding),
        ] {
            println!(
                "Metrics ({name}): {:?}",
                workload.execution_metrics(&runtime)
            );
        }
    }
}
