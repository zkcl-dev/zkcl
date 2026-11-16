use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::{
    Arity, Callee, CaptureKind, FunctionDebug, HandlerTable, Instruction, JumpTable, KeywordTable,
    RestartTable,
};

use crate::SourceDocument;
use crate::heap::Word;
use crate::native::{
    AotCodeDescriptor, JitBackend, JitCEntry, JitCEntryFn, JitCompilationRequest, JitFloatFormat,
    JitFunction, JitNumericOperand, JitSiteHint, JitSiteKind, LeafEntry, LeafGuard, NativeEntry,
    RegisterLeaf, RegisterLeafFn,
};
use crate::vm::Primitive;
use crate::vm::ops::{CanonicalSymbol, LoweredCode, PcMapping, VmOp, lower_code};

pub(crate) type CodeId = usize;

// External bytecode can grow quickly even if the Lisp heap has few live objects.
// Use the larger of 2 MiB and half the retained bytecode size as the next allocation budget.
const MIN_CODE_COLLECTION_BYTES: usize = 2 << 20;
const NO_FEEDBACK: u32 = u32::MAX;

/// Process-local runtime feedback for an executable `VmOp` site; never serialized into FASL.
#[derive(Clone, Copy)]
pub(crate) struct StampedSiteFeedback {
    pub(crate) checked_at: usize,
    pub(crate) kind: SiteFeedback,
}

impl StampedSiteFeedback {
    const COLD: Self = Self {
        checked_at: 0,
        kind: SiteFeedback::Cold,
    };
}

#[derive(Clone, Copy)]
pub(crate) enum SiteFeedback {
    Cold,
    CallPrimitive {
        prim: Primitive,
    },
    CallLispFixed {
        gc_epoch: usize,
        code: Word,
        code_id: CodeId,
    },
    ArithPrimitive {
        prim: Primitive,
        operands: ArithOperands,
    },
    ArrayReader {
        // A site with multiple formats or a general AREF path is not specialized.
        format: Option<JitFloatFormat>,
    },
    ArrayWriter {
        format: Option<JitFloatFormat>,
    },
    SimdI32x4 {
        prim: Primitive,
        operation: zkcl_bytecode::native_abi::I32x4Operation,
    },
    Megamorphic,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithOperands {
    Pair(JitNumericOperand, JitNumericOperand),
    Polymorphic,
}

impl ArithOperands {
    pub(crate) const FIXNUMS: Self =
        Self::Pair(JitNumericOperand::Fixnum, JitNumericOperand::Fixnum);

    pub(crate) fn merge(self, observed: Self) -> Self {
        if self == observed {
            observed
        } else {
            Self::Polymorphic
        }
    }
}

/// Immutable executable representation stored outside the moving Immix heap.
pub(crate) struct VmCode {
    pub(crate) id: CodeId,
    pub(crate) name: Option<String>,
    pub(crate) source: Option<SourceDocument>,
    pub(crate) debug: FunctionDebug,
    pub(crate) arity: Arity,
    pub(crate) lambda_list: Option<u32>,
    pub(crate) registers: usize,
    pub(crate) canonical: Box<[Instruction]>,
    pub(crate) vm_ops: Box<[VmOp]>,
    pc_mapping: Option<PcMapping>,
    pub(crate) captures: Box<[CaptureKind]>,
    pub(crate) notinline: Box<[u32]>,
    pub(crate) handlers: Box<[HandlerTable]>,
    pub(crate) restarts: Box<[RestartTable]>,
    pub(crate) keywords: Box<[KeywordTable]>,
    pub(crate) jump_tables: Box<[JumpTable]>,
    pub(crate) param_facts: Box<[zkcl_bytecode::ParamFact]>,
    pub(crate) canonical_symbols: Box<[Option<CanonicalSymbol>]>,
    feedback_indices: Box<[u32]>,
    feedback: Box<[Cell<StampedSiteFeedback>]>,
    call_count: Cell<u32>,
    native: RefCell<Option<Rc<NativeCode>>>,
}

pub(crate) struct NativeCode {
    pub(crate) native_debug: bool,
    pub(crate) code_id: CodeId,
    pub(crate) entry: NativeEntry,
    pub(crate) c_entry: Option<JitCEntry>,
    pub(crate) leaf: Option<RegisterLeaf>,
    pub(crate) resumable: bool,
    jit_lease: Option<JitLease>,
}

impl NativeCode {
    pub(crate) fn fits_slice(&self) -> bool {
        self.leaf.is_some() || self.resumable
    }

    pub(crate) fn has_leaf_fallback(&self) -> bool {
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        {
            matches!(self.c_entry, Some(JitCEntry::Wasm { .. }))
        }
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        {
            // The native C adapter enters its scalar leaf or the bytecode VM.
            false
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "installation checks the native entry against the owning bytecode layout"
    )]
    fn install(
        code_id: CodeId,
        entry: NativeEntry,
        c_entry: Option<JitCEntryFn>,
        resumable: bool,
        native_debug: bool,
        leaf: Option<(RegisterLeafFn, Box<[LeafGuard]>)>,
        arity: Arity,
        constants: usize,
        jit_lease: Option<JitLease>,
    ) -> Option<Rc<Self>> {
        let leaf = match leaf {
            Some((entry, guards)) => Some(RegisterLeaf::new(
                LeafEntry::Native(entry),
                arity,
                constants,
                guards,
            )?),
            None => None,
        };
        Some(Rc::new(Self {
            code_id,
            entry,
            c_entry: c_entry.map(JitCEntry::Native),
            leaf,
            resumable,
            native_debug,
            jit_lease,
        }))
    }

    pub(crate) fn machine_code(&self) -> Option<crate::native::MachineCode> {
        let lease = self.jit_lease.as_ref()?;
        lease.backend.machine_code(lease.code_id, lease.revision)
    }
}

struct JitLease {
    backend: Arc<dyn JitBackend>,
    code_id: CodeId,
    revision: u64,
    _executable_owner: Option<Arc<dyn Send + Sync>>,
}

impl Drop for JitLease {
    fn drop(&mut self) {
        self.backend.release(self.code_id, self.revision);
    }
}

impl VmCode {
    fn allocated_bytes(&self) -> usize {
        size_of::<Self>()
            + self.name.as_ref().map_or(0, String::capacity)
            + size_of_val(self.canonical.as_ref())
            + self.debug.variables.capacity() * size_of::<zkcl_bytecode::DebugVariable>()
            + size_of_val(self.vm_ops.as_ref())
            + self.pc_mapping.as_ref().map_or(0, |map| {
                size_of_val(map.canonical_by_vm.as_ref())
                    + size_of_val(map.vm_by_canonical.as_ref())
            })
            + size_of_val(self.captures.as_ref())
            + size_of_val(self.notinline.as_ref())
            + size_of_val(self.handlers.as_ref())
            + size_of_val(self.restarts.as_ref())
            + size_of_val(self.keywords.as_ref())
            + size_of_val(self.jump_tables.as_ref())
            + size_of_val(self.param_facts.as_ref())
            + size_of_val(self.canonical_symbols.as_ref())
            + size_of_val(self.feedback_indices.as_ref())
            + size_of_val(self.feedback.as_ref())
            + self
                .handlers
                .iter()
                .map(|t| size_of_val(t.handlers.as_slice()))
                .sum::<usize>()
            + self
                .restarts
                .iter()
                .map(|t| size_of_val(t.restarts.as_slice()))
                .sum::<usize>()
            + self
                .keywords
                .iter()
                .map(|t| size_of_val(t.names.as_slice()))
                .sum::<usize>()
            + self
                .jump_tables
                .iter()
                .map(|t| size_of_val(t.targets.as_slice()))
                .sum::<usize>()
    }

    pub(crate) fn native_entry(&self) -> Option<Rc<NativeCode>> {
        self.native.borrow().clone()
    }

    #[inline(always)]
    pub(crate) fn feedback(&self, vm_pc: usize) -> &Cell<StampedSiteFeedback> {
        &self.feedback[self.feedback_indices[vm_pc] as usize]
    }

    pub(crate) fn canonical_pc(&self, vm_pc: usize) -> Option<usize> {
        match &self.pc_mapping {
            Some(mapping) => mapping.canonical_by_vm.get(vm_pc).map(|&pc| pc as usize),
            None => (vm_pc < self.vm_ops.len()).then_some(vm_pc),
        }
    }

    pub(crate) fn vm_pc(&self, canonical_pc: usize) -> Option<usize> {
        match &self.pc_mapping {
            Some(mapping) => mapping
                .vm_by_canonical
                .get(canonical_pc)
                .map(|&pc| pc as usize),
            None => (canonical_pc <= self.canonical.len()).then_some(canonical_pc),
        }
    }

    pub(crate) fn vm_target(&self, canonical_pc: usize) -> usize {
        self.vm_pc(canonical_pc)
            .expect("verified canonical PC maps to a VM PC")
    }

    pub(crate) fn bump_call_count(&self) -> u32 {
        let next = self.call_count.get().saturating_add(1);
        self.call_count.set(next);
        next
    }
}

#[derive(Default)]
struct JitState {
    backend: Option<Arc<dyn JitBackend>>,
    pending: FxHashMap<CodeId, JitLease>,
    finished: FxHashSet<CodeId>,
}

pub(crate) struct CodeStore {
    entries: RefCell<FxHashMap<CodeId, Rc<VmCode>>>,
    next_id: Cell<CodeId>,
    allocated_bytes: Cell<usize>,
    bytes_since_collection: Cell<usize>,
    collection_threshold: Cell<usize>,
    jit: RefCell<JitState>,
    native_epoch: Cell<usize>,
}

static NEXT_NATIVE_EPOCH: AtomicUsize = AtomicUsize::new(1);
// Code IDs are runtime-local. JIT revisions identify requests across runtimes.
static NEXT_JIT_REVISION: AtomicU64 = AtomicU64::new(1);

impl Default for CodeStore {
    fn default() -> Self {
        Self {
            entries: RefCell::default(),
            next_id: Cell::new(0),
            allocated_bytes: Cell::new(0),
            bytes_since_collection: Cell::new(0),
            collection_threshold: Cell::new(MIN_CODE_COLLECTION_BYTES),
            jit: RefCell::default(),
            native_epoch: Cell::new(NEXT_NATIVE_EPOCH.fetch_add(1, Ordering::Relaxed)),
        }
    }
}

pub(crate) struct CodeSideTables {
    pub(crate) handlers: Vec<HandlerTable>,
    pub(crate) restarts: Vec<RestartTable>,
    pub(crate) keywords: Vec<KeywordTable>,
    pub(crate) jump_tables: Vec<JumpTable>,
    pub(crate) facts: zkcl_bytecode::FunctionFacts,
}

/// Bytecode and metadata used to create an executable code-store entry.
pub(crate) struct CodeToInstall {
    pub(crate) name: Option<String>,
    pub(crate) source: Option<SourceDocument>,
    pub(crate) debug: FunctionDebug,
    pub(crate) arity: Arity,
    pub(crate) lambda_list: Option<u32>,
    pub(crate) registers: usize,
    pub(crate) instructions: Vec<Instruction>,
    pub(crate) captures: Vec<CaptureKind>,
    pub(crate) notinline: Vec<u32>,
    pub(crate) tables: CodeSideTables,
    pub(crate) canonical_symbols: Vec<Option<CanonicalSymbol>>,
    pub(crate) native: Option<AotCodeDescriptor>,
}

impl CodeStore {
    pub(crate) fn insert(&self, input: CodeToInstall) -> CodeId {
        let LoweredCode { vm_ops, pc_mapping } = lower_code(
            &input.instructions,
            input.registers,
            input.captures.len(),
            &input.canonical_symbols,
            &input.tables.handlers,
            &input.tables.restarts,
            &input.tables.jump_tables,
            input.debug.retain_frame,
        );
        // VM PCs give direct access to compact feedback for named calls, including
        // fused instructions whose canonical call lies inside the fused span.
        let mut feedback_indices = Vec::new();
        let mut feedback_len = 0;
        for (pc, instruction) in input.instructions.iter().enumerate() {
            if matches!(
                instruction,
                Instruction::Call {
                    callee: Callee::Named(_),
                    ..
                }
            ) {
                let vm_pc = pc_mapping
                    .as_ref()
                    .map_or(pc, |map| map.vm_by_canonical[pc] as usize);
                debug_assert!(vm_pc >= feedback_indices.len());
                feedback_indices.resize(vm_pc + 1, NO_FEEDBACK);
                feedback_indices[vm_pc] = feedback_len;
                feedback_len += 1;
            }
        }
        let id = self.next_id.get();
        // Background requests and feedback may outlive the bytecode they describe.
        self.next_id
            .set(id.checked_add(1).expect("code ID space exhausted"));
        let constants = input.canonical_symbols.len();
        let code = Rc::new(VmCode {
            id,
            name: input.name,
            source: input.source,
            debug: input.debug,
            arity: input.arity,
            lambda_list: input.lambda_list,
            registers: input.registers,
            canonical: input.instructions.into_boxed_slice(),
            vm_ops,
            pc_mapping,
            captures: input.captures.into_boxed_slice(),
            notinline: input.notinline.into_boxed_slice(),
            handlers: input.tables.handlers.into_boxed_slice(),
            restarts: input.tables.restarts.into_boxed_slice(),
            keywords: input.tables.keywords.into_boxed_slice(),
            jump_tables: input.tables.jump_tables.into_boxed_slice(),
            param_facts: input.tables.facts.params.into_boxed_slice(),
            canonical_symbols: input.canonical_symbols.into_boxed_slice(),
            feedback_indices: feedback_indices.into_boxed_slice(),
            feedback: vec![Cell::new(StampedSiteFeedback::COLD); feedback_len as usize]
                .into_boxed_slice(),
            call_count: Cell::new(0),
            native: RefCell::new(
                input
                    .native
                    .filter(|descriptor| descriptor.arity == input.arity)
                    .and_then(|descriptor| {
                        NativeCode::install(
                            id,
                            descriptor.to_native_entry(),
                            descriptor.c_entry,
                            descriptor.continuation_abi.is_some(),
                            descriptor.native_debug,
                            descriptor
                                .leaf_entry
                                .map(|entry| (entry, descriptor.leaf_guards.into())),
                            input.arity,
                            constants,
                            None,
                        )
                    }),
            ),
        });
        let bytes = code.allocated_bytes();
        self.allocated_bytes
            .update(|total| total.saturating_add(bytes));
        self.bytes_since_collection
            .update(|total| total.saturating_add(bytes));
        self.entries.borrow_mut().insert(id, code);
        id
    }

    pub(crate) fn get(&self, id: CodeId) -> Rc<VmCode> {
        self.find(id).expect("live function has VM code")
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    pub(crate) fn install_wasm(
        &self,
        id: CodeId,
        leaf: Option<(LeafEntry, Box<[LeafGuard]>)>,
        function: Option<(JitCEntry, usize, bool)>,
    ) -> bool {
        let code = self.get(id);
        let leaf = match leaf {
            Some((entry, guards)) => {
                let Some(leaf) =
                    RegisterLeaf::new(entry, code.arity, code.canonical_symbols.len(), guards)
                else {
                    return false;
                };
                Some(leaf)
            }
            None => None,
        };
        code.native.replace(Some(Rc::new(NativeCode {
            code_id: id,
            entry: NativeEntry::from_jit_c(function.map_or(0, |(_, roots, _)| roots), 0),
            c_entry: function.map(|(entry, _, _)| entry),
            leaf,
            resumable: function.is_some(),
            native_debug: function.is_some_and(|(_, _, debug)| debug),
            jit_lease: None,
        })));
        true
    }

    pub(crate) fn find(&self, id: CodeId) -> Option<Rc<VmCode>> {
        self.entries.borrow().get(&id).cloned()
    }

    pub(crate) fn should_collect(&self) -> bool {
        self.bytes_since_collection.get() >= self.collection_threshold.get()
    }

    pub(crate) fn sweep(&self, live: &FxHashSet<CodeId>) {
        let dead = {
            let mut dead_bytes = 0usize;
            let mut entries = self.entries.borrow_mut();
            let dead: Vec<_> = entries
                .extract_if(|id, code| {
                    // Frames, cached templates, and function allocations hold an Rc.
                    if live.contains(id) || Rc::strong_count(code) != 1 {
                        return false;
                    }
                    dead_bytes = dead_bytes.saturating_add(code.allocated_bytes());
                    true
                })
                .collect();
            let retained_capacity = entries.len().saturating_mul(2).max(128);
            if entries.capacity() > retained_capacity.saturating_mul(2) {
                entries.shrink_to(retained_capacity);
            }
            self.bytes_since_collection.set(0);
            // The measured bytecode and side tables are immutable after insertion.
            let live_bytes = self.allocated_bytes.get().saturating_sub(dead_bytes);
            self.allocated_bytes.set(live_bytes);
            self.collection_threshold
                .set(MIN_CODE_COLLECTION_BYTES.max(live_bytes / 2));
            dead
        };
        let submitted: Vec<_> = {
            let mut jit = self.jit.borrow_mut();
            dead.iter()
                .filter_map(|(id, _)| {
                    jit.finished.remove(id);
                    jit.pending.remove(id)
                })
                .collect()
        };
        // Release the backend's executable allocations and outstanding work outside store borrows.
        drop(submitted);
        drop(dead);
    }

    pub(crate) fn has_jit_backend(&self) -> bool {
        self.jit.borrow().backend.is_some()
    }

    pub(crate) fn native_epoch(&self) -> usize {
        self.native_epoch.get()
    }

    pub(crate) fn invalidate_native_calls(&self) {
        self.native_epoch
            .set(NEXT_NATIVE_EPOCH.fetch_add(1, Ordering::Relaxed));
    }

    pub(crate) fn set_jit_backend(&self, backend: Arc<dyn JitBackend>) {
        let (previous, submitted) = {
            let mut jit = self.jit.borrow_mut();
            if jit
                .backend
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &backend))
            {
                return;
            }
            jit.finished.clear();
            (
                jit.backend.replace(backend),
                std::mem::take(&mut jit.pending),
            )
        };
        drop(submitted);
        drop(previous);
    }

    pub(crate) fn submit_jit(&self, code_id: CodeId, explicit: bool, func_epoch: usize) -> bool {
        let backend = {
            let jit = self.jit.borrow();
            if jit.finished.contains(&code_id) {
                return false;
            }
            if let Some(lease) = jit.pending.get(&code_id) {
                if explicit {
                    let backend = Arc::clone(&lease.backend);
                    let revision = lease.revision;
                    drop(jit);
                    backend.prioritize_explicit(code_id, revision);
                }
                // The existing lease still covers the request.
                return false;
            }
            let Some(backend) = jit.backend.clone() else {
                return false;
            };
            backend
        };
        if !explicit && !backend.can_submit_automatic() {
            return false;
        }
        let Some(code) = self.find(code_id) else {
            return false;
        };
        let revision = NEXT_JIT_REVISION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |revision| {
                revision.checked_add(1)
            })
            .expect("JIT revision space exhausted");
        let request = Self::snapshot_for_jit(&code, code_id, revision, func_epoch);
        // SAFETY: The request is an unchanged snapshot of installed bytecode with a fresh revision.
        let accepted = unsafe {
            if explicit {
                backend.submit_explicit(request)
            } else {
                backend.try_submit(request)
            }
        };
        if !accepted {
            return false;
        }
        let lease = JitLease {
            backend,
            code_id,
            revision,
            _executable_owner: None,
        };
        let mut jit = self.jit.borrow_mut();
        if jit
            .backend
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, &lease.backend))
            && !jit.finished.contains(&code_id)
            && let std::collections::hash_map::Entry::Vacant(entry) = jit.pending.entry(code_id)
        {
            entry.insert(lease);
            true
        } else {
            false
        }
    }

    fn take_submission(
        &self,
        code_id: CodeId,
        revision: u64,
        backend: &Arc<dyn JitBackend>,
    ) -> Option<JitLease> {
        let mut jit = self.jit.borrow_mut();
        let lease = jit.pending.get(&code_id)?;
        if lease.revision != revision || !Arc::ptr_eq(&lease.backend, backend) {
            return None;
        }
        jit.finished.insert(code_id);
        jit.pending.remove(&code_id)
    }

    pub(crate) fn poll_jit_completions(&self) -> usize {
        let backend = {
            let jit = self.jit.borrow();
            if jit.pending.is_empty() {
                return 0;
            }
            let backend = jit
                .backend
                .as_ref()
                .expect("pending requests have a backend");
            if !backend.has_completions() {
                return 0;
            }
            Arc::clone(backend)
        };
        let mut installed = 0;
        let Some((completions, failures)) = backend.poll_for(&mut |code_id, revision| {
            let jit = self.jit.borrow();
            jit.pending.get(&code_id).is_some_and(|lease| {
                lease.revision == revision && Arc::ptr_eq(&lease.backend, &backend)
            })
        }) else {
            return 0;
        };
        for (id, revision) in failures {
            drop(self.take_submission(id, revision, &backend));
        }
        for JitFunction {
            native_debug,
            code_id,
            entry,
            c_entry,
            resumable,
            leaf_entry,
            leaf_guards,
            executable_owner,
        } in completions
        {
            let Some(mut lease) = self.take_submission(code_id, entry.revision, &backend) else {
                continue;
            };
            let code = self.get(code_id);
            if code
                .native
                .borrow()
                .as_ref()
                .is_some_and(|code| entry.revision <= code.entry.revision)
            {
                continue;
            }
            let entry = if c_entry.is_some() {
                NativeEntry::from_jit_c(entry.root_slots, entry.revision)
            } else {
                entry
            };
            lease._executable_owner = executable_owner;
            let Some(executable) = NativeCode::install(
                code_id,
                entry,
                c_entry,
                resumable,
                native_debug,
                leaf_entry.map(|entry| (entry, leaf_guards)),
                code.arity,
                code.canonical_symbols.len(),
                Some(lease),
            ) else {
                continue;
            };
            self.invalidate_native_calls();
            drop(code.native.replace(Some(executable)));
            installed += 1;
        }
        installed
    }

    fn snapshot_for_jit(
        code: &VmCode,
        id: CodeId,
        revision: u64,
        func_epoch: usize,
    ) -> JitCompilationRequest {
        let mut site_feedback = Vec::new();
        for (vm_pc, &index) in code.feedback_indices.iter().enumerate() {
            if index == NO_FEEDBACK {
                continue;
            }
            let observed = code.feedback[index as usize].get();
            let kind = match observed.kind {
                SiteFeedback::Cold => continue,
                // These observations select guarded specializations. Native code checks
                // the current binding, operand types, and array layout before using them.
                // The request does not resolve a function object's constant pool, so
                // use previous observations as hints and let the native guards decide.
                SiteFeedback::CallPrimitive { .. } => JitSiteKind::CallPrimitive,
                SiteFeedback::ArithPrimitive { operands, .. } => match operands {
                    ArithOperands::FIXNUMS => JitSiteKind::KnownFixnumArith,
                    ArithOperands::Pair(lhs, rhs) => JitSiteKind::KnownFloatArith { lhs, rhs },
                    ArithOperands::Polymorphic => JitSiteKind::PolymorphicArithmetic,
                },
                SiteFeedback::ArrayReader { format, .. } => match format {
                    Some(format) => JitSiteKind::FloatArrayReader { format },
                    None => JitSiteKind::CallPrimitive,
                },
                SiteFeedback::ArrayWriter {
                    format: Some(format),
                    ..
                } => JitSiteKind::FloatArrayWriter { format },
                SiteFeedback::SimdI32x4 { operation, .. } => JitSiteKind::SimdI32x4 { operation },
                SiteFeedback::Megamorphic if observed.checked_at == func_epoch => {
                    JitSiteKind::Megamorphic
                }
                _ => continue,
            };
            site_feedback.push(JitSiteHint {
                canonical_pc: code
                    .canonical_pc(vm_pc)
                    .expect("feedback belongs to a VM instruction"),
                kind,
            });
        }
        JitCompilationRequest {
            native_debug: code.debug.retain_frame,
            code_id: id,
            revision,
            arity: code.arity,
            registers: code.registers,
            canonical: code.canonical.clone(),
            captures: code.captures.clone(),
            notinline: code.notinline.clone(),
            handlers: code.handlers.clone(),
            restarts: code.restarts.clone(),
            keywords: code.keywords.clone(),
            jump_tables: code.jump_tables.clone(),
            param_facts: code.param_facts.clone(),
            site_feedback,
            cl_const_names: code
                .canonical_symbols
                .iter()
                .map(|symbol| symbol.and_then(canonical_cl_name))
                .collect(),
        }
    }
}

fn canonical_cl_name(prim: CanonicalSymbol) -> Option<&'static str> {
    Some(match prim {
        CanonicalSymbol::Add => "+",
        CanonicalSymbol::Aref => "AREF",
        CanonicalSymbol::Sub => "-",
        CanonicalSymbol::Mul => "*",
        CanonicalSymbol::Div => "/",
        CanonicalSymbol::Inc => "1+",
        CanonicalSymbol::Dec => "1-",
        CanonicalSymbol::Funcall => "FUNCALL",
        CanonicalSymbol::EqNum => "=",
        CanonicalSymbol::NeNum => "/=",
        CanonicalSymbol::Lt => "<",
        CanonicalSymbol::Le => "<=",
        CanonicalSymbol::Gt => ">",
        CanonicalSymbol::Ge => ">=",
        CanonicalSymbol::Eq => "EQ",
        CanonicalSymbol::Eql => "EQL",
        CanonicalSymbol::Car => "CAR",
        CanonicalSymbol::Cdr => "CDR",
        CanonicalSymbol::Cons => "CONS",
        CanonicalSymbol::Null => "NULL",
        CanonicalSymbol::Typep => "TYPEP",
        CanonicalSymbol::SingleFloat => "SINGLE-FLOAT",
        CanonicalSymbol::DoubleFloat => "DOUBLE-FLOAT",
        CanonicalSymbol::IntegerBits(op) => op.name(),
    })
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::sync::{Arc, Mutex};

    use super::{CodeStore, NO_FEEDBACK};
    use crate::heap::{Heap, RootSlots, Word};
    use crate::native::{
        Fault, JitBackend, JitCompilationRequest, JitFunction, JitNumericOperand, JitSiteKind,
        NativeContext, NativeEntry, NativeReturn,
    };
    use crate::vm::ops::VmOp;
    use crate::vm::{ConstantTemplate, FunctionTemplate};
    use crate::{Name, Root, Runtime};
    use zkcl_bytecode::{Arity, Callee, Immediate, Instruction};

    fn snapshot(runtime: &Runtime, function: &Root<'_>) -> JitCompilationRequest {
        let id = runtime
            .heap
            .function_info(function.slot.get())
            .unwrap()
            .code_id;
        let code = runtime.heap.code_store.get(id);
        CodeStore::snapshot_for_jit(&code, id, 1, runtime.heap.function_epoch())
    }

    fn eval<'rt>(runtime: &'rt Runtime, source: &str) -> Root<'rt> {
        runtime.eval(source).unwrap().pop().unwrap()
    }

    fn apply(runtime: &Runtime, function: &Root<'_>, arguments: &[&str]) {
        let arguments = arguments
            .iter()
            .map(|source| eval(runtime, source))
            .collect::<Vec<_>>();
        let refs = arguments.iter().collect::<Vec<_>>();
        runtime.call(function, &refs).unwrap();
    }

    fn numeric_hint(request: &JitCompilationRequest, op: &str) -> Option<JitSiteKind> {
        request.site_feedback.iter().find_map(|hint| {
            let Instruction::Call {
                callee: Callee::Named(sym),
                ..
            } = &request.canonical[hint.canonical_pc]
            else {
                return None;
            };
            (request.cl_const_names[*sym] == Some(op)).then_some(hint.kind)
        })
    }

    #[test]
    fn code_byte_accounting_tracks_retained_and_reclaimed_functions() {
        let runtime = Runtime::start().unwrap();
        let retained = eval(&runtime, "(lambda (x) (1+ x))");
        let discarded = eval(&runtime, "(lambda (x) (1- x))");
        let retained_id = runtime
            .heap
            .function_info(retained.slot.get())
            .unwrap()
            .code_id;
        let discarded_id = runtime
            .heap
            .function_info(discarded.slot.get())
            .unwrap()
            .code_id;
        let store = &runtime.heap.code_store;
        let check_bytes = || {
            let entries = store.entries.borrow();
            assert_eq!(
                store.allocated_bytes.get(),
                entries
                    .values()
                    .map(|code| code.allocated_bytes())
                    .sum::<usize>()
            );
        };
        check_bytes();
        drop(discarded);
        runtime.collect();
        assert!(store.find(retained_id).is_some());
        assert!(store.find(discarded_id).is_none());
        check_bytes();
        runtime.collect();
        check_bytes();
    }

    #[test]
    fn feedback_is_allocated_only_for_named_calls() {
        let runtime = Runtime::start().unwrap();
        let installed = |function: &Root<'_>| {
            let info = runtime.heap.function_info(function.slot.get()).unwrap();
            runtime.heap.code_store.get(info.code_id)
        };
        let identity = eval(&runtime, "(lambda (x) x)");
        assert!(installed(&identity).feedback.is_empty());
        assert!(snapshot(&runtime, &identity).site_feedback.is_empty());

        let function = FunctionTemplate {
            name: None,
            debug: Default::default(),
            instructions: vec![
                Instruction::Load(2, Immediate::Nil),
                Instruction::Load(1, Immediate::Fixnum(1)),
                Instruction::Call {
                    callee: Callee::Named(0),
                    out: Some(1),
                    args: 0..2,
                    at: 0,
                    associate: false,
                    notinline: false,
                },
                Instruction::SetValues(1..2),
                Instruction::Clear(2..3),
            ],
            constants: vec![ConstantTemplate::Symbol(Name::common_lisp("+"), 0)],
            lambda_list: None,
            registers: 3,
            arity: Arity {
                required: 1,
                maximum: Some(1),
            },
            captures: vec![],
            notinline: vec![],
            vop_calls: Vec::new(),
            handlers: vec![],
            restarts: vec![],
            keywords: vec![],
            jump_tables: vec![],
            facts: Default::default(),
        }
        .instantiate(&runtime)
        .unwrap();
        let function = runtime.root(function);
        let code = installed(&function);
        let vm_pc = code.vm_target(2);
        assert_eq!(code.feedback.len(), 1);
        assert_eq!(code.feedback_indices.len(), vm_pc + 1);
        assert!(
            code.feedback_indices[..vm_pc]
                .iter()
                .all(|&i| i == NO_FEEDBACK)
        );
        if !cfg!(feature = "vm-no-spec") {
            assert!(matches!(code.vm_ops[vm_pc], VmOp::AddFixnumImm { .. }));
        }
        let argument = runtime.fixnum(41).unwrap();
        let values = runtime.call(&function, &[&argument]).unwrap();
        assert_eq!(values[0].as_fixnum(), Some(42));
        let request = snapshot(&runtime, &function);
        assert_eq!(request.site_feedback.len(), 1);
        assert_eq!(request.site_feedback[0].canonical_pc, 2);
        assert_eq!(request.site_feedback[0].kind, JitSiteKind::KnownFixnumArith);
    }

    #[test]
    fn arithmetic_feedback_uses_observed_operand_formats_in_canonical_order() {
        use JitNumericOperand::{F32, F64, Fixnum};
        use JitSiteKind::{KnownFixnumArith, KnownFloatArith, PolymorphicArithmetic};

        let runtime = Runtime::start().unwrap();
        let add = eval(&runtime, "(lambda (a b) (cons (+ a b) nil))");
        assert_eq!(numeric_hint(&snapshot(&runtime, &add), "+"), None);
        apply(&runtime, &add, &["1", "2"]);
        assert_eq!(
            numeric_hint(&snapshot(&runtime, &add), "+"),
            Some(KnownFixnumArith)
        );
        apply(&runtime, &add, &["1.0", "2.0"]);
        apply(&runtime, &add, &["1", "2"]);
        assert_eq!(
            numeric_hint(&snapshot(&runtime, &add), "+"),
            Some(PolymorphicArithmetic)
        );

        for (src, op, inputs) in [
            ("(lambda (a b) (+ a b))", "+", &["1", "2"][..]),
            ("(lambda (a) (cons (* a 2) nil))", "*", &["2"][..]),
            ("(lambda (a b) (if (< a b) 1 0))", "<", &["1", "2"][..]),
        ] {
            let function = eval(&runtime, src);
            apply(&runtime, &function, inputs);
            assert_eq!(
                numeric_hint(&snapshot(&runtime, &function), op),
                Some(KnownFixnumArith),
                "{src}"
            );
        }

        for (src, op, inputs, lhs, rhs) in [
            (
                "(lambda (a b) (cons (+ a b) nil))",
                "+",
                &["1.0", "2.0"][..],
                F32,
                F32,
            ),
            ("(lambda (a b) (/ a b))", "/", &["1d0", "2d0"][..], F64, F64),
            (
                "(lambda (a b) (if (< a b) 1 0))",
                "<",
                &["1d0", "2.0"][..],
                F64,
                F32,
            ),
            (
                "(lambda (a) (cons (* 2 a) nil))",
                "*",
                &["1.0"][..],
                Fixnum,
                F32,
            ),
            (
                "(lambda (a) (cons (* a 2) nil))",
                "*",
                &["1d0"][..],
                F64,
                Fixnum,
            ),
        ] {
            let function = eval(&runtime, src);
            for _ in 0..2 {
                apply(&runtime, &function, inputs);
                assert_eq!(
                    numeric_hint(&snapshot(&runtime, &function), op),
                    Some(KnownFloatArith { lhs, rhs }),
                    "{src}"
                );
            }
        }
    }

    #[test]
    fn unsupported_numeric_calls_produce_polymorphic_feedback() {
        use JitSiteKind::PolymorphicArithmetic;

        let runtime = Runtime::start().unwrap();
        let large_fixnum = Word::FIXNUM_MAX.to_string();
        for (src, op, arguments) in [
            (
                "(lambda (a b) (cons (+ a b) nil))",
                "+",
                &[large_fixnum.as_str(), "1"][..],
            ),
            (
                "(lambda (a b) (+ a b))",
                "+",
                &[large_fixnum.as_str(), "1"][..],
            ),
            ("(lambda (a b) (/ a b))", "/", &["1", "2"][..]),
            ("(lambda (a b) (if (< a b) 1 0))", "<", &["1/3", "2.0"][..]),
        ] {
            let function = eval(&runtime, src);
            apply(&runtime, &function, arguments);
            assert_eq!(
                numeric_hint(&snapshot(&runtime, &function), op),
                Some(PolymorphicArithmetic),
                "{src}"
            );
        }
    }

    #[test]
    fn arithmetic_samples_restart_after_a_binding_change() {
        use JitNumericOperand::{F32, F64};
        use JitSiteKind::KnownFloatArith;

        let runtime = Runtime::start().unwrap();
        eval(&runtime, "(setf (fdefinition 'arith-alias) #'+)");
        let function = eval(&runtime, "(lambda (a b) (arith-alias a b))");
        apply(&runtime, &function, &["1.0", "2.0"]);
        assert_eq!(
            snapshot(&runtime, &function).site_feedback[0].kind,
            KnownFloatArith { lhs: F32, rhs: F32 }
        );
        runtime.collect();
        apply(&runtime, &function, &["3.0", "4.0"]);
        assert_eq!(
            snapshot(&runtime, &function).site_feedback[0].kind,
            KnownFloatArith { lhs: F32, rhs: F32 }
        );

        eval(&runtime, "(setf (fdefinition 'arith-alias) #'/)");
        assert_eq!(
            snapshot(&runtime, &function).site_feedback[0].kind,
            KnownFloatArith { lhs: F32, rhs: F32 }
        );
        apply(&runtime, &function, &["1d0", "2d0"]);
        assert_eq!(
            snapshot(&runtime, &function).site_feedback[0].kind,
            KnownFloatArith { lhs: F64, rhs: F64 }
        );
    }

    #[test]
    fn unrelated_definitions_preserve_guarded_jit_observations() {
        use JitNumericOperand::F64;
        use JitSiteKind::{FloatArrayReader, KnownFloatArith};

        let runtime = Runtime::start().unwrap();
        let function = eval(&runtime, "(lambda (a i x) (+ (aref a i) x))");
        apply(
            &runtime,
            &function,
            &[
                "(make-array 1 :element-type 'double-float :initial-element 1d0)",
                "0",
                "2d0",
            ],
        );
        let before = snapshot(&runtime, &function).site_feedback;
        assert!(
            before
                .iter()
                .any(|hint| matches!(hint.kind, FloatArrayReader { .. }))
        );
        assert!(
            before
                .iter()
                .any(|hint| hint.kind == KnownFloatArith { lhs: F64, rhs: F64 })
        );
        eval(&runtime, "(defun unrelated-jit-definition (x) x)");
        assert_eq!(snapshot(&runtime, &function).site_feedback, before);
    }

    #[test]
    fn unrelated_redefinitions_preserve_running_vm_profiles() {
        use JitSiteKind::PolymorphicArithmetic;

        let runtime = Runtime::start().unwrap();
        eval(&runtime, "(setf (fdefinition 'observed-add) #'+)");
        for (src, first, second) in [
            (
                "(lambda (x y) (observed-add x y))",
                &["1.0", "2.0"][..],
                &["1d0", "2d0"][..],
            ),
            ("(lambda (x) (cons (+ x 2) nil))", &["1.0"], &["1d0"]),
            (
                "(lambda (x y) (if (< x y) 1 0))",
                &["1.0", "2.0"],
                &["1d0", "2d0"],
            ),
        ] {
            let function = eval(&runtime, src);
            apply(&runtime, &function, first);
            eval(&runtime, "(defun unrelated-to-profile () 13)");
            runtime.collect();
            apply(&runtime, &function, second);
            assert!(
                snapshot(&runtime, &function)
                    .site_feedback
                    .iter()
                    .any(|hint| hint.kind == PolymorphicArithmetic),
                "{src}"
            );
        }
    }

    #[test]
    fn cached_calls_refresh_after_reentry_and_moving_collection() {
        let mut config = Heap::builder_with_heap_size(16 << 20);
        config.always_defrag = true;
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        runtime
            .register_function("replace-binding", |runtime, _| {
                runtime.load_str("(defun site-target (x) (+ x 20))")?;
                runtime.collect();
                Ok(vec![runtime.nil()])
            })
            .unwrap();
        runtime
            .load_str(
                "(defun site-target (x) (+ x 10))
                 (defun site-caller (x edit)
                   (declare (notinline site-target))
                   (let ((before (site-target x)))
                     (when edit (replace-binding))
                     (values before (site-target x))))",
            )
            .unwrap();
        let saved = eval(&runtime, "#'site-target");
        let caller = eval(&runtime, "#'site-caller");
        let x = runtime.fixnum(1).unwrap();
        let read = |edit: &str| {
            let edit = eval(&runtime, edit);
            runtime
                .call(&caller, &[&x, &edit])
                .unwrap()
                .iter()
                .map(|result| result.as_fixnum().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(read("nil"), [11, 11]);
        let relocated = runtime.heap.relocations();
        assert_eq!(read("t"), [11, 21]);
        assert!(runtime.heap.relocations() > relocated);
        assert_eq!(read("nil"), [21, 21]);
        assert_eq!(
            runtime.call(&saved, &[&x]).unwrap()[0].as_fixnum(),
            Some(11)
        );

        eval(
            &runtime,
            "(setf (macro-function 'site-target) (lambda (form env) (declare (ignore form env)) nil))",
        );
        let Err(error) = runtime.call(&caller, &[&x, &runtime.nil()]) else {
            panic!("a function call must not invoke a macro expander");
        };
        assert_eq!(
            error.kind(),
            crate::EvalErrorKind::LispCondition(crate::condition::Class::UndefinedFunction)
        );
    }

    #[test]
    fn tracing_a_fused_comparison_reports_each_call_once() {
        let runtime = Runtime::start().unwrap();
        runtime
            .load_str(
                r#"(defun choose-order (a b) (if (< a b) :before :after))
             (assert (eq (choose-order 1f0 2d0) :before))
             (let ((*trace-output* (make-string-output-stream)))
               (trace <)
               (assert (eq (choose-order 1f0 2d0) :before))
               (assert (eq (choose-order 2d0 1f0) :after))
               (assert (eq (choose-order 1 2) :before))
               (assert (handler-case (choose-order 1f0 :invalid)
                         (type-error () t)))
               (untrace <)
               (let* ((output (get-output-stream-string *trace-output*))
                      (entry "TRACE <: enter"))
                 (dotimes (i 4)
                   (let ((pos (search entry output)))
                     (assert pos)
                     (setq output (subseq output (+ pos (length entry))))))
                 (assert (not (search entry output))))
               (assert (eq (choose-order 1d0 2d0) :before))
               (assert (string= (get-output-stream-string *trace-output*) "")))"#,
            )
            .unwrap();
    }

    #[test]
    fn fused_branches_follow_the_rebound_comparison() {
        let runtime = Runtime::start().unwrap();
        runtime
            .register_function("collect-in-comparison", |runtime, _| {
                runtime.collect();
                Ok(Vec::new())
            })
            .unwrap();
        runtime
            .load_str("(defun choose-order (a b) (if (< a b) :before :after))")
            .unwrap();
        let symbol = eval(&runtime, "'<");
        for (source, expected) in [
            (
                "(lambda (a b) (declare (ignore a b)) (collect-in-comparison) (values))",
                ":AFTER",
            ),
            (
                "(lambda (a b) (declare (ignore a b)) (values nil t))",
                ":AFTER",
            ),
            (
                "(lambda (a b) (declare (ignore a b)) (values t nil))",
                ":BEFORE",
            ),
            (
                "(lambda (a b) (declare (ignore a b)) (throw 'exit :escaped))",
                ":ESCAPED",
            ),
        ] {
            // A synthetic binding can produce values and exits that CL:< never produces.
            let replacement = eval(&runtime, source);
            runtime
                .heap
                .set_symbol_cell(symbol.slot.get(), true, replacement.slot.get());
            let result = eval(&runtime, "(catch 'exit (choose-order 1.0 2.0))");
            assert_eq!(runtime.repr(&result).unwrap(), expected);
        }
    }

    #[derive(Default)]
    struct ReadyJit(Mutex<Vec<JitFunction>>);

    fn return_native_value<'rt, 'exec>(
        _runtime: &'rt Runtime,
        _context: &mut NativeContext<'rt, 'exec>,
        _target: Word,
        _slots: &RootSlots,
        _range: Range<usize>,
    ) -> Result<NativeReturn, Fault> {
        Ok(NativeReturn::One(Word::fixnum(42).unwrap()))
    }

    // SAFETY: Each test completion preserves the request identity and returns a
    // tagged immediate from a static entry without reading arguments or heap objects.
    unsafe impl JitBackend for ReadyJit {
        unsafe fn submit(&self, request: JitCompilationRequest) {
            self.0.lock().unwrap().push(JitFunction {
                native_debug: false,
                code_id: request.code_id,
                entry: NativeEntry {
                    fast_entry: None,
                    generic_entry: return_native_value,
                    root_slots: 0,
                    revision: request.revision,
                },
                c_entry: None,
                resumable: false,
                leaf_entry: None,
                leaf_guards: Box::default(),
                executable_owner: None,
            });
        }

        fn poll_completions(&self) -> Vec<JitFunction> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    #[test]
    fn a_call_uses_the_native_entry_it_installs() {
        let runtime = Runtime::start().unwrap();
        let function = runtime.eval("(lambda () 7)").unwrap().pop().unwrap();
        let backend = Arc::new(ReadyJit::default());
        runtime.set_jit_backend(backend.clone());
        assert_eq!(
            runtime.call(&function, &[]).unwrap()[0].as_fixnum(),
            Some(7)
        );
        let owner = Arc::new(());
        let executable_owner = Arc::downgrade(&owner);
        backend.0.lock().unwrap()[0].executable_owner = Some(owner);
        assert_eq!(
            runtime.call(&function, &[]).unwrap()[0].as_fixnum(),
            Some(42)
        );
        assert!(backend.0.lock().unwrap().is_empty());
        assert!(executable_owner.upgrade().is_some());
        drop(function);
        drop(runtime);
        assert!(executable_owner.upgrade().is_none());
    }

    #[test]
    fn automatic_admission_only_submits_when_the_queue_can_accept() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        #[derive(Default)]
        struct GatedJit {
            ready: AtomicBool,
            checks: AtomicUsize,
            backend: ReadyJit,
        }

        // SAFETY: Admission tracking leaves the request and ReadyJit's completion intact.
        unsafe impl JitBackend for GatedJit {
            unsafe fn submit(&self, request: JitCompilationRequest) {
                // SAFETY: Forward the caller's submitted snapshot unchanged.
                unsafe { self.backend.submit(request) };
            }

            fn can_submit_automatic(&self) -> bool {
                self.checks.fetch_add(1, Ordering::Relaxed);
                self.ready.load(Ordering::Relaxed)
            }

            fn poll_completions(&self) -> Vec<JitFunction> {
                self.backend.poll_completions()
            }
        }

        let runtime = Runtime::start().unwrap();
        let function = eval(&runtime, "(lambda () 7)");
        let code_id = runtime
            .heap
            .function_info(function.slot.get())
            .unwrap()
            .code_id;
        let store = &runtime.heap.code_store;
        let backend = Arc::new(GatedJit::default());
        runtime.set_jit_backend(backend.clone());

        assert!(!store.submit_jit(code_id, false, runtime.heap.function_epoch()));
        assert_eq!(
            runtime.call(&function, &[]).unwrap()[0].as_fixnum(),
            Some(7)
        );
        assert_eq!(runtime.poll_jit_completions(), 0);
        assert!(backend.backend.0.lock().unwrap().is_empty());

        backend.ready.store(true, Ordering::Relaxed);
        assert_eq!(
            runtime.call(&function, &[]).unwrap()[0].as_fixnum(),
            Some(7)
        );
        assert_eq!(backend.checks.load(Ordering::Relaxed), 3);
        assert!(!store.submit_jit(code_id, false, runtime.heap.function_epoch()));
        assert_eq!(backend.checks.load(Ordering::Relaxed), 3);
        assert_eq!(runtime.poll_jit_completions(), 1);
        assert_eq!(
            runtime.call(&function, &[]).unwrap()[0].as_fixnum(),
            Some(42)
        );
    }

    #[test]
    fn replaced_bytecode_is_reclaimed_after_its_last_reference_dies() {
        let mut config = Heap::builder();
        config.always_defrag = true;
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let store = &runtime.heap.code_store;
        runtime
            .register_function("replace-active-and-collect", |runtime, _| {
                runtime.load_str("(defun replace-active () 0)")?;
                runtime.collect();
                Ok(vec![runtime.nil()])
            })
            .unwrap();
        runtime
            .load_str(
                "(defun replace-active () (replace-active-and-collect) 42)
             (defun edited () '(7 . 9))",
            )
            .unwrap();

        let code_id = |name| {
            let function = runtime.eval(name).unwrap().pop().unwrap();
            runtime
                .heap
                .function_info(function.slot.get())
                .unwrap()
                .code_id
        };
        let active_id = code_id("#'replace-active");
        assert_eq!(
            runtime.eval("(replace-active)").unwrap()[0].as_fixnum(),
            Some(42)
        );
        runtime.collect();
        assert!(store.find(active_id).is_none());

        let saved = runtime.eval("#'edited").unwrap().pop().unwrap();
        let saved_id = code_id("#'edited");
        let collected_at = runtime.heap.relocations();
        for i in 0..5_000 {
            let definition = if i % 2 == 0 {
                "(defun edited () 8)"
            } else {
                "(defun edited () 9)"
            };
            runtime.load_str(definition).unwrap();
        }
        assert!(runtime.heap.relocations() > collected_at);
        assert!(store.entries.borrow().len() < 5_000);
        assert_eq!(runtime.eval("(edited)").unwrap()[0].as_fixnum(), Some(9));
        assert_eq!(
            runtime
                .repr(&runtime.call(&saved, &[]).unwrap()[0])
                .unwrap(),
            "(7 . 9)"
        );
        assert!(store.find(saved_id).is_some());
        drop(saved);
        runtime.collect();
        assert!(store.find(saved_id).is_none());
    }
}
