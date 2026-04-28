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

use super::{
    ImmediatelyWokenUp, Instant, RawAtomicU32, RawMutex, RawMutexProvider, SystemTime,
    TimeProvider, UnblockedOrTimedOut,
};

/// A Loom model of a futex word and its wait queue.
///
/// `block` models `FUTEX_WAIT`: while holding the internal queue lock, it checks
/// whether the word still equals the expected value and only then parks on the
/// condition variable. `wake_many` holds the same queue lock while selecting and
/// notifying waiters, so a wake cannot be lost between the value check and the
/// wait operation.
pub struct LoomFutex {
    word: RawAtomicU32,
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

/// A [`RawMutex`] implementation backed by [`LoomFutex`].
pub struct LoomRawMutex {
    futex: LoomFutex,
}

impl LoomRawMutex {
    /// Creates a new Loom raw mutex.
    pub fn new() -> Self {
        Self {
            futex: LoomFutex::new(0),
        }
    }
}

impl Default for LoomRawMutex {
    fn default() -> Self {
        Self::new()
    }
}

impl RawMutex for LoomRawMutex {
    fn new() -> Self {
        LoomRawMutex::new()
    }

    fn underlying_atomic(&self) -> &RawAtomicU32 {
        self.futex.word()
    }

    fn wake_many(&self, n: usize) -> usize {
        self.futex.wake_many(n)
    }

    fn block(&self, val: u32) -> Result<(), ImmediatelyWokenUp> {
        self.futex.block(val)
    }

    fn block_or_timeout(
        &self,
        val: u32,
        timeout: core::time::Duration,
    ) -> Result<UnblockedOrTimedOut, ImmediatelyWokenUp> {
        self.futex.block_or_timeout(val, timeout)
    }
}

/// A minimal platform for Loom tests of raw synchronization primitives.
pub struct LoomPlatform {
    current_time: atomic::AtomicU64,
}

impl LoomPlatform {
    /// Creates a new Loom platform model.
    pub fn new() -> Self {
        Self {
            current_time: atomic::AtomicU64::new(0),
        }
    }
}

impl Default for LoomPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl RawMutexProvider for LoomPlatform {
    type RawMutex = LoomRawMutex;
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LoomInstant {
    time: u64,
}

impl Instant for LoomInstant {
    fn checked_duration_since(&self, earlier: &Self) -> Option<core::time::Duration> {
        if earlier.time <= self.time {
            Some(core::time::Duration::from_millis(self.time - earlier.time))
        } else {
            None
        }
    }

    fn checked_add(&self, duration: core::time::Duration) -> Option<Self> {
        let duration_millis: u64 = duration.as_millis().try_into().ok()?;
        Some(Self {
            time: self.time.checked_add(duration_millis)?,
        })
    }
}

pub struct LoomSystemTime {
    time: u64,
}

impl SystemTime for LoomSystemTime {
    const UNIX_EPOCH: Self = Self { time: 0 };

    fn duration_since(&self, earlier: &Self) -> Result<core::time::Duration, core::time::Duration> {
        match self.time.cmp(&earlier.time) {
            core::cmp::Ordering::Less => {
                Err(core::time::Duration::from_millis(earlier.time - self.time))
            }
            core::cmp::Ordering::Equal => Ok(core::time::Duration::from_millis(0)),
            core::cmp::Ordering::Greater => {
                Ok(core::time::Duration::from_millis(self.time - earlier.time))
            }
        }
    }
}

impl TimeProvider for LoomPlatform {
    type Instant = LoomInstant;
    type SystemTime = LoomSystemTime;

    fn now(&self) -> Self::Instant {
        LoomInstant {
            time: self.current_time.fetch_add(1, Ordering::SeqCst),
        }
    }

    fn current_time(&self) -> Self::SystemTime {
        LoomSystemTime {
            time: self.current_time.load(Ordering::SeqCst),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::Ordering;

    use super::{Arc, LoomFutex};
    use crate::platform::{ImmediatelyWokenUp, UnblockedOrTimedOut};

    #[test]
    fn block_returns_immediately_when_word_mismatches() {
        loom::model(|| {
            let futex = LoomFutex::new(1);

            match futex.block(0) {
                Err(ImmediatelyWokenUp) => {}
                Ok(()) => panic!("futex wait should not block when the word already differs"),
            }
        });
    }

    #[test]
    fn block_or_timeout_returns_immediately_when_word_mismatches() {
        loom::model(|| {
            let futex = LoomFutex::new(1);

            match futex.block_or_timeout(0, core::time::Duration::from_secs(1)) {
                Err(ImmediatelyWokenUp) => {}
                Ok(UnblockedOrTimedOut::Unblocked) => {
                    panic!("futex wait should not consume a wake when the word already differs")
                }
                Ok(UnblockedOrTimedOut::TimedOut) => {
                    panic!("LoomFutex does not model timeout expiration")
                }
            }
        });
    }

    #[test]
    fn wake_many_returns_zero_without_waiters() {
        loom::model(|| {
            let futex = LoomFutex::new(0);

            assert_eq!(futex.wake_many(1), 0);
            assert_eq!(futex.wake_all(), 0);
        });
    }

    #[test]
    fn wake_one_does_not_select_the_same_waiter_twice() {
        loom::model(|| {
            let futex = LoomFutex::new(0);
            futex.queue.lock().unwrap().waiters = 1;

            assert!(futex.wake_one());
            assert!(!futex.wake_one());

            let queue = futex.queue.lock().unwrap();
            assert_eq!(queue.waiters, 1);
            assert_eq!(queue.pending_wakeups, 1);
        });
    }

    #[test]
    fn wake_all_selects_all_eligible_waiters() {
        loom::model(|| {
            let futex = LoomFutex::new(0);
            futex.queue.lock().unwrap().waiters = 3;

            assert_eq!(futex.wake_all(), 3);

            let queue = futex.queue.lock().unwrap();
            assert_eq!(queue.waiters, 3);
            assert_eq!(queue.pending_wakeups, 3);
        });
    }

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
