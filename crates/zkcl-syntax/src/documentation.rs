//! Documentation comments and explicitly marked Markdown examples.
//!
//! This layer examines source text without evaluating Lisp or calling the reader.

use std::ops::Range;
use std::sync::Arc;

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};

use crate::lint::{LintConfig, LintDiagnostic, lint_with_workspace};
use crate::navigation::index::{DocumentIndex, WorkspaceDeclarations, WorkspaceIndex};
use crate::outline::Element;
use crate::syntax::{
    ParsedSource, SourceReadtable, SyntaxKind, TextRange, TextSize, parse_source_with_readtable,
};

/// Markdown attached to a recognized top-level definition.
#[derive(Debug)]
pub struct DocumentationComment {
    /// Range of the definition form.
    pub definition: TextRange,
    /// Range of the comment block, including its markers.
    pub range: TextRange,
    markdown: MappedText,
}

impl DocumentationComment {
    pub fn markdown(&self) -> &str {
        &self.markdown.text
    }

    pub(crate) fn preceding(definition: TextRange, anchor: &Element) -> Option<Self> {
        let mut lines = Vec::new();
        let mut previous = anchor.prev_sibling_or_token();
        while let Some(gap) = previous.and_then(Element::into_token) {
            if gap.kind() != SyntaxKind::Whitespace
                || !gap.text().strip_prefix('\n').is_some_and(horizontal_space)
            {
                break;
            }
            let Some(line) = gap.prev_sibling_or_token().and_then(Element::into_token) else {
                break;
            };
            if line.kind() != SyntaxKind::LineComment || !line.text().starts_with(";;!") {
                break;
            }
            previous = line.prev_sibling_or_token();
            let own_line = line.text_range().start() == 0.into()
                || previous.as_ref().is_some_and(|prefix| {
                    prefix.as_token().is_some_and(|prefix| {
                        if prefix.kind() != SyntaxKind::Whitespace {
                            return false;
                        }
                        if let Some((_, indent)) = prefix.text().rsplit_once('\n') {
                            horizontal_space(indent)
                        } else {
                            prefix.text_range().start() == 0.into()
                                && horizontal_space(prefix.text())
                        }
                    })
                });
            if !own_line {
                break;
            }
            lines.push(line);
        }

        let range = TextRange::new(
            lines.last()?.text_range().start(),
            lines.first()?.text_range().end(),
        );
        let mut markdown = MappedText::default();
        for line in lines.iter().rev() {
            let payload = &line.text()[3..];
            let payload = payload.strip_prefix(' ').unwrap_or(payload);
            let end = line.text_range().end();
            markdown.push(payload, TextRange::new(end - TextSize::of(payload), end));
            markdown.push("\n", TextRange::at(end, 1.into()));
        }
        Some(Self {
            definition,
            range,
            markdown,
        })
    }

    pub(crate) fn collect_tests(
        &self,
        tests: &mut DocumentationTests,
        package: Option<&str>,
        readtable: SourceReadtable,
    ) {
        tests.collect(self.markdown(), Some(&self.markdown), package, readtable);
    }
}

/// An explicitly marked `lisp test` fence, with its initial reading context.
#[derive(Debug)]
pub struct DocumentationExample {
    pub range: TextRange,
    pub package: Option<String>,
    pub readtable: SourceReadtable,
    source: MappedText,
    parsed: ParsedSource,
}

impl DocumentationExample {
    pub fn source(&self) -> &str {
        &self.source.text
    }

    pub(crate) fn parsed(&self) -> &ParsedSource {
        &self.parsed
    }

    /// Map a half-open range in the extracted Lisp back to its original UTF-8 bytes.
    pub fn source_range(&self, range: Range<usize>) -> Option<TextRange> {
        if range.start > range.end
            || !self.source().is_char_boundary(range.start)
            || !self.source().is_char_boundary(range.end)
        {
            return None;
        }
        Some(if self.source.spans.is_empty() {
            TextRange::empty(self.range.start())
        } else {
            self.source.origin_range(range)
        })
    }

    fn original_range(&self, range: TextRange) -> TextRange {
        self.source_range(range.into())
            .expect("example diagnostic ranges belong to the extracted source")
    }

    fn lint(
        &self,
        features: Option<&[String]>,
        workspace: &WorkspaceIndex<'_>,
        config: &LintConfig,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Vec<LintDiagnostic>> {
        if cancelled() {
            return None;
        }
        let root = self.parsed.root();
        let index = DocumentIndex::collect(
            &root,
            self.package.as_deref(),
            features,
            self.parsed.initial_readtable(),
        );
        // A fence can define names for its own forms. Keep those declarations
        // out of the project index and out of the other examples.
        let example_document = index.has_workspace_entries().then(|| {
            let last = workspace
                .documents
                .iter()
                .map(|&(name, _)| name)
                .max()
                .unwrap_or_default();
            // A nonempty suffix to the largest key cannot collide with a document.
            format!("{last} (documentation example)")
        });
        let extended = if let Some(name) = example_document.as_deref() {
            let documents: Vec<_> = workspace
                .documents
                .iter()
                .copied()
                .chain([(name, &index)])
                .collect();
            let declarations = Arc::new(WorkspaceDeclarations::collect(&documents, cancelled)?);
            Some(WorkspaceIndex::new(documents).with_declarations(declarations))
        } else {
            None
        };
        let mut diagnostics = lint_with_workspace(
            self.source(),
            &index,
            &root,
            extended.as_ref().unwrap_or(workspace),
            config,
            cancelled,
        )?;
        for diagnostic in &mut diagnostics {
            diagnostic.range = self.original_range(diagnostic.range);
            for related in &mut diagnostic.related {
                if related.document.is_none() || related.document == example_document {
                    related.document = None;
                    related.range = self.original_range(related.range);
                }
            }
            // A source fix can cross stripped comment or Markdown prefixes.
            diagnostic.fix = None;
        }
        Some(diagnostics)
    }
}

#[derive(Debug)]
pub struct DocumentationDiagnostic {
    pub range: TextRange,
    pub message: &'static str,
}

impl DocumentationDiagnostic {
    pub fn to_diagnostic(
        &self,
        source: &zkcl_diagnostics::SourceDocument,
    ) -> zkcl_diagnostics::Diagnostic {
        use zkcl_diagnostics::{Diagnostic, LabelRole, Severity};
        let mut diagnostic = Diagnostic::new(Severity::Error, self.message);
        diagnostic.code = Some("doc/example".into());
        diagnostic
            .add_label(
                source,
                usize::from(self.range.start())..usize::from(self.range.end()),
                LabelRole::Primary,
                "",
            )
            .expect("documentation range belongs to analyzed source");
        diagnostic
    }
}

#[derive(Debug, Default)]
pub struct DocumentationTests {
    pub examples: Vec<DocumentationExample>,
    pub diagnostics: Vec<DocumentationDiagnostic>,
}

impl DocumentationTests {
    /// Find and check the top-level `lisp test` fences in a Markdown document.
    /// The caller supplies the initial readtable. Examples start in CL-USER.
    pub fn markdown(text: &str, readtable: SourceReadtable) -> Self {
        let mut tests = Self::default();
        if TextSize::try_from(text.len()).is_ok() {
            tests.collect(text, None, Some("COMMON-LISP-USER"), readtable);
        } else {
            tests.diagnostics.push(DocumentationDiagnostic {
                range: TextRange::empty(0.into()),
                message: "documentation exceeds the source byte offset limit",
            });
        }
        tests
    }

    pub(crate) fn lint(
        &self,
        features: Option<&[String]>,
        workspace: &WorkspaceIndex<'_>,
        config: &LintConfig,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Vec<LintDiagnostic>> {
        let mut diagnostics = Vec::new();
        for example in &self.examples {
            diagnostics.extend(example.lint(features, workspace, config, cancelled)?);
        }
        Some(diagnostics)
    }

    fn collect(
        &mut self,
        markdown: &str,
        mapped: Option<&MappedText>,
        package: Option<&str>,
        readtable: SourceReadtable,
    ) {
        if !markdown.contains("test") {
            return;
        }
        let original = |range: Range<usize>| match mapped {
            Some(mapped) => mapped.origin_range(range),
            None => byte_range(range),
        };
        let mut depth = 0;
        let mut fence = None;
        for (event, range) in Parser::new(markdown).into_offset_iter() {
            match event {
                Event::Start(tag) => {
                    if depth == 0
                        && matches!(tag, Tag::CodeBlock(CodeBlockKind::Fenced(_)))
                        && let Some((marker, count, header)) =
                            test_fence_header(markdown, range.start)
                    {
                        fence = Some(Fence {
                            marker,
                            count,
                            content_end: header.end,
                            header,
                            source: MappedText::default(),
                        });
                    }
                    depth += 1;
                }
                Event::End(tag) => {
                    depth -= 1;
                    if tag != TagEnd::CodeBlock || depth != 0 {
                        continue;
                    }
                    let Some(fence) = fence.take() else {
                        continue;
                    };
                    let closing = markdown[fence.content_end..range.end].trim_ascii();
                    if closing.len() < fence.count
                        || !closing.bytes().all(|byte| byte == fence.marker)
                    {
                        self.diagnostics.push(DocumentationDiagnostic {
                            range: original(fence.header),
                            message: "unclosed lisp test fence",
                        });
                        continue;
                    }
                    let parsed = parse_source_with_readtable(&fence.source.text, readtable);
                    let example = DocumentationExample {
                        range: original(range),
                        package: package.map(str::to_owned),
                        readtable,
                        source: fence.source,
                        parsed,
                    };
                    if example.parsed.diagnostics().is_empty() {
                        self.examples.push(example);
                    } else {
                        self.diagnostics
                            .extend(example.parsed.diagnostics().iter().map(|diagnostic| {
                                DocumentationDiagnostic {
                                    range: example.original_range(diagnostic.range()),
                                    message: diagnostic.message(),
                                }
                            }));
                    }
                }
                Event::Text(text) => {
                    if let Some(fence) = &mut fence {
                        fence.content_end = range.end;
                        if let Some(mapped) = mapped {
                            fence.source.push_from(mapped, &text, range);
                        } else {
                            fence.source.push(&text, byte_range(range));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

struct Fence {
    marker: u8,
    count: usize,
    header: Range<usize>,
    content_end: usize,
    source: MappedText,
}

fn horizontal_space(text: &str) -> bool {
    text.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
}

fn test_fence_header(markdown: &str, start: usize) -> Option<(u8, usize, Range<usize>)> {
    let line = markdown[start..].split(['\n', '\r']).next()?;
    let marker = *line.as_bytes().first()?;
    let count = line.bytes().take_while(|&byte| byte == marker).count();
    line[count..]
        .split_ascii_whitespace()
        .eq(["lisp", "test"])
        .then_some((marker, count, start..start + line.len()))
}

fn byte_range(range: Range<usize>) -> TextRange {
    TextRange::new((range.start as u32).into(), (range.end as u32).into())
}

#[derive(Debug, Default)]
struct MappedText {
    text: String,
    spans: Vec<Span>,
}

#[derive(Debug)]
struct Span {
    range: Range<usize>,
    original: TextRange,
}

impl MappedText {
    fn push(&mut self, text: &str, original: TextRange) {
        if !text.is_empty() {
            let start = self.text.len();
            self.text.push_str(text);
            self.spans.push(Span {
                range: start..self.text.len(),
                original,
            });
        }
    }

    fn push_from(&mut self, input: &Self, text: &str, range: Range<usize>) {
        if range.is_empty() {
            // CommonMark can synthesize indentation from part of a tab.
            self.push(text, input.origin_range(range));
            return;
        }
        let first = input
            .spans
            .partition_point(|span| span.range.end <= range.start);
        for span in &input.spans[first..] {
            if span.range.start >= range.end {
                break;
            }
            let start = span.range.start.max(range.start);
            let end = span.range.end.min(range.end);
            self.push(
                &text[start - range.start..end - range.start],
                TextRange::new(span.origin(start), span.origin(end)),
            );
        }
    }

    fn origin_range(&self, range: Range<usize>) -> TextRange {
        let first = self
            .spans
            .partition_point(|span| span.range.end <= range.start);
        let start = self.spans.get(first).map_or_else(
            || {
                self.spans
                    .last()
                    .map_or(0.into(), |span| span.original.end())
            },
            |span| span.origin(range.start),
        );
        let end = if range.is_empty() {
            start
        } else {
            let last = self
                .spans
                .partition_point(|span| span.range.start < range.end);
            self.spans[last - 1].origin(range.end)
        };
        TextRange::new(start, end)
    }
}

impl Span {
    fn origin(&self, offset: usize) -> TextSize {
        let delta = (offset - self.range.start).min(self.original.len().into());
        self.original.start() + TextSize::from(delta as u32)
    }
}
