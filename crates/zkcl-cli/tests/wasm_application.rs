use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, id};

use wasmparser::{
    ExternalKind, FunctionBody, KnownCustom, Name, Operator, Parser, Payload, TypeRef,
};

const OUTPUT: &str = "waiting 雪🙂\ncleanup\n";

fn zkcl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_zkcl"))
}

fn exported_function(bytes: &[u8], name: &str) -> u32 {
    Parser::new(0)
        .parse_all(bytes)
        .find_map(|payload| {
            let Payload::ExportSection(exports) = payload.unwrap() else {
                return None;
            };
            exports.into_iter().map(Result::unwrap).find_map(|export| {
                (export.name == name && export.kind == ExternalKind::Func).then_some(export.index)
            })
        })
        .unwrap()
}

fn compiled_body<'a>(bytes: &'a [u8], name: &str, indices: Range<u32>) -> FunctionBody<'a> {
    let function = Parser::new(0)
        .parse_all(bytes)
        .find_map(|payload| {
            let Payload::CustomSection(section) = payload.unwrap() else {
                return None;
            };
            let KnownCustom::Name(subsections) = section.as_known() else {
                return None;
            };
            subsections
                .into_iter()
                .map(Result::unwrap)
                .find_map(|subsection| {
                    let Name::Function(names) = subsection else {
                        return None;
                    };
                    names.into_iter().map(Result::unwrap).find_map(|entry| {
                        (indices.contains(&entry.index) && entry.name == name)
                            .then_some(entry.index)
                    })
                })
        })
        .expect("named compiled Wasm function");
    body_at_index(bytes, function)
}

fn body_at_index(bytes: &[u8], function: u32) -> FunctionBody<'_> {
    let mut index = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.unwrap() {
            Payload::ImportSection(imports) => {
                for import in imports {
                    let import = import.unwrap();
                    assert_eq!(import.module, "zkcl.host");
                    assert!(matches!(import.ty, TypeRef::Func(_)));
                    index += 1;
                }
            }
            Payload::CodeSectionEntry(body) => {
                if index == function {
                    return body;
                }
                index += 1;
            }
            _ => {}
        }
    }
    panic!("missing Wasm body for function {function}");
}

fn call_instruction(body: FunctionBody<'_>, callee: u32) -> Range<usize> {
    let mut ops = body.get_operators_reader().unwrap();
    while !ops.eof() {
        let start = ops.original_position();
        if matches!(ops.read().unwrap(), Operator::Call { function_index } if function_index == callee)
        {
            return start..ops.original_position();
        }
    }
    panic!("missing call to Wasm function {callee}");
}

#[test]
#[ignore = "requires ZKCL_WASM_RUNTIME and Node with JavaScript Promise Integration"]
fn linked_application_runs_in_both_engines() {
    let sources = Path::new(env!("CARGO_MANIFEST_DIR"));
    let input = sources.join("../../tests/lisp/wasm/application.lisp");
    let runtime =
        PathBuf::from(std::env::var_os("ZKCL_WASM_RUNTIME").expect("prebuilt Wasm runtime"));
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let wasm = directory.join(format!("wasm-application-{}.wasm", id()));
    let build = zkcl()
        .arg("build")
        .arg(&input)
        .args(["--target", "wasm32-unknown-unknown", "--wasm-runtime"])
        .arg(&runtime)
        .arg("--list-entries")
        .arg("-o")
        .arg(&wasm)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let listing = String::from_utf8_lossy(&build.stderr);
    assert!(
        listing.contains("leaf selection skipped (leaf functions require fixed arity"),
        "{listing}"
    );
    let leaf_count = listing
        .lines()
        .filter(|line| line.ends_with("  wasm leaf") || line.ends_with("  wasm leaf + fallback"))
        .count() as u32;
    let function_count = listing
        .lines()
        .filter(|line| line.ends_with("  wasm function"))
        .count();
    assert!(leaf_count >= 2 && function_count >= 4, "{listing}");
    for index in 0..3 {
        let expected = format!("  {index}  wasm function");
        assert!(listing.lines().any(|line| line == expected), "{listing}");
    }

    let compiler = zkcl::Runtime::builder()
        .compilation_target(zkcl_wasm::codegen::target().unwrap())
        .start()
        .unwrap();
    let compiled = compiler.compile_file_to_bytecode(&input).unwrap();
    for name in [
        "VOP-CHAIN",
        "VOP-JOIN",
        "VOP-FOLD",
        "VOP-BITS",
        "VOP-SIGN-BIT",
        "VOP-LOOP",
    ] {
        let index = compiled
            .code_entries()
            .position(|entry| {
                entry
                    .name
                    .as_deref()
                    .is_some_and(|entry_name| entry_name.ends_with(name))
            })
            .unwrap_or_else(|| panic!("missing compiled function {name}"));
        let entry = listing
            .lines()
            .find(|line| line.starts_with(&format!("  {index}  ")))
            .unwrap_or_else(|| panic!("missing Wasm entry for {name}\n{listing}"));
        assert!(
            entry.ends_with("  wasm leaf") || entry.ends_with("  wasm leaf + fallback"),
            "{name}\n{listing}"
        );
        if name == "VOP-LOOP" {
            assert!(entry.ends_with("  wasm leaf + fallback"), "{listing}");
        }
    }
    let (optional, variadic): (Vec<_>, Vec<_>) = compiled
        .code_entries()
        .enumerate()
        .filter(|(_, entry)| entry.arity.maximum != Some(entry.arity.required))
        .partition(|(_, entry)| entry.arity.maximum.is_some());
    assert_eq!(optional.len(), 2);
    assert_eq!(variadic.len(), 2);
    for (index, _) in optional.into_iter().chain(variadic) {
        let expected = format!("  {index}  wasm function");
        assert!(listing.lines().any(|line| line == expected), "{listing}");
    }
    let caller = compiled
        .code_entries()
        .position(|entry| entry.name.as_deref() == Some("USE-VOP-LOOP"))
        .unwrap();
    assert!(
        listing
            .lines()
            .any(|line| line == format!("  {caller}  wasm function")),
        "{listing}"
    );

    let fasl = wasm.with_extension("zkclfasl");
    std::fs::write(&fasl, compiled.encode().unwrap()).unwrap();
    let bridge = zkcl_wasm::abi::CALL_LEAF.as_bytes();
    let mut stale_template = std::fs::read(&runtime).unwrap();
    let exports = Parser::new(0)
        .parse_all(&stale_template)
        .find_map(|payload| match payload.unwrap() {
            Payload::ExportSection(exports) => Some(exports.range()),
            _ => None,
        })
        .unwrap();
    let export = stale_template[exports.clone()]
        .windows(bridge.len())
        .position(|bytes| bytes == bridge)
        .unwrap();
    stale_template[exports.start + export] = b'_';
    let stale_runtime = wasm.with_extension("stale-runtime.wasm");
    std::fs::write(&stale_runtime, stale_template).unwrap();
    let stale_build = zkcl()
        .arg("build")
        .arg(&fasl)
        .args(["--target", "wasm32-unknown-unknown", "--wasm-runtime"])
        .arg(&stale_runtime)
        .arg("-o")
        .arg(wasm.with_extension("stale-linked.wasm"))
        .output()
        .unwrap();
    std::fs::remove_file(stale_runtime).unwrap();
    let diagnostic = String::from_utf8_lossy(&stale_build.stderr);
    assert!(
        !stale_build.status.success()
            && diagnostic.contains(zkcl_wasm::abi::CALL_LEAF)
            && diagnostic.contains("Rebuild the runtime template"),
        "{diagnostic}"
    );
    let linked_fasl = wasm.with_file_name(format!("wasm-fasl-{}.wasm", id()));
    let build = zkcl()
        .arg("build")
        .arg(&fasl)
        .args(["--target", "wasm32-unknown-unknown", "--wasm-runtime"])
        .arg(&runtime)
        .arg("-o")
        .arg(&linked_fasl)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mut bytes = std::fs::read(&wasm).unwrap();
    assert!(bytes == std::fs::read(&linked_fasl).unwrap());
    std::fs::remove_file(linked_fasl).unwrap();
    let released = wasm.with_file_name(format!("wasm-release-{}.wasm", id()));
    let build = zkcl()
        .arg("build")
        .arg(&fasl)
        .args([
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "--wasm-runtime",
        ])
        .arg(&runtime)
        .arg("-o")
        .arg(&released)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let released_bytes = std::fs::read(&released).unwrap();
    for payload in Parser::new(0).parse_all(&released_bytes) {
        if let Payload::CustomSection(section) = payload.unwrap() {
            assert!(!matches!(section.name(), "name" | "zkcl.source"));
        }
    }
    let path = input.canonicalize().unwrap();
    let path = path.to_str().unwrap();
    assert!(
        !released_bytes
            .windows(path.len())
            .any(|window| window == path.as_bytes())
    );
    assert!(released_bytes.len() < bytes.len());
    std::fs::remove_file(fasl).unwrap();
    std::fs::remove_file(released).unwrap();

    let run = zkcl()
        .arg("run")
        .arg(&wasm)
        .args(["--", "雪🙂"])
        .output()
        .unwrap();
    assert_eq!(
        run.status.code(),
        Some(17),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(run.stdout, OUTPUT.as_bytes());
    assert!(run.stderr.is_empty());

    let inactive_cancel = wasm.with_extension("inactive-cancel.wasm");
    let cancel = body_at_index(&bytes, exported_function(&bytes, "cancel"));
    let cancel_ops =
        cancel.get_operators_reader().unwrap().original_position()..cancel.range().end - 1;
    let mut inactive = bytes.clone();
    inactive[cancel_ops.clone()].fill(0x01);
    // i32.const 0; preserve the final end and the function section layout.
    inactive[cancel_ops.start..][..2].copy_from_slice(&[0x41, 0x00]);
    std::fs::write(&inactive_cancel, inactive).unwrap();
    let browser_host = Command::new("node")
        .arg(sources.join("tests/wasm_application.mjs"))
        .arg(&wasm)
        .arg(&inactive_cancel)
        .output()
        .unwrap();
    std::fs::remove_file(inactive_cancel).unwrap();
    assert!(
        browser_host.status.success(),
        "{}",
        String::from_utf8_lossy(&browser_host.stderr)
    );

    let first_leaf = exported_function(&bytes, "run") + 1;
    let first_ssa_function = first_leaf + leaf_count;
    let fallback = compiled_body(&bytes, "VOP-LOOP", first_ssa_function..u32::MAX);
    let operators = fallback.get_operators_reader().unwrap();
    assert!(
        operators.into_iter().any(|op| matches!(
            op.unwrap(),
            Operator::Loop { .. } | Operator::BrTable { .. }
        )),
        "loop fallback should retain its SSA loop form"
    );

    let leaf = compiled_body(&bytes, "TASK-SCALAR", first_leaf..first_ssa_function);
    let leaf_range = leaf.get_operators_reader().unwrap().original_position()..leaf.range().end - 1;
    let mut trapped_leaf = bytes.clone();
    trapped_leaf[leaf_range.clone()].fill(0x01);
    trapped_leaf[leaf_range.start] = 0x00;
    std::fs::write(&wasm, trapped_leaf).unwrap();
    let run = zkcl()
        .arg("run")
        .arg(&wasm)
        .args(["--", "雪🙂"])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert_eq!(run.stdout, OUTPUT.as_bytes());
    let trap = String::from_utf8_lossy(&run.stderr);
    assert!(
        trap.contains("unreachable instruction") && trap.contains("TASK-SCALAR"),
        "{trap}"
    );

    let template = std::fs::read(runtime).unwrap();
    let deopt = exported_function(&template, "zkcl_rt_deopt_to_vm");
    let body = compiled_body(&bytes, "TASK-SUSPEND", first_ssa_function..u32::MAX);
    let handoff = call_instruction(body, deopt);
    let mut trapped_handoff = bytes.clone();
    trapped_handoff[handoff.clone()].fill(0x01);
    trapped_handoff[handoff.start] = 0x00;
    std::fs::write(&wasm, trapped_handoff).unwrap();
    let run = zkcl()
        .arg("run")
        .arg(&wasm)
        .args(["--", "雪🙂"])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert_eq!(run.stdout, OUTPUT.as_bytes());
    let trap = String::from_utf8_lossy(&run.stderr);
    assert!(
        trap.contains("unreachable instruction") && trap.contains("TASK-SUSPEND"),
        "the task must execute the SSA-to-VM handoff, not start in bytecode: {trap}"
    );

    let cell_reader = exported_function(&template, "zkcl_rt_read_cell");
    let body = compiled_body(&bytes, "READ-INPUT", first_ssa_function..u32::MAX);
    let call = call_instruction(body, cell_reader);
    bytes[call.clone()].fill(0x01);
    bytes[call.start] = 0x00;
    std::fs::write(&wasm, bytes).unwrap();
    let run = zkcl()
        .arg("run")
        .arg(&wasm)
        .args(["--", "雪🙂"])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    let trap = String::from_utf8_lossy(&run.stderr);
    let source = std::fs::read_to_string(&input).unwrap();
    let definition = "(defun read-input ()\n  ";
    let offset = source.find(definition).unwrap() + definition.len();
    assert!(trap.contains("unreachable instruction"), "{trap}");
    assert!(trap.contains("READ-INPUT"), "{trap}");
    assert!(
        trap.contains(&format!("{path:?} at byte {offset}, bytecode PC ")),
        "{trap}"
    );
    std::fs::remove_file(wasm).unwrap();
}
