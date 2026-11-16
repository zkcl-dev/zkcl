use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::TaskId;
use crate::Runtime;
use crate::heap::{ForeignEdge, ForeignQueue, RootFrame, RootSlots, Word};

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotPhase {
    Free,
    Queued,
    Extracted,
}

trait QueueTicket {
    fn task(&self) -> TaskId;

    fn try_claim(&self) -> bool {
        true
    }
}

struct WaitNode<T> {
    generation: u64,
    phase: SlotPhase,
    prev: u32,
    next: u32,
    data: Option<T>,
}

struct WaitQueue<T> {
    nodes: Vec<WaitNode<T>>,
    head: u32,
    tail: u32,
    free_head: u32,
    live_len: usize,
}

impl<T> Default for WaitQueue<T> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            head: NONE,
            tail: NONE,
            free_head: NONE,
            live_len: 0,
        }
    }
}

impl<T: QueueTicket> WaitQueue<T> {
    fn push(&mut self, item: T) -> (u32, u64) {
        let index = if self.free_head != NONE {
            let index = self.free_head;
            let node = &mut self.nodes[index as usize];
            debug_assert!(node.phase == SlotPhase::Free);
            self.free_head = node.next;
            node.phase = SlotPhase::Queued;
            node.data = Some(item);
            index
        } else {
            let index = u32::try_from(self.nodes.len()).expect("waiter slot count fits in u32");
            self.nodes.push(WaitNode {
                generation: 0,
                phase: SlotPhase::Queued,
                prev: NONE,
                next: NONE,
                data: Some(item),
            });
            index
        };

        let tail = self.tail;
        let node = &mut self.nodes[index as usize];
        node.prev = tail;
        node.next = NONE;
        let generation = node.generation;
        if tail != NONE {
            self.nodes[tail as usize].next = index;
        } else {
            self.head = index;
        }
        self.tail = index;
        self.live_len += 1;
        (index, generation)
    }

    fn unlink(&mut self, index: u32) {
        let prev = self.nodes[index as usize].prev;
        let next = self.nodes[index as usize].next;
        if prev != NONE {
            self.nodes[prev as usize].next = next;
        } else {
            self.head = next;
        }
        if next != NONE {
            self.nodes[next as usize].prev = prev;
        } else {
            self.tail = prev;
        }
        self.live_len -= 1;
    }

    fn release(&mut self, index: u32, generation: u64) {
        let Some(node) = self.nodes.get(index as usize) else {
            return;
        };
        if node.generation != generation || node.phase == SlotPhase::Free {
            return;
        }
        if node.phase == SlotPhase::Queued {
            self.unlink(index);
        }
        let free_head = self.free_head;
        let node = &mut self.nodes[index as usize];
        node.phase = SlotPhase::Free;
        node.prev = NONE;
        node.next = free_head;
        node.data = None;
        node.generation = node.generation.wrapping_add(1);
        self.free_head = index;
    }

    fn pop_live_with(&mut self, mutate: impl FnOnce(&mut T)) -> Option<TaskId> {
        while self.head != NONE {
            let index = self.head;
            self.unlink(index);
            let node = &mut self.nodes[index as usize];
            node.phase = SlotPhase::Extracted;
            node.prev = NONE;
            node.next = NONE;
            let data = node.data.as_mut().expect("queued waiter holds data");
            if data.try_claim() {
                let task = data.task();
                mutate(data);
                return Some(task);
            }
            // Drop payload/roots immediately when an already-claimed Select waiter is skipped.
            node.data = None;
        }
        None
    }

    fn drain_live(queue: &Rc<RefCell<Self>>, mut visit: impl FnMut(TaskId)) {
        // Release the queue borrow before waking waiters so drop/reentry stays safe.
        loop {
            let task = queue.borrow_mut().pop_live_with(|_| {});
            let Some(task) = task else { break };
            visit(task);
        }
    }

    fn get(&self, index: u32, generation: u64) -> Option<&WaitNode<T>> {
        self.nodes
            .get(index as usize)
            .filter(|node| node.generation == generation)
    }

    fn get_mut(&mut self, index: u32, generation: u64) -> Option<&mut WaitNode<T>> {
        self.nodes
            .get_mut(index as usize)
            .filter(|node| node.generation == generation)
    }
}

pub(super) struct SelectClaim {
    winner: Cell<Option<usize>>,
    committed_transfer: Cell<bool>,
    payload: RefCell<Option<RootFrame>>,
}

impl SelectClaim {
    pub(super) fn new() -> Rc<Self> {
        Rc::new(Self {
            winner: Cell::new(None),
            committed_transfer: Cell::new(false),
            payload: RefCell::new(None),
        })
    }

    pub(super) fn winner(&self) -> Option<usize> {
        self.winner.get()
    }

    pub(super) fn claim(&self, branch: usize) -> bool {
        if self.winner.get().is_some() {
            return false;
        }
        self.winner.set(Some(branch));
        true
    }

    pub(super) fn has_committed_transfer(&self) -> bool {
        self.committed_transfer.get()
    }

    fn record_transfer(&self, payload: Option<RootFrame>) {
        self.committed_transfer.set(true);
        *self.payload.borrow_mut() = payload;
    }

    pub(super) fn take_payload(&self) -> Option<RootFrame> {
        self.payload.borrow_mut().take()
    }
}

type SelectSlot = Option<(Rc<SelectClaim>, usize)>;

struct WaiterTicket {
    task: TaskId,
    payload: Option<RootFrame>,
    select: SelectSlot,
}

impl WaiterTicket {
    fn deliver(&mut self, payload: RootFrame) {
        if let Some((claim, _)) = &self.select {
            claim.record_transfer(Some(payload));
        } else {
            self.payload = Some(payload);
        }
    }
}

impl QueueTicket for WaiterTicket {
    fn task(&self) -> TaskId {
        self.task
    }

    fn try_claim(&self) -> bool {
        match &self.select {
            Some((claim, branch)) => claim.claim(*branch),
            None => true,
        }
    }
}

pub(super) struct QueueRegistration {
    queue: Rc<RefCell<WaitQueue<WaiterTicket>>>,
    slot: u32,
    generation: u64,
}

impl QueueRegistration {
    pub(super) fn is_woken(&self) -> bool {
        self.queue
            .borrow()
            .get(self.slot, self.generation)
            .is_some_and(|node| node.phase == SlotPhase::Extracted)
    }

    pub(super) fn take_payload(&self) -> Option<RootFrame> {
        self.queue
            .borrow_mut()
            .get_mut(self.slot, self.generation)
            .and_then(|node| node.data.as_mut())
            .and_then(|ticket| ticket.payload.take())
    }
}

impl Drop for QueueRegistration {
    fn drop(&mut self) {
        self.queue.borrow_mut().release(self.slot, self.generation);
    }
}

fn register_waiter(
    queue: &Rc<RefCell<WaitQueue<WaiterTicket>>>,
    task: TaskId,
    select: SelectSlot,
) -> QueueRegistration {
    let (slot, generation) = queue.borrow_mut().push(WaiterTicket {
        task,
        payload: None,
        select,
    });
    QueueRegistration {
        queue: Rc::clone(queue),
        slot,
        generation,
    }
}

pub(crate) struct Event {
    signaled: Cell<bool>,
    waiters: Rc<RefCell<WaitQueue<WaiterTicket>>>,
}

impl Event {
    pub(super) fn new(signaled: bool) -> Rc<Self> {
        Rc::new(Self {
            signaled: Cell::new(signaled),
            waiters: Rc::default(),
        })
    }

    pub(super) fn is_set(&self) -> bool {
        self.signaled.get()
    }

    pub(super) fn set(&self, wake: impl FnMut(TaskId)) -> bool {
        let changed = !self.signaled.replace(true);
        WaitQueue::drain_live(&self.waiters, wake);
        changed
    }

    pub(super) fn reset(&self) -> bool {
        self.signaled.replace(false)
    }

    pub(super) fn register_wait(&self, task: TaskId) -> QueueRegistration {
        register_waiter(&self.waiters, task, None)
    }

    pub(super) fn register_select_wait(
        &self,
        task: TaskId,
        claim: Rc<SelectClaim>,
        branch: usize,
    ) -> QueueRegistration {
        register_waiter(&self.waiters, task, Some((claim, branch)))
    }
}

pub(super) struct EventHandle(pub(super) Rc<Event>);

pub(crate) struct Semaphore {
    permits: Cell<usize>,
    waiters: Rc<RefCell<WaitQueue<WaiterTicket>>>,
}

impl Semaphore {
    pub(super) fn new(permits: usize) -> Rc<Self> {
        Rc::new(Self {
            permits: Cell::new(permits),
            waiters: Rc::default(),
        })
    }

    pub(super) fn count(&self) -> usize {
        self.permits.get()
    }

    pub(super) fn try_acquire(&self) -> bool {
        let permits = self.permits.get();
        if permits == 0 {
            return false;
        }
        self.permits.set(permits - 1);
        true
    }

    pub(super) fn release(&self, mut wake: impl FnMut(TaskId)) -> Result<usize, &'static str> {
        let waiter = self.waiters.borrow_mut().pop_live_with(|_| {});
        if let Some(task) = waiter {
            wake(task);
            return Ok(self.permits.get());
        }
        let next = self
            .permits
            .get()
            .checked_add(1)
            .ok_or("Semaphore permit count exceeded supported range")?;
        self.permits.set(next);
        Ok(next)
    }

    pub(super) fn register_acquire(&self, task: TaskId) -> QueueRegistration {
        register_waiter(&self.waiters, task, None)
    }
}

pub(super) struct SemaphoreHandle(pub(super) Rc<Semaphore>);

struct WaitingSender {
    task: TaskId,
    value: Option<RootFrame>,
    delivered: bool,
    select: SelectSlot,
}

impl WaitingSender {
    fn new(runtime: &Runtime, task: TaskId, value: Word, select: SelectSlot) -> Self {
        let slot = runtime.root_frame(1);
        slot.set(0, value);
        Self {
            task,
            value: Some(slot),
            delivered: false,
            select,
        }
    }

    fn take_and_mark_delivered(&mut self) -> Word {
        let word = self
            .value
            .take()
            .expect("live sender entry holds a value")
            .get(0);
        self.delivered = true;
        if let Some((claim, _)) = &self.select {
            claim.record_transfer(None);
        }
        word
    }
}

impl QueueTicket for WaitingSender {
    fn task(&self) -> TaskId {
        self.task
    }

    fn try_claim(&self) -> bool {
        match &self.select {
            Some((claim, branch)) => claim.claim(*branch),
            None => true,
        }
    }
}

pub(super) struct SenderRegistration {
    queue: Rc<RefCell<WaitQueue<WaitingSender>>>,
    slot: u32,
    generation: u64,
}

impl SenderRegistration {
    pub(super) fn is_delivered(&self) -> bool {
        self.queue
            .borrow()
            .get(self.slot, self.generation)
            .and_then(|node| node.data.as_ref())
            .is_some_and(|entry| entry.delivered)
    }
}

impl Drop for SenderRegistration {
    fn drop(&mut self) {
        self.queue.borrow_mut().release(self.slot, self.generation);
    }
}

pub(crate) struct Channel {
    capacity: Option<usize>,
    closed: Cell<bool>,
    buffer: ForeignQueue,
    senders: Rc<RefCell<WaitQueue<WaitingSender>>>,
    receivers: Rc<RefCell<WaitQueue<WaiterTicket>>>,
}

impl Channel {
    pub(super) fn new(runtime: &Runtime, capacity: Option<usize>) -> Rc<Self> {
        Rc::new(Self {
            capacity,
            closed: Cell::new(false),
            buffer: ForeignQueue::new(runtime),
            senders: Rc::default(),
            receivers: Rc::default(),
        })
    }

    pub(super) fn edge(&self) -> ForeignEdge {
        self.buffer.edge()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.get()
    }

    pub(super) fn close(&self, mut wake: impl FnMut(TaskId)) -> bool {
        if self.closed.replace(true) {
            return false;
        }
        WaitQueue::drain_live(&self.receivers, &mut wake);
        WaitQueue::drain_live(&self.senders, wake);
        true
    }

    pub(super) fn try_send(
        &self,
        runtime: &Runtime,
        value: Word,
        mut wake: impl FnMut(TaskId),
    ) -> Result<bool, &'static str> {
        if self.closed.get() {
            return Err("Cannot send to a closed channel");
        }
        // A waiting receiver observed an empty buffer. Reserve the message
        // for it before waking it, even on a channel with buffer capacity.
        let receiver = self.receivers.borrow_mut().pop_live_with(|ticket| {
            let payload = runtime.root_frame(1);
            payload.set(0, value);
            ticket.deliver(payload);
        });
        if let Some(task) = receiver {
            wake(task);
            return Ok(true);
        }
        Ok(self.buffer.try_push(value, self.capacity))
    }

    pub(super) fn try_recv(&self, mut wake: impl FnMut(TaskId)) -> Option<Word> {
        let buffered = self.buffer.pop();
        let mut sent = None;
        let sender = self.senders.borrow_mut().pop_live_with(|entry| {
            sent = Some(entry.take_and_mark_delivered());
        });
        let Some(task) = sender else {
            return buffered;
        };
        let sent = sent.expect("sender provided a value");
        let item = if let Some(item) = buffered {
            // Refill the slot just freed by the receive, in sender FIFO order.
            self.buffer.try_push(sent, None);
            item
        } else {
            sent
        };
        wake(task);
        Some(item)
    }

    pub(super) fn register_send(
        &self,
        runtime: &Runtime,
        task: TaskId,
        value: Word,
    ) -> SenderRegistration {
        self.register_send_entry(runtime, task, value, None)
    }

    pub(super) fn register_select_send(
        &self,
        runtime: &Runtime,
        task: TaskId,
        value: Word,
        claim: Rc<SelectClaim>,
        branch: usize,
    ) -> SenderRegistration {
        self.register_send_entry(runtime, task, value, Some((claim, branch)))
    }

    fn register_send_entry(
        &self,
        runtime: &Runtime,
        task: TaskId,
        value: Word,
        select: SelectSlot,
    ) -> SenderRegistration {
        let (slot, generation) = self
            .senders
            .borrow_mut()
            .push(WaitingSender::new(runtime, task, value, select));
        SenderRegistration {
            queue: Rc::clone(&self.senders),
            slot,
            generation,
        }
    }

    pub(super) fn register_recv(&self, task: TaskId) -> QueueRegistration {
        register_waiter(&self.receivers, task, None)
    }

    pub(super) fn register_select_recv(
        &self,
        task: TaskId,
        claim: Rc<SelectClaim>,
        branch: usize,
    ) -> QueueRegistration {
        register_waiter(&self.receivers, task, Some((claim, branch)))
    }
}

pub(super) struct ChannelHandle(pub(super) Rc<Channel>);

#[derive(Clone)]
pub(super) enum PromiseStatus {
    Pending,
    Fulfilled,
    Failed(Option<Rc<str>>),
}

pub(crate) struct Promise {
    status: RefCell<PromiseStatus>,
    result: ForeignEdge,
    waiters: Rc<RefCell<WaitQueue<WaiterTicket>>>,
}

impl Promise {
    pub(super) fn new(runtime: &Runtime) -> Rc<Self> {
        Rc::new(Self {
            status: RefCell::new(PromiseStatus::Pending),
            result: ForeignEdge::empty(runtime),
            waiters: Rc::default(),
        })
    }

    pub(super) fn edge(&self) -> ForeignEdge {
        self.result.clone()
    }

    pub(super) fn status(&self) -> PromiseStatus {
        self.status.borrow().clone()
    }

    pub(super) fn is_done(&self) -> bool {
        !matches!(*self.status.borrow(), PromiseStatus::Pending)
    }

    pub(super) fn with_result<R>(&self, read: impl FnOnce(&RootSlots) -> R) -> R {
        self.result.with_slots(read)
    }

    pub(super) fn fulfill(
        &self,
        values: impl ExactSizeIterator<Item = Word>,
        wake: impl FnMut(TaskId),
    ) -> bool {
        {
            let mut status = self.status.borrow_mut();
            if !matches!(*status, PromiseStatus::Pending) {
                return false;
            }
            self.result.set_words(values);
            *status = PromiseStatus::Fulfilled;
        }
        WaitQueue::drain_live(&self.waiters, wake);
        true
    }

    pub(super) fn fail_condition(&self, condition: Word, wake: impl FnMut(TaskId)) -> bool {
        {
            let mut status = self.status.borrow_mut();
            if !matches!(*status, PromiseStatus::Pending) {
                return false;
            }
            self.result.set_words(std::iter::once(condition));
            *status = PromiseStatus::Failed(None);
        }
        WaitQueue::drain_live(&self.waiters, wake);
        true
    }

    pub(super) fn fail_message(&self, message: &str, wake: impl FnMut(TaskId)) -> bool {
        {
            let mut status = self.status.borrow_mut();
            if !matches!(*status, PromiseStatus::Pending) {
                return false;
            }
            *status = PromiseStatus::Failed(Some(Rc::from(message)));
        }
        WaitQueue::drain_live(&self.waiters, wake);
        true
    }

    pub(super) fn register_wait(&self, task: TaskId) -> QueueRegistration {
        register_waiter(&self.waiters, task, None)
    }

    pub(super) fn register_select_wait(
        &self,
        task: TaskId,
        claim: Rc<SelectClaim>,
        branch: usize,
    ) -> QueueRegistration {
        register_waiter(&self.waiters, task, Some((claim, branch)))
    }
}

pub(super) struct PromiseHandle(pub(super) Rc<Promise>);

#[cfg(test)]
mod tests {
    use super::{Channel, Event, Promise, Runtime, SelectClaim, Semaphore, Word};

    #[test]
    fn promise_wakes_waiters_and_ignores_duplicate_completion() {
        let runtime = Runtime::start().unwrap();
        let promise = Promise::new(&runtime);
        assert!(!promise.is_done());

        let claim = SelectClaim::new();
        let wait_a = promise.register_wait(1);
        let wait_b = promise.register_select_wait(2, claim.clone(), 3);
        let mut woken = Vec::new();
        assert!(promise.fulfill([Word::T, Word::NIL].into_iter(), |id| woken.push(id)));
        assert_eq!(woken, [1, 2]);
        assert!(promise.is_done());
        assert!(wait_a.is_woken());
        assert!(wait_b.is_woken());
        assert_eq!(claim.winner(), Some(3));
        assert!(!promise.fail_message("late", |_| unreachable!("already resolved")));
        promise.with_result(|slots| {
            assert_eq!(slots.len(), 2);
            assert_eq!(slots.get(0), Word::T);
            assert_eq!(slots.get(1), Word::NIL);
        });
    }

    #[test]
    fn semaphore_preserves_permits_and_fifo_handoff_under_cancellation() {
        let sem = Semaphore::new(1);
        assert_eq!(sem.count(), 1);
        assert!(sem.try_acquire());
        assert_eq!(sem.count(), 0);
        assert!(!sem.try_acquire());

        let cancelled = sem.register_acquire(1);
        let next = sem.register_acquire(2);
        drop(cancelled);
        let mut woken = Vec::new();
        assert_eq!(sem.release(|id| woken.push(id)), Ok(0));
        assert_eq!(woken, [2]);
        assert!(next.is_woken());
        assert_eq!(sem.release(|_| unreachable!("no queued waiters")), Ok(1));
        assert_eq!(sem.count(), 1);
    }

    #[test]
    fn select_claim_skips_already_claimed_waiters_and_senders() {
        let runtime = Runtime::start().unwrap();
        let ch1 = Channel::new(&runtime, Some(0));
        let ch2 = Channel::new(&runtime, Some(1));
        let ch3 = Channel::new(&runtime, Some(0));
        let claim = SelectClaim::new();
        let _r1 = ch1.register_select_recv(10, claim.clone(), 0);
        let _r2 = ch2.register_select_recv(10, claim.clone(), 1);
        let _s3 = ch3.register_select_send(&runtime, 10, Word::NIL, claim.clone(), 2);
        let _other_recv = ch3.register_recv(20);

        let mut woken = Vec::new();
        assert_eq!(ch1.try_send(&runtime, Word::T, |id| woken.push(id)), Ok(true));
        assert_eq!(woken, [10]);
        assert_eq!(claim.winner(), Some(0));
        assert!(claim.has_committed_transfer());
        assert_eq!(claim.take_payload().map(|p| p.get(0)), Some(Word::T));

        // Second send on ch2 must not deliver to the already-claimed select waiter.
        woken.clear();
        assert_eq!(ch2.try_send(&runtime, Word::NIL, |id| woken.push(id)), Ok(true));
        assert!(woken.is_empty());
        assert_eq!(ch2.try_recv(|_| {}), Some(Word::NIL));

        // ch3 already has _other_recv queued behind _s3; recv skips the claimed sender.
        assert_eq!(ch3.try_recv(|_| {}), None);
    }

    #[test]
    fn event_wakes_live_waiters_and_skips_cancelled_ones() {
        let event = Event::new(false);
        let cancelled = event.register_wait(1);
        let live_a = event.register_wait(2);
        let live_b = event.register_wait(3);
        drop(cancelled);
        let mut woken = Vec::new();
        assert!(event.set(|id| woken.push(id)));
        assert_eq!(woken, [2, 3]);
        assert!(live_a.is_woken());
        assert!(live_b.is_woken());
        assert!(!event.set(|_| unreachable!("already signaled")));
        assert!(event.reset());
        assert!(!event.is_set());
    }

    #[test]
    fn bounded_and_rendezvous_channels_preserve_fifo_and_close_semantics() {
        let runtime = Runtime::start().unwrap();
        let ch = Channel::new(&runtime, Some(1));
        assert_eq!(ch.try_send(&runtime, Word::T, |_| {}), Ok(true));
        assert_eq!(ch.try_send(&runtime, Word::NIL, |_| {}), Ok(false));
        let mut woken = Vec::new();
        let sender = ch.register_send(&runtime, 10, Word::NIL);
        assert_eq!(ch.try_recv(|id| woken.push(id)), Some(Word::T));
        assert_eq!(woken, [10]);
        assert!(sender.is_delivered());
        assert!(ch.close(|_| {}));
        assert_eq!(
            ch.try_send(&runtime, Word::T, |_| {}),
            Err("Cannot send to a closed channel")
        );
        assert_eq!(ch.try_recv(|_| {}), Some(Word::NIL));
        assert_eq!(ch.try_recv(|_| {}), None);

        let rdv = Channel::new(&runtime, Some(0));
        assert_eq!(rdv.try_send(&runtime, Word::T, |_| {}), Ok(false));
        let recv = rdv.register_recv(20);
        woken.clear();
        assert_eq!(rdv.try_send(&runtime, Word::T, |id| woken.push(id)), Ok(true));
        assert_eq!(woken, [20]);
        assert!(recv.is_woken());
        assert_eq!(recv.take_payload().map(|slot| slot.get(0)), Some(Word::T));
    }

    #[test]
    fn wait_queue_reuses_slots_and_unlinks_cancellations_in_o1() {
        let runtime = Runtime::start().unwrap();
        let ch = Channel::new(&runtime, Some(0));
        let mut live = Vec::new();
        for id in 0u64..1000 {
            let reg = ch.register_send(&runtime, id, Word::T);
            if id % 2 == 0 {
                drop(reg);
            } else {
                live.push((id, reg));
            }
        }
        assert_eq!(ch.senders.borrow().live_len, 500);
        let capacity = ch.senders.borrow().nodes.len();
        // Each immediately released slot is reused by the following sender.
        assert_eq!(capacity, 500);
        let mut last = Some(live.pop().unwrap());
        let mut woken = Vec::new();
        for _ in 0..499 {
            assert_eq!(ch.try_recv(|id| woken.push(id)), Some(Word::T));
        }
        assert_eq!(woken.len(), 499);
        assert!(ch.close(|id| {
            assert_eq!(id, 999);
            drop(last.take());
        }));
        assert_eq!(ch.senders.borrow().live_len, 0);
        assert_eq!(ch.senders.borrow().nodes.len(), capacity);
    }
}
