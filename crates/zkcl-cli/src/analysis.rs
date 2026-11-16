use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use zkcl::CompilationTarget;
use zkcl_diagnostics::{
    Applicability, Diagnostic, LabelRole, Severity, SourceDocument, Suggestion,
};
use zkcl_syntax::documentation::DocumentationTests;
use zkcl_syntax::syntax::SourceReadtable;
use zkcl_syntax::{
    LintConfig, LintLevel, LintPreset, SourceSnapshot, SourceWorkspace, all_rules, find_rule,
};

const INPUT_DOCUMENT: &str = "input";

use super::args::DiagnosticFormat as OutputFormat;

#[derive(Clone, Copy)]
enum AnalysisCommand {
    Check,
    Lint,
}

impl AnalysisCommand {
    const fn default_preset(self) -> LintPreset {
        match self {
            Self::Check => LintPreset::Default,
            Self::Lint => LintPreset::Strict,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FixMode {
    Apply,
    Preview,
}

struct LintCliOptions {
    target: Option<CompilationTarget>,
    jobs: NonZeroUsize,
    readtable: SourceReadtable,
    config_path: Option<PathBuf>,
    preset: Option<LintPreset>,
    overrides: Vec<(String, LintLevel)>,
    deny_warnings: bool,
    fix_mode: Option<FixMode>,
    unsafe_fixes: bool,
    format: OutputFormat,
    list_rules: bool,
    explain: Option<String>,
    paths: Vec<OsString>,
}

struct AnalyzedDocument<'a> {
    path: &'a OsStr,
    key: String,
    label: String,
    snapshot: Arc<SourceSnapshot>,
    markdown: bool,
}

pub(super) fn check(request: super::args::AnalysisArgs) -> ExitCode {
    run_analysis(request, AnalysisCommand::Check)
}

pub(super) fn lint(request: super::args::AnalysisArgs) -> ExitCode {
    run_analysis(request, AnalysisCommand::Lint)
}

pub(super) fn explain(request: super::args::ExplainArgs) -> ExitCode {
    if let Some(rule) = request.rule {
        print_rule_explanation(&rule)
    } else {
        print_rule_list();
        ExitCode::SUCCESS
    }
}

impl From<super::args::AnalysisArgs> for LintCliOptions {
    fn from(request: super::args::AnalysisArgs) -> Self {
        Self {
            target: request
                .target
                .map(|target| target.parse().expect("clap validated compilation target")),
            jobs: request.jobs.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .unwrap_or(NonZeroUsize::MIN)
                    .min(NonZeroUsize::new(8).unwrap())
            }),
            readtable: SourceReadtable::named(&request.readtable)
                .expect("clap validated readtable"),
            config_path: request.config,
            preset: request.preset.as_deref().and_then(LintPreset::parse),
            overrides: request
                .overrides
                .into_iter()
                .filter_map(|(name, level)| {
                    let name = name.trim().to_owned();
                    (!name.is_empty()).then_some((
                        name,
                        match level {
                            super::args::RuleLevel::Warn => LintLevel::Warn,
                            super::args::RuleLevel::Allow => LintLevel::Allow,
                            super::args::RuleLevel::Deny => LintLevel::Deny,
                        },
                    ))
                })
                .collect(),
            deny_warnings: request.deny_warnings,
            fix_mode: if request.fix {
                Some(FixMode::Apply)
            } else if request.fix_dry_run {
                Some(FixMode::Preview)
            } else {
                None
            },
            unsafe_fixes: request.unsafe_fixes,
            format: request.format,
            list_rules: request.list_rules,
            explain: request.explain,
            paths: request
                .paths
                .into_iter()
                .map(PathBuf::into_os_string)
                .collect(),
        }
    }
}

fn driver_diagnostic(message: impl Into<String>) -> Diagnostic {
    let mut diagnostic = Diagnostic::new(Severity::Error, message);
    diagnostic.code = Some("cli/input".into());
    diagnostic
}

fn finish_report(
    opts: &LintCliOptions,
    diagnostics: Vec<Diagnostic>,
    fixes: FixSummary,
    mut status: u8,
) -> ExitCode {
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Warning)
        .count();
    if opts.format == OutputFormat::Json {
        let mut payload = crate::diagnostics::batch_json(&diagnostics);
        payload["fixed_files"] = fixes.fixed_files.into();
        payload["summary"] = serde_json::json!({ "errors": errors, "warnings": warnings });
        if opts.fix_mode == Some(FixMode::Preview) {
            payload["fix_diff"] = fixes.diff.into();
        }
        println!("{payload}");
    } else {
        let mut options = zkcl_terminal::RenderOptions::stderr();
        options.short = opts.format == OutputFormat::Short;
        for diagnostic in &diagnostics {
            eprintln!("{}", zkcl_terminal::render(diagnostic, &options));
        }
        let plural = |count| if count == 1 { "" } else { "s" };
        if errors + warnings > 0 {
            eprintln!(
                "{errors} error{}, {warnings} warning{}",
                plural(errors),
                plural(warnings)
            );
        }
        if fixes.fixed_files > 0 {
            eprintln!(
                "Applied fixes in {} file{}",
                fixes.fixed_files,
                plural(fixes.fixed_files)
            );
        }
        if !fixes.diff.is_empty()
            && let Err(error) = io::stdout().lock().write_all(fixes.diff.as_bytes())
        {
            eprintln!(
                "{}",
                zkcl_terminal::render_stderr(&driver_diagnostic(format!(
                    "cannot write fix preview: {error}"
                )))
            );
            status = 2;
        }
    }
    ExitCode::from(status)
}
fn fail_analysis(opts: &LintCliOptions, message: impl Into<String>) -> ExitCode {
    finish_report(
        opts,
        vec![driver_diagnostic(message)],
        FixSummary::default(),
        2,
    )
}

pub(super) fn input_failure(request: super::args::AnalysisArgs, message: String) -> ExitCode {
    fail_analysis(&LintCliOptions::from(request), message)
}

fn run_analysis(mut request: super::args::AnalysisArgs, mode: AnalysisCommand) -> ExitCode {
    if let Err(error) = super::project::analysis_paths(
        &request.project,
        &mut request.paths,
        request.list_rules || request.explain.is_some(),
    ) {
        return finish_report(
            &LintCliOptions::from(request),
            vec![crate::diagnostics::from_error("", error.as_ref())],
            FixSummary::default(),
            2,
        );
    }
    let opts = LintCliOptions::from(request);
    if opts.fix_mode == Some(FixMode::Apply) && opts.paths.iter().any(|path| path == "-") {
        return fail_analysis(
            &opts,
            "--fix requires files; use --fix-dry-run to preview fixes for stdin",
        );
    }
    if opts.list_rules {
        if opts.format == OutputFormat::Json {
            println!(
                "{}",
                serde_json::json!({"rules": all_rules().iter().map(rule_json).collect::<Vec<_>>() })
            );
        } else {
            print_rule_list();
        }
        return ExitCode::SUCCESS;
    }
    if let Some(query) = &opts.explain {
        if opts.format == OutputFormat::Json {
            let Some(rule) = find_rule(query) else {
                return fail_analysis(
                    &opts,
                    format!(
                        "unknown lint rule `{query}`; run `zkcl explain --list` to see available rules"
                    ),
                );
            };
            println!("{}", serde_json::json!({"rule": rule_json(rule)}));
            return ExitCode::SUCCESS;
        }
        return print_rule_explanation(query);
    }

    let config = match resolve_lint_config(&opts, mode) {
        Ok(cfg) => cfg,
        Err(err) => {
            return fail_analysis(&opts, err.to_string());
        }
    };

    let (inputs, collect_err) = expand_input_paths(&opts.paths);
    if let Some(err) = collect_err {
        return fail_analysis(&opts, err);
    }
    if inputs.is_empty() {
        return fail_analysis(
            &opts,
            "no source files matched; supply a source path or select a project",
        );
    }

    let mut workspace = SourceWorkspace::new();
    workspace.set_target(opts.target.as_ref());
    workspace.set_readtable(opts.readtable);
    let mut markdown_workspace = SourceWorkspace::new();

    let (mut loaded_docs, mut status, mut diagnostics) =
        load_analysis_documents(&inputs, &mut workspace, &mut markdown_workspace);
    let lint_loaded = |workspace: &SourceWorkspace, documents: &[AnalyzedDocument<'_>]| {
        let keys = documents
            .iter()
            .filter(|doc| !doc.markdown)
            .map(|doc| doc.key.as_str());
        workspace.lint_documents_with_jobs(keys, &config, opts.jobs)
    };
    let mut lint_results = lint_loaded(&workspace, &loaded_docs);
    let fix_outcome = match opts.fix_mode {
        Some(fix_mode) => apply_workspace_fixes(
            &opts,
            fix_mode,
            &mut workspace,
            &mut loaded_docs,
            &lint_results,
            &mut status,
            &mut diagnostics,
        ),
        None => FixSummary::default(),
    };
    if fix_outcome.fixed_files != 0 {
        lint_results = lint_loaded(&workspace, &loaded_docs);
    }

    diagnostics.extend(analysis_diagnostics(
        &opts,
        &workspace,
        &config,
        &loaded_docs,
        lint_results,
        &mut status,
    ));
    finish_report(&opts, diagnostics, fix_outcome, status)
}

#[derive(Default)]
struct FixSummary {
    fixed_files: usize,
    diff: String,
}

fn source_document(path: &OsStr, text: String) -> SourceDocument {
    if path == "-" {
        SourceDocument::new(None, Some(text)).with_uri("<stdin>")
    } else {
        let path = Path::new(path);
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .expect("current directory available after argument dispatch")
                .join(path)
        };
        let uri = url::Url::from_file_path(&absolute)
            .ok()
            .map(|uri| uri.to_string());
        let source = SourceDocument::file(absolute, Some(text));
        if let Some(uri) = uri {
            source.with_uri(uri)
        } else {
            source
        }
    }
}

fn load_analysis_documents<'a>(
    inputs: &'a [OsString],
    workspace: &mut SourceWorkspace,
    markdown_workspace: &mut SourceWorkspace,
) -> (Vec<AnalyzedDocument<'a>>, u8, Vec<Diagnostic>) {
    let mut status = 0;
    let mut diagnostics = Vec::new();
    let mut loaded_docs = Vec::new();
    if inputs.len() > 1 && inputs.iter().any(|path| path == "-") {
        return (
            loaded_docs,
            2,
            vec![driver_diagnostic(
                "cannot combine `-` (stdin) with multiple paths",
            )],
        );
    }
    for raw_path in inputs {
        let path = Path::new(raw_path);
        let label = if raw_path == "-" {
            "<stdin>".to_owned()
        } else {
            zkcl_terminal::display_path(path)
        };
        let markdown = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        let target = if markdown {
            &mut *markdown_workspace
        } else {
            &mut *workspace
        };
        let source = if raw_path == "-" {
            io::read_to_string(io::stdin().lock())
        } else {
            fs::read_to_string(path)
        };
        match source {
            Ok(source) => {
                let source = source_document(raw_path, source);
                let doc_key = source
                    .uri()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("input:{}", loaded_docs.len()));
                target
                    .set_document_source(&doc_key, source)
                    .expect("read text is a valid source snapshot");
                let snapshot = target.snapshot(&doc_key).expect("input just set");
                loaded_docs.push(AnalyzedDocument {
                    path: raw_path,
                    key: doc_key,
                    label,
                    snapshot,
                    markdown,
                });
            }
            Err(error) => {
                diagnostics.push(driver_diagnostic(format!("cannot read {label}: {error}")));
                status = 2;
            }
        }
    }
    (loaded_docs, status, diagnostics)
}

fn apply_workspace_fixes(
    opts: &LintCliOptions,
    fix_mode: FixMode,
    workspace: &mut SourceWorkspace,
    loaded_docs: &mut [AnalyzedDocument<'_>],
    lint_results: &[Vec<zkcl_syntax::LintDiagnostic>],
    status: &mut u8,
    errors: &mut Vec<Diagnostic>,
) -> FixSummary {
    let mut candidates = Vec::<Suggestion>::new();
    for (doc, diagnostics) in loaded_docs
        .iter()
        .filter(|doc| !doc.markdown)
        .zip(lint_results)
    {
        for diagnostic in diagnostics {
            let Some(report) = diagnostic.to_diagnostic(doc.snapshot.source(), |name| {
                loaded_docs
                    .iter()
                    .find(|doc| doc.key == name)
                    .map(|doc| doc.snapshot.source())
            }) else {
                continue;
            };
            let Some(suggestion) = report.suggestions.into_iter().next() else {
                continue;
            };
            if suggestion.applicability == Applicability::MachineApplicable
                || (opts.unsafe_fixes && suggestion.applicability == Applicability::MaybeIncorrect)
            {
                candidates.push(suggestion);
            }
        }
    }
    let selected = zkcl_diagnostics::select_suggestions(&candidates);
    let changes =
        zkcl_diagnostics::apply_suggestions(selected).expect("selected edits do not overlap");
    if fix_mode == FixMode::Apply {
        // Validate every disk preimage before writing any selected group. Edits
        // analyzed against stdin are rejected before this function is reached.
        for (source, _) in &changes {
            let doc = loaded_docs
                .iter()
                .find(|doc| doc.snapshot.source().snapshot_id() == source.snapshot_id())
                .expect("edit source belongs to loaded workspace");
            match fs::read_to_string(doc.path) {
                Ok(text) if Some(text.as_str()) == source.text() => {}
                Ok(_) => {
                    errors.push(driver_diagnostic(format!(
                        "{} changed after analysis; analyze the current revision again",
                        doc.label
                    )));
                    *status = 2;
                    return FixSummary::default();
                }
                Err(error) => {
                    errors.push(driver_diagnostic(format!(
                        "cannot read {} before applying fixes: {error}",
                        doc.label
                    )));
                    *status = 2;
                    return FixSummary::default();
                }
            }
        }
    }
    let mut outcome = FixSummary::default();
    for (source, updated) in changes {
        let doc = loaded_docs
            .iter_mut()
            .find(|doc| doc.snapshot.source().snapshot_id() == source.snapshot_id())
            .expect("edit source belongs to loaded workspace");
        if Some(updated.as_str()) == source.text() {
            continue;
        }
        match fix_mode {
            FixMode::Apply => {
                if let Err(error) = fs::write(doc.path, &updated) {
                    errors.push(driver_diagnostic(format!(
                        "cannot write fixes to {}: {error}",
                        doc.label
                    )));
                    *status = 2;
                    continue;
                }
                outcome.fixed_files += 1;
                workspace
                    .set_document_source(&doc.key, source_document(doc.path, updated))
                    .expect("fixed text is valid source");
                doc.snapshot = workspace
                    .snapshot(&doc.key)
                    .expect("input was just updated");
            }
            FixMode::Preview => {
                write_fix_diff(&mut outcome.diff, &doc.label, doc.snapshot.text(), &updated)
            }
        }
    }
    outcome
}

fn analysis_diagnostics(
    opts: &LintCliOptions,
    workspace: &SourceWorkspace,
    config: &LintConfig,
    loaded_docs: &[AnalyzedDocument<'_>],
    lint_results: Vec<Vec<zkcl_syntax::LintDiagnostic>>,
    status: &mut u8,
) -> Vec<Diagnostic> {
    let mut sources = HashMap::with_capacity(loaded_docs.len());
    for doc in loaded_docs {
        sources
            .entry(doc.key.as_str())
            .or_insert_with(|| doc.snapshot.source());
    }
    let mut reports = Vec::new();
    let mut lint_results = lint_results.into_iter();
    for doc in loaded_docs {
        let markdown_tests;
        let (syntax, documentation) = if doc.markdown {
            markdown_tests = DocumentationTests::markdown(doc.snapshot.text(), opts.readtable);
            (&[][..], &markdown_tests)
        } else {
            (
                doc.snapshot.diagnostics(),
                doc.snapshot.documentation_tests(),
            )
        };
        for diagnostic in syntax {
            *status = (*status).max(1);
            reports.push(diagnostic.to_diagnostic(doc.snapshot.source()));
        }
        for diagnostic in &documentation.diagnostics {
            *status = (*status).max(1);
            let mut report = Diagnostic::new(Severity::Error, diagnostic.message);
            report.code = Some("doc/example".into());
            report
                .add_label(
                    doc.snapshot.source(),
                    usize::from(diagnostic.range.start())..usize::from(diagnostic.range.end()),
                    LabelRole::Primary,
                    "",
                )
                .expect("documentation range belongs to analyzed source");
            reports.push(report);
        }
        let lint = if doc.markdown {
            workspace.lint_documentation(documentation, config)
        } else {
            lint_results
                .next()
                .expect("lint results follow source request order")
        };
        for diagnostic in lint {
            if let Some(report) =
                diagnostic.to_diagnostic(doc.snapshot.source(), |name| sources.get(name).copied())
            {
                if report.severity == Severity::Error
                    || (opts.deny_warnings && report.severity == Severity::Warning)
                {
                    *status = (*status).max(1);
                }
                reports.push(report);
            }
        }
    }
    reports
}

// One unified hunk per input, trimming unchanged edges to three lines of context.
fn write_fix_diff(output: &mut String, label: &str, original: &str, updated: &str) {
    const CONTEXT: usize = 3;
    let old: Vec<_> = original.split_inclusive('\n').collect();
    let new: Vec<_> = updated.split_inclusive('\n').collect();
    let common_prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let common_suffix = old[common_prefix..]
        .iter()
        .rev()
        .zip(new[common_prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let start = common_prefix.saturating_sub(CONTEXT);
    let old_end = old.len() - common_suffix;
    let new_end = new.len() - common_suffix;
    let trailing = common_suffix.min(CONTEXT);
    let old_len = old_end + trailing - start;
    let new_len = new_end + trailing - start;

    if label.contains(['\t', '\r', '\n']) {
        let _ = writeln!(output, "--- {label:?}\n+++ {label:?}");
    } else {
        let _ = writeln!(output, "--- {label}\n+++ {label}");
    }
    let _ = writeln!(
        output,
        "@@ -{},{old_len} +{},{new_len} @@",
        start + usize::from(old_len != 0),
        start + usize::from(new_len != 0),
    );
    for (marker, lines) in [
        (' ', &old[start..common_prefix]),
        ('-', &old[common_prefix..old_end]),
        ('+', &new[common_prefix..new_end]),
        (' ', &old[old_end..old_end + trailing]),
    ] {
        for line in lines {
            output.push(marker);
            output.push_str(line);
            if !line.ends_with('\n') {
                output.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ConfigLoadError {
    #[error("failed to read config {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Config(#[from] zkcl_syntax::LintConfigError),
}

fn resolve_lint_config(
    opts: &LintCliOptions,
    mode: AnalysisCommand,
) -> Result<LintConfig, ConfigLoadError> {
    let mut cfg = if let Some(path) = &opts.config_path {
        load_config_file(path)?
    } else if let Some(discovered) = discover_config_file(&opts.paths) {
        load_config_file(&discovered)?
    } else {
        LintConfig::from_preset(opts.preset.unwrap_or_else(|| mode.default_preset()))
    };

    if let Some(preset) = opts.preset {
        cfg.preset = preset;
    }
    for (target, level) in &opts.overrides {
        cfg.set_target_level(target, *level)?;
    }
    Ok(cfg)
}

fn load_config_file(path: &Path) -> Result<LintConfig, ConfigLoadError> {
    let content = fs::read_to_string(path).map_err(|source| ConfigLoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    if path.extension().and_then(|e| e.to_str()) == Some("toml")
        || path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".toml"))
    {
        Ok(LintConfig::parse_toml(&content)?)
    } else {
        Ok(LintConfig::parse_sexp(&content)?)
    }
}

fn discover_config_file(paths: &[OsString]) -> Option<PathBuf> {
    let start_dir = paths
        .first()
        .filter(|p| *p != "-")
        .map(Path::new)
        .and_then(|p| {
            if p.is_dir() {
                Some(p.to_path_buf())
            } else {
                p.parent()
                    .map(Path::to_path_buf)
                    .filter(|parent| !parent.as_os_str().is_empty())
            }
        })
        .or_else(|| std::env::current_dir().ok())?;

    let mut cur = start_dir.as_path();
    loop {
        let lisp = cur.join(".zkcl-lint.lisp");
        if lisp.is_file() {
            return Some(lisp);
        }
        let toml = cur.join(".zkcl-lint.toml");
        if toml.is_file() {
            return Some(toml);
        }
        cur = cur.parent()?;
    }
}

fn expand_input_paths(raw: &[OsString]) -> (Vec<OsString>, Option<String>) {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for item in raw {
        if item == "-" {
            files.push(item.clone());
            continue;
        }
        let path = Path::new(item);
        if path.is_dir() {
            let mut dir_files = BTreeSet::new();
            if let Err(err) = collect_dir_lisp_files(path, &mut dir_files) {
                return (
                    Vec::new(),
                    Some(format!("reading {}: {err}", path.display())),
                );
            }
            for df in dir_files {
                if seen.insert(df.clone()) {
                    files.push(df.into_os_string());
                }
            }
        } else {
            let key = path.to_path_buf();
            if seen.insert(key.clone()) {
                files.push(key.into_os_string());
            }
        }
    }
    (files, None)
}

fn collect_dir_lisp_files(dir: &Path, out: &mut BTreeSet<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.is_dir() {
            if matches!(name_str.as_ref(), "target" | "node_modules") {
                continue;
            }
            collect_dir_lisp_files(&path, out)?;
        } else if metadata.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| matches!(ext, "lisp" | "cl" | "lsp" | "asd"))
        {
            out.insert(path);
        }
    }
    Ok(())
}

fn rule_json(rule: &zkcl_syntax::RuleDescriptor) -> serde_json::Value {
    serde_json::json!({"id": rule.id, "short_name": rule.short_name,
        "category": rule.category.as_str(), "default_preset": rule.default_preset.as_str(),
        "evidence": rule.evidence.as_str(), "summary": rule.summary, "rationale": rule.rationale,
        "bad_example": rule.bad_example, "good_example": rule.good_example})
}

fn print_rule_list() {
    println!("{:<42} {:<11} {:<8} Summary", "Rule", "Category", "Preset");
    println!("{}", "-".repeat(88));
    for rule in all_rules() {
        println!(
            "{:<42} {:<11} {:<8} {}",
            rule.id,
            rule.category.as_str(),
            rule.default_preset.as_str(),
            rule.summary
        );
    }
}

fn print_rule_explanation(query: &str) -> ExitCode {
    let Some(rule) = find_rule(query) else {
        eprintln!(
            "{}",
            zkcl_terminal::render_stderr(&driver_diagnostic(format!(
                "unknown lint rule `{query}`; run `zkcl explain --list` to see available rules"
            )))
        );
        return ExitCode::from(2);
    };
    println!("Rule:     {} ({})", rule.id, rule.short_name);
    println!("Category: {}", rule.category.as_str());
    println!("Default:  {}", rule.default_preset.as_str());
    println!("Basis:    {}", rule.evidence.as_str());
    println!();
    println!("Summary:\n  {}", rule.summary);
    println!();
    println!("Why it matters:\n  {}", rule.rationale);
    println!();
    println!("Bad:\n  {}", rule.bad_example.replace('\n', "\n  "));
    println!();
    println!("Good:\n  {}", rule.good_example.replace('\n', "\n  "));
    ExitCode::SUCCESS
}

pub(super) fn format(mut request: super::args::FormatArgs) -> ExitCode {
    if let Err(error) = super::project::analysis_paths(&request.project, &mut request.paths, false)
    {
        eprintln!(
            "{}",
            zkcl_terminal::render_stderr(&driver_diagnostic(error.to_string()))
        );
        return ExitCode::from(2);
    }
    let check_only = request.check;
    let readtable = SourceReadtable::named(&request.readtable).expect("clap validated readtable");
    if request.paths.is_empty() {
        eprintln!(
            "{}",
            zkcl_terminal::render_stderr(&driver_diagnostic(
                "provide source paths or select a project"
            ))
        );
        return ExitCode::from(2);
    }
    let raw: Vec<_> = request
        .paths
        .into_iter()
        .map(PathBuf::into_os_string)
        .collect();
    let (paths, error) = expand_input_paths(&raw);
    if let Some(error) = error {
        eprintln!(
            "{}",
            zkcl_terminal::render_stderr(&driver_diagnostic(error))
        );
        return ExitCode::from(2);
    }
    let mut workspace = SourceWorkspace::new();
    workspace.set_readtable(readtable);
    let mut status = 0u8;
    for path in paths {
        let is_stdin = path == "-";
        let (label, snapshot) = read_snapshot(&mut workspace, &path);
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!(
                    "{}",
                    zkcl_terminal::render_stderr(&driver_diagnostic(format!(
                        "cannot read {label}: {error}"
                    )))
                );
                status = 2;
                continue;
            }
        };

        if !snapshot.diagnostics().is_empty() {
            for diagnostic in snapshot.diagnostics() {
                eprintln!(
                    "{}",
                    zkcl_terminal::render_stderr(&diagnostic.to_diagnostic(snapshot.source()))
                );
            }
            status = status.max(1);
            continue;
        }
        let formatted = snapshot.formatted_text();
        let formatted = formatted.as_deref().unwrap_or(snapshot.text());
        let changed = formatted != snapshot.text();
        if check_only {
            if changed {
                eprintln!(
                    "{}",
                    zkcl_terminal::render_stderr(&Diagnostic::new(
                        Severity::Note,
                        format!("would reformat {label}")
                    ))
                );
                status = status.max(1);
            }
        } else if is_stdin {
            if let Err(error) = io::stdout().lock().write_all(formatted.as_bytes()) {
                eprintln!(
                    "{}",
                    zkcl_terminal::render_stderr(&driver_diagnostic(format!(
                        "cannot write formatted output: {error}"
                    )))
                );
                status = 2;
            }
        } else if changed && let Err(error) = fs::write(&path, formatted) {
            eprintln!(
                "{}",
                zkcl_terminal::render_stderr(&driver_diagnostic(format!(
                    "cannot write {label}: {error}"
                )))
            );
            status = 2;
        }
    }
    ExitCode::from(status)
}

fn read_snapshot(
    workspace: &mut SourceWorkspace,
    path: &OsStr,
) -> (String, io::Result<Arc<SourceSnapshot>>) {
    let (label, source) = if path == "-" {
        ("<stdin>".to_owned(), io::read_to_string(io::stdin().lock()))
    } else {
        let label = zkcl_terminal::display_path(Path::new(path));
        (label, fs::read_to_string(path))
    };
    let snapshot = source.map(|source| {
        workspace
            .set_document_source(INPUT_DOCUMENT, source_document(path, source))
            .expect("read text is a valid source snapshot");
        workspace
            .snapshot(INPUT_DOCUMENT)
            .expect("the input was just set")
    });
    (label, snapshot)
}
