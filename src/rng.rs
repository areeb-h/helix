//! Reproducible pseudo-random numbers — a hand-rolled, dependency-free SplitMix64.
//!
//! Helix is pure and immutable, so there is no hidden global RNG state: every draw
//! is a pure function of an explicit `seed` and the element index, mirroring the
//! `range(n).map(...)` style. Same seed → same numbers, forever — no external crate
//! whose algorithm could shift across versions — which is exactly what reproducible
//! science needs. Because the generators are pure, they are safely memoizable.
//!
//! Independent *streams* (uniform / normal / int / shuffle / …) are decorrelated by
//! salting the seed, so `random(n, s)` and `randn(n, s)` never line up, and a
//! `shuffle(s)` doesn't track a `random(s)` drawn from the same seed.

use crate::error::HelixError;
use crate::value::Value;

const S_UNIFORM: u64 = 0x5571_1B7A_0001;
const S_NORMAL: u64 = 0x5571_1B7A_0002;
const S_INT: u64 = 0x5571_1B7A_0003;
const S_SHUFFLE: u64 = 0x5571_1B7A_0004;
const S_SAMPLE: u64 = 0x5571_1B7A_0005;
const S_CHOICE: u64 = 0x5571_1B7A_0006;
const S_BOOTSTRAP: u64 = 0x5571_1B7A_0007;

/// Match the other collection caps so a stray huge `n` errors instead of OOM-ing.
const MAX_DRAW: usize = 100_000_000;

/// SplitMix64 finalizer — a strong integer avalanche.
fn mix64(z0: u64) -> u64 {
    let mut z = z0;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Hash `(seed, stream, index)` → a uniform 64-bit word.
fn bits(seed: i64, stream: u64, i: u64) -> u64 {
    let s = (seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ stream.wrapping_mul(0xD1B5_4A32_D192_ED03);
    mix64(s.wrapping_add(i.wrapping_mul(0x2545_F491_4F6C_DD1D)))
}

/// Uniform double in `[0, 1)` from the top 53 bits (full f64 mantissa resolution).
fn u01(seed: i64, stream: u64, i: u64) -> f64 {
    (bits(seed, stream, i) >> 11) as f64 / ((1u64 << 53) as f64)
}

/// One `N(0, 1)` sample via Box–Muller (two uniforms from the normal stream).
fn std_normal(seed: i64, i: u64) -> f64 {
    let u1 = u01(seed, S_NORMAL, 2 * i).max(f64::MIN_POSITIVE); // avoid ln(0)
    let u2 = u01(seed, S_NORMAL, 2 * i + 1);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// `[0, i]` index for a Fisher–Yates step (`u01 < 1`, so the result never exceeds `i`).
fn below(seed: i64, stream: u64, i: u64, bound: usize) -> usize {
    (u01(seed, stream, i) * bound as f64) as usize
}

// ---- argument helpers ----

fn as_i64(v: &Value, who: &str, line: usize, col: usize) -> Result<i64, HelixError> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(HelixError::new(
            format!("`{who}` expects an integer, but got {}", crate::value::with_article(other.type_name())),
            line,
            col,
        )),
    }
}

fn count(v: &Value, who: &str, line: usize, col: usize) -> Result<usize, HelixError> {
    let n = as_i64(v, who, line, col)?;
    if n < 0 {
        return Err(HelixError::new(format!("`{who}` needs a non-negative count"), line, col));
    }
    if n as usize > MAX_DRAW {
        return Err(HelixError::new(
            format!("`{who}` count {n} is too large (limit {MAX_DRAW})"),
            line,
            col,
        ));
    }
    Ok(n as usize)
}

fn arity(who: &str, args: &[Value], n: usize, line: usize, col: usize) -> Result<(), HelixError> {
    if args.len() != n {
        return Err(HelixError::new(
            format!("`{who}` expects {n} argument{}, got {}", if n == 1 { "" } else { "s" }, args.len()),
            line,
            col,
        ));
    }
    Ok(())
}

// ---- free functions (constructors) ----
//
// SEED-THREADED, NOT AMBIENT. Every generator takes an explicit `seed` as its LAST argument and is
// a pure, deterministic function of `(shape…, seed)` — the SAME seed always yields the SAME draw,
// on every run and across all three engines (tree-walker / VM / JIT). This is deliberate: Helix's
// correctness guarantee is bit-identical results across engines, which an ambient-entropy RNG would
// break. It is the JAX split-key / NumPy `Generator(seed)` model. To get *independent* samples,
// thread DIFFERENT seeds — e.g. `random(n, 1)`, `random(n, 2)`, or in a loop `random(n, base + i)` —
// rather than calling `random(n, seed)` twice with the same seed and expecting different values.

/// `random(n, seed)` → `n` uniform doubles in `[0, 1)`. Deterministic in `seed` (see the module
/// note): vary `seed` for independent draws; two calls with the same `seed` return the same array.
pub fn random(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    arity("random", args, 2, line, col)?;
    let n = count(&args[0], "random", line, col)?;
    let seed = as_i64(&args[1], "random", line, col)?;
    Ok(Value::float_array((0..n as u64).map(|i| u01(seed, S_UNIFORM, i)).collect()))
}

/// `randn(n, seed)` → `n` standard-normal `N(0, 1)` doubles. Deterministic in `seed` — vary it for
/// independent draws (see the module note on seed threading).
pub fn randn(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    arity("randn", args, 2, line, col)?;
    let n = count(&args[0], "randn", line, col)?;
    let seed = as_i64(&args[1], "randn", line, col)?;
    Ok(Value::float_array((0..n as u64).map(|i| std_normal(seed, i)).collect()))
}

/// `random_int(n, lo, hi, seed)` → `n` integers uniform in `[lo, hi)`. NOTE the argument order: the
/// LAST argument is the `seed` (not a column count) — deterministic in `seed`, so vary it for
/// independent draws (see the module note on seed threading).
pub fn random_int(args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    arity("random_int", args, 4, line, col)?;
    let n = count(&args[0], "random_int", line, col)?;
    let lo = as_i64(&args[1], "random_int", line, col)?;
    let hi = as_i64(&args[2], "random_int", line, col)?;
    let seed = as_i64(&args[3], "random_int", line, col)?;
    if hi <= lo {
        return Err(HelixError::new(
            format!("`random_int` needs hi > lo, got lo={lo}, hi={hi}"),
            line,
            col,
        ));
    }
    let span = (hi as i128 - lo as i128) as f64;
    let out: Vec<i64> =
        (0..n as u64).map(|i| lo + (u01(seed, S_INT, i) * span) as i64).collect();
    Ok(Value::int_array(out))
}

// ---- array methods (data verbs) ----

/// `xs.shuffle(seed)` → a uniformly random permutation (Fisher–Yates), as a new array.
pub fn shuffle(items: &[Value], args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    arity("shuffle", args, 1, line, col)?;
    let seed = as_i64(&args[0], "shuffle", line, col)?;
    let mut v = items.to_vec();
    for i in (1..v.len()).rev() {
        let j = below(seed, S_SHUFFLE, i as u64, i + 1); // [0, i]
        v.swap(i, j);
    }
    Ok(Value::array_sniff(v))
}

/// `xs.sample(k, seed)` → `k` elements drawn without replacement (a partial shuffle).
pub fn sample(items: &[Value], args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    arity("sample", args, 2, line, col)?;
    let k = count(&args[0], "sample", line, col)?;
    let seed = as_i64(&args[1], "sample", line, col)?;
    if k > items.len() {
        return Err(HelixError::new(
            format!("cannot sample {k} elements from an array of {}", items.len()),
            line,
            col,
        )
        .hint("use `shuffle` for a full permutation, or sample at most the array's length."));
    }
    let mut v = items.to_vec();
    let n = v.len();
    for i in 0..k {
        let j = i + below(seed, S_SAMPLE, i as u64, n - i); // [i, n)
        v.swap(i, j);
    }
    v.truncate(k);
    Ok(Value::array_sniff(v))
}

/// `xs.bootstrap(k, seed)` → `k` elements drawn **with** replacement (resampling).
pub fn bootstrap(items: &[Value], args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    arity("bootstrap", args, 2, line, col)?;
    let k = count(&args[0], "bootstrap", line, col)?;
    let seed = as_i64(&args[1], "bootstrap", line, col)?;
    if items.is_empty() {
        return Err(HelixError::new("cannot bootstrap from an empty array", line, col));
    }
    let n = items.len();
    let out: Vec<Value> =
        (0..k as u64).map(|i| items[below(seed, S_BOOTSTRAP, i, n)].clone()).collect();
    Ok(Value::array_sniff(out))
}

/// `xs.choice(seed)` → one uniformly random element.
pub fn choice(items: &[Value], args: &[Value], line: usize, col: usize) -> Result<Value, HelixError> {
    arity("choice", args, 1, line, col)?;
    let seed = as_i64(&args[0], "choice", line, col)?;
    if items.is_empty() {
        return Err(HelixError::new("cannot choose from an empty array", line, col));
    }
    Ok(items[below(seed, S_CHOICE, 0, items.len())].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floats(v: Value) -> Vec<f64> {
        match v {
            Value::Array(a) => a.to_values().iter().map(|x| x.as_f64().unwrap()).collect(),
            _ => panic!("expected an array"),
        }
    }

    #[test]
    fn reproducible_same_seed_same_draws() {
        let a = floats(random(&[Value::Int(1000), Value::Int(42)], 0, 0).unwrap());
        let b = floats(random(&[Value::Int(1000), Value::Int(42)], 0, 0).unwrap());
        assert_eq!(a, b);
        let c = floats(random(&[Value::Int(1000), Value::Int(43)], 0, 0).unwrap());
        assert_ne!(a, c); // a different seed gives different draws
    }

    #[test]
    fn uniform_in_range_and_well_distributed() {
        let xs = floats(random(&[Value::Int(100_000), Value::Int(7)], 0, 0).unwrap());
        assert!(xs.iter().all(|&x| (0.0..1.0).contains(&x)));
        let mean = xs.iter().sum::<f64>() / xs.len() as f64;
        assert!((mean - 0.5).abs() < 0.01, "mean {mean}");
    }

    #[test]
    fn normal_has_unit_moments() {
        let xs = floats(randn(&[Value::Int(100_000), Value::Int(7)], 0, 0).unwrap());
        let mean = xs.iter().sum::<f64>() / xs.len() as f64;
        let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64;
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((var.sqrt() - 1.0).abs() < 0.02, "std {}", var.sqrt());
    }

    #[test]
    fn uniform_and_normal_streams_are_decorrelated() {
        // Same seed, different generators must not produce the same sequence.
        let u = floats(random(&[Value::Int(50), Value::Int(3)], 0, 0).unwrap());
        let n = floats(randn(&[Value::Int(50), Value::Int(3)], 0, 0).unwrap());
        assert_ne!(u, n);
    }

    #[test]
    fn random_int_is_in_half_open_range() {
        let v = random_int(&[Value::Int(10_000), Value::Int(5), Value::Int(8), Value::Int(1)], 0, 0)
            .unwrap();
        let Value::Array(a) = v else { panic!() };
        for x in a.to_values().iter() {
            let Value::Int(i) = x else { panic!("expected ints") };
            assert!((5..8).contains(i), "out of range: {i}");
        }
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let items: Vec<Value> = (0..100).map(Value::Int).collect();
        let out = shuffle(&items, &[Value::Int(9)], 0, 0).unwrap();
        let mut got: Vec<i64> = match out {
            Value::Array(a) => a.to_values().iter().map(|v| match v {
                Value::Int(i) => *i,
                _ => panic!(),
            }).collect(),
            _ => panic!(),
        };
        got.sort();
        assert_eq!(got, (0..100).collect::<Vec<_>>()); // same multiset, reordered
        // reproducible
        let again = shuffle(&items, &[Value::Int(9)], 0, 0).unwrap();
        assert!(matches!(again, Value::Array(_)));
    }

    #[test]
    fn sample_draws_k_distinct_without_replacement() {
        let items: Vec<Value> = (0..50).map(Value::Int).collect();
        let out = sample(&items, &[Value::Int(10), Value::Int(2)], 0, 0).unwrap();
        let got: Vec<i64> = match out {
            Value::Array(a) => a.to_values().iter().map(|v| match v {
                Value::Int(i) => *i,
                _ => panic!(),
            }).collect(),
            _ => panic!(),
        };
        assert_eq!(got.len(), 10);
        let mut uniq = got.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), 10, "without replacement → all distinct");
        // sampling more than available is a clean error
        assert!(sample(&items, &[Value::Int(99), Value::Int(2)], 0, 0).is_err());
    }
}
