//! Smoke test: exercises the distilled annotation surface so a rename or
//! signature drift in `lock_annotations` breaks the build. Only the stable
//! subset is covered (item-level attributes); expression-position attributes
//! (`lock_identity` / `same_lock` on a block) require nightly
//! `proc_macro_hygiene` and are validated by consumers.

use lock_annotations::{
    foreign, lock, lock_acquire, lock_guard, lock_new, lock_release, mhp, mhp_group,
};

// ---- Tier R — bespoke lock recognition (type = sibling marker) ----
#[lock]
struct SpinLock {
    held: bool,
}

struct SpinGuard<'a>(&'a SpinLock);

impl SpinLock {
    #[lock_new]
    fn new() -> Self {
        SpinLock { held: false }
    }

    #[lock_acquire]
    fn lock(&self) -> SpinGuard<'_> {
        SpinGuard(self)
    }
}

// impl-level attribute nests its marker into the impl body.
#[lock_guard]
impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {}
}

// Explicit-unlock primitive (Tier R, no guard).
struct RawMutex;
impl RawMutex {
    #[lock_acquire(read)]
    fn read_lock(&self) {}
    #[lock_release]
    fn unlock(&self) {}
}

// ---- Tier F — foreign summary (attrs on Rust wrapper methods) ----
struct Futex;
impl Futex {
    #[foreign(wait, on = self.inner, blocks)]
    fn block(&self) {}
    #[foreign(wake, on = self.inner)]
    fn wake_many(&self) {}
    #[foreign(acquire, on = self.inner)]
    fn ffi_lock(&self) {}
}

// ---- Tier M — may-happen-in-parallel (MHP) groups ----
#[mhp_group("syscalls")]
fn dispatch() {}

struct Shim;
impl Shim {
    #[mhp("syscalls")]
    fn sys_close(&self) {}
    #[mhp("syscalls")]
    fn sys_dup(&self) {}
}

#[test]
fn expands() {
    let m = SpinLock::new();
    let _g = m.lock();
    let raw = RawMutex;
    raw.read_lock();
    raw.unlock();
    let _f = Futex;
    dispatch();
    let s = Shim;
    s.sys_close();
    s.sys_dup();
}
