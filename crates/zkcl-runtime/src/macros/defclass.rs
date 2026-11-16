use std::collections::HashSet;

use super::{
    Expansion, INITIALIZE_SHARED_SLOT, REGISTER_CLASS, call, fresh, invalid, list, native_call,
    proper_list, quote, symbol,
};
use crate::classes::{self, RuntimeClassKind};
use crate::condition::{self, Class, Fault};
use crate::heap::Word;
use crate::package::Package;
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime};

pub(super) fn expand<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_class(runtime, forms, RuntimeClassKind::Standard, "DEFCLASS")
}

pub(super) fn expand_condition<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    expand_class(
        runtime,
        forms,
        RuntimeClassKind::Condition,
        "DEFINE-CONDITION",
    )
}

fn expand_class<'rt>(
    runtime: &'rt Runtime,
    forms: &[Root<'rt>],
    kind: RuntimeClassKind,
    macro_name: &str,
) -> Expansion<'rt> {
    let [name, superclasses, slots, options @ ..] = forms else {
        return Err(invalid(
            macro_name,
            "expects a name, superclass list and slot list",
        ));
    };
    let mut seen = HashSet::new();
    expect_name(runtime, name, macro_name)?;
    for parent in proper_list(runtime, superclasses.clone(), macro_name)? {
        let id = expect_name(runtime, &parent, macro_name)?;
        if !seen.insert(id) {
            return Err(invalid(macro_name, "repeats a direct superclass"));
        }
    }
    let superclasses = quote(runtime, superclasses)?;
    let mut extra_options = Vec::new();
    let mut slot_forms = Vec::new();
    seen.clear();
    for slot in proper_list(runtime, slots.clone(), macro_name)? {
        let elements = if runtime.heap.is_symbol(slot.slot.get()) {
            vec![slot]
        } else {
            proper_list(runtime, slot, macro_name)?
        };
        let Some((slot, options)) = elements.split_first() else {
            return Err(invalid(macro_name, "expects a slot name"));
        };
        let id = expect_name(runtime, slot, macro_name)?;
        if slot.slot.get() == Word::T
            || runtime.heap.symbol_home(slot.slot.get()) == Some(Package::KEYWORD)
        {
            return Err(invalid(
                macro_name,
                "expects a slot name valid as a variable",
            ));
        }
        if !seen.insert(id) {
            return Err(invalid(macro_name, "repeats a direct slot"));
        }
        let spec = slot_form(runtime, slot, options, macro_name)?;
        slot_forms.push(spec);
    }
    let slots = call(runtime, "LIST", slot_forms.iter())?;

    let mut seen = HashSet::new();
    let mut defaults = runtime.nil();
    let mut documentation = runtime.nil();
    let mut report = runtime.nil();
    let mut metaclass = runtime.nil();
    for option in options {
        let (key, tail) = runtime
            .cons_fields(option)
            .ok_or_else(|| invalid(macro_name, "expects a class option"))?;
        let key_id = runtime
            .heap
            .symbol_attributes(key.slot.get())
            .ok_or_else(|| invalid(macro_name, "expected a class option name"))?
            .id;
        if !seen.insert(key_id) {
            return Err(invalid(macro_name, "repeats a class option"));
        }
        let spelling = super::keyword_name_opt(runtime, &key);
        let extension = !matches!(
            spelling.as_deref(),
            Some("DEFAULT-INITARGS" | "DOCUMENTATION" | "METACLASS")
        );
        if kind == RuntimeClassKind::Standard && extension {
            // AMOP passes the cdr verbatim. ContextL uses both lists and dotted options.
            extra_options.push(quote(runtime, &key)?);
            extra_options.push(quote(runtime, &tail)?);
            continue;
        }
        let args = proper_list(runtime, tail, macro_name)?;
        match (spelling.as_deref(), args.as_slice()) {
            (Some("DEFAULT-INITARGS"), args) => {
                defaults = default_initargs(runtime, args, macro_name)?
            }
            (Some("DOCUMENTATION"), [doc]) if runtime.heap.is_string(doc.slot.get()) => {
                documentation = doc.clone()
            }
            (Some("METACLASS"), [class]) if kind == RuntimeClassKind::Standard => {
                expect_name(runtime, class, macro_name)?;
                metaclass = quote(runtime, class)?;
            }
            (Some("REPORT"), [report_name]) if kind == RuntimeClassKind::Condition => {
                report = report_form(runtime, report_name, macro_name)?;
            }
            _ => return Err(invalid(macro_name, "invalid class option")),
        }
    }
    let quoted_name = quote(runtime, name)?;
    let condition_flag = runtime.root(if kind == RuntimeClassKind::Condition {
        Word::T
    } else {
        Word::NIL
    });
    let active = runtime.root(Word::T);
    let provisional = runtime.nil();
    let register = |phase: &Root<'rt>, report: &Root<'rt>| {
        native_call(
            runtime,
            REGISTER_CLASS,
            [
                &condition_flag,
                phase,
                &quoted_name,
                &superclasses,
                &slots,
                &defaults,
                &documentation,
                report,
                &metaclass,
            ],
        )
    };
    let compile_definition = register(&provisional, &provisional)?;
    let compile = eval_when(runtime, &["COMPILE-TOPLEVEL"], &[compile_definition])?;
    let keyword = |name| {
        runtime
            .intern(&Name::keyword(name), 0)
            .map(|w| runtime.root(w))
    };
    let body = if kind == RuntimeClassKind::Condition {
        let definition = register(&active, &report)?;
        let class = call(runtime, "FIND-CLASS", [&quoted_name].into_iter())?;
        let accessors = runtime.root(
            runtime
                .intern_package(Package::MOP, "%INSTALL-ACCESSORS", 0)?
                .0,
        );
        let mut body = vec![
            initialize_shared_slots(runtime, definition)?,
            list(runtime, [&accessors, &class].into_iter())?,
        ];
        if !report.is_nil() {
            let object = fresh(runtime, "CONDITION")?;
            let stream = fresh(runtime, "STREAM")?;
            let specialized = list(runtime, [&object, name].into_iter())?;
            let parameters = list(runtime, [&specialized, &stream].into_iter())?;
            let emit = if runtime.heap.is_string(report.slot.get()) {
                call(runtime, "WRITE-STRING", [&report, &stream].into_iter())?
            } else {
                call(runtime, "FUNCALL", [&report, &object, &stream].into_iter())?
            };
            let escape = symbol(runtime, "*PRINT-ESCAPE*")?;
            let readably = symbol(runtime, "*PRINT-READABLY*")?;
            let escaped = call(runtime, "OR", [&escape, &readably].into_iter())?;
            let next = call(runtime, "CALL-NEXT-METHOD", [].into_iter())?;
            let dispatch = call(runtime, "IF", [&escaped, &next, &emit].into_iter())?;
            let print_object = symbol(runtime, "PRINT-OBJECT")?;
            body.push(call(
                runtime,
                "DEFMETHOD",
                [&print_object, &parameters, &dispatch].into_iter(),
            )?);
        }
        body.push(quoted_name);
        body
    } else {
        let ensure = runtime.root(runtime.intern_package(Package::MOP, "ENSURE-CLASS", 0)?.0);
        let mut arguments = vec![
            ensure,
            quoted_name,
            keyword("DIRECT-SUPERCLASSES")?,
            superclasses,
            keyword("DIRECT-SLOTS")?,
            slots,
            keyword("DIRECT-DEFAULT-INITARGS")?,
            defaults,
            keyword("DOCUMENTATION")?,
            documentation,
        ];
        if !metaclass.is_nil() {
            arguments.extend([keyword("METACLASS")?, metaclass]);
        }
        arguments.extend(extra_options);
        vec![list(runtime, arguments.iter())?]
    };
    let load = eval_when(runtime, &["LOAD-TOPLEVEL", "EXECUTE"], &body)?;
    call(runtime, "PROGN", [&compile, &load].into_iter())
}

fn initialize_shared_slots<'rt>(runtime: &'rt Runtime, definition: Root<'rt>) -> Expansion<'rt> {
    let finish = runtime.root(
        runtime
            .intern_package(Package::MOP, "%FINISH-CLASS-CHANGE", 0)?
            .0,
    );
    let finish = call(runtime, "FUNCTION", [&finish].into_iter())?;
    let definition = call(
        runtime,
        "MULTIPLE-VALUE-CALL",
        [&finish, &definition].into_iter(),
    )?;
    let slot = fresh(runtime, "SHARED-SLOT")?;
    let binding = list(runtime, [&slot, &definition].into_iter())?;
    let class = call(runtime, "FIRST", [&slot].into_iter())?;
    let name = call(runtime, "SECOND", [&slot].into_iter())?;
    let cell = call(runtime, "THIRD", [&slot].into_iter())?;
    let initform = call(runtime, "FOURTH", [&slot].into_iter())?;
    let value = call(runtime, "FUNCALL", [&initform].into_iter())?;
    let body = native_call(
        runtime,
        INITIALIZE_SHARED_SLOT,
        [&class, &name, &cell, &value],
    )?;
    call(runtime, "DOLIST", [&binding, &body].into_iter())
}

fn slot_form<'rt>(
    runtime: &'rt Runtime,
    name: &Root<'rt>,
    options: &[Root<'rt>],
    macro_name: &str,
) -> Expansion<'rt> {
    let (pairs, remainder) = options.as_chunks::<2>();
    let mut seen = HashSet::new();
    let mut initargs = Vec::new();
    let mut initform = runtime.nil();
    let mut initform_source = runtime.nil();
    let mut readers = Vec::new();
    let mut writers = Vec::new();
    let mut allocation = None;
    let mut specifier = None;
    let mut documentation = runtime.nil();
    let mut extensions: Vec<(&Root<'rt>, Vec<&Root<'rt>>)> = Vec::new();
    for [key, value] in pairs {
        if !runtime.heap.is_symbol(key.slot.get()) {
            return Err(invalid(macro_name, "expected a slot option name"));
        }
        let keyword = super::keyword_name_opt(runtime, key).unwrap_or_default();
        let repeatable = match keyword.as_str() {
            "READER" | "ACCESSOR" => {
                expect_name(runtime, value, macro_name)?;
                readers.push(value.clone());
                if keyword == "ACCESSOR" {
                    let setf = call(runtime, "SETF", [value].into_iter())?;
                    writers.push(setf);
                }
                true
            }
            "WRITER" => {
                if !runtime.heap.is_symbol(value.slot.get()) {
                    let writer = proper_list(runtime, value.clone(), macro_name)?;
                    if !matches!(writer.as_slice(), [operator, name]
                        if condition::named_word(runtime, operator.slot.get(), Package::COMMON_LISP)
                            .is_some_and(|name| name == "SETF") && expect_name(runtime, name, macro_name).is_ok())
                    {
                        return Err(invalid(macro_name, "expects a writer function name"));
                    }
                } else {
                    expect_name(runtime, value, macro_name)?;
                }
                writers.push(value.clone());
                true
            }
            "INITARG" => {
                if !runtime.heap.is_symbol(value.slot.get()) {
                    return Err(invalid(macro_name, "expects a symbol as an :initarg name"));
                }
                initargs.push(value);
                true
            }
            "INITFORM" => {
                initform = thunk(runtime, value)?;
                initform_source = quote(runtime, value)?;
                false
            }
            "ALLOCATION" => {
                if !runtime.heap.is_symbol(value.slot.get())
                    || value.is_nil()
                    || macro_name != "DEFCLASS"
                        && !matches!(
                            super::keyword_name_opt(runtime, value).as_deref(),
                            Some("CLASS" | "INSTANCE")
                        )
                {
                    return Err(invalid(macro_name, "invalid slot allocation"));
                }
                allocation = Some(quote(runtime, value)?);
                false
            }
            "TYPE" => {
                specifier = Some(quote(runtime, value)?);
                false
            }
            "DOCUMENTATION" if runtime.heap.is_string(value.slot.get()) => {
                documentation = value.clone();
                false
            }
            _ if macro_name == "DEFCLASS" && keyword != "DOCUMENTATION" => {
                if let Some((_, values)) = extensions
                    .iter_mut()
                    .find(|(k, _)| k.slot.get() == key.slot.get())
                {
                    values.push(value);
                } else {
                    extensions.push((key, vec![value]));
                }
                true
            }
            _ => {
                return Err(invalid(
                    macro_name,
                    &format!("invalid slot option :{keyword}"),
                ));
            }
        };
        if !repeatable && !seen.insert(keyword) {
            return Err(invalid(macro_name, "repeats a slot option"));
        }
    }
    if !remainder.is_empty() {
        return Err(invalid(macro_name, "expects slot option/value pairs"));
    }
    let keyword = |name| {
        runtime
            .intern(&Name::keyword(name), 0)
            .map(|w| runtime.root(w))
    };
    let mut fields = vec![keyword("NAME")?, quote(runtime, name)?];
    for (key, value) in [
        (
            "INITARGS",
            (!initargs.is_empty())
                .then(|| list(runtime, initargs.into_iter()))
                .transpose()?,
        ),
        (
            "READERS",
            (!readers.is_empty())
                .then(|| list(runtime, readers.iter()))
                .transpose()?,
        ),
        (
            "WRITERS",
            (!writers.is_empty())
                .then(|| list(runtime, writers.iter()))
                .transpose()?,
        ),
    ] {
        if let Some(value) = value {
            fields.extend([keyword(key)?, quote(runtime, &value)?]);
        }
    }
    // AMOP permits a false initfunction for an omitted source initializer.
    // A slot metaclass can then amend its reflected form without accidentally
    // introducing an unpaired initarg. An explicit :INITFORM NIL has a thunk.
    fields.extend([
        keyword("INITFORM")?,
        initform_source,
        keyword("INITFUNCTION")?,
        initform,
    ]);
    if let Some(value) = allocation {
        fields.extend([keyword("ALLOCATION")?, value]);
    }
    if let Some(value) = specifier {
        fields.extend([keyword("TYPE")?, value]);
    }
    if !documentation.is_nil() {
        fields.extend([keyword("DOCUMENTATION")?, documentation]);
    }
    for (key, values) in extensions {
        let value = if let [value] = values.as_slice() {
            (*value).clone()
        } else {
            list(runtime, values.into_iter())?
        };
        fields.extend([quote(runtime, key)?, quote(runtime, &value)?]);
    }
    call(runtime, "LIST", fields.iter())
}

fn default_initargs<'rt>(
    runtime: &'rt Runtime,
    arguments: &[Root<'rt>],
    macro_name: &str,
) -> Expansion<'rt> {
    let mut names = HashSet::new();
    let mut forms = Vec::new();
    let (pairs, remainder) = arguments.as_chunks::<2>();
    for [name, form] in pairs {
        let id = runtime
            .heap
            .symbol_attributes(name.slot.get())
            .ok_or_else(|| invalid(macro_name, "expects a symbol as a default initarg name"))?
            .id;
        if !names.insert(id) {
            return Err(invalid(macro_name, "repeats a default initarg"));
        }
        let name = quote(runtime, name)?;
        let initform = thunk(runtime, form)?;
        let initform_source = quote(runtime, form)?;
        forms.push(call(
            runtime,
            "LIST",
            [&name, &initform_source, &initform].into_iter(),
        )?);
    }
    if !remainder.is_empty() {
        return Err(invalid(macro_name, "expects default initarg/value pairs"));
    }
    call(runtime, "LIST", forms.iter())
}

fn report_form<'rt>(runtime: &'rt Runtime, report: &Root<'rt>, macro_name: &str) -> Expansion<'rt> {
    if runtime.heap.is_string(report.slot.get()) {
        return Ok(report.clone());
    }
    let lambda = runtime
        .heap
        .cons_fields(report.slot.get())
        .is_some_and(|(operator, _)| {
            condition::named_word(runtime, operator, Package::COMMON_LISP)
                .is_some_and(|name| name == "LAMBDA")
        });
    if !lambda && !runtime.heap.is_symbol(report.slot.get()) {
        return Err(invalid(
            macro_name,
            "expects a report string, function name or lambda expression",
        ));
    }
    call(runtime, "FUNCTION", [report].into_iter())
}

fn expect_name(runtime: &Runtime, name: &Root<'_>, macro_name: &str) -> Result<usize, EvalError> {
    runtime
        .heap
        .symbol_attributes(name.slot.get())
        .filter(|_| !name.is_nil())
        .map(|symbol| symbol.id)
        .ok_or_else(|| invalid(macro_name, "expects a non-NIL symbol name"))
}

fn thunk<'rt>(runtime: &'rt Runtime, form: &Root<'rt>) -> Expansion<'rt> {
    let lambda = call(runtime, "LAMBDA", [&runtime.nil(), form].into_iter())?;
    call(runtime, "FUNCTION", [&lambda].into_iter())
}

fn eval_when<'rt>(
    runtime: &'rt Runtime,
    situations: &[&str],
    body: &[Root<'rt>],
) -> Expansion<'rt> {
    let keyword = |name: &&str| -> Expansion<'rt> {
        runtime
            .intern(&Name::keyword(name), 0)
            .map(|value| runtime.root(value))
    };
    let situations = situations
        .iter()
        .map(keyword)
        .collect::<Result<Vec<_>, _>>()?;
    let situations = list(runtime, situations.iter())?;
    let operator = symbol(runtime, "EVAL-WHEN")?;
    list(runtime, [&operator, &situations].into_iter().chain(body))
}

pub(super) fn initialize_shared_slot<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let invalid = || {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected a shared slot initializer"),
        )
    };
    let [class, slot, cell, value] = args else {
        return Err(invalid());
    };
    classes::initialize_shared_slot(
        runtime,
        class.slot.get(),
        slot.slot.get(),
        cell.slot.get(),
        value.slot.get(),
    )
    .ok_or_else(invalid)?;
    Ok(Vec::new())
}
