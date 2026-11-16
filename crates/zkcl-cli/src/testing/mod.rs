use std::{collections::VecDeque, error::Error, process::ExitCode, sync::Mutex, time::Instant};

use zkcl::Runtime;

mod documentation;

use super::args::TestFormat as OutputFormat;

fn push_lisp_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' | '"' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out.push('"');
}

pub(super) fn command(request: super::args::TestArgs, heap_size: Option<usize>) -> ExitCode {
    let system = request.system.expect("ASDF test selected after parsing");
    let doc = request.doc;
    let jobs = request.jobs.map_or(1, std::num::NonZeroUsize::get);
    let format = request.format.unwrap_or(OutputFormat::Text);
    let mut run_options_source = String::new();
    for (name, value) in [
        ("seed", request.seed),
        ("trials", request.trials),
        ("max-size", request.max_size),
        ("max-discards", request.max_discards),
        ("max-discard-ratio", request.max_discard_ratio),
        ("max-shrinks", request.max_shrinks),
        ("max-enumerated", request.max_enumerated),
    ] {
        if let Some(value) = value {
            run_options_source.push_str(&format!(" :{name} {value}"));
        }
    }
    for (name, value) in [("filter", request.filter), ("corpus", request.corpus)] {
        if let Some(value) = value {
            run_options_source.push_str(&format!(" :{name} "));
            push_lisp_string(&mut run_options_source, &value);
        }
    }
    let result = if doc {
        documentation::test_system(&system, heap_size)
    } else if jobs > 1 {
        test_system_parallel(&system, &run_options_source, format, jobs, heap_size)
    } else {
        test_system_serial(&system, &run_options_source, format, heap_size)
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            super::report_error("zkcl test: ", e.as_ref());
            ExitCode::FAILURE
        }
    }
}

fn asdf_runtime(heap_size: Option<usize>) -> Result<Runtime, Box<dyn Error>> {
    let runtime = super::standalone_runtime(heap_size)?;
    runtime.eval("(require :asdf)")?;
    runtime.eval("(pushnew (uiop:getcwd) asdf:*central-registry* :test #'equal)")?;
    Ok(runtime)
}

fn test_system_serial(
    system: &str,
    run_options_source: &str,
    format: OutputFormat,
    heap_size: Option<usize>,
) -> Result<ExitCode, Box<dyn Error>> {
    let runtime = asdf_runtime(heap_size)?;
    runtime.eval("(require :zk-test)")?;
    if format == OutputFormat::Text {
        runtime.eval(&format!(
            "(setq zk-test:*run-options* '({run_options_source}) zk-test:*report-hook* #'zk-test:print-report)"
        ))?;
        let asdf_test_system = runtime.eval("#'asdf:test-system")?;
        let system_name = runtime.make_string(system)?;
        runtime.call(&asdf_test_system[0], &[&system_name])?;
        println!("ASDF test-op completed for {system}");
        return Ok(ExitCode::SUCCESS);
    }

    runtime.eval(&format!(
        "(setq zk-test:*run-options* '({run_options_source}))"
    ))?;
    let runner = runtime.eval(
        r#"#'(lambda (sys fmt)
             (let ((reports nil)
                   (failed nil))
               (let ((zk-test:*report-hook* (lambda (r) (push r reports))))
                 (handler-case (asdf:test-system sys)
                   (zk-test:check-failed () (setq failed t))))
               (setq reports (nreverse reports))
               (values
                (with-output-to-string (s)
                  (cond
                    ((string= fmt "json")
                     (if (= 1 (length reports))
                         (zk-test:write-json-report (first reports) s)
                         (progn
                           (write-char #\[ s)
                           (loop for r in reports for first = t then nil do
                             (unless first (write-string ", " s))
                             (zk-test:write-json-report r s))
                           (write-char #\] s))))
                    (t
                     (if (= 1 (length reports))
                         (zk-test:write-junit-report (first reports) s)
                         (progn
                           (format s "<?xml version=\"1.0\" encoding=\"UTF-8\"?>~%<testsuites>~%")
                           (dolist (r reports) (zk-test:write-junit-report r s))
                           (format s "</testsuites>~%"))))))
                (if failed 1 0))))"#,
    )?;
    let system_name = runtime.make_string(system)?;
    let fmt_name = runtime.make_string(if format == OutputFormat::Json {
        "json"
    } else {
        "junit"
    })?;
    let vals = runtime.call(&runner[0], &[&system_name, &fmt_name])?;
    let text = runtime
        .string(&vals[0])
        .ok_or("expected formatted report string")?;
    let failed = vals[1].as_fixnum().unwrap_or(1) != 0;
    println!("{text}");
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

struct DiscoveredCheck {
    name: String,
    is_suite: bool,
    leaf_range: std::ops::Range<usize>,
}

struct TestCase {
    index: usize,
    check_index: usize,
    package_name: String,
    symbol_name: String,
    options_source: String,
}

#[derive(Clone)]
struct TestCaseReport {
    severity: i64,
    name: String,
    elapsed_ms: u64,
    text: String,
    json: String,
    junit: String,
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 32 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn severity_to_status(severity: i64) -> &'static str {
    match severity {
        0 => "passed",
        1 => "insufficient",
        2 => "falsified",
        3 => "unstable",
        _ => "error",
    }
}

fn test_system_parallel(
    system: &str,
    run_options_source: &str,
    format: OutputFormat,
    jobs: usize,
    heap_size: Option<usize>,
) -> Result<ExitCode, Box<dyn Error>> {
    let coordinator = asdf_runtime(heap_size)?;
    coordinator.eval("(require :zk-test)")?;
    coordinator.eval(&format!(
        "(setq zk-test:*discover-only* t zk-test:*discovered-checks* nil zk-test:*run-options* '({run_options_source}))"
    ))?;
    let discover_fn = coordinator.eval(
        r#"#'(lambda (sys text-output-p)
             (asdf:test-system sys)
             (let ((checks (nreverse zk-test:*discovered-checks*))
                   (out nil))
               (dolist (entry checks (nreverse out))
                 (let* ((root (first entry))
                        (opts (second entry))
                        (filter (zk-test::split-keyword-option
                                  (append opts zk-test:*run-options*) :filter))
                        (leaves (zk-test:list-tests root filter))
                        (suitep (eq :suite (zk-test::entry-kind (gethash root zk-test::*test-registry*))))
                        (clean-opts (nth-value 1 (zk-test::split-keyword-option opts :filter)))
                        (opts-src (let ((*print-readably* nil)) (prin1-to-string clean-opts)))
                        (root-name (if text-output-p
                                       (prin1-to-string root)
                                       (zk-test::qualified-symbol-string root)))
                        (leaf-pairs (mapcar (lambda (sym)
                                              (list (package-name (symbol-package sym))
                                                    (symbol-name sym)))
                                            leaves)))
                   (push (list root-name suitep opts-src leaf-pairs) out)))))"#,
    )?;
    let system_name = coordinator.make_string(system)?;
    let discovered_vals = {
        let text_output = if format == OutputFormat::Text {
            coordinator.t()
        } else {
            coordinator.nil()
        };
        coordinator.call(&discover_fn[0], &[&system_name, &text_output])?
    };

    let mut suites = Vec::new();
    let mut tasks = Vec::new();
    let mut cur = discovered_vals[0].clone();
    while let Some((item, tail)) = coordinator.cons_fields(&cur) {
        let check_index = suites.len();
        let (root, remaining) = coordinator
            .cons_fields(&item)
            .ok_or("malformed discovered check")?;
        let (suite, remaining) = coordinator
            .cons_fields(&remaining)
            .ok_or("malformed discovered check")?;
        let (options, remaining) = coordinator
            .cons_fields(&remaining)
            .ok_or("malformed discovered check")?;
        let (mut leaves, _) = coordinator
            .cons_fields(&remaining)
            .ok_or("malformed discovered check")?;

        let name = coordinator.string(&root).unwrap_or_default();
        let is_suite = !suite.is_nil();
        let options_source = coordinator.string(&options).unwrap_or_else(|| "NIL".into());
        let start = tasks.len();

        while let Some((pair, rest)) = coordinator.cons_fields(&leaves) {
            let (package, name) = coordinator.cons_fields(&pair).ok_or("malformed leaf")?;
            let (symbol, _) = coordinator.cons_fields(&name).ok_or("malformed leaf")?;
            tasks.push(TestCase {
                index: tasks.len(),
                check_index,
                package_name: coordinator.string(&package).unwrap_or_default(),
                symbol_name: coordinator.string(&symbol).unwrap_or_default(),
                options_source: options_source.clone(),
            });
            leaves = rest;
        }
        suites.push(DiscoveredCheck {
            name,
            is_suite,
            leaf_range: start..tasks.len(),
        });
        cur = tail;
    }
    drop(discovered_vals);
    drop(discover_fn);
    drop(system_name);
    drop(coordinator);

    if suites.is_empty() {
        if format == OutputFormat::Text {
            println!("ASDF test-op completed for {system}");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if tasks.len() <= 1 {
        return test_system_serial(system, run_options_source, format, heap_size);
    }

    let total_tasks = tasks.len();
    let worker_count = jobs.min(total_tasks);
    let queue = Mutex::new(VecDeque::from(tasks));
    let outcomes: Mutex<Vec<Option<TestCaseReport>>> = Mutex::new(vec![None; total_tasks]);
    let worker_error: Mutex<Option<String>> = Mutex::new(None);
    let wall_start = Instant::now();

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            scope.spawn(|| {
                let run_worker = || -> Result<(), Box<dyn Error>> {
                    let rt = asdf_runtime(heap_size)?;
                    rt.eval("(require :zk-test)")?;
                    rt.eval(&format!(
                        "(setq zk-test:*discover-only* t zk-test:*discovered-checks* nil zk-test:*run-options* '({run_options_source}))"
                    ))?;
                    let test_system_fn = rt.eval("#'asdf:test-system")?;
                    let sys_str = rt.make_string(system)?;
                    rt.call(&test_system_fn[0], &[&sys_str])?;
                    rt.eval("(setq zk-test:*discover-only* nil)")?;
                    let exec_leaf = rt.eval(
                        r#"#'(lambda (pkg-name sym-name opts-src suite-name)
                             (let* ((pkg (or (find-package pkg-name)
                                             (error "Missing package ~A" pkg-name)))
                                    (sym (or (find-symbol sym-name pkg)
                                             (error "Missing test ~A:~A" pkg-name sym-name)))
                                    (opts (if (string= opts-src "NIL")
                                              nil
                                              (read-from-string opts-src)))
                                    (report (apply #'zk-test:run sym opts))
                                    (status (zk-test:report-status report))
                                    (sev (zk-test::status-severity status))
                                    (ms (or (getf (zk-test:report-data report) :elapsed-ms) 0))
                                    (qname (zk-test::qualified-symbol-string sym))
                                    (txt (with-output-to-string (*standard-output*)
                                           (zk-test:print-report report)))
                                    (js (with-output-to-string (s)
                                          (zk-test:write-json-report report s)))
                                    (xml (with-output-to-string (s)
                                           (zk-test::write-junit-testcase report s suite-name))))
                               (values sev qname ms txt js xml)))"#,
                    )?;

                    loop {
                        let task = {
                            let mut q = queue.lock().unwrap();
                            q.pop_front()
                        };
                        let Some(task) = task else {
                            break;
                        };
                        let suite_name = &suites[task.check_index].name;
                        let pkg_w = rt.make_string(&task.package_name)?;
                        let sym_w = rt.make_string(&task.symbol_name)?;
                        let opts_w = rt.make_string(&task.options_source)?;
                        let suite_w = rt.make_string(suite_name)?;
                        let vals =
                            rt.call(&exec_leaf[0], &[&pkg_w, &sym_w, &opts_w, &suite_w])?;
                        let [severity, name, elapsed, text, json, junit] = vals.as_slice() else {
                            return Err("malformed test case report".into());
                        };
                        let outcome = TestCaseReport {
                            severity: severity.as_fixnum().unwrap_or(4) as i64,
                            name: rt.string(name).unwrap_or(task.symbol_name),
                            elapsed_ms: elapsed.as_fixnum().unwrap_or(0).max(0) as u64,
                            text: rt.string(text).unwrap_or_default(),
                            json: rt.string(json).unwrap_or_default(),
                            junit: rt.string(junit).unwrap_or_default(),
                        };
                        let mut out = outcomes.lock().unwrap();
                        out[task.index] = Some(outcome);
                    }
                    Ok(())
                };
                if let Err(err) = run_worker() {
                    let mut slot = worker_error.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(err.to_string());
                    }
                }
            });
        }
    });

    if let Some(err) = worker_error.into_inner().unwrap() {
        return Err(err.into());
    }
    let wall_ms = wall_start.elapsed().as_millis() as u64;
    let outcomes: Vec<TestCaseReport> = outcomes
        .into_inner()
        .unwrap()
        .into_iter()
        .enumerate()
        .map(|(i, o)| o.ok_or_else(|| format!("missing parallel result for task {i}")))
        .collect::<Result<Vec<_>, _>>()?;

    let mut overall_passed = true;
    match format {
        OutputFormat::Text => {
            for suite in &suites {
                let slice = &outcomes[suite.leaf_range.clone()];
                if suite.is_suite {
                    let worst_sev = slice.iter().map(|o| o.severity).max().unwrap_or(1);
                    if worst_sev != 0 {
                        overall_passed = false;
                    }
                    let status_upper = severity_to_status(worst_sev).to_uppercase();
                    let passed = slice.iter().filter(|o| o.severity == 0).count();
                    println!("{status_upper} {}", suite.name);
                    println!(
                        "  summary: {passed}/{} passed (jobs={worker_count}, {wall_ms}ms)",
                        slice.len()
                    );
                    for leaf in slice {
                        print!("{}", leaf.text);
                    }
                    let failed: Vec<&str> = slice
                        .iter()
                        .filter(|o| o.severity != 0)
                        .map(|o| o.name.as_str())
                        .collect();
                    if !failed.is_empty() {
                        println!("  failed: {}", failed.join(", "));
                    }
                } else {
                    for leaf in slice {
                        if leaf.severity != 0 {
                            overall_passed = false;
                        }
                        print!("{}", leaf.text);
                    }
                }
            }
            if overall_passed {
                println!("ASDF test-op completed for {system}");
            }
        }
        OutputFormat::Json => {
            let mut suite_jsons = Vec::with_capacity(suites.len());
            for suite in &suites {
                let slice = &outcomes[suite.leaf_range.clone()];
                let worst_sev = slice.iter().map(|o| o.severity).max().unwrap_or(1);
                if worst_sev != 0 {
                    overall_passed = false;
                }
                if suite.is_suite {
                    let status = severity_to_status(worst_sev);
                    let results_joined: String = slice
                        .iter()
                        .map(|o| o.json.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    suite_jsons.push(format!(
                        "{{\"status\": {}, \"name\": {}, \"elapsed_ms\": {wall_ms}, \"jobs\": {worker_count}, \"results\": [{results_joined}]}}",
                        json_escape(status),
                        json_escape(&suite.name)
                    ));
                } else if let Some(leaf) = slice.first() {
                    suite_jsons.push(leaf.json.clone());
                }
            }
            if suite_jsons.len() == 1 {
                println!("{}", suite_jsons[0]);
            } else {
                println!("[{}]", suite_jsons.join(", "));
            }
        }
        OutputFormat::Junit => {
            let multi = suites.len() > 1;
            println!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
            if multi {
                println!("<testsuites>");
            }
            for suite in &suites {
                let slice = &outcomes[suite.leaf_range.clone()];
                let worst_sev = slice.iter().map(|o| o.severity).max().unwrap_or(1);
                if worst_sev != 0 {
                    overall_passed = false;
                }
                let total = slice.len();
                let failures = slice
                    .iter()
                    .filter(|o| o.severity == 2 || o.severity == 3)
                    .count();
                let errors = slice.iter().filter(|o| o.severity >= 4).count();
                let skipped = slice.iter().filter(|o| o.severity == 1).count();
                let total_ms: u64 = slice.iter().map(|o| o.elapsed_ms).sum();
                println!(
                    "<testsuite name=\"{}\" tests=\"{total}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\" time=\"{:.3}\">",
                    xml_escape(&suite.name),
                    (total_ms as f64) / 1000.0
                );
                for leaf in slice {
                    print!("{}", leaf.junit);
                }
                println!("</testsuite>");
            }
            if multi {
                println!("</testsuites>");
            }
        }
    }

    Ok(if overall_passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

pub(super) fn test_project_documentation(
    runtime: &Runtime,
    name: &str,
    root: &std::path::Path,
    files: &[std::path::PathBuf],
) -> Result<ExitCode, Box<dyn Error>> {
    documentation::test_project(runtime, name, root, files)
}
