//! Structured control-flow reconstruction over SSA basic-block graphs.
//!
//! Decomposes arbitrary reducible and irreducible control-flow graphs into a
//! tree of [`StructuredRegion`]s (`Block`, single-entry `Loop`, and local
//! `Dispatch` switches) shared by structured targets such as WebAssembly and JS.

use rustc_hash::FxHashSet;

use super::dom::IndexedDomTree;
use crate::SsaFunction;
use crate::native::NativeFrameLayout;

const MAX_LOOP_NESTING: usize = 128;

/// One structured control-flow region over block indices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StructuredRegion {
    Block(usize),
    Loop {
        header: usize,
        body: Vec<StructuredRegion>,
    },
    Dispatch(Vec<usize>),
}

impl StructuredRegion {
    /// Returns the entry block index executed first when branching into this region.
    pub fn entry(&self) -> usize {
        match self {
            Self::Block(block) | Self::Loop { header: block, .. } => *block,
            Self::Dispatch(blocks) => blocks[0],
        }
    }
}

/// Structured region tree paired with per-block local dispatch indices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuredCfg {
    pub regions: Vec<StructuredRegion>,
    pub dispatch: Vec<Option<(usize, u32)>>,
    /// Whether the region needs a forward label in the enclosing block list.
    /// Loop headers and dispatcher anchors additionally have a local backedge label.
    pub forward_labels: Vec<bool>,
}

/// Reconstruct structured regions from an indexed control-flow graph.
pub fn structure_graph(
    size: usize,
    entries: &[usize],
    visit_edges: impl Fn(usize, &mut dyn FnMut(usize)),
    order: &[usize],
) -> StructuredCfg {
    let mut cfg = Cfg::with_size(size);
    for &entry in entries {
        cfg.entries[entry] = true;
    }
    for &block in order {
        visit_edges(block, &mut |target| cfg.edge(block, target));
    }
    cfg.finish(order, entries)
}

/// Reconstruct structured regions for an [`SsaFunction`] and its [`NativeFrameLayout`].
pub fn structure_cfg(function: &SsaFunction, frame: &NativeFrameLayout) -> StructuredCfg {
    let size = function.blocks.len();
    let index = |id| frame.block_index(id).expect("RPO block index");
    let mut entries = Vec::with_capacity(1 + frame.unwind_table().targets().len());
    entries.push(index(function.entry));
    entries.extend(frame.unwind_table().targets().iter().copied().map(index));
    let order: Vec<_> = frame
        .reverse_postorder()
        .iter()
        .copied()
        .map(index)
        .collect();
    structure_graph(
        size,
        &entries,
        |block, add_edge| {
            function.blocks[block]
                .terminator
                .visit_targets(|target| add_edge(index(target.block)));
        },
        &order,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Visit {
    Outside,
    Unseen,
    Seen,
    Assigned,
}

struct Cfg {
    successors: Vec<Vec<usize>>,
    predecessors: Vec<Vec<usize>>,
    entries: Vec<bool>,
    visits: Vec<Visit>,
    component: Vec<usize>,
    dispatch: Vec<Option<(usize, u32)>>,
}

enum Scc {
    Acyclic(usize),
    Loop { header: usize, rest: Vec<usize> },
    Irreducible(Vec<usize>),
}

impl Cfg {
    fn with_size(size: usize) -> Self {
        Self {
            successors: vec![Vec::new(); size],
            predecessors: vec![Vec::new(); size],
            entries: vec![false; size],
            visits: vec![Visit::Outside; size],
            component: vec![0; size],
            dispatch: vec![None; size],
        }
    }

    fn edge(&mut self, from: usize, target: usize) {
        self.successors[from].push(target);
        self.predecessors[target].push(from);
    }

    fn finish(mut self, blocks: &[usize], entries: &[usize]) -> StructuredCfg {
        let regions = self.regions(blocks, 0);
        let forward_labels = self.forward_labels(&regions, entries);
        StructuredCfg {
            regions,
            dispatch: self.dispatch,
            forward_labels,
        }
    }

    fn forward_labels(&self, regions: &[StructuredRegion], entries: &[usize]) -> Vec<bool> {
        #[derive(Clone, Copy, Default)]
        struct Location {
            order: usize,
            next: Option<usize>,
        }

        fn place(regions: &[StructuredRegion], locations: &mut [Location], order: &mut usize) {
            for (i, region) in regions.iter().enumerate() {
                let next = regions.get(i + 1).map(StructuredRegion::entry);
                let index = *order;
                *order += 1;
                match region {
                    StructuredRegion::Block(block) => locations[*block] = Location { order: index, next },
                    StructuredRegion::Loop { header, body } => {
                        locations[*header] = Location {
                            order: index,
                            next: body.first().map(StructuredRegion::entry),
                        };
                        place(body, locations, order);
                    }
                    StructuredRegion::Dispatch(blocks) => {
                        for &block in blocks {
                            locations[block] = Location { order: index, next: None };
                        }
                    }
                }
            }
        }

        let size = self.successors.len();
        let mut locations = vec![Location::default(); size];
        let mut order = 0;
        place(regions, &mut locations, &mut order);
        let anchor = |target: usize| self.dispatch[target].map_or(target, |(entry, _)| entry);
        let mut labels = vec![false; size];
        let initial_fallthrough = regions.first().map(StructuredRegion::entry);
        for (i, &entry) in entries.iter().enumerate() {
            let target = anchor(entry);
            if i != 0 || Some(target) != initial_fallthrough {
                labels[target] = true;
            }
        }
        for (block, successors) in self.successors.iter().enumerate() {
            let loc = locations[block];
            let mut next = loc.next;
            for &successor in successors {
                let target = anchor(successor);
                if locations[target].order > loc.order {
                    // Only one arm can fall through. Another arm reaching the
                    // same region can still have different block arguments.
                    if next == Some(target) {
                        next = None;
                    } else {
                        labels[target] = true;
                    }
                }
            }
        }
        labels
    }

    fn regions(&mut self, blocks: &[usize], depth: usize) -> Vec<StructuredRegion> {
        self.components(blocks)
            .into_iter()
            .map(|component| match component {
                Scc::Acyclic(block) => StructuredRegion::Block(block),
                Scc::Loop { header, rest } if depth < MAX_LOOP_NESTING => StructuredRegion::Loop {
                    header,
                    body: self.regions(&rest, depth + 1),
                },
                Scc::Loop { header, mut rest } => {
                    rest.insert(0, header);
                    self.dispatch_region(rest)
                }
                Scc::Irreducible(blocks) => self.dispatch_region(blocks),
            })
            .collect()
    }

    fn dispatch_region(&mut self, blocks: Vec<usize>) -> StructuredRegion {
        let entry = blocks[0];
        for (case, &block) in blocks.iter().enumerate() {
            self.dispatch[block] = Some((entry, case as u32));
        }
        StructuredRegion::Dispatch(blocks)
    }

    fn components(&mut self, blocks: &[usize]) -> Vec<Scc> {
        let components = self.partition(blocks);
        let regions = components
            .into_iter()
            .enumerate()
            .map(|(index, mut members)| {
                if members.len() == 1 && !self.successors[members[0]].contains(&members[0]) {
                    return Scc::Acyclic(members[0]);
                }
                let mut headers = members.iter().copied().filter(|&block| {
                    self.entries[block]
                        || self.predecessors[block].iter().any(|&pred| {
                            self.visits[pred] == Visit::Outside || self.component[pred] != index
                        })
                });
                if let (Some(header), None) = (headers.next(), headers.next()) {
                    members.retain(|&block| block != header);
                    Scc::Loop {
                        header,
                        rest: members,
                    }
                } else {
                    Scc::Irreducible(members)
                }
            })
            .collect();

        for &block in blocks {
            self.visits[block] = Visit::Outside;
        }
        regions
    }

    fn partition(&mut self, blocks: &[usize]) -> Vec<Vec<usize>> {
        let mut postorder = Vec::with_capacity(blocks.len());
        let mut stack = Vec::new();
        for &block in blocks {
            self.visits[block] = Visit::Unseen;
        }
        for &block in blocks {
            stack.push((block, false));
            while let Some((block, exit)) = stack.pop() {
                if exit {
                    postorder.push(block);
                } else if self.visits[block] == Visit::Unseen {
                    self.visits[block] = Visit::Seen;
                    stack.push((block, true));
                    stack.extend(
                        self.successors[block]
                            .iter()
                            .rev()
                            .map(|&next| (next, false)),
                    );
                }
            }
        }

        let mut components = Vec::new();
        let mut work = Vec::new();
        for block in postorder.into_iter().rev() {
            if self.visits[block] == Visit::Assigned {
                continue;
            }
            let index = components.len();
            let mut members = Vec::new();
            work.push(block);
            while let Some(member) = work.pop() {
                if self.visits[member] == Visit::Seen {
                    self.visits[member] = Visit::Assigned;
                    self.component[member] = index;
                    members.push(member);
                    work.extend(self.predecessors[member].iter().copied());
                }
            }
            components.push(members);
        }

        components
    }
}

/// Ramsey (*Beyond Relooper*, ICFP 2022) single-pass dominator-tree control-flow
/// representation shared by structured Wasm and JS backends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StructuredNode {
    /// One basic block followed by its structured terminator.
    Block {
        block: usize,
        terminator: StructuredTerminator,
    },
    /// A single-entry natural loop headed by `header`.
    Loop {
        header: usize,
        body: Box<StructuredNode>,
    },
    /// Forward merge construct: branches targeting `BreakTo(merge)` inside
    /// `body` jump directly to `continuation`.
    LabeledBlock {
        merge: usize,
        body: Box<StructuredNode>,
        continuation: Box<StructuredNode>,
    },
    /// Dispatcher fallback restricted to a minimal irreducible multi-entry SCC.
    Dispatch { blocks: Vec<usize> },
}

/// Structured terminator at the end of a [`StructuredNode::Block`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StructuredTerminator {
    Exit,
    Jump(StructuredJump),
    Branch { targets: Vec<StructuredJump> },
}

/// Outgoing structured control edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StructuredJump {
    /// Target has a single forward predecessor and is emitted inline without a label.
    Inline(Box<StructuredNode>),
    /// Forward jump to an enclosing [`StructuredNode::LabeledBlock`] for `merge`.
    BreakTo(usize),
    /// Backedge jump to an enclosing [`StructuredNode::Loop`] for `header`.
    ContinueTo(usize),
}

/// Reconstruct a Ramsey *Beyond Relooper* [`StructuredNode`] tree over an indexed
/// CFG reachable from `entry`.
pub fn beyond_relooper_graph(
    size: usize,
    entry: usize,
    visit_edges: impl Fn(usize, &mut dyn FnMut(usize)),
    _rpo: &[usize],
) -> StructuredNode {
    let mut orig_succs = vec![Vec::new(); size];
    for (u, slot) in orig_succs.iter_mut().enumerate() {
        visit_edges(u, &mut |v| slot.push(v));
    }

    let rpo = compute_graph_rpo(size, entry, &orig_succs);
    let mut rpo_pos = vec![usize::MAX; size];
    for (idx, &b) in rpo.iter().enumerate() {
        rpo_pos[b] = idx;
    }

    let orig_dom = graph_dom(entry, &rpo, &orig_succs);

    // By Tarjan's theorem, a CFG is reducible iff removing all natural backedges
    // (`v dominates u`) leaves an acyclic graph. Any nontrivial SCC in the
    // remainder is a minimal irreducible cyclic region contracted into `Dispatch`.
    let mut acyclic = Cfg::with_size(size);
    for &u in &rpo {
        for &v in &orig_succs[u] {
            if !orig_dom.dominates(v, u) {
                acyclic.edge(u, v);
            }
        }
    }
    let sccs = acyclic.partition(&rpo);
    let mut scc_of = vec![usize::MAX; size];
    for (cid, members) in sccs.iter().enumerate() {
        for &m in members {
            scc_of[m] = cid;
        }
    }

    enum CondKind {
        Single(usize),
        Dispatch(Vec<usize>),
    }

    let cond_count = sccs.len();
    let cond_nodes: Vec<CondKind> = sccs
        .into_iter()
        .map(|mut members| {
            if members.len() == 1 {
                CondKind::Single(members[0])
            } else {
                members.sort_by_key(|&b| rpo_pos[b]);
                CondKind::Dispatch(members)
            }
        })
        .collect();
    let repr_block: Vec<usize> = cond_nodes
        .iter()
        .map(|node| match node {
            CondKind::Single(b) => *b,
            CondKind::Dispatch(blocks) => blocks[0],
        })
        .collect();

    let mut cond_succs: Vec<Vec<usize>> = vec![Vec::new(); cond_count];
    for (cid, kind) in cond_nodes.iter().enumerate() {
        match kind {
            CondKind::Single(u) => {
                for &v in &orig_succs[*u] {
                    cond_succs[cid].push(scc_of[v]);
                }
            }
            CondKind::Dispatch(blocks) => {
                for &u in blocks {
                    for &v in &orig_succs[u] {
                        let target_cid = scc_of[v];
                        if target_cid != cid {
                            cond_succs[cid].push(target_cid);
                        }
                    }
                }
            }
        }
    }

    let cond_entry = scc_of[entry];
    let cond_rpo = compute_graph_rpo(cond_count, cond_entry, &cond_succs);
    let mut cond_rpo_pos = vec![usize::MAX; cond_count];
    for (idx, &cid) in cond_rpo.iter().enumerate() {
        cond_rpo_pos[cid] = idx;
    }

    let cond_dom = graph_dom(cond_entry, &cond_rpo, &cond_succs);

    let mut is_loop_header = vec![false; cond_count];
    let mut forward_preds = vec![0usize; cond_count];
    let mut loop_backedges: Vec<Vec<usize>> = vec![Vec::new(); cond_count];
    let mut cond_preds: Vec<Vec<usize>> = vec![Vec::new(); cond_count];

    for &u in &cond_rpo {
        for &v in &cond_succs[u] {
            cond_preds[v].push(u);
            if cond_dom.dominates(v, u) {
                is_loop_header[v] = true;
                loop_backedges[v].push(u);
            } else {
                forward_preds[v] += 1;
            }
        }
    }

    // Any forward target of a Dispatch SCC acts as a merge point so `Dispatch`
    // can sit cleanly inside `LabeledBlock { merge, body: Dispatch, continuation }`.
    let mut is_merge: Vec<bool> = forward_preds.iter().map(|&count| count >= 2).collect();
    for (u, kind) in cond_nodes.iter().enumerate() {
        if matches!(kind, CondKind::Dispatch(_)) {
            for &v in &cond_succs[u] {
                if !cond_dom.dominates(v, u) {
                    is_merge[v] = true;
                }
            }
        }
    }

    let mut loop_bodies: Vec<FxHashSet<usize>> = vec![FxHashSet::default(); cond_count];
    for h in 0..cond_count {
        if !is_loop_header[h] {
            continue;
        }
        let mut body = FxHashSet::default();
        body.insert(h);
        let mut stack = loop_backedges[h].clone();
        while let Some(node) = stack.pop() {
            if body.insert(node) {
                for &pred in &cond_preds[node] {
                    if !body.contains(&pred) {
                        stack.push(pred);
                    }
                }
            }
        }
        loop_bodies[h] = body;
    }

    struct RamseyBuilder<'a> {
        cond_nodes: &'a [CondKind],
        repr_block: &'a [usize],
        orig_succs: &'a [Vec<usize>],
        scc_of: &'a [usize],
        cond_dom: &'a IndexedDomTree,
        cond_rpo_pos: &'a [usize],
        is_loop_header: &'a [bool],
        is_merge: &'a [bool],
        loop_bodies: &'a [FxHashSet<usize>],
    }

    impl RamseyBuilder<'_> {
        fn wrap_merges(
            &self,
            mut node: StructuredNode,
            merges: &[usize],
        ) -> StructuredNode {
            for &merge_cid in merges.iter().rev() {
                let continuation = Box::new(self.do_tree(merge_cid));
                node = StructuredNode::LabeledBlock {
                    merge: self.repr_block[merge_cid],
                    body: Box::new(node),
                    continuation,
                };
            }
            node
        }

        fn do_tree(&self, x: usize) -> StructuredNode {
            let mut merge_children: Vec<usize> = self.cond_dom.children[x]
                .iter()
                .copied()
                .filter(|&c| self.is_merge[c])
                .collect();
            merge_children.sort_by_key(|&c| self.cond_rpo_pos[c]);

            if self.is_loop_header[x] {
                let loop_body = &self.loop_bodies[x];
                let mut in_loop_merges = Vec::new();
                let mut exit_merges = Vec::new();
                for c in merge_children {
                    if loop_body.contains(&c) {
                        in_loop_merges.push(c);
                    } else {
                        exit_merges.push(c);
                    }
                }
                let inner = self.wrap_merges(self.node_within(x), &in_loop_merges);
                let looped = StructuredNode::Loop {
                    header: self.repr_block[x],
                    body: Box::new(inner),
                };
                self.wrap_merges(looped, &exit_merges)
            } else {
                self.wrap_merges(self.node_within(x), &merge_children)
            }
        }

        fn node_within(&self, x: usize) -> StructuredNode {
            match &self.cond_nodes[x] {
                CondKind::Dispatch(blocks) => StructuredNode::Dispatch {
                    blocks: blocks.clone(),
                },
                CondKind::Single(orig_block) => {
                    let succs = &self.orig_succs[*orig_block];
                    let terminator = match succs.len() {
                        0 => StructuredTerminator::Exit,
                        1 => StructuredTerminator::Jump(self.do_branch(x, self.scc_of[succs[0]])),
                        _ => StructuredTerminator::Branch {
                            targets: succs
                                .iter()
                                .map(|&y| self.do_branch(x, self.scc_of[y]))
                                .collect(),
                        },
                    };
                    StructuredNode::Block {
                        block: *orig_block,
                        terminator,
                    }
                }
            }
        }

        fn do_branch(&self, from_cid: usize, to_cid: usize) -> StructuredJump {
            if self.cond_dom.dominates(to_cid, from_cid) {
                StructuredJump::ContinueTo(self.repr_block[to_cid])
            } else if self.is_merge[to_cid] {
                StructuredJump::BreakTo(self.repr_block[to_cid])
            } else {
                StructuredJump::Inline(Box::new(self.do_tree(to_cid)))
            }
        }
    }

    let builder = RamseyBuilder {
        cond_nodes: &cond_nodes,
        repr_block: &repr_block,
        orig_succs: &orig_succs,
        scc_of: &scc_of,
        cond_dom: &cond_dom,
        cond_rpo_pos: &cond_rpo_pos,
        is_loop_header: &is_loop_header,
        is_merge: &is_merge,
        loop_bodies: &loop_bodies,
    };
    builder.do_tree(cond_entry)
}

/// Reconstruct a Ramsey *Beyond Relooper* [`StructuredNode`] tree for an [`SsaFunction`]
/// and its [`NativeFrameLayout`], matching [`structure_cfg`]'s block-indexing contract.
pub fn beyond_relooper(function: &SsaFunction, frame: &NativeFrameLayout) -> StructuredNode {
    let index = |id| frame.block_index(id).expect("RPO block index");
    let rpo: Vec<usize> = frame
        .reverse_postorder()
        .iter()
        .copied()
        .map(index)
        .collect();
    beyond_relooper_graph(
        function.blocks.len(),
        index(function.entry),
        |block_idx, add_edge| {
            function.blocks[block_idx]
                .terminator
                .visit_targets(|target| add_edge(index(target.block)));
        },
        &rpo,
    )
}

fn compute_graph_rpo(size: usize, entry: usize, succs: &[Vec<usize>]) -> Vec<usize> {
    let mut visited = vec![false; size];
    let mut postorder = Vec::new();
    let mut stack = vec![(entry, false)];
    while let Some((node, exit)) = stack.pop() {
        if exit {
            postorder.push(node);
        } else if !visited[node] {
            visited[node] = true;
            stack.push((node, true));
            for &succ in succs[node].iter().rev() {
                if !visited[succ] {
                    stack.push((succ, false));
                }
            }
        }
    }
    postorder.reverse();
    postorder
}

fn graph_dom(entry: usize, rpo: &[usize], successors: &[Vec<usize>]) -> IndexedDomTree {
    let mut predecessors = vec![Vec::new(); successors.len()];
    for &block in rpo {
        for &target in &successors[block] {
            predecessors[target].push(block);
        }
    }
    IndexedDomTree::compute(&predecessors, rpo, [entry])
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;
    use zkcl_bytecode::Arity;

    use super::*;
    use crate::native::native_frame_layout;
    use crate::{
        BasicBlock, BlockId, BlockParam, BranchTarget, CallSiteProfile, ScopeId, ScopeTree,
        SsaInst, SsaOp, Terminator, TypeTag, ValueId, ValueRepr,
    };

    fn from_adj(edges: &[&[usize]], entries: &[usize]) -> StructuredCfg {
        let order: Vec<_> = (0..edges.len()).collect();
        structure_graph(
            edges.len(),
            entries,
            |block, add| {
                for &succ in edges[block] {
                    add(succ);
                }
            },
            &order,
        )
    }

    #[test]
    fn linear_and_diamond_cfgs_remain_acyclic_blocks() {
        let linear = from_adj(&[&[1], &[2], &[]], &[0]);
        assert_eq!(
            linear.regions,
            vec![
                StructuredRegion::Block(0),
                StructuredRegion::Block(1),
                StructuredRegion::Block(2),
            ]
        );
        assert!(linear.dispatch.iter().all(Option::is_none));
        assert_eq!(linear.forward_labels, vec![false; 3]);

        let diamond = from_adj(&[&[1, 2], &[3], &[3], &[]], &[0]);
        assert_eq!(diamond.regions.len(), 4);
        assert_eq!(diamond.regions[0], StructuredRegion::Block(0));
        assert_eq!(diamond.regions[3], StructuredRegion::Block(3));
        assert!(diamond.dispatch.iter().all(Option::is_none));
    }

    #[test]
    fn parallel_edges_to_a_fallthrough_keep_a_label_for_block_arguments() {
        let flow = from_adj(&[&[1, 1], &[]], &[0]);
        assert_eq!(flow.forward_labels, [false, true]);
    }

    #[test]
    fn self_loop_forms_single_header_loop() {
        let flow = from_adj(&[&[1], &[1, 2], &[]], &[0]);
        assert_eq!(
            flow.regions,
            vec![
                StructuredRegion::Block(0),
                StructuredRegion::Loop {
                    header: 1,
                    body: Vec::new(),
                },
                StructuredRegion::Block(2),
            ]
        );
        assert_eq!(flow.regions[1].entry(), 1);
    }

    #[test]
    fn nested_natural_loops_have_distinct_headers() {
        let flow = from_adj(&[&[1], &[2, 4], &[3, 1], &[2], &[]], &[0]);
        let [
            StructuredRegion::Block(0),
            StructuredRegion::Loop { header: 1, body },
            StructuredRegion::Block(4),
        ] = flow.regions.as_slice()
        else {
            panic!("expected a loop between entry and exit");
        };
        let [StructuredRegion::Loop { header: 2, body }] = body.as_slice() else {
            panic!("expected a nested loop");
        };
        assert_eq!(body.as_slice(), &[StructuredRegion::Block(3)]);
        assert!(flow.dispatch.iter().all(Option::is_none));
    }

    #[test]
    fn dispatcher_is_local_to_a_multi_entry_cycle() {
        let flow = from_adj(&[&[1, 2], &[2, 3], &[1, 3], &[]], &[0]);
        let [
            StructuredRegion::Block(0),
            StructuredRegion::Dispatch(members),
            StructuredRegion::Block(3),
        ] = flow.regions.as_slice()
        else {
            panic!("expected dispatch only inside the cycle");
        };
        assert_eq!(members.len(), 2);
        for (case, &member) in members.iter().enumerate() {
            assert_eq!(flow.dispatch[member], Some((members[0], case as u32)));
        }
        assert_eq!(flow.dispatch[0], None);
        assert_eq!(flow.dispatch[3], None);
    }

    #[test]
    fn beyond_relooper_structures_diamond_without_dispatch() {
        let order = [0, 1, 2, 3];
        let tree = beyond_relooper_graph(
            4,
            0,
            |b, add| match b {
                0 => {
                    add(1);
                    add(2);
                }
                1 | 2 => add(3),
                _ => {}
            },
            &order,
        );
        assert_eq!(
            tree,
            StructuredNode::LabeledBlock {
                merge: 3,
                body: Box::new(StructuredNode::Block {
                    block: 0,
                    terminator: StructuredTerminator::Branch {
                        targets: vec![
                            StructuredJump::Inline(Box::new(StructuredNode::Block {
                                block: 1,
                                terminator: StructuredTerminator::Jump(StructuredJump::BreakTo(3)),
                            })),
                            StructuredJump::Inline(Box::new(StructuredNode::Block {
                                block: 2,
                                terminator: StructuredTerminator::Jump(StructuredJump::BreakTo(3)),
                            })),
                        ],
                    },
                }),
                continuation: Box::new(StructuredNode::Block {
                    block: 3,
                    terminator: StructuredTerminator::Exit,
                }),
            }
        );
    }

    #[test]
    fn beyond_relooper_places_in_loop_merge_inside_loop_and_exit_merge_outside() {
        // 0 -> 1 (loop header), 1 -> [2, 3], 3 -> 2 (in-loop merge!),
        // 2 -> [1 (backedge), 4 (exit)], 4 -> []
        let order = [0, 1, 3, 2, 4];
        let tree = beyond_relooper_graph(
            5,
            0,
            |b, add| match b {
                0 => add(1),
                1 => {
                    add(2);
                    add(3);
                }
                3 => add(2),
                2 => {
                    add(1);
                    add(4);
                }
                _ => {}
            },
            &order,
        );
        let StructuredNode::Block {
            block: 0,
            terminator: StructuredTerminator::Jump(StructuredJump::Inline(loop_node)),
        } = tree
        else {
            panic!("expected inline entry into loop header 1, got {tree:?}");
        };
        let StructuredNode::Loop { header: 1, body } = *loop_node else {
            panic!("expected Loop(1), got {loop_node:?}");
        };
        let StructuredNode::LabeledBlock {
            merge: 2,
            continuation,
            ..
        } = *body
        else {
            panic!("expected in-loop LabeledBlock(merge: 2) inside Loop(1), got {body:?}");
        };
        let StructuredNode::Block {
            block: 2,
            terminator: StructuredTerminator::Branch { targets },
        } = *continuation
        else {
            panic!("expected Block(2) in continuation of LabeledBlock(2)");
        };
        assert_eq!(targets[0], StructuredJump::ContinueTo(1));
        assert!(matches!(targets[1], StructuredJump::Inline(_)));
    }

    #[test]
    fn beyond_relooper_isolates_irreducible_cycle_into_local_dispatch() {
        // 0 -> [1, 2], 1 -> [2, 3], 2 -> [1, 3], 3 -> []
        let order = [0, 1, 2, 3];
        let tree = beyond_relooper_graph(
            4,
            0,
            |b, add| match b {
                0 => {
                    add(1);
                    add(2);
                }
                1 => {
                    add(2);
                    add(3);
                }
                2 => {
                    add(1);
                    add(3);
                }
                _ => {}
            },
            &order,
        );
        fn contains_dispatch(node: &StructuredNode) -> bool {
            match node {
                StructuredNode::Dispatch { blocks } => blocks == &[1, 2],
                StructuredNode::Loop { body, .. } => contains_dispatch(body),
                StructuredNode::LabeledBlock {
                    body,
                    continuation,
                    ..
                } => contains_dispatch(body) || contains_dispatch(continuation),
                StructuredNode::Block { terminator, .. } => match terminator {
                    StructuredTerminator::Exit => false,
                    StructuredTerminator::Jump(j) => jump_has_dispatch(j),
                    StructuredTerminator::Branch { targets } => targets.iter().any(jump_has_dispatch),
                },
            }
        }
        fn jump_has_dispatch(j: &StructuredJump) -> bool {
            matches!(j, StructuredJump::Inline(n) if contains_dispatch(n))
        }
        assert!(contains_dispatch(&tree), "expected local Dispatch([1, 2]), got {tree:?}");
    }

    #[test]
    fn beyond_relooper_matches_ssa_function_frame_layout() {
        let target = |b| BranchTarget {
            block: BlockId(b),
            args: Vec::new(),
        };
        let ssa = SsaFunction {
            fixnum_bounds: (-536_870_912, 536_870_911),
            arity: Arity {
                required: 1,
                maximum: Some(1),
            },
            constant_count: 0,
            entry: BlockId(0),
            entry_params: vec![ValueId(0)],
            next_value_id: 2,
            scopes: ScopeTree::new(),
            pc_to_block: FxHashMap::default(),
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    params: vec![BlockParam {
                        value: ValueId(0),
                        ty: TypeTag::Any,
                        repr: ValueRepr::TaggedWord,
                        origin_reg: Some(0),
                    }],
                    insts: Vec::new(),
                    terminator: Terminator::BranchTruthy {
                        cond: ValueId(0),
                        truthy: target(1),
                        falsy: target(2),
                    },
                    terminator_pc: 0,
                    entry_scope: ScopeId::ROOT,
                    unwind_succs: Vec::new(),
                },
                BasicBlock {
                    id: BlockId(1),
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Terminator::Jump(target(3)),
                    terminator_pc: 1,
                    entry_scope: ScopeId::ROOT,
                    unwind_succs: Vec::new(),
                },
                BasicBlock {
                    id: BlockId(2),
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Terminator::Jump(target(3)),
                    terminator_pc: 2,
                    entry_scope: ScopeId::ROOT,
                    unwind_succs: Vec::new(),
                },
                BasicBlock {
                    id: BlockId(3),
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Terminator::ReturnOne(ValueId(0)),
                    terminator_pc: 3,
                    entry_scope: ScopeId::ROOT,
                    unwind_succs: Vec::new(),
                },
            ],
        };
        let frame = native_frame_layout(&ssa);
        let tree = beyond_relooper(&ssa, &frame);
        assert!(matches!(
            tree,
            StructuredNode::LabeledBlock { merge: 3, .. }
        ));
    }

    #[test]
    fn structure_cfg_treats_unwind_targets_as_loop_entries() {
        let target = |b| BranchTarget {
            block: BlockId(b),
            args: Vec::new(),
        };
        let handler_param = BlockParam {
            value: ValueId(0),
            ty: TypeTag::Any,
            repr: ValueRepr::TaggedWord,
            origin_reg: None,
        };
        let ssa = SsaFunction {
            fixnum_bounds: (-536_870_912, 536_870_911),
            arity: Arity::ZERO,
            constant_count: 1,
            entry: BlockId(0),
            entry_params: Vec::new(),
            next_value_id: 1,
            scopes: ScopeTree::new(),
            pc_to_block: FxHashMap::default(),
            blocks: vec![
                BasicBlock {
                    id: BlockId(0),
                    params: Vec::new(),
                    insts: vec![
                        SsaInst {
                            id: None,
                            op: SsaOp::EnterHandlers {
                                table: 0,
                                targets: vec![BlockId(1)],
                                function_targets: false,
                            },
                            ty: TypeTag::Any,
                            pc: Some(0),
                            deopt: None,
                        },
                        SsaInst {
                            id: None,
                            op: SsaOp::CallNamed {
                                symbol: 0,
                                args: Vec::new(),
                                has_result: false,
                                at: 1,
                                associate: false,
                                notinline: false,
                                profile: CallSiteProfile::default(),
                            },
                            ty: TypeTag::Any,
                            pc: Some(1),
                            deopt: None,
                        },
                    ],
                    terminator: Terminator::Jump(target(1)),
                    terminator_pc: 0,
                    entry_scope: ScopeId::ROOT,
                    unwind_succs: vec![BlockId(1)],
                },
                BasicBlock {
                    id: BlockId(1),
                    params: vec![handler_param],
                    insts: Vec::new(),
                    terminator: Terminator::BranchTruthy {
                        cond: ValueId(0),
                        truthy: target(1),
                        falsy: target(2),
                    },
                    terminator_pc: 1,
                    entry_scope: ScopeId::ROOT,
                    unwind_succs: Vec::new(),
                },
                BasicBlock {
                    id: BlockId(2),
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Terminator::ReturnValues(Vec::new()),
                    terminator_pc: 2,
                    entry_scope: ScopeId::ROOT,
                    unwind_succs: Vec::new(),
                },
            ],
        };
        let frame = native_frame_layout(&ssa);
        let flow = structure_cfg(&ssa, &frame);
        assert_eq!(flow.forward_labels, [false, true, true]);
        assert_eq!(
            flow.regions,
            vec![
                StructuredRegion::Block(0),
                StructuredRegion::Loop {
                    header: 1,
                    body: Vec::new(),
                },
                StructuredRegion::Block(2),
            ]
        );
    }
}
