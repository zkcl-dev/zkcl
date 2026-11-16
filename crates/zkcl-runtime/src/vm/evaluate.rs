use crate::compile;
use crate::condition::Fault;
use crate::heap::{RootFrame, RootSlots, Word};
use crate::loader::{self, FileContents, LoadFile};
use crate::macros::CallExpander;
use crate::read::{Form, FormKind, SourceReadCursor, StreamRead};
use crate::{EvalError, EvalErrorKind, Root, Runtime, SourceDocument};

use super::modules::Require;
use super::{
    ExecState, FunctionTemplate, Invocation, Pending, Primitive, Resume, StepControl, Values,
    accept, callback, dispatch_error, execute, finish, invoke,
};

struct FileCursor {
    source: LoadFile,
    reader: SourceReadCursor,
    bindings: LoadBindings,
    print_previous: bool,
    call_site: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoadBindings {
    Pending,
    Installed,
    None,
}

pub(super) struct Evaluation {
    forms: Vec<(usize, Form)>,
    file: Option<FileCursor>,
    document: Option<SourceDocument>,
    definitions: Vec<(SourceDocument, zkcl_bytecode::DefinitionDebug)>,
    following: Option<Box<Require>>,
    /// A form that finished compiling: its warnings are signalled before the
    /// code runs, so handlers around EVAL or LOAD see them.
    ready: Option<(FunctionTemplate, usize)>,
    warnings: std::vec::IntoIter<compile::Warning>,
    resume: Resume,
    defer_unhandled_expansion_errors: bool,
}

enum Step {
    Warn(compile::Warning),
    InvokeFunction { function: Word, at: usize },
    Done,
}

impl Evaluation {
    fn form(form: Form, resume: Resume, document: Option<SourceDocument>) -> Self {
        Self {
            forms: vec![(0, form)],
            file: None,
            document,
            definitions: Vec::new(),
            following: None,
            ready: None,
            warnings: Vec::new().into_iter(),
            resume,
            defer_unhandled_expansion_errors: false,
        }
    }

    pub(super) fn lisp_eval(form: Form, resume: Resume) -> Self {
        Self {
            defer_unhandled_expansion_errors: true,
            ..Self::form(form, resume, None)
        }
    }

    pub(super) fn file(file: LoadFile, resume: Resume, call_site: Option<usize>) -> Self {
        Self::file_cursor(file, resume, call_site, LoadBindings::Pending)
    }

    fn file_cursor(
        file: LoadFile,
        resume: Resume,
        call_site: Option<usize>,
        bindings: LoadBindings,
    ) -> Self {
        let document = match &file.contents {
            FileContents::Source(document) => Some(document.clone()),
            _ => None,
        };
        Self {
            forms: Vec::new(),
            document,
            definitions: Vec::new(),
            file: Some(FileCursor {
                source: file,
                reader: SourceReadCursor::default(),
                bindings,
                print_previous: false,
                call_site,
            }),
            following: None,
            ready: None,
            warnings: Vec::new().into_iter(),
            resume,
            defer_unhandled_expansion_errors: false,
        }
    }

    fn builtin_source(source: &'static str) -> Self {
        // As with Runtime::load_str, IN-PACKAGE changes the active package.
        Self::file_cursor(
            LoadFile::embedded_source(source),
            Resume::Call(None),
            None,
            LoadBindings::None,
        )
    }

    pub(super) fn module(file: LoadFile, require: Box<Require>, resume: Resume) -> Self {
        let mut evaluation = Self::file(file, resume, Some(require.at));
        evaluation.following = Some(require);
        evaluation
    }

    fn step(
        &mut self,
        runtime: &Runtime,
        access: &callback::CallbackAccess<'_>,
        call_expander: Option<&CallExpander<'_>>,
        call: Option<&callback::CallValues<'_>>,
    ) -> Result<Step, Fault> {
        loop {
            if self.forms.is_empty() && self.ready.is_none() {
                for (document, definition) in self.definitions.drain(..) {
                    runtime.record_definition_debug(&document, [definition]);
                }
            }
            if let Some(warning) = self.warnings.next() {
                return Ok(Step::Warn(warning));
            }
            if let Some((code, at)) = self.ready.take() {
                let function = code.instantiate_with_source(runtime, self.document.as_ref())?;
                return Ok(Step::InvokeFunction { function, at });
            }
            if let Some((depth, form)) = self.forms.pop() {
                if let Some(document) = &self.document {
                    self.definitions.extend(
                        crate::definition_source::definition_debug(runtime, &form)?
                            .into_iter()
                            .map(|definition| (document.clone(), definition)),
                    );
                }
                if depth == compile::EXPANSION_LIMIT {
                    return Err(EvalError::macro_expansion(
                        form.at,
                        "top-level expansion depth exceeded",
                    )
                    .into());
                }
                if let Some(expansion) = compile::expand_global(runtime, &form, call_expander)? {
                    self.forms.push((depth + 1, expansion));
                    continue;
                }
                if let Some(body_len) = sequence_len(&form)? {
                    if body_len == 0 {
                        access.set_one_value(Word::NIL);
                    } else {
                        let FormKind::List(mut items, None) = form.kind else {
                            unreachable!("an executable sequence is a proper list");
                        };
                        let body_start = items.len() - body_len;
                        self.forms.extend(
                            items
                                .drain(body_start..)
                                .rev()
                                .map(|child| (depth + 1, child)),
                        );
                    }
                    continue;
                }
                let feedback = self.file.is_none()
                    && self.document.is_some()
                    && runtime.repl_compiler_feedback.get();
                let (code, warnings) = compile::compile_with_feedback(
                    runtime,
                    &form,
                    call_expander,
                    feedback,
                    self.document.as_ref(),
                )?;
                self.ready = Some((code, form.at));
                self.warnings = warnings.into_iter();
                continue;
            }

            let Some(file) = &mut self.file else {
                return Ok(Step::Done);
            };
            if file.print_previous && file.source.print {
                let at = file.call_site.unwrap_or(0);
                let text = access.with_values(|values| printable_values(runtime, values))?;
                if !text.is_empty() {
                    write_load_message(runtime, call, &text, at)?;
                }
            }
            let form = match &mut file.source.contents {
                FileContents::Source(document) => {
                    let text = document.text().expect("opened source file has text");
                    read_source_form(runtime, text, &mut file.reader, file.call_site).map_err(
                        |mut fault| {
                            if file.call_site.is_none() {
                                fault.host = fault.host.in_document(document);
                            }
                            fault
                        },
                    )?
                }
                FileContents::EmbeddedSource(source) => {
                    read_source_form(runtime, source, &mut file.reader, file.call_site)?
                }
                FileContents::SourceStream(stream) => {
                    let at = file.call_site.unwrap_or(0);
                    let input = super::streams::input_stream(runtime, stream.get(), at)?;
                    let operation = StreamRead::Form {
                        recursive: false,
                        preserve: false,
                    };
                    super::streams::read_form(runtime, input, operation, at)?
                }
                FileContents::ProtocolStream {
                    stream,
                    inspect_type,
                    verbose,
                } => {
                    let at = file.call_site.unwrap_or(0);
                    if *inspect_type {
                        if *verbose {
                            write_load_message(runtime, call, "; Loading stream\n", at)?;
                        }
                        if let Some(bytes) =
                            super::streams::gray::load_octets(runtime, stream, call, at)?
                        {
                            file.source.contents =
                                loader::compiled_stream(runtime, stream, &bytes, at)?;
                            continue;
                        }
                        *inspect_type = false;
                    }
                    let operation = StreamRead::Form {
                        recursive: false,
                        preserve: false,
                    };
                    super::streams::gray_input::read_form(runtime, stream, operation, call, at)?
                }
                FileContents::Compiled(loader) => {
                    let first_value = access.values_first();
                    let function = loader.next(runtime, first_value).map_err(|error| {
                        if error.kind() == EvalErrorKind::OutOfMemory {
                            error.into()
                        } else {
                            Fault::file_error(runtime, file.source.pathname.get(), error)
                        }
                    })?;
                    if let Some((function, top_level_form)) = function {
                        if let Some(code) = runtime.heap.function_info(function) {
                            let code = runtime.heap.code_store.get(code.code_id);
                            if let Some(document) = &code.source {
                                self.definitions.extend(
                                    code.debug
                                        .definitions
                                        .iter()
                                        .cloned()
                                        .map(|definition| (document.clone(), definition)),
                                );
                            }
                        }
                        file.print_previous = top_level_form;
                        return Ok(Step::InvokeFunction {
                            function,
                            at: file.call_site.unwrap_or(0),
                        });
                    }
                    access.set_one_value(Word::T);
                    return Ok(Step::Done);
                }
            };
            let Some(form) = form else {
                access.set_one_value(Word::T);
                return Ok(Step::Done);
            };
            file.print_previous = true;
            self.forms.push((0, form));
        }
    }
}

fn read_source_form(
    runtime: &Runtime,
    source: &str,
    cursor: &mut SourceReadCursor,
    call_site: Option<usize>,
) -> Result<Option<Form>, Fault> {
    cursor.next_form(runtime, source).map_err(|error| {
        if error.kind() == EvalErrorKind::Read
            && let Some(call_site) = call_site
        {
            let (class, error) = error.in_lisp_read(call_site, 0);
            Fault::plain(class, error)
        } else {
            error.into()
        }
    })
}

fn evaluation_invocation<'rt>(
    runtime: &'rt Runtime,
    function: Word,
    values: &mut Values,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    invoke(
        runtime,
        function,
        RootSlots::empty(),
        0..0,
        values,
        Resume::Evaluate,
        at,
    )
}

fn sequence_len(form: &Form) -> Result<Option<usize>, EvalError> {
    let Some([operator, body @ ..]) = form.list() else {
        return Ok(None);
    };
    match operator.name().map(|name| name.operator()) {
        Some("PROGN") => Ok(Some(body.len())),
        Some("EVAL-WHEN") => Ok(Some(compile::eval_when_body(body, form.at)?.len())),
        _ => Ok(None),
    }
}

fn printable_values(runtime: &Runtime, values: &Values) -> Result<String, Fault> {
    let mut output = String::new();
    for index in 0..values.len {
        let value = runtime.root(values.get(index));
        output.push_str(&runtime.repr(&value)?);
        output.push('\n');
    }
    Ok(output)
}

fn write_load_message(
    runtime: &Runtime,
    call: Option<&callback::CallValues<'_>>,
    text: &str,
    at: usize,
) -> Result<(), Fault> {
    let write = runtime.root(super::Primitive::named("WRITE-STRING").expect("WRITE-STRING"));
    let text = runtime
        .make_string(text)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    if let Some(call) = call {
        call(&write, &[&text], at)?;
    } else {
        runtime.call(&write, &[&text])?;
    }
    Ok(())
}

fn warning_arguments(runtime: &Runtime, warning: &compile::Warning) -> Result<RootFrame, Fault> {
    let args = runtime.root_frame(2);
    for (index, text) in ["~A", warning.message.as_str()].into_iter().enumerate() {
        args.set(
            index,
            runtime
                .heap
                .string(text)
                .map_err(|e| EvalError::out_of_memory(warning.at, e))?,
        );
    }
    Ok(args)
}

fn deliver_style_warning(runtime: &Runtime, warning: &compile::Warning) -> Result<(), Fault> {
    use std::fmt::Write as _;
    let mut message = String::new();
    writeln!(message, "Style warning: {}", warning.message).unwrap();
    let stream = runtime.intern_common_lisp("*ERROR-OUTPUT*", warning.at)?;
    super::streams::write_text(
        runtime,
        super::symbol_value(runtime, stream, warning.at)?,
        &message,
        warning.at,
    )
}

pub(crate) fn signal_compiler_warning(
    runtime: &Runtime,
    warning: &compile::Warning,
) -> Result<super::ContinuationStep, Fault> {
    Ok(super::ContinuationStep::Call {
        target: super::Primitive::named("WARN").expect("WARN primitive"),
        args: warning_arguments(runtime, warning)?,
        at: warning.at,
    })
}

pub(crate) fn signal_host_warning(
    runtime: &Runtime,
    warning: &compile::Warning,
) -> Result<(), Fault> {
    let args = warning_arguments(runtime, warning)?;
    let mut values = Values::new(runtime);
    let invocation = invoke(
        runtime,
        super::Primitive::named("WARN").expect("WARN primitive"),
        &args,
        0..args.len(),
        &mut values,
        Resume::Call(None),
        warning.at,
    )?;
    execute(runtime, values, invocation)
        .map(|_| ())
        .map_err(Fault::from)
}

pub(super) fn dispatch<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut evaluation: Box<Evaluation>,
) -> Result<StepControl<'rt>, EvalError> {
    if let Some(file) = &mut evaluation.file
        && file.bindings == LoadBindings::Pending
    {
        let bindings = match loader::bindings(runtime, &file.source, file.call_site.unwrap_or(0)) {
            Ok(bindings) => bindings,
            Err(error) => return dispatch_error(runtime, exec, error.into()),
        };
        exec.control
            .bind(exec.frames.len().saturating_sub(1), &runtime.heap, bindings);
        file.bindings = LoadBindings::Installed;
    }
    let step = match callback::run_with_access(
        runtime,
        exec,
        evaluation.defer_unhandled_expansion_errors,
        |access, call_expander, call| evaluation.step(runtime, access, call_expander, call),
    ) {
        callback::Outcome::Completed(step) => step,
        callback::Outcome::Unwind(target) => return Ok(StepControl::Unwind(target)),
        callback::Outcome::Unhandled(error) => return Err(error),
    };
    let invocation = match step {
        Ok(Step::Warn(warning)) => {
            if warning.style {
                deliver_style_warning(runtime, &warning)
                    .map(|()| Invocation::Done(Resume::Evaluate))
            } else {
                warning_arguments(runtime, &warning).and_then(|args| {
                    invoke(
                        runtime,
                        Primitive::named("WARN").expect("WARN primitive"),
                        &args,
                        0..args.len(),
                        &mut exec.values,
                        Resume::Evaluate,
                        warning.at,
                    )
                })
            }
        }
        Ok(Step::InvokeFunction { function, at }) => {
            evaluation_invocation(runtime, function, &mut exec.values, at)
        }
        Ok(Step::Done) => {
            if evaluation
                .file
                .as_ref()
                .is_some_and(|file| file.bindings == LoadBindings::Installed)
            {
                exec.control.unbind(1);
            }
            if let Some(mut following) = evaluation.following {
                match following.next_file(runtime) {
                    Ok(Some(file)) => {
                        return Ok(StepControl::pending(Pending::Evaluate(Box::new(
                            Evaluation::module(file, following, evaluation.resume),
                        ))));
                    }
                    Ok(None) => exec.values.one(Word::NIL),
                    Err(error) => return dispatch_error(runtime, exec, error),
                }
            }
            return Ok(StepControl::Pending(finish(
                evaluation.resume,
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            )));
        }
        Err(mut error) => {
            if matches!(
                error.host.kind(),
                EvalErrorKind::Read | EvalErrorKind::Compile | EvalErrorKind::MacroExpansion
            ) && let Some(document) = &evaluation.document
            {
                error.host = error.host.in_document(document);
            }
            return dispatch_error(runtime, exec, error);
        }
    };
    match invocation {
        Ok(invocation) => {
            exec.control
                .pause_evaluation(exec.frames.len().saturating_sub(1), evaluation);
            accept(runtime, exec, invocation)
        }
        Err(error) => dispatch_error(runtime, exec, error),
    }
}

pub(crate) fn eval_host(
    runtime: &Runtime,
    form: Form,
    document: Option<SourceDocument>,
) -> Result<Vec<Root<'_>>, EvalError> {
    execute(
        runtime,
        Values::new(runtime),
        Invocation::Evaluate(Box::new(Evaluation::form(
            form,
            Resume::Call(None),
            document,
        ))),
    )
}

pub(crate) fn prepare_eval<'rt>(
    runtime: &'rt Runtime,
    form: Form,
    document: SourceDocument,
) -> super::task_scheduler::TaskExecutor<'rt> {
    super::task_scheduler::TaskExecutor::new(
        Values::new(runtime),
        Invocation::Evaluate(Box::new(Evaluation::form(
            form,
            Resume::Call(None),
            Some(document),
        ))),
    )
}

pub(crate) fn load_host(runtime: &Runtime, file: LoadFile) -> Result<(), EvalError> {
    execute(
        runtime,
        Values::new(runtime),
        Invocation::Evaluate(Box::new(Evaluation::file(file, Resume::Call(None), None))),
    )
    .map(|_| ())
}

pub(crate) fn load_builtin_source(
    runtime: &Runtime,
    source: &'static str,
) -> Result<(), EvalError> {
    execute(
        runtime,
        Values::new(runtime),
        Invocation::Evaluate(Box::new(Evaluation::builtin_source(source))),
    )
    .map(|_| ())
}
