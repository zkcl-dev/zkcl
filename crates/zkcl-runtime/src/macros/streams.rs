use crate::form_value::name_of_word;
use crate::heap::Word;
use crate::symbol::Name;
use crate::{Root, Runtime};

use super::{
    Expansion, binding, call, fresh, invalid, keyword_name_opt as keyword_name, list, proper_list,
    quote, symbol,
};

pub(super) fn with_standard_io_syntax<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let user_pkg = call(
        runtime,
        "FIND-PACKAGE",
        [&runtime.root(
            runtime
                .heap
                .string("COMMON-LISP-USER")
                .map_err(|e| crate::EvalError::out_of_memory(0, e))?,
        )]
        .into_iter(),
    )?;
    let upcase = runtime.root(runtime.intern(&Name::keyword("UPCASE"), 0)?);
    let single_float = quote(runtime, &symbol(runtime, "SINGLE-FLOAT")?)?;
    let ten = runtime.fixnum(10).unwrap();
    let t = runtime.root(Word::T);
    let nil = runtime.nil();
    let standard_readtable = call(runtime, "COPY-READTABLE", [&nil].into_iter())?;

    let specs: [(&str, &Root<'rt>); 21] = [
        ("*PACKAGE*", &user_pkg),
        ("*PRINT-ARRAY*", &t),
        ("*PRINT-BASE*", &ten),
        ("*PRINT-CASE*", &upcase),
        ("*PRINT-CIRCLE*", &nil),
        ("*PRINT-ESCAPE*", &t),
        ("*PRINT-GENSYM*", &t),
        ("*PRINT-LENGTH*", &nil),
        ("*PRINT-LEVEL*", &nil),
        ("*PRINT-LINES*", &nil),
        ("*PRINT-MISER-WIDTH*", &nil),
        ("*PRINT-PPRINT-DISPATCH*", &nil),
        ("*PRINT-PRETTY*", &nil),
        ("*PRINT-RADIX*", &nil),
        ("*PRINT-READABLY*", &t),
        ("*PRINT-RIGHT-MARGIN*", &nil),
        ("*READ-BASE*", &ten),
        ("*READ-DEFAULT-FLOAT-FORMAT*", &single_float),
        ("*READ-EVAL*", &t),
        ("*READ-SUPPRESS*", &nil),
        ("*READTABLE*", &standard_readtable),
    ];
    let mut bindings = Vec::with_capacity(specs.len());
    for (var, val) in specs {
        bindings.push(binding(runtime, &symbol(runtime, var)?, val)?);
    }
    let bindings = list(runtime, bindings.iter())?;
    call(runtime, "LET", std::iter::once(&bindings).chain(forms))
}

pub(super) fn with_input_from_string<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let name = "WITH-INPUT-FROM-STRING";
    let Some((spec, body)) = forms.split_first() else {
        return Err(invalid(name, "expects a stream binding"));
    };
    let spec = proper_list(runtime, spec.clone(), name)?;
    let [variable, text, arguments @ ..] = spec.as_slice() else {
        return Err(invalid(name, "expects a variable and a string form"));
    };
    let text_temp = fresh(runtime, "INPUT STRING")?;
    let mut bindings = vec![list(runtime, [&text_temp, text].into_iter())?];
    let mut start = None;
    let mut end = None;
    let mut index = None;
    let (pairs, remainder) = arguments.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(invalid(name, "expected keyword and value pairs"));
    }
    for [key, value] in pairs {
        let slot = match keyword_name(runtime, key).as_deref() {
            Some("INDEX") if index.is_none() => {
                index = Some(value);
                continue;
            }
            Some("START") if start.is_none() => &mut start,
            Some("END") if end.is_none() => &mut end,
            _ => return Err(invalid(name, "unknown or repeated option")),
        };
        let temp = fresh(runtime, "STRING BOUND")?;
        bindings.push(list(runtime, [&temp, value].into_iter())?);
        *slot = Some(temp);
    }
    let start = start.unwrap_or_else(|| runtime.fixnum(0).expect("zero is a fixnum"));
    let end = end.unwrap_or_else(|| runtime.nil());
    let input = call(
        runtime,
        "MAKE-STRING-INPUT-STREAM",
        [&text_temp, &start, &end].into_iter(),
    )?;
    let hidden = fresh(runtime, "INPUT STREAM")?;
    bindings.push(list(runtime, [&hidden, &input].into_iter())?);
    let bindings = list(runtime, bindings.iter())?;
    let inner = scoped_stream(runtime, variable, &hidden, body, |body| {
        if let Some(index) = index {
            let operation = crate::vm::Primitive::named(crate::vm::streams::INPUT_STRING_INDEX)
                .expect("registered input string index function");
            let operation = runtime.root(operation);
            let position = call(runtime, "FUNCALL", [&operation, &hidden].into_iter())?;
            let assign = call(runtime, "SETF", [index, &position].into_iter())?;
            call(
                runtime,
                "MULTIPLE-VALUE-PROG1",
                [&body, &assign].into_iter(),
            )
        } else {
            Ok(body)
        }
    })?;
    call(runtime, "LET*", [&bindings, &inner].into_iter())
}

pub(super) fn with_output_to_string<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
) -> Expansion<'rt> {
    let name = "WITH-OUTPUT-TO-STRING";
    let Some((spec, body)) = forms.split_first() else {
        return Err(invalid(name, "expects a stream binding"));
    };
    let spec = proper_list(runtime, spec.clone(), name)?;
    let (variable, existing, element_type) = match spec.as_slice() {
        [variable] => (variable, None, None),
        [variable, string] => (variable, Some(string), None),
        [variable, string, key, element_type]
            if keyword_name(runtime, key).as_deref() == Some("ELEMENT-TYPE") =>
        {
            (variable, Some(string), Some((key, element_type)))
        }
        _ => {
            return Err(invalid(
                name,
                "expected a stream variable and output options",
            ));
        }
    };
    let existing = existing.filter(|form| !form.is_nil());
    let output = if let Some(string) = existing {
        let operation = crate::vm::Primitive::named(crate::vm::streams::OUTPUT_TO_STRING)
            .expect("registered fill-pointer output stream constructor");
        let operation = runtime.root(operation);
        call(
            runtime,
            "FUNCALL",
            [&operation, string]
                .into_iter()
                .chain(element_type.map(|(_, value)| value)),
        )?
    } else if let Some((key, element_type)) = element_type {
        call(
            runtime,
            "MAKE-STRING-OUTPUT-STREAM",
            [key, element_type].into_iter(),
        )?
    } else {
        call(runtime, "MAKE-STRING-OUTPUT-STREAM", std::iter::empty())?
    };
    let hidden = fresh(runtime, "OUTPUT STREAM")?;
    let pair = list(runtime, [&hidden, &output].into_iter())?;
    let bindings = list(runtime, [&pair].into_iter())?;
    let inner = scoped_stream(runtime, variable, &hidden, body, |body| {
        if existing.is_some() {
            Ok(body)
        } else {
            let collect = call(runtime, "GET-OUTPUT-STREAM-STRING", [&hidden].into_iter())?;
            call(runtime, "PROGN", [&body, &collect].into_iter())
        }
    })?;
    call(runtime, "LET", [&bindings, &inner].into_iter())
}

pub(super) fn with_open_stream<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let name = "WITH-OPEN-STREAM";
    let Some((spec, body)) = forms.split_first() else {
        return Err(invalid(name, "expects a stream binding"));
    };
    let spec = proper_list(runtime, spec.clone(), name)?;
    let [variable, stream] = spec.as_slice() else {
        return Err(invalid(name, "expects a variable and a stream form"));
    };
    let hidden = fresh(runtime, "OPEN STREAM")?;
    let pair = list(runtime, [&hidden, stream].into_iter())?;
    let bindings = list(runtime, [&pair].into_iter())?;
    let inner = scoped_stream(runtime, variable, &hidden, body, Ok)?;
    call(runtime, "LET", [&bindings, &inner].into_iter())
}

pub(super) fn with_open_file<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let name = "WITH-OPEN-FILE";
    let Some((spec, body)) = forms.split_first() else {
        return Err(invalid(name, "expects a stream binding"));
    };
    let spec = proper_list(runtime, spec.clone(), name)?;
    let [variable, filespec, options @ ..] = spec.as_slice() else {
        return Err(invalid(name, "expects a variable and a file specification"));
    };
    let hidden = fresh(runtime, "OPEN FILE")?;
    let open = call(
        runtime,
        "OPEN",
        std::iter::once(filespec).chain(options.iter()),
    )?;
    let pair = list(runtime, [&hidden, &open].into_iter())?;
    let bindings = list(runtime, [&pair].into_iter())?;
    let inner = scoped_stream(runtime, variable, &hidden, body, Ok)?;
    let guarded = call(runtime, "WHEN", [&hidden, &inner].into_iter())?;
    call(runtime, "LET", [&bindings, &guarded].into_iter())
}

fn scoped_stream<'rt>(
    runtime: &'rt Runtime,
    variable: &Root<'rt>,
    hidden: &Root<'rt>,
    body: &[Root<'rt>],
    on_exit: impl FnOnce(Root<'rt>) -> Expansion<'rt>,
) -> Expansion<'rt> {
    let pair = list(runtime, [variable, hidden].into_iter())?;
    let bindings = list(runtime, [&pair].into_iter())?;
    let declarations = body
        .iter()
        .take_while(|form| is_declaration(runtime, form))
        .count();
    let result = call(runtime, "PROGN", body[declarations..].iter())?;
    let result = on_exit(result)?;
    let close = call(runtime, "CLOSE", [hidden].into_iter())?;
    let protect = call(runtime, "UNWIND-PROTECT", [&result, &close].into_iter())?;
    call(
        runtime,
        "LET",
        std::iter::once(&bindings)
            .chain(&body[..declarations])
            .chain(std::iter::once(&protect)),
    )
}

fn is_declaration(runtime: &Runtime, form: &Root<'_>) -> bool {
    runtime
        .heap
        .cons_fields(form.slot.get())
        .and_then(|(head, _)| name_of_word(runtime, head))
        .is_some_and(|name| name.operator() == "DECLARE")
}
