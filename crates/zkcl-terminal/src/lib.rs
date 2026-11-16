//! Shared terminal policy and the single human diagnostic renderer.
//!
//! Reports retain their own source snapshots. Rendering never reads a source
//! file from disk and never interprets a newer revision using old offsets.

use std::borrow::Cow;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use annotate_snippets::{AnnotationKind, Group, Level, Origin, Patch, Renderer, Snippet};
use zkcl_diagnostics::{Applicability, Diagnostic, LabelRole, NoteKind, Severity, SnapshotId};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[repr(u8)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

static COLOR_MODE: AtomicU8 = AtomicU8::new(ColorMode::Auto as u8);

/// Set the process's output policy after parsing global options.
pub fn configure(color: ColorMode) {
    COLOR_MODE.store(color as u8, Ordering::Relaxed);
}

pub fn color_mode() -> ColorMode {
    match COLOR_MODE.load(Ordering::Relaxed) {
        1 => ColorMode::Always,
        2 => ColorMode::Never,
        _ => ColorMode::Auto,
    }
}

fn color_enabled(mode: ColorMode, terminal: bool, no_color: bool, dumb: bool) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => terminal && !no_color && !dumb,
    }
}

pub fn use_color(stream: Stream) -> bool {
    color_for(color_mode(), stream)
}

pub fn color_for(mode: ColorMode, stream: Stream) -> bool {
    let terminal = match stream {
        Stream::Stdout => std::io::stdout().is_terminal(),
        Stream::Stderr => std::io::stderr().is_terminal(),
    };
    color_enabled(
        mode,
        terminal,
        std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
        std::env::var_os("TERM").is_some_and(|value| value == "dumb"),
    )
}

/// Resolve automatic policy for the actual destination before passing it to clap.
pub fn clap_color(mode: ColorMode, stream: Stream) -> clap::ColorChoice {
    if color_for(mode, stream) {
        clap::ColorChoice::Always
    } else {
        clap::ColorChoice::Never
    }
}

/// Discover output policy from the actual command tree even when help or a
/// usage error prevents construction of the typed command request. Builtin
/// display flags become inert for this probe; ordinary arguments still use
/// their declared arities and value parsers.
pub fn command_color(command: clap::Command, args: &[OsString], fallback: ColorMode) -> ColorMode {
    fn without_builtin_displays(command: clap::Command) -> clap::Command {
        command
            .disable_help_flag(true)
            .disable_help_subcommand(true)
            .disable_version_flag(true)
            .mut_subcommands(without_builtin_displays)
    }
    without_builtin_displays(command)
        .arg(
            clap::Arg::new("__output_help")
                .long("help")
                .short('h')
                .global(true)
                .hide(true)
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            clap::Arg::new("__output_version")
                .long("version")
                .short('V')
                .global(true)
                .hide(true)
                .action(clap::ArgAction::SetTrue),
        )
        .ignore_errors(true)
        .try_get_matches_from(args.iter())
        .ok()
        .and_then(|matches| {
            matches
                .try_get_one::<ColorMode>("color")
                .ok()
                .flatten()
                .copied()
        })
        .unwrap_or(fallback)
}

pub fn terminal_width(stream: Stream) -> usize {
    let size = match stream {
        Stream::Stdout => terminal_size::terminal_size_of(std::io::stdout()),
        Stream::Stderr => terminal_size::terminal_size_of(std::io::stderr()),
    };
    size.map_or(100, |(terminal_size::Width(width), _)| usize::from(width))
}

/// Display a path relative to the current directory, including outside paths.
/// This is lexical: presentation does not require the path to still exist.
pub fn display_path(path: &Path) -> String {
    let relative = std::env::current_dir()
        .ok()
        .and_then(|cwd| pathdiff::diff_paths(path, cwd));
    let path = relative.as_deref().unwrap_or(path);
    if path.as_os_str().is_empty() {
        return ".".into();
    }
    single_line(&path.to_string_lossy())
}

/// Escape text shown inside a prompt, path, or other one-line terminal field.
pub fn single_line(text: &str) -> String {
    let mut line = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => line.push_str("\\n"),
            '\r' => line.push_str("\\r"),
            '\t' => line.push_str("\\t"),
            c if c.is_control()
                || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                let _ = write!(line, "\\u{{{:x}}}", c as u32);
            }
            c => line.push(c),
        }
    }
    line
}

fn escape_c1(rendered: String) -> String {
    if rendered
        .chars()
        .any(|character| ('\u{80}'..='\u{9f}').contains(&character))
    {
        let mut safe = String::with_capacity(rendered.len());
        for character in rendered.chars() {
            if ('\u{80}'..='\u{9f}').contains(&character) {
                safe.extend(character.escape_default());
            } else {
                safe.push(character);
            }
        }
        safe
    } else {
        rendered
    }
}

fn safe_text(text: &str) -> String {
    escape_c1(annotate_snippets::normalize_untrusted_str(text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub color: bool,
    pub width: usize,
    pub short: bool,
}

impl RenderOptions {
    pub fn for_stream(stream: Stream) -> Self {
        Self {
            color: use_color(stream),
            width: terminal_width(stream),
            short: false,
        }
    }

    pub fn stderr() -> Self {
        Self::for_stream(Stream::Stderr)
    }
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self::stderr()
    }
}

fn level(severity: Severity) -> Level<'static> {
    match severity {
        Severity::Error => Level::ERROR,
        Severity::Warning => Level::WARNING,
        Severity::Note => Level::NOTE,
        Severity::Help => Level::HELP,
    }
}

fn applicability(value: Applicability) -> &'static str {
    match value {
        Applicability::MachineApplicable => "machine applicable",
        Applicability::MaybeIncorrect => "may be incorrect",
        Applicability::HasPlaceholders => "contains placeholders",
        Applicability::Unspecified => "applicability unspecified",
    }
}

fn source_name(source: &zkcl_diagnostics::SourceDocument) -> String {
    source
        .path()
        .map(display_path)
        .unwrap_or_else(|| single_line(source.uri().unwrap_or("<input>")))
}

/// The source model also recognizes lone CR line endings. Converting them
/// to LF for annotate-snippets preserves every source byte boundary.
fn snippet_text(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut starts = text
        .match_indices('\r')
        .map(|(offset, _)| offset)
        .filter(|&offset| bytes.get(offset + 1) != Some(&b'\n'));
    let Some(first) = starts.next() else {
        return Cow::Borrowed(text);
    };
    let mut display = String::with_capacity(text.len());
    let mut previous = 0;
    for offset in std::iter::once(first).chain(starts) {
        display.push_str(&text[previous..offset]);
        display.push('\n');
        previous = offset + 1;
    }
    display.push_str(&text[previous..]);
    Cow::Owned(display)
}

/// Render annotations, related locations and atomic suggested edit groups using
/// annotate-snippets. All offsets refer to the source snapshots in the report.
pub fn render(diagnostic: &Diagnostic, options: &RenderOptions) -> String {
    let message = if options.short {
        let mut message = single_line(&diagnostic.message);
        if !diagnostic
            .labels
            .iter()
            .any(|label| label.role == LabelRole::Primary)
            && let Some(path) = diagnostic
                .contexts
                .iter()
                .find_map(|entry| entry.path.as_deref())
        {
            let _ = write!(message, " ({})", display_path(path));
        }
        Cow::Owned(message)
    } else {
        Cow::Borrowed(diagnostic.message.as_str())
    };
    let mut title = level(diagnostic.severity).primary_title(message);
    if let Some(code) = &diagnostic.code {
        title = title.id(single_line(code));
    }
    let mut primary = Group::with_title(title);

    // The primary location leads even when a producer supplied related labels
    // first. Preserve producer order within each class and within each file.
    let mut snapshots = Vec::<SnapshotId>::new();
    for role in [LabelRole::Primary, LabelRole::Secondary] {
        for label in diagnostic.labels.iter().filter(|label| label.role == role) {
            if !snapshots.contains(&label.span.snapshot()) {
                snapshots.push(label.span.snapshot());
            }
        }
    }
    for snapshot in snapshots {
        let labels = diagnostic
            .labels
            .iter()
            .filter(|label| label.span.snapshot() == snapshot);
        let Some(source) = diagnostic.source(snapshot) else {
            // Public report fields can refer to an unavailable snapshot. Keep
            // its explanation without inventing a source location.
            for label in labels {
                primary = primary.element(Level::NOTE.message(safe_text(&label.message)));
            }
            continue;
        };
        let path = source_name(source);
        if let Some(text) = source.text() {
            let annotations = labels.map(|label| {
                let kind = match label.role {
                    LabelRole::Primary => AnnotationKind::Primary,
                    LabelRole::Secondary => AnnotationKind::Context,
                };
                let message = if options.short {
                    single_line(&label.message)
                } else {
                    safe_text(&label.message)
                };
                kind.span(label.span.range()).label(message)
            });
            primary = primary.element(
                Snippet::source(snippet_text(text))
                    .path(path)
                    .annotations(annotations),
            );
        } else {
            primary = primary.element(Origin::path(path));
            primary = primary.element(Level::NOTE.message("source text is unavailable"));
            for label in labels {
                let range = label.span.byte_range();
                let message = if label.message.is_empty() {
                    format!("source bytes {}..{}", range.start(), range.end())
                } else {
                    format!(
                        "{} at source bytes {}..{}",
                        safe_text(&label.message),
                        range.start(),
                        range.end()
                    )
                };
                primary = primary.element(Level::NOTE.message(message));
            }
        }
    }
    if let Some(rationale) = &diagnostic.rationale {
        primary = primary.element(Level::NOTE.message(safe_text(rationale)));
    }
    for context in &diagnostic.contexts {
        let message = match &context.path {
            Some(path) => format!("{}: {}", context.message, display_path(path)),
            None => context.message.clone(),
        };
        primary = primary.element(Level::NOTE.message(safe_text(&message)));
    }
    for note in &diagnostic.notes {
        let level = match note.kind {
            NoteKind::Note => Level::NOTE,
            NoteKind::Help => Level::HELP,
        };
        primary = primary.element(level.message(safe_text(&note.message)));
    }
    let mut groups = vec![primary];
    for suggestion in diagnostic.suggestions.iter().filter(|_| !options.short) {
        let mut group = Group::with_title(Level::HELP.secondary_title(format!(
            "{} ({})",
            safe_text(&suggestion.message),
            applicability(suggestion.applicability)
        )));
        let mut snapshots = Vec::new();
        for edit in suggestion.edits() {
            if !snapshots.contains(&edit.span().snapshot()) {
                snapshots.push(edit.span().snapshot());
            }
        }
        for snapshot in snapshots {
            let mut edits: Vec<_> = suggestion
                .edits()
                .iter()
                .filter(|edit| edit.span().snapshot() == snapshot)
                .collect();
            edits.sort_by_key(|edit| (edit.span().range().start, edit.span().range().end));
            let source = edits[0].source();
            // SourceEdit construction guarantees retained text and valid spans.
            let text = source
                .text()
                .expect("validated source edit retains its text");
            let patches: Vec<_> = edits
                .iter()
                .map(|edit| Patch::new(edit.span().range(), snippet_text(edit.replacement())))
                .collect();
            // Multiple replacements in a file form one patch preview, rather
            // than independent suggestions that a user might apply partially.
            group = group.element(
                Snippet::source(snippet_text(text))
                    .path(source_name(source))
                    .patches(patches),
            );
        }
        groups.push(group);
    }
    let renderer = if options.color {
        Renderer::styled()
    } else {
        Renderer::plain()
    };
    let rendered = renderer
        .term_width(options.width.max(1))
        .short_message(options.short)
        .render(&groups);
    // annotate-snippets replaces C0 controls and bidi controls before layout.
    // Its current normalization table leaves C1 controls untouched; escape
    // those too while retaining the ANSI sequences produced by styled output.
    escape_c1(rendered)
}

pub fn render_stderr(diagnostic: &Diagnostic) -> String {
    render(diagnostic, &RenderOptions::stderr())
}

pub fn write_reports<'a>(
    output: &mut impl Write,
    reports: impl IntoIterator<Item = &'a Diagnostic>,
    options: &RenderOptions,
) -> io::Result<()> {
    for (index, report) in reports.into_iter().enumerate() {
        if index > 0 && !options.short {
            writeln!(output)?;
        }
        writeln!(output, "{}", render(report, options))?;
    }
    Ok(())
}

/// Emit a diagnostic while handling a command failure. A closed diagnostic
/// stream does not replace the original error or its exit status.
pub fn emit(diagnostic: &Diagnostic) {
    let _ = write_reports(
        &mut io::stderr().lock(),
        [diagnostic],
        &RenderOptions::stderr(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use zkcl_diagnostics::{Note, SourceDocument, SourceEdit, Suggestion};

    fn plain(width: usize) -> RenderOptions {
        RenderOptions {
            color: false,
            width,
            short: false,
        }
    }

    #[test]
    fn renders_primary_related_files_and_atomic_edit_previews() {
        let source = SourceDocument::file("main.lisp".into(), Some("(let* ((x 1)) x)\n".into()));
        let definition =
            SourceDocument::file("library.lisp".into(), Some("(defun f (x) x)\n".into()));
        let mut diagnostic =
            Diagnostic::new(Severity::Warning, "sequential bindings are unnecessary");
        diagnostic.code = Some("complexity/redundant-let".into());
        diagnostic
            .add_label(&definition, 10..11, LabelRole::Secondary, "declared here")
            .unwrap();
        diagnostic
            .add_label(
                &source,
                1..5,
                LabelRole::Primary,
                "no binding refers to an earlier one",
            )
            .unwrap();
        diagnostic.notes.push(Note {
            kind: NoteKind::Note,
            message: "both forms are valid Common Lisp".into(),
        });
        diagnostic
            .contexts
            .push(zkcl_diagnostics::DiagnosticContext {
                message: "while loading".into(),
                path: Some(std::env::current_dir().unwrap().join("cached.fasl")),
            });
        diagnostic.suggestions.push(
            Suggestion::new(
                "use parallel bindings",
                Applicability::MachineApplicable,
                vec![
                    SourceEdit::new(source.clone(), 1..5, "let").unwrap(),
                    SourceEdit::new(definition, 7..8, "g").unwrap(),
                ],
            )
            .unwrap(),
        );
        let rendered = render(&diagnostic, &plain(100));
        assert!(
            rendered.contains("warning[complexity/redundant-let]"),
            "{rendered}"
        );
        assert!(rendered.find("main.lisp").unwrap() < rendered.find("library.lisp").unwrap());
        for expected in [
            "declared here",
            "no binding refers",
            "both forms",
            "while loading: cached.fasl",
            "machine applicable",
            "(let*",
            "(let ",
            "(defun g",
        ] {
            assert!(
                rendered.contains(expected),
                "missing {expected:?}:\n{rendered}"
            );
        }
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn unicode_crlf_tabs_overlaps_and_empty_eof_spans_render_at_narrow_width() {
        let text = "\t(雪 😀e\u{301})\r\n(second line)\r\n";
        let source = SourceDocument::file("unicode.lisp".into(), Some(text.into()));
        let mut diagnostic = Diagnostic::new(Severity::Error, "Unicode input");
        diagnostic
            .add_label(
                &source,
                2.."\t(雪 😀".len(),
                LabelRole::Primary,
                "wide and combining characters",
            )
            .unwrap();
        diagnostic
            .add_label(
                &source,
                2..text.len(),
                LabelRole::Secondary,
                "multiline related span",
            )
            .unwrap();
        diagnostic
            .add_label(
                &source,
                text.len()..text.len(),
                LabelRole::Secondary,
                "at end of input",
            )
            .unwrap();
        for width in [1, 20, 40, 100] {
            let rendered = render(&diagnostic, &plain(width));
            assert!(rendered.contains("Unicode input"), "{rendered}");
            assert!(rendered.contains("unicode.lisp"), "{rendered}");
            assert!(!rendered.contains('\r'));
        }
    }

    #[test]
    fn untrusted_control_characters_do_not_escape_the_renderer() {
        let source = SourceDocument::file(
            "bad\u{1b}[2J.lisp".into(),
            Some("(bad \u{1b}[31m)\n".into()),
        );
        let mut diagnostic = Diagnostic::new(Severity::Error, "bad\u{1b}[2J\u{9b}2J message");
        diagnostic
            .add_label(&source, 1..4, LabelRole::Primary, "label\u{7}")
            .unwrap();
        diagnostic.notes.push(Note {
            kind: NoteKind::Note,
            message: "note\u{1b}[2J".into(),
        });
        diagnostic.suggestions.push(
            Suggestion::new(
                "replace",
                Applicability::MaybeIncorrect,
                vec![SourceEdit::new(source, 1..4, "new\u{1b}[2J").unwrap()],
            )
            .unwrap(),
        );
        let rendered = render(&diagnostic, &plain(80));
        assert!(
            !rendered
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "{rendered:?}"
        );
    }

    #[test]
    fn unavailable_sources_do_not_fabricate_snippets_and_short_uses_same_engine() {
        let source = SourceDocument::file("removed.lisp".into(), None);
        let mut diagnostic = Diagnostic::new(Severity::Error, "source unavailable");
        diagnostic
            .add_label(
                &source,
                100..110,
                LabelRole::Primary,
                "original source was not retained",
            )
            .unwrap();
        let rendered = render(&diagnostic, &plain(80));
        assert!(rendered.contains("removed.lisp"));
        assert!(rendered.contains("source text is unavailable"));
        assert!(!rendered.contains("1 |"));
        let mut options = plain(80);
        options.short = true;
        assert!(render(&diagnostic, &options).contains("error: source unavailable"));
    }

    #[test]
    fn explicit_color_overrides_auto_environment_and_stream_detection() {
        for terminal in [false, true] {
            for no_color in [false, true] {
                for dumb in [false, true] {
                    assert!(color_enabled(ColorMode::Always, terminal, no_color, dumb));
                    assert!(!color_enabled(ColorMode::Never, terminal, no_color, dumb));
                    assert_eq!(
                        color_enabled(ColorMode::Auto, terminal, no_color, dumb),
                        terminal && !no_color && !dumb
                    );
                }
            }
        }
    }

    #[test]
    fn paths_are_lexically_relative_and_safe_for_direct_status_messages() {
        let path = std::env::current_dir()
            .unwrap()
            .join("never-created\u{1b}[2J\u{9b}\n.lisp");
        let label = display_path(&path);
        assert!(label.starts_with("never-created"), "{label:?}");
        assert!(!label.chars().any(char::is_control), "{label:?}");
        assert_eq!(display_path(&std::env::current_dir().unwrap()), ".");
        assert_eq!(
            single_line("line1\r\nline2\t\x07"),
            "line1\\r\\nline2\\t\\u{7}"
        );
    }

    #[test]
    fn bare_cr_line_endings_keep_the_original_source_coordinates() {
        let text = "(first)\r(雪)\r\n(final)";
        let source = SourceDocument::file("classic.lisp".into(), Some(text.into()));
        let mut report = Diagnostic::new(Severity::Error, "unknown symbol");
        let start = text.find('雪').unwrap();
        report
            .add_label(
                &source,
                start..start + '雪'.len_utf8(),
                LabelRole::Primary,
                "here",
            )
            .unwrap();
        let rendered = render(&report, &plain(80));
        assert!(rendered.contains("classic.lisp:2:2"), "{rendered}");
        assert!(rendered.contains("(雪)"), "{rendered}");
    }

    #[test]
    fn color_probe_uses_the_same_tree_without_displaying_help_or_version() {
        use clap::CommandFactory;
        #[derive(clap::Parser)]
        #[command(version = "1", subcommand_required = true)]
        struct Arguments {
            #[arg(long, global = true, value_enum, default_value = "auto")]
            color: ColorMode,
            #[command(subcommand)]
            command: Commands,
        }
        #[derive(clap::Subcommand)]
        enum Commands {
            Run {
                #[arg(long, allow_hyphen_values = true)]
                name: Option<String>,
            },
        }
        for arguments in [
            vec!["test", "run", "--help", "--color=always"],
            vec!["test", "--version", "--color=always"],
            vec!["test", "run", "--name", "--color=never", "--color=always"],
            vec!["test", "run", "--color=always", "--bogus"],
        ] {
            let arguments = arguments
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>();
            assert_eq!(
                command_color(Arguments::command(), &arguments, ColorMode::Never),
                ColorMode::Always,
                "{arguments:?}"
            );
        }
    }
}
