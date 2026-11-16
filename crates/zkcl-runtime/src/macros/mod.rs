use std::mem::DropGuard;

use rustc_hash::FxHashMap;

use crate::form_value::name_of_word;
use crate::heap::{RootSlot, Word};
use crate::symbol::{Name, NameKey};
use crate::vm::{Arity, Instruction};
use crate::{EvalError, Root, Runtime};

mod standard;

pub(crate) use standard::extract_method_parameters;
pub(crate) use standard::places::get_setf_expansion;
pub(crate) use standard::{
    BUILTIN_INSTALLER_DESCRIPTORS, DEFINE_CONSTANT, INITIALIZE_SHARED_SLOT, RESIDUAL_MACRO_ERROR,
    builtin_natives, compiled_native, compiled_native_id, core_operator,
};

pub(crate) const EXPANSION_LIMIT: usize = 256;

/// A present entry without an expander shadows a global binding in that namespace.
#[derive(Default)]
pub(crate) struct MacroEnvironment {
    // Compiler keys are symbol identities or compiler-known names.
    pub(crate) operators: FxHashMap<NameKey, Option<RootSlot>>,
    pub(crate) setters: rustc_hash::FxHashSet<NameKey>,
    pub(crate) variables: FxHashMap<NameKey, Option<RootSlot>>,
    pub(crate) variable_types: FxHashMap<NameKey, crate::read::Form>,
    pub(crate) declarations: Vec<(Name, std::rc::Rc<RootSlot>)>,
    pub(crate) qualities: Option<crate::core_ir::OptimizationQualities>,
    pub(crate) file_constants: Option<crate::compile::FileConstants>,
}

impl MacroEnvironment {
    pub(crate) fn is_empty(&self) -> bool {
        self.operators.is_empty()
            && self.setters.is_empty()
            && self.variables.is_empty()
            && self.variable_types.is_empty()
            && self.declarations.is_empty()
            && self.qualities.is_none()
            && self
                .file_constants
                .as_ref()
                .is_none_or(|names| names.borrow().is_empty())
    }
}

impl Runtime {
    pub(crate) fn with_macro_environment<T>(
        &self,
        bindings: MacroEnvironment,
        at: usize,
        f: impl FnOnce(&Root<'_>) -> Result<T, EvalError>,
    ) -> Result<T, EvalError> {
        let id = self.next_macro_environment.get();
        let next = id.checked_add(1).ok_or_else(|| {
            EvalError::macro_expansion(at, "macro environment identities exhausted")
        })?;
        let environment = self
            .heap
            .macro_environment(id)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        let environment = self.root(environment);
        self.next_macro_environment.set(next);
        self.macro_environments.borrow_mut().insert(id, bindings);
        let _scope = DropGuard::new(id, |id| {
            self.macro_environments.borrow_mut().remove(&id);
        });
        f(&environment)
    }

    pub(crate) fn macro_function(
        &self,
        symbol: Word,
        environment: Word,
        at: usize,
    ) -> Result<Option<Root<'_>>, EvalError> {
        let Some(name) = name_of_word(self, symbol) else {
            validate_environment(self, environment, at)?;
            return Ok(None);
        };
        lookup_in_environment(self, &name, environment, at)
    }
}

pub(crate) fn local_operator_binding<'rt>(
    runtime: &'rt Runtime,
    name: &Name,
    environment: Word,
    at: usize,
) -> Result<Option<Option<Root<'rt>>>, EvalError> {
    if let Some(id) = validate_environment(runtime, environment, at)? {
        let environments = runtime.macro_environments.borrow();
        if let Some(binding) = environments[&id].operators.get(name.key()) {
            return Ok(Some(binding.as_ref().map(|slot| runtime.root(slot.get()))));
        }
    }
    Ok(None)
}

pub(crate) fn lookup_in_environment<'rt>(
    runtime: &'rt Runtime,
    name: &Name,
    environment: Word,
    at: usize,
) -> Result<Option<Root<'rt>>, EvalError> {
    if let Some(binding) = local_operator_binding(runtime, name, environment, at)? {
        return Ok(binding);
    }
    Ok(lookup(runtime, name))
}

pub(crate) fn local_setter_binding(
    runtime: &Runtime,
    name: &Name,
    environment: Word,
    at: usize,
) -> Result<bool, EvalError> {
    Ok(
        validate_environment(runtime, environment, at)?.is_some_and(|id| {
            runtime.macro_environments.borrow()[&id]
                .setters
                .contains(name.key())
        }),
    )
}

pub(crate) fn global_symbol_expander(runtime: &Runtime, name: &Name) -> Option<Word> {
    runtime
        .find_named_symbol(name)
        .and_then(|word| runtime.heap.symbol_macro_expander(word))
}

fn lookup_symbol_macro<'rt>(
    runtime: &'rt Runtime,
    name: &Name,
    environment: Word,
    at: usize,
) -> Result<Option<Root<'rt>>, EvalError> {
    if let Some(id) = validate_environment(runtime, environment, at)? {
        let environments = runtime.macro_environments.borrow();
        if let Some(binding) = environments[&id].variables.get(name.key()) {
            return Ok(binding.as_ref().map(|slot| runtime.root(slot.get())));
        }
    }
    Ok(global_symbol_expander(runtime, name).map(|word| runtime.root(word)))
}

/// Build the callable two-argument expander passed to *MACROEXPAND-HOOK*.
pub(crate) fn symbol_expander<'rt>(
    runtime: &'rt Runtime,
    expansion: &Root<'_>,
    at: usize,
) -> Result<Root<'rt>, EvalError> {
    let constants = runtime.root_frame(1);
    constants.set(0, expansion.slot.get());
    let (code_id, _) = runtime.symbol_expander_code.get_or_init(|| {
        let code_id = runtime
            .heap
            .code_store
            .insert(crate::code_store::CodeToInstall {
                name: None,
                source: None,
                debug: Default::default(),
                arity: Arity {
                    required: 2,
                    maximum: Some(2),
                },
                lambda_list: None,
                registers: 3,
                instructions: vec![Instruction::Constant(2, 0), Instruction::SetValues(2..3)],
                captures: Vec::new(),
                notinline: Vec::new(),
                tables: crate::code_store::CodeSideTables {
                    handlers: Vec::new(),
                    restarts: Vec::new(),
                    keywords: Vec::new(),
                    jump_tables: Vec::new(),
                    facts: Default::default(),
                },
                canonical_symbols: vec![None],
                native: None,
            });
        (code_id, runtime.heap.code_store.get(code_id))
    });
    runtime
        .heap
        .function(*code_id, &constants)
        .map(|word| runtime.root(word))
        .map_err(|error| EvalError::out_of_memory(at, error))
}

pub(crate) fn lookup<'rt>(runtime: &'rt Runtime, name: &Name) -> Option<Root<'rt>> {
    runtime
        .find_named_symbol(name)
        .and_then(|word| runtime.heap.macro_expander(word))
        .map(|word| runtime.root(word))
        .or_else(|| standard::expander(name.operator()).map(|word| runtime.root(word)))
}

pub(crate) fn validate_environment(
    runtime: &Runtime,
    environment: Word,
    at: usize,
) -> Result<Option<usize>, EvalError> {
    if environment == Word::NIL {
        return Ok(None);
    }
    let id = runtime
        .heap
        .macro_environment_id(environment)
        .ok_or_else(|| EvalError::macro_expansion(at, "expected a macro environment or NIL"))?;
    if !runtime.macro_environments.borrow().contains_key(&id) {
        return Err(EvalError::macro_expansion(
            at,
            "macro environment is no longer active",
        ));
    }
    Ok(Some(id))
}

pub(crate) struct ExpansionCall<'rt> {
    pub(crate) hook: Root<'rt>,
    pub(crate) expander: Root<'rt>,
}

pub(crate) fn prepare<'rt>(
    runtime: &'rt Runtime,
    form: &Root<'_>,
    environment: Word,
    at: usize,
) -> Result<Option<ExpansionCall<'rt>>, EvalError> {
    let word = form.slot.get();
    let expander = if let Some((head, _)) = runtime.heap.cons_fields(word) {
        runtime.macro_function(head, environment, at)?
    } else if let Some(name) = name_of_word(runtime, word) {
        lookup_symbol_macro(runtime, &name, environment, at)?
    } else {
        validate_environment(runtime, environment, at)?;
        None
    };
    let Some(expander) = expander else {
        return Ok(None);
    };
    let hook = macroexpand_hook(runtime, at)?;
    Ok(Some(ExpansionCall { hook, expander }))
}

fn macroexpand_hook(runtime: &Runtime, at: usize) -> Result<Root<'_>, EvalError> {
    let hook_symbol = runtime.intern_common_lisp("*MACROEXPAND-HOOK*", at)?;
    let hook = runtime
        .heap
        .symbol_cell(hook_symbol, false)
        .ok_or_else(|| EvalError::new(at, "unbound variable *MACROEXPAND-HOOK*"))?;
    Ok(runtime.root(hook))
}

#[derive(Clone, Copy)]
pub(crate) enum ExpansionKind {
    MacroForm,
    MacroFormInHandler,
    Other,
}

pub(crate) type CallExpander<'a> =
    dyn Fn(&Root<'_>, &[&Root<'_>], usize, ExpansionKind) -> Result<Word, EvalError> + 'a;

pub(crate) fn invoke_expander<'rt>(
    runtime: &'rt Runtime,
    expander: &Root<'_>,
    form: &Root<'_>,
    environment: &Root<'_>,
    at: usize,
    kind: ExpansionKind,
    call: Option<&CallExpander<'_>>,
) -> Result<Root<'rt>, EvalError> {
    let hook = macroexpand_hook(runtime, at)?;
    call_expander(
        runtime,
        &hook,
        &[expander, form, environment],
        at,
        kind,
        call,
    )
}

pub(crate) fn expand_once<'rt>(
    runtime: &'rt Runtime,
    form: &Root<'_>,
    environment: &Root<'_>,
) -> Result<Option<Root<'rt>>, EvalError> {
    expand_once_with(
        runtime,
        form,
        environment,
        0,
        ExpansionKind::MacroForm,
        None,
    )
}

pub(crate) fn expand_once_with<'rt>(
    runtime: &'rt Runtime,
    form: &Root<'_>,
    environment: &Root<'_>,
    at: usize,
    kind: ExpansionKind,
    call: Option<&CallExpander<'_>>,
) -> Result<Option<Root<'rt>>, EvalError> {
    let Some(expansion) = prepare(runtime, form, environment.slot.get(), at)? else {
        return Ok(None);
    };
    call_expander(
        runtime,
        &expansion.hook,
        &[&expansion.expander, form, environment],
        at,
        kind,
        call,
    )
    .map(Some)
}

fn call_expander<'rt>(
    runtime: &'rt Runtime,
    hook: &Root<'_>,
    args: &[&Root<'_>],
    at: usize,
    kind: ExpansionKind,
    call: Option<&CallExpander<'_>>,
) -> Result<Root<'rt>, EvalError> {
    if let Some(call) = call {
        let value = call(hook, args, at, kind)?;
        return Ok(runtime.root(value));
    }
    let results = runtime.call(hook, args)?;
    Ok(results.into_iter().next().unwrap_or_else(|| runtime.nil()))
}

pub(crate) fn expand_compiler_macro_once<'rt>(
    runtime: &'rt Runtime,
    expander: &Root<'_>,
    form: &Root<'_>,
    environment: &Root<'_>,
    at: usize,
    kind: ExpansionKind,
    call: Option<&CallExpander<'_>>,
) -> Result<Option<Root<'rt>>, EvalError> {
    let expanded = invoke_expander(runtime, expander, form, environment, at, kind, call)?;
    if expanded.same_identity(form) {
        Ok(None)
    } else {
        Ok(Some(expanded))
    }
}
