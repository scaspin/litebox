# `lock_annotations`

Trusted, last-resort annotations that teach a MIR-level concurrency analyzer the
few synchronization facts it **cannot** recover from the code itself.

Every macro in this crate is a **no-op for normal compilation**. It injects a
uniquely-named, never-called marker function whose name hex-encodes the
annotation, so the analyzer can read the fact back purely from def-paths — no
runtime effect, no behavioral change, nothing to link.

---

## 1. The golden rule: annotate only what MIR cannot recover

The analyzer (Tracks A/B) already derives, from ordinary Rust it can see:

- the ordered trace of lock ops (`Mutex::lock`, guard `Drop`, `Condvar::wait`, …),
- aliasing (clone / deref / `Arc`-wrap), allocation sites, `self`/formal identity,
- per-call bindings, cross-crate summaries, and the whole `std::sync` surface.

**None of that should ever be annotated.** Every annotation here is a *trusted*
input: a wrong one is unsound (a missed deadlock), so the bar is high —
"MIR genuinely cannot tell."

> **Accessibility test.** "Foreign" means **not Rust we have MIR for** — a raw
> `syscall`, inline asm, an `extern "C"` function, an HV-call. If it's a Rust
> crate the analyzer can see (your crate, a workspace member, or a dependency
> with available MIR), it is analyzed normally — **do not annotate it.**

If you find yourself reaching for an annotation to describe plain, visible Rust,
stop: the analyzer already knows.

---

## 2. The four tiers — **FIRM**

The surface is exactly four tiers, spelling **FIRM**:

| Tier | Name | You reach for it when… | Attributes |
|---|---|---|---|
| **F** | Foreign summary | the body is genuinely non-Rust (syscall / asm / FFI) — no MIR to analyze | `foreign` |
| **I** | Identity repair | the alias chain is **severed** (`transmute`, `from_raw`, an FFI handle, a HW register) | `define_lock` · `lock_identity` · `same_lock` |
| **R** | Recognition | a **non-std** type/method *is* a lock op, but isn't `std::sync` so it isn't recognized | `lock` · `lock_new` · `lock_acquire` · `lock_release` · `lock_guard` |
| **M** | MHP groups | code may run **in parallel** from dispatch the analyzer can't see (syscalls, handlers) | `mhp` · `mhp_group` |

Cheat sheet:

```text
F  #[foreign(acquire|release|access|wait|wake, on = …)]   opaque/FFI effect; no ordering
I  #[define_lock(NAME = PLACE)]                            mint an anchor with no birth site
   #[lock_identity(NAME)] <expr>                           re-attach a value to a cross-scope NAME
   #[same_lock(a, b)]                                      assert two in-scope places are one lock
R  #[lock] / #[lock_new] / #[lock_acquire(read?)]          recognize a bespoke lock type & ops
   #[lock_release] / #[lock_guard]
M  #[mhp_group("name")] / #[mhp("name", key = …)]         declare a may-happen-in-parallel clique
                                                          (key partitions it; defaults to self)
```

---

## 3. Setup

```toml
# Cargo.toml
[dependencies]
lock_annotations = { path = "../lock_annotations" }
```

```rust
use lock_annotations::{lock, lock_new, lock_acquire, lock_guard, foreign, mhp, mhp_group};
```

All attributes work on stable for item positions (functions, methods, types,
`impl` blocks). The two **expression-position** identity attributes
(`#[lock_identity(...)] <expr>` and `#[same_lock(...)] <expr>`) attach to a
statement/block and therefore need nightly (`proc_macro_hygiene`), which the
analyzer's toolchain already enables.

---

## 4. Tier R — Recognition (bespoke / non-std locks)

Teach the analyzer that a user-defined type and its methods are lock operations.
Identity is **not** annotated here — the lock's field is a real MIR place, so the
same resolver used for `std` locks recovers it for free. You only mark the
*recognition* of the ops; bespoke primitives then inherit clone/`Arc`-wrap
aliasing, cross-thread spawn carrying, and summaries unchanged.

| Attribute | Put it on | Lowers to |
|---|---|---|
| `#[lock]` | the lock **type** | declares the type an anchor *class* (anchorable before any acquire) |
| `#[lock_new]` | a **constructor** | birth site → `Declare` + a fresh `Alloc` anchor |
| `#[lock_acquire(read?)]` | an **acquire method** | `Acquire`; `read` ⇒ shared (rwlock-like), else `Write` |
| `#[lock_release]` | an **explicit unlock method** | `Release{via: ExplicitCall}` |
| `#[lock_guard]` | the **guard type's `Drop` impl** | guard drop → `Release{via: GuardDrop}` — no call-site annotation |

Mode is **inferred from roles**: the presence of any `#[lock_acquire(read)]`
makes the type shared; otherwise acquires are exclusive (`Write`).

### 4a. RAII guard primitive — from [`tests/smoke.rs`](tests/smoke.rs)

A hand-rolled spinlock. Four markers turn it into a first-class lock the whole
pipeline understands (double-lock, order-inversion, etc.), with zero new analysis.

```rust
#[lock]                                   // SpinLock is an anchor class
struct SpinLock {
    held: bool,
}

struct SpinGuard<'a>(&'a SpinLock);

impl SpinLock {
    #[lock_new]                           // birth site → Declare + fresh Alloc anchor
    fn new() -> Self {
        SpinLock { held: false }
    }

    #[lock_acquire]                       // → Acquire{Write, via: GuardDrop}
    fn lock(&self) -> SpinGuard<'_> {
        SpinGuard(self)
    }
}

// The guard's Drop is the release — nothing to write at each `}`.
#[lock_guard]                             // Drop ⇒ Release{via: GuardDrop}
impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {}
}
```

> Note `#[lock]` on a `struct` and `#[lock_guard]` on an `impl` emit their marker
> as a *sibling* item (a struct field-list / trait-impl body can't hold an extra
> `fn`); everything else nests the marker inside the body. You don't need to think
> about this — it's automatic — but it's why the `Self` type name is encoded in
> the marker for those cases.

### 4b. Explicit-unlock primitive (no RAII guard) — from `tests/smoke.rs`

A platform mutex unlocked by an explicit call, so there is no guard `Drop`:

```rust
struct RawMutex;

impl RawMutex {
    #[lock_acquire(read)]                 // `read` role ⇒ this type is shared / rwlock-like
    fn read_lock(&self) {}

    #[lock_release]                       // pairs the non-RAII unlock with its acquire
    fn unlock(&self) {}
}
```

Without `#[lock_release]` the `unlock()` call is opaque and the lock looks held
forever.

---

## 5. Tier F — Foreign summary (opaque / FFI bodies)

Use **only** at a genuine non-Rust leaf — a `syscall`, inline asm, an
`extern "C"` function, an HV-call — where there is *no MIR to summarize*. The Rust
wrappers **above** that leaf are analyzed normally and inherit the effect by
composition; do not annotate them (though annotating the nearest Rust method is an
accepted pragmatic shortcut when you choose not to descend to the leaf).

A foreign body tells us nothing about memory ordering, so a foreign contract is an
**unordered, identity-tagged bag of effects — no ordering, no predicate.**

```text
#[foreign(MODE, on = …)]     MODE ∈ acquire | release | access | wait | wake
```

- **`acquire` / `release` / `access`** — mutual-exclusion effects. Feed wait-for /
  lock-ordering (so a cross-thread `a.lock(); b.lock()` vs `b.lock(); a.lock()`
  inversion is still caught). They contribute **no** happens-before edge.
- **`wait` / `wake`** — the futex/park rendezvous. `wait` also takes a bare
  `blocks` flag. These feed lost-wakeup and MHP pairing, matched **by identity**
  (never by predicate).
- **`on = …`** is the identity: `on = self.field`, `on = self.accessor()`, or an
  **argument** (e.g. a guard/lock passed in). This is what pairs a `wait` on one
  thread with a `wake` on another.

Consequence: a "relaxed publication across FFI" cannot be flagged from a foreign
contract alone — with no ordering there is no release/acquire edge to check. That
is parked for a future runtime/visible-body check, not over-promised here.

### 5a. Futex-backed mutex — from `tests/smoke.rs`

```rust
struct Futex;

impl Futex {
    #[foreign(wait, on = self.inner, blocks)]   // FUTEX_WAIT: parks on self.inner
    fn block(&self) {}

    #[foreign(wake, on = self.inner)]           // FUTEX_WAKE: wakes waiters on self.inner
    fn wake_many(&self) {}

    #[foreign(acquire, on = self.inner)]        // non-wait FFI mutual-exclusion effect
    fn ffi_lock(&self) {}
}
```

The shared `on = self.inner` identity is what lets a `block` on one thread pair
with a `wake_many` on another: a `block` that is program-order-after the only
reachable `wake_many` is a **lost wakeup**.

---

## 6. Tier I — Identity repair (severed alias chains only)

Use **only** where the alias chain is genuinely broken so the analyzer can't tell
two values are the same lock: `transmute`, `into_raw`/`from_raw` round-trips, an
FFI-returned handle, or a HW/HV register with no `new` site.

> An `UnsafeCell` or `unsafe impl Sync` (like `SpinLock` above) is **Tier R, not
> Tier I** — the field is still a real place, so identity is not lost.

The name is a **cross-scope join key, always place-anchored.** The two attributes
are the two halves of one join:

| | `#[define_lock(NAME = PLACE)]` | `#[lock_identity(NAME)]` |
|---|---|---|
| **Role** | *declaration* — mint the anchor `NAME`, bind it to a canonical place | *reference* — re-attach another in-scope value to an already-declared `NAME` |
| **`= PLACE`?** | yes — the canonical root (a `static`, field, or raw expr) | no — the place **is** the decorated expression |
| **How many** | one per name (the root) | zero or more (each severed re-entry point) |
| **When** | there is **no MIR birth site** to anchor to | the birth site exists elsewhere, but the chain to *this* value is broken |

`#[define_lock]` is *not* redundant with Tier R's `#[lock_new]`: `lock_new` marks
a constructor that **has** a real MIR birth call; `define_lock` mints an anchor
when **no such call exists**.

### 6a. Register with no birth site

```rust
#[define_lock(GICR_LOCK = GICR_BASE)]     // mint the anchor; bind to the static that addresses it
static GICR_BASE: HwReg = unsafe { HwReg::at(0x0800_0000) };
```

### 6b. Re-attach across a severed chain (`into_raw` → `from_raw`)

```rust
// thread A: let raw = Box::into_raw(lock);   // (GICR_LOCK defined at its root elsewhere)

// thread B:
fn worker(raw: *mut Raw) {
    #[lock_identity(GICR_LOCK)]           // "(*raw) is that same lock" — reconnects to thread A
    unsafe { (*raw).lock() }
}
```

Without the marker the two `*mut Raw` values are distinct anchors and the
cross-thread contention is invisible.

### 6c. `#[same_lock(a, b)]` — both places visible together

When two aliases the chain missed are in scope at once, skip the name and assert
equality directly:

```rust
fn reattach(handle: Arc<Mutex<()>>, raw: *mut Mutex<()>) {
    #[same_lock(handle, raw)]             // raw was transmuted from handle; unify them
    unsafe { /* … */ }
}
```

(6b and 6c are expression-position attributes — nightly `proc_macro_hygiene`.)

---

## 7. Tier M — MHP groups (may-happen-in-parallel)

Declare that a set of entry points may run **in parallel** on distinct threads —
parallelism that originates *outside* the analyzed code (external dispatch,
concurrent syscalls, handler registries) and so can only enter via a spec.

Formally an MHP group is a **reflexive clique in the may-happen-in-parallel
relation**: every pair of members `(hᵢ, hⱼ)` runs in parallel — **including
`i == j`** (two threads in the *same* handler) — with **no join ordering**.

| Attribute | Put it on | Meaning |
|---|---|---|
| `#[mhp_group("name")]` | the **dispatch fn** | every concrete handler it fans out to is a member of group `"name"` |
| `#[mhp("name")]` | each **member** fn/method | explicit membership in group `"name"` |

**Members share one keyed object.** The whole group is analyzed as running on a
**single shared receiver instance** — by default one common `self`. This is the
crux that makes an MHP group find anything: two members both touch `self.field`,
but those are *distinct* anchors (roughly `Formal{owner: sys_close, "self.field"}`
vs `Formal{owner: sys_dup, "self.field"}`) until the analysis **rebases every
member's shared-state root onto one synthetic per-`(group, key)` anchor**. After
that rebinding both accesses unify, so a `read`/`write` in one member can race a
`write` in another (exactly how `sys_close ∥ sys_dup` on one shim is caught).
Program `static`s are already global and unify for free; it is the keyed state
(`self` by default) that this binding pairs. The default key `self` bakes in a
**singleton-receiver assumption** — one instance per group; **keying** (below)
makes the sharing precise and explicit rather than assumed.

**Keying the group.** Declare a `key` to partition the group: two members
may-conflict only when their keys name the same object. The key is a place —
`self` (the default), a parameter, a field, or a global.

```rust
#[mhp("fdops", key = fd)]  fn sys_read (&self, fd: Fd, /* … */) { /* … */ }
#[mhp("fdops", key = fd)]  fn sys_write(&self, fd: Fd, /* … */) { /* … */ }
```

Here the pair race only on the *same* `fd`: the analysis roots both members' state
at a synthetic per-`(group, key)` anchor, so `desc[fd].offset` unifies precisely
instead of collapsing all of `self`. The anchor is minted from the **names** (the
group string + the key expression), not from pointer provenance — the same
mechanism that lets the default `self` binding unify handlers with no real
aliasing.

**Cross-crate keying.** Because the group name and the key are *global join keys*
minted by the whole-program pass — not crate-local provenance — a group can span
crates exactly as a `static` does. Membership unions the `#[mhp]` markers from
every loaded summary; the per-`(group, key)` anchor is derived from the names, so
two members in different crates pair when they share that key. Two requirements:
(1) the analyzer sees whole-program summaries (cross-crate summaries are
available), and (2) the group name and key are spelled **consistently** across
crates — a shared `const` is the safe way, since a mismatch silently drops the
pairing (a miss). For robust cross-crate identity, prefer a key that is itself
def-path-addressable (a shared `static` registry, or a receiver type + accessor);
a key that is a raw argument value is sound but over-approximate (it assumes the
values *may* be equal, and cannot prove them disjoint).

Benign cases self-suppress: if the fix wraps the shared state in a `Mutex`, both
members carry it in their guard sets, the sets intersect, and no race is reported;
the unlocked version races. That buggy/fixed differential comes for free.

### 7a. Concurrent syscall handlers — from `tests/smoke.rs`

```rust
#[mhp_group("syscalls")]                  // the dispatcher: its arms are the group
fn dispatch() {}

struct Shim;
impl Shim {
    #[mhp("syscalls")]                    // two userland threads may run these concurrently…
    fn sys_close(&self) {}

    #[mhp("syscalls")]                    // …on the same shim receiver, in any pairing
    fn sys_dup(&self) {}
}
```

This is what surfaces "close + insert on the FD table must be atomic" and
"read/write race the shared file offset": the members share one receiver, and the
all-pairs (incl. self) parallelism is exactly the missing fact.

---

## 8. How it works (mechanism)

Each attribute expands to a marker function named `__la_<hex>_<n>`, where `<hex>`
encodes the payload `"<item-name>|<kind>|<args>"` and `<n>` disambiguates repeats.
The marker is **uncalled** (zero runtime cost, no call edges) and is placed so the
analyzer can associate it:

- **nested** as the first item of the body — for `fn` / method / inherent `impl` /
  `trait` / `mod` — so its def-path parent *is* the annotated item;
- **sibling** immediately after the item — for a `struct` / `enum` / `union` field
  list, a trait-`impl` body, or a body-less declaration — with the item name
  carried in the payload.

You never write or read these markers directly; the analyzer decodes them.

---

## 9. Guidance for humans and agents

Decision checklist before adding any annotation:

1. **Can the analyzer already see it?** Is this ordinary Rust with available MIR
   (your crate, a workspace member, a dependency with summaries)? → **Do not
   annotate.** Fix the contract on the real function instead if needed.
2. **Is the body genuinely non-Rust** (syscall / asm / FFI / HV-call)? →
   **Tier F**, on the leaf (or, pragmatically, the nearest Rust wrapper).
3. **Is a type/method a lock op that just isn't `std::sync`?** → **Tier R**.
4. **Is the alias chain actually severed** (`transmute` / `from_raw` / FFI handle /
   register with no `new`)? → **Tier I**. An `UnsafeCell` alone is **not** severed.
5. **Does parallelism come from outside the code** (external/concurrent dispatch)?
   → **Tier M**.

Anti-patterns:

- Annotating a plain `std::sync::Mutex` or its guard drop — redundant, and adds
  trusted surface for no gain.
- Using **Tier F** on a Rust body you *could* analyze — that throws away the real
  ordering the analyzer would otherwise recover.
- Using **Tier I** for an `UnsafeCell`/`unsafe impl Sync` — the field is a real
  place; use **Tier R**.
- Inventing a `foreign` mode: only `acquire`, `release`, `access`, `wait`, `wake`
  are meaningful (the analyzer's decode layer recognizes exactly these).

Soundness boundary: **Tier I needs an in-scope Rust place to anchor to.** A
foreign lock with no Rust value anywhere is out of analysis scope *soundly* —
there is nothing for two threads to share as "the same," hence no wait-for edge to
recover. That is the natural boundary, not a gap.

---

## 10. Status & limitations

- **Trusted, not verified.** All four tiers are trusted inputs today; a
  cross-check of an annotated bag against reality (runtime / `loom` / visible
  body) is future work.
- **Decode layer.** The analyzer's pass that reads `__la_*` markers back into its
  model is under construction; the crate emits the markers and validates
  placement, and the decoder must mirror the `foreign` mode spellings (a
  `proc-macro` crate cannot export types for it to import).
- **No trace events / no contracts.** Positional in-body events and function-level
  contracts (footprints, pre/post-conditions) are intentionally absent — inference
  recovers both for any body it can see. This crate is *only* the FIRM facts.

See [`tests/smoke.rs`](tests/smoke.rs) for a compilable example of every
item-position annotation.
