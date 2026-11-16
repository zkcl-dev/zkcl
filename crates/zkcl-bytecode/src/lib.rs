//! Register bytecode and the compiled-file format shared by the compiler and loader.

mod compiled_file;
pub mod dispatch;
pub mod effects;
#[cfg(feature = "analysis")]
mod flow;
mod immediate;
mod instruction;
mod literal_graph;
#[cfg(feature = "analysis")]
mod liveness;
pub mod native_abi;
mod registers;
mod simd;
mod tail_calls;
mod verify;
mod vop;

pub use compiled_file::*;
pub use effects::{AliasDomain, BarrierEffect, ControlEffect, OpEffects};
#[cfg(feature = "analysis")]
pub use flow::{BlockLeaders, InstructionFlow};
pub use immediate::*;
pub use instruction::*;
#[cfg(feature = "analysis")]
pub use liveness::{LiveEdge, LiveIns, LiveSet, RegisterTransfer};
pub use registers::{RegisterOperands, RegisterSet};
pub use simd::{LaneKind, PackShape, SimdMask, SimdPack};
pub use tail_calls::tail_call_sites;
pub use verify::ValidationError;
pub use vop::{Aarch64Popcount, VopCall, WordCountOp};
