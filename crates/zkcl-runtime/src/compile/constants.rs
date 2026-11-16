//! Runtime-scoped tracking and installation for `DEFCONSTANT` symbols.

use crate::condition::{Class, Fault};
use crate::heap::{HashTest, Word};
use crate::symbol::{Name, Package};
use crate::vm::equality;
use crate::{EvalError, Root, Runtime};

/// Whether `symbol` is a constant variable (built-in, keyword, or `DEFCONSTANT`).
pub(crate) fn is_constant(runtime: &Runtime, symbol: Word) -> bool {
    runtime
        .heap
        .symbol_attributes(symbol)
        .is_some_and(|attrs| attrs.is_constant_variable())
}

/// Whether `name` denotes a constant variable in `runtime`.
pub(crate) fn is_constant_name(runtime: &Runtime, name: &Name) -> bool {
    name.constant()
        || runtime
            .find_named_symbol(name)
            .is_some_and(|word| is_constant(runtime, word))
}

/// Whether `name` is a built-in constant (keyword, `T`, `NIL`, or canonical CL constant)
/// that cannot be redefined with `DEFCONSTANT`.
pub(crate) fn is_canonical_constant(runtime: &Runtime, name: &Name) -> bool {
    name.package() == Some(Package::KEYWORD)
        || matches!(name.operator(), "T" | "NIL")
        || runtime
            .find_named_symbol(name)
            .and_then(|word| runtime.heap.symbol_attributes(word))
            .is_some_and(|data| data.canonical && data.constant)
}

/// Installer invoked by compiled `DEFCONSTANT` forms (both `EVAL` and FASL `LOAD`).
pub(crate) fn define_constant<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [symbol_root, value_root, doc_root] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(
                at,
                "DEFCONSTANT installer expects symbol, value, and documentation",
            ),
        ));
    };
    let symbol = symbol_root.slot.get();
    let Some(data) = runtime.heap.symbol_attributes(symbol) else {
        return Err(Fault::type_error(runtime, 
            symbol,
            "SYMBOL",
            EvalError::new(at, "DEFCONSTANT requires a symbol"),
        ));
    };
    let spelling = runtime
        .heap
        .symbol_name(symbol)
        .unwrap_or_else(|| "constant".to_owned());
    if data.home == Some(Package::KEYWORD) || (data.canonical && data.constant) {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, format!("cannot redefine built-in constant {spelling}")),
        ));
    }
    if runtime.heap.symbol_macro_expander(symbol).is_some() {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(
                at,
                format!("cannot define {spelling} as a constant: it names a symbol macro"),
            ),
        ));
    }
    if (data.constant || data.defined_constant)
        && let Some(existing) = runtime.heap.symbol_cell(symbol, false)
        && !equality::matches(
            &runtime.heap,
            existing,
            value_root.slot.get(),
            HashTest::Eql,
        )
    {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(
                at,
                format!("cannot redefine constant {spelling} with a non-EQL value"),
            ),
        ));
    }
    let doc = doc_root.slot.get();
    if doc != Word::NIL && !runtime.heap.is_string(doc) {
        return Err(Fault::type_error(runtime, 
            doc,
            "STRING",
            EvalError::new(at, "DEFCONSTANT documentation must be a string"),
        ));
    }

    let symbol = symbol_root.slot.get();
    runtime.heap.proclaim_constant(symbol);
    let value = value_root.slot.get();
    let doc = doc_root.slot.get();
    runtime.heap.set_symbol_cell(symbol, false, value);
    if doc != Word::NIL {
        runtime.heap.set_variable_documentation(symbol, doc);
    }
    Ok(vec![runtime.root(symbol)])
}
