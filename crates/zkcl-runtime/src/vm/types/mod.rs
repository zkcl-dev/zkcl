use std::cmp::Ordering;

use num_bigint::{BigInt, Sign};

use super::type_check::{self, TYPE_NESTING_LIMIT, argument_words, satisfies_predicate};
use super::{equality::eql, is_function, numbers::real_cmp};
use crate::condition::{self, Class, Fault};
use crate::heap::{ArrayElement, Heap, ListSpine, Word};
use crate::number::{Number, NumberKind};
use crate::symbol::Package;
use crate::{EvalError, Runtime};

pub(crate) mod simd;
mod subtype;
mod type_of;

pub(super) use subtype::subtypep;
pub(crate) use subtype::upgraded_complex_part_type;
pub(super) use subtype::upgraded_numeric_array_type;
pub(super) use type_of::type_of;

// The host heap stores compiler objects. Lisp type queries must classify integers
// using the target word size even when that differs from their host storage.
fn target_number_kind(runtime: &Runtime, value: Word) -> Option<NumberKind> {
    let kind = runtime.heap.number_kind(value)?;
    if runtime.native_word_width() || !matches!(kind, NumberKind::Fixnum | NumberKind::Bignum) {
        return Some(kind);
    }
    Some(if runtime.target_fixnum_value(value).is_some() {
        NumberKind::Fixnum
    } else {
        NumberKind::Bignum
    })
}

pub(crate) fn typep(runtime: &Runtime, value: Word, spec: Word, at: usize) -> Result<bool, Fault> {
    typep_at_depth(runtime, value, spec, at, 0)
}

pub(super) fn typep_at_depth(
    runtime: &Runtime,
    value: Word,
    spec: Word,
    at: usize,
    depth: usize,
) -> Result<bool, Fault> {
    Types { runtime, at }.matches(value, spec, depth)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ComplexPartKind {
    Empty,
    Rational,
    SingleFloat,
    DoubleFloat,
    Float,
    Real,
}

enum ComplexPartResolution {
    Upgraded(ComplexPartKind),
    Unknown,
    NotReal,
}

impl ComplexPartKind {
    fn matches_number_kind(self, kind: Option<NumberKind>) -> bool {
        match self {
            Self::Empty => false,
            Self::Rational => matches!(
                kind,
                Some(NumberKind::Fixnum | NumberKind::Bignum | NumberKind::Ratio)
            ),
            Self::SingleFloat => matches!(kind, Some(NumberKind::Single)),
            Self::DoubleFloat => matches!(kind, Some(NumberKind::Double)),
            Self::Float => matches!(kind, Some(NumberKind::Single | NumberKind::Double)),
            Self::Real => true,
        }
    }
}

struct Types<'rt> {
    runtime: &'rt Runtime,
    at: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum NumericType {
    Fixnum,
    Bignum,
    Integer,
    Ratio,
    Rational,
    Single,
    Double,
    Float,
    Real,
    Complex,
    Number,
}

impl NumericType {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "FIXNUM" => Self::Fixnum,
            "BIGNUM" => Self::Bignum,
            "INTEGER" => Self::Integer,
            "RATIO" => Self::Ratio,
            "RATIONAL" => Self::Rational,
            "SHORT-FLOAT" | "SINGLE-FLOAT" => Self::Single,
            "DOUBLE-FLOAT" | "LONG-FLOAT" => Self::Double,
            "FLOAT" => Self::Float,
            "REAL" => Self::Real,
            "COMPLEX" => Self::Complex,
            "NUMBER" => Self::Number,
            _ => return None,
        })
    }

    fn contains(self, kind: NumberKind) -> bool {
        match self {
            Self::Fixnum => kind == NumberKind::Fixnum,
            Self::Bignum => kind == NumberKind::Bignum,
            Self::Integer => matches!(kind, NumberKind::Fixnum | NumberKind::Bignum),
            Self::Ratio => kind == NumberKind::Ratio,
            Self::Rational => matches!(
                kind,
                NumberKind::Fixnum | NumberKind::Bignum | NumberKind::Ratio
            ),
            Self::Single => kind == NumberKind::Single,
            Self::Double => kind == NumberKind::Double,
            Self::Float => matches!(kind, NumberKind::Single | NumberKind::Double),
            Self::Real => kind != NumberKind::Complex,
            Self::Complex => kind == NumberKind::Complex,
            Self::Number => true,
        }
    }

    fn matches(self, heap: &Heap, value: Word) -> bool {
        heap.number_kind(value)
            .is_some_and(|kind| self.contains(kind))
    }

    fn interval(name: &str) -> Option<Self> {
        match Self::named(name)? {
            ty @ (Self::Integer
            | Self::Rational
            | Self::Real
            | Self::Float
            | Self::Single
            | Self::Double) => Some(ty),
            _ => None,
        }
    }
}

type Bound = Option<(Word, bool)>;

fn string_type_matches(
    heap: &Heap,
    value: Word,
    simple: bool,
    size: Option<usize>,
    allow_nil: bool,
) -> bool {
    heap.array_info(value).is_some_and(|info| {
        info.rank == 1
            && info.element == ArrayElement::Character
            && (allow_nil || !info.nil_element)
            && (!simple || info.simple())
            && size.is_none_or(|size| info.size == size)
    })
}

impl Types<'_> {
    fn matches(&self, value: Word, spec: Word, depth: usize) -> Result<bool, Fault> {
        if depth >= TYPE_NESTING_LIMIT {
            return Err(self.error("type specifier nesting exceeds the limit"));
        }
        match spec {
            Word::T => return Ok(true),
            Word::NIL => return Ok(false),
            _ => {}
        }
        if let Some((expander, arguments)) = type_check::derived_expander(self.runtime, spec) {
            let value = self.runtime.root_slot(value);
            let expansion = type_check::expand(self.runtime, expander, arguments, self.at)?;
            let expansion = self.runtime.root_slot(expansion);
            return self.matches(value.get(), expansion.get(), depth + 1);
        }
        let heap = &self.runtime.heap;
        if let Some(class) = heap.class_info(spec) {
            return Ok(super::classes::typep(self.runtime, value, class));
        }
        if let Some(simd_type) = simd::parse(self.runtime, spec) {
            return simd_type
                .map(|simd_type| simd_type.matches(heap, value))
                .map_err(|message| self.error(message));
        }
        if let Some((head, arguments)) = heap.cons_fields(spec) {
            let name = specifier_name(self.runtime, head);
            return self.compound(value, name.as_deref(), arguments, depth + 1);
        }
        let name = specifier_name(self.runtime, spec);
        if let Some(number_type) = name.as_deref().and_then(NumericType::named) {
            return Ok(
                if matches!(number_type, NumericType::Fixnum | NumericType::Bignum) {
                    target_number_kind(self.runtime, value)
                        .is_some_and(|kind| number_type.contains(kind))
                } else {
                    number_type.matches(heap, value)
                },
            );
        }
        let matched = match name.as_deref() {
            Some("ATOM") => heap.cons_fields(value).is_none(),
            Some("BOOLEAN") => matches!(value, Word::NIL | Word::T),
            Some("HASH-TABLE") => heap.hash_table_info(value).is_some(),
            Some("RANDOM-STATE") => heap.is_random_state(value),
            Some("READTABLE") => heap.readtable_case(value).is_some(),
            Some("CONS") => heap.cons_fields(value).is_some(),
            Some("BIT") => matches!(value.as_fixnum(), Some(0 | 1)),
            Some("SIGNED-BYTE") => NumericType::Integer.matches(heap, value),
            Some("UNSIGNED-BYTE") => heap
                .integer(value)
                .is_some_and(|value| value.sign() != Sign::Minus),
            Some("FUNCTION" | "COMPILED-FUNCTION") => is_function(self.runtime, value),
            Some("LIST") => value == Word::NIL || heap.cons_fields(value).is_some(),
            Some("NULL") => value == Word::NIL,
            Some("PACKAGE") => value.package_index().is_some(),
            Some("STREAM") => super::streams::gray::streamp(self.runtime, value),
            Some("INPUT-STREAM") => super::streams::stream_is_input(self.runtime, value),
            Some("OUTPUT-STREAM") => super::streams::stream_is_output(self.runtime, value),
            Some("STRING-STREAM") => heap
                .stream_info(value)
                .is_some_and(|info| info.kind.string()),
            Some("PATHNAME") => heap.pathname_fields(value).is_some(),
            Some("LOGICAL-PATHNAME") => heap
                .pathname_fields(value)
                .is_some_and(|fields| heap.is_string(fields[0])),
            Some("RESTART") => heap.restart_name(value).is_some(),
            Some("STRING") => heap.is_string(value),
            Some("BASE-STRING") => string_type_matches(heap, value, false, None, false),
            Some("SIMPLE-STRING") => string_type_matches(heap, value, true, None, true),
            Some("SIMPLE-BASE-STRING") => string_type_matches(heap, value, true, None, false),
            Some(
                kind @ ("ARRAY" | "SIMPLE-ARRAY" | "VECTOR" | "SIMPLE-VECTOR" | "BIT-VECTOR"
                | "SIMPLE-BIT-VECTOR"),
            ) => super::arrays::matches_type(self.runtime, value, kind, Word::NIL, self.at, depth)?,
            Some("CHARACTER" | "BASE-CHAR") => value.as_character().is_some(),
            Some("STANDARD-CHAR") => value
                .as_character()
                .is_some_and(crate::character::is_standard),
            Some("EXTENDED-CHAR") => false,
            Some("RADIX") => value
                .as_fixnum()
                .is_some_and(|radix| (2..=36).contains(&radix)),
            Some("SEQUENCE") => {
                value == Word::NIL
                    || heap.cons_fields(value).is_some()
                    || super::arrays::vector_length(self.runtime, value).is_some()
                    || heap.treelist_len(value).is_some()
            }
            Some("SYMBOL") => heap.is_symbol(value),
            Some("KEYWORD") => heap.symbol_home(value) == Some(Package::KEYWORD),
            Some("STRUCTURE-OBJECT") => heap.structure_layout(value).is_some(),
            _ if self.runtime.structure_definition(spec).is_some() => {
                self.runtime.structure_is_a(value, spec)
            }
            Some(name)
                if let Some(matches) =
                    super::classes::named_includes(self.runtime, value, name) =>
            {
                matches
            }
            _ if let Some(matches) =
                super::classes::named_user_includes(self.runtime, value, spec) =>
            {
                matches
            }
            _ => return Err(self.error("unsupported type specifier")),
        };
        Ok(matched)
    }

    fn compound(
        &self,
        value: Word,
        name: Option<&str>,
        arguments: Word,
        depth: usize,
    ) -> Result<bool, Fault> {
        let heap = &self.runtime.heap;
        if let Some(number_type) = name.and_then(NumericType::interval) {
            let [lower, upper] = self.interval_bounds(number_type, arguments)?;
            if !number_type.matches(heap, value) {
                return Ok(false);
            }
            let compare = |limit: Word| {
                if let (Some(value), Some(limit)) = (value.as_fixnum(), limit.as_fixnum()) {
                    return value.cmp(&limit);
                }
                let (Some(Number::Real(value)), Some(Number::Real(limit))) =
                    (heap.number(value), heap.number(limit))
                else {
                    unreachable!("interval members and bounds are real numbers");
                };
                real_cmp(&value, &limit)
            };
            let above = lower.is_none_or(|(limit, inclusive)| match compare(limit) {
                Ordering::Greater => true,
                Ordering::Equal => inclusive,
                Ordering::Less => false,
            });
            let below = upper.is_none_or(|(limit, inclusive)| match compare(limit) {
                Ordering::Less => true,
                Ordering::Equal => inclusive,
                Ordering::Greater => false,
            });
            return Ok(above && below);
        }
        match name {
            Some("CONS") => self.cons_type(value, arguments, depth),
            Some(
                kind @ ("ARRAY" | "SIMPLE-ARRAY" | "VECTOR" | "SIMPLE-VECTOR" | "BIT-VECTOR"
                | "SIMPLE-BIT-VECTOR"),
            ) => super::arrays::matches_type(self.runtime, value, kind, arguments, self.at, depth),
            Some(kind @ ("STRING" | "SIMPLE-STRING" | "BASE-STRING" | "SIMPLE-BASE-STRING")) => {
                let size = if arguments == Word::NIL {
                    None
                } else {
                    let size = single_argument(heap, arguments)
                        .ok_or_else(|| self.error("expected one string size"))?;
                    if wildcard(self.runtime, size) {
                        None
                    } else {
                        Some(
                            size.as_fixnum()
                                .and_then(|size| usize::try_from(size).ok())
                                .ok_or_else(|| {
                                    self.error("expected a non-negative string size or *")
                                })?,
                        )
                    }
                };
                Ok(string_type_matches(
                    heap,
                    value,
                    kind.starts_with("SIMPLE-"),
                    size,
                    !kind.contains("BASE-STRING"),
                ))
            }
            Some(op @ ("AND" | "OR")) => {
                let all = op == "AND";
                let value = self.runtime.root_slot(value);
                let arguments: Vec<_> = argument_words(self.runtime, arguments, self.at)?
                    .into_iter()
                    .map(|argument| self.runtime.root_slot(argument))
                    .collect();
                for argument in arguments {
                    let matched = self.matches(value.get(), argument.get(), depth)?;
                    if matched != all {
                        return Ok(matched);
                    }
                }
                Ok(all)
            }
            Some("MEMBER") => {
                let invalid = || self.error("expected a proper type argument list");
                let mut spine = ListSpine::new(arguments);
                let mut result = false;
                while let Some((argument, _)) = spine.next(heap).map_err(|_| invalid())? {
                    result = result || eql(heap, value, argument);
                }
                if spine.tail() != Word::NIL {
                    return Err(invalid());
                }
                Ok(result)
            }
            Some("NOT" | "EQL") => {
                let argument = single_argument(heap, arguments)
                    .ok_or_else(|| self.error("expected one type argument"))?;
                if name == Some("EQL") {
                    Ok(eql(heap, value, argument))
                } else {
                    Ok(!self.matches(value, argument, depth)?)
                }
            }
            Some("COMPLEX") => {
                let value = self.runtime.root_slot(value);
                let Some(part) = self
                    .one_argument(arguments)?
                    .filter(|&part| !wildcard(self.runtime, part))
                else {
                    return Ok(NumericType::Complex.matches(heap, value.get()));
                };
                let part = self.runtime.root_slot(part);
                let resolution = subtype::resolve_complex_part(self.runtime, part.get(), depth + 1);
                match resolution {
                    ComplexPartResolution::Upgraded(kind) => {
                        let Some((real, _)) = heap.complex_fields(value.get()) else {
                            return Ok(false);
                        };
                        Ok(kind.matches_number_kind(heap.number_kind(real)))
                    }
                    ComplexPartResolution::NotReal => {
                        Err(self.error("COMPLEX part type specifier must be a subtype of REAL"))
                    }
                    ComplexPartResolution::Unknown => {
                        let Some((real, imaginary)) = heap.complex_fields(value.get()) else {
                            return Ok(false);
                        };
                        let imaginary = self.runtime.root_slot(imaginary);
                        Ok(self.matches(real, part.get(), depth + 1)?
                            && self.matches(imaginary.get(), part.get(), depth + 1)?)
                    }
                }
            }
            Some("SATISFIES") => {
                let predicate = single_argument(heap, arguments)
                    .ok_or_else(|| self.error("expected one predicate name"))?;
                let predicate = satisfies_predicate(self.runtime, predicate, self.at)?;
                let target = self.runtime.root(predicate);
                let argument = self.runtime.root(value);
                let values = self
                    .runtime
                    .call(&target, &[&argument])
                    .map_err(type_check::callback_fault)?;
                Ok(values
                    .first()
                    .is_some_and(|result| result.slot.get() != Word::NIL))
            }
            Some("SIGNED-BYTE") => self.byte_type(value, arguments, true),
            Some("UNSIGNED-BYTE") => self.byte_type(value, arguments, false),
            Some("MOD") => {
                let bound = self
                    .one_argument(arguments)?
                    .and_then(|argument| positive_integer(heap, argument))
                    .ok_or_else(|| self.error("expected a positive integer type argument"))?;
                Ok(heap
                    .integer(value)
                    .is_some_and(|number| number.sign() != Sign::Minus && number < bound))
            }
            _ => Err(self.error("unsupported compound type specifier")),
        }
    }

    fn cons_type(&self, value: Word, mut arguments: Word, depth: usize) -> Result<bool, Fault> {
        let heap = &self.runtime.heap;
        let invalid = || self.error("CONS expects up to two type specifiers");
        let mut types = [None, None];
        for specifier in &mut types {
            if arguments == Word::NIL {
                break;
            }
            let (spec, rest) = heap.cons_fields(arguments).ok_or_else(invalid)?;
            *specifier = Some(self.runtime.root_slot(spec));
            arguments = rest;
        }
        if arguments != Word::NIL {
            return Err(invalid());
        }
        let Some((car, cdr)) = heap.cons_fields(value) else {
            return Ok(false);
        };
        let fields = [self.runtime.root_slot(car), self.runtime.root_slot(cdr)];
        for (field, spec) in fields.into_iter().zip(types) {
            if let Some(spec) = spec
                && !wildcard(self.runtime, spec.get())
                && !self.matches(field.get(), spec.get(), depth)?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn interval_bounds(
        &self,
        number_type: NumericType,
        mut arguments: Word,
    ) -> Result<[Bound; 2], Fault> {
        let mut bounds = [None, None];
        for bound in &mut bounds {
            if arguments == Word::NIL {
                break;
            }
            let (limit, rest) = self
                .runtime
                .heap
                .cons_fields(arguments)
                .ok_or_else(|| self.error("expected an interval bound"))?;
            *bound = self.bound(number_type, limit)?;
            arguments = rest;
        }
        if arguments != Word::NIL {
            return Err(self.error("expected at most two interval bounds"));
        }
        Ok(bounds)
    }

    fn bound(&self, number_type: NumericType, limit: Word) -> Result<Bound, Fault> {
        let Some((word, inclusive)) = bound_word(self.runtime, limit) else {
            return Ok(None);
        };
        let heap = &self.runtime.heap;
        if number_type.matches(heap, word) {
            return Ok(Some((word, inclusive)));
        }
        Err(self.error("expected an interval bound of the specified numeric type"))
    }

    fn one_argument(&self, arguments: Word) -> Result<Option<Word>, Fault> {
        optional_argument(&self.runtime.heap, arguments)
            .ok_or_else(|| self.error("expected at most one type argument"))
    }

    fn byte_width(&self, arguments: Word) -> Result<Option<BigInt>, Fault> {
        self.one_argument(arguments)?
            .filter(|&width| !wildcard(self.runtime, width))
            .map(|width| {
                positive_integer(&self.runtime.heap, width)
                    .ok_or_else(|| self.error("expected a positive integer type argument"))
            })
            .transpose()
    }

    fn byte_type(&self, value: Word, arguments: Word, signed: bool) -> Result<bool, Fault> {
        let width = self.byte_width(arguments)?;
        let Some(number) = self.runtime.heap.integer(value) else {
            return Ok(false);
        };
        if !signed && number.sign() == Sign::Minus {
            return Ok(false);
        }
        if let Some(width) = width {
            // Comparing widths avoids constructing 2^width for large type arguments.
            let bits = if signed && number.sign() == Sign::Minus {
                (!number).bits()
            } else {
                number.bits()
            };
            let bits = BigInt::from(bits);
            return Ok(if signed { bits < width } else { bits <= width });
        }
        Ok(true)
    }

    fn error(&self, message: &'static str) -> Fault {
        Fault::plain(Class::Error, EvalError::new(self.at, message))
    }
}

pub(super) fn specifier_name(runtime: &Runtime, word: Word) -> Option<String> {
    condition::named_word(runtime, word, Package::COMMON_LISP)
}

fn wildcard(runtime: &Runtime, word: Word) -> bool {
    specifier_name(runtime, word).as_deref() == Some("*")
}

pub(super) fn single_argument(heap: &Heap, arguments: Word) -> Option<Word> {
    let (argument, Word::NIL) = heap.cons_fields(arguments)? else {
        return None;
    };
    Some(argument)
}

fn optional_argument(heap: &Heap, arguments: Word) -> Option<Option<Word>> {
    if arguments == Word::NIL {
        Some(None)
    } else {
        single_argument(heap, arguments).map(Some)
    }
}

pub(super) fn arguments(heap: &Heap, arguments: Word, limit: usize) -> Option<Vec<Word>> {
    let mut values = Vec::new();
    let mut spine = ListSpine::new(arguments);
    while let Some((item, _)) = spine.next(heap).ok()? {
        if values.len() == limit {
            return None;
        }
        values.push(item);
    }
    (spine.tail() == Word::NIL).then_some(values)
}

fn bound_word(runtime: &Runtime, word: Word) -> Bound {
    if wildcard(runtime, word) {
        return None;
    }
    Some(match single_argument(&runtime.heap, word) {
        Some(bound) => (bound, false),
        None => (word, true),
    })
}

fn positive_integer(heap: &Heap, word: Word) -> Option<BigInt> {
    heap.integer(word)
        .filter(|value| value.sign() == Sign::Plus)
}
