use std::ops::Range;

use smallvec::SmallVec;

use super::association::Association;
use super::deduplicate::Deduplicate;
use super::find_position_count;
use super::hash_tables::MapHash;
use super::mapping::Mapping;
use super::predicate_remove::PredicateRemove;
use super::reduce::Reduction;
use super::restart::RestartOperation;
use super::search_mismatch::SubsequenceComparison;
use super::sequence_map::SequenceMap;
use super::sets::SetOperation;
use super::sort::SortOperation;
use super::tree::TreeOperation;
use super::type_check::TypeCheck;
use super::{
    Continuation, CorrectableError, Evaluation, Frame, Invocation, Macroexpansion, Primitive,
    Register, RestartRequest, Resume, Signal, Values, designator, environment, format,
    function_definitions, hash_tables, instances, method_combination, modules, packages, primitive,
    printer, proper_list, streams, task_scheduler, try_native_call, warn_invocation,
};
use crate::condition::{self, Class, Fault};
use crate::heap::{RootFrame, RootSlots, RootStackFrame, Word};
use crate::{EvalError, RegisteredFunction, Runtime};

// Both APPLY and generic dispatch can redirect a call repeatedly. The current
// argument owner stays alive until the next argument list has been prepared.
enum CallArguments<'a, 'rt> {
    Caller(&'a RootSlots),
    Applied(RootStackFrame<'rt>),
    Generic(RootFrame),
}

impl CallArguments<'_, '_> {
    fn slots(&self) -> &RootSlots {
        match self {
            Self::Caller(slots) => slots,
            Self::Applied(slots) => slots,
            Self::Generic(slots) => slots,
        }
    }
}

pub(super) fn invoke<'rt>(
    runtime: &'rt Runtime,
    mut target: Word,
    frame: &RootSlots,
    mut args: Range<Register>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let mut arguments = CallArguments::Caller(frame);
    loop {
        let frame = arguments.slots();
        if let Some(function) = runtime.heap.generic_function_target(target) {
            if function != Word::NIL {
                target = function;
                continue;
            }
            if let Some(primary) =
                method_combination::cached_direct_primary(runtime, target, args.len(), |i| {
                    frame.get(args.start + i)
                })
            {
                let prepared = runtime.staging_stack.alloc(2 + args.len());
                // The root stack initializes the two unused next-method arguments to NIL.
                for (index, argument) in args.enumerate() {
                    prepared.set(2 + index, frame.get(argument));
                }
                let code = runtime.heap.function_code(primary).expect("cached method code");
                return invoke_lisp(
                    runtime,
                    primary,
                    code,
                    &prepared,
                    0..prepared.len(),
                    values,
                    resume,
                    at,
                );
            }
            let protocol_call = match runtime.invocation_protocol.get() {
                Some(protocol) => protocol.invocation(runtime, target, frame, args.clone(), at)?,
                None => None,
            };
            let (callable, prepared) = match protocol_call {
                Some(call) => call,
                None => method_combination::prepare_generic_call(runtime, target, frame, args, at)?,
            };
            target = callable;
            args = 0..prepared.len();
            arguments = CallArguments::Generic(prepared);
            continue;
        }
        if let Some(function) = runtime.heap.funcallable_target(target) {
            if function == Word::UNBOUND {
                return Err(Fault::plain(
                    Class::Error,
                    EvalError::new(at, "funcallable instance has no installed function"),
                ));
            }
            target = function;
            continue;
        }
        if let Some(index) = target.native_index() {
            // Release the registry borrow before calling out: the callback may call Lisp
            // again or install another function. Arguments stay rooted through either.
            let registered = runtime.natives.borrow().0[index].clone();
            let callback = match registered {
                RegisteredFunction::Sync(callback) => callback,
                RegisteredFunction::Host(callback) => {
                    return Ok(Invocation::Continuation(Box::new(
                        super::callback::HostCall::new(runtime, callback, frame, args, resume, at),
                    )));
                }
                RegisteredFunction::Async(start) => {
                    return Ok(task_scheduler::invoke_host_operation(
                        runtime, start, frame, args, resume, at,
                    ));
                }
            };
            let arguments = args
                .map(|reg| runtime.root(frame.get(reg)))
                .collect::<SmallVec<[_; 4]>>();
            let _pin = runtime.pin_rust_frame();
            let results = callback(runtime, &arguments, at)?;
            values.select_roots(&results);
            return Ok(Invocation::Done(resume));
        }
        let Some(op) = Primitive::decode(target) else {
            if let Some(code) = runtime.heap.function_code(target) {
                return invoke_lisp(runtime, target, code, frame, args, values, resume, at);
            }
            if let Some(result) = {
                let _pin = runtime.pin_rust_frame();
                runtime.call_host_function(target, frame, args, at)
            } {
                match result? {
                    Some(word) => values.one(word),
                    None => values.resize(0),
                }
                return Ok(Invocation::Done(resume));
            }
            return Err(Fault::type_error(runtime, 
                target,
                "FUNCTION",
                EvalError::new(at, "expected a function designator"),
            ));
        };
        crate::metrics::record_primitive_call();
        if !op.accepts(args.len()) {
            return Err(Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "wrong number of arguments"),
            ));
        }
        match op {
            Primitive::Funcall => {
                let function = frame.get(args.next().unwrap());
                if runtime.heap.is_symbol(function)
                    && runtime.heap.symbol_cell(function, true).is_none()
                {
                    return Ok(Invocation::Continuation(Box::new(
                        super::recovery::CellRecovery::call(
                            runtime, function, frame, args, resume, at,
                        ),
                    )));
                }
                target = designator(runtime, function, at)?;
            }
            Primitive::Apply => {
                let function = frame.get(args.start);
                let prepared = apply_arguments(runtime, frame, args.start + 1..args.end, at)?;
                if runtime.heap.is_symbol(function)
                    && runtime.heap.symbol_cell(function, true).is_none()
                {
                    return Ok(Invocation::Continuation(Box::new(
                        super::recovery::CellRecovery::call(
                            runtime,
                            function,
                            &prepared,
                            0..prepared.len(),
                            resume,
                            at,
                        ),
                    )));
                }
                target = designator(runtime, function, at)?;
                args = 0..prepared.len();
                arguments = CallArguments::Applied(prepared);
            }
            _ => return invoke_primitive(runtime, op, frame, args, values, resume, at),
        }
    }
}

impl Runtime {
    pub(crate) fn call_host_function(
        &self,
        target: Word,
        frame: &RootSlots,
        args: Range<Register>,
        at: usize,
    ) -> Option<Result<Option<Word>, Fault>> {
        let callback = self.heap.host_callback(target)?;
        // The callback may unbind itself and run a moving GC. The function
        // traces its Lisp capture and owns the registry entry for the callback.
        let callee = self.staging_stack.alloc(1);
        callee.set(0, target);
        // SAFETY: callee keeps the Rc allocation live across reentry. Arguments
        // borrow registered slots, so subsequent reads observe relocated words.
        Some(unsafe { &*callback }(self, frame.slice(args), at))
    }
}

fn apply_arguments<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    args: Range<Register>,
    at: usize,
) -> Result<RootStackFrame<'rt>, Fault> {
    let last = args.end - 1;
    let tail = frame.get(last);
    let suffix =
        proper_list(runtime, tail, at).map_err(|error| Fault::type_error(runtime, tail, "LIST", error))?;
    let prefix = args.start..last;
    let flattened = runtime.staging_stack.alloc(prefix.len() + suffix.len());
    for (index, word) in prefix.map(|reg| frame.get(reg)).chain(suffix).enumerate() {
        flattened.set(index, word);
    }
    Ok(flattened)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal VM call setup forwards target, slots, and continuation resume directly"
)]
fn invoke_lisp<'rt>(
    runtime: &'rt Runtime,
    target: Word,
    code: Word,
    frame: &RootSlots,
    args: Range<Register>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let info = runtime.heap.function_info(code).unwrap();
    let vm_code = runtime.heap.code_store.get(info.code_id);
    if !vm_code.arity.accepts(args.len()) {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(
                at,
                format!(
                    "function requires {} arguments, got {}",
                    vm_code.arity.description(),
                    args.len(),
                ),
            ),
        ));
    }
    if let Some(call) = try_native_call(
        runtime,
        info.code_id,
        &vm_code,
        target,
        frame,
        args.clone(),
        values,
        resume,
        false,
    ) {
        return Ok(call);
    }
    Ok(Invocation::Lisp(Frame::new_with_code(
        runtime, target, code, vm_code, frame, args, resume, at,
    )))
}

fn continued<'rt>(operation: impl Continuation + 'rt) -> Invocation<'rt> {
    Invocation::Continuation(Box::new(operation))
}

fn invoke_primitive<'rt>(
    runtime: &'rt Runtime,
    op: Primitive,
    frame: &RootSlots,
    args: Range<Register>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    if let Some(reader) =
        streams::gray_input::GrayRead::for_call(runtime, op, frame, args.clone(), resume, at)?
    {
        return Ok(continued(reader));
    }
    if let Some(call) = streams::gray::redirect(runtime, op, frame, args.clone(), at)? {
        let (function, arguments) = call;
        return invoke(
            runtime,
            function,
            &arguments,
            0..arguments.len(),
            values,
            resume,
            at,
        );
    }
    let op = match op {
        Primitive::Stream(streams::Op::Princ) => Primitive::Format(format::OutputPrimitive::Princ),
        Primitive::Stream(streams::Op::Prin1) => Primitive::Format(format::OutputPrimitive::Prin1),
        Primitive::Stream(streams::Op::Print) => Primitive::Format(format::OutputPrimitive::Print),
        other => other,
    };

    let restart = |operation| {
        Invocation::Restart(RestartRequest::new(
            runtime,
            operation,
            frame,
            args.clone(),
            at,
            resume,
        ))
    };
    if matches!(op, Primitive::SymbolValue) {
        let symbol = frame.get(args.start);
        if runtime.heap.is_symbol(symbol) && runtime.heap.symbol_cell(symbol, false).is_none() {
            return Ok(continued(super::recovery::CellRecovery::read(
                runtime, symbol, false, resume, at,
            )));
        }
    }
    if matches!(
        op,
        Primitive::FunctionDefinition(
            function_definitions::DefinitionOp::SymbolFunction
                | function_definitions::DefinitionOp::Fdefinition
        )
    ) {
        let name = frame.get(args.start);
        if (runtime.heap.is_symbol(name)
            || matches!(
                op,
                Primitive::FunctionDefinition(function_definitions::DefinitionOp::Fdefinition)
            ))
            && let Ok(function_name) = super::FunctionName::parse(runtime, name, at)
            && function_name.get(runtime).is_none()
        {
            return Ok(continued(super::recovery::CellRecovery::definition(
                runtime, name, resume, at,
            )));
        }
    }
    Ok(match op {
        Primitive::Printer(_) | Primitive::Format(_) => continued(printer::PrintContinuation::new(
            runtime, op, frame, args, at, resume,
        )),
        Primitive::Debug(op) => {
            continued(super::debugging::Request::new(runtime, op, frame, args, resume, at))
        }
        Primitive::Environment(super::environment::Op::Inspect) => {
            let inspector = runtime.inspector.borrow().clone();
            if let Some(inspector) = inspector {
                return invoke(runtime, inspector.get(), frame, args, values, resume, at);
            }
            primitive(runtime, frame, op, args, values, at)?;
            Invocation::Done(resume)
        }
        Primitive::Association(op) => {
            continued(Association::new(runtime, frame, op, args, resume, at)?)
        }
        Primitive::Deduplicate(op) => {
            continued(Deduplicate::new(runtime, frame, op, args, resume, at)?)
        }
        Primitive::PredicateRemove(op) => {
            continued(PredicateRemove::new(runtime, frame, op, args, resume, at)?)
        }
        Primitive::ElementSearch(op) => {
            return find_position_count::invoke(runtime, frame, op, args, values, resume, at);
        }
        Primitive::SubsequenceComparison(op) => continued(SubsequenceComparison::new(
            runtime, frame, op, args, resume, at,
        )?),
        Primitive::Tree(op) => continued(TreeOperation::new(runtime, frame, op, args, resume, at)?),
        Primitive::Sets(op) => continued(SetOperation::new(runtime, frame, op, args, resume, at)?),
        Primitive::Sort(op) => continued(SortOperation::new(runtime, frame, op, args, resume, at)?),
        Primitive::Reduce(op) => continued(Reduction::new(runtime, frame, op, args, resume, at)?),
        Primitive::Mapping(op) => continued(Mapping::new(runtime, frame, op, args, resume, at)?),
        Primitive::SequenceMap(op) => {
            continued(SequenceMap::new(runtime, frame, op, args, resume, at)?)
        }
        Primitive::HashTable(hash_tables::Op::Map) => {
            continued(MapHash::new(runtime, frame, args, resume, at)?)
        }
        Primitive::Typep => match TypeCheck::new(runtime, frame, args.clone(), resume, at)? {
            Some(operation) => continued(operation),
            None => {
                primitive(runtime, frame, op, args, values, at)?;
                return Ok(Invocation::Done(resume));
            }
        },
        Primitive::CompileFile => Invocation::Continuation(crate::fasl::start_compile_lisp(
            runtime, frame, args, resume, at,
        )?),
        Primitive::InvokeDebugger => {
            let condition = frame.get(args.start);
            if !condition::is_condition(runtime, condition) {
                return Err(Fault::type_error(runtime, 
                    condition,
                    "CONDITION",
                    EvalError::new(at, "INVOKE-DEBUGGER expects a condition"),
                ));
            }
            Invocation::InvokeDebugger(Signal::from_lisp(runtime, condition, true, at, resume))
        }
        Primitive::MakeCondition | Primitive::Signal | Primitive::Error | Primitive::Warn => {
            return invoke_condition(runtime, op, frame, args, values, resume, at);
        }
        Primitive::Cerror => Invocation::CorrectableError(Box::new(CorrectableError::new(
            runtime, frame, args, resume, at,
        )?)),
        Primitive::CallWithSimpleRestart => {
            let restart = runtime
                .heap
                .restart(|| frame.get(args.start))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            runtime
                .heap
                .set_restart_report(restart, frame.get(args.start + 1));
            Invocation::SimpleRestart {
                restart: runtime.root_slot(restart),
                body: runtime.root_slot(frame.get(args.start + 2)),
                resume,
                at,
            }
        }
        Primitive::FindRestart => restart(RestartOperation::Find),
        Primitive::ComputeRestarts => restart(RestartOperation::Compute),
        Primitive::InvokeRestart => restart(RestartOperation::Invoke),
        Primitive::InvokeRestartInteractively => restart(RestartOperation::InvokeInteractively),
        Primitive::StandardRestart(which) => restart(RestartOperation::Standard(which)),
        Primitive::Macroexpand | Primitive::MacroexpandOne => {
            let form = runtime.root_frame(1);
            form.set(0, frame.get(args.start));
            Invocation::Macroexpand(Macroexpansion {
                form,
                environment: runtime.root_slot(if args.len() == 2 {
                    frame.get(args.start + 1)
                } else {
                    Word::NIL
                }),
                repeat: matches!(op, Primitive::Macroexpand),
                steps: 0,
                resume,
                at,
            })
        }
        Primitive::Instance(op) => {
            return instances::start(runtime, frame, op, args, values, resume, at);
        }
        Primitive::DefineNamedClass => continued(super::class_definition::DefineNamedClass::new(
            runtime,
            frame.get(args.start),
            frame.get(args.start + 1),
            resume,
            at,
        )?),
        Primitive::Eval => {
            let object = runtime.root(frame.get(args.start));
            let form = crate::form_value::from_value(runtime, &object, at)?;
            Invocation::Evaluate(Box::new(Evaluation::lisp_eval(form, resume)))
        }
        Primitive::Load | Primitive::LoadLogicalPathnameTranslations => {
            let file = if matches!(op, Primitive::Load) {
                crate::loader::prepare_lisp_load(runtime, frame, args, at)?
            } else {
                crate::loader::prepare_translations_load(runtime, frame.get(args.start), at)?
            };
            match file {
                Some(file) => {
                    Invocation::Evaluate(Box::new(Evaluation::file(file, resume, Some(at))))
                }
                None => {
                    values.one(Word::NIL);
                    Invocation::Done(resume)
                }
            }
        }
        Primitive::Require => {
            let name = frame.get(args.start);
            let pathnames = if args.len() == 2 {
                frame.get(args.start + 1)
            } else {
                Word::NIL
            };
            if let Some(invocation) = modules::Require::start(runtime, name, pathnames, resume, at)?
            {
                return Ok(invocation);
            }
            values.one(Word::NIL);
            Invocation::Done(resume)
        }
        Primitive::Compile => {
            return function_definitions::start(runtime, frame, args, values, resume, at);
        }
        Primitive::Package(package) => {
            return packages::invoke(runtime, frame, package, args, values, resume, at);
        }
        Primitive::Environment(environment::Op::Sleep) => {
            return task_scheduler::invoke_sleep(runtime, frame, args, resume, at);
        }
        Primitive::Async(op) => {
            return task_scheduler::invoke_async(runtime, op, frame, args, values, resume, at);
        }
        _ => {
            primitive(runtime, frame, op, args, values, at)?;
            Invocation::Done(resume)
        }
    })
}

fn invoke_condition<'rt>(
    runtime: &'rt Runtime,
    op: Primitive,
    frame: &RootSlots,
    args: Range<Register>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let datum = frame.get(args.start);
    if matches!(op, Primitive::MakeCondition) {
        return super::condition_type::construct(runtime, op, frame, args, values, resume, at);
    }
    let condition = if condition::is_condition(runtime, datum) {
        if args.len() != 1 {
            return Err(Fault::type_error(runtime, 
                frame.get(args.start + 1),
                "NULL",
                EvalError::new(at, "a condition object takes no initargs"),
            ));
        }
        datum
    } else if runtime.heap.is_symbol(datum) || runtime.heap.class_info(datum).is_some() {
        return super::condition_type::construct(runtime, op, frame, args, values, resume, at);
    } else {
        let default = match op {
            Primitive::Warn => Class::SimpleWarning,
            Primitive::Error => Class::SimpleError,
            _ => Class::SimpleCondition,
        };
        condition::from_format_control(runtime, frame, args, default, at)?
    };
    if matches!(op, Primitive::Warn) {
        return warn_invocation(runtime, condition, resume, at, None);
    }
    Ok(Invocation::Signal(Signal::from_lisp(
        runtime,
        condition,
        matches!(op, Primitive::Error),
        at,
        resume,
    )))
}
