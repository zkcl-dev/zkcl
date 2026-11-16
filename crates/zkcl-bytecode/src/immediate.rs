//! Tagged immediates shared by bytecode, the interpreter, and native emitters.

use serde::{Deserialize, Serialize};

pub use zkcl_target::{FIXNUM_SHIFT, fixnum_bounds_for_bits};
pub const TAG_MASK: u64 = 7;
pub const TAG_FIXNUM: u64 = 1;
pub const WORD_UNBOUND: u64 = 0;
pub const WORD_NIL: u64 = 2;
pub const WORD_T: u64 = 3;

/// A value encoded directly in an instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Immediate {
    Nil,
    True,
    Fixnum(i64),
    Character(char),
}

impl Immediate {
    /// Encode using the target's pointer width. The high bits of a 32-bit word are zero.
    pub const fn encode_word(self, word_bits: u8) -> Option<u64> {
        let mask = match word_bits {
            32 => u32::MAX as u64,
            64 => u64::MAX,
            _ => return None,
        };
        Some(match self {
            Self::Nil => WORD_NIL,
            Self::True => WORD_T,
            Self::Character(c) => (((c as u64) + 1) << FIXNUM_SHIFT) | WORD_NIL,
            Self::Fixnum(n) => {
                let Some((min, max)) = fixnum_bounds_for_bits(word_bits) else {
                    return None;
                };
                if n < min || n > max {
                    return None;
                }
                (((n as u64) << FIXNUM_SHIFT) | TAG_FIXNUM) & mask
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixnum_words_respect_target_width() {
        assert_eq!(Immediate::Fixnum(-1).encode_word(32), Some(0xffff_fff9));
        assert_eq!(
            Immediate::Fixnum(-1).encode_word(64),
            Some(0xffff_ffff_ffff_fff9)
        );
        for bits in [32, 64] {
            let limit = 1i64 << (bits - 4);
            for n in [-limit, limit - 1] {
                assert!(Immediate::Fixnum(n).encode_word(bits).is_some());
            }
            for n in [-limit - 1, limit] {
                assert_eq!(Immediate::Fixnum(n).encode_word(bits), None);
            }
        }
    }
}
