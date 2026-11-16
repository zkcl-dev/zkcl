use std::iter::once;

use crate::heap::Word;
use crate::{EvalError, Root, Runtime};

use super::{
    Expansion, HASH_TABLE_STEP, PACKAGE_ITERATOR_ENTRIES, binding, call, fresh, invalid,
    keyword_name_opt, list, native_call, proper_list, symbol,
};

pub(super) fn dotimes<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let operator = "DOTIMES";
    let Some((specification, body)) = forms.split_first() else {
        return Err(invalid(operator, "requires a control specification"));
    };
    let specification = proper_list(runtime, specification.clone(), operator)?;
    let nil = runtime.nil();
    let (variable, input, result) = match specification.as_slice() {
        [variable, input] => (variable, input, &nil),
        [variable, input, result] => (variable, input, result),
        _ => {
            return Err(invalid(
                operator,
                "expects a variable, an input form and an optional result form",
            ));
        }
    };
    validate_variable(runtime, variable, operator)?;

    let (declarations, statements) = split_declarations(runtime, body)?;
    let statements = call(runtime, "TAGBODY", statements.iter())?;
    let cursor = super::fresh(runtime, "ITERATION CURSOR")?;
    let limit = super::fresh(runtime, "DOTIMES LIMIT")?;
    let zero = runtime.fixnum(0).unwrap();
    let one = runtime.fixnum(1).unwrap();
    let limit_binding = list(runtime, [&limit, input].into_iter())?;
    let index_binding = list(runtime, [&cursor, &zero].into_iter())?;
    let bindings = list(runtime, [&limit_binding, &index_binding].into_iter())?;
    let in_range = call(runtime, "<", [&cursor, &limit].into_iter())?;
    let advance = call(runtime, "+", [&cursor, &one].into_iter())?;

    let body = iteration_binding(runtime, variable, &cursor, declarations, &statements)?;
    let result = iteration_binding(runtime, variable, &cursor, declarations, result)?;
    let advance = call(runtime, "SETQ", [&cursor, &advance].into_iter())?;
    let repeat = repeat(
        runtime,
        RepeatCondition::While(&in_range),
        &result,
        &body,
        Some(&advance),
    )?;
    let scope = call(runtime, "LET", [&bindings, &repeat].into_iter())?;
    call(runtime, "BLOCK", [&runtime.nil(), &scope].into_iter())
}

pub(super) fn dolist<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let operator = "DOLIST";
    let Some((specification, body)) = forms.split_first() else {
        return Err(invalid(operator, "requires a control specification"));
    };
    let specification = proper_list(runtime, specification.clone(), operator)?;
    let nil = runtime.nil();
    let (variable, input, result) = match specification.as_slice() {
        [variable, input] => (variable, input, &nil),
        [variable, input, result] => (variable, input, result),
        _ => {
            return Err(invalid(
                operator,
                "expects a variable, an input form and an optional result form",
            ));
        }
    };
    validate_variable(runtime, variable, operator)?;
    expand_list_loop(runtime, variable, input, result, body)
}

pub(super) fn do_symbols<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_symbol_loop(runtime, forms, SymbolLoop::AllInPackage)
}

pub(super) fn do_external_symbols<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    expand_symbol_loop(runtime, forms, SymbolLoop::ExternalInPackage)
}

pub(super) fn do_all_symbols<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_symbol_loop(runtime, forms, SymbolLoop::AllPackages)
}

pub(super) fn with_package_iterator<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let op = "WITH-PACKAGE-ITERATOR";
    let Some((spec, body)) = forms.split_first() else {
        return Err(invalid(op, "requires an iterator specification"));
    };
    let spec = proper_list(runtime, spec.clone(), op)?;
    let [name, package_list, symbol_types @ ..] = spec.as_slice() else {
        return Err(invalid(
            op,
            "expects an iterator name, package list, and symbol types",
        ));
    };
    if name.is_nil() || !runtime.heap.is_symbol(name.slot.get()) {
        return Err(invalid(op, "expects a symbol as the iterator name"));
    }
    if symbol_types.is_empty() {
        return Err(invalid(op, "requires at least one symbol type"));
    }
    let mut mask = 0isize;
    for kind in symbol_types {
        match keyword_name_opt(runtime, kind).as_deref() {
            Some("INTERNAL") => mask |= 1,
            Some("EXTERNAL") => mask |= 2,
            Some("INHERITED") => mask |= 4,
            _ => return Err(invalid(op, "expects :INTERNAL, :EXTERNAL, or :INHERITED")),
        }
    }
    let entries_var = fresh(runtime, "PACKAGE ENTRIES")?;
    let mask_word = runtime.fixnum(mask).unwrap();
    let collect = native_call(
        runtime,
        PACKAGE_ITERATOR_ENTRIES,
        [package_list, &mask_word],
    )?;
    let sym = fresh(runtime, "SYM")?;
    let status = fresh(runtime, "STATUS")?;
    let pkg = fresh(runtime, "PKG")?;
    let t = runtime.root(Word::T);
    let step_bindings = list(
        runtime,
        [
            &binding(runtime, &sym, &call(runtime, "CAR", once(&entries_var))?)?,
            &binding(
                runtime,
                &status,
                &call(runtime, "CADR", once(&entries_var))?,
            )?,
            &binding(runtime, &pkg, &call(runtime, "CADDR", once(&entries_var))?)?,
        ]
        .into_iter(),
    )?;
    let advance = call(
        runtime,
        "SETQ",
        [&entries_var, &call(runtime, "CDDDR", once(&entries_var))?].into_iter(),
    )?;
    let yield_values = call(runtime, "VALUES", [&t, &sym, &status, &pkg].into_iter())?;
    let step_body = call(runtime, "PROGN", [&advance, &yield_values].into_iter())?;
    let when_more = call(runtime, "LET", [&step_bindings, &step_body].into_iter())?;
    let iter_body = call(
        runtime,
        "IF",
        [&entries_var, &when_more, &runtime.nil()].into_iter(),
    )?;
    let fn_def = list(runtime, [name, &runtime.nil(), &iter_body].into_iter())?;
    let fns = list(runtime, once(&fn_def))?;
    let flet = call(runtime, "FLET", once(&fns).chain(body))?;
    let outer_bindings = list(runtime, once(&binding(runtime, &entries_var, &collect)?))?;
    call(runtime, "LET", [&outer_bindings, &flet].into_iter())
}

pub(super) fn with_hash_table_iterator<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let op = "WITH-HASH-TABLE-ITERATOR";
    let Some((spec, body)) = forms.split_first() else {
        return Err(invalid(op, "requires an iterator specification"));
    };
    let spec = proper_list(runtime, spec.clone(), op)?;
    let [name, table_form] = spec.as_slice() else {
        return Err(invalid(
            op,
            "expects an iterator name and a hash table form",
        ));
    };
    if name.is_nil() || !runtime.heap.is_symbol(name.slot.get()) {
        return Err(invalid(op, "expects a symbol as the iterator name"));
    }
    let table_var = fresh(runtime, "HASH TABLE")?;
    let cursor_var = fresh(runtime, "HASH CURSOR")?;
    let zero = runtime.fixnum(0).unwrap();
    let minus_one = runtime.fixnum(-1).unwrap();
    let check = native_call(runtime, HASH_TABLE_STEP, [&table_var, &minus_one])?;
    let step = native_call(runtime, HASH_TABLE_STEP, [&table_var, &cursor_var])?;
    let next = fresh(runtime, "NEXT")?;
    let key = fresh(runtime, "KEY")?;
    let val = fresh(runtime, "VAL")?;
    let mv_vars = list(runtime, [&next, &key, &val].into_iter())?;
    let set_cursor = call(runtime, "SETQ", [&cursor_var, &next].into_iter())?;
    let yield_values = call(
        runtime,
        "VALUES",
        [&runtime.root(Word::T), &key, &val].into_iter(),
    )?;
    let when_next = call(runtime, "PROGN", [&set_cursor, &yield_values].into_iter())?;
    let branch = call(
        runtime,
        "IF",
        [&next, &when_next, &runtime.nil()].into_iter(),
    )?;
    let iter_body = call(
        runtime,
        "MULTIPLE-VALUE-BIND",
        [&mv_vars, &step, &branch].into_iter(),
    )?;
    // CLHS defines the iterator name as a local macro, so MACROEXPAND must
    // find it in the environment where WITH-HASH-TABLE-ITERATOR appears.
    let expansion = call(runtime, "QUOTE", once(&iter_body))?;
    let macro_def = list(runtime, [name, &runtime.nil(), &expansion].into_iter())?;
    let macros = list(runtime, once(&macro_def))?;
    let macrolet = call(runtime, "MACROLET", once(&macros).chain(body))?;
    let outer_bindings = list(
        runtime,
        [
            &binding(runtime, &table_var, table_form)?,
            &binding(runtime, &cursor_var, &zero)?,
        ]
        .into_iter(),
    )?;
    call(
        runtime,
        "LET",
        [&outer_bindings, &check, &macrolet].into_iter(),
    )
}

#[derive(Clone, Copy)]
enum SymbolLoop {
    AllInPackage,
    ExternalInPackage,
    AllPackages,
}

fn expand_symbol_loop<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    kind: SymbolLoop,
) -> Expansion<'rt> {
    let operator = match kind {
        SymbolLoop::AllInPackage => "DO-SYMBOLS",
        SymbolLoop::ExternalInPackage => "DO-EXTERNAL-SYMBOLS",
        SymbolLoop::AllPackages => "DO-ALL-SYMBOLS",
    };
    let Some((specification, body)) = forms.split_first() else {
        return Err(invalid(operator, "requires a control specification"));
    };
    let specification = proper_list(runtime, specification.clone(), operator)?;
    let nil = runtime.nil();
    let default_pkg = symbol(runtime, "*PACKAGE*")?;
    let (variable, input, result) = match kind {
        SymbolLoop::AllPackages => match specification.as_slice() {
            [variable] => (variable, &nil, &nil),
            [variable, result] => (variable, &nil, result),
            _ => {
                return Err(invalid(
                    operator,
                    "expects a variable and an optional result form",
                ));
            }
        },
        SymbolLoop::AllInPackage | SymbolLoop::ExternalInPackage => {
            match specification.as_slice() {
                [variable] => (variable, &default_pkg, &nil),
                [variable, package] => (variable, package, &nil),
                [variable, package, result] => (variable, package, result),
                _ => {
                    return Err(invalid(
                        operator,
                        "expects a variable, optional package, and optional result form",
                    ));
                }
            }
        }
    };
    validate_variable(runtime, variable, operator)?;
    let mode = match kind {
        SymbolLoop::AllInPackage => 1 | 2 | 4 | 16,
        SymbolLoop::ExternalInPackage => 2 | 16,
        SymbolLoop::AllPackages => 8,
    };
    let mode_word = runtime.fixnum(mode).unwrap();
    let list_expr = native_call(runtime, PACKAGE_ITERATOR_ENTRIES, [input, &mode_word])?;
    expand_list_loop(runtime, variable, &list_expr, result, body)
}

pub(super) fn do_parallel<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_do(runtime, forms, false)
}

pub(super) fn do_sequential<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_do(runtime, forms, true)
}

fn expand_list_loop<'rt>(
    runtime: &'rt Runtime,
    variable: &Root<'rt>,
    input: &Root<'rt>,
    result: &Root<'rt>,
    body: &[Root<'rt>],
) -> Expansion<'rt> {
    let (declarations, statements) = split_declarations(runtime, body)?;
    let statements = call(runtime, "TAGBODY", statements.iter())?;
    let cursor = fresh(runtime, "ITERATION CURSOR")?;
    let binding_pair = list(runtime, [&cursor, input].into_iter())?;
    let bindings = list(runtime, once(&binding_pair))?;
    let done = call(runtime, "ENDP", once(&cursor))?;
    let advance = call(runtime, "CDR", once(&cursor))?;
    let item = call(runtime, "CAR", once(&cursor))?;
    let body = iteration_binding(runtime, variable, &item, declarations, &statements)?;
    let result = if result.is_nil() {
        runtime.nil()
    } else {
        iteration_binding(runtime, variable, &runtime.nil(), declarations, result)?
    };
    let advance = call(runtime, "SETQ", [&cursor, &advance].into_iter())?;
    let repeat = repeat(
        runtime,
        RepeatCondition::Until(&done),
        &result,
        &body,
        Some(&advance),
    )?;
    let scope = call(runtime, "LET", [&bindings, &repeat].into_iter())?;
    call(runtime, "BLOCK", [&runtime.nil(), &scope].into_iter())
}

fn iteration_binding<'rt>(
    runtime: &'rt Runtime,
    variable: &Root<'rt>,
    value: &Root<'rt>,
    declarations: &[Root<'rt>],
    body: &Root<'rt>,
) -> Expansion<'rt> {
    let binding = list(runtime, [variable, value].into_iter())?;
    let bindings = list(runtime, once(&binding))?;
    call(
        runtime,
        "LET",
        once(&bindings).chain(declarations).chain(once(body)),
    )
}

fn expand_do<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>], sequential: bool) -> Expansion<'rt> {
    let operator = if sequential { "DO*" } else { "DO" };
    let [specifications, exit, body @ ..] = forms else {
        return Err(invalid(operator, "requires a binding list and end test"));
    };
    let exit = proper_list(runtime, exit.clone(), operator)?;
    let Some((test, results)) = exit.split_first() else {
        return Err(invalid(operator, "requires an end test"));
    };
    let specifications = proper_list(runtime, specifications.clone(), operator)?;
    let mut bindings = Vec::with_capacity(specifications.len());
    let mut steps = Vec::new();
    for specification in specifications {
        let spec = if runtime.heap.is_symbol(specification.slot.get()) {
            vec![specification]
        } else {
            proper_list(runtime, specification, operator)?
        };
        let (binding, step) = match spec.as_slice() {
            [binding] => (std::slice::from_ref(binding), None),
            [_, _] => (spec.as_slice(), None),
            [_, _, step] => (&spec[..2], Some(step)),
            _ => return Err(invalid(operator, "invalid variable specification")),
        };
        validate_variable(runtime, &binding[0], operator)?;
        bindings.push(list(runtime, binding.iter())?);
        if let Some(step) = step {
            steps.push((binding[0].clone(), step.clone()));
        }
    }

    let bindings = list(runtime, bindings.iter())?;
    let (declarations, statements) = split_declarations(runtime, body)?;
    let statements = call(runtime, "TAGBODY", statements.iter())?;
    let result = call(runtime, "PROGN", results.iter())?;
    let advance = if steps.is_empty() {
        None
    } else if sequential {
        Some(call(
            runtime,
            "SETQ",
            steps.iter().flat_map(|(variable, step)| [variable, step]),
        )?)
    } else {
        Some(parallel_step(runtime, &steps)?)
    };
    let repeat = repeat(
        runtime,
        RepeatCondition::Until(test),
        &result,
        &statements,
        advance.as_ref(),
    )?;
    let binding_form = if sequential { "LET*" } else { "LET" };
    let scope = call(
        runtime,
        binding_form,
        once(&bindings).chain(declarations).chain(once(&repeat)),
    )?;
    call(runtime, "BLOCK", [&runtime.nil(), &scope].into_iter())
}

fn parallel_step<'rt>(runtime: &'rt Runtime, steps: &[(Root<'rt>, Root<'rt>)]) -> Expansion<'rt> {
    let mut bindings = Vec::with_capacity(steps.len());
    let mut assignments = Vec::with_capacity(steps.len() * 2);
    for (variable, step) in steps {
        let temp = fresh(runtime, "DO STEP")?;
        bindings.push(list(runtime, [&temp, step].into_iter())?);
        assignments.push(variable.clone());
        assignments.push(temp);
    }
    let bindings = list(runtime, bindings.iter())?;
    let assign = call(runtime, "SETQ", assignments.iter())?;
    call(runtime, "LET", [&bindings, &assign].into_iter())
}

enum RepeatCondition<'a, 'rt> {
    While(&'a Root<'rt>),
    Until(&'a Root<'rt>),
}

fn repeat<'rt>(
    runtime: &'rt Runtime,
    condition: RepeatCondition<'_, 'rt>,
    result: &Root<'rt>,
    body: &Root<'rt>,
    advance: Option<&Root<'rt>>,
) -> Expansion<'rt> {
    let tag = fresh(runtime, "ITERATION START")?;
    let leave = call(runtime, "RETURN-FROM", [&runtime.nil(), result].into_iter())?;
    let again = call(runtime, "GO", once(&tag))?;
    let iteration = once(body).chain(advance).chain(once(&again));
    match condition {
        RepeatCondition::While(test) => {
            // Use IF's false edge for the exit so the comparison can lower
            // directly to a branch in the VM.
            let next = call(runtime, "PROGN", iteration)?;
            let check = call(runtime, "IF", [test, &next, &leave].into_iter())?;
            call(runtime, "TAGBODY", [&tag, &check].into_iter())
        }
        RepeatCondition::Until(test) => {
            let check = call(runtime, "IF", [test, &leave].into_iter())?;
            call(
                runtime,
                "TAGBODY",
                [&tag, &check].into_iter().chain(iteration),
            )
        }
    }
}

fn validate_variable(
    runtime: &Runtime,
    variable: &Root<'_>,
    operator: &str,
) -> Result<(), EvalError> {
    if runtime.heap.is_variable(variable.slot.get()) {
        Ok(())
    } else {
        Err(invalid(operator, "expected a variable name"))
    }
}

fn split_declarations<'a, 'rt>(
    runtime: &'rt Runtime,
    body: &'a [Root<'rt>],
) -> Result<(&'a [Root<'rt>], &'a [Root<'rt>]), EvalError> {
    let declare = symbol(runtime, "DECLARE")?;
    let declaration_count = body
        .iter()
        .take_while(|form| {
            runtime
                .cons_fields(form)
                .is_some_and(|(head, _)| head.same_identity(&declare))
        })
        .count();
    Ok(body.split_at(declaration_count))
}
