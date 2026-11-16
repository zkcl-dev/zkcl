use std::rc::Rc;

use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::ToPrimitive;

use super::{
    ComplexPartKind, ComplexPartResolution, NumericType, simd::SimdTypes, specifier_name, wildcard,
};
use crate::condition::{Class, Fault};
use crate::heap::{ArrayElement, Heap, RootSlot, Word};
use crate::number::{Number, NumberKind, Real};
use crate::symbol::Name;
use crate::vm::{classes, equality::eql};
use crate::{EvalError, Runtime};

mod parse;

/// The subtype relation as a three-valued decision. Some(true) is a proved
/// subtype, Some(false) a proved non-subtype, and None is the standard's
/// (values nil nil).
type Decision = Option<bool>;

const DEPTH_LIMIT: usize = 32;
const CLAUSE_LIMIT: usize = 256;

/// Combine decisions in order, skipping later checks once the result is known.
fn combine_decisions(decisions: impl Iterator<Item = Decision>, all: bool) -> Decision {
    let mut unknown = false;
    for decision in decisions {
        match decision {
            Some(result) if result != all => return Some(result),
            Some(_) => {}
            None => unknown = true,
        }
    }
    (!unknown).then_some(all)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cell {
    Int,
    Ratio,
    Single,
    Double,
    ComplexRational,
    ComplexSingle,
    ComplexDouble,
}

impl Cell {
    fn real_kind(kind: NumberKind) -> Option<Self> {
        Some(match kind {
            NumberKind::Fixnum | NumberKind::Bignum => Self::Int,
            NumberKind::Ratio => Self::Ratio,
            NumberKind::Single => Self::Single,
            NumberKind::Double => Self::Double,
            NumberKind::Complex => return None,
        })
    }

    fn unbounded(cells: &[Self]) -> Vec<(Self, Range)> {
        cells
            .iter()
            .map(|&cell| (cell, Range::unbounded()))
            .collect()
    }
}

impl NumericType {
    fn cells(self) -> &'static [Cell] {
        use Cell::*;
        match self {
            Self::Fixnum | Self::Bignum | Self::Integer => &[Int],
            Self::Ratio => &[Ratio],
            Self::Rational => &[Int, Ratio],
            Self::Single => &[Single],
            Self::Double => &[Double],
            Self::Float => &[Single, Double],
            Self::Real => &[Int, Ratio, Single, Double],
            Self::Complex => ComplexPartKind::Real.cells(),
            Self::Number => &[
                Int,
                Ratio,
                Single,
                Double,
                ComplexRational,
                ComplexSingle,
                ComplexDouble,
            ],
        }
    }

    fn unbounded_cells(self) -> Vec<(Cell, Range)> {
        Cell::unbounded(self.cells())
    }
}

impl ComplexPartKind {
    fn cells(self) -> &'static [Cell] {
        use Cell::*;
        match self {
            Self::Empty => &[],
            Self::Rational => &[ComplexRational],
            Self::SingleFloat => &[ComplexSingle],
            Self::DoubleFloat => &[ComplexDouble],
            Self::Float => &[ComplexSingle, ComplexDouble],
            Self::Real => &[ComplexRational, ComplexSingle, ComplexDouble],
        }
    }

    fn type_name(self) -> &'static str {
        match self {
            Self::Empty => "NIL",
            Self::Rational => "RATIONAL",
            Self::SingleFloat => "SINGLE-FLOAT",
            Self::DoubleFloat => "DOUBLE-FLOAT",
            Self::Float => "FLOAT",
            Self::Real => "REAL",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    MetaAtom,
    Symbol,
    Character,
    Function,
    Package,
    Pathname,
    Stream,
    RandomState,
    Condition,
    Restart,
    ReadTable,
    HashTable,
    StandardObject,
    StructureObject,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Elem {
    Wildcard,
    General,
    Bit,
    Octet,
    SingleFloat,
    DoubleFloat,
    BaseChar,
    NilElement,
    String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Rank {
    Any,
    Dims(Vec<Option<usize>>),
}

#[derive(Clone)]
struct AtomTy {
    word: Rc<RootSlot>,
    name: Option<String>,
    group: Group,
}

#[derive(Clone)]
struct MemberTy {
    values: Rc<[RootSlot]>,
}

impl MemberTy {
    fn contains(&self, heap: &Heap, value: Word) -> bool {
        self.values
            .iter()
            .any(|candidate| eql(heap, value, candidate.get()))
    }
}

#[derive(Clone)]
struct ConsTy {
    spec: Rc<RootSlot>,
    car: Box<Ty>,
    cdr: Box<Ty>,
}

#[derive(Clone)]
struct ArrayTy {
    spec: Rc<RootSlot>,
    simple: bool,
    element: Elem,
    rank: Rank,
}

#[derive(Clone)]
struct NumTy {
    spec: Rc<RootSlot>,
    cells: Vec<(Cell, Range)>,
}

#[derive(Clone)]
enum Ty {
    Empty,
    Top,
    Unknown,
    Atom(AtomTy),
    Num(NumTy),
    Cons(ConsTy),
    Array(ArrayTy),
    Simd(SimdTypes),
    Member(MemberTy),
    Union(Vec<Ty>),
    And(Vec<Ty>),
    Not(Box<Ty>),
}

#[derive(Clone)]
struct Clause {
    pos: Vec<Ty>,
    neg: Vec<Ty>,
}

impl Clause {
    fn new() -> Self {
        Self {
            pos: Vec::new(),
            neg: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct Limit {
    value: BigRational,
    inclusive: bool,
}

impl Limit {
    fn integer(value: BigInt, inclusive: bool) -> Self {
        Self {
            value: BigRational::from_integer(value),
            inclusive,
        }
    }

    fn least_integer(&self) -> BigInt {
        if self.inclusive {
            self.value.ceil().to_integer()
        } else {
            self.value.floor().to_integer() + 1
        }
    }

    fn greatest_integer(&self) -> BigInt {
        if self.inclusive {
            self.value.floor().to_integer()
        } else {
            self.value.ceil().to_integer() - 1
        }
    }
}

#[derive(Clone)]
struct Range {
    low: Option<Limit>,
    high: Option<Limit>,
}

impl Range {
    fn unbounded() -> Self {
        Self {
            low: None,
            high: None,
        }
    }

    fn closed(low: BigInt, high: BigInt) -> Self {
        Self {
            low: Some(Limit::integer(low, true)),
            high: Some(Limit::integer(high, true)),
        }
    }

    fn intersect(&self, other: &Range) -> Range {
        let low = match (&self.low, &other.low) {
            (None, bound) | (bound, None) => bound.clone(),
            (Some(left), Some(right)) => Some(if left.value > right.value {
                left.clone()
            } else if right.value > left.value {
                right.clone()
            } else {
                Limit {
                    value: left.value.clone(),
                    inclusive: left.inclusive && right.inclusive,
                }
            }),
        };
        let high = match (&self.high, &other.high) {
            (None, bound) | (bound, None) => bound.clone(),
            (Some(left), Some(right)) => Some(if left.value < right.value {
                left.clone()
            } else if right.value < left.value {
                right.clone()
            } else {
                Limit {
                    value: left.value.clone(),
                    inclusive: left.inclusive && right.inclusive,
                }
            }),
        };
        Range { low, high }
    }

    fn contains_value(&self, value: &BigRational) -> bool {
        let above_low = match &self.low {
            None => true,
            Some(limit) if limit.inclusive => value >= &limit.value,
            Some(limit) => value > &limit.value,
        };
        let above_high = match &self.high {
            None => true,
            Some(limit) if limit.inclusive => value <= &limit.value,
            Some(limit) => value < &limit.value,
        };
        above_low && above_high
    }

    fn contains(&self, other: &Range) -> bool {
        let low_ok = match (&self.low, &other.low) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(outer), Some(inner)) => {
                outer.value < inner.value
                    || (outer.value == inner.value && (outer.inclusive || !inner.inclusive))
            }
        };
        let high_ok = match (&self.high, &other.high) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(outer), Some(inner)) => {
                outer.value > inner.value
                    || (outer.value == inner.value && (outer.inclusive || !inner.inclusive))
            }
        };
        low_ok && high_ok
    }

    fn subtract(&self, other: &Range) -> Vec<Range> {
        if !self.intersect(other).real_nonempty() {
            return vec![self.clone()];
        }
        let mut pieces = Vec::new();
        if let Some(low) = &other.low {
            let mut below = self.clone();
            below.high = Some(Limit {
                value: low.value.clone(),
                inclusive: !low.inclusive,
            });
            if below.real_nonempty() {
                pieces.push(below);
            }
        }
        if let Some(high) = &other.high {
            let mut above = self.clone();
            above.low = Some(Limit {
                value: high.value.clone(),
                inclusive: !high.inclusive,
            });
            if above.real_nonempty() {
                pieces.push(above);
            }
        }
        pieces
    }

    fn real_nonempty(&self) -> bool {
        match (&self.low, &self.high) {
            (Some(low), Some(high)) => {
                low.value < high.value
                    || (low.value == high.value && low.inclusive && high.inclusive)
            }
            _ => true,
        }
    }

    fn nonempty(&self, cell: Cell) -> bool {
        match cell {
            Cell::Int => match (&self.low, &self.high) {
                (Some(low), Some(high)) => low.least_integer() <= high.greatest_integer(),
                _ => true,
            },
            Cell::ComplexRational | Cell::ComplexSingle | Cell::ComplexDouble => {
                self.real_nonempty()
            }
            Cell::Ratio => match (&self.low, &self.high) {
                (Some(low), Some(high)) if low.value == high.value => {
                    low.inclusive && high.inclusive && !low.value.is_integer()
                }
                _ => self.real_nonempty(),
            },
            Cell::Single | Cell::Double => match (&self.low, &self.high) {
                (Some(low), Some(high)) if low.value == high.value => {
                    low.inclusive && high.inclusive && float_representable(cell, &low.value)
                }
                _ => self.real_nonempty(),
            },
        }
    }
}

fn float_representable(cell: Cell, value: &BigRational) -> bool {
    match cell {
        Cell::Single => value
            .to_f32()
            .and_then(|single| BigRational::from_float(f64::from(single)))
            .is_some_and(|round| round == *value),
        Cell::Double => value
            .to_f64()
            .and_then(BigRational::from_float)
            .is_some_and(|round| round == *value),
        _ => true,
    }
}

struct Subtype<'rt> {
    runtime: &'rt Runtime,
}

pub(in crate::vm) fn subtypep(runtime: &Runtime, spec1: Word, spec2: Word) -> (bool, bool) {
    if spec1 == spec2 || spec2 == Word::T || spec1 == Word::NIL {
        return (true, true);
    }
    let engine = Subtype { runtime };
    // Expanding the first specifier can call Lisp and relocate the second.
    let spec2 = runtime.root_slot(spec2);
    let source = engine.parse(spec1, 0);
    let target = engine.parse(spec2.get(), 0);
    match engine.subset(&source, &target, 0) {
        Some(true) => (true, true),
        Some(false) => (false, true),
        None => (false, false),
    }
}

impl<'rt> Subtype<'rt> {
    fn base_matches(&self, value: Word, spec: Word, depth: usize) -> Decision {
        super::typep_at_depth(self.runtime, value, spec, 0, depth).ok()
    }

    fn value_matches(&self, value: &RootSlot, ty: &Ty, depth: usize) -> Decision {
        if depth > DEPTH_LIMIT {
            return None;
        }
        match ty {
            Ty::Empty => Some(false),
            Ty::Top => Some(true),
            Ty::Unknown => None,
            Ty::Atom(atom) => {
                if atom.name.as_deref() == Some("ATOM") {
                    Some(self.runtime.heap.cons_fields(value.get()).is_none())
                } else {
                    self.base_matches(value.get(), atom.word.get(), depth)
                }
            }
            Ty::Num(num) => self.num_value_matches(value.get(), num),
            Ty::Cons(cons) => self.base_matches(value.get(), cons.spec.get(), depth),
            Ty::Array(array) => self.base_matches(value.get(), array.spec.get(), depth),
            Ty::Simd(simd) => Some(simd.matches(&self.runtime.heap, value.get())),
            Ty::Member(member) => Some(member.contains(&self.runtime.heap, value.get())),
            Ty::Union(items) => combine_decisions(
                items
                    .iter()
                    .map(|item| self.value_matches(value, item, depth + 1)),
                false,
            ),
            Ty::And(items) => combine_decisions(
                items
                    .iter()
                    .map(|item| self.value_matches(value, item, depth + 1)),
                true,
            ),
            Ty::Not(item) => self
                .value_matches(value, item, depth + 1)
                .map(|matches| !matches),
        }
    }

    fn num_value_matches(&self, value: Word, num: &NumTy) -> Decision {
        let Some(kind) = self.runtime.heap.number_kind(value) else {
            return Some(false);
        };
        if kind == NumberKind::Complex {
            let Some(Number::Complex(real, imaginary)) = self.runtime.heap.number(value) else {
                return Some(false);
            };
            let part = match (&real, &imaginary) {
                (Real::Integer(_) | Real::Ratio(_), Real::Integer(_) | Real::Ratio(_)) => {
                    Cell::ComplexRational
                }
                (Real::Single(_), Real::Single(_)) => Cell::ComplexSingle,
                _ => Cell::ComplexDouble,
            };
            return Some(num.cells.iter().any(|(cell, _)| *cell == part));
        }
        let Some(Number::Real(real)) = self.runtime.heap.number(value) else {
            return Some(false);
        };
        let number = real.as_rational();
        let expected_cell = Cell::real_kind(kind);
        let mut unknown = false;
        for (cell, range) in &num.cells {
            if Some(*cell) != expected_cell {
                continue;
            }
            match &number {
                Some(number) if range.contains_value(number) => return Some(true),
                Some(_) => {}
                None => unknown = true,
            }
        }
        if unknown { None } else { Some(false) }
    }

    fn subset(&self, source: &Ty, target: &Ty, depth: usize) -> Decision {
        if depth > DEPTH_LIMIT {
            return None;
        }
        match (source, target) {
            (Ty::Empty, _) => Some(true),
            (_, Ty::Top) => Some(true),
            (_, Ty::Empty) => self.nonempty(source, depth + 1).map(|nonempty| !nonempty),
            (Ty::Top, _) => self
                .nonempty(&Ty::Not(Box::new(target.clone())), depth + 1)
                .map(|nonempty| !nonempty),
            (Ty::Unknown, _) | (_, Ty::Unknown) => None,
            (Ty::Union(items), _) => combine_decisions(
                items
                    .iter()
                    .map(|item| self.subset(item, target, depth + 1)),
                true,
            ),
            (Ty::And(items), _) => {
                for item in items {
                    if self.subset(item, target, depth + 1) == Some(true) {
                        return Some(true);
                    }
                }
                self.exhaustive(source, target, depth)
            }
            (_, Ty::Union(items)) => {
                for item in items {
                    if self.subset(source, item, depth + 1) == Some(true) {
                        return Some(true);
                    }
                }
                self.exhaustive(source, target, depth)
            }
            (_, Ty::And(items)) => combine_decisions(
                items
                    .iter()
                    .map(|item| self.subset(source, item, depth + 1)),
                true,
            )
            .or_else(|| self.exhaustive(source, target, depth)),
            (Ty::Not(source), Ty::Not(target)) => self.subset(target, source, depth + 1),
            (_, Ty::Not(target)) => self.disjoint(source, target, depth + 1),
            (Ty::Not(_), _) => self.exhaustive(source, target, depth),
            _ => match self.subset_primitive(source, target, depth + 1) {
                Some(decision) => Some(decision),
                None => self.exhaustive(source, target, depth),
            },
        }
    }

    fn exhaustive(&self, source: &Ty, target: &Ty, depth: usize) -> Decision {
        let difference = Ty::And(vec![source.clone(), Ty::Not(Box::new(target.clone()))]);
        self.nonempty(&difference, depth + 1)
            .map(|nonempty| !nonempty)
    }

    fn subset_primitive(&self, source: &Ty, target: &Ty, depth: usize) -> Decision {
        match (source, target) {
            (Ty::Num(source), Ty::Num(target)) => self.num_subset(source, target, depth),
            (Ty::Num(_), Ty::Atom(target)) => Some(target.name.as_deref() == Some("ATOM")),
            (Ty::Num(_), Ty::Cons(_)) | (Ty::Num(_), Ty::Array(_)) => Some(false),
            (Ty::Num(source), Ty::Member(target)) => self.num_subset_member(source, target),
            (Ty::Atom(source), Ty::Atom(target)) => self.atom_subset(source, target),
            (Ty::Atom(_), Ty::Num(_)) | (Ty::Atom(_), Ty::Cons(_)) => Some(false),
            (Ty::Atom(_), Ty::Array(_)) => Some(false),
            (Ty::Atom(_), Ty::Member(_)) => None,
            (Ty::Cons(source), Ty::Cons(target)) => self.cons_subset(source, target, depth),
            (Ty::Cons(_), Ty::Atom(target)) => Some(matches!(
                target.name.as_deref(),
                Some("LIST") | Some("SEQUENCE")
            )),
            (Ty::Cons(_), Ty::Num(_)) | (Ty::Cons(_), Ty::Array(_)) => Some(false),
            (Ty::Cons(_), Ty::Member(_)) => Some(false),
            (Ty::Array(source), Ty::Array(target)) => self.array_subset(source, target),
            (Ty::Array(source), Ty::Atom(target)) => Some(match target.name.as_deref() {
                Some("ATOM") => true,
                Some("SEQUENCE") => array_rank_len(source) == Some(1),
                _ => false,
            }),
            (Ty::Array(_), Ty::Num(_)) | (Ty::Array(_), Ty::Cons(_)) => Some(false),
            (Ty::Array(_), Ty::Member(_)) => None,
            (Ty::Simd(source), Ty::Simd(target)) => Some(source.difference(*target).is_empty()),
            (Ty::Simd(_), Ty::Atom(target)) => Some(target.name.as_deref() == Some("ATOM")),
            (Ty::Simd(_), _)
            | (Ty::Atom(_) | Ty::Num(_) | Ty::Cons(_) | Ty::Array(_), Ty::Simd(_)) => Some(false),
            (Ty::Member(source), Ty::Member(target)) => self.member_subset(source, target),
            (Ty::Member(source), _) => self.member_targets(source, target, depth),
            (_, Ty::Member(_)) => None,
            _ => None,
        }
    }

    fn disjoint(&self, left: &Ty, right: &Ty, depth: usize) -> Decision {
        if depth > DEPTH_LIMIT {
            return None;
        }
        match (left, right) {
            (Ty::Empty, _) | (_, Ty::Empty) => Some(true),
            (Ty::Top, _) | (_, Ty::Top) => Some(false),
            (Ty::Unknown, _) | (_, Ty::Unknown) => None,
            (Ty::Union(items), _) => combine_decisions(
                items
                    .iter()
                    .map(|item| self.disjoint(item, right, depth + 1)),
                true,
            ),
            (_, Ty::Union(items)) => combine_decisions(
                items
                    .iter()
                    .map(|item| self.disjoint(left, item, depth + 1)),
                true,
            ),
            (Ty::And(items), _) => {
                for item in items {
                    if self.disjoint(item, right, depth + 1) == Some(true) {
                        return Some(true);
                    }
                }
                self.nonempty(&Ty::And(vec![left.clone(), right.clone()]), depth + 1)
                    .map(|nonempty| !nonempty)
            }
            (_, Ty::And(items)) => {
                for item in items {
                    if self.disjoint(left, item, depth + 1) == Some(true) {
                        return Some(true);
                    }
                }
                self.nonempty(&Ty::And(vec![left.clone(), right.clone()]), depth + 1)
                    .map(|nonempty| !nonempty)
            }
            (Ty::Not(_), Ty::Not(_)) => self
                .nonempty(&Ty::And(vec![left.clone(), right.clone()]), depth + 1)
                .map(|nonempty| !nonempty),
            (Ty::Not(inner), _) => self.subset(right, inner, depth + 1),
            (_, Ty::Not(inner)) => self.subset(left, inner, depth + 1),
            _ => match self.disjoint_primitive(left, right, depth) {
                Some(decision) => Some(decision),
                None => self
                    .nonempty(&Ty::And(vec![left.clone(), right.clone()]), depth + 1)
                    .map(|nonempty| !nonempty),
            },
        }
    }

    fn disjoint_primitive(&self, left: &Ty, right: &Ty, depth: usize) -> Decision {
        match (left, right) {
            (Ty::Num(left), Ty::Num(right)) => self.num_disjoint(left, right),
            (Ty::Num(_), Ty::Atom(target)) => Some(target.name.as_deref() != Some("ATOM")),
            (Ty::Num(num), Ty::Member(member)) => {
                Some(!member.values.iter().any(|value| {
                    self.base_matches(value.get(), num.spec.get(), depth) == Some(true)
                }))
            }
            (Ty::Num(_), Ty::Cons(_)) | (Ty::Num(_), Ty::Array(_)) => Some(true),
            (Ty::Atom(source), Ty::Atom(target)) => self.atom_disjoint(source, target),
            (Ty::Atom(atom), Ty::Num(_)) => Some(atom.name.as_deref() != Some("ATOM")),
            (Ty::Atom(_), Ty::Cons(_)) => Some(true),
            (Ty::Atom(atom), Ty::Array(_)) => Some(atom.name.as_deref() != Some("ATOM")),
            (Ty::Atom(atom), Ty::Member(member)) => {
                if atom.name.as_deref() == Some("ATOM") {
                    return Some(false);
                }
                combine_decisions(
                    member
                        .values
                        .iter()
                        .map(|value| self.value_matches(value, left, depth + 1)),
                    false,
                )
                .map(|matches| !matches)
            }
            (Ty::Cons(_), Ty::Cons(_)) => None,
            (Ty::Cons(_), Ty::Atom(atom)) => Some(!matches!(
                atom.name.as_deref(),
                Some("LIST") | Some("SEQUENCE")
            )),
            (Ty::Cons(_), Ty::Num(_)) | (Ty::Cons(_), Ty::Array(_)) => Some(true),
            (Ty::Cons(_), Ty::Member(_)) => None,
            (Ty::Array(array), Ty::Array(target)) => self
                .array_intersection(array, target)
                .is_none()
                .then_some(true),
            (Ty::Array(array), Ty::Atom(atom)) => match atom.name.as_deref() {
                Some("ATOM") => Some(false),
                Some("SEQUENCE") => match array.rank {
                    Rank::Any => None,
                    Rank::Dims(ref dims) => Some(dims.len() != 1),
                },
                _ => Some(true),
            },
            (Ty::Array(_), Ty::Num(_)) | (Ty::Array(_), Ty::Cons(_)) => Some(true),
            (Ty::Array(_), Ty::Member(_)) => None,
            (Ty::Simd(left), Ty::Simd(right)) => Some(left.intersection(*right).is_empty()),
            (Ty::Simd(_), Ty::Atom(atom)) | (Ty::Atom(atom), Ty::Simd(_)) => {
                Some(atom.name.as_deref() != Some("ATOM"))
            }
            (Ty::Simd(_), Ty::Num(_) | Ty::Cons(_) | Ty::Array(_))
            | (Ty::Num(_) | Ty::Cons(_) | Ty::Array(_), Ty::Simd(_)) => Some(true),
            (Ty::Simd(simd), Ty::Member(member)) => Some(
                !member
                    .values
                    .iter()
                    .any(|value| simd.matches(&self.runtime.heap, value.get())),
            ),
            (Ty::Member(member), _) => combine_decisions(
                member
                    .values
                    .iter()
                    .map(|value| self.value_matches(value, right, depth + 1)),
                false,
            )
            .map(|matches| !matches),
            (_, Ty::Member(_)) => None,
            _ => None,
        }
    }

    fn nonempty(&self, ty: &Ty, depth: usize) -> Decision {
        if depth > DEPTH_LIMIT {
            return None;
        }
        let clauses = self.dnf(ty, depth + 1)?;
        combine_decisions(
            clauses
                .iter()
                .map(|clause| self.satisfiable(clause, depth + 1)),
            false,
        )
    }

    fn dnf(&self, ty: &Ty, depth: usize) -> Option<Vec<Clause>> {
        if depth > DEPTH_LIMIT {
            return None;
        }
        match ty {
            Ty::Empty => Some(Vec::new()),
            Ty::Top => Some(vec![Clause::new()]),
            Ty::Unknown => None,
            Ty::Not(inner) => {
                let clauses = self.dnf(inner, depth + 1)?;
                self.negate_dnf(clauses, depth + 1)
            }
            Ty::Union(items) => {
                let mut clauses = Vec::new();
                for item in items {
                    clauses.extend(self.dnf(item, depth + 1)?);
                    if clauses.len() > CLAUSE_LIMIT {
                        return None;
                    }
                }
                Some(clauses)
            }
            Ty::And(items) => {
                let mut product = vec![Clause::new()];
                for item in items {
                    let next = self.dnf(item, depth + 1)?;
                    product = multiply_clauses(product, next)?;
                }
                Some(product)
            }
            _ => Some(vec![Clause {
                pos: vec![ty.clone()],
                neg: Vec::new(),
            }]),
        }
    }

    fn negate_dnf(&self, clauses: Vec<Clause>, depth: usize) -> Option<Vec<Clause>> {
        if depth > DEPTH_LIMIT {
            return None;
        }
        if clauses.is_empty() {
            return Some(vec![Clause::new()]);
        }
        let mut product = vec![Clause::new()];
        for clause in clauses {
            let mut options = Vec::new();
            for item in &clause.pos {
                options.push(Clause {
                    pos: Vec::new(),
                    neg: vec![item.clone()],
                });
            }
            for item in &clause.neg {
                options.push(Clause {
                    pos: vec![item.clone()],
                    neg: Vec::new(),
                });
            }
            if options.is_empty() {
                return Some(Vec::new());
            }
            product = multiply_clauses(product, options)?;
        }
        Some(product)
    }

    fn satisfiable(&self, clause: &Clause, depth: usize) -> Decision {
        if depth > DEPTH_LIMIT {
            return None;
        }
        let mut positive = Vec::new();
        let mut negative = Vec::new();
        for item in &clause.pos {
            match item {
                Ty::Top => {}
                Ty::Empty => return Some(false),
                Ty::Unknown => return None,
                _ => positive.push(item),
            }
        }
        for item in &clause.neg {
            match item {
                Ty::Empty => {}
                Ty::Top => return Some(false),
                Ty::Unknown => return None,
                _ => negative.push(item),
            }
        }
        if let Some(member) = positive.iter().find_map(|item| match item {
            Ty::Member(member) => Some(member),
            _ => None,
        }) {
            let mut unknown = false;
            for value in member.values.iter() {
                let mut satisfied = true;
                for item in &positive {
                    match self.value_matches(value, item, depth + 1) {
                        Some(true) => {}
                        Some(false) => {
                            satisfied = false;
                            break;
                        }
                        None => {
                            unknown = true;
                            satisfied = false;
                            break;
                        }
                    }
                }
                if !satisfied {
                    continue;
                }
                for item in &negative {
                    match self.value_matches(value, item, depth + 1) {
                        Some(false) => {}
                        Some(true) => {
                            satisfied = false;
                            break;
                        }
                        None => {
                            unknown = true;
                            satisfied = false;
                            break;
                        }
                    }
                }
                if satisfied {
                    return Some(true);
                }
            }
            return (!unknown).then_some(false);
        }

        let mut numbers: Vec<&NumTy> = Vec::new();
        let mut conses: Vec<&ConsTy> = Vec::new();
        let mut arrays: Vec<&ArrayTy> = Vec::new();
        let mut atoms: Vec<&AtomTy> = Vec::new();
        let mut simd_types: Option<SimdTypes> = None;
        for item in &positive {
            match item {
                Ty::Num(num) => numbers.push(num),
                Ty::Cons(cons) => conses.push(cons),
                Ty::Array(array) => arrays.push(array),
                Ty::Atom(atom) => atoms.push(atom),
                Ty::Simd(simd) => {
                    simd_types = Some(simd_types.map_or(*simd, |types| types.intersection(*simd)));
                }
                _ => return None,
            }
        }

        if let Some(mut simd_types) = simd_types {
            if !numbers.is_empty()
                || !conses.is_empty()
                || !arrays.is_empty()
                || atoms
                    .iter()
                    .any(|atom| atom.name.as_deref() != Some("ATOM"))
            {
                return Some(false);
            }
            for item in &negative {
                match item {
                    Ty::Simd(excluded) => simd_types = simd_types.difference(*excluded),
                    Ty::Atom(atom) if atom.name.as_deref() == Some("ATOM") => return Some(false),
                    // EQL compares SIMD values by identity. A finite member list
                    // cannot exclude a whole shape of independently allocated objects.
                    Ty::Atom(_) | Ty::Num(_) | Ty::Cons(_) | Ty::Array(_) | Ty::Member(_) => {}
                    _ => return None,
                }
            }
            return Some(!simd_types.is_empty());
        }

        if !numbers.is_empty() {
            if !conses.is_empty() || !arrays.is_empty() {
                return Some(false);
            }
            if atoms
                .iter()
                .any(|atom| atom.name.as_deref() != Some("ATOM"))
            {
                return Some(false);
            }
            let mut entries = numbers[0].cells.clone();
            for number in &numbers[1..] {
                let mut next = Vec::new();
                for (cell, range) in entries {
                    if let Some((_, other)) = number.cells.iter().find(|(kind, _)| *kind == cell) {
                        let range = range.intersect(other);
                        if range.nonempty(cell) {
                            next.push((cell, range));
                        }
                    }
                }
                entries = next;
                if entries.is_empty() {
                    return Some(false);
                }
            }
            for item in &negative {
                match item {
                    Ty::Num(number) => {
                        let mut next = Vec::new();
                        for (cell, range) in entries {
                            if let Some((_, other)) =
                                number.cells.iter().find(|(kind, _)| *kind == cell)
                            {
                                for piece in range.subtract(other) {
                                    if piece.nonempty(cell) {
                                        next.push((cell, piece));
                                    }
                                }
                            } else {
                                next.push((cell, range));
                            }
                        }
                        entries = next;
                    }
                    Ty::Member(member) => {
                        let zero = BigRational::from_integer(BigInt::from(0));
                        let mut points = Vec::new();
                        let mut zero_signs: Vec<(Cell, bool)> = Vec::new();
                        for value in member.values.iter() {
                            let Some(Number::Real(real)) = self.runtime.heap.number(value.get())
                            else {
                                continue;
                            };
                            let point_value = real.as_rational()?;
                            let cell = match &real {
                                Real::Integer(_) => Cell::Int,
                                Real::Ratio(_) => Cell::Ratio,
                                Real::Single(_) => Cell::Single,
                                Real::Double(_) => Cell::Double,
                            };
                            if matches!(cell, Cell::Single | Cell::Double) && point_value == zero {
                                let negative = match &real {
                                    Real::Single(value) => value.is_sign_negative(),
                                    Real::Double(value) => value.is_sign_negative(),
                                    _ => false,
                                };
                                zero_signs.push((cell, negative));
                            } else {
                                points.push((cell, point_value));
                            }
                        }
                        for (cell, point_value) in points {
                            let point = Range {
                                low: Some(Limit {
                                    value: point_value.clone(),
                                    inclusive: true,
                                }),
                                high: Some(Limit {
                                    value: point_value,
                                    inclusive: true,
                                }),
                            };
                            let mut next = Vec::new();
                            for (kind, range) in entries {
                                if kind == cell {
                                    for piece in range.subtract(&point) {
                                        if piece.nonempty(cell) {
                                            next.push((kind, piece));
                                        }
                                    }
                                } else {
                                    next.push((kind, range));
                                }
                            }
                            entries = next;
                        }
                        for cell in [Cell::Single, Cell::Double] {
                            let signs: Vec<bool> = zero_signs
                                .iter()
                                .filter(|(kind, _)| *kind == cell)
                                .map(|(_, negative)| *negative)
                                .collect();
                            if signs.is_empty() {
                                continue;
                            }
                            let both = signs.contains(&true) && signs.contains(&false);
                            let point = Range {
                                low: Some(Limit {
                                    value: zero.clone(),
                                    inclusive: true,
                                }),
                                high: Some(Limit {
                                    value: zero.clone(),
                                    inclusive: true,
                                }),
                            };
                            let mut next = Vec::new();
                            for (kind, range) in entries {
                                if kind == cell {
                                    let mut pieces = Vec::new();
                                    for piece in range.subtract(&point) {
                                        if piece.nonempty(cell) {
                                            pieces.push(piece);
                                        }
                                    }
                                    if pieces.is_empty() && !both && zero_point(&range) {
                                        pieces.push(range);
                                    }
                                    next.extend(pieces.into_iter().map(|piece| (kind, piece)));
                                } else {
                                    next.push((kind, range));
                                }
                            }
                            entries = next;
                        }
                    }
                    Ty::Atom(atom) => {
                        if atom.name.as_deref() == Some("ATOM") {
                            return Some(false);
                        }
                    }
                    Ty::Cons(_) | Ty::Array(_) | Ty::Simd(_) => {}
                    _ => return None,
                }
            }
            return Some(entries.iter().any(|(cell, range)| range.nonempty(*cell)));
        }

        if !conses.is_empty() {
            if !arrays.is_empty() {
                return Some(false);
            }
            if !atoms.is_empty() {
                return Some(false);
            }
            let mut cars = Vec::new();
            let mut cdrs = Vec::new();
            for cons in &conses {
                cars.push((*cons.car).clone());
                cdrs.push((*cons.cdr).clone());
            }
            let mut excluded = Vec::new();
            let mut has_cons_member = false;
            for item in &negative {
                match item {
                    Ty::Cons(target) => excluded.push(target),
                    Ty::Atom(atom) => {
                        if matches!(atom.name.as_deref(), Some("LIST") | Some("SEQUENCE")) {
                            return Some(false);
                        }
                    }
                    Ty::Member(member) => {
                        if member
                            .values
                            .iter()
                            .any(|value| self.runtime.heap.cons_fields(value.get()).is_some())
                        {
                            has_cons_member = true;
                        }
                    }
                    Ty::Num(_) | Ty::Array(_) | Ty::Simd(_) => {}
                    _ => return None,
                }
            }
            let car_type = Ty::And(cars);
            let cdr_type = Ty::And(cdrs);
            for target in &excluded {
                if self.subset(&car_type, &target.car, depth + 1) == Some(true)
                    && self.subset(&cdr_type, &target.cdr, depth + 1) == Some(true)
                {
                    return Some(false);
                }
            }
            let car = self.nonempty(&car_type, depth + 1);
            let cdr = self.nonempty(&cdr_type, depth + 1);
            return match (car, cdr) {
                (Some(false), _) | (_, Some(false)) => Some(false),
                (Some(true), Some(true)) => {
                    if excluded.len() <= 1 && !has_cons_member {
                        Some(true)
                    } else {
                        None
                    }
                }
                _ => None,
            };
        }

        if !arrays.is_empty() {
            if atoms
                .iter()
                .any(|atom| !matches!(atom.name.as_deref(), Some("ATOM")))
            {
                return Some(false);
            }
            let mut combined = arrays[0].clone();
            for array in &arrays[1..] {
                let Some(intersection) = self.array_intersection(&combined, array) else {
                    return Some(false);
                };
                combined = intersection;
            }
            for item in &negative {
                match item {
                    Ty::Array(target) => {
                        if self.array_subset(&combined, target) == Some(true) {
                            return Some(false);
                        }
                    }
                    Ty::Atom(atom) => {
                        if atom.name.as_deref() == Some("SEQUENCE")
                            && array_rank_len(&combined) == Some(1)
                        {
                            return Some(false);
                        }
                    }
                    Ty::Num(_) | Ty::Cons(_) | Ty::Simd(_) | Ty::Member(_) => {}
                    _ => return None,
                }
            }
            return Some(true);
        }

        if !atoms.is_empty() {
            let mut unknown = false;
            for (index, atom) in atoms.iter().enumerate() {
                for other in &atoms[index + 1..] {
                    match self.atom_disjoint(atom, other) {
                        Some(true) => return Some(false),
                        Some(false) => {}
                        None => unknown = true,
                    }
                }
            }
            for item in &negative {
                match item {
                    Ty::Atom(target) => {
                        if target.name.as_deref() != Some("ATOM") {
                            for atom in &atoms {
                                if self.atom_subset(atom, target) == Some(true) {
                                    return Some(false);
                                }
                            }
                        }
                    }
                    Ty::Member(_) | Ty::Num(_) | Ty::Cons(_) | Ty::Array(_) | Ty::Simd(_) => {}
                    _ => return None,
                }
            }
            return (!unknown).then_some(true);
        }

        Some(true)
    }

    fn num_subset(&self, source: &NumTy, target: &NumTy, depth: usize) -> Decision {
        for (cell, range) in &source.cells {
            let Some((_, target)) = target.cells.iter().find(|(kind, _)| kind == cell) else {
                return Some(false);
            };
            if !target.contains(range) {
                return Some(false);
            }
        }
        let complex_part = |spec| {
            let (operator, arguments) = self.runtime.heap.cons_fields(spec)?;
            if specifier_name(self.runtime, operator).as_deref() != Some("COMPLEX") {
                return None;
            }
            let (part, Word::NIL) = self.runtime.heap.cons_fields(arguments)? else {
                return None;
            };
            (!wildcard(self.runtime, part)).then_some(part)
        };
        // SUBTYPEP gives explicitly specialized complex types their own rule:
        // equal upgraded part types or a subtype relation on the expressed types.
        if source.cells.len() != target.cells.len()
            && let Some(source_part) = complex_part(source.spec.get())
            && let Some(target_part) = complex_part(target.spec.get())
        {
            let target_part = self.runtime.root_slot(target_part);
            let source = self.parse(source_part, depth + 1);
            let target = self.parse(target_part.get(), depth + 1);
            return self.subset(&source, &target, depth + 1);
        }
        Some(true)
    }

    fn num_disjoint(&self, left: &NumTy, right: &NumTy) -> Decision {
        for (cell, range) in &left.cells {
            if let Some((_, other)) = right.cells.iter().find(|(kind, _)| kind == cell)
                && range.intersect(other).nonempty(*cell)
            {
                return Some(false);
            }
        }
        Some(true)
    }

    fn num_subset_member(&self, source: &NumTy, target: &MemberTy) -> Decision {
        for (cell, range) in &source.cells {
            match cell {
                Cell::Int => {
                    let (Some(low), Some(high)) = (&range.low, &range.high) else {
                        return Some(false);
                    };
                    let low = low.value.to_integer();
                    let high = high.value.to_integer();
                    let count = &high - &low + BigInt::from(1);
                    if count > BigInt::from(4096) {
                        return None;
                    }
                    let mut value = low;
                    while value <= high {
                        let present = target.values.iter().any(|candidate| {
                            self.runtime.heap.integer(candidate.get()).as_ref() == Some(&value)
                        });
                        if !present {
                            return Some(false);
                        }
                        value += BigInt::from(1);
                    }
                }
                _ => {
                    if range.nonempty(*cell) {
                        return None;
                    }
                }
            }
        }
        Some(true)
    }

    fn member_subset(&self, source: &MemberTy, target: &MemberTy) -> Decision {
        Some(
            source
                .values
                .iter()
                .all(|value| target.contains(&self.runtime.heap, value.get())),
        )
    }

    fn member_targets(&self, source: &MemberTy, target: &Ty, depth: usize) -> Decision {
        combine_decisions(
            source
                .values
                .iter()
                .map(|value| self.value_matches(value, target, depth + 1)),
            true,
        )
    }

    fn cons_subset(&self, source: &ConsTy, target: &ConsTy, depth: usize) -> Decision {
        match (
            self.subset(&source.car, &target.car, depth + 1),
            self.subset(&source.cdr, &target.cdr, depth + 1),
        ) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        }
    }

    fn element_subset(&self, source: Elem, target: Elem) -> bool {
        use Elem::*;
        match (source, target) {
            (_, Wildcard) => true,
            (Wildcard, _) => false,
            (left, right) if left == right => true,
            (Bit, _) | (_, Bit) => false,
            (BaseChar | NilElement, String) => true,
            _ => false,
        }
    }

    fn element_intersection(&self, left: Elem, right: Elem) -> Option<Elem> {
        use Elem::*;
        match (left, right) {
            (Wildcard, element) | (element, Wildcard) => Some(element),
            (left, right) if left == right => Some(left),
            (BaseChar, String) | (String, BaseChar) => Some(BaseChar),
            (NilElement, String) | (String, NilElement) => Some(NilElement),
            _ => None,
        }
    }

    fn array_subset(&self, source: &ArrayTy, target: &ArrayTy) -> Decision {
        if target.simple && !source.simple {
            return Some(false);
        }
        if !self.element_subset(source.element, target.element) {
            return Some(false);
        }
        match (&source.rank, &target.rank) {
            (_, Rank::Any) => {}
            (Rank::Any, _) => return Some(false),
            (Rank::Dims(source), Rank::Dims(target)) => {
                if source.len() != target.len() {
                    return Some(false);
                }
                for (source, target) in source.iter().zip(target) {
                    match (source, target) {
                        (_, None) => {}
                        (Some(left), Some(right)) if left == right => {}
                        _ => return Some(false),
                    }
                }
            }
        }
        Some(true)
    }

    fn array_intersection(&self, left: &ArrayTy, right: &ArrayTy) -> Option<ArrayTy> {
        let element = self.element_intersection(left.element, right.element)?;
        let rank = match (&left.rank, &right.rank) {
            (Rank::Any, rank) | (rank, Rank::Any) => rank.clone(),
            (Rank::Dims(left), Rank::Dims(right)) => {
                if left.len() != right.len() {
                    return None;
                }
                let mut dims = Vec::with_capacity(left.len());
                for (left, right) in left.iter().zip(right) {
                    match (left, right) {
                        (Some(left), Some(right)) if left != right => return None,
                        (Some(dimension), _) | (_, Some(dimension)) => dims.push(Some(*dimension)),
                        (None, None) => dims.push(None),
                    }
                }
                Rank::Dims(dims)
            }
        };
        Some(ArrayTy {
            spec: Rc::clone(&left.spec),
            simple: left.simple || right.simple,
            element,
            rank,
        })
    }

    fn atom_subset(&self, source: &AtomTy, target: &AtomTy) -> Decision {
        let source_name = source.name.as_deref();
        let target_name = target.name.as_deref();
        if source.word.get() == target.word.get() {
            return Some(true);
        }
        if target_name == Some("ATOM") {
            return Some(true);
        }
        if let (Some(source_name), Some(target_name)) = (source_name, target_name) {
            if let (Some(source_level), Some(target_level)) =
                (character_level(source_name), character_level(target_name))
            {
                return Some(source_level <= target_level);
            }
            if atom_supertype_path(source_name, target_name) {
                return Some(true);
            }
        }
        if let Some(result) =
            classes::class_subtypep(self.runtime, source.word.get(), target.word.get())
        {
            return Some(result);
        }
        Some(false)
    }

    fn atom_disjoint(&self, left: &AtomTy, right: &AtomTy) -> Decision {
        let left_name = left.name.as_deref();
        let right_name = right.name.as_deref();
        if left.word.get() == right.word.get() {
            return Some(false);
        }
        if left_name == Some("ATOM") || right_name == Some("ATOM") {
            return Some(false);
        }
        if let (Some(left_name), Some(right_name)) = (left_name, right_name) {
            if character_level(left_name).is_some() && character_level(right_name).is_some() {
                return Some(false);
            }
            if atom_supertype_path(left_name, right_name)
                || atom_supertype_path(right_name, left_name)
            {
                return Some(false);
            }
        }
        if classes::class_subtypep(self.runtime, left.word.get(), right.word.get()) == Some(true)
            || classes::class_subtypep(self.runtime, right.word.get(), left.word.get())
                == Some(true)
        {
            return Some(false);
        }
        match (left.group, right.group) {
            (left, right) if left != right => Some(true),
            _ => None,
        }
    }
}

fn multiply_clauses(left: Vec<Clause>, right: Vec<Clause>) -> Option<Vec<Clause>> {
    if left.is_empty() || right.is_empty() {
        return Some(Vec::new());
    }
    if left.len().saturating_mul(right.len()) > CLAUSE_LIMIT {
        return None;
    }
    let mut product = Vec::with_capacity(left.len() * right.len());
    for one in &left {
        for two in &right {
            let mut clause = Clause {
                pos: one.pos.clone(),
                neg: one.neg.clone(),
            };
            clause.pos.extend(two.pos.iter().cloned());
            clause.neg.extend(two.neg.iter().cloned());
            product.push(clause);
        }
    }
    Some(product)
}

fn zero_point(range: &Range) -> bool {
    let zero = BigRational::from_integer(BigInt::from(0));
    matches!(
        (&range.low, &range.high),
        (Some(low), Some(high)) if low.inclusive && high.inclusive && low.value == zero && high.value == zero
    )
}

fn array_rank_len(array: &ArrayTy) -> Option<usize> {
    match &array.rank {
        Rank::Any => None,
        Rank::Dims(dims) => Some(dims.len()),
    }
}

fn character_level(name: &str) -> Option<u8> {
    match name {
        "CHARACTER" | "BASE-CHAR" => Some(2),
        "STANDARD-CHAR" => Some(1),
        _ => None,
    }
}

fn atom_supertype_path(source: &str, target: &str) -> bool {
    let supers = |name: &str| -> &'static [&'static str] {
        match name {
            "KEYWORD" => &["SYMBOL"],
            "FUNCTION" => &["COMPILED-FUNCTION"],
            "COMPILED-FUNCTION" => &["FUNCTION"],
            "INPUT-STREAM" | "OUTPUT-STREAM" => &["STREAM"],
            "GENERIC-FUNCTION" => &["FUNCTION"],
            "STANDARD-GENERIC-FUNCTION" => &["GENERIC-FUNCTION"],
            "METHOD" => &["STANDARD-OBJECT"],
            "STANDARD-METHOD" => &["METHOD"],
            _ => &[],
        }
    };
    let mut pending = vec![source];
    let mut seen = Vec::new();
    while let Some(name) = pending.pop() {
        if name == target {
            return true;
        }
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        pending.extend(supers(name).iter().copied());
    }
    false
}

pub(super) fn resolve_complex_part(
    runtime: &Runtime,
    part: Word,
    depth: usize,
) -> ComplexPartResolution {
    Subtype { runtime }.resolve_complex_part(part, depth, true)
}

pub(in crate::vm) fn upgraded_numeric_array_type(
    runtime: &Runtime,
    spec: Word,
    depth: usize,
) -> Option<ArrayElement> {
    let engine = Subtype { runtime };
    let depth = depth + 1;
    let source = engine.parse(spec, depth);
    if engine.nonempty(&source, depth) == Some(false) {
        return None;
    }
    // Equivalent numeric type specifiers must select the same storage. Use
    // the subtype relation for bounds, aliases, and compound element types.
    for (element, cell, range) in [
        (
            ArrayElement::Bit,
            Cell::Int,
            Range::closed(0.into(), 1.into()),
        ),
        (
            ArrayElement::Octet,
            Cell::Int,
            Range::closed(0.into(), 255.into()),
        ),
        (ArrayElement::SingleFloat, Cell::Single, Range::unbounded()),
        (ArrayElement::DoubleFloat, Cell::Double, Range::unbounded()),
    ] {
        let target = Ty::Num(NumTy {
            spec: Rc::new(runtime.root_slot(Word::NIL)),
            cells: vec![(cell, range)],
        });
        if engine.subset(&source, &target, depth) == Some(true) {
            return Some(element);
        }
    }
    Some(ArrayElement::General)
}

pub(crate) fn upgraded_complex_part_type(
    runtime: &Runtime,
    spec: Word,
    at: usize,
) -> Result<Word, Fault> {
    match resolve_complex_part(runtime, spec, 0) {
        ComplexPartResolution::Upgraded(kind) => match kind {
            ComplexPartKind::Empty => Ok(Word::NIL),
            _ => Ok(runtime.intern(&Name::common_lisp(kind.type_name()), at)?),
        },
        ComplexPartResolution::NotReal | ComplexPartResolution::Unknown => Err(Fault::plain(
            Class::Error,
            EvalError::new(at, "UPGRADED-COMPLEX-PART-TYPE requires a subtype of REAL"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtypep_resumes_after_collection_inside_a_type_expander() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        runtime
            .register_function("collect-during-type-expansion", |runtime, args| {
                let before = args[0].slot.get();
                runtime.collect();
                assert_ne!(args[0].slot.get(), before);
                Ok(Vec::new())
            })
            .unwrap();
        runtime
            .load_str(
                "(deftype collected-type (type)
                   (collect-during-type-expansion (cons nil nil))
                   type)",
            )
            .unwrap();

        for source in [
            "(subtypep '(collected-type integer) '(integer * *))",
            "(subtypep '(or (collected-type bit) (integer 2 10)) 'integer)",
            "(subtypep '(cons (collected-type integer) (integer 0 10)) '(cons rational integer))",
            "(subtypep '(complex (collected-type single-float))
                        '(complex (or single-float double-float)))",
            "(let ((object (cons 3 4)))
               (subtypep (list 'eql object) (list 'collected-type (list 'eql object))))",
            "(let ((object (cons 3 4)))
               (subtypep (list 'eql object) '(cons (collected-type integer) integer)))",
            "(let ((object (make-array 2 :element-type 'double-float
                                        :initial-contents '(1d0 -0d0))))
               (subtypep (list 'eql object) '(vector (collected-type long-float) 2)))",
        ] {
            let values = runtime.eval(source).unwrap();
            assert!(values[0].is_t() && values[1].is_t(), "{source}");
        }
    }
}
