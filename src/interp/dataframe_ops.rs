//! DataFrame / GroupBy column-verb helpers that take *unevaluated* column ASTs:
//! `df_column_verb` (where/filter/select/sort/group) and `groupby_agg` (the
//! aggregations). Shared by the tree-walker (`eval_df_method`) and the VM
//! (`Op::DfColumnVerb` / `Op::GroupByAgg`), so the two engines never diverge.

use super::*;

/// A DataFrame column argument is the `@name` sigil (`select(@name, @age)`) or a
/// bare identifier (`select(name, age)` — the legacy spelling, still accepted).
fn arg_as_column_name(e: &Expr, line: usize, col: usize) -> Result<String, HelixError> {
    match e {
        Expr::Column { name, .. } | Expr::Ident { name, .. } => Ok(name.clone()),
        _ => Err(
            HelixError::new("expected a column name", line, col)
                .hint("write a column with the `@` sigil, e.g. `df.select(@name, @age)`."),
        ),
    }
}

/// Column-name arguments for `select`/`sort`/`group` (each must be a bare ident).
pub(crate) fn column_name_args(
    args: &[Expr],
    line: usize,
    col: usize,
) -> Result<Vec<String>, HelixError> {
    if args.is_empty() {
        return Err(HelixError::new("expected at least one column name", line, col)
            .hint("e.g. `df.select(name, age)`."));
    }
    args.iter().map(|a| arg_as_column_name(a, line, col)).collect()
}

/// Parse the trailing arguments of `a.join(b, key.., [how])`: the key columns are
/// bare identifiers; an optional final string literal selects the join type. Shared
/// by the tree-walker (`eval_df_method`) and the VM (compiled into `Op::DfJoin`).
pub(crate) fn parse_join_spec(
    args: &[Expr],
    line: usize,
    col: usize,
) -> Result<(Vec<String>, String), HelixError> {
    let mut keys = Vec::new();
    let mut how = String::from("inner");
    for (i, a) in args.iter().enumerate() {
        match a {
            Expr::Column { name, .. } | Expr::Ident { name, .. } => keys.push(name.clone()),
            Expr::Str(s) if i == args.len() - 1 => how = s.clone(),
            _ => {
                return Err(HelixError::new(
                    "a join key must be a bare column name",
                    line,
                    col,
                )
                .hint("e.g. `samples.join(meta, sample_id)`, with an optional join type like `\"left\"` last."))
            }
        }
    }
    if keys.is_empty() {
        return Err(HelixError::new("`join` needs at least one key column", line, col)
            .hint("e.g. `samples.join(meta, sample_id)`."));
    }
    Ok((keys, how))
}

/// A DataFrame column-verb that takes *unevaluated* column/predicate ASTs:
/// `where`/`filter`/`select`/`sort`/`group`. The single source of truth for both
/// the tree-walker ([`Interp::eval_df_method`]) and the VM (`Op::DfDispatch`).
/// `resolve_var` resolves a bare name that is *not* a column to a Helix variable's
/// value (for predicates like `where(age > threshold)`).
pub(crate) fn df_column_verb(
    lf: &Df,
    name: &str,
    args: &[Expr],
    resolve_var: &dyn Fn(&str) -> Option<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "where" | "filter" => {
            if args.len() != 1 {
                return Err(HelixError::new(format!("`{}` takes one predicate", name), line, col)
                    .hint("e.g. `patients.where(age > 40)`."));
            }
            let columns = lf.column_names(line, col)?;
            let pred = dataframe::ast_to_colexpr(&args[0], &columns, resolve_var)?;
            // Before any backend sees it. A predicate that is provably not a condition
            // ABORTED the process on the lazy CSV-scan path — see `validate_predicate`.
            crate::backend::validate_predicate(&pred, line, col)?;
            Ok(Value::dataframe(lf.filter(&pred, line, col)?))
        }
        "select" => {
            let names = column_name_args(args, line, col)?;
            crate::backend::validate_columns_exist(lf, &names, line, col)?;
            Ok(Value::dataframe(lf.select(&names, line, col)?))
        }
        "sort" => {
            let names = column_name_args(args, line, col)?;
            crate::backend::validate_columns_exist(lf, &names, line, col)?;
            Ok(Value::dataframe(lf.sort(&names, line, col)?))
        }
        "group" => {
            let names = column_name_args(args, line, col)?;
            crate::backend::validate_columns_exist(lf, &names, line, col)?;
            Ok(Value::GroupBy(Rc::new(crate::value::GroupByData {
                handle: lf.clone(),
                keys: Rc::new(names),
            })))
        }
        "with" => {
            // `df.with({name: expr, ...})` — add or replace columns from expressions
            // over existing columns, e.g. `df.with({bmi: weight / height})`.
            let fields = match args {
                [crate::ast::Expr::Record(fields)] => fields,
                _ => {
                    return Err(HelixError::new("`with` takes a record of new columns", line, col)
                        .hint("e.g. `df.with({bmi: weight / height})`."))
                }
            };
            let columns = lf.column_names(line, col)?;
            let mut cols = Vec::with_capacity(fields.len());
            for (cname, vexpr) in fields {
                let ce = dataframe::ast_to_colexpr(vexpr, &columns, resolve_var)?;
                cols.push((cname.clone(), ce));
            }
            Ok(Value::dataframe(lf.with_columns(&cols, line, col)?))
        }
        _ => unreachable!("df_column_verb only handles where/filter/select/sort/group/with"),
    }
}

/// A grouped-DataFrame aggregation over one column: `mean`/`sum`/`min`/`max`/
/// `count`/`std`. Shared by the tree-walker and the VM (`Op::DfDispatch`).
pub(crate) fn groupby_agg(
    handle: &Df,
    keys: &Rc<Vec<String>>,
    name: &str,
    args: &[Expr],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "mean" | "sum" | "min" | "max" | "count" | "std" => {
            if args.len() != 1 {
                return Err(HelixError::new(format!("grouped `{}` takes one column", name), line, col)
                    .hint("e.g. `genes.group(species).mean(expression)`."));
            }
            let value_col = arg_as_column_name(&args[0], line, col)?;
            crate::backend::validate_columns_exist(
                handle,
                std::slice::from_ref(&value_col),
                line,
                col,
            )?;
            Ok(Value::dataframe(handle.group_agg(keys, name, &value_col, line, col)?))
        }
        _ => Err(HelixError::new(
            format!("a grouped DataFrame has no aggregation `{}`", name),
            line,
            col,
        )
        .hint("try mean, sum, min, max, count, or std.")),
    }
}

impl super::Interp {
    pub(super) fn eval_df_method(
        &mut self,
        lf: Df,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match name {
            "where" | "filter" | "select" | "sort" | "group" | "with" => {
                // A bare name in a predicate resolves like any other name:
                // frame locals first, then globals (`df.where(@a > threshold)`
                // with a top-level `threshold`).
                let resolve = |n: &str| self.lookup(n).map(|b| b.value.clone());
                df_column_verb(&lf, name, args, &resolve, line, col)
            }
            "join" => {
                // The first argument is an *evaluated* DataFrame; the rest are key
                // columns (and an optional join-type string), parsed unevaluated.
                let other = match args.first() {
                    Some(e) => self.eval(e)?,
                    None => {
                        return Err(HelixError::new(
                            "`join` needs a DataFrame to join with",
                            line,
                            col,
                        )
                        .hint("e.g. `samples.join(meta, sample_id)`."))
                    }
                };
                let right = match other {
                    Value::DataFrame(lf) => lf,
                    v => {
                        return Err(HelixError::new(
                            format!("`join` expects a DataFrame, found {}", v.type_name()),
                            line,
                            col,
                        ))
                    }
                };
                let (keys, how) = parse_join_spec(&args[1..], line, col)?;
                Ok(Value::dataframe(lf.join(&right, &keys, &how, line, col)?))
            }
            // Every other DataFrame method takes plain *value* arguments (a row count,
            // a DataFrame, column-name strings, a path) rather than unevaluated column
            // refs. Evaluate them once and delegate to the shared `df_value_method` —
            // the same dispatcher the VM calls — so count/columns/cache/head/vstack/
            // unique/column/to_json/write_*/to_html/... and the unknown-method error are
            // ONE implementation, not two hand-synced copies. (This also aligns the
            // tree-walker to the VM on the edge where an arg is present but rejected on
            // arity, e.g. `df.count(expr)`: both now evaluate `expr` before erroring.)
            _ => {
                let vals: Vec<Value> = args.iter().map(|a| self.eval(a)).collect::<Result<_, _>>()?;
                crate::interp::access::df_value_method(&lf, name, vals, line, col)
            }
        }
    }

    pub(super) fn eval_groupby_method(
        &mut self,
        handle: Df,
        keys: Rc<Vec<String>>,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        groupby_agg(&handle, &keys, name, args, line, col)
    }
}
