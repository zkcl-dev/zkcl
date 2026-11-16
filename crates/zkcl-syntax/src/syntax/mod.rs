//! Lossless source syntax for editor tools. The parser groups forms of the
//! standard Common Lisp readtable and known zkcl syntax without executing reader macros.

use std::hash::BuildHasher;
use std::ops::Range;

use hashbrown::HashTable;
use rowan::{GreenNode, GreenNodeData, GreenToken, Language, NodeOrToken};
pub use rowan::{TextRange, TextSize};
use rustc_hash::FxBuildHasher;

pub mod lexer;

use lexer::{Lexeme, Lexer};

#[cfg(test)]
mod reparse_tests;

/// An initial reader syntax understood by the source tools.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SourceReadtable {
    #[default]
    Standard,
    RawStrings,
}

impl SourceReadtable {
    pub fn named(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("standard") {
            Some(Self::Standard)
        } else if name.eq_ignore_ascii_case("raw-strings") {
            Some(Self::RawStrings)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SyntaxKind {
    SourceFile,
    List,
    Vector,
    ReaderForm,
    Error,
    Whitespace,
    LineComment,
    BlockComment,
    Atom,
    String,
    Character,
    OpenParen,
    CloseParen,
    VectorStart,
    Quote,
    Backquote,
    Unquote,
    UnquoteSplicing,
    UnquoteDestructive,
    FunctionQuote,
    DispatchPrefix,
    ReaderConditional,
    UnknownDispatch,
    RawString,
    Dot,
}

impl SyntaxKind {
    /// Whitespace and comments preserved in the concrete syntax tree.
    pub const fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::LineComment | Self::BlockComment
        )
    }
}

// Both raw kinds represent a List in the public syntax tree. The distinction
// records whether a dot is one of the list's direct children.
const DOTTED_LIST: rowan::SyntaxKind = rowan::SyntaxKind(SyntaxKind::Dot as u16 + 1);

const KINDS: [SyntaxKind; DOTTED_LIST.0 as usize + 1] = {
    use SyntaxKind::*;
    [
        SourceFile,
        List,
        Vector,
        ReaderForm,
        Error,
        Whitespace,
        LineComment,
        BlockComment,
        Atom,
        String,
        Character,
        OpenParen,
        CloseParen,
        VectorStart,
        Quote,
        Backquote,
        Unquote,
        UnquoteSplicing,
        UnquoteDestructive,
        FunctionQuote,
        DispatchPrefix,
        ReaderConditional,
        UnknownDispatch,
        RawString,
        Dot,
        List,
    ]
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LispLanguage {}

impl Language for LispLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> Self::Kind {
        KINDS[usize::from(raw.0)]
    }

    fn kind_to_raw(kind: Self::Kind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind as u16)
    }
}

pub type SyntaxNode = rowan::SyntaxNode<LispLanguage>;

type SyntaxElement = rowan::SyntaxElement<LispLanguage>;
type GreenElement = NodeOrToken<GreenNode, GreenToken>;

pub(crate) fn list_has_dot(list: &SyntaxNode) -> bool {
    list.green().kind() == DOTTED_LIST
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SyntaxDiagnosticKind {
    UnexpectedCloseParen,
    MissingCloseParen,
    MissingReaderOperand,
    UnterminatedString,
    UnterminatedBlockComment,
    UnterminatedMultipleEscape,
    IncompleteEscape,
    IncompleteDispatch,
    InvalidDispatch,
    InvalidRawStringArgument,
    NestingLimitExceeded,
    SourceTooLarge,
    UnexpectedDot,
    InvalidDotToken,
    DottedListWithoutHead,
    MissingDottedTail,
    ExtraDottedTail,
    IncompleteCharacterLiteral,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
#[error("{} at byte {}", self.message(), u32::from(.range.start()))]
pub struct SyntaxDiagnostic {
    kind: SyntaxDiagnosticKind,
    range: TextRange,
    needs_more_input: bool,
}

impl SyntaxDiagnostic {
    pub fn kind(&self) -> SyntaxDiagnosticKind {
        self.kind
    }

    /// Half-open range in UTF-8 bytes in the original source.
    ///
    /// Diagnostics for missing delimiters and operands point to the opening
    /// token.
    pub fn range(&self) -> TextRange {
        self.range
    }

    pub fn message(&self) -> &'static str {
        self.kind.message()
    }

    /// Whether this diagnostic comes from a form that ran out of input and
    /// can be continued by appending text.
    pub fn needs_more_input(&self) -> bool {
        self.needs_more_input
    }
}

impl SyntaxDiagnosticKind {
    pub fn message(self) -> &'static str {
        use SyntaxDiagnosticKind::*;
        match self {
            UnexpectedCloseParen => "unexpected closing parenthesis",
            MissingCloseParen => "missing closing parenthesis",
            MissingReaderOperand => "missing reader operand",
            UnterminatedString => "unterminated string",
            UnterminatedBlockComment => "unterminated block comment",
            UnterminatedMultipleEscape => "unterminated escaped symbol",
            IncompleteEscape => "incomplete escape",
            IncompleteDispatch => "incomplete reader dispatch",
            InvalidDispatch => "invalid standard reader dispatch",
            InvalidRawStringArgument => "#> does not accept a numeric argument",
            NestingLimitExceeded => "syntax nesting limit exceeded",
            SourceTooLarge => "source exceeds Rowan's byte offset limit",
            UnexpectedDot => "dot must occur between elements and the tail of a list",
            InvalidDotToken => "a token of multiple dots requires an escape",
            DottedListWithoutHead => "a dotted list needs a preceding element",
            MissingDottedTail => "expected a form after the dot",
            ExtraDottedTail => "expected a closing parenthesis after the dotted tail",
            IncompleteCharacterLiteral => "expected a character or character name after #\\",
        }
    }
}

/// An immutable concrete syntax tree and any structural diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSource {
    green: GreenNode,
    diagnostics: Vec<SyntaxDiagnostic>,
    initial_readtable: SourceReadtable,
    readtable_changes: Vec<(TextRange, SourceReadtable)>,
}

#[cfg_attr(not(feature = "analysis"), allow(dead_code))]
impl ParsedSource {
    pub fn root(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }

    pub fn diagnostics(&self) -> &[SyntaxDiagnostic] {
        &self.diagnostics
    }

    /// Source forms in order, including incomplete forms and recovery nodes.
    pub fn forms(&self) -> impl Iterator<Item = crate::ast::AstForm> + use<> {
        self.root()
            .children_with_tokens()
            .filter_map(crate::ast::AstForm::cast)
    }

    /// The first diagnostic that requires an edit, or the first diagnostic
    /// that can be continued by appending input if there are only those.
    pub fn primary_diagnostic(&self) -> Option<&SyntaxDiagnostic> {
        self.diagnostics
            .iter()
            .find(|diagnostic| !diagnostic.needs_more_input())
            .or_else(|| self.diagnostics.first())
    }

    /// The readtable supplied when parsing started.
    pub fn initial_readtable(&self) -> SourceReadtable {
        self.initial_readtable
    }

    /// Recognized literal top-level readtable selections in source order.
    /// Each range covers the selecting form. The new table applies at its end.
    pub fn readtable_changes(&self) -> &[(TextRange, SourceReadtable)] {
        &self.readtable_changes
    }

    pub(crate) fn uses_readtable_selections(&self) -> bool {
        self.initial_readtable != SourceReadtable::Standard || !self.readtable_changes.is_empty()
    }

    pub(crate) fn has_readtable_selection(&self, range: TextRange) -> bool {
        self.readtable_changes
            .binary_search_by_key(&range.start(), |(span, _)| span.start())
            .is_ok_and(|index| self.readtable_changes[index].0 == range)
    }

    /// The readtable used to parse a position, accounting for earlier literal
    /// selections. The source parser does not execute reader macros.
    pub fn readtable_at(&self, at: TextSize) -> SourceReadtable {
        let index = self
            .readtable_changes
            .partition_point(|(span, _)| span.end() <= at);
        self.readtable_changes[..index]
            .last()
            .map_or(self.initial_readtable, |(_, table)| *table)
    }

    pub(crate) fn reparse(
        &self,
        edited: &zkcl_diagnostics::SourceDocument,
        replaced: TextRange,
        inserted: usize,
    ) -> Option<Self> {
        let old_len = self.green.text_len();
        let edited_len = edited.byte_len()?;
        if replaced.end() > old_len
            || usize::from(old_len - replaced.len()) + inserted != edited_len
            || TextSize::try_from(edited_len).is_err()
        {
            return None;
        }
        let root = self.root();
        let edit = ReparseEdit { replaced, inserted };
        if self.diagnostics.is_empty()
            && let Some(token) = root.covering_element(replaced).into_token()
            && matches!(token.kind(), SyntaxKind::Atom | SyntaxKind::LineComment)
        {
            let range = token.text_range();
            let start = usize::from(range.start());
            let end = usize::from(edit.shift(range.end()));
            // These two token kinds need at most one following scalar to
            // distinguish EOF from a continuation at their right boundary.
            let scan_end = if end == edited_len {
                end
            } else {
                (end + 1..=edited_len)
                    .take(4)
                    .find(|&at| edited.is_char_boundary(at))?
            };
            let window = edited.text_slice(start..scan_end)?;
            let text = &window[..end - start];
            // Keep the left lexer boundary and prove that the scanner stops
            // at the edited right boundary. For atoms, dots affect list shape
            // and changing a readtable selection affects all subsequent forms.
            if text.as_bytes().first() == token.text().as_bytes().first()
                && (token.kind() == SyntaxKind::LineComment
                    || (!text.bytes().all(|byte| byte == b'.')
                        && ![text, token.text()].into_iter().any(|text| {
                            text.eq_ignore_ascii_case("zk:in-readtable")
                                || text
                                    .strip_prefix(':')
                                    .and_then(SourceReadtable::named)
                                    .is_some()
                        })))
                && Lexer::new(window, self.readtable_at(range.start()))
                    .next()
                    .is_some_and(|lexeme| {
                        lexeme.kind == token.kind()
                            && lexeme.end == text.len()
                            && lexeme.diagnostic.is_none()
                    })
            {
                let green = token.replace_with(GreenToken::new(token.green().kind(), text));
                return Some(self.replace_clean(green, edit));
            }
        }
        if self.diagnostics.is_empty()
            && let Some(list) =
                top_level_child_at(&root, replaced.start()).and_then(NodeOrToken::into_node)
            && list.kind() == SyntaxKind::List
        {
            let span = list.text_range();
            // Both original delimiters survive. Give the unchanged suffix its
            // original lexer state by accepting only one complete list.
            if span.start() < replaced.start()
                && span.end() > replaced.end()
                && !self.has_readtable_selection(span)
                && let Some(text) = edited
                    .text_slice(usize::from(span.start())..usize::from(edit.shift(span.end())))
            {
                let parsed = parse_source_with_readtable(text, self.readtable_at(span.start()));
                if parsed.diagnostics.is_empty() && parsed.readtable_changes.is_empty() {
                    let mut children = parsed.green.children();
                    if let Some(child) = children.next().and_then(NodeOrToken::into_node)
                        && LispLanguage::kind_from_raw(child.kind()) == SyntaxKind::List
                        && children.next().is_none()
                    {
                        let green = self
                            .green
                            .replace_child(list.index(), child.to_owned().into());
                        return Some(self.replace_clean(green, edit));
                    }
                }
            }
        }
        Some(self.reparse_top_level(&root, edited.text()?, edit))
    }

    fn replace_clean(&self, green: GreenNode, edit: ReparseEdit) -> Self {
        let mut readtable_changes = self.readtable_changes.clone();
        for (range, _) in &mut readtable_changes {
            if range.start() >= edit.replaced.end() {
                *range = edit.shift_range(*range);
            } else if range.end() > edit.replaced.start() {
                *range = TextRange::new(range.start(), edit.shift(range.end()));
            }
        }
        Self {
            green,
            diagnostics: Vec::new(),
            initial_readtable: self.initial_readtable,
            readtable_changes,
        }
    }

    fn reparse_top_level(&self, root: &SyntaxNode, edited: &str, edit: ReparseEdit) -> Self {
        let ReparseEdit { replaced, inserted } = edit;
        let first = self.restart_child(root, edited, replaced.start());
        let (first_index, start) = first.as_ref().map_or((0, 0.into()), |child| {
            (child.index(), child.text_range().start())
        });
        let mut suffix = top_level_child_at(root, replaced.end()).and_then(|child| {
            if child.text_range().start() == replaced.end() {
                Some(child)
            } else {
                child.next_sibling_or_token()
            }
        });

        let start_byte = usize::from(start);
        let first_possible_end = suffix
            .as_ref()
            .map_or(self.green.text_len(), |child| child.text_range().start());
        let mut parser =
            Parser::with_source_len(usize::from(edit.shift(first_possible_end) - start));
        let mut lexer = Lexer::starting_at(edited, start_byte, self.readtable_at(start));
        let mut readtable_changes = Vec::new();
        let (suffix_index, old_end) = 'parse: loop {
            let at = lexer.offset();
            if at >= usize::from(replaced.start()) + inserted && parser.frames.is_empty() {
                while let Some(child) = &suffix {
                    let old_at = child.text_range().start();
                    let new_at = usize::from(edit.shift(old_at));
                    if at < new_at {
                        break;
                    }
                    if at == new_at
                        && lexer.readtable == self.readtable_at(old_at)
                        && !before_reader_recovery(child)
                    {
                        // The edit leaves this suffix byte-for-byte unchanged.
                        // Both sides are at a lexer boundary with an empty stack
                        // and the same active readtable, so recovery also agrees.
                        break 'parse (child.index(), old_at);
                    }
                    suffix = child.next_sibling_or_token();
                }
            }
            if !parser.step(edited, &mut lexer, &mut readtable_changes) {
                parser.recover_eof();
                break (self.green.children().len(), self.green.text_len());
            }
        };

        let children = first_index..suffix_index;
        self.reuse_window_children(&mut parser.children, children.clone(), start, edit);
        let green = if children.len() == 1 && parser.children.len() == 1 {
            self.green
                .replace_child(first_index, parser.children.pop().expect("one replacement"))
        } else {
            self.green.splice_children(children, parser.children)
        };

        let diagnostics = self
            .diagnostics
            .partition_point(|diagnostic| diagnostic.range.start() < start);
        let suffix_diagnostics = self
            .diagnostics
            .partition_point(|diagnostic| diagnostic.range.start() < old_end);
        let mut all_diagnostics = self.diagnostics[..diagnostics].to_vec();
        parser
            .diagnostics
            .sort_by_key(|diagnostic| diagnostic.range.start());
        all_diagnostics.extend(parser.diagnostics);
        all_diagnostics.extend(self.diagnostics[suffix_diagnostics..].iter().cloned().map(
            |mut diagnostic| {
                diagnostic.range = edit.shift_range(diagnostic.range);
                diagnostic
            },
        ));

        let selections = self
            .readtable_changes
            .partition_point(|(range, _)| range.start() < start);
        let suffix_selections = self
            .readtable_changes
            .partition_point(|(range, _)| range.start() < old_end);
        let mut all_selections = self.readtable_changes[..selections].to_vec();
        all_selections.extend(readtable_changes);
        all_selections.extend(
            self.readtable_changes[suffix_selections..]
                .iter()
                .map(|&(range, table)| (edit.shift_range(range), table)),
        );
        Self {
            green,
            diagnostics: all_diagnostics,
            initial_readtable: self.initial_readtable,
            readtable_changes: all_selections,
        }
    }

    fn restart_child(
        &self,
        root: &SyntaxNode,
        edited: &str,
        at: TextSize,
    ) -> Option<SyntaxElement> {
        // Appending may complete the final lexer token or a form recovered at
        // EOF. Start before that child even when the edited range is empty.
        let mut first = top_level_child_at(root, at).or_else(|| root.last_child_or_token())?;
        while let Some(previous) = first.prev_sibling_or_token() {
            if before_reader_recovery(&first) {
                first = previous;
                continue;
            }
            if first.text_range().start() == at
                && last_token_kind(&previous) != SyntaxKind::CloseParen
            {
                // The first changed byte could join the preceding lexer token.
                first = previous;
                continue;
            }
            if self.readtable_at(first.text_range().start()) == SourceReadtable::RawStrings {
                // A raw string can look through the next newline and indent
                // for #>. This can cross an otherwise unchanged trivia token.
                let raw = if previous.kind() == SyntaxKind::Whitespace {
                    previous.prev_sibling_or_token()
                } else {
                    Some(previous)
                };
                if let Some(raw) = raw
                    && let Some(token) = last_token(&raw)
                    && token.kind() == SyntaxKind::RawString
                    && Lexer::starting_at(
                        edited,
                        usize::from(token.text_range().start()),
                        SourceReadtable::RawStrings,
                    )
                    .next()
                    .is_some_and(|lexeme| usize::from(token.text_range().end()) != lexeme.end)
                {
                    first = raw;
                    continue;
                }
            }
            break;
        }
        Some(first)
    }

    fn reuse_window_children(
        &self,
        children: &mut [GreenElement],
        old_children: Range<usize>,
        start: TextSize,
        edit: ReparseEdit,
    ) {
        // Reuse occurrences that were included in the window only to check
        // the boundary. Equal forms at other positions are distinct occurrences.
        let mut unchanged = self
            .green
            .children()
            .skip(old_children.start)
            .take(old_children.len())
            .scan(start, |offset, child| {
                let range = TextRange::at(*offset, child.text_len());
                *offset = range.end();
                Some((range, child))
            })
            .filter_map(|(range, child)| {
                if range.end() <= edit.replaced.start() {
                    Some((range.start(), child))
                } else if range.start() >= edit.replaced.end() {
                    Some((edit.shift(range.start()), child))
                } else {
                    None
                }
            })
            .peekable();
        let mut offset = start;
        for child in children {
            while unchanged.peek().is_some_and(|(at, _)| *at < offset) {
                unchanged.next();
            }
            if let Some(&(at, old)) = unchanged.peek()
                && at == offset
            {
                let same = match (old, &*child) {
                    (NodeOrToken::Node(old), NodeOrToken::Node(new)) => old == &**new,
                    (NodeOrToken::Token(old), NodeOrToken::Token(new)) => old == &**new,
                    _ => false,
                };
                if same {
                    *child = old.to_owned();
                }
            }
            offset += child.text_len();
        }
    }
}

#[derive(Clone, Copy)]
struct ReparseEdit {
    replaced: TextRange,
    inserted: usize,
}

impl ReparseEdit {
    fn shift(self, at: TextSize) -> TextSize {
        TextSize::try_from(usize::from(at - self.replaced.len()) + self.inserted)
            .expect("validated edit size")
    }

    fn shift_range(self, range: TextRange) -> TextRange {
        TextRange::new(self.shift(range.start()), self.shift(range.end()))
    }
}

fn top_level_child_at(root: &SyntaxNode, at: TextSize) -> Option<SyntaxElement> {
    (at < root.text_range().end())
        .then(|| root.child_or_token_at_range(TextRange::at(at, 1.into())))
        .flatten()
}

fn last_token(element: &SyntaxElement) -> Option<rowan::SyntaxToken<LispLanguage>> {
    match element {
        NodeOrToken::Node(node) => node.last_token(),
        NodeOrToken::Token(token) => Some(token.clone()),
    }
}

fn last_token_kind(element: &SyntaxElement) -> SyntaxKind {
    last_token(element)
        .expect("top-level child contains at least one token")
        .kind()
}

fn before_reader_recovery(child: &SyntaxElement) -> bool {
    // An unexpected ')' can emit both an incomplete reader form and its
    // own error node. There was no empty parser stack between those children.
    child.kind() == SyntaxKind::Error
        && child
            .prev_sibling_or_token()
            .is_some_and(|previous| previous.kind() == SyntaxKind::ReaderForm)
}

/// Parse a source buffer into a lossless concrete syntax tree.
///
/// Incomplete forms remain visible in the tree. Excessively deep forms retain
/// all of their tokens, with nested nodes flattened past the nesting limit.
/// Dotted-list diagnostics are deferred when a reader macro may change which
/// source forms count as elements, or a feature conditional may suppress them.
/// Buffers beyond Rowan's 32-bit byte offset limit produce an empty tree and a
/// size diagnostic.
pub fn parse_source(text: &str) -> ParsedSource {
    parse_source_with_readtable(text, SourceReadtable::Standard)
}

/// Parse source with an initial readtable supplied by the caller. Literal
/// top-level `(zk:in-readtable ...)` selections apply after their own form.
pub fn parse_source_with_readtable(text: &str, initial_readtable: SourceReadtable) -> ParsedSource {
    let mut parser = Parser::with_source_len(text.len());
    let mut readtable_changes = Vec::new();
    if TextSize::try_from(text.len()).is_err() {
        parser.report(
            SyntaxDiagnosticKind::SourceTooLarge,
            TextRange::empty(0.into()),
        );
    } else {
        let mut lexer = Lexer::new(text, initial_readtable);
        while parser.step(text, &mut lexer, &mut readtable_changes) {}
    }
    parser.finish(initial_readtable, readtable_changes)
}

fn readtable_selection(node: &GreenNodeData) -> Option<SourceReadtable> {
    if node.kind() != LispLanguage::kind_to_raw(SyntaxKind::List) {
        return None;
    }
    let mut parts = node.children().skip(1).filter(|part| {
        !part
            .as_token()
            .is_some_and(|token| LispLanguage::kind_from_raw(token.kind()).is_trivia())
    });
    let operator = parts.next()?.into_token()?;
    if operator.kind() != LispLanguage::kind_to_raw(SyntaxKind::Atom)
        || !operator.text().eq_ignore_ascii_case("zk:in-readtable")
    {
        return None;
    }
    let keyword = parts.next()?.into_token()?;
    if keyword.kind() != LispLanguage::kind_to_raw(SyntaxKind::Atom)
        || parts.next()?.kind() != LispLanguage::kind_to_raw(SyntaxKind::CloseParen)
    {
        return None;
    }
    SourceReadtable::named(keyword.text().strip_prefix(':')?)
}

fn byte_range(start: usize, end: usize) -> TextRange {
    // parse_source checks the complete buffer against TextSize's limit.
    TextRange::new(TextSize::from(start as u32), TextSize::from(end as u32))
}

struct Token<'a> {
    kind: SyntaxKind,
    text: &'a str,
    range: TextRange,
}

// Green nodes drop their children recursively even though parsing is iterative.
const MAX_NESTING: usize = 256;

enum FrameKind {
    Group(GroupShape),
    ReaderForm {
        remaining: u8,
        conditional: bool,
        uncertain: bool,
    },
}

enum GroupShape {
    Empty,
    Elements,
    NeedsTail(TextRange),
    HasTail,
    Malformed(SyntaxDiagnostic),
    Ambiguous,
}

impl GroupShape {
    fn dot(&mut self, range: TextRange) {
        use SyntaxDiagnosticKind::{DottedListWithoutHead, ExtraDottedTail, UnexpectedDot};

        let kind = match self {
            Self::Empty => DottedListWithoutHead,
            Self::Elements => {
                *self = Self::NeedsTail(range);
                return;
            }
            Self::NeedsTail(_) => UnexpectedDot,
            Self::HasTail => ExtraDottedTail,
            Self::Malformed(_) | Self::Ambiguous => return,
        };
        *self = Self::Malformed(SyntaxDiagnostic {
            kind,
            range,
            needs_more_input: false,
        });
    }

    fn form(&mut self, range: TextRange, uncertain: bool) {
        // A feature conditional or an unrecognized macro can return no value.
        // An enclosing reader prefix may then read the next source form too.
        if uncertain {
            *self = Self::Ambiguous;
            return;
        }
        match self {
            Self::Empty => *self = Self::Elements,
            Self::NeedsTail(_) => *self = Self::HasTail,
            Self::HasTail => {
                *self = Self::Malformed(SyntaxDiagnostic {
                    kind: SyntaxDiagnosticKind::ExtraDottedTail,
                    range,
                    needs_more_input: false,
                });
            }
            Self::Elements | Self::Malformed(_) | Self::Ambiguous => {}
        }
    }

    fn diagnostic(self) -> Option<SyntaxDiagnostic> {
        match self {
            Self::Malformed(diagnostic) => Some(diagnostic),
            Self::NeedsTail(range) => Some(SyntaxDiagnostic {
                kind: SyntaxDiagnosticKind::MissingDottedTail,
                range,
                needs_more_input: false,
            }),
            _ => None,
        }
    }
}

struct Frame {
    kind: FrameKind,
    opener: TextRange,
    node: Option<OpenNode>,
    may_be_suppressed: bool,
}

struct OpenNode {
    kind: rowan::SyntaxKind,
    first_child: usize,
}

struct Parser {
    children: Vec<NodeOrToken<GreenNode, GreenToken>>,
    tokens: HashTable<GreenToken>,
    single_byte_tokens: [Option<GreenToken>; 128],
    indentation_tokens: [Option<GreenToken>; 32],
    frames: Vec<Frame>,
    diagnostics: Vec<SyntaxDiagnostic>,
}

impl Parser {
    fn with_source_len(source_len: usize) -> Self {
        Self {
            children: Vec::with_capacity((source_len / 64).clamp(16, 256)),
            tokens: HashTable::with_capacity((source_len / 48).clamp(32, 2048)),
            single_byte_tokens: [const { None }; 128],
            indentation_tokens: [const { None }; 32],
            frames: Vec::with_capacity(16),
            diagnostics: Vec::new(),
        }
    }

    fn step(
        &mut self,
        text: &str,
        lexer: &mut Lexer<'_>,
        readtable_changes: &mut Vec<(TextRange, SourceReadtable)>,
    ) -> bool {
        let start = lexer.offset();
        let Some(Lexeme {
            kind,
            end,
            diagnostic,
        }) = lexer.next()
        else {
            return false;
        };
        if let Some(diagnostic) = diagnostic
            && (diagnostic.kind != SyntaxDiagnosticKind::InvalidRawStringArgument
                || !self.may_be_suppressed())
        {
            let complete = matches!(
                diagnostic.kind,
                SyntaxDiagnosticKind::InvalidDispatch
                    | SyntaxDiagnosticKind::InvalidRawStringArgument
            );
            self.report_with_input(
                diagnostic.kind,
                byte_range(diagnostic.start, end),
                end == text.len() && !complete,
            );
        }
        self.token(Token {
            kind,
            text: &text[start..end],
            range: byte_range(start, end),
        });
        if kind == SyntaxKind::CloseParen
            && self.frames.is_empty()
            && let Some(NodeOrToken::Node(node)) = self.children.last()
            && let Some(readtable) = readtable_selection(node)
        {
            readtable_changes.push((
                byte_range(end - usize::from(node.text_len()), end),
                readtable,
            ));
            lexer.readtable = readtable;
        }
        true
    }

    fn report(&mut self, kind: SyntaxDiagnosticKind, range: TextRange) {
        self.report_with_input(kind, range, false);
    }

    fn report_with_input(
        &mut self,
        kind: SyntaxDiagnosticKind,
        range: TextRange,
        needs_more_input: bool,
    ) {
        self.diagnostics.push(SyntaxDiagnostic {
            kind,
            range,
            needs_more_input,
        });
    }

    fn may_be_suppressed(&self) -> bool {
        self.frames
            .last()
            .is_some_and(|frame| frame.may_be_suppressed)
    }

    fn token(&mut self, token: Token<'_>) {
        use SyntaxKind::*;

        match token.kind {
            Whitespace | LineComment | BlockComment => self.emit(token),
            OpenParen => self.open(List, FrameKind::Group(GroupShape::Empty), token),
            VectorStart => self.open(Vector, FrameKind::Group(GroupShape::Empty), token),
            CloseParen => self.close(token),
            Quote | Backquote | Unquote | UnquoteSplicing | UnquoteDestructive | FunctionQuote
            | DispatchPrefix | ReaderConditional => self.open_reader_form(token),
            Atom if token.text == "." => self.dot(token),
            _ => {
                let range = token.range;
                let uncertain = token.kind == UnknownDispatch;
                if token.kind == Atom
                    && token.text.starts_with('.')
                    && token.text.bytes().all(|byte| byte == b'.')
                    && !self.may_be_suppressed()
                {
                    self.report(SyntaxDiagnosticKind::InvalidDotToken, range);
                }
                self.emit(token);
                self.complete_form(range, uncertain);
            }
        }
    }

    fn dot(&mut self, mut token: Token<'_>) {
        let range = token.range;
        token.kind = SyntaxKind::Dot;
        self.emit(token);
        // Beyond the nesting limit, tokens are direct children of the last
        // node represented in the tree, even while more frames are open.
        let represented_frames = self.frames.len().min(MAX_NESTING);
        if let Some(frame) = self.frames[..represented_frames].last_mut()
            && let Some(node) = &mut frame.node
            && node.kind == LispLanguage::kind_to_raw(SyntaxKind::List)
        {
            node.kind = DOTTED_LIST;
        }
        if let Some(Frame {
            kind: FrameKind::Group(shape),
            ..
        }) = self.frames.last_mut()
        {
            shape.dot(range);
        } else {
            if !self.may_be_suppressed() {
                self.report(SyntaxDiagnosticKind::UnexpectedDot, range);
            }
            // Let the malformed token fill a reader operand for recovery.
            self.complete_form(range, false);
        }
    }

    fn open_reader_form(&mut self, token: Token<'_>) {
        let conditional = token.kind == SyntaxKind::ReaderConditional;
        self.open(
            SyntaxKind::ReaderForm,
            FrameKind::ReaderForm {
                remaining: if conditional { 2 } else { 1 },
                conditional,
                uncertain: false,
            },
            token,
        );
    }

    fn green_token(&mut self, token: Token<'_>) -> GreenToken {
        let kind = LispLanguage::kind_to_raw(token.kind);
        if let &[byte] = token.text.as_bytes() {
            // Each ASCII byte that forms a token has a single syntax kind.
            // Share these tokens without hashing punctuation and spaces.
            return self.single_byte_tokens[usize::from(byte)]
                .get_or_insert_with(|| GreenToken::new(kind, token.text))
                .clone();
        }
        if token.kind == SyntaxKind::Whitespace
            && let Some(slot) = self.indentation_tokens.get_mut(token.text.len())
        {
            // Short indentation runs repeat often. Their length selects a
            // candidate, while the text check distinguishes spaces and tabs.
            let cached = slot.get_or_insert_with(|| GreenToken::new(kind, token.text));
            if cached.text() == token.text {
                return cached.clone();
            }
        }
        let hash = |kind, text: &str| FxBuildHasher.hash_one((kind, text));
        self.tokens
            .entry(
                hash(kind, token.text),
                |cached| cached.kind() == kind && cached.text() == token.text,
                |cached| hash(cached.kind(), cached.text()),
            )
            .or_insert_with(|| GreenToken::new(kind, token.text))
            .get()
            .clone()
    }

    fn emit(&mut self, token: Token<'_>) {
        let green = self.green_token(token);
        self.children.push(NodeOrToken::Token(green));
    }

    fn open(&mut self, node: SyntaxKind, kind: FrameKind, token: Token<'_>) {
        let depth = self.frames.len();
        let node = (depth < MAX_NESTING).then_some(OpenNode {
            kind: LispLanguage::kind_to_raw(node),
            first_child: self.children.len(),
        });
        if depth == MAX_NESTING {
            self.report(SyntaxDiagnosticKind::NestingLimitExceeded, token.range);
        }
        self.frames.push(Frame {
            may_be_suppressed: self.may_be_suppressed()
                || matches!(
                    kind,
                    FrameKind::ReaderForm {
                        conditional: true,
                        ..
                    }
                ),
            kind,
            opener: token.range,
            node,
        });
        self.emit(token);
    }

    fn close(&mut self, token: Token<'_>) {
        while self
            .frames
            .last()
            .is_some_and(|frame| matches!(frame.kind, FrameKind::ReaderForm { .. }))
        {
            self.finish_incomplete(false);
        }
        if let Some(frame) = self.frames.pop() {
            let range = frame.opener.cover(token.range);
            self.emit(token);
            self.finish_node(frame.node);
            if !frame.may_be_suppressed
                && let FrameKind::Group(shape) = frame.kind
                && let Some(diagnostic) = shape.diagnostic()
            {
                self.diagnostics.push(diagnostic);
            }
            self.complete_form(range, false);
        } else {
            self.report(SyntaxDiagnosticKind::UnexpectedCloseParen, token.range);
            let token = self.green_token(token);
            let error = GreenNode::new(
                LispLanguage::kind_to_raw(SyntaxKind::Error),
                [NodeOrToken::Token(token)],
            );
            self.children.push(NodeOrToken::Node(error));
        }
    }

    fn finish_node(&mut self, node: Option<OpenNode>) {
        if let Some(node) = node {
            let green = GreenNode::new(node.kind, self.children.drain(node.first_child..));
            self.children.push(NodeOrToken::Node(green));
        }
    }

    fn complete_form(&mut self, mut range: TextRange, mut uncertain: bool) {
        while let Some(frame) = self.frames.last_mut() {
            match &mut frame.kind {
                FrameKind::Group(shape) => {
                    shape.form(range, uncertain);
                    return;
                }
                FrameKind::ReaderForm {
                    remaining,
                    conditional,
                    uncertain: operand_uncertainty,
                } => {
                    *operand_uncertainty |= uncertain;
                    *remaining -= 1;
                    if *remaining != 0 {
                        return;
                    }
                    uncertain = *conditional || *operand_uncertainty;
                }
            }
            let frame = self
                .frames
                .pop()
                .expect("the completed reader form is still open");
            range = frame.opener.cover(range);
            self.finish_node(frame.node);
        }
    }

    fn finish_incomplete(&mut self, needs_more_input: bool) {
        let frame = self
            .frames
            .pop()
            .expect("there is an open frame to recover");
        let kind = match frame.kind {
            FrameKind::Group(_) => SyntaxDiagnosticKind::MissingCloseParen,
            FrameKind::ReaderForm { .. } => SyntaxDiagnosticKind::MissingReaderOperand,
        };
        self.report_with_input(kind, frame.opener, needs_more_input);
        self.finish_node(frame.node);
        self.complete_form(frame.opener, true);
    }

    fn recover_eof(&mut self) {
        while !self.frames.is_empty() {
            self.finish_incomplete(true);
        }
    }

    fn finish(
        mut self,
        initial_readtable: SourceReadtable,
        readtable_changes: Vec<(TextRange, SourceReadtable)>,
    ) -> ParsedSource {
        self.recover_eof();
        // The emitted tokens are owned by the tree. Release the interner
        // before allocating the root, whose child table can be large.
        drop(self.tokens);
        self.diagnostics
            .sort_by_key(|diagnostic| diagnostic.range.start());
        ParsedSource {
            green: GreenNode::new(
                LispLanguage::kind_to_raw(SyntaxKind::SourceFile),
                self.children,
            ),
            diagnostics: self.diagnostics,
            initial_readtable,
            readtable_changes,
        }
    }
}

#[cfg(test)]
#[test]
fn equal_length_whitespace_tokens_keep_their_spelling() {
    let text = "(first  second \tthird\t fourth\r\nfifth\n sixth)";
    let parsed = parse_source(text);
    assert!(parsed.diagnostics().is_empty());
    assert_eq!(parsed.root().text().to_string(), text);
}

#[cfg(test)]
#[test]
fn dotted_list_kind_matches_direct_tokens_even_at_the_nesting_limit() {
    let nested = format!(
        "{}a . b{}",
        "(".repeat(MAX_NESTING + 1),
        ")".repeat(MAX_NESTING + 1)
    );
    let parsed = parse_source(&format!("(list '(a . b) '(c . ) (d) {nested})"));
    for list in parsed
        .root()
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::List)
    {
        let has_dot = list
            .children_with_tokens()
            .any(|child| child.kind() == SyntaxKind::Dot);
        assert_eq!(
            list_has_dot(&list),
            has_dot,
            "list at {:?}",
            list.text_range()
        );
    }
}
