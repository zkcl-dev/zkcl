use serde::{Deserialize, Serialize};

/// A total operation on an unsigned 64-bit value with a result in 0..=64.
/// Both zero counts return 64 for an input of zero.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum WordCountOp {
    Popcount = 0,
    LeadingZeros = 1,
    TrailingZeros = 2,
}

impl WordCountOp {
    pub const fn descriptor(self) -> usize {
        self as usize
    }

    pub fn from_descriptor(descriptor: usize) -> Option<Self> {
        [Self::Popcount, Self::LeadingZeros, Self::TrailingZeros]
            .get(descriptor)
            .copied()
    }

    pub const fn evaluate(self, input: u64) -> u32 {
        match self {
            Self::Popcount => input.count_ones(),
            Self::LeadingZeros => input.leading_zeros(),
            Self::TrailingZeros => input.trailing_zeros(),
        }
    }
}

/// The checked POPCOUNT assembly forms that can appear in a compiled file.
/// Both compute a count in 0..=64 for every input word and use only registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Aarch64Popcount {
    AddBytes,
    WidenAndAddBytes,
}

/// A named call's optional specialization. Its operation is target-independent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VopCall {
    pub pc: usize,
    /// Constant-pool index of a private capture cons or a registered operation descriptor.
    pub binding: usize,
    pub operation: WordCountOp,
    /// Optional backend template. Its semantics must match `operation`.
    pub aarch64_popcount: Option<Aarch64Popcount>,
}
