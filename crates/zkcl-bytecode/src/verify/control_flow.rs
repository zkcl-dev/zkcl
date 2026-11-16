use std::collections::VecDeque;

use super::{CodeVerifier, Result};
use crate::{HandlerTarget, Instruction, RestartTarget};

#[derive(Clone, Copy)]
enum Transfer {
    Next,
    Jump(usize),
    Branch(usize),
    Exit,
    EnterTarget(usize),
    EnterTagbody(usize),
    EnterProtect(usize),
    BeginCleanup,
    EnterHandlers(usize),
    EnterRestarts(usize),
    Push(ScopeKind),
    Pop(ScopeKind),
    AppendValues,
    Unbind(usize),
}

impl Transfer {
    fn of(instruction: &Instruction) -> Self {
        use Instruction::*;

        match instruction {
            Jump(target) => Self::Jump(*target),
            Unless(_, target)
            | IfPresent(_, target)
            | UnlessFixnum(_, target)
            | UnlessFixnumInRange(_, _, _, target)
            | WhenBound { jump: target, .. } => Self::Branch(*target),
            ReturnFrom { .. } | Throw { .. } | Go { .. } => Self::Exit,
            EnterBlock { resume, .. } | EnterCatch { resume, .. } => Self::EnterTarget(*resume),
            EnterTagbody { jumps, .. } => Self::EnterTagbody(*jumps),
            EnterProtect { cleanup, .. } => Self::EnterProtect(*cleanup),
            BeginCleanup => Self::BeginCleanup,
            EnterHandlers { table } => Self::EnterHandlers(*table),
            EnterRestarts { table, .. } => Self::EnterRestarts(*table),
            AssociateRestarts { .. } => Self::Push(ScopeKind::Association),
            BindSpecial { .. } | BindProgv { .. } => Self::Push(ScopeKind::Dynamic),
            SaveValues => Self::Push(ScopeKind::Values),
            EndCleanup => Self::Pop(ScopeKind::Cleanup),
            LeaveTarget => Self::Pop(ScopeKind::Target),
            LeaveHandlers => Self::Pop(ScopeKind::Handler),
            LeaveRestarts => Self::Pop(ScopeKind::Restart),
            LeaveRestartAssociation => Self::Pop(ScopeKind::Association),
            RestoreValues => Self::Pop(ScopeKind::Values),
            AppendValues => Self::AppendValues,
            Unbind(count) => Self::Unbind(*count),
            Load(..)
            | Constant(..)
            | Copy(..)
            | Clear(..)
            | SetValues(..)
            | BindValues(..)
            | ReadValue(..)
            | ReadCell { .. }
            | WriteCell { .. }
            | DefineFunction { .. }
            | ProclaimSpecial { .. }
            | ProclaimInline { .. }
            | SetVariableDocumentation { .. }
            | BoxLocal { .. }
            | ReadBox { .. }
            | WriteBox { .. }
            | Capture(..)
            | Closure { .. }
            | ExitToken(..)
            | OptionalArgument(..)
            | KeywordArgument(..)
            | RestArguments { .. }
            | CheckKeywords { .. }
            | DestructureNext { .. }
            | DestructureEnd { .. }
            | DestructureKeywords { .. }
            | ReleaseArguments
            | TryFixnumArithmetic { .. }
            | CallValues { .. }
            | Call { .. } => Self::Next,
        }
    }
}

#[inline]
pub(super) fn requires_scope_validation(instruction: &Instruction) -> bool {
    !matches!(
        Transfer::of(instruction),
        Transfer::Next | Transfer::Jump(_) | Transfer::Branch(_) | Transfer::Exit
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ScopeId(usize);

impl ScopeId {
    const EMPTY: Self = Self(0);
    const UNVISITED: Self = Self(usize::MAX);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Empty,
    Target,
    Protect {
        cleanup: usize,
        cleanup_scope: ScopeId,
    },
    Cleanup,
    Handler,
    Restart,
    Association,
    Dynamic,
    Values,
}

#[derive(Clone, Copy)]
struct Scope {
    kind: ScopeKind,
    parent: ScopeId,
}

#[derive(Default)]
pub(super) struct ControlFlowScratch {
    pending: VecDeque<(usize, ScopeId)>,
    visited: Vec<ScopeId>,
    scopes: Vec<Scope>,
}

impl ControlFlowScratch {
    // The operand pass checks every instruction, jump target, and side table index first.
    pub(super) fn validate(&mut self, verifier: &CodeVerifier<'_>) -> Result {
        let code = &verifier.code.instructions;
        self.visited.clear();
        self.visited.resize(code.len() + 1, ScopeId::UNVISITED);
        self.scopes.clear();
        self.push(ScopeId::EMPTY, ScopeKind::Empty);
        self.pending.push_back((0, ScopeId::EMPTY));
        self.drain(verifier)?;

        // Dead instructions still use the lexical scope of the preceding instruction.
        // Code after BeginCleanup belongs to the cleanup, including when it is unreachable.
        for pc in 0..code.len() {
            if self.visited[pc] != ScopeId::UNVISITED {
                continue;
            }
            let previous = self.visited[pc - 1];
            let scope = if matches!(code[pc - 1], Instruction::BeginCleanup) {
                let ScopeKind::Protect { cleanup_scope, .. } = self.scope(previous).kind else {
                    unreachable!("validated BeginCleanup is inside a protect scope");
                };
                cleanup_scope
            } else {
                previous
            };
            self.pending.push_back((pc, scope));
            self.drain(verifier)?;
        }
        Ok(())
    }

    fn drain(&mut self, verifier: &CodeVerifier<'_>) -> Result {
        let code = &verifier.code.instructions;
        while let Some((mut pc, mut scope)) = self.pending.pop_front() {
            loop {
                let invalid = |reason| verifier.invalid(Some(pc), reason);
                let previous = self.visited[pc];
                if previous != ScopeId::UNVISITED {
                    if previous != scope {
                        return Err(invalid("control-flow paths disagree on active scopes"));
                    }
                    break;
                }
                self.visited[pc] = scope;
                let Some(inst) = code.get(pc) else {
                    if scope != ScopeId::EMPTY {
                        return Err(invalid("function returns with active control scopes"));
                    }
                    break;
                };
                let Some(next_scope) = self.transfer(verifier, pc, scope, Transfer::of(inst))?
                else {
                    break;
                };
                pc += 1;
                scope = next_scope;
                if !self.pending.is_empty() {
                    self.pending.push_back((pc, scope));
                    break;
                }
            }
        }
        Ok(())
    }

    fn transfer(
        &mut self,
        verifier: &CodeVerifier<'_>,
        pc: usize,
        mut scope: ScopeId,
        transfer: Transfer,
    ) -> Result<Option<ScopeId>> {
        let invalid = |reason| verifier.invalid(Some(pc), reason);
        let pop = |current: Scope, kind| {
            (current.kind == kind)
                .then_some(current.parent)
                .ok_or_else(|| invalid("instruction closes the wrong control scope"))
        };
        match transfer {
            Transfer::Next => {}
            Transfer::Jump(target) => {
                self.pending.push_back((target, scope));
                return Ok(None);
            }
            Transfer::Branch(target) => self.pending.push_back((target, scope)),
            Transfer::Exit => return Ok(None),
            Transfer::EnterTarget(resume) => {
                self.pending.push_back((resume, scope));
                scope = self.push(scope, ScopeKind::Target);
            }
            Transfer::EnterTagbody(table) => {
                scope = self.push(scope, ScopeKind::Target);
                self.pending.extend(
                    verifier.code.jump_tables[table]
                        .targets
                        .iter()
                        .map(|&target| (target, scope)),
                );
            }
            Transfer::EnterProtect(cleanup) => {
                // Normal cleanup and unwind enter the same control scope.
                let cleanup_scope = self.push(scope, ScopeKind::Cleanup);
                self.pending.push_back((cleanup, cleanup_scope));
                scope = self.push(
                    scope,
                    ScopeKind::Protect {
                        cleanup,
                        cleanup_scope,
                    },
                );
            }
            Transfer::BeginCleanup => {
                let ScopeKind::Protect {
                    cleanup,
                    cleanup_scope,
                } = self.scope(scope).kind
                else {
                    return Err(invalid("cleanup begins outside a protect scope"));
                };
                self.pending.push_back((cleanup, cleanup_scope));
                return Ok(None);
            }
            Transfer::EnterHandlers(table) => {
                for handler in &verifier.code.handlers[table].handlers {
                    if let HandlerTarget::Case { pc, .. } = handler.target {
                        self.pending.push_back((pc, scope));
                    }
                }
                scope = self.push(scope, ScopeKind::Handler);
            }
            Transfer::EnterRestarts(table) => {
                for spec in &verifier.code.restarts[table].restarts {
                    if let RestartTarget::Case { pc, .. } = spec.target {
                        self.pending.push_back((pc, scope));
                    }
                }
                scope = self.push(scope, ScopeKind::Restart);
            }
            Transfer::Push(kind) => scope = self.push(scope, kind),
            Transfer::Pop(kind) => scope = pop(self.scope(scope), kind)?,
            Transfer::AppendValues => {
                if self.scope(scope).kind != ScopeKind::Values {
                    return Err(invalid(
                        "appending values requires an active saved-values scope",
                    ));
                }
            }
            Transfer::Unbind(count) => {
                for _ in 0..count {
                    scope = pop(self.scope(scope), ScopeKind::Dynamic)?;
                }
            }
        }
        Ok(Some(scope))
    }

    fn push(&mut self, parent: ScopeId, kind: ScopeKind) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        self.scopes.push(Scope { kind, parent });
        id
    }

    fn scope(&self, id: ScopeId) -> Scope {
        self.scopes[id.0]
    }
}
