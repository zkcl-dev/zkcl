use std::ops::Range;

use super::{
    Continuation, ContinuationStep, ExecState, Invocation, Resume, StepControl, Values, accept,
    callback, check_definable_name, dispatch_error, evaluate, function_cell, is_function,
    undefined_function,
};
use crate::compile;
use crate::condition::{Class, Fault};
use crate::form_value;
use crate::heap::{RootSlot, RootSlots, Word};
use crate::macros;
use crate::symbol::{Name, Package};
use crate::{EvalError, EvalErrorKind, Runtime};

pub(super) fn start<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let name = frame.get(args.start);
    let function_name = FunctionName::parse(runtime, name, at)?;
    let is_macro = match function_name {
        FunctionName::Symbol(symbol) => runtime.macro_function(symbol, Word::NIL, at)?.is_some(),
        FunctionName::Setf(_) => false,
    };
    let result = if args.len() == 1 {
        if name == Word::NIL {
            return Err(program_error(at, "COMPILE with NIL requires a definition"));
        }
        // This runtime's existing function and macro cells contain compiled code.
        match function_name {
            FunctionName::Symbol(symbol) if !is_macro => {
                function_cell(runtime, symbol, at)?;
            }
            FunctionName::Setf(_) => {
                function_name.definition(runtime, name, at)?;
            }
            FunctionName::Symbol(_) => {}
        }
        name
    } else {
        let definition = frame.get(args.start + 1);
        if !is_function(runtime, definition) {
            return Ok(Invocation::Compile(Box::new(CompileDefinition {
                name: runtime.root_slot(name),
                definition: runtime.root_slot(definition),
                is_macro,
                resume,
                at,
            })));
        }
        install_compiled(runtime, name, is_macro, definition, at)?
    };
    Ok(compile_succeeded(values, result, resume))
}

fn compile_succeeded<'rt>(values: &mut Values, result: Word, resume: Resume) -> Invocation<'rt> {
    values.resize(3);
    values.slots.set(0, result);
    values.slots.set(1, Word::NIL);
    values.slots.set(2, Word::NIL);
    Invocation::Done(resume)
}

pub(super) struct CompileDefinition {
    name: RootSlot,
    definition: RootSlot,
    is_macro: bool,
    resume: Resume,
    at: usize,
}

pub(super) fn dispatch<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    request: CompileDefinition,
) -> Result<StepControl<'rt>, EvalError> {
    let (callable, warnings) = match callback::run(runtime, exec, |call, _| {
        compile_definition_with(runtime, request.definition.get(), request.at, call)
    }) {
        callback::Outcome::Completed(Ok(compiled)) => compiled,
        callback::Outcome::Completed(Err(fault)) => {
            return dispatch_error(runtime, exec, fault);
        }
        callback::Outcome::Unwind(target) => return Ok(StepControl::Unwind(target)),
        callback::Outcome::Unhandled(error) => return Err(error),
    };
    let CompileDefinition {
        name,
        is_macro,
        resume,
        at,
        ..
    } = request;
    if !warnings.is_empty() {
        return accept(
            runtime,
            exec,
            Invocation::Continuation(Box::new(CompileContinuation {
                name,
                callable: runtime.root_slot(callable),
                is_macro,
                warnings: warnings.into_iter(),
                resume,
                at,
            })),
        );
    }
    let result = match install_compiled(runtime, name.get(), is_macro, callable, at) {
        Ok(result) => result,
        Err(fault) => return dispatch_error(runtime, exec, fault),
    };
    let success = compile_succeeded(&mut exec.values, result, resume);
    accept(runtime, exec, success)
}

struct CompileContinuation {
    name: RootSlot,
    callable: RootSlot,
    is_macro: bool,
    warnings: std::vec::IntoIter<compile::Warning>,
    resume: Resume,
    at: usize,
}

impl Continuation for CompileContinuation {
    fn step(
        &mut self,
        runtime: &Runtime,
        _values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if let Some(warning) = self.warnings.next() {
            return evaluate::signal_compiler_warning(runtime, &warning);
        }
        let result = install_compiled(
            runtime,
            self.name.get(),
            self.is_macro,
            self.callable.get(),
            self.at,
        )?;
        Ok(ContinuationStep::DoneValues(vec![result, Word::T, Word::T]))
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

pub(super) fn fmakunbound(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    let function_name = FunctionName::parse(runtime, value, at)?;
    let symbol = match function_name {
        FunctionName::Symbol(symbol) | FunctionName::Setf(symbol) => symbol,
    };
    check_definable_name(runtime, symbol, at)?;
    match function_name {
        FunctionName::Symbol(symbol) => {
            runtime.heap.set_symbol_cell(symbol, true, Word::UNBOUND);
            if let Some(name) = Name::from_word(runtime, symbol) {
                runtime.clear_inline_def(&name);
            }
        }
        FunctionName::Setf(accessor) => runtime.heap.set_symbol_setter(accessor, Word::UNBOUND),
    }
    Ok(value)
}

pub(super) fn function_lambda_expression(
    runtime: &Runtime,
    function: Word,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    if !is_function(runtime, function) {
        return Err(Fault::type_error(runtime, 
            function,
            "FUNCTION",
            EvalError::new(at, "expected a function"),
        ));
    }
    let closure_p = if runtime
        .heap
        .function_code(function)
        .is_some_and(|code| code != function)
    {
        Word::T
    } else {
        Word::NIL
    };
    values.resize(3);
    let metadata = runtime.heap.function_code(function).and_then(|code| {
        let info = runtime.heap.function_info(code)?;
        let debug = &runtime.heap.code_store.get(info.code_id).debug;
        Some((
            debug
                .source_expression
                .map(|index| runtime.heap.function_constant(code, index as usize)),
            debug
                .function_name
                .map(|index| runtime.heap.function_constant(code, index as usize)),
        ))
    });
    values
        .slots
        .set(0, metadata.and_then(|value| value.0).unwrap_or(Word::NIL));
    values.slots.set(1, closure_p);
    let name = metadata
        .and_then(|value| value.1)
        .or_else(|| {
            runtime
                .heap
                .generic_info(function)
                .map(|generic| generic.name)
        })
        .or_else(|| {
            super::Primitive::compiled_name(function).and_then(|name| {
                let name = if name.contains(':') {
                    Name::read(runtime, name, at).ok()?
                } else {
                    Name::common_lisp(name)
                };
                runtime.find_named_symbol(&name)
            })
        })
        .unwrap_or(Word::NIL);
    values.slots.set(2, name);
    Ok(())
}

pub(crate) fn initialize_function_introspection(runtime: &Runtime) -> Result<(), EvalError> {
    let name = "FUNCTION-LAMBDA-LIST";
    let symbol = runtime.intern_package(Package::ZK, name, 0)?.0;
    let function = super::Primitive::named("ZK:FUNCTION-LAMBDA-LIST")
        .expect("registered function introspection primitive");
    runtime.heap.set_symbol_cell(symbol, true, function);
    runtime
        .packages
        .borrow_mut()
        .export(Package::ZK, name, symbol)
        .map_err(|error| EvalError::from_error(0, error))?;
    Ok(())
}

/// ZK:FUNCTION-LAMBDA-LIST returns read-only source syntax and an availability flag.
pub(super) fn function_lambda_list(
    runtime: &Runtime,
    function: Word,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    if !is_function(runtime, function) {
        return Err(Fault::type_error(runtime, 
            function,
            "FUNCTION",
            EvalError::new(at, "expected a function"),
        ));
    }
    let lambda_list = lambda_list_word(runtime, function);
    values.resize(2);
    values.slots.set(0, lambda_list.unwrap_or(Word::NIL));
    values.slots.set(
        1,
        if lambda_list.is_some() {
            Word::T
        } else {
            Word::NIL
        },
    );
    Ok(())
}

pub(crate) fn lambda_list_word(runtime: &Runtime, function: Word) -> Option<Word> {
    if let Some(info) = runtime.heap.generic_info(function) {
        (info.lambda_list != Word::UNBOUND).then_some(info.lambda_list)
    } else {
        runtime.heap.function_code(function).and_then(|code| {
            let info = runtime.heap.function_info(code).expect("function code");
            runtime
                .heap
                .code_store
                .get(info.code_id)
                .lambda_list
                .map(|index| runtime.heap.function_constant(code, index as usize))
        })
    }
}

fn install_compiled(
    runtime: &Runtime,
    name: Word,
    is_macro: bool,
    callable: Word,
    at: usize,
) -> Result<Word, Fault> {
    if !is_macro
        && let Some(code) = runtime.heap.function_code(callable)
        && let Some(info) = runtime.heap.function_info(code)
    {
        runtime.submit_jit_now(info.code_id);
    }
    if name == Word::NIL {
        return Ok(callable);
    }
    if is_macro {
        check_definable_name(runtime, name, at)?;
        runtime.heap.set_macro_expander(name, callable);
    } else {
        FunctionName::parse(runtime, name, at)?.install(runtime, callable, at)?;
    }
    Ok(name)
}

#[derive(Clone, Copy)]
pub(crate) enum DefinitionOp {
    Fboundp,
    SpecialOperatorP,
    SymbolFunction,
    Fdefinition,
}

pub(super) fn inspect(
    runtime: &Runtime,
    op: DefinitionOp,
    value: Word,
    at: usize,
) -> Result<Word, Fault> {
    let truth = |test| if test { Word::T } else { Word::NIL };
    let symbol_error =
        || Fault::type_error(runtime, value, "SYMBOL", EvalError::new(at, "expected a symbol"));
    match op {
        DefinitionOp::SpecialOperatorP => {
            let name = Name::from_word(runtime, value).ok_or_else(symbol_error)?;
            Ok(truth(
                compile::is_special_operator(name.operator())
                    || macros::core_operator(runtime, &name).is_some(),
            ))
        }
        DefinitionOp::SymbolFunction => {
            if !runtime.heap.is_symbol(value) {
                return Err(symbol_error());
            }
            symbol_definition(runtime, value, at)
        }
        DefinitionOp::Fboundp => Ok(truth(
            FunctionName::parse(runtime, value, at)?
                .get(runtime)
                .is_some(),
        )),
        DefinitionOp::Fdefinition => {
            FunctionName::parse(runtime, value, at)?.definition(runtime, value, at)
        }
    }
}

fn setf_definition(runtime: &Runtime, accessor: Word) -> Option<Word> {
    if let Some(function) = runtime.heap.symbol_setter(accessor) {
        return Some(function);
    }
    let accessor = Name::from_word(runtime, accessor)?;
    super::setf_primitive(accessor.operator())
}

fn symbol_definition(runtime: &Runtime, symbol: Word, at: usize) -> Result<Word, Fault> {
    global_definition(runtime, symbol).ok_or_else(|| undefined_function(runtime, symbol, at))
}

fn global_definition(runtime: &Runtime, symbol: Word) -> Option<Word> {
    if let Some(function) = runtime.heap.symbol_cell(symbol, true) {
        return Some(function);
    }
    let name = Name::from_word(runtime, symbol)?;
    if let Some(expander) = macros::lookup(runtime, &name) {
        return Some(expander.slot.get());
    }
    (compile::has_syntax_binding(name.operator())
        || macros::core_operator(runtime, &name).is_some())
    .then_some(symbol)
}

#[derive(Clone, Copy)]
pub(crate) enum FunctionName {
    Symbol(Word),
    Setf(Word),
}

impl FunctionName {
    pub(crate) fn parse(runtime: &Runtime, value: Word, at: usize) -> Result<Self, Fault> {
        if runtime.heap.is_symbol(value) {
            return Ok(Self::Symbol(value));
        }
        if let Some((operator, rest)) = runtime.heap.cons_fields(value)
            && runtime.find_named_symbol(&Name::common_lisp("SETF")) == Some(operator)
            && let Some((accessor, Word::NIL)) = runtime.heap.cons_fields(rest)
            && runtime.heap.is_symbol(accessor)
        {
            return Ok(Self::Setf(accessor));
        }
        Err(Fault::function_name(runtime, 
            value,
            EvalError::new(at, "expected a function name"),
        ))
    }

    pub(crate) fn get(self, runtime: &Runtime) -> Option<Word> {
        match self {
            Self::Symbol(symbol) => global_definition(runtime, symbol),
            Self::Setf(accessor) => setf_definition(runtime, accessor),
        }
    }

    pub(crate) fn install(self, runtime: &Runtime, function: Word, at: usize) -> Result<(), Fault> {
        self.check_definable(runtime, at)?;
        if !is_function(runtime, function) {
            return Err(Fault::type_error(runtime, 
                function,
                "FUNCTION",
                EvalError::new(at, "expected a function"),
            ));
        }
        let (symbol, is_setf) = match self {
            Self::Symbol(symbol) => (symbol, false),
            Self::Setf(symbol) => (symbol, true),
        };
        let symbol = runtime.root_slot(symbol);
        let function =
            super::environment::reinstall_trace(runtime, symbol.get(), is_setf, function, at)?;
        let name = if is_setf {
            Self::Setf(symbol.get())
        } else {
            Self::Symbol(symbol.get())
        };
        name.install_validated(runtime, function);
        Ok(())
    }

    pub(super) fn check_definable(self, runtime: &Runtime, at: usize) -> Result<(), Fault> {
        match self {
            Self::Symbol(symbol) | Self::Setf(symbol) => check_definable_name(runtime, symbol, at),
        }
    }

    pub(super) fn install_validated(self, runtime: &Runtime, function: Word) {
        match self {
            Self::Symbol(symbol) => {
                runtime.heap.set_symbol_cell(symbol, true, function);
                if let Some(name) = Name::from_word(runtime, symbol) {
                    runtime.clear_inline_def(&name);
                }
            }
            Self::Setf(accessor) => runtime.heap.set_symbol_setter(accessor, function),
        }
    }

    pub(super) fn definition(
        self,
        runtime: &Runtime,
        value: Word,
        at: usize,
    ) -> Result<Word, Fault> {
        self.get(runtime).ok_or_else(|| match self {
            Self::Symbol(symbol) => undefined_function(runtime, symbol, at),
            Self::Setf(accessor) => {
                let spelling = runtime
                    .heap
                    .symbol_name(accessor)
                    .expect("SETF accessor is a symbol");
                Fault::cell(runtime, 
                    Class::UndefinedFunction,
                    value,
                    EvalError::new(at, format!("undefined function (SETF {spelling})")),
                )
            }
        })
    }
}

pub(super) fn compile_definition(
    runtime: &Runtime,
    definition: Word,
    at: usize,
) -> Result<(Word, Vec<compile::Warning>), Fault> {
    compile_definition_with(runtime, definition, at, None)
}

fn compile_definition_with(
    runtime: &Runtime,
    definition: Word,
    at: usize,
    call_expander: Option<&macros::CallExpander<'_>>,
) -> Result<(Word, Vec<compile::Warning>), Fault> {
    if !runtime
        .heap
        .cons_fields(definition)
        .is_some_and(|(head, _)| {
            runtime.find_named_symbol(&Name::common_lisp("LAMBDA")) == Some(head)
        })
    {
        return Err(Fault::compile_definition(runtime, 
            definition,
            EvalError::new(at, "COMPILE requires a function or lambda expression"),
        ));
    }
    let definition = runtime.root(definition);
    let form = form_value::from_value(runtime, &definition, at).map_err(compilation_fault)?;
    let Some([_, parameters, body @ ..]) = form.list() else {
        return Err(program_error(
            at,
            "expected a proper lambda expression and lambda list",
        ));
    };
    let (code, warnings) =
        compile::compile_lambda_with(runtime, parameters, body, at, call_expander, &[])
            .map_err(compilation_fault)?;
    Ok((code.instantiate(runtime)?, warnings))
}

fn compilation_fault(error: EvalError) -> Fault {
    if matches!(
        error.kind(),
        EvalErrorKind::Compile | EvalErrorKind::MacroExpansion
    ) {
        Fault::plain(Class::ProgramError, error)
    } else {
        error.into()
    }
}

fn program_error(at: usize, message: &'static str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}
