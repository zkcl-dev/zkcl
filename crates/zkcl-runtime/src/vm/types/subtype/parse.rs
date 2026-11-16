use std::rc::Rc;

use num_bigint::BigInt;
use num_traits::ToPrimitive;

use super::super::{
    ComplexPartKind, ComplexPartResolution, NumericType, arguments, bound_word, optional_argument,
    positive_integer, simd, single_argument, specifier_name, wildcard,
};
use super::{
    ArrayTy, AtomTy, Cell, ConsTy, DEPTH_LIMIT, Elem, Group, Limit, MemberTy, NumTy, Range, Rank,
    Subtype, Ty,
};
use crate::heap::{ClassKind, RootSlot, Word};
use crate::number::Number;
use crate::symbol::Name;
use crate::vm::{arrays, classes, type_check};

impl Subtype<'_> {
    pub(super) fn parse(&self, spec: Word, depth: usize) -> Ty {
        if depth > DEPTH_LIMIT {
            return Ty::Unknown;
        }
        if spec == Word::T {
            return Ty::Top;
        }
        if spec == Word::NIL {
            return Ty::Empty;
        }
        // A failing DEFTYPE expander leaves the relation undecided.
        if let Some((expander, arguments)) = type_check::derived_expander(self.runtime, spec) {
            let Ok(expansion) = type_check::expand(self.runtime, expander, arguments, 0) else {
                return Ty::Unknown;
            };
            return self.parse(expansion, depth + 1);
        }
        if let Some(class) = self.runtime.heap.class_info(spec) {
            if classes::is_system_class(self.runtime, class.origin_name) {
                return self.parse_named(class.origin_name, depth + 1);
            }
            let group = match class.kind {
                ClassKind::ForwardReferenced => return Ty::Unknown,
                ClassKind::Structure => Group::StructureObject,
                _ => declared_class_group(
                    classes::class_declaration(self.runtime, spec).map(|class| class.kind),
                ),
            };
            return Ty::Atom(AtomTy {
                word: Rc::new(self.runtime.root_slot(spec)),
                name: None,
                group,
            });
        }
        if let Some((head, arguments)) = self.runtime.heap.cons_fields(spec) {
            if let Some(simd_type) = simd::parse(self.runtime, spec) {
                return simd_type.map_or(Ty::Unknown, Ty::Simd);
            }
            let Some(operator) = specifier_name(self.runtime, head) else {
                return Ty::Unknown;
            };
            return self.parse_compound(spec, &operator, arguments, depth + 1);
        }
        if self.runtime.heap.is_symbol(spec) {
            return self.parse_named(spec, depth + 1);
        }
        Ty::Unknown
    }

    fn parse_named(&self, word: Word, depth: usize) -> Ty {
        if depth > DEPTH_LIMIT {
            return Ty::Unknown;
        }
        if word == Word::T {
            return Ty::Top;
        }
        if word == Word::NIL {
            return Ty::Empty;
        }
        if let Some(simd_type) = simd::parse(self.runtime, word) {
            return simd_type.map_or(Ty::Unknown, Ty::Simd);
        }
        let name = specifier_name(self.runtime, word);
        if let Some(name) = name.as_deref() {
            if let Some(ty) = self.numeric_named(word, name) {
                return ty;
            }
            match name {
                "CONS" => {
                    return Ty::Cons(ConsTy {
                        spec: Rc::new(self.runtime.root_slot(word)),
                        car: Box::new(Ty::Top),
                        cdr: Box::new(Ty::Top),
                    });
                }
                "LIST" => return self.list_type(),
                "SEQUENCE" => return self.sequence_type(),
                "NULL" => {
                    return Ty::Member(MemberTy {
                        values: Rc::new([self.runtime.root_slot(Word::NIL)]),
                    });
                }
                "BOOLEAN" => {
                    return Ty::Member(MemberTy {
                        values: Rc::new([
                            self.runtime.root_slot(Word::NIL),
                            self.runtime.root_slot(Word::T),
                        ]),
                    });
                }
                "EXTENDED-CHAR" => return Ty::Empty,
                name if is_array_type(name) => {
                    return self.array_type(word, name, None, depth);
                }
                _ => {}
            }
            if let Some(group) = standard_group(name) {
                return Ty::Atom(AtomTy {
                    word: Rc::new(self.runtime.root_slot(word)),
                    name: Some(name.to_owned()),
                    group,
                });
            }
        }
        let group = if self.runtime.structure_definition(word).is_some() {
            Group::StructureObject
        } else if crate::classes::declared(self.runtime, word)
            || classes::is_system_class(self.runtime, word)
            || classes::is_condition_class(self.runtime, word)
        {
            declared_class_group(
                crate::classes::named_definition(self.runtime, word)
                    .map(|class| class.kind)
                    .or_else(|| crate::classes::class_definition_kind(self.runtime, word)),
            )
        } else {
            return Ty::Unknown;
        };
        Ty::Atom(AtomTy {
            word: Rc::new(self.runtime.root_slot(word)),
            name,
            group,
        })
    }

    fn list_type(&self) -> Ty {
        let cons = self
            .runtime
            .find_named_symbol(&Name::common_lisp("CONS"))
            .unwrap_or(Word::NIL);
        Ty::Union(vec![
            Ty::Cons(ConsTy {
                spec: Rc::new(self.runtime.root_slot(cons)),
                car: Box::new(Ty::Top),
                cdr: Box::new(Ty::Top),
            }),
            Ty::Member(MemberTy {
                values: Rc::new([self.runtime.root_slot(Word::NIL)]),
            }),
        ])
    }

    fn sequence_type(&self) -> Ty {
        let mut arms = vec![self.list_type(), self.vector_type()];
        for name in ["TREELIST", "MUTABLE-TREELIST"] {
            let sym = self
                .runtime
                .packages
                .borrow()
                .present(crate::package::Package::TREELIST, name)
                .map(|(word, _)| word);
            if let Some(sym) = sym {
                arms.push(self.parse_named(sym, 1));
            }
        }
        Ty::Union(arms)
    }

    fn vector_type(&self) -> Ty {
        let spec = self
            .runtime
            .find_named_symbol(&Name::common_lisp("VECTOR"))
            .unwrap_or(Word::NIL);
        Ty::Array(ArrayTy {
            spec: Rc::new(self.runtime.root_slot(spec)),
            simple: false,
            element: Elem::Wildcard,
            rank: Rank::Dims(vec![None]),
        })
    }

    fn numeric_named(&self, spec: Word, name: &str) -> Option<Ty> {
        let (fixnum_min, fixnum_max) = self.runtime.compilation_target().fixnum_bounds();
        let cells = match name {
            "BIT" => vec![(Cell::Int, Range::closed(BigInt::from(0), BigInt::from(1)))],
            "UNSIGNED-BYTE" => vec![(
                Cell::Int,
                Range {
                    low: Some(Limit::integer(BigInt::from(0), true)),
                    high: None,
                },
            )],
            "SIGNED-BYTE" => NumericType::Integer.unbounded_cells(),
            _ => match NumericType::named(name)? {
                NumericType::Fixnum => vec![(
                    Cell::Int,
                    Range::closed(BigInt::from(fixnum_min), BigInt::from(fixnum_max)),
                )],
                NumericType::Bignum => {
                    let spec = Rc::new(self.runtime.root_slot(spec));
                    let below_fixnums = Range {
                        low: None,
                        high: Some(Limit::integer(BigInt::from(fixnum_min), false)),
                    };
                    let above_fixnums = Range {
                        low: Some(Limit::integer(BigInt::from(fixnum_max), false)),
                        high: None,
                    };
                    return Some(Ty::Union(vec![
                        Ty::Num(NumTy {
                            spec: Rc::clone(&spec),
                            cells: vec![(Cell::Int, below_fixnums)],
                        }),
                        Ty::Num(NumTy {
                            spec,
                            cells: vec![(Cell::Int, above_fixnums)],
                        }),
                    ]));
                }
                number_type => number_type.unbounded_cells(),
            },
        };
        Some(Ty::Num(NumTy {
            spec: Rc::new(self.runtime.root_slot(spec)),
            cells,
        }))
    }

    fn parse_compound(&self, spec: Word, operator: &str, operands: Word, depth: usize) -> Ty {
        if let Some(number_type) = NumericType::interval(operator) {
            return self.interval(spec, number_type.cells(), operands);
        }
        let heap = &self.runtime.heap;
        match operator {
            "AND" | "OR" => {
                let Some(items) = self.rooted_arguments(operands, 128) else {
                    return Ty::Unknown;
                };
                let mut parsed = Vec::with_capacity(items.len());
                for item in items {
                    parsed.push(self.parse(item.get(), depth + 1));
                }
                if operator == "AND" {
                    if parsed.iter().any(|item| matches!(item, Ty::Empty)) {
                        return Ty::Empty;
                    }
                    parsed.retain(|item| !matches!(item, Ty::Top));
                    match parsed.len() {
                        0 => Ty::Top,
                        1 => parsed.pop().expect("one type"),
                        _ => Ty::And(parsed),
                    }
                } else {
                    if parsed.iter().any(|item| matches!(item, Ty::Top)) {
                        return Ty::Top;
                    }
                    parsed.retain(|item| !matches!(item, Ty::Empty));
                    match parsed.len() {
                        0 => Ty::Empty,
                        1 => parsed.pop().expect("one type"),
                        _ => Ty::Union(parsed),
                    }
                }
            }
            "NOT" => {
                let Some(item) = single_argument(heap, operands) else {
                    return Ty::Unknown;
                };
                match self.parse(item, depth + 1) {
                    Ty::Empty => Ty::Top,
                    Ty::Top => Ty::Empty,
                    Ty::Not(inner) => *inner,
                    Ty::Unknown => Ty::Unknown,
                    other => Ty::Not(Box::new(other)),
                }
            }
            "MEMBER" => {
                let Some(values) = self.rooted_arguments(operands, 256) else {
                    return Ty::Unknown;
                };
                if values.is_empty() {
                    Ty::Empty
                } else {
                    Ty::Member(MemberTy {
                        values: values.into(),
                    })
                }
            }
            "EQL" => {
                let Some(value) = single_argument(heap, operands) else {
                    return Ty::Unknown;
                };
                Ty::Member(MemberTy {
                    values: Rc::new([self.runtime.root_slot(value)]),
                })
            }
            "CONS" => {
                let Some(items) = self.rooted_arguments(operands, 2) else {
                    return Ty::Unknown;
                };
                let spec = Rc::new(self.runtime.root_slot(spec));
                let mut parsed = items
                    .iter()
                    .map(|item| self.parse_component(item.get(), depth + 1));
                let car = parsed.next().unwrap_or(Ty::Top);
                let cdr = parsed.next().unwrap_or(Ty::Top);
                if matches!(car, Ty::Empty) || matches!(cdr, Ty::Empty) {
                    Ty::Empty
                } else {
                    Ty::Cons(ConsTy {
                        spec,
                        car: Box::new(car),
                        cdr: Box::new(cdr),
                    })
                }
            }
            "COMPLEX" => {
                let Some(part) = optional_argument(heap, operands) else {
                    return Ty::Unknown;
                };
                let spec = Rc::new(self.runtime.root_slot(spec));
                let kind = match part.filter(|&part| !wildcard(self.runtime, part)) {
                    None => ComplexPartKind::Real,
                    Some(part) => match self.resolve_complex_part(part, depth + 1, false) {
                        ComplexPartResolution::Upgraded(kind) => kind,
                        ComplexPartResolution::Unknown | ComplexPartResolution::NotReal => {
                            return Ty::Unknown;
                        }
                    },
                };
                if kind == ComplexPartKind::Empty {
                    Ty::Empty
                } else {
                    Ty::Num(NumTy {
                        spec,
                        cells: Cell::unbounded(kind.cells()),
                    })
                }
            }
            "MOD" => self.mod_type(spec, operands),
            "UNSIGNED-BYTE" => self.byte_type(spec, operands, false),
            "SIGNED-BYTE" => self.byte_type(spec, operands, true),
            name if is_array_type(name) => {
                let Some(items) = arguments(heap, operands, 256) else {
                    return Ty::Unknown;
                };
                self.array_type(spec, name, Some(items), depth)
            }
            _ => Ty::Unknown,
        }
    }

    fn parse_component(&self, word: Word, depth: usize) -> Ty {
        if wildcard(self.runtime, word) {
            Ty::Top
        } else {
            self.parse(word, depth + 1)
        }
    }

    fn rooted_arguments(&self, operands: Word, limit: usize) -> Option<Vec<RootSlot>> {
        arguments(&self.runtime.heap, operands, limit).map(|items| {
            items
                .into_iter()
                .map(|item| self.runtime.root_slot(item))
                .collect()
        })
    }

    fn interval(&self, spec: Word, cells: &[Cell], arguments: Word) -> Ty {
        let Some((low, high)) = self.parse_bounds(arguments) else {
            return Ty::Unknown;
        };
        let mut entries = Vec::new();
        for &cell in cells {
            let range = match cell {
                Cell::Int => Range {
                    low: low
                        .as_ref()
                        .map(|bound| Limit::integer(bound.least_integer(), true)),
                    high: high
                        .as_ref()
                        .map(|bound| Limit::integer(bound.greatest_integer(), true)),
                },
                _ => Range {
                    low: low.clone(),
                    high: high.clone(),
                },
            };
            if range.nonempty(cell) {
                entries.push((cell, range));
            }
        }
        if entries.is_empty() {
            Ty::Empty
        } else {
            Ty::Num(NumTy {
                spec: Rc::new(self.runtime.root_slot(spec)),
                cells: entries,
            })
        }
    }

    fn parse_bounds(&self, mut arguments: Word) -> Option<(Option<Limit>, Option<Limit>)> {
        let mut bounds = [None, None];
        for bound in &mut bounds {
            if arguments == Word::NIL {
                break;
            }
            let (word, rest) = self.runtime.heap.cons_fields(arguments)?;
            arguments = rest;
            *bound = self.parse_bound(word)?;
        }
        if arguments != Word::NIL {
            return None;
        }
        Some((bounds[0].take(), bounds[1].take()))
    }

    fn parse_bound(&self, word: Word) -> Option<Option<Limit>> {
        let Some((value, inclusive)) = bound_word(self.runtime, word) else {
            return Some(None);
        };
        let Some(Number::Real(real)) = self.runtime.heap.number(value) else {
            return None;
        };
        Some(Some(Limit {
            value: real.as_rational()?,
            inclusive,
        }))
    }

    fn mod_type(&self, spec: Word, arguments: Word) -> Ty {
        let heap = &self.runtime.heap;
        let Some(modulus) =
            single_argument(heap, arguments).and_then(|word| positive_integer(heap, word))
        else {
            return Ty::Unknown;
        };
        let high = &modulus - BigInt::from(1);
        Ty::Num(NumTy {
            spec: Rc::new(self.runtime.root_slot(spec)),
            cells: vec![(Cell::Int, Range::closed(BigInt::from(0), high))],
        })
    }

    fn byte_type(&self, spec: Word, arguments: Word, signed: bool) -> Ty {
        let heap = &self.runtime.heap;
        let Some(width) = optional_argument(heap, arguments) else {
            return Ty::Unknown;
        };
        let width = match width.filter(|&word| !wildcard(self.runtime, word)) {
            None => None,
            Some(word) => {
                // Keep explicit interval endpoints within the integer allocation limit.
                let Some(width) = positive_integer(heap, word)
                    .and_then(|width| width.to_u64())
                    .filter(|&width| width <= heap.max_integer_bits())
                    .and_then(|width| usize::try_from(width).ok())
                else {
                    return Ty::Unknown;
                };
                Some(width)
            }
        };
        let (low, high) = match (signed, width) {
            (false, None) => (BigInt::from(0), None),
            (false, Some(width)) => (
                BigInt::from(0),
                Some((BigInt::from(1) << width) - BigInt::from(1)),
            ),
            (true, None) => {
                return Ty::Num(NumTy {
                    spec: Rc::new(self.runtime.root_slot(spec)),
                    cells: vec![(Cell::Int, Range::unbounded())],
                });
            }
            (true, Some(width)) => {
                let half = BigInt::from(1) << (width - 1);
                (-half.clone(), Some(half - BigInt::from(1)))
            }
        };
        let range = Range {
            low: Some(Limit::integer(low, true)),
            high: high.map(|high| Limit::integer(high, true)),
        };
        Ty::Num(NumTy {
            spec: Rc::new(self.runtime.root_slot(spec)),
            cells: vec![(Cell::Int, range)],
        })
    }

    pub(super) fn resolve_complex_part(
        &self,
        part: Word,
        depth: usize,
        allow_unknown_nonempty: bool,
    ) -> ComplexPartResolution {
        let part_ty = self.parse(part, depth + 1);
        if matches!(part_ty, Ty::Empty) {
            return ComplexPartResolution::Upgraded(ComplexPartKind::Empty);
        }
        if matches!(part_ty, Ty::Unknown) {
            return ComplexPartResolution::Unknown;
        }
        let num = |number_type: NumericType| {
            Ty::Num(NumTy {
                spec: Rc::new(self.runtime.root_slot(Word::NIL)),
                cells: number_type.unbounded_cells(),
            })
        };
        let rational_ty = num(NumericType::Rational);
        let single_ty = num(NumericType::Single);
        let double_ty = num(NumericType::Double);
        let float_ty = num(NumericType::Float);
        let real_ty = num(NumericType::Real);

        match self.subset(&part_ty, &real_ty, depth + 1) {
            Some(false) => return ComplexPartResolution::NotReal,
            None => {
                if self.nonempty(&part_ty, depth + 1) == Some(false) {
                    return ComplexPartResolution::Upgraded(ComplexPartKind::Empty);
                }
                return ComplexPartResolution::Unknown;
            }
            Some(true) => {}
        }
        match self.nonempty(&part_ty, depth + 1) {
            Some(false) => return ComplexPartResolution::Upgraded(ComplexPartKind::Empty),
            None if !allow_unknown_nonempty => return ComplexPartResolution::Unknown,
            _ => {}
        }
        let kind = if self.subset(&part_ty, &rational_ty, depth + 1) == Some(true) {
            ComplexPartKind::Rational
        } else if self.subset(&part_ty, &single_ty, depth + 1) == Some(true) {
            ComplexPartKind::SingleFloat
        } else if self.subset(&part_ty, &double_ty, depth + 1) == Some(true) {
            ComplexPartKind::DoubleFloat
        } else if self.subset(&part_ty, &float_ty, depth + 1) == Some(true) {
            ComplexPartKind::Float
        } else {
            ComplexPartKind::Real
        };
        ComplexPartResolution::Upgraded(kind)
    }

    fn array_type(&self, spec: Word, name: &str, arguments: Option<Vec<Word>>, depth: usize) -> Ty {
        let spec = Rc::new(self.runtime.root_slot(spec));
        let items = arguments.unwrap_or_default();
        let (simple, element, rank) = match name {
            "ARRAY" | "SIMPLE-ARRAY" => {
                if items.len() > 2 {
                    return Ty::Unknown;
                }
                let rank = match items.get(1) {
                    None => Rank::Any,
                    Some(&word) => {
                        if word
                            .as_fixnum()
                            .is_some_and(|rank| rank >= arrays::RANK_LIMIT as isize)
                        {
                            return Ty::Empty;
                        }
                        let Some(rank) = self.rank_spec(word) else {
                            return Ty::Unknown;
                        };
                        rank
                    }
                };
                let Some(element) = self.array_element(items.first().copied(), depth + 1) else {
                    return Ty::Unknown;
                };
                (name == "SIMPLE-ARRAY", element, rank)
            }
            "VECTOR" => {
                if items.len() > 2 {
                    return Ty::Unknown;
                }
                let Some(size) = self.optional_size(items.get(1).copied()) else {
                    return Ty::Unknown;
                };
                let Some(element) = self.array_element(items.first().copied(), depth + 1) else {
                    return Ty::Unknown;
                };
                (false, element, Rank::Dims(vec![size]))
            }
            "SIMPLE-VECTOR" => {
                if items.len() > 1 {
                    return Ty::Unknown;
                }
                let Some(size) = self.optional_size(items.first().copied()) else {
                    return Ty::Unknown;
                };
                (true, Elem::General, Rank::Dims(vec![size]))
            }
            "BIT-VECTOR" | "SIMPLE-BIT-VECTOR" => {
                if items.len() > 1 {
                    return Ty::Unknown;
                }
                let Some(size) = self.optional_size(items.first().copied()) else {
                    return Ty::Unknown;
                };
                (
                    name == "SIMPLE-BIT-VECTOR",
                    Elem::Bit,
                    Rank::Dims(vec![size]),
                )
            }
            "STRING" | "SIMPLE-STRING" | "BASE-STRING" | "SIMPLE-BASE-STRING" => {
                if items.len() > 1 {
                    return Ty::Unknown;
                }
                let Some(size) = self.optional_size(items.first().copied()) else {
                    return Ty::Unknown;
                };
                let element = if name.contains("BASE-STRING") {
                    Elem::BaseChar
                } else {
                    Elem::String
                };
                (name.starts_with("SIMPLE-"), element, Rank::Dims(vec![size]))
            }
            _ => return Ty::Unknown,
        };
        if matches!(&rank, Rank::Dims(dims) if dims.len() >= arrays::RANK_LIMIT) {
            return Ty::Empty;
        }
        Ty::Array(ArrayTy {
            spec,
            simple,
            element,
            rank,
        })
    }

    fn optional_size(&self, word: Option<Word>) -> Option<Option<usize>> {
        match word {
            None => Some(None),
            Some(word) if word == Word::NIL => Some(None),
            Some(word) if wildcard(self.runtime, word) => Some(None),
            Some(word) => {
                let size = word.as_fixnum()?;
                usize::try_from(size).ok().map(Some)
            }
        }
    }

    fn rank_spec(&self, word: Word) -> Option<Rank> {
        if wildcard(self.runtime, word) {
            return Some(Rank::Any);
        }
        if word == Word::NIL {
            return Some(Rank::Dims(Vec::new()));
        }
        if let Some(rank) = word.as_fixnum() {
            let rank = usize::try_from(rank).ok()?;
            return Some(Rank::Dims(vec![None; rank]));
        }
        let items = arguments(&self.runtime.heap, word, 256)?;
        let mut dims = Vec::with_capacity(items.len());
        for item in items {
            if wildcard(self.runtime, item) {
                dims.push(None);
            } else {
                let dimension = item.as_fixnum()?;
                dims.push(Some(usize::try_from(dimension).ok()?));
            }
        }
        Some(Rank::Dims(dims))
    }

    fn array_element(&self, word: Option<Word>, depth: usize) -> Option<Elem> {
        let Some(word) = word else {
            return Some(Elem::Wildcard);
        };
        if specifier_name(self.runtime, word).as_deref() == Some("*") {
            return Some(Elem::Wildcard);
        }
        Some(
            match arrays::upgraded_type_at_depth(self.runtime, word, 0, depth).ok()? {
                None => Elem::NilElement,
                Some(crate::heap::ArrayElement::Bit) => Elem::Bit,
                Some(crate::heap::ArrayElement::Character) => Elem::BaseChar,
                Some(crate::heap::ArrayElement::SingleFloat) => Elem::SingleFloat,
                Some(crate::heap::ArrayElement::DoubleFloat) => Elem::DoubleFloat,
                Some(crate::heap::ArrayElement::Octet) => Elem::Octet,
                Some(crate::heap::ArrayElement::General) => Elem::General,
            },
        )
    }
}

fn is_array_type(name: &str) -> bool {
    matches!(
        name,
        "ARRAY"
            | "SIMPLE-ARRAY"
            | "VECTOR"
            | "SIMPLE-VECTOR"
            | "BIT-VECTOR"
            | "SIMPLE-BIT-VECTOR"
            | "STRING"
            | "SIMPLE-STRING"
            | "BASE-STRING"
            | "SIMPLE-BASE-STRING"
    )
}

fn declared_class_group(kind: Option<crate::classes::RuntimeClassKind>) -> Group {
    match kind {
        Some(crate::classes::RuntimeClassKind::Condition) => Group::Condition,
        _ => Group::StandardObject,
    }
}

fn standard_group(name: &str) -> Option<Group> {
    Some(match name {
        "ATOM" => Group::MetaAtom,
        "SYMBOL" | "KEYWORD" => Group::Symbol,
        "CHARACTER" | "BASE-CHAR" | "STANDARD-CHAR" => Group::Character,
        "CONDITION"
        | "SERIOUS-CONDITION"
        | "ERROR"
        | "TYPE-ERROR"
        | "SIMPLE-TYPE-ERROR"
        | "SIMPLE-CONDITION"
        | "SIMPLE-ERROR"
        | "SIMPLE-WARNING"
        | "PARSE-ERROR"
        | "CELL-ERROR"
        | "UNBOUND-SLOT"
        | "WARNING"
        | "STYLE-WARNING"
        | "STORAGE-CONDITION"
        | "UNBOUND-VARIABLE"
        | "CONTROL-ERROR"
        | "PROGRAM-ERROR"
        | "UNDEFINED-FUNCTION"
        | "PACKAGE-ERROR"
        | "ARITHMETIC-ERROR"
        | "DIVISION-BY-ZERO"
        | "FLOATING-POINT-INVALID-OPERATION"
        | "FLOATING-POINT-INEXACT"
        | "FLOATING-POINT-OVERFLOW"
        | "FLOATING-POINT-UNDERFLOW"
        | "FILE-ERROR"
        | "STREAM-ERROR"
        | "END-OF-FILE"
        | "PRINT-NOT-READABLE"
        | "READER-ERROR" => Group::Condition,
        "FUNCTION" | "COMPILED-FUNCTION" | "GENERIC-FUNCTION" | "STANDARD-GENERIC-FUNCTION" => {
            Group::Function
        }
        "PACKAGE" => Group::Package,
        "PATHNAME" | "LOGICAL-PATHNAME" => Group::Pathname,
        "STREAM"
        | "INPUT-STREAM"
        | "OUTPUT-STREAM"
        | "BROADCAST-STREAM"
        | "CONCATENATED-STREAM"
        | "ECHO-STREAM"
        | "FILE-STREAM"
        | "STRING-STREAM"
        | "SYNONYM-STREAM"
        | "TWO-WAY-STREAM" => Group::Stream,
        "RANDOM-STATE" => Group::RandomState,
        "RESTART" => Group::Restart,
        "READTABLE" => Group::ReadTable,
        "HASH-TABLE" => Group::HashTable,
        "CLASS"
        | "BUILT-IN-CLASS"
        | "STANDARD-CLASS"
        | "STRUCTURE-CLASS"
        | "FUNCALLABLE-STANDARD-CLASS"
        | "METHOD"
        | "STANDARD-METHOD"
        | "METHOD-COMBINATION"
        | "STANDARD-OBJECT" => Group::StandardObject,
        "STRUCTURE-OBJECT" => Group::StructureObject,
        _ => return None,
    })
}
