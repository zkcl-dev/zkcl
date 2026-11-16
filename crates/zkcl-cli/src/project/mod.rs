pub(crate) use zkcl::project::delivery;
use zkcl::project::{build_script, cache, registries, resolved, workspace};
mod package_exec;
mod publishing;
mod repl_refresh;
mod tooling_commands;
mod work_commands;

pub(super) use repl_refresh::{ActiveProject, RefreshReport};

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use zkcl::Runtime;
use zkcl::project::lookup_entry as symbol;
use zkcl_aot::AotBackendKind;
use zkcl_diagnostics::SourceDocument;
use zkcl_project::manifest::{
    BuildTarget, BuildTargetKind, MANIFEST_NAME, ManifestFile, ModuleName, insert_dependency,
    parse_manifest, remove_dependency,
};

use self::resolved::{ResolvedProject, Update};
use self::tooling_commands::{project_info, registry_account_command};
use self::workspace::{Project, ProjectMetadataFileError, Workspace};

#[derive(Clone, Default)]
struct ProjectArgs {
    manifest: Option<PathBuf>,
    members: Vec<String>,
    workspace: bool,
    bin: Option<String>,
    locked: bool,
    offline: bool,
    all_configurations: bool,
    registries: Vec<PathBuf>,
    features: Option<BTreeSet<String>>,
    arguments: Vec<String>,
    output: Option<PathBuf>,
    emit_dir: Option<PathBuf>,
    backend: Option<AotBackendKind>,
    release: bool,
    target: Option<String>,
    wasm_runtime: Option<PathBuf>,
    split_runtime: bool,
    jobs: Option<NonZeroUsize>,
    dry_run: bool,
    list: bool,
    package: Option<String>,
    alias: Option<String>,
    from: Option<String>,
    system: Option<String>,
    doc: bool,
    json: bool,
    jit: super::JitBackendRequest,
    directories: Vec<PathBuf>,
}

impl ProjectArgs {
    fn find_project(&self) -> Result<Option<Project>, Box<dyn Error>> {
        self.find_project_at(&env::current_dir()?)
    }
    fn find_project_at(&self, directory: &Path) -> Result<Option<Project>, Box<dyn Error>> {
        match &self.manifest {
            Some(path) => Project::open_metadata(&directory.join(path)).map(Some),
            None => Project::discover_metadata(directory),
        }
    }

    fn projects(&self, command: &str) -> Result<Vec<Project>, Box<dyn Error>> {
        self.projects_at(command, &env::current_dir()?)
    }
    fn projects_at(&self, command: &str, directory: &Path) -> Result<Vec<Project>, Box<dyn Error>> {
        let location = self
            .manifest
            .as_ref()
            .map(|path| directory.join(path))
            .unwrap_or_else(|| directory.to_owned());
        let workspace = Workspace::discover(&location)?;
        let at_root = match &workspace {
            Some(workspace) => {
                let selected = location.canonicalize()?;
                selected == workspace.root || selected == workspace.manifest_path
            }
            None => false,
        };
        if self.workspace || !self.members.is_empty() || at_root {
            let workspace = workspace.ok_or("no workspace found for this selection")?;
            let mut selected = Vec::new();
            let mut matched = BTreeSet::new();
            for project in workspace.members {
                let relative = project.root.strip_prefix(&workspace.root)?;
                let mut requested = self.members.is_empty();
                for name in &self.members {
                    if name == project.manifest.name.as_str()
                        || Path::new(name) == relative
                        || Path::new(name) == project.root
                    {
                        matched.insert(name);
                        requested = true;
                    }
                }
                if requested {
                    selected.push(project);
                }
            }
            if let Some(missing) = self.members.iter().find(|name| !matched.contains(name)) {
                return Err(format!("unknown workspace member {missing:?}").into());
            }
            if selected.len() > 1
                && matches!(
                    command,
                    "run" | "repl" | "add" | "remove" | "publish" | "package"
                )
            {
                let members = selected
                    .iter()
                    .map(|project| format!("  --member {}", project.manifest.name))
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(format!(
                    "{command} requires one workspace member. Choose one of:\n{members}"
                )
                .into());
            }
            return Ok(selected);
        }
        Ok(vec![self.find_project_at(directory)?.ok_or(
            "could not find zkcl.sexp in this directory or any parent directory",
        )?])
    }
}

impl From<super::args::ProjectSelection> for ProjectArgs {
    fn from(selection: super::args::ProjectSelection) -> Self {
        Self {
            manifest: selection.manifest_path,
            members: selection.member,
            workspace: selection.workspace,
            ..Self::default()
        }
    }
}
impl From<super::args::ProjectOptions> for ProjectArgs {
    fn from(options: super::args::ProjectOptions) -> Self {
        Self {
            locked: options.locked,
            offline: options.offline,
            registries: options.registry,
            features: (!options.features.is_empty())
                .then(|| options.features.into_iter().collect()),
            ..options.selection.into()
        }
    }
}

pub(super) fn bin_candidates(
    selection: super::args::ProjectSelection,
    directory: &Path,
) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let options: ProjectArgs = selection.into();
    let mut candidates = Vec::new();
    for project in options.projects_at("run", directory)? {
        for target in binary_targets(&project) {
            if let Some(name) = &target.name {
                let help = target
                    .files
                    .first()
                    .map(|file| zkcl_terminal::display_path(file))
                    .unwrap_or_default();
                candidates.push((name.clone(), help));
            }
        }
    }
    Ok(candidates)
}

/// Discover source inputs from the same project model used by execution. No dependency resolution occurs.
pub(super) fn analysis_paths(
    command: &str,
    options: &super::args::ProjectSelection,
    paths: &mut Vec<PathBuf>,
    query: bool,
) -> Result<(), Box<dyn Error>> {
    if query || !paths.is_empty() {
        return Ok(());
    }
    let selection: ProjectArgs = options.clone().into();
    let projects = match selection.projects(command) {
        Ok(projects) => projects,
        Err(_) if !options.explicit() && selection.find_project()?.is_none() => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut seen = BTreeSet::new();
    for project in projects {
        for file in project
            .manifest
            .build
            .iter()
            .map(|build| &build.path)
            .chain(project.authored_target_files())
        {
            let path = project.source_file(file)?;
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
    }
    Ok(())
}

pub(super) fn repl_project(
    request: &super::args::ReplArgs,
    directory: &Path,
) -> Option<ReplProject> {
    if request.no_project || request.system.is_some() {
        return None;
    }
    if !request.project.explicit()
        && request.bin.is_none()
        && !directory
            .ancestors()
            .any(|ancestor| ancestor.join(MANIFEST_NAME).is_file())
    {
        return None;
    }
    let mut options: ProjectArgs = request.project.clone().into();
    options.bin.clone_from(&request.bin);
    for registry in &mut options.registries {
        *registry = directory.join(&*registry);
    }
    Some(ReplProject {
        options,
        directory: directory.to_owned(),
    })
}

pub(super) fn command(request: super::args::Command, heap: Option<usize>) -> ExitCode {
    match execute(request, heap) {
        Ok(status) => status,
        Err(error) => {
            super::report_error("", error.as_ref());
            ExitCode::FAILURE
        }
    }
}

fn execute(request: super::args::Command, heap: Option<usize>) -> Result<ExitCode, Box<dyn Error>> {
    use super::args::Command;
    let (command, options) = match request {
        Command::New(request) => return tooling_commands::new_project(request),
        Command::Work(request) => return work_commands::command(request),
        Command::Registry(request) => return registry_command(request),
        Command::Login(request) => return registry_account_command(true, request),
        Command::Logout(request) => return registry_account_command(false, request),
        Command::Owner(request) => return tooling_commands::owner_command(request),
        Command::Info(request) => (
            "info",
            ProjectArgs {
                json: request.json,
                ..request.project.into()
            },
        ),
        Command::Clean(request) => ("clean", request.into()),
        Command::Lock(request) => (
            "lock",
            ProjectArgs {
                target: request.target,
                ..request.project.into()
            },
        ),
        Command::Update(request) => (
            "update",
            ProjectArgs {
                package: request.package,
                target: request.resolve.target,
                ..request.resolve.project.into()
            },
        ),
        Command::Tree(request) => (
            "tree",
            ProjectArgs {
                json: request.json,
                target: request.resolve.target,
                ..request.resolve.project.into()
            },
        ),
        Command::Fetch(request) => (
            "fetch",
            ProjectArgs {
                all_configurations: request.all_configurations,
                target: request.resolve.target,
                ..request.resolve.project.into()
            },
        ),
        Command::Add(request) => (
            "add",
            ProjectArgs {
                package: Some(request.package),
                alias: request.alias,
                from: request.from,
                system: request.system,
                ..request.project.into()
            },
        ),
        Command::Remove(request) => (
            "remove",
            ProjectArgs {
                package: Some(request.package),
                ..request.project.into()
            },
        ),
        Command::Package(request) => (
            "package",
            ProjectArgs {
                list: request.list,
                output: request.output,
                ..request.project.into()
            },
        ),
        Command::Publish(request) => (
            "publish",
            ProjectArgs {
                dry_run: request.dry_run,
                registries: vec![request.registry],
                ..request.project.into()
            },
        ),
        Command::Build(request) => (
            "build",
            ProjectArgs {
                bin: request.bin,
                output: request.output,
                emit_dir: request.emit_dir,
                backend: request.backend.map(|backend| backend.kind()),
                release: request.release,
                target: request.target,
                wasm_runtime: request.wasm_runtime,
                split_runtime: request.split_runtime,
                jobs: request.jobs,
                ..request.project.into()
            },
        ),
        Command::Run(request) => (
            "run",
            ProjectArgs {
                bin: request.bin,
                list: request.list,
                arguments: super::utf8_arguments(request.arguments)?,
                jit: request.runtime.jit_request(),
                directories: request.runtime.module_dir,
                ..request.project.into()
            },
        ),
        Command::Test(request) => (
            "test",
            ProjectArgs {
                doc: request.doc,
                ..request.project.into()
            },
        ),
        Command::X(request) => {
            return package_exec::execute(
                ProjectArgs {
                    package: Some(request.package),
                    bin: request.bin,
                    offline: request.offline,
                    registries: request.registry,
                    from: request.from,
                    arguments: super::utf8_arguments(request.arguments)?,
                    ..ProjectArgs::default()
                },
                heap,
            );
        }
        _ => unreachable!("main dispatch routes only project operations here"),
    };
    let projects = options.projects(command)?;
    if projects.len() > 1 && options.output.is_some() {
        return Err("--output requires a single project target".into());
    }
    if projects.len() > 1 && command == "info" && options.json {
        let metadata = projects
            .iter()
            .map(tooling_commands::project_metadata)
            .collect::<Result<Vec<_>, _>>()?;
        println!("{}", serde_json::to_string_pretty(&metadata)?);
        return Ok(ExitCode::SUCCESS);
    }
    if projects.len() > 1 && command == "tree" && options.json {
        let mut trees = Vec::new();
        for project in projects {
            let resolved = ResolvedProject::resolve_for_target(
                project,
                options.locked || options.offline,
                options.offline,
                &options.registries,
                options.features.clone(),
                Update::Keep,
                options.target.as_deref(),
            )?;
            trees.push(serde_json::json!({
                "project": resolved.project.manifest.name.as_str(),
                "target": resolved.target(),
                "modules": dependency_metadata(&resolved),
            }));
        }
        println!("{}", serde_json::to_string_pretty(&trees)?);
        return Ok(ExitCode::SUCCESS);
    }
    let multiple_projects = projects.len() > 1;
    if multiple_projects && command == "fetch" && options.all_configurations {
        return execute_project(
            command,
            projects
                .into_iter()
                .next()
                .expect("selected workspace member"),
            options,
            heap,
        );
    }
    let mut status = ExitCode::SUCCESS;
    for project in projects {
        let mut options = options.clone();
        if multiple_projects {
            options.emit_dir = options
                .emit_dir
                .map(|directory| directory.join(project.manifest.name.as_str()));
        }
        let module =
            (multiple_projects && command == "test").then(|| project.manifest.name.clone());
        match execute_project(command, project, options, heap) {
            Ok(result) if result == ExitCode::SUCCESS => {}
            Ok(result) => status = result,
            Err(error) => {
                if let Some(module) = module {
                    super::report_error(&format!("zkcl test {module}: "), error.as_ref());
                    status = ExitCode::FAILURE;
                } else {
                    return Err(error);
                }
            }
        }
    }
    Ok(status)
}

fn execute_project(
    command: &str,
    mut project: Project,
    mut options: ProjectArgs,
    heap: Option<usize>,
) -> Result<ExitCode, Box<dyn Error>> {
    let _manifest_lock = if matches!(command, "add" | "remove") {
        Some(work_commands::manifest_lock(&project.lock_root()?)?)
    } else {
        None
    };
    if command == "clean" {
        clean_build_outputs(&project.root)?;
        return Ok(ExitCode::SUCCESS);
    }
    if command == "info" {
        return project_info(&project, options.json);
    }
    if command == "package" {
        let output = options.output.unwrap_or_else(|| {
            project.root.join(format!(
                "{}-{}.tar",
                project.manifest.name.as_str().replace('/', "-"),
                project.manifest.version
            ))
        });
        publishing::package(
            &project,
            (!options.list).then_some(output.as_path()),
            options.list,
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    if command == "publish" {
        let [registry] = options.registries.as_slice() else {
            return Err("publish requires --registry PATH or --registry HTTPS_URL".into());
        };
        if let Some(url) = registry
            .to_str()
            .filter(|value| value.starts_with("https://") || value.starts_with("http://"))
        {
            publishing::publish(&project, url, options.dry_run)?;
        } else if options.dry_run {
            publishing::package(&project, None, false)?;
        } else {
            // Publication uses declared immutable sources, including workspace
            // member releases, rather than substituting local development paths.
            let digest = registries::publish(&project, registry)?;
            println!(
                "Published {} {} ({digest})",
                project.manifest.name, project.manifest.version
            );
        }
        return Ok(ExitCode::SUCCESS);
    }
    if command == "run" && options.list {
        let candidates = binary_targets(&project)
            .map(|target| TargetCandidate::new(&project, target))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            println!("No runnable targets in {}", project.manifest.name);
        } else {
            print!("{}", target_listing(&candidates));
        }
        return Ok(ExitCode::SUCCESS);
    }
    let selected = if command == "run" || (command == "build" && options.bin.is_some()) {
        Some(select_binary(&project, options.bin.as_deref(), command)?.clone())
    } else {
        None
    };
    let features = options.features.take();
    if matches!(command, "add" | "remove" | "update") && (options.locked || options.offline) {
        return Err(format!(
            "{command} needs to resolve new dependencies and cannot use --locked or --offline"
        )
        .into());
    }
    if command == "fetch" && options.all_configurations {
        if features.is_some() {
            return Err("--all-configurations and --features select different fetch modes".into());
        }
        ResolvedProject::fetch_lockfile_configurations(
            &project,
            &options.registries,
            options.offline,
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    if matches!(command, "add" | "remove") {
        let package = options
            .package
            .as_deref()
            .expect("dependency edit has an operand");
        let path = &project.manifest_path;
        let source = fs::read_to_string(path).map_err(|source| ProjectMetadataFileError::Read {
            path: path.clone(),
            source,
        })?;
        let (edit, message) = if command == "add" {
            let (name, requirement) = package.split_once('@').unwrap_or((package, "*"));
            let module: ModuleName = name.parse()?;
            let alias = options
                .alias
                .as_deref()
                .unwrap_or_else(|| name.split_once('/').expect("module name validated").1);
            let edit = insert_dependency(
                &source,
                alias,
                &module,
                requirement,
                options.from.as_deref(),
                options.system.as_deref(),
            );
            (edit, format!("Added {module}"))
        } else {
            (
                remove_dependency(&source, package),
                format!("Removed {package}"),
            )
        };
        let parse_error = |text: &str, source| ProjectMetadataFileError::Parse {
            document: SourceDocument::file(path.clone(), Some(text.to_owned())),
            source,
        };
        let edited = edit.map_err(|error| parse_error(&source, error))?;
        let ManifestFile::Project(manifest) =
            parse_manifest(&edited).map_err(|error| parse_error(&edited, error))?
        else {
            unreachable!("dependency edit produces a project")
        };
        project.manifest = manifest;
        let resolved = ResolvedProject::resolve_for_target(
            project,
            options.locked || options.offline,
            options.offline,
            &options.registries,
            features,
            Update::Keep,
            options.target.as_deref(),
        )?;
        resolved.write_manifest_edit(&source, &edited)?;
        println!("{message}");
        return Ok(ExitCode::SUCCESS);
    }
    let upgrade = if command == "update" {
        options.package.as_deref().map(str::parse).transpose()?
    } else {
        None
    };
    let update = match (command, upgrade.as_ref()) {
        ("update", Some(name)) => Update::One(name),
        ("update", None) => Update::All,
        _ => Update::Keep,
    };
    let resolved = ResolvedProject::resolve_for_target(
        project,
        options.locked || options.offline || command == "fetch",
        options.offline,
        &options.registries,
        features,
        update,
        options.target.as_deref(),
    )?
    .with_build_options(build_options()?);
    match command {
        "lock" | "update" => {
            resolved.write_lock()?;
            println!("Locked {}", resolved.project.manifest.name);
        }
        "tree" => {
            if options.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&dependency_metadata(&resolved))?
                );
            } else {
                for id in &resolved.resolution.order {
                    let module = &resolved.resolution.modules[id];
                    println!("{id} {}", module.release.id);
                }
            }
        }
        "fetch" => resolved.fetch()?,
        "test" if options.doc => {
            let temp = project_output(&resolved)?;
            let runtime = super::standalone_runtime(heap)?;
            resolved.fetch()?;
            resolved.load_libraries(&runtime, temp.path())?;
            let generated = build_script::output_dir(temp.path());
            let mut files = Vec::new();
            for target in resolved
                .project
                .manifest
                .targets
                .iter()
                .filter(|target| target.kind == BuildTargetKind::Library)
            {
                for file in &target.files {
                    files.push(resolved.project.compilation_source(file, &generated)?);
                }
            }
            return super::testing::test_project_documentation(
                &runtime,
                &resolved.project.manifest.name.to_string(),
                &resolved.project.root,
                &files,
            );
        }
        "build" | "run" | "test" => {
            return execute_target(&resolved, command, options, selected.as_ref(), heap);
        }
        _ => unreachable!(),
    }
    Ok(ExitCode::SUCCESS)
}

fn clean_build_outputs(root: &Path) -> std::io::Result<()> {
    let directory = root.join("target/zkcl");
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        // Active project sessions own their temporary directories. Clean only
        // directories used for published build outputs.
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.parse::<zkcl::CompilationTarget>().is_ok())
        {
            continue;
        }
        fs::remove_dir_all(entry.path())?;
    }
    Ok(())
}

fn dependency_metadata(resolved: &ResolvedProject) -> Vec<serde_json::Value> {
    resolved
        .resolution
        .order
        .iter()
        .map(|id| {
            let module = &resolved.resolution.modules[id];
            serde_json::json!({
                "module": id.name.as_str(),
                "source": id.source.to_string(),
                "release": module.release.id.to_string(),
                "features": module.features,
                "dependencies": module.dependencies.iter()
                    .map(|dependency| dependency.module.to_string()).collect::<Vec<_>>(),
            })
        })
        .collect()
}

fn registry_command(request: super::args::RegistryCommand) -> Result<ExitCode, Box<dyn Error>> {
    use super::args::RegistryCommand;
    match request {
        RegistryCommand::Init { identity, path } => {
            let registry = zkcl_registry::FileRegistry::init(path, &identity)?;
            println!("Initialized registry {}", registry.id());
        }
        RegistryCommand::Yank(request) => registry_yank(request, true)?,
        RegistryCommand::Unyank(request) => registry_yank(request, false)?,
        RegistryCommand::ExportSparse(request) => export_sparse_registry(request)?,
    }
    Ok(ExitCode::SUCCESS)
}
fn registry_yank(
    request: super::args::RegistryRelease,
    yanked: bool,
) -> Result<(), Box<dyn Error>> {
    let (name, version) = request
        .package
        .split_once('@')
        .ok_or("expected namespace/name@version")?;
    let module: ModuleName = name.parse()?;
    if let Some(url) = request
        .registry
        .to_str()
        .filter(|value| value.starts_with("https://") || value.starts_with("http://"))
    {
        publishing::control(
            url,
            zkcl_registry::remote::Control::Yank {
                module: module.to_string(),
                version: version.to_owned(),
                yanked,
            },
        )?;
        println!("Updated {module}@{version}");
        return Ok(());
    }
    let registry = zkcl_registry::FileRegistry::open(request.registry)?;
    let record = registry.set_yanked(module.as_str(), version, yanked)?;
    let operation = if yanked { "Yanked" } else { "Restored" };
    println!(
        "{operation} {module} {} in {}",
        record.version,
        registry.id()
    );
    Ok(())
}
fn export_sparse_registry(request: super::args::SparseExportArgs) -> Result<(), Box<dyn Error>> {
    use zkcl_registry::sparse_export::{SparseSigningOptions, export_signed_sparse, export_sparse};
    let registry = zkcl_registry::FileRegistry::open(request.registry)?;
    let report = if let Some(trusted_root) = request.trusted_root {
        let options = SparseSigningOptions {
            trusted_root,
            signing_keys: request.key,
            version: request.version.expect("clap requires signing version"),
            expires: request.expires.expect("clap requires signing expiration"),
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(export_signed_sparse(&registry, &request.output, options))?
    } else {
        export_sparse(&registry, &request.output)?
    };
    println!(
        "Exported {} modules, {} shards, and {} source archives to {}",
        report.modules,
        report.shards,
        report.sources,
        request.output.display()
    );
    Ok(())
}

fn execute_target(
    resolved: &ResolvedProject,
    command: &str,
    options: ProjectArgs,
    selected: Option<&BuildTarget>,
    heap: Option<usize>,
) -> Result<ExitCode, Box<dyn Error>> {
    let kind = if command == "test" {
        BuildTargetKind::Test
    } else {
        BuildTargetKind::Binary
    };
    resolved.fetch()?;
    if let Some(target) = selected {
        return execute_resolved_target(
            resolved,
            &resolved.project,
            target,
            command,
            options,
            heap,
        );
    }
    let targets: Vec<_> = resolved
        .project
        .manifest
        .targets
        .iter()
        .filter(|target| target.kind == kind)
        .collect();
    if targets.is_empty() {
        if command == "build" {
            if options.output.is_some() {
                return Err(
                    "--output requires a binary target; use --emit-dir for library output".into(),
                );
            }
            let output = project_output(resolved)?;
            let runtime = target_runtime(heap, resolved.target())?;
            let delivery = resolved.load_libraries(&runtime, output.path())?;
            let target: zkcl::CompilationTarget = resolved.target().parse()?;
            let destination = options
                .emit_dir
                .unwrap_or_else(|| build_output_dir(resolved, options.release).join("lib"));
            if target.is_javascript() || resolved.target() == "wasm32-unknown-unknown" {
                if options.backend.is_some() || options.jobs.is_some() {
                    return Err("this library target produces a project bundle; native backend options require a machine object target".into());
                }
                fs::create_dir_all(&destination)?;
                let stage = tempfile::Builder::new()
                    .prefix("bundle-")
                    .tempdir_in(destination)?;
                let bundle = stage.path().join("bundle");
                zkcl::project::ProjectBundle::write(&delivery, &bundle, resolved.target(), None)?;
                let directory = stage.keep();
                println!("{}", directory.join("bundle").display());
                return Ok(ExitCode::SUCCESS);
            }
            let triple = target
                .machine()
                .expect("native library target")
                .triple()
                .clone();
            let mut config = super::build::native_aot_config(
                resolved.project.manifest.name.as_str(),
                options.backend,
                options.release,
            )
            .with_target(triple);
            config.jobs = options.jobs;
            fs::create_dir_all(&destination)?;
            let build = tempfile::Builder::new()
                .prefix("library-")
                .tempdir_in(destination.canonicalize()?)?;
            let staged = build.path().join(".stage");
            let library = build.path().join("library");
            delivery::emit_aot_units(&delivery.units, &config, &staged)?;
            fs::rename(staged, &library)?;
            let _ = build.keep();
            println!("{}", library.display());
        }
        return Ok(ExitCode::SUCCESS);
    }
    if targets.len() > 1 && options.output.is_some() {
        return Err("--output requires --bin when a project has multiple binaries".into());
    }
    let multiple_targets = targets.len() > 1;
    let mut status = ExitCode::SUCCESS;
    for target in targets {
        let mut options = options.clone();
        if multiple_targets {
            options.emit_dir = options
                .emit_dir
                .map(|directory| directory.join(target.name.as_deref().unwrap_or("test")));
        }
        match execute_resolved_target(resolved, &resolved.project, target, command, options, heap) {
            Ok(result) if result == ExitCode::SUCCESS => {}
            Ok(result) => status = result,
            Err(error) if command == "test" && multiple_targets => {
                super::report_error(
                    &format!("zkcl test {}: ", resolved.project.manifest.name),
                    error.as_ref(),
                );
                status = ExitCode::FAILURE;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(status)
}

fn execute_resolved_target(
    resolved: &ResolvedProject,
    project: &Project,
    target: &BuildTarget,
    command: &str,
    options: ProjectArgs,
    heap: Option<usize>,
) -> Result<ExitCode, Box<dyn Error>> {
    let temp_root = resolved.project.root.join("target/zkcl");
    fs::create_dir_all(&temp_root)?;
    let output = tempfile::Builder::new()
        .prefix("compile-")
        .tempdir_in(temp_root)?;
    let runtime = target_runtime(heap, resolved.target())?;
    if (options.jit.required()
        || runtime.compilation_target() == &zkcl::CompilationTarget::native())
        && let Some(jit) = options.jit.create_backend()?
    {
        runtime.set_jit_threshold(super::CLI_JIT_THRESHOLD);
        runtime.set_jit_backend(jit);
    }
    super::add_module_directories(&runtime, &options.directories)?;
    runtime.set_command_line_arguments(options.arguments);
    if target.kind == BuildTargetKind::Test {
        runtime.eval("(require :zk-test)")?;
        runtime.eval("(setf zk-test:*report-hook* #'zk-test:print-report)")?;
    }
    let delivery = resolved.load_target(&runtime, output.path(), project, Some(target))?;
    if command == "build" {
        let output_path = if options.emit_dir.is_none() && options.output.is_none() {
            let directory = build_output_dir(resolved, options.release);
            fs::create_dir_all(&directory)?;
            let extension = if resolved.target() == "wasm32-unknown-unknown" {
                ".wasm"
            } else if runtime.compilation_target().is_javascript() {
                ".mjs"
            } else if resolved.target().contains("windows") {
                ".exe"
            } else {
                ""
            };
            Some(directory.join(format!(
                "{}{extension}",
                target.name.as_deref().expect("named binary target")
            )))
        } else {
            options.output
        };
        super::build::build_project(
            target.name.as_deref().expect("named binary target"),
            target.entry.as_deref().expect("binary entry"),
            &delivery,
            super::build::ProjectBuildOptions {
                output: output_path,
                emit_dir: options.emit_dir,
                backend: options.backend,
                release: options.release,
                target: options.target,
                wasm_runtime: options.wasm_runtime,
                split_runtime: options.split_runtime,
                jobs: options.jobs,
                link_libraries: delivery.link_libraries.clone(),
                link_search: delivery.link_search.clone(),
            },
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    match target.kind {
        BuildTargetKind::Binary => {
            let entry = target
                .entry
                .as_deref()
                .ok_or("binary target needs an entry")?;
            runtime.call(&symbol(&runtime, entry)?, &[])?;
        }
        BuildTargetKind::Test => {
            let suite = target.suite.as_deref().ok_or("test target needs a suite")?;
            let suite = symbol(&runtime, suite)?;
            runtime.call(&symbol(&runtime, "ZK-TEST:CHECK")?, &[&suite])?;
        }
        BuildTargetKind::Library => unreachable!(),
    }
    Ok(ExitCode::from(runtime.requested_exit_code()))
}

fn target_runtime(heap: Option<usize>, target: &str) -> Result<Runtime, Box<dyn Error>> {
    Ok(super::start_standalone(
        super::standalone_builder(heap).compilation_target(target.parse()?),
    )?)
}

fn build_output_dir(resolved: &ResolvedProject, release: bool) -> PathBuf {
    resolved
        .project
        .root
        .join("target/zkcl")
        .join(resolved.target())
        .join(if release { "release" } else { "debug" })
}

fn binary_targets(project: &Project) -> impl Iterator<Item = &BuildTarget> {
    project
        .manifest
        .targets
        .iter()
        .filter(|target| target.kind == BuildTargetKind::Binary)
}

fn target_listing(candidates: &[TargetCandidate]) -> String {
    use std::fmt::Write as _;
    let mut listing = String::new();
    let width = candidates
        .iter()
        .map(|target| target.name.chars().count())
        .max()
        .unwrap_or(0);
    for target in candidates {
        let TargetCandidate {
            name,
            source,
            other_files,
            ..
        } = target;
        let _ = write!(listing, "  {name:width$}");
        if let Some(source) = source {
            let _ = write!(listing, "  {}", zkcl_terminal::display_path(source));
        }
        match other_files {
            0 => {}
            1 => listing.push_str(" (+1 file)"),
            more => {
                let _ = write!(listing, " (+{more} files)");
            }
        }
        if let Some(entry) = &target.entry {
            let _ = write!(listing, "  entry {entry:?}");
        }
        listing.push('\n');
    }
    listing
}

fn nearest_target<'a>(requested: &str, names: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    fn distance(left: &str, right: &str) -> usize {
        let right: Vec<_> = right.chars().collect();
        let mut row: Vec<_> = (0..=right.len()).collect();
        for (i, left) in left.chars().enumerate() {
            let mut diagonal = row[0];
            row[0] = i + 1;
            for (j, right) in right.iter().enumerate() {
                let previous = row[j + 1];
                row[j + 1] = (row[j] + 1)
                    .min(previous + 1)
                    .min(diagonal + usize::from(left != *right));
                diagonal = previous;
            }
        }
        row[right.len()]
    }
    let mut ranked: Vec<_> = names
        .map(|name| (distance(requested, name), name))
        .filter(|(distance, _)| *distance <= 2)
        .collect();
    ranked.sort_unstable();
    let first = ranked.first()?;
    if ranked.get(1).is_some_and(|second| second.0 == first.0) {
        None
    } else {
        Some(first.1)
    }
}

#[derive(Debug)]
enum TargetSelectionProblem {
    Unknown(String),
    Missing,
    Ambiguous,
}

#[derive(Debug)]
struct TargetCandidate {
    name: String,
    source: Option<PathBuf>,
    other_files: usize,
    entry: Option<String>,
}

impl TargetCandidate {
    fn new(project: &Project, target: &BuildTarget) -> Self {
        Self {
            name: target.name.clone().expect("binary targets are named"),
            source: target.files.first().map(|file| project.root.join(file)),
            other_files: target.files.len().saturating_sub(1),
            entry: target.entry.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct TargetSelectionError {
    command: String,
    problem: TargetSelectionProblem,
    candidates: Vec<TargetCandidate>,
    manifest: PathBuf,
    suggested: Option<String>,
}
impl std::fmt::Display for TargetSelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.problem {
            TargetSelectionProblem::Unknown(name) => {
                write!(formatter, "unknown binary target {name:?}")
            }
            TargetSelectionProblem::Missing => {
                formatter.write_str("project has no runnable targets")
            }
            TargetSelectionProblem::Ambiguous => {
                formatter.write_str("this project has several runnable targets")
            }
        }
    }
}
impl Error for TargetSelectionError {}
impl TargetSelectionError {
    pub(crate) fn to_diagnostic(&self) -> zkcl_diagnostics::Diagnostic {
        use zkcl_diagnostics::{Diagnostic, DiagnosticContext, Note, NoteKind, Severity};
        let mut diagnostic = Diagnostic::new(Severity::Error, self.to_string());
        diagnostic.code = Some(
            match self.problem {
                TargetSelectionProblem::Unknown(_) => "project/unknown-target",
                TargetSelectionProblem::Missing => "project/missing-target",
                TargetSelectionProblem::Ambiguous => "project/ambiguous-target",
            }
            .into(),
        );
        if self.candidates.is_empty() {
            diagnostic.contexts.push(DiagnosticContext {
                message: "project manifest".into(),
                path: Some(self.manifest.clone()),
            });
            diagnostic.notes.push(Note {
                kind: NoteKind::Help,
                message: "declare a binary target with source files and an entry function".into(),
            });
        } else {
            diagnostic.notes.push(Note {
                kind: NoteKind::Note,
                message: format!(
                    "Available targets:\n{}",
                    target_listing(&self.candidates).trim_end()
                ),
            });
            diagnostic.notes.push(Note {
                kind: NoteKind::Help,
                message: format!(
                    "choose a target, for example `zkcl {} --bin {}`",
                    self.command, self.candidates[0].name
                ),
            });
        }
        if let Some(suggested) = &self.suggested {
            diagnostic.notes.push(Note {
                kind: NoteKind::Help,
                message: format!("did you mean `--bin {suggested}`?"),
            });
        }
        diagnostic
    }
}

fn select_binary<'a>(
    project: &'a Project,
    name: Option<&str>,
    command: &str,
) -> Result<&'a BuildTarget, Box<dyn Error>> {
    let mut matched = binary_targets(project)
        .filter(|target| name.is_none_or(|name| target.name.as_deref() == Some(name)));
    if let Some(target) = matched.next()
        && matched.next().is_none()
    {
        return Ok(target);
    }
    let suggested = name
        .and_then(|name| {
            nearest_target(
                name,
                binary_targets(project).filter_map(|target| target.name.as_deref()),
            )
        })
        .map(str::to_owned);
    let candidates: Vec<_> = binary_targets(project)
        .map(|target| TargetCandidate::new(project, target))
        .collect();
    let problem = if let Some(name) = name {
        TargetSelectionProblem::Unknown(name.to_owned())
    } else if candidates.is_empty() {
        TargetSelectionProblem::Missing
    } else {
        TargetSelectionProblem::Ambiguous
    };
    Err(Box::new(TargetSelectionError {
        command: command.to_owned(),
        problem,
        candidates,
        manifest: project.manifest_path.clone(),
        suggested,
    }))
}

fn project_output(resolved: &ResolvedProject) -> Result<tempfile::TempDir, Box<dyn Error>> {
    let root = resolved.project.root.join("target/zkcl");
    fs::create_dir_all(&root)?;
    Ok(tempfile::Builder::new()
        .prefix("session-")
        .tempdir_in(root)?)
}

pub(super) struct ReplProject {
    options: ProjectArgs,
    directory: PathBuf,
}

impl ReplProject {
    pub(super) fn resolve(&self) -> Result<ResolvedRepl, Box<dyn Error>> {
        let options = &self.options;
        let project = options
            .projects_at("repl", &self.directory)?
            .into_iter()
            .next()
            .ok_or("workspace has no selected member")?;
        let has_library = project
            .manifest
            .targets
            .iter()
            .any(|target| target.kind == BuildTargetKind::Library);
        let target = if options.bin.is_some()
            || (!has_library && binary_targets(&project).next().is_some())
        {
            Some(select_binary(&project, options.bin.as_deref(), "repl")?.clone())
        } else {
            None
        };
        let resolved = ResolvedProject::resolve(
            project,
            options.locked || options.offline,
            options.offline,
            &options.registries,
            options.features.clone(),
            Update::Keep,
        )?
        .with_build_options(build_options()?);
        resolved.fetch()?;
        Ok(ResolvedRepl { resolved, target })
    }
}

pub(super) struct ResolvedRepl {
    resolved: ResolvedProject,
    target: Option<BuildTarget>,
}

impl ResolvedRepl {
    pub(super) fn description(&self) -> String {
        let project = &self.resolved.project;
        let target = if let Some(name) = self.target.as_ref().and_then(|target| target.name.as_deref()) {
            format!("bin {name}")
        } else if project.manifest.targets.iter().any(|target| target.kind == BuildTargetKind::Library) {
            "library".to_owned()
        } else {
            "dependencies".to_owned()
        };
        format!(
            "{} ({target}) · {}",
            project.manifest.name,
            project.manifest_path.display(),
        )
    }

    pub(super) fn load(&self, runtime: &Runtime) -> Result<ActiveProject, Box<dyn Error>> {
        let output = project_output(&self.resolved)?;
        ActiveProject::load(
            &self.resolved,
            runtime,
            output,
            self.target.as_ref(),
        )
    }
}

fn build_options() -> Result<build_script::BuildOptions, Box<dyn Error>> {
    Ok(build_script::BuildOptions {
        program: Some(std::env::current_exe()?),
        runtime: super::standalone_builder(None),
        ..Default::default()
    })
}

#[cfg(test)]
mod command_tests {
    use super::*;

    fn project(directory: &Path) -> Project {
        fs::write(
            directory.join("zkcl.sexp"),
            r#"(project "test/targets" (version "0.1.0")
            (dependency "missing" (module "test/missing") (version "1.0.0"))
            (binary "first" (files "first.lisp") (entry "DEMO:MAIN"))
            (binary "second" (files "second.lisp" "helper.lisp") (entry "DEMO:SECOND")))"#,
        )
        .unwrap();
        Project::open_metadata(&directory.join("zkcl.sexp")).unwrap()
    }

    #[test]
    fn target_query_and_failure_use_the_same_candidates_without_sources_or_lock() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.agents/tmp/zkcl/diagnostics-rework/commands");
        fs::create_dir_all(&root).unwrap();
        let directory = tempfile::tempdir_in(root).unwrap();
        let project = project(directory.path());
        let candidates: Vec<_> = binary_targets(&project)
            .map(|target| TargetCandidate::new(&project, target))
            .collect();
        let listing = target_listing(&candidates);
        assert!(listing.contains("first.lisp"));
        assert!(listing.contains("second.lisp (+1 file)"));
        let error = select_binary(&project, None, "run").unwrap_err();
        assert!(!error.to_string().contains('\n'));
        let diagnostic = error
            .downcast_ref::<TargetSelectionError>()
            .unwrap()
            .to_diagnostic();
        assert_eq!(diagnostic.code.as_deref(), Some("project/ambiguous-target"));
        assert!(
            diagnostic
                .notes
                .iter()
                .any(|note| note.message.contains(listing.trim_end()))
        );
        assert!(
            diagnostic
                .notes
                .iter()
                .any(|note| note.message.contains("zkcl run --bin first"))
        );
        let error = select_binary(&project, Some("missing"), "run").unwrap_err();
        let diagnostic = error
            .downcast_ref::<TargetSelectionError>()
            .unwrap()
            .to_diagnostic();
        assert_eq!(diagnostic.code.as_deref(), Some("project/unknown-target"));
        let typo = select_binary(&project, Some("firs"), "run").unwrap_err();
        let diagnostic = typo
            .downcast_ref::<TargetSelectionError>()
            .unwrap()
            .to_diagnostic();
        assert!(
            diagnostic
                .notes
                .iter()
                .any(|note| note.message.contains("did you mean `--bin first`"))
        );
        let candidates = bin_candidates(
            super::super::args::ProjectSelection::default(),
            directory.path(),
        )
        .unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        let status = execute_project(
            "run",
            project,
            ProjectArgs {
                list: true,
                ..ProjectArgs::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(status, ExitCode::SUCCESS);
        assert!(!directory.path().join("zkcl.lock").exists());
        assert!(!directory.path().join("target").exists());
    }
}
