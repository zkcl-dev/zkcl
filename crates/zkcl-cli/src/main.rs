mod analysis;
mod args;
mod build;
mod dap;
mod diagnostics;
mod project;
mod repl;
mod testing;
mod wasm;

use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use zkcl::Runtime;

fn main() -> ExitCode {
    use clap::CommandFactory;
    clap_complete::CompleteEnv::with_factory(args::Cli::command).complete();
    let mut arguments: Vec<OsString> = env::args_os().collect();
    let invoked_as_x = arguments
        .first()
        .as_deref()
        .and_then(|p| Path::new(p).file_name())
        .is_some_and(|name| name == "zkclx" || name == "zkclx.exe");
    if invoked_as_x {
        arguments[0] = "zkcl".into();
        arguments.insert(1, "x".into());
    }
    let color = zkcl_terminal::command_color(args::Cli::command(), &arguments, args::Color::Auto);
    let cli = match args::Cli::try_request_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            let stream = if error.use_stderr() {
                zkcl_terminal::Stream::Stderr
            } else {
                zkcl_terminal::Stream::Stdout
            };
            let code = error.exit_code();
            let command = args::Cli::command().color(zkcl_terminal::clap_color(color, stream));
            let _ = error.with_cmd(&command).print();
            return ExitCode::from(code as u8);
        }
    };
    zkcl_terminal::configure(cli.color);
    if let Some(directory) = &cli.directory {
        if let Err(error) = env::set_current_dir(directory) {
            let message = format!("cannot enter {}: {error}", directory.display());
            return match cli.command {
                args::Command::Check(request) | args::Command::Lint(request) => {
                    analysis::input_failure(request, message)
                }
                _ => {
                    report_error("zkcl: ", &io::Error::new(error.kind(), message));
                    ExitCode::FAILURE
                }
            };
        }
    }
    dispatch(cli.command, cli.heap_size_mib)
}

fn dispatch(command: args::Command, heap_size: Option<usize>) -> ExitCode {
    use args::Command;
    match command {
        Command::Check(request) => analysis::check(request),
        Command::Lint(request) => analysis::lint(request),
        Command::Fmt(request) => analysis::format(request),
        Command::Explain(request) => analysis::explain(request),
        Command::Compile(request) => compile(request, heap_size),
        Command::Build(request) if request.path.is_some() => build::run(request, heap_size),
        Command::Run(request) if request.path.is_some() => run(request, heap_size),
        Command::Test(request) if request.system.is_some() => testing::command(request, heap_size),
        Command::Repl(request) => match repl::run(&request, heap_size) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                report_error("", error.as_ref());
                ExitCode::FAILURE
            }
        },
        Command::Dap => dap::command(std::iter::empty(), heap_size),
        Command::Lsp => match zkcl_lsp::run_stdio() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                report_error("zkcl lsp: ", &error);
                ExitCode::FAILURE
            }
        },
        Command::Completions(request) => {
            let shells = clap_complete::env::Shells::builtins();
            let shell = shells
                .completer(&request.shell.to_string())
                .expect("all declared shells have dynamic completion adapters");
            let executable = env::current_exe().unwrap_or_else(|_| PathBuf::from("zkcl"));
            match shell.write_registration(
                "COMPLETE",
                "zkcl",
                "zkcl",
                &executable.to_string_lossy(),
                &mut io::stdout(),
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    report_error("zkcl completions: ", &error);
                    ExitCode::FAILURE
                }
            }
        }
        project_command => project::command(project_command, heap_size),
    }
}

fn run(request: args::RunArgs, heap_size: Option<usize>) -> ExitCode {
    let jit_request = request.runtime.jit_request();
    let module_directories = request.runtime.module_dir;
    let path = request
        .path
        .as_deref()
        .expect("file run selected after parsing");
    let arguments = match utf8_arguments(request.arguments) {
        Ok(arguments) => arguments,
        Err(error) => {
            report_error("zkcl run: ", error.as_ref());
            return ExitCode::from(2);
        }
    };
    if path
        .extension()
        .is_some_and(|extension| extension == "wasm")
    {
        if jit_request.is_some() || !module_directories.is_empty() {
            eprintln!("zkcl run: JIT and Lisp module directories apply to Lisp source and FASL");
            return ExitCode::from(2);
        }
        return match wasm::run(path, &arguments, heap_size) {
            Ok(status) => ExitCode::from(status),
            Err(error) => {
                report_error("", &error);
                ExitCode::FAILURE
            }
        };
    }
    let runtime = match jit_request
        .map(JitBackendRequest::create_backend)
        .transpose()
        .and_then(|backend| Ok(session_runtime(backend.as_ref(), heap_size)?))
    {
        Ok(runtime) => runtime,
        Err(error) => {
            report_error("zkcl run: ", error.as_ref());
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = add_module_directories(&runtime, &module_directories) {
        report_error("zkcl run: ", &error);
        return ExitCode::FAILURE;
    }
    runtime.set_command_line_arguments(arguments);
    match runtime.load_file(path) {
        Ok(()) => ExitCode::from(runtime.requested_exit_code()),
        Err(error) => {
            report_error("", &error);
            ExitCode::FAILURE
        }
    }
}

fn utf8_arguments(arguments: Vec<OsString>) -> Result<Vec<String>, Box<dyn Error>> {
    arguments
        .into_iter()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| "Lisp and Wasm program arguments must be UTF-8".into())
        })
        .collect()
}

fn report_error(prefix: &str, error: &(dyn Error + 'static)) {
    eprintln!(
        "{}",
        zkcl_terminal::render_stderr(&diagnostics::from_error(prefix, error))
    );
}

// Keep startup forms and one-off evaluations on the VM.
const CLI_JIT_THRESHOLD: u32 = 32;

fn standalone_builder(heap_size: Option<usize>) -> zkcl::RuntimeBuilder {
    let mut builder = Runtime::builder();
    if let Some(bytes) = heap_size {
        builder = builder.heap_size_bytes(bytes);
    }
    // Standalone scripts have process authority and supply their C library contracts.
    jit_and_ffi::enable_foreign_calls(builder)
}

fn standalone_runtime(heap_size: Option<usize>) -> Result<Runtime, zkcl::AlreadyStarted> {
    start_standalone(standalone_builder(heap_size))
}

fn start_standalone(builder: zkcl::RuntimeBuilder) -> Result<Runtime, zkcl::AlreadyStarted> {
    builder.start()
}

#[derive(Clone, Copy)]
enum JitBackendRequest {
    Default,
    Direct,
    Cranelift,
}

impl JitBackendRequest {
    fn create_backend(self) -> Result<Arc<dyn zkcl::native::JitBackend>, Box<dyn Error>> {
        match self {
            Self::Default if cfg!(feature = "cranelift-jit") => Self::Cranelift.create_backend(),
            Self::Default if cfg!(feature = "direct-jit") => Self::Direct.create_backend(),
            Self::Default => {
                Err("--jit requires the cranelift-jit or direct-jit Cargo feature".into())
            }
            Self::Direct => jit_and_ffi::create_direct_jit(),
            Self::Cranelift => jit_and_ffi::create_cranelift_jit(),
        }
    }
}

mod jit_and_ffi {
    use std::error::Error;
    use std::sync::Arc;

    pub(super) fn enable_foreign_calls(builder: zkcl::RuntimeBuilder) -> zkcl::RuntimeBuilder {
        #[cfg(feature = "ffi")]
        {
            // SAFETY: Standalone CLI scripts opt into process authority for C libraries.
            unsafe { builder.allow_foreign_calls() }
        }
        #[cfg(not(feature = "ffi"))]
        builder
    }

    pub(super) fn create_direct_jit() -> Result<Arc<dyn zkcl::native::JitBackend>, Box<dyn Error>> {
        #[cfg(feature = "direct-jit")]
        {
            Ok(Arc::new(zkcl::jit::DirectJitBackend::new()?))
        }
        #[cfg(not(feature = "direct-jit"))]
        Err("direct JIT requires the direct-jit Cargo feature".into())
    }

    pub(super) fn create_cranelift_jit() -> Result<Arc<dyn zkcl::native::JitBackend>, Box<dyn Error>>
    {
        #[cfg(feature = "cranelift-jit")]
        {
            Ok(Arc::new(zkcl::jit::CraneliftJitBackend::new()?))
        }
        #[cfg(not(feature = "cranelift-jit"))]
        Err("Cranelift JIT requires the cranelift-jit Cargo feature".into())
    }
}

fn session_runtime(
    backend: Option<&Arc<dyn zkcl::native::JitBackend>>,
    heap_size: Option<usize>,
) -> Result<Runtime, zkcl::AlreadyStarted> {
    let runtime = standalone_runtime(heap_size)?;
    if let Some(backend) = backend {
        runtime.set_jit_threshold(CLI_JIT_THRESHOLD);
        runtime.set_jit_backend(backend.clone());
    }
    Ok(runtime)
}

#[derive(Debug, thiserror::Error)]
#[error("{path}: {source}", path = .path.display())]
struct ModuleDirectoryError {
    path: PathBuf,
    #[source]
    source: io::Error,
}

fn add_module_directories(
    runtime: &Runtime,
    directories: &[PathBuf],
) -> Result<(), ModuleDirectoryError> {
    for directory in directories {
        runtime
            .add_module_directory(directory)
            .map_err(|source| ModuleDirectoryError {
                path: directory.clone(),
                source,
            })?;
    }
    Ok(())
}

fn compile(request: args::CompileArgs, heap_size: Option<usize>) -> ExitCode {
    let mut builder = standalone_builder(heap_size);
    if let Some(target) = request.target {
        builder = builder.compilation_target(target.parse().expect("clap validated target triple"));
    }
    let runtime = match start_standalone(builder) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("zkcl compile: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.compile_file(&request.path, request.output.as_deref()) {
        Ok(compiled) => {
            println!("{}", compiled.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            report_error("", &error);
            ExitCode::FAILURE
        }
    }
}
