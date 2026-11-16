#[cfg(any(feature = "cranelift", feature = "llvm"))]
use std::str::FromStr;

#[cfg(any(feature = "cranelift", feature = "llvm"))]
use object::{Architecture, BinaryFormat, ObjectSection, SectionKind, SymbolKind};
#[cfg(any(feature = "cranelift", feature = "llvm"))]
use object::{Object, ObjectSymbol};
use target_lexicon::Triple;
#[cfg(feature = "llvm")]
use zkcl_aot::AotOptLevel;
use zkcl_aot::{
    AotBackendKind, AotTargetConfig, compile_fasl_to_object,
    rust_source::generate_standalone_main_source,
};
use zkcl_bytecode::{
    Arity, BytecodeFunction, Callee, CompiledFile, Immediate, Instruction, LiteralNode,
};

fn code_without_side_tables(
    arity: Arity,
    registers: usize,
    instructions: Vec<Instruction>,
) -> BytecodeFunction {
    BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity,
        registers,
        instructions,
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

/// Build a valid multi-function `CompiledFile` containing:
/// - entry 0 (`Arity::ZERO`): defines `CL-USER::SUM-TO` from nested `LiteralNode::Function` (index 2)
/// - entry 1 (`Arity::ZERO`): pure fixnum calculation `(+ 10 32)` returning 42
/// - nested function (index 2, `Arity { required: 1, maximum: Some(1) }`): fixnum loop `sum(0..n)`
///   with a `CallNamed` safepoint inside the loop body.
fn compiled_file_with_nested_sum_loop() -> CompiledFile {
    compiled_file_for_target(&Triple::host())
}

fn compiled_file_for_target(triple: &Triple) -> CompiledFile {
    // Nodes:
    // 0: CL:+
    // 1: CL:<
    // 2: CL-USER::HOOK (safepoint inside loop)
    // 3: CL-USER::SUM-TO
    // 4: LiteralNode::Function(loop_fn)
    let mut loop_code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        5,
        vec![
            Instruction::Load(1, Immediate::Fixnum(0)), // 0: sum = 0
            Instruction::Load(2, Immediate::Fixnum(0)), // 1: i = 0
            // loop header (pc 2):
            Instruction::Copy(3, 2), // 2: r3 = i
            Instruction::Copy(4, 0), // 3: r4 = n
            Instruction::Call {
                callee: Callee::Named(1), // CL:<
                out: Some(3),
                args: 3..5,
                at: 4,
                associate: false,
                notinline: false,
            }, // 4
            Instruction::Unless(3, 12), // 5 -> exit (12)
            Instruction::Call {
                callee: Callee::Named(0), // CL:+
                out: Some(1),
                args: 1..3, // sum + i
                at: 6,
                associate: false,
                notinline: false,
            }, // 6
            Instruction::Load(4, Immediate::Fixnum(1)), // 7: r4 = 1
            Instruction::Copy(3, 2), // 8: r3 = i
            Instruction::Call {
                callee: Callee::Named(0), // CL:+
                out: Some(2),
                args: 3..5, // i + 1
                at: 9,
                associate: false,
                notinline: false,
            }, // 9
            Instruction::Call {
                callee: Callee::Named(2), // CL-USER::HOOK(n)
                out: None,
                args: 0..1,
                at: 10,
                associate: false,
                notinline: false,
            }, // 10
            Instruction::Jump(2),    // 11
            Instruction::SetValues(1..2), // 12
        ],
    );
    loop_code.constants = vec![0, 1, 2];

    let mut sum_to_definition = code_without_side_tables(
        Arity::ZERO,
        1,
        vec![
            Instruction::Constant(0, 1), // r0 = #'sum-to
            Instruction::DefineFunction {
                symbol: 0,
                definition: 0,
                is_macro: false,
                at: 1,
            },
            Instruction::Load(0, Immediate::True),
            Instruction::SetValues(0..1),
        ],
    );
    sum_to_definition.constants = vec![3, 4];

    let mut constant_addition = code_without_side_tables(
        Arity::ZERO,
        3,
        vec![
            Instruction::Load(0, Immediate::Fixnum(10)),
            Instruction::Load(1, Immediate::Fixnum(32)),
            Instruction::Call {
                callee: Callee::Named(0), // CL:+
                out: Some(2),
                args: 0..2,
                at: 2,
                associate: false,
                notinline: false,
            },
            Instruction::SetValues(2..3),
        ],
    );
    constant_addition.constants = vec![0];

    CompiledFile {
        target: zkcl_target::CompilationTarget::from_triple(triple.clone()).unwrap(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::InternedSymbol {
                package: "COMMON-LISP".to_string(),
                name: "+".to_string(),
            },
            LiteralNode::InternedSymbol {
                package: "COMMON-LISP".to_string(),
                name: "<".to_string(),
            },
            LiteralNode::InternedSymbol {
                package: "CL-USER".to_string(),
                name: "HOOK".to_string(),
            },
            LiteralNode::InternedSymbol {
                package: "CL-USER".to_string(),
                name: "SUM-TO".to_string(),
            },
            LiteralNode::Function(loop_code),
        ],
        entries: vec![sum_to_definition, constant_addition],
    }
}

#[cfg(any(feature = "cranelift", feature = "llvm"))]
fn find_defined_symbol<'a>(
    obj: &'a object::File<'a>,
    expected: &str,
) -> Option<object::Symbol<'a, 'a>> {
    obj.symbols().find(|sym| {
        sym.is_definition()
            && sym
                .name()
                .ok()
                .is_some_and(|name| name == expected || name.strip_prefix('_') == Some(expected))
    })
}

#[cfg(any(feature = "cranelift", feature = "llvm"))]
#[test]
fn ssa_backends_import_the_progv_binding_function() {
    let code = code_without_side_tables(
        Arity {
            required: 2,
            maximum: Some(2),
        },
        2,
        vec![
            Instruction::BindProgv {
                symbols: 0,
                values: 1,
                at: 0,
            },
            Instruction::Unbind(1),
            Instruction::SetValues(0..1),
        ],
    );
    let file = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![LiteralNode::Function(code)],
        entries: Vec::new(),
    };

    for backend in [AotBackendKind::CraneliftObject, AotBackendKind::Llvm] {
        if !backend.is_available() {
            continue;
        }
        let config = AotTargetConfig::host("progv").with_backend(backend);
        let object = compile_fasl_to_object(&file, &config).unwrap();
        assert_eq!(object.registration_entries.len(), 1, "{backend:?}");
        let parsed = object::File::parse(object.object_bytes.as_slice()).unwrap();
        assert!(
            parsed.symbols().any(|symbol| {
                symbol.is_undefined()
                    && symbol
                        .name()
                        .is_ok_and(|name| name.trim_prefix('_') == "zkcl_rt_bind_progv")
            }),
            "{backend:?} missing PROGV binding function"
        );
    }
}

#[cfg(feature = "llvm")]
#[test]
fn llvm_selects_popcount_template_for_apple_arm64() {
    use zkcl_bytecode::{Aarch64Popcount, VopCall, WordCountOp};

    let explicit_template = Aarch64Popcount::WidenAndAddBytes;
    for (target, template, expected) in [
        ("aarch64-apple-darwin", Some(explicit_template), "uaddlv"),
        ("aarch64-apple-darwin", None, "@llvm.ctpop.i64"),
        (
            "aarch64-unknown-linux-gnu",
            Some(explicit_template),
            "@llvm.ctpop.i64",
        ),
    ] {
        let target: Triple = target.parse().unwrap();
        let operation = WordCountOp::Popcount;
        let mut code = code_without_side_tables(
            Arity {
                required: 1,
                maximum: Some(1),
            },
            1,
            vec![Instruction::Call {
                callee: Callee::Named(0),
                out: None,
                args: 0..1,
                at: 0,
                associate: false,
                notinline: false,
            }],
        );
        code.constants = vec![0, 1];
        code.vop_calls.push(VopCall {
            pc: 0,
            binding: 1,
            operation,
            aarch64_popcount: template,
        });
        let file = CompiledFile {
            target: zkcl_target::CompilationTarget::from_triple(target.clone()).unwrap(),
            source_files: Vec::new(),
            nodes: vec![
                LiteralNode::InternedSymbol {
                    package: "COMMON-LISP-USER".into(),
                    name: "WORD-COUNT".into(),
                },
                LiteralNode::Fixnum(operation.descriptor() as i64),
                LiteralNode::Function(code),
            ],
            entries: Vec::new(),
        };
        let config = AotTargetConfig::host("word-count")
            .with_target(target)
            .with_backend(AotBackendKind::Llvm);
        let object = compile_fasl_to_object(&file, &config).unwrap();
        assert_eq!(object.registration_entries.len(), 1);
        let ir = object.llvm_ir.unwrap();
        assert!(ir.contains(expected), "{ir}");
        if expected.starts_with('@') {
            assert!(!ir.contains(" asm "), "{ir}");
        }
    }
}

#[test]
fn incompatible_fasl_is_rejected_by_both_object_entry_points() {
    let file = compiled_file_for_target(&"x86_64-unknown-linux-gnu".parse().unwrap());
    let config =
        AotTargetConfig::host("mismatched").with_target("x86_64-pc-windows-msvc".parse().unwrap());
    for outcome in [
        compile_fasl_to_object(&file, &config),
        zkcl_aot::compile_fasl_bytes_to_object(file.encode().unwrap(), &config),
    ] {
        assert!(matches!(
            outcome,
            Err(zkcl_aot::AotError::CompiledFile(
                zkcl_bytecode::CompiledFileError::TargetMismatch { .. }
            ))
        ));
    }
}

#[cfg(not(feature = "cranelift"))]
#[test]
fn selecting_disabled_cranelift_reports_its_feature() {
    assert!(!AotBackendKind::CraneliftObject.is_available());
    let host = AotTargetConfig::host("disabled");
    assert_eq!(
        host.backend,
        if cfg!(feature = "llvm") {
            AotBackendKind::Llvm
        } else {
            AotBackendKind::Direct
        }
    );
    let config = host.with_backend(AotBackendKind::CraneliftObject);
    assert!(matches!(
        compile_fasl_to_object(&compiled_file_with_nested_sum_loop(), &config),
        Err(zkcl_aot::AotError::CraneliftUnavailable)
    ));
}

#[cfg(feature = "cranelift")]
#[test]
fn cranelift_exports_native_entries_for_the_embedded_fasl() {
    let file = compiled_file_with_nested_sum_loop();
    let config = AotTargetConfig::host("test-mod");
    let object = compile_fasl_to_object(&file, &config).expect("AOT compilation succeeds");

    assert!(object.llvm_ir.is_none());
    assert_eq!(object.registration_entries.len(), 3);
    assert_eq!(object.registration_entries[0].entry_index, 0);
    assert_eq!(
        object.registration_entries[0].symbol_name,
        "zkcl_aot_test_mod_fn_0"
    );
    assert_eq!(object.registration_entries[1].entry_index, 1);
    assert_eq!(
        object.registration_entries[1].symbol_name,
        "zkcl_aot_test_mod_fn_1"
    );
    assert_eq!(object.registration_entries[1].root_slots, 2); // frame base slots for the CL:+ call
    assert_eq!(object.registration_entries[2].entry_index, 2);
    assert_eq!(
        object.registration_entries[2].symbol_name,
        "zkcl_aot_test_mod_fn_2"
    );
    assert!(object.registration_entries[2].root_slots >= 3); // slot[0]=code, slot[1]=closure, slot[2]=n

    let parsed = object::File::parse(object.object_bytes.as_slice())
        .expect("object bytes must parse cleanly");

    for f in &object.registration_entries {
        let sym = find_defined_symbol(&parsed, &f.symbol_name)
            .unwrap_or_else(|| panic!("missing symbol {}", f.symbol_name));
        assert_eq!(sym.kind(), SymbolKind::Text);
    }

    let fasl_sym = find_defined_symbol(&parsed, "zkcl_aot_test_mod_fasl")
        .expect("embedded FASL symbol must exist");
    assert_eq!(fasl_sym.kind(), SymbolKind::Data);

    let rodata = parsed
        .sections()
        .find(|s| s.kind() == SectionKind::ReadOnlyData)
        .expect("read-only data section must exist");
    let rodata_bytes = rodata.data().expect("rodata bytes readable");
    assert!(!rodata_bytes.is_empty());
    assert!(
        rodata_bytes
            .windows(object.fasl_bytes.len())
            .any(|w| w == object.fasl_bytes.as_slice()),
        "rodata must contain exact serialized CompiledFile FASL bytes"
    );

    let decoded = CompiledFile::decode(&object.fasl_bytes).expect("FASL bytes decode");
    assert_eq!(decoded, file);
}

#[cfg(feature = "cranelift")]
#[test]
fn cranelift_aot_output_is_deterministic_across_worker_counts() {
    let mut file = compiled_file_with_nested_sum_loop();
    let entries = std::mem::take(&mut file.entries);
    file.entries = (0..40).flat_map(|_| entries.iter().cloned()).collect();
    let mut vm_only = code_without_side_tables(
        Arity::ZERO,
        0,
        vec![Instruction::ProclaimSpecial { symbol: 0, at: 0 }],
    );
    vm_only.constants.push(2); // CL-USER::HOOK
    let skipped = 21;
    file.entries.insert(skipped, vm_only);
    let mut config = AotTargetConfig::host("many-functions");
    config.jobs = std::num::NonZeroUsize::new(1);
    let first = compile_fasl_to_object(&file, &config).unwrap();
    config.jobs = std::num::NonZeroUsize::new(4);
    let second = compile_fasl_to_object(&file, &config).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.registration_entries.len(), file.entries.len());
    let expected = (0..=file.entries.len()).filter(|&index| index != skipped);
    assert!(
        first
            .registration_entries
            .iter()
            .map(|f| f.entry_index as usize)
            .eq(expected)
    );
    let object = object::File::parse(first.object_bytes.as_slice()).unwrap();
    assert!(
        first
            .registration_entries
            .iter()
            .all(|f| find_defined_symbol(&object, &f.symbol_name).is_some())
    );
}

#[cfg(feature = "cranelift")]
#[test]
fn cranelift_reuses_function_stencils_at_new_entry_indices() {
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, UNIX_EPOCH};

    let scratch =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/incremental-build");
    fs::create_dir_all(&scratch).unwrap();
    let cache = tempfile::tempdir_in(scratch).unwrap();
    let cached = |file: &CompiledFile, config: &AotTargetConfig| {
        zkcl_aot::compile_fasl_bytes_to_object_with_function_cache(
            file.encode().unwrap(),
            config,
            cache.path(),
        )
        .unwrap()
    };

    let mut file = compiled_file_with_nested_sum_loop();
    let mut config = AotTargetConfig::host("before-edit");
    file.source_files.push("before.lisp".into());
    let LiteralNode::Function(nested) = &mut file.nodes[4] else {
        panic!("expected the nested loop")
    };
    nested.source_file = Some(0);
    nested.debug.retain_frame = true;
    let before = cached(&file, &config);
    assert_eq!(before, compile_fasl_to_object(&file, &config).unwrap());

    let original_records: Vec<_> = fs::read_dir(cache.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(original_records.len(), 3);
    let original_time = UNIX_EPOCH + Duration::from_secs(1_000_000);
    for record in &original_records {
        fs::File::options()
            .write(true)
            .open(record)
            .unwrap()
            .set_modified(original_time)
            .unwrap();
    }

    // Adding code before the loop changes its entry index and the embedded
    // FASL. Machine code still receives the current function at runtime.
    file.entries.insert(
        0,
        code_without_side_tables(
            Arity::ZERO,
            1,
            vec![
                Instruction::Load(0, Immediate::Fixnum(17)),
                Instruction::SetValues(0..1),
            ],
        ),
    );
    file.source_files[0] = "after.lisp".into();
    config.module_id = "after-edit".into();
    let after = cached(&file, &config);
    assert_eq!(after, compile_fasl_to_object(&file, &config).unwrap());
    assert_eq!(after.registration_entries.len(), 4);
    let nested = &after.registration_entries[3];
    assert_eq!(nested.symbol_name, "zkcl_aot_after_edit_fn_3");
    assert!(nested.native_debug);
    assert_eq!(fs::read_dir(cache.path()).unwrap().count(), 4);
    for record in &original_records {
        assert_eq!(
            fs::metadata(record).unwrap().modified().unwrap(),
            original_time
        );
    }

    // A valid record placed at a different key fails the key-bound checksum.
    let expected_record = fs::read(&original_records[1]).unwrap();
    fs::copy(&original_records[0], &original_records[1]).unwrap();
    assert_eq!(cached(&file, &config), after);
    assert_eq!(fs::read(&original_records[1]).unwrap(), expected_record);
}

#[cfg(feature = "cranelift")]
#[test]
fn cranelift_object_headers_match_target_triples() {
    // 1. iOS ARM64 Mach-O
    let ios_triple = Triple::from_str("aarch64-apple-ios").expect("valid iOS triple");
    let ios_config = AotTargetConfig::host("ios-mod").with_target(ios_triple);
    let ios_object =
        compile_fasl_to_object(&compiled_file_for_target(&ios_config.target), &ios_config)
            .expect("iOS AOT compilation succeeds");
    let ios_obj =
        object::File::parse(ios_object.object_bytes.as_slice()).expect("parse iOS Mach-O");
    assert_eq!(ios_obj.format(), BinaryFormat::MachO);
    assert_eq!(ios_obj.architecture(), Architecture::Aarch64);
    for idx in 0..3 {
        let name = format!("zkcl_aot_ios_mod_fn_{idx}");
        assert!(
            find_defined_symbol(&ios_obj, &name).is_some(),
            "missing iOS symbol {name}"
        );
    }
    assert!(find_defined_symbol(&ios_obj, "zkcl_aot_ios_mod_fasl").is_some());

    // 2. Linux x86_64 ELF
    let linux_triple =
        Triple::from_str("x86_64-unknown-linux-gnu").expect("valid Linux x86_64 triple");
    let linux_config = AotTargetConfig::host("linux-mod").with_target(linux_triple);
    let linux_object = compile_fasl_to_object(
        &compiled_file_for_target(&linux_config.target),
        &linux_config,
    )
    .expect("Linux ELF AOT compilation succeeds");
    let linux_obj =
        object::File::parse(linux_object.object_bytes.as_slice()).expect("parse Linux ELF");
    assert_eq!(linux_obj.format(), BinaryFormat::Elf);
    assert_eq!(linux_obj.architecture(), Architecture::X86_64);
    for idx in 0..3 {
        let name = format!("zkcl_aot_linux_mod_fn_{idx}");
        assert!(
            find_defined_symbol(&linux_obj, &name).is_some(),
            "missing Linux ELF symbol {name}"
        );
    }
    let rodata = linux_obj
        .section_by_name(".rodata")
        .expect("ELF .rodata section must exist");
    let rodata_data = rodata.data().expect("ELF .rodata bytes readable");
    assert!(
        rodata_data
            .windows(linux_object.fasl_bytes.len())
            .any(|w| w == linux_object.fasl_bytes.as_slice())
    );
}

#[cfg(feature = "cranelift")]
#[test]
fn cranelift_fixnum_arithmetic_and_fallback_compile_on_all_native_architectures() {
    use object::{RelocationFlags, RelocationTarget, elf};
    use zkcl_bytecode::FixnumArithmetic;

    for (triple, architecture) in [
        ("aarch64-apple-darwin", Architecture::Aarch64),
        ("x86_64-unknown-linux-gnu", Architecture::X86_64),
        ("riscv64gc-unknown-linux-gnu", Architecture::Riscv64),
        ("s390x-unknown-linux-gnu", Architecture::S390x),
    ] {
        for (op, name, arguments) in [
            (FixnumArithmetic::Add, "+", 2),
            (FixnumArithmetic::Sub, "-", 2),
            (FixnumArithmetic::Mul, "*", 2),
            (FixnumArithmetic::Inc, "1+", 1),
            (FixnumArithmetic::Dec, "1-", 1),
        ] {
            let mut code = code_without_side_tables(
                Arity {
                    required: arguments,
                    maximum: Some(arguments),
                },
                arguments + 1,
                vec![
                    Instruction::TryFixnumArithmetic {
                        op,
                        symbol: 0,
                        out: arguments,
                        args: 0..arguments,
                    },
                    Instruction::Unless(arguments, 3),
                    Instruction::Jump(4),
                    Instruction::Call {
                        callee: Callee::Named(0),
                        out: Some(arguments),
                        args: 0..arguments,
                        at: 3,
                        associate: false,
                        notinline: true,
                    },
                    Instruction::SetValues(arguments..arguments + 1),
                ],
            );
            code.constants.push(0);
            let file = CompiledFile {
                target: triple.parse().unwrap(),
                source_files: Vec::new(),
                nodes: vec![
                    LiteralNode::InternedSymbol {
                        package: "COMMON-LISP".into(),
                        name: name.into(),
                    },
                    LiteralNode::Function(code),
                ],
                entries: Vec::new(),
            };
            let config = AotTargetConfig::host("arithmetic")
                .with_target(triple.parse().unwrap())
                .with_backend(AotBackendKind::CraneliftObject);
            let compiled = compile_fasl_to_object(&file, &config)
                .unwrap_or_else(|error| panic!("{triple} {name}: {error}"));
            assert_eq!(compiled.registration_entries.len(), 1, "{triple} {name}");
            let object = object::File::parse(compiled.object_bytes.as_slice()).unwrap();
            assert_eq!(object.architecture(), architecture);
            assert!(object.symbols().any(|symbol| {
                symbol.is_undefined()
                    && symbol
                        .name()
                        .is_ok_and(|name| name.trim_start_matches('_') == "zkcl_rt_call_named")
            }));
            if architecture == Architecture::Riscv64 {
                let object::FileFlags::Elf { e_flags, .. } = object.flags() else {
                    panic!("RISC-V target did not emit ELF flags");
                };
                assert_eq!(
                    e_flags & elf::EF_RISCV_FLOAT_ABI,
                    elf::EF_RISCV_FLOAT_ABI_DOUBLE
                );
                let text = object.section_by_name(".text").unwrap();
                let relocs: Vec<_> = text.relocations().collect();
                let mut pairs = 0;
                for (_, relocation) in &relocs {
                    if relocation.flags()
                        != (RelocationFlags::Elf {
                            r_type: elf::R_RISCV_PCREL_LO12_I,
                        })
                    {
                        continue;
                    }
                    let RelocationTarget::Symbol(index) = relocation.target() else {
                        panic!("RISC-V low relocation did not reference a label");
                    };
                    let label = object.symbol_by_index(index).unwrap();
                    assert_eq!(label.section_index(), Some(text.index()));
                    assert!(label.is_local());
                    assert_eq!(relocation.addend(), 0);
                    assert!(relocs.iter().any(|(offset, high)| {
                        *offset == label.address()
                            && high.flags()
                                == (RelocationFlags::Elf {
                                    r_type: elf::R_RISCV_GOT_HI20,
                                })
                    }));
                    pairs += 1;
                }
                assert!(
                    pairs > 0,
                    "{triple} {name} did not emit a GOT-relative pair"
                );
            }
        }
    }
}

#[cfg(not(feature = "llvm"))]
#[test]
fn llvm_aot_reports_unavailable_without_llvm_feature() {
    assert!(!AotBackendKind::Llvm.is_available());
    let file = compiled_file_with_nested_sum_loop();
    let config = AotTargetConfig::host("llvm-mod").with_backend(AotBackendKind::Llvm);
    assert!(matches!(
        compile_fasl_to_object(&file, &config),
        Err(zkcl_aot::AotError::LlvmUnavailable)
    ));
}

#[cfg(feature = "cranelift")]
#[test]
fn windows_runtime_calls_relocate_directly_to_runtime_symbols() {
    use object::{RelocationFlags, RelocationTarget, pe};

    for (triple, expected) in [
        ("aarch64-pc-windows-msvc", pe::IMAGE_REL_ARM64_BRANCH26),
        ("x86_64-pc-windows-msvc", pe::IMAGE_REL_AMD64_REL32),
    ] {
        let config = AotTargetConfig::host("win").with_target(triple.parse().unwrap());
        let file = compiled_file_for_target(&config.target);
        let object = compile_fasl_to_object(&file, &config).expect(triple);
        let parsed = object::File::parse(object.object_bytes.as_slice()).unwrap();
        assert_eq!(parsed.format(), BinaryFormat::Coff);
        let text = parsed.section_by_name(".text").unwrap();
        let mut saw_safepoint = false;
        for (_, relocation) in text.relocations() {
            let RelocationTarget::Symbol(index) = relocation.target() else {
                panic!("unexpected relocation target for {triple}");
            };
            let symbol = parsed.symbol_by_index(index).unwrap();
            let name = symbol.name().unwrap();
            assert!(
                symbol.is_undefined() && name.starts_with("zkcl_rt_"),
                "{name}"
            );
            assert_eq!(
                relocation.flags(),
                RelocationFlags::Coff { typ: expected },
                "{triple} {name}"
            );
            saw_safepoint |= name == "zkcl_rt_poll_execution";
        }
        assert!(
            saw_safepoint,
            "{triple} missing the loop's runtime safepoint call"
        );
    }
}

#[cfg(feature = "llvm")]
#[test]
fn llvm_keeps_unsupported_entries_in_the_embedded_fasl() {
    let mut file = compiled_file_with_nested_sum_loop();
    let mut vm_entry = code_without_side_tables(
        Arity::ZERO,
        0,
        vec![Instruction::ProclaimSpecial { symbol: 0, at: 0 }],
    );
    vm_entry.constants.push(2);
    file.entries.insert(1, vm_entry);
    let config = AotTargetConfig::host("mixed").with_backend(AotBackendKind::Llvm);
    let compiled = compile_fasl_to_object(&file, &config).unwrap();
    let indices: Vec<_> = compiled
        .registration_entries
        .iter()
        .map(|entry| entry.entry_index)
        .collect();
    assert_eq!(indices, [0, 2, 3]);
    assert!(
        !compiled
            .llvm_ir
            .as_deref()
            .unwrap()
            .contains("@zkcl_aot_mixed_fn_1(")
    );
    assert_eq!(CompiledFile::decode(&compiled.fasl_bytes).unwrap(), file);
}

#[cfg(feature = "llvm")]
#[test]
fn llvm_lowers_named_calls_inside_a_polling_loop() {
    assert!(AotBackendKind::Llvm.is_available());
    let file = compiled_file_with_nested_sum_loop();
    let config = AotTargetConfig::host("llvm-mod")
        .with_backend(AotBackendKind::Llvm)
        .with_opt_level(AotOptLevel::Speed);
    let object = compile_fasl_to_object(&file, &config).expect("LLVM AOT compilation succeeds");

    let ir = object
        .llvm_ir
        .as_deref()
        .expect("LLVM IR text must be populated");
    assert!(ir.contains("target datalayout ="));
    assert!(ir.contains("target triple ="));
    assert!(ir.contains("phi "));
    assert!(ir.contains("br "));
    assert!(ir.contains("@zkcl_rt_invoke"));
    assert!(ir.contains("@zkcl_rt_alloc_root_frame"));
    assert!(ir.contains("@zkcl_rt_function_constant"));
    assert!(
        ir.contains(&format!("call i{} @zkcl_rt_call_primary(", usize::BITS)),
        "primary results from small calls use the scalar ABI"
    );
    assert!(
        ir.contains("call void @zkcl_rt_call_named("),
        "the hook call keeps the values-preserving array ABI"
    );
    assert!(
        ir.contains("call void @zkcl_rt_poll_execution("),
        "LLVM AOT loop backedges must poll the execution safepoint"
    );
    assert!(ir.contains("@zkcl_aot_llvm_mod_fasl = constant"));

    let parsed = object::File::parse(object.object_bytes.as_slice())
        .expect("LLVM output .o must parse cleanly");
    assert_eq!(object.registration_entries.len(), 3);
    for entry in &object.registration_entries {
        let symbol = find_defined_symbol(&parsed, &entry.symbol_name)
            .unwrap_or_else(|| panic!("missing LLVM-compiled symbol {}", entry.symbol_name));
        assert_eq!(symbol.kind(), SymbolKind::Text);
    }
    assert!(find_defined_symbol(&parsed, "zkcl_aot_llvm_mod_fasl").is_some());
}

#[cfg(feature = "llvm")]
#[test]
fn llvm_grows_one_scratch_array_for_successive_runtime_calls() {
    let mut code = code_without_side_tables(
        Arity::ZERO,
        4,
        vec![
            Instruction::Load(0, Immediate::Fixnum(1)),
            Instruction::Load(1, Immediate::Fixnum(2)),
            Instruction::Load(2, Immediate::Fixnum(3)),
            Instruction::Call {
                callee: Callee::Named(0),
                out: Some(3),
                args: 0..1,
                at: 3,
                associate: false,
                notinline: true,
            },
            Instruction::Call {
                callee: Callee::Named(0),
                out: None,
                args: 0..3,
                at: 4,
                associate: false,
                notinline: true,
            },
        ],
    );
    code.constants.push(0);
    let file = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: vec![],
        nodes: vec![LiteralNode::InternedSymbol {
            package: "COMMON-LISP-USER".into(),
            name: "HOOK".into(),
        }],
        entries: vec![code],
    };
    let config = AotTargetConfig::host("shared-scratch").with_backend(AotBackendKind::Llvm);
    let object = compile_fasl_to_object(&file, &config).unwrap();
    let ir = object.llvm_ir.unwrap();
    let allocas: Vec<_> = ir
        .lines()
        .filter(|line| line.trim_start().starts_with("%call_words"))
        .collect();
    assert_eq!(allocas.len(), 1, "{allocas:?}");
    let width = usize::BITS;
    assert!(allocas[0].contains(&format!("alloca i{width}, i{width} 3")));
    assert!(ir.contains("%restore_registers = alloca i32"));
}

#[cfg(feature = "llvm")]
#[test]
fn llvm_checks_fixnum_product_overflow_at_the_target_word_width() {
    for (triple, width) in [
        ("i686-unknown-linux-gnu", 32),
        ("x86_64-unknown-linux-gnu", 64),
    ] {
        let config = AotTargetConfig::host("product")
            .with_target(triple.parse().unwrap())
            .with_backend(AotBackendKind::Llvm);
        let mut code = code_without_side_tables(
            Arity {
                required: 2,
                maximum: Some(2),
            },
            3,
            vec![
                Instruction::TryFixnumArithmetic {
                    op: zkcl_bytecode::FixnumArithmetic::Mul,
                    symbol: 0,
                    out: 2,
                    args: 0..2,
                },
                Instruction::SetValues(2..3),
            ],
        );
        code.constants.push(0);
        let file = CompiledFile {
            target: triple.parse().unwrap(),
            source_files: Vec::new(),
            nodes: vec![
                LiteralNode::InternedSymbol {
                    package: "COMMON-LISP".into(),
                    name: "*".into(),
                },
                LiteralNode::Function(code),
            ],
            entries: Vec::new(),
        };
        let object = compile_fasl_to_object(&file, &config).unwrap();
        assert_eq!(object.registration_entries.len(), 1);
        let ir = object.llvm_ir.unwrap();
        assert!(ir.contains(&format!(
            "call {{ i{width}, i1 }} @llvm.smul.with.overflow.i{width}("
        )));
        assert!(ir.contains("declare zeroext i1 @zkcl_rt_is_primitive_binding("));
        assert!(ir.contains("call zeroext i1 @zkcl_rt_is_primitive_binding("));
    }
}

#[cfg(feature = "llvm")]
#[test]
fn llvm_branches_on_a_fixnum_range_check() {
    let config = AotTargetConfig::host("fixnum-range").with_backend(AotBackendKind::Llvm);
    let code = code_without_side_tables(
        Arity {
            required: 1,
            maximum: Some(1),
        },
        2,
        vec![
            Instruction::UnlessFixnumInRange(0, -4, 9, 4),
            Instruction::Load(1, Immediate::True),
            Instruction::SetValues(1..2),
            Instruction::Jump(6),
            Instruction::Load(1, Immediate::Nil),
            Instruction::SetValues(1..2),
        ],
    );
    let file = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![LiteralNode::Function(code)],
        entries: Vec::new(),
    };

    let object = compile_fasl_to_object(&file, &config).unwrap();
    assert_eq!(object.registration_entries.len(), 1);
}

#[cfg(feature = "llvm")]
#[test]
fn llvm_object_headers_match_target_triples() {
    let linux_triple = Triple::from_str("x86_64-unknown-linux-gnu").unwrap();
    let linux_cfg = AotTargetConfig::host("llvm-x64")
        .with_target(linux_triple)
        .with_backend(AotBackendKind::Llvm)
        .with_opt_level(AotOptLevel::Aggressive);
    let linux_object =
        compile_fasl_to_object(&compiled_file_for_target(&linux_cfg.target), &linux_cfg)
            .expect("LLVM x86_64 ELF succeeds");
    let linux_ir = linux_object.llvm_ir.as_deref().expect("LLVM IR text");
    assert!(
        linux_ir.contains("declare void @zkcl_rt_poll_execution(ptr, i64, ptr)"),
        "x86_64 safepoint poll must use the 64-bit native ABI"
    );
    let linux_obj =
        object::File::parse(linux_object.object_bytes.as_slice()).expect("parse LLVM ELF");
    assert_eq!(linux_obj.format(), BinaryFormat::Elf);
    assert_eq!(linux_obj.architecture(), Architecture::X86_64);
    assert!(find_defined_symbol(&linux_obj, "zkcl_aot_llvm_x64_fn_2").is_some());
    let fasl_symbol = find_defined_symbol(&linux_obj, "zkcl_aot_llvm_x64_fasl")
        .expect("ELF embedded FASL symbol");
    assert_eq!(fasl_symbol.kind(), SymbolKind::Data);
    let fasl_section = linux_obj
        .section_by_index(fasl_symbol.section_index().expect("FASL symbol is defined"))
        .expect("read FASL symbol section");
    assert_eq!(fasl_section.kind(), SectionKind::ReadOnlyData);
    assert_eq!(
        fasl_section
            .data_range(fasl_symbol.address(), fasl_symbol.size())
            .expect("read FASL symbol range")
            .expect("embedded FASL payload"),
        linux_object.fasl_bytes.as_slice()
    );

    for (triple, level, format, arch) in [
        (
            "i686-unknown-linux-gnu",
            AotOptLevel::Speed,
            BinaryFormat::Elf,
            Architecture::I386,
        ),
        (
            "aarch64-apple-ios",
            AotOptLevel::Size,
            BinaryFormat::MachO,
            Architecture::Aarch64,
        ),
        (
            "x86_64-pc-windows-msvc",
            AotOptLevel::None,
            BinaryFormat::Coff,
            Architecture::X86_64,
        ),
    ] {
        let config = AotTargetConfig::host("cross")
            .with_target(triple.parse().unwrap())
            .with_backend(AotBackendKind::Llvm)
            .with_opt_level(level);
        let object =
            compile_fasl_to_object(&compiled_file_for_target(&config.target), &config).unwrap();
        let parsed = object::File::parse(object.object_bytes.as_slice()).unwrap();
        assert_eq!((parsed.format(), parsed.architecture()), (format, arch));
        assert!(find_defined_symbol(&parsed, "zkcl_aot_cross_fn_2").is_some());
        if arch == Architecture::I386 {
            assert!(
                object
                    .llvm_ir
                    .as_deref()
                    .unwrap()
                    .contains("declare void @zkcl_rt_poll_execution(ptr, i32, ptr)")
            );
        }
    }
}

#[test]
fn available_backends_record_the_macho_platform_and_deployment_target() {
    use object::macho::{
        PLATFORM_IOS, PLATFORM_IOSSIMULATOR, PLATFORM_MACCATALYST, PLATFORM_MACOS,
    };
    for (triple, platform, minos) in [
        ("aarch64-apple-darwin", PLATFORM_MACOS, 0x000e_0000),
        ("x86_64-apple-macosx", PLATFORM_MACOS, 0x000e_0000),
        ("aarch64-apple-darwin22.0.0", PLATFORM_MACOS, 0x000d_0000),
        ("aarch64-apple-darwin25.1.0", PLATFORM_MACOS, 0x001a_0000),
        ("aarch64-apple-macosx13.3.1", PLATFORM_MACOS, 0x000d_0301),
        ("aarch64-apple-ios16.1.2", PLATFORM_IOS, 0x0010_0102),
        (
            "aarch64-apple-ios16.1.2-sim",
            PLATFORM_IOSSIMULATOR,
            0x0010_0102,
        ),
        ("x86_64-apple-ios16.1.2", PLATFORM_IOSSIMULATOR, 0x0010_0102),
        (
            "aarch64-apple-ios16.1.2-macabi",
            PLATFORM_MACCATALYST,
            0x0010_0102,
        ),
    ] {
        let config = AotTargetConfig::host("deployment").with_target(triple.parse().unwrap());
        let file = CompiledFile {
            target: zkcl_target::CompilationTarget::from_triple(config.target.clone()).unwrap(),
            source_files: Vec::new(),
            nodes: vec![],
            entries: vec![],
        };
        for backend in [
            AotBackendKind::Direct,
            AotBackendKind::CraneliftObject,
            AotBackendKind::Llvm,
        ] {
            if !backend.is_available() {
                continue;
            }
            let config = config.clone().with_backend(backend);
            let object = compile_fasl_to_object(&file, &config).unwrap();
            let macho = object::read::macho::MachOFile64::<object::Endianness>::parse(
                object.object_bytes.as_slice(),
            )
            .unwrap();
            let version = macho.build_version().unwrap().expect(triple);
            assert_eq!(
                version.platform.get(macho.endian()),
                platform,
                "{backend:?} {triple}"
            );
            assert_eq!(
                version.minos.get(macho.endian()),
                minos,
                "{backend:?} {triple}"
            );
        }
    }
}

#[cfg(feature = "cranelift")]
#[test]
fn cranelift_registration_exposes_entries_used_by_standalone_main() {
    let file = compiled_file_with_nested_sum_loop();
    let config = AotTargetConfig::host("demo-app");
    let object = compile_fasl_to_object(&file, &config).expect("AOT compilation succeeds");

    assert!(
        object
            .registration_source
            .contains("pub static AOT_MODULE: AotModuleDescriptor")
    );
    assert!(
        object
            .registration_source
            .contains("pub static AOT_CODES: &[AotCodeDescriptor]")
    );
    assert!(
        object
            .registration_source
            .contains("AbiFingerprint::CURRENT")
    );
    assert!(
        object
            .registration_source
            .contains("AotModuleDescriptor::new(\"demo-app\"")
    );
    assert!(
        object
            .registration_source
            .contains("zkcl_aot_demo_app_fn_0")
    );
    assert!(
        object
            .registration_source
            .contains("zkcl_aot_demo_app_fn_1")
    );
    assert!(
        object
            .registration_source
            .contains("zkcl_aot_demo_app_fn_2")
    );
    assert!(
        object
            .registration_source
            .contains("zkcl_aot_demo_app_fasl")
    );
    assert!(!object.registration_source.contains("zkcl_rt_invoke"));
    assert!(
        object
            .registration_source
            .contains("out_ret: *mut JitReturn")
    );
    assert!(
        object
            .registration_source
            .contains("generic_entry: jit_c_trampoline")
    );
    assert!(
        object
            .registration_source
            .contains("c_entry: Some(zkcl_aot_demo_app_fn_0)")
    );
    assert!(!object.registration_source.contains("__zkcl_aot_c_entry_"));
    assert!(object.registration_source.contains("entry_index: 0"));
    assert!(object.registration_source.contains("entry_index: 1"));
    assert!(object.registration_source.contains("entry_index: 2"));

    assert!(
        object
            .standalone_main_source
            .contains("zkcl::Runtime::start()")
    );
    assert!(
        object
            .standalone_main_source
            .contains("runtime.load_aot_module(&aot_module::AOT_MODULE)")
    );
}

#[test]
fn standalone_main_only_uses_native_entries_for_the_matching_fasl() {
    let with_include = generate_standalone_main_source("demo-app", Some("target/demo.fasl"));
    assert!(with_include.contains("include_bytes!(\"target/demo.fasl\")"));
    assert!(with_include.contains("EMBEDDED_FASL_OVERRIDE == aot_module::FASL_BYTES"));
    assert!(with_include.contains("runtime.load_aot_module(&aot_module::AOT_MODULE)"));
    assert!(with_include.contains("runtime.load_fasl(EMBEDDED_FASL_OVERRIDE)"));
}
