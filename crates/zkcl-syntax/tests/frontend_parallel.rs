//! Ignored probes for frontend parsing parallelism.
//!
//! Protocol (absolute times only, min/median/max; the operator records load):
//!
//! ```text
//! cargo test --release -p zkcl-syntax --test frontend_parallel \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The corpus is the repository's own Lisp sources, so the numbers are
//! reproducible without ANSI-TEST: `tests/lisp` plus `crates/zkcl-runtime/lisp`.

#![cfg(feature = "analysis")]

use std::fmt::Write;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use zkcl_syntax::lint::FormLintCache;
use zkcl_syntax::{LintConfig, SourceWorkspace};

/// Sorted `(relative name, text)` for every `.lisp` file of the two source trees.
fn corpus() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut paths = Vec::new();
    for directory in [root.join("tests/lisp"), root.join("crates/zkcl-runtime/lisp")] {
        collect_lisp(&directory, &mut paths);
    }
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(&root)
                .expect("corpus path is below the workspace root")
                .to_string_lossy()
                .into_owned();
            let text = fs::read_to_string(&path).expect("corpus file is UTF-8");
            (name, text)
        })
        .collect()
}

fn collect_lisp(directory: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read Lisp corpus directory") {
        let path = entry.expect("read Lisp corpus entry").path();
        if path.is_dir() {
            collect_lisp(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "lisp") {
            out.push(path);
        }
    }
}

fn report(label: &str, mut samples: Vec<Duration>) {
    samples.sort();
    println!(
        "{label}: min {:.3} ms / median {:.3} ms / max {:.3} ms",
        samples[0].as_secs_f64() * 1e3,
        samples[(samples.len() - 1) / 2].as_secs_f64() * 1e3,
        samples[samples.len() - 1].as_secs_f64() * 1e3,
    );
}

/// Whole-library preparation: syntax tree plus document index, four worker counts.
#[test]
#[ignore = "measurement probe; run explicitly with --ignored --nocapture"]
fn whole_library_preparse_by_jobs() {
    let corpus = corpus();
    let bytes: usize = corpus.iter().map(|(_, text)| text.len()).sum();
    println!("corpus: {} files, {bytes} bytes", corpus.len());
    let tiers = [1, 2, 4, 8].map(|jobs| NonZeroUsize::new(jobs).unwrap());
    let mut samples = tiers.map(|_| Vec::with_capacity(5));
    for round in 0..5 {
        for offset in 0..tiers.len() {
            let tier = (round + offset) % tiers.len();
            let mut workspace = SourceWorkspace::new();
            for (name, text) in &corpus {
                workspace.set_document(name.as_str(), text.as_str());
            }
            let started = Instant::now();
            workspace.preparse_with_jobs(tiers[tier]);
            samples[tier].push(started.elapsed());
        }
    }
    for (jobs, samples) in tiers.into_iter().zip(samples) {
        report(&format!("preparse jobs={}", jobs.get()), samples);
    }
}

/// One document, one worker: the per-file floor of the table above.
#[test]
#[ignore = "measurement probe; run explicitly with --ignored --nocapture"]
fn single_file_preparse() {
    let corpus = corpus();
    let (name, text) = corpus
        .iter()
        .max_by_key(|(_, text)| text.len())
        .expect("corpus is not empty");
    let mut samples = Vec::new();
    for _ in 0..9 {
        let mut workspace = SourceWorkspace::new();
        workspace.set_document(name.as_str(), text.as_str());
        let started = Instant::now();
        workspace.preparse_with_jobs(NonZeroUsize::MIN);
        samples.push(started.elapsed());
    }
    report(&format!("single file {name} ({} bytes)", text.len()), samples);
}

/// LSP-equivalent in-process latency: capture a snapshot after one body edit and
/// run the full lint with a retained form cache, 21 times.
#[test]
#[ignore = "measurement probe; run explicitly with --ignored --nocapture"]
fn edit_then_full_diagnostics() {
    let mut document = String::new();
    for index in 0..1800 {
        writeln!(
            document,
            "(defun probe{index:04} (x)\n  (let ((y (+ x 1))) (* y y)))"
        )
        .unwrap();
    }
    let bytes = document.len();
    let edit = document.match_indices("(+ x 1)").nth(900).unwrap().0 + "(+ x ".len();
    let mut workspace = SourceWorkspace::new();
    workspace.set_document("large", document);
    let config = LintConfig::default();
    let mut form_cache = FormLintCache::default();
    let lint = |workspace: &SourceWorkspace, form_cache: &mut FormLintCache| {
        workspace
            .diagnostic_snapshot("large")
            .unwrap()
            .lint_with_cache(&config, || false, Some(form_cache))
            .unwrap()
    };
    let original = lint(&workspace, &mut form_cache);
    let mut samples = Vec::new();
    for round in 0..21 {
        let digit = if round % 2 == 0 { "2" } else { "1" };
        workspace.apply_edit("large", edit..edit + 1, digit).unwrap();
        let started = Instant::now();
        let diagnostics = lint(&workspace, &mut form_cache);
        samples.push(started.elapsed());
        assert_eq!(diagnostics, original);
    }
    report(&format!("edit then full diagnostics ({bytes} bytes)"), samples);
}
