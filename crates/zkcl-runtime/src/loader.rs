use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

use zkcl_bytecode::{CompiledFile, CompiledFileError};

use crate::condition::{Class, Fault};
use crate::encoding::ExternalFormat;
use crate::fasl::CompiledFileLoader;
use crate::heap::{ReadtableCase, RootSlot, RootSlots, Word};
use crate::package::Package;
use crate::pathname::{self, Pathname};
use crate::vm::{named_keyword_arguments, streams};
use crate::{EvalError, EvalErrorContext, EvalStage, Runtime, SourceDocument};

pub(crate) struct LoadFile {
    pub(crate) contents: FileContents,
    pub(crate) pathname: RootSlot,
    pub(crate) truename: RootSlot,
    pub(crate) print: bool,
}

pub(crate) enum FileContents {
    Source(SourceDocument),
    EmbeddedSource(&'static str),
    SourceStream(RootSlot),
    ProtocolStream {
        stream: RootSlot,
        inspect_type: bool,
        verbose: bool,
    },
    Compiled(Box<CompiledFileLoader>),
}

impl FileContents {
    pub(crate) fn compiled(runtime: &Runtime, bytes: &[u8]) -> Result<Self, CompiledFileError> {
        let compiled = CompiledFile::decode_for_target(bytes, runtime.compilation_target())?;
        Ok(Self::Compiled(Box::new(CompiledFileLoader::new(
            runtime,
            compiled,
            runtime.aot_module_for(bytes),
        ))))
    }
}

impl LoadFile {
    pub(crate) fn embedded_source(source: &'static str) -> Self {
        Self::in_memory(FileContents::EmbeddedSource(source))
    }

    pub(crate) fn in_memory(contents: FileContents) -> Self {
        Self {
            contents,
            pathname: RootSlot::nil(),
            truename: RootSlot::nil(),
            print: false,
        }
    }
}

pub(crate) struct OpenFile {
    bytes: Vec<u8>,
    pub(crate) path: PathBuf,
    pub(crate) pathname: RootSlot,
    pub(crate) truename: RootSlot,
}

impl OpenFile {
    pub(crate) fn take_source(
        &mut self,
        runtime: &Runtime,
        format: ExternalFormat,
        at: usize,
    ) -> Result<String, Fault> {
        format
            .decode_owned(std::mem::take(&mut self.bytes))
            .map_err(|error| {
                Fault::file_error(
                    runtime,
                    self.pathname.get(),
                    EvalError::file_format(at, &self.path, error),
                )
            })
    }

    fn into_load(
        mut self,
        runtime: &Runtime,
        compiled: bool,
        print: bool,
        format: Option<Word>,
        at: usize,
    ) -> Result<LoadFile, Fault> {
        let contents = if compiled || CompiledFile::has_magic(&self.bytes) {
            FileContents::compiled(runtime, &self.bytes).map_err(|error| {
                Fault::file_error(runtime, 
                    self.pathname.get(),
                    EvalError::from_error(at, error)
                        .with_context(EvalErrorContext::Artifact(self.path.clone()))
                        .with_context(EvalErrorContext::Stage(EvalStage::Load)),
                )
            })?
        } else {
            let format = ExternalFormat::parse(runtime, format, "LOAD", at)?;
            let text = self.take_source(runtime, format, at)?;
            FileContents::Source(SourceDocument::file(self.path, Some(text)))
        };
        Ok(LoadFile {
            contents,
            pathname: self.pathname,
            truename: self.truename,
            print,
        })
    }
}

pub(crate) fn prepare_host_load(runtime: &Runtime, path: &Path) -> Result<LoadFile, EvalError> {
    let requested = host_pathname(path)?;
    let file = prepare_file_load(runtime, &requested, false, false, None, 0).map_err(|error| {
        error
            .host
            .without_source_location()
            .with_context(EvalErrorContext::Stage(EvalStage::Load))
    })?;
    Ok(file.expect("a host load requires an existing file"))
}

pub(crate) fn prepare_lisp_load(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Option<LoadFile>, Fault> {
    let [verbose, print, missing_ok, format] = named_keyword_arguments(
        runtime,
        frame,
        args.start + 1..args.end,
        ["VERBOSE", "PRINT", "IF-DOES-NOT-EXIST", "EXTERNAL-FORMAT"],
        at,
    )?;
    let verbose = file_flag(runtime, frame, verbose, "*LOAD-VERBOSE*", at)?;
    let print = file_flag(runtime, frame, print, "*LOAD-PRINT*", at)?;
    let missing_ok = missing_ok.is_some_and(|index| frame.get(index) == Word::NIL);
    let argument = frame.get(args.start);
    if streams::gray::streamp(runtime, argument) {
        let mut file = load_stream(runtime, argument, print, at)?;
        if let FileContents::ProtocolStream {
            verbose: pending, ..
        } = &mut file.contents
        {
            *pending = verbose;
        } else if verbose {
            crate::vm::streams::write_text(runtime, Word::NIL, "; Loading stream\n", at)?;
        }
        return Ok(Some(file));
    }
    let defaults = pathname::default_pathname(runtime, at)?;
    let requested = designator(runtime, argument, at)?.merge(&defaults);
    let format = format.map(|reg| runtime.root_slot(frame.get(reg)));
    let file = prepare_file_load(runtime, &requested, print, missing_ok, format.as_ref(), at)?;
    if verbose && let Some(file) = &file {
        let name = pathname::from_word(runtime, file.pathname.get())
            .expect("opened file pathname")
            .namestring()
            .expect("an opened file has a namestring");
        crate::vm::streams::write_text(runtime, Word::NIL, &format!("; Loading {name}\n"), at)?;
    }
    Ok(file)
}

/// Open the translation definition file for an already defined logical
/// pathname host. The file is `<HOST>.translations` in the directory named by
/// `*DEFAULT-PATHNAME-DEFAULTS*`; loading it installs the host translations.
pub(crate) fn prepare_translations_load(
    runtime: &Runtime,
    host_word: Word,
    at: usize,
) -> Result<Option<LoadFile>, Fault> {
    let invalid = || {
        Fault::type_error(runtime, 
            host_word,
            "STRING",
            EvalError::new(at, "expected a logical pathname host name"),
        )
    };
    let text = runtime
        .heap
        .string_contents(host_word)
        .ok_or_else(invalid)?;
    let host = text.to_ascii_uppercase();
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(invalid());
    }
    if pathname::logical_host(runtime, &host).is_none() {
        return Err(Fault::type_error(runtime, 
            host_word,
            "STRING",
            EvalError::new(at, format!("no logical pathname host named {host}")),
        ));
    }
    let defaults = pathname::default_pathname(runtime, at)?;
    let mut file = if defaults.logical_host.is_some() {
        pathname::translate(runtime, defaults, at)?
    } else {
        defaults
    };
    file.name = pathname::Leaf::Text(host.to_ascii_lowercase());
    file.file_type = pathname::Leaf::Text("translations".to_owned());
    file.version = pathname::Version::Newest;
    prepare_file_load(runtime, &file, false, true, None, at)
}

/// A file stream is a pathname designator for the file it is connected to.
pub(crate) fn designator(runtime: &Runtime, word: Word, at: usize) -> Result<Pathname, Fault> {
    if let Some(info) = runtime.heap.stream_info(word)
        && info.kind.file()
        && let Some(pathname) = pathname::from_word(runtime, info.contents)
    {
        return Ok(pathname);
    }
    pathname::designator(runtime, word, at)
}

fn load_stream(runtime: &Runtime, stream: Word, print: bool, at: usize) -> Result<LoadFile, Fault> {
    let source = runtime.root_slot(stream);
    if streams::gray::uses_protocol(runtime, stream, at)? {
        return Ok(LoadFile {
            contents: FileContents::ProtocolStream {
                stream: source,
                inspect_type: true,
                verbose: false,
            },
            pathname: runtime.root_slot(Word::NIL),
            truename: runtime.root_slot(Word::NIL),
            print,
        });
    }
    let input = streams::input_stream(runtime, stream, at)?;
    let pathname =
        runtime.root_slot(pathname::file_stream_pathname(runtime, input).unwrap_or(Word::NIL));
    let truename = if pathname.get() == Word::NIL {
        runtime.root_slot(Word::NIL)
    } else {
        let name = pathname::designator(runtime, pathname.get(), at)?;
        let path = physical_file_path(runtime, name, &pathname, at)?;
        let full = fs::canonicalize(&path).map_err(|error| {
            Fault::file_error(runtime, pathname.get(), EvalError::file_io(at, &path, error))
        })?;
        file_truename(runtime, &full, &pathname, at)?
    };
    let contents = match streams::read_fasl_octets(runtime, &source, at)? {
        Some(bytes) => compiled_stream(runtime, &source, &bytes, at)?,
        None => FileContents::SourceStream(source),
    };
    Ok(LoadFile {
        contents,
        pathname,
        truename,
        print,
    })
}

pub(crate) fn compiled_stream(
    runtime: &Runtime,
    stream: &RootSlot,
    bytes: &[u8],
    at: usize,
) -> Result<FileContents, Fault> {
    FileContents::compiled(runtime, bytes).map_err(|error| {
        Fault::stream_error(runtime, 
            Class::StreamError,
            stream.get(),
            EvalError::from_error(at, error),
        )
    })
}

pub(crate) fn host_pathname(path: &Path) -> Result<Pathname, EvalError> {
    let absolute = std::path::absolute(path).map_err(|source| {
        EvalError::file_io(0, path, source)
            .without_source_location()
            .with_condition(Class::FileError)
    })?;
    Pathname::from_path(&absolute).map_err(|error| {
        EvalError::file_format(0, path, error)
            .without_source_location()
            .with_condition(Class::FileError)
    })
}

pub(crate) fn file_flag(
    runtime: &Runtime,
    frame: &RootSlots,
    index: Option<usize>,
    special: &str,
    at: usize,
) -> Result<bool, Fault> {
    let value = match index {
        Some(index) => frame.get(index),
        None => {
            let symbol = runtime.intern_common_lisp(special, at)?;
            runtime.heap.symbol_cell(symbol, false).ok_or_else(|| {
                Fault::cell(runtime, 
                    Class::UnboundVariable,
                    symbol,
                    EvalError::new(at, format!("unbound variable {special}")),
                )
            })?
        }
    };
    Ok(value != Word::NIL)
}

fn prepare_file_load(
    runtime: &Runtime,
    requested: &Pathname,
    print: bool,
    missing_ok: bool,
    format: Option<&RootSlot>,
    at: usize,
) -> Result<Option<LoadFile>, Fault> {
    let defaulted = if matches!(requested.file_type, pathname::Leaf::Missing)
        && matches!(requested.name, pathname::Leaf::Text(_))
    {
        default_load_pathname(runtime, requested, at)?
    } else {
        None
    };
    let selected = defaulted.as_ref().unwrap_or(requested);
    let compiled = matches!(&selected.file_type, pathname::Leaf::Text(kind) if kind.eq_ignore_ascii_case(pathname::COMPILED_FILE_TYPE));
    open_file(runtime, selected, missing_ok, at)?
        .map(|file| file.into_load(runtime, compiled, print, format.map(RootSlot::get), at))
        .transpose()
}

// LOAD first honors an existing extensionless file. Otherwise select the
// newer source or FASL, preferring the compiled file when timestamps tie.
fn default_load_pathname(
    runtime: &Runtime,
    requested: &Pathname,
    at: usize,
) -> Result<Option<Pathname>, Fault> {
    let modified = |candidate: &Pathname| -> Result<Option<std::time::SystemTime>, Fault> {
        let object = candidate.to_lisp_value(runtime, at)?;
        let path = physical_file_path(runtime, candidate.clone(), &object.slot, at)?;
        match fs::metadata(&path).and_then(|m| m.modified()) {
            Ok(time) => Ok(Some(time)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Fault::file_error(runtime, 
                object.slot.get(),
                EvalError::file_io(at, &path, e),
            )),
        }
    };
    if modified(requested)?.is_some() {
        return Ok(None);
    }
    let compiled = pathname::compiled_output(requested, None);
    let mut source = requested.clone();
    source.file_type = pathname::Leaf::Text(
        if source.logical_host.is_some() {
            "LISP"
        } else {
            "lisp"
        }
        .into(),
    );
    let fasl_date = modified(&compiled)?;
    let source_date = modified(&source)?;
    Ok(if source_date > fasl_date {
        Some(source)
    } else {
        fasl_date.map(|_| compiled)
    })
}

pub(crate) fn open_file(
    runtime: &Runtime,
    requested: &Pathname,
    missing_ok: bool,
    at: usize,
) -> Result<Option<OpenFile>, Fault> {
    let pathname = requested.to_lisp_value(runtime, at)?;
    let path = physical_file_path(runtime, requested.clone(), &pathname.slot, at)?;
    let opened =
        fs::canonicalize(&path).and_then(|full| fs::read(&full).map(|bytes| (full, bytes)));
    let (full, bytes) = match opened {
        Ok(value) => value,
        Err(source) if missing_ok && source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Fault::file_error(runtime, 
                pathname.slot.get(),
                EvalError::file_io(at, &path, source),
            ));
        }
    };
    let truename = file_truename(runtime, &full, &pathname.slot, at)?;
    Ok(Some(OpenFile {
        bytes,
        path: full,
        pathname: pathname.slot,
        truename,
    }))
}

pub(super) fn physical_file_path(
    runtime: &Runtime,
    name: Pathname,
    pathname: &RootSlot,
    at: usize,
) -> Result<PathBuf, Fault> {
    pathname::translate(runtime, name, at)?
        .file_path()
        .map_err(|error| Fault::file_error(runtime, pathname.get(), EvalError::from_error(at, error)))
}

fn file_truename(
    runtime: &Runtime,
    full: &Path,
    pathname: &RootSlot,
    at: usize,
) -> Result<RootSlot, Fault> {
    let name = Pathname::from_path(full)
        .map_err(|error| Fault::file_error(runtime, pathname.get(), EvalError::from_error(at, error)))?;
    Ok(name.to_lisp_value(runtime, at)?.slot)
}

pub(crate) fn bindings(
    runtime: &Runtime,
    file: &LoadFile,
    at: usize,
) -> Result<impl ExactSizeIterator<Item = (Word, Word)>, EvalError> {
    // Shipped source uses the standard reader, including read-time evaluation.
    // The VM restores the caller's reader configuration when the load exits.
    let embedded = matches!(file.contents, FileContents::EmbeddedSource(_));
    let readtable = embedded
        .then(|| runtime.heap.readtable(ReadtableCase::Upcase))
        .transpose()
        .map_err(|error| EvalError::out_of_memory(at, error))?
        .map(|table| runtime.root_slot(table));
    let [mut package, mut table, pathname, truename] = file_bindings(
        runtime,
        "*LOAD-PATHNAME*",
        "*LOAD-TRUENAME*",
        &file.pathname,
        &file.truename,
        at,
    )?;
    let (read_base, read_eval, suppress, len) = if let Some(readtable) = readtable {
        let read_base = runtime.intern_common_lisp("*READ-BASE*", at)?;
        let read_eval = runtime.intern_common_lisp("*READ-EVAL*", at)?;
        let suppress = runtime.intern_common_lisp("*READ-SUPPRESS*", at)?;
        package.1 = Package::COMMON_LISP.word();
        table.1 = readtable.get();
        (
            (read_base, Word::fixnum(10).unwrap()),
            (read_eval, Word::T),
            (suppress, Word::NIL),
            7,
        )
    } else {
        (
            (Word::NIL, Word::NIL),
            (Word::NIL, Word::NIL),
            (Word::NIL, Word::NIL),
            4,
        )
    };
    Ok([
        package, table, pathname, truename, read_base, read_eval, suppress,
    ]
    .into_iter()
    .take(len))
}

pub(crate) fn file_bindings(
    runtime: &Runtime,
    pathname_name: &str,
    truename_name: &str,
    file_pathname: &RootSlot,
    file_truename: &RootSlot,
    at: usize,
) -> Result<[(Word, Word); 4], EvalError> {
    let package = runtime.intern_common_lisp("*PACKAGE*", at)?;
    let readtable = runtime.intern_common_lisp("*READTABLE*", at)?;
    let pathname = runtime.intern_common_lisp(pathname_name, at)?;
    let truename = runtime.intern_common_lisp(truename_name, at)?;
    Ok([
        (
            package,
            runtime
                .heap
                .symbol_cell(package, false)
                .unwrap_or(Word::UNBOUND),
        ),
        (
            readtable,
            runtime
                .heap
                .symbol_cell(readtable, false)
                .unwrap_or(Word::UNBOUND),
        ),
        (pathname, file_pathname.get()),
        (truename, file_truename.get()),
    ])
}
