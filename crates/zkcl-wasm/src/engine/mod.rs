mod instance;
mod lower;
mod module;
mod numeric;

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use thiserror::Error;

pub use instance::{Caller, Instance};
pub use module::Module;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    I32,
    I64,
    F32,
    F64,
    FuncRef,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    FuncRef(Option<FuncRef>),
}

/// A function reference owned by one instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuncRef {
    instance: u64,
    index: u32,
}

impl Value {
    pub fn value_type(self) -> ValueType {
        match self {
            Self::I32(_) => ValueType::I32,
            Self::I64(_) => ValueType::I64,
            Self::F32(_) => ValueType::F32,
            Self::F64(_) => ValueType::F64,
            Self::FuncRef(_) => ValueType::FuncRef,
        }
    }

    fn bits(self) -> u64 {
        match self {
            Self::I32(value) => u64::from(value as u32),
            Self::I64(value) => value as u64,
            Self::F32(bits) => u64::from(bits),
            Self::F64(bits) => bits,
            Self::FuncRef(reference) => {
                reference.map_or(0, |reference| u64::from(reference.index) + 1)
            }
        }
    }

    fn from_bits(ty: ValueType, bits: u64, instance: u64) -> Self {
        match ty {
            ValueType::I32 => Self::I32(bits as i32),
            ValueType::I64 => Self::I64(bits as i64),
            ValueType::F32 => Self::F32(bits as u32),
            ValueType::F64 => Self::F64(bits),
            ValueType::FuncRef => Self::FuncRef(bits.checked_sub(1).map(|index| FuncRef {
                instance,
                index: index as u32,
            })),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuncType {
    pub params: Box<[ValueType]>,
    pub results: Box<[ValueType]>,
}

pub struct FuncImport<'a> {
    pub module: &'a str,
    pub name: &'a str,
    pub ty: &'a FuncType,
}

pub type HostFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Value>, Trap>> + 'a>>;

pub enum HostCall<'a> {
    Ready(Vec<Value>),
    Pending(HostFuture<'a>),
}

pub trait Host {
    type Func;

    fn link(&mut self, import: FuncImport<'_>) -> Result<Self::Func, Error>;

    fn call<'a>(
        &'a mut self,
        function: &Self::Func,
        arguments: &[Value],
        caller: Caller<'a>,
    ) -> Result<HostCall<'a>, Trap>;
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub memory_bytes: usize,
    pub table_elements: usize,
    pub call_depth: usize,
    pub stack_slots: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: 256 << 20,
            table_elements: 1 << 20,
            call_depth: 16_384,
            stack_slots: 1 << 20,
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Invalid(#[from] wasmparser::BinaryReaderError),
    #[error("unsupported Wasm {feature} at byte {offset}")]
    Unsupported { feature: String, offset: usize },
    #[error("missing Wasm import {module}.{name}")]
    Import { module: String, name: String },
    #[error("Wasm signature mismatch for {0}")]
    Signature(String),
    #[error("no Wasm function export {0:?}")]
    Export(String),
    #[error("Wasm instantiation failed: {0}")]
    Initialize(#[source] Trap),
    #[error("Wasm {frame}: {source}")]
    Execution {
        frame: TrapFrame,
        callers: Box<[TrapFrame]>,
        source: Trap,
    },
}

#[derive(Debug)]
pub struct TrapFrame {
    pub function: u32,
    pub name: Option<Box<str>>,
    pub offset: Option<usize>,
    pub lisp: Option<Box<LispLocation>>,
}

#[derive(Debug)]
pub struct LispLocation {
    pub file: Option<Box<str>>,
    pub bytecode_pc: Option<u32>,
    pub source_offset: Option<u64>,
}

impl fmt::Display for TrapFrame {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "function {}", self.function)?;
        if let Some(name) = &self.name {
            write!(output, " ({name:?})")?;
        }
        if let Some(offset) = self.offset {
            write!(output, " at byte {offset}")?;
        }
        if let Some(lisp) = &self.lisp {
            write!(output, " (Lisp")?;
            if let Some(file) = &lisp.file {
                write!(output, " {file:?}")?;
            }
            if let Some(offset) = lisp.source_offset {
                write!(output, " at byte {offset}")?;
            }
            if let Some(pc) = lisp.bytecode_pc {
                write!(output, ", bytecode PC {pc}")?;
            }
            write!(output, ")")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum Trap {
    #[error("unreachable instruction")]
    Unreachable,
    #[error("integer division by zero")]
    DivisionByZero,
    #[error("integer overflow")]
    IntegerOverflow,
    #[error("invalid conversion to integer")]
    InvalidConversion,
    #[error("out of bounds memory access")]
    MemoryBounds,
    #[error("out of bounds table access")]
    TableBounds,
    #[error("indirect call through a null function reference")]
    NullFunction,
    #[error("indirect call signature mismatch")]
    IndirectSignature,
    #[error("instance resource limit exceeded")]
    ResourceLimit,
    #[error("host call returned values of the wrong type")]
    HostResults,
    #[error("{0}")]
    Host(String),
}

fn unsupported(offset: usize, feature: impl Into<String>) -> Error {
    Error::Unsupported {
        feature: feature.into(),
        offset,
    }
}

type Slot = u32;

#[derive(Clone, Copy)]
struct Branch {
    target: u32,
    source: Slot,
    destination: Slot,
    count: u32,
}

#[derive(Clone, Copy)]
struct MemoryOperand {
    memory: u32,
    offset: u32,
    slot: Slot,
    width: u8,
    sign: bool,
    narrow: bool,
}

#[derive(Clone, Copy)]
enum RegisterOp {
    Copy {
        from: Slot,
        to: Slot,
    },
    Const {
        to: Slot,
        bits: u64,
    },
    Unary {
        op: numeric::Unary,
        from: Slot,
        to: Slot,
    },
    Binary {
        op: numeric::Binary,
        left: Slot,
        right: Slot,
        to: Slot,
    },
    BinaryImm {
        op: numeric::Binary,
        left: Slot,
        right: u64,
        to: Slot,
    },
    Select {
        first: Slot,
        second: Slot,
        test: Slot,
    },
    Jump(Branch),
    BranchIf {
        test: Slot,
        branch: Branch,
        when: bool,
    },
    BranchTable {
        test: Slot,
        first: u32,
        count: u32,
    },
    Call {
        function: u32,
        first: Slot,
        tail: bool,
    },
    CallIndirect {
        table: u32,
        signature: u32,
        index: Slot,
        first: Slot,
        tail: bool,
    },
    Return,
    Unreachable,
    GlobalGet {
        global: u32,
        to: Slot,
    },
    GlobalSet {
        global: u32,
        from: Slot,
    },
    Load(MemoryOperand),
    Store(MemoryOperand),
    MemorySize {
        memory: u32,
        to: Slot,
    },
    MemoryGrow {
        memory: u32,
        pages: Slot,
    },
    MemoryFill {
        memory: u32,
        first: Slot,
    },
    MemoryCopy {
        destination: u32,
        source: u32,
        first: Slot,
    },
    MemoryInit {
        memory: u32,
        data: u32,
        first: Slot,
    },
    DataDrop(u32),
    TableGet {
        table: u32,
        slot: Slot,
    },
    TableSet {
        table: u32,
        first: Slot,
    },
    TableSize {
        table: u32,
        to: Slot,
    },
    TableGrow {
        table: u32,
        first: Slot,
    },
    TableFill {
        table: u32,
        first: Slot,
    },
    TableCopy {
        destination: u32,
        source: u32,
        first: Slot,
    },
    TableInit {
        table: u32,
        element: u32,
        first: Slot,
    },
    ElemDrop(u32),
}

struct CompiledFunction {
    body_offset: usize,
    locals: u32,
    slots: u32,
    instructions: Box<[RegisterOp]>,
    offsets: Box<[usize]>,
    branches: Box<[Branch]>,
}
