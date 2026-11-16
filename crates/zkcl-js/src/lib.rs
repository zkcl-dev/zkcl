//! Compile validated zkcl bytecode to an ECMAScript module with an embedded or
//! separately delivered JavaScript runtime.
//!
//! Each Lisp function has a resumable JavaScript step with a PC switch and
//! direct runtime operation calls. Eligible functions also have a structured
//! entry for unbudgeted execution. These entries execute without an instruction
//! decoder. Resumable steps retain source PCs, and source snapshots with debug
//! locations produce an inline Source Map.
//!
//! Generated source is written to be read: named step functions, one instruction
//! per `case`, comments naming the Lisp objects an instruction refers to, and
//! data objects laid out one field per line. `js_source` owns those layout
//! rules, `emit` the program structure, and `structured` the lowering of
//! non-suspending functions.

#[cfg(test)]
mod bundle;
mod emit;
mod js_source;
mod source_map;
mod structured;

pub use emit::{
    EmitError, emit_compiled_file, emit_compiled_files, emit_compiled_files_with_runtime,
    emit_project, emit_project_with_runtime, emit_runtime_module,
};
