use zkcl::native::{AbiFingerprint, AotModuleDescriptor};
use zkcl::{EvalErrorKind, Runtime};
use zkcl_bytecode::{Arity, BytecodeFunction, CompiledFile, FASL_MAGIC, Instruction, LiteralNode};

#[test]
fn embedded_aot_loading_can_retry_after_validation_or_budget_failure() {
    let mut file = CompiledFile {
        target: zkcl::CompilationTarget::native(),
        source_files: Vec::new(),
        nodes: Vec::new(),
        entries: vec![BytecodeFunction {
            name: None,
            source_file: None,
            debug: Default::default(),
            arity: Arity::ZERO,
            registers: 1,
            instructions: vec![Instruction::Constant(0, 0), Instruction::SetValues(0..1)],
            constants: vec![0],
            lambda_list: None,
            captures: Vec::new(),
            notinline: Vec::new(),
            vop_calls: Vec::new(),
            handlers: Vec::new(),
            restarts: Vec::new(),
            keywords: Vec::new(),
            jump_tables: Vec::new(),
            facts: Default::default(),
        }],
    };
    let descriptor = |bytes: Vec<u8>| -> &'static AotModuleDescriptor {
        // SAFETY: This descriptor has no native entries. Loading validates the FASL.
        Box::leak(Box::new(unsafe {
            AotModuleDescriptor::new(
                "test-aot-load",
                AbiFingerprint::CURRENT,
                Some(bytes.leak()),
                &[],
            )
        }))
    };
    let runtime = Runtime::start().unwrap();
    let malformed = descriptor(postcard::to_extend(&file, FASL_MAGIC.to_vec()).unwrap());
    let error = runtime.load_aot_module(malformed).unwrap_err();
    assert!(error.contexts().iter().any(|context| matches!(context, zkcl::EvalErrorContext::EmbeddedModule(module) if module == "test-aot-load")));
    assert_eq!(error.source_range(), None);

    file.nodes.push(LiteralNode::Nil);
    let foreign = ["aarch64-apple-ios", "x86_64-unknown-linux-gnu"]
        .into_iter()
        .map(|triple| triple.parse().unwrap())
        .find(|target| !file.target.is_compatible_with(target))
        .unwrap();
    let mut incompatible = file.clone();
    incompatible.target = foreign;
    let error = runtime
        .load_aot_module(descriptor(incompatible.encode().unwrap()))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("incompatible with requested target")
    );
    let corrected = descriptor(file.encode().unwrap());

    runtime.eval("nil").unwrap();
    runtime.set_step_budget(Some(100));
    let error = runtime.eval("(loop)").err().unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);
    assert!(error.to_string().contains("execution step budget exceeded"));

    runtime.load_aot_module(corrected).unwrap(); // Outermost load gets a fresh budget.

    runtime
        .register_function("LOAD-AOT-WITH-EXHAUSTED-BUDGET", move |runtime, _| {
            let error = runtime.eval("(loop)").err().unwrap();
            assert_eq!(error.kind(), EvalErrorKind::Cancelled);
            let error = runtime.load_aot_module(corrected).unwrap_err();
            assert_eq!(error.kind(), EvalErrorKind::Cancelled);
            Ok(vec![])
        })
        .unwrap();
    let error = runtime
        .eval("(load-aot-with-exhausted-budget)")
        .err()
        .unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);

    runtime.set_step_budget(Some(0));
    let error = runtime.load_aot_module(corrected).unwrap_err();
    assert_eq!(error.kind(), EvalErrorKind::Cancelled);
    let duplicate = descriptor(file.encode().unwrap());
    let error = runtime.register_aot_module(duplicate).unwrap_err();
    assert!(error.to_string().contains("already registered"));

    runtime.set_step_budget(Some(100));
    runtime.load_aot_module(corrected).unwrap();
}
