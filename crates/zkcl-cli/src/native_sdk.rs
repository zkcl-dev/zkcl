//! Link installed native toolchains using the platform C toolchain only.
use super::*;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fmt::Write as _;

pub(super) struct Sdk {
    root: PathBuf,
    target: Triple,
    link_args: Vec<String>,
}

/// Select an explicit template first, then the matching installed Wasm SDK.
pub(super) fn wasm_runtime_path(explicit: Option<&Path>) -> Result<PathBuf, BuildError> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    if let Some(path) = env::var_os("ZKCL_WASM_RUNTIME") {
        return Ok(PathBuf::from(path));
    }
    const TARGET: &str = "wasm32-unknown-unknown";
    let executable =
        env::current_exe().map_err(|error| BuildError::NativeSdk(error.to_string()))?;
    let root = env::var_os("ZKCL_SDK_ROOT")
        .map(PathBuf::from)
        .or_else(|| env::var_os("ZKCL_TOOLCHAIN_ROOT").map(|root| PathBuf::from(root).join("sdk")))
        .or_else(|| {
            executable
                .parent()
                .and_then(Path::parent)
                .map(|root| root.join("sdk"))
        })
        .ok_or_else(|| {
            BuildError::NativeSdk("cannot determine installed Wasm SDK location".into())
        })?;
    let root = if root.join("runtime.wasm").is_file() {
        root
    } else {
        root.join(TARGET)
    };
    let metadata: Value = serde_json::from_slice(&fs::read(root.join("runtime.json"))
        .map_err(|error| BuildError::NativeSdk(format!("Wasm SDK is not installed at {}: {error}; install target {TARGET} or provide --wasm-runtime", root.display())))?)
        .map_err(|error| BuildError::NativeSdk(format!("invalid Wasm SDK manifest: {error}")))?;
    if metadata["version"].as_str() != Some(env!("CARGO_PKG_VERSION"))
        || metadata["target"].as_str() != Some(TARGET)
        || metadata["kind"].as_str() != Some("wasm")
    {
        return Err(BuildError::NativeSdk(format!(
            "Wasm SDK version/target mismatch at {}; expected zkcl {} for {TARGET}",
            root.display(),
            env!("CARGO_PKG_VERSION")
        )));
    }
    let path = root.join("runtime.wasm");
    if !path.is_file() {
        return Err(BuildError::NativeSdk(format!(
            "incomplete Wasm SDK: missing {}",
            path.display()
        )));
    }
    Ok(path)
}

impl Sdk {
    pub(super) fn locate(target: &Triple) -> Result<Option<Self>, BuildError> {
        let explicit = env::var_os("ZKCL_SDK_ROOT").map(PathBuf::from);
        let managed =
            env::var_os("ZKCL_TOOLCHAIN_ROOT").map(|root| PathBuf::from(root).join("sdk"));
        let executable =
            env::current_exe().map_err(|error| BuildError::NativeSdk(error.to_string()))?;
        let sibling = executable
            .parent()
            .and_then(Path::parent)
            .map(|root| root.join("sdk"));
        let root = explicit
            .clone()
            .or(managed.clone())
            .or_else(|| sibling.filter(|path| path.is_dir()));
        let Some(root) = root else {
            // Cargo is a development path only. An installed binary must never
            // download a crate or require Rust to build a Lisp application.
            let local = RuntimeDependency::locate(
                Path::new(env!("CARGO_MANIFEST_DIR")),
                env::var_os("ZKCL_CRATE_PATH").map(PathBuf::from),
            );
            if matches!(local, RuntimeDependency::Local { .. }) {
                return Ok(None);
            }
            return Err(BuildError::NativeSdk(format!(
                "native SDK for {target} is not installed; install the matching SDK or set ZKCL_SDK_ROOT"
            )));
        };
        let root = if root.join("runtime.json").is_file() {
            root
        } else {
            root.join(target.to_string())
        };
        let manifest_path = root.join("runtime.json");
        let bytes = fs::read(&manifest_path).map_err(|error| {
            BuildError::NativeSdk(format!(
                "native SDK for {target} is unavailable at {}: {error}",
                root.display()
            ))
        })?;
        let metadata: Value = serde_json::from_slice(&bytes)
            .map_err(|error| BuildError::NativeSdk(format!("invalid SDK manifest: {error}")))?;
        if metadata["version"].as_str() != Some(env!("CARGO_PKG_VERSION"))
            || metadata["target"].as_str() != Some(target.to_string().as_str())
        {
            return Err(BuildError::NativeSdk(format!(
                "SDK version/target mismatch at {}; expected zkcl {} for {target}",
                manifest_path.display(),
                env!("CARGO_PKG_VERSION")
            )));
        }
        let link_args = metadata["link_args"]
            .as_array()
            .ok_or_else(|| BuildError::NativeSdk("SDK manifest requires link_args".into()))?
            .iter()
            .map(|arg| {
                arg.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| BuildError::NativeSdk("SDK link_args must be strings".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for path in [root.join("include/zkcl.h"), root.join("lib/libzkcl_capi.a")] {
            if !path.is_file() {
                return Err(BuildError::NativeSdk(format!(
                    "incomplete SDK: missing {}",
                    path.display()
                )));
            }
        }
        Ok(Some(Self {
            root,
            target: target.clone(),
            link_args,
        }))
    }

    pub(super) fn build(
        &self,
        dir: &Path,
        stem: &str,
        opts: &BuildArgs,
        source: &str,
        objects: &[PathBuf],
        libraries: &[String],
        search: &[PathBuf],
    ) -> Result<PathBuf, BuildError> {
        let source_path = dir.join("main.c");
        write_generated_file(&source_path, source)?;
        let executable = opts
            .output_path
            .clone()
            .unwrap_or_else(|| PathBuf::from(opts.output_kind.output_filename(stem, &self.target)));
        if opts.emit_dir.is_some() && opts.output_path.is_none() {
            let commands = json!({
                "compiler": "cc",
                "include": self.root.join("include"),
                "main": "main.c",
                "output": executable,
                "objects": objects,
                "runtime": self.root.join("lib/libzkcl_capi.a"),
                "library_search": search,
                "libraries": libraries,
                "link_args": self.link_args,
            });
            write_generated_file(
                &dir.join("link.json"),
                serde_json::to_vec_pretty(&commands)
                    .map_err(|error| BuildError::NativeSdk(error.to_string()))?,
            )?;
            return Ok(dir.to_owned());
        }
        let parent = executable
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|error| BuildError::path_io("create", parent, error))?;
        let staged = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| BuildError::path_io("stage native output", parent, error))?;
        // CC_<target> is the cross-compiler escape hatch; clang can otherwise
        // select a target directly. The selected SDK supplies target link flags.
        let key = format!("CC_{}", self.target.to_string().replace('-', "_"));
        let cc = env::var_os(&key)
            .or_else(|| env::var_os("CC"))
            .unwrap_or_else(|| {
                OsString::from(if self.target == Triple::host() {
                    "cc"
                } else {
                    "clang"
                })
            });
        let mut compile = Command::new(&cc);
        if self.target != Triple::host() && env::var_os(&key).is_none() {
            compile.arg(format!("--target={}", self.target));
        }
        compile
            .arg("-std=c11")
            .arg(if opts.release { "-O2" } else { "-O0" })
            .arg("-I")
            .arg(self.root.join("include"));
        if opts.output_kind == OutputKind::StaticLibrary {
            let wrapper = dir.join("main.o");
            compile
                .arg("-DZKCL_STATICLIB")
                .arg("-c")
                .arg(&source_path)
                .arg("-o")
                .arg(&wrapper);
            run(&mut compile)?;
            fs::copy(self.root.join("lib/libzkcl_capi.a"), staged.path()).map_err(|error| {
                BuildError::path_io("copy SDK runtime archive", staged.path(), error)
            })?;
            let ar_key = format!("AR_{}", self.target.to_string().replace('-', "_"));
            let ar = env::var_os(ar_key)
                .or_else(|| env::var_os("AR"))
                .unwrap_or_else(|| OsString::from("ar"));
            run(Command::new(ar)
                .arg("rcs")
                .arg(staged.path())
                .arg(wrapper)
                .args(objects))?;
        } else {
            compile
                .arg(&source_path)
                .args(objects)
                .arg(self.root.join("lib/libzkcl_capi.a"));
            for path in search {
                compile.arg("-L").arg(path);
            }
            for library in libraries {
                compile.arg(format!("-l{library}"));
            }
            compile.args(&self.link_args).arg("-o").arg(staged.path());
            run(&mut compile)?;
        }
        staged.persist(&executable).map_err(|error| {
            BuildError::path_io("publish native output", &executable, error.error)
        })?;
        Ok(executable)
    }
}

fn run(command: &mut Command) -> Result<(), BuildError> {
    let output = command.output().map_err(|error| {
        BuildError::NativeSdk(format!(
            "cannot invoke native toolchain {command:?}: {error}"
        ))
    })?;
    if !output.status.success() {
        return Err(BuildError::NativeSdk(format!(
            "native linker failed ({command:?}):\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn literal(value: &str) -> String {
    // Encode bytes, avoiding C execution charset and hexadecimal escape capture.
    let mut result = String::from("\"");
    for byte in value.bytes() {
        write!(result, "\\{byte:03o}").unwrap();
    }
    result.push('"');
    result
}

fn module_source(
    index: usize,
    id: &str,
    entries: &[zkcl_aot::AotRegistrationEntry],
    fasl_len: usize,
    target: &Triple,
) -> (String, String) {
    let mut source = String::new();
    let fasl = zkcl_aot::fasl_symbol_name(id);
    writeln!(source, "extern const unsigned char {fasl}[{fasl_len}];").unwrap();
    for entry in entries {
        writeln!(
            source,
            "extern void {}(void *, uintptr_t, const uintptr_t *, size_t, void *);",
            entry.symbol_name
        )
        .unwrap();
        if let Some(leaf) = &entry.leaf_symbol {
            writeln!(
                source,
                "extern uintptr_t {leaf}(uintptr_t, uintptr_t, uintptr_t, uintptr_t);"
            )
            .unwrap();
        }
    }
    // C11 does not have empty arrays; the sentinel is excluded from code_count.
    writeln!(source, "static const zkcl_native_code codes_{index}[] = {{").unwrap();
    for entry in entries {
        let guards = serde_json::to_string(&entry.leaf_guards).expect("native leaf guards");
        writeln!(
            source,
            "  {{{}, {}, {}, {}, {}, {}, {}, {}, {{{}, {}}}}},",
            entry.entry_index,
            entry.arity.required,
            entry.arity.maximum.unwrap_or(0),
            entry.root_slots,
            entry.arity.maximum.is_none(),
            entry.native_debug,
            entry.symbol_name,
            entry.leaf_symbol.as_deref().unwrap_or("NULL"),
            literal(&guards),
            guards.len()
        )
        .unwrap();
    }
    source.push_str("  {0,0,0,0,false,false,NULL,NULL,{NULL,0}}\n};\n");
    let fingerprint = zkcl::native::AbiFingerprint::for_target(
        zkcl::CompilationTarget::from_triple(target.clone())
            .expect("validated AOT target")
            .machine()
            .expect("AOT target has a machine ABI"),
    );
    let descriptor = format!(
        "  {{{{{}, {}}}, {fasl}, {fasl_len}, codes_{index}, {}, {}, {}, UINT64_C({}), {}}},\n",
        literal(id),
        id.len(),
        entries.len(),
        fingerprint.pointer_width,
        fingerprint.word_layout_version,
        fingerprint.bytecode_contract_version,
        zkcl_bytecode::native_abi::NATIVE_CALL_ABI_REVISION
    );
    (source, descriptor)
}

fn finish_source(mut source: String, descriptors: String, count: usize, plan: &Value) -> String {
    source.push_str("static const zkcl_native_module modules[] = {\n");
    source.push_str(&descriptors);
    source.push_str("  {0}\n};\n");
    let plan = serde_json::to_string(plan).expect("native startup plan");
    writeln!(source, "static const char plan[] = {};", literal(&plan)).unwrap();
    writeln!(source, "int zkcl_aot_run(void) {{ return zkcl_native_run(modules, {count}, (zkcl_str){{plan, sizeof(plan)-1}}, 0, NULL); }}").unwrap();
    writeln!(source, "#ifndef ZKCL_STATICLIB\nint main(int argc, char **argv) {{ return zkcl_native_run(modules, {count}, (zkcl_str){{plan, sizeof(plan)-1}}, argc, (const char *const *)argv); }}\n#endif").unwrap();
    source
}

pub(super) fn single_source(id: &str, object: &AotObject, target: &Triple) -> String {
    let (source, descriptor) = module_source(
        0,
        id,
        &object.registration_entries,
        object.fasl_bytes.len(),
        target,
    );
    finish_source(
        format!("#include \"zkcl.h\"\n{source}"),
        descriptor,
        1,
        &json!({"steps":[{"kind":"fasl","index":0}]}),
    )
}

pub(super) fn project_plan(delivery: &Delivery, entry: &str, assets: &str) -> Value {
    let mut steps = Vec::new();
    for step in &delivery.steps {
        match step {
            StartupStep::LoadFasl(index) => steps.push(json!({"kind":"fasl","index":index})),
            StartupStep::LoadAsdf(name) => steps.push(json!({"kind":"asdf","name":name})),
            StartupStep::ReadyAsdfAlias(names) => {
                for name in names {
                    steps.push(json!({"kind":"ready","name":name}));
                }
            }
        }
    }
    json!({"assets": assets, "asdf":delivery.asdf.is_some(),"entry":entry,"steps":steps})
}

pub(super) fn project_source(units: &[AotUnit], plan: &Value, target: &Triple) -> String {
    let mut source = String::from("#include \"zkcl.h\"\n");
    let mut descriptors = String::new();
    for (index, unit) in units.iter().enumerate() {
        let (code, descriptor) = module_source(
            index,
            &unit.module_id,
            &unit.entries,
            unit.fasl_length,
            target,
        );
        source.push_str(&code);
        descriptors.push_str(&descriptor);
    }
    finish_source(source, descriptors, units.len(), plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_literals_preserve_utf8_and_escape_boundaries() {
        assert_eq!(literal("é9\n"), "\"\\303\\251\\071\\012\"");
    }

    #[test]
    fn generated_native_registration_compiles_as_c11() {
        let parent =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/native-sdk");
        fs::create_dir_all(&parent).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("c-registration-")
            .tempdir_in(parent)
            .unwrap();
        let source = dir.path().join("main.c");
        let (code, descriptor) = module_source(
            0,
            "test-module",
            &[zkcl_aot::AotRegistrationEntry {
                native_debug: false,
                entry_index: 3,
                symbol_name: "zkcl_aot_test_fn_3".into(),
                arity: zkcl::native::Arity {
                    required: 1,
                    maximum: Some(2),
                },
                root_slots: 4,
                leaf_symbol: None,
                leaf_guards: Vec::new(),
            }],
            16,
            &Triple::host(),
        );
        fs::write(
            &source,
            finish_source(
                format!("#include \"zkcl.h\"\n{code}"),
                descriptor,
                1,
                &json!({"steps":[{"kind":"fasl","index":0}]}),
            ),
        )
        .unwrap();
        let include = Path::new(env!("CARGO_MANIFEST_DIR")).join("../zkcl-capi/include");
        run(Command::new("cc")
            .arg("-std=c11")
            .arg("-Werror")
            .arg("-I")
            .arg(include)
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(dir.path().join("main.o")))
        .unwrap();
    }
}
