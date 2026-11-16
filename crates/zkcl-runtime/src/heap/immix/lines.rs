use std::sync::atomic::{AtomicU64, Ordering};

use super::{LINE_SHIFT, LINE_SIZE, LINES_PER_BLOCK};

const WORD_BITS: usize = u64::BITS as usize;

/// One bit for each line in a block.
pub(super) struct LineMap([AtomicU64; LINES_PER_BLOCK / WORD_BITS]);

impl Default for LineMap {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicU64::new(0)))
    }
}

impl LineMap {
    #[inline]
    pub(super) fn mark_object(&self, offset: usize, size: usize) {
        let first = offset >> LINE_SHIFT;
        let last = (offset + size - 1) >> LINE_SHIFT;
        let first_word = first / WORD_BITS;
        let last_word = last / WORD_BITS;
        let first_mask = u64::MAX << (first % WORD_BITS);
        let last_mask = u64::MAX >> (WORD_BITS - 1 - last % WORD_BITS);
        if first_word == last_word {
            self.0[first_word].fetch_or(first_mask & last_mask, Ordering::Relaxed);
        } else {
            self.0[first_word].fetch_or(first_mask, Ordering::Relaxed);
            for word in &self.0[first_word + 1..last_word] {
                word.store(u64::MAX, Ordering::Relaxed);
            }
            self.0[last_word].fetch_or(last_mask, Ordering::Relaxed);
        }
    }

    pub(super) fn marked_count(&self) -> usize {
        self.0
            .iter()
            .map(|word| word.load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }

    fn next<const MARKED: bool>(&self, line: usize) -> usize {
        let start_word = line / WORD_BITS;
        for (index, atomic) in self.0.iter().enumerate().skip(start_word) {
            let occupied = atomic.load(Ordering::Relaxed);
            let mut matching = if MARKED { occupied } else { !occupied };
            if index == start_word {
                matching &= u64::MAX << (line % WORD_BITS);
            }
            if matching != 0 {
                return index * WORD_BITS + matching.trailing_zeros() as usize;
            }
        }
        LINES_PER_BLOCK
    }

    pub(super) fn free_runs(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        let mut end = 0;
        std::iter::from_fn(move || {
            let first = self.next::<false>(end);
            if first == LINES_PER_BLOCK {
                return None;
            }
            end = self.next::<true>(first);
            Some((first * LINE_SIZE, (end - first) * LINE_SIZE))
        })
    }

    fn set(&mut self, index: usize, set: bool) {
        let bit = 1 << (index % WORD_BITS);
        let word = self.0[index / WORD_BITS].get_mut();
        if set {
            *word |= bit;
        } else {
            *word &= !bit;
        }
    }
}

/// Unreserved runs indexed by their exact size in lines. The bitmap gives a
/// best fit without walking blocks whose holes are too small.
#[derive(Default)]
pub(super) struct FreeLines {
    lengths: LineMap,
    starts: Box<[Vec<usize>]>,
}

impl FreeLines {
    pub(super) fn insert(&mut self, start: usize, bytes: usize) {
        if self.starts.is_empty() {
            self.starts = (0..LINES_PER_BLOCK).map(|_| Vec::new()).collect();
        }
        let index = (bytes >> LINE_SHIFT) - 1;
        self.starts[index].push(start);
        self.lengths.set(index, true);
    }

    pub(super) fn take(&mut self, size: usize) -> Option<(usize, usize)> {
        let index = self.lengths.next::<true>((size - 1) >> LINE_SHIFT);
        let starts = self.starts.get_mut(index)?;
        let start = starts.pop().expect("available line run");
        if starts.is_empty() {
            self.lengths.set(index, false);
        }
        Some((start, start + (index + 1) * LINE_SIZE))
    }

    pub(super) fn clear(&mut self) {
        let mut index = self.lengths.next::<true>(0);
        while index < LINES_PER_BLOCK {
            self.starts[index].clear();
            index = self.lengths.next::<true>(index + 1);
        }
        self.lengths = LineMap::default();
    }

    pub(super) fn available_bytes(&self) -> usize {
        self.starts
            .iter()
            .enumerate()
            .map(|(index, starts)| starts.len() * (index + 1) * LINE_SIZE)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupied_ranges_and_free_runs_cross_bitmap_words() {
        let map = LineMap::default();
        map.mark_object(63 * LINE_SIZE + 10, LINE_SIZE);
        map.mark_object(95 * LINE_SIZE, 34 * LINE_SIZE);
        map.mark_object(190 * LINE_SIZE, 66 * LINE_SIZE);
        assert_eq!(map.marked_count(), 102);
        assert_eq!(
            map.free_runs().collect::<Vec<_>>(),
            [(0, 63), (65, 30), (129, 61)].map(|(start, len)| (start * LINE_SIZE, len * LINE_SIZE))
        );
    }

    #[test]
    fn concurrent_line_marking_preserves_all_bits() {
        use std::sync::{Arc, Barrier};

        let map = Arc::new(LineMap::default());
        let barrier = Arc::new(Barrier::new(4));

        // Each thread marks disjoint odd lines across word boundaries plus a shared cross-word span.
        std::thread::scope(|scope| {
            for tid in 0..4usize {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for step in 0..32 {
                        let line = tid + step * 8;
                        map.mark_object(line * LINE_SIZE + 16, LINE_SIZE / 2);
                    }
                    // Shared overlapping cross-word interval [60..70)
                    map.mark_object((60 + tid) * LINE_SIZE, 4 * LINE_SIZE);
                });
            }
        });

        let expected = LineMap::default();
        for tid in 0..4usize {
            for step in 0..32 {
                let line = tid + step * 8;
                expected.mark_object(line * LINE_SIZE + 16, LINE_SIZE / 2);
            }
            expected.mark_object((60 + tid) * LINE_SIZE, 4 * LINE_SIZE);
        }
        assert_eq!(map.marked_count(), expected.marked_count());
        assert_eq!(
            map.free_runs().collect::<Vec<_>>(),
            expected.free_runs().collect::<Vec<_>>()
        );
    }

    #[test]
    fn best_fit_keeps_unsuitable_runs_available() {
        let mut free = FreeLines::default();
        for (start, lines) in [(0, 2), (10, 90), (200, 1), (250, 3)] {
            free.insert(start * LINE_SIZE, lines * LINE_SIZE);
        }
        assert_eq!(
            free.take(3 * LINE_SIZE + 1),
            Some((10 * LINE_SIZE, 100 * LINE_SIZE))
        );
        assert_eq!(free.take(LINE_SIZE + 1), Some((0, 2 * LINE_SIZE)));
        assert_eq!(
            free.take(LINE_SIZE),
            Some((200 * LINE_SIZE, 201 * LINE_SIZE))
        );
        free.clear();
        assert_eq!(free.take(1), None);
    }
}
