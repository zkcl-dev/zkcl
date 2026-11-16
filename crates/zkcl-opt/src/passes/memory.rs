//! `AliasDomain`-driven late load/store elimination (Turboshaft-style) and
//! escape-analysis scalar replacement for local lexical boxes (`BoxLocal`).

use rustc_hash::{FxHashMap, FxHashSet};
use zkcl_bytecode::AliasDomain;

use super::dom::DomTree;
use super::rewrite::{resolve_rewrite_chains, rewrite_all_uses, rewrite_deopt_register_value};
use crate::ir::{
    BlockId, CellTarget, DeoptRegisterValue, FloatFormat, SsaFunction, SsaOp, ValueId,
};

/// Returns all `BoxLocal`-defined `ValueId`s whose only uses across the function
/// are `ReadBox(CellTarget::Local(v))` or `WriteBox { cell: CellTarget::Local(v), src }`
/// with `src != v`, and which do not appear in any `Closure`, call, global write,
/// `DeoptState`, unwind checkpoint, debug point, or terminator.
fn find_non_escaping_boxes(func: &SsaFunction) -> FxHashSet<ValueId> {
    let mut box_defs = Vec::new();
    let mut escaped = FxHashSet::default();

    for block in &func.blocks {
        for inst in &block.insts {
            if let SsaOp::BoxLocal { .. } = inst.op
                && let Some(id) = inst.id
            {
                box_defs.push(id);
            }

            match &inst.op {
                SsaOp::ReadBox(CellTarget::Local(_)) => {}
                SsaOp::WriteBox {
                    cell: CellTarget::Local(_),
                    src,
                } => {
                    escaped.insert(*src);
                }
                other => {
                    other.visit_inputs(|v| {
                        escaped.insert(v);
                    });
                }
            }

            if let Some(deopt) = &inst.deopt {
                for &(_, kind) in &deopt.registers_to_restore {
                    match kind {
                        DeoptRegisterValue::TaggedWord(v)
                        | DeoptRegisterValue::UnboxedI64(v) => {
                            escaped.insert(v);
                        }
                        DeoptRegisterValue::UnboxedFloat { value: v, .. } => {
                            escaped.insert(v);
                        }
                        DeoptRegisterValue::UnboxedI32x4 { identity, value } => {
                            escaped.insert(identity);
                            escaped.insert(value);
                        }
                        DeoptRegisterValue::ConstImmediate(_) => {}
                    }
                }
            }
        }
        block.terminator.visit_inputs(|v| {
            escaped.insert(v);
        });
    }

    box_defs
        .into_iter()
        .filter(|id| !escaped.contains(id))
        .collect()
}

struct MemoryState<'a> {
    func: &'a mut SsaFunction,
    block_map: &'a FxHashMap<BlockId, usize>,
    dom: &'a DomTree,
    non_escaping_boxes: FxHashSet<ValueId>,
    /// Immutable closure captures (`ReadCapture(index)`).
    captures: FxHashMap<usize, ValueId>,
    /// Current contents of mutable lexical cells (`Local(box_id)` or `Capture(idx)`).
    boxes: FxHashMap<CellTarget, ValueId>,
    /// Current contents of global symbol cells (`(symbol, is_function)`).
    cells: FxHashMap<(usize, bool), ValueId>,
    /// Verified `AREF` symbol constants since the last `AliasDomain::SYMBOL_FN` write.
    checked_float_readers: FxHashSet<usize>,
    /// Current unboxed elements of `(format, array, index)`.
    float_vectors: FxHashMap<(FloatFormat, ValueId, ValueId), ValueId>,
    rewrites: FxHashMap<ValueId, ValueId>,
    changed: bool,
}

impl MemoryState<'_> {
    #[inline]
    fn rewrite_val(&self, v: ValueId) -> ValueId {
        *self.rewrites.get(&v).unwrap_or(&v)
    }

    fn set_box(
        &mut self,
        cell: CellTarget,
        value: ValueId,
        undo_boxes: &mut Vec<(CellTarget, Option<ValueId>)>,
    ) {
        let prev = self.boxes.insert(cell, value);
        undo_boxes.push((cell, prev));
    }

    fn clear_escaping_boxes(&mut self, undo_boxes: &mut Vec<(CellTarget, Option<ValueId>)>) {
        let escaping_keys: Vec<CellTarget> = self
            .boxes
            .keys()
            .copied()
            .filter(|cell| {
                !matches!(cell, CellTarget::Local(id) if self.non_escaping_boxes.contains(id))
            })
            .collect();
        for key in escaping_keys {
            if let Some(prev) = self.boxes.remove(&key) {
                undo_boxes.push((key, Some(prev)));
            }
        }
    }

    fn set_cell(
        &mut self,
        key: (usize, bool),
        value: ValueId,
        undo_cells: &mut Vec<((usize, bool), Option<ValueId>)>,
    ) {
        let prev = self.cells.insert(key, value);
        undo_cells.push((key, prev));
    }

    fn clear_cells(
        &mut self,
        only_function: Option<bool>,
        undo_cells: &mut Vec<((usize, bool), Option<ValueId>)>,
    ) {
        let keys: Vec<(usize, bool)> = self
            .cells
            .keys()
            .copied()
            .filter(|&(_, is_fn)| only_function.is_none_or(|target| is_fn == target))
            .collect();
        for key in keys {
            if let Some(prev) = self.cells.remove(&key) {
                undo_cells.push((key, Some(prev)));
            }
        }
    }

    fn set_float_vector(
        &mut self,
        key: (FloatFormat, ValueId, ValueId),
        value: ValueId,
        undo_fv: &mut Vec<((FloatFormat, ValueId, ValueId), Option<ValueId>)>,
    ) {
        let prev = self.float_vectors.insert(key, value);
        undo_fv.push((key, prev));
    }

    fn clear_float_vectors(
        &mut self,
        undo_fv: &mut Vec<((FloatFormat, ValueId, ValueId), Option<ValueId>)>,
    ) {
        for (key, prev) in std::mem::take(&mut self.float_vectors) {
            undo_fv.push((key, Some(prev)));
        }
    }

    fn mark_float_reader_checked(&mut self, symbol: usize, undo_checked: &mut Vec<(usize, bool)>) {
        if self.checked_float_readers.insert(symbol) {
            undo_checked.push((symbol, false));
        }
    }

    fn clear_checked_float_readers(&mut self, undo_checked: &mut Vec<(usize, bool)>) {
        for symbol in std::mem::take(&mut self.checked_float_readers) {
            undo_checked.push((symbol, true));
        }
    }

    fn invalidate_writes(
        &mut self,
        writes: AliasDomain,
        undo_boxes: &mut Vec<(CellTarget, Option<ValueId>)>,
        undo_cells: &mut Vec<((usize, bool), Option<ValueId>)>,
        undo_fv: &mut Vec<((FloatFormat, ValueId, ValueId), Option<ValueId>)>,
        undo_checked: &mut Vec<(usize, bool)>,
    ) {
        if writes.is_empty() {
            return;
        }
        if writes.contains(AliasDomain::BOX_CELL) {
            self.clear_escaping_boxes(undo_boxes);
        }
        if writes.contains(AliasDomain::SYMBOL_VALUE) {
            self.clear_cells(Some(false), undo_cells);
        }
        if writes.contains(AliasDomain::SYMBOL_FN) {
            self.clear_cells(Some(true), undo_cells);
            self.clear_checked_float_readers(undo_checked);
        }
        if writes.contains(AliasDomain::ARRAY_ELEM) {
            self.clear_float_vectors(undo_fv);
        }
    }

    fn clear_all_mutable(
        &mut self,
        undo_boxes: &mut Vec<(CellTarget, Option<ValueId>)>,
        undo_cells: &mut Vec<((usize, bool), Option<ValueId>)>,
        undo_fv: &mut Vec<((FloatFormat, ValueId, ValueId), Option<ValueId>)>,
        undo_checked: &mut Vec<(usize, bool)>,
    ) {
        for (key, prev) in std::mem::take(&mut self.boxes) {
            undo_boxes.push((key, Some(prev)));
        }
        self.clear_cells(None, undo_cells);
        self.clear_float_vectors(undo_fv);
        self.clear_checked_float_readers(undo_checked);
    }

    fn visit(&mut self, bid: BlockId) {
        let bidx = self.block_map[&bid];

        let mut undo_captures: Vec<(usize, Option<ValueId>)> = Vec::new();
        let mut undo_boxes: Vec<(CellTarget, Option<ValueId>)> = Vec::new();
        let mut undo_cells: Vec<((usize, bool), Option<ValueId>)> = Vec::new();
        let mut undo_fv: Vec<((FloatFormat, ValueId, ValueId), Option<ValueId>)> = Vec::new();
        let mut undo_checked: Vec<(usize, bool)> = Vec::new();

        // Merge points and loop headers can observe writes along multiple paths.
        // A non-local resume starts a fresh dominator subtree with empty state.
        if self.dom.cfg_preds(bid).len() > 1 {
            self.clear_all_mutable(
                &mut undo_boxes,
                &mut undo_cells,
                &mut undo_fv,
                &mut undo_checked,
            );
        }

        let old_insts = std::mem::take(&mut self.func.blocks[bidx].insts);
        let mut new_insts = Vec::with_capacity(old_insts.len());

        for mut inst in old_insts {
            inst.op
                .rewrite_inputs(|v| *self.rewrites.get(&v).unwrap_or(&v));
            if let Some(deopt) = &mut inst.deopt {
                deopt.rewrite_values(|k| rewrite_deopt_register_value(k, &self.rewrites));
            }

            let mut keep = true;
            match inst.op {
                SsaOp::ReadCapture(idx) => {
                    if let Some(id) = inst.id {
                        if let Some(&known) = self.captures.get(&idx) {
                            self.rewrites.insert(id, known);
                            self.changed = true;
                            keep = false;
                        } else {
                            let prev = self.captures.insert(idx, id);
                            undo_captures.push((idx, prev));
                        }
                    }
                }
                SsaOp::BoxLocal { value, .. } => {
                    self.invalidate_writes(
                        inst.op.effects().writes,
                        &mut undo_boxes,
                        &mut undo_cells,
                        &mut undo_fv,
                        &mut undo_checked,
                    );
                    if let Some(id) = inst.id {
                        self.set_box(CellTarget::Local(id), self.rewrite_val(value), &mut undo_boxes);
                    }
                }
                SsaOp::WriteBox { cell, src } => {
                    let is_isolated = matches!(
                        cell,
                        CellTarget::Local(id) if self.non_escaping_boxes.contains(&id)
                    );
                    if !is_isolated {
                        self.clear_escaping_boxes(&mut undo_boxes);
                    }
                    self.set_box(cell, self.rewrite_val(src), &mut undo_boxes);
                }
                SsaOp::ReadBox(cell) => {
                    if let Some(id) = inst.id {
                        if let Some(&known) = self.boxes.get(&cell) {
                            self.rewrites.insert(id, known);
                            self.changed = true;
                            keep = false;
                        } else {
                            self.set_box(cell, id, &mut undo_boxes);
                        }
                    }
                }
                SsaOp::WriteCell { symbol, src } => {
                    self.clear_cells(Some(false), &mut undo_cells);
                    self.set_cell((symbol, false), self.rewrite_val(src), &mut undo_cells);
                }
                SsaOp::ReadCell {
                    symbol, function, ..
                } => {
                    if let Some(id) = inst.id {
                        if let Some(&known) = self.cells.get(&(symbol, function)) {
                            self.rewrites.insert(id, known);
                            self.changed = true;
                            keep = false;
                        } else {
                            // `ReadCell` carries `OpEffects::CL_SIGNAL`: on miss it may
                            // run handlers before returning `id`, so invalidate writes
                            // first and then record `(symbol, function) -> id`.
                            self.invalidate_writes(
                                inst.op.effects().writes,
                                &mut undo_boxes,
                                &mut undo_cells,
                                &mut undo_fv,
                                &mut undo_checked,
                            );
                            self.set_cell((symbol, function), id, &mut undo_cells);
                        }
                    } else {
                        self.invalidate_writes(
                            inst.op.effects().writes,
                            &mut undo_boxes,
                            &mut undo_cells,
                            &mut undo_fv,
                            &mut undo_checked,
                        );
                    }
                }
                SsaOp::ReadFloatVector {
                    symbol,
                    format,
                    array,
                    index,
                } => {
                    let key = (format, self.rewrite_val(array), self.rewrite_val(index));
                    if let Some(id) = inst.id {
                        if self.checked_float_readers.contains(&symbol)
                            && let Some(&known) = self.float_vectors.get(&key)
                        {
                            self.rewrites.insert(id, known);
                            self.changed = true;
                            keep = false;
                        } else {
                            self.mark_float_reader_checked(symbol, &mut undo_checked);
                            self.set_float_vector(key, id, &mut undo_fv);
                        }
                    }
                }
                SsaOp::WriteFloatVector {
                    format,
                    array,
                    index,
                    value,
                    ..
                } => {
                    self.invalidate_writes(
                        inst.op.effects().writes,
                        &mut undo_boxes,
                        &mut undo_cells,
                        &mut undo_fv,
                        &mut undo_checked,
                    );
                    let key = (format, self.rewrite_val(array), self.rewrite_val(index));
                    self.set_float_vector(key, self.rewrite_val(value), &mut undo_fv);
                }
                _ => {
                    self.invalidate_writes(
                        inst.op.effects().writes,
                        &mut undo_boxes,
                        &mut undo_cells,
                        &mut undo_fv,
                        &mut undo_checked,
                    );
                }
            }

            if keep {
                new_insts.push(inst);
            }
        }

        self.func.blocks[bidx].insts = new_insts;
        self.func.blocks[bidx]
            .terminator
            .rewrite_inputs(|v| *self.rewrites.get(&v).unwrap_or(&v));

        let children = self.dom.children(bid).to_vec();
        for child in children {
            self.visit(child);
        }

        for (symbol, was_present) in undo_checked.into_iter().rev() {
            if was_present {
                self.checked_float_readers.insert(symbol);
            } else {
                self.checked_float_readers.remove(&symbol);
            }
        }
        for (key, prev) in undo_fv.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.float_vectors.insert(key, v);
                }
                None => {
                    self.float_vectors.remove(&key);
                }
            }
        }
        for (key, prev) in undo_cells.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.cells.insert(key, v);
                }
                None => {
                    self.cells.remove(&key);
                }
            }
        }
        for (key, prev) in undo_boxes.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.boxes.insert(key, v);
                }
                None => {
                    self.boxes.remove(&key);
                }
            }
        }
        for (idx, prev) in undo_captures.into_iter().rev() {
            match prev {
                Some(v) => {
                    self.captures.insert(idx, v);
                }
                None => {
                    self.captures.remove(&idx);
                }
            }
        }
    }
}

/// Dominator-tree scoped load/store forwarding and redundant load elimination
/// driven by `AliasDomain` mutation epochs.
pub(super) fn late_load_elimination(
    func: &mut SsaFunction,
    block_map: &FxHashMap<BlockId, usize>,
    dom: &DomTree,
) -> bool {
    let non_escaping_boxes = find_non_escaping_boxes(func);

    let mut state = MemoryState {
        func,
        block_map,
        dom,
        non_escaping_boxes,
        captures: FxHashMap::default(),
        boxes: FxHashMap::default(),
        cells: FxHashMap::default(),
        checked_float_readers: FxHashSet::default(),
        float_vectors: FxHashMap::default(),
        rewrites: FxHashMap::default(),
        changed: false,
    };

    for &root in dom.roots() {
        state.visit(root);
    }

    let changed = state.changed;
    let mut rewrites = state.rewrites;
    if !rewrites.is_empty() {
        resolve_rewrite_chains(&mut rewrites);
        rewrite_all_uses(func, &rewrites);
    }
    changed
}

/// Escape-analysis scalar replacement for non-escaping `BoxLocal` cells.
///
/// Composes directly after [`late_load_elimination`]:
/// - A non-escaping `BoxLocal` with zero remaining writes is replaced by its
///   initial value everywhere (`ReadBox` and `BoxLocal` removed).
/// - A non-escaping `BoxLocal` with zero remaining reads has all of its
///   `WriteBox` updates and `BoxLocal` allocation removed.
pub(crate) fn scalar_replace_local_boxes(func: &mut SsaFunction, _dom: &DomTree) -> bool {
    let non_escaping = find_non_escaping_boxes(func);
    if non_escaping.is_empty() {
        return false;
    }

    #[derive(Clone, Copy)]
    struct BoxCounts {
        init: ValueId,
        reads: usize,
        writes: usize,
    }

    let mut stats: FxHashMap<ValueId, BoxCounts> = FxHashMap::default();
    for block in &func.blocks {
        for inst in &block.insts {
            if let SsaOp::BoxLocal { value, .. } = inst.op
                && let Some(id) = inst.id
                && non_escaping.contains(&id)
            {
                stats.insert(
                    id,
                    BoxCounts {
                        init: value,
                        reads: 0,
                        writes: 0,
                    },
                );
            }
        }
    }

    for block in &func.blocks {
        for inst in &block.insts {
            match inst.op {
                SsaOp::ReadBox(CellTarget::Local(id)) => {
                    if let Some(entry) = stats.get_mut(&id) {
                        entry.reads += 1;
                    }
                }
                SsaOp::WriteBox {
                    cell: CellTarget::Local(id),
                    ..
                } => {
                    if let Some(entry) = stats.get_mut(&id) {
                        entry.writes += 1;
                    }
                }
                _ => {}
            }
        }
    }

    let mut replace_with_init: FxHashMap<ValueId, ValueId> = FxHashMap::default();
    let mut dead_write_only: FxHashSet<ValueId> = FxHashSet::default();
    for (box_id, counts) in stats {
        if counts.writes == 0 {
            replace_with_init.insert(box_id, counts.init);
        } else if counts.reads == 0 {
            dead_write_only.insert(box_id);
        }
    }

    if replace_with_init.is_empty() && dead_write_only.is_empty() {
        return false;
    }

    let mut rewrites: FxHashMap<ValueId, ValueId> = FxHashMap::default();
    let mut changed = false;

    for block in &mut func.blocks {
        let before = block.insts.len();
        block.insts.retain(|inst| match inst.op {
            SsaOp::BoxLocal { .. } => {
                if let Some(id) = inst.id
                    && (replace_with_init.contains_key(&id) || dead_write_only.contains(&id))
                {
                    return false;
                }
                true
            }
            SsaOp::ReadBox(CellTarget::Local(box_id)) => {
                if let Some(&init_val) = replace_with_init.get(&box_id) {
                    if let Some(read_id) = inst.id {
                        rewrites.insert(read_id, init_val);
                    }
                    return false;
                }
                true
            }
            SsaOp::WriteBox {
                cell: CellTarget::Local(box_id),
                ..
            } => !dead_write_only.contains(&box_id),
            _ => true,
        });
        changed |= block.insts.len() != before;
    }

    if !rewrites.is_empty() {
        resolve_rewrite_chains(&mut rewrites);
        rewrite_all_uses(func, &rewrites);
    }

    changed
}
