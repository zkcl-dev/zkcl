use crate::backend::{CompiledRevision, DirectJitBackend, jit_function_from_mapping, worker_count};
use crate::compiler::{JitCompiler, host_isa};
use crate::cranelift_backend::prepare_request;
use crate::{JitError, MappedJitFunction};
use zkcl_runtime::native::{
    JitCEntryFn, JitCompilationRequest, JitFunction, LeafGuard, LeafPrimitive, RegisterLeafFn,
    zkcl_register_leaf_dispatch,
};
use zkcl_vop::{LeafFunction, Target};

pub(crate) fn select_leaf(request: &JitCompilationRequest, target: Target) -> Option<LeafFunction> {
    zkcl_vop::select_bytecode(request.bytecode(), target.word_bits(), |symbol| {
        LeafPrimitive::named((*request.cl_const_names.get(symbol)?)?)
    })
    .ok()
}

/// The register entry and generic entry are owned by the same mapping.
pub(crate) struct RegisterCode {
    offset: usize,
    guards: Box<[LeafGuard]>,
}

impl RegisterCode {
    pub(crate) fn append(
        function: &LeafFunction,
        target: Target,
        code: &mut Vec<u8>,
    ) -> Option<Self> {
        let bytes = function.emit_guarded(target)?;
        let offset = code.len().next_multiple_of(16);
        code.resize(offset, 0);
        code.extend_from_slice(&bytes);
        Some(Self {
            offset,
            guards: function.guards().into(),
        })
    }

    pub(crate) fn attach(self, function: &mut JitFunction, mapping: &MappedJitFunction) {
        // SAFETY: `append` emitted a register entry at this offset and the
        // completion owns the mapping used for both entries.
        function.leaf_entry = Some(unsafe {
            let address = mapping.executable_code.as_ptr().add(self.offset);
            std::mem::transmute::<*const u8, RegisterLeafFn>(address)
        });
        function.leaf_guards = self.guards;
    }
}

impl DirectJitBackend {
    /// Start a background compiler for the host's direct machine-code target.
    pub fn new() -> Result<Self, JitError> {
        let triple = target_lexicon::Triple::host();
        if cfg!(all(target_vendor = "apple", not(target_os = "macos"))) {
            return Err(JitError::UnsupportedTarget(triple.to_string()));
        }
        let target = Target::from_triple(&triple)
            .ok_or_else(|| JitError::UnsupportedTarget(triple.to_string()))?;
        Self::start_workers(target, worker_count(), false, false, |target| {
            // The shared SSA compiler's host ISA and codegen contexts are worker
            // state: build them once, not once per retained debug compile.
            let debug_isa = host_isa().ok();
            let mut debug_compiler = JitCompiler::new();
            move |request, _, _, cache, is_pending| {
                // The direct scalar emitter has no calls or safepoints. Retained
                // debug code uses the shared SSA compiler while still executing
                // native machine code, with the same debugger ABI as other tiers.
                if request.native_debug {
                    let isa = debug_isa.as_deref()?;
                    let function = prepare_request(&request, false).ok()?;
                    let machine = debug_compiler.compile(&function, isa).ok()?;
                    if !is_pending() {
                        return None;
                    }
                    let mapping = cache
                        .allocate(request.code_id, request.revision, &machine.bytes, None)
                        .ok()?;
                    let function = jit_function_from_mapping(&mapping, machine.root_slots, true);
                    return Some(CompiledRevision { mapping, function });
                }
                let leaf_function = select_leaf(&request, target)?;
                if !is_pending() {
                    return None;
                }
                let (mut bytes, dispatcher): (_, Option<JitCEntryFn>) =
                    if !leaf_function.needs_dispatch() {
                        (leaf_function.emit_entry(target)?, None)
                    } else {
                        (Vec::new(), Some(zkcl_register_leaf_dispatch))
                    };
                let register_code = RegisterCode::append(&leaf_function, target, &mut bytes)?;
                if !is_pending() {
                    return None;
                }
                let mapping = cache
                    .allocate(request.code_id, request.revision, &bytes, dispatcher)
                    .ok()?;
                let mut function = jit_function_from_mapping(&mapping, 0, false);
                function.resumable = false;
                register_code.attach(&mut function, &mapping);
                Some(CompiledRevision { mapping, function })
            }
        })
    }
}
