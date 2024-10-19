
pub mod select;
pub mod r#async;

mod signal;
mod error;
mod iterator;
mod weak;

// Reexports
pub use select::Selector;
pub use self::error::*;
pub use self::iterator::*;
pub use self::weak::*;

use std::{
    collections::VecDeque,
    sync::{Arc, atomic::{AtomicUsize, AtomicBool, Ordering}, Weak, Mutex},
    time::{Duration, Instant},
    marker::PhantomData,
    thread,
    fmt::{self, Formatter},
};
use crate::signal::{Signal, SyncSignal};

enum TrySendTimeoutError<T> {
    Full(T),
    Disconnected(T),
    Timeout(T),
}

enum TryRecvTimeoutError {
    Empty,
    Timeout,
    Disconnected,
}

/// shared channel state
struct Shared<T> {
    /// lockable channel state
    lockable: Mutex<Lockable<T>>,
    /// whether the channel is disconnected
    disconnected: AtomicBool,
    /// sender ref count
    send_count: AtomicUsize,
    /// receiver ref count
    recv_count: AtomicUsize,
}

/// lockable channel state
struct Lockable<T> {
    /// messages in the channel
    queue: VecDeque<T>,
    /// if the channel is bounded, state for things blocking on sending
    send_waiting: Option<SendWaiting<T>>,
    /// hook for each thread-like blocking on receiving a message
    recv_waiting: VecDeque<Hook<T, dyn Signal>>,
}

/// see Lockable.send_waiting
struct SendWaiting<T> {
    /// channel capacity
    cap: usize,
    /// hook for each thread-like blocking on sending a message
    signals: VecDeque<Hook<T, dyn Signal>>,
}

/// reference-counted struct of optional mutex-guarded message "slot" plus dyn-able notify signal
struct Hook<T, S: ?Sized>(Arc<HookInner<T, S>>);

/// see Hook
struct HookInner<T, S: ?Sized> {
    slot: Option<Mutex<Option<T>>>,
    signal: S
}

impl<T, S: Signal> Hook<T, S> {
    /// construct with slot
    fn new_slot(msg: Option<T>, signal: S) -> Self {
        Self(Arc::new(HookInner { slot: Some(Mutex::new(msg)), signal }))
    }
    
    /// construct without slot
    fn new_trigger(signal: S) -> Self {
        Self(Arc::new(HookInner { slot: None, signal }))
    }
    
    /// upcast into dyn signal
    fn into_dyn(self) -> Hook<T, dyn Signal> {
        Hook(self.0)
    }
}

impl<T, S: ?Sized + Signal> Clone for Hook<T, S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T, S: ?Sized + Signal> Hook<T, S> {
    /// get the mutex-guarded slot by optional reference
    fn slot(&self) -> Option<&Mutex<Option<T>>> {
        self.0.slot.as_ref()
    }
    
    /// get the signal
    fn signal(&self) -> &S {
        &self.0.signal
    }

    /// false if there is a full slot, true if there is either an empty slot or no slot
    fn is_empty(&self) -> bool {
        self.slot().map(|slot| slot.lock().unwrap().is_none()).unwrap_or(true)
    }

    /// take the message from the slot
    fn take(&self) -> Option<T> {
        self.slot().unwrap().lock().unwrap().take()
    }
}

impl<T> Hook<T, SyncSignal> {
    /// repeatedly park the thread on the signal until a message can be taken from the slot or
    /// `abort` indicates that the channel is disconnected
    fn wait_recv(&self, abort: &AtomicBool) -> Option<T> {
        loop {
            let disconnected = abort.load(Ordering::SeqCst); // Check disconnect *before* msg
            let msg = self.take();
            if let Some(msg) = msg {
                break Some(msg);
            } else if disconnected {
                break None;
            } else {
                self.signal().wait()
            }
        }
    }

    /// like `wait_recv` but with timeout. returns `Err(true)` if timeout
    fn wait_deadline_recv(&self, abort: &AtomicBool, deadline: Instant) -> Result<T, bool> {
        loop {
            let disconnected = abort.load(Ordering::SeqCst); // Check disconnect *before* msg
            if let Some(msg) = self.take() {
                break Ok(msg);
            } else if disconnected {
                break Err(false);
            } else if let Some(dur) = deadline.checked_duration_since(Instant::now()) {
                self.signal().wait_timeout(dur);
            } else {
                break Err(true);
            }
        }
    }

    /// repeatedly park the thread on the signal until `self.is_empty()` or `abort` indicates that
    /// the channel is disconnected
    fn wait_send(&self, abort: &AtomicBool) {
        loop {
            let disconnected = abort.load(Ordering::SeqCst); // Check disconnect *before* msg
            if disconnected || self.is_empty() {
                break;
            }
            self.signal().wait();
        }
    }

    /// like `wait_send` but with timeout. returns `Err(true)` if timeout
    fn wait_deadline_send(&self, abort: &AtomicBool, deadline: Instant) -> Result<(), bool> {
        loop {
            let disconnected = abort.load(Ordering::SeqCst); // Check disconnect *before* msg
            if self.is_empty() {
                break Ok(());
            } else if disconnected {
                break Err(false);
            } else if let Some(dur) = deadline.checked_duration_since(Instant::now()) {
                self.signal().wait_timeout(dur);
            } else {
                break Err(true);
            }
        }
    }
}

impl<T> Lockable<T> {
    /// pop and resolve as many hooks in `send_waiting` as possible, by moving their message to
    /// the queue and notifying their signals. if `pull_extra` is true, the queue is allowed to
    /// have one more message than its capacity.
    fn pull_pending(&mut self, pull_extra: bool) {
        if let Some(send_waiting) = &mut self.send_waiting {
            let effective_cap = send_waiting.cap + pull_extra as usize;

            while self.queue.len() < effective_cap {
                if let Some(s) = send_waiting.signals.pop_front() {
                    let msg = s.take().unwrap();
                    s.signal().fire();
                    self.queue.push_back(msg);
                } else {
                    break;
                }
            }
        }
    }

    /// if the queue is non-empty, pop and notify a hook from `recv_waiting` if able. if the hook
    /// contained a stream signal, try to pop and notify more until one isn't.
    fn try_wake_receiver_if_pending(&mut self) {
        if !self.queue.is_empty() {
            while Some(false) == self.recv_waiting.pop_front().map(|s| s.signal().fire()) {}
        }
    }
}

impl<T> Shared<T> {
    /// construct
    fn new(cap: Option<usize>) -> Self {
        Self {
            lockable: Mutex::new(Lockable {
                send_waiting: cap.map(|cap| SendWaiting {
                    cap,
                    signals: Default::default(),
                }),
                queue: Default::default(),
                recv_waiting: Default::default(),
            }),
            disconnected: AtomicBool::new(false),
            send_count: AtomicUsize::new(1),
            recv_count: AtomicUsize::new(1),
        }
    }

    /// inner routine to send a message into the channel. if should block, calls `make_hook`, adds
    /// one handle as a `send_waiting` hook, and returns a second handle in `Ok(Some)`.
    fn send_inner<S: Signal>(
        &self,
        msg: T,
        may_block: bool,
        make_hook: impl FnOnce(T) -> Hook<T, S>,
    ) -> Result<Option<Hook<T, S>>, TrySendTimeoutError<T>> {
        let mut lockable = self.lockable.lock().unwrap();

        if self.is_disconnected() {
            // channel disconnected, return error
            Err(TrySendTimeoutError::Disconnected(msg))
        } else if !lockable.recv_waiting.is_empty() {
            // try giving the message to recv_waiting hooks before pushing to the queue
            loop {
                match lockable.recv_waiting.pop_front() {
                    None => {
                        // exhausted hooks, push to queue
                        lockable.queue.push_back(msg);
                        break;
                    }
                    Some(hook) => if let Some(slot) = hook.slot() {
                        // hook with a slot (sync). give the message to the hook and fire it.
                        *slot.lock().unwrap() = Some(msg);
                        drop(lockable);
                        hook.signal().fire();
                        break;
                    } else if !hook.signal().fire() {
                        // non-stream hook without slot (async non-stream). it was fired. push the
                        // message to the queue to it can be taken.
                        lockable.queue.push_back(msg);
                        break;
                    }
                }
            }
            Ok(None)
        } else if lockable.send_waiting.as_ref().map(|send_waiting| lockable.queue.len() < send_waiting.cap).unwrap_or(true) {
            // simply put the message in the queue
            lockable.queue.push_back(msg);
            Ok(None)
        } else if may_block {
            // queue is full, create a hook, put one handle in `send_waiting` and return another
            let hook = make_hook(msg);
            lockable.send_waiting.as_mut().unwrap().signals.push_back(hook.clone().into_dyn());
            drop(lockable);
            Ok(Some(hook))
        } else {
            // queue is full and blocking is not allowed
            Err(TrySendTimeoutError::Full(msg))
        }
    }

    /// call `send_inner` and uses a `SyncSignal` to block if necessary.
    fn send_blocking(
        &self,
        msg: T,
        block: Option<Option<Instant>>,
    ) -> Result<(), TrySendTimeoutError<T>> {
        if let Some(hook) = self.send_inner(
            msg,
            block.is_some(),
            |msg| Hook::new_slot(Some(msg), SyncSignal::default()),
        )? {
            if let Some(deadline) = block.unwrap() {
                hook.wait_deadline_send(&self.disconnected, deadline)
                    .or_else(|timed_out| {
                        if timed_out { // Remove our signal
                            let hook = hook.clone();
                            self.lockable.lock().unwrap().send_waiting
                                .as_mut()
                                .unwrap().signals
                                .retain(|s| s.signal().as_ptr() != hook.signal().as_ptr());
                        }
                        hook.take().map(|msg| if self.is_disconnected() {
                            Err(TrySendTimeoutError::Disconnected(msg))
                        } else {
                            Err(TrySendTimeoutError::Timeout(msg))
                        })
                        .unwrap_or(Ok(()))
                    })?;
            } else {
                hook.wait_send(&self.disconnected);

                if let Some(msg) = hook.take() {
                    return Err(TrySendTimeoutError::Disconnected(msg));
                }
            }
        }
        Ok(())
    }

    fn recv<S: Signal, R: From<Result<T, TryRecvTimeoutError>>>(
        &self,
        should_block: bool,
        make_signal: impl FnOnce() -> Hook<T, S>,
        do_block: impl FnOnce(Hook<T, S>) -> R,
    ) -> R {
        let mut lockable = self.lockable.lock().unwrap();
        lockable.pull_pending(true);

        if let Some(msg) = lockable.queue.pop_front() {
            drop(lockable);
            Ok(msg).into()
        } else if self.is_disconnected() {
            drop(lockable);
            Err(TryRecvTimeoutError::Disconnected).into()
        } else if should_block {
            let hook = make_signal();
            lockable.recv_waiting.push_back(hook.clone().into_dyn());
            drop(lockable);

            do_block(hook)
        } else {
            drop(lockable);
            Err(TryRecvTimeoutError::Empty).into()
        }
    }

    fn recv_sync(&self, block: Option<Option<Instant>>) -> Result<T, TryRecvTimeoutError> {
        self.recv(
            // should_block
            block.is_some(),
            // make_signal
            || Hook::new_slot(None, SyncSignal::default()),
            // do_block
            |hook| if let Some(deadline) = block.unwrap() {
                hook.wait_deadline_recv(&self.disconnected, deadline)
                    .or_else(|timed_out| {
                        if timed_out { // Remove our signal
                            let hook = hook.clone();
                            self.lockable.lock().unwrap().recv_waiting
                                .retain(|s| s.signal().as_ptr() != hook.signal().as_ptr());
                        }
                        match hook.take() {
                            Some(msg) => Ok(msg),
                            None => {
                                let disconnected = self.is_disconnected(); // Check disconnect *before* msg
                                if let Some(msg) = self.lockable.lock().unwrap().queue.pop_front() {
                                    Ok(msg)
                                } else if disconnected {
                                    Err(TryRecvTimeoutError::Disconnected)
                                } else {
                                    Err(TryRecvTimeoutError::Timeout)
                                }
                            },
                        }
                    })
            } else {
                hook.wait_recv(&self.disconnected)
                    .or_else(|| self.lockable.lock().unwrap().queue.pop_front())
                    .ok_or(TryRecvTimeoutError::Disconnected)
            },
        )
    }

    /// Disconnect anything listening on this channel (this will not prevent receivers receiving
    /// msgs that have already been sent)
    fn disconnect_all(&self) {
        self.disconnected.store(true, Ordering::Relaxed);

        let mut lockable = self.lockable.lock().unwrap();
        lockable.pull_pending(false);
        if let Some(send_waiting) = lockable.send_waiting.as_ref() {
            send_waiting.signals.iter().for_each(|hook| {
                hook.signal().fire();
            })
        }
        lockable.recv_waiting.iter().for_each(|hook| {
            hook.signal().fire();
        });
    }

    fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn is_full(&self) -> bool {
        self.capacity().map(|cap| cap == self.len()).unwrap_or(false)
    }

    fn len(&self) -> usize {
        let mut lockable = self.lockable.lock().unwrap();
        lockable.pull_pending(false);
        lockable.queue.len()
    }

    fn capacity(&self) -> Option<usize> {
        self.lockable.lock().unwrap().send_waiting.as_ref().map(|send_waiting| send_waiting.cap)
    }

    fn sender_count(&self) -> usize {
        self.send_count.load(Ordering::Relaxed)
    }

    fn receiver_count(&self) -> usize {
        self.recv_count.load(Ordering::Relaxed)
    }
}


pub struct Sender<T>(Arc<Shared<T>>);

pub struct Receiver<T>(Arc<Shared<T>>);

impl<T> Sender<T> {
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        self.0.send_blocking(msg, None).map_err(|err| match err {
            TrySendTimeoutError::Full(msg) => TrySendError::Full(msg),
            TrySendTimeoutError::Disconnected(msg) => TrySendError::Disconnected(msg),
            _ => unreachable!(),
        })
    }

    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        self.0.send_blocking(msg, Some(None)).map_err(|err| match err {
            TrySendTimeoutError::Disconnected(msg) => SendError(msg),
            _ => unreachable!(),
        })
    }

    pub fn send_deadline(&self, msg: T, deadline: Instant) -> Result<(), SendTimeoutError<T>> {
        self.0.send_blocking(msg, Some(Some(deadline))).map_err(|err| match err {
            TrySendTimeoutError::Disconnected(msg) => SendTimeoutError::Disconnected(msg),
            TrySendTimeoutError::Timeout(msg) => SendTimeoutError::Timeout(msg),
            _ => unreachable!(),
        })
    }

    pub fn send_timeout(&self, msg: T, dur: Duration) -> Result<(), SendTimeoutError<T>> {
        self.send_deadline(msg, Instant::now().checked_add(dur).unwrap())
    }

    pub fn is_disconnected(&self) -> bool {
        self.0.is_disconnected()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.0.is_full()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn capacity(&self) -> Option<usize> {
        self.0.capacity()
    }

    pub fn sender_count(&self) -> usize {
        self.0.sender_count()
    }

    pub fn receiver_count(&self) -> usize {
        self.0.receiver_count()
    }

    pub fn downgrade(&self) -> WeakSender<T> {
        WeakSender {
            shared: Arc::downgrade(&self.0),
        }
    }

    pub fn same_channel(&self, other: &Sender<T>) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.send_count.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Sender").finish()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // Notify receivers that all senders have been dropped if the number of senders drops to 0.
        if self.0.send_count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.0.disconnect_all();
        }
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.0.recv_sync(None).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => TryRecvError::Disconnected,
            TryRecvTimeoutError::Empty => TryRecvError::Empty,
            _ => unreachable!(),
        })
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        self.0.recv_sync(Some(None)).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => RecvError::Disconnected,
            _ => unreachable!(),
        })
    }

    pub fn recv_deadline(&self, deadline: Instant) -> Result<T, RecvTimeoutError> {
        self.0.recv_sync(Some(Some(deadline))).map_err(|err| match err {
            TryRecvTimeoutError::Disconnected => RecvTimeoutError::Disconnected,
            TryRecvTimeoutError::Timeout => RecvTimeoutError::Timeout,
            _ => unreachable!(),
        })
    }

    pub fn recv_timeout(&self, dur: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_deadline(Instant::now().checked_add(dur).unwrap())
    }

    pub fn iter(&self) -> Iter<T> {
        Iter { receiver: &self }
    }

    pub fn try_iter(&self) -> TryIter<T> {
        TryIter { receiver: &self }
    }

    pub fn drain(&self) -> Drain<T> {
        let mut lockable = self.0.lockable.lock().unwrap();
        lockable.pull_pending(false);
        let queue = std::mem::take(&mut lockable.queue);

        Drain { queue, _phantom: PhantomData }
    }

    pub fn is_disconnected(&self) -> bool {
        self.0.is_disconnected()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.0.is_full()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn capacity(&self) -> Option<usize> {
        self.0.capacity()
    }

    pub fn sender_count(&self) -> usize {
        self.0.sender_count()
    }

    pub fn receiver_count(&self) -> usize {
        self.0.receiver_count()
    }

    pub fn same_channel(&self, other: &Receiver<T>) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.0.recv_count.fetch_add(1, Ordering::Relaxed);
        Self(self.0.clone())
    }
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Receiver").finish()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // Notify senders that all receivers have been dropped if the number of receivers drops
        // to 0.
        if self.0.recv_count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.0.disconnect_all();
        }
    }
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared::new(None));
    (Sender(shared.clone()), Receiver(shared))
}

pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared::new(Some(cap)));
    (Sender(shared.clone()), Receiver(shared))
}
