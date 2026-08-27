//! SQLite queries as DataFrames (`sqlite_query`), behind the `db` cargo feature.
//!
//! **Why a query returns a DataFrame.** Helix already has a full frame verb surface —
//! `where`, `group`, `sort`, `join`, `write_csv`, the whole of `helix doc DataFrame`. A
//! query that returned rows-of-records would arrive next to that surface and not join it;
//! returning a `Df` means the result plugs straight into everything a `read_csv` result
//! can do. It is the same decision `read_csv` and the genomics readers already made, and
//! it goes through the same backend-agnostic seam (`backend::build_frame`), so this works
//! on the polars backend and the native one alike.
//!
//! **Read-only, and the capability label is therefore true.** Opening a SQLite file can
//! create it, and arbitrary SQL can write to it — so a verb classified `fs-read` that
//! could execute `DELETE` would be a lie in the audit log. This opens the database
//! `SQLITE_OPEN_READ_ONLY`, which makes the classification honest: `sqlite_query` reads.
//! Writing needs its own verb and its own `fs-write` label, which is a later stage rather
//! than a flag on this one.
//!
//! **Parameters are values, never text.** `sqlite_query(path, sql, params)` binds
//! `params` positionally to `?`. Building SQL by interpolation is how injection happens,
//! and ADR 0037 made the same call for subprocesses: the safe form is the only form the
//! API offers.

// `ColData` is only used by the real implementation; `Df` is in both signatures.
#[cfg(feature = "db")]
use crate::backend::ColData;
use crate::backend::Df;
use crate::error::HelixError;
use crate::value::Value;

#[cfg(feature = "db")]
/// One column being accumulated. SQLite is dynamically typed per VALUE, not per column,
/// so the column type is discovered from the rows and widens as they arrive.
enum Col {
    /// Nothing but NULL so far — the type is still unknown.
    Null(usize),
    Int(Vec<Option<i64>>),
    Float(Vec<Option<f64>>),
    Str(Vec<Option<String>>),
}

#[cfg(feature = "db")]
impl Col {
    fn len(&self) -> usize {
        match self {
            Col::Null(n) => *n,
            Col::Int(v) => v.len(),
            Col::Float(v) => v.len(),
            Col::Str(v) => v.len(),
        }
    }

    /// Widen so this column can hold `v`, then push it.
    ///
    /// The widening order is Int -> Float -> Str, which is the order that loses the least:
    /// an Int column meeting a Float becomes Float; anything meeting text becomes text.
    /// A column of only NULLs stays unknown until something real arrives, so a fully-NULL
    /// column lands as a null String column rather than guessing a type it never saw.
    fn push(&mut self, v: SqlVal) {
        match (&mut *self, v) {
            (_, SqlVal::Null) => match self {
                Col::Null(n) => *n += 1,
                Col::Int(c) => c.push(None),
                Col::Float(c) => c.push(None),
                Col::Str(c) => c.push(None),
            },
            (Col::Null(n), val) => {
                let nulls = *n;
                *self = match val {
                    SqlVal::Int(i) => Col::Int({
                        let mut v = vec![None; nulls];
                        v.push(Some(i));
                        v
                    }),
                    SqlVal::Float(f) => Col::Float({
                        let mut v = vec![None; nulls];
                        v.push(Some(f));
                        v
                    }),
                    SqlVal::Str(s) => Col::Str({
                        let mut v = vec![None; nulls];
                        v.push(Some(s));
                        v
                    }),
                    SqlVal::Null => unreachable!("handled above"),
                };
            }
            (Col::Int(c), SqlVal::Int(i)) => c.push(Some(i)),
            (Col::Float(c), SqlVal::Float(f)) => c.push(Some(f)),
            (Col::Str(c), SqlVal::Str(s)) => c.push(Some(s)),
            // Widen: Int meeting a Float becomes Float.
            (Col::Int(c), SqlVal::Float(f)) => {
                let widened = c.iter().map(|x| x.map(|i| i as f64)).collect::<Vec<_>>();
                let mut widened = widened;
                widened.push(Some(f));
                *self = Col::Float(widened);
            }
            (Col::Float(c), SqlVal::Int(i)) => c.push(Some(i as f64)),
            // Anything meeting text becomes text, so no value is dropped.
            (Col::Int(c), SqlVal::Str(s)) => {
                let mut w: Vec<Option<String>> =
                    c.iter().map(|x| x.map(|i| i.to_string())).collect();
                w.push(Some(s));
                *self = Col::Str(w);
            }
            (Col::Float(c), SqlVal::Str(s)) => {
                let mut w: Vec<Option<String>> =
                    c.iter().map(|x| x.map(crate::value::fmt_float)).collect();
                w.push(Some(s));
                *self = Col::Str(w);
            }
            (Col::Str(c), SqlVal::Int(i)) => c.push(Some(i.to_string())),
            (Col::Str(c), SqlVal::Float(f)) => c.push(Some(crate::value::fmt_float(f))),
        }
    }

    fn into_coldata(self) -> ColData {
        match self {
            // Never saw a non-NULL value: an all-null String column states "no data"
            // without inventing a type.
            Col::Null(n) => ColData::StrOpt(vec![None; n]),
            Col::Int(v) => ColData::IntOpt(v),
            Col::Float(v) => ColData::Float(v),
            Col::Str(v) => ColData::StrOpt(v),
        }
    }
}

#[cfg(feature = "db")]
/// A SQLite cell, reduced to the four shapes Helix frames hold.
enum SqlVal {
    Null,
    Int(i64),
    Float(f64),
    Str(String),
}

/// `sqlite_query(path, sql, params?)` — run a read-only query and return a DataFrame.
#[cfg(feature = "db")]
pub fn sqlite_query(args: &[Value], line: usize, col: usize) -> Result<Df, HelixError> {
    use rusqlite::types::ValueRef;
    use rusqlite::OpenFlags;

    let err = |m: String| HelixError::new(m, line, col);
    let (Some(Value::Str(path)), Some(Value::Str(sql))) = (args.first(), args.get(1)) else {
        return Err(err("`sqlite_query` takes a database path and a SQL string".to_string())
            .hint("e.g. `sqlite_query(\"app.db\", \"select * from users where id = ?\", [7])`."));
    };

    // READ-ONLY, so the `fs-read` capability label is the truth. It also means a typo in
    // the path fails instead of silently creating an empty database — the failure mode
    // that makes "why is my table missing" take an afternoon.
    let conn = rusqlite::Connection::open_with_flags(
        path.as_str(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| {
        err(format!("could not open the database `{path}`: {e}"))
            .hint("`sqlite_query` opens read-only; the file must exist.".to_string())
    })?;

    let mut stmt = conn
        .prepare(sql.as_str())
        .map_err(|e| err(format!("could not prepare the query: {e}")))?;

    // Parameters bind as VALUES. There is deliberately no way to splice text into the
    // statement, which is what makes injection unrepresentable rather than discouraged.
    let params: Vec<rusqlite::types::Value> = match args.get(2) {
        None | Some(Value::Missing) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter_values()
            .map(|v| match v {
                Value::Int(i) => Ok(rusqlite::types::Value::Integer(i)),
                Value::Float(f) => Ok(rusqlite::types::Value::Real(f)),
                Value::Str(s) => Ok(rusqlite::types::Value::Text(s.to_string())),
                Value::Bool(b) => Ok(rusqlite::types::Value::Integer(i64::from(b))),
                Value::Missing => Ok(rusqlite::types::Value::Null),
                other => Err(err(format!(
                    "a query parameter must be a number, string, bool or missing, not {}",
                    crate::value::with_article(other.type_name())
                ))),
            })
            .collect::<Result<_, _>>()?,
        Some(other) => {
            return Err(err(format!(
                "`sqlite_query`'s parameters must be an array, not {}",
                crate::value::with_article(other.type_name())
            ))
            .hint("pass them positionally: `sqlite_query(db, \"… where id = ?\", [7])`.".to_string()));
        }
    };

    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut cols: Vec<Col> = names.iter().map(|_| Col::Null(0)).collect();

    let mut rows = stmt
        .query(rusqlite::params_from_iter(params))
        .map_err(|e| err(format!("the query failed: {e}")))?;
    while let Some(row) = rows.next().map_err(|e| err(format!("reading a row failed: {e}")))? {
        for (i, c) in cols.iter_mut().enumerate() {
            let v = match row.get_ref(i) {
                Ok(ValueRef::Null) | Err(_) => SqlVal::Null,
                Ok(ValueRef::Integer(n)) => SqlVal::Int(n),
                Ok(ValueRef::Real(f)) => SqlVal::Float(f),
                Ok(ValueRef::Text(t)) => SqlVal::Str(String::from_utf8_lossy(t).into_owned()),
                // A BLOB has no Helix scalar. Naming its size beats both a panic and a
                // silent empty string.
                Ok(ValueRef::Blob(b)) => SqlVal::Str(format!("<blob {} bytes>", b.len())),
            };
            c.push(v);
        }
    }

    // A query returning no columns at all (e.g. `pragma` forms) would build an empty
    // frame; report it instead of handing back something with no shape.
    if names.is_empty() {
        return Err(err("the query returned no columns".to_string())
            .hint("`sqlite_query` is for statements that SELECT rows.".to_string()));
    }
    let n = cols.first().map(|c| c.len()).unwrap_or(0);
    debug_assert!(cols.iter().all(|c| c.len() == n), "every column takes one value per row");
    let columns: Vec<(String, ColData)> =
        names.into_iter().zip(cols.into_iter().map(Col::into_coldata)).collect();
    crate::backend::build_frame(columns, line, col)
}

/// The error `sqlite_query` answers with in a build without the `db` feature — the
/// ADR 0032 gate-the-body shape: the builtin still exists, type-checks and describes
/// itself, and running it says what to rebuild with.
#[cfg(not(feature = "db"))]
pub fn sqlite_query(args: &[Value], line: usize, col: usize) -> Result<Df, HelixError> {
    let _ = args;
    Err(HelixError::new("this build has no SQLite support", line, col)
        .hint("rebuild with `--features db`."))
}
