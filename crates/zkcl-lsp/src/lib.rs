//! Language Server Protocol integration for zkcl source documents.
//!
//! Initialization options and `workspace/didChangeConfiguration` settings
//! accept a `target` triple, a `features` array of feature names, and a
//! `readtable` name (`standard` or `raw-strings`). The
//! settings may be nested under `zkcl`. A feature list, including an empty
//! one, takes precedence over the target's initial reader features. Omitted
//! settings keep their current values and `null` clears a setting. The
//! reader feature set starts unknown. The `lint` settings override the first
//! valid workspace-root lint configuration. A valid open config buffer takes
//! precedence over its file on disk until closed.

mod diagnostics;
mod requests;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, Write},
    ops,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
};

use diagnostics::{
    DiagnosticEvent, DiagnosticJob, DocumentAnalysis, DocumentChange, ProjectMetadataKind,
    SyntaxAnalysis, WorkspaceChanges,
};
use lsp_server::{ErrorCode, Message, Notification, Request, Response};
use lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionProviderCapability, CompletionOptions, Diagnostic,
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, FoldingRangeProviderCapability, HoverProviderCapability,
    InitializeParams, InitializeResult, OneOf, Position, PositionEncodingKind,
    PublishDiagnosticsParams, Range, RenameOptions, SelectionRangeProviderCapability,
    SemanticTokenModifier, SemanticTokenType, SemanticTokensFullOptions, SemanticTokensLegend,
    SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo,
    SignatureHelpOptions, TextDocumentContentChangeEvent, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions, TextDocumentSyncSaveOptions, Uri,
    notification::{
        DidChangeConfiguration, DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument,
        Exit, Notification as _, PublishDiagnostics,
    },
    request::{Initialize, Request as _, Shutdown},
};
use requests::respond;
use serde_json::Value;
use zkcl_syntax::{
    LintConfig, LintLevel, LintPreset, SourcePosition, SourcePositionEncoding, SourceSnapshot,
    SourceWorkspace, all_rules,
    lint::FormLintCache,
    syntax::{SourceReadtable, TextRange, TextSize},
};
use zkcl_target::CompilationTarget;

/// Serve source diagnostics, navigation, formatting, and language intelligence over stdio.
pub fn run_stdio() -> io::Result<()> {
    let (event_sender, event_receiver) = mpsc::sync_channel(16);
    let input_events = event_sender.clone();
    thread::Builder::new()
        .name("zkcl-lsp-input".into())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            loop {
                let message = Message::read(&mut stdin);
                let done = match &message {
                    Ok(Some(Message::Notification(notification))) => {
                        notification.method == Exit::METHOD
                    }
                    Ok(Some(_)) => false,
                    _ => true,
                };
                if input_events.send(ServerEvent::Input(message)).is_err() || done {
                    break;
                }
            }
        })?;

    let (syntax_jobs, syntax_receiver) = mpsc::sync_channel(1);
    let syntax_events = event_sender.clone();
    let syntax_worker = thread::Builder::new()
        .name("zkcl-lsp-syntax".into())
        .spawn(move || diagnostics::run_syntax_worker(syntax_receiver, syntax_events))?;
    let (job_sender, job_receiver) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("zkcl-lsp-diagnostics".into())
        .spawn(move || diagnostics::run_worker(job_receiver, event_sender))?;
    let result = serve(
        &event_receiver,
        &syntax_jobs,
        &job_sender,
        &mut io::stdout().lock(),
    );
    drop(syntax_jobs);
    drop(job_sender);
    drop(event_receiver);
    syntax_worker
        .join()
        .map_err(|_| io::Error::other("LSP syntax worker panicked"))?;
    worker
        .join()
        .map_err(|_| io::Error::other("LSP diagnostics worker panicked"))?;
    result
}

enum ServerEvent {
    Input(io::Result<Option<Message>>),
    Diagnostics(DiagnosticEvent),
}

enum Server {
    AwaitingInitialize,
    Running(Box<Session>),
    ShuttingDown,
}

fn serve(
    events: &Receiver<ServerEvent>,
    syntax_jobs: &SyncSender<SyntaxAnalysis>,
    jobs: &SyncSender<DiagnosticJob>,
    output: &mut impl Write,
) -> io::Result<()> {
    let mut server = Server::AwaitingInitialize;
    let mut input_closed = false;
    loop {
        let event = match events.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => {
                // Apply ready edits before starting analysis so bursts share one job per file.
                let diagnostics_pending = match &mut server {
                    Server::Running(session) => {
                        let syntax_pending = session.start_syntax_diagnostics(syntax_jobs)?;
                        session.start_diagnostics(jobs)? || syntax_pending
                    }
                    _ => false,
                };
                if input_closed && !diagnostics_pending {
                    return Ok(());
                }
                events.recv().map_err(io::Error::other)?
            }
            Err(error) => return Err(io::Error::other(error)),
        };
        let message = match event {
            ServerEvent::Input(message) => match message? {
                Some(message) => message,
                None => {
                    input_closed = true;
                    continue;
                }
            },
            ServerEvent::Diagnostics(event) => {
                if let Server::Running(session) = &mut server
                    && let Some(notification) = session.handle_diagnostic_event(event)
                {
                    Message::Notification(notification).write(output)?;
                }
                continue;
            }
        };
        match message {
            Message::Request(request) => {
                let response = server.request(request);
                Message::Response(response).write(output)?;
            }
            Message::Notification(notification) if notification.method == Exit::METHOD => {
                return match server {
                    Server::ShuttingDown => Ok(()),
                    _ => Err(io::Error::other("LSP client exited before shutdown")),
                };
            }
            Message::Notification(notification) => {
                if let Server::Running(session) = &mut server
                    && let Some(notification) = session.document_notification(notification)
                {
                    Message::Notification(notification).write(output)?;
                }
            }
            Message::Response(_) => {}
        }
    }
}

impl Server {
    fn request(&mut self, request: Request) -> Response {
        match self {
            Self::AwaitingInitialize if request.method == Initialize::METHOD => {
                respond::<Initialize>(request, |params| {
                    let (session, capabilities) = Session::new(params);
                    *self = Self::Running(Box::new(session));
                    InitializeResult {
                        capabilities,
                        server_info: Some(ServerInfo {
                            name: "zkcl".into(),
                            version: Some(env!("CARGO_PKG_VERSION").into()),
                        }),
                    }
                })
            }
            Self::AwaitingInitialize => Response::new_err(
                request.id,
                ErrorCode::ServerNotInitialized as i32,
                "Server is awaiting initialize".into(),
            ),
            Self::Running(_) if request.method == Shutdown::METHOD => {
                *self = Self::ShuttingDown;
                Response::new_ok(request.id, ())
            }
            Self::Running(_) if request.method == Initialize::METHOD => Response::new_err(
                request.id,
                ErrorCode::InvalidRequest as i32,
                "Server is already initialized".into(),
            ),
            Self::Running(session) => session.request(request),
            Self::ShuttingDown => Response::new_err(
                request.id,
                ErrorCode::InvalidRequest as i32,
                "Server has shut down".into(),
            ),
        }
    }
}

struct Session {
    workspace: SourceWorkspace,
    source_settings: SourceSettings,
    lint_config: Arc<LintConfig>,
    lint_settings: LintSettings,
    workspace_roots: Vec<PathBuf>,
    project_sources: HashMap<PathBuf, zkcl_project::sources::ProjectSources>,
    open: HashMap<Uri, OpenDocument>,
    pending_diagnostics: VecDeque<Uri>,
    active_syntax_diagnostics: Option<ActiveDiagnostics>,
    active_diagnostics: Option<ActiveDiagnostics>,
    snapshots_before_changes: HashMap<String, Option<Arc<SourceSnapshot>>>,
    checking_workspace_warning_inputs: bool,
    encoding: SourcePositionEncoding,
    hierarchical_symbols: bool,
}

#[derive(Default)]
struct SourceSettings {
    features: Option<Vec<String>>,
    target: Option<CompilationTarget>,
}

impl SourceSettings {
    fn update(&mut self, settings: &Value, workspace: &mut SourceWorkspace) -> bool {
        let settings = settings.get("zkcl").unwrap_or(settings);
        if let Some(target) = settings.get("target") {
            self.target = target.as_str().and_then(|triple| triple.parse().ok());
        }
        if let Some(features) = settings.get("features") {
            self.features = features.as_array().map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            });
        }
        let readtable_changed = settings.get("readtable").is_some_and(|value| {
            if value.is_null() {
                return workspace.set_readtable(SourceReadtable::Standard);
            }
            value
                .as_str()
                .and_then(SourceReadtable::named)
                .is_some_and(|mode| workspace.set_readtable(mode))
        });
        let features_changed = match &self.features {
            Some(features) => workspace.set_features(Some(features.clone())),
            None => workspace.set_target(self.target.as_ref()),
        };
        readtable_changed || features_changed
    }
}

const LINT_CONFIG_NAMES: [&str; 2] = [".zkcl-lint.lisp", ".zkcl-lint.toml"];

fn parse_lint_config(path: &Path, source: &str) -> Option<LintConfig> {
    match path.file_name()?.to_str()? {
        ".zkcl-lint.lisp" => LintConfig::parse_sexp(source).ok(),
        ".zkcl-lint.toml" => LintConfig::parse_toml(source).ok(),
        _ => None,
    }
}

fn load_workspace_lint_config(
    roots: &[PathBuf],
    open_config: impl Fn(&Path) -> Option<Arc<SourceSnapshot>>,
) -> LintConfig {
    for root in roots {
        for name in LINT_CONFIG_NAMES {
            let path = root.join(name);
            if let Some(config) = open_config(&path)
                .and_then(|snapshot| parse_lint_config(&path, snapshot.text()))
                .or_else(|| parse_lint_config(&path, &std::fs::read_to_string(&path).ok()?))
            {
                return config;
            }
        }
    }
    LintConfig::default()
}

#[derive(Default)]
struct LintSettings {
    preset: Option<LintPreset>,
    max_complexity: Option<usize>,
    max_nesting: Option<usize>,
    rules: std::collections::BTreeMap<String, LintLevel>,
}

impl LintSettings {
    fn update(&mut self, settings: &Value) -> bool {
        let settings = settings.get("zkcl").unwrap_or(settings);
        let Some(lint) = settings.get("lint") else {
            return false;
        };
        if lint.is_null() {
            *self = Self::default();
            return true;
        }
        if let Some(preset) = lint.get("preset") {
            self.preset = preset.as_str().and_then(LintPreset::parse);
        }
        if let Some(max) = lint.get("maxComplexity") {
            self.max_complexity = max.as_u64().and_then(|value| usize::try_from(value).ok());
        }
        if let Some(max) = lint.get("maxNesting") {
            self.max_nesting = max.as_u64().and_then(|value| usize::try_from(value).ok());
        }
        if let Some(rules) = lint.get("rules") {
            if rules.is_null() {
                self.rules.clear();
            } else if let Some(rules) = rules.as_object() {
                for (target, level) in rules {
                    if level.is_null() {
                        self.rules.remove(target);
                    } else if let Some(level) = level.as_str().and_then(LintLevel::parse) {
                        self.rules.insert(target.clone(), level);
                    }
                }
            }
        }
        true
    }

    fn apply(&self, config: &mut LintConfig) {
        if let Some(preset) = self.preset {
            config.preset = preset;
        }
        for (target, &level) in &self.rules {
            let _ = config.set_target_level(target, level);
        }
        if let Some(max) = self.max_complexity {
            config.max_complexity = max;
        }
        if let Some(max) = self.max_nesting {
            config.max_nesting = max;
        }
    }
}

#[derive(Default)]
struct OpenDocument {
    version: i32,
    syntax: Option<Arc<[Diagnostic]>>,
    has_syntax_errors: bool,
    published: Option<(i32, Vec<Diagnostic>)>,
    lint: Option<Vec<zkcl_diagnostics::Diagnostic>>,
    form_lint_cache: Arc<Mutex<FormLintCache>>,
}

impl OpenDocument {
    fn republish_complete_diagnostics(&mut self, uri: &Uri, version: i32) -> Option<Notification> {
        // Structural diagnostics may already have been published while lint is pending.
        self.lint.as_ref()?;
        let (published_version, diagnostics) = self.published.as_mut()?;
        debug_assert_eq!(*published_version, self.version);
        self.version = version;
        *published_version = version;
        Some(publish(uri.clone(), diagnostics.clone(), Some(version)))
    }

    fn publish_diagnostics(
        &mut self,
        active: &ActiveDiagnostics,
        diagnostics: Vec<Diagnostic>,
    ) -> Notification {
        self.published = Some((active.version, diagnostics.clone()));
        publish(active.uri.clone(), diagnostics, Some(active.version))
    }
}

struct ActiveDiagnostics {
    uri: Uri,
    version: i32,
    cancelled: Arc<AtomicBool>,
}

impl Drop for ActiveDiagnostics {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Session {
    fn coordinates<'a>(&self, snapshot: &'a SourceSnapshot) -> LspCoordinates<'a> {
        LspCoordinates {
            snapshot,
            encoding: self.encoding,
        }
    }

    #[expect(
        deprecated,
        reason = "root_uri is fallback when workspace_folders is absent"
    )]
    fn new(params: InitializeParams) -> (Self, ServerCapabilities) {
        let client = params.capabilities;
        let mut root_uris = Vec::new();
        if let Some(folders) = params.workspace_folders {
            for folder in folders {
                root_uris.push(folder.uri);
            }
        } else if let Some(root_uri) = params.root_uri {
            root_uris.push(root_uri);
        }
        let (encoding, protocol_encoding) = client
            .general
            .and_then(|general| general.position_encodings)
            .into_iter()
            .flatten()
            .find_map(|kind| {
                let encoding = match kind.as_str() {
                    "utf-8" => SourcePositionEncoding::Utf8,
                    "utf-16" => SourcePositionEncoding::Utf16,
                    "utf-32" => SourcePositionEncoding::Utf32,
                    _ => return None,
                };
                Some((encoding, kind))
            })
            .unwrap_or((SourcePositionEncoding::Utf16, PositionEncodingKind::UTF16));
        let text_document = client.text_document.unwrap_or_default();
        let hierarchical_symbols = text_document
            .document_symbol
            .and_then(|symbols| symbols.hierarchical_document_symbol_support)
            .unwrap_or(false);
        let prepare_rename = text_document
            .rename
            .and_then(|rename| rename.prepare_support)
            .unwrap_or(false);

        let server = ServerCapabilities {
            position_encoding: Some(protocol_encoding),
            text_document_sync: Some(TextDocumentSyncCapability::Options(
                TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: Some(TextDocumentSyncKind::INCREMENTAL),
                    save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                    ..TextDocumentSyncOptions::default()
                },
            )),
            document_symbol_provider: Some(OneOf::Left(true)),
            workspace_symbol_provider: Some(OneOf::Left(true)),
            definition_provider: Some(OneOf::Left(true)),
            references_provider: Some(OneOf::Left(true)),
            rename_provider: Some(if prepare_rename {
                OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })
            } else {
                OneOf::Left(true)
            }),
            document_formatting_provider: Some(OneOf::Left(true)),
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            signature_help_provider: Some(SignatureHelpOptions {
                trigger_characters: Some(vec!["(".into(), " ".into()]),
                retrigger_characters: Some(vec![" ".into(), "\n".into()]),
                work_done_progress_options: Default::default(),
            }),
            completion_provider: Some(CompletionOptions {
                trigger_characters: Some(vec![":".into(), "*".into(), "&".into(), "-".into()]),
                ..CompletionOptions::default()
            }),
            document_highlight_provider: Some(OneOf::Left(true)),
            folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
            selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
            inlay_hint_provider: Some(OneOf::Left(true)),
            code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
                code_action_kinds: Some(vec![
                    CodeActionKind::QUICKFIX,
                    CodeActionKind::REFACTOR,
                    CodeActionKind::SOURCE_FIX_ALL,
                ]),
                ..CodeActionOptions::default()
            })),
            semantic_tokens_provider: Some(
                SemanticTokensServerCapabilities::SemanticTokensOptions(SemanticTokensOptions {
                    legend: SemanticTokensLegend {
                        token_types: vec![
                            SemanticTokenType::KEYWORD,
                            SemanticTokenType::FUNCTION,
                            SemanticTokenType::MACRO,
                            SemanticTokenType::VARIABLE,
                            SemanticTokenType::PARAMETER,
                            SemanticTokenType::TYPE,
                            SemanticTokenType::STRING,
                            SemanticTokenType::NUMBER,
                            SemanticTokenType::COMMENT,
                            SemanticTokenType::OPERATOR,
                        ],
                        token_modifiers: vec![SemanticTokenModifier::DEFINITION],
                    },
                    full: Some(SemanticTokensFullOptions::Bool(true)),
                    range: None,
                    work_done_progress_options: Default::default(),
                }),
            ),
            ..ServerCapabilities::default()
        };
        let mut workspace = SourceWorkspace::new();
        let mut source_settings = SourceSettings::default();
        let mut workspace_roots = Vec::new();
        for root_uri in &root_uris {
            if let Some(root_path) = uri_to_file_path(root_uri.as_str()) {
                let manifest = root_path.join(zkcl_project::manifest::MANIFEST_NAME);
                if let Ok(project) = zkcl_project::sources::ProjectSources::load(&manifest) {
                    index_project_sources(&mut workspace, &project, |_| false);
                } else if !manifest.exists() {
                    index_workspace_directory(&mut workspace, &root_path, 0);
                }
                workspace_roots.push(root_path);
            }
        }
        let mut lint_config = load_workspace_lint_config(&workspace_roots, |_| None);
        let mut lint_settings = LintSettings::default();
        if let Some(options) = params.initialization_options {
            source_settings.update(&options, &mut workspace);
            lint_settings.update(&options);
            lint_settings.apply(&mut lint_config);
        }
        let session = Self {
            workspace,
            source_settings,
            lint_config: Arc::new(lint_config),
            lint_settings,
            workspace_roots,
            project_sources: HashMap::new(),
            open: HashMap::new(),
            pending_diagnostics: VecDeque::new(),
            active_syntax_diagnostics: None,
            active_diagnostics: None,
            snapshots_before_changes: HashMap::new(),
            checking_workspace_warning_inputs: false,
            encoding,
            hierarchical_symbols,
        };
        (session, server)
    }

    fn document_notification(&mut self, notification: Notification) -> Option<Notification> {
        match notification.method.as_str() {
            DidChangeConfiguration::METHOD => {
                let params: DidChangeConfigurationParams =
                    serde_json::from_value(notification.params).ok()?;
                let source_changed = self
                    .source_settings
                    .update(&params.settings, &mut self.workspace);
                let lint_changed =
                    self.lint_settings.update(&params.settings) && self.reload_lint_config();
                if source_changed || lint_changed {
                    self.requeue_all_diagnostics();
                }
                None
            }
            DidOpenTextDocument::METHOD => {
                let params: DidOpenTextDocumentParams =
                    serde_json::from_value(notification.params).ok()?;
                let document = params.text_document;
                if let Some(path) = uri_to_file_path(document.uri.as_str()) {
                    if let Some(project) = zkcl_project::sources::ProjectSources::discover(&path) {
                        self.project_sources
                            .insert(project.manifest.clone(), project);
                        self.refresh_project_sources();
                    } else if path.parent().is_some_and(|parent| {
                        parent.ancestors().any(|directory| {
                            directory
                                .join(zkcl_project::manifest::MANIFEST_NAME)
                                .exists()
                        })
                    }) {
                        let name = document.uri.as_str().to_owned();
                        self.workspace.set_visible_sources(name.clone(), vec![name]);
                    }
                }
                let lint_config = self.is_workspace_lint_config(&document.uri);
                let before = self.workspace.snapshot(document.uri.as_str());
                self.workspace
                    .set_document(document.uri.as_str(), document.text);
                self.record_document_change(document.uri.as_str(), before);
                self.queue_diagnostics(document.uri, document.version);
                if lint_config && self.reload_lint_config() {
                    self.requeue_all_diagnostics();
                }
                None
            }
            DidChangeTextDocument::METHOD => {
                let params: DidChangeTextDocumentParams =
                    serde_json::from_value(notification.params).ok()?;
                self.change(params).ok().flatten()
            }
            DidCloseTextDocument::METHOD => {
                let params: DidCloseTextDocumentParams =
                    serde_json::from_value(notification.params).ok()?;
                let uri = params.text_document.uri;
                self.open.remove(&uri);
                self.pending_diagnostics.retain(|pending| pending != &uri);
                self.cancel_syntax_diagnostics(&uri);
                let before = self.workspace.snapshot(uri.as_str());
                if let Some(path) = uri_to_file_path(uri.as_str())
                    && let Ok(disk_text) = std::fs::read_to_string(path)
                {
                    self.workspace.set_document(uri.as_str(), disk_text);
                } else {
                    self.workspace.remove_document(uri.as_str());
                }
                self.record_document_change(uri.as_str(), before);
                if self.is_workspace_lint_config(&uri) && self.reload_lint_config() {
                    self.requeue_all_diagnostics();
                } else {
                    self.cancel_and_requeue_diagnostics();
                }
                Some(publish(uri, Vec::new(), None))
            }
            "workspace/didChangeWatchedFiles" | "textDocument/didSave" => {
                self.refresh_project_sources();
                self.requeue_all_diagnostics();
                None
            }
            _ => None,
        }
    }

    fn refresh_project_sources(&mut self) {
        let previous: Vec<_> = self
            .project_sources
            .values()
            .flat_map(|project| project.files.iter())
            .filter_map(|path| file_path_to_uri(path))
            .collect();
        let mut before: HashMap<_, _> = previous
            .iter()
            .map(|name| (name.clone(), self.workspace.snapshot(name)))
            .collect();
        for project in self.project_sources.values_mut() {
            if let Ok(updated) = zkcl_project::sources::ProjectSources::load(&project.manifest) {
                *project = updated;
            } else {
                project.files.clear();
                project.project_files.clear();
            }
            for path in &project.files {
                if let Some(name) = file_path_to_uri(path) {
                    before
                        .entry(name)
                        .or_insert_with_key(|name| self.workspace.snapshot(name));
                }
            }
            index_project_sources(&mut self.workspace, project, |name| {
                name.parse::<Uri>()
                    .is_ok_and(|uri| self.open.contains_key(&uri))
            });
        }
        let current: HashSet<_> = self
            .project_sources
            .values()
            .flat_map(|project| project.files.iter())
            .filter_map(|path| file_path_to_uri(path))
            .collect();
        for name in previous {
            if !current.contains(&name) {
                if name
                    .parse::<Uri>()
                    .ok()
                    .is_some_and(|uri| self.open.contains_key(&uri))
                {
                    self.workspace.set_visible_sources(name.clone(), vec![name]);
                } else {
                    self.workspace.remove_document(&name);
                }
            }
        }
        for (name, snapshot) in before {
            self.record_document_change(&name, snapshot);
        }
    }

    fn change(&mut self, params: DidChangeTextDocumentParams) -> Result<Option<Notification>, ()> {
        let document = params.text_document;
        if document.version <= self.open.get(&document.uri).ok_or(())?.version {
            return Err(());
        }
        let uri = document.uri.as_str();
        let before = self.workspace.snapshot(uri).ok_or(())?;
        self.apply_document_changes(uri, params.content_changes)
            .ok_or(())?;
        let source_changed = self.record_document_change(uri, Some(before));
        let lint_changed =
            self.is_workspace_lint_config(&document.uri) && self.reload_lint_config();
        if !source_changed
            && !lint_changed
            && self.workspace_warnings_current()
            && let Some(diagnostics) = self
                .open
                .get_mut(&document.uri)
                .expect("the edited document is open")
                .republish_complete_diagnostics(&document.uri, document.version)
        {
            return Ok(Some(diagnostics));
        }
        self.queue_diagnostics(document.uri, document.version);
        if lint_changed {
            self.requeue_all_diagnostics();
        }
        Ok(None)
    }

    fn apply_document_changes(
        &mut self,
        uri: &str,
        changes: Vec<TextDocumentContentChangeEvent>,
    ) -> Option<()> {
        if changes.is_empty() {
            return None;
        }
        let edits = changes.iter().map(|change| {
            let range = change
                .range
                .map(|range| source_position(range.start)..source_position(range.end));
            (range, change.text.as_str())
        });
        self.workspace
            .apply_position_edits(uri, self.encoding, edits)
            .ok()
    }

    fn record_document_change(&mut self, name: &str, before: Option<Arc<SourceSnapshot>>) -> bool {
        let after = self.workspace.snapshot(name);
        let changed = match (&before, &after) {
            (Some(before), Some(after)) => !Arc::ptr_eq(before, after),
            (None, None) => false,
            _ => true,
        };
        if changed && self.open.keys().any(|open| open.as_str() != name) {
            self.snapshots_before_changes
                .entry(name.to_owned())
                .or_insert(before);
        }
        changed
    }

    fn workspace_warnings_current(&self) -> bool {
        !self.checking_workspace_warning_inputs && self.snapshots_before_changes.is_empty()
    }

    fn queue_diagnostics(&mut self, uri: Uri, version: i32) {
        self.cancel_syntax_diagnostics(&uri);
        let editing = self.open.contains_key(&uri);
        let open = self.open.entry(uri.clone()).or_default();
        open.version = version;
        open.syntax = None;
        open.lint = None;
        if editing {
            // Prefer an edited buffer over earlier workspace refreshes.
            self.pending_diagnostics.retain(|pending| pending != &uri);
            self.pending_diagnostics.push_front(uri);
        } else {
            self.pending_diagnostics.push_back(uri);
        }
        self.cancel_and_requeue_diagnostics();
    }

    fn cancel_syntax_diagnostics(&self, uri: &Uri) {
        if let Some(active) = &self.active_syntax_diagnostics
            && active.uri == *uri
        {
            active.cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn cancel_and_requeue_diagnostics(&mut self) {
        let Some(active) = &self.active_diagnostics else {
            return;
        };
        // A semantic pass uses declarations from the entire workspace snapshot.
        active.cancelled.store(true, Ordering::Relaxed);
        if self.open.contains_key(&active.uri) && !self.pending_diagnostics.contains(&active.uri) {
            self.pending_diagnostics.push_back(active.uri.clone());
        }
    }

    fn requeue_all_diagnostics(&mut self) {
        self.snapshots_before_changes.clear();
        self.pending_diagnostics.clear();
        self.pending_diagnostics.extend(self.open.keys().cloned());
        for open in self.open.values_mut() {
            open.syntax = None;
            open.lint = None;
        }
        if let Some(active) = &self.active_syntax_diagnostics {
            active.cancelled.store(true, Ordering::Relaxed);
        }
        self.cancel_and_requeue_diagnostics();
    }

    fn is_workspace_lint_config(&self, uri: &Uri) -> bool {
        uri_to_file_path(uri.as_str()).is_some_and(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| LINT_CONFIG_NAMES.contains(&name))
                && path
                    .parent()
                    .is_some_and(|parent| self.workspace_roots.iter().any(|root| root == parent))
        })
    }

    fn project_metadata_kind(&self, uri: &Uri) -> Option<ProjectMetadataKind> {
        let path = uri_to_file_path(uri.as_str())?;
        match path.file_name()?.to_str()? {
            zkcl_project::manifest::MANIFEST_NAME => Some(ProjectMetadataKind::Manifest),
            zkcl_project::lockfile::LOCKFILE_NAME => Some(ProjectMetadataKind::Lockfile),
            _ => None,
        }
    }

    fn reload_lint_config(&mut self) -> bool {
        let mut next = load_workspace_lint_config(&self.workspace_roots, |path| {
            let uri = self
                .open
                .keys()
                .find(|uri| uri_to_file_path(uri.as_str()).as_deref() == Some(path))?;
            self.workspace.snapshot(uri.as_str())
        });
        self.lint_settings.apply(&mut next);
        if next == *self.lint_config {
            return false;
        }
        self.lint_config = Arc::new(next);
        true
    }

    fn start_syntax_diagnostics(&mut self, jobs: &SyncSender<SyntaxAnalysis>) -> io::Result<bool> {
        if self.active_syntax_diagnostics.is_some() {
            return Ok(true);
        }
        let Some(uri) = self
            .pending_diagnostics
            .iter()
            .find(|uri| self.open[*uri].syntax.is_none())
        else {
            return Ok(false);
        };
        let snapshot = self
            .workspace
            .snapshot(uri.as_str())
            .expect("an open document is in the source workspace");
        let cancelled = Arc::new(AtomicBool::new(false));
        let job = SyntaxAnalysis {
            project_metadata: self.project_metadata_kind(uri),
            snapshot,
            encoding: self.encoding,
            cancelled: cancelled.clone(),
        };
        self.active_syntax_diagnostics = Some(ActiveDiagnostics {
            uri: uri.clone(),
            version: self.open[uri].version,
            cancelled,
        });
        jobs.send(job)
            .map_err(|_| io::Error::other("LSP syntax worker stopped"))?;
        Ok(true)
    }

    fn start_diagnostics(&mut self, jobs: &SyncSender<DiagnosticJob>) -> io::Result<bool> {
        if self.active_diagnostics.is_some() || self.checking_workspace_warning_inputs {
            return Ok(true);
        }
        if self.open.is_empty() {
            self.snapshots_before_changes.clear();
            return Ok(false);
        }
        if self.pending_diagnostics.is_empty() && self.snapshots_before_changes.is_empty() {
            return Ok(false);
        }
        // Publish the current structural result before checking workspace lint.
        if let Some(uri) = self.pending_diagnostics.front()
            && self.open[uri].syntax.is_none()
        {
            return Ok(false);
        }
        let lint_enabled = all_rules()
            .iter()
            .any(|rule| self.lint_config.level_for(rule) != LintLevel::Allow);
        // Capture the changes visible to this job. The worker compares them
        // before it computes semantic warnings from the new context.
        let candidates: Vec<_> = if !lint_enabled || self.snapshots_before_changes.is_empty() {
            Vec::new()
        } else {
            self.open
                .iter()
                .filter_map(|(uri, open)| {
                    // Pending diagnostics will already use the new workspace.
                    open.lint.as_ref()?;
                    self.workspace
                        .snapshot(uri.as_str())
                        .map(|source| (uri.clone(), source))
                })
                .collect()
        };
        let edits = if candidates.is_empty() {
            self.snapshots_before_changes.clear();
            Vec::new()
        } else {
            self.snapshots_before_changes
                .drain()
                .map(|(name, before)| DocumentChange {
                    before,
                    after: self.workspace.snapshot(&name),
                })
                .collect()
        };
        let changes = WorkspaceChanges { edits, candidates };
        self.checking_workspace_warning_inputs = !changes.edits.is_empty();
        let job = if let Some(uri) = self.pending_diagnostics.pop_front() {
            let open = &self.open[&uri];
            let cancelled = Arc::new(AtomicBool::new(false));
            let lint_document = lint_enabled
                && !self.is_workspace_lint_config(&uri)
                && self.project_metadata_kind(&uri).is_none();
            let snapshot = lint_document.then(|| {
                self.workspace
                    .diagnostic_snapshot(uri.as_str())
                    .expect("an open document is in the source workspace")
            });
            let job = DiagnosticJob::Analyze(DocumentAnalysis {
                uri: uri.clone(),
                snapshot,
                syntax: open.syntax.clone().expect("structural diagnostics ready"),
                lint_config: self.lint_config.clone(),
                form_lint_cache: open.form_lint_cache.clone(),
                document_changes: changes,
                encoding: self.encoding,
                cancelled: cancelled.clone(),
            });
            self.active_diagnostics = Some(ActiveDiagnostics {
                uri,
                version: open.version,
                cancelled,
            });
            job
        } else if !changes.edits.is_empty() {
            DiagnosticJob::CheckWorkspaceWarnings(changes)
        } else {
            return Ok(false);
        };
        jobs.send(job)
            .map_err(|_| io::Error::other("LSP diagnostics worker stopped"))?;
        Ok(true)
    }

    fn handle_diagnostic_event(&mut self, event: DiagnosticEvent) -> Option<Notification> {
        match event {
            DiagnosticEvent::SyntaxDiagnostics(diagnostics) => {
                let active = self.active_syntax_diagnostics.take()?;
                if active.cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                let open = self.open.get_mut(&active.uri).unwrap();
                let diagnostics = diagnostics?;
                let publish = (open.has_syntax_errors || !diagnostics.is_empty())
                    && open
                        .published
                        .as_ref()
                        .is_none_or(|(version, _)| *version != active.version);
                open.has_syntax_errors = !diagnostics.is_empty();
                let notification =
                    publish.then(|| open.publish_diagnostics(&active, diagnostics.clone()));
                open.syntax = Some(diagnostics.into());
                notification
            }
            DiagnosticEvent::DocumentAnalyzed(diagnostics) => {
                let active = self.active_diagnostics.take()?;
                if active.cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                let open = self.open.get_mut(&active.uri).unwrap();
                let result = diagnostics?;
                open.lint = Some(result.lint);
                let diagnostics = result.diagnostics;
                if open.published.as_ref().is_some_and(|(version, published)| {
                    *version == active.version && published == &diagnostics
                }) {
                    return None;
                }
                Some(open.publish_diagnostics(&active, diagnostics))
            }
            DiagnosticEvent::WorkspaceWarningsChecked(affected) => {
                self.checking_workspace_warning_inputs = false;
                for uri in affected {
                    let Some(open) = self.open.get_mut(&uri) else {
                        continue;
                    };
                    open.lint = None;
                    if self
                        .active_diagnostics
                        .as_ref()
                        .is_none_or(|active| active.uri != uri)
                        && !self.pending_diagnostics.contains(&uri)
                    {
                        self.pending_diagnostics.push_back(uri);
                    }
                }
                None
            }
        }
    }
}

fn publish(uri: Uri, diagnostics: Vec<Diagnostic>, version: Option<i32>) -> Notification {
    Notification::new(
        PublishDiagnostics::METHOD.into(),
        PublishDiagnosticsParams::new(uri, diagnostics, version),
    )
}

struct LspCoordinates<'a> {
    snapshot: &'a SourceSnapshot,
    encoding: SourcePositionEncoding,
}

impl LspCoordinates<'_> {
    fn position(&self, offset: TextSize) -> Position {
        let position = self
            .snapshot
            .position_with_encoding(offset.into(), self.encoding)
            .expect("syntax ranges use source character boundaries");
        // Both coordinates are bounded by the u32 byte offset.
        Position::new(position.line as u32, position.column as u32)
    }

    fn range(&self, range: TextRange) -> Range {
        Range::new(self.position(range.start()), self.position(range.end()))
    }

    fn byte_offset(&self, position: Position) -> Option<usize> {
        self.snapshot
            .clamped_byte_offset_with_encoding(source_position(position), self.encoding)
    }

    fn byte_range(&self, range: Range) -> Option<ops::Range<usize>> {
        let start = self.byte_offset(range.start)?;
        let end = self.byte_offset(range.end)?;
        (start <= end).then_some(start..end)
    }

    fn text_range(&self, range: Range) -> Option<TextRange> {
        let bytes = self.byte_range(range)?;
        Some(TextRange::new(
            bytes.start.try_into().ok()?,
            bytes.end.try_into().ok()?,
        ))
    }
}

fn source_position(position: Position) -> SourcePosition {
    SourcePosition {
        line: position.line as usize,
        column: position.character as usize,
    }
}

fn uri_to_file_path(uri: &str) -> Option<std::path::PathBuf> {
    if uri.len() < 7 || !uri[..7].eq_ignore_ascii_case("file://") {
        return None;
    }
    let rest = &uri[7..];
    let raw_path = if rest.starts_with('/') {
        rest
    } else {
        let slash = rest.find('/')?;
        let authority = &rest[..slash];
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return None;
        }
        &rest[slash..]
    };
    let raw_path = raw_path.split(['?', '#']).next()?;
    let decoded = percent_decode_utf8(raw_path)?;
    #[cfg(windows)]
    let decoded = {
        let bytes = decoded.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            &decoded[1..]
        } else {
            decoded.as_str()
        }
    };
    Some(std::path::PathBuf::from(decoded))
}

fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_value(*bytes.get(i + 1)?)?;
            let lo = hex_value(*bytes.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn file_path_to_uri(path: &std::path::Path) -> Option<String> {
    use std::fmt::Write as _;
    let s = path.to_str()?;
    let normalized;
    let raw = if s.starts_with('/') {
        s
    } else {
        normalized = format!("/{}", s.replace('\\', "/"));
        &normalized
    };
    let mut out = String::with_capacity("file://".len() + raw.len());
    out.push_str("file://");
    for &byte in raw.as_bytes() {
        if matches!(
            byte,
            b'/' | b':' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~'
        ) {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    Some(out)
}

fn index_project_sources(
    workspace: &mut SourceWorkspace,
    project: &zkcl_project::sources::ProjectSources,
    is_open: impl Fn(&str) -> bool,
) {
    let sources: Vec<_> = project
        .files
        .iter()
        .filter_map(|path| file_path_to_uri(path))
        .collect();
    for source in &sources {
        if is_open(source) {
            continue;
        }
        if let Some(path) = uri_to_file_path(source)
            && let Ok(text) = std::fs::read_to_string(path)
        {
            workspace.set_document(source.clone(), text);
        }
    }
    // Dependencies keep their own visibility when opened as project roots.
    let visible: Arc<[String]> = sources.into();
    for path in &project.project_files {
        if let Some(source) = file_path_to_uri(path) {
            workspace.set_visible_sources(source, visible.clone());
        }
    }
}

fn index_workspace_directory(workspace: &mut SourceWorkspace, dir: &std::path::Path, depth: usize) {
    if depth > 8 {
        return;
    }
    let manifest = dir.join(zkcl_project::manifest::MANIFEST_NAME);
    if manifest.exists() {
        if let Ok(project) = zkcl_project::sources::ProjectSources::load(&manifest) {
            index_project_sources(workspace, &project, |_| false);
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> =
        entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            index_workspace_directory(workspace, &path, depth + 1);
        } else if path.is_file()
            && let Some(ext) = path.extension().and_then(|e| e.to_str())
            && matches!(ext, "lisp" | "lsp" | "cl" | "asd")
            && let Some(uri) = file_path_to_uri(&path)
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            workspace.set_document(uri, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::request::DocumentSymbolRequest;
    use std::path::Path;

    #[test]
    fn local_file_uris_decode_percent_escaped_utf8() {
        assert_eq!(
            uri_to_file_path("file:///tmp/my%20dir/%E9%9B%AA+1%25.lisp"),
            Some(std::path::PathBuf::from("/tmp/my dir/雪+1%.lisp"))
        );
        assert_eq!(
            uri_to_file_path("FILE://LocalHost/tmp/a%20b.lisp#frag"),
            Some(std::path::PathBuf::from("/tmp/a b.lisp"))
        );
    }

    #[test]
    fn file_uri_decoder_rejects_nonlocal_or_malformed_paths() {
        assert_eq!(uri_to_file_path("file://remote/tmp/a.lisp"), None);
        assert_eq!(uri_to_file_path("file:///tmp/bad%ZZ.lisp"), None);
        assert_eq!(uri_to_file_path("file:///tmp/%FF.lisp"), None);
        assert_eq!(uri_to_file_path("untitled:scratch.lisp"), None);
    }

    #[test]
    fn file_uri_round_trip_preserves_escaped_path_characters() {
        let path = Path::new("/tmp/space dir/雪%file.lisp");
        let uri = file_path_to_uri(path).unwrap();
        assert_eq!(uri, "file:///tmp/space%20dir/%E9%9B%AA%25file.lisp");
        assert_eq!(uri_to_file_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn did_close_without_a_version_discards_the_snapshot() {
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "processId": null,
            "capabilities": {}
        }))
        .unwrap();
        let (mut session, _) = Session::new(params);
        let uri_str = "untitled:no-version.lisp";
        let uri: Uri = uri_str.parse().unwrap();
        session.workspace.set_document(uri_str, "(defun f () 1)");
        assert!(session.workspace.snapshot(uri_str).is_some());
        assert!(!session.open.contains_key(&uri));

        let notif = session
            .document_notification(Notification::new(
                DidCloseTextDocument::METHOD.into(),
                serde_json::json!({ "textDocument": { "uri": uri_str } }),
            ))
            .expect("didClose publishes empty diagnostics even without version");
        assert_eq!(notif.method, PublishDiagnostics::METHOD);
        assert!(session.workspace.snapshot(uri_str).is_none());
    }

    #[test]
    fn reopening_document_discards_cancelled_diagnostics() {
        let params = serde_json::from_value(serde_json::json!({
            "capabilities": { "textDocument": { "publishDiagnostics": { "versionSupport": true } } }
        }))
        .unwrap();
        let (mut session, _) = Session::new(params);
        let uri: Uri = "untitled:diagnostic-jobs.lisp".parse().unwrap();
        let (sender, jobs) = mpsc::sync_channel(1);
        let (syntax_sender, syntax_jobs) = mpsc::sync_channel(1);

        for version in 1..=5 {
            session
                .workspace
                .set_document(uri.as_str(), format!("({version})"));
            session.queue_diagnostics(uri.clone(), version);
        }
        assert_eq!(session.pending_diagnostics.len(), 1);
        assert!(run_syntax(&mut session).is_none());
        assert!(session.start_diagnostics(&sender).unwrap());
        let DiagnosticJob::Analyze(old_job) = jobs.try_recv().unwrap() else {
            panic!("expected a document analysis job");
        };
        assert_eq!(old_job.snapshot.as_ref().unwrap().source().text(), "(5)");
        assert!(!old_job.cancelled.load(Ordering::Relaxed));
        session.workspace.set_document(uri.as_str(), ")");
        session.queue_diagnostics(uri.clone(), 6);
        assert!(session.start_syntax_diagnostics(&syntax_sender).unwrap());
        let old_syntax = syntax_jobs.try_recv().unwrap();
        assert_eq!(old_syntax.snapshot.text(), ")");

        let closed = session
            .document_notification(Notification::new(
                DidCloseTextDocument::METHOD.into(),
                serde_json::json!({ "textDocument": { "uri": &uri } }),
            ))
            .unwrap()
            .extract::<PublishDiagnosticsParams>(PublishDiagnostics::METHOD)
            .unwrap();
        assert!(closed.diagnostics.is_empty());
        assert_eq!(closed.version, None);
        assert!(old_job.cancelled.load(Ordering::Relaxed));
        assert!(old_syntax.cancelled.load(Ordering::Relaxed));

        session.workspace.set_document(uri.as_str(), "))");
        session.queue_diagnostics(uri.clone(), 6);
        assert!(
            session
                .handle_diagnostic_event(DiagnosticEvent::SyntaxDiagnostics(Some(vec![
                    Diagnostic::default(),
                ])))
                .is_none()
        );
        assert!(
            session
                .handle_diagnostic_event(DiagnosticEvent::DocumentAnalyzed(Some(
                    diagnostics::AnalyzedDocument::default()
                )))
                .is_none()
        );

        let published = run_syntax(&mut session)
            .unwrap()
            .extract::<PublishDiagnosticsParams>(PublishDiagnostics::METHOD)
            .unwrap();
        assert_eq!(published.uri, uri);
        assert_eq!(published.version, Some(6));
        assert_eq!(published.diagnostics.len(), 2);
        assert!(session.start_diagnostics(&sender).unwrap());
        let DiagnosticJob::Analyze(reopened_job) = jobs.try_recv().unwrap() else {
            panic!("expected a document analysis job");
        };
        assert_eq!(reopened_job.snapshot.unwrap().source().text(), "))");
        assert!(!reopened_job.cancelled.load(Ordering::Relaxed));
        assert!(
            session
                .handle_diagnostic_event(DiagnosticEvent::DocumentAnalyzed(None))
                .is_none()
        );
        assert!(!session.start_diagnostics(&sender).unwrap());
    }

    fn run_syntax(session: &mut Session) -> Option<Notification> {
        let (job_sender, job_receiver) = mpsc::sync_channel(1);
        let (event_sender, receiver) = mpsc::sync_channel(1);
        assert!(session.start_syntax_diagnostics(&job_sender).unwrap());
        drop(job_sender);
        diagnostics::run_syntax_worker(job_receiver, event_sender);
        let ServerEvent::Diagnostics(event @ DiagnosticEvent::SyntaxDiagnostics(_)) =
            receiver.try_recv().unwrap()
        else {
            panic!("expected structural diagnostics");
        };
        session.handle_diagnostic_event(event)
    }

    #[test]
    fn unchanged_document_republishes_complete_diagnostics_only_with_current_workspace() {
        let params = serde_json::from_value(serde_json::json!({
            "capabilities": {}
        }))
        .unwrap();
        let (mut session, _) = Session::new(params);
        let source: Uri = "untitled:source.lisp".parse().unwrap();
        let dependency: Uri = "untitled:dependency.lisp".parse().unwrap();
        let open = |uri: &Uri, text: &str| {
            Notification::new(
                DidOpenTextDocument::METHOD.into(),
                serde_json::json!({
                    "textDocument": {
                        "uri": uri, "languageId": "common-lisp", "version": 1, "text": text
                    }
                }),
            )
        };
        let no_op = |version: i32| {
            Notification::new(
                DidChangeTextDocument::METHOD.into(),
                serde_json::json!({
                    "textDocument": { "uri": source, "version": version },
                    "contentChanges": [{
                        "range": {
                            "start": { "line": 1, "character": 7 },
                            "end": { "line": 1, "character": 9 }
                        },
                        "text": "😀"
                    }]
                }),
            )
        };
        let run_lint = |session: &mut Session| {
            let (sender, jobs) = mpsc::sync_channel(1);
            let (events, receiver) = mpsc::sync_channel(2);
            assert!(session.start_diagnostics(&sender).unwrap());
            drop(sender);
            diagnostics::run_worker(jobs, events);
            let ServerEvent::Diagnostics(event @ DiagnosticEvent::DocumentAnalyzed(_)) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected only a document analysis result");
            };
            session
                .handle_diagnostic_event(event)
                .unwrap()
                .extract::<PublishDiagnosticsParams>(PublishDiagnostics::METHOD)
                .unwrap()
        };

        let original_dependency = "(in-package :cl-user)\n(defun dep (x) x)";
        session
            .workspace
            .set_document(dependency.as_str(), original_dependency);
        session.document_notification(open(&source, "(in-package :cl-user)\n(list \"😀\")\n(dep)"));
        assert!(run_syntax(&mut session).is_none());
        let first = run_lint(&mut session);
        assert_eq!(first.diagnostics.len(), 1);
        assert!(first.diagnostics[0].related_information.is_some());

        session.document_notification(open(&dependency, original_dependency));
        assert!(run_syntax(&mut session).is_none());
        let (sender, jobs) = mpsc::sync_channel(1);
        assert!(session.start_diagnostics(&sender).unwrap());
        let DiagnosticJob::Analyze(active) = jobs.try_recv().unwrap() else {
            panic!("expected a pending lint job for the dependency");
        };
        let repeated = session
            .document_notification(no_op(2))
            .unwrap()
            .extract::<PublishDiagnosticsParams>(PublishDiagnostics::METHOD)
            .unwrap();
        assert_eq!(repeated.uri, source);
        assert_eq!(repeated.version, Some(2));
        assert_eq!(repeated.diagnostics, first.diagnostics);
        assert!(session.open[&source].lint.is_some());
        assert!(session.pending_diagnostics.is_empty());
        assert!(!active.cancelled.load(Ordering::Relaxed));
        assert!(session.document_notification(no_op(2)).is_none());

        session.document_notification(Notification::new(
            DidChangeTextDocument::METHOD.into(),
            serde_json::json!({
                "textDocument": { "uri": dependency, "version": 2 },
                "contentChanges": [{ "text": "(in-package :cl-user)\n(defun dep () nil)" }]
            }),
        ));
        assert!(!session.workspace_warnings_current());
        assert!(session.document_notification(no_op(3)).is_none());
        assert!(session.open[&source].lint.is_none());
        assert!(active.cancelled.load(Ordering::Relaxed));
        assert!(
            session
                .handle_diagnostic_event(DiagnosticEvent::DocumentAnalyzed(None))
                .is_none()
        );
        assert!(run_syntax(&mut session).is_none());
        let updated = run_lint(&mut session);
        assert_eq!(updated.version, Some(3));
        assert!(updated.diagnostics.is_empty());
    }

    #[test]
    fn latest_structural_errors_publish_while_an_older_workspace_job_is_pending() {
        let params = serde_json::from_value(serde_json::json!({
            "capabilities": { "textDocument": { "publishDiagnostics": { "versionSupport": true } } }
        }))
        .unwrap();
        let (mut session, _) = Session::new(params);
        let source: Uri = "untitled:source.lisp".parse().unwrap();
        let dependency: Uri = "untitled:dependency.lisp".parse().unwrap();
        let edited: Uri = "untitled:edited.lisp".parse().unwrap();
        let checked: Uri = "untitled:checked.lisp".parse().unwrap();
        let (sender, jobs) = mpsc::sync_channel(1);

        session.workspace.set_document(checked.as_str(), "(dep 1)");
        session.open.insert(
            checked.clone(),
            OpenDocument {
                lint: Some(Vec::new()),
                ..OpenDocument::default()
            },
        );
        session.workspace.set_document(source.as_str(), "(dep 1)");
        session.queue_diagnostics(source.clone(), 1);
        session
            .workspace
            .set_document(dependency.as_str(), "(defun dep () nil)");
        session.record_document_change(dependency.as_str(), None);
        session.queue_diagnostics(dependency.clone(), 1);
        assert!(run_syntax(&mut session).is_none());
        session.start_diagnostics(&sender).unwrap();
        let DiagnosticJob::Analyze(old_job) = jobs.try_recv().unwrap() else {
            panic!("expected a document analysis job");
        };
        assert!(!old_job.document_changes.edits.is_empty());
        let [(candidate, _)] = old_job.document_changes.candidates.as_slice() else {
            panic!("expected only the document with cached lint as a candidate");
        };
        assert_eq!(candidate, &checked);
        for version in 2..=6 {
            session.workspace.set_document(edited.as_str(), ")");
            session.queue_diagnostics(edited.clone(), version);
        }
        let published = run_syntax(&mut session)
            .unwrap()
            .extract::<PublishDiagnosticsParams>(PublishDiagnostics::METHOD)
            .unwrap();
        assert_eq!(published.uri, edited);
        assert_eq!(published.version, Some(6));
        assert_eq!(
            published.diagnostics[0].message,
            "unexpected closing parenthesis"
        );
        assert!(session.start_diagnostics(&sender).unwrap());
        assert!(matches!(jobs.try_recv(), Err(TryRecvError::Empty)));

        assert!(old_job.cancelled.load(Ordering::Relaxed));
        assert!(
            session
                .handle_diagnostic_event(DiagnosticEvent::DocumentAnalyzed(Some(
                    diagnostics::AnalyzedDocument::default()
                )))
                .is_none()
        );
        assert_eq!(
            session.pending_diagnostics,
            VecDeque::from([edited, dependency, source])
        );
    }

    #[test]
    fn document_edits_publish_syntax_errors_before_checking_workspace_warnings() {
        let params = serde_json::from_value(serde_json::json!({ "capabilities": {} })).unwrap();
        let (mut session, _) = Session::new(params);
        let source: Uri = "untitled:source.lisp".parse().unwrap();
        let dependency: Uri = "untitled:dependency.lisp".parse().unwrap();
        session
            .workspace
            .set_document(source.as_str(), "(dependency)");
        session.open.insert(
            source.clone(),
            OpenDocument {
                lint: Some(Vec::new()),
                ..OpenDocument::default()
            },
        );
        let unrelated: Uri = "untitled:unrelated.lisp".parse().unwrap();
        session
            .workspace
            .set_document(unrelated.as_str(), "(print :unrelated)");
        session.open.insert(
            unrelated,
            OpenDocument {
                lint: Some(Vec::new()),
                ..OpenDocument::default()
            },
        );
        session.document_notification(Notification::new(
            DidOpenTextDocument::METHOD.into(),
            serde_json::json!({
                "textDocument": {
                    "uri": dependency,
                    "languageId": "common-lisp",
                    "version": 1,
                    "text": "(in-package :cl-user)\n(defun dependency () nil)\n)"
                }
            }),
        ));

        let (job_sender, job_receiver) = mpsc::sync_channel(1);
        assert!(!session.start_diagnostics(&job_sender).unwrap());
        let published = run_syntax(&mut session)
            .unwrap()
            .extract::<PublishDiagnosticsParams>(PublishDiagnostics::METHOD)
            .unwrap();
        assert_eq!(published.uri, dependency);
        assert_eq!(published.diagnostics.len(), 1);
        assert_eq!(
            published.diagnostics[0].message,
            "unexpected closing parenthesis"
        );

        let (event_sender, receiver) = mpsc::sync_channel(2);
        assert!(session.start_diagnostics(&job_sender).unwrap());
        drop(job_sender);
        diagnostics::run_worker(job_receiver, event_sender);
        let ServerEvent::Diagnostics(event @ DiagnosticEvent::WorkspaceWarningsChecked(_)) =
            receiver.try_recv().unwrap()
        else {
            panic!("expected workspace comparison before semantic diagnostics");
        };
        assert!(session.handle_diagnostic_event(event).is_none());
        assert_eq!(session.pending_diagnostics, VecDeque::from([source]));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            ServerEvent::Diagnostics(DiagnosticEvent::DocumentAnalyzed(_))
        ));
    }

    #[test]
    fn incremental_change_commits_text_and_version_atomically() {
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "processId": null,
            "initializationOptions": { "features": ["ENABLE-FN"] },
            "capabilities": {
                "textDocument": {
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
                }
            }
        }))
        .unwrap();
        let (mut session, _) = Session::new(params);
        let uri_str = "untitled:inc.lisp";
        let uri: Uri = uri_str.parse().unwrap();

        session.document_notification(Notification::new(
            DidOpenTextDocument::METHOD.into(),
            serde_json::json!({
                "textDocument": {
                    "uri": uri_str,
                    "languageId": "common-lisp",
                    "version": 1,
                    "text": "#+enable-fn (defun alpha () 1)\n#+disabled-fn (defun beta () 2)\n"
                }
            }),
        ));

        // Single incremental range edit routes through SourceWorkspace::apply_edit.
        session.change(
            serde_json::from_value(
                serde_json::json!({
                    "textDocument": { "uri": uri_str, "version": 2 },
                    "contentChanges": [{
                        "range": { "start": { "line": 0, "character": 19 }, "end": { "line": 0, "character": 24 } },
                        "text": "gamma"
                    }]
                }),
            )
            .unwrap(),
        )
        .expect("valid incremental change queues diagnostics");
        assert_eq!(
            session.workspace.snapshot(uri_str).unwrap().text(),
            "#+enable-fn (defun gamma () 1)\n#+disabled-fn (defun beta () 2)\n"
        );
        assert_eq!(session.open[&uri].version, 2);

        // DocumentSymbol reflects the edited name and initialization_options.features.
        let sym_resp = session.request(Request::new(
            1.into(),
            DocumentSymbolRequest::METHOD.into(),
            serde_json::json!({ "textDocument": { "uri": uri_str } }),
        ));
        let syms = sym_resp.response_result.unwrap();
        let syms = syms.as_array().unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0]["name"], "gamma");

        // Invalid batch must remain atomic (no partial edit committed).
        assert!(
            session.change(serde_json::from_value(serde_json::json!({
                    "textDocument": { "uri": uri_str, "version": 3 },
                    "contentChanges": [
                        {
                            "range": { "start": { "line": 0, "character": 19 }, "end": { "line": 0, "character": 24 } },
                            "text": "partial"
                        },
                        {
                            "range": { "start": { "line": 99, "character": 0 }, "end": { "line": 99, "character": 0 } },
                            "text": "bad"
                        }
                    ]
                })).unwrap())
            .is_err()
        );
        assert_eq!(
            session.workspace.snapshot(uri_str).unwrap().text(),
            "#+enable-fn (defun gamma () 1)\n#+disabled-fn (defun beta () 2)\n"
        );
        assert_eq!(session.open[&uri].version, 2);

        // A version advances even when the final text is byte-identical.
        let before = session.workspace.snapshot(uri_str).unwrap();
        assert!(session.change(serde_json::from_value(serde_json::json!({
                "textDocument": { "uri": uri_str, "version": 3 },
                "contentChanges": [
                    {
                        "range": { "start": { "line": 0, "character": 19 }, "end": { "line": 0, "character": 24 } },
                        "text": "temporary"
                    },
                    {
                        "range": { "start": { "line": 0, "character": 19 }, "end": { "line": 0, "character": 28 } },
                        "text": "gamma"
                    }
                ]
            })).unwrap()).is_ok());
        assert_eq!(session.open[&uri].version, 3);
        assert!(Arc::ptr_eq(
            &before,
            &session.workspace.snapshot(uri_str).unwrap()
        ));
    }

    #[test]
    fn raw_string_semantic_tokens_cover_each_source_line() {
        let params = serde_json::from_value(serde_json::json!({
            "capabilities": {},
            "initializationOptions": { "readtable": "raw-strings" }
        }))
        .unwrap();
        let (mut session, _) = Session::new(params);
        let uri = "untitled:raw.lisp";
        session.workspace.set_document(uri, "#>😀\r\n  #>雪\r\n");
        let response = session.request(Request::new(
            1.into(),
            "textDocument/semanticTokens/full".to_owned(),
            serde_json::json!({ "textDocument": { "uri": uri } }),
        ));
        let result = response.response_result.unwrap();
        let string = zkcl_syntax::SemanticTokenKind::String as u32;
        assert_eq!(
            result["data"],
            serde_json::json!([0, 0, 4, string, 0, 1, 0, 5, string, 0])
        );
    }

    struct TempProjectDir(PathBuf);

    impl TempProjectDir {
        fn new() -> Self {
            let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .unwrap();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = workspace.join(format!(
                ".agents/tmp/zkcl/source_layout/{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path.canonicalize().unwrap()
        }

        fn open(session: &mut Session, path: &Path, text: &str) {
            session.document_notification(Notification::new(
                DidOpenTextDocument::METHOD.into(),
                serde_json::json!({ "textDocument": {
                    "uri": file_path_to_uri(path).unwrap(), "languageId": "common-lisp",
                    "version": 1, "text": text
                }}),
            ));
        }
    }

    impl Drop for TempProjectDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn project_layout_indexes_unopened_sources_and_preserves_editor_ownership() {
        let project_dir = TempProjectDir::new();
        project_dir.write("app/zkcl.sexp", "(project \"acme/app\" (version \"1.0.0\") (library (files \"package.lisp\" \"use.lisp\")) (dependency \"dep\" (module \"acme/dep\") (path \"../dep\")))");
        project_dir.write(
            "dep/zkcl.sexp",
            "(project \"acme/dep\" (version \"1.0.0\") (library (files \"dep.lisp\")))",
        );
        let package = project_dir.write(
            "app/package.lisp",
            "(in-package :cl-user)\n(defun sibling () :disk)",
        );
        let dependency = project_dir.write(
            "dep/dep.lisp",
            "(in-package :cl-user)\n(defun dependency () nil)",
        );
        let use_text = "(in-package :cl-user)\n(sibling)\n(dependency)";
        let source = project_dir.write("app/use.lisp", use_text);
        project_dir.write(
            "app/undeclared.lisp",
            "(in-package :cl-user)\n(defun intruder () nil)",
        );
        let (mut session, _) = Session::new(
            serde_json::from_value(serde_json::json!({ "capabilities": {} })).unwrap(),
        );
        TempProjectDir::open(&mut session, &source, use_text);
        let uri = file_path_to_uri(&source).unwrap();
        let sibling = session
            .workspace
            .definition(&uri, use_text.find("sibling").unwrap())
            .unwrap();
        assert_eq!(sibling.document, file_path_to_uri(&package).unwrap());
        assert_eq!(usize::from(sibling.selection_range.start()), 29);
        assert_eq!(usize::from(sibling.selection_range.end()), 36);
        let dep = session
            .workspace
            .definition(&uri, use_text.find("dependency").unwrap())
            .unwrap();
        assert_eq!(dep.document, file_path_to_uri(&dependency).unwrap());
        assert!(
            session
                .workspace
                .snapshot(&file_path_to_uri(&project_dir.0.join("app/undeclared.lisp")).unwrap())
                .is_none()
        );

        let unsaved = "(in-package :cl-user)\n(defun edited () :buffer)";
        TempProjectDir::open(&mut session, &package, unsaved);
        session.document_notification(Notification::new(
            "textDocument/didSave".into(),
            serde_json::json!({ "textDocument": { "uri": uri }}),
        ));
        let package_uri = file_path_to_uri(&package).unwrap();
        assert_eq!(
            session.workspace.snapshot(&package_uri).unwrap().text(),
            unsaved
        );
        assert!(
            session
                .workspace
                .definition(&uri, use_text.find("sibling").unwrap())
                .is_none()
        );
        session.document_notification(Notification::new(
            DidCloseTextDocument::METHOD.into(),
            serde_json::json!({ "textDocument": { "uri": package_uri }}),
        ));
        assert!(
            session
                .workspace
                .definition(&uri, use_text.find("sibling").unwrap())
                .is_some()
        );

        project_dir.write(
            "app/zkcl.sexp",
            "(project \"acme/app\" (version \"1.0.0\") (library (files \"use.lisp\")))",
        );
        session.document_notification(Notification::new(
            "workspace/didChangeWatchedFiles".into(),
            serde_json::json!({ "changes": [] }),
        ));
        assert!(
            session
                .workspace
                .definition(&uri, use_text.find("sibling").unwrap())
                .is_none()
        );
        assert!(
            session
                .workspace
                .definition(&uri, use_text.find("dependency").unwrap())
                .is_none()
        );
        assert!(
            session
                .workspace
                .snapshot(&file_path_to_uri(&dependency).unwrap())
                .is_none()
        );
    }

    #[test]
    fn unrelated_projects_do_not_supply_each_others_definitions() {
        let project_dir = TempProjectDir::new();
        for name in ["a", "b"] {
            project_dir.write(
                &format!("{name}/zkcl.sexp"),
                &format!(
                    "(project \"acme/{name}\" (version \"1.0.0\") (library (files \"main.lisp\")))"
                ),
            );
        }
        let text = "(in-package :cl-user)\n(foreign)";
        let a = project_dir.write("a/main.lisp", text);
        let b_text = "(in-package :cl-user)\n(defun foreign () nil)\n(foreign)";
        let b = project_dir.write("b/main.lisp", b_text);
        let (mut session, _) = Session::new(
            serde_json::from_value(serde_json::json!({ "capabilities": {} })).unwrap(),
        );
        TempProjectDir::open(&mut session, &a, text);
        TempProjectDir::open(&mut session, &b, b_text);
        let a = file_path_to_uri(&a).unwrap();
        let b = file_path_to_uri(&b).unwrap();
        assert!(
            session
                .workspace
                .definition(&a, text.find("foreign").unwrap())
                .is_none()
        );
        assert!(
            session
                .workspace
                .definition(&b, b_text.rfind("foreign").unwrap())
                .is_some()
        );
        let diagnostics = session.workspace.diagnostic_snapshot(&a).unwrap();
        assert!(diagnostics.document(&b).is_none());
        assert!(diagnostics.document(&a).is_some());
    }
}
