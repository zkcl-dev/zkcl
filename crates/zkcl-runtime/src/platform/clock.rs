use std::time::Duration;

use crate::EvalError;
use crate::host::HostServices;

pub(crate) use imp::{Timer, sys};

/// Sleep on the current thread or delegate to the host service on bare WebAssembly.
pub(crate) fn sleep_host(
    host: &dyn HostServices,
    duration: Duration,
    at: usize,
) -> Result<(), EvalError> {
    sys::sleep_host(host, duration).map_err(|error| EvalError::io_error(at, error))
}

#[cfg(not(all(target_family = "wasm", any(target_os = "unknown", target_os = "none"))))]
mod imp {
    use std::io;
    use std::sync::LazyLock;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use crate::host::HostServices;

    /// Monotonic timer backed by `std::time::Instant`.
    pub(crate) struct Timer {
        started: Instant,
    }

    impl Timer {
        #[inline]
        pub(crate) fn start() -> Self {
            Self {
                started: Instant::now(),
            }
        }

        #[inline]
        pub(crate) fn elapsed(&self) -> Option<Duration> {
            Some(self.started.elapsed())
        }
    }

    /// System clock, monotonic timer, and entropy helpers for host services.
    pub(crate) mod sys {
        use super::*;

        pub(crate) fn wall_time_secs() -> io::Result<i64> {
            Ok(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64)
        }

        pub(crate) fn monotonic_nanos() -> io::Result<u64> {
            static START: LazyLock<Instant> = LazyLock::new(Instant::now);
            let nanos = START.elapsed().as_nanos();
            Ok(nanos.min(u128::from(u64::MAX)) as u64)
        }

        pub(crate) fn fill_entropy(bytes: &mut [u8]) -> io::Result<()> {
            getrandom::fill(bytes).map_err(io::Error::other)
        }

        pub(crate) fn sleep_host(_host: &dyn HostServices, duration: Duration) -> io::Result<()> {
            std::thread::sleep(duration);
            Ok(())
        }
    }
}

#[cfg(all(target_family = "wasm", any(target_os = "unknown", target_os = "none")))]
mod imp {
    use std::io;
    use std::time::Duration;

    use crate::host::HostServices;

    /// No-op timer on bare WebAssembly targets without a host clock.
    pub(crate) struct Timer;

    impl Timer {
        #[inline]
        pub(crate) fn start() -> Self {
            Self
        }

        #[inline]
        pub(crate) fn elapsed(&self) -> Option<Duration> {
            None
        }
    }

    /// System clock, monotonic timer, and entropy stubs on bare WebAssembly.
    pub(crate) mod sys {
        use super::*;

        pub(crate) fn wall_time_secs() -> io::Result<i64> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no host wall clock",
            ))
        }

        pub(crate) fn monotonic_nanos() -> io::Result<u64> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no host monotonic clock",
            ))
        }

        pub(crate) fn fill_entropy(_bytes: &mut [u8]) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no host entropy source",
            ))
        }

        pub(crate) fn sleep_host(host: &dyn HostServices, duration: Duration) -> io::Result<()> {
            host.sleep(duration)
        }
    }
}
