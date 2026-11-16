//! Dominator-tree scoped Global Value Numbering (GVN) and `OpEffects`-aware
//! Loop-Invariant Code Motion (LICM).

use rustc_hash::FxHashMap;
use zkcl_bytecode::native_abi::I32x4Operation;
use zkcl_bytecode::{AliasDomain, Immediate};

use super::dom::{DomTree, LoopInfo};
use super::rewrite::{resolve_rewrite_chains, rewrite_all_uses, rewrite_deopt_register_value};
use crate::ir::{
    BlockId, CmpOp, DeoptRegisterValue, FloatFormat, SsaFunction, SsaOp, Terminator, ValueId,
    ValueRepr,
};

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
enum ImmKey {
    Nil,
    True,
    Fixnum(i64),
    Char(char),
}

impl From<Immediate> for ImmKey {
    fn from(imm: Immediate) -> Self {
        match imm {
            Immediate::Nil => Self::Nil,
            Immediate::True => Self::True,
            Immediate::Fixnum(n) => Self::Fixnum(n),
            Immediate::Character(c) => Self::Char(c),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
enum GvnKey {
    AddI64(ValueId, ValueId),
    SubI64(ValueId, ValueId),
    MulI64(ValueId, ValueId),
    CmpI64(CmpOp, ValueId, ValueId),
    I32x4Binary(I32x4Operation, ValueId, ValueId),
    IsPresent(ValueId),
    IsNil(ValueId),
    IsFixnum(ValueId),
    IsFixnumInRange(ValueId, i32, i32),
    NotBool(ValueId),
    SelectBool(ValueId, ImmKey, ImmKey),
    UnboxFixnumGuard(ValueId),
    BoxFixnum(ValueId),
}

#[inline]
fn canonical_pair(a: ValueId, b: ValueId) -> (ValueId, ValueId) {
    if a <= b { (a, b) } else { (b, a) }
}

fn gvn_key(op: &SsaOp) -> Option<GvnKey> {
    match *op {
        SsaOp::AddI64Unchecked(a, b) => {
            let (a, b) = canonical_pair(a, b);
            Some(GvnKey::AddI64(a, b))
        }
        SsaOp::SubI64Unchecked(a, b) => Some(GvnKey::SubI64(a, b)),
        SsaOp::MulI64Unchecked(a, b) => {
            let (a, b) = canonical_pair(a, b);
            Some(GvnKey::MulI64(a, b))
        }
        SsaOp::CmpI64 { op, lhs, rhs } => {
            let (lhs, rhs) = match op {
                CmpOp::Eq | CmpOp::Ne => canonical_pair(lhs, rhs),
                _ => (lhs, rhs),
            };
            Some(GvnKey::CmpI64(op, lhs, rhs))
        }
        SsaOp::I32x4Binary { op, lhs, rhs } => {
            let (lhs, rhs) = match op {
                I32x4Operation::Add
                | I32x4Operation::BitAnd
                | I32x4Operation::BitOr
                | I32x4Operation::BitXor => canonical_pair(lhs, rhs),
                I32x4Operation::Sub => (lhs, rhs),
            };
            Some(GvnKey::I32x4Binary(op, lhs, rhs))
        }
        SsaOp::IsPresent(v) => Some(GvnKey::IsPresent(v)),
        SsaOp::IsNil(v) => Some(GvnKey::IsNil(v)),
        SsaOp::IsFixnum(v) => Some(GvnKey::IsFixnum(v)),
        SsaOp::IsFixnumInRange { value, low, high } => Some(GvnKey::IsFixnumInRange(value, low, high)),
        SsaOp::NotBool(v) => Some(GvnKey::NotBool(v)),
        SsaOp::SelectBool {
            cond,
            when_true,
            when_false,
        } => Some(GvnKey::SelectBool(cond, when_true.into(), when_false.into())),
        SsaOp::UnboxFixnumGuard(src) => Some(GvnKey::UnboxFixnumGuard(src)),
        SsaOp::BoxFixnum(u) => Some(GvnKey::BoxFixnum(u)),
        _ => None,
    }
}

struct GvnScope<'a> {
    func: &'a mut SsaFunction,
    block_map: &'a FxHashMap<BlockId, usize>,
    dom: &'a DomTree,
    values: FxHashMap<GvnKey, ValueId>,
    rewrites: FxHashMap<ValueId, ValueId>,
    changed: bool,
}

impl GvnScope<'_> {
    fn visit(&mut self, bid: BlockId) {
        let bidx = self.block_map[&bid];
        let mut undo_values: Vec<(GvnKey, Option<ValueId>)> = Vec::new();

        let old_insts = std::mem::take(&mut self.func.blocks[bidx].insts);
        let mut new_insts = Vec::with_capacity(old_insts.len());

        for mut inst in old_insts {
            inst.op
                .rewrite_inputs(|v| *self.rewrites.get(&v).unwrap_or(&v));
            if let Some(deopt) = &mut inst.deopt {
                deopt.rewrite_values(|k| rewrite_deopt_register_value(k, &self.rewrites));
            }

            if let Some(id) = inst.id
                && let Some(key) = gvn_key(&inst.op)
            {
                if let Some(&existing) = self.values.get(&key) {
                    self.rewrites.insert(id, existing);
                    self.changed = true;
                    continue;
                }
                let prev = self.values.insert(key, id);
                undo_values.push((key, prev));

                // Bidirectional box/unbox cancellation across dominated blocks.
                match inst.op {
                    SsaOp::UnboxFixnumGuard(src) => {
                        let inv = GvnKey::BoxFixnum(id);
                        if !self.values.contains_key(&inv) {
                            let prev_inv = self.values.insert(inv, src);
                            undo_values.push((inv, prev_inv));
                        }
                    }
                    SsaOp::BoxFixnum(u) => {
                        let inv = GvnKey::UnboxFixnumGuard(id);
                        if !self.values.contains_key(&inv) {
                            let prev_inv = self.values.insert(inv, u);
                            undo_values.push((inv, prev_inv));
                        }
                    }
                    _ => {}
                }
            }

            new_insts.push(inst);
        }

        self.func.blocks[bidx].insts = new_insts;
        self.func.blocks[bidx]
            .terminator
            .rewrite_inputs(|v| *self.rewrites.get(&v).unwrap_or(&v));

        let children = self.dom.children(bid).to_vec();
        for child in children {
            self.visit(child);
        }

        for (key, prev) in undo_values.into_iter().rev() {
            if let Some(old) = prev {
                self.values.insert(key, old);
            } else {
                self.values.remove(&key);
            }
        }
    }
}

/// Dominator-tree scoped Global Value Numbering over pure operations and fixnum boxes and guards.
///
/// The block-local pass shares primitive binding checks when intervening operations preserve the
/// binding.
pub(super) fn gvn(func: &mut SsaFunction, dom: &DomTree) -> bool {
    let block_map = func.block_index_map();
    let entry = func.entry;
    let mut scope = GvnScope {
        func,
        block_map: &block_map,
        dom,
        values: FxHashMap::default(),
        rewrites: FxHashMap::default(),
        changed: false,
    };
    scope.visit(entry);

    let mut rewrites = scope.rewrites;
    let changed = scope.changed;
    if !rewrites.is_empty() {
        resolve_rewrite_chains(&mut rewrites);
        rewrite_all_uses(func, &rewrites);
    }
    changed
}

/// Hoist loop-invariant speculatable computations and loop-invariant header binding
/// guards into unique loop preheaders using `OpEffects`.
pub(super) fn licm(func: &mut SsaFunction, dom: &DomTree, loops: &LoopInfo) -> bool {
    if loops.loops.is_empty() {
        return false;
    }

    let block_map = func.block_index_map();
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
    let reprs: FxHashMap<ValueId, (ValueRepr, Option<FloatFormat>)> = func
        .blocks
        .iter()
        .flat_map(|b| {
            b.params
                .iter()
                .map(|p| (p.value, (p.repr, None)))
                .chain(b.insts.iter().filter_map(|inst| {
                    let id = inst.id?;
                    let fmt = match inst.op {
                        SsaOp::UnboxFloatGuard { format, .. }
                        | SsaOp::FloatArithChecked { format, .. }
                        | SsaOp::ReadFloatVector { format, .. } => Some(format),
                        _ => None,
                    };
                    Some((id, (inst.op.result_repr(), fmt)))
                }))
        })
        .collect();

    let mut changed = false;
    for nat_loop in &loops.loops {
        let Some(pre_bid) = nat_loop.preheader else {
            continue;
        };
        let pre_idx = block_map[&pre_bid];
        let h_idx = block_map[&nat_loop.header];

        let h_param_map: FxHashMap<ValueId, ValueId> = match &func.blocks[pre_idx].terminator {
            Terminator::Jump(target) if target.block == nat_loop.header => func.blocks[h_idx]
                .params
                .iter()
                .zip(target.args.iter())
                .map(|(p, &a)| (p.value, a))
                .collect(),
            _ => continue,
        };

        let mut hoisted = Vec::new();

        // 1. Hoist loop-invariant `GuardPrimitiveBinding`s from the header prefix when
        //    the loop body cannot write `AliasDomain::SYMBOL_FN`.
        if nat_loop.effects.writes.disjoint(AliasDomain::SYMBOL_FN) {
            let old_header = std::mem::take(&mut func.blocks[h_idx].insts);
            let mut keep_header = Vec::with_capacity(old_header.len());
            let mut prefix_open = true;
            for mut inst in old_header {
                if prefix_open
                    && matches!(inst.op, SsaOp::GuardPrimitiveBinding { .. })
                    && let Some(deopt) = inst.deopt.as_ref()
                {
                    let mut deopt_ok = true;
                    deopt.visit_values(|v| {
                        let avail = h_param_map.contains_key(&v)
                            || def_block.get(&v).is_some_and(|&db| {
                                !nat_loop.body.contains(&db) && dom.dominates(db, pre_bid)
                            });
                        if !avail {
                            deopt_ok = false;
                        }
                    });
                    if deopt_ok {
                        let mut remapped = deopt.clone();
                        remapped.rewrite_values(|kind| {
                            remap_deopt_for_preheader(kind, &h_param_map, &reprs)
                        });
                        inst.deopt = Some(remapped);
                        hoisted.push(inst);
                        changed = true;
                        continue;
                    }
                }
                let eff = inst.op.effects();
                if !eff.is_pure()
                    && !matches!(
                        inst.op,
                        SsaOp::GuardPrimitiveBinding { .. } | SsaOp::UnboxFixnumGuard(_)
                    )
                {
                    prefix_open = false;
                }
                keep_header.push(inst);
            }
            func.blocks[h_idx].insts = keep_header;
        }

        // 2. Hoist loop-invariant speculatable pure ops from all blocks in the loop body.
        for &bid in dom.rpo() {
            if !nat_loop.body.contains(&bid) {
                continue;
            }
            let bidx = block_map[&bid];
            let old_insts = std::mem::take(&mut func.blocks[bidx].insts);
            let mut keep = Vec::with_capacity(old_insts.len());
            for inst in old_insts {
                if inst.op.effects().can_hoist_out_of_loop(nat_loop.effects)
                    && !matches!(
                        inst.op,
                        SsaOp::ConstWord(_) | SsaOp::ConstI64(_) | SsaOp::ConstBool(_) | SsaOp::Copy(_)
                    )
                    && inst.deopt.is_none()
                {
                    let mut operands_invariant = true;
                    inst.op.visit_inputs(|v| {
                        let inv = def_block.get(&v).is_some_and(|&db| {
                            !nat_loop.body.contains(&db) && dom.dominates(db, pre_bid)
                        });
                        if !inv {
                            operands_invariant = false;
                        }
                    });
                    if operands_invariant {
                        if let Some(id) = inst.id {
                            def_block.insert(id, pre_bid);
                        }
                        hoisted.push(inst);
                        changed = true;
                        continue;
                    }
                }
                keep.push(inst);
            }
            func.blocks[bidx].insts = keep;
        }

        if !hoisted.is_empty() {
            func.blocks[pre_idx].insts.extend(hoisted);
        }
    }

    changed
}

fn remap_deopt_for_preheader(
    kind: DeoptRegisterValue,
    h_param_map: &FxHashMap<ValueId, ValueId>,
    reprs: &FxHashMap<ValueId, (ValueRepr, Option<FloatFormat>)>,
) -> DeoptRegisterValue {
    match kind {
        DeoptRegisterValue::TaggedWord(v) => {
            let mapped = h_param_map.get(&v).copied().unwrap_or(v);
            match reprs.get(&mapped).copied() {
                Some((ValueRepr::UnboxedI64, _)) => DeoptRegisterValue::UnboxedI64(mapped),
                Some((ValueRepr::UnboxedF32, _)) => DeoptRegisterValue::UnboxedFloat {
                    format: FloatFormat::F32,
                    value: mapped,
                },
                Some((ValueRepr::UnboxedF64, _)) => DeoptRegisterValue::UnboxedFloat {
                    format: FloatFormat::F64,
                    value: mapped,
                },
                _ => DeoptRegisterValue::TaggedWord(mapped),
            }
        }
        DeoptRegisterValue::UnboxedI64(v) => {
            DeoptRegisterValue::UnboxedI64(h_param_map.get(&v).copied().unwrap_or(v))
        }
        DeoptRegisterValue::UnboxedFloat { format, value } => DeoptRegisterValue::UnboxedFloat {
            format,
            value: h_param_map.get(&value).copied().unwrap_or(value),
        },
        DeoptRegisterValue::UnboxedI32x4 { identity, value } => DeoptRegisterValue::UnboxedI32x4 {
            identity: h_param_map.get(&identity).copied().unwrap_or(identity),
            value: h_param_map.get(&value).copied().unwrap_or(value),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::{DomTree, gvn};
    use crate::{
        BasicBlock, BlockId, BlockParam, BranchTarget, DeoptRegisterValue, DeoptState,
        GuardedPrimitive, ScopeId, ScopeTree, SsaFunction, SsaInst, SsaOp, Terminator, TypeTag,
        ValueId, ValueRepr,
    };
    use zkcl_bytecode::{Arity, FixnumArithmetic};

    #[test]
    fn binding_guard_survives_a_write_on_one_path_to_a_join() {
        let argument = ValueId(0);
        let guard = |pc| SsaInst {
            id: None,
            op: SsaOp::GuardPrimitiveBinding {
                symbol: 0,
                primitive: GuardedPrimitive::FixnumArithmetic(FixnumArithmetic::Inc),
            },
            ty: TypeTag::Top,
            pc: Some(pc),
            deopt: Some(DeoptState {
                pc,
                registers_to_restore: vec![(0, DeoptRegisterValue::TaggedWord(argument))],
            }),
        };
        let target = |id| BranchTarget {
            block: BlockId(id),
            args: Vec::new(),
        };
        let block = |id, insts, terminator, terminator_pc| BasicBlock {
            id: BlockId(id),
            params: Vec::new(),
            insts,
            terminator,
            terminator_pc,
            entry_scope: ScopeId::ROOT,
            unwind_succs: Vec::new(),
        };
        let mut entry = block(
            0,
            vec![guard(0)],
            Terminator::BranchTruthy {
                cond: argument,
                truthy: target(1),
                falsy: target(2),
            },
            1,
        );
        entry.params.push(BlockParam {
            value: argument,
            ty: TypeTag::Any,
            repr: ValueRepr::TaggedWord,
            origin_reg: Some(0),
        });
        let redefine = SsaInst {
            id: None,
            op: SsaOp::DefineFunction {
                symbol: 0,
                definition: argument,
                is_macro: false,
                at: 2,
            },
            ty: TypeTag::Top,
            pc: Some(2),
            deopt: None,
        };
        let mut function = SsaFunction {
            fixnum_bounds: (crate::FIXNUM_MIN, crate::FIXNUM_MAX),
            arity: Arity {
                required: 1,
                maximum: Some(1),
            },
            constant_count: 1,
            entry: entry.id,
            entry_params: vec![argument],
            blocks: vec![
                entry,
                block(1, vec![redefine], Terminator::Jump(target(3)), 3),
                block(2, Vec::new(), Terminator::Jump(target(3)), 4),
                block(3, vec![guard(5)], Terminator::ReturnOne(argument), 6),
            ],
            scopes: ScopeTree::new(),
            pc_to_block: Default::default(),
            next_value_id: 1,
        };
        let dom = DomTree::compute(&function, &function.block_index_map());

        gvn(&mut function, &dom);

        assert_eq!(function.blocks[3].insts, [guard(5)]);
    }
}
