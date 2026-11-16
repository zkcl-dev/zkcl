//! Registration of declaration names introduced by PROCLAIM and DECLAIM.
//!
//! ANSI Common Lisp requires a symbol that names a declaration to be distinct
//! from a symbol naming a type, class, structure, or condition (CLHS TYPE).  The
//! set is global to the runtime, so it lives in the value cell of an internal
//! symbol instead of in the compiler instance.

use std::rc::Rc;

use crate::condition::{Class, Fault};
use crate::heap::Word;
use crate::symbol::{Name, Package};
use crate::{EvalError, Root, Runtime};

/// Internal CL symbol whose value is the list of declaration name symbols.
const REGISTRY: &str = "%ZK-DECLARATIONS%";

fn registry_symbol(runtime: &Runtime) -> Option<Word> {
    runtime.find_named_symbol(&Name::common_lisp(REGISTRY))
}

fn registered_names(runtime: &Runtime) -> Word {
    registry_symbol(runtime)
        .and_then(|registry| runtime.heap.symbol_cell(registry, false))
        .unwrap_or(Word::NIL)
}

/// Whether NAME has been proclaimed or declaimed as a declaration name.
pub(crate) fn is_declaration(runtime: &Runtime, name: Word) -> bool {
    let mut cursor = registered_names(runtime);
    while let Some((item, next)) = runtime.heap.cons_fields(cursor) {
        if item == name {
            return true;
        }
        cursor = next;
    }
    false
}

fn add(runtime: &Runtime, name: &Root<'_>, at: usize) -> Result<(), Fault> {
    let registry = match registry_symbol(runtime) {
        Some(word) => word,
        None => {
            runtime
                .intern_package(Package::COMMON_LISP, REGISTRY, at)
                .map_err(|error| Fault::plain(Class::ProgramError, error))?
                .0
        }
    };
    let registry = runtime.root(registry);
    let tail = runtime.root(registered_names(runtime));
    let cons = runtime
        .cons(name, &tail)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    runtime
        .heap
        .set_symbol_cell(registry.slot.get(), false, cons.slot.get());
    Ok(())
}

/// Whether NAME is already defined as a type, class, structure, or condition.
pub(crate) fn names_a_type(runtime: &Runtime, name: Word, at: usize) -> Result<bool, Fault> {
    if runtime.type_expander(name).is_some() || runtime.structure_definition(name).is_some() {
        return Ok(true);
    }
    if let Some(name) = Name::from_word(runtime, name)
        && crate::core_ir::CoreType::from_atomic_name(name.operator()).is_some()
    {
        return Ok(true);
    }
    Ok(crate::vm::classes::canonical_class(runtime, name, at)?.is_some())
}

/// Record NAME as a declaration name.
pub(crate) fn register(runtime: &Runtime, name: Word, at: usize) -> Result<(), Fault> {
    if !runtime.heap.is_symbol(name) {
        return Err(Fault::type_error(runtime, 
            name,
            "SYMBOL",
            EvalError::new(at, "a declaration name must be a symbol"),
        ));
    }
    if is_declaration(runtime, name) {
        return Ok(());
    }
    let name = runtime.root(name);
    if names_a_type(runtime, name.slot.get(), at)? {
        let spelling = runtime
            .heap
            .symbol_name(name.slot.get())
            .unwrap_or_default();
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(
                at,
                format!(
                    "cannot declare {spelling} as a declaration name: it already names a type, class, structure, or condition"
                ),
            ),
        ));
    }
    add(runtime, &name, at)
}

/// Signal an error when a defining form would give NAME a type-like meaning.
///
/// The caller is a macro expander, so this is reported as a macro expansion
/// error, which surfaces as a PROGRAM-ERROR from EVAL.
pub(crate) fn reject_declaration(
    runtime: &Runtime,
    name: Word,
    form: &str,
    at: usize,
) -> Result<(), EvalError> {
    if is_declaration(runtime, name) {
        let spelling = runtime.heap.symbol_name(name).unwrap_or_default();
        return Err(EvalError::macro_expansion(
            at,
            format!("{form} cannot define {spelling}: the name is a declaration name"),
        ));
    }
    Ok(())
}

/// Install the implementation's CLtL2 declaration protocol. A CLtL2 portability
/// package can expose these same operators without a second declaration registry.
pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    for (name, primitive) in [
        (
            "%REGISTER-DECLARATION-HANDLER",
            "ZK:%REGISTER-DECLARATION-HANDLER",
        ),
        ("DECLARATION-INFORMATION", "ZK:DECLARATION-INFORMATION"),
    ] {
        let symbol = runtime.intern_package(Package::ZK, name, 0)?.0;
        let function = crate::vm::Primitive::named(primitive).expect("declaration primitive");
        runtime.heap.set_symbol_cell(symbol, true, function);
    }
    runtime.load_builtin_source(include_str!(
        "../../lisp/runtime/compiler/define-declaration.lisp"
    ))
}

pub(crate) fn set_handler(
    runtime: &Runtime,
    symbol: Word,
    function: Word,
    at: usize,
) -> Result<Word, Fault> {
    let name = Name::from_word(runtime, symbol).ok_or_else(|| {
        Fault::type_error(runtime, 
            symbol,
            "SYMBOL",
            EvalError::new(at, "expected a declaration name"),
        )
    })?;
    if !crate::vm::is_function(runtime, function) {
        return Err(Fault::type_error(runtime, 
            function,
            "FUNCTION",
            EvalError::new(at, "expected a declaration handler"),
        ));
    }
    runtime
        .declaration_handlers
        .borrow_mut()
        .insert(name.key().clone(), runtime.root_slot(function));
    Ok(symbol)
}

pub(crate) fn information(
    runtime: &Runtime,
    symbol: Word,
    environment: Word,
    at: usize,
) -> Result<Word, Fault> {
    let name = Name::from_word(runtime, symbol).ok_or_else(|| {
        Fault::type_error(runtime, 
            symbol,
            "SYMBOL",
            EvalError::new(at, "expected a declaration name"),
        )
    })?;
    let id = crate::macros::validate_environment(runtime, environment, at)?;
    match name.operator() {
        "OPTIMIZE" => {
            let policy = id
                .and_then(|i| runtime.macro_environments.borrow()[&i].qualities)
                .unwrap_or_else(|| runtime.optimization_qualities.get());
            let mut list = runtime.nil();
            for (quality, value) in [
                ("COMPILATION-SPEED", policy.compilation_speed),
                ("SPACE", policy.space),
                ("DEBUG", policy.debug),
                ("SAFETY", policy.safety),
                ("SPEED", policy.speed),
            ] {
                let key = runtime.root(runtime.intern(&Name::common_lisp(quality), at)?);
                let level = runtime
                    .fixnum(isize::from(value))
                    .expect("optimization quality");
                let args = runtime
                    .cons(&level, &runtime.nil())
                    .map_err(|e| EvalError::out_of_memory(at, e))?;
                let pair = runtime
                    .cons(&key, &args)
                    .map_err(|e| EvalError::out_of_memory(at, e))?;
                list = runtime
                    .cons(&pair, &list)
                    .map_err(|e| EvalError::out_of_memory(at, e))?;
            }
            Ok(list.slot.get())
        }
        "DECLARATION" => {
            let mut list = runtime.nil();
            let mut tail = runtime.root(registered_names(runtime));
            while let Some((item, rest)) = runtime.heap.cons_fields(tail.slot.get()) {
                tail = runtime.root(rest);
                list = runtime
                    .cons(&runtime.root(item), &list)
                    .map_err(|e| EvalError::out_of_memory(at, e))?;
            }
            Ok(list.slot.get())
        }
        _ => {
            if let Some(value) = id.and_then(|i| {
                runtime.macro_environments.borrow()[&i]
                    .declarations
                    .iter()
                    .rev()
                    .find(|(key, _)| key == &name)
                    .map(|(_, value)| value.get())
            }) {
                return Ok(value);
            }
            if runtime
                .declaration_handlers
                .borrow()
                .contains_key(name.key())
            {
                return Ok(Word::NIL);
            }
            Err(Fault::plain(
                Class::SimpleError,
                EvalError::new(
                    at,
                    format!(
                        "unknown declaration for DECLARATION-INFORMATION: {}",
                        name.spelling()
                    ),
                ),
            ))
        }
    }
}

impl super::Compiler<'_> {
    pub(super) fn process_custom_declaration(
        &mut self,
        name: &Name,
        form: &crate::read::Form,
        policy: crate::core_ir::OptimizationQualities,
    ) -> Result<(), EvalError> {
        let runtime = self.runtime;
        let function = runtime.root(runtime.declaration_handlers.borrow()[name.key()].get());
        let spec = crate::form_value::to_value(runtime, form)?;
        let mut bindings = self.macro_environment(&|name| {
            super::type_intersection(self.current_var_types(name, false))
        });
        bindings.qualities = (policy != runtime.optimization_qualities.get()).then_some(policy);
        let invoke = |env: &crate::Root<'_>| {
            let args = [&spec, env];
            if let Some(call) = self.context.call_expander {
                call(
                    &function,
                    &args,
                    form.at,
                    crate::macros::ExpansionKind::Other,
                )
            } else {
                Ok(runtime
                    .call(&function, &args)?
                    .first()
                    .map_or(Word::NIL, |value| value.slot.get()))
            }
        };
        let result = if bindings.is_empty() {
            invoke(&runtime.nil())?
        } else {
            runtime.with_macro_environment(bindings, form.at, invoke)?
        };
        let (key, value) = runtime
            .heap
            .cons_fields(result)
            .and_then(|(key, value)| Name::from_word(runtime, key).map(|key| (key, value)))
            .ok_or_else(|| {
                EvalError::compile(
                    form.at,
                    "a :DECLARE handler must return (declaration-name . value)",
                )
            })?;
        self.custom_declarations
            .push((key, Rc::new(self.runtime.root_slot(value))));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;

    #[test]
    fn declaration_registration_survives_moving_collection() {
        let mut config = Heap::builder();
        config.stress_bytes = Some(1);
        config.always_defrag = true;
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let (symbol, _) = runtime
            .intern_package(Package::USER, "CUSTOM-NOTE", 0)
            .unwrap();
        let name = runtime.root(symbol);

        register(&runtime, name.slot.get(), 0).unwrap();

        assert!(is_declaration(&runtime, name.slot.get()));
    }
}
