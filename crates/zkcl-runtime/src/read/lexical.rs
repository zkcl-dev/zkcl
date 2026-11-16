//! Token boundaries and spelling under the standard and custom readtables.

use std::borrow::Cow;
use std::ops::Range;

use zkcl_syntax::syntax::lexer::{Lexeme, LexicalDiagnostic, scan_token};
use zkcl_syntax::syntax::{SyntaxDiagnosticKind, SyntaxKind};

use super::{
    BuiltinMacro, CharSyntax, MacroDefinition, ReadMode, ReadNesting, Reader, ReaderInput, Token,
    dispatch_character, effective_syntax, macro_definition,
};
use crate::EvalError;
use crate::heap::Word;

impl<'a> Reader<'a> {
    fn syntax(&self, ch: char) -> Result<CharSyntax, EvalError> {
        effective_syntax(self.runtime, self.current_table(), ch)
    }

    fn is_atom_delimiter(
        &self,
        table: Word,
        ch: char,
        syntax: CharSyntax,
    ) -> Result<bool, EvalError> {
        match syntax {
            CharSyntax::Whitespace => Ok(true),
            CharSyntax::Macro => Ok(match macro_definition(self.runtime, table, ch)? {
                Some(MacroDefinition::Builtin(kind)) => !kind.non_terminating(),
                Some(
                    MacroDefinition::Custom {
                        non_terminating, ..
                    }
                    | MacroDefinition::Dispatch {
                        non_terminating, ..
                    },
                ) => !non_terminating,
                None => false,
            }),
            _ => Ok(false),
        }
    }

    /// Escape spellings for symbol and number parsing.  The readtable's
    /// escape and constituent syntax is mapped onto the standard text so the
    /// existing token parsing reads the original character names.
    fn name_text(&self, raw: &str) -> Result<String, EvalError> {
        let mut text = String::with_capacity(raw.len() + 2);
        let mut escaped = false;
        let mut in_multiple_escape = false;
        for ch in raw.chars() {
            if escaped {
                text.push(ch);
                escaped = false;
                continue;
            }
            let syntax = self.syntax(ch)?;
            if in_multiple_escape {
                match syntax {
                    CharSyntax::SingleEscape => {
                        text.push('\\');
                        escaped = true;
                    }
                    CharSyntax::MultipleEscape => {
                        text.push('|');
                        in_multiple_escape = false;
                    }
                    // Every character is ordinary inside multiple escapes, but
                    // SYMBOL parsing rejects control characters unless they are
                    // escaped, so keep the escape spelling.
                    _ if crate::symbol::is_symbol_constituent(ch) => text.push(ch),
                    _ => {
                        text.push('\\');
                        text.push(ch);
                    }
                }
                continue;
            }
            match syntax {
                CharSyntax::SingleEscape => {
                    text.push('\\');
                    escaped = true;
                }
                CharSyntax::MultipleEscape => {
                    text.push('|');
                    in_multiple_escape = true;
                }
                CharSyntax::Constituent | CharSyntax::Macro => {
                    if ch == ':' || crate::symbol::is_symbol_constituent(ch) {
                        text.push(ch);
                    } else {
                        text.push('\\');
                        text.push(ch);
                    }
                }
                CharSyntax::Whitespace | CharSyntax::Invalid => text.push(ch),
            }
        }
        Ok(text)
    }

    fn character_text(&self, raw: &str) -> String {
        let mut chars = raw.chars();
        if chars.next().is_none() {
            return raw.to_owned();
        }
        let mut digits = String::new();
        let mut rest = chars.as_str();
        while let Some(ch) = rest.chars().next() {
            if ch.is_ascii_digit() {
                digits.push(ch);
                rest = &rest[1..];
            } else {
                break;
            }
        }
        let mut iter = rest.chars();
        let _ = iter.next();
        format!("#{digits}\\{}", iter.as_str())
    }

    fn dispatch_text(&self, raw: &str) -> Result<String, EvalError> {
        let mut chars = raw.chars();
        let _ = chars.next();
        let mut text = String::with_capacity(raw.len() + 2);
        text.push('#');
        text.push_str(&self.name_text(chars.as_str())?);
        Ok(text)
    }

    pub(super) fn next_standard_text_token(
        &mut self,
        text: &'a str,
        mode: ReadMode,
        nesting: ReadNesting,
    ) -> Result<Option<Token<'a>>, EvalError> {
        while let Some(&first) = text.as_bytes().get(self.at) {
            if let Some((depth, delimiter)) = self.delimiter
                && depth == nesting.depth
                && text[self.at..].starts_with(delimiter)
            {
                return Ok(None);
            }
            if matches!(first, b'\x08' | b'\x7f') {
                return Err(EvalError::read(self.at, "invalid character in input"));
            }
            let lexeme = scan_token(text, self.at).expect("token starts within input");
            if matches!(
                lexeme.kind,
                SyntaxKind::Whitespace | SyntaxKind::LineComment
            ) {
                self.at = lexeme.end;
            } else if let Some(token) = self.scanned_token(lexeme, false, true, mode, false)?
                && !(token.kind == SyntaxKind::UnknownDispatch && mode == ReadMode::Suppressed)
            {
                return Ok(Some(token));
            }
        }
        Ok(None)
    }

    pub(super) fn scan_buffered_token(
        &mut self,
        table: Word,
        syntax: CharSyntax,
        standard: bool,
        builtin: Option<BuiltinMacro>,
        mode: ReadMode,
    ) -> Result<Option<Token<'a>>, EvalError> {
        let (lexeme, plain_atom) = self.lexeme(table, syntax, standard, builtin)?;
        let dispatch = matches!(builtin, Some(BuiltinMacro::Dispatch));
        self.scanned_token(lexeme, plain_atom, standard, mode, dispatch)
    }

    fn lexeme(
        &mut self,
        table: Word,
        syntax: CharSyntax,
        standard: bool,
        builtin: Option<BuiltinMacro>,
    ) -> Result<(Lexeme, bool), EvalError> {
        loop {
            let (lexeme, plain) = if standard {
                (
                    scan_token(self.input.text(), self.at).expect("token starts within input"),
                    false,
                )
            } else {
                self.scan_readtable_token(table, syntax, builtin)?
            };
            if lexeme.end == self.input.text().len()
                && !self.input.ends_at_eof()
                && self.token_needs_lookahead(&lexeme, syntax)
            {
                if self.input.refill(self.runtime, self.at) {
                    continue;
                }
                if !self.input.ends_at_eof() {
                    let end = self.input.location(lexeme.end);
                    return Err(EvalError::unexpected_read_eof(
                        end..end,
                        "expected the rest of a token",
                    ));
                }
            }
            return Ok((lexeme, plain));
        }
    }

    fn token_needs_lookahead(&self, lexeme: &Lexeme, syntax: CharSyntax) -> bool {
        if let Some(diagnostic) = &lexeme.diagnostic {
            return diagnostic.kind != SyntaxDiagnosticKind::InvalidDispatch;
        }
        match lexeme.kind {
            SyntaxKind::Atom => {
                // #n# terminates at its final dispatch character. Symbols,
                // including those ending in a constituent #, need a delimiter.
                let raw = &self.input.text()[self.at..lexeme.end];
                let mut chars = raw.chars();
                chars.next();
                !(syntax == CharSyntax::Macro
                    && chars
                        .as_str()
                        .strip_suffix('#')
                        .is_some_and(|digits| digits.bytes().all(|ch| ch.is_ascii_digit())))
            }
            SyntaxKind::LineComment => !self.input.text()[self.at..lexeme.end].ends_with('\n'),
            SyntaxKind::Character | SyntaxKind::Unquote => true,
            _ => false,
        }
    }

    fn scanned_token(
        &mut self,
        lexeme: Lexeme,
        plain_atom: bool,
        standard: bool,
        mode: ReadMode,
        dispatch: bool,
    ) -> Result<Option<Token<'a>>, EvalError> {
        let at = self.at;
        let end = lexeme.end;
        self.at = end;
        let ignored = lexeme.kind.is_trivia();
        // These delimiters and quotation tokens are handled by kind and span.
        let text = if ignored
            || matches!(
                lexeme.kind,
                SyntaxKind::OpenParen
                    | SyntaxKind::CloseParen
                    | SyntaxKind::Quote
                    | SyntaxKind::Backquote
                    | SyntaxKind::Unquote
                    | SyntaxKind::UnquoteSplicing
                    | SyntaxKind::UnquoteDestructive
            ) {
            Cow::Borrowed("")
        } else {
            self.token_text(lexeme.kind, at..end, plain_atom, standard, dispatch)?
        };
        if let Some(diagnostic) = lexeme.diagnostic {
            let missing_extended_token = diagnostic.kind
                == SyntaxDiagnosticKind::IncompleteDispatch
                && matches!(
                    dispatch_character(&text),
                    Some(b':' | b'B' | b'O' | b'R' | b'X')
                );
            if !(mode != ReadMode::Normal && missing_extended_token) {
                let range = self.input.location(diagnostic.start)..self.input.location(end);
                let message = diagnostic.kind.message();
                let is_eof = lexeme.end == self.input.text().len()
                    && !missing_extended_token
                    && diagnostic.kind != SyntaxDiagnosticKind::InvalidDispatch;
                return Err(if is_eof {
                    EvalError::unexpected_read_eof(range, message)
                } else {
                    EvalError::read_span(range, message)
                });
            }
        }
        if mode == ReadMode::Normal
            && matches!(
                lexeme.kind,
                SyntaxKind::BlockComment | SyntaxKind::FunctionQuote
            )
            && self.input.text()[at..end]
                .chars()
                .nth(1)
                .is_some_and(|ch| ch.is_ascii_digit())
        {
            return Err(EvalError::read_span(
                self.input.location(at)..self.input.location(end),
                "reader dispatch does not accept a numeric argument",
            ));
        }
        Ok((!ignored).then(|| Token {
            kind: lexeme.kind,
            text,
            at: self.input.location(at),
            end: self.input.location(end),
            evaluated: None,
        }))
    }

    fn scan_readtable_token(
        &self,
        table: Word,
        first_syntax: CharSyntax,
        builtin: Option<BuiltinMacro>,
    ) -> Result<(Lexeme, bool), EvalError> {
        use SyntaxKind::*;

        let start = self.at;
        let source = self.input.text();
        let mut chars = source[start..].chars();
        let first = chars.next().expect("token starts within input");
        let total = source.len();
        let offset = |chars: &std::str::Chars<'_>| total - chars.as_str().len();
        let peek = |chars: &std::str::Chars<'_>| chars.as_str().chars().next();

        // Atoms consisting entirely of ordinary symbol characters already have
        // the spelling the number and symbol parsers expect.
        let mut plain_atom = first_syntax == CharSyntax::Constituent;
        let mut scan_atom = |chars: &mut std::str::Chars<'_>,
                             mut pending: Option<char>|
         -> Result<Option<LexicalDiagnostic>, EvalError> {
            let mut multiple_escape = None;
            loop {
                let ch_offset = if pending.is_some() {
                    start
                } else {
                    offset(chars)
                };
                let (ch, syntax) = match pending.take() {
                    Some(ch) => (ch, first_syntax),
                    None => {
                        let Some(ch) = peek(chars) else {
                            break;
                        };
                        let syntax = effective_syntax(self.runtime, table, ch)?;
                        if multiple_escape.is_none() && self.is_atom_delimiter(table, ch, syntax)? {
                            break;
                        }
                        chars.next();
                        (ch, syntax)
                    }
                };
                plain_atom = plain_atom
                    && matches!(syntax, CharSyntax::Constituent | CharSyntax::Macro)
                    && (ch == ':' || crate::symbol::is_symbol_constituent(ch));
                match syntax {
                    CharSyntax::Invalid if multiple_escape.is_none() => {
                        return Err(EvalError::read(
                            self.input.location(ch_offset),
                            "invalid character in input",
                        ));
                    }
                    CharSyntax::SingleEscape => {
                        if chars.next().is_none() {
                            return Ok(Some(LexicalDiagnostic {
                                kind: SyntaxDiagnosticKind::IncompleteEscape,
                                start: ch_offset,
                            }));
                        }
                    }
                    CharSyntax::MultipleEscape => {
                        multiple_escape = if multiple_escape.is_some() {
                            None
                        } else {
                            Some(ch_offset)
                        };
                    }
                    _ => {}
                }
            }
            Ok(multiple_escape.map(|start| LexicalDiagnostic {
                kind: SyntaxDiagnosticKind::UnterminatedMultipleEscape,
                start,
            }))
        };

        let (kind, diagnostic) = match first_syntax {
            CharSyntax::Whitespace => {
                while let Some(next_ch) = peek(&chars) {
                    if effective_syntax(self.runtime, table, next_ch)? != CharSyntax::Whitespace {
                        break;
                    }
                    chars.next();
                }
                (Whitespace, None)
            }
            CharSyntax::Macro => {
                let kind = builtin.expect("custom macros are handled before token scanning");
                match kind {
                    BuiltinMacro::LeftParen => (OpenParen, None),
                    BuiltinMacro::RightParen => (CloseParen, None),
                    BuiltinMacro::Quote => (Quote, None),
                    BuiltinMacro::Backquote => (Backquote, None),
                    BuiltinMacro::Comma => {
                        if peek(&chars) == Some('@') {
                            chars.next();
                            (UnquoteSplicing, None)
                        } else if peek(&chars) == Some('.') {
                            chars.next();
                            (UnquoteDestructive, None)
                        } else {
                            (Unquote, None)
                        }
                    }
                    BuiltinMacro::Semicolon => {
                        for ch in chars.by_ref() {
                            if ch == '\n' {
                                break;
                            }
                        }
                        (LineComment, None)
                    }
                    BuiltinMacro::String => {
                        let mut diag = Some(LexicalDiagnostic {
                            kind: SyntaxDiagnosticKind::UnterminatedString,
                            start,
                        });
                        while let Some(ch) = chars.next() {
                            if ch == first {
                                diag = None;
                                break;
                            }
                            if effective_syntax(self.runtime, table, ch)?
                                == CharSyntax::SingleEscape
                                && chars.next().is_none()
                            {
                                break;
                            }
                        }
                        (String, diag)
                    }
                    BuiltinMacro::Dispatch => {
                        while peek(&chars).is_some_and(|ch| ch.is_ascii_digit()) {
                            chars.next();
                        }
                        match chars.next() {
                            Some('|') => {
                                let mut depth = 1usize;
                                let mut diag = Some(LexicalDiagnostic {
                                    kind: SyntaxDiagnosticKind::UnterminatedBlockComment,
                                    start,
                                });
                                while let Some(ch) = chars.next() {
                                    if ch == '#' && peek(&chars) == Some('|') {
                                        chars.next();
                                        depth += 1;
                                    } else if ch == '|' && peek(&chars) == Some('#') {
                                        chars.next();
                                        depth -= 1;
                                        if depth == 0 {
                                            diag = None;
                                            break;
                                        }
                                    }
                                }
                                (BlockComment, diag)
                            }
                            Some('(') => (VectorStart, None),
                            Some('\'') => (FunctionQuote, None),
                            Some('+' | '-') => (ReaderConditional, None),
                            Some(
                                '.' | '=' | 'a' | 'A' | 'c' | 'C' | 'p' | 'P' | 's' | 'S' | 'y'
                                | 'Y',
                            ) => (DispatchPrefix, None),
                            Some('\\') => match chars.next() {
                                Some(_) => (Character, scan_atom(&mut chars, None)?),
                                None => (
                                    Character,
                                    Some(LexicalDiagnostic {
                                        kind: SyntaxDiagnosticKind::IncompleteCharacterLiteral,
                                        start,
                                    }),
                                ),
                            },
                            Some('#') => (Atom, None),
                            Some('*') => (Atom, scan_atom(&mut chars, None)?),
                            Some(':' | 'b' | 'B' | 'o' | 'O' | 'r' | 'R' | 'x' | 'X') => {
                                let at_delimiter = match peek(&chars) {
                                    Some(ch) => self.is_atom_delimiter(
                                        table,
                                        ch,
                                        effective_syntax(self.runtime, table, ch)?,
                                    )?,
                                    None => true,
                                };
                                if at_delimiter {
                                    (
                                        Atom,
                                        Some(LexicalDiagnostic {
                                            kind: SyntaxDiagnosticKind::IncompleteDispatch,
                                            start,
                                        }),
                                    )
                                } else {
                                    (Atom, scan_atom(&mut chars, None)?)
                                }
                            }
                            Some(ch)
                                if effective_syntax(self.runtime, table, ch)?
                                    == CharSyntax::Whitespace
                                    || matches!(ch, ')' | '<' | '\x08') =>
                            {
                                (
                                    UnknownDispatch,
                                    Some(LexicalDiagnostic {
                                        kind: SyntaxDiagnosticKind::InvalidDispatch,
                                        start,
                                    }),
                                )
                            }
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
                }
            }
            CharSyntax::Constituent | CharSyntax::SingleEscape | CharSyntax::MultipleEscape => {
                (Atom, scan_atom(&mut chars, Some(first))?)
            }
            CharSyntax::Invalid => {
                return Err(EvalError::read(
                    self.input.location(start),
                    "invalid character in input",
                ));
            }
        };

        Ok((
            Lexeme {
                kind,
                end: offset(&chars),
                diagnostic,
            },
            plain_atom,
        ))
    }

    fn token_text(
        &mut self,
        kind: SyntaxKind,
        range: Range<usize>,
        plain_atom: bool,
        standard: bool,
        dispatch: bool,
    ) -> Result<Cow<'a, str>, EvalError> {
        let raw = &self.input.text()[range.clone()];
        if kind == SyntaxKind::Atom {
            if dispatch {
                return self.dispatch_text(raw).map(Cow::Owned);
            }
            let plain = plain_atom
                || (standard
                    && raw.bytes().all(|byte| {
                        byte == b':' || crate::symbol::is_symbol_constituent(char::from(byte))
                    }));
            if !plain {
                return self.name_text(raw).map(Cow::Owned);
            }
            return Ok(match self.input {
                ReaderInput::Text(source) => Cow::Borrowed(&source[range]),
                ReaderInput::Stream(_) => {
                    // Stream text can move on refill. Reuse the spelling
                    // buffer once the preceding atom has been interpreted.
                    let mut text = std::mem::take(&mut self.stream_atom_text);
                    text.clear();
                    text.push_str(raw);
                    Cow::Owned(text)
                }
            });
        }
        if standard {
            return Ok(self.input.slice(range));
        }
        let text = match kind {
            SyntaxKind::Character => self.character_text(raw),
            SyntaxKind::DispatchPrefix
            | SyntaxKind::FunctionQuote
            | SyntaxKind::ReaderConditional
            | SyntaxKind::VectorStart
            | SyntaxKind::UnknownDispatch => dispatch_prefix_text(raw),
            _ => return Ok(self.input.slice(range)),
        };
        Ok(Cow::Owned(text))
    }

    pub(super) fn dispatch_prefix(
        &mut self,
        start: usize,
    ) -> Result<(Cow<'a, str>, char, usize), EvalError> {
        let mut cursor = start;
        loop {
            let text = self.input.text();
            cursor += text[cursor..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            if let Some(sub) = text[cursor..].chars().next() {
                return Ok((
                    self.input.slice(start..cursor),
                    sub,
                    cursor + sub.len_utf8(),
                ));
            }
            if !self.input.refill(self.runtime, start) {
                let at = self.input.location(cursor);
                return Err(EvalError::unexpected_read_eof(
                    at..at,
                    "expected a dispatch sub-character",
                ));
            }
        }
    }

    pub(super) fn decode_string(&self, raw: &str) -> Result<String, EvalError> {
        if self.is_standard_readtable(self.current_table()) {
            return Ok(decode_standard_string(raw));
        }
        let mut chars = raw.chars();
        chars.next();
        chars.next_back();
        let mut text = String::with_capacity(raw.len());
        while let Some(ch) = chars.next() {
            if self.syntax(ch)? == CharSyntax::SingleEscape {
                text.push(chars.next().expect("lexer validated the string escape"));
            } else {
                text.push(ch);
            }
        }
        Ok(text)
    }
}

fn dispatch_prefix_text(raw: &str) -> String {
    let mut chars = raw.chars();
    let _ = chars.next();
    format!("#{}", chars.as_str())
}

fn decode_standard_string(token: &str) -> String {
    let contents = &token[1..token.len() - 1];
    let mut text = String::with_capacity(contents.len());
    let mut chars = contents.chars();
    while let Some(ch) = chars.next() {
        text.push(if ch == '\\' {
            chars.next().expect("lexer validated the string escape")
        } else {
            ch
        });
    }
    text
}
