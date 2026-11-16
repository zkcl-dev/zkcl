//! Cranelift code generation and `object` relocatable `.o` emission via `zkcl-cranelift`.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Mutex;
use std::thread;

use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::incremental_cache::compute_cache_key;
use cranelift_codegen::ir::{
    ExtFuncData, ExternalName, FuncRef, Function, Inst, InstBuilder, UserExternalName,
    UserFuncName, Value,
};
use cranelift_codegen::isa::{self, OwnedTargetIsa, TargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, FinalizedRelocTarget};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use object::write::{
    Object, Relocation, SectionId, StandardSection, Symbol, SymbolId, SymbolSection,
};
use object::{
    BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SymbolFlags, SymbolKind,
    SymbolScope,
};
use rustc_hash::FxHashMap;
use target_lexicon::Triple;
use zkcl_bytecode::{BytecodeFunction, CompiledFile};
use zkcl_cranelift::{
    LoweringConfig, RuntimeAbiFunction, RuntimeCallEmitter, lower_ssa_function_into,
};

use crate::{
    AotError, AotOptLevel, AotRegistrationEntry, AotTargetConfig,
    function_cache::FunctionCache,
    object_file::{add_text_symbol, create_object},
    ssa::{optimize_native_entry, register_native_entry},
};

struct ExternalRuntimeCallEmitter {
    ptr_ty: cranelift_codegen::ir::Type,
    call_conv: cranelift_codegen::isa::CallConv,
    colocated: bool,
    runtime_function_refs: [Option<FuncRef>; RuntimeAbiFunction::ALL.len()],
}

impl RuntimeCallEmitter for ExternalRuntimeCallEmitter {
    fn emit_runtime_call(
        &mut self,
        builder: &mut FunctionBuilder<'_>,
        runtime_function: RuntimeAbiFunction,
        args: &[Value],
    ) -> Result<Inst, zkcl_cranelift::LoweringError> {
        let func_ref = *self.runtime_function_refs[runtime_function as usize]
            .get_or_insert_with(|| {
                let sig = zkcl_cranelift::abi::runtime_call_signature(
                    runtime_function,
                    self.ptr_ty,
                    self.call_conv,
                );
                let sig_ref = builder.import_signature(sig);
                let external_name = builder
                    .func
                    .declare_imported_user_function(UserExternalName {
                        namespace: 1,
                        index: runtime_function as u32,
                    });
                builder.import_function(ExtFuncData {
                    name: ExternalName::User(external_name),
                    signature: sig_ref,
                    colocated: self.colocated,
                })
            });
        Ok(builder.ins().call(func_ref, args))
    }
}

struct RelocatableFunction {
    registration_entry: AotRegistrationEntry,
    bytes: Vec<u8>,
    alignment: u64,
    relocations: Vec<CodeRelocation>,
}

struct CodeRelocation {
    offset: u64,
    kind: Reloc,
    addend: i64,
    target: RelocationTarget,
}

enum RelocationTarget {
    External(Cow<'static, str>),
    FunctionOffset(u32),
}

/// Emit a relocatable object with native functions and the serialized FASL.
pub(crate) fn emit_object(
    file: &CompiledFile,
    codes: &[&BytecodeFunction],
    prefix: &str,
    fasl_bytes: &[u8],
    config: &AotTargetConfig,
    function_cache: Option<&Path>,
) -> Result<(Vec<u8>, Vec<AotRegistrationEntry>), AotError> {
    let isa = build_target_isa(&config.target, config.opt_level)?;
    let mut object = create_object(
        config,
        file.target
            .machine()
            .expect("object compilation has a machine ABI")
            .environment(),
        fasl_bytes,
    )?;
    if let object::FileFlags::Elf { e_flags, .. } = &mut object.flags
        && matches!(
            isa.triple().architecture,
            target_lexicon::Architecture::Riscv64(_)
        )
        && isa
            .isa_flags()
            .iter()
            .any(|flag| matches!(flag.name, "has_zca" | "has_zcd") && flag.as_bool() == Some(true))
    {
        *e_flags |= object::elf::EF_RISCV_RVC;
    }
    let text = object.section_id(StandardSection::Text);

    let cache = function_cache.and_then(FunctionCache::open);
    let functions = compile_functions(
        &*isa,
        file,
        codes,
        prefix,
        config.worker_limit().get(),
        cache,
    )?;
    let mut registration_entries = Vec::with_capacity(functions.len());
    let mut symbols = FxHashMap::default();
    for function in functions {
        append_function_to_text(&mut object, text, &function, &mut symbols)?;
        registration_entries.push(function.registration_entry);
    }
    let bytes = object.write()?;
    Ok((bytes, registration_entries))
}

fn compile_functions(
    isa: &dyn TargetIsa,
    file: &CompiledFile,
    codes: &[&BytecodeFunction],
    prefix: &str,
    jobs: usize,
    cache: Option<FunctionCache<'_>>,
) -> Result<Vec<RelocatableFunction>, AotError> {
    // Leave small files on the caller and give workers enough functions to reuse their buffers.
    let jobs = jobs.min(codes.len().div_ceil(24));
    if jobs <= 1 {
        let mut compile = function_compiler(isa, file, prefix, cache);
        return codes
            .iter()
            .enumerate()
            .filter_map(|(index, &code)| compile(index, code).transpose())
            .collect();
    }
    // Workers keep their Cranelift contexts and claim the next function when
    // ready. An installer's bytecode length need not predict its native cost.
    let mut results: Vec<_> = (0..codes.len()).map(|_| Ok(None)).collect();
    let queue = Mutex::new(codes.iter().copied().zip(&mut results).enumerate());
    let work = || {
        let mut compile = function_compiler(isa, file, prefix, cache);
        let entries = std::iter::from_fn(|| queue.lock().unwrap().next());
        for (index, (code, result)) in entries {
            *result = compile(index, code);
            if result.is_err() {
                break;
            }
        }
    };
    thread::scope(|scope| -> std::io::Result<()> {
        for _ in 1..jobs {
            thread::Builder::new().spawn_scoped(scope, work)?;
        }
        work();
        Ok(())
    })?;
    // Results and errors retain file order regardless of completion order.
    results.into_iter().filter_map(Result::transpose).collect()
}

fn function_compiler<'a>(
    isa: &'a dyn TargetIsa,
    file: &'a CompiledFile,
    prefix: &'a str,
    mut cache: Option<FunctionCache<'a>>,
) -> impl FnMut(usize, &BytecodeFunction) -> Result<Option<RelocatableFunction>, AotError> + 'a {
    let mut builder_context = FunctionBuilderContext::new();
    let mut codegen_context = Context::new();
    let mut control_plane = ControlPlane::default();
    let mut runtime_calls = ExternalRuntimeCallEmitter {
        ptr_ty: isa.pointer_type(),
        call_conv: isa.default_call_conv(),
        // COFF linkers resolve direct calls to the runtime or an import thunk.
        // An indirect ARM64 GOT load would instead need a pointer slot.
        colocated: isa.triple().binary_format == target_lexicon::BinaryFormat::Coff,
        runtime_function_refs: [None; RuntimeAbiFunction::ALL.len()],
    };
    move |index, code| {
        let (entry_index, ssa) = optimize_native_entry(file, code, index, None)?;
        let Some(lowering_config) = LoweringConfig::new(&ssa, UserFuncName::user(0, entry_index))
        else {
            return Ok(None);
        };
        let registration_entry =
            register_native_entry(prefix, entry_index, code, lowering_config.root_slots());
        codegen_context.clear();
        runtime_calls.runtime_function_refs.fill(None);
        lower_ssa_function_into(
            &lowering_config,
            isa,
            &mut builder_context,
            &mut runtime_calls,
            &mut codegen_context.func,
        )
        .map_err(|source| AotError::CraneliftLowering {
            symbol: registration_entry.symbol_name.clone(),
            source,
        })?;
        // The stencil depends on the lowered operations and effective ISA.
        // Resolve external references and the registration against this call's
        // function; cached machine code does not carry the old FASL indices.
        match cache.as_mut() {
            Some(cache) => codegen_context
                .compile_with_cache(isa, cache, &mut control_plane)
                .map(|(compiled, _hit)| compiled),
            None => codegen_context.compile(isa, &mut control_plane),
        }
        .map_err(|err| AotError::CraneliftCodegen {
            symbol: registration_entry.symbol_name.clone(),
            source: err.inner,
        })?;
        let compiled = codegen_context.compiled_code().expect("compiled above");
        let names = codegen_context.func.params.user_named_funcs();
        let relocations = compiled
            .buffer
            .relocs()
            .iter()
            .map(|relocation| {
                let target = match &relocation.target {
                    FinalizedRelocTarget::ExternalName(ExternalName::User(name)) => {
                        let index = names[*name].index;
                        let runtime_function =
                            RuntimeAbiFunction::ALL.get(index as usize).ok_or_else(|| {
                                AotError::InvalidRelocation(format!(
                                    "unknown external runtime function index {index}"
                                ))
                            })?;
                        RelocationTarget::External(runtime_function.symbol_name().into())
                    }
                    FinalizedRelocTarget::ExternalName(other) => {
                        RelocationTarget::External(format!("{other:?}").into())
                    }
                    FinalizedRelocTarget::Func(offset) => RelocationTarget::FunctionOffset(*offset),
                };
                Ok(CodeRelocation {
                    offset: u64::from(relocation.offset),
                    kind: relocation.kind,
                    addend: relocation.addend,
                    target,
                })
            })
            .collect::<Result<_, AotError>>()?;
        Ok(Some(RelocatableFunction {
            registration_entry,
            bytes: compiled.code_buffer().to_vec(),
            alignment: u64::from(compiled.buffer.alignment).max(16),
            relocations,
        }))
    }
}

fn append_function_to_text(
    object: &mut Object<'_>,
    text: SectionId,
    code: &RelocatableFunction,
    symbols: &mut FxHashMap<Cow<'static, str>, SymbolId>,
) -> Result<(), AotError> {
    let symbol = add_text_symbol(
        object,
        &code.registration_entry.symbol_name,
        SymbolScope::Linkage,
    );
    let start = object.add_symbol_data(symbol, text, &code.bytes, code.alignment);
    for relocation in &code.relocations {
        let (symbol, addend) = match &relocation.target {
            RelocationTarget::External(name) => {
                let symbol = *symbols
                    .entry(name.clone())
                    .or_insert_with(|| add_text_symbol(object, name, SymbolScope::Dynamic));
                (symbol, relocation.addend)
            }
            RelocationTarget::FunctionOffset(offset) => {
                let address = start.checked_add(u64::from(*offset)).ok_or_else(|| {
                    AotError::InvalidRelocation("function relocation offset overflow".into())
                })?;
                if relocation.kind == Reloc::RiscvPCRelLo12I {
                    // The linker finds the paired HI20 relocation at this
                    // local label. The section symbol with a nonzero addend
                    // would not identify the AUIPC site for the linker.
                    let label = object.add_symbol(Symbol {
                        name: format!(".Lzkcl_pcrel_{address:x}").into_bytes(),
                        value: address,
                        size: 0,
                        kind: SymbolKind::Label,
                        scope: SymbolScope::Compilation,
                        weak: false,
                        section: SymbolSection::Section(text),
                        flags: SymbolFlags::None,
                    });
                    (label, relocation.addend)
                } else {
                    let addend = i64::try_from(address)
                        .ok()
                        .and_then(|address| address.checked_add(relocation.addend))
                        .ok_or_else(|| {
                            AotError::InvalidRelocation(
                                "function relocation offset overflow".into(),
                            )
                        })?;
                    (object.section_symbol(text), addend)
                }
            }
        };
        let flags =
            translate_cranelift_reloc(object.format(), object.architecture(), relocation.kind)?;
        object.add_relocation(
            text,
            Relocation {
                offset: start + relocation.offset,
                symbol,
                addend,
                flags,
            },
        )?;
    }
    Ok(())
}

pub(crate) fn codegen_target_identity(config: &AotTargetConfig) -> Result<String, AotError> {
    let isa = build_target_isa(&config.target, config.opt_level)?;
    // A fixed stencil isolates the same settings and resolved ISA features
    // that Cranelift uses to validate its per-function cache.
    Ok(compute_cache_key(&*isa, &Function::new()).to_string())
}

fn build_target_isa(target: &Triple, opt_level: AotOptLevel) -> Result<OwnedTargetIsa, AotError> {
    let mut flag_builder = settings::builder();
    let opt_str = match opt_level {
        AotOptLevel::None => "none",
        AotOptLevel::Speed | AotOptLevel::Aggressive => "speed",
        AotOptLevel::Size => "speed_and_size",
    };
    flag_builder
        .set("opt_level", opt_str)
        .map_err(|err| AotError::UnsupportedTarget {
            target: target.to_string(),
            reason: err.to_string(),
        })?;
    flag_builder
        .set("is_pic", "true")
        .map_err(|err| AotError::UnsupportedTarget {
            target: target.to_string(),
            reason: err.to_string(),
        })?;
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = if *target == Triple::host() {
        cranelift_native::builder().or_else(|_| isa::lookup(target.clone()))
    } else {
        isa::lookup(target.clone())
    }
    .map_err(|error| AotError::UnsupportedTarget {
        target: target.to_string(),
        reason: error.to_string(),
    })?;

    isa_builder
        .finish(flags)
        .map_err(|err| AotError::UnsupportedTarget {
            target: target.to_string(),
            reason: err.to_string(),
        })
}

fn translate_cranelift_reloc(
    format: BinaryFormat,
    arch: object::Architecture,
    kind: Reloc,
) -> Result<RelocationFlags, AotError> {
    let flags = match kind {
        Reloc::Abs4 => RelocationFlags::Generic {
            kind: RelocationKind::Absolute,
            encoding: RelocationEncoding::Generic,
            size: 32,
        },
        Reloc::Abs8 => RelocationFlags::Generic {
            kind: RelocationKind::Absolute,
            encoding: RelocationEncoding::Generic,
            size: 64,
        },
        Reloc::X86PCRel4 => RelocationFlags::Generic {
            kind: RelocationKind::Relative,
            encoding: RelocationEncoding::X86RipRelative,
            size: 32,
        },
        Reloc::X86CallPCRel4 => RelocationFlags::Generic {
            kind: RelocationKind::Relative,
            encoding: RelocationEncoding::X86Branch,
            size: 32,
        },
        Reloc::X86CallPLTRel4 => RelocationFlags::Generic {
            kind: RelocationKind::PltRelative,
            encoding: RelocationEncoding::X86Branch,
            size: 32,
        },
        Reloc::X86GOTPCRel4 => RelocationFlags::Generic {
            kind: RelocationKind::GotRelative,
            encoding: RelocationEncoding::Generic,
            size: 32,
        },
        Reloc::X86SecRel => RelocationFlags::Generic {
            kind: RelocationKind::SectionOffset,
            encoding: RelocationEncoding::Generic,
            size: 32,
        },
        Reloc::Arm64Call => RelocationFlags::Generic {
            kind: RelocationKind::Relative,
            encoding: RelocationEncoding::AArch64Call,
            size: 26,
        },
        Reloc::Aarch64AdrGotPage21 => match format {
            BinaryFormat::Elf => RelocationFlags::Elf {
                r_type: object::elf::R_AARCH64_ADR_GOT_PAGE,
            },
            BinaryFormat::MachO => RelocationFlags::MachO {
                r_type: object::macho::ARM64_RELOC_GOT_LOAD_PAGE21,
                r_pcrel: true,
                r_length: 2,
            },
            _ => {
                return Err(AotError::InvalidRelocation(format!(
                    "unsupported Aarch64AdrGotPage21 relocation for {format:?}"
                )));
            }
        },
        Reloc::Aarch64Ld64GotLo12Nc => match format {
            BinaryFormat::Elf => RelocationFlags::Elf {
                r_type: object::elf::R_AARCH64_LD64_GOT_LO12_NC,
            },
            BinaryFormat::MachO => RelocationFlags::MachO {
                r_type: object::macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12,
                r_pcrel: false,
                r_length: 2,
            },
            _ => {
                return Err(AotError::InvalidRelocation(format!(
                    "unsupported Aarch64Ld64GotLo12Nc relocation for {format:?}"
                )));
            }
        },
        Reloc::RiscvCallPlt if format == BinaryFormat::Elf => RelocationFlags::Elf {
            r_type: object::elf::R_RISCV_CALL_PLT,
        },
        Reloc::RiscvGotHi20 if format == BinaryFormat::Elf => RelocationFlags::Elf {
            r_type: object::elf::R_RISCV_GOT_HI20,
        },
        Reloc::RiscvPCRelLo12I if format == BinaryFormat::Elf => RelocationFlags::Elf {
            r_type: object::elf::R_RISCV_PCREL_LO12_I,
        },
        Reloc::S390xPCRel32Dbl => RelocationFlags::Generic {
            kind: RelocationKind::Relative,
            encoding: RelocationEncoding::S390xDbl,
            size: 32,
        },
        Reloc::S390xPLTRel32Dbl => RelocationFlags::Generic {
            kind: RelocationKind::PltRelative,
            encoding: RelocationEncoding::S390xDbl,
            size: 32,
        },
        other => {
            return Err(AotError::InvalidRelocation(format!(
                "unsupported Cranelift relocation {other:?} for {format:?}/{arch:?}"
            )));
        }
    };
    Ok(flags)
}
