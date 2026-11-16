use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::mem::DropGuard;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zkcl_bytecode::{BytecodeFunction, CompiledFile, Immediate, Instruction};

use crate::compile::{self, FileCompiler};
use crate::condition::{self, Class, Fault};
use crate::encoding::ExternalFormat;
use crate::heap::{RootSlot, RootSlots, Word};
use crate::loader;
use crate::macros::CallExpander;
use crate::pathname::{self, Pathname};
use crate::read::{Reader, SourceReadCursor};
use crate::vm::{Continuation, ContinuationStep, ControlStack, Resume, Values};
use crate::vm::{callback, evaluate};
use crate::{EvalError, EvalErrorContext, EvalErrorKind, EvalStage, Runtime, SourceDocument};

mod build;
mod load;
mod load_forms;

pub(crate) use build::CompiledFileBuilder;
pub(crate) use load::CompiledFileLoader;

/// A tagged immediate from a wider VM becomes a constant on the narrower VM.
fn spill_wide_fixnums<E>(
    code: &mut BytecodeFunction,
    (min, max): (i64, i64),
    mut literal: impl FnMut(i64) -> Result<u32, E>,
) -> Result<(), E> {
    for instruction in &mut code.instructions {
        if let Instruction::Load(out, Immediate::Fixnum(value)) = *instruction
            && !(min..=max).contains(&value)
        {
            let index = code.constants.len();
            code.constants.push(literal(value)?);
            *instruction = Instruction::Constant(out, index);
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct WarningCounts {
    style: u64,
    non_style: u64,
}

/// COMPILE-FILE reports warnings signalled by compile-time evaluation as well
/// as compiler warnings. Count WARN conditions separately for each runtime.
pub(crate) fn note_signalled_warning(runtime: &Runtime, condition_word: Word) {
    runtime.signalled_warnings.update(|mut counts| {
        let count = if condition::matches_builtin(runtime, condition_word, Class::StyleWarning) {
            &mut counts.style
        } else {
            &mut counts.non_style
        };
        *count = count.wrapping_add(1);
        counts
    });
}

struct CompilationFiles {
    input_path: PathBuf,
    input_pathname: RootSlot,
    input_truename: RootSlot,
    output_path: PathBuf,
    output_pathname: RootSlot,
    source: SourceDocument,
}

impl Runtime {
    /// Save the current value and function bindings of the supplied symbols as
    /// a loadable FASL. Literal graph identity and mutable closure cells are
    /// preserved. Objects with a MAKE-LOAD-FORM method use its creation and
    /// initialization forms. Referenced packages and prerequisites of those
    /// forms must be available when the file is loaded.
    pub fn save_bindings(
        &self,
        path: &Path,
        symbols: &[&crate::Root<'_>],
    ) -> Result<(), EvalError> {
        use crate::vm::{ConstantTemplate, FunctionTemplate};
        use zkcl_bytecode::{Arity, Instruction as Ins};

        let _guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        let mut builder = CompiledFileBuilder::new(self, None);
        let mut code = FunctionTemplate {
            name: Some("restore saved bindings".into()),
            debug: Default::default(),
            instructions: Vec::new(),
            constants: Vec::new(),
            lambda_list: None,
            registers: 1,
            arity: Arity::ZERO,
            captures: Vec::new(),
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        };
        let mut seen = std::collections::HashSet::new();
        let mut constant = |word| {
            let index = code.constants.len();
            code.constants
                .push(ConstantTemplate::Rooted(std::rc::Rc::new(self.root_slot(
                    word,
                ))));
            index
        };
        for name in symbols {
            if !name.belongs_to(self) {
                return Err(EvalError::new(0, "binding belongs to a different runtime")
                    .without_source_location());
            }
            let word = name.slot.get();
            let attributes = self.heap.symbol_attributes(word).ok_or_else(|| {
                EvalError::new(0, "expected an interned binding name").without_source_location()
            })?;
            let Some(home) = attributes.home else {
                return Err(EvalError::new(0, "expected an interned binding name")
                    .without_source_location());
            };
            if attributes.is_constant_variable() || home == crate::Package::COMMON_LISP {
                return Err(EvalError::new(
                    0,
                    "cannot save a constant or built-in Common Lisp binding",
                ));
            }
            if !seen.insert(attributes.id) {
                continue;
            }
            let variable = self.heap.symbol_cell(word, false);
            let macro_function = self.heap.macro_expander(word);
            let function = macro_function.or_else(|| self.heap.symbol_cell(word, true));
            if variable.is_none() && function.is_none() {
                return Err(EvalError::new(
                    0,
                    format!("{} has no value or function binding", self.repr(name)?),
                ));
            }
            let symbol = constant(word);
            if self.heap.is_special(word) {
                code.instructions
                    .push(Ins::ProclaimSpecial { symbol, at: 0 });
            }
            if let Some(value) = variable {
                code.instructions.push(Ins::Constant(0, constant(value)));
                code.instructions.push(Ins::WriteCell { symbol, src: 0 });
            }
            let doc = self.heap.variable_documentation(word);
            if doc != Word::NIL {
                code.instructions.push(Ins::SetVariableDocumentation {
                    symbol,
                    doc: constant(doc),
                });
            }
            if let Some(function) = function {
                code.instructions.push(Ins::Constant(0, constant(function)));
                code.instructions.push(Ins::DefineFunction {
                    symbol,
                    definition: 0,
                    is_macro: macro_function.is_some(),
                    at: 0,
                });
            }
        }
        code.instructions.push(Ins::SetValues(0..0));
        builder.push_top_level_entry(self, code, None)?;
        let bytes = builder.finish(self)?.encode().map_err(|e| {
            EvalError::from_error(0, e)
                .without_source_location()
                .with_context(EvalErrorContext::Artifact(path.to_owned()))
        })?;
        write_fasl_atomically(path, &bytes).map_err(|e| {
            EvalError::io_error(0, e)
                .without_source_location()
                .with_context(EvalErrorContext::Artifact(path.to_owned()))
        })
    }
}

pub(crate) fn compile_host(
    runtime: &Runtime,
    input: &Path,
    output: Option<&Path>,
) -> Result<PathBuf, EvalError> {
    let requested = loader::host_pathname(input)?;
    let output = output.map(loader::host_pathname).transpose()?;
    let files = prepare_compile_file(
        runtime,
        &requested,
        output.as_ref(),
        false,
        ExternalFormat::Default,
        0,
    )
    .map_err(|error| {
        error
            .host
            .without_source_location()
            .with_context(EvalErrorContext::Stage(EvalStage::Compile))
    })?;
    let builder = compile_host_source(
        runtime,
        &files.source,
        &files.input_pathname,
        &files.input_truename,
    )?;
    write_fasl(runtime, builder, &files, 0).map_err(|error| {
        error
            .host
            .without_source_location()
            .with_context(EvalErrorContext::Stage(EvalStage::Compile))
    })
}

pub(crate) fn compile_host_bytecode(
    runtime: &Runtime,
    input: &Path,
) -> Result<CompiledFile, EvalError> {
    let requested = loader::host_pathname(input)?;
    let mut input = loader::open_file(runtime, &requested, false, 0)
        .map_err(|fault| {
            fault
                .host
                .without_source_location()
                .with_context(EvalErrorContext::Stage(EvalStage::Compile))
        })?
        .expect("compilation requires an existing file");
    let source = input
        .take_source(runtime, ExternalFormat::Default, 0)
        .map_err(|fault| {
            fault
                .host
                .without_source_location()
                .with_context(EvalErrorContext::Stage(EvalStage::Compile))
        })?;
    let source = SourceDocument::file(input.path.clone(), Some(source));
    compile_host_source(runtime, &source, &input.pathname, &input.truename)?
        .finish(runtime)
        .map_err(|error| error.in_file(&input.path))
}

fn compilation_bindings(
    runtime: &Runtime,
    pathname: &RootSlot,
    truename: &RootSlot,
    at: usize,
) -> Result<[(Word, Word); 5], EvalError> {
    let generics = runtime.root_slot(
        runtime
            .intern_package(crate::package::Package::MOP, "*FILE-GENERICS*", at)?
            .0,
    );
    let [package, readtable, file, real] = loader::file_bindings(
        runtime,
        "*COMPILE-FILE-PATHNAME*",
        "*COMPILE-FILE-TRUENAME*",
        pathname,
        truename,
        at,
    )?;
    Ok([package, readtable, file, real, (generics.get(), Word::NIL)])
}

fn compile_host_source(
    runtime: &Runtime,
    source: &SourceDocument,
    pathname: &RootSlot,
    truename: &RootSlot,
) -> Result<CompiledFileBuilder, EvalError> {
    let bindings = compilation_bindings(runtime, pathname, truename, 0)?;
    crate::vm::with_special_bindings(runtime, bindings, || compile_source_forms(runtime, source))
        .map_err(|fault| {
            let mut error = fault
                .host
                .in_document(source)
                .with_context(EvalErrorContext::Stage(EvalStage::Compile));
            if let Some(path) = source.path() {
                error = error.in_file(path);
            }
            error
        })
}

/// Compile a Lisp source file via the VM continuation dispatcher, binding
/// `*COMPILE-FILE-PATHNAME*` / `*COMPILE-FILE-TRUENAME*` and signalling
/// compiler warnings as `SIMPLE-WARNING` conditions.
pub(crate) fn start_compile_lisp<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    resume: Resume,
    at: usize,
) -> Result<Box<dyn Continuation + 'rt>, Fault> {
    let [output, verbose, print, format] = crate::vm::named_keyword_arguments(
        runtime,
        frame,
        args.start + 1..args.end,
        ["OUTPUT-FILE", "VERBOSE", "PRINT", "EXTERNAL-FORMAT"],
        at,
    )?;
    let format = ExternalFormat::parse(
        runtime,
        format.map(|index| frame.get(index)),
        "COMPILE-FILE",
        at,
    )?;
    let verbose = loader::file_flag(runtime, frame, verbose, "*COMPILE-VERBOSE*", at)?;
    let print = loader::file_flag(runtime, frame, print, "*COMPILE-PRINT*", at)?;
    let requested = loader::designator(runtime, frame.get(args.start), at)?
        .merge(&pathname::default_pathname(runtime, at)?);
    let output = output
        .map(|index| frame.get(index))
        .filter(|&word| word != Word::NIL)
        .map(|word| loader::designator(runtime, word, at))
        .transpose()?;
    let files = prepare_compile_file(runtime, &requested, output.as_ref(), verbose, format, at)?;
    let builder = CompiledFileBuilder::new_document(runtime, &files.source);
    let compiler = FileCompiler::in_document(runtime, &files.source);
    Ok(Box::new(CompileFileContinuation {
        files,
        cursor: SourceReadCursor::default(),
        compiler,
        builder: Some(builder),
        pending_warnings: Vec::new().into_iter(),
        warnings_p: false,
        failure_p: false,
        print,
        resume,
        at,
    }))
}

struct CompileFileContinuation<'rt> {
    files: CompilationFiles,
    cursor: SourceReadCursor,
    compiler: FileCompiler<'rt>,
    builder: Option<CompiledFileBuilder>,
    pending_warnings: std::vec::IntoIter<compile::Warning>,
    warnings_p: bool,
    failure_p: bool,
    print: bool,
    resume: Resume,
    at: usize,
}

impl Continuation for CompileFileContinuation<'_> {
    fn on_start<'a>(
        &mut self,
        runtime: &'a Runtime,
        frame: usize,
        control: &mut ControlStack<'a>,
    ) -> Result<(), Fault> {
        let bindings = compilation_bindings(
            runtime,
            &self.files.input_pathname,
            &self.files.input_truename,
            self.at,
        )?;
        control.bind(frame, &runtime.heap, bindings.into_iter());
        Ok(())
    }

    fn on_done(&mut self, control: &mut ControlStack<'_>) {
        control.unbind(1);
    }

    fn step(
        &mut self,
        runtime: &Runtime,
        _values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        self.advance(runtime, None, None)
    }

    fn step_in_vm<'rt>(
        &mut self,
        runtime: &'rt Runtime,
        exec: &mut crate::vm::ExecState<'rt>,
        _returned: bool,
    ) -> callback::Outcome<Result<ContinuationStep, Fault>> {
        let outcome = callback::run(runtime, exec, |expand, invoke| {
            self.advance(runtime, expand, invoke)
        });
        match outcome {
            callback::Outcome::Unhandled(error) => callback::Outcome::Unhandled(
                error
                    .in_document(&self.files.source)
                    .in_file(&self.files.input_path)
                    .with_context(EvalErrorContext::Stage(EvalStage::Compile)),
            ),
            outcome => outcome,
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

impl CompileFileContinuation<'_> {
    fn advance(
        &mut self,
        runtime: &Runtime,
        call_expander: Option<&CallExpander<'_>>,
        call: Option<&callback::CallValues<'_>>,
    ) -> Result<ContinuationStep, Fault> {
        loop {
            if let Some(warning) = self.pending_warnings.next() {
                self.warnings_p = true;
                // These compiler diagnostics are signalled through WARN as
                // non-style conditions by signal_compiler_warning.
                self.failure_p = true;
                return evaluate::signal_compiler_warning(runtime, &warning);
            }
            let form = self
                .cursor
                .next_form(
                    runtime,
                    self.files.source.text().expect("compilation source"),
                )
                .map_err(|error| {
                    let error = error
                        .in_document(&self.files.source)
                        .in_file(&self.files.input_path)
                        .with_context(EvalErrorContext::Stage(EvalStage::Compile));
                    if error.kind() == EvalErrorKind::Read {
                        let (class, error) = error.in_lisp_read(self.at, 0);
                        Fault::plain(class, error)
                    } else {
                        error.into()
                    }
                })?;
            let Some(form) = form else {
                let builder = self.builder.take().expect("FASL builder active");
                let path = write_fasl(runtime, builder, &self.files, self.at)?;
                let truename = loader::host_pathname(&path)?.to_lisp_value(runtime, self.at)?;
                let truth = |flag| if flag { Word::T } else { Word::NIL };
                return Ok(ContinuationStep::DoneValues(vec![
                    truename.slot.get(),
                    truth(self.warnings_p),
                    truth(self.failure_p),
                ]));
            };
            if self.print {
                crate::vm::streams::write_text(
                    runtime,
                    Word::NIL,
                    &format!("; Compiling form at byte {}\n", form.at),
                    self.at,
                )?;
            }
            let warnings_before = runtime.signalled_warnings.get();
            let builder = self.builder.as_mut().expect("FASL builder active");
            let warnings = builder
                .compile_form(runtime, &mut self.compiler, &form, call_expander, call)
                .map_err(|error| {
                    error
                        .in_document(&self.files.source)
                        .in_file(&self.files.input_path)
                        .with_context(EvalErrorContext::Stage(EvalStage::Compile))
                })?;
            let warnings_after = runtime.signalled_warnings.get();
            self.warnings_p |= warnings_before != warnings_after;
            self.failure_p |= warnings_before.non_style != warnings_after.non_style;
            self.pending_warnings = warnings.into_iter();
        }
    }
}

fn prepare_compile_file(
    runtime: &Runtime,
    requested: &Pathname,
    output: Option<&Pathname>,
    verbose: bool,
    format: ExternalFormat,
    at: usize,
) -> Result<CompilationFiles, Fault> {
    let mut input = loader::open_file(runtime, requested, false, at)?
        .expect("compilation requires an existing file");
    let source = input.take_source(runtime, format, at)?;
    let output = pathname::compiled_output(requested, output);
    let output_pathname = output.to_lisp_value(runtime, at)?.slot;
    let output_path = loader::physical_file_path(runtime, output, &output_pathname, at)?;
    if verbose {
        crate::vm::streams::write_text(
            runtime,
            Word::NIL,
            &format!("; Compiling {}\n", input.path.display()),
            at,
        )?;
    }
    let source = SourceDocument::file(input.path.clone(), Some(source));
    Ok(CompilationFiles {
        input_path: input.path,
        input_pathname: input.pathname,
        input_truename: input.truename,
        output_path,
        output_pathname,
        source,
    })
}

fn compile_source_forms(
    runtime: &Runtime,
    document: &SourceDocument,
) -> Result<CompiledFileBuilder, Fault> {
    let source = document.text().ok_or_else(|| {
        EvalError::new(0, "source document has no text").without_source_location()
    })?;
    let mut reader = Reader::new(runtime, source);
    let mut compiler = FileCompiler::in_document(runtime, document);
    let mut builder = CompiledFileBuilder::new_document(runtime, document);
    while let Some(form) = reader.next_form()? {
        for warning in builder.compile_form(runtime, &mut compiler, &form, None, None)? {
            evaluate::signal_host_warning(runtime, &warning)?;
        }
    }
    Ok(builder)
}

fn write_fasl(
    runtime: &Runtime,
    builder: CompiledFileBuilder,
    files: &CompilationFiles,
    at: usize,
) -> Result<PathBuf, Fault> {
    let compiled = builder
        .finish(runtime)
        .map_err(|error| error.in_file(&files.input_path))?;
    let output_path = &files.output_path;
    let bytes = compiled.encode().map_err(|error| {
        Fault::file_error(runtime, 
            files.output_pathname.get(),
            EvalError::from_error(at, error)
                .with_context(EvalErrorContext::Artifact(output_path.clone()))
                .with_context(EvalErrorContext::Stage(EvalStage::Compile)),
        )
    })?;
    write_fasl_atomically(output_path, &bytes)
        .and_then(|()| output_path.canonicalize())
        .map_err(|error| {
            Fault::file_error(runtime, 
                files.output_pathname.get(),
                EvalError::io_error(at, error)
                    .with_context(EvalErrorContext::Artifact(output_path.clone()))
                    .with_context(EvalErrorContext::Stage(EvalStage::Compile)),
            )
        })
}

pub(crate) fn write_fasl_atomically(output: &Path, bytes: &[u8]) -> io::Result<()> {
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);
    let (temporary_path, file) = loop {
        let id = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary = output.with_file_name(format!(".zkcl-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let temporary_path = DropGuard::new(temporary_path, |path| {
        let _ = fs::remove_file(path);
    });
    // Close the file before removing it, including when a write fails.
    let mut file = file;
    file.write_all(bytes)?;
    file.flush()?;
    drop(file);
    fs::rename(&*temporary_path, output)?;
    DropGuard::dismiss(temporary_path);
    Ok(())
}

/// Compile a host buffer while retaining its exact source revision.
pub(crate) fn compile_host_document(
    runtime: &Runtime,
    document: &SourceDocument,
) -> Result<CompiledFile, EvalError> {
    compile_source_forms(runtime, document)
        .and_then(|builder| builder.finish(runtime).map_err(Fault::from))
        .map_err(|fault| {
            let mut error = fault
                .host
                .in_document(document)
                .with_context(EvalErrorContext::Stage(EvalStage::Compile));
            if let Some(path) = document.path() {
                error = error.in_file(path);
            }
            error
        })
}
