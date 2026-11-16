//! Platform-specific abstractions for clocks, entropy, thread parking, and OS filesystem probes.

mod clock;
mod fs;
mod sync;

pub(crate) use clock::{Timer, sleep_host, sys};
pub(crate) use fs::file_owner_name;
#[cfg(not(target_family = "wasm"))]
pub(crate) use fs::open_regular_file_nonblocking;
pub(crate) use sync::{ExecutionWake, SleepSignal};
