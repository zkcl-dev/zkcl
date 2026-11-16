use super::{
    EditError, Group, contains_unknown_reader, group_at_caret, surface_form, validate_range,
};
use crate::ast::{AstForm, Element, Token, tokens_at_offset};
use crate::source_workspace::SourceSnapshot;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};
use rowan::Direction;

/// Cursor movement in zkcl's source syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    ForwardForm,
    BackwardForm,
    UpList,
    BackwardUpList,
    DownList,
}

/// Move a caret without changing the source. `None` means there is no target.
/// Forward/backward form motion skips trivia on the current level but stops
/// at a group boundary. Up exits the containing group, and down enters the
/// next group on the current level. Reader prefixes move with their operand.
pub fn move_cursor(
    source: &SourceSnapshot,
    at: TextSize,
    motion: Motion,
) -> Result<Option<TextSize>, EditError> {
    validate_range(source.text(), TextRange::empty(at))?;
    let root = source.parsed().root();
    match motion {
        Motion::ForwardForm => {
            Ok(adjacent_form(source, &root, at, Direction::Next)?
                .map(|form| form.text_range().end()))
        }
        Motion::BackwardForm => Ok(adjacent_form(source, &root, at, Direction::Prev)?
            .map(|form| form.text_range().start())),
        Motion::UpList | Motion::BackwardUpList => {
            let Some(group) = group_at_caret(&root, at) else {
                return Ok(None);
            };
            let range = if motion == Motion::UpList {
                let Some(close) = group.closer else {
                    return Ok(None);
                };
                close.text_range()
            } else {
                group.opener.text_range()
            };
            known(source, at.max(range.end()) - TextSize::from(1))?;
            Ok(Some(if motion == Motion::UpList {
                range.end()
            } else {
                range.start()
            }))
        }
        Motion::DownList => {
            let mut cursor = at;
            while let Some(form) = adjacent_form(source, &root, cursor, Direction::Next)? {
                cursor = form.text_range().end();
                if let Some(group) = first_group(form, at) {
                    return Ok(Some(group.opener.text_range().end()));
                }
            }
            Ok(None)
        }
    }
}

fn known(source: &SourceSnapshot, at: TextSize) -> Result<(), EditError> {
    if source.editable_reader_syntax_at(at) {
        Ok(())
    } else {
        Err(EditError::UnknownReaderBoundary)
    }
}

fn adjacent_form(
    source: &SourceSnapshot,
    root: &SyntaxNode,
    at: TextSize,
    direction: Direction,
) -> Result<Option<AstForm>, EditError> {
    let adjacent = tokens_at_offset(root, at);
    let mut token = match direction {
        Direction::Next => adjacent
            .right_biased()
            .filter(|token| at < token.text_range().end()),
        Direction::Prev => adjacent
            .left_biased()
            .filter(|token| token.text_range().start() < at),
    };
    while let Some(current) = token {
        known(source, current.text_range().start())?;
        use SyntaxKind::*;
        let form = match (current.kind(), direction) {
            (CloseParen, Direction::Next) | (OpenParen | VectorStart, Direction::Prev) => {
                return Ok(None);
            }
            (CloseParen | OpenParen | VectorStart, _) => current
                .parent()
                .filter(|node| matches!(node.kind(), List | Vector))
                .and_then(|node| AstForm::cast(Element::Node(node))),
            (Atom | String | RawString | Character, _) => {
                AstForm::cast(Element::Token(current.clone()))
            }
            (UnknownDispatch | Error, _) => return Err(EditError::UnknownReaderBoundary),
            _ => reader_parent(&current),
        };
        if let Some(form) = form {
            let form = surface_form(form);
            let range = form.text_range();
            known(source, range.end() - TextSize::from(1))?;
            if contains_unknown_reader(&form.clone().into_element()) {
                return Err(EditError::UnknownReaderBoundary);
            }
            return Ok(Some(form));
        }
        token = match direction {
            Direction::Next => current.next_token(),
            Direction::Prev => current.prev_token(),
        };
    }
    Ok(None)
}

fn reader_parent(token: &Token) -> Option<AstForm> {
    let parent = token
        .parent()
        .filter(|parent| parent.kind() == SyntaxKind::ReaderForm)?;
    AstForm::cast(Element::Node(parent))
}

fn first_group(form: AstForm, at: TextSize) -> Option<Group> {
    if let Some(reader) = form.as_reader_form() {
        return reader
            .operands()
            .find_map(|operand| first_group(operand, at));
    }
    let group = Group::cast(form.into_element().into_node()?)?;
    (at <= group.opener.text_range().start()).then_some(group)
}
