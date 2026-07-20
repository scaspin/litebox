//! Lock / synchronization annotations (distilled surface).
//!
//! Every macro here is a **no-op for normal compilation**: each only injects a
//! uniquely-named marker function whose name hex-encodes the annotation, so a
//! MIR analyzer can recover the annotations purely from def-paths — no need to
//! read constants or parse source. Every annotation is a **function-decorating
//! attribute** ([`inject`]) — written `#[attr(..)]` on a whole item
//! (fn / method / type / impl). It states a *contract*: a fact about the item as
//! a unit, carrying **no** control-flow position. The marker is emitted
//! *uncalled* — nested as the first item of the body when the item has one, or
//! as a sibling right after the item when it does not (a `struct` field list, an
//! `extern` fn declaration). Association is by the marker's def-path parent
//! (nested) or by the item name encoded in its payload (sibling).
//!
//! Positional, in-body "trace" events are deliberately **not** provided: every
//! real synchronization operation already appears in MIR at its true control-flow
//! position, so inference (Tracks A/B) plus Tier R recognition recover the ordered
//! trace directly. Nor are function-level *contracts* (footprints / pre- &
//! post-conditions) provided: inference already computes them for any body it can
//! see. Annotations are a last resort for the few facts a MIR analyzer
//! **structurally cannot** produce, in four tiers that spell **FIRM** — Foreign,
//! Identity, Recognition, MHP:
//!
//! | Tier | Kind | Members |
//! |---|---|---|
//! | R — recognition (`track_c_foreign_bespoke.md`) | item attrs | `lock` `lock_new` `lock_acquire` `lock_release` `lock_guard` |
//! | F — foreign summary | fn attr | `foreign` (mode `acquire` / `release` / `access` / `wait` / `wake`) |
//! | I — identity repair | attrs | `define_lock` `lock_identity` `same_lock` |
//! | M — may-happen-in-parallel groups (`handler_mhp_design.md`) | fn-level attrs | `mhp` `mhp_group` |

use proc_macro::{Delimiter, Group, TokenStream, TokenTree};
use std::sync::atomic::{AtomicU64, Ordering};

/// Disambiguates markers so repeated identical annotations on one item (e.g. two
/// `#[reads(m)]`) stay distinct.
static COUNTER: AtomicU64 = AtomicU64::new(0);

// ===========================================================================
// Item-decorating attributes (all four tiers)
// ===========================================================================

/// Generate all item-decorating attributes. Each forwards to [`inject`].
macro_rules! annotation_attrs {
    ($($name:ident),* $(,)?) => {
        $(
            #[proc_macro_attribute]
            pub fn $name(attr: TokenStream, item: TokenStream) -> TokenStream {
                inject(stringify!($name), attr, item)
            }
        )*
    };
}

annotation_attrs!(
    // ---- Tier R — non-std / bespoke lock recognition ----
    lock,         // `#[lock]` — the type is an anchor CLASS
    lock_new,     // constructor → Declare + fresh Alloc anchor
    lock_acquire, // `#[lock_acquire(read?)]` — method → Acquire (read ⇒ shared, else Write)
    lock_release, // method → Release{via: ExplicitCall}
    lock_guard,   // guard type's Drop → Release{via: GuardDrop}
    // ---- Tier F — opaque / FFI foreign summary (no ordering) ----
    // `#[foreign(MODE, on = …)]`, MODE ∈ acquire | release | access | wait | wake.
    // `wait` also takes `blocks`; wait/wake feed lost-wakeup + MHP pairing, the
    // other modes feed lock-ordering. Identity via `on = self.field` /
    // `self.accessor()` / an argument. No ordering, no predicate.
    foreign,
    // ---- Tier I — identity repair (severed alias chain only) ----
    define_lock, // `#[define_lock(NAME = PLACE)]` — mint an anchor with no MIR birth site
    lock_identity, // `#[lock_identity(NAME)]` — re-attach a value to a cross-scope NAME
    same_lock,   // `#[same_lock(a, b)]` — assert two in-scope places are one lock
    // ---- Tier M — may-happen-in-parallel (MHP) groups (handler_mhp_design §5) ----
    // `#[mhp_group("name")]` on a dispatch fn; `#[mhp("name")]` on each member.
    // Members of an MHP group may run in parallel — all pairs, incl. self-pairs
    // (two threads in the same handler) — with no join ordering. Formally the
    // members form a (reflexive) clique in the MHP relation.
    mhp,
    mhp_group,
);

/// Inject a marker encoding this attribute. The marker name hex-encodes
/// `"<item-name>|<kind>|<args>"`. It is emitted *uncalled*: nested as the first
/// item of the body (fn / method / impl / trait / mod) so its def-path parent is
/// the annotated item, or — when the item has only a field brace (`struct` /
/// `enum` / `union`) or no brace at all (a body-less `fn` declaration, a tuple
/// struct) — as a **sibling** immediately after it, where the item name in the
/// payload carries the association.
fn inject(kind: &str, attr: TokenStream, item: TokenStream) -> TokenStream {
    let name = item_name(&item).unwrap_or_else(|| "?".to_string());
    let args = attr.to_string();
    let payload = format!("{name}|{kind}|{args}");
    let hex = hex_encode(payload.as_bytes());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let marker_src = format!("#[allow(non_snake_case, dead_code)] fn __la_{hex}_{n}() {{}}");
    let marker: TokenStream = match marker_src.parse() {
        Ok(ts) => ts,
        Err(_) => return item,
    };
    // A `struct`/`enum`/`union` exposes only a field brace we must not nest an
    // item into; an `impl` (especially a trait impl) rejects non-member items in
    // its body; and a body-less declaration has no brace at all. In all of these
    // the marker rides as a sibling right after the item, associated by the item
    // name in its payload.
    let sibling = matches!(
        leading_item_kw(&item).as_deref(),
        Some("struct" | "enum" | "union" | "impl")
    ) || !has_top_level_brace(&item);
    if sibling {
        let mut out = item;
        out.extend(marker);
        out
    } else {
        prepend_into_body(item, marker)
    }
}

// ===========================================================================
// Shared helpers (token munging + marker name encoding)
// ===========================================================================

/// Item keywords whose following identifier is the item's name.
const NAME_KEYWORDS: &[&str] = &[
    "fn", "struct", "enum", "union", "trait", "type", "const", "static", "mod",
];

/// The item's declared name for the marker payload. For a keyword item it is
/// the identifier following one of [`NAME_KEYWORDS`]. For an `impl` block it is
/// the `Self` type: the identifier after a top-level `for` (a trait impl), else
/// the first type identifier after `impl`'s optional generic parameters. Returns
/// `None` when no name can be recovered.
fn item_name(item: &TokenStream) -> Option<String> {
    if matches!(leading_item_kw(item).as_deref(), Some("impl")) {
        return impl_self_name(item);
    }
    let mut awaiting = false;
    for tt in item.clone() {
        if let TokenTree::Ident(id) = tt {
            let s = id.to_string();
            if awaiting {
                return Some(s);
            }
            if NAME_KEYWORDS.contains(&s.as_str()) {
                awaiting = true;
            }
        }
    }
    None
}

/// The `Self` type name of an `impl` block. Reads the identifier after a
/// top-level `for` when present (`impl Trait for Type` → `Type`), otherwise the
/// first identifier after `impl`, skipping a leading generic block `<...>`
/// (`impl<T> Type<T>` → `Type`).
fn impl_self_name(item: &TokenStream) -> Option<String> {
    let toks: Vec<TokenTree> = item.clone().into_iter().collect();
    let impl_pos = toks
        .iter()
        .position(|tt| matches!(tt, TokenTree::Ident(id) if id.to_string() == "impl"))?;
    // Stop scanning at the impl body brace.
    let brace_pos = toks
        .iter()
        .position(|tt| matches!(tt, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace))
        .unwrap_or(toks.len());
    let region = &toks[impl_pos + 1..brace_pos];
    // Trait impl: take the first ident after a top-level `for`.
    if let Some(for_pos) = region
        .iter()
        .position(|tt| matches!(tt, TokenTree::Ident(id) if id.to_string() == "for"))
    {
        return region[for_pos + 1..].iter().find_map(ident_string);
    }
    // Inherent impl: skip a leading `<...>` generic block, then first ident.
    let mut depth: i32 = 0;
    for tt in region {
        if let TokenTree::Punct(p) = tt {
            match p.as_char() {
                '<' => depth += 1,
                '>' => depth -= 1,
                _ => {}
            }
            continue;
        }
        if depth == 0 {
            if let Some(s) = ident_string(tt) {
                return Some(s);
            }
        }
    }
    None
}

/// The identifier text of a token tree, if it is an identifier.
fn ident_string(tt: &TokenTree) -> Option<String> {
    match tt {
        TokenTree::Ident(id) => Some(id.to_string()),
        _ => None,
    }
}

/// The leading item keyword (`fn`, `struct`, `impl`, `trait`, …), scanning only
/// top-level identifiers so `pub` / `unsafe` / `async` / `extern` and any
/// attributes (whose contents live inside bracket groups) are skipped.
fn leading_item_kw(item: &TokenStream) -> Option<String> {
    const ITEM_KEYWORDS: &[&str] = &[
        "fn", "struct", "enum", "union", "trait", "type", "const", "static", "mod", "impl",
    ];
    for tt in item.clone() {
        if let TokenTree::Ident(id) = tt {
            let s = id.to_string();
            if ITEM_KEYWORDS.contains(&s.as_str()) {
                return Some(s);
            }
        }
    }
    None
}

/// Whether the item has any top-level brace group (a body we can nest into).
fn has_top_level_brace(item: &TokenStream) -> bool {
    item.clone()
        .into_iter()
        .any(|tt| matches!(tt, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace))
}

/// Insert `inject` tokens at the start of the item's body block (the last
/// top-level brace group). If there is no body, return the item unchanged.
fn prepend_into_body(item: TokenStream, inject: TokenStream) -> TokenStream {
    let trees: Vec<TokenTree> = item.into_iter().collect();
    let body_idx = trees
        .iter()
        .rposition(|tt| matches!(tt, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace));
    let Some(body_idx) = body_idx else {
        return trees.into_iter().collect();
    };
    let mut out: Vec<TokenTree> = Vec::with_capacity(trees.len());
    for (i, tt) in trees.into_iter().enumerate() {
        if i == body_idx {
            if let TokenTree::Group(g) = tt {
                let mut new_stream = TokenStream::new();
                new_stream.extend(inject.clone());
                new_stream.extend(g.stream());
                let mut new_group = Group::new(Delimiter::Brace, new_stream);
                new_group.set_span(g.span());
                out.push(TokenTree::Group(new_group));
                continue;
            }
        }
        out.push(tt);
    }
    out.into_iter().collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}
