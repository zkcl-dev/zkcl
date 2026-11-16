use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use zkcl_bytecode::{
    Arity, ArrayContents, ArrayElement, BytecodeFunction, Callee, CompiledFile, DebugVariable,
    DebugVariableLocation, Immediate, Instruction, LiteralNode,
};
use zkcl_js::{emit_compiled_file, emit_compiled_files, emit_project};
use zkcl_target::CompilationTarget;

fn function(
    instructions: Vec<Instruction>,
    constants: Vec<u32>,
    registers: usize,
) -> BytecodeFunction {
    BytecodeFunction {
        name: Some("EMISSION-TEST".into()),
        source_file: None,
        debug: Default::default(),
        arity: Arity::ZERO,
        registers,
        instructions,
        constants,
        lambda_list: None,
        captures: vec![],
        notinline: vec![],
        vop_calls: Vec::new(),
        handlers: vec![],
        restarts: vec![],
        keywords: vec![],
        jump_tables: vec![],
        facts: Default::default(),
    }
}

fn file(nodes: Vec<LiteralNode>, entries: Vec<BytecodeFunction>) -> CompiledFile {
    CompiledFile {
        target: CompilationTarget::javascript(),
        source_files: vec![],
        nodes,
        entries,
    }
}

struct Artifact(PathBuf);

impl Artifact {
    fn new(name: &str) -> Self {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let directory = workspace
            .join(".agents/tmp/zkcl/js-backend/emitter")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&directory).unwrap();
        Self(directory)
    }

    fn write(&self, name: &str, source: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, source).unwrap();
        path
    }
}

impl Drop for Artifact {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node(module: &Path, checks: &str) {
    use std::io::Write;

    let path = serde_json::to_string(module.to_str().unwrap()).unwrap();
    let script = format!(
        "import assert from 'node:assert/strict';\nimport {{pathToFileURL}} from 'node:url';\nconst moduleUrl = pathToFileURL({path}).href;\nconst module = await import(moduleUrl);\n{checks}\n"
    );
    // AOT output must run even when the host disallows string compilation.
    let mut child = Command::new("node")
        .args([
            "--disallow-code-generation-from-strings",
            "--enable-source-maps",
            "--input-type=module",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Node.js is required to verify standalone JavaScript output");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "Node failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn source_maps_trace_unicode_and_macro_sites_across_files() {
    let artifacts = Artifact::new("source-maps");
    let unicode = "(defun unicode-failure () (length \"😀\") (throw 'absent :unicode))\r\n";
    let macro_source =
        "(defmacro fail-here () '(throw 'absent :macro))\n(defun macro-failure () (fail-here))\n";
    let first = artifacts.write("unicode 😀 #1.lisp", unicode);
    let second = artifacts.write("generated macro.lisp", macro_source);
    let runtime = zkcl_runtime::Runtime::builder()
        .compilation_target(CompilationTarget::javascript())
        .start()
        .unwrap();
    let mut files = [
        runtime.compile_file_to_bytecode(&first).unwrap(),
        runtime.compile_file_to_bytecode(&second).unwrap(),
    ];
    // Exercise the inline sourcesContent rather than reading these back from disk.
    std::fs::remove_file(first).unwrap();
    std::fs::remove_file(second).unwrap();
    artifacts.write("runtime.mjs", zkcl_js::emit_runtime_module());
    let standalone = artifacts.write("standalone.mjs", emit_compiled_files(&files).unwrap());
    let split = artifacts.write(
        "split.mjs",
        zkcl_js::emit_compiled_files_with_runtime(&files, "./runtime.mjs").unwrap(),
    );
    for module in [standalone, split] {
        node(
            &module,
            r#"
const { findSourceMap } = await import('node:module');
const { fileURLToPath } = await import('node:url');
const runtime = module.instantiate();
const sources = module.programs.map(program => program.source_files[0]);
const { payload } = findSourceMap(moduleUrl);
assert.deepEqual(payload.sources.map(fileURLToPath), sources.map(source => source.path));
assert.deepEqual(payload.sourcesContent, sources.map(source => source.text));
for (const [index, name, token, line] of [
  [0, 'UNICODE-FAILURE', 'throw', 0],
  [1, 'MACRO-FAILURE', '(fail-here)', 1],
]) {
  const { path, text } = sources[index];
  const column = text.split(/\r?\n/)[line].indexOf(token);
  assert(column >= 0);
  assert.throws(() => runtime.call(name), error => {
    assert.equal(error.className, 'CONTROL-ERROR');
    assert(error.stack.includes(`${path}:${line + 1}:${column + 1}`), error.stack);
    return true;
  });
}
"#,
        );
    }

    for file in &mut files {
        file.strip_debug();
    }
    assert!(
        !emit_compiled_files(&files)
            .unwrap()
            .contains("//# sourceMappingURL=")
    );
}

#[test]
fn rejects_machine_targets_and_invalid_bytecode_before_emitting() {
    let mut code = file(vec![], vec![function(vec![], vec![], 0)]);
    code.target = CompilationTarget::native();
    assert!(
        emit_compiled_file(&code)
            .unwrap_err()
            .to_string()
            .contains("incompatible")
    );
    code.target = CompilationTarget::javascript();
    code.entries[0]
        .instructions
        .push(Instruction::Load(0, Immediate::Nil));
    assert!(
        emit_compiled_file(&code)
            .unwrap_err()
            .to_string()
            .contains("register")
    );
}

#[test]
fn emits_function_entrypoints_and_preserves_source_pc_metadata() {
    let code = file(
        vec![],
        vec![function(
            vec![
                Instruction::Load(0, Immediate::Fixnum(17)),
                Instruction::SetValues(0..1),
            ],
            vec![],
            1,
        )],
    );
    let output = emit_compiled_file(&code).unwrap();
    // Compiler frontend may contain instruction data; the generated section has none.
    let (_, program) = output.split_once("// Compiled Lisp code.").unwrap();
    assert!(!program.contains("\"instructions\""));
    assert!(program.contains("function step_EMISSION_TEST_0_e0(task, frame)"));
    assert!(program.contains("\n      r[0] = 17;"));
    assert!(
        program
            .lines()
            .any(|line| line.trim() == "name: \"EMISSION-TEST\",")
    );
    assert!(program.lines().any(|line| line.trim() == "registers: 1,"));
    assert!(!program.contains("const operands"));
    assert!(!program.contains("task.runtime.objects"));
    assert_eq!(program.matches("task.yielded = true;").count(), 1);
    let artifact = Artifact::new("entrypoints");
    let module = artifact.write("program.mjs", output);
    node(
        &module,
        r#"
const [entry] = module.program.entries;
assert.equal(entry.step.name, 'step_EMISSION_TEST_0_e0');
assert.equal('instructions' in entry, false);
assert.equal(entry.instruction_count, 2);
assert.deepEqual(entry.source_pcs, [null, null]);
assert.deepEqual(entry.operands, []);
assert.deepEqual((await module.run()).values, [17]);
const runtime = module.instantiate();
const task = runtime.createTask(runtime.makeFunction(entry, { constants: [] }));
task.run({budget: 1});
assert.equal(task.currentFrame.pc, 1);
assert.equal(task.currentFrame.sourcePc, 0);
assert.deepEqual(task.values, []);
task.run({budget: 1});
assert.equal(task.done, true);
assert.deepEqual(task.values, [17]);
"#,
    );
}

#[test]
fn packed_select_and_synchronization_run_in_a_split_aot_module() {
    let artifact = Artifact::new("async-select");
    let input = artifact.write(
        "async.lisp",
        r#"
(defun async-answer ()
  (let ((channel (zk-async:make-channel :capacity 0))
        (semaphore (zk-async:make-semaphore)))
    (zk-async:with-task-group (group)
      (zk-async:spawn-task (group)
        (zk-async:with-semaphore (semaphore)
          (zk-async:channel-send channel 42)))
      (zk-async:select
        ((:recv channel item ok) (values item ok))
        ((:timeout 5) (error "Channel did not deliver the item"))))))
"#,
    );
    let runtime = zkcl_runtime::Runtime::builder()
        .compilation_target(CompilationTarget::javascript())
        .start()
        .unwrap();
    let file = runtime.compile_file_to_bytecode(&input).unwrap();
    artifact.write("runtime.mjs", zkcl_js::emit_runtime_module());
    let emitted = zkcl_js::emit_compiled_files_with_runtime(&[file], "./runtime.mjs").unwrap();
    assert!(emitted.contains("ZK::%SELECT-PACKED"));
    let module = artifact.write("program.mjs", emitted);
    node(
        &module,
        r#"
const runtime = module.instantiate();
const result = await runtime.callAsync('ASYNC-ANSWER', [], { slice: 1 });
assert.deepEqual(result.values, [42, runtime.objects.T]);
"#,
    );
}

#[test]
fn binding_multiple_values_consumes_them_in_fast_and_resumable_bodies() {
    let compiled = file(
        vec![],
        vec![function(
            vec![
                Instruction::Load(0, Immediate::Fixnum(17)),
                Instruction::SetValues(0..1),
                Instruction::BindValues(1..2),
                Instruction::ReadValue(2, 0),
                Instruction::SetValues(1..3),
            ],
            vec![],
            3,
        )],
    );
    let artifact = Artifact::new("bind-values");
    let module = artifact.write("program.mjs", emit_compiled_file(&compiled).unwrap());
    node(
        &module,
        r#"
const runtime = module.instantiate();
const entry = module.program.entries[0];
assert.equal(typeof entry.fast, 'function');
const expected = [17, runtime.objects.NIL];
assert.deepEqual(runtime.run().values, expected);
const task = runtime.createTask(runtime.makeFunction(entry));
while (!task.done) task.run({ budget: 1 });
assert.deepEqual(task.values, expected);
"#,
    );
}

#[test]
fn symbol_comments_escape_names_and_preserve_stepping() {
    let unusual_name = "mix: \"λ\\\r\n\t\u{2028}\u{2029}\u{202e}*/";
    let call = |index, at| Instruction::Call {
        callee: Callee::Named(index),
        out: Some(0),
        args: 0..1,
        at,
        associate: false,
        notinline: false,
    };
    let compiled = file(
        vec![
            LiteralNode::InternedSymbol {
                package: "COMMON-LISP".into(),
                name: "IDENTITY".into(),
            },
            LiteralNode::InternedSymbol {
                package: "COMMON-LISP-USER".into(),
                name: unusual_name.into(),
            },
            LiteralNode::Function(function(
                vec![
                    Instruction::Constant(0, 1),
                    call(0, 10),
                    call(1, 20),
                    Instruction::SetValues(0..1),
                ],
                vec![0, 1],
                1,
            )),
        ],
        vec![function(
            vec![Instruction::Constant(0, 0), Instruction::SetValues(0..1)],
            vec![2],
            1,
        )],
    );
    let artifact = Artifact::new("symbol-comments");
    artifact.write("runtime.mjs", zkcl_js::emit_runtime_module());
    let emitted = zkcl_js::emit_compiled_files_with_runtime(&[compiled], "./runtime.mjs").unwrap();
    let module = artifact.write("program.mjs", emitted);
    node(
        &module,
        r#"
const runtime = module.instantiate();
const [code] = runtime.run().values;
const label = String.raw`COMMON-LISP-USER::"mix: \"λ\\\r\n\t\u{2028}\u{2029}\u{202e}*/"`;
const bodyLines = entry => entry.toString().split('\n').map(line => line.trim());
const stepLines = bodyLines(code.entry.step);
assert.ok(stepLines.includes(`r[0] = frame.constants[1]; // ${label}`));
assert.ok(bodyLines(code.entry.fast).includes(`r0 = constants[1]; // ${label}`));
const callLines = stepLines.filter(line => line.startsWith('task.ops.Call('));
assert.deepEqual(callLines, [
  'task.ops.Call(task, frame, operands[0]); // COMMON-LISP::IDENTITY',
  `task.ops.Call(task, frame, operands[1]); // ${label}`,
]);
assert.deepEqual(code.entry.source_pcs, [null, 10, 20, null]);
const symbol = runtime.intern('mix: "λ\\\r\n\t\u2028\u2029\u202e*/', 'COMMON-LISP-USER');
runtime.registerFunction(symbol, ([value]) => {
  assert.equal(value, symbol);
  return 8;
});
const fn = runtime.makeFunction(code);
assert.deepEqual(runtime.call(fn).values, [8]);
const task = runtime.createTask(fn);
for (const [pc, sourcePc] of [[1, 0], [2, 1], [3, 2]]) {
  task.run({ budget: 1 });
  assert.equal(task.currentFrame?.pc, pc);
  assert.equal(task.currentFrame?.sourcePc, sourcePc);
}
task.run({ budget: 1 });
assert.equal(task.done, true);
assert.deepEqual(task.values, [8]);
"#,
    );
}

#[test]
fn aot_multiple_values_obey_the_runtime_limit() {
    let body = file(
        vec![],
        vec![function(
            vec![Instruction::SetValues(0..65_536)],
            vec![],
            65_536,
        )],
    );
    let artifact = Artifact::new("multiple-values-limit");
    let module = artifact.write("program.mjs", emit_compiled_file(&body).unwrap());
    node(
        &module,
        "assert.throws(() => module.run(), error => error.className === 'PROGRAM-ERROR' && /MULTIPLE-VALUES-LIMIT/.test(error.message));",
    );
}

#[test]
fn standalone_module_reconstructs_sharing_cycles_and_exact_numeric_bits() {
    let nodes = vec![
        LiteralNode::Fixnum(7),
        LiteralNode::Pair(0, 1),
        LiteralNode::DoubleFloat((-0.0_f64).to_bits()),
        LiteralNode::DoubleFloat(f64::from_bits(1).to_bits()),
        LiteralNode::SingleFloat((-0.0_f32).to_bits()),
        LiteralNode::Bignum {
            negative: false,
            magnitude: vec![1, 0, 0, 0, 0, 0, 0, 128],
        },
        LiteralNode::RandomState([1, u64::MAX, 1 << 63, 9_007_199_254_740_993]),
        LiteralNode::Array {
            dimensions: vec![2],
            element: ArrayElement::DoubleFloat,
            adjustable: false,
            fill_pointer: None,
            contents: ArrayContents::DoubleFloats(vec![(-0.0_f64).to_bits(), 1]),
        },
    ];
    let constants = vec![1, 1, 2, 3, 4, 5, 6, 7];
    let mut instructions: Vec<_> = (0..constants.len())
        .map(|index| Instruction::Constant(index, index))
        .collect();
    instructions.push(Instruction::SetValues(0..constants.len()));
    let code = file(nodes, vec![function(instructions, constants, 8)]);
    let artifact = Artifact::new("literal-graph");
    let module = artifact.write("program.mjs", emit_compiled_file(&code).unwrap());
    node(
        &module,
        r#"
assert.equal(module.program.nodes[2].DoubleFloat, '9223372036854775808');
assert.equal(module.program.nodes[6].RandomState[1], '18446744073709551615');
const rt = module.instantiate();
const values = rt.run().values;
assert.equal(values[0], values[1]);
assert.equal(values[0].cdr, values[0]);
assert.equal(values[0].car, 7);
assert.equal(values[2].bits, 9223372036854775808n);
assert(Object.is(values[2].value, -0));
assert.equal(values[3].bits, 1n);
assert.equal(values[3].value, Number.MIN_VALUE);
assert(Object.is(values[4].value, -0));
assert.equal(values[5], 9223372036854775809n);
assert.equal(rt.print(values[6]), '#Y"0000000000000001FFFFFFFFFFFFFFFF80000000000000000020000000000001"');
assert.equal(rt.toJS(rt.call('RANDOM-STATE-P', [values[6]]).values[0]), true);
const copy = rt.call('MAKE-RANDOM-STATE', [values[6]]).values[0];
assert.notEqual(copy, values[6]);
assert.equal(rt.call('RANDOM', [2n ** 83n, copy]).values[0], rt.call('RANDOM', [2n ** 83n, values[6]]).values[0]);
assert(Object.is(values[7].get(0).value, -0));
assert.equal(values[7].get(1).value, Number.MIN_VALUE);
"#,
    );
}

#[test]
fn rust_compiled_source_runs_aot_closures_and_multiple_values_after_source_removal() {
    let artifact = Artifact::new("compiled-source");
    let input = artifact.write(
        "input.lisp",
        r#"
(let ((counter 0))
  (labels ((next () (incf counter)))
    (values (next) (next))))
"#,
    );
    let runtime = zkcl_runtime::Runtime::builder()
        .compilation_target(CompilationTarget::javascript())
        .start()
        .unwrap();
    let code = runtime.compile_file_to_bytecode(&input).unwrap();
    let module = artifact.write("program.mjs", emit_compiled_file(&code).unwrap());
    std::fs::remove_file(input).unwrap();
    node(
        &module,
        "assert.deepEqual((await module.run()).values, [1,2]);\nfor (const node of module.program.nodes) if (node.Function) assert.equal(typeof node.Function.step, 'function');",
    );
}

#[test]
fn rust_compiled_formatter_remains_callable_in_a_standalone_module() {
    let artifact = Artifact::new("compiled-format");
    let input = artifact.write(
        "format.lisp",
        r#"
(defun format-entry ()
  (let ((one (formatter "(~D)")))
    (values (format nil "~@?~D" one 1 2)
            (with-output-to-string (out)
              (format out "~{~D~^, ~}" '(3 4)))
            (with-output-to-string (out)
              (let ((*print-pretty* t))
                (pprint-logical-block (out '(a b) :prefix "[" :suffix "]")
                  (write (pprint-pop) :stream out)
                  (format out "~:@_")
                  (write (pprint-pop) :stream out)))))))
"#,
    );
    let runtime = zkcl_runtime::Runtime::builder()
        .compilation_target(CompilationTarget::javascript())
        .start()
        .unwrap();
    let code = runtime.compile_file_to_bytecode(&input).unwrap();
    let module = artifact.write("program.mjs", emit_compiled_file(&code).unwrap());
    node(
        &module,
        r#"
const rt = module.instantiate();
assert.deepEqual(rt.call('FORMAT-ENTRY').values.map(value => rt.toJS(value)), ['(1)2', '3, 4', '[A\n B]']);
"#,
    );
}

#[test]
fn rust_pathname_literals_share_identity_within_each_js_load() {
    let artifact = Artifact::new("compiled-pathnames");
    let input = artifact.write(
        "pathnames.lisp",
        r#"
(eval-when (:compile-toplevel :load-toplevel :execute)
  (setf (logical-pathname-translations "JSAPP")
        '(("**;*.*.*" "/sandbox/**/*.*"))))
(defun pathname-literals ()
  (values #P"/app/资源.v1.lisp" #P"JSAPP:SRC;TASK.LISP.42"))
(multiple-value-bind (physical logical) (pathname-literals)
  (values (pathnamep physical) (typep logical 'logical-pathname)
          (namestring physical) (namestring logical)
          (namestring (translate-logical-pathname logical))))
"#,
    );
    let runtime = zkcl_runtime::Runtime::builder()
        .compilation_target(CompilationTarget::javascript())
        .start()
        .unwrap();
    let code = runtime.compile_file_to_bytecode(&input).unwrap();
    assert!(
        code.nodes
            .iter()
            .filter(|node| matches!(node, LiteralNode::Pathname(_)))
            .count()
            >= 2
    );
    let module = artifact.write("program.mjs", emit_compiled_file(&code).unwrap());
    node(
        &module,
        r#"
const rt = module.instantiate();
assert.deepEqual(rt.run().values.map(value => rt.toJS(value)),
  [true, true, '/app/资源.v1.lisp', 'JSAPP:SRC;TASK.LISP.42', '/sandbox/src/task.lisp']);
const first = rt.call('PATHNAME-LITERALS').values;
const again = rt.call('PATHNAME-LITERALS').values;
const fn = rt.readFunction('PATHNAME-LITERALS');
assert.equal(typeof (fn.code ?? fn).entry.step, 'function');
assert.equal(first[0], again[0]);
assert.equal(first[1], again[1]);
assert.deepEqual(first.map(value => rt.evalString(`(type-of ${rt.print(value, {readably: true})})`).values[0].name),
  ['PATHNAME', 'LOGICAL-PATHNAME']);
rt.load(module.program);
const second = rt.call('PATHNAME-LITERALS').values;
for (let i = 0; i < first.length; i++) {
  assert.notEqual(first[i], second[i]);
  assert.equal(rt.toJS(rt.call('EQUAL', [first[i], second[i]]).values[0]), true);
}
"#,
    );
}

#[test]
fn several_files_execute_in_order_with_one_runtime_and_separate_literal_graphs() {
    let shared = || LiteralNode::InternedSymbol {
        package: "COMMON-LISP-USER".into(),
        name: "JS-SHARED".into(),
    };
    let first = file(
        vec![shared(), LiteralNode::Fixnum(9)],
        vec![function(
            vec![
                Instruction::Constant(0, 1),
                Instruction::WriteCell { symbol: 0, src: 0 },
                Instruction::SetValues(0..1),
            ],
            vec![0, 1],
            1,
        )],
    );
    let second = file(
        vec![shared()],
        vec![function(
            vec![
                Instruction::ReadCell {
                    out: 0,
                    symbol: 0,
                    function: false,
                    at: 42,
                },
                Instruction::SetValues(0..1),
            ],
            vec![0],
            1,
        )],
    );
    let artifact = Artifact::new("several-files");
    let module = artifact.write(
        "program.mjs",
        emit_compiled_files(&[first, second]).unwrap(),
    );
    node(
        &module,
        "assert.equal(module.programs.length, 2);\nassert.notEqual(module.programs[0].entries[0].step.name, module.programs[1].entries[0].step.name);\nassert.deepEqual(module.programs[1].entries[0].source_pcs, [42,null]);\nassert.deepEqual((await module.run()).values, [9]);",
    );
}

#[test]
fn project_entry_is_validated_and_called_after_loading() {
    for entry in [
        "MAIN",
        ":MAIN",
        "CL-USER:",
        "CL-USER:::MAIN",
        "CL-USER:MAIN:MORE",
        "CL USER:MAIN",
    ] {
        assert!(
            emit_project(&[], entry)
                .unwrap_err()
                .to_string()
                .contains("package-qualified project entry")
        );
    }
    let artifact = Artifact::new("project-entry");
    let input = artifact.write("input.lisp", "(defun main () (values 33 34))\n(values 99)");
    let runtime = zkcl_runtime::Runtime::builder()
        .compilation_target(CompilationTarget::javascript())
        .start()
        .unwrap();
    let code = runtime.compile_file_to_bytecode(&input).unwrap();
    let module = artifact.write(
        "program.mjs",
        emit_project(&[code], "CL-USER::MAIN").unwrap(),
    );
    node(
        &module,
        "assert.deepEqual((await module.run()).values, [33,34]);",
    );
}

#[test]
fn function_literals_can_reference_themselves() {
    let body = function(
        vec![Instruction::Constant(0, 0), Instruction::SetValues(0..1)],
        vec![0],
        1,
    );
    let code = file(vec![LiteralNode::Function(body.clone())], vec![body]);
    let artifact = Artifact::new("function-cycle");
    let module = artifact.write("program.mjs", emit_compiled_file(&code).unwrap());
    node(
        &module,
        "const [fn] = (await module.run()).values;\nassert.equal(fn.constants[0], fn);",
    );
}

#[test]
fn signed_source_revisions_are_exact_and_unrepresentable_positions_are_rejected() {
    let mut code = file(vec![], vec![]);
    let mut source = zkcl_bytecode::CompiledSource::from("revision.lisp");
    source.version = Some(i64::MAX);
    code.source_files.push(source);
    let artifact = Artifact::new("source-revision");
    let module = artifact.write("program.mjs", emit_compiled_file(&code).unwrap());
    node(
        &module,
        "assert.equal(module.program.source_files[0].version, '9223372036854775807');",
    );
    if usize::BITS > 53 {
        code.source_files[0].base_offset = usize::try_from(1_u64 << 53).unwrap();
        assert!(
            emit_compiled_file(&code)
                .unwrap_err()
                .to_string()
                .contains("exact integer range")
        );
    }
}

#[test]
fn generated_modules_link_into_another_bundles_runtime() {
    let artifact = Artifact::new("shared-runtime");
    let a = artifact.write("a.mjs", emit_compiled_file(&file(vec![], vec![])).unwrap());
    let mut body = function(
        vec![
            Instruction::Unless(0, 4),
            Instruction::Load(1, Immediate::Fixnum(1)),
            Instruction::SetValues(1..2),
            Instruction::Jump(6),
            Instruction::Load(1, Immediate::Fixnum(2)),
            Instruction::SetValues(1..2),
        ],
        vec![],
        2,
    );
    body.arity = Arity {
        required: 1,
        maximum: Some(1),
    };
    let code = file(
        vec![LiteralNode::Function(body)],
        vec![function(
            vec![
                Instruction::Constant(0, 0),
                Instruction::Load(1, Immediate::Character('λ')),
                Instruction::SetValues(0..2),
            ],
            vec![0],
            2,
        )],
    );
    let b = artifact.write("b.mjs", emit_compiled_file(&code).unwrap());
    let path = serde_json::to_string(b.to_str().unwrap()).unwrap();
    node(
        &a,
        &format!(
            r#"
const other = await import(pathToFileURL({path}).href);
const rt = module.createRuntime();
const [fn, char] = rt.load(other.program).values;
assert.deepEqual(rt.call(fn, [rt.objects.NIL]).values, [2]);
assert.deepEqual(rt.call(fn, [rt.objects.T]).values, [1]);
const branched = rt.createTask(fn, [rt.objects.NIL]);
branched.run({{budget: 1}});
assert.equal(branched.currentFrame.pc, 4);
assert.equal(branched.currentFrame.sourcePc, 0);
branched.run({{budget: 1}});
assert.equal(branched.currentFrame.pc, 5);
assert.equal(branched.currentFrame.sourcePc, 4);
branched.run({{budget: 1}});
assert.deepEqual(branched.values, [2]);
assert.equal(branched.done, true);
assert(char instanceof rt.objects.LispCharacter);
assert.equal(rt.call('CHARACTERP', [char]).values[0], rt.objects.T);
"#
        ),
    );
}

#[test]
fn native_image_object_kinds_are_rejected_before_deployment() {
    for node in [
        LiteralNode::RuntimeObject {
            kind: "ExampleObject".into(),
            data: vec![],
            slots: vec![],
        },
        LiteralNode::ExternalResource {
            key: "ExampleResource".into(),
            data: vec![],
        },
    ] {
        assert!(
            emit_compiled_file(&file(vec![node], vec![]))
                .unwrap_err()
                .to_string()
                .contains("native-image literal kind")
        );
    }
}

#[test]
#[cfg(target_pointer_width = "64")]
fn inline_instruction_indices_must_be_exact_js_integers() {
    let code = file(
        vec![],
        vec![function(
            vec![Instruction::ReadValue(0, 1 << 53)],
            vec![],
            1,
        )],
    );
    assert!(matches!(
        emit_compiled_file(&code),
        Err(zkcl_js::EmitError::IntegerRange { .. })
    ));
}

#[test]
fn builtin_native_installer_table_matches_runtime() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let installers_path = manifest_dir.join("runtime/installers.mjs");
    let builtins_path = manifest_dir.join("runtime/builtins.mjs");
    let builtins_url_arg = serde_json::to_string(builtins_path.to_str().unwrap()).unwrap();
    let expected: Vec<(u32, &str)> = zkcl_runtime::builtin_installer_descriptors().to_vec();
    let expected_json = serde_json::to_string(&expected).unwrap();

    node(
        &installers_path,
        &format!(
            r#"
const builtins = await import(pathToFileURL({builtins_url_arg}).href);
const expected = {expected_json};

assert.ok(expected.length > 0);
for (const [label, table] of [
  ['installers.mjs', module.BUILTIN_NATIVE_NAMES],
  ['builtins.mjs', builtins.BUILTIN_NATIVE_NAMES],
]) {{
  const actual = Object.entries(table)
    .map(([id, name]) => [Number(id), name])
    .sort((a, b) => a[0] - b[0]);
  const sortedExpected = [...expected].sort((a, b) => a[0] - b[0]);
  assert.deepEqual(actual, sortedExpected, `${{label}} mismatch against zkcl-runtime`);
}}
"#
        ),
    );
}

#[test]
fn structured_fast_mode_executes_branches_and_loops_without_pc_switch() {
    let artifact = Artifact::new("structured-fast");
    let mut fn_code = function(
        vec![
            Instruction::Load(1, Immediate::Fixnum(0)),
            Instruction::UnlessFixnumInRange(0, 1, 5, 7),
            Instruction::Load(1, Immediate::Fixnum(42)),
            Instruction::Unless(0, 6),
            Instruction::Load(0, Immediate::Nil),
            Instruction::Jump(3),
            Instruction::Jump(8),
            Instruction::Load(1, Immediate::Fixnum(7)),
            Instruction::SetValues(1..2),
        ],
        vec![1],
        2,
    );
    fn_code.arity = Arity {
        required: 1,
        maximum: Some(1),
    };
    fn_code.name = Some("FAST-BRANCH-LOOP".into());
    fn_code.debug.variables.push(DebugVariable {
        name: 0,
        live: 0..fn_code.instructions.len(),
        location: DebugVariableLocation::Register {
            index: 0,
            boxed: false,
        },
    });

    let mut compiled = file(
        vec![
            LiteralNode::Function(fn_code),
            LiteralNode::InternedSymbol {
                package: "COMMON-LISP-USER".into(),
                name: "INPUT\nλ".into(),
            },
        ],
        vec![function(
            vec![Instruction::Constant(0, 0), Instruction::SetValues(0..1)],
            vec![0],
            1,
        )],
    );
    let emitted = emit_compiled_file(&compiled).unwrap();
    let fast_start = emitted
        .find("function fast_FAST_BRANCH_LOOP")
        .expect("pure function emits structured fast path");
    let fast_end = emitted[fast_start..]
        .find("\nfunction ")
        .map_or(emitted.len(), |offset| fast_start + offset);
    let fast_body = &emitted[fast_start..fast_end];
    assert!(
        fast_body.contains(
            "\n  let r0_input = regs[0]; // parameter \"INPUT\\nλ\"\n  let r1 = regs[1];\n"
        )
    );
    assert!(fast_body.contains("while (true)"));
    assert!(!fast_body.contains("switch (frame.pc)"));
    // The back edge guards the budget and records the loop header's PC (pc=3 is
    // the `Unless(0, 6)` the `Jump(3)` targets), so `step` can resume there.
    assert!(fast_body.contains("if (task.budget-- <= 0) {"));
    assert!(fast_body.contains("frame.pc = 3;"));
    assert!(fast_body.contains("continue loop_pc3;"));

    compiled.strip_debug();
    assert!(
        !emit_compiled_file(&compiled)
            .unwrap()
            .contains(" // parameter ")
    );
    let module_path = artifact.write("fast.mjs", emitted);
    node(
        &module_path,
        r#"
const rt = module.instantiate();
const [fnCode] = rt.run().values;
assert.equal(fnCode.constants[0], rt.intern('INPUT\nλ', 'COMMON-LISP-USER'));
const fn = rt.makeFunction(fnCode);
assert.equal(typeof fnCode.entry.fast, 'function');
assert.equal(typeof fnCode.entry.step, 'function');
if (!Object.isFrozen(fnCode.entry)) {
  const rawStep = fnCode.entry.step;
  let firstStepPc;
  fnCode.entry.step = (task, frame) => {
    firstStepPc ??= frame.pc;
    return rawStep(task, frame);
  };
  assert.deepEqual(rt.call(fn, [3]).values, [42]);
  assert.equal(firstStepPc, undefined);
  const stepped = rt.createTask(fn, [3]);
  while (!stepped.done) stepped.run({ budget: 1 });
  assert.equal(firstStepPc, 0);
  assert.deepEqual(stepped.values, [42]);
  firstStepPc = undefined;
  const sliced = await rt.createTask(fn, [3]).runAsync({ slice: 1 });
  assert.equal(firstStepPc, 3);
  assert.deepEqual(sliced.values, [42]);
  fnCode.entry.step = rawStep;
}

for (const arg of [1, 5, 6, 0, rt.objects.NIL]) {
  const fastResult = rt.call(fn, [arg]).values;
  const stepped = rt.createTask(fn, [arg]);
  while (!stepped.done) stepped.run({ budget: 1 });
  assert.deepEqual(fastResult, stepped.values);
}
assert.deepEqual(rt.call(fn, [3]).values, [42]);
assert.deepEqual(rt.call(fn, [9]).values, [7]);
const yielded = rt.createTask(fn, [3]);
const completion = yielded.runAsync({ slice: 1 });
assert.equal(yielded.currentFrame.pc, 3);
assert.equal(yielded.currentFrame.sourcePc, 5);
assert.deepEqual((await completion).values, [42]);
"#,
    );
}

#[test]
fn structured_calls_commit_branch_writes_and_yield_on_same_pc_reactivation() {
    let call = |callee, out, args| Instruction::Call {
        callee: Callee::Named(callee),
        out,
        args,
        at: 0,
        associate: false,
        notinline: false,
    };
    let mut code = function(
        vec![
            Instruction::Load(1, Immediate::Fixnum(11)),
            Instruction::Unless(0, 4),
            Instruction::Load(2, Immediate::Fixnum(22)),
            Instruction::Jump(5),
            Instruction::Load(2, Immediate::Fixnum(44)),
            call(0, Some(3), 2..3),
            call(1, None, 3..4),
            Instruction::SetValues(2..3),
        ],
        vec![0, 1],
        4,
    );
    code.arity = Arity {
        required: 1,
        maximum: Some(1),
    };
    let compiled = file(
        ["OBSERVE-FAST-FRAME", "REACTIVATE-FAST-FRAME"]
            .into_iter()
            .map(|name| LiteralNode::InternedSymbol {
                package: "COMMON-LISP-USER".into(),
                name: name.into(),
            })
            .collect(),
        vec![code],
    );
    let artifacts = Artifact::new("structured-writeback");
    let program = artifacts.write("program.mjs", emit_compiled_file(&compiled).unwrap());
    node(
        &program,
        r#"
const rt = module.createRuntime();
const pkg = 'COMMON-LISP-USER';
const [entry] = module.program.entries;
const fast = entry.fast.toString();
assert.equal((fast.match(/regs\[1\] = r1;/g) ?? []).length, 1);
assert.equal((fast.match(/regs\[2\] = r2;/g) ?? []).length, 1);
const names = ['OBSERVE-FAST-FRAME', 'REACTIVATE-FAST-FRAME'];
const constants = names.map(name => rt.intern(name, pkg));
const observed = [];
rt.registerFunction(constants[0], ([value], _runtime, task) => {
  assert.equal(task.currentFrame.registers[1], 11);
  observed.push(value);
  return value + 1;
});
rt.registerFunction(constants[1], ([value], _runtime, task) => {
  const frame = task.currentFrame;
  assert.equal(frame.registers[2] + 1, value);
  task.resume(frame, frame.pc, 2);
  frame.registers[2] = 99;
  return -1;
});
const fn = rt.makeFunction(entry, { constants });
assert.deepEqual(rt.call(fn, [rt.objects.T]).values, [99]);
const stepped = rt.createTask(fn, [rt.objects.NIL]);
while (!stepped.done) stepped.run({ budget: 1 });
assert.deepEqual(stepped.values, [99]);
assert.deepEqual(observed, [22, 44]);
"#,
    );
}

#[test]
fn structured_fast_mode_reads_current_arguments_after_release() {
    let mut fn_code = function(
        vec![
            Instruction::ReleaseArguments,
            Instruction::OptionalArgument(0, 0),
            Instruction::IfPresent(0, 5),
            Instruction::Load(0, Immediate::Fixnum(11)),
            Instruction::Jump(6),
            Instruction::Load(0, Immediate::Fixnum(22)),
            Instruction::SetValues(0..1),
        ],
        vec![],
        1,
    );
    fn_code.arity = Arity {
        required: 0,
        maximum: Some(1),
    };
    let compiled = file(
        vec![LiteralNode::Function(fn_code)],
        vec![function(
            vec![Instruction::Constant(0, 0), Instruction::SetValues(0..1)],
            vec![0],
            1,
        )],
    );
    let artifact = Artifact::new("structured-release-arguments");
    let module = artifact.write("program.mjs", emit_compiled_file(&compiled).unwrap());
    node(
        &module,
        r#"
const runtime = module.instantiate();
const [code] = runtime.run().values;
const fn = runtime.makeFunction(code);
assert.equal(typeof code.entry.fast, 'function');
assert.deepEqual(runtime.call(fn, [17]).values, [11]);
const stepped = runtime.createTask(fn, [17]);
while (!stepped.done) stepped.run({ budget: 1 });
assert.deepEqual(stepped.values, [11]);
"#,
    );
}

#[test]
fn structured_fast_mode_emits_label_free_linear_code_and_if_else() {
    let linear = function(
        vec![
            Instruction::Load(0, Immediate::Fixnum(17)),
            Instruction::SetValues(0..1),
        ],
        vec![],
        1,
    );
    let mut diamond = function(
        vec![
            Instruction::Unless(0, 3),
            Instruction::Load(1, Immediate::Fixnum(11)),
            Instruction::Jump(4),
            Instruction::Load(1, Immediate::Fixnum(22)),
            Instruction::SetValues(1..2),
        ],
        vec![],
        2,
    );
    diamond.arity = Arity {
        required: 1,
        maximum: Some(1),
    };
    let compiled = file(
        vec![
            LiteralNode::Function(linear),
            LiteralNode::Function(diamond),
        ],
        vec![function(
            vec![
                Instruction::Constant(0, 0),
                Instruction::Constant(1, 1),
                Instruction::SetValues(0..2),
            ],
            vec![0, 1],
            2,
        )],
    );
    let artifact = Artifact::new("structured-acyclic-labels");
    artifact.write("runtime.mjs", zkcl_js::emit_runtime_module());
    let emitted = zkcl_js::emit_compiled_files_with_runtime(&[compiled], "./runtime.mjs").unwrap();
    let module = artifact.write("program.mjs", emitted);
    node(
        &module,
        r#"
const runtime = module.instantiate();
const [linear, diamond] = runtime.run().values;
const label = /^ +block_pc(\d+): \{$/gm;
assert.doesNotMatch(linear.entry.fast.toString(), label);
// Acyclic bodies have no back edge, so they must not spend or guard the budget.
assert.doesNotMatch(linear.entry.fast.toString(), /task\.budget/);
const source = diamond.entry.fast.toString();
assert.doesNotMatch(source, /task\.budget/);
assert.doesNotMatch(source, label);
assert.match(source, /if \(r0 === NIL\) \{[\s\S]+\} else \{/);

for (const [code, args, expected, positions] of [
  [linear, [], [17], [[1, 0]]],
  [diamond, [runtime.objects.T], [11], [[1, 0], [2, 1], [4, 2]]],
  [diamond, [runtime.objects.NIL], [22], [[3, 0], [4, 3]]],
]) {
  const fn = runtime.makeFunction(code);
  assert.deepEqual(runtime.call(fn, args).values, expected);
  // Run the same metadata and constants through the dispatched step body.
  const stepOnly = runtime.makeFunction({ ...code.entry, fast: undefined }, { constants: code.constants });
  const stepped = runtime.createTask(stepOnly, args);
  for (const [pc, sourcePc] of positions) {
    stepped.run({ budget: 1 });
    assert.equal(stepped.currentFrame?.pc, pc);
    assert.equal(stepped.currentFrame?.sourcePc, sourcePc);
  }
  stepped.run({ budget: 1 });
  assert.equal(stepped.done, true);
  assert.deepEqual(stepped.values, expected);
}
"#,
    );
}
