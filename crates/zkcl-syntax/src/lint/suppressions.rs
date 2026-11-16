//! Inline comment and declaration lint suppression directives.

use crate::navigation::index::{closed_list, symbol};
use crate::outline::elements;
use crate::reader_tokens::Prefix;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

use super::LintCategory;

#[derive(Debug, Clone)]
pub(crate) struct Suppression {
    pub(crate) directive_range: TextRange,
    pub(crate) target: SuppressionTarget,
    pub(crate) rules: Vec<String>,
    pub(crate) matched: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SuppressionTarget {
    Line(usize),
    Range(TextRange),
}

#[derive(Debug, Default)]
pub(crate) struct SuppressionTable {
    entries: Vec<Suppression>,
    line_starts: Vec<usize>,
}

impl SuppressionTable {
    pub(crate) fn collect(text: &str, root: &SyntaxNode) -> Self {
        if !may_have_directives(text) {
            return Self::default();
        }
        let line_starts = compute_line_starts(text);
        let mut entries = Vec::new();

        for el in root.descendants_with_tokens() {
            if let Some(tok) = el.as_token() {
                if tok.kind() != SyntaxKind::LineComment {
                    continue;
                }
                let Some(rules) = parse_comment_directive(tok.text()) else {
                    continue;
                };
                let range = tok.text_range();
                let start = usize::from(range.start());
                let line = line_index(&line_starts, start);
                let line_start = line_starts[line];
                let before_on_line = &text[line_start..start];
                let standalone = before_on_line.bytes().all(|b| matches!(b, b' ' | b'\t'));

                let target = if standalone {
                    if let Some(form_range) = next_element_range_after(tok) {
                        SuppressionTarget::Range(form_range)
                    } else {
                        SuppressionTarget::Line(line)
                    }
                } else {
                    SuppressionTarget::Line(line)
                };

                entries.push(Suppression {
                    directive_range: range,
                    target,
                    rules,
                    matched: false,
                });
            } else if let Some(node) = el.as_node() {
                collect_declare_suppression(node, &mut entries);
            }
        }

        Self {
            entries,
            line_starts,
        }
    }

    pub(crate) fn is_suppressed(
        &mut self,
        rule_id: &str,
        short_name: &str,
        category: LintCategory,
        range: TextRange,
    ) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let diag_line = line_index(&self.line_starts, usize::from(range.start()));
        let cat_name = category.as_str();
        for entry in &mut self.entries {
            let matches_target = match entry.target {
                SuppressionTarget::Line(line) => line == diag_line,
                SuppressionTarget::Range(target_range) => target_range.contains_range(range),
            };
            if !matches_target {
                continue;
            }
            if entry.rules.iter().any(|r| {
                r == "all"
                    || r.eq_ignore_ascii_case(rule_id)
                    || r.eq_ignore_ascii_case(short_name)
                    || r.eq_ignore_ascii_case(cat_name)
            }) {
                entry.matched = true;
                return true;
            }
        }
        false
    }

    pub(crate) fn unmatched(&self) -> impl Iterator<Item = &Suppression> {
        self.entries.iter().filter(|entry| !entry.matched)
    }
}

fn may_have_directives(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut colons = memchr::memchr_iter(b':', bytes);
    let Some(first) = colons.next() else {
        return false;
    };
    // Escaped declaration names can still read as ZKCL:ALLOW. Comments,
    // which are not read as Lisp, use the literal markers below.
    if memchr::memchr2(b'\\', b'|', bytes).is_some() {
        return true;
    }
    std::iter::once(first).chain(colons).any(|at| {
        let tail = &bytes[at + 1..];
        tail.get(..5)
            .is_some_and(|name| name.eq_ignore_ascii_case(b"allow"))
            || tail
                .get(..8)
                .is_some_and(|name| name.eq_ignore_ascii_case(b"suppress"))
    })
}

fn collect_declare_suppression(node: &SyntaxNode, out: &mut Vec<Suppression>) {
    if node.kind() != SyntaxKind::List || !closed_list(node) {
        return;
    }
    let mut parts = elements(node);
    let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
        return;
    };
    if !head.name.eq_ignore_ascii_case("DECLARE") {
        return;
    }
    let Some(parent) = node.parent() else {
        return;
    };
    let scope_node = if parent.kind() == SyntaxKind::ReaderForm {
        match parent.parent() {
            Some(gp) if gp.kind() == SyntaxKind::List => gp,
            _ => return,
        }
    } else if parent.kind() == SyntaxKind::List {
        parent
    } else {
        return;
    };
    for spec in parts {
        let Some(spec_node) = spec.as_node().filter(|n| closed_list(n)) else {
            continue;
        };
        let mut spec_parts = elements(spec_node);
        let Some(spec_head) = spec_parts.next().and_then(|e| symbol(&e)) else {
            continue;
        };
        let is_zkcl_allow = matches!(
            &spec_head.prefix,
            Prefix::Package { name, .. }
            if (name.eq_ignore_ascii_case("ZKCL") && matches!(spec_head.name.as_str(), "ALLOW" | "SUPPRESS"))
                || (name.eq_ignore_ascii_case("MALLET") && spec_head.name == "SUPPRESS")
        );
        if !is_zkcl_allow {
            continue;
        }
        let rules: Vec<String> = spec_parts
            .filter_map(|arg| Some(symbol(&arg)?.name.to_ascii_lowercase()))
            .collect();
        if !rules.is_empty() {
            out.push(Suppression {
                directive_range: spec_node.text_range(),
                target: SuppressionTarget::Range(scope_node.text_range()),
                rules,
                matched: false,
            });
        }
    }
}

fn next_element_range_after(tok: &crate::outline::Token) -> Option<TextRange> {
    let mut cur = tok.next_sibling_or_token();
    while let Some(el) = cur {
        match el.kind() {
            SyntaxKind::Whitespace | SyntaxKind::LineComment | SyntaxKind::BlockComment => {
                cur = el.next_sibling_or_token();
            }
            SyntaxKind::CloseParen => return None,
            _ => return Some(el.text_range()),
        }
    }
    None
}

fn parse_comment_directive(comment: &str) -> Option<Vec<String>> {
    let trimmed = comment.trim_start_matches(';').trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let rest = ["zkcl:allow", "zkcl:suppress", "mallet:suppress"]
        .iter()
        .find_map(|prefix| {
            let after = lower.strip_prefix(prefix)?;
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                Some(&trimmed[prefix.len()..])
            } else {
                None
            }
        })?;

    let rules_part = rest.split("--").next().unwrap_or(rest).trim();
    if rules_part.is_empty() {
        return Some(vec!["all".to_owned()]);
    }
    let rules: Vec<String> = rules_part
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|s| s.trim().trim_start_matches(':').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if rules.is_empty() { None } else { Some(rules) }
}

fn compute_line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            if bytes.get(i + 1) == Some(&b'\n') {
                i += 2;
            } else {
                i += 1;
            }
            starts.push(i);
        } else if bytes[i] == b'\n' {
            i += 1;
            starts.push(i);
        } else {
            i += 1;
        }
    }
    starts
}

fn line_index(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(idx) => idx,
        Err(idx) => idx.saturating_sub(1),
    }
}
