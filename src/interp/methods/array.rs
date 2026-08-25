//! Array methods — the aggregations, transforms, and packed fast paths — moved verbatim from the one-file methods module (2026-08-24).

#[allow(unused_imports)]
use super::super::*;
#[allow(unused_imports)]
use super::*;
#[allow(unused_imports)]
use std::rc::Rc;

/// Numeric-reduction fast path for **typed** arrays (`Ints`/`Floats`): read the
/// packed buffer directly, never materializing a `Vec<Value>`. Returns `Ok(None)`
/// for a `Values` array, a non-reduction method, an argument-bearing call, or a
/// `Float` array containing `NaN` — so the caller's general, missing/NaN-aware path
/// runs and the result matches the untyped array exactly. Typed arrays are
/// missing-free by construction, so no missing check is needed here.
/// The index of the largest (`want_max`) or smallest element of a PACKED array, or `None`
/// to decline — which sends `argmin`/`argmax` back to the tuple reduce their desugar
/// produces, so a declined shape keeps its exact error text and caret column.
///
/// This is the whole of the `argmin`/`argmax` fix: those methods never reach runtime as
/// names (they are rewritten at parse time into `enumerate` + a reduce over tuples), so the
/// only way to give them a kernel is to give the desugar a verb that DOES reach here.
///
/// THE COMPARISON RULES DIFFER BY TYPE AND THAT IS DELIBERATE. They mirror
/// [`crate::interp::ops`]'s `<`/`>`, which is what the reduce this replaces actually
/// evaluates — NOT `total_cmp`, which every neighbouring packed arm (`sort`, `argsort`,
/// `min`, `max`) uses:
///
/// * Ints compare with exact `i64` ordering. `total_cmp` on an `as f64` cast would lose
///   precision above 2^53, and a comment in `ops.rs` records that exact bug.
/// * Floats compare with IEEE `>`/`<`, under which `0.0` and `-0.0` are EQUAL. So
///   `[0.0, -0.0].argmin()` and `[-0.0, 0.0].argmin()` both answer 0 — first-wins keeps
///   index 0 either way. A `total_cmp` kernel would order the zeros and answer 1 for two
///   of those four shapes, silently changing results under a performance commit. Whether
///   that IEEE answer is the right one is a separate, recorded, open question; reproducing
///   it is this change's job.
/// * ANY NaN declines. `[sqrt(-1.0)].argmax()` — one element, no comparison to make —
///   still raises "cannot compare these values (NaN?)" today, because the reduce compares
///   the seed against the first element. Note the opposite convention two arms above:
///   packed `sort`/`argsort` deliberately do NOT defer on NaN, because they have NaN
///   *placement* semantics rather than a raise.
///
/// Ties are first-wins (`[2,2,2].argmax()` → 0), so the scan must update only on a STRICT
/// improvement. A range needs no comparisons at all: it is strictly monotonic (a zero step
/// is rejected at construction), so the answer is an endpoint. Every empty array declines —
/// the empty error belongs to the reduce seed, and only the reduce can say it with the
/// right column.
pub(crate) fn packed_arg_extreme(ad: &crate::value::ArrayData, want_max: bool) -> Option<i64> {
    use crate::value::ArrayData;
    fn scan<T: Copy>(xs: &[T], better: impl Fn(T, T) -> bool) -> i64 {
        let mut best = 0usize;
        for i in 1..xs.len() {
            if better(xs[i], xs[best]) {
                best = i;
            }
        }
        best as i64
    }
    match ad {
        // Boxed and lazy-pair arrays keep the general path: their elements are `Value`s,
        // so there is no packed buffer to scan and nothing to win.
        ArrayData::Values(_) | ArrayData::Enumerate { .. } => None,
        ArrayData::Ints(xs) if !xs.is_empty() => Some(if want_max {
            scan(xs, |a, b| a > b)
        } else {
            scan(xs, |a, b| a < b)
        }),
        ArrayData::Floats(xs) if !xs.is_empty() && !xs.iter().any(|f| f.is_nan()) => {
            Some(if want_max { scan(xs, |a, b| a > b) } else { scan(xs, |a, b| a < b) })
        }
        ArrayData::Range { step, len, .. } if *len > 0 => {
            Some(if (*step > 0) == want_max { *len as i64 - 1 } else { 0 })
        }
        // Empties (in every representation) and float arrays holding a NaN.
        _ => None,
    }
}

pub(crate) fn array_numeric_fast(
    ad: &crate::value::ArrayData,
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Option<Value>, HelixError> {
    use crate::value::ArrayData;
    // Lazy-Range `take`/`drop`: O(1) re-slicing of the arithmetic progression — the
    // whole point of the lazy representation (`range(100000000).take(1)` previously
    // materialized ~1.6 GB of boxed Values to keep one element, contradicting the
    // documented O(1)). Int counts only, mirroring the general path exactly (negative
    // clamps to 0, over-take/-drop clamps to the length); float/missing counts defer
    // so the general path's errors stay identical. A fully-dropped range keeps an
    // empty representation rather than computing `start + step*len`, which the Range
    // invariant does not guarantee to fit i64.
    if let ("take" | "drop", [Value::Int(n)], ArrayData::Range { start, step, len }) =
        (name, args, ad)
    {
        let k = (*n).max(0).min(*len as i64) as usize;
        return Ok(Some(if name == "take" {
            Value::lazy_range(*start, *step, k)
        } else if k >= *len {
            Value::lazy_range(0, 1, 0)
        } else {
            // Element k is in range by the Range invariant, so the i128 math fits i64.
            Value::lazy_range(
                (*start as i128 + *step as i128 * k as i128) as i64,
                *step,
                *len - k,
            )
        }));
    }
    // The same re-slice for a PACKED numeric array. The lazy-`Range` arm above was added
    // when `range(100000000).take(1)` was found materializing ~1.6 GB to keep one element —
    // but a range that has been through a `map` is `Ints`/`Floats`, not `Range`, and that
    // spelling kept boxing the whole source: `(0..20_000_000).map(it * 2).take(3)` cost
    // 503 MB against 190 MB for the array alone, i.e. 320 MB of `Vec<Value>` to keep three
    // numbers. One defect, fixed for one representation and not its neighbour.
    //
    // Counts are clamped exactly as the arm above and the general path do (negative → 0,
    // over-take/-drop → the length), and a non-`Int` count defers so the general path's
    // errors stay identical.
    if let ("take" | "drop", [Value::Int(n)]) = (name, args) {
        let slice_ints = |v: &Vec<i64>| -> Value {
            let k = (*n).max(0).min(v.len() as i64) as usize;
            let part = if name == "take" { &v[..k] } else { &v[k..] };
            Value::int_array(part.to_vec())
        };
        let slice_floats = |v: &Vec<f64>| -> Value {
            let k = (*n).max(0).min(v.len() as i64) as usize;
            let part = if name == "take" { &v[..k] } else { &v[k..] };
            Value::float_array(part.to_vec())
        };
        match ad {
            ArrayData::Ints(v) => return Ok(Some(slice_ints(v))),
            ArrayData::Floats(v) => return Ok(Some(slice_floats(v))),
            _ => {}
        }
    }
    // `contains(v)` / `index_of(v)` answer a SCALAR, but both boxed the entire source to
    // do it: `(0..20_000_000).map(it * 2).contains(4)` cost 491 MB against 185 MB for the
    // array alone — 306 MB of `Vec<Value>` built to settle a question decided by element
    // 2. Their closure-taking neighbours `any(p)` / `position(p)` already stream and cost
    // nothing extra, so this is one operation with two spellings where only one was fixed:
    // the same shape as `take`/`drop` (packed vs lazy `Range`), `clamp` (array vs scalar),
    // `dot` (vs `sum`/`cumsum`), and duplicate record fields (literal vs update).
    //
    // The scan calls the SAME `values_equal` on the SAME `Value` the general path would
    // have built — `to_values()` is `(0..len).map(get)` for every non-`Values`
    // representation — one stack temporary at a time instead of a heap `Vec` of them. So
    // cross-type equality (`1 == 1.0`, Rational-vs-Int), `missing` identity equality and
    // `NaN != NaN` all stay exactly as they were by construction, not by re-derivation;
    // re-deriving them here is precisely how the second spelling drifts from the first.
    //
    // `Values` arrays already hold their `Value`s, so they defer — there is nothing to
    // avoid materializing, and the general path's `any`/`position` are the same scan.
    // A wrong arity also defers, so both methods' (differing) arity errors are untouched.
    if let ("contains" | "index_of", [needle]) = (name, args)
        && !matches!(ad, ArrayData::Values(_))
    {
        let hit =
            (0..ad.len()).position(|i| crate::interp::ops::values_equal(&ad.get(i), needle));
        return Ok(Some(match (name, hit) {
            ("contains", h) => Value::Bool(h.is_some()),
            (_, Some(i)) => Value::Int(i as i64),
            (_, None) => Value::Missing,
        }));
    }
    if !args.is_empty() {
        return Ok(None);
    }
    // Cheap length/positional methods read the packed buffer directly, so they don't
    // box every element into a `Value` the way `to_values()` would — e.g.
    // `range(1_000_000).first()` returns element 0 without materializing a million
    // Values. Byte-identical to the general path (`length`==`count`==len; `first`/`last`
    // are Missing on empty else the element).
    match name {
        "count" | "length" => {
            return Ok(match ad {
                ArrayData::Values(_) => None,
                ArrayData::Ints(xs) => Some(Value::Int(xs.len() as i64)),
                ArrayData::Floats(xs) => Some(Value::Int(xs.len() as i64)),
                // O(1) on the lazy representations — a lazy enumerate's count is its
                // inner length (previously it materialized every (index, element)
                // tuple: ~1 GB for `range(10000000).enumerate().count()`).
                ArrayData::Range { len, .. } => Some(Value::Int(*len as i64)),
                ArrayData::Enumerate { inner } => Some(Value::Int(inner.len() as i64)),
                // The FROZEN len (see the variant's invariant 1) — never a recomputed
                // `min(a.len(), b.len())`, which is exponential on `z = z.zip(z)`. This is
                // the line that turns `a.zip(a).length()` on 5M from 631 MB into ~16 MB.
                ArrayData::Zip { len, .. } => Some(Value::Int(*len as i64)),
            });
        }
        "first" | "last" => {
            let first = name == "first";
            return Ok(match ad {
                ArrayData::Values(_) => None,
                // O(1): one (index, element) tuple on demand; Missing on empty like
                // the general path.
                ArrayData::Enumerate { inner } => Some(if inner.is_empty() {
                    Value::Missing
                } else {
                    ad.get(if first { 0 } else { inner.len() - 1 })
                }),
                // READ THE STORED `len`, NOT `a.len()`. On `[1,2,3,4].zip([10,20])` those are
                // 4 and 2; indexing at 3 would read `b[3]` out of bounds and abort the
                // runtime — which ADR-0024 forbids outright.
                ArrayData::Zip { len, .. } => Some(if *len == 0 {
                    Value::Missing
                } else {
                    ad.get(if first { 0 } else { *len - 1 })
                }),
                ArrayData::Ints(xs) => Some(if xs.is_empty() {
                    Value::Missing
                } else {
                    Value::Int(if first { xs[0] } else { xs[xs.len() - 1] })
                }),
                ArrayData::Floats(xs) => Some(if xs.is_empty() {
                    Value::Missing
                } else {
                    Value::Float(if first { xs[0] } else { xs[xs.len() - 1] })
                }),
                // O(1) — `range(20M).first()` computes one element, no 160 MB materialization.
                ArrayData::Range { len, .. } if *len == 0 => Some(Value::Missing),
                ArrayData::Range { .. } => Some(ad.get(if first { 0 } else { ad.len() - 1 })),
            });
        }
        // `sort`/`reverse` on a packed array stayed packed nowhere: both built a
        // `Vec<Value>` of the whole source AND returned it through `Value::array`, which
        // (unlike `Value::array_sniff`) does not re-pack. So the cost was paid twice —
        // 320 MB of boxing to do the work, and a permanently boxed result that silently
        // stripped the fast path from everything downstream:
        //
        //     xs = (0..20000000).map(it * 2)
        //     xs.reverse().first()   797 MB  ->  346 MB
        //     xs.sort().first()      799 MB  ->  346 MB
        //
        // Sorting the packed buffer is also the exact comparison `numeric_cmp` performs:
        // two `Int`s compare as `i64` (deliberately NOT via `f64`, which collapses values
        // above 2^53), and anything else via `total_cmp`. `sort_unstable` is safe here
        // where `sort_by` was stable, because elements that compare equal under either
        // order are indistinguishable — equal `i64`s, and `total_cmp`-equal `f64`s are
        // bit-identical (`-0.0` and `0.0` are NOT equal under `total_cmp`, so even signed
        // zeros keep a deterministic position).
        //
        // `Values` defers (nothing to unbox) and so does `Enumerate`, whose elements are
        // tuples — `sort` rejects those, and that rejection is the general path's to make.
        "sort" | "reverse" => {
            let rev = name == "reverse";
            return Ok(match ad {
                ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => None,
                ArrayData::Ints(xs) => {
                    let mut v = xs.clone();
                    if rev {
                        v.reverse();
                    } else {
                        v.sort_unstable();
                    }
                    Some(Value::int_array(v))
                }
                ArrayData::Floats(xs) => {
                    let mut v = xs.clone();
                    if rev {
                        v.reverse();
                    } else {
                        v.sort_unstable_by(|a, b| crate::interp::ops::float_order(*a, *b));
                    }
                    Some(Value::float_array(v))
                }
                // Reversing an arithmetic progression is another progression, so this is
                // O(1) and stays lazy: `range(100000000).reverse().first()` allocates
                // nothing. The last element is in range by the Range invariant, so the
                // i128 start math fits i64.
                //
                // `wrapping_neg`, and it is exact rather than a shrug. The only step it
                // does not negate cleanly is `i64::MIN`, which it returns unchanged — and
                // that is the right step anyway, because `-2^63 == 2^63 (mod 2^64)`, so
                // adding it and subtracting it are the same operation in the i128-then-
                // truncate arithmetic `get` uses. Such a range also holds at most two
                // elements (`start + 2*i64::MIN` cannot fit), so only i=0 and i=1 are ever
                // evaluated, where the equivalence is exact.
                //
                // This started life as a `checked_neg` with a materializing fallback for
                // the overflow. Sabotaging it to `wrapping_neg` SURVIVED the test — and
                // the reason was not a weak test but the equivalence above: the fallback
                // could never produce a different answer. A guard whose removal breaks
                // nothing is decoration, so it is gone. The two-element `i64::MIN`-step
                // case is pinned in the test regardless, since that is the behaviour
                // being relied on here.
                //
                // SORTING one is O(1) for the same reason: a range is monotonic, so its
                // sorted form is either the range itself (ascending step) or exactly that
                // reverse (descending step). A zero step is rejected at construction
                // ("`range` step must not be zero"), so there is no third case. Writing it
                // this way also removed the last `unwrap` here — materializing the range
                // to sort it needed `to_ints().unwrap()`, which the ADR-0024 never-abort
                // ratchet rightly refused; the better code has nothing to unwrap.
                ArrayData::Range { start, step, len } if *len > 0 => Some(if rev || *step < 0 {
                    Value::lazy_range(
                        (*start as i128 + *step as i128 * (*len as i128 - 1)) as i64,
                        step.wrapping_neg(),
                        *len,
                    )
                } else {
                    Value::lazy_range(*start, *step, *len)
                }),
                // An empty range both sorts and reverses to itself.
                ArrayData::Range { .. } => Some(Value::lazy_range(0, 1, 0)),
            });
        }
        // `argsort` sorts INDICES by the values they point at, so the general path paid
        // twice: `to_values()` boxed the whole column (16 B/element), then every
        // comparison chased a `Value` enum through two random-access derefs. The packed
        // arm sorts the same indices against the raw buffer.
        //
        // STABILITY IS OBSERVABLE HERE, unlike `sort`: the output is indices, so equal
        // keys must keep their original order exactly as the general path's stable
        // `sort_by` does. `sort_unstable_by` + a `.then(a.cmp(&b))` index tie-break
        // reproduces a stable sort exactly (no two index pairs ever compare Equal).
        //
        // Floats compare by `total_cmp`, which is what `numeric_cmp` does for two floats
        // — so NaN (by sign bit) and signed zeros land exactly where the general path
        // puts them, and no NaN deferral is needed or wanted (argsort, like `sort`, has
        // NaN placement semantics rather than a `missing` answer).
        //
        // A RANGE needs no comparisons at all: it is strictly monotonic (zero step is
        // rejected at construction), so its argsort is the identity permutation for an
        // ascending step and the reversal for a descending one — both lazy, O(1).
        "argsort" => {
            return Ok(match ad {
                ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => None,
                ArrayData::Ints(xs) => {
                    let mut idx: Vec<i64> = (0..xs.len() as i64).collect();
                    idx.sort_unstable_by(|&a, &b| {
                        xs[a as usize].cmp(&xs[b as usize]).then(a.cmp(&b))
                    });
                    Some(Value::int_array(idx))
                }
                ArrayData::Floats(xs) => {
                    let mut idx: Vec<i64> = (0..xs.len() as i64).collect();
                    idx.sort_unstable_by(|&a, &b| {
                        crate::interp::ops::float_order(xs[a as usize], xs[b as usize])
                            .then(a.cmp(&b))
                    });
                    Some(Value::int_array(idx))
                }
                ArrayData::Range { step, len, .. } => Some(if *step > 0 || *len == 0 {
                    Value::lazy_range(0, 1, *len)
                } else {
                    Value::lazy_range(*len as i64 - 1, -1, *len)
                }),
            });
        }
        // `cumsum` already RETURNED a packed column; what it never had was a packed
        // INPUT, so the source was boxed into a `Vec<Value>` before it was even called:
        // `(0..20000000).map(it * 2).cumsum().last()` cost 645 MB against 186 MB for the
        // array alone. Same class as `sort`/`reverse`, opposite end of the pipe.
        //
        // The accumulation is deliberately identical to the general path rather than
        // better: `wrapping_add` for ints, and plain `+=` for floats — NOT the Neumaier
        // summation `sum`/`mean` use, which would give a different (more accurate) result
        // and break the two paths apart. `cumsum` also checks only for `missing`, never
        // NaN, so unlike the numeric reductions this must NOT defer on a NaN — a NaN
        // simply poisons the running total from that point on, exactly as before.
        "cumsum" => {
            return Ok(match ad {
                ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => None,
                // `to_ints` borrows for `Ints` and computes for `Range`; `None` cannot
                // happen for either, and deferring is the safe reading of it regardless.
                ArrayData::Ints(_) | ArrayData::Range { .. } => match ad.to_ints() {
                    Some(xs) => {
                        let mut acc = 0i64;
                        Some(Value::int_array(
                            xs.iter()
                                .map(|&i| {
                                    acc = acc.wrapping_add(i);
                                    acc
                                })
                                .collect(),
                        ))
                    }
                    None => None,
                },
                ArrayData::Floats(xs) => {
                    let mut acc = 0.0;
                    Some(Value::float_array(
                        xs.iter()
                            .map(|&x| {
                                acc += x;
                                acc
                            })
                            .collect(),
                    ))
                }
            });
        }
        _ => {}
    }
    if !matches!(
        name,
        "sum" | "mean" | "std" | "var" | "median" | "min" | "max"
    ) {
        return Ok(None);
    }
    match ad {
        ArrayData::Values(_) | ArrayData::Enumerate { .. } | ArrayData::Zip { .. } => Ok(None),
        ArrayData::Ints(xs) => array_int_reduce(xs, name, line, col).map(Some),
        // A reduction consumes every element, so materialize the range once (bit-identical to
        // reducing the equivalent `Int` array); still lazy for the O(1) methods above.
        ArrayData::Range { .. } => {
            array_int_reduce(&ad.to_ints().unwrap(), name, line, col).map(Some)
        }
        ArrayData::Floats(xs) => {
            // A `NaN` flips the answer to `missing` under ADR-0001; defer so the
            // general path matches the untyped result exactly.
            if xs.iter().any(|x| x.is_nan()) {
                Ok(None)
            } else {
                array_float_reduce(xs, name, line, col).map(Some)
            }
        }
    }
}

pub(crate) fn array_int_reduce(xs: &[i64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
        "count" => Ok(Value::Int(xs.len() as i64)),
        "sum" => {
            // i128 accumulate; stay exact `Int` if it fits, else compensated `Float`.
            let wide: i128 = xs.iter().map(|&n| n as i128).sum();
            Ok(match i64::try_from(wide) {
                Ok(n) => Value::Int(n),
                Err(_) => {
                    let fs: Vec<f64> = xs.iter().map(|&n| n as f64).collect();
                    Value::Float(neumaier_sum(&fs))
                }
            })
        }
        "min" | "max" => {
            if xs.is_empty() {
                empty_guard(&Vec::<f64>::new(), name, line, col)?;
            }
            let best = if name == "min" {
                *xs.iter().min().unwrap()
            } else {
                *xs.iter().max().unwrap()
            };
            Ok(Value::Int(best))
        }
        // mean/std/var/median: widen to f64 (still half a `Vec<Value>`).
        _ => {
            let fs: Vec<f64> = xs.iter().map(|&n| n as f64).collect();
            float_stat(&fs, name, line, col)
        }
    }
}

pub(crate) fn array_float_reduce(xs: &[f64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    match name {
        "count" => Ok(Value::Int(xs.len() as i64)),
        "sum" => Ok(Value::Float(neumaier_sum(xs))),
        "min" | "max" => {
            if xs.is_empty() {
                empty_guard(&Vec::<f64>::new(), name, line, col)?;
            }
            // `total_cmp`, NOT IEEE `<`/`>` — the same comparison the boxed path's
            // `numeric_cmp` makes for floats. Under IEEE, `-0.0` and `0.0` compare EQUAL,
            // so a first-wins scan returns whichever zero came first and the SAME array
            // answered differently depending on its representation:
            //
            //     [0.0, -0.0].min()          was  0.0   (packed: IEEE tie, first wins)
            //     [0.0, -0.0][0:2].min()          -0.0  (boxed: total_cmp, no tie)
            //     [0.0, -0.0].sort().first()      -0.0
            //
            // — and packed `min` was not even permutation-invariant ([-0.0, 0.0].min()
            // was -0.0). Under `total_cmp` the zeros are ORDERED (-0.0 < 0.0), so min is
            // -0.0 and max is 0.0 regardless of order and of representation, and
            // `min() == sort().first()` / `max() == sort().last()` hold everywhere. For
            // every pair of distinct non-zero values `total_cmp` agrees with IEEE `<`, so
            // nothing else moves. A NaN never reaches here — the caller defers any
            // NaN-containing array to the general path, which yields `missing` (ADR 0001).
            let mut best = xs[0];
            for &x in &xs[1..] {
                let ord = crate::interp::ops::float_order(x, best);
                if (name == "min" && ord == std::cmp::Ordering::Less)
                    || (name == "max" && ord == std::cmp::Ordering::Greater)
                {
                    best = x;
                }
            }
            Ok(Value::Float(best))
        }
        _ => float_stat(xs, name, line, col),
    }
}

/// Shared `f64` reductions (`mean`/`std`/`var`/`median`) — identical kernels to the
/// general `array_method` path, so a typed array's result matches the untyped one.
pub(crate) fn float_stat(xs: &[f64], name: &str, line: usize, col: usize) -> Result<Value, HelixError> {
    empty_guard(xs, name, line, col)?;
    Ok(match name {
        "mean" => Value::Float(neumaier_sum(xs) / xs.len() as f64),
        "std" => Value::Float(population_std(xs)),
        "var" => Value::Float(crate::stats::variance(xs)),
        "median" => Value::Float(crate::stats::median(xs)),
        _ => unreachable!("float_stat only handles mean/std/var/median"),
    })
}

pub(crate) fn array_method(
    items: &[Value],
    name: &str,
    args: &[Value],
    line: usize,
    col: usize,
) -> Result<Value, HelixError> {
    let no_args = |n: &str| {
        if args.is_empty() {
            Ok(())
        } else {
            Err(HelixError::new(
                format!("`{}` takes no arguments, got {}", n, args.len()),
                line,
                col,
            ))
        }
    };

    match name {
        "count" | "length" => {
            no_args(name)?;
            // Counts every slot, including `missing` holes. `length` is an alias so the
            // size of an Array/String/Dna is the same call everywhere.
            Ok(Value::Int(items.len() as i64))
        }
        // The first index whose element equals `args[0]` (structural equality), or
        // `missing` if none — pairs with `?? -1` / a `match`. Mirrors `Dna.find`.
        "index_of" => {
            arity("index_of", args, 1, line, col)?;
            match items.iter().position(|v| crate::interp::ops::values_equal(v, &args[0])) {
                Some(i) => Ok(Value::Int(i as i64)),
                None => Ok(Value::Missing),
            }
        }
        "mean" => {
            no_args(name)?;
            // TRACKED elements: fold-add on the tape, divide by the count — division
            // is differentiable, so `.mean()` carries gradients exactly as its
            // reduce-then-divide spelling does. Same ADR-0003 rule as `.sum()`: the
            // spellings of one concept must not fork by capability (the v0.2.5 field
            // re-verification found `.sum()` closed and this one still open).
            if items.iter().any(|v| matches!(v, Value::Node(_))) {
                if let Some(short) = tracked_fold_gate(items, "mean", line, col)? {
                    return Ok(short);
                }
                let mut it = items.iter();
                let mut acc = it.next().cloned().unwrap_or(Value::Int(0));
                for v in it {
                    acc = crate::autodiff::binary(&crate::ast::BinOp::Add, &acc, v, line, col)?;
                }
                return crate::autodiff::binary(
                    &crate::ast::BinOp::Div,
                    &acc,
                    &Value::Float(items.len() as f64),
                    line,
                    col,
                );
            }
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            let xs = numeric_vec(items, "mean", line, col)?;
            empty_guard(&xs, "mean", line, col)?;
            Ok(Value::Float(neumaier_sum(&xs) / xs.len() as f64))
        }
        "std" => {
            // Optional `ddof`: `std()` = population (÷n, default), `std(1)` = sample (÷n−1).
            let ddof = parse_ddof(name, args, line, col)?;
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            let xs = numeric_vec(items, "std", line, col)?;
            empty_guard(&xs, "std", line, col)?;
            ddof_fits(&xs, ddof, "std", line, col)?;
            // ddof == 0 keeps the exact existing population path (bit-identical to before).
            let v = if ddof == 0 {
                population_std(&xs)
            } else {
                crate::stats::variance_ddof(&xs, ddof).sqrt()
            };
            Ok(Value::Float(v))
        }
        "median" => {
            no_args(name)?;
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            let xs = numeric_vec(items, "median", line, col)?;
            empty_guard(&xs, "median", line, col)?;
            Ok(Value::Float(crate::stats::median(&xs)))
        }
        "var" => {
            // Optional `ddof`: `var()` = population (÷n, default), `var(1)` = sample (÷n−1).
            let ddof = parse_ddof(name, args, line, col)?;
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            let xs = numeric_vec(items, "var", line, col)?;
            empty_guard(&xs, "var", line, col)?;
            ddof_fits(&xs, ddof, "var", line, col)?;
            // ddof == 0 keeps the exact existing population path (bit-identical to before).
            let v = if ddof == 0 {
                crate::stats::variance(&xs)
            } else {
                crate::stats::variance_ddof(&xs, ddof)
            };
            Ok(Value::Float(v))
        }
        "quantile" => {
            // One argument: the probability `p` in [0, 1] (e.g. `xs.quantile(0.95)`).
            if args.len() != 1 {
                return Err(HelixError::new(
                    format!("`quantile` takes one probability in [0, 1], got {}", args.len()),
                    line,
                    col,
                )
                .hint("e.g. `xs.quantile(0.95)` for the 95th percentile."));
            }
            let p = match args[0].as_f64() {
                Some(p) => p,
                None => return Err(type_err("quantile", "a number in [0, 1]", &args[0], line, col)),
            };
            if !(0.0..=1.0).contains(&p) {
                return Err(HelixError::new(
                    format!("`quantile` needs a probability in [0, 1], got {}", p),
                    line,
                    col,
                )
                .hint("0 is the minimum, 0.5 the median, 1 the maximum."));
            }
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            let xs = numeric_vec(items, "quantile", line, col)?;
            empty_guard(&xs, "quantile", line, col)?;
            Ok(Value::Float(crate::stats::quantile(&xs, p)))
        }
        "summary" => {
            no_args(name)?;
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            let mut xs = numeric_vec(items, "summary", line, col)?;
            empty_guard(&xs, "summary", line, col)?;
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            // A descriptive overview (the `describe()` analogue): count, central
            // tendency, spread, and the three order-statistic extremes/center.
            let fields = vec![
                (Symbol::intern("count"), Value::Int(xs.len() as i64)),
                (Symbol::intern("mean"), Value::Float(crate::stats::mean(&xs))),
                (Symbol::intern("std"), Value::Float(crate::stats::std(&xs))),
                (Symbol::intern("min"), Value::Float(xs[0])),
                (Symbol::intern("median"), Value::Float(crate::stats::quantile_sorted(&xs, 0.5))),
                (Symbol::intern("max"), Value::Float(xs[xs.len() - 1])),
            ];
            Ok(Value::Record(Rc::new(fields)))
        }
        "sum" => {
            no_args(name)?;
            // TRACKED elements fold on the tape — left-to-right adds, exactly what the
            // reduce spelling produces — so `.sum()` and the fold carry gradients
            // alike. Before this, the two spellings of one concept silently forked by
            // CAPABILITY (ADR 0003's wound, found by the nn field report): the fold
            // differentiated while `.sum()` errored, forcing every dot product and
            // loss into hand-written folds.
            if items.iter().any(|v| matches!(v, Value::Node(_))) {
                if let Some(short) = tracked_fold_gate(items, "sum", line, col)? {
                    return Ok(short);
                }
                let mut it = items.iter();
                let mut acc = it.next().cloned().unwrap_or(Value::Int(0));
                for v in it {
                    acc = crate::autodiff::binary(&crate::ast::BinOp::Add, &acc, v, line, col)?;
                }
                return Ok(acc);
            }
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            // Keep Int if every element is an Int; otherwise compensated float sum.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                // Accumulate in i128 so a total that exceeds i64 neither panics
                // (debug) nor silently wraps (release): stay exact `Int` when it
                // fits, else promote to a compensated `Float` — mirroring `**`'s
                // Int→Float overflow promotion, so a large sum is never wrong.
                let wide: i128 = items
                    .iter()
                    .map(|v| if let Value::Int(i) = v { *i as i128 } else { 0 })
                    .sum();
                match i64::try_from(wide) {
                    Ok(n) => Ok(Value::Int(n)),
                    Err(_) => {
                        let xs: Vec<f64> = items
                            .iter()
                            .map(|v| if let Value::Int(i) = v { *i as f64 } else { 0.0 })
                            .collect();
                        Ok(Value::Float(neumaier_sum(&xs)))
                    }
                }
            } else {
                let xs = numeric_vec(items, "sum", line, col)?;
                Ok(Value::Float(neumaier_sum(&xs)))
            }
        }
        "min" | "max" => {
            no_args(name)?;
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            // A tracked element: fold with the differentiable max/min. The fold
            // plus the ties-to-first rule means the FIRST extreme element gets
            // the gradient — deterministic, and consistent with the scalar pair.
            if items.iter().any(|v| matches!(v, Value::Node(_))) {
                if let Some(short) = tracked_fold_gate(items, name, line, col)? {
                    return Ok(short);
                }
                let mut acc = items[0].clone();
                for v in &items[1..] {
                    acc = crate::autodiff::binary_builtin(name, &acc, v, line, col)?;
                }
                return Ok(acc);
            }
            // WIDENED TO `sort`'s DOMAIN — ADR 0025 (b), option b1: all numbers, all
            // strings, or all DNA, each ordered by the comparator `sort` uses for that
            // type. `min`/`max` were the one ordering spelling still numbers-only, so
            // `["b","a"].min()` errored while `["b","a"].min_by(it)` answered "a" and
            // `["b","a"].sort()` answered ["a","b"] — three spellings, two domains.
            //
            // The REDUCTION policy is unchanged and is the part that stays different from
            // `sort` on purpose: an array containing `missing` (or NaN) reduces to
            // `missing` (checked above, now covering the widened types too), where sorting
            // REFUSES — reducing and ordering-in-place are different questions (ADR 0001).
            // The empty array still errors, via `empty_guard` on the numeric branch —
            // `.all()` is vacuously true on empty, so empty arrays take that branch, as
            // before.
            //
            // The selector below is `sort().first()`/`sort().last()` in one pass:
            // first-wins on ties, which for numerics under `total_cmp` cannot differ from
            // `sort`'s stable order, and for Str/Dna ties means equal `Rc` contents.
            let pick = |cmp: &dyn Fn(&Value, &Value) -> std::cmp::Ordering| {
                let mut best_idx = 0;
                for i in 1..items.len() {
                    let ord = cmp(&items[i], &items[best_idx]);
                    let better = if name == "min" {
                        ord == std::cmp::Ordering::Less
                    } else {
                        ord == std::cmp::Ordering::Greater
                    };
                    if better {
                        best_idx = i;
                    }
                }
                items[best_idx].clone()
            };
            if items.iter().all(|v| v.as_f64().is_some()) {
                // Numeric (or empty). `numeric_cmp` compares the original `Value`s EXACTLY
                // — not their f64 widening, which would collapse two i64 above 2^53 to the
                // same value and pick the wrong element (and disagree with the packed Int
                // path). `numeric_vec` cannot fail here; it powers `empty_guard`.
                let xs = numeric_vec(items, name, line, col)?;
                empty_guard(&xs, name, line, col)?;
                Ok(pick(&|a, b| numeric_cmp(a, b)))
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                Ok(pick(&|a, b| match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                }))
            } else if items.iter().all(|v| matches!(v, Value::Dna(_))) {
                Ok(pick(&|a, b| match (a, b) {
                    (Value::Dna(x), Value::Dna(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                }))
            } else {
                // `sort`'s domain wording, with this method's name — one concept, one
                // message (the old text named numbers only and pointed at the first
                // offending element, which is now often a legal type in the wrong mix).
                Err(HelixError::new(
                    format!("`{name}` needs an array of all numbers, all strings, or all DNA"),
                    line,
                    col,
                ))
            }
        }
        "normalize" => {
            no_args(name)?;
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            let xs = numeric_vec(items, "normalize", line, col)?;
            empty_guard(&xs, "normalize", line, col)?;
            let mean = neumaier_sum(&xs) / xs.len() as f64;
            let sd = population_std(&xs);
            if sd == 0.0 {
                return Err(HelixError::new(
                    "cannot normalize: all values are identical (standard deviation is 0)",
                    line,
                    col,
                )
                .hint("normalize rescales by spread; a constant column has no spread."));
            }
            let out: Vec<Value> = xs.iter().map(|x| Value::Float((x - mean) / sd)).collect();
            Ok(Value::array(out))
        }
        "drop_missing" => {
            no_args(name)?;
            // Common case: nothing to drop → share the input array (an `Rc` bump,
            // zero allocation) instead of copying every element into a new `Vec`.
            if !items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::array(items.to_vec()));
            }
            let out: Vec<Value> = items
                .iter()
                .filter(|v| !matches!(v, Value::Missing))
                .cloned()
                .collect();
            Ok(Value::array(out))
        }
        "sort" => {
            no_args(name)?;
            let mut sorted: Vec<Value> = items.to_vec();
            // numeric sort if all numeric, else lexical if all strings
            if items.iter().all(|v| v.as_f64().is_some()) {
                // Exact compare (see `numeric_cmp`) so two i64 above 2^53 keep their
                // distinct order instead of collapsing through f64.
                sorted.sort_by(numeric_cmp);
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else if items.iter().all(|v| matches!(v, Value::Dna(_))) {
                sorted.sort_by(|a, b| match (a, b) {
                    (Value::Dna(x), Value::Dna(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else if items.iter().any(|v| matches!(v, Value::Missing)) {
                // Name the actual blocker: every present value may well be
                // sortable — it's the `missing` that has no order (ADR 0001
                // makes dropping them an explicit, visible step).
                return Err(HelixError::new(
                    "cannot sort: the array has missing values",
                    line,
                    col,
                )
                .hint("drop them explicitly first: `xs.drop_missing().sort()`."));
            } else {
                return Err(HelixError::new(
                    "`sort` needs an array of all numbers, all strings, or all DNA",
                    line,
                    col,
                ));
            }
            Ok(Value::array(sorted))
        }
        "join" => {
            arity("join", args, 1, line, col)?;
            let sep = str_arg(args, 0, "join", line, col)?;
            // Each element is rendered with its normal display (a string element is
            // its raw text, not a quoted form), then joined: `[1,2,3].join("-")`.
            let joined = items.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(sep);
            Ok(Value::Str(Rc::new(joined)))
        }
        "reverse" => {
            no_args(name)?;
            let mut v: Vec<Value> = items.to_vec();
            v.reverse();
            Ok(Value::array(v))
        }
        "first" | "last" => {
            no_args(name)?;
            // `missing` (not an error) on an empty array, so `xs.first() ?? default` and
            // `is_missing` give a safe first-or-default — missing propagates as elsewhere.
            if items.is_empty() {
                return Ok(Value::Missing);
            }
            let idx = if name == "first" { 0 } else { items.len() - 1 };
            Ok(items[idx].clone())
        }
        "take" => {
            arity("take", args, 1, line, col)?;
            let n = as_int(&args[0], "take", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().take(n).cloned().collect();
            Ok(Value::array(out))
        }
        "drop" => {
            arity("drop", args, 1, line, col)?;
            let n = as_int(&args[0], "drop", line, col)?.max(0) as usize;
            let out: Vec<Value> = items.iter().skip(n).cloned().collect();
            Ok(Value::array(out))
        }
        "zip" => {
            arity("zip", args, 1, line, col)?;
            let other = match &args[0] {
                Value::Array(a) => a.to_values().into_owned(),
                v => {
                    return Err(HelixError::new(
                        format!("`zip` needs an array, but got {}", crate::value::with_article(v.type_name())),
                        line,
                        col,
                    )
                    .hint("e.g. `xs.zip(ys)` pairs elements positionally."))
                }
            };
            let n = items.len().min(other.len());
            let out: Vec<Value> = (0..n)
                .map(|i| Value::Tuple(Rc::new(vec![items[i].clone(), other[i].clone()])))
                .collect();
            Ok(Value::array(out))
        }
        "enumerate" => {
            no_args(name)?;
            let out: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(i, v)| Value::Tuple(Rc::new(vec![Value::Int(i as i64), v.clone()])))
                .collect();
            Ok(Value::array(out))
        }
        "top" => {
            arity("top", args, 1, line, col)?;
            let n = as_int(&args[0], "top", line, col)?.max(0) as usize;
            let out: Vec<Value> = value_histogram(items)
                .into_iter()
                .take(n)
                .map(|(v, c)| Value::Tuple(Rc::new(vec![v, Value::Int(c)])))
                .collect();
            Ok(Value::array(out))
        }
        "frequencies" => {
            // The full value-count histogram as `(value, count)` pairs (count desc,
            // value asc) — `top` without the limit. For k-mer spectra etc.
            no_args(name)?;
            let out: Vec<Value> = value_histogram(items)
                .into_iter()
                .map(|(v, c)| Value::Tuple(Rc::new(vec![v, Value::Int(c)])))
                .collect();
            Ok(Value::array(out))
        }
        "to_dict" => {
            // Array of `(key, value)` pairs → a `Dict` (ADR 0020), for O(log n) lookup
            // instead of an O(n) scan. Later duplicate keys win (last-wins upsert), and
            // `xs.frequencies().to_dict()` turns a histogram into a count lookup.
            no_args(name)?;
            use crate::value::DictKey;
            let mut map = std::collections::BTreeMap::new();
            for (i, item) in items.iter().enumerate() {
                // A pair is a pair however it is written. `(k, v)` is the canonical
                // spelling, but a two-element ARRAY is what a table transcribed from
                // JSON, a CSV, or a reference document looks like, and refusing it
                // sent people to `reduce(dict(), (d, kv) => d.insert(kv[0], kv[1]))` —
                // a fold standing in for a literal, seventeen times in one corpus.
                // The ARITY still has to be two: a three-element row is a mistake, not
                // a pair, and silently taking its first two would be worse than saying so.
                let pair: Option<Vec<Value>> = match item {
                    Value::Tuple(t) if t.len() == 2 => Some(t.to_vec()),
                    Value::Array(a) if a.len() == 2 => Some(vec![a.get(0), a.get(1)]),
                    _ => None,
                };
                let pair = match pair {
                    Some(p) => p,
                    None => {
                        let what = match item {
                            Value::Tuple(t) => format!("a {}-element tuple", t.len()),
                            Value::Array(a) => format!("a {}-element array", a.len()),
                            other => crate::value::with_article(other.type_name()).to_string(),
                        };
                        return Err(HelixError::new(
                            format!("`to_dict` needs (key, value) pairs, but element {i} is {what}"),
                            line,
                            col,
                        )
                        .hint("each element must hold exactly two values — `[(\"a\", 1), (\"b\", 2)]` or `[[\"a\", 1], [\"b\", 2]]`."));
                    }
                };
                let key = DictKey::from_value(&pair[0]).map_err(|m| HelixError::new(m, line, col))?;
                map.insert(key, pair[1].clone());
            }
            Ok(Value::Dict(Rc::new(map)))
        }
        "unique" => {
            // Distinct values in first-seen order. Text and `Int`/`missing` arrays are
            // O(n) on the same keys `frequencies` uses — the two operations report the
            // same identities by construction, so `xs.unique().length()` can never
            // disagree with `xs.frequencies().length()`. Anything else keeps the O(n^2)
            // `values_equal` scan, which is the only thing that can express the
            // cross-type numeric collapse (`1 == 1.0`); see `IntKey`.
            no_args(name)?;
            let out: Vec<Value> = if items.iter().all(|v| matches!(v, Value::Str(_) | Value::Dna(_)))
            {
                // Borrowed keys: the old `HashSet<String>` minted a fresh `String` for
                // every element just to probe the set — 5M allocations to find 10k
                // distinct words.
                let mut seen: std::collections::HashSet<(bool, &str)> =
                    std::collections::HashSet::new();
                items.iter().filter(|v| seen.insert(text_key(v))).cloned().collect()
            } else if items.iter().all(|v| matches!(v, Value::Int(_) | Value::Missing)) {
                // `range(50_000).unique()` was ~1.25 billion `values_equal` comparisons.
                let mut seen: std::collections::HashSet<IntKey> = std::collections::HashSet::new();
                items.iter().filter(|v| seen.insert(int_key(v))).cloned().collect()
            } else if items.iter().all(|v| matches!(v, Value::Float(_) | Value::Missing)) {
                // Same key as `frequencies` uses, so the two cannot disagree on how many
                // identities a float array has. A NaN has NO key — it is equal to nothing,
                // not even itself — so every NaN survives `unique`, which is what the
                // `values_equal` scan did.
                let mut seen: std::collections::HashSet<FloatKey> =
                    std::collections::HashSet::new();
                items
                    .iter()
                    .filter(|v| float_key(v).is_none_or(|k| seen.insert(k)))
                    .cloned()
                    .collect()
            } else {
                let mut out: Vec<Value> = Vec::new();
                for v in items.iter() {
                    if !out.iter().any(|u| values_equal(u, v)) {
                        out.push(v.clone());
                    }
                }
                out
            };
            Ok(Value::array(out))
        }
        // `xs.concat(a, b, …)` — append the elements of each array argument. The
        // result is re-sniffed so a numeric concat stays a packed (fast) array.
        "concat" => {
            let mut out = items.to_vec();
            for (k, a) in args.iter().enumerate() {
                match a {
                    Value::Array(arr) => out.extend(arr.to_values().iter().cloned()),
                    other => {
                        return Err(HelixError::new(
                            format!(
                                "`concat` expects arrays, but argument {} is {}",
                                k + 1,
                                crate::value::with_article(other.type_name())
                            ),
                            line,
                            col,
                        ))
                    }
                }
            }
            Ok(Value::array_sniff(out))
        }
        // Every consecutive run of `n` elements, overlapping — the sliding window
        // signal processing and k-mer scanning both want. `Dna` has had this since
        // the bio work; an array had to hand-roll
        // `range(0, len - n + 1).map(i => xs.drop(i).take(n))`, which allocates two
        // intermediate arrays per window. Shorter than `n` yields `[]`, the same
        // answer `Dna.windows` gives, so the two read alike.
        "windows" | "chunks" => {
            if args.len() != 1 {
                return Err(HelixError::new(
                    format!("`{name}` expects 1 argument, got {}", args.len()),
                    line,
                    col,
                ));
            }
            let n = as_int(&args[0], name, line, col)?;
            if n <= 0 {
                return Err(HelixError::new(
                    format!("`{}` needs a positive size, got {}", name, n),
                    line,
                    col,
                )
                .hint("the window size counts elements, so it must be at least 1."));
            }
            let n = n as usize;
            let mut out: Vec<Value> = Vec::new();
            if name == "windows" {
                if n <= items.len() {
                    let count = items.len() - n + 1;
                    window_count_guard("windows", count, line, col)?;
                    out.reserve(count);
                    for w in items.windows(n) {
                        out.push(Value::array(w.to_vec()));
                    }
                }
            } else {
                // `chunks` partitions instead of sliding: no element appears twice, and
                // the last group is short when the length does not divide evenly —
                // dropping it would silently lose data, which is worse than a ragged
                // tail the caller can see and handle.
                window_count_guard("chunks", items.len().div_ceil(n), line, col)?;
                for c in items.chunks(n) {
                    out.push(Value::array(c.to_vec()));
                }
            }
            Ok(Value::array(out))
        }
        // `xss.flatten()` — one level: spread each array element, keep scalars. Turns
        // an array of arrays (e.g. dictionary column-groups) into one array.
        "flatten" => {
            if !args.is_empty() {
                return Err(HelixError::new("`flatten` takes no arguments", line, col));
            }
            // Concatenating packed columns needs no boxing at all. The general path below
            // boxes every inner element twice — once when `to_values()` materializes the
            // inner array, again into `out` — before `array_sniff` unpacks the lot again:
            // `[xs].flatten()` on a 20M-element xs cost 797 MB against 186 MB for the
            // array alone. Here the i64/f64 buffers are appended directly, and the result
            // is the same packed column `array_sniff` would have arrived at.
            //
            // Ints and Floats are kept as separate cases rather than one numeric case,
            // because a MIXED nesting (`[[1], [2.0]]`) must still reach `array_sniff`, and
            // `array_sniff` leaves that boxed — packing it to floats here would silently
            // turn an `Int` element into a `Float`.
            use crate::value::ArrayData;
            fn inner(v: &Value) -> Option<&ArrayData> {
                match v {
                    Value::Array(a) => Some(&**a),
                    _ => None,
                }
            }
            let all = |f: fn(&ArrayData) -> bool| {
                !items.is_empty() && items.iter().all(|v| inner(v).is_some_and(f))
            };
            let width = || items.iter().filter_map(inner).map(|a| a.len()).sum();
            if all(|a| matches!(a, ArrayData::Ints(_) | ArrayData::Range { .. })) {
                let mut out: Vec<i64> = Vec::with_capacity(width());
                for a in items.iter().filter_map(inner) {
                    if let Some(xs) = a.to_ints() {
                        out.extend_from_slice(&xs);
                    }
                }
                return Ok(Value::int_array(out));
            }
            if all(|a| matches!(a, ArrayData::Floats(_))) {
                let mut out: Vec<f64> = Vec::with_capacity(width());
                for a in items.iter().filter_map(inner) {
                    if let ArrayData::Floats(xs) = a {
                        out.extend_from_slice(xs);
                    }
                }
                return Ok(Value::float_array(out));
            }
            let mut out: Vec<Value> = Vec::with_capacity(items.len());
            for v in items {
                match v {
                    Value::Array(a) => out.extend(a.to_values().iter().cloned()),
                    other => out.push(other.clone()),
                }
            }
            Ok(Value::array_sniff(out))
        }
        // --- descriptive statistics over one numeric array (missing propagates) ---
        "standard_error" | "coefficient_of_variation" | "iqr" | "spread" | "zscores" => {
            // This family checked only `missing`, so a NaN fell through to the
            // computation — and `spread` folds with Rust's `f64::min`/`f64::max`,
            // which are IEEE-754-2008 `minNum`/`maxNum` and IGNORE a NaN operand by
            // design (both were REMOVED in 754-2019 for being non-associative). The
            // result was `[1.0, nan, 3.0].spread()` == `2.0`: not missing, not NaN,
            // but a plausible and confidently WRONG number, in the stats surface of a
            // language aimed at scientific work. It was the only reduction returning a
            // wrong value rather than a wrong kind of answer.
            if let Some(v) = degenerate_reduction(items) {
                return Ok(v);
            }
            let xs = numeric_vec(items, name, line, col)?;
            if xs.is_empty() {
                return Err(HelixError::new(
                    format!("cannot compute `{name}` of an empty array"),
                    line,
                    col,
                ));
            }
            match name {
                "standard_error" => {
                    Ok(Value::Float(crate::stats::std(&xs) / (xs.len() as f64).sqrt()))
                }
                "coefficient_of_variation" => {
                    let m = crate::stats::mean(&xs);
                    if m == 0.0 {
                        return Err(HelixError::new(
                            "coefficient of variation is undefined: the mean is zero",
                            line,
                            col,
                        ));
                    }
                    Ok(Value::Float(crate::stats::std(&xs) / m))
                }
                "iqr" => Ok(Value::Float(
                    crate::stats::quantile(&xs, 0.75) - crate::stats::quantile(&xs, 0.25),
                )),
                "spread" => {
                    let (mut lo, mut hi) = (xs[0], xs[0]);
                    for &x in &xs {
                        lo = lo.min(x);
                        hi = hi.max(x);
                    }
                    Ok(Value::Float(hi - lo))
                }
                _ => {
                    let (m, sd) = (crate::stats::mean(&xs), crate::stats::std(&xs));
                    if sd == 0.0 {
                        return Err(HelixError::new(
                            "cannot compute z-scores: the values have zero spread",
                            line,
                            col,
                        )
                        .hint("a constant series has no standard deviation to scale by."));
                    }
                    Ok(Value::array(xs.iter().map(|x| Value::Float((x - m) / sd)).collect()))
                }
            }
        }
        // --- sequence helpers over an array of DNA values (missing propagates) ---
        "mean_gc" | "total_length" => {
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let seqs: Vec<&Rc<String>> = items
                .iter()
                .map(|v| match v {
                    Value::Dna(s) => Ok(s),
                    other => Err(HelixError::new(
                        format!(
                            "`{name}` needs an array of DNA sequences, found {}",
                            crate::value::with_article(other.type_name())
                        ),
                        line,
                        col,
                    )),
                })
                .collect::<Result<_, _>>()?;
            if name == "total_length" {
                Ok(Value::Int(seqs.iter().map(|s| s.len() as i64).sum()))
            } else {
                if seqs.is_empty() {
                    return Err(HelixError::new("cannot compute `mean_gc` of no sequences", line, col));
                }
                // A sequence with no classifiable base has an unknown GC fraction, and an
                // unknown term makes the mean unknown — the same propagation the arm's
                // first line already applies to a `missing` element (ADR 0001).
                let mut total = 0.0;
                for s in &seqs {
                    match dna_gc(s, name, line, col)? {
                        Some(gc) => total += gc,
                        None => return Ok(Value::Missing),
                    }
                }
                Ok(Value::Float(total / seqs.len() as f64))
            }
        }
        // --- vector math over a numeric array (missing propagates) ---
        "dot" => {
            if args.len() != 1 {
                return Err(HelixError::new("`dot` takes one array argument", line, col));
            }
            let other = match &args[0] {
                Value::Array(a) => a.to_values(),
                Value::Missing => return Ok(Value::Missing),
                o => return Err(HelixError::new(
                    format!("`dot` expects an array, but got {}", crate::value::with_article(o.type_name())),
                    line,
                    col,
                )),
            };
            if items.iter().chain(other.iter()).any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            // Whether BOTH sides are `Int` is decided before anything is widened, but the
            // conversions and the length check still run first and unchanged, so every
            // error — non-numeric element, mismatched lengths — keeps its exact wording
            // and its exact precedence.
            let int_pair = items.iter().all(|v| matches!(v, Value::Int(_)))
                && other.iter().all(|v| matches!(v, Value::Int(_)));
            let (xs, ys) = (numeric_vec(items, "dot", line, col)?, numeric_vec(&other, "dot", line, col)?);
            if xs.len() != ys.len() {
                return Err(HelixError::new(
                    format!("`dot` needs equal-length arrays, got {} and {}", xs.len(), ys.len()),
                    line,
                    col,
                ));
            }
            // Preserve int-ness, the rule `sum` and `cumsum` already follow: an all-`Int`
            // dot product is an `Int`. Going through `f64` unconditionally made this the
            // only integer reduction that could return a WRONG answer — at n = 1e6,
            // `xs.dot(xs)` was 333332833333127552.0 where `xs.map(it * it).sum()` and
            // `xs.zip(xs).map((a, b) => a * b).sum()` both give the exact 333332833333500000.
            // Off by 372,448, silently, because f64 cannot hold integers past 2^53.
            //
            // `checked` throughout: a single i64*i64 product fits i128, but four of them
            // need not sum inside one, so overflow falls back to the same `f64` expression
            // as before — bit-identical to what this returned for such inputs.
            if int_pair {
                let wide = items.iter().zip(other.iter()).try_fold(0i128, |acc, (a, b)| {
                    match (a, b) {
                        (Value::Int(x), Value::Int(y)) => {
                            (*x as i128).checked_mul(*y as i128).and_then(|p| acc.checked_add(p))
                        }
                        // Unreachable under `int_pair`, and total either way.
                        _ => None,
                    }
                });
                if let Some(n) = wide.and_then(|w| i64::try_from(w).ok()) {
                    return Ok(Value::Int(n));
                }
            }
            Ok(Value::Float(xs.iter().zip(&ys).map(|(a, b)| a * b).sum()))
        }
        "norm" => {
            if !args.is_empty() {
                return Err(HelixError::new("`norm` takes no arguments", line, col));
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "norm", line, col)?;
            // fold seeded +0.0: Rust's empty f64 `sum()` is -0.0, and
            // sqrt(-0.0) is -0.0 — a negative empty-vector norm (sweep find).
            Ok(Value::Float(xs.iter().map(|x| x * x).fold(0.0f64, |s, x| s + x).sqrt()))
        }
        "cumsum" => {
            if !args.is_empty() {
                return Err(HelixError::new("`cumsum` takes no arguments", line, col));
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            // Preserve int-ness: an all-Int array cumsums to ints.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let mut acc = 0i64;
                let out: Vec<i64> = items
                    .iter()
                    .map(|v| {
                        if let Value::Int(i) = v {
                            acc = acc.wrapping_add(*i);
                        }
                        acc
                    })
                    .collect();
                Ok(Value::int_array(out))
            } else {
                let xs = numeric_vec(items, "cumsum", line, col)?;
                let mut acc = 0.0;
                Ok(Value::float_array(xs.iter().map(|x| { acc += x; acc }).collect()))
            }
        }
        "product" => {
            if !args.is_empty() {
                return Err(HelixError::new("`product` takes no arguments", line, col));
            }
            // TRACKED elements: fold-mul on the tape — same rule as `.sum()`/`.mean()`.
            // (`.max()`/`.min()` fold too, via the ties-to-first binary pair.)
            if items.iter().any(|v| matches!(v, Value::Node(_))) {
                if let Some(short) = tracked_fold_gate(items, "product", line, col)? {
                    return Ok(short);
                }
                let mut it = items.iter();
                let mut acc = it.next().cloned().unwrap_or(Value::Int(1));
                for v in it {
                    acc = crate::autodiff::binary(&crate::ast::BinOp::Mul, &acc, v, line, col)?;
                }
                return Ok(acc);
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let p = items.iter().fold(1i64, |acc, v| {
                    if let Value::Int(i) = v { acc.wrapping_mul(*i) } else { acc }
                });
                Ok(Value::Int(p))
            } else {
                let xs = numeric_vec(items, "product", line, col)?;
                Ok(Value::Float(xs.iter().product()))
            }
        }
        // --- ML helpers (missing propagates) ---
        "argsort" => {
            if !args.is_empty() {
                return Err(HelixError::new("`argsort` takes no arguments", line, col));
            }
            // ONE ORDER, ONE DOMAIN (ADR 0025, question (a), option a1). `argsort` used to
            // have its own policy — propagate `missing`, refuse `Dna` — while `sort` errored
            // on `missing` and accepted `Dna`. Two spellings of one concept disagreeing about
            // both edges is the tax a library author pays by being surprised, and `sort_by`
            // IS `argsort` (see `desugar_sort_by`), so `xs.sort()` and `xs.sort_by(it)` did
            // not even agree with each other. They do now.
            let mut idx: Vec<i64> = (0..items.len() as i64).collect();
            if items.iter().all(|v| v.as_f64().is_some()) {
                idx.sort_by(|&a, &b| numeric_cmp(&items[a as usize], &items[b as usize]));
            } else if items.iter().all(|v| matches!(v, Value::Str(_))) {
                idx.sort_by(|&a, &b| match (&items[a as usize], &items[b as usize]) {
                    (Value::Str(x), Value::Str(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else if items.iter().all(|v| matches!(v, Value::Dna(_))) {
                // `ops::compare` has always ordered `Dna`; `argsort` refusing it was the
                // outlier, and DNA ordering is a bio-first flagship's own use case.
                idx.sort_by(|&a, &b| match (&items[a as usize], &items[b as usize]) {
                    (Value::Dna(x), Value::Dna(y)) => x.cmp(y),
                    _ => std::cmp::Ordering::Equal,
                });
            } else if items.iter().any(|v| matches!(v, Value::Missing)) {
                // `sort`'s wording and hint verbatim — one concept, one message.
                return Err(HelixError::new(
                    "cannot sort: the array has missing values",
                    line,
                    col,
                )
                .hint("drop them explicitly first: `xs.drop_missing().sort()`."));
            } else {
                return Err(HelixError::new(
                    "`argsort` needs an array of all numbers, all strings, or all DNA",
                    line,
                    col,
                ));
            }
            Ok(Value::int_array(idx))
        }
        "clamp" => {
            if args.len() != 2 {
                return Err(HelixError::new("`clamp` takes (lo, hi)", line, col));
            }
            let lo = args[0].as_f64().ok_or_else(|| {
                HelixError::new("`clamp` lo must be a number", line, col)
            })?;
            let hi = args[1].as_f64().ok_or_else(|| {
                HelixError::new("`clamp` hi must be a number", line, col)
            })?;
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            // `lo > hi` is a CALLER ERROR, and it has to be caught here rather than left to
            // `Ord::clamp`/`f64::clamp`, both of which PANIC on it. `[1, 2, 3].clamp(5, 1)`
            // aborted the process with a core dump (exit 134) and `try` could not catch it —
            // an ADR-0024 violation, since user input must never take the host down. The
            // scalar `clamp(x, lo, hi)` builtin has always had this guard; the array method
            // did not, so the same mistake was catchable one way and fatal the other. Same
            // wording and hint as the scalar, so the two agree.
            if lo > hi {
                return Err(HelixError::new(
                    format!("`clamp` needs lo <= hi, got lo = {lo}, hi = {hi}"),
                    line,
                    col,
                )
                .hint("clamp(x, lo, hi) bounds x to [lo, hi]; pass the low bound before the high one."));
            }
            // Preserve int-ness when all elements are integral. Selection is written as
            // comparisons rather than `.clamp()` for the second reason that method is
            // unsafe here: it also panics when a bound is NaN, which `lo > hi` cannot
            // detect (every comparison against NaN is false). Comparisons are total — a NaN
            // bound simply matches nothing and the element passes through, exactly as the
            // scalar builtin behaves.
            if items.iter().all(|v| matches!(v, Value::Int(_))) {
                let (loi, hii) = (lo as i64, hi as i64);
                let out: Vec<i64> = items
                    .iter()
                    .map(|v| match v {
                        Value::Int(i) if *i < loi => loi,
                        Value::Int(i) if *i > hii => hii,
                        Value::Int(i) => *i,
                        _ => 0,
                    })
                    .collect();
                Ok(Value::int_array(out))
            } else {
                let xs = numeric_vec(items, "clamp", line, col)?;
                let out: Vec<f64> = xs
                    .iter()
                    .map(|x| if *x < lo { lo } else if *x > hi { hi } else { *x })
                    .collect();
                Ok(Value::float_array(out))
            }
        }
        "softmax" => {
            if !args.is_empty() {
                return Err(HelixError::new("`softmax` takes no arguments", line, col));
            }
            if items.iter().any(|v| matches!(v, Value::Missing)) {
                return Ok(Value::Missing);
            }
            let xs = numeric_vec(items, "softmax", line, col)?;
            if xs.is_empty() {
                return Ok(Value::float_array(Vec::new()));
            }
            // Subtract the max for numerical stability.
            let m = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let exps: Vec<f64> = xs.iter().map(|x| (x - m).exp()).collect();
            let total: f64 = exps.iter().sum();
            Ok(Value::float_array(exps.iter().map(|e| e / total).collect()))
        }
        "bootstrap" => crate::rng::bootstrap(items, args, line, col),
        "contains" => {
            if args.len() != 1 {
                return Err(HelixError::new("`contains` takes one value to look for", line, col));
            }
            Ok(Value::Bool(items.iter().any(|v| values_equal(v, &args[0]))))
        }
        // --- charts + tabular export/write → shared dispatch (rebuild the receiver) ---
        "bar_chart" | "histogram" | "line_chart" | "sparkline" | "scatter" | "svg_bar"
        | "svg_line" | "write_csv" | "write_tsv" | "write_json" | "to_html" | "to_markdown"
        | "to_table" | "write_fasta" | "write_fastq" => {
            export_method(Value::array(items.to_vec()), name, args, line, col)
        }
        // --- reproducible sampling (seeded) ---
        "shuffle" => crate::rng::shuffle(items, args, line, col),
        "sample" => crate::rng::sample(items, args, line, col),
        "choice" => crate::rng::choice(items, args, line, col),
        _ => Err(unknown_method(
            "Array",
            name,
            &crate::registry::methods_of(crate::registry::ARRAY_METHODS),
            line,
            col,
        )),
    }
}

/// The hash key of a text value, BORROWED from the array so probing the table mints
/// no `String`. The DNA-ness is part of the key because `values_equal` does **not**
/// equate `dna("AT")` with `"AT"`: it has a `(Str, Str)` arm and a `(Dna, Dna)` arm,
/// and the cross pair falls to `_ => false` — which is what `contains`/`index_of`
/// report. Keying on the bytes alone silently merged them, so `[dna("AT"), "AT"]`
/// had `unique().length() == 1` while `index_of("AT") == 1` said they were distinct.
pub(crate) fn text_key(v: &Value) -> (bool, &str) {
    match v {
        Value::Dna(s) => (true, s.as_str()),
        Value::Str(s) => (false, s.as_str()),
        _ => unreachable!("callers guard with an `all(Str | Dna)` kind check"),
    }
}

/// The hash key of an `Int`-or-`missing` array. `values_equal` treats all missings as
/// one identity and never equates a missing with an integer (ADR 0001), and integers
/// compare as exact `i64` — so these two variants reproduce its classes EXACTLY.
/// An array holding any `Float` or `Rational` may not use this key: `values_equal`
/// collapses `1 == 1.0` across types, and above 2^53 that collapse is not even
/// transitive (`9007199254740993` and `…92` are both equal to `9007199254740992.0`
/// but not to each other), so no hash key can reproduce it. Those keep the scan.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub(crate) enum IntKey {
    N(i64),
    Missing,
}

pub(crate) fn int_key(v: &Value) -> IntKey {
    match v {
        Value::Int(n) => IntKey::N(*n),
        Value::Missing => IntKey::Missing,
        _ => unreachable!("callers guard with an `all(Int | Missing)` kind check"),
    }
}

/// One pass, ONE hash probe per element, first-seen order preserved. `key` must
/// reproduce `values_equal`'s equivalence classes exactly over `items` — every
/// caller establishes that with an `all(...)` kind check before picking a key.
///
/// No `with_capacity(items.len())`: distinct keys are usually FAR fewer than elements
/// (a k-mer spectrum is the design centre), and reserving one bucket per element built
/// a 5M-bucket table — whose control bytes alone are memset — to hold 10k entries.
/// Growth is amortized O(1), and dropping the reserve measured faster in BOTH regimes:
/// 0.33s -> 0.068s at 10k-distinct/5M, and 1.83s -> 1.75s (-200 MB) even when every
/// element is distinct, which is the only case the reserve could have helped.
/// The hash key of a `Float`-or-`missing` array. Two wrinkles that the `Int` key does not
/// have, and both are why floats were left out of the first pass:
///
/// * `-0.0 == 0.0` is TRUE, but their bit patterns differ — so zero is canonicalized and
///   the first of the pair seen stays the representative, exactly as the scan would leave it.
/// * **NaN is not equal to itself**, so a NaN belongs to no equivalence class at all. It
///   gets `None`: no key, no table entry, a fresh bucket every time — which is precisely
///   what the `values_equal` scan produced, since `NaN == NaN` is false there too.
///
/// As with `IntKey`, an array holding BOTH `Int` and `Float` may not use this: `values_equal`
/// collapses `1 == 1.0`, and above 2^53 that collapse is not transitive.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub(crate) enum FloatKey {
    Bits(u64),
    Missing,
}

pub(crate) fn float_key(v: &Value) -> Option<FloatKey> {
    match v {
        Value::Float(f) if f.is_nan() => None,
        // `+0.0` and `-0.0` compare equal, so they must hash equal.
        Value::Float(f) => Some(FloatKey::Bits(if *f == 0.0 { 0.0f64 } else { *f }.to_bits())),
        Value::Missing => Some(FloatKey::Missing),
        _ => unreachable!("callers guard with an `all(Float | Missing)` kind check"),
    }
}

/// One pass, ONE hash probe per element, first-seen order preserved. `key` must reproduce
/// `values_equal`'s equivalence classes EXACTLY over `items` — every caller establishes that
/// with an `all(...)` kind check before picking a key. A `None` key means the value is equal
/// to NOTHING, not even another copy of itself (only NaN), so it takes a fresh bucket and
/// never enters the table.
///
/// No `with_capacity(items.len())`: distinct keys are usually FAR fewer than elements
/// (a k-mer spectrum is the design centre), and reserving one bucket per element built
/// a 5M-bucket table — whose control bytes alone are memset — to hold 10k entries.
/// Growth is amortized O(1), and dropping the reserve measured faster in BOTH regimes:
/// 0.33s -> 0.068s at 10k-distinct/5M, and 1.83s -> 1.75s (-200 MB) even when every
/// element is distinct, which is the only case the reserve could have helped.
pub(crate) fn tally<'a, K: Eq + std::hash::Hash>(
    items: &'a [Value],
    key: impl Fn(&'a Value) -> Option<K>,
) -> Vec<(Value, i64)> {
    use std::collections::hash_map::Entry;
    let mut idx: std::collections::HashMap<K, usize> = std::collections::HashMap::new();
    let mut counts: Vec<(Value, i64)> = Vec::new();
    for v in items.iter() {
        let next = counts.len();
        let Some(k) = key(v) else {
            counts.push((v.clone(), 1));
            continue;
        };
        match idx.entry(k) {
            Entry::Occupied(e) => counts[*e.get()].1 += 1,
            Entry::Vacant(e) => {
                e.insert(next);
                counts.push((v.clone(), 1));
            }
        }
    }
    counts
}

/// Value-count histogram, sorted by count desc then value asc — the shared core of
/// `top`/`frequencies`. Text arrays (k-mer spectra) and `Int`/`missing` arrays take a
/// ~O(n) hash path; everything else falls back to the value-equality scan, which
/// honors cross-type numeric equality (`1 == 1.0`) that no hash key can express.
/// Insertion order is preserved before the sort, matching the old `top`.
///
/// The `Int` path is not a micro-optimization: the scan is O(n × distinct), so a 5M
/// histogram over 10k distinct integers ran 2.5e10 `values_equal` calls — 41.7s, versus
/// 0.06s for the SAME histogram spelled with string keys. `unique` had had an all-Int
/// hash path for exactly this reason; `frequencies` never got one.
pub(crate) fn value_histogram(items: &[Value]) -> Vec<(Value, i64)> {
    let mut counts = if items.iter().all(|v| matches!(v, Value::Str(_) | Value::Dna(_))) {
        tally(items, |v| Some(text_key(v)))
    } else if items.iter().all(|v| matches!(v, Value::Int(_) | Value::Missing)) {
        tally(items, |v| Some(int_key(v)))
    } else if items.iter().all(|v| matches!(v, Value::Float(_) | Value::Missing)) {
        // Floats were left out of the first pass and it cost 220x on the element type
        // scientific data actually uses: `(0..60000).map(to_float(it)).unique()` took 3.2s,
        // while stringifying every float and hashing the TEXT took 0.04s — the same
        // O(n × distinct) scan the integer path was rescued from.
        tally(items, float_key)
    } else {
        let mut counts: Vec<(Value, i64)> = Vec::new();
        for v in items.iter() {
            if let Some(e) = counts.iter_mut().find(|(k, _)| values_equal(k, v)) {
                e.1 += 1;
            } else {
                counts.push((v.clone(), 1));
            }
        }
        counts
    };
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_string().cmp(&b.0.to_string())));
    counts
}

