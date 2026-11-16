#[path = "native_sdk.rs"]
mod native_sdk;

use std::env;
use std::fs;
use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use sha2::{Digest, Sha256};
use target_lexicon::{Environment, OperatingSystem, Triple};
use thiserror::Error;
use zkcl_aot::{
    AotBackendKind, AotError, AotObject, AotOptLevel, AotTargetConfig,
    compile_fasl_bytes_to_object, compile_fasl_to_object,
};

use super::project::delivery::{
    AotUnit, Delivery, StartupStep, emit_aot_units, stage_delivery_assets, validate_delivery,
};

#[derive(Debug, PartialEq, Eq)]
struct BuildArgs {
    input_path: PathBuf,
    output_path: Option<PathBuf>,
    backend: Option<AotBackendKind>,
    jobs: Option<NonZeroUsize>,
    target: Option<(zkcl::CompilationTarget, String)>,
    wasm_runtime: Option<PathBuf>,
    split_runtime: bool,
    output_kind: OutputKind,
    emit_dir: Option<PathBuf>,
    module_id: Option<String>,
    list_entries: bool,
    release: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputKind {
    Executable,
    StaticLibrary,
}

#[derive(Debug, Error)]
pub(crate) enum BuildError {
    #[error("{0}")]
    NativeSdk(String),
    #[error("failed to {action} {path}: {source}", path = .path.display())]
    PathIo {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to start runtime: {0}")]
    RuntimeStart(#[from] zkcl::AlreadyStarted),
    #[error("Lisp compilation failed: {0}")]
    CompileLisp(#[from] zkcl::EvalError),
    #[error(transparent)]
    LinkWasm(#[from] zkcl_wasm::codegen::LinkError),
    #[error("{0}")]
    WasmOptions(&'static str),
    #[error("{0}")]
    JavaScriptOptions(&'static str),
    #[error("unexpected contents at JavaScript runtime path {}", .0.display())]
    JavaScriptRuntimeCollision(PathBuf),
    #[error(transparent)]
    JavaScript(#[from] zkcl_js::EmitError),
    #[error(transparent)]
    CompiledFile(#[from] zkcl_bytecode::CompiledFileError),
    #[error(transparent)]
    Target(#[from] zkcl::TargetError),
    #[error("AOT compilation failed for {path}: {source}", path = .path.display())]
    CompileAot {
        path: PathBuf,
        #[source]
        source: AotError,
    },
    #[error("zkcl crate path is not UTF-8: {}", .0.display())]
    NonUtf8CratePath(PathBuf),
    #[error("failed to copy Cargo.lock: {0}")]
    CopyLockfile(#[source] io::Error),
    #[error("failed to invoke cargo build: {0}")]
    InvokeCargo(#[source] io::Error),
    #[error("Cargo link failed ({status})")]
    Link {
        status: std::process::ExitStatus,
        stdout: String,
        stderr: String,
    },
    #[error("failed to copy linked file {linked} to {output}: {source}", linked = .linked.display(), output = .output.display())]
    CopyOutput {
        linked: PathBuf,
        output: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl BuildError {
    fn path_io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::PathIo {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

pub(crate) fn run(request: super::args::BuildArgs, heap_size: Option<usize>) -> ExitCode {
    let opts = BuildArgs::from_request(request);
    if let Err(error) = execute_build(&opts, heap_size) {
        super::report_error("zkcl build: ", &error);
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

impl BuildArgs {
    fn from_request(request: super::args::BuildArgs) -> Self {
        Self {
            input_path: request.path.expect("file build selected after parsing"),
            output_path: request.output,
            backend: request.backend.map(|backend| backend.kind()),
            jobs: request.jobs,
            target: request.target.map(|name| {
                (
                    name.parse().expect("clap validated compilation target"),
                    name,
                )
            }),
            wasm_runtime: request.wasm_runtime,
            split_runtime: request.split_runtime,
            output_kind: if request.staticlib {
                OutputKind::StaticLibrary
            } else {
                OutputKind::Executable
            },
            emit_dir: request.emit_dir,
            module_id: request.module_id,
            list_entries: request.list_entries,
            release: request.release,
        }
    }
}

/// Link an ordered project delivery as one native executable and its FASL assets.
fn copy_component_fasls(components: &[AotUnit], destination: &Path) -> Result<(), io::Error> {
    for (index, component) in components.iter().enumerate() {
        fs::copy(
            &component.fasl,
            destination.join(format!("{index:04}.zkclfasl")),
        )?;
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct ProjectBuildOptions {
    pub output: Option<PathBuf>,
    pub emit_dir: Option<PathBuf>,
    pub backend: Option<AotBackendKind>,
    pub release: bool,
    pub target: Option<String>,
    pub jobs: Option<NonZeroUsize>,
    pub wasm_runtime: Option<PathBuf>,
    pub split_runtime: bool,
    pub link_libraries: Vec<String>,
    pub link_search: Vec<PathBuf>,
}

pub(crate) fn build_project(
    input_stem: &str,
    entry: &str,
    delivery: &Delivery,
    options: ProjectBuildOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let ProjectBuildOptions {
        output,
        emit_dir,
        backend,
        release,
        target,
        jobs,
        link_libraries,
        link_search,
        wasm_runtime,
        split_runtime,
    } = options;
    let target = target
        .map(|name| {
            name.parse::<zkcl::CompilationTarget>()
                .map(|target| (target, name))
        })
        .transpose()?;
    let opts = BuildArgs {
        input_path: PathBuf::from(input_stem),
        output_path: output,
        backend,
        jobs,
        target,
        wasm_runtime,
        split_runtime,
        output_kind: OutputKind::Executable,
        emit_dir,
        module_id: None,
        list_entries: false,
        release,
    };
    if opts
        .target
        .as_ref()
        .is_some_and(|(target, _)| target.is_javascript())
    {
        if !link_libraries.is_empty() || !link_search.is_empty() {
            return Err("JavaScript project delivery cannot link native host libraries".into());
        }
        return build_javascript_project(&opts, input_stem, entry, delivery);
    }
    if opts.split_runtime {
        return Err(BuildError::JavaScriptOptions("--split-runtime requires --target js").into());
    }
    if opts.target.as_ref().is_some_and(|(target, _)| {
        target.machine().is_some_and(|machine| {
            machine.triple().architecture == target_lexicon::Architecture::Wasm32
        })
    }) {
        if !link_libraries.is_empty() || !link_search.is_empty() {
            return Err("Wasm project delivery cannot link native host libraries".into());
        }
        return build_wasm_project(&opts, input_stem, entry, delivery);
    }
    if opts.wasm_runtime.is_some() {
        return Err("--wasm-runtime requires --target wasm32-unknown-unknown".into());
    }
    let temporary_project;
    let dir = if let Some(dir) = &opts.emit_dir {
        dir.as_path()
    } else {
        temporary_project = temporary_build_directory(&env::temp_dir(), input_stem)?;
        temporary_project.path()
    };
    fs::create_dir_all(dir).map_err(|source| BuildError::path_io("create", dir, source))?;
    let _emit_lock = if opts.emit_dir.is_some() {
        Some(lock_file(&dir.join(".zkcl-emit.lock"))?)
    } else {
        None
    };
    let link_search = if opts.emit_dir.is_some() {
        stage_link_search(&link_search, dir)?
    } else {
        link_search
    };
    let mut config = native_aot_config("project", backend, release);
    if let Some((target, _)) = &opts.target {
        config = config.with_target(
            target
                .machine()
                .expect("native build target")
                .triple()
                .clone(),
        );
    }
    config.jobs = opts.jobs;
    let native = emit_aot_units(&delivery.units, &config, &dir.join("modules"))?;
    let target = opts
        .target
        .as_ref()
        .map(|(target, _)| {
            target
                .machine()
                .expect("native build target")
                .triple()
                .clone()
        })
        .unwrap_or_else(Triple::host);
    let sdk = native_sdk::Sdk::locate(&target)?;
    let mut project = GeneratedCargoProject::new(dir, input_stem, OutputKind::Executable);
    let generated_root = canonicalize(dir)?;
    project.object_filenames = native
        .iter()
        .map(|component| {
            component
                .object
                .strip_prefix(&generated_root)
                .map(Path::to_path_buf)
        })
        .collect::<Result<_, _>>()?;
    let link = opts.output_path.is_some() || opts.emit_dir.is_none();
    let executable = if link {
        opts.output_path.clone().unwrap_or_else(|| {
            PathBuf::from(OutputKind::Executable.output_filename(input_stem, &target))
        })
    } else {
        dir.join(OutputKind::Executable.output_filename(&project.package_name, &target))
    };
    let sidecar = sidecar_directory(&executable)?;
    fs::create_dir_all(&sidecar)
        .map_err(|source| BuildError::path_io("create", &sidecar, source))?;
    // Old executables can still be running while another build replaces the
    // executable. Give each executable its own immutable asset directory.
    let assets = tempfile::Builder::new()
        .prefix("build-")
        .tempdir_in(&sidecar)?;
    let asset_name = assets
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("tempfile generates a UTF-8 build directory name");
    copy_component_fasls(&native, assets.path())?;
    stage_delivery_assets(delivery, assets.path())?;
    let validation_runtime = super::start_standalone(
        super::standalone_builder(None)
            .compilation_target(zkcl::CompilationTarget::from_triple(target.clone())?),
    )?;
    validate_delivery(delivery, assets.path(), &validation_runtime)
        .map_err(|error| format!("delivered startup validation failed: {error}"))?;
    if link && opts.emit_dir.is_some() {
        let emitted_sidecar = sidecar_directory(
            &dir.join(OutputKind::Executable.output_filename(&project.package_name, &target)),
        )?
        .join(asset_name);
        fs::create_dir_all(&emitted_sidecar)?;
        copy_component_fasls(&native, &emitted_sidecar)?;
        stage_delivery_assets(delivery, &emitted_sidecar)?;
    }
    if let Some(sdk) = sdk {
        let plan = native_sdk::project_plan(delivery, entry, asset_name);
        let source = native_sdk::project_source(&native, &plan, &target);
        let stem = if link {
            input_stem
        } else {
            &project.package_name
        };
        let output = sdk.build(
            dir,
            stem,
            &opts,
            &source,
            &native
                .iter()
                .map(|unit| unit.object.clone())
                .collect::<Vec<_>>(),
            &link_libraries,
            &link_search,
        )?;
        let _ = assets.keep();
        println!("{}", output.display());
        return Ok(());
    }
    let src = dir.join("src");
    fs::create_dir_all(&src).map_err(|source| BuildError::path_io("create", &src, source))?;
    write_generated_file(
        &src.join("main.rs"),
        project_main_source(&native, delivery, entry, asset_name)?,
    )?;
    let _ = fs::remove_file(src.join("lib.rs"));
    let runtime = project.write_build_configuration()?;
    if delivery.asdf.is_some() {
        use std::io::Write as _;
        let mut manifest = fs::OpenOptions::new()
            .append(true)
            .open(dir.join("Cargo.toml"))?;
        // Generated manifests may finish with build-dependencies; use an explicit table.
        writeln!(manifest, "\n[dependencies.tempfile]\nversion = \"3\"")?;
    }
    if !link_libraries.is_empty() || !link_search.is_empty() {
        let path = dir.join("build.rs");
        let mut script = fs::read_to_string(&path)?;
        let extra = link_search
            .iter()
            .map(|path| {
                format!(
                    "    println!({:?});\n",
                    format!("cargo:rustc-link-search=native={}", path.display())
                )
            })
            .chain(link_libraries.iter().map(|name| {
                format!(
                    "    println!({:?});\n",
                    format!("cargo:rustc-link-lib={name}")
                )
            }))
            .collect::<String>();
        let at = script.rfind('}').expect("generated build main");
        script.insert_str(at, &extra);
        write_generated_file(&path, script)?;
    }
    if link {
        let output = project.link_and_copy_output(&opts, &runtime)?;
        let _ = assets.keep();
        println!("{}", output.display());
    } else {
        let _ = assets.keep();
        println!("{}", dir.display());
    }
    Ok(())
}

fn lisp_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn sidecar_directory(executable: &Path) -> Result<PathBuf, io::Error> {
    let mut name = executable
        .file_name()
        .ok_or_else(|| io::Error::other("project output must name an executable file"))?
        .to_os_string();
    name.push(".zkcl");
    Ok(executable.with_file_name(name))
}

fn stage_link_search(directories: &[PathBuf], destination: &Path) -> io::Result<Vec<PathBuf>> {
    if directories.is_empty() {
        return Ok(Vec::new());
    }
    let stage = tempfile::Builder::new()
        .prefix("native-link-")
        .tempdir_in(destination)?;
    let paths = directories
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let target = stage.path().join(index.to_string());
            fs::create_dir_all(&target)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                if entry.file_type()?.is_file() {
                    fs::copy(entry.path(), target.join(entry.file_name()))?;
                }
            }
            target.canonicalize()
        })
        .collect::<io::Result<Vec<_>>>()?;
    let _directory = stage.keep();
    Ok(paths)
}

fn project_main_source(
    components: &[AotUnit],
    delivery: &Delivery,
    entry: &str,
    assets: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let (package, name) = entry
        .split_once(':')
        .ok_or_else(|| format!("expected a package-qualified entry, found {entry:?}"))?;
    let name = name.strip_prefix(':').unwrap_or(name);
    let mut source = String::new();
    for (index, component) in components.iter().enumerate() {
        let filename = component
            .registration
            .file_name()
            .and_then(|name| name.to_str())
            .expect("generated registration filename is UTF-8");
        source.push_str(&format!(
            "#[path = \"../modules/{filename}\"]\nmod component_{index:04};\n"
        ));
    }
    source.push_str(
        r#"
fn main() -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    #[cfg(feature = "ffi")]
    let runtime = unsafe { zkcl::Runtime::builder().allow_foreign_calls() }.start()?;
    #[cfg(not(feature = "ffi"))]
    let runtime = zkcl::Runtime::start()?;
    let arguments = std::env::args_os().skip(1).map(|argument| {
        argument.into_string().map_err(|_| std::io::Error::new(
            std::io::ErrorKind::InvalidData, "script arguments must be UTF-8"
        ))
    }).collect::<Result<Vec<_>, _>>()?;
    runtime.set_command_line_arguments(arguments);
    let executable = std::env::current_exe()?;
    let mut name = executable.file_name().ok_or_else(|| std::io::Error::other(
        "cannot determine the executable filename"
    ))?.to_os_string();
    name.push(".zkcl");
    let assets = executable.with_file_name(name);
"#,
    );
    source.push_str(&format!("    let assets = assets.join({assets:?});\n"));
    if delivery.asdf.is_some() {
        source.push_str(r#"
    let assets = assets.canonicalize()?;
    let asdf_output = tempfile::Builder::new().prefix("zkcl-asdf-").tempdir()?;
    runtime.load_file(&assets.join("asdf-bootstrap.lisp"))?;
    let initialize = runtime.lookup_symbol("ZKCL-PROJECT-ASDF", "INITIALIZE-DELIVERY")?.ok_or("missing ASDF delivery initializer")?;
    let source_root = runtime.make_string(assets.to_str().ok_or("ASDF delivery paths must be UTF-8")?)?;
    let output_root = runtime.make_string(&format!("{}/", asdf_output.path().to_str().ok_or("ASDF output paths must be UTF-8")?))?;
    runtime.call(&initialize, &[&source_root, &output_root])?;
"#);
    }
    for step in &delivery.steps {
        match step {
            StartupStep::LoadFasl(index) => {
                if *index >= components.len() {
                    return Err(format!("invalid native delivery action {index}").into());
                }
                source.push_str(&format!("    runtime.register_aot_module(&component_{index:04}::AOT_MODULE)?;\n    runtime.load_file(&assets.join(\"{index:04}.zkclfasl\"))?;\n"));
            }
            StartupStep::LoadAsdf(name) => source.push_str(&format!(
                "    runtime.eval({:?})?;\n",
                format!("(asdf:load-system {})", lisp_string(name))
            )),
            StartupStep::ReadyAsdfAlias(names) => {
                for name in names {
                    source.push_str(&format!(
                        "    runtime.eval({:?})?;\n",
                        format!(
                            "(zkcl-project-asdf::mark-native-ready {})",
                            lisp_string(name)
                        )
                    ));
                }
            }
        }
    }
    source.push_str(&format!(
        "    let entry = runtime.lookup_symbol({package:?}, {name:?})?.ok_or_else(|| std::io::Error::other({:?}))?;\n    runtime.call(&entry, &[])?;\n    Ok(std::process::ExitCode::from(runtime.requested_exit_code()))\n}}\n",
        format!("cannot find {entry}")
    ));
    Ok(source)
}

fn execute_build(opts: &BuildArgs, heap_size: Option<usize>) -> Result<(), BuildError> {
    let input_stem = opts
        .input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("module")
        .to_string();
    if opts
        .target
        .as_ref()
        .is_some_and(|(target, _)| target.is_javascript())
    {
        return build_javascript(opts, &input_stem, heap_size);
    }
    if opts.split_runtime {
        return Err(BuildError::JavaScriptOptions(
            "--split-runtime requires --target js",
        ));
    }
    if opts
        .target
        .as_ref()
        .is_some_and(|(_, name)| name == "wasm32-unknown-unknown")
    {
        return build_wasm(opts, &input_stem, heap_size);
    }
    if opts.wasm_runtime.is_some() {
        return Err(BuildError::WasmOptions(
            "--wasm-runtime requires --target wasm32-unknown-unknown",
        ));
    }
    let module_id = opts.module_id.as_deref().unwrap_or(&input_stem);
    let mut config = native_aot_config(module_id, opts.backend, opts.release);
    if let Some((target, _)) = &opts.target {
        config = config.with_target(
            target
                .machine()
                .expect("native build target")
                .triple()
                .clone(),
        );
    }
    config.jobs = opts.jobs;
    let object = compile_aot_object(&opts.input_path, &config, heap_size)?;
    if opts.list_entries {
        print_code_entries(&object, &config);
    }

    let temporary_project;
    let project_dir = if let Some(dir) = &opts.emit_dir {
        dir.as_path()
    } else {
        temporary_project = temporary_build_directory(&env::temp_dir(), &input_stem)?;
        temporary_project.path()
    };

    if let Some(sdk) = native_sdk::Sdk::locate(&config.target)? {
        fs::create_dir_all(project_dir)
            .map_err(|source| BuildError::path_io("create", project_dir, source))?;
        let _lock = if opts.emit_dir.is_some() {
            Some(lock_file(&project_dir.join(".zkcl-emit.lock"))?)
        } else {
            None
        };
        let object_path = write_aot_artifacts(project_dir, &input_stem, &object)?;
        let source = native_sdk::single_source(module_id, &object, &config.target);
        let output = sdk.build(
            project_dir,
            &input_stem,
            opts,
            &source,
            &[object_path],
            &[],
            &[],
        )?;
        println!("{}", output.display());
        return Ok(());
    }
    let project = GeneratedCargoProject::new(project_dir, &input_stem, opts.output_kind);
    project.emit_and_maybe_link(opts, &object)
}

pub(super) fn native_aot_config(
    module_id: impl Into<String>,
    backend: Option<AotBackendKind>,
    release: bool,
) -> AotTargetConfig {
    let mut config = AotTargetConfig::host(module_id).with_opt_level(if release {
        AotOptLevel::Speed
    } else {
        AotOptLevel::None
    });
    if let Some(backend) = backend {
        config = config.with_backend(backend);
    } else if release && AotBackendKind::Llvm.is_available() {
        config = config.with_backend(AotBackendKind::Llvm);
    }
    config
}

fn validate_javascript_options(opts: &BuildArgs) -> Result<(), BuildError> {
    if opts.backend.is_some()
        || opts.jobs.is_some()
        || opts.output_kind == OutputKind::StaticLibrary
        || opts.module_id.is_some()
        || opts.wasm_runtime.is_some()
    {
        return Err(BuildError::JavaScriptOptions(
            "JavaScript builds do not accept native AOT options or --wasm-runtime",
        ));
    }
    Ok(())
}

fn javascript_output_path(opts: &BuildArgs, stem: &str) -> PathBuf {
    opts.output_path.clone().unwrap_or_else(|| {
        opts.emit_dir
            .as_ref()
            .map(|dir| dir.join(format!("{stem}.mjs")))
            .unwrap_or_else(|| PathBuf::from(format!("{stem}.mjs")))
    })
}

fn write_javascript_module(
    opts: &BuildArgs,
    stem: &str,
    files: &[zkcl_bytecode::CompiledFile],
    entry: Option<&str>,
) -> Result<(), BuildError> {
    let output = javascript_output_path(opts, stem);
    let shared = opts.split_runtime.then(|| {
        let source = zkcl_js::emit_runtime_module();
        let name = format!("zkcl-runtime-{:x}.mjs", Sha256::digest(source.as_bytes()));
        (name, source)
    });
    let module = match (entry, &shared) {
        (Some(entry), Some((name, _))) => {
            zkcl_js::emit_project_with_runtime(files, entry, &format!("./{name}"))?
        }
        (Some(entry), None) => zkcl_js::emit_project(files, entry)?,
        (None, Some((name, _))) => {
            zkcl_js::emit_compiled_files_with_runtime(files, &format!("./{name}"))?
        }
        (None, None) => zkcl_js::emit_compiled_files(files)?,
    };
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|source| BuildError::path_io("create", parent, source))?;
    if let Some((name, source)) = shared {
        let runtime = parent.join(name);
        if runtime.file_name() == output.file_name() {
            return Err(BuildError::JavaScriptRuntimeCollision(runtime));
        }
        install_javascript_runtime(&runtime, source.as_bytes())?;
    }
    write_generated_file(&output, module)?;
    println!("{}", output.display());
    Ok(())
}

fn install_javascript_runtime(path: &Path, source: &[u8]) -> Result<(), BuildError> {
    let verify = |previous: Vec<u8>| {
        if previous == source {
            Ok(())
        } else {
            Err(BuildError::JavaScriptRuntimeCollision(path.to_owned()))
        }
    };
    match fs::read(path) {
        Ok(previous) => return verify(previous),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(BuildError::path_io("read", path, error)),
    }
    let mut temporary = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary.permissions(fs::Permissions::from_mode(0o666));
    }
    let mut staged = temporary
        .tempfile_in(path.parent().expect("runtime directory"))
        .map_err(|error| BuildError::path_io("stage", path, error))?;
    staged
        .write_all(source)
        .map_err(|error| BuildError::path_io("write", path, error))?;
    match staged.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            verify(fs::read(path).map_err(|error| BuildError::path_io("read", path, error))?)
        }
        Err(error) => Err(BuildError::path_io("install", path, error.error)),
    }
}

fn build_javascript(
    opts: &BuildArgs,
    stem: &str,
    heap_size: Option<usize>,
) -> Result<(), BuildError> {
    validate_javascript_options(opts)?;
    let target = zkcl::CompilationTarget::javascript();
    let mut file = if is_fasl(&opts.input_path) {
        let bytes = fs::read(&opts.input_path)
            .map_err(|source| BuildError::path_io("read", &opts.input_path, source))?;
        zkcl_bytecode::CompiledFile::decode_for_target(&bytes, &target)?
    } else {
        let runtime = super::start_standalone(
            super::standalone_builder(heap_size).compilation_target(target),
        )?;
        runtime.compile_file_to_bytecode(&opts.input_path)?
    };
    if opts.release {
        file.strip_debug();
    }
    if opts.list_entries {
        eprintln!("{} JavaScript top-level entries", file.entries.len());
    }
    write_javascript_module(opts, stem, &[file], None)
}

fn build_javascript_project(
    opts: &BuildArgs,
    stem: &str,
    entry: &str,
    delivery: &Delivery,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_javascript_options(opts)?;
    if delivery.asdf.is_some()
        || delivery
            .steps
            .iter()
            .any(|step| !matches!(step, StartupStep::LoadFasl(_)))
    {
        return Err(
            "JavaScript project delivery requires compiled startup units; ASDF filesystem actions cannot be omitted"
                .into(),
        );
    }
    let target = zkcl::CompilationTarget::javascript();
    let mut files = Vec::with_capacity(delivery.steps.len());
    for step in &delivery.steps {
        let StartupStep::LoadFasl(index) = step else {
            unreachable!("startup steps were checked");
        };
        let unit = delivery
            .units
            .get(*index)
            .ok_or("invalid project startup module index")?;
        let mut file =
            zkcl_bytecode::CompiledFile::decode_for_target(&fs::read(&unit.fasl)?, &target)?;
        if opts.release {
            file.strip_debug();
        }
        files.push(file);
    }
    write_javascript_module(opts, stem, &files, Some(entry))?;
    Ok(())
}

fn temporary_build_directory(
    parent: &Path,
    input_stem: &str,
) -> Result<tempfile::TempDir, BuildError> {
    fs::create_dir_all(parent).map_err(|source| BuildError::path_io("create", parent, source))?;
    tempfile::Builder::new()
        .prefix(&format!("zkcl-build-{input_stem}-"))
        .tempdir_in(parent)
        .map_err(|source| {
            BuildError::path_io("create temporary build directory in", parent, source)
        })
}

fn build_wasm(
    opts: &BuildArgs,
    input_stem: &str,
    heap_size: Option<usize>,
) -> Result<(), BuildError> {
    if opts.backend.is_some()
        || opts.jobs.is_some()
        || opts.output_kind == OutputKind::StaticLibrary
        || opts.emit_dir.is_some()
        || opts.module_id.is_some()
    {
        return Err(BuildError::WasmOptions(
            "native AOT options cannot be used with wasm32-unknown-unknown",
        ));
    }
    let runtime_path = native_sdk::wasm_runtime_path(opts.wasm_runtime.as_deref())?;
    let runtime_module = fs::read(&runtime_path)
        .map_err(|source| BuildError::path_io("read", &runtime_path, source))?;
    let input = &opts.input_path;
    let module = if is_fasl(input) {
        let fasl = fs::read(input).map_err(|source| BuildError::path_io("read", input, source))?;
        zkcl_wasm::codegen::link(&runtime_module, &fasl, opts.release)?
    } else {
        let runtime = super::start_standalone(
            super::standalone_builder(heap_size).compilation_target(zkcl_wasm::codegen::target()?),
        )?;
        let compiled = runtime.compile_file_to_bytecode(input)?;
        zkcl_wasm::codegen::link_compiled_file(&runtime_module, compiled, opts.release)?
    };
    if opts.list_entries {
        eprintln!(
            "{}/{} Wasm entries",
            module.entries.len(),
            module.bytecode_entries
        );
        let mut entries = module.entries.iter().peekable();
        let mut rejected = module.leaf_rejections.iter().peekable();
        for index in 0..module.bytecode_entries {
            let backend = match entries
                .next_if(|entry| entry.entry_index as usize == index)
                .map(|entry| &entry.entry)
            {
                Some(zkcl_wasm::abi::RegisteredEntry::Leaf {
                    fallback: Some(_), ..
                }) => "wasm leaf + fallback",
                Some(zkcl_wasm::abi::RegisteredEntry::Leaf { fallback: None, .. }) => "wasm leaf",
                Some(zkcl_wasm::abi::RegisteredEntry::Function(_)) => "wasm function",
                None => "bytecode",
            };
            eprintln!("  {index}  {backend}");
            if let Some((_, reason)) = rejected.next_if(|(entry, _)| *entry as usize == index) {
                eprintln!("     leaf selection skipped ({reason})");
            }
        }
    }
    let output = opts
        .output_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{input_stem}.wasm")));
    fs::write(&output, module.bytes).map_err(|source| BuildError::path_io("write", &output, source))
}

fn build_wasm_project(
    opts: &BuildArgs,
    stem: &str,
    entry: &str,
    delivery: &Delivery,
) -> Result<(), Box<dyn std::error::Error>> {
    if opts.target.as_ref().map(|(_, name)| name.as_str()) != Some("wasm32-unknown-unknown") {
        return Err("project Wasm builds require --target wasm32-unknown-unknown".into());
    }
    if delivery.asdf.is_some()
        || delivery
            .steps
            .iter()
            .any(|step| !matches!(step, StartupStep::LoadFasl(_)))
    {
        return Err(
            "Wasm project delivery cannot load ASDF systems that require a host filesystem".into(),
        );
    }
    if !delivery.link_libraries.is_empty() || !delivery.link_search.is_empty() {
        return Err("Wasm project delivery cannot link native host libraries".into());
    }
    if opts.backend.is_some() || opts.jobs.is_some() {
        return Err("Wasm project builds do not accept native AOT backend or job options".into());
    }
    let runtime_path = native_sdk::wasm_runtime_path(opts.wasm_runtime.as_deref())?;
    let runtime = fs::read(&runtime_path)?;
    let mut fasls = Vec::new();
    for step in &delivery.steps {
        let StartupStep::LoadFasl(index) = step else {
            unreachable!()
        };
        let unit = delivery
            .units
            .get(*index)
            .ok_or("invalid project startup module index")?;
        fasls.push(fs::read(&unit.fasl)?);
    }
    let module = zkcl_wasm::codegen::link_project(&runtime, &fasls, entry, opts.release)?;
    let output = opts.output_path.clone().unwrap_or_else(|| {
        opts.emit_dir
            .as_ref()
            .map(|dir| dir.join(format!("{stem}.wasm")))
            .unwrap_or_else(|| PathBuf::from(format!("{stem}.wasm")))
    });
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    write_generated_file(&output, module.bytes)?;
    println!("{}", output.display());
    Ok(())
}

fn print_code_entries(object: &AotObject, config: &AotTargetConfig) {
    eprintln!(
        "{}  {}/{} native entries",
        config.target,
        object.registration_entries.len(),
        object.code_entry_count
    );
    let mut native = object.registration_entries.iter().peekable();
    let mut rejected = object.direct_rejections.iter().peekable();
    for index in 0..object.code_entry_count {
        if let Some(entry) = native.next_if(|entry| entry.entry_index as usize == index) {
            eprintln!("  {index}  native  {}", entry.symbol_name);
            if let Some(leaf) = &entry.leaf_symbol {
                eprintln!("     register entry  {leaf}");
            }
        } else {
            eprintln!("  {index}  bytecode");
            if let Some((_, reason)) = rejected.next_if(|(entry, _)| *entry as usize == index) {
                eprintln!("     {reason}");
            }
        }
    }
}

fn compile_aot_object(
    input: &Path,
    config: &AotTargetConfig,
    heap_size: Option<usize>,
) -> Result<AotObject, BuildError> {
    let object = if is_fasl(input) {
        let bytes = fs::read(input).map_err(|source| BuildError::path_io("read", input, source))?;
        compile_fasl_bytes_to_object(bytes, config)
    } else {
        let target = zkcl::CompilationTarget::from_triple(config.target.clone())?;
        let runtime = super::start_standalone(
            super::standalone_builder(heap_size).compilation_target(target),
        )?;
        let compiled = runtime.compile_file_to_bytecode(input)?;
        compile_fasl_to_object(&compiled, config)
    };
    object.map_err(|source| BuildError::CompileAot {
        path: input.to_owned(),
        source,
    })
}

fn is_fasl(input: &Path) -> bool {
    input
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zkclfasl"))
}

struct GeneratedCargoProject<'a> {
    dir: &'a Path,
    input_stem: &'a str,
    kind: OutputKind,
    package_name: String,
    object_filenames: Vec<PathBuf>,
}

impl<'a> GeneratedCargoProject<'a> {
    fn new(dir: &'a Path, input_stem: &'a str, kind: OutputKind) -> Self {
        let kind_label = match kind {
            OutputKind::Executable => "bin",
            OutputKind::StaticLibrary => "lib",
        };
        let package_stem = input_stem.replace(|c: char| !c.is_ascii_alphanumeric(), "-");
        Self {
            dir,
            input_stem,
            kind,
            package_name: format!("zkcl-aot-{kind_label}-{package_stem}"),
            object_filenames: vec![PathBuf::from(format!("{input_stem}.o"))],
        }
    }

    fn emit_and_maybe_link(&self, opts: &BuildArgs, object: &AotObject) -> Result<(), BuildError> {
        let src_dir = self.dir.join("src");
        fs::create_dir_all(&src_dir)
            .map_err(|source| BuildError::path_io("create", &src_dir, source))?;

        // An explicit emit directory is shared by all builds that name it. Keep
        // its sources and object together until Cargo has linked this invocation.
        let _emit_lock = if opts.emit_dir.is_some() {
            Some(lock_file(&self.dir.join(".zkcl-emit.lock"))?)
        } else {
            None
        };

        let runtime = self.write_project_files(&src_dir, object)?;
        if opts.output_path.is_some() || opts.emit_dir.is_none() {
            let output_path = self.link_and_copy_output(opts, &runtime)?;
            println!("{}", output_path.display());
        } else {
            println!("{}", self.dir.display());
        }
        Ok(())
    }

    fn write_project_files(
        &self,
        src_dir: &Path,
        object: &AotObject,
    ) -> Result<RuntimeDependency, BuildError> {
        write_aot_artifacts(self.dir, self.input_stem, object)?;
        write_generated_file(&src_dir.join("aot_module.rs"), &object.registration_source)?;

        let (entry_file, unused_entry, source) = match self.kind {
            OutputKind::Executable => ("main.rs", "lib.rs", object.standalone_main_source.as_str()),
            OutputKind::StaticLibrary => ("lib.rs", "main.rs", staticlib_entry_source()),
        };
        let _ = fs::remove_file(src_dir.join(unused_entry));
        write_generated_file(&src_dir.join(entry_file), source)?;

        self.write_build_configuration()
    }

    fn write_build_configuration(&self) -> Result<RuntimeDependency, BuildError> {
        let crate_override = env::var_os("ZKCL_CRATE_PATH").map(PathBuf::from);
        let runtime =
            RuntimeDependency::locate(Path::new(env!("CARGO_MANIFEST_DIR")), crate_override);
        self.write_cargo_files(&runtime)?;
        Ok(runtime)
    }

    fn write_cargo_files(&self, runtime: &RuntimeDependency) -> Result<(), BuildError> {
        write_generated_file(
            &self.dir.join("build.rs"),
            build_script(&self.object_filenames, self.kind),
        )?;

        let dependency = runtime.manifest_fields()?;
        let package_name = &self.package_name;
        let default_features = if cfg!(feature = "ffi") {
            "[\"ffi\"]"
        } else {
            "[]"
        };
        let lib_section = match self.kind {
            OutputKind::Executable => "",
            OutputKind::StaticLibrary => "\n[lib]\ncrate-type = [\"staticlib\"]\n",
        };
        let mut cargo_toml = format!(
            "[package]\nname = \"{package_name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{lib_section}\n[workspace]\n\n[features]\ndefault = {default_features}\nffi = [\"zkcl/ffi\"]\n\n[dependencies]\nzkcl = {{ {dependency} }}\n"
        );
        if self.kind == OutputKind::StaticLibrary {
            cargo_toml.push_str("\n[build-dependencies]\ncc = \"1\"\n");
        }
        write_generated_file(&self.dir.join("Cargo.toml"), cargo_toml)?;

        if let Some(workspace_lockfile) = runtime
            .workspace_root()
            .map(|root| root.join("Cargo.lock"))
            .filter(|path| path.exists())
        {
            fs::copy(&workspace_lockfile, self.dir.join("Cargo.lock"))
                .map_err(BuildError::CopyLockfile)?;
        }
        Ok(())
    }

    fn link_and_copy_output(
        &self,
        opts: &BuildArgs,
        runtime: &RuntimeDependency,
    ) -> Result<PathBuf, BuildError> {
        let cargo_target_dir = env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .or_else(|| runtime.workspace_root().map(|root| root.join("target")))
            .map(|target| target.join("aot-build-cache"))
            .unwrap_or_else(|| env::temp_dir().join("zkcl-aot-build-cache"));

        // Cargo releases its own lock before zkcl copies the linked file. Keep builds
        // of the same generated package from replacing that file during the copy.
        fs::create_dir_all(&cargo_target_dir)
            .map_err(|source| BuildError::path_io("create", &cargo_target_dir, source))?;
        let _link_lock = lock_file(&cargo_target_dir.join(".zkcl-link.lock"))?;

        let mut cargo_build = Command::new("cargo");
        cargo_build.args(["build", "--color", "never"]);
        if let Some(jobs) = opts.jobs {
            cargo_build.arg("--jobs").arg(jobs.to_string());
        }
        if matches!(runtime, RuntimeDependency::Local { .. }) {
            cargo_build.arg("--offline");
        }
        cargo_build
            .arg("--manifest-path")
            .arg(self.dir.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&cargo_target_dir);
        for (index, object) in self.object_filenames.iter().enumerate() {
            let object_path = canonicalize(&self.dir.join(object))?;
            cargo_build.env(link_object_environment(index), object_path);
        }
        if let Some((_, target_arg)) = &opts.target {
            cargo_build.arg("--target").arg(target_arg);
        }
        if opts.release {
            cargo_build.arg("--release");
        }
        let cargo_output = cargo_build.output().map_err(BuildError::InvokeCargo)?;
        if !cargo_output.status.success() {
            return Err(BuildError::Link {
                status: cargo_output.status,
                stdout: String::from_utf8_lossy(&cargo_output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&cargo_output.stderr).into_owned(),
            });
        }

        let profile = if opts.release { "release" } else { "debug" };
        let profile_dir = match &opts.target {
            Some((_, target_arg)) => cargo_target_dir.join(target_arg).join(profile),
            None => cargo_target_dir.join(profile),
        };
        let effective_triple = opts
            .target
            .as_ref()
            .map(|(target, _)| {
                target
                    .machine()
                    .expect("native build target")
                    .triple()
                    .clone()
            })
            .unwrap_or_else(Triple::host);
        let linked_path = profile_dir.join(
            self.kind
                .linked_filename(&self.package_name, &effective_triple),
        );
        let output_path = opts.output_path.clone().unwrap_or_else(|| {
            PathBuf::from(
                self.kind
                    .output_filename(self.input_stem, &effective_triple),
            )
        });
        if let Some(parent) = output_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|source| BuildError::path_io("create", parent, source))?;
        }
        let output_parent = output_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let staged = tempfile::NamedTempFile::new_in(output_parent)
            .map_err(|source| BuildError::path_io("stage output in", output_parent, source))?;
        fs::copy(&linked_path, staged.path()).map_err(|source| BuildError::CopyOutput {
            linked: linked_path.clone(),
            output: output_path.clone(),
            source,
        })?;
        staged
            .persist(&output_path)
            .map_err(|error| BuildError::CopyOutput {
                linked: linked_path,
                output: output_path.clone(),
                source: error.error,
            })?;
        Ok(output_path)
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, BuildError> {
    path.canonicalize()
        .map_err(|source| BuildError::path_io("resolve", path, source))
}

fn lock_file(path: &Path) -> Result<fs::File, BuildError> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| BuildError::path_io("open", path, source))?;
    file.lock()
        .map_err(|source| BuildError::path_io("lock", path, source))?;
    Ok(file)
}

/// Cargo uses modification times to decide whether to rebuild the generated crate.
fn write_generated_file(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), BuildError> {
    let contents = contents.as_ref();
    if fs::read(path).is_ok_and(|previous| previous == contents) {
        return Ok(());
    }
    fs::write(path, contents).map_err(|source| BuildError::path_io("write", path, source))
}

fn write_aot_artifacts(dir: &Path, stem: &str, object: &AotObject) -> Result<PathBuf, BuildError> {
    let object_path = dir.join(format!("{stem}.o"));
    write_generated_file(&object_path, &object.object_bytes)?;
    write_generated_file(&dir.join(format!("{stem}.zkclfasl")), &object.fasl_bytes)?;
    if let Some(ir) = &object.llvm_ir {
        write_generated_file(&dir.join(format!("{stem}.ll")), ir)?;
    }
    Ok(object_path)
}

fn staticlib_entry_source() -> &'static str {
    "// Static library entry points generated by zkcl build.\n\
pub mod aot_module;\n\n\
pub use aot_module::{AOT_CODES, AOT_MODULE, FASL_BYTES, load, register};\n\n\
/// Return a pointer to this module's static `AotModuleDescriptor`.\n\
#[unsafe(no_mangle)]\n\
pub extern \"C\" fn zkcl_aot_module_descriptor() -> *const zkcl::native::AotModuleDescriptor {\n    \
    &aot_module::AOT_MODULE\n\
}\n\n\
/// Start a runtime and load this AOT module. Returns 0 on success, 1 on failure.\n\
#[unsafe(no_mangle)]\n\
pub extern \"C\" fn zkcl_aot_run() -> i32 {\n    \
    let builder = zkcl::Runtime::builder();\n    \
    #[cfg(feature = \"ffi\")]\n    \
    let builder = unsafe { builder.allow_foreign_calls() };\n    \
    let Ok(runtime) = builder.start() else {\n        \
        return 1;\n    \
    };\n    \
    if runtime.load_aot_module(&aot_module::AOT_MODULE).is_ok() {\n        \
        0\n    \
    } else {\n        \
        1\n    \
    }\n\
}\n"
}

fn link_object_environment(index: usize) -> String {
    if index == 0 {
        "ZKCL_AOT_LINK_OBJECT".to_owned()
    } else {
        format!("ZKCL_AOT_LINK_OBJECT_{index}")
    }
}

fn build_script(object_names: &[PathBuf], kind: OutputKind) -> String {
    let mut inputs = String::new();
    for (index, object) in object_names.iter().enumerate() {
        inputs.push_str(&format!(
            "        ({:?}, {:?}),\n",
            link_object_environment(index),
            object.to_str().expect("generated object filename is UTF-8")
        ));
    }
    let link = match kind {
        OutputKind::StaticLibrary => {
            "    cc::Build::new().objects(&objects).compile(\"zkcl_aot_obj\");\n"
        }
        OutputKind::Executable => {
            "    for object in objects {\n        println!(\"cargo:rustc-link-arg={}\", object.display());\n    }\n"
        }
    };
    format!(
        r#"fn main() {{
    // The CLI shares a Cargo target directory between generated projects.
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let manifest = std::path::PathBuf::from(manifest);
    let inputs: &[(&str, &str)] = &[
{inputs}    ];
    let objects = inputs.iter().map(|(variable, filename)| {{
        println!("cargo:rerun-if-env-changed={{variable}}");
        let object = std::env::var_os(variable)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| manifest.join(filename));
        println!("cargo:rerun-if-changed={{}}", object.display());
        object
    }}).collect::<Vec<_>>();
{link}}}
"#
    )
}

impl OutputKind {
    fn linked_filename(self, package: &str, target: &Triple) -> String {
        if self == Self::StaticLibrary {
            self.output_filename(&package.replace('-', "_"), target)
        } else {
            self.output_filename(package, target)
        }
    }

    fn output_filename(self, stem: &str, target: &Triple) -> String {
        match self {
            Self::Executable if target.operating_system == OperatingSystem::Windows => {
                format!("{stem}.exe")
            }
            Self::Executable => stem.to_owned(),
            Self::StaticLibrary
                if target.operating_system == OperatingSystem::Windows
                    && target.environment == Environment::Msvc =>
            {
                format!("{stem}.lib")
            }
            Self::StaticLibrary => format!("lib{stem}.a"),
        }
    }
}

enum RuntimeDependency {
    Local {
        crate_dir: PathBuf,
        workspace_root: Option<PathBuf>,
    },
    Registry,
}

impl RuntimeDependency {
    fn locate(cli_dir: &Path, crate_override: Option<PathBuf>) -> Self {
        if let Some(crate_dir) = crate_override.filter(|path| path.join("Cargo.toml").is_file()) {
            return Self::Local {
                crate_dir,
                workspace_root: None,
            };
        }
        if let Some(crates_dir) = cli_dir.parent() {
            let crate_dir = crates_dir.join("zkcl");
            if crate_dir.join("Cargo.toml").is_file() {
                return Self::Local {
                    crate_dir,
                    workspace_root: crates_dir.parent().map(Path::to_path_buf),
                };
            }
        }
        Self::Registry
    }

    fn manifest_fields(&self) -> Result<String, BuildError> {
        let Self::Local { crate_dir, .. } = self else {
            // Generated native modules link against the same release that emitted them.
            return Ok(format!("version = \"={}\"", env!("CARGO_PKG_VERSION")));
        };
        let crate_dir = canonicalize(crate_dir)?;
        let path = crate_dir
            .to_str()
            .ok_or_else(|| BuildError::NonUtf8CratePath(crate_dir.clone()))?;
        // A JSON string is a TOML basic string once the DEL character is escaped.
        let literal = serde_json::to_string(path)
            .expect("serialize the UTF-8 dependency path")
            .replace('\x7f', "\\u007f");
        Ok(format!("path = {literal}"))
    }

    fn workspace_root(&self) -> Option<&Path> {
        match self {
            Self::Local { workspace_root, .. } => workspace_root.as_deref(),
            Self::Registry => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_cli_uses_matching_registry_runtime() {
        let registry = tempfile::tempdir().unwrap();
        let cli_dir = registry
            .path()
            .join(concat!("zkcl-cli-", env!("CARGO_PKG_VERSION")));
        let runtime_dir = registry
            .path()
            .join(concat!("zkcl-", env!("CARGO_PKG_VERSION")));
        fs::create_dir_all(&cli_dir).unwrap();
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(runtime_dir.join("Cargo.toml"), "").unwrap();

        let runtime = RuntimeDependency::locate(&cli_dir, None);
        let emit_dir = tempfile::tempdir().unwrap();
        let project =
            GeneratedCargoProject::new(emit_dir.path(), "example", OutputKind::Executable);
        project.write_cargo_files(&runtime).unwrap();

        let manifest = fs::read_to_string(emit_dir.path().join("Cargo.toml")).unwrap();
        let dependency = concat!("zkcl = { version = \"=", env!("CARGO_PKG_VERSION"), "\" }");
        assert!(manifest.lines().any(|line| line == dependency));
    }

    #[test]
    fn typed_command_selects_cross_target_static_library_options() {
        use clap::Parser;
        let cli = super::super::args::Cli::try_parse_from([
            "zkcl",
            "build",
            "app.lisp",
            "--target",
            "aarch64-apple-ios",
            "--staticlib",
            "--release",
            "--emit-dir",
            "out",
            "--module-id",
            "demo-mod",
            "-o",
            "libdemo.a",
        ])
        .unwrap();
        let super::super::args::Command::Build(request) = cli.command else {
            panic!("build command")
        };
        let opts = BuildArgs::from_request(request);
        assert_eq!(opts.input_path, PathBuf::from("app.lisp"));
        assert_eq!(opts.output_path, Some(PathBuf::from("libdemo.a")));
        assert_eq!(opts.output_kind, OutputKind::StaticLibrary);
        assert!(opts.release);
        assert_eq!(opts.emit_dir, Some(PathBuf::from("out")));
        assert_eq!(opts.module_id.as_deref(), Some("demo-mod"));
        let (triple, raw) = opts.target.unwrap();
        assert_eq!(raw, "aarch64-apple-ios");
        assert_eq!(
            triple,
            "aarch64-apple-ios"
                .parse::<zkcl::CompilationTarget>()
                .unwrap()
        );
    }

    #[test]
    fn temporary_projects_are_distinct_and_cleaned_during_unwind() {
        let parent =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/cli-embedding");
        fs::create_dir_all(&parent).unwrap();
        let first = temporary_build_directory(&parent, "ownership").unwrap();
        let second = temporary_build_directory(&parent, "ownership").unwrap();
        let first_path = first.path().to_owned();
        let second_path = second.path().to_owned();
        assert_ne!(first_path, second_path);
        fs::write(first_path.join("input"), "first").unwrap();
        fs::write(second_path.join("input"), "second").unwrap();

        let unwind = std::panic::catch_unwind(move || {
            let _project = first;
            panic!("abort build");
        });
        assert!(unwind.is_err());
        assert!(!first_path.exists());
        assert_eq!(
            fs::read_to_string(second_path.join("input")).unwrap(),
            "second"
        );
        drop(second);
        assert!(!second_path.exists());
    }

    #[test]
    fn linked_filenames_follow_target_conventions() {
        let ios = "aarch64-apple-ios".parse().unwrap();
        assert_eq!(
            OutputKind::StaticLibrary.linked_filename("zkcl-app-demo", &ios),
            "libzkcl_app_demo.a"
        );

        assert_eq!(
            OutputKind::StaticLibrary.output_filename("demo-app", &ios),
            "libdemo-app.a"
        );

        let win_msvc = "x86_64-pc-windows-msvc".parse().unwrap();
        assert_eq!(
            OutputKind::StaticLibrary.linked_filename("zkcl-app-demo", &win_msvc),
            "zkcl_app_demo.lib"
        );
        assert_eq!(
            OutputKind::StaticLibrary.output_filename("demo-app", &win_msvc),
            "demo-app.lib"
        );
        assert_eq!(
            OutputKind::Executable.linked_filename("zkcl-app-demo", &win_msvc),
            "zkcl-app-demo.exe"
        );
    }
}
