use std::cell::OnceCell;
use std::ops::Range;

use thiserror::Error;
use zkcl_bytecode::native_abi::{FixnumPrimitive, LEAF_ARGUMENTS, LeafGuard, LeafPrimitive};
use zkcl_bytecode::{
    Arity, BlockLeaders, BytecodeBody, BytecodeFunction, Callee, FIXNUM_SHIFT, FixnumArithmetic,
    Immediate, Instruction, LiteralNode, LiveIns, TAG_FIXNUM, TAG_MASK, WORD_NIL, WORD_T,
    fixnum_bounds_for_bits,
};

use crate::LeafFunction;
use crate::flow::{self, Edge};
use crate::leaf::{Block, Exit, Operand, Operation, Step, Test};
use crate::locals::{self, MAX_LOCALS};

mod bits;

const NIL: Operand = Operand::Immediate(WORD_NIL);
const MAX_REGISTERS: usize = 256;
const MAX_BLOCKS: usize = 128;
const MAX_STEPS: usize = 1024;
const MAX_LOOP_WORK: usize = 4096;
const MAX_BACKEDGES: usize = 128;

/// Why a bytecode function could not be selected as a scalar leaf.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum SelectionError {
    #[error("unsupported tagged word width {0}")]
    WordWidth(u8),
    #[error("leaf functions require fixed arity of at most {LEAF_ARGUMENTS}")]
    Arity,
    #[error("leaf registers must cover the arguments and fit within {MAX_REGISTERS} slots")]
    Registers,
    #[error("control flow exceeds the leaf selection budget")]
    Budget,
    #[error("branch at bytecode PC {pc} goes out of bounds")]
    Branch { pc: usize },
    #[error("backedge at bytecode PC {pc} has no selected entry at PC {target}")]
    LoopEntry { pc: usize, target: usize },
    #[error("a loop at bytecode PC {pc} carries a live multiple-value result")]
    LoopValues { pc: usize },
    #[error("unsupported leaf instruction at bytecode PC {pc}")]
    Instruction { pc: usize },
    #[error("call to constant {symbol} at bytecode PC {pc} is declared NOTINLINE")]
    NotinlineCall { pc: usize, symbol: usize },
    #[error("call to constant {symbol} at bytecode PC {pc} is not a supported leaf primitive")]
    UnrecognizedNamedCall { pc: usize, symbol: usize },
    #[error("LOGBITP at bytecode PC {pc} needs a nonnegative constant fixnum index")]
    BitIndex { pc: usize },
    #[error("unsupported register, immediate, or numeric operand at bytecode PC {pc}")]
    Operand { pc: usize },
    #[error("every leaf path must return a single value")]
    ResultCount,
    #[error("could not fit leaf temporaries into {MAX_LOCALS} slots")]
    Locals,
}

impl SelectionError {
    fn unsupported_operation(self) -> bool {
        matches!(
            self,
            Self::Instruction { .. }
                | Self::NotinlineCall { .. }
                | Self::UnrecognizedNamedCall { .. }
                | Self::BitIndex { .. }
        )
    }
}

/// Select a compiled-file entry, resolving named calls by their literal symbol identity.
pub fn select_compiled(
    code: &BytecodeFunction,
    literals: &[LiteralNode],
    word_bits: u8,
) -> Result<LeafFunction, SelectionError> {
    select_bytecode(code.body(), word_bits, |symbol| {
        let index = *code.constants.get(symbol)? as usize;
        match literals.get(index)? {
            LiteralNode::InternedSymbol { package, name } if package == "COMMON-LISP" => {
                LeafPrimitive::named(name)
            }
            _ => None,
        }
    })
}

/// Select a function body, honoring both bytecode representations of NOTINLINE.
pub fn select_bytecode(
    body: BytecodeBody<'_>,
    word_bits: u8,
    resolve_primitive: impl Fn(usize) -> Option<LeafPrimitive>,
) -> Result<LeafFunction, SelectionError> {
    select_instructions(
        body.arity,
        body.registers,
        body.instructions,
        body.notinline,
        word_bits,
        resolve_primitive,
    )
}

/// Select a raw instruction sequence using the declarations encoded in each call.
/// For a bytecode body with function-wide metadata, use [`select_bytecode`].
pub fn select(
    arity: Arity,
    registers: usize,
    instructions: &[Instruction],
    word_bits: u8,
    resolve_primitive: impl Fn(usize) -> Option<LeafPrimitive>,
) -> Result<LeafFunction, SelectionError> {
    select_instructions(
        arity,
        registers,
        instructions,
        &[],
        word_bits,
        resolve_primitive,
    )
}

fn select_instructions(
    arity: Arity,
    registers: usize,
    instructions: &[Instruction],
    notinline: &[u32],
    word_bits: u8,
    resolve_primitive: impl Fn(usize) -> Option<LeafPrimitive>,
) -> Result<LeafFunction, SelectionError> {
    let (low, high) =
        fixnum_bounds_for_bits(word_bits).ok_or(SelectionError::WordWidth(word_bits))?;
    let argc = arity.required;
    if arity.maximum != Some(argc) || argc > LEAF_ARGUMENTS {
        return Err(SelectionError::Arity);
    }
    if !(argc..=MAX_REGISTERS).contains(&registers) {
        return Err(SelectionError::Registers);
    }
    if instructions.len() > MAX_STEPS {
        return Err(SelectionError::Budget);
    }

    let mut regs = vec![NIL; registers];
    for (index, register) in regs[..argc].iter_mut().enumerate() {
        *register = Operand::Arg(index as u8);
    }
    let mut selector = Selector {
        word_bits,
        fixnums: Bounds { low, high },
        instructions,
        notinline,
        resolve_primitive,
        guards: Vec::new(),
        facts: Vec::new(),
        edges: Vec::new(),
        incoming: (0..=instructions.len()).map(|_| Vec::new()).collect(),
        loop_inputs: vec![None; instructions.len() + 1],
        backedges: Vec::new(),
        remaining_expansion: MAX_STEPS - instructions.len(),
    };
    let entry = SelectionPath {
        regs,
        values: None,
        ranges: Vec::new(),
        type_failure: false,
    };
    let mut blocks = selector.select_blocks(entry)?;
    let locals = locals::assign(&mut blocks, selector.facts.len())?;
    let blocks = flow::combine_fallthroughs(blocks);
    let loop_budget = blocks
        .iter()
        .flat_map(|block| &block.steps)
        .any(|step| *step == Step::CheckLoopBudget)
        .then(|| {
            let cost = blocks
                .iter()
                .map(|block| 1 + block.steps.len())
                .sum::<usize>();
            (MAX_LOOP_WORK / cost).clamp(1, MAX_BACKEDGES) as u32
        });
    Ok(LeafFunction {
        arity: argc,
        word_bits,
        guards: selector.guards,
        blocks,
        locals,
        loop_budget,
    })
}

#[derive(Clone)]
struct SelectionPath {
    regs: Vec<Operand>,
    // A snapshot survives register reuse. Before the first write, the VM's
    // values register is outside the leaf's contract.
    values: Option<ValueFacts>,
    ranges: Vec<(Operand, Bounds)>,
    type_failure: bool,
}

#[derive(Clone)]
struct ValueFacts {
    items: Vec<Operand>,
    /// A join can supply the values read by an instruction even when its
    /// predecessors produced different result counts.
    exact_count: Option<usize>,
}

impl ValueFacts {
    fn new(items: &[Operand]) -> Self {
        Self {
            items: items.to_vec(),
            exact_count: Some(items.len()),
        }
    }

    fn get(&self, index: usize) -> Operand {
        self.items.get(index).copied().unwrap_or(NIL)
    }
}

struct Incoming {
    edge: usize,
    path: SelectionPath,
}

struct Backedge {
    edge: usize,
    assignments: Vec<(usize, Operand)>,
}

#[derive(Clone, Copy)]
enum Fact {
    Unknown,
    Immediate(u64),
    Fixnum(Bounds),
    Boolean,
}

#[derive(Clone, Copy)]
struct Bounds {
    low: i64,
    high: i64,
}

impl Bounds {
    fn intersect(self, other: Self) -> Option<Self> {
        let low = self.low.max(other.low);
        let high = self.high.min(other.high);
        (low <= high).then_some(Self { low, high })
    }

    fn within(self, other: Self) -> bool {
        self.low >= other.low && self.high <= other.high
    }
}

impl Fact {
    fn merge(self, other: Self, bits: u8) -> Self {
        if let (Self::Immediate(lhs), Self::Immediate(rhs)) = (self, other)
            && lhs == rhs
        {
            return self;
        }
        if let (Some(lhs), Some(rhs)) = (self.fixnum(bits), other.fixnum(bits)) {
            return Self::Fixnum(Bounds {
                low: lhs.low.min(rhs.low),
                high: lhs.high.max(rhs.high),
            });
        }
        let boolean = |fact| matches!(fact, Self::Boolean | Self::Immediate(WORD_NIL | WORD_T));
        if boolean(self) && boolean(other) {
            Self::Boolean
        } else {
            Self::Unknown
        }
    }

    fn fixnum(self, bits: u8) -> Option<Bounds> {
        match self {
            Self::Immediate(word) if word & TAG_MASK == TAG_FIXNUM => {
                let word = if bits == 32 {
                    i64::from(word as i32)
                } else {
                    word as i64
                };
                let n = word >> FIXNUM_SHIFT;
                Some(Bounds { low: n, high: n })
            }
            Self::Fixnum(bounds) => Some(bounds),
            _ => None,
        }
    }

    fn test(self, test: Test, bits: u8) -> Option<bool> {
        match test {
            Test::Nil => match self {
                Self::Immediate(word) => Some(word == WORD_NIL),
                Self::Fixnum(_) => Some(false),
                _ => None,
            },
            Test::Fixnum => (!matches!(self, Self::Unknown)).then(|| self.fixnum(bits).is_some()),
            Test::FixnumInRange(low, high) => {
                if matches!(self, Self::Unknown) {
                    return None;
                }
                let Some(actual) = self.fixnum(bits) else {
                    return Some(false);
                };
                let test = Bounds {
                    low: low.into(),
                    high: high.into(),
                };
                if actual.within(test) {
                    Some(true)
                } else if actual.intersect(test).is_none() {
                    Some(false)
                } else {
                    None
                }
            }
        }
    }
}

struct Selector<'a, Resolve> {
    word_bits: u8,
    fixnums: Bounds,
    instructions: &'a [Instruction],
    notinline: &'a [u32],
    resolve_primitive: Resolve,
    guards: Vec<LeafGuard>,
    facts: Vec<Fact>,
    edges: Vec<Edge>,
    incoming: Vec<Vec<Incoming>>,
    // The bytecode registers that become block inputs at each loop header.
    loop_inputs: Vec<Option<Vec<(usize, usize)>>>,
    backedges: Vec<Backedge>,
    remaining_expansion: usize,
}

impl<Resolve: Fn(usize) -> Option<LeafPrimitive>> Selector<'_, Resolve> {
    fn fact(&self, ranges: &[(Operand, Bounds)], operand: Operand) -> Fact {
        if let Some((_, bounds)) = ranges.iter().rev().find(|&&(value, _)| value == operand) {
            return Fact::Fixnum(*bounds);
        }
        match operand {
            Operand::Arg(_) => Fact::Unknown,
            Operand::Immediate(word) => Fact::Immediate(word),
            Operand::Local(id) => self.facts[id],
        }
    }

    fn select_blocks(&mut self, entry: SelectionPath) -> Result<Vec<Block>, SelectionError> {
        let registers = entry.regs.len();
        let mut leaders = BlockLeaders::new(self.instructions.len());
        let liveness = OnceCell::new();
        let mut loop_headers = vec![false; self.instructions.len() + 1];
        for (pc, instruction) in self.instructions.iter().enumerate() {
            let flow = flow::leaf_flow(instruction);
            leaders.observe(pc, flow);
            if let Some(target) = flow.target().filter(|&target| target <= pc) {
                loop_headers[target] = true;
            }
        }
        let blocks: Vec<_> = leaders.ranges().collect();
        // Edge zero models function entry, including initialization for a loop at PC zero.
        self.forward_edge(0, entry);
        let mut bodies = Vec::new();
        let mut type_failure = None;
        for (index, range) in blocks.iter().enumerate() {
            let pc = range.start;
            let mut paths = std::mem::take(&mut self.incoming[pc]);
            if paths.is_empty() {
                continue;
            }
            let path = if !loop_headers[pc] && paths.len() == 1 {
                paths.pop().unwrap().path
            } else {
                let live =
                    liveness.get_or_init(|| flow::liveness(self.instructions, registers, &blocks));
                if loop_headers[pc] {
                    self.loop_entry(pc, &paths, live, index)?
                } else {
                    self.merge(&paths, live, index)
                }
            };
            if bodies.len() == MAX_BLOCKS {
                return Err(SelectionError::Budget);
            }
            let allow_miss = path.type_failure;
            let guard_count = self.guards.len();
            let mut steps = Vec::new();
            let exit = match self.select_block(range.clone(), path, &mut steps) {
                Err(error) if allow_miss && error.unsupported_operation() => {
                    // Type failures can reach code that constructs or signals
                    // a condition. The selected prefix has performed no Lisp effects.
                    type_failure.get_or_insert(error);
                    self.guards.truncate(guard_count);
                    steps.clear();
                    Exit::Miss
                }
                result => result?,
            };
            bodies.push((pc, Block { steps, exit }));
        }
        if !bodies
            .iter()
            .any(|(_, block)| matches!(block.exit, Exit::Return(_)))
        {
            return Err(type_failure.unwrap_or(SelectionError::ResultCount));
        }
        // Schedule parallel copies after the selector has finished allocating values.
        let mut values = self.facts.len();
        for Backedge { edge, assignments } in std::mem::take(&mut self.backedges) {
            self.edges[edge].copies = flow::backedge_copies(assignments, &mut values);
        }
        self.facts.resize(values, Fact::Unknown);
        Ok(flow::layout(
            bodies,
            std::mem::take(&mut self.edges),
            self.instructions.len(),
        ))
    }

    fn forward_edge(&mut self, target_pc: usize, path: SelectionPath) -> usize {
        let edge = self.edges.len();
        self.edges.push(Edge {
            target_pc,
            copies: Vec::new(),
        });
        self.incoming[target_pc].push(Incoming { edge, path });
        edge
    }

    fn edge(
        &mut self,
        pc: usize,
        target: usize,
        path: SelectionPath,
    ) -> Result<usize, SelectionError> {
        if target > pc {
            return Ok(self.forward_edge(target, path));
        }
        let inputs = self.loop_inputs[target]
            .as_ref()
            .ok_or(SelectionError::LoopEntry { pc, target })?;
        let assignments = inputs
            .iter()
            .map(|&(register, value)| (value, path.regs[register]))
            .collect();
        let edge = self.edges.len();
        self.edges.push(Edge {
            target_pc: target,
            copies: Vec::new(),
        });
        self.backedges.push(Backedge { edge, assignments });
        Ok(edge)
    }

    fn loop_entry(
        &mut self,
        pc: usize,
        paths: &[Incoming],
        live: &LiveIns,
        block: usize,
    ) -> Result<SelectionPath, SelectionError> {
        if live.values(block) != 0 {
            return Err(SelectionError::LoopValues { pc });
        }
        let mut regs = vec![NIL; paths[0].path.regs.len()];
        let mut inputs = Vec::new();
        for register in live.registers(block) {
            // ValueFacts supplied by a backedge need not have the facts of their
            // entry values. Arity and live registers still agree on every edge.
            let destination = self.facts.len();
            self.facts.push(Fact::Unknown);
            regs[register] = Operand::Local(destination);
            inputs.push((register, destination));
            for incoming in paths {
                self.edges[incoming.edge].copies.push(Step::Copy {
                    destination,
                    value: incoming.path.regs[register],
                });
            }
        }
        self.loop_inputs[pc] = Some(inputs);
        Ok(SelectionPath {
            regs,
            values: None,
            ranges: Vec::new(),
            type_failure: false,
        })
    }

    fn merge(&mut self, paths: &[Incoming], live: &LiveIns, block: usize) -> SelectionPath {
        let mut path = SelectionPath {
            regs: vec![NIL; paths[0].path.regs.len()],
            values: None,
            ranges: Vec::new(),
            type_failure: paths.iter().all(|input| input.path.type_failure),
        };
        for index in live.registers(block) {
            path.regs[index] = self.merge_value(paths, |path| path.regs[index], &mut path.ranges);
        }
        let demand = live.values(block);
        if demand != 0 && paths.iter().all(|input| input.path.values.is_some()) {
            let mut values = paths
                .iter()
                .map(|input| input.path.values.as_ref().unwrap());
            let max = values
                .clone()
                .map(|values| values.items.len())
                .max()
                .unwrap();
            let exact_count = values
                .next()
                .unwrap()
                .exact_count
                .filter(|count| values.all(|values| values.exact_count == Some(*count)));
            let items = (0..max.min(demand))
                .map(|index| {
                    self.merge_value(
                        paths,
                        |path| path.values.as_ref().unwrap().get(index),
                        &mut path.ranges,
                    )
                })
                .collect();
            path.values = Some(ValueFacts { items, exact_count });
        }
        path
    }

    fn merge_value(
        &mut self,
        paths: &[Incoming],
        operand: impl Fn(&SelectionPath) -> Operand,
        ranges: &mut Vec<(Operand, Bounds)>,
    ) -> Operand {
        let first = operand(&paths[0].path);
        let facts = paths
            .iter()
            .map(|input| self.fact(&input.path.ranges, operand(&input.path)));
        let fact = facts.reduce(|a, b| a.merge(b, self.word_bits)).unwrap();
        if paths.iter().all(|input| operand(&input.path) == first) {
            if let Fact::Fixnum(bounds) = fact {
                ranges.push((first, bounds));
            }
            return first;
        }
        if let Fact::Immediate(word) = fact {
            return Operand::Immediate(word);
        }
        let destination = self.facts.len();
        self.facts.push(fact);
        for input in paths {
            self.edges[input.edge].copies.push(Step::Copy {
                destination,
                value: operand(&input.path),
            });
        }
        Operand::Local(destination)
    }

    fn select_block(
        &mut self,
        region: Range<usize>,
        mut path: SelectionPath,
        steps: &mut Vec<Step>,
    ) -> Result<Exit, SelectionError> {
        let end = region.end;
        for pc in region {
            let invalid = SelectionError::Operand { pc };
            let branch_target = |target| {
                (target <= self.instructions.len())
                    .then_some(target)
                    .ok_or(SelectionError::Branch { pc })
            };
            match &self.instructions[pc] {
                Instruction::ReleaseArguments => {}
                Instruction::Load(out, literal) => {
                    let word = literal.encode_word(self.word_bits).ok_or(invalid)?;
                    *path.regs.get_mut(*out).ok_or(invalid)? = Operand::Immediate(word);
                }
                Instruction::Copy(out, input) => {
                    *path.regs.get_mut(*out).ok_or(invalid)? =
                        *path.regs.get(*input).ok_or(invalid)?;
                }
                Instruction::Clear(range) => {
                    path.regs.get_mut(range.clone()).ok_or(invalid)?.fill(NIL)
                }
                Instruction::SetValues(range) => {
                    let src = path.regs.get(range.clone()).ok_or(invalid)?;
                    path.values = Some(ValueFacts::new(src));
                }
                Instruction::ReadValue(out, position) => {
                    let values = path
                        .values
                        .as_ref()
                        .ok_or(SelectionError::Instruction { pc })?;
                    *path.regs.get_mut(*out).ok_or(invalid)? = values.get(*position);
                }
                Instruction::BindValues(range) => {
                    let values = path.values.take();
                    if values.is_none() && !range.is_empty() {
                        return Err(SelectionError::Instruction { pc });
                    }
                    for (index, slot) in path
                        .regs
                        .get_mut(range.clone())
                        .ok_or(invalid)?
                        .iter_mut()
                        .enumerate()
                    {
                        *slot = values.as_ref().unwrap().get(index);
                    }
                    path.values = Some(ValueFacts::new(&[]));
                }
                Instruction::TryFixnumArithmetic {
                    op,
                    symbol,
                    out,
                    args,
                } => {
                    let operands = path.regs.get(args.clone()).ok_or(invalid)?;
                    let guard = LeafGuard {
                        symbol: *symbol,
                        primitive: LeafPrimitive::Arithmetic(*op),
                    };
                    let value = self
                        .numeric(
                            FixnumPrimitive::Arithmetic(*op),
                            operands,
                            &mut path.ranges,
                            steps,
                        )
                        .ok_or(invalid)?;
                    self.record_guard(guard);
                    *path.regs.get_mut(*out).ok_or(invalid)? = value;
                }
                Instruction::Call {
                    callee: Callee::Named(symbol),
                    out,
                    args,
                    associate: false,
                    notinline,
                    ..
                } => {
                    let symbol = *symbol;
                    if *notinline
                        || u32::try_from(symbol).is_ok_and(|index| self.notinline.contains(&index))
                    {
                        return Err(SelectionError::NotinlineCall { pc, symbol });
                    }
                    let primitive = (self.resolve_primitive)(symbol)
                        .ok_or(SelectionError::UnrecognizedNamedCall { pc, symbol })?;
                    let guard = LeafGuard { symbol, primitive };
                    let operands = path.regs.get(args.clone()).ok_or(invalid)?;
                    let value = self.named_call(guard, operands, &mut path.ranges, steps, pc)?;
                    if let Some(out) = *out {
                        *path.regs.get_mut(out).ok_or(invalid)? = value;
                        path.values = Some(ValueFacts::new(&[]));
                    } else {
                        path.values = Some(ValueFacts::new(&[value]));
                    }
                }
                Instruction::Jump(target) => {
                    return Ok(Exit::Jump(self.edge(pc, branch_target(*target)?, path)?));
                }
                Instruction::Unless(..)
                | Instruction::UnlessFixnum(..)
                | Instruction::UnlessFixnumInRange(..) => {
                    let (register, test, yes, no) = match self.instructions[pc] {
                        Instruction::Unless(reg, target) => {
                            (reg, Test::Nil, branch_target(target)?, pc + 1)
                        }
                        Instruction::UnlessFixnum(reg, target) => {
                            (reg, Test::Fixnum, pc + 1, branch_target(target)?)
                        }
                        Instruction::UnlessFixnumInRange(reg, low, high, target) => {
                            let no = branch_target(target)?;
                            let bounds = Bounds {
                                low: low.into(),
                                high: high.into(),
                            };
                            let Some(bounds) = bounds.intersect(self.fixnums) else {
                                return Ok(Exit::Jump(self.edge(pc, no, path)?));
                            };
                            let test = if self.fixnums.within(bounds) {
                                Test::Fixnum
                            } else {
                                Test::FixnumInRange(bounds.low as i32, bounds.high as i32)
                            };
                            (reg, test, pc + 1, no)
                        }
                        _ => unreachable!(),
                    };
                    let value = *path.regs.get(register).ok_or(invalid)?;
                    let fact = self.fact(&path.ranges, value);
                    if yes == no {
                        return Ok(Exit::Jump(self.edge(pc, yes, path)?));
                    } else if let Some(result) = fact.test(test, self.word_bits) {
                        return Ok(Exit::Jump(self.edge(
                            pc,
                            if result { yes } else { no },
                            path,
                        )?));
                    } else {
                        let mut success = path.clone();
                        if let Some(bounds) = self.test_bounds(test) {
                            let bounds = fact.fixnum(self.word_bits).map_or(bounds, |actual| {
                                actual.intersect(bounds).expect("the branch can succeed")
                            });
                            success.ranges.push((value, bounds));
                            path.type_failure = true;
                        }
                        return Ok(Exit::Branch {
                            value,
                            test,
                            yes: self.edge(pc, yes, success)?,
                            no: self.edge(pc, no, path)?,
                        });
                    }
                }
                _ => return Err(SelectionError::Instruction { pc }),
            }
        }
        if end != self.instructions.len() {
            return Ok(Exit::Jump(self.forward_edge(end, path)));
        }
        match path.values {
            Some(values) if values.exact_count == Some(1) => Ok(Exit::Return(values.get(0))),
            _ => Err(SelectionError::ResultCount),
        }
    }

    fn test_bounds(&self, test: Test) -> Option<Bounds> {
        match test {
            Test::Nil => None,
            Test::Fixnum => Some(self.fixnums),
            Test::FixnumInRange(low, high) => Some(Bounds {
                low: low.into(),
                high: high.into(),
            }),
        }
    }

    fn named_call(
        &mut self,
        guard: LeafGuard,
        operands: &[Operand],
        ranges: &mut Vec<(Operand, Bounds)>,
        steps: &mut Vec<Step>,
        pc: usize,
    ) -> Result<Operand, SelectionError> {
        let result = if let LeafPrimitive::IntegerBits(op) = guard.primitive {
            self.integer_bits(op, operands, ranges, steps, pc)?
        } else {
            self.numeric_call(
                guard.primitive.numeric().expect("numeric leaf primitive"),
                operands,
                ranges,
                steps,
                pc,
            )?
        };
        self.record_guard(guard);
        Ok(result)
    }

    fn numeric_call(
        &mut self,
        primitive: FixnumPrimitive,
        operands: &[Operand],
        ranges: &mut Vec<(Operand, Bounds)>,
        steps: &mut Vec<Step>,
        pc: usize,
    ) -> Result<Operand, SelectionError> {
        use FixnumArithmetic::{Add, Mul, Sub};
        use FixnumPrimitive::Arithmetic;

        let invalid = SelectionError::Operand { pc };
        match (primitive, operands) {
            (Arithmetic(op @ (Add | Mul)), []) => Ok(self.fixnum(i64::from(op == Mul))),
            (Arithmetic(Add | Mul), &[value]) => {
                self.fixnum_operand(value, ranges, steps).ok_or(invalid)?;
                Ok(value)
            }
            (Arithmetic(Sub), &[value]) => self
                .numeric(primitive, &[self.fixnum(0), value], ranges, steps)
                .ok_or(invalid),
            (Arithmetic(Add | Sub | Mul), &[first, second, ref rest @ ..]) => {
                self.charge_expansion(rest.len())?;
                let mut result = self
                    .numeric(primitive, &[first, second], ranges, steps)
                    .ok_or(invalid)?;
                for &operand in rest {
                    result = self
                        .numeric(primitive, &[result, operand], ranges, steps)
                        .ok_or(invalid)?;
                }
                Ok(result)
            }
            _ => self
                .numeric(primitive, operands, ranges, steps)
                .ok_or(invalid),
        }
    }

    fn charge_expansion(&mut self, count: usize) -> Result<(), SelectionError> {
        self.remaining_expansion = self
            .remaining_expansion
            .checked_sub(count)
            .ok_or(SelectionError::Budget)?;
        Ok(())
    }

    fn fixnum(&self, value: i64) -> Operand {
        Operand::Immediate(
            Immediate::Fixnum(value)
                .encode_word(self.word_bits)
                .expect("proven fixnum"),
        )
    }

    fn fixnum_operand(
        &self,
        operand: Operand,
        ranges: &mut Vec<(Operand, Bounds)>,
        steps: &mut Vec<Step>,
    ) -> Option<Bounds> {
        match self.fact(ranges, operand) {
            Fact::Unknown => {
                steps.push(Step::CheckFixnum(operand));
                ranges.push((operand, self.fixnums));
                Some(self.fixnums)
            }
            fact => fact.fixnum(self.word_bits),
        }
    }

    fn record_guard(&mut self, guard: LeafGuard) {
        if !self.guards.contains(&guard) {
            self.guards.push(guard);
        }
    }

    fn numeric(
        &mut self,
        primitive: FixnumPrimitive,
        operands: &[Operand],
        ranges: &mut Vec<(Operand, Bounds)>,
        steps: &mut Vec<Step>,
    ) -> Option<Operand> {
        if operands.len() != primitive.arity() {
            return None;
        }
        let mut bounds = [Bounds { low: 1, high: 1 }; 2];
        for (&operand, bounds) in operands.iter().zip(&mut bounds) {
            *bounds = self.fixnum_operand(operand, ranges, steps)?;
        }
        let lhs = operands[0];
        let rhs = operands.get(1).copied();
        let [x, y] = bounds;
        let (op, fact) = match primitive {
            FixnumPrimitive::Arithmetic(op) => {
                let (low, high) = arithmetic_bounds(op, x, y);
                let checked =
                    low < i128::from(self.fixnums.low) || high > i128::from(self.fixnums.high);
                let low = i64::try_from(low.max(i128::from(self.fixnums.low))).ok()?;
                let high = i64::try_from(high.min(i128::from(self.fixnums.high))).ok()?;
                if low > high {
                    return None;
                }
                if low == high && !checked {
                    return Some(self.fixnum(low));
                }
                // This bound only describes the result along the success edge.
                (
                    Operation::Arithmetic {
                        op,
                        lhs,
                        rhs,
                        checked,
                    },
                    Fact::Fixnum(Bounds { low, high }),
                )
            }
            FixnumPrimitive::Compare(relation) => {
                if x.low == x.high && y.low == y.high {
                    return Some(Operand::Immediate(if relation.test(x.low, y.low) {
                        WORD_T
                    } else {
                        WORD_NIL
                    }));
                }
                (
                    Operation::Compare {
                        relation,
                        lhs,
                        rhs: operands[1],
                    },
                    Fact::Boolean,
                )
            }
        };
        Some(self.compute(op, fact, steps))
    }

    fn compute(&mut self, op: Operation, fact: Fact, steps: &mut Vec<Step>) -> Operand {
        let id = self.facts.len();
        self.facts.push(fact);
        steps.push(Step::Compute {
            destination: id,
            op,
        });
        Operand::Local(id)
    }
}

fn arithmetic_bounds(op: FixnumArithmetic, x: Bounds, y: Bounds) -> (i128, i128) {
    let (a, b) = (i128::from(x.low), i128::from(x.high));
    let (c, d) = (i128::from(y.low), i128::from(y.high));
    match op {
        FixnumArithmetic::Add | FixnumArithmetic::Inc => (a + c, b + d),
        FixnumArithmetic::Sub | FixnumArithmetic::Dec => (a - d, b - c),
        FixnumArithmetic::Mul => {
            let products = [a * c, a * d, b * c, b * d];
            (
                *products.iter().min().unwrap(),
                *products.iter().max().unwrap(),
            )
        }
    }
}
