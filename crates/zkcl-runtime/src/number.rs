use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::Zero;

/// Owned arithmetic values, independent of any Lisp heap address.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Number {
    Real(Real),
    Complex(Real, Real),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Real {
    Integer(BigInt),
    Ratio(BigRational),
    Single(f32),
    Double(f64),
}

impl Real {
    pub(crate) fn as_rational(&self) -> Option<BigRational> {
        match self {
            Self::Integer(value) => Some(BigRational::from_integer(value.clone())),
            Self::Ratio(value) => Some(value.clone()),
            Self::Single(value) => BigRational::from_float(*value),
            Self::Double(value) => BigRational::from_float(*value),
        }
    }

    pub(crate) fn is_exact_zero(&self) -> bool {
        match self {
            Self::Integer(value) => value.is_zero(),
            Self::Ratio(value) => value.is_zero(),
            Self::Single(_) | Self::Double(_) => false,
        }
    }
}

/// Numeric membership without copying an owned arithmetic value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NumberKind {
    Fixnum,
    Bignum,
    Ratio,
    Single,
    Double,
    Complex,
}

/// A scalar real whose payload fits in a native register.
#[derive(Clone, Copy)]
pub(crate) enum MachineReal {
    Fixnum(isize),
    Single(f32),
    Double(f64),
}
