use std::ops::Range;

use super::call_site::CallSite;
use super::{Primitive, Values, numbers, numbers::Op, primitive};
use crate::Runtime;
use crate::code_store::{ArithOperands, SiteFeedback};
use crate::condition::Fault;
use crate::heap::{Heap, RootSlots, Word};
use crate::native::JitNumericOperand;
use crate::number::MachineReal;

pub(super) fn is_binary_numeric(prim: Primitive) -> bool {
    matches!(
        prim,
        Primitive::Number(
            Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Eq
                | Op::Ne
                | Op::Lt
                | Op::Le
                | Op::Gt
                | Op::Ge
        )
    )
}

fn operand(value: MachineReal) -> JitNumericOperand {
    match value {
        MachineReal::Fixnum(_) => JitNumericOperand::Fixnum,
        MachineReal::Single(_) => JitNumericOperand::F32,
        MachineReal::Double(_) => JitNumericOperand::F64,
    }
}

fn sample([lhs, rhs]: [Option<MachineReal>; 2]) -> ArithOperands {
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => ArithOperands::Pair(operand(lhs), operand(rhs)),
        _ => ArithOperands::Polymorphic,
    }
}

pub(super) fn record(
    site: CallSite<'_>,
    prim: Primitive,
    previous: Option<ArithOperands>,
    observed: ArithOperands,
) {
    let operands = previous.map_or(observed, |previous| previous.merge(observed));
    if Some(operands) != previous {
        site.record(SiteFeedback::ArithPrimitive { prim, operands });
    }
}

#[inline(always)]
pub(super) fn record_fixnums(site: CallSite<'_>, prim: Primitive, previous: Option<ArithOperands>) {
    if !matches!(
        previous,
        Some(ArithOperands::FIXNUMS | ArithOperands::Polymorphic)
    ) {
        record(site, prim, previous, ArithOperands::FIXNUMS);
    }
}

#[inline]
pub(super) fn compare_machine_reals(
    heap: &Heap,
    site: CallSite<'_>,
    prim: Primitive,
    previous: Option<ArithOperands>,
    [left, right]: [Word; 2],
) -> Option<bool> {
    let Primitive::Number(op @ (Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge)) = prim else {
        return None;
    };
    let left = heap.machine_real(left)?;
    let right = heap.machine_real(right)?;
    let result = numbers::machine_comparison(op, left, right)?;
    if previous != Some(ArithOperands::Polymorphic) {
        record(
            site,
            prim,
            previous,
            ArithOperands::Pair(operand(left), operand(right)),
        );
    }
    Some(result)
}

pub(super) fn exec_numeric_primitive(
    runtime: &Runtime,
    site: CallSite<'_>,
    prim: Primitive,
    slots: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let previous = match site.observed {
        SiteFeedback::ArithPrimitive { operands, .. } => Some(operands),
        _ => None,
    };
    if previous == Some(ArithOperands::Polymorphic) {
        return primitive(runtime, slots, prim, args, values, at);
    }
    let result = profiled_binary_result(runtime, site, prim, previous, slots, args, || at)?;
    values.one(result);
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "VM numeric feedback helper receives the observation and deferred location callback"
)]
pub(super) fn profiled_binary_result(
    runtime: &Runtime,
    site: CallSite<'_>,
    prim: Primitive,
    previous: Option<ArithOperands>,
    slots: &RootSlots,
    args: Range<usize>,
    at: impl FnOnce() -> usize,
) -> Result<Word, Fault> {
    let Primitive::Number(op) = prim else {
        unreachable!("feedback is attached only to numeric primitive calls");
    };
    let operands = [
        runtime.heap.machine_real(slots.get(args.start)),
        runtime.heap.machine_real(slots.get(args.start + 1)),
    ];
    let mut observed = sample(operands);
    let result = numbers::run_decoded_binary(runtime, slots, op, args, at, operands);
    if observed == ArithOperands::FIXNUMS
        && matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div)
        && !matches!(&result, Ok(word) if word.as_fixnum().is_some())
    {
        // Promoted integers and exact quotients require the general arithmetic path.
        observed = ArithOperands::Polymorphic;
    }
    record(site, prim, previous, observed);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::Name;

    #[test]
    fn specialized_numeric_calls_use_the_bound_primitive_on_each_hit() {
        let rt = Runtime::start().unwrap();
        let source = "(lambda (x y) (list (+ x y) (+ 9 x) (+ x 9)))";
        let arithmetic = rt.eval(source).unwrap().pop().unwrap();
        let comparison = rt
            .eval("(lambda (x y) (catch 'escape (if (< x y) :less :other)))")
            .unwrap()
            .pop()
            .unwrap();
        let x = rt.fixnum(8).unwrap();
        let y = rt.fixnum(3).unwrap();
        let run = |function| {
            let result = rt.call(function, &[&x, &y]).unwrap();
            rt.repr(&result[0]).unwrap()
        };
        assert_eq!(run(&arithmetic), "(11 17 17)");
        assert_eq!(run(&comparison), ":OTHER");

        let add_symbol = rt.root(rt.find_named_symbol(&Name::common_lisp("+")).unwrap());
        let lt_symbol = rt.root(rt.find_named_symbol(&Name::common_lisp("<")).unwrap());
        rt.heap
            .set_symbol_cell(add_symbol.slot.get(), true, Primitive::named("-").unwrap());
        rt.heap
            .set_symbol_cell(lt_symbol.slot.get(), true, Primitive::named(">").unwrap());
        for _ in 0..2 {
            assert_eq!(run(&arithmetic), "(5 1 -1)");
            assert_eq!(run(&comparison), ":LESS");
        }

        let escape = rt
            .eval("(lambda (x y) (if x (throw 'escape y) (values)))")
            .unwrap()
            .pop()
            .unwrap();
        rt.heap
            .set_symbol_cell(lt_symbol.slot.get(), true, escape.slot.get());
        assert_eq!(run(&comparison), "3");
        let result = rt.call(&comparison, &[&rt.nil(), &y]).unwrap();
        assert_eq!(rt.repr(&result[0]).unwrap(), ":OTHER");

        let immediate = "(lambda (x y) (list (+ 9 x) y))";
        let immediate_call = rt.eval(immediate).unwrap().pop().unwrap();
        rt.heap
            .set_symbol_cell(add_symbol.slot.get(), true, Word::UNBOUND);
        let fault = rt.call(&immediate_call, &[&x, &y]).err().unwrap();
        assert_eq!(fault.offset(), immediate.find("+ 9 x").unwrap());
    }
}
