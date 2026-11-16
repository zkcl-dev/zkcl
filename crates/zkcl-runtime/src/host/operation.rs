use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use crate::condition::Fault;
use crate::execution::ExecutionWake;
use crate::{EvalError, NativeResult, Root, Runtime};

pub(crate) type HostError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type HostAsyncFunction =
    dyn for<'rt> Fn(&'rt Runtime, &[Root<'rt>], HostCompletion) -> Result<(), HostError>
        + Send
        + Sync;

pub(super) type Materialize =
    Box<dyn for<'rt> FnOnce(&'rt Runtime, usize) -> Result<Vec<Root<'rt>>, Fault> + Send>;

pub(crate) enum HostReply {
    Complete(Materialize),
    Abandoned,
}

enum ReplySlot {
    Pending {
        registration: Option<Registration>,
        cancel_waker: Option<Waker>,
    },
    Ready(HostReply),
    Closed,
}

struct SharedOperation {
    cancelled: AtomicBool,
    reply: Mutex<ReplySlot>,
}

struct Registration {
    task: u64,
    events: Arc<HostEvents>,
}

/// A one-shot completion for a registered asynchronous host function. It can be moved to
/// another thread. Complete or drop it after the provider has finished using the operation's
/// resources. Dropping a pending completion reports abandonment to Lisp unless cancelled.
#[must_use = "complete the host operation or drop it to report abandonment"]
pub struct HostCompletion(Option<Arc<SharedOperation>>);

impl HostCompletion {
    /// Submit owned result data. `materialize` runs once on the runtime's owning thread
    /// if the invocation reaches this result. It can allocate zero or more rooted values.
    /// Returns `false` if the runtime has already retired the wait.
    pub fn complete<F>(mut self, materialize: F) -> bool
    where
        F: for<'rt> FnOnce(&'rt Runtime) -> NativeResult<'rt> + Send + 'static,
    {
        self.publish(HostReply::Complete(Box::new(move |runtime, at| {
            materialize(runtime).map_err(|error| EvalError::host_callback(at, error).into())
        })))
    }

    /// Check whether the runtime requests that the provider stop. The provider owns its
    /// resources until it finishes. A cooperative cancellation waits for this completion
    /// to be submitted or dropped before the Lisp task continues unwinding.
    pub fn is_cancelled(&self) -> bool {
        self.0.as_ref().unwrap().cancelled.load(Ordering::Acquire)
    }

    /// Wait for a request to stop without polling a timer. Keep driving the host
    /// operation until its resources are released, then complete or drop the token.
    /// Each call replaces the previous cancellation waker. A waker may be invoked
    /// on the runtime's thread, so it should schedule work and return promptly.
    pub fn poll_cancelled(&self, cx: &mut Context<'_>) -> Poll<()> {
        match self.replace_cancel_waker(cx.waker().clone()) {
            Ok(_) => Poll::Pending,
            Err(_) => Poll::Ready(()),
        }
    }

    pub(super) fn replace_cancel_waker(&self, waker: Waker) -> Result<Option<Waker>, Waker> {
        let operation = self.0.as_ref().unwrap();
        let mut state = operation.reply.lock().unwrap();
        match &mut *state {
            ReplySlot::Pending { cancel_waker, .. }
                if !operation.cancelled.load(Ordering::Relaxed) =>
            {
                Ok(cancel_waker.replace(waker))
            }
            _ => Err(waker),
        }
    }

    pub(super) fn publish(&mut self, reply: HostReply) -> bool {
        let Some(slot) = self.0.take() else {
            return false;
        };
        let (notify, cancel_waker) = {
            let mut state = slot.reply.lock().unwrap();
            if matches!(*state, ReplySlot::Closed) {
                return false;
            }
            let ReplySlot::Pending {
                registration,
                cancel_waker,
                ..
            } = std::mem::replace(&mut *state, ReplySlot::Ready(reply))
            else {
                unreachable!("only one producer owns the completion")
            };
            let notify = registration.and_then(|Registration { task, events }| {
                events
                    .push(HostNotification {
                        task,
                        reply: Arc::downgrade(&slot),
                    })
                    .then_some(events)
            });
            (notify, cancel_waker)
        };
        if let Some(events) = notify {
            events.wake.notify();
        }
        drop(cancel_waker);
        true
    }
}

impl Drop for HostCompletion {
    fn drop(&mut self) {
        self.publish(HostReply::Abandoned);
    }
}

/// The scheduler's exclusive right to consume one host reply and retire its registration.
pub(crate) struct HostOperation {
    slot: Arc<SharedOperation>,
    start_error: Option<Box<EvalError>>,
}

pub(crate) enum HostCancellation {
    Acknowledged,
    Waiting(Option<Waker>),
}

impl HostOperation {
    pub(crate) fn new() -> (Self, HostCompletion) {
        let slot = Arc::new(SharedOperation {
            cancelled: AtomicBool::new(false),
            reply: Mutex::new(ReplySlot::Pending {
                registration: None,
                cancel_waker: None,
            }),
        });
        (
            Self {
                slot: Arc::clone(&slot),
                start_error: None,
            },
            HostCompletion(Some(slot)),
        )
    }

    /// A completion that arrived before registration is already ready to dispatch.
    pub(crate) fn register(&self, task: u64, events: &Arc<HostEvents>) -> bool {
        let mut state = self.slot.reply.lock().unwrap();
        let ReplySlot::Pending { registration, .. } = &mut *state else {
            debug_assert!(matches!(*state, ReplySlot::Ready(_)));
            return false;
        };
        debug_assert!(registration.is_none());
        *registration = Some(Registration {
            task,
            events: Arc::clone(events),
        });
        true
    }

    /// Start cooperative cancellation. A ready reply also acknowledges that
    /// the provider has returned ownership of the operation's resources.
    pub(crate) fn request_cancel(&self) -> HostCancellation {
        match &mut *self.slot.reply.lock().unwrap() {
            ReplySlot::Pending { cancel_waker, .. } => {
                self.slot.cancelled.store(true, Ordering::Release);
                HostCancellation::Waiting(cancel_waker.take())
            }
            ReplySlot::Ready(_) => HostCancellation::Acknowledged,
            ReplySlot::Closed => unreachable!("a waiting host operation is live"),
        }
    }

    pub(crate) fn fail_start(&mut self, error: EvalError) -> Option<Waker> {
        self.start_error = Some(Box::new(error));
        match self.request_cancel() {
            HostCancellation::Waiting(waker) => waker,
            HostCancellation::Acknowledged => None,
        }
    }

    pub(crate) fn take_reply(&mut self) -> Option<Result<HostReply, EvalError>> {
        let reply = {
            let mut state = self.slot.reply.lock().unwrap();
            if !matches!(*state, ReplySlot::Ready(_)) {
                return None;
            }
            let ReplySlot::Ready(reply) = std::mem::replace(&mut *state, ReplySlot::Closed) else {
                unreachable!("reply checked under the same lock")
            };
            reply
        };
        Some(match self.start_error.take() {
            Some(error) => Err(*error),
            None => Ok(reply),
        })
    }

    pub(crate) fn matches(&self, notification: &HostNotification) -> bool {
        // The weak reference keeps the allocation identity unique even if an earlier
        // wait is retired before its notification is dispatched.
        std::ptr::eq(Arc::as_ptr(&self.slot), notification.reply.as_ptr())
    }
}

impl Drop for HostOperation {
    fn drop(&mut self) {
        let previous = {
            let mut state = self.slot.reply.lock().unwrap();
            self.slot.cancelled.store(true, Ordering::Release);
            std::mem::replace(&mut *state, ReplySlot::Closed)
        };
        if let ReplySlot::Pending {
            cancel_waker: Some(waker),
            ..
        } = previous
            && !std::thread::panicking()
        {
            waker.wake();
        }
    }
}

pub(crate) struct HostNotification {
    pub(crate) task: u64,
    reply: Weak<SharedOperation>,
}

pub(crate) struct HostEvents {
    wake: Arc<ExecutionWake>,
    ready: AtomicBool,
    notifications: Mutex<VecDeque<HostNotification>>,
}

impl HostEvents {
    pub(crate) fn new(wake: &Arc<ExecutionWake>) -> Arc<Self> {
        Arc::new(Self {
            wake: Arc::clone(wake),
            ready: AtomicBool::new(false),
            notifications: Mutex::default(),
        })
    }

    // Only the transition from empty to ready needs to wake the driver. The
    // driver observes this queue before parking and re-arms it by draining it.
    fn push(&self, notification: HostNotification) -> bool {
        let mut notifications = self.notifications.lock().unwrap();
        let was_empty = notifications.is_empty();
        notifications.push_back(notification);
        if was_empty {
            self.ready.store(true, Ordering::Release);
        }
        was_empty
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub(crate) fn drain(&self, mut dispatch: impl FnMut(HostNotification)) {
        let mut notifications = self.notifications.lock().unwrap();
        self.ready.store(false, Ordering::Relaxed);
        notifications.drain(..).for_each(&mut dispatch);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Wake, Waker};

    use super::{ExecutionWake, HostCancellation, HostEvents, HostOperation, HostReply};

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn queued_replies_notify_once_until_the_driver_drains_them() {
        let wake = Arc::new(ExecutionWake::default());
        let events = HostEvents::new(&wake);
        let (mut first, first_reply) = HostOperation::new();
        let (mut second, second_reply) = HostOperation::new();
        assert!(first.register(1, &events));
        assert!(second.register(2, &events));
        let generation = wake.generation();
        std::thread::spawn(move || {
            assert!(first_reply.complete(|_| Ok(Vec::new())));
            drop(second_reply);
        })
        .join()
        .unwrap();
        assert!(events.is_ready());
        assert_eq!(wake.generation(), generation + 1);
        let mut ready = Vec::new();
        events.drain(|event| ready.push(event.task));
        assert_eq!(ready, [1, 2]);
        assert!(!events.is_ready());
        assert!(matches!(
            first.take_reply(),
            Some(Ok(HostReply::Complete(_)))
        ));
        assert!(matches!(
            second.take_reply(),
            Some(Ok(HostReply::Abandoned))
        ));

        let (next, reply) = HostOperation::new();
        assert!(next.register(3, &events));
        drop(reply);
        assert!(events.is_ready());
        assert_eq!(wake.generation(), generation + 2);
    }

    #[test]
    fn cancellation_is_visible_before_the_provider_acknowledges() {
        let (operation, completion) = HostOperation::new();
        let count = Arc::new(WakeCount::default());
        let wake = Waker::from(Arc::clone(&count));
        assert!(
            completion
                .poll_cancelled(&mut Context::from_waker(&wake))
                .is_pending()
        );

        let HostCancellation::Waiting(Some(notify)) = operation.request_cancel() else {
            panic!("the provider must acknowledge the cancellation");
        };
        std::thread::scope(|scope| {
            scope.spawn(|| {
                assert!(completion.is_cancelled());
                assert!(
                    completion
                        .poll_cancelled(&mut Context::from_waker(&wake))
                        .is_ready()
                );
            });
        });
        notify.wake();
        assert_eq!(count.0.load(Ordering::Relaxed), 1);
        assert!(matches!(
            operation.request_cancel(),
            HostCancellation::Waiting(None)
        ));
        drop(completion);
        assert!(matches!(
            operation.request_cancel(),
            HostCancellation::Acknowledged
        ));
    }
}
