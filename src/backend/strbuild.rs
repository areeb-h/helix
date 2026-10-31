//! A dictionary-encoded text column under construction — the ONE hash-consing builder, used by
//! the native engine (which stores text this way: ADR 0033 Stage 3), by any reader that meets
//! its cells one at a time, and — its interning half — by the readers' worker threads.
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
//! one probe). THE HASH IS STILL SipHash under a random key — the cells are data from files,
//! sockets and strangers, and a faster unkeyed hash is a way to be sent a table that takes
//! quadratic time to read. (`foldhash` measured 3 ns better and says of itself that it does not
//! resist an attacker who can observe timings; not worth it.) `hashbrown` is no new crate: it
//! is what `std::collections::HashMap` is made of, and already in every build.
//!
//! ONCE ON A WORKER THREAD TOO, AND NOT AGAIN AT THE SPLICE. The parallel readers intern on
//! worker threads, where an `Rc` may not go, so each kept an interner of its own: a
//! `HashMap<String, u32>` beside a `Vec<String>`. That was every distinct text stored TWICE,
//! hashed by `get`, by `insert` and by every growth — and then a further time, and copied a
//! third, when its chunk was spliced into the column. [`Interner`] is the index alone, generic
//! over how a text is HELD ([`Held`]): `Rc<String>` in the column, plain `String` in a chunk.
//! A read's chunks are made under ONE key (`with_key`), so they all hash alike: the first to
//! reach the splice ([`StrBuilder::append`]) becomes the column's dictionary, table and all,
//! and each later one is looked up by the hashes its worker computed, a text that is new
//! MOVED into its `Rc`. One hash and one copy of a text, from the file to the frame: 200 000
//! rows of four distinct text columns read in 50 ms where they took 90, a million rows of two
//! in 163 where they took 270 (283 for 615 on one thread), and a parquet column of distinct
//! names in 11 where it took 27 — its worker interns the WHOLE column, so its dictionary is
//! wrapped where it lies and interned by nobody twice.
//!
//! [`ColData`]: super::ColData

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::rc::Rc;

use hashbrown::hash_table::{Entry, HashTable};

/// How a dictionary holds a text. The engine's column holds `Rc<String>`, so that reading a
/// cell is a count bumped; a worker thread's chunk holds plain `String`, because an `Rc` may not
/// cross a thread and a chunk has to.
pub trait Held: From<String> {
    fn text(&self) -> &str;
}

impl Held for String {
    fn text(&self) -> &str {
        self
    }
}

impl Held for Rc<String> {
    fn text(&self) -> &str {
        self
    }
}

/// Texts held once each, numbered in first-seen order and findable by hash — the half of a
/// builder that is the same on every thread.
pub struct Interner<S> {
    dict: Vec<S>,
    /// `hashes[c]` is the hash of `dict[c]`: what the table asks for when it grows, and what a
    /// splice under the same key finds the text by instead of hashing it again.
    hashes: Vec<u64>,
    /// The codes, findable by their text's hash.
    index: HashTable<u32>,
    state: RandomState,
}

impl<S: Held> Default for Interner<S> {
    fn default() -> Interner<S> {
        Interner::new()
    }
}

// The native engine is the first user of most of this; a build without it (the oracle alone)
// uses only what a reader needs.
#[cfg_attr(not(feature = "native-df"), allow(dead_code))]
impl<S: Held> Interner<S> {
    /// An empty interner under a key of its own.
    pub fn new() -> Interner<S> {
        Interner::with_key(&RandomState::new())
    }

    /// An empty interner under `key`. Interners under one key agree on every text's hash, which
    /// is what lets one take in another's texts without hashing them; a key shared by the
    /// tables of one read is as secret as one table's was.
    pub fn with_key(key: &RandomState) -> Interner<S> {
        Interner {
            dict: Vec::new(),
            hashes: Vec::new(),
            index: HashTable::new(),
            state: key.clone(),
        }
    }

    /// The code for `key`'s text under a hash the caller has, interning it if new: one probe,
    /// and on a miss whatever `own` costs — a copy of the text, a bump of a count, or nothing
    /// at all when the caller holds a `String` it is done with.
    fn find_or_insert<K>(&mut self, hash: u64, key: K, own: impl FnOnce(K) -> S) -> u32
    where
        K: AsRef<str>,
    {
        let (dict, hashes) = (&mut self.dict, &mut self.hashes);
        let found = self.index.entry(
            hash,
            |&c| dict.get(c as usize).is_some_and(|d| d.text() == key.as_ref()),
            |&c| hashes.get(c as usize).copied().unwrap_or(0),
        );
        match found {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(v) => {
                let c = dict.len() as u32;
                dict.push(own(key));
                hashes.push(hash);
                v.insert(c);
                c
            }
        }
    }

    /// The code for `s`, interning it if new: one hash, one probe, and a copy on a miss.
    pub fn intern(&mut self, s: &str) -> u32 {
        let hash = self.state.hash_one(s);
        self.find_or_insert(hash, s, |s| S::from(s.to_string()))
    }

    /// The same for a text the caller built for this cell and is done with (a timestamp or a
    /// decimal, rendered): a miss keeps the `String` it was given, a hit drops it.
    pub fn intern_owned(&mut self, s: String) -> u32 {
        let hash = self.state.hash_one(s.as_str());
        self.find_or_insert(hash, s, S::from)
    }

    /// The texts in first-seen order: a code is an index into them.
    pub fn into_texts(self) -> Vec<S> {
        self.dict
    }
}

#[cfg_attr(not(feature = "native-df"), allow(dead_code))]
impl Interner<String> {
    /// These texts held the engine's way: each `String` wrapped where it lies. The table, the
    /// hashes and the key carry over — a code means what it meant.
    pub fn into_shared(self) -> Interner<Rc<String>> {
        Interner {
            dict: self.dict.into_iter().map(Rc::new).collect(),
            hashes: self.hashes,
            index: self.index,
            state: self.state,
        }
    }
}

#[cfg_attr(not(feature = "native-df"), allow(dead_code))]
impl Interner<Rc<String>> {
    /// The code for `s`, which on a miss is SHARED with the caller rather than copied.
    pub fn intern_rc(&mut self, s: &Rc<String>) -> u32 {
        let hash = self.state.hash_one(s.as_str());
        self.find_or_insert(hash, s.as_str(), |_| s.clone())
    }

    /// Take in what a worker thread interned, answering for each of ITS codes the code that
    /// text has here. Per distinct text of the chunk: a probe under the hash the worker
    /// computed, and for a text this dictionary had not met, the `String` itself moved into its
    /// `Rc` — not hashed again, not copied again.
    pub fn absorb(&mut self, chunk: Interner<String>) -> Vec<u32> {
        let Interner { dict, hashes, .. } = chunk;
        // A HASH IS ONLY AS GOOD AS ITS KEY. Looking a text up under a hash made with another
        // key would not fail — it would MISS, and intern a second copy of a text already here:
        // two codes for one value, and a group-by that splits a group in two. So one text is
        // hashed again to see that the chunk was made under this key, and a chunk that was not
        // is merely slower: every text of it is hashed here, as they all used to be.
        let same_key = dict
            .first()
            .zip(hashes.first())
            .is_none_or(|(s, h)| self.state.hash_one(s.as_str()) == *h);
        dict.into_iter()
            .zip(hashes)
            .map(|(s, h)| {
                let h = if same_key { h } else { self.state.hash_one(s.as_str()) };
                self.find_or_insert(h, s, Rc::new)
            })
            .collect()
    }
}

/// Hash-consing builder for a dictionary-encoded string column.
pub struct StrBuilder {
    texts: Interner<Rc<String>>,
    codes: Vec<u32>,
    valid: Vec<bool>,
}

// As above: what only the native engine's readers call is dead in a build without it.
#[cfg_attr(not(feature = "native-df"), allow(dead_code))]
impl StrBuilder {
    pub fn with_capacity(rows: usize) -> StrBuilder {
        StrBuilder {
            texts: Interner::new(),
            codes: Vec::with_capacity(rows),
            valid: Vec::with_capacity(rows),
        }
    }

    pub fn push_missing(&mut self) {
        self.codes.push(0);
        self.valid.push(false);
    }

    pub fn push_str(&mut self, s: &str) {
        let c = self.texts.intern(s);
        self.codes.push(c);
        self.valid.push(true);
    }

    pub fn push_rc(&mut self, s: &Rc<String>) {
        let c = self.texts.intern_rc(s);
        self.codes.push(c);
        self.valid.push(true);
    }

    /// Append the rows a worker thread built — the splice of a chunked read. The texts are
    /// taken in per DISTINCT value; the cells are integer rewrites, and a missing cell is code
    /// 0, as `push_missing` writes it (its code in the chunk is a placeholder, not a text of
    /// the chunk's).
    pub fn append(&mut self, texts: Interner<String>, codes: &[u32], valid: &[bool]) {
        let cells = codes.iter().zip(valid);
        if self.texts.dict.is_empty() {
            // THE FIRST TEXTS TO ARRIVE ARE THE DICTIONARY SO FAR. Nothing came before them, so
            // the chunk's codes are already the column's, and its table — hashes, key and all —
            // is the column's table: nothing is probed, and a read of ONE chunk (a CSV of 4 096
            // rows or fewer) never probes at all. The chunks that follow were made under the
            // same key, which is why `absorb` can take their hashes as they are.
            self.texts = texts.into_shared();
            self.codes.extend(cells.map(|(&c, &ok)| if ok { c } else { 0 }));
        } else {
            let trans = self.texts.absorb(texts);
            self.codes.extend(cells.map(|(&c, &ok)| {
                if ok { trans.get(c as usize).copied().unwrap_or(0) } else { 0 }
            }));
        }
        self.valid.extend_from_slice(valid);
    }

    /// The dictionary in first-seen order, a code per cell, and which cells hold a value —
    /// what an engine builds its own column from.
    pub fn into_parts(self) -> (Vec<Rc<String>>, Vec<u32>, Vec<bool>) {
        (self.texts.into_texts(), self.codes, self.valid)
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
        b.push_str("gamma");
        let (dict, codes, valid) = b.into_parts();
        let texts: Vec<&str> = dict.iter().map(|s| s.as_str()).collect();
        assert_eq!(texts, ["alpha", "beta", "", "gamma"]);
        assert_eq!(codes, [0, 1, 0, 0, 2, 0, 3]);
        assert_eq!(valid, [true, true, false, true, true, true, true]);
        // `push_rc` on a miss shares the caller's string rather than copying it.
        assert!(Rc::ptr_eq(&dict[1], &shared));
    }

    /// Growth re-reads no string and loses no entry: far past every resize of the table, each
    /// text still finds the code it was given — however the text is held.
    #[test]
    fn the_index_survives_growth() {
        let mut shared: Interner<Rc<String>> = Interner::new();
        let mut plain: Interner<String> = Interner::new();
        let n = 50_000u32;
        for i in 0..n {
            assert_eq!(shared.intern(&format!("value {i}")), i);
            assert_eq!(plain.intern_owned(format!("value {i}")), i);
        }
        for i in (0..n).step_by(97) {
            assert_eq!(shared.intern(&format!("value {i}")), i);
            assert_eq!(plain.intern(&format!("value {i}")), i);
        }
        assert_eq!(shared.into_texts().len(), n as usize);
        assert_eq!(plain.into_texts().len(), n as usize);
    }

    /// A chunk a worker built: `texts` interned under `key`, every cell present but the ones
    /// spelled `None`.
    fn chunk(key: &RandomState, cells: &[Option<&str>]) -> (Interner<String>, Vec<u32>, Vec<bool>) {
        let mut texts = Interner::with_key(key);
        let codes = cells.iter().map(|c| c.map_or(0, |s| texts.intern(s))).collect();
        (texts, codes, cells.iter().map(|c| c.is_some()).collect())
    }

    /// The splice: chunks appended in order read as if every cell had been pushed in order —
    /// one code per text across chunks, first sight deciding it, a missing cell code 0 — and a
    /// chunk with no text at all (every cell missing) is only its rows.
    #[test]
    fn chunks_append_as_if_pushed_cell_by_cell() {
        let key = RandomState::new();
        let rows: [&[Option<&str>]; 4] = [
            &[Some("b"), None, Some("a"), Some("b")],
            &[None, None],
            &[Some("c"), Some("a"), None, Some("")],
            &[Some("b"), Some("d")],
        ];
        let mut spliced = StrBuilder::with_capacity(0);
        let mut pushed = StrBuilder::with_capacity(0);
        for cells in rows {
            let (texts, codes, valid) = chunk(&key, cells);
            spliced.append(texts, &codes, &valid);
            for cell in cells {
                match cell {
                    Some(s) => pushed.push_str(s),
                    None => pushed.push_missing(),
                }
            }
        }
        let (dict, codes, valid) = spliced.into_parts();
        let (want_dict, want_codes, want_valid) = pushed.into_parts();
        assert_eq!(dict, want_dict);
        assert_eq!(dict.iter().map(|s| s.as_str()).collect::<Vec<_>>(), ["b", "a", "c", "", "d"]);
        assert_eq!(codes, want_codes);
        assert_eq!(valid, want_valid);
    }

    /// The splice grows the table too, under hashes it did not compute: chunks of mostly NEW
    /// text, far past every resize, still read as if pushed cell by cell.
    #[test]
    fn a_splice_of_distinct_text_survives_growth() {
        let key = RandomState::new();
        let mut spliced = StrBuilder::with_capacity(0);
        let mut pushed = StrBuilder::with_capacity(0);
        for k in 0..4 {
            // 10 000 cells a chunk, the first 2 000 of them met in the chunk before.
            let owned: Vec<String> = (0..10_000).map(|i| format!("text {}", k * 8_000 + i)).collect();
            let cells: Vec<Option<&str>> = owned.iter().map(|s| Some(s.as_str())).collect();
            let (texts, codes, valid) = chunk(&key, &cells);
            spliced.append(texts, &codes, &valid);
            for s in &owned {
                pushed.push_str(s);
            }
        }
        let (dict, codes, valid) = spliced.into_parts();
        assert_eq!(dict.len(), 34_000);
        assert!((dict, codes, valid) == pushed.into_parts());
    }

    /// A chunk's text reaches the column by being MOVED — the bytes the worker copied out of
    /// the file are the bytes the frame keeps — whether its chunk was the first (taken whole)
    /// or a later one (looked up, and new).
    #[test]
    fn a_chunks_text_is_moved_into_the_column_not_copied() {
        let key = RandomState::new();
        let (first, c1, v1) = chunk(&key, &[Some("the first chunk's own text"), Some("both")]);
        let (later, c2, v2) = chunk(&key, &[Some("both"), None, Some("a text new to the column")]);
        let (p0, p2) = (first.dict[0].as_ptr(), later.dict[1].as_ptr());
        let mut b = StrBuilder::with_capacity(0);
        b.append(first, &c1, &v1);
        b.append(later, &c2, &v2);
        let (dict, codes, valid) = b.into_parts();
        assert_eq!(codes, [0, 1, 1, 0, 2]);
        assert_eq!(valid, [true, true, true, false, true]);
        assert_eq!((dict[0].as_ptr(), dict[2].as_ptr()), (p0, p2));
    }

    /// A chunk made under ANOTHER key carries hashes that mean nothing here. It must still
    /// find the texts already interned — one code per text — rather than miss them all.
    #[test]
    fn a_chunk_under_another_key_is_hashed_again_not_trusted() {
        let mut b = StrBuilder::with_capacity(0);
        let n = 2_000;
        let cells: Vec<String> = (0..n).map(|i| format!("text {}", i % 500)).collect();
        for s in &cells {
            b.push_str(s);
        }
        let cells: Vec<Option<&str>> = cells.iter().map(|s| Some(s.as_str())).collect();
        let (texts, codes, valid) = chunk(&RandomState::new(), &cells);
        b.append(texts, &codes, &valid);
        let (dict, codes, _) = b.into_parts();
        assert_eq!(dict.len(), 500);
        assert_eq!(codes[..n], codes[n..]);
    }
}
