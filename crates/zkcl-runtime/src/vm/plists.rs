use std::ops::Range;

use super::{Primitive, Values, proper_list};
use crate::condition::Fault;
use crate::heap::{ListSpine, RootSlots, Word};
use crate::{EvalError, Runtime};

// These functions appear as function objects in SETF and REMF expansions.
pub(crate) const PUT: &str = "%PUTF";
pub(crate) const REMOVE: &str = "%REMF";

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Get,
    GetProperties,
    Put,
    Remove,
    SymbolPlist,
    SetSymbolPlist,
    SymbolGet,
    SetSymbolGet,
    Remprop,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("GETF", Op::Get),
    ("GET-PROPERTIES", Op::GetProperties),
    (PUT, Op::Put),
    (REMOVE, Op::Remove),
    ("SYMBOL-PLIST", Op::SymbolPlist),
    ("(SETF SYMBOL-PLIST)", Op::SetSymbolPlist),
    ("GET", Op::SymbolGet),
    ("(SETF GET)", Op::SetSymbolGet),
    ("REMPROP", Op::Remprop),
];

pub(super) fn setf_primitive(accessor: &str) -> Option<Word> {
    match accessor {
        "SYMBOL-PLIST" => Primitive::named("(SETF SYMBOL-PLIST)"),
        "GET" => Primitive::named("(SETF GET)"),
        _ => None,
    }
}

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Get | Self::SymbolGet => (2..=3).contains(&count),
            Self::SetSymbolGet => (3..=4).contains(&count),
            Self::Put => count == 3,
            Self::GetProperties | Self::Remove | Self::SetSymbolPlist | Self::Remprop => count == 2,
            Self::SymbolPlist => count == 1,
        }
    }
}

fn require_symbol(runtime: &Runtime, word: Word, at: usize) -> Result<Word, Fault> {
    if runtime.heap.is_symbol(word) {
        Ok(word)
    } else {
        Err(Fault::type_error(
            runtime,
            word,
            "SYMBOL",
            EvalError::new(at, "expected a symbol"),
        ))
    }
}

fn put_property(
    runtime: &Runtime,
    plist: Word,
    indicator: Word,
    value: Word,
    at: usize,
) -> Result<Word, Fault> {
    let heap = &runtime.heap;
    if let Some(entry) = find(runtime, plist, |key| key == indicator, at)? {
        heap.set_cons_car(entry.value_cell, value);
        return Ok(plist);
    }
    let oom = |error| EvalError::out_of_memory(at, error);
    let indicator = runtime.root_slot(indicator);
    let value = runtime.root_slot(value);
    let plist = runtime.root_slot(plist);
    let tail = runtime.root_slot(heap.cons(|| (value.get(), plist.get())).map_err(oom)?);
    heap.cons(|| (indicator.get(), tail.get()))
        .map_err(oom)
        .map_err(Fault::from)
}

fn remove_property(
    runtime: &Runtime,
    plist: Word,
    indicator: Word,
    at: usize,
) -> Result<(Word, bool), Fault> {
    Ok(match find(runtime, plist, |key| key == indicator, at)? {
        Some(entry) => {
            let new_head = if let Some(previous) = entry.previous_value_cell {
                runtime.heap.set_cons_cdr(previous, entry.rest);
                plist
            } else {
                entry.rest
            };
            (new_head, true)
        }
        None => (plist, false),
    })
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    match op {
        Op::SymbolPlist => {
            let symbol = require_symbol(runtime, frame.get(args.start), at)?;
            values.one(runtime.symbol_plist(symbol));
            return Ok(());
        }
        Op::SetSymbolPlist => {
            let plist = frame.get(args.start);
            let symbol = require_symbol(runtime, frame.get(args.start + 1), at)?;
            runtime.set_symbol_plist(symbol, plist);
            values.one(plist);
            return Ok(());
        }
        Op::SymbolGet => {
            let symbol = require_symbol(runtime, frame.get(args.start), at)?;
            let indicator = frame.get(args.start + 1);
            let default = if args.len() == 3 {
                frame.get(args.start + 2)
            } else {
                Word::NIL
            };
            let plist = runtime.symbol_plist(symbol);
            let entry = find(runtime, plist, |key| key == indicator, at)?;
            values.one(entry.map_or(default, |entry| entry.value));
            return Ok(());
        }
        Op::SetSymbolGet => {
            let new_value = frame.get(args.start);
            let symbol = require_symbol(runtime, frame.get(args.start + 1), at)?;
            let indicator = frame.get(args.start + 2);
            let plist = runtime.symbol_plist(symbol);
            let updated = put_property(runtime, plist, indicator, new_value, at)?;
            runtime.set_symbol_plist(frame.get(args.start + 1), updated);
            values.one(frame.get(args.start));
            return Ok(());
        }
        Op::Remprop => {
            let symbol = require_symbol(runtime, frame.get(args.start), at)?;
            let indicator = frame.get(args.start + 1);
            let plist = runtime.symbol_plist(symbol);
            let (updated, found) = remove_property(runtime, plist, indicator, at)?;
            if found {
                runtime.set_symbol_plist(symbol, updated);
            }
            values.one(if found { Word::T } else { Word::NIL });
            return Ok(());
        }
        _ => {}
    }

    let plist = frame.get(args.start);
    let indicator = frame.get(args.start + 1);
    if matches!(op, Op::GetProperties) {
        let indicators = proper_list(runtime, indicator, at).map_err(|error| {
            Fault::type_error(
                runtime,
                crate::vm::list::list_tail(runtime, indicator),
                "LIST",
                error,
            )
        })?;
        let result = find(runtime, plist, |key| indicators.contains(&key), at)?;
        let [key, value, tail] = result.map_or([Word::NIL; 3], |entry| {
            [entry.indicator, entry.value, entry.indicator_cell]
        });
        values.resize(3);
        values.slots.set(0, key);
        values.slots.set(1, value);
        values.slots.set(2, tail);
        return Ok(());
    }

    match op {
        Op::Get => {
            let entry = find(runtime, plist, |key| key == indicator, at)?;
            let default = if args.len() == 3 {
                frame.get(args.start + 2)
            } else {
                Word::NIL
            };
            values.one(entry.map_or(default, |entry| entry.value));
        }
        Op::Put => {
            let updated = put_property(runtime, plist, indicator, frame.get(args.start + 2), at)?;
            values.one(updated);
        }
        Op::Remove => {
            let (plist, found) = remove_property(runtime, plist, indicator, at)?;
            values.resize(2);
            values.slots.set(0, plist);
            values.slots.set(1, if found { Word::T } else { Word::NIL });
        }
        Op::GetProperties
        | Op::SymbolPlist
        | Op::SetSymbolPlist
        | Op::SymbolGet
        | Op::SetSymbolGet
        | Op::Remprop => unreachable!("handled above"),
    }
    Ok(())
}

struct Property {
    indicator: Word,
    value: Word,
    indicator_cell: Word,
    value_cell: Word,
    previous_value_cell: Option<Word>,
    rest: Word,
}

fn find(
    runtime: &Runtime,
    plist: Word,
    matches: impl Fn(Word) -> bool,
    at: usize,
) -> Result<Option<Property>, Fault> {
    let heap = &runtime.heap;
    let invalid = |datum| {
        Fault::type_error(
            runtime,
            datum,
            "LIST",
            EvalError::new(
                at,
                "expected a proper property list of indicator/value pairs",
            ),
        )
    };
    let mut spine = ListSpine::new(plist);
    let mut previous_value_cell = None;
    let mut found = None;
    while spine.tail() != Word::NIL {
        let indicator_cell = spine.tail();
        let (indicator, value_cell) = spine
            .next(heap)
            .map_err(|_| invalid(indicator_cell))?
            .ok_or_else(|| invalid(indicator_cell))?;
        let value_cell_tail = spine.tail();
        let (value, rest) = spine
            .next(heap)
            .map_err(|_| invalid(value_cell_tail))?
            .ok_or_else(|| invalid(value_cell_tail))?;
        if found.is_none() && matches(indicator) {
            found = Some(Property {
                indicator,
                value,
                indicator_cell,
                value_cell,
                previous_value_cell,
                rest,
            });
        }
        // Finish validation before returning a cell to a mutating operation.
        previous_value_cell = Some(value_cell);
    }
    Ok(found)
}
