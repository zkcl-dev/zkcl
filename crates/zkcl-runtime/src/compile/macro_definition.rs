use std::collections::HashSet;
use std::rc::Rc;

use super::{
    Compiler, Destination, EvalError, Form, FormKind, Ins, Name, Namespace, ScopedBinding, variable,
};
use crate::Runtime;

impl Compiler<'_> {
    pub(super) fn defmacro(
        &mut self,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let [name_form, parameters, body @ ..] = args else {
            return Err(EvalError::compile(
                at,
                "expected a macro name, macro lambda list, and body",
            ));
        };
        let name = name_form
            .name()
            .ok_or_else(|| EvalError::compile(name_form.at, "expected a macro name"))?;

        let (wrapper_params, wrapper_body) = expander_syntax(
            self.runtime,
            &self.context.warnings,
            name_form,
            parameters,
            body,
            false,
            at,
        )?;
        let expander = self.reserve(1).start;
        let child =
            self.lower_function(&wrapper_params, &wrapper_body, None, Some(parameters), at)?;
        self.emit_closure_for_compiled_child(child, Destination::One(expander), at);
        self.document_definition(expander, body)?;
        let symbol = self.symbol(name, name_form.at);
        self.emit(Ins::DefineFunction {
            symbol,
            definition: expander,
            is_macro: true,
            at: name_form.at,
        });
        self.scalar(dest, |out| Ins::Constant(out, symbol));
        Ok(())
    }

    pub(super) fn define_compiler_macro(
        &mut self,
        args: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let [name_form, parameters, body @ ..] = args else {
            return Err(EvalError::compile(
                at,
                "expected a compiler macro name, lambda list, and body",
            ));
        };
        let block_name = if let Some(name) = name_form.name() {
            Form::new(name_form.at, FormKind::Name(name.clone()))
        } else if let Some(accessor) = super::setf_accessor(name_form) {
            Form::new(name_form.at, FormKind::Name(accessor.clone()))
        } else {
            return Err(EvalError::compile(
                name_form.at,
                "expected a compiler macro function name",
            ));
        };
        let (wrapper_params, wrapper_body) = expander_syntax(
            self.runtime,
            &self.context.warnings,
            &block_name,
            parameters,
            body,
            true,
            at,
        )?;
        let mut lambda_forms = Vec::with_capacity(wrapper_body.len() + 2);
        lambda_forms.push(cl_name("LAMBDA", at));
        lambda_forms.push(wrapper_params);
        lambda_forms.extend(wrapper_body);
        let lambda = list(lambda_forms, at);
        let setter_name = call("SETF", vec![cl_name("COMPILER-MACRO-FUNCTION", at)], at);
        let setter_fn = call("FUNCTION", vec![setter_name], at);
        let quoted_name = call("QUOTE", vec![name_form.clone()], name_form.at);
        let install = call("FUNCALL", vec![setter_fn, lambda, quoted_name.clone()], at);
        let form = call("PROGN", vec![install, quoted_name], at);
        self.form(&form, dest)
    }

    pub(super) fn macrolet(
        &mut self,
        args: &[Form],
        dest: Destination,
        compile: fn(&mut Self, &Form, Destination) -> Result<(), EvalError>,
        at: usize,
    ) -> Result<(), EvalError> {
        self.with_local_macros(args, at, |compiler, body| {
            compiler.sequence(body, dest, compile)
        })
    }

    pub(super) fn build_macrolet_scopes(
        &self,
        specs: &Form,
    ) -> Result<Vec<ScopedBinding>, EvalError> {
        let definitions = specs
            .list()
            .ok_or_else(|| EvalError::compile(specs.at, "expected a list of macro definitions"))?;
        let mut names = HashSet::new();
        let mut bindings = Vec::with_capacity(definitions.len());
        for definition in definitions {
            let Some([name_form, parameters, body @ ..]) = definition.list() else {
                return Err(EvalError::compile(
                    definition.at,
                    "expected a macro name, lambda list, and body",
                ));
            };
            let Some(name) = name_form.name() else {
                return Err(EvalError::compile(name_form.at, "expected a macro name"));
            };
            if !names.insert(name.key()) {
                return Err(EvalError::compile(
                    name_form.at,
                    "duplicate local macro name",
                ));
            }
            let (params, wrapper) = expander_syntax(
                self.runtime,
                &self.context.warnings,
                name_form,
                parameters,
                body,
                false,
                definition.at,
            )?;
            // Keep the expansion namespaces, including ordinary bindings that
            // shadow a symbol macro. Runtime registers are outside this evaluation.
            let outer = self
                .scopes()
                .filter(|scope| scope.namespace != Namespace::Block)
                .cloned()
                .map(|mut scope| {
                    scope.binding = None;
                    scope
                })
                .collect();
            let mut initializer = self.nested(outer);
            initializer.context.warnings = Rc::default();
            initializer.begin_top_level_ltv();
            let child = initializer.lower_function(
                &params,
                &wrapper,
                None,
                Some(parameters),
                definition.at,
            )?;
            initializer.emit_closure_for_compiled_child(child, Destination::All, definition.at);
            initializer.finalize_top_level_ltv()?;
            let expander = self
                .run_during_compilation(initializer.finish(), definition.at)
                .map_err(|error| error.during_macro_expansion(definition.at))?;
            bindings.push(ScopedBinding {
                name: name.clone(),
                namespace: Namespace::Macro,
                binding: None,
                expander: Some(Rc::new(self.runtime.root_slot(expander))),
                used: None,
            });
        }
        Ok(bindings)
    }

    pub(super) fn with_local_macros(
        &mut self,
        args: &[Form],
        at: usize,
        process: impl FnOnce(&mut Self, &[Form]) -> Result<(), EvalError>,
    ) -> Result<(), EvalError> {
        let Some((specs, body)) = args.split_first() else {
            return Err(EvalError::compile(
                at,
                "expected local macro definitions and body",
            ));
        };
        let bindings = self.build_macrolet_scopes(specs)?;
        self.with_lexical_scope(|compiler, _| {
            compiler.bindings.extend(bindings);
            compiler.with_local_declarations(body, process)
        })
    }
}

fn expander_syntax(
    runtime: &Runtime,
    warnings: &Rc<std::cell::RefCell<Vec<super::Warning>>>,
    name: &Form,
    parameters: &Form,
    body: &[Form],
    is_compiler_macro: bool,
    at: usize,
) -> Result<(Form, Vec<Form>), EvalError> {
    let (whole, environment, ordinary) = macro_parameters(runtime, parameters)?;
    let (declarations, body) = macro_body(runtime, body);
    let (decls, _) = super::declarations(runtime, warnings, &declarations)?;
    // The wrapper binds only &WHOLE and &ENVIRONMENT. Hoisting any other
    // declaration would change the scope of the destructuring defaults.
    let wrapper_specials: Vec<_> = [&whole, &environment]
        .into_iter()
        .filter(|parameter| {
            parameter
                .name()
                .is_some_and(|name| decls.special.contains(&name))
        })
        .cloned()
        .collect();
    let mut wrapper_body = Vec::new();
    if !wrapper_specials.is_empty() {
        let special = call("SPECIAL", wrapper_specials, at);
        wrapper_body.push(call("DECLARE", vec![special], at));
    }
    let whole_pattern = whole.name().is_none().then(|| whole.clone());
    let whole_parameter = if whole_pattern.is_some() {
        hidden_parameter(runtime, "macro whole form", at)?
    } else {
        whole
    };
    let wrapper_params = list(vec![whole_parameter.clone(), environment], parameters.at);
    let mut block = vec![name.clone()];
    block.extend_from_slice(body);
    let whole_expression = whole_parameter.clone();
    let form_args = if is_compiler_macro {
        call(
            "IF",
            vec![
                call(
                    "EQ",
                    vec![
                        call("CAR", vec![whole_expression.clone()], at),
                        call("QUOTE", vec![cl_name("FUNCALL", at)], at),
                    ],
                    at,
                ),
                call("CDDR", vec![whole_expression.clone()], at),
                call("CDR", vec![whole_expression], at),
            ],
            at,
        )
    } else {
        call("CDR", vec![whole_expression], at)
    };
    let mut destructure = vec![ordinary, form_args];
    destructure.extend(declarations.clone());
    destructure.push(call("BLOCK", block, at));
    let inner = call("DESTRUCTURING-BIND", destructure, at);
    wrapper_body.push(match whole_pattern {
        Some(pattern) => {
            let mut outer = vec![pattern, whole_parameter];
            outer.extend(declarations);
            outer.push(call("BLOCK", vec![name.clone(), inner], at));
            call("DESTRUCTURING-BIND", outer, at)
        }
        None => inner,
    });
    Ok((wrapper_params, wrapper_body))
}

fn macro_parameters(runtime: &Runtime, form: &Form) -> Result<(Form, Form, Form), EvalError> {
    let (parameters, dotted_tail) = match &form.kind {
        FormKind::List(parameters, tail) => (parameters.as_slice(), tail.as_deref()),
        FormKind::Name(name) if name.operator() == "NIL" => (&[][..], None),
        _ => return Err(EvalError::compile(form.at, "expected a macro lambda list")),
    };
    let at = form.at;
    let mut whole = None;
    let mut environment = None;
    let mut ordinary = Vec::with_capacity(parameters.len());
    let mut iter = parameters.iter();
    while let Some(parameter) = iter.next() {
        let marker = parameter.name().map(Name::operator).unwrap_or("");
        match marker {
            "&WHOLE" | "&ENVIRONMENT" => {
                let target = if marker == "&WHOLE" {
                    if !ordinary.is_empty() || environment.is_some() {
                        return Err(EvalError::compile(
                            parameter.at,
                            "&WHOLE must start a macro lambda list",
                        ));
                    }
                    &mut whole
                } else {
                    &mut environment
                };
                if target.is_some() {
                    return Err(EvalError::compile(
                        parameter.at,
                        format!("duplicate {marker}"),
                    ));
                }
                let next = iter.next().ok_or_else(|| {
                    EvalError::compile(parameter.at, format!("expected a variable after {marker}"))
                })?;
                if marker == "&WHOLE" && matches!(next.kind, FormKind::List(_, _)) {
                    *target = Some(next.clone());
                    continue;
                }
                let name = variable(next)?;
                if name.operator().starts_with('&') {
                    return Err(EvalError::compile(
                        next.at,
                        "expected a macro parameter name",
                    ));
                }
                *target = Some(next.clone());
            }
            "&BODY" => ordinary.push(cl_name("&REST", parameter.at)),
            _ => ordinary.push(parameter.clone()),
        }
    }
    if let Some(rest) = dotted_tail {
        variable(rest)?;
        ordinary.push(cl_name("&REST", rest.at));
        ordinary.push(rest.clone());
    }
    Ok((
        whole.map_or_else(|| hidden_parameter(runtime, "macro whole form", at), Ok)?,
        environment.map_or_else(|| hidden_parameter(runtime, "macro environment", at), Ok)?,
        list(ordinary, at),
    ))
}

fn macro_body<'a>(runtime: &Runtime, body: &'a [Form]) -> (Vec<Form>, &'a [Form]) {
    let (declarations, _, body) = super::function_body(runtime, body);
    (declarations.into_iter().cloned().collect(), body)
}

fn hidden_parameter(runtime: &Runtime, label: &str, at: usize) -> Result<Form, EvalError> {
    let (_, word) = runtime.fresh_symbol(label, at)?;
    Ok(Form::new(
        at,
        FormKind::Name(Name::from_word(runtime, word).expect("fresh symbol")),
    ))
}

fn cl_name(name: &str, at: usize) -> Form {
    Form::new(at, FormKind::Name(Name::common_lisp(name)))
}

fn list(forms: Vec<Form>, at: usize) -> Form {
    Form::new(at, FormKind::List(forms, None))
}

fn call(name: &str, arguments: Vec<Form>, at: usize) -> Form {
    let mut forms = Vec::with_capacity(arguments.len() + 1);
    forms.push(cl_name(name, at));
    forms.extend(arguments);
    list(forms, at)
}
