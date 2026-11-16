//! Core type lattice and the part of Lisp type syntax used by the compiler.

use crate::heap::Word;
use crate::read::{Form, FormKind};
use crate::symbol::Name;

/// Static value-type lattice for Core IR type propagation and safe folding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreType {
    Top,
    Empty,
    Null,
    True,
    Boolean,
    NonNull,
    Fixnum,
    Bignum,
    Integer,
    Ratio,
    Rational,
    Float,
    Real,
    Complex,
    Number,
    Character,
    Symbol,
    NonNilSymbol,
    Keyword,
    Cons,
    List,
    String,
    Vector,
    Array,
    Function,
    HashTable,
    Package,
    Pathname,
    Readtable,
    RandomState,
}

impl CoreType {
    pub const fn is_definitely_truthy(self) -> bool {
        !matches!(
            self,
            Self::Top | Self::Empty | Self::Null | Self::Boolean | Self::Symbol | Self::List
        )
    }

    pub const fn is_definitely_nil(self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn is_subtype(self, other: Self) -> bool {
        if self == other || other == Self::Top || self == Self::Empty {
            return true;
        }
        if other == Self::NonNull {
            return self.is_definitely_truthy();
        }
        matches!(
            (self, other),
            (
                Self::Fixnum | Self::Bignum,
                Self::Integer | Self::Rational | Self::Real | Self::Number,
            ) | (
                Self::Integer | Self::Ratio,
                Self::Rational | Self::Real | Self::Number,
            ) | (Self::Rational | Self::Float, Self::Real | Self::Number)
                | (Self::Real | Self::Complex, Self::Number)
                | (Self::Null, Self::Boolean | Self::Symbol | Self::List)
                | (
                    Self::True,
                    Self::Boolean | Self::NonNilSymbol | Self::Symbol
                )
                | (Self::Boolean, Self::Symbol)
                | (Self::Keyword, Self::NonNilSymbol | Self::Symbol)
                | (Self::NonNilSymbol, Self::Symbol)
                | (Self::Cons, Self::List)
                | (Self::String, Self::Vector | Self::Array)
                | (Self::Vector, Self::Array)
        )
    }

    pub fn join(self, other: Self) -> Self {
        if self == Self::Empty {
            return other;
        }
        if other == Self::Empty {
            return self;
        }
        if self == other {
            return self;
        }
        if self.is_subtype(other) {
            return other;
        }
        if other.is_subtype(self) {
            return self;
        }
        match (self, other) {
            (Self::Fixnum, Self::Bignum) | (Self::Bignum, Self::Fixnum) => Self::Integer,
            (Self::Fixnum | Self::Bignum | Self::Integer, Self::Ratio)
            | (Self::Ratio, Self::Fixnum | Self::Bignum | Self::Integer) => Self::Rational,
            (
                Self::Fixnum | Self::Bignum | Self::Integer | Self::Ratio | Self::Rational,
                Self::Float,
            )
            | (
                Self::Float,
                Self::Fixnum | Self::Bignum | Self::Integer | Self::Ratio | Self::Rational,
            ) => Self::Real,
            (a, b) if a.is_subtype(Self::Number) && b.is_subtype(Self::Number) => Self::Number,
            (Self::Null, Self::True) | (Self::True, Self::Null) => Self::Boolean,
            (Self::Null, Self::Cons) | (Self::Cons, Self::Null) => Self::List,
            (Self::Null | Self::Boolean, Self::NonNilSymbol | Self::Keyword | Self::Symbol)
            | (Self::NonNilSymbol | Self::Keyword | Self::Symbol, Self::Null | Self::Boolean) => {
                Self::Symbol
            }
            (
                Self::True | Self::Keyword | Self::NonNilSymbol,
                Self::True | Self::Keyword | Self::NonNilSymbol,
            ) => Self::NonNilSymbol,
            (a, b) if a.is_definitely_truthy() && b.is_definitely_truthy() => Self::NonNull,
            _ => Self::Top,
        }
    }

    pub fn meet(self, other: Self) -> Self {
        if self == other {
            return self;
        }
        if self == Self::Empty || other == Self::Empty {
            return Self::Empty;
        }
        if self.is_subtype(other) {
            return self;
        }
        if other.is_subtype(self) {
            return other;
        }
        match (self, other) {
            (Self::NonNull, x) | (x, Self::NonNull) => match x {
                Self::Null => Self::Empty,
                Self::Boolean => Self::True,
                Self::Symbol => Self::NonNilSymbol,
                Self::List => Self::Cons,
                _ => x,
            },
            (Self::Boolean, Self::List) | (Self::List, Self::Boolean) => Self::Null,
            (Self::Symbol, Self::List) | (Self::List, Self::Symbol) => Self::Null,
            (Self::Boolean, Self::NonNilSymbol) | (Self::NonNilSymbol, Self::Boolean) => Self::True,
            _ => Self::Empty,
        }
    }

    /// Return an upper bound for the difference of the represented value sets.
    pub fn excluding(self, excluded: Self) -> Self {
        let overlap = self.meet(excluded);
        if overlap == self {
            return Self::Empty;
        }
        match (self, overlap) {
            (Self::Top, Self::NonNull)
            | (Self::Boolean, Self::True)
            | (Self::Symbol, Self::NonNilSymbol)
            | (Self::List, Self::Cons) => Self::Null,
            (Self::Integer, Self::Fixnum) => Self::Bignum,
            (Self::Integer, Self::Bignum) => Self::Fixnum,
            (Self::Rational, Self::Integer) => Self::Ratio,
            (Self::Rational, Self::Ratio) => Self::Integer,
            (Self::Real, Self::Rational) => Self::Float,
            (Self::Real, Self::Float) => Self::Rational,
            (Self::Number, Self::Real) => Self::Complex,
            (Self::Number, Self::Complex) => Self::Real,
            (_, overlap) if Self::Null.is_subtype(overlap) => self.meet(Self::NonNull),
            _ => self,
        }
    }

    pub fn is_disjoint(self, other: Self) -> bool {
        self.meet(other) == Self::Empty
    }

    /// Map a canonical `COMMON-LISP` atomic type name to `(CoreType, exact)`.
    pub fn from_atomic_name(op: &str) -> Option<(Self, bool)> {
        Some(match op {
            "T" => (Self::Top, true),
            "NIL" => (Self::Empty, true),
            "NULL" => (Self::Null, true),
            "BOOLEAN" => (Self::Boolean, true),
            "FIXNUM" => (Self::Fixnum, true),
            "BIGNUM" => (Self::Bignum, true),
            "INTEGER" => (Self::Integer, true),
            "SIGNED-BYTE" => (Self::Integer, true),
            "UNSIGNED-BYTE" | "BIT" => (Self::Integer, false),
            "RATIO" => (Self::Ratio, true),
            "RATIONAL" => (Self::Rational, true),
            "FLOAT" => (Self::Float, true),
            "SHORT-FLOAT" | "SINGLE-FLOAT" | "DOUBLE-FLOAT" | "LONG-FLOAT" => (Self::Float, false),
            "REAL" => (Self::Real, true),
            "COMPLEX" => (Self::Complex, true),
            "NUMBER" => (Self::Number, true),
            "CHARACTER" | "BASE-CHAR" => (Self::Character, true),
            "STANDARD-CHAR" => (Self::Character, false),
            "EXTENDED-CHAR" => (Self::Empty, false),
            "SYMBOL" => (Self::Symbol, true),
            "KEYWORD" => (Self::Keyword, true),
            "CONS" => (Self::Cons, true),
            "LIST" => (Self::List, true),
            "STRING" => (Self::String, true),
            "BASE-STRING" | "SIMPLE-STRING" | "SIMPLE-BASE-STRING" => (Self::String, false),
            "VECTOR" => (Self::Vector, true),
            "SIMPLE-VECTOR" | "BIT-VECTOR" | "SIMPLE-BIT-VECTOR" => (Self::Vector, false),
            "ARRAY" => (Self::Array, true),
            "SIMPLE-ARRAY" => (Self::Array, false),
            "FUNCTION" | "COMPILED-FUNCTION" => (Self::Function, true),
            // Gray streams and funcallable standard instances can share a class.
            "STREAM" | "STRING-STREAM" => (Self::NonNull, false),
            "HASH-TABLE" => (Self::HashTable, true),
            "PACKAGE" => (Self::Package, true),
            "PATHNAME" => (Self::Pathname, true),
            "LOGICAL-PATHNAME" => (Self::Pathname, false),
            "READTABLE" => (Self::Readtable, true),
            "RANDOM-STATE" => (Self::RandomState, true),
            "RESTART" | "CONDITION" | "CLASS" | "METHOD" | "STANDARD-OBJECT"
            | "STRUCTURE-OBJECT" => (Self::NonNull, false),
            "ATOM" | "SEQUENCE" => (Self::Top, false),
            _ => return None,
        })
    }
}

/// Parsed `(values ...)` type specifier for `THE` and `FTYPE`.
#[derive(Clone)]
pub struct CoreValuesSpec {
    pub required: Vec<(Form, CoreType, bool)>,
    pub optional: Vec<(Form, CoreType, bool)>,
    pub rest: Option<(Form, CoreType, bool)>,
}

impl CoreValuesSpec {
    pub fn primary_type(&self) -> CoreType {
        if let Some((_, ty, _)) = self.required.first() {
            *ty
        } else if let Some((_, ty, _)) = self.optional.first() {
            ty.join(CoreType::Null)
        } else if let Some((_, ty, _)) = &self.rest {
            ty.join(CoreType::Null)
        } else {
            CoreType::Top
        }
    }
}

#[derive(Clone)]
pub enum ParsedTheType {
    Single { ty: CoreType, exact: bool },
    Values(CoreValuesSpec),
}

impl ParsedTheType {
    pub fn primary_type(&self) -> CoreType {
        match self {
            Self::Single { ty, .. } => *ty,
            Self::Values(v) => v.primary_type(),
        }
    }
}

fn is_wildcard_star(form: &Form) -> bool {
    form.name().is_some_and(|n| n.operator() == "*")
}

/// Parse a Common Lisp type specifier into an upper bound and an equivalence flag.
/// A type check can use the bound as its target only when the flag is true.
pub fn parse_type_spec(form: &Form) -> (CoreType, bool) {
    if let Some(name) = form.name() {
        return CoreType::from_atomic_name(name.operator()).unwrap_or((CoreType::Top, false));
    }
    let Some(items) = form.list() else {
        return (CoreType::Top, false);
    };
    let Some((head, args)) = items.split_first() else {
        return (CoreType::Empty, true);
    };
    let Some(op) = head.name().map(Name::operator) else {
        return (CoreType::Top, false);
    };
    match op {
        "INTEGER" | "RATIONAL" | "REAL" | "FLOAT" | "SHORT-FLOAT" | "SINGLE-FLOAT"
        | "DOUBLE-FLOAT" | "LONG-FLOAT"
            if args.len() <= 2 =>
        {
            let (base, base_exact) = CoreType::from_atomic_name(op).unwrap();
            let unbounded = args.iter().all(is_wildcard_star);
            (base, base_exact && unbounded)
        }
        "SIGNED-BYTE" if args.len() <= 1 => (CoreType::Integer, args.iter().all(is_wildcard_star)),
        "UNSIGNED-BYTE" if args.len() <= 1 => (CoreType::Integer, false),
        "MOD" if args.len() == 1 => (CoreType::Integer, false),
        "COMPLEX" if args.len() <= 1 => (CoreType::Complex, args.iter().all(is_wildcard_star)),
        "CONS" if args.len() <= 2 => (CoreType::Cons, args.iter().all(is_wildcard_star)),
        "STRING" if args.len() <= 1 => (CoreType::String, args.iter().all(is_wildcard_star)),
        "BASE-STRING" | "SIMPLE-STRING" | "SIMPLE-BASE-STRING" if args.len() <= 1 => {
            (CoreType::String, false)
        }
        "VECTOR" if args.len() <= 2 => (CoreType::Vector, args.iter().all(is_wildcard_star)),
        "SIMPLE-VECTOR" | "BIT-VECTOR" | "SIMPLE-BIT-VECTOR" if args.len() <= 1 => {
            (CoreType::Vector, false)
        }
        "ARRAY" if args.len() <= 2 => (CoreType::Array, args.iter().all(is_wildcard_star)),
        "SIMPLE-ARRAY" if args.len() <= 2 => (CoreType::Array, false),
        "OR" => combine_type_union(args.iter().map(parse_type_spec)),
        "AND" => args.iter().map(parse_type_spec).fold(
            (CoreType::Top, true),
            |(bound, exact), (arg_bound, arg_exact)| (bound.meet(arg_bound), exact && arg_exact),
        ),
        "NOT" => {
            if let [inner] = args {
                let (ty, exact) = parse_type_spec(inner);
                if exact {
                    let complement = CoreType::Top.excluding(ty);
                    return (complement, complement.is_disjoint(ty));
                }
            }
            (CoreType::Top, false)
        }
        "EQL" => {
            if let [arg] = args {
                return singleton_type(arg);
            }
            (CoreType::Top, false)
        }
        "MEMBER" => combine_type_union(args.iter().map(singleton_type)),
        "FUNCTION" if args.len() <= 2 => (CoreType::Function, args.is_empty()),
        _ => (CoreType::Top, false),
    }
}

fn combine_type_union(types: impl Iterator<Item = (CoreType, bool)>) -> (CoreType, bool) {
    types.fold(
        (CoreType::Empty, true),
        |(bound, exact), (arg_bound, arg_exact)| {
            let joined = bound.join(arg_bound);
            // Every value added by the widening must belong to the next arm.
            let covered = exact && arg_exact && joined.excluding(bound).is_subtype(arg_bound);
            (joined, covered)
        },
    )
}

fn singleton_type(form: &Form) -> (CoreType, bool) {
    let ty = type_of_literal_form(form);
    (ty, matches!(ty, CoreType::Null | CoreType::True))
}

// Literal type specifiers may be examined without an active compiler target.
// Only refine to FIXNUM when the integer has that representation on every target.
fn portable_integer_type(n: isize) -> CoreType {
    if zkcl_bytecode::Immediate::Fixnum(n as i64)
        .encode_word(32)
        .is_some()
    {
        CoreType::Fixnum
    } else {
        CoreType::Integer
    }
}

pub(super) fn type_of_literal_form(form: &Form) -> CoreType {
    if let Some(root) = form.original_object() {
        return type_of_word(root.get());
    }
    match &form.kind {
        FormKind::Integer(n) => portable_integer_type(*n),
        FormKind::Character(_) => CoreType::Character,
        FormKind::String(_) => CoreType::String,
        FormKind::Name(n) => type_of_symbol_name(n),
        FormKind::List(items, tail) => {
            if items.is_empty() && tail.is_none() {
                CoreType::Null
            } else if !items.is_empty() {
                CoreType::Cons
            } else {
                CoreType::Top
            }
        }
        FormKind::Literal(root) => type_of_word(root.get()),
    }
}

pub(super) fn type_of_symbol_name(n: &Name) -> CoreType {
    if n.operator() == "NIL" {
        CoreType::Null
    } else if n.operator() == "T" {
        CoreType::True
    } else {
        // A symbol's home package can change after the literal is compiled.
        CoreType::NonNilSymbol
    }
}

fn type_of_word(word: Word) -> CoreType {
    if word == Word::NIL {
        CoreType::Null
    } else if word == Word::T {
        CoreType::True
    } else if let Some(n) = word.as_fixnum() {
        portable_integer_type(n)
    } else if word.as_character().is_some() {
        CoreType::Character
    } else {
        CoreType::NonNull
    }
}

/// Parse a `THE` type specifier, supporting both `(values ...)` and ordinary types.
pub fn parse_the_type(form: &Form) -> ParsedTheType {
    if let Some([head, specs @ ..]) = form.list()
        && head.name().is_some_and(|n| n.operator() == "VALUES")
    {
        let mut required = Vec::new();
        let mut optional = Vec::new();
        let mut rest = None;
        enum ValuesTypeSection {
            Required,
            Optional,
            Rest,
        }
        let mut mode = ValuesTypeSection::Required;
        for item in specs {
            match item.name().map(Name::operator) {
                Some("&OPTIONAL") => {
                    mode = ValuesTypeSection::Optional;
                    continue;
                }
                Some("&REST") => {
                    mode = ValuesTypeSection::Rest;
                    continue;
                }
                Some("&ALLOW-OTHER-KEYS") => continue,
                _ => {}
            }
            let (ty, exact) = parse_type_spec(item);
            match mode {
                ValuesTypeSection::Required => required.push((item.clone(), ty, exact)),
                ValuesTypeSection::Optional => optional.push((item.clone(), ty, exact)),
                ValuesTypeSection::Rest => {
                    rest = Some((item.clone(), ty, exact));
                    break;
                }
            }
        }
        return ParsedTheType::Values(CoreValuesSpec {
            required,
            optional,
            rest,
        });
    }
    let (ty, exact) = parse_type_spec(form);
    ParsedTheType::Single { ty, exact }
}

/// Extract the parameter and result specs from a compound function type.
pub(crate) fn ftype_signature(form: &Form) -> Option<(&Form, Option<&Form>)> {
    let [head, arguments, result @ ..] = form.list()? else {
        return None;
    };
    (head.name()?.operator() == "FUNCTION" && result.len() <= 1)
        .then_some((arguments, result.first()))
}

#[cfg(test)]
mod tests {
    use super::super::CoreExpr;
    use super::super::simplify::builtin_return_type;
    use super::{CoreType, parse_type_spec};
    use crate::condition::Class;
    use crate::read;
    use crate::symbol::Name;
    use crate::{EvalErrorKind, Runtime};

    #[test]
    fn core_type_operations_respect_their_value_sets() {
        use CoreType::*;

        // Each column describes a disjoint region of values in the abstraction.
        let regions: &[&[CoreType]] = &[
            &[Null, Boolean, Symbol, List],
            &[True, Boolean, NonNilSymbol, Symbol],
            &[Keyword, NonNilSymbol, Symbol],
            &[NonNilSymbol, Symbol],
            &[Cons, List],
            &[Fixnum, Integer, Rational, Real, Number],
            &[Bignum, Integer, Rational, Real, Number],
            &[Ratio, Rational, Real, Number],
            &[Float, Real, Number],
            &[Complex, Number],
            &[Character],
            &[String, Vector, Array],
            &[Vector, Array],
            &[Array],
            &[Function],
            &[HashTable],
            &[Package],
            &[Pathname],
            &[Readtable],
            &[RandomState],
            &[],
        ];
        let mask = |ty| {
            regions.iter().enumerate().fold(0u32, |bits, (i, region)| {
                if ty == Top || ty == NonNull && i != 0 || region.contains(&ty) {
                    bits | (1 << i)
                } else {
                    bits
                }
            })
        };
        let types: Vec<CoreType> = [Top, Empty, NonNull]
            .into_iter()
            .chain(regions.iter().flat_map(|region| region.iter().copied()))
            .collect();
        for &left in &types {
            for &right in &types {
                let (a, b) = (mask(left), mask(right));
                assert_eq!(left.is_subtype(right), a & !b == 0, "{left:?} <= {right:?}");
                assert_eq!(mask(left.meet(right)), a & b, "{left:?} ∩ {right:?}");
                let union = a | b;
                let difference = a & !b;
                let join = mask(left.join(right));
                let excluding = mask(left.excluding(right));
                assert_eq!(union & !join, 0, "{left:?} ∪ {right:?}");
                assert_eq!(difference & !excluding, 0, "{left:?} minus {right:?}");
                for &candidate in &types {
                    let upper = mask(candidate);
                    if union & !upper == 0 {
                        assert_eq!(join & !upper, 0, "{left:?} ∪ {right:?} <= {candidate:?}");
                    }
                    if difference & !upper == 0 {
                        assert_eq!(
                            excluding & !upper,
                            0,
                            "{left:?} minus {right:?} <= {candidate:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn core_type_spec_bounds_distinguish_exact_and_approximate_types() {
        use CoreType::*;

        let rt = Runtime::start().unwrap();
        rt.initialize_packages().unwrap();
        for (source, expected) in [
            ("nil", (Empty, true)),
            ("(and list symbol)", (Null, true)),
            ("(and boolean (not null))", (True, true)),
            ("(or null cons)", (List, true)),
            ("(or cons string)", (NonNull, false)),
            ("(or (satisfies test) t)", (Top, false)),
            ("(and (integer 0 4) fixnum)", (Fixnum, false)),
            ("(member nil t nil)", (Boolean, true)),
            ("(member t)", (True, true)),
            ("(member nil)", (Null, true)),
            ("(member :nil #:t)", (NonNilSymbol, false)),
            ("(member 0 1)", (Fixnum, false)),
            ("(eql t)", (True, true)),
            ("(eql cl:nil)", (Null, true)),
            ("(eql :t)", (NonNilSymbol, false)),
            ("(not (member))", (Top, true)),
            ("(not (not null))", (Null, true)),
            ("(not boolean)", (NonNull, false)),
            ("(not (member 0 1))", (Top, false)),
            ("(integer * *)", (Integer, true)),
            ("(integer :* *)", (Integer, false)),
            ("(integer * * *)", (Top, false)),
            ("(signed-byte * *)", (Top, false)),
            ("(base-string *)", (String, false)),
            ("(and function standard-object)", (Function, false)),
            ("(and function stream)", (Function, false)),
            ("(:or null cons)", (Top, false)),
            ("#:nil", (Top, false)),
        ] {
            let form = read::read(&rt, source).unwrap();
            assert_eq!(parse_type_spec(&form), expected, "{source}");
        }
    }

    #[test]
    fn numeric_return_inference_predicts_the_primary_value_type() {
        use CoreType::*;

        for (op, inputs, expected) in [
            ("/", &[Fixnum, Fixnum][..], Rational),
            ("/", &[Float, Integer][..], Float),
            ("+", &[Float, Complex][..], Number),
            ("+", &[Fixnum, Bignum][..], Integer),
            ("*", &[Integer, Ratio][..], Rational),
            ("REM", &[Real, Float][..], Float),
            ("MOD", &[Top, Integer][..], Real),
            ("ASH", &[Top, Top][..], Integer),
            ("ABS", &[Complex][..], Real),
            ("ABS", &[Fixnum][..], Integer),
            ("SIGNUM", &[Ratio][..], Fixnum),
            ("SIGNUM", &[Complex][..], Complex),
            ("MIN", &[Fixnum, Bignum][..], Integer),
            ("MAX", &[Ratio, Ratio][..], Ratio),
            ("FLOOR", &[Float][..], Integer),
            ("FFLOOR", &[Fixnum][..], Float),
        ] {
            let args: Vec<_> = (0..inputs.len()).map(CoreExpr::nil).collect();
            let name = Name::common_lisp(op);
            assert_eq!(
                builtin_return_type(&name, &args, |arg| inputs[arg.at]),
                expected,
                "{op}"
            );
        }
    }

    #[test]
    fn numeric_return_inference_requires_a_common_lisp_symbol() {
        let args = [CoreExpr::nil(0)];
        assert_eq!(
            builtin_return_type(&Name::keyword("+"), &args, |_| panic!(
                "nonstandard function"
            )),
            CoreType::Top
        );
    }

    #[test]
    fn approximate_core_types_keep_runtime_assertions() {
        let rt = Runtime::start().unwrap();
        rt.load_str(
            "(defgeneric lattice-generic (x))
             (defun lattice-as-base-string ()
               (the base-string (the string (make-array 0 :element-type nil))))
             (defun lattice-generic-as-fixnum ()
               (the fixnum (the (and function standard-object) #'lattice-generic)))
             (defun lattice-keyword () (the keyword ':lattice-keyword))",
        )
        .unwrap();

        for source in [
            "(the (member 0 1) 2)",
            "(the (or cons string) 42)",
            "(the (and fixnum (integer 0 4)) 5)",
            "(the integer (/ 2))",
            "(the fixnum (1+ most-positive-fixnum))",
            "(the real (+ 1.0 #c(1.0 2.0)))",
            "(lattice-as-base-string)",
            "(lattice-generic-as-fixnum)",
            "(progn (unintern (lattice-keyword) \"KEYWORD\") (lattice-keyword))",
        ] {
            let err = rt.eval(source).err().unwrap_or_else(|| panic!("{source}"));
            assert_eq!(
                err.kind(),
                EvalErrorKind::LispCondition(Class::TypeError),
                "{source}"
            );
        }
    }

    #[test]
    fn compound_type_assertions_preserve_predicate_calls() {
        let rt = Runtime::start().unwrap();
        rt.load_str(
            "(defparameter *lattice-predicate-calls* 0)
             (defun lattice-test (x)
               (incf *lattice-predicate-calls*)
               x)
             (defun lattice-check ()
               (the (or (satisfies lattice-test) t) 1)
               *lattice-predicate-calls*)",
        )
        .unwrap();
        let result = rt.eval("(lattice-check)").unwrap();
        assert_eq!(result[0].as_fixnum(), Some(1));
    }
}
