use std::convert::Infallible;
use std::future::Future;
use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use wasm_encoder::{
    BlockType, CodeSection, ExportKind, ExportSection, Function, FunctionSection, Instruction,
    TypeSection, ValType,
};
use zkcl_wasm::{Caller, Error, FuncImport, Host, HostCall, Instance, Limits, Module, Trap, Value};

struct NoImports;

impl Host for NoImports {
    type Func = Infallible;

    fn link(&mut self, _: FuncImport<'_>) -> Result<Self::Func, Error> {
        unreachable!()
    }

    fn call<'a>(
        &'a mut self,
        function: &Infallible,
        _: &[Value],
        _: Caller<'a>,
    ) -> Result<HostCall<'a>, Trap> {
        match *function {}
    }
}

fn drive<F: Future>(future: F) -> F::Output {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            return result;
        }
    }
}

fn checksum_module() -> Vec<u8> {
    use Instruction::*;
    let mut types = TypeSection::new();
    types
        .ty()
        .function([ValType::I32, ValType::I32], [ValType::I32]);
    let mut functions = FunctionSection::new();
    functions.function(0);
    let mut exports = ExportSection::new();
    exports.export("checksum", ExportKind::Func, 0);
    let mut body = Function::new([]);
    for op in [
        Block(BlockType::Empty),
        Loop(BlockType::Empty),
        LocalGet(0),
        I32Eqz,
        BrIf(1),
        LocalGet(1),
        I32Const(1_664_525),
        I32Mul,
        LocalGet(0),
        I32Add,
        LocalSet(1),
        LocalGet(0),
        I32Const(1),
        I32Sub,
        LocalSet(0),
        Br(0),
        End,
        End,
        LocalGet(1),
        End,
    ] {
        body.instruction(&op);
    }
    let mut bodies = CodeSection::new();
    bodies.function(&body);
    let mut module = wasm_encoder::Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&exports)
        .section(&bodies);
    module.finish()
}

fn main() {
    const STEPS: i32 = 2_000_000;
    const SEED: i32 = 17;
    let expected = (1..=STEPS).rev().fold(SEED, |state, index| {
        state.wrapping_mul(1_664_525).wrapping_add(index)
    });
    let bytes = checksum_module();
    let module = Module::parse(&bytes).unwrap();
    let mut instance = drive(Instance::instantiate(&module, NoImports, Limits::default())).unwrap();
    let mut samples = Vec::new();
    for iteration in 0..20 {
        let started = Instant::now();
        let results = drive(instance.invoke(
            "checksum",
            black_box(&[Value::I32(STEPS), Value::I32(SEED)]),
        ))
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(results, [Value::I32(expected)]);
        if iteration >= 5 {
            samples.push(elapsed);
        }
    }
    samples.sort_unstable();
    println!(
        "checksum: {STEPS} iterations, median {:?}",
        samples[samples.len() / 2]
    );
}
