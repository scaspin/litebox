// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Loom models for platform synchronization primitives.
//!
//! These types are intended for tests in this crate and dependent crates. They
//! model platform behavior with Loom primitives, but they are not production
//! platform implementations.

use core::sync::atomic::Ordering;

pub use loom::sync::{Arc, atomic};
use loom::sync::{Condvar, Mutex};

use super::{ImmediatelyWokenUp, UnblockedOrTimedOut};

/// A Loom model of a futex word and its wait queue.
///
/// `block` models `FUTEX_WAIT`: while holding the internal queue lock, it checks
/// whether the word still equals the expected value and only then parks on the
/// condition variable. `wake_many` holds the same queue lock while selecting and
/// notifying waiters, so a wake cannot be lost between the value check and the
/// wait operation.
pub struct LoomFutex {
    word: atomic::AtomicU32,
    queue: Mutex<WaitQueue>,
    condvar: Condvar,
}

#[derive(Default)]
struct WaitQueue {
    waiters: usize,
    pending_wakeups: usize,
}

impl LoomFutex {
    /// Creates a new futex model with the given initial word value.
    pub fn new(value: u32) -> Self {
        Self {
            word: atomic::AtomicU32::new(value),
            queue: Mutex::new(WaitQueue::default()),
            condvar: Condvar::new(),
        }
    }

    /// Returns the modeled futex word.
    pub fn word(&self) -> &atomic::AtomicU32 {
        &self.word
    }

    /// Wakes up to `n` waiters blocked on this futex.
    ///
    /// The returned value is the number of waiters selected for wakeup. This
    /// mirrors futex semantics: selected waiters are no longer eligible for a
    /// second wake, even if they have not yet resumed execution.
    ///
    /// # Panics
    ///
    /// Panics if Loom reports that the modeled wait-queue mutex has been
    /// poisoned.
    pub fn wake_many(&self, n: usize) -> usize {
        let mut queue = self.queue.lock().unwrap();
        let eligible = queue.waiters - queue.pending_wakeups;
        let to_wake = eligible.min(n);
        queue.pending_wakeups += to_wake;

        for _ in 0..to_wake {
            self.condvar.notify_one();
        }

        to_wake
    }

    /// Wakes one waiter blocked on this futex.
    pub fn wake_one(&self) -> bool {
        self.wake_many(1) > 0
    }

    /// Wakes all currently eligible waiters blocked on this futex.
    pub fn wake_all(&self) -> usize {
        self.wake_many(usize::MAX)
    }

    /// Blocks if the futex word is still equal to `expected`.
    ///
    /// This returns after a wake operation selects this waiter. A wake does not
    /// imply that the futex word has changed; callers must re-check their own
    /// synchronization state, just as they would around a real futex wait.
    ///
    /// # Panics
    ///
    /// Panics if Loom reports that the modeled wait-queue mutex or condition
    /// variable has been poisoned.
    pub fn block(&self, expected: u32) -> Result<(), ImmediatelyWokenUp> {
        let mut queue = self.queue.lock().unwrap();
        if self.word.load(Ordering::Acquire) != expected {
            return Err(ImmediatelyWokenUp);
        }

        queue.waiters += 1;
        loop {
            queue = self.condvar.wait(queue).unwrap();
            if queue.pending_wakeups > 0 {
                queue.pending_wakeups -= 1;
                queue.waiters -= 1;
                return Ok(());
            }
        }
    }

    /// Blocks like [`Self::block`].
    ///
    /// Loom's condition variable does not model timeouts, so this returns
    /// `Unblocked` after a modeled wake and never produces `TimedOut`.
    ///
    /// # Panics
    ///
    /// Panics under the same conditions as [`Self::block`].
    pub fn block_or_timeout(
        &self,
        expected: u32,
        _timeout: core::time::Duration,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        self.block(expected)
            .map(|()| UnblockedOrTimedOut::Unblocked)
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;

    use super::{Arc, LoomFutex};

    #[test]
    fn wait_does_not_miss_wake_between_check_and_park() {
        loom::model(|| {
            let futex = Arc::new(LoomFutex::new(0));

            let waiter = {
                let futex = Arc::clone(&futex);
                loom::thread::spawn(move || {
                    let _ = futex.block(0);
                })
            };

            let waker = {
                let futex = Arc::clone(&futex);
                loom::thread::spawn(move || {
                    futex.word().store(1, Ordering::Release);
                    futex.wake_one();
                })
            };

            waiter.join().unwrap();
            waker.join().unwrap();
        });
    }
}
