//! The pattern expander consumes the lexical declarations of its call site.
use crate::condition::Fault;
use crate::heap::Word;
use crate::symbol::{Name, Package};
use crate::{EvalError, NativeFunction, Root, Runtime};

mod bit_layout;
mod coverage;
#[cfg(feature = "lexical-patterns")]
mod lexical;

fn variable_info<'rt>(
    rt: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [symbol, env] = args else {
        return Err(EvalError::new(at, "expected a variable and macro environment").into());
    };
    let name = Name::from_word(rt, symbol.slot.get())
        .ok_or_else(|| EvalError::new(at, "expected a variable name"))?;
    let id = crate::macros::validate_environment(rt, env.slot.get(), at)?;
    let spec = id.and_then(|id| {
        rt.macro_environments.borrow()[&id]
            .variable_types
            .get(name.key())
            .cloned()
    });
    let initializer = id.and_then(|id| {
        rt.macro_environments.borrow()[&id]
            .file_constants
            .as_ref()
            .and_then(|names| names.borrow().get(name.key()).cloned())
    });
    let file_constant = initializer.is_some();
    let initial_form = match initializer {
        Some(form) => crate::form_value::to_value(rt, &form)?,
        None => rt.nil(),
    };
    let ty = match spec {
        Some(form) => crate::form_value::to_value(rt, &form)?,
        None => rt.root(Word::T),
    };
    let local_operator =
        crate::macros::local_operator_binding(rt, &name, env.slot.get(), at)?.is_some();
    Ok(vec![
        ty,
        rt.root(if rt.heap.is_special(symbol.slot.get()) {
            Word::T
        } else {
            Word::NIL
        }),
        rt.root(if local_operator { Word::T } else { Word::NIL }),
        rt.root(if file_constant { Word::T } else { Word::NIL }),
        initial_form,
    ])
}

fn load_compiler<'rt>(
    rt: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    if !args.is_empty() {
        return Err(EvalError::new(at, "expected no arguments").into());
    }
    rt.load_str(include_str!(
        "../../lisp/runtime/patterns/match-coverage.lisp"
    ))?;
    rt.load_str(include_str!(
        "../../lisp/runtime/patterns/match-compiler.lisp"
    ))?;
    rt.load_str(include_str!(
        "../../lisp/runtime/patterns/match-structural.lisp"
    ))?;
    #[cfg(feature = "lexical-patterns")]
    rt.load_str(include_str!(
        "../../lisp/runtime/patterns/lexical-compiler.lisp"
    ))?;
    Ok(vec![rt.root(Word::T)])
}

pub(crate) fn initialize(rt: &Runtime) -> Result<(), EvalError> {
    #[cfg(feature = "lexical-patterns")]
    lexical::initialize(rt)?;
    for (name, callback) in [
        (
            "%PATTERN-VARIABLE-INFO",
            std::sync::Arc::new(variable_info) as std::sync::Arc<NativeFunction>,
        ),
        ("%LOAD-MATCH-COMPILER", std::sync::Arc::new(load_compiler)),
        ("%BIT-INTEGER", std::sync::Arc::new(bit_layout::integer)),
        ("%PATTERN-COVERAGE-KEY", std::sync::Arc::new(coverage::key)),
    ] {
        let sym = rt.intern_package(Package::ZK, name, 0)?.0;
        let mut natives = rt.natives.borrow_mut();
        let index = natives.len();
        natives.push(callback);
        rt.heap.set_symbol_cell(sym, true, Word::native(index));
    }
    for (name, primitive) in [
        ("%PATTERN-TAG", "ZK::%PATTERN-TAG"),
        ("%PATTERN-FIELD", "ZK::%PATTERN-FIELD"),
        ("%PATTERN-LOOKUP", "ZK::%PATTERN-LOOKUP"),
    ] {
        let symbol = rt.intern_package(Package::ZK, name, 0)?.0;
        let function =
            crate::vm::Primitive::named(primitive).expect("registered pattern primitive");
        rt.heap.set_symbol_cell(symbol, true, function);
    }
    rt.load_builtin_source(include_str!("../../lisp/runtime/patterns/patterns.lisp"))?;
    #[cfg(feature = "lexical-patterns")]
    rt.load_builtin_source(concat!(
        "(in-package #:zk-pattern-compiler)\n",
        include_str!("../../lisp/runtime/patterns/lexical.lisp"),
        "\n(in-package #:cl-user)"
    ))?;
    Ok(())
}
