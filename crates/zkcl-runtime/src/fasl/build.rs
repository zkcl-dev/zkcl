use std::path::Path;

use crate::compile::{FileCompiler, Warning};
use crate::macros::CallExpander;
use crate::read::Form;

use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;
use rustc_hash::FxHashMap;
use zkcl_bytecode::{ArrayContents, BytecodeFunction, CompiledFile, HashTableTest, LiteralNode};

use crate::heap::{ArrayElement, ArrayValue, HashTest, RootFrame, RootSlot, Word};
use crate::number::{Number, Real};
use crate::symbol::Name;
use crate::vm::callback::CallValues;
use crate::vm::{ConstantTemplate, FunctionTemplate, Primitive};
use crate::{EvalError, Runtime, macros, pathname};

use super::load_forms::{CompiledLoadForms, emit_load_forms};

/// Build a file's literal graph and schedule any MAKE-LOAD-FORM entries.
pub(crate) struct CompiledFileBuilder {
    image: bool,
    compiled: CompiledFile,
    input_source: Option<u32>,
    objects: FxHashMap<Word, u32>,
    roots: LiteralRoots,
    epoch: usize,
    pending: Vec<(u32, Word)>,
    warnings: Vec<Warning>,
}

/// Keep literal identities across compile-time Lisp calls without allocating
/// a separate registered root for every object in the file.
#[derive(Default)]
struct LiteralRoots {
    nodes: Vec<u32>,
    frames: Vec<RootFrame>,
}

impl LiteralRoots {
    // Use dedicated frames so finishing a large compile also releases the
    // backing slots. Small frames come from the thread's persistent slab.
    const FRAME_LEN: usize = 256;

    fn push(&mut self, runtime: &Runtime, node: u32, word: Word) {
        let slot = self.nodes.len() % Self::FRAME_LEN;
        if slot == 0 {
            self.frames.push(runtime.root_frame(Self::FRAME_LEN));
        }
        self.frames
            .last()
            .expect("active literal root frame")
            .set(slot, word);
        self.nodes.push(node);
    }

    fn iter(&self) -> impl Iterator<Item = (u32, Word)> + '_ {
        self.nodes
            .chunks(Self::FRAME_LEN)
            .zip(&self.frames)
            .flat_map(|(nodes, frame)| {
                nodes
                    .iter()
                    .enumerate()
                    .map(move |(slot, &node)| (node, frame.get(slot)))
            })
    }
}

impl CompiledFileBuilder {
    /// Complete one form's compile-time work, including externalizing its
    /// literals. The caller handles warnings before the reader advances.
    pub(super) fn compile_form(
        &mut self,
        runtime: &Runtime,
        compiler: &mut FileCompiler<'_>,
        form: &Form,
        expander: Option<&CallExpander<'_>>,
        call: Option<&CallValues<'_>>,
    ) -> Result<Vec<Warning>, EvalError> {
        compiler.compile_top_level_form(form, expander, &mut |code| {
            self.push_top_level_entry(runtime, code, call)
        })?;
        let mut warnings = compiler.take_warnings();
        warnings.append(&mut self.warnings);
        Ok(warnings)
    }

    pub(crate) fn new(runtime: &Runtime, source: Option<(&Path, &str)>) -> Self {
        let source_files: Vec<_> = source
            .map(|(path, text)| zkcl_bytecode::CompiledSource {
                uri: None,
                version: None,
                base_offset: 0,
                path: path.to_str().map(str::to_owned),
                text: Some(text.to_owned()),
            })
            .into_iter()
            .collect();
        Self {
            image: false,
            input_source: (!source_files.is_empty()).then_some(0),
            compiled: CompiledFile {
                target: runtime.compilation_target().clone(),
                source_files,
                nodes: Vec::new(),
                entries: Vec::new(),
            },
            objects: FxHashMap::default(),
            roots: LiteralRoots::default(),
            epoch: runtime.heap.gc_epoch(),
            pending: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub(super) fn new_document(runtime: &Runtime, document: &crate::SourceDocument) -> Self {
        let mut builder = Self::new(runtime, None);
        builder.input_source = builder.source_file(document);
        builder
    }

    pub(super) fn push_top_level_entry(
        &mut self,
        runtime: &Runtime,
        code: FunctionTemplate,
        call: Option<&CallValues<'_>>,
    ) -> Result<(), EvalError> {
        let first_node = self.compiled.nodes.len();
        let mut objects = Vec::new();
        let entry = self.encode_code(runtime, code, self.input_source, &mut objects)?;
        if !objects.is_empty() {
            self.add_load_forms(runtime, call, first_node, objects)?;
        }
        self.compiled.entries.push(entry);
        Ok(())
    }

    pub(crate) fn push_image_entry(
        &mut self,
        runtime: &Runtime,
        code: FunctionTemplate,
        hooks: &dyn crate::image::ImageHooks,
    ) -> Result<(), EvalError> {
        self.image = true;
        let mut pending = Vec::new();
        let entry = self.code(runtime, code, None, &mut pending)?;
        let mut documentation = Vec::new();
        while let Some((id, mut word)) = pending.pop() {
            let node = if word == Word::UNBOUND {
                LiteralNode::Unbound
            } else if let Some((kind, data, edges)) = runtime.heap.image_object(word) {
                let slots = edges
                    .into_iter()
                    .map(|edge| self.reference(runtime, edge, &mut pending))
                    .collect::<Result<_, _>>()?;
                LiteralNode::RuntimeObject { kind, data, slots }
            } else if let Some(index) = word
                .native_index()
                .filter(|&index| index < runtime.builtin_native_count.get())
            {
                LiteralNode::RuntimeObject {
                    kind: "builtin-native".into(),
                    data: vec![index as u64],
                    slots: Vec::new(),
                }
            } else if let Some(index) = word.package_index() {
                LiteralNode::RuntimeObject {
                    kind: "package".into(),
                    data: vec![index as u64],
                    slots: Vec::new(),
                }
            } else {
                match self.heap_node(runtime, word, &mut pending) {
                    Err(error) if error.to_string().contains("non-local exit target") => {
                        return Err(error);
                    }
                    Ok(LiteralNode::LoadForm { .. }) | Err(_) => {
                        let resource = runtime.root(word);
                        let Some((key, data)) = hooks.save_external(runtime, &resource)? else {
                            return Err(EvalError::new(
                                0,
                                format!(
                                    "cannot save external or unsupported image object {}; supply an image save/restore hook",
                                    runtime.repr(&resource)?
                                ),
                            ));
                        };
                        word = resource.slot.get();
                        let epoch = runtime.heap.gc_epoch();
                        if self.epoch != epoch {
                            let moved: FxHashMap<_, _> = self.roots.iter().collect();
                            self.objects.retain(|word, _| word.is_immediate());
                            self.objects
                                .extend(moved.iter().map(|(&node, &word)| (word, node)));
                            for (node, word) in &mut pending {
                                if let Some(current) = moved.get(node) {
                                    *word = *current;
                                }
                            }
                            self.epoch = epoch;
                        }
                        LiteralNode::ExternalResource { key, data }
                    }
                    Ok(node) => node,
                }
            };
            if runtime.heap.function_info(word).is_some() {
                let doc = runtime
                    .heap
                    .function_documentation(word)
                    .unwrap_or(Word::NIL);
                documentation.push(id);
                documentation.push(self.reference(runtime, doc, &mut pending)?);
            }
            self.compiled.nodes[id as usize] = node;
        }
        let mut docs = entry.clone();
        docs.name = Some("image function documentation".into());
        docs.constants = documentation;
        docs.lambda_list = None;
        self.compiled.entries.push(entry);
        self.compiled.entries.push(docs);
        Ok(())
    }

    fn add_load_forms(
        &mut self,
        runtime: &Runtime,
        call: Option<&CallValues<'_>>,
        first_node: usize,
        mut objects: Vec<(u32, RootSlot)>,
    ) -> Result<(), EvalError> {
        let symbol = runtime.intern(&Name::common_lisp("MAKE-LOAD-FORM"), 0)?;
        let make_load_form = runtime.root(symbol);
        let environment = runtime.nil();
        let mut forms = Vec::new();
        while let Some((node_id, object)) = objects.pop() {
            let object = runtime.root(object.get());
            let args = [&object, &environment];
            let values = match call {
                Some(call) => call(&make_load_form, &args, 0)?,
                None => crate::vm::call_host(runtime, &make_load_form, &args)?
                    .into_iter()
                    .map(|value| value.slot)
                    .collect(),
            };
            let mut compile =
                |value: &RootSlot| self.compile_load_form(runtime, value, call, &mut objects);
            let creation = compile(values.first().unwrap_or(&environment.slot))?;
            let initialization = values
                .get(1)
                .filter(|value| value.get() != Word::NIL)
                .map(compile)
                .transpose()?;
            forms.push(CompiledLoadForms {
                node_id,
                creation,
                initialization,
            });
        }
        emit_load_forms(&mut self.compiled, first_node, forms)
    }

    fn compile_load_form(
        &mut self,
        runtime: &Runtime,
        value: &RootSlot,
        call: Option<&CallValues<'_>>,
        objects: &mut Vec<(u32, RootSlot)>,
    ) -> Result<BytecodeFunction, EvalError> {
        let form = crate::form_value::from_value(runtime, &runtime.root(value.get()), 0)?;
        let (code, warnings) = match call {
            Some(call) => {
                let expand = |function: &crate::Root<'_>, args: &[&crate::Root<'_>], at, _kind| {
                    Ok(call(function, args, at)?
                        .first()
                        .map_or(Word::NIL, RootSlot::get))
                };
                crate::compile::compile(runtime, &form, Some(&expand))?
            }
            None => crate::compile::compile(runtime, &form, None)?,
        };
        self.warnings.extend(warnings);
        self.encode_code(runtime, code, None, objects)
    }

    fn encode_code(
        &mut self,
        runtime: &Runtime,
        code: FunctionTemplate,
        source_file: Option<u32>,
        objects: &mut Vec<(u32, RootSlot)>,
    ) -> Result<BytecodeFunction, EvalError> {
        let epoch = runtime.heap.gc_epoch();
        if self.epoch != epoch {
            self.objects.retain(|word, _| word.is_immediate());
            self.objects
                .extend(self.roots.iter().map(|(node, word)| (word, node)));
            self.epoch = epoch;
        }

        // This walk reads the Lisp heap without allocating into it. Rooting an
        // object registers a Rust slot and cannot forward the queued words.
        let mut pending = std::mem::take(&mut self.pending);
        let code = self.code(runtime, code, source_file, &mut pending)?;
        while let Some((id, word)) = pending.pop() {
            // Symbols have no outgoing literal edges. Their final home is
            // known only after the last compile-time form has run.
            if word.is_heap_object() && runtime.heap.is_symbol(word) {
                continue;
            }
            let node = self.heap_node(runtime, word, &mut pending)?;
            if matches!(&node, LiteralNode::LoadForm { .. }) {
                objects.push((id, runtime.root_slot(word)));
            }
            self.compiled.nodes[id as usize] = node;
        }
        self.pending = pending;
        Ok(code)
    }

    pub(crate) fn finish(mut self, runtime: &Runtime) -> Result<CompiledFile, EvalError> {
        // A later compile-time form can unintern a symbol. Encode the final
        // home so the displaced symbol cannot coalesce with its replacement.
        for (node, word) in self.roots.iter() {
            if !self.image && runtime.heap.is_symbol(word) {
                self.compiled.nodes[node as usize] = encode_symbol(runtime, word)?;
            }
        }
        Ok(self.compiled)
    }

    fn add(&mut self, node: LiteralNode) -> Result<u32, EvalError> {
        let id = u32::try_from(self.compiled.nodes.len()).map_err(|_| {
            EvalError::compile(0, "compiled file literal graph is too large")
                .without_source_location()
        })?;
        self.compiled.nodes.push(node);
        Ok(id)
    }

    fn reference(
        &mut self,
        runtime: &Runtime,
        word: Word,
        pending: &mut Vec<(u32, Word)>,
    ) -> Result<u32, EvalError> {
        if let Some(id) = self.objects.get(&word) {
            return Ok(*id);
        }
        let id = self.record_object(runtime, word, LiteralNode::Nil)?;
        pending.push((id, word));
        Ok(id)
    }

    fn record_object(
        &mut self,
        runtime: &Runtime,
        word: Word,
        node: LiteralNode,
    ) -> Result<u32, EvalError> {
        let id = self.add(node)?;
        self.objects.insert(word, id);
        if !word.is_immediate() {
            self.roots.push(runtime, id, word);
        }
        Ok(id)
    }

    fn code(
        &mut self,
        runtime: &Runtime,
        source: FunctionTemplate,
        source_file: Option<u32>,
        pending: &mut Vec<(u32, Word)>,
    ) -> Result<BytecodeFunction, EvalError> {
        let mut constants = Vec::with_capacity(source.constants.len());
        for datum in source.constants {
            let id = match datum {
                ConstantTemplate::Immediate(word) => self.reference(runtime, word, pending)?,
                ConstantTemplate::Rooted(root) => self.reference(runtime, root.get(), pending)?,
                ConstantTemplate::LoadTimeCell(cell) => {
                    // EVAL-WHEN may have run the initializer already. Keep the
                    // cell's identity, but let LOAD provide its contents.
                    if let Some(id) = self.objects.get(&cell.get()) {
                        *id
                    } else {
                        let nil = self.reference(runtime, Word::NIL, pending)?;
                        self.record_object(runtime, cell.get(), LiteralNode::Pair(nil, nil))?
                    }
                }
                ConstantTemplate::String(text, _) => self.add(LiteralNode::String(text))?,
                ConstantTemplate::Symbol(name, at) => self.symbol(runtime, &name, at, pending)?,
                ConstantTemplate::Pair(car, cdr) => {
                    self.add(LiteralNode::Pair(constants[car], constants[cdr]))?
                }
                ConstantTemplate::Function(function) => {
                    let nested = self.code(runtime, *function, source_file, pending)?;
                    self.add(LiteralNode::Function(nested))?
                }
            };
            constants.push(id);
        }
        let mut code = BytecodeFunction {
            name: source.name,
            source_file,
            debug: source.debug,
            arity: source.arity,
            registers: source.registers,
            instructions: source.instructions,
            constants,
            lambda_list: source.lambda_list,
            captures: source.captures,
            notinline: source.notinline,
            vop_calls: source.vop_calls,
            handlers: source.handlers,
            restarts: source.restarts,
            keywords: source.keywords,
            jump_tables: source.jump_tables,
            facts: source.facts,
        };
        let bounds = self.compiled.target.fixnum_bounds();
        if bounds.0 > Word::FIXNUM_MIN as i64 {
            super::spill_wide_fixnums(&mut code, bounds, |value| {
                let word = Word::fixnum(value as isize).expect("compiler emitted a host fixnum");
                self.reference(runtime, word, pending)
            })?;
        }
        Ok(code)
    }

    fn source_file(&mut self, source: &crate::SourceDocument) -> Option<u32> {
        let path = source.path().and_then(Path::to_str);
        let text = source.text();
        let files = &mut self.compiled.source_files;
        if let Some(index) = files.iter().position(|known| {
            known.path.as_deref() == path
                && known.text.as_deref() == text
                && known.uri.as_deref() == source.uri()
                && known.version == source.version()
                && known.base_offset == source.base_offset()
        }) {
            return Some(index as u32);
        }
        let index = u32::try_from(files.len()).ok()?;
        files.push(zkcl_bytecode::CompiledSource {
            uri: source.uri().map(str::to_owned),
            version: source.version(),
            base_offset: source.base_offset(),
            path: path.map(str::to_owned),
            text: text.map(str::to_owned),
        });
        Some(index)
    }

    fn symbol(
        &mut self,
        runtime: &Runtime,
        name: &Name,
        at: usize,
        pending: &mut Vec<(u32, Word)>,
    ) -> Result<u32, EvalError> {
        if let Some(word) = runtime.find_named_symbol(name) {
            return self.reference(runtime, word, pending);
        }
        let registry = runtime.packages.borrow();
        let package = name.package().and_then(|package| registry.name(package));
        let package =
            package.ok_or_else(|| EvalError::compile(at, "symbol's package is unavailable"))?;
        self.add(LiteralNode::InternedSymbol {
            package: package.into(),
            name: name.spelling().into(),
        })
    }

    fn heap_node(
        &mut self,
        runtime: &Runtime,
        word: Word,
        pending: &mut Vec<(u32, Word)>,
    ) -> Result<LiteralNode, EvalError> {
        if word == Word::NIL {
            return Ok(LiteralNode::Nil);
        }
        if word == Word::T {
            return Ok(LiteralNode::True);
        }
        if let Some(number) = word.as_fixnum() {
            let value = number as i64;
            let (min, max) = self.compiled.target.fixnum_bounds();
            return Ok(if (min..=max).contains(&value) {
                LiteralNode::Fixnum(value)
            } else {
                bignum_literal(&BigInt::from(value))
            });
        }
        if let Some(character) = word.as_character() {
            return Ok(LiteralNode::Character(character));
        }
        if let Some(state) = runtime.heap.random_state_value(word) {
            return Ok(LiteralNode::RandomState(state.words()));
        }
        if let Some(pack) = runtime.heap.simd_pack(word) {
            return Ok(LiteralNode::SimdPack(pack));
        }
        if let Some(mask) = runtime.heap.simd_mask(word) {
            return Ok(LiteralNode::SimdMask(mask));
        }
        if let Some((numerator, denominator)) = runtime.heap.ratio_fields(word) {
            return Ok(LiteralNode::Ratio(
                self.reference(runtime, numerator, pending)?,
                self.reference(runtime, denominator, pending)?,
            ));
        }
        if let Some((real, imag)) = runtime.heap.complex_fields(word) {
            return Ok(LiteralNode::Complex(
                self.reference(runtime, real, pending)?,
                self.reference(runtime, imag, pending)?,
            ));
        }
        if let Some(Number::Real(real)) = runtime.heap.number(word) {
            return Ok(match real {
                Real::Integer(value) => {
                    let (min, max) = self.compiled.target.fixnum_bounds();
                    if let Some(number) = value.to_i64().filter(|n| (min..=max).contains(n)) {
                        LiteralNode::Fixnum(number)
                    } else {
                        bignum_literal(&value)
                    }
                }
                Real::Single(value) => LiteralNode::SingleFloat(value.to_bits()),
                Real::Double(value) => LiteralNode::DoubleFloat(value.to_bits()),
                Real::Ratio(_) => unreachable!("ratio literal uses component references"),
            });
        }
        if let Some((car, cdr)) = runtime.heap.cons_fields(word) {
            return Ok(LiteralNode::Pair(
                self.reference(runtime, car, pending)?,
                self.reference(runtime, cdr, pending)?,
            ));
        }
        if let Some(package) = crate::package::Package::from_word(word) {
            return Ok(LiteralNode::Package(encode_package_name(runtime, package)?));
        }
        if let Some(fields) = runtime.heap.pathname_fields(word) {
            if pathname::normalized_fields(runtime, fields).is_none() {
                return Err(
                    EvalError::compile(0, "pathname literal has invalid components")
                        .without_source_location(),
                );
            }
            let mut references = [0; 6];
            for (index, value) in fields.into_iter().enumerate() {
                references[index] = self.reference(runtime, value, pending)?;
            }
            return Ok(LiteralNode::Pathname(references));
        }
        if let Some(info) = runtime.heap.hash_table_info(word) {
            let mut entries = Vec::with_capacity(info.count);
            let mut cursor = 0;
            while let Some((key, value)) = runtime.heap.hash_table_next_entry(word, &mut cursor) {
                entries.push((
                    self.reference(runtime, key, pending)?,
                    self.reference(runtime, value, pending)?,
                ));
            }
            return Ok(LiteralNode::HashTable {
                test: encode_hash_test(info.test),
                weakness: info.weakness,
                rehash_size: info.rehash_size,
                rehash_threshold: info.rehash_threshold,
                entries,
            });
        }
        if let Some(info) = runtime.heap.array_info(word) {
            if info.element == ArrayElement::Character && info.rank == 1 && info.simple() {
                let text = runtime.heap.string_contents(word).expect("simple string");
                return Ok(LiteralNode::String(text));
            }
            let dimensions = runtime
                .heap
                .array_dimensions(word)
                .expect("array dimensions");
            let contents = if let Some((target, offset)) = info.displacement {
                ArrayContents::Displaced {
                    target: self.reference(runtime, target, pending)?,
                    offset,
                }
            } else {
                let values = (0..info.size).map(|index| {
                    runtime
                        .heap
                        .array_value(word, index)
                        .expect("array element")
                });
                match info.element {
                    ArrayElement::Character => {
                        let elements = runtime
                            .heap
                            .with_array_character_slice(word, 0, info.size, |text| {
                                text.chars()
                                    .map(|value| self.reference(runtime, Word::character(value), pending))
                                    .collect::<Result<_, _>>()
                            })
                            .expect("character array storage")?;
                        ArrayContents::Elements(elements)
                    }
                    ArrayElement::Octet => ArrayContents::Octets(
                        runtime
                            .heap
                            .octet_array_range(word, 0, info.size)
                            .expect("octet storage"),
                    ),
                    ArrayElement::SingleFloat => ArrayContents::SingleFloats(
                        values
                            .map(|value| {
                                let ArrayValue::SingleFloat(value) = value else {
                                    unreachable!("single-float array storage");
                                };
                                value.to_bits()
                            })
                            .collect(),
                    ),
                    ArrayElement::DoubleFloat => ArrayContents::DoubleFloats(
                        values
                            .map(|value| {
                                let ArrayValue::DoubleFloat(value) = value else {
                                    unreachable!("double-float array storage");
                                };
                                value.to_bits()
                            })
                            .collect(),
                    ),
                    ArrayElement::General | ArrayElement::Bit => {
                        let elements = values
                            .map(|value| self.reference(runtime, value.expect_word(), pending))
                            .collect::<Result<_, _>>()?;
                        ArrayContents::Elements(elements)
                    }
                }
            };
            return Ok(LiteralNode::Array {
                dimensions,
                element: info.element,
                adjustable: info.adjustable,
                fill_pointer: info.fill_pointer,
                contents,
            });
        }
        if let Some(name) = Primitive::compiled_name(word) {
            return Ok(LiteralNode::Primitive(name.into()));
        }
        if let Some(id) = macros::compiled_native_id(word) {
            return Ok(LiteralNode::BuiltinNative(id));
        }
        if let Some(value) = runtime.heap.read_cell(word) {
            return Ok(LiteralNode::LexicalCell(self.reference(runtime, value, pending)?));
        }
        if let Some(code) = runtime
            .heap
            .function_code(word)
            .filter(|&code| code != word)
        {
            let code_info = runtime.heap.function_info(code).expect("closure code");
            let vm_code = runtime.heap.code_store.get(code_info.code_id);
            if vm_code
                .captures
                .contains(&zkcl_bytecode::CaptureKind::ExitToken)
            {
                return Err(EvalError::compile(
                    0,
                    "cannot persist a closure with a non-local exit target",
                )
                .without_source_location());
            }
            let captures = vm_code
                .captures
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    self.reference(
                        runtime,
                        runtime.heap.capture(word, index).expect("closure capture"),
                        pending,
                    )
                })
                .collect::<Result<_, _>>()?;
            return Ok(LiteralNode::Closure {
                code: self.reference(runtime, code, pending)?,
                captures,
                documentation: self.reference(
                    runtime,
                    runtime
                        .heap
                        .function_documentation(word)
                        .unwrap_or(Word::NIL),
                    pending,
                )?,
            });
        }
        if let Some(info) = runtime.heap.function_info(word) {
            let vm_code = runtime.heap.code_store.get(info.code_id);
            let constants = (0..info.constants)
                .map(|index| {
                    self.reference(runtime, runtime.heap.function_constant(word, index), pending)
                })
                .collect::<Result<_, _>>()?;
            return Ok(LiteralNode::Function(BytecodeFunction {
                name: vm_code.name.clone(),
                source_file: vm_code
                    .source
                    .as_ref()
                    .and_then(|source| self.source_file(source)),
                debug: vm_code.debug.clone(),
                arity: vm_code.arity,
                registers: vm_code.registers,
                instructions: vm_code.canonical.to_vec(),
                constants,
                lambda_list: vm_code.lambda_list,
                captures: vm_code.captures.to_vec(),
                notinline: vm_code.notinline.to_vec(),
                vop_calls: vm_code.vop_calls.to_vec(),
                handlers: vm_code.handlers.to_vec(),
                restarts: vm_code.restarts.to_vec(),
                keywords: vm_code.keywords.to_vec(),
                jump_tables: vm_code.jump_tables.to_vec(),
                facts: zkcl_bytecode::FunctionFacts {
                    params: vm_code.param_facts.to_vec(),
                },
            }));
        }
        if crate::vm::classes::requires_load_form(runtime, word) {
            // Assign entry indices after all the method-generated constants
            // have been discovered and their dependencies are known.
            return Ok(LiteralNode::LoadForm {
                creation: usize::MAX,
                initialization: None,
            });
        }
        Err(
            EvalError::compile(0, "object has no compiled-file literal representation")
                .without_source_location(),
        )
    }
}

fn bignum_literal(value: &BigInt) -> LiteralNode {
    let (sign, magnitude) = value.to_bytes_le();
    LiteralNode::Bignum {
        negative: sign == Sign::Minus,
        magnitude,
    }
}

fn encode_symbol(runtime: &Runtime, word: Word) -> Result<LiteralNode, EvalError> {
    let data = runtime
        .heap
        .symbol_attributes(word)
        .expect("symbol literal");
    let name = runtime.heap.symbol_name(word).expect("symbol name");
    match data.home {
        Some(package) => Ok(LiteralNode::InternedSymbol {
            package: encode_package_name(runtime, package)?,
            name,
        }),
        None => Ok(LiteralNode::UninternedSymbol(name)),
    }
}

fn encode_package_name(
    runtime: &Runtime,
    package: crate::package::Package,
) -> Result<String, EvalError> {
    runtime
        .packages
        .borrow()
        .name(package)
        .map(str::to_owned)
        .ok_or_else(|| {
            EvalError::compile(0, "deleted package cannot be a compiled literal")
                .without_source_location()
        })
}

fn encode_hash_test(test: HashTest) -> HashTableTest {
    match test {
        HashTest::Eq => HashTableTest::Eq,
        HashTest::Eql => HashTableTest::Eql,
        HashTest::Equal => HashTableTest::Equal,
        HashTest::Equalp => HashTableTest::Equalp,
    }
}
