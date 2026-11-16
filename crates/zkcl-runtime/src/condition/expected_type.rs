use crate::Runtime;
use crate::heap::{RootSlot, Word};
use crate::number::{Number, Real};
use crate::symbol::{Name, Package};

/// Lisp type specifiers are built when the VM requests the condition object.
pub(super) enum ExpectedType {
    Named(&'static str),
    Specifier(RootSlot),
    SymbolList(&'static [&'static str]),
    FunctionName,
    CompileDefinition,
    CharacterDesignator,
    RestartDesignator,
    IntegerRange(isize, Option<isize>),
    NonNegativeIntegerOr(&'static str),
    VectorSatisfying(&'static str),
    RandomLimit,
}

impl ExpectedType {
    pub(super) fn to_type_specifier(&self, runtime: &Runtime, at: usize) -> Option<RootSlot> {
        let symbol = |name: &str| {
            runtime
                .intern(&Name::common_lisp(name), at)
                .ok()
                .map(|word| runtime.root_slot(word))
        };
        let list = |names: &[&str], arguments: &[RootSlot]| {
            let mut tail = runtime.root_slot(Word::NIL);
            for value in arguments.iter().rev() {
                tail = runtime.root_slot(runtime.heap.cons(|| (value.get(), tail.get())).ok()?);
            }
            for name in names.iter().rev() {
                let value = symbol(name)?;
                tail = runtime.root_slot(runtime.heap.cons(|| (value.get(), tail.get())).ok()?);
            }
            Some(tail)
        };
        let fixnum = |value| Word::fixnum(value).map(RootSlot::immediate);
        let integer = |minimum, maximum| {
            let lower = fixnum(minimum)?;
            match maximum {
                Some(upper) => list(&["INTEGER"], &[lower, fixnum(upper)?]),
                None => list(&["INTEGER"], &[lower]),
            }
        };

        match self {
            // Implementation types such as INPUT-STREAM can be internal to COMMON-LISP.
            Self::Named(name) => {
                let (name, _) = runtime
                    .intern_package(Package::COMMON_LISP, name, at)
                    .ok()?;
                Some(runtime.root_slot(name))
            }
            Self::Specifier(specifier) => Some(runtime.root_slot(specifier.get())),
            Self::SymbolList(names) => list(names, &[]),
            Self::FunctionName => {
                let setf = list(&["EQL", "SETF"], &[])?;
                let arguments = list(&["CONS", "SYMBOL", "NULL"], &[])?;
                let name = list(&["CONS"], &[setf, arguments])?;
                list(&["OR", "SYMBOL"], &[name])
            }
            Self::CompileDefinition => {
                let lambda = list(&["EQL", "LAMBDA"], &[])?;
                let expression = list(&["CONS"], &[lambda, symbol("CONS")?])?;
                list(&["OR", "FUNCTION"], &[expression])
            }
            Self::CharacterDesignator => {
                let single_character = list(&["STRING"], &[fixnum(1)?])?;
                list(&["OR", "CHARACTER", "SYMBOL"], &[single_character])
            }
            Self::RestartDesignator => {
                let non_nil = list(&["NOT", "NULL"], &[])?;
                let symbol = list(&["AND", "SYMBOL"], &[non_nil])?;
                list(&["OR", "RESTART"], &[symbol])
            }
            Self::IntegerRange(minimum, maximum) => integer(*minimum, *maximum),
            Self::NonNegativeIntegerOr(other) => list(&["OR", other], &[integer(0, None)?]),
            Self::VectorSatisfying(predicate) => {
                let constraint = list(&["SATISFIES", predicate], &[])?;
                list(&["AND", "VECTOR"], &[constraint])
            }
            Self::RandomLimit => {
                let positive_integer = integer(1, None)?;
                let zero = runtime
                    .heap
                    .allocate_number(&Number::Real(Real::Single(0.0)))
                    .ok()?;
                let exclusive_lower_bound = list(&[], &[runtime.root_slot(zero)])?;
                let positive_float = list(&["FLOAT"], &[exclusive_lower_bound])?;
                list(&["OR"], &[positive_integer, positive_float])
            }
        }
    }
}
