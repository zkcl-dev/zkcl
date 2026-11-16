use std::ops::Range;

use serde::{Deserialize, Serialize};

pub type Register = usize;

use crate::Immediate;

/// The primitive and fixed arity checked by a fixnum arithmetic fast path.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FixnumArithmetic {
    Add,
    Sub,
    Mul,
    Inc,
    Dec,
}

impl FixnumArithmetic {
    pub const fn arity(self) -> usize {
        match self {
            Self::Inc | Self::Dec => 1,
            Self::Add | Self::Sub | Self::Mul => 2,
        }
    }

    /// Index in the native primitive-binding query ABI.
    pub const fn binding_index(self) -> u8 {
        match self {
            Self::Add => 0,
            Self::Sub => 1,
            Self::Mul => 2,
            Self::Inc => 6,
            Self::Dec => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Arity {
    pub required: usize,
    pub maximum: Option<usize>,
}

impl Arity {
    pub const ZERO: Self = Self {
        required: 0,
        maximum: Some(0),
    };

    pub fn accepts(self, count: usize) -> bool {
        count >= self.required && self.maximum.is_none_or(|max| count <= max)
    }

    pub fn description(self) -> String {
        match self.maximum {
            Some(max) if max == self.required => self.required.to_string(),
            Some(max) => format!("{} to {max}", self.required),
            None => format!("at least {}", self.required),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Instruction {
    Load(Register, Immediate),
    Constant(Register, usize),
    Copy(Register, Register),
    Clear(Range<Register>),
    Jump(usize),
    Unless(Register, usize),
    IfPresent(Register, usize),
    /// Branch on any value other than a fixnum.
    UnlessFixnum(Register, usize),
    /// Branch unless the register holds a fixnum within the inclusive bounds.
    UnlessFixnumInRange(Register, i32, i32, usize),
    /// Write a tagged fixnum on success or NIL on a binding, type, or overflow
    /// miss. The caller retains the original arguments for ordinary dispatch.
    TryFixnumArithmetic {
        op: FixnumArithmetic,
        symbol: usize,
        out: Register,
        args: Range<Register>,
    },
    SetValues(Range<Register>),
    BindValues(Range<Register>),
    /// Read one result without consuming the values. A missing result is NIL.
    ReadValue(Register, usize),
    SaveValues,
    AppendValues,
    RestoreValues,
    ReadCell {
        out: Register,
        symbol: usize,
        function: bool,
        at: usize,
    },
    WriteCell {
        symbol: usize,
        src: Register,
    },
    DefineFunction {
        symbol: usize,
        definition: Register,
        is_macro: bool,
        at: usize,
    },
    ProclaimSpecial {
        symbol: usize,
        at: usize,
    },
    ProclaimInline {
        symbol: usize,
        setf: bool,
        notinline: bool,
    },
    SetVariableDocumentation {
        symbol: usize,
        doc: usize,
    },
    WhenBound {
        symbol: usize,
        jump: usize,
    },
    BindSpecial {
        symbol: usize,
        value: Register,
    },
    BindProgv {
        symbols: Register,
        values: Register,
        at: usize,
    },
    Unbind(usize),
    BoxLocal {
        reg: Register,
        at: usize,
    },
    ReadBox {
        out: Register,
        cell: CellLocation,
    },
    WriteBox {
        cell: CellLocation,
        src: Register,
    },
    Capture(Register, usize),
    Closure {
        out: Register,
        code: usize,
        cells: Range<Register>,
        at: usize,
    },
    ExitToken(Register, usize),
    EnterBlock {
        token: Register,
        resume: usize,
        clear: Register,
    },
    EnterCatch {
        tag: Register,
        resume: usize,
        clear: Register,
    },
    EnterTagbody {
        token: Register,
        jumps: usize,
        clear: Register,
    },
    LeaveTarget,
    EnterProtect {
        cleanup: usize,
        clear: Register,
    },
    BeginCleanup,
    EndCleanup,
    ReturnFrom {
        token: Register,
        at: usize,
    },
    Throw {
        tag: Register,
        at: usize,
    },
    Go {
        token: Register,
        label: usize,
        at: usize,
    },
    EnterHandlers {
        table: usize,
    },
    LeaveHandlers,
    EnterRestarts {
        table: usize,
        at: usize,
    },
    LeaveRestarts,
    AssociateRestarts {
        condition: Register,
        list: Register,
        at: usize,
    },
    LeaveRestartAssociation,
    OptionalArgument(usize, Register),
    KeywordArgument(usize, Register),
    RestArguments {
        start: usize,
        out: Register,
        at: usize,
    },
    CheckKeywords {
        start: usize,
        table: usize,
    },
    DestructureNext {
        cursor: Register,
        out: Register,
        required: bool,
        macro_call: bool,
        at: usize,
    },
    DestructureEnd {
        cursor: Register,
        macro_call: bool,
        at: usize,
    },
    DestructureKeywords {
        cursor: Register,
        table: usize,
        out: Range<Register>,
        macro_call: bool,
        at: usize,
    },
    ReleaseArguments,
    CallValues {
        callee: Register,
        at: usize,
    },
    Call {
        callee: Callee,
        out: Option<Register>,
        args: Range<Register>,
        at: usize,
        associate: bool,
        notinline: bool,
    },
}

impl Instruction {
    /// Byte offset in the decoded source text, if this instruction carries one.
    pub fn source_offset(&self) -> Option<usize> {
        match self {
            Self::ReadCell { at, .. }
            | Self::DefineFunction { at, .. }
            | Self::ProclaimSpecial { at, .. }
            | Self::BindProgv { at, .. }
            | Self::BoxLocal { at, .. }
            | Self::Closure { at, .. }
            | Self::ReturnFrom { at, .. }
            | Self::Throw { at, .. }
            | Self::Go { at, .. }
            | Self::EnterRestarts { at, .. }
            | Self::AssociateRestarts { at, .. }
            | Self::RestArguments { at, .. }
            | Self::DestructureNext { at, .. }
            | Self::DestructureEnd { at, .. }
            | Self::DestructureKeywords { at, .. }
            | Self::CallValues { at, .. }
            | Self::Call { at, .. } => Some(*at),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Handler {
    pub test: Test,
    pub target: HandlerTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum HandlerTarget {
    Function(Register),
    Case {
        pc: usize,
        condition: Register,
        clear: Register,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CellLocation {
    Local(Register),
    Capture(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Callee {
    Named(usize),
    Value(Register),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RestartTarget {
    Function(Register),
    Case { pc: usize, clear: Register },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RestartSpec {
    pub name: usize,
    pub object: Register,
    pub test: Option<Register>,
    pub interactive: Option<Register>,
    pub report: Option<Register>,
    pub target: RestartTarget,
}

macro_rules! condition_classes {
    ($($class:ident => ($name:literal, [$($parent:ident),*]),)*) => {
        /// A built-in Common Lisp condition class.
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
        #[repr(usize)]
        #[non_exhaustive]
        pub enum Class {
            $($class,)*
        }

        impl Class {
            pub fn all() -> impl Iterator<Item = Self> {
                [$(Self::$class),*].iter().copied()
            }

            pub fn named(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$class),)*
                    _ => None,
                }
            }

            /// The name of this class's symbol in the COMMON-LISP package.
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$class => $name,)*
                }
            }

            /// The built-in condition parents in local precedence order.
            pub fn direct_superclasses(self) -> &'static [Self] {
                match self {
                    $(Self::$class => &[$(Self::$parent),*],)*
                }
            }
        }
    };
}

// Declaration order determines the discriminants and the FASL encoding.
condition_classes! {
    Condition => ("CONDITION", []),
    SeriousCondition => ("SERIOUS-CONDITION", [Condition]),
    Warning => ("WARNING", [Condition]),
    Error => ("ERROR", [SeriousCondition]),
    SimpleCondition => ("SIMPLE-CONDITION", [Condition]),
    SimpleWarning => ("SIMPLE-WARNING", [SimpleCondition, Warning]),
    SimpleError => ("SIMPLE-ERROR", [SimpleCondition, Error]),
    ProgramError => ("PROGRAM-ERROR", [Error]),
    ControlError => ("CONTROL-ERROR", [Error]),
    TypeError => ("TYPE-ERROR", [Error]),
    CellError => ("CELL-ERROR", [Error]),
    UnboundVariable => ("UNBOUND-VARIABLE", [CellError]),
    UndefinedFunction => ("UNDEFINED-FUNCTION", [CellError]),
    ArithmeticError => ("ARITHMETIC-ERROR", [Error]),
    ParseError => ("PARSE-ERROR", [Error]),
    StreamError => ("STREAM-ERROR", [Error]),
    ReaderError => ("READER-ERROR", [ParseError, StreamError]),
    EndOfFile => ("END-OF-FILE", [StreamError]),
    PackageError => ("PACKAGE-ERROR", [Error]),
    FileError => ("FILE-ERROR", [Error]),
    PrintNotReadable => ("PRINT-NOT-READABLE", [Error]),
    StyleWarning => ("STYLE-WARNING", [Warning]),
    DivisionByZero => ("DIVISION-BY-ZERO", [ArithmeticError]),
    FloatingPointOverflow => ("FLOATING-POINT-OVERFLOW", [ArithmeticError]),
    FloatingPointUnderflow => ("FLOATING-POINT-UNDERFLOW", [ArithmeticError]),
    FloatingPointInvalidOperation => ("FLOATING-POINT-INVALID-OPERATION", [ArithmeticError]),
    FloatingPointInexact => ("FLOATING-POINT-INEXACT", [ArithmeticError]),
    UnboundSlot => ("UNBOUND-SLOT", [CellError]),
    SimpleTypeError => ("SIMPLE-TYPE-ERROR", [SimpleCondition, TypeError]),
    StorageCondition => ("STORAGE-CONDITION", [SeriousCondition]),
}

impl Class {
    /// Whether this class is the given class or inherits from it.
    pub fn is_a(self, expected: Self) -> bool {
        self == expected
            || self
                .direct_superclasses()
                .iter()
                .any(|parent| parent.is_a(expected))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Test {
    Any,
    Never,
    Class(Class),
    Named(usize),
}

impl Test {
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "T" => Some(Self::Any),
            "NIL" => Some(Self::Never),
            _ => Class::named(name).map(Self::Class),
        }
    }

    pub fn matches(self, class: Option<Class>) -> bool {
        match self {
            Self::Any => true,
            Self::Never | Self::Named(_) => false,
            Self::Class(expected) => class.is_some_and(|class| class.is_a(expected)),
        }
    }
}
