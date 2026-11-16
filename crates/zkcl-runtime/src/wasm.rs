use zkcl_bytecode::CompiledFile;
use zkcl_wasm::abi::{CallFunction, CallLeaf, RegisteredEntry, decode_registrations};

use crate::{EvalError, Runtime, fasl, loader, vm};

impl Runtime {
    /// Load the FASL and the function registrations made by the Wasm linker.
    ///
    /// # Safety
    /// Each registration must name the corresponding FASL entry and agree
    /// with its function table. The bridges must obey the registered ABI,
    /// including for a function object that outlives this load. A leaf must
    /// obey its guards, return a valid Lisp word, and not allocate or reenter
    /// Lisp. A general function must keep live words rooted across safepoints.
    pub unsafe fn load_linked_fasl(
        &self,
        fasl_bytes: &[u8],
        registrations: &[u8],
        call_leaf: CallLeaf,
        call_function: CallFunction,
    ) -> Result<(), EvalError> {
        // SAFETY: the caller supplies the table/registration agreement.
        unsafe {
            self.load_linked_fasl_at(fasl_bytes, registrations, call_leaf, call_function, 0, 0)
                .map(|_| ())
        }
    }

    /// Load a separately indexed FASL into shared linked function tables.
    ///
    /// # Safety
    /// The same requirements as `load_linked_fasl` apply, with slots beginning
    /// at `leaf_base` and `function_base` in their respective tables. Returns
    /// the next unused slot in each table.
    pub unsafe fn load_linked_fasl_at(
        &self,
        fasl_bytes: &[u8],
        registrations: &[u8],
        call_leaf: CallLeaf,
        call_function: CallFunction,
        leaf_base: u32,
        function_base: u32,
    ) -> Result<(u32, u32), EvalError> {
        let compiled = CompiledFile::decode_for_target(fasl_bytes, self.compilation_target())
            .map_err(|error| EvalError::from_error(0, error))?;
        let registrations =
            decode_registrations(registrations).map_err(|error| EvalError::from_error(0, error))?;
        // Preflight both table ends before the loader assigns slots or runs code.
        let mut next_leaf = leaf_base;
        let mut next_function = function_base;
        for registration in &registrations {
            let (leaf, function) = match &registration.entry {
                RegisteredEntry::Leaf { fallback, .. } => (1, u32::from(fallback.is_some())),
                RegisteredEntry::Function(_) => (0, 1),
            };
            next_leaf = next_leaf
                .checked_add(leaf)
                .ok_or_else(|| EvalError::new(0, "too many Wasm leaves"))?;
            next_function = next_function
                .checked_add(function)
                .ok_or_else(|| EvalError::new(0, "too many Wasm functions"))?;
        }
        let _execution = self.enter_host_execution()?;
        self.initialize_packages()?;
        let loader = fasl::CompiledFileLoader::with_wasm_entries(
            self,
            compiled,
            registrations,
            call_leaf,
            call_function,
            leaf_base,
            function_base,
        );
        let contents = loader::FileContents::Compiled(Box::new(loader));
        vm::load_host(self, loader::LoadFile::in_memory(contents))?;
        Ok((next_leaf, next_function))
    }
}
