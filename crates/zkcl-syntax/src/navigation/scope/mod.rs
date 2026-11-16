use std::cell::OnceCell;

use crate::cl_builtins::evaluates_all_arguments;
use crate::outline::{Element, FormElements, elements, elements_at_offset};
use crate::reader_tokens::{Prefix, SymbolName, integer_token};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

use super::index::{
    MacroOperands, WorkspaceIndex, closed_list, non_dotted_list, operator,
    parse_setf_function_name, possibly_read, symbol,
};
use super::{Name, Namespace, OccurrenceBinding, ScopeBinding};

mod binding_forms;
mod binding_visibility;
mod evaluated_position;
mod lambda_lists;
mod match_forms;
pub(crate) use match_forms::{lexical_call_parts, lexical_input_parts, lexical_regex_warnings};
mod special_declarations;

use binding_forms::{LocalCallableForm, VariableBindingForm};
pub(crate) use evaluated_position::is_evaluated_call_position;
use lambda_lists::MethodScope;

pub(crate) fn restart_case_option(element: &Element) -> bool {
    symbol(element).is_some_and(|keyword| {
        keyword.prefix == Prefix::Keyword
            && matches!(keyword.name.as_str(), "REPORT" | "INTERACTIVE" | "TEST")
    })
}

fn tag_name(element: &Element) -> Option<(Namespace, SymbolName)> {
    if let Some(spelling) = control_symbol(element) {
        return Some((Namespace::Tag, spelling));
    }
    let token = element.as_token()?;
    if token.kind() != SyntaxKind::Atom {
        return None;
    }
    let name = integer_token(token.text())?;
    Some((
        Namespace::IntegerTag,
        SymbolName {
            prefix: Prefix::None,
            name,
        },
    ))
}

pub(super) fn is_empty_list(element: &Element) -> bool {
    element
        .as_node()
        .is_some_and(|list| closed_list(list) && elements(list).next().is_none())
}

fn control_symbol(element: &Element) -> Option<SymbolName> {
    symbol(element).or_else(|| is_empty_list(element).then(nil_block_name))
}

fn nil_block_name() -> SymbolName {
    SymbolName {
        prefix: Prefix::Package {
            name: "COMMON-LISP".into(),
            external: true,
        },
        name: "NIL".into(),
    }
}

fn loop_compound_form(form: &Element) -> bool {
    form.as_node().is_some_and(|node| match node.kind() {
        SyntaxKind::List => non_dotted_list(node) && elements(node).next().is_some(),
        SyntaxKind::ReaderForm => node.first_token().is_some_and(|token| {
            matches!(
                token.kind(),
                SyntaxKind::Quote | SyntaxKind::FunctionQuote | SyntaxKind::Backquote
            )
        }),
        _ => false,
    })
}

fn loop_compound_clause(form: &Element) -> bool {
    symbol(form).is_some_and(|keyword| {
        matches!(
            keyword.name.as_str(),
            "DO" | "DOING" | "INITIALLY" | "FINALLY"
        )
    })
}

pub(crate) struct ScopeAtOffset<'a, 'docs> {
    pub(crate) package: Option<&'a str>,
    pub(crate) offset: TextSize,
    pub(crate) workspace: &'a WorkspaceIndex<'docs>,
    pub(crate) known_features: Option<&'a [String]>,
    pub(crate) macro_operands: Option<&'a MacroOperands>,
}

pub(crate) struct FunctionBodyScope {
    header_end: TextSize,
    first_child: usize,
    bindings: Vec<ScopeBinding>,
}

impl FunctionBodyScope {
    pub(crate) fn contains(&self, offset: TextSize) -> bool {
        // The inclusive lookup treats the lambda list's closing boundary as
        // part of the header, even if the first body form starts there.
        offset > self.header_end
    }
}

impl ScopeAtOffset<'_, '_> {
    pub(crate) fn contains(&self, range: TextRange) -> bool {
        range.contains_inclusive(self.offset)
    }

    fn possibly_read(&self, element: Element) -> Option<(Element, bool)> {
        possibly_read(element, self.known_features)
    }

    fn loop_body(
        &self,
        head: &Element,
        forms: FormElements<'_>,
    ) -> Option<(ScopeBinding, Option<Element>)> {
        let mut forms = forms.filter_map(|form| self.possibly_read(form)).peekable();
        let implicit = || self.implicit_nil_block(head);
        let Some((first, known)) = forms.peek() else {
            return Some((implicit(), None));
        };
        if !known {
            return None;
        }
        if loop_compound_form(first) {
            let mut selected = None;
            for (form, known) in forms {
                if !known || !loop_compound_form(&form) {
                    return None;
                }
                if self.contains(form.text_range()) {
                    selected = Some(form);
                }
            }
            return Some((implicit(), selected));
        }

        let block = if symbol(first).is_some_and(|keyword| keyword.name == "NAMED") {
            forms.next();
            let (name, true) = forms.next()? else {
                return None;
            };
            self.scope_binding(Namespace::Block, control_symbol(&name)?, name.text_range())
        } else if loop_compound_clause(first) {
            implicit()
        } else {
            return None;
        };

        // These clauses have an unambiguous run of compound body forms and
        // introduce no variable bindings. Stop before other clause grammars.
        while let Some((clause, true)) = forms.next() {
            if !loop_compound_clause(&clause) {
                break;
            }
            while let Some((form, _)) =
                forms.next_if(|(form, known)| *known && loop_compound_form(form))
            {
                if self.contains(form.text_range()) {
                    return Some((block, Some(form)));
                }
            }
        }
        Some((block, None))
    }

    fn local_target(
        &self,
        namespace: Namespace,
        spelling: SymbolName,
        range: TextRange,
        params: Option<std::sync::Arc<[String]>>,
    ) -> OccurrenceBinding {
        OccurrenceBinding::LocalSite {
            def_range: range,
            namespace,
            spelling,
            package: self.package.map(str::to_owned),
            params,
            occurrence_range: range,
        }
    }

    fn scope_binding(
        &self,
        namespace: Namespace,
        spelling: SymbolName,
        range: TextRange,
    ) -> ScopeBinding {
        ScopeBinding {
            namespace,
            spelling,
            package: self.package.map(str::to_owned),
            def_range: Some(range),
            params: None,
            evaluates_arguments: true,
        }
    }

    pub(crate) fn prepare_function_body(
        &self,
        form: &Element,
        scope: &[ScopeBinding],
    ) -> Option<FunctionBodyScope> {
        let list = form.as_node().filter(|list| closed_list(list))?;
        let mut parts = elements(list);
        let head = symbol(&parts.next()?)?;
        let op = self.workspace.standard_symbol(&head, self.package)?;
        if !matches!(op, "DEFUN" | "DEFMACRO" | "DEFMETHOD") {
            return None;
        }
        let name = parts.next()?;
        let lambda = if op == "DEFMETHOD" {
            self.method_lambda_list(&mut parts)?
        } else {
            parts.next()?
        };
        let first_child = parts.clone().next()?.index();
        let specials = self.special_declarations(parts, true);
        let mut bindings = scope.to_vec();
        let outer_len = bindings.len();
        match op {
            "DEFUN" => self
                .walk_ordinary_lambda_list(&lambda, &mut bindings, false, &specials)
                .ok()?,
            "DEFMACRO" => self
                .walk_destructuring_lambda_list(&lambda, &mut bindings, false, &specials)
                .ok()?,
            "DEFMETHOD" => self
                .walk_specialized_lambda_list(&lambda, &mut bindings, MethodScope::Body, &specials)
                .ok()?,
            _ => unreachable!(),
        };
        self.named_block(&name, &mut bindings);
        self.add_free_specials(&mut bindings, outer_len, &specials);
        Some(FunctionBodyScope {
            header_end: lambda.text_range().end(),
            first_child,
            bindings,
        })
    }

    pub(crate) fn resolve_function_body(
        &self,
        form: &Element,
        body: &FunctionBodyScope,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let element = elements_at_offset(form.as_node()?, body.first_child, self.offset).next()?;
        scope.clear();
        scope.extend_from_slice(&body.bindings);
        self.body_expression(element, scope)
    }

    fn push_function_body_bindings(
        &self,
        name: &Element,
        lambda: &Element,
        scope: &mut Vec<ScopeBinding>,
        specials: &[SymbolName],
    ) -> Option<()> {
        let outer_len = scope.len();
        self.walk_ordinary_lambda_list(lambda, scope, false, specials)
            .ok()?;
        self.named_block(name, scope);
        self.add_free_specials(scope, outer_len, specials);
        Some(())
    }

    pub(crate) fn top_level(
        &self,
        form: Element,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let head = form.as_node().and_then(|list| elements(list).next());
        let spelling = head.as_ref().and_then(symbol);
        let standard = spelling
            .as_ref()
            .and_then(|spelling| self.workspace.standard_symbol(spelling, self.package));
        match standard {
            Some("PROGN" | "LOCALLY") => {
                let list = form.as_node()?;
                if !non_dotted_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                if standard == Some("LOCALLY") {
                    let specials = self.special_declarations(elems.clone(), false);
                    let outer_len = scope.len();
                    self.add_free_specials(scope, outer_len, &specials);
                    return self.body_expression(elems.at_offset(self.offset).next()?, scope);
                }
                let inner = elems.at_offset(self.offset).next()?;
                self.top_level(inner, scope)
            }
            Some("EVAL-WHEN") => {
                let list = form.as_node()?;
                if !non_dotted_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let situations_el = elems.next()?;
                if self.contains(situations_el.text_range()) {
                    return None;
                }
                if !crate::outline::eval_when_allows_top_level_outline(list) {
                    return None;
                }
                let inner = elems.at_offset(self.offset).next()?;
                self.top_level(inner, scope)
            }
            Some("DEFUN") => {
                let list = form.as_node()?;
                if !closed_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let name_el = elems.next()?;
                if self.contains(name_el.text_range()) {
                    return self.resolve_function_declaration_name(&name_el, scope);
                }
                let lambda_el = elems.next()?;
                let specials = self.special_declarations(elems.clone(), true);
                if self.contains(lambda_el.text_range()) {
                    return self
                        .walk_ordinary_lambda_list(&lambda_el, scope, true, &specials)
                        .ok()
                        .flatten();
                }
                self.push_function_body_bindings(&name_el, &lambda_el, scope, &specials)?;
                self.body_expression(elems.at_offset(self.offset).next()?, scope)
            }
            Some("DEFVAR" | "DEFPARAMETER" | "DEFCONSTANT" | "DEFINE-SYMBOL-MACRO") => {
                let list = form.as_node()?;
                if !closed_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let name_el = elems.next()?;
                if self.contains(name_el.text_range()) {
                    return self.resolve_declaration_name(&name_el, Namespace::Value);
                }
                let expression = elems.next()?;
                if self.contains(expression.text_range()) {
                    return self.expression(expression, scope);
                }
                None
            }
            Some("DEFGENERIC") => {
                let list = form.as_node()?;
                if !closed_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let name_el = elems.next()?;
                if self.contains(name_el.text_range()) {
                    return self.resolve_function_declaration_name(&name_el, scope);
                }
                self.resolve_generic_options(&name_el, elems, scope)
            }
            Some("DEFMETHOD") => {
                let list = form.as_node()?;
                if !closed_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let name_el = elems.next()?;
                if self.contains(name_el.text_range()) {
                    return self.function_designator(name_el, scope);
                }
                self.resolve_method(&name_el, elems, scope)
            }
            Some("DEFMACRO" | "DEFINE-COMPILER-MACRO" | "DEFINE-SETF-EXPANDER") => {
                let list = form.as_node()?;
                if !closed_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let name_el = elems.next()?;
                if self.contains(name_el.text_range()) {
                    if standard == Some("DEFMACRO") {
                        return self.resolve_declaration_name(&name_el, Namespace::Function);
                    }
                    return self.function_designator(name_el, scope);
                }
                let lambda_el = elems.next()?;
                let specials = self.special_declarations(elems.clone(), true);
                let outer_len = scope.len();
                if self.contains(lambda_el.text_range()) {
                    return self
                        .walk_destructuring_lambda_list(&lambda_el, scope, true, &specials)
                        .ok()
                        .flatten();
                }
                self.walk_destructuring_lambda_list(&lambda_el, scope, false, &specials)
                    .ok()?;
                self.named_block(&name_el, scope);
                self.add_free_specials(scope, outer_len, &specials);
                self.body_expression(elems.at_offset(self.offset).next()?, scope)
            }
            Some("DEFCLASS" | "DEFINE-CONDITION") => {
                let list = form.as_node()?;
                if !closed_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let name_el = elems.next()?;
                if self.contains(name_el.text_range()) {
                    return self.resolve_declaration_name(&name_el, Namespace::Type);
                }
                let supers_el = elems.next()?;
                if self.contains(supers_el.text_range()) {
                    if let Some(supers_list) = supers_el.as_node()
                        && closed_list(supers_list)
                    {
                        let super_el =
                            elements(supers_list).find(|c| self.contains(c.text_range()))?;
                        return self.resolve_symbol(&super_el, Namespace::Type, scope);
                    }
                    return None;
                }
                let slots_el = elems.next()?;
                if self.contains(slots_el.text_range()) {
                    let slots_list = slots_el.as_node()?;
                    if !closed_list(slots_list) {
                        return None;
                    }
                    let slot = elements(slots_list)
                        .find(|s| self.contains(s.text_range()))?
                        .into_node()?;
                    if !closed_list(&slot) {
                        return None;
                    }
                    let slot_parts: Vec<_> = elements(&slot).collect();
                    let mut idx = 1;
                    while idx + 1 < slot_parts.len() {
                        let key = &slot_parts[idx];
                        let val = &slot_parts[idx + 1];
                        if self.contains(val.text_range())
                            && let Some(kw) = symbol(key)
                            && kw.prefix == Prefix::Keyword
                        {
                            return match kw.name.as_str() {
                                "ACCESSOR" | "READER" => {
                                    self.resolve_declaration_name(val, Namespace::Function)
                                }
                                "WRITER" => self.resolve_function_declaration_name(val, scope),
                                "TYPE" => self.type_specifier(val.clone(), scope),
                                "INITFORM" => self.expression(val.clone(), scope),
                                _ => None,
                            };
                        }
                        idx += 2;
                    }
                }
                None
            }
            Some("DEFSTRUCT") => {
                let list = form.as_node()?;
                if !closed_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let name_el = elems.next()?;
                if self.contains(name_el.text_range()) {
                    if let Some(name_list) = name_el.as_node() {
                        if !closed_list(name_list) {
                            return None;
                        }
                        let mut n_elems = elements(name_list);
                        let first = n_elems.next()?;
                        if self.contains(first.text_range()) {
                            return self.resolve_declaration_name(&first, Namespace::Type);
                        }
                        let opt = n_elems
                            .find(|o| self.contains(o.text_range()))?
                            .into_node()?;
                        if closed_list(&opt) {
                            let mut o_elems = elements(&opt);
                            let kw = symbol(&o_elems.next()?)?;
                            if kw.prefix == Prefix::Keyword {
                                let arg = o_elems.next()?;
                                if self.contains(arg.text_range()) {
                                    return match kw.name.as_str() {
                                        "INCLUDE" => {
                                            self.resolve_symbol(&arg, Namespace::Type, scope)
                                        }
                                        "TYPE" => self.type_specifier(arg, scope),
                                        "CONSTRUCTOR" | "COPIER" | "PREDICATE"
                                            if symbol(&arg).is_some_and(|name| {
                                                self.workspace.standard_symbol(&name, self.package)
                                                    != Some("NIL")
                                            }) =>
                                        {
                                            self.resolve_declaration_name(&arg, Namespace::Function)
                                        }
                                        _ => None,
                                    };
                                }
                            }
                        }
                    } else {
                        return self.resolve_declaration_name(&name_el, Namespace::Type);
                    }
                    return None;
                }
                let slot_el = elems.find(|s| self.contains(s.text_range()))?;
                if let Some(slot_node) = slot_el.as_node()
                    && closed_list(slot_node)
                {
                    let slot_parts: Vec<_> = elements(slot_node).collect();
                    if let Some(init_el) = slot_parts.get(1)
                        && self.contains(init_el.text_range())
                    {
                        return self.expression(init_el.clone(), scope);
                    }
                    let mut idx = 2;
                    while idx + 1 < slot_parts.len() {
                        if self.contains(slot_parts[idx + 1].text_range())
                            && let Some(kw) = symbol(&slot_parts[idx])
                            && kw.prefix == Prefix::Keyword
                            && kw.name == "TYPE"
                        {
                            return self.type_specifier(slot_parts[idx + 1].clone(), scope);
                        }
                        idx += 2;
                    }
                }
                None
            }
            Some("DEFTYPE") => {
                let list = form.as_node()?;
                if !closed_list(list) {
                    return None;
                }
                let mut elems = elements(list);
                let head_el = elems.next()?;
                if self.contains(head_el.text_range()) {
                    return self.resolve_symbol(&head_el, Namespace::Function, scope);
                }
                let name_el = elems.next()?;
                if self.contains(name_el.text_range()) {
                    return self.resolve_declaration_name(&name_el, Namespace::Type);
                }
                let lambda_el = elems.next()?;
                let specials = self.special_declarations(elems.clone(), true);
                let outer_len = scope.len();
                if self.contains(lambda_el.text_range()) {
                    return self
                        .walk_destructuring_lambda_list(&lambda_el, scope, true, &specials)
                        .ok()
                        .flatten();
                }
                self.walk_destructuring_lambda_list(&lambda_el, scope, false, &specials)
                    .ok()?;
                self.named_block(&name_el, scope);
                self.add_free_specials(scope, outer_len, &specials);
                self.body_expression(elems.at_offset(self.offset).next()?, scope)
            }
            Some("DEFPACKAGE") => None,
            _ => self.expression(form, scope),
        }
    }

    fn body_expressions(
        &self,
        forms: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let target = forms
            .iter()
            .find(|child| self.contains(child.text_range()))?;
        self.body_expression(target.clone(), scope)
    }

    fn body_expression(
        &self,
        form: Element,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        if let Some(node) = form.as_node()
            && self.has_standard_head(node, "DECLARE")
        {
            return self.declaration_spec(node, scope);
        }
        self.expression(form, scope)
    }

    fn method_lambda_list(&self, fields: &mut FormElements<'_>) -> Option<Element> {
        for field in fields.by_ref() {
            let Some((field, known)) = self.possibly_read(field) else {
                continue;
            };
            if !known {
                return None;
            }
            if field.kind() == SyntaxKind::List
                || symbol(&field).is_some_and(|name| {
                    self.workspace.standard_symbol(&name, self.package) == Some("NIL")
                })
            {
                return Some(field);
            }
            if matches!(
                field.kind(),
                SyntaxKind::ReaderForm | SyntaxKind::UnknownDispatch | SyntaxKind::Error
            ) {
                return None;
            }
        }
        None
    }

    fn inline_method<'a>(&self, option: &'a SyntaxNode) -> Option<FormElements<'a>> {
        if !non_dotted_list(option) {
            return None;
        }
        let mut fields = elements(option);
        let tag = symbol(&fields.next()?)?;
        (tag.name == "METHOD"
            && self.workspace.symbol_home_package(&tag, self.package) == Some("KEYWORD"))
        .then_some(fields)
    }

    fn resolve_generic_options(
        &self,
        name: &Element,
        mut fields: FormElements<'_>,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let _generic_lambda_list = fields.next()?;
        let (option, known) = self.possibly_read(fields.at_offset(self.offset).next()?)?;
        if !known {
            return None;
        }
        let option = option.as_node().filter(|node| closed_list(node))?;
        self.resolve_method(name, self.inline_method(option)?, scope)
    }

    fn resolve_method(
        &self,
        name: &Element,
        mut fields: FormElements<'_>,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let lambda = self.method_lambda_list(&mut fields)?;
        let specials = self.special_declarations(fields.clone(), true);
        if self.contains(lambda.text_range()) {
            return self
                .walk_specialized_lambda_list(&lambda, scope, MethodScope::Resolve, &specials)
                .ok()?;
        }
        let outer_len = scope.len();
        self.walk_specialized_lambda_list(&lambda, scope, MethodScope::Body, &specials)
            .ok()?;
        self.named_block(name, scope);
        self.add_free_specials(scope, outer_len, &specials);
        self.body_expression(fields.at_offset(self.offset).next()?, scope)
    }

    pub(super) fn declaration_spec(
        &self,
        decl_list: &SyntaxNode,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        if !closed_list(decl_list) {
            return None;
        }
        let spec = elements(decl_list)
            .skip(1)
            .find(|el| self.contains(el.text_range()))?
            .into_node()?;
        if !closed_list(&spec) {
            return None;
        }
        let mut parts = elements(&spec);
        let head = parts.next()?;
        let head_sym = symbol(&head)?;
        let std_head = self.workspace.standard_symbol(&head_sym, self.package)?;
        match std_head {
            "SPECIAL" => {
                let target = parts.at_offset(self.offset).next()?;
                let spelling = symbol(&target)?;
                self.resolve_local(
                    Namespace::SpecialValue,
                    &spelling,
                    target.text_range(),
                    scope,
                )
            }
            "IGNORE" | "IGNORABLE" | "DYNAMIC-EXTENT" => {
                let target = parts.at_offset(self.offset).next()?;
                if let Some(node) = target.as_node()
                    && closed_list(node)
                {
                    let mut fn_parts = elements(node);
                    let fn_head = fn_parts.next()?;
                    let fn_sym = symbol(&fn_head)?;
                    if self.workspace.standard_symbol(&fn_sym, self.package) == Some("FUNCTION") {
                        let arg = fn_parts.next()?;
                        if self.contains(arg.text_range()) {
                            return self.function_designator(arg, scope);
                        }
                    }
                    return None;
                }
                self.resolve_symbol(&target, Namespace::Value, scope)
            }
            "INLINE" | "NOTINLINE" => {
                let target = parts.at_offset(self.offset).next()?;
                self.function_designator(target, scope)
            }
            "FTYPE" => {
                let _ftype_el = parts.next()?;
                let target = parts.at_offset(self.offset).next()?;
                self.function_designator(target, scope)
            }
            "TYPE" => {
                let type_el = parts.next()?;
                if self.contains(type_el.text_range()) {
                    return self.type_specifier(type_el, scope);
                }
                let var_el = parts.at_offset(self.offset).next()?;
                self.resolve_symbol(&var_el, Namespace::Value, scope)
            }
            "OPTIMIZE" | "DECLARATION" => None,
            _ => {
                let var_el = parts.at_offset(self.offset).next()?;
                self.resolve_symbol(&var_el, Namespace::Value, scope)
            }
        }
    }

    fn type_specifier(&self, form: Element, scope: &[ScopeBinding]) -> Option<OccurrenceBinding> {
        if form.as_token().is_some() {
            return self.resolve_symbol(&form, Namespace::Type, scope);
        }
        let list = form.into_node()?;
        if !closed_list(&list) {
            return None;
        }
        let mut parts = elements(&list);
        let head = parts.next()?;
        if self.contains(head.text_range()) {
            return self.resolve_symbol(&head, Namespace::Type, scope);
        }
        let head_sym = symbol(&head)?;
        match self.workspace.standard_symbol(&head_sym, self.package) {
            Some("OR" | "AND" | "NOT" | "VALUES") => {
                self.type_specifier(parts.at_offset(self.offset).next()?, scope)
            }
            Some("SATISFIES") => {
                let predicate = parts.next()?;
                // SATISFIES names a global function, even inside FLET or LABELS.
                self.resolve_symbol(&predicate, Namespace::Function, &[])
            }
            _ => None,
        }
    }

    fn expression(
        &self,
        form: Element,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        if form.as_token().is_some() {
            return self.resolve_symbol(&form, Namespace::Value, scope);
        }

        let list = form.into_node()?;
        if list.kind() == SyntaxKind::ReaderForm {
            let first = list.children_with_tokens().next()?.into_token()?;
            match first.kind() {
                SyntaxKind::FunctionQuote => {
                    let target = elements(&list).nth(1)?;
                    if self.contains(target.text_range()) {
                        return self.function_designator(target, scope);
                    }
                }
                SyntaxKind::Backquote => {
                    let target = elements(&list).nth(1)?;
                    if self.contains(target.text_range()) {
                        return self.backquote_form(target, scope, 1);
                    }
                }
                SyntaxKind::ReaderConditional => {
                    let (guarded, known) = self.possibly_read(list.into())?;
                    if known && self.contains(guarded.text_range()) {
                        return self.expression(guarded, scope);
                    }
                }
                _ => {}
            }
            return None;
        }
        if !closed_list(&list) {
            return None;
        }
        let mut parts = elements(&list);
        let head = parts.next()?;
        if let Some(head_node) = head.as_node() {
            if self.contains(head_node.text_range()) {
                return self.lambda_form(head_node, scope);
            }
            if crate::outline::with_list_operator(head_node, |op| op == "LAMBDA").unwrap_or(false) {
                let arg = parts.at_offset(self.offset).next()?;
                return self.expression(arg, scope);
            }
            return None;
        }
        let spelling = symbol(&head)?;
        let local_evaluates_args = scope
            .iter()
            .rev()
            .find(|b| b.namespace == Namespace::Function && self.same_symbol(b, &spelling))
            .map(|b| b.evaluates_arguments && b.def_range.is_some());
        let standard = if local_evaluates_args.is_none() {
            self.workspace.standard_symbol(&spelling, self.package)
        } else {
            None
        };
        if standard == Some("LOOP") {
            if let Some((block, body)) = self.loop_body(&head, parts)
                && let Some(range) = block.def_range
            {
                if self.contains(range) {
                    return Some(self.local_target(block.namespace, block.spelling, range, None));
                }
                if let Some(body) = body {
                    scope.push(block);
                    return self.expression(body, scope);
                }
            }
            return self
                .contains(head.text_range())
                .then(|| self.resolve_symbol(&head, Namespace::Function, scope))
                .flatten();
        }
        if self.contains(head.text_range()) {
            if standard == Some("RETURN")
                && parts.clone().nth(1).is_none()
                && let Some(target) = self.resolve_local(
                    Namespace::Block,
                    &nil_block_name(),
                    head.text_range(),
                    scope,
                )
            {
                return Some(target);
            }
            if matches!(
                standard,
                Some(
                    "DO" | "DO*"
                        | "PROG"
                        | "PROG*"
                        | "DOLIST"
                        | "DOTIMES"
                        | "DO-SYMBOLS"
                        | "DO-EXTERNAL-SYMBOLS"
                        | "DO-ALL-SYMBOLS"
                )
            ) {
                return Some(self.local_target(
                    Namespace::ImplicitBlock,
                    nil_block_name(),
                    head.text_range(),
                    None,
                ));
            }
            return self.resolve_symbol(&head, Namespace::Function, scope);
        }
        if let Some(operands) = self
            .macro_operands
            .and_then(|operands| operands.get(&list.text_range()))
        {
            let next = operands.partition_point(|form| form.text_range().start() <= self.offset);
            let form = operands[..next]
                .last()
                .filter(|form| form.text_range().contains(self.offset))?;
            return self.expression(form.clone(), scope);
        }
        let expression = parts.clone().at_offset(self.offset).next()?;
        let rest = OnceCell::new();
        let args = || {
            rest.get_or_init(|| parts.clone().collect::<Vec<_>>())
                .as_slice()
        };
        let position = || {
            args()
                .iter()
                .position(|arg| arg.index() == expression.index())
                .expect("the selected expression is an argument")
        };

        if local_evaluates_args.is_none() && self.zk_symbol(&spelling) {
            match spelling.name.as_str() {
                "MATCH" | "EMATCH" | "CMATCH" => return self.match_call(args(), scope),
                "IF-MATCH" | "WHEN-MATCH" | "UNLESS-MATCH" => {
                    return self.single_pattern_call(&spelling.name, args(), scope);
                }
                "LET-MATCH" | "LET-MATCH*" => {
                    return self.match_bindings(args(), scope, spelling.name == "LET-MATCH*");
                }
                _ => {}
            }
        }

        if local_evaluates_args.is_none() && self.package_symbol(&spelling, "ZK-LEX") {
            match spelling.name.as_str() {
                "LEXMATCH" | "LEXSCAN" => return self.lexical_call(args(), scope),
                _ => {}
            }
        }

        if local_evaluates_args.is_none()
            && self.workspace.symbol_home_package(&spelling, self.package)
                == Some(crate::treelist_builtins::PACKAGE)
        {
            match spelling.name.as_str() {
                "WITH-TRANSIENT" => return self.with_stream_form(args(), scope),
                "DO-ELEMENTS" => return self.iteration_form(&head, args(), scope, true, false),
                _ => {}
            }
        }

        if let Some(form) = standard.and_then(VariableBindingForm::from_operator) {
            return self.variable_binding_form(&head, parts, scope, form);
        }
        if let Some(form) = standard.and_then(LocalCallableForm::from_operator) {
            return self.local_callable_form(parts, scope, form);
        }

        match standard {
            Some("QUOTE" | "DECLARE" | "DECLAIM") => return None,
            Some("DEFGENERIC" | "DEFMETHOD") => {
                let name = parts.next()?;
                if self.contains(name.text_range()) {
                    return self.function_designator(name, scope);
                }
                if standard == Some("DEFGENERIC") {
                    return self.resolve_generic_options(&name, parts, scope);
                }
                return self.resolve_method(&name, parts, scope);
            }
            Some("LOCALLY") => {
                let specials = self.special_declarations(parts, false);
                let outer_len = scope.len();
                self.add_free_specials(scope, outer_len, &specials);
                return self.body_expression(expression, scope);
            }
            Some("EVAL-WHEN") => {
                if position() == 0 {
                    return None;
                }
                return self.body_expressions(&args()[1..], scope);
            }
            Some("DO-SYMBOLS" | "DO-EXTERNAL-SYMBOLS") => {
                return self.iteration_form(&head, args(), scope, true, false);
            }
            Some("DO-ALL-SYMBOLS") => {
                return self.iteration_form(&head, args(), scope, false, false);
            }
            Some("CHECK-TYPE") => {
                if position() == 0 {
                    return self.expression(expression, scope);
                } else if position() == 1 {
                    return self.type_specifier(expression, scope);
                } else if position() == 2 && args().len() == 3 {
                    return self.expression(expression, scope);
                }
                return None;
            }
            Some("TYPEP" | "COERCE") => {
                if position() == 0 && args().len() == 2 {
                    return self.expression(expression, scope);
                } else if position() == 1 && args().len() == 2 {
                    if let Some(rf) = expression.as_node()
                        && rf.kind() == SyntaxKind::ReaderForm
                        && rf
                            .first_token()
                            .is_some_and(|t| t.kind() == SyntaxKind::Quote)
                        && let Some(inner) = elements(rf).nth(1)
                        && self.contains(inner.text_range())
                    {
                        return self.type_specifier(inner, scope);
                    }
                    return self.expression(expression, scope);
                }
                return None;
            }
            Some("MULTIPLE-VALUE-SETQ") => {
                if position() == 0 {
                    let vars_node = expression.as_node()?;
                    if !closed_list(vars_node) {
                        return None;
                    }
                    let var_el = elements(vars_node).find(|v| self.contains(v.text_range()))?;
                    return self.resolve_symbol(&var_el, Namespace::Value, scope);
                } else if position() == 1 && args().len() == 2 {
                    return self.expression(expression, scope);
                }
                return None;
            }
            Some("PRINT-UNREADABLE-OBJECT" | "PPRINT-LOGICAL-BLOCK") => {
                if position() == 0 {
                    let spec_node = expression.as_node()?;
                    if !closed_list(spec_node) {
                        return None;
                    }
                    let sub = elements(spec_node).find(|e| self.contains(e.text_range()))?;
                    return self.expression(sub, scope);
                }
                return self.body_expressions(&args()[1..], scope);
            }
            Some("WITH-SIMPLE-RESTART" | "WITH-COMPILATION-UNIT") => {
                if position() == 0 {
                    let spec_node = expression.as_node()?;
                    if !closed_list(spec_node) {
                        return None;
                    }
                    let skip = usize::from(standard == Some("WITH-SIMPLE-RESTART"));
                    let sub = elements(spec_node)
                        .skip(skip)
                        .find(|e| self.contains(e.text_range()))?;
                    return self.expression(sub, scope);
                }
                return self.body_expressions(&args()[1..], scope);
            }
            Some("WITH-PACKAGE-ITERATOR" | "WITH-HASH-TABLE-ITERATOR") => {
                return self.with_iterator_form(args(), scope);
            }
            Some("FUNCTION") => {
                return (position() == 0 && args().len() == 1)
                    .then(|| self.function_designator(expression, scope))
                    .flatten();
            }
            Some("THE") => {
                if position() == 0 {
                    return self.type_specifier(expression, scope);
                } else if position() == 1 && args().len() == 2 {
                    return self.expression(expression, scope);
                }
                return None;
            }
            Some("LAMBDA") => return self.lambda_form(&list, scope),
            Some("SYMBOL-MACROLET") => return self.symbol_macrolet_form(args(), scope),
            Some("MULTIPLE-VALUE-BIND") => return self.mvb_form(args(), scope),
            Some("DESTRUCTURING-BIND") => return self.destructuring_bind_form(args(), scope),
            Some("DOLIST" | "DOTIMES") => {
                return self.iteration_form(&head, args(), scope, true, true);
            }
            Some(
                "WITH-OPEN-FILE"
                | "WITH-OPEN-STREAM"
                | "WITH-INPUT-FROM-STRING"
                | "WITH-OUTPUT-TO-STRING",
            ) => return self.with_stream_form(args(), scope),
            Some("WITH-SLOTS") => return self.with_slots_form(args(), scope, false),
            Some("WITH-ACCESSORS") => return self.with_slots_form(args(), scope, true),
            Some("RESTART-CASE") => return self.restart_case_form(args(), scope),
            Some("BLOCK") => return self.block_form(args(), scope),
            Some("RETURN-FROM") => return self.return_from_form(args(), scope),
            Some("RETURN") => {
                return (args().len() == 1)
                    .then(|| self.expression(expression, scope))
                    .flatten();
            }
            Some("TAGBODY") => return self.tagbody_form(args(), scope, false),
            Some("GO") => {
                if position() != 0 || args().len() != 1 {
                    return None;
                }
                let (namespace, spelling) = tag_name(&expression)?;
                return self.resolve_local(namespace, &spelling, expression.text_range(), scope);
            }
            Some("SETQ" | "PSETQ") => {
                if !args().len().is_multiple_of(2) {
                    return None;
                }
                return self.expression(expression, scope);
            }
            Some("COND") => {
                let clause = expression.as_node()?;
                if !closed_list(clause) {
                    return None;
                }
                let sub = elements(clause).find(|e| self.contains(e.text_range()))?;
                return self.expression(sub, scope);
            }
            Some("CASE" | "ECASE" | "CCASE") => {
                if position() == 0 {
                    return self.expression(expression, scope);
                }
                let clause = expression.as_node()?;
                if !closed_list(clause) {
                    return None;
                }
                let sub = elements(clause)
                    .skip(1)
                    .find(|e| self.contains(e.text_range()))?;
                return self.expression(sub, scope);
            }
            Some("TYPECASE" | "ETYPECASE" | "CTYPECASE") => {
                if position() == 0 {
                    return self.expression(expression, scope);
                }
                let clause = expression.as_node()?;
                if !closed_list(clause) {
                    return None;
                }
                let mut c_elems = elements(clause);
                let type_sel = c_elems.next()?;
                if self.contains(type_sel.text_range()) {
                    return self.type_specifier(type_sel, scope);
                }
                let sub = c_elems.find(|e| self.contains(e.text_range()))?;
                return self.expression(sub, scope);
            }
            Some("HANDLER-BIND" | "RESTART-BIND") => {
                if position() != 0 {
                    return self.body_expressions(&args()[1..], scope);
                }
                let bindings = expression.as_node()?;
                let binding = elements(bindings)
                    .at_offset(self.offset)
                    .next()?
                    .into_node()?;
                if !closed_list(&binding) {
                    return None;
                }
                let mut entries = elements(&binding);
                let name = entries.next()?;
                if self.contains(name.text_range()) {
                    return if standard == Some("HANDLER-BIND") {
                        self.type_specifier(name, scope)
                    } else {
                        None
                    };
                }
                let function = entries.next()?;
                if self.contains(function.text_range()) {
                    return self.expression(function, scope);
                }
                if standard == Some("RESTART-BIND") {
                    // Every option value is evaluated in the enclosing environment.
                    return self.expression(
                        entries
                            .skip(1)
                            .step_by(2)
                            .find(|value| self.contains(value.text_range()))?,
                        scope,
                    );
                }
                return None;
            }
            Some("HANDLER-CASE") => {
                if position() == 0 {
                    return self.expression(expression, scope);
                }
                let clause = expression.as_node()?;
                if !closed_list(clause) {
                    return None;
                }
                let mut c_elems = elements(clause);
                let cond_type = c_elems.next()?;
                if self.contains(cond_type.text_range()) {
                    if symbol(&cond_type).is_some_and(|s| s.prefix == Prefix::Keyword) {
                        return None;
                    }
                    return self.type_specifier(cond_type, scope);
                }
                let params_el = c_elems.next()?;
                let specials = self.special_declarations(c_elems.clone(), false);
                let outer_len = scope.len();
                if self.contains(params_el.text_range()) {
                    return self
                        .walk_ordinary_lambda_list(&params_el, scope, true, &specials)
                        .ok()
                        .flatten();
                }
                self.walk_ordinary_lambda_list(&params_el, scope, false, &specials)
                    .ok()?;
                self.add_free_specials(scope, outer_len, &specials);
                return self.body_expression(c_elems.at_offset(self.offset).next()?, scope);
            }
            _ => {}
        }

        let evaluates_args = if let Some(evaluates) = local_evaluates_args {
            evaluates
        } else if !self.workspace.may_have_declaration(&spelling.name) {
            standard.is_some_and(evaluates_all_arguments)
        } else {
            let standard_evaluates_args = standard.is_some_and(evaluates_all_arguments);
            let function = self.name(spelling, Namespace::Function);
            self.workspace
                .lookup_unique_declaration(&function)
                .map_or(standard_evaluates_args, |(_, definition)| {
                    definition.evaluates_arguments
                })
        };
        evaluates_args.then_some(())?;
        self.expression(expression, scope)
    }

    fn function_designator(
        &self,
        target: Element,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        if target.as_token().is_some() {
            return self.resolve_symbol(&target, Namespace::Function, scope);
        }
        let node = target.into_node()?;
        if !closed_list(&node) {
            return None;
        }
        let mut parts = elements(&node);
        let first = parts.next()?;
        let first_sym = symbol(&first)?;
        match self.workspace.standard_symbol(&first_sym, self.package) {
            Some("LAMBDA") => self.lambda_form(&node, scope),
            Some("SETF") => {
                let second = parts.next()?;
                if parts.next().is_some() {
                    return None;
                }
                if self.contains(first.text_range()) {
                    return self.resolve_symbol(&first, Namespace::Function, scope);
                }
                if self.contains(second.text_range()) {
                    return self.resolve_symbol(&second, Namespace::SetfFunction, scope);
                }
                None
            }
            _ => None,
        }
    }

    fn extract_fn_designator_name(
        &self,
        element: &Element,
    ) -> Option<(Namespace, SymbolName, TextRange)> {
        if let Some(sym) = symbol(element) {
            return Some((Namespace::Function, sym, element.text_range()));
        }
        parse_setf_function_name(element).map(|(sym, range)| (Namespace::SetfFunction, sym, range))
    }

    fn resolve_function_declaration_name(
        &self,
        element: &Element,
        scope: &[ScopeBinding],
    ) -> Option<OccurrenceBinding> {
        if element.as_token().is_some() {
            return self.resolve_declaration_name(element, Namespace::Function);
        }
        let node = element.as_node()?;
        if !closed_list(node) {
            return None;
        }
        let mut parts = elements(node);
        let first = parts.next()?;
        let first_sym = symbol(&first)?;
        if self.workspace.standard_symbol(&first_sym, self.package) != Some("SETF") {
            return None;
        }
        let second = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        if self.contains(first.text_range()) {
            return self.resolve_symbol(&first, Namespace::Function, scope);
        }
        if self.contains(second.text_range()) {
            return self.resolve_declaration_name(&second, Namespace::SetfFunction);
        }
        None
    }

    fn lambda_form(
        &self,
        list: &SyntaxNode,
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        if !closed_list(list) {
            return None;
        }
        let mut elems = elements(list);
        let head = elems.next()?;
        if self.contains(head.text_range()) {
            return self.resolve_symbol(&head, Namespace::Function, scope);
        }
        let lambda_el = elems.next()?;
        let specials = self.special_declarations(elems.clone(), true);
        let outer_len = scope.len();
        if self.contains(lambda_el.text_range()) {
            return self
                .walk_ordinary_lambda_list(&lambda_el, scope, true, &specials)
                .ok()
                .flatten();
        }
        self.walk_ordinary_lambda_list(&lambda_el, scope, false, &specials)
            .ok()?;
        self.add_free_specials(scope, outer_len, &specials);
        self.body_expression(elems.at_offset(self.offset).next()?, scope)
    }

    fn backquote_form(
        &self,
        form: Element,
        scope: &mut Vec<ScopeBinding>,
        depth: usize,
    ) -> Option<OccurrenceBinding> {
        let node = form.into_node()?;
        match node.kind() {
            SyntaxKind::ReaderForm => {
                let first = node.children_with_tokens().next()?.into_token()?;
                match first.kind() {
                    SyntaxKind::Backquote => {
                        let inner = elements(&node).nth(1)?;
                        if self.contains(inner.text_range()) {
                            return self.backquote_form(inner, scope, depth + 1);
                        }
                    }
                    SyntaxKind::Unquote
                    | SyntaxKind::UnquoteSplicing
                    | SyntaxKind::UnquoteDestructive => {
                        let inner = elements(&node).nth(1)?;
                        if self.contains(inner.text_range()) {
                            if depth <= 1 {
                                return self.expression(inner, scope);
                            }
                            return self.backquote_form(inner, scope, depth - 1);
                        }
                    }
                    SyntaxKind::ReaderConditional => {
                        let guarded = elements(&node).nth(2)?;
                        if self.contains(guarded.text_range()) {
                            return self.backquote_form(guarded, scope, depth);
                        }
                    }
                    _ => {}
                }
                None
            }
            SyntaxKind::List | SyntaxKind::Vector => {
                let child = elements(&node)
                    .at_offset(self.offset)
                    .find(|child| self.contains(child.text_range()))?;
                self.backquote_form(child, scope, depth)
            }
            _ => None,
        }
    }

    fn iteration_form(
        &self,
        head: &Element,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
        has_initializer: bool,
        requires_initializer: bool,
    ) -> Option<OccurrenceBinding> {
        let [spec_el, body @ ..] = rest else {
            return None;
        };
        let spec_node = spec_el.as_node()?;
        if !closed_list(spec_node) {
            return None;
        }
        let mut parts = elements(spec_node);
        let var_el = parts.next()?;
        let sym = symbol(&var_el)?;
        let specials = self.special_declarations(body.iter().cloned(), false);
        if self.contains(var_el.text_range()) {
            return Some(self.variable_target(sym, var_el.text_range(), &specials));
        }
        let outer_len = scope.len();
        scope.push(self.implicit_nil_block(head));
        if has_initializer {
            let initializer = parts.next();
            if requires_initializer && initializer.is_none() {
                return None;
            }
            if let Some(initializer) = initializer
                && self.contains(initializer.text_range())
            {
                return self.expression(initializer, scope);
            }
        }
        let result_expr = parts.next();
        if parts.next().is_some() {
            return None;
        }
        scope.push(self.variable_binding(sym, var_el.text_range(), &specials));
        self.add_free_specials(scope, outer_len, &specials);
        if let Some(res) = result_expr
            && self.contains(res.text_range())
        {
            return self.expression(res, scope);
        }
        self.tagbody_form(body, scope, true)
    }

    fn with_iterator_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let [spec_el, body @ ..] = rest else {
            return None;
        };
        let spec_node = spec_el.as_node()?;
        if !closed_list(spec_node) {
            return None;
        }
        let mut parts = elements(spec_node);
        let name_el = parts.next()?;
        let sym = symbol(&name_el)?;
        if self.contains(name_el.text_range()) {
            return Some(self.local_target(
                Namespace::Function,
                sym,
                name_el.text_range(),
                Some(std::sync::Arc::from([])),
            ));
        }
        if let Some(source_expr) = parts.next()
            && self.contains(source_expr.text_range())
        {
            return self.expression(source_expr, scope);
        }
        scope.push(ScopeBinding {
            namespace: Namespace::Function,
            spelling: sym,
            package: self.package.map(str::to_owned),
            def_range: Some(name_el.text_range()),
            params: Some(std::sync::Arc::from([])),
            evaluates_arguments: true,
        });
        self.body_expressions(body, scope)
    }

    fn symbol_macrolet_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let (defs_el, body) = rest.split_first()?;
        let defs_node = defs_el.as_node()?;
        if !closed_list(defs_node) {
            return None;
        }
        let outer_len = scope.len();
        for def_el in elements(defs_node) {
            let def_node = def_el.into_node()?;
            if !closed_list(&def_node) {
                return None;
            }
            let mut parts = elements(&def_node);
            let name_el = parts.next()?;
            let sym = symbol(&name_el)?;
            let exp_el = parts.next()?;
            if self.contains(name_el.text_range()) {
                return Some(self.local_target(Namespace::Value, sym, name_el.text_range(), None));
            }
            if self.contains(exp_el.text_range()) {
                scope.truncate(outer_len);
                return self.expression(exp_el, scope);
            }
            scope.push(self.scope_binding(Namespace::Value, sym, name_el.text_range()));
        }
        self.body_expressions(body, scope)
    }

    fn mvb_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let [vars_el, values_el, body @ ..] = rest else {
            return None;
        };
        if self.contains(values_el.text_range()) {
            return self.expression(values_el.clone(), scope);
        }
        let vars_node = vars_el.as_node()?;
        if !closed_list(vars_node) {
            return None;
        }
        let outer_len = scope.len();
        let specials = self.special_declarations(body.iter().cloned(), false);
        for var_el in elements(vars_node) {
            let sym = symbol(&var_el)?;
            if self.contains(var_el.text_range()) {
                return Some(self.variable_target(sym, var_el.text_range(), &specials));
            }
            scope.push(self.variable_binding(sym, var_el.text_range(), &specials));
        }
        self.add_free_specials(scope, outer_len, &specials);
        self.body_expressions(body, scope)
    }

    fn destructuring_bind_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let [pattern_el, expr_el, body @ ..] = rest else {
            return None;
        };
        if self.contains(expr_el.text_range()) {
            return self.expression(expr_el.clone(), scope);
        }
        let specials = self.special_declarations(body.iter().cloned(), false);
        let outer_len = scope.len();
        if self.contains(pattern_el.text_range()) {
            return self
                .walk_destructuring_lambda_list(pattern_el, scope, true, &specials)
                .ok()
                .flatten();
        }
        self.walk_destructuring_lambda_list(pattern_el, scope, false, &specials)
            .ok()?;
        self.add_free_specials(scope, outer_len, &specials);
        self.body_expressions(body, scope)
    }

    fn with_stream_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let [spec_el, body @ ..] = rest else {
            return None;
        };
        let spec_node = spec_el.as_node()?;
        if !closed_list(spec_node) {
            return None;
        }
        let mut parts = elements(spec_node);
        let var_el = parts.next()?;
        let sym = symbol(&var_el)?;
        let specials = self.special_declarations(body.iter().cloned(), false);
        if self.contains(var_el.text_range()) {
            return Some(self.variable_target(sym, var_el.text_range(), &specials));
        }
        for arg in parts {
            if self.contains(arg.text_range()) {
                return self.expression(arg, scope);
            }
        }
        let outer_len = scope.len();
        scope.push(self.variable_binding(sym, var_el.text_range(), &specials));
        self.add_free_specials(scope, outer_len, &specials);
        self.body_expressions(body, scope)
    }

    fn with_slots_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
        accessors: bool,
    ) -> Option<OccurrenceBinding> {
        let [entries_el, instance_el, body @ ..] = rest else {
            return None;
        };
        if self.contains(instance_el.text_range()) {
            return self.expression(instance_el.clone(), scope);
        }
        let entries_node = entries_el.as_node()?;
        if !closed_list(entries_node) {
            return None;
        }
        let outer_len = scope.len();
        for entry in elements(entries_node) {
            if let Some(tok) = entry.as_token() {
                if accessors {
                    return None;
                }
                let sym = symbol(&entry)?;
                if self.contains(tok.text_range()) {
                    return Some(self.local_target(Namespace::Value, sym, tok.text_range(), None));
                }
                scope.push(self.scope_binding(Namespace::Value, sym, tok.text_range()));
            } else {
                let entry_node = entry.as_node()?;
                if !closed_list(entry_node) {
                    return None;
                }
                let mut parts = elements(entry_node);
                let var_el = parts.next()?;
                let target_el = parts.next()?;
                if parts.next().is_some() {
                    return None;
                }
                let sym = symbol(&var_el)?;
                if self.contains(var_el.text_range()) {
                    return Some(self.local_target(
                        Namespace::Value,
                        sym,
                        var_el.text_range(),
                        None,
                    ));
                }
                if accessors && self.contains(target_el.text_range()) {
                    return self.resolve_symbol(
                        &target_el,
                        Namespace::Function,
                        &scope[..outer_len],
                    );
                }
                scope.push(self.scope_binding(Namespace::Value, sym, var_el.text_range()));
            }
        }
        self.body_expressions(body, scope)
    }

    fn restart_case_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let [expr_el, clauses @ ..] = rest else {
            return None;
        };
        if self.contains(expr_el.text_range()) {
            return self.expression(expr_el.clone(), scope);
        }
        let clause_el = clauses.iter().find(|c| self.contains(c.text_range()))?;
        let clause = clause_el.as_node()?;
        if !closed_list(clause) {
            return None;
        }
        let mut parts = elements(clause);
        let _case_name = parts.next()?;
        let lambda_el = parts.next()?;
        let rest_parts: Vec<_> = parts.collect();
        let mut idx = 0;
        while idx + 1 < rest_parts.len() {
            if restart_case_option(&rest_parts[idx]) {
                let arg = &rest_parts[idx + 1];
                if self.contains(arg.text_range()) {
                    if arg.as_token().is_some() && symbol(arg).is_some() {
                        return self.resolve_symbol(arg, Namespace::Function, scope);
                    }
                    return self.expression(arg.clone(), scope);
                }
                idx += 2;
            } else {
                break;
            }
        }
        let specials = self.special_declarations(rest_parts[idx..].iter().cloned(), false);
        let outer_len = scope.len();
        if self.contains(lambda_el.text_range()) {
            return self
                .walk_ordinary_lambda_list(&lambda_el, scope, true, &specials)
                .ok()
                .flatten();
        }
        self.walk_ordinary_lambda_list(&lambda_el, scope, false, &specials)
            .ok()?;
        self.add_free_specials(scope, outer_len, &specials);
        self.body_expressions(&rest_parts[idx..], scope)
    }

    fn block_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let (name_el, body) = rest.split_first()?;
        let sym = control_symbol(name_el)?;
        if self.contains(name_el.text_range()) {
            return Some(self.local_target(Namespace::Block, sym, name_el.text_range(), None));
        }
        scope.push(self.scope_binding(Namespace::Block, sym, name_el.text_range()));
        let target = body.iter().find(|form| self.contains(form.text_range()))?;
        self.expression(target.clone(), scope)
    }

    fn return_from_form(
        &self,
        rest: &[Element],
        scope: &mut Vec<ScopeBinding>,
    ) -> Option<OccurrenceBinding> {
        let (name_el, value) = rest.split_first()?;
        if value.len() > 1 {
            return None;
        }
        if self.contains(name_el.text_range()) {
            let spelling = control_symbol(name_el)?;
            return self.resolve_local(Namespace::Block, &spelling, name_el.text_range(), scope);
        }
        if let [val_el] = value
            && self.contains(val_el.text_range())
        {
            return self.expression(val_el.clone(), scope);
        }
        None
    }

    fn tagbody_form(
        &self,
        statements: &[Element],
        scope: &mut Vec<ScopeBinding>,
        declarations_allowed: bool,
    ) -> Option<OccurrenceBinding> {
        self.add_tag_bindings(statements.iter().cloned(), scope);
        let target = statements
            .iter()
            .find(|stmt| self.contains(stmt.text_range()))?;
        let (target, known) = self.possibly_read(target.clone())?;
        if !known || !self.contains(target.text_range()) {
            return None;
        }
        if let Some((namespace, spelling)) = tag_name(&target) {
            return Some(self.local_target(namespace, spelling, target.text_range(), None));
        }
        if declarations_allowed
            && let Some(node) = target.as_node()
            && self.has_standard_head(node, "DECLARE")
        {
            return self.declaration_spec(node, scope);
        }
        self.expression(target, scope)
    }

    fn add_tag_bindings(
        &self,
        statements: impl IntoIterator<Item = Element>,
        scope: &mut Vec<ScopeBinding>,
    ) {
        for statement in statements {
            let Some((statement, known)) = self.possibly_read(statement) else {
                continue;
            };
            if let Some((namespace, spelling)) = tag_name(&statement) {
                let mut binding = self.scope_binding(namespace, spelling, statement.text_range());
                if !known {
                    binding.def_range = None;
                }
                scope.push(binding);
            }
        }
    }

    fn resolve_declaration_name(
        &self,
        element: &Element,
        namespace: Namespace,
    ) -> Option<OccurrenceBinding> {
        let spelling = symbol(element)?;
        Some(OccurrenceBinding::Global {
            name: self.name(spelling, namespace),
            occurrence_range: element.text_range(),
            is_declaration: true,
        })
    }

    fn resolve_symbol(
        &self,
        element: &Element,
        namespace: Namespace,
        scope: &[ScopeBinding],
    ) -> Option<OccurrenceBinding> {
        let spelling = symbol(element)?;
        if let Some(target) = self.resolve_local(namespace, &spelling, element.text_range(), scope)
        {
            return Some(target);
        }
        Some(OccurrenceBinding::Global {
            name: self.name(spelling, namespace),
            occurrence_range: element.text_range(),
            is_declaration: false,
        })
    }

    fn resolve_local(
        &self,
        namespace: Namespace,
        spelling: &SymbolName,
        occurrence_range: TextRange,
        scope: &[ScopeBinding],
    ) -> Option<OccurrenceBinding> {
        if let Some(binding) = scope.iter().rev().find(|b| {
            (b.namespace == namespace
                || namespace == Namespace::Block && b.namespace == Namespace::ImplicitBlock
                || namespace == Namespace::Value && b.namespace == Namespace::SpecialValue)
                && self.same_symbol(b, spelling)
        }) {
            if namespace == Namespace::Value
                && (binding.namespace == Namespace::SpecialValue
                    || self.workspace.is_proclaimed_special(spelling, self.package))
            {
                // The declaration determines how the name is looked up. The
                // active binding depends on the runtime's dynamic environment.
                return Some(OccurrenceBinding::LocalWithoutSite);
            }
            return match binding.def_range {
                Some(def_range) => Some(OccurrenceBinding::LocalSite {
                    def_range,
                    namespace: binding.namespace,
                    spelling: binding.spelling.clone(),
                    package: binding.package.clone(),
                    params: binding.params.clone(),
                    occurrence_range,
                }),
                None => Some(OccurrenceBinding::LocalWithoutSite),
            };
        }
        None
    }

    fn name(&self, spelling: SymbolName, namespace: Namespace) -> Name {
        Name {
            namespace,
            spelling,
            package: self.package.map(str::to_owned),
        }
    }

    pub(crate) fn same_symbol(&self, binding: &ScopeBinding, right: &SymbolName) -> bool {
        self.symbols_match(&binding.spelling, binding.package.as_deref(), right)
    }

    fn symbols_match(
        &self,
        left: &SymbolName,
        left_context: Option<&str>,
        right: &SymbolName,
    ) -> bool {
        if left.name != right.name {
            return false;
        }
        if left.prefix == Prefix::None
            && right.prefix == Prefix::None
            && left_context == self.package
        {
            return true;
        }
        let left_package = self.workspace.symbol_home_package(left, left_context);
        left_package.is_some()
            && left_package == self.workspace.symbol_home_package(right, self.package)
    }
    fn implicit_nil_block(&self, head: &Element) -> ScopeBinding {
        self.scope_binding(
            Namespace::ImplicitBlock,
            nil_block_name(),
            head.text_range(),
        )
    }

    fn named_block(&self, name: &Element, scope: &mut Vec<ScopeBinding>) {
        if let Some((_, spelling, range)) = self.extract_fn_designator_name(name) {
            scope.push(self.scope_binding(Namespace::Block, spelling, range));
        }
    }
}
