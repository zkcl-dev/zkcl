use zkcl_bytecode::{
    Arity, ArrayContents, ArrayElement, BytecodeFunction, CompiledFile, CompiledFileError, Handler,
    HandlerTable, HandlerTarget, HashTableTest, Immediate, Instruction, JumpTable, KeywordTable,
    LiteralNode, RestartSpec, RestartTable, RestartTarget, Test, ValidationError,
};

const EXPECTED_FASL_MAGIC: &[u8] = b"\x89ZKCLFASL";

fn compiled_file_with_cycle() -> CompiledFile {
    CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: vec!["example.lisp".into()],
        nodes: vec![
            LiteralNode::UninternedSymbol("fresh".into()),
            LiteralNode::Pair(0, 1),
            LiteralNode::Function(BytecodeFunction {
                name: Some("CYCLE-FUNCTION".into()),
                source_file: Some(0),
                debug: Default::default(),
                arity: Arity::ZERO,
                registers: 1,
                instructions: vec![
                    Instruction::Load(0, Immediate::Character('λ')),
                    Instruction::SetValues(0..1),
                ],
                constants: vec![],
                lambda_list: None,
                captures: vec![],
                notinline: vec![],
                vop_calls: Vec::new(),
                handlers: vec![],
                restarts: vec![],
                keywords: vec![],
                jump_tables: vec![],
                facts: Default::default(),
            }),
        ],
        entries: vec![BytecodeFunction {
            name: None,
            source_file: Some(0),
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 2,
            instructions: vec![
                Instruction::Constant(0, 0),
                Instruction::Constant(1, 1),
                Instruction::SetValues(0..2),
            ],
            constants: vec![1, 2],
            lambda_list: None,
            captures: vec![],
            notinline: vec![],
            vop_calls: Vec::new(),
            handlers: vec![],
            restarts: vec![],
            keywords: vec![],
            jump_tables: vec![],
            facts: Default::default(),
        }],
    }
}

fn expected_fasl_header() -> Vec<u8> {
    EXPECTED_FASL_MAGIC.to_vec()
}

#[test]
fn round_trip_preserves_cyclic_literal_references() {
    let file = compiled_file_with_cycle();
    let bytes = file.encode().unwrap();
    assert!(bytes.starts_with(&expected_fasl_header()));
    assert!(CompiledFile::has_magic(&bytes));
    assert_eq!(CompiledFile::decode(&bytes).unwrap(), file);
}

#[test]
fn javascript_fasl_keeps_its_target_and_rejects_machine_runtimes() {
    let mut file = compiled_file_with_cycle();
    let target = zkcl_target::CompilationTarget::javascript();
    file.target = target.clone();
    let bytes = file.encode().unwrap();
    let restored = CompiledFile::decode_for_target(&bytes, &target).unwrap();
    assert_eq!(restored, file);
    assert!(matches!(
        CompiledFile::decode_for_target(&bytes, &"wasm32-unknown-unknown".parse().unwrap()),
        Err(CompiledFileError::TargetMismatch { .. })
    ));
    assert!(matches!(
        CompiledFile::decode_for_target(&bytes, &zkcl_target::CompilationTarget::native()),
        Err(CompiledFileError::TargetMismatch { .. })
    ));
}

#[test]
fn verifies_literal_and_immediate_fixnums_against_the_stored_target() {
    let mut file = compiled_file_with_cycle();
    file.target = "i686-unknown-linux-gnu".parse().unwrap();
    let (min, max) = file.target.fixnum_bounds();
    let node = file.nodes.len();
    file.nodes.push(LiteralNode::Fixnum(min));
    file.entries[0].instructions[0] = Instruction::Load(0, Immediate::Fixnum(max));
    file.encode().unwrap();

    file.nodes[node] = LiteralNode::Fixnum(min - 1);
    assert!(file.encode().is_err());
    file.nodes[node] = LiteralNode::Fixnum(min);
    file.entries[0].instructions[0] = Instruction::Load(0, Immediate::Fixnum(max + 1));
    assert!(file.encode().is_err());
    file.target = "x86_64-unknown-linux-gnu".parse().unwrap();
    file.nodes[node] = LiteralNode::Fixnum(min - 1);
    file.encode().unwrap();
}

#[test]
fn decode_rejects_input_without_fasl_magic() {
    assert!(!CompiledFile::has_magic(b"ZKCLFASL"));
    assert!(matches!(
        CompiledFile::decode(b"(progn)"),
        Err(CompiledFileError::InvalidMagic)
    ));
}

#[test]
fn decode_rejects_bytes_after_the_payload() {
    let mut bytes = compiled_file_with_cycle().encode().unwrap();
    bytes.push(0);
    assert!(matches!(
        CompiledFile::decode(&bytes),
        Err(CompiledFileError::TrailingBytes(1))
    ));
}

#[test]
fn decode_rejects_a_truncated_payload() {
    let mut bad_count = expected_fasl_header();
    bad_count.push(0x80);
    assert!(matches!(
        CompiledFile::decode(&bad_count),
        Err(CompiledFileError::Decode(_))
    ));
}

#[test]
fn rejects_out_of_bounds_literal_reference() {
    let mut invalid = compiled_file_with_cycle();
    invalid.nodes[1] = LiteralNode::Pair(0, 99);
    let bytes = postcard::to_extend(&invalid, expected_fasl_header()).unwrap();
    assert!(matches!(
        CompiledFile::decode(&bytes),
        Err(CompiledFileError::Validation(_))
    ));
    assert!(matches!(
        invalid.encode(),
        Err(CompiledFileError::Validation(_))
    ));
}

#[test]
fn invalid_literal_errors_preserve_the_validation_source() {
    use std::error::Error as _;

    let mut invalid = compiled_file_with_cycle();
    invalid.nodes[1] = LiteralNode::Pair(0, 99);
    let bytes = postcard::to_extend(&invalid, expected_fasl_header()).unwrap();
    for error in [
        invalid.encode().unwrap_err(),
        CompiledFile::decode(&bytes).unwrap_err(),
    ] {
        let source = error
            .source()
            .and_then(|source| source.downcast_ref::<ValidationError>())
            .expect("compiled-file errors retain the typed validation error");
        assert_eq!(source.to_string(), error.to_string());
        assert_eq!(
            source.to_string(),
            "literal node 1: literal reference is out of bounds"
        );
    }
}

fn file_containing_a_load_form_in_a_hash_entry() -> CompiledFile {
    let mut file = compiled_file_with_cycle();
    file.nodes.extend([
        LiteralNode::HashTable {
            test: HashTableTest::Equalp,
            weakness: Some(zkcl_bytecode::HashTableWeakness::Key),
            rehash_size: zkcl_bytecode::RehashHint::integer(8),
            rehash_threshold: zkcl_bytecode::RehashHint::integer(1),
            entries: vec![(0, 4)],
        },
        LiteralNode::LoadForm {
            creation: 0,
            initialization: None,
        },
    ]);
    file
}

#[test]
fn round_trip_preserves_a_weak_hash_table_literal() {
    let mut file = file_containing_a_load_form_in_a_hash_entry();
    let LiteralNode::HashTable {
        rehash_size,
        rehash_threshold,
        ..
    } = &mut file.nodes[3]
    else {
        unreachable!()
    };
    *rehash_size = zkcl_bytecode::RehashHint::float(1.5).unwrap();
    *rehash_threshold = zkcl_bytecode::RehashHint::float(0.5).unwrap();
    let bytes = file.encode().unwrap();
    assert_eq!(CompiledFile::decode(&bytes).unwrap(), file);
    for (size, threshold) in [(1.0, 0.5), (1.5, 1.5)] {
        let LiteralNode::HashTable {
            rehash_size,
            rehash_threshold,
            ..
        } = &mut file.nodes[3]
        else {
            unreachable!()
        };
        *rehash_size = zkcl_bytecode::RehashHint::float(size).unwrap();
        *rehash_threshold = zkcl_bytecode::RehashHint::float(threshold).unwrap();
        assert!(file.validate().is_err());
    }
}

#[test]
fn load_form_creation_must_precede_a_reference_through_a_hash_table() {
    let mut file = file_containing_a_load_form_in_a_hash_entry();
    file.entries.push(file.entries[0].clone());
    file.entries[1].constants[0] = 3;
    file.validate().unwrap();
    file.entries[0].constants[0] = 3;
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("before its load form")
    );
}

#[test]
fn load_form_initialization_must_follow_creation() {
    let mut file = file_containing_a_load_form_in_a_hash_entry();
    file.entries.push(file.entries[0].clone());
    file.entries[1].constants[0] = 3;
    file.nodes[4] = LiteralNode::LoadForm {
        creation: 1,
        initialization: Some(0),
    };
    assert!(matches!(
        file.encode(),
        Err(CompiledFileError::Validation(_))
    ));
}

#[test]
fn hash_table_entries_must_reference_existing_literals() {
    let mut file = file_containing_a_load_form_in_a_hash_entry();
    file.nodes[4] = LiteralNode::Nil;
    file.nodes[3] = LiteralNode::HashTable {
        test: HashTableTest::Eq,
        weakness: None,
        rehash_size: zkcl_bytecode::RehashHint::integer(8),
        rehash_threshold: zkcl_bytecode::RehashHint::integer(1),
        entries: vec![(99, 0)],
    };
    assert!(matches!(
        file.encode(),
        Err(CompiledFileError::Validation(_))
    ));
}

#[test]
fn validates_numeric_literal_shapes() {
    let mut file = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::Bignum {
                negative: false,
                magnitude: vec![0, 0, 0, 0, 0, 0, 0, 128],
            },
            LiteralNode::Fixnum(3),
            LiteralNode::Ratio(0, 1),
            LiteralNode::DoubleFloat((-0.0_f64).to_bits()),
            LiteralNode::DoubleFloat(1.5_f64.to_bits()),
            LiteralNode::Complex(4, 3),
        ],
        entries: vec![],
    };
    let bytes = file.encode().unwrap();
    assert_eq!(CompiledFile::decode(&bytes).unwrap(), file);
    file.nodes[5] = LiteralNode::Complex(4, 2);
    assert!(matches!(
        file.encode(),
        Err(CompiledFileError::Validation(_))
    ));
    file.nodes[5] = LiteralNode::Complex(4, 3);
    file.nodes[2] = LiteralNode::Ratio(0, 4);
    assert!(matches!(
        file.encode(),
        Err(CompiledFileError::Validation(_))
    ));
    file.nodes[2] = LiteralNode::Ratio(0, 1);
    file.nodes[3] = LiteralNode::DoubleFloat(f64::INFINITY.to_bits());
    assert!(matches!(
        file.encode(),
        Err(CompiledFileError::Validation(_))
    ));
}

fn literal_array(
    dimensions: Vec<usize>,
    element: ArrayElement,
    fill_pointer: Option<usize>,
    contents: ArrayContents,
) -> LiteralNode {
    LiteralNode::Array {
        dimensions,
        element,
        adjustable: false,
        fill_pointer,
        contents,
    }
}

fn displaced_general_array_file() -> CompiledFile {
    let base = literal_array(
        vec![2],
        ArrayElement::General,
        Some(1),
        ArrayContents::Elements(vec![1, 2]),
    );
    let view = literal_array(
        vec![1],
        ArrayElement::General,
        None,
        ArrayContents::Displaced {
            target: 0,
            offset: 1,
        },
    );
    CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![base, view, LiteralNode::Nil],
        entries: vec![],
    }
}

#[test]
fn round_trip_preserves_displacement_into_a_general_array() {
    let file = displaced_general_array_file();
    let bytes = file.encode().unwrap();
    assert_eq!(CompiledFile::decode(&bytes).unwrap(), file);
}

#[test]
fn rejects_a_displaced_array_that_targets_itself() {
    let mut file = displaced_general_array_file();
    file.nodes[0] = literal_array(
        vec![2],
        ArrayElement::General,
        Some(1),
        ArrayContents::Displaced {
            target: 0,
            offset: 0,
        },
    );
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("circular array displacement")
    );
}

#[test]
fn bit_array_elements_must_be_bits() {
    let mut file = displaced_general_array_file();
    file.nodes[0] = literal_array(
        vec![2],
        ArrayElement::Bit,
        None,
        ArrayContents::Elements(vec![1, 2]),
    );
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("only bits")
    );
}

#[test]
fn rejects_a_fill_pointer_on_a_rank_two_array() {
    let mut file = displaced_general_array_file();
    file.nodes[0] = literal_array(
        vec![2, 1],
        ArrayElement::General,
        Some(1),
        ArrayContents::Elements(vec![1, 2]),
    );
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("invalid array fill pointer")
    );
}

#[test]
fn validates_inline_float_arrays() {
    let array = |element, contents| LiteralNode::Array {
        dimensions: vec![2],
        element,
        adjustable: false,
        fill_pointer: None,
        contents,
    };
    let single = ArrayElement::SingleFloat;
    let double = ArrayElement::DoubleFloat;
    let mut file = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            array(
                single,
                ArrayContents::SingleFloats(vec![(-0.0f32).to_bits(), 1]),
            ),
            array(
                double,
                ArrayContents::DoubleFloats(vec![(-0.0f64).to_bits(), 1]),
            ),
        ],
        entries: vec![],
    };
    assert_eq!(CompiledFile::decode(&file.encode().unwrap()).unwrap(), file);

    let mut references = vec![];
    for node in &file.nodes {
        node.push_references(&mut references);
    }
    assert!(references.is_empty());

    for (element, contents, expected) in [
        (single, ArrayContents::Elements(vec![0, 1]), "do not match"),
        (
            double,
            ArrayContents::SingleFloats(vec![0, 1]),
            "do not match",
        ),
        (
            single,
            ArrayContents::DoubleFloats(vec![0, 1]),
            "do not match",
        ),
        (
            double,
            ArrayContents::DoubleFloats(vec![0]),
            "element count",
        ),
        (
            single,
            ArrayContents::SingleFloats(vec![0, f32::INFINITY.to_bits()]),
            "must be finite",
        ),
        (
            double,
            ArrayContents::DoubleFloats(vec![0, f64::NAN.to_bits()]),
            "must be finite",
        ),
        (
            single,
            ArrayContents::Displaced {
                target: 1,
                offset: 0,
            },
            "element types must match",
        ),
    ] {
        file.nodes[0] = array(element, contents);
        assert!(file.validate().unwrap_err().to_string().contains(expected));
    }
}

fn character_array_view(element: ArrayElement, target: u32, offset: usize) -> LiteralNode {
    LiteralNode::Array {
        dimensions: vec![1],
        element,
        adjustable: true,
        fill_pointer: Some(0),
        contents: ArrayContents::Displaced { target, offset },
    }
}

fn displaced_character_array_file() -> CompiledFile {
    CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::String("猫λ".into()),
            character_array_view(ArrayElement::Character, 0, 1),
            character_array_view(ArrayElement::Character, 1, 0),
            LiteralNode::Character('🦀'),
            LiteralNode::Array {
                dimensions: vec![1, 1],
                element: ArrayElement::Character,
                adjustable: false,
                fill_pointer: None,
                contents: ArrayContents::Elements(vec![3]),
            },
        ],
        entries: vec![],
    }
}

#[test]
fn round_trip_preserves_a_character_array_displaced_into_a_string() {
    let file = displaced_character_array_file();
    let bytes = file.encode().unwrap();
    assert_eq!(CompiledFile::decode(&bytes).unwrap(), file);
}

#[test]
fn string_displacement_bounds_are_measured_in_characters() {
    let mut file = displaced_character_array_file();
    file.nodes[1] = character_array_view(ArrayElement::Character, 0, 2);
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("does not fit in its target")
    );
}

#[test]
fn string_displacement_requires_a_character_array() {
    let mut file = displaced_character_array_file();
    file.nodes[1] = character_array_view(ArrayElement::General, 0, 1);
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("element types must match")
    );
}

#[test]
fn character_array_elements_must_be_characters() {
    let mut file = displaced_character_array_file();
    file.nodes[3] = LiteralNode::Fixnum(0);
    assert!(
        file.validate()
            .unwrap_err()
            .to_string()
            .contains("only characters")
    );
}

#[test]
fn round_trip_preserves_handler_restart_keyword_and_jump_tables() {
    use Instruction::*;
    let file = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::InternedSymbol {
                package: "KEYWORD".into(),
                name: "ALLOW-OTHER-KEYS".into(),
            },
            LiteralNode::InternedSymbol {
                package: "KEYWORD".into(),
                name: "KEY".into(),
            },
            LiteralNode::UninternedSymbol("ABORT".into()),
        ],
        entries: vec![BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 4,
            instructions: vec![
                EnterHandlers { table: 0 },
                ExitToken(0, 0),
                EnterTagbody {
                    token: 0,
                    jumps: 0,
                    clear: 1,
                },
                EnterRestarts { table: 0, at: 0 },
                CheckKeywords { start: 2, table: 0 },
                DestructureKeywords {
                    cursor: 1,
                    table: 0,
                    out: 2..3,
                    macro_call: false,
                    at: 0,
                },
                LeaveRestarts,
                LeaveTarget,
                LeaveHandlers,
                Jump(11),
                SetValues(2..3),
            ],
            constants: vec![0, 1, 2],
            lambda_list: None,
            captures: vec![],
            notinline: vec![],
            vop_calls: Vec::new(),
            handlers: vec![HandlerTable {
                handlers: vec![Handler {
                    test: Test::Any,
                    target: HandlerTarget::Case {
                        pc: 10,
                        condition: 2,
                        clear: 3,
                    },
                }],
            }],
            restarts: vec![RestartTable {
                restarts: vec![RestartSpec {
                    name: 2,
                    object: 1,
                    test: None,
                    interactive: None,
                    report: None,
                    target: RestartTarget::Function(1),
                }],
            }],
            keywords: vec![KeywordTable {
                names: vec![1],
                allow_other_keys: false,
                override_key: 0,
            }],
            jump_tables: vec![JumpTable { targets: vec![7] }],
            facts: Default::default(),
        }],
    };
    let bytes = file.encode().unwrap();
    assert_eq!(CompiledFile::decode(&bytes).unwrap(), file);
}

#[test]
fn validates_ratio_coprimality_including_large_bignums() {
    let mut non_coprime_fixnum = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::Fixnum(-6),
            LiteralNode::Fixnum(9),
            LiteralNode::Ratio(0, 1),
        ],
        entries: vec![],
    };
    assert!(non_coprime_fixnum.validate().is_err());
    non_coprime_fixnum.nodes[0] = LiteralNode::Fixnum(-7);
    non_coprime_fixnum.validate().unwrap();

    // Two >128-bit integers sharing factor 3: (2^130 - 1) / 3 vs 3 * 2^128.
    let mut mag_a = vec![0xffu8; 17];
    mag_a[16] = 0x03; // 2^130 - 1 (divisible by 3)
    let mut mag_b = vec![0u8; 17];
    mag_b[16] = 3; // 3 * 2^128 (divisible by 3)
    let mut large_non_coprime = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::Bignum {
                negative: true,
                magnitude: mag_a.clone(),
            },
            LiteralNode::Bignum {
                negative: false,
                magnitude: mag_b,
            },
            LiteralNode::Ratio(0, 1),
        ],
        entries: vec![],
    };
    assert!(large_non_coprime.validate().is_err());

    // Coprime >128-bit pair: (2^130 - 1) and 2^128.
    let mut mag_pow2 = vec![0u8; 17];
    mag_pow2[16] = 1;
    large_non_coprime.nodes[1] = LiteralNode::Bignum {
        negative: false,
        magnitude: mag_pow2,
    };
    large_non_coprime.validate().unwrap();
}

#[test]
fn validates_pathname_component_invariants() {
    let kw = |name: &str| LiteralNode::InternedSymbol {
        package: "KEYWORD".into(),
        name: name.into(),
    };
    // Valid physical pathname: (:ABSOLUTE "tmp") "file" "lisp" :NEWEST
    let physical = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::Nil,                   // 0: nil
            kw("UNSPECIFIC"),                   // 1
            kw("ABSOLUTE"),                     // 2
            LiteralNode::String("tmp".into()),  // 3
            LiteralNode::Pair(3, 0),            // 4
            LiteralNode::Pair(2, 4),            // 5: (:ABSOLUTE "tmp")
            LiteralNode::String("file".into()), // 6
            LiteralNode::String("lisp".into()), // 7
            kw("NEWEST"),                       // 8
            LiteralNode::Pathname([0, 1, 5, 6, 7, 8]),
        ],
        entries: vec![],
    };
    physical.validate().unwrap();

    // Valid logical pathname: "SYS" :UNSPECIFIC (:ABSOLUTE "SRC") "MAIN" "LISP" 1
    let logical = CompiledFile {
        target: zkcl_target::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: vec![
            LiteralNode::Nil,                   // 0
            LiteralNode::String("SYS".into()),  // 1: logical host
            kw("UNSPECIFIC"),                   // 2
            kw("ABSOLUTE"),                     // 3
            LiteralNode::String("SRC".into()),  // 4
            LiteralNode::Pair(4, 0),            // 5
            LiteralNode::Pair(3, 5),            // 6
            LiteralNode::String("MAIN".into()), // 7
            LiteralNode::String("LISP".into()), // 8
            LiteralNode::Fixnum(1),             // 9
            LiteralNode::Pathname([1, 2, 6, 7, 8, 9]),
        ],
        entries: vec![],
    };
    logical.validate().unwrap();

    // Reject lowercase logical name, non-unspecific logical device, invalid physical segment, circular directory.
    let mut bad_logical_case = logical.clone();
    bad_logical_case.nodes[7] = LiteralNode::String("main".into());
    assert!(bad_logical_case.validate().is_err());

    let mut bad_logical_device = logical.clone();
    bad_logical_device.nodes[10] = LiteralNode::Pathname([1, 0, 6, 7, 8, 9]);
    assert!(bad_logical_device.validate().is_err());

    let mut bad_segment = physical.clone();
    bad_segment.nodes[6] = LiteralNode::String("a/b".into());
    assert!(bad_segment.validate().is_err());

    let mut circular_dir = physical.clone();
    circular_dir.nodes[4] = LiteralNode::Pair(3, 4);
    assert!(circular_dir.validate().is_err());

    let mut bad_version = physical;
    bad_version.nodes[8] = LiteralNode::Fixnum(0);
    assert!(bad_version.validate().is_err());
}
