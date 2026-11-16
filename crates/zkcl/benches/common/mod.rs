use std::time::{Duration, Instant};

use zkcl::metrics::{self, VmMetrics};
use zkcl::{Root, Runtime};

const BATCHES: usize = 9;

pub struct Workload<'a> {
    pub source: &'a str,
    pub input: isize,
    pub expected: isize,
}

impl Workload<'_> {
    pub fn measure_execution(&self, runtime: &Runtime, repetitions: u32) -> (Duration, Duration) {
        let function = runtime.eval(self.source).unwrap().pop().unwrap();
        let argument = runtime.fixnum(self.input).unwrap();
        let call = || runtime.call(&function, &[&argument]).unwrap();

        self.check_result(&call());
        measure_batches(repetitions, call, |values| self.check_result(values))
    }

    pub fn measure_compilation(&self, runtime: &Runtime, repetitions: u32) -> (Duration, Duration) {
        let form = runtime
            .eval(&format!("'{}", self.source))
            .unwrap()
            .pop()
            .unwrap();
        let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
        let nil = runtime.eval("nil").unwrap().pop().unwrap();

        measure_batches(
            repetitions,
            || {
                runtime
                    .call(&compile, &[&nil, &form])
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap()
            },
            |function| {
                let argument = runtime.fixnum(self.input).unwrap();
                let values = runtime.call(function, &[&argument]).unwrap();
                self.check_result(&values);
            },
        )
    }

    pub fn execution_metrics(&self, runtime: &Runtime) -> VmMetrics {
        let function = runtime.eval(self.source).unwrap().pop().unwrap();
        let argument = runtime.fixnum(self.input).unwrap();
        metrics::reset();
        let values = runtime.call(&function, &[&argument]).unwrap();
        self.check_result(&values);
        metrics::snapshot()
    }

    fn check_result(&self, values: &[Root<'_>]) {
        assert_eq!(
            values.first().and_then(Root::as_fixnum),
            Some(self.expected),
            "unexpected primary value for input {} to {}",
            self.input,
            self.source
        );
    }
}

fn measure_batches<T>(
    repetitions: u32,
    mut operation: impl FnMut() -> T,
    mut check: impl FnMut(&T),
) -> (Duration, Duration) {
    assert!(repetitions > 0);

    let mut per_operation = [Duration::ZERO; BATCHES];
    for sample in &mut per_operation {
        let start = Instant::now();
        let mut result = operation();
        for _ in 1..repetitions {
            result = operation();
        }
        *sample = start.elapsed() / repetitions;
        check(&result);
    }
    per_operation.sort_unstable();
    (per_operation[BATCHES / 2], per_operation[BATCHES - 1])
}
