use std::collections::hash_map::Entry;

use rustc_hash::FxHashMap;
use zkcl_bytecode::{
    BytecodeBody, HandlerTarget, Instruction, LiveEdge, LiveIns, LiveSet, Register,
    RegisterTransfer,
};

use super::BytecodeBlock;
use super::cfg::BytecodeCfg;

/// Width buckets for revisited blocks: `<8`, `8–15`, `16–31`, `32+` instructions.
///
/// A revisit is bucketed by the width of the block it replays, not by whether it
/// was summarized: the summary threshold follows the frame size
/// (`max(registers / word, 8)`), so the width distribution is what tells whether
/// lowering it would summarize more revisits.
#[cfg(feature = "counters")]
const REVISIT_WIDTH_BUCKETS: usize = 4;

#[cfg(feature = "counters")]
fn revisit_width_bucket(width: usize) -> usize {
    match width {
        0..=7 => 0,
        8..=15 => 1,
        16..=31 => 2,
        _ => 3,
    }
}

/// Counts of the block-summary path inside one solved function.
///
/// The design note in `research/shortcut-memoization.md` gates extending summary
/// lifetimes across compilations on measuring this path first: a cache that no
/// real revisit reaches is a cost with no benefit. These counts are that
/// measurement, not a cache.
#[cfg(feature = "counters")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LivenessSummaries {
    /// Blocks handed to the solver, summed over every solved function.
    pub blocks: u64,
    /// Successor edges in the ordinary CFG, summed over every solved function.
    pub succ_edges: u64,
    /// The successor edges that point at the same or an earlier block. Zero here
    /// means the solved CFG has no back edge at all, so no revisit is possible.
    pub back_edges: u64,
    /// Transfer invocations, summed over every solved function. Zero means the
    /// solver never reached a block, one per block means nothing was revisited.
    pub transfers: u64,
    /// Summaries composed because the worklist revisited a long block, one count
    /// per [`REVISIT_WIDTH_BUCKETS`] bucket.
    pub built_by_size: [u64; REVISIT_WIDTH_BUCKETS],
    /// Revisits of a long block whose summary already existed and was applied.
    pub applied: u64,
    /// Revisits replayed instruction by instruction, one count per
    /// [`REVISIT_WIDTH_BUCKETS`] bucket.
    pub direct_by_size: [u64; REVISIT_WIDTH_BUCKETS],
}

#[cfg(feature = "counters")]
mod counters {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{LivenessSummaries, REVISIT_WIDTH_BUCKETS, revisit_width_bucket};

    // Per-thread counts: one test reads back its own work, unaffected by the
    // tests libtest runs beside it.
    thread_local! {
        static LOCAL: Cell<LivenessSummaries> = const {
            Cell::new(LivenessSummaries {
                blocks: 0,
                succ_edges: 0,
                back_edges: 0,
                transfers: 0,
                built_by_size: [0; REVISIT_WIDTH_BUCKETS],
                applied: 0,
                direct_by_size: [0; REVISIT_WIDTH_BUCKETS],
            })
        };
    }

    // Process-wide counts: a suite total cannot be read from any one thread.
    static TOTAL_BLOCKS: AtomicU64 = AtomicU64::new(0);
    static TOTAL_SUCC_EDGES: AtomicU64 = AtomicU64::new(0);
    static TOTAL_BACK_EDGES: AtomicU64 = AtomicU64::new(0);
    static TOTAL_TRANSFERS: AtomicU64 = AtomicU64::new(0);
    static TOTAL_APPLIED: AtomicU64 = AtomicU64::new(0);
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static TOTAL_BUILT_BY_SIZE: [AtomicU64; REVISIT_WIDTH_BUCKETS] = [ZERO; REVISIT_WIDTH_BUCKETS];
    static TOTAL_DIRECT_BY_SIZE: [AtomicU64; REVISIT_WIDTH_BUCKETS] = [ZERO; REVISIT_WIDTH_BUCKETS];

    macro_rules! record {
        ($field:ident, $total:ident, $amount:expr) => {{
            let amount = $amount;
            LOCAL.with(|counts| {
                let mut current = counts.get();
                current.$field += amount;
                counts.set(current);
            });
            $total.fetch_add(amount, Ordering::Relaxed);
        }};
    }

    pub(super) fn record_blocks(blocks: usize) {
        record!(blocks, TOTAL_BLOCKS, blocks as u64);
    }

    pub(super) fn record_successor(backward: bool) {
        record!(succ_edges, TOTAL_SUCC_EDGES, 1);
        if backward {
            record!(back_edges, TOTAL_BACK_EDGES, 1);
        }
    }

    pub(super) fn record_transfer() {
        record!(transfers, TOTAL_TRANSFERS, 1);
    }

    /// A revisit that composed a summary, bucketed by the block's width.
    pub(super) fn record_built(width: usize) {
        let bucket = revisit_width_bucket(width);
        LOCAL.with(|counts| {
            let mut current = counts.get();
            current.built_by_size[bucket] += 1;
            counts.set(current);
        });
        TOTAL_BUILT_BY_SIZE[bucket].fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_applied() {
        record!(applied, TOTAL_APPLIED, 1);
    }

    /// A revisit replayed instruction by instruction, bucketed by block width.
    pub(super) fn record_direct(width: usize) {
        let bucket = revisit_width_bucket(width);
        LOCAL.with(|counts| {
            let mut current = counts.get();
            current.direct_by_size[bucket] += 1;
            counts.set(current);
        });
        TOTAL_DIRECT_BY_SIZE[bucket].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts for the current thread, reset by [`reset`].
    pub fn read() -> LivenessSummaries {
        LOCAL.with(Cell::get)
    }

    /// Counts for the whole process since it started or since [`reset_total`].
    /// A suite reports these so that a zero here, not a zero from one synthetic
    /// test, says "no revisit".
    pub fn read_total() -> LivenessSummaries {
        LivenessSummaries {
            blocks: TOTAL_BLOCKS.load(Ordering::Relaxed),
            succ_edges: TOTAL_SUCC_EDGES.load(Ordering::Relaxed),
            back_edges: TOTAL_BACK_EDGES.load(Ordering::Relaxed),
            transfers: TOTAL_TRANSFERS.load(Ordering::Relaxed),
            built_by_size: std::array::from_fn(|bucket| {
                TOTAL_BUILT_BY_SIZE[bucket].load(Ordering::Relaxed)
            }),
            applied: TOTAL_APPLIED.load(Ordering::Relaxed),
            direct_by_size: std::array::from_fn(|bucket| {
                TOTAL_DIRECT_BY_SIZE[bucket].load(Ordering::Relaxed)
            }),
        }
    }

    /// Drops this thread's counts. The process-wide totals stay.
    pub fn reset() {
        LOCAL.with(|counts| counts.set(LivenessSummaries::default()));
    }

    /// Drops the process-wide totals, so a caller that wants only its own work
    /// reads a clean [`read_total`]. Other threads adding while this runs are
    /// racing with it, so reset before starting the measured work, not during.
    pub fn reset_total() {
        let scalars = [
            &TOTAL_BLOCKS,
            &TOTAL_SUCC_EDGES,
            &TOTAL_BACK_EDGES,
            &TOTAL_TRANSFERS,
            &TOTAL_APPLIED,
        ];
        for total in scalars {
            total.store(0, Ordering::SeqCst);
        }
        for total in TOTAL_BUILT_BY_SIZE.iter().chain(&TOTAL_DIRECT_BY_SIZE) {
            total.store(0, Ordering::SeqCst);
        }
    }
}

#[cfg(feature = "counters")]
pub use counters::{
    read as liveness_summaries, read_total as liveness_summaries_total,
    reset as reset_liveness_summaries, reset_total as reset_liveness_summaries_total,
};

/// Register liveness at ordinary block entries and bytecode PCs with recovery state.
pub(super) fn register_liveness(
    code: &BytecodeBody<'_>,
    cfg: &BytecodeCfg,
    native_debug: bool,
) -> (LiveIns, FxHashMap<usize, Vec<Register>>) {
    let blocks = &cfg.blocks;
    let successors = |index: usize| {
        let block = &blocks[index];
        block
            .succs
            .iter()
            .map(|id| LiveEdge::ordinary(id.0 as usize))
            .chain(block.unwind_succs.iter().map(|entry| {
                LiveEdge::recovery(
                    entry.target.0 as usize,
                    entry.registers.clear_from,
                    entry.registers.condition_register,
                )
            }))
    };
    let normal_clear = |block: &BytecodeBlock| {
        // Local GO and BeginCleanup terminate their respective blocks.
        if block.start_pc == block.end_pc {
            return None;
        }
        cfg.normal_recoveries
            .get(&(block.end_pc - 1))
            .map(|recovery| recovery.clear_from)
    };
    let transfer = |instruction: &Instruction, live: &mut LiveSet| {
        live.transfer_registers(instruction);
        side_table_registers(code, instruction).for_each(|reg| live.insert(reg));
    };

    let transfer_block = {
        let mut summaries = FxHashMap::<usize, RegisterTransfer>::default();
        // A summary visits every word of the frame while a short block is cheap
        // to replay, so the width at which summarizing pays is
        // `max(registers / word, 8)` instructions.
        let summary_threshold = code.registers.div_ceil(usize::BITS as usize).max(8);
        let compose_summary = |block: &BytecodeBlock, instructions: &[Instruction]| {
            let mut summary = RegisterTransfer::new(code.registers);
            if let Some(clear_from) = normal_clear(block) {
                summary.then_clear_from(clear_from);
            }
            for instruction in instructions.iter().rev() {
                summary.then_instruction(instruction);
                side_table_registers(code, instruction).for_each(|reg| summary.then_read(reg));
            }
            summary
        };
        move |index: usize, live: &mut LiveSet, revisiting: bool| {
            #[cfg(feature = "counters")]
            counters::record_transfer();
            let block = &blocks[index];
            let instructions = &code.instructions[block.start_pc..block.end_pc];
            if native_debug && !instructions.is_empty() {
                live.fill_registers();
                return;
            }
            if revisiting && instructions.len() > summary_threshold {
                match summaries.entry(index) {
                    Entry::Occupied(entry) => {
                        #[cfg(feature = "counters")]
                        counters::record_applied();
                        entry.get().apply(live);
                    }
                    Entry::Vacant(entry) => {
                        #[cfg(feature = "counters")]
                        counters::record_built(instructions.len());
                        entry
                            .insert(compose_summary(block, instructions))
                            .apply(live);
                    }
                }
                return;
            }
            #[cfg(feature = "counters")]
            {
                if revisiting {
                    counters::record_direct(instructions.len());
                }
            }
            if let Some(clear_from) = normal_clear(block) {
                live.clear_from(clear_from);
            }
            for instruction in instructions.iter().rev() {
                transfer(instruction, live);
            }
        }
    };
    #[cfg(feature = "counters")]
    {
        counters::record_blocks(blocks.len());
        for (index, block) in blocks.iter().enumerate() {
            for successor in &block.succs {
                counters::record_successor(successor.0 as usize <= index);
            }
        }
    }
    let live_in =
        LiveIns::solve_registers(blocks.len(), code.registers, successors, transfer_block);
    let mut live = LiveSet::new(code.registers);
    if native_debug {
        live.fill_registers();
    }
    let mut deopt_registers = FxHashMap::default();
    for block in blocks {
        if !native_debug {
            live_in.live_out(successors(block.id.0 as usize), &mut live);
            if let Some(clear_from) = normal_clear(block) {
                live.clear_from(clear_from);
            }
        }
        let instructions = &code.instructions[block.start_pc..block.end_pc];
        for (offset, instruction) in instructions.iter().enumerate().rev() {
            let pc = block.start_pc + offset;
            if !native_debug {
                transfer(instruction, &mut live);
            }
            let needs_recovery = match instruction {
                // A call can hand off to the VM after its callee suspends, or
                // before dispatch if specialization inserts a guard. Native
                // frame recovery transfers any active scopes with the registers.
                Instruction::CallValues { .. }
                | Instruction::Call { .. }
                | Instruction::ReadCell { .. } => true,
                _ => cfg.execution_checkpoint(pc, instruction),
            };
            if needs_recovery {
                deopt_registers.insert(pc, live.registers().collect());
            }
        }
    }
    (live_in, deopt_registers)
}

/// Slots used at entry or by later handler and restart dispatch.
pub(super) fn side_table_registers<'a>(
    code: &'a BytecodeBody<'_>,
    inst: &Instruction,
) -> impl Iterator<Item = Register> + use<'a> {
    let (handlers, restarts): (&[_], &[_]) = match inst {
        Instruction::EnterHandlers { table } => (&code.handlers[*table].handlers, &[]),
        Instruction::EnterRestarts { table, .. } => (&[], &code.restarts[*table].restarts),
        _ => (&[], &[]),
    };
    handlers
        .iter()
        .filter_map(|handler| match handler.target {
            HandlerTarget::Function(reg) => Some(reg),
            HandlerTarget::Case { .. } => None,
        })
        .chain(restarts.iter().flat_map(|spec| spec.frame_registers()))
}
