use std::ops::Range;

use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;

use super::{Register, Values, proper_list};
use crate::condition::Fault;
use crate::heap::{ListSpine, RootFrame, RootSlot, RootSlots, Word};
use crate::{EvalError, Runtime};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    Cons,
    Access(&'static str),
    Element(usize),
    List,
    ListStar,
    MakeList,
    Append,
    CopyList,
    CopyTree,
    CopyAlist,
    Acons,
    Pairlis,
    ListLength,
    Revappend,
    Nreconc,
    Tailp,
    Ldiff,
    Rplaca,
    Rplacd,
    Nconc,
    Listp,
    Consp,
    Atom,
    Null,
    Endp,
    Nth,
    Nthcdr,
    Last,
    Butlast { destructive: bool },
}

const fn accessor(name: &'static str) -> (&'static str, Op) {
    (name, Op::Access(name))
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("CONS", Op::Cons),
    accessor("CAR"),
    accessor("CDR"),
    accessor("CAAR"),
    accessor("CADR"),
    accessor("CDAR"),
    accessor("CDDR"),
    accessor("CAAAR"),
    accessor("CAADR"),
    accessor("CADAR"),
    accessor("CADDR"),
    accessor("CDAAR"),
    accessor("CDADR"),
    accessor("CDDAR"),
    accessor("CDDDR"),
    accessor("CAAAAR"),
    accessor("CAAADR"),
    accessor("CAADAR"),
    accessor("CAADDR"),
    accessor("CADAAR"),
    accessor("CADADR"),
    accessor("CADDAR"),
    accessor("CADDDR"),
    accessor("CDAAAR"),
    accessor("CDAADR"),
    accessor("CDADAR"),
    accessor("CDADDR"),
    accessor("CDDAAR"),
    accessor("CDDADR"),
    accessor("CDDDAR"),
    accessor("CDDDDR"),
    ("FIRST", Op::Element(0)),
    ("SECOND", Op::Element(1)),
    ("THIRD", Op::Element(2)),
    ("FOURTH", Op::Element(3)),
    ("FIFTH", Op::Element(4)),
    ("SIXTH", Op::Element(5)),
    ("SEVENTH", Op::Element(6)),
    ("EIGHTH", Op::Element(7)),
    ("NINTH", Op::Element(8)),
    ("TENTH", Op::Element(9)),
    ("REST", Op::Access("CDR")),
    ("LIST", Op::List),
    ("LIST*", Op::ListStar),
    ("MAKE-LIST", Op::MakeList),
    ("APPEND", Op::Append),
    ("COPY-LIST", Op::CopyList),
    ("COPY-TREE", Op::CopyTree),
    ("COPY-ALIST", Op::CopyAlist),
    ("ACONS", Op::Acons),
    ("PAIRLIS", Op::Pairlis),
    ("LIST-LENGTH", Op::ListLength),
    ("REVAPPEND", Op::Revappend),
    ("NRECONC", Op::Nreconc),
    ("TAILP", Op::Tailp),
    ("LDIFF", Op::Ldiff),
    ("RPLACA", Op::Rplaca),
    ("RPLACD", Op::Rplacd),
    ("NCONC", Op::Nconc),
    ("LISTP", Op::Listp),
    ("CONSP", Op::Consp),
    ("ATOM", Op::Atom),
    ("NULL", Op::Null),
    ("NOT", Op::Null),
    ("ENDP", Op::Endp),
    ("NTH", Op::Nth),
    ("NTHCDR", Op::Nthcdr),
    ("LAST", Op::Last),
    ("BUTLAST", Op::Butlast { destructive: false }),
    ("NBUTLAST", Op::Butlast { destructive: true }),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::Cons
            | Self::Rplaca
            | Self::Rplacd
            | Self::Nth
            | Self::Nthcdr
            | Self::Revappend
            | Self::Nreconc
            | Self::Tailp
            | Self::Ldiff => count == 2,
            Self::Acons => count == 3,
            Self::Access(_)
            | Self::Element(_)
            | Self::Listp
            | Self::Consp
            | Self::Atom
            | Self::Null
            | Self::CopyList
            | Self::CopyTree
            | Self::CopyAlist
            | Self::ListLength
            | Self::Endp => count == 1,
            Self::Last | Self::Butlast { .. } => (1..=2).contains(&count),
            Self::Pairlis => (2..=3).contains(&count),
            Self::ListStar | Self::MakeList => count > 0,
            Self::List | Self::Append | Self::Nconc => true,
        }
    }
}

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Op,
    mut args: Range<Register>,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    let boolean = |yes| if yes { Word::T } else { Word::NIL };
    let first = || frame.get(args.start);
    let word = match op {
        Op::Cons => runtime
            .heap
            .cons(|| (first(), frame.get(args.start + 1)))
            .map_err(|error| EvalError::out_of_memory(at, error))?,
        Op::Access(name) => access(runtime, first(), name, at)?,
        Op::Element(index) => fields(runtime, advance_cdrs(runtime, first(), index, at)?, at)?.0,
        Op::List | Op::ListStar => {
            let tail = match op {
                Op::ListStar => frame.get(args.next_back().expect("LIST* takes an argument")),
                _ => Word::NIL,
            };
            return prepend(runtime, frame, args, tail, values, at);
        }
        Op::MakeList => {
            let size = non_negative_index(runtime, first(), at)?;
            let [initial_reg] = super::named_keyword_arguments(
                runtime,
                frame,
                args.start + 1..args.end,
                ["INITIAL-ELEMENT"],
                at,
            )?;
            let roots = runtime.root_frame(2);
            roots.set(0, initial_reg.map_or(Word::NIL, |reg| frame.get(reg)));
            roots.set(1, Word::NIL);
            for _ in 0..size {
                let pair = runtime
                    .heap
                    .cons(|| (roots.get(0), roots.get(1)))
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
                roots.set(1, pair);
            }
            roots.get(1)
        }
        Op::Append => {
            let Some(last) = args.next_back() else {
                values.one(Word::NIL);
                return Ok(());
            };
            let mut prefix = Vec::new();
            for reg in args {
                let list = frame.get(reg);
                prefix.extend(
                    proper_list(runtime, list, at).map_err(|error| {
                        Fault::type_error(runtime, list_tail(runtime, list), "LIST", error)
                    })?,
                );
            }
            // Traversal does not allocate Lisp objects. Root the gathered items
            // before allocating the copied conses.
            let count = prefix.len();
            let roots = runtime.root_frame(count);
            for (index, item) in prefix.into_iter().enumerate() {
                roots.set(index, item);
            }
            return prepend(runtime, &roots, 0..count, frame.get(last), values, at);
        }
        Op::CopyList => copy_list(runtime, first(), at)?,
        Op::CopyTree => copy_tree(runtime, first(), at)?,
        Op::Rplaca | Op::Rplacd => {
            let cons = first();
            if runtime.heap.cons_fields(cons).is_none() {
                return Err(Fault::type_error(runtime, 
                    cons,
                    "CONS",
                    EvalError::new(at, "expected a cons"),
                ));
            }
            let value = frame.get(args.start + 1);
            if matches!(op, Op::Rplaca) {
                runtime.heap.set_cons_car(cons, value);
            } else {
                runtime.heap.set_cons_cdr(cons, value);
            }
            cons
        }
        Op::Nconc => nconc(runtime, args.map(|reg| frame.get(reg)), at)?,
        Op::Listp => {
            let list = first();
            boolean(list == Word::NIL || runtime.heap.cons_fields(list).is_some())
        }
        Op::Consp => boolean(runtime.heap.cons_fields(first()).is_some()),
        Op::Atom => boolean(runtime.heap.cons_fields(first()).is_none()),
        Op::Null => boolean(first() == Word::NIL),
        Op::Endp => {
            let list = first();
            fields(runtime, list, at)?;
            boolean(list == Word::NIL)
        }
        Op::Nth | Op::Nthcdr => {
            let list = frame.get(args.start + 1);
            let tail = indexed_tail(runtime, first(), list, at)?;
            if matches!(op, Op::Nth) {
                fields(runtime, tail, at)?.0
            } else {
                tail
            }
        }
        Op::Last | Op::Butlast { .. } => {
            let count = if args.len() == 2 {
                suffix_count(runtime, frame.get(args.start + 1), at)?
            } else {
                1
            };
            match op {
                Op::Butlast { destructive } => butlast(runtime, first(), count, destructive, at)?,
                _ => last(runtime, first(), count, at)?,
            }
        }
        Op::Acons => {
            let roots = runtime.root_frame(2);
            roots.set(0, frame.get(args.start + 1));
            roots.set(1, frame.get(args.start + 2));
            let pair = runtime
                .heap
                .cons(|| (first(), roots.get(0)))
                .map_err(|error| EvalError::out_of_memory(at, error))?;
            let pair = runtime.root_slot(pair);
            runtime
                .heap
                .cons(|| (pair.get(), roots.get(1)))
                .map_err(|error| EvalError::out_of_memory(at, error))?
        }
        Op::Pairlis => {
            let keys = proper_list(runtime, first(), at)
                .map_err(|error| Fault::type_error(runtime, list_tail(runtime, first()), "LIST", error))?;
            let data_word = frame.get(args.start + 1);
            let data = proper_list(runtime, data_word, at)
                .map_err(|error| Fault::type_error(runtime, list_tail(runtime, data_word), "LIST", error))?;
            if keys.len() != data.len() {
                return Err(Fault::plain(
                    crate::condition::Class::SimpleError,
                    EvalError::new(at, "PAIRLIS lists have different lengths"),
                ));
            }
            let alist = if args.len() == 3 {
                frame.get(args.start + 2)
            } else {
                Word::NIL
            };
            let count = keys.len();
            let roots = runtime.root_frame(count * 2 + 1);
            for (index, (k, d)) in keys.into_iter().zip(data).enumerate() {
                roots.set(index * 2, k);
                roots.set(index * 2 + 1, d);
            }
            roots.set(count * 2, alist);
            let tail = runtime.root_frame(2);
            tail.set(0, roots.get(count * 2));
            for index in (0..count).rev() {
                let entry = runtime
                    .heap
                    .cons(|| (roots.get(index * 2), roots.get(index * 2 + 1)))
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
                tail.set(1, entry);
                let cell = runtime
                    .heap
                    .cons(|| (tail.get(1), tail.get(0)))
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
                tail.set(0, cell);
            }
            tail.get(0)
        }
        Op::CopyAlist => copy_alist(runtime, first(), at)?,
        Op::ListLength => list_length(runtime, first(), at)?,
        Op::Revappend => revappend(runtime, first(), frame.get(args.start + 1), at)?,
        Op::Nreconc => nreconc(runtime, first(), frame.get(args.start + 1), at)?,
        Op::Tailp => boolean(tailp(runtime, first(), frame.get(args.start + 1), at)?),
        Op::Ldiff => ldiff(runtime, first(), frame.get(args.start + 1), at)?,
    };
    values.one(word);
    Ok(())
}

/// Builds a fresh proper list and keeps both ends rooted while it grows.
pub(super) struct ListBuilder(RootFrame);

impl ListBuilder {
    pub(super) fn new(runtime: &Runtime) -> Self {
        Self(runtime.root_frame(2))
    }

    pub(super) fn head(&self) -> Word {
        self.0.get(0)
    }

    pub(super) fn push(
        &self,
        runtime: &Runtime,
        value: impl FnOnce() -> Word,
        at: usize,
    ) -> Result<(), EvalError> {
        let cell = runtime
            .heap
            .cons(|| (value(), Word::NIL))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        let tail = self.0.get(1);
        if tail == Word::NIL {
            self.0.set(0, cell);
        } else {
            runtime.heap.set_cons_cdr(tail, cell);
        }
        self.0.set(1, cell);
        Ok(())
    }
}

pub(super) fn nconc(
    runtime: &Runtime,
    lists: impl Iterator<Item = Word>,
    at: usize,
) -> Result<Word, Fault> {
    let mut lists = lists.peekable();
    let mut prefixes = Vec::new();
    let mut result = Word::NIL;
    while let Some(list) = lists.next() {
        if lists.peek().is_none() {
            result = list;
            break;
        }
        if list != Word::NIL {
            prefixes.push((list, last(runtime, list, 1, at)?));
        }
    }
    // Traversal and stores cannot collect. Validate the prefixes before
    // relinking them so a malformed source leaves every input intact.
    for (head, tail) in prefixes.into_iter().rev() {
        runtime.heap.set_cons_cdr(tail, result);
        result = head;
    }
    Ok(result)
}

fn prepend(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<Register>,
    tail: Word,
    values: &mut Values,
    at: usize,
) -> Result<(), Fault> {
    values.one(tail);
    for reg in args.rev() {
        let pair = runtime
            .heap
            .cons(|| (frame.get(reg), values.get(0)))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        values.slots.set(0, pair);
    }
    Ok(())
}

pub(super) fn copy_list(runtime: &Runtime, list: Word, at: usize) -> Result<Word, Fault> {
    fields(runtime, list, at)?;
    let mut spine = ListSpine::new(list);
    let mut elements = Vec::new();
    while let Some((element, _)) = spine
        .next(&runtime.heap)
        .map_err(|_| EvalError::new(at, "expected a list with a finite cdr chain"))?
    {
        elements.push(runtime.root_slot(element));
    }
    copy_elements(runtime, elements, spine.tail(), at)
}

fn copy_elements(
    runtime: &Runtime,
    elements: Vec<RootSlot>,
    tail: Word,
    at: usize,
) -> Result<Word, Fault> {
    let mut tail = runtime.root_slot(tail);
    for element in elements.into_iter().rev() {
        let pair = runtime
            .heap
            .cons(|| (element.get(), tail.get()))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        tail = runtime.root_slot(pair);
    }
    Ok(tail.get())
}

/// The tail at which a cdr chain stops being a cons (an atom, a vector, or
/// NIL), used as the offending datum for improper-list type errors. A circular
/// chain has no such tail, so the original object is returned.
pub(super) fn list_tail(runtime: &Runtime, word: Word) -> Word {
    let mut spine = ListSpine::new(word);
    loop {
        match spine.next(&runtime.heap) {
            Ok(Some(_)) => {}
            Ok(None) => return spine.tail(),
            Err(_) => return word,
        }
    }
}

fn fields(runtime: &Runtime, list: Word, at: usize) -> Result<(Word, Word), Fault> {
    if list == Word::NIL {
        return Ok((Word::NIL, Word::NIL));
    }
    runtime.heap.cons_fields(list).ok_or_else(|| {
        Fault::type_error(runtime, list, "LIST", EvalError::new(at, "expected a cons or NIL"))
    })
}

fn access(runtime: &Runtime, mut list: Word, name: &str, at: usize) -> Result<Word, Fault> {
    // The outermost call is named first, so apply the operations right to left.
    for &direction in name.as_bytes()[1..name.len() - 1].iter().rev() {
        list = runtime
            .heap
            .list_field(list, direction == b'A')
            .ok_or_else(|| {
                Fault::type_error(runtime, list, "LIST", EvalError::new(at, "expected a cons or NIL"))
            })?;
    }
    Ok(list)
}

fn non_negative_index(runtime: &Runtime, value: Word, at: usize) -> Result<usize, Fault> {
    value
        .as_fixnum()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| {
            Fault::non_negative_integer(
                runtime,
                value,
                EvalError::new(at, "expected a non-negative integer"),
            )
        })
}

fn advance_cdrs(runtime: &Runtime, mut list: Word, count: usize, at: usize) -> Result<Word, Fault> {
    for _ in 0..count {
        if list == Word::NIL {
            break;
        }
        list = fields(runtime, list, at)?.1;
    }
    Ok(list)
}

fn indexed_tail(runtime: &Runtime, index: Word, list: Word, at: usize) -> Result<Word, Fault> {
    let (bound, oversized) = non_negative_count(runtime, index, at)?;
    let mut spine = ListSpine::new(list);
    for traversed in 1..=bound {
        match spine.next(&runtime.heap) {
            Ok(Some(_)) => {}
            Ok(None) => return Ok(fields(runtime, spine.tail(), at)?.1),
            Err(cycle) => {
                // The detected step leads back to the checkpoint. Every full
                // period after that leads to the same cons.
                let checkpoint = runtime
                    .heap
                    .cons_cdr(spine.tail())
                    .expect("cycle predecessor");
                let offset = if let Some(index) = oversized {
                    ((index - traversed) % cycle.period)
                        .to_usize()
                        .expect("remainder fits its divisor")
                } else {
                    (bound - traversed) % cycle.period
                };
                return advance_cdrs(runtime, checkpoint, offset, at);
            }
        }
    }
    Ok(spine.tail())
}

fn last(runtime: &Runtime, list: Word, count: usize, at: usize) -> Result<Word, Fault> {
    Ok(suffix_boundary(runtime, list, count, at)?.1)
}

fn suffix_count(runtime: &Runtime, value: Word, at: usize) -> Result<usize, Fault> {
    // Any count above usize::MAX exceeds the length of a finite list.
    non_negative_count(runtime, value, at).map(|(bound, _)| bound)
}

fn non_negative_count(
    runtime: &Runtime,
    value: Word,
    at: usize,
) -> Result<(usize, Option<BigInt>), Fault> {
    let invalid = || {
        Fault::non_negative_integer(runtime, value, EvalError::new(at, "expected a non-negative integer"))
    };
    if let Some(number) = value.as_fixnum() {
        return usize::try_from(number)
            .map(|index| (index, None))
            .map_err(|_| invalid());
    }
    let integer = runtime
        .heap
        .integer(value)
        .filter(|value| value.sign() != Sign::Minus)
        .ok_or_else(invalid)?;
    Ok(match integer.to_usize() {
        Some(index) => (index, None),
        None => (usize::MAX, Some(integer)),
    })
}

fn suffix_boundary(
    runtime: &Runtime,
    list: Word,
    count: usize,
    at: usize,
) -> Result<(Word, Word), Fault> {
    fields(runtime, list, at)?;
    // Keep `lead` up to `count` conses ahead so `tail` is the final suffix
    // when `lead` reaches the end. Neither cursor crosses an allocation.
    let mut lead = ListSpine::new(list);
    let mut tail = list;
    let mut preceding = Word::NIL;
    let mut distance = count;
    while lead
        .next(&runtime.heap)
        .map_err(|_| {
            Fault::type_error(runtime, 
                list,
                "LIST",
                EvalError::new(at, "expected a list with a finite cdr chain"),
            )
        })?
        .is_some()
    {
        if distance == 0 {
            preceding = tail;
            tail = runtime.heap.cons_fields(tail).expect("cons before lead").1;
        } else {
            distance -= 1;
        }
    }
    Ok((preceding, tail))
}

fn butlast(
    runtime: &Runtime,
    list: Word,
    count: usize,
    destructive: bool,
    at: usize,
) -> Result<Word, Fault> {
    let (preceding, suffix) = suffix_boundary(runtime, list, count, at)?;
    if preceding == Word::NIL {
        return Ok(Word::NIL);
    }
    if destructive {
        runtime.heap.set_cons_cdr(preceding, Word::NIL);
        return Ok(list);
    }

    let mut elements = Vec::new();
    let mut tail = list;
    while tail != suffix {
        let (element, cdr) = runtime.heap.cons_fields(tail).expect("cons before suffix");
        elements.push(runtime.root_slot(element));
        tail = cdr;
    }
    copy_elements(runtime, elements, Word::NIL, at)
}

pub(super) fn copy_tree(runtime: &Runtime, root: Word, at: usize) -> Result<Word, Fault> {
    enum Task {
        Visit(RootSlot),
        Cons,
    }
    let mut stack = vec![Task::Visit(runtime.root_slot(root))];
    let mut out: Vec<RootSlot> = Vec::new();
    while let Some(task) = stack.pop() {
        match task {
            Task::Visit(slot) => {
                let word = slot.get();
                if let Some((car, cdr)) = runtime.heap.cons_fields(word) {
                    stack.push(Task::Cons);
                    stack.push(Task::Visit(runtime.root_slot(cdr)));
                    stack.push(Task::Visit(runtime.root_slot(car)));
                } else {
                    out.push(slot);
                }
            }
            Task::Cons => {
                let cdr = out.pop().unwrap();
                let car = out.pop().unwrap();
                let cell = runtime
                    .heap
                    .cons(|| (car.get(), cdr.get()))
                    .map_err(|error| EvalError::out_of_memory(at, error))?;
                out.push(runtime.root_slot(cell));
            }
        }
    }
    Ok(out.pop().unwrap().get())
}

fn copy_alist(runtime: &Runtime, list: Word, at: usize) -> Result<Word, Fault> {
    fields(runtime, list, at)?;
    let mut spine = ListSpine::new(list);
    let mut entries = Vec::new();
    while let Some((element, _)) = spine.next(&runtime.heap).map_err(|_| {
        Fault::type_error(runtime, list, "LIST", EvalError::new(at, "expected a finite list"))
    })? {
        entries.push(runtime.root_slot(element));
    }
    if spine.tail() != Word::NIL {
        return Err(Fault::type_error(runtime, 
            spine.tail(),
            "LIST",
            EvalError::new(at, "expected a proper association list"),
        ));
    }
    let tail = runtime.root_frame(2);
    tail.set(0, spine.tail());
    for entry in entries.into_iter().rev() {
        let copied_entry = if let Some((car, cdr)) = runtime.heap.cons_fields(entry.get()) {
            let pair = runtime.root_frame(2);
            pair.set(0, car);
            pair.set(1, cdr);
            runtime
                .heap
                .cons(|| (pair.get(0), pair.get(1)))
                .map_err(|error| EvalError::out_of_memory(at, error))?
        } else {
            entry.get()
        };
        tail.set(1, copied_entry);
        let cell = runtime
            .heap
            .cons(|| (tail.get(1), tail.get(0)))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        tail.set(0, cell);
    }
    Ok(tail.get(0))
}

fn list_length(runtime: &Runtime, list: Word, at: usize) -> Result<Word, Fault> {
    fields(runtime, list, at)?;
    let mut spine = ListSpine::new(list);
    let mut len: usize = 0;
    loop {
        match spine.next(&runtime.heap) {
            Ok(Some(_)) => len += 1,
            Ok(None) => {
                if spine.tail() != Word::NIL {
                    return Err(Fault::type_error(runtime, 
                        spine.tail(),
                        "LIST",
                        EvalError::new(at, "expected a proper list"),
                    ));
                }
                return isize::try_from(len)
                    .ok()
                    .and_then(Word::fixnum)
                    .ok_or_else(|| {
                        Fault::from(EvalError::new(at, "list length exceeds fixnum range"))
                    });
            }
            Err(_) => return Ok(Word::NIL),
        }
    }
}

fn revappend(runtime: &Runtime, list: Word, tail: Word, at: usize) -> Result<Word, Fault> {
    let items = proper_list(runtime, list, at)
        .map_err(|error| Fault::type_error(runtime, list_tail(runtime, list), "LIST", error))?;
    let count = items.len();
    let roots = runtime.root_frame(count + 2);
    for (index, item) in items.into_iter().enumerate() {
        roots.set(index, item);
    }
    roots.set(count, tail);
    roots.set(count + 1, tail);
    for index in 0..count {
        let cell = runtime
            .heap
            .cons(|| (roots.get(index), roots.get(count + 1)))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        roots.set(count + 1, cell);
    }
    Ok(roots.get(count + 1))
}

fn nreconc(runtime: &Runtime, list: Word, mut tail: Word, at: usize) -> Result<Word, Fault> {
    fields(runtime, list, at)?;
    let mut spine = ListSpine::new(list);
    let mut cells = Vec::new();
    loop {
        let cell = spine.tail();
        match spine.next(&runtime.heap) {
            Ok(Some(_)) => cells.push(cell),
            Ok(None) => {
                if spine.tail() != Word::NIL {
                    return Err(Fault::type_error(runtime, 
                        spine.tail(),
                        "LIST",
                        EvalError::new(at, "expected a proper list"),
                    ));
                }
                break;
            }
            Err(_) => {
                return Err(Fault::type_error(runtime, 
                    list,
                    "LIST",
                    EvalError::new(at, "expected a finite list"),
                ));
            }
        }
    }
    for cell in cells {
        runtime.heap.set_cons_cdr(cell, tail);
        tail = cell;
    }
    Ok(tail)
}

fn tailp(runtime: &Runtime, object: Word, list: Word, at: usize) -> Result<bool, Fault> {
    fields(runtime, list, at)?;
    let mut spine = ListSpine::new(list);
    loop {
        if super::equality::eql(&runtime.heap, object, spine.tail()) {
            return Ok(true);
        }
        match spine.next(&runtime.heap) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return Ok(false),
        }
    }
}

fn ldiff(runtime: &Runtime, list: Word, object: Word, at: usize) -> Result<Word, Fault> {
    fields(runtime, list, at)?;
    let mut spine = ListSpine::new(list);
    let mut elements = Vec::new();
    loop {
        let cursor = spine.tail();
        if super::equality::eql(&runtime.heap, cursor, object) {
            return copy_elements(runtime, elements, Word::NIL, at);
        }
        match spine.next(&runtime.heap) {
            Ok(Some((car, _))) => elements.push(runtime.root_slot(car)),
            Ok(None) => return copy_elements(runtime, elements, spine.tail(), at),
            Err(_) => {
                return Err(Fault::type_error(runtime, 
                    list,
                    "LIST",
                    EvalError::new(at, "expected a finite list"),
                ));
            }
        }
    }
}
