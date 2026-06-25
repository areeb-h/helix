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

use std::sync::{LazyLock, RwLock};

use rustc_hash::FxHashMap;

/// An interned name. Two `Symbol`s are equal iff they were interned from equal
/// text, so equality and hashing are a single `u32` op. Recover the text with
/// [`Symbol::as_str`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Symbol(pub u32);

struct Interner {
    /// Text -> symbol, for deduplication. Keys are the leaked `&'static str`s,
    /// so the map borrows nothing it doesn't own forever.
    map: FxHashMap<&'static str, Symbol>,
    /// Symbol index -> text. `names[s.0]` is the string `s` was interned from.
    names: Vec<&'static str>,
}

static INTERNER: LazyLock<RwLock<Interner>> =
    LazyLock::new(|| RwLock::new(Interner { map: FxHashMap::default(), names: Vec::new() }));

impl Symbol {
    /// Intern `text`, returning its `Symbol` — the same one for equal text. Cold
    /// path: call it at parse/compile time or when a dynamic key first appears,
    /// never inside a hot loop.
    pub fn intern(text: &str) -> Symbol {
        // Fast path: already interned (a read lock, no allocation).
        if let Some(&sym) = INTERNER.read().unwrap().map.get(text) {
            return sym;
        }
        // Slow path: insert under the write lock, re-checking in case another
        // thread interned it between the two locks.
        let mut g = INTERNER.write().unwrap();
        if let Some(&sym) = g.map.get(text) {
            return sym;
        }
        // Leak the text so its lifetime is the process — a `Symbol` then never
        // dangles and `as_str` needs no guard. Names are bounded by program size.
        let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
        let sym = Symbol(g.names.len() as u32);
        g.names.push(leaked);
        g.map.insert(leaked, sym);
        sym
    }

    /// The `Symbol` for `text` **only if it has already been interned** — never
    /// inserts. Used for `r["key"]` lookups so a missing/typo key (which yields
    /// `missing`) doesn't pollute the interner with junk in a loop.
    pub fn lookup(text: &str) -> Option<Symbol> {
        INTERNER.read().unwrap().map.get(text).copied()
    }

    /// The text this symbol was interned from. Cold path (display, errors, JSON).
    pub fn as_str(self) -> &'static str {
        INTERNER.read().unwrap().names[self.0 as usize]
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
