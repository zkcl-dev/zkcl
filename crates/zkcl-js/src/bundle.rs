use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

// This build-time linker is for the runtime's acyclic modules with stable
// exported bindings. Each closure owns its module's private names.
pub(super) fn bundle_modules(entry: &str, modules: &[(&str, &str)]) -> String {
    let mut bundle = Bundle {
        sources: modules.iter().copied().collect(),
        active: BTreeSet::new(),
        exports: BTreeMap::new(),
        output: String::from(
            "// Bundled zkcl JS runtime.\nconst __zkcl_modules = Object.create(null);\n",
        ),
    };
    bundle.link(entry);
    bundle.output
}

struct Bundle<'a> {
    sources: BTreeMap<&'a str, &'a str>,
    active: BTreeSet<&'a str>,
    exports: BTreeMap<&'a str, BTreeSet<&'a str>>,
    output: String,
}

impl<'a> Bundle<'a> {
    fn link(&mut self, name: &'a str) {
        if self.exports.contains_key(name) {
            return;
        }
        assert!(
            self.active.insert(name),
            "cyclic dependency involving bundled runtime module `{name}`"
        );
        let module = transform_module(name, self.sources[name]);
        // Dependencies execute once, in static import and reexport order.
        for dep in &module.deps {
            assert!(
                self.sources.contains_key(dep.name),
                "runtime module `{name}` references unknown bundled module `{}`",
                dep.name
            );
            self.link(dep.name);
            for &binding in &dep.bindings {
                assert!(
                    self.exports[dep.name].contains(binding),
                    "runtime module `{name}` imports `{binding}`, which `{}` does not export",
                    dep.name
                );
            }
        }
        self.active.remove(name);
        self.exports.insert(name, module.exports);
        self.output.push_str(&module.code);
    }
}

struct Dependency<'a> {
    name: &'a str,
    bindings: Vec<&'a str>,
}

struct BundledModule<'a> {
    deps: Vec<Dependency<'a>>,
    exports: BTreeSet<&'a str>,
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

    fn start_statement(&mut self) {
        self.expect_expression();
        self.last_can_start_block_before_brace = true;
    }

    fn expect_expression(&mut self) {
        self.regex_allowed = true;
        self.last_can_start_block_before_brace = false;
        self.last_can_end_expr = false;
    }

    fn end_expression(&mut self) {
        self.regex_allowed = false;
        self.last_can_start_block_before_brace = false;
        self.last_can_end_expr = true;
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
                self.expect_expression();
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
            self.end_expression();
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
            }
            self.end_expression();
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
            self.end_expression();
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
            self.end_expression();
            return Token {
                kind: TokenKind::Number,
                span: Span { start, end },
                newline_before,
            };
        }

        if b == b'#' && self.pos + 1 < self.bytes.len() && is_ident_start(self.src, self.pos + 1) {
            self.pos += 1;
            self.advance_ident();
            self.end_expression();
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
                self.end_expression();
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
        const COMPOUND: &[&str] = &[
            ">>>=", "===", "!==", ">>>", "<<=", ">>=", "&&=", "||=", "??=", "**=", "...", "=>",
            "++", "--", "**", "&&", "||", "??", "==", "!=", "<=", ">=", "<<", ">>", "+=", "-=",
            "*=", "/=", "%=", "&=", "|=", "^=",
        ];
        let rest = &self.src[self.pos..];
        let op =
            if rest.starts_with("?.") && !rest.as_bytes().get(2).is_some_and(u8::is_ascii_digit) {
                "?."
            } else if let Some(op) = COMPOUND.iter().copied().find(|op| rest.starts_with(op)) {
                op
            } else {
                match self.bytes[self.pos] {
                    b'(' => "(",
                    b')' => ")",
                    b'[' => "[",
                    b']' => "]",
                    b'{' => "{",
                    b'}' => "}",
                    b'.' => ".",
                    b';' => ";",
                    b',' => ",",
                    b':' => ":",
                    b'?' => "?",
                    b'+' => "+",
                    b'-' => "-",
                    b'*' => "*",
                    b'/' => "/",
                    b'%' => "%",
                    b'&' => "&",
                    b'|' => "|",
                    b'^' => "^",
                    b'!' => "!",
                    b'~' => "~",
                    b'<' => "<",
                    b'>' => ">",
                    b'=' => "=",
                    other => panic!(
                        "unexpected ASCII punctuator `{}` in JS runtime module",
                        char::from(other)
                    ),
                }
            };
        self.pos += op.len();
        let can_start_block = self.last_can_start_block_before_brace;
        self.expect_expression();
        match op {
            "(" => self.paren_stack.push(if pending_control {
                ParenKind::ControlHeader
            } else {
                ParenKind::Normal
            }),
            ")" => {
                let kind = self.paren_stack.pop().unwrap_or(ParenKind::Normal);
                self.regex_allowed = kind == ParenKind::ControlHeader;
                self.last_can_start_block_before_brace = true;
                self.last_can_end_expr = true;
            }
            "[" => self.bracket_depth += 1,
            "]" => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                self.end_expression();
            }
            "{" => {
                self.brace_stack.push(if can_start_block {
                    BraceKind::Block
                } else {
                    BraceKind::ObjectLiteral
                });
                self.start_statement();
            }
            "}" => match self.brace_stack.pop() {
                Some(BraceKind::TemplateExpr) => {}
                Some(BraceKind::Block) | None => {
                    self.last_can_start_block_before_brace = true;
                    self.last_can_end_expr = true;
                }
                Some(BraceKind::ObjectLiteral) => self.end_expression(),
            },
            "." | "?." => {
                self.prev_was_dot = true;
                self.regex_allowed = false;
            }
            ";" | "=>" => self.start_statement(),
            "++" | "--" => self.end_expression(),
            _ => {}
        }
        op
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

struct Export<'a> {
    name: &'a str,
    binding: &'a str,
    from: Option<&'a str>,
}

impl<'a> Export<'a> {
    fn local(name: &'a str) -> Self {
        Self {
            name,
            binding: name,
            from: None,
        }
    }
}

fn transform_module<'a>(name: &str, source: &'a str) -> BundledModule<'a> {
    assert!(
        name.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid bundled runtime module name `{name}`"
    );
    let mut lexer = JsLexer::new(source);
    let mut deps = Vec::new();
    let mut exports = Vec::new();
    let mut imports = String::new();
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
            deps.push(rewrite_import(&mut lexer, name, &mut imports));
            last_emitted = lexer.pos;
            lexer.start_statement();
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
                        let dep = parse_local_module_specifier(lexer.slice(spec_tok.span), name);
                        deps.push(Dependency {
                            name: dep,
                            bindings: specifiers.iter().map(|&(binding, _)| binding).collect(),
                        });
                        consume_optional_semicolon(&mut lexer);
                        exports.extend(specifiers.into_iter().map(|(binding, name)| Export {
                            name,
                            binding,
                            from: Some(dep),
                        }));
                    } else {
                        if follow.kind == TokenKind::Punct(";") {
                            lexer = after;
                        }
                        exports.extend(specifiers.into_iter().map(|(binding, name)| Export {
                            name,
                            binding,
                            from: None,
                        }));
                    }
                    last_emitted = lexer.pos;
                    lexer.start_statement();
                }
                (TokenKind::Ident, "async") => {
                    let fn_tok = look.next_token();
                    assert!(
                        fn_tok.kind == TokenKind::Ident && look.slice(fn_tok.span) == "function",
                        "unsupported `export async` declaration in `{name}`"
                    );
                    let decl_name = parse_function_or_class_name(&mut look, name);
                    exports.push(Export::local(decl_name));
                    last_emitted = next.span.start;
                }
                (TokenKind::Ident, "function" | "class") => {
                    let decl_name = parse_function_or_class_name(&mut look, name);
                    exports.push(Export::local(decl_name));
                    last_emitted = next.span.start;
                }
                (TokenKind::Ident, "const" | "let" | "var") => {
                    let mut names = Vec::new();
                    parse_var_declarator_names(&mut look, name, &mut names);
                    exports.extend(names.into_iter().map(Export::local));
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
    code.push_str(&imports);
    code.push_str(body.trim_matches('\n').trim_end());
    writeln!(code).unwrap();
    code.push_str("return { ");
    let mut exported_names = BTreeSet::new();
    for Export {
        name: exported,
        binding,
        from,
    } in exports
    {
        if !exported_names.is_empty() {
            code.push_str(", ");
        }
        assert!(
            exported_names.insert(exported),
            "duplicate export `{exported}` in `{name}`"
        );
        match from {
            Some(dep) => write!(code, "{exported}: __zkcl_modules.{dep}.{binding}").unwrap(),
            None if exported != binding => write!(code, "{exported}: {binding}").unwrap(),
            None => code.push_str(exported),
        }
    }
    code.push_str(" };\n");
    writeln!(code, "}})();").unwrap();

    BundledModule {
        deps,
        code,
        exports: exported_names,
    }
}

fn rewrite_import<'a>(
    lexer: &mut JsLexer<'a>,
    module_name: &str,
    imports: &mut String,
) -> Dependency<'a> {
    let first = lexer.next_token();
    let mut bindings = Vec::new();
    let dep = match first.kind {
        TokenKind::String => parse_local_module_specifier(lexer.slice(first.span), module_name),
        TokenKind::Punct("*") => {
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
            let dep = parse_from_clause(lexer, module_name);
            let alias = lexer.slice(alias_tok.span);
            writeln!(imports, "const {alias} = __zkcl_modules.{dep};").unwrap();
            dep
        }
        TokenKind::Punct("{") => {
            let specifiers = parse_specifier_list(lexer, module_name);
            let dep = parse_from_clause(lexer, module_name);
            imports.push_str("const { ");
            for (imported, local) in specifiers {
                if !bindings.is_empty() {
                    imports.push_str(", ");
                }
                bindings.push(imported);
                if imported != local {
                    write!(imports, "{imported}: ").unwrap();
                }
                imports.push_str(local);
            }
            writeln!(imports, " }} = __zkcl_modules.{dep};").unwrap();
            dep
        }
        _ => panic!(
            "unsupported `import` form near `{}` in `{module_name}`",
            lexer.slice(first.span)
        ),
    };
    consume_optional_semicolon(lexer);
    Dependency {
        name: dep,
        bindings,
    }
}

fn parse_from_clause<'a>(lexer: &mut JsLexer<'a>, module_name: &str) -> &'a str {
    let from = lexer.next_token();
    assert!(
        from.kind == TokenKind::Ident && lexer.slice(from.span) == "from",
        "expected `from` in `{module_name}`"
    );
    let spec = lexer.next_token();
    assert_eq!(
        spec.kind,
        TokenKind::String,
        "expected module specifier string after `from` in `{module_name}`"
    );
    parse_local_module_specifier(lexer.slice(spec.span), module_name)
}

fn consume_optional_semicolon(lexer: &mut JsLexer<'_>) {
    let mut look = lexer.clone();
    if look.next_token().kind == TokenKind::Punct(";") {
        *lexer = look;
    }
}

fn parse_specifier_list<'a>(lexer: &mut JsLexer<'a>, module_name: &str) -> Vec<(&'a str, &'a str)> {
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
        let source_name = lexer.slice(tok.span);
        let next = lexer.next_token();
        match next.kind {
            TokenKind::Punct("}") => {
                items.push((source_name, source_name));
                break;
            }
            TokenKind::Punct(",") => {
                items.push((source_name, source_name));
            }
            TokenKind::Ident if lexer.slice(next.span) == "as" => {
                let alias_tok = lexer.next_token();
                assert_eq!(
                    alias_tok.kind,
                    TokenKind::Ident,
                    "expected alias identifier after `as` in `{module_name}`"
                );
                let alias_name = lexer.slice(alias_tok.span);
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

fn parse_function_or_class_name<'a>(lexer: &mut JsLexer<'a>, module_name: &str) -> &'a str {
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
    lexer.slice(tok.span)
}

fn parse_var_declarator_names<'a>(
    lexer: &mut JsLexer<'a>,
    module_name: &str,
    exports: &mut Vec<&'a str>,
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

fn parse_binding_pattern<'a>(
    lexer: &mut JsLexer<'a>,
    module_name: &str,
    exports: &mut Vec<&'a str>,
) {
    let tok = lexer.next_token();
    match tok.kind {
        TokenKind::Ident => {
            exports.push(lexer.slice(tok.span));
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
                        exports.push(lexer.slice(item.span));
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

fn parse_local_module_specifier<'a>(raw_string_token: &'a str, module_name: &str) -> &'a str {
    let inner = &raw_string_token[1..raw_string_token.len() - 1];
    assert!(
        !inner.contains('\\'),
        "escaped module specifier `{raw_string_token}` is not supported in `{module_name}`"
    );
    inner
        .strip_prefix("./")
        .and_then(|s| s.strip_suffix(".mjs"))
        .unwrap_or_else(|| {
            panic!("bundled runtime only supports local `./<mod>.mjs` imports, got `{inner}` in `{module_name}`")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_modules_keep_private_names_and_multiline_aliases() {
        let bundled = bundle_modules(
            "sample",
            &[
                (
                    "sample",
                    "import {\n  alpha as localAlpha,\n  beta,\n} from './dep.mjs';\nconst privateFlag = true;\nexport const publicValue = 1;\nexport function publicFn() { return privateFlag; }\n",
                ),
                ("dep", "export const alpha = 10;\nexport const beta = 20;\n"),
            ],
        );
        assert!(bundled.contains("const { alpha: localAlpha, beta } = __zkcl_modules.dep;"));
        assert!(bundled.contains("return { publicValue, publicFn };"));
        assert!(!bundled.contains("privateFlag }"));
        assert!(!bundled.contains("privateFlag,"));
    }

    #[test]
    fn multiline_import_ignores_semicolons_in_comments_and_strings() {
        let bundled = bundle_modules(
            "consumer",
            &[
                (
                    "consumer",
                    "import {\n  // ; bogus ; from './missing.mjs';\n  /* ; another ; comment */\n  one,\n  two as aliasTwo,\n} from './provider.mjs';\nexport const result = one + aliasTwo;\n",
                ),
                ("provider", "export const one = 1;\nexport const two = 2;\n"),
            ],
        );
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
${/}/.test('}')}${base}
export function nope() {}
`;
const re = /export const fake3/;
const div = (10 + 2) / 3;
export async function* streamValues() {
  yield div;
}
export class Box {}
"#;
        let bundled = bundle_modules(
            "demo",
            &[("demo", src), ("base", "export const base = 42;\n")],
        );
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
        let bundled = bundle_modules(
            "facade",
            &[
                (
                    "facade",
                    "export { alpha, beta as renamedBeta } from './inner.mjs';\nconst beta = 999;\nexport function getLocalBeta() { return beta; }\n",
                ),
                ("inner", "export const alpha = 1;\nexport const beta = 2;\n"),
            ],
        );
        let inner_pos = bundled.find("__zkcl_modules.inner =").unwrap();
        let facade_pos = bundled.find("__zkcl_modules.facade =").unwrap();
        assert!(inner_pos < facade_pos);
        assert!(bundled.contains(
            "return { alpha: __zkcl_modules.inner.alpha, renamedBeta: __zkcl_modules.inner.beta, getLocalBeta };"
        ));
    }

    #[test]
    fn bundle_follows_source_order_from_entry_and_hoists_imports() {
        let modules = [
            ("shared", "export const value = 1;"),
            ("alpha", "import './shared.mjs';"),
            ("unused", "import './missing.mjs';"),
            ("zeta", "import './shared.mjs';"),
            (
                "entry",
                "import './zeta.mjs';\nexport const result = value;\nimport './alpha.mjs';\nimport { value } from './shared.mjs';",
            ),
        ];
        let output = bundle_modules("entry", &modules);
        let positions = ["shared", "zeta", "alpha", "entry"]
            .map(|module| output.find(&format!("__zkcl_modules.{module} =")).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(output.matches("__zkcl_modules.shared =").count(), 1);
        assert!(!output.contains("__zkcl_modules.unused"));
        assert!(output.find("const { value }").unwrap() < output.find("const result").unwrap());
        let reversed: Vec<_> = modules.into_iter().rev().collect();
        assert_eq!(output, bundle_modules("entry", &reversed));
    }

    #[test]
    #[should_panic(
        expected = "runtime module `entry` imports `missing`, which `inner` does not export"
    )]
    fn missing_import_fails_linking() {
        bundle_modules(
            "entry",
            &[
                ("entry", "import { missing as value } from './inner.mjs';"),
                ("inner", "const missing = 1;"),
            ],
        );
    }

    #[test]
    #[should_panic(
        expected = "runtime module `entry` imports `missing`, which `inner` does not export"
    )]
    fn missing_reexport_fails_linking() {
        bundle_modules(
            "entry",
            &[
                ("entry", "export { missing as value } from './inner.mjs';"),
                ("inner", "const missing = 1;"),
            ],
        );
    }

    #[test]
    fn export_destructuring_and_asi_declarations_collect_all_bound_names() {
        let bundled = bundle_modules(
            "destr",
            &[(
                "destr",
                "export const { a, b: aliasB, nested: [x, , ...rest] = [1, 2, 3] } = obj\nexport const first = 1, second = 2\n",
            )],
        );
        assert!(bundled.contains("return { a, aliasB, x, rest, first, second };"));
    }
}
