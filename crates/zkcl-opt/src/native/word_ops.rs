//! Scalar operations whose semantics don't depend on a native runtime call.

use zkcl_bytecode::{Immediate, TAG_FIXNUM, TAG_MASK};

use crate::{BitwiseOp, CmpOp, ValueId};

#[derive(Clone, Copy)]
pub enum ScalarConstant {
    Word(Immediate),
    I64(i64),
    U64(u64),
    Bool(bool),
}

#[derive(Clone, Copy)]
pub enum I64Arithmetic {
    Add,
    Sub,
    Mul,
}

/// Compare all or selected bits of a tagged word against its canonical encoding.
#[derive(Clone, Copy)]
pub struct WordPredicate {
    pub mask: Option<u64>,
    pub bits: u64,
    pub equal: bool,
}

impl WordPredicate {
    pub const FIXNUM: Self = Self {
        mask: Some(TAG_MASK),
        bits: TAG_FIXNUM,
        equal: true,
    };
}

#[derive(Clone, Copy)]
pub enum WordOp {
    Constant(ScalarConstant),
    Copy(ValueId),
    Select(ValueId, Immediate, Immediate),
    BoxFixnum(ValueId),
    Test(ValueId, WordPredicate),
    /// A fixnum in the inclusive interval: `(decoded - low) <= span`, both
    /// sides interpreted as unsigned words of the arithmetic width.
    FixnumRange {
        value: ValueId,
        low: i64,
        span: i64,
    },
    /// The producer has proved the operation does not overflow signed i64.
    I64(I64Arithmetic, ValueId, ValueId),
    BitwiseI64(BitwiseOp, ValueId, ValueId),
    CompareI64(CmpOp, ValueId, ValueId),
    /// Negate a canonical SSA boolean (0 or 1), not a Lisp truth value.
    Not(ValueId),
}
