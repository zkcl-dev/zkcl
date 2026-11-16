use std::time::{Duration, Instant};

use zkcl::{EvaluationPoll, Runtime, SimulatedHostServices};

fn eval_true(runtime: &Runtime, form: &str) {
    let values = runtime.eval(form).unwrap_or_else(|error| {
        panic!("expected form to succeed:\n{form}\nerror: {error:?}");
    });
    assert!(
        !values.is_empty() && values[0].is_t(),
        "expected T for form:\n{form}"
    );
}

#[test]
fn simulated_host_services_drives_async_timeout_and_entropy_deterministically() {
    let sim = SimulatedHostServices::new(2026);
    let mut runtime = Runtime::start().unwrap();
    runtime.set_host_services(sim.clone());

    // Warm up zk-async before measuring execution latency.
    runtime.eval("(zk-async:current-task)").unwrap();

    let started = Instant::now();
    let mut eval = runtime
        .start_eval(
            "(eq (zk-async:with-timeout (3 :on-timeout :timed-out) (sleep 10) :done)
                 :timed-out)",
        )
        .unwrap();

    let ready = loop {
        match eval.resume(None).unwrap() {
            EvaluationPoll::Runnable => {}
            EvaluationPoll::Waiting(Some(delay)) => {
                sim.advance(delay);
            }
            EvaluationPoll::Waiting(None) => {
                panic!("unexpected indefinite wait without timer")
            }
            EvaluationPoll::Paused(_) => panic!("unexpected debugger pause"),
            EvaluationPoll::Ready(values) => break values,
        }
    };
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(50),
        "virtual clock timeout took too long: {elapsed:?}"
    );
    assert_eq!(sim.now_nanos(), 3_000_000_000);
    assert_eq!(ready.len(), 1);
    assert!(ready[0].is_t());
    drop(ready);
    drop(eval);

    // Identical SimulatedHostServices seeds yield identical Lisp random streams.
    let mut rt_a = Runtime::start().unwrap();
    rt_a.set_host_services(SimulatedHostServices::new(77));
    let mut rt_b = Runtime::start().unwrap();
    rt_b.set_host_services(SimulatedHostServices::new(77));

    let fmt_expr = "(let ((*random-state* (make-random-state t))) (format nil \"~S\" (list (random 1000000) (random 1000000) (get-universal-time))))";
    let s_a = rt_a.eval(fmt_expr).unwrap();
    let s_b = rt_b.eval(fmt_expr).unwrap();
    assert_eq!(s_a[0].as_string(&rt_a), s_b[0].as_string(&rt_b));
}

#[test]
fn pct_schedules_are_deterministic_and_bounded() {
    let runtime = Runtime::start().unwrap();
    runtime.eval("(require :zk-pbt)").unwrap();
    eval_true(
        &runtime,
        "(let ((s1 (zk-pbt:sample (zk-pbt:pct-schedules 3 :steps 12 :bug-depth 2) :count 4 :seed 99))
               (s2 (zk-pbt:sample (zk-pbt:pct-schedules 3 :steps 12 :bug-depth 2) :count 4 :seed 99)))
           (and (equal s1 s2)
                (= (length s1) 4)
                (every (lambda (sched)
                         (and (= (length sched) 12)
                              (every (lambda (tid) (and (integerp tid) (<= 0 tid 2))) sched)))
                       s1)))",
    );
}

#[test]
fn pct_schedules_exposes_and_shrinks_depth2_race_bug() {
    let runtime = Runtime::start().unwrap();
    runtime.eval("(require :zk-pbt)").unwrap();
    eval_true(
        &runtime,
        "(progn
           (zk-pbt:defproperty pct-lost-update ((sched (zk-pbt:pct-schedules 2 :steps 8 :bug-depth 2)))
             (let ((balance 0)
                   (a 0)
                   (b 0))
               (zk-pbt:run-interleaved
                (list (zk-pbt:interleaved-steps
                        (setq a balance)
                        (setq balance (+ a 10)))
                      (zk-pbt:interleaved-steps
                        (setq b balance)
                        (setq balance (+ b 5))))
                sched)
               (zk-pbt:claim :lost-update (= balance 15))))
           (let* ((report (zk-pbt:run 'pct-lost-update :seed 123 :trials 50))
                  (data (zk-pbt:report-data report))
                  (reduced (first (getf data :reduced))))
             (and (eq (zk-pbt:report-status report) :falsified)
                  (listp reduced)
                  (<= (length reduced) 4))))",
    );
}

#[test]
fn pct_schedules_integrates_with_run_parallel_commands() {
    let runtime = Runtime::start().unwrap();
    runtime.eval("(require :zk-pbt)").unwrap();
    eval_true(
        &runtime,
        "(progn
           ;; Buggy read-modify-write register: INC reads and overwrites a stale value
           ;; when interleaved between another thread's read and write.
           (defstruct buggy-reg (val 0) (pending 0))
           (zk-pbt:defmachine pct-buggy-reg
             (:model () 0)
             (:open () (make-buggy-reg))
             (:command :read-step ()
               (:step (m) (values m (list m)))
               (:act (s) (setf (buggy-reg-pending s) (buggy-reg-val s))))
             (:command :write-inc ()
               (:step (m) (values (1+ m) (list (1+ m))))
               (:act (s) (setf (buggy-reg-val s) (1+ (buggy-reg-pending s)))))
             (:command :get ()
               (:step (m) (values m (list m)))
               (:act (s) (buggy-reg-val s))))
           (let ((found-non-linearizable nil))
             (dolist (sched (zk-pbt:sample (zk-pbt:pct-schedules 2 :steps 6 :bug-depth 2) :count 20 :seed 7))
               (handler-case
                   (zk-pbt:run-parallel-commands
                    'pct-buggy-reg
                    '(nil ((:read-step) (:write-inc)) ((:read-step) (:write-inc) (:get)))
                    :interleave-schedule sched)
                 (zk-pbt::property-violation ()
                   (setq found-non-linearizable t))))
             found-non-linearizable))",
    );
}

#[test]
fn parallel_pbt_samples_match_single_threaded_results_and_min_failing_index() {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MASTER_SEED: u64 = 2026;
    const SAMPLES: usize = 16;

    let run_sample = |sample_index: usize| -> (String, bool) {
        let sample_seed = SimulatedHostServices::split_seed(MASTER_SEED, sample_index as u64);
        let sim = SimulatedHostServices::new(sample_seed);
        let mut runtime = Runtime::start().unwrap();
        runtime.set_host_services(sim);
        runtime.eval("(require :zk-pbt)").unwrap();
        let expr = format!(
            "(let ((*random-state* (make-random-state t)))
               (let* ((draw (random 1000))
                      (xs (zk-pbt:sample (zk-pbt:integers :min 0 :max 100) :count 4 :seed {sample_seed}))
                      (payload (format nil \"~D:~D:~S\" draw (get-universal-time) xs))
                      (ok (not (zerop (mod draw 3)))))
                 (values payload ok)))"
        );
        let values = runtime.eval(&expr).unwrap();
        let payload = values[0].as_string(&runtime).unwrap();
        let ok = !values[1].is_nil();
        (payload, ok)
    };

    let seq_results: Vec<(String, bool)> = (0..SAMPLES).map(run_sample).collect();
    let seq_min_fail = seq_results.iter().position(|(_, ok)| !ok);
    assert!(
        seq_min_fail.is_some(),
        "expected at least one deterministic failing sample across {SAMPLES} draws"
    );

    let next_index = AtomicUsize::new(0);
    let par_slots: Mutex<Vec<Option<(String, bool)>>> = Mutex::new(vec![None; SAMPLES]);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= SAMPLES {
                        break;
                    }
                    let outcome = run_sample(index);
                    par_slots.lock().unwrap()[index] = Some(outcome);
                }
            });
        }
    });

    let par_results: Vec<(String, bool)> = par_slots
        .into_inner()
        .unwrap()
        .into_iter()
        .map(Option::unwrap)
        .collect();
    let par_min_fail = par_results.iter().position(|(_, ok)| !ok);

    assert_eq!(seq_results, par_results);
    assert_eq!(seq_min_fail, par_min_fail);
}
