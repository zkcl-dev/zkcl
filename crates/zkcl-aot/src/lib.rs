//! Offline AOT compiler and relocatable object file emitter for zkcl.
//!
//! Compiles verified [`CompiledFile`] bytecode using a selected emitter.
//! Cranelift and LLVM optimize SSA and share the same GC root and unwind layout.
//!
//! - direct machine-code emission for supported targets and functions,
//! - a Mach-O / ELF / COFF relocatable `.o` via the default Cranelift backend, or
//! - an LLVM module and relocatable object via Inkwell with the `llvm` feature.
//!
//! Alongside machine code, `zkcl-aot` embeds the serialized `CompiledFile` FASL
//! bytes in a read-only data section and generates Rust registration source
//! (`AotModuleDescriptor` + `AotCodeDescriptor` array) and a standalone `main.rs`
//! template for `zkcl build`.

#[cfg(feature = "cranelift")]
pub mod cranelift_object;
mod direct_object;
#[cfg(feature = "cranelift")]
mod function_cache;
#[cfg(feature = "llvm")]
mod llvm_ir;
mod object_file;
pub mod rust_source;
#[cfg(any(feature = "llvm", feature = "cranelift"))]
mod ssa;

use std::num::NonZeroUsize;
use std::path::Path;

use target_lexicon::{DeploymentTarget, Triple};
use thiserror::Error;
use zkcl_bytecode::{Arity, CompiledFile, CompiledFileError};
use zkcl_target::CompilationTarget;

const DEFAULT_MACHO_DEPLOYMENT_TARGET: DeploymentTarget = DeploymentTarget {
    major: 14,
    minor: 0,
    patch: 0,
};

/// Code generation backend selected for offline AOT compilation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AotBackendKind {
    /// Emit native instructions directly for supported functions.
    Direct,
    /// Lower directly via `cranelift-codegen` and emit `.o` bytes with `object`.
    CraneliftObject,
    /// Build an LLVM module and emit a relocatable object in memory.
    Llvm,
}

impl AotBackendKind {
    /// Whether this build includes the selected code generator.
    pub const fn is_available(self) -> bool {
        match self {
            Self::Direct => true,
            Self::CraneliftObject => cfg!(feature = "cranelift"),
            Self::Llvm => cfg!(feature = "llvm"),
        }
    }
}

/// Optimization level requested from the AOT backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AotOptLevel {
    /// Disable backend optimizations (`none` / `-O0`).
    None,
    /// Optimize for execution speed (`speed` / `-O2`).
    #[default]
    Speed,
    /// Aggressive speed optimizations (`speed` / `-O3`).
    Aggressive,
    /// Optimize for code size and speed (`speed_and_size` / `-Os`).
    Size,
}

/// Target and backend configuration for an AOT compilation unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AotTargetConfig {
    /// Logical module identifier recorded in `AotModuleDescriptor::module_id`.
    pub module_id: String,
    /// Target triple for machine code generation and object emission.
    pub target: Triple,
    /// Optimization level for native code generation.
    pub opt_level: AotOptLevel,
    /// Selected native code generation backend.
    pub backend: AotBackendKind,
    /// Maximum number of native compilation jobs, including the caller.
    pub jobs: Option<NonZeroUsize>,
}

impl AotTargetConfig {
    /// Resolve the requested or default concurrency limit.
    pub fn worker_limit(&self) -> NonZeroUsize {
        self.jobs.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .unwrap_or(NonZeroUsize::MIN)
                .min(NonZeroUsize::new(8).unwrap())
        })
    }

    /// Target the host with Cranelift when enabled, then LLVM, then the direct emitter.
    pub fn host(module_id: impl Into<String>) -> Self {
        Self {
            module_id: module_id.into(),
            target: Triple::host(),
            opt_level: AotOptLevel::Speed,
            backend: if cfg!(feature = "cranelift") {
                AotBackendKind::CraneliftObject
            } else if cfg!(feature = "llvm") {
                AotBackendKind::Llvm
            } else {
                AotBackendKind::Direct
            },
            jobs: None,
        }
    }

    /// Override the target triple.
    pub fn with_target(mut self, target: Triple) -> Self {
        self.target = target;
        self
    }

    /// Override the optimization level.
    pub fn with_opt_level(mut self, opt_level: AotOptLevel) -> Self {
        self.opt_level = opt_level;
        self
    }

    /// Override the code generation backend.
    pub fn with_backend(mut self, backend: AotBackendKind) -> Self {
        self.backend = backend;
        self
    }

    /// Identify target features resolved by the backend beyond the target triple.
    ///
    /// Include this in artifact cache keys: Cranelift may enable host CPU
    /// features for the native triple, so the triple alone is insufficient.
    pub fn codegen_target_identity(&self) -> Result<Option<String>, AotError> {
        if self.backend == AotBackendKind::CraneliftObject {
            #[cfg(feature = "cranelift")]
            return cranelift_object::codegen_target_identity(self).map(Some);
            #[cfg(not(feature = "cranelift"))]
            return Err(AotError::CraneliftUnavailable);
        }
        Ok(None)
    }
}

/// Symbol and calling information required to register one AOT-compiled function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AotRegistrationEntry {
    /// The generated entry preserves canonical native debug safepoints.
    pub native_debug: bool,
    /// Index assigned by the compiled-file loader to this code entry.
    pub entry_index: u32,
    /// Exported C-ABI symbol name (`zkcl_aot_<sanitized_module>_fn_<index>`).
    pub symbol_name: String,
    /// Lisp calling arity of this code object.
    pub arity: Arity,
    /// Total `RootStack` slots required by this frame (`0` if leaf / no safepoints).
    pub root_slots: usize,
    /// Optional direct entry accepting four raw words and returning one word.
    pub leaf_symbol: Option<String>,
    /// Preconditions checked before transferring control to the direct entry.
    pub leaf_guards: Vec<zkcl_bytecode::native_abi::LeafGuard>,
}

/// Complete output of offline AOT compilation for one [`CompiledFile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AotObject {
    /// Relocatable `.o` bytes (Mach-O, ELF, or COFF) with compiled symbols and FASL `.rodata`.
    pub object_bytes: Vec<u8>,
    /// LLVM IR before backend optimization when using [`AotBackendKind::Llvm`].
    pub llvm_ir: Option<String>,
    /// Validated serialized `CompiledFile` FASL payload.
    pub fasl_bytes: Vec<u8>,
    /// Number of top-level and nested code entries in the embedded FASL.
    pub code_entry_count: usize,
    /// Registration entries sorted by compiled-file code index.
    pub registration_entries: Vec<AotRegistrationEntry>,
    /// Selection diagnostics for Direct entries that execute as bytecode.
    pub direct_rejections: Vec<(u32, zkcl_vop::SelectionError)>,
    /// Generated Rust `AotModuleDescriptor` + `AotCodeDescriptor` registration source.
    pub registration_source: String,
    /// Generated standalone `main.rs` template for `zkcl build` executables.
    pub standalone_main_source: String,
}

/// Errors returned during AOT compilation or object file emission.
#[derive(Debug, Error)]
pub enum AotError {
    #[error("compiled file validation/encoding failed: {0}")]
    CompiledFile(#[from] CompiledFileError),
    #[cfg(any(feature = "llvm", feature = "cranelift"))]
    #[error("bytecode lifting failed for entry {entry_index}: {source}")]
    Lift {
        entry_index: u32,
        #[source]
        source: zkcl_opt::BytecodeLiftError,
    },
    #[error("unsupported target triple `{target}`: {reason}")]
    UnsupportedTarget { target: String, reason: String },
    #[cfg(feature = "cranelift")]
    #[error("Cranelift SSA lowering failed for `{symbol}`: {source}")]
    CraneliftLowering {
        symbol: String,
        #[source]
        source: zkcl_cranelift::LoweringError,
    },
    #[cfg(feature = "cranelift")]
    #[error("Cranelift code generation failed for `{symbol}`: {source:?}")]
    CraneliftCodegen {
        symbol: String,
        #[source]
        source: cranelift_codegen::CodegenError,
    },
    #[error("LLVM IR generation failed for `{symbol}`: {reason}")]
    LlvmLowering { symbol: String, reason: String },
    #[error("object file emission failed: {0}")]
    ObjectWrite(#[from] object::write::Error),
    #[error("object file emission failed: {0}")]
    InvalidRelocation(String),
    #[error("too many code entries in CompiledFile")]
    TooManyCodeEntries,
    #[error("direct code emission failed for entry {entry_index} on {target}")]
    DirectEmission { entry_index: u32, target: String },
    #[error("immediate fixnum {0} is outside tagged fixnum range")]
    InvalidFixnum(i64),
    #[error("rebuild with the `cranelift` Cargo feature to enable Cranelift AOT")]
    CraneliftUnavailable,
    #[error("rebuild with the `llvm` Cargo feature to enable LLVM AOT")]
    LlvmUnavailable,
    #[cfg(feature = "llvm")]
    #[error("LLVM code generation failed: {}", .0.to_string_lossy())]
    Llvm(#[source] inkwell::support::LLVMString),
    #[cfg(feature = "llvm")]
    #[error("LLVM code generation failed: {0}")]
    LlvmBuilder(#[from] inkwell::builder::BuilderError),
    #[error("I/O error during AOT compilation: {0}")]
    Io(#[from] std::io::Error),
}

/// Sanitize a logical `module_id` into a safe ASCII C identifier component.
pub fn sanitize_module_id(module_id: &str) -> String {
    let mut out = String::with_capacity(module_id.len());
    for ch in module_id.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "module".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Symbol prefix shared by every function entry of a module.
fn function_symbol_prefix(module_id: &str) -> String {
    format!("zkcl_aot_{}_fn_", sanitize_module_id(module_id))
}

/// Compute the C-ABI symbol name for an AOT-compiled function entry.
pub fn function_symbol_name(module_id: &str, entry_index: u32) -> String {
    format!("{}{entry_index}", function_symbol_prefix(module_id))
}

/// Compute the read-only data symbol name for the embedded FASL payload.
pub fn fasl_symbol_name(module_id: &str) -> String {
    format!("zkcl_aot_{}_fasl", sanitize_module_id(module_id))
}

/// Compile all top-level and nested [`BytecodeFunction`](zkcl_bytecode::BytecodeFunction) objects in `file` into an
/// [`AotObject`].
pub fn compile_fasl_to_object(
    file: &CompiledFile,
    config: &AotTargetConfig,
) -> Result<AotObject, AotError> {
    emit_aot_object(file, file.encode()?, config, None)
}

/// Compile a serialized FASL file and embed the validated bytes in the object.
pub fn compile_fasl_bytes_to_object(
    fasl_bytes: Vec<u8>,
    config: &AotTargetConfig,
) -> Result<AotObject, AotError> {
    let file = CompiledFile::decode(&fasl_bytes)?;
    emit_aot_object(&file, fasl_bytes, config, None)
}

/// Compile a serialized FASL with reuse of independent Cranelift code emission.
///
/// Use a cache directory specific to the current toolchain and native runtime
/// ABI. Cache errors are treated as misses. The current FASL and its native
/// registrations are assembled on every call, including on function hits.
pub fn compile_fasl_bytes_to_object_with_function_cache(
    fasl_bytes: Vec<u8>,
    config: &AotTargetConfig,
    function_cache: &Path,
) -> Result<AotObject, AotError> {
    let file = CompiledFile::decode(&fasl_bytes)?;
    emit_aot_object(&file, fasl_bytes, config, Some(function_cache))
}

fn emit_aot_object(
    file: &CompiledFile,
    fasl_bytes: Vec<u8>,
    config: &AotTargetConfig,
    function_cache: Option<&Path>,
) -> Result<AotObject, AotError> {
    #[cfg(not(feature = "cranelift"))]
    let _ = function_cache;
    let target = CompilationTarget::from_triple(config.target.clone()).map_err(|error| {
        AotError::UnsupportedTarget {
            target: config.target.to_string(),
            reason: error.to_string(),
        }
    })?;
    file.check_target(&target)?;
    let codes: Vec<_> = file.code_entries().collect();
    let prefix = function_symbol_prefix(&config.module_id);
    let code_entry_count = codes.len();
    let mut direct_rejections = Vec::new();

    let (object_bytes, registration_entries, llvm_ir_text) = match config.backend {
        AotBackendKind::Direct => {
            let (object, entries) = direct_object::emit_object(
                file,
                &codes,
                &prefix,
                &fasl_bytes,
                config,
                &mut direct_rejections,
            )?;
            (object, entries, None)
        }
        #[cfg(feature = "cranelift")]
        AotBackendKind::CraneliftObject => {
            let (object, registration_entries) = cranelift_object::emit_object(
                file,
                &codes,
                &prefix,
                &fasl_bytes,
                config,
                function_cache,
            )?;
            (object, registration_entries, None)
        }
        #[cfg(not(feature = "cranelift"))]
        AotBackendKind::CraneliftObject => return Err(AotError::CraneliftUnavailable),
        #[cfg(feature = "llvm")]
        AotBackendKind::Llvm => {
            let object = llvm_ir::emit_object(file, &codes, &prefix, &fasl_bytes, config, &target)?;
            (object.bytes, object.registrations, Some(object.ir))
        }
        #[cfg(not(feature = "llvm"))]
        AotBackendKind::Llvm => return Err(AotError::LlvmUnavailable),
    };

    let registration_source = rust_source::generate_registration_source(
        &config.module_id,
        &registration_entries,
        &fasl_bytes,
    );
    let standalone_main_source =
        rust_source::generate_standalone_main_source(&config.module_id, None);

    Ok(AotObject {
        object_bytes,
        llvm_ir: llvm_ir_text,
        fasl_bytes,
        code_entry_count,
        registration_entries,
        direct_rejections,
        registration_source,
        standalone_main_source,
    })
}
