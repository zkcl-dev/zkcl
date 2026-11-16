use object::{Architecture, BinaryFormat, Object, ObjectSection, ObjectSymbol, SectionKind};
use target_lexicon::Triple;
use zkcl_aot::{AotBackendKind, AotTargetConfig, compile_fasl_to_object};
use zkcl_bytecode::{
    Arity, BytecodeFunction, CompiledFile, FixnumArithmetic, Instruction, LiteralNode,
};
use zkcl_target::CompilationTarget;

fn returns(arity: usize, values: std::ops::Range<usize>) -> BytecodeFunction {
    BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity: Arity {
            required: arity,
            maximum: Some(arity),
        },
        registers: arity.max(values.end),
        instructions: vec![Instruction::SetValues(values)],
        constants: Vec::new(),
        lambda_list: None,
        captures: Vec::new(),
        notinline: Vec::new(),
        vop_calls: Vec::new(),
        handlers: Vec::new(),
        restarts: Vec::new(),
        keywords: Vec::new(),
        jump_tables: Vec::new(),
        facts: Default::default(),
    }
}

#[test]
fn direct_object_keeps_file_indices_and_target_fasl_across_abis() {
    use Architecture::{Aarch64, Arm, I386, Riscv64, X86_64};
    use BinaryFormat::{Coff, Elf, MachO};
    for (triple, format, arch) in [
        ("i686-unknown-linux-gnu", Elf, I386),
        ("armv7-unknown-linux-gnueabihf", Elf, Arm),
        ("armv7-unknown-linux-gnueabi", Elf, Arm),
        ("armv7-linux-androideabi", Elf, Arm),
        ("riscv64gc-unknown-linux-gnu", Elf, Riscv64),
        ("aarch64-apple-ios", MachO, Aarch64),
        ("aarch64-apple-ios-sim", MachO, Aarch64),
        ("aarch64-apple-darwin", MachO, Aarch64),
        ("aarch64-linux-android", Elf, Aarch64),
        ("aarch64-unknown-linux-gnu", Elf, Aarch64),
        ("aarch64-pc-windows-msvc", Coff, Aarch64),
        ("x86_64-pc-windows-msvc", Coff, X86_64),
        ("x86_64-apple-darwin", MachO, X86_64),
        ("x86_64-apple-ios", MachO, X86_64),
        ("x86_64-unknown-linux-gnu", Elf, X86_64),
    ] {
        let triple: Triple = triple.parse().unwrap();
        let mut choice = returns(4, 3..4);
        choice.instructions = vec![
            Instruction::UnlessFixnumInRange(0, -3, 7, 3),
            Instruction::SetValues(1..2),
            Instruction::Jump(4),
            Instruction::SetValues(3..4),
        ];
        let file = CompiledFile {
            source_files: Vec::new(),
            target: CompilationTarget::from_triple(triple.clone()).unwrap(),
            entries: vec![returns(0, 0..0)],
            nodes: vec![LiteralNode::Function(choice)],
        };
        let config = AotTargetConfig::host("direct-test")
            .with_target(triple.clone())
            .with_backend(AotBackendKind::Direct);
        let result = compile_fasl_to_object(&file, &config).unwrap();
        assert_eq!(result.registration_entries.len(), 1, "{triple}");
        let entry = &result.registration_entries[0];
        assert_eq!(entry.entry_index, 1);
        assert_eq!(entry.root_slots, 0);
        let object = object::File::parse(result.object_bytes.as_slice()).unwrap();
        assert_eq!((object.format(), object.architecture()), (format, arch));
        let get_symbol = |name: &str| {
            object
                .symbols()
                .find(|s| s.name().is_ok_and(|n| n.trim_start_matches('_') == name))
                .unwrap()
        };
        let leaf_symbol = get_symbol(entry.leaf_symbol.as_deref().unwrap());
        let function = get_symbol(&entry.symbol_name);
        assert!(leaf_symbol.address() > function.address());
        let text = object
            .section_by_index(function.section_index().unwrap())
            .unwrap();
        assert_eq!(text.kind(), SectionKind::Text);
        if arch == Arm {
            let arm_state = get_symbol("$a");
            assert_eq!(arm_state.section_index(), function.section_index());
            assert_eq!(arm_state.address(), function.address());
            assert!(arm_state.is_local());
        }
        assert!(text.size() > 0 && text.relocations().next().is_none());
        let fasl = get_symbol("zkcl_aot_direct_test_fasl");
        let rodata = object
            .section_by_index(fasl.section_index().unwrap())
            .unwrap();
        assert_eq!(rodata.kind(), SectionKind::ReadOnlyData);
        let embedded = rodata
            .data_range(fasl.address(), result.fasl_bytes.len() as u64)
            .unwrap()
            .unwrap();
        assert_eq!(embedded, file.encode().unwrap());
        assert_eq!(CompiledFile::decode(embedded).unwrap(), file);
    }
}

#[test]
fn module_without_native_functions_still_exports_its_fasl() {
    let target: CompilationTarget = "x86_64-unknown-linux-gnu".parse().unwrap();
    let config = AotTargetConfig::host("empty")
        .with_backend(AotBackendKind::Direct)
        .with_target(target.machine().unwrap().triple().clone());
    let file = CompiledFile {
        target,
        source_files: Vec::new(),
        entries: vec![returns(0, 0..0)],
        nodes: Vec::new(),
    };
    let result = compile_fasl_to_object(&file, &config).unwrap();
    assert!(result.registration_entries.is_empty());
    assert_eq!(
        result.direct_rejections,
        [(0, zkcl_vop::SelectionError::ResultCount)]
    );
    let object = object::File::parse(result.object_bytes.as_slice()).unwrap();
    assert!(object.symbols().any(|s| {
        s.name()
            .is_ok_and(|n| n.trim_start_matches('_') == "zkcl_aot_empty_fasl")
    }));
}

#[test]
fn guarded_c_entries_tail_jump_to_runtime_dispatch() {
    for triple in [
        "i686-unknown-linux-gnu",
        "armv7-unknown-linux-gnueabihf",
        "armv7-linux-androideabi",
        "riscv64gc-unknown-linux-gnu",
        "aarch64-apple-ios",
        "aarch64-unknown-linux-gnu",
        "aarch64-pc-windows-msvc",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    ] {
        let target: CompilationTarget = triple.parse().unwrap();
        let config = AotTargetConfig::host("guard")
            .with_target(target.machine().unwrap().triple().clone())
            .with_backend(AotBackendKind::Direct);
        let mut increment = returns(1, 0..1);
        increment.constants.push(0);
        increment.instructions.insert(
            0,
            Instruction::TryFixnumArithmetic {
                op: FixnumArithmetic::Inc,
                symbol: 0,
                out: 0,
                args: 0..1,
            },
        );
        let file = CompiledFile {
            target,
            source_files: Vec::new(),
            entries: vec![returns(0, 0..0)],
            nodes: vec![
                LiteralNode::InternedSymbol {
                    package: "COMMON-LISP".into(),
                    name: "1+".into(),
                },
                LiteralNode::Function(increment),
            ],
        };
        let output = compile_fasl_to_object(&file, &config).unwrap();
        let [entry] = output.registration_entries.as_slice() else {
            panic!("missing guarded leaf on {triple}")
        };
        assert!(!entry.leaf_guards.is_empty());
        let object = object::File::parse(output.object_bytes.as_slice()).unwrap();
        let text = object
            .sections()
            .find(|s| s.kind() == SectionKind::Text)
            .unwrap();
        let relocations = text.relocations().collect::<Vec<_>>();
        let [(offset, jump)] = relocations.as_slice() else {
            panic!("expected one dispatch jump on {triple}")
        };
        if triple.starts_with("i686") {
            assert_eq!(
                jump.flags(),
                object::RelocationFlags::Elf {
                    r_type: object::elf::R_386_PLT32
                }
            );
            assert!(jump.has_implicit_addend());
            let at = *offset as usize;
            let displacement =
                i32::from_le_bytes(text.data().unwrap()[at..at + 4].try_into().unwrap());
            assert_eq!(displacement, -4);
        } else if triple.starts_with("armv7") {
            assert_eq!(
                jump.flags(),
                object::RelocationFlags::Elf {
                    r_type: object::elf::R_ARM_JUMP24
                }
            );
            assert!(jump.has_implicit_addend());
            let at = *offset as usize;
            // AAELF32 embeds the PC bias in B's signed, scaled 24-bit immediate.
            let instruction =
                i32::from_le_bytes(text.data().unwrap()[at..at + 4].try_into().unwrap());
            assert_eq!((instruction << 8) >> 6, -8);
        } else if triple.starts_with("riscv64") {
            assert!(matches!(
                object.flags(),
                object::FileFlags::Elf {
                    e_flags: object::elf::EF_RISCV_FLOAT_ABI_DOUBLE,
                    ..
                }
            ));
            assert_eq!(
                jump.flags(),
                object::RelocationFlags::Elf {
                    r_type: object::elf::R_RISCV_CALL_PLT,
                }
            );
            let at = *offset as usize;
            assert_eq!(
                &text.data().unwrap()[at..at + 8],
                &[0x17, 0x03, 0, 0, 0x67, 0, 0x03, 0]
            );
        }
        let object::RelocationTarget::Symbol(index) = jump.target() else {
            panic!("dispatch must be an external symbol")
        };
        assert_eq!(
            object
                .symbol_by_index(index)
                .unwrap()
                .name()
                .unwrap()
                .trim_start_matches('_'),
            "zkcl_register_leaf_dispatch"
        );
    }
}
