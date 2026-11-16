#![cfg(feature = "engine")]

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataCountSection, DataSection, ElementSection, Elements,
    EntityType, ExportKind, ExportSection, Function, FunctionSection, GlobalSection, GlobalType,
    ImportSection, Instruction, MemArg, MemorySection, MemoryType, NameMap, NameSection, RefType,
    TableSection, TableType, TypeSection, ValType,
};
use zkcl_wasm::{Caller, Error, FuncImport, Host, HostCall, Instance, Limits, Module, Trap, Value};

struct NoImports;

impl Host for NoImports {
    type Func = std::convert::Infallible;

    fn link(&mut self, import: FuncImport<'_>) -> Result<Self::Func, Error> {
        Err(Error::Import {
            module: import.module.into(),
            name: import.name.into(),
        })
    }

    fn call<'a>(
        &'a mut self,
        function: &Self::Func,
        _: &[Value],
        _: Caller<'a>,
    ) -> Result<HostCall<'a>, Trap> {
        match *function {}
    }
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("unexpected suspension"),
    }
}

fn instructions(ops: &[Instruction<'_>]) -> Function {
    let mut body = Function::new([]);
    for op in ops {
        body.instruction(op);
    }
    body.instruction(&Instruction::End);
    body
}

fn function_module(params: &[ValType], results: &[ValType], body: &Function) -> Vec<u8> {
    let mut types = TypeSection::new();
    types
        .ty()
        .function(params.iter().copied(), results.iter().copied());
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 0);
    let mut bodies = CodeSection::new();
    bodies.function(body);
    let mut module = wasm_encoder::Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&exports)
        .section(&bodies);
    module.finish()
}

#[test]
fn instance_memory_limit_exposes_the_initialization_trap() {
    use std::error::Error as _;

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: Some(1),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut binary = wasm_encoder::Module::new();
    binary.section(&memories);
    let module = Module::parse(&binary.finish()).unwrap();
    let limits = Limits {
        memory_bytes: 0,
        ..Limits::default()
    };
    let error = ready(Instance::instantiate(&module, NoImports, limits))
        .err()
        .expect("memory exceeds limit");
    assert_eq!(
        error.source().and_then(|source| source.downcast_ref()),
        Some(&Trap::ResourceLimit)
    );
}

#[test]
fn branch_table_uses_selected_label() {
    use Instruction::*;
    let bytes = function_module(
        &[ValType::I32],
        &[ValType::I32],
        &instructions(&[
            Block(BlockType::Result(ValType::I32)),
            Block(BlockType::Result(ValType::I32)),
            I32Const(-1),
            I32Const(41),
            LocalGet(0),
            BrTable(vec![0].into(), 1),
            End,
            I32Const(1),
            I32Add,
            End,
        ]),
    );
    let module = Module::parse(&bytes).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    assert_eq!(
        ready(instance.invoke("run", &[Value::I32(0)])).unwrap(),
        [Value::I32(42)]
    );
    assert_eq!(
        ready(instance.invoke("run", &[Value::I32(-1)])).unwrap(),
        [Value::I32(41)]
    );
}

#[test]
fn loop_labels_receive_parameters() {
    use Instruction::*;
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I32], [ValType::I32, ValType::I64]);
    types.ty().function([ValType::I32, ValType::I64], []);
    types.ty().function([], [ValType::I32, ValType::I64]);
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 0);
    let mut bodies = CodeSection::new();
    let mut body = Function::new([(1, ValType::I64)]);
    for op in [
        Block(BlockType::FunctionType(2)),
        LocalGet(0),
        I64Const(0),
        Loop(BlockType::FunctionType(1)),
        LocalSet(1),
        LocalSet(0),
        I32Const(42),
        LocalGet(1),
        LocalGet(0),
        I32Eqz,
        BrIf(1),
        Drop,
        Drop,
        LocalGet(0),
        I32Const(1),
        I32Sub,
        LocalGet(1),
        I64Const(2),
        I64Add,
        Br(0),
        End,
        Unreachable,
        End,
        End,
    ] {
        body.instruction(&op);
    }
    bodies.function(&body);
    let mut binary = wasm_encoder::Module::new();
    binary
        .section(&types)
        .section(&functions)
        .section(&exports)
        .section(&bodies);
    let module = Module::parse(&binary.finish()).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    assert_eq!(
        ready(instance.invoke("run", &[Value::I32(26)])).unwrap(),
        [Value::I32(42), Value::I64(52)]
    );
}

#[test]
fn implicit_else_preserves_block_parameters() {
    use Instruction::*;
    let bytes = function_module(
        &[ValType::I32],
        &[ValType::I32],
        &instructions(&[
            I32Const(41),
            LocalGet(0),
            If(BlockType::FunctionType(0)),
            I32Const(1),
            I32Add,
            End,
        ]),
    );
    let module = Module::parse(&bytes).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    assert_eq!(
        ready(instance.invoke("run", &[Value::I32(0)])).unwrap(),
        [Value::I32(41)]
    );
    assert_eq!(
        ready(instance.invoke("run", &[Value::I32(1)])).unwrap(),
        [Value::I32(42)]
    );
}

#[test]
fn function_references_keep_their_instance() {
    let reference = ValType::Ref(RefType::FUNCREF);
    let bytes = function_module(
        &[reference],
        &[reference],
        &instructions(&[Instruction::LocalGet(0)]),
    );
    let module = Module::parse(&bytes).unwrap();
    let mut first = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    let mut second = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    let reference = Value::FuncRef(first.func_ref("run"));
    assert_ne!(reference, Value::FuncRef(second.func_ref("run")));
    assert_eq!(
        ready(first.invoke("run", &[reference])).unwrap(),
        [reference]
    );
    assert!(matches!(
        ready(second.invoke("run", &[reference])),
        Err(Error::Signature(_))
    ));
}

#[test]
fn indirect_call_matches_structural_types() {
    use Instruction::*;
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32, ValType::I64]);
    types.ty().function([], [ValType::I32, ValType::I64]);
    types
        .ty()
        .function([ValType::I32], [ValType::I32, ValType::I64]);
    let mut functions = FunctionSection::new();
    functions.function(0).function(2);
    let mut tables = TableSection::new();
    tables.table(TableType {
        element_type: RefType::FUNCREF,
        table64: false,
        minimum: 3,
        maximum: None,
        shared: false,
    });
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 1);
    let mut elements = ElementSection::new();
    elements.active(
        None,
        &ConstExpr::i32_const(0),
        Elements::Functions([0, 1][..].into()),
    );
    let mut bodies = CodeSection::new();
    bodies.function(&instructions(&[I32Const(42), I64Const(43)]));
    bodies.function(&instructions(&[
        LocalGet(0),
        CallIndirect {
            type_index: 1,
            table_index: 0,
        },
    ]));
    let mut binary = wasm_encoder::Module::new();
    binary
        .section(&types)
        .section(&functions)
        .section(&tables)
        .section(&exports)
        .section(&elements)
        .section(&bodies);
    let module = Module::parse(&binary.finish()).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    assert_eq!(
        ready(instance.invoke("run", &[Value::I32(0)])).unwrap(),
        [Value::I32(42), Value::I64(43)]
    );
    for (index, expected) in [
        (1, Trap::IndirectSignature),
        (2, Trap::NullFunction),
        (3, Trap::TableBounds),
    ] {
        assert!(
            matches!(ready(instance.invoke("run", &[Value::I32(index)])), Err(Error::Execution { source, .. }) if source == expected)
        );
    }
}

#[test]
fn passive_data_drops_after_overlapping_memory_copy() {
    use Instruction::*;
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 0);
    let mut bodies = CodeSection::new();
    bodies.function(&instructions(&[
        I32Const(0),
        I32Const(0),
        I32Const(4),
        MemoryInit {
            mem: 0,
            data_index: 0,
        },
        I32Const(1),
        I32Const(0),
        I32Const(4),
        MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        },
        DataDrop(0),
        I32Const(1),
        I32Load(MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }),
    ]));
    let mut data = DataSection::new();
    data.passive(*b"wasm");
    let mut binary = wasm_encoder::Module::new();
    binary
        .section(&types)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&DataCountSection { count: 1 })
        .section(&bodies)
        .section(&data);
    let module = Module::parse(&binary.finish()).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    assert_eq!(
        ready(instance.invoke("run", &[])).unwrap(),
        [Value::I32(i32::from_le_bytes(*b"wasm"))]
    );
    assert!(matches!(
        ready(instance.invoke("run", &[])),
        Err(Error::Execution {
            source: Trap::MemoryBounds,
            ..
        })
    ));
}

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct Timer {
    calls: Arc<AtomicUsize>,
}

impl Host for Timer {
    type Func = ();

    fn link(&mut self, import: FuncImport<'_>) -> Result<Self::Func, Error> {
        assert_eq!((import.module, import.name), ("test", "wait"));
        Ok(())
    }

    fn call<'a>(
        &'a mut self,
        _: &(),
        args: &[Value],
        mut caller: Caller<'a>,
    ) -> Result<HostCall<'a>, Trap> {
        assert!(args.is_empty());
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(HostCall::Pending(Box::pin(async move {
            zkcl_bytecode::dispatch::yield_now().await;
            if call == 2 {
                return Err(Trap::Host("timer failed".into()));
            }
            caller.write_memory(0, 0, &[21])?;
            Ok(vec![Value::I32(20), Value::I32(1)])
        })))
    }
}

#[test]
fn pending_import_resumes_the_caller_once() {
    use Instruction::*;
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32, ValType::I32]);
    types.ty().function([], [ValType::I32]);
    let mut imports = ImportSection::new();
    imports.import("test", "wait", EntityType::Function(0));
    let mut functions = FunctionSection::new();
    functions.function(1).function(1).function(1).function(0);
    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: 1,
        maximum: Some(1),
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 2);
    exports.export("calls", ExportKind::Func, 3);
    exports.export("tail", ExportKind::Func, 4);
    let mut bodies = CodeSection::new();
    bodies.function(&instructions(&[
        GlobalGet(0),
        I32Const(1),
        I32Add,
        GlobalSet(0),
        Call(4),
        I32Add,
        I32Const(0),
        I32Load8U(MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }),
        I32Add,
    ]));
    bodies.function(&instructions(&[I32Const(0), Call(1), I32Add]));
    bodies.function(&instructions(&[GlobalGet(0)]));
    bodies.function(&instructions(&[ReturnCall(0)]));
    let mut binary = wasm_encoder::Module::new();
    binary
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&memories)
        .section(&globals)
        .section(&exports)
        .section(&bodies);
    let module = Module::parse(&binary.finish()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut instance = ready(Instance::instantiate(
        &module,
        Timer {
            calls: calls.clone(),
        },
        Limits::default(),
    ))
    .unwrap();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    {
        let mut run = pin!(instance.invoke("run", &[]));
        assert!(run.as_mut().poll(&mut context).is_pending());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
        let Poll::Ready(result) = run.as_mut().poll(&mut context) else {
            panic!("call did not resume")
        };
        assert_eq!(result.unwrap(), [Value::I32(42)]);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        ready(instance.invoke("calls", &[])).unwrap(),
        [Value::I32(1)]
    );
    {
        let mut tail = pin!(instance.invoke("tail", &[]));
        assert!(tail.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(result) = tail.as_mut().poll(&mut context) else {
            panic!("tail-called import did not resume")
        };
        assert_eq!(result.unwrap(), [Value::I32(20), Value::I32(1)]);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 2);

    let mut run = pin!(instance.invoke("run", &[]));
    assert!(run.as_mut().poll(&mut context).is_pending());
    let Poll::Ready(Err(Error::Execution {
        frame,
        callers,
        source,
    })) = run.as_mut().poll(&mut context)
    else {
        panic!("expected a trap from the tail-called import");
    };
    assert_eq!(source, Trap::Host("timer failed".into()));
    assert_eq!(frame.function, 0);
    assert_eq!(frame.offset, None);
    assert_eq!(
        callers
            .iter()
            .map(|frame| frame.function)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn repeated_imports_preserve_arguments_across_a_suspended_tail_call() {
    use Instruction::*;

    struct Echo;

    enum EchoFunction {
        Wide,
        Pending,
    }

    impl Host for Echo {
        type Func = EchoFunction;

        fn link(&mut self, import: FuncImport<'_>) -> Result<Self::Func, Error> {
            match (import.module, import.name) {
                ("test", "wide") => Ok(EchoFunction::Wide),
                ("test", "pending") => Ok(EchoFunction::Pending),
                _ => panic!("unexpected import {}.{}", import.module, import.name),
            }
        }

        fn call<'a>(
            &'a mut self,
            function: &EchoFunction,
            arguments: &[Value],
            _: Caller<'a>,
        ) -> Result<HostCall<'a>, Trap> {
            match function {
                EchoFunction::Wide => {
                    let [
                        Value::I32(7),
                        reference @ Value::FuncRef(Some(_)),
                        Value::I64(-33),
                    ] = arguments
                    else {
                        panic!("unexpected wide import arguments: {arguments:?}");
                    };
                    Ok(HostCall::Ready(vec![*reference]))
                }
                EchoFunction::Pending => {
                    let [reference @ Value::FuncRef(Some(_))] = arguments else {
                        panic!("unexpected pending import arguments: {arguments:?}");
                    };
                    let reference = *reference;
                    Ok(HostCall::Pending(Box::pin(async move {
                        zkcl_bytecode::dispatch::yield_now().await;
                        Ok(vec![reference])
                    })))
                }
            }
        }
    }

    let reference_type = ValType::Ref(RefType::FUNCREF);
    let mut types = TypeSection::new();
    types.ty().function(
        [ValType::I32, reference_type, ValType::I64],
        [reference_type],
    );
    types.ty().function([reference_type], [reference_type]);
    let mut imports = ImportSection::new();
    imports.import("test", "wide", EntityType::Function(0));
    imports.import("test", "pending", EntityType::Function(1));
    let mut functions = FunctionSection::new();
    functions.function(1).function(1);
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 2);
    exports.export("nested", ExportKind::Func, 3);
    let mut bodies = CodeSection::new();
    bodies.function(&instructions(&[
        I32Const(7),
        LocalGet(0),
        I64Const(-33),
        Call(0),
        ReturnCall(1),
    ]));
    bodies.function(&instructions(&[LocalGet(0), Call(2)]));
    let mut binary = wasm_encoder::Module::new();
    binary
        .section(&types)
        .section(&imports)
        .section(&functions)
        .section(&exports)
        .section(&bodies);

    let module = Module::parse(&binary.finish()).unwrap();
    let mut instance = ready(Instance::instantiate(&module, Echo, Limits::default())).unwrap();
    let arguments = [Value::FuncRef(instance.func_ref("run"))];
    let mut context = Context::from_waker(Waker::noop());
    {
        let mut discarded = pin!(instance.invoke("run", &arguments));
        assert!(discarded.as_mut().poll(&mut context).is_pending());
    }
    for entry in ["run", "nested"] {
        let mut call = pin!(instance.invoke(entry, &arguments));
        assert!(call.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(result) = call.as_mut().poll(&mut context) else {
            panic!("{entry} did not resume its pending import");
        };
        assert_eq!(result.unwrap(), arguments);
    }
}

#[test]
fn tail_calls_reuse_the_frame_and_clear_locals() {
    use Instruction::*;

    for indirect in [false, true] {
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], [ValType::I32]);
        let mut functions = FunctionSection::new();
        functions.function(0).function(0);
        let mut tables = TableSection::new();
        tables.table(TableType {
            element_type: RefType::FUNCREF,
            table64: false,
            minimum: 1,
            maximum: Some(1),
            shared: false,
        });
        let mut exports = ExportSection::new();
        exports.export("run", ExportKind::Func, 0);
        exports.export("ordinary", ExportKind::Func, 1);
        let mut elements = ElementSection::new();
        elements.active(
            None,
            &ConstExpr::i32_const(0),
            Elements::Functions([0][..].into()),
        );
        let mut body = Function::new([(1, ValType::I32)]);
        for op in [
            LocalGet(0),
            I32Eqz,
            If(BlockType::Empty),
            LocalGet(1),
            Return,
            End,
            I32Const(99),
            LocalSet(1),
            LocalGet(0),
            I32Const(1),
            I32Sub,
        ] {
            body.instruction(&op);
        }
        if indirect {
            body.instruction(&I32Const(0));
            body.instruction(&ReturnCallIndirect {
                type_index: 0,
                table_index: 0,
            });
        } else {
            body.instruction(&ReturnCall(0));
        }
        body.instruction(&End);
        let mut bodies = CodeSection::new();
        bodies.function(&body);
        bodies.function(&instructions(&[LocalGet(0), Call(0)]));
        let mut binary = wasm_encoder::Module::new();
        binary
            .section(&types)
            .section(&functions)
            .section(&tables)
            .section(&exports)
            .section(&elements)
            .section(&bodies);
        let module = Module::parse(&binary.finish()).unwrap();
        let limits = Limits {
            call_depth: 1,
            ..Limits::default()
        };
        let mut instance = ready(Instance::instantiate(&module, NoImports, limits)).unwrap();
        assert_eq!(
            ready(instance.invoke("run", &[Value::I32(800)])).unwrap(),
            [Value::I32(0)]
        );
        assert!(matches!(
            ready(instance.invoke("ordinary", &[Value::I32(800)])),
            Err(Error::Execution {
                source: Trap::ResourceLimit,
                ..
            })
        ));
    }
}

#[test]
fn instruction_quantum_yields_back_edges() {
    use Instruction::*;
    let bytes = function_module(
        &[],
        &[],
        &instructions(&[Loop(BlockType::Empty), Br(0), End]),
    );
    let module = Module::parse(&bytes).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    let mut context = Context::from_waker(&waker);
    let mut run = pin!(instance.invoke("run", &[]));
    for count in 1..=3 {
        assert!(run.as_mut().poll(&mut context).is_pending());
        assert_eq!(wakes.0.load(Ordering::Relaxed), count);
    }
}

#[test]
fn validates_dead_instructions() {
    use Instruction::*;
    let bytes = function_module(&[], &[], &instructions(&[Unreachable, Call(1)]));
    assert!(matches!(Module::parse(&bytes), Err(Error::Invalid(_))));
    let valid = function_module(&[], &[], &instructions(&[Unreachable, Drop]));
    assert!(Module::parse(&valid).is_ok());
    assert!(matches!(
        Module::parse(&valid[..valid.len() - 1]),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn trap_reports_original_instruction_offset() {
    use Instruction::*;
    use zkcl_wasm::source_map::{FunctionSourceMap, InstructionRange, SECTION_NAME, SourceMap};
    let mut types = TypeSection::new();
    types.ty().function([], [ValType::I32]);
    let mut functions = FunctionSection::new();
    functions.function(0).function(0).function(0).function(0);
    let mut exports = ExportSection::new();
    exports.export("run", ExportKind::Func, 1);
    exports.export("tail", ExportKind::Func, 3);
    let mut bodies = CodeSection::new();
    let mut divide = Function::new([(2, ValType::I64)]);
    divide
        .instruction(&I32Const(42))
        .instruction(&Nop)
        .instruction(&I32Const(0));
    let start = divide.byte_len() as u32;
    divide.instruction(&I32DivS);
    let end = divide.byte_len() as u32;
    divide.instruction(&End);
    bodies.function(&divide);
    bodies.function(&instructions(&[Call(0)]));
    bodies.function(&instructions(&[ReturnCall(0)]));
    bodies.function(&instructions(&[Call(2)]));
    let mut function_names = NameMap::new();
    function_names.append(0, "divide");
    let mut names = NameSection::new();
    names.functions(&function_names);
    let locations = SourceMap {
        files: vec!["divide.lisp".into()],
        functions: vec![FunctionSourceMap {
            function: 0,
            source_file: Some(0),
            instructions: vec![InstructionRange {
                wasm: start..end,
                bytecode_pc: 42,
                source_offset: Some(100),
            }],
        }],
    };
    let mut binary = wasm_encoder::Module::new();
    binary
        .section(&types)
        .section(&functions)
        .section(&exports)
        .section(&bodies)
        .section(&names)
        .section(&wasm_encoder::CustomSection {
            name: SECTION_NAME.into(),
            data: postcard::to_allocvec(&locations).unwrap().into(),
        });
    let bytes = binary.finish();
    let offset = wasmparser::Parser::new(0)
        .parse_all(&bytes)
        .find_map(|payload| {
            let wasmparser::Payload::CodeSectionEntry(body) = payload.unwrap() else {
                return None;
            };
            body.get_operators_reader()
                .unwrap()
                .into_iter_with_offsets()
                .find_map(|entry| {
                    let (op, offset) = entry.unwrap();
                    matches!(op, wasmparser::Operator::I32DivS).then_some(offset)
                })
        })
        .unwrap();
    let module = Module::parse(&bytes).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    let Err(Error::Execution {
        frame,
        callers,
        source: Trap::DivisionByZero,
    }) = ready(instance.invoke("run", &[]))
    else {
        panic!("expected division by zero");
    };
    assert_eq!(frame.function, 0);
    assert_eq!(frame.offset, Some(offset));
    assert_eq!(frame.name.as_deref(), Some("divide"));
    let location = frame.lisp.as_ref().unwrap();
    assert_eq!(location.file.as_deref(), Some("divide.lisp"));
    assert_eq!(location.bytecode_pc, Some(42));
    assert_eq!(location.source_offset, Some(100));
    assert_eq!(callers.len(), 1);
    assert_eq!(callers[0].name.as_deref(), Some("run"));
    assert!(callers[0].lisp.is_none());

    let Err(Error::Execution {
        frame,
        callers,
        source: Trap::DivisionByZero,
    }) = ready(instance.invoke("tail", &[]))
    else {
        panic!("expected division by zero from a tail call");
    };
    assert_eq!(frame.offset, Some(offset));
    assert_eq!(callers.len(), 1);
    assert_eq!(callers[0].name.as_deref(), Some("tail"));
}

#[test]
fn float_min_max_choose_signed_zero() {
    use Instruction::*;
    let bytes = function_module(
        &[],
        &[ValType::F32, ValType::F64],
        &instructions(&[
            F32Const(0.0f32.into()),
            F32Const((-0.0f32).into()),
            F32Min,
            F64Const((-0.0f64).into()),
            F64Const(0.0f64.into()),
            F64Max,
        ]),
    );
    let module = Module::parse(&bytes).unwrap();
    let mut instance = ready(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    assert_eq!(
        ready(instance.invoke("run", &[])).unwrap(),
        [Value::F32((-0.0f32).to_bits()), Value::F64(0)]
    );
}
