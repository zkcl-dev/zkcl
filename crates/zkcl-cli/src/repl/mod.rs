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
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Instant;

use reedline::{CommandLineSearch, SearchDirection, SearchFilter, SearchQuery, Signal};
use zkcl::native::JitBackend;
use zkcl::{EvalError, EvalErrorKind, Root, Runtime};

use commands::{
    ParsedCommand, apply_setting, expand_tilde, parse_command, render_env, render_help,
    run_apropos_command, run_cd_command, run_describe_command, run_disassemble_command,
    run_doc_command, run_fmt_command, run_gc_command, run_lint_command, run_macroexpand_command,
    run_package_command, run_packages_command, run_room_command, run_time_command,
    run_trace_command, run_type_command, unquote_path,
};
use editor::EditorAnalysis;
use theme::{ReplTheme, format_duration};

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

#[derive(Clone)]
struct TrackedStdout(OutputTracker);

impl Write for TrackedStdout {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut stdout = io::stdout().lock();
        let written = stdout.write(bytes)?;
        if written > 0 {
            self.0.set_at_line_start(bytes[written - 1] == b'\n');
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().lock().flush()
    }
}

#[derive(Clone)]
struct TrackedStderr(OutputTracker);

impl Write for TrackedStderr {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut stderr = io::stderr().lock();
        let written = stderr.write(bytes)?;
        if written > 0 {
            self.0.set_at_line_start(bytes[written - 1] == b'\n');
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().lock().flush()
    }
}

struct ReplPrinter {
    tracker: OutputTracker,
    theme: ReplTheme,
}

impl ReplPrinter {
    fn new(color: bool) -> Self {
        Self {
            tracker: OutputTracker::new(),
            theme: ReplTheme::new(color),
        }
    }

    fn standard_output(&self) -> TrackedStdout {
        TrackedStdout(self.tracker.clone())
    }

    fn error_output(&self) -> TrackedStderr {
        TrackedStderr(self.tracker.clone())
    }

    fn writer(&self) -> TrackedStdout {
        self.standard_output()
    }

    fn ensure_newline(&self) -> io::Result<()> {
        if !self.tracker.at_line_start() {
            let mut output = self.writer();
            output.write_all(b"\n")?;
            output.flush()?;
        }
        Ok(())
    }

    fn write_block(&self, text: &str) -> io::Result<()> {
        self.ensure_newline()?;
        let mut output = self.writer();
        output.write_all(text.as_bytes())?;
        if !text.ends_with('\n') {
            output.write_all(b"\n")?;
        }
        output.flush()
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
                    print_errors.push((index + 1, error));
                }
            }
        }
        let text = format_values_themed(&rendered, self.tracker.at_line_start(), &self.theme);
        runtime.record_dribble_text(&format_values_themed(
            &rendered,
            self.tracker.at_line_start(),
            &ReplTheme::new(false),
        ))?;
        {
            let mut output = self.writer();
            output.write_all(text.as_bytes())?;
            output.flush()?;
        }
        if !print_errors.is_empty() {
            let mut error_output = self.error_output();
            for (index, error) in print_errors {
                let label = self.theme.paint("print error", self.theme.error());
                writeln!(
                    error_output,
                    "{label} in value {index}: {}",
                    single_line(&error.to_string())
                )?;
            }
            error_output.flush()?;
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
    if values.is_empty() {
        return if at_line_start {
            String::new()
        } else {
            "\n".to_owned()
        };
    }
    let prefix_style = theme.result_prefix();
    let indent_style = theme.subtle();
    let mut out = String::new();
    let mut line_start = at_line_start;
    for (index, value) in values.iter().enumerate() {
        if !line_start {
            out.push('\n');
        }
        let prefix = if index == 0 {
            theme.paint(RESULT_PREFIX, prefix_style)
        } else {
            theme.paint(RESULT_INDENT, indent_style)
        };
        out.push_str(&prefix);
        let mut lines = value.split('\n');
        if let Some(first) = lines.next() {
            out.push_str(first);
        }
        for line in lines {
            out.push('\n');
            out.push_str(&theme.paint(RESULT_INDENT, indent_style));
            out.push_str(line);
        }
        out.push('\n');
        line_start = true;
    }
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
    let project = super::project::repl_project(request)?;
    let jit_request = request.runtime.jit_request();
    let jit_name = jit_request.map(|req| match req {
        super::JitBackendRequest::Default => {
            if cfg!(feature = "cranelift-jit") {
                "cranelift"
            } else {
                "direct"
            }
        }
        super::JitBackendRequest::Cranelift => "cranelift",
        super::JitBackendRequest::Direct => "direct",
    });
    let interactive = !request.json && io::stdin().is_terminal() && io::stderr().is_terminal();
    let startup = ReplStartup {
        module_directories: &request.runtime.module_dir,
        startup_files: &request.load,
        project: project.as_ref(),
        project_output: std::cell::RefCell::new(None),
        candidate_output: std::cell::RefCell::new(None),
        project_plan: std::cell::RefCell::new(None),
        heap_size,
        init_file: request.init_file().resolve(interactive)?,
        jit_backend: jit_request
            .map(super::JitBackendRequest::create_backend)
            .transpose()?,
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

struct ReplStartup<'a> {
    project: Option<&'a super::project::ReplProject>,
    project_output: std::cell::RefCell<Option<tempfile::TempDir>>,
    candidate_output: std::cell::RefCell<Option<tempfile::TempDir>>,
    project_plan: std::cell::RefCell<Option<super::project::ResolvedRepl>>,
    module_directories: &'a [PathBuf],
    startup_files: &'a [PathBuf],
    init_file: Option<PathBuf>,
    heap_size: Option<usize>,
    jit_backend: Option<Arc<dyn JitBackend>>,
    jit_name: Option<&'static str>,
}

impl ReplStartup<'_> {
    fn start(
        &self,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
    ) -> Result<Runtime, Box<dyn Error>> {
        let runtime = self.prepare(configure_io)?;
        self.load_files(&runtime, &[])?;
        self.commit_output();
        Ok(runtime)
    }

    fn start_bare(
        &self,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
    ) -> Result<Runtime, Box<dyn Error>> {
        self.discard_output();
        let mut runtime = super::session_runtime(self.jit_backend.as_ref(), self.heap_size)?;
        configure_io(&mut runtime)?;
        super::add_module_directories(&runtime, self.module_directories)?;
        Ok(runtime)
    }

    fn prepare(
        &self,
        configure_io: impl FnOnce(&mut Runtime) -> Result<(), zkcl::EvalError>,
    ) -> Result<Runtime, Box<dyn Error>> {
        self.discard_output();
        let plan = self
            .project
            .map(super::project::ReplProject::resolve)
            .transpose()?;
        let mut runtime = super::session_runtime(self.jit_backend.as_ref(), self.heap_size)?;
        configure_io(&mut runtime)?;
        super::add_module_directories(&runtime, self.module_directories)?;
        *self.project_plan.borrow_mut() = plan;
        Ok(runtime)
    }

    fn load_files(&self, runtime: &Runtime, extra_files: &[PathBuf]) -> Result<(), Box<dyn Error>> {
        if let Some(path) = &self.init_file {
            runtime.load_init_file(path)?;
        }
        let output = self
            .project_plan
            .borrow_mut()
            .take()
            .map(|plan| plan.load(runtime))
            .transpose()?;
        for path in self.startup_files.iter().chain(extra_files.iter()) {
            runtime.load_file(path)?;
        }
        *self.candidate_output.borrow_mut() = output;
        Ok(())
    }

    fn commit_output(&self) {
        *self.project_output.borrow_mut() = self.candidate_output.borrow_mut().take();
    }

    fn discard_output(&self) {
        self.candidate_output.borrow_mut().take();
    }

    fn start_with_debugger(
        &self,
        printer: &ReplPrinter,
        extra_files: &[PathBuf],
    ) -> Result<Runtime, Box<dyn Error>> {
        let runtime = self.prepare(|runtime| configure_interactive_io(runtime, printer))?;
        self.load_files(&runtime, extra_files)?;
        configure_interactive_tools(&runtime, printer)?;
        self.commit_output();
        Ok(runtime)
    }

    fn start_bare_with_debugger(&self, printer: &ReplPrinter) -> Result<Runtime, Box<dyn Error>> {
        self.start_bare(|runtime| {
            configure_interactive_io(runtime, printer)?;
            configure_interactive_tools(runtime, printer)
        })
    }
}

fn configure_interactive_io(runtime: &mut Runtime, printer: &ReplPrinter) -> Result<(), EvalError> {
    runtime.set_interactive_input(terminal_input::TerminalInput::new(
        runtime.cancellation_handle(),
    ));
    runtime.set_standard_output(printer.standard_output());
    runtime.set_error_output(printer.error_output());
    runtime.eval("(CL:SETQ CL:*PRINT-PRETTY* CL:T)")?;
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

fn index_startup_files(analysis: &EditorAnalysis, files: &[PathBuf]) {
    for path in files {
        if path.extension().and_then(|e| e.to_str()) == Some("lisp") {
            analysis.index_file(path);
        }
    }
}

fn run_interactive(startup: &ReplStartup<'_>) -> Result<(), Box<dyn Error>> {
    let use_color = terminal_colors_enabled();
    let mut printer = ReplPrinter::new(use_color);
    let mut user_loaded_files: Vec<PathBuf> = Vec::new();
    let mut runtime = startup.start_with_debugger(&printer, &user_loaded_files)?;
    printer.ensure_newline()?;

    let analysis = EditorAnalysis::with_color(runtime.current_package(), use_color);
    analysis.set_jit_active(startup.jit_name.is_some());
    index_startup_files(&analysis, startup.startup_files);
    if let Some(path) = &startup.init_file {
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
        match editor.read_line(&prompt) {
            Ok(Signal::Success(input)) => {
                if input.trim().is_empty() {
                    continue;
                }
                if let Some(parsed) = parse_command(&input) {
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
                    )? {
                        LoopControl::Continue => {
                            analysis.refresh_runtime_snapshot(&runtime);
                            continue;
                        }
                        LoopControl::Exit => return Ok(()),
                    }
                }

                printer.tracker.set_at_line_start(true);
                let _interrupt_guard =
                    EvalInterruptGuard::enter(&eval_active, &interrupt_count, &runtime);
                let started = Instant::now();
                let res = submit_interactive(&runtime, &analysis, &input, &mut printer);
                let elapsed = started.elapsed();
                drop(_interrupt_guard);

                analysis.set_last_duration(Some(elapsed));
                analysis.refresh_runtime_snapshot(&runtime);
                if let Err(error) = res {
                    let _ = printer.ensure_newline();
                    report_repl_error(error.as_ref(), &printer.theme);
                } else if analysis.settings().timing == commands::TimingMode::On {
                    let _ = printer.write_block(&printer.theme.paint(
                        &format!("  time {}", format_duration(elapsed)),
                        printer.theme.muted(),
                    ));
                }
            }
            Ok(Signal::CtrlD) => return Ok(()),
            Ok(Signal::CtrlC(_)) => continue,
            Ok(Signal::HostCommand(command)) if command == "help" => {
                let help = render_help(&printer.theme, "");
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
    let (spec, args) = (parsed.spec, parsed.args);

    match spec.name {
        ":quit" => return Ok(LoopControl::Exit),
        ":help" => {
            let help = render_help(&theme, args);
            printer.write_block(&help)?;
        }
        ":clear" => {
            let _ = editor.clear_scrollback();
        }
        ":edit" => {
            if args.is_empty() {
                let scratch_path =
                    env::temp_dir().join(format!("zkcl-scratch-{}.lisp", std::process::id()));
                let initial = ";; Write Common Lisp forms below, then save and exit to evaluate.\n";
                fs::write(&scratch_path, initial)?;
                let status = editor::launch_external_editor(&scratch_path);
                let content = fs::read_to_string(&scratch_path).unwrap_or_default();
                let _ = fs::remove_file(&scratch_path);
                match status {
                    Ok(exit) if exit.success() => {
                        let body = content.trim();
                        if !body.is_empty() && body != initial.trim() {
                            printer.tracker.set_at_line_start(true);
                            let _interrupt_guard =
                                EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
                            let started = Instant::now();
                            let res = submit_interactive(runtime, analysis, &content, printer);
                            let elapsed = started.elapsed();
                            drop(_interrupt_guard);
                            analysis.set_last_duration(Some(elapsed));
                            if let Err(error) = res {
                                let _ = printer.ensure_newline();
                                report_repl_error(error.as_ref(), &theme);
                            }
                        } else {
                            printer.write_block(&theme.paint(
                                "Scratch buffer empty; nothing evaluated.",
                                theme.muted(),
                            ))?;
                        }
                    }
                    Ok(exit) => {
                        eprintln!(
                            "{}",
                            theme.paint(
                                &format!("Editor exited with status {exit}; evaluation skipped."),
                                theme.incomplete()
                            )
                        );
                    }
                    Err(err) => {
                        eprintln!(
                            "{}",
                            theme.paint(&format!("Failed to launch editor: {err}"), theme.error())
                        );
                    }
                }
            } else {
                let path = expand_tilde(unquote_path(args));
                match editor::launch_external_editor(&path) {
                    Ok(exit) if exit.success() => {
                        printer.write_block(&theme.paint(
                            &format!(
                                "Edited {} (run :load {} to evaluate).",
                                path.display(),
                                path.display()
                            ),
                            theme.muted(),
                        ))?;
                    }
                    Ok(exit) => {
                        eprintln!(
                            "{}",
                            theme.paint(
                                &format!("Editor exited with status {exit}."),
                                theme.incomplete()
                            )
                        );
                    }
                    Err(err) => {
                        eprintln!(
                            "{}",
                            theme.paint(&format!("Failed to launch editor: {err}"), theme.error())
                        );
                    }
                }
            }
        }
        ":env" => {
            let mut all_files: Vec<PathBuf> = startup.startup_files.to_vec();
            all_files.extend(user_loaded_files.iter().cloned());
            let text = render_env(
                &theme,
                runtime,
                &analysis.settings(),
                startup.jit_name,
                &all_files,
                startup.project.is_some(),
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
                    eprintln!("{}", theme.paint(&err, theme.error()));
                }
            }
        }
        ":history" => {
            handle_history_command(editor, args, printer, &theme)?;
        }
        ":reload" => {
            if startup.startup_files.is_empty()
                && startup.project.is_none()
                && user_loaded_files.is_empty()
            {
                eprintln!(
                    "{}",
                    theme.paint(
                        "No project, --load, or :load files to reload (use :reset for a clean session).",
                        theme.muted()
                    )
                );
                return Ok(LoopControl::Continue);
            }
            match startup.start_with_debugger(printer, user_loaded_files) {
                Ok(candidate) => {
                    *runtime = candidate;
                    if let Some(slot) = cancel_slot
                        && let Ok(mut guard) = slot.lock()
                    {
                        *guard = InterruptHandles::new(runtime);
                    }
                    analysis.reset(runtime.current_package());
                    if let Some(path) = &startup.init_file {
                        analysis.index_file(path);
                    }
                    index_startup_files(analysis, startup.startup_files);
                    index_startup_files(analysis, user_loaded_files);
                    printer.write_block(
                        &theme.paint("Session reloaded cleanly.", theme.result_prefix()),
                    )?;
                }
                Err(error) => {
                    let _ = printer.ensure_newline();
                    super::report_error(
                        "Reload failed, keeping the current session: ",
                        error.as_ref(),
                    );
                }
            }
        }
        ":reset" => match startup.start_bare_with_debugger(printer) {
            Ok(candidate) => {
                *runtime = candidate;
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
                super::report_error("Reset failed: ", error.as_ref());
            }
        },
        ":package" => match run_package_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":packages" => {
            let text = run_packages_command(runtime, &theme);
            printer.write_block(&text)?;
        }
        ":load" => {
            if args.is_empty() {
                eprintln!("{}", theme.paint("Usage: :load <path>", theme.error()));
                return Ok(LoopControl::Continue);
            }
            let path = expand_tilde(unquote_path(args));
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
                    if path.extension().and_then(|e| e.to_str()) == Some("lisp") {
                        analysis.index_file(&path);
                    }
                    analysis.set_last_duration(Some(elapsed));
                    printer.write_block(&theme.paint(
                        &format!("Loaded {} ({})", path.display(), format_duration(elapsed)),
                        theme.muted(),
                    ))?;
                }
                Err(error) => {
                    let _ = printer.ensure_newline();
                    report_repl_error(&error, &theme);
                }
            }
        }
        ":compile" => {
            let mut parts = args.split_whitespace();
            let Some(src_raw) = parts.next() else {
                eprintln!(
                    "{}",
                    theme.paint("Usage: :compile <path> [output]", theme.error())
                );
                return Ok(LoopControl::Continue);
            };
            let src = expand_tilde(unquote_path(src_raw));
            let out = parts.next().map(|p| expand_tilde(unquote_path(p)));
            let t0 = Instant::now();
            match runtime.compile_file(&src, out.as_deref()) {
                Ok(compiled) => {
                    printer.write_block(&theme.paint(
                        &format!(
                            "Compiled {} -> {} ({})",
                            src.display(),
                            compiled.display(),
                            format_duration(t0.elapsed())
                        ),
                        theme.muted(),
                    ))?;
                }
                Err(error) => {
                    report_repl_error(&error, &theme);
                }
            }
        }
        ":pwd" => {
            let cwd = env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unknown>".into());
            printer.write_block(&cwd)?;
        }
        ":cd" => match run_cd_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":doc" => {
            let mut state = analysis.lock_source();
            match run_doc_command(runtime, &mut state.workspace, args, &theme) {
                Ok(msg) => {
                    drop(state);
                    printer.write_block(&msg)?;
                }
                Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
            }
        }
        ":describe" => match run_describe_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":apropos" => match run_apropos_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":macroexpand" => match run_macroexpand_command(runtime, args, false, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":macroexpand-1" => match run_macroexpand_command(runtime, args, true, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":disassemble" => match run_disassemble_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":break" => match debugger::add_breakpoint(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => eprintln!("{}", theme.paint(&error, theme.error())),
        },
        ":break-file" => match debugger::add_file_breakpoint(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => eprintln!("{}", theme.paint(&error, theme.error())),
        },
        ":break-locations" => match debugger::executable_locations(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => eprintln!("{}", theme.paint(&error, theme.error())),
        },
        ":breakpoints" => printer.write_block(&debugger::list_breakpoints(runtime))?,
        ":enable-breakpoint" | ":disable-breakpoint" => match args.parse::<u64>() {
            Ok(id) => match runtime.enable_breakpoint(id, spec.name == ":enable-breakpoint") {
                Ok(breakpoint) => printer.write_block(&debugger::format_breakpoint(&breakpoint))?,
                Err(error) => eprintln!("{}", theme.paint(&error.to_string(), theme.error())),
            },
            Err(_) => eprintln!("Usage: {} <id>", spec.name),
        },
        ":save-state" => match save_state_command(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => eprintln!("{}", theme.paint(&error.to_string(), theme.error())),
        },
        ":save-image" => {
            if args.is_empty() {
                eprintln!("Usage: :save-image <path>");
            } else {
                let path = expand_tilde(unquote_path(args));
                let hooks = zkcl::NamedImageHooks::new(&[("ZK", "%REPL-INSPECT")]);
                match runtime.save_image_with_hooks(&path, &hooks) {
                    Ok(()) => {
                        printer.write_block(&format!("Saved image to {}.", path.display()))?
                    }
                    Err(error) => super::report_error("Save image failed: ", &error),
                }
            }
        }
        ":restore-image" => {
            if args.is_empty() {
                eprintln!("Usage: :restore-image <path>");
            } else {
                let path = expand_tilde(unquote_path(args));
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
                        *runtime = candidate;
                        user_loaded_files.clear();
                        if let Some(slot) = cancel_slot
                            && let Ok(mut guard) = slot.lock()
                        {
                            *guard = InterruptHandles::new(runtime);
                        }
                        analysis.reset(runtime.current_package());
                        printer.write_block(&format!("Restored image from {}.", path.display()))?;
                    }
                    Err(error) => super::report_error(
                        "Restore failed, keeping the current session: ",
                        error.as_ref(),
                    ),
                }
            }
        }
        ":dribble" => {
            let source = if args.is_empty() {
                "(CL:DRIBBLE)".to_owned()
            } else {
                let path = expand_tilde(unquote_path(args));
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
                Err(error) => super::report_error("Dribble failed: ", &error),
            }
        }
        ":profile" => match profile_command(runtime, args) {
            Ok(message) => printer.write_block(&message)?,
            Err(error) => eprintln!("{}", theme.paint(&error, theme.error())),
        },
        ":definition" | ":source" => {
            match definition_command(runtime, args, spec.name == ":source") {
                Ok(message) => printer.write_block(&message)?,
                Err(error) => eprintln!("{}", theme.paint(&error, theme.error())),
            }
        }
        ":delete-breakpoint" => match args.parse() {
            Ok(id) if runtime.remove_breakpoint(id) => {
                printer.write_block(&format!("Deleted breakpoint {id}."))?
            }
            Ok(_) => eprintln!("{}", theme.paint("No such breakpoint.", theme.error())),
            Err(_) => eprintln!(
                "{}",
                theme.paint("Usage: :delete-breakpoint <id>", theme.error())
            ),
        },
        ":type" => {
            let static_ty = analysis.static_expression_type(args);
            let _interrupt_guard = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let result = run_type_command(runtime, static_ty, args, &theme);
            drop(_interrupt_guard);
            match result {
                Ok(msg) => printer.write_block(&msg)?,
                Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
            }
        }
        ":time" => {
            if args.is_empty() {
                eprintln!("{}", theme.paint("Usage: :time <form>", theme.error()));
                return Ok(LoopControl::Continue);
            }
            let _interrupt_guard = EvalInterruptGuard::enter(eval_active, interrupt_count, runtime);
            let res = run_time_command(runtime, args, analysis.settings().pretty, printer);
            drop(_interrupt_guard);
            if let Err(error) = res {
                let _ = printer.ensure_newline();
                report_repl_error(error.as_ref(), &theme);
            }
        }
        ":trace" => match run_trace_command(runtime, true, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":untrace" => match run_trace_command(runtime, false, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":room" => {
            let text = run_room_command(runtime, &theme);
            printer.write_block(&text)?;
        }
        ":gc" => {
            let text = run_gc_command(runtime, &theme);
            printer.write_block(&text)?;
        }
        ":lint" => match run_lint_command(runtime, args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        ":fmt" => match run_fmt_command(args, &theme) {
            Ok(msg) => printer.write_block(&msg)?,
            Err(err) => eprintln!("{}", theme.paint(&err, theme.error())),
        },
        _ => {}
    }
    Ok(LoopControl::Continue)
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
                item.command_line.replace('\n', " ↵ ")
            );
        }
        printer.write_block(&out)?;
        return Ok(());
    }

    let limit = if trimmed.is_empty() {
        20
    } else {
        trimmed.parse::<i64>().unwrap_or(20).clamp(1, 200)
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
            item.command_line.replace('\n', " ↵ ")
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
    let result = runtime.eval_submission(
        input,
        move || read_continuation_line(&cancellation),
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

fn read_continuation_line(cancellation: &zkcl::CancellationHandle) -> io::Result<Option<String>> {
    {
        let mut output = io::stdout().lock();
        output.write_all(b"... ")?;
        output.flush()?;
    }
    terminal_input::read_line(cancellation)
}

fn terminal_colors_enabled() -> bool {
    zkcl_terminal::use_color(zkcl_terminal::Stream::Stdout)
}

pub(super) fn prompt_package(package: Option<String>) -> String {
    match package.as_deref() {
        Some("COMMON-LISP-USER" | "CL-USER") | None => "CL-USER".into(),
        Some("COMMON-LISP" | "CL") => "CL".into(),
        Some(package) => package.into(),
    }
}

fn report_repl_error(error: &(dyn Error + 'static), theme: &ReplTheme) {
    if let Some(error) = error.downcast_ref::<EvalError>() {
        if error.debugger_aborted() {
            return;
        }
        if error.kind() == EvalErrorKind::Cancelled {
            eprintln!(
                "{}",
                theme.paint("^C evaluation interrupted", theme.incomplete())
            );
            return;
        }
    }
    super::report_error("", error);
}

fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\r' => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            '\t' => out.push(' '),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.trim_end().to_owned()
}

fn evaluate_and_print(
    runtime: &Runtime,
    input: &str,
    output: &mut impl Write,
    mut on_evaluated: impl FnMut(usize),
) -> Result<(), Box<dyn Error>> {
    runtime.eval_source_with_offsets(input, |end, values| {
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
            project_output: std::cell::RefCell::new(None),
            candidate_output: std::cell::RefCell::new(None),
            project_plan: std::cell::RefCell::new(None),
            jit_backend: None,
            jit_name: None,
        }
        .start_with_debugger(&ReplPrinter::new(false), &[])
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
        assert_eq!(format_values(&[], false, false), "\n");
        assert_eq!(format_values(&[], true, false), "");
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
    fn error_formatting_keeps_single_line_messages() {
        assert_eq!(single_line("line1\r\nline2\t\x07"), "line1 line2 \\u{7}");
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
