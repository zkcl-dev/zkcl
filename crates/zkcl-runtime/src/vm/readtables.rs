use std::ops::Range;

use super::{Primitive, Values, symbol_value};
use crate::condition::Fault;
use crate::heap::{ReadtableCase, RootSlots, Word};
use crate::symbol::{Name, Package};
use crate::{EvalError, Runtime};

const SET_CASE: &str = "(SETF READTABLE-CASE)";

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Copy,
    Predicate,
    Case,
    SetCase,
    GetMacroCharacter,
    SetMacroCharacter,
    MakeDispatchMacroCharacter,
    GetDispatchMacroCharacter,
    SetDispatchMacroCharacter,
    SetSyntaxFromChar,
    ReadDelimitedList,
    ReaderLeftParen,
    ReaderRightParen,
    ReaderQuote,
    ReaderSemicolon,
    ReaderString,
    ReaderBackquote,
    ReaderComma,
    ReaderDispatch,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("COPY-READTABLE", Op::Copy),
    ("READTABLEP", Op::Predicate),
    ("READTABLE-CASE", Op::Case),
    (SET_CASE, Op::SetCase),
    ("GET-MACRO-CHARACTER", Op::GetMacroCharacter),
    ("SET-MACRO-CHARACTER", Op::SetMacroCharacter),
    (
        "MAKE-DISPATCH-MACRO-CHARACTER",
        Op::MakeDispatchMacroCharacter,
    ),
    (
        "GET-DISPATCH-MACRO-CHARACTER",
        Op::GetDispatchMacroCharacter,
    ),
    (
        "SET-DISPATCH-MACRO-CHARACTER",
        Op::SetDispatchMacroCharacter,
    ),
    ("SET-SYNTAX-FROM-CHAR", Op::SetSyntaxFromChar),
    ("READ-DELIMITED-LIST", Op::ReadDelimitedList),
    ("%READER-LEFT-PAREN", Op::ReaderLeftParen),
    ("%READER-RIGHT-PAREN", Op::ReaderRightParen),
    ("%READER-QUOTE", Op::ReaderQuote),
    ("%READER-SEMICOLON", Op::ReaderSemicolon),
    ("%READER-STRING", Op::ReaderString),
    ("%READER-BACKQUOTE", Op::ReaderBackquote),
    ("%READER-COMMA", Op::ReaderComma),
    ("%READER-DISPATCH", Op::ReaderDispatch),
];

pub(super) fn builtin_macro(op: Op) -> Option<crate::read::BuiltinMacro> {
    use crate::read::BuiltinMacro;
    Some(match op {
        Op::ReaderLeftParen => BuiltinMacro::LeftParen,
        Op::ReaderRightParen => BuiltinMacro::RightParen,
        Op::ReaderQuote => BuiltinMacro::Quote,
        Op::ReaderSemicolon => BuiltinMacro::Semicolon,
        Op::ReaderString => BuiltinMacro::String,
        Op::ReaderBackquote => BuiltinMacro::Backquote,
        Op::ReaderComma => BuiltinMacro::Comma,
        Op::ReaderDispatch => BuiltinMacro::Dispatch,
        _ => return None,
    })
}

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Copy => count <= 2,
            Self::Predicate | Self::Case => count == 1,
            Self::SetCase => count == 2,
            Self::GetMacroCharacter => (1..=2).contains(&count),
            Self::SetMacroCharacter => (2..=4).contains(&count),
            Self::MakeDispatchMacroCharacter => (1..=3).contains(&count),
            Self::GetDispatchMacroCharacter => (2..=3).contains(&count),
            Self::SetDispatchMacroCharacter => (3..=4).contains(&count),
            Self::SetSyntaxFromChar => (2..=4).contains(&count),
            Self::ReadDelimitedList => (1..=3).contains(&count),
            Self::ReaderLeftParen
            | Self::ReaderRightParen
            | Self::ReaderQuote
            | Self::ReaderSemicolon
            | Self::ReaderString
            | Self::ReaderBackquote
            | Self::ReaderComma => count == 2,
            Self::ReaderDispatch => (2..=3).contains(&count),
        }
    }
}

pub(super) fn setf_primitive(accessor: &str) -> Option<Word> {
    match accessor {
        "READTABLE-CASE" => Primitive::named(SET_CASE),
        _ => None,
    }
}

const CASES: [(ReadtableCase, &str); 4] = [
    (ReadtableCase::Upcase, "UPCASE"),
    (ReadtableCase::Downcase, "DOWNCASE"),
    (ReadtableCase::Preserve, "PRESERVE"),
    (ReadtableCase::Invert, "INVERT"),
];

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    match op {
        Op::GetMacroCharacter => get_macro_character(runtime, frame, args, values, at),
        Op::SetMacroCharacter => set_macro_character(runtime, frame, args, values, at),
        Op::MakeDispatchMacroCharacter => {
            make_dispatch_macro_character(runtime, frame, args, values, at)
        }
        Op::GetDispatchMacroCharacter => {
            get_dispatch_macro_character(runtime, frame, args, values, at)
        }
        Op::SetDispatchMacroCharacter => {
            set_dispatch_macro_character(runtime, frame, args, values, at)
        }
        Op::SetSyntaxFromChar => set_syntax_from_char(runtime, frame, args, values, at),
        Op::ReadDelimitedList => read_delimited_list(runtime, frame, args, values, at),
        op if let Some(kind) = builtin_macro(op) => {
            let stream = frame.get(args.start);
            let character = character_arg(runtime, frame.get(args.start + 1), at)?;
            let stream = super::streams::require_input_stream(runtime, stream, at)?;
            let operation = crate::read::StreamRead::Macro { kind, character };
            match super::streams::read_form(runtime, stream, operation, at)? {
                Some(form) => values.one(crate::form_value::to_value(runtime, &form)?.slot.get()),
                None => values.resize(0),
            }
            Ok(())
        }
        _ => {
            values.one(run_one(runtime, frame, op, args, at)?);
            Ok(())
        }
    }
}

fn run_one(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let heap = &runtime.heap;
    let arg = |index| frame.get(args.start + index);
    let case_of = |table| {
        heap.readtable_case(table).ok_or_else(|| {
            Fault::type_error(runtime, 
                table,
                "READTABLE",
                EvalError::new(at, "expected a readtable"),
            )
        })
    };
    match op {
        Op::Predicate => Ok(if heap.readtable_case(arg(0)).is_some() {
            Word::T
        } else {
            Word::NIL
        }),
        Op::Case => {
            let case = case_of(arg(0))?;
            let (_, name) = CASES.iter().find(|(mode, _)| *mode == case).unwrap();
            Ok(runtime.intern_package(Package::KEYWORD, name, at)?.0)
        }
        Op::SetCase => {
            case_of(arg(1))?;
            let value = arg(0);
            let case = (heap.symbol_home(value) == Some(Package::KEYWORD))
                .then(|| heap.symbol_name(value))
                .flatten()
                .and_then(|name| {
                    CASES
                        .iter()
                        .find_map(|&(case, spelling)| (spelling == name).then_some(case))
                });
            if let Some(case) = case {
                heap.set_readtable_case(arg(1), case);
                Ok(value)
            } else {
                let expected = [Name::common_lisp("MEMBER")]
                    .into_iter()
                    .chain(CASES.map(|(_, name)| Name::keyword(name)));
                Err(type_error(
                    runtime,
                    value,
                    expected,
                    "expected a readtable case",
                    at,
                )?)
            }
        }
        Op::Copy => {
            let (case, macros, syntax) = if args.is_empty() {
                let symbol = runtime.intern(&Name::common_lisp("*READTABLE*"), at)?;
                let source = symbol_value(runtime, symbol, at)?;
                (
                    case_of(source)?,
                    heap.readtable_macros(source).expect("live *READTABLE*"),
                    heap.readtable_syntax(source).expect("live *READTABLE*"),
                )
            } else if arg(0) == Word::NIL {
                (ReadtableCase::Upcase, Word::NIL, Word::NIL)
            } else {
                copy_readtable_case(runtime, arg(0), at)?;
                (
                    case_of(arg(0))?,
                    heap.readtable_macros(arg(0)).expect("validated readtable"),
                    heap.readtable_syntax(arg(0)).expect("validated readtable"),
                )
            };
            if args.len() < 2 || arg(1) == Word::NIL {
                let macros = runtime.root(macros);
                let syntax = runtime.root(syntax);
                let copy = heap
                    .readtable(case)
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
                heap.set_readtable_macros(copy, macros.slot.get());
                heap.set_readtable_syntax(copy, syntax.slot.get());
                Ok(copy)
            } else {
                let target = arg(1);
                copy_readtable_case(runtime, target, at)?;
                heap.set_readtable_case(target, case);
                heap.set_readtable_macros(target, macros);
                heap.set_readtable_syntax(target, syntax);
                Ok(target)
            }
        }
        _ => unreachable!("multi-valued readtable operations dispatch in run"),
    }
}

fn table_arg(runtime: &Runtime, value: Option<Word>, at: usize) -> Result<Word, Fault> {
    let table = match value {
        None | Some(Word::NIL) => {
            let symbol = runtime.intern(&Name::common_lisp("*READTABLE*"), at)?;
            symbol_value(runtime, symbol, at)?
        }
        Some(table) => table,
    };
    require_readtable(runtime, table, at)
}

fn require_readtable(runtime: &Runtime, table: Word, at: usize) -> Result<Word, Fault> {
    if runtime.heap.readtable_macros(table).is_some() {
        Ok(table)
    } else {
        Err(Fault::type_error(runtime, 
            table,
            "READTABLE",
            EvalError::new(at, "expected a readtable"),
        ))
    }
}

pub(super) fn character_arg(runtime: &Runtime, word: Word, at: usize) -> Result<char, Fault> {
    word.as_character().ok_or_else(|| {
        Fault::type_error(
            runtime,
            word,
            "CHARACTER",
            EvalError::new(at, "expected a character"),
        )
    })
}

fn store_macro(
    runtime: &Runtime,
    table: Word,
    character: char,
    definition: Word,
    at: usize,
) -> Result<(), Fault> {
    let table = runtime.root(table);
    let old = runtime.root(
        runtime
            .heap
            .readtable_macros(table.slot.get())
            .expect("validated readtable"),
    );
    let definition = runtime.root(definition);
    let key = Word::character(character);
    let entry = runtime
        .heap
        .cons(|| (key, definition.slot.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let entry = runtime.root(entry);
    let list = runtime
        .heap
        .cons(|| (entry.slot.get(), old.slot.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    runtime.heap.set_readtable_macros(table.slot.get(), list);
    Ok(())
}

fn simple_definition(
    runtime: &Runtime,
    function: Word,
    non_terminating: bool,
    at: usize,
) -> Result<Word, Fault> {
    let function = runtime.root(function);
    let flag = if non_terminating { Word::T } else { Word::NIL };
    Ok(runtime
        .heap
        .cons(|| (function.slot.get(), flag))
        .map_err(|error| EvalError::out_of_memory(at, error))?)
}

fn dispatch_definition(
    runtime: &Runtime,
    non_terminating: bool,
    entries: Word,
    standard: bool,
    at: usize,
) -> Result<Word, Fault> {
    let entries = runtime.root(entries);
    let flag = if non_terminating { Word::T } else { Word::NIL };
    let tail = runtime
        .heap
        .cons(|| (flag, entries.slot.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let tail = runtime.root(tail);
    let tag = if standard {
        "STANDARD-DISPATCH"
    } else {
        "DISPATCH"
    };
    let dispatch = runtime.root(runtime.intern(&Name::keyword(tag), at)?);
    Ok(runtime
        .heap
        .cons(|| (dispatch.slot.get(), tail.slot.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?)
}

fn remove_syntax_override(
    runtime: &Runtime,
    table: Word,
    character: char,
    at: usize,
) -> Result<(), Fault> {
    let table = runtime.root(table);
    let mut list = runtime
        .heap
        .readtable_syntax(table.slot.get())
        .expect("validated readtable");
    let mut kept = Vec::new();
    let mut removed = false;
    while list != Word::NIL {
        let Some((entry, rest)) = runtime.heap.cons_fields(list) else {
            break;
        };
        let Some((key, _)) = runtime.heap.cons_fields(entry) else {
            break;
        };
        if key != Word::character(character) {
            kept.push(entry);
        } else {
            removed = true;
        }
        list = rest;
    }
    if !removed {
        return Ok(());
    }
    let entries = runtime.root_frame(kept.len() + 1);
    let result = kept.len();
    for (index, entry) in kept.into_iter().enumerate() {
        entries.set(index, entry);
    }
    for index in (0..result).rev() {
        let list = runtime
            .heap
            .cons(|| (entries.get(index), entries.get(result)))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        entries.set(result, list);
    }
    runtime
        .heap
        .set_readtable_syntax(table.slot.get(), entries.get(result));
    Ok(())
}

fn store_syntax(
    runtime: &Runtime,
    table: Word,
    character: char,
    syntax: crate::read::CharSyntax,
    at: usize,
) -> Result<(), Fault> {
    let table = runtime.root(table);
    let old = runtime.root(
        runtime
            .heap
            .readtable_syntax(table.slot.get())
            .expect("validated readtable"),
    );
    let keyword = runtime.root(crate::read::syntax_keyword(runtime, syntax, at)?);
    let entry = runtime
        .heap
        .cons(|| (Word::character(character), keyword.slot.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let entry = runtime.root(entry);
    let list = runtime
        .heap
        .cons(|| (entry.slot.get(), old.slot.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    runtime.heap.set_readtable_syntax(table.slot.get(), list);
    Ok(())
}

fn macro_function(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    if value == Word::NIL {
        return Ok(Word::NIL);
    }
    super::designator(runtime, value, at)
}

fn read_definition(
    runtime: &Runtime,
    table: Word,
    character: char,
) -> Result<Option<crate::read::MacroDefinition>, Fault> {
    crate::read::macro_definition(runtime, table, character).map_err(Fault::from)
}

fn get_macro_character(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let character = character_arg(runtime, frame.get(args.start), at)?;
    let table = table_arg(
        runtime,
        (args.len() > 1).then(|| frame.get(args.start + 1)),
        at,
    )?;
    let Some(definition) = read_definition(runtime, table, character)? else {
        values.resize(2);
        values.slots.set(0, Word::NIL);
        values.slots.set(1, Word::NIL);
        return Ok(());
    };
    let (function, non_terminating, _) = crate::read::macro_function(&definition);
    values.resize(2);
    values.slots.set(0, function);
    values
        .slots
        .set(1, if non_terminating { Word::T } else { Word::NIL });
    Ok(())
}

fn set_macro_character(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let character = character_arg(runtime, frame.get(args.start), at)?;
    let non_terminating = args.len() > 2 && frame.get(args.start + 2) != Word::NIL;
    let table = runtime.root(table_arg(
        runtime,
        (args.len() > 3).then(|| frame.get(args.start + 3)),
        at,
    )?);
    let function = macro_function(runtime, frame.get(args.start + 1), at)?;
    let definition = if function == Word::NIL {
        Word::NIL
    } else {
        simple_definition(runtime, function, non_terminating, at)?
    };
    store_macro(runtime, table.slot.get(), character, definition, at)?;
    remove_syntax_override(runtime, table.slot.get(), character, at)?;
    values.one(Word::T);
    Ok(())
}

fn make_dispatch_macro_character(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let character = character_arg(runtime, frame.get(args.start), at)?;
    let non_terminating = args.len() > 1 && frame.get(args.start + 1) != Word::NIL;
    let table = runtime.root(table_arg(
        runtime,
        (args.len() > 2).then(|| frame.get(args.start + 2)),
        at,
    )?);
    let definition = dispatch_definition(runtime, non_terminating, Word::NIL, false, at)?;
    store_macro(runtime, table.slot.get(), character, definition, at)?;
    remove_syntax_override(runtime, table.slot.get(), character, at)?;
    values.one(Word::T);
    Ok(())
}

fn get_dispatch_macro_character(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let dispatch = character_arg(runtime, frame.get(args.start), at)?;
    let sub = character_arg(runtime, frame.get(args.start + 1), at)?;
    let table = table_arg(
        runtime,
        (args.len() > 2).then(|| frame.get(args.start + 2)),
        at,
    )?;
    let entries = match read_definition(runtime, table, dispatch)? {
        Some(crate::read::MacroDefinition::Dispatch { entries, .. }) => entries,
        Some(crate::read::MacroDefinition::Builtin(crate::read::BuiltinMacro::Dispatch)) => {
            Word::NIL
        }
        _ => {
            return Err(Fault::plain(
                crate::condition::Class::TypeError,
                EvalError::new(at, "expected a dispatch macro character"),
            ));
        }
    };
    values.one(crate::read::dispatch_function(runtime, entries, sub).unwrap_or(Word::NIL));
    Ok(())
}

fn set_dispatch_macro_character(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let dispatch = character_arg(runtime, frame.get(args.start), at)?;
    let sub = character_arg(runtime, frame.get(args.start + 1), at)?;
    if sub.is_ascii_digit() {
        return Err(Fault::plain(
            crate::condition::Class::Error,
            EvalError::new(at, "a digit cannot be a dispatch sub-character"),
        ));
    }
    let table = runtime.root(table_arg(
        runtime,
        (args.len() > 3).then(|| frame.get(args.start + 3)),
        at,
    )?);
    let function = runtime.root(macro_function(runtime, frame.get(args.start + 2), at)?);
    let (non_terminating, entries, standard) =
        match read_definition(runtime, table.slot.get(), dispatch)? {
            Some(crate::read::MacroDefinition::Dispatch {
                non_terminating,
                entries,
                standard,
            }) => (non_terminating, entries, standard),
            Some(crate::read::MacroDefinition::Builtin(crate::read::BuiltinMacro::Dispatch)) => {
                (true, Word::NIL, true)
            }
            _ => {
                return Err(Fault::plain(
                    crate::condition::Class::TypeError,
                    EvalError::new(at, "expected a dispatch macro character"),
                ));
            }
        };
    let key = Word::character(crate::character::simple_uppercase(sub));
    let entries = runtime.root(entries);
    let entry = runtime
        .heap
        .cons(|| (key, function.slot.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let entry = runtime.root(entry);
    let entries = runtime
        .heap
        .cons(|| (entry.slot.get(), entries.slot.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let definition = dispatch_definition(runtime, non_terminating, entries, standard, at)?;
    store_macro(runtime, table.slot.get(), dispatch, definition, at)?;
    values.one(Word::T);
    Ok(())
}

fn set_syntax_from_char(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let to = character_arg(runtime, frame.get(args.start), at)?;
    let from = character_arg(runtime, frame.get(args.start + 1), at)?;
    let to_table = runtime.root(table_arg(
        runtime,
        (args.len() > 2).then(|| frame.get(args.start + 2)),
        at,
    )?);
    let from_table = table_arg(
        runtime,
        (args.len() > 3).then(|| frame.get(args.start + 3)),
        at,
    )?;
    let source = crate::read::effective_syntax(runtime, from_table, from).map_err(Fault::from)?;
    if source == crate::read::CharSyntax::Macro {
        let definition = read_definition(runtime, from_table, from)?
            .expect("a macro character has a definition");
        let definition = match definition {
            crate::read::MacroDefinition::Builtin(kind) => simple_definition(
                runtime,
                crate::read::builtin_function(kind),
                kind.non_terminating(),
                at,
            )?,
            crate::read::MacroDefinition::Custom {
                function,
                non_terminating,
            } => simple_definition(runtime, function, non_terminating, at)?,
            crate::read::MacroDefinition::Dispatch {
                non_terminating,
                entries,
                standard,
            } => dispatch_definition(runtime, non_terminating, entries, standard, at)?,
        };
        store_macro(runtime, to_table.slot.get(), to, definition, at)?;
        remove_syntax_override(runtime, to_table.slot.get(), to, at)?;
    } else {
        let target =
            crate::read::effective_syntax(runtime, to_table.slot.get(), to).map_err(Fault::from)?;
        let syntax = match source {
            crate::read::CharSyntax::Constituent => {
                if matches!(
                    target,
                    crate::read::CharSyntax::Whitespace | crate::read::CharSyntax::Invalid
                ) {
                    crate::read::CharSyntax::Invalid
                } else {
                    crate::read::CharSyntax::Constituent
                }
            }
            syntax => syntax,
        };
        store_macro(runtime, to_table.slot.get(), to, Word::NIL, at)?;
        store_syntax(runtime, to_table.slot.get(), to, syntax, at)?;
    }
    values.one(Word::T);
    Ok(())
}

fn read_delimited_list(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let delimiter = character_arg(runtime, frame.get(args.start), at)?;
    let designator = if args.len() > 1 {
        frame.get(args.start + 1)
    } else {
        Word::NIL
    };
    let stream = super::streams::input_stream(runtime, designator, at)?;
    let recursive = args.len() == 3 && frame.get(args.start + 2) != Word::NIL;
    let operation = crate::read::StreamRead::DelimitedList {
        delimiter,
        recursive,
    };
    let form = super::streams::read_form(runtime, stream, operation, at)?
        .expect("a delimited list returns one object");
    values.one(crate::form_value::to_value(runtime, &form)?.slot.get());
    Ok(())
}

fn copy_readtable_case(runtime: &Runtime, value: Word, at: usize) -> Result<ReadtableCase, Fault> {
    match runtime.heap.readtable_case(value) {
        Some(case) => Ok(case),
        None => Err(type_error(
            runtime,
            value,
            ["OR", "READTABLE", "NULL"].map(Name::common_lisp),
            "expected a readtable or NIL",
            at,
        )?),
    }
}

fn type_error(
    runtime: &Runtime,
    value: Word,
    expected: impl IntoIterator<IntoIter: DoubleEndedIterator, Item = Name>,
    message: &str,
    at: usize,
) -> Result<Fault, Fault> {
    let datum = runtime.root(value);
    let mut specifier = runtime.nil();
    for name in expected.into_iter().rev() {
        let symbol = runtime.root(runtime.intern(&name, at)?);
        specifier = runtime
            .cons(&symbol, &specifier)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    Ok(Fault::type_error_specifier(runtime, 
        datum.slot.get(),
        specifier.slot.get(),
        EvalError::new(at, message),
    ))
}
