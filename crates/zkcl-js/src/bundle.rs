use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

// The JS runtime ships as a self-contained ES module. Bundling resolves local
// module dependencies statically at compile time so generated code runs
// without eval, data URLs, an external bundler, or filesystem dependencies.
const MODULES: &[(&str, &str)] = &[
    ("objects", include_str!("../runtime/objects.mjs")),
    ("numbers", include_str!("../runtime/numbers.mjs")),
    ("hash_tables", include_str!("../runtime/hash_tables.mjs")),
    ("execution", include_str!("../runtime/execution.mjs")),
    ("async", include_str!("../runtime/async.mjs")),
    ("codegen", include_str!("../runtime/codegen.mjs")),
    ("loader", include_str!("../runtime/loader.mjs")),
    (
        "format_numeric",
        include_str!("../runtime/format_numeric.mjs"),
    ),
    ("format", include_str!("../runtime/format.mjs")),
    (
        "builtins_helpers",
        include_str!("../runtime/builtins_helpers.mjs"),
    ),
    ("types", include_str!("../runtime/types.mjs")),
    ("printer", include_str!("../runtime/printer.mjs")),
    ("installers", include_str!("../runtime/installers.mjs")),
    (
        "builtins_numbers",
        include_str!("../runtime/builtins_numbers.mjs"),
    ),
    (
        "builtins_sequences",
        include_str!("../runtime/builtins_sequences.mjs"),
    ),
    (
        "builtins_arrays",
        include_str!("../runtime/builtins_arrays.mjs"),
    ),
    (
        "builtins_chars",
        include_str!("../runtime/builtins_chars.mjs"),
    ),
    (
        "builtins_symbols",
        include_str!("../runtime/builtins_symbols.mjs"),
    ),
    (
        "builtins_printing",
        include_str!("../runtime/builtins_printing.mjs"),
    ),
    ("builtins", include_str!("../runtime/builtins.mjs")),
    ("reader", include_str!("../runtime/reader.mjs")),
    ("file_streams", include_str!("../runtime/file_streams.mjs")),
    ("readtable", include_str!("../runtime/readtable.mjs")),
    ("pathnames", include_str!("../runtime/pathnames.mjs")),
    ("macros", include_str!("../runtime/macros.mjs")),
    (
        "method_combination",
        include_str!("../runtime/method_combination.mjs"),
    ),
    ("clos", include_str!("../runtime/clos.mjs")),
    ("compiler", include_str!("../runtime/compiler.mjs")),
    ("runtime", include_str!("../runtime/runtime.mjs")),
];

pub(super) fn runtime_bundle() -> String {
    bundle_modules(MODULES)
}

fn bundle_modules(modules: &[(&str, &str)]) -> String {
    let known: BTreeSet<&str> = modules.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        known.len(),
        modules.len(),
        "duplicate runtime module name in MODULES"
    );

    let mut transformed: BTreeMap<&str, BundledModule> = BTreeMap::new();
    for &(name, source) in modules {
        transformed.insert(name, transform_module(name, source, &known));
    }

    let mut output =
        String::from("// Bundled zkcl JS runtime.\nconst __zkcl_modules = Object.create(null);\n");
    let mut visited = BTreeSet::new();
    let mut visiting = BTreeSet::new();

    for &(name, _) in modules {
        visit_module(name, &transformed, &mut visiting, &mut visited, &mut output);
    }

    output
}

fn visit_module<'a>(
    name: &'a str,
    modules: &'a BTreeMap<&'a str, BundledModule>,
    visiting: &mut BTreeSet<&'a str>,
    visited: &mut BTreeSet<&'a str>,
    output: &mut String,
) {
    if visited.contains(name) {
        return;
    }
    assert!(
        visiting.insert(name),
        "cyclic dependency involving bundled runtime module `{name}`"
    );

    let bundled = &modules[name];
    for dep in &bundled.deps {
        visit_module(dep.as_str(), modules, visiting, visited, output);
    }

    visiting.remove(name);
    visited.insert(name);
    output.push_str(&bundled.code);
}

struct BundledModule {
    deps: BTreeSet<String>,
    code: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BraceKind {
    Block,
    ObjectLiteral,
    TemplateExpr,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParenKind {
    ControlHeader,
    Normal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenKind {
    Ident,
    PrivateIdent,
    String,
    Number,
    Regex,
    TemplateEnd,
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: TokenKind,
    span: Span,
    newline_before: bool,
}

#[derive(Clone)]
struct JsLexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    brace_stack: Vec<BraceKind>,
    paren_stack: Vec<ParenKind>,
    bracket_depth: usize,
    regex_allowed: bool,
    prev_was_dot: bool,
    pending_control_header: bool,
    last_can_start_block_before_brace: bool,
    last_can_end_expr: bool,
}

impl<'a> JsLexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            brace_stack: Vec::new(),
            paren_stack: Vec::new(),
            bracket_depth: 0,
            regex_allowed: true,
            prev_was_dot: false,
            pending_control_header: false,
            last_can_start_block_before_brace: true,
            last_can_end_expr: false,
        }
    }

    fn depth(&self) -> usize {
        self.brace_stack.len() + self.paren_stack.len() + self.bracket_depth
    }

    fn slice(&self, span: Span) -> &'a str {
        &self.src[span.start..span.end]
    }

    fn skip_trivia(&mut self) -> bool {
        let mut saw_newline = false;
        if self.pos == 0 && self.src.starts_with("#!") {
            while self.pos < self.bytes.len()
                && self.bytes[self.pos] != b'\n'
                && self.bytes[self.pos] != b'\r'
            {
                self.pos += 1;
            }
        }
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'\n' || b == b'\r' {
                saw_newline = true;
                self.pos += 1;
                continue;
            }
            if b.is_ascii_whitespace() {
                self.pos += 1;
                continue;
            }
            if !b.is_ascii() {
                if let Some(ch) = self.src[self.pos..].chars().next() {
                    if ch.is_whitespace() {
                        if ch == '\u{2028}' || ch == '\u{2029}' {
                            saw_newline = true;
                        }
                        self.pos += ch.len_utf8();
                        continue;
                    }
                }
            }
            if b == b'/' && self.bytes.get(self.pos + 1) == Some(&b'/') {
                self.pos += 2;
                while self.pos < self.bytes.len()
                    && self.bytes[self.pos] != b'\n'
                    && self.bytes[self.pos] != b'\r'
                {
                    self.pos += 1;
                }
                continue;
            }
            if b == b'/' && self.bytes.get(self.pos + 1) == Some(&b'*') {
                self.pos += 2;
                let mut closed = false;
                while self.pos < self.bytes.len() {
                    let cur = self.bytes[self.pos];
                    if cur == b'\n' || cur == b'\r' {
                        saw_newline = true;
                    }
                    if cur == b'*' && self.bytes.get(self.pos + 1) == Some(&b'/') {
                        self.pos += 2;
                        closed = true;
                        break;
                    }
                    self.pos += 1;
                }
                assert!(closed, "unterminated block comment in JS runtime module");
                continue;
            }
            break;
        }
        saw_newline
    }

    fn scan_quoted(&mut self, quote: u8) -> usize {
        self.pos += 1;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'\\' {
                self.pos += 1;
                if self.pos < self.bytes.len() {
                    let step = self.src[self.pos..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8);
                    self.pos += step;
                }
                continue;
            }
            if b == quote {
                self.pos += 1;
                return self.pos;
            }
            assert!(
                b != b'\n' && b != b'\r',
                "unterminated string literal in JS runtime module"
            );
            self.pos += 1;
        }
        panic!("unterminated string literal in JS runtime module");
    }

    fn scan_regex(&mut self) -> usize {
        self.pos += 1;
        let mut in_class = false;
        let mut closed = false;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            assert!(
                b != b'\n' && b != b'\r',
                "unterminated regex literal in JS runtime module"
            );
            if b == b'\\' {
                self.pos += 1;
                if self.pos < self.bytes.len() {
                    let step = self.src[self.pos..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8);
                    self.pos += step;
                }
                continue;
            }
            if b == b'[' {
                in_class = true;
                self.pos += 1;
                continue;
            }
            if b == b']' {
                in_class = false;
                self.pos += 1;
                continue;
            }
            if b == b'/' && !in_class {
                self.pos += 1;
                closed = true;
                break;
            }
            self.pos += 1;
        }
        assert!(closed, "unterminated regex literal in JS runtime module");
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_alphabetic() {
            self.pos += 1;
        }
        self.pos
    }

    fn scan_template_chunk(&mut self) -> bool {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'\\' {
                self.pos += 1;
                if self.pos < self.bytes.len() {
                    let step = self.src[self.pos..]
                        .chars()
                        .next()
                        .map_or(1, char::len_utf8);
                    self.pos += step;
                }
                continue;
            }
            if b == b'`' {
                self.pos += 1;
                return true;
            }
            if b == b'$' && self.bytes.get(self.pos + 1) == Some(&b'{') {
                self.pos += 2;
                self.brace_stack.push(BraceKind::TemplateExpr);
                return false;
            }
            self.pos += 1;
        }
        panic!("unterminated template literal in JS runtime module");
    }

    fn scan_number(&mut self) -> usize {
        if self.bytes[self.pos] == b'0' {
            if let Some(&prefix) = self.bytes.get(self.pos + 1) {
                if matches!(prefix, b'x' | b'X' | b'b' | b'B' | b'o' | b'O') {
                    self.pos += 2;
                    while self.pos < self.bytes.len()
                        && (self.bytes[self.pos].is_ascii_hexdigit()
                            || self.bytes[self.pos] == b'_')
                    {
                        self.pos += 1;
                    }
                    if self.bytes.get(self.pos) == Some(&b'n') {
                        self.pos += 1;
                    }
                    return self.pos;
                }
            }
        }
        while self.pos < self.bytes.len()
            && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'_')
        {
            self.pos += 1;
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            while self.pos < self.bytes.len()
                && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'_')
            {
                self.pos += 1;
            }
        }
        if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while self.pos < self.bytes.len()
                && (self.bytes[self.pos].is_ascii_digit() || self.bytes[self.pos] == b'_')
            {
                self.pos += 1;
            }
        } else if self.bytes.get(self.pos) == Some(&b'n') {
            self.pos += 1;
        }
        self.pos
    }

    fn next_token(&mut self) -> Token {
        let newline_before = self.skip_trivia();
        let start = self.pos;
        if start >= self.bytes.len() {
            return Token {
                kind: TokenKind::Eof,
                span: Span { start, end: start },
                newline_before,
            };
        }

        let b = self.bytes[start];
        let after_dot = self.prev_was_dot;
        let pending_control = self.pending_control_header;
        self.prev_was_dot = false;
        self.pending_control_header = false;

        if b == b'\'' || b == b'"' {
            let end = self.scan_quoted(b);
            self.regex_allowed = false;
            self.last_can_start_block_before_brace = false;
            self.last_can_end_expr = true;
            return Token {
                kind: TokenKind::String,
                span: Span { start, end },
                newline_before,
            };
        }

        if b == b'`' {
            self.pos += 1;
            while !self.scan_template_chunk() {
                // Consume interpolated expressions balanced to the template's own closing backtick.
                let target_depth = self.brace_stack.len();
                while self.brace_stack.len() >= target_depth {
                    let inner = self.next_token();
                    assert!(
                        inner.kind != TokenKind::Eof,
                        "unterminated template expression in JS runtime module"
                    );
                }
                if self.bytes.get(self.pos - 1) == Some(&b'`')
                    && self.brace_stack.len() < target_depth
                {
                    break;
                }
            }
            self.regex_allowed = false;
            self.last_can_start_block_before_brace = false;
            self.last_can_end_expr = true;
            return Token {
                kind: TokenKind::TemplateEnd,
                span: Span {
                    start,
                    end: self.pos,
                },
                newline_before,
            };
        }

        if b == b'/' && self.regex_allowed {
            let end = self.scan_regex();
            self.regex_allowed = false;
            self.last_can_start_block_before_brace = false;
            self.last_can_end_expr = true;
            return Token {
                kind: TokenKind::Regex,
                span: Span { start, end },
                newline_before,
            };
        }

        if b.is_ascii_digit()
            || (b == b'.' && self.bytes.get(start + 1).is_some_and(u8::is_ascii_digit))
        {
            let end = self.scan_number();
            self.regex_allowed = false;
            self.last_can_start_block_before_brace = false;
            self.last_can_end_expr = true;
            return Token {
                kind: TokenKind::Number,
                span: Span { start, end },
                newline_before,
            };
        }

        if b == b'#' && self.pos + 1 < self.bytes.len() && is_ident_start(self.src, self.pos + 1) {
            self.pos += 1;
            self.advance_ident();
            self.regex_allowed = false;
            self.last_can_start_block_before_brace = false;
            self.last_can_end_expr = true;
            return Token {
                kind: TokenKind::PrivateIdent,
                span: Span {
                    start,
                    end: self.pos,
                },
                newline_before,
            };
        }

        if is_ident_start(self.src, start) {
            self.advance_ident();
            let end = self.pos;
            let ident = &self.src[start..end];
            if after_dot {
                self.regex_allowed = false;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = true;
            } else {
                if pending_control && ident == "await" {
                    self.pending_control_header = true;
                } else {
                    self.pending_control_header = matches!(ident, "if" | "while" | "for" | "with");
                }
                self.regex_allowed = matches!(
                    ident,
                    "return"
                        | "throw"
                        | "case"
                        | "default"
                        | "delete"
                        | "void"
                        | "typeof"
                        | "await"
                        | "yield"
                        | "in"
                        | "instanceof"
                        | "new"
                        | "else"
                        | "do"
                        | "of"
                        | "export"
                );
                self.last_can_start_block_before_brace =
                    matches!(ident, "else" | "do" | "try" | "finally");
                self.last_can_end_expr = !matches!(
                    ident,
                    "return"
                        | "throw"
                        | "case"
                        | "delete"
                        | "void"
                        | "typeof"
                        | "await"
                        | "in"
                        | "instanceof"
                        | "new"
                        | "else"
                        | "do"
                        | "of"
                );
            }
            return Token {
                kind: TokenKind::Ident,
                span: Span { start, end },
                newline_before,
            };
        }

        let punct = self.scan_punct(pending_control);
        Token {
            kind: TokenKind::Punct(punct),
            span: Span {
                start,
                end: self.pos,
            },
            newline_before,
        }
    }

    fn advance_ident(&mut self) {
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b.is_ascii() {
                if b.is_ascii_alphanumeric() || b == b'_' || b == b'$' {
                    self.pos += 1;
                } else {
                    break;
                }
            } else {
                let Some(ch) = self.src[self.pos..].chars().next() else {
                    break;
                };
                if ch.is_whitespace() {
                    break;
                }
                self.pos += ch.len_utf8();
            }
        }
    }

    fn scan_punct(&mut self, pending_control: bool) -> &'static str {
        let rest = &self.src[self.pos..];
        let multi: &[&str] = &[
            ">>>=", "===", "!==", ">>>", "<<=", ">>=", "&&=", "||=", "??=", "**=", "...", "=>",
            "++", "--", "**", "&&", "||", "??", "==", "!=", "<=", ">=", "<<", ">>", "+=", "-=",
            "*=", "/=", "%=", "&=", "|=", "^=",
        ];
        if rest.starts_with("?.") && !rest.as_bytes().get(2).is_some_and(u8::is_ascii_digit) {
            self.pos += 2;
            self.prev_was_dot = true;
            self.regex_allowed = false;
            self.last_can_start_block_before_brace = false;
            self.last_can_end_expr = false;
            return "?.";
        }
        for &op in multi {
            if rest.starts_with(op) {
                self.pos += op.len();
                match op {
                    "++" | "--" => {
                        self.regex_allowed = false;
                        self.last_can_start_block_before_brace = false;
                        self.last_can_end_expr = true;
                    }
                    "=>" => {
                        self.regex_allowed = true;
                        self.last_can_start_block_before_brace = true;
                        self.last_can_end_expr = false;
                    }
                    _ => {
                        self.regex_allowed = true;
                        self.last_can_start_block_before_brace = false;
                        self.last_can_end_expr = false;
                    }
                }
                return op;
            }
        }

        let single = &self.src[self.pos..self.pos + 1];
        self.pos += 1;
        match single {
            "(" => {
                self.paren_stack.push(if pending_control {
                    ParenKind::ControlHeader
                } else {
                    ParenKind::Normal
                });
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "("
            }
            ")" => {
                let kind = self.paren_stack.pop().unwrap_or(ParenKind::Normal);
                self.regex_allowed = kind == ParenKind::ControlHeader;
                self.last_can_start_block_before_brace = true;
                self.last_can_end_expr = true;
                ")"
            }
            "[" => {
                self.bracket_depth += 1;
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "["
            }
            "]" => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                self.regex_allowed = false;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = true;
                "]"
            }
            "{" => {
                let kind = if self.last_can_start_block_before_brace {
                    BraceKind::Block
                } else {
                    BraceKind::ObjectLiteral
                };
                self.brace_stack.push(kind);
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = true;
                self.last_can_end_expr = false;
                "{"
            }
            "}" => {
                match self.brace_stack.pop() {
                    Some(BraceKind::TemplateExpr) => {
                        if !self.scan_template_chunk() {
                            // Another `${` follows inside the same outer template literal.
                            self.regex_allowed = true;
                            self.last_can_start_block_before_brace = false;
                            self.last_can_end_expr = false;
                        } else {
                            self.regex_allowed = false;
                            self.last_can_start_block_before_brace = false;
                            self.last_can_end_expr = true;
                        }
                    }
                    Some(BraceKind::Block) | None => {
                        self.regex_allowed = true;
                        self.last_can_start_block_before_brace = true;
                        self.last_can_end_expr = true;
                    }
                    Some(BraceKind::ObjectLiteral) => {
                        self.regex_allowed = false;
                        self.last_can_start_block_before_brace = false;
                        self.last_can_end_expr = true;
                    }
                }
                "}"
            }
            "." => {
                self.prev_was_dot = true;
                self.regex_allowed = false;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "."
            }
            ";" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = true;
                self.last_can_end_expr = false;
                ";"
            }
            "," => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                ","
            }
            ":" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                ":"
            }
            "?" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "?"
            }
            "+" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "+"
            }
            "-" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "-"
            }
            "*" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "*"
            }
            "/" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "/"
            }
            "%" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "%"
            }
            "&" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "&"
            }
            "|" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "|"
            }
            "^" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "^"
            }
            "!" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "!"
            }
            "~" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "~"
            }
            "<" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "<"
            }
            ">" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                ">"
            }
            "=" => {
                self.regex_allowed = true;
                self.last_can_start_block_before_brace = false;
                self.last_can_end_expr = false;
                "="
            }
            other => panic!("unexpected ASCII punctuator `{other}` in JS runtime module"),
        }
    }
}

fn is_ident_start(src: &str, pos: usize) -> bool {
    let b = src.as_bytes()[pos];
    if b.is_ascii() {
        b.is_ascii_alphabetic() || b == b'_' || b == b'$'
    } else {
        src[pos..]
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_whitespace())
    }
}

fn transform_module(name: &str, source: &str, known: &BTreeSet<&str>) -> BundledModule {
    let mut lexer = JsLexer::new(source);
    let mut deps = BTreeSet::new();
    let mut exports = Vec::new();
    let mut body = String::new();
    let mut last_emitted = 0usize;

    loop {
        let was_depth_zero = lexer.depth() == 0;
        let prev_was_dot = lexer.prev_was_dot;
        let tok = lexer.next_token();
        if tok.kind == TokenKind::Eof {
            break;
        }
        if !was_depth_zero || prev_was_dot || tok.kind != TokenKind::Ident {
            continue;
        }

        let kw = lexer.slice(tok.span);
        if kw == "import" {
            let mut look = lexer.clone();
            let next = look.next_token();
            if matches!(next.kind, TokenKind::Punct("(" | ".")) {
                continue;
            }
            body.push_str(&source[last_emitted..tok.span.start]);
            let rewritten = rewrite_import(&mut lexer, name, known, &mut deps);
            body.push_str(&rewritten);
            last_emitted = lexer.pos;
            lexer.regex_allowed = true;
            lexer.last_can_start_block_before_brace = true;
            lexer.last_can_end_expr = false;
            continue;
        }

        if kw == "export" {
            body.push_str(&source[last_emitted..tok.span.start]);
            let mut look = lexer.clone();
            let next = look.next_token();
            match (next.kind, look.slice(next.span)) {
                (TokenKind::Punct("{"), _) => {
                    lexer = look;
                    let specifiers = parse_specifier_list(&mut lexer, name);
                    let mut after = lexer.clone();
                    let follow = after.next_token();
                    if follow.kind == TokenKind::Ident && after.slice(follow.span) == "from" {
                        lexer = after;
                        let spec_tok = lexer.next_token();
                        assert_eq!(
                            spec_tok.kind,
                            TokenKind::String,
                            "expected module specifier string after `from` in `{name}`"
                        );
                        let dep =
                            parse_local_module_specifier(lexer.slice(spec_tok.span), name, known);
                        deps.insert(dep.clone());
                        consume_optional_semicolon(&mut lexer);
                        for (imported, exported) in specifiers {
                            exports.push(format!("{exported}: __zkcl_modules.{dep}.{imported}"));
                        }
                    } else {
                        if follow.kind == TokenKind::Punct(";") {
                            lexer = after;
                        }
                        for (local, exported) in specifiers {
                            if local == exported {
                                exports.push(local);
                            } else {
                                exports.push(format!("{exported}: {local}"));
                            }
                        }
                    }
                    last_emitted = lexer.pos;
                    lexer.regex_allowed = true;
                    lexer.last_can_start_block_before_brace = true;
                    lexer.last_can_end_expr = false;
                }
                (TokenKind::Ident, "async") => {
                    let fn_tok = look.next_token();
                    assert!(
                        fn_tok.kind == TokenKind::Ident && look.slice(fn_tok.span) == "function",
                        "unsupported `export async` declaration in `{name}`"
                    );
                    let decl_name = parse_function_or_class_name(&mut look, name);
                    exports.push(decl_name);
                    last_emitted = next.span.start;
                }
                (TokenKind::Ident, "function" | "class") => {
                    let decl_name = parse_function_or_class_name(&mut look, name);
                    exports.push(decl_name);
                    last_emitted = next.span.start;
                }
                (TokenKind::Ident, "const" | "let" | "var") => {
                    parse_var_declarator_names(&mut look, name, &mut exports);
                    last_emitted = next.span.start;
                }
                (TokenKind::Ident, other) => {
                    panic!("unsupported `export {other}` syntax in `{name}`");
                }
                _ => {
                    panic!(
                        "unsupported `export` syntax near `{}` in `{name}`",
                        lexer.slice(next.span)
                    );
                }
            }
        }
    }

    body.push_str(&source[last_emitted..]);

    let mut code = String::new();
    writeln!(code, "__zkcl_modules.{name} = (() => {{").unwrap();
    code.push_str(body.trim_end());
    writeln!(code).unwrap();
    writeln!(code, "return {{ {} }};", exports.join(", ")).unwrap();
    writeln!(code, "}})();").unwrap();

    BundledModule { deps, code }
}

fn rewrite_import(
    lexer: &mut JsLexer<'_>,
    module_name: &str,
    known: &BTreeSet<&str>,
    deps: &mut BTreeSet<String>,
) -> String {
    let first = lexer.next_token();
    match (first.kind, lexer.slice(first.span)) {
        (TokenKind::String, spec) => {
            let dep = parse_local_module_specifier(spec, module_name, known);
            deps.insert(dep);
            consume_optional_semicolon(lexer);
            String::new()
        }
        (TokenKind::Punct("*"), _) => {
            let as_tok = lexer.next_token();
            assert!(
                as_tok.kind == TokenKind::Ident && lexer.slice(as_tok.span) == "as",
                "expected `as` after `import *` in `{module_name}`"
            );
            let alias_tok = lexer.next_token();
            assert_eq!(
                alias_tok.kind,
                TokenKind::Ident,
                "expected namespace alias after `import * as` in `{module_name}`"
            );
            let alias = lexer.slice(alias_tok.span).to_owned();
            let from_tok = lexer.next_token();
            assert!(
                from_tok.kind == TokenKind::Ident && lexer.slice(from_tok.span) == "from",
                "expected `from` after `import * as {alias}` in `{module_name}`"
            );
            let spec_tok = lexer.next_token();
            assert_eq!(
                spec_tok.kind,
                TokenKind::String,
                "expected module specifier string in `{module_name}`"
            );
            let dep = parse_local_module_specifier(lexer.slice(spec_tok.span), module_name, known);
            deps.insert(dep.clone());
            consume_optional_semicolon(lexer);
            format!("const {alias} = __zkcl_modules.{dep};")
        }
        (TokenKind::Punct("{"), _) => {
            let specifiers = parse_specifier_list(lexer, module_name);
            let from_tok = lexer.next_token();
            assert!(
                from_tok.kind == TokenKind::Ident && lexer.slice(from_tok.span) == "from",
                "expected `from` after `import {{ ... }}` in `{module_name}`"
            );
            let spec_tok = lexer.next_token();
            assert_eq!(
                spec_tok.kind,
                TokenKind::String,
                "expected module specifier string in `{module_name}`"
            );
            let dep = parse_local_module_specifier(lexer.slice(spec_tok.span), module_name, known);
            deps.insert(dep.clone());
            consume_optional_semicolon(lexer);
            let items = specifiers
                .into_iter()
                .map(|(imported, local)| {
                    if imported == local {
                        local
                    } else {
                        format!("{imported}: {local}")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("const {{ {items} }} = __zkcl_modules.{dep};")
        }
        _ => panic!(
            "unsupported `import` form near `{}` in `{module_name}`",
            lexer.slice(first.span)
        ),
    }
}

fn consume_optional_semicolon(lexer: &mut JsLexer<'_>) {
    let mut look = lexer.clone();
    if look.next_token().kind == TokenKind::Punct(";") {
        *lexer = look;
    }
}

fn parse_specifier_list(lexer: &mut JsLexer<'_>, module_name: &str) -> Vec<(String, String)> {
    let mut items = Vec::new();
    loop {
        let tok = lexer.next_token();
        if tok.kind == TokenKind::Punct("}") {
            break;
        }
        assert_eq!(
            tok.kind,
            TokenKind::Ident,
            "expected specifier identifier near `{}` in `{module_name}`",
            lexer.slice(tok.span)
        );
        let source_name = lexer.slice(tok.span).to_owned();
        let next = lexer.next_token();
        match next.kind {
            TokenKind::Punct("}") => {
                items.push((source_name.clone(), source_name));
                break;
            }
            TokenKind::Punct(",") => {
                items.push((source_name.clone(), source_name));
            }
            TokenKind::Ident if lexer.slice(next.span) == "as" => {
                let alias_tok = lexer.next_token();
                assert_eq!(
                    alias_tok.kind,
                    TokenKind::Ident,
                    "expected alias identifier after `as` in `{module_name}`"
                );
                let alias_name = lexer.slice(alias_tok.span).to_owned();
                items.push((source_name, alias_name));
                let sep = lexer.next_token();
                match sep.kind {
                    TokenKind::Punct(",") => {}
                    TokenKind::Punct("}") => break,
                    _ => panic!(
                        "expected `,` or `}}` in specifier list near `{}` in `{module_name}`",
                        lexer.slice(sep.span)
                    ),
                }
            }
            _ => panic!(
                "unexpected token `{}` in specifier list in `{module_name}`",
                lexer.slice(next.span)
            ),
        }
    }
    items
}

fn parse_function_or_class_name(lexer: &mut JsLexer<'_>, module_name: &str) -> String {
    let mut tok = lexer.next_token();
    if tok.kind == TokenKind::Punct("*") {
        tok = lexer.next_token();
    }
    assert_eq!(
        tok.kind,
        TokenKind::Ident,
        "expected declaration name near `{}` in `{module_name}`",
        lexer.slice(tok.span)
    );
    lexer.slice(tok.span).to_owned()
}

fn parse_var_declarator_names(
    lexer: &mut JsLexer<'_>,
    module_name: &str,
    exports: &mut Vec<String>,
) {
    let base_depth = lexer.depth();
    loop {
        parse_binding_pattern(lexer, module_name, exports);
        let next = lexer.next_token();
        match next.kind {
            TokenKind::Punct(";") | TokenKind::Eof => break,
            TokenKind::Punct(",") if lexer.depth() == base_depth => continue,
            TokenKind::Punct("=") => {
                if !skip_initializer_and_check_comma(lexer, base_depth) {
                    break;
                }
            }
            _ => break,
        }
    }
}

fn parse_binding_pattern(lexer: &mut JsLexer<'_>, module_name: &str, exports: &mut Vec<String>) {
    let tok = lexer.next_token();
    match tok.kind {
        TokenKind::Ident => {
            exports.push(lexer.slice(tok.span).to_owned());
        }
        TokenKind::Punct("{") => {
            let target_depth = lexer.depth();
            loop {
                let item = lexer.next_token();
                if item.kind == TokenKind::Punct("}") {
                    break;
                }
                assert!(
                    item.kind != TokenKind::Eof,
                    "unterminated object destructuring pattern in `{module_name}`"
                );
                if item.kind == TokenKind::Punct("...") {
                    parse_binding_pattern(lexer, module_name, exports);
                } else {
                    if item.kind == TokenKind::Punct("[") {
                        skip_balanced_until(lexer, target_depth);
                    }
                    let mut look = lexer.clone();
                    let sep = look.next_token();
                    if sep.kind == TokenKind::Punct(":") {
                        *lexer = look;
                        parse_binding_pattern(lexer, module_name, exports);
                    } else if item.kind == TokenKind::Ident {
                        exports.push(lexer.slice(item.span).to_owned());
                    }
                    let mut eq_look = lexer.clone();
                    if eq_look.next_token().kind == TokenKind::Punct("=") {
                        *lexer = eq_look;
                        skip_pattern_default(lexer, target_depth, "}");
                    }
                }
                let mut sep_look = lexer.clone();
                let sep = sep_look.next_token();
                if sep.kind == TokenKind::Punct(",") {
                    *lexer = sep_look;
                } else if sep.kind == TokenKind::Punct("}") {
                    *lexer = sep_look;
                    break;
                }
            }
        }
        TokenKind::Punct("[") => {
            let target_depth = lexer.depth();
            loop {
                let mut look = lexer.clone();
                let item = look.next_token();
                if item.kind == TokenKind::Punct("]") {
                    *lexer = look;
                    break;
                }
                if item.kind == TokenKind::Punct(",") {
                    *lexer = look;
                    continue;
                }
                assert!(
                    item.kind != TokenKind::Eof,
                    "unterminated array destructuring pattern in `{module_name}`"
                );
                if item.kind == TokenKind::Punct("...") {
                    *lexer = look;
                    parse_binding_pattern(lexer, module_name, exports);
                } else {
                    parse_binding_pattern(lexer, module_name, exports);
                }
                let mut eq_look = lexer.clone();
                if eq_look.next_token().kind == TokenKind::Punct("=") {
                    *lexer = eq_look;
                    skip_pattern_default(lexer, target_depth, "]");
                }
                let mut sep_look = lexer.clone();
                let sep = sep_look.next_token();
                if sep.kind == TokenKind::Punct(",") {
                    *lexer = sep_look;
                } else if sep.kind == TokenKind::Punct("]") {
                    *lexer = sep_look;
                    break;
                }
            }
        }
        _ => panic!(
            "expected binding pattern near `{}` in `{module_name}`",
            lexer.slice(tok.span)
        ),
    }
}

fn skip_balanced_until(lexer: &mut JsLexer<'_>, target_depth: usize) {
    while lexer.depth() > target_depth {
        let tok = lexer.next_token();
        assert_ne!(tok.kind, TokenKind::Eof, "unexpected EOF in JS pattern");
    }
}

fn skip_pattern_default(lexer: &mut JsLexer<'_>, target_depth: usize, closing: &'static str) {
    loop {
        let mut look = lexer.clone();
        let tok = look.next_token();
        if tok.kind == TokenKind::Eof {
            break;
        }
        if lexer.depth() == target_depth
            && (tok.kind == TokenKind::Punct(",") || tok.kind == TokenKind::Punct(closing))
        {
            break;
        }
        *lexer = look;
    }
}

fn skip_initializer_and_check_comma(lexer: &mut JsLexer<'_>, base_depth: usize) -> bool {
    loop {
        let can_end_expr = lexer.last_can_end_expr;
        let mut look = lexer.clone();
        let tok = look.next_token();
        if tok.kind == TokenKind::Eof {
            *lexer = look;
            return false;
        }
        if lexer.depth() == base_depth {
            if tok.kind == TokenKind::Punct(";") {
                *lexer = look;
                return false;
            }
            if tok.kind == TokenKind::Punct(",") {
                *lexer = look;
                return true;
            }
            if tok.newline_before && can_end_expr && !can_continue_expr(&look, tok) {
                return false;
            }
        }
        *lexer = look;
    }
}

fn can_continue_expr(lexer_after_tok: &JsLexer<'_>, tok: Token) -> bool {
    match tok.kind {
        TokenKind::Punct(p) => matches!(
            p,
            "+" | "-"
                | "*"
                | "/"
                | "%"
                | "**"
                | "&"
                | "&&"
                | "|"
                | "||"
                | "^"
                | "=="
                | "==="
                | "!="
                | "!=="
                | "<"
                | "<="
                | "<<"
                | ">"
                | ">="
                | ">>"
                | ">>>"
                | "??"
                | "?"
                | ":"
                | "."
                | "?."
                | "("
                | "["
                | "="
                | "+="
                | "-="
                | "*="
                | "/="
                | "%="
                | "&="
                | "|="
                | "^="
                | "<<="
                | ">>="
                | ">>>="
                | "&&="
                | "||="
                | "??="
                | "**="
        ),
        TokenKind::Ident => matches!(lexer_after_tok.slice(tok.span), "in" | "instanceof"),
        _ => false,
    }
}

fn parse_local_module_specifier(
    raw_string_token: &str,
    module_name: &str,
    known: &BTreeSet<&str>,
) -> String {
    assert!(
        raw_string_token.len() >= 2,
        "invalid module specifier `{raw_string_token}` in `{module_name}`"
    );
    let inner = &raw_string_token[1..raw_string_token.len() - 1];
    assert!(
        !inner.contains('\\'),
        "escaped module specifier `{raw_string_token}` is not supported in `{module_name}`"
    );
    let local = inner
        .strip_prefix("./")
        .and_then(|s| s.strip_suffix(".mjs"))
        .unwrap_or_else(|| {
            panic!("bundled runtime only supports local `./<mod>.mjs` imports, got `{inner}` in `{module_name}`")
        });
    assert!(
        known.contains(local),
        "runtime module `{module_name}` references unknown bundled module `{local}`"
    );
    local.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_modules_keep_private_names_and_multiline_aliases() {
        let bundled = bundle_modules(&[
            (
                "sample",
                "import {\n  alpha as localAlpha,\n  beta,\n} from './dep.mjs';\nconst privateFlag = true;\nexport const publicValue = 1;\nexport function publicFn() { return privateFlag; }\n",
            ),
            ("dep", "export const alpha = 10;\nexport const beta = 20;\n"),
        ]);
        assert!(bundled.contains("const { alpha: localAlpha, beta } = __zkcl_modules.dep;"));
        assert!(bundled.contains("return { publicValue, publicFn };"));
        assert!(!bundled.contains("privateFlag }"));
        assert!(!bundled.contains("privateFlag,"));
    }

    #[test]
    fn multiline_import_ignores_semicolons_in_comments_and_strings() {
        let bundled = bundle_modules(&[
            (
                "consumer",
                "import {\n  // ; bogus ; from './missing.mjs';\n  /* ; another ; comment */\n  one,\n  two as aliasTwo,\n} from './provider.mjs';\nexport const result = one + aliasTwo;\n",
            ),
            ("provider", "export const one = 1;\nexport const two = 2;\n"),
        ]);
        let provider_pos = bundled.find("__zkcl_modules.provider =").unwrap();
        let consumer_pos = bundled.find("__zkcl_modules.consumer =").unwrap();
        assert!(provider_pos < consumer_pos);
        assert!(bundled.contains("const { one, two: aliasTwo } = __zkcl_modules.provider;"));
        assert!(bundled.contains("return { result };"));
    }

    #[test]
    fn templates_comments_and_regex_literals_do_not_trigger_import_export() {
        let src = r#"
import { base } from './base.mjs';
// import { fake } from './missing.mjs';
/*
export const neverExported = 1;
*/
const tpl = `
import { fake2 } from './missing.mjs';
export const alsoFake = ${`nested ${base} export const inner = 1;`}
export function nope() {}
`;
const re = /export const fake3/;
const div = (10 + 2) / 3;
export async function* streamValues() {
  yield div;
}
export class Box {}
"#;
        let bundled = bundle_modules(&[("demo", src), ("base", "export const base = 42;\n")]);
        assert!(bundled.contains("const { base } = __zkcl_modules.base;"));
        assert!(bundled.contains("return { streamValues, Box };"));
        assert!(!bundled.contains("neverExported }"));
        assert!(!bundled.contains("alsoFake,"));
        assert!(!bundled.contains("nope,"));
        assert!(bundled.contains("export const fake3"));
        assert!(bundled.contains("async function* streamValues()"));
        assert!(bundled.contains("class Box {}"));
    }

    #[test]
    fn reexport_from_local_module_emits_binding_and_orders_dependency_first() {
        let bundled = bundle_modules(&[
            (
                "facade",
                "export { alpha, beta as renamedBeta } from './inner.mjs';\nconst beta = 999;\nexport function getLocalBeta() { return beta; }\n",
            ),
            ("inner", "export const alpha = 1;\nexport const beta = 2;\n"),
        ]);
        let inner_pos = bundled.find("__zkcl_modules.inner =").unwrap();
        let facade_pos = bundled.find("__zkcl_modules.facade =").unwrap();
        assert!(inner_pos < facade_pos);
        assert!(bundled.contains(
            "return { alpha: __zkcl_modules.inner.alpha, renamedBeta: __zkcl_modules.inner.beta, getLocalBeta };"
        ));
    }

    #[test]
    fn export_destructuring_and_asi_declarations_collect_all_bound_names() {
        let bundled = bundle_modules(&[(
            "destr",
            "export const { a, b: aliasB, nested: [x, , ...rest] = [1, 2, 3] } = obj\nexport const first = 1, second = 2\n",
        )]);
        assert!(bundled.contains("return { a, aliasB, x, rest, first, second };"));
    }
}
