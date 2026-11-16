use super::{
    Edit, EditError, Group, Selection, StructuralOperation, checked_text, edit, ensure_editable,
    group_at_caret, insert, preserve_boundaries, ranges_intersect, remove, validate_range,
};
use crate::ast::{AstReaderForm, Element, Token, tokens_at_offset};
use crate::format::TextEdit;
use crate::source_workspace::SourceSnapshot;
use crate::syntax::lexer::lex;
use crate::syntax::{SyntaxDiagnosticKind, SyntaxKind, SyntaxNode, TextRange, TextSize};
use rowan::Direction;
use std::sync::Arc;

pub(super) fn delete(
    source: Arc<SourceSnapshot>,
    root: &SyntaxNode,
    selection: TextRange,
    direction: Direction,
) -> Result<Edit, EditError> {
    if !selection.is_empty() {
        return erase(source, root, selection);
    }
    let at = selection.start();
    let text = source.text();
    let byte = usize::from(at);
    let range = match direction {
        Direction::Next => text[byte..]
            .chars()
            .next()
            .map(|ch| byte..byte + ch.len_utf8()),
        Direction::Prev => text[..byte]
            .char_indices()
            .next_back()
            .map(|(start, _)| start..byte),
    }
    .ok_or(EditError::NoSibling)?;
    let adjacent = tokens_at_offset(root, at);
    let token = match direction {
        Direction::Next => adjacent.right_biased(),
        Direction::Prev => adjacent.left_biased(),
    }
    .ok_or(EditError::NoSibling)?;
    let span = token.text_range();
    let mut cut = bytes(range.start, range.end);
    if !source.editable_reader_syntax_at(cut.start()) {
        return Err(EditError::UnknownReaderBoundary);
    }
    // Treat CRLF as one line break, including in line comments (whose token
    // contains the CR but not the LF).
    if let Some(newline) = crlf_at(text, range.start).or_else(|| crlf_at(text, range.end)) {
        cut = bytes(newline - 1, newline + 1);
        return erase(source, root, cut);
    }

    use SyntaxKind::*;
    let pair = match token.kind() {
        OpenParen | VectorStart | CloseParen => {
            let group = token.parent().and_then(Group::cast);
            let Some(group) = group else {
                // An unmatched close is already a syntax error; deleting it
                // does not remove another form's delimiter.
                return if token.kind() == CloseParen {
                    erase(source, root, cut)
                } else {
                    Err(EditError::UnsafeTokenBoundary)
                };
            };
            if let Some(closer) = &group.closer {
                Some((group.opener.text_range(), closer.text_range()))
            } else {
                return Err(EditError::IncompleteGroup);
            }
        }
        String => string_pair(&source, &token),
        _ => None,
    };
    if let Some((open, close)) = pair {
        if ranges_intersect(cut, open) || ranges_intersect(cut, close) {
            if at == open.end() && at == close.start() {
                return erase(source, root, TextRange::new(open.start(), close.end()));
            }
            return match direction {
                Direction::Next if at == open.start() => Ok(move_to(source, open.end())),
                Direction::Prev if at == close.end() => Ok(move_to(source, close.start())),
                _ => Err(EditError::UnsafeTokenBoundary),
            };
        }
    }

    match token.kind() {
        UnknownDispatch | Error => return Err(EditError::UnknownReaderBoundary),
        RawString | Dot => return Err(EditError::UnsafeTokenBoundary),
        Character => {
            if at != span.start() && at != span.end() {
                return Err(EditError::UnsafeTokenBoundary);
            }
            cut = span;
        }
        String | Atom | LineComment | BlockComment if !dispatch_atom(&token) => {
            for (unit, kind) in protected_units(&source, &token) {
                if ranges_intersect(cut, unit) {
                    if kind == ProtectedUnitKind::Delimiter {
                        return Err(EditError::UnsafeTokenBoundary);
                    }
                    cut = unit;
                    break;
                }
            }
        }
        Atom => return Err(EditError::UnsafeTokenBoundary),
        _ => {}
    }
    erase(source, root, cut)
}

fn erase(
    source: Arc<SourceSnapshot>,
    root: &SyntaxNode,
    range: TextRange,
) -> Result<Edit, EditError> {
    let mut changes = vec![remove(range)];
    if crlf_at(source.text(), usize::from(range.start())).is_some()
        || crlf_at(source.text(), usize::from(range.end())).is_some()
    {
        return Err(EditError::UnsafeTokenBoundary);
    }
    let mut partial = None;
    let mut needs_separator = true;
    let start_token = tokens_at_offset(root, range.start()).right_biased();
    let end_token = tokens_at_offset(root, range.end()).left_biased();
    // A selection can cut through whitespace or edit one token's payload.
    // Cuts through two different text tokens could fuse two distinct forms.
    for token in start_token.iter().chain(end_token.iter()) {
        let span = token.text_range();
        if range.contains_range(span) || !ranges_intersect(range, span) {
            continue;
        }
        needs_separator = false;
        if token.kind() != SyntaxKind::Whitespace {
            // The CR at the end of a line comment belongs to a removed CRLF.
            let removes_crlf =
                token.kind() == SyntaxKind::LineComment && source.text_at(range) == Some("\r\n");
            if !span.contains_range(range) && !removes_crlf {
                return Err(EditError::UnsafeTokenBoundary);
            }
            // Removing an entire CRLF after a comment is only legal if it
            // does not cause any later token to become part of the comment.
            if !removes_crlf {
                partial = Some(token);
            }
        }
    }

    let covered = root.covering_element(range);
    // Check the intersected subtree and its ancestors for partly removed
    // reader operands and groups. These need not cause a parser diagnostic:
    // e.g. deleting A in 'A B would quietly quote B instead.
    let nodes = covered
        .as_node()
        .into_iter()
        .flat_map(SyntaxNode::descendants);
    for node in nodes.chain(
        covered
            .parent()
            .into_iter()
            .flat_map(|node| node.ancestors()),
    ) {
        let span = node.text_range();
        if !ranges_intersect(range, span) || range.contains_range(span) {
            continue;
        }
        if let Some(reader) = AstReaderForm::cast(node.clone())
            && reader
                .operands()
                .any(|operand| range.contains_range(operand.text_range()))
        {
            return Err(EditError::UnsafeTokenBoundary);
        }
        if let Some(group) = Group::cast(node) {
            if ranges_intersect(range, group.opener.text_range())
                || group
                    .closer
                    .as_ref()
                    .is_some_and(|close| ranges_intersect(range, close.text_range()))
            {
                return Err(EditError::UnsafeTokenBoundary);
            }
            if group.counted_vector()
                && group
                    .forms
                    .iter()
                    .any(|form| range.contains_range(form.text_range()))
            {
                return Err(EditError::UnsupportedSelection);
            }
        }
    }

    for token in covered
        .as_node()
        .into_iter()
        .flat_map(SyntaxNode::descendants_with_tokens)
        .filter_map(Element::into_token)
        .chain(covered.as_token().cloned())
        .filter(|token| ranges_intersect(range, token.text_range()))
    {
        use SyntaxKind::*;
        match token.kind() {
            UnknownDispatch => return Err(EditError::UnknownReaderBoundary),
            Quote | Backquote | Unquote | UnquoteSplicing | UnquoteDestructive | FunctionQuote
            | DispatchPrefix | ReaderConditional | Dot => {
                if token
                    .parent()
                    .is_none_or(|node| !range.contains_range(node.text_range()))
                {
                    return Err(EditError::UnsafeTokenBoundary);
                }
            }
            _ => {}
        }
    }
    if let Some(token) = partial {
        use SyntaxKind::*;
        match token.kind() {
            String | Atom | LineComment | BlockComment if !dispatch_atom(token) => {
                for (unit, kind) in protected_units(&source, token) {
                    if ranges_intersect(range, unit)
                        && (kind == ProtectedUnitKind::Delimiter || !range.contains_range(unit))
                    {
                        return Err(EditError::UnsafeTokenBoundary);
                    }
                }
            }
            _ => return Err(EditError::UnsafeTokenBoundary),
        }
    }
    if needs_separator {
        preserve_boundaries(&source, root, &mut changes);
        if source.text_at(range) == Some(changes[0].replacement.as_str()) {
            return Err(EditError::UnsafeTokenBoundary);
        }
    }
    input_edit(source, root, changes, range.start(), partial)
}

/// Insert zkcl's `(`, `)` or `"` as an explicit editing command.
///
/// `(` wraps a selection or inserts `()`. `"` inserts `""`, escapes a
/// quote in a string, or steps past its closing quote. `)` exits a closed
/// group or closes an unfinished group at EOF. In comments these characters
/// are literal. Other selections and unsupported reader contexts are refused.
pub fn insert_delimiter(
    source: Arc<SourceSnapshot>,
    selection: Selection,
    delimiter: char,
) -> Result<Edit, EditError> {
    let range = selection.range();
    validate_range(source.text(), range)?;
    if !range.is_empty() {
        return if delimiter == '(' {
            edit(source, selection, StructuralOperation::Wrap)
        } else {
            Err(EditError::UnsupportedSelection)
        };
    }
    let at = range.start();
    if !source.editable_reader_syntax_at(at) {
        return Err(EditError::UnknownReaderBoundary);
    }
    if crlf_at(source.text(), usize::from(at)).is_some() {
        return Err(EditError::UnsafeTokenBoundary);
    }
    if !matches!(delimiter, '(' | ')' | '"') {
        return Err(EditError::UnsupportedSelection);
    }
    let root = source.parsed().root();
    let left = tokens_at_offset(&root, at)
        .left_biased()
        .filter(|token| token.text_range().start() < at);
    if let Some(token) = left.as_ref() {
        use SyntaxKind::*;
        let span = token.text_range();
        let prefix = &token.text()[..usize::from(at - span.start())];
        let spelling = delimiter.to_string();
        let diagnostic = lex(prefix, source.parsed().readtable_at(span.start()))
            .last()
            .and_then(|token| token.diagnostic)
            .map(|diag| diag.kind);
        if at < span.end()
            && (matches!(token.kind(), Character)
                || matches!(token.kind(), Atom | String) && pending_escape(prefix))
        {
            return Err(EditError::UnsafeTokenBoundary);
        }
        let literal = match (token.kind(), diagnostic) {
            (String, Some(SyntaxDiagnosticKind::UnterminatedString)) => {
                if delimiter == '"'
                    && string_pair(&source, token).is_some_and(|(_, close)| close.start() == at)
                {
                    return Ok(move_to(source, at + TextSize::from(1)));
                }
                if delimiter == '"' && at != span.end() && !pending_escape(prefix) {
                    Some("\\\"")
                } else {
                    Some(spelling.as_str())
                }
            }
            (LineComment, _)
            | (BlockComment, Some(SyntaxDiagnosticKind::UnterminatedBlockComment))
            | (
                Atom,
                Some(
                    SyntaxDiagnosticKind::UnterminatedMultipleEscape
                    | SyntaxDiagnosticKind::IncompleteEscape,
                ),
            )
            | (Character, Some(SyntaxDiagnosticKind::IncompleteCharacterLiteral)) => {
                Some(spelling.as_str())
            }
            (RawString, _) => return Err(EditError::UnsupportedSelection),
            _ => None,
        };
        if let Some(literal) = literal {
            return input_edit(
                source,
                &root,
                vec![insert(at, literal)],
                at + TextSize::try_from(literal.len()).unwrap(),
                Some(token),
            );
        }
    }

    if delimiter != ')' && before_reader_operand(&root, at) {
        return Err(EditError::UnsafeTokenBoundary);
    }

    let spelling = match delimiter {
        '(' => "()",
        '"' => "\"\"",
        ')' => {
            let group = group_at_caret(&root, at).ok_or(EditError::NoEnclosingGroup)?;
            if let Some(close) = group.closer {
                let span = close.text_range();
                if !source.editable_reader_syntax_at(span.start()) {
                    return Err(EditError::UnknownReaderBoundary);
                }
                return Ok(move_to(source, span.end()));
            }
            if at != root.text_range().end() {
                return Err(EditError::IncompleteGroup);
            }
            ")"
        }
        _ => unreachable!(),
    };
    input_edit(
        source,
        &root,
        vec![insert(at, spelling)],
        at + TextSize::from(1),
        None,
    )
}

fn before_reader_operand(root: &SyntaxNode, at: TextSize) -> bool {
    tokens_at_offset(root, at)
        .right_biased()
        .is_some_and(|token| {
            token
                .parent_ancestors()
                .filter_map(AstReaderForm::cast)
                .any(|reader| {
                    reader.syntax().text_range().start() < at
                        && !reader.operands().any(|form| {
                            let span = form.text_range();
                            span.start() < at && at < span.end()
                        })
                        && reader
                            .operands()
                            .any(|form| at <= form.text_range().start())
                })
        })
}

fn move_to(source: Arc<SourceSnapshot>, at: TextSize) -> Edit {
    Edit {
        snapshot: source,
        edits: vec![],
        selection: Selection::new(at, at),
    }
}

fn input_edit(
    source: Arc<SourceSnapshot>,
    root: &SyntaxNode,
    changes: Vec<TextEdit>,
    caret: TextSize,
    token: Option<&Token>,
) -> Result<Edit, EditError> {
    ensure_editable(&source, &changes)?;
    let (result, _) = checked_text(&source, root, &changes, None, token)?;
    validate_range(&result, TextRange::empty(caret))?;
    Ok(Edit {
        snapshot: source,
        edits: changes,
        selection: Selection::new(caret, caret),
    })
}

fn bytes(start: usize, end: usize) -> TextRange {
    TextRange::new(
        TextSize::try_from(start).unwrap(),
        TextSize::try_from(end).unwrap(),
    )
}

fn crlf_at(text: &str, byte: usize) -> Option<usize> {
    (byte > 0 && text.as_bytes().get(byte - 1..byte + 1) == Some(b"\r\n")).then_some(byte)
}

fn dispatch_atom(token: &Token) -> bool {
    token.kind() == SyntaxKind::Atom && token.text().starts_with('#')
}

fn string_pair(source: &SourceSnapshot, token: &Token) -> Option<(TextRange, TextRange)> {
    let span = token.text_range();
    let one = TextSize::from(1);
    if token.text().len() < 2
        || lex(token.text(), source.parsed().readtable_at(span.start()))
            .next()?
            .diagnostic
            .is_some()
    {
        return None;
    }
    Some((
        TextRange::at(span.start(), one),
        TextRange::at(span.end() - one, one),
    ))
}

fn pending_escape(text: &str) -> bool {
    !text
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count()
        .is_multiple_of(2)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProtectedUnitKind {
    Escape,
    Delimiter,
}

// These are editing units within a lexer token. Escape pairs can be deleted
// together; quoting and comment delimiters must remain unless the whole token
// is selected. The lexer continues to own token boundaries.
fn protected_units(source: &SourceSnapshot, token: &Token) -> Vec<(TextRange, ProtectedUnitKind)> {
    use ProtectedUnitKind::{Delimiter, Escape};
    let spelling = token.text();
    let start = usize::from(token.text_range().start());
    let mut units = vec![];
    let mut add = |first, end, kind| units.push((bytes(start + first, start + end), kind));
    use SyntaxKind::*;
    match token.kind() {
        LineComment => add(0, 1, Delimiter),
        BlockComment => {
            let head = spelling.find('|').unwrap() + 1;
            add(0, head, Delimiter);
            let mut offset = head;
            while offset + 1 < spelling.len() {
                if matches!(&spelling.as_bytes()[offset..offset + 2], b"#|" | b"|#") {
                    add(offset, offset + 2, Delimiter);
                    offset += 2;
                } else {
                    offset += 1;
                }
            }
        }
        String | Atom => {
            let mut chars = spelling.char_indices();
            if token.kind() == String {
                chars.next();
                add(0, 1, Delimiter);
                if string_pair(source, token).is_some() {
                    add(spelling.len() - 1, spelling.len(), Delimiter);
                }
            }
            while let Some((offset, ch)) = chars.next() {
                match ch {
                    '\\' => {
                        let end = chars
                            .next()
                            .map_or(offset + 1, |(next, ch)| next + ch.len_utf8());
                        add(offset, end, Escape);
                    }
                    '|' if token.kind() == Atom => add(offset, offset + 1, Delimiter),
                    _ => {}
                }
            }
        }
        _ => {}
    }
    units
}
