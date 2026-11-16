//! Structural editing over zkcl source snapshots.
//!
//! Transformations move or remove concrete syntax delimiters. It never evaluates
//! reader syntax and does not normalize the text between affected delimiters.

use std::sync::Arc;

use rowan::Direction;

use crate::ast::{AstForm, AstList, AstVector, Element, Token};
use crate::format::TextEdit;
use crate::source_workspace::SourceSnapshot;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

mod infer;
mod input;
mod motion;
pub use input::insert_delimiter;
pub use motion::{Motion, move_cursor};

/// A structural source transformation requested for a selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralOperation {
    /// Surround the selected whole forms with a new list.
    Wrap,
    /// Remove the delimiters around the selected list or vector.
    Splice,
    /// Replace the enclosing form with the selected whole forms.
    Raise,
    /// Move the closing delimiter past the next sibling form.
    SlurpForward,
    /// Move the opening delimiter before the previous sibling form.
    SlurpBackward,
    /// Move the closing delimiter before the last sibling form.
    BarfForward,
    /// Move the opening delimiter after the first sibling form.
    BarfBackward,
    /// Split a proper list/vector between direct children at the cursor.
    Split,
    /// Join the selected group with the following group of the same kind.
    JoinForward,
    /// Join the selected group with the preceding group of the same kind.
    JoinBackward,
    /// Exchange selected sibling forms with the following sibling.
    TransposeForward,
    /// Exchange selected sibling forms with the preceding sibling.
    TransposeBackward,
    /// Delete the next character or an empty structural pair at the caret.
    DeleteForward,
    /// Delete the previous character or an empty structural pair at the caret.
    DeleteBackward,
    /// Set the indentation of the lines touched by the selection.
    Indent,
    /// Infer end-of-line closing parentheses from the source's indentation.
    InferParens,
}

/// A source edit that could not be created or applied safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("selection is outside the source or splits a UTF-8 character")]
    InvalidRange,
    #[error("selection does not identify a whole Lisp form")]
    NoForm,
    #[error("selection has no enclosing list or vector")]
    NoEnclosingGroup,
    #[error("there is no sibling form on that side")]
    NoSibling,
    #[error("the operation would cross a dotted-list boundary")]
    DottedBoundary,
    #[error("the operation would cross an unknown reader boundary")]
    UnknownReaderBoundary,
    #[error("the enclosing list or vector is incomplete for this operation")]
    IncompleteGroup,
    #[error("the operation does not support this form or selection")]
    UnsupportedSelection,
    #[error("the edit belongs to a different source snapshot")]
    StaleSnapshot,
    #[error("source changes overlap or share a start")]
    OverlappingEdits,
    #[error("the operation would change an existing token boundary")]
    UnsafeTokenBoundary,
    #[error("cannot infer closing parentheses with these reader forms or fixed delimiters")]
    CannotInferParens,
}

/// A directed UTF-8 byte selection. Anchor is fixed; head is the active end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: TextSize,
    pub head: TextSize,
}

impl Selection {
    pub fn new(anchor: TextSize, head: TextSize) -> Self {
        Self { anchor, head }
    }

    pub fn range(self) -> TextRange {
        TextRange::new(self.anchor.min(self.head), self.anchor.max(self.head))
    }

    fn with_range(self, range: TextRange) -> Self {
        if self.anchor > self.head {
            Self::new(range.end(), range.start())
        } else {
            Self::new(range.start(), range.end())
        }
    }
}

/// Atomic source changes and a directed result selection.
#[derive(Debug, Clone)]
pub struct Edit {
    snapshot: Arc<SourceSnapshot>,
    edits: Vec<TextEdit>,
    selection: Selection,
}

impl Edit {
    /// Build a generic text transaction, including Lisp-authored commands.
    /// Edits must already be in ascending source order. Selection coordinates
    /// refer to the edited text; arbitrary replacements are permitted. Edits
    /// sharing a start (including duplicate point insertions) are rejected.
    pub fn from_changes(
        snapshot: Arc<SourceSnapshot>,
        edits: Vec<TextEdit>,
        selection: Selection,
    ) -> Result<Self, EditError> {
        validate_edits(snapshot.text(), &edits)?;
        let result_length = edits
            .iter()
            .try_fold(snapshot.text().len(), |length, edit| {
                length
                    .checked_sub(usize::from(edit.range.len()))?
                    .checked_add(edit.replacement.len())
            })
            .ok_or(EditError::InvalidRange)?;
        TextSize::try_from(result_length).map_err(|_| EditError::InvalidRange)?;
        let result = apply_edits(snapshot.text(), &edits);
        validate_range(&result, selection.range())?;
        Ok(Self {
            snapshot,
            edits,
            selection,
        })
    }

    /// The immutable source snapshot against which these changes were built.
    pub fn source(&self) -> &Arc<SourceSnapshot> {
        &self.snapshot
    }
    pub fn changes(&self) -> &[TextEdit] {
        &self.edits
    }
    /// Directed selection in the edited text, in UTF-8 byte coordinates.
    pub fn selection(&self) -> Selection {
        self.selection
    }

    /// Apply these validated changes to their own immutable source.
    pub fn apply(&self) -> String {
        apply_edits(self.snapshot.text(), &self.edits)
    }

    /// Apply only if `source` is the exact snapshot object used for this edit.
    pub fn apply_to(&self, source: &Arc<SourceSnapshot>) -> Result<String, EditError> {
        if !Arc::ptr_eq(&self.snapshot, source) {
            return Err(EditError::StaleSnapshot);
        }
        Ok(self.apply())
    }
}

/// Transform local concrete syntax without evaluating reader forms.
///
/// At a cursor, an atom selects itself and trivia selects a direct sibling in
/// the innermost containing group (right first). Comments are never selected.
/// A nonempty selection must consist of whole direct siblings, with optional
/// surrounding trivia. Reader prefixes stay attached to their operands.
/// Split accepts a cursor between direct group children; join requires two
/// unprefixed groups with identical openers. Strings are not split or joined.
pub fn edit(
    snapshot: Arc<SourceSnapshot>,
    directed: Selection,
    operation: StructuralOperation,
) -> Result<Edit, EditError> {
    let selection = directed.range();
    validate_range(snapshot.text(), selection)?;
    let root = snapshot.parsed().root();
    match operation {
        StructuralOperation::DeleteForward => {
            return input::delete(snapshot, &root, selection, Direction::Next);
        }
        StructuralOperation::DeleteBackward => {
            return input::delete(snapshot, &root, selection, Direction::Prev);
        }
        StructuralOperation::Indent => return indent(snapshot, directed),
        StructuralOperation::InferParens => return infer::infer_parens(snapshot, &root, directed),
        _ => {}
    }
    if selection.is_empty()
        && crate::ast::tokens_at_offset(&root, selection.start())
            .into_iter()
            .any(|token| {
                (token.text_range().contains(selection.start())
                    || (token.text_range().end() == selection.start()
                        && (token.kind() == SyntaxKind::LineComment
                            || (token.kind() == SyntaxKind::BlockComment && snapshot.parsed().diagnostics().iter().any(|diagnostic| diagnostic.kind() == crate::syntax::SyntaxDiagnosticKind::UnterminatedBlockComment && token.text_range().contains_range(diagnostic.range()))))))
                    && matches!(
                        token.kind(),
                        SyntaxKind::LineComment | SyntaxKind::BlockComment
                    )
            })
    {
        return Err(EditError::NoForm);
    }
    if !selection.is_empty()
        && !matches!(
            operation,
            StructuralOperation::Wrap
                | StructuralOperation::Raise
                | StructuralOperation::TransposeForward
                | StructuralOperation::TransposeBackward
        )
    {
        select_forms(&root, selection)?;
    }
    let edits = match operation {
        StructuralOperation::Wrap => plan_wrap(&snapshot, &root, selection)?,
        StructuralOperation::Splice => plan_splice(&snapshot, &root, selection)?,
        StructuralOperation::Raise => plan_raise(&snapshot, &root, selection)?,
        StructuralOperation::SlurpForward => plan_slurp_forward(&snapshot, &root, selection)?,
        StructuralOperation::SlurpBackward => plan_slurp_backward(&snapshot, &root, selection)?,
        StructuralOperation::BarfForward => plan_barf_forward(&snapshot, &root, selection)?,
        StructuralOperation::BarfBackward => plan_barf_backward(&snapshot, &root, selection)?,
        StructuralOperation::Split => plan_split(&root, selection)?,
        StructuralOperation::JoinForward => plan_join(&root, selection, Direction::Next)?,
        StructuralOperation::JoinBackward => plan_join(&root, selection, Direction::Prev)?,
        StructuralOperation::TransposeForward => {
            plan_transpose(&snapshot, &root, selection, Direction::Next)?
        }
        StructuralOperation::TransposeBackward => {
            plan_transpose(&snapshot, &root, selection, Direction::Prev)?
        }
        StructuralOperation::DeleteForward
        | StructuralOperation::DeleteBackward
        | StructuralOperation::Indent
        | StructuralOperation::InferParens => unreachable!(),
    };
    let mut edits = normalize_edits(snapshot.text(), edits)?;
    ensure_editable(&snapshot, &edits)?;
    let padding = preserve_boundaries(&snapshot, &root, &mut edits);
    let selected = if operation == StructuralOperation::Raise && selection.is_empty() {
        let forms = select_forms(&root, selection)?;
        let source = forms.first().ok_or(EditError::NoForm)?.text_range();
        let at = selection.start().max(source.start()).min(source.end());
        let start = edits.first().ok_or(EditError::NoForm)?.range.start();
        let leading = padding[0].0;
        TextRange::empty(
            start
                + TextSize::try_from(leading).map_err(|_| EditError::InvalidRange)?
                + (at - source.start()),
        )
    } else if matches!(
        operation,
        StructuralOperation::TransposeForward | StructuralOperation::TransposeBackward
    ) {
        transpose_selection(&root, selection, operation, &edits, &padding)?
    } else {
        result_selection(operation, selection, &edits, &padding)?
    };
    let transpose = match operation {
        StructuralOperation::TransposeForward => Some((selection, Direction::Next)),
        StructuralOperation::TransposeBackward => Some((selection, Direction::Prev)),
        _ => None,
    };
    let (result, _) = checked_text(&snapshot, &root, &edits, transpose, None)?;
    validate_range(&result, selected)?;
    Ok(Edit {
        snapshot,
        edits,
        selection: directed.with_range(selected),
    })
}

fn indent(snapshot: Arc<SourceSnapshot>, selection: Selection) -> Result<Edit, EditError> {
    let changes = crate::format::indent_edits(&snapshot, selection.range());
    let anchor = map_offset(selection.anchor, &changes, Affinity::After)?;
    let head = map_offset(selection.head, &changes, Affinity::After)?;
    Edit::from_changes(snapshot, changes, Selection::new(anchor, head))
}

fn ensure_editable(snapshot: &SourceSnapshot, edits: &[TextEdit]) -> Result<(), EditError> {
    let Some((first, last)) = edits.first().zip(edits.last()) else {
        return Ok(());
    };
    let affected = TextRange::new(first.range.start(), last.range.end());
    // A nonempty change ending at the cutoff does not touch the next byte.
    // An insertion there would itself be read under the unknown readtable.
    let last_byte = if last.range.is_empty() {
        affected.end()
    } else {
        affected.end() - TextSize::from(1)
    };
    if !snapshot.editable_reader_syntax_at(last_byte)
        || snapshot
            .parsed()
            .readtable_changes()
            .iter()
            .any(|(span, _)| ranges_intersect(*span, affected))
    {
        return Err(EditError::UnknownReaderBoundary);
    }
    Ok(())
}

fn checked_text(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    edits: &[TextEdit],
    transpose: Option<(TextRange, Direction)>,
    changed_token: Option<&Token>,
) -> Result<(String, crate::syntax::ParsedSource), EditError> {
    let result = apply_edits(snapshot.text(), edits);
    TextSize::try_from(result.len()).map_err(|_| EditError::InvalidRange)?;
    let reparsed =
        crate::syntax::parse_source_with_readtable(&result, snapshot.parsed().initial_readtable());
    validate_surviving_tokens(snapshot, root, &reparsed, edits, changed_token)?;
    if let Some(token) = changed_token {
        let range = token.text_range();
        let start = map_offset(range.start(), edits, Affinity::Before)?;
        let end = map_offset(range.end(), edits, Affinity::After)?;
        let range = TextRange::new(start, end);
        let element = reparsed.root().covering_element(range);
        if element.text_range() != range || element.kind() != token.kind() {
            return Err(EditError::UnsafeTokenBoundary);
        }
    }
    validate_reader_context(snapshot, root, &result, &reparsed, edits, transpose)?;
    Ok((result, reparsed))
}

#[derive(Clone)]
struct Group {
    node: SyntaxNode,
    opener: Token,
    closer: Option<Token>,
    forms: Vec<AstForm>,
    dotted: bool,
}

impl Group {
    fn cast(node: SyntaxNode) -> Option<Self> {
        let (opener_kind, dotted, forms) = if let Some(list) = AstList::cast(node.clone()) {
            (
                SyntaxKind::OpenParen,
                list.is_dotted(),
                list.forms().collect::<Vec<_>>(),
            )
        } else {
            let vector = AstVector::cast(node.clone())?;
            (SyntaxKind::VectorStart, false, vector.forms().collect())
        };
        let opener = node.first_child_or_token()?.into_token()?;
        if opener.kind() != opener_kind {
            return None;
        }
        let closer = node
            .last_child_or_token()
            .and_then(Element::into_token)
            .filter(|token| token.kind() == SyntaxKind::CloseParen);
        Some(Self {
            node,
            opener,
            closer,
            forms,
            dotted,
        })
    }

    fn range(&self) -> TextRange {
        self.node.text_range()
    }

    fn opening_text(&self) -> &str {
        self.opener.text()
    }

    fn closing_text(&self) -> Result<&str, EditError> {
        self.closer
            .as_ref()
            .map(Token::text)
            .ok_or(EditError::IncompleteGroup)
    }

    fn ensure_boundary_is_safe(&self) -> Result<(), EditError> {
        if let Some(parent) = self
            .node
            .parent()
            .filter(|parent| parent.kind() == SyntaxKind::ReaderForm)
            && direct_forms(&parent)
                .last()
                .is_some_and(|last| last.text_range() != self.range())
        {
            return Err(EditError::UnsupportedSelection);
        }
        if self.dotted
            || self
                .node
                .ancestors()
                .skip(1)
                .filter_map(Group::cast)
                .next()
                .is_some_and(|parent| parent.dotted)
        {
            return Err(EditError::DottedBoundary);
        }
        if contains_unknown_reader(&Element::Node(self.node.clone())) {
            return Err(EditError::UnknownReaderBoundary);
        }
        Ok(())
    }

    fn counted_vector(&self) -> bool {
        self.node.kind() == SyntaxKind::Vector && self.opening_text() != "#("
    }

    fn outer_group(&self) -> Option<Group> {
        let form = surface_form(AstForm::cast(Element::Node(self.node.clone()))?);
        enclosing_group(&form)
    }

    fn ensure_cardinality_change_is_safe(&self) -> Result<(), EditError> {
        if self.counted_vector()
            || self
                .outer_group()
                .is_some_and(|parent| parent.counted_vector())
        {
            return Err(EditError::UnsupportedSelection);
        }
        self.ensure_boundary_is_safe()
    }

    fn next_form(&self) -> Option<AstForm> {
        sibling_form(
            &surface_form(AstForm::cast(Element::Node(self.node.clone()))?),
            Direction::Next,
        )
    }

    fn previous_form(&self) -> Option<AstForm> {
        sibling_form(
            &surface_form(AstForm::cast(Element::Node(self.node.clone()))?),
            Direction::Prev,
        )
    }

    fn opening_range(&self) -> TextRange {
        let form = surface_form(AstForm::cast(Element::Node(self.node.clone())).unwrap());
        TextRange::new(form.text_range().start(), self.opener.text_range().end())
    }
}

fn plan_wrap(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    selection: TextRange,
) -> Result<Vec<TextEdit>, EditError> {
    let forms = select_forms(root, selection)?;
    if forms.iter().any(has_unknown_reader_boundary) {
        return Err(EditError::UnknownReaderBoundary);
    }
    let start = forms.first().ok_or(EditError::NoForm)?.text_range().start();
    let end = forms.last().ok_or(EditError::NoForm)?.text_range().end();
    snapshot
        .text_at(TextRange::new(start, end))
        .ok_or(EditError::InvalidRange)?;
    Ok(vec![insert(start, "("), insert(end, ")")])
}

fn plan_splice(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    selection: TextRange,
) -> Result<Vec<TextEdit>, EditError> {
    let group = group_containing(root, selection).ok_or(EditError::NoEnclosingGroup)?;
    group.ensure_boundary_is_safe()?;
    if group
        .outer_group()
        .is_some_and(|parent| parent.counted_vector())
    {
        return Err(EditError::UnsupportedSelection);
    }
    if group
        .node
        .parent()
        .is_some_and(|parent| parent.kind() == SyntaxKind::ReaderForm)
    {
        return Err(EditError::UnsupportedSelection);
    }
    snapshot
        .text_at(group.range())
        .ok_or(EditError::InvalidRange)?;
    let mut edits = vec![remove(group.opener.text_range())];
    if let Some(closer) = group.closer {
        edits.push(remove(closer.text_range()));
    }
    Ok(edits)
}

fn plan_raise(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    selection: TextRange,
) -> Result<Vec<TextEdit>, EditError> {
    let forms = select_forms(root, selection)?;
    let first = forms.first().ok_or(EditError::NoForm)?;
    let last = forms.last().ok_or(EditError::NoForm)?;
    let parent = enclosing_group(first).ok_or(EditError::NoEnclosingGroup)?;
    parent.ensure_boundary_is_safe()?;
    if parent
        .outer_group()
        .is_some_and(|outer| outer.counted_vector())
        && (forms.len() != 1
            || forms[0]
                .as_reader_form()
                .is_some_and(|reader| reader.prefix_kind() == Some(SyntaxKind::ReaderConditional)))
    {
        return Err(EditError::UnsupportedSelection);
    }
    if forms.iter().any(has_unknown_reader_boundary) {
        return Err(EditError::UnknownReaderBoundary);
    }
    if forms
        .iter()
        .any(|form| enclosing_group(form).is_none_or(|group| group.node != parent.node))
    {
        return Err(EditError::UnsupportedSelection);
    }
    let selected = TextRange::new(first.text_range().start(), last.text_range().end());
    let replacement = snapshot
        .text_at(selected)
        .ok_or(EditError::InvalidRange)?
        .to_owned();
    Ok(vec![TextEdit {
        range: surface_form(AstForm::cast(Element::Node(parent.node)).unwrap()).text_range(),
        replacement,
    }])
}

fn plan_slurp_forward(
    _snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    selection: TextRange,
) -> Result<Vec<TextEdit>, EditError> {
    let group = group_containing(root, selection).ok_or(EditError::NoEnclosingGroup)?;
    group.ensure_cardinality_change_is_safe()?;
    let closer = group.closer.as_ref().ok_or(EditError::IncompleteGroup)?;
    let next = group.next_form().ok_or(EditError::NoSibling)?;
    if has_unknown_reader_boundary(&next) {
        return Err(EditError::UnknownReaderBoundary);
    }
    Ok(vec![
        remove(closer.text_range()),
        insert(next.text_range().end(), group.closing_text()?),
    ])
}

fn plan_slurp_backward(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    selection: TextRange,
) -> Result<Vec<TextEdit>, EditError> {
    let group = group_containing(root, selection).ok_or(EditError::NoEnclosingGroup)?;
    group.ensure_cardinality_change_is_safe()?;
    let previous = group.previous_form().ok_or(EditError::NoSibling)?;
    if has_unknown_reader_boundary(&previous) {
        return Err(EditError::UnknownReaderBoundary);
    }
    Ok(vec![
        remove(group.opening_range()),
        insert(
            previous.text_range().start(),
            snapshot.text_at(group.opening_range()).unwrap(),
        ),
    ])
}

fn plan_barf_forward(
    _snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    selection: TextRange,
) -> Result<Vec<TextEdit>, EditError> {
    let group = group_containing(root, selection).ok_or(EditError::NoEnclosingGroup)?;
    group.ensure_cardinality_change_is_safe()?;
    let closer = group.closer.as_ref().ok_or(EditError::IncompleteGroup)?;
    if group.forms.is_empty() {
        return Err(EditError::NoSibling);
    }
    let remaining_end = group
        .forms
        .iter()
        .rev()
        .nth(1)
        .map_or(group.opener.text_range().end(), |form| {
            form.text_range().end()
        });
    Ok(vec![
        remove(closer.text_range()),
        insert(
            remaining_end,
            if group.forms.len() == 1 {
                ") "
            } else {
                group.closing_text()?
            },
        ),
    ])
}

fn plan_barf_backward(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    selection: TextRange,
) -> Result<Vec<TextEdit>, EditError> {
    let group = group_containing(root, selection).ok_or(EditError::NoEnclosingGroup)?;
    group.ensure_cardinality_change_is_safe()?;
    if group.forms.is_empty() {
        return Err(EditError::NoSibling);
    }
    let remaining_start = group
        .forms
        .get(1)
        .map(|form| form.text_range().start())
        .or_else(|| {
            group
                .closer
                .as_ref()
                .map(|token| token.text_range().start())
        })
        .ok_or(EditError::IncompleteGroup)?;
    Ok(vec![
        remove(group.opening_range()),
        insert(
            remaining_start,
            &format!(
                "{}{}",
                if group.forms.len() == 1 { " " } else { "" },
                snapshot.text_at(group.opening_range()).unwrap()
            ),
        ),
    ])
}

fn plan_split(root: &SyntaxNode, selection: TextRange) -> Result<Vec<TextEdit>, EditError> {
    if !selection.is_empty() {
        return Err(EditError::UnsupportedSelection);
    }
    let group = group_containing(root, selection).ok_or(EditError::NoEnclosingGroup)?;
    group.ensure_cardinality_change_is_safe()?;
    group.closing_text()?;
    if group.opening_range() != group.opener.text_range() {
        return Err(EditError::UnsupportedSelection);
    }
    let at = selection.start();
    if at < group.opener.text_range().end()
        || at > group.closer.as_ref().unwrap().text_range().start()
    {
        return Err(EditError::UnsupportedSelection);
    }
    if group
        .forms
        .iter()
        .any(|form| form.text_range().start() < at && at < form.text_range().end())
    {
        return Err(EditError::UnsupportedSelection);
    }
    let left = group
        .forms
        .iter()
        .rev()
        .find(|form| form.text_range().end() <= at)
        .map_or(group.opener.text_range().end(), |form| {
            form.text_range().end()
        });
    let right = group
        .forms
        .iter()
        .find(|form| form.text_range().start() >= at)
        .map_or(
            group.closer.as_ref().unwrap().text_range().start(),
            |form| form.text_range().start(),
        );
    if left == right {
        Ok(vec![insert(left, &format!(") {}", group.opening_text()))])
    } else {
        Ok(vec![insert(left, ")"), insert(right, group.opening_text())])
    }
}

fn plan_join(
    root: &SyntaxNode,
    selection: TextRange,
    direction: Direction,
) -> Result<Vec<TextEdit>, EditError> {
    let between_groups = if selection.is_empty() {
        point_form(root, selection.start()).filter(|form| {
            matches!(
                form,
                AstForm::List(_) | AstForm::Vector(_) | AstForm::ReaderForm(_)
            ) && selection.start() <= form.text_range().start()
        })
    } else {
        None
    };
    let group = if let Some(right) = between_groups {
        let form = match direction {
            Direction::Next => sibling_form(&right, Direction::Prev).unwrap_or(right),
            Direction::Prev => right,
        };
        match form.into_element() {
            Element::Node(node) => Group::cast(node).ok_or(EditError::UnsupportedSelection)?,
            Element::Token(_) => return Err(EditError::UnsupportedSelection),
        }
    } else {
        group_containing(root, selection).ok_or(EditError::NoEnclosingGroup)?
    };
    group.ensure_cardinality_change_is_safe()?;
    if group.opening_range() != group.opener.text_range() {
        return Err(EditError::UnsupportedSelection);
    }
    let sibling = match direction {
        Direction::Next => group.next_form(),
        Direction::Prev => group.previous_form(),
    }
    .ok_or(EditError::NoSibling)?;
    if has_unknown_reader_boundary(&sibling) {
        return Err(EditError::UnknownReaderBoundary);
    }
    let Element::Node(node) = sibling.into_element() else {
        return Err(EditError::UnsupportedSelection);
    };
    let other = Group::cast(node).ok_or(EditError::UnsupportedSelection)?;
    other.ensure_cardinality_change_is_safe()?;
    if group.opening_text() != other.opening_text() {
        return Err(EditError::UnsupportedSelection);
    }
    let (left, right) = match direction {
        Direction::Next => (&group, &other),
        Direction::Prev => (&other, &group),
    };
    left.closing_text()?;
    right.closing_text()?;
    Ok(vec![
        remove(left.closer.as_ref().unwrap().text_range()),
        remove(right.opener.text_range()),
    ])
}

fn plan_transpose(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    selection: TextRange,
    direction: Direction,
) -> Result<Vec<TextEdit>, EditError> {
    let forms = select_forms(root, selection)?;
    if forms.iter().any(has_unknown_reader_boundary) {
        return Err(EditError::UnknownReaderBoundary);
    }
    let first = forms.first().ok_or(EditError::NoForm)?;
    let last = forms.last().unwrap();
    if enclosing_group(first).is_some_and(|group| group.dotted) {
        return Err(EditError::DottedBoundary);
    }
    let sibling = sibling_form(
        match direction {
            Direction::Next => last,
            Direction::Prev => first,
        },
        direction,
    )
    .ok_or(EditError::NoSibling)?;
    if has_unknown_reader_boundary(&sibling) {
        return Err(EditError::UnknownReaderBoundary);
    }
    let selected = TextRange::new(first.text_range().start(), last.text_range().end());
    let (left, right) = match direction {
        Direction::Next => (selected, sibling.text_range()),
        Direction::Prev => (sibling.text_range(), selected),
    };
    // Trivia stays at the same boundary; neither comments nor reader prefixes
    // are guessed to belong to a neighboring form.
    let gap = TextRange::new(left.end(), right.start());
    let left_text = snapshot.text_at(left).unwrap();
    let right_text = snapshot.text_at(right).unwrap();
    let mut separator = snapshot.text_at(gap).unwrap().to_owned();
    if separator.is_empty() {
        let table = snapshot.parsed().readtable_at(left.start());
        let expected: Vec<_> = crate::syntax::lexer::lex(right_text, table)
            .chain(crate::syntax::lexer::lex(left_text, table))
            .map(|token| (token.kind, token.text.to_owned()))
            .collect();
        let joined = format!("{right_text}{left_text}");
        let actual: Vec<_> = crate::syntax::lexer::lex(&joined, table)
            .map(|token| (token.kind, token.text.to_owned()))
            .collect();
        if actual != expected {
            separator.push(' ');
        }
    }
    Ok(vec![TextEdit {
        range: TextRange::new(left.start(), right.end()),
        replacement: format!("{right_text}{separator}{left_text}"),
    }])
}

fn dispatch(node: &SyntaxNode) -> Option<String> {
    if node.kind() != SyntaxKind::ReaderForm {
        return None;
    }
    let prefix = node
        .first_token()
        .filter(|token| token.kind() == SyntaxKind::DispatchPrefix)?;
    Some(prefix.text().strip_prefix('#')?.to_ascii_uppercase())
}

fn enclosing_group(form: &AstForm) -> Option<Group> {
    let mut node = match form.clone().into_element() {
        Element::Node(node) => node.parent(),
        Element::Token(token) => token.parent(),
    };
    while let Some(candidate) = node {
        if let Some(group) = Group::cast(candidate.clone()) {
            return Some(group);
        }
        node = candidate.parent();
    }
    None
}

fn group_containing(root: &SyntaxNode, selection: TextRange) -> Option<Group> {
    root.descendants()
        .filter_map(Group::cast)
        .filter(|group| {
            let range = group.range();
            if selection.is_empty() {
                range.contains(selection.start())
                    || (selection.start() == root.text_range().end()
                        && range.end() == selection.start())
            } else {
                range.contains_range(selection)
            }
        })
        .min_by_key(|group| group.range().len())
}

// A caret is between characters. Just before an opener and just after a
// closer it belongs to the enclosing group, not the group at that delimiter.
fn group_at_caret(root: &SyntaxNode, at: TextSize) -> Option<Group> {
    crate::ast::tokens_at_offset(root, at)
        .right_biased()?
        .parent_ancestors()
        .filter_map(Group::cast)
        .find(|group| {
            group.opener.text_range().end() <= at
                && group.closer.as_ref().map_or_else(
                    || at <= group.range().end(),
                    |token| at <= token.text_range().start(),
                )
        })
}

fn direct_forms(node: &SyntaxNode) -> Vec<AstForm> {
    node.children_with_tokens()
        .filter_map(AstForm::cast)
        .collect()
}

fn select_forms(root: &SyntaxNode, selection: TextRange) -> Result<Vec<AstForm>, EditError> {
    let forms = if selection.is_empty() {
        vec![point_form(root, selection.start()).ok_or(EditError::NoForm)?]
    } else {
        select_in_container(root, selection).ok_or(EditError::UnsupportedSelection)?
    };
    Ok(forms)
}

fn select_in_container(container: &SyntaxNode, selection: TextRange) -> Option<Vec<AstForm>> {
    let forms = direct_forms(container);
    if let Some(form) = forms
        .iter()
        .find(|form| form.text_range().contains_range(selection))
    {
        if form.text_range() == selection {
            return Some(vec![surface_form(form.clone())]);
        }
        if let Element::Node(node) = form.clone().into_element() {
            return select_in_container(&node, selection);
        }
        return None;
    }
    let selected: Vec<_> = forms
        .into_iter()
        .filter(|form| ranges_intersect(form.text_range(), selection))
        .collect();
    let first = selected.first()?;
    let last = selected.last()?;
    if !selection.contains_range(first.text_range()) || !selection.contains_range(last.text_range())
    {
        return None;
    }
    let leading = TextRange::new(selection.start(), first.text_range().start());
    let trailing = TextRange::new(last.text_range().end(), selection.end());
    let only_trivia = |range: TextRange| {
        container
            .children_with_tokens()
            .filter(|element| ranges_intersect(element.text_range(), range))
            .all(|element| element.kind().is_trivia())
    };
    (selected.iter().all(is_structural_form) && only_trivia(leading) && only_trivia(trailing))
        .then_some(selected)
}

fn point_form(container: &SyntaxNode, offset: TextSize) -> Option<AstForm> {
    let forms = direct_forms(container);
    for element in container.children_with_tokens() {
        if element.text_range().contains(offset)
            && matches!(
                element.kind(),
                SyntaxKind::LineComment | SyntaxKind::BlockComment
            )
        {
            return None;
        }
    }
    if let Some(form) = forms.iter().find(|form| form.text_range().contains(offset)) {
        if let Element::Node(node) = form.clone().into_element() {
            if Group::cast(node.clone()).is_some() {
                let group = Group::cast(node.clone())?;
                if offset >= group.opener.text_range().end()
                    && group
                        .closer
                        .as_ref()
                        .is_none_or(|closer| offset < closer.text_range().start())
                {
                    return point_form(&node, offset).or(Some(form.clone()));
                }
            } else if node.kind() == SyntaxKind::ReaderForm {
                if let Some(inner) = direct_forms(&node)
                    .into_iter()
                    .find(|inner| inner.text_range().contains(offset))
                {
                    if let Element::Node(inner_node) = inner.clone().into_element()
                        && inner_node.kind() == SyntaxKind::ReaderForm
                    {
                        return point_form(&node, offset).or(Some(form.clone()));
                    }
                    if let Element::Node(inner_node) = inner.clone().into_element()
                        && let Some(group) = Group::cast(inner_node.clone())
                        && offset >= group.opener.text_range().end()
                        && group
                            .closer
                            .as_ref()
                            .is_none_or(|close| offset < close.text_range().start())
                    {
                        return point_form(&inner_node, offset).or(Some(form.clone()));
                    }
                }
            }
        }
        return Some(surface_form(form.clone()));
    }
    forms
        .iter()
        .find(|form| form.text_range().start() >= offset)
        .or_else(|| forms.last())
        .cloned()
        .map(surface_form)
}

/// Climb contiguous reader wrappers only; never cross a list/vector boundary.
fn surface_form(mut form: AstForm) -> AstForm {
    loop {
        let parent = match form.clone().into_element() {
            Element::Node(node) => node.parent(),
            Element::Token(token) => token.parent(),
        };
        let Some(parent) = parent.filter(|p| p.kind() == SyntaxKind::ReaderForm) else {
            return form;
        };
        if direct_forms(&parent)
            .last()
            .is_none_or(|last| last.text_range() != form.text_range())
        {
            return form;
        }
        form = AstForm::cast(Element::Node(parent)).unwrap();
    }
}

fn sibling_form(form: &AstForm, direction: Direction) -> Option<AstForm> {
    let mut next = match form.clone().into_element() {
        Element::Node(node) => match direction {
            Direction::Next => node.next_sibling_or_token(),
            Direction::Prev => node.prev_sibling_or_token(),
        },
        Element::Token(token) => match direction {
            Direction::Next => token.next_sibling_or_token(),
            Direction::Prev => token.prev_sibling_or_token(),
        },
    };
    while let Some(element) = next {
        if let Some(form) = AstForm::cast(element.clone()) {
            return Some(form);
        }
        next = match element {
            Element::Node(node) => match direction {
                Direction::Next => node.next_sibling_or_token(),
                Direction::Prev => node.prev_sibling_or_token(),
            },
            Element::Token(token) => match direction {
                Direction::Next => token.next_sibling_or_token(),
                Direction::Prev => token.prev_sibling_or_token(),
            },
        };
    }
    None
}

fn is_structural_form(form: &AstForm) -> bool {
    matches!(
        form,
        AstForm::List(_)
            | AstForm::Vector(_)
            | AstForm::ReaderForm(_)
            | AstForm::Atom(_)
            | AstForm::Literal(_)
    )
}

fn ranges_intersect(left: TextRange, right: TextRange) -> bool {
    left.start() < right.end() && right.start() < left.end()
}

fn contains_unknown_reader(element: &Element) -> bool {
    match element {
        Element::Node(node) => node.descendants_with_tokens().any(|element| {
            matches!(
                element.kind(),
                SyntaxKind::UnknownDispatch | SyntaxKind::Error
            )
        }),
        Element::Token(token) => matches!(
            token.kind(),
            SyntaxKind::UnknownDispatch | SyntaxKind::Error
        ),
    }
}

fn has_unknown_reader_boundary(form: &AstForm) -> bool {
    if !is_structural_form(form) {
        return true;
    }
    let element = form.clone().into_element();
    contains_unknown_reader(&element)
        || match element {
            Element::Node(node) => {
                edge_has_unknown(node.prev_sibling_or_token(), Direction::Prev)
                    || edge_has_unknown(node.next_sibling_or_token(), Direction::Next)
            }
            Element::Token(token) => {
                edge_has_unknown(token.prev_sibling_or_token(), Direction::Prev)
                    || edge_has_unknown(token.next_sibling_or_token(), Direction::Next)
            }
        }
}

fn edge_has_unknown(mut element: Option<Element>, direction: Direction) -> bool {
    while let Some(current) = element {
        if !current.kind().is_trivia() {
            return matches!(
                current.kind(),
                SyntaxKind::UnknownDispatch | SyntaxKind::Error
            );
        }
        element = match current {
            Element::Node(node) => match direction {
                Direction::Next => node.next_sibling_or_token(),
                Direction::Prev => node.prev_sibling_or_token(),
            },
            Element::Token(token) => match direction {
                Direction::Next => token.next_sibling_or_token(),
                Direction::Prev => token.prev_sibling_or_token(),
            },
        };
    }
    false
}

fn insert(at: TextSize, text: &str) -> TextEdit {
    TextEdit {
        range: TextRange::empty(at),
        replacement: text.to_owned(),
    }
}

fn remove(range: TextRange) -> TextEdit {
    TextEdit {
        range,
        replacement: String::new(),
    }
}

fn normalize_edits(text: &str, mut edits: Vec<TextEdit>) -> Result<Vec<TextEdit>, EditError> {
    edits.retain(|edit| text.get(range_bytes(edit.range)) != Some(edit.replacement.as_str()));
    edits.sort_by_key(|edit| (edit.range.start(), edit.range.end()));
    let mut combined: Vec<TextEdit> = Vec::new();
    for edit in edits {
        if let Some(last) = combined.last_mut()
            && last.range.end() == edit.range.start()
            && last.replacement.is_empty()
            && edit.replacement.is_empty()
        {
            last.range = TextRange::new(last.range.start(), edit.range.end());
        } else {
            combined.push(edit);
        }
    }
    validate_edits(text, &combined)?;
    Ok(combined)
}

fn validate_edits(text: &str, edits: &[TextEdit]) -> Result<(), EditError> {
    let mut previous_end = TextSize::from(0);
    let mut previous_start = None;
    for edit in edits {
        validate_range(text, edit.range)?;
        if edit.range.start() < previous_end || previous_start == Some(edit.range.start()) {
            return Err(EditError::OverlappingEdits);
        }
        previous_start = Some(edit.range.start());
        previous_end = edit.range.end();
    }
    Ok(())
}

fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    for edit in edits {
        let range = range_bytes(edit.range);
        output.push_str(&text[copied..range.start]);
        output.push_str(&edit.replacement);
        copied = range.end;
    }
    output.push_str(&text[copied..]);
    output
}

// Removed delimiters and replacement edges may need a separator. Compare the shared lexer's exact
// token boundaries, including escapes and dispatch syntax; character classes
// alone cannot distinguish #\a, |escaped symbols| or nonterminating #.
fn preserve_boundaries(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    edits: &mut [TextEdit],
) -> Vec<(usize, usize)> {
    let tokens: Vec<_> = root
        .descendants_with_tokens()
        .filter_map(Element::into_token)
        .collect();
    let mut padding = vec![(0, 0); edits.len()];
    for (index, edit) in edits.iter_mut().enumerate() {
        if edit.range.is_empty() {
            continue;
        }
        let left = tokens
            .iter()
            .rev()
            .find(|token| token.text_range().end() <= edit.range.start());
        let right = tokens
            .iter()
            .find(|token| token.text_range().start() >= edit.range.end());
        let table = snapshot.parsed().readtable_at(edit.range.start());
        if !edit.replacement.is_empty() {
            if let Some(left) = left {
                let first = crate::syntax::lexer::lex(&edit.replacement, table).next();
                if let Some(first) = first {
                    let joined = format!("{}{}", left.text(), first.text);
                    let actual: Vec<_> = crate::syntax::lexer::lex(&joined, table).collect();
                    if actual.len() != 2
                        || actual[0].text != left.text()
                        || actual[1].text != first.text
                    {
                        edit.replacement.insert(0, ' ');
                        padding[index].0 += 1;
                    }
                }
            }
            if let Some(right) = right {
                let last = crate::syntax::lexer::lex(&edit.replacement, table).last();
                if let Some(last) = last {
                    let joined = format!("{}{}", last.text, right.text());
                    let actual: Vec<_> = crate::syntax::lexer::lex(&joined, table).collect();
                    if actual.len() != 2
                        || actual[0].text != last.text
                        || actual[1].text != right.text()
                    {
                        edit.replacement.push(' ');
                        padding[index].1 += 1;
                    }
                }
            }
            continue;
        }
        if let (Some(left), Some(right)) = (left, right) {
            if left.kind() == SyntaxKind::Whitespace && right.kind() == SyntaxKind::Whitespace {
                continue;
            }
            let joined = format!("{}{}", left.text(), right.text());
            let lexed: Vec<_> = crate::syntax::lexer::lex(&joined, table).collect();
            if lexed.len() != 2 || lexed[0].text != left.text() || lexed[1].text != right.text() {
                edit.replacement.push(' ');
            }
        }
    }
    padding
}

fn validate_surviving_tokens(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    reparsed: &crate::syntax::ParsedSource,
    edits: &[TextEdit],
    changed_token: Option<&Token>,
) -> Result<(), EditError> {
    let result_root = reparsed.root();
    let mut result_tokens = result_root
        .descendants_with_tokens()
        .filter_map(Element::into_token);
    let mut upcoming = edits.iter().peekable();
    let mut delta = 0i64;
    // An untouched token must remain precisely one token, with its original
    // spelling and kind. This also catches a delimiter swallowed by a comment.
    for token in root
        .descendants_with_tokens()
        .filter_map(Element::into_token)
    {
        if changed_token == Some(&token) || token.kind() == SyntaxKind::Whitespace {
            continue;
        }
        let before = token.text_range();
        while let Some(edit) = upcoming.next_if(|edit| edit.range.end() <= before.start()) {
            delta += i64::try_from(edit.replacement.len()).map_err(|_| EditError::InvalidRange)?
                - i64::from(u32::from(edit.range.len()));
        }
        if let Some(edit) = upcoming
            .peek()
            .filter(|edit| edit.range.start() < before.end())
        {
            if edit.range.is_empty() {
                return Err(EditError::UnsafeTokenBoundary);
            }
            continue;
        }
        let shifted = |at| {
            u32::try_from(i64::from(u32::from(at)) + delta)
                .map(TextSize::from)
                .map_err(|_| EditError::InvalidRange)
        };
        let range = TextRange::new(shifted(before.start())?, shifted(before.end())?);
        if !result_tokens
            .by_ref()
            .find(|token| token.text_range().end() > range.start())
            .is_some_and(|other| {
                other.text_range() == range
                    && other.kind() == token.kind()
                    && other.text() == token.text()
            })
        {
            return Err(EditError::UnsafeTokenBoundary);
        }
    }
    // Partial input elsewhere is allowed, but structural operations may never
    // introduce new malformed consing-dot boundaries.
    use crate::syntax::SyntaxDiagnosticKind::*;
    let dotted = |kind| {
        matches!(
            kind,
            UnexpectedDot
                | InvalidDotToken
                | DottedListWithoutHead
                | MissingDottedTail
                | ExtraDottedTail
        )
    };
    let before = snapshot
        .parsed()
        .diagnostics()
        .iter()
        .filter(|d| dotted(d.kind()))
        .count();
    let after = reparsed
        .diagnostics()
        .iter()
        .filter(|d| dotted(d.kind()))
        .count();
    if after > before {
        return Err(EditError::DottedBoundary);
    }
    for diagnostic in reparsed.diagnostics() {
        let count = |diagnostics: &[crate::syntax::SyntaxDiagnostic]| {
            diagnostics
                .iter()
                .filter(|other| other.kind() == diagnostic.kind())
                .count()
        };
        if count(reparsed.diagnostics()) > count(snapshot.parsed().diagnostics()) {
            return Err(if diagnostic.kind() == MissingReaderOperand {
                EditError::UnknownReaderBoundary
            } else {
                EditError::UnsafeTokenBoundary
            });
        }
    }
    Ok(())
}

// Constrained reader data has a fixed skeleton, with explicit general values
// inside it. Edits in a general value are permitted if it remains one object.
fn general_reader_values(reader: &SyntaxNode, name: &str) -> Vec<AstForm> {
    let Some(reader) = crate::ast::AstReaderForm::cast(reader.clone()) else {
        return vec![];
    };
    let Some(operand) = reader.operands().next() else {
        return vec![];
    };
    if name == "S" {
        return operand
            .as_list()
            .map(|list| list.forms().skip(2).step_by(2).collect())
            .unwrap_or_default();
    }
    if let Some(rank) = name
        .strip_suffix('A')
        .and_then(|rank| rank.parse::<usize>().ok())
    {
        let mut values = vec![operand];
        for _ in 0..rank {
            let mut nested = Vec::new();
            for value in values {
                let Some(list) = value.as_list() else {
                    return vec![];
                };
                nested.extend(list.forms());
            }
            values = nested;
            if values.is_empty() {
                break;
            }
        }
        return values;
    }
    vec![]
}

fn validate_reader_context(
    snapshot: &SourceSnapshot,
    root: &SyntaxNode,
    result: &str,
    parsed: &crate::syntax::ParsedSource,
    edits: &[TextEdit],
    transpose: Option<(TextRange, Direction)>,
) -> Result<(), EditError> {
    let after = parsed.root();
    if invalid_unquotes(&after) > invalid_unquotes(root)
        || invalid_labels(&after) > invalid_labels(root)
    {
        return Err(EditError::UnknownReaderBoundary);
    }
    for reader in root
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::ReaderForm)
    {
        let Some(name) = dispatch(&reader) else {
            continue;
        };
        if !matches!(name.as_str(), "C" | "S")
            && !(name.ends_with('A')
                && name[..name.len() - 1]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit()))
        {
            continue;
        }
        let span = reader.text_range();
        // Changes at the wrapper's outside edges can wrap or move it intact.
        let touched: Vec<_> = edits
            .iter()
            .filter(|edit| {
                if edit.range.is_empty() {
                    span.start() < edit.range.start() && edit.range.start() < span.end()
                } else {
                    ranges_intersect(span, edit.range) && !edit.range.contains_range(span)
                }
            })
            .cloned()
            .collect();
        if touched.is_empty() {
            continue;
        }
        let values = general_reader_values(&reader, &name);
        let mut safe = false;
        if let Some((selection, direction)) = transpose {
            let selected = select_forms(root, selection)?;
            let edge = if direction == Direction::Next {
                selected.last()
            } else {
                selected.first()
            }
            .ok_or(EditError::NoForm)?;
            if let Some(sibling) = sibling_form(edge, direction) {
                safe = selected
                    .iter()
                    .chain(std::iter::once(&sibling))
                    .all(|form| {
                        values
                            .iter()
                            .any(|value| value.text_range() == form.text_range())
                    });
            }
        }
        for value in values {
            let range = value.text_range();
            if touched.iter().any(|edit| !range.contains_range(edit.range)) {
                continue;
            }
            let start = map_offset(range.start(), edits, Affinity::Before)?;
            let end = map_offset(range.end(), edits, Affinity::After)?;
            if start > end {
                continue;
            }
            let Some(text) = result.get(range_bytes(TextRange::new(start, end))) else {
                continue;
            };
            let parsed = crate::syntax::parse_source_with_readtable(
                text,
                snapshot.parsed().readtable_at(range.start()),
            );
            if parsed.diagnostics().is_empty() && parsed.forms().count() == 1 {
                safe = true;
                break;
            }
        }
        if !safe {
            return Err(EditError::UnsupportedSelection);
        }
    }
    Ok(())
}

fn invalid_unquotes(root: &SyntaxNode) -> usize {
    root.descendants()
        .filter(|node| {
            let Some(reader) = crate::ast::AstReaderForm::cast(node.clone()) else {
                return false;
            };
            if !matches!(
                reader.prefix_kind(),
                Some(
                    SyntaxKind::Unquote
                        | SyntaxKind::UnquoteSplicing
                        | SyntaxKind::UnquoteDestructive
                )
            ) {
                return false;
            }
            let mut depth = 0isize;
            for ancestor in node.ancestors() {
                let Some(reader) = crate::ast::AstReaderForm::cast(ancestor) else {
                    continue;
                };
                match reader.prefix_kind() {
                    Some(SyntaxKind::Backquote) => depth += 1,
                    Some(
                        SyntaxKind::Unquote
                        | SyntaxKind::UnquoteSplicing
                        | SyntaxKind::UnquoteDestructive,
                    ) => depth -= 1,
                    _ => {}
                }
            }
            depth < 0
        })
        .count()
}

fn invalid_labels(root: &SyntaxNode) -> usize {
    let mut invalid = 0;
    for form in direct_forms(root) {
        let mut defined = std::collections::HashSet::new();
        let tokens: Vec<_> = match form.into_element() {
            Element::Node(node) => node
                .descendants_with_tokens()
                .filter_map(Element::into_token)
                .collect(),
            Element::Token(token) => vec![token],
        };
        for token in tokens {
            let spelling = token.text();
            let Some(rest) = spelling.strip_prefix('#') else {
                continue;
            };
            let (digits, definition) = if token.kind() == SyntaxKind::DispatchPrefix {
                let Some(digits) = rest.strip_suffix('=') else {
                    continue;
                };
                (digits, true)
            } else if token.kind() == SyntaxKind::Atom {
                let Some(digits) = rest.strip_suffix('#') else {
                    continue;
                };
                (digits, false)
            } else {
                continue;
            };
            if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let number = digits.trim_start_matches('0').to_owned();
            if definition {
                if !defined.insert(number) {
                    invalid += 1;
                }
            } else if !defined.contains(&number) {
                invalid += 1;
            }
        }
    }
    invalid
}

fn transpose_selection(
    root: &SyntaxNode,
    selection: TextRange,
    operation: StructuralOperation,
    edits: &[TextEdit],
    padding: &[(usize, usize)],
) -> Result<TextRange, EditError> {
    let forms = select_forms(root, selection)?;
    let first = forms.first().ok_or(EditError::NoForm)?.text_range().start();
    let last = forms.last().unwrap().text_range().end();
    let edit = edits.first().ok_or(EditError::NoForm)?;
    let moved_start = if operation == StructuralOperation::TransposeForward {
        edit.range.start()
            + TextSize::try_from(edit.replacement.len() - padding[0].1)
                .map_err(|_| EditError::InvalidRange)?
            - (last - first)
    } else {
        let leading = padding[0].0;
        edit.range.start() + TextSize::try_from(leading).map_err(|_| EditError::InvalidRange)?
    };
    let original = if selection.is_empty() {
        TextRange::empty(selection.start().max(first).min(last))
    } else {
        TextRange::new(first, last)
    };
    Ok(TextRange::new(
        moved_start + (original.start() - first),
        moved_start + (original.end() - first),
    ))
}

fn result_selection(
    operation: StructuralOperation,
    selection: TextRange,
    edits: &[TextEdit],
    padding: &[(usize, usize)],
) -> Result<TextRange, EditError> {
    if matches!(operation, StructuralOperation::Raise)
        && let Some((index, edit)) = edits
            .iter()
            .enumerate()
            .find(|(_, edit)| !edit.replacement.is_empty())
    {
        let start = map_offset(edit.range.start(), &edits[..index], Affinity::Before)?;
        let leading = padding[index].0;
        let length = edit.replacement.len() - leading - padding[index].1;
        let start = start + TextSize::try_from(leading).map_err(|_| EditError::InvalidRange)?;
        let inserted = TextSize::try_from(length).map_err(|_| EditError::InvalidRange)?;
        return Ok(TextRange::new(start, start + inserted));
    }
    if selection.is_empty() {
        let at = map_offset(selection.start(), edits, Affinity::After)?;
        return Ok(TextRange::empty(at));
    }
    let start = map_offset(selection.start(), edits, Affinity::After)?;
    let end = map_offset(selection.end(), edits, Affinity::Before)?;
    Ok(TextRange::new(start.min(end), end.max(start)))
}

#[derive(Clone, Copy)]
enum Affinity {
    Before,
    After,
}

fn map_offset(
    offset: TextSize,
    edits: &[TextEdit],
    affinity: Affinity,
) -> Result<TextSize, EditError> {
    let mut delta = 0i64;
    for edit in edits {
        let start = i64::from(u32::from(edit.range.start()));
        let end = i64::from(u32::from(edit.range.end()));
        let at = i64::from(u32::from(offset));
        let inserted =
            i64::try_from(edit.replacement.len()).map_err(|_| EditError::InvalidRange)?;
        if at < start {
            break;
        }
        if at > end || (at == end && start != end) {
            delta += inserted - (end - start);
        } else {
            let mapped = start
                + delta
                + if matches!(affinity, Affinity::After) {
                    inserted
                } else {
                    0
                };
            return u32::try_from(mapped)
                .map(TextSize::from)
                .map_err(|_| EditError::InvalidRange);
        }
    }
    u32::try_from(i64::from(u32::from(offset)) + delta)
        .map(TextSize::from)
        .map_err(|_| EditError::InvalidRange)
}

fn validate_range(text: &str, range: TextRange) -> Result<(), EditError> {
    text.get(range_bytes(range))
        .map(|_| ())
        .ok_or(EditError::InvalidRange)
}

fn range_bytes(range: TextRange) -> std::ops::Range<usize> {
    usize::from(range.start())..usize::from(range.end())
}
