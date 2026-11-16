use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Default)]
pub(in crate::vm) struct TaskCancellation {
    requested: Cell<bool>,
    triggered_boundaries: Cell<usize>,
    groups_in_scope: Cell<usize>,
    scopes_in_scope: Cell<usize>,
    deadlines_in_scope: Cell<usize>,
}

impl TaskCancellation {
    pub(super) fn request(&self) -> bool {
        !self.requested.replace(true)
    }

    pub(in crate::vm) fn requested(&self) -> bool {
        self.requested.get()
    }

    pub(in crate::vm) fn has_triggered_boundary(&self) -> bool {
        self.triggered_boundaries.get() != 0
    }

    pub(in crate::vm) fn is_pending(&self) -> bool {
        self.requested() || self.has_triggered_boundary()
    }

    pub(super) fn has_cancel_boundary_in_scope(&self) -> bool {
        self.groups_in_scope.get() != 0 || self.scopes_in_scope.get() != 0
    }

    pub(in crate::vm) fn has_deadline_in_scope(&self) -> bool {
        self.deadlines_in_scope.get() != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundaryState {
    New,
    Armed,
    Triggered,
    Unwound,
    Inactive,
}

pub(crate) struct BoundarySignal {
    owner: Rc<TaskCancellation>,
    state: Cell<BoundaryState>,
    source: BoundarySource,
}

enum BoundarySource {
    Group,
    Scope,
    Deadline(RefCell<Option<Rc<str>>>),
}

impl BoundarySignal {
    fn new(owner: Rc<TaskCancellation>, source: BoundarySource) -> Rc<Self> {
        Rc::new(Self {
            owner,
            state: Cell::new(BoundaryState::New),
            source,
        })
    }

    pub(super) fn for_group(owner: Rc<TaskCancellation>) -> Rc<Self> {
        Self::new(owner, BoundarySource::Group)
    }

    pub(super) fn for_scope(owner: Rc<TaskCancellation>) -> Rc<Self> {
        Self::new(owner, BoundarySource::Scope)
    }

    pub(super) fn for_deadline(owner: Rc<TaskCancellation>) -> Rc<Self> {
        Self::new(owner, BoundarySource::Deadline(RefCell::default()))
    }

    pub(in crate::vm) fn arm(&self) {
        debug_assert!(self.state.get() == BoundaryState::New);
        self.state.set(BoundaryState::Armed);
        self.scope_count().update(|n| n + 1);
    }

    fn scope_count(&self) -> &Cell<usize> {
        match self.source {
            BoundarySource::Group => &self.owner.groups_in_scope,
            BoundarySource::Scope => &self.owner.scopes_in_scope,
            BoundarySource::Deadline(_) => &self.owner.deadlines_in_scope,
        }
    }

    pub(super) fn trigger(&self) -> bool {
        if self.state.get() == BoundaryState::Armed {
            self.owner.triggered_boundaries.update(|n| n + 1);
            self.state.set(BoundaryState::Triggered);
            true
        } else {
            false
        }
    }

    pub(super) fn belongs_to(&self, owner: &Rc<TaskCancellation>) -> bool {
        Rc::ptr_eq(&self.owner, owner)
    }

    pub(super) fn fail_timer(&self, error: Rc<str>) {
        let BoundarySource::Deadline(fault) = &self.source else {
            unreachable!("timer belongs to a deadline boundary");
        };
        fault.replace(Some(error));
        self.trigger();
    }

    pub(super) fn take_timer_error(&self) -> Option<Rc<str>> {
        let BoundarySource::Deadline(fault) = &self.source else {
            unreachable!("only deadline boundaries have timer errors");
        };
        fault.borrow_mut().take()
    }

    pub(in crate::vm) fn has_deadline(&self) -> bool {
        matches!(self.source, BoundarySource::Deadline(_))
    }

    pub(in crate::vm) fn is_triggered(&self) -> bool {
        self.state.get() == BoundaryState::Triggered
    }

    pub(super) fn was_unwound(&self) -> bool {
        self.state.get() == BoundaryState::Unwound
    }

    pub(in crate::vm) fn unwind(&self) {
        debug_assert!(self.is_triggered());
        self.disarm();
        self.state.set(BoundaryState::Unwound);
    }

    pub(in crate::vm) fn disarm(&self) {
        let state = self.state.get();
        if matches!(state, BoundaryState::Armed | BoundaryState::Triggered) {
            self.state.set(BoundaryState::Inactive);
            self.scope_count().update(|n| n - 1);
            if state == BoundaryState::Triggered {
                self.owner.triggered_boundaries.update(|n| n - 1);
            }
        }
    }
}
