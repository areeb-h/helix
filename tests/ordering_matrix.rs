//! **The ordering matrix.** Every spelling of "put these in order" × every shape a
//! Helix array can have, pinned to the PRINTED value or the exact `error:` line, on
//! all three engines.
//!
//! This file asserts nothing about what ordering *should* do. It is the artifact
//! [ADR 0025](../docs/adr/0025-ordering.md) is decided against: a regression net that
//! fails loudly whichever direction the owner picks, so a change to one spelling
//! cannot silently leave the other three where they were.
//!
//! ## Why it exists
//!
//! Helix has one concept of order and **four** implementations of it:
//!
//! | domain | spellings | comparator | `missing` | `NaN` | empty |
//! |---|---|---|---|---|---|
//! | A. **sort** | `sort` | `numeric_cmp` (`total_cmp`) + Str + Dna | **error** | placed | `[]` |
//! | B. **argsort** | `argsort`, `sort_by` | *(same as A since ADR 0025 (a1))* | **error** | placed | `[]` |
//! | C. **reduction** | `min`, `max`, free `argmin`/`argmax` | `numeric_cmp` + Str + Dna since ADR 0025 (b1) (free `argmin`/`argmax` numbers only) | `missing` | `missing` | error |
//! | D. **`<`-reduce** | `min_by`, `max_by`, method `argmin`/`argmax` | `ops::compare` (IEEE, 3-valued, Tuple/Str/Dna) | `missing` since (c1) | `missing` since (c1) | error since (c1) |
//!
//! Four domains, four answers to the same question — WAS. All four ADR 0025 questions are
//! now taken:
//!
//! * **(a1)** merged A and B: `argsort` adopted `sort`'s policy (error on `missing`,
//!   accept `Dna`), and `sort_by` followed for free because `desugar_sort_by` rewrites it
//!   through `argsort` — before that, `xs.sort()` and `xs.sort_by(it)` did not agree with
//!   each other.
//! * **(b1)** widened `min`/`max` to `sort`'s type domain (numbers, strings, DNA).
//! * **(c1)** moved D onto C's POLICY: `missing`/NaN propagate, empty is the named domain
//!   error, and the method/free split on `argmin`/`argmax` is closed. The leaked internals
//!   (`if` condition is `missing`, `index 0 is out of bounds`, "cannot be indexed") are
//!   gone.
//! * **(d1)** documented IEEE first-wins on signed-zero ties
//!   (`examples/language/ordering.helix`). The COMPARATORS remain two (`total_cmp` for
//!   sorting and C, IEEE `<` for D's selection), which is why the signed-zero cells below
//!   still disagree — that residue is d2, and it needs comparator unification, not policy.
//!
//! ## Two rules this file follows
//!
//! 1. **Never assert with `==`.** `[0.0, -0.0].min_by(it) == [0.0, -0.0].min()` is
//!    `true`, because `0.0 == -0.0` — an equality-based test is structurally blind to
//!    the signed-zero half of the defect. Every cell compares the *rendered* text, in
//!    which `-0.0` and `0.0` are different strings.
//! 2. **Run every case on all three engines and require agreement first.** A value
//!    that is not the same on the tree-walker, the VM and the JIT is a divergence bug,
//!    not a design question. All 247 cells agree on all three engines, so everything
//!    below is a **spelling** inconsistency — one shared implementation
//!    (`src/interp/methods.rs`, no VM/JIT copy of `sort`/`argsort`) reached by four
//!    different front doors.

use std::process::{Command, Stdio};

/// Run one expression and render the result the way a user sees it: the printed value
/// on success, or the `error:` line on failure. The `-->` path line is deliberately
/// dropped — it holds the temp-file name, which is noise, not behaviour.
fn render(expr: &str, env: &[(&str, &str)], tag: &str) -> String {
    let src = format!("print({expr})\n");
    let path = std::env::temp_dir().join(format!("helix_ordering_{tag}.helix"));
    std::fs::write(&path, src).expect("write temp program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_helix"));
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to run helix");
    let _ = std::fs::remove_file(&path);
    if out.status.success() {
        return String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    stderr
        .lines()
        .find(|l| l.starts_with("error:"))
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| format!("<no error line, rc={:?}>", out.status.code()))
}

/// `(case, expression, rendered)` — the pinned matrix. Comments mark every cell where
/// two spellings of the same question disagree, naming which two and how.
#[rustfmt::skip]
const CASES: &[(&str, &str, &str)] = &[
    // ============================ Int — everything agrees ============================
    ("int/sort", "[3, 1, 2].sort()", "[1, 2, 3]"),
    ("int/argsort", "[3, 1, 2].argsort()", "[1, 2, 0]"),
    ("int/sort_by", "[3, 1, 2].sort_by(it)", "[1, 2, 3]"),
    ("int/min", "[3, 1, 2].min()", "1"),
    ("int/max", "[3, 1, 2].max()", "3"),
    ("int/min_by", "[3, 1, 2].min_by(it)", "1"),
    ("int/max_by", "[3, 1, 2].max_by(it)", "3"),
    ("int/argmin", "[3, 1, 2].argmin()", "1"),
    ("int/argmax", "[3, 1, 2].argmax()", "0"),
    ("int/fn_argmin", "argmin([3, 1, 2])", "1"),
    ("int/fn_argmax", "argmax([3, 1, 2])", "0"),
    // There is NO free array `min`/`max`: the free `min`/`max` are the two-argument
    // scalar functions, so `min(xs)` is an arity error, not a second reduction. Pinned
    // because option (b) in ADR 0025 must not accidentally invent one.
    ("int/fn_min", "min([3, 1, 2])", "error: `min` takes 2 arguments, got 1"),
    ("int/fn_max", "max([3, 1, 2])", "error: `max` takes 2 arguments, got 1"),

    // =========================== Float — everything agrees ===========================
    ("float/sort", "[3.0, 1.0, 2.0].sort()", "[1.0, 2.0, 3.0]"),
    ("float/argsort", "[3.0, 1.0, 2.0].argsort()", "[1, 2, 0]"),
    ("float/sort_by", "[3.0, 1.0, 2.0].sort_by(it)", "[1.0, 2.0, 3.0]"),
    ("float/min", "[3.0, 1.0, 2.0].min()", "1.0"),
    ("float/max", "[3.0, 1.0, 2.0].max()", "3.0"),
    ("float/min_by", "[3.0, 1.0, 2.0].min_by(it)", "1.0"),
    ("float/max_by", "[3.0, 1.0, 2.0].max_by(it)", "3.0"),
    ("float/argmin", "[3.0, 1.0, 2.0].argmin()", "1"),
    ("float/argmax", "[3.0, 1.0, 2.0].argmax()", "0"),
    ("float/fn_argmin", "argmin([3.0, 1.0, 2.0])", "1"),
    ("float/fn_argmax", "argmax([3.0, 1.0, 2.0])", "0"),
    ("float/fn_min", "min([3.0, 1.0, 2.0])", "error: `min` takes 2 arguments, got 1"),
    ("float/fn_max", "max([3.0, 1.0, 2.0])", "error: `max` takes 2 arguments, got 1"),

    // ================================ Float with NaN =================================
    // TWO answers to one question since ADR 0025 (c1), down from three: the sorting
    // spellings PLACE the NaN; every reduction spelling — `min`/`max`, `min_by`/`max_by`,
    // method AND free `argmin`/`argmax` — propagates `missing`.
    //
    // Note where the NaN lands: FIRST, not last. `sqrt(-1.0)` produces a NaN with its
    // sign bit SET, and `total_cmp` orders by the sign bit — so ADR 0024's prose
    // ("after `+inf`, numpy-style") does not describe what ships. numpy places every
    // NaN last regardless of sign; Helix places it by sign. Pinned as it behaves.
    ("float_nan/sort", "[1.0, sqrt(0.0 - 1.0), 3.0].sort()", "[NaN, 1.0, 3.0]"),
    ("float_nan/argsort", "[1.0, sqrt(0.0 - 1.0), 3.0].argsort()", "[1, 0, 2]"),
    ("float_nan/sort_by", "[1.0, sqrt(0.0 - 1.0), 3.0].sort_by(it)", "[NaN, 1.0, 3.0]"),
    ("float_nan/min", "[1.0, sqrt(0.0 - 1.0), 3.0].min()", "missing"),
    ("float_nan/max", "[1.0, sqrt(0.0 - 1.0), 3.0].max()", "missing"),
    ("float_nan/min_by", "[1.0, sqrt(0.0 - 1.0), 3.0].min_by(it)", "missing"),
    ("float_nan/max_by", "[1.0, sqrt(0.0 - 1.0), 3.0].max_by(it)", "missing"),
    ("float_nan/argmin", "[1.0, sqrt(0.0 - 1.0), 3.0].argmin()", "missing"),
    ("float_nan/argmax", "[1.0, sqrt(0.0 - 1.0), 3.0].argmax()", "missing"),
    ("float_nan/fn_argmin", "argmin([1.0, sqrt(0.0 - 1.0), 3.0])", "missing"),
    ("float_nan/fn_argmax", "argmax([1.0, sqrt(0.0 - 1.0), 3.0])", "missing"),
    ("float_nan/fn_min", "min([1.0, sqrt(0.0 - 1.0), 3.0])", "error: `min` takes 2 arguments, got 1"),
    ("float_nan/fn_max", "max([1.0, sqrt(0.0 - 1.0), 3.0])", "error: `max` takes 2 arguments, got 1"),

    // ============================== Float signed zeros ===============================
    // ADR 0025 question (d). `sort`/`argsort`/`min`/`max` use `total_cmp`, under which
    // `-0.0 < 0.0`. `min_by`/`max_by`/`argmin`/`argmax` desugar through IEEE `<`/`>`,
    // under which the two zeros are EQUAL — so first-wins returns element 0 whatever
    // it is, and MIN AND MAX RETURN THE SAME ELEMENT.
    ("float_signed_zero/sort", "[0.0, -0.0].sort()", "[-0.0, 0.0]"),
    ("float_signed_zero/argsort", "[0.0, -0.0].argsort()", "[1, 0]"),
    ("float_signed_zero/sort_by", "[0.0, -0.0].sort_by(it)", "[-0.0, 0.0]"),
    ("float_signed_zero/min", "[0.0, -0.0].min()", "-0.0"),
    ("float_signed_zero/max", "[0.0, -0.0].max()", "0.0"),
    // DISAGREE (min vs min_by): `min()` renders `-0.0`, `min_by(it)` renders `0.0`.
    // `==` cannot see this: `0.0 == -0.0` is `true`. Only the rendering shows it.
    ("float_signed_zero/min_by", "[0.0, -0.0].min_by(it)", "0.0"),
    // DISAGREE (min_by vs max_by): both return element 0. One array, one comparator,
    // and the smallest element IS the largest element.
    ("float_signed_zero/max_by", "[0.0, -0.0].max_by(it)", "0.0"),
    // DISAGREE (argsort vs argmin): argsort says index 1 is smallest; argmin says 0.
    ("float_signed_zero/argmin", "[0.0, -0.0].argmin()", "0"),
    ("float_signed_zero/argmax", "[0.0, -0.0].argmax()", "0"),
    ("float_signed_zero/fn_argmin", "argmin([0.0, -0.0])", "0"),
    ("float_signed_zero/fn_argmax", "argmax([0.0, -0.0])", "0"),
    ("float_signed_zero/fn_min", "min([0.0, -0.0])", "error: `min` takes 2 arguments, got 1"),
    ("float_signed_zero/fn_max", "max([0.0, -0.0])", "error: `max` takes 2 arguments, got 1"),

    // The same array reversed. `sort`/`min`/`max` are permutation-invariant (they give
    // the identical rendering); the `<`-reduce family is not — it just returns whatever
    // sat at index 0, so `max_by` now renders `-0.0` where `max()` renders `0.0`.
    ("float_signed_zero_rev/sort", "[-0.0, 0.0].sort()", "[-0.0, 0.0]"),
    ("float_signed_zero_rev/argsort", "[-0.0, 0.0].argsort()", "[0, 1]"),
    ("float_signed_zero_rev/sort_by", "[-0.0, 0.0].sort_by(it)", "[-0.0, 0.0]"),
    ("float_signed_zero_rev/min", "[-0.0, 0.0].min()", "-0.0"),
    ("float_signed_zero_rev/max", "[-0.0, 0.0].max()", "0.0"),
    ("float_signed_zero_rev/min_by", "[-0.0, 0.0].min_by(it)", "-0.0"),
    // DISAGREE (max vs max_by): `max()` renders `0.0`, `max_by(it)` renders `-0.0`.
    ("float_signed_zero_rev/max_by", "[-0.0, 0.0].max_by(it)", "-0.0"),
    // Both orderings of the pair give argmin 0 and argmax 0 — the documented
    // first-wins consequence of the IEEE kernel (`packed_arg_extreme`'s doc comment).
    ("float_signed_zero_rev/argmin", "[-0.0, 0.0].argmin()", "0"),
    ("float_signed_zero_rev/argmax", "[-0.0, 0.0].argmax()", "0"),
    ("float_signed_zero_rev/fn_argmin", "argmin([-0.0, 0.0])", "0"),
    ("float_signed_zero_rev/fn_argmax", "argmax([-0.0, 0.0])", "0"),
    ("float_signed_zero_rev/fn_min", "min([-0.0, 0.0])", "error: `min` takes 2 arguments, got 1"),
    ("float_signed_zero_rev/fn_max", "max([-0.0, 0.0])", "error: `max` takes 2 arguments, got 1"),

    // ==================================== String =====================================
    // ADR 0025 question (b), the headline: `sort` orders strings, `min` refuses them,
    // and `min_by(it)` — which is `min` with the identity key — answers.
    ("str/sort", "[\"b\", \"a\"].sort()", "[\"a\", \"b\"]"),
    ("str/argsort", "[\"b\", \"a\"].argsort()", "[1, 0]"),
    ("str/sort_by", "[\"b\", \"a\"].sort_by(it)", "[\"a\", \"b\"]"),
    // AGREE since ADR 0025 (b1): `min`/`max` order strings exactly as `sort` does.
    ("str/min", "[\"b\", \"a\"].min()", "a"),
    ("str/max", "[\"b\", \"a\"].max()", "b"),
    ("str/min_by", "[\"b\", \"a\"].min_by(it)", "a"),
    ("str/max_by", "[\"b\", \"a\"].max_by(it)", "b"),
    // DISAGREE (method argmin vs free argmin): the method orders strings, the free
    // function refuses them.
    ("str/argmin", "[\"b\", \"a\"].argmin()", "1"),
    ("str/argmax", "[\"b\", \"a\"].argmax()", "0"),
    ("str/fn_argmin", "argmin([\"b\", \"a\"])", "error: `argmin` expected an array of numbers, found a value of type String"),
    ("str/fn_argmax", "argmax([\"b\", \"a\"])", "error: `argmax` expected an array of numbers, found a value of type String"),
    ("str/fn_min", "min([\"b\", \"a\"])", "error: `min` takes 2 arguments, got 1"),
    ("str/fn_max", "max([\"b\", \"a\"])", "error: `max` takes 2 arguments, got 1"),

    // ====================================== Dna ======================================
    // ADR 0025 question (a), the type-domain half. `<` orders DNA and so does `sort`;
    // `argsort` does not, and `sort_by` inherits the refusal because it desugars to
    // `map(key).argsort().map(...)` (src/parser.rs, `desugar_sort_by`).
    ("dna/sort", "[dna(\"GG\"), dna(\"AA\")].sort()", "[AA, GG]"),
    // DISAGREE (sort vs argsort): `sort` accepts Dna, `argsort` rejects it.
    ("dna/argsort", "[dna(\"GG\"), dna(\"AA\")].argsort()", "[1, 0]"),
    // DISAGREE (sort vs sort_by): two spellings of one operation, one works. Note the
    // error names `argsort`, a method the user did not write — the desugar leaks.
    ("dna/sort_by", "[dna(\"GG\"), dna(\"AA\")].sort_by(it)", "[AA, GG]"),
    // DISAGREE (sort vs min): `sort` orders DNA; `min` calls it a non-number.
    ("dna/min", "[dna(\"GG\"), dna(\"AA\")].min()", "AA"),
    ("dna/max", "[dna(\"GG\"), dna(\"AA\")].max()", "GG"),
    ("dna/min_by", "[dna(\"GG\"), dna(\"AA\")].min_by(it)", "AA"),
    ("dna/max_by", "[dna(\"GG\"), dna(\"AA\")].max_by(it)", "GG"),
    // DISAGREE (method argmin vs free argmin) and (method argmin vs argsort): the
    // method orders DNA, both of the others refuse it.
    ("dna/argmin", "[dna(\"GG\"), dna(\"AA\")].argmin()", "1"),
    ("dna/argmax", "[dna(\"GG\"), dna(\"AA\")].argmax()", "0"),
    ("dna/fn_argmin", "argmin([dna(\"GG\"), dna(\"AA\")])", "error: `argmin` expected an array of numbers, found a value of type Dna"),
    ("dna/fn_argmax", "argmax([dna(\"GG\"), dna(\"AA\")])", "error: `argmax` expected an array of numbers, found a value of type Dna"),
    ("dna/fn_min", "min([dna(\"GG\"), dna(\"AA\")])", "error: `min` takes 2 arguments, got 1"),
    ("dna/fn_max", "max([dna(\"GG\"), dna(\"AA\")])", "error: `max` takes 2 arguments, got 1"),

    // ================================== Bool =========================================
    // Every spelling refuses — but with four different sentences, two of which name an
    // operator (`<`, `>`) the user never typed.
    ("bool/sort", "[true, false].sort()", "error: `sort` needs an array of all numbers, all strings, or all DNA"),
    ("bool/argsort", "[true, false].argsort()", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("bool/sort_by", "[true, false].sort_by(it)", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("bool/min", "[true, false].min()", "error: `min` needs an array of all numbers, all strings, or all DNA"),
    ("bool/max", "[true, false].max()", "error: `max` needs an array of all numbers, all strings, or all DNA"),
    ("bool/min_by", "[true, false].min_by(it)", "error: `<` cannot order a Bool — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("bool/max_by", "[true, false].max_by(it)", "error: `>` cannot order a Bool — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("bool/argmin", "[true, false].argmin()", "error: `<` cannot order a Bool — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("bool/argmax", "[true, false].argmax()", "error: `>` cannot order a Bool — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("bool/fn_argmin", "argmin([true, false])", "error: `argmin` expected an array of numbers, found a value of type Bool"),
    ("bool/fn_argmax", "argmax([true, false])", "error: `argmax` expected an array of numbers, found a value of type Bool"),
    ("bool/fn_min", "min([true, false])", "error: `min` takes 2 arguments, got 1"),
    ("bool/fn_max", "max([true, false])", "error: `max` takes 2 arguments, got 1"),

    // ============================ Mixed Int/Float — agrees ===========================
    ("mixed_int_float/sort", "[1, 2.5].sort()", "[1, 2.5]"),
    ("mixed_int_float/argsort", "[1, 2.5].argsort()", "[0, 1]"),
    ("mixed_int_float/sort_by", "[1, 2.5].sort_by(it)", "[1, 2.5]"),
    ("mixed_int_float/min", "[1, 2.5].min()", "1"),
    ("mixed_int_float/max", "[1, 2.5].max()", "2.5"),
    ("mixed_int_float/min_by", "[1, 2.5].min_by(it)", "1"),
    ("mixed_int_float/max_by", "[1, 2.5].max_by(it)", "2.5"),
    ("mixed_int_float/argmin", "[1, 2.5].argmin()", "0"),
    ("mixed_int_float/argmax", "[1, 2.5].argmax()", "1"),
    ("mixed_int_float/fn_argmin", "argmin([1, 2.5])", "0"),
    ("mixed_int_float/fn_argmax", "argmax([1, 2.5])", "1"),
    ("mixed_int_float/fn_min", "min([1, 2.5])", "error: `min` takes 2 arguments, got 1"),
    ("mixed_int_float/fn_max", "max([1, 2.5])", "error: `max` takes 2 arguments, got 1"),

    // ============================= Mixed Int/Str — refused ===========================
    // All four domains refuse, with four different sentences.
    ("mixed_int_str/sort", "[1, \"a\"].sort()", "error: `sort` needs an array of all numbers, all strings, or all DNA"),
    ("mixed_int_str/argsort", "[1, \"a\"].argsort()", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("mixed_int_str/sort_by", "[1, \"a\"].sort_by(it)", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("mixed_int_str/min", "[1, \"a\"].min()", "error: `min` needs an array of all numbers, all strings, or all DNA"),
    ("mixed_int_str/max", "[1, \"a\"].max()", "error: `max` needs an array of all numbers, all strings, or all DNA"),
    ("mixed_int_str/min_by", "[1, \"a\"].min_by(it)", "error: cannot order a String and an Int — `<` compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("mixed_int_str/max_by", "[1, \"a\"].max_by(it)", "error: cannot order a String and an Int — `>` compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("mixed_int_str/argmin", "[1, \"a\"].argmin()", "error: cannot order a String and an Int — `<` compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("mixed_int_str/argmax", "[1, \"a\"].argmax()", "error: cannot order a String and an Int — `>` compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("mixed_int_str/fn_argmin", "argmin([1, \"a\"])", "error: `argmin` expected an array of numbers, found a value of type String"),
    ("mixed_int_str/fn_argmax", "argmax([1, \"a\"])", "error: `argmax` expected an array of numbers, found a value of type String"),
    ("mixed_int_str/fn_min", "min([1, \"a\"])", "error: `min` takes 2 arguments, got 1"),
    ("mixed_int_str/fn_max", "max([1, \"a\"])", "error: `max` takes 2 arguments, got 1"),

    // ============================= Array containing missing ==========================
    // ADR 0025 question (a), the missing half — and the sharpest cell in the file.
    // THREE policies for one element: `sort` REFUSES (ADR 0001's "make dropping
    // visible"), `argsort`/`sort_by`/`min`/`max`/free-`argmin` PROPAGATE, and
    // `min_by`/method-`argmin` RAISE — with an error about an `if` the user never
    // wrote, leaked by `desugar_order_by`'s reduce.
    ("with_missing/sort", "[1, missing, 3].sort()", "error: cannot sort: the array has missing values"),
    ("with_missing/argsort", "[1, missing, 3].argsort()", "error: cannot sort: the array has missing values"),
    ("with_missing/sort_by", "[1, missing, 3].sort_by(it)", "error: cannot sort: the array has missing values"),
    ("with_missing/min", "[1, missing, 3].min()", "missing"),
    ("with_missing/max", "[1, missing, 3].max()", "missing"),
    // DISAGREE (min vs min_by): `min` propagates, `min_by(it)` raises.
    ("with_missing/min_by", "[1, missing, 3].min_by(it)", "missing"),
    ("with_missing/max_by", "[1, missing, 3].max_by(it)", "missing"),
    // DISAGREE (method argmin vs free argmin): raise vs propagate.
    ("with_missing/argmin", "[1, missing, 3].argmin()", "missing"),
    ("with_missing/argmax", "[1, missing, 3].argmax()", "missing"),
    ("with_missing/fn_argmin", "argmin([1, missing, 3])", "missing"),
    ("with_missing/fn_argmax", "argmax([1, missing, 3])", "missing"),
    ("with_missing/fn_min", "min([1, missing, 3])", "error: `min` takes 2 arguments, got 1"),
    ("with_missing/fn_max", "max([1, missing, 3])", "error: `max` takes 2 arguments, got 1"),

    // =================================== Empty array =================================
    // Three empties. `sort` family: `[]`. Reduction: a named domain error. `<`-reduce:
    // an INDEX error leaked from the reduce seed `$ob[0]` — the caret points at a
    // subscript the user did not write.
    ("empty/sort", "[].sort()", "[]"),
    ("empty/argsort", "[].argsort()", "[]"),
    ("empty/sort_by", "[].sort_by(it)", "[]"),
    ("empty/min", "[].min()", "error: cannot compute `min` of an empty array"),
    ("empty/max", "[].max()", "error: cannot compute `max` of an empty array"),
    // DISAGREE (min vs min_by): a domain error vs a leaked index error.
    ("empty/min_by", "[].min_by(it)", "error: `min_by` of an empty collection"),
    ("empty/max_by", "[].max_by(it)", "error: `max_by` of an empty collection"),
    ("empty/argmin", "[].argmin()", "error: `argmin` of an empty collection"),
    ("empty/argmax", "[].argmax()", "error: `argmax` of an empty collection"),
    // DISAGREE (method argmin vs free argmin): three empty-array messages for one
    // concept, across `min`, `argmin()` and `argmin(xs)`.
    ("empty/fn_argmin", "argmin([])", "error: `argmin` of an empty collection"),
    ("empty/fn_argmax", "argmax([])", "error: `argmax` of an empty collection"),
    ("empty/fn_min", "min([])", "error: `min` takes 2 arguments, got 1"),
    ("empty/fn_max", "max([])", "error: `max` takes 2 arguments, got 1"),

    // ================================ Missing receiver ===============================
    // `missing.sort()` is `missing` (ADR 0001 propagation through methods), but
    // `missing.argmin()` raises "cannot be indexed" — the reduce seed again.
    ("missing_receiver/sort", "missing.sort()", "missing"),
    ("missing_receiver/argsort", "missing.argsort()", "missing"),
    ("missing_receiver/sort_by", "missing.sort_by(it)", "missing"),
    ("missing_receiver/min", "missing.min()", "missing"),
    ("missing_receiver/max", "missing.max()", "missing"),
    // DISAGREE (min vs min_by): propagate vs raise, on the receiver itself.
    ("missing_receiver/min_by", "missing.min_by(it)", "missing"),
    ("missing_receiver/max_by", "missing.max_by(it)", "missing"),
    ("missing_receiver/argmin", "missing.argmin()", "missing"),
    ("missing_receiver/argmax", "missing.argmax()", "missing"),
    ("missing_receiver/fn_argmin", "argmin(missing)", "error: `argmin` expected an array or tensor of numbers, found a value of type Missing"),
    ("missing_receiver/fn_argmax", "argmax(missing)", "error: `argmax` expected an array or tensor of numbers, found a value of type Missing"),
    ("missing_receiver/fn_min", "min(missing)", "error: `min` takes 2 arguments, got 1"),
    ("missing_receiver/fn_max", "max(missing)", "error: `max` takes 2 arguments, got 1"),

    // ===================================== Tuples ====================================
    // `<` orders tuples lexicographically (src/interp/ops.rs, `compare`'s Tuple arm),
    // and the `<`-reduce family therefore does too — but `sort`, `argsort`, `sort_by`,
    // `min` and `max` all refuse. Same value, same order relation, five refusals and
    // four answers.
    ("tuple/sort", "[(2, 1), (1, 2)].sort()", "error: `sort` needs an array of all numbers, all strings, or all DNA"),
    ("tuple/argsort", "[(2, 1), (1, 2)].argsort()", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("tuple/sort_by", "[(2, 1), (1, 2)].sort_by(it)", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    // DISAGREE (`<` vs min): `(1, 2) < (2, 1)` is `true`, yet `min` refuses the array.
    ("tuple/min", "[(2, 1), (1, 2)].min()", "error: `min` needs an array of all numbers, all strings, or all DNA"),
    ("tuple/max", "[(2, 1), (1, 2)].max()", "error: `max` needs an array of all numbers, all strings, or all DNA"),
    // DISAGREE (min vs min_by): `min_by(it)` — `min` with the identity key — answers.
    ("tuple/min_by", "[(2, 1), (1, 2)].min_by(it)", "(1, 2)"),
    ("tuple/max_by", "[(2, 1), (1, 2)].max_by(it)", "(2, 1)"),
    ("tuple/argmin", "[(2, 1), (1, 2)].argmin()", "1"),
    ("tuple/argmax", "[(2, 1), (1, 2)].argmax()", "0"),
    ("tuple/fn_argmin", "argmin([(2, 1), (1, 2)])", "error: `argmin` expected an array of numbers, found a value of type Tuple"),
    ("tuple/fn_argmax", "argmax([(2, 1), (1, 2)])", "error: `argmax` expected an array of numbers, found a value of type Tuple"),
    ("tuple/fn_min", "min([(2, 1), (1, 2)])", "error: `min` takes 2 arguments, got 1"),
    ("tuple/fn_max", "max([(2, 1), (1, 2)])", "error: `max` takes 2 arguments, got 1"),

    // ==================================== Records ====================================
    // Records have no order in any domain — the one shape all four agree on refusing.
    ("record/sort", "[{a: 2}, {a: 1}].sort()", "error: `sort` needs an array of all numbers, all strings, or all DNA"),
    ("record/argsort", "[{a: 2}, {a: 1}].argsort()", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("record/sort_by", "[{a: 2}, {a: 1}].sort_by(it)", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("record/min", "[{a: 2}, {a: 1}].min()", "error: `min` needs an array of all numbers, all strings, or all DNA"),
    ("record/max", "[{a: 2}, {a: 1}].max()", "error: `max` needs an array of all numbers, all strings, or all DNA"),
    ("record/min_by", "[{a: 2}, {a: 1}].min_by(it)", "error: `<` cannot order a Record — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("record/max_by", "[{a: 2}, {a: 1}].max_by(it)", "error: `>` cannot order a Record — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("record/argmin", "[{a: 2}, {a: 1}].argmin()", "error: `<` cannot order a Record — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("record/argmax", "[{a: 2}, {a: 1}].argmax()", "error: `>` cannot order a Record — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("record/fn_argmin", "argmin([{a: 2}, {a: 1}])", "error: `argmin` expected an array of numbers, found a value of type Record"),
    ("record/fn_argmax", "argmax([{a: 2}, {a: 1}])", "error: `argmax` expected an array of numbers, found a value of type Record"),
    ("record/fn_min", "min([{a: 2}, {a: 1}])", "error: `min` takes 2 arguments, got 1"),
    ("record/fn_max", "max([{a: 2}, {a: 1}])", "error: `max` takes 2 arguments, got 1"),

    // ================================= Nested arrays =================================
    // Also refused everywhere — but note that unlike Tuple, `<` refuses Arrays too, so
    // the `<`-reduce family is CONSISTENT with `sort` here. It is the Tuple arm of
    // `compare` that creates the tuple asymmetry, not arrays.
    ("nested_array/sort", "[[2], [1]].sort()", "error: `sort` needs an array of all numbers, all strings, or all DNA"),
    ("nested_array/argsort", "[[2], [1]].argsort()", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("nested_array/sort_by", "[[2], [1]].sort_by(it)", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("nested_array/min", "[[2], [1]].min()", "error: `min` needs an array of all numbers, all strings, or all DNA"),
    ("nested_array/max", "[[2], [1]].max()", "error: `max` needs an array of all numbers, all strings, or all DNA"),
    ("nested_array/min_by", "[[2], [1]].min_by(it)", "error: `<` cannot order an Array — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("nested_array/max_by", "[[2], [1]].max_by(it)", "error: `>` cannot order an Array — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("nested_array/argmin", "[[2], [1]].argmin()", "error: `<` cannot order an Array — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("nested_array/argmax", "[[2], [1]].argmax()", "error: `>` cannot order an Array — it compares two numbers, two strings, two DNA sequences, or two tuples of those"),
    ("nested_array/fn_argmin", "argmin([[2], [1]])", "error: `argmin` expected an array of numbers, found a value of type Array"),
    ("nested_array/fn_argmax", "argmax([[2], [1]])", "error: `argmax` expected an array of numbers, found a value of type Array"),
    ("nested_array/fn_min", "min([[2], [1]])", "error: `min` takes 2 arguments, got 1"),
    ("nested_array/fn_max", "max([[2], [1]])", "error: `max` takes 2 arguments, got 1"),

    // ================================== Lazy: range ==================================
    // A range is Int-shaped and strictly monotonic, so all four domains agree.
    ("lazy_range/sort", "range(3).sort()", "[0, 1, 2]"),
    ("lazy_range/argsort", "range(3).argsort()", "[0, 1, 2]"),
    ("lazy_range/sort_by", "range(3).sort_by(it)", "[0, 1, 2]"),
    ("lazy_range/min", "range(3).min()", "0"),
    ("lazy_range/max", "range(3).max()", "2"),
    ("lazy_range/min_by", "range(3).min_by(it)", "0"),
    ("lazy_range/max_by", "range(3).max_by(it)", "2"),
    ("lazy_range/argmin", "range(3).argmin()", "0"),
    ("lazy_range/argmax", "range(3).argmax()", "2"),
    ("lazy_range/fn_argmin", "argmin(range(3))", "0"),
    ("lazy_range/fn_argmax", "argmax(range(3))", "2"),
    ("lazy_range/fn_min", "min(range(3))", "error: `min` takes 2 arguments, got 1"),
    ("lazy_range/fn_max", "max(range(3))", "error: `max` takes 2 arguments, got 1"),

    // ================================ Lazy: enumerate ================================
    // `enumerate` yields Tuples, so it inherits the tuple split exactly: the very verb
    // the language hands you for "index alongside value" cannot be sorted by `sort`,
    // only by `min_by`/`argmin`.
    ("lazy_enumerate/sort", "[10, 20].enumerate().sort()", "error: `sort` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_enumerate/argsort", "[10, 20].enumerate().argsort()", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_enumerate/sort_by", "[10, 20].enumerate().sort_by(it)", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_enumerate/min", "[10, 20].enumerate().min()", "error: `min` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_enumerate/max", "[10, 20].enumerate().max()", "error: `max` needs an array of all numbers, all strings, or all DNA"),
    // DISAGREE (min vs min_by), same as `tuple/`.
    ("lazy_enumerate/min_by", "[10, 20].enumerate().min_by(it)", "(0, 10)"),
    ("lazy_enumerate/max_by", "[10, 20].enumerate().max_by(it)", "(1, 20)"),
    ("lazy_enumerate/argmin", "[10, 20].enumerate().argmin()", "0"),
    ("lazy_enumerate/argmax", "[10, 20].enumerate().argmax()", "1"),
    ("lazy_enumerate/fn_argmin", "argmin([10, 20].enumerate())", "error: `argmin` expected an array of numbers, found a value of type Tuple"),
    ("lazy_enumerate/fn_argmax", "argmax([10, 20].enumerate())", "error: `argmax` expected an array of numbers, found a value of type Tuple"),
    ("lazy_enumerate/fn_min", "min([10, 20].enumerate())", "error: `min` takes 2 arguments, got 1"),
    ("lazy_enumerate/fn_max", "max([10, 20].enumerate())", "error: `max` takes 2 arguments, got 1"),

    // =================================== Lazy: zip ===================================
    ("lazy_zip/sort", "[1, 2].zip([3, 4]).sort()", "error: `sort` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_zip/argsort", "[1, 2].zip([3, 4]).argsort()", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_zip/sort_by", "[1, 2].zip([3, 4]).sort_by(it)", "error: `argsort` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_zip/min", "[1, 2].zip([3, 4]).min()", "error: `min` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_zip/max", "[1, 2].zip([3, 4]).max()", "error: `max` needs an array of all numbers, all strings, or all DNA"),
    ("lazy_zip/min_by", "[1, 2].zip([3, 4]).min_by(it)", "(1, 3)"),
    ("lazy_zip/max_by", "[1, 2].zip([3, 4]).max_by(it)", "(2, 4)"),
    ("lazy_zip/argmin", "[1, 2].zip([3, 4]).argmin()", "0"),
    ("lazy_zip/argmax", "[1, 2].zip([3, 4]).argmax()", "1"),
    ("lazy_zip/fn_argmin", "argmin([1, 2].zip([3, 4]))", "error: `argmin` expected an array of numbers, found a value of type Tuple"),
    ("lazy_zip/fn_argmax", "argmax([1, 2].zip([3, 4]))", "error: `argmax` expected an array of numbers, found a value of type Tuple"),
    ("lazy_zip/fn_min", "min([1, 2].zip([3, 4]))", "error: `min` takes 2 arguments, got 1"),
    ("lazy_zip/fn_max", "max([1, 2].zip([3, 4]))", "error: `max` takes 2 arguments, got 1"),
];

/// The three engines, by the environment switch that selects them.
const ENGINES: &[(&str, &[(&str, &str)])] = &[
    ("jit", &[]),
    ("vm", &[("HELIX_NOJIT", "1")]),
    ("tree-walker", &[("HELIX_NOVM", "1")]),
];

/// Every cell of the matrix, on every engine.
///
/// Two assertions per cell, in this order:
///
/// 1. **The oracle.** The tree-walker, the VM and the JIT must render the same text.
///    A failure here is an ENGINE DIVERGENCE — a correctness bug, not a design
///    question, and it outranks anything in ADR 0025.
/// 2. **The pin.** That text is what today's binary produces.
///
/// As of `e267a25`, assertion 1 holds for all 247 cells: `sort`, `argsort`, `min`,
/// `max` have exactly one implementation each (`src/interp/methods.rs`; neither
/// `src/vm.rs` nor the JIT carries a copy), and `sort_by`/`min_by`/`max_by`/`argmin`/
/// `argmax` are parse-time desugarings, so all three engines walk the same code. Every
/// disagreement this file records is therefore between two SPELLINGS, never between
/// two engines.
#[test]
fn ordering_matrix_is_pinned_and_identical_on_all_three_engines() {
    assert!(CASES.len() > 200, "the matrix shrank: {} cases", CASES.len());
    let mut checked = 0usize;
    for (case, expr, want) in CASES {
        let mut rendered: Vec<(&str, String)> = Vec::new();
        for (engine, env) in ENGINES {
            let tag = format!("{}_{}", case.replace('/', "_"), engine);
            rendered.push((engine, render(expr, env, &tag)));
        }
        // 1. THE ORACLE FIRST: a value that is not engine-identical means nothing.
        for w in rendered.windows(2) {
            assert_eq!(
                w[0].1, w[1].1,
                "ENGINE DIVERGENCE on `{case}` (`{expr}`): {} says {:?}, {} says {:?}",
                w[0].0, w[0].1, w[1].0, w[1].1
            );
        }
        // 2. THE PIN.
        assert_eq!(
            &rendered[0].1, want,
            "`{case}` (`{expr}`) changed\n  was: {want:?}\n  now: {:?}\n\
             If this change is intended, it is an ADR 0025 decision — update \
             docs/adr/0025-ordering.md in the same commit.",
            rendered[0].1
        );
        checked += 1;
    }
    assert_eq!(checked, CASES.len());
}

/// **Why this file never uses `==`.** The signed-zero half of the defect is invisible
/// to equality: `0.0 == -0.0` is `true`, so a test written as
/// `[0.0, -0.0].min_by(it) == [0.0, -0.0].min()` PASSES while the two spellings return
/// different elements of the array. This test asserts both halves — the equality that
/// hides the bug and the rendering that shows it — so nobody re-derives the blind
/// version later.
#[test]
fn equality_is_blind_to_the_signed_zero_disagreement() {
    for (engine, env) in ENGINES {
        // The blind assertion: equality says the two spellings agree.
        assert_eq!(
            render("[0.0, -0.0].min_by(it) == [0.0, -0.0].min()", env, &format!("sz_eq_{engine}")),
            "true",
            "engine {engine}"
        );
        assert_eq!(
            render("[-0.0, 0.0].max_by(it) == [-0.0, 0.0].max()", env, &format!("sz_eq2_{engine}")),
            "true",
            "engine {engine}"
        );
        // The sighted assertion: they return different elements.
        assert_eq!(render("[0.0, -0.0].min()", env, &format!("sz_a_{engine}")), "-0.0");
        assert_eq!(render("[0.0, -0.0].min_by(it)", env, &format!("sz_b_{engine}")), "0.0");
        assert_eq!(render("[-0.0, 0.0].max()", env, &format!("sz_c_{engine}")), "0.0");
        assert_eq!(render("[-0.0, 0.0].max_by(it)", env, &format!("sz_d_{engine}")), "-0.0");
        // And the sharpest form: on this array the smallest element and the largest
        // element are the SAME element, because IEEE `<` and `>` are both false for a
        // signed-zero pair and the reduce is first-wins.
        assert_eq!(render("[0.0, -0.0].min_by(it)", env, &format!("sz_e_{engine}")), "0.0");
        assert_eq!(render("[0.0, -0.0].max_by(it)", env, &format!("sz_f_{engine}")), "0.0");
    }
}

/// The four ADR 0025 questions, stated as executable one-liners. If a future commit
/// answers one of them, exactly the block below fails — which is the point: the
/// failure names the decision and sends the reader to the ADR.
#[test]
fn the_four_order_domains_disagree_today() {
    let env: &[(&str, &str)] = &[];

    // (a) ANSWERED — a1 taken: `argsort` adopted `sort`'s policy, so the two now agree on
    // both edges. `sort_by` followed for free, because `desugar_sort_by` rewrites it to
    // `map(key).argsort().map(...)` — which means `xs.sort()` and `xs.sort_by(it)` did not
    // agree with each other before this, and do now.
    assert_eq!(
        render("[1, missing, 3].sort()", env, "q_a1"),
        "error: cannot sort: the array has missing values"
    );
    assert_eq!(
        render("[1, missing, 3].argsort()", env, "q_a2"),
        "error: cannot sort: the array has missing values"
    );
    assert_eq!(render("[dna(\"GG\"), dna(\"AA\")].sort()", env, "q_a3"), "[AA, GG]");
    assert_eq!(render("[dna(\"GG\"), dna(\"AA\")].argsort()", env, "q_a4"), "[1, 0]");
    assert_eq!(
        render("[1, missing, 3].sort_by(it)", env, "q_a5"),
        "error: cannot sort: the array has missing values"
    );
    assert_eq!(render("[dna(\"GG\"), dna(\"AA\")].sort_by(it)", env, "q_a6"), "[AA, GG]");

    // (b) ANSWERED — b1 taken: `min`/`max` widened to `sort`'s domain (numbers, strings,
    // DNA), so the three spellings of "which is smallest" finally agree about WHAT can be
    // asked. Tuples remain outside every sort/reduction spelling — only `<` orders them —
    // which the ADR's own "tuple claim, verified" section already established.
    assert_eq!(render("[\"b\", \"a\"].min()", env, "q_b1"), "a");
    assert_eq!(render("[\"b\", \"a\"].min_by(it)", env, "q_b2"), "a");
    assert_eq!(render("[\"b\", \"a\"].sort()", env, "q_b3"), "[\"a\", \"b\"]");
    assert_eq!(render("(1, 2) < (2, 1)", env, "q_b4"), "true");
    assert_eq!(
        render("[(2, 1), (1, 2)].min()", env, "q_b5"),
        "error: `min` needs an array of all numbers, all strings, or all DNA"
    );
    assert_eq!(
        render("[(2, 1), (1, 2)].sort()", env, "q_b6"),
        "error: `sort` needs an array of all numbers, all strings, or all DNA"
    );
    assert_eq!(render("[(2, 1), (1, 2)].min_by(it)", env, "q_b7"), "(1, 2)");

    // (c) ANSWERED — c1 taken: the `_by` family and the method `argmin`/`argmax` adopted
    // the reduction policy. `missing`/NaN propagate, the empty array gets the free
    // function's own named error, and the METHOD vs FREE-FUNCTION split on the same name is
    // closed: `xs.argmin()` and `argmin(xs)` finally give one answer to one question.
    // The leaked internals (`if` condition, `index 0`, "cannot be indexed") are gone.
    assert_eq!(render("[1, missing, 3].min()", env, "q_c1"), "missing");
    assert_eq!(render("[1, missing, 3].min_by(it)", env, "q_c2"), "missing");
    assert_eq!(render("[1, missing, 3].argmin()", env, "q_c3"), "missing");
    assert_eq!(render("argmin([1, missing, 3])", env, "q_c4"), "missing");
    assert_eq!(render("[].argmin()", env, "q_c5"), "error: `argmin` of an empty collection");
    assert_eq!(render("argmin([])", env, "q_c6"), "error: `argmin` of an empty collection");
    assert_eq!(render("[].min()", env, "q_c7"), "error: cannot compute `min` of an empty array");

    // (d) IEEE first-wins on signed-zero ties: BOTH orderings of the pair answer 0,
    //     while `argsort` orders the zeros.
    assert_eq!(render("[0.0, -0.0].argmin()", env, "q_d1"), "0");
    assert_eq!(render("[-0.0, 0.0].argmin()", env, "q_d2"), "0");
    assert_eq!(render("[0.0, -0.0].argsort()", env, "q_d3"), "[1, 0]");
    assert_eq!(render("[-0.0, 0.0].argsort()", env, "q_d4"), "[0, 1]");
    // So `xs[xs.argmin()]` and `xs.min()` are different elements of `xs`.
    assert_eq!(render("[0.0, -0.0][[0.0, -0.0].argmin()]", env, "q_d5"), "0.0");
    assert_eq!(render("[0.0, -0.0].min()", env, "q_d6"), "-0.0");
}
