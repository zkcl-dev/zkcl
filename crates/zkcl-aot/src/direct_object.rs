//! Package call-free direct machine code alongside the validated FASL.

use object::write::{Object, Relocation, StandardSection, Symbol, SymbolSection};
use object::{
    BinaryFormat, RelocationEncoding, RelocationFlags, RelocationKind, SymbolFlags, SymbolKind,
    SymbolScope,
};
use zkcl_bytecode::{BytecodeFunction, CompiledFile};
use zkcl_vop::{LeafFunction, SelectionError, Target, select_compiled};

use crate::object_file::{add_text_symbol, create_object};
use crate::{AotError, AotRegistrationEntry, AotTargetConfig};

pub(crate) fn emit_object(
    file: &CompiledFile,
    codes: &[&BytecodeFunction],
    prefix: &str,
    fasl_bytes: &[u8],
    config: &AotTargetConfig,
    rejected: &mut Vec<(u32, SelectionError)>,
) -> Result<(Vec<u8>, Vec<AotRegistrationEntry>), AotError> {
    if codes.iter().any(|code| code.debug.retain_frame) {
        // Scalar direct objects have no call sites where a source debugger can
        // stop. DEBUG 3 uses the shared native compiler for this object unit.
        #[cfg(feature = "cranelift")]
        return crate::cranelift_object::emit_object(file, codes, prefix, fasl_bytes, config, None);
        #[cfg(not(feature = "cranelift"))]
        return Err(AotError::UnsupportedTarget {
            target: config.target.to_string(),
            reason: "DEBUG 3 direct objects require the cranelift native debug compiler feature"
                .into(),
        });
    }
    let target =
        Target::from_triple(&config.target).ok_or_else(|| AotError::UnsupportedTarget {
            target: config.target.to_string(),
            reason: "unsupported direct instruction set or calling convention".into(),
        })?;
    let word_bits = target.word_bits();
    let mut object = create_object(
        config,
        file.target
            .machine()
            .expect("object compilation has a machine ABI")
            .environment(),
        fasl_bytes,
    )?;
    let mut entries = Vec::new();
    for (index, code) in codes.iter().enumerate() {
        let entry_index = u32::try_from(index).map_err(|_| AotError::TooManyCodeEntries)?;
        let leaf_function = match select_compiled(code, &file.nodes, word_bits) {
            Ok(function) => function,
            Err(reason) => {
                rejected.push((entry_index, reason));
                continue;
            }
        };
        let emission_error = || AotError::DirectEmission {
            entry_index,
            target: config.target.to_string(),
        };
        let leaf = leaf_function
            .emit_guarded(target)
            .ok_or_else(emission_error)?;
        let (generic, relocation) =
            generic_entry(&mut object, target, &leaf_function).ok_or_else(emission_error)?;
        let symbol_name = format!("{prefix}{entry_index}");
        let leaf_name = format!("{symbol_name}_leaf");
        let text = object.section_id(StandardSection::Text);
        if entries.is_empty() && target == Target::Armv7 {
            // AAELF32 mapping symbol: this interval contains ARM-state instructions.
            object.add_symbol(Symbol {
                name: b"$a".to_vec(),
                value: 0,
                size: 0,
                kind: SymbolKind::Label,
                scope: SymbolScope::Compilation,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        }
        let symbol = add_text_symbol(&mut object, &symbol_name, SymbolScope::Linkage);
        let start = object.add_symbol_data(symbol, text, &generic, 16);
        if let Some(mut relocation) = relocation {
            relocation.offset += start;
            object.add_relocation(text, relocation)?;
        }
        let leaf_symbol = add_text_symbol(&mut object, &leaf_name, SymbolScope::Linkage);
        object.add_symbol_data(leaf_symbol, text, &leaf, 16);
        entries.push(AotRegistrationEntry {
            native_debug: false,
            entry_index,
            symbol_name,
            arity: code.arity,
            root_slots: 0,
            leaf_symbol: Some(leaf_name),
            leaf_guards: leaf_function.guards().to_vec(),
        });
    }
    Ok((object.write()?, entries))
}

fn generic_entry(
    object: &mut Object<'_>,
    target: Target,
    function: &LeafFunction,
) -> Option<(Vec<u8>, Option<Relocation>)> {
    if !function.needs_dispatch() {
        return function.emit_entry(target).map(|code| (code, None));
    }
    // Guarded calls enter the runtime with all five original ABI arguments.
    // The tail jump leaves the caller's frame and return address intact.
    let code = target.emit_dispatch_jump();
    let (offset, addend, flags) = match target {
        Target::Aarch64 => {
            let flags = if object.format() == BinaryFormat::Elf {
                RelocationFlags::Elf {
                    r_type: object::elf::R_AARCH64_JUMP26,
                }
            } else {
                RelocationFlags::Generic {
                    kind: RelocationKind::Relative,
                    encoding: RelocationEncoding::AArch64Call,
                    size: 26,
                }
            };
            (0, 0, flags)
        }
        Target::X86_64SysV | Target::X86_64Windows => (
            1,
            -4,
            RelocationFlags::Generic {
                kind: RelocationKind::Relative,
                encoding: RelocationEncoding::X86Branch,
                size: 32,
            },
        ),
        // IA-32 uses REL. object writes the -4 addend into the disp32 field.
        Target::I686 => (
            1,
            -4,
            RelocationFlags::Elf {
                r_type: object::elf::R_386_PLT32,
            },
        ),
        // A32 B reads PC+8. The assembler has encoded the implicit -8 addend
        // in imm24; the REL record must leave the instruction untouched.
        Target::Armv7 => (
            0,
            0,
            RelocationFlags::Elf {
                r_type: object::elf::R_ARM_JUMP24,
            },
        ),
        Target::Riscv64 => (
            0,
            0,
            RelocationFlags::Elf {
                r_type: object::elf::R_RISCV_CALL_PLT,
            },
        ),
    };
    const DISPATCH: &str = "zkcl_register_leaf_dispatch";
    let symbol = object
        .symbol_id(DISPATCH.as_bytes())
        .unwrap_or_else(|| add_text_symbol(object, DISPATCH, SymbolScope::Dynamic));
    Some((
        code,
        Some(Relocation {
            offset,
            addend,
            flags,
            symbol,
        }),
    ))
}
