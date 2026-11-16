// Cargo compiles each integration test as a separate crate, and each uses a
// subset of this shared support.
#![allow(dead_code)]

pub mod files;
pub mod lisp;
