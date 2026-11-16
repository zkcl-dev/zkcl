use serde::{Deserialize, Serialize};

/// The checked POPCOUNT assembly forms that can appear in a compiled file.
/// Both compute a count in 0..=64 for every input word and use only registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Aarch64Popcount {
    AddBytes,
    WidenAndAddBytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PopcountImplementations {
    pub portable: bool,
    pub aarch64: Option<Aarch64Popcount>,
}

/// A hint for a canonical named call. Bytecode dispatch ignores the hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VopCall {
    pub pc: usize,
    /// Constant-pool index of an uninterned symbol that captures the Lisp function at load time.
    pub binding: usize,
    pub implementations: PopcountImplementations,
}
