use std::collections::BTreeMap;
use std::ops::Range;

use zkcl_bytecode::{Instruction, InstructionFlow, LiveEdge, LiveIns};

use crate::leaf::{Block, Exit, Operand, Step};

/// Scope transfers and argument/binding branches are not leaf operations.
/// Keep them in their containing block so selection can reject that path or
/// turn an unsupported type-failure path into a leaf miss.
pub(super) fn leaf_flow(instruction: &Instruction) -> InstructionFlow {
    match instruction {
        Instruction::IfPresent(..) | Instruction::WhenBound { .. } => InstructionFlow::Next,
        _ => match instruction.flow() {
            InstructionFlow::Transfer => InstructionFlow::Next,
            flow => flow,
        },
    }
}

pub(super) fn liveness(code: &[Instruction], registers: usize, blocks: &[Range<usize>]) -> LiveIns {
    let mut block_at_pc = vec![0; code.len() + 1];
    for (index, block) in blocks.iter().enumerate() {
        block_at_pc[block.start] = index;
    }
    let successors = |index: usize| {
        let block = &blocks[index];
        let flow = if block.is_empty() {
            InstructionFlow::Transfer
        } else {
            leaf_flow(&code[block.end - 1])
        };
        flow.successors(block.end)
            .filter_map(|pc| block_at_pc.get(pc).copied().map(LiveEdge::ordinary))
    };
    LiveIns::solve(
        blocks.len(),
        registers,
        successors,
        |index, live| {
            for instruction in code[blocks[index].clone()].iter().rev() {
                live.transfer_registers(instruction);
                live.values = match instruction {
                    Instruction::SetValues(_) | Instruction::Call { .. } => 0,
                    Instruction::ReadValue(_, index) => live.values.max(index.saturating_add(1)),
                    Instruction::BindValues(range) => range.len(),
                    _ => live.values,
                };
            }
        },
        Some(blocks.len() - 1),
    )
}

pub(super) struct Edge {
    pub(super) target_pc: usize,
    pub(super) copies: Vec<Step>,
}

/// Loop inputs can be read and assigned by the same edge. Preserve the old
/// values in a parallel assignment, using a temporary only for a cycle.
pub(super) fn backedge_copies(mut moves: Vec<(usize, Operand)>, values: &mut usize) -> Vec<Step> {
    moves.retain(|&(destination, source)| source != Operand::Local(destination));
    let mut remaining_reads = BTreeMap::new();
    for &(_, source) in &moves {
        if let Operand::Local(value) = source {
            *remaining_reads.entry(value).or_insert(0usize) += 1;
        }
    }
    let mut steps = vec![Step::CheckLoopBudget];
    while !moves.is_empty() {
        if let Some(index) = moves
            .iter()
            .position(|(destination, _)| !remaining_reads.contains_key(destination))
        {
            let (destination, value) = moves.swap_remove(index);
            if let Operand::Local(source) = value {
                let reads = remaining_reads
                    .get_mut(&source)
                    .expect("original move source");
                *reads -= 1;
                if *reads == 0 {
                    remaining_reads.remove(&source);
                }
            }
            steps.push(Step::Copy { destination, value });
        } else {
            let source = moves[0].0;
            let old = Operand::Local(source);
            let destination = *values;
            *values += 1;
            steps.push(Step::Copy {
                destination,
                value: old,
            });
            let reads = remaining_reads
                .remove(&source)
                .expect("cycle reads its source");
            remaining_reads.insert(destination, reads);
            for (_, value) in &mut moves {
                if *value == old {
                    *value = Operand::Local(destination);
                }
            }
        }
    }
    steps
}

/// Lay out selected bodies in bytecode order and replace edge indices in their
/// terminators with block indices. Conditional edge copies get their own block.
pub(super) fn layout(
    bodies: Vec<(usize, Block)>,
    mut edges: Vec<Edge>,
    code_len: usize,
) -> Vec<Block> {
    let mut block_at_pc = vec![0; code_len + 1];
    let mut copy_block = vec![None; edges.len()];
    let entry_copies = std::mem::take(&mut edges[0].copies);
    let entry_block = !entry_copies.is_empty();
    let mut emitted = usize::from(entry_block);
    for (pc, block) in &bodies {
        block_at_pc[*pc] = emitted;
        emitted += 1;
        if matches!(block.exit, Exit::Branch { .. }) {
            block.exit.visit_targets(|edge| {
                if !edges[edge].copies.is_empty() {
                    copy_block[edge] = Some(emitted);
                    emitted += 1;
                }
            });
        }
    }

    let mut blocks = Vec::with_capacity(emitted);
    if entry_block {
        blocks.push(Block {
            steps: entry_copies,
            exit: Exit::Jump(block_at_pc[0]),
        });
    }
    for (_, mut block) in bodies {
        let edge_exit = block.exit;
        if let Exit::Jump(edge) = edge_exit {
            block.steps.append(&mut edges[edge].copies);
        }
        block
            .exit
            .map_targets(|edge| copy_block[edge].unwrap_or(block_at_pc[edges[edge].target_pc]));
        blocks.push(block);
        edge_exit.visit_targets(|edge| {
            if copy_block[edge].is_some() {
                let edge = &mut edges[edge];
                blocks.push(Block {
                    steps: std::mem::take(&mut edge.copies),
                    exit: Exit::Jump(block_at_pc[edge.target_pc]),
                });
            }
        });
    }
    blocks
}

/// Combine adjoining blocks connected by an edge that has no other predecessor.
pub(super) fn combine_fallthroughs(blocks: Vec<Block>) -> Vec<Block> {
    let mut predecessors = vec![0; blocks.len()];
    for block in &blocks {
        block.exit.visit_targets(|target| predecessors[target] += 1);
    }
    let mut compacted: Vec<Block> = Vec::with_capacity(blocks.len());
    let mut indices = Vec::with_capacity(blocks.len());
    for (index, mut block) in blocks.into_iter().enumerate() {
        if predecessors[index] == 1
            && let Some(previous) = compacted.last_mut()
            && previous.exit == Exit::Jump(index)
        {
            previous.steps.append(&mut block.steps);
            previous.exit = block.exit;
            indices.push(compacted.len() - 1);
        } else {
            indices.push(compacted.len());
            compacted.push(block);
        }
    }
    for block in &mut compacted {
        block.exit.map_targets(|target| indices[target]);
    }
    compacted
}
