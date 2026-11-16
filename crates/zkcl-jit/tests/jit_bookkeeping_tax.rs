//! Separates "the compiled code is slow" from "the interpreter pays for the JIT".
//!
//! `bench-sum-loop` in the stage-three baseline is called five times, and the CLI
//! threshold is 32, so it is never compiled: its million-iteration loop runs in the VM
//! in both the plain and the `--jit` run. The 23.7% gap between those two runs therefore
//! cannot come from generated code — it can only come from what an installed backend
//! adds to a call that has no native entry yet.
//!
//! Three configurations differ in nothing but the backend:
//!
//! * no backend at all,
//! * a backend with the threshold at `u32::MAX`, so nothing can ever be compiled,
//! * a backend with the CLI threshold, so hot callees do get compiled.
//!
//! The second minus the first is the bookkeeping tax on interpreted calls, with no
//! compilation in the picture. The third shows where the tax is repaid.
//!
//! The three arms are run **in rotation, one repetition each**, and the tax is read from
//! the per-round paired difference. Running each arm to completion in turn was the first
//! version of this probe and it reported 20.0 ns/call while the machine load climbed from
//! 18.8 to 28.7 between arms — a difference of that size cannot be separated from drift
//! when the arms do not share each round. Rotation makes every round pay the same load.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zkcl_jit::CraneliftJitBackend;
use zkcl_runtime::{Root, Runtime};

/// Four interpreted calls per iteration: `tax-inner` itself, then `*`, `+` and `logand`.
const CALLS_PER_ITERATION: u64 = 4;

const ITERATIONS: usize = 200_000;

/// Odd, so the rotation alternates which arm runs first and which runs last.
const ROUNDS: usize = 9;

/// The threshold the CLI installs (`zkcl-cli/src/main.rs:192`).
const CLI_THRESHOLD: u32 = 32;

const SOURCE: &str = "
(defun tax-inner (a b)
  (logand (+ (* a 3) b) 65535))

(defun tax-loop (n)
  (let ((acc 0))
    (dotimes (k n acc)
      (setq acc (tax-inner acc k)))))
";

#[test]
#[ignore = "timing probe; run with --release -- --ignored --nocapture"]
fn an_installed_backend_taxes_interpreted_calls_before_anything_is_compiled() {
    let plain = Runtime::builder().start().unwrap();
    plain.load_str(SOURCE).unwrap();
    let (plain_function, plain_count) = prepare(&plain);

    let gated = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    gated.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));
    gated.load_str(SOURCE).unwrap();
    let (gated_function, gated_count) = prepare(&gated);

    let compiled = Runtime::builder()
        .jit_threshold(CLI_THRESHOLD)
        .start()
        .unwrap();
    compiled.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));
    compiled.load_str(SOURCE).unwrap();
    let (compiled_function, compiled_count) = prepare(&compiled);

    let mut plain_runs = Vec::with_capacity(ROUNDS);
    let mut gated_runs = Vec::with_capacity(ROUNDS);
    let mut compiled_runs = Vec::with_capacity(ROUNDS);
    for round in 0..ROUNDS {
        if round % 2 == 0 {
            plain_runs.push(run_once(&plain, &plain_function, &plain_count));
            gated_runs.push(run_once(&gated, &gated_function, &gated_count));
            compiled_runs.push(run_once(&compiled, &compiled_function, &compiled_count));
        } else {
            compiled_runs.push(run_once(&compiled, &compiled_function, &compiled_count));
            gated_runs.push(run_once(&gated, &gated_function, &gated_count));
            plain_runs.push(run_once(&plain, &plain_function, &plain_count));
        }
    }

    let reference = plain_runs[0].1.clone();
    for (_, value) in plain_runs.iter().chain(&gated_runs).chain(&compiled_runs) {
        assert_eq!(
            *value, reference,
            "the arms computed different values, so their timings are not comparable"
        );
    }

    let plain_times = times(&plain_runs);
    let gated_times = times(&gated_runs);
    let compiled_times = times(&compiled_runs);

    report("no backend", &plain_times);
    report("backend, threshold u32::MAX", &gated_times);
    report("backend, threshold 32", &compiled_times);

    let tax = paired_ns_per_call(&plain_times, &gated_times);
    let repaid = paired_ns_per_call(&plain_times, &compiled_times);
    println!(
        "tax on interpreted calls: median {:.1} ns/call, mean {:.1} ns/call over {ROUNDS} paired rounds",
        tax.median, tax.mean,
    );
    println!(
        "same load with the hot callee compiled: median {:.1} ns/call, mean {:.1} ns/call",
        repaid.median, repaid.mean,
    );
}

/// Resolves the workload once per runtime so timed rounds contain no name lookup.
fn prepare<'rt>(runtime: &'rt Runtime) -> (Root<'rt>, Root<'rt>) {
    let function = runtime.eval("#'tax-loop").unwrap().pop().unwrap();
    let count = runtime
        .eval(&ITERATIONS.to_string())
        .unwrap()
        .pop()
        .unwrap();
    (function, count)
}

/// One timed run of the whole workload: no compilation setup, no warmup, no reuse.
fn run_once(runtime: &Runtime, function: &Root<'_>, count: &Root<'_>) -> (Duration, String) {
    let started = Instant::now();
    let produced = runtime.call(function, &[count]).unwrap();
    let elapsed = started.elapsed();
    (elapsed, runtime.repr(&produced[0]).unwrap())
}

fn times(runs: &[(Duration, String)]) -> Vec<Duration> {
    runs.iter().map(|(elapsed, _)| *elapsed).collect()
}

/// A paired difference between two arms, in nanoseconds per interpreted call.
struct PairedCost {
    median: f64,
    mean: f64,
}

/// Takes the difference **within each round**, so a load change affects both sides of
/// the subtraction instead of one.
fn paired_ns_per_call(baseline: &[Duration], other: &[Duration]) -> PairedCost {
    let mut deltas: Vec<f64> = baseline
        .iter()
        .zip(other)
        .map(|(baseline, other)| (other.as_secs_f64() - baseline.as_secs_f64()) * 1e9 / calls())
        .collect();
    let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
    deltas.sort_by(|left, right| left.total_cmp(right));
    PairedCost {
        median: deltas[deltas.len() / 2],
        mean,
    }
}

fn calls() -> f64 {
    ((ITERATIONS as u64) * CALLS_PER_ITERATION) as f64
}

fn report(label: &str, times: &[Duration]) {
    let mut sorted = times.to_vec();
    sorted.sort();
    let median = sorted[sorted.len() / 2];
    let best = sorted[0];
    println!(
        "{label}: median {:.2} ms ({:.1} ns/call), best {:.2} ms, {ROUNDS} rounds",
        median.as_secs_f64() * 1e3,
        median.as_secs_f64() * 1e9 / calls(),
        best.as_secs_f64() * 1e3,
    );
}
