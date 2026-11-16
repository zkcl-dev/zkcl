//! Ignored probes for the stage-3 VM budget: absolute interpreter cost per workload.
//!
//! `dispatches` is only populated with `vm-metrics`; without the feature every
//! `record_dispatch` is an `#[inline(always)]` no-op, so the plain run is the honest
//! time and the instrumented run is the honest step count. Run both and compare:
//!
//! `cargo test --release -p zkcl-runtime --lib dispatch_cost -- --ignored --nocapture`
//! `cargo test --release -p zkcl-runtime --features vm-metrics --lib dispatch_cost -- --ignored --nocapture`

use std::time::{Duration, Instant};

use crate::Runtime;
use crate::metrics;

/// Timed repetitions per workload; the best one is reported.
const ROUNDS: usize = 5;

/// One fixed workload: `source` defines `spelling`, which is called on `argument`.
struct Workload {
    name: &'static str,
    source: &'static str,
    spelling: &'static str,
    argument: isize,
}

const WORKLOADS: [Workload; 3] = [
    Workload {
        name: "dispatch-loop",
        source: "(defun probe-dispatch-loop (n) \
                 (let ((i 0)) \
                 (tagbody \
                 again (if (= i n) (go done)) \
                 (setq i (+ i 1)) \
                 (go again) \
                 done i)))",
        spelling: "#'probe-dispatch-loop",
        argument: 1_000_000,
    },
    Workload {
        name: "fixnum-sum",
        source: "(defun probe-fixnum-sum (n) \
                 (let ((acc 0)) \
                 (dotimes (k n acc) \
                 (setq acc (logand (+ acc (* k k)) most-positive-fixnum)))))",
        spelling: "#'probe-fixnum-sum",
        argument: 1_000_000,
    },
    Workload {
        name: "recursive-fib",
        source: "(defun probe-fib (n) \
                 (if (< n 2) n (+ (probe-fib (- n 1)) (probe-fib (- n 2)))))",
        spelling: "#'probe-fib",
        argument: 24,
    },
];

#[test]
#[ignore = "timing probe; run with --ignored --nocapture"]
fn dispatch_cost() {
    let runtime = Runtime::start().expect("probe runtime");
    for workload in &WORKLOADS {
        runtime
            .load_str(workload.source)
            .expect("probe definition loads");
        let function = runtime
            .eval(workload.spelling)
            .expect("probe function designator evaluates")
            .pop()
            .expect("a function designator has one value");
        let argument = runtime.fixnum(workload.argument).expect("probe argument");

        let mut best = Duration::MAX;
        for _ in 0..ROUNDS {
            let started = Instant::now();
            runtime.call(&function, &[&argument]).expect("probe call");
            best = best.min(started.elapsed());
        }

        // One more run with the counters cleared, so the step count and its own cost
        // are measured on the same work. Without the feature the count reads zero.
        metrics::reset();
        let started = Instant::now();
        let values = runtime.call(&function, &[&argument]).expect("probe call");
        let counted = started.elapsed();
        let dispatches = metrics::snapshot().dispatches;
        let steps = if dispatches == 0 {
            "dispatches 0 (build without vm-metrics)".to_owned()
        } else {
            format!(
                "dispatches {dispatches} | {:.2} ns/dispatch",
                counted.as_secs_f64() * 1e9 / dispatches as f64
            )
        };
        println!(
            "{}: best {:.3} ms | counted {:.3} ms | {steps} | value {:?}",
            workload.name,
            best.as_secs_f64() * 1e3,
            counted.as_secs_f64() * 1e3,
            values.first().and_then(|value| value.as_fixnum()),
        );
    }
}
