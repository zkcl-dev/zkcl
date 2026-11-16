//! Ordinary control transfer and block boundaries in canonical bytecode.

use std::ops::Range;

use crate::Instruction;

/// The successor rule of an instruction before resolving dynamic control scopes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InstructionFlow {
    Next,
    Jump(usize),
    Branch(usize),
    Transfer,
}

impl InstructionFlow {
    pub fn target(self) -> Option<usize> {
        match self {
            Self::Jump(target) | Self::Branch(target) => Some(target),
            Self::Next | Self::Transfer => None,
        }
    }

    /// Ordinary successors in fallthrough-first order. An instruction that
    /// transfers through a control scope has no ordinary successor until resolved.
    pub fn successors(self, next: usize) -> impl DoubleEndedIterator<Item = usize> {
        let fallthrough = matches!(self, Self::Next | Self::Branch(_)).then_some(next);
        fallthrough.into_iter().chain(self.target())
    }
}

impl Instruction {
    pub fn flow(&self) -> InstructionFlow {
        use InstructionFlow::*;
        match *self {
            Self::Jump(target) => Jump(target),
            Self::Unless(_, target)
            | Self::IfPresent(_, target)
            | Self::UnlessFixnum(_, target)
            | Self::UnlessFixnumInRange(_, _, _, target)
            | Self::WhenBound { jump: target, .. } => Branch(target),
            Self::ReturnFrom { .. } | Self::Throw { .. } | Self::Go { .. } | Self::BeginCleanup => {
                Transfer
            }
            _ => Next,
        }
    }
}

/// Collect reachable transfers and side-table destinations before assigning
/// block IDs. The end of the instruction stream is an explicit exit block.
pub struct BlockLeaders(Vec<bool>);

impl BlockLeaders {
    pub fn new(len: usize) -> Self {
        let mut leaders = Self(vec![false; len + 1]);
        leaders.mark(0);
        leaders.mark(len);
        leaders
    }

    /// Mark a candidate destination; a target past the exit cannot split a block.
    pub fn mark(&mut self, pc: usize) {
        if let Some(leader) = self.0.get_mut(pc) {
            *leader = true;
        }
    }

    pub fn observe(&mut self, pc: usize, flow: InstructionFlow) {
        if flow != InstructionFlow::Next {
            self.mark(pc + 1);
        }
        if let Some(target) = flow.target() {
            self.mark(target);
        }
    }

    pub fn ranges(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        let mut starts = self
            .0
            .iter()
            .enumerate()
            .filter_map(|(pc, &leader)| leader.then_some(pc))
            .peekable();
        std::iter::from_fn(move || {
            let start = starts.next()?;
            Some(start..starts.peek().copied().unwrap_or(start))
        })
    }
}
