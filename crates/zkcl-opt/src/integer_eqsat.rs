//! Equality saturation for closed, range-proven fixnum expression DAGs in one SSA block.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use egg::{CostFunction, EGraph, Id, Language, RecExpr, Rewrite, define_language};
use rustc_hash::FxHashMap;

use crate::ir::{
    BasicBlock, BlockId, CmpOp, SsaFunction, SsaInst, SsaOp, Terminator, TypeTag, ValueId,
};
use crate::passes::visit_control_targets;

const MAX_PREDECESSOR_EDGES: usize = 16;
const MAX_INPUT_NODES: usize = 32;
const MAX_SEARCHES_PER_FUNCTION: usize = 4;
const MAX_ITERATIONS: usize = 8;
const MAX_EGRAPH_NODES: usize = 256;
const MAX_MATCHES_PER_RULE: usize = 64;
// Work units bound candidate setup, egg rewriting, and extraction independently.
const MAX_PREPROCESS_WORK_PER_FUNCTION: usize = 4_096;
const MAX_REWRITE_WORK_PER_FUNCTION: usize = 4_096;
const MAX_EXTRACTION_WORK_PER_FUNCTION: usize = 8_192;
// Preserve the previous four 5 ms region limits as one function-wide deadline.
const MAX_SEARCH_TIME_PER_FUNCTION: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BudgetStop {
    PreprocessWork,
    RewriteWork,
    ExtractionWork,
    Time,
}

struct SearchBudget {
    started: Instant,
    time_limit: Duration,
    preprocess_work_left: usize,
    rewrite_work_left: usize,
    extraction_work_left: usize,
    stop: Option<BudgetStop>,
}

impl SearchBudget {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            time_limit: MAX_SEARCH_TIME_PER_FUNCTION,
            preprocess_work_left: MAX_PREPROCESS_WORK_PER_FUNCTION,
            rewrite_work_left: MAX_REWRITE_WORK_PER_FUNCTION,
            extraction_work_left: MAX_EXTRACTION_WORK_PER_FUNCTION,
            stop: None,
        }
    }

    fn can_continue(&mut self) -> bool {
        self.check_deadline()
    }

    fn check_deadline(&mut self) -> bool {
        if self.stop.is_some() {
            return false;
        }
        if self.started.elapsed() >= self.time_limit {
            self.stop = Some(BudgetStop::Time);
            return false;
        }
        true
    }

    fn spend_preprocess_work(&mut self, work: usize) -> bool {
        if !self.check_deadline() {
            return false;
        }
        let Some(remaining) = self.preprocess_work_left.checked_sub(work) else {
            self.stop = Some(BudgetStop::PreprocessWork);
            return false;
        };
        self.preprocess_work_left = remaining;
        true
    }

    fn spend_rewrite_work(&mut self, work: usize) -> bool {
        if !self.check_deadline() {
            return false;
        }
        let Some(remaining) = self.rewrite_work_left.checked_sub(work) else {
            self.stop = Some(BudgetStop::RewriteWork);
            return false;
        };
        self.rewrite_work_left = remaining;
        true
    }

    fn spend_extraction_work(&mut self, work: usize) -> bool {
        if !self.check_deadline() {
            return false;
        }
        let Some(remaining) = self.extraction_work_left.checked_sub(work) else {
            self.stop = Some(BudgetStop::ExtractionWork);
            return false;
        };
        self.extraction_work_left = remaining;
        true
    }
}

define_language! {
    enum IntegerExpr {
        "+" = Add([Id; 2]),
        "*" = Mul([Id; 2]),
        Input(u32),
    }
}

static RULES: LazyLock<[Rewrite<IntegerExpr, ()>; 5]> = LazyLock::new(|| {
    [
        egg::rewrite!("factor"; "(+ (* ?a ?b) (* ?a ?c))" => "(* ?a (+ ?b ?c))"),
        egg::rewrite!("commute-add"; "(+ ?a ?b)" => "(+ ?b ?a)"),
        egg::rewrite!("commute-mul"; "(* ?a ?b)" => "(* ?b ?a)"),
        egg::rewrite!("associate-add"; "(+ (+ ?a ?b) ?c)" => "(+ ?a (+ ?b ?c))"),
        egg::rewrite!("associate-mul"; "(* (* ?a ?b) ?c)" => "(* ?a (* ?b ?c))"),
    ]
});

fn find_cheaper_expression(
    dag: &ExpressionDag,
    ranges: &BlockRanges<'_>,
    budget: &mut SearchBudget,
) -> Option<RecExpr<IntegerExpr>> {
    // Every rule retains all distinct inputs. A connected binary expression
    // on n distinct inputs needs at least n - 1 arithmetic nodes, even as a DAG.
    let input_count = dag.node_by_value.len() - dag.instruction_indices.len();
    let min_arithmetic_ops = input_count.saturating_sub(1);
    let one_op_above_minimum = dag.instruction_indices.len() == min_arithmetic_ops + 1;
    // egg 0.11 calibrates quanta's hardware clock in rebuild(), even for disabled
    // logging. Override that timer locally and enforce the deadline with std::time.
    let (clock, _) = quanta::Clock::mock();
    quanta::with_clock(&clock, || {
        if !budget.spend_rewrite_work(dag.expression.as_ref().len()) {
            return None;
        }
        let mut graph = EGraph::default();
        let root = graph.add_expr(&dag.expression);
        if !budget.spend_rewrite_work(graph.total_size()) {
            return None;
        }
        graph.rebuild();
        'saturate: for _ in 0..MAX_ITERATIONS {
            let mut changed = false;
            // Try factoring before the rules that enlarge the graph with
            // alternative operand orders and associations.
            for rule in RULES.iter() {
                // A rule can add at most two nodes per match.
                let remaining = MAX_EGRAPH_NODES.saturating_sub(graph.total_size());
                let limit = (remaining / 2).min(MAX_MATCHES_PER_RULE);
                if limit == 0 || !budget.spend_rewrite_work(graph.total_size()) {
                    break 'saturate;
                }
                let matches = rule.search_with_limit(&graph, limit);
                if !matches.is_empty() {
                    let applications: usize = matches.iter().map(|m| m.substs.len()).sum();
                    if !budget.spend_rewrite_work(
                        graph
                            .total_size()
                            .saturating_add(applications.saturating_mul(2)),
                    ) {
                        break 'saturate;
                    }
                    let applied = rule.apply(&mut graph, &matches);
                    changed |= !applied.is_empty();
                    graph.rebuild();
                    if one_op_above_minimum
                        && rule.name.as_str() == "factor"
                        && applied.iter().any(|&id| graph.find(id) == graph.find(root))
                    {
                        if !budget.can_continue() {
                            return None;
                        }
                        let (tree_cost, candidate) = extract_best(&graph, root, budget)?;
                        if tree_cost == min_arithmetic_ops
                            && check_candidate_ranges(ranges, &candidate, budget)
                        {
                            return Some(candidate);
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        if !budget.can_continue() {
            return None;
        }
        let (_, candidate) = extract_best(&graph, root, budget)?;
        // On a shared subexpression, tree cost can exceed the number of SSA instructions.
        let dag_arithmetic_ops = candidate
            .as_ref()
            .iter()
            .filter(|node| !matches!(node, IntegerExpr::Input(_)))
            .count();
        (dag_arithmetic_ops < dag.instruction_indices.len()
            && check_candidate_ranges(ranges, &candidate, budget))
        .then_some(candidate)
    })
}

fn check_candidate_ranges(
    ranges: &BlockRanges<'_>,
    candidate: &RecExpr<IntegerExpr>,
    budget: &mut SearchBudget,
) -> bool {
    budget.spend_extraction_work(candidate.as_ref().len())
        && ranges.check_intermediate_ranges(candidate).is_some()
}

fn extract_best(
    graph: &EGraph<IntegerExpr, ()>,
    root: Id,
    budget: &mut SearchBudget,
) -> Option<(usize, RecExpr<IntegerExpr>)> {
    let mut extractor = BoundedExtractor {
        graph,
        costs: FxHashMap::default(),
    };
    extractor.find_costs(budget)?;
    let (cost, node) = extractor.costs.get(&graph.find(root))?.clone();
    let candidate = node
        .try_build_recexpr(|id| {
            if !budget.spend_extraction_work(1) {
                return Err(());
            }
            extractor
                .costs
                .get(&graph.find(id))
                .map(|(_, node)| node.clone())
                .ok_or(())
        })
        .ok()?;
    Some((cost, candidate))
}

struct BoundedExtractor<'a> {
    graph: &'a EGraph<IntegerExpr, ()>,
    costs: FxHashMap<Id, (usize, IntegerExpr)>,
}

impl BoundedExtractor<'_> {
    /// Match egg's in-place class traversal and `min_by` tie choice, but charge
    /// every class and node examination so a cyclic graph cannot defer the stop.
    fn find_costs(&mut self, budget: &mut SearchBudget) -> Option<()> {
        let mut changed = true;
        while changed {
            if !budget.spend_extraction_work(self.graph.total_size().saturating_add(1)) {
                return None;
            }
            changed = false;
            for class in self.graph.classes() {
                if !budget.spend_extraction_work(class.len().saturating_add(1)) {
                    return None;
                }
                let candidate = class
                    .iter()
                    .filter_map(|node| {
                        if !node.all(|child| self.costs.contains_key(&self.graph.find(child))) {
                            return None;
                        }
                        let cost = ArithmeticTreeCost
                            .cost(node, |child| self.costs[&self.graph.find(child)].0);
                        Some((cost, node.clone()))
                    })
                    .min_by(|a, b| a.0.cmp(&b.0));
                match (&self.costs.get(&class.id), candidate) {
                    (None, Some(new)) => {
                        self.costs.insert(class.id, new);
                        changed = true;
                    }
                    (Some(old), Some(new)) if new.0 < old.0 => {
                        self.costs.insert(class.id, new);
                        changed = true;
                    }
                    _ => {}
                }
            }
        }
        Some(())
    }
}

struct ArithmeticTreeCost;

impl CostFunction<IntegerExpr> for ArithmeticTreeCost {
    type Cost = usize;

    fn cost<C>(&mut self, node: &IntegerExpr, mut child_cost: C) -> Self::Cost
    where
        C: FnMut(Id) -> Self::Cost,
    {
        match *node {
            IntegerExpr::Input(_) => 0,
            IntegerExpr::Add([a, b]) | IntegerExpr::Mul([a, b]) => 1usize
                .saturating_add(child_cost(a))
                .saturating_add(child_cost(b)),
        }
    }
}

#[derive(Clone, Copy)]
enum Arithmetic {
    Add,
    Mul,
}

fn match_fixnum_arithmetic(inst: &SsaInst) -> Option<(Arithmetic, ValueId, ValueId)> {
    let (op, a, b, checked) = match inst.op {
        SsaOp::AddI64Checked(a, b) => (Arithmetic::Add, a, b, true),
        SsaOp::AddI64Unchecked(a, b) => (Arithmetic::Add, a, b, false),
        SsaOp::MulI64Checked(a, b) => (Arithmetic::Mul, a, b, true),
        SsaOp::MulI64Unchecked(a, b) => (Arithmetic::Mul, a, b, false),
        _ => return None,
    };
    (inst.deopt.is_some() == checked).then_some((op, a, b))
}

fn has_add_and_two_products(block: &BasicBlock) -> bool {
    let (mut adds, mut products) = (0, 0);
    for inst in &block.insts {
        match match_fixnum_arithmetic(inst) {
            Some((Arithmetic::Add, ..)) => adds += 1,
            Some((Arithmetic::Mul, ..)) => products += 1,
            None => continue,
        }
        if adds >= 1 && products >= 2 {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy)]
struct Interval {
    low: i64,
    high: i64,
}

impl Interval {
    fn constant(n: i64) -> Self {
        Self { low: n, high: n }
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let low = self.low.max(other.low);
        let high = self.high.min(other.high);
        (low <= high).then_some(Self { low, high })
    }

    fn fixnum_result(self, op: Arithmetic, rhs: Self, domain: Self) -> Option<Self> {
        let (a, b) = (i128::from(self.low), i128::from(self.high));
        let (c, d) = (i128::from(rhs.low), i128::from(rhs.high));
        let (low, high) = match op {
            Arithmetic::Add => (a + c, b + d),
            Arithmetic::Mul => {
                let products = [a * c, a * d, b * c, b * d];
                (
                    *products.iter().min().unwrap(),
                    *products.iter().max().unwrap(),
                )
            }
        };
        (low >= i128::from(domain.low) && high <= i128::from(domain.high)).then_some(Self {
            low: low as i64,
            high: high as i64,
        })
    }
}

/// Use counts establish closure of a rewrite. The other indices supply bounds
/// from constants and from comparisons on unique incoming edges.
struct SsaIndex {
    domain: Interval,
    use_counts: FxHashMap<ValueId, usize>,
    fixnum_constants: FxHashMap<ValueId, i64>,
    integer_comparisons: FxHashMap<ValueId, (CmpOp, ValueId, ValueId)>,
    // None denotes multiple incoming edges or an implicit control transfer.
    unique_predecessor: FxHashMap<BlockId, Option<usize>>,
}

impl SsaIndex {
    fn collect(func: &SsaFunction) -> Self {
        let (low, high) = func.fixnum_bounds;
        let mut ssa = Self {
            domain: Interval { low, high },
            use_counts: FxHashMap::default(),
            fixnum_constants: FxHashMap::default(),
            integer_comparisons: FxHashMap::default(),
            unique_predecessor: FxHashMap::default(),
        };
        ssa.unique_predecessor.insert(func.entry, None);
        let mut mark_use = |value| *ssa.use_counts.entry(value).or_default() += 1;
        for (index, block) in func.blocks.iter().enumerate() {
            block.terminator.visit_inputs(&mut mark_use);
            block.terminator.visit_targets(|target| {
                ssa.unique_predecessor
                    .entry(target.block)
                    .and_modify(|pred| *pred = None)
                    .or_insert(Some(index));
            });
            visit_control_targets(block, |target| {
                ssa.unique_predecessor.insert(target, None);
            });
            for inst in &block.insts {
                inst.visit_inputs(&mut mark_use);
                match (inst.id, &inst.op) {
                    (Some(value), SsaOp::ConstI64(n)) if (low..=high).contains(n) => {
                        ssa.fixnum_constants.insert(value, *n);
                    }
                    (Some(value), SsaOp::CmpI64 { op, lhs, rhs }) => {
                        ssa.integer_comparisons.insert(value, (*op, *lhs, *rhs));
                    }
                    _ => {}
                }
            }
        }
        ssa
    }

    fn intervals_at_entry(
        &self,
        func: &SsaFunction,
        mut block: BlockId,
        budget: &mut SearchBudget,
    ) -> Option<FxHashMap<ValueId, Interval>> {
        let mut intervals = FxHashMap::default();
        for _ in 0..MAX_PREDECESSOR_EDGES {
            if !budget.spend_preprocess_work(1) {
                return None;
            }
            let Some(&Some(index)) = self.unique_predecessor.get(&block) else {
                break;
            };
            let pred = &func.blocks[index];
            if let Terminator::BranchBool {
                cond, then_target, ..
            } = &pred.terminator
                && let Some(&(op, a, b)) = self.integer_comparisons.get(cond)
                && let Some((value, interval)) =
                    self.interval_on_branch(op, a, b, then_target.block == block)
            {
                let current = intervals.entry(value).or_insert(self.domain);
                *current = current.intersect(interval)?;
            }
            block = pred.id;
        }
        Some(intervals)
    }

    fn interval_on_branch(
        &self,
        mut op: CmpOp,
        a: ValueId,
        b: ValueId,
        condition_is_true: bool,
    ) -> Option<(ValueId, Interval)> {
        let (value, bound) = if let Some(&n) = self.fixnum_constants.get(&b) {
            (a, n)
        } else {
            op = match op {
                CmpOp::Lt => CmpOp::Gt,
                CmpOp::Le => CmpOp::Ge,
                CmpOp::Gt => CmpOp::Lt,
                CmpOp::Ge => CmpOp::Le,
                other => other,
            };
            (b, *self.fixnum_constants.get(&a)?)
        };
        let interval = match (op, condition_is_true) {
            (CmpOp::Lt, true) | (CmpOp::Ge, false) => Interval {
                high: bound - 1,
                ..self.domain
            },
            (CmpOp::Le, true) | (CmpOp::Gt, false) => Interval {
                high: bound,
                ..self.domain
            },
            (CmpOp::Gt, true) | (CmpOp::Le, false) => Interval {
                low: bound + 1,
                ..self.domain
            },
            (CmpOp::Ge, true) | (CmpOp::Lt, false) => Interval {
                low: bound,
                ..self.domain
            },
            (CmpOp::Eq, true) | (CmpOp::Ne, false) => Interval::constant(bound),
            (CmpOp::Ne, true) | (CmpOp::Eq, false) => return None,
        };
        Some((value, interval))
    }
}

struct BlockRanges<'a> {
    domain: Interval,
    block: &'a BasicBlock,
    fixnum_constants: &'a FxHashMap<ValueId, i64>,
    intervals: FxHashMap<ValueId, Interval>,
    non_overflowing_defs: FxHashMap<ValueId, usize>,
}

impl<'a> BlockRanges<'a> {
    fn infer(
        func: &'a SsaFunction,
        block: &'a BasicBlock,
        ssa: &'a SsaIndex,
        budget: &mut SearchBudget,
    ) -> Option<Self> {
        if !budget.spend_preprocess_work(block.insts.len().saturating_add(1)) {
            return None;
        }
        let mut ranges = Self {
            domain: ssa.domain,
            block,
            fixnum_constants: &ssa.fixnum_constants,
            intervals: ssa.intervals_at_entry(func, block.id, budget)?,
            non_overflowing_defs: FxHashMap::default(),
        };
        for (index, inst) in block.insts.iter().enumerate() {
            if let Some(id) = inst.id
                && let Some((op, a, b)) = match_fixnum_arithmetic(inst)
                && let Some(interval) =
                    ranges
                        .interval(a)
                        .fixnum_result(op, ranges.interval(b), ranges.domain)
            {
                ranges.intervals.insert(id, interval);
                ranges.non_overflowing_defs.insert(id, index);
            }
        }
        Some(ranges)
    }

    fn interval(&self, value: ValueId) -> Interval {
        // All inputs to these SSA operations are unboxed fixnums. The output of
        // a checked instruction is a fixnum on its successful continuation.
        if let Some(&n) = self.fixnum_constants.get(&value) {
            Interval::constant(n)
        } else {
            self.intervals.get(&value).copied().unwrap_or(self.domain)
        }
    }

    fn check_intermediate_ranges(&self, candidate: &RecExpr<IntegerExpr>) -> Option<()> {
        let mut intervals: Vec<Interval> = Vec::with_capacity(candidate.as_ref().len());
        for node in candidate.as_ref() {
            let interval = match *node {
                IntegerExpr::Input(value) => self.interval(ValueId(value)),
                IntegerExpr::Add([a, b]) => intervals[usize::from(a)].fixnum_result(
                    Arithmetic::Add,
                    intervals[usize::from(b)],
                    self.domain,
                )?,
                IntegerExpr::Mul([a, b]) => intervals[usize::from(a)].fixnum_result(
                    Arithmetic::Mul,
                    intervals[usize::from(b)],
                    self.domain,
                )?,
            };
            intervals.push(interval);
        }
        Some(())
    }
}

#[derive(Default)]
struct ExpressionDag {
    expression: RecExpr<IntegerExpr>,
    node_by_value: FxHashMap<ValueId, Id>,
    instruction_indices: Vec<usize>,
}

impl ExpressionDag {
    fn import_value(
        &mut self,
        value: ValueId,
        ranges: &BlockRanges<'_>,
        nodes_left: &mut usize,
        budget: &mut SearchBudget,
    ) -> Option<Id> {
        if let Some(&id) = self.node_by_value.get(&value) {
            return Some(id);
        }
        if !budget.spend_preprocess_work(1) {
            return None;
        }
        *nodes_left = nodes_left.checked_sub(1)?;
        let node = if let Some(&index) = ranges.non_overflowing_defs.get(&value) {
            let (op, a, b) = match_fixnum_arithmetic(&ranges.block.insts[index]).unwrap();
            let children = [
                self.import_value(a, ranges, nodes_left, budget)?,
                self.import_value(b, ranges, nodes_left, budget)?,
            ];
            self.instruction_indices.push(index);
            match op {
                Arithmetic::Add => IntegerExpr::Add(children),
                Arithmetic::Mul => IntegerExpr::Mul(children),
            }
        } else {
            IntegerExpr::Input(value.0)
        };
        let id = self.expression.add(node);
        self.node_by_value.insert(value, id);
        Some(id)
    }

    fn has_common_factor(&self) -> bool {
        let mut factors = [false; MAX_INPUT_NODES];
        self.expression.as_ref().iter().any(|node| {
            if let IntegerExpr::Mul([a, b]) = *node {
                let (a, b) = (usize::from(a), usize::from(b));
                let shared = factors[a] || factors[b];
                factors[a] = true;
                factors[b] = true;
                shared
            } else {
                false
            }
        })
    }

    fn intermediates_stay_in_dag(
        &self,
        block: &BasicBlock,
        use_counts: &FxHashMap<ValueId, usize>,
        root: ValueId,
        budget: &mut SearchBudget,
    ) -> bool {
        let mut internal_uses = [0usize; MAX_INPUT_NODES];
        for &index in &self.instruction_indices {
            if !budget.spend_preprocess_work(1) {
                return false;
            }
            block.insts[index].visit_inputs(|value| {
                if let Some(&id) = self.node_by_value.get(&value) {
                    internal_uses[usize::from(id)] += 1;
                }
            });
        }
        self.instruction_indices.iter().all(|&index| {
            if !budget.spend_preprocess_work(1) {
                return false;
            }
            let value = block.insts[index].id.unwrap();
            value == root
                || use_counts.get(&value).copied().unwrap_or(0)
                    == internal_uses[usize::from(self.node_by_value[&value])]
        })
    }
}

struct ExpressionRewrite {
    root: usize,
    removed: Vec<usize>,
    expression: RecExpr<IntegerExpr>,
}

fn find_block_rewrite(
    ranges: &BlockRanges<'_>,
    use_counts: &FxHashMap<ValueId, usize>,
    searches_left: &mut usize,
    budget: &mut SearchBudget,
) -> Option<ExpressionRewrite> {
    for (index, inst) in ranges.block.insts.iter().enumerate().rev() {
        if !budget.spend_preprocess_work(1) {
            return None;
        }
        let Some(value) = inst.id else { continue };
        if !matches!(
            inst.op,
            SsaOp::AddI64Checked(..) | SsaOp::AddI64Unchecked(..)
        ) || !ranges.non_overflowing_defs.contains_key(&value)
        {
            continue;
        }
        let mut dag = ExpressionDag::default();
        let mut nodes_left = MAX_INPUT_NODES;
        if dag
            .import_value(value, ranges, &mut nodes_left, budget)
            .is_none()
            || dag.instruction_indices.len() < 3
            || !budget.spend_preprocess_work(dag.expression.as_ref().len())
            || !dag.has_common_factor()
            || !dag.intermediates_stay_in_dag(ranges.block, use_counts, value, budget)
        {
            if budget.stop.is_some() {
                return None;
            }
            continue;
        }
        *searches_left = searches_left.checked_sub(1)?;
        if let Some(candidate) = find_cheaper_expression(&dag, ranges, budget) {
            dag.instruction_indices.sort_unstable();
            return Some(ExpressionRewrite {
                root: index,
                removed: dag.instruction_indices,
                expression: candidate,
            });
        }
        if budget.stop.is_some() {
            return None;
        }
    }
    None
}

impl ExpressionRewrite {
    fn apply(self, func: &mut SsaFunction, block: usize) {
        let root = &func.blocks[block].insts[self.root];
        let root_id = root.id.unwrap();
        let pc = root.pc;
        let mut values: Vec<ValueId> = Vec::with_capacity(self.expression.as_ref().len());
        let mut replacement = Vec::with_capacity(self.removed.len() - 1);
        for (index, node) in self.expression.as_ref().iter().enumerate() {
            let op = match *node {
                IntegerExpr::Input(value) => {
                    values.push(ValueId(value));
                    continue;
                }
                IntegerExpr::Add([a, b]) => {
                    SsaOp::AddI64Unchecked(values[usize::from(a)], values[usize::from(b)])
                }
                IntegerExpr::Mul([a, b]) => {
                    SsaOp::MulI64Unchecked(values[usize::from(a)], values[usize::from(b)])
                }
            };
            let id = if index + 1 == self.expression.as_ref().len() {
                root_id
            } else {
                func.fresh_value()
            };
            values.push(id);
            replacement.push(SsaInst {
                id: Some(id),
                op,
                ty: TypeTag::Fixnum,

                pc,
                deopt: None,
            });
        }
        // The sorted instruction indices end at the root. Compact the intervening
        // instructions once, keeping their order and original metadata.
        let insts = &mut func.blocks[block].insts;
        let mut removed = self.removed.iter().copied().peekable();
        let mut index = self.removed[0];
        insts
            .extract_if(index..self.root, |_| {
                let remove = removed.next_if_eq(&index).is_some();
                index += 1;
                remove
            })
            .for_each(drop);
        let new_root = self.root + 1 - self.removed.len();
        insts.splice(new_root..=new_root, replacement);
    }
}

pub(crate) fn optimize(func: &mut SsaFunction) -> bool {
    optimize_with_budget(func, &mut SearchBudget::new())
}

fn optimize_with_budget(func: &mut SsaFunction, budget: &mut SearchBudget) -> bool {
    let candidates: Vec<usize> = func
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| has_add_and_two_products(block).then_some(index))
        .collect();
    if candidates.is_empty() {
        return false;
    }
    let ssa = SsaIndex::collect(func);
    let mut searches_left = MAX_SEARCHES_PER_FUNCTION;
    let mut replacements = Vec::new();
    for index in candidates {
        if searches_left == 0 || !budget.can_continue() {
            break;
        }
        let block = &func.blocks[index];
        if let Some(ranges) = BlockRanges::infer(func, block, &ssa, budget)
            && let Some(replacement) =
                find_block_rewrite(&ranges, &ssa.use_counts, &mut searches_left, budget)
        {
            replacements.push((index, replacement));
        }
        if budget.stop.is_some() {
            break;
        }
    }
    let changed = !replacements.is_empty();
    for (index, replacement) in replacements {
        replacement.apply(func, index);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ScopeId, ScopeTree};
    use egg::Extractor;

    fn test_budget(
        preprocess_work: usize,
        rewrite_work: usize,
        extraction_work: usize,
    ) -> SearchBudget {
        SearchBudget {
            started: Instant::now(),
            time_limit: Duration::MAX,
            preprocess_work_left: preprocess_work,
            rewrite_work_left: rewrite_work,
            extraction_work_left: extraction_work,
            stop: None,
        }
    }

    fn constant_factor_function() -> SsaFunction {
        let v = ValueId;
        let ops = [
            (Some(v(0)), SsaOp::ConstI64(2)),
            (Some(v(1)), SsaOp::ConstI64(3)),
            (Some(v(2)), SsaOp::ConstI64(5)),
            (Some(v(3)), SsaOp::MulI64Unchecked(v(0), v(1))),
            (Some(v(4)), SsaOp::MulI64Unchecked(v(0), v(2))),
            (Some(v(5)), SsaOp::AddI64Unchecked(v(3), v(4))),
            (Some(v(6)), SsaOp::BoxFixnum(v(5))),
        ];
        SsaFunction {
            fixnum_bounds: (crate::FIXNUM_MIN, crate::FIXNUM_MAX),
            arity: zkcl_bytecode::Arity::ZERO,
            constant_count: 0,
            entry: BlockId(0),
            entry_params: vec![],
            blocks: vec![BasicBlock {
                id: BlockId(0),
                params: vec![],
                insts: ops
                    .into_iter()
                    .enumerate()
                    .map(|(pc, (id, op))| SsaInst {
                        id,

                        op,
                        ty: TypeTag::Fixnum,
                        pc: Some(pc),
                        deopt: None,
                    })
                    .collect(),
                terminator: Terminator::ReturnOne(v(6)),
                terminator_pc: 0,
                entry_scope: ScopeId::ROOT,
                unwind_succs: vec![],
            }],
            scopes: ScopeTree::new(),
            pc_to_block: FxHashMap::default(),
            next_value_id: 7,
        }
    }

    fn rejected_factor_candidates(groups: u32) -> SsaFunction {
        let mut insts = Vec::with_capacity(groups as usize * 7);
        for group in 0..groups {
            let base = group * 7;
            let constants = [2, 3, 5, 7];
            for (offset, value) in constants.into_iter().enumerate() {
                insts.push(SsaInst {
                    id: Some(ValueId(base + offset as u32)),
                    op: SsaOp::ConstI64(value),

                    ty: TypeTag::Fixnum,
                    pc: Some(insts.len()),
                    deopt: None,
                });
            }
            insts.push(SsaInst {
                id: Some(ValueId(base + 4)),
                op: SsaOp::MulI64Unchecked(ValueId(base), ValueId(base + 1)),

                ty: TypeTag::Fixnum,
                pc: Some(insts.len()),
                deopt: None,
            });
            insts.push(SsaInst {
                id: Some(ValueId(base + 5)),
                op: SsaOp::MulI64Unchecked(ValueId(base + 2), ValueId(base + 3)),

                ty: TypeTag::Fixnum,
                pc: Some(insts.len()),
                deopt: None,
            });
            insts.push(SsaInst {
                id: Some(ValueId(base + 6)),
                op: SsaOp::AddI64Unchecked(ValueId(base + 4), ValueId(base + 5)),

                ty: TypeTag::Fixnum,
                pc: Some(insts.len()),
                deopt: None,
            });
        }
        SsaFunction {
            fixnum_bounds: (crate::FIXNUM_MIN, crate::FIXNUM_MAX),
            arity: zkcl_bytecode::Arity::ZERO,
            constant_count: 0,
            entry: BlockId(0),
            entry_params: vec![],
            blocks: vec![BasicBlock {
                id: BlockId(0),
                params: vec![],
                insts,
                terminator: Terminator::ReturnDynamicValues,
                terminator_pc: 0,
                entry_scope: ScopeId::ROOT,
                unwind_succs: vec![],
            }],
            scopes: ScopeTree::new(),
            pc_to_block: FxHashMap::default(),
            next_value_id: groups * 7,
        }
    }

    fn saturated_test_graph() -> (EGraph<IntegerExpr, ()>, Id) {
        let mut graph = EGraph::default();
        let root = graph.add_expr(&"(+ (* 0 2) (* 0 3))".parse().unwrap());
        graph.rebuild();
        for _ in 0..3 {
            for rule in RULES.iter() {
                let matches = rule.search(&graph);
                if !matches.is_empty() {
                    rule.apply(&mut graph, &matches);
                    graph.rebuild();
                }
            }
        }
        (graph, root)
    }

    #[test]
    fn bounded_extractor_matches_egg_cost_and_selected_expression() {
        let (graph, root) = saturated_test_graph();
        let (egg_cost, egg_expression) = Extractor::new(&graph, ArithmeticTreeCost).find_best(root);
        let mut budget = test_budget(
            MAX_PREPROCESS_WORK_PER_FUNCTION,
            MAX_REWRITE_WORK_PER_FUNCTION,
            8_192,
        );
        let (cost, expression) = extract_best(&graph, root, &mut budget).unwrap();
        assert_eq!(cost, egg_cost);
        assert_eq!(expression, egg_expression);
    }

    #[test]
    fn zero_candidate_budget_leaves_ssa_unchanged() {
        let mut func = constant_factor_function();
        let before = func.clone();
        let mut budget = test_budget(0, MAX_REWRITE_WORK_PER_FUNCTION, 8_192);
        assert!(!optimize_with_budget(&mut func, &mut budget));
        assert_eq!(budget.stop, Some(BudgetStop::PreprocessWork));
        assert_eq!(func, before);
    }

    #[test]
    fn default_work_budget_keeps_a_known_profitable_candidate() {
        let mut func = constant_factor_function();
        let mut budget = test_budget(
            MAX_PREPROCESS_WORK_PER_FUNCTION,
            MAX_REWRITE_WORK_PER_FUNCTION,
            8_192,
        );
        assert!(optimize_with_budget(&mut func, &mut budget));
        assert!(budget.stop.is_none());
        assert_eq!(
            func.blocks[0]
                .insts
                .iter()
                .filter(|inst| {
                    matches!(
                        inst.op,
                        SsaOp::AddI64Checked(..)
                            | SsaOp::AddI64Unchecked(..)
                            | SsaOp::MulI64Checked(..)
                            | SsaOp::MulI64Unchecked(..)
                    )
                })
                .count(),
            2
        );
    }

    #[test]
    fn rewrite_budget_exhaustion_leaves_ssa_unchanged() {
        let mut func = constant_factor_function();
        let before = func.clone();
        let mut budget = test_budget(
            MAX_PREPROCESS_WORK_PER_FUNCTION,
            0,
            MAX_EXTRACTION_WORK_PER_FUNCTION,
        );
        assert!(!optimize_with_budget(&mut func, &mut budget));
        assert_eq!(budget.stop, Some(BudgetStop::RewriteWork));
        assert_eq!(func, before);
    }

    #[test]
    fn expiry_checkpoint_leaves_ssa_unchanged() {
        let mut func = constant_factor_function();
        let before = func.clone();
        let mut budget = test_budget(
            MAX_PREPROCESS_WORK_PER_FUNCTION,
            MAX_REWRITE_WORK_PER_FUNCTION,
            MAX_EXTRACTION_WORK_PER_FUNCTION,
        );
        budget.time_limit = Duration::ZERO;
        assert!(!optimize_with_budget(&mut func, &mut budget));
        assert_eq!(budget.stop, Some(BudgetStop::Time));
        assert_eq!(func, before);
    }

    #[test]
    fn exhausted_extraction_budget_returns_no_candidate() {
        let (graph, root) = saturated_test_graph();
        let mut budget = test_budget(
            MAX_PREPROCESS_WORK_PER_FUNCTION,
            MAX_REWRITE_WORK_PER_FUNCTION,
            0,
        );
        assert!(extract_best(&graph, root, &mut budget).is_none());
        assert_eq!(budget.stop, Some(BudgetStop::ExtractionWork));
    }

    #[test]
    fn rejected_candidates_share_one_function_work_bound() {
        let mut func = rejected_factor_candidates(512);
        let before = func.clone();
        let mut budget = test_budget(
            MAX_PREPROCESS_WORK_PER_FUNCTION,
            MAX_REWRITE_WORK_PER_FUNCTION,
            8_192,
        );
        assert!(!optimize_with_budget(&mut func, &mut budget));
        assert_eq!(budget.stop, Some(BudgetStop::PreprocessWork));
        assert_eq!(func, before);
    }

    #[test]
    fn factoring_preserves_ssa_order_across_safepoints() {
        let v = ValueId;
        let ops = [
            (Some(v(0)), SsaOp::ConstI64(2)),
            (Some(v(1)), SsaOp::ConstI64(3)),
            (Some(v(2)), SsaOp::ConstI64(5)),
            (Some(v(3)), SsaOp::MulI64Unchecked(v(0), v(1))),
            (None, SsaOp::SaveValues),
            (Some(v(4)), SsaOp::MulI64Unchecked(v(0), v(2))),
            (None, SsaOp::RestoreValues),
            (Some(v(5)), SsaOp::AddI64Unchecked(v(3), v(4))),
            (Some(v(6)), SsaOp::BoxFixnum(v(5))),
        ];
        let insts = ops
            .into_iter()
            .enumerate()
            .map(|(pc, (id, op))| SsaInst {
                id,

                op,
                ty: TypeTag::Fixnum,
                pc: Some(pc),
                deopt: None,
            })
            .collect();
        let mut func = SsaFunction {
            fixnum_bounds: (crate::FIXNUM_MIN, crate::FIXNUM_MAX),
            arity: zkcl_bytecode::Arity::ZERO,
            constant_count: 0,
            entry: BlockId(0),
            entry_params: vec![],
            blocks: vec![BasicBlock {
                id: BlockId(0),
                params: vec![],
                insts,
                terminator: Terminator::ReturnOne(v(6)),
                terminator_pc: 0,
                entry_scope: ScopeId::ROOT,
                unwind_succs: vec![],
            }],
            scopes: ScopeTree::new(),
            pc_to_block: FxHashMap::default(),
            next_value_id: 7,
        };
        ExpressionRewrite {
            root: 7,
            removed: vec![3, 5, 7],
            expression: "(* 0 (+ 1 2))".parse().unwrap(),
        }
        .apply(&mut func, 0);
        let insts = &func.blocks[0].insts;
        assert_eq!(
            insts
                .iter()
                .map(|inst| inst.pc.unwrap())
                .collect::<Vec<_>>(),
            [0, 1, 2, 4, 6, 7, 7, 8]
        );
        assert!(matches!(insts[3].op, SsaOp::SaveValues));
        assert!(matches!(insts[4].op, SsaOp::RestoreValues));
        assert_eq!(
            (insts[6].id, &insts[6].op),
            (Some(v(5)), &SsaOp::MulI64Unchecked(v(0), v(7)))
        );
    }

    #[test]
    fn factoring_may_introduce_an_out_of_range_intermediate() {
        let v = ValueId;
        let ops = [
            SsaOp::MulI64Unchecked(v(0), v(1)),
            SsaOp::MulI64Unchecked(v(0), v(2)),
            SsaOp::AddI64Unchecked(v(3), v(4)),
        ];
        let block = BasicBlock {
            id: BlockId(0),
            params: Vec::new(),
            insts: ops
                .into_iter()
                .enumerate()
                .map(|(index, op)| SsaInst {
                    id: Some(v(index as u32 + 3)),

                    op,
                    ty: TypeTag::Fixnum,
                    pc: Some(index),
                    deopt: None,
                })
                .collect(),
            terminator: Terminator::ReturnDynamicValues,
            terminator_pc: 0,
            entry_scope: ScopeId::ROOT,
            unwind_succs: Vec::new(),
        };
        let fixnum_constants = FxHashMap::default();
        let ranges = BlockRanges {
            domain: Interval {
                low: crate::FIXNUM_MIN,
                high: crate::FIXNUM_MAX,
            },
            block: &block,
            fixnum_constants: &fixnum_constants,
            intervals: [(ValueId(0), Interval::constant(0))].into_iter().collect(),
            non_overflowing_defs: [(v(3), 0), (v(4), 1), (v(5), 2)].into_iter().collect(),
        };
        let original = "(+ (* 0 1) (* 0 2))".parse().unwrap();
        let factored = "(* 0 (+ 1 2))".parse().unwrap();
        assert!(ranges.check_intermediate_ranges(&original).is_some());
        assert!(ranges.check_intermediate_ranges(&factored).is_none());
        let mut dag = ExpressionDag::default();
        let mut nodes_left = MAX_INPUT_NODES;
        let mut budget = test_budget(
            MAX_PREPROCESS_WORK_PER_FUNCTION,
            MAX_REWRITE_WORK_PER_FUNCTION,
            8_192,
        );
        dag.import_value(v(5), &ranges, &mut nodes_left, &mut budget)
            .unwrap();
        assert!(find_cheaper_expression(&dag, &ranges, &mut budget).is_none());
    }
}
