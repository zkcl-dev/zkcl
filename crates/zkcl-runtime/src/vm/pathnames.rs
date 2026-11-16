use std::ops::Range;

use super::keywords::named_keyword_arguments_allowing_other_keys;
use super::{Primitive, Values, named_keyword_arguments as keys};
use crate::condition::{Class, Fault};
use crate::heap::{RootSlots, Word};
use crate::pathname::{self, Field, Pathname, PathnameError, Version};
use crate::{EvalError, Root, Runtime};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Pathname,
    LogicalPathname,
    Translations,
    SetTranslations,
    TranslateLogical,
    TranslatePathname,
    Match,
    Predicate,
    Make,
    Parse,
    Merge,
    Component(Field),
    Namestring,
    EnoughNamestring,
    FileNamestring,
    DirectoryNamestring,
    HostNamestring,
    CompileFilePathname,
    Wild,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("PATHNAME", Op::Pathname),
    ("LOGICAL-PATHNAME", Op::LogicalPathname),
    ("LOGICAL-PATHNAME-TRANSLATIONS", Op::Translations),
    ("(SETF LOGICAL-PATHNAME-TRANSLATIONS)", Op::SetTranslations),
    ("TRANSLATE-LOGICAL-PATHNAME", Op::TranslateLogical),
    ("TRANSLATE-PATHNAME", Op::TranslatePathname),
    ("PATHNAME-MATCH-P", Op::Match),
    ("PATHNAMEP", Op::Predicate),
    ("MAKE-PATHNAME", Op::Make),
    ("PARSE-NAMESTRING", Op::Parse),
    ("MERGE-PATHNAMES", Op::Merge),
    ("PATHNAME-HOST", Op::Component(Field::Host)),
    ("PATHNAME-DEVICE", Op::Component(Field::Device)),
    ("PATHNAME-DIRECTORY", Op::Component(Field::Directory)),
    ("PATHNAME-NAME", Op::Component(Field::Name)),
    ("PATHNAME-TYPE", Op::Component(Field::Type)),
    ("PATHNAME-VERSION", Op::Component(Field::Version)),
    ("NAMESTRING", Op::Namestring),
    ("ENOUGH-NAMESTRING", Op::EnoughNamestring),
    ("FILE-NAMESTRING", Op::FileNamestring),
    ("DIRECTORY-NAMESTRING", Op::DirectoryNamestring),
    ("HOST-NAMESTRING", Op::HostNamestring),
    ("COMPILE-FILE-PATHNAME", Op::CompileFilePathname),
    ("WILD-PATHNAME-P", Op::Wild),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Make => true,
            Self::Merge => (1..=3).contains(&count),
            Self::Match | Self::SetTranslations => count == 2,
            Self::TranslatePathname => count >= 3,
            Self::Wild | Self::EnoughNamestring => (1..=2).contains(&count),
            Self::Parse | Self::CompileFilePathname | Self::TranslateLogical => count >= 1,
            Self::Component(Field::Version) => count == 1,
            Self::Component(_) => count >= 1,
            _ => count == 1,
        }
    }
}

pub(super) fn setf_primitive(accessor: &str) -> Option<Word> {
    if accessor == "LOGICAL-PATHNAME-TRANSLATIONS" {
        Primitive::named("(SETF LOGICAL-PATHNAME-TRANSLATIONS)")
    } else {
        None
    }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let arg = |index| frame.get(args.start + index);
    let output = match op {
        Op::LogicalPathname => {
            let value = arg(0);
            let invalid = |error| Fault::type_error(runtime, arg(0), "LOGICAL-PATHNAME", error);
            let word = pathname::file_stream_pathname(runtime, value).unwrap_or(value);
            if pathname::from_word(runtime, word).is_some_and(|path| path.logical_host.is_some()) {
                runtime.root(word)
            } else {
                let text = runtime.heap.string_contents(word).ok_or_else(|| {
                    invalid(EvalError::new(
                        at,
                        "expected a logical pathname or namestring",
                    ))
                })?;
                pathname::logical_pathname(runtime, &text)
                    .map_err(|error| invalid(EvalError::from_error(at, error)))?
                    .to_lisp_value(runtime, at)?
            }
        }
        Op::Translations => {
            let word = arg(0);
            let host = pathname::translation_host(runtime, word, at)?;
            pathname::translations(runtime, &host, word, at)?
        }
        Op::SetTranslations => {
            let rules = arg(0);
            let host = pathname::translation_host(runtime, arg(1), at)?;
            pathname::set_translations(runtime, &host, rules, at)?;
            runtime.root(rules)
        }
        Op::TranslateLogical => {
            keys(runtime, frame, args.start + 1..args.end, [], at)?;
            let word = arg(0);
            let word = pathname::file_stream_pathname(runtime, word).unwrap_or(word);
            let path = designator(runtime, word, at)?;
            if path.logical_host.is_none() && runtime.heap.pathname_fields(word).is_some() {
                runtime.root(word)
            } else {
                pathname::translate(runtime, path, at)?.to_lisp_value(runtime, at)?
            }
        }
        Op::TranslatePathname => {
            keys(runtime, frame, args.start + 3..args.end, [], at)?;
            let source = designator(runtime, arg(0), at)?;
            let from = designator(runtime, arg(1), at)?;
            let to = designator(runtime, arg(2), at)?;
            pathname::translate_pathname(&source, &from, &to)
                .map_err(|error| Fault::file_error(runtime, arg(0), EvalError::from_error(at, error)))?
                .to_lisp_value(runtime, at)?
        }
        Op::Match => {
            let source = designator(runtime, arg(0), at)?;
            let pattern = designator(runtime, arg(1), at)?;
            runtime.root(truth(pathname::pathname_match(&source, &pattern)))
        }
        Op::Pathname => {
            let word = arg(0);
            let word = pathname::file_stream_pathname(runtime, word).unwrap_or(word);
            if runtime.heap.pathname_fields(word).is_some() {
                runtime.root(word)
            } else {
                designator(runtime, word, at)?.to_lisp_value(runtime, at)?
            }
        }
        Op::Predicate => runtime.root(truth(runtime.heap.pathname_fields(arg(0)).is_some())),
        Op::Make => make(runtime, frame, args, at)?,
        Op::Parse => return parse(runtime, frame, args, values, at),
        Op::Merge => {
            let defaults = if args.len() >= 2 {
                designator(runtime, arg(1), at)?
            } else {
                pathname::default_pathname(runtime, at)?
            };
            let path = designator_with_defaults(runtime, arg(0), &defaults, at)?;
            let version = if args.len() == 3 {
                component(pathname::decode_version(runtime, arg(2)), "version", at)?
            } else {
                Version::Newest
            };
            path.merge_with(&defaults, version)
                .to_lisp_value(runtime, at)?
        }
        Op::Component(field) => {
            let [case] = keys(runtime, frame, args.start + 1..args.end, ["CASE"], at)?;
            let common = common_case(runtime, frame, case, at)?;
            let word = arg(0);
            match runtime.heap.pathname_fields(word) {
                Some(fields) if !common => runtime.root(fields[field as usize]),
                _ => designator(runtime, word, at)?.component(runtime, field, common, at)?,
            }
        }
        Op::Namestring | Op::FileNamestring | Op::DirectoryNamestring | Op::HostNamestring => {
            let path = designator(runtime, arg(0), at)?;
            let text = match op {
                Op::Namestring => path.namestring(),
                Op::FileNamestring => path.file_namestring(),
                Op::DirectoryNamestring => Some(path.directory_namestring()),
                Op::HostNamestring => Some(path.logical_host.unwrap_or_default()),
                _ => unreachable!(),
            };
            match text {
                Some(text) => string(runtime, &text, at)?,
                None => runtime.nil(),
            }
        }
        Op::EnoughNamestring => {
            let defaults = if args.len() == 2 {
                designator(runtime, arg(1), at)?
            } else {
                pathname::default_pathname(runtime, at)?
            };
            let pathname = designator_with_defaults(runtime, arg(0), &defaults, at)?;
            let target = pathname.merge(&defaults);
            let same_prefix = target.logical_host == defaults.logical_host
                && target.host_unspecific == defaults.host_unspecific
                && target.device_unspecific == defaults.device_unspecific
                && target.directory == defaults.directory;
            let text = if same_prefix {
                target.file_namestring().map(|file| {
                    if target.logical_host.is_some() {
                        // A hostless logical namestring without a semicolon is absolute.
                        format!(";{file}")
                    } else {
                        file
                    }
                })
            } else {
                target.namestring()
            };
            match text {
                Some(text) => string(runtime, &text, at)?,
                None => runtime.nil(),
            }
        }
        Op::CompileFilePathname => {
            let [output_file] = named_keyword_arguments_allowing_other_keys(
                runtime,
                frame,
                args.start + 1..args.end,
                ["OUTPUT-FILE"],
                at,
            )?;
            let input = designator(runtime, arg(0), at)?;
            let defaults = pathname::default_pathname(runtime, at)?;
            let input = input.merge(&defaults);
            let output = output_file
                .map(|index| frame.get(index))
                .filter(|&word| word != Word::NIL)
                .map(|word| designator(runtime, word, at))
                .transpose()?;
            pathname::compiled_output(&input, output.as_ref()).to_lisp_value(runtime, at)?
        }
        Op::Wild => {
            let path = designator(runtime, arg(0), at)?;
            let selector = if args.len() == 2 { arg(1) } else { Word::NIL };
            let wild = if selector == Word::NIL {
                path.is_wild()
            } else {
                match pathname::keyword_name(runtime, selector).as_deref() {
                    Some("HOST" | "DEVICE") => false,
                    Some("NAME") => path.wild_leaf(&path.name),
                    Some("TYPE") => path.wild_leaf(&path.file_type),
                    Some("VERSION") => path.version == Version::Wild,
                    Some("DIRECTORY") => path.wild_directory(),
                    _ => {
                        return Err(Fault::plain(
                            Class::TypeError,
                            EvalError::new(at, "expected a pathname component keyword"),
                        ));
                    }
                }
            };
            runtime.root(truth(wild))
        }
    };
    values.one(output.slot.get());
    Ok(())
}

fn make<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let [
        host,
        device,
        directory,
        name,
        file_type,
        version,
        defaults,
        case,
    ] = keys(
        runtime,
        frame,
        args,
        [
            "HOST",
            "DEVICE",
            "DIRECTORY",
            "NAME",
            "TYPE",
            "VERSION",
            "DEFAULTS",
            "CASE",
        ],
        at,
    )?;
    let common = common_case(runtime, frame, case, at)?;
    let default_path = match defaults {
        Some(index) => designator(runtime, frame.get(index), at)?,
        None => {
            let defaults = pathname::default_pathname(runtime, at)?;
            Pathname {
                logical_host: defaults.logical_host,
                host_unspecific: defaults.host_unspecific,
                ..Pathname::default()
            }
        }
    };
    let decode = |index: Option<usize>| index.map_or(Word::NIL, |index| frame.get(index));
    let (logical_host, host_unspecific) = if let Some(index) = host {
        host_argument(runtime, frame.get(index), at)?
    } else {
        (
            default_path.logical_host.clone(),
            default_path.host_unspecific,
        )
    };
    let common = common && logical_host.is_none();
    let path = Pathname {
        logical_host,
        host_unspecific,
        device_unspecific: component(
            pathname::decode_unspecific(runtime, decode(device)),
            "device",
            at,
        )?,
        directory: component(
            pathname::decode_directory(runtime, decode(directory), common),
            "directory",
            at,
        )?,
        name: component(
            pathname::decode_leaf(runtime, decode(name), common),
            "name",
            at,
        )?,
        file_type: component(
            pathname::decode_leaf(runtime, decode(file_type), common),
            "type",
            at,
        )?,
        version: component(
            pathname::decode_version(runtime, decode(version)),
            "version",
            at,
        )?,
    };
    // Supplied NIL means an explicitly empty slot. Only omitted slots default.
    let mut result = path.merge_with(&default_path, Version::Missing);
    if host.is_some() {
        result.host_unspecific = path.host_unspecific;
    }
    if device.is_some() {
        result.device_unspecific = path.device_unspecific;
    }
    if directory.is_some() {
        result.directory = path.directory;
    }
    if name.is_some() {
        result.name = path.name;
    }
    if file_type.is_some() {
        result.file_type = path.file_type;
    }
    if version.is_some() {
        result.version = path.version;
    }
    pathname::normalize_logical(&mut result)
        .map_err(|error| Fault::plain(Class::TypeError, EvalError::from_error(at, error)))?;
    Ok(result.to_lisp_value(runtime, at)?)
}

fn parse(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let options = (args.start + 3).min(args.end)..args.end;
    let [start, end, junk_allowed] = keys(
        runtime,
        frame,
        options,
        ["START", "END", "JUNK-ALLOWED"],
        at,
    )?;
    let start = start.map_or(Ok(0), |index| bounding_index(runtime, frame.get(index), at))?;
    let end = end
        .map(|index| frame.get(index))
        .filter(|&word| word != Word::NIL);
    let end = end.map(|word| bounding_index(runtime, word, at)).transpose()?;
    let junk_allowed = junk_allowed.is_some_and(|index| frame.get(index) != Word::NIL);
    let host = if args.len() >= 2 && frame.get(args.start + 1) != Word::NIL {
        Some(host_argument(runtime, frame.get(args.start + 1), at)?)
    } else {
        None
    };
    let given = frame.get(args.start);
    let word = pathname::file_stream_pathname(runtime, given).unwrap_or(given);
    let (pathname, position) = if let Some(path) = pathname::from_word(runtime, word) {
        if host.as_ref().is_some_and(|(logical, unspecific)| {
            !pathname::hosts_equal(&path, logical.as_deref(), *unspecific)
        }) {
            return Err(Fault::file_error(runtime, 
                word,
                EvalError::new(at, "pathname host does not match"),
            ));
        }
        (runtime.root(word), start)
    } else {
        let text = runtime.heap.string_contents(word).ok_or_else(|| {
            Fault::pathname_designator(runtime, 
                given,
                EvalError::new(at, "expected a pathname, namestring, or file stream"),
            )
        })?;
        let default = if args.len() >= 3 && frame.get(args.start + 2) != Word::NIL {
            designator(runtime, frame.get(args.start + 2), at)?
        } else {
            pathname::default_pathname(runtime, at)?
        };
        let bounds = || {
            Fault::plain(
                Class::Error,
                EvalError::new(at, "pathname bounds are outside the string"),
            )
        };
        let length = text.chars().count();
        let end = end.unwrap_or(length);
        if start > end || end > length {
            return Err(bounds());
        }
        let byte_index = |index| {
            text.char_indices()
                .nth(index)
                .map_or(text.len(), |(byte, _)| byte)
        };
        let text = &text[byte_index(start)..byte_index(end)];
        let parse_text = |text| -> Result<Pathname, PathnameError> {
            match &host {
                Some((Some(host), _)) => {
                    pathname::parse_namestring(runtime, text, Some(host), None)
                }
                Some((None, unspecific)) => {
                    if text.split_once(':').is_some_and(|(prefix, _)| {
                        pathname::logical_host(runtime, prefix).is_some()
                    }) {
                        return Err(PathnameError::HostMismatch);
                    }
                    Pathname::parse(text).map(|mut result| {
                        result.host_unspecific = *unspecific;
                        result
                    })
                }
                None => {
                    pathname::parse_namestring(runtime, text, None, default.logical_host.as_deref())
                }
            }
        };
        let (parsed, position) = match parse_text(text) {
            Ok(parsed) => (Some(parsed), end),
            Err(error) if junk_allowed && !matches!(error, PathnameError::HostMismatch) => {
                let logical = host.as_ref().is_some_and(|(host, _)| host.is_some())
                    || text.split_once(':').is_some_and(|(prefix, _)| {
                        pathname::logical_host(runtime, prefix).is_some()
                    });
                let mut stop = if logical {
                    text.char_indices()
                        .find(|&(_, ch)| {
                            !ch.is_ascii_alphanumeric()
                                && !matches!(ch, '-' | '*' | ':' | ';' | '.')
                        })
                        .map_or(text.len(), |(index, _)| index)
                } else {
                    delimiter(text)
                };
                let mut result = None;
                while stop != 0 {
                    if let Ok(parsed) = parse_text(&text[..stop]) {
                        result = Some(parsed);
                        break;
                    }
                    stop = text[..stop]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                }
                (result, start + text[..stop].chars().count())
            }
            Err(error) => {
                let host = matches!(error, PathnameError::HostMismatch);
                return Err(Fault::plain(
                    if host {
                        Class::Error
                    } else {
                        Class::ParseError
                    },
                    EvalError::from_error(at, error),
                ));
            }
        };
        let value = if let Some(parsed) = parsed {
            parsed.to_lisp_value(runtime, at)?
        } else {
            runtime.nil()
        };
        (value, position)
    };
    let position = isize::try_from(position)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| {
            EvalError::new(at, "pathname position exceeds the supported fixnum range")
        })?;
    values.resize(2);
    values.slots.set(0, pathname.slot.get());
    values.slots.set(1, position);
    Ok(())
}

fn host_argument(
    runtime: &Runtime,
    word: Word,
    at: usize,
) -> Result<(Option<String>, bool), Fault> {
    if let Some(text) = runtime.heap.string_contents(word) {
        return pathname::logical_host(runtime, &text)
            .map(|host| (Some(host), false))
            .ok_or_else(|| {
                Fault::type_error(runtime, 
                    word,
                    "STRING",
                    EvalError::new(at, format!("no logical pathname host named {text}")),
                )
            });
    }
    component(pathname::decode_unspecific(runtime, word), "host", at)
        .map(|unspecific| (None, unspecific))
}

// Parsing only fails on a NUL or an escape without an escaped character.
fn delimiter(text: &str) -> usize {
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\0' => return index,
            '\\' => match chars.next() {
                None | Some((_, '/' | '\0')) => return index,
                Some(_) => {}
            },
            _ => {}
        }
    }
    text.len()
}

fn bounding_index(runtime: &Runtime, word: Word, at: usize) -> Result<usize, Fault> {
    word.as_fixnum()
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| {
            Fault::non_negative_integer(
                runtime,
                word,
                EvalError::new(at, "expected a non-negative pathname index"),
            )
        })
}

// A file stream is a pathname designator for the file it is connected to.
fn designator(runtime: &Runtime, word: Word, at: usize) -> Result<Pathname, Fault> {
    crate::loader::designator(runtime, word, at)
}

fn designator_with_defaults(
    runtime: &Runtime,
    word: Word,
    defaults: &Pathname,
    at: usize,
) -> Result<Pathname, Fault> {
    if let Some(text) = runtime.heap.string_contents(word) {
        pathname::namestring_designator(runtime, word, &text, defaults, at)
    } else {
        designator(runtime, word, at)
    }
}

fn common_case(
    runtime: &Runtime,
    frame: &RootSlots,
    parameter: Option<usize>,
    at: usize,
) -> Result<bool, Fault> {
    let Some(index) = parameter else {
        return Ok(false);
    };
    match pathname::keyword_name(runtime, frame.get(index)).as_deref() {
        Some("COMMON") => Ok(true),
        Some("LOCAL") => Ok(false),
        _ => Err(Fault::plain(
            Class::TypeError,
            EvalError::new(at, "expected pathname case :LOCAL or :COMMON"),
        )),
    }
}

fn component<T>(component: Option<T>, name: &str, at: usize) -> Result<T, Fault> {
    component.ok_or_else(|| {
        Fault::plain(
            Class::TypeError,
            EvalError::new(at, format!("invalid physical pathname {name}")),
        )
    })
}

fn string<'rt>(runtime: &'rt Runtime, text: &str, at: usize) -> Result<Root<'rt>, EvalError> {
    let word = runtime
        .heap
        .string(text)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(runtime.root(word))
}

fn truth(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}
