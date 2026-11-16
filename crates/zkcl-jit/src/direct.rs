use crate::JitError;
use crate::backend::{CompiledRevision, DirectJitBackend, jit_function_from_mapping, worker_count};
use zkcl_runtime::native::{
    JitCEntryFn, LeafPrimitive, RegisterLeafFn, zkcl_register_leaf_dispatch,
};
use zkcl_vop::Target;

impl DirectJitBackend {
    /// Start a background compiler for the host's direct machine-code target.
    pub fn new() -> Result<Self, JitError> {
        let triple = target_lexicon::Triple::host();
        if cfg!(all(target_vendor = "apple", not(target_os = "macos"))) {
            return Err(JitError::UnsupportedTarget(triple.to_string()));
        }
        let target = Target::from_triple(&triple)
            .ok_or_else(|| JitError::UnsupportedTarget(triple.to_string()))?;
        Self::start_workers(target, worker_count(), false, |target| {
            let mut debug_compiler = crate::compiler::JitCompiler::new();
            move |request, _, cache, is_pending| {
                // The direct scalar emitter has no calls or safepoints. Retained
                // debug code uses the shared SSA compiler while still executing
                // native machine code, with the same debugger ABI as other tiers.
                if request.native_debug {
                    let function =
                        crate::cranelift_backend::prepare_request(&request, false).ok()?;
                    let isa = crate::compiler::host_isa().ok()?;
                    let machine = debug_compiler.compile(&function, &*isa).ok()?;
                    if !is_pending() {
                        return None;
                    }
                    let mapping = cache
                        .allocate(request.code_id, request.revision, &machine.bytes, None)
                        .ok()?;
                    let function = jit_function_from_mapping(&mapping, machine.root_slots, true);
                    return Some(CompiledRevision { mapping, function });
                }
                let leaf_function =
                    zkcl_vop::select_bytecode(request.bytecode(), target.word_bits(), |symbol| {
                        LeafPrimitive::named((*request.cl_const_names.get(symbol)?)?)
                    })
                    .ok()?;
                if !is_pending() {
                    return None;
                }
                let leaf = leaf_function.emit_guarded(target)?;
                let (mut bytes, dispatcher): (_, Option<JitCEntryFn>) =
                    if !leaf_function.needs_dispatch() {
                        (leaf_function.emit_entry(target)?, None)
                    } else {
                        (Vec::new(), Some(zkcl_register_leaf_dispatch))
                    };
                let leaf_offset = bytes.len().next_multiple_of(16);
                bytes.resize(leaf_offset, 0);
                bytes.extend_from_slice(&leaf);
                if !is_pending() {
                    return None;
                }
                let mapping = cache
                    .allocate(request.code_id, request.revision, &bytes, dispatcher)
                    .ok()?;
                let mut function = jit_function_from_mapping(&mapping, 0, false);
                // The verified register body starts at this aligned offset. The same
                // revision lease owns both entries throughout each active native call.
                function.leaf_entry = Some(unsafe {
                    let address = mapping.executable_code.as_ptr().add(leaf_offset);
                    std::mem::transmute::<*const u8, RegisterLeafFn>(address)
                });
                function.leaf_guards = leaf_function.guards().into();
                Some(CompiledRevision { mapping, function })
            }
        })
    }
}
