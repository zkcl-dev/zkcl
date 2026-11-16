use std::borrow::Cow;
use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr;
use zkcl::{EvalError, Runtime};
use zkcl_diagnostics::SourceDocument;
use zkcl_syntax::syntax::SourceReadtable;
use zkcl_syntax::{LintConfig, LintPreset, SourceWorkspace};

use super::snapshot::{
    eval_apropos, eval_describe, eval_disassemble, eval_macroexpand, eval_runtime_doc,
    eval_runtime_types, eval_trace_op, switch_package, sync_lisp_cwd,
};
use super::theme::{ReplTheme, format_bytes, format_duration, highlight_lisp_output};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TimingMode {
    Auto,
    On,
    Off,
}

impl TimingMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    pub(super) fn should_show(self, elapsed: Duration) -> bool {
        match self {
            Self::Auto => elapsed >= Duration::from_millis(25),
            Self::On => true,
            Self::Off => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct ReplSettings {
    pub timing: TimingMode,
    pub pretty: bool,
    pub rainbow: bool,
    pub preview: bool,
    pub hints: bool,
    pub vi_mode: bool,
}

impl Default for ReplSettings {
    fn default() -> Self {
        Self {
            timing: TimingMode::Auto,
            pretty: true,
            rainbow: false,
            preview: true,
            hints: true,
            vi_mode: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandCategory {
    Session,
    Navigation,
    Inspection,
    Analysis,
}

impl CommandCategory {
    pub(super) const fn title(self) -> &'static str {
        match self {
            Self::Session => "Session & UI",
            Self::Navigation => "Packages & Files",
            Self::Inspection => "Inspection & Debugging",
            Self::Analysis => "Lint, Format & Types",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandArgKind {
    None,
    Command,
    FilePath,
    Name,
    Package,
    System,
    Symbol,
    Setting,
    Form,
}

pub(super) struct CommandDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub usage: &'static str,
    pub category: CommandCategory,
    pub summary: &'static str,
    pub arg_kind: CommandArgKind,
    pub takes_lisp_form: bool,
}

pub(super) const COMMANDS: &[CommandDef] = &[
    CommandDef {
        name: ":help",
        aliases: &[":h", ":?"],
        usage: ":help [command | symbol]",
        category: CommandCategory::Session,
        summary: "Show commands and shortcuts, or inspect a command/symbol",
        arg_kind: CommandArgKind::Command,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":quit",
        aliases: &[":q", ":exit"],
        usage: ":quit",
        category: CommandCategory::Session,
        summary: "Leave the REPL",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":clear",
        aliases: &[":cls"],
        usage: ":clear",
        category: CommandCategory::Session,
        summary: "Clear the terminal screen",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":env",
        aliases: &[":status"],
        usage: ":env",
        category: CommandCategory::Session,
        summary: "Show session info (package, JIT, heap, files, settings)",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":set",
        aliases: &[],
        usage: ":set [option [value]]",
        category: CommandCategory::Session,
        summary: "Configure REPL options (timing, pretty, rainbow, preview, hints, edit-mode)",
        arg_kind: CommandArgKind::Setting,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":history",
        aliases: &[":hist"],
        usage: ":history [count | search <text> | clear]",
        category: CommandCategory::Session,
        summary: "View, search, or clear persistent input history",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":refresh",
        aliases: &[],
        usage: ":refresh",
        category: CommandCategory::Session,
        summary: "Reload changed project components in the current session",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":reload",
        aliases: &[":r"],
        usage: ":reload",
        category: CommandCategory::Session,
        summary: "Rebuild a fresh session from project and loaded files",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":reset",
        aliases: &[],
        usage: ":reset",
        category: CommandCategory::Session,
        summary: "Start a clean fresh Lisp runtime session",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":edit",
        aliases: &[":ed"],
        usage: ":edit [path]",
        category: CommandCategory::Session,
        summary: "Edit a scratch form (then eval) or file in $VISUAL / $EDITOR",
        arg_kind: CommandArgKind::FilePath,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":package",
        aliases: &[":in", ":pkg"],
        usage: ":package [name]",
        category: CommandCategory::Navigation,
        summary: "Show current package or switch *PACKAGE*",
        arg_kind: CommandArgKind::Package,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":packages",
        aliases: &[":pkgs"],
        usage: ":packages",
        category: CommandCategory::Navigation,
        summary: "List all live packages with nicknames and symbol counts",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":require",
        aliases: &[],
        usage: ":require <module>",
        category: CommandCategory::Navigation,
        summary: "Load a named module through REQUIRE",
        arg_kind: CommandArgKind::Name,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":modules",
        aliases: &[],
        usage: ":modules",
        category: CommandCategory::Navigation,
        summary: "List provided Lisp modules",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":system",
        aliases: &[],
        usage: ":system [name]",
        category: CommandCategory::Navigation,
        summary: "Load an ASDF system or refresh the selected --system",
        arg_kind: CommandArgKind::System,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":systems",
        aliases: &[],
        usage: ":systems",
        category: CommandCategory::Navigation,
        summary: "List ASDF's registered system definitions",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":load",
        aliases: &[":l"],
        usage: ":load <path>",
        category: CommandCategory::Navigation,
        summary: "Load a .lisp or .fasl file and index it for completions",
        arg_kind: CommandArgKind::FilePath,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":compile",
        aliases: &[],
        usage: ":compile <path> [output]",
        category: CommandCategory::Navigation,
        summary: "Compile a Lisp source file to bytecode (FASL)",
        arg_kind: CommandArgKind::FilePath,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":pwd",
        aliases: &[],
        usage: ":pwd",
        category: CommandCategory::Navigation,
        summary: "Show current working directory",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":cd",
        aliases: &[],
        usage: ":cd <dir>",
        category: CommandCategory::Navigation,
        summary: "Change working directory and *DEFAULT-PATHNAME-DEFAULTS*",
        arg_kind: CommandArgKind::FilePath,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":doc",
        aliases: &[":d"],
        usage: ":doc <symbol>",
        category: CommandCategory::Inspection,
        summary: "Show signature, kind, and documentation for a symbol",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":describe",
        aliases: &[":desc"],
        usage: ":describe <form>",
        category: CommandCategory::Inspection,
        summary: "Evaluate a form or inspect a symbol's bindings and properties",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: true,
    },
    CommandDef {
        name: ":apropos",
        aliases: &[],
        usage: ":apropos <pattern> [package]",
        category: CommandCategory::Inspection,
        summary: "Search symbols across packages with kind tags",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":macroexpand",
        aliases: &[":me"],
        usage: ":macroexpand <form>",
        category: CommandCategory::Inspection,
        summary: "Fully macroexpand a form and pretty-print the result",
        arg_kind: CommandArgKind::Form,
        takes_lisp_form: true,
    },
    CommandDef {
        name: ":macroexpand-1",
        aliases: &[":m1"],
        usage: ":macroexpand-1 <form>",
        category: CommandCategory::Inspection,
        summary: "Expand one macro step and pretty-print the result",
        arg_kind: CommandArgKind::Form,
        takes_lisp_form: true,
    },
    CommandDef {
        name: ":disassemble",
        aliases: &[":dis"],
        usage: ":disassemble [--native] <fn>",
        category: CommandCategory::Inspection,
        summary: "Disassemble bytecode or the installed JIT machine code",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":time",
        aliases: &[],
        usage: ":time <form>",
        category: CommandCategory::Inspection,
        summary: "Evaluate a form and report wall time, heap delta, and GC runs",
        arg_kind: CommandArgKind::Form,
        takes_lisp_form: true,
    },
    CommandDef {
        name: ":trace",
        aliases: &[],
        usage: ":trace [fn :condition form :print form :break form ...]",
        category: CommandCategory::Inspection,
        summary: "Trace calls with optional conditions, output forms, and debugger stops",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":break",
        aliases: &[],
        usage: ":break <fn> [pc | --line N] [--condition FORM] [--hit N] [--every N] [--log FORM] [--log-only] [--disabled] [--follow object|name|source]",
        category: CommandCategory::Inspection,
        summary: "Set a conditional stop/logpoint; optionally follow a redefined function or source line",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":break-file",
        aliases: &[],
        usage: ":break-file <path> <line> [breakpoint options]",
        category: CommandCategory::Inspection,
        summary: "Set a file/line breakpoint, pending until loaded; follows source definitions",
        arg_kind: CommandArgKind::FilePath,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":break-locations",
        aliases: &[],
        usage: ":break-locations <function>",
        category: CommandCategory::Inspection,
        summary: "List executable bytecode locations and their source positions",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":breakpoints",
        aliases: &[],
        usage: ":breakpoints",
        category: CommandCategory::Inspection,
        summary: "List active breakpoints",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":enable-breakpoint",
        aliases: &[],
        usage: ":enable-breakpoint <id>",
        category: CommandCategory::Inspection,
        summary: "Enable a breakpoint while preserving its options and hit count",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":disable-breakpoint",
        aliases: &[],
        usage: ":disable-breakpoint <id>",
        category: CommandCategory::Inspection,
        summary: "Disable a breakpoint while preserving its options and hit count",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":save-state",
        aliases: &[],
        usage: ":save-state \"path.zkclfasl\" symbol...",
        category: CommandCategory::Session,
        summary: "Save named value and function bindings for a later session",
        arg_kind: CommandArgKind::Form,
        takes_lisp_form: true,
    },
    CommandDef {
        name: ":delete-breakpoint",
        aliases: &[],
        usage: ":delete-breakpoint <id>",
        category: CommandCategory::Inspection,
        summary: "Delete a breakpoint by number",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":untrace",
        aliases: &[],
        usage: ":untrace [fn...]",
        category: CommandCategory::Inspection,
        summary: "Stop tracing specified functions (or all when omitted)",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":profile",
        aliases: &[],
        usage: ":profile [start [milliseconds] | stop | report | reset]",
        category: CommandCategory::Inspection,
        summary: "Measure function calls, wall time, safe-point samples, allocation, and GC",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":definition",
        aliases: &[":definitions"],
        usage: ":definition <name>",
        category: CommandCategory::Inspection,
        summary: "Show the running definition's kind, source, signature, and documentation",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":source",
        aliases: &[],
        usage: ":source <name>",
        category: CommandCategory::Inspection,
        summary: "Show the source snapshot belonging to a running definition",
        arg_kind: CommandArgKind::Symbol,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":dribble",
        aliases: &[],
        usage: ":dribble [path]",
        category: CommandCategory::Session,
        summary: "Record terminal Lisp input/output to a file, or stop recording",
        arg_kind: CommandArgKind::FilePath,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":save-image",
        aliases: &[],
        usage: ":save-image <path>",
        category: CommandCategory::Session,
        summary: "Save the idle Lisp runtime's definitions and state",
        arg_kind: CommandArgKind::FilePath,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":restore-image",
        aliases: &[],
        usage: ":restore-image <path>",
        category: CommandCategory::Session,
        summary: "Restore a saved runtime and reconnect this terminal",
        arg_kind: CommandArgKind::FilePath,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":room",
        aliases: &[],
        usage: ":room",
        category: CommandCategory::Inspection,
        summary: "Show managed Lisp heap usage and GC statistics",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":gc",
        aliases: &[],
        usage: ":gc",
        category: CommandCategory::Inspection,
        summary: "Trigger garbage collection and report reclaimed memory",
        arg_kind: CommandArgKind::None,
        takes_lisp_form: false,
    },
    CommandDef {
        name: ":type",
        aliases: &[],
        usage: ":type <form>",
        category: CommandCategory::Analysis,
        summary: "Show static inferred type and runtime (TYPE-OF ...) values",
        arg_kind: CommandArgKind::Form,
        takes_lisp_form: true,
    },
    CommandDef {
        name: ":lint",
        aliases: &[],
        usage: ":lint <form | @path>",
        category: CommandCategory::Analysis,
        summary: "Run zkcl linter on a form or file",
        arg_kind: CommandArgKind::Form,
        takes_lisp_form: true,
    },
    CommandDef {
        name: ":fmt",
        aliases: &[],
        usage: ":fmt <form | @path>",
        category: CommandCategory::Analysis,
        summary: "Format a Lisp form or file",
        arg_kind: CommandArgKind::Form,
        takes_lisp_form: true,
    },
];

pub(super) struct ParsedCommand<'a> {
    pub spec: &'static CommandDef,
    pub token: &'a str,
    pub args: &'a str,
    pub args_start: usize,
}

impl<'a> ParsedCommand<'a> {
    pub fn takes_file_or_form(&self) -> bool {
        matches!(self.spec.name, ":lint" | ":fmt")
    }

    pub fn lisp_args(&self) -> Option<&'a str> {
        (self.spec.takes_lisp_form && !(self.takes_file_or_form() && self.args.starts_with('@')))
            .then_some(self.args)
    }

    pub fn evaluates_form(&self) -> bool {
        matches!(self.spec.name, ":time" | ":type" | ":describe")
    }
}

pub(super) fn parse_command(input: &str) -> Option<ParsedCommand<'_>> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with(':') {
        return None;
    }
    let split = trimmed
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(trimmed.len(), |(idx, _)| idx);
    let token = &trimmed[..split];
    let args = trimmed[split..].trim_start();
    let spec = find_command(token)?;
    Some(ParsedCommand {
        spec,
        token,
        args,
        args_start: input.len() - args.len(),
    })
}

pub(super) fn find_command(token: &str) -> Option<&'static CommandDef> {
    COMMANDS.iter().find(|spec| {
        spec.name.eq_ignore_ascii_case(token)
            || spec
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(token))
    })
}

pub(super) fn find_help_command(arg: &str) -> Option<&'static CommandDef> {
    find_command(arg).or_else(|| {
        (!arg.starts_with(':'))
            .then(|| find_command(&format!(":{arg}")))
            .flatten()
    })
}

pub(super) fn render_help(
    theme: &ReplTheme,
    command: Option<&CommandDef>,
    line_editing: bool,
) -> String {
    if let Some(spec) = command {
        let mut out = String::new();
        let _ = writeln!(out, "{}", theme.paint(spec.usage, theme.command()));
        let _ = writeln!(out, "  {}", spec.summary);
        if spec.name == ":trace" {
            let _ = writeln!(
                out,
                "  Options before names apply to all names; later options apply to the preceding name."
            );
            let _ = writeln!(
                out,
                "  :condition/:print/:break also accept -after and -all; :report accepts TRACE or NIL."
            );
            let _ = writeln!(
                out,
                "  Option forms can read ZK::*TRACE-ARGUMENTS* and ZK::*TRACE-VALUES*."
            );
        }
        if spec.name == ":system" && line_editing {
            let _ = writeln!(
                out,
                "  Press Tab to complete a system name. With --system, use :system by itself to refresh the selected system."
            );
        }
        if spec.name == ":compile" {
            let _ = writeln!(
                out,
                "  Put double quotes around a path that contains spaces."
            );
        }
        if spec.name == ":break" || spec.name == ":break-file" {
            let _ = writeln!(
                out,
                "  Conditions and log forms run in the stopped frame; :break-file uses the same options."
            );
            let _ = writeln!(
                out,
                "  --log prints a form's first value; --log-only records it and continues execution."
            );
            let _ = writeln!(
                out,
                "  --hit N starts at hit N (default 1); --every K then selects N, N+K, ... . Lines start at 1."
            );
        }
        if !spec.aliases.is_empty() {
            let _ = writeln!(
                out,
                "  {} {}",
                theme.paint("Aliases:", theme.muted()),
                spec.aliases.join(", ")
            );
        }
        return out;
    }

    let mut out = String::new();
    let width = zkcl_terminal::terminal_width(zkcl_terminal::Stream::Stderr);
    let entry = |out: &mut String, key: &str, description: &str, style| {
        let indent: usize = if width >= 64 { 26 } else { 4 };
        let _ = write!(out, "  {}", theme.paint(key, style));
        let mut column = 2 + key.width();
        if width < 64 {
            out.push('\n');
            column = 0;
        }
        out.extend(std::iter::repeat_n(' ', indent.saturating_sub(column)));
        column = indent;
        for word in description.split_whitespace() {
            if column > indent && column + word.width() + 1 > width {
                out.push('\n');
                out.extend(std::iter::repeat_n(' ', indent));
                column = indent;
            } else if column > indent {
                out.push(' ');
                column += 1;
            }
            out.push_str(word);
            column += word.width();
        }
        out.push('\n');
    };
    for category in [
        CommandCategory::Session,
        CommandCategory::Navigation,
        CommandCategory::Inspection,
        CommandCategory::Analysis,
    ] {
        let _ = writeln!(out, "{}", theme.paint(category.title(), theme.heading()));
        for spec in COMMANDS.iter().filter(|s| s.category == category) {
            entry(&mut out, spec.name, spec.summary, theme.command());
        }
        out.push('\n');
    }

    if !line_editing {
        return out;
    }
    let _ = writeln!(out, "{}", theme.paint("Shortcuts", theme.heading()));
    for (key, desc) in [
        (
            "Enter",
            "evaluate complete input; insert newline when incomplete",
        ),
        ("Ctrl+J / Alt+Enter", "insert an indented newline"),
        (
            "Tab / Shift+Tab",
            "complete symbol, :command, or path; cycle menu",
        ),
        ("Ctrl+Space", "open completion menu"),
        ("Ctrl+R", "reverse incremental history search"),
        ("Up / Down", "navigate persistent history"),
        (
            "Ctrl+G / Ctrl+O",
            "edit current buffer in $VISUAL / $EDITOR",
        ),
        ("Ctrl+L", "clear the screen"),
        (
            "Ctrl+C",
            "clear input; pause running evaluation; repeat to cancel, then force stop",
        ),
        ("Ctrl+D", "leave the REPL on an empty line"),
        ("F1", "show this help"),
    ] {
        entry(&mut out, key, desc, theme.badge());
    }
    let _ = writeln!(
        out,
        "\n{}",
        theme.paint("Status Indicators", theme.heading())
    );
    let _ = writeln!(
        out,
        "  {}   returned values (subsequent values align below)",
        theme.paint("=>", theme.result_prefix())
    );
    let _ = writeln!(
        out,
        "  {}  statically inferred output type",
        theme.paint("→ type", theme.muted())
    );
    let _ = writeln!(
        out,
        "  {}  live constant preview of pure expressions",
        theme.paint("= val", theme.muted())
    );
    out
}

/// Label column widths of the REPL reports.
const ENVIRONMENT_LABEL_WIDTH: usize = 14;
const HEAP_LABEL_WIDTH: usize = 18;

/// Writes one aligned `label   value` row of a REPL report. The label is padded
/// before it is painted, so colour escapes never count towards the column width.
fn field_row(out: &mut String, theme: &ReplTheme, width: usize, label: &str, value: &str) {
    let padded = format!("{label:<width$}");
    let _ = writeln!(out, "  {} {value}", theme.paint(&padded, theme.muted()));
}

pub(super) fn render_env(
    theme: &ReplTheme,
    runtime: &Runtime,
    settings: &ReplSettings,
    jit_name: Option<&str>,
    loaded_files: &[PathBuf],
    project: Option<&str>,
) -> String {
    let pkg = runtime
        .current_package()
        .unwrap_or_else(|| "COMMON-LISP-USER".to_owned());
    let readtable = if runtime.raw_strings_enabled() {
        "raw-strings (#>)"
    } else {
        "standard"
    };
    let stats = runtime.heap_stats();
    let cwd = env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        theme.paint("Session Environment", theme.heading())
    );
    let row = |out: &mut String, label: &str, value: &str| {
        field_row(out, theme, ENVIRONMENT_LABEL_WIDTH, label, value)
    };
    row(
        &mut out,
        "Version",
        &format!("zkcl {}", env!("CARGO_PKG_VERSION")),
    );
    row(&mut out, "Package", &theme.paint(&pkg, theme.package()));
    row(&mut out, "Readtable", readtable);
    row(
        &mut out,
        "JIT",
        jit_name.unwrap_or("disabled (VM interpreter)"),
    );
    row(
        &mut out,
        "Heap",
        &format!(
            "{} committed / {} limit ({} GC runs)",
            format_bytes(stats.committed_bytes),
            format_bytes(stats.allocation_limit_bytes),
            stats.collections,
        ),
    );
    row(&mut out, "Directory", &cwd);
    if let Some(project) = project {
        row(&mut out, "Project", project);
    }
    if !loaded_files.is_empty() {
        let list: Vec<String> = loaded_files
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        row(&mut out, "Loaded", &list.join(", "));
    }
    row(
        &mut out,
        "Settings",
        &format!(
            "timing={}  pretty={}  rainbow={}  preview={}  hints={}  mode={}",
            settings.timing.as_str(),
            on_off(settings.pretty),
            on_off(settings.rainbow),
            on_off(settings.preview),
            on_off(settings.hints),
            if settings.vi_mode { "vi" } else { "emacs" },
        ),
    );
    out
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

pub(super) fn parse_bool_setting(val: &str) -> Option<bool> {
    match val.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Some(true),
        "off" | "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

pub(super) fn apply_setting(
    settings: &mut ReplSettings,
    args: &str,
    theme: &ReplTheme,
) -> Result<(String, bool), String> {
    let mut parts = args.split_whitespace();
    let Some(key) = parts.next() else {
        let mut out = String::new();
        let _ = writeln!(out, "{}", theme.paint("REPL Settings", theme.heading()));
        for (k, v, desc) in [
            ("timing", settings.timing.as_str(), "auto | on | off"),
            ("pretty", on_off(settings.pretty), "on | off"),
            ("rainbow", on_off(settings.rainbow), "on | off"),
            ("preview", on_off(settings.preview), "on | off"),
            ("hints", on_off(settings.hints), "on | off"),
            (
                "edit-mode",
                if settings.vi_mode { "vi" } else { "emacs" },
                "emacs | vi",
            ),
        ] {
            let _ = writeln!(
                out,
                "  {:<12} {:<8} {}",
                theme.paint(k, theme.command()),
                theme.paint(v, theme.badge()),
                theme.paint(desc, theme.muted()),
            );
        }
        return Ok((out, false));
    };

    let Some(val) = parts.next() else {
        return Err(format!("Usage: :set {key} <value>"));
    };
    if parts.next().is_some() {
        return Err("Usage: :set <option> <value>".into());
    }

    let mut needs_rebuild = false;
    let summary = match key.to_ascii_lowercase().as_str() {
        "timing" => {
            settings.timing = match val.to_ascii_lowercase().as_str() {
                "auto" => TimingMode::Auto,
                "on" | "true" | "1" => TimingMode::On,
                "off" | "false" | "0" => TimingMode::Off,
                _ => return Err("timing expects auto, on, or off".into()),
            };
            format!("timing = {}", settings.timing.as_str())
        }
        "pretty" => {
            settings.pretty = parse_bool_setting(val).ok_or("pretty expects on or off")?;
            format!("pretty = {}", on_off(settings.pretty))
        }
        "rainbow" => {
            settings.rainbow = parse_bool_setting(val).ok_or("rainbow expects on or off")?;
            format!("rainbow = {}", on_off(settings.rainbow))
        }
        "preview" => {
            settings.preview = parse_bool_setting(val).ok_or("preview expects on or off")?;
            format!("preview = {}", on_off(settings.preview))
        }
        "hints" => {
            settings.hints = parse_bool_setting(val).ok_or("hints expects on or off")?;
            needs_rebuild = true;
            format!("hints = {}", on_off(settings.hints))
        }
        "edit-mode" | "mode" => {
            settings.vi_mode = match val.to_ascii_lowercase().as_str() {
                "emacs" => false,
                "vi" => true,
                _ => return Err("edit-mode expects emacs or vi".into()),
            };
            needs_rebuild = true;
            format!(
                "edit-mode = {}",
                if settings.vi_mode { "vi" } else { "emacs" }
            )
        }
        _ => {
            return Err(format!(
                "Unknown setting '{key}'. Available: timing, pretty, rainbow, preview, hints, edit-mode."
            ));
        }
    };
    Ok((summary, needs_rebuild))
}

pub(super) fn run_package_command(
    runtime: &Runtime,
    args: &str,
    theme: &ReplTheme,
) -> Result<String, String> {
    if args.is_empty() {
        let snap = runtime.namespace_snapshot();
        let cur = runtime
            .current_package()
            .unwrap_or_else(|| "COMMON-LISP-USER".into());
        let mut out = format!("Current package: {}", theme.paint(&cur, theme.package()));
        if let Some(info) = snap.packages.iter().find(|p| p.is_current) {
            if !info.nicknames.is_empty() {
                let _ = write!(out, " (nicknames: {})", info.nicknames.join(", "));
            }
            let _ = write!(
                out,
                " — {} exported, {} internal/present symbols",
                info.external_count, info.total_count
            );
        }
        out.push('\n');
        return Ok(out);
    }
    let name = switch_package(runtime, args).map_err(|e| e.message().to_owned())?;
    let out = format!(
        "Switched package to {}\n",
        theme.paint(&name, theme.package())
    );
    Ok(out)
}

pub(super) fn run_packages_command(runtime: &Runtime, theme: &ReplTheme) -> String {
    let snap = runtime.namespace_snapshot();
    let mut out = String::new();
    let _ = writeln!(out, "{}", theme.paint("Live Packages", theme.heading()));
    for pkg in &snap.packages {
        let marker = if pkg.is_current {
            theme.paint("* ", theme.prompt_arrow())
        } else {
            "  ".to_owned()
        };
        let nicks = if pkg.nicknames.is_empty() {
            String::new()
        } else {
            format!(" ({})", pkg.nicknames.join(", "))
        };
        let name_styled = if pkg.is_current {
            theme.paint(&pkg.name, theme.package())
        } else {
            pkg.name.clone()
        };
        let stats = theme.paint(
            &format!("[{} ext / {} sym]", pkg.external_count, pkg.total_count),
            theme.muted(),
        );
        let _ = writeln!(out, "{marker}{name_styled}{nicks}  {stats}");
    }
    out
}

pub(super) fn run_cd_command(
    runtime: &Runtime,
    args: &str,
    theme: &ReplTheme,
) -> Result<String, Box<dyn Error>> {
    let target = if args.is_empty() {
        env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set; pass a directory to :cd")?
    } else {
        command_path(args)?
    };
    let previous = env::current_dir()?;
    let label = zkcl_terminal::display_path(&target);
    let canonical = target
        .canonicalize()
        .map_err(|error| format!("cannot resolve directory {label}: {error}"))?;
    let directory = canonical
        .to_str()
        .ok_or("the directory name must be UTF-8 to set the Lisp pathname defaults")?;
    env::set_current_dir(&canonical).map_err(|error| format!("cannot enter {label}: {error}"))?;
    if let Err(error) = sync_lisp_cwd(runtime, &canonical) {
        if let Err(restore) = env::set_current_dir(&previous) {
            return Err(format!(
                "{error}\nCannot return to {}: {restore}",
                zkcl_terminal::display_path(&previous)
            )
            .into());
        }
        return Err(error.into());
    }
    Ok(format!(
        "{}\n",
        theme.paint(&zkcl_terminal::single_line(directory), theme.command_arg())
    ))
}

pub(super) fn run_describe_command(
    runtime: &Runtime,
    args: &str,
    theme: &ReplTheme,
) -> Result<String, EvalError> {
    let text = eval_describe(runtime, args)?;
    Ok(highlight_lisp_output(&text, theme))
}

pub(super) fn run_apropos_command(
    runtime: &Runtime,
    args: &str,
    theme: &ReplTheme,
) -> Result<String, String> {
    let mut parts = args.split_whitespace();
    let Some(pattern) = parts.next() else {
        return Err("Usage: :apropos <pattern> [package]".into());
    };
    let pkg = parts.next();
    if parts.next().is_some() {
        return Err("Usage: :apropos <pattern> [package]".into());
    }
    let entries = eval_apropos(runtime, pattern, pkg).map_err(|e| e.message().to_owned())?;
    if entries.is_empty() {
        return Ok(format!(
            "{}\n",
            theme.paint(&format!("No symbols matching {pattern:?}."), theme.muted())
        ));
    }
    let total = entries.len();
    let limit = 80;
    let mut out = String::new();
    for entry in entries.iter().take(limit) {
        let kind_tags = &entry.kind_tags;
        let name = &entry.qualified_name;
        let tags = if kind_tags.is_empty() {
            String::new()
        } else {
            theme.paint(&format!(" [{}]", kind_tags.join(",")), theme.muted())
        };
        let styled_name = if kind_tags.contains(&"mac") {
            theme.paint(name, theme.macro_style())
        } else if kind_tags.contains(&"sp") {
            theme.paint(name, theme.operator())
        } else if kind_tags.contains(&"fn") {
            theme.paint(name, theme.function())
        } else if kind_tags.contains(&"var") || kind_tags.contains(&"const") {
            theme.paint(name, theme.variable())
        } else if kind_tags.contains(&"class") {
            theme.paint(name, theme.type_style())
        } else {
            name.clone()
        };
        let _ = writeln!(out, "  {styled_name}{tags}");
    }
    if total > limit {
        let _ = writeln!(
            out,
            "{}",
            theme.paint(
                &format!(
                    "  ... and {} more (narrow with a package or longer pattern)",
                    total - limit
                ),
                theme.muted()
            )
        );
    }
    Ok(out)
}

pub(super) fn run_macroexpand_command(
    runtime: &Runtime,
    args: &str,
    once: bool,
    theme: &ReplTheme,
) -> Result<String, EvalError> {
    let (expanded, changed) = eval_macroexpand(runtime, args, once)?;
    let mut out = String::new();
    if !changed {
        let _ = writeln!(
            out,
            "{}",
            theme.paint(
                "; macro expansion left this form unchanged",
                theme.comment()
            )
        );
    }
    let _ = writeln!(out, "{}", highlight_lisp_output(&expanded, theme));
    Ok(out)
}

pub(super) fn run_disassemble_command(
    runtime: &Runtime,
    args: &str,
    theme: &ReplTheme,
) -> Result<String, String> {
    let text = if let Some(args) = args
        .strip_prefix("--native")
        .filter(|rest| rest.starts_with(char::is_whitespace))
    {
        super::debugger::disassemble_native(runtime, args)?
    } else if args.is_empty() || args == "--native" {
        return Err("Usage: :disassemble [--native] <function>".into());
    } else {
        eval_disassemble(runtime, args).map_err(|error| error.message().to_owned())?
    };
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with(';') {
            let _ = writeln!(out, "{}", theme.paint(line, theme.heading()));
        } else if let Some((pc, instr)) = line.split_once("  ")
            && pc.chars().all(|c| c.is_ascii_digit())
        {
            let _ = writeln!(
                out,
                "{}  {}",
                theme.paint(pc, theme.muted()),
                highlight_lisp_output(instr, theme)
            );
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    Ok(out)
}

pub(super) fn run_doc_command(
    runtime: &Runtime,
    workspace: &mut SourceWorkspace,
    args: &str,
    theme: &ReplTheme,
) -> Result<String, String> {
    let target = args.trim();
    if target.is_empty() {
        return Err("Usage: :doc <symbol>".into());
    }
    let mut sections: Vec<_> =
        source_documentation(workspace, runtime.current_package(), target, theme)
            .into_iter()
            .collect();

    // 2. Runtime documentation
    if let Ok(runtime_docs) = eval_runtime_doc(runtime, target) {
        for (kind, doc) in runtime_docs {
            let formatted = format!(
                "{} {}",
                theme.paint(&format!("[{kind}]"), theme.badge()),
                doc
            );
            if !sections.iter().any(|s: &String| s.contains(&doc)) {
                sections.push(formatted);
            }
        }
    }

    if sections.is_empty() {
        return Ok(format!(
            "{}\n",
            theme.paint(
                &format!("No documentation found for {target}."),
                theme.muted()
            )
        ));
    }
    Ok(format!("{}\n", sections.join("\n\n")))
}

pub(super) fn source_documentation(
    workspace: &mut SourceWorkspace,
    package: Option<String>,
    target: &str,
    theme: &ReplTheme,
) -> Option<String> {
    let document = "__doc_probe__";
    workspace.set_document(document, target);
    workspace.set_initial_package(document, package);
    let hover = workspace.hover(document, 0);
    workspace.remove_document(document);
    hover.map(|hover| format_hover_markdown(&hover.contents, theme))
}

fn format_hover_markdown(md: &str, theme: &ReplTheme) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for line in md.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        if in_code {
            out.push_str(&highlight_lisp_output(line, theme));
        } else {
            out.push_str(line);
        }
    }
    out
}

pub(super) fn run_type_command(
    runtime: &Runtime,
    static_type: Option<&'static str>,
    args: &str,
    theme: &ReplTheme,
) -> Result<String, EvalError> {
    let mut out = String::new();
    if let Some(st) = static_type {
        let _ = writeln!(
            out,
            "  {} {}",
            theme.paint("Inferred type", theme.muted()),
            theme.paint(st, theme.type_style())
        );
    }
    let pairs = eval_runtime_types(runtime, args)?;
    if pairs.is_empty() {
        let _ = writeln!(out, "  {}", theme.paint("; no values", theme.muted()));
    } else {
        for (val, ty) in pairs {
            let _ = writeln!(
                out,
                "  {} {}   {}",
                theme.paint("=>", theme.result_prefix()),
                zkcl_terminal::single_line(&val),
                theme.paint(
                    &format!("→ {}", zkcl_terminal::single_line(&ty)),
                    theme.type_style()
                )
            );
        }
    }
    Ok(out)
}

pub(super) fn run_time_command(
    runtime: &Runtime,
    analysis: &super::EditorAnalysis,
    args: &str,
    printer: &mut super::ReplPrinter,
) -> Result<(), Box<dyn Error>> {
    let before_stats = runtime.heap_stats();
    let t0 = Instant::now();
    let pretty = analysis.settings().pretty;
    let document = SourceDocument::input(args).with_uri("<repl>");
    let mut evaluated_end = 0;
    let result = runtime.eval_document_with_offsets(&document, |end, values| {
        evaluated_end = end;
        printer.print_values(runtime, values, pretty)
    });
    let elapsed = t0.elapsed();
    analysis.record_submission(&args[..evaluated_end], runtime.current_package());
    analysis.set_last_duration(Some(elapsed));
    result?;
    let after_stats = runtime.heap_stats();

    let theme = printer.theme;
    if evaluated_end == 0 {
        printer.write_block(&theme.paint("No forms evaluated.", theme.muted()))?;
    }
    let heap_delta = after_stats
        .committed_bytes
        .saturating_sub(before_stats.committed_bytes);
    let gc_delta = after_stats
        .collections
        .saturating_sub(before_stats.collections);
    printer.write_block(&theme.paint(
        &format!(
            "  time {}   heap +{} ({} committed)   gc {}",
            format_duration(elapsed),
            format_bytes(heap_delta),
            format_bytes(after_stats.committed_bytes),
            gc_delta,
        ),
        theme.muted(),
    ))?;
    Ok(())
}

pub(super) fn run_trace_command(
    runtime: &Runtime,
    trace: bool,
    args: &str,
    theme: &ReplTheme,
) -> Result<String, String> {
    let result = eval_trace_op(runtime, trace, args).map_err(|e| e.message().to_owned())?;
    if result.is_empty() {
        Ok(format!(
            "{}\n",
            theme.paint(
                if trace {
                    "No functions currently traced."
                } else {
                    "All functions untraced."
                },
                theme.muted()
            )
        ))
    } else {
        Ok(format!("{}\n", highlight_lisp_output(&result, theme)))
    }
}

pub(super) fn run_room_command(runtime: &Runtime, theme: &ReplTheme) -> String {
    let stats = runtime.heap_stats();
    let mut out = String::new();
    let _ = writeln!(out, "{}", theme.paint("Managed Lisp Heap", theme.heading()));
    let row = |out: &mut String, label: &str, value: &str| {
        field_row(out, theme, HEAP_LABEL_WIDTH, label, value)
    };
    row(&mut out, "Committed:", &format_bytes(stats.committed_bytes));
    row(
        &mut out,
        "Allocation limit:",
        &format_bytes(stats.allocation_limit_bytes),
    );
    row(
        &mut out,
        "Large objects:",
        &format_bytes(stats.large_object_bytes),
    );
    row(&mut out, "Collections:", &stats.collections.to_string());
    if let Some(last) = stats.last_collection_duration {
        row(&mut out, "Last GC time:", &format_duration(last));
    }
    out
}

pub(super) fn run_gc_command(runtime: &Runtime, theme: &ReplTheme) -> String {
    let before = runtime.heap_stats();
    let t0 = Instant::now();
    runtime.collect();
    let elapsed = t0.elapsed();
    let after = runtime.heap_stats();
    let freed = before.committed_bytes.saturating_sub(after.committed_bytes);
    format!(
        "{}\n",
        theme.paint(
            &format!(
                "GC complete in {}: {} freed ({} committed)",
                format_duration(elapsed),
                format_bytes(freed),
                format_bytes(after.committed_bytes)
            ),
            theme.muted()
        )
    )
}

pub(super) fn analyze_source_command(
    runtime: &Runtime,
    args: &str,
    format: bool,
    printer: &super::ReplPrinter,
) -> Result<(), Box<dyn Error>> {
    let from_file = args.strip_prefix('@');
    let source = if let Some(path_str) = from_file {
        let path = match command_path(path_str) {
            Ok(path) => path,
            Err(message) => {
                let diagnostic = crate::diagnostics::error_with_code("repl/source", message);
                printer.write_reports([&diagnostic])?;
                return Ok(());
            }
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                let diagnostic = crate::diagnostics::io_error(
                    "repl/source",
                    "cannot read Lisp source",
                    Some(&path),
                    &error,
                );
                printer.write_reports([&diagnostic])?;
                return Ok(());
            }
        };
        SourceDocument::file(path, Some(text))
    } else {
        if runtime.custom_readtable() || !runtime.standard_source_reader_bindings() {
            let mut diagnostic = crate::diagnostics::error_with_code(
                "repl/readtable",
                "the active reader settings require evaluation to read this input",
            );
            crate::diagnostics::help(
                &mut diagnostic,
                "pass `@path` to analyze a file with a built-in readtable",
            );
            printer.write_reports([&diagnostic])?;
            return Ok(());
        }
        SourceDocument::input(args).with_uri("<repl>")
    };
    let source_name = "repl";
    let mut ws = SourceWorkspace::new();
    ws.set_document_source(source_name, source)
        .expect("REPL lint has source text");
    ws.set_initial_package(source_name, runtime.current_package());
    ws.set_initial_readtable(
        source_name,
        if from_file.is_none() && runtime.raw_strings_enabled() {
            SourceReadtable::RawStrings
        } else {
            SourceReadtable::Standard
        },
    );
    let snapshot = ws.snapshot(source_name).expect("REPL source was set");
    let mut reports: Vec<_> = snapshot
        .diagnostics()
        .iter()
        .map(|diagnostic| diagnostic.to_diagnostic(snapshot.source()))
        .collect();
    let theme = &printer.theme;
    if format {
        if let Some(formatted) = snapshot.formatted_text() {
            printer.write_block(&highlight_lisp_output(&formatted, theme))?;
        } else {
            if reports.is_empty() {
                reports.push(crate::analysis::formatter_refused(snapshot.source()));
            }
            printer.write_reports(&reports)?;
        }
        return Ok(());
    }
    reports.extend(
        snapshot
            .documentation_tests()
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.to_diagnostic(snapshot.source())),
    );
    let config = LintConfig::from_preset(LintPreset::Strict);
    reports.extend(
        ws.lint_document(source_name, &config)
            .into_iter()
            .filter_map(|diagnostic| diagnostic.to_diagnostic(snapshot.source(), |_| None)),
    );
    if reports.is_empty() {
        printer.write_block(&theme.paint("No lint or syntax issues.", theme.muted()))?;
    } else {
        printer.write_reports(&reports)?;
    }
    Ok(())
}

pub(super) fn unquote_path(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

pub(super) fn path_word_end(text: &str) -> Option<usize> {
    if text.starts_with('"') {
        quoted_command_path(text).1
    } else {
        Some(text.find(char::is_whitespace).unwrap_or(text.len()))
    }
}

fn quoted_command_path(text: &str) -> (String, Option<usize>) {
    let mut path = String::new();
    let mut chars = text[1..].char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => return (path, Some(index + 2)),
            '\\' if chars
                .peek()
                .is_some_and(|(_, next)| matches!(next, '"' | '\\')) =>
            {
                path.push(chars.next().unwrap().1);
            }
            _ => path.push(ch),
        }
    }
    (path, None)
}

pub(super) fn command_path_prefix(text: &str) -> Cow<'_, str> {
    if text.starts_with('"') {
        Cow::Owned(quoted_command_path(text).0)
    } else {
        Cow::Borrowed(text)
    }
}

pub(super) fn command_path(text: &str) -> Result<PathBuf, &'static str> {
    let text = text.trim();
    let path = if text.starts_with('"') {
        let (path, end) = quoted_command_path(text);
        let end = end.ok_or("close the quoted path with `\"`")?;
        if end != text.len() {
            return Err("unexpected text after the quoted path");
        }
        Cow::Owned(path)
    } else {
        Cow::Borrowed(text)
    };
    if path.is_empty() {
        return Err("pass a non-empty path");
    }
    Ok(expand_tilde(&path))
}

pub(super) fn compile_paths(args: &str) -> Result<(PathBuf, Option<PathBuf>), &'static str> {
    const USAGE: &str = "Usage: :compile <path> [output]";
    let take_word = |text: &str| path_word_end(text).ok_or("close the quoted path with `\"`");
    let args = args.trim();
    let end = take_word(args)?;
    let tail = &args[end..];
    if end == 0 || (!tail.is_empty() && !tail.starts_with(char::is_whitespace)) {
        return Err(USAGE);
    }
    let source = command_path(&args[..end])?;
    let tail = tail.trim_start();
    let output = if tail.is_empty() {
        None
    } else {
        let end = take_word(tail)?;
        if end != tail.len() {
            return Err(USAGE);
        }
        Some(command_path(tail)?)
    };
    Ok((source, output))
}

pub(super) fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~"
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return Path::new(&home).join(rest);
    }
    PathBuf::from(raw)
}
