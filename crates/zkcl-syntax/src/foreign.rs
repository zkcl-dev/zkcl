//! Source-level names shared by ZK-FFI and the language tools.

/// Maximum number of nested C structure descriptions accepted by ZK-FFI.
pub const MAX_STRUCT_DEPTH: usize = 128;

/// A scalar type designator in ZK-FFI. Native widths are selected by the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForeignScalar {
    Int8,
    UInt8,
    Int16,
    UInt16,
    Int32,
    UInt32,
    Int64,
    UInt64,
    Char,
    Short,
    UShort,
    Int,
    UInt,
    Long,
    ULong,
    LongLong,
    ULongLong,
    IntPtr,
    UIntPtr,
    Float,
    Double,
    CString,
    Pointer,
    Void,
}

impl ForeignScalar {
    /// Parse the exact print name of a keyword.
    pub fn from_keyword(name: &str) -> Option<Self> {
        Some(match name {
            "INT8" | "SIGNED-CHAR" => Self::Int8,
            "UINT8" | "UNSIGNED-CHAR" => Self::UInt8,
            "INT16" => Self::Int16,
            "UINT16" => Self::UInt16,
            "INT32" => Self::Int32,
            "UINT32" => Self::UInt32,
            "INT64" => Self::Int64,
            "UINT64" => Self::UInt64,
            "CHAR" => Self::Char,
            "SHORT" => Self::Short,
            "UNSIGNED-SHORT" => Self::UShort,
            "INT" => Self::Int,
            "UNSIGNED-INT" => Self::UInt,
            "LONG" => Self::Long,
            "UNSIGNED-LONG" => Self::ULong,
            "LONG-LONG" => Self::LongLong,
            "UNSIGNED-LONG-LONG" => Self::ULongLong,
            "SSIZE" | "PTRDIFF" | "INTPTR" => Self::IntPtr,
            "SIZE" | "UINTPTR" => Self::UIntPtr,
            "FLOAT" => Self::Float,
            "DOUBLE" => Self::Double,
            "CSTRING" => Self::CString,
            "POINTER" => Self::Pointer,
            "VOID" => Self::Void,
            _ => return None,
        })
    }
}

pub(crate) fn signature(name: &str) -> Option<(&'static str, &'static [&'static str])> {
    macro_rules! signatures {
        ($($operator:literal => [$($parameter:literal),*]),+ $(,)?) => {
            match name {
                $($operator => Some(($operator, &[$($parameter),*])),)+
                _ => None,
            }
        };
    }
    signatures! {
        "OPEN-LIBRARY" => ["path"],
        "FIND-SYMBOL-POINTER" => ["library", "name"],
        "BIND-FUNCTION" => ["library", "name", "result-type", "&rest", "parameter-types"],
        "BIND-POINTER" => ["pointer", "result-type", "&rest", "parameter-types"],
        "BIND-VARIADIC-FUNCTION" => ["library", "name", "fixed-count", "result-type", "&rest", "parameter-types"],
        "BIND-VARIADIC-POINTER" => ["pointer", "fixed-count", "result-type", "&rest", "parameter-types"],
        "MAKE-CALLBACK" => ["function", "result-type", "&rest", "parameter-types"],
        "ALLOCATE" => ["byte-count"],
        "FREE" => ["pointer"],
        "POINTER-P" => ["pointer"],
        "POINTER-ADDRESS" => ["pointer"],
        "WITH-FOREIGN-MEMORY" => ["(name byte-count)", "&body", "body"],
        "INC-POINTER" => ["pointer", "byte-offset"],
        "MAKE-POINTER" => ["address"],
        "TYPE-SIZE" => ["type"],
        "TYPE-ALIGNMENT" => ["type"],
        "FIELD-OFFSET" => ["structure-type", "index"],
        "MEM-REF" => ["pointer", "type", "&optional", "byte-offset"],
        "MEM-SET" => ["pointer", "type", "byte-offset", "value"],
        "READ-C-STRING" => ["pointer", "&optional", "byte-count"],
        "WRITE-C-STRING" => ["pointer", "string", "&optional", "capacity"],
        "UTF8-PREFIX" => ["string", "start", "end", "byte-budget"],
        "WRITE-UTF8" => ["pointer", "string", "start", "end", "&optional", "byte-offset"],
    }
}
