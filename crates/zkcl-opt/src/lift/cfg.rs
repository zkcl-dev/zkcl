use std::collections::VecDeque;

use rustc_hash::FxHashMap;
use zkcl_bytecode::{
    BlockLeaders, BytecodeBody, HandlerTarget, Instruction, Register, RestartTarget,
};

use super::liveness::side_table_registers;
use super::{BlockId, BytecodeBlock, BytecodeLiftError, RecoveryEntry, UnwindEntryRegisters};
use crate::ir::{ControlScopeKind, ScopeId, ScopeTree};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Empty,
    Target,
    Protect {
        cleanup: usize,
        clear_from: Register,
    },
    Cleanup,
    Handler,
    Restart,
    Association,
    Dynamic,
    Values,
}

#[derive(Clone, Copy)]
struct ControlScope {
    kind: ScopeKind,
    parent: usize,
}

struct UnwindEdge {
    source: usize,
    target: usize,
    clear_from: Register,
    condition_register: Option<Register>,
}

struct TagbodyScope {
    scope: usize,
    token: Register,
    jumps: usize,
    clear_from: Register,
}

pub(super) struct BytecodeCfg {
    pub blocks: Vec<BytecodeBlock>,
    pub scope_tree: ScopeTree,
    pub pc_to_block: FxHashMap<usize, BlockId>,
    pub local_go_target: FxHashMap<usize, usize>,
    pub begin_cleanup_target: FxHashMap<usize, usize>,
    pub normal_recoveries: FxHashMap<usize, UnwindEntryRegisters>,
    pub recovery_entries: Vec<RecoveryEntry>,
    pub recovery_blocks: FxHashMap<RecoveryEntry, BlockId>,
}

impl BytecodeCfg {
    pub fn execution_checkpoint(&self, pc: usize, instruction: &Instruction) -> bool {
        if let Some(target) = instruction.flow().target() {
            return target <= pc;
        }
        match instruction {
            Instruction::BeginCleanup => self.begin_cleanup_target[&pc] <= pc,
            // A transfer can land at an earlier bytecode PC through a native
            // unwind state. Direct jumps synthesized for local GO count too.
            Instruction::Go { .. } | Instruction::Throw { .. } | Instruction::ReturnFrom { .. } => {
                true
            }
            _ => false,
        }
    }

    pub fn recovery_target(
        &self,
        pc: usize,
        clear_from: Register,
        condition_register: Option<Register>,
    ) -> BlockId {
        self.recovery_blocks[&RecoveryEntry {
            target: self.pc_to_block[&pc],
            registers: UnwindEntryRegisters {
                clear_from,
                condition_register,
            },
        }]
    }

    pub fn analyze(code: &BytecodeBody<'_>) -> Result<Self, BytecodeLiftError> {
        let len = code.instructions.len();
        // Track enclosing protect scopes so `BeginCleanup` resolves to its cleanup PC,
        // and collect all basic-block leaders.
        let mut leaders = BlockLeaders::new(len);

        for table in code.handlers {
            for handler in &table.handlers {
                if let HandlerTarget::Case { pc, .. } = handler.target {
                    leaders.mark(pc);
                }
            }
        }
        for table in code.restarts {
            for spec in &table.restarts {
                if let RestartTarget::Case { pc, .. } = spec.target {
                    leaders.mark(pc);
                }
            }
        }
        for table in code.jump_tables {
            for &target in &table.targets {
                leaders.mark(target);
            }
        }

        let mut begin_cleanup_target: FxHashMap<usize, usize> = FxHashMap::default();
        let mut normal_recoveries = FxHashMap::default();
        let mut unwind_edges = Vec::new();
        let mut tagbody_entries = Vec::new();

        let mut scope_at_pc: Vec<Option<usize>> = vec![None; len + 1];
        let mut scopes = vec![ControlScope {
            kind: ScopeKind::Empty,
            parent: 0,
        }];
        let mut pending = VecDeque::from([(0usize, 0usize)]);

        while let Some((pc, mut stack)) = pending.pop_front() {
            if let Some(prev) = scope_at_pc[pc] {
                if prev != stack {
                    return Err(BytecodeLiftError::InvalidControlFlow {
                        pc,
                        reason: "control-flow paths disagree on active scopes",
                    });
                }
                continue;
            }
            scope_at_pc[pc] = Some(stack);
            if pc == len {
                continue;
            }

            let pop = |stack: usize, kind: ScopeKind| -> Result<usize, BytecodeLiftError> {
                let entry = scopes[stack];
                if entry.kind == kind {
                    Ok(entry.parent)
                } else {
                    Err(BytecodeLiftError::InvalidControlFlow {
                        pc,
                        reason: "instruction closes the wrong control scope",
                    })
                }
            };

            let instruction = &code.instructions[pc];
            let flow = instruction.flow();
            leaders.observe(pc, flow);
            let mut unwind_to = |target, clear, condition, target_scope| {
                leaders.mark(target);
                unwind_edges.push(UnwindEdge {
                    source: pc,
                    target,
                    clear_from: clear,
                    condition_register: condition,
                });
                pending.push_back((target, target_scope));
            };
            let mut push = None;
            match instruction {
                Instruction::EnterBlock { resume, clear, .. }
                | Instruction::EnterCatch { resume, clear, .. } => {
                    unwind_to(*resume, *clear, None, stack);
                    push = Some(ScopeKind::Target);
                }
                Instruction::EnterTagbody {
                    token,
                    jumps,
                    clear,
                } => {
                    let child = scopes.len();
                    tagbody_entries.push(TagbodyScope {
                        scope: child,
                        token: *token,
                        jumps: *jumps,
                        clear_from: *clear,
                    });
                    for &target in &code.jump_tables[*jumps].targets {
                        unwind_to(target, *clear, None, child);
                    }
                    push = Some(ScopeKind::Target);
                }
                Instruction::EnterProtect { cleanup, clear } => {
                    let child = scopes.len();
                    scopes.push(ControlScope {
                        kind: ScopeKind::Cleanup,
                        parent: stack,
                    });
                    unwind_to(*cleanup, *clear, None, child);
                    push = Some(ScopeKind::Protect {
                        cleanup: *cleanup,
                        clear_from: *clear,
                    });
                }
                Instruction::BeginCleanup => {
                    let ScopeKind::Protect {
                        cleanup,
                        clear_from,
                    } = scopes[stack].kind
                    else {
                        return Err(BytecodeLiftError::InvalidControlFlow {
                            pc,
                            reason: "cleanup begins outside a protect scope",
                        });
                    };
                    begin_cleanup_target.insert(pc, cleanup);
                    normal_recoveries.insert(
                        pc,
                        UnwindEntryRegisters {
                            clear_from,
                            condition_register: None,
                        },
                    );
                    // EnterProtect already queued the cleanup PC with its cleanup scope.
                }
                Instruction::EndCleanup => {
                    stack = pop(stack, ScopeKind::Cleanup)?;
                }
                Instruction::EnterHandlers { table } => {
                    for handler in &code.handlers[*table].handlers {
                        if let HandlerTarget::Case {
                            pc: target,
                            condition,
                            clear,
                        } = handler.target
                        {
                            unwind_to(target, clear, Some(condition), stack);
                        }
                    }
                    push = Some(ScopeKind::Handler);
                }
                Instruction::EnterRestarts { table, .. } => {
                    for spec in &code.restarts[*table].restarts {
                        if let RestartTarget::Case { pc: target, clear } = spec.target {
                            unwind_to(target, clear, None, stack);
                        }
                    }
                    push = Some(ScopeKind::Restart);
                }
                Instruction::AssociateRestarts { .. } => push = Some(ScopeKind::Association),
                Instruction::BindSpecial { .. } | Instruction::BindProgv { .. } => {
                    push = Some(ScopeKind::Dynamic)
                }
                Instruction::SaveValues => push = Some(ScopeKind::Values),
                Instruction::AppendValues => {
                    if scopes[stack].kind != ScopeKind::Values {
                        return Err(BytecodeLiftError::InvalidControlFlow {
                            pc,
                            reason: "appending values requires an active saved-values scope",
                        });
                    }
                }
                Instruction::RestoreValues => stack = pop(stack, ScopeKind::Values)?,
                Instruction::LeaveTarget => stack = pop(stack, ScopeKind::Target)?,
                Instruction::LeaveHandlers => stack = pop(stack, ScopeKind::Handler)?,
                Instruction::LeaveRestarts => stack = pop(stack, ScopeKind::Restart)?,
                Instruction::LeaveRestartAssociation => stack = pop(stack, ScopeKind::Association)?,
                Instruction::Unbind(count) => {
                    for _ in 0..*count {
                        stack = pop(stack, ScopeKind::Dynamic)?;
                    }
                }
                _ => {}
            }

            if let Some(kind) = push {
                let next = scopes.len();
                scopes.push(ControlScope {
                    kind,
                    parent: stack,
                });
                stack = next;
            }
            // The scope walk visits the explicit branch before its fallthrough.
            pending.extend(flow.successors(pc + 1).rev().map(|target| (target, stack)));
        }

        let local_gos =
            resolve_local_gos(code, &scopes, &scope_at_pc, &tagbody_entries, &unwind_edges)?;
        let mut local_go_target = FxHashMap::default();
        for (pc, (target, clear_from)) in local_gos {
            local_go_target.insert(pc, target);
            normal_recoveries.insert(
                pc,
                UnwindEntryRegisters {
                    clear_from,
                    condition_register: None,
                },
            );
        }
        // Build basic blocks for reachable leaders.
        let mut pc_to_block: FxHashMap<usize, BlockId> = FxHashMap::default();
        let mut bytecode_blocks: Vec<BytecodeBlock> = Vec::new();
        for std::ops::Range {
            start: start_pc,
            end: end_pc,
        } in leaders.ranges()
        {
            let Some(scope) = scope_at_pc[start_pc] else {
                continue;
            };
            let id = BlockId(bytecode_blocks.len() as u32);
            pc_to_block.insert(start_pc, id);
            bytecode_blocks.push(BytecodeBlock {
                id,
                start_pc,
                end_pc,
                entry_scope: ScopeId(scope as u32),
                succs: Vec::new(),
                unwind_succs: Vec::new(),
            });
        }

        // All incoming destinations are leaders. Within a block, reachable instructions
        // were visited in bytecode order during scope analysis.
        let mut recovery_entries = Vec::new();
        let mut recovery_blocks = FxHashMap::default();
        for edge in unwind_edges {
            let index = bytecode_blocks.partition_point(|block| block.start_pc <= edge.source) - 1;
            let entry = RecoveryEntry {
                target: pc_to_block[&edge.target],
                registers: UnwindEntryRegisters {
                    clear_from: edge.clear_from,
                    condition_register: edge.condition_register,
                },
            };
            recovery_blocks.entry(entry).or_insert_with(|| {
                let id = BlockId((bytecode_blocks.len() + recovery_entries.len()) as u32);
                recovery_entries.push(entry);
                id
            });
            push_unique(&mut bytecode_blocks[index].unwind_succs, entry);
        }

        for block in &mut bytecode_blocks {
            let start_pc = block.start_pc;
            let end_pc = block.end_pc;
            if start_pc == end_pc {
                continue;
            }
            let last_pc = end_pc - 1;
            let mut edge = |pc| push_unique(&mut block.succs, pc_to_block[&pc]);
            if let Some(&target) = begin_cleanup_target
                .get(&last_pc)
                .or_else(|| local_go_target.get(&last_pc))
            {
                edge(target);
            } else {
                code.instructions[last_pc]
                    .flow()
                    .successors(end_pc)
                    .for_each(edge);
            }
        }

        let mut scope_tree = ScopeTree::new();
        for (idx, scope) in scopes.iter().enumerate().skip(1) {
            let kind = match scope.kind {
                ScopeKind::Empty => ControlScopeKind::Root,
                ScopeKind::Target => ControlScopeKind::Target,
                ScopeKind::Protect { .. } => ControlScopeKind::Protect,
                ScopeKind::Cleanup => ControlScopeKind::Cleanup,
                ScopeKind::Handler => ControlScopeKind::Handler,
                ScopeKind::Restart => ControlScopeKind::Restart,
                ScopeKind::Association => ControlScopeKind::Association,
                ScopeKind::Dynamic => ControlScopeKind::Dynamic,
                ScopeKind::Values => ControlScopeKind::Values,
            };
            let pushed = scope_tree.push(kind, ScopeId(scope.parent as u32));
            debug_assert_eq!(pushed, ScopeId(idx as u32));
        }

        Ok(Self {
            blocks: bytecode_blocks,
            scope_tree,
            pc_to_block,
            local_go_target,
            begin_cleanup_target,
            normal_recoveries,
            recovery_entries,
            recovery_blocks,
        })
    }
}

fn resolve_local_gos(
    code: &BytecodeBody<'_>,
    scopes: &[ControlScope],
    scope_at_pc: &[Option<usize>],
    tagbody_entries: &[TagbodyScope],
    unwind_edges: &[UnwindEdge],
) -> Result<FxHashMap<usize, (usize, Register)>, BytecodeLiftError> {
    let mut local_go_target = FxHashMap::default();
    if tagbody_entries.is_empty() {
        return Ok(local_go_target);
    }

    // Skip intervening handler, target and binding scopes when looking up the
    // tagbodies that can observe an instruction or recovery edge.
    let mut nearest_tagbody = vec![None; scopes.len()];
    for (idx, tagbody) in tagbody_entries.iter().enumerate() {
        nearest_tagbody[tagbody.scope] = Some(idx);
    }
    for scope in 1..scopes.len() {
        if nearest_tagbody[scope].is_none() {
            nearest_tagbody[scope] = nearest_tagbody[scopes[scope].parent];
        }
    }
    let enclosing_tagbodies = |scope: usize| {
        std::iter::successors(nearest_tagbody[scope], |&idx| {
            nearest_tagbody[scopes[tagbody_entries[idx].scope].parent]
        })
    };

    // An unwind can replace a token while leaving its tagbody active.
    let mut token_safe = vec![true; tagbody_entries.len()];
    for edge in unwind_edges {
        for idx in enclosing_tagbodies(scope_at_pc[edge.target].unwrap()) {
            let token = tagbody_entries[idx].token;
            token_safe[idx] &= edge.clear_from > token && edge.condition_register != Some(token);
        }
    }

    // A direct jump is legal only within the exact tagbody scope and when
    // the token never escapes or gets overwritten anywhere inside it.
    let mut candidate_gos = vec![Vec::new(); tagbody_entries.len()];
    for (pc, inst) in code.instructions.iter().enumerate() {
        let Some(scope) = scope_at_pc[pc] else {
            continue;
        };
        if nearest_tagbody[scope].is_none() {
            continue;
        }
        let operands = inst.register_operands();
        for idx in enclosing_tagbodies(scope) {
            if !token_safe[idx] {
                continue;
            }
            let tagbody = &tagbody_entries[idx];
            if operands.writes.contains(tagbody.token) {
                token_safe[idx] = false;
            } else if let Instruction::Go { token, label, .. } = inst
                && *token == tagbody.token
            {
                if scope == tagbody.scope {
                    candidate_gos[idx].push((pc, *label));
                }
            } else if operands.reads.contains(tagbody.token)
                || side_table_registers(code, inst).any(|reg| reg == tagbody.token)
            {
                token_safe[idx] = false;
            }
        }
    }

    for (idx, tagbody) in tagbody_entries.iter().enumerate() {
        if !token_safe[idx] {
            continue;
        }
        let targets = &code.jump_tables[tagbody.jumps].targets;
        for &(go_pc, label) in &candidate_gos[idx] {
            let target = targets
                .get(label)
                .ok_or(BytecodeLiftError::InvalidControlFlow {
                    pc: go_pc,
                    reason: "GO label is outside the tagbody jump table",
                })?;
            local_go_target.insert(go_pc, (*target, tagbody.clear_from));
        }
    }

    Ok(local_go_target)
}

fn push_unique<T: Eq + Copy>(vec: &mut Vec<T>, item: T) {
    if !vec.contains(&item) {
        vec.push(item);
    }
}
