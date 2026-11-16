use crate::EvalError;
use crate::heap::{
    CELL_VALUE_OFFSET, CLOSURE_CAPTURES_OFFSET, CLOSURE_CODE_OFFSET, FUNCTION_CONSTANTS_OFFSET,
    FUNCTION_INFO_OFFSET, Word,
};
use zkcl_bytecode::Class;

/// Tagged `Word` and heap-object layout constants shared with AOT and JIT codegen.
pub struct WordLayout;

impl WordLayout {
    pub const VERSION: u32 = 1;
    pub const POINTER_WIDTH_BITS: u8 = usize::BITS as u8;
    pub const TAG_BITS: u32 = zkcl_target::FIXNUM_SHIFT;
    pub const TAG_MASK: usize = 0b111;
    pub const TAG_OBJECT: usize = 0;
    pub const TAG_FIXNUM: usize = 1;
    pub const TAG_NIL_CHAR: usize = 2;
    pub const TAG_T: usize = 3;
    pub const TAG_PRIMITIVE: usize = 4;
    pub const TAG_PACKAGE: usize = 5;
    pub const TAG_NATIVE: usize = 6;
    pub const TAG_EXIT_TOKEN: usize = 7;

    pub const UNBOUND: usize = Word::UNBOUND.raw();
    pub const NIL: usize = Word::NIL.raw();
    pub const T: usize = Word::T.raw();
    pub const FIXNUM_SHIFT: u32 = zkcl_target::FIXNUM_SHIFT;
    pub const FIXNUM_TAG: usize = 1;
    pub const FIXNUM_MIN: isize = Word::FIXNUM_MIN;
    pub const FIXNUM_MAX: isize = Word::FIXNUM_MAX;

    pub const FUNCTION_INFO_OFFSET: usize = FUNCTION_INFO_OFFSET;
    pub const FUNCTION_CONSTANTS_OFFSET: usize = FUNCTION_CONSTANTS_OFFSET;
    pub const CLOSURE_CODE_OFFSET: usize = CLOSURE_CODE_OFFSET;
    pub const CLOSURE_CAPTURES_OFFSET: usize = CLOSURE_CAPTURES_OFFSET;
    pub const CELL_VALUE_OFFSET: usize = CELL_VALUE_OFFSET;
}

const fn compute_bytecode_contract_version() -> u64 {
    let inputs: [u64; 13] = [
        WordLayout::POINTER_WIDTH_BITS as u64,
        WordLayout::VERSION as u64,
        WordLayout::TAG_BITS as u64,
        WordLayout::UNBOUND as u64,
        WordLayout::NIL as u64,
        WordLayout::T as u64,
        WordLayout::FIXNUM_SHIFT as u64,
        WordLayout::FUNCTION_INFO_OFFSET as u64,
        WordLayout::FUNCTION_CONSTANTS_OFFSET as u64,
        WordLayout::CLOSURE_CODE_OFFSET as u64,
        WordLayout::CLOSURE_CAPTURES_OFFSET as u64,
        WordLayout::CELL_VALUE_OFFSET as u64,
        zkcl_bytecode::native_abi::NATIVE_CALL_ABI_REVISION as u64,
    ];
    hash_contract(inputs)
}

const fn hash_contract(inputs: [u64; 13]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < inputs.len() {
        hash ^= inputs[i];
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

/// Binary ABI contract fingerprint verified when registering AOT modules.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AbiFingerprint {
    pub pointer_width: u8,
    pub word_layout_version: u32,
    pub bytecode_contract_version: u64,
}

impl AbiFingerprint {
    pub const CURRENT: Self = Self {
        pointer_width: WordLayout::POINTER_WIDTH_BITS,
        word_layout_version: WordLayout::VERSION,
        bytecode_contract_version: compute_bytecode_contract_version(),
    };

    pub const fn current() -> Self {
        Self::CURRENT
    }

    /// ABI fingerprint for an offline compilation target.
    ///
    /// The heap header contains two words followed by a u16/u8 trailer, rounded
    /// to word alignment. Function and closure metadata each add two words.
    /// These are repr(C) layouts, independent of the host running the compiler.
    pub fn for_target(target: &crate::MachineTarget) -> Self {
        let bits = target.pointer_width().bits() as u8;
        let word = u64::from(bits / 8);
        Self {
            pointer_width: bits,
            word_layout_version: WordLayout::VERSION,
            bytecode_contract_version: hash_contract([
                u64::from(bits),
                u64::from(WordLayout::VERSION),
                u64::from(WordLayout::TAG_BITS),
                WordLayout::UNBOUND as u64,
                WordLayout::NIL as u64,
                WordLayout::T as u64,
                u64::from(WordLayout::FIXNUM_SHIFT),
                3 * word,
                5 * word,
                3 * word,
                5 * word,
                3 * word,
                u64::from(zkcl_bytecode::native_abi::NATIVE_CALL_ABI_REVISION),
            ]),
        }
    }

    pub fn validate(&self) -> Result<(), EvalError> {
        if *self == Self::CURRENT {
            Ok(())
        } else {
            Err(EvalError::new(
                0,
                format!(
                    "incompatible AOT ABI fingerprint: expected {:?}, got {:?}",
                    Self::CURRENT,
                    self
                ),
            )
            .with_condition(Class::ProgramError))
        }
    }
}
