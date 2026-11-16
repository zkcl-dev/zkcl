use std::ops::Range;

use crate::EvalError;
use crate::heap::{RootSlots, Word};

use super::super as vm;
use super::{ArithKind, CmpKind, VmOp};

#[inline(always)]
fn checked_fixnum_arith(kind: ArithKind, va: Word, vb: Word) -> Option<Word> {
    match kind {
        ArithKind::Add => va.checked_fixnum_add(vb),
        ArithKind::Sub => va.checked_fixnum_sub(vb),
        ArithKind::Mul => va.checked_fixnum_mul(vb),
    }
}

/// Guarded comparison + branch fast path.
#[inline(always)]
pub(crate) fn exec_cmp_branch_fast(kind: CmpKind, wa: Word, wb: Word) -> Option<bool> {
    match kind {
        CmpKind::Eq => Some(wa == wb),
        CmpKind::Eql => {
            if wa == wb {
                Some(true)
            } else if wa.is_immediate() || wb.is_immediate() {
                Some(false)
            } else {
                None
            }
        }
        _ => {
            let (a, b) = wa.ordered_fixnum_pair(wb)?;
            Some(match kind {
                CmpKind::EqNum => a == b,
                CmpKind::NeNum => a != b,
                CmpKind::Lt => a < b,
                CmpKind::Le => a <= b,
                CmpKind::Gt => a > b,
                CmpKind::Ge => a >= b,
                CmpKind::Eq | CmpKind::Eql => unreachable!(),
            })
        }
    }
}

struct Dispatch<'rt, 'a> {
    runtime: &'rt crate::Runtime,
    frame: &'a mut vm::Frame<'rt>,
    frame_index: usize,
    vm_pc: usize,
    values: &'a mut vm::Values,
    control: &'a mut vm::ControlStack<'rt>,
}

impl<'rt, 'a> Dispatch<'rt, 'a> {
    /// The operand context every op that can leave the dispatch loop hands on.
    #[inline(always)]
    fn at(
        runtime: &'rt crate::Runtime,
        frame: &'a mut vm::Frame<'rt>,
        frame_index: usize,
        vm_pc: usize,
        values: &'a mut vm::Values,
        control: &'a mut vm::ControlStack<'rt>,
    ) -> Self {
        Self {
            runtime,
            frame,
            frame_index,
            vm_pc,
            values,
            control,
        }
    }

    #[inline(always)]
    fn call_named(
        &mut self,
        out: u16,
        sym: u32,
        range: Range<usize>,
    ) -> Result<vm::InstructionAction<'rt>, vm::Fault> {
        if let Some(invocation) = vm::call_named_range(
            self.runtime,
            self.frame,
            self.vm_pc,
            sym as usize,
            range,
            out as usize,
            self.values,
        )? {
            return Ok(vm::InstructionAction::Invoke(invocation));
        }
        Ok(vm::InstructionAction::Continue)
    }

    #[inline(always)]
    fn arith2(
        &mut self,
        kind: ArithKind,
        out: u16,
        first: usize,
        sym: u32,
        fast: impl FnOnce(&RootSlots) -> Option<Word>,
    ) -> Result<vm::InstructionAction<'rt>, vm::Fault> {
        let site = vm::CallSite::lookup(self.runtime, self.frame, self.vm_pc, sym as usize);
        if let Some((prim, previous)) = vm::cached_primitive(site) {
            let result = if kind.matches_primitive(prim)
                && let Some(result) = fast(&self.frame.slots)
            {
                vm::numeric_feedback::record_fixnums(site, prim, previous);
                result
            } else {
                crate::metrics::record_primitive_call();
                vm::exec_cached_primitive_pair(
                    self.runtime,
                    self.frame,
                    site,
                    prim,
                    previous,
                    first,
                    self.values,
                    || self.frame.logical_at(self.vm_pc),
                )?
            };
            self.frame.slots.set(out as usize, result);
            self.values.resize(0);
        } else {
            crate::metrics::record_slow_path();
            if let Some(invocation) = vm::slow_arith2(
                self.runtime,
                self.frame,
                self.vm_pc,
                sym as usize,
                first,
                out as usize,
                self.values,
            )? {
                return Ok(vm::InstructionAction::Invoke(invocation));
            }
        }
        Ok(vm::InstructionAction::Continue)
    }

    #[inline(always)]
    fn arith_imm(
        &mut self,
        kind: ArithKind,
        out: u16,
        src: u16,
        literal: u16,
        imm: i32,
        sym: u32,
    ) -> Result<vm::InstructionAction<'rt>, vm::Fault> {
        let value = Word::fixnum(imm as isize).expect("validated fixnum immediate");
        self.frame.slots.set(literal as usize, value);
        let first = src.min(literal) as usize;
        self.arith2(kind, out, first, sym, |slots| {
            checked_fixnum_arith(kind, slots.get(src as usize), value)
        })
    }

    #[inline(always)]
    fn tail_call(
        &mut self,
        callee: vm::Callee,
        start: u16,
        argc: u16,
    ) -> Result<vm::InstructionAction<'rt>, vm::Fault> {
        let start = start as usize;
        vm::call_site::prepare_tail_call(
            self.runtime,
            self.frame,
            self.frame_index,
            self.vm_pc,
            callee,
            start..start + argc as usize,
            self.values,
            self.control,
        )
    }

    #[inline(always)]
    fn car_cdr_fast(
        &mut self,
        is_car: bool,
        out: u16,
        src: u16,
        sym: u32,
    ) -> Result<vm::InstructionAction<'rt>, vm::Fault> {
        let slots = &self.frame.slots;
        let builtin = if is_car {
            vm::UnaryBuiltin::Car
        } else {
            vm::UnaryBuiltin::Cdr
        };
        if vm::cached_unary_builtin(self.runtime, self.frame, self.vm_pc, sym as usize)
            && let Some(elem) = self.runtime.heap.list_field(slots.get(src as usize), is_car)
        {
            slots.set(out as usize, elem);
            self.values.resize(0);
        } else {
            crate::metrics::record_slow_path();
            let invocation = vm::slow_unary_builtin(
                self.runtime,
                self.frame,
                self.vm_pc,
                sym as usize,
                src as usize,
                builtin,
                self.values,
                vm::Resume::Call(Some(out as usize)),
            )?;
            if let Some(invocation) =
                vm::finish_single_result_call(self.frame, invocation, self.values)
            {
                return Ok(vm::InstructionAction::Invoke(invocation));
            }
        }
        Ok(vm::InstructionAction::Continue)
    }
}

#[inline(always)]
pub(in crate::vm) fn dispatch_vm_op<'rt>(
    runtime: &'rt crate::Runtime,
    frame: &mut vm::Frame<'rt>,
    frame_index: usize,
    vm_pc: usize,
    op: VmOp,
    values: &mut vm::Values,
    control: &mut vm::ControlStack<'rt>,
) -> Result<vm::InstructionAction<'rt>, vm::Fault> {
    let slots = &frame.slots;
    match op {
        VmOp::LoadNil { out } => slots.set(out as usize, Word::NIL),
        VmOp::LoadTrue { out } => slots.set(out as usize, Word::T),
        VmOp::LoadFixnum { out, value } => {
            let word = Word::fixnum(value as isize).ok_or_else(|| {
                EvalError::new(
                    frame.call_at,
                    "immediate fixnum is outside the supported range",
                )
            })?;
            slots.set(out as usize, word);
        }
        VmOp::LoadChar { out, ch } => slots.set(out as usize, Word::character(ch)),
        VmOp::Constant { out, idx } => {
            slots.set(out as usize, frame.constant(runtime, idx as usize))
        }
        VmOp::Copy { out, src } => slots.set(out as usize, slots.get(src as usize)),
        VmOp::Clear { start, len } => {
            let s = start as usize;
            frame.clear(s..s + len as usize);
        }
        VmOp::ClearPair {
            union_start,
            union_len,
            ..
        } => {
            let start = usize::from(union_start);
            frame.clear(start..start + usize::from(union_len));
            frame.pc += 1;
        }
        VmOp::Jump { pc } => frame.pc = pc as usize,
        VmOp::Unless { test, pc } => {
            if slots.get(test as usize) == Word::NIL {
                frame.pc = pc as usize;
            }
        }
        VmOp::IfPresent { reg, pc } => {
            if slots.get(reg as usize) != Word::UNBOUND {
                frame.pc = pc as usize;
            }
        }
        VmOp::UnlessFixnum { reg, pc } => {
            if runtime
                .target_fixnum_value(slots.get(reg as usize))
                .is_none()
            {
                frame.pc = pc as usize;
            }
        }
        VmOp::UnlessFixnumInRange { reg, low, high, pc } => {
            if !runtime
                .target_fixnum_value(slots.get(reg as usize))
                .is_some_and(|n| (i64::from(low)..=i64::from(high)).contains(&n))
            {
                frame.pc = pc as usize;
            }
        }
        VmOp::TryFixnumArithmetic {
            op,
            symbol,
            out,
            start,
            len,
        } => {
            debug_assert_eq!(usize::from(len), op.arity());
            let result =
                vm::try_fixnum_arithmetic(runtime, frame, op, symbol as usize, start as usize);
            slots.set(out as usize, result.unwrap_or(Word::NIL));
        }
        VmOp::SetValues1 { src } => values.one(slots.get(src as usize)),
        VmOp::SetValues { start, len } => {
            let start = start as usize;
            values.select(slots, start..start + len as usize);
        }
        VmOp::BindValues1 { out } => {
            slots.set(out as usize, values.get(0));
            if values.len != 0 {
                values.resize(0);
            }
        }
        VmOp::BindValues { start, len } => {
            let start = start as usize;
            values.bind(slots, start..start + len as usize);
        }
        VmOp::OptionalArgument { index, out } => {
            slots.set(out as usize, frame.arguments.get(index as usize));
        }
        VmOp::KeywordArgument { index, out } => {
            slots.set(
                out as usize,
                frame.arguments.keyword(index as usize, frame.call_at)?,
            );
        }
        VmOp::ReleaseArguments {} => frame.arguments = Default::default(),
        VmOp::ReadCell { out, sym, function } => {
            let symbol = frame.constant(runtime, sym as usize);
            let value = vm::read_symbol_cell(runtime, symbol, function, || frame.logical_at(vm_pc))?;
            slots.set(out as usize, value);
        }
        VmOp::WriteCell { sym, src } => {
            let symbol = frame.constant(runtime, sym as usize);
            runtime
                .heap
                .set_symbol_cell(symbol, false, slots.get(src as usize));
        }
        VmOp::BindSpecial { symbol, value } => {
            frame.bind_special(
                runtime,
                control,
                frame_index,
                symbol as usize,
                value as usize,
            );
        }
        VmOp::Unbind { count } => control.unbind(count as usize),
        VmOp::BoxLocal { reg } => {
            frame
                .box_local(runtime, reg as usize)
                .map_err(|error| EvalError::out_of_memory(frame.logical_at(vm_pc), error))?;
        }
        VmOp::Closure {
            out,
            code,
            start,
            len,
        } => {
            let closure = frame
                .make_closure(runtime, code as usize, start as usize, len as usize)
                .map_err(|error| EvalError::out_of_memory(frame.logical_at(vm_pc), error))?;
            slots.set(out as usize, closure);
        }
        VmOp::ReadBoxLocal { out, cell } => {
            let value = frame.read_box(runtime, vm::CellLocation::Local(cell as usize))?;
            slots.set(out as usize, value);
        }
        VmOp::Capture { out, capture } => {
            let value = frame.cell(runtime, vm::CellLocation::Capture(capture as usize))?;
            slots.set(out as usize, value);
        }
        VmOp::ReadBoxCapture { out, capture } => {
            let value = frame.read_box(runtime, vm::CellLocation::Capture(capture as usize))?;
            slots.set(out as usize, value);
        }
        VmOp::WriteBoxLocal { cell, src } => {
            let cell_word = slots.get(cell as usize);
            runtime
                .heap
                .write_cell(cell_word, slots.get(src as usize))
                .ok_or_else(|| frame.invalid_operand("expected a lexical cell"))?;
        }
        VmOp::WriteBoxCapture { capture, src } => {
            let cell_word = runtime
                .heap
                .capture(slots.get(frame.closure), capture as usize)
                .ok_or_else(|| frame.invalid_operand("closure capture is out of bounds"))?;
            runtime
                .heap
                .write_cell(cell_word, slots.get(src as usize))
                .ok_or_else(|| frame.invalid_operand("expected a lexical cell"))?;
        }
        VmOp::CallNamed0 { out, sym } => {
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.call_named(out, sym, 0..0);
        }
        VmOp::CallNamed1 { out, sym, arg0 } => {
            let a = arg0 as usize;
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.call_named(out, sym, a..a + 1);
        }
        VmOp::CallNamed2 {
            out,
            sym,
            arg_start,
        } => {
            let a = arg_start as usize;
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.call_named(out, sym, a..a + 2);
        }
        VmOp::CallNamed3 {
            out,
            sym,
            arg_start,
        } => {
            let a = arg_start as usize;
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.call_named(out, sym, a..a + 3);
        }
        VmOp::CallNamedVariadic {
            out,
            sym,
            arg_start,
            argc,
        } => {
            let a = arg_start as usize;
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.call_named(out, sym, a..a + argc as usize);
        }
        VmOp::TailCallNamed { sym, start, argc } => {
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.tail_call(vm::Callee::Named(sym as usize), start, argc);
        }
        VmOp::TailCallValue {
            callee,
            start,
            argc,
        } => {
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.tail_call(vm::Callee::Value(callee as usize), start, argc);
        }
        VmOp::TailCallCanonical { pc } => {
            return vm::call_site::prepare_canonical_tail_call(
                runtime,
                frame,
                frame_index,
                vm_pc,
                pc as usize,
                values,
                control,
            );
        }
        VmOp::CmpBranch {
            kind,
            a,
            sym,
            out,
            clear_start,
            clear_len,
            target,
        } => {
            return vm::exec_cmp_branch(
                runtime,
                frame,
                vm_pc,
                kind,
                sym as usize,
                a as usize,
                vm::FusedBranch {
                    target,
                    out,
                    clear_start,
                    clear_len,
                },
                values,
            );
        }
        VmOp::NullBranch {
            reg,
            sym,
            out,
            clear_start,
            clear_len,
            target,
        } => {
            let branch = vm::FusedBranch {
                target,
                out,
                clear_start,
                clear_len,
            };
            if vm::cached_unary_builtin(runtime, frame, vm_pc, sym as usize) {
                let result = if slots.get(reg as usize) == Word::NIL {
                    Word::T
                } else {
                    Word::NIL
                };
                branch.finish(frame, result, values);
            } else {
                crate::metrics::record_slow_path();
                let invocation = vm::slow_unary_builtin(
                    runtime,
                    frame,
                    vm_pc,
                    sym as usize,
                    reg as usize,
                    vm::UnaryBuiltin::Null,
                    values,
                    vm::Resume::BranchUnless(branch),
                )?;
                return Ok(vm::InstructionAction::Invoke(invocation));
            }
        }
        VmOp::ArithFixnumGuarded {
            kind,
            out,
            first,
            sym,
        } => {
            let first = first as usize;
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.arith2(kind, out, first, sym, |slots| {
                checked_fixnum_arith(kind, slots.get(first), slots.get(first + 1))
            });
        }
        VmOp::AddFixnumImm {
            out,
            src,
            literal,
            imm,
            sym,
        } => {
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.arith_imm(ArithKind::Add, out, src, literal, imm, sym);
        }
        VmOp::SubFixnumImm {
            out,
            src,
            literal,
            imm,
            sym,
        } => {
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.arith_imm(ArithKind::Sub, out, src, literal, imm, sym);
        }
        VmOp::MulFixnumImm {
            out,
            src,
            literal,
            imm,
            sym,
        } => {
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.arith_imm(ArithKind::Mul, out, src, literal, imm, sym);
        }
        VmOp::CarFast { out, src, sym } => {
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.car_cdr_fast(true, out, src, sym);
        }
        VmOp::CdrFast { out, src, sym } => {
            let mut dispatch = Dispatch::at(runtime, frame, frame_index, vm_pc, values, control);
            return dispatch.car_cdr_fast(false, out, src, sym);
        }
        VmOp::ConsFast { out, first, sym } => {
            let first = first as usize;
            if let crate::code_store::SiteFeedback::CallPrimitive {
                prim: vm::Primitive::List(vm::list::Op::Cons),
            } = vm::CallSite::lookup(runtime, frame, vm_pc, sym as usize).observed
            {
                let pair = runtime
                    .heap
                    .cons(|| (slots.get(first), slots.get(first + 1)))
                    .map_err(|error| EvalError::out_of_memory(frame.logical_at(vm_pc), error))?;
                slots.set(out as usize, pair);
                values.resize(0);
            } else {
                crate::metrics::record_slow_path();
                if let Some(invocation) = vm::slow_cons(
                    runtime,
                    frame,
                    vm_pc,
                    sym as usize,
                    first,
                    out as usize,
                    values,
                )? {
                    return Ok(vm::InstructionAction::Invoke(invocation));
                }
            }
        }
        VmOp::ClearBackedge { start, len, target } => {
            let start = start as usize;
            frame.clear(start..start + len as usize);
            frame.pc = target as usize;
        }
        VmOp::Slow { pc } => {
            return vm::exec_slow(
                runtime,
                frame,
                frame_index,
                vm_pc,
                pc as usize,
                values,
                control,
            );
        }
    }
    Ok(vm::InstructionAction::Continue)
}
