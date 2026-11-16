use serde::{Deserialize, Serialize};
use zkcl_target::CompilationTarget;

use crate::verify::ValidationError;
use crate::{Arity, Handler, Instruction, RestartSpec, SimdMask, SimdPack};

// The non-UTF-8 marker keeps valid Lisp source from matching the envelope.
/// Envelope prefix identifying a zkcl compiled file.
pub const FASL_MAGIC: &[u8; 9] = b"\x89ZKCLFASL";

/// Exclusive upper bound on the number of dimensions of a zkcl array.
pub const ARRAY_RANK_LIMIT: usize = 128;

/// Static role of each closure capture slot in [`BytecodeFunction`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CaptureKind {
    /// The captured Lisp value itself.
    Value,
    /// A shared lexical cell whose contents may change.
    MutableCell,
    /// The immutable token identifying a dynamic BLOCK or TAGBODY activation.
    ExitToken,
}

/// A compiled file. Entries execute in file order and share literal nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledFile {
    /// Target whose reader conditionals and compile-time forms selected this code.
    #[serde(with = "target_identity")]
    pub target: CompilationTarget,
    pub source_files: Vec<CompiledSource>,
    pub nodes: Vec<LiteralNode>,
    pub entries: Vec<BytecodeFunction>,
}

/// One decoded source revision shared by all of its compiled functions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledSource {
    pub uri: Option<String>,
    pub version: Option<i64>,
    pub base_offset: usize,
    pub path: Option<String>,
    pub text: Option<String>,
}

impl From<&str> for CompiledSource {
    fn from(path: &str) -> Self {
        Self {
            uri: None,
            version: None,
            base_offset: 0,
            path: Some(path.into()),
            text: None,
        }
    }
}

mod target_identity {
    use serde::{Deserialize, Deserializer, Serializer};
    use zkcl_target::CompilationTarget;

    pub(super) fn serialize<S: Serializer>(
        target: &CompilationTarget,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(target)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<CompilationTarget, D::Error> {
        let triple = String::deserialize(deserializer)?;
        triple.parse().map_err(serde::de::Error::custom)
    }
}

/// Structured condition-handler table referenced by [`Instruction::EnterHandlers`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerTable {
    pub handlers: Vec<Handler>,
}

/// Structured restart table referenced by [`Instruction::EnterRestarts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartTable {
    pub restarts: Vec<RestartSpec>,
}

/// Structured keyword-argument descriptor referenced by
/// [`Instruction::CheckKeywords`] and [`Instruction::DestructureKeywords`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordTable {
    pub names: Vec<usize>,
    pub allow_other_keys: bool,
    pub override_key: usize,
}

/// Structured jump table referenced by [`Instruction::EnterTagbody`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JumpTable {
    pub targets: Vec<usize>,
}

/// Locations used to examine a lexical variable in a suspended bytecode frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugVariableLocation {
    Register { index: usize, boxed: bool },
    Capture(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebugVariable {
    /// Index of the lexical name in this function's constants.
    pub name: u32,
    /// Canonical instruction range over which the binding is available.
    pub live: std::ops::Range<usize>,
    pub location: DebugVariableLocation,
}

/// The namespace occupied by a runtime definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DefinitionKind {
    Function,
    Macro,
    Method,
    Class,
    Variable,
    Constant,
    GenericFunction,
}

/// Origin of a top-level definition, recorded before macro expansion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionDebug {
    pub package: String,
    pub name: String,
    pub kind: DefinitionKind,
    pub at: usize,
    pub range: Option<std::ops::Range<usize>>,
    pub lambda_list: Option<String>,
}

/// Whether a location names original syntax or the editable input that
/// produced generated syntax. It never claims unique identity for Lisp objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceOrigin {
    Original,
    Reader,
    Macro,
    Compiler,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    /// Source-bearing bytecode operand used to find this occurrence.
    pub at: usize,
    /// Exact original range, or the generating call's range for generated code.
    pub range: Option<std::ops::Range<usize>>,
    pub origin: SourceOrigin,
    /// Outermost to innermost macro calls that produced this occurrence.
    pub expansion: Vec<std::ops::Range<usize>>,
}

impl Default for SourceLocation {
    fn default() -> Self {
        Self {
            at: 0,
            range: None,
            origin: SourceOrigin::Compiler,
            expansion: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionDebug {
    /// Used source occurrences, sorted by their source-bearing operand.
    pub sources: Vec<SourceLocation>,
    pub definitions: Vec<DefinitionDebug>,
    /// The name denotes a global binding rather than a local FLET/LABELS definition.
    pub global_function: bool,
    /// Package active while the function was read, as a package constant index.
    pub reader_package: Option<u32>,
    /// Actual compiled lambda syntax, as an index in the function constants.
    pub source_expression: Option<u32>,
    /// The original function name symbol, as an index in the constants.
    pub function_name: Option<u32>,
    pub entry: Option<usize>,
    pub retain_frame: bool,
    pub variables: Vec<DebugVariable>,
}

impl FunctionDebug {
    pub fn source_at(&self, at: usize) -> Option<&SourceLocation> {
        self.sources
            .binary_search_by_key(&at, |source| source.at)
            .ok()
            .map(|index| &self.sources[index])
    }
}

/// Static type guarantee for a required entry parameter derived from trusted
/// declarations under zero safety.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum ParamTypeFact {
    #[default]
    Any,
    Fixnum,
    Float,
    Char,
    Cons,
    Symbol,
    Function,
    Nil,
}

/// Static declaration facts recorded for one required bytecode parameter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ParamFact {
    pub ty: ParamTypeFact,
    pub dynamic_extent: bool,
}

/// Static compiler-proved facts carried alongside a compiled function for
/// middle-end SSA lifting and native optimization.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FunctionFacts {
    pub params: Vec<ParamFact>,
}

/// A function body and its bytecode metadata, with constants indexed by the caller.
/// In a [`CompiledFile`], those indices refer to the shared literal table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytecodeFunction {
    pub name: Option<String>,
    /// Index in the compiled file's source path table. Instruction offsets refer to this file's decoded text.
    pub source_file: Option<u32>,
    pub debug: FunctionDebug,
    pub arity: Arity,
    pub registers: usize,
    pub instructions: Vec<Instruction>,
    pub constants: Vec<u32>,
    /// The source lambda list, when available, as an index in `constants`.
    pub lambda_list: Option<u32>,
    pub captures: Vec<CaptureKind>,
    pub notinline: Vec<u32>,
    /// Optional native replacements for ordinary named calls in this function.
    pub vop_calls: Vec<crate::VopCall>,
    pub handlers: Vec<HandlerTable>,
    pub restarts: Vec<RestartTable>,
    pub keywords: Vec<KeywordTable>,
    pub jump_tables: Vec<JumpTable>,
    pub facts: FunctionFacts,
}

/// The instructions and frame metadata shared by compiled files and JIT snapshots.
/// Constant operands name slots in the original function's constant pool.
#[derive(Clone, Copy)]
pub struct BytecodeBody<'a> {
    pub arity: Arity,
    pub registers: usize,
    pub instructions: &'a [Instruction],
    pub constant_count: usize,
    pub captures: &'a [CaptureKind],
    pub notinline: &'a [u32],
    pub vop_calls: &'a [crate::VopCall],
    pub handlers: &'a [HandlerTable],
    pub restarts: &'a [RestartTable],
    pub keywords: &'a [KeywordTable],
    pub jump_tables: &'a [JumpTable],
    pub param_facts: &'a [ParamFact],
}

impl BytecodeFunction {
    pub fn body(&self) -> BytecodeBody<'_> {
        BytecodeBody {
            arity: self.arity,
            registers: self.registers,
            instructions: &self.instructions,
            constant_count: self.constants.len(),
            captures: &self.captures,
            notinline: &self.notinline,
            vop_calls: &self.vop_calls,
            handlers: &self.handlers,
            restarts: &self.restarts,
            keywords: &self.keywords,
            jump_tables: &self.jump_tables,
            param_facts: &self.facts.params,
        }
    }
}

/// A node in the file's literal graph, including nested function templates.
/// References are indices into [`CompiledFile::nodes`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LiteralNode {
    /// Runtime image-only marker for an unbound cell.
    Unbound,
    /// Typed runtime object. The runtime validates the shape before allocation.
    RuntimeObject {
        kind: String,
        data: Vec<u64>,
        slots: Vec<u32>,
    },
    /// Host resource reconstructed by an explicitly supplied image hook.
    ExternalResource {
        key: String,
        data: Vec<u8>,
    },
    Nil,
    True,
    Fixnum(i64),
    Character(char),
    String(String),
    InternedSymbol {
        package: String,
        name: String,
    },
    UninternedSymbol(String),
    Pair(u32, u32),
    Package(String),
    Pathname([u32; 6]),
    Primitive(String),
    BuiltinNative(u32),
    Function(BytecodeFunction),
    Closure {
        code: u32,
        captures: Vec<u32>,
        documentation: u32,
    },
    LexicalCell(u32),
    HashTable {
        test: HashTableTest,
        weakness: Option<HashTableWeakness>,
        rehash_size: RehashHint,
        rehash_threshold: RehashHint,
        entries: Vec<(u32, u32)>,
    },
    /// Entry whose primary value supplies this literal, and any subsequent
    /// entry which finishes initializing it. Both run in ordinary file order.
    LoadForm {
        creation: usize,
        initialization: Option<usize>,
    },
    Bignum {
        negative: bool,
        /// Unsigned magnitude, least significant byte first.
        magnitude: Vec<u8>,
    },
    Ratio(u32, u32),
    SingleFloat(u32),
    DoubleFloat(u64),
    Complex(u32, u32),
    Array {
        dimensions: Vec<usize>,
        element: ArrayElement,
        adjustable: bool,
        fill_pointer: Option<usize>,
        contents: ArrayContents,
    },
    RandomState([u64; 4]),
    SimdPack(SimdPack),
    SimdMask(SimdMask),
}

/// The upgraded element type of an array literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArrayElement {
    General,
    Bit,
    Character,
    Octet,
    SingleFloat,
    DoubleFloat,
}

/// Contents in row-major order or a displacement into another array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArrayContents {
    Elements(Vec<u32>),
    Octets(Vec<u8>),
    /// IEEE 754 binary32 element bits, stored inline in the literal graph.
    SingleFloats(Vec<u32>),
    /// IEEE 754 binary64 element bits, stored inline in the literal graph.
    DoubleFloats(Vec<u64>),
    Displaced {
        target: u32,
        offset: usize,
    },
}

/// The equality test of a hash-table literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashTableTest {
    Eq,
    Eql,
    Equal,
    Equalp,
}

/// A nonnegative integer or binary64 rehash hint, shared by heap and file.
/// The otherwise unused sign bit distinguishes a float from an integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct RehashHint(u64);

impl RehashHint {
    const FLOAT_TAG: u64 = 1 << 63;

    /// Larger additive steps already exceed the addressable table size.
    pub fn integer(value: u64) -> Self {
        Self(value.min(Self::FLOAT_TAG - 1))
    }

    pub fn float(value: f64) -> Option<Self> {
        (value.is_finite() && value >= 0.0).then(|| Self(Self::FLOAT_TAG | value.to_bits()))
    }

    pub fn as_integer(self) -> Option<u64> {
        (self.0 & Self::FLOAT_TAG == 0).then_some(self.0)
    }

    pub fn as_f64(self) -> f64 {
        match self.as_integer() {
            Some(value) => value as f64,
            None => f64::from_bits(self.0 & !Self::FLOAT_TAG),
        }
    }

    pub fn valid_size(self) -> bool {
        match self.as_integer() {
            Some(step) => step > 0,
            None => {
                let scale = self.as_f64();
                scale.is_finite() && scale > 1.0
            }
        }
    }

    pub fn valid_threshold(self) -> bool {
        (0.0..=1.0).contains(&self.as_f64())
    }
}

/// The reachability condition for keeping a weak hash-table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashTableWeakness {
    Key,
    Value,
    KeyOrValue,
    KeyAndValue,
}

/// An error in a compiled file's binary envelope, data, or bytecode.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CompiledFileError {
    #[error("invalid zkcl compiled-file magic")]
    InvalidMagic,
    #[error("could not encode zkcl compiled-file payload: {0}")]
    Encode(#[source] postcard::Error),
    #[error("invalid zkcl compiled-file payload: {0}")]
    Decode(#[source] postcard::Error),
    #[error("zkcl compiled file has {0} trailing bytes")]
    TrailingBytes(usize),
    #[error(
        "FASL compiled for {compiled} is incompatible with requested target {requested}. Recompile the Lisp source for {requested}."
    )]
    TargetMismatch { compiled: String, requested: String },
    #[error("{0}")]
    Validation(#[from] ValidationError),
}

impl CompiledFile {
    /// Remove optional source and debugger metadata from every code object.
    ///
    /// Literal identity, canonical instructions, and loading order are retained.
    pub fn strip_debug(&mut self) {
        self.source_files.clear();
        for function in self
            .entries
            .iter_mut()
            .chain(self.nodes.iter_mut().filter_map(|node| match node {
                LiteralNode::Function(function) => Some(function),
                _ => None,
            }))
        {
            function.name = None;
            function.source_file = None;
            function.debug = FunctionDebug::default();
        }
    }

    /// Check the target before executing the file or lowering it to native code.
    pub fn check_target(&self, target: &CompilationTarget) -> Result<(), CompiledFileError> {
        if self.target.is_compatible_with(target) {
            return Ok(());
        }
        Err(CompiledFileError::TargetMismatch {
            compiled: self.target.to_string(),
            requested: target.to_string(),
        })
    }

    /// Decode a FASL for a runtime and check its compilation target before loading.
    pub fn decode_for_target(
        bytes: &[u8],
        target: &CompilationTarget,
    ) -> Result<Self, CompiledFileError> {
        let file = Self::decode(bytes)?;
        file.check_target(target)?;
        Ok(file)
    }

    /// Identify the compiled-file envelope by its magic header.
    pub fn has_magic(bytes: &[u8]) -> bool {
        bytes.starts_with(FASL_MAGIC)
    }

    /// Validate and serialize executable bytecode and its literal nodes.
    pub fn encode(&self) -> Result<Vec<u8>, CompiledFileError> {
        self.validate()?;
        postcard::to_extend(self, FASL_MAGIC.to_vec()).map_err(CompiledFileError::Encode)
    }

    /// Read one whole compiled file and validate its bytecode and literal graph.
    pub fn decode(bytes: &[u8]) -> Result<Self, CompiledFileError> {
        let payload = bytes
            .strip_prefix(FASL_MAGIC)
            .ok_or(CompiledFileError::InvalidMagic)?;
        let (file, trailing): (Self, _) =
            postcard::take_from_bytes(payload).map_err(CompiledFileError::Decode)?;
        if !trailing.is_empty() {
            return Err(CompiledFileError::TrailingBytes(trailing.len()));
        }
        file.validate()?;
        Ok(file)
    }
}
