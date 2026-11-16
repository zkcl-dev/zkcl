//! Bounded scheduling for independent work on immutable source snapshots.

use std::cmp::Reverse;
use std::iter;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::thread;

use super::SourceSnapshot;

const MIN_BYTES_PER_WORKER: usize = 8 * 1024;
const MAX_IMPLICIT_JOBS: NonZeroUsize = NonZeroUsize::new(8).unwrap();

pub(super) fn default_jobs() -> NonZeroUsize {
    thread::available_parallelism()
        .unwrap_or(NonZeroUsize::MIN)
        .min(MAX_IMPLICIT_JOBS)
}

pub(super) fn prepare_documents<'a>(
    documents: impl Iterator<Item = &'a SourceSnapshot>,
    jobs: impl FnOnce() -> NonZeroUsize,
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    for_each_document(
        documents.filter(|source| source.index.get().is_none()),
        jobs,
        |source| source.source().byte_len().expect("snapshot has text"),
        cancelled,
        |source| {
            source.index();
        },
    )
}

pub(super) fn for_each_document<T: Send>(
    work: impl Iterator<Item = T>,
    jobs: impl FnOnce() -> NonZeroUsize,
    bytes: impl Fn(&T) -> usize,
    cancelled: &impl Fn() -> bool,
    run: impl Fn(T) + Sync,
) -> Option<()> {
    if cancelled() {
        return None;
    }
    let mut work = work.peekable();
    let Some(first) = work.next() else {
        return Some(());
    };
    let limit = if cfg!(target_family = "wasm") || work.peek().is_none() {
        1
    } else {
        jobs().get()
    };
    let work = iter::once(first).chain(work);
    if limit == 1 {
        return run_serial(work, cancelled, run);
    }

    let mut total = 0usize;
    let mut work: Vec<_> = work
        .map(|task| {
            let size = bytes(&task);
            total = total.saturating_add(size);
            (size, task)
        })
        .collect();
    let workers = limit.min(work.len()).min(total.div_ceil(MIN_BYTES_PER_WORKER));
    if workers <= 1 {
        return run_serial(work.into_iter().map(|(_, task)| task), cancelled, run);
    }

    // Start large files early. The callers provide the result slots, so job
    // order is independent of report order.
    work.sort_unstable_by_key(|(size, _)| Reverse(*size));
    if cancelled() {
        return None;
    }
    let pending = Mutex::new(work.into_iter());
    let next = || pending.lock().unwrap().next().map(|(_, task)| task);
    thread::scope(|scope| {
        for _ in 1..workers {
            if thread::Builder::new()
                .spawn_scoped(scope, || iter::from_fn(&next).for_each(&run))
                .is_err()
            {
                break;
            }
        }
        // The caller alone owns the cancellation callback. Empty the queue
        // when it observes cancellation and let in-flight indexes finish.
        loop {
            if cancelled() {
                let queued = std::mem::take(&mut *pending.lock().unwrap());
                drop(queued);
                return None;
            }
            let Some(task) = next() else {
                return Some(());
            };
            run(task);
        }
    })?;
    (!cancelled()).then_some(())
}

fn run_serial<T>(
    work: impl Iterator<Item = T>,
    cancelled: &impl Fn() -> bool,
    run: impl Fn(T),
) -> Option<()> {
    for task in work {
        if cancelled() {
            return None;
        }
        run(task);
    }
    (!cancelled()).then_some(())
}
