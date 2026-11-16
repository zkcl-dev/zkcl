//! Implementation-defined environment, time, and documentation queries.

use std::io;
use std::ops::Range;
use std::time::Duration;

use num_bigint::BigInt;
use num_traits::ToPrimitive;

use super::Values;
use crate::condition::{Class, Fault};
use crate::heap::{RootSlot, RootSlots, Word};
use crate::number::{Number, Real};
use crate::package::Package;
use crate::read::{Form, FormKind};
use crate::symbol::Name;
use crate::{EvalError, Runtime, TracedFunction};

#[derive(Clone, Default)]
pub(crate) struct TraceOptions(Vec<(String, Form)>);

impl TraceOptions {
    pub(crate) fn write_image(
        &self,
        image: &mut crate::image::ImageWriter<'_>,
    ) -> Result<(), EvalError> {
        image.usize(self.0.len());
        for (key, form) in &self.0 {
            image.string(key);
            image.form(form)?;
        }
        Ok(())
    }

    pub(crate) fn read_image(image: &mut crate::image::ImageReader<'_>) -> Result<Self, EvalError> {
        let count = image.usize()?;
        let mut options = Vec::new();
        options
            .try_reserve(count)
            .map_err(|_| crate::image::invalid())?;
        for _ in 0..count {
            let key = image.string()?;
            if !matches!(
                key.as_str(),
                "CONDITION"
                    | "CONDITION-AFTER"
                    | "CONDITION-ALL"
                    | "BREAK"
                    | "BREAK-AFTER"
                    | "BREAK-ALL"
                    | "PRINT"
                    | "PRINT-AFTER"
                    | "PRINT-ALL"
                    | "REPORT"
            ) {
                return Err(crate::image::invalid());
            }
            let form = image.form()?;
            if key == "REPORT"
                && !form
                    .name()
                    .is_some_and(|name| matches!(name.operator(), "NIL" | "TRACE"))
            {
                return Err(crate::image::invalid());
            }
            options.push((key, form));
        }
        Ok(Self(options))
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Apropos,
    AproposList,
    Describe,
    Inspect,
    Room,
    Disassemble,
    Ed,
    Dribble,
    MachineType,
    MachineVersion,
    MachineInstance,
    SoftwareType,
    SoftwareVersion,
    ShortSiteName,
    LongSiteName,
    UserHomedirPathname,
    GetUniversalTime,
    GetDecodedTime,
    DecodeUniversalTime,
    EncodeUniversalTime,
    GetInternalRealTime,
    GetInternalRunTime,
    Sleep,
    Trace,
    Untrace,
}

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Apropos | Self::AproposList => (1..=2).contains(&count),
            Self::Describe => (1..=2).contains(&count),
            Self::Inspect => count == 1,
            Self::Room | Self::Ed | Self::Dribble => count <= 1,
            Self::Disassemble | Self::Sleep => count == 1,
            Self::MachineType
            | Self::MachineVersion
            | Self::MachineInstance
            | Self::SoftwareType
            | Self::SoftwareVersion
            | Self::ShortSiteName
            | Self::LongSiteName
            | Self::GetUniversalTime
            | Self::GetDecodedTime
            | Self::GetInternalRealTime
            | Self::GetInternalRunTime => count == 0,
            Self::UserHomedirPathname => count <= 1,
            Self::DecodeUniversalTime => (1..=2).contains(&count),
            Self::EncodeUniversalTime => (6..=7).contains(&count),
            Self::Trace | Self::Untrace => true,
        }
    }
}

pub(crate) const PRIMITIVES: &[(&str, Op)] = &[
    ("APROPOS", Op::Apropos),
    ("APROPOS-LIST", Op::AproposList),
    ("DESCRIBE", Op::Describe),
    ("INSPECT", Op::Inspect),
    ("ROOM", Op::Room),
    ("DISASSEMBLE", Op::Disassemble),
    ("ED", Op::Ed),
    ("DRIBBLE", Op::Dribble),
    ("MACHINE-TYPE", Op::MachineType),
    ("MACHINE-VERSION", Op::MachineVersion),
    ("MACHINE-INSTANCE", Op::MachineInstance),
    ("SOFTWARE-TYPE", Op::SoftwareType),
    ("SOFTWARE-VERSION", Op::SoftwareVersion),
    ("SHORT-SITE-NAME", Op::ShortSiteName),
    ("LONG-SITE-NAME", Op::LongSiteName),
    ("USER-HOMEDIR-PATHNAME", Op::UserHomedirPathname),
    ("GET-UNIVERSAL-TIME", Op::GetUniversalTime),
    ("GET-DECODED-TIME", Op::GetDecodedTime),
    ("DECODE-UNIVERSAL-TIME", Op::DecodeUniversalTime),
    ("ENCODE-UNIVERSAL-TIME", Op::EncodeUniversalTime),
    ("GET-INTERNAL-REAL-TIME", Op::GetInternalRealTime),
    ("GET-INTERNAL-RUN-TIME", Op::GetInternalRunTime),
    ("SLEEP", Op::Sleep),
    ("TRACE", Op::Trace),
    ("UNTRACE", Op::Untrace),
];

const UNIX_EPOCH_UNIVERSAL: i64 = 2_208_988_800;

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let arg = |index: usize| frame.get(args.start + index);
    let count = args.len();
    match op {
        Op::Trace => {
            trace_functions(runtime, frame, args.clone(), values, at)?;
        }
        Op::Untrace => {
            untrace_functions(runtime, frame, args.clone(), at)?;
            values.resize(0);
        }
        Op::Apropos | Op::AproposList => {
            let text = runtime.string_designator_text(arg(0)).ok_or_else(|| {
                Fault::string_designator(runtime, arg(0), EvalError::new(at, "expected a string designator"))
            })?;
            let package = if count == 2 {
                apropos_package(runtime, arg(1), at)?
            } else {
                None
            };
            let mut symbols = apropos_symbols(runtime, package, &text, at)?;
            if matches!(op, Op::AproposList) {
                let mut list = runtime.root_slot(Word::NIL);
                for symbol in symbols.into_iter().rev() {
                    let next = runtime
                        .heap
                        .cons(|| (symbol.get(), list.get()))
                        .map_err(|error| EvalError::out_of_memory(at, error))?;
                    list = runtime.root_slot(next);
                }
                values.one(list.get());
            } else {
                symbols.sort_by_key(|symbol| runtime.heap.symbol_name(symbol.get()));
                let mut output = String::new();
                for symbol in symbols {
                    if let Some(name) = runtime.heap.symbol_name(symbol.get()) {
                        output.push_str(&name);
                        output.push('\n');
                    }
                }
                super::streams::write_text(runtime, Word::NIL, &output, at)?;
                values.resize(0);
            }
        }
        Op::Describe => {
            let stream = if count == 2 { arg(1) } else { Word::NIL };
            let object = runtime.root(frame.get(args.start));
            let representation = runtime.repr(&object)?;
            let class = super::classes::class_of(runtime, object.slot.get(), at)?;
            let name = runtime
                .heap
                .class_info(class)
                .and_then(|info| runtime.heap.symbol_name(info.name))
                .unwrap_or_else(|| "T".into());
            let text = format!("{representation}\n  is an object of type {name}\n");
            super::streams::write_text(runtime, stream, &text, at)?;
            values.resize(0);
        }
        Op::Inspect => {
            let object = runtime.root(frame.get(args.start));
            let text = runtime.repr(&object)?;
            super::streams::write_text(runtime, Word::NIL, &format!("{text}\n"), at)?;
            values.one(object.slot.get());
        }
        Op::Room => {
            let verbosity = if count == 0 { None } else { Some(arg(0)) };
            room(runtime, verbosity, at)?;
            values.resize(0);
        }
        Op::Disassemble => {
            let value = frame.get(args.start);
            if !disassemble_designator(runtime, value) {
                return Err(Fault::type_error(runtime, 
                    value,
                    "FUNCTION",
                    EvalError::new(at, "expected a function name or function"),
                ));
            }
            let object = runtime.root(value);
            let label = runtime.repr(&object)?;
            let target = if super::is_function(runtime, object.slot.get()) {
                Some(object.slot.get())
            } else {
                super::FunctionName::parse(runtime, object.slot.get(), at)
                    .ok()
                    .and_then(|name| name.get(runtime))
            };
            let bytecode = target
                .and_then(|target| runtime.heap.function_code(target))
                .and_then(|code| runtime.heap.function_info(code))
                .map(|info| runtime.heap.code_store.get(info.code_id));
            let mut text = String::new();
            if let Some(code) = bytecode {
                use std::fmt::Write as _;
                writeln!(
                    text,
                    "; {label} — zkcl bytecode, {} registers",
                    code.registers
                )
                .unwrap();
                for (pc, instruction) in code.canonical.iter().enumerate() {
                    writeln!(text, "{pc:04}  {instruction:?}").unwrap();
                }
            } else {
                text = format!("No zkcl bytecode is available for {label}.\n");
            }
            super::streams::write_text(runtime, Word::NIL, &text, at)?;
            values.resize(0);
        }
        Op::Dribble => {
            if count == 0 {
                runtime
                    .host_io
                    .stop_dribble()
                    .map_err(|error| EvalError::io_error(at, error))?;
            } else {
                if runtime.host_io.dribbling() {
                    return Err(Fault::plain(
                        Class::FileError,
                        EvalError::new(at, "DRIBBLE is already recording; call (DRIBBLE) first"),
                    ));
                }
                let path = crate::loader::designator(runtime, arg(0), at)?;
                let path = crate::pathname::translate(runtime, path, at)?
                    .file_path()
                    .map_err(|error| Fault::file_error(runtime, arg(0), EvalError::from_error(at, error)))?;
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|error| {
                        Fault::file_error(runtime, arg(0), EvalError::file_io(at, &path, error))
                    })?;
                runtime
                    .host_io
                    .start_dribble(file)
                    .map_err(|error| EvalError::io_error(at, error))?;
            }
            values.one(Word::NIL);
        }
        Op::Ed => {
            edit(runtime, (count == 1).then(|| arg(0)), at)?;
            values.one(Word::NIL);
        }
        Op::MachineType | Op::SoftwareType => {
            if let Some(machine) = runtime.compilation_target().machine() {
                let description = if matches!(op, Op::MachineType) {
                    machine.architecture()
                } else {
                    machine.operating_system()
                };
                string(runtime, description, values, at)?;
            } else {
                values.one(Word::NIL);
            }
        }
        Op::MachineVersion | Op::MachineInstance | Op::SoftwareVersion => values.one(Word::NIL),
        Op::ShortSiteName | Op::LongSiteName => string(runtime, "zkcl", values, at)?,
        Op::UserHomedirPathname => {
            let mut home = std::env::var_os("HOME")
                .map(|home| home.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".into());
            if !home.ends_with('/') {
                home.push('/');
            }
            let path =
                crate::pathname::parse_namestring(runtime, &home, None, None).map_err(|error| {
                    Fault::plain(
                        crate::condition::Class::FileError,
                        EvalError::new(at, error.to_string()),
                    )
                })?;
            let word = path.to_lisp_value(runtime, at)?;
            values.one(word.slot.get());
        }
        Op::GetUniversalTime | Op::GetDecodedTime => {
            let unix = runtime
                .host_services
                .wall_time_secs()
                .map_err(|err| EvalError::io_error(at, err))?;
            let value = unix.saturating_add(UNIX_EPOCH_UNIVERSAL);
            if matches!(op, Op::GetUniversalTime) {
                integer(runtime, BigInt::from(value), values, at)?;
            } else {
                decode_universal(runtime, value, None, values, at)?;
            }
        }
        Op::DecodeUniversalTime => {
            let universal = integer_argument(runtime, arg(0), at)?;
            let zone = (count > 1).then(|| arg(1));
            if let Some(zone) = zone {
                real_argument(runtime, zone, at)?;
            }
            decode_universal(runtime, universal, zone, values, at)?;
        }
        Op::EncodeUniversalTime => {
            let second = integer_argument(runtime, arg(0), at)?;
            let minute = integer_argument(runtime, arg(1), at)?;
            let hour = integer_argument(runtime, arg(2), at)?;
            let day = integer_argument(runtime, arg(3), at)?;
            let month = integer_argument(runtime, arg(4), at)?;
            let year = integer_argument(runtime, arg(5), at)?;
            if !(0..60).contains(&second)
                || !(0..60).contains(&minute)
                || !(0..24).contains(&hour)
                || !(1..32).contains(&day)
                || !(1..13).contains(&month)
            {
                return Err(Fault::plain(
                    crate::condition::Class::TypeError,
                    EvalError::new(at, "invalid decoded universal time components"),
                ));
            }
            let zone = if count > 6 {
                real_argument(runtime, arg(6), at)?
            } else {
                0.0
            };
            let days = days_from_civil(year, month as u32, day as u32);
            let unix = days
                .saturating_mul(86_400)
                .saturating_add(hour * 3600 + minute * 60 + second)
                .saturating_add((zone * 3600.0).round() as i64);
            integer(
                runtime,
                BigInt::from(unix.saturating_add(UNIX_EPOCH_UNIVERSAL)),
                values,
                at,
            )?;
        }
        Op::GetInternalRealTime => {
            let nanos = runtime
                .host_services
                .monotonic_nanos()
                .map_err(|err| EvalError::io_error(at, err))?;
            integer(runtime, BigInt::from(nanos), values, at)?;
        }
        Op::GetInternalRunTime => {
            let nanos = internal_run_time(runtime).map_err(|err| EvalError::io_error(at, err))?;
            integer(runtime, BigInt::from(nanos), values, at)?;
        }
        Op::Sleep => {
            let duration = sleep_duration(runtime, arg(0), at)?;
            runtime.sleep_for(duration, at)?;
            values.one(Word::NIL);
        }
    }
    Ok(())
}

pub(super) fn sleep_duration(runtime: &Runtime, value: Word, at: usize) -> Result<Duration, Fault> {
    let seconds = real_argument(runtime, value, at)?;
    if seconds < 0.0 || !seconds.is_finite() {
        return Err(Fault::plain(
            crate::condition::Class::TypeError,
            EvalError::new(at, "expected a non-negative real number"),
        ));
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_| EvalError::new(at, "SLEEP duration exceeds the host timer range").into())
}

fn room(runtime: &Runtime, verbosity: Option<Word>, at: usize) -> Result<(), Fault> {
    use std::fmt::Write;

    if let Some(word) = verbosity
        && !matches!(word, Word::NIL | Word::T)
        && !(runtime.heap.symbol_home(word) == Some(Package::KEYWORD)
            && runtime.heap.symbol_name(word).as_deref() == Some("DEFAULT"))
    {
        return Err(Fault::type_error(runtime, 
            word,
            "(MEMBER NIL T :DEFAULT)",
            EvalError::new(at, "expected NIL, T or :DEFAULT as the ROOM verbosity"),
        ));
    }

    let stats = runtime.heap_stats();
    let mut text = format!(
        "Managed heap has {} of {} bytes committed.\n",
        stats.committed_bytes, stats.allocation_limit_bytes,
    );
    if verbosity != Some(Word::NIL) {
        writeln!(
            text,
            "{} Immix blocks, {} bytes in large objects, {} collections.",
            stats.block_count, stats.large_object_bytes, stats.collections,
        )
        .unwrap();
        if stats.collections != 0 {
            writeln!(
                text,
                "Live after last collection: {} bytes.",
                stats.live_bytes
            )
            .unwrap();
        }
    }
    if verbosity == Some(Word::T) {
        writeln!(
            text,
            "Allocated since last collection: {} bytes.\n\
             Last collection released {} bytes and moved {} bytes in {} objects.\n\
             Evacuation reserve: {} bytes. Compaction retries: {}.",
            stats.allocated_bytes_since_collection,
            stats.last_reclaimed_bytes,
            stats.last_evacuated_bytes,
            stats.last_evacuated_objects,
            stats.evacuation_reserve_bytes,
            stats.compaction_retries,
        )
        .unwrap();
        if let Some(duration) = stats.last_collection_duration {
            writeln!(
                text,
                "Last GC pause: {:.3} ms.",
                duration.as_secs_f64() * 1000.0
            )
            .unwrap();
        }
    }
    super::streams::write_text(runtime, Word::NIL, &text, at)
}

fn edit(runtime: &Runtime, target: Option<Word>, at: usize) -> Result<(), Fault> {
    let path = match target.filter(|word| *word != Word::NIL) {
        None => None,
        Some(word) if runtime.heap.is_symbol(word) || runtime.heap.cons_fields(word).is_some() => {
            let name = runtime.repr(&runtime.root(word))?;
            let definitions = runtime.find_definitions(&name)?;
            let path = definitions.iter().find_map(|definition| {
                definition
                    .source
                    .as_ref()
                    .and_then(|source| source.path())
                    .map(std::path::Path::to_path_buf)
            });
            Some(path.ok_or_else(|| {
                Fault::file_error(runtime, 
                    word,
                    EvalError::new(at, format!("ED has no physical source file for {name}")),
                )
            })?)
        }
        Some(word) => {
            let path = crate::loader::designator(runtime, word, at)?;
            Some(
                crate::pathname::translate(runtime, path, at)?
                    .file_path()
                    .map_err(|error| Fault::file_error(runtime, word, EvalError::from_error(at, error)))?,
            )
        }
    };
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            #[cfg(not(target_os = "windows"))]
            let candidates = ["nvim", "vim", "nano", "vi"];
            #[cfg(target_os = "windows")]
            let candidates = ["notepad"];
            let paths = std::env::var_os("PATH")?;
            candidates
                .into_iter()
                .find(|candidate| {
                    std::env::split_paths(&paths).any(|directory| {
                        #[cfg(not(target_os = "windows"))]
                        {
                            directory.join(candidate).is_file()
                        }
                        #[cfg(target_os = "windows")]
                        {
                            directory.join(format!("{candidate}.exe")).is_file()
                        }
                    })
                })
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            Fault::plain(
                Class::FileError,
                EvalError::new(at, "ED cannot find an editor; set VISUAL or EDITOR"),
            )
        })?;
    #[cfg(not(target_os = "windows"))]
    let status = {
        let mut command = std::process::Command::new("sh");
        // The user's editor setting is a command, while the pathname is a
        // separate positional argument and never becomes shell source.
        command
            .arg("-c")
            .arg(if path.is_some() {
                format!("exec {editor} \"$1\"")
            } else {
                format!("exec {editor}")
            })
            .arg("zkcl-ed");
        if let Some(path) = &path {
            command.arg(path);
        }
        command.status()
    };
    #[cfg(target_os = "windows")]
    let status = {
        let mut parts = editor.split_whitespace();
        let mut command = std::process::Command::new(parts.next().expect("nonempty editor"));
        command.args(parts);
        if let Some(path) = &path {
            command.arg(path);
        }
        command.status()
    };
    let status = status.map_err(|error| EvalError::io_error(at, error))?;
    if !status.success() {
        return Err(Fault::plain(
            Class::FileError,
            EvalError::new(at, format!("ED editor exited with {status}")),
        ));
    }
    Ok(())
}

fn trace_functions(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    // Options before the first name apply to every name. Later options affect
    // only the immediately preceding name, following SBCL's TRACE convention.
    let mut global = TraceOptions::default();
    let mut specifications: Vec<(RootSlot, TraceOptions)> = Vec::new();
    let mut index = args.start;
    while index < args.end {
        let word = frame.get(index);
        if runtime
            .heap
            .symbol_attributes(word)
            .is_some_and(|a| a.home == Some(Package::KEYWORD))
        {
            let key = runtime.heap.symbol_name(word).expect("keyword name");
            index += 1;
            if index == args.end {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(at, "TRACE option requires a form"),
                ));
            }
            if !matches!(
                key.as_str(),
                "CONDITION"
                    | "CONDITION-AFTER"
                    | "CONDITION-ALL"
                    | "BREAK"
                    | "BREAK-AFTER"
                    | "BREAK-ALL"
                    | "PRINT"
                    | "PRINT-AFTER"
                    | "PRINT-ALL"
                    | "REPORT"
            ) {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(at, format!("unknown TRACE option :{key}")),
                ));
            }
            let form = crate::form_value::from_value(runtime, &runtime.root(frame.get(index)), at)?;
            if key == "REPORT"
                && !form
                    .name()
                    .is_some_and(|name| matches!(name.operator(), "NIL" | "TRACE"))
            {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(at, "TRACE :REPORT accepts TRACE or NIL"),
                ));
            }
            let options = specifications
                .last_mut()
                .map(|(_, options)| options)
                .unwrap_or(&mut global);
            options.0.push((key, form));
        } else {
            function_name(runtime, word, at)?;
            specifications.push((runtime.root_slot(word), global.clone()));
        }
        index += 1;
    }
    let mut selected = Vec::new();
    for (name, options) in specifications {
        enable_trace(runtime, name.get(), options, at)?;
        selected.push(name);
    }
    if args.is_empty() {
        for record in runtime.traced.borrow().iter() {
            selected.push(traced_name(runtime, &record.symbol, record.is_setf, at)?);
        }
    }
    let mut list = runtime.root_slot(Word::NIL);
    for name in selected.into_iter().rev() {
        let next = runtime
            .heap
            .cons(|| (name.get(), list.get()))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        list = runtime.root_slot(next);
    }
    values.one(list.get());
    Ok(())
}

fn enable_trace(
    runtime: &Runtime,
    name: Word,
    options: TraceOptions,
    at: usize,
) -> Result<(), Fault> {
    let (symbol, is_setf) = function_name(runtime, name, at)?;
    let symbol = runtime.root_slot(symbol);
    let position = runtime
        .traced
        .borrow()
        .iter()
        .position(|record| record.is_setf == is_setf && record.symbol.get() == symbol.get());
    if let Some(position) = position {
        let record = runtime.traced.borrow_mut().remove(position);
        restore_trace(runtime, &record, at)?;
    }
    let original = if is_setf {
        runtime.heap.symbol_setter(symbol.get())
    } else {
        runtime.heap.symbol_cell(symbol.get(), true)
    };
    let Some(original) = original else {
        return Err(undefined_trace(runtime, &symbol, is_setf, name, at));
    };
    let original = runtime.root_slot(original);
    let (wrapper, generic) = make_trace(runtime, &symbol, is_setf, &original, &options, at)?;
    if !generic {
        if is_setf {
            runtime.heap.set_symbol_setter(symbol.get(), wrapper.get());
        } else {
            runtime
                .heap
                .set_symbol_cell(symbol.get(), true, wrapper.get());
        }
    }
    runtime.traced.borrow_mut().push(TracedFunction {
        symbol,
        is_setf,
        original,
        wrapper,
        generic,
        options,
    });
    Ok(())
}

fn make_trace(
    runtime: &Runtime,
    symbol: &RootSlot,
    is_setf: bool,
    original: &RootSlot,
    options: &TraceOptions,
    at: usize,
) -> Result<(RootSlot, bool), Fault> {
    let display = trace_display(runtime, symbol.get(), is_setf);
    let generic = runtime.heap.generic_info(original.get()).is_some();
    let wrapper = if generic {
        // DEFINE-METHOD resolves its target by name. During a replacement the
        // incoming generic has not yet been installed by the caller.
        let previous = runtime.root_slot(
            if is_setf {
                runtime.heap.symbol_setter(symbol.get())
            } else {
                runtime.heap.symbol_cell(symbol.get(), true)
            }
            .unwrap_or(Word::UNBOUND),
        );
        if is_setf {
            runtime.heap.set_symbol_setter(symbol.get(), original.get());
        } else {
            runtime
                .heap
                .set_symbol_cell(symbol.get(), true, original.get());
        }
        let guard = std::mem::DropGuard::new((), |_| {
            if is_setf {
                runtime.heap.set_symbol_setter(symbol.get(), previous.get());
            } else {
                runtime
                    .heap
                    .set_symbol_cell(symbol.get(), true, previous.get());
            }
        });
        let method =
            install_generic_trace(runtime, symbol, is_setf, original, &display, options, at)?;
        drop(guard);
        method
    } else {
        install_function_trace(runtime, original, &display, options, at)?
    };
    Ok((runtime.root_slot(wrapper), generic))
}

/// Replace a traced definition without changing saved old function objects.
/// The caller installs the returned function into the already validated name.
pub(crate) fn reinstall_trace(
    runtime: &Runtime,
    symbol: Word,
    is_setf: bool,
    function: Word,
    at: usize,
) -> Result<Word, Fault> {
    let symbol = runtime.root_slot(symbol);
    let original = runtime.root_slot(function);
    let position = runtime
        .traced
        .borrow()
        .iter()
        .position(|record| record.is_setf == is_setf && record.symbol.get() == symbol.get());
    let Some(position) = position else {
        return Ok(original.get());
    };
    let options = runtime.traced.borrow()[position].options.clone();
    let (wrapper, generic) = make_trace(runtime, &symbol, is_setf, &original, &options, at)?;
    let installed = runtime.root_slot(if generic {
        original.get()
    } else {
        wrapper.get()
    });
    let old = std::mem::replace(
        &mut runtime.traced.borrow_mut()[position],
        TracedFunction {
            symbol,
            is_setf,
            original,
            wrapper,
            generic,
            options,
        },
    );
    if old.generic {
        super::methods::remove(runtime, old.original.get(), old.wrapper.get(), at)?;
    }
    Ok(installed.get())
}

fn untrace_functions(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<(), Fault> {
    if args.is_empty() {
        let records: Vec<TracedFunction> = runtime.traced.borrow_mut().drain(..).collect();
        for record in &records {
            restore_trace(runtime, record, at)?;
        }
        return Ok(());
    }
    for index in args {
        let (symbol, is_setf) = function_name(runtime, frame.get(index), at)?;
        let symbol = runtime.root_slot(symbol);
        let position =
            runtime.traced.borrow().iter().position(|record| {
                record.is_setf == is_setf && record.symbol.get() == symbol.get()
            });
        if let Some(position) = position {
            let record = runtime.traced.borrow_mut().remove(position);
            restore_trace(runtime, &record, at)?;
        }
    }
    Ok(())
}

fn function_name(runtime: &Runtime, value: Word, at: usize) -> Result<(Word, bool), Fault> {
    match super::FunctionName::parse(runtime, value, at)? {
        super::FunctionName::Symbol(symbol) => Ok((symbol, false)),
        super::FunctionName::Setf(accessor) => Ok((accessor, true)),
    }
}

fn trace_display(runtime: &Runtime, symbol: Word, is_setf: bool) -> String {
    let spelling = runtime
        .heap
        .symbol_name(symbol)
        .unwrap_or_else(|| "?".into());
    if is_setf {
        format!("(SETF {spelling})")
    } else {
        spelling
    }
}

fn undefined_trace(
    runtime: &Runtime,
    symbol: &RootSlot,
    is_setf: bool,
    name: Word,
    at: usize,
) -> Fault {
    let display = trace_display(runtime, symbol.get(), is_setf);
    Fault::cell(runtime, 
        Class::UndefinedFunction,
        name,
        EvalError::new(at, format!("undefined function {display}")),
    )
}

fn restore_trace(runtime: &Runtime, record: &TracedFunction, at: usize) -> Result<(), Fault> {
    if record.generic {
        super::methods::remove(runtime, record.original.get(), record.wrapper.get(), at)?;
    } else if record.is_setf {
        if runtime.heap.symbol_setter(record.symbol.get()) == Some(record.wrapper.get()) {
            runtime
                .heap
                .set_symbol_setter(record.symbol.get(), record.original.get());
        }
    } else if runtime.heap.symbol_cell(record.symbol.get(), true) == Some(record.wrapper.get()) {
        runtime
            .heap
            .set_symbol_cell(record.symbol.get(), true, record.original.get());
    }
    Ok(())
}

fn traced_name(
    runtime: &Runtime,
    symbol: &RootSlot,
    is_setf: bool,
    at: usize,
) -> Result<RootSlot, Fault> {
    if !is_setf {
        return Ok(runtime.root_slot(symbol.get()));
    }
    let setf = setf_symbol(runtime, at)?;
    let setf = runtime.root_slot(setf);
    let tail = runtime
        .heap
        .cons(|| (symbol.get(), Word::NIL))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let tail = runtime.root_slot(tail);
    let name = runtime
        .heap
        .cons(|| (setf.get(), tail.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(runtime.root_slot(name))
}

fn setf_symbol(runtime: &Runtime, at: usize) -> Result<Word, Fault> {
    runtime
        .find_named_symbol(&Name::common_lisp("SETF"))
        .ok_or_else(|| {
            Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "SETF is not defined"),
            )
        })
}

fn install_function_trace(
    runtime: &Runtime,
    original: &RootSlot,
    display: &str,
    options: &TraceOptions,
    at: usize,
) -> Result<Word, Fault> {
    let name = lisp_string(display);
    let source = format!(
        "(CL:LAMBDA (ZK::ORIGINAL)
           (CL:LAMBDA (CL:&REST ZK::ARGUMENTS)
             {}))",
        trace_body(
            &name,
            "ZK::ARGUMENTS",
            "(CL:APPLY ZK::ORIGINAL ZK::ARGUMENTS)",
            options
        )
    );
    let outer = eval_trace_lambda(runtime, &source, options, at)?;
    let original = runtime.root(original.get());
    let results = runtime
        .call(&runtime.root(outer.get()), &[&original])
        .map_err(Fault::from)?;
    match results.as_slice() {
        [wrapper] => Ok(wrapper.slot.get()),
        _ => Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "TRACE could not create a wrapper function"),
        )),
    }
}

fn install_generic_trace(
    runtime: &Runtime,
    symbol: &RootSlot,
    is_setf: bool,
    generic: &RootSlot,
    display: &str,
    options: &TraceOptions,
    at: usize,
) -> Result<Word, Fault> {
    let info = runtime
        .heap
        .generic_info(generic.get())
        .expect("traced generic function");
    let (required, optional, variadic) = generic_lambda_list_arity(runtime, info.lambda_list, at)?;
    let mut parameters = Vec::with_capacity(required + optional + 3);
    let mut source_parameters = String::from("ZK::TRACE-NEXT ZK::TRACE-NEXT-P");
    let mut required_arguments = String::from("(CL:LIST");
    let mut remaining_arguments = String::new();
    for index in 0..required {
        let name = format!("ZK-TRACE-REQ-{index}");
        source_parameters.push_str(" ZK::");
        source_parameters.push_str(&name);
        parameters.push(intern_user_symbol(runtime, &name, at)?);
        use std::fmt::Write;
        write!(required_arguments, " ZK::{name}").unwrap();
    }
    if optional > 0 {
        parameters.push(runtime.root_slot(common_lisp_symbol(runtime, "&OPTIONAL", at)?));
        source_parameters.push_str(" CL:&OPTIONAL");
        for index in 0..optional {
            let name = format!("ZK-TRACE-OPT-{index}");
            use std::fmt::Write;
            write!(source_parameters, " (ZK::{name} CL:NIL ZK::{name}-P)").unwrap();
            write!(
                remaining_arguments,
                " (CL:WHEN ZK::{name}-P (CL:LIST ZK::{name}))"
            )
            .unwrap();
            parameters.push(intern_user_symbol(runtime, &name, at)?);
        }
    }
    if variadic {
        parameters.push(runtime.root_slot(common_lisp_symbol(runtime, "&REST", at)?));
        parameters.push(intern_user_symbol(runtime, "ZK-TRACE-REST", at)?);
        source_parameters.push_str(" CL:&REST ZK::TRACE-REST");
        remaining_arguments.push_str(" ZK::TRACE-REST");
    }
    required_arguments.push(')');
    let name = lisp_string(display);
    let args = format!("(CL:APPEND {required_arguments}{remaining_arguments})");
    let source = format!(
        "(CL:LAMBDA ({source_parameters}) {})",
        trace_body(&name, &args, "(CL:FUNCALL ZK::TRACE-NEXT)", options)
    );
    let function = eval_trace_lambda(runtime, &source, options, at)?;
    let name_word = traced_name(runtime, symbol, is_setf, at)?;
    let around = runtime.root_slot(
        runtime
            .intern_package(Package::KEYWORD, "AROUND", at)
            .map_err(Fault::from)?
            .0,
    );
    let qualifiers = list_of(runtime, &[around], at)?;
    let specializers: Vec<RootSlot> = (0..required).map(|_| runtime.root_slot(Word::T)).collect();
    let specializers = list_of(runtime, &specializers, at)?;
    let lambda_list = list_of(runtime, &parameters, at)?;
    let arguments = [
        runtime.root(name_word.get()),
        runtime.root(qualifiers.get()),
        runtime.root(specializers.get()),
        runtime.root(lambda_list.get()),
        runtime.root(function.get()),
        runtime.nil(),
        runtime.nil(),
    ];
    let results = super::generic::define_method(runtime, &arguments, at)?;
    match results.as_slice() {
        [method] => Ok(method.slot.get()),
        _ => Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "TRACE could not create a tracing method"),
        )),
    }
}

fn eval_trace_lambda(
    runtime: &Runtime,
    source: &str,
    options: &TraceOptions,
    at: usize,
) -> Result<RootSlot, Fault> {
    fn substitute(form: &mut Form, options: &TraceOptions) {
        if let Some(name) = form.name()
            && let Some(index) = name
                .spelling()
                .strip_prefix("%TRACE-OPTION-")
                .and_then(|n| n.parse::<usize>().ok())
        {
            *form = options.0[index].1.clone();
            return;
        }
        if let FormKind::List(items, tail) = &mut form.kind {
            for item in items {
                substitute(item, options);
            }
            if let Some(tail) = tail {
                substitute(tail, options);
            }
            // Reader forms can retain their original Lisp object. Macro
            // expansion must observe the substituted syntax instead.
            *form = Form::new(form.at, form.kind.clone());
        }
    }
    let mut form = crate::read::read(runtime, source)?;
    substitute(&mut form, options);
    super::eval_host(runtime, form, None)?
        .into_iter()
        .next()
        .map(|value| value.slot)
        .ok_or_else(|| {
            Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "TRACE expected a function"),
            )
        })
}

fn trace_body(name: &str, arguments: &str, call: &str, options: &TraceOptions) -> String {
    let expression = |index| format!("ZK::%TRACE-OPTION-{index}");
    let last = |key: &str| {
        options
            .0
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (k, _))| k == key)
            .map(|(i, _)| expression(i))
    };
    let condition = |key: &str| {
        options
            .0
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (name, _))| name == key || name == "CONDITION-ALL")
            .map_or_else(|| "CL:T".into(), |(index, _)| expression(index))
    };
    let before = condition("CONDITION");
    let after = condition("CONDITION-AFTER");
    let report = options
        .0
        .iter()
        .rev()
        .find(|(key, _)| key == "REPORT")
        .map_or("CL:T", |(_, form)| {
            if form.name().is_some_and(|name| name.operator() == "NIL") {
                "CL:NIL"
            } else {
                "CL:T"
            }
        });
    let mut entry = String::new();
    let mut exit = String::new();
    for (index, (key, _)) in options.0.iter().enumerate() {
        let form = expression(index);
        let target = if key.ends_with("-AFTER") {
            &mut exit
        } else {
            &mut entry
        };
        let statement = if key.starts_with("PRINT") {
            format!("(CL:FORMAT CL:*TRACE-OUTPUT* \"~&~S~%\" {form})")
        } else if key.starts_with("BREAK") {
            format!("(CL:WHEN {form} (CL:BREAK \"TRACE ~A\" {name}))")
        } else {
            continue;
        };
        target.push_str(&statement);
        if key.ends_with("-ALL") {
            exit.push_str(&statement);
        }
    }
    let entry_report = if last("CONDITION-AFTER").is_some() {
        "CL:NIL"
    } else {
        "ZK::TRACE-REPORT"
    };
    format!("(CL:LET* ((ZK::*TRACE-ARGUMENTS* {arguments}) (ZK::*TRACE-VALUES* CL:NIL) (ZK::RETURNED CL:NIL) (ZK::TRACE-ACTIVE {before}) (ZK::TRACE-REPORT {report}))
      (CL:DECLARE (CL:SPECIAL ZK::*TRACE-ARGUMENTS* ZK::*TRACE-VALUES*))
      (CL:WHEN ZK::TRACE-ACTIVE
        (CL:WHEN {entry_report} (CL:FORMAT CL:*TRACE-OUTPUT* \"~&TRACE ~A: enter ~S~%\" {name} ZK::*TRACE-ARGUMENTS*)) {entry})
      (CL:UNWIND-PROTECT
        (CL:MULTIPLE-VALUE-CALL
          (CL:LAMBDA (CL:&REST ZK::RESULTS)
            (CL:SETQ ZK::RETURNED CL:T ZK::*TRACE-VALUES* ZK::RESULTS)
            (CL:WHEN (CL:AND ZK::TRACE-ACTIVE {after})
              (CL:WHEN ZK::TRACE-REPORT (CL:FORMAT CL:*TRACE-OUTPUT* \"~&TRACE ~A: exit ~S~%\" {name} ZK::RESULTS)) {exit})
            (CL:VALUES-LIST ZK::RESULTS)) {call})
        (CL:WHEN (CL:AND ZK::TRACE-ACTIVE ZK::TRACE-REPORT (CL:IF ZK::RETURNED CL:NIL CL:T))
          (CL:FORMAT CL:*TRACE-OUTPUT* \"~&TRACE ~A: exit (non-local transfer)~%\" {name}))))")
}

fn generic_lambda_list_arity(
    runtime: &Runtime,
    lambda_list: Word,
    at: usize,
) -> Result<(usize, usize, bool), Fault> {
    let mut required = 0;
    let mut optional = 0;
    let mut variadic = false;
    let mut state = LambdaListSection::Required;
    let mut rest = lambda_list;
    while rest != Word::NIL {
        let Some((item, next)) = runtime.heap.cons_fields(rest) else {
            return Err(Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "malformed generic function lambda list"),
            ));
        };
        if let Some(name) = runtime.heap.symbol_name(item) {
            match name.as_str() {
                "&OPTIONAL" => state = LambdaListSection::Optional,
                "&REST" | "&BODY" | "&KEY" => {
                    variadic = true;
                    state = LambdaListSection::Variadic;
                }
                "&AUX" | "&WHOLE" | "&ENVIRONMENT" => break,
                "&ALLOW-OTHER-KEYS" => {}
                _ if name.starts_with('&') => break,
                _ => match state {
                    LambdaListSection::Required => required += 1,
                    LambdaListSection::Optional => optional += 1,
                    LambdaListSection::Variadic => {}
                },
            }
        }
        rest = next;
    }
    Ok((required, optional, variadic))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LambdaListSection {
    Required,
    Optional,
    Variadic,
}

fn intern_user_symbol(runtime: &Runtime, name: &str, at: usize) -> Result<RootSlot, Fault> {
    let (word, _) = runtime
        .intern_package(Package::USER, name, at)
        .map_err(Fault::from)?;
    Ok(runtime.root_slot(word))
}

fn common_lisp_symbol(runtime: &Runtime, name: &str, at: usize) -> Result<Word, Fault> {
    runtime
        .find_named_symbol(&Name::common_lisp(name))
        .ok_or_else(|| {
            Fault::plain(
                Class::ProgramError,
                EvalError::new(at, format!("missing symbol {name}")),
            )
        })
}

fn list_of(runtime: &Runtime, items: &[RootSlot], at: usize) -> Result<RootSlot, Fault> {
    let mut list = runtime.root_slot(Word::NIL);
    for item in items.iter().rev() {
        let next = runtime
            .heap
            .cons(|| (item.get(), list.get()))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        list = runtime.root_slot(next);
    }
    Ok(list)
}

fn lisp_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        if character == '"' || character == '\\' {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

fn disassemble_designator(runtime: &Runtime, value: Word) -> bool {
    if runtime.heap.is_symbol(value) || super::is_function(runtime, value) {
        return true;
    }
    let Some((head, rest)) = runtime.heap.cons_fields(value) else {
        return false;
    };
    if runtime.heap.symbol_name(head).as_deref() == Some("LAMBDA") {
        return true;
    }
    if runtime.heap.symbol_name(head).as_deref() != Some("SETF") {
        return false;
    }
    let Some((name, tail)) = runtime.heap.cons_fields(rest) else {
        return false;
    };
    tail == Word::NIL && runtime.heap.is_symbol(name)
}

fn string(runtime: &Runtime, text: &str, values: &mut Values, at: usize) -> Result<(), Fault> {
    let word = runtime
        .heap
        .string(text)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    values.one(word);
    Ok(())
}

fn integer(runtime: &Runtime, value: BigInt, values: &mut Values, at: usize) -> Result<(), Fault> {
    let word = runtime
        .make_integer(&value)
        .map_err(|error| EvalError::out_of_memory(at, error))?
        .slot
        .get();
    values.one(word);
    Ok(())
}

fn integer_argument(runtime: &Runtime, word: Word, at: usize) -> Result<i64, Fault> {
    match runtime.heap.number(word) {
        Some(Number::Real(Real::Integer(value))) => value
            .to_i64()
            .ok_or_else(|| EvalError::new(at, "integer is out of range").into()),
        Some(_) => Err(Fault::type_error(runtime, 
            word,
            "INTEGER",
            EvalError::new(at, "expected an integer"),
        )),
        None => Err(Fault::type_error(runtime, 
            word,
            "INTEGER",
            EvalError::new(at, "expected an integer"),
        )),
    }
}

fn real_argument(runtime: &Runtime, word: Word, at: usize) -> Result<f64, Fault> {
    let Some(Number::Real(real)) = runtime.heap.number(word) else {
        return Err(Fault::type_error(runtime, 
            word,
            "REAL",
            EvalError::new(at, "expected a real number"),
        ));
    };
    match real {
        Real::Integer(value) => value
            .to_f64()
            .ok_or_else(|| EvalError::new(at, "real is out of range").into()),
        Real::Ratio(value) => value
            .to_f64()
            .ok_or_else(|| EvalError::new(at, "real is out of range").into()),
        Real::Single(value) => Ok(f64::from(value)),
        Real::Double(value) => Ok(value),
    }
}

fn apropos_package(
    runtime: &Runtime,
    designator: Word,
    at: usize,
) -> Result<Option<Package>, Fault> {
    if designator == Word::NIL {
        return Ok(None);
    }
    if let Some(package) = Package::from_word(designator) {
        return Ok(Some(package));
    }
    let name = runtime.string_designator_text(designator).ok_or_else(|| {
        Fault::package_designator(runtime, 
            designator,
            EvalError::new(at, "expected a package designator"),
        )
    })?;
    runtime.find_package_named(&name).map(Some).ok_or_else(|| {
        Fault::plain(
            crate::condition::Class::PackageError,
            EvalError::new(at, format!("package {name} does not exist")),
        )
    })
}

fn apropos_symbols(
    runtime: &Runtime,
    package: Option<Package>,
    text: &str,
    at: usize,
) -> Result<Vec<RootSlot>, Fault> {
    let needle = text.to_uppercase();
    let mut seen = std::collections::HashSet::new();
    let mut symbols = Vec::new();
    let packages = match package {
        Some(package) => vec![package],
        None => runtime.packages.borrow().list().collect(),
    };
    for package in packages {
        let entries = runtime
            .packages
            .borrow()
            .entries_snapshot(package, true, true, true)
            .map_err(|error| Fault::from_package_error(error, at))?;
        for (symbol, _, _) in entries {
            if !seen.insert(symbol.0) {
                continue;
            }
            if runtime
                .heap
                .symbol_name(symbol)
                .is_some_and(|name| name.to_uppercase().contains(&needle))
            {
                symbols.push(runtime.root_slot(symbol));
            }
        }
    }
    Ok(symbols)
}

fn internal_run_time(_runtime: &Runtime) -> io::Result<u128> {
    cfg_select! {
        any(
            target_vendor = "apple",
            target_os = "linux",
            target_os = "android",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "solaris",
            target_os = "illumos",
        ) => {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            // SAFETY: getrusage writes to this live, suitably aligned buffer.
            let status = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
            if status != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: Successful getrusage initialized the structure.
            let usage = unsafe { usage.assume_init() };
            let nanos = |time: libc::timeval| {
                Duration::new(time.tv_sec as u64, time.tv_usec as u32 * 1_000).as_nanos()
            };
            Ok(nanos(usage.ru_utime) + nanos(usage.ru_stime))
        }
        windows => {
            use windows_sys::Win32::Foundation::FILETIME;
            use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

            let [mut creation, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
            // SAFETY: The pseudo-handle refers to this process and the pointers
            // each point to distinct live FILETIME storage.
            let status = unsafe {
                GetProcessTimes(GetCurrentProcess(), &mut creation, &mut exit, &mut kernel, &mut user)
            };
            if status == 0 {
                return Err(io::Error::last_os_error());
            }
            let ticks = |time: FILETIME| {
                (u128::from(time.dwHighDateTime) << 32) | u128::from(time.dwLowDateTime)
            };
            Ok((ticks(kernel) + ticks(user)) * 100)
        }
        _ => {
            // Targets without a process CPU clock use elapsed real time.
            _runtime.host_services.monotonic_nanos().map(u128::from)
        }
    }
}

fn decode_universal(
    runtime: &Runtime,
    universal: i64,
    zone: Option<Word>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let zone_hours = match zone {
        Some(zone) => real_argument(runtime, zone, at)?,
        None => 0.0,
    };
    let unix = universal
        .saturating_sub(UNIX_EPOCH_UNIVERSAL)
        .saturating_sub((zone_hours * 3600.0).round() as i64);
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3600;
    let minute = (seconds % 3600) / 60;
    let second = seconds % 60;
    let weekday = (days + 3).rem_euclid(7);
    let numbers = [
        BigInt::from(second),
        BigInt::from(minute),
        BigInt::from(hour),
        BigInt::from(day),
        BigInt::from(month),
        BigInt::from(year),
        BigInt::from(weekday),
    ];
    values.resize(9);
    for (index, value) in numbers.into_iter().enumerate() {
        let word = runtime
            .make_integer(&value)
            .map_err(|error| EvalError::out_of_memory(at, error))?
            .slot
            .get();
        values.slots.set(index, word);
    }
    values.slots.set(7, Word::NIL);
    values.slots.set(
        8,
        zone.unwrap_or_else(|| runtime.fixnum(0).expect("zero").slot.get()),
    );
    Ok(())
}

/// Install standard functions that enter the debugger or query the user.
///
/// These are ordinary Lisp functions so that they reuse FORMAT, conditions,
/// and the host-neutral answer loop instead of duplicating stream logic.
pub(crate) fn initialize_interaction(runtime: &Runtime) -> Result<(), EvalError> {
    for (name, source) in [
        ("BREAK", BREAK),
        ("Y-OR-N-P", Y_OR_N_P),
        ("YES-OR-NO-P", YES_OR_NO_P),
        ("INVALID-METHOD-ERROR", INVALID_METHOD_ERROR),
        ("METHOD-COMBINATION-ERROR", METHOD_COMBINATION_ERROR),
    ] {
        let values = runtime.eval(source)?;
        let function = values
            .first()
            .ok_or_else(|| EvalError::new(0, format!("{name} initializer returned no function")))?;
        let (symbol, _) = runtime.intern_package(Package::COMMON_LISP, name, 0)?;
        runtime
            .heap
            .set_symbol_cell(symbol, true, function.slot.get());
    }
    Ok(())
}

const BREAK: &str = r#"
(lambda (&optional (format-control "Break") &rest format-arguments)
  (let ((*debugger-hook* nil))
    (with-simple-restart (continue "Return from BREAK.")
      (invoke-debugger (make-condition 'simple-condition
                        :format-control format-control :format-arguments format-arguments))))
  nil)
"#;

const Y_OR_N_P: &str = r#"
(lambda (&optional format-control &rest format-arguments)
  (when format-control
    (apply #'format *query-io* format-control format-arguments))
  (loop
    (format *query-io* "~&(Y or N) ")
    (finish-output *query-io*)
    (let ((line (read-line *query-io* nil nil)))
      (when (null line) (return nil))
      (let ((answer (string-trim '(#\Space #\Tab) line)))
        (cond ((or (string-equal answer "y") (string-equal answer "yes"))
               (return t))
              ((or (string-equal answer "n") (string-equal answer "no"))
               (return nil))
              (t (format *query-io* "~&Please answer Y or N.~%")))))))
"#;

const YES_OR_NO_P: &str = r#"
(lambda (&optional format-control &rest format-arguments)
  (when format-control
    (apply #'format *query-io* format-control format-arguments))
  (loop
    (format *query-io* "~&(Yes or No) ")
    (finish-output *query-io*)
    (let ((line (read-line *query-io* nil nil)))
      (when (null line) (return nil))
      (let ((answer (string-trim '(#\Space #\Tab) line)))
        (cond ((string-equal answer "yes") (return t))
              ((string-equal answer "no") (return nil))
              (t (format *query-io* "~&Please answer Yes or No.~%")))))))
"#;

const INVALID_METHOD_ERROR: &str = r#"
(lambda (method format-control &rest format-arguments)
  (error "Invalid method ~S: ~?" method format-control format-arguments))
"#;

const METHOD_COMBINATION_ERROR: &str = r#"
(lambda (format-control &rest format-arguments)
  (error "Method combination error: ~?" format-control format-arguments))
"#;

// Howard Hinnant's civil calendar algorithms, with 1970-01-01 as day zero.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}
