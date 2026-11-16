//! Relocatable object format and embedded FASL shared by the native emitters.

use object::write::{Object, StandardSection, Symbol, SymbolId, SymbolSection};
use object::{BinaryFormat, Endianness, FileFlags, SymbolFlags, SymbolKind, SymbolScope, elf};
use target_lexicon::{DeploymentTarget, Environment, OperatingSystem, Riscv64Architecture, Triple};

use crate::{AotError, AotTargetConfig, DEFAULT_MACHO_DEPLOYMENT_TARGET, fasl_symbol_name};

pub(crate) fn add_text_symbol(object: &mut Object<'_>, name: &str, scope: SymbolScope) -> SymbolId {
    object.add_symbol(Symbol {
        name: name.as_bytes().to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Text,
        scope,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    })
}

pub(crate) fn create_object(
    config: &AotTargetConfig,
    environment: Environment,
    fasl_bytes: &[u8],
) -> Result<Object<'static>, AotError> {
    let target = &config.target;
    let format = match target.binary_format {
        target_lexicon::BinaryFormat::Elf => BinaryFormat::Elf,
        target_lexicon::BinaryFormat::Macho => BinaryFormat::MachO,
        target_lexicon::BinaryFormat::Coff => BinaryFormat::Coff,
        other => {
            return Err(AotError::UnsupportedTarget {
                target: target.to_string(),
                reason: format!("unsupported object format {other:?}"),
            });
        }
    };

    let arch = match target.architecture {
        target_lexicon::Architecture::Aarch64(_) => object::Architecture::Aarch64,
        target_lexicon::Architecture::Arm(
            target_lexicon::ArmArchitecture::Armv7 | target_lexicon::ArmArchitecture::Armv7a,
        ) => object::Architecture::Arm,
        target_lexicon::Architecture::X86_32(_) => object::Architecture::I386,
        target_lexicon::Architecture::X86_64 => object::Architecture::X86_64,
        target_lexicon::Architecture::Riscv64(_) => object::Architecture::Riscv64,
        target_lexicon::Architecture::S390x => object::Architecture::S390x,
        other => {
            return Err(AotError::UnsupportedTarget {
                target: target.to_string(),
                reason: format!("unsupported architecture {other:?}"),
            });
        }
    };

    let endian = match target.endianness() {
        Ok(target_lexicon::Endianness::Little) => Endianness::Little,
        Ok(target_lexicon::Endianness::Big) => Endianness::Big,
        Err(()) => {
            return Err(AotError::UnsupportedTarget {
                target: target.to_string(),
                reason: "unknown target endianness".to_string(),
            });
        }
    };

    let mut obj = Object::new(format, arch, endian);
    if format == BinaryFormat::Elf && arch == object::Architecture::Arm {
        set_armv7_elf_attributes(&mut obj, environment);
    }
    if format == BinaryFormat::Elf
        && target.architecture
            == target_lexicon::Architecture::Riscv64(Riscv64Architecture::Riscv64gc)
    {
        // The supported riscv64gc C targets link against LP64D runtime objects.
        obj.flags = FileFlags::Elf {
            os_abi: elf::ELFOSABI_NONE,
            abi_version: 0,
            e_flags: elf::EF_RISCV_FLOAT_ABI_DOUBLE,
        };
    }
    if format == BinaryFormat::MachO {
        obj.set_subsections_via_symbols();
        if let Some((_, platform, version)) = macho_deployment(target, environment)? {
            let packed_version = (u32::from(version.major) << 16)
                | (u32::from(version.minor) << 8)
                | u32::from(version.patch);
            let mut bv = object::write::MachOBuildVersion::default();
            bv.platform = platform;
            bv.minos = packed_version;
            bv.sdk = packed_version;
            obj.set_macho_build_version(bv);
        }
    }

    let rodata = obj.section_id(StandardSection::ReadOnlyData);
    let fasl = obj.add_symbol(Symbol {
        name: fasl_symbol_name(&config.module_id).into_bytes(),
        value: 0,
        size: 0,
        kind: SymbolKind::Data,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    obj.add_symbol_data(fasl, rodata, fasl_bytes, 8);
    Ok(obj)
}

/// The LLVM OS name, Mach-O platform ID and minimum product version for this target.
pub(crate) fn macho_deployment(
    target: &Triple,
    environment: Environment,
) -> Result<Option<(&'static str, u32, DeploymentTarget)>, AotError> {
    use OperatingSystem as Os;
    use object::macho::*;

    if target.binary_format != target_lexicon::BinaryFormat::Macho {
        return Ok(None);
    }
    let simulator = environment == Environment::Sim;
    let (name, platform, version) = match target.operating_system {
        Os::Darwin(Some(kernel)) => {
            // Darwin kernel majors map to product releases; kernel minor and
            // patch numbers do not identify a macOS deployment target.
            let (major, minor) = match kernel.major {
                4..=19 => (10, (kernel.major - 4) as u8),
                20..=24 => (kernel.major - 9, 0),
                25.. if kernel.major != u16::MAX => (kernel.major + 1, 0),
                _ => {
                    return Err(AotError::UnsupportedTarget {
                        target: target.to_string(),
                        reason:
                            "Darwin kernel version has no representable macOS deployment target"
                                .into(),
                    });
                }
            };
            (
                "macosx",
                PLATFORM_MACOS,
                Some(DeploymentTarget {
                    major,
                    minor,
                    patch: 0,
                }),
            )
        }
        Os::Darwin(None) => ("macosx", PLATFORM_MACOS, None),
        Os::MacOSX(version) => ("macosx", PLATFORM_MACOS, version),
        Os::IOS(version) if simulator => ("ios", PLATFORM_IOSSIMULATOR, version),
        Os::IOS(version) if environment == Environment::Macabi => {
            ("ios", PLATFORM_MACCATALYST, version)
        }
        Os::IOS(version) => ("ios", PLATFORM_IOS, version),
        Os::TvOS(version) if simulator => ("tvos", PLATFORM_TVOSSIMULATOR, version),
        Os::TvOS(version) => ("tvos", PLATFORM_TVOS, version),
        Os::WatchOS(version) if simulator => ("watchos", PLATFORM_WATCHOSSIMULATOR, version),
        Os::WatchOS(version) => ("watchos", PLATFORM_WATCHOS, version),
        Os::XROS(version) | Os::VisionOS(version) if simulator => {
            ("xros", PLATFORM_XROSSIMULATOR, version)
        }
        Os::XROS(version) | Os::VisionOS(version) => ("xros", PLATFORM_XROS, version),
        _ => return Ok(None),
    };
    Ok(Some((
        name,
        platform,
        version.unwrap_or(DEFAULT_MACHO_DEPLOYMENT_TARGET),
    )))
}

fn set_armv7_elf_attributes(object: &mut Object<'_>, environment: Environment) {
    use object::SectionKind;

    object.flags = FileFlags::Elf {
        os_abi: elf::ELFOSABI_NONE,
        abi_version: 0,
        e_flags: elf::EF_ARM_EABI_VER5,
    };
    // In a relocatable file the linker reads the float calling convention
    // from Tag_ABI_VFP_args. EF_ARM_ABI_FLOAT_* describes the linked image.
    let hard_float = matches!(
        environment,
        Environment::Eabihf
            | Environment::Gnueabihf
            | Environment::Musleabihf
            | Environment::Uclibceabihf
    );
    let mut attributes = object::write::elf::AttributesWriter::new(Endianness::Little);
    attributes.start_subsection(b"aeabi");
    attributes.start_subsubsection(elf::Tag_File);
    for (tag, value) in [
        (6, 10),                     // Tag_CPU_arch = Arm v7
        (7, u64::from(b'A')),        // Tag_CPU_arch_profile = Application
        (8, 1),                      // Tag_ARM_ISA_use = permitted
        (28, u64::from(hard_float)), // Tag_ABI_VFP_args = base / VFP registers
    ] {
        attributes.write_attribute_tag(tag);
        attributes.write_attribute_integer(value);
    }
    attributes.end_subsubsection();
    attributes.end_subsection();
    let section = object.add_section(
        Vec::new(),
        b".ARM.attributes".to_vec(),
        SectionKind::Elf(elf::SHT_ARM_ATTRIBUTES),
    );
    object.section_mut(section).set_data(attributes.data(), 1);
}
