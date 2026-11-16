use std::rc::Rc;

use super::{Form, FormKind};
use crate::form_value;
use crate::heap::Word;
use crate::package::Package;
use crate::{EvalError, Runtime};

pub(super) fn read_structure(
    runtime: &Runtime,
    form: &Form,
    calls: Option<&super::ReadCall<'_>>,
) -> Result<FormKind, EvalError> {
    let Some([name, arguments @ ..]) = form.list() else {
        return Err(EvalError::read(
            form.at,
            "#S requires a structure name and slots",
        ));
    };
    let name = name
        .name()
        .ok_or_else(|| EvalError::read(name.at, "#S requires a structure name symbol"))?;
    let name = runtime.intern(name, form.at)?;
    let definition = runtime
        .structure_definition(name)
        .ok_or_else(|| EvalError::read(form.at, "undefined structure type in #S"))?;
    let constructor = definition.keyword_constructor.get();
    if constructor == Word::NIL {
        return Err(EvalError::read(
            form.at,
            "#S requires a keyword constructor",
        ));
    }
    let constructor = runtime.root(constructor);
    let (pairs, rest) = arguments.as_chunks::<2>();
    if let [missing] = rest {
        return Err(EvalError::read(missing.at, "#S slot is missing its value"));
    }
    let mut args = Vec::with_capacity(arguments.len());
    for [slot, value] in pairs {
        let designator = form_value::to_value(runtime, slot)?;
        let name = runtime
            .string_designator_text(designator.slot.get())
            .ok_or_else(|| EvalError::read(slot.at, "#S slot name must be a string designator"))?;
        let (keyword, _) = runtime.intern_package(Package::KEYWORD, &name, slot.at)?;
        args.push(runtime.root(keyword));
        args.push(form_value::to_value(runtime, value)?);
    }
    let references = args.iter().collect::<Vec<_>>();
    let value = super::reader_call(runtime, calls, &constructor, &references, None, form.at)?
        .into_iter()
        .next()
        .unwrap_or_else(|| runtime.nil());
    Ok(FormKind::Literal(Rc::new(value.slot)))
}
