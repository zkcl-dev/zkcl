use rustc_hash::FxHashMap;
use zkcl_bytecode::{
    BytecodeBody, HandlerTarget, Instruction, LiveEdge, LiveIns, LiveSet, Register,
    RegisterTransfer,
};

use super::BytecodeBlock;
use super::cfg::BytecodeCfg;

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
        // A summary visits every word of the frame. Replay short blocks directly
        // and only pay to build a summary once the worklist revisits a block.
        let summary_threshold = code.registers.div_ceil(usize::BITS as usize).max(8);
        move |index: usize, live: &mut LiveSet, revisiting: bool| {
            let block = &blocks[index];
            let instructions = &code.instructions[block.start_pc..block.end_pc];
            if native_debug && !instructions.is_empty() {
                live.fill_registers();
                return;
            }
            if revisiting && instructions.len() > summary_threshold {
                let summary = summaries.entry(index).or_insert_with(|| {
                    let mut summary = RegisterTransfer::new(code.registers);
                    if let Some(clear_from) = normal_clear(block) {
                        summary.then_clear_from(clear_from);
                    }
                    for instruction in instructions.iter().rev() {
                        summary.then_instruction(instruction);
                        side_table_registers(code, instruction)
                            .for_each(|reg| summary.then_read(reg));
                    }
                    summary
                });
                summary.apply(live);
                return;
            }
            if let Some(clear_from) = normal_clear(block) {
                live.clear_from(clear_from);
            }
            for instruction in instructions.iter().rev() {
                transfer(instruction, live);
            }
        }
    };
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
