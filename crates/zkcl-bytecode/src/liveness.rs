//! Backward liveness for the canonical register frame.

use crate::{Instruction, Register, RegisterSet};

const WORD_BITS: usize = usize::BITS as usize;

/// The registers a predecessor can inherit from one successor. Recovery
/// supplies an independent suffix and, for a handler, a condition register.
#[derive(Clone, Copy)]
pub struct LiveEdge {
    target: usize,
    recovery: Option<(Register, Option<Register>)>,
}

impl LiveEdge {
    pub fn ordinary(target: usize) -> Self {
        Self {
            target,
            recovery: None,
        }
    }

    pub fn recovery(target: usize, clear_from: Register, condition: Option<Register>) -> Self {
        Self {
            target,
            recovery: Some((clear_from, condition)),
        }
    }
}

pub struct LiveSet {
    words: Vec<usize>,
    registers: usize,
    pub values: usize,
}

impl LiveSet {
    pub fn new(registers: usize) -> Self {
        Self {
            words: vec![0; registers.div_ceil(WORD_BITS)],
            registers,
            values: 0,
        }
    }

    /// Kill definitions before adding uses, including aliasing operands.
    pub fn transfer_registers(&mut self, instruction: &Instruction) {
        let operands = instruction.register_operands();
        for_each_in_frame(operands.writes, self.registers, |reg| self.remove(reg));
        for_each_in_frame(operands.reads, self.registers, |reg| self.insert(reg));
    }

    pub fn insert(&mut self, reg: Register) {
        if reg < self.registers {
            self.words[reg / WORD_BITS] |= bit(reg);
        }
    }

    fn remove(&mut self, reg: Register) {
        if reg < self.registers {
            self.words[reg / WORD_BITS] &= !bit(reg);
        }
    }

    pub fn clear_from(&mut self, reg: Register) {
        if let Some((word, rest)) = self.words[reg / WORD_BITS..].split_first_mut() {
            *word &= bit(reg) - 1;
            rest.fill(0);
        }
    }

    pub fn fill_registers(&mut self) {
        self.words.fill(usize::MAX);
        if !self.registers.is_multiple_of(WORD_BITS) {
            *self.words.last_mut().unwrap() = bit(self.registers) - 1;
        }
    }

    pub fn registers(&self) -> impl Iterator<Item = Register> + '_ {
        set_registers(&self.words)
    }

    fn union(&mut self, input: &[usize], edge: LiveEdge) {
        let Some((clear, condition)) = edge.recovery else {
            for (out, &input) in self.words.iter_mut().zip(input) {
                *out |= input;
            }
            return;
        };
        // The edge supplies the condition; a use from another edge stays live.
        let newly_supplied = condition.filter(|&reg| self.words[reg / WORD_BITS] & bit(reg) == 0);
        let (full_words, partial_bits) = (clear / WORD_BITS, clear % WORD_BITS);
        for (out, &input) in self.words[..full_words]
            .iter_mut()
            .zip(&input[..full_words])
        {
            *out |= input;
        }
        if partial_bits != 0 {
            self.words[full_words] |= input[full_words] & (bit(clear) - 1);
        }
        if let Some(reg) = newly_supplied {
            self.remove(reg);
        }
    }
}

#[derive(Clone, Copy, Default)]
struct TransferWord {
    generated: usize,
    killed: usize,
}

impl TransferWord {
    fn kill(&mut self, mask: usize) {
        self.generated &= !mask;
        self.killed |= mask;
    }
}

/// A sequence of backward register transfers, `G ∪ (live_out \ K)`.
/// Compose validated operands in analysis order, from the last instruction to
/// the first, and apply to live sets with the same frame size.
pub struct RegisterTransfer {
    words: Vec<TransferWord>,
    registers: usize,
}

impl RegisterTransfer {
    pub fn new(registers: usize) -> Self {
        Self {
            words: vec![TransferWord::default(); registers.div_ceil(WORD_BITS)],
            registers,
        }
    }

    /// Compose the instruction's writes, then its reads, including aliases.
    pub fn then_instruction(&mut self, instruction: &Instruction) {
        let operands = instruction.register_operands();
        operands
            .writes
            .for_each(|reg| self.words[reg / WORD_BITS].kill(bit(reg)));
        operands.reads.for_each(|reg| self.then_read(reg));
    }

    pub fn then_read(&mut self, reg: Register) {
        self.words[reg / WORD_BITS].generated |= bit(reg);
    }

    pub fn then_clear_from(&mut self, reg: Register) {
        if let Some((word, rest)) = self.words[reg / WORD_BITS..].split_first_mut() {
            word.kill(!(bit(reg) - 1));
            rest.fill(TransferWord {
                generated: 0,
                killed: usize::MAX,
            });
        }
    }

    pub fn apply(&self, live: &mut LiveSet) {
        debug_assert_eq!(self.registers, live.registers);
        for (word, summary) in live.words.iter_mut().zip(&self.words) {
            *word = summary.generated | (*word & !summary.killed);
        }
    }
}

pub struct LiveIns {
    words: Vec<usize>,
    words_per_block: usize,
    // Only leaf selection tracks how many values a successor observes.
    values: Option<Vec<usize>>,
}

impl LiveIns {
    /// Compute the least fixed point. `exit` requests multiple-value demand
    /// tracking and gives the exit block that observes the complete values buffer.
    pub fn solve<S: IntoIterator<Item = LiveEdge>>(
        blocks: usize,
        registers: usize,
        successors: impl Fn(usize) -> S,
        transfer: impl Fn(usize, &mut LiveSet),
        exit: Option<usize>,
    ) -> Self {
        Self::solve_worklist::<S, false>(
            blocks,
            registers,
            successors,
            |index, live, _| transfer(index, live),
            exit,
        )
    }

    /// Solve register liveness for fixed generation and kill sets, reporting
    /// repeated block visits to the transfer.
    pub fn solve_registers<S: IntoIterator<Item = LiveEdge>>(
        blocks: usize,
        registers: usize,
        successors: impl Fn(usize) -> S,
        transfer: impl FnMut(usize, &mut LiveSet, bool),
    ) -> Self {
        Self::solve_worklist::<S, true>(blocks, registers, successors, transfer, None)
    }

    fn solve_worklist<S: IntoIterator<Item = LiveEdge>, const REGISTERS_ONLY: bool>(
        blocks: usize,
        registers: usize,
        successors: impl Fn(usize) -> S,
        mut transfer: impl FnMut(usize, &mut LiveSet, bool),
        exit: Option<usize>,
    ) -> Self {
        let mut live = LiveSet::new(registers);
        let words_per_block = live.words.len();
        let mut result = Self {
            words: vec![
                0;
                blocks
                    .checked_mul(words_per_block)
                    .expect("register liveness matrix")
            ],
            words_per_block,
            values: exit.map(|index| {
                let mut values = vec![0; blocks];
                values[index] = usize::MAX;
                values
            }),
        };
        let mut predecessors = vec![Vec::new(); blocks];
        for index in 0..blocks {
            for edge in successors(index) {
                // A self-edge feeds back only a subset of the new live-in set.
                // Applying the same generation and kills cannot add anything.
                if !REGISTERS_ONLY || edge.target != index {
                    predecessors[edge.target].push(index);
                }
            }
        }
        let mut pending: Vec<_> = (0..blocks).filter(|&index| Some(index) != exit).collect();
        let mut queued = vec![true; blocks];
        // A block is re-pushed only after it has been processed once, so this
        // records the first visit instead of inferring it from stack depth.
        // The transfer uses it to replay short blocks and summarize revisited
        // ones; `solve` ignores it.
        let mut visited = vec![false; blocks];
        while let Some(index) = pending.pop() {
            let revisiting = std::mem::replace(&mut visited[index], true);
            queued[index] = false;
            result.live_out(successors(index), &mut live);
            transfer(index, &mut live, revisiting);
            if live.words != result.row(index) || live.values != result.values(index) {
                let start = index * words_per_block;
                result.words[start..start + words_per_block].copy_from_slice(&live.words);
                if let Some(values) = &mut result.values {
                    values[index] = live.values;
                }
                for &pred in &predecessors[index] {
                    if !std::mem::replace(&mut queued[pred], true) {
                        pending.push(pred);
                    }
                }
            }
        }
        result
    }

    fn row(&self, index: usize) -> &[usize] {
        let start = index * self.words_per_block;
        &self.words[start..start + self.words_per_block]
    }

    pub fn registers(&self, index: usize) -> impl Iterator<Item = Register> + '_ {
        set_registers(self.row(index))
    }

    pub fn values(&self, index: usize) -> usize {
        self.values.as_ref().map_or(0, |values| values[index])
    }

    pub fn live_out(&self, successors: impl IntoIterator<Item = LiveEdge>, out: &mut LiveSet) {
        out.words.fill(0);
        out.values = 0;
        for edge in successors {
            out.values = out.values.max(self.values(edge.target));
            out.union(self.row(edge.target), edge);
        }
    }
}

// Raw leaf selection also accepts unvalidated instruction slices. In that
// path, ignore invalid operands in liveness; selection checks them when used.
fn for_each_in_frame(operands: RegisterSet<'_>, registers: usize, mut f: impl FnMut(Register)) {
    match operands {
        RegisterSet::Span(span) => (span.start..span.end.min(registers)).for_each(f),
        RegisterSet::OneAndSpan(reg, span) => {
            f(reg);
            (span.start..span.end.min(registers)).for_each(f);
        }
        _ => operands.for_each(f),
    }
}

fn bit(reg: Register) -> usize {
    1 << (reg % WORD_BITS)
}

fn set_registers(live: &[usize]) -> impl Iterator<Item = Register> + '_ {
    live.iter().enumerate().flat_map(|(index, &word)| {
        let mut word = word;
        std::iter::from_fn(move || {
            if word == 0 {
                return None;
            }
            let reg = index * WORD_BITS + word.trailing_zeros() as usize;
            word &= word - 1;
            Some(reg)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_supplies_only_its_own_condition_and_cleared_suffix() {
        let edges = [
            [
                Some(LiveEdge::ordinary(1)),
                Some(LiveEdge::recovery(2, 65, Some(1))),
            ],
            [None, None],
            [None, None],
        ];
        let live = LiveIns::solve(
            3,
            66,
            |index| edges[index].into_iter().flatten(),
            |index, out| {
                if index == 1 {
                    out.insert(1);
                } else if index == 2 {
                    for reg in [1, 63, 64, 65] {
                        out.insert(reg);
                    }
                }
            },
            None,
        );
        assert_eq!(live.registers(0).collect::<Vec<_>>(), [1, 63, 64]);
        let mut out = LiveSet::new(66);
        live.live_out([LiveEdge::recovery(2, 65, Some(1))], &mut out);
        assert_eq!(out.registers().collect::<Vec<_>>(), [63, 64]);
    }

    #[test]
    fn a_definition_that_reads_its_destination_keeps_the_input_live() {
        let mut live = LiveSet::new(2);
        live.transfer_registers(&Instruction::Clear(0..usize::MAX));
        live.transfer_registers(&Instruction::Copy(1, 1));
        assert_eq!(live.registers().collect::<Vec<_>>(), [1]);
    }

    #[test]
    fn summary_replays_aliased_operands_and_clears_across_words() {
        let boundary = WORD_BITS;
        let registers = 2 * boundary + 1;
        let instructions = [
            Instruction::Copy(boundary, boundary + 1),
            Instruction::Clear(boundary - 1..boundary + 1),
            Instruction::Copy(boundary - 1, boundary - 1),
            Instruction::Copy(boundary + 2, boundary + 2),
        ];
        let mut summary = RegisterTransfer::new(registers);
        summary.then_clear_from(boundary + 2);
        for instruction in instructions.iter().rev() {
            summary.then_instruction(instruction);
        }
        summary.then_read(boundary);

        let inputs = [
            0,
            boundary - 1,
            boundary,
            boundary + 1,
            boundary + 2,
            boundary + 3,
            2 * boundary,
        ];
        for mask in 0..(1 << inputs.len()) {
            let mut direct = LiveSet::new(registers);
            let mut composed = LiveSet::new(registers);
            for (index, reg) in inputs.into_iter().enumerate() {
                if mask & (1 << index) != 0 {
                    direct.insert(reg);
                    composed.insert(reg);
                }
            }
            direct.clear_from(boundary + 2);
            for instruction in instructions.iter().rev() {
                direct.transfer_registers(instruction);
            }
            direct.insert(boundary);
            summary.apply(&mut composed);
            assert_eq!(direct.words, composed.words, "live-out subset {mask:b}");
        }
    }

    #[test]
    fn a_self_edge_does_not_change_the_least_solution() {
        let edges = [
            [
                Some(LiveEdge::ordinary(1)),
                Some(LiveEdge::recovery(0, 2, Some(0))),
            ],
            [Some(LiveEdge::ordinary(0)), Some(LiveEdge::ordinary(2))],
            [None, None],
        ];
        let mut visited = [false; 3];
        let live = LiveIns::solve_registers(
            3,
            4,
            |index| edges[index].into_iter().flatten(),
            |index, out, revisiting| {
                assert_eq!(revisiting, std::mem::replace(&mut visited[index], true));
                match index {
                    0 => out.transfer_registers(&Instruction::Copy(1, 2)),
                    1 => out.transfer_registers(&Instruction::Copy(2, 3)),
                    _ => {
                        out.insert(0);
                        out.insert(1);
                    }
                }
            },
        );
        assert_eq!(live.registers(0).collect::<Vec<_>>(), [0, 2, 3]);
        assert_eq!(live.registers(1).collect::<Vec<_>>(), [0, 1, 3]);
    }
}
