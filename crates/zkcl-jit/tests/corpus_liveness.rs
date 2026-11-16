//! Compiles the Lisp sources the runtime ships with and reports what the block-summary
//! path in `zkcl-opt`'s liveness solver does on real bytecode.
//!
//! `research/shortcut-memoization.md` gates extending summary lifetimes on this
//! measurement: a summary cache that no real revisit reaches is a cost with no benefit.
//! Synthetic functions cannot answer it — they are written to reach the path. This test
//! drives the real corpus through the same Cranelift backend production uses and prints
//! the process-wide counters.
//!
//! What a run may conclude: whether the summary branch fires on this corpus, and in
//! which block-width buckets. What it may not conclude: that a cache would pay off.
//! `applied` counts revisits that already found a summary; `built_by_size` counts the
//! ones that had to compose one. Both being zero leaves the question open, it does not
//! refute the design.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zkcl_jit::CraneliftJitBackend;
use zkcl_opt::lift::{liveness_summaries_total, reset_liveness_summaries_total};
use zkcl_runtime::Runtime;

/// The Lisp sources shipped with the runtime, relative to this crate's manifest.
const CORPUS: &str = "../zkcl-runtime/lisp";

/// How long a single submission may take before the walk stops waiting for it.
/// A refused submission never completes, and the queue holds at most 64 requests,
/// so one wait per definition keeps the walk from stalling on a full queue.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(3);

#[test]
#[ignore = "walks the real corpus; needs --features counters and takes seconds"]
fn corpus_compilation_reports_which_liveness_shortcut_real_bytecode_reaches() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    let files = corpus_files(&root);
    assert!(!files.is_empty(), "no Lisp source under {}", root.display());

    let runtime = Runtime::builder().jit_threshold(u32::MAX).start().unwrap();
    runtime.set_jit_backend(Arc::new(CraneliftJitBackend::new().unwrap()));

    // Load every file before compiling any definition: a later file may add the package
    // an earlier one mentions, or redefine a function it defines. Compiling only what
    // the corpus finally defines keeps the denominator honest.
    let mut definitions = Vec::new();
    let mut loaded = 0;
    let mut unloaded = Vec::new();
    for path in &files {
        let Ok(source) = std::fs::read_to_string(path) else {
            unloaded.push(path.clone());
            continue;
        };
        let scanned = scan_definitions(&source);
        // A file that never says `in-package` defines into whatever package is current,
        // which is why this is read before the load rather than after it.
        let reading_into = current_package(&runtime);
        if runtime.load_str(&source).is_err() {
            unloaded.push(path.clone());
            continue;
        }
        loaded += 1;
        definitions.extend(scanned.into_iter().map(|(package, name)| {
            match package.or_else(|| reading_into.clone()) {
                Some(package) => format!("{package}::{name}"),
                None => name,
            }
        }));
    }
    let found = definitions.len();
    definitions.sort();
    definitions.dedup();
    let unique = definitions.len();

    let compile = runtime.eval("#'compile").unwrap().pop().unwrap();
    reset_liveness_summaries_total();

    let mut latencies = Vec::new();
    let mut unresolved = 0;
    let mut refused = 0;
    for symbol in &definitions {
        let Ok(values) = runtime.eval(&format!("#'{symbol}")) else {
            unresolved += 1;
            continue;
        };
        let Some(function) = values.first() else {
            unresolved += 1;
            continue;
        };
        let started = Instant::now();
        if runtime.call(&compile, &[&runtime.nil(), function]).is_err() {
            unresolved += 1;
            continue;
        }
        if !wait_for_completion(&runtime) {
            refused += 1;
            continue;
        }
        latencies.push(started.elapsed());
    }

    let totals = liveness_summaries_total();
    let compiled = latencies.len();
    for path in &unloaded {
        println!("corpus file not loaded: {}", path.display());
    }
    println!(
        "corpus: {} files, {loaded} loaded, {found} defun forms, {unique} unique, {compiled} \
         compiled, {unresolved} unresolved, {refused} not completed",
        files.len(),
    );
    if let Some((min, median, max, total)) = latency_spread(&latencies) {
        println!(
            "corpus compile (submit to installed code): n={compiled} min {min:.1} µs, \
             median {median:.1} µs, max {max:.1} µs, total {total:.1} ms"
        );
    }
    println!(
        "liveness: blocks {} succ_edges {} back_edges {} transfers {} applied {}",
        totals.blocks, totals.succ_edges, totals.back_edges, totals.transfers, totals.applied
    );
    println!("liveness built_by_size {:?}", totals.built_by_size);
    println!("liveness direct_by_size {:?}", totals.direct_by_size);
    // Runtime's exit counters ride along so one run answers both the liveness question
    // and "which exits did this corpus take". They are all zero unless the build has the
    // `vm-metrics` feature: add `zkcl-runtime/vm-metrics` to the feature list to make
    // them real, and read a zero here as "not measured", never as "not taken".
    println!("vm metrics: {:#?}", zkcl_runtime::metrics::snapshot());

    assert!(
        compiled > 0,
        "the corpus walk compiled nothing, so the counters have no denominator"
    );
    assert!(
        totals.blocks > 0,
        "no function reached the liveness solver although {compiled} compiled"
    );
}

/// Waits for one completion, then drains whatever finished alongside it.
///
/// Returns false when nothing arrived within [`COMPLETION_TIMEOUT`]: a submission the
/// queue refused returns immediately from `compile` but never completes.
fn wait_for_completion(runtime: &Runtime) -> bool {
    let deadline = Instant::now() + COMPLETION_TIMEOUT;
    loop {
        if runtime.poll_jit_completions() > 0 {
            while runtime.poll_jit_completions() > 0 {}
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// `min`, `median`, `max` in microseconds and the sum in milliseconds.
fn latency_spread(latencies: &[Duration]) -> Option<(f64, f64, f64, f64)> {
    let mut sorted = latencies.to_vec();
    sorted.sort();
    let micros = |at: usize| sorted[at].as_secs_f64() * 1e6;
    let total_ms = sorted.iter().map(Duration::as_secs_f64).sum::<f64>() * 1e3;
    let last = sorted.len().checked_sub(1)?;
    Some((micros(0), micros(last / 2), micros(last), total_ms))
}

/// Every `.lisp` file under `root`, in a stable order.
fn corpus_files(root: &Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "lisp")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// The package `load` is reading into, or `None` when `*package*` cannot be read.
fn current_package(runtime: &Runtime) -> Option<String> {
    let value = runtime.eval("(package-name *package*)").ok()?.pop()?;
    value.as_string(runtime)
}

/// A token reduced to what the corpus walk needs: where forms start and their head.
enum Token {
    Open,
    Close,
    Atom(String),
}

/// Splits `source` into tokens. String literals, `;` line comments, nestable `#| … |#`
/// block comments and `#\x` character literals are skipped, so their contents never
/// look like a form. This is a scanner, not a reader: it has to recognise `defun` and
/// `in-package` in a file it will never fully parse.
fn tokenize(source: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut atom = String::new();
    let mut chars = source.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            ';' => {
                flush_atom(&mut atom, &mut tokens);
                for comment in chars.by_ref() {
                    if comment == '\n' {
                        break;
                    }
                }
            }
            '#' if chars.peek() == Some(&'|') => {
                chars.next();
                flush_atom(&mut atom, &mut tokens);
                let mut depth = 1;
                while depth > 0 {
                    match chars.next() {
                        Some('|') if chars.peek() == Some(&'#') => {
                            chars.next();
                            depth -= 1;
                        }
                        Some('#') if chars.peek() == Some(&'|') => {
                            chars.next();
                            depth += 1;
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
            }
            '#' if chars.peek() == Some(&'\\') => {
                chars.next();
                flush_atom(&mut atom, &mut tokens);
                // `#\(` and `#\;` name one character; `#\Space` names it in words.
                if chars.next().is_some_and(|first| first.is_alphanumeric()) {
                    while chars
                        .peek()
                        .is_some_and(|next| next.is_alphanumeric() || *next == '-')
                    {
                        chars.next();
                    }
                }
            }
            '"' => {
                flush_atom(&mut atom, &mut tokens);
                let mut escaped = false;
                for inside in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if inside == '\\' {
                        escaped = true;
                    } else if inside == '"' {
                        break;
                    }
                }
            }
            '(' => {
                flush_atom(&mut atom, &mut tokens);
                tokens.push(Token::Open);
            }
            ')' => {
                flush_atom(&mut atom, &mut tokens);
                tokens.push(Token::Close);
            }
            whitespace if whitespace.is_whitespace() => {
                flush_atom(&mut atom, &mut tokens);
            }
            other => atom.push(other),
        }
    }
    flush_atom(&mut atom, &mut tokens);
    tokens
}

fn flush_atom(atom: &mut String, tokens: &mut Vec<Token>) {
    if !atom.is_empty() {
        tokens.push(Token::Atom(std::mem::take(atom)));
    }
}

/// The `defun` names a file defines, each with the package in effect at that point.
///
/// `in-package` may appear more than once, so the package is tracked as the tokens are
/// walked. A form whose name is itself a list — `(defun (setf accessor) …)` — is
/// skipped: the walk needs a symbol it can hand to `compile`.
fn scan_definitions(source: &str) -> Vec<(Option<String>, String)> {
    let tokens = tokenize(source);
    let mut package = None;
    let mut definitions = Vec::new();
    for window in tokens.windows(3) {
        let [Token::Open, Token::Atom(head), Token::Atom(name)] = window else {
            continue;
        };
        if head.eq_ignore_ascii_case("in-package") {
            package = Some(package_name(name));
        } else if head.eq_ignore_ascii_case("defun") {
            definitions.push((package.clone(), name.clone()));
        }
    }
    definitions
}

/// `:asdf`, `#:asdf` and `'asdf` all name the package `ASDF`.
fn package_name(designator: &str) -> String {
    designator
        .trim_start_matches(['\'', '#', ':'])
        .to_ascii_uppercase()
}
