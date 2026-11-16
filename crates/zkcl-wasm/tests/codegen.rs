#![cfg(all(feature = "codegen", feature = "engine"))]

use std::convert::Infallible;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use wasm_encoder::{
    CodeSection, EntityType, ExportKind, ExportSection, Function, FunctionSection, ImportSection,
    Instruction as Wasm, MemArg, MemorySection, MemoryType, TypeSection, ValType,
};
use zkcl_bytecode::{
    Arity, BytecodeFunction, Callee, Immediate, Instruction, LiteralNode, WORD_NIL, WORD_T,
    fixnum_bounds_for_bits, native_abi::LEAF_MISS,
};
use zkcl_opt::native::JIT_RETURN_TAG_ONE;
use zkcl_opt::{SpecializationInputs, lift_code};
use zkcl_wasm::codegen::lower_ssa_function;
use zkcl_wasm::{Caller, Error, FuncImport, Host, HostCall, Instance, Limits, Module, Trap, Value};

struct Wasm32;

impl SpecializationInputs for Wasm32 {
    fn fixnum_bounds(&self) -> (i64, i64) {
        fixnum_bounds_for_bits(32).unwrap()
    }
}

struct NoImports;

impl Host for NoImports {
    type Func = Infallible;

    fn link(&mut self, import: FuncImport<'_>) -> Result<Self::Func, Error> {
        Err(Error::Import {
            module: import.module.into(),
            name: import.name.into(),
        })
    }

    fn call<'a>(
        &'a mut self,
        reference: &Infallible,
        _: &[Value],
        _: Caller<'a>,
    ) -> Result<HostCall<'a>, Trap> {
        match *reference {}
    }
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let Poll::Ready(result) = future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("expected synchronous Wasm execution");
    };
    result
}

fn scalar_leaf(
    arity: usize,
    registers: usize,
    instructions: Vec<Instruction>,
    primitives: &[&str],
    cases: &[([u64; 4], u64)],
) {
    let code = BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity: Arity {
            required: arity,
            maximum: Some(arity),
        },
        registers,
        instructions,
        constants: (0..primitives.len() as u32).collect(),
        lambda_list: None,
        captures: vec![],
        notinline: vec![],
        vop_calls: Vec::new(),
        handlers: vec![],
        restarts: vec![],
        keywords: vec![],
        jump_tables: vec![],
        facts: Default::default(),
    };
    let literals: Vec<_> = primitives
        .iter()
        .map(|&name| LiteralNode::InternedSymbol {
            package: "COMMON-LISP".into(),
            name: name.into(),
        })
        .collect();
    let body = zkcl_wasm::codegen::compile_leaf(&code, &literals)
        .unwrap()
        .body;
    let mut types = TypeSection::new();
    types.ty().function([ValType::I32; 4], [ValType::I32]);
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 0);
    let mut bodies = CodeSection::new();
    bodies.function(&body);
    let mut module = wasm_encoder::Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&exports)
        .section(&bodies);
    let module = Module::parse(&module.finish()).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    for &(args, expected) in cases {
        let args = args.map(|word| Value::I32(word as i32));
        assert_eq!(
            ready(instance.invoke("run", &args)).unwrap(),
            [Value::I32(expected as i32)],
            "{args:?}"
        );
    }
}

fn named_call(symbol: usize, out: Option<usize>, args: std::ops::Range<usize>) -> Instruction {
    Instruction::Call {
        callee: Callee::Named(symbol),
        out,
        args,
        at: 0,
        associate: false,
        notinline: false,
    }
}

fn tagged(n: i64) -> u64 {
    Immediate::Fixnum(n).encode_word(32).unwrap()
}

#[test]
fn scalar_leaf_reports_type_and_overflow_misses() {
    let (_, max) = fixnum_bounds_for_bits(32).unwrap();
    scalar_leaf(
        2,
        2,
        vec![named_call(0, Some(0), 0..1), named_call(1, None, 0..2)],
        &["1+", "<"],
        &[
            ([tagged(-2), tagged(0), WORD_NIL, WORD_NIL], WORD_T),
            ([tagged(2), tagged(3), WORD_NIL, WORD_NIL], WORD_NIL),
            ([WORD_NIL, tagged(3), WORD_NIL, WORD_NIL], LEAF_MISS),
            ([tagged(max), tagged(3), WORD_NIL, WORD_NIL], LEAF_MISS),
        ],
    );
}

#[test]
fn scalar_leaf_swaps_two_live_values_at_a_join() {
    use Instruction::{Copy, Unless};
    scalar_leaf(
        3,
        4,
        vec![
            named_call(0, Some(1), 1..2),
            named_call(0, Some(2), 2..3),
            Unless(0, 6),
            Copy(3, 1),
            Copy(1, 2),
            Copy(2, 3),
            named_call(1, None, 1..3),
        ],
        &["1+", "-"],
        &[
            ([WORD_NIL, tagged(5), tagged(9), WORD_NIL], tagged(-4)),
            ([WORD_T, tagged(5), tagged(9), WORD_NIL], tagged(4)),
            ([WORD_T, WORD_NIL, tagged(9), WORD_NIL], LEAF_MISS),
        ],
    );
}

#[test]
fn scalar_leaf_preserves_variadic_integer_arithmetic() {
    let (_, max) = fixnum_bounds_for_bits(32).unwrap();
    scalar_leaf(
        3,
        5,
        vec![
            named_call(0, Some(3), 0..0),
            named_call(1, Some(4), 0..0),
            named_call(2, None, 0..5),
        ],
        &["+", "*", "-"],
        &[
            ([tagged(20), tagged(4), tagged(3), WORD_NIL], tagged(12)),
            ([tagged(max), tagged(-1), tagged(2), WORD_NIL], LEAF_MISS),
            ([tagged(20), WORD_NIL, tagged(3), WORD_NIL], LEAF_MISS),
        ],
    );
    let (min, _) = fixnum_bounds_for_bits(32).unwrap();
    scalar_leaf(
        1,
        1,
        vec![named_call(0, None, 0..1)],
        &["-"],
        &[
            ([tagged(-17), WORD_NIL, WORD_NIL, WORD_NIL], tagged(17)),
            ([tagged(min), WORD_NIL, WORD_NIL, WORD_NIL], LEAF_MISS),
        ],
    );
}

#[test]
fn scalar_leaf_preserves_two_complement_bitwise_results_and_tags() {
    type Binary = fn(i64, i64) -> i64;
    let (min, max) = fixnum_bounds_for_bits(32).unwrap();
    let inputs = [min, min + 1, -129, -2, -1, 0, 1, 128, max - 1, max];
    let ops: [(&str, Binary); 10] = [
        ("LOGAND", |a, b| a & b),
        ("LOGIOR", |a, b| a | b),
        ("LOGXOR", |a, b| a ^ b),
        ("LOGEQV", |a, b| !(a ^ b)),
        ("LOGNAND", |a, b| !(a & b)),
        ("LOGNOR", |a, b| !(a | b)),
        ("LOGANDC1", |a, b| !a & b),
        ("LOGANDC2", |a, b| a & !b),
        ("LOGORC1", |a, b| !a | b),
        ("LOGORC2", |a, b| a | !b),
    ];
    for (name, op) in ops {
        let mut cases = vec![([WORD_NIL, tagged(0), WORD_NIL, WORD_NIL], LEAF_MISS)];
        for a in inputs {
            for b in inputs {
                cases.push(([tagged(a), tagged(b), WORD_NIL, WORD_NIL], tagged(op(a, b))));
            }
        }
        scalar_leaf(2, 2, vec![named_call(0, None, 0..2)], &[name], &cases);
    }
}

#[test]
fn scalar_leaf_handles_bit_predicates_and_logeqv_arity() {
    let (_, max) = fixnum_bounds_for_bits(32).unwrap();
    for (name, n, expected) in [
        ("LOGAND", 0, -1),
        ("LOGIOR", 0, 0),
        ("LOGXOR", 0, 0),
        ("LOGEQV", 0, -1),
        ("LOGEQV", 1, -9),
        ("LOGEQV", 3, -9 ^ 6 ^ 4),
        ("LOGEQV", 4, !(-9 ^ 6 ^ 4 ^ 2)),
    ] {
        let args = [-9, 6, 4, 2].map(tagged);
        scalar_leaf(
            n,
            n,
            vec![named_call(0, None, 0..n)],
            &[name],
            &[(args, tagged(expected))],
        );
    }
    for bit in [0, 27, 28, 104, max] {
        let at = |n: i64| {
            if bit >= 28 {
                n < 0
            } else {
                n & (1 << bit) != 0
            }
        };
        let cases: Vec<_> = [-1, -2, -9, 0, 1, 42, max]
            .into_iter()
            .map(|n| {
                (
                    [tagged(n), WORD_NIL, WORD_NIL, WORD_NIL],
                    if at(n) { WORD_T } else { WORD_NIL },
                )
            })
            .chain([([WORD_NIL; 4], LEAF_MISS)])
            .collect();
        scalar_leaf(
            1,
            3,
            vec![
                Instruction::Load(1, Immediate::Fixnum(bit)),
                Instruction::Copy(2, 0),
                named_call(0, None, 1..3),
            ],
            &["LOGBITP"],
            &cases,
        );
    }
    for (name, input, expected) in [
        ("ODDP", -1, WORD_T),
        ("ODDP", 0, WORD_NIL),
        ("EVENP", -2, WORD_T),
        ("EVENP", -1, WORD_NIL),
        ("LOGNOT", max, tagged(!max)),
        ("LOGNOT", -1, tagged(0)),
    ] {
        scalar_leaf(
            1,
            1,
            vec![named_call(0, None, 0..1)],
            &[name],
            &[
                ([tagged(input), WORD_NIL, WORD_NIL, WORD_NIL], expected),
                ([WORD_NIL; 4], LEAF_MISS),
            ],
        );
    }
}

#[test]
fn scalar_leaf_uses_a_bounded_loop_with_parallel_backedge_copies() {
    use Instruction::{Copy, Jump, Load, SetValues, Unless};

    // PC zero is a loop header. A backedge swaps two still-live values and
    // decrements the counter. The exit deliberately distinguishes the lanes.
    let code = vec![
        Copy(3, 0),
        Load(4, Immediate::Fixnum(0)),
        named_call(0, Some(3), 3..5),
        Unless(3, 6),
        SetValues(1..2),
        Jump(11),
        Copy(3, 1),
        Copy(1, 2),
        Copy(2, 3),
        named_call(1, Some(0), 0..1),
        Jump(0),
    ];
    scalar_leaf(
        3,
        5,
        code,
        &["<=", "1-"],
        &[
            ([tagged(-1), tagged(41), tagged(42), WORD_NIL], tagged(41)),
            ([tagged(0), tagged(41), tagged(42), WORD_NIL], tagged(41)),
            ([tagged(1), tagged(41), tagged(42), WORD_NIL], tagged(42)),
            ([tagged(2), tagged(41), tagged(42), WORD_NIL], tagged(41)),
            ([tagged(7), tagged(41), tagged(42), WORD_NIL], tagged(42)),
            ([tagged(10000), tagged(41), tagged(42), WORD_NIL], LEAF_MISS),
            ([WORD_T, tagged(41), tagged(42), WORD_NIL], LEAF_MISS),
        ],
    );
}

#[test]
fn scalar_leaf_preserves_both_entries_into_the_same_wasm_loop() {
    use Instruction::{Jump, Load, SetValues, Unless};
    let code = vec![
        Unless(0, 2),
        Jump(4),
        named_call(0, Some(1), 1..2),
        Jump(4),
        Load(2, Immediate::Fixnum(0)),
        named_call(1, Some(3), 1..3),
        Unless(3, 8),
        Jump(2),
        SetValues(1..2),
    ];
    scalar_leaf(
        2,
        4,
        code,
        &["1-", ">"],
        &[
            ([WORD_NIL, tagged(0), WORD_NIL, WORD_NIL], tagged(-1)),
            ([WORD_T, tagged(0), WORD_NIL, WORD_NIL], tagged(0)),
            ([WORD_T, tagged(7), WORD_NIL, WORD_NIL], tagged(0)),
            ([WORD_NIL, tagged(7), WORD_NIL, WORD_NIL], tagged(0)),
            ([WORD_NIL, tagged(10000), WORD_NIL, WORD_NIL], LEAF_MISS),
        ],
    );
}

#[test]
fn top_level_declarations_use_the_runtime_abi() {
    use zkcl_bytecode::{Immediate, native_abi::RuntimeAbiType};
    use zkcl_opt::native::RuntimeAbiFunction as Abi;

    let declarations = BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity: Arity::ZERO,
        registers: 1,
        instructions: vec![
            Instruction::ProclaimSpecial { symbol: 0, at: 0 },
            Instruction::ProclaimInline {
                symbol: 0,
                setf: false,
                notinline: true,
            },
            Instruction::SetVariableDocumentation { symbol: 0, doc: 1 },
            Instruction::WhenBound { symbol: 0, jump: 6 },
            Instruction::Load(0, Immediate::Nil),
            Instruction::Jump(7),
            Instruction::Load(0, Immediate::True),
            Instruction::SetValues(0..1),
        ],
        constants: vec![0, 1],
        lambda_list: None,
        captures: vec![],
        notinline: vec![],
        vop_calls: Vec::new(),
        handlers: vec![],
        restarts: vec![],
        keywords: vec![],
        jump_tables: vec![],
        facts: Default::default(),
    };
    let ssa = lift_code(&declarations, &Wasm32).unwrap();
    let used = std::cell::RefCell::new(Vec::new());
    let lowered = lower_ssa_function(
        &ssa,
        &Wasm32,
        Some(&declarations.instructions),
        |function| {
            used.borrow_mut().push(function);
            Some(function as u32)
        },
    )
    .unwrap()
    .unwrap();
    assert!(
        lowered
            .instructions
            .iter()
            .any(|site| { site.bytecode_pc == 0 && site.source_offset == Some(0) })
    );
    assert!(lowered.instructions.iter().all(|site| {
        !site.wasm.is_empty() && site.wasm.end as usize <= lowered.body.byte_len()
    }));
    let body = lowered.body;
    let used = used.into_inner();
    for function in [
        Abi::ProclaimSpecial,
        Abi::ProclaimInline,
        Abi::SetVariableDocumentation,
        Abi::IsSymbolBound,
    ] {
        assert!(used.contains(&function), "missing {function:?}");
    }

    let val_type = |ty| match ty {
        RuntimeAbiType::Bool | RuntimeAbiType::Pointer | RuntimeAbiType::Word => ValType::I32,
        RuntimeAbiType::I64 => ValType::I64,
    };
    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    for function in Abi::ALL {
        types.ty().function(
            function.parameter_types().iter().copied().map(val_type),
            function.return_type().map(val_type),
        );
        imports.import(
            "runtime",
            function.symbol_name(),
            EntityType::Function(function as u32),
        );
    }
    types.ty().function([ValType::I32; 6], []);
    let mut functions = FunctionSection::new();
    functions.function(Abi::ALL.len() as u32);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut code = CodeSection::new();
    code.function(&body);
    let mut module = wasm_encoder::Module::new();
    module
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&code);
    Module::parse(&module.finish()).unwrap();
}

#[test]
fn optional_argument_is_unbound_when_omitted() {
    const RESULT: i32 = 32;
    const ARGS: i32 = 16;
    let bytecode = BytecodeFunction {
        name: None,
        source_file: None,
        debug: Default::default(),
        arity: Arity {
            required: 0,
            maximum: Some(1),
        },
        registers: 1,
        instructions: vec![
            Instruction::OptionalArgument(0, 0),
            Instruction::IfPresent(0, 3),
            Instruction::Load(0, zkcl_bytecode::Immediate::Nil),
            Instruction::ReleaseArguments,
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
    };
    let ssa = lift_code(&bytecode, &Wasm32).unwrap();
    let entry = lower_ssa_function(&ssa, &Wasm32, None, |function| {
        panic!("unexpected runtime function {function:?}")
    })
    .unwrap()
    .unwrap()
    .body;

    let mut types = TypeSection::new();
    types.ty().function([ValType::I32; 6], []);
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32, ValType::I32]);
    let mut functions = FunctionSection::new();
    functions.function(0).function(1);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: Some(1),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut exports = ExportSection::new();
    exports.export("invoke", ExportKind::Func, 1);
    let word = MemArg {
        offset: 0,
        align: 2,
        memory_index: 0,
    };
    let mut invoke = Function::new([]);
    for instruction in [
        Wasm::I32Const(ARGS),
        Wasm::LocalGet(1),
        Wasm::I32Store(word),
        Wasm::I32Const(0),
        Wasm::I32Const(WORD_NIL as i32),
        Wasm::I32Const(ARGS),
        Wasm::LocalGet(0),
        Wasm::I32Const(RESULT),
        Wasm::I32Const(64),
        Wasm::Call(0),
        Wasm::I32Const(RESULT),
        Wasm::I32Load8U(MemArg { align: 0, ..word }),
        Wasm::I32Const(RESULT),
        Wasm::I32Load(MemArg { offset: 4, ..word }),
        Wasm::End,
    ] {
        invoke.instruction(&instruction);
    }
    let mut code = CodeSection::new();
    code.function(&entry).function(&invoke);
    let mut wasm = wasm_encoder::Module::new();
    wasm.section(&types)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code);
    let module = Module::parse(&wasm.finish()).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    for (argc, expected) in [(0, WORD_NIL), (1, WORD_T)] {
        let values =
            ready(instance.invoke("invoke", &[Value::I32(argc), Value::I32(WORD_T as i32)]))
                .unwrap();
        assert_eq!(
            values,
            [
                Value::I32(JIT_RETURN_TAG_ONE.into()),
                Value::I32(expected as i32)
            ]
        );
    }
}

#[test]
fn debug_point_continues_or_forwards_control_and_drops_the_frame_once() {
    use std::cell::Cell;
    use zkcl_opt::native::{
        JIT_RETURN_TAG_FAULT, JIT_RETURN_TAG_UNWIND, JIT_RETURN_TAG_VM_RESUME,
        RuntimeAbiFunction as Abi, jit_return_layout,
    };
    use zkcl_opt::{
        BasicBlock, BlockId, ScopeId, ScopeTree, SsaFunction, SsaInst, SsaOp, Terminator, TypeTag,
        ValueId,
    };

    const RESULT: i32 = 32;
    const SCRATCH: i32 = 64;
    const ROOTS: i32 = 96;
    const FRAME: i32 = 2;
    const UNWIND_FRAME: i32 = 3;
    const UNWIND_STATE: i32 = 7;
    const HELPERS: &[Abi] = &[
        Abi::AllocRootFrame,
        Abi::DropRootFrame,
        Abi::StageDebugArguments,
        Abi::DebugPoint,
    ];

    // Model only the runtime helpers called by the real lowered function.
    struct DebugHelpers<'a>(&'a Cell<usize>);

    impl Host for DebugHelpers<'_> {
        type Func = Abi;

        fn link(&mut self, import: FuncImport<'_>) -> Result<Self::Func, Error> {
            Ok(Abi::from_symbol_name(import.name).unwrap())
        }

        fn call<'a>(
            &'a mut self,
            function: &Abi,
            args: &[Value],
            mut caller: Caller<'a>,
        ) -> Result<HostCall<'a>, Trap> {
            use Value::I32;
            match (*function, args) {
                (Abi::AllocRootFrame, [_, _, I32(out_slots), I32(out_index)]) => {
                    caller.write_memory(0, *out_slots as u32, &ROOTS.to_le_bytes())?;
                    caller.write_memory(0, *out_index as u32, &FRAME.to_le_bytes())?;
                }
                (Abi::StageDebugArguments, [_, _, _]) => {}
                (Abi::DebugPoint, [I32(tag), _, _, _, I32(out)]) => {
                    let result = [*tag, 0, UNWIND_FRAME, UNWIND_STATE].map(i32::to_le_bytes);
                    caller.write_memory(0, *out as u32, result.as_flattened())?;
                }
                (Abi::DropRootFrame, [I32(tag), I32(FRAME)]) => {
                    let primary = if *tag == i32::from(JIT_RETURN_TAG_ONE) {
                        WORD_T as i32
                    } else {
                        0
                    };
                    let expected =
                        [*tag, primary, UNWIND_FRAME, UNWIND_STATE].map(i32::to_le_bytes);
                    assert_eq!(
                        caller.read_memory(0, RESULT as u32, jit_return_layout(4).size as usize)?,
                        expected.as_flattened(),
                    );
                    self.0.set(self.0.get() + 1);
                }
                _ => panic!("unexpected helper call {function:?} with {args:?}"),
            }
            Ok(HostCall::Ready(vec![]))
        }
    }

    let ssa = SsaFunction {
        fixnum_bounds: Wasm32.fixnum_bounds(),
        arity: Arity::ZERO,
        constant_count: 0,
        entry: BlockId(0),
        entry_params: vec![],
        blocks: vec![BasicBlock {
            id: BlockId(0),
            params: vec![],
            insts: vec![
                SsaInst {
                    id: None,
                    op: SsaOp::DebugPoint {
                        pc: 0,
                        registers: vec![],
                    },
                    ty: TypeTag::Top,
                    pc: Some(0),
                    deopt: None,
                },
                SsaInst {
                    id: Some(ValueId(0)),
                    op: SsaOp::ConstWord(Immediate::True),
                    ty: TypeTag::True,
                    pc: Some(0),
                    deopt: None,
                },
            ],
            terminator: Terminator::ReturnOne(ValueId(0)),
            terminator_pc: 1,
            entry_scope: ScopeId::ROOT,
            unwind_succs: vec![],
        }],
        scopes: ScopeTree::new(),
        pc_to_block: Default::default(),
        next_value_id: 1,
    };
    let lowered = lower_ssa_function(&ssa, &Wasm32, None, |abi| {
        HELPERS
            .iter()
            .position(|helper| *helper == abi)
            .map(|index| index as u32)
    })
    .unwrap()
    .unwrap();

    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    for (index, abi) in HELPERS.iter().enumerate() {
        types.ty().function(
            std::iter::repeat_n(ValType::I32, abi.parameter_types().len()),
            [],
        );
        imports.import(
            "runtime",
            abi.symbol_name(),
            EntityType::Function(index as u32),
        );
    }
    let entry = HELPERS.len() as u32;
    types.ty().function([ValType::I32; 6], []);
    let mut functions = FunctionSection::new();
    functions.function(entry);
    let mut code = CodeSection::new();
    code.function(&lowered.body);

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: Some(1),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut exports = ExportSection::new();
    exports.export("invoke", ExportKind::Func, entry);
    let mut wasm = wasm_encoder::Module::new();
    wasm.section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code);
    let module = Module::parse(&wasm.finish()).unwrap();
    let drops = Cell::new(0);
    let mut instance = ready(Instance::instantiate(
        &module,
        DebugHelpers(&drops),
        Limits::default(),
    ))
    .unwrap();
    let tags = [
        JIT_RETURN_TAG_ONE,
        JIT_RETURN_TAG_VM_RESUME,
        JIT_RETURN_TAG_UNWIND,
        JIT_RETURN_TAG_FAULT,
    ];
    for (index, tag) in tags.into_iter().enumerate() {
        let args = [i32::from(tag), WORD_NIL as i32, 0, 0, RESULT, SCRATCH].map(Value::I32);
        assert!(ready(instance.invoke("invoke", &args)).unwrap().is_empty());
        assert_eq!(drops.get(), index + 1, "return tag {tag}");
    }
}
