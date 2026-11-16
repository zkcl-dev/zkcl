use super::super::debugging::FrameTransfer;
use super::{
    CleanupGuard, ControlScope, ControlStack, DeferredExpansionError, EvalError, ExecFrame,
    HandlerTarget, PendingExit, Register, ScopeKind, UnwindProtect, UnwindTarget, Values, Word,
};

impl PendingExit {
    fn destination(&self) -> Option<usize> {
        match self {
            Self::ExitPoint { scope_index, .. }
            | Self::HandlerCase { scope_index, .. }
            | Self::RestartCase { scope_index, .. }
            | Self::ResumableRestart { scope_index, .. } => Some(*scope_index),
            Self::DebugFrame { .. }
            | Self::NativeDebugReturn { .. }
            | Self::TaskCancelled { .. }
            | Self::Error { .. } => None,
        }
    }
}

impl<'rt> ControlStack<'rt> {
    pub(super) fn unwind(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        mut exit: PendingExit,
    ) -> Result<UnwindTarget, EvalError> {
        loop {
            if let PendingExit::NativeDebugReturn {
                frame,
                scope_boundary,
            } = exit
            {
                if self.scopes.len() <= scope_boundary {
                    if let ExecFrame::Bytecode(bytecode) = &frames[frame] {
                        // The cleanup entered the VM. Its recovered frame now
                        // carries the original continuation for these values.
                        let continuation =
                            FrameTransfer::return_values(bytecode.resume, bytecode.call_at);
                        frames.truncate(frame);
                        self.resumed_restart = Some(Box::new(continuation));
                        return Ok(UnwindTarget::Bytecode(frame.saturating_sub(1)));
                    }
                    frames.truncate(frame + 1);
                    return Ok(UnwindTarget::Native {
                        frame,
                        state: usize::MAX,
                    });
                }
            }
            if let PendingExit::DebugFrame {
                frame,
                scope_boundary,
                ..
            } = &exit
            {
                if self.scopes.len() <= *scope_boundary {
                    let frame = *frame;
                    let PendingExit::DebugFrame {
                        mut continuation, ..
                    } = exit
                    else {
                        unreachable!()
                    };
                    let ExecFrame::Bytecode(bytecode) = &frames[frame] else {
                        // Keep the selected activation until the machine entry
                        // returns, then restart through its caller's tail-call path.
                        frames.truncate(frame + 1);
                        self.native_debug_restart = Some((frame, continuation));
                        return Ok(UnwindTarget::Native { frame, state: 0 });
                    };
                    continuation.resume = bytecode.resume;
                    frames.truncate(frame);
                    self.resumed_restart = Some(continuation);
                    return Ok(UnwindTarget::Bytecode(frame.saturating_sub(1)));
                }
            }
            let Some(scope) = self.scopes.pop() else {
                break;
            };
            if exit.destination() == Some(self.scopes.len()) {
                return Ok(self.resume_at_destination(frames, values, scope, exit));
            }
            match scope.kind {
                ScopeKind::UnwindProtect(protected) => {
                    return Ok(self.start_cleanup(
                        frames,
                        values,
                        scope.frame,
                        protected,
                        Some(exit),
                    ));
                }
                // A transfer leaving cleanup supersedes the one that was saved there.
                ScopeKind::Cleanup { .. } => {}
                ScopeKind::SpecialBindings(bindings) => drop(bindings),
                ScopeKind::MacroExpansionCall {
                    at,
                    caller_frame_count,
                    error_handling,
                } => {
                    if let PendingExit::Error {
                        error,
                        condition,
                        defer_expansion_error,
                    } = exit
                    {
                        let condition = match condition {
                            Some(condition)
                                if defer_expansion_error
                                    && error_handling.defers_to_evaluation() =>
                            {
                                // Inner cleanups have run. The Lisp caller and
                                // its pending work remain active at this depth.
                                frames.truncate(caller_frame_count);
                                self.deferred_expansion_error =
                                    Some(DeferredExpansionError { error, condition });
                                return Ok(UnwindTarget::Bytecode(
                                    caller_frame_count.saturating_sub(1),
                                ));
                            }
                            condition => condition,
                        };
                        exit = PendingExit::Error {
                            error: error.during_macro_expansion(at),
                            condition,
                            defer_expansion_error,
                        };
                    }
                }
                ScopeKind::CancelShield(guard) => drop(guard),
                ScopeKind::CancelBoundary(boundary) => {
                    if matches!(exit, PendingExit::TaskCancelled { .. })
                        && boundary.signal.is_triggered()
                        && !self.has_outer_cancel_below(self.scopes.len())
                    {
                        boundary.signal.unwind();
                        let saved_frames = boundary.saved_frames;
                        drop(boundary);
                        let Some(ScopeKind::Continuation(resumed)) =
                            self.scopes.pop().map(|outer| outer.kind)
                        else {
                            unreachable!("cancel boundary follows its continuation");
                        };
                        frames.truncate(saved_frames);
                        self.resumed_restart = Some(resumed);
                        return Ok(UnwindTarget::Bytecode(saved_frames.saturating_sub(1)));
                    }
                    drop(boundary);
                }
                ScopeKind::ExitPoint { .. }
                | ScopeKind::HandlerCluster(_)
                | ScopeKind::RestartCluster(_)
                | ScopeKind::ResumableRestart { .. }
                | ScopeKind::Signaling { .. }
                | ScopeKind::SavedValues(_)
                | ScopeKind::Continuation(_)
                | ScopeKind::RestartAssociation(_)
                | ScopeKind::RestartSelection(_)
                | ScopeKind::InteractiveRestart(_)
                | ScopeKind::Macroexpanding(_)
                | ScopeKind::Instance(_)
                | ScopeKind::Evaluating(_) => {}
            }
        }
        match exit {
            PendingExit::NativeDebugReturn { .. } => {
                unreachable!("native frame return stops at its activation boundary")
            }
            PendingExit::DebugFrame { .. } => {
                unreachable!("debug frame transfer stops at its activation boundary")
            }
            PendingExit::TaskCancelled { error } => Err(error),
            PendingExit::Error {
                error, condition, ..
            } => {
                self.last_unhandled_condition = condition;
                Err(error)
            }
            PendingExit::ExitPoint { .. } => unreachable!("transfer has a matching exit point"),
            PendingExit::HandlerCase { .. } => unreachable!("transfer has a handler case target"),
            PendingExit::RestartCase { .. } => unreachable!("transfer has a restart case target"),
            PendingExit::ResumableRestart { .. } => {
                unreachable!("restart has a matching resumable scope")
            }
        }
    }

    fn resume_at_destination(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        scope: ControlScope<'rt>,
        exit: PendingExit,
    ) -> UnwindTarget {
        let frame = scope.frame;
        match (exit, &scope.kind) {
            (
                PendingExit::ExitPoint {
                    label: Some(label), ..
                },
                ScopeKind::ExitPoint { resume, clear, .. },
            ) => {
                let pc = match &frames[frame] {
                    ExecFrame::Bytecode(bytecode) => {
                        let canonical_pc = bytecode.vm_code.jump_tables[*resume].targets[label];
                        bytecode.vm_code.vm_target(canonical_pc)
                    }
                    ExecFrame::Native(_) => *resume + label,
                };
                let target = Self::resume(frames, frame, pc, *clear, true);
                // GO resumes inside the same active TAGBODY.
                self.scopes.push(scope);
                target
            }
            (
                PendingExit::ExitPoint { label: None, .. },
                ScopeKind::ExitPoint { resume, clear, .. },
            ) => Self::resume(frames, frame, *resume, *clear, true),
            (PendingExit::HandlerCase { binding, .. }, ScopeKind::HandlerCluster(cluster)) => {
                let HandlerTarget::Case {
                    pc,
                    condition,
                    clear,
                } = cluster.bindings(&frames[frame])[binding].target
                else {
                    unreachable!("handler case target is a case binding");
                };
                let pc = match &frames[frame] {
                    ExecFrame::Bytecode(frame) => frame.vm_code.vm_target(pc),
                    ExecFrame::Native(_) => pc,
                };
                let target = Self::resume(frames, frame, pc, clear, true);
                let slots = frames[frame].slots();
                if condition < slots.len() {
                    values.bind(slots, condition..condition + 1);
                }
                target
            }
            (PendingExit::RestartCase { pc, clear, .. }, ScopeKind::RestartCluster(_)) => {
                Self::resume(frames, frame, pc, clear, true)
            }
            (
                PendingExit::ResumableRestart { binding, .. },
                ScopeKind::ResumableRestart { saved_frames, .. },
            ) => {
                let Some(ScopeKind::Continuation(mut resumed)) =
                    self.scopes.pop().map(|outer| outer.kind)
                else {
                    unreachable!("resumable restart scope follows its continuation");
                };
                frames.truncate(*saved_frames);
                resumed.restart_invoked(binding);
                self.resumed_restart = Some(resumed);
                // The continuation belongs to the dispatcher that established the restart.
                // Propagate across any native frames still returning on the Rust stack.
                UnwindTarget::Bytecode(saved_frames.saturating_sub(1))
            }
            _ => unreachable!("the destination of an exit must have the corresponding scope kind"),
        }
    }

    pub(super) fn start_cleanup(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        frame: usize,
        protected: UnwindProtect<'rt>,
        exit: Option<PendingExit>,
    ) -> UnwindTarget {
        let target = Self::resume(
            frames,
            frame,
            protected.cleanup,
            protected.clear,
            exit.is_some(),
        );
        let guard = CleanupGuard::enter(self.scope_depths(), protected.runtime_depth);
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::Cleanup {
                saved: values.take(),
                exit,
                guard,
            },
        });
        target
    }

    fn resume(
        frames: &mut Vec<ExecFrame<'_>>,
        frame: usize,
        pc: usize,
        clear: Register,
        record_native_pending: bool,
    ) -> UnwindTarget {
        frames.truncate(frame + 1);
        match &mut frames[frame] {
            ExecFrame::Bytecode(frame_ref) => {
                // The jump bypasses the compiler's usual releases for the abandoned forms.
                frame_ref.clear(clear..frame_ref.code);
                frame_ref.pc = pc;
                UnwindTarget::Bytecode(frame)
            }
            ExecFrame::Native(anchor) => {
                if clear < anchor.slots_len {
                    let slots = anchor.slots();
                    for reg in clear..anchor.slots_len {
                        slots.set(reg, Word::NIL);
                    }
                }
                if record_native_pending {
                    anchor.pending_state = Some(pc);
                }
                UnwindTarget::Native { frame, state: pc }
            }
        }
    }
}
