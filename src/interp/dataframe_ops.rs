//! DataFrame / GroupBy column-verb helpers that take *unevaluated* column ASTs:
//! `df_column_verb` (where/filter/select/sort/group) and `groupby_agg` (the
//! aggregations). Shared by the tree-walker (`eval_df_method`) and the VM
//! (`Op::DfColumnVerb` / `Op::GroupByAgg`), so the two engines never diverge.

use super::*;

/// A DataFrame column argument is a bare identifier (`select(name, age)`).
fn arg_as_column_name(e: &Expr, line: usize, col: usize) -> Result<String, HelixError> {
    match e {
        Expr::Ident { name, .. } => Ok(name.clone()),
        _ => Err(
            HelixError::new("expected a column name", line, col)
                .hint("write a bare column name, e.g. `df.select(name, age)`."),
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

/// A DataFrame column-verb that takes *unevaluated* column/predicate ASTs:
/// `where`/`filter`/`select`/`sort`/`group`. The single source of truth for both
/// the tree-walker ([`Interp::eval_df_method`]) and the VM (`Op::DfDispatch`).
/// `resolve_var` resolves a bare name that is *not* a column to a Helix variable's
/// value (for predicates like `where(age > threshold)`).
pub(crate) fn df_column_verb(
    lf: &Rc<LazyFrame>,
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
            let columns = dataframe::column_names(lf, line, col)?;
            let pred = dataframe::to_polars(&args[0], &columns, resolve_var)?;
            Ok(Value::DataFrame(Rc::new(dataframe::filter(lf, pred))))
        }
        "select" => {
            let names = column_name_args(args, line, col)?;
            Ok(Value::DataFrame(Rc::new(dataframe::select(lf, &names))))
        }
        "sort" => {
            let names = column_name_args(args, line, col)?;
            Ok(Value::DataFrame(Rc::new(dataframe::sort(lf, &names))))
        }
        "group" => {
            let names = column_name_args(args, line, col)?;
            Ok(Value::GroupBy { lf: lf.clone(), keys: Rc::new(names) })
        }
        _ => unreachable!("df_column_verb only handles where/filter/select/sort/group"),
    }
}

/// A grouped-DataFrame aggregation over one column: `mean`/`sum`/`min`/`max`/
/// `count`/`std`. Shared by the tree-walker and the VM (`Op::DfDispatch`).
pub(crate) fn groupby_agg(
    lf: &Rc<LazyFrame>,
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
            let out = dataframe::group_agg(lf, keys, name, &value_col, line, col)?;
            Ok(Value::DataFrame(Rc::new(out)))
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
        lf: Rc<LazyFrame>,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        match name {
            "where" | "filter" | "select" | "sort" | "group" => {
                let env = &self.env;
                let resolve = |n: &str| env.get(n).map(|b| b.value.clone());
                df_column_verb(&lf, name, args, &resolve, line, col)
            }
            "head" => {
                if args.len() != 1 {
                    return Err(HelixError::new("`head` takes a row count", line, col)
                        .hint("e.g. `df.head(5)`."));
                }
                let v = self.eval(&args[0])?;
                let n = as_int(&v, "head", line, col)?.max(0) as usize;
                Ok(Value::DataFrame(Rc::new(dataframe::head(&lf, n))))
            }
            "count" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`count` takes no arguments", line, col));
                }
                Ok(Value::Int(dataframe::row_count(&lf, line, col)? as i64))
            }
            "cache" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`cache` takes no arguments", line, col)
                        .hint("e.g. `big = read_csv(\"x.csv\").cache()` to reuse without re-scanning."));
                }
                Ok(Value::DataFrame(Rc::new(dataframe::cache(&lf, line, col)?)))
            }
            "columns" => {
                if !args.is_empty() {
                    return Err(HelixError::new("`columns` takes no arguments", line, col));
                }
                let names: Vec<Value> = dataframe::column_names(&lf, line, col)?
                    .into_iter()
                    .map(|c| Value::Str(Rc::new(c)))
                    .collect();
                Ok(Value::Array(Rc::new(names)))
            }
            _ => {
                const DF_METHODS: &[&str] = &[
                    "where", "select", "sort", "group", "head", "count", "columns", "cache",
                ];
                let mut err = HelixError::new(
                    format!("a DataFrame has no method `{}`", name),
                    line,
                    col,
                );
                if let Some(s) = suggest(name, DF_METHODS) {
                    err = err.hint(format!("did you mean `{}`?", s));
                } else {
                    err = err.hint(format!("DataFrame methods: {}", DF_METHODS.join(", ")));
                }
                Err(err)
            }
        }
    }

    pub(super) fn eval_groupby_method(
        &mut self,
        lf: Rc<LazyFrame>,
        keys: Rc<Vec<String>>,
        name: &str,
        args: &[Expr],
        line: usize,
        col: usize,
    ) -> Result<Value, HelixError> {
        groupby_agg(&lf, &keys, name, args, line, col)
    }
}
