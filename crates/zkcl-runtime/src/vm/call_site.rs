use std::cell::Cell;
use std::ops::Range;

use zkcl_bytecode::native_abi::I32x4Operation;
use zkcl_bytecode::{LaneKind, PackShape};

use super::{
    ArgumentTail, Callee, ControlStack, ExecFrame, ExecState, Frame, Instruction,
    InstructionAction, Invocation, Primitive, Register, Resume, TailArguments, Values,
    array_access, arrays, designator, exec_numeric_primitive, exec_slow,
    finish_checked_single_result_call, finish_single_result_call, invoke, is_binary_numeric,
    is_funcall, primitive, simd, try_native_call, undefined_function,
};
use crate::Runtime;
use crate::code_store::{ArithOperands, CodeId, SiteFeedback, StampedSiteFeedback, VmCode};
use crate::condition::Fault;
use crate::heap::{RootSlots, RootStackFrame, Word};

/// A named call site's feedback under the binding read at dispatch.
#[derive(Clone, Copy)]
pub(super) struct CallSite<'a> {
    cell: &'a Cell<StampedSiteFeedback>,
    checked_at: usize,
    pub(super) observed: SiteFeedback,
}

impl<'a> CallSite<'a> {
    #[inline(always)]
    pub(super) fn lookup(
        runtime: &Runtime,
        frame: &'a Frame<'_>,
        pc: usize,
        symbol: usize,
    ) -> Self {
        let cell = frame.vm_code.feedback(pc);
        let checked_at = runtime.heap.function_epoch();
        let prior = cell.get();
        let observed = if prior.checked_at == checked_at {
            prior.kind
        } else {
            Self::refresh(runtime, frame, symbol, cell, prior, checked_at)
        };
        Self {
            cell,
            checked_at,
            observed,
        }
    }

    #[cold]
    #[inline(never)]
    fn refresh(
        runtime: &Runtime,
        frame: &Frame<'_>,
        symbol: usize,
        cell: &Cell<StampedSiteFeedback>,
        prior: StampedSiteFeedback,
        checked_at: usize,
    ) -> SiteFeedback {
        // Named call sites refer to fixed symbol constants, including across
        // closures of the same code, GC, and package membership changes.
        if prior.checked_at != 0
            && runtime
                .heap
                .symbol_function_revision(frame.constant(runtime, symbol))
                .is_some_and(|revision| revision <= prior.checked_at)
        {
            cell.set(StampedSiteFeedback {
                checked_at,
                ..prior
            });
            prior.kind
        } else {
            SiteFeedback::Cold
        }
    }

    pub(super) fn record(self, kind: SiteFeedback) {
        // The stamp belongs to the original binding read, even if filling the
        // observation crosses a GC or reentry boundary.
        self.cell.set(StampedSiteFeedback {
            checked_at: self.checked_at,
            kind,
        });
    }
}

#[inline(always)]
fn try_restart_self(frame: &mut Frame<'_>, target: Word, args: Range<Register>) -> bool {
    let arity = frame.vm_code.arity;
    if target != frame.slots.get(frame.closure)
        || arity.required != args.len()
        || arity.maximum != Some(arity.required)
    {
        return false;
    }
    restart_fixed_self(frame, args);
    true
}

#[inline(always)]
fn restart_fixed_self(frame: &mut Frame<'_>, args: Range<Register>) {
    let required = args.len();
    frame.slots.move_to_start(args);
    frame.clear(required..frame.code);
    frame.pc = 0;
}

pub(super) fn stage_call<'rt>(
    runtime: &'rt Runtime,
    target: Word,
    slots: &RootSlots,
    args: Range<Register>,
) -> RootStackFrame<'rt> {
    let staged = runtime.staging_stack.alloc(1 + args.len());
    staged.set(0, target);
    for (i, reg) in args.enumerate() {
        staged.set(1 + i, slots.get(reg));
    }
    staged
}

#[inline]
#[expect(
    clippy::too_many_arguments,
    reason = "VM tail dispatch receives the live frame and control state"
)]
pub(super) fn prepare_tail_call<'rt>(
    runtime: &'rt Runtime,
    frame: &mut Frame<'rt>,
    frame_index: usize,
    vm_pc: usize,
    callee: Callee,
    mut args: Range<Register>,
    values: &mut Values,
    control: &mut ControlStack<'rt>,
) -> Result<InstructionAction<'rt>, Fault> {
    if control.has_frame_scopes(frame_index) {
        return exec_slow(
            runtime,
            frame,
            frame_index,
            vm_pc,
            frame
                .vm_code
                .canonical_pc(vm_pc)
                .expect("tail call has a source instruction"),
            values,
            control,
        );
    }

    let call = match callee {
        Callee::Named(sym) => resolve_named_call(runtime, frame, vm_pc, sym, &args)?,
        Callee::Value(reg) => {
            let target = frame.slots.get(reg);
            if try_restart_self(frame, target, args.clone()) {
                return Ok(InstructionAction::Continue);
            }
            CallTarget::General(designator(runtime, target, frame.logical_at(vm_pc))?)
        }
    };
    let target = match call {
        CallTarget::Fixed { code, code_id } => {
            if try_restart_self(frame, code, args.clone()) {
                return Ok(InstructionAction::Continue);
            }
            return prepare_fixed_tail_call(
                runtime, frame, vm_pc, code, code, code_id, args, values,
            );
        }
        CallTarget::General(target) => {
            if is_funcall(target)
                && let Some(reg) = args.next()
            {
                crate::metrics::record_primitive_call();
                let target = frame.slots.get(reg);
                if try_restart_self(frame, target, args.clone()) {
                    return Ok(InstructionAction::Continue);
                }
                designator(runtime, target, frame.logical_at(vm_pc))?
            } else {
                target
            }
        }
        // Leave the frame in place until the operation succeeds so faults
        // still refer to the call site, then let the dispatcher return its values.
        CallTarget::Primitive(prim) => {
            crate::metrics::record_primitive_call();
            primitive(
                runtime,
                &frame.slots,
                prim,
                args,
                values,
                frame.logical_at(vm_pc),
            )?;
            frame.pc = frame.vm_code.vm_ops.len();
            return Ok(InstructionAction::Continue);
        }
        CallTarget::Arithmetic(prim, site) => {
            crate::metrics::record_primitive_call();
            exec_numeric_primitive(
                runtime,
                site,
                prim,
                &frame.slots,
                args,
                values,
                frame.logical_at(vm_pc),
            )?;
            frame.pc = frame.vm_code.vm_ops.len();
            return Ok(InstructionAction::Continue);
        }
        CallTarget::ArrayReader(site) => {
            array_access::exec_named_aref(runtime, frame, site, args.start, values, || {
                frame.logical_at(vm_pc)
            })?;
            frame.pc = frame.vm_code.vm_ops.len();
            return Ok(InstructionAction::Continue);
        }
    };
    prepare_general_tail_call(runtime, frame, vm_pc, target, args, values)
}

fn prepare_general_tail_call<'rt>(
    runtime: &'rt Runtime,
    frame: &mut Frame<'rt>,
    vm_pc: usize,
    target: Word,
    args: Range<Register>,
    values: &mut Values,
) -> Result<InstructionAction<'rt>, Fault> {
    let arity = frame.vm_code.arity;
    if target == frame.slots.get(frame.closure) && arity.accepts(args.len()) {
        if arity.maximum == Some(arity.required) {
            restart_fixed_self(frame, args);
            return Ok(InstructionAction::Continue);
        }
        let first_optional = args.start + arity.required;
        // The new arguments already live in the frame's registers. Release
        // the previous tail before reserving a new slice on the root stack.
        frame.arguments = ArgumentTail::default();
        frame.arguments = ArgumentTail::new(runtime, args.end - first_optional, |index| {
            frame.slots.get(first_optional + index)
        });
        frame.slots.move_to_start(args.start..first_optional);
        frame.clear(arity.required..frame.code);
        frame.call_at = frame.logical_at(vm_pc);
        frame.pc = 0;
        return Ok(InstructionAction::Continue);
    }
    if let Some(code) = runtime.heap.function_code(target) {
        let code_id = runtime.heap.function_info(code).unwrap().code_id;
        return prepare_fixed_tail_call(runtime, frame, vm_pc, target, code, code_id, args, values);
    }
    Ok(InstructionAction::TailCall {
        target,
        args: TailArguments::Registers(args),
        at: frame.logical_at(vm_pc),
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "tail replacement receives the resolved function and its bytecode"
)]
fn prepare_fixed_tail_call<'rt>(
    runtime: &'rt Runtime,
    frame: &mut Frame<'rt>,
    vm_pc: usize,
    target: Word,
    code: Word,
    code_id: CodeId,
    args: Range<Register>,
    values: &mut Values,
) -> Result<InstructionAction<'rt>, Fault> {
    let vm_code = runtime.heap.code_store.get(code_id);
    let argc = args.len();
    let closure = vm_code.registers + usize::from(target != code);
    if closure < frame.slots.len()
        && vm_code.arity.required == argc
        && vm_code.arity.maximum == Some(argc)
    {
        if let Some(invocation) = try_native_call(
            runtime,
            code_id,
            &vm_code,
            target,
            &frame.slots,
            args.clone(),
            values,
            frame.resume,
            true,
        ) {
            return Ok(InstructionAction::TailInvoke(invocation));
        }
        let at = frame.logical_at(vm_pc);
        replace_tail_frame(frame, target, code, vm_code, closure, args, at);
        return Ok(InstructionAction::Continue);
    }
    Ok(InstructionAction::TailCall {
        target,
        args: TailArguments::Registers(args),
        at: frame.logical_at(vm_pc),
    })
}

fn replace_tail_frame(
    frame: &mut Frame<'_>,
    target: Word,
    code: Word,
    vm_code: std::rc::Rc<VmCode>,
    closure: Register,
    args: Range<Register>,
    at: usize,
) {
    let argc = args.len();
    let registers = vm_code.registers;
    // There is no collection between the reads and writes. Clear even the
    // unused capacity left by a larger caller, so it does not retain old roots.
    frame.slots.move_to_start(args);
    frame.clear(argc..registers);
    frame.slots.set(registers, code);
    frame.slots.set(closure, target);
    frame.clear(closure + 1..frame.slots.len());
    // Replacing the activation keeps its caller's dynamic scope boundary.
    // In particular, frameless continuation scopes can share frame index zero.
    frame.vm_code = vm_code;
    frame.code = registers;
    frame.closure = closure;
    frame.pc = 0;
    frame.arguments = Default::default();
    frame.call_at = at;
}

pub(super) fn prepare_canonical_tail_call<'rt>(
    runtime: &'rt Runtime,
    frame: &mut Frame<'rt>,
    frame_index: usize,
    vm_pc: usize,
    pc: usize,
    values: &mut Values,
    control: &mut ControlStack<'rt>,
) -> Result<InstructionAction<'rt>, Fault> {
    match frame.vm_code.canonical[pc] {
        Instruction::Call {
            callee, ref args, ..
        } => prepare_tail_call(
            runtime,
            frame,
            frame_index,
            vm_pc,
            callee,
            args.clone(),
            values,
            control,
        ),
        Instruction::CallValues { callee, at } if !control.has_frame_scopes(frame_index) => {
            let target = designator(runtime, frame.slots.get(callee), at)?;
            let arity = frame.vm_code.arity;
            if target == frame.slots.get(frame.closure) && arity.accepts(values.len) {
                frame.arguments = ArgumentTail::default();
                frame.arguments =
                    ArgumentTail::new(runtime, values.len - arity.required, |index| {
                        values.get(arity.required + index)
                    });
                for index in 0..arity.required {
                    frame.slots.set(index, values.get(index));
                }
                frame.clear(arity.required..frame.code);
                values.resize(0);
                frame.call_at = at;
                frame.pc = 0;
                return Ok(InstructionAction::Continue);
            }
            Ok(InstructionAction::TailCall {
                target,
                args: TailArguments::Values,
                at,
            })
        }
        Instruction::CallValues { .. } => {
            exec_slow(runtime, frame, frame_index, vm_pc, pc, values, control)
        }
        _ => unreachable!("a canonical tail call refers to a call instruction"),
    }
}

pub(super) fn dispatch_tail_call<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    target: Word,
    args: TailArguments,
    at: usize,
    preserve_frame: bool,
) -> Result<Invocation<'rt>, Fault> {
    let Some(ExecFrame::Bytecode(frame)) = exec.frames.last() else {
        unreachable!("a tail call replaces a bytecode frame");
    };
    // Stage arguments before removing the current frame or clearing Values.
    let staged = match args {
        TailArguments::Registers(args) => stage_call(runtime, target, &frame.slots, args),
        TailArguments::Values => {
            let staged = stage_call(runtime, target, &exec.values.slots, 0..exec.values.len);
            exec.values.resize(0);
            staged
        }
    };
    let resume = if preserve_frame {
        Resume::Call(None)
    } else {
        let resume = frame.resume;
        exec.frames.pop();
        resume
    };
    invoke(
        runtime,
        staged.get(0),
        &staged,
        1..staged.len(),
        &mut exec.values,
        resume,
        at,
    )
}

fn update_call_feedback(runtime: &Runtime, site: CallSite<'_>, target: Word, argc: usize) {
    if let Some(op) = Primitive::decode(target) {
        if op.is_direct_leaf() && op.accepts(argc) {
            site.record(SiteFeedback::CallPrimitive { prim: op });
        } else {
            site.record(SiteFeedback::Megamorphic);
        }
        return;
    }
    if runtime.heap.generic_info(target).is_none()
        && target.native_index().is_none()
        && let Some(code) = runtime.heap.function_code(target)
        && target == code
        && let Some(info) = runtime.heap.function_info(code)
    {
        let vm_code = runtime.heap.code_store.get(info.code_id);
        let arity = vm_code.arity;
        if arity.required == argc && arity.maximum == Some(argc) {
            site.record(SiteFeedback::CallLispFixed {
                gc_epoch: runtime.heap.gc_epoch(),
                code,
                code_id: info.code_id,
            });
            return;
        }
    }
    site.record(SiteFeedback::Megamorphic);
}

enum CallTarget<'a> {
    Fixed { code: Word, code_id: CodeId },
    Primitive(Primitive),
    Arithmetic(Primitive, CallSite<'a>),
    ArrayReader(CallSite<'a>),
    General(Word),
}

fn resolve_named_call<'a>(
    runtime: &Runtime,
    frame: &'a Frame<'_>,
    vm_pc: usize,
    symbol: usize,
    args: &Range<Register>,
) -> Result<CallTarget<'a>, Fault> {
    let site = CallSite::lookup(runtime, frame, vm_pc, symbol);
    let stale = match site.observed {
        SiteFeedback::CallLispFixed {
            gc_epoch,
            code,
            code_id,
        } if gc_epoch == runtime.heap.gc_epoch() => return Ok(CallTarget::Fixed { code, code_id }),
        SiteFeedback::CallPrimitive { prim } => return Ok(CallTarget::Primitive(prim)),
        SiteFeedback::SimdI32x4 { prim, .. } => {
            observe_i32x4(runtime, &frame.slots, site, prim, args.start);
            return Ok(CallTarget::Primitive(prim));
        }
        SiteFeedback::ArithPrimitive { prim, .. } => return Ok(CallTarget::Arithmetic(prim, site)),
        SiteFeedback::ArrayReader { .. } => return Ok(CallTarget::ArrayReader(site)),
        SiteFeedback::Megamorphic => false,
        _ => true,
    };
    let symbol = frame.constant(runtime, symbol);
    let target = runtime
        .heap
        .symbol_cell(symbol, true)
        .ok_or_else(|| undefined_function(runtime, symbol, frame.logical_at(vm_pc)))?;
    if stale {
        let argc = args.len();
        if argc == 2 {
            match Primitive::decode(target) {
                Some(Primitive::Array(arrays::Op::Aref)) => {
                    return Ok(CallTarget::ArrayReader(site));
                }
                Some(prim) if is_binary_numeric(prim) => {
                    return Ok(CallTarget::Arithmetic(prim, site));
                }
                Some(prim @ Primitive::Simd(_))
                    if observe_i32x4(runtime, &frame.slots, site, prim, args.start) =>
                {
                    return Ok(CallTarget::Primitive(prim));
                }
                _ => {}
            }
        }
        update_call_feedback(runtime, site, target, argc);
    }
    Ok(CallTarget::General(target))
}

#[inline]
pub(super) fn observe_i32x4(
    runtime: &Runtime,
    slots: &RootSlots,
    site: CallSite<'_>,
    prim: Primitive,
    first: Register,
) -> bool {
    if matches!(site.observed, SiteFeedback::SimdI32x4 { .. }) {
        if i32x4_pair(runtime, slots, first) {
            return true;
        }
        site.record(SiteFeedback::CallPrimitive { prim });
        return false;
    }
    if matches!(site.observed, SiteFeedback::Cold)
        && cfg!(all(
            target_endian = "little",
            any(target_arch = "aarch64", target_arch = "x86_64")
        ))
        && let Primitive::Simd(op) = prim
        && let Some(operation) = i32x4_operation(op)
        && i32x4_pair(runtime, slots, first)
    {
        site.record(SiteFeedback::SimdI32x4 { prim, operation });
        return true;
    }
    false
}

fn i32x4_pair(runtime: &Runtime, slots: &RootSlots, first: Register) -> bool {
    let expected = PackShape::new(LaneKind::I32, 4);
    runtime.heap.simd_pack_shape(slots.get(first)) == expected
        && runtime.heap.simd_pack_shape(slots.get(first + 1)) == expected
}

fn i32x4_operation(op: simd::Op) -> Option<I32x4Operation> {
    match op {
        simd::Op::Add => Some(I32x4Operation::Add),
        simd::Op::Sub => Some(I32x4Operation::Sub),
        simd::Op::BitAnd => Some(I32x4Operation::BitAnd),
        simd::Op::BitOr => Some(I32x4Operation::BitOr),
        simd::Op::BitXor => Some(I32x4Operation::BitXor),
        _ => None,
    }
}

pub(super) fn call_named_range<'rt>(
    runtime: &'rt Runtime,
    frame: &Frame<'rt>,
    vm_pc: usize,
    sym: usize,
    args: Range<Register>,
    out: Register,
    values: &mut Values,
) -> Result<Option<Invocation<'rt>>, Fault> {
    let at = || frame.logical_at(vm_pc);
    let target = match resolve_named_call(runtime, frame, vm_pc, sym, &args)? {
        CallTarget::Fixed { code, code_id } => {
            let vm_code = if code_id == frame.vm_code.id {
                frame.vm_code.clone()
            } else {
                runtime.heap.code_store.get(code_id)
            };
            let resume = Resume::Call(Some(out));
            let call = try_native_call(
                runtime,
                code_id,
                &vm_code,
                code,
                &frame.slots,
                args.clone(),
                values,
                resume,
                true,
            )
            .unwrap_or_else(|| {
                Invocation::Lisp(Frame::new_cached(
                    runtime,
                    code,
                    vm_code,
                    &frame.slots,
                    args,
                    resume,
                    at(),
                ))
            });
            return Ok(finish_single_result_call(frame, call, values));
        }
        CallTarget::Primitive(prim) => {
            crate::metrics::record_primitive_call();
            primitive(runtime, &frame.slots, prim, args, values, at())?;
            values.bind(&frame.slots, out..out + 1);
            return Ok(None);
        }
        CallTarget::Arithmetic(prim, site) => {
            crate::metrics::record_primitive_call();
            if let SiteFeedback::ArithPrimitive {
                operands: operands @ ArithOperands::Pair(..),
                ..
            } = site.observed
            {
                let result = super::numeric_feedback::profiled_binary_result(
                    runtime,
                    site,
                    prim,
                    Some(operands),
                    &frame.slots,
                    args,
                    at,
                )?;
                frame.slots.set(out, result);
                values.resize(0);
            } else {
                exec_numeric_primitive(runtime, site, prim, &frame.slots, args, values, at())?;
                values.bind(&frame.slots, out..out + 1);
            }
            return Ok(None);
        }
        CallTarget::ArrayReader(site) => {
            array_access::exec_named_aref(runtime, frame, site, args.start, values, at)?;
            values.bind(&frame.slots, out..out + 1);
            return Ok(None);
        }
        CallTarget::General(target) => target,
    };
    let invocation = invoke(
        runtime,
        target,
        &frame.slots,
        args,
        values,
        Resume::Call(Some(out)),
        at(),
    )?;
    Ok(finish_checked_single_result_call(
        runtime, frame, invocation, values,
    ))
}
