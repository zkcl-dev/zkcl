use std::io;
use std::time::Duration;

#[cfg(not(target_family = "wasm"))]
mod blocking;
mod environment;
mod operation;
mod simulated;

#[cfg(not(target_family = "wasm"))]
pub use blocking::{BlockingPool, BlockingPoolFull};
pub use operation::HostCompletion;
pub(crate) use operation::{
    HostAsyncFunction, HostCancellation, HostError, HostEvents, HostOperation, HostReply,
};
pub use simulated::SimulatedHostServices;

/// Clocks, entropy, and timers available to Common Lisp code in one runtime.
pub trait HostServices: Send + Sync {
    /// Whole seconds since the Unix epoch.
    fn wall_time_secs(&self) -> io::Result<i64>;

    /// Nanoseconds from a host-defined, monotonic origin.
    fn monotonic_nanos(&self) -> io::Result<u64>;

    /// Fill the entire buffer with operating-system entropy or its host equivalent.
    fn fill_entropy(&self, bytes: &mut [u8]) -> io::Result<()>;

    /// Wait on a target without a system timer, such as bare WebAssembly.
    fn sleep(&self, duration: Duration) -> io::Result<()> {
        if duration.is_zero() {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::Unsupported, "no host timer"))
        }
    }

    /// Park the Lisp scheduler on a target without a system timer. The host
    /// may wake it early; the scheduler then checks cancellation and deadlines.
    fn park(&self, duration: Duration) -> io::Result<()> {
        self.sleep(duration)
    }
}

pub(crate) struct SystemServices;

impl HostServices for SystemServices {
    fn wall_time_secs(&self) -> io::Result<i64> {
        crate::platform::sys::wall_time_secs()
    }

    fn monotonic_nanos(&self) -> io::Result<u64> {
        crate::platform::sys::monotonic_nanos()
    }

    fn fill_entropy(&self, bytes: &mut [u8]) -> io::Result<()> {
        crate::platform::sys::fill_entropy(bytes)
    }
}
