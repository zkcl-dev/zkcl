//! Resolve patterns from source declarations without running a pattern expander.
use super::*;

fn keyword(element: &Element) -> Option<String> {
    let name = symbol(element)?;
    (name.prefix == Prefix::Keyword).then_some(name.name)
}

pub(crate) fn lexical_call_parts(args: &[Element]) -> Option<(&Element, &[Element])> {
    let (input, rows) = args.split_first()?;
    let rows = if rows.first().and_then(keyword).is_some() {
        &rows[1..]
    } else {
        rows
    };
    Some((input, rows))
}

pub(crate) fn lexical_input_parts(
    input: &Element,
) -> Option<(zkcl_pattern::InputDomain, Vec<Element>)> {
    let mut parts = elements(input.as_node()?);
    let domain = match keyword(&parts.next()?)?.as_str() {
        "TEXT" => zkcl_pattern::InputDomain::Text,
        "BYTES" => zkcl_pattern::InputDomain::Bytes,
        _ => return None,
    };
    let input = parts.next()?;
    Some((
        domain,
        std::iter::once(input)
            .chain(parts.skip(1).step_by(2))
            .collect(),
    ))
}

pub(crate) fn lexical_regex_warnings(args: &[Element]) -> Vec<(TextRange, String)> {
    let Some((input, rows)) = lexical_call_parts(args) else {
        return Vec::new();
    };
    let domain =
        lexical_input_parts(input).map_or(zkcl_pattern::InputDomain::Text, |(domain, _)| domain);
    let mut warnings = Vec::new();
    for clause in rows {
        if let Some(rule) = clause
            .as_node()
            .filter(|node| closed_list(node))
            .and_then(|node| elements(node).next())
        {
            regex_warnings(&rule, domain, &mut warnings);
        }
    }
    warnings
}

fn regex_warnings(
    rule: &Element,
    domain: zkcl_pattern::InputDomain,
    warnings: &mut Vec<(TextRange, String)>,
) {
    let Some(node) = rule.as_node().filter(|node| closed_list(node)) else {
        return;
    };
    let mut parts = elements(node);
    let Some(head) = parts.next().as_ref().and_then(keyword) else {
        return;
    };
    let child = match head.as_str() {
        "RE" => {
            if let Some(token) = parts
                .next()
                .and_then(|element| element.into_token())
                .filter(|token| token.kind() == SyntaxKind::String)
                && let Some(pattern) = crate::reader_tokens::string_literal(&token)
                && let Err(error) = zkcl_pattern::parse_regex(domain, &pattern)
            {
                let (start, end) = crate::reader_tokens::string_token_source_range(
                    token.text(),
                    error.offset..error.offset.saturating_add(1),
                );
                let base = token.text_range().start();
                warnings.push((
                    TextRange::new(
                        base + TextSize::from(start as u32),
                        base + TextSize::from(end as u32),
                    ),
                    error.message,
                ));
            }
            None
        }
        "WITH" | "*" | "+" | "?" => parts.next(),
        "CAPTURE" => parts.nth(1),
        "REPEAT" => parts.nth(2),
        "SEQ" | "OR" => {
            for child in parts {
                regex_warnings(&child, domain, warnings);
            }
            None
        }
        _ => None,
    };
    if let Some(child) = child {
        regex_warnings(&child, domain, warnings);
    }
}

impl ScopeAtOffset<'_, '_> {
    pub(super) fn match_call(
        &self,
        args: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let (subject, clauses) = args.split_first()?;
        let row = Self::input_row(subject);
        if self.contains(subject.text_range()) {
            return self.match_source(subject, scope);
        }
        self.match_clauses(clauses, row, scope)
    }

    fn input_row(subject: &Element) -> bool {
        subject
            .as_node()
            .and_then(|n| elements(n).next())
            .and_then(|el| symbol(&el))
            .is_some_and(|s| {
                s.prefix == Prefix::Keyword && matches!(s.name.as_str(), "INPUTS" | "VALUES")
            })
    }

    fn match_source(
        &self,
        subject: &Element,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        if Self::input_row(subject) {
            return self.body_expressions(
                &elements(subject.as_node()?).skip(1).collect::<Vec<_>>(),
                scope,
            );
        }
        self.expression(subject.clone(), scope)
    }

    fn bind_pattern(
        &self,
        pattern: &Element,
        row: bool,
        outer_len: usize,
        locals: &mut Vec<ScopeBinding>,
        opaque: &mut bool,
    ) -> Option<OccurrenceBinding> {
        if row && let Some(list) = pattern.as_node() {
            elements(list).find_map(|p| self.pattern_node(&p, outer_len, locals, opaque))
        } else {
            self.pattern_node(pattern, outer_len, locals, opaque)
        }
    }

    pub(super) fn single_pattern_call(
        &self,
        op: &str,
        args: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let (p, rest) = args.split_first()?;
        let (source, body) = rest.split_first()?;
        if self.contains(source.text_range()) {
            return self.match_source(source, scope);
        }
        let row = Self::input_row(source);
        let outer_len = scope.len();
        let mut opaque = false;
        let target = self.bind_pattern(p, row, outer_len, scope, &mut opaque);
        if self.contains(p.text_range()) {
            return target;
        }
        let (guard, branches) = if body
            .first()
            .and_then(symbol)
            .is_some_and(|s| s.prefix == Prefix::Keyword && s.name == "WHEN")
        {
            (body.get(1), body.get(2..).unwrap_or(&[]))
        } else {
            (None, body)
        };
        if let Some(guard) = guard
            && self.contains(guard.text_range())
        {
            if opaque {
                return None;
            }
            return self.body_expression(guard.clone(), scope);
        }
        let position = branches
            .iter()
            .position(|f| self.contains(f.text_range()))?;
        let bound = op != "UNLESS-MATCH" && (op != "IF-MATCH" || position == 0);
        if bound && opaque {
            return None;
        }
        if !bound {
            scope.truncate(outer_len);
        }
        self.body_expression(branches[position].clone(), scope)
    }

    pub(super) fn match_bindings(
        &self,
        args: &[Element],
        scope: &mut Vec<ScopeBinding>,
        sequential: bool,
    ) -> Option<OccurrenceBinding> {
        let (bindings, body) = args.split_first()?;
        let base_len = scope.len();
        let mut opaque = false;
        for binding in elements(bindings.as_node()?) {
            let mut pair = elements(binding.as_node()?);
            let p = pair.next()?;
            let source = pair.next()?;
            if self.contains(source.text_range()) {
                if sequential && opaque {
                    return None;
                }
                if !sequential {
                    scope.truncate(base_len);
                }
                return self.match_source(&source, scope);
            }
            let outer_len = if sequential { scope.len() } else { base_len };
            let result =
                self.bind_pattern(&p, Self::input_row(&source), outer_len, scope, &mut opaque);
            if self.contains(p.text_range()) {
                return result;
            }
        }
        if opaque {
            None
        } else {
            self.body_expressions(body, scope)
        }
    }

    pub(super) fn match_clauses(
        &self,
        clauses: &[Element],
        row: bool,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let clause = clauses
            .iter()
            .find(|el| self.contains(el.text_range()))?
            .as_node()?;
        let mut parts = elements(clause);
        let pattern = parts.next()?;
        let outer_len = scope.len();
        let mut opaque = false;
        let target = self.bind_pattern(&pattern, row, outer_len, scope, &mut opaque);
        if self.contains(pattern.text_range()) {
            return target;
        }
        // A user expander can introduce arbitrary bindings. Remain unresolved
        // rather than identifying a body name as an unrelated global.
        if opaque {
            return None;
        }
        let expr = parts.at_offset(self.offset).next()?;
        self.expression(expr, scope)
    }

    fn pattern_node(
        &self,
        p: &Element,
        outer_len: usize,
        bindings: &mut Vec<ScopeBinding>,
        opaque: &mut bool,
    ) -> Option<OccurrenceBinding> {
        if let Some(name) = symbol(p) {
            if name.prefix == Prefix::Keyword
                || name.name == "_"
                || matches!(
                    self.workspace.standard_symbol(&name, self.package),
                    Some("T" | "NIL")
                )
            {
                return None;
            }
            self.collect_binding(p, Namespace::Value, bindings);
            return self
                .contains(p.text_range())
                .then(|| self.local_target(Namespace::Value, name, p.text_range(), None));
        }
        let list = p.as_node()?;
        if list.kind() != SyntaxKind::List {
            return None;
        }
        let mut parts = elements(list);
        let head = parts.next()?;
        let op = symbol(&head)?;
        let standard = self.workspace.standard_symbol(&op, self.package);
        if standard == Some("QUOTE") {
            return None;
        }
        if op.prefix == Prefix::Keyword {
            match op.name.as_str() {
                "CONST" => {
                    return parts
                        .next()
                        .filter(|el| self.contains(el.text_range()))
                        .and_then(|el| {
                            self.resolve_symbol(&el, Namespace::Value, &bindings[..outer_len])
                        });
                }
                "=" => {
                    if let Some(el) = parts.step_by(2).find(|el| self.contains(el.text_range())) {
                        bindings.truncate(outer_len);
                        return self.expression(el, bindings);
                    }
                    return None;
                }
                "REST" => return self.pattern_node(&parts.next()?, outer_len, bindings, opaque),
                "VALUES" => {
                    return parts
                        .find_map(|child| self.pattern_node(&child, outer_len, bindings, opaque));
                }
                _ => {}
            }
        }
        if self.package_symbol(&op, "ZK-LEX") && op.name == "LEX" {
            return self.lexical_pattern(&parts.next()?, bindings, opaque);
        }
        if self.package_symbol(&op, "ZK-BITS") && op.name == "BITS" {
            for field in parts {
                let Some(node) = field.as_node() else {
                    continue;
                };
                let mut fields = elements(node);
                let Some(head) = fields.next().and_then(|head| symbol(&head)) else {
                    continue;
                };
                let name = head.name.as_str();
                if head.prefix == Prefix::Keyword && matches!(name, "U" | "I" | "BYTES") {
                    let width = fields.next()?;
                    if self.contains(width.text_range()) {
                        return self.expression(width, bindings);
                    }
                    if matches!(name, "U" | "I") {
                        fields.next()?;
                    }
                }
                if let Some(child) = fields.next()
                    && let Some(target) = self.pattern_node(&child, outer_len, bindings, opaque)
                {
                    return Some(target);
                }
            }
            return None;
        }
        if self.zk_symbol(&op) {
            match op.name.as_str() {
                "RANGE" => {
                    for bound in parts.by_ref().take(2) {
                        if self.contains(bound.text_range()) {
                            return self.pattern_bound_reference(bound, &bindings[..outer_len]);
                        }
                    }
                    return self.pattern_node(&parts.next()?, outer_len, bindings, opaque);
                }
                "KEYS" => {
                    return parts.find_map(|entry| {
                        let mut fields = elements(entry.as_node()?);
                        let key = fields.next()?;
                        if self.contains(key.text_range()) {
                            return self.pattern_bound_reference(key, &bindings[..outer_len]);
                        }
                        self.pattern_node(&fields.next()?, outer_len, bindings, opaque)
                    });
                }
                _ => {}
            }
        }
        if standard == Some("TYPE") || (op.name == "VIEW" && self.zk_symbol(&op)) {
            let target = parts.next()?;
            if self.contains(target.text_range()) {
                return if standard == Some("TYPE") {
                    self.type_specifier(target, bindings)
                } else if symbol(&target).is_some() {
                    self.resolve_symbol(&target, Namespace::Function, &bindings[..outer_len])
                } else {
                    bindings.truncate(outer_len);
                    self.expression(target, bindings)
                };
            }
        } else if !matches!(
            standard,
            Some("CONS" | "LIST" | "LIST*" | "VECTOR" | "AND" | "OR")
        ) && !(op.name == "TREELIST"
            && (self.zk_symbol(&op)
                || self.workspace.symbol_home_package(&op, self.package)
                    == Some(crate::treelist_builtins::PACKAGE)))
        {
            if self.contains(head.text_range()) {
                return self.resolve_symbol(&head, Namespace::Pattern, bindings);
            }
            let target = self.name(op, Namespace::Pattern);
            if !self
                .workspace
                .lookup_unique_declaration(&target)
                .is_some_and(|(_, decl)| decl.operator.name == "DEFVARIANT")
            {
                *opaque = true;
                return None;
            }
        }
        parts.find_map(|child| self.pattern_node(&child, outer_len, bindings, opaque))
    }

    fn pattern_bound_reference(
        &self,
        mut bound: Element,
        outer: &[ScopeBinding],
    ) -> Option<OccurrenceBinding> {
        if let Some(list) = bound.as_node()
            && let [first] = elements(list).collect::<Vec<_>>().as_slice()
        {
            bound = first.clone();
        }
        let mut items = elements(bound.as_node()?);
        let operator = items.next().and_then(|head| symbol(&head))?;
        if operator.prefix == Prefix::Keyword && operator.name == "CONST" {
            let name = items.next()?;
            if self.contains(name.text_range()) {
                return self.resolve_symbol(&name, Namespace::Value, outer);
            }
        }
        None
    }

    pub(super) fn lexical_call(
        &self,
        args: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let (input, rows) = lexical_call_parts(args)?;
        if self.contains(input.text_range()) {
            return match lexical_input_parts(input) {
                Some((_, expressions)) => self.body_expressions(&expressions, scope),
                None => self.expression(input.clone(), scope),
            };
        }
        let clause = rows
            .iter()
            .find(|row| self.contains(row.text_range()))?
            .as_node()?;
        let mut items = elements(clause);
        let rule = items.next()?;
        let mut opaque = false;
        let result = self.lexical_pattern(&rule, scope, &mut opaque);
        if self.contains(rule.text_range()) {
            return result;
        }
        if opaque {
            return None;
        }
        self.expression(items.at_offset(self.offset).next()?, scope)
    }

    fn lexical_pattern(
        &self,
        rule: &Element,
        bindings: &mut Vec<ScopeBinding>,
        opaque: &mut bool,
    ) -> Option<OccurrenceBinding> {
        let mut items = elements(rule.as_node()?);
        let head = items.next()?;
        let op = symbol(&head)?;
        if op.prefix != Prefix::Keyword {
            if self.contains(head.text_range()) {
                return self.resolve_symbol(&head, Namespace::Pattern, bindings);
            }
            *opaque = true;
            return None;
        }
        match op.name.as_str() {
            "CAPTURE" => {
                let name = items.next()?;
                if let Some(found) = self.lexical_capture(&name, bindings) {
                    return Some(found);
                }
                self.lexical_pattern(&items.next()?, bindings, opaque)
            }
            "WITH" => {
                if let Some(found) = self.lexical_pattern(&items.next()?, bindings, opaque) {
                    return Some(found);
                }
                items
                    .skip(1)
                    .step_by(2)
                    .find_map(|name| match name.as_node() {
                        Some(list) => self.lexical_capture(&elements(list).next()?, bindings),
                        None => self.lexical_capture(&name, bindings),
                    })
            }
            "SEQ" | "OR" | "*" | "+" | "?" => {
                items.find_map(|part| self.lexical_pattern(&part, bindings, opaque))
            }
            "REPEAT" => self.lexical_pattern(&items.nth(2)?, bindings, opaque),
            _ => None,
        }
    }

    fn lexical_capture(
        &self,
        name: &Element,
        bindings: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let spelling = symbol(name)?;
        if spelling.name == "_"
            || spelling.prefix == Prefix::Keyword
            || matches!(
                self.workspace.standard_symbol(&spelling, self.package),
                Some("T" | "NIL")
            )
        {
            return None;
        }
        self.collect_binding(name, Namespace::Value, bindings);
        self.contains(name.text_range())
            .then(|| self.local_target(Namespace::Value, spelling, name.text_range(), None))
    }

    pub(crate) fn match_warnings(
        &self,
        args: &[Element],
        warnings: &mut Vec<crate::navigation::SemanticWarning>,
    ) {
        let Some((subject, clauses)) = args.split_first() else {
            return;
        };
        let row = Self::input_row(subject);
        let width = subject.as_node().and_then(|list| {
            let mut items = elements(list);
            let mode = items.next().and_then(|el| symbol(&el))?;
            (row && mode.name == "INPUTS").then(|| items.count())
        });
        for clause in clauses {
            let Some(list) = clause.as_node().filter(|n| closed_list(n)) else {
                continue;
            };
            let Some(p) = elements(list).next() else {
                continue;
            };
            if row {
                let Some(cols) = p.as_node().filter(|n| closed_list(n)) else {
                    continue;
                };
                if let Some(width) = width
                    && elements(cols).count() != width
                {
                    warnings.push(crate::navigation::SemanticWarning {
                        range: p.text_range(),
                        message: format!(
                            "expected {width} match columns, got {}",
                            elements(cols).count()
                        ),
                    });
                    continue;
                }
                for p in elements(cols) {
                    self.constructor_warnings(&p, warnings);
                }
            } else {
                self.constructor_warnings(&p, warnings);
            }
        }
    }

    fn constructor_warnings(
        &self,
        p: &Element,
        warnings: &mut Vec<crate::navigation::SemanticWarning>,
    ) {
        let Some(list) = p.as_node().filter(|n| closed_list(n)) else {
            return;
        };
        let mut parts = elements(list);
        let Some(head) = parts.next().and_then(|e| symbol(&e)) else {
            return;
        };
        if head.prefix == Prefix::Keyword {
            match head.name.as_str() {
                "CONST" | "=" => return,
                "REST" => {
                    if let Some(child) = parts.next() {
                        self.constructor_warnings(&child, warnings);
                    }
                    return;
                }
                "VALUES" => {
                    for child in parts {
                        self.constructor_warnings(&child, warnings);
                    }
                    return;
                }
                _ => {}
            }
        }
        if self.package_symbol(&head, "ZK-LEX") && head.name == "LEX" {
            let Some(rule) = parts.next() else {
                return;
            };
            let _mode = parts.next();
            let domain = if parts.next().as_ref().and_then(keyword).as_deref() == Some("BYTES") {
                zkcl_pattern::InputDomain::Bytes
            } else {
                zkcl_pattern::InputDomain::Text
            };
            let mut regex_errors = Vec::new();
            regex_warnings(&rule, domain, &mut regex_errors);
            for (range, message) in regex_errors {
                warnings.push(crate::navigation::SemanticWarning { range, message });
            }
            return;
        }
        if self.package_symbol(&head, "ZK-BITS") && head.name == "BITS" {
            for field in parts {
                let Some(node) = field.as_node().filter(|n| closed_list(n)) else {
                    continue;
                };
                let mut fields = elements(node);
                let Some(field_head) = fields.next().and_then(|el| symbol(&el)) else {
                    continue;
                };
                if field_head.prefix == Prefix::Keyword {
                    match field_head.name.as_str() {
                        "U" | "I" => {
                            fields.next();
                            fields.next();
                        }
                        "BYTES" => {
                            fields.next();
                        }
                        _ => {}
                    }
                }
                if let Some(child) = fields.next() {
                    self.constructor_warnings(&child, warnings);
                }
            }
            return;
        }
        if self.zk_symbol(&head) {
            match head.name.as_str() {
                "RANGE" => {
                    if let Some(child) = parts.nth(2) {
                        self.constructor_warnings(&child, warnings);
                    }
                    return;
                }
                "KEYS" => {
                    for entry in parts {
                        let Some(node) = entry.as_node().filter(|n| closed_list(n)) else {
                            continue;
                        };
                        if let Some(child) = elements(node).nth(1) {
                            self.constructor_warnings(&child, warnings);
                        }
                    }
                    return;
                }
                "VIEW" => {
                    parts.next();
                    for child in parts {
                        self.constructor_warnings(&child, warnings);
                    }
                    return;
                }
                _ => {}
            }
        }
        match self.workspace.standard_symbol(&head, self.package) {
            Some("QUOTE") => return,
            Some("TYPE") => {
                parts.next();
            }
            Some("CONS" | "LIST" | "LIST*" | "VECTOR" | "AND" | "OR") => {}
            _ => {
                let key = self.name(head, Namespace::Pattern);
                let Some((_, declaration)) = self.workspace.lookup_unique_declaration(&key) else {
                    return;
                };
                if declaration.operator.name != "DEFVARIANT" {
                    return;
                }
                let Some(fields) = declaration.params.as_ref() else {
                    return;
                };
                let count = parts.clone().count();
                if count != fields.len() {
                    warnings.push(crate::navigation::SemanticWarning {
                        range: p.text_range(),
                        message: format!(
                            "constructor {} expects {} fields, got {count}",
                            declaration.name.spelling.name,
                            fields.len()
                        ),
                    });
                }
            }
        }
        for child in parts {
            self.constructor_warnings(&child, warnings);
        }
    }

    pub(crate) fn package_symbol(&self, s: &SymbolName, package: &str) -> bool {
        match &s.prefix {
            Prefix::Package { name, .. } => {
                self.workspace.canonical_global_package(name) == Some(package)
            }
            Prefix::None => self.workspace.symbol_home_package(s, self.package) == Some(package),
            _ => false,
        }
    }

    pub(crate) fn zk_symbol(&self, s: &SymbolName) -> bool {
        self.package_symbol(s, "ZK")
    }
}
