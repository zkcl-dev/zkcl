//! Lower integer bit primitives once, before selecting target instructions.

use zkcl_bytecode::FIXNUM_SHIFT;
use zkcl_bytecode::native_abi::{FixnumComparison, FixnumPrimitive, IntegerBitPrimitive};

use crate::leaf::{BitwiseOp, Operand, Operation, Step};

use super::{Bounds, Fact, SelectionError, Selector};

impl<Resolve> Selector<'_, Resolve>
where
    Resolve: Fn(usize) -> Option<zkcl_bytecode::native_abi::LeafPrimitive>,
{
    pub(super) fn integer_bits(
        &mut self,
        primitive: IntegerBitPrimitive,
        operands: &[Operand],
        ranges: &mut Vec<(Operand, Bounds)>,
        steps: &mut Vec<Step>,
        pc: usize,
    ) -> Result<Operand, SelectionError> {
        use BitwiseOp::{And, Or, Xor};
        use IntegerBitPrimitive as Primitive;

        let invalid = SelectionError::Operand { pc };
        if matches!(
            primitive,
            Primitive::And | Primitive::Or | Primitive::Xor | Primitive::Eqv
        ) {
            self.charge_expansion(operands.len().saturating_sub(2))?;
        } else {
            let arity = match primitive {
                Primitive::Not | Primitive::Oddp | Primitive::Evenp => 1,
                _ => 2,
            };
            if operands.len() != arity {
                return Err(invalid);
            }
        }
        for &operand in operands {
            self.fixnum_operand(operand, ranges, steps).ok_or(invalid)?;
        }
        let zero = self.fixnum(0);
        Ok(match (primitive, operands) {
            (Primitive::And | Primitive::Or | Primitive::Xor | Primitive::Eqv, _) => {
                let op = match primitive {
                    Primitive::And => And,
                    Primitive::Or => Or,
                    _ => Xor,
                };
                let (mut accum, rest) = operands.split_first().map_or(
                    (self.fixnum(if op == And { -1 } else { 0 }), &[][..]),
                    |(&first, rest)| (first, rest),
                );
                for &operand in rest {
                    accum = self.bitwise(op, accum, operand, ranges, steps);
                }
                // EQV complements the XOR on each binary reduction. These
                // complements cancel in pairs, including the -1 identity.
                if primitive == Primitive::Eqv && operands.len().is_multiple_of(2) {
                    self.complement(accum, ranges, steps)
                } else {
                    accum
                }
            }
            (Primitive::Nand | Primitive::Nor, &[lhs, rhs]) => {
                let op = if primitive == Primitive::Nand {
                    And
                } else {
                    Or
                };
                let value = self.bitwise(op, lhs, rhs, ranges, steps);
                self.complement(value, ranges, steps)
            }
            (
                Primitive::AndC1 | Primitive::AndC2 | Primitive::OrC1 | Primitive::OrC2,
                &[lhs, rhs],
            ) => {
                let op = if matches!(primitive, Primitive::AndC1 | Primitive::AndC2) {
                    And
                } else {
                    Or
                };
                let (inverted, other) = if matches!(primitive, Primitive::AndC1 | Primitive::OrC1) {
                    (lhs, rhs)
                } else {
                    (rhs, lhs)
                };
                let inverted = self.complement(inverted, ranges, steps);
                self.bitwise(op, other, inverted, ranges, steps)
            }
            (Primitive::Not, &[value]) => self.complement(value, ranges, steps),
            (Primitive::Test, &[lhs, rhs]) => {
                let value = self.bitwise(And, lhs, rhs, ranges, steps);
                self.numeric(
                    FixnumPrimitive::Compare(FixnumComparison::Ne),
                    &[value, zero],
                    ranges,
                    steps,
                )
                .expect("fixnum outputs")
            }
            (Primitive::Oddp | Primitive::Evenp, &[value]) => {
                let bit = self.bitwise(And, value, self.fixnum(1), ranges, steps);
                let relation = if primitive == Primitive::Oddp {
                    FixnumComparison::Ne
                } else {
                    FixnumComparison::Eq
                };
                self.numeric(
                    FixnumPrimitive::Compare(relation),
                    &[bit, zero],
                    ranges,
                    steps,
                )
                .expect("fixnum outputs")
            }
            (Primitive::Bitp, &[index, integer]) => {
                let index = self.bounds(index, ranges);
                if index.low != index.high || index.low < 0 {
                    return Err(SelectionError::BitIndex { pc });
                }
                let sign_bit = i64::from(self.word_bits) - i64::from(FIXNUM_SHIFT) - 1;
                let (value, relation) = if index.low >= sign_bit {
                    (integer, FixnumComparison::Lt)
                } else {
                    let mask = self.fixnum(1 << index.low);
                    (
                        self.bitwise(And, integer, mask, ranges, steps),
                        FixnumComparison::Ne,
                    )
                };
                self.numeric(
                    FixnumPrimitive::Compare(relation),
                    &[value, zero],
                    ranges,
                    steps,
                )
                .expect("fixnum outputs")
            }
            _ => unreachable!("bit primitive arity checked"),
        })
    }

    fn bounds(&self, value: Operand, ranges: &[(Operand, Bounds)]) -> Bounds {
        self.fact(ranges, value)
            .fixnum(self.word_bits)
            .expect("checked bit operand")
    }

    fn complement(
        &mut self,
        value: Operand,
        ranges: &[(Operand, Bounds)],
        steps: &mut Vec<Step>,
    ) -> Operand {
        let Bounds { low, high } = self.bounds(value, ranges);
        if low == high {
            return self.fixnum(!low);
        }
        self.compute(
            Operation::Complement(value),
            Fact::Fixnum(Bounds {
                low: !high,
                high: !low,
            }),
            steps,
        )
    }

    fn bitwise(
        &mut self,
        op: BitwiseOp,
        lhs: Operand,
        rhs: Operand,
        ranges: &[(Operand, Bounds)],
        steps: &mut Vec<Step>,
    ) -> Operand {
        use BitwiseOp::{And, Or, Xor};

        if lhs == rhs {
            return if op == Xor { self.fixnum(0) } else { lhs };
        }
        let mut lhs_bounds = self.bounds(lhs, ranges);
        let mut rhs_bounds = self.bounds(rhs, ranges);
        let (lhs, rhs) = if lhs_bounds.low == lhs_bounds.high && rhs_bounds.low != rhs_bounds.high {
            std::mem::swap(&mut lhs_bounds, &mut rhs_bounds);
            (rhs, lhs)
        } else {
            (lhs, rhs)
        };
        if rhs_bounds.low == rhs_bounds.high {
            let rhs_constant = rhs_bounds.low;
            if lhs_bounds.low == lhs_bounds.high {
                let lhs_constant = lhs_bounds.low;
                return self.fixnum(match op {
                    And => lhs_constant & rhs_constant,
                    Or => lhs_constant | rhs_constant,
                    Xor => lhs_constant ^ rhs_constant,
                });
            }
            match (op, rhs_constant) {
                (And, 0) | (Or, -1) => return self.fixnum(rhs_constant),
                (And, -1) | (Or | Xor, 0) => return lhs,
                (Xor, -1) => return self.complement(lhs, ranges, steps),
                _ => {}
            }
        }
        // AND can only clear bits. A known nonnegative operand also
        // clears the sign bit, giving an upper bound on the result.
        let bounds = if op == And {
            [lhs_bounds, rhs_bounds]
                .into_iter()
                .filter(|bounds| bounds.low >= 0)
                .map(|bounds| bounds.high)
                .min()
                .map_or(self.fixnums, |high| Bounds { low: 0, high })
        } else {
            self.fixnums
        };
        self.compute(
            Operation::Bitwise { op, lhs, rhs },
            Fact::Fixnum(bounds),
            steps,
        )
    }
}
