//! Closed intervals of target fixnums. A bound outside the representation
//! makes the proof unavailable; ordinary Common Lisp arithmetic still promotes.

use crate::read::{Form, FormKind};
use zkcl_bytecode::FixnumArithmetic;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IntegerRange {
    low: i64,
    high: i64,
}

impl IntegerRange {
    pub const BIT: Self = Self { low: 0, high: 1 };
    pub fn for_target(target: &zkcl_target::CompilationTarget) -> Self {
        let (low, high) = target.fixnum_bounds();
        Self { low, high }
    }

    fn checked_range(self, low: i128, high: i128) -> Option<Self> {
        (i128::from(self.low) <= low && low <= high && high <= i128::from(self.high)).then_some(
            Self {
                low: low as i64,
                high: high as i64,
            },
        )
    }

    pub(super) fn from_bounds(low: i64, high: i64) -> Option<Self> {
        (low <= high).then_some(Self { low, high })
    }

    pub(crate) fn bounds(self) -> (i64, i64) {
        (self.low, self.high)
    }

    pub fn contains(self, other: Self) -> bool {
        self.low <= other.low && other.high <= self.high
    }

    pub fn disjoint(self, other: Self) -> bool {
        self.high < other.low || other.high < self.low
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        self.checked_range(
            self.low.max(other.low).into(),
            self.high.min(other.high).into(),
        )
    }

    pub(super) fn singleton(value: i64) -> Self {
        Self {
            low: value,
            high: value,
        }
    }

    /// Split at the first integer of the upper interval, when both halves are nonempty.
    pub(super) fn split_at(self, cut: i128) -> Option<[Self; 2]> {
        let low = self.checked_range(self.low.into(), (cut - 1).min(self.high.into()))?;
        let high = self.checked_range(cut.max(self.low.into()), self.high.into())?;
        Some([low, high])
    }

    pub fn spec(self, form: &Form) -> Option<Self> {
        if let Some(name) = form.name() {
            return match name.operator() {
                "FIXNUM" => Some(self),
                "BIT" => Some(Self::BIT),
                _ => None,
            };
        }
        let (head, args) = form.list()?.split_first()?;
        match (head.name()?.operator(), args) {
            ("INTEGER", [low, high]) => self.checked_range(bound(low, 1)?, bound(high, -1)?),
            ("MOD", [n]) => self.checked_range(0, literal(n)? - 1),
            ("UNSIGNED-BYTE", [n]) => {
                let width = u32::try_from(literal(n)?).ok()?;
                if width == 0 || width >= i64::BITS {
                    return None;
                }
                self.checked_range(0, (1i128 << width) - 1)
            }
            ("SIGNED-BYTE", [n]) => {
                let width = u32::try_from(literal(n)?).ok()?;
                if width == 0 || width > i64::BITS {
                    return None;
                }
                let limit = 1i128 << (width - 1);
                self.checked_range(-limit, limit - 1)
            }
            _ => None,
        }
    }

    pub fn comparison(self, op: &str, rhs: Self) -> Option<bool> {
        match op {
            "<" if self.high < rhs.low => Some(true),
            "<" if self.low >= rhs.high => Some(false),
            "<=" if self.high <= rhs.low => Some(true),
            "<=" if self.low > rhs.high => Some(false),
            ">" => rhs.comparison("<", self),
            ">=" => rhs.comparison("<=", self),
            _ => None,
        }
    }

    pub(super) fn arithmetic(self, op: FixnumArithmetic, rhs: Self, domain: Self) -> Option<Self> {
        let (a, b) = (i128::from(self.low), i128::from(self.high));
        let (c, d) = (i128::from(rhs.low), i128::from(rhs.high));
        match op {
            FixnumArithmetic::Add | FixnumArithmetic::Inc => domain.checked_range(a + c, b + d),
            FixnumArithmetic::Sub | FixnumArithmetic::Dec => domain.checked_range(a - d, b - c),
            FixnumArithmetic::Mul => {
                let ends = [a * c, a * d, b * c, b * d];
                domain.checked_range(*ends.iter().min()?, *ends.iter().max()?)
            }
        }
    }
}

fn literal(form: &Form) -> Option<i128> {
    if let Some(root) = form.original_object() {
        return Some(root.get().as_fixnum()? as i128);
    }
    match &form.kind {
        FormKind::Integer(value) => Some(*value as i128),
        FormKind::Literal(root) => Some(root.get().as_fixnum()? as i128),
        _ => None,
    }
}

fn bound(form: &Form, exclusive_step: i128) -> Option<i128> {
    if let Some([inner]) = form.list() {
        Some(literal(inner)? + exclusive_step)
    } else {
        literal(form)
    }
}
