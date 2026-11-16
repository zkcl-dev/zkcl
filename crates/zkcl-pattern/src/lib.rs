mod coverage;
mod dfa;
mod regex;
pub mod unicode;

use std::collections::HashSet;
use std::ops::Range;

pub use coverage::Coverage;
pub use regex::{RegexError, parse_regex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputDomain {
    Text,
    Bytes,
}

impl InputDomain {
    fn limit(self) -> u32 {
        match self {
            Self::Text => 0x10ffff,
            Self::Bytes => 255,
        }
    }

    fn contains(self, unit: u32) -> bool {
        unit <= self.limit() && !(0xd800..=0xdfff).contains(&unit)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchMode {
    Full,
    Prefix,
    LongestPrefix,
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assertion {
    Start,
    End,
    Word,
    NotWord,
    Grapheme,
    NotGrapheme,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Set(Vec<(u32, u32)>);

impl Set {
    pub fn from_ranges(ranges: impl IntoIterator<Item = (u32, u32)>) -> Self {
        let mut ranges: Vec<_> = ranges
            .into_iter()
            .filter(|(low, high)| low <= high)
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (low, high) in ranges {
            match merged.last_mut() {
                Some((_, end)) if low <= end.saturating_add(1) => *end = (*end).max(high),
                _ => merged.push((low, high)),
            }
        }
        Self(merged)
    }

    pub fn range(low: u32, high: u32) -> Self {
        Self::from_ranges([(low, high)])
    }

    pub fn property(property: crate::unicode::Property) -> Self {
        Self::from_ranges(
            property
                .ranges()
                .into_iter()
                .map(|range| (*range.start(), *range.end())),
        )
    }

    /// Test one input unit.
    ///
    /// Literals and small classes have a single range, and both matchers call
    /// this per consumed unit, so that case is tested directly instead of
    /// searching the range table.
    #[inline]
    pub fn contains(&self, unit: u32) -> bool {
        match self.0.as_slice() {
            [] => false,
            &[(low, high)] => (low..=high).contains(&unit),
            ranges => {
                let index = ranges.partition_point(|(low, _)| *low <= unit);
                index != 0 && unit <= ranges[index - 1].1
            }
        }
    }

    fn complement(&self, domain: InputDomain) -> Self {
        let limit = domain.limit();
        let mut next = 0;
        let mut ranges = Vec::new();
        for &(low, high) in &self.0 {
            if low > limit {
                break;
            }
            if low > next {
                ranges.push((next, low - 1));
            }
            if high >= limit {
                return Self(ranges);
            }
            next = high + 1;
        }
        if next <= limit {
            ranges.push((next, limit));
        }
        Self(ranges)
    }
}

#[derive(Clone, Debug)]
pub enum Rule {
    Literal(Vec<u32>),
    Set(Set),
    Assert(Assertion),
    Grapheme,
    Sequence(Vec<Self>),
    Choice(Vec<Self>),
    Repeat {
        body: Box<Self>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    Capture {
        group: usize,
        body: Box<Self>,
    },
}

impl Rule {
    pub fn repeat(self, min: usize, max: Option<usize>, greedy: bool) -> Self {
        Self::Repeat {
            body: Box::new(self),
            min,
            max,
            greedy,
        }
    }

    pub fn width(&self) -> (usize, Option<usize>) {
        match self {
            Self::Literal(units) => (units.len(), Some(units.len())),
            Self::Set(_) => (1, Some(1)),
            Self::Assert(_) => (0, Some(0)),
            Self::Grapheme => (1, None),
            Self::Capture { body, .. } => body.width(),
            Self::Sequence(parts) => parts.iter().map(Self::width).fold(
                (0usize, Some(0usize)),
                |(min, max), (lo, hi)| {
                    (
                        min.saturating_add(lo),
                        max.zip(hi).and_then(|(max, hi)| max.checked_add(hi)),
                    )
                },
            ),
            Self::Choice(parts) => parts
                .iter()
                .map(Self::width)
                .reduce(|(min, max), (lo, hi)| {
                    (min.min(lo), max.zip(hi).map(|(max, hi)| max.max(hi)))
                })
                .unwrap_or((0, Some(0))),
            Self::Repeat { body, min, max, .. } => {
                let (low, high) = body.width();
                let maximum = match (*max, high) {
                    (Some(0), _) | (_, Some(0)) => Some(0),
                    (Some(max), Some(high)) => high.checked_mul(max),
                    _ => None,
                };
                (low.saturating_mul(*min), maximum)
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum CompileError {
    #[error("lexical program exceeds the instruction limit")]
    InstructionLimit,
    #[error("literal unit {unit:#x} is outside the {domain:?} input domain")]
    LiteralOutsideDomain { unit: u32, domain: InputDomain },
    #[error("grapheme rules require text")]
    GraphemeInBytePattern,
    #[error("empty lexical alternative")]
    EmptyAlternation,
    #[error("lexical capture index {index} is outside the declared count {count}")]
    InvalidCaptureIndex { index: usize, count: usize },
    #[error("capture count {count} exceeds the lexical program limit {limit}")]
    CaptureLimit { count: usize, limit: usize },
    #[error("lazy quantifiers are not supported in longest-prefix mode")]
    LazyLongestPrefix,
    #[error("repetition upper bound {max} is below its lower bound {min}")]
    InvertedRepetitionBounds { min: usize, max: usize },
    #[error("repetition count {count} exceeds the lexical instruction limit {limit}")]
    RepetitionLimit { count: usize, limit: usize },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Inst {
    Consume(Set, usize),
    Test(Assertion, usize),
    Split(usize, usize),
    Begin(usize, usize),
    Save(usize, usize),
    RepeatEnter(usize, usize),
    RepeatExit(usize, usize),
    Accept(usize),
}

struct Compiler {
    instructions: Vec<Inst>,
    domain: InputDomain,
    mode: MatchMode,
    loop_count: usize,
}

const PROGRAM_LIMIT: usize = 1 << 18;

impl Compiler {
    fn push(&mut self, inst: Inst) -> Result<usize, CompileError> {
        let pc = self.instructions.len();
        if pc == PROGRAM_LIMIT {
            return Err(CompileError::InstructionLimit);
        }
        self.instructions.push(inst);
        Ok(pc)
    }

    fn rule(&mut self, rule: &Rule, next: usize, groups: usize) -> Result<usize, CompileError> {
        let any = || Set::range(0, self.domain.limit());
        match rule {
            Rule::Literal(units) => {
                let mut enter = next;
                for &unit in units.iter().rev() {
                    if !self.domain.contains(unit) {
                        return Err(CompileError::LiteralOutsideDomain {
                            unit,
                            domain: self.domain,
                        });
                    }
                    enter = self.push(Inst::Consume(Set::range(unit, unit), enter))?;
                }
                Ok(enter)
            }
            Rule::Set(set) => self.push(Inst::Consume(set.clone(), next)),
            Rule::Assert(assertion) => self.push(Inst::Test(*assertion, next)),
            Rule::Grapheme => {
                if self.domain == InputDomain::Bytes {
                    return Err(CompileError::GraphemeInBytePattern);
                }
                let any = any();
                let end = self.push(Inst::Test(Assertion::Grapheme, next))?;
                let split = self.push(Inst::Split(end, end))?;
                let more = self.push(Inst::Consume(any, split))?;
                let non_boundary = self.push(Inst::Test(Assertion::NotGrapheme, more))?;
                self.instructions[split] = Inst::Split(end, non_boundary);
                self.push(Inst::Test(Assertion::Grapheme, more))
            }
            Rule::Sequence(parts) => parts
                .iter()
                .rev()
                .try_fold(next, |next, part| self.rule(part, next, groups)),
            Rule::Choice(parts) => {
                let mut parts = parts.iter().rev();
                let last = parts.next().ok_or(CompileError::EmptyAlternation)?;
                let mut enter = self.rule(last, next, groups)?;
                for part in parts {
                    let first = self.rule(part, next, groups)?;
                    enter = self.push(Inst::Split(first, enter))?;
                }
                Ok(enter)
            }
            Rule::Capture { group, body } => {
                if *group >= groups {
                    return Err(CompileError::InvalidCaptureIndex {
                        index: *group,
                        count: groups,
                    });
                }
                let end = self.push(Inst::Save(group * 2 + 1, next))?;
                let enter = self.rule(body, end, groups)?;
                self.push(Inst::Save(group * 2, enter))
            }
            Rule::Repeat {
                body,
                min,
                max,
                greedy,
            } => {
                if !greedy && self.mode == MatchMode::LongestPrefix {
                    return Err(CompileError::LazyLongestPrefix);
                }
                if let Some(max) = max
                    && max < min
                {
                    return Err(CompileError::InvertedRepetitionBounds {
                        min: *min,
                        max: *max,
                    });
                }
                let count = max.unwrap_or(*min);
                if count > PROGRAM_LIMIT {
                    return Err(CompileError::RepetitionLimit {
                        count,
                        limit: PROGRAM_LIMIT,
                    });
                }
                let fork = |more, done| {
                    if *greedy {
                        Inst::Split(more, done)
                    } else {
                        Inst::Split(done, more)
                    }
                };
                let mut enter = next;
                if let Some(max) = max {
                    for _ in *min..*max {
                        let more = self.rule(body, enter, groups)?;
                        enter = self.push(fork(more, next))?;
                    }
                } else if body.width().1 != Some(0) {
                    let split = self.push(Inst::Split(next, next))?;
                    let more = if body.width().0 == 0 {
                        let loop_id = self.loop_count;
                        self.loop_count += 1;
                        let exit = self.push(Inst::RepeatExit(loop_id, split))?;
                        let body = self.rule(body, exit, groups)?;
                        self.push(Inst::RepeatEnter(loop_id, body))?
                    } else {
                        self.rule(body, split, groups)?
                    };
                    self.instructions[split] = fork(more, next);
                    enter = split;
                }
                for _ in 0..*min {
                    enter = self.rule(body, enter, groups)?;
                }
                Ok(enter)
            }
        }
    }
}

pub struct Program {
    instructions: Vec<Inst>,
    entries: Vec<usize>,
    capture_counts: Vec<usize>,
    domain: InputDomain,
    mode: MatchMode,
    all_rules_advance: bool,
    graphemes: bool,
    dfa: Option<dfa::Dfa>,
}

impl Program {
    pub fn compile(
        domain: InputDomain,
        mode: MatchMode,
        rules: &[(Rule, usize)],
    ) -> Result<Self, CompileError> {
        let mut compiler = Compiler {
            instructions: Vec::new(),
            domain,
            mode,
            loop_count: 0,
        };
        let mut entries = Vec::with_capacity(rules.len());
        let mut capture_counts = Vec::with_capacity(rules.len());
        let mut all_rules_advance = true;
        for (index, (rule, groups)) in rules.iter().enumerate() {
            if *groups > PROGRAM_LIMIT {
                return Err(CompileError::CaptureLimit {
                    count: *groups,
                    limit: PROGRAM_LIMIT,
                });
            }
            all_rules_advance &= rule.width().0 > 0;
            capture_counts.push(*groups);
            let exit = compiler.push(Inst::Accept(index))?;
            let enter = compiler.rule(rule, exit, *groups)?;
            let begin = compiler.push(Inst::Begin(*groups, enter))?;
            if mode == MatchMode::Search {
                let search = compiler.push(Inst::Split(begin, begin))?;
                let later = compiler.push(Inst::Consume(Set::range(0, domain.limit()), search))?;
                compiler.instructions[search] = Inst::Split(begin, later);
                entries.push(search);
            } else {
                entries.push(begin);
            }
        }
        let graphemes = compiler.instructions.iter().any(|inst| {
            matches!(
                inst,
                Inst::Test(Assertion::Grapheme | Assertion::NotGrapheme, _)
            )
        });
        let mut program = Self {
            instructions: compiler.instructions,
            entries,
            capture_counts,
            domain,
            mode,
            all_rules_advance,
            graphemes,
            dfa: None,
        };
        program.dfa = dfa::Dfa::compile(&program);
        Ok(program)
    }

    pub fn domain(&self) -> InputDomain {
        self.domain
    }

    pub fn is_scanner(&self) -> bool {
        self.is_prefix() && self.all_rules_advance && !self.entries.is_empty()
    }

    fn is_prefix(&self) -> bool {
        matches!(self.mode, MatchMode::Prefix | MatchMode::LongestPrefix)
    }

    pub fn matcher(&self, start: usize) -> Matcher {
        Matcher {
            origin: start,
            position: start,
            frontier: if self.dfa.is_some() {
                Vec::new()
            } else {
                self.entries
                    .iter()
                    .map(|&pc| Thread {
                        pc,
                        start,
                        captures: Vec::new(),
                        pending: Vec::new(),
                    })
                    .collect()
            },
            best: None,
            since_accept: Vec::new(),
            dfa_state: None,
            dfa_best: None,
            dfa_since_accept: Vec::new(),
        }
    }

    pub fn run(&self, units: &[u32]) -> Option<Match> {
        self.run_with(units, || Ok::<_, std::convert::Infallible>(()))
            .unwrap()
    }

    pub fn run_str(&self, text: &str) -> Option<Match> {
        self.run_str_with(text, || Ok::<_, std::convert::Infallible>(()))
            .unwrap()
    }

    pub fn run_str_with<E>(
        &self,
        text: &str,
        checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Match>, E> {
        debug_assert_eq!(self.domain, InputDomain::Text);
        if let Some(dfa) = &self.dfa {
            return dfa::run_dfa_str(self, dfa, text, checkpoint);
        }
        let units: Vec<u32> = text.chars().map(u32::from).collect();
        self.run_with(&units, checkpoint)
    }

    pub fn run_bytes(&self, bytes: &[u8]) -> Option<Match> {
        self.run_bytes_with(bytes, || Ok::<_, std::convert::Infallible>(()))
            .unwrap()
    }

    pub fn run_bytes_with<E>(
        &self,
        bytes: &[u8],
        checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Match>, E> {
        debug_assert_eq!(self.domain, InputDomain::Bytes);
        if let Some(dfa) = &self.dfa {
            return dfa::run_dfa_bytes(self, dfa, bytes, checkpoint);
        }
        let units: Vec<u32> = bytes.iter().copied().map(u32::from).collect();
        self.run_with(&units, checkpoint)
    }

    pub fn run_with<E>(
        &self,
        units: &[u32],
        checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Match>, E> {
        let boundaries = if self.graphemes {
            grapheme_boundaries(units, &mut crate::unicode::Graphemes::default())
        } else {
            Vec::new()
        };
        match self.matcher(0).resume_with(
            self,
            Input {
                base: 0,
                units,
                left: None,
                graphemes: &boundaries,
                eof: true,
            },
            None,
            checkpoint,
        )? {
            Outcome::Match(matched) => Ok(Some(matched)),
            Outcome::Miss => Ok(None),
            Outcome::NeedMore => unreachable!("finite input"),
        }
    }
}

pub fn grapheme_boundaries(units: &[u32], state: &mut crate::unicode::Graphemes) -> Vec<bool> {
    units
        .iter()
        .map(|unit| state.push(char::from_u32(*unit).expect("text consists of scalars")))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub rule: usize,
    /// Half-open input range, measured in Unicode scalars for text or octets for bytes.
    pub range: Range<usize>,
    /// Capture ranges in the same coordinates as `range`, indexed by capture ordinal.
    pub groups: Vec<Option<Range<usize>>>,
}

pub enum Outcome {
    Match(Match),
    Miss,
    NeedMore,
}

/// Buffered input for a matcher. Positions count Unicode scalars or octets according
/// to the program's domain. When resuming, the buffer must cover the match origin
/// through the matcher's current position so capture replay can read the accepted range.
pub struct Input<'a> {
    /// Absolute position of the first buffered unit.
    pub base: usize,
    /// Unicode scalar values for text, or values in `0..=255` for bytes.
    pub units: &'a [u32],
    /// Unit immediately before `base`, used by word boundary assertions.
    pub left: Option<u32>,
    /// For text, whether a grapheme boundary occurs before each buffered unit.
    pub graphemes: &'a [bool],
    /// Whether this buffer reaches the end of the complete input.
    pub eof: bool,
}

#[derive(Clone)]
struct Thread {
    pc: usize,
    start: usize,
    captures: Vec<Option<usize>>,
    pending: Vec<usize>,
}

pub struct Matcher {
    origin: usize,
    position: usize,
    frontier: Vec<Thread>,
    best: Option<Match>,
    since_accept: Vec<ScanState>,
    dfa_state: Option<u32>,
    dfa_best: Option<(usize, usize)>,
    dfa_since_accept: Vec<(usize, u32)>,
}

#[derive(PartialEq, Eq, Hash)]
struct ScanState {
    position: usize,
    at_origin: bool,
    pcs: Box<[usize]>,
}

#[derive(Default)]
pub struct FailedSuffixes {
    nfa: HashSet<ScanState>,
    dfa: HashSet<(usize, u32)>,
}

impl FailedSuffixes {
    pub fn discard_before(&mut self, position: usize) {
        self.nfa.retain(|state| state.position >= position);
        self.dfa.retain(|&(pos, _)| pos >= position);
    }
}

fn word(unit: Option<u32>) -> bool {
    unit.is_some_and(|unit| matches!(unit, 48..=57 | 65..=90 | 95 | 97..=122))
}

#[derive(Clone, Copy)]
struct AssertionContext {
    start: bool,
    left_word: bool,
    right: Option<u32>,
    grapheme: Option<bool>,
    eof: bool,
}

impl AssertionContext {
    fn test(self, assertion: Assertion) -> Option<bool> {
        match assertion {
            Assertion::Start => Some(self.start),
            Assertion::End => {
                if self.right.is_some() || self.eof {
                    Some(self.right.is_none())
                } else {
                    None
                }
            }
            Assertion::Word | Assertion::NotWord => (self.right.is_some() || self.eof)
                .then(|| (self.left_word != word(self.right)) == (assertion == Assertion::Word)),
            Assertion::Grapheme | Assertion::NotGrapheme => self
                .grapheme
                .or_else(|| (self.eof && self.right.is_none()).then_some(true))
                .map(|yes| yes == (assertion == Assertion::Grapheme)),
        }
    }
}

impl Program {
    pub(crate) fn capture_groups<E>(
        &self,
        rule: usize,
        origin: usize,
        end: usize,
        input: Input<'_>,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Vec<Option<Range<usize>>>, E> {
        let mut frontier = vec![Thread {
            pc: self.entries[rule],
            start: origin,
            captures: Vec::new(),
            pending: Vec::new(),
        }];
        let mut seen = HashSet::new();
        let mut stack = Vec::new();
        let mut next = Vec::new();
        let mut work = 0usize;
        for position in origin..=end {
            let index = position - input.base;
            let right = input.units.get(index).copied();
            let left = if index == 0 {
                input.left
            } else {
                input.units.get(index - 1).copied()
            };
            let context = AssertionContext {
                start: position == origin,
                left_word: word(left),
                right,
                grapheme: input.graphemes.get(index).copied(),
                eof: input.eof,
            };
            seen.clear();
            stack.clear();
            stack.extend(frontier.drain(..).rev());
            while let Some(mut thread) = stack.pop() {
                loop {
                    if work == 0 {
                        checkpoint()?;
                    }
                    work = (work + 1) % 1024;
                    if !seen.insert((thread.pc, thread.pending.clone())) {
                        break;
                    }
                    match &self.instructions[thread.pc] {
                        Inst::Begin(groups, continuation) => {
                            thread.start = position;
                            thread.captures = vec![None; groups * 2];
                            thread.pc = *continuation;
                        }
                        Inst::Save(slot, continuation) => {
                            thread.captures[*slot] = Some(position);
                            thread.pc = *continuation;
                        }
                        Inst::RepeatEnter(loop_id, continuation) => {
                            thread.pending.push(*loop_id);
                            thread.pc = *continuation;
                        }
                        Inst::RepeatExit(loop_id, continuation) => {
                            if thread.pending.contains(loop_id) {
                                break;
                            }
                            thread.pc = *continuation;
                        }
                        Inst::Split(first, second) => {
                            let mut later = thread.clone();
                            later.pc = *second;
                            stack.push(later);
                            thread.pc = *first;
                        }
                        Inst::Test(assertion, continuation) => {
                            if context.test(*assertion) == Some(true) {
                                thread.pc = *continuation;
                            } else {
                                break;
                            }
                        }
                        Inst::Consume(set, continuation) => {
                            if position < end
                                && let Some(unit) = right
                                && set.contains(unit)
                            {
                                thread.pc = *continuation;
                                thread.pending.clear();
                                next.push(thread);
                            }
                            break;
                        }
                        Inst::Accept(_) => {
                            if position == end {
                                return Ok(thread
                                    .captures
                                    .as_chunks::<2>()
                                    .0
                                    .iter()
                                    .map(|&[start, stop]| {
                                        start.zip(stop).map(|(start, stop)| start..stop)
                                    })
                                    .collect());
                            }
                            break;
                        }
                    }
                }
            }
            std::mem::swap(&mut frontier, &mut next);
        }
        unreachable!("DFA accepted rule {rule} at {origin}..{end}")
    }
}

impl Matcher {
    pub fn resume(&mut self, program: &Program, input: Input<'_>) -> Outcome {
        self.resume_with(program, input, None, || {
            Ok::<_, std::convert::Infallible>(())
        })
        .unwrap()
    }

    pub fn resume_with<E>(
        &mut self,
        program: &Program,
        input: Input<'_>,
        mut failed_suffixes: Option<&mut FailedSuffixes>,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Outcome, E> {
        if let Some(dfa) = &program.dfa {
            return dfa::run_dfa(program, dfa, self, input, failed_suffixes, checkpoint);
        }
        let mut seen = HashSet::new();
        let mut stack = Vec::new();
        let mut next = Vec::new();
        let mut work = 0usize;
        let memoize = failed_suffixes.is_some() && program.is_prefix();
        loop {
            let position = self.position;
            let state = memoize.then(|| ScanState {
                position,
                at_origin: position == self.origin,
                pcs: self.frontier.iter().map(|thread| thread.pc).collect(),
            });
            if let Some(state) = &state
                && let Some(cache) = failed_suffixes.as_deref_mut()
                && cache.nfa.contains(state)
            {
                cache.nfa.extend(self.since_accept.drain(..));
                return Ok(self.best.take().map_or(Outcome::Miss, Outcome::Match));
            }
            let index = position - input.base;
            let right = input.units.get(index).copied();
            let left = if index == 0 {
                input.left
            } else {
                input.units.get(index - 1).copied()
            };
            let context = AssertionContext {
                start: position == self.origin,
                left_word: word(left),
                right,
                grapheme: input.graphemes.get(index).copied(),
                eof: input.eof,
            };
            let mut blocked = false;
            let mut accepted = false;
            seen.clear();
            stack.extend(self.frontier.iter().rev().cloned());
            while let Some(mut thread) = stack.pop() {
                loop {
                    if work == 0 {
                        checkpoint()?;
                    }
                    work = (work + 1) % 1024;
                    if !seen.insert((thread.pc, thread.pending.clone())) {
                        break;
                    }
                    match &program.instructions[thread.pc] {
                        Inst::Begin(groups, continuation) => {
                            thread.start = position;
                            thread.captures = vec![None; groups * 2];
                            thread.pc = *continuation;
                        }
                        Inst::Save(slot, continuation) => {
                            thread.captures[*slot] = Some(position);
                            thread.pc = *continuation;
                        }
                        Inst::RepeatEnter(loop_id, continuation) => {
                            thread.pending.push(*loop_id);
                            thread.pc = *continuation;
                        }
                        Inst::RepeatExit(loop_id, continuation) => {
                            if thread.pending.contains(loop_id) {
                                break;
                            }
                            thread.pc = *continuation;
                        }
                        Inst::Split(first, second) => {
                            let mut later = thread.clone();
                            later.pc = *second;
                            stack.push(later);
                            thread.pc = *first;
                        }
                        Inst::Test(assertion, continuation) => match context.test(*assertion) {
                            Some(true) => thread.pc = *continuation,
                            Some(false) => break,
                            None => {
                                blocked = true;
                                break;
                            }
                        },
                        Inst::Consume(set, continuation) => {
                            if let Some(unit) = right {
                                if set.contains(unit) {
                                    thread.pc = *continuation;
                                    thread.pending.clear();
                                    next.push(thread);
                                }
                            } else if !input.eof && !set.0.is_empty() {
                                blocked = true;
                            }
                            break;
                        }
                        Inst::Accept(rule) => {
                            if !accepted
                                && (program.mode != MatchMode::Full
                                    || (right.is_none() && input.eof))
                            {
                                accepted = true;
                                self.best = Some(Match {
                                    rule: *rule,
                                    range: thread.start..position,
                                    groups: thread
                                        .captures
                                        .as_chunks::<2>()
                                        .0
                                        .iter()
                                        .map(|&[start, end]| {
                                            start.zip(end).map(|(start, end)| start..end)
                                        })
                                        .collect(),
                                });
                                if program.mode != MatchMode::LongestPrefix {
                                    stack.clear();
                                }
                            }
                            break;
                        }
                    }
                }
            }
            if accepted {
                self.since_accept.clear();
            } else if let Some(state) = state
                && (right.is_some() || !blocked)
            {
                self.since_accept.push(state);
            }
            if right.is_none() && blocked {
                return Ok(Outcome::NeedMore);
            }
            if next.is_empty() {
                if let Some(cache) = failed_suffixes {
                    cache.nfa.extend(self.since_accept.drain(..));
                }
                return Ok(self.best.take().map_or(Outcome::Miss, Outcome::Match));
            }
            self.position += 1;
            self.frontier.clear();
            std::mem::swap(&mut next, &mut self.frontier);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_nfa_only(domain: InputDomain, mode: MatchMode, rules: &[(Rule, usize)]) -> Program {
        let mut program = Program::compile(domain, mode, rules).unwrap();
        program.dfa = None;
        program.matcher(0);
        program
    }

    fn assert_outcome_eq(a: &Outcome, b: &Outcome, label: &str) {
        match (a, b) {
            (Outcome::Match(ma), Outcome::Match(mb)) => assert_eq!(ma, mb, "{label}"),
            (Outcome::Miss, Outcome::Miss) => {}
            (Outcome::NeedMore, Outcome::NeedMore) => {}
            _ => panic!("{label}: outcome mismatch"),
        }
    }

    #[test]
    fn dfa_and_nfa_agree_across_modes_assertions_captures_and_chunks() {
        type MatchSuite = (
            MatchMode,
            &'static [(&'static str, bool)],
            &'static [&'static str],
        );
        let suites: &[MatchSuite] = &[
            (
                MatchMode::LongestPrefix,
                &[
                    ("if", false),
                    (r"\p{XID_Start}\p{XID_Continue}*", true),
                    ("==", false),
                    ("=", false),
                    (r"a\b", true),
                    ("a+", true),
                ],
                &["if", "if中!", "if_2", "==", "=", "a!", "ab", "aaa!", ""],
            ),
            (
                MatchMode::Prefix,
                &[
                    ("a$", true),
                    ("ab", true),
                    ("a", true),
                    (r"(?:^|\b)x+?", true),
                ],
                &["a", "ab", "ax", "xx!", "b"],
            ),
            (
                MatchMode::Full,
                &[("^$", false), (r"(?i:Σ)+", true), (r"[a-z]+\b[0-9]*", true)],
                &["", "Σσς", "abc12", "abc!", "s"],
            ),
        ];

        for &(mode, patterns, inputs) in suites {
            let rules: Vec<_> = patterns
                .iter()
                .map(|&(pat, cap)| {
                    let parsed = parse_regex(InputDomain::Text, pat).unwrap();
                    if cap {
                        (
                            Rule::Capture {
                                group: 0,
                                body: Box::new(parsed),
                            },
                            1,
                        )
                    } else {
                        (parsed, 0)
                    }
                })
                .collect();
            let dfa_prog = Program::compile(InputDomain::Text, mode, &rules).unwrap();
            assert!(dfa_prog.dfa.is_some(), "expected DFA for {mode:?}");
            let nfa_prog = compile_nfa_only(InputDomain::Text, mode, &rules);

            for &text in inputs {
                let units: Vec<u32> = text.chars().map(u32::from).collect();
                assert_eq!(
                    dfa_prog.run(&units),
                    nfa_prog.run(&units),
                    "mode={mode:?} text={text:?}"
                );
                assert_eq!(
                    dfa_prog.run_str(text),
                    nfa_prog.run(&units),
                    "run_str mode={mode:?} text={text:?}"
                );

                // Check incremental chunk splits at every boundary.
                for split in 0..=units.len() {
                    let mut dfa_m = dfa_prog.matcher(0);
                    let mut nfa_m = nfa_prog.matcher(0);
                    let d1 = dfa_m.resume(
                        &dfa_prog,
                        Input {
                            base: 0,
                            units: &units[..split],
                            left: None,
                            graphemes: &[],
                            eof: false,
                        },
                    );
                    let n1 = nfa_m.resume(
                        &nfa_prog,
                        Input {
                            base: 0,
                            units: &units[..split],
                            left: None,
                            graphemes: &[],
                            eof: false,
                        },
                    );
                    assert_outcome_eq(
                        &d1,
                        &n1,
                        &format!("split1 mode={mode:?} text={text:?} @{split}"),
                    );
                    if matches!(d1, Outcome::NeedMore) {
                        let d2 = dfa_m.resume(
                            &dfa_prog,
                            Input {
                                base: 0,
                                units: &units,
                                left: None,
                                graphemes: &[],
                                eof: true,
                            },
                        );
                        let n2 = nfa_m.resume(
                            &nfa_prog,
                            Input {
                                base: 0,
                                units: &units,
                                left: None,
                                graphemes: &[],
                                eof: true,
                            },
                        );
                        assert_outcome_eq(
                            &d2,
                            &n2,
                            &format!("split2 mode={mode:?} text={text:?} @{split}"),
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn dfa_handles_nonzero_origin_and_left_word_context() {
        let rules = vec![
            (parse_regex(InputDomain::Text, r"\bb").unwrap(), 0),
            (parse_regex(InputDomain::Text, "^b").unwrap(), 0),
        ];
        let dfa_prog =
            Program::compile(InputDomain::Text, MatchMode::LongestPrefix, &rules).unwrap();
        let nfa_prog = compile_nfa_only(InputDomain::Text, MatchMode::LongestPrefix, &rules);
        assert!(dfa_prog.dfa.is_some());
        let input = vec![u32::from('b')];
        for left in [None, Some(u32::from('a')), Some(u32::from(' '))] {
            let d = dfa_prog.matcher(1).resume(
                &dfa_prog,
                Input {
                    base: 1,
                    units: &input,
                    left,
                    graphemes: &[],
                    eof: true,
                },
            );
            let n = nfa_prog.matcher(1).resume(
                &nfa_prog,
                Input {
                    base: 1,
                    units: &input,
                    left,
                    graphemes: &[],
                    eof: true,
                },
            );
            assert_outcome_eq(&d, &n, &format!("left={left:?}"));
        }
    }

    #[test]
    fn run_bytes_matches_unit_execution_for_longest_prefix_and_captures() {
        let rules = vec![
            (parse_regex(InputDomain::Bytes, r"a\b").unwrap(), 0),
            (
                Rule::Capture {
                    group: 0,
                    body: Box::new(parse_regex(InputDomain::Bytes, r"\x61[\x80-\xff]+").unwrap()),
                },
                1,
            ),
            (parse_regex(InputDomain::Bytes, "a").unwrap(), 0),
        ];
        let program =
            Program::compile(InputDomain::Bytes, MatchMode::LongestPrefix, &rules).unwrap();
        assert!(program.dfa.is_some());
        for sample in [&b"a!"[..], &b"a\x80\xff!"[..], &b"ab"[..], &b"z"[..]] {
            let units: Vec<u32> = sample.iter().copied().map(u32::from).collect();
            assert_eq!(program.run_bytes(sample), program.run(&units));
        }
    }
}
