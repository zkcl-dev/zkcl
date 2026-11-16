//! GC-safe handles for lossless syntax snapshots and structural edits.
//!
//! Foreign values contain Rust data only. Every foreign borrow ends before a
//! Lisp value is allocated, so the collector never sees untraced Lisp edges.
use std::sync::Arc;

use zkcl_syntax::ast::{AstForm, Element};
use zkcl_syntax::editing::{
    Edit, EditError, Motion, Selection, StructuralOperation, edit, insert_delimiter, move_cursor,
};
use zkcl_syntax::syntax::{SourceReadtable, SyntaxKind, TextRange, TextSize};
use zkcl_syntax::{SourceSnapshot, SourceWorkspace, TextEdit};

use crate::condition::{Fault, named_word};
use crate::heap::Word;
use crate::symbol::{Name, Package};
use crate::{EvalError, Root, Runtime, vm};

#[derive(Clone)]
struct Node {
    snapshot: Arc<SourceSnapshot>,
    element: Element,
}

#[derive(Clone, Copy)]
enum Operation {
    ParseSource,
    MakeWorkspace,
    SetReadtable,
    SetDocument,
    ReplaceText,
    RemoveDocument,
    Snapshot,
    SnapshotEqualP,
    Text,
    Root,
    NodeAt,
    Kind,
    Range,
    NodeText,
    NodeSnapshot,
    NodeEqualP,
    Parent,
    Children,
    Forms,
    PreviousSibling,
    NextSibling,
    Diagnostics,
    ByteOffset,
    CharacterIndex,
    SelectionRanges,
    MoveCursor,
    StructuralEdit,
    InsertDelimiter,
    MakeEdit,
    EditSource,
    EditChanges,
    EditSelection,
    ApplyEdit,
    ApplyEditToDocument,
    WorkspaceP,
    SnapshotP,
    NodeP,
    EditP,
}

pub(super) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    use Operation::*;
    for (name, operation) in [
        ("PARSE-SOURCE", ParseSource),
        ("MAKE-WORKSPACE", MakeWorkspace),
        ("SET-READTABLE", SetReadtable),
        ("SET-DOCUMENT", SetDocument),
        ("REPLACE-TEXT", ReplaceText),
        ("REMOVE-DOCUMENT", RemoveDocument),
        ("SNAPSHOT", Snapshot),
        ("SNAPSHOT-EQUAL-P", SnapshotEqualP),
        ("TEXT", Text),
        ("ROOT", Root),
        ("NODE-AT", NodeAt),
        ("KIND", Kind),
        ("RANGE", Range),
        ("NODE-TEXT", NodeText),
        ("NODE-SNAPSHOT", NodeSnapshot),
        ("NODE-EQUAL-P", NodeEqualP),
        ("PARENT", Parent),
        ("CHILDREN", Children),
        ("FORMS", Forms),
        ("PREVIOUS-SIBLING", PreviousSibling),
        ("NEXT-SIBLING", NextSibling),
        ("DIAGNOSTICS", Diagnostics),
        ("BYTE-OFFSET", ByteOffset),
        ("CHARACTER-INDEX", CharacterIndex),
        ("SELECTION-RANGES", SelectionRanges),
        ("MOVE-CURSOR", MoveCursor),
        ("STRUCTURAL-EDIT", StructuralEdit),
        ("INSERT-DELIMITER", InsertDelimiter),
        ("MAKE-EDIT", MakeEdit),
        ("EDIT-SOURCE", EditSource),
        ("EDIT-CHANGES", EditChanges),
        ("EDIT-SELECTION", EditSelection),
        ("APPLY-EDIT", ApplyEdit),
        ("APPLY-EDIT-TO-DOCUMENT", ApplyEditToDocument),
        ("WORKSPACEP", WorkspaceP),
        ("SNAPSHOTP", SnapshotP),
        ("NODEP", NodeP),
        ("EDITP", EditP),
    ] {
        vm::install_host_function(
            runtime,
            &format!("ZK::%SYNTAX-{name}"),
            std::sync::Arc::new(move |rt, args, at| execute(rt, operation, args, at)),
        )?;
    }
    Ok(())
}

fn error(at: usize, message: impl Into<String>) -> Fault {
    EvalError::new(at, message).into()
}

fn foreign_ref<T: 'static, R>(
    runtime: &Runtime,
    value: &Root<'_>,
    at: usize,
    expected: &str,
    read: impl FnOnce(&T) -> R,
) -> Result<R, Fault> {
    value.try_with_foreign_ref(runtime, read).map_err(|_| {
        error(
            at,
            format!("expected a ZK-SYNTAX {expected} belonging to this runtime"),
        )
    })
}

fn workspace_mut<R>(
    runtime: &Runtime,
    value: &Root<'_>,
    at: usize,
    update: impl FnOnce(&mut SourceWorkspace) -> R,
) -> Result<R, Fault> {
    value.try_with_foreign_mut(runtime, update).map_err(|_| {
        error(
            at,
            "expected a ZK-SYNTAX workspace belonging to this runtime",
        )
    })
}

fn snapshot(runtime: &Runtime, value: &Root<'_>, at: usize) -> Result<Arc<SourceSnapshot>, Fault> {
    foreign_ref(runtime, value, at, "snapshot", Arc::clone)
}

fn node(runtime: &Runtime, value: &Root<'_>, at: usize) -> Result<Node, Fault> {
    foreign_ref(runtime, value, at, "node", Node::clone)
}

fn string(runtime: &Runtime, value: &Root<'_>, at: usize) -> Result<String, Fault> {
    runtime.string(value).ok_or_else(|| {
        Fault::type_error(runtime, 
            value.slot.get(),
            "STRING",
            EvalError::new(at, "expected a string"),
        )
    })
}

fn size(runtime: &Runtime, value: &Root<'_>, at: usize) -> Result<usize, Fault> {
    let word = value.slot.get();
    let number = match word.as_fixnum() {
        Some(number) => usize::try_from(number).ok(),
        None => runtime
            .heap
            .integer(word)
            .and_then(|number| number.try_into().ok()),
    };
    number.ok_or_else(|| {
        Fault::non_negative_integer(runtime, 
            value.slot.get(),
            EvalError::new(
                at,
                "expected a non-negative integer fitting a machine index",
            ),
        )
    })
}

fn byte(source: &SourceSnapshot, offset: usize, at: usize) -> Result<TextSize, Fault> {
    if !source.text().is_char_boundary(offset) {
        return Err(error(
            at,
            "byte offset must be within the source and on a UTF-8 boundary",
        ));
    }
    TextSize::try_from(offset).map_err(|_| error(at, "byte offset exceeds the syntax offset limit"))
}

fn edit_selection(anchor: usize, head: usize) -> Result<Selection, EditError> {
    let offset = |value| TextSize::try_from(value).map_err(|_| EditError::InvalidRange);
    Ok(Selection::new(offset(anchor)?, offset(head)?))
}

fn input_list<'rt>(
    runtime: &'rt Runtime,
    value: &Root<'rt>,
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    // No Lisp allocation or reentry occurs between extracting words and rooting them.
    Ok(vm::proper_list(runtime, value.slot.get(), at)?
        .into_iter()
        .map(|word| runtime.root(word))
        .collect())
}

fn input_edits(
    runtime: &Runtime,
    value: &Root<'_>,
    at: usize,
) -> Result<Result<Vec<TextEdit>, EditError>, Fault> {
    let mut edits = Vec::new();
    for value in input_list(runtime, value, at)? {
        let fields = input_list(runtime, &value, at)?;
        let [start, end, replacement] = fields.as_slice() else {
            return Err(error(
                at,
                "each text edit must be a (start end replacement) triple",
            ));
        };
        let start = size(runtime, start, at)?;
        let end = size(runtime, end, at)?;
        let replacement = string(runtime, replacement, at)?;
        let selection = match edit_selection(start, end) {
            Ok(selection) if start <= end => selection,
            _ => return Ok(Err(EditError::InvalidRange)),
        };
        edits.push(TextEdit {
            range: selection.range(),
            replacement,
        });
    }
    Ok(Ok(edits))
}

fn edit_error_reason(error: EditError) -> &'static str {
    match error {
        EditError::InvalidRange => "INVALID-RANGE",
        EditError::NoForm => "NO-FORM",
        EditError::NoEnclosingGroup => "NO-ENCLOSING-GROUP",
        EditError::NoSibling => "NO-SIBLING",
        EditError::DottedBoundary => "DOTTED-BOUNDARY",
        EditError::UnknownReaderBoundary => "UNKNOWN-READER-BOUNDARY",
        EditError::IncompleteGroup => "INCOMPLETE-GROUP",
        EditError::UnsupportedSelection => "UNSUPPORTED-SELECTION",
        EditError::StaleSnapshot => "STALE-SNAPSHOT",
        EditError::OverlappingEdits => "OVERLAPPING-EDITS",
        EditError::UnsafeTokenBoundary => "UNSAFE-TOKEN-BOUNDARY",
        EditError::CannotInferParens => "CANNOT-INFER-PARENS",
    }
}

fn edit_failure_parts<'rt>(
    runtime: &'rt Runtime,
    reason: &str,
    message: &str,
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    Ok(vec![
        runtime.root(Word::NIL),
        keyword(runtime, reason, at)?,
        text(runtime, message, at)?,
    ])
}

fn edit_failure(runtime: &Runtime, error: EditError, at: usize) -> Result<Vec<Root<'_>>, Fault> {
    edit_failure_parts(runtime, edit_error_reason(error), &error.to_string(), at)
}

fn readtable(runtime: &Runtime, value: &Root<'_>, at: usize) -> Result<SourceReadtable, Fault> {
    match named_word(runtime, value.slot.get(), Package::KEYWORD).as_deref() {
        Some("STANDARD") => Ok(SourceReadtable::Standard),
        Some("RAW-STRINGS") => Ok(SourceReadtable::RawStrings),
        _ => Err(error(at, "readtable must be :standard or :raw-strings")),
    }
}

fn structural_operation(
    runtime: &Runtime,
    value: &Root<'_>,
    at: usize,
) -> Result<StructuralOperation, Fault> {
    use StructuralOperation::*;
    match named_word(runtime, value.slot.get(), Package::KEYWORD).as_deref() {
        Some("WRAP") => Ok(Wrap),
        Some("SPLICE") => Ok(Splice),
        Some("RAISE") => Ok(Raise),
        Some("SLURP-FORWARD") => Ok(SlurpForward),
        Some("SLURP-BACKWARD") => Ok(SlurpBackward),
        Some("BARF-FORWARD") => Ok(BarfForward),
        Some("BARF-BACKWARD") => Ok(BarfBackward),
        Some("SPLIT") => Ok(Split),
        Some("JOIN-FORWARD") => Ok(JoinForward),
        Some("JOIN-BACKWARD") => Ok(JoinBackward),
        Some("TRANSPOSE-FORWARD") => Ok(TransposeForward),
        Some("TRANSPOSE-BACKWARD") => Ok(TransposeBackward),
        Some("DELETE-FORWARD") => Ok(DeleteForward),
        Some("DELETE-BACKWARD") => Ok(DeleteBackward),
        Some("INDENT") => Ok(Indent),
        Some("INFER-PARENS") => Ok(InferParens),
        _ => Err(error(at, "unknown structural edit operation keyword")),
    }
}

fn motion(runtime: &Runtime, value: &Root<'_>, at: usize) -> Result<Motion, Fault> {
    match named_word(runtime, value.slot.get(), Package::KEYWORD).as_deref() {
        Some("FORWARD-FORM") => Ok(Motion::ForwardForm),
        Some("BACKWARD-FORM") => Ok(Motion::BackwardForm),
        Some("UP-LIST") => Ok(Motion::UpList),
        Some("BACKWARD-UP-LIST") => Ok(Motion::BackwardUpList),
        Some("DOWN-LIST") => Ok(Motion::DownList),
        _ => Err(error(at, "unknown source motion keyword")),
    }
}

fn boolean(runtime: &Runtime, value: bool) -> Root<'_> {
    runtime.root(if value { Word::T } else { Word::NIL })
}

fn foreign<T: 'static>(runtime: &Runtime, value: T, at: usize) -> Result<Root<'_>, Fault> {
    runtime
        .foreign(value)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn integer(runtime: &Runtime, value: usize, at: usize) -> Result<Root<'_>, Fault> {
    runtime
        .integer_from(value)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn text<'rt>(runtime: &'rt Runtime, value: &str, at: usize) -> Result<Root<'rt>, Fault> {
    runtime
        .make_string(value)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn keyword<'rt>(runtime: &'rt Runtime, value: &str, at: usize) -> Result<Root<'rt>, Fault> {
    let word = runtime.intern(&Name::keyword(value), at)?;
    Ok(runtime.root(word))
}

fn keyword_name(value: impl std::fmt::Debug) -> String {
    let name = format!("{value:?}");
    let mut result = String::with_capacity(name.len());
    for (index, ch) in name.chars().enumerate() {
        if index > 0 && ch.is_ascii_uppercase() {
            result.push('-');
        }
        result.push(ch.to_ascii_uppercase());
    }
    result
}

fn list<'rt>(runtime: &'rt Runtime, values: Vec<Root<'rt>>, at: usize) -> Result<Root<'rt>, Fault> {
    let mut result = runtime.root(Word::NIL);
    for value in values.iter().rev() {
        result = runtime
            .cons(value, &result)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    Ok(result)
}

fn range_values(runtime: &Runtime, range: TextRange, at: usize) -> Result<Vec<Root<'_>>, Fault> {
    Ok(vec![
        integer(runtime, usize::from(range.start()), at)?,
        integer(runtime, usize::from(range.end()), at)?,
    ])
}

fn optional_node(runtime: &Runtime, value: Option<Node>, at: usize) -> Result<Root<'_>, Fault> {
    match value {
        Some(value) => foreign(runtime, value, at),
        None => Ok(runtime.root(Word::NIL)),
    }
}

fn sibling(node: &Node, forward: bool) -> Option<Node> {
    let parent = node.element.parent()?;
    let mut previous = None;
    let mut found = false;
    for element in parent.children_with_tokens() {
        if element == node.element {
            if !forward {
                return previous;
            }
            found = true;
        } else if source_form(&element) {
            let candidate = Node {
                snapshot: Arc::clone(&node.snapshot),
                element,
            };
            if found {
                return Some(candidate);
            }
            previous = Some(candidate);
        }
    }
    None
}

fn source_form(element: &Element) -> bool {
    element.kind() != SyntaxKind::Dot && AstForm::cast(element.clone()).is_some()
}

fn execute<'rt>(
    runtime: &'rt Runtime,
    operation: Operation,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    use Operation::*;
    let result = match (operation, args) {
        (ParseSource, [source, table]) => {
            let text = string(runtime, source, at)?;
            let readtable = readtable(runtime, table, at)?;
            foreign(runtime, SourceSnapshot::from_text(text, readtable), at)?
        }
        (MakeWorkspace, [] | [_]) => {
            let mut workspace = SourceWorkspace::new();
            if let [value] = args {
                workspace.set_readtable(readtable(runtime, value, at)?);
            }
            foreign(runtime, workspace, at)?
        }
        (SetReadtable, [workspace, value]) => {
            let readtable = readtable(runtime, value, at)?;
            boolean(
                runtime,
                workspace_mut(runtime, workspace, at, |workspace| {
                    workspace.set_readtable(readtable)
                })?,
            )
        }
        (SetDocument, [workspace, name, value]) => {
            let name = string(runtime, name, at)?;
            let value = string(runtime, value, at)?;
            if u32::try_from(value.len()).is_err() {
                return Err(error(at, "document exceeds the syntax byte offset limit"));
            }
            let source = workspace_mut(runtime, workspace, at, |workspace| {
                workspace.set_document(&name, value);
                workspace
                    .snapshot(&name)
                    .expect("document was just installed")
            })?;
            foreign(runtime, source, at)?
        }
        (ReplaceText, [workspace, name, start, end, replacement]) => {
            let name = string(runtime, name, at)?;
            let start = size(runtime, start, at)?;
            let end = size(runtime, end, at)?;
            let replacement = string(runtime, replacement, at)?;
            let after = workspace_mut(runtime, workspace, at, |workspace| {
                let source = workspace
                    .snapshot(&name)
                    .ok_or_else(|| "unknown syntax document".to_owned())?;
                let original = source.text().get(start..end).ok_or_else(|| {
                    "edit range is outside the source or splits a UTF-8 character".to_owned()
                })?;
                let new_len = source
                    .text()
                    .len()
                    .checked_sub(original.len())
                    .and_then(|len| len.checked_add(replacement.len()));
                if new_len.is_none_or(|len| u32::try_from(len).is_err()) {
                    return Err("document exceeds the syntax byte offset limit".to_owned());
                }
                workspace
                    .apply_edit(&name, start..end, &replacement)
                    .map_err(|err| err.to_string())?;
                Ok(workspace
                    .snapshot(&name)
                    .expect("edited document remains installed"))
            })?
            .map_err(|message| error(at, message))?;
            foreign(runtime, after, at)?
        }
        (RemoveDocument, [workspace, name]) => {
            let name = string(runtime, name, at)?;
            boolean(
                runtime,
                workspace_mut(runtime, workspace, at, |workspace| {
                    workspace.remove_document(&name)
                })?,
            )
        }
        (Snapshot, [workspace, name]) => {
            let name = string(runtime, name, at)?;
            let source = foreign_ref::<SourceWorkspace, _>(
                runtime,
                workspace,
                at,
                "workspace",
                |workspace| workspace.snapshot(&name),
            )?;
            match source {
                Some(source) => foreign(runtime, source, at)?,
                None => runtime.root(Word::NIL),
            }
        }
        (SnapshotEqualP, [left, right]) => {
            let left = snapshot(runtime, left, at)?;
            let right = snapshot(runtime, right, at)?;
            boolean(runtime, Arc::ptr_eq(&left, &right))
        }
        (Text, [value]) => text(runtime, snapshot(runtime, value, at)?.text(), at)?,
        (Root, [value]) => {
            let snapshot = snapshot(runtime, value, at)?;
            let element = snapshot.parsed().root().into();
            foreign(runtime, Node { snapshot, element }, at)?
        }
        (NodeAt, [value, offset]) => {
            let snapshot = snapshot(runtime, value, at)?;
            let offset = byte(&snapshot, size(runtime, offset, at)?, at)?;
            let element = zkcl_syntax::tokens_at_offset(&snapshot.parsed().root(), offset)
                .right_biased()
                .map(Element::from);
            optional_node(
                runtime,
                element.map(|element| Node { snapshot, element }),
                at,
            )?
        }
        (Kind, [value]) => keyword(
            runtime,
            &keyword_name(node(runtime, value, at)?.element.kind()),
            at,
        )?,
        (Range, [value]) => {
            return range_values(runtime, node(runtime, value, at)?.element.text_range(), at);
        }
        (NodeText, [value]) => {
            let node = node(runtime, value, at)?;
            let range = node.element.text_range();
            text(
                runtime,
                &node.snapshot.text()[usize::from(range.start())..usize::from(range.end())],
                at,
            )?
        }
        (NodeSnapshot, [value]) => foreign(runtime, node(runtime, value, at)?.snapshot, at)?,
        (NodeEqualP, [left, right]) => {
            let left = node(runtime, left, at)?;
            let right = node(runtime, right, at)?;
            boolean(
                runtime,
                Arc::ptr_eq(&left.snapshot, &right.snapshot)
                    && left.element.kind() == right.element.kind()
                    && left.element.text_range() == right.element.text_range(),
            )
        }
        (Parent, [value]) => {
            let node = node(runtime, value, at)?;
            optional_node(
                runtime,
                node.element.parent().map(|element| Node {
                    snapshot: node.snapshot,
                    element: element.into(),
                }),
                at,
            )?
        }
        (Children | Forms, [value]) => {
            let node = node(runtime, value, at)?;
            let children: Vec<_> = node
                .element
                .as_node()
                .map(|parent| {
                    parent
                        .children_with_tokens()
                        .filter(|element| matches!(operation, Children) || source_form(element))
                        .collect()
                })
                .unwrap_or_default();
            let values = children
                .into_iter()
                .map(|element| {
                    foreign(
                        runtime,
                        Node {
                            snapshot: Arc::clone(&node.snapshot),
                            element,
                        },
                        at,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            list(runtime, values, at)?
        }
        (PreviousSibling | NextSibling, [value]) => {
            let node = node(runtime, value, at)?;
            optional_node(
                runtime,
                sibling(&node, matches!(operation, NextSibling)),
                at,
            )?
        }
        (Diagnostics, [value]) => {
            let source = snapshot(runtime, value, at)?;
            let mut values = Vec::new();
            for diagnostic in source.diagnostics() {
                let range = diagnostic.range();
                values.push(list(
                    runtime,
                    vec![
                        keyword(runtime, "KIND", at)?,
                        keyword(runtime, &keyword_name(diagnostic.kind()), at)?,
                        keyword(runtime, "START", at)?,
                        integer(runtime, usize::from(range.start()), at)?,
                        keyword(runtime, "END", at)?,
                        integer(runtime, usize::from(range.end()), at)?,
                        keyword(runtime, "MESSAGE", at)?,
                        text(runtime, diagnostic.message(), at)?,
                        keyword(runtime, "NEEDS-MORE-INPUT", at)?,
                        boolean(runtime, diagnostic.needs_more_input()),
                    ],
                    at,
                )?);
            }
            list(runtime, values, at)?
        }
        (ByteOffset, [value, index]) => {
            let source = snapshot(runtime, value, at)?;
            let index = size(runtime, index, at)?;
            let offset = source
                .text()
                .char_indices()
                .map(|(offset, _)| offset)
                .chain(std::iter::once(source.text().len()))
                .nth(index)
                .ok_or_else(|| error(at, "character index exceeds the source length"))?;
            integer(runtime, offset, at)?
        }
        (CharacterIndex, [value, offset]) => {
            let source = snapshot(runtime, value, at)?;
            let offset = size(runtime, offset, at)?;
            byte(&source, offset, at)?;
            integer(runtime, source.text()[..offset].chars().count(), at)?
        }
        (SelectionRanges, [value, offset]) => {
            let source = snapshot(runtime, value, at)?;
            let offset = size(runtime, offset, at)?;
            byte(&source, offset, at)?;
            let ranges = source
                .selection_ranges(offset)
                .ok_or_else(|| error(at, "invalid selection byte offset"))?;
            let values = ranges
                .into_iter()
                .map(|range| list(runtime, range_values(runtime, range, at)?, at))
                .collect::<Result<Vec<_>, Fault>>()?;
            list(runtime, values, at)?
        }
        (MoveCursor, [value, offset, direction]) => {
            let source = snapshot(runtime, value, at)?;
            let offset = match TextSize::try_from(size(runtime, offset, at)?) {
                Ok(offset) => offset,
                Err(_) => return edit_failure(runtime, EditError::InvalidRange, at),
            };
            let direction = motion(runtime, direction, at)?;
            match move_cursor(&source, offset, direction) {
                Ok(Some(result)) => integer(runtime, usize::from(result), at)?,
                Ok(None) => runtime.root(Word::NIL),
                Err(err) => return edit_failure(runtime, err, at),
            }
        }
        (StructuralEdit, [value, anchor, head, operation]) => {
            let source = snapshot(runtime, value, at)?;
            let selection =
                match edit_selection(size(runtime, anchor, at)?, size(runtime, head, at)?) {
                    Ok(selection) => selection,
                    Err(err) => return edit_failure(runtime, err, at),
                };
            let operation = structural_operation(runtime, operation, at)?;
            let edit = match edit(source, selection, operation) {
                Ok(edit) => edit,
                Err(err) => return edit_failure(runtime, err, at),
            };
            foreign(runtime, edit, at)?
        }
        (InsertDelimiter, [value, anchor, head, delimiter]) => {
            let source = snapshot(runtime, value, at)?;
            let selection =
                match edit_selection(size(runtime, anchor, at)?, size(runtime, head, at)?) {
                    Ok(selection) => selection,
                    Err(err) => return edit_failure(runtime, err, at),
                };
            let character = delimiter.as_character().ok_or_else(|| {
                Fault::type_error(runtime, 
                    delimiter.slot.get(),
                    "CHARACTER",
                    EvalError::new(at, "expected a delimiter character"),
                )
            })?;
            match insert_delimiter(source, selection, character) {
                Ok(edit) => foreign(runtime, edit, at)?,
                Err(err) => return edit_failure(runtime, err, at),
            }
        }
        (MakeEdit, [value, changes, anchor, head]) => {
            let source = snapshot(runtime, value, at)?;
            let changes = match input_edits(runtime, changes, at)? {
                Ok(changes) => changes,
                Err(err) => return edit_failure(runtime, err, at),
            };
            let selection =
                match edit_selection(size(runtime, anchor, at)?, size(runtime, head, at)?) {
                    Ok(selection) => selection,
                    Err(err) => return edit_failure(runtime, err, at),
                };
            let edit = match Edit::from_changes(source, changes, selection) {
                Ok(edit) => edit,
                Err(err) => return edit_failure(runtime, err, at),
            };
            foreign(runtime, edit, at)?
        }
        (EditSource, [value]) => {
            let source = foreign_ref::<Edit, _>(runtime, value, at, "edit", |edit| {
                Arc::clone(edit.source())
            })?;
            foreign(runtime, source, at)?
        }
        (EditChanges, [value]) => {
            let changes =
                foreign_ref::<Edit, _>(runtime, value, at, "edit", |edit| edit.changes().to_vec())?;
            let values = changes
                .into_iter()
                .map(|edit| {
                    let mut values = range_values(runtime, edit.range, at)?;
                    values.push(text(runtime, &edit.replacement, at)?);
                    list(runtime, values, at)
                })
                .collect::<Result<Vec<_>, Fault>>()?;
            list(runtime, values, at)?
        }
        (EditSelection, [value]) => {
            let selection = foreign_ref::<Edit, _>(runtime, value, at, "edit", Edit::selection)?;
            return Ok(vec![
                integer(runtime, usize::from(selection.anchor), at)?,
                integer(runtime, usize::from(selection.head), at)?,
            ]);
        }
        (ApplyEdit, [value]) => {
            let result = foreign_ref::<Edit, _>(runtime, value, at, "edit", Edit::apply)?;
            text(runtime, &result, at)?
        }
        (ApplyEditToDocument, [workspace, name, value]) => {
            let name = string(runtime, name, at)?;
            let edit = foreign_ref::<Edit, _>(runtime, value, at, "edit", Edit::clone)?;
            let after = workspace_mut(runtime, workspace, at, |workspace| {
                let current = workspace
                    .snapshot(&name)
                    .ok_or_else(|| ("UNKNOWN-DOCUMENT", "unknown syntax document".to_owned()))?;
                let result = edit
                    .apply_to(&current)
                    .map_err(|err| (edit_error_reason(err), err.to_string()))?;
                workspace.set_document(&name, result);
                Ok(workspace
                    .snapshot(&name)
                    .expect("document remains installed"))
            })?;
            let after = match after {
                Ok(after) => after,
                Err((reason, message)) => return edit_failure_parts(runtime, reason, &message, at),
            };
            foreign(runtime, after, at)?
        }
        (WorkspaceP, [value]) => boolean(
            runtime,
            value
                .try_with_foreign_ref::<SourceWorkspace, _>(runtime, |_| ())
                .is_ok(),
        ),
        (SnapshotP, [value]) => boolean(
            runtime,
            value
                .try_with_foreign_ref::<Arc<SourceSnapshot>, _>(runtime, |_| ())
                .is_ok(),
        ),
        (NodeP, [value]) => boolean(
            runtime,
            value
                .try_with_foreign_ref::<Node, _>(runtime, |_| ())
                .is_ok(),
        ),
        (EditP, [value]) => boolean(
            runtime,
            value
                .try_with_foreign_ref::<Edit, _>(runtime, |_| ())
                .is_ok(),
        ),
        _ => {
            return Err(super::arity_error(
                at,
                "wrong number of arguments to syntax operation",
            ));
        }
    };
    Ok(vec![result])
}
