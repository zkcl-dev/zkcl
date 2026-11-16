use std::sync::Mutex;

use super::Address;

pub(super) const GRAY_PACKET_CAPACITY: usize = 128;

/// Fixed-capacity batch of marked object addresses for cache-friendly parallel GC tracing.
pub(super) struct GrayPacket {
    entries: [Address; GRAY_PACKET_CAPACITY],
    len: usize,
}

impl GrayPacket {
    pub(super) fn new() -> Self {
        Self {
            entries: [Address::from_usize(0); GRAY_PACKET_CAPACITY],
            len: 0,
        }
    }

    #[inline(always)]
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline(always)]
    pub(super) fn is_full(&self) -> bool {
        self.len == GRAY_PACKET_CAPACITY
    }

    #[inline(always)]
    pub(super) fn push(&mut self, addr: Address) -> bool {
        if self.is_full() {
            return false;
        }
        self.entries[self.len] = addr;
        self.len += 1;
        true
    }

    #[inline(always)]
    pub(super) fn pop(&mut self) -> Option<Address> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        Some(self.entries[self.len])
    }
}

/// Shared pool of full and empty gray packets for parallel GC tracing workers.
#[derive(Default)]
pub(super) struct GrayPacketPool {
    full: Mutex<Vec<GrayPacket>>,
    empty: Mutex<Vec<GrayPacket>>,
}

impl GrayPacketPool {
    pub(super) fn push_full(&self, packet: GrayPacket) {
        if packet.is_empty() {
            self.push_empty(packet);
        } else {
            self.full.lock().unwrap().push(packet);
        }
    }

    pub(super) fn pop_full(&self) -> Option<GrayPacket> {
        self.full.lock().unwrap().pop()
    }

    pub(super) fn push_empty(&self, mut packet: GrayPacket) {
        packet.len = 0;
        self.empty.lock().unwrap().push(packet);
    }

    pub(super) fn pop_empty(&self) -> GrayPacket {
        self.empty
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(GrayPacket::new)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    #[test]
    fn parallel_gray_packet_pool_work_stealing_preserves_all_entries() {
        const PRODUCERS: usize = 4;
        const ITEMS_PER_PRODUCER: usize = 512;

        let pool = Arc::new(GrayPacketPool::default());
        let producers_done = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(PRODUCERS + 4));
        let consumed = Arc::new(Mutex::new(Vec::with_capacity(PRODUCERS * ITEMS_PER_PRODUCER)));

        let mut threads = Vec::new();
        for worker_id in 0..PRODUCERS {
            let pool = Arc::clone(&pool);
            let done = Arc::clone(&producers_done);
            let start = Arc::clone(&start);
            threads.push(thread::spawn(move || {
                start.wait();
                let mut packet = pool.pop_empty();
                let base = (worker_id + 1) * 10_000;
                for i in 0..ITEMS_PER_PRODUCER {
                    let addr = Address::from_usize(base + i * 8);
                    if !packet.push(addr) {
                        let full = std::mem::replace(&mut packet, pool.pop_empty());
                        pool.push_full(full);
                        assert!(packet.push(addr));
                    }
                }
                pool.push_full(packet);
                done.fetch_add(1, Ordering::Release);
            }));
        }

        for _ in 0..4 {
            let pool = Arc::clone(&pool);
            let done = Arc::clone(&producers_done);
            let start = Arc::clone(&start);
            let consumed = Arc::clone(&consumed);
            threads.push(thread::spawn(move || {
                start.wait();
                let mut local = Vec::new();
                loop {
                    if let Some(mut packet) = pool.pop_full() {
                        while let Some(addr) = packet.pop() {
                            local.push(addr.as_usize());
                        }
                        pool.push_empty(packet);
                        continue;
                    }
                    if done.load(Ordering::Acquire) == PRODUCERS {
                        if let Some(mut packet) = pool.pop_full() {
                            while let Some(addr) = packet.pop() {
                                local.push(addr.as_usize());
                            }
                            pool.push_empty(packet);
                            continue;
                        }
                        break;
                    }
                    thread::yield_now();
                }
                consumed.lock().unwrap().extend(local);
            }));
        }

        for handle in threads {
            handle.join().unwrap();
        }

        let mut results = consumed.lock().unwrap();
        results.sort_unstable();
        assert_eq!(results.len(), PRODUCERS * ITEMS_PER_PRODUCER);
        let dedup_len = {
            results.dedup();
            results.len()
        };
        assert_eq!(dedup_len, PRODUCERS * ITEMS_PER_PRODUCER);
    }
}
