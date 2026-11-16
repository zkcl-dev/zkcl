//! The complete command grammar. Parsing is deliberately independent of project discovery.
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "zkcl",
    version,
    about = "Common Lisp runtime and project tools",
    after_help = "Managed installations also provide `zkcl toolchain --help` and +TOOLCHAIN selection.",
    subcommand_required = true
)]
pub(crate) struct Cli {
    /// Run from another directory.
    #[arg(short = 'C', long, global = true, help_heading = "Global options")]
    pub directory: Option<PathBuf>,
    /// Managed Lisp heap limit in MiB.
    #[arg(
        long,
        global = true,
        allow_negative_numbers = true,
        value_name = "MIB",
        value_parser = heap_size,
        help_heading = "Global options"
    )]
    pub heap_size_mib: Option<usize>,
    /// Control terminal colors.
    #[arg(
        long,
        global = true,
        value_enum,
        default_value = "auto",
        help_heading = "Global options"
    )]
    pub color: Color,
    #[command(subcommand)]
    pub command: Command,
}
impl Cli {
    /// Parse the tree once, retaining command-order semantics for lint overrides.
    pub fn try_request_from(
        arguments: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    ) -> Result<Self, clap::Error> {
        use clap::{CommandFactory, FromArgMatches};
        let matches = Self::command().try_get_matches_from(arguments)?;
        let mut request = Self::from_arg_matches(&matches)?;
        if let Command::Check(options) | Command::Lint(options) = &mut request.command {
            let (_, command) = matches.subcommand().expect("analysis command has matches");
            let mut overrides = Vec::new();
            for (field, level) in [
                ("enable", RuleLevel::Warn),
                ("disable", RuleLevel::Allow),
                ("deny", RuleLevel::Deny),
            ] {
                if let (Some(indices), Some(values)) =
                    (command.indices_of(field), command.get_many::<String>(field))
                {
                    overrides.extend(
                        indices
                            .zip(values)
                            .map(|(index, value)| (index, value.clone(), level)),
                    );
                }
            }
            overrides.sort_by_key(|(index, _, _)| *index);
            options.overrides = overrides
                .into_iter()
                .map(|(_, value, level)| (value, level))
                .collect();
        }
        Ok(request)
    }
}
#[derive(Debug, Clone, Copy)]
pub(crate) enum RuleLevel {
    Warn,
    Allow,
    Deny,
}

fn heap_size(value: &str) -> Result<usize, String> {
    value
        .parse::<NonZeroUsize>()
        .ok()
        .and_then(|n| n.get().checked_mul(1024 * 1024))
        .ok_or_else(|| "expected a positive whole number of MiB that fits this host".to_owned())
}
pub(crate) use zkcl_terminal::ColorMode as Color;

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run a source file, FASL, Wasm module, or project binary.
    Run(RunArgs),
    /// Compile a source file to bytecode.
    Compile(CompileArgs),
    /// Build a native executable, library, or project target.
    Build(BuildArgs),
    /// Start an interactive or JSON Lisp session.
    Repl(ReplArgs),
    /// Run project tests, ASDF tests, or documentation examples.
    Test(TestArgs),
    /// Check syntax and conservative correctness rules without evaluation.
    Check(AnalysisArgs),
    /// Check syntax and configured quality rules without evaluation.
    Lint(AnalysisArgs),
    /// Explain a lint rule or list built-in rules.
    Explain(ExplainArgs),
    /// Format Lisp source files.
    Fmt(FormatArgs),
    /// Serve the Language Server Protocol over stdio.
    Lsp,
    /// Serve the Debug Adapter Protocol over stdio.
    Dap,
    /// Show project targets, dependencies, and build settings.
    Info(InfoArgs),
    /// Remove generated project build files.
    Clean(ProjectSelection),
    /// Create a runnable Lisp project in a new directory.
    New(NewArgs),
    /// Resolve dependencies and write zkcl.lock.
    Lock(ResolveArgs),
    /// Update a package or the full locked graph.
    Update(UpdateArgs),
    /// Show the selected dependency graph.
    Tree(TreeArgs),
    /// Cache locked source archives.
    Fetch(FetchArgs),
    /// Add a registry dependency and lock it.
    Add(AddArgs),
    /// Remove a dependency and update the lock.
    Remove(RemoveArgs),
    /// Create or inspect a canonical source archive.
    Package(PackageArgs),
    /// Publish the current project.
    Publish(PublishArgs),
    /// Run a binary from a registry release.
    #[command(display_name = "zkcl", version)]
    X(ExecuteArgs),
    /// Manage workspace membership and versions.
    #[command(subcommand)]
    Work(WorkCommand),
    /// Manage a local package registry.
    #[command(subcommand)]
    Registry(RegistryCommand),
    /// Save registry credentials.
    Login(AccountArgs),
    /// Remove saved registry credentials.
    Logout(AccountArgs),
    /// Manage registry publishing permissions.
    Owner(OwnerArgs),
    /// Generate shell completion scripts.
    Completions(CompletionArgs),
}
#[derive(Debug, Clone, Default, Args)]
pub(crate) struct ProjectSelection {
    /// Select a project manifest explicitly.
    #[arg(help_heading = "Project selection", long, value_name = "PATH")]
    pub manifest_path: Option<PathBuf>,
    /// Select a workspace member by module or path (repeatable).
    #[arg(
        help_heading = "Project selection",
        long,
        value_name = "MODULE",
        conflicts_with = "workspace"
    )]
    pub member: Vec<String>,
    /// Visit every workspace member.
    #[arg(help_heading = "Project selection", long)]
    pub workspace: bool,
}
impl ProjectSelection {
    pub fn explicit(&self) -> bool {
        self.manifest_path.is_some() || !self.member.is_empty() || self.workspace
    }
}

#[derive(Debug, Clone, Default, Args)]
pub(crate) struct ProjectOptions {
    #[command(flatten)]
    pub selection: ProjectSelection,
    /// Require the locked dependency graph.
    #[arg(help_heading = "Dependencies", long)]
    pub locked: bool,
    /// Use only cached sources and the locked graph.
    #[arg(help_heading = "Dependencies", long)]
    pub offline: bool,
    /// Use a local registry or signed registry descriptor (repeatable).
    #[arg(help_heading = "Dependencies", long, value_name = "PATH")]
    pub registry: Vec<PathBuf>,
    /// Select an exact set of project features (repeatable).
    #[arg(help_heading = "Dependencies", long, value_name = "NAME")]
    pub features: Vec<String>,
}
impl ProjectOptions {
    pub fn explicit(&self) -> bool {
        self.selection.explicit()
            || self.locked
            || self.offline
            || !self.registry.is_empty()
            || !self.features.is_empty()
    }
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum JitBackend {
    Direct,
    Cranelift,
}
#[derive(Debug, Clone, Default, Args)]
pub(crate) struct RuntimeOptions {
    /// Search this directory for REQUIRE modules (repeatable).
    #[arg(
        help_heading = "Runtime",
        short = 'I',
        long = "module-dir",
        value_name = "DIR"
    )]
    pub module_dir: Vec<PathBuf>,
    /// Enable the first available JIT backend.
    #[arg(help_heading = "Runtime", long, conflicts_with = "jit_backend")]
    pub jit: bool,
    /// Enable a specific JIT backend.
    #[arg(help_heading = "Runtime", long, value_enum, value_name = "BACKEND")]
    pub jit_backend: Option<JitBackend>,
}
impl RuntimeOptions {
    pub fn jit_request(&self) -> Option<super::JitBackendRequest> {
        self.jit_backend
            .map(|backend| match backend {
                JitBackend::Direct => super::JitBackendRequest::Direct,
                JitBackend::Cranelift => super::JitBackendRequest::Cranelift,
            })
            .or(self.jit.then_some(super::JitBackendRequest::Default))
    }
}
#[derive(Debug, Clone, Args)]
#[command(
    after_long_help = "Examples:\n  zkcl run script.lisp -- first second\n  zkcl run --list\n  zkcl run --bin demo -- argument\n\nRead arguments with (zk:command-line-arguments). Request an exit status with (zk:set-exit-code N)."
)]
pub(crate) struct RunArgs {
    #[command(flatten)]
    pub project: ProjectOptions,
    #[command(flatten)]
    pub runtime: RuntimeOptions,
    /// Source, FASL, or Wasm input; omit to select a project binary.
    #[arg(value_name = "PATH", conflicts_with_all = ["bin", "list", "manifest_path", "member", "workspace", "locked", "offline", "registry", "features"])]
    pub path: Option<PathBuf>,
    /// Select a project binary.
    #[arg(long, value_name = "NAME", conflicts_with = "list", add = clap_complete::ArgValueCompleter::new(bin_completions))]
    pub bin: Option<String>,
    /// List project binaries without resolving dependencies or building.
    #[arg(long, conflicts_with_all = ["arguments", "jit", "jit_backend", "module_dir"])]
    pub list: bool,
    /// Arguments passed unchanged to the selected program.
    #[arg(last = true, value_name = "ARGS", allow_hyphen_values = true)]
    pub arguments: Vec<OsString>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct CompileArgs {
    #[arg(long, value_parser = target)]
    pub target: Option<String>,
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    #[arg(value_name = "OUTPUT")]
    pub output: Option<PathBuf>,
}
pub(crate) fn target(value: &str) -> Result<String, String> {
    value
        .parse::<zkcl::CompilationTarget>()
        .map(|target| target.to_string())
        .map_err(|e| e.to_string())
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum AotBackend {
    Direct,
    Cranelift,
    Llvm,
}
impl AotBackend {
    pub fn kind(self) -> zkcl_aot::AotBackendKind {
        match self {
            Self::Direct => zkcl_aot::AotBackendKind::Direct,
            Self::Cranelift => zkcl_aot::AotBackendKind::CraneliftObject,
            Self::Llvm => zkcl_aot::AotBackendKind::Llvm,
        }
    }
}
fn aot_backend(value: &str) -> Result<AotBackend, String> {
    let backend = AotBackend::from_str(value, false)?;
    if backend.kind().is_available() {
        Ok(backend)
    } else {
        Err(format!(
            "{value} AOT backend is not enabled in this zkcl build"
        ))
    }
}
#[derive(Debug, Clone, Args)]
pub(crate) struct BuildArgs {
    #[command(flatten)]
    pub project: ProjectOptions,
    /// Input file; omit to build project targets.
    #[arg(value_name = "PATH", conflicts_with_all = ["bin", "manifest_path", "member", "workspace", "locked", "offline", "registry", "features"])]
    pub path: Option<PathBuf>,
    #[arg(long, value_name = "NAME", add = clap_complete::ArgValueCompleter::new(bin_completions))]
    pub bin: Option<String>,
    #[arg(short, long, value_name = "PATH")]
    pub output: Option<PathBuf>,
    #[arg(long, value_enum, value_parser = aot_backend)]
    pub backend: Option<AotBackend>,
    #[arg(short = 'j', long)]
    pub jobs: Option<NonZeroUsize>,
    #[arg(long, value_parser = target)]
    pub target: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub wasm_runtime: Option<PathBuf>,
    /// Share a JavaScript runtime module between applications in the output directory.
    #[arg(long, requires = "target")]
    pub split_runtime: bool,
    #[arg(long, requires = "path")]
    pub staticlib: bool,
    #[arg(long, value_name = "DIR")]
    pub emit_dir: Option<PathBuf>,
    #[arg(long, requires = "path")]
    pub module_id: Option<String>,
    #[arg(long, requires = "path")]
    pub list_entries: bool,
    #[arg(long)]
    pub release: bool,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct ReplArgs {
    #[command(flatten)]
    pub project: ProjectOptions,
    #[command(flatten)]
    pub runtime: RuntimeOptions,
    /// Load a project binary's files without calling its entry point.
    #[arg(long, value_name = "NAME", add = clap_complete::ArgValueCompleter::new(bin_completions))]
    pub bin: Option<String>,
    /// Evaluate newline-delimited JSON requests.
    #[arg(long)]
    pub json: bool,
    /// Load a Lisp source file or FASL before accepting input (repeatable).
    #[arg(long, value_name = "PATH")]
    pub load: Vec<PathBuf>,
    /// Read this initialization file in place of the default.
    #[arg(long, value_name = "PATH", conflicts_with = "no_init")]
    pub init: Option<PathBuf>,
    /// Skip the user initialization file.
    #[arg(long)]
    pub no_init: bool,
}
impl ReplArgs {
    pub fn init_file(&self) -> super::repl::InitFile {
        if self.no_init {
            super::repl::InitFile::Disabled
        } else if let Some(path) = &self.init {
            super::repl::InitFile::Explicit(path.clone())
        } else {
            super::repl::InitFile::default()
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum TestFormat {
    Text,
    Json,
    Junit,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct TestArgs {
    #[command(flatten)]
    pub project: ProjectOptions,
    /// ASDF system name; omit to run declared project tests.
    #[arg(conflicts_with_all = ["manifest_path", "member", "workspace", "locked", "offline", "registry", "features"])]
    pub system: Option<String>,
    #[arg(long, conflicts_with_all = ["seed", "trials", "max_size", "max_discards", "max_discard_ratio", "max_shrinks", "max_enumerated", "filter", "corpus", "jobs", "format"])]
    pub doc: bool,
    #[arg(long, requires = "system")]
    pub seed: Option<u64>,
    #[arg(long, requires = "system")]
    pub trials: Option<u64>,
    #[arg(long, requires = "system")]
    pub max_size: Option<u64>,
    #[arg(long, requires = "system")]
    pub max_discards: Option<u64>,
    #[arg(long, requires = "system")]
    pub max_discard_ratio: Option<u64>,
    #[arg(long, requires = "system")]
    pub max_shrinks: Option<u64>,
    #[arg(long, requires = "system")]
    pub max_enumerated: Option<u64>,
    #[arg(long, requires = "system")]
    pub filter: Option<String>,
    #[arg(long, requires = "system")]
    pub corpus: Option<String>,
    #[arg(short = 'j', long, requires = "system")]
    pub jobs: Option<NonZeroUsize>,
    #[arg(long, value_enum, requires = "system")]
    pub format: Option<TestFormat>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum DiagnosticFormat {
    Text,
    Short,
    Json,
}
#[derive(Debug, Clone, Args)]
#[command(group(clap::ArgGroup::new("fix_mode").args(["fix", "fix_dry_run"]).multiple(false)))]
pub(crate) struct AnalysisArgs {
    #[command(flatten)]
    pub project: ProjectSelection,
    #[arg(
        short = 'j',
        long,
        value_name = "COUNT",
        help = "Limit concurrent source analysis jobs"
    )]
    pub jobs: Option<NonZeroUsize>,
    #[arg(long, value_parser = target)]
    pub target: Option<String>,
    #[arg(long, value_parser = ["standard", "raw-strings"], default_value = "standard")]
    pub readtable: String,
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long, value_parser = ["default", "strict", "all"])]
    pub preset: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub enable: Vec<String>,
    #[arg(long, alias = "allow", value_delimiter = ',')]
    pub disable: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    pub deny: Vec<String>,
    #[arg(skip)]
    pub overrides: Vec<(String, RuleLevel)>,
    #[arg(long)]
    pub deny_warnings: bool,
    #[arg(long, conflicts_with_all = ["list_rules", "explain"])]
    pub fix: bool,
    #[arg(long, conflicts_with_all = ["list_rules", "explain"])]
    pub fix_dry_run: bool,
    #[arg(long, requires = "fix_mode")]
    pub unsafe_fixes: bool,
    #[arg(long, value_enum, default_value = "text")]
    pub format: DiagnosticFormat,
    #[arg(long, conflicts_with = "explain")]
    pub list_rules: bool,
    #[arg(long, value_name = "RULE")]
    pub explain: Option<String>,
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct FormatArgs {
    #[command(flatten)]
    pub project: ProjectSelection,
    #[arg(long)]
    pub check: bool,
    #[arg(long, value_parser = ["standard", "raw-strings"], default_value = "standard")]
    pub readtable: String,
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct ExplainArgs {
    #[arg(long, conflicts_with = "rule")]
    pub list: bool,
    pub rule: Option<String>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct InfoArgs {
    #[command(flatten)]
    pub project: ProjectSelection,
    #[arg(long)]
    pub json: bool,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct ResolveArgs {
    #[command(flatten)]
    pub project: ProjectOptions,
    #[arg(long, value_parser = target)]
    pub target: Option<String>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct UpdateArgs {
    #[command(flatten)]
    pub resolve: ResolveArgs,
    pub package: Option<String>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct TreeArgs {
    #[command(flatten)]
    pub resolve: ResolveArgs,
    #[arg(long)]
    pub json: bool,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct FetchArgs {
    #[command(flatten)]
    pub resolve: ResolveArgs,
    #[arg(long, conflicts_with = "features")]
    pub all_configurations: bool,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct AddArgs {
    #[command(flatten)]
    pub project: ProjectOptions,
    pub package: String,
    #[arg(long = "as")]
    pub alias: Option<String>,
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long)]
    pub system: Option<String>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct RemoveArgs {
    #[command(flatten)]
    pub project: ProjectOptions,
    pub package: String,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct PackageArgs {
    #[command(flatten)]
    pub project: ProjectSelection,
    #[arg(long, conflicts_with = "output")]
    pub list: bool,
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct PublishArgs {
    #[command(flatten)]
    pub project: ProjectSelection,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long, value_name = "PATH|URL")]
    pub registry: PathBuf,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct ExecuteArgs {
    pub package: String,
    #[arg(long)]
    pub bin: Option<String>,
    #[arg(long)]
    pub offline: bool,
    #[arg(long)]
    pub registry: Vec<PathBuf>,
    #[arg(long)]
    pub from: Option<String>,
    #[arg(last = true, allow_hyphen_values = true)]
    pub arguments: Vec<OsString>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct NewArgs {
    /// Directory to create.
    pub path: PathBuf,
    /// Module name in namespace/name form.
    #[arg(long, value_name = "MODULE")]
    pub name: zkcl_project::manifest::ModuleName,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct WorkPaths {
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct WorkSelection {
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct WorkList {
    #[command(flatten)]
    pub selection: WorkSelection,
    #[arg(long)]
    pub json: bool,
}
#[derive(Debug, Clone, Subcommand)]
pub(crate) enum WorkCommand {
    Init(WorkPaths),
    Use(WorkPaths),
    /// Remove projects from the workspace without deleting their files.
    Remove(WorkMembers),
    Sync(WorkSelection),
    List(WorkList),
}
#[derive(Debug, Clone, Args)]
pub(crate) struct WorkMembers {
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,
    #[arg(required = true, value_name = "MODULE|PATH")]
    pub members: Vec<String>,
}
#[derive(Debug, Clone, Subcommand)]
pub(crate) enum RegistryCommand {
    Init { identity: String, path: PathBuf },
    Yank(RegistryRelease),
    Unyank(RegistryRelease),
    ExportSparse(SparseExportArgs),
}
#[derive(Debug, Clone, Args)]
pub(crate) struct RegistryRelease {
    #[arg(long)]
    pub registry: PathBuf,
    pub package: String,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct SparseExportArgs {
    #[arg(long)]
    pub registry: PathBuf,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long, requires_all = ["key", "version", "expires"])]
    pub trusted_root: Option<PathBuf>,
    #[arg(long, requires = "trusted_root")]
    pub key: Vec<PathBuf>,
    #[arg(long, requires = "trusted_root")]
    pub version: Option<NonZeroU64>,
    #[arg(long, requires = "trusted_root")]
    pub expires: Option<String>,
}
#[derive(Debug, Clone, Args)]
pub(crate) struct AccountArgs {
    #[arg(long)]
    pub registry: String,
}
#[derive(Debug, Clone, Args)]
#[command(group(clap::ArgGroup::new("registry_account").args(["registry"]).required(true)))]
pub(crate) struct OwnerArgs {
    #[arg(long, global = true)]
    pub registry: Option<String>,
    #[command(subcommand)]
    pub operation: OwnerOperation,
}
#[derive(Debug, Clone, Subcommand)]
pub(crate) enum OwnerOperation {
    Claim { namespace: String },
    Add { module: String, subject: String },
    Remove { module: String, subject: String },
    NamespaceAdd { namespace: String, subject: String },
    NamespaceRemove { namespace: String, subject: String },
}
#[derive(Debug, Clone, Args)]
pub(crate) struct CompletionArgs {
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

// Completion protocol framing is separate from command parsing: the words after
// its first `--` are parsed by this exact tree with incomplete input allowed.
fn bin_completions(current: &std::ffi::OsStr) -> Vec<clap_complete::CompletionCandidate> {
    use clap::{CommandFactory, FromArgMatches};
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let words: Vec<_> = std::env::args_os()
        .skip_while(|word| word != "--")
        .skip(1)
        .collect();
    if words.is_empty() {
        return Vec::new();
    }
    let Ok(matches) = Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(words)
    else {
        return Vec::new();
    };
    let Some((_, command)) = matches.subcommand() else {
        return Vec::new();
    };
    let Ok(selection) = ProjectSelection::from_arg_matches(command) else {
        return Vec::new();
    };
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let directory = matches
        .get_one::<PathBuf>("directory")
        .map(|path| cwd.join(path))
        .unwrap_or(cwd);
    super::project::bin_candidates(selection, &directory)
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, help)| clap_complete::CompletionCandidate::new(name).help(Some(help.into())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn complete_tree_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn help_works_after_command_arguments() {
        for words in [
            vec!["zkcl", "run", "--bin", "demo", "--help"],
            vec!["zkcl", "build", "input.lisp", "--release", "--help"],
            vec!["zkcl", "repl", "--load", "init.lisp", "--help"],
            vec!["zkcl", "work", "use", "project", "--help"],
            vec![
                "zkcl",
                "registry",
                "yank",
                "--registry",
                "registry",
                "test/tool@1.0.0",
                "--help",
            ],
            vec!["zkcl", "owner", "add", "test/core", "alice", "--help"],
        ] {
            assert_eq!(
                Cli::try_parse_from(words).unwrap_err().kind(),
                clap::error::ErrorKind::DisplayHelp
            );
        }
    }

    #[test]
    fn separator_preserves_flags_as_program_arguments() {
        let cli = Cli::try_parse_from([
            "zkcl",
            "run",
            "script.lisp",
            "--",
            "--help",
            "--bin",
            "demo",
        ])
        .unwrap();
        let Command::Run(request) = cli.command else {
            panic!("run command")
        };
        assert_eq!(request.path, Some("script.lisp".into()));
        assert_eq!(
            request.arguments,
            ["--help", "--bin", "demo"].map(OsString::from)
        );
    }

    #[test]
    fn invalid_options_values_and_conflicts_fail_in_the_grammar() {
        for words in [
            vec!["zkcl", "run", "--bogus"],
            vec!["zkcl", "run", "--bin"],
            vec!["zkcl", "run", "--bin", "demo", "script.lisp"],
            vec!["zkcl", "run", "--list", "--bin", "demo"],
            vec!["zkcl", "build", "source.lisp", "--jobs", "0"],
            vec![
                "zkcl",
                "compile",
                "--target",
                "not-a-triple-$$$",
                "source.lisp",
            ],
            vec!["zkcl", "repl", "--init", "init.lisp", "--no-init"],
            vec!["zkcl", "run", "--jit", "--jit-backend", "direct"],
            vec!["zkcl", "lint", "--fix", "--fix-dry-run", "source.lisp"],
            vec!["zkcl", "lint", "--unsafe-fixes", "source.lisp"],
            vec!["zkcl", "test", "--doc", "--seed", "1", "system"],
            vec![
                "zkcl",
                "registry",
                "export-sparse",
                "--registry",
                "r",
                "--output",
                "o",
                "--key",
                "key",
            ],
            vec![
                "zkcl",
                "owner",
                "claim",
                "test",
                "extra",
                "--registry",
                "https://registry.test",
            ],
        ] {
            assert!(Cli::try_parse_from(&words).is_err(), "accepted {words:?}");
        }
    }

    #[test]
    fn lint_override_order_is_retained_across_option_kinds() {
        let cli = Cli::try_request_from([
            "zkcl",
            "lint",
            "--deny",
            "style",
            "--enable",
            "style",
            "--disable",
            "unused,complexity",
        ])
        .unwrap();
        let Command::Lint(request) = cli.command else {
            panic!("lint command")
        };
        assert!(
            matches!(request.overrides.as_slice(), [(a, RuleLevel::Deny), (b, RuleLevel::Warn), (c, RuleLevel::Allow), (d, RuleLevel::Allow)]
            if a == "style" && b == "style" && c == "unused" && d == "complexity")
        );
    }

    #[test]
    fn global_options_accept_any_legal_position() {
        let cli = Cli::try_parse_from([
            "zkcl",
            "run",
            "--bin",
            "demo",
            "-C",
            "project",
            "--heap-size-mib",
            "32",
            "--color",
            "never",
        ])
        .unwrap();
        assert_eq!(cli.directory, Some("project".into()));
        assert_eq!(cli.heap_size_mib, Some(32 * 1024 * 1024));
        assert!(matches!(cli.color, Color::Never));
    }

    #[cfg(unix)]
    #[test]
    fn native_paths_and_forwarded_arguments_keep_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(vec![b'f', 0xff]);
        let argument = OsString::from_vec(vec![b'a', 0xfe]);
        let cli = Cli::try_parse_from([
            OsString::from("zkcl"),
            "run".into(),
            path.clone(),
            "--".into(),
            argument.clone(),
        ])
        .unwrap();
        let Command::Run(request) = cli.command else {
            panic!("run command")
        };
        assert_eq!(request.path.unwrap().as_os_str(), &path);
        assert_eq!(request.arguments, [argument]);
    }
}
