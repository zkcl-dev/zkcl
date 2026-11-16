use std::collections::HashSet;

use crate::compile::generic_required_parameters;
use crate::heap::Word;
use crate::symbol::{Name, Package};
use crate::vm::FunctionName;
use crate::{EvalError, Root, Runtime};

use super::{
    DEFINE_GENERIC, DEFINE_METHOD, Expansion, call, fresh, invalid, list, native_call, proper_list,
    quote, symbol,
};

const GENERIC: &str = "DEFGENERIC";
const METHOD: &str = "DEFMETHOD";

pub(super) fn defgeneric<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let [name, lambda_list, options @ ..] = forms else {
        return Err(invalid(
            GENERIC,
            "expects a function name and generic lambda list",
        ));
    };
    function_name(runtime, name, GENERIC)?;
    let required = generic_required_parameters(runtime, lambda_list, 0)
        .map_err(|error| invalid(GENERIC, error.message()))?;

    let mut argument_precedence_order = runtime.nil();
    let mut method_combination_spec = list(runtime, [&symbol(runtime, "STANDARD")?].into_iter())?;
    let mut documentation = runtime.nil();
    let mut methods = Vec::new();
    let mut mop_options = Vec::new();
    let mut generic_function_class = runtime.nil();
    let mut method_class = runtime.nil();
    let mut declarations = Vec::new();
    let mut seen = HashSet::new();
    for option in options {
        let option = proper_list(runtime, option.clone(), GENERIC)?;
        let Some((head, args)) = option.split_first() else {
            return Err(invalid(GENERIC, "expects a nonempty option list"));
        };
        let tag = Name::from_word(runtime, head.slot.get())
            .ok_or_else(|| invalid(GENERIC, "expects a symbol at the start of each option"))?;
        if tag.operator() == "DECLARE" {
            check_optimize_declarations(runtime, args)?;
            declarations.extend_from_slice(args);
            continue;
        }
        if tag.package() != Some(Package::KEYWORD) {
            return Err(invalid(GENERIC, "expects a keyword option or DECLARE"));
        }
        let key = tag.spelling();
        if key == "METHOD" {
            methods.push(expand_method(runtime, name, args, true)?);
            continue;
        }
        if !seen.insert(key.to_owned()) {
            return Err(invalid(GENERIC, &format!("repeats the :{key} option")));
        }
        match (key, args) {
            ("ARGUMENT-PRECEDENCE-ORDER", [_, ..]) => {
                check_argument_precedence_order(&required, args)?;
                argument_precedence_order = list(runtime, args.iter())?;
            }
            ("METHOD-COMBINATION", [kind, ..])
                if !kind.is_nil() && runtime.heap.is_symbol(kind.slot.get()) =>
            {
                method_combination_spec = list(runtime, args.iter())?;
            }
            ("DOCUMENTATION", [text]) if runtime.heap.is_string(text.slot.get()) => {
                documentation = text.clone();
            }
            ("GENERIC-FUNCTION-CLASS", [class])
                if is_operator(runtime, class, "STANDARD-GENERIC-FUNCTION") => {}
            ("METHOD-CLASS", [class]) if is_operator(runtime, class, "STANDARD-METHOD") => {}
            ("GENERIC-FUNCTION-CLASS" | "METHOD-CLASS", [class])
                if runtime.invocation_protocol.get().is_some()
                    && runtime.heap.is_symbol(class.slot.get()) => {}
            ("GENERIC-FUNCTION-CLASS" | "METHOD-CLASS", [_]) => {
                return Err(invalid(
                    GENERIC,
                    &format!("does not support :{key} with a custom class"),
                ));
            }
            _ => return Err(invalid(GENERIC, &format!("has an invalid :{key} option"))),
        }
        if runtime.invocation_protocol.get().is_some() {
            let val = match key {
                "METHOD-COMBINATION" | "ARGUMENT-PRECEDENCE-ORDER" => list(runtime, args.iter())?,
                _ => args[0].clone(),
            };
            if key == "GENERIC-FUNCTION-CLASS" {
                generic_function_class = val.clone();
            }
            if key == "METHOD-CLASS" {
                method_class = val.clone();
            }
            mop_options.push(head.clone());
            mop_options.push(quote(runtime, &val)?);
        }
    }

    let name = quote(runtime, name)?;
    let definition = if runtime.invocation_protocol.get().is_some() {
        let op = runtime.root(runtime.intern_package(Package::MOP, "%DEFGENERIC", 0)?.0);
        let kw = runtime.root(runtime.intern(&Name::keyword("LAMBDA-LIST"), 0)?);
        let mut args = vec![op, name.clone(), kw, quote(runtime, lambda_list)?];
        if !declarations.is_empty() {
            args.push(runtime.root(runtime.intern(&Name::keyword("DECLARATIONS"), 0)?));
            args.push(quote(runtime, &list(runtime, declarations.iter())?)?);
        }
        args.extend(mop_options);
        let definition = list(runtime, args.iter())?;
        let note = runtime.root(
            runtime
                .intern_package(Package::MOP, "%NOTE-FILE-GENERIC", 0)?
                .0,
        );
        let note = list(
            runtime,
            [
                &note,
                &name,
                &quote(runtime, &generic_function_class)?,
                &quote(runtime, &method_class)?,
            ]
            .into_iter(),
        )?;
        let compile = runtime.root(runtime.intern(&Name::keyword("COMPILE-TOPLEVEL"), 0)?);
        let situations = list(runtime, [&compile].into_iter())?;
        let provisional = call(runtime, "EVAL-WHEN", [&situations, &note].into_iter())?;
        for method in &mut methods {
            let mut fields = proper_list(runtime, method.clone(), GENERIC)?;
            fields.extend([generic_function_class.clone(), method_class.clone()]);
            *method = list(runtime, fields.iter())?;
        }
        call(runtime, "PROGN", [&provisional, &definition].into_iter())?
    } else {
        native_call(
            runtime,
            DEFINE_GENERIC,
            [
                &name,
                &quote(runtime, lambda_list)?,
                &quote(runtime, &argument_precedence_order)?,
                &quote(runtime, &method_combination_spec)?,
                &documentation,
            ],
        )?
    };
    if methods.is_empty() {
        return Ok(definition);
    }
    let result = call(runtime, "FDEFINITION", [&name].into_iter())?;
    call(
        runtime,
        "PROGN",
        [&definition].into_iter().chain(&methods).chain([&result]),
    )
}

fn check_argument_precedence_order(required: &[Name], order: &[Root<'_>]) -> Result<(), EvalError> {
    if order.len() != required.len()
        || required.iter().any(|parameter| {
            !order
                .iter()
                .any(|name| Some(name.slot.get()) == parameter.rooted_word())
        })
    {
        return Err(invalid(
            GENERIC,
            "argument precedence order must list every required parameter once",
        ));
    }
    Ok(())
}

fn check_optimize_declarations(
    runtime: &Runtime,
    declarations: &[Root<'_>],
) -> Result<(), EvalError> {
    if declarations.is_empty() {
        return Err(invalid(
            GENERIC,
            "requires an OPTIMIZE specifier in DECLARE",
        ));
    }
    for declaration in declarations {
        let specifier = proper_list(runtime, declaration.clone(), GENERIC)?;
        if specifier
            .first()
            .is_none_or(|head| !is_operator(runtime, head, "OPTIMIZE"))
        {
            return Err(invalid(GENERIC, "accepts OPTIMIZE declaration specifiers"));
        }
    }
    Ok(())
}

pub(super) fn defmethod<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let Some((name, definition)) = forms.split_first() else {
        return Err(invalid(
            METHOD,
            "expects a function name and a specialized lambda list",
        ));
    };
    expand_method(runtime, name, definition, false)
}

fn expand_method<'rt>(
    runtime: &'rt Runtime,
    name: &Root<'rt>,
    definition: &[Root<'rt>],
    inline: bool,
) -> Expansion<'rt> {
    let block = function_name(runtime, name, METHOD)?;
    let qualifier_count = definition
        .iter()
        .take_while(|value| !value.is_nil() && runtime.cons_fields(value).is_none())
        .count();
    let (qualifiers, definition) = definition.split_at(qualifier_count);
    let Some((specialized, body)) = definition.split_first() else {
        return Err(invalid(METHOD, "expects a specialized lambda list"));
    };
    let (ordinary, names) = specialized_parameters(runtime, specialized, METHOD)?;
    let specializers = names
        .iter()
        .map(|name| specializer_form(runtime, name))
        .collect::<Result<Vec<_>, _>>()?;
    let original_parameters = list(runtime, ordinary.iter())?;
    let body = method_body(runtime, body);
    if runtime.invocation_protocol.get().is_some() {
        let fast_entry = method_function(
            runtime,
            &block,
            ordinary.clone(),
            body.declarations.clone(),
            body.forms,
        )?;
        let mut parameters = ordinary;
        add_allow_other_keys(runtime, &mut parameters)?;
        let parameters = list(runtime, parameters.iter())?;
        let body_form = call(runtime, "BLOCK", [&block].into_iter().chain(body.forms))?;
        let lambda = call(
            runtime,
            "LAMBDA",
            [&parameters]
                .into_iter()
                .chain(body.declarations)
                .chain([&body_form]),
        )?;
        let op = runtime.root(runtime.intern_package(Package::MOP, "%DEFINE-METHOD", 0)?.0);
        return list(
            runtime,
            [
                &op,
                name,
                &list(runtime, qualifiers.iter())?,
                &original_parameters,
                &call(runtime, "LIST", specializers.iter())?,
                &lambda,
                &fast_entry,
                &body.documentation,
                &runtime.root(if inline { Word::T } else { Word::NIL }),
            ]
            .into_iter(),
        );
    }
    let function = method_function(runtime, &block, ordinary, body.declarations, body.forms)?;
    let inline = runtime.root(if inline { Word::T } else { Word::NIL });
    native_call(
        runtime,
        DEFINE_METHOD,
        [
            &quote(runtime, name)?,
            &quote(runtime, &list(runtime, qualifiers.iter())?)?,
            &call(runtime, "LIST", specializers.iter())?,
            &quote(runtime, &original_parameters)?,
            &function,
            &body.documentation,
            &inline,
        ],
    )
}

fn specialized_parameters<'rt>(
    runtime: &'rt Runtime,
    lambda_list: &Root<'rt>,
    operator: &str,
) -> Result<(Vec<Root<'rt>>, Vec<Root<'rt>>), EvalError> {
    let mut ordinary = proper_list(runtime, lambda_list.clone(), operator)?;
    let mut names = Vec::new();
    for parameter in &mut ordinary {
        if is_lambda_marker(runtime, parameter) {
            break;
        }
        if runtime.heap.is_variable(parameter.slot.get()) {
            names.push(runtime.root(Word::T));
        } else {
            let pair = proper_list(runtime, parameter.clone(), operator)?;
            let [variable, name] = pair.as_slice() else {
                return Err(invalid(
                    operator,
                    "expects a variable or a (variable specializer) parameter",
                ));
            };
            if !runtime.heap.is_variable(variable.slot.get()) || is_lambda_marker(runtime, variable)
            {
                return Err(invalid(
                    operator,
                    "expects a variable in a specialized parameter",
                ));
            }
            *parameter = variable.clone();
            names.push(name.clone());
        }
    }
    Ok((ordinary, names))
}

pub(crate) fn extract_method_parameters<'rt>(
    runtime: &'rt Runtime,
    lambda_list: &Root<'rt>,
    names_only: bool,
    at: usize,
) -> Expansion<'rt> {
    let operator = if names_only {
        "EXTRACT-SPECIALIZER-NAMES"
    } else {
        "EXTRACT-LAMBDA-LIST"
    };
    let (ordinary, names) = specialized_parameters(runtime, lambda_list, operator)?;
    let result = list(runtime, ordinary.iter())?;
    // Validate the ordinary lambda list after removing required specializers.
    // A specializer name is opaque to these readers, including the form in EQL.
    crate::compile::call_signature(runtime, &result, at)
        .map_err(|err| invalid(operator, err.message()))?;
    if names_only {
        list(runtime, names.iter())
    } else {
        Ok(result)
    }
}

fn specializer_form<'rt>(runtime: &'rt Runtime, name: &Root<'rt>) -> Expansion<'rt> {
    let word = name.slot.get();
    if runtime.heap.is_symbol(word) || runtime.heap.class_info(word).is_some() {
        // Preserve a class object chosen during macroexpansion. COMPILE-FILE's
        // MAKE-LOAD-FORM method for CLASS resolves it in the loading image.
        return quote(runtime, name);
    }
    let spec = proper_list(runtime, name.clone(), METHOD)?;
    let [head, value] = spec.as_slice() else {
        return Err(invalid(
            METHOD,
            "expects a class name or an (EQL form) specializer",
        ));
    };
    if !is_operator(runtime, head, "EQL") {
        return Err(invalid(
            METHOD,
            "expects a class name or an (EQL form) specializer",
        ));
    }
    call(runtime, "CONS", [&quote(runtime, head)?, value].into_iter())
}

struct MethodBody<'forms, 'rt> {
    declarations: Vec<&'forms Root<'rt>>,
    documentation: Root<'rt>,
    forms: &'forms [Root<'rt>],
}

fn method_body<'forms, 'rt>(
    runtime: &'rt Runtime,
    body: &'forms [Root<'rt>],
) -> MethodBody<'forms, 'rt> {
    let mut body = MethodBody {
        declarations: Vec::new(),
        documentation: runtime.nil(),
        forms: body,
    };
    while let Some((first, rest)) = body.forms.split_first() {
        if runtime
            .cons_fields(first)
            .is_some_and(|(head, _)| is_operator(runtime, &head, "DECLARE"))
        {
            body.declarations.push(first);
        } else if body.documentation.is_nil()
            && !rest.is_empty()
            && runtime.heap.is_string(first.slot.get())
        {
            body.documentation = first.clone();
        } else {
            break;
        }
        body.forms = rest;
    }
    body
}

fn add_allow_other_keys<'rt>(
    runtime: &'rt Runtime,
    parameters: &mut Vec<Root<'rt>>,
) -> Result<(), EvalError> {
    // The generic function validates the keywords accepted by applicable methods.
    let has = |keyword| {
        parameters
            .iter()
            .any(|value| is_operator(runtime, value, keyword))
    };
    if has("&KEY") && !has("&ALLOW-OTHER-KEYS") {
        let end = parameters
            .iter()
            .position(|value| is_operator(runtime, value, "&AUX"))
            .unwrap_or(parameters.len());
        parameters.insert(end, symbol(runtime, "&ALLOW-OTHER-KEYS")?);
    }
    Ok(())
}

fn method_function<'rt>(
    runtime: &'rt Runtime,
    block: &Root<'rt>,
    mut parameters: Vec<Root<'rt>>,
    declarations: Vec<&Root<'rt>>,
    body: &[Root<'rt>],
) -> Expansion<'rt> {
    add_allow_other_keys(runtime, &mut parameters)?;
    let next = fresh(runtime, "NEXT-METHOD")?;
    let next_p = fresh(runtime, "NEXT-METHOD-AVAILABLE")?;
    let forwarded = fresh(runtime, "NEXT-METHOD-ARGUMENTS")?;
    let rest = symbol(runtime, "&REST")?;
    let forward_parameters = list(runtime, [&rest, &forwarded].into_iter())?;
    let apply = call(runtime, "APPLY", [&next, &forwarded].into_iter())?;
    let call_next = call(
        runtime,
        "CALL-NEXT-METHOD",
        [&forward_parameters, &apply].into_iter(),
    )?;
    let no_next = call(runtime, "NULL", [&next_p].into_iter())?;
    let has_next = call(runtime, "NOT", [&no_next].into_iter())?;
    let test_next = call(
        runtime,
        "NEXT-METHOD-P",
        [&runtime.nil(), &has_next].into_iter(),
    )?;
    let local_functions = list(runtime, [&call_next, &test_next].into_iter())?;
    let body = call(runtime, "FLET", [&local_functions].into_iter().chain(body))?;
    let block = call(runtime, "BLOCK", [block, &body].into_iter())?;
    let parameters = list(runtime, [&next, &next_p].into_iter().chain(&parameters))?;
    let lambda = call(
        runtime,
        "LAMBDA",
        [&parameters]
            .into_iter()
            .chain(declarations)
            .chain([&block]),
    )?;
    call(runtime, "FUNCTION", [&lambda].into_iter())
}

fn function_name<'rt>(runtime: &'rt Runtime, name: &Root<'rt>, context: &str) -> Expansion<'rt> {
    let name = FunctionName::parse(runtime, name.slot.get(), 0)
        .map_err(|_| invalid(context, "expects a symbol or (SETF symbol) function name"))?;
    let block = match name {
        FunctionName::Symbol(symbol) | FunctionName::Setf(symbol) => symbol,
    };
    Ok(runtime.root(block))
}

fn is_operator(runtime: &Runtime, value: &Root<'_>, operator: &str) -> bool {
    runtime.find_named_symbol(&Name::common_lisp(operator)) == Some(value.slot.get())
}

fn is_lambda_marker(runtime: &Runtime, value: &Root<'_>) -> bool {
    Name::from_word(runtime, value.slot.get()).is_some_and(|name| {
        matches!(
            name.operator(),
            "&OPTIONAL"
                | "&REST"
                | "&KEY"
                | "&ALLOW-OTHER-KEYS"
                | "&AUX"
                | "&BODY"
                | "&WHOLE"
                | "&ENVIRONMENT"
        )
    })
}
