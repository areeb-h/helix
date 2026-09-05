//! String interning: identifier, method, and record-field names collapse to a
//! `Symbol` (a `u32`) so that the hot paths — record-field lookup, method
//! dispatch, environment binding — compare and hash a single integer instead of
//! a heap string. The actual text is recovered only on the cold paths (display,
//! error messages, JSON serialization) via [`Symbol::as_str`].
//!
//! The interner is a **process-global, append-only** table. Append-only because
//! a name, once seen, is valid for the rest of the run; nothing is ever removed,
//! so a `Symbol` never dangles and resolving one needs no lifetime. Interned text
//! is leaked deliberately (its lifetime *is* the process), which lets
//! [`Symbol::as_str`] hand back a `&'static str` with no guard — ideal for
//! `Display`, which has nowhere to thread a borrow.
//!
//! Interning happens on cold paths only (parse/compile, plus runtime-dynamic keys
//! from JSON or `r["key"]`); the lock is therefore effectively uncontended —
//! Helix executes a program on a single thread (Polars' internal parallelism never
//! touches the interner). `Symbol` values are assigned in first-seen order, so they
//! are deterministic within a single program run; because every user-visible path
//! resolves back to text, the particular integers never leak into output.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{LazyLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use rustc_hash::FxHashMap;

/// Hard ceiling on the number of distinct interned names. The interner is
/// append-only and leaked, so without a cap a stream of distinct runtime-dynamic
/// keys (e.g. `parse_json` over a file with millions of unique object keys) would
/// grow process memory without bound. 50M is far above any real program — source
/// identifiers and method names number in the thousands — so the cap is invisible
/// in practice and only ever trips on adversarial input.
const MAX_SYMBOLS: usize = 50_000_000;

/// An interned name. Two `Symbol`s are equal iff they were interned from equal
/// text, so equality and hashing are a single `u32` op. Recover the text with
/// [`Symbol::as_str`]. The field is private so a `Symbol` can only come from the
/// interner — an out-of-range value can never be forged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(u32);

struct Interner {
    /// Text -> symbol, for deduplication. Keys are the leaked `&'static str`s,
    /// so the map borrows nothing it doesn't own forever.
    map: FxHashMap<&'static str, Symbol>,
    /// Symbol index -> text. `names[s.0]` is the string `s` was interned from.
    names: Vec<&'static str>,
}

static INTERNER: LazyLock<RwLock<Interner>> =
    LazyLock::new(|| RwLock::new(Interner { map: FxHashMap::default(), names: Vec::new() }));

/// Read the interner, recovering from a poisoned lock. The table is append-only —
/// a panic mid-insert can leave at most one half-written entry, never an invariant
/// a reader depends on — so recovering the guard is safe and avoids a poison
/// cascade taking down the whole process.
fn read_interner() -> RwLockReadGuard<'static, Interner> {
    INTERNER.read().unwrap_or_else(|e| e.into_inner())
}

fn write_interner() -> RwLockWriteGuard<'static, Interner> {
    INTERNER.write().unwrap_or_else(|e| e.into_inner())
}

impl Symbol {
    /// The symbol handed back when the interner is full ([`MAX_SYMBOLS`]). It has
    /// no backing text, so [`Symbol::as_str`] resolves it to `"<overflow>"`.
    const OVERFLOW: Symbol = Symbol(u32::MAX);

    /// Intern `text`, returning its `Symbol` — the same one for equal text. Cold
    /// path: call it at parse/compile time or when a dynamic key first appears,
    /// never inside a hot loop. Past the [`MAX_SYMBOLS`] cap a *new* name collapses
    /// to [`Symbol::OVERFLOW`] rather than growing memory; callers that must reject
    /// over-cap input cleanly (e.g. `parse_json`) use [`Symbol::try_intern`].
    pub fn intern(text: &str) -> Symbol {
        Symbol::try_intern(text).unwrap_or(Symbol::OVERFLOW)
    }

    /// Like [`Symbol::intern`], but returns `None` instead of an overflow sentinel
    /// when the interner is at the [`MAX_SYMBOLS`] cap and `text` is new — letting
    /// a runtime-dynamic ingest path (JSON keys) surface a clean error.
    pub fn try_intern(text: &str) -> Option<Symbol> {
        // Fast path: already interned (a read lock, no allocation).
        if let Some(&sym) = read_interner().map.get(text) {
            return Some(sym);
        }
        // Slow path: insert under the write lock, re-checking in case another
        // thread interned it between the two locks.
        let mut g = write_interner();
        if let Some(&sym) = g.map.get(text) {
            return Some(sym);
        }
        if g.names.len() >= MAX_SYMBOLS {
            return None;
        }
        // Leak the text so its lifetime is the process — a `Symbol` then never
        // dangles and `as_str` needs no guard. Names are bounded by the cap.
        let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
        let sym = Symbol(g.names.len() as u32);
        g.names.push(leaked);
        g.map.insert(leaked, sym);
        Some(sym)
    }

    /// The `Symbol` for `text` **only if it has already been interned** — never
    /// inserts. Used for `r["key"]` lookups so a missing/typo key (which yields
    /// `missing`) doesn't pollute the interner with junk in a loop.
    pub fn lookup(text: &str) -> Option<Symbol> {
        read_interner().map.get(text).copied()
    }

    /// The text this symbol was interned from. Cold path (display, errors, JSON).
    /// A `Symbol` is always in range (the field is private and only the interner
    /// mints one), but resolve defensively — `get().unwrap_or(...)` — so a stray or
    /// overflow symbol yields a placeholder string instead of panicking *inside the
    /// read guard* and poisoning the lock.
    pub fn as_str(self) -> &'static str {
        if self == Symbol::OVERFLOW {
            return "<overflow>";
        }
        read_interner().names.get(self.0 as usize).copied().unwrap_or("<unknown-symbol>")
    }

    /// The text as a shared `Rc<String>` — the shape a `Value::Str` holds — from a per-thread
    /// cache filled on first use, so `r.keys()` / `r.items()` hand out a clone of ONE
    /// allocation per distinct name instead of taking the interner's read lock and allocating
    /// a fresh `String` and `Rc` per key per call (field build, 1.46.3: `keys()` + `items()`
    /// on a two-field record cost ~575 ns of a 3000 ns render). Thread-local because `Rc` is
    /// not `Sync`; Helix runs a program on one thread, and a worker thread simply fills its
    /// own cache. A map rather than a table indexed by id, so a program that interned millions
    /// of JSON keys pays for the names it actually converts, not for the highest id it holds.
    pub fn as_rc_string(self) -> Rc<String> {
        thread_local! {
            static RC_NAMES: RefCell<FxHashMap<u32, Rc<String>>> = RefCell::new(FxHashMap::default());
        }
        if self == Symbol::OVERFLOW {
            return Rc::new(self.as_str().to_string());
        }
        RC_NAMES.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(rc) = cache.get(&self.0) {
                return rc.clone();
            }
            let rc = Rc::new(self.as_str().to_string());
            cache.insert(self.0, rc.clone());
            rc
        })
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Symbol({}, {:?})", self.0, self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_text_interns_to_the_same_symbol() {
        let a = Symbol::intern("chromosome");
        let b = Symbol::intern("chromosome");
        assert_eq!(a, b);
        assert_eq!(a.0, b.0);
    }

    #[test]
    fn different_text_interns_distinctly() {
        let a = Symbol::intern("alpha");
        let b = Symbol::intern("beta");
        assert_ne!(a, b);
    }

    #[test]
    fn round_trips_through_text() {
        let s = Symbol::intern("BRCA1");
        assert_eq!(s.as_str(), "BRCA1");
        assert_eq!(s.to_string(), "BRCA1");
    }

    #[test]
    fn overflow_symbol_resolves_without_panicking() {
        // The overflow sentinel has no backing entry; `as_str` must still resolve
        // it (to a placeholder) rather than index out of range inside the guard.
        assert_eq!(Symbol::OVERFLOW.as_str(), "<overflow>");
        assert_eq!(Symbol::OVERFLOW.to_string(), "<overflow>");
        // `intern` is total — it never returns an unresolvable symbol.
        assert_eq!(Symbol::intern("anything").as_str(), "anything");
    }

    #[test]
    fn lookup_does_not_insert() {
        // A string never interned has no symbol...
        assert!(Symbol::lookup("never-seen-zzz").is_none());
        // ...and asking did not create one.
        assert!(Symbol::lookup("never-seen-zzz").is_none());
        // But an interned one is found without inserting again.
        let s = Symbol::intern("seen-yyy");
        assert_eq!(Symbol::lookup("seen-yyy"), Some(s));
    }
}
