//! ANSI binding and clause syntax. Only value positions become executable forms.

use rustc_hash::FxHashMap;

use crate::navigation::index::{closed_list, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SyntaxKind, TextRange};

use super::super::context::{LintDocument, LintEmitter, is_nil_syntax};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LambdaListKind {
    Ordinary,
    Macro,
    Destructuring,
    Specialized,
    Generic,
}

impl LambdaListKind {
    fn destructures(self) -> bool {
        matches!(self, Self::Macro | Self::Destructuring)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Section {
    Required,
    Optional,
    Rest,
    Key,
    AllowOtherKeys,
    Aux,
}

pub(super) fn lambda_initforms(
    ctx: &LintDocument<'_>,
    params: &Element,
    kind: LambdaListKind,
    emit: &mut LintEmitter<'_>,
    cancelled: &impl Fn() -> bool,
) -> Option<Vec<Element>> {
    let mut analysis = LambdaAnalysis {
        ctx,
        package: ctx.source.package_at(params.text_range().start()),
        emit,
        bindings: Bindings::default(),
        initforms: Vec::new(),
        cancelled,
    };
    analysis.list(params, kind)?;
    Some(analysis.initforms)
}

struct LambdaAnalysis<'a, 'src, 'emit, 'config, F> {
    ctx: &'a LintDocument<'src>,
    package: Option<&'a str>,
    emit: &'emit mut LintEmitter<'config>,
    bindings: Bindings,
    initforms: Vec<Element>,
    cancelled: &'a F,
}

impl<F: Fn() -> bool> LambdaAnalysis<'_, '_, '_, '_, F> {
    fn list(&mut self, list: &Element, kind: LambdaListKind) -> Option<()> {
        if (self.cancelled)() {
            return None;
        }
        if is_nil_syntax(list, self.package, self.ctx.workspace) {
            return Some(());
        }
        let Some(node) = list
            .as_node()
            .filter(|node| node.kind() == SyntaxKind::List)
        else {
            self.invalid(list, "expected a lambda list");
            return Some(());
        };
        if !node
            .last_child_or_token()
            .is_some_and(|end| end.kind() == SyntaxKind::CloseParen)
        {
            return Some(());
        }
        let Some(items) = self.ctx.read_elements(elements(node)) else {
            // A feature conditional can change both the section and its operands.
            return Some(());
        };
        let mut section = Section::Required;
        let mut environment = None;
        let mut keys = Bindings::default();
        let mut items = items.iter().enumerate();
        while let Some((index, item)) = items.next() {
            if (self.cancelled)() {
                return None;
            }
            let marker = self.marker(item);
            let dotted = item.kind() == SyntaxKind::Dot;
            let next = match marker {
                Some("&OPTIONAL") => Section::Optional,
                Some("&REST" | "&BODY") => Section::Rest,
                Some("&KEY") => Section::Key,
                Some("&ALLOW-OTHER-KEYS") => Section::AllowOtherKeys,
                Some("&AUX") => Section::Aux,
                _ if dotted => Section::Rest,
                _ => section,
            };
            if matches!(marker, Some("&WHOLE" | "&ENVIRONMENT")) {
                let present = if marker == Some("&WHOLE") {
                    kind.destructures() && index == 0
                } else {
                    let first = environment.replace(item.text_range());
                    if let Some(first) = first {
                        if let Some(diag) = self.emit.diagnostic(
                            "correctness/invalid-lambda-list",
                            item.text_range(),
                            "`&environment` can appear only once",
                            None,
                            Vec::new(),
                            None,
                        ) {
                            diag.note(first, "first `&environment` occurs here");
                        }
                        return Some(());
                    }
                    kind == LambdaListKind::Macro
                };
                if !present {
                    self.invalid(item, "this lambda-list marker is invalid here");
                    return Some(());
                }
                let Some((_, operand)) = items.next().filter(|(_, el)| self.marker(el).is_none())
                else {
                    self.invalid(item, "expected a parameter name after this marker");
                    return Some(());
                };
                self.variable(operand);
                continue;
            }
            if marker.is_some() || dotted {
                let valid = next > section
                    && (next != Section::AllowOtherKeys || section == Section::Key)
                    && (next != Section::Aux || kind != LambdaListKind::Generic)
                    && (marker != Some("&BODY") || kind.destructures())
                    && (!dotted || kind.destructures());
                if !valid {
                    self.invalid(item, "lambda-list marker is out of order or unavailable in this kind of lambda list");
                    return Some(());
                }
                section = next;
                if section == Section::Rest {
                    let Some((_, operand)) =
                        items.next().filter(|(_, el)| self.marker(el).is_none())
                    else {
                        self.invalid(item, "expected a rest parameter after this marker");
                        return Some(());
                    };
                    if kind.destructures() && !dotted {
                        self.pattern(operand, kind)?;
                    } else {
                        self.variable(operand);
                    }
                    if dotted && let Some((_, extra)) = items.next() {
                        self.invalid(extra, "a dotted rest parameter ends the lambda list");
                        return Some(());
                    }
                }
                continue;
            }
            match section {
                Section::Required
                    if kind == LambdaListKind::Specialized && item.as_node().is_some() =>
                {
                    let Some(parts) = self.spec(item, 1, 2) else {
                        continue;
                    };
                    self.variable(&parts[0]);
                    if let Some(specializer) = parts.get(1)
                        && let Some(node) = specializer.as_node().filter(|node| closed_list(node))
                        && let Some(parts) = self.ctx.read_elements(elements(node))
                        && let [head, value] = parts.as_slice()
                        && symbol(head).is_some_and(|name| {
                            self.ctx.workspace.standard_symbol(&name, self.package) == Some("EQL")
                        })
                    {
                        self.initforms.push(value.clone());
                    }
                }
                Section::Required => self.pattern(item, kind)?,
                Section::Rest | Section::AllowOtherKeys => {
                    self.invalid(item, "a parameter here needs a new lambda-list section");
                    return Some(());
                }
                Section::Optional | Section::Key | Section::Aux => {
                    if item.as_node().is_none() {
                        if section == Section::Key
                            && let Some(var) = symbol(item)
                        {
                            keys.keyword(self.ctx, self.package, item, &var.name, None, self.emit);
                        }
                        self.variable(item);
                        continue;
                    }
                    let max = if kind == LambdaListKind::Generic || section == Section::Aux {
                        2
                    } else {
                        3
                    };
                    let max = if kind == LambdaListKind::Generic {
                        1
                    } else {
                        max
                    };
                    let Some(parts) = self.spec(item, 1, max) else {
                        continue;
                    };
                    let mut binding = &parts[0];
                    let actual_key;
                    if section == Section::Key {
                        if binding.as_node().is_some() {
                            let Some(pair) = self.spec(binding, 2, 2) else {
                                continue;
                            };
                            if let Some(key) = symbol(&pair[0]) {
                                keys.keyword(
                                    self.ctx,
                                    self.package,
                                    &pair[0],
                                    &key.name,
                                    Some(&key),
                                    self.emit,
                                );
                            } else {
                                self.invalid(
                                    &pair[0],
                                    "an explicit keyword parameter name must be a symbol",
                                );
                            }
                            actual_key = pair;
                            binding = &actual_key[1];
                        } else if let Some(var) = symbol(binding) {
                            keys.keyword(
                                self.ctx,
                                self.package,
                                binding,
                                &var.name,
                                None,
                                self.emit,
                            );
                        }
                    }
                    if let Some(init) = parts.get(1) {
                        self.initforms.push(init.clone());
                    }
                    if section == Section::Aux {
                        self.variable(binding);
                    } else {
                        self.pattern(binding, kind)?;
                    }
                    if let Some(supplied) = parts.get(2) {
                        self.variable(supplied);
                    }
                }
            }
        }
        Some(())
    }

    fn marker(&self, element: &Element) -> Option<&'static str> {
        let name = symbol(element)?;
        if !name.name.starts_with('&') {
            return None;
        }
        let known = self.ctx.workspace.standard_symbol(&name, self.package)?;
        [
            "&OPTIONAL",
            "&REST",
            "&BODY",
            "&KEY",
            "&ALLOW-OTHER-KEYS",
            "&AUX",
            "&WHOLE",
            "&ENVIRONMENT",
        ]
        .into_iter()
        .find(|marker| *marker == known)
    }

    fn pattern(&mut self, item: &Element, kind: LambdaListKind) -> Option<()> {
        if kind.destructures()
            && (item
                .as_node()
                .is_some_and(|node| node.kind() == SyntaxKind::List)
                || is_nil_syntax(item, self.package, self.ctx.workspace))
        {
            self.list(item, LambdaListKind::Destructuring)?;
        } else {
            self.variable(item);
        }
        Some(())
    }

    fn variable(&mut self, element: &Element) {
        self.bindings
            .variable(self.ctx, self.package, element, true, self.emit);
    }

    fn spec(&mut self, item: &Element, min: usize, max: usize) -> Option<Vec<Element>> {
        let Some(node) = item.as_node().filter(|node| closed_list(node)) else {
            self.invalid(item, "expected a proper parameter specifier");
            return None;
        };
        let parts = self.ctx.read_elements(elements(node))?;
        if (min..=max).contains(&parts.len()) {
            Some(parts)
        } else {
            self.invalid(item, "wrong number of fields in this parameter specifier");
            None
        }
    }

    fn invalid(&mut self, element: &Element, message: &str) {
        report(
            self.emit,
            "correctness/invalid-lambda-list",
            element,
            message,
        );
    }
}

#[derive(Default)]
struct Bindings(FxHashMap<(String, Option<String>), TextRange>);

impl Bindings {
    fn variable(
        &mut self,
        ctx: &LintDocument<'_>,
        package: Option<&str>,
        element: &Element,
        unique: bool,
        emit: &mut LintEmitter<'_>,
    ) {
        let Some(name) = variable_name(ctx, package, element, emit) else {
            return;
        };
        if unique && name.prefix != Prefix::Uninterned {
            let home = ctx
                .workspace
                .symbol_home_package(&name, package)
                .or(package)
                .map(str::to_owned);
            self.insert(
                (name.name, home),
                element,
                "correctness/duplicate-binding",
                "binding",
                emit,
            );
        }
    }

    fn keyword(
        &mut self,
        ctx: &LintDocument<'_>,
        package: Option<&str>,
        element: &Element,
        name: &str,
        explicit: Option<&SymbolName>,
        emit: &mut LintEmitter<'_>,
    ) {
        let home = match explicit {
            None => Some("KEYWORD"),
            Some(sym) => ctx.workspace.symbol_home_package(sym, package).or(package),
        };
        if explicit.is_none_or(|sym| sym.prefix != Prefix::Uninterned) {
            self.insert(
                (name.to_owned(), home.map(str::to_owned)),
                element,
                "suspicious/duplicate-key-parameter",
                "keyword parameter",
                emit,
            );
        }
    }

    fn insert(
        &mut self,
        identity: (String, Option<String>),
        element: &Element,
        rule: &'static str,
        kind: &str,
        emit: &mut LintEmitter<'_>,
    ) {
        match self.0.entry(identity) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(element.text_range());
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                if let Some(diag) = emit.diagnostic(
                    rule,
                    element.text_range(),
                    format!("duplicate {kind} `{}`", entry.key().0.to_ascii_lowercase()),
                    None,
                    Vec::new(),
                    None,
                ) {
                    diag.note(*entry.get(), format!("first {kind} occurs here"));
                }
            }
        }
    }
}

pub(super) fn variable_name(
    ctx: &LintDocument<'_>,
    package: Option<&str>,
    item: &Element,
    emit: &mut LintEmitter<'_>,
) -> Option<SymbolName> {
    let name = symbol(item);
    if name.as_ref().is_some_and(|name| {
        name.prefix != Prefix::Keyword
            && !matches!(
                ctx.workspace.standard_symbol(name, package),
                Some("NIL" | "T")
            )
    }) {
        return name;
    }
    if item.as_token().is_some_and(|token| {
        token.kind() == SyntaxKind::Atom
            && crate::reader_tokens::potential_number(token.text())
            && !crate::reader_tokens::is_number_literal_token(token.text())
    }) {
        return None;
    }
    report(
        emit,
        "correctness/invalid-binding-name",
        item,
        "expected a variable name, which cannot be `nil`, `t`, or a keyword",
    );
    None
}

pub(super) fn binding_specs(
    ctx: &LintDocument<'_>,
    bindings: &Element,
    max_fields: usize,
    unique: bool,
    emit: &mut LintEmitter<'_>,
    cancelled: &impl Fn() -> bool,
) -> Option<Vec<Vec<Element>>> {
    let package = ctx.source.package_at(bindings.text_range().start());
    let Some(items) = proper_list(
        ctx,
        bindings,
        package,
        "correctness/invalid-binding-spec",
        "expected a proper list of bindings",
        emit,
    ) else {
        return Some(Vec::new());
    };
    let mut names = Bindings::default();
    let mut parsed = Vec::new();
    for item in items {
        if cancelled() {
            return None;
        }
        let spec = if let Some(node) = item.as_node().filter(|node| closed_list(node)) {
            let Some(fields) = ctx.read_elements(elements(node)) else {
                continue;
            };
            if !(1..=max_fields).contains(&fields.len()) {
                report(
                    emit,
                    "correctness/invalid-binding-spec",
                    &item,
                    "wrong number of fields in this binding",
                );
                continue;
            }
            fields
        } else {
            vec![item]
        };
        names.variable(ctx, package, &spec[0], unique, emit);
        parsed.push(spec);
    }
    Some(parsed)
}

pub(super) fn multiple_value_names(
    ctx: &LintDocument<'_>,
    names: &Element,
    unique: bool,
    package: Option<&str>,
    emit: &mut LintEmitter<'_>,
) {
    let Some(parts) = proper_list(
        ctx,
        names,
        package,
        "correctness/invalid-binding-spec",
        "expected a list of variable names",
        emit,
    ) else {
        return;
    };
    let mut bindings = Bindings::default();
    for part in parts {
        bindings.variable(ctx, package, &part, unique, emit);
    }
}

pub(super) fn proper_list(
    ctx: &LintDocument<'_>,
    el: &Element,
    package: Option<&str>,
    rule: &'static str,
    message: &str,
    emit: &mut LintEmitter<'_>,
) -> Option<Vec<Element>> {
    if is_nil_syntax(el, package, ctx.workspace) {
        return Some(Vec::new());
    }
    if let Some(node) = el.as_node().filter(|node| closed_list(node)) {
        return ctx.read_elements(elements(node));
    }
    report(emit, rule, el, message);
    None
}

pub(super) fn report(emit: &mut LintEmitter<'_>, rule: &'static str, el: &Element, message: &str) {
    emit.emit(rule, el.text_range(), message, None, Vec::new(), None);
}
