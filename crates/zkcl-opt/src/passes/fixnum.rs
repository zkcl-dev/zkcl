//! Fixnum proofs and representation changes across block boundaries.

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::Immediate;

use super::cfg::incoming_branches;
use super::dom::DomTree;
use super::rewrite::{resolve_rewrite_chains, rewrite_all_uses, rewrite_deopt_register_value};
use crate::ir::{
    BlockId, CmpOp, DeoptRegisterValue, InstBuilder, SsaFunction, SsaInst, SsaOp, Terminator,
    TypeTag, ValueId, ValueRepr,
};

/// Promote `TaggedWord` block parameters (e.g. loop-carried fixnum induction variables and
/// accumulators) to `ValueRepr::UnboxedI64` when all incoming edges provide fixnum values and at
/// least one `UnboxFixnumGuard` consumes the parameter.
pub(super) fn unbox_loop_params(func: &mut SsaFunction) -> bool {
    if !func
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .any(|inst| matches!(inst.op, SsaOp::UnboxFixnumGuard(_)))
    {
        return false;
    }
    let mut changed = false;

    loop {
        let mut unwind_targets: FxHashSet<BlockId> = FxHashSet::default();
        for b in &func.blocks {
            for &u in &b.unwind_succs {
                unwind_targets.insert(u);
            }
        }

        let mut const_fixnums: FxHashMap<ValueId, i64> = FxHashMap::default();
        let mut box_sources: FxHashMap<ValueId, ValueId> = FxHashMap::default();
        let mut unbox_guarded: FxHashSet<ValueId> = FxHashSet::default();
        let mut param_reprs: FxHashMap<ValueId, ValueRepr> = FxHashMap::default();

        for b in &func.blocks {
            for p in &b.params {
                param_reprs.insert(p.value, p.repr);
            }
            for inst in &b.insts {
                if let Some(id) = inst.id {
                    match inst.op {
                        SsaOp::ConstWord(Immediate::Fixnum(n)) => {
                            const_fixnums.insert(id, n);
                        }
                        SsaOp::BoxFixnum(u) => {
                            box_sources.insert(id, u);
                        }
                        _ => {}
                    }
                }
                if let SsaOp::UnboxFixnumGuard(src) = inst.op {
                    unbox_guarded.insert(src);
                }
            }
        }

        let incoming = incoming_branches(func);

        let mut candidate: Option<(BlockId, usize, ValueId)> = None;
        'outer: for b in &func.blocks {
            if b.id == func.entry || unwind_targets.contains(&b.id) {
                continue;
            }
            let Some(edges) = incoming.get(&b.id) else {
                continue;
            };
            for (pidx, p) in b.params.iter().enumerate() {
                if p.repr != ValueRepr::TaggedWord || !unbox_guarded.contains(&p.value) {
                    continue;
                }
                let ok = edges.iter().all(|edge| {
                    let Some(&arg) = edge.args.get(pidx) else {
                        return false;
                    };
                    arg == p.value
                        || box_sources.contains_key(&arg)
                        || const_fixnums.contains_key(&arg)
                        || param_reprs.get(&arg) == Some(&ValueRepr::UnboxedI64)
                });
                if ok {
                    candidate = Some((b.id, pidx, p.value));
                    break 'outer;
                }
            }
        }

        let Some((target_bid, param_idx, param_val)) = candidate else {
            break;
        };
        changed = true;

        // 1. Flip BlockParam repr to UnboxedI64.
        let target_block = func.block_mut(target_bid).unwrap();
        target_block.params[param_idx].repr = ValueRepr::UnboxedI64;
        target_block.params[param_idx].ty = TypeTag::Fixnum;

        // 2. Rewrite incoming edge arguments to pass unboxed i64 values.
        let mut pending_const_inserts: FxHashMap<(BlockId, i64), ValueId> = FxHashMap::default();
        let num_blocks = func.blocks.len();
        for bi in 0..num_blocks {
            let pred_id = func.blocks[bi].id;
            let mut new_consts: Vec<(i64, ValueId)> = Vec::new();
            let mut targets_to_update = Vec::new();
            func.blocks[bi].terminator.visit_targets(|t| {
                if t.block == target_bid {
                    targets_to_update.push(t.args[param_idx]);
                }
            });
            if targets_to_update.is_empty() {
                continue;
            }
            for arg in &targets_to_update {
                if let Some(&n) = const_fixnums.get(arg) {
                    pending_const_inserts
                        .entry((pred_id, n))
                        .or_insert_with(|| {
                            let cid = func.fresh_value();
                            new_consts.push((n, cid));
                            cid
                        });
                }
            }
            for (n, cid) in new_consts {
                func.blocks[bi].insts.push(SsaInst {
                    id: Some(cid),
                    op: SsaOp::ConstI64(n),
                    ty: TypeTag::Fixnum,

                    pc: None,
                    deopt: None,
                });
            }
            func.blocks[bi].terminator.visit_targets_mut(|t| {
                if t.block == target_bid {
                    let arg = t.args[param_idx];
                    if let Some(&u) = box_sources.get(&arg) {
                        t.args[param_idx] = u;
                    } else if let Some(&n) = const_fixnums.get(&arg) {
                        t.args[param_idx] = pending_const_inserts[&(pred_id, n)];
                    }
                }
            });
        }

        // 3. Remove `UnboxFixnumGuard(param_val)` and replace its result with `param_val`.
        let mut guard_rewrites: FxHashMap<ValueId, ValueId> = FxHashMap::default();
        for b in &mut func.blocks {
            b.insts.retain(|inst| {
                if let SsaOp::UnboxFixnumGuard(src) = inst.op
                    && src == param_val
                    && let Some(id) = inst.id
                {
                    guard_rewrites.insert(id, param_val);
                    return false;
                }
                true
            });
        }
        if !guard_rewrites.is_empty() {
            rewrite_all_uses(func, &guard_rewrites);
        }

        // 4. Update `DeoptState` references `TaggedWord(param_val)` -> `UnboxedI64(param_val)`.
        for b in &mut func.blocks {
            for inst in &mut b.insts {
                if let Some(deopt) = &mut inst.deopt {
                    deopt.rewrite_values(|k| match k {
                        DeoptRegisterValue::TaggedWord(v) if v == param_val => {
                            DeoptRegisterValue::UnboxedI64(param_val)
                        }
                        other => other,
                    });
                }
            }
        }

        // 5. Materialize `BoxFixnum(param_val)` for any remaining boxed uses (e.g. `ReturnOne`,
        //    or calls, or passing to another `TaggedWord` BlockParam).
        let unboxed_param_positions: FxHashMap<BlockId, Vec<bool>> = func
            .blocks
            .iter()
            .map(|b| {
                (
                    b.id,
                    b.params
                        .iter()
                        .map(|p| p.repr == ValueRepr::UnboxedI64)
                        .collect(),
                )
            })
            .collect();

        for bi in 0..func.blocks.len() {
            let mut boxed_in_block = None;
            let mut box_param = |value, func: &mut SsaFunction, insts: &mut Vec<SsaInst>| {
                if value != param_val {
                    return value;
                }
                *boxed_in_block.get_or_insert_with(|| {
                    InstBuilder {
                        next_value_id: &mut func.next_value_id,
                        insts,
                        pc: None,
                    }
                    .value(SsaOp::BoxFixnum(param_val), TypeTag::Fixnum)
                })
            };

            let old_insts = std::mem::take(&mut func.blocks[bi].insts);
            let mut new_insts = Vec::with_capacity(old_insts.len());
            for mut inst in old_insts {
                if !op_inputs_are_unboxed_i64(&inst.op) {
                    inst.op
                        .rewrite_inputs(|v| box_param(v, func, &mut new_insts));
                }
                new_insts.push(inst);
            }

            let mut term = std::mem::replace(
                &mut func.blocks[bi].terminator,
                Terminator::ReturnDynamicValues,
            );
            // Only BranchBool consumes a raw predicate. The other direct operands
            // (truthiness, return values and non-local transfer tokens) are tagged.
            if !matches!(term, Terminator::BranchBool { .. }) {
                for value in term.direct_inputs_mut() {
                    *value = box_param(*value, func, &mut new_insts);
                }
            }
            term.visit_targets_mut(|target| {
                if let Some(flags) = unboxed_param_positions.get(&target.block) {
                    for (index, arg) in target.args.iter_mut().enumerate() {
                        if !flags.get(index).copied().unwrap_or(false) {
                            *arg = box_param(*arg, func, &mut new_insts);
                        }
                    }
                }
            });

            func.blocks[bi].insts = new_insts;
            func.blocks[bi].terminator = term;
        }
    }

    changed
}

fn op_inputs_are_unboxed_i64(op: &SsaOp) -> bool {
    matches!(
        op,
        SsaOp::BoxFixnum(_)
            | SsaOp::AddI64Checked(_, _)
            | SsaOp::SubI64Checked(_, _)
            | SsaOp::MulI64Checked(_, _)
            | SsaOp::AddI64Unchecked(_, _)
            | SsaOp::SubI64Unchecked(_, _)
            | SsaOp::MulI64Unchecked(_, _)
            | SsaOp::CmpI64 { .. }
    )
}

/// Hoist loop-invariant `UnboxFixnumGuard`s from loop-header prefixes into unique preheaders.
pub(super) fn hoist_loop_guards(func: &mut SsaFunction) -> bool {
    if func.blocks.len() < 2
        || !func
            .blocks
            .iter()
            .flat_map(|block| &block.insts)
            .any(|inst| matches!(inst.op, SsaOp::UnboxFixnumGuard(_)))
    {
        return false;
    }
    let block_map = func.block_index_map();
    let dom = DomTree::compute(func, &block_map);

    let mut unwind_targets: FxHashSet<BlockId> = FxHashSet::default();
    let mut cfg_preds: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
    for b in &func.blocks {
        for &u in &b.unwind_succs {
            unwind_targets.insert(u);
        }
        b.terminator.visit_targets(|t| {
            cfg_preds.entry(t.block).or_default().push(b.id);
        });
    }

    let mut def_block: FxHashMap<ValueId, BlockId> = FxHashMap::default();
    for b in &func.blocks {
        for p in &b.params {
            def_block.insert(p.value, b.id);
        }
        for inst in &b.insts {
            if let Some(id) = inst.id {
                def_block.insert(id, b.id);
            }
        }
    }
    let reprs: FxHashMap<ValueId, ValueRepr> = func
        .value_metadata()
        .into_iter()
        .map(|(v, (r, _))| (v, r))
        .collect();

    let mut changed = false;
    for &h_bid in dom.rpo() {
        if h_bid == func.entry || unwind_targets.contains(&h_bid) {
            continue;
        }
        let Some(preds) = cfg_preds.get(&h_bid) else {
            continue;
        };
        let mut backedges = 0usize;
        let mut outside = Vec::new();
        for &p in preds {
            if dom.dominates(h_bid, p) {
                backedges += 1;
            } else {
                outside.push(p);
            }
        }
        if backedges == 0 || outside.len() != 1 {
            continue;
        }
        let pre_bid = outside[0];
        let pre_idx = block_map[&pre_bid];
        let h_idx = block_map[&h_bid];
        let Terminator::Jump(ref pre_target) = func.blocks[pre_idx].terminator else {
            continue;
        };
        if pre_target.block != h_bid {
            continue;
        }
        let pre_args = pre_target.args.clone();
        let h_param_map: FxHashMap<ValueId, ValueId> = func.blocks[h_idx]
            .params
            .iter()
            .zip(pre_args.iter())
            .map(|(p, &a)| (p.value, a))
            .collect();

        let is_pre_avail = |v: ValueId| -> bool {
            h_param_map.contains_key(&v)
                || def_block
                    .get(&v)
                    .is_some_and(|&db| db != h_bid && dom.dominates(db, pre_bid))
        };

        let mut hoisted = Vec::new();
        let mut keep = Vec::with_capacity(func.blocks[h_idx].insts.len());
        let mut prefix_open = true;
        let old_insts = std::mem::take(&mut func.blocks[h_idx].insts);
        for mut inst in old_insts {
            if prefix_open
                && let SsaOp::UnboxFixnumGuard(src) = inst.op
                && def_block
                    .get(&src)
                    .is_some_and(|&db| db != h_bid && dom.dominates(db, pre_bid))
                && let Some(deopt) = inst.deopt.as_ref()
            {
                let mut deopt_ok = true;
                deopt.visit_values(|v| {
                    if !is_pre_avail(v) {
                        deopt_ok = false;
                    }
                });
                if deopt_ok {
                    let mut remapped = deopt.clone();
                    remapped.rewrite_values(|kind| match kind {
                        DeoptRegisterValue::TaggedWord(v) => {
                            let mapped = h_param_map.get(&v).copied().unwrap_or(v);
                            if reprs.get(&mapped) == Some(&ValueRepr::UnboxedI64) {
                                DeoptRegisterValue::UnboxedI64(mapped)
                            } else {
                                DeoptRegisterValue::TaggedWord(mapped)
                            }
                        }
                        DeoptRegisterValue::UnboxedI64(v) => {
                            let mapped = h_param_map.get(&v).copied().unwrap_or(v);
                            DeoptRegisterValue::UnboxedI64(mapped)
                        }
                        other => other,
                    });
                    inst.deopt = Some(remapped);
                    hoisted.push(inst);
                    changed = true;
                    continue;
                }
            }
            if !inst.op.effects().is_pure() && !matches!(inst.op, SsaOp::UnboxFixnumGuard(_)) {
                prefix_open = false;
            }
            keep.push(inst);
        }
        func.blocks[h_idx].insts = keep;
        if !hoisted.is_empty() {
            for h_inst in hoisted {
                if let Some(id) = h_inst.id {
                    def_block.insert(id, pre_bid);
                }
                func.blocks[pre_idx].insts.push(h_inst);
            }
        }
    }

    changed
}

/// Dominator-based CSE for `UnboxFixnumGuard` / `BoxFixnum` and range-proven overflow elimination.
pub(super) fn eliminate_redundant_fixnum_checks(func: &mut SsaFunction) -> bool {
    if !func
        .blocks
        .iter()
        .flat_map(|block| &block.insts)
        .any(|inst| {
            matches!(
                inst.op,
                SsaOp::UnboxFixnumGuard(_)
                    | SsaOp::BoxFixnum(_)
                    | SsaOp::AddI64Checked(_, _)
                    | SsaOp::SubI64Checked(_, _)
            )
        })
    {
        return false;
    }
    let block_map = func.block_index_map();
    let dom = DomTree::compute(func, &block_map);

    let mut dom_children: FxHashMap<BlockId, Vec<BlockId>> = FxHashMap::default();
    let mut pred_counts: FxHashMap<BlockId, usize> = FxHashMap::default();
    for &bid in dom.rpo() {
        let children = dom.children(bid);
        if !children.is_empty() {
            dom_children.insert(bid, children.to_vec());
        }
        let b = &func.blocks[block_map[&bid]];
        b.terminator.visit_targets(|t| {
            *pred_counts.entry(t.block).or_default() += 1;
        });
    }

    let mut const_i64s: FxHashMap<ValueId, i64> = FxHashMap::default();
    let mut cmp_defs: FxHashMap<ValueId, (CmpOp, ValueId, ValueId)> = FxHashMap::default();
    for b in &func.blocks {
        for inst in &b.insts {
            if let Some(id) = inst.id {
                match inst.op {
                    SsaOp::ConstI64(n) => {
                        const_i64s.insert(id, n);
                    }
                    SsaOp::CmpI64 { op, lhs, rhs } => {
                        cmp_defs.insert(id, (op, lhs, rhs));
                    }
                    _ => {}
                }
            }
        }
    }

    struct FixnumCheckElimination<'a> {
        func: &'a mut SsaFunction,
        block_map: &'a FxHashMap<BlockId, usize>,
        dom_children: &'a FxHashMap<BlockId, Vec<BlockId>>,
        pred_counts: &'a FxHashMap<BlockId, usize>,
        const_i64s: &'a FxHashMap<ValueId, i64>,
        cmp_defs: &'a FxHashMap<ValueId, (CmpOp, ValueId, ValueId)>,
        rewrites: FxHashMap<ValueId, ValueId>,
        changed: bool,
    }

    fn walk(
        bid: BlockId,
        state: &mut FixnumCheckElimination<'_>,
        mut unboxed_map: FxHashMap<ValueId, ValueId>,
        mut boxed_map: FxHashMap<ValueId, ValueId>,
        lt_bounded: FxHashSet<ValueId>,
        gt_bounded: FxHashSet<ValueId>,
    ) {
        let bidx = state.block_map[&bid];
        let old_insts = std::mem::take(&mut state.func.blocks[bidx].insts);
        let mut new_insts = Vec::with_capacity(old_insts.len());

        for mut inst in old_insts {
            inst.op
                .rewrite_inputs(|v| *state.rewrites.get(&v).unwrap_or(&v));
            if let Some(deopt) = &mut inst.deopt {
                deopt.rewrite_values(|k| rewrite_deopt_register_value(k, &state.rewrites));
            }
            if let Some(id) = inst.id {
                match inst.op {
                    SsaOp::UnboxFixnumGuard(src) => {
                        if let Some(&existing) = unboxed_map.get(&src) {
                            state.rewrites.insert(id, existing);
                            state.changed = true;
                            continue;
                        }
                        unboxed_map.insert(src, id);
                        boxed_map.entry(id).or_insert(src);
                    }
                    SsaOp::BoxFixnum(u) => {
                        if let Some(&existing) = boxed_map.get(&u) {
                            state.rewrites.insert(id, existing);
                            state.changed = true;
                            continue;
                        }
                        boxed_map.insert(u, id);
                        unboxed_map.entry(id).or_insert(u);
                    }
                    SsaOp::AddI64Checked(a, b) => {
                        let a_lt = lt_bounded.contains(&a) && state.const_i64s.get(&b) == Some(&1);
                        let b_lt = lt_bounded.contains(&b) && state.const_i64s.get(&a) == Some(&1);
                        if a_lt || b_lt {
                            inst.op = SsaOp::AddI64Unchecked(a, b);

                            inst.deopt = None;
                            state.changed = true;
                        }
                    }
                    SsaOp::SubI64Checked(a, b)
                        if gt_bounded.contains(&a) && state.const_i64s.get(&b) == Some(&1) =>
                    {
                        inst.op = SsaOp::SubI64Unchecked(a, b);

                        inst.deopt = None;
                        state.changed = true;
                    }
                    _ => {}
                }
            }
            new_insts.push(inst);
        }

        state.func.blocks[bidx].insts = new_insts;
        state.func.blocks[bidx]
            .terminator
            .rewrite_inputs(|v| *state.rewrites.get(&v).unwrap_or(&v));

        let term_branch = if let Terminator::BranchBool {
            cond,
            then_target,
            else_target,
        } = &state.func.blocks[bidx].terminator
        {
            state
                .cmp_defs
                .get(cond)
                .map(|&(op, lhs, rhs)| (op, lhs, rhs, then_target.block, else_target.block))
        } else {
            None
        };

        let children = state.dom_children.get(&bid).cloned().unwrap_or_default();
        for child in children {
            let mut c_lt = lt_bounded.clone();
            let mut c_gt = gt_bounded.clone();
            if state.pred_counts.get(&child).copied().unwrap_or(0) == 1
                && let Some((op, lhs, rhs, then_b, else_b)) = term_branch
            {
                if child == then_b {
                    match op {
                        CmpOp::Lt => {
                            c_lt.insert(lhs);
                            c_gt.insert(rhs);
                        }
                        CmpOp::Gt => {
                            c_gt.insert(lhs);
                            c_lt.insert(rhs);
                        }
                        _ => {}
                    }
                } else if child == else_b {
                    match op {
                        CmpOp::Ge => {
                            c_lt.insert(lhs);
                            c_gt.insert(rhs);
                        }
                        CmpOp::Le => {
                            c_gt.insert(lhs);
                            c_lt.insert(rhs);
                        }
                        _ => {}
                    }
                }
            }
            walk(
                child,
                state,
                unboxed_map.clone(),
                boxed_map.clone(),
                c_lt,
                c_gt,
            );
        }
    }

    let entry_id = func.entry;
    let mut state = FixnumCheckElimination {
        func,
        block_map: &block_map,
        dom_children: &dom_children,
        pred_counts: &pred_counts,
        const_i64s: &const_i64s,
        cmp_defs: &cmp_defs,
        rewrites: FxHashMap::default(),
        changed: false,
    };
    walk(
        entry_id,
        &mut state,
        FxHashMap::default(),
        FxHashMap::default(),
        FxHashSet::default(),
        FxHashSet::default(),
    );

    let mut rewrites = state.rewrites;
    let changed = state.changed;
    if !rewrites.is_empty() {
        resolve_rewrite_chains(&mut rewrites);
        rewrite_all_uses(func, &rewrites);
    }
    changed
}
