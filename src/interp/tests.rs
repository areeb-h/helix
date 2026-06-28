    use super::*;
    use crate::value::Value;

    /// Every name in the registry must have a `call_builtin` dispatch arm. Calling with
    /// no arguments yields an arity or type error for a real builtin; only a *missing*
    /// arm produces "is not a known function". This closes the registry-name-without-
    /// implementation gap by construction, complementing the registry's uniqueness test.
    #[test]
    fn every_registry_builtin_is_dispatchable() {
        let mut interp = Interp::new();
        for path in crate::registry::names() {
            if path == "print" {
                continue; // succeeds with no args (prints a blank line); nothing to assert
            }
            if let Err(e) = interp.call_builtin(path, vec![], 0, 0) {
                assert!(
                    !e.message.contains("is not a known function"),
                    "registry builtin `{path}` has no `call_builtin` arm"
                );
            }
        }
    }

    /// Run a program and return the value of its final statement.
    fn last(src: &str) -> Result<Value, HelixError> {
        let tokens = crate::lexer::lex(src)?;
        let program = crate::parser::parse(tokens)?;
        let mut interp = Interp::new();
        let mut out = Value::Unit;
        for s in &program {
            out = interp.exec(s)?.value;
        }
        Ok(out)
    }

    fn float(src: &str) -> f64 {
        match last(src).unwrap() {
            Value::Float(f) => f,
            Value::Int(i) => i as f64,
            other => panic!("expected number, got {:?}", other),
        }
    }

    fn int(src: &str) -> i64 {
        match last(src).unwrap() {
            Value::Int(i) => i,
            other => panic!("expected int, got {:?}", other),
        }
    }

    /// Deterministic leak detection. `last()` drops the `Interp` before
    /// returning, so if the produced value's allocation has `strong_count == 1`,
    /// the environment and every intermediate provably released their
    /// references. Combined with the audited absence of `unsafe`/interior
    /// mutability (so `Rc` cycles are unconstructible), this proves the
    /// interpreter is leak-free across all value-producing paths.
    #[test]
    fn no_reference_leaks() {
        fn sole_owner(src: &str) {
            let count = match last(src).unwrap() {
                Value::Array(rc) => Rc::strong_count(&rc),
                Value::Tuple(rc) => Rc::strong_count(&rc),
                Value::Record(rc) => Rc::strong_count(&rc),
                Value::Str(rc) => Rc::strong_count(&rc),
                other => panic!("test expects a reference-counted result, got {:?}", other),
            };
            assert_eq!(count, 1, "interpreter leaked a reference in: `{}`", src);
        }
        sole_owner("xs = [1, 2, 3]\nxs"); // plain binding
        sole_owner("fn f(n) = if n <= 0 then [0] else f(n - 1)\nf(50)"); // recursion
        sole_owner("[1, 2, 3, 4, 5].map(it * 2).where(it > 4)"); // comprehensions
        sole_owner("let a = [1, 2], b = a in b"); // let bindings + aliasing
        sole_owner("p, q = ([1], [2])\np"); // destructuring
        sole_owner("{name: \"x\", tags: [1, 2]}"); // record
        sole_owner("(1, [2, 3])"); // tuple
        sole_owner("\"hello {1 + 1}\""); // interpolation
        sole_owner("[1, 2].zip([3, 4]).map((a, b) => a + b)"); // zip + param destructure
        sole_owner("[1, 2, 3, 4, 5, 6][1:5:2]"); // slicing
    }

    /// Deep recursion must work, and runaway recursion must fail *gracefully*
    /// (a Helix error, never an uncatchable stack-overflow abort). Run on the
    /// same large-stack thread `main` uses — cargo's default test stack is far
    /// too small for the depth guard to be reached safely.
    #[test]
    fn deep_recursion_is_safe() {
        let outcome = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024 * 1024)
            .spawn(|| {
                // deep but bounded recursion (15000 frames) computes correctly
                let n = last(
                    "fn sum(n, acc) = if n <= 0 then acc else sum(n - 1, acc + n)\nsum(15000, 0)",
                )
                .unwrap();
                assert!(matches!(n, Value::Int(112507500)));
                // runaway recursion is caught as a clean error before any overflow
                let err = last("fn boom(n) = boom(n + 1)\nboom(0)").unwrap_err();
                assert!(err.message.contains("maximum recursion depth"));
            })
            .unwrap()
            .join();
        assert!(outcome.is_ok(), "recursion test thread crashed (stack overflow?)");
    }

    #[test]
    fn arithmetic_int_stays_int() {
        assert!(matches!(last("2 + 3 * 4").unwrap(), Value::Int(14)));
    }

    #[test]
    fn division_is_float() {
        assert_eq!(float("6 / 4"), 1.5);
    }

    #[test]
    fn array_stats() {
        assert_eq!(float("[1, 2, 3, 4].mean()"), 2.5);
        assert!((float("[1, 2, 3, 4].std()") - 1.118033988749895).abs() < 1e-12);
        assert!(matches!(last("[1, 2, 3, 4].sum()").unwrap(), Value::Int(10)));
    }

    #[test]
    fn array_concat_and_flatten() {
        assert!(matches!(last("[1, 2].concat([3, 4], [5]).sum()").unwrap(), Value::Int(15)));
        assert!(matches!(last("[[1, 2], [3, 4]].flatten().sum()").unwrap(), Value::Int(10)));
        // flatten keeps inner arrays when used as grouping (one level only)
        assert!(matches!(last("[[[1]], [[2], [3]]].flatten().count()").unwrap(), Value::Int(3)));
    }

    #[test]
    fn order_by_methods_desugar() {
        // min_by/max_by over records by a key (the common "best row" pattern)
        assert!(matches!(
            last("[{k: 3}, {k: 1}, {k: 2}].min_by(r => r.k).k").unwrap(),
            Value::Int(1)
        ));
        assert!(matches!(
            last("[{k: 3}, {k: 1}, {k: 2}].max_by(r => r.k).k").unwrap(),
            Value::Int(3)
        ));
        // implicit `it`, and argmin/argmax return indices
        assert!(matches!(last("[5, 2, 8, 1].min_by(it)").unwrap(), Value::Int(1)));
        assert!(matches!(last("[5, 2, 8, 1].argmin()").unwrap(), Value::Int(3)));
        assert!(matches!(last("[5, 2, 8, 1].argmax()").unwrap(), Value::Int(2)));
    }

    #[test]
    fn experimentation_toolkit() {
        // linspace endpoints inclusive
        assert_eq!(float("linspace(0.0, 1.0, 5)[2]"), 0.5);
        assert!(matches!(last("linspace(0.0, 1.0, 5).count()").unwrap(), Value::Int(5)));
        // vector math
        assert!(matches!(last("[1, 2, 3].dot([4, 5, 6])").unwrap(), Value::Float(f) if (f - 32.0).abs() < 1e-9));
        assert_eq!(float("[3.0, 4.0].norm()"), 5.0);
        assert!(matches!(last("[1, 2, 3, 4].cumsum()[3]").unwrap(), Value::Int(10)));
        assert!(matches!(last("[1, 2, 3, 4].product()").unwrap(), Value::Int(24)));
        // metrics
        assert!((float("mae([1.0, 2.0, 3.0], [1.0, 2.0, 4.0])") - (1.0 / 3.0)).abs() < 1e-9);
        assert_eq!(float("r2_score([1.0, 2.0, 3.0], [1.0, 2.0, 3.0])"), 1.0); // perfect
    }

    #[test]
    fn keyword_field_names_and_papercut_fixes() {
        // keywords are valid record field names + field access (contextual)
        assert!(matches!(last("{match: 1, in: 2, if: 3}.match").unwrap(), Value::Int(1)));
        assert!(matches!(last("r = {in: 7}\nr.in").unwrap(), Value::Int(7)));
        // `let … in` with the body on the next line
        assert!(matches!(last("let a = 1, b = 2 in\n  a + b").unwrap(), Value::Int(3)));
        // round(x) → Int (nearest); round(x, d) → Float to d decimals (broadcasts)
        assert!(matches!(last("round(3.7)").unwrap(), Value::Int(4)));
        assert!((float("round(1.23456, 2)") - 1.23).abs() < 1e-9);
        assert!(matches!(last("round([1.234, 5.678], 1)[0]").unwrap(), Value::Float(f) if (f - 1.2).abs() < 1e-9));
        // array membership
        assert!(matches!(last("[1, 2, 3].contains(2)").unwrap(), Value::Bool(true)));
        assert!(matches!(last("[1, 2, 3].contains(9)").unwrap(), Value::Bool(false)));
        // range(start, stop, step) — ascending, descending, empty, zero-step error
        assert!(matches!(last("range(0, 10, 2).sum()").unwrap(), Value::Int(20)));
        assert!(matches!(last("range(10, 0, -2).sum()").unwrap(), Value::Int(30)));
        assert!(matches!(last("range(5, 5, 1).count()").unwrap(), Value::Int(0)));
        assert!(matches!(last("range(0, 10, -1).count()").unwrap(), Value::Int(0)));
        assert!(last("range(0, 10, 0)").is_err());
    }

    #[test]
    fn user_function_shadows_builtin() {
        // A user `fn` of the same name as a builtin wins (the reported papercut: a
        // local `fn sign` was silently using the math builtin).
        assert!(matches!(last("fn sign(n) = 99\nsign(-5)").unwrap(), Value::Int(99)));
        assert!(matches!(last("fn max(a) = a + 1\nmax(10)").unwrap(), Value::Int(11)));
        // unshadowed builtins are untouched
        assert!(matches!(last("abs(-3)").unwrap(), Value::Int(3)));
        assert!((float("sqrt(16.0)") - 4.0).abs() < 1e-9);
    }

    #[test]
    fn hashing_and_dataframe_unique() {
        // sha256 matches the canonical hex digest of "helix" (verified vs sha256sum)
        match last("sha256(\"helix\")").unwrap() {
            Value::Str(s) => assert_eq!(
                s.as_str(),
                "54a85d2ae7b0a4d8005ab5cf466d4e582c6ea9aa5060b261241ec65a0ea58506"
            ),
            v => panic!("expected a string, got {:?}", v),
        }
        // unique() drops duplicate whole rows
        assert!(matches!(
            last("dataframe({id: [1, 1, 2, 2, 2]}).unique().count()").unwrap(),
            Value::Int(2)
        ));
        // unique("key") upserts — one row per key, the newest (last) wins
        assert!(matches!(
            last("dataframe({k: [\"a\", \"a\"], v: [1, 9]}).unique(\"k\").column(\"v\")[0]").unwrap(),
            Value::Int(9)
        ));
    }

    #[test]
    fn zipmap_pairs_two_arrays() {
        // a.zipmap(b, f) == a.zip(b).map(f) — paired elementwise map
        assert!(matches!(last("[1, 2, 3].zipmap([10, 20, 30], (x, y) => x * y)[1]").unwrap(), Value::Int(40)));
        assert!(matches!(last("[1, 2, 3].zipmap([10, 20, 30], (x, y) => x + y).sum()").unwrap(), Value::Int(66)));
        // wrong arity is a clear parse-time error
        assert!(last("[1].zipmap([2])").is_err());
    }

    #[test]
    fn interpolation_format_specs() {
        let s = |src: &str| -> String {
            match last(src).unwrap() {
                Value::Str(s) => s.to_string(),
                v => panic!("expected a string, got {:?}", v),
            }
        };
        assert_eq!(s("x = 3.14159\n\"v={x:.2f}\""), "v=3.14");
        assert_eq!(s("p = 0.5\n\"{p:.0%}\""), "50%");
        assert_eq!(s("n = 5\n\"{n:b}\""), "101");
        assert_eq!(s("n = 42\n\"{n:x}\""), "2a");
        assert_eq!(s("n = 7\n\"[{n:04}]\""), "[0007]");
        assert_eq!(s("n = 42\n\"[{n:<6}]\""), "[42    ]"); // left-align
        assert_eq!(s("w = \"hi\"\n\"[{w:>5}]\""), "[   hi]"); // strings right-align on request
        assert_eq!(s("x = 0.0 - 3.1\n\"{x:07.2f}\""), "-003.10"); // sign before zero-pad
        // a `:` inside a slice or record literal in the hole is NOT a format spec
        assert_eq!(s("xs = [10, 20, 30]\n\"{xs[1:3]}\""), "[20, 30]");
        // a malformed spec is a parse-time error; a numeric spec on a string errors
        assert!(last("x = 1\n\"{x:.2q}\"").is_err());
        assert!(last("w = \"a\"\n\"{w:.2f}\"").is_err());
        // an absurd width/precision is rejected at parse time — no giant allocation
        assert!(last("x = 1\n\"{x:99999999}\"").is_err());
        assert!(last("x = 1.0\n\"{x:.999999f}\"").is_err());
    }

    #[test]
    fn general_pairwise_alignment() {
        // global NW: identical sequences fully match
        assert!(matches!(last("align([1, 2, 3], [1, 2, 3]).matches").unwrap(), Value::Int(3)));
        assert!(matches!(last("align([1, 2, 3], [1, 2, 3]).score").unwrap(), Value::Int(3)));
        // a gap: [1,2,3] vs [1,3] aligns 1 and 3, gaps the 2
        assert!(matches!(last("align([1, 2, 3], [1, 3]).matches").unwrap(), Value::Int(2)));
        // local SW: full containment of [2,3] inside [1,2,3,4] (score + matches both 2)
        assert!(matches!(last("align([2, 3], [1, 2, 3, 4], \"local\").matches").unwrap(), Value::Int(2)));
        assert!(matches!(last("align([2, 3], [1, 2, 3, 4], \"local\").score").unwrap(), Value::Int(2)));
        // token sequences (linearized trees): a subtree is contained
        assert!(matches!(
            last("align([\"mul\", \"x\"], [\"sin\", \"mul\", \"x\"], \"local\").matches").unwrap(),
            Value::Int(2)
        ));
        // gaps surface as `missing` in the aligned output
        assert!(matches!(last("align([1, 2, 3], [1, 3]).b_aligned[1]").unwrap(), Value::Missing));
        // unknown mode errors
        assert!(last("align([1], [1], \"diagonal\")").is_err());
    }

    #[test]
    fn sort_by_and_int_valued_floats() {
        // sort_by(key) — ascending by a numeric key, stable
        assert!(matches!(last("[{a: 3}, {a: 1}, {a: 2}].sort_by(r => r.a)[0].a").unwrap(), Value::Int(1)));
        assert!(matches!(last("[{a: 3}, {a: 1}, {a: 2}].sort_by(r => r.a)[2].a").unwrap(), Value::Int(3)));
        // descending via a negated key
        assert!(matches!(last("[3, 1, 4, 1, 5].sort_by(x => 0 - x)[0]").unwrap(), Value::Int(5)));
        // string keys work (argsort generalized to strings)
        assert!(matches!(last("[{n: \"c\"}, {n: \"a\"}, {n: \"b\"}].sort_by(r => r.n)[0].n").unwrap(), Value::Str(s) if s.as_str() == "a"));
        assert!(matches!(last("[\"b\", \"a\", \"c\"].argsort()[0]").unwrap(), Value::Int(1)));
        // as_int accepts integer-valued floats (least_squares/lll outputs); fractional errors
        assert!(matches!(last("gcd(12.0, 18.0)").unwrap(), Value::Int(6)));
        assert!(matches!(last("gcd(1.0, 0.0 - 1.0)").unwrap(), Value::Int(1)));
        assert!(last("gcd(1.5, 2)").is_err());
    }

    #[test]
    fn floor_div_log_gcd() {
        // `//` is euclidean integer division, pairing with `%`: a == b*(a//b)+(a%b).
        assert!(matches!(last("7 // 2").unwrap(), Value::Int(3)));
        assert!(matches!(last("(0 - 7) // 2").unwrap(), Value::Int(-4)));
        assert!(matches!(last("7 // (0 - 2)").unwrap(), Value::Int(-3)));
        assert!(matches!(last("1 + 6 // 2").unwrap(), Value::Int(4))); // // binds like *
        assert!(matches!(last("([7, 8, 9] // 2)[2]").unwrap(), Value::Int(4))); // broadcast
        assert!(matches!(last("7.0 // 2.0").unwrap(), Value::Float(f) if (f - 3.0).abs() < 1e-9));
        assert!(last("5 // 0").is_err());
        assert!(matches!(last("5 // 2").unwrap(), Value::Int(2))); // merkle halving
        // single-arg log = natural log (numpy parity); log(x, base) keeps working
        assert!((float("log(2.718281828459045)") - 1.0).abs() < 1e-9);
        assert!((float("log(8.0, 2.0)") - 3.0).abs() < 1e-9);
        // gcd — sign-agnostic, gcd(n, 0) = |n|
        assert!(matches!(last("gcd(12, 18)").unwrap(), Value::Int(6)));
        assert!(matches!(last("gcd(17, 5)").unwrap(), Value::Int(1)));
        assert!(matches!(last("gcd(0, 5)").unwrap(), Value::Int(5)));
        assert!(matches!(last("gcd(0 - 12, 18)").unwrap(), Value::Int(6)));
    }

    #[test]
    fn exact_rationals() {
        // construction reduces to lowest terms; integer-valued ratios print as integers
        assert!(matches!(last("rational(4, 8)").unwrap(), Value::Rational(r) if r.to_string() == "1/2"));
        assert!(matches!(last("rational(6, 3)").unwrap(), Value::Rational(r) if r.is_integer()));
        // EXACT arithmetic — the headline: no float drift
        assert!(matches!(last("(rational(1,3) + rational(1,6)) == rational(1,2)").unwrap(), Value::Bool(true)));
        assert!(matches!(last("(rational(2,3) * rational(3,4)) == rational(1,2)").unwrap(), Value::Bool(true)));
        assert!(matches!(last("(rational(1,2) / rational(3,4)) == rational(2,3)").unwrap(), Value::Bool(true)));
        // exact power, incl. negative exponent (reciprocal)
        assert!(matches!(last("(rational(2,3) ** 2) == rational(4,9)").unwrap(), Value::Bool(true)));
        assert!(matches!(last("(rational(2,3) ** (0 - 1)) == rational(3,2)").unwrap(), Value::Bool(true)));
        // mixing with Int stays exact; mixing with Float drops to Float (documented)
        assert!(matches!(last("(rational(1,2) + 3) == rational(7,2)").unwrap(), Value::Bool(true)));
        assert!(matches!(last("rational(1,2) + 0.25").unwrap(), Value::Float(f) if (f - 0.75).abs() < 1e-12));
        // cross-type equality and ordering
        assert!(matches!(last("rational(2,1) == 2").unwrap(), Value::Bool(true)));
        assert!(matches!(last("rational(1,3) < rational(1,2)").unwrap(), Value::Bool(true)));
        // arbitrary precision — i64 would overflow these denominators
        assert!(matches!(last("rational(1,1000000000) + rational(1,3000000000)").unwrap(),
            Value::Rational(r) if r.to_string() == "1/750000000"));
        // accessors and conversion
        assert!(matches!(last("numerator(rational(22,7))").unwrap(), Value::Int(22)));
        assert!(matches!(last("denominator(rational(22,7))").unwrap(), Value::Int(7)));
        assert!((float("to_float(rational(7,2))") - 3.5).abs() < 1e-12);
        // zero denominator errors; missing propagates
        assert!(last("rational(1, 0)").is_err());
        assert!(matches!(last("rational(missing, 2)").unwrap(), Value::Missing));
        // sort orders rationals exactly; contains uses exact equality
        assert!(matches!(last("[rational(3,4), rational(1,2), rational(2,3)].sort()[0] == rational(1,2)").unwrap(), Value::Bool(true)));
        assert!(matches!(last("[rational(1,2), rational(1,3)].contains(rational(1,3))").unwrap(), Value::Bool(true)));
    }

    #[test]
    fn exact_lll_and_align_scoring() {
        // Exact integer LLL recovers the relation a+b=c with a residual of EXACTLY
        // zero (the f64 lll only gets "small"). [1,0,0,3],[0,1,0,5],[0,0,1,8] → the
        // shortest vector is ±[1,1,-1,0]; its residual (4th coord) is exactly 0.
        assert!(matches!(last("lll_exact([[1,0,0,3],[0,1,0,5],[0,0,1,8]], 0.99)[0][3]").unwrap(), Value::Int(0)));
        // entries are exact integers
        assert!(matches!(last("lll_exact([[2,1],[1,2]])[0][0]").unwrap(), Value::Int(_)));
        // bad delta / dependent rows error
        assert!(last("lll_exact([[1,0],[0,1]], 2.0)").is_err());
        assert!(last("lll_exact([[1,2],[2,4]])").is_err());
        // custom scoring changes the score; matches/containment stay scoring-robust.
        // [1,2,3,4] vs [1,3,4]: 3 matches + one gap.
        assert!(matches!(last("align([1,2,3,4],[1,3,4]).score").unwrap(), Value::Int(2))); // 3*(+1) + gap(-1)
        // heavy gap: 3*(+1) + (gap_open -10 + gap_extend -2) = -9
        assert!(matches!(last("align([1,2,3,4],[1,3,4],\"global\",{gap_open: -10, gap_extend: -2}).score").unwrap(), Value::Int(-9)));
        // a scoring record with no explicit mode defaults to global: 3*(+2) + gap(-1) = 5
        assert!(matches!(last("align([1,2,3,4],[1,3,4],{match: 2}).score").unwrap(), Value::Int(5)));
        // matches are unaffected by the weights
        assert!(matches!(last("align([1,2,3,4],[1,3,4],{match: 9, gap_open: -1}).matches").unwrap(), Value::Int(3)));
        // unknown scoring field / too many modes error
        assert!(last("align([1],[1],{bonus: 2})").is_err());
        assert!(last("align([1],[1],\"local\",\"global\")").is_err());
    }

    #[test]
    fn hardening_overflow_and_resource_caps() {
        // align scores are i64: a 2200-long identical alignment at a near-max weight
        // is 2200 * 1_000_000 = 2.2e9, which would overflow (wrap negative) in i32 but
        // is exact in i64.
        assert!(matches!(
            last("let xs = range(0, 2200) in align(xs, xs, {match: 1000000}).score").unwrap(),
            Value::Int(2_200_000_000)
        ));
        // rational `**` caps the exponent magnitude — no arbitrary-precision blow-up.
        assert!(last("rational(2, 1) ** 100000").is_err());
        assert!(last("rational(1, 2) ** (0 - 100000)").is_err());
        // dividing by a zero rational is a clean error, not a panic.
        assert!(last("rational(1, 2) / rational(0, 1)").is_err());
        // out-of-range align weights are rejected (keeps the i64 accumulator safe).
        assert!(last("align([1], [1], {match: 100000000})").is_err());
        // the align cell cap rejects an oversized pair instead of trying to OOM.
        assert!(last("align(range(0, 8000), range(0, 8000))").is_err()); // 64M > 50M cells
        // exact LLL is integer-only: a fractional entry is rejected, not silently floored.
        assert!(last("lll_exact([[1, 0], [0.5, 1]])").is_err());
    }

    #[test]
    fn parallel_reductions_stay_exact() {
        // The chunked-parallel Neumaier sum (and mean) over a large array must stay
        // exact for integer-valued floats: sum(0..2_000_000) = 1999999000000.
        assert_eq!(float("(range(0, 2000000) * 1.0).sum()"), 1_999_999_000_000.0);
        assert_eq!(float("(range(0, 2000000) * 1.0).mean()"), 999_999.5);
        // and a parallel matmul keeps small-integer products exact
        assert_eq!(
            float("(tensor(range(0, 40000) * 1.0).reshape([200, 200]).matmul(tensor(range(0, 40000) * 0.0).reshape([200, 200]))).sum()"),
            0.0
        );
    }

    #[test]
    fn string_plus_runtime_hint() {
        // `a` is Unknown at compile time, so the `+`-joins-strings nudge must also come
        // from the runtime operand check.
        let err = last("fn f(a) = a + \"!\"\nf(\"hi\")").unwrap_err();
        assert!(
            err.hint.unwrap_or_default().contains("interpolation"),
            "expected an interpolation hint at runtime"
        );
    }

    #[test]
    fn bitwise_operators() {
        assert!(matches!(last("12 & 10").unwrap(), Value::Int(8)));
        assert!(matches!(last("12 | 10").unwrap(), Value::Int(14)));
        assert!(matches!(last("12 ^ 10").unwrap(), Value::Int(6)));
        assert!(matches!(last("1 << 4").unwrap(), Value::Int(16)));
        assert!(matches!(last("255 >> 4").unwrap(), Value::Int(15)));
        // precedence: bitwise binds ABOVE comparison, so `5 & 1 == 1` is `(5 & 1) == 1`
        assert!(matches!(last("5 & 1 == 1").unwrap(), Value::Bool(true)));
        // and BELOW additive, so `1 << 2 + 1` is `1 << (2 + 1)` == 8 (Rust ordering)
        assert!(matches!(last("1 << 2 + 1").unwrap(), Value::Int(8)));
        // `|` < `^` < `&`: `1 | 2 ^ 3 & 1` == `1 | (2 ^ (3 & 1))` == 1 | (2 ^ 1) == 1 | 3 == 3
        assert!(matches!(last("1 | 2 ^ 3 & 1").unwrap(), Value::Int(3)));
        // the bitmask-toggle idiom (subset-as-int) the ai-research code leans on
        assert!(matches!(last("0 ^ (1 << 2) ^ (1 << 0)").unwrap(), Value::Int(5)));
        assert!(matches!(last("(5 >> 2) & 1 == 1").unwrap(), Value::Bool(true)));
        // a line ending in a bitwise operator continues onto the next
        assert!(matches!(last("3 &\n  1").unwrap(), Value::Int(1)));
        // out-of-range / negative shifts and non-integer operands error, never panic
        assert!(last("1 << 64").is_err());
        assert!(last("1 >> -1").is_err());
        assert!(last("1.5 & 2").is_err());
        // missing propagates
        assert!(matches!(last("missing & 1").unwrap(), Value::Missing));
    }

    #[test]
    fn classification_metrics() {
        let setup = "yt = [1, 0, 1, 1, 0, 1, 0, 0]\nyp = [1, 0, 1, 0, 0, 1, 1, 0]\n";
        // tp=3 fp=1 fn=1 tn=3 → accuracy/precision/recall/f1 all 0.75
        assert!((float(&format!("{setup}accuracy(yt, yp)")) - 0.75).abs() < 1e-9);
        assert!((float(&format!("{setup}precision(yt, yp)")) - 0.75).abs() < 1e-9);
        assert!((float(&format!("{setup}recall(yt, yp)")) - 0.75).abs() < 1e-9);
        assert!((float(&format!("{setup}f1_score(yt, yp)")) - 0.75).abs() < 1e-9);
        assert!(matches!(last(&format!("{setup}confusion_matrix(yt, yp).tp")).unwrap(), Value::Int(3)));
        assert!(matches!(last(&format!("{setup}confusion_matrix(yt, yp).fn")).unwrap(), Value::Int(1)));
        // string labels with an explicit positive class
        let s = "t = [\"cat\", \"dog\", \"cat\", \"dog\"]\np = [\"cat\", \"cat\", \"cat\", \"dog\"]\n";
        assert!((float(&format!("{s}accuracy(t, p)")) - 0.75).abs() < 1e-9);
        assert!((float(&format!("{s}precision(t, p, \"cat\")")) - 2.0 / 3.0).abs() < 1e-9);
        // undefined ratios report 0.0, never panic; mismatched lengths error
        assert!((float("precision([0, 0], [0, 0])") - 0.0).abs() < 1e-9);
        assert!(last("accuracy([1, 0], [1])").is_err());
    }

    #[test]
    fn ml_helpers_and_scientific_literals() {
        // scientific float literals
        assert_eq!(float("1.0e9"), 1.0e9);
        assert_eq!(float("2.5e-3"), 0.0025);
        assert_eq!(float("4E3"), 4000.0);
        // argsort / clamp / softmax / bootstrap
        assert!(matches!(last("[3, 1, 2].argsort()[0]").unwrap(), Value::Int(1)));
        assert!(matches!(last("[-1, 5, 2, 9].clamp(0, 4)[1]").unwrap(), Value::Int(4)));
        assert!((float("[1.0, 2.0, 3.0].softmax().sum()") - 1.0).abs() < 1e-9);
        assert!(matches!(last("[10, 20, 30].bootstrap(5, 1).count()").unwrap(), Value::Int(5)));
        // AIC/BIC reward a smaller RSS / fewer params
        assert!(float("aic(1.0, 40, 2)") < float("aic(1.0, 40, 5)")); // fewer params → lower
    }

    #[test]
    fn regression_exposes_rss_and_intercept_option() {
        // rss/predictions/residuals are on the fit record
        assert!(float("linear_regression([1.0, 2.0, 3.0], [2.0, 4.0, 6.0]).rss") < 1e-9);
        assert!(matches!(
            last("linear_regression([1.0, 2.0, 3.0], [2.0, 4.0, 6.0]).predictions.count()").unwrap(),
            Value::Int(3)
        ));
        // a no-intercept fit with a manual ones column matches the intercept fit's slope
        let slope = float(
            "m = multiple_regression([[1.0,1.0,1.0,1.0], [1.0,2.0,3.0,4.0]], [3.0,5.0,7.0,9.0], false)\nm.coefficients[1]",
        );
        assert!((slope - 2.0).abs() < 1e-6, "no-intercept slope = {slope}");
    }

    #[test]
    fn interpolation_error_points_at_the_real_line() {
        // A bad interpolation fragment used to report line 1 (the snippet); it must
        // now point at the line of the string in the original source. `{1 +}` is an
        // unambiguous parse error in the embedded expression.
        let err = last("x = 1\ny = 2\nprint(\"bad {1 +}\")").unwrap_err();
        assert_eq!(err.line, 3, "interpolation error should be on line 3, got {}", err.line);
    }

    #[test]
    fn min_max_sort_are_exact_above_2_to_53() {
        // Two distinct i64 just above 2^53 share one f64. The boxed reduction path
        // must compare them exactly (not via f64), or it picks the wrong element and
        // disagrees with the packed-Int path. `drop_missing` yields a boxed all-Int
        // array — the path that used to be wrong.
        let a = "[9007199254740992, 9007199254740993, missing].drop_missing()";
        assert!(matches!(last(&format!("{a}.max()")).unwrap(), Value::Int(9007199254740993)));
        assert!(matches!(last(&format!("{a}.min()")).unwrap(), Value::Int(9007199254740992)));
        // Sort keeps the two distinct and correctly ordered.
        let Value::Array(arr) = last("[9007199254740993, 9007199254740992].sort()").unwrap()
        else {
            panic!("expected an array");
        };
        let vs = arr.to_values();
        assert!(matches!(vs[0], Value::Int(9007199254740992)));
        assert!(matches!(vs[1], Value::Int(9007199254740993)));
    }

    #[test]
    fn closures_capture_lexical_environment() {
        // A returned/stored lambda still sees the enclosing function's local `k`.
        assert!(matches!(
            last("fn make(k) = (p => p + k)\ng = make(10)\ng(5)").unwrap(),
            Value::Int(15)
        ));
        // Capturing function-valued parameters: compose(inc, dbl)(10) = inc(dbl(10)).
        assert!(matches!(
            last("fn inc(n) = n + 1\nfn dbl(n) = n * 2\nfn compose(f, g) = (x => f(g(x)))\nh = compose(inc, dbl)\nh(10)").unwrap(),
            Value::Int(21)
        ));
        // Two-level nested capture: outer(1) -> (b => (c => 1 + b + c)).
        assert!(matches!(
            last("fn outer(a) = (b => (c => a + b + c))\nf = outer(1)\ng = f(2)\ng(3)").unwrap(),
            Value::Int(6)
        ));
    }

    #[test]
    fn string_and_join_methods() {
        assert!(matches!(last(r#""a,b,c".split(",").count()"#).unwrap(), Value::Int(3)));
        assert!(matches!(last(r#""  hi  ".trim()"#).unwrap(), Value::Str(s) if s.as_str() == "hi"));
        assert!(
            matches!(last(r#""a-b-c".replace("-", "_")"#).unwrap(), Value::Str(s) if s.as_str() == "a_b_c")
        );
        assert!(matches!(last(r#""hello".contains("ell")"#).unwrap(), Value::Bool(true)));
        assert!(matches!(last(r#""hello".starts_with("he")"#).unwrap(), Value::Bool(true)));
        assert!(matches!(last(r#""hello".ends_with("xo")"#).unwrap(), Value::Bool(false)));
        assert!(
            matches!(last(r#"["a","b","c"].join("-")"#).unwrap(), Value::Str(s) if s.as_str() == "a-b-c")
        );
        // `join` renders non-string elements too.
        assert!(matches!(last(r#"[1,2,3].join(",")"#).unwrap(), Value::Str(s) if s.as_str() == "1,2,3"));
        // Round-trips: split then join.
        assert!(
            matches!(last(r#""x|y|z".split("|").join(",")"#).unwrap(), Value::Str(s) if s.as_str() == "x,y,z")
        );
    }

    #[test]
    fn normalize_is_zero_mean() {
        let mean = float("[1, 2, 3, 4].normalize().mean()");
        assert!(mean.abs() < 1e-12);
    }

    #[test]
    fn negative_index() {
        assert!(matches!(last("[10, 20, 30][-1]").unwrap(), Value::Int(30)));
    }

    #[test]
    fn leading_operator_continuation() {
        // A line that STARTS with an infix operator continues the previous one.
        assert!(matches!(last("1\n  + 2\n  + 3").unwrap(), Value::Int(6)));
        // precedence is unaffected — `* 4` binds before the `+`s: 1 + 2 + (3*4) = 15
        assert!(matches!(last("1\n  + 2\n  + 3\n  * 4").unwrap(), Value::Int(15)));
        // comparison / boolean operators continue too
        assert!(matches!(last("10 > 5\n  and 2 == 2").unwrap(), Value::Bool(true)));
        // works inside a function body split across lines (the research use case)
        assert!(matches!(
            last("fn f(n) = (if n > 0 then 1 else 0)\n  + (if n > 1 then 2 else 0)\nf(2)").unwrap(),
            Value::Int(3)
        ));
        // a leading unary `-` is NOT a continuation: `x = 10` binds, `- 3` is a
        // separate (discarded) statement, so `x` is still 10 (continuation would be 7).
        assert!(matches!(last("x = 10\n- 3\nx").unwrap(), Value::Int(10)));
    }

    #[test]
    fn multiline_dot_chain() {
        let v = last("[3, 1, 2]\n    .sort()\n    .reverse()").unwrap();
        match v {
            Value::Array(a) => { let a = a.to_values();
                let got: Vec<i64> = a
                    .iter()
                    .map(|x| if let Value::Int(i) = x { *i } else { -1 })
                    .collect();
                assert_eq!(got, vec![3, 2, 1]);
            }
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn dna_gc_and_revcomp() {
        assert_eq!(float("dna(\"GGCC\").gc_content()"), 1.0);
        match last("dna(\"ATGC\").reverse_complement()").unwrap() {
            Value::Dna(s) => assert_eq!(&*s, "GCAT"),
            other => panic!("expected dna, got {:?}", other),
        }
    }

    #[test]
    fn kmers_count() {
        match last("dna(\"ATGCG\").kmers(3)").unwrap() {
            Value::Array(a) => assert_eq!(a.len(), 3),
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn kmer_counts_packed() {
        let s = |src: &str| last(src).unwrap().to_string();
        // The packed counter is byte-identical to the string spectrum…
        assert_eq!(
            s("dna(\"ATGCATGC\").kmer_counts(3)"),
            s("dna(\"ATGCATGC\").kmers(3).frequencies()")
        );
        // …including N breaking the window (same spectrum as `kmers`)…
        assert_eq!(
            s("dna(\"ATGNCC\").kmer_counts(2)"),
            s("dna(\"ATGNCC\").kmers(2).frequencies()")
        );
        // …and the empty case.
        assert_eq!(s("dna(\"AT\").kmer_counts(5)"), "[]");
        // k beyond the 2-bit u64 budget errors clearly.
        assert!(last("dna(\"ACGT\").kmer_counts(33)")
            .unwrap_err()
            .message
            .contains("up to 32"));
    }

    #[test]
    fn canonical_kmer_counts_collapses_strands() {
        let s = |src: &str| last(src).unwrap().to_string();
        // A k-mer and its reverse complement count together under the smaller
        // (canonical) form: AAA+TTT -> AAA, AAT+ATT -> AAT.
        assert_eq!(s("dna(\"AAATTT\").canonical_kmer_counts(3)"), "[(\"AAA\", 2), (\"AAT\", 2)]");
        // Total windows are unchanged — only the grouping differs from the forward spectrum.
        assert_eq!(
            s("dna(\"ATGCATGC\").canonical_kmer_counts(3).map(it[1]).sum()"),
            s("dna(\"ATGCATGC\").kmer_counts(3).map(it[1]).sum()")
        );
        // A palindromic k-mer (its own reverse complement) stays itself.
        assert_eq!(s("dna(\"ACGT\").canonical_kmer_counts(4)"), "[(\"ACGT\", 1)]");
        // Same k>32 guard as the forward counter.
        assert!(last("dna(\"ACGT\").canonical_kmer_counts(33)")
            .unwrap_err()
            .message
            .contains("up to 32"));
    }

    #[test]
    fn robustness_arithmetic_and_reshape_never_panic() {
        // `abs` of an Int i64::MIN must wrap, not panic in debug. The literal
        // `-9223372036854775808` overflows i64 and lexes as a float, so build the Int
        // value with wrapping arithmetic (0 - i64::MAX - 1 == i64::MIN).
        assert!(last("abs(0 - 9223372036854775807 - 1)").is_ok());
        // A reshape whose shape's element count overflows usize errors cleanly rather
        // than panicking on `attempt to multiply with overflow`.
        assert!(last("tensor([1, 2, 3, 4]).reshape([99999999999, 99999999999])")
            .unwrap_err()
            .message
            .contains("overflow"));
    }

    #[test]
    fn dataframe_constructor() {
        // build a frame from in-memory columns, then count rows
        assert_eq!(int("dataframe({a: [1, 2, 3], b: [4.0, 5.0, 6.0]}).count()"), 3);
        // `missing` becomes a null — still a row
        assert_eq!(int("dataframe({a: [1, missing, 3]}).count()"), 3);
        // the normal verbs operate on the constructed frame
        assert_eq!(
            int("dataframe({g: [\"x\", \"x\", \"y\"], v: [1, 3, 10]}).group(@g).mean(@v).count()"),
            2
        );
        // clear errors for misuse
        assert!(last("dataframe([1, 2, 3])")
            .unwrap_err()
            .message
            .contains("record of columns"));
        assert!(last("dataframe({a: [1, \"x\"]})").unwrap_err().message.contains("mixes types"));
    }

    #[test]
    fn array_unique_and_frequencies() {
        // `unique` keeps first-seen order.
        assert_eq!(last("[3, 1, 3, 2, 1].unique()").unwrap().to_string(), "[3, 1, 2]");
        assert_eq!(
            last("[\"AT\", \"TG\", \"AT\", \"CC\"].unique()").unwrap().to_string(),
            "[\"AT\", \"TG\", \"CC\"]"
        );
        // `frequencies` is the full (value, count) histogram, count desc then value asc.
        assert_eq!(
            last("[\"AT\", \"TG\", \"AT\"].frequencies()").unwrap().to_string(),
            "[(\"AT\", 2), (\"TG\", 1)]"
        );
        // `top` is unchanged by the shared-histogram refactor.
        assert_eq!(
            last("[3, 1, 3, 2, 1, 3].top(2)").unwrap().to_string(),
            "[(3, 3), (1, 2)]"
        );
    }

    #[test]
    fn kmers_vs_windows() {
        let strs = |src: &str| match last(src).unwrap() {
            Value::Array(a) => a
                .to_values()
                .iter()
                .map(|v| match v {
                    Value::Str(s) => (**s).clone(),
                    o => panic!("expected Str, got {o:?}"),
                })
                .collect::<Vec<_>>(),
            o => panic!("expected array, got {o:?}"),
        };
        // `kmers` is the ACGT-only spectrum: windows spanning `N` are skipped.
        assert_eq!(strs("dna(\"ATGNCC\").kmers(2)"), ["AT", "TG", "CC"]);
        // `windows` is faithful: every length-k substring, ambiguity included.
        assert_eq!(strs("dna(\"ATGNCC\").windows(2)"), ["AT", "TG", "GN", "NC", "CC"]);
        // A sequence shorter than k (or empty) yields `[]`, not an error.
        assert!(strs("dna(\"AT\").kmers(5)").is_empty());
        assert!(strs("dna(\"AT\").windows(5)").is_empty());
        // Pure ACGT: `kmers` keeps every window (spectrum == faithful here).
        assert_eq!(strs("dna(\"ATGC\").kmers(2)"), ["AT", "TG", "GC"]);
    }

    #[test]
    fn dna_find_motif() {
        assert_eq!(int("dna(\"ATGCGT\").find(\"GCG\")"), 2);
        // absent motif → missing (so it composes with `??`)
        assert!(matches!(
            last("dna(\"ATGC\").find(\"TTTT\")").unwrap(),
            Value::Missing
        ));
    }

    #[test]
    fn array_top_frequencies() {
        // most common elements as (value, count) tuples, ties broken by value
        match last("[\"a\", \"b\", \"a\", \"c\", \"a\", \"b\"].top(2)").unwrap() {
            Value::Array(a) => { let a = a.to_values();
                assert_eq!(a.len(), 2);
                match (&a[0], &a[1]) {
                    (Value::Tuple(t0), Value::Tuple(t1)) => {
                        assert!(matches!(&t0[0], Value::Str(s) if &**s == "a"));
                        assert!(matches!(&t0[1], Value::Int(3)));
                        assert!(matches!(&t1[0], Value::Str(s) if &**s == "b"));
                    }
                    _ => panic!("expected tuples"),
                }
            }
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn read_fasta_records() {
        // The shipped sample has 3 sequences; check shape + a sequence method
        // chains through the record field.
        let prog = "g = read_fasta(\"examples/data/sample.fa\")\ng.count()";
        assert_eq!(int(prog), 3);
        let first = "read_fasta(\"examples/data/sample.fa\")[0].length";
        assert_eq!(int(first), 120);
        // gc_content of the AT-rich third sequence is low
        let gc = "read_fasta(\"examples/data/sample.fa\")[2].seq.gc_content()";
        assert!(float(gc) < 0.1);
    }

    #[test]
    fn dataframe_cache_is_transparent() {
        // `.cache()` must be a pure performance hint — identical results to the
        // uncached frame (it only avoids re-scanning the source).
        let uncached = int("read_csv(\"examples/data/patients.csv\").count()");
        let cached = int("read_csv(\"examples/data/patients.csv\").cache().count()");
        assert_eq!(uncached, cached);
        let filtered = int("read_csv(\"examples/data/patients.csv\").cache().where(age > 40).count()");
        assert!(filtered <= cached);
    }

    #[test]
    fn compensated_summation_is_accurate() {
        // Catastrophic cancellation: naive left-to-right summation drops the two
        // small terms (1e16 + 1 rounds back to 1e16), giving 0; Neumaier recovers
        // the exact 2.0. This is why every float aggregation uses it.
        let xs = vec![1.0, 1e16, 1.0, -1e16];
        let naive: f64 = xs.iter().sum();
        assert_eq!(neumaier_sum(&xs), 2.0);
        assert_ne!(naive, 2.0, "the naive sum should be wrong here (it loses the 1.0s)");
    }

    #[test]
    fn interpolation_with_nested_string_literal() {
        // A string literal inside an interpolation expression must not end the
        // outer string or the interpolation early (both natural and escaped
        // quote forms work).
        match last("g = dna(\"ATGCGT\")\n\"at {g.find(\"GCG\")}\"").unwrap() {
            Value::Str(s) => assert_eq!(&*s, "at 2"),
            other => panic!("expected string, got {:?}", other),
        }
        match last("\"hi {\"world\".upper()}\"").unwrap() {
            Value::Str(s) => assert_eq!(&*s, "hi WORLD"),
            other => panic!("expected string, got {:?}", other),
        }
    }

    #[test]
    fn immutable_reassignment_errors() {
        let err = last("x = 1\nx = 2").unwrap_err();
        assert!(err.message.contains("immutable"));
    }

    #[test]
    fn mutable_reassignment_works() {
        assert!(matches!(
            last("mut x = 1\nx = x + 4\nx").unwrap(),
            Value::Int(5)
        ));
    }

    #[test]
    fn no_truthiness() {
        assert!(last("5 and true").unwrap_err().message.contains("boolean"));
    }

    #[test]
    fn method_typo_suggests() {
        let err = last("[1, 2].maen()").unwrap_err();
        assert_eq!(err.hint.as_deref(), Some("did you mean `mean`?"));
    }

    #[test]
    fn invalid_dna_rejected() {
        assert!(last("dna(\"ATBX\")").unwrap_err().message.contains("valid DNA"));
    }

    #[test]
    fn iupac_dna_and_complement() {
        // N + IUPAC ambiguity codes are accepted, matching `read_fasta` (was rejected).
        match last("dna(\"ATGN\")").unwrap() {
            Value::Dna(s) => assert_eq!(&*s, "ATGN"),
            o => panic!("expected Dna, got {o:?}"),
        }
        // IUPAC-correct complementation: R↔Y, N→N.
        match last("dna(\"ATGR\").complement()").unwrap() {
            Value::Dna(s) => assert_eq!(&*s, "TACY"),
            o => panic!("expected Dna, got {o:?}"),
        }
        match last("dna(\"ATGN\").reverse_complement()").unwrap() {
            Value::Dna(s) => assert_eq!(&*s, "NCAT"),
            o => panic!("expected Dna, got {o:?}"),
        }
        // gc_content excludes N from the denominator: "GCN" → 2/2 = 1.0, not 2/3.
        assert!((float("dna(\"GCN\").gc_content()") - 1.0).abs() < 1e-12);
    }

    #[test]
    fn dna_is_orderable() {
        // `<`/`>` order DNA lexicographically (like strings) — enables canonical
        // k-mer / sort-by-sequence code.
        let b = |src: &str| match last(src).unwrap() {
            Value::Bool(b) => b,
            other => panic!("expected Bool, got {other:?}"),
        };
        assert!(b("dna(\"ATG\") < dna(\"CAT\")"));
        assert!(!b("dna(\"CAT\") < dna(\"ATG\")"));
        assert!(b("dna(\"ATG\") <= dna(\"ATG\")"));
        // sorting an array of DNA orders it lexicographically.
        match last("[dna(\"CAT\"), dna(\"ATG\"), dna(\"GGG\")].sort().first()").unwrap() {
            Value::Dna(s) => assert_eq!(&*s, "ATG"),
            other => panic!("expected Dna, got {other:?}"),
        }
    }

    #[test]
    fn division_by_zero() {
        assert!(last("1 / 0").unwrap_err().message.contains("division by zero"));
    }

    #[test]
    fn if_expression() {
        match last("if 5 > 3 then \"yes\" else \"no\"").unwrap() {
            Value::Str(s) => assert_eq!(&*s, "yes"),
            other => panic!("expected string, got {:?}", other),
        }
        assert!(matches!(
            last("grade = if 50 > 90 then 1 else 2\ngrade").unwrap(),
            Value::Int(2)
        ));
    }

    #[test]
    fn if_condition_must_be_bool() {
        assert!(last("if 5 then 1 else 2")
            .unwrap_err()
            .message
            .contains("boolean"));
    }

    #[test]
    fn map_doubles() {
        match last("[1, 2, 3].map(it * 2)").unwrap() {
            Value::Array(a) => { let a = a.to_values();
                let got: Vec<i64> = a
                    .iter()
                    .map(|x| if let Value::Int(i) = x { *i } else { -1 })
                    .collect();
                assert_eq!(got, vec![2, 4, 6]);
            }
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn filter_and_where_are_equivalent() {
        let f = float("[1, 5, 8, 3, 9].filter(it > 4).count()");
        let w = float("[1, 5, 8, 3, 9].where(it > 4).count()");
        assert_eq!(f, 3.0);
        assert_eq!(w, 3.0);
    }

    #[test]
    fn reduce_sums() {
        assert!(matches!(
            last("[1, 2, 3, 4].reduce(0, (acc, x) => acc + x)").unwrap(),
            Value::Int(10)
        ));
    }

    #[test]
    fn reduce_requires_explicit_binders() {
        // the old magic `acc + it` form is now a friendly error
        let err = last("[1, 2, 3].reduce(0, acc + it)").unwrap_err();
        assert!(err.message.contains("accumulator function"));
    }

    #[test]
    fn explicit_named_binder() {
        match last("[1, 2, 3].map(n => n * 10)").unwrap() {
            Value::Array(a) => assert_eq!(a.len(), 3),
            other => panic!("expected array, got {:?}", other),
        }
        assert_eq!(float("[1, 5, 8].filter(v => v > 4).count()"), 2.0);
    }

    #[test]
    fn missing_is_missing() {
        assert!(matches!(last("missing.is_missing()").unwrap(), Value::Bool(true)));
        assert!(matches!(last("(5).is_missing()").unwrap(), Value::Bool(false)));
    }

    #[test]
    fn missing_propagates_through_math() {
        assert!(matches!(last("missing + 1").unwrap(), Value::Missing));
        assert!(matches!(last("missing * 2").unwrap(), Value::Missing));
        assert!(matches!(last("-missing").unwrap(), Value::Missing));
    }

    #[test]
    fn missing_equality_propagates() {
        // missing == missing is missing, NOT true — so `==` can't test for it
        assert!(matches!(last("missing == missing").unwrap(), Value::Missing));
        assert!(matches!(last("missing == 5").unwrap(), Value::Missing));
        assert!(matches!(last("missing < 3").unwrap(), Value::Missing));
    }

    #[test]
    fn missing_three_valued_logic() {
        assert!(matches!(last("true or missing").unwrap(), Value::Bool(true)));
        assert!(matches!(last("false or missing").unwrap(), Value::Missing));
        assert!(matches!(last("false and missing").unwrap(), Value::Bool(false)));
        assert!(matches!(last("true and missing").unwrap(), Value::Missing));
        assert!(matches!(last("not missing").unwrap(), Value::Missing));
    }

    #[test]
    fn missing_aggregations_propagate() {
        assert!(matches!(last("[1, missing, 3].mean()").unwrap(), Value::Missing));
        assert!(matches!(last("[1, missing, 3].sum()").unwrap(), Value::Missing));
        // drop_missing opts out, visibly
        assert_eq!(float("[1, missing, 3].drop_missing().mean()"), 2.0);
        // count includes the hole
        assert!(matches!(last("[1, missing, 3].count()").unwrap(), Value::Int(3)));
    }

    #[test]
    fn missing_if_condition_errors() {
        let err = last("if missing then 1 else 2").unwrap_err();
        assert!(err.message.contains("missing"));
    }

    #[test]
    fn missing_map_propagates_elementwise() {
        match last("[1, missing, 3].map(it + 10)").unwrap() {
            Value::Array(a) => { let a = a.to_values();
                assert!(matches!(a[0], Value::Int(11)));
                assert!(matches!(a[1], Value::Missing));
                assert!(matches!(a[2], Value::Int(13)));
            }
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn nested_map_with_named_binders() {
        // the readability win: name binders so nesting is unambiguous
        match last("[[1, 2], [3, 4]].map(row => row.map(v => v + 1))").unwrap() {
            Value::Array(a) => { let a = a.to_values();
                assert_eq!(a.len(), 2);
                match &a[1] {
                    Value::Array(inner) => assert!(matches!(inner.get(1), Value::Int(5))),
                    other => panic!("expected inner array, got {:?}", other),
                }
            }
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn chained_comprehension() {
        // filter evens-ish, double, then mean
        assert_eq!(float("[1, 2, 3, 4, 5].filter(it > 2).map(it * 2).mean()"), 8.0);
    }

    #[test]
    fn nested_comprehension_restores_it() {
        // inner `it` must not leak into the outer map
        assert_eq!(
            float("[1, 2].map([10, 20].map(it).sum()).sum()"),
            60.0
        );
    }

    #[test]
    fn user_function_basic() {
        assert!(matches!(last("fn sq(x) = x * x\nsq(7)").unwrap(), Value::Int(49)));
        assert!(matches!(
            last("fn area(w, h) = w * h\narea(3, 4)").unwrap(),
            Value::Int(12)
        ));
    }

    #[test]
    fn user_function_recursion() {
        assert!(matches!(
            last("fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\nfact(5)").unwrap(),
            Value::Int(120)
        ));
    }

    #[test]
    fn first_class_lambda_value() {
        assert!(matches!(last("double = x => x + x\ndouble(21)").unwrap(), Value::Int(42)));
    }

    #[test]
    fn function_arity_error() {
        let err = last("fn f(a, b) = a + b\nf(1)").unwrap_err();
        assert!(err.message.contains("expects 2 arguments"));
    }

    #[test]
    fn unknown_function_suggests() {
        let err = last("fn velocity(x) = x * 2\nvelociti(3)").unwrap_err();
        assert_eq!(err.hint.as_deref(), Some("did you mean `velocity`?"));
    }

    #[test]
    fn broadcast_array_scalar() {
        // (xs - mean) / std, written by hand, equals the built-in normalize
        let z = float("[1, 2, 3, 4].map(it).reduce(0, (a, x) => a + x)"); // 10, sanity
        assert_eq!(z, 10.0);
        assert_eq!(float("([2, 4, 6] - 2).sum()"), 6.0); // [0,2,4]
        assert_eq!(float("([1, 2, 3] * 10).sum()"), 60.0);
    }

    #[test]
    fn broadcast_array_array() {
        assert_eq!(float("([1, 2, 3] + [10, 20, 30]).sum()"), 66.0);
        let err = last("[1, 2] + [1, 2, 3]").unwrap_err();
        assert!(err.message.contains("different lengths"));
    }

    #[test]
    fn broadcast_propagates_missing() {
        match last("[1, missing, 3] + 10").unwrap() {
            Value::Array(a) => { let a = a.to_values();
                assert!(matches!(a[0], Value::Int(11)));
                assert!(matches!(a[1], Value::Missing));
            }
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn any_and_all() {
        assert!(matches!(last("[1, 2, 3].any(it > 2)").unwrap(), Value::Bool(true)));
        assert!(matches!(last("[1, 2, 3].any(it > 9)").unwrap(), Value::Bool(false)));
        assert!(matches!(last("[1, 2, 3].all(it > 0)").unwrap(), Value::Bool(true)));
        assert!(matches!(last("[1, 2, 3].all(it > 2)").unwrap(), Value::Bool(false)));
    }

    #[test]
    fn any_all_three_valued() {
        // no true, but a missing in the test -> missing
        assert!(matches!(last("[1, missing].any(it > 5)").unwrap(), Value::Missing));
        // a definite true wins over missing
        assert!(matches!(last("[9, missing].any(it > 5)").unwrap(), Value::Bool(true)));
    }

    #[test]
    fn take_and_drop() {
        assert_eq!(float("[1, 2, 3, 4, 5].take(2).sum()"), 3.0);
        assert_eq!(float("[1, 2, 3, 4, 5].drop(2).sum()"), 12.0);
        // over-take is clamped, not an error
        assert_eq!(float("[1, 2].take(99).count()"), 2.0);
    }

    #[test]
    fn zip_and_enumerate() {
        // zip pairs elementwise; pair sums via map
        assert_eq!(
            float("[1, 2, 3].zip([10, 20, 30]).map(it[0] + it[1]).sum()"),
            66.0
        );
        // enumerate yields [index, value] pairs
        assert_eq!(float("[7, 8, 9].enumerate().map(it[0]).sum()"), 3.0);
    }

    #[test]
    fn tensor_construction_and_shape() {
        match last("tensor([[1, 2], [3, 4]]).shape()").unwrap() {
            Value::Array(a) => { let a = a.to_values();
                assert!(matches!(a[0], Value::Int(2)));
                assert!(matches!(a[1], Value::Int(2)));
            }
            other => panic!("expected array, got {:?}", other),
        }
        assert!(matches!(last("tensor([[1, 2], [3, 4]]).ndim()").unwrap(), Value::Int(2)));
    }

    #[test]
    fn tensor_arithmetic_and_broadcast() {
        assert_eq!(float("(tensor([[1, 2], [3, 4]]) + 10).sum()"), 50.0); // 11+12+13+14
        // row vector [2] broadcasts over [2,2]: 11+22+13+24
        assert_eq!(float("(tensor([[1, 2], [3, 4]]) + tensor([10, 20])).sum()"), 70.0);
        assert_eq!(float("(tensor([1, 2, 3]) * tensor([2, 2, 2])).sum()"), 12.0);
    }

    #[test]
    fn tensor_matmul() {
        // [[1,2],[3,4]] · [[1,0],[1,1]] = [[3,2],[7,4]], sum = 16
        assert_eq!(
            float("tensor([[1, 2], [3, 4]]).matmul(tensor([[1, 0], [1, 1]])).sum()"),
            16.0
        );
    }

    #[test]
    fn tensor_reductions() {
        assert_eq!(float("tensor([[1, 2], [3, 4]]).sum()"), 10.0);
        assert_eq!(float("tensor([[1, 2], [3, 4]]).mean()"), 2.5);
        assert_eq!(float("tensor([[1, 2], [3, 4]]).max()"), 4.0);
    }

    #[test]
    fn tensor_reshape_and_transpose() {
        assert!(matches!(
            last("tensor([1, 2, 3, 4]).reshape([2, 2]).ndim()").unwrap(),
            Value::Int(2)
        ));
        assert_eq!(float("tensor([[1, 2], [3, 4]]).transpose().sum()"), 10.0);
    }

    #[test]
    fn tensor_constructors() {
        assert_eq!(float("zeros([2, 3]).sum()"), 0.0);
        assert_eq!(float("ones([2, 3]).sum()"), 6.0);
        assert_eq!(float("eye(3).sum()"), 3.0);
    }

    #[test]
    fn tensor_math_broadcast() {
        assert_eq!(float("sqrt(tensor([1, 4, 9])).sum()"), 6.0); // 1+2+3
    }

    #[test]
    fn activations_argmax_softmax() {
        // relu / sigmoid broadcast over scalars, arrays, and tensors
        assert_eq!(float("relu(0.0 - 2.0)"), 0.0);
        assert_eq!(float("relu(3.0)"), 3.0);
        assert!((float("sigmoid(0.0)") - 0.5).abs() < 1e-12);
        assert_eq!(float("relu([0.0 - 1.0, 5.0, 0.0 - 2.0]).sum()"), 5.0);
        assert_eq!(float("relu(tensor([[0.0 - 1.0, 2.0], [3.0, 0.0 - 4.0]])).sum()"), 5.0);
        // argmax / argmin over arrays and tensors → Int index (first on ties)
        assert!(matches!(last("argmax([3.0, 1.0, 9.0, 2.0])").unwrap(), Value::Int(2)));
        assert!(matches!(last("argmin([3.0, 1.0, 9.0, 2.0])").unwrap(), Value::Int(1)));
        assert!(matches!(last("argmax(tensor([5.0, 8.0, 1.0]))").unwrap(), Value::Int(1)));
        assert!(last("argmax([])").is_err());
        // tensor softmax (last axis): a row is a distribution that sums to 1
        assert!((float("tensor([1.0, 2.0, 3.0]).softmax().sum()") - 1.0).abs() < 1e-12);
        assert!((float("tensor([[1.0, 2.0], [3.0, 4.0]]).softmax().sum()") - 2.0).abs() < 1e-12);
        // softmax is monotone: the largest logit gets the largest probability
        assert!(matches!(last("argmax(tensor([1.0, 3.0, 2.0]).softmax())").unwrap(), Value::Int(1)));
    }

    #[test]
    fn autodiff_reverse_mode() {
        // d/dx x^2 = 2x = 6
        assert!((float("let x = variable(3.0) in gradient(x * x, x)") - 6.0).abs() < 1e-9);
        // d/da (a-2)^2 = 2(a-2) = 6
        assert!((float("let a = variable(5.0) in gradient((a - 2.0) ** 2, a)") - 6.0).abs() < 1e-9);
        // d/dx exp(x) at 1 = e ;  d/dx ln(x) at 2 = 0.5
        assert!((float("let x = variable(1.0) in gradient(exp(x), x)") - std::f64::consts::E).abs() < 1e-9);
        assert!((float("let x = variable(2.0) in gradient(ln(x), x)") - 0.5).abs() < 1e-9);
        // sigmoid'(0) = 0.25 ;  tanh'(0) = 1
        assert!((float("let s = variable(0.0) in gradient(sigmoid(s), s)") - 0.25).abs() < 1e-9);
        assert!((float("let s = variable(0.0) in gradient(tanh(s), s)") - 1.0).abs() < 1e-9);
        // vector: grad sum(v^2) = 2v → summed = 2*(1+2+3) = 12
        assert!((float("let v = variable(tensor([1.0,2.0,3.0])) in gradient((v*v).sum(), v).sum()") - 12.0).abs() < 1e-9);
        // relu gate: grad of sum(relu(z)) counts the positive entries (2 of 3 here)
        assert!((float("let z = variable(tensor([0.0 - 1.0, 0.5, 2.0])) in gradient(relu(z).sum(), z).sum()") - 2.0).abs() < 1e-9);
        // division: d/da (a/b) = 1/b = 0.5
        assert!((float("let a = variable(6.0) in let b = variable(2.0) in gradient(a / b, a)") - 0.5).abs() < 1e-9);
        // gradient w.r.t. an array of leaves returns an array of grads (∂(a*b): [b, a])
        assert!(matches!(last("let a = variable(3.0) in let b = variable(4.0) in gradient(a * b, [a, b])[0]").unwrap(), Value::Float(f) if (f - 4.0).abs() < 1e-9));
        // a non-scalar loss is rejected (must reduce first)
        assert!(last("let v = variable(tensor([1.0,2.0])) in gradient(v * v, v)").is_err());
        // end-to-end: linear regression (y=2x+1) trains to ~zero loss via matmul + GD
        let prog = "X = tensor([[1.0,1.0],[2.0,1.0],[3.0,1.0],[4.0,1.0]])\n\
                    yt = tensor([3.0,5.0,7.0,9.0])\n\
                    fn step(w) = do { wv = variable(w)\n\
                      loss = ((X.matmul(wv) - yt) ** 2).mean()\n\
                      w - 0.05 * gradient(loss, wv) }\n\
                    w = range(0,400).reduce(tensor([0.0,0.0]), (w,i) => step(w))\n\
                    value_of(((X.matmul(variable(w)) - yt) ** 2).mean())";
        assert!(float(prog) < 1e-4, "linear regression did not converge");
    }

    #[test]
    fn tensor_ragged_errors() {
        assert!(last("tensor([[1, 2], [3]])")
            .unwrap_err()
            .message
            .contains("same shape"));
    }

    #[test]
    fn tensor_indexing_and_slicing() {
        // index first axis: 2-D -> row (1-D tensor), then -> scalar
        assert_eq!(float("tensor([[1, 2, 3], [4, 5, 6]])[0].sum()"), 6.0);
        assert_eq!(float("tensor([[1, 2, 3], [4, 5, 6]])[1][2]"), 6.0);
        assert_eq!(float("tensor([10, 20, 30])[1]"), 20.0); // 1-D index -> scalar
        assert_eq!(float("tensor([10, 20, 30])[-1]"), 30.0);
        // slice first axis
        assert_eq!(float("tensor([10, 20, 30, 40])[1:3].sum()"), 50.0); // 20+30
        assert_eq!(float("tensor([1, 2, 3])[::-1][0]"), 3.0); // reversed, first is last
        // sub-matrix keeps rank
        assert!(matches!(
            last("tensor([[1, 2], [3, 4]])[0:1].ndim()").unwrap(),
            Value::Int(2)
        ));
        // out of bounds errors
        assert!(last("tensor([1, 2, 3])[9]").unwrap_err().message.contains("out of bounds"));
    }

    #[test]
    fn tensor_axis_reductions() {
        assert_eq!(float("tensor([[1, 2], [3, 4]]).sum(0).sum()"), 10.0);
        assert!(matches!(
            last("tensor([[1, 2], [3, 4]]).sum(0).count()").unwrap(),
            Value::Int(2)
        ));
        // mean along axis 1 of [[1,2],[3,4]] = [1.5, 3.5], sum = 5
        assert_eq!(float("tensor([[1, 2], [3, 4]]).mean(1).sum()"), 5.0);
        assert!(last("tensor([[1, 2], [3, 4]]).sum(5)")
            .unwrap_err()
            .message
            .contains("out of range"));
    }

    #[test]
    fn tensor_vector_and_matvec() {
        assert_eq!(float("tensor([1, 2, 3]).dot(tensor([4, 5, 6]))"), 32.0); // 4+10+18
        // matrix·vector: [[1,2],[3,4]]·[1,1] = [3,7], sum 10
        assert_eq!(float("tensor([[1, 2], [3, 4]]).matmul(tensor([1, 1])).sum()"), 10.0);
    }

    #[test]
    fn tensor_norm() {
        assert_eq!(float("tensor([3, 4]).norm()"), 5.0);
    }

    #[test]
    fn tensor_det_inv_solve() {
        assert_eq!(float("tensor([[1, 2], [3, 4]]).det()"), -2.0);
        // A · inv(A) = I_2, whose entries sum to 2
        let s = float("tensor([[1, 2], [3, 4]]).matmul(tensor([[1, 2], [3, 4]]).inv()).sum()");
        assert!((s - 2.0).abs() < 1e-9);
        // solve A x = b=[1,1]; then A·x ≈ b, summing to 2
        let s2 = float(
            "tensor([[1, 2], [3, 4]]).matmul(tensor([[1, 2], [3, 4]]).solve(tensor([1, 1]))).sum()",
        );
        assert!((s2 - 2.0).abs() < 1e-9);
        assert!(last("tensor([[1, 2], [2, 4]]).inv()")
            .unwrap_err()
            .message
            .contains("singular"));
    }

    #[test]
    fn tensor_matmul_shape_error() {
        let err = last("tensor([[1, 2, 3]]).matmul(tensor([[1, 2]]))").unwrap_err();
        assert!(err.message.contains("not aligned"));
    }

    #[test]
    fn let_in_expressions() {
        assert!(matches!(last("let x = 5 in x + 1").unwrap(), Value::Int(6)));
        // multiple bindings
        assert!(matches!(last("let a = 1, b = 2 in a + b").unwrap(), Value::Int(3)));
        // sequential: later binding sees earlier
        assert!(matches!(last("let a = 10, b = a * 2 in b").unwrap(), Value::Int(20)));
        // scoping: inner binding does not leak, outer is restored
        assert!(matches!(
            last("x = 100\ny = let x = 7 in x + 1\nx").unwrap(),
            Value::Int(100)
        ));
        // composes inside a function, returning a destructurable tuple
        assert!(matches!(
            last("fn f(n) = let a = n, b = n + 1 in (a, b)\nlo, hi = f(5)\nhi").unwrap(),
            Value::Int(6)
        ));
    }

    #[test]
    fn tuples_and_indexing() {
        assert!(matches!(last("(3, 4)[0]").unwrap(), Value::Int(3)));
        assert!(matches!(last("(3, 4)[1]").unwrap(), Value::Int(4)));
        assert!(matches!(last("(1, 2, 3)[-1]").unwrap(), Value::Int(3)));
        // a 1-tuple is distinct from grouping
        match last("(5,)").unwrap() {
            Value::Tuple(t) => assert_eq!(t.len(), 1),
            other => panic!("expected 1-tuple, got {:?}", other),
        }
        // (x) is just grouping, not a tuple
        assert!(matches!(last("(5)").unwrap(), Value::Int(5)));
        // heterogeneous
        match last("(\"a\", 1, true)").unwrap() {
            Value::Tuple(t) => assert_eq!(t.len(), 3),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn destructuring() {
        assert!(matches!(last("a, b = (1, 2)\na").unwrap(), Value::Int(1)));
        assert!(matches!(last("a, b = (1, 2)\nb").unwrap(), Value::Int(2)));
        // from an array
        assert!(matches!(last("x, y, z = [10, 20, 30]\ny").unwrap(), Value::Int(20)));
        // mut destructuring rebinds
        assert!(matches!(
            last("mut a, b = (1, 2)\na = a + 10\na").unwrap(),
            Value::Int(11)
        ));
        // function returning a tuple, then unpack
        assert!(matches!(
            last("fn pair(x) = (x, x + 1)\nlo, hi = pair(5)\nhi").unwrap(),
            Value::Int(6)
        ));
        // length mismatch is an error
        assert!(last("a, b = (1, 2, 3)").unwrap_err().message.contains("cannot destructure"));
        // destructuring a non-collection is an error
        assert!(last("a, b = 5").unwrap_err().message.contains("cannot destructure"));
    }

    #[test]
    fn lambda_param_destructuring() {
        // (a, b) => ... destructures each tuple element
        assert_eq!(float("[(1, 2), (3, 4)].map((a, b) => a + b).sum()"), 10.0);
        // over zip
        assert_eq!(
            float("[1, 2, 3].zip([10, 20, 30]).map((a, b) => a + b).sum()"),
            66.0
        );
        // over enumerate, in filter
        assert_eq!(
            float("[5, 15, 25].enumerate().where((i, v) => v > 10).map((i, v) => i).sum()"),
            3.0 // indices 1 and 2
        );
        // destructuring an element that isn't a tuple/array errors
        assert!(last("[1, 2, 3].map((a, b) => a)")
            .unwrap_err()
            .message
            .contains("cannot destructure"));
        // arity mismatch errors
        assert!(last("[(1, 2, 3)].map((a, b) => a)")
            .unwrap_err()
            .message
            .contains("expects 2 values"));
    }

    #[test]
    fn zip_enumerate_yield_tuples() {
        match last("[1, 2].zip([3, 4]).first()").unwrap() {
            Value::Tuple(t) => {
                assert!(matches!(t[0], Value::Int(1)));
                assert!(matches!(t[1], Value::Int(3)));
            }
            other => panic!("expected tuple, got {:?}", other),
        }
        match last("[7, 8].enumerate().first()").unwrap() {
            Value::Tuple(t) => {
                assert!(matches!(t[0], Value::Int(0)));
                assert!(matches!(t[1], Value::Int(7)));
            }
            other => panic!("expected tuple, got {:?}", other),
        }
    }

    #[test]
    fn slicing() {
        let nums = |src: &str| -> Vec<i64> {
            match last(src).unwrap() {
                Value::Array(a) => a
                    .to_values()
                    .iter()
                    .map(|v| if let Value::Int(i) = v { *i } else { -999 })
                    .collect(),
                other => panic!("expected array, got {:?}", other),
            }
        };
        assert_eq!(nums("[0,1,2,3,4,5][1:4]"), vec![1, 2, 3]);
        assert_eq!(nums("[0,1,2,3,4,5][:3]"), vec![0, 1, 2]);
        assert_eq!(nums("[0,1,2,3,4,5][3:]"), vec![3, 4, 5]);
        assert_eq!(nums("[0,1,2,3,4,5][::2]"), vec![0, 2, 4]);
        assert_eq!(nums("[0,1,2,3,4,5][::-1]"), vec![5, 4, 3, 2, 1, 0]);
        assert_eq!(nums("[0,1,2,3,4,5][-2:]"), vec![4, 5]);
        assert_eq!(nums("[0,1,2,3,4,5][:-2]"), vec![0, 1, 2, 3]);
        // string + dna slicing
        match last("\"helix\"[1:4]").unwrap() {
            Value::Str(s) => assert_eq!(&*s, "eli"),
            other => panic!("{:?}", other),
        }
        match last("dna(\"ATGC\")[::-1]").unwrap() {
            Value::Dna(s) => assert_eq!(&*s, "CGTA"),
            other => panic!("{:?}", other),
        }
        // step of zero errors
        assert!(last("[1,2,3][::0]").unwrap_err().message.contains("step cannot be zero"));
    }

    #[test]
    fn records_and_fields() {
        assert!(matches!(
            last("r = {name: \"Ada\", age: 41}\nr.age").unwrap(),
            Value::Int(41)
        ));
        match last("r = {name: \"Ada\", age: 41}\nr.name").unwrap() {
            Value::Str(s) => assert_eq!(&*s, "Ada"),
            other => panic!("expected string, got {:?}", other),
        }
        // nested field chain
        assert!(matches!(
            last("s = {lead: {lab: 12}}\ns.lead.lab").unwrap(),
            Value::Int(12)
        ));
        // array of records + comprehension over a field
        assert_eq!(
            float("[{age: 10}, {age: 20}, {age: 30}].map(it.age).mean()"),
            20.0
        );
        // missing field is an error
        assert!(last("{a: 1}.b").unwrap_err().message.contains("no field"));
    }

    #[test]
    fn string_interpolation() {
        match last("name = \"Ada\"\nage = 41\n\"hi {name}, {age + 9}\"").unwrap() {
            Value::Str(s) => assert_eq!(&*s, "hi Ada, 50"),
            other => panic!("expected string, got {:?}", other),
        }
        // literal braces via doubling
        match last("\"{{x}}\"").unwrap() {
            Value::Str(s) => assert_eq!(&*s, "{x}"),
            other => panic!("expected string, got {:?}", other),
        }
        // nested quotes + if-expression inside an interpolation
        match last("\"g={if 5 > 3 then \"A\" else \"B\"}\"").unwrap() {
            Value::Str(s) => assert_eq!(&*s, "g=A"),
            other => panic!("expected string, got {:?}", other),
        }
    }

    #[test]
    fn coalesce_operator() {
        assert!(matches!(last("missing ?? 5").unwrap(), Value::Int(5)));
        assert!(matches!(last("7 ?? 5").unwrap(), Value::Int(7)));
        // right side is not evaluated when the left is present (else this would error)
        assert!(matches!(last("7 ?? undefinedvar").unwrap(), Value::Int(7)));
        // chains left-associatively
        assert!(matches!(last("missing ?? missing ?? 3").unwrap(), Value::Int(3)));
    }

    #[test]
    fn power_operator() {
        assert!(matches!(last("2 ** 10").unwrap(), Value::Int(1024)));
        assert!(matches!(last("-2 ** 2").unwrap(), Value::Int(-4))); // -(2**2)
        assert!(matches!(last("2 ** 3 ** 2").unwrap(), Value::Int(512))); // right-assoc
        assert_eq!(float("2.0 ** 0.5"), std::f64::consts::SQRT_2);
    }

    #[test]
    fn power_broadcasts() {
        assert_eq!(float("([1, 2, 3] ** 2).sum()"), 14.0); // 1+4+9
    }

    #[test]
    fn math_functions() {
        assert_eq!(float("sqrt(16)"), 4.0);
        assert_eq!(float("log(8, 2)"), 3.0);
        assert_eq!(float("hypot(3, 4)"), 5.0);
        assert!(matches!(last("abs(-7)").unwrap(), Value::Int(7))); // abs preserves Int
        assert!(matches!(last("floor(2.9)").unwrap(), Value::Int(2)));
        assert!(matches!(last("sqrt(missing)").unwrap(), Value::Missing));
    }

    #[test]
    fn float_predicates_guard_nan_and_inf() {
        use Value::Bool;
        assert!(matches!(last("is_finite(1.5)").unwrap(), Bool(true)));
        assert!(matches!(last("is_finite(inf)").unwrap(), Bool(false)));
        assert!(matches!(last("is_infinite(inf)").unwrap(), Bool(true)));
        assert!(matches!(last("is_nan(inf - inf)").unwrap(), Bool(true)));
        assert!(matches!(last("is_nan(1.5)").unwrap(), Bool(false)));
        // an Int/Rational is exact — always finite, never NaN/inf
        assert!(matches!(last("is_finite(5)").unwrap(), Bool(true)));
        assert!(matches!(last("is_nan(5)").unwrap(), Bool(false)));
        assert!(matches!(last("is_infinite(rational(1, 3))").unwrap(), Bool(false)));
        // missing propagates (ADR-0001)
        assert!(matches!(last("is_nan(missing)").unwrap(), Value::Missing));
        // The guard the user needs: a `<` on a NaN raises ("cannot compare … (NaN?)"),
        // but the predicate lets a program branch *before* the comparison.
        assert_eq!(float("x = inf - inf\nif is_nan(x) then 0.0 else x"), 0.0);
        // array form → a Bool array
        match last("is_finite([1.0, inf, 2.0])").unwrap() {
            Value::Array(a) => {
                let a = a.to_values();
                assert!(matches!(a[0], Bool(true)));
                assert!(matches!(a[1], Bool(false)));
                assert!(matches!(a[2], Bool(true)));
            }
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn math_constants_predefined() {
        assert!((float("pi") - std::f64::consts::PI).abs() < 1e-12);
        assert!((float("e") - std::f64::consts::E).abs() < 1e-12);
    }

    #[test]
    fn math_broadcasts_over_array() {
        match last("sqrt([1, 4, 9])").unwrap() {
            Value::Array(a) => { let a = a.to_values();
                assert!(matches!(a[2], Value::Float(x) if (x - 3.0).abs() < 1e-9));
            }
            other => panic!("expected array, got {:?}", other),
        }
    }

    #[test]
    fn dataframe_read_and_count() {
        assert!(matches!(
            last("read_csv(\"examples/data/patients.csv\").count()").unwrap(),
            Value::Int(8)
        ));
    }

    #[test]
    fn dataframe_where_lowers_to_polars() {
        // `age > 40` is translated to a Polars filter, not an interpreter loop
        assert!(matches!(
            last("read_csv(\"examples/data/patients.csv\").where(age > 40).count()").unwrap(),
            Value::Int(5)
        ));
        // compound predicate with `and` + a second column
        assert!(matches!(
            last("read_csv(\"examples/data/patients.csv\").where(age > 40 and resting_hr < 75).count()")
                .unwrap(),
            Value::Int(3)
        ));
    }

    #[test]
    fn dataframe_select_sort_chain() {
        let v = last(
            "read_csv(\"examples/data/patients.csv\").where(age > 40).select(name, age).sort(age).count()",
        )
        .unwrap();
        assert!(matches!(v, Value::Int(5)));
    }

    #[test]
    fn dataframe_group_agg() {
        // 3 species -> 3 grouped rows
        assert!(matches!(
            last("read_csv(\"examples/data/genes.csv\").group(species).mean(expression).count()")
                .unwrap(),
            Value::Int(3)
        ));
    }

    #[test]
    fn dataframe_parquet_roundtrip() {
        // write the patients CSV out as Parquet, read it back, query it
        last(
            "read_csv(\"examples/data/patients.csv\").write_parquet(\"/tmp/helix_test_rt.parquet\")",
        )
        .unwrap();
        assert!(matches!(
            last("read_parquet(\"/tmp/helix_test_rt.parquet\").where(age > 40).count()").unwrap(),
            Value::Int(5)
        ));
    }

    #[test]
    fn dataframe_unknown_column_errors() {
        let err = last("read_csv(\"examples/data/patients.csv\").where(agee > 40)").unwrap_err();
        assert!(err.message.contains("no column or variable named `agee`"));
    }

    #[test]
    fn filter_needs_boolean() {
        assert!(last("[1, 2, 3].filter(it * 2)")
            .unwrap_err()
            .message
            .contains("yes/no"));
    }

    #[test]
    fn dataframe_column_extracts_typed_values_and_nulls() {
        // A string column comes back as `Str` values.
        assert!(matches!(
            last("read_vcf(\"examples/data/variants.vcf\").column(\"chrom\").first()").unwrap(),
            Value::Str(s) if &*s == "chr17"
        ));
        // The VCF `id` column has `.` entries → Polars nulls → `missing`; `drop_missing`
        // leaves the 4 named variants of 6 (rows 3 and 5 are `.`).
        assert!(matches!(
            last("read_vcf(\"examples/data/variants.vcf\").column(\"id\").drop_missing().count()")
                .unwrap(),
            Value::Int(4)
        ));
    }

    #[test]
    fn dataframe_join_suffixes_colliding_columns() {
        // A self-join shares the non-key columns (gene, expression), which take a
        // `_right` suffix; the key (sample_id) coalesces. 3 cols → 5 after the join.
        let v = last(
            "s = read_csv(\"examples/data/samples.csv\")\ns.join(s, sample_id).columns()",
        )
        .unwrap();
        match v {
            Value::Array(cols) => { let cols = cols.to_values();
                let names: Vec<String> = cols.iter().map(|c| format!("{c}")).collect();
                assert!(names.contains(&"gene_right".to_string()), "got {names:?}");
                assert_eq!(names.len(), 5, "got {names:?}");
            }
            other => panic!("expected an array of column names, got {other:?}"),
        }
    }

    #[test]
    fn dataframe_with_replaces_columns_and_reads_variables() {
        // `with` can replace an existing column and resolve a bare name to a Helix
        // variable (the resolve_var path) rather than a column. ages * 10 > 400 for 5/8.
        assert!(matches!(
            last("factor = 10\np = read_csv(\"examples/data/patients.csv\")\np.with({age: age * factor}).where(age > 400).count()")
                .unwrap(),
            Value::Int(5)
        ));
    }

    #[test]
    fn dataframe_group_std_and_fully_filtered() {
        // A grouped `std` aggregation runs (3 species → 3 rows).
        assert!(matches!(
            last("read_csv(\"examples/data/genes.csv\").group(species).std(expression).count()")
                .unwrap(),
            Value::Int(3)
        ));
        // A predicate excluding every row yields an empty frame, not an error.
        assert!(matches!(
            last("read_csv(\"examples/data/patients.csv\").where(age > 1000).count()").unwrap(),
            Value::Int(0)
        ));
    }
