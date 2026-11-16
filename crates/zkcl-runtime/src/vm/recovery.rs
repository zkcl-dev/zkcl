//! Continuations for a failed cell access. Arguments are copied only after
//! their evaluation, so repairing a definition never replays source forms.
use std::ops::Range;

use super::{
    Callee, Continuation, ContinuationStep, ExecFrame, ExecState, Instruction, Invocation, Pending,
    Primitive, Resume, Signal, StepControl, Values, designator, function_cell, symbol_value,
};
use crate::condition::{Class, Fault};
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::symbol::Name;
use crate::{EvalError, Runtime};

pub(super) struct CellRecovery {
    symbol: RootSlot,
    function: bool,
    definition: bool,
    arguments: Option<RootFrame>,
    choice: usize,
    awaiting_call: bool,
    resume: Resume,
    at: usize,
    pc: Option<usize>,
}

impl CellRecovery {
    pub(super) fn read(
        runtime: &Runtime,
        symbol: Word,
        function: bool,
        resume: Resume,
        at: usize,
    ) -> Self {
        Self {
            symbol: runtime.root_slot(symbol),
            function,
            definition: false,
            arguments: None,
            choice: 0,
            awaiting_call: false,
            resume,
            at,
            pc: None,
        }
    }

    pub(super) fn definition(runtime: &Runtime, name: Word, resume: Resume, at: usize) -> Self {
        Self {
            definition: true,
            ..Self::read(runtime, name, true, resume, at)
        }
    }

    pub(super) fn call(
        runtime: &Runtime,
        symbol: Word,
        slots: &RootSlots,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Self {
        let arguments = runtime.root_frame(args.len());
        for (index, source) in args.enumerate() {
            arguments.set(index, slots.get(source));
        }
        Self {
            arguments: Some(arguments),
            ..Self::read(runtime, symbol, true, resume, at)
        }
    }
}

impl Continuation for CellRecovery {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if self.awaiting_call {
            return Ok(ContinuationStep::KeepValues);
        }
        let choice = std::mem::take(&mut self.choice);
        let value = if choice == 0 {
            match if self.definition {
                super::FunctionName::parse(runtime, self.symbol.get(), self.at)?.definition(
                    runtime,
                    self.symbol.get(),
                    self.at,
                )
            } else if self.function {
                function_cell(runtime, self.symbol.get(), self.at)
            } else {
                symbol_value(runtime, self.symbol.get(), self.at)
            } {
                Ok(value) => value,
                Err(fault) => return Ok(ContinuationStep::CellError { fault }),
            }
        } else {
            let value = values.get(0);
            let value = runtime.root_slot(if self.function {
                designator(runtime, value, self.at)?
            } else {
                value
            });
            if choice == 2 {
                if self.function {
                    let name = if self.definition {
                        super::FunctionName::parse(runtime, self.symbol.get(), self.at)?
                    } else {
                        super::FunctionName::Symbol(self.symbol.get())
                    };
                    name.install(runtime, value.get(), self.at)?;
                } else {
                    runtime
                        .heap
                        .set_symbol_cell(self.symbol.get(), false, value.get());
                }
            }
            value.get()
        };
        if let Some(args) = self.arguments.take() {
            self.awaiting_call = true;
            Ok(ContinuationStep::Call {
                target: value,
                args,
                at: self.at,
            })
        } else {
            Ok(ContinuationStep::Done(value))
        }
    }

    fn restart_invoked(&mut self, binding: usize) {
        self.choice = binding;
    }
    fn signal_pc(&self) -> Option<usize> {
        self.pc
    }
    fn resume_to(&self) -> Resume {
        self.resume
    }
}

/// Establish all choices over a single continuation, so selecting any choice
/// leaves the complete recovery cluster and runs intervening Lisp cleanups.
pub(super) fn start<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    continuation: Box<dyn Continuation + 'rt>,
    fault: Fault,
) -> Result<StepControl<'rt>, EvalError> {
    let condition = match fault.make_condition(runtime) {
        Some(condition) => runtime.root_slot(condition),
        None => return super::dispatch_error(runtime, exec, fault),
    };
    let make_restarts = || -> Result<RootFrame, EvalError> {
        let restarts = runtime.root_frame(3);
        for (index, name) in ["CONTINUE", "USE-VALUE", "STORE-VALUE"]
            .into_iter()
            .enumerate()
        {
            let name =
                runtime.root_slot(runtime.intern(&Name::common_lisp(name), fault.host.offset())?);
            let restart = runtime
                .heap
                .restart(|| name.get())
                .map_err(|error| EvalError::out_of_memory(fault.host.offset(), error))?;
            restarts.set(index, restart);
            let report = runtime
                .heap
                .string(match index {
                    0 => "Retry the cell access after repairing its binding.",
                    1 => "Supply a value for this operation.",
                    _ => "Store a value in the binding and continue.",
                })
                .map_err(|error| EvalError::out_of_memory(fault.host.offset(), error))?;
            runtime.heap.set_restart_report(restarts.get(index), report);
        }
        Ok(restarts)
    };
    let restarts = match make_restarts() {
        Ok(restarts) => restarts,
        Err(error) => return super::dispatch_error(runtime, exec, error.into()),
    };
    let at = fault.host.offset();
    let mut signal = Signal::new(
        runtime,
        condition.get(),
        Some(fault.host),
        at,
        Resume::Continuation,
    );
    signal.vm_pc = continuation.signal_pc();
    signal.associated_restart_scope = Some(exec.control.enter_resumable_restarts(
        exec.frames.len(),
        restarts,
        vec![0, 1, 1].into_boxed_slice(),
        None,
        continuation,
    ));
    Ok(StepControl::pending(Pending::Signal(
        exec.control.search_handlers(signal),
    )))
}

/// Recover only the failed operation at the current bytecode instruction.
/// Primitive internals that already performed effects are deliberately outside
/// this path: they must install their own continuation at the failing operation.
pub(super) fn for_instruction(
    runtime: &Runtime,
    exec: &ExecState<'_>,
    fault: &Fault,
    vm_pc: usize,
) -> Option<CellRecovery> {
    let (class, symbol) = fault.cell_name()?;
    let ExecFrame::Bytecode(frame) = exec.frames.last()? else {
        return None;
    };
    let pc = frame.vm_code.canonical_pc(vm_pc)?;
    let mut recovery = match &frame.vm_code.canonical[pc] {
        Instruction::ReadCell {
            out,
            symbol: index,
            function,
            at,
        } if frame.constant(runtime, *index) == symbol => {
            CellRecovery::read(runtime, symbol, *function, Resume::Call(Some(*out)), *at)
        }
        Instruction::CallValues { callee, at }
            if class == Class::UndefinedFunction && frame.slots.get(*callee) == symbol =>
        {
            CellRecovery::call(
                runtime,
                symbol,
                &exec.values.slots,
                0..exec.values.len,
                Resume::Call(None),
                *at,
            )
        }
        Instruction::Call {
            callee, args, at, ..
        } => {
            let mut target = match *callee {
                Callee::Named(index) => frame.constant(runtime, index),
                Callee::Value(register) => frame.slots.get(register),
            };
            let mut arguments = runtime.root_frame(args.len());
            for (index, source) in args.clone().enumerate() {
                arguments.set(index, frame.slots.get(source));
            }
            let resume = super::ops::resume_native_call(&frame.vm_code, pc);
            loop {
                if runtime.heap.is_symbol(target) {
                    if target == symbol
                        && class == Class::UndefinedFunction
                        && runtime.heap.symbol_cell(target, true).is_none()
                    {
                        break CellRecovery::call(
                            runtime,
                            symbol,
                            &arguments,
                            0..arguments.len(),
                            resume,
                            *at,
                        );
                    }
                    target = runtime.heap.symbol_cell(target, true)?;
                }
                match Primitive::decode(target)? {
                    Primitive::Funcall if !arguments.is_empty() => {
                        target = arguments.get(0);
                        let rest = runtime.root_frame(arguments.len() - 1);
                        for index in 0..rest.len() {
                            rest.set(index, arguments.get(index + 1));
                        }
                        arguments = rest;
                    }
                    Primitive::Apply if arguments.len() >= 2 => {
                        target = arguments.get(0);
                        let tail =
                            super::proper_list(runtime, arguments.get(arguments.len() - 1), *at)
                                .ok()?;
                        let rest = runtime.root_frame(arguments.len() - 2 + tail.len());
                        for index in 0..arguments.len() - 2 {
                            rest.set(index, arguments.get(index + 1));
                        }
                        for (index, value) in tail.into_iter().enumerate() {
                            rest.set(arguments.len() - 2 + index, value);
                        }
                        arguments = rest;
                    }
                    Primitive::SymbolValue if class == Class::UnboundVariable => {
                        break CellRecovery::read(runtime, symbol, false, resume, *at);
                    }
                    Primitive::FunctionDefinition(
                        super::function_definitions::DefinitionOp::SymbolFunction
                        | super::function_definitions::DefinitionOp::Fdefinition,
                    ) if class == Class::UndefinedFunction => {
                        break CellRecovery::definition(runtime, symbol, resume, *at);
                    }
                    _ => return None,
                }
            }
        }
        _ => return None,
    };
    recovery.pc = Some(vm_pc);
    Some(recovery)
}

pub(super) fn call<'rt>(
    runtime: &'rt Runtime,
    symbol: Word,
    slots: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    match function_cell(runtime, symbol, at) {
        Ok(target) => super::invoke(runtime, target, slots, args, values, resume, at),
        Err(_) => Ok(Invocation::Continuation(Box::new(CellRecovery::call(
            runtime, symbol, slots, args, resume, at,
        )))),
    }
}

struct AbortEvaluation;
impl Continuation for AbortEvaluation {
    fn step(
        &mut self,
        _runtime: &Runtime,
        _values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        Err(
            EvalError::execution_stopped(0, crate::ExecutionStop::Cancelled)
                .after_debugger_abort()
                .into(),
        )
    }
    fn resume_to(&self) -> Resume {
        Resume::Call(None)
    }
}

pub(super) fn enter_toplevel<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
) -> Result<(), EvalError> {
    let name = runtime.staging_stack.alloc(1);
    name.set(0, runtime.intern(&Name::common_lisp("ABORT"), 0)?);
    let restart = runtime
        .heap
        .restart(|| name.get(0))
        .map_err(|error| EvalError::out_of_memory(0, error))?;
    exec.control
        .enter_resumable_restart(runtime, 0, restart, None, Box::new(AbortEvaluation));
    Ok(())
}

/// Native slow paths call this before they discard the failed cell operation.
/// The native activation and all its arguments remain live during the debugger.
pub(crate) fn read_from_native<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    symbol: Word,
    function: bool,
    at: usize,
) -> Result<crate::native::NativeReturn, Fault> {
    let saved = exec.values.take();
    let recovery = CellRecovery::read(runtime, symbol, function, Resume::Call(None), at);
    let result = run_from_native(runtime, exec, recovery)?;
    if !matches!(result, crate::native::NativeReturn::Unwind { .. }) {
        exec.values = saved;
    }
    Ok(result)
}

pub(crate) fn call_from_native<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    symbol: Word,
    slots: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<crate::native::NativeReturn, Fault> {
    run_from_native(
        runtime,
        exec,
        CellRecovery::call(runtime, symbol, slots, args, Resume::Call(None), at),
    )
}

fn run_from_native<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    recovery: CellRecovery,
) -> Result<crate::native::NativeReturn, Fault> {
    let base = exec.frames.len();
    match super::dispatch_continuation(runtime, exec, Box::new(recovery), false)
        .map_err(Fault::dispatched)?
    {
        StepControl::Pending(pending) => {
            super::execute_until(runtime, exec, base, pending).map_err(Fault::dispatched)
        }
        StepControl::Unwind(target) => Ok(target.into()),
        StepControl::QuantumYield | StepControl::Suspend { .. } => {
            unreachable!("native recovery is synchronous")
        }
    }
}

pub(super) struct InteractiveValue {
    phase: u8,
    stream: RootSlot,
    at: usize,
}

impl InteractiveValue {
    pub(super) fn new(at: usize) -> Self {
        Self {
            phase: 0,
            stream: RootSlot::nil(),
            at,
        }
    }
}

impl Continuation for InteractiveValue {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        let phase = self.phase;
        self.phase += 1;
        let (target, args) = match phase {
            0 => {
                let symbol = runtime.intern(&Name::common_lisp("*QUERY-IO*"), self.at)?;
                self.stream = runtime.root_slot(symbol_value(runtime, symbol, self.at)?);
                let args = runtime.root_frame(2);
                args.set(0, self.stream.get());
                args.set(
                    1,
                    runtime
                        .heap
                        .string("~&Value for this restart: ")
                        .map_err(|error| EvalError::out_of_memory(self.at, error))?,
                );
                (Primitive::named("FORMAT").expect("FORMAT primitive"), args)
            }
            1 => {
                let args = runtime.root_frame(1);
                args.set(0, self.stream.get());
                (Primitive::named("READ").expect("READ primitive"), args)
            }
            2 => {
                let args = runtime.root_frame(1);
                args.set(0, values.get(0));
                (Primitive::named("EVAL").expect("EVAL primitive"), args)
            }
            _ => {
                let arguments = runtime
                    .heap
                    .cons(|| (values.get(0), Word::NIL))
                    .map_err(|error| EvalError::out_of_memory(self.at, error))?;
                return Ok(ContinuationStep::Done(arguments));
            }
        };
        Ok(ContinuationStep::Call {
            target,
            args,
            at: self.at,
        })
    }
    fn resume_to(&self) -> Resume {
        Resume::RestartInteractive
    }
}

#[cfg(test)]
mod tests {
    use crate::{Runtime, heap::Heap};

    #[test]
    fn traced_store_value_roots_the_name_and_replacement_during_relocation() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        config.stress_bytes = Some(128 << 10);
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        runtime
            .load_str("(defun relocating-recovery (x) x) (trace relocating-recovery)")
            .unwrap();
        let result = runtime
            .eval(
                r#"
          (let ((*trace-output* (make-string-output-stream)))
            (dotimes (i 24 t)
              (fmakunbound 'relocating-recovery)
              (let ((replacement (lambda (x) (list x))))
                (handler-bind ((undefined-function (lambda (c) (store-value replacement c))))
                  (assert (= (car (relocating-recovery i)) i)))
                (assert (= (car (relocating-recovery (+ i 1))) (+ i 1)))
                (untrace relocating-recovery)
                (assert (eq replacement #'relocating-recovery))
                (trace relocating-recovery))))"#,
            )
            .unwrap();
        assert!(!result[0].is_nil());
    }
}
