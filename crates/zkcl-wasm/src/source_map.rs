//! Map generated Core Wasm instructions to canonical Lisp locations.

use std::ops::Range;

use serde::{Deserialize, Serialize};

pub const SECTION_NAME: &str = "zkcl.source";

#[derive(Default, Serialize, Deserialize)]
pub struct SourceMap {
    pub files: Vec<String>,
    pub functions: Vec<FunctionSourceMap>,
}

#[derive(Serialize, Deserialize)]
pub struct FunctionSourceMap {
    pub function: u32,
    pub source_file: Option<u32>,
    pub instructions: Vec<InstructionRange>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionRange {
    /// Relative to the start of the function body, including its local declarations.
    pub wasm: Range<u32>,
    pub bytecode_pc: u32,
    pub source_offset: Option<u64>,
}

impl SourceMap {
    #[cfg(feature = "codegen")]
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    #[cfg(feature = "engine")]
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let (map, trailing): (Self, _) = postcard::take_from_bytes(bytes).ok()?;
        if !trailing.is_empty()
            || !map
                .functions
                .windows(2)
                .all(|pair| pair[0].function < pair[1].function)
            || !map.functions.iter().all(|function| {
                function
                    .source_file
                    .is_none_or(|index| (index as usize) < map.files.len())
                    && function
                        .instructions
                        .iter()
                        .all(|site| !site.wasm.is_empty())
                    && function
                        .instructions
                        .windows(2)
                        .all(|pair| pair[0].wasm.end <= pair[1].wasm.start)
            })
        {
            return None;
        }
        Some(map)
    }

    #[cfg(feature = "engine")]
    pub(crate) fn function(&self, index: u32) -> Option<&FunctionSourceMap> {
        let position = self
            .functions
            .binary_search_by_key(&index, |entry| entry.function)
            .ok()?;
        Some(&self.functions[position])
    }
}

impl FunctionSourceMap {
    #[cfg(feature = "engine")]
    pub(crate) fn instruction(&self, offset: u32) -> Option<&InstructionRange> {
        let position = self
            .instructions
            .partition_point(|site| site.wasm.start <= offset);
        let site = self.instructions.get(position.checked_sub(1)?)?;
        site.wasm.contains(&offset).then_some(site)
    }
}
