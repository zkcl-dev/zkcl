//! Shared lint context, environment tracking, and ANSI CL semantic predicates.

use std::cell::RefCell;
use std::rc::Rc;

use crate::cl_builtins::{BuiltinKind, builtin_kind, is_external_cl_symbol};
use crate::navigation::ScopeBinding;
use crate::navigation::index::{
    DocumentIndex, MacroOperands, WorkspaceIndex, closed_list, possibly_read, symbol,
};
use crate::navigation::queries::find_local_fn;
use crate::outline::{Element, elements};
use crate::reader_tokens::{
    Prefix, SymbolName, integer_token, is_number_literal_token, is_numeric_token,
    is_radix_number_token, string_literal,
};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange};

use super::macro_template::MacroTemplate;
use super::rules::find_rule_id;
use super::suppressions::SuppressionTable;
use super::{LintCategory, LintConfig, LintDiagnostic, LintFix, LintLevel, LintRelated, LintTag};

pub(crate) struct LintEmitter<'a> {
    pub(crate) config: &'a LintConfig,
    pub(crate) suppressions: &'a mut SuppressionTable,
    pub(crate) diagnostics: Vec<LintDiagnostic>,
}

impl LintEmitter<'_> {
    pub(crate) fn emit(
        &mut self,
        rule_id: &'static str,
        range: TextRange,
        message: impl Into<String>,
        help: Option<String>,
        tags: Vec<LintTag>,
        fix: Option<LintFix>,
    ) {
        self.diagnostic(rule_id, range, message, help, tags, fix);
    }

    pub(crate) fn diagnostic(
        &mut self,
        rule_id: &'static str,
        range: TextRange,
        message: impl Into<String>,
        help: Option<String>,
        tags: Vec<LintTag>,
        fix: Option<LintFix>,
    ) -> Option<&mut LintDiagnostic> {
        let desc = find_rule_id(rule_id)?;
        let level = self.config.level_for(desc);
        if level == LintLevel::Allow {
            return None;
        }
        if self
            .suppressions
            .is_suppressed(desc.id, desc.short_name, desc.category, range)
        {
            return None;
        }
        self.diagnostics.push(LintDiagnostic {
            rule_id: desc.id,
            category: desc.category,
            level,
            range,
            message: message.into(),
            help,
            related: Vec::new(),
            tags,
            fix,
        });
        self.diagnostics.last_mut()
    }

    pub(crate) fn is_enabled(&self, rule_id: &str) -> bool {
        find_rule_id(rule_id).is_some_and(|desc| self.config.level_for(desc) != LintLevel::Allow)
    }

    pub(crate) fn emit_buffered(&mut self, diagnostic: LintDiagnostic) {
        let desc = find_rule_id(diagnostic.rule_id).expect("emitted lint rule");
        if !self.suppressions.is_suppressed(
            desc.id,
            desc.short_name,
            desc.category,
            diagnostic.range,
        ) {
            self.diagnostics.push(diagnostic);
        }
    }

    pub(crate) fn emit_unmatched_suppressions(&mut self) {
        let Some(desc) = find_rule_id("pedantic/stale-suppression") else {
            return;
        };
        let level = self.config.level_for(desc);
        if level == LintLevel::Allow {
            return;
        }
        for entry in self.suppressions.unmatched() {
            self.diagnostics.push(LintDiagnostic {
                rule_id: desc.id,
                category: LintCategory::Pedantic,
                level,
                range: entry.directive_range,
                message: format!(
                    "suppression directive `{}` matched no active lint",
                    entry.rules.join(", ")
                ),
                help: Some("remove the unused suppression comment".into()),
                related: Vec::new(),
                tags: vec![LintTag::Unnecessary],
                fix: None,
            });
        }
    }
}

impl LintDiagnostic {
    pub(crate) fn note(&mut self, range: TextRange, message: impl Into<String>) {
        self.related.push(LintRelated {
            document: None,
            range,
            message: message.into(),
        });
    }
}

#[derive(Clone, Default)]
pub(crate) struct FormContext {
    pub(crate) in_top_level: bool,
    pub(crate) in_defmacro: bool,
    pub(crate) nil_blocks: usize,
    pub(crate) in_loop: bool,
    pub(crate) in_tagbody: bool,
    pub(crate) loop_bindings: Rc<Vec<LoopBinding>>,
    pub(crate) blocks: Rc<Vec<ControlTarget>>,
    pub(crate) tagbody_tags: Rc<Vec<ControlTarget>>,
    pub(crate) read_eval_disabled: bool,
    pub(crate) in_open_guard: bool,
    pub(crate) protected_loop_open: Option<TextRange>,
    pub(crate) in_boolean_test: bool,
    pub(crate) result_discarded: bool,
    pub(crate) entry_test: Option<Rc<BranchFact>>,
    pub(crate) local_fns: Rc<Vec<String>>,
    pub(crate) local_macros: Option<Rc<Vec<LocalMacroTemplate>>>,
    pub(crate) list_declarations: Rc<Vec<(String, TextRange)>>,
    pub(crate) macro_origin: Option<Rc<MacroOrigin>>,
    pub(crate) symbol_macros: bool,
    pub(crate) fn_scope_id: u32,
}

pub(crate) struct MacroOrigin {
    pub(crate) location: LintRelated,
    pub(crate) enclosing: Option<Rc<Self>>,
}

#[derive(Clone)]
pub(crate) struct LocalMacroTemplate {
    pub(crate) definition: TextRange,
    pub(crate) template: Rc<MacroTemplate>,
}

#[derive(Clone)]
pub(crate) struct LoopBinding {
    pub(crate) name: SymbolName,
    pub(crate) definition: TextRange,
    pub(crate) indexed_list: Option<Rc<IndexedListLoop>>,
}

pub(crate) struct IndexedListLoop {
    pub(crate) source: SymbolName,
    pub(crate) source_definition: TextRange,
    pub(crate) length: TextRange,
    pub(crate) rewrite: Option<(TextRange, LintFix)>,
}

pub(crate) struct BranchFact {
    pub(crate) test: String,
    pub(crate) range: TextRange,
    pub(crate) truth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ControlTarget {
    Symbol(String, Option<String>),
    Integer(String),
}

impl FormContext {
    pub(crate) fn top_level() -> Self {
        Self {
            in_top_level: true,
            ..Self::default()
        }
    }

    pub(crate) fn nested_expr(&self, in_boolean_test: bool, result_discarded: bool) -> Self {
        Self {
            in_top_level: false,
            in_tagbody: false,
            in_boolean_test,
            result_discarded,
            entry_test: None,
            ..self.clone()
        }
    }

    pub(crate) fn function_body(&self) -> Self {
        let mut body = self.nested_expr(false, false);
        body.in_loop = false;
        if !body.loop_bindings.is_empty() {
            body.loop_bindings = Rc::default();
        }
        body
    }

    pub(crate) fn push_block(&mut self, target: ControlTarget) {
        Rc::make_mut(&mut self.blocks).push(target);
    }

    pub(crate) fn push_nil_block(&mut self) {
        self.nil_blocks += 1;
        self.push_block(ControlTarget::Symbol(
            "NIL".into(),
            Some("COMMON-LISP".into()),
        ));
    }

    pub(crate) fn is_fn_shadowed(&self, sym: &SymbolName) -> bool {
        matches!(sym.prefix, Prefix::None) && self.local_fns.iter().any(|f| f == &sym.name)
    }
}

pub(crate) struct LintDocument<'a> {
    pub(crate) text: &'a str,
    pub(crate) root: &'a SyntaxNode,
    pub(crate) source: &'a DocumentIndex,
    pub(crate) workspace: &'a WorkspaceIndex<'a>,
    pub(crate) macro_operands: RefCell<MacroOperands>,
}

impl LintDocument<'_> {
    pub(crate) fn unqualified_standard(
        &self,
        name: &str,
        package: Option<&str>,
        lexicals: &[ScopeBinding],
        env: &FormContext,
    ) -> bool {
        let name = SymbolName {
            prefix: Prefix::None,
            name: name.to_ascii_uppercase(),
        };
        self.std_op(&name, package, lexicals, env).is_some()
    }

    pub(crate) fn setf_name(&self, el: &Element, package: Option<&str>) -> Option<SymbolName> {
        let node = el.as_node().filter(|node| closed_list(node))?;
        let parts = self.read_elements(elements(node))?;
        let [setf, name] = parts.as_slice() else {
            return None;
        };
        (self.workspace.standard_symbol(&symbol(setf)?, package) == Some("SETF"))
            .then(|| symbol(name))?
    }

    pub(crate) fn control_name(
        &self,
        name: SymbolName,
        package: Option<&str>,
    ) -> Option<ControlTarget> {
        let home = match &name.prefix {
            Prefix::Uninterned => return None,
            _ if self.workspace.standard_symbol(&name, package) == Some("NIL") => {
                Some("COMMON-LISP")
            }
            Prefix::None => self
                .workspace
                .symbol_home_package(&name, package)
                .or(package),
            _ => Some(self.workspace.symbol_home_package(&name, package)?),
        };
        let home = home.map(str::to_owned);
        Some(ControlTarget::Symbol(name.name, home))
    }

    pub(crate) fn control_tag(&self, el: &Element, package: Option<&str>) -> Option<ControlTarget> {
        if let Some(sym) = symbol(el) {
            return self.control_name(sym, package);
        }
        Some(ControlTarget::Integer(integer_token(
            el.as_token()?.text(),
        )?))
    }

    pub(crate) fn control_block(
        &self,
        el: &Element,
        package: Option<&str>,
    ) -> Option<ControlTarget> {
        if is_nil_syntax(el, package, self.workspace) {
            return Some(ControlTarget::Symbol(
                "NIL".into(),
                Some("COMMON-LISP".into()),
            ));
        }
        self.control_name(symbol(el)?, package)
    }
    pub(crate) fn std_op<'s>(
        &self,
        sym: &'s SymbolName,
        package: Option<&str>,
        lexicals: &[ScopeBinding],
        env: &FormContext,
    ) -> Option<&'s str> {
        if env.is_fn_shadowed(sym)
            || find_local_fn(lexicals, sym, package, self.workspace).is_some()
        {
            return None;
        }
        self.workspace.standard_symbol(sym, package)
    }

    pub(crate) fn resolve_head(
        &self,
        list: &SyntaxNode,
        package: Option<&str>,
        lexicals: &[ScopeBinding],
        env: &FormContext,
    ) -> Option<(SymbolName, Vec<Element>, Option<String>)> {
        if !closed_list(list) {
            return None;
        }
        let known_features = self.source.known_features();
        let mut iter = elements(list).filter_map(|el| possibly_read(el, known_features));
        let (head_el, head_known) = iter.next()?;
        if !head_known {
            return None;
        }
        let head_sym = symbol(&head_el)?;
        let mut args = Vec::new();
        for (arg, known) in iter {
            if !known {
                return None;
            }
            args.push(arg);
        }
        let std_name = self
            .std_op(&head_sym, package, lexicals, env)
            .map(str::to_owned);
        Some((head_sym, args, std_name))
    }

    /// Read known feature conditionals and preserve uncertainty about positions.
    pub(crate) fn read_elements(
        &self,
        parts: impl IntoIterator<Item = Element>,
    ) -> Option<Vec<Element>> {
        let mut out = Vec::new();
        self.read_elements_into(parts, &mut out).then_some(out)
    }

    pub(crate) fn read_elements_into(
        &self,
        parts: impl IntoIterator<Item = Element>,
        out: &mut Vec<Element>,
    ) -> bool {
        out.clear();
        let known_features = self.source.known_features();
        for el in parts {
            if let Some((el, known)) = possibly_read(el, known_features) {
                if !known {
                    return false;
                }
                out.push(el);
            }
        }
        true
    }

    pub(crate) fn text_of(&self, range: TextRange) -> &str {
        &self.text[usize::from(range.start())..usize::from(range.end())]
    }
}

pub(crate) fn is_nil_element(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> bool {
    let quoted = unwrap_quote(el, package, workspace);
    is_nil_syntax(quoted.as_ref().unwrap_or(el), package, workspace)
}

pub(crate) fn is_nil_syntax(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> bool {
    if let Some(sym) = symbol(el) {
        return workspace.standard_symbol(&sym, package) == Some("NIL");
    }
    el.as_node()
        .is_some_and(|node| closed_list(node) && elements(node).next().is_none())
}

pub(crate) fn is_t_element(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> bool {
    let quoted = unwrap_quote(el, package, workspace);
    symbol(quoted.as_ref().unwrap_or(el))
        .is_some_and(|name| workspace.standard_symbol(&name, package) == Some("T"))
}

pub(crate) fn unwrap_quote(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> Option<Element> {
    let node = el.as_node()?;
    if node.kind() == SyntaxKind::ReaderForm
        && node
            .first_token()
            .is_some_and(|t| t.kind() == SyntaxKind::Quote)
    {
        return elements(node).nth(1);
    }
    if node.kind() == SyntaxKind::List && closed_list(node) {
        let mut parts = elements(node);
        let head = parts.next().and_then(|e| symbol(&e))?;
        if workspace.standard_symbol(&head, package) == Some("QUOTE") {
            let inner = parts.next()?;
            if parts.next().is_none() {
                return Some(inner);
            }
        }
    }
    None
}

pub(crate) fn is_function_quote(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> Option<Element> {
    let node = el.as_node()?;
    if node.kind() == SyntaxKind::ReaderForm
        && node
            .first_token()
            .is_some_and(|t| t.kind() == SyntaxKind::FunctionQuote)
    {
        return elements(node).nth(1);
    }
    if node.kind() == SyntaxKind::List && closed_list(node) {
        let mut parts = elements(node);
        let head = parts.next().and_then(|e| symbol(&e))?;
        if workspace.standard_symbol(&head, package) == Some("FUNCTION") {
            let inner = parts.next()?;
            if parts.next().is_none() {
                return Some(inner);
            }
        }
    }
    None
}

pub(crate) fn is_string_literal_el(el: &Element) -> bool {
    el.as_token()
        .is_some_and(|t| matches!(t.kind(), SyntaxKind::String | SyntaxKind::RawString))
}

pub(crate) fn string_literal_value(el: &Element) -> Option<String> {
    let tok = el.as_token()?;
    matches!(tok.kind(), SyntaxKind::String | SyntaxKind::RawString).then(|| string_literal(tok))?
}

pub(crate) fn is_char_literal_el(el: &Element) -> bool {
    el.as_token()
        .is_some_and(|t| t.kind() == SyntaxKind::Character)
}

pub(crate) fn is_number_literal_el(el: &Element) -> bool {
    el.as_token()
        .is_some_and(|t| t.kind() == SyntaxKind::Atom && is_number_literal_token(t.text()))
}

pub(crate) fn numeric_literal_is_zero(el: &Element) -> bool {
    let Some(tok) = el.as_token() else {
        return false;
    };
    if tok.kind() != SyntaxKind::Atom {
        return false;
    }
    let text = tok.text();
    if let Ok(v) = text.parse::<i64>() {
        return v == 0;
    }
    if is_numeric_token(text) {
        if let Some((num, den)) = text.split_once('/') {
            let num = num.trim_start_matches(['+', '-']);
            let den = den.trim_start_matches(['+', '-']);
            return !num.is_empty()
                && num.bytes().all(|b| b == b'0')
                && !den.is_empty()
                && den.bytes().any(|b| matches!(b, b'1'..=b'9'));
        }
        let lower = text.to_ascii_lowercase();
        let mantissa = lower
            .split(['d', 'e', 'f', 'l', 's'])
            .next()
            .unwrap_or(&lower)
            .trim_start_matches(['+', '-']);
        return !mantissa.is_empty()
            && mantissa.bytes().all(|b| b == b'0' || b == b'.')
            && mantissa.bytes().any(|b| b == b'0');
    }
    if is_radix_number_token(text)
        && let Some((_, digits)) = text.split_once(['r', 'R', 'x', 'X', 'o', 'O', 'b', 'B'])
    {
        let digits = digits.trim_start_matches(['+', '-']);
        return !digits.is_empty() && digits.bytes().all(|b| b == b'0');
    }
    false
}

pub(crate) fn integer_literal_value(el: &Element) -> Option<i64> {
    let tok = el.as_token()?;
    (tok.kind() == SyntaxKind::Atom)
        .then(|| integer_token(tok.text()).and_then(|s| s.parse::<i64>().ok()))
        .flatten()
}

pub(crate) fn is_literal_compound(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> bool {
    if is_string_literal_el(el) {
        return true;
    }
    if el.as_node().is_some_and(|n| n.kind() == SyntaxKind::Vector) {
        return true;
    }
    if let Some(inner) = unwrap_quote(el, package, workspace) {
        return inner
            .as_node()
            .is_some_and(|n| matches!(n.kind(), SyntaxKind::List | SyntaxKind::Vector))
            || is_string_literal_el(&inner);
    }
    false
}

pub(crate) fn is_strict_boolean_expr(
    ctx: &LintDocument<'_>,
    el: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> bool {
    if is_nil_element(el, package, ctx.workspace) || is_t_element(el, package, ctx.workspace) {
        return true;
    }
    let Some(node) = el.as_node() else {
        return false;
    };
    if node.kind() != SyntaxKind::List || !closed_list(node) {
        return false;
    }
    let Some(head) = elements(node).next().and_then(|e| symbol(&e)) else {
        return false;
    };
    matches!(
        ctx.std_op(&head, package, lexicals, env),
        Some("NOT" | "NULL")
    )
}

pub(crate) fn is_pure_atom(el: &Element) -> bool {
    if let Some(tok) = el.as_token() {
        return matches!(
            tok.kind(),
            SyntaxKind::Atom | SyntaxKind::String | SyntaxKind::RawString | SyntaxKind::Character
        );
    }
    if let Some(node) = el.as_node() {
        if node.kind() == SyntaxKind::Vector {
            return true;
        }
        if node.kind() == SyntaxKind::ReaderForm
            && node
                .first_token()
                .is_some_and(|t| matches!(t.kind(), SyntaxKind::Quote | SyntaxKind::FunctionQuote))
        {
            return true;
        }
    }
    false
}

pub(crate) fn is_pure_expr(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> bool {
    if is_pure_atom(el) {
        return true;
    }
    let Some(node) = el.as_node() else {
        return false;
    };
    if unwrap_quote(el, package, workspace).is_some() {
        return true;
    }
    if node.kind() != SyntaxKind::List || !closed_list(node) {
        return false;
    }
    let mut parts = elements(node);
    let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
        return false;
    };
    let Some(std_op) = workspace.standard_symbol(&head, package) else {
        return false;
    };
    let is_pure_op = matches!(
        std_op,
        "+" | "-"
            | "*"
            | "/"
            | "1+"
            | "1-"
            | "ABS"
            | "MIN"
            | "MAX"
            | "MOD"
            | "REM"
            | "FLOOR"
            | "CEILING"
            | "TRUNCATE"
            | "ROUND"
            | "NOT"
            | "NULL"
            | "EQ"
            | "EQL"
            | "EQUAL"
            | "EQUALP"
            | "="
            | "/="
            | "<"
            | "<="
            | ">"
            | ">="
            | "ZEROP"
            | "PLUSP"
            | "MINUSP"
            | "EVENP"
            | "ODDP"
            | "CONSP"
            | "ATOM"
            | "LISTP"
            | "NUMBERP"
            | "STRINGP"
            | "SYMBOLP"
            | "CHARACTERP"
            | "VECTORP"
            | "ARRAYP"
            | "TYPEP"
            | "CAR"
            | "CDR"
            | "FIRST"
            | "SECOND"
            | "THIRD"
            | "FOURTH"
            | "REST"
            | "CONS"
            | "LIST"
            | "LIST*"
            | "VECTOR"
            | "LENGTH"
            | "GETHASH"
            | "AREF"
            | "SVREF"
            | "CHAR"
            | "SCHAR"
            | "ELT"
            | "NTH"
            | "STRING="
            | "STRING-EQUAL"
            | "CHAR="
            | "CHAR-EQUAL"
            | "SUBSEQ"
            | "APPEND"
            | "REVERSE"
            | "COPY-LIST"
            | "COPY-SEQ"
            | "MAKE-LIST"
            | "MAKE-STRING"
            | "LAST"
            | "BUTLAST"
            | "NTHCDR"
            | "CONCATENATE"
            | "STRING-UPCASE"
            | "STRING-DOWNCASE"
            | "STRING-TRIM"
            | "SYMBOL-NAME"
            | "KEYWORDP"
            | "RATIONALP"
            | "FLOATP"
            | "REALP"
            | "COMPLEXP"
            | "BIT-VECTOR-P"
            | "SIMPLE-VECTOR-P"
            | "HASH-TABLE-P"
            | "FUNCTIONP"
            | "PACKAGEP"
            | "PATHNAMEP"
            | "STREAMP"
    );
    is_pure_op && parts.all(|arg| is_pure_expr(&arg, package, workspace))
}

pub(crate) fn is_destructive_sequence_op(name: &str) -> bool {
    matches!(
        name,
        "SORT"
            | "STABLE-SORT"
            | "DELETE"
            | "DELETE-IF"
            | "DELETE-IF-NOT"
            | "DELETE-DUPLICATES"
            | "NREVERSE"
            | "NRECONC"
            | "NCONC"
            | "NSUBST"
            | "NSUBST-IF"
            | "NSUBST-IF-NOT"
            | "NSUBSTITUTE"
            | "NSUBSTITUTE-IF"
            | "NSUBSTITUTE-IF-NOT"
            | "NUNION"
            | "NINTERSECTION"
            | "NSET-DIFFERENCE"
            | "NSET-EXCLUSIVE-OR"
    )
}

pub(crate) fn is_literal_mutating_op(name: &str) -> bool {
    is_destructive_sequence_op(name)
        || matches!(
            name,
            "RPLACA" | "RPLACD" | "NSTRING-UPCASE" | "NSTRING-DOWNCASE" | "NSTRING-CAPITALIZE"
        )
}

pub(crate) fn normalized_sexpr(el: &Element) -> String {
    let mut out = String::new();
    push_normalized(el, &mut out);
    out
}

/// A key for tests that can be evaluated again without an intervening form.
/// A reader form, symbol macro, arbitrary call, or type-dependent operation
/// may signal, allocate, run user code or depend on mutable state.
pub(crate) fn repeatable_test(
    ctx: &LintDocument<'_>,
    el: &Element,
    package: Option<&str>,
    lexicals: &[ScopeBinding],
    env: &FormContext,
) -> Option<String> {
    fn encode(
        ctx: &LintDocument<'_>,
        el: &Element,
        package: Option<&str>,
        lexicals: &[ScopeBinding],
        env: &FormContext,
        key: &mut String,
    ) -> Option<()> {
        use std::fmt::Write;
        if let Some(token) = el.as_token() {
            if token.kind() == SyntaxKind::Atom {
                if let Some(name) = symbol(el) {
                    if name.prefix == Prefix::Uninterned
                        || (name.prefix != Prefix::Keyword
                            && (env.symbol_macros || ctx.workspace.is_symbol_macro(&name, package)))
                    {
                        return None;
                    }
                    match &name.prefix {
                        Prefix::None => {
                            key.push('N');
                            key.push(':');
                            key.push_str(&name.name);
                        }
                        Prefix::Keyword => {
                            key.push('K');
                            key.push(':');
                            key.push_str(&name.name);
                        }
                        Prefix::Package {
                            name: pkg,
                            external,
                        } => {
                            key.push(if *external { 'E' } else { 'I' });
                            key.push_str(pkg);
                            key.push(':');
                            key.push_str(&name.name);
                        }
                        Prefix::Uninterned => return None,
                    }
                    return Some(());
                }
                let text = token.text();
                write!(key, "{}:{text}", text.len()).ok()?;
                return Some(());
            }
            // Identically spelled string literals can denote distinct objects.
            if token.kind() != SyntaxKind::Character {
                return None;
            }
            let text = token.text();
            write!(key, "{}:{text}", text.len()).ok()?;
            return Some(());
        }
        let node = el.as_node().filter(|node| closed_list(node))?;
        let mut parts = elements(node);
        let head = symbol(&parts.next()?)?;
        let op = ctx.std_op(&head, package, lexicals, env)?;
        let arity = match op {
            "NOT" | "NULL" | "CONSP" | "ATOM" | "LISTP" | "NUMBERP" | "INTEGERP" | "REALP"
            | "STRINGP" | "SYMBOLP" | "CHARACTERP" | "VECTORP" | "ARRAYP" | "FUNCTIONP" => 1,
            "EQL" => 2,
            _ => return None,
        };
        write!(key, "({op}:").ok()?;
        for _ in 0..arity {
            encode(ctx, &parts.next()?, package, lexicals, env, key)?;
        }
        if parts.next().is_some() {
            return None;
        }
        key.push(')');
        Some(())
    }
    let mut key = String::new();
    encode(ctx, el, package, lexicals, env, &mut key)?;
    Some(key)
}

fn push_normalized(el: &Element, out: &mut String) {
    match el {
        Element::Token(tok) => match tok.kind() {
            SyntaxKind::Whitespace | SyntaxKind::LineComment | SyntaxKind::BlockComment => {}
            SyntaxKind::Atom => {
                if let Some(sym) = SymbolName::parse(tok.text()) {
                    match sym.prefix {
                        Prefix::None => out.push_str(&sym.name),
                        Prefix::Keyword => {
                            out.push(':');
                            out.push_str(&sym.name);
                        }
                        Prefix::Uninterned => {
                            out.push_str("#:");
                            out.push_str(&sym.name);
                        }
                        Prefix::Package { name, external } => {
                            out.push_str(&name);
                            out.push_str(if external { ":" } else { "::" });
                            out.push_str(&sym.name);
                        }
                    }
                } else {
                    out.push_str(tok.text());
                }
            }
            _ => out.push_str(tok.text()),
        },
        Element::Node(node) => match node.kind() {
            SyntaxKind::List => {
                out.push('(');
                let mut first = true;
                for child in elements(node) {
                    if !first {
                        out.push(' ');
                    }
                    first = false;
                    push_normalized(&child, out);
                }
                out.push(')');
            }
            SyntaxKind::Vector => {
                out.push_str("#(");
                let mut first = true;
                for child in elements(node) {
                    if !first {
                        out.push(' ');
                    }
                    first = false;
                    push_normalized(&child, out);
                }
                out.push(')');
            }
            _ => {
                for child in elements(node) {
                    push_normalized(&child, out);
                }
            }
        },
    }
}

/// ANSI Common Lisp standard type & condition subsumption lattice.
pub(crate) fn is_cl_subtype(sub: &str, sup: &str) -> bool {
    if sub == sup || sup == "T" || sub == "NIL" {
        return true;
    }
    if sub == "T" || sup == "NIL" {
        return false;
    }
    let mut stack = vec![sub];
    let mut visited = Vec::new();
    while let Some(cur) = stack.pop() {
        if cur == sup {
            return true;
        }
        if visited.contains(&cur) {
            continue;
        }
        visited.push(cur);
        for parent in cl_direct_supertypes(cur) {
            stack.push(parent);
        }
    }
    false
}

fn cl_direct_supertypes(ty: &str) -> &'static [&'static str] {
    match ty {
        "BIT" => &["FIXNUM", "UNSIGNED-BYTE"],
        "UNSIGNED-BYTE" | "SIGNED-BYTE" => &["INTEGER"],
        "FIXNUM" | "BIGNUM" => &["INTEGER"],
        "INTEGER" | "RATIO" => &["RATIONAL"],
        "RATIONAL" => &["REAL"],
        "SHORT-FLOAT" | "SINGLE-FLOAT" | "DOUBLE-FLOAT" | "LONG-FLOAT" => &["FLOAT"],
        "FLOAT" => &["REAL"],
        "REAL" | "COMPLEX" => &["NUMBER"],
        "NUMBER" => &["ATOM"],

        "STANDARD-CHAR" => &["BASE-CHAR"],
        "BASE-CHAR" | "EXTENDED-CHAR" => &["CHARACTER"],
        "CHARACTER" => &["ATOM"],
        "KEYWORD" | "BOOLEAN" => &["SYMBOL"],
        "SYMBOL" => &["ATOM"],

        "NULL" => &["SYMBOL", "LIST"],
        "CONS" => &["LIST"],
        "LIST" => &["SEQUENCE"],
        "SIMPLE-BASE-STRING" => &["BASE-STRING", "SIMPLE-STRING"],
        "BASE-STRING" => &["STRING"],
        "SIMPLE-STRING" => &["STRING", "SIMPLE-ARRAY"],
        "STRING" => &["VECTOR"],
        "SIMPLE-BIT-VECTOR" => &["BIT-VECTOR", "SIMPLE-ARRAY"],
        "BIT-VECTOR" => &["VECTOR"],
        "SIMPLE-VECTOR" => &["VECTOR", "SIMPLE-ARRAY"],
        "VECTOR" => &["ARRAY", "SEQUENCE"],
        "SIMPLE-ARRAY" => &["ARRAY"],
        "ARRAY" | "SEQUENCE" => &["ATOM"],

        "COMPILED-FUNCTION" | "GENERIC-FUNCTION" => &["FUNCTION"],
        "STANDARD-GENERIC-FUNCTION" => &["GENERIC-FUNCTION"],
        "FILE-STREAM"
        | "STRING-STREAM"
        | "BROADCAST-STREAM"
        | "CONCATENATED-STREAM"
        | "ECHO-STREAM"
        | "SYNONYM-STREAM"
        | "TWO-WAY-STREAM" => &["STREAM"],
        "LOGICAL-PATHNAME" => &["PATHNAME"],

        "STYLE-WARNING" => &["WARNING"],
        "SIMPLE-WARNING" => &["WARNING", "SIMPLE-CONDITION"],
        "WARNING" => &["CONDITION"],
        "SIMPLE-ERROR" => &["ERROR", "SIMPLE-CONDITION"],
        "SIMPLE-TYPE-ERROR" => &["TYPE-ERROR", "SIMPLE-CONDITION"],
        "DIVISION-BY-ZERO"
        | "FLOATING-POINT-INEXACT"
        | "FLOATING-POINT-INVALID-OPERATION"
        | "FLOATING-POINT-OVERFLOW"
        | "FLOATING-POINT-UNDERFLOW" => &["ARITHMETIC-ERROR"],
        "UNBOUND-SLOT" | "UNBOUND-VARIABLE" | "UNDEFINED-FUNCTION" => &["CELL-ERROR"],
        "END-OF-FILE" => &["STREAM-ERROR"],
        "ARITHMETIC-ERROR" | "CELL-ERROR" | "CONTROL-ERROR" | "FILE-ERROR" | "PACKAGE-ERROR"
        | "PARSE-ERROR" | "PRINT-NOT-READABLE" | "PROGRAM-ERROR" | "READER-ERROR"
        | "STREAM-ERROR" | "TYPE-ERROR" => &["ERROR"],
        "ERROR" | "STORAGE-CONDITION" => &["SERIOUS-CONDITION"],
        "SERIOUS-CONDITION" | "SIMPLE-CONDITION" => &["CONDITION"],

        _ => &[],
    }
}

/// Checks whether type/condition specifier `sub` is statically subsumed by `sup`,
/// supporting standard CL atomic types as well as `(or ...)`, `(and ...)`, `(eql ...)`,
/// `(member ...)`, and restricted numeric/array/sequence type specifiers.
pub(crate) fn is_cl_type_subsumed(
    sub: &Element,
    sup: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> bool {
    is_cl_type_subsumed_depth(sub, sup, package, workspace, 0)
}

fn is_cl_type_subsumed_depth(
    sub: &Element,
    sup: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
    depth: usize,
) -> bool {
    if depth >= 8 {
        return false;
    }
    let next_depth = depth + 1;

    if let Some(sup_sym) = symbol(sup)
        && let Some(sup_std) = workspace.standard_symbol(&sup_sym, package)
    {
        if sup_std == "T" {
            return true;
        }
        if let Some(sub_sym) = symbol(sub)
            && let Some(sub_std) = workspace.standard_symbol(&sub_sym, package)
        {
            return is_cl_subtype(sub_std, sup_std);
        }
    }

    if let Some((sup_head, sup_args)) = compound_type_parts(sup, package, workspace) {
        match sup_head.as_str() {
            "OR" => {
                return sup_args.iter().any(|arm| {
                    is_cl_type_subsumed_depth(sub, arm, package, workspace, next_depth)
                });
            }
            "AND" => {
                return !sup_args.is_empty()
                    && sup_args.iter().all(|arm| {
                        is_cl_type_subsumed_depth(sub, arm, package, workspace, next_depth)
                    });
            }
            "EQL" if sup_args.len() == 1 => {
                if let Some((sub_head, sub_args)) = compound_type_parts(sub, package, workspace)
                    && sub_head == "EQL"
                    && sub_args.len() == 1
                {
                    return same_eql_literal(&sub_args[0], &sup_args[0], package, workspace);
                }
                if symbol(sub)
                    .is_some_and(|s| workspace.standard_symbol(&s, package) == Some("NULL"))
                {
                    return is_nil_syntax(&sup_args[0], package, workspace);
                }
                return false;
            }
            _ => {}
        }
    }

    if let Some((sub_head, sub_args)) = compound_type_parts(sub, package, workspace) {
        match sub_head.as_str() {
            "OR" => {
                return !sub_args.is_empty()
                    && sub_args.iter().all(|arm| {
                        is_cl_type_subsumed_depth(arm, sup, package, workspace, next_depth)
                    });
            }
            "AND" => {
                return sub_args.iter().any(|arm| {
                    is_cl_type_subsumed_depth(arm, sup, package, workspace, next_depth)
                });
            }
            "EQL" if sub_args.len() == 1 => {
                if let Some(ty) = literal_cl_type(&sub_args[0], package, workspace)
                    && let Some(sup_sym) = symbol(sup)
                    && let Some(sup_std) = workspace.standard_symbol(&sup_sym, package)
                {
                    return is_cl_subtype(ty, sup_std);
                }
                return false;
            }
            "MEMBER" => {
                let Some(sup_sym) = symbol(sup) else {
                    return false;
                };
                let Some(sup_std) = workspace.standard_symbol(&sup_sym, package) else {
                    return false;
                };
                return !sub_args.is_empty()
                    && sub_args.iter().all(|member| {
                        literal_cl_type(member, package, workspace)
                            .is_some_and(|ty| is_cl_subtype(ty, sup_std))
                    });
            }
            "INTEGER" | "RATIONAL" | "REAL" | "FLOAT" | "SHORT-FLOAT" | "SINGLE-FLOAT"
            | "DOUBLE-FLOAT" | "LONG-FLOAT" | "COMPLEX" | "ARRAY" | "SIMPLE-ARRAY" | "VECTOR"
            | "SIMPLE-VECTOR" | "STRING" | "SIMPLE-STRING" | "BASE-STRING"
            | "SIMPLE-BASE-STRING" | "BIT-VECTOR" | "SIMPLE-BIT-VECTOR" => {
                if let Some(sup_sym) = symbol(sup)
                    && let Some(sup_std) = workspace.standard_symbol(&sup_sym, package)
                {
                    return is_cl_subtype(&sub_head, sup_std);
                }
            }
            _ => {}
        }
    }

    false
}

fn compound_type_parts(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> Option<(String, Vec<Element>)> {
    let node = el.as_node().filter(|n| closed_list(n))?;
    let mut parts = elements(node);
    let head = symbol(&parts.next()?)?;
    let std = workspace.standard_symbol(&head, package)?.to_owned();
    Some((std, parts.collect()))
}

fn literal_cl_type(
    el: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> Option<&'static str> {
    if is_nil_syntax(el, package, workspace) {
        return Some("NULL");
    }
    if is_char_literal_el(el) {
        return Some("CHARACTER");
    }
    if is_string_literal_el(el) {
        return Some("SIMPLE-STRING");
    }
    if let Some(n) = integer_literal_value(el) {
        return Some(if (-32768..=32767).contains(&n) {
            "FIXNUM"
        } else {
            "INTEGER"
        });
    }
    if is_number_literal_el(el) {
        return Some("NUMBER");
    }
    if let Some(sym) = symbol(el) {
        if sym.prefix == Prefix::Keyword {
            return Some("KEYWORD");
        }
        if sym.prefix != Prefix::Uninterned {
            return Some("SYMBOL");
        }
    }
    None
}

fn same_eql_literal(
    a: &Element,
    b: &Element,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> bool {
    if is_nil_syntax(a, package, workspace) && is_nil_syntax(b, package, workspace) {
        return true;
    }
    if let (Some(ia), Some(ib)) = (integer_literal_value(a), integer_literal_value(b)) {
        return ia == ib;
    }
    if let (Some(sa), Some(sb)) = (symbol(a), symbol(b))
        && sa.prefix != Prefix::Uninterned
        && sb.prefix != Prefix::Uninterned
        && sa.name == sb.name
    {
        return workspace.symbol_home_package(&sa, package)
            == workspace.symbol_home_package(&sb, package)
            && workspace.symbol_home_package(&sa, package).is_some();
    }
    is_char_literal_el(a) && is_char_literal_el(b) && normalized_sexpr(a) == normalized_sexpr(b)
}

/// Returns `true` when two standard unary predicates cannot both hold for the same object.
pub(crate) fn cl_predicates_disjoint(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    let (x, y) = if a < b { (a, b) } else { (b, a) };
    matches!(
        (x, y),
        ("ATOM", "CONSP")
            | ("CONSP", "NULL")
            | ("CONSP", "SYMBOLP")
            | ("CONSP", "NUMBERP")
            | ("CONSP", "INTEGERP")
            | ("CONSP", "REALP")
            | ("CONSP", "CHARACTERP")
            | ("CONSP", "STRINGP")
            | ("CONSP", "VECTORP")
            | ("CONSP", "ARRAYP")
            | ("CONSP", "FUNCTIONP")
            | ("CONSP", "HASH-TABLE-P")
            | ("CONSP", "PACKAGEP")
            | ("CONSP", "STREAMP")
            | ("CONSP", "PATHNAMEP")
            | ("LISTP", "NUMBERP")
            | ("LISTP", "INTEGERP")
            | ("LISTP", "REALP")
            | ("LISTP", "CHARACTERP")
            | ("LISTP", "STRINGP")
            | ("LISTP", "VECTORP")
            | ("LISTP", "ARRAYP")
            | ("LISTP", "FUNCTIONP")
            | ("LISTP", "HASH-TABLE-P")
            | ("LISTP", "PACKAGEP")
            | ("LISTP", "STREAMP")
            | ("LISTP", "PATHNAMEP")
            | ("NULL", "STRINGP")
            | ("NULL", "VECTORP")
            | ("NULL", "ARRAYP")
            | ("NULL", "NUMBERP")
            | ("NULL", "INTEGERP")
            | ("NULL", "REALP")
            | ("NULL", "CHARACTERP")
            | ("NULL", "KEYWORDP")
            | ("NULL", "FUNCTIONP")
            | ("NULL", "HASH-TABLE-P")
            | ("NULL", "PACKAGEP")
            | ("NULL", "STREAMP")
            | ("NULL", "PATHNAMEP")
            | ("NUMBERP", "SYMBOLP")
            | ("INTEGERP", "SYMBOLP")
            | ("REALP", "SYMBOLP")
            | ("CHARACTERP", "SYMBOLP")
            | ("STRINGP", "SYMBOLP")
            | ("SYMBOLP", "VECTORP")
            | ("ARRAYP", "SYMBOLP")
            | ("FUNCTIONP", "SYMBOLP")
            | ("CHARACTERP", "NUMBERP")
            | ("CHARACTERP", "INTEGERP")
            | ("CHARACTERP", "STRINGP")
            | ("CHARACTERP", "VECTORP")
            | ("NUMBERP", "STRINGP")
            | ("INTEGERP", "STRINGP")
            | ("NUMBERP", "VECTORP")
            | ("ARRAYP", "NUMBERP")
            | ("FUNCTIONP", "NUMBERP")
            | ("FUNCTIONP", "STRINGP")
            | ("EVENP", "ODDP")
            | ("MINUSP", "PLUSP")
            | ("MINUSP", "ZEROP")
            | ("PLUSP", "ZEROP")
    )
}

pub(crate) fn is_macro_or_special_builtin(name: &str) -> bool {
    is_external_cl_symbol(name)
        && matches!(
            builtin_kind(name),
            Some(BuiltinKind::Macro | BuiltinKind::SpecialOperator)
        )
}
