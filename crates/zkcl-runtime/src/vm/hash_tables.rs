use std::ops::Range;

use num_traits::ToPrimitive;
use zkcl_bytecode::{HashTableWeakness, RehashHint};

use super::{
    Continuation, ContinuationStep, Primitive, Resume, Values, designator, equality,
    named_keyword_arguments,
};
use crate::condition::{Class, Fault};
use crate::heap::{HashTableLayout, HashTest, Heap, RootSlot, RootSlots, Word};
use crate::number::{Number, Real};
use crate::symbol::{Name, Package};
use crate::{EvalError, OutOfMemory, Root, Runtime};

pub(crate) fn iterator_step<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [table_arg, cursor_arg] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected hash table iterator arguments"),
        ));
    };
    let table = table_arg.slot.get();
    table_info(runtime, table, at)?;
    let cursor = cursor_arg.as_fixnum().ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected hash table cursor"),
        )
    })?;
    if cursor < 0 {
        return Ok(vec![table_arg.clone()]);
    }
    let mut index = cursor as usize;
    let Some((key, value)) = runtime.heap.hash_table_next_entry(table, &mut index) else {
        return Ok(vec![runtime.nil()]);
    };
    let next = count_word(index, at)?;
    Ok(vec![
        runtime.root(next),
        runtime.root(key),
        runtime.root(value),
    ])
}

const SET_GETHASH: &str = "(SETF GETHASH)";

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Make,
    Predicate,
    Count,
    Size,
    Test,
    RehashSize,
    RehashThreshold,
    Get,
    PatternResult,
    Set,
    Remove,
    Clear,
    Map,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("MAKE-HASH-TABLE", Op::Make),
    ("HASH-TABLE-P", Op::Predicate),
    ("HASH-TABLE-COUNT", Op::Count),
    ("HASH-TABLE-SIZE", Op::Size),
    ("HASH-TABLE-TEST", Op::Test),
    ("HASH-TABLE-REHASH-SIZE", Op::RehashSize),
    ("HASH-TABLE-REHASH-THRESHOLD", Op::RehashThreshold),
    ("GETHASH", Op::Get),
    ("ZK::%PATTERN-LOOKUP", Op::PatternResult),
    (SET_GETHASH, Op::Set),
    ("REMHASH", Op::Remove),
    ("CLRHASH", Op::Clear),
    ("MAPHASH", Op::Map),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Make => true,
            Self::Get => (2..=3).contains(&count),
            Self::PatternResult => count == 3,
            Self::Set => (3..=4).contains(&count),
            Self::Remove | Self::Map => count == 2,
            _ => count == 1,
        }
    }
}

pub(crate) fn setf_primitive(accessor: &str) -> Option<Word> {
    if accessor == "GETHASH" {
        Primitive::named(SET_GETHASH)
    } else {
        None
    }
}

pub(crate) fn put(
    heap: &Heap,
    table: Word,
    test: HashTest,
    key: Word,
    value: Word,
) -> Result<(), OutOfMemory> {
    let hash = equality::hash(heap, key, test);
    heap.hash_table_set(table, key, value, hash, |left, right| {
        equality::matches(heap, left, right, test)
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
    let heap = &runtime.heap;
    let arg = |index| frame.get(args.start + index);
    let result = match op {
        Op::Make => make(runtime, frame, args, at)?,
        Op::Predicate => truth(heap.hash_table_info(arg(0)).is_some()),
        Op::Get | Op::PatternResult | Op::Remove => {
            let (key, table) = (arg(0), arg(1));
            let test = table_info(runtime, table, at)?.test;
            let hash = equality::hash(heap, key, test);
            let same = |left, right| equality::matches(heap, left, right, test);
            if matches!(op, Op::Remove) {
                truth(heap.hash_table_remove(table, key, hash, same))
            } else {
                let found = heap.hash_table_get(table, key, hash, same);
                let default = if args.len() == 3 { arg(2) } else { Word::NIL };
                if matches!(op, Op::Get) {
                    values.resize(2);
                    values.slots.set(0, found.unwrap_or(default));
                    values.slots.set(1, truth(found.is_some()));
                    return Ok(());
                }
                // The pattern compiler has an explicit miss value. Its table
                // represents a single-valued expression, even on a stored NIL.
                found.unwrap_or(default)
            }
        }
        Op::Set => {
            let table = arg(2);
            let test = table_info(runtime, table, at)?.test;
            put(heap, table, test, arg(1), arg(0))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            // Growth can move Lisp objects. Reload the stored result from the frame.
            arg(0)
        }
        Op::Count | Op::Size | Op::Test | Op::RehashSize | Op::RehashThreshold | Op::Clear => {
            let table = arg(0);
            let info = table_info(runtime, table, at)?;
            match op {
                Op::Count => count_word(info.count, at)?,
                Op::Size => count_word(info.size, at)?,
                Op::Test => runtime.intern(&Name::common_lisp(test_name(info.test)), at)?,
                Op::RehashSize => rehash_word(runtime, info.rehash_size, at)?,
                Op::RehashThreshold => rehash_word(runtime, info.rehash_threshold, at)?,
                Op::Clear => {
                    heap.hash_table_clear(table);
                    table
                }
                _ => unreachable!("hash table accessor or clear"),
            }
        }
        Op::Map => unreachable!("MAPHASH dispatches through the VM"),
    };
    values.one(result);
    Ok(())
}

fn make(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    let options = named_keyword_arguments(
        runtime,
        frame,
        args,
        [
            "TEST",
            "SIZE",
            "REHASH-SIZE",
            "REHASH-THRESHOLD",
            "WEAKNESS",
        ],
        at,
    )?;
    let arg = |index: usize| options[index].map(|index| frame.get(index));
    let test = if let Some(value) = arg(0) {
        hash_test(runtime, value, at)?
    } else {
        HashTest::Eql
    };
    let heap = &runtime.heap;
    let size = arg(1).map_or(Ok(8), |value| non_negative(runtime, value, at))?;
    let rehash_size = match arg(2) {
        Some(value) => rehash_size_argument(heap, value, at)?,
        None => RehashHint::integer(8),
    };
    let threshold = match arg(3) {
        Some(value) => rehash_threshold_argument(heap, value, at)?,
        None => RehashHint::integer(1),
    };
    let weakness = weakness_argument(runtime, arg(4).unwrap_or(Word::NIL), at)?;
    runtime
        .heap
        .hash_table(test, weakness, size, rehash_size, threshold)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

const WEAKNESSES: [(HashTableWeakness, &str); 4] = [
    (HashTableWeakness::Key, "KEY"),
    (HashTableWeakness::Value, "VALUE"),
    (HashTableWeakness::KeyOrValue, "KEY-OR-VALUE"),
    (HashTableWeakness::KeyAndValue, "KEY-AND-VALUE"),
];

fn weakness_argument(
    runtime: &Runtime,
    value: Word,
    at: usize,
) -> Result<Option<HashTableWeakness>, Fault> {
    if value == Word::NIL {
        return Ok(None);
    }
    if runtime.heap.symbol_home(value) == Some(Package::KEYWORD)
        && let Some(name) = runtime.heap.symbol_name(value)
        && let Some((kind, _)) = WEAKNESSES.iter().find(|(_, spelling)| *spelling == name)
    {
        return Ok(Some(*kind));
    }
    let datum = runtime.root(value);
    let mut expected = runtime.nil();
    for name in WEAKNESSES
        .iter()
        .rev()
        .map(|(_, name)| Name::keyword(name))
        .chain([Name::common_lisp("NIL"), Name::common_lisp("MEMBER")])
    {
        let symbol = runtime.root(runtime.intern(&name, at)?);
        expected = runtime
            .cons(&symbol, &expected)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    Err(Fault::type_error_specifier(runtime, 
        datum.slot.get(),
        expected.slot.get(),
        EvalError::new(
            at,
            "expected NIL, :KEY, :VALUE, :KEY-OR-VALUE or :KEY-AND-VALUE for :WEAKNESS",
        ),
    ))
}

pub(super) fn weakness<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [table] = args else {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "HASH-TABLE-WEAKNESS expects one argument"),
        ));
    };
    let kind = table_info(runtime, table.slot.get(), at)?.weakness;
    let word = match WEAKNESSES.iter().find(|(mode, _)| Some(*mode) == kind) {
        Some((_, name)) => runtime.intern_package(Package::KEYWORD, name, at)?.0,
        None => Word::NIL,
    };
    Ok(vec![runtime.root(word)])
}

fn table_info(runtime: &Runtime, table: Word, at: usize) -> Result<HashTableLayout, Fault> {
    runtime.heap.hash_table_info(table).ok_or_else(|| {
        Fault::type_error(
            runtime,
            table,
            "HASH-TABLE",
            EvalError::new(at, "expected a hash table"),
        )
    })
}

fn hash_test(runtime: &Runtime, value: Word, at: usize) -> Result<HashTest, Fault> {
    let function = designator(runtime, value, at)?;
    match Primitive::compiled_name(function) {
        Some("EQ") => Ok(HashTest::Eq),
        Some("EQL") => Ok(HashTest::Eql),
        Some("EQUAL") => Ok(HashTest::Equal),
        Some("EQUALP") => Ok(HashTest::Equalp),
        _ => Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(
                at,
                "hash table test must designate EQ, EQL, EQUAL or EQUALP",
            ),
        )),
    }
}

fn test_name(test: HashTest) -> &'static str {
    match test {
        HashTest::Eq => "EQ",
        HashTest::Eql => "EQL",
        HashTest::Equal => "EQUAL",
        HashTest::Equalp => "EQUALP",
    }
}

fn rehash_size_argument(heap: &Heap, value: Word, at: usize) -> Result<RehashHint, Fault> {
    let hint = match heap.number(value) {
        Some(Number::Real(Real::Integer(n))) if n.sign() == num_bigint::Sign::Plus => {
            Some(RehashHint::integer(n.to_u64().unwrap_or(u64::MAX)))
        }
        Some(Number::Real(Real::Single(n))) => RehashHint::float(f64::from(n)),
        Some(Number::Real(Real::Double(n))) => RehashHint::float(n),
        _ => None,
    };
    hint.filter(|hint| hint.valid_size()).ok_or_else(|| {
        Fault::plain(
            Class::TypeError,
            EvalError::new(
                at,
                "expected a positive integer or a float greater than 1.0",
            ),
        )
    })
}

fn rehash_threshold_argument(heap: &Heap, value: Word, at: usize) -> Result<RehashHint, Fault> {
    let hint = match heap.number(value) {
        Some(Number::Real(Real::Integer(n))) => n.to_u64().map(RehashHint::integer),
        Some(Number::Real(Real::Ratio(n)))
            if n.numer().sign() != num_bigint::Sign::Minus && n.numer() <= n.denom() =>
        {
            n.to_f64().and_then(RehashHint::float)
        }
        Some(Number::Real(Real::Single(n))) => RehashHint::float(f64::from(n)),
        Some(Number::Real(Real::Double(n))) => RehashHint::float(n),
        _ => None,
    };
    hint.filter(|hint| hint.valid_threshold()).ok_or_else(|| {
        Fault::plain(
            Class::TypeError,
            EvalError::new(at, "expected a real between zero and one"),
        )
    })
}

fn rehash_word(runtime: &Runtime, hint: RehashHint, at: usize) -> Result<Word, Fault> {
    if let Some(step) = hint.as_integer() {
        return runtime
            .integer_from(step)
            .map(|root| root.slot.get())
            .map_err(|error| EvalError::out_of_memory(at, error).into());
    }
    runtime
        .heap
        .allocate_number(&Number::Real(Real::Double(hint.as_f64())))
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

fn non_negative(runtime: &Runtime, value: Word, at: usize) -> Result<usize, Fault> {
    value
        .as_fixnum()
        .and_then(|integer| usize::try_from(integer).ok())
        .ok_or_else(|| {
            Fault::non_negative_integer(
                runtime,
                value,
                EvalError::new(at, "expected a non-negative integer"),
            )
        })
}

fn count_word(count: usize, at: usize) -> Result<Word, Fault> {
    isize::try_from(count)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| {
            Fault::plain(
                Class::ArithmeticError,
                EvalError::new(at, "hash table count exceeds the fixnum range"),
            )
        })
}

fn truth(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

pub(super) struct MapHash {
    table: RootSlot,
    function: RootSlot,
    cursor: usize,
    resume: Resume,
    at: usize,
}

impl MapHash {
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let table_index = args.start + 1;
        table_info(runtime, frame.get(table_index), at)?;
        let function = designator(runtime, frame.get(args.start), at)?;
        Ok(Self {
            table: runtime.root_slot(frame.get(table_index)),
            function: runtime.root_slot(function),
            cursor: 0,
            resume,
            at,
        })
    }
}

impl Continuation for MapHash {
    fn step(
        &mut self,
        runtime: &Runtime,
        _values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        let Some((key, value)) = runtime
            .heap
            .hash_table_next_entry(self.table.get(), &mut self.cursor)
        else {
            return Ok(ContinuationStep::Done(Word::NIL));
        };
        let args = runtime.root_frame(2);
        args.set(0, key);
        args.set(1, value);
        // Updating or removing the current entry preserves the bucket positions.
        Ok(ContinuationStep::Call {
            target: self.function.get(),
            args,
            at: self.at,
        })
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}
