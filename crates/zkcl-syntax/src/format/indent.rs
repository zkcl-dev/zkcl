//! Indent the existing lines without changing their contents or layout.

use super::{
    Element, FormatContext, ListRole, SourceSnapshot, SyntaxKind, TextEdit, TextRange, TextSize,
    child_list_role, classify_list, next_list_indent, observe_list_element,
};
use std::ops::RangeInclusive;

pub(crate) fn indent_edits(source: &SourceSnapshot, range: TextRange) -> Vec<TextEdit> {
    let line_starts = source.source().line_starts().expect("snapshot has text");
    let line = |offset| line_starts.partition_point(|start| *start <= offset) - 1;
    let first = line(usize::from(range.start()));
    let last = line(usize::from(range.end()) - usize::from(!range.is_empty()));
    let blank = range
        .is_empty()
        .then(|| {
            let start = line_starts[first];
            let end = line_starts
                .get(first + 1)
                .copied()
                .unwrap_or(source.text().len());
            let line = source.text()[start..end].trim_end_matches(['\r', '\n']);
            horizontal_whitespace(line).then(|| bytes(start, start + line.len()))
        })
        .flatten();
    let mut indenter = Indenter {
        source,
        line_starts,
        lines: first..=last,
        blank,
        last_position: None,
        edits: Vec::new(),
    };
    let root = source.parsed().root();
    for child in root.children_with_tokens() {
        if indenter.line(child.text_range().start()) > last {
            break;
        }
        indenter.visit(&child, FormatContext::ROOT, 0);
    }
    indenter.edits
}

struct Indenter<'a> {
    source: &'a SourceSnapshot,
    line_starts: &'a [usize],
    lines: RangeInclusive<usize>,
    blank: Option<TextRange>,
    last_position: Option<(usize, TextSize, usize)>,
    edits: Vec<TextEdit>,
}

impl Indenter<'_> {
    fn line(&self, at: TextSize) -> usize {
        self.line_starts
            .partition_point(|start| *start <= usize::from(at))
            - 1
    }

    fn visit(&mut self, element: &Element, ctx: FormatContext, indent: usize) -> (usize, usize) {
        use SyntaxKind::*;
        let range = element.text_range();
        let line = self.line(range.start());
        if line > *self.lines.end() {
            return (line, 0);
        }
        if element.kind() == Whitespace {
            if let Some(blank) = self.blank.filter(|blank| range.contains_range(*blank)) {
                self.blank = None;
                self.change_indent(blank, indent);
            }
            return (self.line(range.start()), 0);
        }
        let (line, column) = self.start_element(range.start(), indent);
        let Some(node) = element.as_node() else {
            return (line, column);
        };
        if usize::from(range.end()) < self.line_starts[*self.lines.start()] {
            return (line, column);
        }
        match node.kind() {
            List => {
                let mut layout = classify_list(node, ctx);
                let mut index = 0;
                for child in node.children_with_tokens() {
                    let kind = child.kind();
                    let child_indent = match kind {
                        OpenParen | CloseParen => column,
                        _ => next_list_indent(column, index, &layout),
                    };
                    let role = child_list_role(ctx.role, index, &layout, &child);
                    let (start_line, child_column) =
                        self.visit(&child, ctx.with_role(role), child_indent);
                    if !kind.is_trivia() && !matches!(kind, OpenParen | CloseParen | Dot) {
                        let end_line = self.line(child.text_range().end());
                        observe_list_element(
                            &mut layout,
                            index,
                            &child,
                            line,
                            start_line,
                            end_line,
                            child_column,
                        );
                        index += 1;
                    }
                }
            }
            Vector => {
                for child in node.children_with_tokens() {
                    let indent = if matches!(child.kind(), VectorStart | CloseParen) {
                        column
                    } else {
                        column + 2
                    };
                    self.visit(&child, ctx.with_role(ListRole::Data), indent);
                }
            }
            ReaderForm => {
                let mut children = node.children_with_tokens();
                let Some(prefix) = children.next() else {
                    return (line, column);
                };
                let mut feature = prefix.kind() == ReaderConditional;
                let operand_ctx = if prefix.kind() == Quote {
                    FormatContext {
                        role: ListRole::Data,
                        quoted: true,
                    }
                } else {
                    ctx.with_role(ListRole::Form)
                };
                for child in children {
                    let child_ctx = if feature && !child.kind().is_trivia() {
                        feature = false;
                        FormatContext {
                            role: ListRole::Data,
                            quoted: false,
                        }
                    } else {
                        operand_ctx
                    };
                    self.visit(&child, child_ctx, column);
                }
            }
            _ => {}
        }
        (line, column)
    }

    fn start_element(&mut self, at: TextSize, indent: usize) -> (usize, usize) {
        let line = self.line(at);
        let start = self.line_starts[line];
        let prefix = bytes(start, usize::from(at));
        let (column, offset) = if let Some((_, previous, column)) =
            self.last_position.filter(|(seen, _, _)| *seen == line)
        {
            (column, previous)
        } else {
            if self.lines.contains(&line)
                && horizontal_whitespace(self.source.text_at(prefix).unwrap())
            {
                self.change_indent(prefix, indent);
            }
            match self.edits.last() {
                Some(edit) if edit.range == prefix => (edit.replacement.len(), at),
                _ => (0, prefix.start()),
            }
        };
        let rest = self.source.text_at(TextRange::new(offset, at)).unwrap();
        let column = super::columns(rest, column);
        self.last_position = Some((line, at, column));
        (line, column)
    }

    fn change_indent(&mut self, range: TextRange, indent: usize) {
        if !self.source.editable_reader_syntax_at(range.end()) {
            return;
        }
        let old = self.source.text_at(range).unwrap();
        if old.len() != indent || old.as_bytes().contains(&b'\t') {
            self.edits.push(TextEdit {
                range,
                replacement: " ".repeat(indent),
            });
        }
    }
}

fn horizontal_whitespace(text: &str) -> bool {
    text.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
}

fn bytes(start: usize, end: usize) -> TextRange {
    TextRange::new(
        TextSize::try_from(start).unwrap(),
        TextSize::try_from(end).unwrap(),
    )
}
