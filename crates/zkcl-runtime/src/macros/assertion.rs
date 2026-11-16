use crate::{EvalError, Root, Runtime};

use super::{
    Expansion, call, cl_string, fresh, invalid, keyword_symbol, list, proper_list, quote, symbol,
};

pub(super) fn expand<'rt>(runtime: &'rt Runtime, operands: &[Root<'rt>]) -> Expansion<'rt> {
    let Some((test, options)) = operands.split_first() else {
        return Err(invalid("ASSERT", "requires a test form"));
    };
    let places = match options.first() {
        Some(places) => proper_list(runtime, places.clone(), "ASSERT places")?,
        None => Vec::new(),
    };
    let error = match options.get(1..) {
        Some(arguments) if !arguments.is_empty() => call(runtime, "ERROR", arguments.iter())?,
        _ => default_error(runtime, test)?,
    };

    let retry = fresh(runtime, "ASSERT RETRY")?;
    let done = fresh(runtime, "ASSERT DONE")?;
    let passed = call(runtime, "GO", [&done].into_iter())?;
    let check = call(runtime, "IF", [test, &passed].into_iter())?;
    let continue_clause = continuation(runtime, &places)?;
    let failure = call(
        runtime,
        "RESTART-CASE",
        [&error, &continue_clause].into_iter(),
    )?;
    let repeat = call(runtime, "GO", [&retry].into_iter())?;
    // TAGBODY returns NIL. The failure path resumes at the same test.
    call(
        runtime,
        "TAGBODY",
        [&retry, &check, &failure, &repeat, &done].into_iter(),
    )
}

fn default_error<'rt>(runtime: &'rt Runtime, test: &Root<'rt>) -> Expansion<'rt> {
    let control = runtime
        .heap
        .string("The assertion ~S failed.")
        .map_err(|error| EvalError::out_of_memory(0, error))?;
    let control = runtime.root(control);
    let test = call(runtime, "QUOTE", [test].into_iter())?;
    call(runtime, "ERROR", [&control, &test].into_iter())
}

fn continuation<'rt>(runtime: &'rt Runtime, places: &[Root<'rt>]) -> Expansion<'rt> {
    let name = symbol(runtime, "CONTINUE")?;
    let report = keyword_symbol(runtime, "REPORT")?;
    let text = runtime.root(cl_string(runtime, "Retry the assertion."));
    if places.is_empty() {
        return list(runtime, [&name, &runtime.nil(), &report, &text].into_iter());
    }

    // The same private marker represents an omitted programmatic argument and
    // a place the user chose to leave alone in the interactive debugger.
    let skip = quote(runtime, &fresh(runtime, "KEEP ASSERT PLACE")?)?;
    let query_io = symbol(runtime, "*QUERY-IO*")?;
    let debug_io = symbol(runtime, "*DEBUG-IO*")?;
    let question = runtime.root(cl_string(runtime, "Supply a new value for ~S? "));
    let prompt = runtime.root(cl_string(runtime, "~&New value for ~S: "));
    let mut parameters = vec![symbol(runtime, "&OPTIONAL")?];
    let mut inputs = Vec::with_capacity(places.len());
    let mut assignments = Vec::with_capacity(places.len());
    for place in places {
        let value = fresh(runtime, "ASSERT REPLACEMENT")?;
        parameters.push(list(runtime, [&value, &skip].into_iter())?);
        let keep = call(runtime, "EQ", [&value, &skip].into_iter())?;
        let cons = call(runtime, "CONSP", [&value].into_iter())?;
        let head = call(runtime, "CAR", [&value].into_iter())?;
        let tagged = call(runtime, "EQ", [&head, &skip].into_iter())?;
        let interactive_values = call(runtime, "AND", [&cons, &tagged].into_iter())?;
        let tail = call(runtime, "CDR", [&value].into_iter())?;
        let values = call(runtime, "VALUES-LIST", [&tail].into_iter())?;
        let replacement = call(
            runtime,
            "IF",
            [&interactive_values, &values, &value].into_iter(),
        )?;
        let setter = call(runtime, "SETF", [place, &replacement].into_iter())?;
        assignments.push(call(runtime, "UNLESS", [&keep, &setter].into_iter())?);

        let label = quote(runtime, place)?;
        let change = call(runtime, "Y-OR-N-P", [&question, &label].into_iter())?;
        let display = call(runtime, "FORMAT", [&debug_io, &prompt, &label].into_iter())?;
        let flush = call(runtime, "FINISH-OUTPUT", [&debug_io].into_iter())?;
        let read = call(runtime, "READ", [&debug_io].into_iter())?;
        let eval = call(runtime, "EVAL", [&read].into_iter())?;
        let values = call(runtime, "MULTIPLE-VALUE-LIST", [&eval].into_iter())?;
        let packed = call(runtime, "CONS", [&skip, &values].into_iter())?;
        let input = call(runtime, "PROGN", [&display, &flush, &packed].into_iter())?;
        inputs.push(call(runtime, "IF", [&change, &input, &skip].into_iter())?);
    }
    let arguments = call(runtime, "LIST", inputs.iter())?;
    let binding = list(runtime, [&query_io, &debug_io].into_iter())?;
    let bindings = list(runtime, [&binding].into_iter())?;
    let read = call(runtime, "LET", [&bindings, &arguments].into_iter())?;
    let interactive = keyword_symbol(runtime, "INTERACTIVE")?;
    let reader = call(runtime, "LAMBDA", [&runtime.nil(), &read].into_iter())?;
    let parameters = list(runtime, parameters.iter())?;
    list(
        runtime,
        [&name, &parameters, &report, &text, &interactive, &reader]
            .into_iter()
            .chain(&assignments),
    )
}
