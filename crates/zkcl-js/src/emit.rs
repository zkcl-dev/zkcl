use std::collections::BTreeSet;
use std::fmt::Write;

use serde_json::Value;
use zkcl_bytecode::{BytecodeFunction, Callee, CompiledFile, Immediate, Instruction, LiteralNode};
use zkcl_target::CompilationTarget;

use crate::source_map::SourceMap;

// Match the JavaScript runtime's exclusive multiple-value limit. Ranges at or
// above it use the runtime operation to signal the standard condition.
const MULTIPLE_VALUES_LIMIT: usize = 65_536;

/// A compiled input cannot be emitted for the JavaScript runtime.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error(transparent)]
    CompiledFile(#[from] zkcl_bytecode::CompiledFileError),
    #[error("could not encode JavaScript metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error(
        "compiled-file index or source position {value} exceeds JavaScript's exact integer range"
    )]
    IntegerRange { value: String },
    #[error(
        "expected a package-qualified project entry PACKAGE:NAME or PACKAGE::NAME, found {entry:?}"
    )]
    InvalidEntry { entry: String },
    #[error("JavaScript compiled files cannot contain the native-image literal kind {kind:?}")]
    UnsupportedLiteral { kind: String },
}

/// Emit one independently deployable ES module, including the shared runtime.
///
/// The module exports its programs, runtime constructor, and synchronous and
/// asynchronous entry points. Retained source snapshots and locations produce
/// an inline source map.
/// JavaScript's binary64 JSON numbers cannot preserve all Lisp integers or float
/// bit patterns: signed literals and 64-bit payloads use decimal strings.
pub fn emit_compiled_file(file: &CompiledFile) -> Result<String, EmitError> {
    emit_compiled_files(std::slice::from_ref(file))
}

/// Emit several compiled files as one module, loading them in input order in a
/// shared runtime. `programs` contains all files; `program` names the first file
/// (or `null` for an empty input). Each file retains its own literal graph.
pub fn emit_compiled_files(files: &[CompiledFile]) -> Result<String, EmitError> {
    emit_module(files, None, None)
}

/// Emit code and literals that import a separately delivered zkcl runtime.
/// `runtime` is an ECMAScript module specifier, relative to the output module
/// or resolved by the embedding host. Emit the runtime with the same compiler.
pub fn emit_compiled_files_with_runtime(
    files: &[CompiledFile],
    runtime: &str,
) -> Result<String, EmitError> {
    emit_module(files, None, Some(runtime))
}

/// Emit a project and call its package-qualified entry after loading its files.
/// Package and symbol names use their exact spelling; entry syntax contains one
/// or two colons and nonempty names without whitespace.
pub fn emit_project(files: &[CompiledFile], entry: &str) -> Result<String, EmitError> {
    emit_project_module(files, entry, None)
}

/// Emit a project as a code module that imports a separately delivered runtime.
pub fn emit_project_with_runtime(
    files: &[CompiledFile],
    entry: &str,
    runtime: &str,
) -> Result<String, EmitError> {
    emit_project_module(files, entry, Some(runtime))
}

/// Emit a reusable ES module with the host-facing JavaScript runtime API.
pub fn emit_runtime_module() -> String {
    let mut output = crate::bundle::runtime_bundle();
    output.push_str(
        "\nexport const { createRuntime, Runtime, Task, MultipleValues, LispCondition, SuspensionError, NIL, T } = __zkcl_modules.runtime;\n",
    );
    output
}

fn emit_project_module(
    files: &[CompiledFile],
    entry: &str,
    runtime: Option<&str>,
) -> Result<String, EmitError> {
    let invalid = || EmitError::InvalidEntry {
        entry: entry.to_owned(),
    };
    let (package, name) = entry.split_once(':').ok_or_else(invalid)?;
    let name = name.strip_prefix(':').unwrap_or(name);
    if [package, name].iter().any(|part| {
        part.is_empty() || part.contains(|c: char| c == ':' || c.is_whitespace() || c.is_control())
    }) {
        return Err(invalid());
    }
    emit_module(files, Some((package, name)), runtime)
}

fn emit_module(
    files: &[CompiledFile],
    entry: Option<(&str, &str)>,
    runtime: Option<&str>,
) -> Result<String, EmitError> {
    for file in files {
        file.check_target(&CompilationTarget::javascript())?;
        for node in &file.nodes {
            match node {
                LiteralNode::RuntimeObject { kind, .. } => {
                    return Err(EmitError::UnsupportedLiteral { kind: kind.clone() });
                }
                LiteralNode::ExternalResource { .. } => {
                    return Err(EmitError::UnsupportedLiteral {
                        kind: "ExternalResource".into(),
                    });
                }
                _ => {}
            }
        }
        file.validate()
            .map_err(zkcl_bytecode::CompiledFileError::from)?;
    }
    let mut programs = String::from("export const program = ");
    let mut steps = Steps::default();
    if let Some(first) = files.first() {
        emit_program(&mut programs, &mut steps, first, 0, "")?;
    } else {
        programs.push_str("null");
    }
    programs.push_str(";\nexport const programs = [");
    if !files.is_empty() {
        programs.push_str("\n  program,\n");
        for (index, file) in files.iter().enumerate().skip(1) {
            programs.push_str("  ");
            emit_program(&mut programs, &mut steps, file, index, "  ")?;
            programs.push_str(",\n");
        }
    }
    programs.push_str("];\n");

    let mut output = if let Some(runtime) = runtime {
        format!(
            "// Generated by zkcl-js.\nimport {{ createRuntime }} from {};\nexport {{ createRuntime }};\n",
            json(&Value::String(runtime.into()))?
        )
    } else {
        let mut output = crate::bundle::runtime_bundle();
        output.push_str("\nexport const createRuntime = __zkcl_modules.runtime.createRuntime;\n");
        output
    };
    output.push_str(
        "\n// Compiled Lisp code. Each case is an instruction and a possible resume point.\n",
    );
    let steps_start = output.len();
    output.push_str(&steps.text);
    output.push_str(&programs);
    output.push_str("\nexport function instantiate(options = {}) {\n  const runtime = createRuntime(options);\n  for (const file of programs) runtime.load(file);\n  return runtime;\n}\n");
    output.push_str("\nexport async function instantiateAsync(options = {}, execution = {}) {\n  const runtime = createRuntime(options);\n  for (const file of programs) await runtime.loadAsync(file, execution);\n  return runtime;\n}\n");
    if let Some((package, name)) = entry {
        let name = json(&Value::String(name.into()))?;
        let package = json(&Value::String(package.into()))?;
        writeln!(output, "\nexport function run(options = {{}}) {{\n  const runtime = instantiate(options);\n  return runtime.call(runtime.intern({name}, {package}));\n}}\n\nexport async function runAsync(options = {{}}, execution = {{}}) {{\n  const runtime = await instantiateAsync(options, execution);\n  return runtime.callAsync(runtime.intern({name}, {package}), [], execution);\n}}").unwrap();
    } else {
        output.push_str(
            "\nexport function run(options = {}) {\n  return instantiate(options).run();\n}\n\nexport async function runAsync(options = {}, execution = {}) {\n  return (await instantiateAsync(options, execution)).run();\n}\n",
        );
    }
    steps.map.append_inline(&mut output, steps_start)?;
    Ok(output)
}

#[derive(Default)]
struct Steps<'a> {
    text: String,
    map: SourceMap<'a>,
}

fn emit_program<'a>(
    output: &mut String,
    steps: &mut Steps<'a>,
    file: &'a CompiledFile,
    file_index: usize,
    indent: &str,
) -> Result<(), EmitError> {
    let mut sources = serde_json::to_value(&file.source_files)?;
    // Source revisions are signed 64-bit values too.
    for source in sources.as_array_mut().expect("source files") {
        if !source["version"].is_null() {
            decimal(&mut source["version"]);
        }
    }
    let target = json(&Value::String(file.target.to_string()))?;
    writeln!(output, "{{\n{indent}  target: {target},").unwrap();
    writeln!(output, "{indent}  source_files: {},", json(&sources)?).unwrap();
    write!(output, "{indent}  nodes: [").unwrap();
    if !file.nodes.is_empty() {
        output.push('\n');
    }
    for (index, node) in file.nodes.iter().enumerate() {
        write!(output, "{indent}    ").unwrap();
        match node {
            LiteralNode::Function(code) => {
                output.push_str("{ Function: ");
                let name = step_name(code.name.as_deref(), file_index, 'n', index);
                emit_function(output, steps, code, &name, indent, file)?;
                output.push_str(" }");
            }
            _ => output.push_str(&json(&literal(node)?)?),
        }
        output.push_str(",\n");
    }
    if !file.nodes.is_empty() {
        write!(output, "{indent}  ").unwrap();
    }
    write!(output, "],\n{indent}  entries: [").unwrap();
    if !file.entries.is_empty() {
        output.push('\n');
    }
    for (index, code) in file.entries.iter().enumerate() {
        let name = step_name(code.name.as_deref(), file_index, 'e', index);
        write!(output, "{indent}    ").unwrap();
        emit_function(output, steps, code, &name, indent, file)?;
        output.push_str(",\n");
    }
    if !file.entries.is_empty() {
        write!(output, "{indent}  ").unwrap();
    }
    write!(output, "],\n{indent}}}").unwrap();
    Ok(())
}

fn step_name(name: Option<&str>, file: usize, kind: char, index: usize) -> String {
    let label: String = name
        .unwrap_or("lambda")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("step_{label}_{file}_{kind}{index}")
}

fn emit_function<'a>(
    output: &mut String,
    compiled: &mut Steps<'a>,
    code: &'a BytecodeFunction,
    name: &str,
    indent: &str,
    file: &'a CompiledFile,
) -> Result<(), EmitError> {
    let mut metadata = serde_json::to_value(code)?;
    let Value::Array(encoded_instructions) = metadata["instructions"].take() else {
        unreachable!("function instructions");
    };
    output.push_str("{\n");
    for (field, value) in metadata.as_object().expect("function object") {
        if field != "instructions" {
            writeln!(output, "{indent}      {field}: {},", json(value)?).unwrap();
        }
    }
    write!(
        output,
        "{indent}      instruction_count: {},\n{indent}      source_pcs: [",
        code.instructions.len()
    )
    .unwrap();
    for (pc, instruction) in code.instructions.iter().enumerate() {
        if pc > 0 {
            output.push(',');
        }
        match instruction.source_offset() {
            Some(offset) => write!(output, "{offset}").unwrap(),
            None => output.push_str("null"),
        }
    }
    let operations: Vec<_> = code
        .instructions
        .iter()
        .zip(encoded_instructions)
        .map(|(instruction, value)| operation(instruction, value))
        .collect::<Result<_, _>>()?;
    let mut objects = BTreeSet::new();
    let inline: Vec<_> = code
        .instructions
        .iter()
        .map(|instruction| inline_operation(instruction, &mut objects))
        .collect();
    let mut operand_indices = vec![None; operations.len()];
    let mut operand_count = 0;
    write!(output, "],\n{indent}      operands: [").unwrap();
    for (pc, (name, payload)) in operations.iter().enumerate() {
        if inline[pc].is_some() || payload.is_null() {
            continue;
        }
        if operand_count == 0 {
            output.push('\n');
        }
        writeln!(
            output,
            "{indent}        {}, // {pc}: {name}",
            json(payload)?
        )
        .unwrap();
        operand_indices[pc] = Some(operand_count);
        operand_count += 1;
    }
    if operand_count != 0 {
        write!(output, "{indent}      ").unwrap();
    }
    let fast_name = format!("fast_{}", name.strip_prefix("step_").unwrap_or(name));
    let has_fast = crate::structured::try_emit_fast_function(
        &mut compiled.text,
        code,
        &fast_name,
        &file.nodes,
    );
    if has_fast {
        write!(output, "],\n{indent}      fast: {fast_name},").unwrap();
    } else {
        output.push_str("],");
    }
    write!(output, "\n{indent}      step: {name}\n{indent}    }}").unwrap();

    let steps = &mut compiled.text;
    let source = code
        .source_file
        .map(|index| &file.source_files[index as usize]);
    if let Some(at) = code.debug.entry {
        compiled.map.mark(
            steps.len() + "function ".len(),
            source,
            &code.debug,
            at,
            code.name.as_deref(),
        );
    }
    writeln!(steps, "function {name}(task, frame) {{").unwrap();
    if operand_count != 0 {
        steps.push_str("  const operands = frame.code.operands;\n");
    }
    if code
        .instructions
        .iter()
        .zip(&inline)
        .any(|(instruction, operation)| {
            operation.is_some()
                && !matches!(
                    instruction,
                    Instruction::Jump(_) | Instruction::ReleaseArguments
                )
        })
    {
        steps.push_str("  const r = frame.registers;\n");
    }
    if !objects.is_empty() {
        let names: Vec<_> = objects.into_iter().collect();
        writeln!(
            steps,
            "  const {{ {} }} = task.runtime.objects;",
            names.join(", ")
        )
        .unwrap();
    }
    steps.push_str("  dispatch: while (true) {\n    switch (frame.pc) {\n");
    for (pc, instruction) in code.instructions.iter().enumerate() {
        let scoped = matches!(
            instruction,
            Instruction::UnlessFixnum(..) | Instruction::UnlessFixnumInRange(..)
        );
        writeln!(steps, "    case {pc}:{}", if scoped { " {" } else { "" }).unwrap();
        steps.push_str(
            "      if (task.budget-- <= 0) break dispatch;\n      frame.sourcePc = frame.pc++;\n",
        );
        if let Some(at) = instruction.source_offset() {
            compiled
                .map
                .mark(steps.len() + 6, source, &code.debug, at, None);
        }
        if let Some(operation) = &inline[pc] {
            operation.emit(steps, pc + 1);
        } else {
            let named_callee = match instruction {
                Instruction::Call {
                    callee: Callee::Named(index),
                    ..
                } => Some(&file.nodes[code.constants[*index] as usize]),
                _ => None,
            };
            emit_helper(
                steps,
                instruction,
                &operations[pc].0,
                operand_indices[pc],
                pc + 1,
                named_callee,
            );
        }
        if scoped {
            steps.push_str("    }\n");
        }
    }
    writeln!(
        steps,
        "    case {}:\n      return task.returnFrame(frame);",
        code.instructions.len()
    )
    .unwrap();
    steps.push_str("    default:\n      throw new Error('invalid generated function PC: ' + frame.pc);\n    }\n  }\n  task.yielded = true;\n}\n\n");
    Ok(())
}

fn emit_helper(
    steps: &mut String,
    instruction: &Instruction,
    name: &str,
    operand_index: Option<usize>,
    next: usize,
    named_callee: Option<&LiteralNode>,
) {
    write!(steps, "      task.ops.{name}(task, frame").unwrap();
    if let Some(index) = operand_index {
        write!(steps, ", operands[{index}]").unwrap();
    }
    steps.push_str(");");
    match named_callee {
        Some(LiteralNode::InternedSymbol { package, name }) => {
            steps.push_str(" // ");
            write_symbol_component(steps, package);
            steps.push_str("::");
            write_symbol_component(steps, name);
        }
        Some(LiteralNode::UninternedSymbol(name)) => {
            steps.push_str(" // #:");
            write_symbol_component(steps, name);
        }
        Some(LiteralNode::Nil) => steps.push_str(" // COMMON-LISP::NIL"),
        Some(LiteralNode::True) => steps.push_str(" // COMMON-LISP::T"),
        _ => {}
    }
    steps.push('\n');
    // A thrown error already leaves the step. Only helpers that invoke Lisp or
    // unwind scopes can return with another frame, suspension or completion.
    let may_leave_frame = matches!(
        instruction,
        Instruction::Call { .. }
            | Instruction::CallValues { .. }
            | Instruction::EndCleanup
            | Instruction::ReturnFrom { .. }
            | Instruction::Throw { .. }
            | Instruction::Go { .. }
    );
    if may_leave_frame {
        steps.push_str(
            "      if (task.currentFrame !== frame || task.suspended || task.done) return;\n",
        );
    }
    if may_leave_frame
        || matches!(
            instruction,
            Instruction::BeginCleanup | Instruction::WhenBound { .. }
        )
    {
        writeln!(steps, "      if (frame.pc !== {next}) continue dispatch;").unwrap();
    }
}

pub(super) fn write_symbol_component(out: &mut String, text: &str) {
    if !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && !matches!(byte, b':' | b'"' | b'\\' | b'|'))
    {
        out.push_str(text);
    } else {
        write!(out, "{text:?}").unwrap();
    }
}

enum InlineOperation {
    Statement(String),
    Branch {
        target: usize,
        test: Option<String>,
        local: Option<String>,
    },
}

impl InlineOperation {
    fn emit(&self, steps: &mut String, next: usize) {
        match self {
            Self::Statement(statement) => writeln!(steps, "      {statement}").unwrap(),
            Self::Branch {
                target,
                test,
                local,
            } if *target != next => {
                if let Some(local) = local {
                    writeln!(steps, "      {local}").unwrap();
                }
                if let Some(test) = test {
                    writeln!(steps, "      if ({test}) {{\n        frame.pc = {target};\n        continue dispatch;\n      }}").unwrap();
                } else {
                    writeln!(
                        steps,
                        "      frame.pc = {target};\n      continue dispatch;"
                    )
                    .unwrap();
                }
            }
            Self::Branch { .. } => {}
        }
    }
}

// Register, branch and value operations lower directly to JavaScript. Calls
// and control effects continue to use the shared runtime.
fn inline_operation(
    instruction: &Instruction,
    objects: &mut BTreeSet<&'static str>,
) -> Option<InlineOperation> {
    use Instruction::*;
    let mut object = |object| {
        objects.insert(object);
        object
    };
    let branch = |target, test| {
        Some(InlineOperation::Branch {
            target,
            test,
            local: None,
        })
    };
    let statement = match instruction {
        Load(out, Immediate::Nil) => format!("r[{out}] = {};", object("NIL")),
        Load(out, Immediate::True) => format!("r[{out}] = {};", object("T")),
        Load(out, Immediate::Fixnum(value)) => format!("r[{out}] = {value};"),
        Load(out, Immediate::Character(value)) => format!(
            "r[{out}] = new {}({});",
            object("LispCharacter"),
            *value as u32
        ),
        Constant(out, index) => format!("r[{out}] = frame.constants[{index}];"),
        Copy(out, src) => format!("r[{out}] = r[{src}];"),
        Clear(range) => format!("r.fill({}, {}, {});", object("NIL"), range.start, range.end),
        Jump(target) => return branch(*target, None),
        Unless(reg, target) => {
            return branch(*target, Some(format!("r[{reg}] === {}", object("NIL"))));
        }
        IfPresent(reg, target) => {
            return branch(*target, Some(format!("r[{reg}] !== {}", object("UNBOUND"))));
        }
        UnlessFixnum(reg, pc) => {
            let (low, high) = CompilationTarget::javascript().fixnum_bounds();
            return Some(fixnum_branch(*reg, low, high, *pc));
        }
        UnlessFixnumInRange(reg, low, high, pc) => {
            return Some(fixnum_branch(*reg, i64::from(*low), i64::from(*high), *pc));
        }
        SetValues(range) if range.len() < MULTIPLE_VALUES_LIMIT => {
            format!("task.values = r.slice({}, {});", range.start, range.end)
        }
        BindValues(range) => format!(
            "for (let i = {}; i < {}; i++) {{\n        r[i] = task.values[i - {}] ?? {};\n      }}\n      task.values = [];",
            range.start,
            range.end,
            range.start,
            object("NIL")
        ),
        ReadValue(out, index) => format!("r[{out}] = task.values[{index}] ?? {};", object("NIL")),
        Capture(out, index) => format!("r[{out}] = frame.captures[{index}];"),
        OptionalArgument(index, out) => format!(
            "r[{out}] = frame.arguments[{index}] ?? {};",
            object("UNBOUND")
        ),
        ReleaseArguments => "frame.arguments = [];\n      frame.keywords = [];".into(),
        _ => return None,
    };
    Some(InlineOperation::Statement(statement))
}

fn fixnum_branch(reg: usize, low: i64, high: i64, target: usize) -> InlineOperation {
    InlineOperation::Branch {
        target,
        test: Some(format!(
            "typeof n !== 'number' || !Number.isInteger(n) || n < {low} || n > {high}"
        )),
        local: Some(format!("const n = r[{reg}];")),
    }
}

fn operation(instruction: &Instruction, mut value: Value) -> Result<(String, Value), EmitError> {
    if let Instruction::Load(_, Immediate::Fixnum(integer)) = instruction {
        value["Load"][1]["Fixnum"] = Value::String(integer.to_string());
    }
    exact_integers(&value)?;
    match value {
        Value::String(name) => Ok((name, Value::Null)),
        Value::Object(map) => {
            let (name, mut payload) = map.into_iter().next().expect("instruction variant");
            if instruction.source_offset().is_some() {
                // Source offsets are already recorded at the instruction PC.
                payload
                    .as_object_mut()
                    .expect("source-bearing operands")
                    .remove("at");
            }
            Ok((name, payload))
        }
        _ => unreachable!("instruction serialization"),
    }
}

fn literal(node: &LiteralNode) -> Result<Value, EmitError> {
    let mut value = serde_json::to_value(node)?;
    match node {
        LiteralNode::Fixnum(_) => decimal(&mut value["Fixnum"]),
        LiteralNode::DoubleFloat(_) => decimal(&mut value["DoubleFloat"]),
        LiteralNode::RandomState(_) => decimals(&mut value["RandomState"]),
        LiteralNode::HashTable { .. } => {
            decimal(&mut value["HashTable"]["rehash_size"]);
            decimal(&mut value["HashTable"]["rehash_threshold"]);
        }
        LiteralNode::Array {
            contents: zkcl_bytecode::ArrayContents::DoubleFloats(_),
            ..
        } => {
            decimals(&mut value["Array"]["contents"]["DoubleFloats"]);
        }
        LiteralNode::SimdMask(_) => decimal(&mut value["SimdMask"]["bits"]),
        _ => {}
    }
    Ok(value)
}

fn decimal(value: &mut Value) {
    *value = Value::String(value.to_string());
}

fn decimals(value: &mut Value) {
    for item in value.as_array_mut().expect("64-bit payload array") {
        decimal(item);
    }
}

fn json(value: &Value) -> Result<String, EmitError> {
    exact_integers(value)?;
    // Valid in script source as well as modules, including older JS parsers.
    Ok(value
        .to_string()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

fn exact_integers(value: &Value) -> Result<(), EmitError> {
    const MAX_SAFE: u64 = (1 << 53) - 1;
    match value {
        Value::Number(number)
            if number.as_u64().is_some_and(|n| n > MAX_SAFE)
                || number.as_i64().is_some_and(|n| n < -(MAX_SAFE as i64)) =>
        {
            Err(EmitError::IntegerRange {
                value: number.to_string(),
            })
        }
        Value::Array(items) => items.iter().try_for_each(exact_integers),
        Value::Object(items) => items.values().try_for_each(exact_integers),
        _ => Ok(()),
    }
}
