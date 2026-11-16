use std::ops::Range;

use num_bigint::Sign;

use crate::condition::{self, Class, Fault};
use crate::heap::{ArrayElement, ArrayValue, ListSpine, RootSlot, RootSlots, Word};
use crate::package::Package;
use crate::{EvalError, OutOfMemory, Runtime};

use super::{arrays, types};

#[derive(Clone, Copy)]
enum Length {
    Any,
    Exact(usize),
    AtLeast(usize),
}

impl Length {
    fn matches(self, count: usize) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(expected) => count == expected,
            Self::AtLeast(minimum) => count >= minimum,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SequenceKind {
    List { check_cons_type: bool },
    Vector(ArrayElement),
    NilVector,
    Treelist,
    MutableTreelist,
}

/// One member of a result type. A result type such as (OR ...) is a type
/// specifier, not a concrete sequence type: each member contributes the
/// element storage and length constraints that can satisfy it.
#[derive(Clone, Copy)]
struct Alternative {
    kind: SequenceKind,
    length: Length,
}

pub(super) struct ResultSequence {
    specifier: RootSlot,
    alternatives: Vec<Alternative>,
}

impl ResultSequence {
    pub(super) fn parse(runtime: &Runtime, specifier: Word, at: usize) -> Result<Self, Fault> {
        let specifier = runtime.root_slot(specifier);
        let mut alternatives = Vec::new();
        collect(runtime, specifier.get(), 0, at, &mut alternatives)?;
        if alternatives.is_empty() {
            return Err(invalid(runtime, specifier.get(), at));
        }
        // An OR result type has to name one concrete sequence shape. Mixing
        // lists and vectors, or vectors with different element types, would
        // leave the result type ambiguous rather than picking a feasible
        // member by length.
        let mut vector_kind = None;
        let mut saw_list = false;
        for alternative in &alternatives {
            match alternative.kind {
                SequenceKind::List { .. } => {
                    if vector_kind.is_some() {
                        return Err(invalid(runtime, specifier.get(), at));
                    }
                    saw_list = true;
                }
                kind @ (SequenceKind::Vector(_)
                | SequenceKind::NilVector
                | SequenceKind::Treelist
                | SequenceKind::MutableTreelist) => {
                    if saw_list {
                        return Err(invalid(runtime, specifier.get(), at));
                    }
                    match vector_kind {
                        None => vector_kind = Some(kind),
                        Some(existing) if existing == kind => {}
                        Some(_) => {
                            return Err(Fault::plain(
                                Class::TypeError,
                                EvalError::new(at, "result type has no single vector element type"),
                            ));
                        }
                    }
                }
            }
        }
        Ok(Self {
            specifier,
            alternatives,
        })
    }

    pub(super) fn check_length(&self, runtime: &Runtime, count: usize, at: usize) -> Result<(), Fault> {
        self.fit(runtime, count, at).map(|_| ())
    }

    pub(super) fn matches_length(&self, count: usize) -> bool {
        self.fitted(count).is_some()
    }

    fn fitted(&self, count: usize) -> Option<&Alternative> {
        self.alternatives
            .iter()
            .find(|alternative| alternative.length.matches(count))
    }

    fn fit(&self, runtime: &Runtime, count: usize, at: usize) -> Result<SequenceKind, Fault> {
        self.fitted(count)
            .map(|alternative| alternative.kind)
            .ok_or_else(|| {
                let datum = isize::try_from(count)
                    .ok()
                    .and_then(Word::fixnum)
                    .unwrap_or(Word::NIL);
                Fault::type_error_specifier(
                    runtime,
                    datum,
                    self.specifier.get(),
                    EvalError::new(
                        at,
                        format!("length {count} does not fit the requested sequence type"),
                    ),
                )
            })
    }

    pub(super) fn repeated(
        &self,
        runtime: &Runtime,
        size: Word,
        initial: Option<Word>,
        at: usize,
    ) -> Result<Word, Fault> {
        let count = match size.as_fixnum() {
            Some(size) if size >= 0 => size as usize,
            None if runtime
                .heap
                .integer(size)
                .is_some_and(|n| n.sign() == Sign::Plus) =>
            {
                if self
                    .alternatives
                    .iter()
                    .all(|alternative| matches!(alternative.length, Length::Exact(_)))
                {
                    return Err(Fault::type_error_specifier(runtime, 
                        size,
                        self.specifier.get(),
                        EvalError::new(at, "length does not fit the requested sequence type"),
                    ));
                }
                return Err(EvalError::out_of_memory(at, OutOfMemory).into());
            }
            _ => {
                return Err(Fault::non_negative_integer(runtime, 
                    size,
                    EvalError::new(at, "expected a non-negative size"),
                ));
            }
        };
        let kind = self.fit(runtime, count, at)?;
        if let SequenceKind::NilVector = kind
            && let Some(initial) = initial.filter(|_| count != 0)
        {
            return Err(arrays::type_error(runtime, initial, "NIL", at));
        }
        if let SequenceKind::Vector(element) = kind {
            let default = if element == ArrayElement::Character {
                ArrayValue::Word(Word::character(' '))
            } else {
                arrays::default_element(element)
            };
            let value = initial
                .map(|word| arrays::checked_element(runtime, element, word, at))
                .transpose()?
                .unwrap_or(default);
            let value_root = match value {
                ArrayValue::Word(word) => Some(runtime.root_slot(word)),
                _ => None,
            };
            arrays::total_size(&[count], at)?;
            return runtime
                .heap
                .array_from_values(&[count], element, false, None, |_| {
                    value_root
                        .as_ref()
                        .map_or(value, |slot| ArrayValue::Word(slot.get()))
                })
                .map_err(|error| EvalError::out_of_memory(at, error).into());
        }
        let initial = runtime.root_slot(initial.unwrap_or(Word::NIL));
        self.allocate(runtime, kind, count, |_| initial.get(), at)
    }

    pub(super) fn build(
        &self,
        runtime: &Runtime,
        frame: &RootSlots,
        range: Range<usize>,
        at: usize,
    ) -> Result<Word, Fault> {
        let count = range.len();
        let kind = self.fit(runtime, count, at)?;
        match kind {
            SequenceKind::Vector(element) if element != ArrayElement::General => {
                for index in range.clone() {
                    arrays::checked_element(runtime, element, frame.get(index), at)?;
                }
            }
            SequenceKind::NilVector if count != 0 => {
                return Err(arrays::type_error(runtime, frame.get(range.start), "NIL", at));
            }
            _ => {}
        }
        self.allocate(
            runtime,
            kind,
            count,
            |index| frame.get(range.start + index),
            at,
        )
    }

    fn allocate(
        &self,
        runtime: &Runtime,
        kind: SequenceKind,
        count: usize,
        item: impl Fn(usize) -> Word,
        at: usize,
    ) -> Result<Word, Fault> {
        let oom = |error| EvalError::out_of_memory(at, error);
        match kind {
            SequenceKind::Vector(element) => {
                arrays::total_size(&[count], at)?;
                runtime
                    .heap
                    .array(&[count], element, false, None, item)
                    .map_err(|error| oom(error).into())
            }
            SequenceKind::NilVector => {
                arrays::total_size(&[count], at)?;
                runtime
                    .heap
                    .nil_element_array(&[count], false, None, count, |_| '\0')
                    .map_err(|error| oom(error).into())
            }
            SequenceKind::Treelist => {
                let items = runtime.root_frame(count);
                for index in 0..count {
                    items.set(index, item(index));
                }
                runtime
                    .heap
                    .treelist_from_slots(&items, count)
                    .map_err(|error| oom(error).into())
            }
            SequenceKind::MutableTreelist => {
                let items = runtime.root_frame(count);
                for index in 0..count {
                    items.set(index, item(index));
                }
                runtime
                    .heap
                    .mutable_treelist_from_slots(&items, count)
                    .map_err(|error| oom(error).into())
            }
            SequenceKind::List { check_cons_type } => {
                let result = runtime.root_frame(1);
                for index in (0..count).rev() {
                    let pair = runtime
                        .heap
                        .cons(|| (item(index), result.get(0)))
                        .map_err(oom)?;
                    result.set(0, pair);
                }
                // A member with a compound CONS type constrains the elements;
                // validate the union as a whole so any feasible member counts.
                let check_element_type = check_cons_type
                    || self.alternatives.iter().any(|alternative| {
                        matches!(
                            alternative.kind,
                            SequenceKind::List {
                                check_cons_type: true
                            }
                        )
                    });
                if check_element_type
                    && !types::typep(runtime, result.get(0), self.specifier.get(), at)?
                {
                    return Err(Fault::type_error_specifier(runtime, 
                        result.get(0),
                        self.specifier.get(),
                        EvalError::new(at, "sequence elements do not fit the requested list type"),
                    ));
                }
                Ok(result.get(0))
            }
        }
    }
}

fn expand_derived_fully(runtime: &Runtime, spec: Word, at: usize) -> Result<Word, Fault> {
    let mut current = runtime.root_slot(spec);
    for _ in 0..256 {
        match super::type_check::expand_derived(runtime, current.get())? {
            Some(next) => {
                current = runtime.root_slot(next);
            }
            None => return Ok(current.get()),
        }
    }
    Err(Fault::plain(
        Class::Error,
        EvalError::new(at, "type specifier nesting exceeds the limit"),
    ))
}

fn collect(
    runtime: &Runtime,
    specifier: Word,
    depth: usize,
    at: usize,
    alternatives: &mut Vec<Alternative>,
) -> Result<(), Fault> {
    if depth == 256 {
        return Err(Fault::plain(
            Class::Error,
            EvalError::new(at, "type specifier nesting exceeds the limit"),
        ));
    }
    let specifier = runtime.root_slot(specifier);
    if let Some(expansion) = super::type_check::expand_derived(runtime, specifier.get())? {
        let expansion = runtime.root_slot(expansion);
        return collect(runtime, expansion.get(), depth + 1, at, alternatives);
    }
    let spec_word = specifier.get();
    let normalized_specifier = runtime
        .heap
        .class_info(spec_word)
        .map_or(spec_word, |info| info.name);
    let (head, rest) = runtime
        .heap
        .cons_fields(normalized_specifier)
        .unwrap_or((normalized_specifier, Word::NIL));
    let head = name(runtime, head);
    if head.as_deref() == Some("OR") {
        let mut members = Vec::new();
        let mut spine = ListSpine::new(rest);
        let fault = || invalid(runtime, specifier.get(), at);
        while let Some((member, _)) = spine.next(&runtime.heap).map_err(|_| fault())? {
            members.push(runtime.root_slot(member));
        }
        if spine.tail() != Word::NIL {
            return Err(fault());
        }
        for member in members {
            collect(runtime, member.get(), depth + 1, at, alternatives)?;
        }
        return Ok(());
    }
    alternatives.push(parse_alternative(
        runtime,
        normalized_specifier,
        head.as_deref(),
        rest,
        specifier.get(),
        at,
    )?);
    Ok(())
}

fn parse_alternative(
    runtime: &Runtime,
    normalized_specifier: Word,
    head: Option<&str>,
    mut args: Word,
    specifier: Word,
    at: usize,
) -> Result<Alternative, Fault> {
    let specifier = runtime.root_slot(specifier);
    let invalid = || invalid(runtime, specifier.get(), at);
    if runtime.heap.is_symbol(normalized_specifier) && args == Word::NIL {
        match condition::named_word(runtime, normalized_specifier, Package::TREELIST).as_deref() {
            Some("TREELIST") => {
                return Ok(Alternative {
                    kind: SequenceKind::Treelist,
                    length: Length::Any,
                });
            }
            Some("MUTABLE-TREELIST") => {
                return Ok(Alternative {
                    kind: SequenceKind::MutableTreelist,
                    length: Length::Any,
                });
            }
            _ => {}
        }
    }
    let kind = match head {
        Some("LIST") if runtime.heap.is_symbol(normalized_specifier) => {
            return Ok(Alternative {
                kind: SequenceKind::List {
                    check_cons_type: false,
                },
                length: Length::Any,
            });
        }
        Some("NULL") if runtime.heap.is_symbol(normalized_specifier) => {
            return Ok(Alternative {
                kind: SequenceKind::List {
                    check_cons_type: false,
                },
                length: Length::Exact(0),
            });
        }
        Some("CONS") => {
            let check_cons_type = args != Word::NIL;
            let length = cons_length(runtime, args, at)?.ok_or_else(invalid)?;
            return Ok(Alternative {
                kind: SequenceKind::List { check_cons_type },
                length,
            });
        }
        Some("STRING" | "SIMPLE-STRING" | "BASE-STRING" | "SIMPLE-BASE-STRING") => {
            SequenceKind::Vector(ArrayElement::Character)
        }
        Some("BIT-VECTOR" | "SIMPLE-BIT-VECTOR") => SequenceKind::Vector(ArrayElement::Bit),
        Some("SIMPLE-VECTOR") => SequenceKind::Vector(ArrayElement::General),
        Some("VECTOR" | "ARRAY" | "SIMPLE-ARRAY") => {
            let element = argument(runtime, &mut args).ok_or_else(invalid)?;
            let rest = runtime.root_slot(args);
            let kind = match element.filter(|&word| name(runtime, word).as_deref() != Some("*")) {
                None => SequenceKind::Vector(ArrayElement::General),
                Some(element) => match arrays::upgraded_type(runtime, element, at)? {
                    None => SequenceKind::NilVector,
                    Some(element) => SequenceKind::Vector(element),
                },
            };
            args = rest.get();
            kind
        }
        _ => return Err(invalid()),
    };
    let size = argument(runtime, &mut args).ok_or_else(invalid)?;
    if args != Word::NIL {
        return Err(invalid());
    }
    let size = if matches!(head, Some("ARRAY" | "SIMPLE-ARRAY")) {
        match size {
            Some(rank) if rank.as_fixnum() == Some(1) => None,
            Some(shape) => {
                let (dimension, Word::NIL) = runtime.heap.cons_fields(shape).ok_or_else(invalid)?
                else {
                    return Err(invalid());
                };
                Some(dimension)
            }
            None => return Err(invalid()),
        }
    } else {
        size
    };
    let length = size
        .filter(|&word| name(runtime, word).as_deref() != Some("*"))
        .map(|word| arrays::natural(runtime, word, at).map(Length::Exact))
        .transpose()?
        .unwrap_or(Length::Any);
    Ok(Alternative { kind, length })
}

fn invalid(runtime: &Runtime, specifier: Word, at: usize) -> Fault {
    Fault::type_error(
        runtime,
        specifier,
        "NIL",
        EvalError::new(at, "expected a recognizable list or vector type"),
    )
}

fn cons_length(runtime: &Runtime, mut args: Word, at: usize) -> Result<Option<Length>, Fault> {
    // Compound TYPEP uses the same nesting bound.
    for minimum in 1..=256 {
        if argument(runtime, &mut args).is_none() {
            return Ok(None);
        }
        let Some(cdr) = argument(runtime, &mut args) else {
            return Ok(None);
        };
        if args != Word::NIL {
            return Ok(None);
        }
        let cdr = match cdr {
            Some(cdr) => expand_derived_fully(runtime, cdr, at)?,
            None => Word::T,
        };
        match name(runtime, cdr).as_deref() {
            Some("T" | "*" | "LIST") => return Ok(Some(Length::AtLeast(minimum))),
            Some("NULL") => return Ok(Some(Length::Exact(minimum))),
            Some("CONS") => args = Word::NIL,
            _ => {
                let Some((head, next)) = runtime.heap.cons_fields(cdr) else {
                    return Ok(None);
                };
                if name(runtime, head).as_deref() != Some("CONS") {
                    return Ok(None);
                }
                args = next;
            }
        }
    }
    Ok(None)
}

fn name(runtime: &Runtime, word: Word) -> Option<String> {
    condition::named_word(runtime, word, Package::COMMON_LISP)
}

fn argument(runtime: &Runtime, args: &mut Word) -> Option<Option<Word>> {
    if *args == Word::NIL {
        return Some(None);
    }
    let (argument, rest) = runtime.heap.cons_fields(*args)?;
    *args = rest;
    Some(Some(argument))
}
