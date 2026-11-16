use crate::outline::{Element, FormElements, elements};

use super::{
    Namespace, OccurrenceBinding, ScopeAtOffset, ScopeBinding, SymbolName, SyntaxNode, TextRange,
    closed_list, non_dotted_list, symbol,
};

#[derive(Clone, Copy)]
enum BindingBody {
    Expressions,
    Tagbody,
    Iteration,
}

pub(super) struct VariableBindingForm {
    body: BindingBody,
    sequential: bool,
}

impl VariableBindingForm {
    pub(super) fn from_operator(operator: &str) -> Option<Self> {
        let body = match operator {
            "LET" | "LET*" => BindingBody::Expressions,
            "PROG" | "PROG*" => BindingBody::Tagbody,
            "DO" | "DO*" => BindingBody::Iteration,
            _ => return None,
        };
        Some(Self {
            body,
            sequential: operator.ends_with('*'),
        })
    }

    fn has_steps(&self) -> bool {
        matches!(self.body, BindingBody::Iteration)
    }

    fn has_tagbody(&self) -> bool {
        matches!(self.body, BindingBody::Tagbody | BindingBody::Iteration)
    }
}

struct VariableSpec {
    name: Element,
    initializer: Option<Element>,
    step: Option<Element>,
    has_extra: bool,
}

impl VariableSpec {
    fn read(spec: Element, form: &VariableBindingForm) -> Option<Self> {
        let Some(node) = spec.as_node() else {
            return Some(Self {
                name: spec,
                initializer: None,
                step: None,
                has_extra: false,
            });
        };
        let mut parts = elements(node);
        Some(Self {
            name: parts.next()?,
            initializer: parts.next(),
            step: form.has_steps().then(|| parts.next()).flatten(),
            has_extra: parts.next().is_some(),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalCallableForm {
    Flet,
    Labels,
    Macrolet,
}

impl LocalCallableForm {
    pub(super) fn from_operator(operator: &str) -> Option<Self> {
        match operator {
            "FLET" => Some(Self::Flet),
            "LABELS" => Some(Self::Labels),
            "MACROLET" => Some(Self::Macrolet),
            _ => None,
        }
    }

    fn bind_parameters(
        self,
        resolver: &ScopeAtOffset<'_, '_>,
        params: &Element,
        scope: &mut Vec<ScopeBinding>,
        resolve: bool,
        specials: &[SymbolName],
    ) -> Result<Option<OccurrenceBinding>, ()> {
        match self {
            Self::Flet | Self::Labels => {
                resolver.walk_ordinary_lambda_list(params, scope, resolve, specials)
            }
            Self::Macrolet => {
                resolver.walk_destructuring_lambda_list(params, scope, resolve, specials)
            }
        }
    }
}

struct LocalDefinition {
    node: SyntaxNode,
    name: Element,
    name_range: TextRange,
    params: Element,
    binding: ScopeBinding,
}

impl ScopeAtOffset<'_, '_> {
    pub(super) fn variable_binding_form(
        &self,
        head: &Element,
        mut arguments: FormElements<'_>,
        scope: &mut Vec<ScopeBinding>,
        form: VariableBindingForm,
    ) -> Option<OccurrenceBinding> {
        let bindings = arguments.next()?;
        let binding_specs = if let Some(list) = bindings.as_node().filter(|list| closed_list(list))
        {
            Some(elements(list))
        } else if symbol(&bindings).is_some_and(|spelling| {
            self.workspace.standard_symbol(&spelling, self.package) == Some("NIL")
        }) {
            None
        } else {
            return None;
        };
        let end_test = if form.has_steps() {
            Some(arguments.next()?.into_node().filter(closed_list)?)
        } else {
            None
        };
        let specials = self.special_declarations(arguments.clone(), false);
        if form.has_tagbody() {
            scope.push(self.implicit_nil_block(head));
        }
        let init_len = scope.len();
        let mut active_step = None;
        for spec in binding_specs.into_iter().flatten() {
            if spec.as_node().is_some_and(|node| !closed_list(node)) {
                return None;
            }
            let spec = VariableSpec::read(spec, &form)?;
            let spelling = symbol(&spec.name)?;
            if spec.has_extra {
                return None;
            }
            let range = spec.name.text_range();
            if self.contains(range) {
                return Some(self.variable_target(spelling, range, &specials));
            }
            if let Some(initializer) = spec.initializer
                && self.contains(initializer.text_range())
            {
                // Sequential initializers see earlier bindings. Parallel ones
                // are evaluated in the surrounding environment.
                if !form.sequential {
                    scope.truncate(init_len);
                }
                return self.expression(initializer, scope);
            }
            if active_step.is_none() {
                active_step = spec.step.filter(|step| self.contains(step.text_range()));
            }
            scope.push(self.variable_binding(spelling, range, &specials));
        }
        self.add_free_specials(scope, init_len, &specials);
        // Every binding is available in a step form, the end test and the body.
        if let Some(step) = active_step {
            return self.expression(step, scope);
        }
        if let Some(end_test) = end_test
            && self.contains(end_test.text_range())
        {
            let target = elements(&end_test).at_offset(self.offset).next()?;
            return self.expression(target, scope);
        }
        if form.has_tagbody() {
            return self.tagbody_form(&arguments.collect::<Vec<_>>(), scope, true);
        }
        self.body_expression(arguments.at_offset(self.offset).next()?, scope)
    }

    pub(super) fn collect_variable_binding_form(
        &self,
        head: &Element,
        mut arguments: FormElements<'_>,
        out: &mut Vec<ScopeBinding>,
        form: VariableBindingForm,
    ) {
        if form.has_tagbody() {
            out.push(self.implicit_nil_block(head));
        }
        let Some(bindings) = arguments.next() else {
            return;
        };
        let end_test = form.has_steps().then(|| arguments.next()).flatten();
        let specials = self.special_declarations(arguments.clone(), false);
        let binding_start = out.len();
        if self.collect_variable_bindings(&bindings, out, &form, &specials) {
            return;
        }
        self.add_free_specials(out, binding_start, &specials);
        if let Some(end_test) = end_test
            && self.contains(end_test.text_range())
        {
            if let Some(list) = end_test.as_node() {
                self.descend_contained(elements(list).at_offset(self.offset), out);
            }
            return;
        }
        if form.has_tagbody() {
            self.collect_tagbody_bindings(arguments, out);
        } else {
            self.descend_contained(arguments.at_offset(self.offset), out);
        }
    }

    fn collect_variable_bindings(
        &self,
        bindings: &Element,
        out: &mut Vec<ScopeBinding>,
        form: &VariableBindingForm,
        specials: &[SymbolName],
    ) -> bool {
        let inside_bindings = self.contains(bindings.text_range());
        let Some(list) = bindings.as_node().filter(|list| non_dotted_list(list)) else {
            return inside_bindings;
        };
        let outer_scope_len = out.len();
        let mut active_step = None;
        for spec in elements(list) {
            let range = spec.text_range();
            if inside_bindings && active_step.is_none() && self.offset < range.start() {
                break;
            }
            let Some(spec) = VariableSpec::read(spec, form) else {
                continue;
            };
            if self.contains(range) {
                active_step = spec.step.filter(|step| self.contains(step.text_range()));
                if active_step.is_none() {
                    if !form.sequential {
                        out.truncate(outer_scope_len);
                    }
                    if let Some(init) = spec
                        .initializer
                        .filter(|init| self.contains(init.text_range()))
                    {
                        self.collect_lexical_bindings(init, out);
                    }
                    return true;
                }
            }
            if let Some(spelling) = symbol(&spec.name) {
                out.push(self.variable_binding(spelling, spec.name.text_range(), specials));
            }
        }
        if let Some(step) = active_step {
            self.add_free_specials(out, outer_scope_len, specials);
            self.collect_lexical_bindings(step, out);
        } else if inside_bindings && !form.sequential {
            out.truncate(outer_scope_len);
        }
        inside_bindings
    }

    pub(super) fn local_callable_form(
        &self,
        mut arguments: FormElements<'_>,
        scope: &mut Vec<ScopeBinding>,
        form: LocalCallableForm,
    ) -> Option<OccurrenceBinding> {
        let definitions = arguments.next()?.into_node().filter(closed_list)?;
        let read = |definition| self.read_local_definition(definition, form);
        let definitions_in_scope =
            !self.contains(definitions.text_range()) || form == LocalCallableForm::Labels;
        let local_defs = elements(&definitions)
            .filter_map(|definition| self.possibly_read(definition))
            .map(|(definition, known)| known.then(|| read(definition)).flatten())
            .collect::<Option<Vec<_>>>()?;
        let outer_len = scope.len();
        if definitions_in_scope {
            scope.extend(local_defs.iter().map(|def| def.binding.clone()));
        }
        if !self.contains(definitions.text_range()) {
            let specials = self.special_declarations(arguments.clone(), false);
            let start = scope.len();
            self.add_free_specials(scope, start, &specials);
            let target = arguments.at_offset(self.offset).next()?;
            return self.body_expression(target, scope);
        }

        let body_len = scope.len();
        for def in local_defs {
            let binding = def.binding;
            if self.contains(def.name.text_range()) {
                if self.contains(def.name_range) {
                    return Some(self.local_target(
                        binding.namespace,
                        binding.spelling,
                        def.name_range,
                        binding.params,
                    ));
                }
                scope.truncate(outer_len);
                return self.function_designator(def.name, scope);
            }
            if !self.contains(def.node.text_range()) {
                continue;
            }
            let mut parts = elements(&def.node);
            parts.nth(1);
            let specials = self.special_declarations(parts.clone(), true);
            let resolve = self.contains(def.params.text_range());
            let target = form
                .bind_parameters(self, &def.params, scope, resolve, &specials)
                .ok()?;
            if resolve {
                return target;
            }
            scope.push(self.scope_binding(Namespace::Block, binding.spelling, def.name_range));
            self.add_free_specials(scope, body_len, &specials);
            if let Some(target) = parts.at_offset(self.offset).next()
                && let Some(binding) = self.body_expression(target, scope)
            {
                return Some(binding);
            }
            scope.truncate(body_len);
        }
        None
    }

    fn read_local_definition(
        &self,
        definition: Element,
        form: LocalCallableForm,
    ) -> Option<LocalDefinition> {
        let node = definition.into_node().filter(closed_list)?;
        let mut parts = elements(&node);
        let name = parts.next()?;
        let (namespace, spelling, range) = match form {
            LocalCallableForm::Macrolet => (Namespace::Function, symbol(&name)?, name.text_range()),
            LocalCallableForm::Flet | LocalCallableForm::Labels => {
                self.extract_fn_designator_name(&name)?
            }
        };
        let params = parts.next()?;
        let binding = self.callable_binding(namespace, spelling, range, Some(&params), form);
        Some(LocalDefinition {
            node,
            name,
            name_range: range,
            params,
            binding,
        })
    }

    pub(super) fn collect_local_callable_form(
        &self,
        mut arguments: FormElements<'_>,
        out: &mut Vec<ScopeBinding>,
        form: LocalCallableForm,
    ) {
        if let Some(defs) = arguments.next().and_then(Element::into_node) {
            let inside_definitions = self.contains(defs.text_range());
            let mut selected = None;
            for (def, known) in elements(&defs)
                .filter_map(|definition| self.possibly_read(definition))
                .filter_map(|(definition, known)| definition.into_node().map(|node| (node, known)))
            {
                let mut parts = elements(&def);
                let Some((namespace, spelling, range)) = parts
                    .next()
                    .and_then(|name| self.extract_fn_designator_name(&name))
                else {
                    continue;
                };
                let params = parts.next();
                if !inside_definitions || form == LocalCallableForm::Labels {
                    let mut binding = self.callable_binding(
                        namespace,
                        spelling.clone(),
                        range,
                        params.as_ref(),
                        form,
                    );
                    if !known {
                        binding.def_range = None;
                        binding.params = None;
                        binding.evaluates_arguments = false;
                    }
                    out.push(binding);
                }
                if known
                    && inside_definitions
                    && selected.is_none()
                    && self.contains(def.text_range())
                {
                    selected = Some((def, spelling, range, params));
                }
            }
            if inside_definitions {
                if let Some((def, spelling, range, params)) = selected {
                    let mut parts = elements(&def);
                    parts.nth(1);
                    let specials = self.special_declarations(parts.clone(), true);
                    let start = out.len();
                    let mut inside_params = false;
                    if let Some(params) = params {
                        inside_params = self.contains(params.text_range());
                        if !inside_params {
                            out.push(self.scope_binding(Namespace::Block, spelling, range));
                        }
                        let _ = form.bind_parameters(self, &params, out, false, &specials);
                    }
                    if !inside_params {
                        self.add_free_specials(out, start, &specials);
                    }
                    self.descend_contained(parts.at_offset(self.offset), out);
                }
                return;
            }
        }
        let specials = self.special_declarations(arguments.clone(), false);
        let start = out.len();
        self.add_free_specials(out, start, &specials);
        self.descend_contained(arguments.at_offset(self.offset), out);
    }

    fn callable_binding(
        &self,
        namespace: Namespace,
        spelling: SymbolName,
        range: TextRange,
        params: Option<&Element>,
        form: LocalCallableForm,
    ) -> ScopeBinding {
        ScopeBinding {
            params: params.and_then(super::super::index::lambda_list_signature),
            evaluates_arguments: form != LocalCallableForm::Macrolet,
            ..self.scope_binding(namespace, spelling, range)
        }
    }
}
