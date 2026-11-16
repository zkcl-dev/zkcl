//! Lossless Common Lisp source formatting over the concrete syntax tree.
//!
//! Formatting normalizes only horizontal whitespace, indentation, and blank
//! line runs whose token boundaries under the standard readtable are proven
//! invariant. All non-whitespace tokens and comments retain their exact source
//! spelling and relative order.

use crate::outline::{Element, elements, unknown_boundary};
use crate::reader_tokens::SymbolName;
use crate::source_workspace::{SourceSnapshot, changed_ranges};
use crate::syntax::lexer::{LexedToken, lex};
use crate::syntax::{
    ParsedSource, SourceReadtable, SyntaxKind, SyntaxNode, TextRange, TextSize, parse_source,
    parse_source_with_readtable,
};

mod indent;
pub(crate) use indent::indent_edits;

pub(crate) fn columns(text: &str, start: usize) -> usize {
    text.chars().fold(start, |column, ch| match ch {
        '\t' => column + (8 - column % 8),
        '\r' => column,
        _ => column + 1,
    })
}

/// A UTF-8 byte range replacement relative to a [`SourceSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub range: TextRange,
    pub replacement: String,
}

/// Format Common Lisp source text under the standard readtable.
///
/// Returns `None` if `text` contains any syntax diagnostics. If a top-level
/// form contains an unknown reader dispatch or mutates the active readtable,
/// formatting stops before that form and preserves the affected region
/// verbatim.
pub fn format_source(text: &str) -> Option<String> {
    format_parsed(text, &parse_source(text))
}

pub(crate) fn format_parsed(text: &str, parsed: &ParsedSource) -> Option<String> {
    if !parsed.diagnostics().is_empty() {
        return None;
    }
    let root = parsed.root();

    let mut last_safe_end = None;
    let mut blocked = false;
    for form in elements(&root) {
        if !parsed.has_readtable_selection(form.text_range()) && blocks_formatting(&form) {
            blocked = true;
            break;
        }
        last_safe_end = Some(usize::from(form.text_range().end()));
    }

    if blocked && last_safe_end.is_none() {
        return Some(text.to_owned());
    }

    let formatted = if let Some(safe_end) = last_safe_end.filter(|_| blocked) {
        let mut emitter = Emitter::new(safe_end);
        format_root_until(&root, Some(safe_end), &mut emitter);
        emitter.out.push_str(&text[safe_end..]);
        emitter.out
    } else {
        let mut emitter = Emitter::new(text.len());
        format_root_until(&root, None, &mut emitter);
        emitter.out
    };

    // The parser's structural state depends only on non-whitespace tokens.
    // Verify their exact boundaries and spelling before accepting the output.
    (formatted == text
        || (TextSize::try_from(formatted.len()).is_ok()
            && if parsed.uses_readtable_selections() {
                let reparsed = parse_source_with_readtable(&formatted, parsed.initial_readtable());
                reparsed.diagnostics().is_empty() && same_syntax_tokens(&root, &reparsed.root())
            } else {
                same_non_whitespace_tokens(text, &formatted)
            }))
    .then_some(formatted)
}

fn same_syntax_tokens(before: &SyntaxNode, after: &SyntaxNode) -> bool {
    let tokens = |root: &SyntaxNode| {
        root.descendants_with_tokens()
            .filter_map(Element::into_token)
            .filter(|token| token.kind() != SyntaxKind::Whitespace)
    };
    let mut after = tokens(after);
    for token in tokens(before) {
        if !after
            .next()
            .is_some_and(|other| token.kind() == other.kind() && token.text() == other.text())
        {
            return false;
        }
    }
    after.next().is_none()
}

/// Compute minimal text edits to format `snapshot`.
///
/// Returns an empty vector if `snapshot` has syntax diagnostics or is already
/// formatted.
pub fn format_edits(snapshot: &SourceSnapshot) -> Vec<TextEdit> {
    let original = snapshot.text();
    let Some(formatted) = snapshot.formatted_text() else {
        return Vec::new();
    };
    if formatted == original {
        return Vec::new();
    }
    vec![minimal_edit(original, &formatted)]
}

fn minimal_edit(original: &str, formatted: &str) -> TextEdit {
    let (replaced, replacement) = changed_ranges(original, formatted);
    TextEdit {
        range: TextRange::new(
            TextSize::try_from(replaced.start).expect("offset fits in TextSize"),
            TextSize::try_from(replaced.end).expect("offset fits in TextSize"),
        ),
        replacement: formatted[replacement].to_owned(),
    }
}

fn blocks_formatting(form: &Element) -> bool {
    unknown_boundary(form) || has_readtable_mutation(form)
}

fn has_readtable_mutation(element: &Element) -> bool {
    let Some(node) = element.as_node() else {
        return false;
    };
    match node.kind() {
        SyntaxKind::ReaderForm => {
            if node
                .first_token()
                .is_some_and(|tok| tok.kind() == SyntaxKind::Quote)
            {
                return false;
            }
            elements(node)
                .skip(1)
                .any(|child| has_readtable_mutation(&child))
        }
        SyntaxKind::List => {
            let mut parts = elements(node);
            let Some(head) = parts.next() else {
                return false;
            };
            if let Some(sym) = element_symbol(&head) {
                if sym.standard_operator() == Some("QUOTE") {
                    return false;
                }
                if crate::outline::is_readtable_mutating_operator(&sym.name, true) {
                    return true;
                }
                if let Some("SETQ" | "SETF" | "PSETQ" | "PSETF" | "SHIFTF" | "ROTATEF") =
                    sym.standard_operator()
                {
                    let all_places = matches!(sym.standard_operator(), Some("SHIFTF" | "ROTATEF"));
                    for (idx, arg) in parts.enumerate() {
                        let is_place = all_places || idx.is_multiple_of(2);
                        if is_place && crate::outline::is_readtable_place(&arg) {
                            return true;
                        }
                        if has_readtable_mutation(&arg) {
                            return true;
                        }
                    }
                    return false;
                }
            }
            has_readtable_mutation(&head) || parts.any(|child| has_readtable_mutation(&child))
        }
        SyntaxKind::Vector | SyntaxKind::SourceFile => {
            elements(node).any(|child| has_readtable_mutation(&child))
        }
        _ => false,
    }
}

fn element_symbol(element: &Element) -> Option<SymbolName> {
    let token = element.as_token()?;
    (token.kind() == SyntaxKind::Atom).then(|| SymbolName::parse(token.text()))?
}

struct Emitter {
    out: String,
    line: usize,
    col: usize,
    pending_indent: Option<usize>,
    raw_string_at_line_end: bool,
}

impl Emitter {
    fn new(capacity: usize) -> Self {
        Self {
            out: String::with_capacity(capacity),
            line: 0,
            col: 0,
            pending_indent: None,
            raw_string_at_line_end: false,
        }
    }

    fn current_col(&self) -> usize {
        self.pending_indent.unwrap_or(self.col)
    }

    fn current_line(&self) -> usize {
        self.line
    }

    fn emit_space(&mut self) {
        if self.pending_indent.is_none() {
            self.out.push(' ');
            self.col += 1;
        }
    }

    fn emit_newline(&mut self, indent: usize) {
        self.out.push('\n');
        self.line += 1;
        self.col = 0;
        self.pending_indent = Some(indent);
        self.raw_string_at_line_end = false;
    }

    fn emit_blank_line(&mut self, indent: usize) {
        self.out.push_str("\n\n");
        self.line += 2;
        self.col = 0;
        self.pending_indent = Some(indent);
        self.raw_string_at_line_end = false;
    }

    fn emit_token(&mut self, text: &str) {
        if let Some(indent) = self.pending_indent.take() {
            for _ in 0..indent {
                self.out.push(' ');
            }
            self.col = indent;
        }
        for ch in text.chars() {
            if ch == '\n' {
                self.line += 1;
                self.col = 0;
            } else if ch != '\r' {
                self.col += 1;
            }
        }
        self.out.push_str(text);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListRole {
    Form,
    Data,
    LocalFunctionList,
    LocalFunction,
    BindingList,
    BindingPair,
    Clause { special_args: usize },
}

#[derive(Debug, Clone, Copy)]
struct FormatContext {
    role: ListRole,
    quoted: bool,
}

impl FormatContext {
    const ROOT: Self = Self {
        role: ListRole::Form,
        quoted: false,
    };

    fn with_role(self, role: ListRole) -> Self {
        Self {
            role: if self.quoted { ListRole::Data } else { role },
            quoted: self.quoted,
        }
    }
}

fn format_root_until(root: &SyntaxNode, stop_at_end: Option<usize>, emitter: &mut Emitter) {
    let mut prev_kind: Option<SyntaxKind> = None;
    let mut ws_newlines = 0usize;

    for child in root.children_with_tokens() {
        let range = child.text_range();
        if let Some(limit) = stop_at_end
            && usize::from(range.start()) >= limit
        {
            break;
        }
        let kind = child.kind();
        if kind == SyntaxKind::Whitespace {
            if let Some(tok) = child.as_token() {
                ws_newlines += count_line_breaks(tok.text());
            }
            continue;
        }

        if let Some(prev) = prev_kind {
            if prev == SyntaxKind::LineComment {
                if ws_newlines >= 2 {
                    emitter.emit_blank_line(0);
                } else {
                    emitter.emit_newline(0);
                }
            } else if ws_newlines >= 2 {
                emitter.emit_blank_line(0);
            } else if ws_newlines == 1 {
                emitter.emit_newline(0);
            } else if kind == SyntaxKind::LineComment
                || kind == SyntaxKind::BlockComment
                || prev == SyntaxKind::BlockComment
            {
                emitter.emit_space();
            } else {
                emitter.emit_newline(0);
            }
        }

        format_element(&child, FormatContext::ROOT, emitter);
        prev_kind = Some(kind);
        ws_newlines = 0;

        if stop_at_end == Some(usize::from(range.end())) {
            return;
        }
    }

    if stop_at_end.is_none() && prev_kind.is_some() && ws_newlines > 0 {
        emitter.out.push('\n');
        emitter.line += 1;
        emitter.col = 0;
        emitter.pending_indent = None;
    }
}

fn format_element(element: &Element, ctx: FormatContext, emitter: &mut Emitter) {
    match element {
        Element::Token(token) => {
            emitter.emit_token(token.text());
            emitter.raw_string_at_line_end = token.kind() == SyntaxKind::RawString;
        }
        Element::Node(node) => match node.kind() {
            SyntaxKind::List => format_list(node, ctx, emitter),
            SyntaxKind::Vector => format_vector(node, ctx, emitter),
            SyntaxKind::ReaderForm => format_reader_form(node, ctx, emitter),
            _ => {
                for child in node.children_with_tokens() {
                    format_element(&child, ctx, emitter);
                }
            }
        },
    }
}

fn format_reader_form(node: &SyntaxNode, ctx: FormatContext, emitter: &mut Emitter) {
    let opener_col = emitter.current_col();
    let mut children = node.children_with_tokens();
    let Some(prefix_el) = children.next() else {
        return;
    };
    let prefix_kind = prefix_el.kind();
    format_element(&prefix_el, ctx, emitter);

    let child_ctx = if prefix_kind == SyntaxKind::Quote {
        FormatContext {
            role: ListRole::Data,
            quoted: true,
        }
    } else {
        ctx.with_role(ListRole::Form)
    };

    let is_conditional = prefix_kind == SyntaxKind::ReaderConditional;
    let mut operands_seen = 0usize;
    let mut prev_kind = prefix_kind;
    let mut ws_newlines = 0usize;

    for child in children {
        let kind = child.kind();
        if kind == SyntaxKind::Whitespace {
            if let Some(tok) = child.as_token() {
                ws_newlines += count_line_breaks(tok.text());
            }
            continue;
        }

        let is_comment = matches!(kind, SyntaxKind::LineComment | SyntaxKind::BlockComment);
        if prev_kind == prefix_kind {
            if is_comment {
                if ws_newlines > 0 {
                    emitter.emit_newline(opener_col);
                } else {
                    emitter.emit_space();
                }
            } else if prefix_kind == SyntaxKind::Unquote && starts_with_unquote_modifier(&child) {
                emitter.emit_space();
            }
        } else if prev_kind == SyntaxKind::LineComment {
            if ws_newlines >= 2 {
                emitter.emit_blank_line(opener_col);
            } else {
                emitter.emit_newline(opener_col);
            }
        } else if ws_newlines >= 2 {
            emitter.emit_blank_line(opener_col);
        } else if ws_newlines == 1 {
            emitter.emit_newline(opener_col);
        } else if is_conditional || is_comment || prev_kind == SyntaxKind::BlockComment {
            emitter.emit_space();
        }

        let operand_ctx = if is_conditional && operands_seen == 0 && !is_comment {
            FormatContext {
                role: ListRole::Data,
                quoted: false,
            }
        } else {
            child_ctx
        };
        format_element(&child, operand_ctx, emitter);
        if !is_comment {
            operands_seen += 1;
        }
        prev_kind = kind;
        ws_newlines = 0;
    }
}

fn starts_with_unquote_modifier(element: &Element) -> bool {
    let first_byte = match element {
        Element::Token(tok) => tok.text().as_bytes().first().copied(),
        Element::Node(node) => node
            .first_token()
            .and_then(|tok| tok.text().as_bytes().first().copied()),
    };
    matches!(first_byte, Some(b'@' | b'.'))
}

fn format_vector(node: &SyntaxNode, ctx: FormatContext, emitter: &mut Emitter) {
    let opener_col = emitter.current_col();
    let elem_indent = opener_col + 2;
    let mut prev_kind: Option<SyntaxKind> = None;
    let mut ws_newlines = 0usize;

    for child in node.children_with_tokens() {
        let kind = child.kind();
        if kind == SyntaxKind::Whitespace {
            if let Some(tok) = child.as_token() {
                ws_newlines += count_line_breaks(tok.text());
            }
            continue;
        }

        if kind == SyntaxKind::VectorStart {
            format_element(&child, ctx, emitter);
            prev_kind = Some(kind);
            ws_newlines = 0;
            continue;
        }

        if kind == SyntaxKind::CloseParen {
            if prev_kind == Some(SyntaxKind::LineComment) || emitter.raw_string_at_line_end {
                emitter.emit_newline(opener_col);
            }
            format_element(&child, ctx, emitter);
            return;
        }

        match prev_kind {
            Some(SyntaxKind::VectorStart) => {
                if kind == SyntaxKind::LineComment {
                    if ws_newlines > 0 {
                        emitter.emit_newline(elem_indent);
                    } else {
                        emitter.emit_space();
                    }
                } else if kind == SyntaxKind::BlockComment && ws_newlines > 0 {
                    emitter.emit_newline(elem_indent);
                }
            }
            Some(SyntaxKind::LineComment) => {
                if ws_newlines >= 2 {
                    emitter.emit_blank_line(elem_indent);
                } else {
                    emitter.emit_newline(elem_indent);
                }
            }
            Some(_) => {
                if ws_newlines >= 2 {
                    emitter.emit_blank_line(elem_indent);
                } else if ws_newlines == 1 {
                    emitter.emit_newline(elem_indent);
                } else {
                    emitter.emit_space();
                }
            }
            None => {}
        }

        format_element(&child, ctx.with_role(ListRole::Data), emitter);
        prev_kind = Some(kind);
        ws_newlines = 0;
    }
}

#[derive(Debug, Clone, Copy)]
enum ListLayout {
    /// Align all elements at `opener_col + 1`.
    Data,
    /// Fixed number of special arguments before body (`opener_col + 2`).
    Special {
        special_args: usize,
        arg1_role: ListRole,
        arg2_role: ListRole,
        arg3_role: ListRole,
        body_role: ListRole,
    },
    /// `DEFMETHOD`: qualifiers + lambda-list before body (`opener_col + 2`).
    Defmethod { seen_lambda_list: bool },
    /// Ordinary call: align with `arg1` if `arg1` is on the opener line, else `opener_col + 2`.
    Call {
        elem0_single_line: bool,
        arg1_col: Option<usize>,
    },
}

fn classify_list(node: &SyntaxNode, ctx: FormatContext) -> ListLayout {
    if ctx.quoted {
        return ListLayout::Data;
    }
    match ctx.role {
        ListRole::Data | ListRole::LocalFunctionList | ListRole::BindingList => ListLayout::Data,
        ListRole::LocalFunction => ListLayout::Special {
            special_args: 1,
            arg1_role: ListRole::Data,
            arg2_role: ListRole::Form,
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },
        ListRole::BindingPair => ListLayout::Special {
            special_args: 0,
            arg1_role: ListRole::Form,
            arg2_role: ListRole::Form,
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },
        ListRole::Clause { special_args } => ListLayout::Special {
            special_args,
            arg1_role: if special_args >= 1 {
                ListRole::Data
            } else {
                ListRole::Form
            },
            arg2_role: ListRole::Form,
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },
        ListRole::Form => {
            let Some(first) = elements(node).next() else {
                return ListLayout::Data;
            };
            if let Some(token) = first
                .as_token()
                .filter(|token| token.kind() == SyntaxKind::Atom)
            {
                return SymbolName::with_standard_operator(token.text(), classify_operator)
                    .unwrap_or(ListLayout::Call {
                        elem0_single_line: false,
                        arg1_col: None,
                    });
            }
            if matches!(
                first,
                Element::Node(ref n) if matches!(n.kind(), SyntaxKind::List | SyntaxKind::Vector)
            ) {
                ListLayout::Data
            } else {
                ListLayout::Call {
                    elem0_single_line: false,
                    arg1_col: None,
                }
            }
        }
    }
}

fn classify_operator(op: &str) -> ListLayout {
    let default_special = |special_args: usize| ListLayout::Special {
        special_args,
        arg1_role: ListRole::Form,
        arg2_role: ListRole::Form,
        arg3_role: ListRole::Form,
        body_role: ListRole::Form,
    };

    match op {
        "PROGN" | "LOCALLY" | "TAGBODY" | "IGNORE-ERRORS" | "WITH-STANDARD-IO-SYNTAX" | "IF" => {
            default_special(0)
        }

        "COND" => ListLayout::Special {
            special_args: 0,
            arg1_role: ListRole::Clause { special_args: 0 },
            arg2_role: ListRole::Clause { special_args: 0 },
            arg3_role: ListRole::Clause { special_args: 0 },
            body_role: ListRole::Clause { special_args: 0 },
        },

        "LET" | "LET*" | "SYMBOL-MACROLET" => ListLayout::Special {
            special_args: 1,
            arg1_role: ListRole::BindingList,
            arg2_role: ListRole::Form,
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },

        "FLET" | "LABELS" | "MACROLET" => ListLayout::Special {
            special_args: 1,
            arg1_role: ListRole::LocalFunctionList,
            arg2_role: ListRole::Form,
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },

        "CASE" | "ECASE" | "CCASE" | "TYPECASE" | "ETYPECASE" | "CTYPECASE" => {
            ListLayout::Special {
                special_args: 1,
                arg1_role: ListRole::Form,
                arg2_role: ListRole::Clause { special_args: 0 },
                arg3_role: ListRole::Clause { special_args: 0 },
                body_role: ListRole::Clause { special_args: 0 },
            }
        }

        "HANDLER-CASE" | "RESTART-CASE" => ListLayout::Special {
            special_args: 1,
            arg1_role: ListRole::Form,
            arg2_role: ListRole::Clause { special_args: 1 },
            arg3_role: ListRole::Clause { special_args: 1 },
            body_role: ListRole::Clause { special_args: 1 },
        },

        "LAMBDA"
        | "PROG"
        | "PROG*"
        | "DOLIST"
        | "DOTIMES"
        | "WITH-OPEN-FILE"
        | "WITH-OPEN-STREAM"
        | "WITH-INPUT-FROM-STRING"
        | "WITH-OUTPUT-TO-STRING"
        | "WITH-SLOTS"
        | "WITH-ACCESSORS"
        | "HANDLER-BIND"
        | "RESTART-BIND"
        | "EVAL-WHEN"
        | "WITH-PACKAGE-ITERATOR"
        | "WITH-HASH-TABLE-ITERATOR"
        | "WITH-COMPILATION-UNIT" => ListLayout::Special {
            special_args: 1,
            arg1_role: ListRole::Data,
            arg2_role: ListRole::Form,
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },

        "BLOCK"
        | "CATCH"
        | "WHEN"
        | "UNLESS"
        | "UNWIND-PROTECT"
        | "PROG1"
        | "MULTIPLE-VALUE-PROG1"
        | "DEFPACKAGE"
        | "DEFSTRUCT"
        | "DEFVAR"
        | "DEFPARAMETER"
        | "DEFCONSTANT"
        | "DEFINE-SYMBOL-MACRO"
        | "LOOP"
        | "PRINT-UNREADABLE-OBJECT" => default_special(1),

        "DEFUN"
        | "DEFMACRO"
        | "DEFGENERIC"
        | "DEFTYPE"
        | "DEFINE-COMPILER-MACRO"
        | "DEFINE-SETF-EXPANDER" => ListLayout::Special {
            special_args: 2,
            arg1_role: ListRole::Form,
            arg2_role: ListRole::Data,
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },

        "MULTIPLE-VALUE-BIND" | "DESTRUCTURING-BIND" => ListLayout::Special {
            special_args: 2,
            arg1_role: ListRole::Data,
            arg2_role: ListRole::Form,
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },

        "DO" | "DO*" => ListLayout::Special {
            special_args: 2,
            arg1_role: ListRole::BindingList,
            arg2_role: ListRole::Clause { special_args: 0 },
            arg3_role: ListRole::Form,
            body_role: ListRole::Form,
        },

        "DEFCLASS" | "DEFINE-CONDITION" => ListLayout::Special {
            special_args: 2,
            arg1_role: ListRole::Form,
            arg2_role: ListRole::Data,
            arg3_role: ListRole::Data,
            body_role: ListRole::Form,
        },

        "PROGV" | "WITH-CONDITION-RESTARTS" => default_special(2),

        "DEFMETHOD" => ListLayout::Defmethod {
            seen_lambda_list: false,
        },

        _ => ListLayout::Call {
            elem0_single_line: false,
            arg1_col: None,
        },
    }
}

fn format_list(node: &SyntaxNode, ctx: FormatContext, emitter: &mut Emitter) {
    let opener_col = emitter.current_col();
    let opener_line = emitter.current_line();
    let mut layout = classify_list(node, ctx);
    let mut elements_seen = 0usize;
    let mut prev_kind: Option<SyntaxKind> = None;
    let mut ws_newlines = 0usize;

    for child in node.children_with_tokens() {
        let kind = child.kind();
        if kind == SyntaxKind::Whitespace {
            if let Some(tok) = child.as_token() {
                ws_newlines += count_line_breaks(tok.text());
            }
            continue;
        }

        if kind == SyntaxKind::OpenParen {
            format_element(&child, ctx, emitter);
            prev_kind = Some(kind);
            ws_newlines = 0;
            continue;
        }

        if kind == SyntaxKind::CloseParen {
            if prev_kind == Some(SyntaxKind::LineComment) || emitter.raw_string_at_line_end {
                emitter.emit_newline(opener_col);
            }
            format_element(&child, ctx, emitter);
            return;
        }

        let indent = next_list_indent(opener_col, elements_seen, &layout);
        match prev_kind {
            Some(SyntaxKind::OpenParen) => {
                if kind == SyntaxKind::LineComment {
                    if ws_newlines > 0 {
                        emitter.emit_newline(indent);
                    } else {
                        emitter.emit_space();
                    }
                } else if kind == SyntaxKind::BlockComment && ws_newlines > 0 {
                    emitter.emit_newline(indent);
                }
            }
            Some(SyntaxKind::LineComment) => {
                if ws_newlines >= 2 {
                    emitter.emit_blank_line(indent);
                } else {
                    emitter.emit_newline(indent);
                }
            }
            Some(_) => {
                if ws_newlines >= 2 {
                    emitter.emit_blank_line(indent);
                } else if ws_newlines == 1 {
                    emitter.emit_newline(indent);
                } else {
                    emitter.emit_space();
                }
            }
            None => {}
        }

        let is_comment = matches!(kind, SyntaxKind::LineComment | SyntaxKind::BlockComment);
        let is_dot = kind == SyntaxKind::Dot;
        if is_comment || is_dot {
            format_element(&child, ctx, emitter);
        } else {
            let elem_start_line = emitter.current_line();
            let elem_start_col = emitter.current_col();
            let child_role = child_list_role(ctx.role, elements_seen, &layout, &child);
            format_element(&child, ctx.with_role(child_role), emitter);
            let elem_end_line = emitter.current_line();

            observe_list_element(
                &mut layout,
                elements_seen,
                &child,
                opener_line,
                elem_start_line,
                elem_end_line,
                elem_start_col,
            );
            elements_seen += 1;
        }

        prev_kind = Some(kind);
        ws_newlines = 0;
    }
}

fn observe_list_element(
    layout: &mut ListLayout,
    index: usize,
    element: &Element,
    opener_line: usize,
    start_line: usize,
    end_line: usize,
    column: usize,
) {
    match layout {
        ListLayout::Call {
            elem0_single_line,
            arg1_col,
        } => {
            if index == 0 {
                *elem0_single_line = start_line == opener_line && end_line == opener_line;
            } else if index == 1 && *elem0_single_line && start_line == opener_line {
                *arg1_col = Some(column);
            }
        }
        ListLayout::Defmethod { seen_lambda_list }
            if index >= 2 && element.kind() == SyntaxKind::List =>
        {
            *seen_lambda_list = true;
        }
        _ => {}
    }
}

fn next_list_indent(opener_col: usize, elements_seen: usize, layout: &ListLayout) -> usize {
    if elements_seen == 0 {
        return opener_col + 1;
    }
    match *layout {
        ListLayout::Data => opener_col + 1,
        ListLayout::Special { special_args, .. } => {
            if elements_seen <= special_args {
                opener_col + 4
            } else {
                opener_col + 2
            }
        }
        ListLayout::Defmethod { seen_lambda_list } => {
            if seen_lambda_list {
                opener_col + 2
            } else {
                opener_col + 4
            }
        }
        ListLayout::Call { arg1_col, .. } => arg1_col.unwrap_or(opener_col + 2),
    }
}

fn child_list_role(
    parent_role: ListRole,
    elements_seen: usize,
    layout: &ListLayout,
    child: &Element,
) -> ListRole {
    match parent_role {
        ListRole::LocalFunctionList => return ListRole::LocalFunction,
        ListRole::BindingList => return ListRole::BindingPair,
        ListRole::Data => return ListRole::Form,
        _ => {}
    }
    match *layout {
        ListLayout::Data => ListRole::Form,
        ListLayout::Special {
            special_args,
            arg1_role,
            arg2_role,
            arg3_role,
            body_role,
        } => match elements_seen {
            0 => ListRole::Form,
            1 => arg1_role,
            2 if special_args >= 2 => arg2_role,
            3 if arg3_role != ListRole::Form => arg3_role,
            _ => body_role,
        },
        ListLayout::Defmethod { seen_lambda_list } => {
            if elements_seen == 1
                || (elements_seen >= 2
                    && !seen_lambda_list
                    && child
                        .as_node()
                        .is_some_and(|n| n.kind() == SyntaxKind::List))
            {
                ListRole::Data
            } else {
                ListRole::Form
            }
        }
        ListLayout::Call { .. } => ListRole::Form,
    }
}

fn count_line_breaks(text: &str) -> usize {
    let bytes = text.as_bytes();
    bytes
        .iter()
        .enumerate()
        .filter(|&(i, &b)| b == b'\n' || (b == b'\r' && bytes.get(i + 1) != Some(&b'\n')))
        .count()
}

fn same_non_whitespace_tokens(original: &str, formatted: &str) -> bool {
    fn non_whitespace_tokens(text: &str) -> impl Iterator<Item = LexedToken<'_>> {
        lex(text, SourceReadtable::Standard).filter(|token| token.kind != SyntaxKind::Whitespace)
    }

    let mut original = non_whitespace_tokens(original);
    let mut formatted = non_whitespace_tokens(formatted);
    loop {
        match (original.next(), formatted.next()) {
            (None, None) => return true,
            (Some(before), Some(after))
                if before.kind == after.kind
                    && before.text == after.text
                    && before.diagnostic.is_none()
                    && after.diagnostic.is_none() => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceWorkspace;

    fn assert_formats_idempotently(input: &str, expected: &str) {
        let formatted = format_source(input).expect("valid source should format");
        assert_eq!(formatted, expected);
        let second = format_source(&formatted).expect("formatted source should format");
        assert_eq!(second, formatted, "formatting must be idempotent");

        let mut ws = SourceWorkspace::default();
        ws.set_document("test.lisp", &formatted);
        let snap = ws.snapshot("test.lisp").unwrap();
        assert!(
            format_edits(&snap).is_empty(),
            "already formatted snapshot must produce empty edits"
        );
    }

    #[test]
    fn formatting_is_idempotent_across_common_indentation_roles() {
        let input = "\
  (  defun   add-values ( x   y )
      ;; Add two numbers
   ( let ( ( sum ( +  x   y ) )
          ( scale   2 ) )
        ( *   sum
            scale ) ) )

( flet ( ( sum-of ( a   b )
              ( +  a   b ) ) )
   ( cond
     ( ( > ( sum-of 1 2 )  0 )
          :positive )
     ( t
          :non-positive ) ) )

( defmethod  process  :after ( ( item   integer )  &optional ( step  1 ) )
     ( multiple-value-bind ( q   r )
         ( floor  item   step )
       ( list  q   r ) ) )
";
        let expected = "\
(defun add-values (x y)
  ;; Add two numbers
  (let ((sum (+ x y))
        (scale 2))
    (* sum
       scale)))

(flet ((sum-of (a b)
         (+ a b)))
  (cond
    ((> (sum-of 1 2) 0)
      :positive)
    (t
      :non-positive)))

(defmethod process :after ((item integer) &optional (step 1))
  (multiple-value-bind (q r)
      (floor item step)
    (list q r)))
";
        assert_formats_idempotently(input, expected);
    }

    #[test]
    fn formatting_preserves_non_whitespace_reader_text() {
        let input = "\
(  defun  |weird name| ( #\\Space  #\\Newline  \\a\\ b )
  #| nested #| block |# comment |#
  ( list  'foo  ` ( ,a  , @b  ,@c  ,.d )
         #'car  #2a( ( 1 2 ) ( 3 4 ) )
         #+sbcl  ( sb-ext:truly-the  fixnum  1 )
         #- (or sbcl ccl)
         \"multi
line   string\"
         ( cons  1  .  2 ) ; inline comment
  ) )";
        let expected = "\
(defun |weird name| (#\\Space #\\Newline \\a\\ b)
  #| nested #| block |# comment |#
  (list 'foo `(,a , @b ,@c ,.d)
        #'car #2a((1 2) (3 4))
        #+sbcl (sb-ext:truly-the fixnum 1)
        #-(or sbcl ccl)
        \"multi
line   string\"
        (cons 1 . 2) ; inline comment
  ))";
        assert_formats_idempotently(input, expected);
    }

    #[test]
    fn formatting_stops_at_unknown_reader_boundaries() {
        let with_unknown = "(  defun ok ( x ) ( + x 1 ) )\n\n#?custom(  unformatted   region )\n(  still   unformatted )\n";
        let expected_unknown = "(defun ok (x) (+ x 1))\n\n#?custom(  unformatted   region )\n(  still   unformatted )\n";
        assert_formats_idempotently(with_unknown, expected_unknown);

        let with_readtable = "(  defun before ()  42 )\n(set-macro-character #\\[ #'read-vec)\n(  after   mutation   1   2 )\n";
        let expected_readtable = "(defun before () 42)\n(set-macro-character #\\[ #'read-vec)\n(  after   mutation   1   2 )\n";
        assert_formats_idempotently(with_readtable, expected_readtable);

        let with_setf_readtable =
            "(setf (readtable-case *readtable*) :invert)\n(  keep   me   verbatim )";
        assert_formats_idempotently(with_setf_readtable, with_setf_readtable);
    }

    #[test]
    fn formatting_omits_edits_for_incomplete_source() {
        let mut ws = SourceWorkspace::default();
        ws.set_document("broken.lisp", "(defun foo (x) (+ x 1)");
        let broken = ws.snapshot("broken.lisp").unwrap();
        assert_eq!(format_source(broken.text()), None);
        assert!(format_edits(&broken).is_empty());
    }

    #[test]
    fn formatting_returns_one_edit_for_a_changed_region() {
        let mut ws = SourceWorkspace::default();
        ws.set_document("dirty.lisp", "(defun foo (x)\r\n  ( +  x   1 ))\r\n");
        let dirty = ws.snapshot("dirty.lisp").unwrap();
        let edits = format_edits(&dirty);
        assert_eq!(edits.len(), 1);
        let edit = &edits[0];
        let mut patched = dirty.text().to_owned();
        let range = usize::from(edit.range.start())..usize::from(edit.range.end());
        patched.replace_range(range, &edit.replacement);
        assert_eq!(patched, "(defun foo (x)\n  (+ x 1))\n");
    }
}
