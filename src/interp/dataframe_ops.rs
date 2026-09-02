//! DataFrame / GroupBy column-verb helpers that take *unevaluated* column ASTs:
//! `df_column_verb` (where/filter/select/sort/group) and `groupby_agg` (the
//! aggregations). Shared by the tree-walker (`eval_df_method`) and the VM
//! (`Op::DfColumnVerb` / `Op::GroupByAgg`), so the two engines never diverge.

use super::*;

/// A DataFrame column argument is the `@name` sigil (`select(@name, @age)`) or a
/// bare identifier (`select(name, age)` — the legacy spelling, still accepted).
fn arg_as_column_name(
    e: &Expr,
    resolve_var: &dyn Fn(&str) -> Option<Value>,
    line: usize,
    col: usize,
) -> Result<String, HelixError> {
    match e {
        // `@name` PINS THE COLUMN, always. It is the spelling for "the column literally
        // called this", and it has to keep meaning that whatever is in scope.
        Expr::Column { name, .. } => Ok(name.clone()),
        // A BINDING IN SCOPE WINS OVER A COLUMN OF THE SAME NAME (ADR 0028), and its value
        // names the column. This position was the last one where it did not, and the shape
        // of the bug is the one ADR 0028 already argued about:
        //
        //     fn top(frame, key) = frame.select(key)
        //
        // returned the column `key` on any frame that happened to have one, ignoring the
        // caller's argument entirely — so a library's PARAMETER NAMES were reserved words
        // in data it has never seen. Same function, same argument, different answer, exit 0,
        // and all three engines agreed because all three were equally wrong.
        //
        // Making this rule uniform also supplies the thing the docs called missing: a column
        // named at RUN TIME. `k = "price"` then `df.sort(k)` sorts by `price`, where before
        // there was no way to express it in this position at all.
        Expr::Ident { name, .. } => match resolve_var(name) {
            None => Ok(name.clone()),
            Some(Value::Str(s)) => Ok((*s).clone()),
            Some(other) => Err(HelixError::new(
                format!(
                    "`{name}` is a {} in scope, and a column name must be a String",
                    other.type_name()
                ),
                line,
                col,
            )
            .hint(format!(
                "write `@{name}` for the column called `{name}`, or bind a String naming \
                 the column you want."
            ))),
        },
        // A STRING NAMES A COLUMN TOO. `df.column("v")` already takes one, and a binding
        // resolves to exactly this, so refusing the literal spelling of the thing bindings
        // produce would be a rule with no reason behind it.
        Expr::Str(s) => Ok(s.clone()),
        _ => Err(
            HelixError::new("expected a column name", line, col)
                .hint("write a column with the `@` sigil, e.g. `df.select(@name, @age)`."),
        ),
    }
}

/// "a grouped DataFrame has no aggregation `x`" — one sentence, both engines.
///
/// The VM reached its own spelling ("a GroupBy has no value-method `x`") whenever a group
/// arrived somewhere the compiler had routed as a value method; the walker reached this
/// one. Same outcome, two sentences, one program.
pub(crate) fn no_such_aggregation(name: &str, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("a grouped DataFrame has no aggregation `{name}`"), line, col)
        .hint("try mean, sum, min, max, count, or std.")
}

/// "the thing you asked to join with is not a frame" — one sentence, both engines.
///
/// The VM reaches this from `Op::DfJoin` and the tree-walker from its own join arm, and
/// they spelled it differently ("needs a DataFrame to join with, found Int" against
/// "expects a DataFrame, found Int"): the same outcome in two sentences, which is still
/// one program answered two ways.
pub(crate) fn join_operand_err(v: &Value, line: usize, col: usize) -> HelixError {
    HelixError::new(format!("`join` expects a DataFrame, found {}", v.type_name()), line, col)
        .hint("e.g. `samples.join(meta, sample_id)`.")
}

/// Column-name arguments for `select`/`sort`/`group` (each must be a bare ident).
pub(crate) fn column_name_args(
    args: &[Expr],
    resolve_var: &dyn Fn(&str) -> Option<Value>,
    line: usize,
    col: usize,
) -> Result<Vec<String>, HelixError> {
    if args.is_empty() {
        return Err(HelixError::new("expected at least one column name", line, col)
            .hint("e.g. `df.select(name, age)`."));
    }
    args.iter().map(|a| arg_as_column_name(a, resolve_var, line, col)).collect()
}

/// A column name written as a bare word: a binding in scope wins, else the word itself.
///
/// The same rule `arg_as_column_name` applies to an *expression* argument, for the two
/// positions where the name is not an expression at all — a `with` record's key and a
/// join key. Only a `Str` binding counts: a name that happens to be bound to a number or
/// a frame is not a column name, and treating it as one would turn a type mistake into a
/// silent lookup of something that will never exist.
pub(crate) fn column_name_from_binding(
    name: &str,
    resolve_var: &dyn Fn(&str) -> Option<Value>,
) -> String {
    match resolve_var(name) {
        Some(Value::Str(s)) => (*s).clone(),
        _ => name.to_string(),
    }
}

/// Parse the trailing arguments of `a.join(b, key.., [how])`: the key columns are
/// bare identifiers; an optional final string literal selects the join type. Shared
/// by the tree-walker (`eval_df_method`) and the VM (compiled into `Op::DfJoin`).
pub(crate) fn parse_join_spec(
    args: &[Expr],
    resolve_var: &dyn Fn(&str) -> Option<Value>,
    line: usize,
    col: usize,
) -> Result<(Vec<String>, String), HelixError> {
    let mut keys = Vec::new();
    let mut how = String::from("inner");
    for (i, a) in args.iter().enumerate() {
        match a {
            // `@name` PINS the column, as everywhere else; a bare word takes the binding
            // if there is one, so `fn on(l, r, k) = l.join(r, k)` joins on the caller's
            // key instead of refusing with "no column `k`".
            Expr::Column { name, .. } => keys.push(name.clone()),
            Expr::Ident { name, .. } => {
                keys.push(column_name_from_binding(name, resolve_var))
            }
            Expr::Str(s) if i == args.len() - 1 => how = s.clone(),
            // A STRING LITERAL BEFORE THE LAST ARGUMENT IS A KEY. ADR 0028 says a String
            // literal names a column — `df.select("price")` is documented — and join was
            // the one name position that did not accept one, because `Expr::Str` was
            // matched only at the trailing index where it means the type. With an options
            // record marking the type, a string before it cannot be the type, so there is
            // nothing left for it to be. The trailing spelling is untouched.
            Expr::Str(s) => keys.push(s.clone()),
            // THE OPTIONS RECORD, and the only way to give the join type from a BINDING.
            //
            // A trailing string literal is sugar, and it is all there was: the type was
            // recognised by its SYNTAX, so `fn on(l, r, k, how) = l.join(r, k, how)` put
            // `how` in the key set and failed with "no column `left`". Pinning the key
            // (`l.join(r, @id, how)`) failed identically, which is what proves this is not
            // an ambiguity between key and type — a bare name in this list is simply always
            // a key.
            //
            // Deciding the role from the VALUE instead would trade a refusal for a wrong
            // answer: `l.join(r, k1, k2)` where `k2` is "left" and no such column exists is
            // a clean error today, and would silently become a left join on `k1` alone. A
            // record cannot be a key, so it disambiguates at any key count, and it is the
            // idiom `http_request({method, url, ...})` already uses.
            Expr::Record(fields) if i == args.len() - 1 => {
                for (k, v) in fields {
                    if k != "how" {
                        return Err(HelixError::new(
                            format!("`join` has no option `{k}`"),
                            line,
                            col,
                        )
                        .hint("the only join option is `how`, e.g. `{how: \"left\"}`."));
                    }
                    how = match v {
                        Expr::Str(s) => s.clone(),
                        Expr::Ident { name, .. } => match resolve_var(name) {
                            Some(Value::Str(s)) => (*s).clone(),
                            Some(other) => {
                                return Err(HelixError::new(
                                    format!(
                                        "a join type must be a string, but `{name}` is {}",
                                        other.type_name()
                                    ),
                                    line,
                                    col,
                                ))
                            }
                            None => {
                                return Err(HelixError::new(
                                    format!("no variable named `{name}`"),
                                    line,
                                    col,
                                )
                                .hint("the join type comes from a string, e.g. `{how: \"left\"}`."))
                            }
                        },
                        _ => {
                            return Err(HelixError::new(
                                "a join type must be a string or a name bound to one",
                                line,
                                col,
                            )
                            .hint("e.g. `{how: \"left\"}` or `{how: kind}`."))
                        }
                    };
                }
            }
            // A record ANYWHERE ELSE is a misplaced options record, not a mystery. Say so,
            // because the generic "a join key must be a bare column name" would send the
            // reader looking at the wrong argument.
            Expr::Record(_) => {
                return Err(HelixError::new(
                    "`join` options must come last",
                    line,
                    col,
                )
                .hint("e.g. `left.join(right, id, {how: \"left\"})`."))
            }
            _ => {
                return Err(HelixError::new(
                    "a join key must be a bare column name",
                    line,
                    col,
                )
                .hint("e.g. `samples.join(meta, sample_id)`, with an optional join type like `\"left\"` or `{how: kind}` last."))
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
        // Keep the rows where EVERY column is non-missing — the frame-level escape
        // hatch ADR 0001's propagation semantics need: `where(@v == missing)` selects
        // nothing (correctly), so intent about missing rows must have its own verb.
        // Desugars to the same filter `where` runs — `not c1.is_missing() and …` over
        // every column — so it is backend-agnostic and inherits filter's validation
        // and engine parity for free rather than growing a second seam.
        // The frame twin of the Array verb, built from `FloatPred` (which C9 admitted
            // into queries). Keeps the rows where NO column holds a NaN — the same
            // shape `drop_missing` uses for `missing`, and deliberately NOT the same
            // predicate: the two verbs remove different things.
        "drop_nan" => {
            if !args.is_empty() {
                return Err(HelixError::new("`drop_nan` takes no arguments", line, col)
                    .hint("it keeps the rows where no column holds a NaN; to test \
                           one column, use `df.where(not is_nan(@col))`."));
            }
            use crate::backend::{ColExpr, FloatPredKind};
            let columns = lf.column_names(line, col)?;
            let mut pred = ColExpr::Lit(Value::Bool(true));
            for (i, c) in columns.iter().enumerate() {
                let keep = ColExpr::Unary(
                    crate::ast::UnOp::Not,
                    Box::new(ColExpr::FloatPred(
                        FloatPredKind::IsNan,
                        Box::new(ColExpr::Col(c.clone())),
                    )),
                );
                pred = if i == 0 {
                    keep
                } else {
                    ColExpr::Binary(crate::ast::BinOp::And, Box::new(pred), Box::new(keep))
                };
            }
            crate::backend::validate_predicate(&pred, line, col)?;
            Ok(Value::dataframe(lf.filter(&pred, line, col)?))
        }
        "drop_missing" => {
            if !args.is_empty() {
                return Err(HelixError::new("`drop_missing` takes no arguments", line, col)
                    .hint("it keeps the rows where every column is non-missing; to test \
                           one column, use `df.where(not @col.is_missing())`."));
            }
            use crate::backend::ColExpr;
            let columns = lf.column_names(line, col)?;
            let mut pred = ColExpr::Lit(Value::Bool(true));
            for (i, c) in columns.iter().enumerate() {
                let keep = ColExpr::Unary(
                    crate::ast::UnOp::Not,
                    Box::new(ColExpr::IsMissing(Box::new(ColExpr::Col(c.clone())))),
                );
                pred = if i == 0 {
                    keep
                } else {
                    ColExpr::Binary(crate::ast::BinOp::And, Box::new(pred), Box::new(keep))
                };
            }
            crate::backend::validate_predicate(&pred, line, col)?;
            Ok(Value::dataframe(lf.filter(&pred, line, col)?))
        }
        "select" => {
            let names = column_name_args(args, resolve_var, line, col)?;
            crate::backend::validate_columns_exist(lf, &names, line, col)?;
            Ok(Value::dataframe(lf.select(&names, line, col)?))
        }
        "sort" => {
            let names = column_name_args(args, resolve_var, line, col)?;
            crate::backend::validate_columns_exist(lf, &names, line, col)?;
            Ok(Value::dataframe(lf.sort(&names, line, col)?))
        }
        "group" => {
            let names = column_name_args(args, resolve_var, line, col)?;
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
                // THE KEY IS A COLUMN NAME, so it follows the column-name rule
                // rather than a record literal's. `f.with({to: @x})` used to add a column
                // literally called `to` even with `to` bound in scope — a wrong answer with
                // no error, on all three engines, which is the exact shape ADR 0028's
                // opening paragraph names. A library renaming a column cannot know the
                // caller's schema, and until now had no way to say which name it meant.
                //
                // ADR 0028 decided the READ positions and left this one open in as many
                // words: "does the same rule apply to the name being DEFINED?". This is
                // that question answered — the same way, because the argument that
                // settled reads is about the library author's blindness to the caller's
                // schema, and defining a column is no less blind than reading one.
                cols.push((column_name_from_binding(cname, resolve_var), ce));
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
    resolve_var: &dyn Fn(&str) -> Option<Value>,
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    match name {
        "mean" | "sum" | "min" | "max" | "count" | "std" => {
            if args.len() != 1 {
                return Err(HelixError::new(format!("grouped `{}` takes one column", name), line, col)
                    .hint("e.g. `genes.group(species).mean(expression)`."));
            }
            let value_col = arg_as_column_name(&args[0], resolve_var, line, col)?;
            crate::backend::validate_columns_exist(
                handle,
                std::slice::from_ref(&value_col),
                line,
                col,
            )?;
            Ok(Value::dataframe(handle.group_agg(keys, name, &value_col, line, col)?))
        }
        _ => Err(no_such_aggregation(name, line, col)),
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
            "where" | "filter" | "drop_missing" | "drop_nan" | "select" | "sort"
            | "group" | "with" => {
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
                    v => return Err(join_operand_err(&v, line, col)),
                };
                let resolve = |n: &str| self.lookup(n).map(|b| b.value.clone());
                let (keys, how) = parse_join_spec(&args[1..], &resolve, line, col)?;
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
        // The same resolution a column verb gets: an aggregation's VALUE column is a column
        // name in exactly the same sense, so `group(@k).mean(v)` with `v` bound must mean
        // the column `v` names, not a column literally called `v`.
        let resolve = |n: &str| self.lookup(n).map(|b| b.value.clone());
        groupby_agg(&handle, &keys, name, args, &resolve, line, col)
    }
}
