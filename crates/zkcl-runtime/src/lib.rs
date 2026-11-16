//! An embeddable Common Lisp implementation for Rust.

mod character;
mod classes;
mod code_store;
mod compile;
mod condition;
mod core_ir;
mod definition_source;
mod encoding;
mod error;
mod execution;
mod fasl;
#[cfg(feature = "ffi")]
mod ffi;
#[cfg(not(feature = "ffi"))]
mod ffi {
    #[derive(Default)]
    pub(crate) struct ForeignSymbolTable;

    impl ForeignSymbolTable {
        pub(crate) fn is_empty(&self) -> bool {
            true
        }
    }

    pub(crate) fn initialize(_rt: &crate::Runtime) -> Result<(), crate::EvalError> {
        Ok(())
    }
}
mod finalizers;
mod form_value;
mod heap;
mod host;
mod image;
mod inspection;
mod loader;
mod macros;
pub mod metrics;
mod namespace;
pub mod native;
mod number;
mod package;
mod pathname;
mod patterns;
mod platform;
mod printer;
mod profiling;
mod read;
mod source_document;
mod symbol;
mod vm;
mod vm_handle;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod wasm;

use std::any::{Any, TypeId, type_name};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::mem::DropGuard;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::time::Duration;

use heap::{Heap, RootFrame, RootSlot, Word};
use package::{Package, PackageRegistry};
use symbol::{Name, SymbolAttributes};

pub use definition_source::{DefinitionInfo, DefinitionKind};
pub use error::{
    AlreadyStarted, EvalError, EvalErrorContext, EvalErrorKind, EvalStackFrame, EvalStage,
    ExecutionStop, OutOfMemory,
};
pub use execution::{CancellationHandle, Evaluation, EvaluationPoll};
pub use heap::PersistentRoot;
#[cfg(not(target_family = "wasm"))]
pub use host::{BlockingPool, BlockingPoolFull};
pub use host::{HostCompletion, HostServices, SimulatedHostServices};
pub use vm_handle::{Domain, Vm, VmBuilder};
pub use image::{ImageHooks, NamedImageHooks};
pub use inspection::{Inspection, InspectionPart};
pub use namespace::StandardSourceSymbols;
pub use namespace::{
    NamespaceBinding, NamespaceBindingKind, NamespacePackage, NamespaceRevision, NamespaceSnapshot,
    NamespaceSymbol, SymbolLookup, SymbolStatus,
};
pub use num_bigint::BigInt;
pub use profiling::{FunctionProfile, ProfileReport, ProfilerConfig};
pub use source_document::SourceDocument;
pub use vm::debugging::{
    Breakpoint, BreakpointFollow, BreakpointLocation, BreakpointOptions, BreakpointStatus,
    DebugEvent, DebugFrame, DebugLocalsAvailability, DebugMotion, DebugVariable, PauseHandle,
};
pub use zkcl_bytecode::Class as ConditionClass;
#[cfg(feature = "syntax-editing")]
pub use zkcl_syntax::{SourceEditError, SourceSnapshot, SourceWorkspace};
pub use zkcl_syntax::{SourcePosition, SourcePositionEncoding, syntax};
pub use zkcl_target::{CompilationTarget, MachineTarget, TargetError};

/// Returns the `(installer_id, builtin_name)` descriptors derived from the runtime installer table.
#[doc(hidden)]
pub fn builtin_installer_descriptors() -> &'static [(u32, &'static str)] {
    macros::BUILTIN_INSTALLER_DESCRIPTORS
}

/// A Lisp runtime with its own heap and program state, used on its owning thread.
pub struct Runtime {
    compilation_target: CompilationTarget,
    foreign_calls: bool,
    foreign_symbols: ffi::ForeignSymbolTable,
    packages: RefCell<PackageRegistry>,
    packages_initialized: Cell<bool>,
    installing_builtins: Cell<bool>,
    nil_name: RefCell<Option<RootSlot>>,
    t_name: RefCell<Option<RootSlot>>,
    next_symbol_id: Cell<usize>,
    read_symbol_cache: RefCell<symbol::ReadSymbolCache>,
    macro_environments: RefCell<rustc_hash::FxHashMap<usize, macros::MacroEnvironment>>,
    next_macro_environment: Cell<usize>,
    declaration_handlers: RefCell<HashMap<symbol::NameKey, RootSlot>>,
    pub(crate) definition_sources: RefCell<HashMap<(usize, DefinitionKind), DefinitionInfo>>,
    natives: RefCell<NativeFunctions>,
    builtin_native_count: Cell<usize>,
    default_print_method: Cell<Word>,
    debugger: OnceCell<vm::signaling::Debugger>,
    pause_requested: Arc<std::sync::atomic::AtomicBool>,
    debug_callback: Cell<Option<std::ptr::NonNull<()>>>,
    evaluation_debugging: Cell<bool>,
    inspector: RefCell<Option<RootSlot>>,
    printer_controls: OnceCell<heap::RootFrame>,
    printer_keywords: OnceCell<heap::RootFrame>,
    structures: RefCell<vm::structures::Registry>,
    logical_hosts: RefCell<pathname::LogicalHosts>,
    module_directories: RefCell<Vec<std::path::PathBuf>>,
    modules_loading: Rc<RefCell<std::collections::HashSet<String>>>,
    command_line_arguments: RefCell<Vec<String>>,
    environment: RefCell<HashMap<String, Option<String>>>,
    working_directory: RefCell<Option<std::path::PathBuf>>,
    exit_code: Cell<u8>,
    classes: RefCell<classes::Registry>,
    simd_type_symbols: OnceCell<vm::types::simd::SimdTypeSymbols>,
    pub(crate) invocation_protocol: OnceCell<vm::function_mop::InvocationProtocol>,
    // A weak-value EQL table: a live specializer retains its target, while
    // interning alone must not retain either an unused specializer or its target.
    eql_specializers: OnceCell<RootSlot>,
    method_combinations: OnceCell<vm::method_combination::MethodCombinations>,
    method_combination_initializing: Cell<bool>,
    next_exit: Cell<usize>,
    host_io: vm::streams::HostIo,
    host_services: Box<dyn HostServices>,
    reader_macro_context: RefCell<Option<read::ReadContext>>,
    signalled_warnings: Cell<fasl::WarningCounts>,
    repl_compiler_feedback: Cell<bool>,
    symbol_expander_code: OnceCell<(code_store::CodeId, Rc<code_store::VmCode>)>,
    constantly_code: OnceCell<RootSlot>,
    complement_code: OnceCell<RootSlot>,
    optimization_qualities: Cell<core_ir::OptimizationQualities>,
    proclaimed_inline: RefCell<HashMap<(usize, bool), core_ir::InlineDeclaration>>,
    proclaimed_types: RefCell<HashMap<symbol::NameKey, read::Form>>,
    proclaimed_ftypes: RefCell<HashMap<(symbol::NameKey, bool), read::Form>>,
    inline_defs: RefCell<HashMap<symbol::NameKey, Rc<core_ir::CoreFn>>>,
    /// Derived type expanders defined by DEFTYPE, keyed by symbol id.
    type_expanders: RefCell<HashMap<usize, RootSlot>>,
    /// User-defined SETF expanders defined by DEFSETF / DEFINE-SETF-EXPANDER, keyed by symbol id.
    setf_expanders: RefCell<HashMap<usize, SetfExpander>>,
    /// Documentation strings for SETF expanders, keyed by symbol id.
    setf_docs: RefCell<HashMap<usize, RootSlot>>,
    /// Functions enabled by TRACE, in tracing order.
    traced: RefCell<Vec<TracedFunction>>,
    profiler: profiling::Profiler,
    finalizers: finalizers::Finalizers,
    /// Compiler macros defined by DEFINE-COMPILER-MACRO / (SETF COMPILER-MACRO-FUNCTION),
    /// keyed by (symbol_id, is_setf).
    compiler_macros: RefCell<HashMap<(usize, bool), RootSlot>>,
    /// Property lists attached to symbols via SYMBOL-PLIST / GET / REMPROP, keyed by symbol id.
    symbol_plists: RefCell<HashMap<usize, RootSlot>>,
    pub(crate) aot_modules: RefCell<Vec<&'static native::AotModuleDescriptor>>,
    jit_threshold: Cell<u32>,
    cancellation: CancellationHandle,
    execution_wake: OnceCell<Arc<execution::ExecutionWake>>,
    evaluation_active: Cell<bool>,
    step_budget: Cell<Option<u64>>,
    remaining_steps: Cell<u64>,
    native_slice_steps: Cell<u16>,
    cleanup_depth: Cell<usize>,
    rust_pin_depth: Cell<usize>,
    active_scheduler: Cell<Option<std::ptr::NonNull<()>>>,
    child_task_running: Cell<bool>,
    eval_depth: Cell<usize>,
    host_scopes: RefCell<Vec<ScopedHostBinding>>,
    foreign_objects: RefCell<HashMap<u64, Rc<ForeignObjectStorage>>>,
    next_foreign_id: Cell<u64>,
    last_swept_foreign_epoch: Cell<u64>,
    pub(crate) root_stack: heap::RootStack,
    pub(crate) staging_stack: heap::RootStack,
    heap: Heap,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Release host userdata before the heap tears down.
        self.foreign_objects.get_mut().clear();
    }
}

/// A snapshot of managed heap allocation and collections for one [`Runtime`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeapStats {
    /// Effective managed heap allocation limit in bytes, with a 1 MiB minimum.
    /// The collector may use [`Self::evacuation_reserve_bytes`] above this limit.
    /// Rust-side allocations such as native code and foreign payloads are outside it.
    pub allocation_limit_bytes: usize,
    /// Total bytes in allocated Immix blocks and large object allocations.
    /// Includes free space within blocks and objects awaiting collection.
    pub committed_bytes: usize,
    /// Bytes in large object allocations, including object headers and alignment padding.
    /// These bytes are included in [`Self::committed_bytes`].
    pub large_object_bytes: usize,
    /// Number of completed collections, both automatic and explicitly requested.
    pub collections: u64,
    /// Number of completed minor (young-generation) collections.
    pub minor_collections: u64,
    /// Number of completed major or compacting full-heap collections.
    pub major_collections: u64,
    /// Elapsed time of the most recent collection, including root scanning and
    /// sweeping runtime code. Foreign host payload destruction is outside this timing.
    /// This is `None` until the first timed collection completes.
    pub last_collection_duration: Option<Duration>,
    /// Cumulative elapsed collection time. This includes the same work as
    /// [`Self::last_collection_duration`] and uses the same clock.
    pub total_collection_duration: Option<Duration>,
    /// Bytes belonging to live Lisp objects after the most recent collection.
    /// This is zero before the first collection.
    pub live_bytes: usize,
    /// Managed heap bytes allocated since the most recent collection.
    pub allocated_bytes_since_collection: usize,
    /// Committed bytes released by the most recent collection, including LOS objects.
    pub last_reclaimed_bytes: usize,
    /// Object bytes moved by the most recent Immix collection.
    pub last_evacuated_bytes: usize,
    /// Number of objects moved by the most recent Immix collection.
    pub last_evacuated_objects: usize,
    /// Number of Immix blocks currently committed to this runtime.
    pub block_count: usize,
    /// Number of new blocks committed as evacuation space in the most recent collection.
    pub last_evacuation_blocks: usize,
    /// Extra capacity available only to evacuation, rounded down to whole blocks
    /// with a one-block minimum. The collector acquires this memory on demand.
    pub evacuation_reserve_bytes: usize,
    /// Collections retried with compaction after sweep could not satisfy an allocation.
    pub compaction_retries: u64,
}

/// Startup configuration for a [`Runtime`] instance.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Requested managed heap limit, applied as described by [`HeapStats::allocation_limit_bytes`].
    /// `None` selects the default limit.
    pub heap_size_bytes: Option<usize>,
    pub jit_threshold: u32,
    pub step_budget: Option<u64>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            heap_size_bytes: None,
            jit_threshold: 1,
            step_budget: None,
        }
    }
}

/// Builder for configuring and starting a [`Runtime`].
#[derive(Clone, Debug, Default)]
pub struct RuntimeBuilder {
    compilation_target: CompilationTarget,
    config: RuntimeConfig,
    cancellation: Option<CancellationHandle>,
    foreign_calls: bool,
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Select the platform seen by reader conditionals and compile-time Lisp code.
    /// Filesystem operations, host callbacks and opt-in foreign calls still use
    /// the compiling process. Foreign type layout queries use that process's C ABI.
    pub fn compilation_target(mut self, target: CompilationTarget) -> Self {
        self.compilation_target = target;
        self
    }

    /// Set the requested managed heap limit in bytes.
    /// See [`HeapStats::allocation_limit_bytes`] for the effective limit and its scope.
    pub fn heap_size_bytes(mut self, bytes: usize) -> Self {
        self.config.heap_size_bytes = Some(bytes);
        self
    }

    pub fn jit_threshold(mut self, threshold: u32) -> Self {
        self.config.jit_threshold = threshold;
        self
    }

    pub fn step_budget(mut self, budget: Option<u64>) -> Self {
        self.config.step_budget = budget;
        self
    }

    pub fn cancellation(mut self, handle: CancellationHandle) -> Self {
        self.cancellation = Some(handle);
        self
    }

    /// Make the ZK-FFI package available to Lisp code in this instance.
    ///
    /// # Safety
    /// All code executed in this instance must honor the native libraries' ABI,
    /// pointer lifetimes, threading rules and other preconditions. This includes
    /// Lisp run by the reader or compiler and library initializers/destructors.
    #[cfg(feature = "ffi")]
    pub unsafe fn allow_foreign_calls(mut self) -> Self {
        self.foreign_calls = true;
        self
    }

    pub fn start(self) -> Result<Runtime, AlreadyStarted> {
        let builder = match self.config.heap_size_bytes {
            Some(bytes) => Heap::builder_with_heap_size(bytes),
            None => Heap::builder(),
        };
        let heap = Heap::start(builder)?;
        Ok(Runtime::with_heap_and_config(
            heap,
            self.config,
            self.cancellation.unwrap_or_default(),
            self.foreign_calls,
            self.compilation_target,
        ))
    }
}

/// Failure when borrowing a value registered via [`Runtime::with_host_mut`] or [`Runtime::with_host_ref`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HostBorrowError {
    #[error("no scoped host value of type {0} is active")]
    Missing(&'static str),
    #[error("scoped host value of type {0} is shared (immutable)")]
    MutableRequired(&'static str),
    #[error("scoped host value of type {0} is already mutably borrowed")]
    AlreadyBorrowed(&'static str),
}

/// Failure when accessing a GC-managed foreign host object stored in a [`Root`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ForeignBorrowError {
    #[error("Lisp value is not a foreign host object")]
    NotForeign,
    #[error("foreign host object has already been released")]
    Collected,
    #[error("foreign host object expects {expected}, but holds {actual}")]
    TypeMismatch {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("foreign host object of type {0} is already mutably borrowed")]
    AlreadyBorrowed(&'static str),
}

enum ScopedHostValue<'a, T> {
    Shared(&'a T),
    Mutable(RefCell<&'a mut T>),
}

struct ScopedHostBinding {
    type_id: TypeId,
    value: *const (),
}

struct ForeignObjectStorage<T: ?Sized = dyn Any> {
    type_id: TypeId,
    type_name: &'static str,
    created_epoch: u64,
    _edge: Option<heap::ForeignEdge>,
    value: RefCell<T>,
}

/// A user-defined SETF expander registered by `DEFSETF` or `DEFINE-SETF-EXPANDER`.
pub(crate) enum SetfExpander {
    /// Expander function `(place env) -> (temps forms stores writer reader)`.
    Function(RootSlot),
    /// Short-form `DEFSETF` operator symbol (function or macro name).
    Short(RootSlot),
    /// Long-form `DEFSETF` writer generator `(args env stores) -> writer`.
    Long {
        function: RootSlot,
        store_count: usize,
        positional_count: usize,
        has_keys: bool,
    },
}

/// Snapshot of a registered SETF expander for macro expansion.
#[derive(Clone, Copy)]
pub(crate) enum SetfExpanderRef {
    Function(Word),
    Short(Word),
    Long {
        function: Word,
        store_count: usize,
        positional_count: usize,
        has_keys: bool,
    },
}

/// A function or generic function enabled by TRACE.
pub(crate) struct TracedFunction {
    /// The function name symbol; for (setf f) this is the accessor symbol.
    pub(crate) symbol: RootSlot,
    pub(crate) is_setf: bool,
    /// The definition replaced by tracing; UNTRACE restores it.
    pub(crate) original: RootSlot,
    /// The wrapper function or tracing method installed by TRACE.
    pub(crate) wrapper: RootSlot,
    /// True when the wrapper is an :around method on a generic function.
    pub(crate) generic: bool,
    pub(crate) options: vm::environment::TraceOptions,
}

/// The values or host error returned by a registered Rust function.
pub type NativeResult<'rt> = Result<Vec<Root<'rt>>, Box<dyn std::error::Error + Send + Sync>>;

type NativeFunction = dyn for<'rt> Fn(&'rt Runtime, &[Root<'rt>], usize) -> Result<Vec<Root<'rt>>, condition::Fault>
    + Send
    + Sync;

#[derive(Clone)]
enum RegisteredFunction {
    Sync(Arc<NativeFunction>),
    Host(Arc<NativeFunction>),
    Async(Arc<host::HostAsyncFunction>),
}

struct NativeFunctions(Vec<RegisteredFunction>);

impl NativeFunctions {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn push(&mut self, function: Arc<NativeFunction>) {
        self.0.push(RegisteredFunction::Sync(function));
    }

    fn extend(&mut self, functions: impl IntoIterator<Item = Arc<NativeFunction>>) {
        self.0
            .extend(functions.into_iter().map(RegisteredFunction::Sync));
    }
}

type HostCallback =
    dyn Fn(&Runtime, &heap::RootSlots, usize) -> Result<Option<Word>, condition::Fault>;

/// A Lisp value kept alive for Rust, even if the collector moves its object.
/// Pass it only to operations on the runtime that created it.
pub struct Root<'rt> {
    slot: RootSlot,
    owner: &'rt Runtime,
}

impl Root<'_> {
    fn belongs_to(&self, runtime: &Runtime) -> bool {
        std::ptr::eq(self.owner, runtime)
    }

    fn foreign_id(&self, runtime: &Runtime) -> Option<u64> {
        if !self.belongs_to(runtime) {
            return None;
        }
        runtime.heap.foreign_id(self.slot.get())
    }

    pub fn is_nil(&self) -> bool {
        self.slot.get() == Word::NIL
    }

    pub fn is_t(&self) -> bool {
        self.slot.get() == Word::T
    }

    pub fn as_fixnum(&self) -> Option<isize> {
        self.slot.get().as_fixnum()
    }

    /// Read either integer representation, checking the signed 64-bit range.
    pub fn as_i64(&self) -> Option<i64> {
        if let Some(n) = self.as_fixnum() {
            return i64::try_from(n).ok();
        }
        self.owner.heap.integer(self.slot.get())?.try_into().ok()
    }

    /// Read a SINGLE-FLOAT without coercing from another numeric format.
    pub fn as_single_float(&self) -> Option<f32> {
        self.owner.heap.single_float(self.slot.get())
    }

    /// Read a DOUBLE-FLOAT without coercing from another numeric format.
    pub fn as_double_float(&self) -> Option<f64> {
        self.owner.heap.double_float(self.slot.get())
    }

    /// Read a Unicode scalar from a Lisp character.
    pub fn as_character(&self) -> Option<char> {
        self.slot.get().as_character()
    }

    /// Copy a Lisp string into an owned Rust string.
    pub fn as_string(&self, runtime: &Runtime) -> Option<String> {
        runtime.string(self)
    }

    /// Whether this value is a GC-managed host foreign object.
    pub fn is_foreign(&self, runtime: &Runtime) -> bool {
        self.foreign_id(runtime).is_some()
    }

    /// The Rust type name recorded when this foreign object was created.
    pub fn foreign_type_name(&self, runtime: &Runtime) -> Option<&'static str> {
        let id = self.foreign_id(runtime)?;
        runtime
            .foreign_objects
            .borrow()
            .get(&id)
            .map(|entry| entry.type_name)
    }

    /// Borrow the underlying host value of type `T` immutably.
    pub fn try_with_foreign_ref<T: 'static, R>(
        &self,
        runtime: &Runtime,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, ForeignBorrowError> {
        if !self.belongs_to(runtime) {
            return Err(ForeignBorrowError::NotForeign);
        }
        runtime.with_rooted_foreign_ref(self.slot.get(), f)
    }

    /// Borrow the underlying host value of type `T` mutably.
    pub fn try_with_foreign_mut<T: 'static, R>(
        &self,
        runtime: &Runtime,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, ForeignBorrowError> {
        if !self.belongs_to(runtime) {
            return Err(ForeignBorrowError::NotForeign);
        }
        let entry = runtime.foreign_entry::<T>(self.slot.get())?;
        let mut value = entry
            .value
            .try_borrow_mut()
            .map_err(|_| ForeignBorrowError::AlreadyBorrowed(entry.type_name))?;
        Ok(f(value.downcast_mut::<T>().expect("checked foreign type")))
    }

    /// Compare object identity. Fixnums and characters compare by value.
    pub fn same_identity(&self, other: &Root<'_>) -> bool {
        let value = self.slot.get();
        value == other.slot.get()
            && (value.native_index().is_none() || self.belongs_to(other.owner))
    }
}

impl Clone for Root<'_> {
    fn clone(&self) -> Self {
        Self {
            slot: self.slot.clone(),
            owner: self.owner,
        }
    }
}

impl Runtime {
    fn foreign_entry<T: 'static>(
        &self,
        word: Word,
    ) -> Result<Rc<ForeignObjectStorage>, ForeignBorrowError> {
        let id = self
            .heap
            .foreign_id(word)
            .ok_or(ForeignBorrowError::NotForeign)?;
        // A foreign borrow can allocate, collect, or access a different foreign object.
        let entry = self
            .foreign_objects
            .borrow()
            .get(&id)
            .cloned()
            .ok_or(ForeignBorrowError::Collected)?;
        if entry.type_id != TypeId::of::<T>() {
            return Err(ForeignBorrowError::TypeMismatch {
                expected: type_name::<T>(),
                actual: entry.type_name,
            });
        }
        Ok(entry)
    }

    fn with_rooted_foreign_ref<T: 'static, R>(
        &self,
        word: Word,
        read: impl FnOnce(&T) -> R,
    ) -> Result<R, ForeignBorrowError> {
        let entry = self.foreign_entry::<T>(word)?;
        let value = entry
            .value
            .try_borrow()
            .map_err(|_| ForeignBorrowError::AlreadyBorrowed(entry.type_name))?;
        Ok(read(
            value.downcast_ref::<T>().expect("checked foreign type"),
        ))
    }

    /// Create a builder to configure heap size, JIT threshold, step budget, and cancellation.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    /// Platform used by this instance for Lisp source compilation and FASL loading.
    pub fn compilation_target(&self) -> &CompilationTarget {
        &self.compilation_target
    }

    pub(crate) fn native_word_width(&self) -> bool {
        self.compilation_target
            .machine()
            .is_some_and(|machine| u32::from(machine.pointer_width().bits()) == usize::BITS)
    }

    /// Interpret a compiler heap integer in the fixnum range of the selected target.
    #[inline]
    pub(crate) fn target_fixnum_value(&self, word: Word) -> Option<i64> {
        let immediate = word.as_fixnum().map(|value| value as i64);
        if self.native_word_width() {
            return immediate;
        }
        let value = immediate.or_else(|| self.heap.integer(word)?.try_into().ok())?;
        let (min, max) = self.compilation_target.fixnum_bounds();
        (min..=max).contains(&value).then_some(value)
    }

    /// Start a Lisp runtime with a new Immix heap.
    pub fn start() -> Result<Self, AlreadyStarted> {
        RuntimeBuilder::new().start()
    }

    #[cfg(test)]
    fn with_heap(heap: Heap) -> Self {
        Self::with_heap_and_config(
            heap,
            RuntimeConfig::default(),
            CancellationHandle::new(),
            false,
            CompilationTarget::native(),
        )
    }

    fn with_heap_and_config(
        heap: Heap,
        config: RuntimeConfig,
        cancellation: CancellationHandle,
        foreign_calls: bool,
        compilation_target: CompilationTarget,
    ) -> Self {
        let initial_budget = config.step_budget;
        Self {
            compilation_target,
            foreign_calls,
            foreign_symbols: ffi::ForeignSymbolTable::default(),
            packages: RefCell::new(PackageRegistry::new(heap.root_registry())),
            packages_initialized: Cell::new(false),
            installing_builtins: Cell::new(false),
            simd_type_symbols: OnceCell::new(),
            nil_name: RefCell::default(),
            t_name: RefCell::default(),
            next_symbol_id: Cell::new(2),
            read_symbol_cache: RefCell::default(),
            macro_environments: RefCell::default(),
            next_macro_environment: Cell::default(),
            declaration_handlers: RefCell::default(),
            definition_sources: RefCell::default(),
            natives: RefCell::new(NativeFunctions(
                macros::builtin_natives()
                    .into_iter()
                    .map(RegisteredFunction::Sync)
                    .collect(),
            )),
            builtin_native_count: Cell::new(0),
            default_print_method: Cell::new(Word::NIL),
            debugger: OnceCell::new(),
            pause_requested: Arc::default(),
            debug_callback: Cell::new(None),
            evaluation_debugging: Cell::new(false),
            inspector: RefCell::new(None),
            printer_controls: OnceCell::new(),
            printer_keywords: OnceCell::new(),
            structures: RefCell::default(),
            logical_hosts: RefCell::default(),
            module_directories: RefCell::default(),
            modules_loading: Rc::default(),
            command_line_arguments: RefCell::default(),
            environment: RefCell::default(),
            working_directory: RefCell::default(),
            exit_code: Cell::new(0),
            classes: RefCell::default(),
            invocation_protocol: OnceCell::new(),
            eql_specializers: OnceCell::new(),
            method_combinations: OnceCell::new(),
            method_combination_initializing: Cell::new(false),
            next_exit: Cell::default(),
            host_io: vm::streams::HostIo::default(),
            host_services: Box::new(host::SystemServices),
            reader_macro_context: RefCell::new(None),
            signalled_warnings: Cell::default(),
            repl_compiler_feedback: Cell::new(false),
            symbol_expander_code: OnceCell::new(),
            constantly_code: OnceCell::new(),
            complement_code: OnceCell::new(),
            optimization_qualities: Cell::default(),
            proclaimed_inline: RefCell::default(),
            proclaimed_types: RefCell::default(),
            proclaimed_ftypes: RefCell::default(),
            inline_defs: RefCell::default(),
            type_expanders: RefCell::default(),
            setf_expanders: RefCell::default(),
            setf_docs: RefCell::default(),
            traced: RefCell::default(),
            profiler: profiling::Profiler::default(),
            finalizers: finalizers::Finalizers::default(),
            compiler_macros: RefCell::default(),
            symbol_plists: RefCell::default(),
            aot_modules: RefCell::default(),
            jit_threshold: Cell::new(config.jit_threshold),
            cancellation,
            execution_wake: OnceCell::new(),
            evaluation_active: Cell::new(false),
            step_budget: Cell::new(initial_budget),
            remaining_steps: Cell::new(initial_budget.unwrap_or(0)),
            native_slice_steps: Cell::new(0),
            cleanup_depth: Cell::new(0),
            rust_pin_depth: Cell::new(0),
            active_scheduler: Cell::new(None),
            child_task_running: Cell::new(false),
            eval_depth: Cell::new(0),
            host_scopes: RefCell::default(),
            foreign_objects: RefCell::default(),
            next_foreign_id: Cell::new(1),
            last_swept_foreign_epoch: Cell::new(0),
            root_stack: heap.root_stack(),
            staging_stack: heap.root_stack(),
            heap,
        }
    }

    /// Return a handle that can cooperatively cancel running evaluation from any thread.
    pub fn cancellation_handle(&self) -> CancellationHandle {
        self.cancellation.clone()
    }

    /// Configure an execution step budget per outermost host invocation (`None` disables).
    pub fn set_step_budget(&self, budget: Option<u64>) {
        self.step_budget.set(budget);
        self.remaining_steps.set(budget.unwrap_or(0));
    }

    /// Reset the remaining step budget back to [`Self::step_budget`].
    pub fn reset_step_budget(&self) {
        let budget = self.step_budget.get();
        self.remaining_steps.set(budget.unwrap_or(0));
    }

    /// Return the configured per-invocation execution step budget, if any.
    pub fn step_budget(&self) -> Option<u64> {
        self.step_budget.get()
    }

    pub(crate) fn cancellation_flag_ptr(&self) -> *const AtomicU8 {
        if self.installing_builtins.get() {
            std::ptr::null()
        } else {
            self.cancellation.flag_ptr()
        }
    }

    pub(crate) fn pin_rust_frame(&self) -> impl Drop + '_ {
        let prev = self.rust_pin_depth.get();
        self.rust_pin_depth.set(prev + 1);
        DropGuard::new((self, prev), |(rt, prev)| rt.rust_pin_depth.set(prev))
    }

    pub(crate) fn has_pinned_rust_frame(&self) -> bool {
        self.rust_pin_depth.get() != 0
    }

    pub(crate) fn remaining_steps_ptr(&self) -> *mut u64 {
        if self.step_budget.get().is_some() && !self.installing_builtins.get() {
            self.remaining_steps.as_ptr()
        } else {
            std::ptr::null_mut()
        }
    }

    fn enter_host_execution(&self) -> Result<impl Drop + '_, EvalError> {
        let prev = self.eval_depth.get();
        if prev == 0 {
            if self.evaluation_active.get() {
                return Err(EvalError::new(0, "the runtime has a suspended evaluation"));
            }
            self.reset_step_budget();
        }
        self.eval_depth.set(prev + 1);
        fn exit_host_execution(state: (&Runtime, usize)) {
            let (rt, prev) = state;
            rt.eval_depth.set(prev);
            if prev == 0 {
                rt.sweep_foreigns_after_execution();
            }
        }
        let guard = DropGuard::new((self, prev), exit_host_execution);
        if prev == 0 {
            self.finalizers.deliver(self)?;
        }
        Ok(guard)
    }

    fn sweep_foreigns_after_execution(&self) {
        // Destructors can reenter the runtime. Defer them until a normal host
        // exit if Rust is already unwinding a callback.
        if !std::thread::panicking() {
            self.sweep_dead_foreigns();
        }
    }

    /// Expose `&mut T` to registered Lisp callbacks for the duration of `f`.
    /// The scoped handle is popped automatically when `f` returns or unwinds.
    pub fn with_host_mut<T: 'static, R>(&self, host: &mut T, f: impl FnOnce() -> R) -> R {
        self.push_host_scope(ScopedHostValue::Mutable(RefCell::new(host)), f)
    }

    /// Expose `&T` to registered Lisp callbacks for the duration of `f`.
    /// The scoped handle is popped automatically when `f` returns or unwinds.
    pub fn with_host_ref<T: 'static, R>(&self, host: &T, f: impl FnOnce() -> R) -> R {
        self.push_host_scope(ScopedHostValue::Shared(host), f)
    }

    fn push_host_scope<T: 'static, R>(
        &self,
        value: ScopedHostValue<'_, T>,
        f: impl FnOnce() -> R,
    ) -> R {
        self.host_scopes.borrow_mut().push(ScopedHostBinding {
            type_id: TypeId::of::<T>(),
            value: std::ptr::from_ref(&value).cast(),
        });
        let _guard = DropGuard::new(&self.host_scopes, |scopes| {
            scopes.borrow_mut().pop();
        });
        f()
    }

    /// Access the innermost active scoped `&mut T` inside a host callback.
    pub fn try_with_host_mut<T: 'static, R>(
        &self,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, HostBorrowError> {
        self.with_host_binding::<T, _>(|binding| {
            let ScopedHostValue::Mutable(cell) = binding else {
                return Err(HostBorrowError::MutableRequired(type_name::<T>()));
            };
            let mut value = cell
                .try_borrow_mut()
                .map_err(|_| HostBorrowError::AlreadyBorrowed(type_name::<T>()))?;
            Ok(f(&mut value))
        })
    }

    /// Access the innermost active scoped `&T` inside a host callback.
    pub fn try_with_host_ref<T: 'static, R>(
        &self,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, HostBorrowError> {
        self.with_host_binding::<T, _>(|binding| match binding {
            ScopedHostValue::Shared(value) => Ok(f(value)),
            ScopedHostValue::Mutable(cell) => {
                let value = cell
                    .try_borrow()
                    .map_err(|_| HostBorrowError::AlreadyBorrowed(type_name::<T>()))?;
                Ok(f(&value))
            }
        })
    }

    fn with_host_binding<T: 'static, R>(
        &self,
        f: impl FnOnce(&ScopedHostValue<'_, T>) -> Result<R, HostBorrowError>,
    ) -> Result<R, HostBorrowError> {
        let value = self
            .host_scopes
            .borrow()
            .iter()
            .rev()
            .find(|entry| entry.type_id == TypeId::of::<T>())
            .ok_or(HostBorrowError::Missing(type_name::<T>()))?
            .value;
        // SAFETY: push_host_scope stores a ScopedHostValue of the matching T on
        // its call stack and pops the entry before returning or unwinding. Re-entry
        // may reallocate the Vec, but the value lives in that enclosing stack frame.
        f(unsafe { &*value.cast::<ScopedHostValue<'_, T>>() })
    }

    /// Wrap a long-lived Rust value into a GC-managed Lisp object.
    ///
    /// When the Lisp object becomes unreachable and is swept after collection,
    /// `value` is dropped on the runtime's thread.
    pub fn foreign<T: 'static>(&self, value: T) -> Result<Root<'_>, OutOfMemory> {
        self.foreign_with_edge(value, None)
    }

    pub(crate) fn foreign_with_edge<T: 'static>(
        &self,
        value: T,
        edge: Option<heap::ForeignEdge>,
    ) -> Result<Root<'_>, OutOfMemory> {
        let slot = edge.clone();
        self.allocate_foreign_value(value, edge, |id| {
            self.heap.allocate_foreign_with_edge(id, slot.as_ref())
        })
    }

    fn allocate_foreign_value<T: 'static>(
        &self,
        value: T,
        edge: Option<heap::ForeignEdge>,
        allocate: impl FnOnce(u64) -> Result<Word, OutOfMemory>,
    ) -> Result<Root<'_>, OutOfMemory> {
        self.sweep_dead_foreigns();
        let id = self.next_foreign_id.get();
        self.next_foreign_id.set(id + 1);
        let word = allocate(id)?;
        let rooted = self.root(word);
        let created_epoch = self.heap.foreign_epoch();
        self.foreign_objects.borrow_mut().insert(
            id,
            Rc::new(ForeignObjectStorage {
                type_id: TypeId::of::<T>(),
                type_name: type_name::<T>(),
                created_epoch,
                _edge: edge,
                value: RefCell::new(value),
            }),
        );
        Ok(rooted)
    }

    /// Make a first-class function with a traced Lisp capture and a Rust callback.
    /// Its arguments borrow the caller's already-registered GC slots.
    #[allow(dead_code)]
    pub(crate) fn host_function<F>(
        &self,
        capture: &Root<'_>,
        callback: F,
    ) -> Result<Root<'_>, OutOfMemory>
    where
        F: Fn(&Runtime, &heap::RootSlots, usize) -> Result<Option<Word>, condition::Fault>
            + 'static,
    {
        assert!(
            capture.belongs_to(self),
            "function capture belongs to another runtime"
        );
        let callback: Rc<HostCallback> = Rc::new(callback);
        let address = Rc::as_ptr(&callback);
        self.allocate_foreign_value(callback, None, |id| {
            self.heap.allocate_host_function(id, &capture.slot, address)
        })
    }

    /// Drop any unreachable foreign host values observed by the latest GC sweep.
    pub fn sweep_dead_foreigns(&self) {
        let last_epoch = self.last_swept_foreign_epoch.get();
        let dead = self
            .heap
            .with_live_foreigns(last_epoch, |swept_epoch, live_ids| {
                self.last_swept_foreign_epoch.set(swept_epoch);
                self.foreign_objects
                    .borrow_mut()
                    .extract_if(|id, entry| {
                        entry.created_epoch < swept_epoch && !live_ids.contains(id)
                    })
                    .map(|(_, entry)| entry)
                    .collect::<Vec<_>>()
            });
        // Host destructors may reenter the runtime. Both registries are unborrowed.
        drop(dead);
    }

    /// Register a statically linked AOT module descriptor after validating its ABI fingerprint.
    pub fn register_aot_module(
        &self,
        module: &'static native::AotModuleDescriptor,
    ) -> Result<(), EvalError> {
        module.fingerprint().validate()?;
        if let Some(revision) = module
            .codes()
            .iter()
            .filter_map(|code| code.continuation_abi)
            .find(|&revision| revision != zkcl_bytecode::native_abi::NATIVE_CALL_ABI_REVISION)
        {
            return Err(EvalError::new(
                0,
                format!(
                    "AOT module {:?} uses native continuation ABI {revision}, expected {}",
                    module.module_id(),
                    zkcl_bytecode::native_abi::NATIVE_CALL_ABI_REVISION,
                ),
            )
            .with_condition(ConditionClass::ProgramError));
        }
        let mut modules = self.aot_modules.borrow_mut();
        if let Some(&existing) = modules
            .iter()
            .find(|existing| existing.module_id() == module.module_id())
        {
            if std::ptr::eq(existing, module) {
                return Ok(());
            }
            return Err(EvalError::new(
                0,
                format!(
                    "AOT module ID {:?} is already registered",
                    module.module_id()
                ),
            ));
        }
        modules.push(module);
        Ok(())
    }

    /// Register and immediately load an AOT module that embeds its own `CompiledFile` FASL bytes.
    ///
    /// The embedded bytes are validated before registration. Loading uses the host invocation's
    /// execution budget.
    ///
    /// Once registered, a module remains registered if evaluation fails.
    /// Loading the same descriptor again starts at the first top-level entry and can repeat
    /// side effects from an earlier attempt.
    pub fn load_aot_module(
        &self,
        module: &'static native::AotModuleDescriptor,
    ) -> Result<(), EvalError> {
        let Some(fasl_bytes) = module.fasl_bytes() else {
            return self.register_aot_module(module);
        };
        let target = self.compilation_target();
        let compiled = zkcl_bytecode::CompiledFile::decode_for_target(fasl_bytes, target).map_err(
            |error| {
                EvalError::from_error(0, error)
                    .without_source_location()
                    .with_context(EvalErrorContext::EmbeddedModule(module.module_id().into()))
            },
        )?;
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        self.register_aot_module(module)?;
        let loader = fasl::CompiledFileLoader::new(self, compiled, Some(module));
        let load = loader::LoadFile::in_memory(loader::FileContents::Compiled(Box::new(loader)));
        vm::load_host(self, load)
    }

    pub(crate) fn aot_module_for(
        &self,
        fasl_bytes: &[u8],
    ) -> Option<&'static native::AotModuleDescriptor> {
        self.aot_modules
            .borrow()
            .iter()
            .copied()
            .find(|module| module.fasl_bytes() == Some(fasl_bytes))
    }

    /// Install a background/in-process JIT backend (e.g. `zkcl-jit::CraneliftJitBackend`).
    /// Code already installed on this runtime remains available after a backend change.
    /// A compiler targeting a different pointer width uses the VM because in-process
    /// native code tests and operates on the host's tagged words.
    pub fn set_jit_backend(&self, backend: Arc<dyn native::JitBackend>) {
        if self.native_word_width() {
            self.heap.code_store.set_jit_backend(backend);
        }
    }

    /// Set how many VM invocations with a backend enabled make a function eligible for JIT.
    pub fn set_jit_threshold(&self, threshold: u32) {
        self.jit_threshold.set(threshold);
    }

    /// Submit a compiled `CodeId` snapshot to the active JIT backend once it reaches `jit_threshold`.
    pub fn submit_jit(&self, code_id: usize) -> bool {
        let store = &self.heap.code_store;
        store.has_jit_backend()
            && store
                .find(code_id)
                .is_some_and(|code| self.submit_hot_code(code_id, &code))
    }

    /// Immediately submit a compiled `CodeId` snapshot to the active JIT backend regardless of threshold.
    pub fn submit_jit_now(&self, code_id: usize) -> bool {
        self.heap
            .code_store
            .submit_jit(code_id, true, self.heap.function_epoch())
    }

    pub(crate) fn native_entry_for_call(
        &self,
        code_id: usize,
        vm_code: &code_store::VmCode,
    ) -> Option<Rc<code_store::NativeCode>> {
        if self.use_debug_bytecode(vm_code) {
            let store = &self.heap.code_store;
            store.poll_jit_completions();
            if self.use_debug_bytecode(vm_code) {
                self.submit_hot_code(code_id, vm_code);
                return None;
            }
        }
        let sliced =
            !self.has_pinned_rust_frame() && vm::task_scheduler::needs_suspendable_calls(self);
        let eligible = |entry: &Rc<code_store::NativeCode>| {
            !sliced || (entry.fits_slice() && self.native_slice_steps.get() != 0)
        };
        if let Some(entry) = vm_code.native_entry() {
            return eligible(&entry).then_some(entry);
        }
        let store = &self.heap.code_store;
        if !store.has_jit_backend() {
            return None;
        }
        if store.poll_jit_completions() != 0
            && let Some(entry) = vm_code.native_entry()
        {
            return eligible(&entry).then_some(entry);
        }
        self.submit_hot_code(code_id, vm_code);
        None
    }

    pub(crate) fn use_debug_bytecode(&self, code: &code_store::VmCode) -> bool {
        let observing = code.debug.retain_frame
            || self.debugger.get().is_some_and(|debugger| {
                debugger.observing_execution(self, self.evaluation_debugging.get())
            });
        observing && !code.native_entry().is_some_and(|entry| entry.native_debug)
    }

    /// Submit `code_id` once it is hot.
    fn submit_hot_code(&self, code_id: usize, code: &code_store::VmCode) -> bool {
        if code.bump_call_count() < self.jit_threshold.get() {
            return false;
        }
        self.heap
            .code_store
            .submit_jit(code_id, false, self.heap.function_epoch())
    }

    /// Poll completed JIT compilations from the active backend and attach them to `CodeStore`.
    pub fn poll_jit_completions(&self) -> usize {
        self.heap.code_store.poll_jit_completions()
    }

    /// Obtain the installed machine code for this exact Lisp function object.
    pub fn machine_code(
        &self,
        function: &Root<'_>,
    ) -> Result<Option<native::MachineCode>, EvalError> {
        let function = self.resolve_function(function)?;
        self.poll_jit_completions();
        Ok(self
            .heap
            .function_code(function.slot.get())
            .and_then(|code| self.heap.function_info(code))
            .and_then(|info| self.heap.code_store.get(info.code_id).native_entry())
            .and_then(|native| native.machine_code()))
    }

    /// Read, compile, and run one Lisp form, returning all its values in order.
    ///
    /// An empty vector means zero values. Returning `nil` produces one value.
    pub fn eval(&self, source: &str) -> Result<Vec<Root<'_>>, EvalError> {
        self.eval_document(&SourceDocument::input(source))
    }

    /// Evaluate one form from an immutable host source snapshot. All locations
    /// refer to UTF-8 byte offsets in `document.text()`, before its base offset.
    pub fn eval_document(&self, document: &SourceDocument) -> Result<Vec<Root<'_>>, EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        let source = document
            .text()
            .ok_or_else(|| EvalError::new(0, "source document has no text"))?;
        let form = read::read(self, source).map_err(|error| error.in_document(document))?;
        vm::eval_host(self, form, Some(document.clone()))
    }

    /// Read one Lisp object in this runtime's current package and readtable.
    /// Reader macros may run, as with Common Lisp `READ`.
    pub fn read_value(&self, source: &str) -> Result<Root<'_>, EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        let form = read::read(self, source)?;
        form_value::to_value(self, &form)
    }

    /// Read one form and prepare it for incremental execution. Reading, including
    /// user-defined reader macros, runs synchronously here. SchedulerStep the returned
    /// invocation on this runtime's thread with [`Evaluation::resume`].
    pub fn start_eval(&self, source: &str) -> Result<Evaluation<'_>, EvalError> {
        self.start_eval_document(&SourceDocument::input(source))
    }

    /// Prepare incremental evaluation from a host source snapshot.
    pub fn start_eval_document(
        &self,
        document: &SourceDocument,
    ) -> Result<Evaluation<'_>, EvalError> {
        self.start_evaluation(|| {
            let source = document
                .text()
                .ok_or_else(|| EvalError::new(0, "source document has no text"))?;
            let form = read::read(self, source).map_err(|error| error.in_document(document))?;
            Ok(vm::prepare_eval(self, form, document.clone()))
        })
    }

    /// Read and evaluate consecutive top-level forms in source order without
    /// maintaining REPL history.
    pub fn load_str(&self, source: &str) -> Result<(), EvalError> {
        self.eval_source_impl(source, false, |_, _| Ok(()))
    }

    fn load_builtin_source(&self, source: &'static str) -> Result<(), EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        vm::load_builtin_source(self, source)
    }

    /// Read and evaluate a source buffer one top-level form at a time.
    ///
    /// Maintain Lisp REPL history. After each form, pass all its values to
    /// the caller before reading the next form. A callback error stops
    /// evaluation.
    pub fn eval_source<'rt, E: From<EvalError>>(
        &'rt self,
        source: &str,
        mut on_values: impl FnMut(&[Root<'rt>]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.eval_source_with_offsets(source, |_, values| on_values(values))
    }

    /// Like [`Self::eval_source`], with the UTF-8 byte offset immediately past
    /// each evaluated form in the original source.
    pub fn eval_source_with_offsets<'rt, E: From<EvalError>>(
        &'rt self,
        source: &str,
        on_form: impl FnMut(usize, &[Root<'rt>]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.eval_source_impl(source, true, on_form)
    }

    /// Evaluate consecutive forms with REPL history, preserving the supplied
    /// snapshot. Callback offsets are relative UTF-8 bytes in its text.
    pub fn eval_document_with_offsets<'rt, E: From<EvalError>>(
        &'rt self,
        document: &SourceDocument,
        on_form: impl FnMut(usize, &[Root<'rt>]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.eval_document_impl(document, true, on_form)
    }

    /// Evaluate all forms in a host snapshot without changing REPL history.
    pub fn load_document(&self, document: &SourceDocument) -> Result<(), EvalError> {
        self.eval_document_impl(document, false, |_, _| Ok(()))
    }

    /// Evaluate a terminal submission, requesting additional lines only when
    /// a started form needs more input. The live Lisp reader owns continuation,
    /// including calls to user reader macros. Each completed form updates the
    /// normal REPL history before calling `on_form` with its decoded source.
    pub fn eval_submission<'rt, E: From<EvalError>>(
        &'rt self,
        initial: &str,
        next_line: impl FnMut() -> std::io::Result<Option<String>> + 'static,
        mut on_form: impl FnMut(&SourceDocument, usize, &[Root<'rt>]) -> Result<(), E>,
    ) -> Result<(), E> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        let mut submission = vm::streams::Submission::new(self, initial, next_line)?;
        while let Some(form) = submission.next_form()? {
            let source = submission.document();
            let values = self.eval_top_level_form(form, source, true)?;
            on_form(source, submission.offset(), &values)?;
            if self.eval_depth.get() == 1 {
                self.finalizers.deliver(self)?;
            }
        }
        Ok(())
    }

    fn eval_source_impl<'rt, E: From<EvalError>>(
        &'rt self,
        source: &str,
        track_history: bool,
        on_form: impl FnMut(usize, &[Root<'rt>]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.eval_document_impl(&SourceDocument::input(source), track_history, on_form)
    }

    fn eval_document_impl<'rt, E: From<EvalError>>(
        &'rt self,
        document: &SourceDocument,
        track_history: bool,
        mut on_form: impl FnMut(usize, &[Root<'rt>]) -> Result<(), E>,
    ) -> Result<(), E> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        let source = document
            .text()
            .ok_or_else(|| EvalError::new(0, "source document has no text"))?;
        let mut reader = read::Reader::new(self, source);
        while let Some(form) = reader
            .next_form()
            .map_err(|error| error.in_document(&document))?
        {
            let end = reader.read_position(true);
            let values = self.eval_top_level_form(form, &document, track_history)?;
            on_form(end, &values)?;
            if self.eval_depth.get() == 1 {
                self.finalizers.deliver(self)?;
            }
        }
        Ok(())
    }

    fn eval_top_level_form(
        &self,
        form: read::Form,
        document: &SourceDocument,
        track_history: bool,
    ) -> Result<Vec<Root<'_>>, EvalError> {
        let at = form.at;
        let history_form = if track_history {
            let value = form_value::to_value(self, &form)?;
            if let Some((symbol, _)) = self.packages.borrow().present(Package::COMMON_LISP, "-") {
                self.heap.set_symbol_cell(symbol, false, value.slot.get());
            }
            Some(value)
        } else {
            None
        };
        let evaluation = vm::eval_host(self, form, Some(document.clone()));
        if let Some(form) = history_form {
            self.shift_repl_history(["+", "++", "+++"], form.slot.get());
        }
        let values = evaluation?;
        if track_history {
            self.record_repl_values(&values, at)?;
        }
        Ok(values)
    }

    fn shift_repl_history(&self, names: [&str; 3], value: Word) {
        let packages = self.packages.borrow();
        let [latest, previous, oldest] = names.map(|name| {
            packages
                .present(Package::COMMON_LISP, name)
                .map(|(word, _)| word)
        });
        for (into, from) in [(oldest, previous), (previous, latest)] {
            if let Some(into) = into {
                let prior = from
                    .and_then(|symbol| self.heap.symbol_cell(symbol, false))
                    .unwrap_or(Word::NIL);
                self.heap.set_symbol_cell(into, false, prior);
            }
        }
        if let Some(symbol) = latest {
            self.heap.set_symbol_cell(symbol, false, value);
        }
    }

    fn record_repl_values(&self, values: &[Root<'_>], at: usize) -> Result<(), EvalError> {
        let allocation_error = |error| EvalError::out_of_memory(at, error);
        let list = match values {
            [] => Word::NIL,
            [value] => self
                .heap
                .cons(|| (value.slot.get(), Word::NIL))
                .map_err(allocation_error)?,
            _ => {
                let tail = self.root_frame(1);
                for value in values.iter().rev() {
                    let pair = self
                        .heap
                        .cons(|| (value.slot.get(), tail.get(0)))
                        .map_err(allocation_error)?;
                    tail.set(0, pair);
                }
                tail.get(0)
            }
        };
        let primary = values.first().map_or(Word::NIL, |value| value.slot.get());
        self.shift_repl_history(["/", "//", "///"], list);
        self.shift_repl_history(["*", "**", "***"], primary);
        Ok(())
    }

    /// Load Lisp source or zkcl bytecode at a host filesystem path.
    ///
    /// Loading uses the same dynamic file and package bindings as Lisp LOAD.
    pub fn load_file(&self, path: &std::path::Path) -> Result<(), EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        let file = loader::prepare_host_load(self, path)?;
        let compiled = matches!(&file.contents, loader::FileContents::Compiled(_));
        let source = match &file.contents {
            loader::FileContents::Source(source) => Some(source.clone()),
            _ => None,
        };
        vm::load_host(self, file).map_err(|error| {
            let source = error.source_document().cloned().or(source);
            let mut error = error;
            if let Some(source) = source {
                error = error.in_document(&source);
                if let Some(path) = source.path() {
                    error = error.in_file(path);
                }
            } else if !compiled {
                error = error.in_file(path);
            }
            if compiled {
                error = error.with_context(EvalErrorContext::Artifact(path.to_owned()));
            }
            error.with_context(EvalErrorContext::Stage(EvalStage::Load))
        })
    }

    /// Load a FASL supplied by the host, such as a bundled application asset.
    /// The load has no file pathname and uses this runtime's compilation target.
    pub fn load_fasl(&self, bytes: &[u8]) -> Result<(), EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        let contents = loader::FileContents::compiled(self, bytes).map_err(|error| {
            EvalError::from_error(0, error)
                .without_source_location()
                .with_context(EvalErrorContext::Stage(EvalStage::Load))
        })?;
        self.initialize_packages()?;
        vm::load_host(self, loader::LoadFile::in_memory(contents))
            .map_err(|error| error.with_context(EvalErrorContext::Stage(EvalStage::Load)))
    }

    /// Evaluate a source initialization file in the ongoing top-level context.
    /// Changes to the package and readtable apply to subsequent input.
    pub fn load_init_file(&self, path: &std::path::Path) -> Result<(), EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        let text = std::fs::read_to_string(path).map_err(|error| {
            EvalError::file_io(0, path, error)
                .without_source_location()
                .with_context(EvalErrorContext::Stage(EvalStage::Load))
        })?;
        let document = SourceDocument::file(path.to_owned(), Some(text));
        let text = document.text().expect("initialization source");
        let mut reader = read::Reader::new(self, text);
        let result = (|| {
            while let Some(form) = reader.next_form()? {
                self.eval_top_level_form(form, &document, false)?;
                if self.eval_depth.get() == 1 {
                    self.finalizers.deliver(self)?;
                }
            }
            Ok::<(), EvalError>(())
        })();
        result.map_err(|error| {
            let source = error.source_document().cloned().unwrap_or(document);
            let error = error
                .in_document(&source)
                .in_file(source.path().unwrap_or(path));
            error.with_context(EvalErrorContext::Stage(EvalStage::Load))
        })
    }

    /// Compile all forms in an editor buffer into a FASL object without writing
    /// a file. Compile-time forms run in the current environment. Source offsets
    /// remain relative to this snapshot, including when its base is nonzero.
    pub fn compile_document_to_bytecode(
        &self,
        document: &SourceDocument,
    ) -> Result<zkcl_bytecode::CompiledFile, EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        fasl::compile_host_document(self, document)
    }

    /// Compile a UTF-8 Lisp file to a zkcl bytecode file.
    ///
    /// Returns the output's filesystem truename. A missing output path selects
    /// the same basename with the zkcl compiled-file extension.
    pub fn compile_file(
        &self,
        input: &std::path::Path,
        output: Option<&std::path::Path>,
    ) -> Result<std::path::PathBuf, EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        fasl::compile_host(self, input, output)
    }

    /// Compile a Lisp source file to a `CompiledFile` in memory.
    /// Compile-time evaluation uses the same file bindings as [`Self::compile_file`].
    pub fn compile_file_to_bytecode(
        &self,
        input: &std::path::Path,
    ) -> Result<zkcl_bytecode::CompiledFile, EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        fasl::compile_host_bytecode(self, input)
    }

    /// Invoke a Lisp function object or a symbol naming a function.
    /// The target and arguments must belong to this runtime.
    pub fn call(&self, target: &Root<'_>, args: &[&Root<'_>]) -> Result<Vec<Root<'_>>, EvalError> {
        let mut results = Vec::new();
        self.call_into(target, args, &mut results)?;
        Ok(results)
    }

    /// Prepare a rooted function designator and arguments for incremental execution.
    /// The function binding is resolved when the invocation first runs.
    pub fn start_call(
        &self,
        target: &Root<'_>,
        args: &[&Root<'_>],
    ) -> Result<Evaluation<'_>, EvalError> {
        self.check_call_roots(target, args)?;
        self.start_evaluation(|| Ok(vm::prepare_call(self, target, args)))
    }

    fn start_evaluation<'rt>(
        &'rt self,
        prepare: impl FnOnce() -> Result<vm::task_scheduler::TaskExecutor<'rt>, EvalError>,
    ) -> Result<Evaluation<'rt>, EvalError> {
        if self.eval_depth.get() != 0 || self.has_pinned_rust_frame() {
            return Err(EvalError::new(
                0,
                "incremental execution requires a top-level invocation",
            ));
        }
        let (scheduled, remaining) = {
            let _exec_guard = self.enter_host_execution()?;
            self.initialize_packages()?;
            let scheduled = prepare()?;
            (scheduled, self.remaining_steps.get())
        };
        if self.evaluation_active.get() {
            return Err(EvalError::new(0, "the runtime has a suspended evaluation"));
        }
        self.remaining_steps.set(remaining);
        Ok(Evaluation::new(self, scheduled))
    }

    /// Invoke a function and append its rooted values to a caller-provided collection.
    /// The target and arguments must belong to this runtime. An evaluation error
    /// leaves the collection untouched. Zero Lisp values append nothing.
    pub fn call_into<'rt>(
        &'rt self,
        target: &Root<'_>,
        args: &[&Root<'_>],
        results: &mut impl Extend<Root<'rt>>,
    ) -> Result<(), EvalError> {
        self.check_call_roots(target, args)?;
        let values = self.call_slots(&target.slot, args.iter().map(|arg| arg.slot.get()))?;
        results.extend(values.roots(self));
        Ok(())
    }

    fn check_call_roots(&self, target: &Root<'_>, args: &[&Root<'_>]) -> Result<(), EvalError> {
        if !target.belongs_to(self) || args.iter().any(|arg| !arg.belongs_to(self)) {
            return Err(EvalError::new(
                0,
                "call received a value from a different runtime",
            ));
        }
        Ok(())
    }

    fn call_slots(
        &self,
        target: &RootSlot,
        args: impl ExactSizeIterator<Item = Word>,
    ) -> Result<vm::Values, EvalError> {
        let _exec_guard = self.enter_host_execution()?;
        self.initialize_packages()?;
        vm::call_host_values(self, target, args)
    }

    /// Add a directory for module names requested by Lisp's `REQUIRE` without paths.
    /// Directories are searched in registration order, with source files preferred
    /// over compiled files in each directory.
    pub fn add_module_directory(&self, path: &std::path::Path) -> std::io::Result<()> {
        let directory = path.canonicalize()?;
        if !directory.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "module search path is not a directory",
            ));
        }
        let mut directories = self.module_directories.borrow_mut();
        if !directories.contains(&directory) {
            directories.push(directory);
        }
        Ok(())
    }

    /// Set the arguments returned by `zk:command-line-arguments`.
    pub fn set_command_line_arguments(&self, arguments: Vec<String>) {
        *self.command_line_arguments.borrow_mut() = arguments;
    }

    /// The status requested by `zk:set-exit-code`. It does not exit the host.
    pub fn requested_exit_code(&self) -> u8 {
        self.exit_code.get()
    }

    /// Redirect this runtime's standard input, including the input side of terminal I/O.
    pub fn set_standard_input(&mut self, input: impl std::io::BufRead + 'static) {
        self.host_io.set_input(input, false);
    }

    /// Supply a host input reader with interactive terminal semantics.
    pub fn set_interactive_input(&mut self, input: impl std::io::BufRead + 'static) {
        self.host_io.set_input(input, true);
    }

    /// Redirect this runtime's standard output, including trace and terminal output.
    pub fn set_standard_output(&mut self, output: impl std::io::Write + 'static) {
        self.host_io.set_output(output);
    }

    pub(crate) fn root_slot(&self, word: Word) -> RootSlot {
        self.heap.root_slot(word)
    }

    pub(crate) fn root_frame(&self, len: usize) -> RootFrame {
        self.heap.root_frame(len)
    }

    /// Supply clocks and entropy for this runtime.
    pub fn set_host_services(&mut self, services: impl HostServices + 'static) {
        self.host_services = Box::new(services);
    }

    /// Install the host's standard debugger. It receives one condition, after
    /// `*DEBUGGER-HOOK*` has run, in the signaling invocation's dynamic environment.
    /// A debugger can transfer control using a restart. Returning, or clearing the
    /// debugger with `None`, propagates the original error to the host.
    pub fn set_debugger(&self, function: Option<&Root<'_>>) -> Result<(), EvalError> {
        self.initialize_packages()?;
        let function = match function {
            Some(f) if f.belongs_to(self) && vm::is_function(self, f.slot.get()) => f.slot.get(),
            Some(_) => {
                return Err(EvalError::new(
                    0,
                    "expected a debugger function from this runtime",
                ));
            }
            None => Word::NIL,
        };
        self.debugger
            .get()
            .expect("initialized debugger")
            .set_frontend(function);
        self.heap.code_store.invalidate_native_calls();
        Ok(())
    }

    /// Redirect this runtime's error output.
    pub fn set_error_output(&mut self, output: impl std::io::Write + 'static) {
        self.host_io.set_error(output);
    }

    /// Record text written directly by an interactive host in the active transcript.
    /// Lisp stream input and output are recorded by the runtime itself.
    pub fn record_dribble_text(&self, text: &str) -> Result<(), EvalError> {
        self.host_io
            .record_dribble(text)
            .map_err(|error| EvalError::io_error(0, error))
    }

    /// Install a Rust function in the function cell of a Lisp symbol or `(setf name)`.
    ///
    /// The name is read in the current package, and can use a package prefix.
    /// Existing function objects remain callable if the name is later redefined;
    /// Rust callbacks live until the runtime is dropped.
    pub fn register_function<F>(&self, name: &str, function: F) -> Result<(), EvalError>
    where
        F: for<'rt> Fn(&'rt Runtime, &[Root<'rt>]) -> NativeResult<'rt> + Send + Sync + 'static,
    {
        self.initialize_packages()?;
        vm::install_host_function(
            self,
            name,
            Arc::new(move |runtime, arguments, at| {
                let results = function(runtime, arguments)
                    .map_err(|error| EvalError::host_callback(at, error))?;
                if results.iter().any(|value| !value.belongs_to(runtime)) {
                    return Err(EvalError::host_callback(
                        at,
                        "callback returned a value from a different runtime".into(),
                    )
                    .into());
                }
                Ok(results)
            }),
        )
    }

    /// Install an asynchronous host function. It starts on the owning thread with rooted
    /// arguments and a one-shot completion. Copy any data the operation needs before returning.
    /// Its Lisp caller can wait in both the synchronous and incremental drivers.
    /// Cooperative cancellation requests that the provider stop and waits until it submits
    /// or drops its completion. Keep that token until the provider has released the resources
    /// borrowed by the work. Hard abort and dropping an evaluation retire its wait immediately.
    /// Returning an error requests cancellation if the completion is still outstanding.
    /// The error is delivered after the provider finishes.
    pub fn register_async_function<F>(&self, name: &str, start: F) -> Result<(), EvalError>
    where
        F: for<'rt> Fn(&'rt Runtime, &[Root<'rt>], HostCompletion) -> Result<(), host::HostError>
            + Send
            + Sync
            + 'static,
    {
        self.initialize_packages()?;
        vm::install_registered_function(self, name, RegisteredFunction::Async(Arc::new(start)))
    }

    /// Copy the print name of a symbol to an owned Rust string.
    pub fn symbol_name(&self, symbol: &Root<'_>) -> Option<String> {
        if !symbol.belongs_to(self) {
            return None;
        }
        self.heap.symbol_name(symbol.slot.get())
    }

    /// Find an accessible symbol by exact package name (or nickname) and print name.
    /// A missing symbol returns `None`; an unknown package is an error. Lookup
    /// does not intern a symbol or invoke the Lisp reader. Passing the returned
    /// symbol to [`Self::call`] resolves its current function definition.
    pub fn lookup_symbol(&self, package: &str, name: &str) -> Result<Option<Root<'_>>, EvalError> {
        Ok(self.find_symbol(package, name)?.map(|found| found.symbol))
    }

    /// Resolve a function designator now. The returned function object stays
    /// callable when the symbol's global function definition is replaced.
    pub fn resolve_function(&self, target: &Root<'_>) -> Result<Root<'_>, EvalError> {
        if !target.belongs_to(self) {
            return Err(EvalError::new(
                0,
                "function designator belongs to a different runtime",
            ));
        }
        self.initialize_packages()?;
        vm::designator(self, target.slot.get(), 0)
            .map(|word| self.root(word))
            .map_err(|fault| fault.host)
    }

    /// Render a value using escaped Lisp syntax in the current package.
    /// Opaque values have descriptive representations such as `#<FUNCTION>`.
    pub fn repr(&self, value: &Root<'_>) -> Result<String, EvalError> {
        if !value.belongs_to(self) {
            return Err(EvalError::new(
                0,
                "cannot print a value from a different runtime",
            ));
        }
        let package = printer::current_package(self, 0)?;
        let options = printer::PrintControls::default();
        printer::render(
            &vm::printer::PrintContext::host(self),
            package,
            value.slot.get(),
            &options,
            None,
            printer::PrintDispatchMode::Diagnostic,
            0,
        )
        .map_err(|error| error.host)
    }

    /// Print a result with the active Common Lisp printer controls and
    /// custom `PRINT-OBJECT` methods. Pass `Some(false)` to turn off pretty printing.
    pub fn print_result(
        &self,
        value: &Root<'_>,
        pretty: Option<bool>,
    ) -> Result<String, EvalError> {
        self.initialize_packages()?;
        let write = self.root(vm::Primitive::named("WRITE-TO-STRING").expect("standard printer"));
        let printed = if let Some(pretty) = pretty {
            let key = self.root(self.intern_package(Package::KEYWORD, "PRETTY", 0)?.0);
            let pretty = if pretty { self.t() } else { self.nil() };
            self.call(&write, &[value, &key, &pretty])?
        } else {
            self.call(&write, &[value])?
        };
        printed
            .first()
            .and_then(|result| self.string(result))
            .ok_or_else(|| EvalError::new(0, "printer did not return a string"))
    }

    fn symbol_name_word(&self, word: Word, at: usize) -> Result<Option<Word>, EvalError> {
        let (slot, name) = match word {
            Word::NIL => (&self.nil_name, "NIL"),
            Word::T => (&self.t_name, "T"),
            _ => return Ok(self.heap.symbol_name_word(word)),
        };
        if let Some(root) = slot.borrow().as_ref() {
            return Ok(Some(root.get()));
        }
        let name = self
            .heap
            .string(name)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        slot.replace(Some(self.root_slot(name)));
        Ok(Some(name))
    }

    /// Copy a Lisp string into an owned Rust string.
    pub fn string(&self, value: &Root<'_>) -> Option<String> {
        if !value.belongs_to(self) {
            return None;
        }
        self.heap.string_contents(value.slot.get())
    }

    fn string_designator_text(&self, word: Word) -> Option<String> {
        self.heap
            .string_contents(word)
            .or_else(|| self.heap.symbol_name(word))
            .or_else(|| word.as_character().map(String::from))
    }

    /// The current package name, if the binding names a live package.
    pub fn current_package(&self) -> Option<String> {
        let package = if self.packages_initialized.get() {
            self.current_package_at(0).ok()?
        } else {
            Package::USER
        };
        self.packages.borrow().name(package).map(str::to_owned)
    }

    /// Whether the current readtable has zkcl's `#>` raw string reader installed.
    pub fn raw_strings_enabled(&self) -> bool {
        read::raw_strings_enabled(self)
    }

    /// Whether the current reader has syntax that the static source tools do
    /// not model. User-defined reader macros are never called by this query.
    pub fn custom_readtable(&self) -> bool {
        read::custom_readtable(self)
    }

    /// Emit non-fatal style warnings for calls to functions without a current definition.
    pub fn set_interactive_compiler_feedback(&self, enabled: bool) {
        self.repl_compiler_feedback.set(enabled);
    }

    fn bound_package(&self) -> Option<Package> {
        self.heap
            .symbol_cell(self.package_variable(), false)
            .and_then(Package::from_word)
    }

    fn find_package_named(&self, name: &str) -> Option<Package> {
        self.packages
            .borrow()
            .named_from(self.bound_package(), name)
    }

    fn current_package_at(&self, at: usize) -> Result<Package, EvalError> {
        self.bound_package()
            .filter(|package| self.packages.borrow().name(*package).is_some())
            .ok_or_else(|| EvalError::read(at, "*PACKAGE* must name a live package"))
    }

    fn package_variable(&self) -> Word {
        self.packages
            .borrow()
            .present(Package::COMMON_LISP, "*PACKAGE*")
            .expect("initialized *PACKAGE*")
            .0
    }

    fn initialize_packages(&self) -> Result<(), EvalError> {
        if self.packages_initialized.get() {
            return self.initialize_method_combination();
        }
        let mut features = self.nil();
        for feature in self
            .compilation_target
            .features()
            .chain(["ZK-TREELIST", zkcl_syntax::simd_builtins::PACKAGE])
            .chain(self.foreign_calls.then_some("ZK-FFI"))
        {
            let (word, _) = self.intern_package(Package::KEYWORD, feature, 0)?;
            features = self
                .cons(&self.root(word), &features)
                .map_err(|error| EvalError::out_of_memory(0, error))?;
        }

        let pathname_defaults = pathname::initial_defaults(self, 0)?;
        let random_state = self.root_slot(
            self.heap
                .allocate_random_state(heap::RandomState::INITIAL)
                .map_err(|error| EvalError::out_of_memory(0, error))?,
        );
        let streams = vm::streams::StandardStreams::new(self)?;
        let readtable = self.root_slot(
            self.heap
                .readtable(heap::ReadtableCase::Upcase)
                .map_err(|error| EvalError::out_of_memory(0, error))?,
        );
        let upcase = self.root_slot(self.intern_package(Package::KEYWORD, "UPCASE", 0)?.0);
        let external_symbols = zkcl_syntax::cl_builtins::EXTERNAL_NAMES;
        let canonical = self.root_frame(external_symbols.len());
        for (index, &spelling) in external_symbols.iter().enumerate() {
            let constant = if let Some(value) = float_constant(spelling) {
                let value = self
                    .heap
                    .allocate_number(&number::Number::Real(value))
                    .map_err(|error| EvalError::out_of_memory(0, error))?;
                Some(self.root_slot(value))
            } else {
                self.integer_constant(spelling)
                    .map(|value| self.integer_from(value).map(|root| root.slot))
                    .transpose()
                    .map_err(|error| EvalError::out_of_memory(0, error))?
            };
            let word = match spelling {
                "NIL" => Word::NIL,
                "T" => Word::T,
                _ => {
                    self.allocate_symbol_with_origin(spelling, Some(Package::COMMON_LISP), true, 0)?
                        .1
                }
            };
            if let Some(function) = vm::Primitive::named(spelling) {
                self.heap.set_symbol_cell(word, true, function);
            }
            if let Some(value) = constant {
                self.heap.set_symbol_cell(word, false, value.get());
            }
            let initial = match spelling {
                "*MACROEXPAND-HOOK*" => vm::Primitive::named("FUNCALL"),
                "*DEBUGGER-HOOK*" => Some(Word::NIL),
                "*GENSYM-COUNTER*" => Word::fixnum(0),
                "*READ-BASE*" => Word::fixnum(10),
                "*READ-SUPPRESS*" => Some(Word::NIL),
                "*READ-EVAL*" => Some(Word::T),
                "*MODULES*" => Some(Word::NIL),
                "*RANDOM-STATE*" => Some(random_state.get()),
                "*LOAD-PATHNAME*" | "*LOAD-TRUENAME*" | "*LOAD-VERBOSE*" | "*LOAD-PRINT*" => {
                    Some(Word::NIL)
                }
                "*COMPILE-FILE-PATHNAME*"
                | "*COMPILE-FILE-TRUENAME*"
                | "*COMPILE-VERBOSE*"
                | "*COMPILE-PRINT*" => Some(Word::NIL),
                "*READTABLE*" => Some(readtable.get()),
                "*PACKAGE*" => Some(Package::USER.word()),
                "*FEATURES*" => Some(features.slot.get()),
                "*DEFAULT-PATHNAME-DEFAULTS*" => Some(pathname_defaults.slot.get()),
                "*BREAK-ON-SIGNALS*" => Some(Word::NIL),
                "*" | "**" | "***" | "+" | "++" | "+++" | "/" | "//" | "///" | "-" => {
                    Some(Word::NIL)
                }
                _ => streams
                    .initial(spelling)
                    .or_else(|| printer::initial(spelling, upcase.get())),
            };
            if let Some(initial) = initial {
                self.heap.proclaim_special(word);
                self.heap.set_symbol_cell(word, false, initial);
            }
            canonical.set(index, word);
        }

        {
            let mut packages = self.packages.borrow_mut();
            packages.reserve_symbols(Package::COMMON_LISP, external_symbols.len());
            for (index, &spelling) in external_symbols.iter().enumerate() {
                packages.insert(
                    Package::COMMON_LISP,
                    spelling.into(),
                    canonical.get(index),
                    true,
                );
            }
        }
        drop(canonical);
        let canonical_word = |name| {
            self.packages
                .borrow()
                .present(Package::COMMON_LISP, name)
                .expect("standard symbol initialized")
                .0
        };
        let format = canonical_word("*READ-DEFAULT-FLOAT-FORMAT*");
        self.heap.proclaim_special(format);
        self.heap
            .set_symbol_cell(format, false, canonical_word("SINGLE-FLOAT"));
        let mut lambda_keywords = self.root_slot(Word::NIL);
        for name in [
            "&OPTIONAL",
            "&REST",
            "&BODY",
            "&KEY",
            "&ALLOW-OTHER-KEYS",
            "&AUX",
            "&WHOLE",
            "&ENVIRONMENT",
        ]
        .into_iter()
        .rev()
        {
            let pair = self
                .heap
                .cons(|| (canonical_word(name), lambda_keywords.get()))
                .map_err(|error| EvalError::out_of_memory(0, error))?;
            lambda_keywords = self.root_slot(pair);
        }
        self.heap.set_symbol_cell(
            canonical_word("LAMBDA-LIST-KEYWORDS"),
            false,
            lambda_keywords.get(),
        );
        vm::format::pprint::initialize_logical_block_depth(self)?;
        self.packages_initialized.set(true);
        // These bundled definitions call back into the reader and VM before the
        // first host form runs. Interrupting that installation leaves half a
        // language environment behind. The host's flag and budget stay intact
        // and apply as soon as execution reaches the host form.
        self.installing_builtins.set(true);
        let _installation = DropGuard::new(&self.installing_builtins, |flag| flag.set(false));
        self.initialize_method_combination()?;
        vm::modules::initialize(self)?;
        vm::methods::initialize(self)?;
        vm::mop::initialize(self)?;
        vm::instances::initialize_load_forms(self)?;
        vm::instances::initialize_documentation_protocol(self)?;
        vm::signaling::initialize(self)?;
        vm::instances::initialize_function_keywords(self)?;
        vm::initialize_function_introspection(self)?;
        compile::declarations::initialize(self)?;
        self.load_builtin_source(include_str!("../lisp/runtime/compiler/vop.lisp"))?;
        vm::packages::initialize_local_nicknames(self)?;
        self.load_builtin_source(include_str!("../lisp/runtime/reader/raw-strings.lisp"))?;
        self.initialize_environment_macros()?;
        vm::initialize_interaction(self)?;
        profiling::install(self)?;
        vm::task_scheduler::initialize(self)?;
        vm::simd::initialize(self)?;
        vm::treelist::initialize(self)?;
        patterns::initialize(self)?;
        if self.foreign_calls {
            ffi::initialize(self)?;
        }
        self.builtin_native_count.set(self.natives.borrow().len());
        Ok(())
    }

    /// Install the macro definitions of TRACE and UNTRACE.
    ///
    /// Both are macros so their function-name arguments are not evaluated.  The
    /// expansions call the primitive implementations with quoted names.
    fn initialize_environment_macros(&self) -> Result<(), EvalError> {
        let install = |binding_name: &str, primitive: &str| -> Result<Word, EvalError> {
            let (symbol, _) = self.intern_package(Package::COMMON_LISP, binding_name, 0)?;
            let function = vm::Primitive::named(primitive)
                .ok_or_else(|| EvalError::new(0, "missing environment primitive"))?;
            self.heap.set_symbol_cell(symbol, true, function);
            Ok(symbol)
        };
        let trace_primitive_symbol = self.root_slot(install("%ZK-TRACE", "TRACE")?);
        let untrace_primitive_symbol = self.root_slot(install("%ZK-UNTRACE", "UNTRACE")?);
        let expander = |target: &RootSlot| -> Result<Root<'_>, EvalError> {
            let target = self
                .heap
                .symbol_name(target.get())
                .ok_or_else(|| EvalError::new(0, "missing TRACE primitive symbol"))?;
            let source = format!(
                "(CL:LAMBDA (CL-USER::ZK-TRACE-FORM CL-USER::ZK-TRACE-ENV) (CL:DECLARE (CL:IGNORE CL-USER::ZK-TRACE-ENV)) (CL:CONS 'CL:FUNCALL (CL:CONS (CL:LIST 'CL:FUNCTION 'COMMON-LISP::{target}) (CL:MAPCAR (CL:FUNCTION (CL:LAMBDA (CL-USER::ZK-TRACE-NAME) (CL:LIST 'CL:QUOTE CL-USER::ZK-TRACE-NAME))) (CL:CDR CL-USER::ZK-TRACE-FORM)))))"
            );
            self.eval(&source)?
                .into_iter()
                .next()
                .ok_or_else(|| EvalError::new(0, "macro expander did not produce a function"))
        };
        let trace_expander = expander(&trace_primitive_symbol)?;
        let untrace_expander = expander(&untrace_primitive_symbol)?;
        let trace = self
            .find_common_lisp("TRACE")
            .expect("standard TRACE symbol");
        let untrace = self
            .find_common_lisp("UNTRACE")
            .expect("standard UNTRACE symbol");
        self.heap.set_macro_expander(trace, trace_expander.slot.get());
        self.heap.set_macro_expander(untrace, untrace_expander.slot.get());
        Ok(())
    }

    fn initialize_method_combination(&self) -> Result<(), EvalError> {
        if self.method_combinations.get().is_none() && !self.method_combination_initializing.get() {
            let combination = {
                self.method_combination_initializing.set(true);
                let _initializing = DropGuard::new(&self.method_combination_initializing, |flag| {
                    flag.set(false);
                });
                vm::method_combination::initialize(self)?
            };
            let result = self.method_combinations.set(combination);
            debug_assert!(result.is_ok());
        }
        Ok(())
    }

    fn intern(&self, name: &Name, at: usize) -> Result<Word, EvalError> {
        if let Some(word) = self.find_named_symbol(name) {
            return Ok(word);
        }
        if read::QUASIQUOTE_MARKERS.contains(&name.operator()) {
            // A recursive read can hand part of a backquote template to a
            // Lisp reader macro. Its private markers must survive that trip.
            return Ok(self
                .allocate_symbol_with_origin(name.spelling(), None, true, at)?
                .1);
        }
        if name.package() == Some(Package::KEYWORD) {
            return Ok(self
                .intern_package(Package::KEYWORD, name.spelling(), at)?
                .0);
        }
        Err(EvalError::new(
            at,
            format!("missing compiler symbol {}", name.spelling()),
        ))
    }

    fn intern_common_lisp(&self, spelling: &str, at: usize) -> Result<Word, EvalError> {
        if let Some(word) = self.find_common_lisp(spelling) {
            return Ok(word);
        }
        self.intern(&Name::common_lisp(spelling), at)
    }

    fn find_common_lisp(&self, spelling: &str) -> Option<Word> {
        self.packages
            .borrow()
            .present(Package::COMMON_LISP, spelling)
            .map(|(word, _)| word)
    }

    fn find_named_symbol(&self, name: &Name) -> Option<Word> {
        name.rooted_word().or_else(|| {
            self.packages
                .borrow()
                .present(name.package()?, name.spelling())
                .map(|(word, _)| word)
        })
    }

    fn intern_package(
        &self,
        package: Package,
        spelling: &str,
        at: usize,
    ) -> Result<(Word, Option<SymbolStatus>), EvalError> {
        if let Some((word, status)) = self.packages.borrow().find(package, spelling) {
            return Ok((word, Some(status)));
        }
        if self.packages.borrow().name(package).is_none() {
            return Err(EvalError::read(at, "package has been deleted"));
        }
        let (_, word) = self.allocate_symbol(spelling, Some(package), at)?;
        self.packages.borrow_mut().insert(
            package,
            spelling.into(),
            word,
            package == Package::KEYWORD,
        );
        Ok((word, None))
    }

    fn allocate_symbol(
        &self,
        spelling: &str,
        home: Option<Package>,
        at: usize,
    ) -> Result<(usize, Word), EvalError> {
        self.allocate_symbol_with_origin(spelling, home, false, at)
    }

    fn integer_constant(&self, spelling: &str) -> Option<i64> {
        let (fixnum_min, fixnum_max) = self.compilation_target.fixnum_bounds();
        Some(match spelling {
            "CHAR-CODE-LIMIT" => character::CODE_LIMIT as i64,
            "MOST-POSITIVE-FIXNUM" => fixnum_max,
            "MOST-NEGATIVE-FIXNUM" => fixnum_min,
            "INTERNAL-TIME-UNITS-PER-SECOND" => 1_000_000_000,
            "ARRAY-RANK-LIMIT" => vm::arrays::RANK_LIMIT as i64,
            "ARRAY-DIMENSION-LIMIT"
            | "ARRAY-TOTAL-SIZE-LIMIT"
            | "CALL-ARGUMENTS-LIMIT"
            | "LAMBDA-PARAMETERS-LIMIT"
            | "MULTIPLE-VALUES-LIMIT" => fixnum_max,
            "BOOLE-CLR" => 0,
            "BOOLE-SET" => 1,
            "BOOLE-1" => 2,
            "BOOLE-2" => 3,
            "BOOLE-C1" => 4,
            "BOOLE-C2" => 5,
            "BOOLE-AND" => 6,
            "BOOLE-IOR" => 7,
            "BOOLE-XOR" => 8,
            "BOOLE-EQV" => 9,
            "BOOLE-NAND" => 10,
            "BOOLE-NOR" => 11,
            "BOOLE-ANDC1" => 12,
            "BOOLE-ANDC2" => 13,
            "BOOLE-ORC1" => 14,
            "BOOLE-ORC2" => 15,
            _ => return None,
        })
    }

    fn allocate_symbol_with_origin(
        &self,
        spelling: &str,
        home: Option<Package>,
        canonical: bool,
        at: usize,
    ) -> Result<(usize, Word), EvalError> {
        let id = self.next_symbol_id.get();
        let next = id
            .checked_add(1)
            .ok_or_else(|| EvalError::new(at, "symbol identities exhausted"))?;
        let constant = home == Some(Package::KEYWORD)
            || canonical
                && (self.integer_constant(spelling).is_some()
                    || float_constant(spelling).is_some()
                    || spelling == "LAMBDA-LIST-KEYWORDS");
        let attributes = SymbolAttributes::new(id, home, canonical, constant);
        let word = self
            .heap
            .symbol(spelling, attributes)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        if constant {
            self.heap.set_symbol_cell(word, false, word);
        }
        self.next_symbol_id.set(next);
        Ok((id, word))
    }

    fn fresh_symbol(&self, spelling: &str, at: usize) -> Result<(usize, Word), EvalError> {
        self.allocate_symbol(spelling, None, at)
    }

    fn is_special(&self, name: &Name) -> bool {
        self.find_named_symbol(name)
            .is_some_and(|word| self.heap.is_special(word))
    }

    /// Register the expander of a derived type specifier. The function is
    /// rooted for the runtime's lifetime, like a global definition.
    pub(crate) fn define_type_expander(&self, name: Word, function: Word) {
        if let Some(attributes) = self.heap.symbol_attributes(name) {
            self.type_expanders
                .borrow_mut()
                .insert(attributes.id, self.root_slot(function));
        }
    }

    pub(crate) fn type_expander(&self, name: Word) -> Option<Word> {
        let attributes = self.heap.symbol_attributes(name)?;
        self.type_expanders
            .borrow()
            .get(&attributes.id)
            .map(|slot| slot.get())
    }

    pub(crate) fn define_setf_expander(
        &self,
        name: Word,
        expander: SetfExpander,
        documentation: Option<Word>,
    ) {
        if let Some(attributes) = self.heap.symbol_attributes(name) {
            self.setf_expanders
                .borrow_mut()
                .insert(attributes.id, expander);
            let mut docs = self.setf_docs.borrow_mut();
            if let Some(doc) = documentation {
                docs.insert(attributes.id, self.root_slot(doc));
            } else {
                docs.remove(&attributes.id);
            }
        }
    }

    pub(crate) fn setf_expander(&self, name: Word) -> Option<SetfExpanderRef> {
        let attributes = self.heap.symbol_attributes(name)?;
        self.setf_expanders
            .borrow()
            .get(&attributes.id)
            .map(|entry| match entry {
                SetfExpander::Function(slot) => SetfExpanderRef::Function(slot.get()),
                SetfExpander::Short(slot) => SetfExpanderRef::Short(slot.get()),
                SetfExpander::Long {
                    function,
                    store_count,
                    positional_count,
                    has_keys,
                } => SetfExpanderRef::Long {
                    function: function.get(),
                    store_count: *store_count,
                    positional_count: *positional_count,
                    has_keys: *has_keys,
                },
            })
    }

    pub(crate) fn setf_documentation(&self, name: Word) -> Word {
        self.heap
            .symbol_attributes(name)
            .and_then(|attributes| {
                self.setf_docs
                    .borrow()
                    .get(&attributes.id)
                    .map(|slot| slot.get())
            })
            .unwrap_or(Word::NIL)
    }

    pub(crate) fn set_setf_documentation(&self, name: Word, doc: Word) {
        if let Some(attributes) = self.heap.symbol_attributes(name) {
            let mut docs = self.setf_docs.borrow_mut();
            if doc == Word::NIL {
                docs.remove(&attributes.id);
            } else {
                docs.insert(attributes.id, self.root_slot(doc));
            }
        }
    }

    pub(crate) fn global_compiler_macro(&self, name: &Name, is_setf: bool) -> Option<Root<'_>> {
        let word = self.find_named_symbol(name)?;
        let attributes = self.heap.symbol_attributes(word)?;
        self.compiler_macros
            .borrow()
            .get(&(attributes.id, is_setf))
            .map(|slot| self.root(slot.get()))
    }

    pub(crate) fn compiler_macro_function(
        &self,
        name: Word,
        is_setf: bool,
        environment: Word,
        at: usize,
    ) -> Result<Option<Root<'_>>, EvalError> {
        if let Some(name_obj) = form_value::name_of_word(self, name) {
            let shadowed = if is_setf {
                macros::local_setter_binding(self, &name_obj, environment, at)?
            } else {
                macros::local_operator_binding(self, &name_obj, environment, at)?.is_some()
            };
            if shadowed {
                return Ok(None);
            }
        } else {
            macros::validate_environment(self, environment, at)?;
        }
        Ok(self.heap.symbol_attributes(name).and_then(|attributes| {
            self.compiler_macros
                .borrow()
                .get(&(attributes.id, is_setf))
                .map(|slot| self.root(slot.get()))
        }))
    }

    pub(crate) fn set_compiler_macro_function(
        &self,
        name: Word,
        is_setf: bool,
        function: Option<Word>,
    ) {
        if let Some(attributes) = self.heap.symbol_attributes(name) {
            let mut map = self.compiler_macros.borrow_mut();
            if let Some(function) = function {
                map.insert((attributes.id, is_setf), self.root_slot(function));
            } else {
                map.remove(&(attributes.id, is_setf));
            }
        }
    }

    pub(crate) fn symbol_plist(&self, symbol: Word) -> Word {
        self.heap
            .symbol_attributes(symbol)
            .and_then(|attributes| {
                self.symbol_plists
                    .borrow()
                    .get(&attributes.id)
                    .map(|slot| slot.get())
            })
            .unwrap_or(Word::NIL)
    }

    pub(crate) fn set_symbol_plist(&self, symbol: Word, plist: Word) {
        if let Some(attributes) = self.heap.symbol_attributes(symbol) {
            let mut plists = self.symbol_plists.borrow_mut();
            if plist == Word::NIL {
                plists.remove(&attributes.id);
            } else {
                plists.insert(attributes.id, self.root_slot(plist));
            }
        }
    }

    pub(crate) fn proclaimed_inline_declaration(
        &self,
        name: &Name,
        setf: bool,
    ) -> Option<core_ir::InlineDeclaration> {
        let attributes = self.heap.symbol_attributes(self.find_named_symbol(name)?)?;
        self.proclaimed_inline
            .borrow()
            .get(&(attributes.id, setf))
            .copied()
    }

    pub(crate) fn proclaim_special(&self, symbol: Word, at: usize) -> Result<(), condition::Fault> {
        if self.heap.symbol_macro_expander(symbol).is_some() {
            return Err(condition::Fault::plain(
                condition::Class::ProgramError,
                EvalError::new(at, "a symbol macro cannot be proclaimed special"),
            ));
        }
        self.heap.proclaim_special(symbol);
        Ok(())
    }

    pub(crate) fn proclaim_inline(
        &self,
        word: Word,
        setf: bool,
        declaration: core_ir::InlineDeclaration,
    ) {
        if let Some(attributes) = self.heap.symbol_attributes(word) {
            self.proclaimed_inline
                .borrow_mut()
                .insert((attributes.id, setf), declaration);
        }
    }

    pub(crate) fn inline_def(&self, name: &Name) -> Option<Rc<core_ir::CoreFn>> {
        self.inline_defs.borrow().get(name.key()).cloned()
    }

    pub(crate) fn record_inline_def(&self, name: &Name, core: Rc<core_ir::CoreFn>) {
        self.inline_defs
            .borrow_mut()
            .insert(name.key().clone(), core);
    }

    pub(crate) fn clear_inline_def(&self, name: &Name) {
        self.inline_defs.borrow_mut().remove(name.key());
    }

    pub fn nil(&self) -> Root<'_> {
        self.root(Word::NIL)
    }

    pub fn t(&self) -> Root<'_> {
        self.root(Word::T)
    }

    /// Return `None` if `value` does not fit in a fixnum.
    pub fn fixnum(&self, value: isize) -> Option<Root<'_>> {
        Word::fixnum(value).map(|word| self.root(word))
    }

    /// Copy a Lisp integer, whether immediate or boxed, into an owned Rust value.
    pub fn integer(&self, value: &Root<'_>) -> Option<BigInt> {
        if !value.belongs_to(self) {
            return None;
        }
        self.heap.integer(value.slot.get())
    }

    /// Create a Lisp integer, using an immediate fixnum whenever it fits.
    pub fn make_integer(&self, value: &BigInt) -> Result<Root<'_>, OutOfMemory> {
        self.heap
            .allocate_integer(value)
            .map(|word| self.root(word))
    }

    /// Construct an integer from a host primitive. Values in the fixnum range
    /// take the immediate path without constructing an intermediate bignum.
    pub fn integer_from<T: Copy + TryInto<isize> + Into<BigInt>>(
        &self,
        value: T,
    ) -> Result<Root<'_>, OutOfMemory> {
        match value.try_into().ok().and_then(Word::fixnum) {
            Some(word) => Ok(self.root(word)),
            None => self.make_integer(&value.into()),
        }
    }

    /// Copy Rust text into a Lisp string.
    pub fn make_string(&self, text: &str) -> Result<Root<'_>, OutOfMemory> {
        self.heap.string(text).map(|word| self.root(word))
    }

    /// Copy Rust bytes into a specialized Lisp octet vector.
    pub fn make_octets(&self, bytes: &[u8]) -> Result<Root<'_>, OutOfMemory> {
        self.heap.octet_vector(bytes).map(|word| self.root(word))
    }

    /// Copy the active contents of a Lisp octet vector, respecting displacement.
    pub fn octets(&self, value: &Root<'_>) -> Option<Vec<u8>> {
        value.belongs_to(self).then_some(())?;
        self.heap.octet_vector_contents(value.slot.get())
    }

    /// Construct a Lisp SINGLE-FLOAT.
    pub fn make_single_float(&self, value: f32) -> Result<Root<'_>, OutOfMemory> {
        self.heap
            .allocate_number(&number::Number::Real(number::Real::Single(value)))
            .map(|word| self.root(word))
    }

    /// Construct a Lisp DOUBLE-FLOAT.
    pub fn make_double_float(&self, value: f64) -> Result<Root<'_>, OutOfMemory> {
        self.heap
            .allocate_number(&number::Number::Real(number::Real::Double(value)))
            .map(|word| self.root(word))
    }

    /// Construct a Lisp character from a Unicode scalar.
    pub fn character(&self, value: char) -> Root<'_> {
        self.root(Word::character(value))
    }

    /// Construct a cons from two values belonging to this runtime.
    ///
    /// # Panics
    /// Panics if either value belongs to a different runtime.
    pub fn cons(&self, car: &Root<'_>, cdr: &Root<'_>) -> Result<Root<'_>, OutOfMemory> {
        assert!(
            car.belongs_to(self) && cdr.belongs_to(self),
            "cannot store a value from a different runtime in a cons"
        );
        self.heap
            .cons(|| (car.slot.get(), cdr.slot.get()))
            .map(|word| self.root(word))
    }

    /// Read the two fields of a cons. Other values have no cons fields.
    pub fn cons_fields(&self, value: &Root<'_>) -> Option<(Root<'_>, Root<'_>)> {
        if !value.belongs_to(self) {
            return None;
        }
        self.heap
            .cons_fields(value.slot.get())
            .map(|(car, cdr)| (self.root(car), self.root(cdr)))
    }

    /// Read the current heap statistics without triggering a collection.
    pub fn heap_stats(&self) -> HeapStats {
        self.heap.stats()
    }

    /// Request a collection and wait for it to finish. Lisp finalizers discovered
    /// by the collector run at the next outer host execution or explicit Lisp GC.
    pub fn collect(&self) {
        self.heap.collect();
        self.sweep_dead_foreigns();
    }

    fn root(&self, word: Word) -> Root<'_> {
        Root {
            slot: self.heap.root_for_host(word),
            owner: self,
        }
    }
}

fn float_constant(name: &str) -> Option<number::Real> {
    use number::Real;

    macro_rules! constants {
        ($width:literal, $variant:ident, $ty:ty) => {{
            let value = match name {
                concat!("MOST-POSITIVE-", $width, "-FLOAT") => <$ty>::MAX,
                concat!("MOST-NEGATIVE-", $width, "-FLOAT") => -<$ty>::MAX,
                concat!("LEAST-POSITIVE-", $width, "-FLOAT") => <$ty>::from_bits(1),
                concat!("LEAST-NEGATIVE-", $width, "-FLOAT") => -<$ty>::from_bits(1),
                concat!("LEAST-POSITIVE-NORMALIZED-", $width, "-FLOAT") => <$ty>::MIN_POSITIVE,
                concat!("LEAST-NEGATIVE-NORMALIZED-", $width, "-FLOAT") => -<$ty>::MIN_POSITIVE,
                concat!($width, "-FLOAT-EPSILON") => (<$ty>::EPSILON / 2.0).next_up(),
                concat!($width, "-FLOAT-NEGATIVE-EPSILON") => (<$ty>::EPSILON / 4.0).next_up(),
                _ => return None,
            };
            Some(Real::$variant(value))
        }};
    }

    if name == "PI" {
        Some(Real::Double(std::f64::consts::PI))
    } else if name.contains("SINGLE-FLOAT") {
        constants!("SINGLE", Single, f32)
    } else if name.contains("SHORT-FLOAT") {
        constants!("SHORT", Single, f32)
    } else if name.contains("DOUBLE-FLOAT") {
        constants!("DOUBLE", Double, f64)
    } else if name.contains("LONG-FLOAT") {
        constants!("LONG", Double, f64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_aot_fasl_error_retains_decode_cause() {
        // SAFETY: There are no native entries and the runtime validates the FASL.
        static INVALID_MODULE: native::AotModuleDescriptor = unsafe {
            native::AotModuleDescriptor::new(
                "invalid-module",
                native::AbiFingerprint::CURRENT,
                Some(b"invalid fasl"),
                &[],
            )
        };
        let runtime = Runtime::start().unwrap();
        let error = runtime.load_aot_module(&INVALID_MODULE).unwrap_err();
        assert_eq!(error.message(), "invalid zkcl compiled-file magic");
        assert_eq!(error.source_range(), None);
        assert!(error.contexts().iter().any(|context| matches!(
            context,
            EvalErrorContext::EmbeddedModule(name) if name == "invalid-module"
        )));
        assert!(matches!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<zkcl_bytecode::CompiledFileError>()),
            Some(zkcl_bytecode::CompiledFileError::InvalidMagic)
        ));
        assert!(runtime.aot_modules.borrow().is_empty());
    }

    #[test]
    fn incremental_completion_sweeps_collected_foreign_values() {
        let runtime = Runtime::start().unwrap();
        runtime
            .register_function("collect-incremental-foreign", |runtime, _| {
                runtime.heap.collect();
                Ok(vec![runtime.nil()])
            })
            .unwrap();
        let target = runtime.eval("'collect-incremental-foreign").unwrap();
        let owner = Rc::new(());
        let weak = Rc::downgrade(&owner);
        drop(runtime.foreign(owner).unwrap());

        let mut evaluation = runtime.start_call(&target[0], &[]).unwrap();
        assert!(matches!(
            evaluation.resume(None).unwrap(),
            EvaluationPoll::Ready(_)
        ));
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn panic_defers_foreign_destructors_until_a_regular_host_exit() {
        let runtime = Runtime::start().unwrap();
        runtime
            .register_function("collect-and-panic", |runtime, _| {
                runtime.heap.collect();
                panic!("host callback panic");
            })
            .unwrap();
        let target = runtime.eval("'collect-and-panic").unwrap();
        let owner = Rc::new(());
        let weak = Rc::downgrade(&owner);
        drop(runtime.foreign(owner).unwrap());

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.call(&target[0], &[])
        }));
        assert!(panic.is_err());
        assert!(weak.upgrade().is_some());
        assert!(runtime.eval("nil").unwrap()[0].is_nil());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn errors_report_host_abort_during_condition_rendering() {
        let runtime = Runtime::start().unwrap();
        let cancellation = runtime.cancellation_handle();
        runtime
            .register_function("abort-report", |runtime, _| {
                runtime.cancellation_handle().cancel();
                Ok(Vec::new())
            })
            .unwrap();
        runtime
            .load_str(
                "(define-condition custom-error (error) ()
                   (:report (lambda (condition stream)
                              (declare (ignore condition))
                              (abort-report)
                              (write-string \"custom report\" stream))))
                 (defun simple-error-report (stream)
                   (abort-report)
                   (write-string \"simple report\" stream))",
            )
            .unwrap();

        for source in ["(error 'custom-error)", "(error #'simple-error-report)"] {
            let error = runtime
                .eval(source)
                .err()
                .expect("condition rendering was aborted");
            assert_eq!(
                error.execution_stop(),
                Some(ExecutionStop::Aborted),
                "{source}"
            );
            cancellation.reset();
        }
    }

    #[test]
    fn host_function_capture_survives_collection_until_the_active_call_returns() {
        let mut config = Heap::builder_with_heap_size(16 << 20);
        config.always_defrag = true;
        let rt = Runtime::with_heap(Heap::start(config).unwrap());
        let name = rt.eval("'bound-host").unwrap().pop().unwrap();
        let captured_owner = Rc::new(());
        let captured_weak = Rc::downgrade(&captured_owner);
        let foreign = rt.foreign(captured_owner).unwrap();
        let capture = rt.cons(&foreign, &rt.nil()).unwrap();
        let callback_owner = Arc::new(());
        let callback_weak = Arc::downgrade(&callback_owner);
        let captured_during_call = captured_weak.clone();
        let function = rt
            .host_function(&capture, move |rt, args, _at| {
                if args.is_empty() {
                    return Ok(None);
                }
                let before = args.get(0);
                rt.eval("(fmakunbound 'bound-host)")?;
                rt.collect();
                assert!(captured_during_call.upgrade().is_some());
                assert_eq!(Arc::strong_count(&callback_owner), 1);
                assert_ne!(args.get(0), before);
                Ok(Some(args.get(0)))
            })
            .unwrap();

        let predicate = rt
            .eval("(lambda (x) (and (functionp x) (typep x 'function)))")
            .unwrap()
            .pop()
            .unwrap();
        assert!(!rt.call(&predicate, &[&function]).unwrap()[0].is_nil());
        assert!(rt.call(&predicate, &[&foreign]).unwrap()[0].is_nil());
        assert!(rt.call(&foreign, &[]).is_err());
        drop(foreign);
        assert!(rt.call(&function, &[]).unwrap().is_empty());
        let mut execution = vm::ExecState::new(vm::Values::new(&rt));
        let mut native = native::NativeContext::new(&rt, std::ptr::NonNull::from(&mut execution));
        let returned = native::native_invoke(
            &mut native,
            function.slot.get(),
            heap::RootSlots::empty(),
            0..0,
            0,
        )
        .unwrap();
        assert!(matches!(returned, native::NativeReturn::Values));
        assert_eq!(native.values_len(), 0);
        rt.heap
            .set_symbol_cell(name.slot.get(), true, function.slot.get());
        // Once the function binding is removed, this cycle must also be swept.
        rt.heap
            .set_cons_cdr(capture.slot.get(), function.slot.get());
        drop(capture);
        drop(function);
        rt.collect();
        assert!(captured_weak.upgrade().is_some());
        assert!(callback_weak.upgrade().is_some());
        assert!(rt.eval("(funcall #'bound-host)").unwrap().is_empty());
        let result = rt.eval("(apply #'bound-host (list (list 73)))").unwrap();
        assert_eq!(rt.cons_fields(&result[0]).unwrap().0.as_fixnum(), Some(73));
        rt.collect();
        assert!(captured_weak.upgrade().is_none());
        assert!(callback_weak.upgrade().is_none());
    }

    #[test]
    fn stress_collections_preserve_objects_reachable_from_runtime_roots() {
        let mut config = Heap::builder_with_heap_size(16 << 20);
        config.always_defrag = true;
        config.stress_bytes = Some(128 << 10);
        let runtime = Runtime::with_heap(Heap::start(config).unwrap());
        let nil = runtime.nil();
        let n = runtime.fixnum(-42).unwrap();
        let child = runtime.cons(&n, &nil).unwrap();
        let parent = runtime.cons(&child, &child).unwrap();
        let (old_parent, old_child) = (parent.slot.get(), child.slot.get());
        drop(child); // The child is now reachable only through the parent.

        for _ in 0..2048 {
            runtime.cons(&n, &nil).unwrap();
        }
        runtime.collect();

        let (left, right) = runtime.cons_fields(&parent).unwrap();
        assert!(left.same_identity(&right));
        assert_ne!(parent.slot.get(), old_parent);
        assert_ne!(left.slot.get(), old_child);
        let (number, tail) = runtime.cons_fields(&left).unwrap();
        assert_eq!(number.as_fixnum(), Some(-42));
        assert!(tail.is_nil());

        heap::check_hash_table_storage(&runtime.heap);
        heap::check_generic_storage(&runtime.heap);
        heap::check_number_storage(&runtime.heap);

        let (_, before) = runtime.fresh_symbol("RELOCATED-NAME", 0).unwrap();
        let name = Name::from_word(&runtime, before).unwrap();
        let before_spelling = runtime.heap.symbol_name_word(before).unwrap();
        runtime.collect();
        let after = name.rooted_word().unwrap();
        assert_ne!(after, before);
        assert_ne!(
            runtime.heap.symbol_name_word(after).unwrap(),
            before_spelling
        );
        let decoded = form_value::name_of_word(&runtime, after).unwrap();
        assert_eq!(decoded.key(), name.key());
        assert_eq!(decoded.spelling(), "RELOCATED-NAME");

        let copies = runtime.heap.relocations();
        let too_large = "x".repeat(17 << 20);
        assert!(matches!(runtime.heap.string(&too_large), Err(OutOfMemory)));
        assert_eq!(runtime.heap.relocations(), copies);
        drop(too_large);

        // Now let an allocation trigger GC. Its new cons must use the forwarded
        // value of the argument rather than the address it had before allocation.
        let old_parent = parent.slot.get();
        let allocated = (0..16_384)
            .find_map(|_| {
                let pair = runtime.cons(&parent, &nil).unwrap();
                (parent.slot.get() != old_parent).then_some(pair)
            })
            .expect("allocation must trigger an Immix relocation");
        let (car, cdr) = runtime.cons_fields(&allocated).unwrap();
        assert!(car.same_identity(&parent));
        assert!(cdr.is_nil());
        // Self-hosted per-runtime Immix allows starting independent runtimes in the same process.
        {
            let second = Runtime::start().expect("second independent runtime starts cleanly");
            assert_eq!(second.eval("(+ 20 22)").unwrap()[0].as_fixnum(), Some(42));
        }

        // These conses live in VM registers, with no Rust root per register.
        let copies = runtime.heap.relocations();
        let source = format!(
            "(let ((x (list 24)) (y (list 18)))
               (multiple-value-bind (a b) (values x y)
                 (list {})
                 (values (+ (car a) (car b)) a x)))",
            " 0".repeat(12_000),
        );
        let values = runtime.eval(&source).unwrap();
        assert!(runtime.heap.relocations() > copies);
        assert_eq!(values[0].as_fixnum(), Some(42));
        assert!(values[1].same_identity(&values[2]));

        let source = format!("({})", " (\"雪\" . 7)".repeat(6_000));
        let form = read::read(&runtime, &source).unwrap();
        let copies = runtime.heap.relocations();
        let mut list = form_value::to_value(&runtime, &form).unwrap();
        assert!(runtime.heap.relocations() > copies);
        for _ in 0..6_000 {
            let (entry, rest) = runtime.cons_fields(&list).unwrap();
            let (text, tail) = runtime.cons_fields(&entry).unwrap();
            assert_eq!(runtime.string(&text).as_deref(), Some("雪"));
            assert_eq!(tail.as_fixnum(), Some(7));
            list = rest;
        }
        assert!(list.is_nil());

        runtime
            .load_str("(defun constant-pair () '(7 . 9))")
            .unwrap();
        let code = runtime.eval("#'constant-pair").unwrap().pop().unwrap();
        let before = code.slot.get();
        // The pair constant is reachable only through the managed function object.
        runtime.collect();
        assert_ne!(before, code.slot.get());
        let result = runtime.call(&code, &[]).unwrap().pop().unwrap();
        let (car, cdr) = runtime.cons_fields(&result).unwrap();
        assert_eq!((car.as_fixnum(), cdr.as_fixnum()), (Some(7), Some(9)));
        assert!(result.same_identity(&runtime.call(&code, &[]).unwrap()[0]));

        // Neither the lexical cell nor the list inside it has a separate Rust root.
        let bank = runtime
            .eval("(let ((x (list 71))) (values (lambda () x) (lambda (new) (setq x new))))")
            .unwrap();
        let before = bank[0].slot.get();
        runtime.collect();
        assert_ne!(before, bank[0].slot.get());
        let captured = runtime.call(&bank[0], &[]).unwrap().pop().unwrap();
        assert_eq!(
            runtime.cons_fields(&captured).unwrap().0.as_fixnum(),
            Some(71)
        );
        runtime.call(&bank[1], &[&n]).unwrap();
        assert_eq!(
            runtime.call(&bank[0], &[]).unwrap()[0].as_fixnum(),
            Some(-42)
        );

        runtime
            .register_function("rooted-native", |rt, args| {
                let before = args[0].slot.get();
                rt.collect();
                assert_ne!(before, args[0].slot.get());
                Ok(vec![args[0].clone()])
            })
            .unwrap();
        let result = runtime.eval("(rooted-native (list 74))").unwrap();
        assert_eq!(
            runtime.cons_fields(&result[0]).unwrap().0.as_fixnum(),
            Some(74)
        );

        runtime
            .register_function("collect-cleanup", |rt, _| {
                let before = rt.heap.relocations();
                rt.collect();
                assert!(rt.heap.relocations() > before);
                Ok(vec![rt.nil()])
            })
            .unwrap();
        let saved = runtime
            .eval(
                "(let ((tag (list 17)))
               (catch tag
                 (unwind-protect (throw tag (values (list 93) 14))
                   (collect-cleanup)
                   (values 5 6))))",
            )
            .unwrap();
        assert_eq!(
            runtime.cons_fields(&saved[0]).unwrap().0.as_fixnum(),
            Some(93)
        );
        assert_eq!(saved[1].as_fixnum(), Some(14));

        runtime
            .load_str("(defparameter *saved-dynamic* (list 83))")
            .unwrap();
        let original = runtime.eval("*saved-dynamic*").unwrap().pop().unwrap();
        let before = original.slot.get();
        drop(original); // After rebinding, the old object is reachable only from the saved binding.
        let temporary = runtime
            .eval(
                "(let ((*saved-dynamic* (list 29)))
               (collect-cleanup)
               *saved-dynamic*)",
            )
            .unwrap();
        assert_eq!(
            runtime.cons_fields(&temporary[0]).unwrap().0.as_fixnum(),
            Some(29)
        );
        let restored = runtime.eval("*saved-dynamic*").unwrap().pop().unwrap();
        assert_ne!(before, restored.slot.get());
        assert_eq!(
            runtime.cons_fields(&restored).unwrap().0.as_fixnum(),
            Some(83)
        );

        let condition = runtime
            .eval(
                "(handler-case
                 (unwind-protect
                     (handler-bind ((simple-error (lambda (c) (collect-cleanup))))
                       (error \"retained condition\" (list 96)))
                   (collect-cleanup))
               (simple-error (c)
                 (values (simple-condition-format-control c)
                         (car (simple-condition-format-arguments c)))))",
            )
            .unwrap();
        assert_eq!(
            runtime.string(&condition[0]).as_deref(),
            Some("retained condition")
        );
        assert_eq!(
            runtime.cons_fields(&condition[1]).unwrap().0.as_fixnum(),
            Some(96)
        );

        let resumed = runtime
            .eval(
                "(restart-case
                 (unwind-protect (invoke-restart 'use (list 73)) (collect-cleanup))
               (use (arg)
                 :test (lambda (condition) (collect-cleanup) t)
                 arg))",
            )
            .unwrap();
        assert_eq!(
            runtime.cons_fields(&resumed[0]).unwrap().0.as_fixnum(),
            Some(73)
        );

        let keyword = runtime
            .eval(
                "(handler-case (values :payload (list 82))
               (:no-error (&key (cleanup (collect-cleanup)) payload) payload))",
            )
            .unwrap();
        assert_eq!(
            runtime.cons_fields(&keyword[0]).unwrap().0.as_fixnum(),
            Some(82)
        );

        runtime
            .load_str(
                "(defmacro expand-local (&environment env)
                   (macroexpand-1 '(checked 7) env))
                 (setq *macroexpand-hook*
                       (lambda (expander form env)
                         (collect-cleanup)
                         (funcall expander form env)))",
            )
            .unwrap();
        let expanded = runtime
            .eval("(macrolet ((checked (n) (list '+ n 13))) (expand-local))")
            .unwrap();
        assert_eq!(expanded[0].as_fixnum(), Some(20));

        // Heap pressure while an object is rooted is recoverable once it is dropped.
        let large = "x".repeat(9 << 20);
        let retained = runtime.root(runtime.heap.string(&large).unwrap());
        assert!(matches!(runtime.heap.string(&large), Err(OutOfMemory)));
        assert!(runtime.heap.is_string(retained.slot.get()));
        drop(retained);
        assert!(runtime.heap.string(&large).is_ok());
    }
}
