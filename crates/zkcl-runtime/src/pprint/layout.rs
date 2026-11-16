//! Resolve logical blocks, tabs, indentation, and conditional line breaks.
use super::{IndentKind, LayoutItem, LayoutToken, NewlineKind, TabKind};

pub(crate) fn tab_padding(
    kind: TabKind,
    colnum: usize,
    colinc: usize,
    column: usize,
    section_start: usize,
) -> usize {
    let relative = matches!(kind, TabKind::LineRelative | TabKind::SectionRelative);
    let from_section = matches!(kind, TabKind::Section | TabKind::SectionRelative);
    let origin = if from_section { section_start } else { 0 };
    let position = column.saturating_sub(origin);
    if relative {
        let mut padding = colnum;
        if colinc > 1 {
            let target = position.saturating_add(colnum);
            let remainder = target % colinc;
            if remainder != 0 {
                padding = padding.saturating_add(colinc - remainder);
            }
        }
        padding
    } else if position < colnum {
        colnum - position
    } else if colinc == 0 {
        0
    } else {
        let remainder = (position - colnum) % colinc;
        if remainder == 0 {
            colinc
        } else {
            colinc - remainder
        }
    }
}

/// Positions used to decide whether printing a logical block flat is possible.
struct LayoutSections<'a> {
    items: &'a [LayoutItem],
    block_ends: Vec<Option<usize>>,
    /// Number of literal or mandatory newlines before each position.
    forced_breaks: Vec<usize>,
}

impl<'a> LayoutSections<'a> {
    fn new(items: &'a [LayoutItem]) -> Self {
        let mut block_ends = vec![None; items.len()];
        let mut open = Vec::new();
        let mut forced_breaks = Vec::with_capacity(items.len() + 1);
        let mut breaks = 0;
        for (index, item) in items.iter().enumerate() {
            forced_breaks.push(breaks);
            match item.token {
                LayoutToken::BlockStart(_) => open.push(index),
                LayoutToken::BlockEnd => {
                    if let Some(start) = open.pop() {
                        block_ends[start] = Some(index);
                    }
                }
                LayoutToken::Newline(NewlineKind::Mandatory | NewlineKind::Literal) => breaks += 1,
                _ => {}
            }
        }
        forced_breaks.push(breaks);
        Self {
            items,
            block_ends,
            forced_breaks,
        }
    }

    fn block_fits(&self, start: usize, column: usize, margin: usize) -> bool {
        self.block_ends[start].is_some_and(|end| {
            self.forced_breaks[end] == self.forced_breaks[start]
                && column.saturating_add(flat_width(&self.items[start + 1..end], column)) <= margin
        })
    }

    fn following_section_fits(&self, newline: usize, column: usize, margin: usize) -> bool {
        let depth = self.items[newline].depth;
        let tail = &self.items[newline + 1..];
        let end = tail
            .iter()
            .position(|item| item.depth <= depth && matches!(item.token, LayoutToken::Newline(_)))
            .unwrap_or(tail.len());
        column.saturating_add(flat_width(&tail[..end], column)) <= margin
    }
}

fn flat_width(items: &[LayoutItem], start_column: usize) -> usize {
    let mut width = 0;
    let mut section_column = start_column;
    for item in items {
        match &item.token {
            LayoutToken::Text(text) => width += text.chars().count(),
            LayoutToken::Newline(_) | LayoutToken::BlockStart(_) => {
                section_column = start_column + width
            }
            LayoutToken::Tab(kind, colnum, colinc) => {
                width += tab_padding(
                    *kind,
                    *colnum,
                    *colinc,
                    start_column + width,
                    section_column,
                );
            }
            LayoutToken::Indent(..) | LayoutToken::BlockEnd => {}
        }
    }
    width
}

struct LogicalBlockLayout {
    start_column: usize,
    prefix_end_column: usize,
    per_line_prefix: String,
    indent_column: usize,
    section_column: usize,
    fits_on_line: bool,
    miser: bool,
    section_line: usize,
}

impl LogicalBlockLayout {
    fn new(
        parent: Option<&Self>,
        prefix: Option<&str>,
        line: &LayoutOutput,
        fits_on_line: bool,
        miser: bool,
    ) -> Self {
        let start_column = line.column;
        let (mut prefix_end_column, mut per_line_prefix) = parent.map_or_else(
            || (0, String::new()),
            |block| (block.prefix_end_column, block.per_line_prefix.clone()),
        );
        if let Some(prefix) = prefix {
            prefix_end_column = prefix_end_column.max(start_column);
            let padding = prefix_end_column.saturating_sub(per_line_prefix.chars().count());
            per_line_prefix.extend(std::iter::repeat_n(' ', padding));
            let keep = prefix_end_column.saturating_sub(prefix.chars().count());
            if let Some((byte, _)) = per_line_prefix.char_indices().nth(keep) {
                per_line_prefix.truncate(byte);
            }
            per_line_prefix.push_str(prefix);
            prefix_end_column = prefix_end_column.max(per_line_prefix.chars().count());
        }
        Self {
            start_column,
            prefix_end_column,
            per_line_prefix,
            indent_column: start_column.max(prefix_end_column),
            section_column: start_column,
            fits_on_line,
            miser,
            section_line: line.number,
        }
    }

    fn indent(&mut self, kind: IndentKind, amount: i64, column: usize) {
        if self.miser {
            return;
        }
        let origin = match kind {
            IndentKind::Block => self.start_column,
            IndentKind::Current => column,
        };
        self.indent_column = if amount == 0 && kind == IndentKind::Block {
            self.start_column.max(self.prefix_end_column)
        } else {
            (origin as i64 + amount)
                .max(self.prefix_end_column as i64)
                .max(0) as usize
        };
    }

    fn break_line(
        &self,
        kind: NewlineKind,
        line_number: usize,
        following_section_fits: impl FnOnce() -> bool,
    ) -> bool {
        !self.fits_on_line
            && match kind {
                NewlineKind::Mandatory | NewlineKind::Literal | NewlineKind::Linear => true,
                NewlineKind::Miser => self.miser,
                NewlineKind::Fill => {
                    self.miser || line_number > self.section_line || !following_section_fits()
                }
            }
    }
}

#[derive(Default)]
struct LayoutOutput {
    text: String,
    column: usize,
    line_start: usize,
    number: usize,
}

impl LayoutOutput {
    fn push(&mut self, text: &str) {
        self.text.push_str(text);
        self.column += text.chars().count();
    }

    fn spaces(&mut self, count: usize) {
        self.text.extend(std::iter::repeat_n(' ', count));
        self.column += count;
    }

    fn newline(&mut self, literal: bool) {
        if !literal {
            while self.text.len() > self.line_start && self.text.ends_with([' ', '\t']) {
                self.text.pop();
            }
        }
        self.text.push('\n');
        self.line_start = self.text.len();
        self.column = 0;
        self.number += 1;
    }

    fn break_in_block(&mut self, block: &mut LogicalBlockLayout, kind: NewlineKind) {
        let literal = kind == NewlineKind::Literal;
        self.newline(literal);
        self.push(&block.per_line_prefix);
        if !literal {
            let target = block.indent_column.max(block.prefix_end_column);
            self.spaces(target.saturating_sub(self.column));
            block.section_line = self.number;
        }
    }
}

/// Without a right margin, blocks fit unless they contain a forced newline.
pub(super) fn render_layout(
    items: &[LayoutItem],
    column: usize,
    margin: Option<usize>,
    miser: Option<usize>,
) -> String {
    let margin = margin.unwrap_or(usize::MAX);
    let sections = LayoutSections::new(items);
    let mut blocks = Vec::<LogicalBlockLayout>::new();
    let mut output = LayoutOutput {
        column,
        ..LayoutOutput::default()
    };
    for (index, item) in items.iter().enumerate() {
        match &item.token {
            LayoutToken::Text(text) => output.push(text),
            LayoutToken::BlockStart(prefix) => {
                let fits = sections.block_fits(index, output.column, margin);
                let miser = miser.is_some_and(|width| {
                    margin != usize::MAX && margin.saturating_sub(output.column) <= width
                });
                blocks.push(LogicalBlockLayout::new(
                    blocks.last(),
                    prefix.as_deref(),
                    &output,
                    fits,
                    miser,
                ));
            }
            LayoutToken::BlockEnd => {
                blocks.pop();
            }
            LayoutToken::Indent(kind, amount) => {
                if let Some(block) = blocks.last_mut() {
                    block.indent(*kind, *amount, output.column);
                }
            }
            LayoutToken::Tab(kind, colnum, colinc) => {
                let section = blocks.last().map_or(0, |block| block.section_column);
                output.spaces(tab_padding(*kind, *colnum, *colinc, output.column, section));
            }
            LayoutToken::Newline(kind) => {
                if let Some(block) = blocks.last_mut() {
                    if block.break_line(*kind, output.number, || {
                        sections.following_section_fits(index, output.column, margin)
                    }) {
                        output.break_in_block(block, *kind);
                    }
                    block.section_column = output.column;
                } else if *kind == NewlineKind::Literal {
                    output.newline(true);
                }
            }
        }
    }
    output.text
}
