use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use super::TaskId;
use super::cancellation::{BoundarySignal, TaskCancellation};
use super::handles::TaskFailure;

pub(crate) type TaskGroup = Rc<RefCell<Group>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Open,
    Joining,
    Cancelling,
    Closed,
}

#[derive(Default)]
struct Children {
    first: Option<(TaskId, bool)>,
    rest: BTreeMap<TaskId, bool>,
}

impl Children {
    fn insert(&mut self, id: TaskId, no_wait: bool) {
        if let Some((first, _)) = self.first {
            debug_assert!(first < id, "child IDs increase in creation order");
            self.rest.insert(id, no_wait);
        } else {
            self.first = Some((id, no_wait));
        }
    }

    fn remove(&mut self, id: TaskId) -> bool {
        if self.first.is_some_and(|(first, _)| first == id) {
            self.first = self.rest.pop_first();
            true
        } else {
            self.rest.remove(&id).is_some()
        }
    }

    fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    fn iter(&self) -> impl Iterator<Item = (TaskId, bool)> + '_ {
        self.first
            .into_iter()
            .chain(self.rest.iter().map(|(&id, &no_wait)| (id, no_wait)))
    }
}

pub(crate) struct Group {
    owner: TaskId,
    signal: Rc<BoundarySignal>,
    phase: Phase,
    failure: Option<TaskFailure>,
    // Store a single child inline and cancel in task creation order.
    children: Children,
}

impl Group {
    pub(super) fn new(owner: TaskId, cancellation: Rc<TaskCancellation>) -> Self {
        Self {
            owner,
            signal: BoundarySignal::for_group(cancellation),
            phase: Phase::Open,
            failure: None,
            children: Children::default(),
        }
    }

    pub(super) fn belongs_to(&self, owner: TaskId, cancellation: &Rc<TaskCancellation>) -> bool {
        self.owner == owner && self.signal.belongs_to(cancellation)
    }

    pub(super) fn is_open(&self) -> bool {
        self.phase == Phase::Open
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.phase == Phase::Cancelling
    }

    pub(super) fn boundary(&self) -> Rc<BoundarySignal> {
        Rc::clone(&self.signal)
    }

    pub(super) fn begin_close(&mut self, cancel_all: bool, cancel: impl FnMut(TaskId)) -> bool {
        let next = if cancel_all {
            Phase::Cancelling
        } else {
            Phase::Joining
        };
        match (self.phase, cancel_all) {
            (Phase::Open, false) | (Phase::Open | Phase::Joining, true) => {
                self.cancel_children_in_phase(next, cancel_all, cancel);
            }
            _ => {}
        }
        self.children.is_empty()
    }

    pub(super) fn cancel(&mut self, cancel: impl FnMut(TaskId)) -> bool {
        if !matches!(self.phase, Phase::Open | Phase::Joining) {
            return false;
        }
        self.cancel_children_in_phase(Phase::Cancelling, true, cancel);
        true
    }

    fn cancel_children_in_phase(
        &mut self,
        next: Phase,
        cancel_all: bool,
        mut cancel: impl FnMut(TaskId),
    ) {
        self.phase = next;
        for (id, no_wait) in self.children.iter() {
            if cancel_all || no_wait {
                cancel(id);
            }
        }
    }

    pub(super) fn take_failure(&mut self) -> Option<TaskFailure> {
        self.failure.take()
    }

    pub(super) fn close(&mut self) {
        self.phase = Phase::Closed;
        self.failure = None;
    }

    pub(super) fn joins(&self, owner_cancelled: bool) -> impl Iterator<Item = (TaskId, bool)> + '_ {
        let cancel_all = owner_cancelled || self.phase == Phase::Cancelling;
        self.children
            .iter()
            .map(move |(child, no_wait)| (child, cancel_all || no_wait))
    }
}

/// The implicit join edges for cycle detection. Each group stores its own
/// children. This index only locates the groups a task currently owns.
#[derive(Default)]
pub(super) struct ActiveGroups {
    by_owner: FxHashMap<TaskId, SmallVec<[TaskGroup; 1]>>,
}

impl ActiveGroups {
    pub(super) fn try_insert_child(
        &mut self,
        group: &TaskGroup,
        child: TaskId,
        no_wait: bool,
    ) -> bool {
        let mut state = group.borrow_mut();
        // Allocating the task handle can run host destructors. Only publish a
        // child if the group is still open after any such reentry.
        if !state.is_open() {
            return false;
        }
        if state.children.is_empty() {
            self.by_owner
                .entry(state.owner)
                .or_default()
                .push(Rc::clone(group));
        }
        state.children.insert(child, no_wait);
        true
    }

    pub(super) fn finish_child(
        &mut self,
        group: &TaskGroup,
        child: TaskId,
        failure: Option<TaskFailure>,
    ) -> (TaskId, bool, bool) {
        let mut state = group.borrow_mut();
        let removed = state.children.remove(child);
        debug_assert!(removed);
        let empty = state.children.is_empty();
        if empty {
            let groups = self
                .by_owner
                .get_mut(&state.owner)
                .expect("live group owner");
            let index = groups
                .iter()
                .position(|live| Rc::ptr_eq(live, group))
                .expect("live group in owner index");
            groups.swap_remove(index);
        }
        let first_failure = if let Some(failure) = failure
            && state.failure.is_none()
        {
            state.failure = Some(failure);
            state.signal.trigger();
            true
        } else {
            false
        };
        (state.owner, empty, first_failure)
    }

    pub(super) fn of(&self, owner: TaskId) -> &[TaskGroup] {
        self.by_owner.get(&owner).map_or(&[], SmallVec::as_slice)
    }

    pub(super) fn retire_owner(&mut self, owner: TaskId) {
        if self.by_owner.get(&owner).is_some_and(SmallVec::is_empty) {
            self.by_owner.remove(&owner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Children;

    #[test]
    fn group_children_keep_creation_order_across_removals() {
        let mut children = Children::default();
        children.insert(1, false);
        children.insert(2, true);
        children.insert(3, false);
        assert!(children.remove(2));
        assert!(children.remove(1));
        children.insert(4, true);
        assert!(children.iter().eq([(3, false), (4, true)]));
        assert!(children.remove(4));
        assert!(children.remove(3));
        assert!(children.is_empty());
    }
}
