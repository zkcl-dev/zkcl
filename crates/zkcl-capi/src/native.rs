//! Process-lifetime registration and startup ABI used by the installed SDK.
use crate::abi::{Error, Utf8View, boundary, input_slice};
use std::ffi::{CStr, c_char};
use zkcl::native::{
    AbiFingerprint, AotCodeDescriptor, AotModuleDescriptor, Arity, JitCEntryFn, jit_c_trampoline,
};

#[repr(C)]
struct NativeCode {
    index: u32,
    required: usize,
    maximum: usize,
    root_slots: usize,
    variadic: bool,
    native_debug: bool,
    entry: JitCEntryFn,
    leaf: Option<zkcl::native::RegisterLeafFn>,
    leaf_guards: Utf8View,
}
#[repr(C)]
struct NativeModule {
    id: Utf8View,
    fasl: *const u8,
    fasl_len: usize,
    codes: *const NativeCode,
    code_count: usize,
    pointer_width: u8,
    word_layout_version: u32,
    bytecode_contract_version: u64,
    continuation_abi: u32,
}
fn text(value: &serde_json::Value, key: &str) -> Result<String, Error> {
    value[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid("invalid native startup plan"))
}
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[unsafe(no_mangle)]
unsafe extern "C" fn zkcl_native_run(
    modules: *const NativeModule,
    module_count: usize,
    plan: Utf8View,
    argc: i32,
    argv: *const *const c_char,
) -> i32 {
    let mut exit = 1;
    let error = boundary(|| {
        let plan: serde_json::Value = serde_json::from_str(unsafe { plan.read_utf8()? })
            .map_err(|error| Error::Invalid(error.to_string()))?;
        // C promises immutable process-lifetime module data. Allocate descriptors
        // once for this process, as generated Rust static registration does.
        let mut registered = Vec::new();
        for module in unsafe { input_slice(modules, module_count)? } {
            let id: &'static str = unsafe { module.id.read_utf8()? };
            let fasl: &'static [u8] = unsafe { input_slice(module.fasl, module.fasl_len)? };
            let codes = unsafe { input_slice(module.codes, module.code_count)? }
                .iter()
                .map(|code| {
                    let guards: Vec<zkcl::native::LeafGuard> =
                        serde_json::from_str(unsafe { code.leaf_guards.read_utf8()? })
                            .map_err(|error| Error::Invalid(error.to_string()))?;
                    Ok(AotCodeDescriptor {
                        native_debug: code.native_debug,
                        entry_index: code.index,
                        arity: Arity {
                            required: code.required,
                            maximum: (!code.variadic).then_some(code.maximum),
                        },
                        root_slots: code.root_slots,
                        fast_entry: None,
                        generic_entry: jit_c_trampoline,
                        c_entry: Some(code.entry),
                        continuation_abi: Some(module.continuation_abi),
                        leaf_entry: code.leaf,
                        leaf_guards: Box::leak(guards.into_boxed_slice()),
                    })
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let descriptor = Box::leak(Box::new(unsafe {
                AotModuleDescriptor::new(
                    id,
                    AbiFingerprint {
                        pointer_width: module.pointer_width,
                        word_layout_version: module.word_layout_version,
                        bytecode_contract_version: module.bytecode_contract_version,
                    },
                    Some(fasl),
                    Box::leak(codes.into_boxed_slice()),
                )
            }));
            registered.push(&*descriptor);
        }
        let builder = zkcl::Runtime::builder();
        #[cfg(feature = "ffi")]
        let builder = unsafe { builder.allow_foreign_calls() };
        let runtime = builder.start().map_err(|source| Error::RuntimeStart {
            message: source.to_string(),
            source,
        })?;
        let mut arguments = Vec::new();
        for &argument in unsafe {
            input_slice(
                argv,
                usize::try_from(argc).map_err(|_| Error::invalid("invalid argc"))?,
            )?
        }
        .iter()
        .skip(1)
        {
            if argument.is_null() {
                return Err(Error::invalid("invalid argv"));
            }
            arguments.push(unsafe { CStr::from_ptr(argument) }.to_str()?.to_owned());
        }
        runtime.set_command_line_arguments(arguments);
        let executable = std::env::current_exe().map_err(|e| Error::Invalid(e.to_string()))?;
        let assets = if let Some(asset) = plan["assets"].as_str() {
            let mut name = executable
                .file_name()
                .ok_or_else(|| Error::invalid("missing executable name"))?
                .to_os_string();
            name.push(".zkcl");
            Some(
                executable
                    .with_file_name(name)
                    .join(asset)
                    .canonicalize()
                    .map_err(|e| Error::Invalid(e.to_string()))?,
            )
        } else {
            None
        };
        let _asdf_output = if plan["asdf"].as_bool() == Some(true) {
            let assets = assets
                .as_ref()
                .ok_or_else(|| Error::invalid("missing ASDF assets"))?;
            runtime.load_file(&assets.join("asdf-bootstrap.lisp"))?;
            let output = tempfile::Builder::new()
                .prefix("zkcl-asdf-")
                .tempdir()
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let initialize = runtime
                .lookup_symbol("ZKCL-PROJECT-ASDF", "INITIALIZE-DELIVERY")?
                .ok_or_else(|| Error::invalid("missing ASDF initializer"))?;
            let source = runtime.make_string(
                assets
                    .to_str()
                    .ok_or_else(|| Error::invalid("ASDF path is not UTF-8"))?,
            )?;
            let output_path = runtime.make_string(&format!(
                "{}/",
                output
                    .path()
                    .to_str()
                    .ok_or_else(|| Error::invalid("ASDF path is not UTF-8"))?
            ))?;
            runtime.call(&initialize, &[&source, &output_path])?;
            Some(output)
        } else {
            None
        };
        for step in plan["steps"]
            .as_array()
            .ok_or_else(|| Error::invalid("missing native startup steps"))?
        {
            match text(step, "kind")?.as_str() {
                "fasl" => {
                    let index = step["index"]
                        .as_u64()
                        .and_then(|i| usize::try_from(i).ok())
                        .ok_or_else(|| Error::invalid("invalid native module index"))?;
                    let module = registered
                        .get(index)
                        .ok_or_else(|| Error::invalid("invalid native module index"))?;
                    if let Some(assets) = &assets {
                        runtime.register_aot_module(module)?;
                        runtime.load_file(&assets.join(format!("{index:04}.zkclfasl")))?;
                    } else {
                        runtime.load_aot_module(module)?;
                    }
                }
                "asdf" => {
                    runtime.eval(&format!(
                        "(asdf:load-system {})",
                        quoted(&text(step, "name")?)
                    ))?;
                }
                "ready" => {
                    runtime.eval(&format!(
                        "(zkcl-project-asdf::mark-native-ready {})",
                        quoted(&text(step, "name")?)
                    ))?;
                }
                _ => return Err(Error::invalid("unknown native startup step")),
            }
        }
        if let Some(entry) = plan["entry"].as_str() {
            let (package, name) = entry
                .split_once(':')
                .ok_or_else(|| Error::invalid("entry must be package-qualified"))?;
            let name = name.strip_prefix(':').unwrap_or(name);
            let function = runtime
                .lookup_symbol(package, name)?
                .ok_or_else(|| Error::Invalid(format!("cannot find {entry}")))?;
            runtime.call(&function, &[])?;
        }
        exit = i32::from(runtime.requested_exit_code());
        Ok(())
    });
    if !error.is_null() {
        let error = unsafe { Box::from_raw(error) };
        eprintln!("zkcl: {error}");
    }
    exit
}
