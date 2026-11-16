//! Selected scalar operations shared by native emitters and Core Wasm.

use zkcl_bytecode::{FixnumArithmetic, native_abi::FixnumComparison};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand {
    Arg(u8),
    Immediate(u64),
    Local(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Test {
    Nil,
    Fixnum,
    FixnumInRange(i32, i32),
}

/// Boolean operations on the payload of two tagged fixnums.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitwiseOp {
    And,
    Or,
    Xor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Arithmetic {
        op: FixnumArithmetic,
        lhs: Operand,
        rhs: Option<Operand>,
        /// The result may overflow the target fixnum. Return a leaf miss if it does.
        checked: bool,
    },
    Compare {
        relation: FixnumComparison,
        lhs: Operand,
        rhs: Operand,
    },
    Bitwise {
        op: BitwiseOp,
        lhs: Operand,
        rhs: Operand,
    },
    Complement(Operand),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    CheckFixnum(Operand),
    CheckLoopBudget,
    Copy { destination: usize, value: Operand },
    Compute { destination: usize, op: Operation },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    Return(Operand),
    Miss,
    Jump(usize),
    Branch {
        value: Operand,
        test: Test,
        yes: usize,
        no: usize,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct Block {
    pub steps: Vec<Step>,
    pub exit: Exit,
}

impl Step {
    pub(super) fn may_miss(&self) -> bool {
        matches!(
            self,
            Self::CheckFixnum(_)
                | Self::CheckLoopBudget
                | Self::Compute {
                    op: Operation::Arithmetic { checked: true, .. },
                    ..
                }
        )
    }

    pub(super) fn visit_inputs(&mut self, mut visit: impl FnMut(&mut Operand)) {
        match self {
            Self::CheckLoopBudget => {}
            Self::CheckFixnum(operand) | Self::Copy { value: operand, .. } => visit(operand),
            Self::Compute { op, .. } => match op {
                Operation::Arithmetic { lhs, rhs, .. } => {
                    visit(lhs);
                    if let Some(rhs) = rhs {
                        visit(rhs);
                    }
                }
                Operation::Compare { lhs, rhs, .. } | Operation::Bitwise { lhs, rhs, .. } => {
                    visit(lhs);
                    visit(rhs);
                }
                Operation::Complement(value) => visit(value),
            },
        }
    }

    pub(super) fn destination(&mut self) -> Option<&mut usize> {
        match self {
            Self::Copy { destination, .. } | Self::Compute { destination, .. } => Some(destination),
            Self::CheckFixnum(_) | Self::CheckLoopBudget => None,
        }
    }
}

impl Exit {
    pub(super) fn operand(&mut self) -> Option<&mut Operand> {
        match self {
            Self::Return(value) | Self::Branch { value, .. } => Some(value),
            Self::Miss | Self::Jump(_) => None,
        }
    }

    pub fn visit_targets(&self, mut visit: impl FnMut(usize)) {
        match *self {
            Self::Jump(target) => visit(target),
            Self::Branch { yes, no, .. } => {
                visit(yes);
                visit(no);
            }
            Self::Return(_) | Self::Miss => {}
        }
    }

    pub(super) fn map_targets(&mut self, mut map: impl FnMut(usize) -> usize) {
        match self {
            Self::Jump(target) => *target = map(*target),
            Self::Branch { yes, no, .. } => {
                *yes = map(*yes);
                *no = map(*no);
            }
            Self::Return(_) | Self::Miss => {}
        }
    }
}
