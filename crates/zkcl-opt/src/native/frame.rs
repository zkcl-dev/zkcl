use std::ops::Range;

use rustc_hash::FxHashMap;

use crate::passes::{root_layout_with_indices, rpo_with_indices, unwind_table_with_indices};
use crate::{
    BasicBlock, BlockId, DeoptState, RootLayout, RootSlot, SsaFunction, SsaInst, SsaOp, Terminator,
    UnwindTable, ValueId,
};

/// Root slots and local unwind targets for an [`SsaFunction`].
pub struct NativeFrameLayout {
    rpo: Vec<BlockId>,
    block_indices: FxHashMap<BlockId, usize>,
    terminal_polls: FxHashMap<BlockId, usize>,
    has_checkpoints: bool,
    has_debug_points: bool,
    roots: RootLayout,
    block_safepoints: Vec<Range<usize>>,
    arguments: NativeArguments,
    unwind_table: UnwindTable,
    root_slots: usize,
    handler_condition_slots: FxHashMap<BlockId, usize>,
    unwind_register_slots: FxHashMap<usize, usize>,
    catch_tag_slots: FxHashMap<(BlockId, usize), usize>,
}

impl NativeFrameLayout {
    /// Canonical registers retained for non-local landings rather than the normal successor.
    pub fn unwind_restore<'a>(
        &'a self,
        recovery: &'a DeoptState,
    ) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.unwind_register_slots
            .iter()
            .filter_map(|(&reg, &slot)| {
                (!recovery
                    .registers_to_restore
                    .iter()
                    .any(|(saved, _)| *saved as usize == reg))
                .then_some((reg, slot))
            })
    }

    pub fn roots(&self) -> &RootLayout {
        &self.roots
    }

    pub fn arguments(&self) -> &NativeArguments {
        &self.arguments
    }

    pub fn root_slots(&self) -> usize {
        self.root_slots
    }

    pub fn unwind_table(&self) -> &UnwindTable {
        &self.unwind_table
    }

    pub fn catch_tag_slot(&self, block: BlockId, instruction_index: usize) -> usize {
        self.catch_tag_slots[&(block, instruction_index)]
    }

    pub fn handler_condition_slot(&self, block: BlockId) -> usize {
        self.handler_condition_slots[&block]
    }

    /// A canonical register needs a copy only if a recovery block reads it.
    /// Canonical registers and physical roots read by non-local landings.
    pub fn debug_unwind_registers(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.unwind_register_slots
            .iter()
            .map(|(&register, &slot)| (register, slot))
    }

    pub fn unwind_register_slot(&self, register: usize) -> Option<usize> {
        self.unwind_register_slots.get(&register).copied()
    }

    /// Values supplied by the runtime frame, in the recovery block's parameter order.
    pub fn unwind_parameters<'a>(
        &'a self,
        block: &'a BasicBlock,
    ) -> impl ExactSizeIterator<Item = (ValueId, usize)> + 'a {
        block.params.iter().map(|parameter| {
            let register = parameter
                .origin_reg
                .expect("unwind entry register parameter");
            (parameter.value, self.unwind_register_slots[&register])
        })
    }

    /// Visit a block in instruction order with the live roots at each instruction.
    pub fn instructions<'a>(
        &'a self,
        block: &'a BasicBlock,
    ) -> impl Iterator<Item = (usize, &'a SsaInst, &'a [(ValueId, RootSlot)])> {
        let sites = if self.block_safepoints.is_empty() {
            &[]
        } else {
            let range = &self.block_safepoints[self.block_indices[&block.id]];
            &self.roots.safepoint_roots[range.clone()]
        };
        let mut safepoints = sites.iter().peekable();
        block.insts.iter().enumerate().map(move |(index, inst)| {
            let live = safepoints
                .next_if(|site| site.inst_index == index)
                .map_or(&[][..], |site| site.live_slots.as_slice());
            (index, inst, live)
        })
    }

    pub fn reverse_postorder(&self) -> &[BlockId] {
        &self.rpo
    }

    /// Position of a block in the function used to build this layout.
    pub fn block_index(&self, id: BlockId) -> Option<usize> {
        self.block_indices.get(&id).copied()
    }

    /// Whether codegen needs the cancellation and execution budget inputs.
    pub fn needs_execution_polls(&self) -> bool {
        self.has_checkpoints || !self.terminal_polls.is_empty()
    }

    /// Whether the native body retains source-level debugger state.
    pub fn has_debug_points(&self) -> bool {
        self.has_debug_points
    }

    /// Poll before an SSA backedge when the block has no bytecode checkpoint.
    pub fn terminal_poll_pc(&self, id: BlockId) -> Option<usize> {
        self.terminal_polls.get(&id).copied()
    }
}

/// Storage of arguments beyond the ABI's required positional parameters.
pub enum NativeArguments {
    /// The function has no optional or keyword reads and never constructs a rest list.
    Unused,
    /// The incoming array stays valid for the duration of this invocation.
    UnrootedOptionals,
    /// Save the referenced bounded arguments before a safepoint can move them.
    RootedOptionals(OptionalArgumentSlots),
    /// Use the runtime's argument binder for the unbounded or keyword tail.
    RuntimeTail,
}

pub struct OptionalArgumentSlots {
    first_slot: usize,
    indices: Vec<usize>,
}

impl OptionalArgumentSlots {
    /// Relative argument index and root slot for each referenced optional argument.
    pub fn iter(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.indices.iter().copied().zip(self.slots())
    }

    pub fn slots(&self) -> Range<usize> {
        self.first_slot..self.first_slot + self.indices.len()
    }

    pub fn slot_for(&self, index: usize) -> usize {
        let offset = self
            .indices
            .binary_search(&index)
            .expect("used optional argument");
        self.first_slot + offset
    }
}

/// Compute roots, cancellation polls and local recovery slots from the same SSA graph.
pub fn native_frame_layout(ssa_function: &SsaFunction) -> NativeFrameLayout {
    let block_indices = ssa_function.block_index_map();
    let rpo = rpo_with_indices(ssa_function, &block_indices);
    let unwind_table = unwind_table_with_indices(ssa_function, &rpo, &block_indices);
    let roots =
        root_layout_with_indices(ssa_function, &block_indices, &rpo, unwind_table.targets());

    let mut block_safepoints = Vec::new();
    if !roots.safepoint_roots.is_empty() {
        block_safepoints.resize(ssa_function.blocks.len(), 0..0);
        let mut start = 0;
        while start < roots.safepoint_roots.len() {
            let block = roots.safepoint_roots[start].block;
            let mut end = start + 1;
            while roots
                .safepoint_roots
                .get(end)
                .is_some_and(|site| site.block == block)
            {
                end += 1;
            }
            block_safepoints[block_indices[&block]] = start..end;
            start = end;
        }
    }

    let mut needs_root_frame = !roots.safepoint_roots.is_empty();
    let mut handler_condition_slots: FxHashMap<BlockId, usize> = FxHashMap::default();
    let mut catch_tag_slots: FxHashMap<(BlockId, usize), usize> = FxHashMap::default();
    let mut next_extra_slot = roots.total_slots.max(2 + ssa_function.arity.required);
    let mut optional_arguments = Vec::new();
    let mut runtime_tail = false;

    let mut unwind_register_slots = FxHashMap::default();
    // An edge to a block already visited in reverse postorder (including
    // the current block) needs a poll before the terminator.
    let mut seen = vec![false; ssa_function.blocks.len()];
    let mut terminal_polls = FxHashMap::default();
    for &id in &rpo {
        let index = block_indices[&id];
        seen[index] = true;
        let block = &ssa_function.blocks[index];
        let mut has_backedge = false;
        block
            .terminator
            .visit_targets(|target| has_backedge |= seen[block_indices[&target.block]]);
        if has_backedge {
            terminal_polls.insert(id, block.terminator_pc);
        }
    }
    for target in unwind_table.targets() {
        for param in &ssa_function.blocks[block_indices[target]].params {
            if let Some(reg) = param.origin_reg {
                unwind_register_slots.entry(reg).or_insert_with(|| {
                    let slot = next_extra_slot;
                    next_extra_slot += 1;
                    slot
                });
            }
        }
    }
    let mut has_checkpoints = false;
    let mut has_debug_points = false;
    for block in &ssa_function.blocks {
        for (inst_idx, inst) in block.insts.iter().enumerate() {
            if inst.deopt.is_some()
                && ssa_function.arity.maximum != Some(ssa_function.arity.required)
            {
                // A default argument may call Lisp before the rest of the arguments
                // are bound. Both tiers use the same saved argument tail.
                runtime_tail = true;
            }
            // Safepoints already request a frame. Keep one for recovery and
            // these non-allocating runtime accesses as well.
            needs_root_frame = needs_root_frame
                || inst.deopt.is_some()
                || matches!(
                    inst.op,
                    SsaOp::ReadBox(_)
                        | SsaOp::WriteBox { .. }
                        | SsaOp::ReadCapture(_)
                        | SsaOp::WriteCell { .. }
                );
            match &inst.op {
                SsaOp::Checkpoint { .. } => {
                    has_checkpoints = true;
                    terminal_polls.remove(&block.id);
                }
                SsaOp::DebugPoint { .. } => has_debug_points = true,
                SsaOp::OptionalArgument(index) => optional_arguments.push(*index),
                SsaOp::RestArguments { .. }
                | SsaOp::CheckKeywords { .. }
                | SsaOp::KeywordArgument(_) => runtime_tail = true,
                SsaOp::EnterHandlers { targets, .. } => {
                    for &t in targets {
                        handler_condition_slots.entry(t).or_insert_with(|| {
                            let slot = next_extra_slot;
                            next_extra_slot += 1;
                            slot
                        });
                    }
                }
                SsaOp::EnterCatch { .. } => {
                    let slot = next_extra_slot;
                    next_extra_slot += 1;
                    catch_tag_slots.insert((block.id, inst_idx), slot);
                }
                _ => {}
            }
        }
        if matches!(block.terminator, Terminator::Unwind(_)) {
            needs_root_frame = true;
        }
    }

    let arguments = if runtime_tail
        || (ssa_function.arity.maximum.is_none() && !optional_arguments.is_empty())
    {
        needs_root_frame = true;
        NativeArguments::RuntimeTail
    } else if optional_arguments.is_empty() {
        NativeArguments::Unused
    } else if needs_root_frame {
        optional_arguments.sort_unstable();
        optional_arguments.dedup();
        let slots = OptionalArgumentSlots {
            first_slot: next_extra_slot,
            indices: optional_arguments,
        };
        next_extra_slot = slots.slots().end;
        NativeArguments::RootedOptionals(slots)
    } else {
        NativeArguments::UnrootedOptionals
    };

    let root_slots = if needs_root_frame { next_extra_slot } else { 0 };

    NativeFrameLayout {
        rpo,
        block_indices,
        terminal_polls,
        has_checkpoints,
        has_debug_points,
        roots,
        block_safepoints,
        arguments,
        unwind_register_slots,
        unwind_table,
        root_slots,
        handler_condition_slots,
        catch_tag_slots,
    }
}
