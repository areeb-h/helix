//! Typed column storage for the native engine — `Vec`-backed values plus a
//! validity mask (ADR 0033 Stage 1). Four dtypes plus all-missing, exactly the
//! set the seam's `ColData`/`column_values` contract speaks.

use std::rc::Rc;

use crate::backend::ColData;
use crate::error::HelixError;
use crate::value::Value;

/// One column: values plus validity. An invalid slot's payload is a placeholder
/// (0 / 0.0 / false / "") and must never be read except through [`Col::get`].
#[derive(Clone, Debug)]
pub enum Col {
    I64 { vals: Vec<i64>, valid: Vec<bool> },
    F64 { vals: Vec<f64>, valid: Vec<bool> },
    Bool { vals: Vec<bool>, valid: Vec<bool> },
    Str { vals: Vec<String>, valid: Vec<bool> },
    /// A column with no non-missing value (its dtype is unknowable).
    Null { len: usize },
}

impl Col {
    pub fn len(&self) -> usize {
        match self {
            Col::I64 { vals, .. } => vals.len(),
            Col::F64 { vals, .. } => vals.len(),
            Col::Bool { vals, .. } => vals.len(),
            Col::Str { vals, .. } => vals.len(),
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
            Col::Str { vals, valid } => {
                if valid[i] { Value::Str(Rc::new(vals[i].clone())) } else { Value::Missing }
            }
            Col::Null { .. } => Value::Missing,
        }
    }

    /// Gather rows by index (sort/filter/join all reduce to this).
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
            Col::Str { vals, valid } => Col::Str {
                vals: idx.iter().map(|&i| vals[i].clone()).collect(),
                valid: idx.iter().map(|&i| valid[i]).collect(),
            },
            Col::Null { .. } => Col::Null { len: idx.len() },
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
                let valid = vec![true; v.len()];
                Col::Str { vals: v, valid }
            }
            ColData::StrOpt(v) => {
                let valid: Vec<bool> = v.iter().map(Option::is_some).collect();
                let vals = v.into_iter().map(Option::unwrap_or_default).collect();
                Col::Str { vals, valid }
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
                let mut vals = Vec::with_capacity(n);
                let mut valid = Vec::with_capacity(n);
                for v in cells {
                    match v {
                        Value::Str(s) => {
                            vals.push((**s).clone());
                            valid.push(true);
                        }
                        _ => {
                            vals.push(String::new());
                            valid.push(false);
                        }
                    }
                }
                Col::Str { vals, valid }
            }
        })
    }
}
