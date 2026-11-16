//! Compile validated zkcl bytecode to an ECMAScript module with an embedded or
//! separately delivered JavaScript runtime.
//!
//! Each Lisp function has a resumable JavaScript step with a PC switch and
//! direct runtime operation calls. Eligible functions also have a structured
//! entry for unbudgeted execution. These entries execute without an instruction
//! decoder. Resumable steps retain source PCs, and source snapshots with debug
//! locations produce an inline Source Map.

mod bundle;
mod emit;
mod source_map;
mod structured;

pub use emit::{
    EmitError, emit_compiled_file, emit_compiled_files, emit_compiled_files_with_runtime,
    emit_project, emit_project_with_runtime, emit_runtime_module,
};
