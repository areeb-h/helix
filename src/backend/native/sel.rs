//! Row selections: the output of a filter or a row-gathering verb, held as
//! either a boolean mask or an explicit index list. A selection knows its
//! cardinality without materializing indices — so `where(...).count()` never
//! builds them — and the index form materializes lazily (memoized) the first
//! time a gather actually runs.

use std::cell::OnceCell;

enum SelKind {
    /// One bool per source row.
    Mask(Vec<bool>),
    /// Explicit row indices (possibly a permutation, possibly repeating).
    Idx(Vec<usize>),
}

pub(crate) struct RowSel {
    kind: SelKind,
    n: usize,
    idx: OnceCell<Vec<usize>>,
}

impl RowSel {
    /// A mask selection; `n_true` must equal the number of set bits.
    pub(crate) fn from_mask(mask: Vec<bool>, n_true: usize) -> RowSel {
        RowSel { kind: SelKind::Mask(mask), n: n_true, idx: OnceCell::new() }
    }

    pub(crate) fn from_idx(idx: Vec<usize>) -> RowSel {
        RowSel { n: idx.len(), kind: SelKind::Idx(idx), idx: OnceCell::new() }
    }

    /// How many rows the selection keeps.
    pub(crate) fn len(&self) -> usize {
        self.n
    }

    /// The index form, computed once from a mask on first need.
    pub(crate) fn indices(&self) -> &[usize] {
        match &self.kind {
            SelKind::Idx(v) => v,
            SelKind::Mask(mask) => self.idx.get_or_init(|| {
                let mut out = Vec::with_capacity(self.n);
                for (i, m) in mask.iter().enumerate() {
                    if *m {
                        out.push(i);
                    }
                }
                out
            }),
        }
    }
}
