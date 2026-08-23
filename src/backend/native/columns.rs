//! Typed column storage for the native engine — `Vec`-backed values plus a
//! validity mask (ADR 0033 Stage 1). Four dtypes plus all-missing, exactly the
//! set the seam's `ColData`/`column_values` contract speaks.
//!
//! Strings are **dictionary-encoded** (Stage 3's structural move): a column is
//! `u32` codes into a shared `Rc<Vec<Rc<String>>>` dictionary. Row movement
//! (filter/sort/join/take) gathers 4-byte codes and bumps ONE refcount for the
//! whole dictionary; equality filters and group-bys can work on codes as
//! integers; low-cardinality columns (the common case in real data) shrink by
//! an order of magnitude. A worst-case all-unique column degrades to codes
//! 0..n — four extra bytes per row, nothing else lost.

use std::collections::HashMap;
use std::rc::Rc;

use crate::backend::ColData;
use crate::error::HelixError;
use crate::value::Value;

/// One column: values plus validity. An invalid slot's payload is a placeholder
/// (0 / 0.0 / false / code 0) and must never be read except through [`Col::get`].
#[derive(Clone, Debug)]
pub enum Col {
    I64 { vals: Vec<i64>, valid: Vec<bool> },
    F64 { vals: Vec<f64>, valid: Vec<bool> },
    Bool { vals: Vec<bool>, valid: Vec<bool> },
    Str { dict: Rc<Vec<Rc<String>>>, codes: Vec<u32>, valid: Vec<bool> },
    /// A column with no non-missing value (its dtype is unknowable).
    Null { len: usize },
}

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

    pub fn finish(self) -> Col {
        Col::Str { dict: Rc::new(self.dict), codes: self.codes, valid: self.valid }
    }
}

impl Col {
    pub fn len(&self) -> usize {
        match self {
            Col::I64 { vals, .. } => vals.len(),
            Col::F64 { vals, .. } => vals.len(),
            Col::Bool { vals, .. } => vals.len(),
            Col::Str { codes, .. } => codes.len(),
            Col::Null { len } => *len,
        }
    }

    /// The cell at `i` as a language value. Out of range is a caller bug — the
    /// frame's verbs only produce in-range indices.
    pub fn get(&self, i: usize) -> Value {
        match self {
            Col::I64 { vals, valid } => {
                if valid[i] { Value::Int(vals[i]) } else { Value::Missing }
            }
            Col::F64 { vals, valid } => {
                if valid[i] { Value::Float(vals[i]) } else { Value::Missing }
            }
            Col::Bool { vals, valid } => {
                if valid[i] { Value::Bool(vals[i]) } else { Value::Missing }
            }
            Col::Str { dict, codes, valid } => {
                if valid[i] {
                    Value::Str(dict[codes[i] as usize].clone())
                } else {
                    Value::Missing
                }
            }
            Col::Null { .. } => Value::Missing,
        }
    }


    /// Gather rows by index (sort/filter/join all reduce to this). A string
    /// column shares its dictionary — the gather moves 4-byte codes.
    pub fn take(&self, idx: &[usize]) -> Col {
        match self {
            Col::I64 { vals, valid } => Col::I64 {
                vals: idx.iter().map(|&i| vals[i]).collect(),
                valid: idx.iter().map(|&i| valid[i]).collect(),
            },
            Col::F64 { vals, valid } => Col::F64 {
                vals: idx.iter().map(|&i| vals[i]).collect(),
                valid: idx.iter().map(|&i| valid[i]).collect(),
            },
            Col::Bool { vals, valid } => Col::Bool {
                vals: idx.iter().map(|&i| vals[i]).collect(),
                valid: idx.iter().map(|&i| valid[i]).collect(),
            },
            Col::Str { dict, codes, valid } => Col::Str {
                dict: dict.clone(),
                codes: idx.iter().map(|&i| codes[i]).collect(),
                valid: idx.iter().map(|&i| valid[i]).collect(),
            },
            Col::Null { .. } => Col::Null { len: idx.len() },
        }
    }

    /// Gather with missing fill: `None` slots become invalid. The join's side
    /// columns reduce to this — typed, never boxing a cell.
    pub fn take_opt(&self, idx: &[Option<usize>]) -> Col {
        match self {
            Col::I64 { vals, valid } => {
                let mut v = Vec::with_capacity(idx.len());
                let mut m = Vec::with_capacity(idx.len());
                for o in idx {
                    match o {
                        Some(i) => {
                            v.push(vals[*i]);
                            m.push(valid[*i]);
                        }
                        None => {
                            v.push(0);
                            m.push(false);
                        }
                    }
                }
                Col::I64 { vals: v, valid: m }
            }
            Col::F64 { vals, valid } => {
                let mut v = Vec::with_capacity(idx.len());
                let mut m = Vec::with_capacity(idx.len());
                for o in idx {
                    match o {
                        Some(i) => {
                            v.push(vals[*i]);
                            m.push(valid[*i]);
                        }
                        None => {
                            v.push(0.0);
                            m.push(false);
                        }
                    }
                }
                Col::F64 { vals: v, valid: m }
            }
            Col::Bool { vals, valid } => {
                let mut v = Vec::with_capacity(idx.len());
                let mut m = Vec::with_capacity(idx.len());
                for o in idx {
                    match o {
                        Some(i) => {
                            v.push(vals[*i]);
                            m.push(valid[*i]);
                        }
                        None => {
                            v.push(false);
                            m.push(false);
                        }
                    }
                }
                Col::Bool { vals: v, valid: m }
            }
            Col::Str { dict, codes, valid } => {
                let mut c = Vec::with_capacity(idx.len());
                let mut m = Vec::with_capacity(idx.len());
                for o in idx {
                    match o {
                        Some(i) => {
                            c.push(codes[*i]);
                            m.push(valid[*i]);
                        }
                        None => {
                            c.push(0);
                            m.push(false);
                        }
                    }
                }
                Col::Str { dict: dict.clone(), codes: c, valid: m }
            }
            Col::Null { .. } => Col::Null { len: idx.len() },
        }
    }

    /// Coalesce-gather for a join's key column: the left row's cell when the
    /// pair has one, else the right's. Both sides share a dtype (checked by the
    /// caller); mixed dtypes take the boxed path instead.
    pub fn coalesce_gather(
        left: &Col,
        right: &Col,
        pairs: &[(Option<usize>, Option<usize>)],
    ) -> Option<Col> {
        match (left, right) {
            (Col::I64 { vals: lv, valid: lm }, Col::I64 { vals: rv, valid: rm }) => {
                let mut v = Vec::with_capacity(pairs.len());
                let mut m = Vec::with_capacity(pairs.len());
                for (l, r) in pairs {
                    match (l, r) {
                        (Some(i), _) => {
                            v.push(lv[*i]);
                            m.push(lm[*i]);
                        }
                        (None, Some(j)) => {
                            v.push(rv[*j]);
                            m.push(rm[*j]);
                        }
                        (None, None) => {
                            v.push(0);
                            m.push(false);
                        }
                    }
                }
                Some(Col::I64 { vals: v, valid: m })
            }
            (Col::F64 { vals: lv, valid: lm }, Col::F64 { vals: rv, valid: rm }) => {
                let mut v = Vec::with_capacity(pairs.len());
                let mut m = Vec::with_capacity(pairs.len());
                for (l, r) in pairs {
                    match (l, r) {
                        (Some(i), _) => {
                            v.push(lv[*i]);
                            m.push(lm[*i]);
                        }
                        (None, Some(j)) => {
                            v.push(rv[*j]);
                            m.push(rm[*j]);
                        }
                        (None, None) => {
                            v.push(0.0);
                            m.push(false);
                        }
                    }
                }
                Some(Col::F64 { vals: v, valid: m })
            }
            (Col::Bool { vals: lv, valid: lm }, Col::Bool { vals: rv, valid: rm }) => {
                let mut v = Vec::with_capacity(pairs.len());
                let mut m = Vec::with_capacity(pairs.len());
                for (l, r) in pairs {
                    match (l, r) {
                        (Some(i), _) => {
                            v.push(lv[*i]);
                            m.push(lm[*i]);
                        }
                        (None, Some(j)) => {
                            v.push(rv[*j]);
                            m.push(rm[*j]);
                        }
                        (None, None) => {
                            v.push(false);
                            m.push(false);
                        }
                    }
                }
                Some(Col::Bool { vals: v, valid: m })
            }
            (
                Col::Str { dict: ld, codes: lc, valid: lm },
                Col::Str { dict: rd, codes: rc, valid: rm },
            ) => {
                // The dictionaries differ; hash-cons the union — Rc reuse, no
                // per-cell text allocation.
                let mut b = StrBuilder::with_capacity(pairs.len());
                for (l, r) in pairs {
                    match (l, r) {
                        (Some(i), _) => {
                            if lm[*i] {
                                b.push_rc(&ld[lc[*i] as usize]);
                            } else {
                                b.push_missing();
                            }
                        }
                        (None, Some(j)) => {
                            if rm[*j] {
                                b.push_rc(&rd[rc[*j] as usize]);
                            } else {
                                b.push_missing();
                            }
                        }
                        (None, None) => b.push_missing(),
                    }
                }
                Some(b.finish())
            }
            _ => None,
        }
    }

    pub fn dtype_name(&self) -> &'static str {
        match self {
            Col::I64 { .. } => "int",
            Col::F64 { .. } => "float",
            Col::Bool { .. } => "bool",
            Col::Str { .. } => "str",
            Col::Null { .. } => "missing",
        }
    }

    /// Same dtype, for vstack's eager schema check (`Null` is compatible with
    /// everything — it holds no counter-evidence).
    pub fn same_dtype(&self, other: &Col) -> bool {
        matches!(self, Col::Null { .. })
            || matches!(other, Col::Null { .. })
            || std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    pub fn from_coldata(data: ColData) -> Col {
        match data {
            ColData::Str(v) => {
                let mut b = StrBuilder::with_capacity(v.len());
                for s in &v {
                    b.push_str(s);
                }
                b.finish()
            }
            ColData::StrOpt(v) => {
                let mut b = StrBuilder::with_capacity(v.len());
                for o in &v {
                    match o {
                        Some(s) => b.push_str(s),
                        None => b.push_missing(),
                    }
                }
                b.finish()
            }
            ColData::Int(v) => {
                let valid = vec![true; v.len()];
                Col::I64 { vals: v, valid }
            }
            ColData::IntOpt(v) => {
                let valid: Vec<bool> = v.iter().map(Option::is_some).collect();
                let vals = v.into_iter().map(Option::unwrap_or_default).collect();
                Col::I64 { vals, valid }
            }
            ColData::Float(v) => {
                let valid: Vec<bool> = v.iter().map(Option::is_some).collect();
                let vals = v.into_iter().map(Option::unwrap_or_default).collect();
                Col::F64 { vals, valid }
            }
            ColData::Bool(v) => {
                let valid = vec![true; v.len()];
                Col::Bool { vals: v, valid }
            }
        }
    }

    /// Pack evaluated cells into a typed column. Ints promote to Float when the
    /// cells mix Int and Float (the language's own promotion); any other mix is
    /// a clean error naming both types.
    pub fn from_values(
        name: &str,
        cells: &[Value],
        line: usize,
        col: usize,
    ) -> Result<Col, HelixError> {
        #[derive(PartialEq, Clone, Copy)]
        enum K {
            None,
            Int,
            Float,
            Bool,
            Str,
        }
        let mut kind = K::None;
        for v in cells {
            let k = match v {
                Value::Missing => continue,
                Value::Int(_) => K::Int,
                Value::Float(_) => K::Float,
                Value::Bool(_) => K::Bool,
                Value::Str(_) => K::Str,
                other => {
                    return Err(HelixError::new(
                        format!(
                            "column `{name}` cannot hold a value of type {}",
                            other.type_name()
                        ),
                        line,
                        col,
                    ))
                }
            };
            kind = match (kind, k) {
                (K::None, k) => k,
                (a, b) if a == b => a,
                (K::Int, K::Float) | (K::Float, K::Int) => K::Float,
                _ => {
                    return Err(HelixError::new(
                        format!("column `{name}` mixes incompatible value types"),
                        line,
                        col,
                    ))
                }
            };
        }
        let n = cells.len();
        Ok(match kind {
            K::None => Col::Null { len: n },
            K::Int => {
                let mut vals = Vec::with_capacity(n);
                let mut valid = Vec::with_capacity(n);
                for v in cells {
                    match v {
                        Value::Int(i) => {
                            vals.push(*i);
                            valid.push(true);
                        }
                        _ => {
                            vals.push(0);
                            valid.push(false);
                        }
                    }
                }
                Col::I64 { vals, valid }
            }
            K::Float => {
                let mut vals = Vec::with_capacity(n);
                let mut valid = Vec::with_capacity(n);
                for v in cells {
                    match v {
                        Value::Float(x) => {
                            vals.push(*x);
                            valid.push(true);
                        }
                        Value::Int(i) => {
                            vals.push(*i as f64);
                            valid.push(true);
                        }
                        _ => {
                            vals.push(0.0);
                            valid.push(false);
                        }
                    }
                }
                Col::F64 { vals, valid }
            }
            K::Bool => {
                let mut vals = Vec::with_capacity(n);
                let mut valid = Vec::with_capacity(n);
                for v in cells {
                    match v {
                        Value::Bool(b) => {
                            vals.push(*b);
                            valid.push(true);
                        }
                        _ => {
                            vals.push(false);
                            valid.push(false);
                        }
                    }
                }
                Col::Bool { vals, valid }
            }
            K::Str => {
                let mut b = StrBuilder::with_capacity(n);
                for v in cells {
                    match v {
                        Value::Str(s) => b.push_rc(s),
                        _ => b.push_missing(),
                    }
                }
                b.finish()
            }
        })
    }
}
