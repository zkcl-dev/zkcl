use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use super::TaskId;
use super::cancellation::TaskCancellation;
use crate::heap::{ForeignEdge, RootSlot, RootSlots, Word};
use crate::{EvalError, ExecutionStop, OutOfMemory, Runtime};

#[derive(Clone)]
pub(super) enum TaskStatus {
    Running,
    Completed,
    Failed(Option<Rc<str>>),
    Cancelled,
}

pub(crate) struct Task {
    pub(super) id: TaskId,
    pub(super) name: Option<String>,
    pub(super) cancellation: Rc<TaskCancellation>,
    status: RefCell<TaskStatus>,
    result: ForeignEdge,
    waiters: RefCell<Awaiters>,
}

impl Task {
    pub(super) fn is_running(&self) -> bool {
        matches!(*self.status.borrow(), TaskStatus::Running)
    }

    pub(super) fn status(&self) -> TaskStatus {
        self.status.borrow().clone()
    }

    pub(super) fn with_result<R>(&self, read: impl FnOnce(&RootSlots) -> R) -> R {
        self.result.with_slots(read)
    }

    pub(super) fn register_await(self: &Rc<Self>, waiter: TaskId) -> Option<AwaitRegistration> {
        self.waiters
            .borrow_mut()
            .insert(waiter)
            .then(|| AwaitRegistration {
                task: Rc::clone(self),
                waiter,
            })
    }

    pub(super) fn wake_waiters(&self, wake: impl FnMut(TaskId)) {
        // Release the registry borrow before switching waiters to ready.
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        waiters.for_each(wake);
    }
}

pub(super) struct AwaitRegistration {
    pub(super) task: Rc<Task>,
    waiter: TaskId,
}

impl Drop for AwaitRegistration {
    fn drop(&mut self) {
        if self.task.is_running() {
            self.task.waiters.borrow_mut().remove(self.waiter);
        }
    }
}

#[derive(Default)]
enum Awaiters {
    #[default]
    None,
    One(TaskId),
    Many(BTreeSet<TaskId>),
}

impl Awaiters {
    fn insert(&mut self, id: TaskId) -> bool {
        match self {
            Self::None => *self = Self::One(id),
            Self::One(first) if *first != id => *self = Self::Many(BTreeSet::from([*first, id])),
            Self::One(_) => return false,
            Self::Many(ids) => return ids.insert(id),
        }
        true
    }

    fn remove(&mut self, id: TaskId) {
        match self {
            Self::One(first) if *first == id => *self = Self::None,
            Self::Many(ids) => {
                ids.remove(&id);
                if ids.len() == 1 {
                    *self = Self::One(*ids.first().expect("one remaining waiter"));
                }
            }
            _ => {}
        }
    }

    fn for_each(self, mut visit: impl FnMut(TaskId)) {
        match self {
            Self::None => {}
            Self::One(id) => visit(id),
            Self::Many(ids) => ids.into_iter().for_each(visit),
        }
    }
}

pub(super) struct TaskHandle(pub(super) Rc<Task>);

/// The scheduler roots the handle until the task exits. After that, the Lisp
/// handle alone keeps its cached result reachable through the collector edge.
pub(super) struct LiveHandle {
    pub(super) word: RootSlot,
    pub(super) task: Rc<Task>,
}

impl Task {
    pub(super) fn allocate_handle(
        runtime: &Runtime,
        id: TaskId,
        name: Option<String>,
        cancellation: Rc<TaskCancellation>,
    ) -> Result<(RootSlot, Rc<Self>), OutOfMemory> {
        let result = ForeignEdge::empty(runtime);
        let task = Rc::new(Task {
            id,
            name,
            cancellation,
            status: RefCell::new(TaskStatus::Running),
            result: result.clone(),
            waiters: RefCell::default(),
        });
        let word = runtime
            .foreign_with_edge(TaskHandle(Rc::clone(&task)), Some(result))?
            .slot;
        Ok((word, task))
    }
}

impl LiveHandle {
    pub(super) fn complete(&self, values: impl ExactSizeIterator<Item = Word>) {
        self.task.result.set_words(values);
        self.task.status.replace(TaskStatus::Completed);
    }

    pub(super) fn fail(
        &self,
        condition: Option<RootSlot>,
        error: &EvalError,
    ) -> Option<TaskFailure> {
        if error.execution_stop() == Some(ExecutionStop::Cancelled) {
            self.task.cancellation.request();
            self.task.status.replace(TaskStatus::Cancelled);
            return None;
        }
        Some(if let Some(condition) = condition {
            self.task.result.set_words(std::iter::once(condition.get()));
            self.task.status.replace(TaskStatus::Failed(None));
            TaskFailure::Condition(condition)
        } else {
            let message: Rc<str> = Rc::from(error.message());
            self.task
                .status
                .replace(TaskStatus::Failed(Some(Rc::clone(&message))));
            TaskFailure::Message(message)
        })
    }
}

impl Drop for LiveHandle {
    fn drop(&mut self) {
        // Host aborts and Rust unwinds can discard an execution with live handles.
        if self.task.is_running() {
            self.task.cancellation.request();
            self.task.status.replace(TaskStatus::Cancelled);
        }
        self.task.waiters.replace(Awaiters::None);
    }
}

pub(super) enum TaskFailure {
    Condition(RootSlot),
    Message(Rc<str>),
}

#[cfg(test)]
mod tests {
    use super::Awaiters;

    #[test]
    fn removing_one_waiter_preserves_the_remaining_creation_order() {
        let mut waiters = Awaiters::default();
        waiters.insert(7);
        waiters.remove(7);
        assert!(matches!(waiters, Awaiters::None));
        for id in [7, 7, 3, 5, 2] {
            waiters.insert(id);
        }
        waiters.remove(5);
        let mut woken = Vec::new();
        waiters.for_each(|id| woken.push(id));
        assert_eq!(woken, [2, 3, 7]);
    }
}
