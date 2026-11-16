use std::collections::{HashMap, HashSet};

use crate::{AssertionContext, Input, Inst, MatchMode, Program, Set, word};

pub(crate) const DEAD: u32 = u32::MAX;
const MAX_DFA_STATES: usize = 512;
const MAX_DFA_WORK: usize = 1 << 17;

pub(crate) struct Dfa {
    pub(crate) start_nonword: u32,
    pub(crate) start_word: u32,
    states: Vec<DfaState>,
}

pub(crate) struct DfaState {
    ascii: [u32; 128],
    ranges: Vec<(u32, u32, u32)>,
    pub(crate) accept_eof: Option<usize>,
    pub(crate) accept_word: Option<usize>,
    pub(crate) accept_nonword: Option<usize>,
    needs_more_at_chunk_end: bool,
}

impl DfaState {
    #[inline]
    pub(crate) fn step(&self, unit: u32) -> u32 {
        if let Some(&next) = self.ascii.get(unit as usize) {
            return next;
        }
        let idx = self.ranges.partition_point(|&(low, _, _)| low <= unit);
        if idx != 0 && unit <= self.ranges[idx - 1].1 {
            self.ranges[idx - 1].2
        } else {
            DEAD
        }
    }

    #[inline]
    pub(crate) fn accept(&self, right: Option<u32>, eof: bool) -> Option<usize> {
        match right {
            Some(unit) => {
                if word(Some(unit)) {
                    self.accept_word
                } else {
                    self.accept_nonword
                }
            }
            None if eof || !self.needs_more_at_chunk_end() => self.accept_eof,
            None => None,
        }
    }

    #[inline]
    pub(crate) fn needs_more_at_chunk_end(&self) -> bool {
        self.needs_more_at_chunk_end
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct StateKey {
    frontier: Vec<usize>,
    start: bool,
    left_word: bool,
}

struct EpsilonClosure<'a> {
    accept: Option<usize>,
    consumers: Vec<(&'a Set, usize)>,
    blocked_before_accept: bool,
}

fn epsilon_closure<'a>(
    program: &'a Program,
    frontier: &[usize],
    context: AssertionContext,
) -> EpsilonClosure<'a> {
    let mut pending: Vec<(usize, Vec<usize>)> =
        frontier.iter().rev().map(|&pc| (pc, Vec::new())).collect();
    let mut seen = HashSet::new();
    let mut accept = None;
    let mut consumers = Vec::new();
    let mut blocked_before_accept = false;
    while let Some((mut pc, mut progress)) = pending.pop() {
        loop {
            if !seen.insert((pc, progress.clone())) {
                break;
            }
            match &program.instructions[pc] {
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
                Inst::Test(assertion, next) => match context.test(*assertion) {
                    Some(true) => pc = *next,
                    Some(false) => break,
                    None => {
                        blocked_before_accept |= accept.is_none();
                        break;
                    }
                },
                Inst::Consume(set, next) => {
                    consumers.push((set, *next));
                    break;
                }
                Inst::Accept(rule) => {
                    if accept.is_none() && (program.mode != MatchMode::Full || context.eof) {
                        accept = Some(*rule);
                        if program.mode != MatchMode::LongestPrefix {
                            pending.clear();
                        }
                    }
                    break;
                }
            }
        }
    }
    EpsilonClosure {
        accept,
        consumers,
        blocked_before_accept,
    }
}

impl Dfa {
    pub(crate) fn compile(program: &Program) -> Option<Self> {
        if program.graphemes
            || !matches!(
                program.mode,
                MatchMode::Full | MatchMode::Prefix | MatchMode::LongestPrefix
            )
            || program.entries.is_empty()
        {
            return None;
        }
        let cuts = program.representatives();
        let has_word = program.instructions.iter().any(|inst| {
            matches!(
                inst,
                Inst::Test(crate::Assertion::Word | crate::Assertion::NotWord, _)
            )
        });
        let has_right_assertion = has_word
            || program
                .instructions
                .iter()
                .any(|inst| matches!(inst, Inst::Test(crate::Assertion::End, _)));
        let nonword_key = StateKey {
            frontier: program.entries.clone(),
            start: true,
            left_word: false,
        };
        let mut keys = vec![nonword_key.clone()];
        let mut indices = HashMap::from([(nonword_key, 0u32)]);
        let start_word = if has_word {
            let word_key = StateKey {
                frontier: program.entries.clone(),
                start: true,
                left_word: true,
            };
            indices.insert(word_key.clone(), 1u32);
            keys.push(word_key);
            1u32
        } else {
            0u32
        };

        let mut states = Vec::new();
        let mut work = 0usize;
        let mut index = 0usize;
        while index < keys.len() {
            let key = keys[index].clone();
            let eof_exp = epsilon_closure(
                program,
                &key.frontier,
                AssertionContext {
                    start: key.start,
                    left_word: key.left_word,
                    right: None,
                    grapheme: None,
                    eof: true,
                },
            );
            let nonword_exp = epsilon_closure(
                program,
                &key.frontier,
                AssertionContext {
                    start: key.start,
                    left_word: key.left_word,
                    right: Some(0),
                    grapheme: None,
                    eof: false,
                },
            );
            let word_exp = if has_word {
                Some(epsilon_closure(
                    program,
                    &key.frontier,
                    AssertionContext {
                        start: key.start,
                        left_word: key.left_word,
                        right: Some(b'a' as u32),
                        grapheme: None,
                        eof: false,
                    },
                ))
            } else {
                None
            };

            let mut ascii = [DEAD; 128];
            let mut ranges: Vec<(u32, u32, u32)> = Vec::new();
            let mut has_transitions = false;
            let limit = program.domain.limit();

            for (pos, &low) in cuts.iter().enumerate() {
                let high = cuts
                    .get(pos + 1)
                    .map_or(limit, |&next| next.saturating_sub(1))
                    .min(limit);
                let is_word = word(Some(low));
                let consumers = if is_word {
                    &word_exp.as_ref().unwrap_or(&nonword_exp).consumers
                } else {
                    &nonword_exp.consumers
                };
                work = work.saturating_add(consumers.len().max(1));
                if work > MAX_DFA_WORK {
                    return None;
                }
                let mut next_frontier = Vec::new();
                for &(set, next_pc) in consumers {
                    if set.contains(low) && !next_frontier.contains(&next_pc) {
                        next_frontier.push(next_pc);
                    }
                }
                let next_id = if next_frontier.is_empty() {
                    DEAD
                } else {
                    has_transitions = true;
                    let next_key = StateKey {
                        frontier: next_frontier,
                        start: false,
                        left_word: has_word && is_word,
                    };
                    if let Some(&existing) = indices.get(&next_key) {
                        existing
                    } else {
                        if keys.len() >= MAX_DFA_STATES {
                            return None;
                        }
                        let id = keys.len() as u32;
                        indices.insert(next_key.clone(), id);
                        keys.push(next_key);
                        id
                    }
                };
                if next_id != DEAD {
                    if low < 128 {
                        let ascii_end = high.min(127) as usize;
                        for slot in &mut ascii[low as usize..=ascii_end] {
                            *slot = next_id;
                        }
                    }
                    if high >= 128 {
                        let range_low = low.max(128);
                        match ranges.last_mut() {
                            Some((_, prev_high, prev_id))
                                if *prev_id == next_id && *prev_high + 1 == range_low =>
                            {
                                *prev_high = high;
                            }
                            _ => ranges.push((range_low, high, next_id)),
                        }
                    }
                }
            }

            let accept_word = word_exp.as_ref().map_or(nonword_exp.accept, |e| e.accept);
            let needs_more_at_chunk_end = has_transitions
                || eof_exp.accept != accept_word
                || eof_exp.accept != nonword_exp.accept
                // The same rule can accept through different capture paths. A
                // higher priority assertion can still change the capture results.
                || (has_right_assertion
                    && eof_exp
                        .accept
                        .is_some_and(|rule| program.capture_counts[rule] != 0)
                    && epsilon_closure(
                        program,
                        &key.frontier,
                        AssertionContext {
                            start: key.start,
                            left_word: key.left_word,
                            right: None,
                            grapheme: None,
                            eof: false,
                        },
                    )
                    .blocked_before_accept);
            states.push(DfaState {
                ascii,
                ranges,
                accept_eof: eof_exp.accept,
                accept_word,
                accept_nonword: nonword_exp.accept,
                needs_more_at_chunk_end,
            });
            index += 1;
        }

        Some(Self {
            start_nonword: 0,
            start_word,
            states,
        })
    }

    #[inline]
    pub(crate) fn state(&self, id: u32) -> &DfaState {
        &self.states[id as usize]
    }

    #[inline]
    pub(crate) fn initial(&self, left: Option<u32>) -> u32 {
        if word(left) {
            self.start_word
        } else {
            self.start_nonword
        }
    }
}

pub(crate) fn run_dfa<E>(
    program: &Program,
    dfa: &Dfa,
    matcher: &mut crate::Matcher,
    input: Input<'_>,
    mut failed_suffixes: Option<&mut crate::FailedSuffixes>,
    mut checkpoint: impl FnMut() -> Result<(), E>,
) -> Result<crate::Outcome, E> {
    let mut work = 0usize;
    let memoize = failed_suffixes.is_some() && program.is_prefix();
    let mut state_id = matcher.dfa_state.unwrap_or_else(|| {
        let index = matcher.position - input.base;
        let left = if index == 0 {
            input.left
        } else {
            input.units.get(index - 1).copied()
        };
        let id = dfa.initial(left);
        matcher.dfa_state = Some(id);
        id
    });

    loop {
        if work == 0 {
            checkpoint()?;
        }
        work = (work + 1) % 1024;

        let position = matcher.position;
        if memoize
            && let Some(cache) = failed_suffixes.as_deref_mut()
            && cache.dfa.contains(&(position, state_id))
        {
            cache.dfa.extend(matcher.dfa_since_accept.drain(..));
            matcher.dfa_state = None;
            return finish_dfa_match(
                program,
                matcher.origin,
                matcher.dfa_best.take(),
                input,
                &mut checkpoint,
            );
        }

        let index = position - input.base;
        let right = input.units.get(index).copied();
        let state = dfa.state(state_id);

        if right.is_none() && !input.eof && state.needs_more_at_chunk_end() {
            matcher.dfa_state = Some(state_id);
            return Ok(crate::Outcome::NeedMore);
        }

        if let Some(rule) = state.accept(right, input.eof) {
            matcher.dfa_best = Some((rule, position));
            matcher.dfa_since_accept.clear();
        } else if memoize && (right.is_some() || input.eof) {
            matcher.dfa_since_accept.push((position, state_id));
        }

        let Some(unit) = right else {
            if let Some(cache) = failed_suffixes {
                cache.dfa.extend(matcher.dfa_since_accept.drain(..));
            }
            matcher.dfa_state = None;
            return finish_dfa_match(
                program,
                matcher.origin,
                matcher.dfa_best.take(),
                input,
                &mut checkpoint,
            );
        };

        let next_id = state.step(unit);
        if next_id == DEAD {
            if let Some(cache) = failed_suffixes {
                cache.dfa.extend(matcher.dfa_since_accept.drain(..));
            }
            matcher.dfa_state = None;
            return finish_dfa_match(
                program,
                matcher.origin,
                matcher.dfa_best.take(),
                input,
                &mut checkpoint,
            );
        }
        state_id = next_id;
        matcher.position += 1;
    }
}

fn finish_dfa_match<E>(
    program: &Program,
    origin: usize,
    best: Option<(usize, usize)>,
    input: Input<'_>,
    checkpoint: &mut impl FnMut() -> Result<(), E>,
) -> Result<crate::Outcome, E> {
    let Some((rule, end)) = best else {
        return Ok(crate::Outcome::Miss);
    };
    let groups = if program.capture_counts[rule] == 0 {
        Vec::new()
    } else {
        program.capture_groups(rule, origin, end, input, checkpoint)?
    };
    Ok(crate::Outcome::Match(crate::Match {
        rule,
        range: origin..end,
        groups,
    }))
}

pub(crate) fn run_dfa_bytes<E>(
    program: &Program,
    dfa: &Dfa,
    bytes: &[u8],
    mut checkpoint: impl FnMut() -> Result<(), E>,
) -> Result<Option<crate::Match>, E> {
    let mut state_id = dfa.start_nonword;
    let mut best: Option<(usize, usize)> = None;
    let mut pos = 0usize;
    let mut work = 0usize;
    loop {
        if work == 0 {
            checkpoint()?;
        }
        work = (work + 1) & 1023;
        let state = dfa.state(state_id);
        let right = bytes.get(pos).copied().map(u32::from);
        if let Some(rule) = state.accept(right, true) {
            best = Some((rule, pos));
        }
        let Some(unit) = right else {
            break;
        };
        let next_id = state.step(unit);
        if next_id == DEAD {
            break;
        }
        state_id = next_id;
        pos += 1;
    }
    let Some((rule, end)) = best else {
        return Ok(None);
    };
    let groups = if program.capture_counts[rule] == 0 {
        Vec::new()
    } else {
        let slice_end = bytes.len().min(end + 1);
        let prefix: Vec<u32> = bytes[..slice_end].iter().copied().map(u32::from).collect();
        program.capture_groups(
            rule,
            0,
            end,
            Input {
                base: 0,
                units: &prefix,
                left: None,
                graphemes: &[],
                eof: slice_end == bytes.len(),
            },
            &mut checkpoint,
        )?
    };
    Ok(Some(crate::Match {
        rule,
        range: 0..end,
        groups,
    }))
}

pub(crate) fn run_dfa_str<E>(
    program: &Program,
    dfa: &Dfa,
    text: &str,
    mut checkpoint: impl FnMut() -> Result<(), E>,
) -> Result<Option<crate::Match>, E> {
    let bytes = text.as_bytes();
    let mut state_id = dfa.start_nonword;
    let mut best: Option<(usize, usize, usize)> = None;
    let mut char_pos = 0usize;
    let mut byte_pos = 0usize;
    let mut work = 0usize;
    loop {
        if work == 0 {
            checkpoint()?;
        }
        work = (work + 1) & 1023;
        let state = dfa.state(state_id);
        let (right, next_byte) = if byte_pos < bytes.len() {
            let b = bytes[byte_pos];
            if b < 128 {
                (Some(u32::from(b)), byte_pos + 1)
            } else {
                let ch = text[byte_pos..].chars().next().unwrap();
                (Some(u32::from(ch)), byte_pos + ch.len_utf8())
            }
        } else {
            (None, byte_pos)
        };
        if let Some(rule) = state.accept(right, true) {
            best = Some((rule, char_pos, byte_pos));
        }
        let Some(unit) = right else {
            break;
        };
        let next_id = state.step(unit);
        if next_id == DEAD {
            break;
        }
        state_id = next_id;
        char_pos += 1;
        byte_pos = next_byte;
    }
    let Some((rule, end_char, end_byte)) = best else {
        return Ok(None);
    };
    let groups = if program.capture_counts[rule] == 0 {
        Vec::new()
    } else {
        let slice_end = if end_byte < bytes.len() {
            let b = bytes[end_byte];
            if b < 128 {
                end_byte + 1
            } else {
                end_byte + text[end_byte..].chars().next().unwrap().len_utf8()
            }
        } else {
            end_byte
        };
        let prefix: Vec<u32> = text[..slice_end].chars().map(u32::from).collect();
        program.capture_groups(
            rule,
            0,
            end_char,
            Input {
                base: 0,
                units: &prefix,
                left: None,
                graphemes: &[],
                eof: slice_end == bytes.len(),
            },
            &mut checkpoint,
        )?
    };
    Ok(Some(crate::Match {
        rule,
        range: 0..end_char,
        groups,
    }))
}
