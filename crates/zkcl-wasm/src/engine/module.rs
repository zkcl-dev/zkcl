use std::collections::HashMap;

use wasmparser::{
    ConstExpr, DataKind, ElementItems, ElementKind, Encoding, ExternalKind,
    FuncValidatorAllocations, KnownCustom, MemoryType, Name, Operator, Parser, Payload, RefType,
    TableInit, TableType, TypeRef, ValType, ValidPayload, Validator, WasmFeatures,
};

use super::{
    CompiledFunction, Error, FuncImport, FuncType, LispLocation, TrapFrame, Value, ValueType,
    lower, unsupported,
};
use crate::source_map::{SECTION_NAME, SourceMap};

#[derive(Default)]
pub struct Module {
    pub(super) types: Vec<FuncType>,
    pub(super) function_types: Vec<u32>,
    imports: Vec<(String, String)>,
    pub(super) functions: Vec<CompiledFunction>,
    pub(super) memories: Vec<MemoryType>,
    pub(super) tables: Vec<(TableType, u64)>,
    pub(super) globals: Vec<u64>,
    pub(super) data: Vec<Segment<u8>>,
    pub(super) elements: Vec<Segment<u64>>,
    pub(super) start: Option<u32>,
    exports: HashMap<String, u32>,
    names: HashMap<u32, Box<str>>,
    sources: SourceMap,
}

pub(super) enum SegmentMode {
    Active { index: u32, offset: u32 },
    Passive,
    Declarative,
}

pub(super) struct Segment<T> {
    pub(super) items: Box<[T]>,
    pub(super) mode: SegmentMode,
}

impl Module {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let features = (WasmFeatures::WASM2 - WasmFeatures::SIMD)
            | WasmFeatures::MULTI_MEMORY
            | WasmFeatures::TAIL_CALL;
        let mut validator = Validator::new_with_features(features);
        let mut parser = Parser::new(0);
        parser.set_features(features);
        let mut module = Self::default();
        let mut allocations = FuncValidatorAllocations::default();
        for payload in parser.parse_all(bytes) {
            let payload = payload?;
            let valid = validator.payload(&payload)?;
            match payload {
                Payload::Version {
                    encoding: Encoding::Module,
                    ..
                } => {}
                Payload::Version { range, .. } => {
                    return Err(unsupported(range.start, "component"));
                }
                Payload::TypeSection(types) => {
                    let offset = types.range().start;
                    for ty in types.into_iter_err_on_gc_types() {
                        let ty = ty?;
                        let read_types = |types: &[ValType]| -> Result<Box<[ValueType]>, Error> {
                            types.iter().map(|ty| value_type(*ty, offset)).collect()
                        };
                        module.types.push(FuncType {
                            params: read_types(ty.params())?,
                            results: read_types(ty.results())?,
                        });
                    }
                }
                Payload::ImportSection(imports) => {
                    for entry in imports.into_iter_with_offsets() {
                        let (offset, entry) = entry?;
                        let TypeRef::Func(ty) = entry.ty else {
                            return Err(unsupported(
                                offset,
                                format!("import {}.{}", entry.module, entry.name),
                            ));
                        };
                        module.function_types.push(ty);
                        module
                            .imports
                            .push((entry.module.into(), entry.name.into()));
                    }
                }
                Payload::FunctionSection(functions) => {
                    for ty in functions {
                        module.function_types.push(ty?);
                    }
                }
                Payload::MemorySection(memories) => {
                    for memory in memories {
                        module.memories.push(memory?);
                    }
                }
                Payload::GlobalSection(globals) => {
                    for global in globals.into_iter_with_offsets() {
                        let (offset, global) = global?;
                        value_type(global.ty.content_type, offset)?;
                        module.globals.push(initializer_bits(global.init_expr)?);
                    }
                }
                Payload::TableSection(tables) => {
                    for table in tables.into_iter_with_offsets() {
                        let (offset, table) = table?;
                        if table.ty.element_type != RefType::FUNCREF {
                            return Err(unsupported(offset, "table element type"));
                        }
                        let initial = match table.init {
                            TableInit::RefNull => 0,
                            TableInit::Expr(expr) => initializer_bits(expr)?,
                        };
                        module.tables.push((table.ty, initial));
                    }
                }
                Payload::ExportSection(exports) => {
                    for export in exports {
                        let export = export?;
                        if export.kind == ExternalKind::Func {
                            module.exports.insert(export.name.into(), export.index);
                        }
                    }
                }
                Payload::StartSection { func, .. } => module.start = Some(func),
                Payload::DataSection(data) => {
                    for segment in data {
                        let segment = segment?;
                        let mode = match segment.kind {
                            DataKind::Passive => SegmentMode::Passive,
                            DataKind::Active {
                                memory_index,
                                offset_expr,
                            } => SegmentMode::Active {
                                index: memory_index,
                                offset: initializer_bits(offset_expr)? as u32,
                            },
                        };
                        module.data.push(Segment {
                            items: segment.data.into(),
                            mode,
                        });
                    }
                }
                Payload::ElementSection(elements) => {
                    for segment in elements {
                        let segment = segment?;
                        let mode = match segment.kind {
                            ElementKind::Active {
                                table_index,
                                offset_expr,
                            } => SegmentMode::Active {
                                index: table_index.unwrap_or(0),
                                offset: initializer_bits(offset_expr)? as u32,
                            },
                            ElementKind::Passive => SegmentMode::Passive,
                            ElementKind::Declared => SegmentMode::Declarative,
                        };
                        let items = match segment.items {
                            ElementItems::Functions(functions) => functions
                                .into_iter()
                                .map(|index| Ok(u64::from(index?) + 1))
                                .collect::<Result<_, Error>>()?,
                            ElementItems::Expressions(RefType::FUNCREF, expressions) => expressions
                                .into_iter()
                                .map(|expr| initializer_bits(expr?))
                                .collect::<Result<_, Error>>()?,
                            _ => return Err(unsupported(segment.range.start, "element type")),
                        };
                        module.elements.push(Segment { items, mode });
                    }
                }
                Payload::CodeSectionEntry(_) => {
                    let ValidPayload::Func(function, body) = valid else {
                        unreachable!()
                    };
                    let mut function = function.into_validator(allocations);
                    let compiled = lower::function(&module, &mut function, body)?;
                    allocations = function.into_allocations();
                    module.functions.push(compiled);
                }
                Payload::CustomSection(section) => {
                    if section.name() == SECTION_NAME {
                        if let Some(sources) = SourceMap::decode(section.data()) {
                            module.sources = sources;
                        }
                    } else if let KnownCustom::Name(names) = section.as_known() {
                        for name in names.flatten() {
                            if let Name::Function(functions) = name {
                                module.names.extend(
                                    functions
                                        .into_iter()
                                        .flatten()
                                        .map(|name| (name.index, name.name.into())),
                                );
                            }
                        }
                    }
                }
                Payload::CodeSectionStart { .. }
                | Payload::DataCountSection { .. }
                | Payload::End(_) => {}
                other => {
                    return Err(unsupported(
                        other.as_section().map_or(0, |(_, range)| range.start),
                        "section",
                    ));
                }
            }
        }
        Ok(module)
    }

    pub fn imports(&self) -> impl Iterator<Item = FuncImport<'_>> {
        (0..self.imports.len()).map(|index| self.import(index))
    }

    pub fn exported_function(&self, name: &str) -> Option<&FuncType> {
        self.exports
            .get(name)
            .map(|index| self.function_type(*index))
    }

    pub fn has_start(&self) -> bool {
        self.start.is_some()
    }

    pub(super) fn export_index(&self, name: &str) -> Option<u32> {
        self.exports.get(name).copied()
    }

    pub(super) fn function_type(&self, index: u32) -> &FuncType {
        &self.types[self.function_types[index as usize] as usize]
    }

    pub(super) fn matches_signature(&self, function: u32, expected: u32) -> bool {
        let actual = self.function_types[function as usize];
        // Distinct type indices can name structurally equal signatures.
        actual == expected || self.types[actual as usize] == self.types[expected as usize]
    }

    pub(super) fn trap_frame(&self, function: u32, offset: Option<usize>) -> TrapFrame {
        let name = self.names.get(&function).cloned().or_else(|| {
            if let Some((module, name)) = self.imports.get(function as usize) {
                return Some(format!("{module}.{name}").into_boxed_str());
            }
            self.exports
                .iter()
                .filter_map(|(name, index)| (*index == function).then_some(name.as_str()))
                .min()
                .map(Into::into)
        });
        TrapFrame {
            function,
            name,
            offset,
            lisp: self.lisp_location(function, offset).map(Box::new),
        }
    }

    fn lisp_location(&self, function: u32, offset: Option<usize>) -> Option<LispLocation> {
        let index = (function as usize).checked_sub(self.imports.len())?;
        let body = self.functions.get(index)?;
        let source = self.sources.function(function)?;
        let instruction = offset
            .and_then(|offset| offset.checked_sub(body.body_offset))
            .and_then(|offset| u32::try_from(offset).ok())
            .and_then(|offset| source.instruction(offset));
        let file = source
            .source_file
            .map(|index| self.sources.files[index as usize].as_str().into());
        if file.is_none() && instruction.is_none() {
            return None;
        }
        Some(LispLocation {
            file,
            bytecode_pc: instruction.map(|site| site.bytecode_pc),
            source_offset: instruction.and_then(|site| site.source_offset),
        })
    }

    fn import(&self, index: usize) -> FuncImport<'_> {
        let (module, name) = &self.imports[index];
        FuncImport {
            module,
            name,
            ty: self.function_type(index as u32),
        }
    }
}

pub(super) fn value_type(ty: ValType, offset: usize) -> Result<ValueType, Error> {
    match ty {
        ValType::I32 => Ok(ValueType::I32),
        ValType::I64 => Ok(ValueType::I64),
        ValType::F32 => Ok(ValueType::F32),
        ValType::F64 => Ok(ValueType::F64),
        ValType::Ref(RefType::FUNCREF) => Ok(ValueType::FuncRef),
        _ => Err(unsupported(offset, format!("value type {ty}"))),
    }
}

/// Evaluate a constant initializer expression to the word the runtime stores.
fn initializer_bits(expr: ConstExpr<'_>) -> Result<u64, Error> {
    let mut reader = expr.get_operators_reader();
    let (operator, offset) = reader.read_with_offset()?;
    let value = match operator {
        Operator::I32Const { value } => Value::I32(value),
        Operator::I64Const { value } => Value::I64(value),
        Operator::F32Const { value } => Value::F32(value.bits()),
        Operator::F64Const { value } => Value::F64(value.bits()),
        Operator::RefNull { hty } if hty == wasmparser::HeapType::FUNC => Value::FuncRef(None),
        Operator::RefFunc { function_index } => return Ok(u64::from(function_index) + 1),
        other => return Err(unsupported(offset, format!("initializer {other:?}"))),
    };
    Ok(value.bits())
}
