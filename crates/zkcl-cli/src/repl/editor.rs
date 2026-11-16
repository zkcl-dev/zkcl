use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nu_ansi_term::{Color, Style};
use reedline::{
    AutoPairAction, AutoPairContext, AutoPairs, Completer, CompletionResult, DefaultHinter,
    DescriptionMode, EditCommand, EditMode, Emacs, EventStatus, FileBackedHistory, Highlighter,
    IdeMenu, InputMode, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode,
    PromptHelixMode, PromptHistorySearch, PromptHistorySearchStatus, PromptViMode, Reedline,
    ReedlineEvent, ReedlineMenu, ReedlineRawEvent, Span, StatusLine, StyledText, Suggestion,
    ValidationResult, Validator, Vi, default_emacs_keybindings, default_vi_insert_keybindings,
    default_vi_normal_keybindings, default_vi_visual_keybindings,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use zkcl_diagnostics::{SourceDocument, SourcePositionEncoding};
use zkcl_syntax::ast::{AstForm, Token, matching_parens_at};
use zkcl_syntax::syntax::{
    SourceReadtable, SyntaxDiagnostic, SyntaxKind, SyntaxNode, TextRange, TextSize,
};
use zkcl_syntax::{
    CallFeedback, CompletionKind, ExpressionFeedback, Prefix, SemanticToken, SemanticTokenKind,
    SignatureHelp, SourceSnapshot, SourceWorkspace, SymbolName,
};
use zkcl_terminal::single_line;

use super::commands::{
    COMMANDS, CommandArgKind, ParsedCommand, ReplSettings, command_path_prefix, parse_command,
    path_word_end,
};
use super::snapshot::{RuntimeSnapshot, lisp_string_literal};
use super::theme::{ReplTheme, format_duration};

const MAX_SESSION_DOCUMENTS: usize = 256;
const HISTORY_CAPACITY: usize = 2000;
const INPUT_DOCUMENT: &str = "input";
const STATUS_INDENT: &str = "  ";
const STATUS_GAP: &str = "   ";
const EMPTY_INPUT_HINT: &str =
    "Tab complete   Ctrl+J newline   Ctrl+R history   Ctrl+G editor   :help";

pub(super) struct ReplSource {
    pub workspace: SourceWorkspace,
    loaded_documents: HashSet<String>,
    submission_documents: VecDeque<String>,
    next_submission_id: usize,
    active_package: Option<String>,
    readtable: SourceReadtable,
    custom_readtable: bool,
    standard_reader_bindings: bool,
    cached_input_tokens: Option<Arc<[SemanticToken]>>,
    cached_input_feedback: Option<Arc<ExpressionFeedback>>,
    cached_call_feedback: Option<(usize, Option<Arc<CallFeedback>>)>,
    runtime_snapshot: Arc<RuntimeSnapshot>,
    runtime_revision: Option<zkcl::NamespaceRevision>,
    runtime_standard_symbols: zkcl::StandardSourceSymbols,
    lisp_pathname_directory: Option<PathBuf>,
    known_systems: BTreeSet<String>,
    pub settings: ReplSettings,
    pub theme: ReplTheme,
    pub jit_active: bool,
    pub last_duration: Option<Duration>,
    last_buffer: String,
    last_cursor: usize,
    show_empty_hint: bool,
}

impl ReplSource {
    fn new(active_package: Option<String>, color: bool) -> Self {
        Self::with_workspace(SourceWorkspace::new(), active_package, color)
    }

    fn with_workspace(
        mut workspace: SourceWorkspace,
        active_package: Option<String>,
        color: bool,
    ) -> Self {
        workspace.set_document(INPUT_DOCUMENT, "");
        workspace.set_initial_package(INPUT_DOCUMENT, active_package.clone());
        Self {
            workspace,
            loaded_documents: HashSet::new(),
            submission_documents: VecDeque::new(),
            next_submission_id: 0,
            active_package,
            readtable: SourceReadtable::Standard,
            custom_readtable: false,
            standard_reader_bindings: true,
            cached_input_tokens: None,
            cached_input_feedback: None,
            cached_call_feedback: None,
            runtime_snapshot: Arc::default(),
            runtime_revision: None,
            runtime_standard_symbols: zkcl::StandardSourceSymbols::default(),
            lisp_pathname_directory: None,
            known_systems: BTreeSet::new(),
            settings: ReplSettings::default(),
            theme: ReplTheme::new(color),
            jit_active: false,
            last_duration: None,
            last_buffer: String::new(),
            last_cursor: 0,
            show_empty_hint: true,
        }
    }

    fn new_session(&self, workspace: SourceWorkspace, package: Option<String>) -> Self {
        Self {
            settings: self.settings,
            theme: self.theme,
            jit_active: self.jit_active,
            ..Self::with_workspace(workspace, package, self.theme.color)
        }
    }

    fn sync_input(&mut self, input: &str) -> Arc<SourceSnapshot> {
        let snapshot = self.workspace.snapshot(INPUT_DOCUMENT).expect("REPL input");
        if snapshot.text() == input {
            return snapshot;
        }
        self.workspace.set_document(INPUT_DOCUMENT, input);
        self.invalidate_input_analysis();
        self.workspace.snapshot(INPUT_DOCUMENT).expect("REPL input")
    }

    fn invalidate_input_analysis(&mut self) {
        self.cached_input_tokens = None;
        self.cached_input_feedback = None;
        self.cached_call_feedback = None;
    }

    fn input_tokens(&mut self) -> Arc<[SemanticToken]> {
        Arc::clone(
            self.cached_input_tokens
                .get_or_insert_with(|| self.workspace.semantic_tokens(INPUT_DOCUMENT).into()),
        )
    }

    fn input_feedback(&mut self) -> Arc<ExpressionFeedback> {
        if let Some(feedback) = &self.cached_input_feedback {
            return Arc::clone(feedback);
        }
        let feedback = Arc::new(self.expression_feedback(self.settings.preview));
        self.cached_input_feedback = Some(Arc::clone(&feedback));
        feedback
    }

    fn call_feedback(&mut self, cursor: usize) -> Option<Arc<CallFeedback>> {
        if let Some((position, feedback)) = &self.cached_call_feedback
            && *position == cursor
        {
            return feedback.clone();
        }
        let feedback = self
            .workspace
            .call_feedback_with(INPUT_DOCUMENT, cursor, |symbol| {
                self.live_standard_symbol(symbol)
            })
            .map(Arc::new);
        self.cached_call_feedback = Some((cursor, feedback.clone()));
        feedback
    }

    fn expression_feedback(&self, preview: bool) -> ExpressionFeedback {
        if !self.has_static_reader() {
            return ExpressionFeedback::default();
        }
        self.workspace
            .expression_feedback_with(INPUT_DOCUMENT, preview, |symbol| {
                self.live_standard_symbol(symbol)
            })
    }

    fn has_static_reader(&self) -> bool {
        !self.custom_readtable && self.standard_reader_bindings
    }

    fn live_standard_symbol(&self, symbol: &SymbolName) -> Option<bool> {
        self.runtime_revision
            .map(|_| self.runtime_standard_symbols.resolves(symbol))
    }

    fn set_active_package(&mut self, package: Option<String>) {
        if self.active_package != package {
            self.workspace
                .set_initial_package(INPUT_DOCUMENT, package.clone());
            self.active_package = package;
            self.invalidate_input_analysis();
        }
    }

    fn set_input_readtable(&mut self, readtable: SourceReadtable) {
        if self
            .workspace
            .set_initial_readtable(INPUT_DOCUMENT, readtable)
        {
            self.readtable = readtable;
            self.invalidate_input_analysis();
        }
    }

    fn record_submission(&mut self, input: &str) {
        self.show_empty_hint = false;
        if !self.has_static_reader() || input.trim().is_empty() {
            return;
        }
        let snapshot = self.sync_input(input);
        if snapshot.outline().is_empty() {
            return;
        }
        if self.submission_documents.len() == MAX_SESSION_DOCUMENTS {
            let oldest = self
                .submission_documents
                .pop_front()
                .expect("session documents");
            self.workspace.remove_document(&oldest);
        }
        let document = format!("session-{}", self.next_submission_id);
        self.next_submission_id += 1;
        self.workspace.set_document(&document, input);
        self.workspace
            .set_initial_package(&document, self.active_package.clone());
        self.workspace
            .set_initial_readtable(&document, self.readtable);
        self.submission_documents.push_back(document);
        self.invalidate_input_analysis();
    }

    fn index_file(&mut self, path: &Path) {
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        // One document per path: loading the same file again replaces its
        // analysis instead of keeping another parsed copy in the workspace.
        let document = format!("loaded-{}", path.display());
        self.workspace.set_document(&document, text);
        self.workspace
            .set_initial_package(&document, self.active_package.clone());
        self.workspace
            .set_initial_readtable(&document, self.readtable);
        self.loaded_documents.insert(document);
        self.invalidate_input_analysis();
    }
}

#[derive(Clone)]
pub(super) struct EditorAnalysis(Arc<Mutex<ReplSource>>);

impl EditorAnalysis {
    #[cfg(test)]
    pub(super) fn new(package: Option<String>) -> Self {
        Self::with_color(package, false)
    }

    pub(super) fn with_color(package: Option<String>, color: bool) -> Self {
        Self(Arc::new(Mutex::new(ReplSource::new(package, color))))
    }

    pub(super) fn editor(&self, use_ansi_coloring: bool) -> Reedline {
        let (theme, settings) = {
            let mut source = self.lock_source();
            source.theme = ReplTheme::new(use_ansi_coloring);
            (source.theme, source.settings)
        };

        let completion_menu = Box::new(
            IdeMenu::default()
                .with_name("completion_menu")
                .with_input_mode(InputMode::FullBuffer)
                .with_default_border()
                .with_correct_cursor_pos(true)
                .with_min_completion_width(16)
                .with_max_completion_width(44)
                .with_max_completion_height(12)
                .with_padding(1)
                .with_description_mode(DescriptionMode::PreferRight)
                .with_min_description_width(12)
                .with_max_description_width(52)
                .with_description_offset(1)
                .with_text_style(theme.menu_text())
                .with_selected_text_style(theme.menu_selected_text())
                .with_description_text_style(theme.menu_description())
                .with_match_text_style(theme.menu_match())
                .with_selected_match_text_style(theme.menu_selected_match()),
        );

        let edit_mode: Box<dyn EditMode> = if settings.vi_mode {
            let mut insert_kb = default_vi_insert_keybindings();
            add_shared_repl_bindings(&mut insert_kb);
            let mut normal_kb = default_vi_normal_keybindings();
            add_shared_repl_bindings(&mut normal_kb);
            Box::new(LispEditMode {
                inner: Box::new(Vi::new(
                    insert_kb,
                    normal_kb,
                    default_vi_visual_keybindings(),
                )),
                analysis: self.clone(),
            })
        } else {
            let mut keybindings = default_emacs_keybindings();
            add_shared_repl_bindings(&mut keybindings);
            Box::new(LispEditMode {
                inner: Box::new(Emacs::new(keybindings)),
                analysis: self.clone(),
            })
        };

        let auto_pairs = AutoPairs::new([('(', ')'), ('"', '"')]);
        let mut editor = Reedline::create()
            .with_auto_pairs(auto_pairs)
            .with_highlighter(Box::new(self.clone()))
            .with_completer(Box::new(self.clone()))
            .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
            .with_edit_mode(edit_mode)
            .with_status_line(Box::new(self.clone()))
            .with_validator(Box::new(self.clone()))
            .use_bracketed_paste(cfg!(not(target_os = "windows")))
            .with_ansi_colors(use_ansi_coloring);

        if settings.hints {
            let hinter = Box::new(DefaultHinter::default().with_style(theme.comment()));
            editor = editor.with_hinter(hinter);
        }

        if let Some(path) = history_file_path()
            && let Ok(history) = FileBackedHistory::with_file(HISTORY_CAPACITY, path)
        {
            editor = editor.with_history(Box::new(history));
        }

        if let Some(command) = external_editor_command() {
            let path = env::temp_dir().join(format!("zkcl-repl-{}.lisp", std::process::id()));
            editor = editor.with_buffer_editor(command, path);
        }
        editor
    }

    pub(super) fn prompt(&self) -> ReplPrompt {
        ReplPrompt {
            analysis: self.clone(),
        }
    }

    pub(super) fn record_submission(&self, input: &str, package: Option<String>) {
        let mut source = self.lock_source();
        source.record_submission(input);
        source.set_active_package(package);
    }

    pub(super) fn index_file(&self, path: &Path) {
        self.lock_source().index_file(path);
    }

    pub(super) fn remember_systems(&self, names: impl IntoIterator<Item = String>) {
        self.lock_source().known_systems.extend(names);
    }

    pub(super) fn refresh_runtime_snapshot(&self, runtime: &zkcl::Runtime) {
        let readtable = if runtime.raw_strings_enabled() {
            SourceReadtable::RawStrings
        } else {
            SourceReadtable::Standard
        };
        let revision = runtime.namespace_revision();
        let custom_readtable = runtime.custom_readtable();
        let standard_reader_bindings = runtime.standard_source_reader_bindings();
        let mut source = self.lock_source();
        source.set_input_readtable(readtable);
        source.lisp_pathname_directory = runtime.local_pathname_defaults();
        if source.custom_readtable != custom_readtable
            || source.standard_reader_bindings != standard_reader_bindings
        {
            source.custom_readtable = custom_readtable;
            source.standard_reader_bindings = standard_reader_bindings;
            source.invalidate_input_analysis();
        }
        if source.runtime_revision == Some(revision) {
            return;
        }
        let snapshot = runtime.namespace_snapshot();
        source.set_active_package(snapshot.current_package.clone());
        source.runtime_snapshot = Arc::new(snapshot);
        source.runtime_standard_symbols = runtime.standard_source_symbols();
        source.runtime_revision = Some(revision);
        source.invalidate_input_analysis();
    }

    pub(super) fn set_jit_active(&self, active: bool) {
        self.lock_source().jit_active = active;
    }

    pub(super) fn set_last_duration(&self, duration: Option<Duration>) {
        self.lock_source().last_duration = duration;
    }

    pub(super) fn settings(&self) -> ReplSettings {
        self.lock_source().settings
    }

    pub(super) fn set_settings(&self, settings: ReplSettings) {
        let mut source = self.lock_source();
        source.settings = settings;
        source.cached_input_feedback = None;
    }

    pub(super) fn static_expression_type(&self, form: &str) -> Option<&'static str> {
        let mut source = self.lock_source();
        source.sync_input(form);
        source.expression_feedback(false).output_type
    }

    fn snapshot(&self, input: &str) -> Arc<SourceSnapshot> {
        self.lock_source().sync_input(input)
    }

    pub(super) fn reset(&self, package: Option<String>) {
        let mut source = self.lock_source();
        *source = source.new_session(SourceWorkspace::new(), package);
    }

    pub(super) fn reload_files<'a>(
        &self,
        package: Option<String>,
        paths: impl IntoIterator<Item = &'a Path>,
    ) {
        let mut source = self.lock_source();
        let mut workspace = std::mem::take(&mut source.workspace);
        workspace.remove_document(INPUT_DOCUMENT);
        for document in &source.submission_documents {
            workspace.remove_document(document);
        }
        let mut fresh = source.new_session(workspace, package);
        // Reread each input. Identical text keeps its syntax, and edits can reparse incrementally.
        for path in paths {
            fresh.index_file(path);
        }
        for document in source.loaded_documents.difference(&fresh.loaded_documents) {
            fresh.workspace.remove_document(document);
        }
        *source = fresh;
    }

    pub(super) fn lock_source(&self) -> MutexGuard<'_, ReplSource> {
        self.0.lock().expect("REPL source workspace")
    }
}

fn add_shared_repl_bindings(keybindings: &mut reedline::Keybindings) {
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char(' '),
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('j'),
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    keybindings.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('g'),
        ReedlineEvent::OpenEditor,
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('o'),
        ReedlineEvent::OpenEditor,
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::F(1),
        ReedlineEvent::ExecuteHostCommand("help".into()),
    );
}

struct LispEditMode {
    inner: Box<dyn EditMode>,
    analysis: EditorAnalysis,
}

impl EditMode for LispEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let parsed = self.inner.parse_event(event);
        if matches!(
            &parsed,
            ReedlineEvent::Edit(cmds) if cmds.as_slice() == [EditCommand::InsertNewline]
        ) {
            let indent = {
                let source = self.analysis.lock_source();
                let end = source.last_cursor.min(source.last_buffer.len());
                compute_lisp_indent(&source.last_buffer[..end])
            };
            if indent > 0 {
                return ReedlineEvent::Edit(vec![
                    EditCommand::InsertNewline,
                    EditCommand::InsertString(" ".repeat(indent)),
                ]);
            }
        }
        parsed
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }

    fn handle_mode_specific_event(&mut self, event: ReedlineEvent) -> EventStatus {
        self.inner.handle_mode_specific_event(event)
    }
}

#[derive(Clone)]
pub(super) struct ReplPrompt {
    analysis: EditorAnalysis,
}

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        let source = self.analysis.lock_source();
        let theme = &source.theme;
        let pkg = super::prompt_package(source.active_package.clone());
        let mut out = String::new();
        out.push_str(&theme.paint("zkcl:", theme.brand()));
        out.push_str(&theme.paint(&single_line(&pkg), theme.package()));
        if source.readtable == SourceReadtable::RawStrings {
            out.push_str(&theme.paint(":raw", theme.badge()));
        }
        Cow::Owned(out)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        let source = self.analysis.lock_source();
        let theme = &source.theme;
        let mut badges = Vec::new();
        if let Some(elapsed) = source.last_duration
            && source.settings.timing.should_show(elapsed)
        {
            badges.push(theme.paint(&format_duration(elapsed), theme.muted()));
        }
        if source.jit_active {
            badges.push(theme.paint("jit", theme.subtle()));
        }
        if badges.is_empty() {
            Cow::Borrowed("")
        } else {
            Cow::Owned(badges.join(&theme.paint("   ", theme.subtle())))
        }
    }

    fn render_prompt_indicator(&self, edit_mode: PromptEditMode) -> Cow<'_, str> {
        let source = self.analysis.lock_source();
        let theme = &source.theme;
        let glyph = match edit_mode {
            PromptEditMode::Vi(PromptViMode::Normal) => ": ",
            PromptEditMode::Vi(PromptViMode::Visual)
            | PromptEditMode::Helix(PromptHelixMode::Select) => "+ ",
            PromptEditMode::Helix(PromptHelixMode::Normal) => ": ",
            _ => "> ",
        };
        Cow::Owned(theme.paint(glyph, theme.prompt_arrow()))
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        let source = self.analysis.lock_source();
        let theme = &source.theme;
        let pkg = super::prompt_package(source.active_package.clone());
        let raw_extra = if source.readtable == SourceReadtable::RawStrings {
            ":raw".len()
        } else {
            0
        };
        // Pad to align `│ ` directly under `> ` in `zkcl:<pkg>> `
        let pad_len = "zkcl:".len() + single_line(&pkg).width() + raw_extra;
        let pad = " ".repeat(pad_len.min(20));
        Cow::Owned(format!("{pad}{}", theme.paint("│ ", theme.multiline_bar())))
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let source = self.analysis.lock_source();
        let theme = &source.theme;
        let (label, style) = match history_search.status {
            PromptHistorySearchStatus::Passing => ("search", theme.brand()),
            PromptHistorySearchStatus::Failing => ("failing search", theme.error()),
        };
        Cow::Owned(format!(
            "({}: {}) ",
            theme.paint(label, style),
            single_line(&history_search.term)
        ))
    }

    fn get_prompt_color(&self) -> Color {
        Color::Default
    }

    fn get_indicator_color(&self) -> Color {
        Color::Default
    }

    fn get_prompt_multiline_color(&self) -> Color {
        Color::Default
    }

    fn get_prompt_right_color(&self) -> Color {
        Color::Default
    }
}

pub(super) fn history_file_path() -> Option<PathBuf> {
    if let Some(explicit) = env::var_os("ZKCL_HISTORY") {
        let path = PathBuf::from(explicit);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    if let Some(xdg) = env::var_os("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("zkcl").join("history"));
    }
    env::var_os("HOME").filter(|h| !h.is_empty()).map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("zkcl")
            .join("history")
    })
}

fn command_exists_in_path(name: &str) -> bool {
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
    })
}

fn external_editor_name() -> Option<String> {
    env::var("VISUAL")
        .ok()
        .filter(|editor| !editor.trim().is_empty())
        .or_else(|| {
            env::var("EDITOR")
                .ok()
                .filter(|editor| !editor.trim().is_empty())
        })
        .or_else(|| {
            #[cfg(not(target_os = "windows"))]
            let fallbacks = ["nvim", "vim", "nano", "vi"];
            #[cfg(target_os = "windows")]
            let fallbacks = ["notepad"];
            fallbacks
                .into_iter()
                .find(|name| command_exists_in_path(name))
                .map(str::to_owned)
        })
}

pub(super) fn launch_external_editor(path: &Path) -> std::io::Result<std::process::ExitStatus> {
    let Some(editor) = external_editor_name() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no editor found ($VISUAL, $EDITOR, nvim, vim, nano, vi)",
        ));
    };
    #[cfg(not(target_os = "windows"))]
    {
        Command::new("sh")
            .arg("-c")
            .arg(format!("exec {editor} \"$1\""))
            .arg("zkcl-editor")
            .arg(path)
            .status()
    }
    #[cfg(target_os = "windows")]
    {
        let mut parts = editor.split_whitespace();
        let executable = parts.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty $EDITOR")
        })?;
        Command::new(executable).args(parts).arg(path).status()
    }
}

fn external_editor_command() -> Option<Command> {
    let editor = external_editor_name()?;
    #[cfg(not(target_os = "windows"))]
    {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("exec {editor} \"$1\""))
            .arg("zkcl-editor")
            .arg("{file}");
        Some(command)
    }
    #[cfg(target_os = "windows")]
    {
        let mut parts = editor.split_whitespace();
        let executable = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Command::new(executable))
    }
}

pub(super) fn compute_lisp_indent(prefix: &str) -> usize {
    let parsed = zkcl_syntax::syntax::parse_source(prefix);
    let root = parsed.root();
    let offset = TextSize::try_from(prefix.len()).unwrap_or_default();

    // Do not auto-indent inside unclosed strings or block comments.
    if let Some(tok) = zkcl_syntax::ast::token_at_offset(&root, offset)
        && matches!(
            tok.kind(),
            SyntaxKind::String | SyntaxKind::BlockComment | SyntaxKind::RawString
        )
    {
        return 0;
    }

    // Walk deepest unclosed list/vector enclosing the end of prefix.
    let mut best_open: Option<SyntaxNode> = None;
    for node in root.descendants() {
        if !matches!(node.kind(), SyntaxKind::List | SyntaxKind::Vector) {
            continue;
        }
        let closed = node
            .last_child_or_token()
            .is_some_and(|last| last.kind() == SyntaxKind::CloseParen);
        if closed {
            continue;
        }
        if best_open
            .as_ref()
            .is_none_or(|b| node.text_range().start() >= b.text_range().start())
        {
            best_open = Some(node);
        }
    }

    let Some(open_node) = best_open else {
        return 0;
    };

    let open_start = usize::from(open_node.text_range().start());
    let line_start = prefix[..open_start].rfind('\n').map_or(0, |i| i + 1);
    let open_col = displayed_width(&prefix[line_start..], open_start - line_start);

    if open_node.kind() == SyntaxKind::Vector {
        return open_col + 2;
    }

    if let Some(list) = AstForm::cast(open_node.clone().into()).and_then(|f| match f {
        AstForm::List(l) => Some(l),
        _ => None,
    }) {
        let mut forms = list.forms();
        if let Some(head) = forms.next() {
            let op_upper = head
                .as_atom()
                .map(|a| a.text().to_ascii_uppercase())
                .unwrap_or_default();
            let op_name = op_upper
                .rsplit_once(':')
                .map_or(op_upper.as_str(), |(_, name)| name);
            if is_body_form(op_name) {
                return open_col + 2;
            }
            if let Some(first_arg) = forms.next() {
                let arg_start = usize::from(first_arg.text_range().start());
                let arg_line_start = prefix[..arg_start].rfind('\n').map_or(0, |i| i + 1);
                if arg_line_start == line_start {
                    return displayed_width(&prefix[arg_line_start..], arg_start - arg_line_start);
                }
            }
            return open_col + 2;
        }
    }

    open_col + 1
}

fn is_body_form(op: &str) -> bool {
    matches!(
        op,
        "DEFUN"
            | "DEFMACRO"
            | "DEFGENERIC"
            | "DEFMETHOD"
            | "DEFSTRUCT"
            | "DEFCLASS"
            | "DEFPACKAGE"
            | "DEFVAR"
            | "DEFPARAMETER"
            | "DEFCONSTANT"
            | "DEFINE-CONDITION"
            | "LAMBDA"
            | "LET"
            | "LET*"
            | "FLET"
            | "LABELS"
            | "MACROLET"
            | "SYMBOL-MACROLET"
            | "PROGN"
            | "PROG1"
            | "PROG2"
            | "BLOCK"
            | "TAGBODY"
            | "WHEN"
            | "UNLESS"
            | "COND"
            | "CASE"
            | "ECASE"
            | "CCASE"
            | "TYPECASE"
            | "ETYPECASE"
            | "CTYPECASE"
            | "IF"
            | "DO"
            | "DO*"
            | "DOTIMES"
            | "DOLIST"
            | "LOOP"
            | "WITH-OPEN-FILE"
            | "WITH-OUTPUT-TO-STRING"
            | "WITH-INPUT-FROM-STRING"
            | "HANDLER-CASE"
            | "HANDLER-BIND"
            | "RESTART-CASE"
            | "RESTART-BIND"
            | "UNWIND-PROTECT"
            | "MULTIPLE-VALUE-BIND"
            | "DESTRUCTURING-BIND"
            | "EVAL-WHEN"
            | "LOCALLY"
    )
}

fn signature_segments(sig: &SignatureHelp, theme: &ReplTheme, color: bool) -> Vec<(Style, String)> {
    let base = theme.muted();
    let active_style = theme.active_param();
    let Some(head_end) = sig.label.find(' ') else {
        return vec![(base, sig.label.clone())];
    };
    let head = &sig.label[..head_end];
    let mut segments = vec![(base, head.to_owned())];
    for (idx, param) in sig.parameters.iter().enumerate() {
        segments.push((base, " ".into()));
        if sig.active_parameter == Some(idx) {
            if color {
                segments.push((active_style, param.clone()));
            } else {
                segments.push((base, format!("[{param}]")));
            }
        } else {
            segments.push((base, param.clone()));
        }
    }
    segments.push((base, ")".into()));
    segments
}

fn render_segments(segments: &[(Style, String)], columns: u16, use_ansi_coloring: bool) -> String {
    // Leave the last column free so a status line cannot wrap at the margin.
    let limit = usize::from(columns).saturating_sub(STATUS_INDENT.len() + 1);
    if segments.is_empty() || limit == 0 {
        return String::new();
    }
    let segments: Vec<_> = segments
        .iter()
        .map(|(style, text)| (*style, single_line(text)))
        .collect();
    let total: usize = segments.iter().map(|(_, text)| text.width()).sum();
    let budget = if total > limit {
        limit.saturating_sub(1)
    } else {
        limit
    };
    let mut out = String::from(STATUS_INDENT);
    let mut remaining = budget;
    for (style, mut text) in segments {
        let width = text.width();
        let cut = width > remaining;
        if cut {
            let mut used = 0;
            let mut end = 0;
            for grapheme in text.graphemes(true) {
                used += grapheme.width();
                if used > remaining {
                    break;
                }
                end += grapheme.len();
            }
            text.truncate(end);
            text.push('…');
        } else {
            remaining -= width;
        }
        if use_ansi_coloring && style != Style::new() {
            out.push_str(&style.paint(text).to_string());
        } else {
            out.push_str(&text);
        }
        if cut {
            break;
        }
    }
    out
}

impl Validator for EditorAnalysis {
    fn validate(&self, input: &str) -> ValidationResult {
        if !self.lock_source().has_static_reader() {
            return ValidationResult::Complete;
        }
        let syntax = parse_command(input).map_or(Some(input), |parsed| parsed.lisp_args());
        if self.needs_more_input(syntax) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

impl Highlighter for EditorAnalysis {
    fn highlight(&self, line: &str, cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        styled.push((Style::new(), line.to_owned()));
        {
            let mut state = self.lock_source();
            state.last_buffer.clear();
            state.last_buffer.push_str(line);
            state.last_cursor = cursor.min(line.len());
        }
        if line.is_empty() {
            return styled;
        }

        if let Some(parsed) = parse_command(line) {
            let (theme, rainbow) = {
                let state = self.lock_source();
                (state.theme, state.settings.rainbow)
            };
            let lead = line.len() - line.trim_start().len();
            styled.style_range(lead, lead + parsed.token.len(), theme.command());
            if let Some(args) = parsed.lisp_args().filter(|args| !args.is_empty()) {
                let sub_cursor = cursor.saturating_sub(parsed.args_start).min(args.len());
                self.highlight_lisp_slice(
                    &mut styled,
                    args,
                    parsed.args_start,
                    sub_cursor,
                    &theme,
                    rainbow,
                    parsed.evaluates_form(),
                );
            } else if !parsed.args.is_empty() {
                styled.style_range(parsed.args_start, line.len(), theme.command_arg());
            }
            return styled;
        }

        let (theme, rainbow) = {
            let state = self.lock_source();
            (state.theme, state.settings.rainbow)
        };
        self.highlight_lisp_slice(&mut styled, line, 0, cursor, &theme, rainbow, true);
        styled
    }

    fn should_auto_pair(&self, context: &AutoPairContext<'_>) -> bool {
        if !self.lock_source().has_static_reader() {
            return false;
        }
        if context.action() != AutoPairAction::Open || context.selection().is_some() {
            return true;
        }
        let input = context.buffer();
        let cursor = context.insertion_point();
        if let Some(command) = parse_command(input) {
            return command.lisp_args().is_some_and(|args| {
                cursor >= command.args_start
                    && self
                        .snapshot(args)
                        .should_auto_pair_at(cursor - command.args_start)
            });
        }
        self.snapshot(input).should_auto_pair_at(cursor)
    }
}

impl EditorAnalysis {
    fn needs_more_input(&self, syntax: Option<&str>) -> bool {
        syntax.is_some_and(|input| {
            self.snapshot(input)
                .parsed()
                .primary_diagnostic()
                .is_some_and(SyntaxDiagnostic::needs_more_input)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn highlight_lisp_slice(
        &self,
        styled: &mut StyledText,
        slice: &str,
        base_offset: usize,
        cursor: usize,
        theme: &ReplTheme,
        rainbow: bool,
        evaluate: bool,
    ) {
        let (tokens, diagnostics, error_range, matched_parens, root) = {
            let mut state = self.lock_source();
            if state.custom_readtable {
                return;
            }
            let snapshot = state.sync_input(slice);
            let tokens = state.input_tokens();
            let diags: Vec<_> = snapshot
                .diagnostics()
                .iter()
                .filter(|d| state.has_static_reader() && !d.needs_more_input())
                .map(|d| d.range())
                .collect();
            let root = snapshot.parsed().root();
            let clamped = cursor.min(slice.len());
            let error_range = if evaluate && state.has_static_reader() && diags.is_empty() {
                match state.call_feedback(clamped).as_deref() {
                    Some(CallFeedback::ArityError { range, .. }) => Some(*range),
                    _ => state
                        .input_feedback()
                        .type_error
                        .as_ref()
                        .map(|error| error.range),
                }
            } else {
                None
            };
            let parens = TextSize::try_from(clamped)
                .ok()
                .and_then(|off| matching_parens_at(&root, off));
            (tokens, diags, error_range, parens, root)
        };

        // 1. Rainbow parentheses + reader prefixes (drawn underneath semantic tokens / match highlights).
        let mut paren_stack: Vec<TextRange> = Vec::new();
        for element in root.descendants_with_tokens() {
            if let Some(tok) = element.into_token() {
                let range = tok.text_range();
                let start = base_offset + usize::from(range.start());
                let end = base_offset + usize::from(range.end());
                match tok.kind() {
                    SyntaxKind::OpenParen => {
                        let style = if rainbow {
                            theme.rainbow_paren(paren_stack.len())
                        } else {
                            theme.paren()
                        };
                        styled.style_range(start, end, style);
                        paren_stack.push(range);
                    }
                    SyntaxKind::VectorStart => {
                        styled.style_range(start, start + 1, theme.reader_prefix());
                        let style = if rainbow {
                            theme.rainbow_paren(paren_stack.len())
                        } else {
                            theme.paren()
                        };
                        styled.style_range(start + 1, end, style);
                        paren_stack.push(range);
                    }
                    SyntaxKind::CloseParen => {
                        if let Some(_open) = paren_stack.pop() {
                            let style = if rainbow {
                                theme.rainbow_paren(paren_stack.len())
                            } else {
                                theme.paren()
                            };
                            styled.style_range(start, end, style);
                        } else {
                            styled.style_range(start, end, theme.error_underline());
                        }
                    }
                    SyntaxKind::Quote
                    | SyntaxKind::Backquote
                    | SyntaxKind::Unquote
                    | SyntaxKind::UnquoteSplicing
                    | SyntaxKind::UnquoteDestructive
                    | SyntaxKind::FunctionQuote
                    | SyntaxKind::ReaderConditional
                    | SyntaxKind::DispatchPrefix
                    | SyntaxKind::UnknownDispatch => {
                        styled.style_range(start, end, theme.reader_prefix());
                    }
                    _ => {}
                }
            }
        }

        // 2. Semantic tokens
        for tok in tokens.iter() {
            let mut style = match tok.kind {
                SemanticTokenKind::Keyword => theme.keyword(),
                SemanticTokenKind::Function => theme.function(),
                SemanticTokenKind::Macro => theme.macro_style(),
                SemanticTokenKind::Parameter => theme.parameter(),
                SemanticTokenKind::Variable => theme.variable(),
                SemanticTokenKind::Type => theme.type_style(),
                SemanticTokenKind::String => theme.string(),
                SemanticTokenKind::Number => theme.number(),
                SemanticTokenKind::Comment => theme.comment(),
                SemanticTokenKind::Operator => theme.operator(),
            };
            if tok.is_definition {
                style = style.bold();
            }
            styled.style_range(
                base_offset + usize::from(tok.range.start()),
                base_offset + usize::from(tok.range.end()),
                style,
            );
        }

        // 3. Matching parenthesis highlight at cursor
        if let Some((open_range, close_range)) = matched_parens {
            let match_style = theme.matched_paren();
            styled.style_range(
                base_offset + usize::from(open_range.start()),
                base_offset + usize::from(open_range.end()),
                match_style,
            );
            styled.style_range(
                base_offset + usize::from(close_range.start()),
                base_offset + usize::from(close_range.end()),
                match_style,
            );
        }

        // 4. Syntax & static type errors
        let err_style = theme.error_underline();
        for range in diagnostics {
            styled.style_range(
                base_offset + usize::from(range.start()),
                base_offset + usize::from(range.end()),
                err_style,
            );
        }
        if let Some(range) = error_range {
            styled.style_range(
                base_offset + usize::from(range.start()),
                base_offset + usize::from(range.end()),
                err_style,
            );
        }
    }
}

impl Completer for EditorAnalysis {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let clamped = line.floor_char_boundary(pos.min(line.len()));
        let (theme, runtime_snapshot) = {
            let mut state = self.lock_source();
            state.show_empty_hint = false;
            (state.theme, state.runtime_snapshot.clone())
        };

        // Check REPL :command completions first.
        let lead = line.len() - line.trim_start().len();
        if clamped >= lead && line[lead..].starts_with(':') {
            let prefix_to_cursor = &line[lead..clamped];
            if !prefix_to_cursor.contains(char::is_whitespace) {
                return CompletionResult::fresh(complete_repl_commands(
                    lead, line, clamped, &theme,
                ));
            }
            if let Some(parsed) = parse_command(line) {
                match parsed.spec.arg_kind {
                    CommandArgKind::FilePath => {
                        return CompletionResult::fresh(complete_command_path(
                            line, clamped, &parsed, &theme,
                        ));
                    }
                    CommandArgKind::Package => {
                        return CompletionResult::fresh(complete_packages(
                            line,
                            clamped,
                            &runtime_snapshot,
                            &theme,
                        ));
                    }
                    CommandArgKind::System => {
                        if clamped < parsed.args_start {
                            return CompletionResult::fresh(Vec::new());
                        }
                        let prefix = &line[parsed.args_start..clamped];
                        return full_span_completions(
                            complete_systems(
                                prefix.trim_start_matches('"').trim_end_matches('"'),
                                Span {
                                    start: parsed.args_start,
                                    end: line.trim_end().len().max(clamped),
                                },
                                &self.lock_source().known_systems,
                                |name| name.to_owned(),
                                &theme,
                            ),
                            clamped,
                        );
                    }
                    CommandArgKind::Command => {
                        return CompletionResult::fresh(complete_help_args(line, clamped, &theme));
                    }
                    CommandArgKind::Setting => {
                        return CompletionResult::fresh(complete_settings(line, clamped, &theme));
                    }
                    CommandArgKind::Form | CommandArgKind::Symbol => {
                        if parsed.takes_file_or_form()
                            && parsed.args.starts_with('@')
                            && clamped > parsed.args_start
                        {
                            let start = parsed.args_start + 1;
                            let start = line.len() - line[start..].trim_start().len();
                            return CompletionResult::fresh(complete_command_path_at(
                                line,
                                start,
                                clamped,
                                false,
                                PathPreference::Source,
                                &theme,
                            ));
                        }
                        if let Some(slice) = parsed.lisp_args()
                            && clamped >= parsed.args_start
                        {
                            return self.complete_lisp(
                                slice,
                                clamped - parsed.args_start,
                                parsed.args_start,
                                &runtime_snapshot,
                                &theme,
                            );
                        }
                        let tok_start = token_start_before(line, clamped);
                        let slice = &line[tok_start..clamped];
                        return self.complete_lisp(
                            slice,
                            slice.len(),
                            tok_start,
                            &runtime_snapshot,
                            &theme,
                        );
                    }
                    CommandArgKind::Name | CommandArgKind::None => {
                        return CompletionResult::fresh(Vec::new());
                    }
                }
            }
        }

        self.complete_lisp(line, clamped, 0, &runtime_snapshot, &theme)
    }
}

impl EditorAnalysis {
    fn complete_lisp(
        &self,
        slice: &str,
        pos: usize,
        offset_shift: usize,
        runtime_snapshot: &RuntimeSnapshot,
        theme: &ReplTheme,
    ) -> CompletionResult {
        let mut state = self.lock_source();
        let snapshot = state.sync_input(slice);
        if let Some(token) = string_token_at(&snapshot, pos) {
            return full_span_completions(
                complete_lisp_string(&state, &snapshot, &token, pos, offset_shift, theme),
                pos + offset_shift,
            );
        }
        let Some((range, candidates)) = state.workspace.completions_with_range(INPUT_DOCUMENT, pos)
        else {
            return CompletionResult::fresh(Vec::new());
        };
        let span = Span {
            start: offset_shift + usize::from(range.start()),
            end: offset_shift + usize::from(range.end()),
        };
        let suffix_bytes = span.end - (offset_shift + pos);
        let mut seen_labels: HashSet<String> = HashSet::new();

        let mut suggestions: Vec<Suggestion> = candidates
            .into_iter()
            .filter(|candidate| {
                candidate
                    .label
                    .len()
                    .checked_sub(suffix_bytes)
                    .is_some_and(|cursor| candidate.label.is_char_boundary(cursor))
            })
            .filter(|cand| seen_labels.insert(cand.label.to_ascii_lowercase()))
            .map(|cand| {
                let kind_tag = completion_kind_tag(cand.kind);
                let detail = cand
                    .detail
                    .as_deref()
                    .and_then(|detail| completion_description(&cand.label, detail));
                let doc = if !cand.label.starts_with('&') {
                    cand.documentation
                } else {
                    None
                };
                let description = match (detail, doc) {
                    (Some(d), Some(doc)) => Some(format!("[{kind_tag}] {d} — {doc}")),
                    (Some(d), None) => Some(format!("[{kind_tag}] {d}")),
                    (None, Some(doc)) => Some(format!("[{kind_tag}] {doc}")),
                    (None, None) => None,
                };
                Suggestion {
                    value: cand.label,
                    display_override: None,
                    description,
                    style: Some(completion_style(cand.kind, theme)),
                    extra: None,
                    span,
                    append_whitespace: false,
                    match_indices: None,
                }
            })
            .collect();

        // Merge live runtime symbols (non-CL packages, dynamically loaded/evaluated definitions).
        let typed = &slice[usize::from(range.start())..pos];
        append_runtime_completions(
            typed,
            span,
            suffix_bytes,
            runtime_snapshot,
            &mut seen_labels,
            &mut suggestions,
            theme,
        );

        CompletionResult::fresh(suggestions)
    }
}

fn full_span_completions(mut suggestions: Vec<Suggestion>, cursor: usize) -> CompletionResult {
    suggestions.retain(|item| safe_completion_cursor(item, cursor));
    CompletionResult::fresh(suggestions)
}

fn safe_completion_cursor(item: &Suggestion, cursor: usize) -> bool {
    // Reedline preserves the cursor's byte distance from the end of the replaced span.
    let suffix = item.span.end.saturating_sub(cursor);
    item.value
        .len()
        .checked_sub(suffix)
        .is_some_and(|position| item.value.is_char_boundary(position))
}

fn string_token_at(snapshot: &SourceSnapshot, pos: usize) -> Option<Token> {
    let at = TextSize::try_from(pos).ok()?;
    zkcl_syntax::tokens_at_offset(&snapshot.parsed().root(), at).find(|token| {
        matches!(token.kind(), SyntaxKind::String | SyntaxKind::RawString)
            && token.text_range().start() < at
    })
}

enum StringArgument {
    Path(PathPreference),
    AsdfSystem,
}

fn complete_lisp_string(
    state: &ReplSource,
    snapshot: &SourceSnapshot,
    token: &Token,
    pos: usize,
    offset_shift: usize,
    theme: &ReplTheme,
) -> Vec<Suggestion> {
    let range = token.text_range();
    let start = usize::from(range.start());
    let span = Span {
        start: offset_shift + start,
        end: offset_shift + usize::from(range.end()),
    };
    if !state.has_static_reader()
        || !snapshot.has_known_reader_at(start)
        || token.kind() != SyntaxKind::String
    {
        return Vec::new();
    }
    let Some(prefix) = lisp_string_prefix(&token.text()[..pos - start]) else {
        return Vec::new();
    };
    let pathname_literal = token.parent().is_some_and(|parent| {
        parent.kind() == SyntaxKind::ReaderForm
            && parent.first_token().is_some_and(|dispatch| {
                dispatch.kind() == SyntaxKind::DispatchPrefix
                    && dispatch.text().eq_ignore_ascii_case("#p")
            })
    });
    let argument = if pathname_literal {
        Some(StringArgument::Path(PathPreference::Any))
    } else {
        state
            .workspace
            .classify_first_call_argument(INPUT_DOCUMENT, token, |operator, package| {
                if package != state.active_package.as_deref() {
                    return None;
                }
                match operator.name.as_str() {
                    "LOAD" if state.live_standard_symbol(operator) == Some(true) => {
                        Some(StringArgument::Path(PathPreference::Load))
                    }
                    "COMPILE-FILE" if state.live_standard_symbol(operator) == Some(true) => {
                        Some(StringArgument::Path(PathPreference::Source))
                    }
                    "LOAD-SYSTEM" if is_asdf_load_system(&state.runtime_snapshot, operator) => {
                        Some(StringArgument::AsdfSystem)
                    }
                    _ => None,
                }
            })
    };
    match argument {
        Some(StringArgument::Path(preference)) => state
            .lisp_pathname_directory
            .as_deref()
            .map_or_else(Vec::new, |base| {
                complete_path(&prefix, span, base, PathSyntax::Lisp, preference, theme)
            }),
        Some(StringArgument::AsdfSystem) => complete_systems(
            &prefix,
            span,
            &state.known_systems,
            lisp_string_literal,
            theme,
        ),
        None => Vec::new(),
    }
}

fn lisp_string_prefix(text: &str) -> Option<String> {
    let mut prefix = String::new();
    let mut chars = text.strip_prefix('"')?.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => break,
            '\\' => prefix.push(chars.next()?),
            _ => prefix.push(ch),
        }
    }
    Some(prefix)
}

fn is_asdf_load_system(snapshot: &RuntimeSnapshot, operator: &SymbolName) -> bool {
    let package = match &operator.prefix {
        Prefix::None => None,
        Prefix::Package { name, external } => {
            let Some(package) = runtime_package_by_name(snapshot, name) else {
                return false;
            };
            Some((&package.name, *external))
        }
        Prefix::Keyword | Prefix::Uninterned => return false,
    };
    snapshot.symbols.iter().any(|symbol| {
        symbol.name == operator.name
            && symbol.home_package.as_deref() == Some("ASDF/OPERATE")
            && package.map_or(symbol.accessible, |(name, external)| {
                symbol.package == *name && (!external || symbol.external)
            })
            && symbol
                .bindings
                .iter()
                .any(|binding| binding.kind == zkcl::NamespaceBindingKind::Function)
    })
}

fn append_runtime_completions(
    typed: &str,
    span: Span,
    suffix_bytes: usize,
    snapshot: &RuntimeSnapshot,
    seen: &mut HashSet<String>,
    out: &mut Vec<Suggestion>,
    theme: &ReplTheme,
) {
    if let Some((pkg_raw, sym_prefix)) = runtime_qualified_prefix(typed) {
        let internal = pkg_raw.ends_with(':');
        let pkg_clean = pkg_raw.trim_end_matches(':');
        if pkg_clean.is_empty() && !internal {
            return;
        }
        let target_pkg = resolve_runtime_package(snapshot, pkg_clean);
        let Some(pkg) = target_pkg else {
            return;
        };
        let sep = if internal { "::" } else { ":" };
        for sym in &snapshot.symbols {
            if sym.package != pkg.name {
                continue;
            }
            if !internal && !sym.external {
                continue;
            }
            let spelling = runtime_symbol_spelling(&sym.name);
            if !runtime_prefix_matches(&spelling, sym_prefix) {
                continue;
            }
            let label = format!("{pkg_clean}{sep}{spelling}");
            if label
                .len()
                .checked_sub(suffix_bytes)
                .is_none_or(|c| !label.is_char_boundary(c))
            {
                continue;
            }
            if !seen.insert(label.clone()) {
                continue;
            }
            let binding = &sym.bindings[0];
            let kind = runtime_completion_kind(binding.kind);
            let tag = completion_kind_tag(kind);
            let description = Some(runtime_description(sym, binding, tag));
            out.push(Suggestion {
                value: label,
                display_override: None,
                description,
                style: Some(completion_style(kind, theme)),
                extra: None,
                span,
                append_whitespace: false,
                match_indices: None,
            });
        }
        return;
    }

    let lower = typed.to_ascii_lowercase();
    for sym in &snapshot.symbols {
        if matches!(sym.package.as_str(), "COMMON-LISP" | "KEYWORD" | "ZK-SIMD") || !sym.accessible
        {
            continue;
        }
        let spelling = runtime_symbol_spelling(&sym.name);
        if !runtime_prefix_matches(&spelling, typed) {
            continue;
        }
        if spelling
            .len()
            .checked_sub(suffix_bytes)
            .is_none_or(|c| !spelling.is_char_boundary(c))
        {
            continue;
        }
        if !seen.insert(spelling.clone()) {
            continue;
        }
        let binding = &sym.bindings[0];
        let kind = runtime_completion_kind(binding.kind);
        let tag = completion_kind_tag(kind);
        let description = Some(runtime_description(sym, binding, tag));
        out.push(Suggestion {
            value: spelling,
            display_override: None,
            description,
            style: Some(completion_style(kind, theme)),
            extra: None,
            span,
            append_whitespace: false,
            match_indices: None,
        });
    }

    // Suggest package prefixes when typing an uncolonized prefix matching a runtime package.
    if !lower.is_empty() {
        for pkg in &snapshot.packages {
            if pkg.name == "KEYWORD" {
                continue;
            }
            let candidate_name = runtime_symbol_spelling(&pkg.name);
            if runtime_prefix_matches(&candidate_name, typed) {
                let label = format!("{candidate_name}:");
                if seen.insert(label.clone()) {
                    out.push(Suggestion {
                        value: label,
                        display_override: None,
                        description: Some(format!("[pkg] {} exported", pkg.external_count)),
                        style: Some(completion_style(CompletionKind::Package, theme)),
                        extra: None,
                        span,
                        append_whitespace: false,
                        match_indices: None,
                    });
                }
            }
        }
    }
}

fn runtime_qualified_prefix(typed: &str) -> Option<(&str, &str)> {
    let mut escaped = false;
    let mut quoted = false;
    let mut colon = None;
    for (at, ch) in typed.char_indices() {
        if escaped {
            escaped = false;
        } else {
            match ch {
                '\\' => escaped = true,
                '|' => quoted = !quoted,
                ':' if !quoted => colon = Some(at),
                _ => {}
            }
        }
    }
    colon.map(|at| (&typed[..at], &typed[at + 1..]))
}

fn runtime_symbol_spelling(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if SymbolName::parse(&lower)
        .is_some_and(|symbol| symbol.prefix == Prefix::None && symbol.name == name)
    {
        lower
    } else {
        format!("|{}|", name.replace('\\', "\\\\").replace('|', "\\|"))
    }
}

fn runtime_prefix_matches(spelling: &str, typed: &str) -> bool {
    if typed.contains(['|', '\\']) {
        spelling.starts_with(typed)
    } else {
        spelling.starts_with(&typed.to_ascii_lowercase())
    }
}

fn resolve_runtime_package<'a>(
    snapshot: &'a RuntimeSnapshot,
    name: &str,
) -> Option<&'a zkcl::NamespacePackage> {
    let Prefix::Package { name, .. } = SymbolName::parse(&format!("{name}:x"))?.prefix else {
        return None;
    };
    runtime_package_by_name(snapshot, &name)
}

fn runtime_package_by_name<'a>(
    snapshot: &'a RuntimeSnapshot,
    name: &str,
) -> Option<&'a zkcl::NamespacePackage> {
    if let Some((_, target)) = snapshot
        .local_nicknames
        .iter()
        .find(|(nick, _)| nick == name)
    {
        return snapshot
            .packages
            .iter()
            .find(|package| package.name == *target);
    }
    snapshot.packages.iter().find(|package| {
        package.name == name || package.nicknames.iter().any(|nickname| nickname == name)
    })
}

fn runtime_completion_kind(kind: zkcl::NamespaceBindingKind) -> CompletionKind {
    match kind {
        zkcl::NamespaceBindingKind::Function => CompletionKind::Function,
        zkcl::NamespaceBindingKind::Macro => CompletionKind::Macro,
        zkcl::NamespaceBindingKind::Variable => CompletionKind::Variable,
        zkcl::NamespaceBindingKind::Constant => CompletionKind::Constant,
        zkcl::NamespaceBindingKind::Type => CompletionKind::Type,
    }
}

fn runtime_description(
    symbol: &zkcl::NamespaceSymbol,
    binding: &zkcl::NamespaceBinding,
    tag: &str,
) -> String {
    let mut text = format!("[{tag}] {}", symbol.package);
    if let Some(lambda_list) = &binding.lambda_list {
        text.push(' ');
        text.push_str(&single_line(lambda_list));
    }
    if let Some(documentation) = &binding.documentation {
        text.push_str(" — ");
        text.push_str(&single_line(documentation));
    }
    text
}

fn token_start_before(line: &str, pos: usize) -> usize {
    line[..pos]
        .rfind(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '\'' | '`'))
        .map_or(0, |idx| {
            let ch = line[idx..].chars().next().map_or(1, char::len_utf8);
            idx + ch
        })
}

fn complete_repl_commands(
    lead: usize,
    line: &str,
    pos: usize,
    theme: &ReplTheme,
) -> Vec<Suggestion> {
    let end = line[pos..]
        .find(char::is_whitespace)
        .map_or(line.len(), |i| pos + i);
    let prefix = line[lead..pos].to_ascii_lowercase();
    let span = Span { start: lead, end };
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(&prefix))
        .map(|spec| Suggestion {
            value: spec.name.to_owned(),
            display_override: None,
            description: Some(format!("{} — {}", spec.usage, spec.summary)),
            style: Some(theme.command()),
            extra: None,
            span,
            append_whitespace: spec.arg_kind != CommandArgKind::None,
            match_indices: None,
        })
        .collect()
}

fn complete_help_args(line: &str, pos: usize, theme: &ReplTheme) -> Vec<Suggestion> {
    let start = token_start_before(line, pos);
    let prefix = line[start..pos].to_ascii_lowercase();
    let span = Span { start, end: pos };
    COMMANDS
        .iter()
        .filter(|spec| {
            spec.name.starts_with(&prefix) || spec.name.trim_start_matches(':').starts_with(&prefix)
        })
        .map(|spec| Suggestion {
            value: spec.name.to_owned(),
            display_override: None,
            description: Some(spec.summary.to_owned()),
            style: Some(theme.command()),
            extra: None,
            span,
            append_whitespace: false,
            match_indices: None,
        })
        .collect()
}

fn complete_settings(line: &str, pos: usize, theme: &ReplTheme) -> Vec<Suggestion> {
    let start = token_start_before(line, pos);
    let prefix = line[start..pos].to_ascii_lowercase();
    let span = Span { start, end: pos };
    [
        ("timing", "auto | on | off — show evaluation timing"),
        ("pretty", "on | off — pretty-print long Lisp values"),
        ("rainbow", "on | off — depth-colored rainbow parentheses"),
        ("preview", "on | off — live constant preview in status line"),
        ("hints", "on | off — inline history ghost suggestions"),
        ("edit-mode", "emacs | vi — switch editor keybindings"),
    ]
    .into_iter()
    .filter(|(k, _)| k.starts_with(&prefix))
    .map(|(k, desc)| Suggestion {
        value: k.to_owned(),
        display_override: None,
        description: Some(desc.to_owned()),
        style: Some(theme.badge()),
        extra: None,
        span,
        append_whitespace: true,
        match_indices: None,
    })
    .collect()
}

fn complete_packages(
    line: &str,
    pos: usize,
    snapshot: &RuntimeSnapshot,
    theme: &ReplTheme,
) -> Vec<Suggestion> {
    let start = token_start_before(line, pos);
    let prefix = line[start..pos]
        .trim_start_matches("#:")
        .trim_start_matches(':')
        .to_ascii_uppercase();
    let span = Span { start, end: pos };
    snapshot
        .packages
        .iter()
        .filter(|pkg| pkg.name.starts_with(&prefix))
        .map(|pkg| Suggestion {
            value: pkg.name.to_ascii_lowercase(),
            display_override: None,
            description: Some(if pkg.nicknames.is_empty() {
                format!("{} exported", pkg.external_count)
            } else {
                format!(
                    "aka {} · {} exported",
                    pkg.nicknames.join(", "),
                    pkg.external_count
                )
            }),
            style: Some(theme.package()),
            extra: None,
            span,
            append_whitespace: false,
            match_indices: None,
        })
        .collect()
}

fn complete_systems(
    prefix: &str,
    span: Span,
    known: &BTreeSet<String>,
    insert: impl Fn(&str) -> String,
    theme: &ReplTheme,
) -> Vec<Suggestion> {
    let mut names: BTreeMap<String, &'static str> = known
        .iter()
        .filter(|name| name.starts_with(prefix))
        .map(|name| (name.clone(), "registered or loaded ASDF system"))
        .collect();
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "asd")
                && let Some(name) = path.file_stem().and_then(|stem| stem.to_str())
                && name.starts_with(prefix)
                && path.is_file()
            {
                names.entry(name.to_owned()).or_insert("local .asd file");
            }
        }
    }
    names
        .into_iter()
        .filter(|(name, _)| !name.chars().any(char::is_control))
        .take(40)
        .map(|(name, description)| Suggestion {
            value: insert(&name),
            display_override: Some(name),
            description: Some(description.to_owned()),
            style: Some(theme.package()),
            extra: None,
            span,
            append_whitespace: false,
            match_indices: None,
        })
        .collect()
}

fn complete_command_path(
    line: &str,
    pos: usize,
    command: &ParsedCommand<'_>,
    theme: &ReplTheme,
) -> Vec<Suggestion> {
    let mut start = command.args_start;
    let (word, preference) = match command.spec.name {
        ":compile" => {
            let end = start + path_word_end(&line[start..]).unwrap_or(line.len() - start);
            if line[end..].starts_with(char::is_whitespace) && pos > end {
                start = line.len() - line[end..].trim_start().len();
                (true, PathPreference::Output)
            } else {
                (true, PathPreference::Source)
            }
        }
        ":break-file" => (true, PathPreference::Source),
        ":load" => (false, PathPreference::Load),
        ":cd" => (false, PathPreference::Directory),
        _ => (false, PathPreference::Any),
    };
    complete_command_path_at(line, start, pos, word, preference, theme)
}

fn complete_command_path_at(
    line: &str,
    start: usize,
    pos: usize,
    word: bool,
    preference: PathPreference,
    theme: &ReplTheme,
) -> Vec<Suggestion> {
    let quoted = line[start..].starts_with('"');
    let end = if word || quoted {
        start + path_word_end(&line[start..]).unwrap_or(line.len() - start)
    } else {
        line.trim_end().len().max(pos)
    };
    if pos < start || pos > end {
        return Vec::new();
    }
    let mut suggestions = complete_path(
        &command_path_prefix(&line[start..pos]),
        Span { start, end },
        Path::new("."),
        PathSyntax::Command { quoted },
        preference,
        theme,
    );
    suggestions.retain(|item| safe_completion_cursor(item, pos));
    suggestions
}

#[derive(Clone, Copy)]
enum PathSyntax {
    Command { quoted: bool },
    Lisp,
}

#[derive(Clone, Copy)]
enum PathPreference {
    Any,
    Directory,
    Load,
    Source,
    Output,
}

#[derive(Clone, Copy)]
enum PathKind {
    Directory,
    Source,
    Fasl,
    File,
}

impl PathPreference {
    fn rank(self, kind: PathKind) -> Option<u8> {
        match (self, kind) {
            (_, PathKind::Directory) => Some(0),
            (Self::Directory, _) => None,
            (Self::Load, PathKind::Source | PathKind::Fasl)
            | (Self::Source, PathKind::Source)
            | (Self::Output, PathKind::Fasl) => Some(1),
            _ => Some(2),
        }
    }
}

fn complete_path(
    prefix: &str,
    span: Span,
    base: &Path,
    syntax: PathSyntax,
    preference: PathPreference,
    theme: &ReplTheme,
) -> Vec<Suggestion> {
    let prefix = if prefix == "~" && matches!(syntax, PathSyntax::Command { .. }) {
        "~/"
    } else {
        prefix
    };
    let split = prefix.rfind('/').map_or(0, |at| at + 1);
    let (dir_prefix, file_prefix) = prefix.split_at(split);
    let (directory, file_prefix) = match syntax {
        PathSyntax::Command { .. } => (
            super::commands::expand_tilde(dir_prefix),
            Cow::Borrowed(file_prefix),
        ),
        PathSyntax::Lisp => {
            let directory_is_wild = dir_prefix.split('/').any(|part| matches!(part, "*" | "**"));
            let Some(directory) = decode_local_namestring_prefix(dir_prefix) else {
                return Vec::new();
            };
            let Some(file_prefix) = decode_local_namestring_prefix(file_prefix) else {
                return Vec::new();
            };
            if directory_is_wild {
                return Vec::new();
            }
            (PathBuf::from(directory), Cow::Owned(file_prefix))
        }
    };
    let directory = base.join(directory);
    let directory = if directory.as_os_str().is_empty() {
        Path::new(".")
    } else {
        &directory
    };

    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };

    let mut items: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            if name.chars().any(char::is_control)
                || (name.starts_with('.') && !file_prefix.starts_with('.'))
                || !name.starts_with(&*file_prefix)
            {
                return None;
            }
            let path = entry.path();
            let kind = if path.is_dir() {
                PathKind::Directory
            } else if crate::analysis::is_lisp_source(&path) {
                PathKind::Source
            } else if path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("zkclfasl") || extension.eq_ignore_ascii_case("fasl")
            }) {
                PathKind::Fasl
            } else {
                PathKind::File
            };
            Some((preference.rank(kind)?, name, kind))
        })
        .collect();

    items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    items
        .into_iter()
        .take(40)
        .filter_map(|(_, mut name, kind)| {
            let (desc, style) = match kind {
                PathKind::Directory => {
                    name.push('/');
                    ("directory", theme.package())
                }
                PathKind::Source => ("Lisp source", theme.function()),
                PathKind::Fasl => ("FASL", theme.function()),
                PathKind::File => ("file", theme.menu_text()),
            };
            let path = format!("{dir_prefix}{name}");
            let display_override = Some(single_line(&path));
            let value = match syntax {
                PathSyntax::Command { quoted }
                    if quoted
                        || path.contains(['"', '\\', '(', ')', '|'])
                        || path.contains(char::is_whitespace) =>
                {
                    lisp_string_literal(&path)
                }
                PathSyntax::Command { .. } => path,
                PathSyntax::Lisp => {
                    let name = zkcl::Runtime::physical_pathname_namestring(Path::new(&name))?;
                    lisp_string_literal(&format!("{dir_prefix}{name}"))
                }
            };
            Some(Suggestion {
                display_override,
                value,
                description: Some(desc.to_owned()),
                style: Some(style),
                extra: None,
                span,
                append_whitespace: false,
                match_indices: None,
            })
        })
        .collect()
}

fn decode_local_namestring_prefix(prefix: &str) -> Option<String> {
    let mut decoded = String::new();
    let mut chars = prefix.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let next = chars.next()?;
                if next == '/' {
                    return None;
                }
                decoded.push(next);
            }
            ':' => return None,
            _ => decoded.push(ch),
        }
    }
    Some(decoded)
}

fn completion_description(label: &str, detail: &str) -> Option<String> {
    let detail = detail.trim();
    let label = label.trim().to_ascii_lowercase();
    let redundant = detail.eq_ignore_ascii_case(&format!("({label})"))
        || detail.eq_ignore_ascii_case(&format!("(cl:{label})"))
        || detail.eq_ignore_ascii_case("keyword");
    (!redundant).then(|| detail.to_owned())
}

fn completion_kind_tag(kind: CompletionKind) -> &'static str {
    match kind {
        CompletionKind::Function => "fn",
        CompletionKind::Macro => "mac",
        CompletionKind::SpecialOperator => "sp",
        CompletionKind::Variable => "var",
        CompletionKind::Constant => "const",
        CompletionKind::Type => "type",
        CompletionKind::Package => "pkg",
        CompletionKind::Keyword => "kw",
    }
}

fn completion_style(kind: CompletionKind, theme: &ReplTheme) -> Style {
    match kind {
        CompletionKind::Function => theme.function(),
        CompletionKind::Macro => theme.macro_style(),
        CompletionKind::SpecialOperator => theme.operator(),
        CompletionKind::Variable => theme.variable(),
        CompletionKind::Constant => theme.constant(),
        CompletionKind::Type => theme.type_style(),
        CompletionKind::Package => theme.brand(),
        CompletionKind::Keyword => theme.keyword(),
    }
}

impl StatusLine for EditorAnalysis {
    fn status(&mut self, input: &str, position: usize, columns: u16, color: bool) -> String {
        let mut state = self.lock_source();
        let theme = state.theme;
        if input.is_empty() {
            if !state.show_empty_hint || !state.settings.hints {
                return String::new();
            }
            let hint = render_segments(
                &[(theme.muted(), EMPTY_INPUT_HINT.to_owned())],
                columns,
                color,
            );
            state.show_empty_hint = false;
            return hint;
        }
        if input.trim().is_empty() {
            return String::new();
        }

        // REPL :command status bar
        if let Some(parsed) = parse_command(input) {
            let spec = parsed.spec;
            if parsed.lisp_args().is_some_and(|args| !args.is_empty())
                && position >= parsed.args_start
            {
                let status = render_lisp_status(
                    &mut state,
                    input,
                    parsed.args_start,
                    position,
                    columns,
                    color,
                    parsed.evaluates_form(),
                );
                if !status.is_empty() {
                    return status;
                }
            }
            let mut segs = vec![
                (theme.muted(), spec.usage.to_owned()),
                (theme.muted(), STATUS_GAP.to_owned()),
                (theme.muted(), spec.summary.to_owned()),
            ];
            if !spec.aliases.is_empty() {
                segs.push((theme.muted(), format!(" ({})", spec.aliases.join(", "))));
            }
            return render_segments(&segs, columns, color);
        }

        render_lisp_status(&mut state, input, 0, position, columns, color, true)
    }
}

fn render_lisp_status(
    state: &mut ReplSource,
    buffer: &str,
    start: usize,
    position: usize,
    columns: u16,
    color: bool,
    evaluate: bool,
) -> String {
    if !state.has_static_reader() {
        return String::new();
    }
    let input = &buffer[start..];
    let theme = state.theme;
    let snapshot = state.sync_input(input);
    let location =
        |range: TextRange| input_location(&snapshot, buffer, start, usize::from(range.start()));
    let mut segments = Vec::new();
    if let Some(diagnostic) = snapshot.parsed().primary_diagnostic() {
        let (icon, style) = if diagnostic.needs_more_input() {
            ("…", theme.incomplete())
        } else {
            ("x", theme.error())
        };
        segments.push((
            style,
            format!(
                "{icon} {} {}",
                location(diagnostic.range()),
                diagnostic.message()
            ),
        ));
        if !diagnostic.needs_more_input() {
            return render_segments(&segments, columns, color);
        }
    }

    if !evaluate {
        return render_segments(&segments, columns, color);
    }

    let position = (position - start).min(input.len());
    let feedback = state.call_feedback(position);
    let signature = match feedback.as_deref() {
        Some(CallFeedback::ArityError { range, message }) => {
            return render_segments(
                &[(theme.error(), format!("x {} {message}", location(*range)))],
                columns,
                color,
            );
        }
        Some(CallFeedback::Signature(signature)) => Some(signature),
        _ => None,
    };
    let input_feedback = state.input_feedback();
    if let Some(error) = &input_feedback.type_error {
        return render_segments(
            &[(
                theme.error(),
                format!("x {} {}", location(error.range), error.message),
            )],
            columns,
            color,
        );
    }

    let limit = usize::from(columns).saturating_sub(STATUS_INDENT.len() + 1);
    let result_label = if let Some(values) = &input_feedback.multiple_values {
        Some(if values.is_empty() {
            "→ no values".into()
        } else {
            format!("→ {} values", values.len())
        })
    } else {
        input_feedback.output_type.map(|ty| format!("→ {ty}"))
    };
    let preview = input_feedback
        .preview
        .as_ref()
        .map(|value| format!("= {value}"))
        .or_else(|| {
            let values = input_feedback.multiple_values.as_ref()?;
            (!values.is_empty()).then(|| format!("= {}", values.join("  ·  ")))
        });
    let result_width = result_label.as_ref().map_or(0, |label| label.width())
        + preview.as_ref().map_or(0, |value| {
            single_line(value).width().min(6)
                + result_label.as_ref().map_or(0, |_| STATUS_GAP.len())
        });
    if let Some(sig) = &signature {
        let signature = signature_segments(sig, &theme, color);
        let required = segments_width(&segments)
            + gap_width(&segments)
            + segments_width(&signature)
            + STATUS_GAP.len()
            + result_width;
        if result_width == 0 || required <= limit {
            push_status_gap(&mut segments);
            segments.extend(signature);
        }
    }
    if let Some(label) = result_label {
        push_status_gap(&mut segments);
        segments.push((theme.type_style(), label));
    }
    if let Some(text) = preview {
        let available = limit.saturating_sub(segments_width(&segments) + gap_width(&segments));
        if segments.is_empty() || available >= single_line(&text).width().min(4) {
            push_status_gap(&mut segments);
            segments.push((theme.muted(), text));
        }
    }
    render_segments(&segments, columns, color)
}

fn input_location(snapshot: &SourceSnapshot, buffer: &str, start: usize, offset: usize) -> String {
    let command = (start != 0).then(|| SourceDocument::input(buffer));
    let source = command.as_ref().unwrap_or_else(|| snapshot.source());
    let position = source
        .position_with_encoding(start + offset, SourcePositionEncoding::Utf8)
        .expect("input diagnostic has a source position");
    let line = source.line_text(position.line).expect("input line");
    format!(
        "{}:{}",
        position.line + 1,
        displayed_width(line, position.column) + 1
    )
}

fn segments_width(segments: &[(Style, String)]) -> usize {
    segments
        .iter()
        .map(|(_, text)| single_line(text).width())
        .sum()
}

fn gap_width(segments: &[(Style, String)]) -> usize {
    if segments.is_empty() {
        0
    } else {
        STATUS_GAP.len()
    }
}

fn push_status_gap(segments: &mut Vec<(Style, String)>) {
    if !segments.is_empty() {
        segments.push((Style::new(), STATUS_GAP.into()));
    }
}

fn displayed_width(line: &str, end: usize) -> usize {
    let mut width = 0;
    for c in line[..line.floor_char_boundary(end.min(line.len()))].chars() {
        match c {
            '\t' => width += 8 - width % 8,
            c if c.is_control() => {
                width += c
                    .escape_unicode()
                    .to_string()
                    .chars()
                    .filter_map(UnicodeWidthChar::width)
                    .sum::<usize>();
            }
            c => width += c.width().unwrap_or(0),
        }
    }
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis() -> EditorAnalysis {
        EditorAnalysis::new(Some("COMMON-LISP-USER".into()))
    }

    #[test]
    fn only_eof_diagnostics_continue_a_submission() {
        let mut analysis = analysis();
        for input in ["(list 1", "'", "#| block comment", "\"string"] {
            assert!(matches!(
                analysis.validate(input),
                ValidationResult::Incomplete
            ));
            assert!(
                analysis
                    .status(input, input.len(), 80, false)
                    .starts_with("  …")
            );
        }
        for input in ["(')", "'(a .)", ") (", "\"雪😀\")\r\n("] {
            assert!(matches!(
                analysis.validate(input),
                ValidationResult::Complete
            ));
            assert!(
                analysis
                    .status(input, input.len(), 80, false)
                    .starts_with("  x")
            );
        }
        assert_eq!(
            analysis.status("\"雪😀\")", 9, 80, false),
            "  x 1:7 unexpected closing parenthesis"
        );
        assert!(matches!(
            analysis.validate("(list\n 1)"),
            ValidationResult::Complete
        ));
        assert_eq!(
            analysis.status("(list\n 1)", 9, 80, false),
            "  → list   = (1)"
        );
    }

    #[test]
    fn repl_commands_validate_lisp_form_arguments() {
        let analysis = analysis();
        assert!(matches!(
            analysis.validate(":help"),
            ValidationResult::Complete
        ));
        assert!(matches!(
            analysis.validate(":time (+ 1"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            analysis.validate(":time (+ 1 2)"),
            ValidationResult::Complete
        ));
        assert!(matches!(
            analysis.validate(":lint @foo.lisp"),
            ValidationResult::Complete
        ));
    }

    #[test]
    fn command_feedback_uses_buffer_coordinates_and_keeps_reader_whitespace() {
        let mut analysis = analysis();
        let input = "  :time (+ \"x\" 1)   ";
        let command = parse_command(input).unwrap();
        assert_eq!(command.args_start, 8);
        assert_eq!(command.lisp_args(), Some("(+ \"x\" 1)   "));
        assert_eq!(
            analysis.status(input, input.len(), 80, false),
            "  x 1:12 expected a number, found string"
        );
        let literal = ":time #\\ ";
        assert!(matches!(
            analysis.validate(literal),
            ValidationResult::Complete
        ));
        assert!(
            analysis
                .status(literal, literal.len(), 80, false)
                .contains("→ character")
        );
        let macro_input = ":macroexpand (+ \"x\" 1)";
        let status = analysis.status(macro_input, macro_input.len(), 80, false);
        assert!(status.contains("macroexpand"), "{status}");
        assert!(!status.contains("expected a number"), "{status}");
    }

    #[test]
    fn status_line_displays_active_parameter_in_call_forms() {
        let mut analysis = analysis();
        let input = "(subseq \"hello\" 1 3)";
        let status_first = analysis.status(input, "(subseq \"hello\"".len(), 80, false);
        assert!(status_first.contains("[sequence]"), "{status_first}");
        assert!(status_first.contains("= \"el\""), "{status_first}");
        let status_second = analysis.status(input, "(subseq \"hello\" 1".len(), 80, false);
        assert!(status_second.contains("[start]"), "{status_second}");
    }

    #[test]
    fn status_line_previews_pure_expression_values() {
        let mut analysis = analysis();
        let status = analysis.status("(+ 10 20 12)", "(+ 10 20 12)".len(), 80, false);
        assert!(status.contains("→ integer"), "{status}");
        assert!(status.contains("= 42"), "{status}");
    }

    #[test]
    fn status_preserves_type_and_preview_with_long_input_or_narrow_terminal() {
        let mut analysis = analysis();
        let input =
            "(+ 1 2 3 4    4 -1 -2 -1000000000000000000000000000000000000000000000000000000)";
        assert_eq!(
            analysis.status(input, input.len(), 85, false),
            "  → integer   = -999999999999999999999999999999999999999999999999999989"
        );

        let input = "(subseq \"世界界界界界界界界界界界界界界界\" 0)";
        let status = analysis.status(input, "(subseq ".len(), 22, false);
        assert_eq!(status, "  → string   = \"世界…");
        assert_eq!(status.width(), 21);

        let input = "(list \"👩‍💻界\")";
        let status = analysis.status(input, input.len(), 19, false);
        assert_eq!(status, "  → list   = (\"👩‍💻…");
        assert_eq!(status.width(), 18);

        let input = "(list \"a\n\tb\u{1b}\")";
        assert_eq!(
            analysis.status(input, input.len(), 80, false),
            "  → list   = (\"a\\n\\tb\\u{1b}\")"
        );
    }

    #[test]
    fn status_keeps_type_when_the_value_is_unavailable_or_preview_is_disabled() {
        let mut analysis = analysis();
        for (input, expected) in [
            ("(+ #xff 1)", "  → integer   = 256"),
            ("(/ 3 2)", "  → rational   = 3/2"),
            ("(+ 1.0d0 2)", "  → float"),
            (
                "(progn (princ \"no output while typing\") 42)",
                "  → integer",
            ),
        ] {
            assert_eq!(analysis.status(input, input.len(), 80, false), expected);
        }
        let input = "(+\n 1 2)";
        assert_eq!(
            analysis.status(input, input.len(), 80, false),
            "  → integer   = 3"
        );
        let settings = ReplSettings {
            preview: false,
            ..analysis.settings()
        };
        analysis.set_settings(settings);
        assert_eq!(
            analysis.status(input, input.len(), 80, false),
            "  → integer"
        );
    }

    #[test]
    fn live_reader_changes_invalidate_cached_static_feedback() {
        let mut analysis = analysis();
        let runtime = zkcl::Runtime::start().unwrap();
        runtime.eval("nil").unwrap();
        let input = "(+ 10 1)";
        analysis.refresh_runtime_snapshot(&runtime);
        assert_eq!(
            analysis.status(input, input.len(), 80, false),
            "  → integer   = 11"
        );

        runtime.eval("(setq *read-base* 16)").unwrap();
        analysis.refresh_runtime_snapshot(&runtime);
        let value = runtime.eval(input).unwrap();
        assert_eq!(value[0].as_fixnum(), Some(17));
        assert_eq!(analysis.status(input, 3, 80, false), "");
        assert_eq!(analysis.static_expression_type(input), None);

        runtime.eval("(setq *read-base* 10.)").unwrap();
        analysis.refresh_runtime_snapshot(&runtime);
        assert_eq!(
            analysis.status(input, input.len(), 80, false),
            "  → integer   = 11"
        );

        runtime.eval("(setq *read-suppress* t)").unwrap();
        analysis.refresh_runtime_snapshot(&runtime);
        assert_eq!(analysis.status(input, input.len(), 80, false), "");
        let set = runtime
            .lookup_symbol("COMMON-LISP", "SET")
            .unwrap()
            .unwrap();
        let suppress = runtime
            .lookup_symbol("COMMON-LISP", "*READ-SUPPRESS*")
            .unwrap()
            .unwrap();
        runtime.call(&set, &[&suppress, &runtime.nil()]).unwrap();
        analysis.refresh_runtime_snapshot(&runtime);
        assert_eq!(
            analysis.status(input, input.len(), 80, false),
            "  → integer   = 11"
        );

        runtime
            .eval("(setf (readtable-case *readtable*) :preserve)")
            .unwrap();
        analysis.refresh_runtime_snapshot(&runtime);
        assert_eq!(analysis.status(input, input.len(), 80, false), "");
    }

    #[test]
    fn terminal_feedback_tracks_actual_standard_symbol_visibility() {
        let mut analysis = analysis();
        let runtime = zkcl::Runtime::start().unwrap();
        runtime.eval("nil").unwrap();
        let input = "(if t (+ 1 2) 0)";
        analysis.refresh_runtime_snapshot(&runtime);
        assert_eq!(
            analysis.status(input, input.len(), 80, false),
            "  → integer   = 3"
        );

        runtime
            .load_str("(defpackage :preview-shadow (:use :cl) (:shadow :+ :if)) (in-package :preview-shadow)")
            .unwrap();
        analysis.refresh_runtime_snapshot(&runtime);
        assert_eq!(analysis.status(input, input.len(), 80, false), "");
        assert_eq!(analysis.status("(if 1 2 3 4)", 11, 80, false), "");
        let qualified = "(cl:if cl:t (cl:+ 1 2) 0)";
        assert_eq!(
            analysis.status(qualified, qualified.len(), 80, false),
            "  → integer   = 3"
        );
    }

    #[test]
    fn status_line_names_statically_known_output_types() {
        let mut analysis = analysis();
        assert!(
            analysis
                .status("(format nil \"x\")", "(format nil \"x\")".len(), 80, false)
                .contains("→ string")
        );
        let list_status = analysis.status("(list 1 2)", "(list 1 2)".len(), 80, false);
        assert!(list_status.contains("→ list"), "{list_status}");
        assert!(list_status.contains("= (1 2)"), "{list_status}");
        assert_eq!(analysis.status("1.5", 3, 80, false), "  → float");
        assert_eq!(
            analysis.status(":demo", 5, 80, false),
            "  → keyword   = :DEMO"
        );
        assert_eq!(
            analysis.status("'(1 2)", 6, 80, false),
            "  → list   = (1 2)"
        );
    }

    #[test]
    fn value_preview_preserves_zero_and_multiple_values_in_tail_positions() {
        let mut analysis = analysis();
        for (input, expected) in [
            ("(values nil)", "  → null   = NIL"),
            ("(values)", "  → no values"),
            (
                "(let ((x 2)) (if t (values (+ x 1) nil) (values)))",
                "  → 2 values   = 3  ·  NIL",
            ),
            ("(list (values 1 2) (values))", "  → list   = (1 NIL)"),
            (
                "(multiple-value-list (or (values t 2) (values 3 4)))",
                "  → list   = (T)",
            ),
            ("(and t (values 1 2))", "  → 2 values   = 1  ·  2"),
        ] {
            assert_eq!(
                analysis.status(input, input.len(), 80, false),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn unknown_body_types_stay_unknown() {
        let mut analysis = analysis();
        for input in [
            "(progn unknown)",
            "(locally unknown)",
            "(let () unknown)",
            "(let* () unknown)",
            "(multiple-value-bind (x) (read) x)",
            "(destructuring-bind (x) (read) x)",
            "(eval-when (:execute) unknown)",
            "(if t nil unknown)",
            "(car (progn unknown))",
        ] {
            let status = analysis.status(input, input.len(), 80, false);
            assert!(!status.contains('→'), "{input}: {status}");
            assert!(!status.starts_with("  x "), "{input}: {status}");
        }
        for input in ["(progn)", "(let ())", "(if t nil)"] {
            let status = analysis.status(input, input.len(), 80, false);
            assert!(status.contains("→ null"), "{input}: {status}");
        }
    }

    #[test]
    fn shortcuts_hint_is_shown_on_the_first_empty_paint() {
        let mut analysis = analysis();
        let status = analysis.status("", 0, 80, false);
        assert!(status.starts_with("  Tab complete"), "{status}");
        assert!(status.contains("Ctrl+J newline"), "{status}");
        assert!(status.contains("Ctrl+G editor"), "{status}");
        assert!(status.contains(":help"), "{status}");
        assert_eq!(analysis.status("   ", 3, 80, false), "");
    }

    #[test]
    fn status_line_reports_arity_errors_at_cursor() {
        let mut analysis = analysis();
        assert_eq!(
            analysis.status("(cons 1 2 3)", "(cons 1 2 3".len(), 80, false),
            "  x 1:2 cons takes at most 2 arguments, found 3"
        );
        let truncated = analysis.status("(cons 1 2 3)", "(cons 1 2 3".len(), 12, false);
        assert_eq!(truncated, "  x 1:2 co…");
        let input = "(cons 1)  ";
        assert!(
            analysis
                .status(input, input.len(), 80, false)
                .starts_with("  x 1:2 cons ")
        );

        let nested = "(abs (cons 1 2) 3)";
        assert_eq!(
            analysis.status(nested, "(abs (cons 1".len(), 80, false),
            "  x 1:2 abs takes at most 1 argument, found 2"
        );
        let shadowed = "(cons (flet ((cons (x) x)) (cons 1)) 2)";
        let position = shadowed.rfind("1)").unwrap();
        assert!(
            !analysis
                .status(shadowed, position, 80, false)
                .starts_with("  x ")
        );
    }

    #[test]
    fn status_line_reports_obvious_standard_operator_type_errors() {
        let mut analysis = analysis();
        assert_eq!(
            analysis.status("(+ \"1123\" 1)", "(+ \"1123\" 1)".len(), 80, false),
            "  x 1:4 expected a number, found string"
        );
        let unclosed_call = "(+ \"bad\" 1";
        assert_eq!(
            analysis.status(unclosed_call, unclosed_call.len(), 80, false),
            "  x 1:4 expected a number, found string"
        );
        let unclosed_string = "(+ \"bad";
        assert!(
            analysis
                .status(unclosed_string, unclosed_string.len(), 80, false)
                .starts_with("  …")
        );
        assert_eq!(
            analysis.status("(car 1)", "(car 1)".len(), 80, false),
            "  x 1:6 expected a list, found integer"
        );
        assert_eq!(
            analysis.status("(funcall 1)", "(funcall 1)".len(), 80, false),
            "  x 1:10 expected a function designator, found integer"
        );
        assert_eq!(
            analysis.status("(list (+ 1 \"x\"))", "(list (+ 1 \"x\"))".len(), 80, false),
            "  x 1:12 expected a number, found string"
        );
    }

    #[test]
    fn static_type_checks_leave_dynamic_forms_alone() {
        let mut analysis = analysis();
        let unknown = analysis.status("(+ value 1)", "(+ value 1)".len(), 80, false);
        assert!(!unknown.starts_with("  x "), "{unknown}");
        let quoted = analysis.status("'(+ \"x\" 1)", "'(+ \"x\" 1)".len(), 80, false);
        assert!(!quoted.starts_with("  x "), "{quoted}");
        assert!(quoted.contains("→ list"), "{quoted}");
    }

    #[test]
    fn static_type_checks_respect_callable_shadowing() {
        let mut analysis = analysis();
        let local = "(flet ((+ (a b) \"x\")) (+ \"a\" \"b\"))";
        assert!(
            !analysis
                .status(local, local.len(), 80, false)
                .starts_with("  x ")
        );

        analysis
            .lock_source()
            .record_submission("(defun length (x) 42)");
        let redefined = "(length 1)";
        assert!(
            !analysis
                .status(redefined, redefined.len(), 80, false)
                .starts_with("  x ")
        );

        let same_input = "(defun + (a b) \"x\") (+ \"a\" \"b\")";
        assert!(
            !analysis
                .status(same_input, same_input.len(), 80, false)
                .starts_with("  x ")
        );
    }

    #[test]
    fn shadowed_callables_disable_standard_previews_and_inferred_types() {
        let mut analysis = analysis();
        let expression = "(+ 1 2)";
        assert!(
            analysis
                .status(expression, expression.len(), 80, false)
                .contains("= 3")
        );
        analysis.record_submission(
            "(defun + (a b) \"string\")",
            Some("COMMON-LISP-USER".into()),
        );
        let status = analysis.status(expression, expression.len(), 80, false);
        assert!(!status.contains('→'), "{status}");
        assert!(!status.contains("= 3"), "{status}");

        let nested = "(car (+ 1 2))";
        assert!(
            !analysis
                .status(nested, nested.len(), 80, false)
                .starts_with("  x ")
        );

        let local = "(flet ((+ (a b) \"string\")) (+ 1 2))";
        let status = analysis.status(local, local.len(), 80, false);
        assert!(!status.contains('→'), "{status}");
        assert!(!status.contains("= "), "{status}");

        let unrelated = "(list 1 2)";
        let unrelated_status = analysis.status(unrelated, unrelated.len(), 80, false);
        assert!(unrelated_status.contains("→ list"), "{unrelated_status}");
        assert!(unrelated_status.contains("= (1 2)"), "{unrelated_status}");
    }

    #[test]
    fn input_feedback_is_shared_by_repaints_and_invalidated_by_session_changes() {
        let analysis = analysis();
        let feedback = |input| {
            let mut state = analysis.lock_source();
            state.sync_input(input);
            state.input_feedback()
        };
        let initial = feedback("(+ 1 2)");
        let _ = analysis.highlight("(+ 1 2)", 1);
        assert!(Arc::ptr_eq(&initial, &feedback("(+ 1 2)")));
        assert_eq!(initial.preview.as_deref(), Some("3"));

        analysis.record_submission("(defun + (a b) \"string\")", None);
        let changed = feedback("(+ 1 2)");
        assert!(!Arc::ptr_eq(&initial, &changed));
        assert!(changed.preview.is_none());
        assert!(!Arc::ptr_eq(&changed, &feedback("(+ 2 3)")));

        let before_package = feedback("42");
        analysis
            .lock_source()
            .set_active_package(Some("COMMON-LISP-USER".into()));
        assert!(!Arc::ptr_eq(&before_package, &feedback("42")));

        let before_readtable = feedback("42");
        analysis
            .lock_source()
            .set_input_readtable(SourceReadtable::RawStrings);
        assert!(!Arc::ptr_eq(&before_readtable, &feedback("42")));

        let before_reset = feedback("42");
        analysis.reset(None);
        assert!(!Arc::ptr_eq(&before_reset, &feedback("42")));
    }

    #[test]
    fn reload_keeps_unchanged_file_snapshots_and_clears_session_sources() {
        let directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/repl-regression");
        fs::create_dir_all(&directory).unwrap();
        let temp = tempfile::tempdir_in(directory).unwrap();
        let stable = temp.path().join("stable.lisp");
        let edited = temp.path().join("edited.lisp");
        let removed = temp.path().join("removed.lisp");
        for path in [&stable, &edited, &removed] {
            fs::write(path, "(defun before-reload () 1)").unwrap();
        }

        let analysis = analysis();
        let package = Some("COMMON-LISP-USER".into());
        for path in [&stable, &edited, &removed] {
            analysis.index_file(path);
        }
        analysis.record_submission("(defun only-in-old-session () 2)", package.clone());
        let stable_name = format!("loaded-{}", stable.display());
        let before = analysis
            .lock_source()
            .workspace
            .snapshot(&stable_name)
            .unwrap();
        assert!(!before.outline().is_empty());
        assert!(
            analysis
                .lock_source()
                .workspace
                .snapshot("session-0")
                .is_some()
        );

        let new_text = "(defun after-reload () 3)";
        fs::write(&edited, new_text).unwrap();
        fs::remove_file(&removed).unwrap();
        analysis.reload_files(package, [&stable, &edited, &removed].map(PathBuf::as_path));

        let source = analysis.lock_source();
        let after = source.workspace.snapshot(&stable_name).unwrap();
        assert!(Arc::ptr_eq(&before, &after));
        let updated = source
            .workspace
            .snapshot(&format!("loaded-{}", edited.display()))
            .unwrap();
        assert_eq!(updated.text(), new_text);
        assert!(
            source
                .workspace
                .snapshot(&format!("loaded-{}", removed.display()))
                .is_none()
        );
        assert!(source.workspace.snapshot("session-0").is_none());
        assert_eq!(
            source.workspace.snapshot(INPUT_DOCUMENT).unwrap().text(),
            ""
        );
    }

    #[test]
    fn editor_reuses_definitions_from_previous_submissions() {
        let mut analysis = analysis();
        analysis
            .lock_source()
            .record_submission("(defun compute-total (price tax) (+ price tax))");

        let status = analysis.status(
            "(compute-total 100 ",
            "(compute-total 100 ".len(),
            80,
            false,
        );
        assert!(status.contains("COMPUTE-TOTAL"), "{status}");
        assert!(status.contains("[tax]"), "{status}");

        let result = analysis.complete("(compute-t", "(compute-t".len());
        assert!(
            result
                .suggestions()
                .iter()
                .any(|s| s.value == "compute-total"),
            "{:?}",
            result.suggestions()
        );
    }

    #[test]
    fn highlighting_preserves_text_while_styling_tokens() {
        let analysis = analysis();
        let line = "(defun f (x) \"hi\" ; comment\n  (+ x 1))";
        let styled = analysis.highlight(line, 1);
        assert_eq!(styled.raw_string(), line);
        assert!(styled.buffer.len() > 4);
    }

    #[test]
    fn matching_parentheses_use_color_without_the_cursor_like_underline() {
        let analysis = analysis();
        let styled = analysis.highlight("(+ 1 2)", 1);
        let matched: Vec<_> = styled
            .buffer
            .iter()
            .filter(|(_, text)| text == "(" || text == ")")
            .collect();
        assert_eq!(matched.len(), 2, "{:?}", styled.buffer);
        for (style, _) in matched {
            assert_eq!(style.foreground, Some(Color::LightCyan));
            assert!(style.background.is_none());
            assert!(style.is_bold);
            assert!(!style.is_underline);
        }
    }

    #[test]
    fn static_type_errors_underline_the_offending_argument() {
        let analysis = analysis();
        let styled = analysis.highlight("(+ \"1123\" 1)", 0);
        let error_style = styled
            .buffer
            .iter()
            .find(|(style, text)| style.is_underline && text.contains("\"1123\""))
            .map(|(style, _)| style);
        assert!(error_style.and_then(|s| s.foreground).is_some());
    }

    #[test]
    fn auto_pairing_checks_reader_context() {
        let analysis = analysis();
        for (input, can_pair) in [
            ("(list ", true),
            ("\"unclosed ", false),
            ("; comment ", false),
            ("#| block ", false),
            ("#\\", false),
            ("#12\\", false),
            ("; \r comment", false),
            ("#", true),
            ("#\\雪", true),
        ] {
            assert_eq!(
                analysis.snapshot(input).should_auto_pair_at(input.len()),
                can_pair,
                "{input:?}"
            );
        }
    }

    #[test]
    fn highlighting_reuses_tokens_for_unchanged_source() {
        let analysis = analysis();
        let tokens = |input| {
            let mut state = analysis.lock_source();
            state.sync_input(input);
            state.input_tokens()
        };
        let initial = tokens("(latest)");
        assert!(Arc::ptr_eq(&initial, &tokens("(latest)")));

        analysis
            .lock_source()
            .record_submission("(defun latest () 1)");
        let defined = tokens("(latest)");
        assert!(!Arc::ptr_eq(&initial, &defined));
        assert!(
            defined
                .iter()
                .any(|token| token.kind == SemanticTokenKind::Function)
        );
        assert!(!Arc::ptr_eq(&defined, &tokens("(latest 1)")));
    }

    #[test]
    fn completer_returns_candidates_with_atom_span() {
        let mut analysis = analysis();
        let line = "(defuxyz foo)";
        let result = analysis.complete(line, "(defu".len());
        assert!(
            result.suggestions().iter().any(|s| s.value == "defun"),
            "{:?}",
            result.suggestions()
        );
        let defun = result
            .suggestions()
            .iter()
            .find(|s| s.value == "defun")
            .unwrap();
        assert_eq!(&line[defun.span.start..defun.span.end], "defuxyz");

        analysis
            .lock_source()
            .record_submission("(defun |DEMO-雪X| () 42)");
        let needle = "(demo-";
        let has_candidate =
            |result: CompletionResult| result.suggestions().iter().any(|s| s.value == "demo-雪x");
        assert!(has_candidate(analysis.complete("(demo-z)", needle.len())));
        assert!(!has_candidate(analysis.complete("(demo-é)", needle.len())));
    }

    #[test]
    fn completer_suggests_repl_commands_and_settings() {
        let mut analysis = analysis();
        let cmds = analysis.complete(":ma", 3);
        assert!(
            cmds.suggestions().iter().any(|s| s.value == ":macroexpand"),
            "{:?}",
            cmds.suggestions()
        );
        let settings = analysis.complete(":set ra", 7);
        assert!(
            settings.suggestions().iter().any(|s| s.value == "rainbow"),
            "{:?}",
            settings.suggestions()
        );
    }

    fn completion_fixture() -> tempfile::TempDir {
        let directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/repl-completion");
        fs::create_dir_all(&directory).unwrap();
        tempfile::tempdir_in(directory).unwrap()
    }

    #[test]
    fn command_paths_round_trip_quotes_and_replace_the_active_word() {
        let temp = completion_fixture();
        let source = temp.path().join("雪\" \\ input.lisp");
        let output = temp.path().join("雪 output.zkclfasl");
        fs::write(&source, "nil").unwrap();
        fs::write(&output, "").unwrap();
        let directory = temp.path().join("雪 directory");
        fs::create_dir(&directory).unwrap();

        let mut analysis = analysis();
        let prefix = format!(":compile \"{}/雪", temp.path().display());
        let line = format!("{prefix} stale.lisp\" \"existing output.zkclfasl\"");
        let result = analysis.complete(&line, prefix.len());
        let suggestion = result
            .suggestions()
            .iter()
            .find(|item| item.description.as_deref() == Some("Lisp source"))
            .unwrap();
        let mut completed = line.clone();
        completed.replace_range(
            suggestion.span.start..suggestion.span.end,
            &suggestion.value,
        );
        let command = parse_command(&completed).unwrap();
        assert_eq!(
            super::super::commands::compile_paths(command.args).unwrap(),
            (
                source.clone(),
                Some(PathBuf::from("existing output.zkclfasl"))
            )
        );

        let line = format!(
            ":compile {} \"{}/雪",
            suggestion.value,
            temp.path().display()
        );
        let result = analysis.complete(&line, line.len());
        assert_eq!(result.suggestions()[1].description.as_deref(), Some("FASL"));

        let line = format!(":cd {}/雪", temp.path().display());
        let result = analysis.complete(&line, line.len());
        assert_eq!(result.suggestions().len(), 1);
        assert_eq!(
            super::super::commands::command_path(&result.suggestions()[0].value).unwrap(),
            directory.join("")
        );
    }

    #[test]
    fn lisp_file_completion_uses_known_reader_and_live_pathname_default() {
        let temp = completion_fixture();
        let path = temp.path().join("雪\" \\ input.lisp");
        fs::write(&path, "(defparameter cl-user::*completed-file* 42)").unwrap();
        let runtime = zkcl::Runtime::start().unwrap();
        let mut analysis = analysis();
        analysis.refresh_runtime_snapshot(&runtime);
        super::super::snapshot::sync_lisp_cwd(&runtime, temp.path()).unwrap();
        analysis.refresh_runtime_snapshot(&runtime);

        for prefix in ["(cl:load \"雪", "(compile-file \"雪", "(cl:load #P\"雪"] {
            let line = format!("{prefix} stale.lisp\")");
            let result = analysis.complete(&line, prefix.len());
            let [item] = result.suggestions() else {
                panic!("{line}: {:?}", result.suggestions());
            };
            assert_eq!(&line[item.span.start..item.span.end], "\"雪 stale.lisp\"");
            if !prefix.starts_with("(compile-file") {
                let mut completed = line.clone();
                completed.replace_range(item.span.start..item.span.end, &item.value);
                runtime.eval(&completed).unwrap();
                let value = runtime.eval("cl-user::*completed-file*").unwrap();
                assert_eq!(runtime.repr(&value[0]).unwrap(), "42");
            }
        }

        for input in [
            "(list \"defu",
            "'(cl:load \"雪",
            "(flet ((cl:load (file) file)) (cl:load \"雪",
            "(cl:load file :external-format \"雪",
            "#1p\"雪",
        ] {
            assert!(
                analysis
                    .complete(input, input.len())
                    .suggestions()
                    .is_empty(),
                "{input}"
            );
        }
    }

    #[test]
    fn asdf_string_completion_requires_the_loaded_operator() {
        let runtime = zkcl::Runtime::start().unwrap();
        let mut analysis = analysis();
        analysis.remember_systems(["repl-test-suite/tools".to_owned()]);
        analysis.refresh_runtime_snapshot(&runtime);
        let prefix = "(asdf:load-system \"repl-test";
        let line = format!("{prefix} stale\")");
        assert!(
            analysis
                .complete(&line, prefix.len())
                .suggestions()
                .is_empty()
        );

        runtime.require_module("asdf").unwrap();
        analysis.refresh_runtime_snapshot(&runtime);
        let result = analysis.complete(&line, prefix.len());
        let [system] = result.suggestions() else {
            panic!("{:?}", result.suggestions());
        };
        assert_eq!(system.value, "\"repl-test-suite/tools\"");
        assert_eq!(
            &line[system.span.start..system.span.end],
            "\"repl-test stale\""
        );
    }

    #[test]
    fn completions_have_compact_descriptions_and_kind_styles() {
        let mut analysis = analysis();
        let empty = analysis.complete("", 0);
        assert!(
            empty
                .suggestions()
                .iter()
                .all(|suggestion| !suggestion.value.starts_with('&'))
        );
        let allow = analysis
            .complete("&", 1)
            .suggestions()
            .iter()
            .find(|suggestion| suggestion.value == "&allow-other-keys")
            .cloned()
            .expect("lambda-list keyword completion");
        assert!(allow.description.is_none(), "{:?}", allow.description);
        assert!(allow.style.and_then(|s| s.foreground).is_some());

        let defun = empty
            .suggestions()
            .iter()
            .find(|suggestion| suggestion.value == "defun")
            .expect("DEFUN completion");
        assert!(
            defun
                .description
                .as_deref()
                .is_some_and(|description| description.contains("function-name")),
            "{:?}",
            defun.description
        );
        assert!(defun.style.and_then(|s| s.foreground).is_some());
    }

    #[test]
    fn smart_lisp_indent_aligns_body_and_argument_forms() {
        assert_eq!(compute_lisp_indent("(defun foo (x)"), 2);
        assert_eq!(compute_lisp_indent("  (let ((a 1))"), 4);
        assert_eq!(compute_lisp_indent("(list 1 2"), 6);
        assert_eq!(compute_lisp_indent("\"(defun foo"), 0);
        assert_eq!(compute_lisp_indent("(+ 1 2)"), 0);
    }

    #[test]
    fn opening_completion_hides_the_empty_input_hint() {
        let mut analysis = analysis();
        assert!(!analysis.status("", 0, 80, false).is_empty());
        let _ = analysis.complete("", 0);
        assert_eq!(analysis.status("", 0, 80, false), "");
    }

    #[test]
    fn session_skips_indexing_expressions_without_definitions() {
        let analysis = analysis();
        analysis.lock_source().record_submission("(+ 1 2)");
        let state = analysis.lock_source();
        assert!(state.submission_documents.is_empty());
    }

    #[test]
    fn session_indexes_submissions_in_active_package() {
        let mut analysis = analysis();
        {
            let mut state = analysis.lock_source();
            state.set_active_package(Some("DEMO".into()));
            state.record_submission("(defun demo-identity (arg) arg)");
        }
        let status = analysis.status("(demo-identity ", "(demo-identity ".len(), 80, false);
        assert!(status.contains("DEMO-IDENTITY"), "{status}");
        assert!(status.contains("[arg]"), "{status}");
    }

    #[test]
    fn session_caps_indexed_submissions() {
        let analysis = analysis();
        let mut state = analysis.lock_source();
        for idx in 0..(MAX_SESSION_DOCUMENTS + 10) {
            state.record_submission(&format!("(defun fn-{idx} () {idx})"));
        }
        assert_eq!(state.submission_documents.len(), MAX_SESSION_DOCUMENTS);
    }
    #[test]
    fn runtime_completion_preserves_escaped_symbol_names() {
        let runtime = zkcl::Runtime::start().unwrap();
        runtime
            .load_str(r#"(defun |Foo| () 1) (defun |foo| () 2) (defun foo () 3)"#)
            .unwrap();
        let snapshot = runtime.namespace_snapshot();
        let theme = ReplTheme::new(false);
        let mut suggestions = Vec::new();
        append_runtime_completions(
            "",
            Span { start: 0, end: 0 },
            0,
            &snapshot,
            &mut HashSet::new(),
            &mut suggestions,
            &theme,
        );
        for name in ["|Foo|", "|foo|", "foo"] {
            assert!(
                suggestions
                    .iter()
                    .any(|suggestion| suggestion.value == name),
                "{name}"
            );
        }
        let mut escaped = Vec::new();
        append_runtime_completions(
            "|F",
            Span { start: 0, end: 2 },
            0,
            &snapshot,
            &mut HashSet::new(),
            &mut escaped,
            &theme,
        );
        assert_eq!(
            escaped
                .iter()
                .map(|suggestion| suggestion.value.as_str())
                .collect::<Vec<_>>(),
            ["|Foo|"]
        );
    }

    #[test]
    fn runtime_symbol_spelling_round_trips_escaped_names() {
        for name in ["Foo", "foo", "FOO", "123", "a:b", "a b", "a|b", "a\\b", ""] {
            let spelling = runtime_symbol_spelling(name);
            let parsed = SymbolName::parse(&spelling).unwrap();
            assert_eq!(parsed.prefix, Prefix::None);
            assert_eq!(parsed.name, name);
        }
        assert_eq!(runtime_qualified_prefix("|a:b|"), None);
        assert_eq!(
            runtime_qualified_prefix("|a:b|::|F"),
            Some(("|a:b|:", "|F"))
        );
    }
}
