use std::cell::Cell;
use std::rc::Rc;

use rustc_hash::FxHashSet;

use super::evaluate::Evaluation;
use super::instances;
use super::restart::{
    ActiveRestart, InteractiveRestartInvocation, RestartCursor, RestartSelection,
};
use super::task_scheduler::{BoundarySignal, TaskCancellation};
use super::{Continuation, visit_proper_list};
use crate::condition::{self, Fault, Test};
use crate::heap::{Heap, RootFrame, RootSlot, Word};
use crate::vm::{
    ExecFrame, Handler, HandlerTarget, Macroexpansion, Register, RestartSpec, Signal,
    UnhandledSignal, Values,
};
use crate::{EvalError, Runtime};

mod multiple_values;
mod unwind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnwindTarget {
    Bytecode(usize),
    Native { frame: usize, state: usize },
}

pub(crate) enum TransferOutcome {
    ResumeFrame(UnwindTarget),
    InvalidExit(EvalError),
}

pub(crate) enum ExitPoint {
    Block(Word),
    Catch(Register),
    Tagbody(Word, usize),
}

pub(crate) enum NonLocalTransfer {
    ReturnFrom(Word),
    Throw(Word),
    Go(Word, usize),
}

enum PendingExit {
    ExitPoint {
        scope_index: usize,
        label: Option<usize>,
    },
    HandlerCase {
        scope_index: usize,
        binding: usize,
    },
    RestartCase {
        scope_index: usize,
        pc: usize,
        clear: Register,
    },
    ResumableRestart {
        scope_index: usize,
        binding: usize,
    },
    DebugFrame {
        frame: usize,
        scope_boundary: usize,
        continuation: Box<super::debugging::FrameTransfer>,
    },
    NativeDebugReturn {
        frame: usize,
        scope_boundary: usize,
    },
    TaskCancelled {
        error: EvalError,
    },
    Error {
        error: EvalError,
        condition: Option<RootSlot>,
        defer_expansion_error: bool,
    },
}

pub(super) struct DeferredExpansionError {
    pub(super) error: EvalError,
    pub(super) condition: RootSlot,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ExpansionErrorHandling {
    /// An unhandled error leaves the macro expansion.
    Propagate,
    /// Search active handlers now. If none intervenes, signal again when the form runs.
    DeferUnhandledToEvaluation,
    /// For an unassociated ERROR, search only handlers inside the expander now.
    /// The evaluated form will install its own handlers when it runs.
    DeferToFormHandlers,
}

impl ExpansionErrorHandling {
    pub(super) fn defers_to_evaluation(self) -> bool {
        matches!(
            self,
            Self::DeferUnhandledToEvaluation | Self::DeferToFormHandlers
        )
    }
}

enum ScopeKind<'rt> {
    ExitPoint {
        point: ExitPoint,
        active: bool,
        resume: usize,
        clear: Register,
        native_pc: Option<usize>,
    },
    UnwindProtect(UnwindProtect<'rt>),
    Cleanup {
        saved: Values,
        exit: Option<PendingExit>,
        guard: CleanupGuard<'rt>,
    },
    SavedValues(Values),
    Continuation(Box<dyn Continuation + 'rt>),
    SpecialBindings(SpecialBindings<'rt>),
    HandlerCluster(HandlerCluster),
    Signaling {
        search: HandlerSearch,
        masked_from: Option<usize>,
    },
    RestartCluster(usize),
    ResumableRestart {
        saved_frames: usize,
        restart: RootFrame,
        arities: Box<[usize]>,
        enclosing: Option<usize>,
    },
    RestartAssociation(RootFrame),
    RestartSelection(RestartSelection),
    InteractiveRestart(InteractiveRestartInvocation),
    Macroexpanding(Macroexpansion),
    MacroExpansionCall {
        at: usize,
        caller_frame_count: usize,
        error_handling: ExpansionErrorHandling,
    },
    Evaluating(Box<Evaluation>),
    Instance(instances::Operation),
    CancelShield(CancelShieldGuard),
    CancelBoundary(CancelBoundary),
}

struct ControlScope<'rt> {
    frame: usize,
    kind: ScopeKind<'rt>,
}

pub(crate) enum ContinuationScope {
    CancelShield,
    CancelBoundary(Rc<BoundarySignal>),
}

struct CancelBoundary {
    saved_frames: usize,
    signal: Rc<BoundarySignal>,
}

impl Drop for CancelBoundary {
    fn drop(&mut self) {
        self.signal.disarm();
    }
}

struct UnwindProtect<'rt> {
    cleanup: usize,
    clear: Register,
    native_pc: Option<usize>,
    runtime_depth: &'rt Cell<usize>,
}

struct ScopeDepths {
    cleanup: Cell<usize>,
    shield: Cell<usize>,
    active: Cell<bool>,
}

struct CancelShieldGuard(Rc<ScopeDepths>);

impl Drop for CancelShieldGuard {
    fn drop(&mut self) {
        self.0.shield.update(|n| n - 1);
    }
}

struct CleanupGuard<'rt> {
    depths: Rc<ScopeDepths>,
    runtime_depth: &'rt Cell<usize>,
}

impl<'rt> CleanupGuard<'rt> {
    fn enter(depths: Rc<ScopeDepths>, runtime_depth: &'rt Cell<usize>) -> Self {
        depths.cleanup.update(|n| n + 1);
        runtime_depth.update(|n| n + 1);
        Self {
            depths,
            runtime_depth,
        }
    }
}

impl Drop for CleanupGuard<'_> {
    fn drop(&mut self) {
        self.depths.cleanup.update(|n| n - 1);
        if self.depths.active.get() {
            self.runtime_depth.update(|n| n.saturating_sub(1));
        }
    }
}

enum HandlerCluster {
    Bytecode(usize),
    Native {
        handlers: Vec<Handler>,
        table: Option<usize>,
    },
}

impl HandlerCluster {
    fn bindings<'a>(&'a self, frame: &'a ExecFrame<'_>) -> &'a [Handler] {
        match self {
            Self::Bytecode(table) => &frame.bytecode().vm_code.handlers[*table].handlers,
            Self::Native { handlers, .. } => handlers,
        }
    }
}

pub(super) enum SignalPhase {
    BeforeHandlers,
    Searching,
    Tested,
    HostDebuggerEntry { check_stop: bool },
    DebuggerReturned,
}

pub(super) struct HandlerSearch {
    pub(super) signal: Signal,
    pub(super) phase: SignalPhase,
    next_binding: Option<(usize, usize)>,
    outermost_scope: usize,
    pub(super) debug_pause: Option<super::debugging::Pause>,
    pub(super) paused_values: Option<Box<Values>>,
}

impl HandlerSearch {
    pub(super) fn for_debugger(signal: Signal) -> Self {
        Self {
            signal,
            phase: SignalPhase::DebuggerReturned,
            next_binding: None,
            outermost_scope: 0,
            debug_pause: None,
            paused_values: None,
        }
    }
}

pub(super) struct MatchingHandler {
    pub(super) cluster_scope: usize,
    pub(super) frame: usize,
    pub(super) target: HandlerTarget,
    binding: usize,
}

pub(super) struct SpecialBindings<'rt> {
    heap: &'rt Heap,
    previous: RootFrame,
    active: bool,
}

impl<'rt> SpecialBindings<'rt> {
    pub(super) fn new(
        heap: &'rt Heap,
        bindings: impl ExactSizeIterator<Item = (Word, Word)>,
    ) -> Self {
        let previous = heap.root_frame(bindings.len() * 2);
        // No Lisp allocation before the raw values become heap cells or registered slots.
        for (i, (symbol, value)) in bindings.enumerate() {
            previous.set(2 * i, symbol);
            previous.set(2 * i + 1, heap.bind_tls_symbol(symbol, value));
        }
        Self {
            heap,
            previous,
            active: true,
        }
    }

    pub(super) fn new_suspended(
        heap: &'rt Heap,
        bindings: impl ExactSizeIterator<Item = (Word, Word)>,
    ) -> Self {
        let previous = heap.root_frame(bindings.len() * 2);
        for (i, (symbol, value)) in bindings.enumerate() {
            previous.set(2 * i, symbol);
            previous.set(2 * i + 1, value);
        }
        Self {
            heap,
            previous,
            active: false,
        }
    }

    fn suspend(&mut self) {
        if !self.active {
            return;
        }
        for i in (0..self.previous.len() / 2).rev() {
            let symbol = self.previous.get(2 * i);
            let outer = self.previous.get(2 * i + 1);
            let inner = self.heap.swap_tls_symbol(symbol, outer);
            self.previous.set(2 * i + 1, inner);
        }
        self.active = false;
    }

    fn resume(&mut self) {
        if self.active {
            return;
        }
        for i in 0..self.previous.len() / 2 {
            let symbol = self.previous.get(2 * i);
            let inner = self.previous.get(2 * i + 1);
            let outer = self.heap.swap_tls_symbol(symbol, inner);
            self.previous.set(2 * i + 1, outer);
        }
        self.active = true;
    }
}

impl Drop for SpecialBindings<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        for i in (0..self.previous.len() / 2).rev() {
            self.heap
                .restore_tls_symbol(self.previous.get(2 * i), self.previous.get(2 * i + 1));
        }
    }
}

#[derive(Default)]
pub(crate) struct ControlStack<'rt> {
    scopes: Vec<ControlScope<'rt>>,
    resumed_restart: Option<Box<dyn Continuation + 'rt>>,
    native_debug_restart: Option<(usize, Box<super::debugging::FrameTransfer>)>,
    deferred_expansion_error: Option<DeferredExpansionError>,
    last_unhandled_condition: Option<RootSlot>,
    // Guards can move with a suspended stack. Allocate their shared counters
    // only when entering a cleanup or cancellation shield.
    depths: Option<Rc<ScopeDepths>>,
    cancellation: Rc<TaskCancellation>,
}

impl Drop for ControlStack<'_> {
    fn drop(&mut self) {
        // In particular, restore inner before outer bindings if a Rust callback panics.
        while let Some(scope) = self.scopes.pop() {
            drop(scope);
        }
    }
}

impl<'rt> ControlStack<'rt> {
    pub(super) fn enter_macro_expansion_call(
        &mut self,
        caller_frame_count: usize,
        at: usize,
        error_handling: ExpansionErrorHandling,
    ) {
        self.scopes.push(ControlScope {
            frame: caller_frame_count.saturating_sub(1),
            kind: ScopeKind::MacroExpansionCall {
                at,
                caller_frame_count,
                error_handling,
            },
        });
    }

    pub(super) fn leave_macro_expansion_call(&mut self) {
        assert!(matches!(
            self.scopes.pop().map(|scope| scope.kind),
            Some(ScopeKind::MacroExpansionCall { .. })
        ));
    }

    pub(super) fn can_defer_expansion_error(&self) -> bool {
        self.scopes.iter().rev().any(|scope| {
            matches!(
                scope.kind,
                ScopeKind::MacroExpansionCall { error_handling, .. }
                    if error_handling.defers_to_evaluation()
            )
        })
    }

    pub(super) fn take_deferred_expansion_error(&mut self) -> Option<DeferredExpansionError> {
        self.deferred_expansion_error.take()
    }

    #[inline(always)]
    pub(super) fn has_frame_scopes(&self, frame: usize) -> bool {
        self.scopes.last().is_some_and(|scope| scope.frame >= frame)
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.scopes.len()
    }

    pub(crate) fn truncate_frame_scopes(&mut self, frame: usize, min_len: usize) {
        while self.scopes.len() > min_len
            && self.scopes.last().is_some_and(|scope| scope.frame >= frame)
        {
            self.scopes.pop();
        }
    }

    pub(super) fn pause_continuation(
        &mut self,
        frame: usize,
        continuation: Box<dyn Continuation + 'rt>,
    ) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::Continuation(continuation),
        });
    }

    pub(super) fn resume_continuation(&mut self) -> Box<dyn Continuation + 'rt> {
        if matches!(
            self.scopes.last().map(|scope| &scope.kind),
            Some(
                ScopeKind::ResumableRestart { .. }
                    | ScopeKind::CancelShield(_)
                    | ScopeKind::CancelBoundary(_)
            )
        ) {
            self.scopes.pop();
        }
        match self.scopes.pop().unwrap().kind {
            ScopeKind::Continuation(continuation) => continuation,
            _ => unreachable!("return from a continuation callback has an active continuation"),
        }
    }

    pub(super) fn push_continuation_scope(
        &mut self,
        saved_frames: usize,
        scope: ContinuationScope,
    ) {
        let frame = saved_frames.saturating_sub(1);
        match scope {
            ContinuationScope::CancelShield => self.enter_cancel_shield(frame),
            ContinuationScope::CancelBoundary(signal) => {
                signal.arm();
                self.scopes.push(ControlScope {
                    frame,
                    kind: ScopeKind::CancelBoundary(CancelBoundary {
                        saved_frames,
                        signal,
                    }),
                });
            }
        }
    }

    fn scope_depths(&mut self) -> Rc<ScopeDepths> {
        Rc::clone(self.depths.get_or_insert_with(|| {
            Rc::new(ScopeDepths {
                cleanup: Cell::new(0),
                shield: Cell::new(0),
                active: Cell::new(true),
            })
        }))
    }

    pub(super) fn enter_cancel_shield(&mut self, frame: usize) {
        let depths = self.scope_depths();
        depths.shield.update(|n| n + 1);
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::CancelShield(CancelShieldGuard(depths)),
        });
    }

    pub(super) fn defers_host_cancellation(&self) -> bool {
        self.depths
            .as_ref()
            .is_some_and(|depths| depths.shield.get() != 0 || depths.cleanup.get() != 0)
    }

    pub(super) fn cancellation_handle(&self) -> Rc<TaskCancellation> {
        Rc::clone(&self.cancellation)
    }

    pub(super) fn can_deliver_task_cancel(&self) -> bool {
        self.cancellation.is_pending()
            && (!self.defers_host_cancellation()
                || (self.cancellation.has_triggered_boundary()
                    && self.has_outer_cancel_below(self.scopes.len())))
    }

    pub(super) fn await_has_deadline(&self) -> bool {
        if !self.cancellation.has_deadline_in_scope() {
            return false;
        }
        if !self.defers_host_cancellation() {
            return true;
        }
        for scope in self.scopes.iter().rev() {
            match &scope.kind {
                ScopeKind::CancelShield(_) | ScopeKind::Cleanup { .. } => return false,
                ScopeKind::CancelBoundary(b) if b.signal.has_deadline() => return true,
                _ => {}
            }
        }
        false
    }

    pub(super) fn has_outer_cancel_below(&self, boundary_index: usize) -> bool {
        // Cleanup and cancellation shields defer requests from their enclosing
        // scopes. Timeouts and groups created within them can still unwind locally.
        for scope in self.scopes[..boundary_index].iter().rev() {
            match &scope.kind {
                ScopeKind::CancelShield(_) | ScopeKind::Cleanup { .. } => return false,
                ScopeKind::CancelBoundary(b) if b.signal.is_triggered() => return true,
                _ => {}
            }
        }
        self.cancellation.requested()
    }

    pub(super) fn unwinding_bypasses_group_scope(&self, boundary: &Rc<BoundarySignal>) -> bool {
        let mut boundary_index = None;
        for (index, scope) in self.scopes.iter().enumerate().rev() {
            match &scope.kind {
                ScopeKind::Cleanup { exit: None, .. } => return false,
                ScopeKind::Cleanup {
                    exit: Some(exit), ..
                } => {
                    return match exit {
                        PendingExit::TaskCancelled { .. } => {
                            if !boundary.is_triggered() {
                                return true;
                            }
                            let idx = boundary_index.or_else(|| {
                                self.scopes[..index].iter().rposition(|outer| {
                                    matches!(&outer.kind, ScopeKind::CancelBoundary(b) if Rc::ptr_eq(&b.signal, boundary))
                                })
                            });
                            idx.is_some_and(|i| self.has_outer_cancel_below(i))
                        }
                        _ => true,
                    };
                }
                ScopeKind::CancelBoundary(b) if Rc::ptr_eq(&b.signal, boundary) => {
                    boundary_index = Some(index);
                }
                _ => {}
            }
        }
        false
    }

    pub(super) fn snapshot_active_specials(&self, heap: &'rt Heap) -> Vec<(Word, Word)> {
        let mut seen = FxHashSet::default();
        let mut bindings = Vec::new();
        for scope in &self.scopes {
            let ScopeKind::SpecialBindings(group) = &scope.kind else {
                continue;
            };
            for i in 0..group.previous.len() / 2 {
                let symbol = group.previous.get(2 * i);
                if seen.insert(symbol.raw()) {
                    let value = heap.symbol_cell(symbol, false).unwrap_or(Word::UNBOUND);
                    bindings.push((symbol, value));
                }
            }
        }
        bindings
    }

    pub(super) fn install_suspended_specials(
        &mut self,
        heap: &'rt Heap,
        bindings: Vec<(Word, Word)>,
    ) {
        if bindings.is_empty() {
            return;
        }
        self.scopes.push(ControlScope {
            frame: 0,
            kind: ScopeKind::SpecialBindings(SpecialBindings::new_suspended(
                heap,
                bindings.into_iter(),
            )),
        });
    }

    pub(super) fn suspend_task(&mut self, runtime: &Runtime) {
        for scope in self.scopes.iter_mut().rev() {
            if let ScopeKind::SpecialBindings(bindings) = &mut scope.kind {
                bindings.suspend();
            }
        }
        if let Some(depths) = &self.depths {
            depths.active.set(false);
        }
        runtime.cleanup_depth.set(0);
    }

    pub(super) fn resume_task(&mut self, runtime: &Runtime) {
        let cleanup_depth = self.depths.as_ref().map_or(0, |depths| {
            depths.active.set(true);
            depths.cleanup.get()
        });
        runtime.cleanup_depth.set(cleanup_depth);
        for scope in &mut self.scopes {
            if let ScopeKind::SpecialBindings(bindings) = &mut scope.kind {
                bindings.resume();
            }
        }
    }

    pub(super) fn take_unhandled_condition(&mut self) -> Option<RootSlot> {
        self.last_unhandled_condition.take()
    }

    pub(super) fn pause_instance(&mut self, frame: usize, operation: instances::Operation) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::Instance(operation),
        });
    }

    pub(super) fn resume_instance(&mut self) -> instances::Operation {
        match self.scopes.pop().unwrap().kind {
            ScopeKind::Instance(operation) => operation,
            _ => unreachable!("an instance callback has an active operation"),
        }
    }

    pub(super) fn pause_evaluation(&mut self, frame: usize, evaluation: Box<Evaluation>) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::Evaluating(evaluation),
        });
    }

    pub(super) fn resume_evaluation(&mut self) -> Box<Evaluation> {
        match self.scopes.pop().unwrap().kind {
            ScopeKind::Evaluating(evaluation) => evaluation,
            _ => unreachable!("return from evaluated code has an active evaluation"),
        }
    }

    pub(super) fn pause_macroexpansion(&mut self, frame: usize, expansion: Macroexpansion) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::Macroexpanding(expansion),
        });
    }

    pub(super) fn resume_macroexpansion(&mut self) -> Macroexpansion {
        match self.scopes.pop().unwrap().kind {
            ScopeKind::Macroexpanding(expansion) => expansion,
            _ => unreachable!("return from a macro expander has an active expansion"),
        }
    }

    pub(crate) fn enter_restarts(&mut self, frame: usize, table: usize) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::RestartCluster(table),
        });
    }

    /// Keep the continuation immediately below its restart so both the
    /// normal return path and a non-local transfer can resume it.
    pub(super) fn enter_resumable_restart(
        &mut self,
        runtime: &Runtime,
        saved_frames: usize,
        restart: Word,
        enclosing: Option<usize>,
        continuation: Box<dyn Continuation + 'rt>,
    ) -> usize {
        let roots = runtime.root_frame(1);
        roots.set(0, restart);
        self.enter_resumable_restarts(
            saved_frames,
            roots,
            vec![0].into_boxed_slice(),
            enclosing,
            continuation,
        )
    }

    pub(super) fn enter_resumable_restarts(
        &mut self,
        saved_frames: usize,
        restart: RootFrame,
        arities: Box<[usize]>,
        enclosing: Option<usize>,
        continuation: Box<dyn Continuation + 'rt>,
    ) -> usize {
        assert_eq!(restart.len(), arities.len());
        let frame = saved_frames.saturating_sub(1);
        self.pause_continuation(frame, continuation);
        let scope_index = self.scopes.len();
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::ResumableRestart {
                saved_frames,
                restart,
                arities,
                enclosing,
            },
        });
        scope_index
    }

    pub(super) fn resumable_restart_object(&self, candidate: ActiveRestart) -> Word {
        match &self.scopes[candidate.scope_index].kind {
            ScopeKind::ResumableRestart { restart, .. } => restart.get(candidate.binding),
            _ => unreachable!("resumable restart candidate points at a resumable scope"),
        }
    }

    pub(super) fn resumable_restart_arity(&self, candidate: ActiveRestart) -> usize {
        match &self.scopes[candidate.scope_index].kind {
            ScopeKind::ResumableRestart { arities, .. } => arities[candidate.binding],
            _ => unreachable!("resumable restart candidate points at a resumable scope"),
        }
    }

    pub(super) fn take_resumed_restart(&mut self) -> Option<Box<dyn Continuation + 'rt>> {
        self.resumed_restart.take()
    }

    pub(crate) fn take_native_debug_restart(
        &mut self,
        runtime: &Runtime,
        frame: usize,
    ) -> Option<Result<crate::native::NativeTailCall, Fault>> {
        if self.native_debug_restart.as_ref()?.0 != frame {
            return None;
        }
        let (_, continuation) = self.native_debug_restart.take().unwrap();
        Some(continuation.native_tail_call(runtime))
    }

    pub(crate) fn leave_restarts(&mut self) {
        assert!(matches!(
            self.scopes.pop().unwrap().kind,
            ScopeKind::RestartCluster(_)
        ));
    }

    pub(super) fn innermost_restart_cluster(&self) -> Option<usize> {
        self.scopes
            .iter()
            .rposition(|scope| matches!(scope.kind, ScopeKind::RestartCluster(_)))
    }

    pub(super) fn associate_restarts(
        &mut self,
        runtime: &Runtime,
        frame: usize,
        condition: Word,
        restarts: &[Word],
    ) {
        let roots = runtime.root_frame(1 + restarts.len());
        roots.set(0, condition);
        for (i, &restart) in restarts.iter().enumerate() {
            roots.set(i + 1, restart);
        }
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::RestartAssociation(roots),
        });
    }

    pub(super) fn leave_restart_association(&mut self) {
        assert!(matches!(
            self.scopes.pop().unwrap().kind,
            ScopeKind::RestartAssociation(_)
        ));
    }

    pub(super) fn next_restart(
        &self,
        frames: &[ExecFrame<'_>],
        cursor: &mut RestartCursor,
    ) -> Option<ActiveRestart> {
        while cursor.end > 0 {
            let scope_index = cursor.end - 1;
            let scope = &self.scopes[scope_index];
            let count = match &scope.kind {
                ScopeKind::RestartCluster(table) => frames[scope.frame].bytecode().vm_code.restarts
                    [*table]
                    .restarts
                    .len(),
                ScopeKind::ResumableRestart { restart, .. } => restart.len(),
                _ => 0,
            };
            if cursor.binding < count {
                let binding = cursor.binding;
                cursor.binding += 1;
                return Some(ActiveRestart {
                    scope_index,
                    binding,
                });
            }
            cursor.end -= 1;
            cursor.binding = 0;
        }
        None
    }

    pub(super) fn restart_binding(
        &self,
        frames: &[ExecFrame<'_>],
        restart: ActiveRestart,
    ) -> Option<(usize, RestartSpec)> {
        let scope = &self.scopes[restart.scope_index];
        match scope.kind {
            ScopeKind::RestartCluster(table) => Some((
                scope.frame,
                frames[scope.frame].bytecode().vm_code.restarts[table].restarts[restart.binding],
            )),
            ScopeKind::ResumableRestart { .. } => None,
            _ => unreachable!("active restart has a restart scope"),
        }
    }

    pub(super) fn restart_applies(
        &self,
        candidate: ActiveRestart,
        object: Word,
        condition: Word,
    ) -> bool {
        if condition == Word::NIL {
            return true;
        }
        let mut associated = false;
        // Associations can only be established after the restart.
        for scope in &self.scopes[candidate.scope_index + 1..] {
            let binding = match &scope.kind {
                ScopeKind::RestartAssociation(roots)
                    if (1..roots.len()).any(|i| roots.get(i) == object) =>
                {
                    roots.get(0)
                }
                ScopeKind::Signaling { search, .. }
                    if search.signal.associated_restart_scope.is_some_and(|group| {
                        self.signal_associates_restart_scope(group, candidate.scope_index)
                    }) =>
                {
                    search.signal.condition.get(0)
                }
                _ => continue,
            };
            if binding == condition {
                return true;
            }
            associated = true;
        }
        !associated
    }

    /// A resumable restart can link its condition to the enclosing restart cluster.
    fn signal_associates_restart_scope(
        &self,
        associated_scope: usize,
        restart_scope: usize,
    ) -> bool {
        let mut scope = associated_scope;
        loop {
            if scope == restart_scope {
                return true;
            }
            let ScopeKind::ResumableRestart {
                enclosing: Some(enclosing),
                ..
            } = self.scopes[scope].kind
            else {
                return false;
            };
            scope = enclosing;
        }
    }

    pub(super) fn pause_restart_selection(&mut self, frame: usize, selection: RestartSelection) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::RestartSelection(selection),
        });
    }

    pub(super) fn restart_test_is_active(&self, candidate: ActiveRestart) -> bool {
        self.scopes.iter().any(|scope| {
            matches!(&scope.kind, ScopeKind::RestartSelection(selection) if selection.is_testing(candidate))
        })
    }

    pub(super) fn resume_restart_selection(&mut self, passed: bool) -> RestartSelection {
        let ScopeKind::RestartSelection(mut selection) = self.scopes.pop().unwrap().kind else {
            unreachable!("a test function returns to its restart selection");
        };
        selection.record_test_result(passed);
        selection
    }

    pub(super) fn pause_interactive_restart(
        &mut self,
        frame: usize,
        invocation: InteractiveRestartInvocation,
    ) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::InteractiveRestart(invocation),
        });
    }

    pub(super) fn resume_interactive_restart(&mut self) -> InteractiveRestartInvocation {
        let ScopeKind::InteractiveRestart(invocation) = self.scopes.pop().unwrap().kind else {
            unreachable!("interactive function returns to its restart invocation");
        };
        invocation
    }

    pub(super) fn restart_case(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        scope_index: usize,
        pc: usize,
        clear: Register,
    ) -> Result<UnwindTarget, EvalError> {
        self.abandon_exit_points_above(scope_index);
        self.unwind(
            frames,
            values,
            PendingExit::RestartCase {
                scope_index,
                pc,
                clear,
            },
        )
    }

    pub(super) fn resume_restart(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        scope_index: usize,
        binding: usize,
    ) -> Result<UnwindTarget, EvalError> {
        self.abandon_exit_points_above(scope_index);
        self.unwind(
            frames,
            values,
            PendingExit::ResumableRestart {
                scope_index,
                binding,
            },
        )
    }

    pub(crate) fn enter_handlers(&mut self, frame: usize, handlers: Vec<Handler>) {
        self.enter_native_handlers(frame, handlers, None);
    }

    pub(crate) fn enter_native_handlers(
        &mut self,
        frame: usize,
        handlers: Vec<Handler>,
        table: Option<usize>,
    ) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::HandlerCluster(HandlerCluster::Native { handlers, table }),
        });
    }

    pub(super) fn enter_bytecode_handlers(&mut self, frame: usize, table: usize) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::HandlerCluster(HandlerCluster::Bytecode(table)),
        });
    }

    pub(crate) fn leave_handlers(&mut self) {
        assert!(matches!(
            self.scopes.pop().unwrap().kind,
            ScopeKind::HandlerCluster(_)
        ));
    }

    pub(super) fn has_handlers(&self) -> bool {
        self.scopes
            .iter()
            .any(|scope| matches!(scope.kind, ScopeKind::HandlerCluster(_)))
    }

    pub(super) fn search_handlers(&self, signal: Signal) -> HandlerSearch {
        // The evaluated form can install its own handler when its protected
        // expression runs. An unassociated ERROR from expanding that expression
        // first goes to the expander's own handlers, then becomes a runtime error
        // at the call site. An active restart can still handle a CERROR in place.
        let outermost_scope = if signal.associated_restart_scope.is_none()
            && matches!(signal.unhandled, Some(UnhandledSignal::LispError))
        {
            self.scopes
                .iter()
                .enumerate()
                .rev()
                .find_map(|(scope_index, scope)| match scope.kind {
                    ScopeKind::MacroExpansionCall { error_handling, .. } => {
                        Some((scope_index, error_handling))
                    }
                    _ => None,
                })
                .filter(|&(_, error_handling)| {
                    error_handling == ExpansionErrorHandling::DeferToFormHandlers
                })
                .map_or(0, |(scope_index, _)| scope_index + 1)
        } else {
            0
        };
        HandlerSearch {
            signal,
            next_binding: self
                .scopes
                .len()
                .checked_sub(1)
                .map(|scope_index| (scope_index, 0)),
            outermost_scope,
            phase: SignalPhase::BeforeHandlers,
            debug_pause: None,
            paused_values: None,
        }
    }

    pub(super) fn next_handler(
        &self,
        runtime: &Runtime,
        frames: &[ExecFrame<'_>],
        search: &mut HandlerSearch,
    ) -> Option<(MatchingHandler, Option<Word>)> {
        let (start, first) = search.next_binding.take()?;
        let mut end = start + 1;
        while end > search.outermost_scope {
            end -= 1;
            let scope = &self.scopes[end];
            let handlers = match &scope.kind {
                ScopeKind::HandlerCluster(cluster) => cluster.bindings(&frames[scope.frame]),
                ScopeKind::Signaling {
                    masked_from: Some(cluster),
                    ..
                } => {
                    // Skip the invoked cluster and all handlers that were newer at entry.
                    end = *cluster;
                    continue;
                }
                _ => continue,
            };
            let offset = if end == start { first } else { 0 };
            for (index, handler) in handlers.iter().enumerate().skip(offset) {
                let specifier = match handler.test {
                    Test::Any => None,
                    Test::Never => continue,
                    Test::Class(class)
                        if condition::matches_builtin(
                            runtime,
                            search.signal.condition.get(0),
                            class,
                        ) =>
                    {
                        None
                    }
                    Test::Class(_) => continue,
                    Test::Named(index) => Some(frames[scope.frame].constant(runtime, index)),
                };
                search.next_binding = Some((end, index + 1));
                return Some((
                    MatchingHandler {
                        cluster_scope: end,
                        frame: scope.frame,
                        target: handler.target,
                        binding: index,
                    },
                    specifier,
                ));
            }
        }
        None
    }

    pub(super) fn tested_handler(
        &self,
        frames: &[ExecFrame<'_>],
        search: &HandlerSearch,
    ) -> MatchingHandler {
        // The cursor already points just past the binding whose TYPEP returned.
        // A transfer out of that binding also unwinds the suspended search.
        let (cluster_scope, next) = search.next_binding.expect("tested handler cursor");
        let scope = &self.scopes[cluster_scope];
        let ScopeKind::HandlerCluster(cluster) = &scope.kind else {
            unreachable!("tested handler cluster")
        };
        MatchingHandler {
            cluster_scope,
            frame: scope.frame,
            target: cluster.bindings(&frames[scope.frame])[next - 1].target,
            binding: next - 1,
        }
    }

    pub(super) fn pause_signal(
        &mut self,
        frame: usize,
        search: HandlerSearch,
        masked_from: Option<usize>,
    ) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::Signaling {
                search,
                masked_from,
            },
        });
    }

    pub(super) fn debug_pause(&self) -> Option<super::debugging::Pause> {
        self.debug_signal().map(|(_, pause)| pause)
    }

    pub(super) fn debug_signal(&self) -> Option<(&Signal, super::debugging::Pause)> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| match &scope.kind {
                ScopeKind::Signaling { search, .. } => {
                    search.debug_pause.map(|pause| (&search.signal, pause))
                }
                _ => None,
            })
    }

    pub(super) fn debug_pause_mut(&mut self) -> Option<&mut super::debugging::Pause> {
        self.scopes
            .iter_mut()
            .rev()
            .find_map(|scope| match &mut scope.kind {
                ScopeKind::Signaling { search, .. } => search.debug_pause.as_mut(),
                _ => None,
            })
    }

    pub(super) fn resume_signal(&mut self) -> HandlerSearch {
        let ScopeKind::Signaling { search, .. } = self.scopes.pop().unwrap().kind else {
            unreachable!("a handler returns to its signaling scope");
        };
        search
    }

    pub(super) fn handler_case(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        handler: MatchingHandler,
        condition: Word,
    ) -> Result<UnwindTarget, EvalError> {
        debug_assert!(matches!(handler.target, HandlerTarget::Case { .. }));
        values.one(condition);
        self.abandon_exit_points_above(handler.cluster_scope);
        self.unwind(
            frames,
            values,
            PendingExit::HandlerCase {
                scope_index: handler.cluster_scope,
                binding: handler.binding,
            },
        )
    }

    pub(crate) fn bind(
        &mut self,
        frame: usize,
        heap: &'rt Heap,
        bindings: impl ExactSizeIterator<Item = (Word, Word)>,
    ) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::SpecialBindings(SpecialBindings::new(heap, bindings)),
        });
    }

    pub(crate) fn bind_progv(
        &mut self,
        runtime: &'rt Runtime,
        frame: usize,
        symbols: Word,
        values: Word,
        at: usize,
    ) -> Result<(), EvalError> {
        let heap = &runtime.heap;
        let mut count = 0usize;
        let mut valid_names = true;
        visit_proper_list(heap, symbols, at, |symbol| {
            count += 1;
            valid_names &= heap
                .symbol_attributes(symbol)
                .is_some_and(|attrs| !attrs.is_constant_variable());
        })?;
        visit_proper_list(heap, values, at, |_| {})?;
        if !valid_names {
            return Err(EvalError::new(
                at,
                "PROGV requires mutable variable symbols",
            ));
        }

        // Both lists are validated. Installing the bindings writes symbol cells and
        // root slots only, so the lists cannot move or change during this walk.
        let mut names = symbols;
        let mut arguments = values;
        let bindings = (0..count).map(|_| {
            let (name, rest) = heap.cons_fields(names).expect("validated PROGV symbols");
            names = rest;
            let value = if let Some((value, rest)) = heap.cons_fields(arguments) {
                arguments = rest;
                value
            } else {
                Word::UNBOUND
            };
            (name, value)
        });
        self.bind(frame, heap, bindings);
        Ok(())
    }

    pub(crate) fn unbind(&mut self, count: usize) {
        for _ in 0..count {
            let ScopeKind::SpecialBindings(bindings) = self.scopes.pop().unwrap().kind else {
                unreachable!("unbind closes a dynamic binding scope");
            };
            drop(bindings);
        }
    }

    pub(crate) fn enter_exit_point(
        &mut self,
        frame: usize,
        point: ExitPoint,
        resume: usize,
        clear: Register,
    ) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::ExitPoint {
                point,
                active: true,
                resume,
                clear,
                native_pc: None,
            },
        });
    }

    pub(crate) fn leave_exit_point(&mut self) {
        assert!(matches!(
            self.scopes.pop().unwrap().kind,
            ScopeKind::ExitPoint { .. }
        ));
    }

    pub(crate) fn protect(
        &mut self,
        runtime: &'rt Runtime,
        frame: usize,
        cleanup: usize,
        clear: Register,
    ) {
        self.scopes.push(ControlScope {
            frame,
            kind: ScopeKind::UnwindProtect(UnwindProtect {
                cleanup,
                clear,
                native_pc: None,
                runtime_depth: &runtime.cleanup_depth,
            }),
        });
    }

    /// Record the bytecode instruction that installed the native scope. Exit tokens,
    /// pending exits and binding values themselves stay on the shared control stack.
    pub(crate) fn mark_native_scope(&mut self, pc: usize) {
        match &mut self
            .scopes
            .last_mut()
            .expect("native scope just entered")
            .kind
        {
            ScopeKind::ExitPoint { native_pc, .. }
            | ScopeKind::UnwindProtect(UnwindProtect { native_pc, .. }) => {
                *native_pc = Some(pc);
            }
            _ => unreachable!("native scope has a bytecode landing point"),
        }
    }

    pub(super) fn recover_native_scopes(
        &mut self,
        entry: &super::JitFrameEntry,
        anchor: &super::NativeFrameAnchor,
        frame: &super::Frame<'_>,
    ) {
        use super::Instruction;

        for scope in self.scopes[entry.control_base..]
            .iter_mut()
            .take_while(|scope| scope.frame == entry.frame_index)
        {
            match &mut scope.kind {
                ScopeKind::ExitPoint {
                    point,
                    resume,
                    clear,
                    native_pc,
                    ..
                } => {
                    let origin = native_pc
                        .take()
                        .expect("generated exit point has an origin");
                    (*resume, *clear) = match (&frame.vm_code.canonical[origin], point) {
                        (Instruction::EnterBlock { resume, clear, .. }, ExitPoint::Block(_)) => {
                            (frame.vm_code.vm_target(*resume), *clear)
                        }
                        (
                            Instruction::EnterCatch { tag, resume, clear },
                            ExitPoint::Catch(slot),
                        ) => {
                            frame.slots.set(*tag, anchor.slots().get(*slot));
                            *slot = *tag;
                            (frame.vm_code.vm_target(*resume), *clear)
                        }
                        (
                            Instruction::EnterTagbody { jumps, clear, .. },
                            ExitPoint::Tagbody(..),
                        ) => (*jumps, *clear),
                        _ => unreachable!("native and bytecode exit points agree"),
                    };
                }
                ScopeKind::UnwindProtect(protect) => {
                    let origin = protect
                        .native_pc
                        .take()
                        .expect("generated cleanup has an origin");
                    let Instruction::EnterProtect { cleanup, clear } =
                        frame.vm_code.canonical[origin]
                    else {
                        unreachable!("native and bytecode cleanup agree");
                    };
                    protect.cleanup = frame.vm_code.vm_target(cleanup);
                    protect.clear = clear;
                }
                ScopeKind::HandlerCluster(cluster) => {
                    let HandlerCluster::Native {
                        table: Some(table), ..
                    } = cluster
                    else {
                        unreachable!("generated native handlers have a bytecode table");
                    };
                    *cluster = HandlerCluster::Bytecode(*table);
                }
                ScopeKind::Cleanup { .. }
                | ScopeKind::SavedValues(_)
                | ScopeKind::SpecialBindings(_) => {}
                _ => unreachable!("generated code installs only portable dynamic scopes"),
            }
        }
    }

    pub(crate) fn begin_cleanup(&mut self, frames: &mut Vec<ExecFrame<'_>>, values: &mut Values) {
        let scope = self.scopes.pop().unwrap();
        let ScopeKind::UnwindProtect(protected) = scope.kind else {
            unreachable!("cleanup starts at its protect scope");
        };
        self.start_cleanup(frames, values, scope.frame, protected, None);
    }

    pub(crate) fn end_cleanup(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
    ) -> Result<Option<UnwindTarget>, EvalError> {
        let scope = self.scopes.pop().unwrap();
        let ScopeKind::Cleanup { saved, exit, guard } = scope.kind else {
            unreachable!("cleanup ends at its cleanup scope");
        };
        drop(guard);
        *values = saved;
        match exit {
            Some(exit) => self.unwind(frames, values, exit).map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn transfer_control(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        transfer: NonLocalTransfer,
        at: usize,
    ) -> Result<TransferOutcome, EvalError> {
        let destination = self
            .scopes
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, scope)| {
                let ScopeKind::ExitPoint { point, active, .. } = &scope.kind else {
                    return None;
                };
                let matches = match (point, &transfer) {
                    (ExitPoint::Catch(reg), NonLocalTransfer::Throw(tag)) => {
                        frames[scope.frame].slots().get(*reg) == *tag
                    }
                    (ExitPoint::Block(token), NonLocalTransfer::ReturnFrom(exit_token))
                    | (ExitPoint::Tagbody(token, _), NonLocalTransfer::Go(exit_token, _)) => {
                        token == exit_token
                    }
                    _ => false,
                };
                // An abandoned CATCH still shadows an older CATCH with the same tag.
                matches.then_some((index, point, *active))
            });
        let Some((scope_index, point, true)) = destination else {
            let message = match transfer {
                NonLocalTransfer::Throw(_) => "no active CATCH for THROW",
                NonLocalTransfer::ReturnFrom(_) => "BLOCK has already exited",
                NonLocalTransfer::Go(..) => "TAGBODY has already exited",
            };
            return Ok(TransferOutcome::InvalidExit(EvalError::new(at, message)));
        };
        let label = if let (ExitPoint::Tagbody(_, count), NonLocalTransfer::Go(_, label)) =
            (point, transfer)
        {
            if label >= *count {
                return Ok(TransferOutcome::InvalidExit(EvalError::new(
                    at,
                    "TAGBODY label is out of bounds",
                )));
            }
            values.resize(0);
            Some(label)
        } else {
            None
        };
        // Abandoned exits lose their extent when the transfer starts, even if
        // some intervening bindings are still present during cleanup.
        self.abandon_exit_points_above(scope_index);
        self.unwind(
            frames,
            values,
            PendingExit::ExitPoint { scope_index, label },
        )
        .map(TransferOutcome::ResumeFrame)
    }

    fn abandon_exit_points_above(&mut self, scope_index: usize) {
        for scope in &mut self.scopes[scope_index + 1..] {
            if let ScopeKind::ExitPoint { active, .. } = &mut scope.kind {
                *active = false;
            }
        }
    }

    pub(super) fn error(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        error: EvalError,
        condition: Option<RootSlot>,
        defer_expansion_error: bool,
    ) -> Result<UnwindTarget, EvalError> {
        self.last_unhandled_condition = None;
        values.resize(0);
        self.unwind(
            frames,
            values,
            PendingExit::Error {
                error,
                condition,
                defer_expansion_error,
            },
        )
    }

    pub(super) fn cancel_task(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        error: EvalError,
    ) -> Result<UnwindTarget, EvalError> {
        self.last_unhandled_condition = None;
        values.resize(0);
        self.unwind(frames, values, PendingExit::TaskCancelled { error })
    }
}

impl<'rt> ControlStack<'rt> {
    pub(super) fn debug_frame_transfer(
        &mut self,
        frames: &mut Vec<ExecFrame<'rt>>,
        values: &mut Values,
        frame: usize,
        scope_boundary: usize,
        continuation: Box<super::debugging::FrameTransfer>,
    ) -> Result<UnwindTarget, EvalError> {
        self.unwind(
            frames,
            values,
            PendingExit::DebugFrame {
                frame,
                scope_boundary,
                continuation,
            },
        )
    }
}

impl ControlStack<'_> {
    pub(super) fn native_debug_return(
        &mut self,
        frames: &mut Vec<ExecFrame<'_>>,
        values: &mut Values,
        frame: usize,
        scope_boundary: usize,
    ) -> Result<UnwindTarget, EvalError> {
        self.unwind(
            frames,
            values,
            PendingExit::NativeDebugReturn {
                frame,
                scope_boundary,
            },
        )
    }
}
