//! Row keys for hashing — group-by and join both reduce to "these cells, as a
//! hashable, comparable tuple". Floats key by bit pattern (join/group equality is
//! identity, not epsilon); missing is its own key cell for GROUPING (missing
//! keys form their own group) while JOIN's "missing never matches" rule is
//! enforced by the join loop itself, not here.

use crate::value::Value;

use super::columns::Col;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum KeyCell {
    Missing,
    Int(i64),
    /// The float's raw bits — join/group equality is identity (NaN groups with
    /// NaN; `-0.0` and `0.0` are distinct bit patterns, as the oracle behaves).
    Float(u64),
    Bool(bool),
    Str(String),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RowKey(pub Vec<KeyCell>);

impl RowKey {
    pub fn at(cols: &[&Col], row: usize) -> RowKey {
        RowKey(cols.iter().map(|c| KeyCell::of(&c.get(row))).collect())
    }

    pub fn has_missing(&self) -> bool {
        self.0.iter().any(|c| matches!(c, KeyCell::Missing))
    }
}

impl KeyCell {
    pub fn of(v: &Value) -> KeyCell {
        match v {
            Value::Missing => KeyCell::Missing,
            Value::Int(i) => KeyCell::Int(*i),
            Value::Float(x) => KeyCell::Float(x.to_bits()),
            Value::Bool(b) => KeyCell::Bool(*b),
            Value::Str(s) => KeyCell::Str((**s).clone()),
            other => KeyCell::Str(other.to_string()),
        }
    }
}
