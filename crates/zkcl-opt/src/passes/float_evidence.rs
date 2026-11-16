//! Same-format evidence used to select guarded native float calls.

use std::collections::VecDeque;

use rustc_hash::FxHashMap;

use super::cfg::{control_targets, incoming_branches};
use super::float::{float_type_symbol, named_arithmetic};
use crate::ir::{FloatFormat, SpecializationInputs, SsaFunction, SsaOp, Terminator, ValueId};

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum Fact {
    /// A value still to be evaluated, such as a loop backedge.
    Pending,
    Exact(FloatFormat),
    #[default]
    Unknown,
}

impl Fact {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Pending, fact) | (fact, Self::Pending) => fact,
            (a, b) if a == b => a,
            _ => Self::Unknown,
        }
    }

    fn arith(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Pending, _) | (_, Self::Pending) => Self::Pending,
            (a, b) => a.join(b),
        }
    }
}

enum Transfer {
    Parameter(Vec<ValueId>),
    Arithmetic([ValueId; 2]),
}

impl Transfer {
    fn inputs(&self) -> &[ValueId] {
        match self {
            Self::Parameter(values) => values,
            Self::Arithmetic(values) => values,
        }
    }

    fn evaluate(&self, facts: &[Fact]) -> Fact {
        let fact = |value: ValueId| facts[value.0 as usize];
        match self {
            Self::Parameter(values) => values
                .iter()
                .copied()
                .map(fact)
                .fold(Fact::Pending, Fact::join),
            Self::Arithmetic([lhs, rhs]) => fact(*lhs).arith(fact(*rhs)),
        }
    }
}

#[derive(Default)]
pub(super) struct StaticFloatFormats {
    facts: Vec<Fact>,
    vectors: FxHashMap<ValueId, Fact>,
}

impl StaticFloatFormats {
    pub(super) fn analyze(func: &SsaFunction, inputs: &dyn SpecializationInputs) -> Self {
        let mut seeds = FxHashMap::<ValueId, Fact>::default();
        let mut vectors = FxHashMap::<ValueId, Fact>::default();
        let mut type_specs = FxHashMap::default();
        let insts = || func.blocks.iter().flat_map(|block| &block.insts);
        for inst in insts() {
            let Some(id) = inst.id else { continue };
            match inst.op {
                SsaOp::LoadConst(index) => {
                    if let Some(format) = inputs.float_literal_format(index) {
                        seeds.insert(id, Fact::Exact(format));
                    }
                    let scalar = float_type_symbol(inputs.named_symbol(index));
                    let vector = inputs.float_vector_type_format(index);
                    if scalar.is_some() || vector.is_some() {
                        type_specs.insert(id, (scalar, vector));
                    }
                }
                SsaOp::BoxFloat { format, .. } => {
                    seeds.insert(id, Fact::Exact(format));
                }
                _ => {}
            }
        }

        // The compiler emits these predicates for checked declarations. Their
        // specifiers select a specialization, and cannot justify dropping its
        // operand guards: a program may trace TYPEP or use both branch arms.
        for block in &func.blocks {
            let Terminator::BranchTruthy { cond, .. } = block.terminator else {
                continue;
            };
            let Some(SsaOp::CallNamed {
                symbol,
                args,
                notinline: false,
                associate: false,
                ..
            }) = block
                .insts
                .iter()
                .find(|inst| inst.id == Some(cond))
                .map(|inst| &inst.op)
            else {
                continue;
            };
            if inputs.is_notinline(*symbol)
                || !matches!(
                    inputs.named_symbol(*symbol),
                    Some(("COMMON-LISP" | "CL", "TYPEP"))
                )
            {
                continue;
            }
            let [value, specifier] = args.as_slice() else {
                continue;
            };
            if let Some(&(scalar, vector)) = type_specs.get(specifier) {
                for (format, evidence) in [(scalar, &mut seeds), (vector, &mut vectors)] {
                    if let Some(format) = format {
                        let fact = evidence.entry(*value).or_insert(Fact::Pending);
                        *fact = fact.join(Fact::Exact(format));
                    }
                }
            }
        }

        if seeds.is_empty() && vectors.is_empty() {
            return Self::default();
        }
        let count = func.next_value_id as usize;
        let mut result = Self {
            facts: vec![Fact::Unknown; count],
            vectors,
        };
        let mut transfers: Vec<Option<Transfer>> = (0..count).map(|_| None).collect();
        let mut pending = VecDeque::new();
        let mut users = vec![Vec::new(); count];
        for (value, fact) in seeds {
            result.facts[value.0 as usize] = fact;
        }
        let mut add_transfer = |result: &mut Self, id: ValueId, transfer: Transfer| {
            let index = id.0 as usize;
            if result.facts[index] != Fact::Unknown {
                return;
            }
            for &input in transfer.inputs() {
                users[input.0 as usize].push(index);
            }
            result.facts[index] = Fact::Pending;
            transfers[index] = Some(transfer);
            pending.push_back(index);
        };
        let control = control_targets(func);
        let incoming = incoming_branches(func);
        for block in &func.blocks {
            if block.id == func.entry || control.contains(&block.id) {
                continue;
            }
            let Some(edges) = incoming.get(&block.id) else {
                continue;
            };
            for (position, param) in block.params.iter().enumerate() {
                let args = edges.iter().map(|edge| edge.args[position]).collect();
                add_transfer(&mut result, param.value, Transfer::Parameter(args));
            }
        }
        for inst in insts() {
            if let (
                Some(id),
                SsaOp::CallNamed {
                    symbol,
                    args,
                    notinline: false,
                    associate: false,
                    ..
                },
            ) = (inst.id, &inst.op)
                && !inputs.is_notinline(*symbol)
                && let &[lhs, rhs] = args.as_slice()
            {
                let name = inputs.named_symbol(*symbol);
                if named_arithmetic(name).is_some() {
                    add_transfer(&mut result, id, Transfer::Arithmetic([lhs, rhs]));
                } else if matches!(name, Some(("COMMON-LISP" | "CL", "AREF")))
                    && let Some(format) = result.vector(lhs)
                {
                    result.facts[id.0 as usize] = Fact::Exact(format);
                }
            }
        }
        while let Some(index) = pending.pop_front() {
            let next = transfers[index].as_ref().unwrap().evaluate(&result.facts);
            if next != result.facts[index] {
                result.facts[index] = next;
                pending.extend(users[index].iter().copied());
            }
        }
        result
    }

    pub(super) fn arithmetic(&self, lhs: ValueId, rhs: ValueId) -> Option<FloatFormat> {
        match self
            .facts
            .get(lhs.0 as usize)?
            .arith(*self.facts.get(rhs.0 as usize)?)
        {
            Fact::Exact(format) => Some(format),
            _ => None,
        }
    }

    pub(super) fn vector(&self, value: ValueId) -> Option<FloatFormat> {
        match self.vectors.get(&value)? {
            Fact::Exact(format) => Some(*format),
            _ => None,
        }
    }

    pub(super) fn scalar(&self, value: ValueId) -> Option<FloatFormat> {
        match self.facts.get(value.0 as usize)? {
            Fact::Exact(format) => Some(*format),
            _ => None,
        }
    }
}
