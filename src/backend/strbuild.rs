//! A dictionary-encoded text column under construction — the ONE hash-consing builder, used by
//! the native engine (which stores text this way: ADR 0033 Stage 3) and by any reader that meets
//! its cells one at a time.
//!
//! WHY IT LIVES AT THE SEAM. A reader may not name an engine type — [`ColData`] is the
//! construction contract — so a reader used to collect `Vec<Option<String>>` and let the engine
//! intern it afterwards. That allocates a `String` per CELL only to hash it and throw it away
//! whenever the text was already in the dictionary, which in real data it nearly always is: a
//! 1 000-row result with a 40-value `city` column was 1 000 allocations for 40 strings. A reader
//! that pushes `&str` here allocates per DISTINCT value, and hands the engine the column it
//! would have built (`ColData::StrBuilt`). Nothing in this file names an engine, so it compiles
//! in every build, the oracle-only one included.
//!
//! A CELL IS HASHED ONCE. The index used to be a `HashMap<DictKey, u32>`, and std's map cannot
//! look a key up by `&str` and insert an owned one on a miss without hashing twice — `get`,
//! then `insert` — and a third time whenever the map grew, because it keeps 7 bits of a hash
//! and recomputes the rest. For a column of distinct values (names, ids, free text) that was
//! most of the work: 76 ns a cell. The index is now a `hashbrown::HashTable` of CODES: the
//! hash is computed once and handed to it, equality is a look at the dictionary, and each
//! entry's hash is kept beside it so growth re-reads no string. 52 ns a distinct cell at
//! 1 000 values, 49 where it was 94 at 100 000; a repeated value costs what it did (one hash,
//! one probe). THE HASH IS STILL SipHash under a per-builder random key — the cells are data
//! from files, sockets and strangers, and a faster unkeyed hash is a way to be sent a table
//! that takes quadratic time to read. (`foldhash` measured 3 ns better and says of itself that
//! it does not resist an attacker who can observe timings; not worth it.) `hashbrown` is no new
//! crate: it is what `std::collections::HashMap` is made of, and already in every build.
//!
//! [`ColData`]: super::ColData

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::rc::Rc;

use hashbrown::hash_table::{Entry, HashTable};

/// Hash-consing builder for a dictionary-encoded string column.
pub struct StrBuilder {
    dict: Vec<Rc<String>>,
    /// `hashes[c]` is the hash of `dict[c]`: what the table asks for when it grows.
    hashes: Vec<u64>,
    /// The codes, findable by their text's hash.
    index: HashTable<u32>,
    state: RandomState,
    codes: Vec<u32>,
    valid: Vec<bool>,
}

// The native engine is this builder's first user; a build without it (the oracle alone) uses
// only what a reader needs.
#[cfg_attr(not(feature = "native-df"), allow(dead_code))]
impl StrBuilder {
    pub fn with_capacity(rows: usize) -> StrBuilder {
        StrBuilder {
            dict: Vec::new(),
            hashes: Vec::new(),
            index: HashTable::new(),
            state: RandomState::new(),
            codes: Vec::with_capacity(rows),
            valid: Vec::with_capacity(rows),
        }
    }

    /// The code for `s`, interning it if new: one hash, one probe, and on a miss whatever
    /// `owned` costs — a copy of the text, or a bump of a count the caller already holds.
    fn code_of(&mut self, s: &str, owned: impl FnOnce() -> Rc<String>) -> u32 {
        let hash = self.state.hash_one(s);
        let (dict, hashes) = (&mut self.dict, &mut self.hashes);
        let found = self.index.entry(
            hash,
            |&c| dict.get(c as usize).is_some_and(|d| d.as_str() == s),
            |&c| hashes.get(c as usize).copied().unwrap_or(0),
        );
        match found {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(v) => {
                let c = dict.len() as u32;
                dict.push(owned());
                hashes.push(hash);
                v.insert(c);
                c
            }
        }
    }

    pub fn push_missing(&mut self) {
        self.codes.push(0);
        self.valid.push(false);
    }

    pub fn push_str(&mut self, s: &str) {
        let c = self.code_of(s, || Rc::new(s.to_string()));
        self.codes.push(c);
        self.valid.push(true);
    }

    pub fn push_rc(&mut self, s: &Rc<String>) {
        let c = self.code_of(s.as_str(), || s.clone());
        self.codes.push(c);
        self.valid.push(true);
    }

    /// The code for `s`, interning it if new — the remap half of a
    /// chunk-dictionary splice (per DISTINCT value, not per cell).
    pub fn intern(&mut self, s: &str) -> u32 {
        self.code_of(s, || Rc::new(s.to_string()))
    }

    /// Append a cell by an already-interned code.
    pub fn push_code(&mut self, code: u32) {
        self.codes.push(code);
        self.valid.push(true);
    }

    /// Adopt pre-built codes/validity wholesale (a worker thread's segment
    /// whose dictionary was interned in the same order — `intern` on a fresh
    /// builder assigns 0,1,2,… exactly like the worker did).
    pub fn set_codes(&mut self, codes: Vec<u32>, valid: Vec<bool>) {
        self.codes = codes;
        self.valid = valid;
    }

    /// The dictionary in first-seen order, a code per cell, and which cells hold a value —
    /// what an engine builds its own column from.
    pub fn into_parts(self) -> (Vec<Rc<String>>, Vec<u32>, Vec<bool>) {
        (self.dict, self.codes, self.valid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract every user relies on: codes are handed out in first-seen order, a repeated
    /// text gets the code it got, and a missing cell is an invalid slot — through every way in.
    #[test]
    fn a_text_is_interned_once_and_codes_follow_first_sight() {
        let mut b = StrBuilder::with_capacity(0);
        let shared = Rc::new("beta".to_string());
        b.push_str("alpha");
        b.push_rc(&shared);
        b.push_missing();
        b.push_str("alpha");
        b.push_str("");
        b.push_rc(&Rc::new("alpha".to_string()));
        assert_eq!(b.intern("beta"), 1);
        assert_eq!(b.intern("gamma"), 3);
        b.push_code(3);
        let (dict, codes, valid) = b.into_parts();
        let texts: Vec<&str> = dict.iter().map(|s| s.as_str()).collect();
        assert_eq!(texts, ["alpha", "beta", "", "gamma"]);
        assert_eq!(codes, [0, 1, 0, 0, 2, 0, 3]);
        assert_eq!(valid, [true, true, false, true, true, true, true]);
        // `push_rc` on a miss shares the caller's string rather than copying it.
        assert!(Rc::ptr_eq(&dict[1], &shared));
    }

    /// Growth re-reads no string and loses no entry: far past every resize of the table, each
    /// text still finds the code it was given.
    #[test]
    fn the_index_survives_growth() {
        let mut b = StrBuilder::with_capacity(0);
        let n = 50_000u32;
        for i in 0..n {
            b.push_str(&format!("value {i}"));
        }
        for i in (0..n).step_by(97) {
            assert_eq!(b.intern(&format!("value {i}")), i);
        }
        let (dict, codes, _) = b.into_parts();
        assert_eq!(dict.len(), n as usize);
        assert!(codes.iter().enumerate().all(|(i, c)| *c as usize == i));
    }
}
