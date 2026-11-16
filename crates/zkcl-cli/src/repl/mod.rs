mod commands;
mod debugger;
mod editor;
mod inspector;
mod json;
mod snapshot;
mod terminal_input;
mod theme;

use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use reedline::{
    CommandLineSearch, HistoryItem, Prompt, PromptEditMode, SearchDirection, SearchFilter,
    SearchQuery, Signal, ValidationResult, Validator,
};
use zkcl::native::JitBackend;
use zkcl::{EvalError, EvalErrorKind, Root, Runtime};
use zkcl_diagnostics::Diagnostic;
use zkcl_terminal::{RenderOptions, Stream, write_reports};

use commands::{
    ParsedCommand, analyze_source_command, apply_setting, command_path, compile_paths,
    expand_tilde, parse_command, render_env, render_help, run_apropos_command, run_cd_command,
    run_describe_command, run_disassemble_command, run_doc_command, run_gc_command,
    run_macroexpand_command, run_package_command, run_packages_command, run_room_command,
    run_time_command, run_trace_command, run_type_command, unquote_path,
};
use editor::EditorAnalysis;
use theme::{ReplTheme, format_duration};

use super::project::{ActiveProject, RefreshReport};

const RESULT_PREFIX: &str = "=> ";
const RESULT_INDENT: &str = "   ";

#[derive(Clone)]
struct OutputTracker(Arc<AtomicBool>);

impl OutputTracker {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    fn at_line_start(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn set_at_line_start(&self, at_line_start: bool) {
        self.0.store(at_line_start, Ordering::Relaxed);
    }
}

struct TrackedOutput<W> {
    writer: W,
    tracker: OutputTracker,
}

impl<W: Write> Write for TrackedOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.writer.write(bytes)?;
        if written > 0 {
            self.tracker.set_at_line_start(bytes[written - 1] == b'\n');
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write> TrackedOutput<W> {
    fn ensure_newline(&mut self) -> io::Result<()> {
        if !self.tracker.at_line_start() {
            self.write_all(b"\n")?;
        }
        self.flush()
    }
}

struct ReplPrinter {
    stdout: OutputTracker,
    stderr: OutputTracker,
    theme: ReplTheme,
    result_theme: ReplTheme,
    line_editing: bool,
}

impl ReplPrinter {
    fn new(color: bool, result_color: bool, stdout_is_terminal: bool) -> Self {
        let stderr = OutputTracker::new();
        Self {
            stdout: if stdout_is_terminal {
                stderr.clone()
            } else {
                OutputTracker::new()
            },
            stderr,
            theme: ReplTheme::new(color),
            result_theme: ReplTheme::new(result_color),
            // The line editor queries cursor position through stdout. Use
            // ordinary terminal input when stdout belongs to a file or pipe.
            line_editing: stdout_is_terminal,
        }
    }

    fn standard_output(&self) -> TrackedOutput<io::Stdout> {
        TrackedOutput {
            writer: io::stdout(),
            tracker: self.stdout.clone(),
        }
    }

    fn error_output(&self) -> TrackedOutput<io::Stderr> {
        TrackedOutput {
            writer: io::stderr(),
            tracker: self.stderr.clone(),
        }
    }

    fn write_block(&self, text: &str) -> io::Result<()> {
        let mut output = self.error_output();
        output.ensure_newline()?;
        output.write_all(text.as_bytes())?;
        if !text.ends_with('\n') {
            output.write_all(b"\n")?;
        }
        output.flush()
    }

    fn write_reports<'a>(
        &self,
        reports: impl IntoIterator<Item = &'a Diagnostic>,
    ) -> io::Result<()> {
        let mut output = self.error_output();
        output.ensure_newline()?;
        write_reports(&mut output, reports, &RenderOptions::stderr())?;
        output.flush()
    }

    fn command_error(&self, command: &commands::CommandDef, message: &str) -> io::Result<()> {
        use crate::diagnostics::{context, error_with_code, help};
        let report = if let Some(usage) = message.strip_prefix("Usage: ") {
            let mut report = error_with_code(
                "repl/usage",
                format!("invalid arguments for {}", command.name),
            );
            help(&mut report, format!("use `{usage}`"));
            report
        } else {
            let mut report = error_with_code("repl/command", message);
            context(&mut report, command.name, None);
            report
        };
        self.write_reports([&report])
    }

    fn usage(&self, command: &commands::CommandDef) -> io::Result<()> {
        self.command_error(command, &format!("Usage: {}", command.usage))
    }

    fn report_error(&self, context: &str, error: &(dyn Error + 'static)) -> io::Result<()> {
        if let Some(error) = error.downcast_ref::<EvalError>() {
            if error.debugger_aborted() {
                return Ok(());
            }
            if error.kind() == EvalErrorKind::Cancelled
                && error.execution_stop() != Some(zkcl::ExecutionStop::BudgetExhausted)
            {
                return self.write_block(
                    &self
                        .theme
                        .paint("^C evaluation interrupted", self.theme.incomplete()),
                );
            }
        }
        let mut report = crate::diagnostics::from_error(context, error);
        if cfg!(feature = "asdf")
            && let Some(error) = error.downcast_ref::<EvalError>()
            && let Some("ASDF" | "UIOP") = error.missing_reader_package()
        {
            crate::diagnostics::help(
                &mut report,
                "load ASDF with `:require asdf`, then retry the form",
            );
        }
        self.write_reports([&report])
    }

    fn print_values(
        &mut self,
        runtime: &Runtime,
        values: &[Root<'_>],
        pretty: bool,
    ) -> Result<(), Box<dyn Error>> {
        let mut rendered = Vec::with_capacity(values.len());
        let mut print_errors = Vec::new();
        for (index, value) in values.iter().enumerate() {
            match classify_result_text(
                runtime.print_result_handling_errors(value, (!pretty).then_some(false)),
            )? {
                ResultText::Printed(text) => rendered.push(text),
                ResultText::PrinterError(error) => {
                    rendered.push(
                        runtime
                            .preview(value)
                            .unwrap_or_else(|_| "#<unavailable>".into()),
                    );
                    print_errors.push(crate::diagnostics::from_error(
                        &format!("while printing value {}", index + 1),
                        &error,
                    ));
                }
            }
        }
        let text = format_values_themed(&rendered, self.stdout.at_line_start(), &self.result_theme);
        runtime.record_dribble_text(&format_values_themed(
            &rendered,
            self.stdout.at_line_start(),
            &ReplTheme::new(false),
        ))?;
        {
            let mut output = self.standard_output();
            output.write_all(text.as_bytes())?;
            output.flush()?;
        }
        if !print_errors.is_empty() {
            self.write_reports(&print_errors)?;
        }
        Ok(())
    }
}

pub(super) enum ResultText {
    Printed(String),
    PrinterError(EvalError),
}

pub(super) fn classify_result_text(
    result: Result<String, EvalError>,
) -> Result<ResultText, EvalError> {
    match result {
        Ok(text) => Ok(ResultText::Printed(text)),
        Err(error) if error.is_control_transfer() || error.kind() == EvalErrorKind::Cancelled => {
            Err(error)
        }
        Err(error) => Ok(ResultText::PrinterError(error)),
    }
}

fn format_values_themed(values: &[String], at_line_start: bool, theme: &ReplTheme) -> String {
    let mut out = String::new();
    if !at_line_start {
        out.push('\n');
    }
    if values.is_empty() {
        out.push_str(&theme.paint("; no values", theme.muted()));
        out.push('\n');
        return out;
    }
    out.push_str(&theme.paint(RESULT_PREFIX, theme.result_prefix()));
    let mut lines = values.iter().flat_map(|value| value.split('\n'));
    if let Some(line) = lines.next() {
        out.push_str(line);
        for line in lines {
            out.push('\n');
            out.push_str(RESULT_INDENT);
            out.push_str(line);
        }
    }
    out.push('\n');
    out
}

#[cfg(test)]
fn format_values(values: &[String], at_line_start: bool, color: bool) -> String {
    format_values_themed(values, at_line_start, &ReplTheme::new(color))
}

pub(super) fn run(
    request: &super::args::ReplArgs,
    heap_size: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    let directory = env::current_dir()?;
    let project = super::project::repl_project(request, &directory);
    let asdf = request.system.as_ref().map(|system| AsdfRepl {
        system: system.clone(),
        directory: directory.clone(),
        definition: request.asd.as_ref().map(|path| directory.join(path)),
    });
    let startup_files: Vec<_> = request
        .load
        .iter()
        .map(|path| directory.join(path))
        .collect();
    let module_directories: Vec<_> = request
        .runtime
        .module_dir
        .iter()
        .map(|path| directory.join(path))
        .collect();
    let jit_request = request.runtime.jit_request();
    let jit = jit_request.create_backend()?;
    let jit_name = jit.as_ref().and_then(|_| jit_request.name());
    let interactive = !request.json && io::stdin().is_terminal() && io::stderr().is_terminal();
    let startup = ReplStartup {
        module_directories: &module_directories,
        startup_files: &startup_files,
        project: project.as_ref(),
        asdf,
        preload_asdf: request.asdf,
        package: request.package.as_deref(),
        active: Default::default(),
        heap_size,
        init_file: request.init_file().resolve(interactive)?,
        jit_backend: jit,
        jit_name,
    };
    if request.json {
        json::run(&startup)
    } else if interactive {
        run_interactive(&startup)
    } else {
        // Stdin belongs to the submitted source, including during startup.
        let runtime = startup.start(|runtime| {
            runtime.set_standard_input(io::empty());
            Ok(())
        })?;
        let input = io::read_to_string(io::stdin().lock())?;
        evaluate_and_print(&runtime, &input, &mut io::stdout().lock(), |_| {})
    }
}

#[derive(Clone)]
struct AsdfRepl {
    system: String,
    directory: PathBuf,
    definition: Option<PathBuf>,
}

impl AsdfRepl {
    fn load(self, runtime: &Runtime) -> Result<ActiveTarget, Box<dyn Error>> {
        let definition = zkcl::project::asdf::load_from_directory(
            runtime,
            &self.system,
            &self.directory,
            self.definition.as_deref(),
        )?;
        let mut description = format!("ASDF system {}", self.system);
        if let Some(definition) = definition {
            let _ = write!(description, " · {definition}");
        }
        Ok(ActiveTarget::Asdf {
            selection: self,
            description,
        })
    }
}

#[derive(Clone, Default)]
pub(super) enum InitFile {
    #[default]
    Default,
    Disabled,
    Explicit(PathBuf),
}

impl InitFile {
    fn resolve(self, terminal: bool) -> io::Result<Option<PathBuf>> {
        match self {
            Self::Disabled => Ok(None),
            Self::Explicit(path) => fs::canonicalize(path).map(Some),
            Self::Default if terminal => {
                let root = env::var_os("XDG_CONFIG_HOME")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .or_else(|| {
                        env::var_os("HOME")
                            .filter(|path| !path.is_empty())
                            .map(|home| PathBuf::from(home).join(".config"))
                    });
                root.map(|root| root.join("zkcl").join("repl.lisp"))
                    .map_or(Ok(None), |path| match fs::canonicalize(path) {
                        Ok(path) => Ok(Some(path)),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                        Err(error) => Err(error),
                    })
            }
            Self::Default => Ok(None),
        }
    }
}

/// Where one session rebuild spends its wall clock. `:reload` prints it in the
/// interactive REPL and the JSON protocol prints it on stderr, because neither
/// surface otherwise attributes the feedback loop.
#[derive(Clone, Copy, Default)]
struct SessionTimings {
    /// Project graph resolution, before any runtime exists.
    resolve: Duration,
    /// `Runtime::start`, I/O configuration and module directories.
    runtime: Duration,
    /// Init file, resolved project sources and startup files.
    load: Duration,
}

impl SessionTimings {
    fn report(self) -> String {
        format!(
            "resolve {} / runtime {} / load {}",
            format_duration(self.resolve),
            format_duration(self.runtime),
            format_duration(self.load),
        )
    }
}

/// A runtime built from the startup sources, together with the temporary
/// project output its loaded code came from. The directory must outlive every
/// value the runtime allocated from it.
struct ReplSession {
    runtime: Runtime,
    /// Target still to load before the session is published.
    plan: Option<ReplPlan>,
    resources: SessionResources,
    timings: SessionTimings,
}

enum ReplPlan {
    Native(Box<super::project::ResolvedRepl>),
    Asdf(AsdfRepl),
}

#[derive(Default)]
struct SessionResources {
    target: Option<ActiveTarget>,
    startup_loaded: bool,
}

enum ActiveTarget {
    Native {
        project: Box<ActiveProject>,
        description: String,
    },
    Asdf {
        selection: AsdfRepl,
        description: String,
    },
}

impl ActiveTarget {
    fn description(&self) -> &str {
        match self {
            Self::Native { description, .. } | Self::Asdf { description, .. } => description,
        }
    }
}

impl SessionResources {
    fn asdf_selection(&self) -> Option<&AsdfRepl> {
        match self.target.as_ref() {
            Some(ActiveTarget::Asdf { selection, .. }) => Some(selection),
            _ => None,
        }
    }

    fn sources(&self) -> impl Iterator<Item = &Path> {
        match &self.target {
            Some(ActiveTarget::Native { project, .. }) => Some(project.as_ref()),
            _ => None,
        }
        .into_iter()
        .flat_map(ActiveProject::sources)
    }
}

impl ReplSession {
    /// Load the init file, the resolved project and every startup source into
    /// this session. A failure leaves the active session untouched.
    fn load(
        mut self,
        startup: &ReplStartup<'_>,
        extra_files: &[PathBuf],
    ) -> Result<Self, Box<dyn Error>> {
        let started = Instant::now();
        if startup.preload_asdf || matches!(self.plan, Some(ReplPlan::Asdf(_))) {
            self.runtime.require_module("asdf")?;
        }
        if let Some(path) = &startup.init_file {
            self.runtime.load_init_file(path)?;
        }
        self.resources.target = match self.plan.take() {
            Some(ReplPlan::Native(plan)) => Some(ActiveTarget::Native {
                description: plan.description(),
                project: Box::new(plan.load(&self.runtime)?),
            }),
            Some(ReplPlan::Asdf(asdf)) => Some(asdf.load(&self.runtime)?),
            None => None,
        };
        for path in startup.startup_files.iter().chain(extra_files) {
            self.runtime.load_file(path)?;
        }
        let configured_package = match &self.resources.target {
            Some(ActiveTarget::Native { project, .. }) => project.package.as_deref(),
            _ => None,
        };
        if let Some(package) = startup.package.or(configured_package) {
            self.runtime.set_current_package(package).map_err(|error| {
                format!("cannot enter REPL package {package:?} after loading the session: {error}")
            })?;
        }
        self.resources.startup_loaded = true;
        self.timings.load = started.elapsed();
        Ok(self)
    }
}

struct ReplStartup<'a> {
    project: Option<&'a super::project::ReplProject>,
    asdf: Option<AsdfRepl>,
    preload_asdf: bool,
    package: Option<&'a str>,
    /// Resources change only after a candidate session is published.
    active: std::cell::RefCell<SessionResources>,
    module_directories: &'a [PathBuf],
    startup_files: &'a [PathBuf],
    init_file: Option<PathBuf>,
    heap_size: Option<usize>,
    jit_backend: Option<Arc<dyn JitBackend>>,
    jit_name: Option<&'static str>,
}

impl ReplStartup<'_> {
    fn load_system(&self, runtime: &Runtime, name: &str) -> Result<String, Box<dyn Error>> {
        let already_selected = match &self.active.borrow().target {
            Some(ActiveTarget::Native { .. }) => true,
            Some(ActiveTarget::Asdf { selection, .. }) => selection.system == name,
            None => false,
        };
        if already_selected {
            zkcl::project::asdf::load(runtime, name)?;
            return Ok(format!("ASDF system {name}"));
        }

        let target = AsdfRepl {
            system: name.to_owned(),
            directory: runtime.working_directory()?,
            definition: None,
        }
        .load(runtime)?;
        let description = target.description().to_owned();
        self.active.borrow_mut().target = Some(target);
        Ok(description)
    }

    fn refresh(&self, runtime: &Runtime, report: &mut RefreshReport) -> Result<(), Box<dyn Error>> {
        match &mut self.active.borrow_mut().target {
            Some(ActiveTarget::Native { project, .. }) => project.refresh(runtime, report),
            Some(ActiveTarget::Asdf { selection, .. }) => {
                report.remaining = 1;
                zkcl::project::asdf::load(runtime, &selection.system)?;
                report.systems.push(selection.system.clone());
                report.remaining = 0;
                Ok(())
            }
            None => Err("no project is loaded. Start a project REPL or use :load for individual files".into()),
        }
    }

    /// A runtime with its I/O configured and module directories registered,
    /// but no startup sources loaded. `:reload` builds its cancellable
    /// candidate in this state so a cancelled request loads nothing.
    fn prepare(
        &self,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
    ) -> Result<ReplSession, Box<dyn Error>> {
        let started = Instant::now();
        let asdf = self.active.borrow().asdf_selection().cloned();
        let plan = match asdf.or_else(|| self.asdf.clone()) {
            Some(asdf) => Some(ReplPlan::Asdf(asdf)),
            None => self
                .project
                .map(super::project::ReplProject::resolve)
                .transpose()?
                .map(|project| ReplPlan::Native(Box::new(project))),
        };
        let resolve = started.elapsed();
        let mut session = self.new_session(plan, configure_io)?;
        session.timings.resolve = resolve;
        Ok(session)
    }

    /// An empty session for `:reset`: no project plan and no startup sources.
    fn prepare_bare(
        &self,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
    ) -> Result<ReplSession, Box<dyn Error>> {
        self.new_session(None, configure_io)
    }

    /// A runtime with no startup sources, used when restoring a saved image.
    fn start_bare(
        &self,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
    ) -> Result<Runtime, Box<dyn Error>> {
        Ok(self.prepare_bare(configure_io)?.runtime)
    }

    fn new_session(
        &self,
        plan: Option<ReplPlan>,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
    ) -> Result<ReplSession, Box<dyn Error>> {
        let started = Instant::now();
        let mut runtime = super::session_runtime(self.jit_backend.as_ref(), self.heap_size)?;
        configure_io(&mut runtime)?;
        super::add_module_directories(&runtime, self.module_directories)?;
        Ok(ReplSession {
            runtime,
            plan,
            resources: SessionResources::default(),
            timings: SessionTimings {
                runtime: started.elapsed(),
                ..SessionTimings::default()
            },
        })
    }

    fn build(
        &self,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
        extra_files: &[PathBuf],
    ) -> Result<ReplSession, Box<dyn Error>> {
        self.prepare(configure_io)?.load(self, extra_files)
    }

    /// Commit a fresh session: retain its FASL directory for as long as its
    /// runtime lives and hand the runtime back to the caller.
    fn commit(&self, session: ReplSession) -> Runtime {
        let ReplSession {
            runtime, resources, ..
        } = session;
        *self.active.borrow_mut() = resources;
        runtime
    }

    /// Swap a built session into `active`. The previous runtime is dropped
    /// before the directory its code was loaded from is replaced.
    fn swap_in(&self, active: &mut Runtime, session: ReplSession) {
        let ReplSession {
            runtime, resources, ..
        } = session;
        *active = runtime;
        *self.active.borrow_mut() = resources;
    }

    fn replace_with_image(&self, active: &mut Runtime, candidate: Runtime) {
        *active = candidate;
        *self.active.borrow_mut() = SessionResources::default();
    }

    fn start(
        &self,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
    ) -> Result<Runtime, Box<dyn Error>> {
        let session = self.build(configure_io, &[])?;
        Ok(self.commit(session))
    }

    /// The session shared by startup and `:reload`: interactive I/O plus the
    /// debugger and inspector tools.
    fn interactive(
        &self,
        printer: &ReplPrinter,
        extra_files: &[PathBuf],
    ) -> Result<ReplSession, Box<dyn Error>> {
        let session = self.build(
            |runtime| configure_interactive_io(runtime, printer),
            extra_files,
        )?;
        configure_interactive_tools(&session.runtime, printer)?;
        Ok(session)
    }

    fn start_with_debugger(
        &self,
        printer: &ReplPrinter,
        extra_files: &[PathBuf],
    ) -> Result<Runtime, Box<dyn Error>> {
        Ok(self.commit(self.interactive(printer, extra_files)?))
    }

    /// Rebuild the session and swap it in. Returns where the rebuild spent its
    /// wall clock, so both REPL surfaces can attribute the feedback loop.
    fn reload_with_debugger(
        &self,
        printer: &ReplPrinter,
        extra_files: &[PathBuf],
        runtime: &mut Runtime,
    ) -> Result<SessionTimings, Box<dyn Error>> {
        let session = self.interactive(printer, extra_files)?;
        let timings = session.timings;
        self.swap_in(runtime, session);
        Ok(timings)
    }

    fn reset_with_debugger(
        &self,
        printer: &ReplPrinter,
        runtime: &mut Runtime,
    ) -> Result<(), Box<dyn Error>> {
        let session = self.prepare_bare(|runtime| {
            configure_interactive_io(runtime, printer)?;
            configure_interactive_tools(runtime, printer)
        })?;
        self.swap_in(runtime, session);
        Ok(())
    }
}

fn configure_interactive_io(runtime: &mut Runtime, printer: &ReplPrinter) -> Result<(), EvalError> {
    runtime.set_interactive_input(terminal_input::TerminalInput::new(
        runtime.cancellation_handle(),
    ));
    runtime.set_standard_output(printer.standard_output());
    runtime.set_error_output(printer.error_output());
    // Reedline and the debugger both need to remain visible when Lisp output
    // is redirected to a file or a pipe.
    runtime.eval("(CL:SETQ CL:*PRINT-PRETTY* CL:T
                           CL:*DEBUG-IO* (CL:MAKE-TWO-WAY-STREAM CL:*STANDARD-INPUT* CL:*ERROR-OUTPUT*))")?;
    Ok(())
}

fn configure_interactive_tools(runtime: &Runtime, printer: &ReplPrinter) -> Result<(), EvalError> {
    let source = debugger::restart_menu_source(printer.theme.color);
    let menu = runtime.eval(&source)?;
    runtime.set_debugger(Some(&menu[0]))?;
    inspector::install_terminal(runtime)?;
    runtime.set_interactive_compiler_feedback(true);
    Ok(())
}

struct EvalInterruptGuard {
    active: Arc<AtomicBool>,
    handle: zkcl::CancellationHandle,
    pause: zkcl::PauseHandle,
}

impl EvalInterruptGuard {
    fn enter(active: &Arc<AtomicBool>, interrupts: &Arc<AtomicUsize>, runtime: &Runtime) -> Self {
        let handle = runtime.cancellation_handle();
        let pause = runtime.pause_handle();
        handle.reset();
        pause.reset();
        interrupts.store(0, Ordering::Release);
        active.store(true, Ordering::Release);
        Self {
            active: Arc::clone(active),
            handle,
            pause,
        }
    }
}

impl Drop for EvalInterruptGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.handle.reset();
        self.pause.reset();
    }
}

struct InterruptHandles {
    cancellation: zkcl::CancellationHandle,
    pause: zkcl::PauseHandle,
}

impl InterruptHandles {
    fn new(runtime: &Runtime) -> Self {
        Self {
            cancellation: runtime.cancellation_handle(),
            pause: runtime.pause_handle(),
        }
    }
}

#[cfg(unix)]
fn install_sigint_watcher(
    runtime: &Runtime,
    eval_active: Arc<AtomicBool>,
    interrupts: Arc<AtomicUsize>,
) -> Option<Arc<std::sync::Mutex<InterruptHandles>>> {
    let handle_slot = Arc::new(std::sync::Mutex::new(InterruptHandles::new(runtime)));
    let slot_for_thread = Arc::clone(&handle_slot);
    if let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT]) {
        let _ = std::thread::Builder::new()
            .name("zkcl-repl-sigint".into())
            .spawn(move || {
                for _ in signals.forever() {
                    if eval_active.load(Ordering::Acquire) {
                        let count = interrupts.fetch_add(1, Ordering::AcqRel) + 1;
                        if let Ok(handle) = slot_for_thread.lock() {
                            if count == 1 && !terminal_input::waiting_for_line() {
                                handle.pause.pause();
                            } else if count <= 2 {
                                handle.cancellation.cancel_with_cleanup();
                            } else {
                                handle.cancellation.cancel();
                            }
                        }
                    }
                }
            });
        Some(handle_slot)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn install_sigint_watcher(
    _runtime: &Runtime,
    _eval_active: Arc<AtomicBool>,
    _interrupts: Arc<AtomicUsize>,
) -> Option<Arc<std::sync::Mutex<InterruptHandles>>> {
    None
}

/// Mirrors the sources a session loads into the editor workspace, so completion
/// and diagnostics see the same files the runtime does. The init file is always
/// indexed; startup and `:load` files contribute only when they hold Lisp source.
fn session_source_paths<'a>(
    startup: &'a ReplStartup<'_>,
    active: &'a SessionResources,
    extra_files: &'a [PathBuf],
) -> impl Iterator<Item = &'a Path> {
    startup.init_file.as_deref().into_iter().chain(
        startup
            .startup_files
            .iter()
            .chain(extra_files)
            .filter(|path| crate::analysis::is_lisp_source(path))
            .map(PathBuf::as_path),
    ).chain(active.sources())
}

fn run_interactive(startup: &ReplStartup<'_>) -> Result<(), Box<dyn Error>> {
    let use_color = zkcl_terminal::use_color(Stream::Stderr);
    let mut printer = ReplPrinter::new(
        use_color,
        zkcl_terminal::use_color(Stream::Stdout),
        io::stdout().is_terminal(),
    );
    let mut user_loaded_files: Vec<PathBuf> = Vec::new();
    let mut runtime = startup.start_with_debugger(&printer, &user_loaded_files)?;
    printer.error_output().ensure_newline()?;
    if let Some(target) = &startup.active.borrow().target {
        printer.write_block(&format!("Loaded {}", target.description()))?;
    }

    let analysis = EditorAnalysis::with_color(runtime.current_package(), use_color);
    analysis.set_jit_active(startup.jit_name.is_some());
    if let Some(selection) = startup.active.borrow().asdf_selection() {
        analysis.remember_systems([selection.system.clone()]);
    }
    for path in session_source_paths(startup, &startup.active.borrow(), &user_loaded_files) {
        analysis.index_file(path);
    }
    analysis.refresh_runtime_snapshot(&runtime);

    let eval_active = Arc::new(AtomicBool::new(false));
    let interrupt_count = Arc::new(AtomicUsize::new(0));
    let cancel_slot = install_sigint_watcher(
        &runtime,
        Arc::clone(&eval_active),
        Arc::clone(&interrupt_count),
    );

    let mut editor = analysis.editor(use_color);

    loop {
        let prompt = analysis.prompt();
        let read = if printer.line_editing {
            editor.read_line(&prompt)
        } else {
            read_plain_submission(&analysis, &prompt, &printer, &mut editor)
        };
        match read {
            Ok(Signal::Success(input)) => {
                printer.stderr.set_at_line_start(true);
                if input.trim().is_empty() {
                    continue;
                }
                if let Some(parsed) = parse_command(&input) {
                    let command = parsed.spec.name;
                    runtime.record_dribble_text(&format!("{input}\n"))?;
                    match handle_repl_command(
                        parsed,
                        startup,
                        &mut runtime,
                        &analysis,
                        &mut editor,
                        &mut printer,
                        &mut user_loaded_files,
                        &eval_active,
                        &interrupt_count,
                        cancel_slot.as_ref(),
                        use_color,
                    ) {
                        Ok(LoopControl::Continue) => {}
                        Ok(LoopControl::Exit) => return Ok(()),
                        Err(error) => printer.report_error(command, error.as_ref())?,
                    }
                    analysis.refresh_runtime_snapshot(&runtime);
                    continue;
                }

                let _interrupt_guard =
                    EvalInterruptGuard::enter(&eval_active, &interrupt_count, &runtime);
                let started = Instant::now();
                let res = submit_interactive(&runtime, &analysis, &input, &mut printer);
                let elapsed = started.elapsed();
                drop(_interrupt_guard);

                analysis.set_last_duration(Some(elapsed));
                analysis.refresh_runtime_snapshot(&runtime);
                if let Err(error) = res {
                    printer.report_error("", error.as_ref())?;
                } else if analysis.settings().timing == commands::TimingMode::On {
                    printer.write_block(&printer.theme.paint(
                        &format!("  time {}", format_duration(elapsed)),
                        printer.theme.muted(),
                    ))?;
                }
            }
            Ok(Signal::CtrlD) => return Ok(()),
            Ok(Signal::CtrlC(_)) => continue,
            Ok(Signal::HostCommand(command)) if command == "help" => {
                let help = render_help(&printer.theme, None, printer.line_editing);
                printer.write_block(&help)?;
                continue;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}

fn read_plain_submission(
    analysis: &EditorAnalysis,
    prompt: &impl Prompt,
    printer: &ReplPrinter,
    editor: &mut reedline::Reedline,
) -> io::Result<Signal> {
    let mut output = printer.error_output();
    output.ensure_newline()?;
    write!(
        output,
        "{}{}",
        prompt.render_prompt_left(),
        prompt.render_prompt_indicator(PromptEditMode::Default)
    )?;
    output.flush()?;
    let mut input = String::new();
    loop {
        if io::stdin().read_line(&mut input)? == 0 {
            output.ensure_newline()?;
            return Ok(Signal::CtrlD);
        }
        output.tracker.set_at_line_start(true);
        if !matches!(analysis.validate(&input), ValidationResult::Incomplete) {
            break;
        }
        output.write_all(b"... ")?;
        output.flush()?;
    }
    if !input.trim().is_empty() {
        let item = HistoryItem::from_command_line(input.trim_end_matches(['\r', '\n']));
        if let Err(error) = editor.history_mut().save(item) {
            printer.report_error("while saving input history", &error)?;
        }
    }
    Ok(Signal::Success(input))
}

enum LoopControl {
    Continue,
    Exit,
}

#[allow(clippy::too_many_arguments)]
fn handle_repl_command(
    parsed: ParsedCommand<'_>,
    startup: &ReplStartup<'_>,
    runtime: &mut Runtime,
    analysis: &EditorAnalysis,
    editor: &mut reedline::Reedline,
    printer: &mut ReplPrinter,
    user_loaded_files: &mut Vec<PathBuf>,
    eval_active: &Arc<AtomicBool>,
    interrupt_count: &Arc<AtomicUsize>,
    cancel_slot: Option<&Arc<std::sync::Mutex<InterruptHandles>>>,
    use_color: bool,
) -> Result<LoopControl, Box<dyn Error>> {
    let theme = printer.theme;
    let spec = parsed.spec;
    let args = parsed.lisp_args().unwrap_or_else(|| parsed.args.trim_end());
    if spec.takes_lisp_form && args.is_empty() {
        printer.usage(spec)?;
        return Ok(LoopControl::Continue);
    }

    match spec.name {
        ":quit" => return Ok(LoopControl::Exit),
        ":help" => {
            let help = if args.is_empty() {
                render_help(&theme, None, printer.line_editing)
            } else if let Some(command) = commands::find_help_command(args) {
                render_help(&theme, Some(command), printer.line_editing)
            } else {
                commands::source_documentation(
                    &mut analysis.lock_source().workspace,
                    runtime.current_package(),
                    args,
                    &theme,
                )
                .unwrap_or_else(|| {
                    format!(
                        "No help found for {}. Use :help to list commands.",
                        zkcl_terminal::single_line(args)
                    )
                })
            };
            printer.write_block(&help)?;
        }
        ":clear" => {
            if !printer.line_editing {
                printer.command_error(
                    spec,
                    "screen clearing requires an interactive terminal on stdout",
                )?;
            } else if let Err(error) = editor.clear_scrollback() {
                printer.report_error(spec.name, &error)?;
            }
        }
        ":edit" => match edit_command(args, printer) {
            Ok(Some(content)) => {
                let interrupt = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
                let started = Instant::now();
                let result = submit_interactive(runtime, analysis, &content, printer);
                analysis.set_last_duration(Some(started.elapsed()));
                drop(interrupt);
                if let Err(error) = result {
                    printer.report_error(spec.name, error.as_ref())?;
                }
            }
            Ok(None) => {}
            Err(error) => printer.report_error(spec.name, error.as_ref())?,
        },
        ":env" => {
            let active = startup.active.borrow();
            let mut all_files = if active.startup_loaded {
                startup.startup_files.to_vec()
            } else {
                Vec::new()
            };
            all_files.extend(user_loaded_files.iter().cloned());
            let text = render_env(
                &theme,
                runtime,
                &analysis.settings(),
                startup.jit_name,
                &all_files,
                active.target.as_ref().map(ActiveTarget::description),
            );
            printer.write_block(&text)?;
        }
        ":set" => {
            let mut settings = analysis.settings();
            match apply_setting(&mut settings, args, &theme) {
                Ok((msg, needs_rebuild)) => {
                    analysis.set_settings(settings);
                    if needs_rebuild {
                        *editor = analysis.editor(use_color);
                    }
                    printer.write_block(&msg)?;
                }
                Err(err) => {
                    printer.command_error(spec, &err)?;
                }
            }
        }
        ":history" => {
            if let Err(error) = handle_history_command(editor, args, printer, &theme) {
                printer.report_error(spec.name, error.as_ref())?;
            }
        }
        ":refresh" => {
            if !args.is_empty() {
                printer.usage(spec)?;
                return Ok(LoopControl::Continue);
            }
            let interrupt = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let mut report = RefreshReport::default();
            let result = startup.refresh(runtime, &mut report);
            drop(interrupt);
            for file in &report.files {
                analysis.index_file(file);
                printer.write_block(&format!("Reloaded {}", file.display()))?;
            }
            for system in &report.systems {
                printer.write_block(&format!("Loaded ASDF system {system}"))?;
            }
            if let Err(error) = result {
                if report.remaining > 0 {
                    printer.write_block(&format!("{} components did not finish; the current session may contain partial changes.", report.remaining))?;
                }
                printer.report_error(spec.name, error.as_ref())?;
            } else if report.files.is_empty() && report.systems.is_empty() {
                printer.write_block("No project files changed.")?;
            }
        }
        ":reload" => {
            if startup.startup_files.is_empty()
                && startup.project.is_none()
                && startup.asdf.is_none()
                && startup.active.borrow().asdf_selection().is_none()
                && !startup.preload_asdf
                && startup.init_file.is_none()
                && user_loaded_files.is_empty()
            {
                printer.write_block(&theme.paint(
                    "Nothing is configured for reload. Use :reset for a clean session.",
                    theme.muted(),
                ))?;
                return Ok(LoopControl::Continue);
            }
            let reload_started = Instant::now();
            match startup.reload_with_debugger(printer, user_loaded_files, runtime) {
                Ok(timings) => {
                    let built = reload_started.elapsed();
                    if let Some(slot) = cancel_slot
                        && let Ok(mut guard) = slot.lock()
                    {
                        *guard = InterruptHandles::new(runtime);
                    }
                    analysis.reload_files(
                        runtime.current_package(),
                        session_source_paths(startup, &startup.active.borrow(), user_loaded_files),
                    );
                    if let Some(selection) = startup.active.borrow().asdf_selection() {
                        analysis.remember_systems([selection.system.clone()]);
                    }
                    let reindex = reload_started.elapsed().saturating_sub(built);
                    printer.write_block(&format!(
                        "{} {}",
                        theme.paint("Session reloaded cleanly.", theme.result_prefix()),
                        theme.paint(
                            &format!(
                                "({} / reindex {})",
                                timings.report(),
                                format_duration(reindex)
                            ),
                            theme.muted(),
                        ),
                    ))?;
                }
                Err(error) => {
                    printer.report_error("while reloading the session", error.as_ref())?;
                }
            }
        }
        ":reset" => match startup.reset_with_debugger(printer, runtime) {
            Ok(()) => {
                user_loaded_files.clear();
                if let Some(slot) = cancel_slot
                    && let Ok(mut guard) = slot.lock()
                {
                    *guard = InterruptHandles::new(runtime);
                }
                analysis.reset(runtime.current_package());
                printer.write_block(
                    &theme.paint("Session reset to a clean runtime.", theme.result_prefix()),
                )?;
            }
            Err(error) => {
                printer.report_error("while resetting the session", error.as_ref())?;
            }
        },
        ":package" => match run_package_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => printer.command_error(spec, &err)?,
        },
        ":packages" => {
            let text = run_packages_command(runtime, &theme);
            printer.write_block(&text)?;
        }
        ":require" | ":system" => {
            let is_system = spec.name == ":system";
            let name = unquote_path(args);
            let selected = if is_system && name.is_empty() {
                startup
                    .active
                    .borrow()
                    .asdf_selection()
                    .map(|selection| selection.system.clone())
            } else {
                None
            };
            let name = selected.as_deref().unwrap_or(name);
            if name.is_empty() {
                printer.usage(spec)?;
                return Ok(LoopControl::Continue);
            }
            let _interrupt_guard = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let result: Result<String, Box<dyn Error>> = if is_system {
                startup.load_system(runtime, name)
            } else {
                runtime
                    .require_module(name)
                    .map(|()| name.to_owned())
                    .map_err(Into::into)
            };
            match result {
                Ok(loaded) => {
                    if is_system {
                        analysis.remember_systems([name.to_owned()]);
                    }
                    printer.write_block(&format!("Loaded {loaded}"))?;
                }
                Err(error) => {
                    printer.report_error(spec.name, error.as_ref())?;
                }
            }
        }
        ":modules" | ":systems" => {
            let _interrupt_guard = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let (kind, names) = if spec.name == ":modules" {
                (
                    "provided modules",
                    runtime.loaded_modules().map_err(Into::into),
                )
            } else if runtime
                .lookup_symbol("ASDF", "REGISTERED-SYSTEMS")
                .is_ok_and(|symbol| symbol.is_some())
            {
                (
                    "registered systems",
                    zkcl::project::asdf::registered_systems(runtime),
                )
            } else {
                printer.write_block("Load an ASDF system with :system <name>.")?;
                return Ok(LoopControl::Continue);
            };
            match names {
                Ok(names) if names.is_empty() => printer.write_block(&format!("No {kind}."))?,
                Ok(names) => {
                    printer.write_block(&names.join("\n"))?;
                    if spec.name == ":systems" {
                        analysis.remember_systems(names);
                    }
                }
                Err(error) => printer.report_error(spec.name, error.as_ref())?,
            }
        }
        ":load" => {
            if args.is_empty() {
                printer.usage(spec)?;
                return Ok(LoopControl::Continue);
            }
            let path = match command_path(args) {
                Ok(path) => path,
                Err(error) => {
                    printer.command_error(spec, error)?;
                    return Ok(LoopControl::Continue);
                }
            };
            let path = match std::path::absolute(path) {
                Ok(path) => path,
                Err(error) => {
                    printer.report_error(spec.name, &error)?;
                    return Ok(LoopControl::Continue);
                }
            };
            let _interrupt_guard = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let t0 = Instant::now();
            let res = runtime.load_file(&path);
            let elapsed = t0.elapsed();
            drop(_interrupt_guard);
            match res {
                Ok(()) => {
                    if !user_loaded_files.contains(&path) {
                        user_loaded_files.push(path.clone());
                    }
                    if crate::analysis::is_lisp_source(&path) {
                        analysis.index_file(&path);
                    }
                    analysis.set_last_duration(Some(elapsed));
                    printer.write_block(&theme.paint(
                        &format!(
                            "Loaded {} ({})",
                            zkcl_terminal::display_path(&path),
                            format_duration(elapsed)
                        ),
                        theme.muted(),
                    ))?;
                }
                Err(error) => {
                    printer.report_error(spec.name, &error)?;
                }
            }
        }
        ":compile" => {
            let (source, output) = match compile_paths(args) {
                Ok(paths) => paths,
                Err(message) => {
                    printer.command_error(spec, message)?;
                    return Ok(LoopControl::Continue);
                }
            };
            let _interrupt_guard = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let t0 = Instant::now();
            match runtime.compile_file(&source, output.as_deref()) {
                Ok(compiled) => {
                    printer.write_block(&theme.paint(
                        &format!(
                            "Compiled {} -> {} ({})",
                            zkcl_terminal::display_path(&source),
                            zkcl_terminal::display_path(&compiled),
                            format_duration(t0.elapsed())
                        ),
                        theme.muted(),
                    ))?;
                }
                Err(error) => {
                    printer.report_error(spec.name, &error)?;
                }
            }
        }
        ":pwd" => match env::current_dir() {
            Ok(cwd) => printer.write_block(&zkcl_terminal::single_line(&cwd.to_string_lossy()))?,
            Err(error) => printer.report_error(spec.name, &error)?,
        },
        ":cd" => match run_cd_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(error) => printer.report_error(spec.name, error.as_ref())?,
        },
        ":doc" => {
            let mut state = analysis.lock_source();
            match run_doc_command(runtime, &mut state.workspace, args, &theme) {
                Ok(msg) => {
                    drop(state);
                    printer.write_block(&msg)?;
                }
                Err(err) => printer.command_error(spec, &err)?,
            }
        }
        ":apropos" => match run_apropos_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => printer.command_error(spec, &err)?,
        },
        ":describe" | ":macroexpand" | ":macroexpand-1" | ":type" => {
            let _interrupt_guard = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let result = match spec.name {
                ":describe" => run_describe_command(runtime, args, &theme),
                ":type" => {
                    run_type_command(runtime, analysis.static_expression_type(args), args, &theme)
                }
                _ => run_macroexpand_command(runtime, args, spec.name == ":macroexpand-1", &theme),
            };
            match result {
                Ok(message) => printer.write_block(&message)?,
                Err(error) => printer.report_error(spec.name, &error)?,
            }
        }
        ":disassemble" => match run_disassemble_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => printer.command_error(spec, &err)?,
        },
        ":break" => match debugger::add_breakpoint(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => printer.command_error(spec, &error)?,
        },
        ":break-file" => match debugger::add_file_breakpoint(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => printer.command_error(spec, &error)?,
        },
        ":break-locations" => match debugger::executable_locations(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => printer.command_error(spec, &error)?,
        },
        ":breakpoints" => printer.write_block(&debugger::list_breakpoints(runtime))?,
        ":enable-breakpoint" | ":disable-breakpoint" => match args.parse::<u64>() {
            Ok(id) => match runtime.enable_breakpoint(id, spec.name == ":enable-breakpoint") {
                Ok(breakpoint) => printer.write_block(&debugger::format_breakpoint(&breakpoint))?,
                Err(error) => printer.report_error(spec.name, &error)?,
            },
            Err(_) => printer.usage(spec)?,
        },
        ":save-state" => match save_state_command(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => printer.report_error(spec.name, error.as_ref())?,
        },
        ":save-image" => {
            if args.is_empty() {
                printer.usage(spec)?;
            } else {
                let path = match command_path(args) {
                    Ok(path) => path,
                    Err(error) => {
                        printer.command_error(spec, error)?;
                        return Ok(LoopControl::Continue);
                    }
                };
                let hooks = zkcl::NamedImageHooks::new(&[("ZK", "%REPL-INSPECT")]);
                match runtime.save_image_with_hooks(&path, &hooks) {
                    Ok(()) => printer.write_block(&format!(
                        "Saved image to {}.",
                        zkcl_terminal::display_path(&path)
                    ))?,
                    Err(error) => printer.report_error(spec.name, &error)?,
                }
            }
        }
        ":restore-image" => {
            if args.is_empty() {
                printer.usage(spec)?;
            } else {
                let path = match command_path(args) {
                    Ok(path) => path,
                    Err(error) => {
                        printer.command_error(spec, error)?;
                        return Ok(LoopControl::Continue);
                    }
                };
                let restored = (|| -> Result<Runtime, Box<dyn Error>> {
                    let mut candidate =
                        super::session_runtime(startup.jit_backend.as_ref(), startup.heap_size)?;
                    configure_interactive_io(&mut candidate, printer)?;
                    configure_interactive_tools(&candidate, printer)?;
                    let hooks = zkcl::NamedImageHooks::new(&[("ZK", "%REPL-INSPECT")]);
                    candidate.restore_image_with_hooks(&path, &hooks)?;
                    configure_interactive_io(&mut candidate, printer)?;
                    configure_interactive_tools(&candidate, printer)?;
                    Ok(candidate)
                })();
                match restored {
                    Ok(candidate) => {
                        startup.replace_with_image(runtime, candidate);
                        user_loaded_files.clear();
                        if let Some(slot) = cancel_slot
                            && let Ok(mut guard) = slot.lock()
                        {
                            *guard = InterruptHandles::new(runtime);
                        }
                        analysis.reset(runtime.current_package());
                        printer.write_block(&format!(
                            "Restored image from {}.",
                            zkcl_terminal::display_path(&path)
                        ))?;
                    }
                    Err(error) => printer.report_error(spec.name, error.as_ref())?,
                }
            }
        }
        ":dribble" => {
            let source = if args.is_empty() {
                "(CL:DRIBBLE)".to_owned()
            } else {
                let path = match command_path(args) {
                    Ok(path) => path,
                    Err(error) => {
                        printer.command_error(spec, error)?;
                        return Ok(LoopControl::Continue);
                    }
                };
                format!(
                    "(CL:DRIBBLE {})",
                    snapshot::lisp_string_literal(&path.to_string_lossy())
                )
            };
            match runtime.eval(&source) {
                Ok(_) => printer.write_block(if args.is_empty() {
                    "Transcript closed."
                } else {
                    "Transcript recording started."
                })?,
                Err(error) => printer.report_error(spec.name, &error)?,
            }
        }
        ":profile" => match profile_command(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => printer.command_error(spec, &error)?,
        },
        ":definition" | ":source" => {
            match definition_command(runtime, args, spec.name == ":source") {
                Ok(message) => printer.write_block(&message)?,
                Err(error) => printer.command_error(spec, &error)?,
            }
        }
        ":delete-breakpoint" => match args.parse() {
            Ok(id) if runtime.remove_breakpoint(id) => {
                printer.write_block(&format!("Deleted breakpoint {id}."))?
            }
            Ok(_) => printer.command_error(spec, "no such breakpoint")?,
            Err(_) => printer.usage(spec)?,
        },
        ":time" => {
            let _interrupt_guard = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let res = run_time_command(runtime, analysis, args, printer);
            drop(_interrupt_guard);
            if let Err(error) = res {
                printer.report_error(spec.name, error.as_ref())?;
            }
        }
        ":trace" => match run_trace_command(runtime, true, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => printer.command_error(spec, &err)?,
        },
        ":untrace" => match run_trace_command(runtime, false, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => printer.command_error(spec, &err)?,
        },
        ":room" => {
            let text = run_room_command(runtime, &theme);
            printer.write_block(&text)?;
        }
        ":gc" => {
            let text = run_gc_command(runtime, &theme);
            printer.write_block(&text)?;
        }
        ":lint" | ":fmt" => {
            if args
                .strip_prefix('@')
                .is_some_and(|path| path.trim().is_empty())
            {
                printer.usage(spec)?;
            } else {
                analyze_source_command(runtime, args, spec.name == ":fmt", printer)?;
            }
        }
        _ => {}
    }
    Ok(LoopControl::Continue)
}

fn edit_command(args: &str, printer: &ReplPrinter) -> Result<Option<String>, Box<dyn Error>> {
    if !args.is_empty() {
        let path = command_path(args)?;
        let status = editor::launch_external_editor(&path)?;
        if !status.success() {
            return Err(format!("editor exited with {status}").into());
        }
        printer.write_block(&format!("Edited {}", zkcl_terminal::display_path(&path)))?;
        return Ok(None);
    }

    let mut scratch = tempfile::Builder::new()
        .prefix("zkcl-scratch-")
        .suffix(".lisp")
        .tempfile()?;
    let initial = ";; Write Common Lisp forms, then save and exit to evaluate.\n";
    scratch.write_all(initial.as_bytes())?;
    let status = editor::launch_external_editor(scratch.path())?;
    if !status.success() {
        return Err(format!("editor exited with {status}").into());
    }
    let content = fs::read_to_string(scratch.path())?;
    if content.trim().is_empty() || content.trim() == initial.trim() {
        printer.write_block("Scratch buffer has no forms to evaluate.")?;
        return Ok(None);
    }
    Ok(Some(content))
}

fn definition_command(runtime: &Runtime, args: &str, show_source: bool) -> Result<String, String> {
    if args.is_empty() {
        return Err(format!(
            "Usage: :{} <name>",
            if show_source { "source" } else { "definition" }
        ));
    }
    let definitions = runtime
        .find_definitions(args)
        .map_err(|error| error.to_string())?;
    if definitions.is_empty() {
        return Ok(format!("No running definition found for {args}."));
    }
    let mut output = String::new();
    for definition in definitions {
        let _ = writeln!(output, "{}  {:?}", definition.name, definition.kind);
        if let Some(source) = &definition.source {
            let location = source
                .path()
                .map(zkcl_terminal::display_path)
                .or_else(|| source.uri().map(str::to_owned))
                .unwrap_or_else(|| format!("submission {}", source.id()));
            let range = source
                .absolute_range(definition.byte_range.clone())
                .unwrap_or_else(|| definition.byte_range.clone());
            if let Some(position) = source.position(definition.byte_range.start) {
                let _ = writeln!(
                    output,
                    "  {location}:{}:{} [bytes {}..{}]",
                    position.line + 1,
                    position.column + 1,
                    range.start,
                    range.end
                );
            } else {
                let _ = writeln!(
                    output,
                    "  {location} [bytes {}..{}]",
                    range.start, range.end
                );
            }
            if show_source {
                if let Some(text) = source.text().and_then(|text| {
                    if definition.byte_range.is_empty() {
                        let offset = definition.byte_range.start;
                        let start = text.get(..offset)?.rfind('\n').map_or(0, |index| index + 1);
                        let end = text
                            .get(offset..)?
                            .find('\n')
                            .map_or(text.len(), |index| offset + index);
                        text.get(start..end)
                    } else {
                        text.get(definition.byte_range.clone())
                    }
                }) {
                    let _ = writeln!(output, "{text}");
                } else {
                    let _ = writeln!(output, "  Source text is unavailable.");
                }
            }
        } else {
            let _ = writeln!(output, "  Source location is unavailable.");
        }
        if !show_source {
            if let Some(signature) = definition.lambda_list {
                let _ = writeln!(output, "  {signature}");
            }
            if let Some(declared_type) = definition.declared_type {
                let _ = writeln!(output, "  Type: {declared_type}");
            }
            if let Some(documentation) = definition.documentation {
                let _ = writeln!(output, "  {documentation}");
            }
        }
    }
    Ok(output)
}

fn profile_command(runtime: &Runtime, args: &str) -> Result<String, String> {
    const USAGE: &str = "Usage: :profile [start [milliseconds] | stop | report | reset]";
    let mut words = args.split_whitespace();
    let action = words.next().unwrap_or("report");
    let report = match action {
        "start" => {
            let interval = words
                .next()
                .map(str::parse::<u64>)
                .transpose()
                .map_err(|_| USAGE.to_owned())?
                .unwrap_or(1);
            if interval == 0 || words.next().is_some() {
                return Err(USAGE.into());
            }
            runtime
                .start_profiling(zkcl::ProfilerConfig {
                    sample_interval: std::time::Duration::from_millis(interval),
                })
                .map_err(|error| error.to_string())?;
            return Ok(format!(
                "Profiling started (safe-point wall-clock samples every {interval} ms)."
            ));
        }
        "stop" if words.next().is_none() => runtime.stop_profiling(),
        "report" if words.next().is_none() => runtime.profile_report(),
        "reset" if words.next().is_none() => {
            runtime.reset_profiling();
            return Ok("Profile measurements reset.".into());
        }
        _ => return Err(USAGE.into()),
    };
    let mut output = format!(
        "Profile {} — {} elapsed; {} samples ({})\nAllocation: {} bytes; GC: {} collections, {}\n",
        if report.running { "running" } else { "stopped" },
        format_duration(report.elapsed),
        report.samples,
        report.sampling_mode,
        report.allocated_bytes,
        report.collections,
        format_duration(report.gc_time)
    );
    let _ = writeln!(
        output,
        "  {:>9} {:>10} {:>10} {:>8} {:>12}  Function",
        "Calls", "Self", "Total", "Samples", "Self bytes"
    );
    for function in report.functions {
        let _ = writeln!(
            output,
            "  {:>9} {:>10} {:>10} {:>8} {:>12}  {}",
            function.calls,
            format_duration(function.self_time),
            format_duration(function.total_time),
            function.samples,
            function.self_allocated_bytes,
            function.name
        );
    }
    Ok(output)
}

fn save_state_command(runtime: &Runtime, args: &str) -> Result<String, Box<dyn Error>> {
    let list = runtime.read_value(&format!("({args})"))?;
    let (path, mut symbols) = runtime
        .cons_fields(&list)
        .ok_or("Usage: :save-state \"path.zkclfasl\" symbol...")?;
    let path = runtime
        .string(&path)
        .ok_or("the saved state path must be a Lisp string")?;
    let mut names = Vec::new();
    let mut seen = Vec::new();
    while let Some((name, tail)) = runtime.cons_fields(&symbols) {
        if seen.iter().any(|previous| symbols.same_identity(previous)) {
            return Err("binding names form a circular list".into());
        }
        seen.push(symbols);
        names.push(name);
        symbols = tail;
    }
    if names.is_empty() || !symbols.is_nil() {
        return Err("supply a proper list of binding names".into());
    }
    let path = expand_tilde(&path);
    runtime.save_bindings(&path, &names.iter().collect::<Vec<_>>())?;
    Ok(format!(
        "Saved {} bindings to {}.",
        names.len(),
        path.display()
    ))
}

fn handle_history_command(
    editor: &mut reedline::Reedline,
    args: &str,
    printer: &ReplPrinter,
    theme: &ReplTheme,
) -> Result<(), Box<dyn Error>> {
    let trimmed = args.trim();
    if trimmed.eq_ignore_ascii_case("clear") {
        editor.history_mut().clear()?;
        printer.write_block(&theme.paint("History cleared.", theme.muted()))?;
        return Ok(());
    }
    if let Some(query) = trimmed.strip_prefix("search ") {
        let query = query.trim();
        let items = editor.history().search(SearchQuery {
            filter: SearchFilter::from_text_search(
                CommandLineSearch::Substring(query.to_owned()),
                None,
            ),
            limit: Some(30),
            ..SearchQuery::everything(SearchDirection::Backward, None)
        })?;
        if items.is_empty() {
            printer.write_block(&theme.paint("No matching history entries.", theme.muted()))?;
            return Ok(());
        }
        let mut out = String::new();
        for item in items.iter().rev() {
            let id = item.id.map_or(0, |id| id.0);
            let _ = writeln!(
                out,
                "  {}  {}",
                theme.paint(&format!("{id:>4}"), theme.muted()),
                zkcl_terminal::single_line(&item.command_line)
            );
        }
        printer.write_block(&out)?;
        return Ok(());
    }

    let limit = if trimmed.is_empty() {
        20
    } else {
        trimmed
            .parse::<i64>()
            .ok()
            .filter(|count| (1..=200).contains(count))
            .ok_or(
                "history count must be between 1 and 200; use `:history search <text>` to search",
            )?
    };
    let items = editor.history().search(SearchQuery {
        limit: Some(limit),
        ..SearchQuery::everything(SearchDirection::Backward, None)
    })?;
    if items.is_empty() {
        printer.write_block(&theme.paint("History is empty.", theme.muted()))?;
        return Ok(());
    }
    let mut out = String::new();
    for item in items.iter().rev() {
        let id = item.id.map_or(0, |id| id.0);
        let _ = writeln!(
            out,
            "  {}  {}",
            theme.paint(&format!("{id:>4}"), theme.muted()),
            zkcl_terminal::single_line(&item.command_line)
        );
    }
    printer.write_block(&out)?;
    Ok(())
}

fn submit_interactive(
    runtime: &Runtime,
    analysis: &EditorAnalysis,
    input: &str,
    printer: &mut ReplPrinter,
) -> Result<(), Box<dyn Error>> {
    let mut evaluated_source = None;
    let pretty = analysis.settings().pretty;
    let cancellation = runtime.cancellation_handle();
    let mut display = printer.error_output();
    let result = runtime.eval_submission(
        input,
        move || read_continuation_line(&cancellation, &mut display),
        |source, end, values| {
            evaluated_source = Some((source.clone(), end));
            printer.print_values(runtime, values, pretty)
        },
    );
    let submitted = evaluated_source
        .as_ref()
        .and_then(|(source, end)| source.text()?.get(..*end))
        .unwrap_or_default();
    analysis.record_submission(submitted, runtime.current_package());
    result
}

fn read_continuation_line(
    cancellation: &zkcl::CancellationHandle,
    display: &mut TrackedOutput<io::Stderr>,
) -> io::Result<Option<String>> {
    display.ensure_newline()?;
    display.write_all(b"... ")?;
    display.flush()?;
    let line = terminal_input::read_line(cancellation)?;
    if line.is_some() {
        display.tracker.set_at_line_start(true);
    }
    Ok(line)
}

pub(super) fn prompt_package(package: Option<String>) -> String {
    match package.as_deref() {
        Some("COMMON-LISP-USER" | "CL-USER") | None => "CL-USER".into(),
        Some("COMMON-LISP" | "CL") => "CL".into(),
        Some(package) => package.into(),
    }
}

fn evaluate_and_print(
    runtime: &Runtime,
    input: &str,
    output: &mut impl Write,
    mut on_evaluated: impl FnMut(usize),
) -> Result<(), Box<dyn Error>> {
    let document = zkcl::SourceDocument::input(input).with_uri("<stdin>");
    runtime.eval_document_with_offsets(&document, |end, values| {
        on_evaluated(end);
        for value in values {
            writeln!(output, "{}", runtime.print_result(value, None)?)?;
        }
        output.flush()?;
        Ok::<(), Box<dyn Error>>(())
    })
}

#[cfg(test)]
mod tests {
    use super::snapshot;
    use super::*;
    use reedline::StatusLine;

    fn terminal_runtime() -> Runtime {
        ReplStartup {
            heap_size: None,
            module_directories: &[],
            startup_files: &[],
            init_file: None,
            project: None,
            asdf: None,
            preload_asdf: false,
            package: None,
            active: Default::default(),
            jit_backend: None,
            jit_name: None,
        }
        .start_with_debugger(&ReplPrinter::new(false, false, false), &[])
        .unwrap()
    }

    #[test]
    fn debugger_menu_reprompts_after_returning_restart() {
        let runtime = terminal_runtime();
        let result = runtime.eval(r#"
          (let ((observed 0) (hook-calls 0) (out (make-string-output-stream)))
            (with-input-from-string (in (format nil "invalid~%ObSeRvE~%2~%"))
              (let ((*debug-io* (make-two-way-stream in out))
                    (*debugger-hook* (lambda (c old) (declare (ignore c old)) (incf hook-calls))))
                (restart-case
                    (restart-bind ((observe (lambda () (incf observed))
                                            :report-function (lambda (s) (write-string "Observe" s))))
                      (error "original failure"))
                  (recover () :report "Recover" nil))))
            (values observed hook-calls (get-output-stream-string out)))"#).unwrap();
        assert_eq!(result[0].as_fixnum(), Some(1));
        assert_eq!(result[1].as_fixnum(), Some(1));
        let transcript = runtime.string(&result[2]).unwrap();
        assert_eq!(transcript.matches("1. [OBSERVE] Observe").count(), 2);
        assert!(transcript.contains("2. [RECOVER] Recover"));
        assert!(transcript.contains("Choose a listed restart by number or name"));
    }

    #[test]
    fn terminal_debugger_returns_multiple_values_and_runs_cleanup() {
        let runtime = terminal_runtime();
        runtime
            .load_str(
                r#"
          (defvar *return-cleaned* nil)
          (defun terminal-return (n)
            (declare (optimize (debug 3)))
            (unwind-protect (progn (break "return here") (+ n 1))
              (setq *return-cleaned* t)))
        "#,
            )
            .unwrap();
        let values = runtime
            .eval(
                r#"
          (let ((*debug-io* (make-two-way-stream
                    (make-string-input-stream (format nil "frame 2~%return (values (+ n 10) 9)~%"))
                    (make-string-output-stream))))
            (multiple-value-list (terminal-return 32)))
        "#,
            )
            .unwrap();
        assert_eq!(runtime.repr(&values[0]).unwrap(), "(42 9)");
        assert!(runtime.eval("*return-cleaned*").unwrap()[0].is_t());
    }

    #[test]
    fn terminal_debugger_restarts_redefined_frame_with_original_arguments() {
        let runtime = terminal_runtime();
        runtime
            .load_str(
                r#"
          (defun terminal-restart (n)
            (declare (optimize (debug 3)))
            (break "restart here") (+ n 1))
        "#,
            )
            .unwrap();
        let values = runtime
            .eval(
                r#"
          (let ((*debug-io* (make-two-way-stream
                    (make-string-input-stream
                      (format nil "frame 2~%p (defun terminal-restart (n) (+ n 10))~%restart-frame~%"))
                    (make-string-output-stream))))
            (terminal-restart 32))
        "#,
            )
            .unwrap();
        assert_eq!(values[0].as_fixnum(), Some(42));
    }

    #[test]
    fn profile_command_measures_a_function_and_rejects_invalid_interval() {
        let runtime = Runtime::start().unwrap();
        runtime
            .eval("(defun terminal-profile (n) (+ n 1))")
            .unwrap();
        assert!(profile_command(&runtime, "start 0").is_err());
        profile_command(&runtime, "start 1").unwrap();
        runtime
            .eval("(dotimes (n 20) (terminal-profile n))")
            .unwrap();
        let report = profile_command(&runtime, "stop").unwrap();
        assert!(report.contains("TERMINAL-PROFILE"), "{report}");
        assert!(report.contains("Self bytes"), "{report}");
        profile_command(&runtime, "reset").unwrap();
        assert!(
            !profile_command(&runtime, "report")
                .unwrap()
                .contains("TERMINAL-PROFILE")
        );
    }

    #[test]
    fn terminal_recovers_unbound_variables_and_undefined_calls_in_place() {
        let runtime = terminal_runtime();
        runtime.load_str(r#"
          (defvar *recovery-output* (make-string-output-stream))
          (setq *debug-io*
            (make-two-way-stream
              (make-string-input-stream
                (format nil "p (defparameter terminal-missing 41)~%continue~%p (defun terminal-future (n) (+ n 10))~%continue~%"))
              *recovery-output*))
        "#).unwrap();
        let values = runtime.eval("(+ terminal-missing 1)").unwrap();
        assert_eq!(values[0].as_fixnum(), Some(42));
        let values = runtime.eval("(terminal-future 32)").unwrap();
        assert_eq!(values[0].as_fixnum(), Some(42));
        let values = runtime
            .eval("(get-output-stream-string *recovery-output*)")
            .unwrap();
        let text = runtime.string(&values[0]).unwrap();
        assert!(text.contains("CONTINUE"), "{text}");
        assert!(text.contains("ABORT"), "{text}");
        assert!(text.contains("debug> "), "{text}");
    }

    #[test]
    fn session_indexes_only_the_executed_prefix_of_a_failed_submission() {
        let runtime = Runtime::start().unwrap();
        let mut analysis = EditorAnalysis::new(Some("COMMON-LISP-USER".into()));
        let input =
            "(defun before-error (arg) arg) \"雪\" (error \"stop\") (defun after-error (arg) arg)";
        let mut evaluated_byte_end = 0;
        let error = evaluate_and_print(&runtime, input, &mut Vec::new(), |end| {
            evaluated_byte_end = end;
        })
        .unwrap_err();
        analysis.record_submission(&input[..evaluated_byte_end], runtime.current_package());
        assert!(error.to_string().contains("stop"));

        let before = "(before-error ";
        let after = "(after-error ";
        assert!(
            analysis
                .status(before, before.len(), 80, false)
                .contains("[arg]")
        );
        assert!(
            !analysis
                .status(after, after.len(), 80, false)
                .contains("[arg]")
        );
    }

    #[test]
    fn custom_reader_can_continue_once_without_replaying_the_read_macro() {
        let runtime = Runtime::start().unwrap();
        runtime
            .load_str(
                r#"
            (defparameter *reads* 0)
            (setf *readtable* (copy-readtable nil))
            (set-macro-character #\[ (lambda (stream character)
              (declare (ignore character))
              (incf *reads*)
              (list 'quote (read-delimited-list #\] stream t))))
            (set-macro-character #\] (get-macro-character #\)))
        "#,
            )
            .unwrap();
        assert!(runtime.custom_readtable());
        let mut lines = ["4 ] (+ 20 22)\n"].into_iter();
        let mut values = Vec::new();
        runtime
            .eval_submission::<EvalError>(
                "[3",
                move || Ok(lines.next().map(str::to_owned)),
                |document, end, returned| {
                    assert!(document.text().unwrap().is_char_boundary(end));
                    values.push(runtime.repr(&returned[0])?);
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(values, ["(3 4)", "42"]);
        assert_eq!(runtime.eval("*reads*").unwrap()[0].as_fixnum(), Some(1));
    }

    #[test]
    fn saved_bindings_preserve_shared_closures_and_cycles_across_runtimes() {
        let class = "(defclass saved-record () ((inner :initarg :inner :accessor saved-inner)))";
        let runtime = Runtime::start().unwrap();
        runtime.load_str(class).unwrap();
        runtime
            .load_str(
                r#"
            (defvar *audit-counter*)
            (defvar *audit-reader*)
            (defvar *audit-cycle* (cons nil nil))
            (setf (car *audit-cycle*) *audit-cycle*)
            (defmethod make-load-form ((record saved-record) &optional environment)
              (make-load-form-saving-slots record :environment environment))
            (defvar *audit-record* (make-instance 'saved-record :inner *audit-cycle*))
            (let ((n 40))
              (setf *audit-counter* (lambda () (incf n))
                    *audit-reader* (lambda () n)))
            (defun audit-saved-fn () (funcall *audit-reader*))
        "#,
            )
            .unwrap();
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.agents/tmp/zkcl/repl-regression");
        fs::create_dir_all(&directory).unwrap();
        let temp = tempfile::tempdir_in(directory).unwrap();
        let file = temp.path().join("saved.zkclfasl");
        let command = format!(
            "{} *audit-counter* *audit-reader* *audit-cycle* *audit-record* audit-saved-fn",
            snapshot::lisp_string_literal(file.to_str().unwrap())
        );
        save_state_command(&runtime, &command).unwrap();
        let target = Runtime::start().unwrap();
        target.load_str(class).unwrap();
        target.load_file(&file).unwrap();
        let values = target
            .eval(
                r#"(list (funcall *audit-counter*) (audit-saved-fn)
                         (eq (car *audit-cycle*) *audit-cycle*)
                         (eq (saved-inner *audit-record*) *audit-cycle*))"#,
            )
            .unwrap();
        assert_eq!(target.repr(&values[0]).unwrap(), "(41 41 T T)");
    }

    #[test]
    fn interactive_values_start_on_a_fresh_line_and_align() {
        assert_eq!(format_values(&["NIL".into()], false, false), "\n=> NIL\n");
        assert_eq!(
            format_values(&["1".into(), "2".into()], true, false),
            "=> 1\n   2\n"
        );
        assert_eq!(format_values(&[], false, false), "\n; no values\n");
        assert_eq!(format_values(&[], true, false), "; no values\n");
    }

    #[test]
    fn colored_result_prefix_keeps_the_newline_after_the_reset() {
        let block = format_values(&["42".into()], true, true);
        assert!(block.starts_with('\x1b'), "{block:?}");
        assert!(block.ends_with("42\n"), "{block:?}");
    }

    #[test]
    fn prompt_uses_short_package_names() {
        assert_eq!(prompt_package(None), "CL-USER");
        assert_eq!(prompt_package(Some("COMMON-LISP-USER".into())), "CL-USER");
        assert_eq!(prompt_package(Some("COMMON-LISP".into())), "CL");
        assert_eq!(prompt_package(Some("ZK-DEMO".into())), "ZK-DEMO");
    }

    #[test]
    fn runtime_bridge_queries_work_even_in_bare_package() {
        let runtime = Runtime::start().unwrap();
        runtime
            .eval(
                "(progn
                   (defpackage :bare-repl-test (:use))
                   (defun cl-user::repl-bridge-helper (x)
                     \"Double x.\"
                     (+ x x))
                   (defmacro cl-user::repl-bridge-mac (x) `(+ ,x 1)))",
            )
            .unwrap();
        let pkg = snapshot::switch_package(&runtime, "bare-repl-test").unwrap();
        assert_eq!(pkg, "BARE-REPL-TEST");

        let snap = runtime.namespace_snapshot();
        assert_eq!(snap.current_package.as_deref(), Some("BARE-REPL-TEST"));
        assert!(
            snap.symbols
                .iter()
                .any(|s| s.package == "COMMON-LISP-USER" && s.name == "REPL-BRIDGE-HELPER")
        );

        let (expanded, changed) =
            snapshot::eval_macroexpand(&runtime, "(cl-user::repl-bridge-mac 10)", true).unwrap();
        assert!(changed);
        assert!(expanded.contains("10"), "{expanded}");

        let apropos = snapshot::eval_apropos(&runtime, "repl-bridge", None).unwrap();
        assert!(
            apropos
                .iter()
                .any(|e| e.qualified_name.contains("repl-bridge-helper")),
            "{:?}",
            apropos
                .iter()
                .map(|e| &e.qualified_name)
                .collect::<Vec<_>>()
        );

        let desc = snapshot::eval_describe(&runtime, "'cl-user::repl-bridge-helper").unwrap();
        assert!(desc.contains("REPL-BRIDGE-HELPER"), "{desc}");

        let tys = snapshot::eval_runtime_types(&runtime, "(cl:+ 1 2)").unwrap();
        assert_eq!(tys.len(), 1);
        assert_eq!(tys[0].0, "3");
    }

    #[test]
    fn result_printing_honors_lisp_printer_controls() {
        let runtime = Runtime::start().unwrap();
        runtime
            .eval("(setf *print-base* 16 *print-length* 2)")
            .unwrap();
        let values = runtime.eval("(list 42 255 16)").unwrap();
        assert_eq!(
            runtime.print_result(&values[0], None).unwrap(),
            "(2A FF ...)"
        );
    }
}
