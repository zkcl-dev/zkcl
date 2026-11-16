use std::ops::Range;
use std::sync::Arc;

use zkcl_bytecode::Arity;
use zkcl_bytecode::native_abi::{LEAF_ARGUMENTS, LEAF_MISS, LeafGuard, LeafPrimitive};

use super::{
    AbiFingerprint, Fault, JitCEntryFn, NativeContext, NativeReturn, Word, jit_c_trampoline,
    native_function_constant,
};
use crate::Runtime;
use crate::heap::RootSlots;
use crate::vm;

/// Generic entry point required by every native (AOT or JIT) function.
/// Reentrant Lisp calls must finish synchronously before the entry returns.
pub type NativeGenericFn = for<'rt, 'exec> fn(
    rt: &'rt Runtime,
    ctx: &mut NativeContext<'rt, 'exec>,
    target: Word,
    args: &RootSlots,
    range: Range<usize>,
) -> Result<NativeReturn, Fault>;

/// Fixed-arity fast entry point for functions with `<= 4` required arguments.
/// Uses the same synchronous call contract as [`NativeGenericFn`].
pub type NativeFastFn = for<'rt, 'exec> fn(
    rt: &'rt Runtime,
    ctx: &mut NativeContext<'rt, 'exec>,
    target: Word,
    args: &[Word],
) -> Result<NativeReturn, Fault>;

/// Internal entry for a verified fixed-arity, single-value leaf. Pass tagged
/// words as four scalar C ABI arguments, padding unused inputs with NIL.
/// The code may neither collect nor call out. All its guards must hold on entry.
/// A leaf can request bytecode execution by returning [`LEAF_MISS`].
pub type RegisterLeafFn = unsafe extern "C" fn(usize, usize, usize, usize) -> usize;

pub(crate) enum LeafEntry {
    Native(RegisterLeafFn),
    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    Wasm {
        slot: u32,
        call: zkcl_wasm::abi::CallLeaf,
    },
}

pub(crate) struct RegisterLeaf {
    entry: LeafEntry,
    argc: usize,
    guards: Box<[LeafGuard]>,
}

impl RegisterLeaf {
    pub(crate) fn new(
        entry: LeafEntry,
        arity: Arity,
        constants: usize,
        guards: Box<[LeafGuard]>,
    ) -> Option<Self> {
        let argc = arity.required;
        if argc > LEAF_ARGUMENTS
            || arity.maximum != Some(argc)
            || guards.iter().any(|guard| guard.symbol >= constants)
        {
            return None;
        }
        Some(Self {
            entry,
            argc,
            guards,
        })
    }

    /// The VM or native call site has checked the target's actual arity. No
    /// guard can collect or call Lisp, so the words stay valid through return.
    pub(crate) fn call(
        &self,
        runtime: &Runtime,
        target: Word,
        read: impl Fn(usize) -> Word,
    ) -> Option<Word> {
        if !self.check_guards(runtime, target) {
            return None;
        }
        let mut words = [Word::NIL; LEAF_ARGUMENTS];
        for (i, word) in words[..self.argc].iter_mut().enumerate() {
            *word = read(i);
        }
        let [a, b, c, d] = words.map(Word::raw);
        self.invoke(a, b, c, d)
    }

    /// Arguments arrive padded with NIL at native call sites of arity at most two.
    #[inline]
    pub(super) fn call_pair(
        &self,
        runtime: &Runtime,
        target: Word,
        a: usize,
        b: usize,
    ) -> Option<Word> {
        if !self.check_guards(runtime, target) {
            return None;
        }
        self.invoke(a, b, Word::NIL.raw(), Word::NIL.raw())
    }

    fn check_guards(&self, runtime: &Runtime, target: Word) -> bool {
        // Explicit step budgets are charged at the original bytecode checkpoints.
        if runtime.profiling_enabled()
            || !runtime.remaining_steps_ptr().is_null()
            || !runtime.native_word_width()
        {
            return false;
        }
        self.guards.iter().all(|&LeafGuard { symbol, primitive }| {
            let expected = match primitive {
                LeafPrimitive::IntegerBits(op) => Some(vm::Primitive::guarded_integer_bits(op)),
                numeric => vm::Primitive::guarded_binding(usize::from(
                    numeric
                        .numeric()
                        .expect("numeric leaf primitive")
                        .binding_index(),
                )),
            };
            let symbol = native_function_constant(runtime, target, symbol);
            expected.is_some() && runtime.heap.symbol_cell(symbol, true) == expected
        })
    }

    #[inline]
    fn invoke(&self, a: usize, b: usize, c: usize, d: usize) -> Option<Word> {
        // SAFETY: All entry guards hold and the caller keeps NativeCode's lease alive.
        let result = unsafe {
            match self.entry {
                LeafEntry::Native(call) => call(a, b, c, d),
                #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
                LeafEntry::Wasm { slot, call } => call(slot, a, b, c, d),
            }
        };
        (result != LEAF_MISS as usize).then_some(Word::from_raw(result))
    }
}

/// Executable native entry attached to a `VmCode` record in `CodeStore`.
#[derive(Clone, Copy)]
pub struct NativeEntry {
    pub fast_entry: Option<NativeFastFn>,
    pub generic_entry: NativeGenericFn,
    pub root_slots: usize,
    pub revision: u64,
}

impl NativeEntry {
    /// Construct a [`NativeEntry`] backed by the C-ABI JIT trampoline (`jit_c_trampoline`).
    pub const fn from_jit_c(root_slots: usize, revision: u64) -> Self {
        Self {
            fast_entry: None,
            generic_entry: jit_c_trampoline,
            root_slots,
            revision,
        }
    }
}

/// Static descriptor for a single AOT-compiled function within an `AotModuleDescriptor`.
#[derive(Clone, Copy)]
pub struct AotCodeDescriptor {
    /// Generated entry supplies canonical source safepoints and register maps.
    pub native_debug: bool,
    pub entry_index: u32,
    pub arity: Arity,
    pub root_slots: usize,
    pub fast_entry: Option<NativeFastFn>,
    pub generic_entry: NativeGenericFn,
    pub c_entry: Option<JitCEntryFn>,
    /// Native call ABI revision recorded by the compiler. Handwritten synchronous entries use `None`.
    pub continuation_abi: Option<u32>,
    pub leaf_entry: Option<RegisterLeafFn>,
    pub leaf_guards: &'static [LeafGuard],
}

impl AotCodeDescriptor {
    pub const fn to_native_entry(&self) -> NativeEntry {
        NativeEntry {
            fast_entry: self.fast_entry,
            generic_entry: if self.c_entry.is_some() {
                jit_c_trampoline
            } else {
                self.generic_entry
            },
            root_slots: self.root_slots,
            revision: 0,
        }
    }
}

/// Static descriptor for an AOT-compiled module linked into the host binary.
pub struct AotModuleDescriptor {
    module_id: &'static str,
    fingerprint: AbiFingerprint,
    fasl_bytes: Option<&'static [u8]>,
    codes: &'static [AotCodeDescriptor],
}

impl AotModuleDescriptor {
    /// Describe a statically linked module and its optional embedded FASL.
    ///
    /// # Safety
    /// Native entries must implement the ABI described by `fingerprint` and
    /// remain callable for the lifetime of the descriptor. Each entry and its
    /// metadata must match its code index in the exact supplied FASL, including
    /// arity, constants, root slots, leaf guards and continuation states. Entry
    /// points must honor the tagged word, GC and activation contracts of that ABI.
    pub const unsafe fn new(
        module_id: &'static str,
        fingerprint: AbiFingerprint,
        fasl_bytes: Option<&'static [u8]>,
        codes: &'static [AotCodeDescriptor],
    ) -> Self {
        Self {
            module_id,
            fingerprint,
            fasl_bytes,
            codes,
        }
    }

    pub const fn module_id(&self) -> &'static str {
        self.module_id
    }

    pub(crate) const fn fingerprint(&self) -> AbiFingerprint {
        self.fingerprint
    }

    pub(crate) const fn fasl_bytes(&self) -> Option<&'static [u8]> {
        self.fasl_bytes
    }

    pub(crate) const fn codes(&self) -> &'static [AotCodeDescriptor] {
        self.codes
    }
}

/// Operand representation observed in the VM before a numeric primitive call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitNumericOperand {
    Fixnum,
    F32,
    F64,
}

/// Storage format observed at a simple floating-point vector read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitFloatFormat {
    F32,
    F64,
}

/// Runtime site feedback kind carried in [`JitCompilationRequest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JitSiteKind {
    Cold,
    CallPrimitive,
    KnownFixnumArith,
    KnownFloatArith {
        lhs: JitNumericOperand,
        rhs: JitNumericOperand,
    },
    FloatArrayReader {
        format: JitFloatFormat,
    },
    FloatArrayWriter {
        format: JitFloatFormat,
    },
    SimdI32x4 {
        operation: zkcl_bytecode::native_abi::I32x4Operation,
    },
    PolymorphicArithmetic,
    Megamorphic,
}

/// Site feedback entry mapped back to canonical bytecode `pc`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitSiteHint {
    pub canonical_pc: usize,
    pub kind: JitSiteKind,
}

/// `Send + Sync` bytecode snapshot sent from `CodeStore` to a background JIT compiler.
#[derive(Clone, Debug)]
pub struct JitCompilationRequest {
    /// Generate canonical source safepoints and writable lexical register maps.
    pub native_debug: bool,
    pub code_id: usize,
    pub revision: u64,
    pub arity: Arity,
    pub registers: usize,
    pub canonical: Box<[zkcl_bytecode::Instruction]>,
    pub captures: Box<[zkcl_bytecode::CaptureKind]>,
    pub notinline: Box<[u32]>,
    pub vop_calls: Box<[zkcl_bytecode::VopCall]>,
    pub handlers: Box<[zkcl_bytecode::HandlerTable]>,
    pub restarts: Box<[zkcl_bytecode::RestartTable]>,
    pub keywords: Box<[zkcl_bytecode::KeywordTable]>,
    pub jump_tables: Box<[zkcl_bytecode::JumpTable]>,
    pub param_facts: Box<[zkcl_bytecode::ParamFact]>,
    pub site_feedback: Vec<JitSiteHint>,
    /// One slot per constant, with a name for recognized Common Lisp symbols.
    pub cl_const_names: Box<[Option<&'static str>]>,
}

impl JitCompilationRequest {
    /// Canonical instructions and frame metadata, independent of the managed heap.
    pub fn bytecode(&self) -> zkcl_bytecode::BytecodeBody<'_> {
        zkcl_bytecode::BytecodeBody {
            arity: self.arity,
            registers: self.registers,
            instructions: &self.canonical,
            constant_count: self.cl_const_names.len(),
            captures: &self.captures,
            notinline: &self.notinline,
            vop_calls: &self.vop_calls,
            handlers: &self.handlers,
            restarts: &self.restarts,
            keywords: &self.keywords,
            jump_tables: &self.jump_tables,
            param_facts: &self.param_facts,
        }
    }
}

/// Compile a continuation of installed bytecode at a taken backedge's canonical target.
#[derive(Clone, Debug)]
pub struct JitOsrRequest {
    pub canonical_pc: usize,
    pub compilation: JitCompilationRequest,
}

/// JIT-compiled function ready to install in the runtime.
pub struct JitFunction {
    /// Entry implements canonical source safepoints and writable register maps.
    pub native_debug: bool,
    pub code_id: usize,
    pub entry: NativeEntry,
    pub c_entry: Option<JitCEntryFn>,
    /// The generated entry can return a VM continuation through the native call ABI.
    /// When combined with a register leaf, this entry must also execute a leaf guard miss.
    pub resumable: bool,
    pub leaf_entry: Option<RegisterLeafFn>,
    pub leaf_guards: Box<[LeafGuard]>,
    /// Keeps any reclaimable executable mapping alive for this entry, including
    /// after the backend releases its cache record. Static entries may omit it.
    pub executable_owner: Option<Arc<dyn Send + Sync>>,
}

/// Completed and failed requests drained for a runtime.
pub type JitCompletions = (Vec<JitFunction>, Vec<(usize, u64)>);

/// A copy of an installed native function's executable instructions.
#[derive(Clone, Debug)]
pub struct MachineCode {
    pub address: u64,
    pub bytes: Vec<u8>,
}

/// Background JIT compiler interface implemented by `zkcl-jit`.
///
/// # Safety
/// A completion must correspond to the exact submitted request for its code ID and
/// revision. Its entry points and metadata must satisfy the native ABI, including
/// arity, operand and constant indices, roots across GC, leaf guards and recovery
/// states. A completion marked `native_debug` must publish tagged canonical
/// registers before every canonical instruction and reload debugger edits before
/// executing it, including the prologue argument and unwind-root maps.
/// The entries must stay valid while the runtime or a call owns the
/// completion, including across `release` and backend replacement.
pub unsafe trait JitBackend: Send + Sync {
    /// Copy instructions for the exact installed revision, when retained.
    fn machine_code(&self, _code_id: usize, _revision: u64) -> Option<MachineCode> {
        None
    }
    /// Submit a native compilation request.
    ///
    /// # Safety
    /// For a request intended for runtime installation, use the exact validated
    /// snapshot supplied by that runtime, including its code ID and revision.
    /// Any standalone request must be kept separate from completions eligible
    /// for runtime installation.
    unsafe fn submit(&self, request: JitCompilationRequest);

    /// Check without waiting whether it is worth preparing an automatic request.
    /// A true result does not reserve queue capacity. `try_submit` still decides admission.
    fn can_submit_automatic(&self) -> bool {
        true
    }

    /// Submit a request without blocking the runtime. Return false when the backend is busy.
    ///
    /// # Safety
    /// The request must satisfy the contract of [`JitBackend::submit`].
    unsafe fn try_submit(&self, request: JitCompilationRequest) -> bool {
        // SAFETY: The caller guarantees the same request contract as `submit`.
        unsafe { self.submit(request) };
        true
    }

    /// Whether this backend can compile entries for running bytecode frames.
    fn supports_osr(&self) -> bool {
        false
    }

    /// Queue a loop entry without blocking. Deliver its outcome through the usual revision poll.
    ///
    /// The resumable C entry receives the active function or closure as `target` and
    /// the frame's tagged canonical registers as `argv`, with `argc` equal to the
    /// register count. It starts at the requested canonical PC and must keep values
    /// and registers live according to the usual ABI and deoptimization contracts.
    /// This entry is never used for a regular function call.
    ///
    /// # Safety
    /// The compilation snapshot must satisfy [`JitBackend::submit`]. Its canonical
    /// PC must identify the target of a taken backward branch in that bytecode.
    unsafe fn try_submit_osr(&self, _request: JitOsrRequest) -> bool {
        false
    }

    /// Queue a requested compilation even if a worker is updating the queue.
    /// A full queue may still reject it.
    ///
    /// # Safety
    /// The request must satisfy the contract of [`JitBackend::submit`].
    unsafe fn submit_explicit(&self, request: JitCompilationRequest) -> bool {
        // SAFETY: The caller guarantees the same request contract as `try_submit`.
        unsafe { self.try_submit(request) }
    }

    /// Give an unstarted request explicit priority while keeping its revision.
    /// Backends that do not prioritize queued work can leave it in place.
    fn prioritize_explicit(&self, _code_id: usize, _revision: u64) {}

    /// Return completed requests with their original code IDs and revisions.
    /// A completion must preserve entry point validity through installation and
    /// execution, as required by the backend contract.
    fn poll_completions(&self) -> Vec<JitFunction>;

    /// Whether polling may return a completion. Backends without a cheap
    /// readiness check can always return true.
    fn has_completions(&self) -> bool {
        true
    }

    /// Return responses accepted by `belongs_to_runtime`. The default drains the
    /// queue for a backend attached to one runtime. Shared backends should keep
    /// responses intended for another runtime. `None` means the worker is busy.
    fn poll_for(
        &self,
        belongs_to_runtime: &mut dyn FnMut(usize, u64) -> bool,
    ) -> Option<JitCompletions> {
        let mut units = self.poll_completions();
        units.retain(|unit| belongs_to_runtime(unit.code_id, unit.entry.revision));
        Some((units, Vec::new()))
    }

    /// Cancel obsolete work or drop backend ownership of a completed unit.
    /// An executable owner already delivered to a runtime remains live for that
    /// revision's installed entry and any active calls.
    fn release(&self, _code_id: usize, _revision: u64) {}
}
