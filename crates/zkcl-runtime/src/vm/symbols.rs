use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};

use num_bigint::Sign;

use crate::condition::{Class, Fault};
use crate::heap::{RootSlots, Word};
use crate::number::{Number, Real};
use crate::symbol::string_text;
use crate::{EvalError, Runtime};

static GENTEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub(super) fn make_symbol(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    let name = string_text(runtime, value).ok_or_else(|| {
        Fault::type_error(runtime, 
            value,
            "STRING",
            EvalError::new(at, "expected a symbol name string"),
        )
    })?;
    Ok(runtime.fresh_symbol(&name, at)?.1)
}

pub(super) fn symbol_name(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    runtime
        .symbol_name_word(value, at)?
        .ok_or_else(|| expected_symbol(runtime, value, at))
}

pub(super) fn symbol_package(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    let data = runtime
        .heap
        .symbol_attributes(value)
        .ok_or_else(|| expected_symbol(runtime, value, at))?;
    Ok(data.home.map_or(Word::NIL, crate::package::Package::word))
}

pub(super) fn gensym(runtime: &Runtime, arg: Option<Word>, at: usize) -> Result<Word, Fault> {
    let prefix = match arg {
        None => "G".into(),
        Some(value) => {
            if let Some(suffix) = runtime
                .heap
                .integer(value)
                .filter(|suffix| suffix.sign() != Sign::Minus)
            {
                return Ok(runtime.fresh_symbol(&format!("G{suffix}"), at)?.1);
            }
            runtime.heap.string_contents(value).ok_or_else(|| {
                Fault::gensym_argument(runtime, 
                    value,
                    EvalError::new(at, "GENSYM expects a string or a non-negative integer"),
                )
            })?
        }
    };
    let counter = runtime.root_slot(runtime.intern_common_lisp("*GENSYM-COUNTER*", at)?);
    let current = super::symbol_value(runtime, counter.get(), at)?;
    let suffix = runtime
        .heap
        .integer(current)
        .filter(|suffix| suffix.sign() != Sign::Minus)
        .ok_or_else(|| {
            Fault::non_negative_integer(runtime, 
                current,
                EvalError::new(at, "*GENSYM-COUNTER* must be a non-negative integer"),
            )
        })?;
    let next = &suffix + 1u8;
    if next.bits() > runtime.heap.max_integer_bits() {
        return Err(Fault::plain(
            Class::ArithmeticError,
            EvalError::new(at, "*GENSYM-COUNTER* exceeds the supported integer range"),
        ));
    }
    let next_word = runtime.root_slot(
        runtime
            .heap
            .allocate_number(&Number::Real(Real::Integer(next)))
            .map_err(|error| EvalError::out_of_memory(at, error))?,
    );
    let (_, result) = runtime.fresh_symbol(&format!("{prefix}{suffix}"), at)?;
    runtime
        .heap
        .set_symbol_cell(counter.get(), false, next_word.get());
    Ok(result)
}

pub(super) fn copy_symbol(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let copy_properties = args.len() == 2 && frame.get(args.start + 1) != Word::NIL;
    let symbol = frame.get(args.start);
    let name = runtime
        .heap
        .symbol_name(symbol)
        .ok_or_else(|| expected_symbol(runtime, symbol, at))?;
    let copy = runtime.root_slot(runtime.fresh_symbol(&name, at)?.1);
    if copy_properties {
        let symbol = frame.get(args.start);
        if let Some(value) = runtime.heap.symbol_cell(symbol, false) {
            runtime.heap.set_symbol_cell(copy.get(), false, value);
        }
        if let Some(function) = runtime.heap.symbol_cell(symbol, true) {
            runtime.heap.set_symbol_cell(copy.get(), true, function);
        }
        let plist = runtime.symbol_plist(symbol);
        if plist != Word::NIL {
            let copied = super::list::copy_list(runtime, plist, at)?;
            runtime.set_symbol_plist(copy.get(), copied);
        }
    }
    Ok(copy.get())
}

pub(super) fn gentemp(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let prefix = if args.is_empty() {
        "T".to_owned()
    } else {
        let word = frame.get(args.start);
        runtime.heap.string_contents(word).ok_or_else(|| {
            Fault::type_error(runtime, 
                word,
                "STRING",
                EvalError::new(at, "GENTEMP expects a string prefix"),
            )
        })?
    };
    let package_arg = (args.len() == 2).then(|| frame.get(args.start + 1));
    let package = super::packages::resolve_package(runtime, package_arg, at)?;
    loop {
        let suffix = GENTEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("{prefix}{suffix}");
        if runtime
            .packages
            .borrow()
            .find(package, &candidate)
            .is_none()
        {
            return Ok(runtime.intern_package(package, &candidate, at)?.0);
        }
    }
}

fn expected_symbol(runtime: &Runtime, value: Word, at: usize) -> Fault {
    Fault::type_error(
        runtime,
        value,
        "SYMBOL",
        EvalError::new(at, "expected a symbol"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;
    use crate::package::Package;

    #[test]
    fn symbol_primitives_reload_relocated_words_after_allocations() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        config.stress_bytes = Some(0);
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let args = runtime.root_frame(2);
        args.set(0, runtime.fresh_symbol("SOURCE", 0).unwrap().1);
        args.set(1, Word::T);
        let value = Word::fixnum(42).unwrap();
        runtime.heap.set_symbol_cell(args.get(0), false, value);
        let tail = runtime.root_slot(runtime.heap.cons(|| (value, Word::NIL)).unwrap());
        let plist = runtime.heap.cons(|| (Word::T, tail.get())).unwrap();
        runtime.set_symbol_plist(args.get(0), plist);

        let copy =
            copy_symbol(&runtime, &args, 0..2, 0).unwrap_or_else(|fault| panic!("{}", fault.host));
        assert_eq!(runtime.heap.symbol_cell(copy, false), Some(value));
        let copied_plist = runtime.symbol_plist(copy);
        assert_ne!(copied_plist, runtime.symbol_plist(args.get(0)));
        let (indicator, tail) = runtime.heap.cons_fields(copied_plist).unwrap();
        assert_eq!(indicator, Word::T);
        assert_eq!(runtime.heap.cons_fields(tail), Some((value, Word::NIL)));

        let (counter, _) = runtime
            .intern_package(Package::COMMON_LISP, "*GENSYM-COUNTER*", 0)
            .unwrap();
        let counter = runtime.root_slot(counter);
        let suffix = num_bigint::BigInt::from(1u128 << 70);
        let integer = runtime
            .heap
            .allocate_number(&Number::Real(Real::Integer(suffix.clone())))
            .unwrap();
        runtime.heap.set_symbol_cell(counter.get(), false, integer);

        let generated = gensym(&runtime, None, 0).unwrap_or_else(|fault| panic!("{}", fault.host));
        assert_eq!(
            runtime.heap.symbol_name(generated),
            Some(format!("G{suffix}"))
        );
        assert_eq!(
            runtime
                .heap
                .symbol_cell(counter.get(), false)
                .and_then(|word| runtime.heap.integer(word)),
            Some(suffix + 1u8),
        );
    }
}
