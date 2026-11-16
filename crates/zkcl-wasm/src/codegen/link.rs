//! Link Lisp functions and their FASL into a standalone Core Wasm module.

use std::collections::BTreeMap;
use std::convert::Infallible;

use crate::abi::{
    CALL_FUNCTION, CALL_LEAF, CANCEL_RUN, HOST_MODULE, HostFunction, HostValueType, LOAD_FASL,
    LOAD_PROJECT, RESERVE_FASL, RegisteredEntry, Registration, RegistrationError,
    encode_registrations,
};
use crate::codegen::{LoweringError, compile_leaf, lower_ssa_function};
use crate::source_map::{FunctionSourceMap, InstructionRange, SECTION_NAME, SourceMap};
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, CustomSection, DataCountSection, DataSection,
    ElementSection, Elements, ExportKind, ExportSection, Function, FunctionSection, Instruction,
    Module, NameMap, NameSection, RefType, SectionId, TableSection, TableType, TypeSection,
    ValType,
};
use wasmparser::{
    ExternalKind, KnownCustom, Name, Operator, Parser, Payload, Validator, WasmFeatures,
};
#[cfg(test)]
use zkcl_bytecode::LiteralNode;
use zkcl_bytecode::{CompiledFile, CompiledFileError};
use zkcl_opt::native::{RuntimeAbiFunction, RuntimeAbiType};
use zkcl_target::{CompilationTarget, TargetError};
use zkcl_vop::SelectionError;

use zkcl_opt::FaslSpecializationInputs;

const RUN: &str = "run";

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error(transparent)]
    Target(#[from] TargetError),
    #[error(transparent)]
    Fasl(#[from] CompiledFileError),
    #[error("the FASL and registrations do not fit in the Wasm load buffer")]
    LoadBufferSize,
    #[error("too many Lisp functions for the Wasm module")]
    FunctionCount,
    #[error("could not encode function registrations: {0}")]
    Registration(#[from] RegistrationError),
    #[error("expected a nonempty package-qualified project entry")]
    ProjectEntry,
    #[error("could not encode the Wasm source map: {0}")]
    SourceMap(#[source] RegistrationError),
    #[error("bytecode lifting failed for entry {entry}: {source}")]
    Lift {
        entry: u32,
        source: zkcl_opt::BytecodeLiftError,
    },
    #[error("Wasm lowering failed for entry {entry}: {source}")]
    Lower { entry: u32, source: LoweringError },
    #[error("invalid Wasm runtime module: {0}")]
    Runtime(#[from] wasmparser::BinaryReaderError),
    #[error(
        "Wasm runtime module must export {0}. Rebuild the runtime template from this checkout."
    )]
    Export(&'static str),
    #[error("Wasm runtime module exports the reserved name {RUN:?}")]
    RunExport,
    #[error("Wasm runtime must export an unwrapped {0} stub. Rebuild the runtime module.")]
    Bridge(&'static str),
    #[error("Wasm runtime function {0} has the wrong signature. Rebuild the runtime module.")]
    RuntimeSignature(&'static str),
    #[error("unknown Wasm runtime import {module}.{name}")]
    Import { module: String, name: String },
    #[error(
        "Wasm runtime import {HOST_MODULE}.{0} has the wrong signature. Rebuild the runtime module."
    )]
    ImportSignature(String),
    #[error("Wasm runtime module must not have a start function")]
    Start,
    #[error("could not reencode Wasm runtime module: {0}")]
    Reencode(#[from] reencode::Error<Infallible>),
    #[error("linked Wasm module is invalid: {0}")]
    Output(#[source] wasmparser::BinaryReaderError),
}

pub struct LinkedModule {
    pub bytes: Vec<u8>,
    pub entries: Vec<Registration>,
    pub bytecode_entries: usize,
    /// Why a function used the SSA emitter or bytecode instead of leaf selection.
    pub leaf_rejections: Vec<(u32, SelectionError)>,
}

pub fn target() -> Result<CompilationTarget, TargetError> {
    CompilationTarget::from_triple("wasm32-unknown-unknown".parse()?)
}

/// Append selected Lisp entries, their function table, and a passive FASL segment.
/// `run` loads the file and returns its exit status.
pub fn link(runtime: &[u8], fasl: &[u8], strip_debug: bool) -> Result<LinkedModule, LinkError> {
    let compiled = CompiledFile::decode_for_target(fasl, &target()?)?;
    if strip_debug {
        encode_and_link(runtime, compiled, true)
    } else {
        link_module(runtime, &compiled, fasl, false)
    }
}

pub fn link_compiled_file(
    runtime: &[u8],
    compiled: CompiledFile,
    strip_debug: bool,
) -> Result<LinkedModule, LinkError> {
    compiled.check_target(&target()?)?;
    encode_and_link(runtime, compiled, strip_debug)
}

fn encode_and_link(
    runtime: &[u8],
    mut compiled: CompiledFile,
    strip_debug: bool,
) -> Result<LinkedModule, LinkError> {
    if strip_debug {
        compiled.strip_debug();
    }
    let fasl = compiled.encode()?;
    link_module(runtime, &compiled, &fasl, strip_debug)
}

/// Link ordered project units into one runtime. Each FASL retains its own
/// literal, entry, and source indices. Startup loads units in order, then calls
/// the package-qualified entry with no arguments.
pub fn link_project(
    runtime: &[u8],
    units: &[Vec<u8>],
    entry: &str,
    strip_debug: bool,
) -> Result<LinkedModule, LinkError> {
    let (package, name) = entry.split_once(':').ok_or(LinkError::ProjectEntry)?;
    if package.is_empty() || name.trim_start_matches(':').is_empty() {
        return Err(LinkError::ProjectEntry);
    }
    let target = target()?;
    let mut decoded = Vec::with_capacity(units.len());
    for unit in units {
        let mut compiled = CompiledFile::decode_for_target(unit, &target)?;
        if strip_debug {
            compiled.strip_debug();
        }
        decoded.push(compiled);
    }
    let encoded = decoded
        .iter()
        .map(CompiledFile::encode)
        .collect::<Result<Vec<_>, _>>()?;
    let units = decoded
        .iter()
        .zip(encoded.iter())
        .map(|(compiled, fasl)| (compiled, fasl.as_slice()))
        .collect::<Vec<_>>();
    link_units(runtime, &units, Some(entry), strip_debug)
}

struct RuntimeImports<'a> {
    reserve: u32,
    load: u32,
    project_load: Option<u32>,
    cancel: u32,
    leaf_body: u32,
    function_body: u32,
    runtime_functions: [Option<u32>; RuntimeAbiFunction::ALL.len()],
    has_elements: bool,
    segments: Option<u32>,
    names: BTreeMap<u32, &'a str>,
}

#[derive(Default)]
struct CompiledWasmEntries<'a> {
    leaves: Vec<LinkedFunction<'a>>,
    functions: Vec<LinkedFunction<'a>>,
    registrations: Vec<Registration>,
    bytecode_entries: usize,
    leaf_rejections: Vec<(u32, SelectionError)>,
}

fn validate_runtime_imports(types: wasmparser::types::TypesRef<'_>) -> Result<u32, LinkError> {
    let mut function_imports = 0;
    for (module, name, entity) in types.core_imports().unwrap() {
        let (Some(function), wasmparser::types::EntityType::Func(id)) =
            (HostFunction::lookup(module, name), entity)
        else {
            return Err(LinkError::Import {
                module: module.into(),
                name: name.into(),
            });
        };
        let ty = types.get(id).unwrap().unwrap_func();
        let (params, result) = function.signature();
        let val_type = |value| match value {
            HostValueType::I32 => wasmparser::ValType::I32,
            HostValueType::I64 => wasmparser::ValType::I64,
        };
        if !ty
            .params()
            .iter()
            .copied()
            .eq(params.iter().copied().map(val_type))
            || ty.results() != [val_type(result)]
        {
            return Err(LinkError::ImportSignature(name.into()));
        }
        function_imports += 1;
    }
    Ok(function_imports)
}

fn inspect_runtime_module<'a>(
    runtime: &'a [u8],
    types: wasmparser::types::TypesRef<'_>,
    function_imports: u32,
    strip_debug: bool,
) -> Result<RuntimeImports<'a>, LinkError> {
    let mut reserve = None;
    let mut load = None;
    let mut project_load = None;
    let mut cancel = None;
    let mut bridges = [(CALL_LEAF, None, None), (CALL_FUNCTION, None, None)];
    let mut runtime_functions = [None; RuntimeAbiFunction::ALL.len()];
    let mut has_memory = false;
    let mut has_elements = false;
    let mut segments = None;
    let mut body_index = 0;
    let mut names = BTreeMap::new();
    for payload in Parser::new(0).parse_all(runtime) {
        match payload? {
            Payload::ExportSection(exports) => {
                for export in exports {
                    let export = export?;
                    match (export.name, export.kind) {
                        (RUN, _) => return Err(LinkError::RunExport),
                        (RESERVE_FASL, ExternalKind::Func) => reserve = Some(export.index),
                        (LOAD_FASL, ExternalKind::Func) => load = Some(export.index),
                        (LOAD_PROJECT, ExternalKind::Func) => project_load = Some(export.index),
                        (CANCEL_RUN, ExternalKind::Func) => cancel = Some(export.index),
                        (CALL_LEAF, ExternalKind::Func) => bridges[0].1 = Some(export.index),
                        (CALL_FUNCTION, ExternalKind::Func) => bridges[1].1 = Some(export.index),
                        ("memory", ExternalKind::Memory) => has_memory = export.index == 0,
                        (name, ExternalKind::Func) => {
                            if let Some(function) = RuntimeAbiFunction::from_symbol_name(name) {
                                let ty = types
                                    .get(types.core_function_at(export.index))
                                    .unwrap()
                                    .unwrap_func();
                                let params =
                                    function.parameter_types().iter().copied().map(abi_type);
                                let result = function.return_type().map(abi_type);
                                if !ty.params().iter().copied().eq(params)
                                    || !ty.results().iter().copied().eq(result)
                                {
                                    return Err(LinkError::RuntimeSignature(
                                        function.symbol_name(),
                                    ));
                                }
                                runtime_functions[function as usize] = Some(export.index);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Payload::StartSection { .. } => return Err(LinkError::Start),
            Payload::ElementSection(_) => has_elements = true,
            Payload::CodeSectionEntry(body) => {
                if let Some((_, _, bridge_body)) = bridges
                    .iter_mut()
                    .find(|(_, index, _)| *index == Some(function_imports + body_index))
                {
                    let mut operators = body.get_operators_reader()?;
                    if body.get_locals_reader()?.get_count() == 0
                        && matches!(operators.read()?, Operator::Unreachable)
                        && matches!(operators.read()?, Operator::End)
                        && operators.eof()
                    {
                        *bridge_body = Some(body_index);
                    }
                }
                body_index += 1;
            }
            Payload::DataSection(data) => segments = Some(data.count()),
            Payload::CustomSection(section) if !strip_debug => {
                if let KnownCustom::Name(section) = section.as_known() {
                    for subsection in section.flatten() {
                        if let Name::Function(functions) = subsection {
                            names.extend(functions.into_iter().flatten().filter_map(|name| {
                                (name.index < types.function_count())
                                    .then_some((name.index, name.name))
                            }));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !has_memory {
        return Err(LinkError::Export("linear memory 0 as `memory`"));
    }
    let i32_type = wasmparser::ValType::I32;
    let verify_export = |index: Option<u32>,
                         params: &[wasmparser::ValType],
                         results: &[wasmparser::ValType],
                         name| {
        let index = index.ok_or(LinkError::Export(name))?;
        let ty = types
            .get(types.core_function_at(index))
            .unwrap()
            .unwrap_func();
        if ty.params() != params || ty.results() != results {
            return Err(LinkError::Export(name));
        }
        Ok(index)
    };
    let reserve = verify_export(reserve, &[i32_type], &[i32_type], RESERVE_FASL)?;
    let load = verify_export(load, &[i32_type], &[i32_type], LOAD_FASL)?;
    if project_load.is_some() {
        verify_export(project_load, &[i32_type], &[i32_type], LOAD_PROJECT)?;
    }
    let cancel = verify_export(cancel, &[], &[i32_type], CANCEL_RUN)?;
    let [
        (leaf_name, leaf, leaf_body),
        (function_name, call_function, function_body),
    ] = bridges;
    verify_export(leaf, &[i32_type; 5], &[i32_type], leaf_name)?;
    verify_export(call_function, &[i32_type; 7], &[], function_name)?;
    let leaf_body = leaf_body.ok_or(LinkError::Bridge(leaf_name))?;
    let function_body = function_body.ok_or(LinkError::Bridge(function_name))?;
    Ok(RuntimeImports {
        reserve,
        load,
        project_load,
        cancel,
        leaf_body,
        function_body,
        runtime_functions,
        has_elements,
        segments,
        names,
    })
}

fn compile_wasm_entries<'a>(
    compiled: &'a CompiledFile,
    runtime_functions: &[Option<u32>; RuntimeAbiFunction::ALL.len()],
    strip_debug: bool,
) -> Result<CompiledWasmEntries<'a>, LinkError> {
    let mut entries = CompiledWasmEntries::default();
    for (index, entry) in compiled.code_entries().enumerate() {
        let entry_index = u32::try_from(index).map_err(|_| LinkError::FunctionCount)?;
        entries.bytecode_entries += 1;
        let leaf = if entry.debug.retain_frame {
            None
        } else {
            match compile_leaf(entry, &compiled.nodes) {
                Ok(leaf) => {
                    entries.leaves.push(LinkedFunction {
                        body: leaf.body,
                        name: entry.name.as_deref(),
                        source_file: entry.source_file,
                        instructions: Vec::new(),
                    });
                    if !leaf.has_loop {
                        entries.registrations.push(Registration {
                            entry_index,
                            entry: RegisteredEntry::Leaf {
                                guards: leaf.guards,
                                fallback: None,
                            },
                        });
                        continue;
                    }
                    Some(leaf.guards)
                }
                Err(reason) => {
                    entries.leaf_rejections.push((entry_index, reason));
                    None
                }
            }
        };
        let specialization = FaslSpecializationInputs::new(compiled, entry);
        let ssa = specialization
            .optimize()
            .map_err(|source| LinkError::Lift {
                entry: entry_index,
                source,
            })?;
        let lowered = match lower_ssa_function(
            &ssa,
            &specialization,
            (!strip_debug).then_some(entry.instructions.as_slice()),
            |function| runtime_functions[function as usize],
        ) {
            Ok(lowered) => lowered,
            Err(LoweringError::RuntimeFunction(_)) if leaf.is_some() => None,
            Err(source) => {
                return Err(LinkError::Lower {
                    entry: entry_index,
                    source,
                });
            }
        };
        let fallback = lowered.map(|lowered| {
            entries.functions.push(LinkedFunction {
                body: lowered.body,
                name: entry.name.as_deref(),
                source_file: entry.source_file,
                instructions: lowered.instructions,
            });
            lowered.registration
        });
        let registration = if let Some(guards) = leaf {
            Some(RegisteredEntry::Leaf { guards, fallback })
        } else {
            fallback.map(RegisteredEntry::Function)
        };
        if let Some(entry) = registration {
            entries.registrations.push(Registration { entry_index, entry });
        }
    }
    Ok(entries)
}

fn build_run_initializer(
    reserve: u32,
    load: u32,
    size: i32,
    fasl_size: i32,
    data_index: u32,
) -> Function {
    let mut initializer = Function::new([(1, ValType::I32)]);
    for instruction in [
        Instruction::I32Const(size),
        Instruction::Call(reserve),
        Instruction::LocalTee(0),
        Instruction::I32Eqz,
        Instruction::If(BlockType::Result(ValType::I32)),
        Instruction::I32Const(1),
        Instruction::Else,
        Instruction::LocalGet(0),
        Instruction::I32Const(0),
        Instruction::I32Const(size),
        Instruction::MemoryInit { mem: 0, data_index },
        Instruction::I32Const(fasl_size),
        Instruction::Call(load),
        Instruction::End,
        Instruction::End,
    ] {
        initializer.instruction(&instruction);
    }
    initializer
}

fn link_module(
    runtime: &[u8],
    compiled: &CompiledFile,
    fasl: &[u8],
    strip_debug: bool,
) -> Result<LinkedModule, LinkError> {
    link_units(runtime, &[(compiled, fasl)], None, strip_debug)
}

fn link_units(
    runtime: &[u8],
    units: &[(&CompiledFile, &[u8])],
    entry: Option<&str>,
    strip_debug: bool,
) -> Result<LinkedModule, LinkError> {
    let features = WasmFeatures::WASM2 - WasmFeatures::SIMD;
    let types = Validator::new_with_features(features).validate_all(runtime)?;
    let types = types.as_ref();
    let function_imports = validate_runtime_imports(types)?;
    let runtime_info = inspect_runtime_module(runtime, types, function_imports, strip_debug)?;
    let mut compiled_entries = CompiledWasmEntries::default();
    let mut project_units = Vec::new();
    let mut source_files = Vec::new();
    for &(compiled, fasl) in units {
        let mut unit =
            compile_wasm_entries(compiled, &runtime_info.runtime_functions, strip_debug)?;
        let source_base =
            u32::try_from(source_files.len()).map_err(|_| LinkError::FunctionCount)?;
        for function in unit.leaves.iter_mut().chain(unit.functions.iter_mut()) {
            if let Some(source) = &mut function.source_file {
                *source = source
                    .checked_add(source_base)
                    .ok_or(LinkError::FunctionCount)?;
            }
        }
        source_files.extend(compiled.source_files.iter().cloned());
        project_units.push(crate::abi::ProjectUnit {
            fasl: fasl.to_vec(),
            registrations: encode_registrations(&unit.registrations)?,
        });
        compiled_entries.leaves.extend(unit.leaves);
        compiled_entries.functions.extend(unit.functions);
        compiled_entries.registrations.extend(unit.registrations);
        compiled_entries.bytecode_entries += unit.bytecode_entries;
        compiled_entries
            .leaf_rejections
            .extend(unit.leaf_rejections);
    }
    let (load_buffer, fasl_size) = if let Some(entry) = entry {
        let image = crate::abi::ProjectImage {
            units: project_units,
            entry: entry.into(),
        };
        (crate::abi::encode_project(&image)?, 0)
    } else {
        let unit = project_units.pop().expect("single file unit");
        let fasl_size = i32::try_from(unit.fasl.len()).map_err(|_| LinkError::LoadBufferSize)?;
        let mut buffer = unit.fasl;
        buffer.extend_from_slice(&unit.registrations);
        (buffer, fasl_size)
    };
    let size = i32::try_from(load_buffer.len()).map_err(|_| LinkError::LoadBufferSize)?;
    let initializer = build_run_initializer(
        runtime_info.reserve,
        if entry.is_some() {
            runtime_info
                .project_load
                .ok_or(LinkError::Export(LOAD_PROJECT))?
        } else {
            runtime_info.load
        },
        size,
        fasl_size,
        runtime_info.segments.unwrap_or(0),
    );

    let initializer_index = types.function_count();
    let initializer_type = types.core_type_count_in_module();
    let table = types.table_count();
    let tables = [
        FunctionTable::new(
            table,
            initializer_type + 1,
            runtime_info.leaf_body,
            4,
            Some(ValType::I32),
            compiled_entries.leaves,
        ),
        FunctionTable::new(
            table + 1,
            initializer_type + 2,
            runtime_info.function_body,
            6,
            None,
            compiled_entries.functions,
        ),
    ];

    let mut linker = Linker {
        load_buffer: &load_buffer,
        initializer,
        initializer_type,
        initializer_index,
        cancel: runtime_info.cancel,
        tables,
        has_elements: runtime_info.has_elements,
        segments: runtime_info.segments,
    };
    let mut module = Module::new();
    linker.parse_core_module(&mut module, Parser::new(0), runtime)?;
    if !strip_debug {
        append_debug_sections(&mut module, linker, runtime_info.names, &source_files)?;
    }
    let bytes = module.finish();
    Validator::new_with_features(features)
        .validate_all(&bytes)
        .map_err(LinkError::Output)?;
    Ok(LinkedModule {
        bytes,
        entries: compiled_entries.registrations,
        bytecode_entries: compiled_entries.bytecode_entries,
        leaf_rejections: compiled_entries.leaf_rejections,
    })
}

fn append_debug_sections<'a>(
    module: &mut Module,
    linker: Linker<'a>,
    mut names: BTreeMap<u32, &'a str>,
    source_files: &[zkcl_bytecode::CompiledSource],
) -> Result<(), LinkError> {
    names.insert(linker.initializer_index, RUN);
    let functions = linker.tables.into_iter().flat_map(|table| table.functions);
    let mut map = SourceMap {
        files: source_files
            .iter()
            .map(|source| {
                source
                    .path
                    .clone()
                    .unwrap_or_else(|| "<compiled source>".into())
            })
            .collect(),
        functions: Vec::new(),
    };
    for (function, entry) in (linker.initializer_index + 1..).zip(functions) {
        if let Some(name) = entry.name {
            names.insert(function, name);
        }
        if entry.source_file.is_some() || !entry.instructions.is_empty() {
            map.functions.push(FunctionSourceMap {
                function,
                source_file: entry.source_file,
                instructions: entry.instructions,
            });
        }
    }
    let mut function_names = NameMap::new();
    for (index, name) in names {
        function_names.append(index, name);
    }
    let mut section = NameSection::new();
    section.functions(&function_names);
    module.section(&section);
    if !map.functions.is_empty() {
        module.section(&CustomSection {
            name: SECTION_NAME.into(),
            data: map.encode().map_err(LinkError::SourceMap)?.into(),
        });
    }
    Ok(())
}

fn abi_type(ty: RuntimeAbiType) -> wasmparser::ValType {
    match ty {
        RuntimeAbiType::Bool | RuntimeAbiType::Pointer | RuntimeAbiType::Word => {
            wasmparser::ValType::I32
        }
        RuntimeAbiType::I64 => wasmparser::ValType::I64,
    }
}

struct LinkedFunction<'a> {
    body: Function,
    name: Option<&'a str>,
    source_file: Option<u32>,
    instructions: Vec<InstructionRange>,
}

struct FunctionTable<'a> {
    table: u32,
    ty: u32,
    parameters: u32,
    result: Option<ValType>,
    bridge_body: u32,
    bridge: Function,
    functions: Vec<LinkedFunction<'a>>,
}

impl<'a> FunctionTable<'a> {
    fn new(
        table: u32,
        ty: u32,
        bridge_body: u32,
        parameters: u32,
        result: Option<ValType>,
        functions: Vec<LinkedFunction<'a>>,
    ) -> Self {
        let mut bridge = Function::new([]);
        for arg in (1..=parameters).chain(std::iter::once(0)) {
            bridge.instruction(&Instruction::LocalGet(arg));
        }
        bridge.instruction(&Instruction::CallIndirect {
            type_index: ty,
            table_index: table,
        });
        bridge.instruction(&Instruction::End);
        Self {
            table,
            ty,
            parameters,
            result,
            bridge_body,
            bridge,
            functions,
        }
    }
}

struct Linker<'a> {
    load_buffer: &'a [u8],
    initializer: Function,
    initializer_type: u32,
    initializer_index: u32,
    cancel: u32,
    tables: [FunctionTable<'a>; 2],
    has_elements: bool,
    segments: Option<u32>,
}

type ReencodeError = reencode::Error<Infallible>;

impl Linker<'_> {
    fn append_tables(&self, tables: &mut TableSection) {
        for table in &self.tables {
            let count = table.functions.len() as u64;
            tables.table(TableType {
                element_type: RefType::FUNCREF,
                table64: false,
                minimum: count,
                maximum: Some(count),
                shared: false,
            });
        }
    }

    fn append_elements(&self, elements: &mut ElementSection) {
        let mut first = self.initializer_index + 1;
        for table in &self.tables {
            let end = first + table.functions.len() as u32;
            elements.active(
                Some(table.table),
                &ConstExpr::i32_const(0),
                Elements::Functions((first..end).collect::<Vec<_>>().into()),
            );
            first = end;
        }
    }
}

impl Reencode for Linker<'_> {
    type Error = Infallible;

    fn parse_custom_section(
        &mut self,
        module: &mut Module,
        section: wasmparser::CustomSectionReader<'_>,
    ) -> Result<(), ReencodeError> {
        if section.name().starts_with(".debug_") || matches!(section.name(), "name" | SECTION_NAME)
        {
            return Ok(());
        }
        reencode::utils::parse_custom_section(self, module, section)
    }

    fn parse_type_section(
        &mut self,
        types: &mut TypeSection,
        section: wasmparser::TypeSectionReader<'_>,
    ) -> Result<(), ReencodeError> {
        reencode::utils::parse_type_section(self, types, section)?;
        types.ty().function([], [ValType::I32]);
        for table in &self.tables {
            types.ty().function(
                std::iter::repeat_n(ValType::I32, table.parameters as usize),
                table.result,
            );
        }
        Ok(())
    }

    fn parse_function_section(
        &mut self,
        functions: &mut FunctionSection,
        section: wasmparser::FunctionSectionReader<'_>,
    ) -> Result<(), ReencodeError> {
        reencode::utils::parse_function_section(self, functions, section)?;
        functions.function(self.initializer_type);
        for table in &self.tables {
            for _ in &table.functions {
                functions.function(table.ty);
            }
        }
        Ok(())
    }

    fn parse_table_section(
        &mut self,
        tables: &mut TableSection,
        section: wasmparser::TableSectionReader<'_>,
    ) -> Result<(), ReencodeError> {
        reencode::utils::parse_table_section(self, tables, section)?;
        self.append_tables(tables);
        Ok(())
    }

    fn parse_export_section(
        &mut self,
        exports: &mut ExportSection,
        _section: wasmparser::ExportSectionReader<'_>,
    ) -> Result<(), ReencodeError> {
        exports.export("memory", ExportKind::Memory, 0);
        exports.export(RUN, ExportKind::Func, self.initializer_index);
        exports.export(CANCEL_RUN, ExportKind::Func, self.cancel);
        Ok(())
    }

    fn parse_element_section(
        &mut self,
        elements: &mut ElementSection,
        section: wasmparser::ElementSectionReader<'_>,
    ) -> Result<(), ReencodeError> {
        reencode::utils::parse_element_section(self, elements, section)?;
        self.append_elements(elements);
        Ok(())
    }

    fn parse_code_section(
        &mut self,
        code: &mut CodeSection,
        section: wasmparser::CodeSectionReader<'_>,
    ) -> Result<(), ReencodeError> {
        for (index, body) in section.into_iter().enumerate() {
            let body = body?;
            if let Some(table) = self
                .tables
                .iter()
                .find(|table| index == table.bridge_body as usize)
            {
                code.function(&table.bridge);
            } else {
                code.raw(body.as_bytes());
            }
        }
        code.function(&self.initializer);
        for table in &self.tables {
            for entry in &table.functions {
                code.function(&entry.body);
            }
        }
        Ok(())
    }

    fn parse_data_section(
        &mut self,
        data: &mut DataSection,
        section: wasmparser::DataSectionReader<'_>,
    ) -> Result<(), ReencodeError> {
        reencode::utils::parse_data_section(self, data, section)?;
        data.passive(self.load_buffer.iter().copied());
        Ok(())
    }

    fn data_count(&mut self, count: u32) -> Result<u32, ReencodeError> {
        Ok(count + 1)
    }

    fn intersperse_section_hook(
        &mut self,
        module: &mut Module,
        after: Option<SectionId>,
        before: Option<SectionId>,
    ) -> Result<(), ReencodeError> {
        if before == Some(SectionId::Memory) && after != Some(SectionId::Table) {
            let mut tables = TableSection::new();
            self.append_tables(&mut tables);
            module.section(&tables);
        }
        if !self.has_elements && matches!(before, Some(SectionId::DataCount | SectionId::Code)) {
            let mut elements = ElementSection::new();
            self.append_elements(&mut elements);
            module.section(&elements);
            self.has_elements = true;
        }
        if before == Some(SectionId::Code) && after != Some(SectionId::DataCount) {
            module.section(&DataCountSection {
                count: self.segments.unwrap_or(0) + 1,
            });
        }
        if before.is_none() && self.segments.is_none() {
            let mut data = DataSection::new();
            data.passive(self.load_buffer.iter().copied());
            module.section(&data);
        }
        Ok(())
    }
}

#[cfg(test)]
mod project_tests {
    use super::*;
    use wasm_encoder::{MemorySection, MemoryType};
    use zkcl_bytecode::{Arity, BytecodeFunction, Immediate};

    fn runtime(project_loader: bool) -> Vec<u8> {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], [ValType::I32]);
        types.ty().function([], [ValType::I32]);
        types.ty().function([ValType::I32; 5], [ValType::I32]);
        types.ty().function([ValType::I32; 7], []);
        module.section(&types);
        let mut functions = FunctionSection::new();
        for ty in [0, 0, 1, 2, 3, 0] {
            functions.function(ty);
        }
        module.section(&functions);
        let mut memory = MemorySection::new();
        memory.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memory);
        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        for (index, name) in [
            RESERVE_FASL,
            LOAD_FASL,
            CANCEL_RUN,
            CALL_LEAF,
            CALL_FUNCTION,
            LOAD_PROJECT,
        ]
        .into_iter()
        .enumerate()
        {
            if name != LOAD_PROJECT || project_loader {
                exports.export(name, ExportKind::Func, index as u32);
            }
        }
        module.section(&exports);
        let mut code = CodeSection::new();
        for _ in 0..6 {
            let mut function = Function::new([]);
            function.instruction(&Instruction::Unreachable);
            function.instruction(&Instruction::End);
            code.function(&function);
        }
        module.section(&code);
        module.finish()
    }

    fn unit(value: i64) -> Vec<u8> {
        CompiledFile {
            target: target().unwrap(),
            source_files: Vec::new(),
            nodes: vec![LiteralNode::String(format!("unit {value}"))],
            entries: vec![BytecodeFunction {
                name: Some(format!("UNIT-{value}")),
                source_file: None,
                debug: Default::default(),
                arity: Arity::ZERO,
                registers: 1,
                instructions: vec![
                    zkcl_bytecode::Instruction::Load(0, Immediate::Fixnum(value)),
                    zkcl_bytecode::Instruction::SetValues(0..1),
                ],
                constants: Vec::new(),
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
        }
        .encode()
        .unwrap()
    }

    #[test]
    fn project_preserves_unit_indices_and_concatenates_native_tables() {
        let units = vec![unit(11), unit(29)];
        let linked = link_project(&runtime(true), &units, "CL-USER::MAIN", false).unwrap();
        assert_eq!(linked.entries.len(), 2);
        assert!(linked.entries.iter().all(|entry| entry.entry_index == 0));
        let mut image = None;
        let mut table_sizes = Vec::new();
        for payload in Parser::new(0).parse_all(&linked.bytes) {
            match payload.unwrap() {
                Payload::TableSection(tables) => {
                    table_sizes.extend(tables.into_iter().map(|table| table.unwrap().ty.initial))
                }
                Payload::DataSection(data) => {
                    for segment in data {
                        image = Some(crate::abi::decode_project(segment.unwrap().data).unwrap());
                    }
                }
                _ => {}
            }
        }
        assert_eq!(table_sizes, [2, 0]);
        let image = image.unwrap();
        assert_eq!(image.entry, "CL-USER::MAIN");
        assert_eq!(image.units.len(), 2);
        for (unit, original) in image.units.iter().zip(units) {
            assert_eq!(unit.fasl, original);
            assert_eq!(
                crate::abi::decode_registrations(&unit.registrations).unwrap()[0].entry_index,
                0
            );
        }
    }

    #[test]
    fn project_requires_current_runtime_loader() {
        assert!(matches!(
            link_project(&runtime(false), &[unit(1)], "CL-USER::MAIN", false),
            Err(LinkError::Export(LOAD_PROJECT))
        ));
    }
}
