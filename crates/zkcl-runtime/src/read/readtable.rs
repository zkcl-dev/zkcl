//! Standard readtable characters and per-readtable syntax overrides.
//!
//! The standard macro characters are implicit: a character without a stored
//! entry uses the standard syntax, so a fresh readtable and a copy of it share
//! the same defaults without allocating them.  SET-MACRO-CHARACTER and
//! SET-SYNTAX-FROM-CHAR store explicit entries; a stored NIL macro entry hides
//! a standard macro character and turns it into a constituent.

use crate::heap::Word;
use crate::symbol::Package;
use crate::{EvalError, Runtime};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinMacro {
    LeftParen,
    RightParen,
    Quote,
    Semicolon,
    String,
    Backquote,
    Comma,
    Dispatch,
}

const BUILTINS: [(char, BuiltinMacro); 8] = [
    ('(', BuiltinMacro::LeftParen),
    (')', BuiltinMacro::RightParen),
    ('\'', BuiltinMacro::Quote),
    (';', BuiltinMacro::Semicolon),
    ('"', BuiltinMacro::String),
    ('`', BuiltinMacro::Backquote),
    (',', BuiltinMacro::Comma),
    ('#', BuiltinMacro::Dispatch),
];

impl BuiltinMacro {
    pub(crate) fn primitive_name(self) -> &'static str {
        match self {
            Self::LeftParen => "%READER-LEFT-PAREN",
            Self::RightParen => "%READER-RIGHT-PAREN",
            Self::Quote => "%READER-QUOTE",
            Self::Semicolon => "%READER-SEMICOLON",
            Self::String => "%READER-STRING",
            Self::Backquote => "%READER-BACKQUOTE",
            Self::Comma => "%READER-COMMA",
            Self::Dispatch => "%READER-DISPATCH",
        }
    }

    pub(crate) fn non_terminating(self) -> bool {
        matches!(self, Self::Dispatch)
    }

    pub(crate) fn from_char(ch: char) -> Option<Self> {
        BUILTINS
            .iter()
            .find_map(|&(standard, kind)| (standard == ch).then_some(kind))
    }

    pub(crate) fn from_function(function: Word) -> Option<Self> {
        BUILTINS
            .iter()
            .map(|&(_, kind)| kind)
            .find(|&kind| builtin_function(kind) == function)
    }
}

pub(crate) fn builtin_function(kind: BuiltinMacro) -> Word {
    crate::vm::Primitive::named(kind.primitive_name()).expect("registered reader macro primitive")
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharSyntax {
    Macro,
    Whitespace,
    Invalid,
    Constituent,
    SingleEscape,
    MultipleEscape,
}

pub(crate) fn default_syntax(ch: char) -> CharSyntax {
    if BuiltinMacro::from_char(ch).is_some() {
        return CharSyntax::Macro;
    }
    match ch {
        ' ' | '\t' | '\n' | '\r' | '\x0c' => CharSyntax::Whitespace,
        '\x08' | '\x7f' => CharSyntax::Invalid,
        '\\' => CharSyntax::SingleEscape,
        '|' => CharSyntax::MultipleEscape,
        _ => CharSyntax::Constituent,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum MacroDefinition {
    Builtin(BuiltinMacro),
    Custom {
        function: Word,
        non_terminating: bool,
    },
    Dispatch {
        non_terminating: bool,
        entries: Word,
        standard: bool,
    },
}

pub(crate) fn dispatch_function(runtime: &Runtime, mut entries: Word, ch: char) -> Option<Word> {
    let key = Word::character(crate::character::simple_uppercase(ch));
    while let Some((entry, rest)) = runtime.heap.cons_fields(entries) {
        let (candidate, function) = runtime.heap.cons_fields(entry)?;
        if key == candidate {
            return (function != Word::NIL).then_some(function);
        }
        entries = rest;
    }
    None
}

pub(crate) fn stored_macro(
    runtime: &Runtime,
    table: Word,
    ch: char,
) -> Result<Option<Word>, EvalError> {
    let mut list = runtime
        .heap
        .readtable_macros(table)
        .ok_or_else(|| EvalError::read(0, "expected a readtable"))?;
    while list != Word::NIL {
        let (entry, rest) = runtime
            .heap
            .cons_fields(list)
            .ok_or_else(|| EvalError::read(0, "malformed readtable macro list"))?;
        let (key, value) = runtime
            .heap
            .cons_fields(entry)
            .ok_or_else(|| EvalError::read(0, "malformed readtable macro entry"))?;
        if key == Word::character(ch) {
            return Ok(Some(value));
        }
        list = rest;
    }
    Ok(None)
}

pub(crate) fn stored_syntax(
    runtime: &Runtime,
    table: Word,
    ch: char,
) -> Result<Option<CharSyntax>, EvalError> {
    let mut list = runtime
        .heap
        .readtable_syntax(table)
        .ok_or_else(|| EvalError::read(0, "expected a readtable"))?;
    while list != Word::NIL {
        let (entry, rest) = runtime
            .heap
            .cons_fields(list)
            .ok_or_else(|| EvalError::read(0, "malformed readtable syntax list"))?;
        let (key, value) = runtime
            .heap
            .cons_fields(entry)
            .ok_or_else(|| EvalError::read(0, "malformed readtable syntax entry"))?;
        if key == Word::character(ch) {
            let syntax = keyword_syntax(runtime, value)
                .ok_or_else(|| EvalError::read(0, "unknown readtable character syntax"))?;
            return Ok(Some(syntax));
        }
        list = rest;
    }
    Ok(None)
}

pub(crate) fn keyword_syntax(runtime: &Runtime, word: Word) -> Option<CharSyntax> {
    if runtime.heap.symbol_home(word) != Some(Package::KEYWORD) {
        return None;
    }
    match runtime.heap.symbol_name(word).as_deref()? {
        "WHITESPACE" => Some(CharSyntax::Whitespace),
        "INVALID" => Some(CharSyntax::Invalid),
        "CONSTITUENT" => Some(CharSyntax::Constituent),
        "SINGLE-ESCAPE" => Some(CharSyntax::SingleEscape),
        "MULTIPLE-ESCAPE" => Some(CharSyntax::MultipleEscape),
        _ => None,
    }
}

pub(crate) fn syntax_keyword(
    runtime: &Runtime,
    syntax: CharSyntax,
    at: usize,
) -> Result<Word, EvalError> {
    let name = match syntax {
        CharSyntax::Whitespace => "WHITESPACE",
        CharSyntax::Invalid => "INVALID",
        CharSyntax::Constituent => "CONSTITUENT",
        CharSyntax::SingleEscape => "SINGLE-ESCAPE",
        CharSyntax::MultipleEscape => "MULTIPLE-ESCAPE",
        CharSyntax::Macro => "MACRO",
    };
    Ok(runtime.intern_package(Package::KEYWORD, name, at)?.0)
}

fn parse_definition(
    runtime: &Runtime,
    definition: Word,
) -> Result<Option<MacroDefinition>, EvalError> {
    let Some((head, tail)) = runtime.heap.cons_fields(definition) else {
        return Ok(None);
    };
    let dispatch = if runtime.heap.symbol_home(head) == Some(Package::KEYWORD) {
        match runtime.heap.symbol_name(head).as_deref() {
            Some("DISPATCH") => Some(false),
            Some("STANDARD-DISPATCH") => Some(true),
            _ => None,
        }
    } else {
        None
    };
    if let Some(standard) = dispatch {
        let Some((flag, entries)) = runtime.heap.cons_fields(tail) else {
            return Ok(None);
        };
        return Ok(Some(MacroDefinition::Dispatch {
            non_terminating: flag != Word::NIL,
            entries,
            standard,
        }));
    }
    if let Some(kind) = BuiltinMacro::from_function(head) {
        return Ok(Some(MacroDefinition::Builtin(kind)));
    }
    Ok(Some(MacroDefinition::Custom {
        function: head,
        non_terminating: tail != Word::NIL,
    }))
}

/// The macro character definition in effect, including the implicit standard
/// definitions.  A syntax override or a stored NIL tombstone suppresses them.
pub(crate) fn macro_definition(
    runtime: &Runtime,
    table: Word,
    ch: char,
) -> Result<Option<MacroDefinition>, EvalError> {
    if let Some(syntax) = stored_syntax(runtime, table, ch)?
        && syntax != CharSyntax::Macro
    {
        return Ok(None);
    }
    match stored_macro(runtime, table, ch)? {
        Some(Word::NIL) => Ok(None),
        Some(definition) => parse_definition(runtime, definition),
        None => Ok(BuiltinMacro::from_char(ch).map(MacroDefinition::Builtin)),
    }
}

pub(crate) fn effective_syntax(
    runtime: &Runtime,
    table: Word,
    ch: char,
) -> Result<CharSyntax, EvalError> {
    if let Some(syntax) = stored_syntax(runtime, table, ch)? {
        return Ok(syntax);
    }
    match stored_macro(runtime, table, ch)? {
        Some(Word::NIL) => Ok(CharSyntax::Constituent),
        Some(_) => Ok(CharSyntax::Macro),
        None => Ok(default_syntax(ch)),
    }
}

pub(crate) fn macro_function(definition: &MacroDefinition) -> (Word, bool, Option<Word>) {
    match *definition {
        MacroDefinition::Builtin(kind) => (builtin_function(kind), kind.non_terminating(), None),
        MacroDefinition::Custom {
            function,
            non_terminating,
        } => (function, non_terminating, None),
        MacroDefinition::Dispatch {
            non_terminating,
            entries,
            ..
        } => (
            builtin_function(BuiltinMacro::Dispatch),
            non_terminating,
            Some(entries),
        ),
    }
}
