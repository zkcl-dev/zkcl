use std::collections::BTreeSet;

use crate::leaf::{Block, Operand, Step};
use crate::select::SelectionError;

// The shared capacity of the scalar native entries, including Win64 home slots.
pub(super) const MAX_LOCALS: usize = 4;

/// Fill `live` with the values live on the way out of `block`.
///
/// The caller owns the buffer: both fixed points below visit every block in
/// every iteration, so allocating a set per visit dominates the pass.
fn live_after(block: &Block, live_at_entry: &[BTreeSet<usize>], live: &mut BTreeSet<usize>) {
    live.clear();
    block
        .exit
        .visit_targets(|target| live.extend(&live_at_entry[target]));
    let mut exit = block.exit;
    if let Some(Operand::Local(value)) = exit.operand().copied() {
        live.insert(value);
    }
}

fn add_inputs(step: &mut Step, live: &mut BTreeSet<usize>) {
    step.visit_inputs(|operand| {
        if let Operand::Local(value) = *operand {
            live.insert(value);
        }
    });
}

fn propagate_loop_liveness(blocks: &[Block], live_at_entry: &mut [BTreeSet<usize>]) {
    let mut live = BTreeSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for (index, block) in blocks.iter().enumerate().rev() {
            live_after(block, live_at_entry, &mut live);
            for mut step in block.steps.iter().rev().copied() {
                if step
                    .destination()
                    .is_some_and(|destination| !live.remove(destination))
                    && !step.may_miss()
                {
                    continue;
                }
                add_inputs(&mut step, &mut live);
            }
            if live_at_entry[index] != live {
                // Copy only when the set actually moved; the scratch buffer keeps
                // its allocation for the next visit.
                live_at_entry[index] = live.clone();
                changed = true;
            }
        }
    }
}

/// Assign slots to values that interfere only when they are live on the same
/// path. Edge copies are ordinary uses and definitions in their execution order.
pub(super) fn assign(blocks: &mut [Block], values: usize) -> Result<usize, SelectionError> {
    if values == 0 {
        return Ok(0);
    }
    let mut live_at_entry = vec![BTreeSet::new(); blocks.len()];
    if blocks
        .iter()
        .any(|block| block.steps.contains(&Step::CheckLoopBudget))
    {
        propagate_loop_liveness(blocks, &mut live_at_entry);
    }
    let mut conflicts = vec![BTreeSet::new(); values];
    let mut affinities = vec![Vec::new(); values];
    let mut used = vec![false; values];
    let mut live = BTreeSet::new();
    for (index, block) in blocks.iter_mut().enumerate().rev() {
        live_after(block, &live_at_entry, &mut live);
        block.steps.reverse();
        let mut overflow = live.len() > MAX_LOCALS;
        block.steps.retain_mut(|step| {
            if let Some(&mut destination) = step.destination() {
                let result_used = live.remove(&destination);
                if !result_used && !step.may_miss() {
                    return false;
                }
                used[destination] = true;
                for &other in &live {
                    conflicts[destination].insert(other);
                    conflicts[other].insert(destination);
                }
                if let Step::Copy {
                    value: Operand::Local(source),
                    ..
                } = *step
                {
                    affinities[destination].push(source);
                    affinities[source].push(destination);
                }
            }
            add_inputs(step, &mut live);
            overflow |= live.len() > MAX_LOCALS;
            true
        });
        if overflow {
            return Err(SelectionError::Locals);
        }
        block.steps.reverse();
        // Hand the computed entry set to the table and take its empty set back as
        // the scratch buffer, so no set is allocated here.
        std::mem::swap(&mut live, &mut live_at_entry[index]);
    }

    let mut slots: Vec<Option<usize>> = vec![None; values];
    let mut count = 0;
    let mut order: Vec<_> = (0..values).filter(|&i| used[i]).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(conflicts[i].len()), i));
    for value in order {
        let mut occupied = [false; MAX_LOCALS];
        for &other in &conflicts[value] {
            if let Some(slot) = slots[other] {
                occupied[slot] = true;
            }
        }
        let slot = affinities[value]
            .iter()
            .filter_map(|&other| slots[other])
            .find(|&slot| !occupied[slot])
            .or_else(|| occupied.iter().position(|&taken| !taken))
            .ok_or(SelectionError::Locals)?;
        slots[value] = Some(slot);
        count = count.max(slot + 1);
    }

    let rewrite = |operand: &mut Operand| {
        if let Operand::Local(value) = *operand {
            *operand = Operand::Local(slots[value].expect("used leaf value"));
        }
    };
    for block in blocks {
        block.steps.retain_mut(|step| {
            step.visit_inputs(rewrite);
            if let Some(destination) = step.destination() {
                *destination = slots[*destination].expect("retained leaf definition");
            }
            !matches!(*step, Step::Copy { destination, value: Operand::Local(source) } if destination == source)
        });
        if let Some(operand) = block.exit.operand() {
            rewrite(operand);
        }
    }
    Ok(count)
}
