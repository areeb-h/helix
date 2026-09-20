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
//! [`ColData`]: super::ColData

use std::collections::HashMap;
use std::rc::Rc;

/// A dictionary key that hashes and compares as its text, so the builder can
/// probe with a bare `&str` (std has no `Borrow<str>` for `Rc<String>`).
#[derive(PartialEq, Eq, Hash)]
struct DictKey(Rc<String>);

impl std::borrow::Borrow<str> for DictKey {
    fn borrow(&self) -> &str {
        self.0.as_str()
    }
}

/// Hash-consing builder for a dictionary-encoded string column.
pub struct StrBuilder {
    dict: Vec<Rc<String>>,
    index: HashMap<DictKey, u32>,
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
            index: HashMap::new(),
            codes: Vec::with_capacity(rows),
            valid: Vec::with_capacity(rows),
        }
    }

    pub fn push_missing(&mut self) {
        self.codes.push(0);
        self.valid.push(false);
    }

    pub fn push_str(&mut self, s: &str) {
        if let Some(&c) = self.index.get(s) {
            self.codes.push(c);
            self.valid.push(true);
            return;
        }
        let rc = Rc::new(s.to_string());
        let c = self.dict.len() as u32;
        self.dict.push(rc.clone());
        self.index.insert(DictKey(rc), c);
        self.codes.push(c);
        self.valid.push(true);
    }

    pub fn push_rc(&mut self, s: &Rc<String>) {
        if let Some(&c) = self.index.get(s.as_str()) {
            self.codes.push(c);
            self.valid.push(true);
            return;
        }
        let c = self.dict.len() as u32;
        self.dict.push(s.clone());
        self.index.insert(DictKey(s.clone()), c);
        self.codes.push(c);
        self.valid.push(true);
    }

    /// The code for `s`, interning it if new — the remap half of a
    /// chunk-dictionary splice (per DISTINCT value, not per cell).
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&c) = self.index.get(s) {
            return c;
        }
        let rc = Rc::new(s.to_string());
        let c = self.dict.len() as u32;
        self.dict.push(rc.clone());
        self.index.insert(DictKey(rc), c);
        c
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
