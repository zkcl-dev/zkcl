use std::collections::HashSet;

use crate::condition::{Class, Fault};
use crate::form_value::name_of_word;
use crate::heap::{ListSpine, Word};
use crate::package::Package;
use crate::symbol::Name;
use crate::vm::{Primitive, plists};
use crate::{EvalError, Root, Runtime, SetfExpander, SetfExpanderRef, macros};

use super::{
    DEFINE_LONG_DEFSETF, DEFINE_SETF_EXPANDER, DEFINE_SHORT_DEFSETF, Expansion, binding, call,
    call_with_operator, compiled_native, define_at_toplevel, expansion_fault, fresh, invalid,
    let_star, list, native_call, progn, proper_list, quote,
};

pub(super) struct Place<'rt> {
    // In evaluation order, each temporary names exactly one value form.
    arguments: Vec<(Root<'rt>, Root<'rt>)>,
    pub(super) stores: Vec<Root<'rt>>,
    pub(super) writer: Root<'rt>,
    pub(super) reader: Root<'rt>,
}

impl<'rt> Place<'rt> {
    pub(super) fn bindings(&self, runtime: &'rt Runtime) -> Result<Vec<Root<'rt>>, EvalError> {
        self.arguments
            .iter()
            .map(|(temporary, form)| binding(runtime, temporary, form))
            .collect()
    }

    fn output(self, runtime: &'rt Runtime) -> Result<[Root<'rt>; 5], EvalError> {
        Ok([
            list(runtime, self.arguments.iter().map(|(temp, _)| temp))?,
            list(runtime, self.arguments.iter().map(|(_, form)| form))?,
            list(runtime, self.stores.iter())?,
            self.writer,
            self.reader,
        ])
    }
}

pub(crate) fn get_setf_expansion(
    runtime: &Runtime,
    place: Word,
    environment: Word,
    at: usize,
) -> Result<[Root<'_>; 5], Fault> {
    let place = runtime.root(place);
    let environment = runtime.root(environment);
    expand_place(runtime, place, &environment, 0)
        .and_then(|place| place.output(runtime))
        .map_err(|error| expansion_fault(error, at))
}

pub(super) fn expand_setf<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let mut writes = Vec::new();
    for [place, value] in pairs(forms, "SETF")? {
        let place = expand_place(runtime, place.clone(), environment, 0)?;
        let bindings = place.bindings(runtime)?;
        let store = bind_values(runtime, &place.stores, value, &place.writer)?;
        writes.push(let_star(runtime, &bindings, store)?);
    }
    progn(runtime, writes)
}

pub(super) fn expand_psetf<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    expand_parallel_assignments(runtime, pairs(forms, "PSETF")?, environment)
}

fn expand_parallel_assignments<'rt>(
    runtime: &'rt Runtime,
    assignments: &[[Root<'rt>; 2]],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let mut bindings = Vec::new();
    let mut writes = Vec::new();
    for [place, value] in assignments {
        let place = expand_place(runtime, place.clone(), environment, 0)?;
        bindings.extend(place.bindings(runtime)?);
        match place.stores.as_slice() {
            [] => bindings.push(binding(runtime, &fresh(runtime, "IGNORED VALUE")?, value)?),
            [store] => bindings.push(binding(runtime, store, value)?),
            stores => {
                // LET* keeps the subforms and the captured values of every place
                // in source order without nesting one binder per assignment.
                let mut rest = fresh(runtime, "STORE VALUES")?;
                let values = call(runtime, "MULTIPLE-VALUE-LIST", std::iter::once(value))?;
                bindings.push(binding(runtime, &rest, &values)?);
                for (index, store) in stores.iter().enumerate() {
                    let head = call(runtime, "CAR", std::iter::once(&rest))?;
                    bindings.push(binding(runtime, store, &head)?);
                    if index + 1 < stores.len() {
                        let next = fresh(runtime, "REMAINING STORE VALUES")?;
                        let tail = call(runtime, "CDR", std::iter::once(&rest))?;
                        bindings.push(binding(runtime, &next, &tail)?);
                        rest = next;
                    }
                }
            }
        }
        writes.push(place.writer);
    }
    writes.push(runtime.nil());
    let_star(runtime, &bindings, progn(runtime, writes)?)
}

pub(super) fn expand_psetq<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let assignments = pairs(forms, "PSETQ")?;
    for [variable, _] in assignments {
        if !runtime.heap.is_symbol(variable.slot.get()) {
            return Err(invalid("PSETQ", "expects a variable name"));
        }
    }
    expand_parallel_assignments(runtime, assignments, environment)
}

pub(super) fn expand_multiple_value_setq<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let [variables, value] = forms else {
        return Err(invalid(
            "MULTIPLE-VALUE-SETQ",
            "expects variables and a value form",
        ));
    };
    let variables = proper_list(runtime, variables.clone(), "MULTIPLE-VALUE-SETQ variables")?;
    if variables.is_empty() {
        return call(runtime, "VALUES", std::iter::once(value));
    }
    if !variables
        .iter()
        .all(|variable| runtime.heap.is_symbol(variable.slot.get()))
    {
        return Err(invalid("MULTIPLE-VALUE-SETQ", "expects variable names"));
    }
    let places = call(runtime, "VALUES", variables.iter())?;
    let writer = call(runtime, "SETF", [&places, value].into_iter())?;
    call(runtime, "VALUES", std::iter::once(&writer))
}

pub(super) fn expand_incf<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    change_number(runtime, "INCF", "+", forms, environment)
}

pub(super) fn expand_decf<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    change_number(runtime, "DECF", "-", forms, environment)
}

fn change_number<'rt>(
    runtime: &'rt Runtime,
    operator: &str,
    arithmetic: &str,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let (target, delta) = match forms {
        [target] => (target, None),
        [target, delta] => (target, Some(delta)),
        _ => return Err(invalid(operator, "expects a place and an optional delta")),
    };
    let place = expand_place(runtime, target.clone(), environment, 0)?;
    let mut bindings = place.bindings(runtime)?;
    let delta = if let Some(form) = delta {
        let delta = fresh(runtime, "DELTA")?;
        bindings.push(binding(runtime, &delta, form)?);
        delta
    } else {
        runtime.fixnum(1).expect("one is a fixnum")
    };
    let value = call(runtime, arithmetic, [&place.reader, &delta].into_iter())?;
    write_update(runtime, place, bindings, value, None)
}

pub(super) fn expand_push<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let [item, target] = forms else {
        return Err(invalid("PUSH", "expects one item and one place"));
    };
    let place = expand_place(runtime, target.clone(), environment, 0)?;
    let item_temp = fresh(runtime, "ITEM")?;
    let mut bindings = vec![binding(runtime, &item_temp, item)?];
    bindings.extend(place.bindings(runtime)?);
    let value = call(runtime, "CONS", [&item_temp, &place.reader].into_iter())?;
    write_update(runtime, place, bindings, value, None)
}

pub(super) fn expand_pushnew<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let [item, target, key_args @ ..] = forms else {
        return Err(invalid(
            "PUSHNEW",
            "expects an item, a place, and keyword arguments",
        ));
    };
    let (pairs, remainder) = key_args.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(invalid("PUSHNEW", "expects keyword/value pairs"));
    }
    let place = expand_place(runtime, target.clone(), environment, 0)?;
    let item_temp = fresh(runtime, "ITEM")?;
    let mut bindings = vec![binding(runtime, &item_temp, item)?];
    bindings.extend(place.bindings(runtime)?);
    let mut adjoin_args = vec![item_temp, place.reader.clone()];
    for [key, value] in pairs {
        let key_arg = if runtime.heap.symbol_home(key.slot.get()) == Some(Package::KEYWORD) {
            key.clone()
        } else {
            let name_temp = fresh(runtime, "KEY NAME")?;
            bindings.push(binding(runtime, &name_temp, key)?);
            name_temp
        };
        let key_temp = fresh(runtime, "KEY ARG")?;
        bindings.push(binding(runtime, &key_temp, value)?);
        adjoin_args.push(key_arg);
        adjoin_args.push(key_temp);
    }
    let value = call(runtime, "ADJOIN", adjoin_args.iter())?;
    write_update(runtime, place, bindings, value, None)
}

pub(super) fn expand_pop<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let [target] = forms else {
        return Err(invalid("POP", "expects one place"));
    };
    let place = expand_place(runtime, target.clone(), environment, 0)?;
    let mut bindings = place.bindings(runtime)?;
    let previous = fresh(runtime, "ORIGINAL LIST")?;
    let head = fresh(runtime, "POPPED VALUE")?;
    bindings.push(binding(runtime, &previous, &place.reader)?);
    let car = call(runtime, "CAR", std::iter::once(&previous))?;
    bindings.push(binding(runtime, &head, &car)?);
    let rest = call(runtime, "CDR", std::iter::once(&previous))?;
    write_update(runtime, place, bindings, rest, Some(head))
}

pub(super) fn expand_shiftf<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let Some((new_value, targets)) = forms
        .split_last()
        .filter(|(_, targets)| !targets.is_empty())
    else {
        return Err(invalid("SHIFTF", "expects at least one place and a value"));
    };
    let (places, bindings) = expand_place_sequence(runtime, targets, environment)?;
    let last = places.last().expect("SHIFTF requires a place");
    let assignments = shifted_values(&places).chain([(last, new_value)]);
    let result = fresh(runtime, "SHIFTF RESULT")?;
    let writes = write_places(runtime, &places, assignments, &result)?;
    let result_bind = binding(runtime, &result, &places[0].reader)?;
    let outer = let_star(runtime, &[result_bind], writes)?;
    let_star(runtime, &bindings, outer)
}

pub(super) fn expand_rotatef<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let (places, bindings) = expand_place_sequence(runtime, forms, environment)?;
    let nil = runtime.nil();
    let body = match places.as_slice() {
        [] => return Ok(nil),
        [place] => call(runtime, "PROGN", [&place.reader, &nil].into_iter())?,
        [first, .., last] => {
            let assignments = std::iter::once((last, &first.reader)).chain(shifted_values(&places));
            write_places(runtime, &places, assignments, &nil)?
        }
    };
    let_star(runtime, &bindings, body)
}

fn expand_place_sequence<'rt>(
    runtime: &'rt Runtime,
    targets: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Result<(Vec<Place<'rt>>, Vec<Root<'rt>>), EvalError> {
    let mut places = Vec::with_capacity(targets.len());
    let mut bindings = Vec::new();
    for target in targets {
        let place = expand_place(runtime, target.clone(), environment, 0)?;
        bindings.extend(place.bindings(runtime)?);
        places.push(place);
    }
    Ok((places, bindings))
}

fn shifted_values<'place, 'rt>(
    places: &'place [Place<'rt>],
) -> impl DoubleEndedIterator<Item = (&'place Place<'rt>, &'place Root<'rt>)> {
    places
        .windows(2)
        .map(|adjacent| (&adjacent[0], &adjacent[1].reader))
}

fn write_places<'place, 'rt: 'place>(
    runtime: &'rt Runtime,
    places: &[Place<'rt>],
    assignments: impl DoubleEndedIterator<Item = (&'place Place<'rt>, &'place Root<'rt>)>,
    result: &Root<'rt>,
) -> Expansion<'rt> {
    let writes = places.iter().map(|place| &place.writer).chain([result]);
    let mut body = call(runtime, "PROGN", writes)?;
    // Nest the value binders in reader order around the source-ordered writers.
    for (target, value) in assignments.rev() {
        body = bind_values(runtime, &target.stores, value, &body)?;
    }
    Ok(body)
}

fn bind_values<'rt>(
    runtime: &'rt Runtime,
    variables: &[Root<'rt>],
    value: &Root<'rt>,
    body: &Root<'rt>,
) -> Expansion<'rt> {
    if let [variable] = variables {
        // A single store receives the primary value, including NIL for zero
        // values. Keep the value and writer visible to lexical optimization.
        return let_star(runtime, &[binding(runtime, variable, value)?], body.clone());
    }
    let variables = list(runtime, variables.iter())?;
    call(
        runtime,
        "MULTIPLE-VALUE-BIND",
        [&variables, value, body].into_iter(),
    )
}

fn write_update<'rt>(
    runtime: &'rt Runtime,
    place: Place<'rt>,
    mut bindings: Vec<Root<'rt>>,
    value: Root<'rt>,
    output: Option<Root<'rt>>,
) -> Expansion<'rt> {
    let new_value = if let Some((first, extra)) = place.stores.split_first() {
        bindings.push(binding(runtime, first, &value)?);
        let nil = runtime.nil();
        for variable in extra {
            bindings.push(binding(runtime, variable, &nil)?);
        }
        first.clone()
    } else {
        let temp = fresh(runtime, "NEW VALUE")?;
        bindings.push(binding(runtime, &temp, &value)?);
        temp
    };
    let result = output.unwrap_or(new_value);
    let body = call(runtime, "PROGN", [&place.writer, &result].into_iter())?;
    let_star(runtime, &bindings, body)
}

fn pairs<'a, 'rt>(
    forms: &'a [Root<'rt>],
    operator: &str,
) -> Result<&'a [[Root<'rt>; 2]], EvalError> {
    let (pairs, remainder) = forms.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(invalid(operator, "expects place and value pairs"));
    }
    Ok(pairs)
}

pub(super) fn expand_define_setf_expander<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let [name, lambda_list, body @ ..] = forms else {
        return Err(invalid(
            "DEFINE-SETF-EXPANDER",
            "expects an access function, lambda list, and body",
        ));
    };
    if !is_definable_symbol(runtime, name.slot.get()) {
        return Err(invalid(
            "DEFINE-SETF-EXPANDER",
            "expects a definable symbol as the access function",
        ));
    }
    let (whole, env, ordinary) = parse_macro_lambda_list(runtime, lambda_list)?;
    let (documentation, declarations, body_forms) = split_doc_and_body(runtime, body);
    let block_form = call(
        runtime,
        "BLOCK",
        std::iter::once(name).chain(body_forms.iter()),
    )?;
    let cdr_whole = call(runtime, "CDR", std::iter::once(&whole))?;
    let destructured = call(
        runtime,
        "DESTRUCTURING-BIND",
        [&ordinary, &cdr_whole]
            .into_iter()
            .chain(&declarations)
            .chain([&block_form]),
    )?;
    let wrapper_params = list(runtime, [&whole, &env].into_iter())?;
    let outer_specials = hoist_special_declarations(runtime, &declarations, &[&whole, &env])?;
    let lambda = call(
        runtime,
        "LAMBDA",
        [&wrapper_params]
            .into_iter()
            .chain(outer_specials.iter())
            .chain([&destructured]),
    )?;
    let quoted_name = quote(runtime, name)?;
    let quoted_doc = quote(runtime, &documentation)?;
    let definition = native_call(
        runtime,
        DEFINE_SETF_EXPANDER,
        [&quoted_name, &lambda, &quoted_doc],
    )?;
    define_at_toplevel(runtime, definition)
}

fn hoist_special_declarations<'rt>(
    runtime: &'rt Runtime,
    declarations: &[Root<'rt>],
    targets: &[&Root<'rt>],
) -> Result<Option<Root<'rt>>, EvalError> {
    let mut matched = Vec::new();
    for declare in declarations {
        let Ok(specs) = proper_list(runtime, declare.clone(), "DECLARE") else {
            continue;
        };
        for spec in specs.into_iter().skip(1) {
            let Ok(items) = proper_list(runtime, spec, "declaration specifier") else {
                continue;
            };
            let Some((head, vars)) = items.split_first() else {
                continue;
            };
            if name_of_word(runtime, head.slot.get()).is_some_and(|n| n.operator() == "SPECIAL") {
                for var in vars {
                    if targets.iter().any(|target| target.same_identity(var)) {
                        matched.push(var.clone());
                    }
                }
            }
        }
    }
    if matched.is_empty() {
        return Ok(None);
    }
    let special = call(runtime, "SPECIAL", matched.iter())?;
    call(runtime, "DECLARE", std::iter::once(&special)).map(Some)
}

fn is_bindable_lambda_var(runtime: &Runtime, word: Word) -> bool {
    runtime.heap.symbol_attributes(word).is_some_and(|data| {
        word != Word::NIL
            && !data.constant
            && runtime.heap.symbol_home(word) != Some(Package::KEYWORD)
            && !name_of_word(runtime, word).is_some_and(|n| n.operator().starts_with('&'))
    })
}

fn parse_macro_lambda_list<'rt>(
    runtime: &'rt Runtime,
    form: &Root<'rt>,
) -> Result<(Root<'rt>, Root<'rt>, Root<'rt>), EvalError> {
    if !form.is_nil() && runtime.heap.cons_fields(form.slot.get()).is_none() {
        return Err(invalid(
            "DEFINE-SETF-EXPANDER",
            "expected a macro lambda list",
        ));
    }
    let mut items = Vec::new();
    let mut spine = ListSpine::new(form.slot.get());
    while let Some((head, _)) = spine
        .next(&runtime.heap)
        .map_err(|_| invalid("DEFINE-SETF-EXPANDER", "expected a macro lambda list"))?
    {
        items.push(runtime.root(head));
    }
    let dotted_tail = (spine.tail() != Word::NIL).then(|| runtime.root(spine.tail()));
    if let Some(ref tail) = dotted_tail
        && !is_bindable_lambda_var(runtime, tail.slot.get())
    {
        return Err(invalid("DEFINE-SETF-EXPANDER", "expected a symbol after ."));
    }

    let mut whole = None;
    let mut environment = None;
    let mut ordinary = Vec::with_capacity(items.len());
    let mut iter = items.into_iter();
    while let Some(item) = iter.next() {
        let marker = name_of_word(runtime, item.slot.get());
        let (slot, marker) = match marker.as_ref().map(|n| n.operator()) {
            Some(name @ "&WHOLE") => {
                if !ordinary.is_empty() || environment.is_some() || whole.is_some() {
                    return Err(invalid(
                        "DEFINE-SETF-EXPANDER",
                        "&WHOLE must start the lambda list",
                    ));
                }
                (&mut whole, name)
            }
            Some(name @ "&ENVIRONMENT") => {
                if environment.is_some() {
                    return Err(invalid(
                        "DEFINE-SETF-EXPANDER",
                        "duplicate &ENVIRONMENT parameter",
                    ));
                }
                (&mut environment, name)
            }
            _ => {
                ordinary.push(item);
                continue;
            }
        };
        let variable = iter.next().ok_or_else(|| {
            invalid(
                "DEFINE-SETF-EXPANDER",
                &format!("expected a variable after {marker}"),
            )
        })?;
        if !is_bindable_lambda_var(runtime, variable.slot.get()) {
            return Err(invalid(
                "DEFINE-SETF-EXPANDER",
                &format!("expected a symbol after {marker}"),
            ));
        }
        *slot = Some(variable);
    }
    if let Some(tail) = dotted_tail {
        ordinary.extend([super::symbol(runtime, "&REST")?, tail]);
    }
    let ordinary_list = list(runtime, ordinary.iter())?;
    let whole = match whole {
        Some(w) => w,
        None => fresh(runtime, "SETF PLACE")?,
    };
    let env = match environment {
        Some(e) => e,
        None => fresh(runtime, "SETF ENV")?,
    };
    Ok((whole, env, ordinary_list))
}

pub(super) fn expand_defsetf<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let Some((access_fn, rest)) = forms.split_first() else {
        return Err(invalid("DEFSETF", "expects an access function"));
    };
    if !is_definable_symbol(runtime, access_fn.slot.get()) {
        return Err(invalid(
            "DEFSETF",
            "expects a definable symbol as the access function",
        ));
    }
    let is_list =
        |form: &Root<'rt>| form.is_nil() || runtime.heap.cons_fields(form.slot.get()).is_some();
    if let [lambda_list, store_vars, body @ ..] = rest
        && is_list(lambda_list)
        && is_list(store_vars)
    {
        return expand_long_defsetf(runtime, access_fn, lambda_list, store_vars, body);
    }
    let (update_fn, documentation) = match rest {
        [update_fn] => (update_fn, runtime.nil()),
        [update_fn, doc] if runtime.heap.is_string(doc.slot.get()) => (update_fn, doc.clone()),
        _ => return Err(invalid("DEFSETF", "invalid short or long DEFSETF syntax")),
    };
    if !runtime.heap.is_symbol(update_fn.slot.get()) || update_fn.is_nil() {
        return Err(invalid(
            "DEFSETF",
            "short form expects a non-NIL symbol as the update function",
        ));
    }
    let quoted_access = quote(runtime, access_fn)?;
    let quoted_update = quote(runtime, update_fn)?;
    let quoted_doc = quote(runtime, &documentation)?;
    let definition = native_call(
        runtime,
        DEFINE_SHORT_DEFSETF,
        [&quoted_access, &quoted_update, &quoted_doc],
    )?;
    define_at_toplevel(runtime, definition)
}

fn expand_long_defsetf<'rt>(
    runtime: &'rt Runtime,
    access_fn: &Root<'rt>,
    lambda_list: &Root<'rt>,
    store_vars: &Root<'rt>,
    body: &[Root<'rt>],
) -> Expansion<'rt> {
    let store_vars = proper_list(runtime, store_vars.clone(), "DEFSETF store variables")?;
    let mut seen_stores = HashSet::new();
    for var in &store_vars {
        let Some(data) = runtime.heap.symbol_attributes(var.slot.get()) else {
            return Err(invalid("DEFSETF", "store variables must be symbols"));
        };
        if var.is_nil()
            || data.constant
            || runtime.heap.symbol_home(var.slot.get()) == Some(Package::KEYWORD)
        {
            return Err(invalid(
                "DEFSETF",
                "store variables must be bindable non-constant symbols",
            ));
        }
        if !seen_stores.insert(data.id) {
            return Err(invalid("DEFSETF", "duplicate store variable"));
        }
    }

    let parsed = parse_defsetf_lambda_list(runtime, lambda_list)?;
    let (documentation, declarations, forms) = split_doc_and_body(runtime, body);

    let args_param = fresh(runtime, "DEFSETF ARGS")?;
    let env_param = fresh(runtime, "DEFSETF ENV")?;
    let stores_param = fresh(runtime, "DEFSETF STORES")?;

    let block_form = call(
        runtime,
        "BLOCK",
        std::iter::once(access_fn).chain(forms.iter()),
    )?;
    let inner_lambda = call(
        runtime,
        "LAMBDA",
        [&parsed.clean_lambda_list]
            .into_iter()
            .chain(&declarations)
            .chain([&block_form]),
    )?;
    let apply = call(runtime, "APPLY", [&inner_lambda, &args_param].into_iter())?;

    let store_list = list(runtime, store_vars.iter())?;
    let destructured = call(
        runtime,
        "DESTRUCTURING-BIND",
        [&store_list, &stores_param, &apply].into_iter(),
    )?;
    let mut wrapper_body = Vec::new();
    let wrapped = if let Some(env_var) = parsed.env_var {
        let env_bind = binding(runtime, &env_var, &env_param)?;
        let env_binds = list(runtime, std::iter::once(&env_bind))?;
        let mut let_args = vec![env_binds];
        if let Some(special_decl) = hoist_special_declarations(runtime, &declarations, &[&env_var])?
        {
            let_args.push(special_decl);
        }
        let_args.push(destructured);
        call(runtime, "LET", let_args.iter())?
    } else {
        let ignore = call(runtime, "IGNORE", std::iter::once(&env_param))?;
        wrapper_body.push(call(runtime, "DECLARE", std::iter::once(&ignore))?);
        destructured
    };
    wrapper_body.push(wrapped);
    let wrapper_params = list(
        runtime,
        [&args_param, &env_param, &stores_param].into_iter(),
    )?;
    let wrapper_lambda = call(
        runtime,
        "LAMBDA",
        std::iter::once(&wrapper_params).chain(wrapper_body.iter()),
    )?;

    let quoted_access = quote(runtime, access_fn)?;
    let store_count = runtime
        .fixnum(store_vars.len() as isize)
        .expect("store count fits in fixnum");
    let positional_count = runtime
        .fixnum(parsed.positional_count as isize)
        .expect("positional count fits in fixnum");
    let has_keys = runtime.root(if parsed.has_keys { Word::T } else { Word::NIL });
    let quoted_doc = quote(runtime, &documentation)?;
    let definition = native_call(
        runtime,
        DEFINE_LONG_DEFSETF,
        [
            &quoted_access,
            &wrapper_lambda,
            &store_count,
            &positional_count,
            &has_keys,
            &quoted_doc,
        ],
    )?;
    define_at_toplevel(runtime, definition)
}

struct DefsetfLambdaList<'rt> {
    clean_lambda_list: Root<'rt>,
    env_var: Option<Root<'rt>>,
    positional_count: usize,
    has_keys: bool,
}

fn parse_defsetf_lambda_list<'rt>(
    runtime: &'rt Runtime,
    form: &Root<'rt>,
) -> Result<DefsetfLambdaList<'rt>, EvalError> {
    let items = proper_list(runtime, form.clone(), "DEFSETF lambda list")?;
    let mut clean = Vec::with_capacity(items.len());
    let mut env_var = None;
    let mut positional_count = 0usize;
    let mut has_keys = false;
    let mut in_positional = true;
    let mut iter = items.into_iter();
    while let Some(item) = iter.next() {
        let marker = name_of_word(runtime, item.slot.get());
        match marker.as_ref().map(|name| name.operator()) {
            Some("&ENVIRONMENT") => {
                if env_var.is_some() {
                    return Err(invalid("DEFSETF", "duplicate &ENVIRONMENT parameter"));
                }
                let var = iter
                    .next()
                    .ok_or_else(|| invalid("DEFSETF", "&ENVIRONMENT requires a variable name"))?;
                if !is_bindable_lambda_var(runtime, var.slot.get()) {
                    return Err(invalid("DEFSETF", "expected a variable after &ENVIRONMENT"));
                }
                env_var = Some(var);
                continue;
            }
            Some("&WHOLE" | "&BODY" | "&AUX") => {
                return Err(invalid(
                    "DEFSETF",
                    "lambda list does not support &WHOLE, &BODY, or &AUX",
                ));
            }
            Some("&REST") => in_positional = false,
            Some("&KEY") => {
                in_positional = false;
                has_keys = true;
            }
            Some("&OPTIONAL" | "&ALLOW-OTHER-KEYS") => {}
            _ if in_positional => positional_count += 1,
            _ => {}
        }
        clean.push(item);
    }
    Ok(DefsetfLambdaList {
        clean_lambda_list: list(runtime, clean.iter())?,
        env_var,
        positional_count,
        has_keys,
    })
}

fn split_doc_and_body<'a, 'rt>(
    runtime: &'rt Runtime,
    body: &'a [Root<'rt>],
) -> (Root<'rt>, Vec<Root<'rt>>, &'a [Root<'rt>]) {
    let mut doc: Option<Root<'rt>> = None;
    let mut declarations = Vec::new();
    let mut index = 0;
    while index < body.len() {
        let form = &body[index];
        let is_declare = runtime
            .cons_fields(form)
            .and_then(|(head, _)| name_of_word(runtime, head.slot.get()))
            .is_some_and(|name| name.operator() == "DECLARE");
        if is_declare {
            declarations.push(form.clone());
            index += 1;
        } else if doc.is_none() && runtime.heap.is_string(form.slot.get()) && index + 1 < body.len()
        {
            doc = Some(form.clone());
            index += 1;
        } else {
            break;
        }
    }
    (
        doc.unwrap_or_else(|| runtime.nil()),
        declarations,
        &body[index..],
    )
}

pub(super) fn expand_define_modify_macro<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let (name, lambda_list, function, documentation) = match forms {
        [name, lambda_list, function] => (name, lambda_list, function, None),
        [name, lambda_list, function, doc] if runtime.heap.is_string(doc.slot.get()) => {
            (name, lambda_list, function, Some(doc))
        }
        _ => {
            return Err(invalid(
                "DEFINE-MODIFY-MACRO",
                "expects a name, lambda list, function, and optional documentation string",
            ));
        }
    };
    if !is_definable_symbol(runtime, name.slot.get()) {
        return Err(invalid(
            "DEFINE-MODIFY-MACRO",
            "expects a definable symbol as the macro name",
        ));
    }
    // LW Compat also uses a literal lambda as the operator of the update
    // form. The expansion already preserves the operator in that position.
    if !(runtime.heap.is_symbol(function.slot.get()) && !function.is_nil()) {
        let lambda = runtime.cons_fields(function).is_some_and(|(head, tail)| {
            runtime.find_named_symbol(&Name::common_lisp("LAMBDA")) == Some(head.slot.get())
                && runtime.cons_fields(&tail).is_some()
        });
        if !lambda {
            return Err(invalid(
                "DEFINE-MODIFY-MACRO",
                "expected a function name or lambda expression",
            ));
        }
    }

    let original_items = proper_list(
        runtime,
        lambda_list.clone(),
        "DEFINE-MODIFY-MACRO lambda list",
    )?;
    let parsed = parse_modify_lambda_list(runtime, &original_items)?;

    let place_param = fresh(runtime, "PLACE")?;
    let env_param = fresh(runtime, "ENV")?;
    let env_marker = super::symbol(runtime, "&ENVIRONMENT")?;
    let mut macro_lambda_items = Vec::with_capacity(original_items.len() + 3);
    macro_lambda_items.push(place_param.clone());
    macro_lambda_items.extend(original_items);
    macro_lambda_items.push(env_marker);
    macro_lambda_items.push(env_param.clone());
    let macro_lambda_list = list(runtime, macro_lambda_items.iter())?;

    let temps_var = fresh(runtime, "TEMPS")?;
    let vals_var = fresh(runtime, "VALS")?;
    let stores_var = fresh(runtime, "STORES")?;
    let writer_var = fresh(runtime, "WRITER")?;
    let reader_var = fresh(runtime, "READER")?;

    let gse_vars = list(
        runtime,
        [&temps_var, &vals_var, &stores_var, &writer_var, &reader_var].into_iter(),
    )?;
    let gse_call = call(
        runtime,
        "GET-SETF-EXPANSION",
        [&place_param, &env_param].into_iter(),
    )?;

    let list_sym = super::symbol(runtime, "LIST")?;
    let list_fn = call(runtime, "FUNCTION", std::iter::once(&list_sym))?;
    let bindings_expr = call(
        runtime,
        "MAPCAR",
        [&list_fn, &temps_var, &vals_var].into_iter(),
    )?;

    let quoted_fn = quote(runtime, function)?;
    let mut fn_call_args =
        Vec::with_capacity(2 + parsed.arg_vars.len() + usize::from(parsed.rest_var.is_some()));
    fn_call_args.push(quoted_fn);
    fn_call_args.push(reader_var.clone());
    fn_call_args.extend(parsed.arg_vars);
    let fn_call_expr = if let Some(rest_var) = parsed.rest_var {
        fn_call_args.push(rest_var);
        call(runtime, "LIST*", fn_call_args.iter())?
    } else {
        call(runtime, "LIST", fn_call_args.iter())?
    };

    let mvb_sym = quote(runtime, &super::symbol(runtime, "MULTIPLE-VALUE-BIND")?)?;
    let mvb_expr = call(
        runtime,
        "LIST",
        [&mvb_sym, &stores_var, &fn_call_expr, &writer_var].into_iter(),
    )?;

    let let_star_sym = quote(runtime, &super::symbol(runtime, "LET*")?)?;
    let let_star_expr = call(
        runtime,
        "LIST",
        [&let_star_sym, &bindings_expr, &mvb_expr].into_iter(),
    )?;

    let body = call(
        runtime,
        "MULTIPLE-VALUE-BIND",
        [&gse_vars, &gse_call, &let_star_expr].into_iter(),
    )?;

    let mut defmacro_args = vec![name.clone(), macro_lambda_list];
    if let Some(doc) = documentation {
        defmacro_args.push(doc.clone());
    }
    defmacro_args.push(body);
    call(runtime, "DEFMACRO", defmacro_args.iter())
}

struct ModifyLambdaList<'rt> {
    arg_vars: Vec<Root<'rt>>,
    rest_var: Option<Root<'rt>>,
}

fn check_modify_var(
    runtime: &Runtime,
    word: Word,
    seen: &mut HashSet<usize>,
) -> Result<(), EvalError> {
    if !is_bindable_lambda_var(runtime, word) {
        return Err(invalid(
            "DEFINE-MODIFY-MACRO",
            "expected a bindable parameter variable symbol",
        ));
    }
    let id = runtime
        .heap
        .symbol_attributes(word)
        .expect("bindable variable is a symbol")
        .id;
    if !seen.insert(id) {
        return Err(invalid(
            "DEFINE-MODIFY-MACRO",
            "duplicate parameter variable",
        ));
    }
    Ok(())
}

fn parse_modify_lambda_list<'rt>(
    runtime: &'rt Runtime,
    items: &[Root<'rt>],
) -> Result<ModifyLambdaList<'rt>, EvalError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Section {
        Required,
        Optional,
        Rest,
        Done,
    }

    let mut section = Section::Required;
    let mut arg_vars = Vec::with_capacity(items.len());
    let mut rest_var = None;
    let mut seen = HashSet::new();

    for item in items {
        let word = item.slot.get();
        if runtime.heap.is_symbol(word) {
            let marker = name_of_word(runtime, word);
            match marker.as_ref().map(|n| n.operator()) {
                Some("&OPTIONAL") => {
                    if section != Section::Required {
                        return Err(invalid(
                            "DEFINE-MODIFY-MACRO",
                            "&OPTIONAL must precede &REST and cannot be repeated",
                        ));
                    }
                    section = Section::Optional;
                }
                Some("&REST") => {
                    if !matches!(section, Section::Required | Section::Optional) {
                        return Err(invalid(
                            "DEFINE-MODIFY-MACRO",
                            "unexpected &REST in lambda list",
                        ));
                    }
                    section = Section::Rest;
                }
                Some(
                    "&KEY" | "&ALLOW-OTHER-KEYS" | "&AUX" | "&BODY" | "&WHOLE" | "&ENVIRONMENT",
                ) => {
                    return Err(invalid(
                        "DEFINE-MODIFY-MACRO",
                        "lambda list supports only required, &OPTIONAL, and &REST parameters",
                    ));
                }
                _ => match section {
                    Section::Required | Section::Optional => {
                        check_modify_var(runtime, word, &mut seen)?;
                        arg_vars.push(item.clone());
                    }
                    Section::Rest => {
                        check_modify_var(runtime, word, &mut seen)?;
                        rest_var = Some(item.clone());
                        section = Section::Done;
                    }
                    Section::Done => {
                        return Err(invalid(
                            "DEFINE-MODIFY-MACRO",
                            "unexpected parameter after &REST",
                        ));
                    }
                },
            }
        } else if section == Section::Optional {
            let parts = proper_list(
                runtime,
                item.clone(),
                "DEFINE-MODIFY-MACRO &OPTIONAL parameter",
            )?;
            let var = match parts.as_slice() {
                [var] | [var, _] => var,
                [var, _, supplied_p] => {
                    check_modify_var(runtime, supplied_p.slot.get(), &mut seen)?;
                    var
                }
                _ => {
                    return Err(invalid(
                        "DEFINE-MODIFY-MACRO",
                        "invalid &OPTIONAL parameter specification",
                    ));
                }
            };
            check_modify_var(runtime, var.slot.get(), &mut seen)?;
            arg_vars.push(var.clone());
        } else {
            return Err(invalid(
                "DEFINE-MODIFY-MACRO",
                "invalid parameter in modify-macro lambda list",
            ));
        }
    }

    if section == Section::Rest {
        return Err(invalid(
            "DEFINE-MODIFY-MACRO",
            "expected a variable after &REST",
        ));
    }

    Ok(ModifyLambdaList { arg_vars, rest_var })
}

fn is_definable_symbol(runtime: &Runtime, word: Word) -> bool {
    runtime.heap.symbol_attributes(word).is_some_and(|data| {
        !data.constant
            && !data.canonical
            && runtime.heap.symbol_home(word) != Some(Package::KEYWORD)
    })
}

pub(super) fn install_setf_expander<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = |msg| Fault::plain(Class::ProgramError, EvalError::new(at, msg));
    let [name, function, documentation] = args else {
        return Err(invalid(
            "expected access function, expander, and documentation",
        ));
    };
    let word = name.slot.get();
    if !is_definable_symbol(runtime, word) {
        return Err(invalid("expected a definable symbol"));
    }
    if !crate::vm::is_function(runtime, function.slot.get()) {
        return Err(invalid("expected an expander function"));
    }
    let doc = (documentation.slot.get() != Word::NIL).then_some(documentation.slot.get());
    runtime.define_setf_expander(
        word,
        SetfExpander::Function(runtime.root_slot(function.slot.get())),
        doc,
    );
    Ok(vec![name.clone()])
}

pub(super) fn install_short_defsetf<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = |msg| Fault::plain(Class::ProgramError, EvalError::new(at, msg));
    let [access_fn, update_fn, documentation] = args else {
        return Err(invalid(
            "expected access function, update function, and documentation",
        ));
    };
    let word = access_fn.slot.get();
    if !is_definable_symbol(runtime, word) {
        return Err(invalid("expected a definable symbol"));
    }
    if !runtime.heap.is_symbol(update_fn.slot.get()) || update_fn.is_nil() {
        return Err(invalid("expected a symbol as update operator"));
    }
    let doc = (documentation.slot.get() != Word::NIL).then_some(documentation.slot.get());
    runtime.define_setf_expander(
        word,
        SetfExpander::Short(runtime.root_slot(update_fn.slot.get())),
        doc,
    );
    Ok(vec![access_fn.clone()])
}

pub(super) fn install_long_defsetf<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = |msg| Fault::plain(Class::ProgramError, EvalError::new(at, msg));
    let [
        access_fn,
        function,
        store_count,
        positional_count,
        has_keys,
        documentation,
    ] = args
    else {
        return Err(invalid("expected long DEFSETF specification"));
    };
    let word = access_fn.slot.get();
    if !is_definable_symbol(runtime, word) {
        return Err(invalid("expected a definable symbol"));
    }
    if !crate::vm::is_function(runtime, function.slot.get()) {
        return Err(invalid("expected a DEFSETF expander function"));
    }
    let store_count = store_count
        .as_fixnum()
        .filter(|&n| n >= 0)
        .ok_or_else(|| invalid("invalid store count"))? as usize;
    let positional_count = positional_count
        .as_fixnum()
        .filter(|&n| n >= 0)
        .ok_or_else(|| invalid("invalid positional count"))? as usize;
    let doc = (documentation.slot.get() != Word::NIL).then_some(documentation.slot.get());
    runtime.define_setf_expander(
        word,
        SetfExpander::Long {
            function: runtime.root_slot(function.slot.get()),
            store_count,
            positional_count,
            has_keys: !has_keys.is_nil(),
        },
        doc,
    );
    Ok(vec![access_fn.clone()])
}

pub(super) fn expand_place<'rt>(
    runtime: &'rt Runtime,
    mut place: Root<'rt>,
    environment: &Root<'rt>,
    depth: usize,
) -> Result<Place<'rt>, EvalError> {
    if depth == macros::EXPANSION_LIMIT {
        return Err(invalid("place", "nesting exceeds the expansion limit"));
    }
    for _ in 0..macros::EXPANSION_LIMIT {
        if runtime.heap.is_symbol(place.slot.get()) {
            match macros::expand_once(runtime, &place, environment)? {
                Some(expansion) => {
                    if let Some(spec) = symbol_macro_type(runtime, &place, environment)? {
                        let mut expanded =
                            expand_place(runtime, expansion, environment, depth + 1)?;
                        expanded.reader =
                            call(runtime, "THE", [&spec, &expanded.reader].into_iter())?;
                        if let Some(store) = expanded.stores.first() {
                            let check = call(runtime, "THE", [&spec, store].into_iter())?;
                            expanded.writer = progn(runtime, vec![check, expanded.writer])?;
                        }
                        return Ok(expanded);
                    }
                    place = expansion;
                    continue;
                }
                None => return expand_variable_place(runtime, place),
            }
        }
        let Some((accessor, rest)) = runtime.cons_fields(&place) else {
            return Err(invalid("place", "expects a variable or an accessor form"));
        };
        let operator = name_of_word(runtime, accessor.slot.get())
            .ok_or_else(|| invalid("place", "expects an accessor name"))?;
        let args = proper_list(runtime, rest, "accessor arguments")?;
        match macros::local_operator_binding(runtime, &operator, environment.slot.get(), 0)? {
            Some(Some(_)) => {
                if let Some(expansion) = macros::expand_once(runtime, &place, environment)? {
                    place = expansion;
                    continue;
                }
            }
            Some(None) => return expand_setf_function_place(runtime, &accessor, args),
            None => {}
        }
        if let Some(expander) = runtime.setf_expander(accessor.slot.get()) {
            return expand_registered_setf(runtime, &place, &accessor, args, expander, environment);
        }
        if let Some(expanded) = expand_builtin_place(
            runtime,
            &accessor,
            operator.operator(),
            &args,
            environment,
            depth,
        )? {
            return Ok(expanded);
        }
        if let Some(expansion) = macros::expand_once(runtime, &place, environment)? {
            place = expansion;
            continue;
        }
        return expand_setf_function_place(runtime, &accessor, args);
    }
    Err(invalid("place", "macro expansion limit exceeded"))
}

fn symbol_macro_type<'rt>(
    runtime: &'rt Runtime,
    symbol: &Root<'rt>,
    environment: &Root<'rt>,
) -> Result<Option<Root<'rt>>, EvalError> {
    let Some(name) = Name::from_word(runtime, symbol.slot.get()) else {
        return Ok(None);
    };
    let id = macros::validate_environment(runtime, environment.slot.get(), 0)?;
    let spec = id.and_then(|id| {
        runtime.macro_environments.borrow()[&id]
            .variable_types
            .get(name.key())
            .cloned()
    });
    spec.map(|spec| crate::form_value::to_value(runtime, &spec))
        .transpose()
}

fn expand_variable_place<'rt>(
    runtime: &'rt Runtime,
    place: Root<'rt>,
) -> Result<Place<'rt>, EvalError> {
    let stored = fresh(runtime, "STORE VALUE")?;
    let writer = call(runtime, "SETQ", [&place, &stored].into_iter())?;
    Ok(Place {
        arguments: Vec::new(),
        stores: vec![stored],
        writer,
        reader: place,
    })
}

fn expand_setf_function_place<'rt>(
    runtime: &'rt Runtime,
    accessor: &Root<'rt>,
    args: Vec<Root<'rt>>,
) -> Result<Place<'rt>, EvalError> {
    let setf_name = call(runtime, "SETF", std::iter::once(accessor))?;
    let setter = call(runtime, "FUNCTION", std::iter::once(&setf_name))?;
    expand_accessor_place(
        runtime,
        accessor,
        AccessorWriter::SetfFunction(setter),
        args.into_iter(),
    )
}

fn expand_registered_setf<'rt>(
    runtime: &'rt Runtime,
    place: &Root<'rt>,
    accessor: &Root<'rt>,
    args: Vec<Root<'rt>>,
    expander: SetfExpanderRef,
    environment: &Root<'rt>,
) -> Result<Place<'rt>, EvalError> {
    match expander {
        SetfExpanderRef::Function(word) => {
            let function = runtime.root(word);
            let results = runtime.call(&function, &[place, environment])?;
            let [temps, forms, stores, writer, reader] = results.as_slice() else {
                return Err(invalid(
                    "SETF expander",
                    "must return five values (temps, forms, stores, writer, reader)",
                ));
            };
            let temporaries = proper_list(runtime, temps.clone(), "SETF temporaries")?;
            let forms = proper_list(runtime, forms.clone(), "SETF value forms")?;
            if temporaries.len() != forms.len() {
                return Err(invalid(
                    "SETF expander",
                    "returned mismatched temporary and value-form counts",
                ));
            }
            let stores = proper_list(runtime, stores.clone(), "SETF store variables")?;
            Ok(Place {
                arguments: temporaries.into_iter().zip(forms).collect(),
                stores,
                writer: writer.clone(),
                reader: reader.clone(),
            })
        }
        SetfExpanderRef::Short(update) => expand_accessor_place(
            runtime,
            accessor,
            AccessorWriter::UpdateFunction(runtime.root(update)),
            args.into_iter(),
        ),
        SetfExpanderRef::Long {
            function,
            store_count,
            positional_count,
            has_keys,
        } => {
            let function = runtime.root(function);
            let mut arguments = Vec::with_capacity(args.len());
            let mut call_args = Vec::with_capacity(args.len());
            let mut args = args.into_iter().enumerate();
            while let Some((index, arg)) = args.next() {
                let (form, temporary_name) = if has_keys
                    && index >= positional_count
                    && runtime.heap.symbol_home(arg.slot.get()) == Some(Package::KEYWORD)
                    && let Some((_, value)) = args.next()
                {
                    call_args.push(arg);
                    (value, "PLACE KEY ARGUMENT")
                } else {
                    (arg, "PLACE ARGUMENT")
                };
                let temporary = fresh(runtime, temporary_name)?;
                arguments.push((temporary.clone(), form));
                call_args.push(temporary);
            }
            let stores = (0..store_count)
                .map(|_| fresh(runtime, "STORE VALUE"))
                .collect::<Result<Vec<_>, _>>()?;
            let call_args_list = list(runtime, call_args.iter())?;
            let stores_list = list(runtime, stores.iter())?;
            let results = runtime.call(&function, &[&call_args_list, environment, &stores_list])?;
            let writer = results.into_iter().next().unwrap_or_else(|| runtime.nil());
            let reader = runtime
                .cons(accessor, &call_args_list)
                .map_err(|error| EvalError::out_of_memory(0, error))?;
            Ok(Place {
                arguments,
                stores,
                writer,
                reader,
            })
        }
    }
}

fn expand_builtin_place<'rt>(
    runtime: &'rt Runtime,
    accessor: &Root<'rt>,
    name: &str,
    args: &[Root<'rt>],
    environment: &Root<'rt>,
    depth: usize,
) -> Result<Option<Place<'rt>>, EvalError> {
    if name == "VALUES" {
        return expand_values(runtime, args, environment, depth + 1).map(Some);
    }
    if name == "GETF" {
        return expand_getf(runtime, args, environment, depth + 1).map(Some);
    }
    if name == "APPLY" {
        return expand_apply(runtime, args).map(Some);
    }
    if name == "LDB" || name == "MASK-FIELD" {
        return expand_ldb_place(runtime, name, args, environment, depth + 1).map(Some);
    }
    let setter = if name == "MACRO-FUNCTION" {
        Some(compiled_native(super::SET_MACRO_FUNCTION).expect("macro-function setter native"))
    } else {
        crate::vm::setf_primitive(name)
    };
    if let Some(setter) = setter {
        return expand_accessor_place(
            runtime,
            accessor,
            AccessorWriter::SetfFunction(runtime.root(setter)),
            args.iter().cloned(),
        )
        .map(Some);
    }
    if name == "THE"
        && let [type_specifier, inner] = args
    {
        let mut place = expand_place(runtime, inner.clone(), environment, depth + 1)?;
        place.reader = call(runtime, "THE", [type_specifier, &place.reader].into_iter())?;
        return Ok(Some(place));
    }
    if let [input] = args
        && let Some((structure, index)) = runtime.structure_place(accessor.slot.get())
    {
        let structure = runtime.root(structure);
        return expand_structure_slot(runtime, accessor.clone(), input.clone(), structure, index)
            .map(Some);
    }
    let (location, reader, write) = match (name, args) {
        ("SYMBOL-VALUE", [symbol]) => (symbol.clone(), "SYMBOL-VALUE", "SET"),
        ("NTH", [index, input]) => {
            let tail = call(runtime, "NTHCDR", [index, input].into_iter())?;
            (tail, "CAR", "RPLACA")
        }
        (_, [input]) if let Some(path) = cons_path(name) => {
            let (part, enclosing) = path.split_first().expect("a cons accessor has a step");
            let mut target = input.clone();
            for &step in enclosing.iter().rev() {
                let reader = if step == b'A' { "CAR" } else { "CDR" };
                target = call(runtime, reader, std::iter::once(&target))?;
            }
            if *part == b'A' {
                (target, "CAR", "RPLACA")
            } else {
                (target, "CDR", "RPLACD")
            }
        }
        _ => return Ok(None),
    };
    // A composite cons place is equivalent to a nested accessor place. Capture
    // its target before evaluating the new value, which may mutate the path.
    let temporary = fresh(runtime, "PLACE LOCATION")?;
    let stored = fresh(runtime, "STORE VALUE")?;
    let effect = call(runtime, write, [&temporary, &stored].into_iter())?;
    let writer = if write == "SET" {
        effect
    } else {
        call(runtime, "PROGN", [&effect, &stored].into_iter())?
    };
    let reader = call(runtime, reader, std::iter::once(&temporary))?;
    Ok(Some(Place {
        arguments: vec![(temporary, location)],
        stores: vec![stored],
        writer,
        reader,
    }))
}

fn expand_ldb_place<'rt>(
    runtime: &'rt Runtime,
    name: &str,
    args: &[Root<'rt>],
    environment: &Root<'rt>,
    depth: usize,
) -> Result<Place<'rt>, EvalError> {
    let [bytespec, target] = args else {
        return Err(invalid(name, "expects a byte specifier and a place"));
    };
    let inner = expand_place(runtime, target.clone(), environment, depth)?;
    let bytespec_temp = fresh(runtime, "BYTE SPECIFIER")?;
    let stored = fresh(runtime, "STORE VALUE")?;
    let reader = call(runtime, name, [&bytespec_temp, &inner.reader].into_iter())?;
    let deposit = if name == "LDB" {
        "DPB"
    } else {
        "DEPOSIT-FIELD"
    };
    let updated = call(
        runtime,
        deposit,
        [&stored, &bytespec_temp, &inner.reader].into_iter(),
    )?;
    let Some(inner_store) = inner.stores.first() else {
        return Err(invalid(name, "place has no store variable"));
    };
    let assign = call(runtime, "SETQ", [inner_store, &updated].into_iter())?;
    let writer = call(
        runtime,
        "PROGN",
        [&assign, &inner.writer, &stored].into_iter(),
    )?;
    let mut arguments = vec![(bytespec_temp, bytespec.clone())];
    arguments.extend(inner.arguments);
    Ok(Place {
        arguments,
        stores: vec![stored],
        writer,
        reader,
    })
}

fn expand_apply<'rt>(runtime: &'rt Runtime, args: &[Root<'rt>]) -> Result<Place<'rt>, EvalError> {
    let [fn_form, arg_forms @ ..] = args else {
        return Err(invalid("APPLY place", "expects a function and arguments"));
    };
    if arg_forms.is_empty() {
        return Err(invalid("APPLY place", "expects at least one argument form"));
    }
    let fn_parts = proper_list(runtime, fn_form.clone(), "APPLY place function")?;
    let [op, fn_name] = fn_parts.as_slice() else {
        return Err(invalid("APPLY place", "requires #'FUNCTION syntax"));
    };
    let Some(op_name) = name_of_word(runtime, op.slot.get()) else {
        return Err(invalid("APPLY place", "requires #'FUNCTION syntax"));
    };
    if op_name.operator() != "FUNCTION" {
        return Err(invalid("APPLY place", "requires #'FUNCTION syntax"));
    }
    let Some(target_name) = name_of_word(runtime, fn_name.slot.get()) else {
        return Err(invalid("APPLY place", "expects a named function"));
    };
    let arguments = place_arguments(runtime, arg_forms.iter().cloned(), "APPLY ARGUMENT")?;
    let temporaries = arguments.iter().map(|(temp, _)| temp);
    let stored = fresh(runtime, "STORE VALUE")?;
    let reader = call(
        runtime,
        "APPLY",
        std::iter::once(fn_form).chain(temporaries.clone()),
    )?;
    let setter = if let Some(primitive) = crate::vm::setf_primitive(target_name.operator()) {
        runtime.root(primitive)
    } else {
        let setf_name = call(runtime, "SETF", std::iter::once(fn_name))?;
        call(runtime, "FUNCTION", std::iter::once(&setf_name))?
    };
    let writer = call(
        runtime,
        "APPLY",
        [&setter, &stored].into_iter().chain(temporaries),
    )?;
    Ok(Place {
        arguments,
        stores: vec![stored],
        writer,
        reader,
    })
}

fn expand_structure_slot<'rt>(
    runtime: &'rt Runtime,
    accessor: Root<'rt>,
    input: Root<'rt>,
    structure: Root<'rt>,
    index: usize,
) -> Result<Place<'rt>, EvalError> {
    let temporary = fresh(runtime, "STRUCTURE")?;
    let stored = fresh(runtime, "STORE VALUE")?;
    let reader = call_with_operator(runtime, &accessor, std::iter::once(&temporary))?;
    let structure = call(runtime, "QUOTE", [&structure].into_iter())?;
    let index = runtime
        .fixnum(index as isize)
        .expect("structure slot index");
    let writer = native_call(
        runtime,
        super::WRITE_STRUCTURE_SLOT,
        [&temporary, &structure, &index, &stored],
    )?;
    Ok(Place {
        arguments: vec![(temporary, input)],
        stores: vec![stored],
        writer,
        reader,
    })
}

fn place_arguments<'rt>(
    runtime: &'rt Runtime,
    forms: impl Iterator<Item = Root<'rt>>,
    name: &str,
) -> Result<Vec<(Root<'rt>, Root<'rt>)>, EvalError> {
    forms
        .map(|form| Ok((fresh(runtime, name)?, form)))
        .collect()
}

enum AccessorWriter<'rt> {
    SetfFunction(Root<'rt>),
    UpdateFunction(Root<'rt>),
}

fn expand_accessor_place<'rt>(
    runtime: &'rt Runtime,
    accessor: &Root<'rt>,
    setter: AccessorWriter<'rt>,
    forms: impl Iterator<Item = Root<'rt>>,
) -> Result<Place<'rt>, EvalError> {
    let arguments = place_arguments(runtime, forms, "PLACE ARGUMENT")?;
    let temporaries = arguments.iter().map(|(temp, _)| temp);
    let stored = fresh(runtime, "STORE VALUE")?;
    let writer = match &setter {
        AccessorWriter::SetfFunction(function) => call(
            runtime,
            "FUNCALL",
            [function, &stored].into_iter().chain(temporaries.clone()),
        )?,
        AccessorWriter::UpdateFunction(function) => {
            call_with_operator(runtime, function, temporaries.clone().chain([&stored]))?
        }
    };
    let reader = call_with_operator(runtime, accessor, temporaries)?;
    Ok(Place {
        arguments,
        stores: vec![stored],
        writer,
        reader,
    })
}

fn expand_getf<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    environment: &Root<'rt>,
    depth: usize,
) -> Result<Place<'rt>, EvalError> {
    let (target, indicator, default) = match args {
        [target, indicator] => (target, indicator, None),
        [target, indicator, default] => (target, indicator, Some(default)),
        _ => {
            return Err(invalid(
                "GETF place",
                "expects a place, an indicator and an optional default",
            ));
        }
    };
    let mut target = expand_place(runtime, target.clone(), environment, depth)?;
    let key = fresh(runtime, "PROPERTY INDICATOR")?;
    let store = fresh(runtime, "STORE VALUE")?;
    let replacement = call_plist(runtime, plists::PUT, [&target.reader, &key, &store])?;
    let writer = write_plist(runtime, &target, &replacement, &store)?;
    let default_temp = default
        .map(|_| fresh(runtime, "DEFAULT PROPERTY VALUE"))
        .transpose()?;
    let reader = call(
        runtime,
        "GETF",
        [&target.reader, &key]
            .into_iter()
            .chain(default_temp.iter()),
    )?;
    target.arguments.push((key, indicator.clone()));
    if let Some((temp, form)) = default_temp.zip(default) {
        target.arguments.push((temp, form.clone()));
    }
    Ok(Place {
        arguments: target.arguments,
        stores: vec![store],
        writer,
        reader,
    })
}

pub(super) fn expand_remf<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    environment: &Root<'rt>,
) -> Expansion<'rt> {
    let [target, indicator] = forms else {
        return Err(invalid("REMF", "expects a place and a property indicator"));
    };
    let target = expand_place(runtime, target.clone(), environment, 0)?;
    let key = fresh(runtime, "PROPERTY INDICATOR")?;
    let plist = fresh(runtime, "NEW PROPERTY LIST")?;
    let found = fresh(runtime, "PROPERTY FOUND")?;
    let mut bindings = target.bindings(runtime)?;
    bindings.push(binding(runtime, &key, indicator)?);
    let remove = call_plist(runtime, plists::REMOVE, [&target.reader, &key])?;
    let writer = write_plist(runtime, &target, &plist, &found)?;
    let write_if_found = call(runtime, "IF", [&found, &writer].into_iter())?;
    let body = bind_values(runtime, &[plist, found], &remove, &write_if_found)?;
    let_star(runtime, &bindings, body)
}

fn write_plist<'rt>(
    runtime: &'rt Runtime,
    target: &Place<'rt>,
    plist: &Root<'rt>,
    result: &Root<'rt>,
) -> Expansion<'rt> {
    let writer = call(runtime, "PROGN", [&target.writer, result].into_iter())?;
    bind_values(runtime, &target.stores, plist, &writer)
}

fn call_plist<'rt, const N: usize>(
    runtime: &'rt Runtime,
    operation: &str,
    args: [&Root<'rt>; N],
) -> Expansion<'rt> {
    let function = runtime.root(Primitive::named(operation).expect("registered plist operation"));
    call(runtime, "FUNCALL", std::iter::once(&function).chain(args))
}

fn expand_values<'rt>(
    runtime: &'rt Runtime,
    places: &[Root<'rt>],
    environment: &Root<'rt>,
    depth: usize,
) -> Result<Place<'rt>, EvalError> {
    let mut arguments = Vec::new();
    let mut stores = Vec::new();
    let mut readers = Vec::new();
    let mut writers = Vec::new();
    for place in places {
        let place = expand_place(runtime, place.clone(), environment, depth)?;
        arguments.extend(place.arguments);
        readers.push(place.reader);
        let extra = if let Some((primary, other)) = place.stores.split_first() {
            stores.push(primary.clone());
            other
        } else {
            &[]
        };
        let nil = runtime.nil();
        let bindings = extra
            .iter()
            .map(|store| binding(runtime, store, &nil))
            .collect::<Result<Vec<_>, _>>()?;
        writers.push(let_star(runtime, &bindings, place.writer)?);
    }
    Ok(Place {
        arguments,
        stores,
        writer: call(runtime, "VALUES", writers.iter())?,
        reader: call(runtime, "VALUES", readers.iter())?,
    })
}

fn cons_path(accessor: &str) -> Option<&[u8]> {
    match accessor {
        "FIRST" => Some(b"A"),
        "REST" => Some(b"D"),
        "SECOND" => Some(b"AD"),
        "THIRD" => Some(b"ADD"),
        "FOURTH" => Some(b"ADDD"),
        "FIFTH" => Some(b"ADDDD"),
        "SIXTH" => Some(b"ADDDDD"),
        "SEVENTH" => Some(b"ADDDDDD"),
        "EIGHTH" => Some(b"ADDDDDDD"),
        "NINTH" => Some(b"ADDDDDDDD"),
        "TENTH" => Some(b"ADDDDDDDDD"),
        _ => {
            let path = accessor.strip_prefix('C')?.strip_suffix('R')?.as_bytes();
            ((1..=4).contains(&path.len()) && path.iter().all(|&step| matches!(step, b'A' | b'D')))
                .then_some(path)
        }
    }
}
