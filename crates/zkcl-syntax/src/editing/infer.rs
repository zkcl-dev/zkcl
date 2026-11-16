use super::{
    Affinity, Edit, EditError, Group, Selection, checked_text, ensure_editable, insert, map_offset,
    normalize_edits, remove, validate_range,
};
use crate::ast::{Element, Token};
use crate::format::{TextEdit, columns};
use crate::source_workspace::SourceSnapshot;
use crate::syntax::{SyntaxDiagnosticKind, SyntaxKind, SyntaxNode, TextRange, TextSize};
use std::sync::Arc;

/// Infer only line-ending closers; keep inline and standalone closers fixed.
/// The tree supplies lexical tokens even when the grouping is incomplete.
pub(super) fn infer_parens(
    source: Arc<SourceSnapshot>,
    root: &SyntaxNode,
    selection: Selection,
) -> Result<Edit, EditError> {
    use SyntaxDiagnosticKind::{MissingCloseParen, UnexpectedCloseParen};
    if source
        .diagnostics()
        .iter()
        .any(|diagnostic| !matches!(diagnostic.kind(), MissingCloseParen | UnexpectedCloseParen))
    {
        return Err(EditError::CannotInferParens);
    }
    let end = root.text_range().end();
    if end > TextSize::from(0) && !source.editable_reader_syntax_at(end - TextSize::from(1)) {
        return Err(EditError::UnknownReaderBoundary);
    }

    let lines = source.source().line_starts().expect("snapshot has text");
    let line = |offset: TextSize| lines.partition_point(|start| *start <= usize::from(offset)) - 1;
    let mut state = Inference::default();
    let mut tokens: Vec<Token> = vec![];
    for token in root
        .descendants_with_tokens()
        .filter_map(Element::into_token)
        .filter(|token| !token.kind().is_trivia())
    {
        if tokens
            .last()
            .is_some_and(|last| line(last.text_range().end()) != line(token.text_range().start()))
        {
            state.visit_line(&source, &tokens, lines, line)?;
            tokens.clear();
        }
        tokens.push(token);
    }
    if !tokens.is_empty() {
        state.visit_line(&source, &tokens, lines, line)?;
    }
    state.finish_trail(Some(0))?;

    let changes = normalize_edits(source.text(), state.changes)?;
    ensure_editable(&source, &changes)?;
    if !changes.is_empty() {
        let (result, parsed) = checked_text(&source, root, &changes, None, None)?;
        if !parsed.diagnostics().is_empty() {
            return Err(EditError::CannotInferParens);
        }
        // A fixed-length vector cannot gain or lose elements by inference.
        let after = parsed.root();
        for group in root
            .descendants()
            .filter_map(Group::cast)
            .filter(Group::counted_vector)
        {
            let span = group.opener.text_range();
            let span = TextRange::new(
                map_offset(span.start(), &changes, Affinity::After)?,
                map_offset(span.end(), &changes, Affinity::Before)?,
            );
            let parent = after.covering_element(span).parent().and_then(Group::cast);
            if parent.is_none_or(|new| group.forms.len() != new.forms.len()) {
                return Err(EditError::CannotInferParens);
            }
        }
        let selection = map_selection(selection, &changes)?;
        validate_range(&result, selection.range())?;
        return Ok(Edit {
            snapshot: source,
            edits: changes,
            selection,
        });
    }
    Ok(Edit {
        snapshot: source,
        edits: changes,
        selection,
    })
}

fn map_selection(selection: Selection, changes: &[TextEdit]) -> Result<Selection, EditError> {
    Ok(Selection::new(
        map_offset(selection.anchor, changes, Affinity::Before)?,
        map_offset(selection.head, changes, Affinity::Before)?,
    ))
}

#[derive(Default)]
struct Inference {
    open_columns: Vec<usize>,
    trail: Option<Trail>,
    changes: Vec<TextEdit>,
}

struct Trail {
    content_end: TextSize,
    closes: Vec<TextRange>,
}

impl Inference {
    fn visit_line(
        &mut self,
        source: &SourceSnapshot,
        tokens: &[Token],
        line_starts: &[usize],
        line: impl Fn(TextSize) -> usize,
    ) -> Result<(), EditError> {
        use SyntaxKind::{CloseParen, OpenParen, VectorStart};
        let prefix = |token: &Token| {
            let at = token.text_range().start();
            &source.text()[line_starts[line(at)]..usize::from(at)]
        };
        let first = &tokens[0];
        let leading = prefix(first);
        // A leading closer or a form after a comment on the same line isn't
        // a reliable indentation cue. Keep the previous trail verbatim.
        let indent = (first.kind() != CloseParen
            && leading.bytes().all(|byte| matches!(byte, b' ' | b'\t')))
        .then(|| columns(leading, 0));
        self.finish_trail(indent)?;

        let trail_start = tokens
            .iter()
            .rposition(|token| token.kind() != CloseParen)
            .map(|index| index + 1);
        for token in &tokens[..trail_start.unwrap_or(tokens.len())] {
            match token.kind() {
                OpenParen | VectorStart => self.open_columns.push(columns(prefix(token), 0)),
                CloseParen => {
                    self.open_columns
                        .pop()
                        .ok_or(EditError::CannotInferParens)?;
                }
                _ => {}
            }
        }
        if let Some(index) = trail_start {
            self.trail = Some(Trail {
                content_end: tokens[index - 1].text_range().end(),
                closes: tokens[index..].iter().map(Token::text_range).collect(),
            });
        }
        Ok(())
    }

    fn finish_trail(&mut self, indent: Option<usize>) -> Result<(), EditError> {
        let Some(trail) = self.trail.take() else {
            if indent
                .zip(self.open_columns.last())
                .is_some_and(|(indent, column)| indent <= *column)
            {
                return Err(EditError::CannotInferParens);
            }
            return Ok(());
        };
        let existing = trail.closes.len();
        let wanted = indent.map_or(existing, |indent| {
            self.open_columns
                .iter()
                .rev()
                .take_while(|&&column| indent <= column)
                .count()
        });
        if wanted > self.open_columns.len() {
            return Err(EditError::CannotInferParens);
        }
        self.open_columns.truncate(self.open_columns.len() - wanted);
        if wanted < existing {
            self.changes
                .extend(trail.closes[wanted..].iter().copied().map(remove));
        } else if wanted > existing {
            let end = trail
                .closes
                .last()
                .map_or(trail.content_end, |span| span.end());
            self.changes
                .push(insert(end, &")".repeat(wanted - existing)));
        }
        Ok(())
    }
}
