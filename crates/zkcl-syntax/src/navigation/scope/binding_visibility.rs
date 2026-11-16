use crate::outline::{Element, FormElements, elements};

use super::{
    LocalCallableForm, MethodScope, Namespace, ScopeAtOffset, ScopeBinding, SyntaxKind,
    VariableBindingForm, closed_list, control_symbol, non_dotted_list, symbol,
};

impl ScopeAtOffset<'_, '_> {
    /// Collect all in-scope lexical bindings visible at `self.offset`, working even inside
    /// unclosed (in-progress) lists for responsive completions and call-site inspection.
    pub(crate) fn collect_lexical_bindings(&self, form: Element, out: &mut Vec<ScopeBinding>) {
        let Some(node) = form.as_node() else {
            return;
        };
        if self.offset == node.text_range().end() && closed_list(node) {
            return;
        }
        if node.kind() == SyntaxKind::ReaderForm {
            for child in elements(node) {
                if self.contains(child.text_range()) {
                    self.collect_lexical_bindings(child, out);
                }
            }
            return;
        }
        if !non_dotted_list(node) {
            return;
        }
        let mut elems = elements(node);
        let Some(head) = elems.next() else {
            return;
        };
        let Some(sym) = symbol(&head) else {
            return;
        };
        let is_locally_shadowed_fn = out
            .iter()
            .rev()
            .any(|b| b.namespace == Namespace::Function && self.same_symbol(b, &sym));
        let std_op = if is_locally_shadowed_fn {
            None
        } else {
            self.workspace.standard_symbol(&sym, self.package)
        };
        let Some(std_op) = std_op else {
            self.descend_contained(elems.at_offset(self.offset), out);
            return;
        };
        if let Some(form) = VariableBindingForm::from_operator(std_op) {
            self.collect_variable_binding_form(&head, elems, out, form);
            return;
        }
        if let Some(form) = LocalCallableForm::from_operator(std_op) {
            self.collect_local_callable_form(elems, out, form);
            return;
        }
        match std_op {
            "DEFUN" | "DEFMACRO" | "DEFTYPE" | "DEFINE-COMPILER-MACRO" | "DEFINE-SETF-EXPANDER" => {
                let name = elems.next();
                if let Some(params) = elems.next() {
                    let inside_params = self.contains(params.text_range());
                    let specials = self.special_declarations(elems.clone(), true);
                    let start = out.len();
                    let _ = if std_op == "DEFUN" {
                        self.walk_ordinary_lambda_list(&params, out, false, &specials)
                    } else {
                        self.walk_destructuring_lambda_list(&params, out, false, &specials)
                    };
                    if !inside_params {
                        if let Some(name) = name {
                            self.named_block(&name, out);
                        }
                        self.add_free_specials(out, start, &specials);
                    }
                }
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "DEFGENERIC" => {
                let (Some(name), Some(_generic_lambda_list)) = (elems.next(), elems.next()) else {
                    return;
                };
                if let Some(option) = elems.at_offset(self.offset).next()
                    && let Some((option, true)) = self.possibly_read(option)
                    && let Some(node) = option.as_node()
                    && let Some(fields) = self.inline_method(node)
                {
                    self.collect_method_bindings(&name, fields, out);
                }
            }
            "DEFMETHOD" => {
                if let Some(name) = elems.next() {
                    self.collect_method_bindings(&name, elems, out);
                }
            }
            "LAMBDA" => {
                if let Some(params) = elems.next() {
                    let specials = self.special_declarations(elems.clone(), true);
                    let start = out.len();
                    let _ = self.walk_ordinary_lambda_list(&params, out, false, &specials);
                    if !self.contains(params.text_range()) {
                        self.add_free_specials(out, start, &specials);
                    }
                }
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "LOCALLY" => {
                let specials = self.special_declarations(elems.clone(), false);
                let start = out.len();
                self.add_free_specials(out, start, &specials);
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "SYMBOL-MACROLET" => {
                if let Some(defs_el) = elems.next()
                    && let Some(d_node) = defs_el.as_node()
                {
                    if self.contains(defs_el.text_range()) {
                        for def in elements(d_node).at_offset(self.offset) {
                            if let Some(def_node) = def.as_node() {
                                let mut parts = elements(def_node);
                                parts.next();
                                self.descend_contained(parts.at_offset(self.offset), out);
                            }
                        }
                        return;
                    }
                    for def in elements(d_node) {
                        if let Some(def_node) = def.as_node()
                            && let Some(name_el) = elements(def_node).next()
                            && let Some(sym) = symbol(&name_el)
                        {
                            out.push(self.scope_binding(
                                Namespace::Value,
                                sym,
                                name_el.text_range(),
                            ));
                        }
                    }
                }
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "MULTIPLE-VALUE-BIND" => {
                let (Some(vars), Some(values)) = (elems.next(), elems.next()) else {
                    return;
                };
                if self.contains(values.text_range()) {
                    self.collect_lexical_bindings(values, out);
                    return;
                }
                if !self.contains(vars.text_range())
                    && let Some(list) = vars.as_node()
                {
                    let specials = self.special_declarations(elems.clone(), false);
                    let start = out.len();
                    for var in elements(list) {
                        self.collect_variable(&var, out, &specials);
                    }
                    self.add_free_specials(out, start, &specials);
                }
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "DESTRUCTURING-BIND" => {
                let (Some(pattern), Some(expression)) = (elems.next(), elems.next()) else {
                    return;
                };
                if self.contains(expression.text_range()) {
                    self.collect_lexical_bindings(expression, out);
                    return;
                }
                if !self.contains(pattern.text_range()) {
                    let specials = self.special_declarations(elems.clone(), false);
                    let start = out.len();
                    let _ = self.walk_destructuring_lambda_list(&pattern, out, false, &specials);
                    self.add_free_specials(out, start, &specials);
                }
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "DOLIST" | "DOTIMES" | "DO-SYMBOLS" | "DO-EXTERNAL-SYMBOLS" | "DO-ALL-SYMBOLS" => {
                let Some(spec) = elems.next().and_then(Element::into_node) else {
                    return;
                };
                out.push(self.implicit_nil_block(&head));
                let mut parts = elements(&spec);
                let Some(var) = parts.next() else {
                    return;
                };
                if std_op != "DO-ALL-SYMBOLS"
                    && let Some(initializer) = parts.next()
                    && self.contains(initializer.text_range())
                {
                    self.collect_lexical_bindings(initializer, out);
                    return;
                }
                let specials = self.special_declarations(elems.clone(), false);
                let start = out.len();
                self.collect_variable(&var, out, &specials);
                self.add_free_specials(out, start, &specials);
                if self.contains(spec.text_range()) {
                    self.descend_contained(parts.at_offset(self.offset), out);
                } else {
                    self.collect_tagbody_bindings(elems, out);
                }
            }
            "WITH-OPEN-FILE"
            | "WITH-OPEN-STREAM"
            | "WITH-INPUT-FROM-STRING"
            | "WITH-OUTPUT-TO-STRING" => {
                let Some(spec) = elems.next().and_then(Element::into_node) else {
                    return;
                };
                let mut parts = elements(&spec);
                let Some(var) = parts.next() else {
                    return;
                };
                if self.contains(spec.text_range()) {
                    self.descend_contained(parts.at_offset(self.offset), out);
                    return;
                }
                let specials = self.special_declarations(elems.clone(), false);
                let start = out.len();
                self.collect_variable(&var, out, &specials);
                self.add_free_specials(out, start, &specials);
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "WITH-PACKAGE-ITERATOR" | "WITH-HASH-TABLE-ITERATOR" => {
                if let Some(spec_el) = elems.next()
                    && let Some(s_node) = spec_el.as_node()
                {
                    let mut parts = elements(s_node);
                    let Some(name_el) = parts.next() else {
                        return;
                    };
                    if self.contains(spec_el.text_range()) {
                        self.descend_contained(parts.at_offset(self.offset), out);
                        return;
                    }
                    if let Some(sym) = symbol(&name_el) {
                        out.push(ScopeBinding {
                            namespace: Namespace::Function,
                            spelling: sym,
                            package: self.package.map(str::to_owned),
                            def_range: Some(name_el.text_range()),
                            params: Some(std::sync::Arc::from([])),
                            evaluates_arguments: true,
                        });
                    }
                }
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "WITH-SLOTS" | "WITH-ACCESSORS" => {
                let (Some(entries), Some(instance)) = (elems.next(), elems.next()) else {
                    return;
                };
                if self.contains(instance.text_range()) {
                    self.collect_lexical_bindings(instance, out);
                    return;
                }
                if !self.contains(entries.text_range())
                    && let Some(e_node) = entries.as_node()
                {
                    for entry in elements(e_node) {
                        let var_el = if entry.as_token().is_some() {
                            Some(entry)
                        } else if let Some(n) = entry.as_node() {
                            elements(n).next()
                        } else {
                            None
                        };
                        if let Some(var) = var_el {
                            self.collect_binding(&var, Namespace::Value, out);
                        }
                    }
                }
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "BLOCK" => {
                if let Some(name_el) = elems.next()
                    && let Some(sym) = control_symbol(&name_el)
                {
                    out.push(self.scope_binding(Namespace::Block, sym, name_el.text_range()));
                }
                self.descend_contained(elems.at_offset(self.offset), out);
            }
            "TAGBODY" => self.collect_tagbody_bindings(elems, out),
            "LOOP" => {
                if let Some((block, Some(body))) = self.loop_body(&head, elems) {
                    out.push(block);
                    self.collect_lexical_bindings(body, out);
                }
            }
            "HANDLER-CASE" | "RESTART-CASE" => {
                if let Some(expr) = elems.next()
                    && self.contains(expr.text_range())
                {
                    self.collect_lexical_bindings(expr, out);
                    return;
                }
                for clause in elems {
                    if self.contains(clause.text_range())
                        && let Some(c_node) = clause.as_node()
                    {
                        let mut parts = elements(c_node);
                        let _tag = parts.next();
                        let Some(params) = parts.next() else { return };
                        if std_op == "RESTART-CASE" {
                            while parts
                                .clone()
                                .next()
                                .as_ref()
                                .is_some_and(super::restart_case_option)
                            {
                                parts.next();
                                let Some(value) = parts.next() else { return };
                                if self.contains(value.text_range()) {
                                    self.collect_lexical_bindings(value, out);
                                    return;
                                }
                            }
                        }
                        let specials = self.special_declarations(parts.clone(), false);
                        let start = out.len();
                        let _ = self.walk_ordinary_lambda_list(&params, out, false, &specials);
                        self.add_free_specials(out, start, &specials);
                        self.descend_contained(parts, out);
                        return;
                    }
                }
            }
            _ => {
                self.descend_contained(elems.at_offset(self.offset), out);
            }
        }
    }

    fn collect_method_bindings(
        &self,
        name: &Element,
        mut fields: FormElements<'_>,
        out: &mut Vec<ScopeBinding>,
    ) {
        let Some(params) = self.method_lambda_list(&mut fields) else {
            return;
        };
        if self.offset < params.text_range().start() {
            return;
        }
        let specials = self.special_declarations(fields.clone(), true);
        let start = out.len();
        if self.contains(params.text_range()) {
            let _ =
                self.walk_specialized_lambda_list(&params, out, MethodScope::Completion, &specials);
        } else {
            let _ = self.walk_specialized_lambda_list(&params, out, MethodScope::Body, &specials);
            self.named_block(name, out);
            self.add_free_specials(out, start, &specials);
            self.descend_contained(fields.at_offset(self.offset), out);
        }
    }

    pub(super) fn collect_tagbody_bindings(
        &self,
        statements: FormElements<'_>,
        out: &mut Vec<ScopeBinding>,
    ) {
        self.add_tag_bindings(statements.clone(), out);
        self.descend_contained(statements.at_offset(self.offset), out);
    }

    pub(super) fn collect_binding(
        &self,
        name: &Element,
        namespace: Namespace,
        out: &mut Vec<ScopeBinding>,
    ) {
        if let Some(spelling) = symbol(name) {
            out.push(self.scope_binding(namespace, spelling, name.text_range()));
        }
    }

    fn collect_variable(
        &self,
        name: &Element,
        out: &mut Vec<ScopeBinding>,
        specials: &[super::SymbolName],
    ) {
        if let Some(spelling) = symbol(name) {
            out.push(self.variable_binding(spelling, name.text_range(), specials));
        }
    }

    pub(super) fn descend_contained(
        &self,
        children: impl IntoIterator<Item = Element>,
        out: &mut Vec<ScopeBinding>,
    ) {
        for child in children {
            if self.contains(child.text_range()) {
                self.collect_lexical_bindings(child, out);
            }
        }
    }
}
