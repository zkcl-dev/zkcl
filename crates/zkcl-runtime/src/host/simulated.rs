use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::HostServices;

const DEFAULT_WALL_TIME_SECS: i64 = 1_700_000_000;
const NANOS_PER_SEC: u64 = 1_000_000_000;

#[derive(Debug)]
struct SimulatedHostState {
    wall_base_secs: i64,
    monotonic_nanos: u64,
    prng_state: u64,
}

/// Deterministic, thread-safe [`HostServices`] implementation backed by a virtual
/// monotonic clock and seeded SplitMix64 entropy for deterministic simulation testing.
#[derive(Clone, Debug)]
pub struct SimulatedHostServices {
    state: Arc<Mutex<SimulatedHostState>>,
}

impl SimulatedHostServices {
    pub fn new(seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(SimulatedHostState {
                wall_base_secs: DEFAULT_WALL_TIME_SECS,
                monotonic_nanos: 0,
                prng_state: seed,
            })),
        }
    }

    pub fn now_nanos(&self) -> u64 {
        self.state.lock().unwrap().monotonic_nanos
    }

    pub fn advance(&self, duration: Duration) {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        let mut state = self.state.lock().unwrap();
        state.monotonic_nanos = state.monotonic_nanos.saturating_add(nanos);
    }

    pub fn set_wall_time_secs(&self, secs: i64) {
        let mut state = self.state.lock().unwrap();
        let elapsed_secs = (state.monotonic_nanos / NANOS_PER_SEC) as i64;
        state.wall_base_secs = secs.saturating_sub(elapsed_secs);
    }

    /// Derive a deterministic, order-independent sample seed using SplitMix64 mixing.
    pub fn split_seed(master_seed: u64, sample_index: u64) -> u64 {
        let mut mixed = master_seed
            .wrapping_mul(0xd6e8_feb8_6659_fd93)
            .wrapping_add(sample_index.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        Self::next_u64(&mut mixed)
    }

    fn next_u64(prng_state: &mut u64) -> u64 {
        *prng_state = prng_state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = *prng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

impl HostServices for SimulatedHostServices {
    fn wall_time_secs(&self) -> io::Result<i64> {
        let state = self.state.lock().unwrap();
        let elapsed_secs = (state.monotonic_nanos / NANOS_PER_SEC) as i64;
        Ok(state.wall_base_secs.saturating_add(elapsed_secs))
    }

    fn monotonic_nanos(&self) -> io::Result<u64> {
        Ok(self.now_nanos())
    }

    fn fill_entropy(&self, bytes: &mut [u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        for chunk in bytes.chunks_mut(8) {
            let word = Self::next_u64(&mut state.prng_state).to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        Ok(())
    }

    fn sleep(&self, duration: Duration) -> io::Result<()> {
        self.advance(duration);
        Ok(())
    }

    fn park(&self, duration: Duration) -> io::Result<()> {
        self.advance(duration);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: () = {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SimulatedHostServices>();
    };

    #[test]
    fn simulated_clock_and_sleep_advance_without_waiting() {
        let services = SimulatedHostServices::new(42);
        assert_eq!(services.monotonic_nanos().unwrap(), 0);
        assert_eq!(services.wall_time_secs().unwrap(), DEFAULT_WALL_TIME_SECS);

        services.sleep(Duration::from_millis(2_500)).unwrap();
        assert_eq!(services.now_nanos(), 2_500_000_000);
        assert_eq!(
            services.wall_time_secs().unwrap(),
            DEFAULT_WALL_TIME_SECS + 2
        );

        services.park(Duration::from_millis(500)).unwrap();
        assert_eq!(services.now_nanos(), 3_000_000_000);
        assert_eq!(
            services.wall_time_secs().unwrap(),
            DEFAULT_WALL_TIME_SECS + 3
        );

        services.set_wall_time_secs(100);
        assert_eq!(services.wall_time_secs().unwrap(), 100);
        services.advance(Duration::from_secs(5));
        assert_eq!(services.wall_time_secs().unwrap(), 105);
    }

    #[test]
    fn simulated_entropy_is_deterministic_per_seed() {
        let a = SimulatedHostServices::new(7);
        let b = SimulatedHostServices::new(7);
        let c = SimulatedHostServices::new(8);

        let mut buf_a = [0u8; 19];
        let mut buf_b = [0u8; 19];
        let mut buf_c = [0u8; 19];
        a.fill_entropy(&mut buf_a).unwrap();
        b.fill_entropy(&mut buf_b).unwrap();
        c.fill_entropy(&mut buf_c).unwrap();

        assert_eq!(buf_a, buf_b);
        assert_ne!(buf_a, buf_c);
        assert_eq!(
            SimulatedHostServices::split_seed(2026, 5),
            SimulatedHostServices::split_seed(2026, 5)
        );
        assert_ne!(
            SimulatedHostServices::split_seed(2026, 5),
            SimulatedHostServices::split_seed(2026, 6)
        );
    }
}
