use std::collections::{HashMap, HashSet};

use crate::{AssertionContext, InputDomain, Inst, MatchMode, Program, word};

#[derive(Debug, PartialEq, Eq)]
pub struct Coverage {
    pub exhaustive: Option<bool>,
    pub witness: Option<Vec<u32>>,
    pub selectable: Option<Vec<bool>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CoverageState {
    frontier: Vec<usize>,
    best: Option<usize>,
    start: bool,
    left_word: bool,
}

struct ReachableState {
    state: CoverageState,
    previous: Option<(usize, u32)>,
}

impl Program {
    pub fn coverage(&self) -> Coverage {
        let mut result = Coverage {
            exhaustive: None,
            witness: None,
            selectable: None,
        };
        if self.graphemes {
            return result;
        }
        let initial = CoverageState {
            frontier: self.entries.clone(),
            best: None,
            start: true,
            left_word: false,
        };
        let mut inputs = vec![ReachableState {
            state: initial.clone(),
            previous: None,
        }];
        let mut indices = HashMap::from([(initial, 0)]);
        let mut selected = vec![false; self.entries.len()];
        let alphabet = self.representatives();
        let mut work = alphabet
            .len()
            .saturating_mul(self.instructions.len())
            .max(1);
        let mut index = 0;
        while index < inputs.len() {
            let state = &inputs[index].state;
            if let Some(selected_rule) = self.transition(state, None).best {
                selected[selected_rule] = true;
            } else if result.witness.is_none() {
                let mut material = Vec::new();
                let mut current = index;
                while let Some((previous, unit)) = inputs[current].previous {
                    material.push(unit);
                    current = previous;
                }
                material.reverse();
                result.exhaustive = Some(false);
                result.witness = Some(material);
            }
            let cost = alphabet.len().saturating_mul(state.frontier.len().max(1));
            work = work.saturating_add(cost);
            if work > 1 << 20 || inputs.len() > 8192 {
                return result;
            }
            for &unit in &alphabet {
                let state = self.transition(&inputs[index].state, Some(unit));
                if !indices.contains_key(&state) {
                    indices.insert(state.clone(), inputs.len());
                    inputs.push(ReachableState {
                        state,
                        previous: Some((index, unit)),
                    });
                }
            }
            index += 1;
        }
        result.exhaustive.get_or_insert(true);
        result.selectable = Some(selected);
        result
    }

    pub(crate) fn representatives(&self) -> Vec<u32> {
        let mut cuts = vec![0];
        if self.domain == InputDomain::Text {
            cuts.extend([0xd800, 0xe000]);
        }
        let mut word_boundary = false;
        for inst in &self.instructions {
            match inst {
                Inst::Consume(set, _) => {
                    cuts.extend(
                        set.0
                            .iter()
                            .flat_map(|&(low, high)| [low, high.saturating_add(1)]),
                    );
                }
                Inst::Test(crate::Assertion::Word | crate::Assertion::NotWord, _) => {
                    word_boundary = true
                }
                _ => {}
            }
        }
        if word_boundary {
            cuts.extend([48, 58, 65, 91, 95, 96, 97, 123]);
        }
        cuts.sort_unstable();
        cuts.dedup();
        cuts.retain(|&unit| self.domain.contains(unit));
        cuts
    }

    fn transition(&self, state: &CoverageState, unit: Option<u32>) -> CoverageState {
        let context = AssertionContext {
            start: state.start,
            left_word: state.left_word,
            right: unit,
            grapheme: None,
            eof: unit.is_none(),
        };
        let mut pending: Vec<_> = state
            .frontier
            .iter()
            .rev()
            .map(|&pc| (pc, Vec::new()))
            .collect();
        let mut seen = HashSet::new();
        let mut frontier = Vec::new();
        let mut best = state.best;
        let mut matched_here = false;
        while let Some((mut pc, mut progress)) = pending.pop() {
            loop {
                if !seen.insert((pc, progress.clone())) {
                    break;
                }
                match &self.instructions[pc] {
                    Inst::Begin(_, next) | Inst::Save(_, next) => pc = *next,
                    Inst::RepeatEnter(loop_id, next) => {
                        progress.push(*loop_id);
                        pc = *next;
                    }
                    Inst::RepeatExit(loop_id, next) => {
                        if progress.contains(loop_id) {
                            break;
                        }
                        pc = *next;
                    }
                    Inst::Split(first, second) => {
                        pending.push((*second, progress.clone()));
                        pc = *first;
                    }
                    Inst::Test(test, next) => {
                        if context.test(*test) == Some(true) {
                            pc = *next;
                        } else {
                            break;
                        }
                    }
                    Inst::Consume(set, next) => {
                        if unit.is_some_and(|unit| set.contains(unit)) && !frontier.contains(next) {
                            frontier.push(*next);
                        }
                        break;
                    }
                    Inst::Accept(rule) => {
                        if !matched_here && (self.mode != MatchMode::Full || unit.is_none()) {
                            matched_here = true;
                            best = Some(*rule);
                            if self.mode != MatchMode::LongestPrefix {
                                pending.clear();
                            }
                        }
                        break;
                    }
                }
            }
        }
        CoverageState {
            frontier,
            best,
            start: false,
            left_word: word(unit),
        }
    }
}
