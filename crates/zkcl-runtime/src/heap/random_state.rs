use std::cell::Cell;

use super::{Header, Heap, HeapObject, ObjectKind, OutOfMemory, Word};

#[derive(Clone, Copy)]
pub(crate) struct RandomState([u64; 4]);

impl RandomState {
    // The initial state is reproducible; these are SplitMix64 outputs from seed zero.
    pub(crate) const INITIAL: Self = Self([
        0xE220_A839_7B1D_CDAF,
        0x6E78_9E6A_A1B9_65F4,
        0x06C4_5D18_8009_454F,
        0xF88B_B8A8_724C_81EC,
    ]);

    pub(crate) fn from_seed(seed: [u8; 32]) -> Self {
        let state = std::array::from_fn(|index| {
            let offset = index * size_of::<u64>();
            u64::from_le_bytes(seed[offset..offset + size_of::<u64>()].try_into().unwrap())
        });
        Self::from_words(state).unwrap_or(Self::INITIAL)
    }

    pub(crate) fn from_words(state: [u64; 4]) -> Option<Self> {
        // The all-zero state cannot advance.
        (state != [0; 4]).then_some(Self(state))
    }

    pub(crate) fn words(self) -> [u64; 4] {
        self.0
    }

    pub(crate) fn next(&mut self) -> u64 {
        // xoshiro256** by David Blackman and Sebastiano Vigna.
        // https://prng.di.unimi.it/xoshiro256starstar.c
        let [a, b, c, d] = self.0;
        let result = b.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let c = c ^ a;
        let d = d ^ b;
        self.0 = [a ^ d, b ^ c, c ^ (b << 17), d.rotate_left(45)];
        result
    }
}

#[repr(C)]
struct LispRandomState {
    header: Header,
    state: Cell<RandomState>,
}

// SAFETY: `LispRandomState` is `#[repr(C)]` with `header: Header` first and always tagged `ObjectKind::RandomState`.
unsafe impl HeapObject for LispRandomState {
    const KIND: ObjectKind = ObjectKind::RandomState;
}

impl Heap {
    pub(crate) fn allocate_random_state(&self, state: RandomState) -> Result<Word, OutOfMemory> {
        self.alloc_object(|header| LispRandomState {
            header,
            state: Cell::new(state),
        })
    }

    pub(crate) fn is_random_state(&self, word: Word) -> bool {
        self.object_ref::<LispRandomState>(word).is_some()
    }

    pub(crate) fn random_state_value(&self, word: Word) -> Option<RandomState> {
        Some(self.object_ref::<LispRandomState>(word)?.state.get())
    }

    pub(crate) fn set_random_state(&self, word: Word, value: RandomState) {
        self.expect_object::<LispRandomState>(word, "validated random state")
            .state
            .set(value);
    }
}
