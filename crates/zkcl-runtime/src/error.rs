use std::error::Error;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use crate::SourceDocument;
use crate::condition::Class;
use crate::read::{Form, FormOrigin, FormSource};
use zkcl_diagnostics::{
    Diagnostic, DiagnosticContext, Label, LabelRole, Note, NoteKind, Severity,
    SourceOrigin as DiagnosticSourceOrigin, SourceProvenance,
};

fn report_provenance(source: &FormSource, document: &SourceDocument) -> SourceProvenance {
    SourceProvenance {
        origin: match source.origin {
            FormOrigin::Original => DiagnosticSourceOrigin::Original,
            FormOrigin::Reader => DiagnosticSourceOrigin::Reader,
            FormOrigin::Macro => DiagnosticSourceOrigin::Macro,
            FormOrigin::Compiler => DiagnosticSourceOrigin::Compiler,
        },
        expansion: source
            .expansion
            .iter()
            .filter_map(|range| document.span(range.clone()).ok())
            .collect(),
    }
}

/// The process already owns a Lisp runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the Lisp runtime has already started in this process")]
pub struct AlreadyStarted;

/// The Lisp heap could not satisfy an allocation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the Lisp heap cannot satisfy the allocation")]
pub struct OutOfMemory;

/// The category of an error returned to an embedding application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvalErrorKind {
    Read,
    Compile,
    MacroExpansion,
    LispCondition(Class),
    HostCallback,
    OutOfMemory,
    Cancelled,
    Runtime,
}

/// Why the runtime stopped an invocation at an execution checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecutionStop {
    /// A cancellation that unwinds Lisp cleanup forms.
    Cancelled,
    /// A host interrupt that skips Lisp cleanup forms.
    Aborted,
    /// The invocation consumed its configured step budget.
    BudgetExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticTag {
    Category(EvalErrorKind),
    ExecutionStop(ExecutionStop),
    UnexpectedReadEof,
    CallbackTransfer,
}

/// One Lisp frame captured when an [`EvalError`] was raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalStackFrame {
    /// The function name recorded when the bytecode was compiled.
    ///
    /// This is `None` for anonymous functions and top-level thunks.
    pub function: Option<String>,
    /// The canonical physical pathname of the loaded source, when known.
    pub source_path: Option<PathBuf>,
    /// The source revision that owns `source_range`.
    pub source: Option<SourceDocument>,
    /// Whether this activation was executing native code.
    pub native: bool,
    /// The byte range in the decoded UTF-8 source carried by this frame's
    /// active canonical instruction.
    ///
    /// This is `None` when that instruction has no source-bearing operand. Point
    /// locations have `start == end`; the range is never borrowed from an
    /// omitted native frame.
    pub source_range: Option<Range<usize>>,
    /// The canonical zkcl bytecode instruction executing in this frame.
    pub bytecode_pc: Option<usize>,
}

impl fmt::Display for EvalStackFrame {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(self.function.as_deref().unwrap_or("<unnamed Lisp frame>"))?;
        if let Some(range) = &self.source_range {
            write!(output, " at source byte {}", range.start)?;
        } else if let Some(pc) = self.bytecode_pc {
            write!(output, " at bytecode instruction {pc}")?;
        }
        if let Some(path) = &self.source_path {
            write!(output, " in {}", path.display())?;
        }
        Ok(())
    }
}

/// The operation that supplied context for a captured error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvalStage {
    Read,
    Compile,
    Load,
    Evaluate,
}

/// Context is kept separately from the condition's original message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvalErrorContext {
    SourceFile(PathBuf),
    Artifact(PathBuf),
    Stage(EvalStage),
    EmbeddedModule(String),
    ReaderInput { range: Range<usize> },
    MacroExpansion { range: Option<Range<usize>> },
}

/// An error reported while reading, compiling, or evaluating Lisp.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct EvalError {
    tag: DiagnosticTag,
    start_byte: usize,
    end_byte: usize,
    location_known: bool,
    provenance: Option<FormSource>,
    contexts: Vec<EvalErrorContext>,
    debugger_aborted: bool,
    message: String,
    document: Option<SourceDocument>,
    stack_frames: Box<[EvalStackFrame]>,
    #[source]
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl EvalError {
    pub(crate) fn new(offset: usize, message: impl Into<String>) -> Self {
        Self::at_offset(EvalErrorKind::Runtime, offset, message)
    }

    pub(crate) fn from_error(offset: usize, source: impl Error + Send + Sync + 'static) -> Self {
        Self::with_source(EvalErrorKind::Runtime, offset, Box::new(source))
    }

    pub(crate) fn read(offset: usize, message: impl Into<String>) -> Self {
        Self::at_offset(EvalErrorKind::Read, offset, message)
    }

    pub(crate) fn read_span(range: Range<usize>, message: impl Into<String>) -> Self {
        Self {
            end_byte: range.end,
            ..Self::at_offset(EvalErrorKind::Read, range.start, message)
        }
    }

    pub(crate) fn unexpected_read_eof(range: Range<usize>, message: impl Into<String>) -> Self {
        Self {
            tag: DiagnosticTag::UnexpectedReadEof,
            ..Self::read_span(range, message)
        }
    }

    /// Whether an incomplete form exhausted the input.
    pub fn is_read_eof(&self) -> bool {
        self.tag == DiagnosticTag::UnexpectedReadEof
    }

    pub(crate) fn in_lisp_read(mut self, at: usize, input_start: usize) -> (Class, Self) {
        if self.kind() == EvalErrorKind::Cancelled {
            return (Class::ReaderError, self);
        }
        let class = if self.is_read_eof() {
            Class::EndOfFile
        } else {
            Class::ReaderError
        };
        self.start_byte += input_start;
        self.end_byte += input_start;
        let original_source = self.document.is_some();
        let wrapped = Self {
            tag: DiagnosticTag::Category(EvalErrorKind::LispCondition(class)),
            start_byte: if original_source { self.start_byte } else { at },
            end_byte: if original_source { self.end_byte } else { at },
            debugger_aborted: self.debugger_aborted,
            message: self.message.clone(),
            location_known: if original_source {
                self.location_known
            } else {
                true
            },
            provenance: original_source.then(|| self.provenance.clone()).flatten(),
            contexts: vec![EvalErrorContext::ReaderInput {
                range: self.byte_range(),
            }],
            document: self.document.clone(),
            stack_frames: Box::default(),
            source: Some(Box::new(self)),
        };
        (class, wrapped)
    }

    pub(crate) fn compile(offset: usize, message: impl Into<String>) -> Self {
        Self::at_offset(EvalErrorKind::Compile, offset, message)
    }

    pub(crate) fn callback_transfer(at: usize) -> Self {
        Self {
            tag: DiagnosticTag::CallbackTransfer,
            ..Self::at_offset(EvalErrorKind::Runtime, at, String::new())
        }
    }

    pub(crate) fn macro_expansion(offset: usize, message: impl Into<String>) -> Self {
        Self::at_offset(EvalErrorKind::MacroExpansion, offset, message)
    }

    pub(crate) fn out_of_memory(offset: usize, source: OutOfMemory) -> Self {
        Self::with_source(EvalErrorKind::OutOfMemory, offset, Box::new(source))
    }

    pub(crate) fn host_callback(offset: usize, source: Box<dyn Error + Send + Sync>) -> Self {
        // A callback can reenter this runtime. Propagating its cancellation
        // with `?` must not turn the host abort into a Lisp condition.
        let source = match source.downcast::<Self>() {
            Ok(error)
                if error.kind() == EvalErrorKind::Cancelled || error.is_control_transfer() =>
            {
                return *error;
            }
            Ok(error) => error,
            Err(other) => other,
        };
        Self::with_source(EvalErrorKind::HostCallback, offset, source)
    }

    #[cold]
    pub(crate) fn execution_stopped(offset: usize, stop: ExecutionStop) -> Self {
        let message = match stop {
            ExecutionStop::Cancelled | ExecutionStop::Aborted => "evaluation cancelled",
            ExecutionStop::BudgetExhausted => "execution step budget exceeded",
        };
        Self {
            tag: DiagnosticTag::ExecutionStop(stop),
            ..Self::at_offset(EvalErrorKind::Cancelled, offset, message)
        }
    }

    pub(crate) fn file_io(offset: usize, path: &Path, source: std::io::Error) -> Self {
        Self::io_error(offset, source).in_file(path)
    }

    pub(crate) fn file_format(
        offset: usize,
        path: &Path,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self::from_error(offset, source).in_file(path)
    }

    pub(crate) fn io_error(offset: usize, source: std::io::Error) -> Self {
        Self::from_error(offset, source)
    }

    /// Add an operation or file context without rewriting the original message.
    pub fn with_context(mut self, context: EvalErrorContext) -> Self {
        if !self.contexts.contains(&context) {
            self.contexts.push(context);
        }
        self
    }

    /// Additional contexts, in the order they were attached while unwinding.
    pub fn contexts(&self) -> &[EvalErrorContext] {
        &self.contexts
    }

    pub(crate) fn without_source_location(mut self) -> Self {
        self.location_known = false;
        self.provenance = None;
        self
    }

    pub(crate) fn append_detail(&mut self, detail: fmt::Arguments<'_>) {
        use fmt::Write;
        write!(self.message, ", {detail}").expect("writing to a string");
    }

    pub(crate) fn in_file(self, path: &Path) -> Self {
        self.with_context(EvalErrorContext::SourceFile(path.to_owned()))
    }

    pub(crate) fn at_form(mut self, form: &Form) -> Self {
        if self
            .provenance
            .as_ref()
            .is_none_or(|source| source.range.is_none())
        {
            if let Some(source) = form.source_at(self.start_byte) {
                if let Some(range) = &source.range {
                    let generated = self.provenance.as_ref().map(|source| source.origin);
                    self.start_byte = range.start;
                    self.end_byte = range.end;
                    self.location_known = true;
                    let mut provenance = source.clone();
                    if let Some(origin) = generated {
                        provenance.origin = origin;
                    }
                    self.provenance = Some(provenance);
                }
            } else if self.provenance.is_none() && form.at == self.start_byte {
                // A synthesized form can inherit an outer editable call later;
                // the inherited range must still be described as generated.
                self.provenance = Some(form.source.clone());
                self.location_known = false;
            }
        }
        self
    }

    pub(crate) fn during_macro_expansion(self, offset: usize) -> Self {
        if self.tag == DiagnosticTag::CallbackTransfer
            || self.kind() == EvalErrorKind::Cancelled
            || (self.kind() == EvalErrorKind::MacroExpansion && self.start_byte == offset)
        {
            return self;
        }
        Self {
            tag: DiagnosticTag::Category(EvalErrorKind::MacroExpansion),
            start_byte: offset,
            end_byte: offset,
            debugger_aborted: self.debugger_aborted,
            message: self.message.clone(),
            location_known: true,
            provenance: None,
            contexts: vec![EvalErrorContext::MacroExpansion {
                range: Some(offset..offset),
            }],
            document: None,
            stack_frames: Box::default(),
            source: Some(Box::new(self)),
        }
    }

    pub(crate) fn with_condition(mut self, class: Class) -> Self {
        if self.kind() == EvalErrorKind::Runtime {
            self.tag = DiagnosticTag::Category(EvalErrorKind::LispCondition(class));
        }
        self
    }

    pub(crate) fn provenance(&self) -> Option<&FormSource> {
        self.provenance.as_ref()
    }

    pub(crate) fn restore_compiled_metadata(
        mut self,
        document: Option<SourceDocument>,
        known: bool,
        provenance: Option<FormSource>,
        contexts: Vec<EvalErrorContext>,
    ) -> Self {
        self.document = document;
        self.location_known = known;
        self.provenance = provenance;
        self.contexts = contexts;
        self
    }

    pub(crate) fn from_compiled_diagnostic(
        kind: EvalErrorKind,
        range: Range<usize>,
        message: String,
        source: Option<Self>,
    ) -> Self {
        Self {
            end_byte: range.end,
            source: source.map(|cause| Box::new(cause) as Box<dyn Error + Send + Sync>),
            ..Self::at_offset(kind, range.start, message)
        }
    }

    fn at_offset(kind: EvalErrorKind, offset: usize, message: impl Into<String>) -> Self {
        Self {
            tag: DiagnosticTag::Category(kind),
            start_byte: offset,
            end_byte: offset,
            location_known: true,
            provenance: None,
            contexts: Vec::new(),
            debugger_aborted: false,
            message: message.into(),
            document: None,
            stack_frames: Box::default(),
            source: None,
        }
    }

    pub(crate) fn with_source(
        kind: EvalErrorKind,
        offset: usize,
        source: Box<dyn Error + Send + Sync>,
    ) -> Self {
        let message = source.to_string();
        Self {
            source: Some(source),
            ..Self::at_offset(kind, offset, message)
        }
    }

    /// The structured category of this error.
    pub fn kind(&self) -> EvalErrorKind {
        match self.tag {
            DiagnosticTag::Category(kind) => kind,
            DiagnosticTag::ExecutionStop(_) => EvalErrorKind::Cancelled,
            DiagnosticTag::UnexpectedReadEof => EvalErrorKind::Read,
            DiagnosticTag::CallbackTransfer => EvalErrorKind::Runtime,
        }
    }

    /// The execution stop reason, when this diagnostic comes from a checkpoint.
    pub fn execution_stop(&self) -> Option<ExecutionStop> {
        match self.tag {
            DiagnosticTag::ExecutionStop(stop) => Some(stop),
            _ => None,
        }
    }

    pub(crate) fn after_debugger_abort(mut self) -> Self {
        self.debugger_aborted = true;
        self
    }

    /// Whether the active debugger abandoned this invocation on request.
    pub fn debugger_aborted(&self) -> bool {
        self.debugger_aborted
    }

    /// Whether Lisp transferred control out of an active host callback.
    ///
    /// A debugger callback must propagate this result so the runtime can finish
    /// the selected restart or frame operation in its original dynamic context.
    pub fn is_control_transfer(&self) -> bool {
        self.tag == DiagnosticTag::CallbackTransfer
    }

    /// The byte offset reported by the reader, compiler, or evaluator.
    /// Use `source_range` to distinguish known points from unavailable positions.
    pub fn offset(&self) -> usize {
        self.start_byte
    }

    /// Half-open range of UTF-8 byte offsets. Point diagnostics have an empty
    /// range at [`Self::offset`]. Prefer `source_range` when position may be unavailable.
    pub fn byte_range(&self) -> Range<usize> {
        self.start_byte..self.end_byte
    }

    /// The known UTF-8 range, including genuinely known zero-length points.
    pub fn source_range(&self) -> Option<Range<usize>> {
        self.location_known.then(|| self.byte_range())
    }

    /// Capture a presentation-independent report without reraising a condition.
    pub fn to_diagnostic(&self) -> Diagnostic {
        let mut report = Diagnostic::new(Severity::Error, self.message.clone());
        report.code = Some(
            match self.kind() {
                EvalErrorKind::Read => "runtime/read",
                EvalErrorKind::Compile => "runtime/compile",
                EvalErrorKind::MacroExpansion => "runtime/macro-expansion",
                EvalErrorKind::LispCondition(_) => "runtime/condition",
                EvalErrorKind::HostCallback => "runtime/host-callback",
                EvalErrorKind::OutOfMemory => "runtime/out-of-memory",
                EvalErrorKind::Cancelled => "runtime/cancelled",
                EvalErrorKind::Runtime => "runtime/evaluation",
            }
            .into(),
        );
        if let Some(document) = &self.document {
            report.sources.push(document.clone());
            if let Some(range) = self.source_range() {
                if let Ok(span) = document.span(range) {
                    let message = match self.provenance.as_ref().map(|source| source.origin) {
                        Some(FormOrigin::Reader) => "reader-generated expression from this input",
                        Some(FormOrigin::Macro) => "expression generated by this macro call",
                        Some(FormOrigin::Compiler) => "expression generated from this input",
                        _ => "",
                    };
                    report.labels.push(Label {
                        span,
                        role: LabelRole::Primary,
                        message: message.into(),
                        provenance: self
                            .provenance
                            .as_ref()
                            .map(|source| report_provenance(source, document)),
                    });
                }
            }
            if let Some(source) = &self.provenance {
                for range in source.expansion.iter().rev().skip(1) {
                    if let Ok(span) = document.span(range.clone()) {
                        report.labels.push(Label {
                            span,
                            role: LabelRole::Secondary,
                            message: "expanded from this macro call".into(),
                            provenance: None,
                        });
                    }
                }
            }
        }
        for context in &self.contexts {
            let message = match context {
                EvalErrorContext::SourceFile(path) => {
                    let shown_in_primary = report.labels.iter().any(|label| {
                        label.role == LabelRole::Primary
                            && report
                                .source(label.span.snapshot())
                                .and_then(SourceDocument::path)
                                == Some(path.as_path())
                    });
                    if !shown_in_primary {
                        report.contexts.push(DiagnosticContext {
                            message: "source file".into(),
                            path: Some(path.clone()),
                        });
                    }
                    continue;
                }
                EvalErrorContext::Artifact(path) => {
                    report.contexts.push(DiagnosticContext {
                        message: "compiled artifact".into(),
                        path: Some(path.clone()),
                    });
                    continue;
                }
                EvalErrorContext::Stage(stage) => format!(
                    "while {}",
                    match stage {
                        EvalStage::Read => "reading",
                        EvalStage::Compile => "compiling",
                        EvalStage::Load => "loading",
                        EvalStage::Evaluate => "evaluating",
                    }
                ),
                EvalErrorContext::EmbeddedModule(module) => {
                    format!("embedded AOT module: {module}")
                }
                EvalErrorContext::ReaderInput { range } => {
                    format!("reader input bytes {}..{}", range.start, range.end)
                }
                EvalErrorContext::MacroExpansion { .. } => "while expanding a macro".into(),
            };
            report.notes.push(Note {
                kind: NoteKind::Note,
                message,
            });
        }
        let mut cause = self.source();
        while let Some(error) = cause {
            if let Some(error) = error.downcast_ref::<EvalError>() {
                if let (Some(document), Some(range)) =
                    (error.source_document(), error.source_range())
                {
                    if let Ok(span) = document.span(range) {
                        if !report.labels.iter().any(|label| label.span == span) {
                            report.add_source(document.clone());
                            report.labels.push(Label {
                                span,
                                role: LabelRole::Secondary,
                                message: error.message().into(),
                                provenance: error
                                    .provenance
                                    .as_ref()
                                    .map(|source| report_provenance(source, document)),
                            });
                        }
                    }
                }
            }
            if error.to_string() != self.message {
                report.notes.push(Note {
                    kind: NoteKind::Note,
                    message: format!("caused by: {error}"),
                });
            }
            cause = error.source();
        }
        report
    }

    /// The diagnostic message without its source offset.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The source revision which owns the primary byte range.
    pub fn source_document(&self) -> Option<&SourceDocument> {
        self.document.as_ref()
    }

    pub(crate) fn in_document(mut self, document: &SourceDocument) -> Self {
        self.attach_document(document);
        self
    }

    /// Attach the compiler's source occurrence captured by the active frame.
    /// The caller uses the frame's own snapshot and current instruction, never
    /// a similarly numbered offset from a different activation.
    pub(crate) fn attach_source_location(
        &mut self,
        document: &SourceDocument,
        source: Option<&zkcl_bytecode::SourceLocation>,
    ) {
        if self
            .document
            .as_ref()
            .is_some_and(|existing| existing.id() != document.id())
        {
            return;
        }
        self.attach_document(document);
        if let Some(source) = source {
            self.provenance = Some(source.clone());
            if let Some(range) = &source.range {
                self.start_byte = range.start;
                self.end_byte = range.end;
                self.location_known = true;
            } else {
                self.location_known = false;
            }
        } else if self.provenance.is_none() {
            self.location_known = false;
        }
    }

    pub(crate) fn attach_document(&mut self, document: &SourceDocument) {
        if self.document.is_none() {
            self.document = Some(document.clone());
        }
    }

    /// Lisp frames captured before the runtime unwound.
    ///
    /// Frames are innermost first. Capture is bounded and is skipped when the
    /// runtime is already out of memory.
    pub fn stack_frames(&self) -> &[EvalStackFrame] {
        &self.stack_frames
    }

    pub(crate) fn attach_stack_frames(&mut self, frames: Vec<EvalStackFrame>) {
        if self.stack_frames.is_empty() {
            self.stack_frames = frames.into_boxed_slice();
        }
    }
}

// Causes may be arbitrary Rust errors. Compare their reported diagnostics.
impl PartialEq for EvalError {
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag
            && self.byte_range() == other.byte_range()
            && self.location_known == other.location_known
            && self.contexts == other.contexts
            && self.message == other.message
    }
}

impl Eq for EvalError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contexts_preserve_original_message_and_snapshot() {
        let document = SourceDocument::file(PathBuf::from("original.lisp"), Some("中文".into()));
        let error = EvalError::read_span(0..6, "bad input")
            .in_document(&document)
            .in_file(Path::new("original.lisp"))
            .with_context(EvalErrorContext::Stage(EvalStage::Read));
        assert_eq!(error.message(), "bad input");
        assert_eq!(error.to_string(), "bad input");
        assert_eq!(error.contexts().len(), 2);
        let report = error.to_diagnostic();
        assert_eq!(report.labels.len(), 1);
        assert_eq!(report.labels[0].span.range(), 0..6);
        assert_eq!(report.sources[0].id(), document.id());
    }

    #[test]
    fn unavailable_position_differs_from_known_zero_point() {
        let unknown = EvalError::new(0, "host failure").without_source_location();
        assert_eq!(unknown.source_range(), None);
        assert!(unknown.to_diagnostic().labels.is_empty());
        assert_eq!(
            EvalError::read(0, "expected a form").source_range(),
            Some(0..0)
        );
    }
    #[test]
    fn macro_report_labels_each_saved_snapshot() {
        let body = SourceDocument::input("(error \"failure\")").with_uri("untitled:definition");
        let call = SourceDocument::input("  (invoke)").with_uri("untitled:call");
        let form = Form::new(2, crate::read::FormKind::Integer(1)).with_reader_range(2..10, false);
        let error = EvalError::new(1, "failure")
            .in_document(&body)
            .during_macro_expansion(2)
            .at_form(&form)
            .in_document(&call);
        let report = error.to_diagnostic();
        assert_eq!(report.labels.len(), 2);
        assert_eq!(report.labels[0].span.snapshot(), call.snapshot_id());
        assert_eq!(report.labels[0].span.range(), 2..10);
        assert_eq!(report.labels[1].span.snapshot(), body.snapshot_id());
        assert_eq!(report.sources.len(), 2);
    }
}
