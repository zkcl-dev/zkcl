//! Unified Sparse Conditional Constant Propagation (SCCP) with SSI edge refinement
//! and bounded fixnum interval abstraction.

use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::Immediate;

use super::cfg::control_targets;
use super::dom::DomTree;
use crate::ir::{
    BlockId, CmpOp, SpecializationInputs, SsaFunction, SsaInst, SsaOp, Terminator, TypeTag,
    ValueId, ValueRepr,
};

const MAX_PARAM_RANGE_UPDATES: u8 = 4;

/// Inclusive closed integer interval over `i64`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IntRange {
    pub low: i64,
    pub high: i64,
}

impl IntRange {
    #[inline]
    pub(crate) const fn singleton(n: i64) -> Self {
        Self { low: n, high: n }
    }

    #[inline]
    pub(crate) const fn full(bounds: (i64, i64)) -> Self {
        Self {
            low: bounds.0,
            high: bounds.1,
        }
    }

    #[inline]
    pub(crate) const fn join(self, other: Self) -> Self {
        Self {
            low: if self.low < other.low {
                self.low
            } else {
                other.low
            },
            high: if self.high > other.high {
                self.high
            } else {
                other.high
            },
        }
    }

    #[inline]
    pub(crate) const fn intersect(self, other: Self) -> Option<Self> {
        let low = if self.low > other.low {
            self.low
        } else {
            other.low
        };
        let high = if self.high < other.high {
            self.high
        } else {
            other.high
        };
        if low <= high {
            Some(Self { low, high })
        } else {
            None
        }
    }

    #[inline]
    pub(crate) const fn fits_in(self, bounds: (i64, i64)) -> bool {
        self.low >= bounds.0 && self.high <= bounds.1
    }

    #[inline]
    pub(crate) const fn as_singleton(self) -> Option<i64> {
        if self.low == self.high {
            Some(self.low)
        } else {
            None
        }
    }

    fn exclude_singleton(self, other: Self) -> Option<Self> {
        // Removing an interior value would require two intervals.
        match other.as_singleton() {
            Some(value) if self.low == value && self.high == value => None,
            Some(value) if self.low == value => Some(Self {
                low: value.saturating_add(1),
                high: self.high,
            }),
            Some(value) if self.high == value => Some(Self {
                low: self.low,
                high: value.saturating_sub(1),
            }),
            _ => Some(self),
        }
    }

    fn widen(self, next: Self, bounds: (i64, i64)) -> Self {
        let joined = self.join(next);
        let low = if joined.low < self.low {
            if joined.low >= 0 { 0 } else { bounds.0 }
        } else {
            joined.low
        };
        let high = if joined.high > self.high {
            bounds.1
        } else {
            joined.high
        };
        Self { low, high }
    }

    fn add(self, rhs: Self) -> Option<Self> {
        let low = i128::from(self.low) + i128::from(rhs.low);
        let high = i128::from(self.high) + i128::from(rhs.high);
        Some(Self {
            low: i64::try_from(low).ok()?,
            high: i64::try_from(high).ok()?,
        })
    }

    fn sub(self, rhs: Self) -> Option<Self> {
        let low = i128::from(self.low) - i128::from(rhs.high);
        let high = i128::from(self.high) - i128::from(rhs.low);
        Some(Self {
            low: i64::try_from(low).ok()?,
            high: i64::try_from(high).ok()?,
        })
    }

    fn mul(self, rhs: Self) -> Option<Self> {
        let corners = [
            i128::from(self.low) * i128::from(rhs.low),
            i128::from(self.low) * i128::from(rhs.high),
            i128::from(self.high) * i128::from(rhs.low),
            i128::from(self.high) * i128::from(rhs.high),
        ];
        let low = *corners.iter().min().unwrap();
        let high = *corners.iter().max().unwrap();
        Some(Self {
            low: i64::try_from(low).ok()?,
            high: i64::try_from(high).ok()?,
        })
    }

    fn eval_cmp(self, op: CmpOp, rhs: Self) -> Option<bool> {
        match op {
            CmpOp::Eq => {
                if self.high < rhs.low || rhs.high < self.low {
                    Some(false)
                } else if self.low == self.high && rhs.low == rhs.high && self.low == rhs.low {
                    Some(true)
                } else {
                    None
                }
            }
            CmpOp::Ne => Self::eval_cmp(self, CmpOp::Eq, rhs).map(|eq| !eq),
            CmpOp::Lt => {
                if self.high < rhs.low {
                    Some(true)
                } else if self.low >= rhs.high {
                    Some(false)
                } else {
                    None
                }
            }
            CmpOp::Le => {
                if self.high <= rhs.low {
                    Some(true)
                } else if self.low > rhs.high {
                    Some(false)
                } else {
                    None
                }
            }
            CmpOp::Gt => Self::eval_cmp(rhs, CmpOp::Lt, self),
            CmpOp::Ge => Self::eval_cmp(rhs, CmpOp::Le, self),
        }
    }

    fn narrow_cmp(
        op: CmpOp,
        lhs: Self,
        rhs: Self,
        taken: bool,
    ) -> (Option<Self>, Option<Self>) {
        let eff_op = if taken { op } else { negate_cmp(op) };
        match eff_op {
            CmpOp::Eq => {
                let both = lhs.intersect(rhs);
                (both, both)
            }
            CmpOp::Ne => (lhs.exclude_singleton(rhs), rhs.exclude_singleton(lhs)),
            CmpOp::Lt => {
                let l = rhs
                    .high
                    .checked_sub(1)
                    .map_or(None, |max_l| lhs.intersect(Self { low: i64::MIN, high: max_l }));
                let r = lhs
                    .low
                    .checked_add(1)
                    .map_or(None, |min_r| rhs.intersect(Self { low: min_r, high: i64::MAX }));
                (l, r)
            }
            CmpOp::Le => {
                let l = lhs.intersect(Self {
                    low: i64::MIN,
                    high: rhs.high,
                });
                let r = rhs.intersect(Self {
                    low: lhs.low,
                    high: i64::MAX,
                });
                (l, r)
            }
            CmpOp::Gt => {
                let l = rhs
                    .low
                    .checked_add(1)
                    .map_or(None, |min_l| lhs.intersect(Self { low: min_l, high: i64::MAX }));
                let r = lhs
                    .high
                    .checked_sub(1)
                    .map_or(None, |max_r| rhs.intersect(Self { low: i64::MIN, high: max_r }));
                (l, r)
            }
            CmpOp::Ge => {
                let l = lhs.intersect(Self {
                    low: rhs.low,
                    high: i64::MAX,
                });
                let r = rhs.intersect(Self {
                    low: i64::MIN,
                    high: lhs.high,
                });
                (l, r)
            }
        }
    }
}

const fn negate_cmp(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Ne,
        CmpOp::Ne => CmpOp::Eq,
        CmpOp::Lt => CmpOp::Ge,
        CmpOp::Ge => CmpOp::Lt,
        CmpOp::Le => CmpOp::Gt,
        CmpOp::Gt => CmpOp::Le,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConstLattice<T> {
    Bottom,
    Known(T),
    Overdefined,
}

impl<T: Copy + Eq> ConstLattice<T> {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Bottom, x) | (x, Self::Bottom) => x,
            (Self::Known(a), Self::Known(b)) if a == b => Self::Known(a),
            _ => Self::Overdefined,
        }
    }

    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Overdefined, x) | (x, Self::Overdefined) => x,
            (Self::Known(a), Self::Known(b)) if a == b => Self::Known(a),
            _ => Self::Bottom,
        }
    }

    fn known(self) -> Option<T> {
        match self {
            Self::Known(v) => Some(v),
            Self::Bottom | Self::Overdefined => None,
        }
    }
}

/// Product abstract domain for SSA values combining type tag, constants, and integer range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValueFact {
    ty: TypeTag,
    const_word: ConstLattice<Immediate>,
    const_i64: ConstLattice<i64>,
    const_bool: ConstLattice<bool>,
    int_range: Option<IntRange>,
}

impl ValueFact {
    const BOTTOM: Self = Self {
        ty: TypeTag::Bottom,
        const_word: ConstLattice::Bottom,
        const_i64: ConstLattice::Bottom,
        const_bool: ConstLattice::Bottom,
        int_range: None,
    };

    const ANY: Self = Self {
        ty: TypeTag::Any,
        const_word: ConstLattice::Overdefined,
        const_i64: ConstLattice::Overdefined,
        const_bool: ConstLattice::Overdefined,
        int_range: None,
    };

    fn from_type(ty: TypeTag, bounds: (i64, i64)) -> Self {
        match ty {
            TypeTag::Bottom => Self::BOTTOM,
            TypeTag::Nil => Self::const_word(Immediate::Nil),
            TypeTag::True => Self::const_word(Immediate::True),
            TypeTag::Fixnum => Self {
                ty: TypeTag::Fixnum,
                const_word: ConstLattice::Overdefined,
                const_i64: ConstLattice::Overdefined,
                const_bool: ConstLattice::Overdefined,
                int_range: Some(IntRange::full(bounds)),
            },
            other => Self {
                ty: other,
                const_word: ConstLattice::Overdefined,
                const_i64: ConstLattice::Overdefined,
                const_bool: ConstLattice::Overdefined,
                int_range: None,
            },
        }
    }

    fn const_word(imm: Immediate) -> Self {
        let int_range = match imm {
            Immediate::Fixnum(n) => Some(IntRange::singleton(n)),
            _ => None,
        };
        let const_i64 = match imm {
            Immediate::Fixnum(n) => ConstLattice::Known(n),
            _ => ConstLattice::Overdefined,
        };
        Self {
            ty: TypeTag::from_immediate(imm),
            const_word: ConstLattice::Known(imm),
            const_i64,
            const_bool: ConstLattice::Overdefined,
            int_range,
        }
    }

    fn const_i64(n: i64) -> Self {
        Self {
            ty: TypeTag::Fixnum,
            const_word: ConstLattice::Overdefined,
            const_i64: ConstLattice::Known(n),
            const_bool: ConstLattice::Overdefined,
            int_range: Some(IntRange::singleton(n)),
        }
    }

    fn const_bool(ty: TypeTag, b: bool) -> Self {
        Self {
            ty,
            const_word: ConstLattice::Overdefined,
            const_i64: ConstLattice::Overdefined,
            const_bool: ConstLattice::Known(b),
            int_range: None,
        }
    }

    fn fixnum_range(range: IntRange, bounds: (i64, i64)) -> Self {
        if let Some(n) = range.as_singleton() {
            if n >= bounds.0 && n <= bounds.1 {
                return Self {
                    ty: TypeTag::Fixnum,
                    const_word: ConstLattice::Known(Immediate::Fixnum(n)),
                    const_i64: ConstLattice::Known(n),
                    const_bool: ConstLattice::Overdefined,
                    int_range: Some(range),
                };
            }
            return Self::const_i64(n);
        }
        Self {
            ty: TypeTag::Fixnum,
            const_word: ConstLattice::Overdefined,
            const_i64: ConstLattice::Overdefined,
            const_bool: ConstLattice::Overdefined,
            int_range: Some(range),
        }
    }

    fn join(self, other: Self) -> Self {
        if self.ty == TypeTag::Bottom {
            return other;
        }
        if other.ty == TypeTag::Bottom {
            return self;
        }
        let int_range = match (self.int_range, other.int_range) {
            (Some(a), Some(b)) => Some(a.join(b)),
            _ => None,
        };
        Self {
            ty: self.ty.join(other.ty),
            const_word: self.const_word.join(other.const_word),
            const_i64: self.const_i64.join(other.const_i64),
            const_bool: self.const_bool.join(other.const_bool),
            int_range,
        }
    }

    fn meet(self, other: Self) -> Self {
        if self.ty == TypeTag::Bottom || other.ty == TypeTag::Bottom {
            return Self::BOTTOM;
        }
        let ty = self.ty.meet(other.ty);
        if ty == TypeTag::Bottom {
            return Self::BOTTOM;
        }
        let int_range = match (self.int_range, other.int_range) {
            (Some(a), Some(b)) => match a.intersect(b) {
                Some(r) => Some(r),
                None => return Self::BOTTOM,
            },
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let mut const_word = self.const_word.meet(other.const_word);
        let mut const_i64 = self.const_i64.meet(other.const_i64);
        if let Some(n) = int_range.and_then(IntRange::as_singleton) {
            const_i64 = ConstLattice::Known(n);
            if ty == TypeTag::Fixnum {
                const_word = ConstLattice::Known(Immediate::Fixnum(n));
            }
        }
        Self {
            ty,
            const_word,
            const_i64,
            const_bool: self.const_bool.meet(other.const_bool),
            int_range,
        }
    }
}

#[derive(Clone, Copy)]
enum BranchPredicate {
    IsFixnum(ValueId),
    IsNil(ValueId),
    IsFixnumInRange { value: ValueId, low: i32, high: i32 },
    CmpI64 { op: CmpOp, lhs: ValueId, rhs: ValueId },
    Not(ValueId),
}

struct FlowSolver<'a> {
    func: &'a SsaFunction,
    specialization: &'a dyn SpecializationInputs,
    dom: &'a DomTree,
    block_map: FxHashMap<BlockId, usize>,
    bounds: (i64, i64),
    facts: Vec<ValueFact>,
    param_range_updates: Vec<u8>,
    predicates: FxHashMap<ValueId, BranchPredicate>,
    /// Dominating edge-derived refinements attached to single-pred blocks.
    edge_refinements: FxHashMap<BlockId, Vec<(ValueId, ValueFact)>>,
    /// Reverse dependency: changing `ValueId` requires recomputing target block refinements.
    refinement_deps: FxHashMap<ValueId, Vec<BlockId>>,
    uses: FxHashMap<ValueId, Vec<BlockId>>,
    reachable_blocks: FxHashSet<BlockId>,
    executable_edges: FxHashSet<(BlockId, BlockId)>,
    cfg_worklist: VecDeque<BlockId>,
    ssa_worklist: VecDeque<BlockId>,
    in_ssa_worklist: FxHashSet<BlockId>,
}

impl<'a> FlowSolver<'a> {
    fn new(
        func: &'a SsaFunction,
        specialization: &'a dyn SpecializationInputs,
        dom: &'a DomTree,
    ) -> Self {
        let block_map = func.block_index_map();
        let bounds = func.fixnum_bounds;
        let num_vals = func.next_value_id as usize;
        let mut facts = vec![ValueFact::BOTTOM; num_vals];
        let mut predicates = FxHashMap::default();
        let mut uses: FxHashMap<ValueId, Vec<BlockId>> = FxHashMap::default();

        let control_entries = control_targets(func);
        for b in &func.blocks {
            let fixed = b.id == func.entry || control_entries.contains(&b.id);
            for p in &b.params {
                if fixed {
                    let initial = if p.repr == ValueRepr::UnboxedI64 {
                        ValueFact::from_type(TypeTag::Fixnum, bounds)
                    } else if b.id == func.entry {
                        ValueFact::from_type(p.ty, bounds)
                    } else {
                        ValueFact::ANY
                    };
                    facts[p.value.0 as usize] = initial;
                }
            }
            for inst in &b.insts {
                if let Some(id) = inst.id {
                    match inst.op {
                        SsaOp::IsFixnum(v) => {
                            predicates.insert(id, BranchPredicate::IsFixnum(v));
                        }
                        SsaOp::IsNil(v) => {
                            predicates.insert(id, BranchPredicate::IsNil(v));
                        }
                        SsaOp::IsFixnumInRange { value, low, high } => {
                            predicates.insert(
                                id,
                                BranchPredicate::IsFixnumInRange { value, low, high },
                            );
                        }
                        SsaOp::CmpI64 { op, lhs, rhs } => {
                            predicates.insert(id, BranchPredicate::CmpI64 { op, lhs, rhs });
                        }
                        SsaOp::NotBool(inner) => {
                            predicates.insert(id, BranchPredicate::Not(inner));
                        }
                        _ => {}
                    }
                }
                inst.op.visit_inputs(|v| {
                    let list = uses.entry(v).or_default();
                    if list.last().copied() != Some(b.id) {
                        list.push(b.id);
                    }
                });
            }
            b.terminator.visit_inputs(|v| {
                let list = uses.entry(v).or_default();
                if list.last().copied() != Some(b.id) {
                    list.push(b.id);
                }
            });
        }

        let mut solver = Self {
            func,
            specialization,
            dom,
            block_map,
            bounds,
            facts,
            param_range_updates: vec![0; num_vals],
            predicates,
            edge_refinements: FxHashMap::default(),
            refinement_deps: FxHashMap::default(),
            uses,
            reachable_blocks: FxHashSet::default(),
            executable_edges: FxHashSet::default(),
            cfg_worklist: VecDeque::new(),
            ssa_worklist: VecDeque::new(),
            in_ssa_worklist: FxHashSet::default(),
        };
        solver.mark_block_reachable(func.entry);
        solver
    }

    fn enqueue_ssa(&mut self, bid: BlockId) {
        if self.reachable_blocks.contains(&bid) && self.in_ssa_worklist.insert(bid) {
            self.ssa_worklist.push_back(bid);
        }
    }

    fn mark_block_reachable(&mut self, bid: BlockId) {
        if self.reachable_blocks.insert(bid) {
            self.cfg_worklist.push_back(bid);
        }
    }

    fn effective_fact(&self, value: ValueId, at_block: BlockId) -> ValueFact {
        effective_fact_in(&self.facts, &self.edge_refinements, self.dom, value, at_block)
    }

    fn finish(self) -> FlowSolution {
        FlowSolution {
            facts: self.facts,
            edge_refinements: self.edge_refinements,
            reachable_blocks: self.reachable_blocks,
        }
    }

    fn update_value_fact(&mut self, value: ValueId, next: ValueFact) -> bool {
        let idx = value.0 as usize;
        if self.facts[idx] == next {
            return false;
        }
        self.facts[idx] = next;
        if let Some(users) = self.uses.get(&value).cloned() {
            for user_block in users {
                self.enqueue_ssa(user_block);
            }
        }
        if let Some(dep_blocks) = self.refinement_deps.get(&value).cloned() {
            for dep_block in dep_blocks {
                self.enqueue_ssa(dep_block);
                for &child in self.dom.children(dep_block) {
                    self.enqueue_ssa(child);
                }
            }
        }
        true
    }

    fn solve(&mut self) {
        while !self.cfg_worklist.is_empty() || !self.ssa_worklist.is_empty() {
            while let Some(bid) = self.cfg_worklist.pop_front() {
                self.visit_block(bid);
            }
            while let Some(bid) = self.ssa_worklist.pop_front() {
                self.in_ssa_worklist.remove(&bid);
                self.visit_block(bid);
            }
        }
    }

    fn visit_block(&mut self, bid: BlockId) {
        let bidx = self.block_map[&bid];
        let block = &self.func.blocks[bidx];

        // Any unwind/control targets registered on this reachable block can be entered at runtime.
        for &unwind_target in &block.unwind_succs {
            self.mark_block_reachable(unwind_target);
        }
        super::cfg::visit_control_targets(block, |target| {
            self.mark_block_reachable(target);
        });

        for inst in &block.insts {
            let Some(id) = inst.id else {
                continue;
            };
            let inferred = self.eval_inst(inst, bid);
            self.update_value_fact(id, inferred);
        }

        match &block.terminator {
            Terminator::Jump(target) => {
                self.activate_edge(bid, target.block, &target.args, None);
            }
            Terminator::BranchTruthy {
                cond,
                truthy,
                falsy,
            } => {
                let cond_fact = self.effective_fact(*cond, bid);
                if cond_fact.ty == TypeTag::Bottom {
                    return;
                }
                if cond_fact.ty.is_definitely_truthy() {
                    self.activate_edge(bid, truthy.block, &truthy.args, None);
                } else if cond_fact.ty.is_definitely_nil() {
                    self.activate_edge(bid, falsy.block, &falsy.args, None);
                } else {
                    self.activate_edge(bid, truthy.block, &truthy.args, None);
                    let falsy_ref = vec![(*cond, ValueFact::const_word(Immediate::Nil))];
                    self.activate_edge(
                        bid,
                        falsy.block,
                        &falsy.args,
                        Some((vec![*cond], falsy_ref)),
                    );
                }
            }
            Terminator::BranchBool {
                cond,
                then_target,
                else_target,
            } => {
                let cond_fact = self.effective_fact(*cond, bid);
                if cond_fact.ty == TypeTag::Bottom {
                    return;
                }
                match cond_fact.const_bool.known() {
                    Some(true) => {
                        self.activate_edge(bid, then_target.block, &then_target.args, None);
                    }
                    Some(false) => {
                        self.activate_edge(bid, else_target.block, &else_target.args, None);
                    }
                    None => {
                        let then_ref = self.compute_predicate_refinement(*cond, bid, true);
                        let else_ref = self.compute_predicate_refinement(*cond, bid, false);
                        self.activate_edge(
                            bid,
                            then_target.block,
                            &then_target.args,
                            then_ref,
                        );
                        self.activate_edge(
                            bid,
                            else_target.block,
                            &else_target.args,
                            else_ref,
                        );
                    }
                }
            }
            Terminator::ReturnOne(_)
            | Terminator::ReturnValues(_)
            | Terminator::ReturnDynamicValues
            | Terminator::Unwind(_) => {}
        }
    }

    fn compute_predicate_refinement(
        &self,
        mut cond: ValueId,
        at_block: BlockId,
        mut taken: bool,
    ) -> Option<(Vec<ValueId>, Vec<(ValueId, ValueFact)>)> {
        let mut steps = 0;
        while let Some(&BranchPredicate::Not(inner)) = self.predicates.get(&cond) {
            if steps >= 8 {
                return None;
            }
            cond = inner;
            taken = !taken;
            steps += 1;
        }
        let pred = self.predicates.get(&cond).copied()?;
        match pred {
            BranchPredicate::IsFixnum(v) if taken => Some((
                vec![v],
                vec![(v, ValueFact::from_type(TypeTag::Fixnum, self.bounds))],
            )),
            BranchPredicate::IsFixnum(_) => None,
            BranchPredicate::IsNil(v) if taken => {
                Some((vec![v], vec![(v, ValueFact::const_word(Immediate::Nil))]))
            }
            BranchPredicate::IsNil(_) => None,
            BranchPredicate::IsFixnumInRange { value, low, high } if taken => {
                let range = IntRange {
                    low: i64::from(low),
                    high: i64::from(high),
                }
                .intersect(IntRange::full(self.bounds))?;
                Some((
                    vec![value],
                    vec![(value, ValueFact::fixnum_range(range, self.bounds))],
                ))
            }
            BranchPredicate::IsFixnumInRange { .. } => None,
            BranchPredicate::CmpI64 { op, lhs, rhs } => {
                let l_range = self
                    .effective_fact(lhs, at_block)
                    .int_range
                    .unwrap_or_else(|| IntRange::full(self.bounds));
                let r_range = self
                    .effective_fact(rhs, at_block)
                    .int_range
                    .unwrap_or_else(|| IntRange::full(self.bounds));
                let (nl, nr) = IntRange::narrow_cmp(op, l_range, r_range, taken);
                let mut refs = Vec::new();
                if let Some(r) = nl {
                    refs.push((lhs, ValueFact::fixnum_range(r, self.bounds)));
                }
                if let Some(r) = nr {
                    refs.push((rhs, ValueFact::fixnum_range(r, self.bounds)));
                }
                (!refs.is_empty()).then_some((vec![lhs, rhs], refs))
            }
            BranchPredicate::Not(_) => None,
        }
    }

    fn activate_edge(
        &mut self,
        from: BlockId,
        to: BlockId,
        args: &[ValueId],
        refinement: Option<(Vec<ValueId>, Vec<(ValueId, ValueFact)>)>,
    ) {
        let first_activation = self.executable_edges.insert((from, to));

        // Update edge refinement for single-predecessor targets before evaluating target block.
        if let Some((deps, refs)) = refinement
            && self.dom.cfg_preds(to).len() == 1
        {
            for dep in deps {
                let dep_list = self.refinement_deps.entry(dep).or_default();
                if !dep_list.contains(&to) {
                    dep_list.push(to);
                }
            }
            let prev = self.edge_refinements.get(&to);
            if prev != Some(&refs) {
                self.edge_refinements.insert(to, refs);
                if !first_activation {
                    self.enqueue_ssa(to);
                    for &child in self.dom.children(to) {
                        self.enqueue_ssa(child);
                    }
                }
            }
        }

        let Some(&tidx) = self.block_map.get(&to) else {
            return;
        };
        let target_block = &self.func.blocks[tidx];
        let is_backedge = self.dom.dominates(to, from);

        let mut param_updates = Vec::new();
        for (param, &arg) in target_block.params.iter().zip(args) {
            let arg_fact = self.effective_fact(arg, from);
            if arg_fact.ty == TypeTag::Bottom {
                continue;
            }
            let pidx = param.value.0 as usize;
            let old = self.facts[pidx];
            let mut joined = old.join(arg_fact);
            if is_backedge
                && let (Some(old_r), Some(joined_r)) = (old.int_range, joined.int_range)
                && old_r != joined_r
            {
                let updates = &mut self.param_range_updates[pidx];
                *updates = updates.saturating_add(1);
                if *updates > MAX_PARAM_RANGE_UPDATES {
                    let widened = old_r.widen(joined_r, self.bounds);
                    joined.int_range = Some(widened);
                    if widened.as_singleton().is_none() {
                        joined.const_i64 = ConstLattice::Overdefined;
                        joined.const_word = ConstLattice::Overdefined;
                    }
                }
            }
            if joined != old {
                param_updates.push((param.value, joined));
            }
        }
        for (param_val, fact) in param_updates {
            self.update_value_fact(param_val, fact);
        }

        if first_activation {
            self.mark_block_reachable(to);
        }
    }

    fn eval_inst(&self, inst: &SsaInst, bid: BlockId) -> ValueFact {
        let fact = |v| self.effective_fact(v, bid);
        match &inst.op {
            SsaOp::ConstWord(imm) => ValueFact::const_word(*imm),
            SsaOp::ConstI64(n) => ValueFact::const_i64(*n),
            SsaOp::ConstBool(b) => ValueFact::const_bool(inst.ty, *b),
            SsaOp::LoadConst(idx) => {
                ValueFact::from_type(self.specialization.constant_type(*idx), self.bounds)
            }
            SsaOp::Copy(src) => fact(*src),
            SsaOp::SelectBool {
                cond,
                when_true,
                when_false,
            } => match fact(*cond).const_bool.known() {
                Some(true) => ValueFact::const_word(*when_true),
                Some(false) => ValueFact::const_word(*when_false),
                None => ValueFact::const_word(*when_true).join(ValueFact::const_word(*when_false)),
            },
            SsaOp::BoxFixnum(src) => {
                let s = fact(*src);
                if s.ty == TypeTag::Bottom {
                    return ValueFact::BOTTOM;
                }
                if let Some(range) = s.int_range {
                    ValueFact::fixnum_range(range, self.bounds)
                } else {
                    ValueFact::from_type(TypeTag::Fixnum, self.bounds)
                }
            }
            SsaOp::UnboxFixnumGuard(src) => {
                let s = fact(*src);
                if s.ty == TypeTag::Bottom {
                    return ValueFact::BOTTOM;
                }
                if let Some(Immediate::Fixnum(n)) = s.const_word.known() {
                    return ValueFact::const_i64(n);
                }
                if let Some(range) = s.int_range.and_then(|r| r.intersect(IntRange::full(self.bounds))) {
                    ValueFact::fixnum_range(range, self.bounds)
                } else {
                    ValueFact::from_type(TypeTag::Fixnum, self.bounds)
                }
            }
            SsaOp::AddI64Checked(lhs, rhs)
            | SsaOp::AddI64Unchecked(lhs, rhs)
            | SsaOp::SubI64Checked(lhs, rhs)
            | SsaOp::SubI64Unchecked(lhs, rhs)
            | SsaOp::MulI64Checked(lhs, rhs)
            | SsaOp::MulI64Unchecked(lhs, rhs) => {
                let lf = fact(*lhs);
                let rf = fact(*rhs);
                if lf.ty == TypeTag::Bottom || rf.ty == TypeTag::Bottom {
                    return ValueFact::BOTTOM;
                }
                let lr = lf.int_range.unwrap_or_else(|| IntRange::full(self.bounds));
                let rr = rf.int_range.unwrap_or_else(|| IntRange::full(self.bounds));
                let computed = match inst.op {
                    SsaOp::AddI64Checked(..) | SsaOp::AddI64Unchecked(..) => lr.add(rr),
                    SsaOp::SubI64Checked(..) | SsaOp::SubI64Unchecked(..) => lr.sub(rr),
                    _ => lr.mul(rr),
                };
                if let Some(range) = computed.and_then(|r| r.intersect(IntRange::full(self.bounds))) {
                    ValueFact::fixnum_range(range, self.bounds)
                } else {
                    ValueFact::from_type(TypeTag::Fixnum, self.bounds)
                }
            }
            SsaOp::CmpI64 { op, lhs, rhs } => {
                let lf = fact(*lhs);
                let rf = fact(*rhs);
                if lf.ty == TypeTag::Bottom || rf.ty == TypeTag::Bottom {
                    return ValueFact::BOTTOM;
                }
                if let (Some(lr), Some(rr)) = (lf.int_range, rf.int_range)
                    && let Some(res) = lr.eval_cmp(*op, rr)
                {
                    return ValueFact::const_bool(inst.ty, res);
                }
                ValueFact::from_type(inst.ty, self.bounds)
            }
            SsaOp::IsPresent(src) => {
                let s = fact(*src);
                if s.ty == TypeTag::Bottom {
                    ValueFact::BOTTOM
                } else if s.const_word.known().is_some()
                    || !matches!(s.ty, TypeTag::Top | TypeTag::Any)
                {
                    ValueFact::const_bool(inst.ty, true)
                } else {
                    ValueFact::from_type(inst.ty, self.bounds)
                }
            }
            SsaOp::IsNil(src) => {
                let s = fact(*src);
                if s.ty == TypeTag::Bottom {
                    ValueFact::BOTTOM
                } else if s.ty.is_definitely_nil() {
                    ValueFact::const_bool(inst.ty, true)
                } else if s.ty.is_definitely_truthy() {
                    ValueFact::const_bool(inst.ty, false)
                } else {
                    ValueFact::from_type(inst.ty, self.bounds)
                }
            }
            SsaOp::IsFixnum(src) => {
                let s = fact(*src);
                if s.ty == TypeTag::Bottom {
                    ValueFact::BOTTOM
                } else if s.ty == TypeTag::Fixnum {
                    ValueFact::const_bool(inst.ty, true)
                } else if !matches!(s.ty, TypeTag::Top | TypeTag::Any) {
                    ValueFact::const_bool(inst.ty, false)
                } else {
                    ValueFact::from_type(inst.ty, self.bounds)
                }
            }
            SsaOp::IsFixnumInRange { value, low, high } => {
                let s = fact(*value);
                if s.ty == TypeTag::Bottom {
                    return ValueFact::BOTTOM;
                }
                if !matches!(s.ty, TypeTag::Top | TypeTag::Any | TypeTag::Fixnum) {
                    return ValueFact::const_bool(inst.ty, false);
                }
                if let Some(r) = s.int_range {
                    let lo = i64::from(*low);
                    let hi = i64::from(*high);
                    if r.low >= lo && r.high <= hi {
                        return ValueFact::const_bool(inst.ty, true);
                    }
                    if r.high < lo || r.low > hi {
                        return ValueFact::const_bool(inst.ty, false);
                    }
                }
                ValueFact::from_type(inst.ty, self.bounds)
            }
            SsaOp::NotBool(src) => {
                let s = fact(*src);
                if s.ty == TypeTag::Bottom {
                    ValueFact::BOTTOM
                } else if let Some(b) = s.const_bool.known() {
                    ValueFact::const_bool(inst.ty, !b)
                } else {
                    ValueFact::from_type(inst.ty, self.bounds)
                }
            }
            SsaOp::Closure { .. } | SsaOp::ReadCell { function: true, .. } => {
                ValueFact::from_type(TypeTag::Function, self.bounds)
            }
            _ => ValueFact::from_type(inst.ty, self.bounds),
        }
    }
}

struct FlowSolution {
    facts: Vec<ValueFact>,
    edge_refinements: FxHashMap<BlockId, Vec<(ValueId, ValueFact)>>,
    reachable_blocks: FxHashSet<BlockId>,
}

impl FlowSolution {
    fn effective_fact(&self, dom: &DomTree, value: ValueId, at_block: BlockId) -> ValueFact {
        effective_fact_in(&self.facts, &self.edge_refinements, dom, value, at_block)
    }
}

fn effective_fact_in(
    facts: &[ValueFact],
    edge_refinements: &FxHashMap<BlockId, Vec<(ValueId, ValueFact)>>,
    dom: &DomTree,
    value: ValueId,
    at_block: BlockId,
) -> ValueFact {
    let mut fact = facts[value.0 as usize];
    if fact.ty == TypeTag::Bottom {
        return ValueFact::BOTTOM;
    }
    // Walk dominating single-predecessor refinement blocks (bounded by dominator depth).
    let mut cur = at_block;
    loop {
        if let Some(refs) = edge_refinements.get(&cur) {
            for &(target_val, ref_fact) in refs {
                if target_val == value {
                    fact = fact.meet(ref_fact);
                }
            }
        }
        let preds = dom.cfg_preds(cur);
        if preds.len() == 1 && dom.strictly_dominates(preds[0], cur) {
            cur = preds[0];
        } else {
            break;
        }
    }
    fact
}

/// Run SCCP with SSI edge refinements and fixnum interval narrowing.
pub(super) fn run_sparse_flow(
    func: &mut SsaFunction,
    specialization: &dyn SpecializationInputs,
    dom: &DomTree,
) -> bool {
    let mut solver = FlowSolver::new(func, specialization, dom);
    solver.solve();
    let solution = solver.finish();

    let bounds = func.fixnum_bounds;
    let mut changed = false;

    for block in &mut func.blocks {
        if !solution.reachable_blocks.contains(&block.id) {
            continue;
        }
        let bid = block.id;
        for param in &mut block.params {
            let inferred = solution.facts[param.value.0 as usize].ty;
            let target_ty = if inferred == TypeTag::Bottom {
                TypeTag::Any
            } else {
                inferred
            };
            if param.ty != target_ty {
                param.ty = target_ty;
                changed = true;
            }
        }

        for inst in &mut block.insts {
            let Some(id) = inst.id else {
                continue;
            };
            let fact = solution.effective_fact(dom, id, bid);
            let target_ty = if fact.ty == TypeTag::Bottom {
                TypeTag::Any
            } else {
                fact.ty
            };
            if inst.ty != target_ty {
                inst.ty = target_ty;
                changed = true;
            }

            match inst.op {
                SsaOp::UnboxFixnumGuard(_) => {
                    if let Some(n) = fact.const_i64.known() {
                        inst.op = SsaOp::ConstI64(n);
                        inst.ty = TypeTag::Fixnum;
                        inst.deopt = None;
                        changed = true;
                    }
                }
                SsaOp::AddI64Checked(lhs, rhs)
                | SsaOp::SubI64Checked(lhs, rhs)
                | SsaOp::MulI64Checked(lhs, rhs) => {
                    if let Some(n) = fact.const_i64.known()
                        && n >= bounds.0
                        && n <= bounds.1
                    {
                        inst.op = SsaOp::ConstI64(n);
                        inst.ty = TypeTag::Fixnum;
                        inst.deopt = None;
                        changed = true;
                    } else {
                        let lf = solution.effective_fact(dom, lhs, bid);
                        let rf = solution.effective_fact(dom, rhs, bid);
                        if let (Some(lr), Some(rr)) = (lf.int_range, rf.int_range) {
                            let exact = match inst.op {
                                SsaOp::AddI64Checked(..) => lr.add(rr),
                                SsaOp::SubI64Checked(..) => lr.sub(rr),
                                SsaOp::MulI64Checked(..) => lr.mul(rr),
                                _ => None,
                            };
                            if let Some(out) = exact
                                && out.fits_in(bounds)
                            {
                                inst.op = match &inst.op {
                                    SsaOp::AddI64Checked(..) => SsaOp::AddI64Unchecked(lhs, rhs),
                                    SsaOp::SubI64Checked(..) => SsaOp::SubI64Unchecked(lhs, rhs),
                                    SsaOp::MulI64Checked(..) => SsaOp::MulI64Unchecked(lhs, rhs),
                                    _ => unreachable!(),
                                };
                                inst.deopt = None;
                                changed = true;
                            }
                        }
                    }
                }
                SsaOp::CmpI64 { .. }
                | SsaOp::IsPresent(_)
                | SsaOp::IsNil(_)
                | SsaOp::IsFixnum(_)
                | SsaOp::IsFixnumInRange { .. }
                | SsaOp::NotBool(_) => {
                    if let Some(b) = fact.const_bool.known() {
                        inst.op = SsaOp::ConstBool(b);
                        changed = true;
                    }
                }
                SsaOp::SelectBool { .. } | SsaOp::BoxFixnum(_) => {
                    if let Some(imm) = fact.const_word.known() {
                        inst.op = SsaOp::ConstWord(imm);
                        inst.ty = TypeTag::from_immediate(imm);
                        changed = true;
                    }
                }
                _ => {}
            }
        }

        let new_term = match &block.terminator {
            Terminator::BranchTruthy {
                cond,
                truthy,
                falsy,
            } => {
                let cond_fact = solution.effective_fact(dom, *cond, bid);
                if cond_fact.ty.is_definitely_truthy() {
                    Some(Terminator::Jump(truthy.clone()))
                } else if cond_fact.ty.is_definitely_nil() {
                    Some(Terminator::Jump(falsy.clone()))
                } else {
                    None
                }
            }
            Terminator::BranchBool {
                cond,
                then_target,
                else_target,
            } => solution
                .effective_fact(dom, *cond, bid)
                .const_bool
                .known()
                .map(|b| {
                    if b {
                        Terminator::Jump(then_target.clone())
                    } else {
                        Terminator::Jump(else_target.clone())
                    }
                }),
            _ => None,
        };
        if let Some(term) = new_term {
            block.terminator = term;
            changed = true;
        }
    }

    changed
}
