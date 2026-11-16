use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::PathBuf;

use num_bigint::{BigInt, Sign};
use rustc_hash::FxHashMap;
use zkcl_bytecode::{ArrayContents, BytecodeFunction, CompiledFile, HashTableTest, LiteralNode};

use crate::code_store::{CodeId, CodeSideTables, CodeToInstall};
use crate::condition::Class;
use crate::heap::{ArrayElement, ArrayValue, HashTest, RootFrame, Word};
use crate::native::{AotCodeDescriptor, AotModuleDescriptor};
use crate::number::{Number, Real};
use crate::vm::ops::CanonicalSymbol;
use crate::vm::{Primitive, canonical_symbol_for_word, equality};
use crate::{EvalError, OutOfMemory, Runtime, SourceDocument, macros, pathname};

/// Load literals and callable entries from an already decoded file.
pub(crate) struct CompiledFileLoader {
    image_only: bool,
    entries: std::iter::Enumerate<std::vec::IntoIter<BytecodeFunction>>,
    source_files: Vec<SourceDocument>,
    nodes: Vec<LiteralNode>,
    canonical_symbols: Vec<Option<CanonicalSymbol>>,
    literals: RootFrame,
    work: LiteralWorklist,
    // Creation entries publish their primary value at the next VM resume.
    load_form_entries: FxHashMap<usize, Option<u32>>,
    pending_created_literal: Option<u32>,
    pending_equalp_tables: Vec<u32>,
    compiled_entries: FxHashMap<usize, LinkedCode>,
    compiled_functions: FxHashMap<usize, LinkedCode>,
}

enum LinkedCode {
    Native(&'static AotCodeDescriptor),
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    Wasm {
        leaf: Option<(crate::native::LeafEntry, Box<[crate::native::LeafGuard]>)>,
        function: Option<(crate::native::JitCEntry, usize, bool)>,
    },
}

enum LiteralLoadStep {
    Visit(u32),
    Fill(u32),
}

/// The explicit stack keeps deep and cyclic literal graphs off the Rust call stack.
#[derive(Default)]
struct LiteralWorklist {
    steps: Vec<LiteralLoadStep>,
    unfinished_pathnames: HashSet<u32>,
    tables: Vec<u32>,
}

impl LiteralWorklist {
    fn visit_children(&mut self, children: &[u32]) {
        self.steps
            .extend(children.iter().rev().copied().map(LiteralLoadStep::Visit));
    }

    fn fill_after_children(&mut self, id: u32, children: &[u32]) {
        self.steps.push(LiteralLoadStep::Fill(id));
        self.visit_children(children);
    }
}

impl CompiledFileLoader {
    pub(crate) fn new(
        runtime: &Runtime,
        compiled: CompiledFile,
        module: Option<&'static AotModuleDescriptor>,
    ) -> Self {
        let mut entries = FxHashMap::default();
        if let Some(module) = module {
            for entry in module.codes() {
                entries
                    .entry(entry.entry_index as usize)
                    .or_insert(LinkedCode::Native(entry));
            }
        }
        Self::with_entries(runtime, compiled, entries)
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    pub(crate) fn with_wasm_entries(
        runtime: &Runtime,
        compiled: CompiledFile,
        registrations: Vec<zkcl_wasm::abi::Registration>,
        call_leaf: zkcl_wasm::abi::CallLeaf,
        call_function: zkcl_wasm::abi::CallFunction,
        leaf_base: u32,
        function_base: u32,
    ) -> Self {
        use zkcl_wasm::abi::{FunctionRegistration, RegisteredEntry};

        let mut leaf_slot = leaf_base;
        let mut function_slot = function_base;
        let mut next_function = |FunctionRegistration {
                                     native_debug,
                                     root_slots,
                                     scratch_words,
                                 }| {
            let slot = function_slot;
            function_slot += 1;
            (
                crate::native::JitCEntry::Wasm {
                    slot,
                    scratch_words,
                    call: call_function,
                },
                root_slots as usize,
                native_debug,
            )
        };
        let entries = registrations
            .into_iter()
            .map(|registration| {
                let entry = match registration.entry {
                    RegisteredEntry::Leaf { guards, fallback } => {
                        let slot = leaf_slot;
                        leaf_slot += 1;
                        LinkedCode::Wasm {
                            leaf: Some((
                                crate::native::LeafEntry::Wasm {
                                    slot,
                                    call: call_leaf,
                                },
                                guards,
                            )),
                            function: fallback.map(&mut next_function),
                        }
                    }
                    RegisteredEntry::Function(registration) => LinkedCode::Wasm {
                        leaf: None,
                        function: Some(next_function(registration)),
                    },
                };
                (registration.entry_index as usize, entry)
            })
            .collect();
        Self::with_entries(runtime, compiled, entries)
    }

    fn with_entries(
        runtime: &Runtime,
        mut compiled: CompiledFile,
        mut compiled_entries: FxHashMap<usize, LinkedCode>,
    ) -> Self {
        // Cross-compilation may load wider-target FASLs to run macros on a
        // narrow host. Append any extra constants after the file's own nodes
        // so native entry indices and shared literal references stay stable.
        if compiled.target.fixnum_bounds().0 < Word::FIXNUM_MIN as i64 {
            let mut extra = Vec::new();
            let base = compiled.nodes.len();
            let nested = compiled.nodes.iter_mut().filter_map(|node| match node {
                LiteralNode::Function(code) => Some(code),
                _ => None,
            });
            for code in compiled.entries.iter_mut().chain(nested) {
                let Ok(()) = super::spill_wide_fixnums(
                    code,
                    (Word::FIXNUM_MIN as i64, Word::FIXNUM_MAX as i64),
                    |value| {
                        // This branch only runs on 32-bit hosts, where every node
                        // index fits in u32.
                        let id = (base + extra.len()) as u32;
                        extra.push(LiteralNode::Fixnum(value));
                        Ok::<_, std::convert::Infallible>(id)
                    },
                );
            }
            compiled.nodes.extend(extra);
        }
        let mut compiled_functions = FxHashMap::default();
        if !compiled_entries.is_empty() {
            let top_level_count = compiled.entries.len();
            for (offset, (node_id, _)) in compiled.nested_function_nodes().enumerate() {
                if let Some(entry) = compiled_entries.remove(&(top_level_count + offset)) {
                    compiled_functions.insert(node_id, entry);
                }
            }
        }
        // UNBOUND marks literal nodes that have not been loaded. It is never a FASL literal.
        let literals = runtime.root_frame(compiled.nodes.len());
        let mut canonical_symbols = Vec::with_capacity(compiled.nodes.len());
        let mut load_form_entries = FxHashMap::default();
        let mut image_only = false;
        for (index, node) in compiled.nodes.iter().enumerate() {
            literals.set(index, Word::UNBOUND);
            // Function literals repeatedly refer to the same symbol nodes.
            canonical_symbols.push(canonical_symbol_for_node(node));
            match node {
                LiteralNode::LoadForm {
                    creation,
                    initialization,
                } => {
                    load_form_entries.insert(*creation, Some(index as u32));
                    if let Some(initialization) = initialization {
                        load_form_entries.insert(*initialization, None);
                    }
                }
                LiteralNode::Unbound
                | LiteralNode::RuntimeObject { .. }
                | LiteralNode::ExternalResource { .. } => image_only = true,
                _ => {}
            }
        }
        Self {
            image_only,
            entries: compiled.entries.into_iter().enumerate(),
            source_files: compiled
                .source_files
                .into_iter()
                .map(|source| {
                    let mut document =
                        SourceDocument::new(source.path.map(PathBuf::from), source.text)
                            .with_base_offset(source.base_offset);
                    if let Some(uri) = source.uri {
                        document = document.with_uri(uri);
                    }
                    if let Some(version) = source.version {
                        document = document.with_version(version);
                    }
                    document
                })
                .collect(),
            canonical_symbols,
            literals,
            nodes: compiled.nodes,
            work: LiteralWorklist::default(),
            load_form_entries,
            pending_created_literal: None,
            pending_equalp_tables: Vec::new(),
            compiled_entries,
            compiled_functions,
        }
    }

    pub(crate) fn next(
        &mut self,
        runtime: &Runtime,
        previous: Word,
    ) -> Result<Option<(Word, bool)>, EvalError> {
        if self.image_only {
            return Err(invalid(
                "runtime image object nodes require the image restore API",
            ));
        }
        if let Some(id) = self.pending_created_literal.take() {
            self.literals.set(id as usize, previous);
        }
        let Some((entry_index, mut entry)) = self.entries.next() else {
            self.check_equalp_keys(runtime)?;
            return Ok(None);
        };
        let mut work = mem::take(&mut self.work);
        for &id in &entry.constants {
            self.load_literal(runtime, id, &mut work)?;
        }
        self.work = work;
        let load_form = self.load_form_entries.remove(&entry_index);
        if load_form.is_none() {
            self.check_equalp_keys(runtime)?;
        }

        let literal_symbols = entry
            .constants
            .iter()
            .map(|&id| canonical_symbol_for_word(runtime, self.literals.get(id as usize)))
            .collect();
        let compiled = self.compiled_entries.remove(&entry_index);
        let code_id = install_code(
            runtime,
            &mut entry,
            &self.source_files,
            literal_symbols,
            compiled,
        )?;
        let function = runtime
            .heap
            .allocate_function(code_id, entry.constants.len(), |index| {
                self.literals.get(entry.constants[index] as usize)
            })
            .map_err(allocation_error)?;
        self.pending_created_literal = load_form.flatten();
        Ok(Some((function, load_form.is_none())))
    }

    pub(crate) fn image_roots(
        mut self,
        runtime: &Runtime,
        hooks: &dyn crate::image::ImageHooks,
    ) -> Result<Vec<crate::heap::RootSlot>, EvalError> {
        let entries = std::mem::replace(&mut self.entries, Vec::new().into_iter().enumerate())
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        if entries.len() != 2 || entries[1].constants.len() % 2 != 0 {
            return Err(invalid("invalid runtime image roots"));
        }
        // Host resources are resolved before replacing package memberships, so
        // explicitly registered host callbacks can be looked up in the candidate.
        for (id, node) in self.nodes.iter().enumerate() {
            if let LiteralNode::ExternalResource { key, data } = node {
                let value = hooks.restore_external(runtime, key, data)?;
                if !value.belongs_to(runtime) {
                    return Err(invalid(
                        "image restore hook returned a foreign runtime value",
                    ));
                }
                self.literals.set(id, value.slot.get());
            }
        }
        let mut work = LiteralWorklist::default();
        for entry in &entries {
            for &id in &entry.constants {
                self.load_literal(runtime, id, &mut work)?;
            }
        }
        for pair in entries[1].constants.chunks_exact(2) {
            runtime.heap.set_function_documentation(
                self.literals.get(pair[0] as usize),
                self.literals.get(pair[1] as usize),
            );
        }
        self.check_equalp_keys(runtime)?;
        Ok(entries[0]
            .constants
            .iter()
            .map(|&id| runtime.root_slot(self.literals.get(id as usize)))
            .collect())
    }

    fn load_literal(
        &mut self,
        runtime: &Runtime,
        id: u32,
        work: &mut LiteralWorklist,
    ) -> Result<(), EvalError> {
        if self.literals.get(id as usize) != Word::UNBOUND {
            return Ok(());
        }
        work.steps.push(LiteralLoadStep::Visit(id));
        while let Some(step) = work.steps.pop() {
            match step {
                LiteralLoadStep::Visit(id) => self.visit(runtime, id, work)?,
                LiteralLoadStep::Fill(id) => self.fill(runtime, id, work)?,
            }
        }
        // Link the containers visited by this walk before inserting table entries.
        self.fill_tables(runtime, &work.tables)?;
        work.tables.clear();
        Ok(())
    }

    fn visit(
        &mut self,
        runtime: &Runtime,
        id: u32,
        work: &mut LiteralWorklist,
    ) -> Result<(), EvalError> {
        let slot = id as usize;
        if self.literals.get(slot) != Word::UNBOUND {
            return Ok(());
        }
        let heap = &runtime.heap;
        // Publish mutable shells before visiting their children so a back edge
        // can refer to the same object. Immutable aggregates are built in fill().
        let word = match &self.nodes[slot] {
            LiteralNode::RuntimeObject { kind, data, slots } => {
                if kind == "package" {
                    if slots.len() != 0 || data.len() != 1 || data[0] > (usize::MAX >> 3) as u64 {
                        return Err(invalid("invalid image package"));
                    }
                    Word::package(data[0] as usize)
                } else if kind == "builtin-native" {
                    if slots.len() != 0 || data.len() != 1 {
                        return Err(invalid("invalid builtin native image reference"));
                    }
                    let index = usize::try_from(data[0])
                        .map_err(|_| invalid("invalid builtin native image index"))?;
                    if index >= runtime.builtin_native_count.get() {
                        return Err(invalid("invalid builtin native image index"));
                    }
                    Word::native(index)
                } else {
                    work.fill_after_children(id, slots);
                    heap.image_shell(kind, data)?
                }
            }
            LiteralNode::Closure {
                code,
                captures,
                documentation,
            } => {
                work.steps.push(LiteralLoadStep::Fill(id));
                work.visit_children(&[*documentation]);
                work.visit_children(captures);
                work.visit_children(&[*code]);
                heap.closure(captures.len(), || Word::NIL, |_| Word::NIL)
                    .map_err(allocation_error)?
            }
            LiteralNode::LexicalCell(value) => {
                work.fill_after_children(id, &[*value]);
                heap.cell(|| Word::NIL).map_err(allocation_error)?
            }
            LiteralNode::Pair(car, cdr) => {
                work.fill_after_children(id, &[*car, *cdr]);
                heap.cons(|| (Word::NIL, Word::NIL))
                    .map_err(allocation_error)?
            }
            LiteralNode::Function(code) => {
                work.fill_after_children(id, &code.constants);
                let literal_symbols = code
                    .constants
                    .iter()
                    .map(|&child| self.canonical_symbols[child as usize])
                    .collect();
                let compiled = self.compiled_functions.remove(&slot);
                let LiteralNode::Function(code) = &mut self.nodes[slot] else {
                    unreachable!("function literal");
                };
                let code_id =
                    install_code(runtime, code, &self.source_files, literal_symbols, compiled)?;
                heap.function_unfilled(code_id, code.constants.len())
                    .map_err(allocation_error)?
            }
            LiteralNode::Pathname(fields) => {
                if !work.unfinished_pathnames.insert(id) {
                    return Err(invalid("circular pathname literal"));
                }
                work.fill_after_children(id, fields);
                return Ok(());
            }
            LiteralNode::Ratio(first, second) | LiteralNode::Complex(first, second) => {
                // Validation excludes cycles through numeric components.
                work.fill_after_children(id, &[*first, *second]);
                return Ok(());
            }
            LiteralNode::HashTable {
                test,
                weakness,
                rehash_size,
                rehash_threshold,
                entries,
            } => {
                work.tables.push(id);
                for &(key, value) in entries.iter().rev() {
                    work.visit_children(&[key, value]);
                }
                heap.hash_table(
                    decode_hash_test(*test),
                    *weakness,
                    entries.len(),
                    *rehash_size,
                    *rehash_threshold,
                )
                .map_err(allocation_error)?
            }
            LiteralNode::Array {
                contents: ArrayContents::Displaced { target, .. },
                ..
            } => {
                work.fill_after_children(id, &[*target]);
                return Ok(());
            }
            LiteralNode::Array {
                dimensions,
                element,
                adjustable,
                fill_pointer,
                contents,
            } => {
                if let ArrayContents::Elements(elements) = contents
                    && *element == ArrayElement::General
                {
                    work.fill_after_children(id, elements);
                }
                heap.array_from_values(dimensions, *element, *adjustable, *fill_pointer, |index| {
                    match contents {
                        ArrayContents::Elements(_) if *element == ArrayElement::General => {
                            ArrayValue::Word(Word::NIL)
                        }
                        ArrayContents::Elements(elements) => {
                            let value = match self.nodes[elements[index] as usize] {
                                LiteralNode::Fixnum(bit) => {
                                    Word::fixnum(bit as isize).expect("validated bit")
                                }
                                LiteralNode::Character(value) => Word::character(value),
                                _ => unreachable!("validated specialized array element"),
                            };
                            ArrayValue::Word(value)
                        }
                        ArrayContents::Octets(bytes) => ArrayValue::Octet(bytes[index]),
                        ArrayContents::SingleFloats(bits) => {
                            ArrayValue::SingleFloat(f32::from_bits(bits[index]))
                        }
                        ArrayContents::DoubleFloats(bits) => {
                            ArrayValue::DoubleFloat(f64::from_bits(bits[index]))
                        }
                        ArrayContents::Displaced { .. } => unreachable!("deferred array view"),
                    }
                })
                .map_err(allocation_error)?
            }
            node => atom(runtime, node)?,
        };
        self.literals.set(slot, word);
        Ok(())
    }

    fn fill(
        &mut self,
        runtime: &Runtime,
        id: u32,
        work: &mut LiteralWorklist,
    ) -> Result<(), EvalError> {
        let heap = &runtime.heap;
        let slot = id as usize;
        let word = |reference: u32| self.literals.get(reference as usize);
        match &self.nodes[slot] {
            LiteralNode::RuntimeObject { slots, .. } => {
                let values = slots.iter().map(|&id| word(id)).collect::<Vec<_>>();
                heap.image_fill(word(id), &values)?;
            }
            LiteralNode::Closure {
                code,
                captures,
                documentation,
            } => {
                let closure = word(id);
                heap.initialize_closure(closure, word(*code), captures.iter().map(|&id| word(id)));
                heap.set_function_documentation(closure, word(*documentation));
            }
            LiteralNode::LexicalCell(value) => {
                heap.write_cell(word(id), word(*value))
                    .expect("allocated lexical cell");
            }
            LiteralNode::Pair(car, cdr) => {
                heap.set_cons_car(word(id), word(*car));
                heap.set_cons_cdr(word(id), word(*cdr));
            }
            LiteralNode::Function(code) => {
                let function = word(id);
                for (index, &constant) in code.constants.iter().enumerate() {
                    heap.set_function_constant(function, index, word(constant));
                }
            }
            LiteralNode::Pathname(fields) => {
                if pathname::normalized_fields(runtime, fields.map(word)).is_none() {
                    return Err(invalid("invalid pathname literal components"));
                }
                let pathname = heap
                    .pathname(|| fields.map(word))
                    .map_err(allocation_error)?;
                self.literals.set(slot, pathname);
                work.unfinished_pathnames.remove(&id);
            }
            LiteralNode::Ratio(numerator, denominator) => {
                let ratio = heap
                    .allocate_ratio(|| (word(*numerator), word(*denominator)))
                    .map_err(allocation_error)?;
                self.literals.set(slot, ratio);
            }
            LiteralNode::Complex(real, imag) => {
                let complex = heap
                    .allocate_complex(|| (word(*real), word(*imag)))
                    .map_err(allocation_error)?;
                self.literals.set(slot, complex);
            }
            LiteralNode::Array {
                contents: ArrayContents::Elements(elements),
                ..
            } => {
                for (index, &element) in elements.iter().enumerate() {
                    let stored = heap
                        .set_array_slot(word(id), index, word(element))
                        .map_err(allocation_error)?;
                    debug_assert!(stored, "validated array element");
                }
            }
            LiteralNode::Array {
                dimensions,
                element,
                adjustable,
                fill_pointer,
                contents: ArrayContents::Displaced { target, offset },
            } => {
                // A target array can contain its own view and have filled it already.
                if word(id) == Word::UNBOUND {
                    let view = heap
                        .displaced_array(
                            dimensions,
                            *element,
                            *adjustable,
                            *fill_pointer,
                            *offset,
                            || word(*target),
                        )
                        .map_err(allocation_error)?;
                    self.literals.set(slot, view);
                }
            }
            _ => unreachable!("only aggregate literals have fill frames"),
        }
        Ok(())
    }

    fn fill_tables(&mut self, runtime: &Runtime, tables: &[u32]) -> Result<(), EvalError> {
        let heap = &runtime.heap;
        let word = |id: u32| self.literals.get(id as usize);
        for &id in tables {
            let LiteralNode::HashTable { test, entries, .. } = &self.nodes[id as usize] else {
                unreachable!("deferred table node");
            };
            let test = decode_hash_test(*test);
            for &(key, value) in entries {
                let key = word(key);
                let hash = equality::hash(heap, key, test);
                // EQUALP can descend into other tables that are still empty.
                // Insert all entries before comparing their completed keys.
                // Structure hashes depend only on their class and slot count.
                heap.hash_table_set(word(id), key, word(value), hash, |left, right| {
                    test != HashTest::Equalp && equality::matches(heap, left, right, test)
                })
                .map_err(allocation_error)?;
            }
            if heap.hash_table_info(word(id)).expect("table literal").count != entries.len() {
                return Err(invalid("duplicate keys in hash table literal"));
            }
            if test == HashTest::Equalp {
                self.pending_equalp_tables.push(id);
            }
        }
        Ok(())
    }

    fn check_equalp_keys(&mut self, runtime: &Runtime) -> Result<(), EvalError> {
        // Two structure keys can look EQUALP immediately after their creation
        // and acquire different slot values during MAKE-LOAD-FORM initialization.
        let heap = &runtime.heap;
        for id in self.pending_equalp_tables.drain(..) {
            let LiteralNode::HashTable { entries, .. } = &self.nodes[id as usize] else {
                unreachable!("deferred EQUALP table")
            };
            let mut keys_by_hash = HashMap::<usize, Vec<Word>>::new();
            for &(key, _) in entries {
                let key = self.literals.get(key as usize);
                let candidates = keys_by_hash
                    .entry(equality::hash(heap, key, HashTest::Equalp))
                    .or_default();
                if candidates
                    .iter()
                    .any(|&other| equality::matches(heap, key, other, HashTest::Equalp))
                {
                    return Err(invalid("duplicate keys in hash table literal"));
                }
                candidates.push(key);
            }
        }
        Ok(())
    }
}

fn install_code(
    runtime: &Runtime,
    code: &mut BytecodeFunction,
    source_files: &[SourceDocument],
    literal_symbols: Vec<Option<CanonicalSymbol>>,
    compiled: Option<LinkedCode>,
) -> Result<CodeId, EvalError> {
    let store = &runtime.heap.code_store;
    let code_id = store.insert(CodeToInstall {
        name: code.name.take(),
        source: code
            .source_file
            .map(|index| source_files[index as usize].clone()),
        debug: mem::take(&mut code.debug),
        arity: code.arity,
        lambda_list: code.lambda_list,
        registers: code.registers,
        instructions: mem::take(&mut code.instructions),
        captures: mem::take(&mut code.captures),
        notinline: mem::take(&mut code.notinline),
        vop_calls: mem::take(&mut code.vop_calls),
        tables: CodeSideTables {
            handlers: mem::take(&mut code.handlers),
            restarts: mem::take(&mut code.restarts),
            keywords: mem::take(&mut code.keywords),
            jump_tables: mem::take(&mut code.jump_tables),
            facts: mem::take(&mut code.facts),
        },
        canonical_symbols: literal_symbols,
        native: match compiled {
            Some(LinkedCode::Native(entry)) => Some(*entry),
            _ => None,
        },
    });
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    if let Some(LinkedCode::Wasm { leaf, function }) = compiled
        && !store.install_wasm(code_id, leaf, function)
    {
        return Err(invalid("invalid Wasm leaf registration"));
    }
    Ok(code_id)
}

fn decode_hash_test(test: HashTableTest) -> HashTest {
    match test {
        HashTableTest::Eq => HashTest::Eq,
        HashTableTest::Eql => HashTest::Eql,
        HashTableTest::Equal => HashTest::Equal,
        HashTableTest::Equalp => HashTest::Equalp,
    }
}

fn atom(runtime: &Runtime, node: &LiteralNode) -> Result<Word, EvalError> {
    match node {
        LiteralNode::Nil => Ok(Word::NIL),
        LiteralNode::True => Ok(Word::T),
        LiteralNode::Fixnum(value) => match isize::try_from(*value).ok().and_then(Word::fixnum) {
            Some(word) => Ok(word),
            None => runtime
                .heap
                .allocate_integer(&BigInt::from(*value))
                .map_err(allocation_error),
        },
        LiteralNode::Character(value) => Ok(Word::character(*value)),
        LiteralNode::Bignum {
            negative,
            magnitude,
        } => {
            let sign = if *negative { Sign::Minus } else { Sign::Plus };
            runtime
                .heap
                .allocate_integer(&BigInt::from_bytes_le(sign, magnitude))
                .map_err(allocation_error)
        }
        LiteralNode::SingleFloat(bits) => runtime
            .heap
            .allocate_number(&Number::Real(Real::Single(f32::from_bits(*bits))))
            .map_err(allocation_error),
        LiteralNode::DoubleFloat(bits) => runtime
            .heap
            .allocate_number(&Number::Real(Real::Double(f64::from_bits(*bits))))
            .map_err(allocation_error),
        LiteralNode::RandomState(state) => {
            let state = crate::heap::RandomState::from_words(*state)
                .expect("validated random-state literal");
            runtime
                .heap
                .allocate_random_state(state)
                .map_err(allocation_error)
        }
        LiteralNode::SimdPack(pack) => runtime
            .heap
            .allocate_simd_pack(*pack)
            .map_err(allocation_error),
        LiteralNode::SimdMask(mask) => runtime
            .heap
            .allocate_simd_mask(*mask)
            .map_err(allocation_error),
        LiteralNode::String(text) => runtime.heap.string(text).map_err(allocation_error),
        LiteralNode::InternedSymbol { package, name } => {
            let package = find_package(runtime, package)?;
            let (word, _) = runtime.intern_package(package, name, 0)?;
            if matches!(word, Word::NIL | Word::T)
                || runtime.heap.symbol_home(word) != Some(package)
            {
                return Err(invalid(format!(
                    "compiled symbol {name:?} has an incompatible home package"
                )));
            }
            Ok(word)
        }
        LiteralNode::Unbound => Ok(Word::UNBOUND),
        LiteralNode::ExternalResource { .. } => {
            Err(invalid("external image resource requires restore hooks"))
        }
        LiteralNode::UninternedSymbol(name) => runtime.fresh_symbol(name, 0).map(|(_, word)| word),
        LiteralNode::Package(name) => {
            find_package(runtime, name).map(crate::package::Package::word)
        }
        LiteralNode::Primitive(name) => Primitive::named(name)
            .ok_or_else(|| invalid(format!("unknown compiled primitive {name:?}"))),
        LiteralNode::BuiltinNative(id) => macros::compiled_native(*id)
            .ok_or_else(|| invalid(format!("unknown compiled builtin installer {id}"))),
        _ => unreachable!("aggregate literals use the work stack"),
    }
}

fn find_package(runtime: &Runtime, name: &str) -> Result<crate::package::Package, EvalError> {
    runtime.packages.borrow().named(name).ok_or_else(|| {
        EvalError::new(
            0,
            format!("compiled file references missing package {name:?}"),
        )
        .without_source_location()
        .with_condition(Class::PackageError)
    })
}

fn allocation_error(error: OutOfMemory) -> EvalError {
    EvalError::out_of_memory(0, error).without_source_location()
}

fn invalid(message: impl Into<String>) -> EvalError {
    EvalError::new(0, message)
        .without_source_location()
        .with_condition(Class::ProgramError)
}

fn canonical_symbol_for_node(node: &LiteralNode) -> Option<CanonicalSymbol> {
    let LiteralNode::InternedSymbol { package, name } = node else {
        return None;
    };
    matches!(package.as_str(), "COMMON-LISP" | "CL").then(|| CanonicalSymbol::from_cl_name(name))?
}
