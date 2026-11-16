//! Token boundaries used by the runtime reader and the lossless parser.

use std::ops::Range;

use super::{SourceReadtable, SyntaxDiagnosticKind, SyntaxKind};

/// A token in the standard readtable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lexeme {
    pub kind: SyntaxKind,
    pub end: usize,
    pub diagnostic: Option<LexicalDiagnostic>,
}

/// A lexical diagnostic with its starting byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexicalDiagnostic {
    pub kind: SyntaxDiagnosticKind,
    pub start: usize,
}

/// A token with its original spelling and absolute UTF-8 byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexedToken<'a> {
    pub kind: SyntaxKind,
    pub start: usize,
    pub text: &'a str,
    /// A lexical error spanning `diagnostic.start` through the end of this token.
    pub diagnostic: Option<LexicalDiagnostic>,
}

impl LexedToken<'_> {
    pub fn range(&self) -> Range<usize> {
        self.start..self.start + self.text.len()
    }
}

/// Scan a buffer in a fixed readtable, including whitespace, comments and
/// malformed tokens. Each token borrows its spelling from `text`.
///
/// This lexer leaves form grouping and top-level readtable selections to
/// [`super::parse_source_with_readtable`]. A bare dot is emitted as an atom.
///
/// ```
/// use zkcl_syntax::syntax::{SourceReadtable, SyntaxKind, lexer::lex};
///
/// let source = "(list #\\雪)";
/// let character = lex(source, SourceReadtable::Standard)
///     .find(|token| token.kind == SyntaxKind::Character)
///     .unwrap();
/// assert_eq!(&source[character.range()], "#\\雪");
/// ```
pub fn lex(text: &str, readtable: SourceReadtable) -> impl Iterator<Item = LexedToken<'_>> {
    let mut lexer = Lexer::new(text, readtable);
    std::iter::from_fn(move || {
        let start = lexer.offset();
        let Lexeme {
            kind,
            end,
            diagnostic,
        } = lexer.next()?;
        Some(LexedToken {
            kind,
            start,
            text: &text[start..end],
            diagnostic,
        })
    })
}

/// Scan at a UTF-8 character boundary in the standard readtable.
/// Offsets are absolute byte indices in the source.
///
/// # Panics
///
/// Panics if the offset is outside the source or splits a UTF-8 character.
pub fn scan_token(text: &str, offset: usize) -> Option<Lexeme> {
    Lexer::starting_at(text, offset, SourceReadtable::Standard).next()
}

pub(crate) struct Lexer<'a> {
    end: usize,
    rest: &'a [u8],
    pub(super) readtable: SourceReadtable,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(text: &'a str, readtable: SourceReadtable) -> Self {
        Self::starting_at(text, 0, readtable)
    }

    pub(super) fn starting_at(text: &'a str, offset: usize, readtable: SourceReadtable) -> Self {
        Self {
            end: text.len(),
            rest: text.split_at(offset).1.as_bytes(),
            readtable,
        }
    }

    pub(super) fn offset(&self) -> usize {
        self.end - self.rest.len()
    }

    fn peek(&self) -> Option<u8> {
        self.rest.first().copied()
    }

    fn consume(&mut self, expected: u8) -> bool {
        if let Some((&first, rest)) = self.rest.split_first()
            && first == expected
        {
            self.rest = rest;
            true
        } else {
            false
        }
    }

    fn consume_while(&mut self, predicate: impl Fn(u8) -> bool) {
        let len = self
            .rest
            .iter()
            .position(|&byte| !predicate(byte))
            .unwrap_or(self.rest.len());
        self.rest = &self.rest[len..];
    }

    // All delimiters are ASCII. Only escapes, character literals and dispatch
    // subcharacters consume exactly one scalar and therefore need its width.
    fn take_character(&mut self) -> Option<u8> {
        let (&first, rest) = self.rest.split_first()?;
        self.rest = rest;
        if !first.is_ascii() {
            self.consume_while(|byte| byte & 0xc0 == 0x80);
        }
        Some(first)
    }

    fn string(&mut self, start: usize) -> Option<LexicalDiagnostic> {
        loop {
            self.skip_until_either(b'"', b'\\');
            if self.consume(b'"') {
                return None;
            }
            if !self.consume(b'\\') || self.take_character().is_none() {
                break;
            }
        }
        Some(LexicalDiagnostic {
            kind: SyntaxDiagnosticKind::UnterminatedString,
            start,
        })
    }

    fn atom(&mut self) -> Option<LexicalDiagnostic> {
        self.consume_while(|byte| !ATOM_STOP[usize::from(byte)]);
        if matches!(self.peek(), Some(b'\\' | b'|')) {
            self.escaped_atom()
        } else {
            None
        }
    }

    #[cold]
    fn escaped_atom(&mut self) -> Option<LexicalDiagnostic> {
        let mut multiple_escape = None;
        loop {
            match self.rest {
                [b'\\', rest @ ..] => {
                    let start = self.offset();
                    self.rest = rest;
                    if self.take_character().is_none() {
                        return Some(LexicalDiagnostic {
                            kind: SyntaxDiagnosticKind::IncompleteEscape,
                            start,
                        });
                    }
                }
                [b'|', rest @ ..] => {
                    if multiple_escape.take().is_none() {
                        multiple_escape = Some(self.offset());
                    }
                    self.rest = rest;
                }
                _ => break,
            }
            if multiple_escape.is_some() {
                self.skip_until_either(b'\\', b'|');
            } else {
                self.consume_while(|byte| !ATOM_STOP[usize::from(byte)]);
            }
        }
        multiple_escape.map(|start| LexicalDiagnostic {
            kind: SyntaxDiagnosticKind::UnterminatedMultipleEscape,
            start,
        })
    }

    fn skip_until_either(&mut self, a: u8, b: u8) {
        let end = memchr::memchr2(a, b, self.rest).unwrap_or(self.rest.len());
        self.rest = &self.rest[end..];
    }

    fn block_comment(&mut self, start: usize) -> Option<LexicalDiagnostic> {
        let mut depth = 1usize;
        loop {
            self.skip_until_either(b'#', b'|');
            if self.consume(b'#') {
                if self.consume(b'|') {
                    depth += 1;
                }
            } else if self.consume(b'|') {
                if self.consume(b'#') {
                    depth -= 1;
                    if depth == 0 {
                        return None;
                    }
                }
            } else {
                break;
            }
        }
        Some(LexicalDiagnostic {
            kind: SyntaxDiagnosticKind::UnterminatedBlockComment,
            start,
        })
    }

    fn raw_string(&mut self) {
        while let Some(newline) = memchr::memchr(b'\n', self.rest) {
            let next = &self.rest[newline + 1..];
            let indent = next
                .iter()
                .take_while(|&&byte| matches!(byte, b' ' | b'\t'))
                .count();
            if let Some(payload) = next[indent..].strip_prefix(b"#>") {
                self.rest = payload;
            } else {
                let cr = usize::from(newline > 0 && self.rest[newline - 1] == b'\r');
                self.rest = &self.rest[newline - cr..];
                return;
            }
        }
        self.rest = &[];
    }

    fn dispatch(&mut self, start: usize) -> (SyntaxKind, Option<LexicalDiagnostic>) {
        use SyntaxKind::*;

        self.consume_while(|byte| byte.is_ascii_digit());
        match self.take_character() {
            Some(b'>') if self.readtable == SourceReadtable::RawStrings => {
                let diagnostic = (self.offset() != start + 2).then_some(LexicalDiagnostic {
                    kind: SyntaxDiagnosticKind::InvalidRawStringArgument,
                    start,
                });
                self.raw_string();
                (RawString, diagnostic)
            }
            Some(b'|') => (BlockComment, self.block_comment(start)),
            Some(b'(') => (VectorStart, None),
            Some(b'\'') => (FunctionQuote, None),
            Some(b'+' | b'-') => (ReaderConditional, None),
            Some(
                b'.' | b'=' | b'a' | b'A' | b'c' | b'C' | b'p' | b'P' | b's' | b'S' | b'y' | b'Y',
            ) => (DispatchPrefix, None),
            // CLHS 2.4.8.1 treats the first payload character as escaped.
            // The rest of the character name follows ordinary token boundaries.
            Some(b'\\') => match self.take_character() {
                Some(_) => (Character, self.atom()),
                None => (
                    Character,
                    Some(LexicalDiagnostic {
                        kind: SyntaxDiagnosticKind::IncompleteCharacterLiteral,
                        start,
                    }),
                ),
            },
            // The numeric argument is semantic. Under *read-suppress*, even
            // an unnumbered ## still consumes no further input.
            Some(b'#') => (Atom, None),
            Some(b'*') => (Atom, self.atom()),
            Some(b':' | b'b' | b'B' | b'o' | b'O' | b'r' | b'R' | b'x' | b'X') => {
                if self.peek().is_none_or(delimiter) {
                    (
                        Atom,
                        Some(LexicalDiagnostic {
                            kind: SyntaxDiagnosticKind::IncompleteDispatch,
                            start,
                        }),
                    )
                } else {
                    (Atom, self.atom())
                }
            }
            Some(byte) if whitespace(byte) || matches!(byte, b')' | b'<' | b'\x08') => (
                UnknownDispatch,
                Some(LexicalDiagnostic {
                    kind: SyntaxDiagnosticKind::InvalidDispatch,
                    start,
                }),
            ),
            // A non-standard dispatch has no known operand structure here.
            Some(_) => (UnknownDispatch, None),
            None => (
                UnknownDispatch,
                Some(LexicalDiagnostic {
                    kind: SyntaxDiagnosticKind::IncompleteDispatch,
                    start,
                }),
            ),
        }
    }

    pub(crate) fn next(&mut self) -> Option<Lexeme> {
        use SyntaxKind::*;

        let start = self.offset();
        let input = self.rest;
        let (&first, rest) = input.split_first()?;
        self.rest = rest;
        let (kind, diagnostic) = match first {
            b'(' => (OpenParen, None),
            b')' => (CloseParen, None),
            b'\'' => (Quote, None),
            b'`' => (Backquote, None),
            b',' if self.consume(b'@') => (UnquoteSplicing, None),
            b',' if self.consume(b'.') => (UnquoteDestructive, None),
            b',' => (Unquote, None),
            b'"' => (String, self.string(start)),
            b'#' => self.dispatch(start),
            b';' => {
                let end = memchr::memchr(b'\n', self.rest).unwrap_or(self.rest.len());
                self.rest = &self.rest[end..];
                (LineComment, None)
            }
            byte if whitespace(byte) => {
                self.consume_while(whitespace);
                (Whitespace, None)
            }
            _ => {
                if matches!(first, b'\\' | b'|') {
                    self.rest = input;
                }
                (Atom, self.atom())
            }
        };
        Some(Lexeme {
            kind,
            diagnostic,
            end: self.offset(),
        })
    }
}

const fn whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

const fn delimiter(byte: u8) -> bool {
    whitespace(byte) || matches!(byte, b'(' | b')' | b'\'' | b'`' | b',' | b'"' | b';')
}

const ATOM_STOP: [bool; 256] = {
    let mut table = [false; 256];
    let mut byte = 0;
    while byte < table.len() {
        let ch = byte as u8;
        table[byte] = delimiter(ch) || matches!(ch, b'\\' | b'|');
        byte += 1;
    }
    table
};
