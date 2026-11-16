use std::rc::{Rc, Weak};

use crate::heap::{ReadtableCase, RootSlot, Word};
pub(crate) use crate::package::Package;
use crate::package::SymbolStatus;
use crate::{EvalError, Runtime};

/// Compiler keys distinguish symbols even after their package membership changes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum NameKey {
    CommonLisp(Rc<str>),
    Keyword(Rc<str>),
    Symbol(usize),
}

#[derive(Clone, Copy)]
pub(crate) struct SymbolAttributes {
    pub(crate) id: usize,
    pub(crate) home: Option<Package>,
    pub(crate) canonical: bool,
    pub(crate) constant: bool,
    // DEFCONSTANT permits an EQL redefinition; built-in constants do not.
    pub(crate) defined_constant: bool,
    // Keep these bits together so the symbol's binding revision fits where
    // the two separately stored flags and their alignment padding used to be.
    flags: u8,
}

impl SymbolAttributes {
    pub(crate) const SPECIAL: u8 = 1;
    pub(crate) const MACRO: u8 = 2;

    pub(crate) fn new(id: usize, home: Option<Package>, canonical: bool, constant: bool) -> Self {
        Self {
            id,
            home,
            canonical,
            constant,
            defined_constant: false,
            flags: 0,
        }
    }

    pub(crate) fn has_flag(self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    pub(crate) fn with_flag(mut self, flag: u8, set: bool) -> Self {
        self.flags = (self.flags & !flag) | (u8::from(set) * flag);
        self
    }

    pub(crate) fn is_constant_variable(self) -> bool {
        self.constant || self.defined_constant || self.home == Some(Package::KEYWORD)
    }
}

#[derive(Clone)]
pub(crate) struct Name(Rc<SymbolName>);

struct SymbolName {
    key: NameKey,
    spelling: Rc<str>,
    home: Option<Package>,
    constant: bool,
    root: Option<RootSlot>,
}

// Reading another occurrence can reuse the form's rooted symbol and spelling.
// A cache entry never extends the Lisp object's lifetime.
#[derive(Default)]
pub(crate) struct ReadSymbolCache([Weak<SymbolName>; 32]);

#[derive(Default)]
pub(crate) struct SymbolTokenBuffer {
    package: String,
    spelling: String,
}

impl PartialEq for Name {
    fn eq(&self, other: &Self) -> bool {
        self.0.key == other.0.key
    }
}

impl Eq for Name {}

impl Name {
    /// A reference produced by the compiler to a standard symbol or private operator.
    pub(crate) fn common_lisp(spelling: &str) -> Self {
        let name: Rc<str> = spelling.into();
        Self(Rc::new(SymbolName {
            key: NameKey::CommonLisp(Rc::clone(&name)),
            spelling: name,
            home: Some(Package::COMMON_LISP),
            constant: matches!(spelling, "T" | "NIL"),
            root: None,
        }))
    }

    /// A compiler-generated keyword reference is interned during materialization.
    pub(crate) fn keyword(spelling: &str) -> Self {
        let name: Rc<str> = spelling.into();
        Self(Rc::new(SymbolName {
            key: NameKey::Keyword(Rc::clone(&name)),
            spelling: name,
            home: Some(Package::KEYWORD),
            constant: true,
            root: None,
        }))
    }

    pub(crate) fn from_word(runtime: &Runtime, word: Word) -> Option<Self> {
        let attributes = runtime.heap.symbol_attributes(word)?;
        let spelling = || match word {
            Word::NIL => Rc::from("NIL"),
            Word::T => Rc::from("T"),
            _ => {
                let name = runtime.heap.symbol_name_word(word).expect("symbol name");
                runtime
                    .heap
                    .with_string_slice(name, |spelling| Rc::from(spelling))
                    .expect("symbol name is a string")
            }
        };
        Some(Self::from_symbol(runtime, word, attributes, spelling))
    }

    fn from_symbol(
        runtime: &Runtime,
        word: Word,
        attributes: SymbolAttributes,
        spelling: impl FnOnce() -> Rc<str>,
    ) -> Self {
        let mut names = runtime.read_symbol_cache.borrow_mut();
        let index = attributes.id % names.0.len();
        let slot = &mut names.0[index];
        if let Some(old) = slot.upgrade()
            && old.home == attributes.home
            && old.constant == attributes.constant
            && old.root.as_ref().is_some_and(|root| root.get() == word)
        {
            return Self(old);
        }
        // A live symbol keeps its name. Its home and constant status may change.
        let spelling = spelling();
        let key = if attributes.canonical {
            NameKey::CommonLisp(Rc::clone(&spelling))
        } else {
            NameKey::Symbol(attributes.id)
        };
        let name = Rc::new(SymbolName {
            key,
            spelling,
            home: attributes.home,
            constant: attributes.constant,
            root: Some(runtime.heap.root_for_host(word)),
        });
        *slot = Rc::downgrade(&name);
        Self(name)
    }

    pub(crate) fn key(&self) -> &NameKey {
        &self.0.key
    }

    pub(crate) fn package(&self) -> Option<Package> {
        self.0.home
    }

    pub(crate) fn spelling(&self) -> &str {
        &self.0.spelling
    }

    pub(crate) fn rooted_word(&self) -> Option<Word> {
        self.0.root.as_ref().map(|root| root.get())
    }

    pub(crate) fn operator(&self) -> &str {
        if matches!(self.0.key, NameKey::CommonLisp(_)) {
            self.spelling()
        } else {
            ""
        }
    }

    pub(crate) fn constant(&self) -> bool {
        self.0.constant
    }

    pub(crate) fn read(runtime: &Runtime, token: &str, at: usize) -> Result<Self, EvalError> {
        if potential_number(token, 10) {
            return Err(EvalError::read(at, "expected a symbol name"));
        }
        Self::read_in_package(
            runtime,
            token,
            at,
            runtime.current_package_at(at)?,
            ReadtableCase::Upcase,
            &mut SymbolTokenBuffer::default(),
        )
    }

    pub(crate) fn read_in_package(
        runtime: &Runtime,
        token: &str,
        at: usize,
        default_package: Package,
        case: ReadtableCase,
        buffer: &mut SymbolTokenBuffer,
    ) -> Result<Self, EvalError> {
        let prefix = buffer.parse_name(token, at, case)?;
        let spelling = buffer.spelling.as_str();
        let (package, external) = match prefix {
            None => (default_package, false),
            Some(Prefix::Keyword) => (Package::KEYWORD, false),
            Some(Prefix::Named { external }) => (
                runtime
                    .packages
                    .borrow()
                    .named_from(Some(default_package), &buffer.package)
                    .ok_or_else(|| {
                        EvalError::read(at, format!("unknown package {:?}", buffer.package))
                    })?,
                external,
            ),
        };
        let word = if external {
            match runtime.packages.borrow().present(package, spelling) {
                Some((word, SymbolStatus::External)) => word,
                _ => {
                    return Err(EvalError::read(
                        at,
                        format!("no such external symbol {token}"),
                    ));
                }
            }
        } else {
            runtime.intern_package(package, spelling, at)?.0
        };
        let attributes = runtime
            .heap
            .symbol_attributes(word)
            .expect("package entries hold symbols");
        Ok(Self::from_symbol(runtime, word, attributes, || {
            spelling.into()
        }))
    }

    pub(crate) fn read_uninterned(
        runtime: &Runtime,
        token: &str,
        at: usize,
        case: ReadtableCase,
        buffer: &mut SymbolTokenBuffer,
    ) -> Result<Self, EvalError> {
        let prefix = buffer.parse_name(token, at + 2, case)?;
        if prefix.is_some() {
            return Err(EvalError::read_span(
                at..at + 2 + token.len(),
                "an uninterned symbol cannot have a package prefix",
            ));
        }
        let spelling = buffer.spelling.as_str();
        let (_, word) = runtime.fresh_symbol(spelling, at)?;
        let attributes = runtime.heap.symbol_attributes(word).expect("fresh symbol");
        Ok(Self::from_symbol(runtime, word, attributes, || {
            spelling.into()
        }))
    }
}

enum Prefix {
    Keyword,
    Named { external: bool },
}

impl SymbolTokenBuffer {
    fn parse_name(
        &mut self,
        token: &str,
        at: usize,
        case: ReadtableCase,
    ) -> Result<Option<Prefix>, EvalError> {
        let invalid = || EvalError::read(at, format!("invalid package syntax {token}"));
        self.spelling.clear();
        if case == ReadtableCase::Upcase
            && token.as_bytes().first().is_some_and(|&first| first != b'.')
            && token
                .bytes()
                .all(|byte| is_symbol_constituent(char::from(byte)))
        {
            self.spelling.push_str(token);
            self.spelling.make_ascii_uppercase();
            return Ok(None);
        }

        let mut prefix = None;
        let mut name_start = 0;
        let mut in_multiple_escape = false;
        let mut fold = name_case(case, token);
        let mut chars = token.char_indices().peekable();
        while let Some((index, ch)) = chars.next() {
            match ch {
                '\\' => {
                    let (_, escaped) = chars.next().expect("lexer validated the single escape");
                    self.spelling.push(escaped);
                }
                '|' => in_multiple_escape = !in_multiple_escape,
                ':' if !in_multiple_escape => {
                    if prefix.is_some() {
                        return Err(invalid());
                    }
                    let internal = chars.next_if(|&(_, next)| next == ':').is_some();
                    if index == 0 && internal {
                        return Err(invalid());
                    }
                    prefix = Some(if index == 0 {
                        Prefix::Keyword
                    } else {
                        std::mem::swap(&mut self.spelling, &mut self.package);
                        self.spelling.clear();
                        Prefix::Named {
                            external: !internal,
                        }
                    });
                    name_start = index + 1 + usize::from(internal);
                    fold = name_case(case, &token[name_start..]);
                }
                ch if in_multiple_escape
                    && (!ch.is_ascii_control() || matches!(ch, '\t' | '\n' | '\r' | '\x0c')) =>
                {
                    self.spelling.push(ch);
                }
                ch if is_symbol_constituent(ch) => {
                    self.spelling.push(fold.fold(ch));
                }
                ch => {
                    return Err(EvalError::read(
                        at + index,
                        format!("unsupported reader constituent {ch:?}"),
                    ));
                }
            }
        }
        if token.len() == name_start {
            return Err(invalid());
        }
        if token.bytes().all(|byte| byte == b'.') {
            return Err(EvalError::read(at, "invalid token of dots"));
        }
        Ok(prefix)
    }
}

fn name_case(case: ReadtableCase, token: &str) -> ReadtableCase {
    if case != ReadtableCase::Invert {
        return case;
    }
    // A package marker separates the package and symbol names. Escapes
    // contribute no letters to either name's case classification.
    let mut chars = token.chars();
    let mut escaped = false;
    ReadtableCase::inverted(std::iter::from_fn(|| {
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    chars.next();
                }
                '|' => escaped = !escaped,
                ':' if !escaped => return None,
                _ if !escaped => return Some(ch),
                _ => {}
            }
        }
        None
    }))
}

/// The text of a string designator: a string, a symbol, or a character.
///
/// A zero-length vector is accepted as the empty string. The heap normalizes
/// the element type of `(make-array 0 :element-type nil)`, so the empty
/// vector cannot be distinguished from an empty general vector afterwards.
pub(crate) fn designator_text(runtime: &Runtime, word: Word) -> Option<String> {
    runtime
        .heap
        .string_contents(word)
        .or_else(|| runtime.heap.symbol_name(word))
        .or_else(|| word.as_character().map(String::from))
        .or_else(|| empty_vector_text(runtime, word))
}

/// The text of a string, accepting a zero-length vector as the empty string.
pub(crate) fn string_text(runtime: &Runtime, word: Word) -> Option<String> {
    runtime
        .heap
        .string_contents(word)
        .or_else(|| empty_vector_text(runtime, word))
}

fn empty_vector_text(runtime: &Runtime, word: Word) -> Option<String> {
    let info = runtime.heap.array_info(word)?;
    (info.rank == 1 && info.size == 0 && info.fill_pointer.is_none()).then(String::new)
}

pub(crate) fn is_symbol_constituent(ch: char) -> bool {
    matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '!' | '#'..='&' | '*' | '+' | '-'
        | '.' | '/' | '<'..='@' | '[' | ']' | '^' | '_' | '{' | '}' | '~')
}

// CLHS 2.3.1.1. Letter digits cannot participate in a token containing a point.
// zkcl treats tokens starting with underscore as symbols.
pub(crate) fn potential_number(token: &str, base: u32) -> bool {
    let bytes = token.as_bytes();
    let base = if bytes.contains(&b'.') { 10 } else { base };
    let is_digit = |byte: u8| char::from(byte).is_digit(base);
    let is_marker = |byte: u8| {
        matches!(byte.to_ascii_lowercase(), b'd' | b'e' | b'f' | b'l' | b's') && !is_digit(byte)
    };
    bytes
        .first()
        .is_some_and(|byte| is_digit(*byte) || b"+-.^".contains(byte))
        && !token.ends_with(['+', '-'])
        && bytes.iter().copied().any(is_digit)
        && bytes
            .iter()
            .all(|byte| is_digit(*byte) || is_marker(*byte) || b"+-/.^_".contains(byte))
        && !bytes.windows(2).any(|pair| {
            pair.iter().all(u8::is_ascii_alphabetic) && pair.iter().copied().any(is_marker)
        })
}

impl ReadSymbolCache {
    pub(crate) fn clear_image_cache(&mut self) {
        *self = Self::default();
    }
}
