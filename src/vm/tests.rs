    use super::*;
    use crate::{bytecode, lexer, parser};

    /// Run a source string on the VM and return the value of its final
    /// expression (the trailing `Pop` is stripped so the value survives).
    fn vm_val(src: &str) -> Value {
        let toks = lexer::lex(src).unwrap();
        let ast = parser::parse(toks).unwrap();
        let mut prog = bytecode::compile_with_types(&ast, None).expect("expected this program to compile to bytecode");
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        exec(&prog, None).unwrap().pop().unwrap_or(Value::Unit)
    }

    /// The same source through the reference tree-walker.
    fn tw_val(src: &str) -> Value {
        let toks = lexer::lex(src).unwrap();
        let ast = parser::parse(toks).unwrap();
        let mut interp = Interp::new();
        let mut last = Value::Unit;
        for stmt in &ast {
            last = interp.exec(stmt).unwrap().value;
        }
        last
    }

    // ---------- differential fuzzing ----------
    //
    // Generate thousands of random programs and assert the bytecode VM and the
    // tree-walker produce the *same outcome* (same value, or both reject). This
    // automatically hunts the cross-engine divergence class the manual audit
    // found by hand (lossy comparison, overflow, etc.) — and guards against
    // regressions as the engines evolve.

    /// Deterministic PRNG (SplitMix64-style) so failures reproduce exactly.
    fn next(rng: &mut u64) -> u64 {
        *rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn pick(rng: &mut u64, n: u64) -> u64 {
        next(rng) % n
    }

    /// Random expression over the VM-compilable scalar core: int/float literals
    /// (including values near the i64 edge, to stress overflow + comparison),
    /// `+ - *`, comparisons, `if`, `let`, negation, and variable reads.
    fn gen_expr(rng: &mut u64, depth: u32, vars: &[String]) -> String {
        if depth == 0 || pick(rng, 3) == 0 {
            return match pick(rng, 4) {
                0 if !vars.is_empty() => vars[pick(rng, vars.len() as u64) as usize].clone(),
                1 => {
                    // occasionally a huge int to probe exact i64 comparison + wrap
                    let big = [0i64, 9_007_199_254_740_992, 9_007_199_254_740_993, i64::MAX, i64::MIN];
                    format!("{}", big[pick(rng, big.len() as u64) as usize])
                }
                2 => format!("{}.0", (next(rng) % 401) as i64 - 200),
                _ => format!("{}", (next(rng) % 4001) as i64 - 2000),
            };
        }
        match pick(rng, 20) {
            19 => {
                // Integer bitwise ops. Float/huge operands error identically on both
                // engines; int operands exercise the success path. Shift amounts are
                // reduced `% 64` (Helix int `%` is rem_euclid → always 0..=63) so
                // `<<`/`>>` stay in range; a float amount errors on both engines.
                match pick(rng, 5) {
                    0 => format!("(({}) & ({}))", gen_expr(rng, depth - 1, vars), gen_expr(rng, depth - 1, vars)),
                    1 => format!("(({}) | ({}))", gen_expr(rng, depth - 1, vars), gen_expr(rng, depth - 1, vars)),
                    2 => format!("(({}) ^ ({}))", gen_expr(rng, depth - 1, vars), gen_expr(rng, depth - 1, vars)),
                    3 => format!("(({}) << (({}) % 64))", gen_expr(rng, depth - 1, vars), gen_expr(rng, 0, vars)),
                    _ => format!("(({}) >> (({}) % 64))", gen_expr(rng, depth - 1, vars), gen_expr(rng, 0, vars)),
                }
            }
            15 => {
                // `??` coalescing: `missing ?? E` is `E`; `E ?? _` is `E` when `E`
                // isn't missing. Bias the left toward `missing` so both the take-left
                // and take-right paths are exercised (exercises Coalesce/CoalesceCheck).
                let left = if pick(rng, 2) == 0 {
                    "missing".to_string()
                } else {
                    gen_expr(rng, depth - 1, vars)
                };
                format!("({} ?? {})", left, gen_expr(rng, depth - 1, vars))
            }
            16 => {
                // `try EXPR` yields `{ok, value}` (value is `missing` on the error
                // path), so `(try E).value ?? F` pulls a scalar back out — exercising
                // TryBegin/TryOk/TryErr + field access, including caught runtime errors
                // (e.g. division by zero from a nested arm). Both engines must catch
                // and recover identically.
                format!(
                    "((try ({})).value ?? ({}))",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, 0, vars)
                )
            }
            17 => {
                // `match` over a scalar with an int-literal arm + wildcard → scalar
                // (exercises MatchArm and the first-match-wins ordering). A non-int or
                // non-matching scrutinee falls to `_` identically on both engines.
                let pat = (next(rng) % 5) as i64;
                format!(
                    "(match ({}) {{ {} => ({}), _ => ({}) }})",
                    gen_expr(rng, depth - 1, vars),
                    pat,
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars)
                )
            }
            18 => {
                // An immediately-applied closure → MakeFunc + CallValue end-to-end.
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                format!(
                    "((z => (z {} ({})))({}))",
                    op,
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, depth - 1, vars)
                )
            }
            14 => {
                // Multi-binder comprehension (pattern binder `(p, q)`) over an array
                // → scalar/Bool. Exercises `DestructureBind` vs the tree-walker's
                // `eval_with_pattern`, including error paths (a scalar element can't
                // destructure into two params — both engines must reject identically).
                let pair = pick(rng, 2) == 0;
                let mk = |rng: &mut u64, vars: &[String]| -> String {
                    if pair {
                        format!("({}, {})", gen_expr(rng, 0, vars), gen_expr(rng, 0, vars))
                    } else {
                        gen_expr(rng, 0, vars) // non-pair → destructure error on both
                    }
                };
                let e0 = mk(rng, vars);
                let e1 = mk(rng, vars);
                match pick(rng, 3) {
                    0 => {
                        let op = ["+", "-", "*"][pick(rng, 3) as usize];
                        format!(
                            "(([{e0}, {e1}]).map((p, q) => p {op} q))[{}]",
                            gen_expr(rng, 0, vars)
                        )
                    }
                    1 => {
                        let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                        format!("([{e0}, {e1}]).all((p, q) => p {cop} q)")
                    }
                    _ => {
                        let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                        format!("([{e0}, {e1}]).any((p, q) => p {cop} q)")
                    }
                }
            }
            13 => {
                // any/all over a small array → Bool (exercises short-circuit loop)
                let m = if pick(rng, 2) == 0 { "any" } else { "all" };
                let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                format!(
                    "([{}, {}, {}]).{}(it {} {})",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    m,
                    cop,
                    gen_expr(rng, 0, vars)
                )
            }
            10 if pick(rng, 2) == 0 => {
                // fused range reduce → scalar (range bound may be huge → cap error,
                // negative → empty, or moderate → loop; all agree with the array path)
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                format!(
                    "(range({} % 5000)).reduce({}, (acc, x) => acc {} x)",
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars),
                    op
                )
            }
            11 => {
                // reduce over a small array → scalar (exercises the reduce loop)
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                format!(
                    "([{}, {}, {}, {}]).reduce({}, (acc, x) => acc {} x)",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, 0, vars),
                    op
                )
            }
            12 => {
                // map then index → scalar (exercises the map loop + binder)
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                format!(
                    "(([{}, {}, {}]).map(it {} {}))[{}]",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    op,
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars)
                )
            }
            8 => {
                // tuple literal indexed → a scalar element (exercises MakeTuple)
                format!(
                    "(({}, {}, {}))[{}]",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars)
                )
            }
            9 => {
                // record literal + field access → scalar (exercises MakeRecord/GetField)
                let field = ["a", "b", "c"][pick(rng, 3) as usize];
                format!(
                    "({{a: {}, b: {}, c: {}}}).{}",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    field
                )
            }
            10 => {
                // array sliced then indexed → scalar (exercises Slice)
                format!(
                    "(([{}, {}, {}, {}])[{}:{}])[{}]",
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars)
                )
            }
            6 => {
                // array literal + index (exercises MakeArray / Index): in-bounds,
                // out-of-bounds, and non-int indices all resolve identically (a
                // value, or both engines reject).
                let n = 1 + pick(rng, 3);
                let elems: Vec<String> =
                    (0..n).map(|_| gen_expr(rng, depth - 1, vars)).collect();
                format!("([{}])[{}]", elems.join(", "), gen_expr(rng, depth - 1, vars))
            }
            7 => {
                // interpolation compared for equality (exercises Interp) — always
                // a Bool, so it composes back into the scalar grammar. Embed
                // leaves to keep the interpolated string un-nested.
                format!(
                    "(\"x{{{}}}\" == \"x{{{}}}\")",
                    gen_expr(rng, 0, vars),
                    gen_expr(rng, 0, vars)
                )
            }
            0 => {
                // includes %, /, // (zero divisors error identically on both engines)
                let op = ["+", "-", "*", "%", "/", "//"][pick(rng, 6) as usize];
                format!("({} {} {})", gen_expr(rng, depth - 1, vars), op, gen_expr(rng, depth - 1, vars))
            }
            1 => format!("(-{})", gen_expr(rng, depth - 1, vars)),
            2 => {
                let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                format!("({} {} {})", gen_expr(rng, depth - 1, vars), cop, gen_expr(rng, depth - 1, vars))
            }
            3 => {
                let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                format!(
                    "if ({} {} {}) then ({}) else ({})",
                    gen_expr(rng, depth - 1, vars),
                    cop,
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                    gen_expr(rng, depth - 1, vars),
                )
            }
            4 => {
                let name = format!("v{}", vars.len());
                let val = gen_expr(rng, depth - 1, vars);
                let mut vars2 = vars.to_vec();
                vars2.push(name.clone());
                format!("(let {} = ({}) in ({}))", name, val, gen_expr(rng, depth - 1, &vars2))
            }
            _ => format!("(-(-{}))", gen_expr(rng, depth - 1, vars)),
        }
    }

    fn run_vm(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let ast = parser::parse(toks).map_err(|_| ())?;
        let mut prog = bytecode::compile_with_types(&ast, None).map_err(|_| ())?;
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        match exec(&prog, None) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(_) => Err(()),
        }
    }

    /// A random scalar literal (incl. i64 extremes, to probe JIT vs interpreter
    /// overflow/comparison on the boundary).
    fn gen_lit(rng: &mut u64) -> String {
        match pick(rng, 3) {
            0 => format!("{}", (next(rng) % 4001) as i64 - 2000),
            1 => format!("{}.0", (next(rng) % 401) as i64 - 200),
            _ => format!("{}", [0i64, i64::MAX, i64::MIN][pick(rng, 3) as usize]),
        }
    }

    /// Like `run_vm`, but *with* the JIT enabled — so eligible functions execute
    /// as native code and are diffed against the tree-walker.
    fn run_vm_jit(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let ast = parser::parse(toks).map_err(|_| ())?;
        let mut prog = bytecode::compile_with_types(&ast, None).map_err(|_| ())?;
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        let jit = crate::jit::build(
            &ast,
            &prog.reduce_loops,
            &prog.map_kernels,
            &prog.filter_kernels,
            &prog.fused_kernels,
        );
        match exec(&prog, jit.as_ref()) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(_) => Err(()),
        }
    }

    fn run_tw(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let ast = parser::parse(toks).map_err(|_| ())?;
        let mut interp = Interp::new();
        let mut last = Value::Unit;
        for stmt in &ast {
            match interp.exec(stmt) {
                Ok(o) => last = o.value,
                Err(_) => return Err(()),
            }
        }
        Ok(format!("{}", last))
    }

    /// True iff the tree-walker rejects `src` specifically by exhausting its native
    /// call stack (the 20k `MAX_CALL_DEPTH` guard). The VM keeps call frames on the
    /// heap and accepts far deeper recursion, so a program recursing in (20k, 1M]
    /// succeeds on the VM and is rejected here — a by-design engine difference (B2),
    /// not a parity violation.
    fn tw_hit_recursion_limit(src: &str) -> bool {
        let Ok(toks) = lexer::lex(src) else { return false };
        let Ok(ast) = parser::parse(toks) else { return false };
        let mut interp = Interp::new();
        for stmt in &ast {
            if let Err(e) = interp.exec(stmt) {
                return e.message.contains("maximum recursion depth");
            }
        }
        false
    }

    /// Full pipeline (type-check → *type-directed* compile → VM), so
    /// receiver-polymorphic methods (DataFrame/Tensor column-verbs) route by the
    /// receiver's inferred type rather than falling back to the tree-walker.
    fn run_vm_typed(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let ast = parser::parse(toks).map_err(|_| ())?;
        let types = crate::types::check(&ast).map_err(|_| ())?;
        let mut prog = bytecode::compile_with_types(&ast, Some(types)).map_err(|_| ())?;
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        match exec(&prog, None) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(_) => Err(()),
        }
    }

    /// Type-directed routing: DataFrame column-verbs (`where`/`select`/`sort`/
    /// `group`) compile and run on the VM (not the tree-walker), matching the
    /// oracle. Locks in Phase 4 of the one-engine collapse.
    #[test]
    fn dataframe_column_verbs_run_on_vm() {
        let csv = "read_csv(\"examples/data/patients.csv\")";
        let cases = [
            format!("{csv}.where(age > 40).count()"),
            format!("{csv}.where(age > 40 and resting_hr < 75).count()"),
            format!("{csv}.where(age > 40).select(name, age).sort(age).count()"),
            // predicate referencing a global variable → the resolve_var path
            format!("t = 40\n{csv}.where(age > t).count()"),
            // grouped aggregation over an unevaluated column
            "read_csv(\"examples/data/genes.csv\").group(species).mean(expression).count()".to_string(),
            // in-memory dataframe() constructor + a verb on it
            "dataframe({g: [\"x\", \"x\", \"y\"], v: [1, 3, 10]}).group(@g).mean(@v).count()".to_string(),
            // the same queries with the `@column` sigil — must behave identically
            format!("{csv}.where(@age > 40).count()"),
            format!("{csv}.where(@age > 40 and @resting_hr < 75).count()"),
            format!("{csv}.where(@age > 40).select(@name, @age).sort(@age).count()"),
            format!("{csv}.with({{adult: @age >= 18}}).count()"),
            "read_csv(\"examples/data/genes.csv\").group(@species).mean(@expression).count()".to_string(),
            // `@age` is ALWAYS the column even when a local `age` shadows it — the
            // sigil's whole point. Bare `age` here would resolve the column too, but
            // `@age` *guarantees* it, with no ambiguity.
            format!("age = 999\n{csv}.where(@age > 40).count()"),
        ];
        for src in &cases {
            assert_eq!(run_vm_typed(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
        // Concrete expected values (mirrors the interpreter's own test).
        assert_eq!(run_vm_typed(&format!("{csv}.where(age > 40).count()")), Ok("5".into()));
        assert_eq!(run_vm_typed(&format!("{csv}.where(@age > 40).count()")), Ok("5".into()));
        // A local that shadows a column name does not affect `@age`.
        assert_eq!(
            run_vm_typed(&format!("age = 999\n{csv}.where(@age > 40).count()")),
            Ok("5".into())
        );
    }

    /// Reassignment/mutability now run on the VM (not via tree-walker fallback):
    /// an immutable reassignment raises the canonical error, `mut` re-declares, and
    /// a mutable reassignment updates — all matching the tree-walker.
    #[test]
    fn reassignment_matches_tree_walker_on_vm() {
        let cases = [
            "x = 1\nx = 2\nx",          // immutable reassignment → both error
            "mut x = 1\nx = 2\nx",      // mutable reassignment → 2
            "x = 1\nmut x = 2\nx",      // `mut` re-declares an immutable → 2
            "mut x = 1\nmut x = 2\nx",  // re-declare a mutable → 2
        ];
        for src in cases {
            assert_eq!(run_vm(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
    }

    /// `try` runs natively on the VM (commit landing it removed the whole-program
    /// tree-walker fallback). These cases pin the VM's handler/unwind path to the
    /// tree-walker oracle: success and error records, nested `try`, an error thrown
    /// several call frames deep (frame unwind), a per-element `try` inside `map`
    /// (the comprehension-iterator stack must unwind), `missing` (not an error), and
    /// that execution continues after a caught error.
    #[test]
    fn try_matches_tree_walker_on_vm() {
        let cases = [
            "print(try (1 + 1))",                               // ok record
            "print(try (1 / 0))",                               // caught div-by-zero
            "r = try (1 / 0)\nprint(r.ok)\nprint(r.error)",     // error fields
            "r = try (10 / 2)\nprint(r.ok)\nprint(r.value)",    // value field
            "print(try (try (1 / 0)))",                         // nested try
            "fn f(n) = if n <= 0 then 1 / 0 else f(n - 1)\nprint((try f(5)).ok)", // deep unwind
            "print([1, 0, 2].map((x) => (try (10 / x)).ok))",   // per-element try in map
            "r = try (missing)\nprint(r.ok)\nprint(r.value)",   // missing is not an error
            "x = try (1 / 0)\nprint(42)",                       // recovers, then continues
        ];
        for src in cases {
            assert_eq!(run_vm(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
    }

    /// One-engine gate: every shipped example must type-check and **compile to
    /// bytecode** — i.e. run on the VM, never fall back to the tree-walker. If a
    /// change reintroduces a fallback for an example, this fails loudly.
    #[test]
    fn examples_compile_on_the_vm() {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut checked = 0;
        // The runnable, self-contained categories (excludes modules/python/api, which
        // need imports / optional features / network).
        for cat in ["language", "numerics", "dataframes", "statistics", "bio"] {
            for entry in std::fs::read_dir(base.join(cat)).expect("category dir") {
                let path = entry.unwrap().path();
                if path.extension().and_then(|e| e.to_str()) != Some("helix") {
                    continue;
                }
                let src = std::fs::read_to_string(&path).unwrap();
                let toks = lexer::lex(&src).unwrap_or_else(|_| panic!("lex failed: {path:?}"));
                let ast = parser::parse(toks).unwrap_or_else(|_| panic!("parse failed: {path:?}"));
                let types = crate::types::check(&ast)
                    .unwrap_or_else(|_| panic!("type-check failed: {path:?}"));
                bytecode::compile_with_types(&ast, Some(types)).unwrap_or_else(|_| {
                    panic!("`{path:?}` falls back to the tree-walker — it should compile on the VM")
                });
                checked += 1;
            }
        }
        assert!(checked >= 10, "expected the full example suite, only saw {checked}");
    }

    #[test]
    fn user_range_shadow_does_not_fuse() {
        // A user `fn range` must NOT be range-fused as the builtin — the VM must agree
        // with the tree-walker (both call the user fn, then error on a method over the
        // resulting Int). Regression for the range-shadow fusion divergence.
        for src in [
            "fn range(a, b) = a + b\nrange(0, 5).reduce(0, (acc, x) => acc + x)",
            "fn range(a, b) = a + b\nrange(0, 5).filter(it > 1).count()",
        ] {
            assert_eq!(run_vm(src), run_tw(src), "engines diverge on `{src}`");
        }
        // the unshadowed builtin range still fuses correctly
        assert_eq!(
            run_vm("range(0, 100).map(it * 2).reduce(0, (a, x) => a + x)"),
            Ok("9900".to_string())
        );
    }

    #[test]
    fn differential_vm_vs_tree_walker() {
        let mut rng = 0x1234_5678_9ABC_DEF0u64;
        for _ in 0..40_000 {
            let src = gen_expr(&mut rng, 5, &[]);
            match (run_vm(&src), run_tw(&src)) {
                // both succeed → values must be identical
                (Ok(a), Ok(b)) => assert_eq!(a, b, "VALUE divergence on `{src}`"),
                // both reject → fine (we don't require identical messages)
                (Err(()), Err(())) => {}
                // The one accepted asymmetry (B2): the VM keeps frames on the heap
                // (1M-deep) while the tree-walker recurses on the native stack (20k),
                // so recursion in (20k, 1M] is VM-ok / tree-walker-rejected — by design.
                // gen_expr bounds depth well under 20k, so this is a defensive guard;
                // see `recursion_depth_is_a_by_design_engine_difference`.
                (Ok(_), Err(())) if tw_hit_recursion_limit(&src) => {}
                // one accepts, the other rejects → a real divergence
                (v, t) => panic!("OUTCOME divergence on `{src}`: vm={v:?} tw={t:?}"),
            }
        }
    }

    #[test]
    fn differential_functions_with_jit() {
        let mut rng = 0xFEED_FACE_DEAD_BEEFu64;
        let params = vec!["a".to_string(), "b".to_string()];
        for _ in 0..10_000 {
            // a non-recursive function over (a, b), called with random scalars —
            // exercises CallFn + the i64/f64 JIT specializations against the
            // tree-walker. (Non-recursive ⇒ no native-stack risk.)
            let body = gen_expr(&mut rng, 4, &params);
            let src = format!(
                "fn f(a, b) = {}\nf({}, {})",
                body,
                gen_lit(&mut rng),
                gen_lit(&mut rng)
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "JIT/VM ≠ tree-walker on `{src}`"),
                (Err(()), Err(())) => {}
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// The native reduce loop (`TryJitReduce`) must equal the tree-walker. Drives
    /// `range(s, e).reduce(init, (acc, x) => body)` through the JIT-enabled runner
    /// with random `i64`-eligible bodies over `{acc, x}`, random (incl. negative
    /// and empty) ranges, and `Int`/`Float`/extreme inits — so the native path,
    /// the cap/float fall-throughs, and overflow wrapping are all diffed.
    #[test]
    fn differential_reduce_loops_with_jit() {
        let mut rng = 0x0DDC_0FFE_EBAD_F00Du64;
        let binders = vec!["acc".to_string(), "x".to_string()];
        for _ in 0..10_000 {
            // A body over {acc, x}: when it stays in {ints, + - *, comparisons in
            // `if`, let} it is JIT-eligible (native path); otherwise (floats, /, %)
            // the guard falls back to the bytecode loop — both are diffed here.
            let body = gen_expr(&mut rng, 3, &binders);
            // Range bounds kept modest so the loop is cheap; `% 600 - 200` spans
            // negative (empty), zero, and positive lengths.
            let start = (next(&mut rng) % 600) as i64 - 200;
            let end = (next(&mut rng) % 600) as i64 - 200;
            let init = gen_lit(&mut rng);
            let src = format!(
                "(range({}, {})).reduce({}, (acc, x) => ({}))",
                start, end, init, body
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "reduce JIT ≠ tree-walker on `{src}`"),
                (Err(()), Err(())) => {}
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// A JIT-**eligible** `i64` expression over `atoms`: only `+ - *`, the inline scalar
    /// builtins (`min`/`max`/`abs`), int literals, and the atoms — so a fold body built
    /// from it genuinely compiles to the native tuple kernel (no float/div/bitwise to make
    /// it fall back). All ops are total (wrapping `i64`, no `/0`/`%0`), so it never errors.
    fn gen_i64_eligible(rng: &mut u64, depth: u32, atoms: &[String]) -> String {
        if depth == 0 || pick(rng, 3) == 0 {
            return match pick(rng, 3) {
                0 if !atoms.is_empty() => atoms[pick(rng, atoms.len() as u64) as usize].clone(),
                _ => format!("{}", (next(rng) % 41) as i64 - 20),
            };
        }
        let op = pick(rng, 6);
        let lhs = gen_i64_eligible(rng, depth - 1, atoms);
        if op == 5 {
            return format!("abs({})", lhs);
        }
        let rhs = gen_i64_eligible(rng, depth - 1, atoms);
        match op {
            0 => format!("(({}) + ({}))", lhs, rhs),
            1 => format!("(({}) - ({}))", lhs, rhs),
            2 => format!("(({}) * ({}))", lhs, rhs),
            3 => format!("min(({}), ({}))", lhs, rhs),
            _ => format!("max(({}), ({}))", lhs, rhs),
        }
    }

    /// The **fold-JIT** in action: a 2-tuple `i64` accumulator whose components are
    /// JIT-eligible, so `run_vm_jit` engages the native multi-slot reduce kernel. Asserts
    /// JIT == tree-walker bit-for-bit across 10k random folds — the codegen's safety net.
    #[test]
    fn differential_tuple_reduce_jit() {
        let mut rng = 0xB01D_FACE_1234_5678u64;
        let atoms = vec!["a[0]".to_string(), "a[1]".to_string(), "x".to_string()];
        for _ in 0..10_000 {
            let e0 = gen_i64_eligible(&mut rng, 3, &atoms);
            let e1 = gen_i64_eligible(&mut rng, 3, &atoms);
            let start = (next(&mut rng) % 400) as i64 - 100;
            let end = (next(&mut rng) % 400) as i64 - 100;
            let i0 = (next(&mut rng) % 41) as i64 - 20;
            let i1 = (next(&mut rng) % 41) as i64 - 20;
            let src = format!(
                "(range({}, {})).reduce(({}, {}), (a, x) => ({}, {}))",
                start, end, i0, i1, e0, e1
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "tuple reduce JIT ≠ tree-walker on `{src}`"),
                (Err(()), Err(())) => {}
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// Oracle foundation for the **fold-JIT** (the multi-slot, record/tuple-accumulator
    /// reduce — ADR-pending). A 2-tuple `i64` accumulator `a = (a[0], a[1])` folded over a
    /// range, each component an independent random `i64` expression over `{a[0], a[1], x}`.
    /// Today this runs on the bytecode loop (no JIT for tuple accumulators yet), so this
    /// pins **VM == tree-walker** bit-for-bit — the safety net the JIT codegen will plug
    /// into. When the tuple-fold JIT lands, `run_vm_jit` engages the native kernel and the
    /// SAME assertion becomes JIT == tree-walker, with no test change. (Divergent
    /// components — floats, `/`, `%` — error or fall back identically on both engines.)
    #[test]
    fn differential_tuple_reduce_vm_vs_tree_walker() {
        let mut rng = 0x7_0FDA_C115_EED1u64;
        let atoms = vec!["a[0]".to_string(), "a[1]".to_string(), "x".to_string()];
        for _ in 0..10_000 {
            let e0 = gen_expr(&mut rng, 3, &atoms);
            let e1 = gen_expr(&mut rng, 3, &atoms);
            let start = (next(&mut rng) % 600) as i64 - 200;
            let end = (next(&mut rng) % 600) as i64 - 200;
            let i0 = gen_lit(&mut rng);
            let i1 = gen_lit(&mut rng);
            let src = format!(
                "(range({}, {})).reduce(({}, {}), (a, x) => ({}, {}))",
                start, end, i0, i1, e0, e1
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "tuple reduce VM ≠ tree-walker on `{src}`"),
                (Err(()), Err(())) => {}
                (v, t) => panic!("OUTCOME divergence on `{src}`: vm={v:?} tw={t:?}"),
            }
        }
    }

    /// Run `src` on the VM with the JIT *disabled* (no native kernels) — the pure
    /// bytecode-loop oracle that `run_vm_jit` is diffed against for the array/fused
    /// kernels (which only engage when a `Jit` is supplied).
    fn run_vm_no_jit(src: &str) -> Result<String, ()> {
        run_vm(src)
    }

    /// A random JIT-eligible `Int`-array pipeline: an `Int` source (`range(s,e)` or a
    /// small int-array literal) followed by 0–3 `.map(it OP k)` / `.filter(it CMP k)`
    /// stages and a scalar terminal (`.reduce` / `.sum()` / `.count()`). The body ops
    /// stay in `{+,-,*}` and comparisons so the map/filter/fused kernels are genuinely
    /// JIT-compiled (the native path), not falling back.
    fn gen_int_pipeline(rng: &mut u64) -> String {
        // A small literal, or — occasionally — a value past 2^53 where the interpreter's
        // `as_f64()`-based `min`/`max` compare loses precision (so a naive native integer
        // compare would diverge): the JIT must mirror the lossy compare exactly.
        fn lit(rng: &mut u64) -> i64 {
            match pick(rng, 6) {
                0 => 9_007_199_254_740_993, // 2^53 + 1
                1 => -9_007_199_254_740_993,
                _ => (next(rng) % 21) as i64 - 10,
            }
        }
        // 0–3 captured `i64` globals (`c0 = …`). A map body sometimes uses a capture
        // instead of a literal, exercising the captured-var kernel path.
        let n_caps = pick(rng, 4) as usize;
        let mut preamble = String::new();
        for i in 0..n_caps {
            preamble.push_str(&format!("c{} = {}\n", i, lit(rng)));
        }
        // An operand is a literal or (when any exist) a captured variable.
        fn operand(rng: &mut u64, n_caps: usize) -> String {
            if n_caps > 0 && pick(rng, 2) == 0 {
                format!("c{}", pick(rng, n_caps as u64))
            } else {
                format!("{}", lit(rng))
            }
        }
        // A map body over `it`: a bare arithmetic op, or a pure scalar builtin
        // (`abs`/`min`/`max`) the kernel now compiles inline.
        fn map_body(rng: &mut u64, n_caps: usize) -> String {
            let op = ["+", "-", "*"][pick(rng, 3) as usize];
            match pick(rng, 5) {
                0 => format!("abs(it {} {})", op, operand(rng, n_caps)),
                1 => format!("min(it, {})", operand(rng, n_caps)),
                2 => format!("max(it, {})", operand(rng, n_caps)),
                _ => format!("it {} {}", op, operand(rng, n_caps)),
            }
        }
        let src = if pick(rng, 2) == 0 {
            let s = (next(rng) % 40) as i64 - 10;
            let e = s + (next(rng) % 60) as i64; // non-negative length, within cap
            format!("range({}, {})", s, e)
        } else {
            let n = 1 + pick(rng, 6);
            let elems: Vec<String> = (0..n).map(|_| format!("{}", lit(rng))).collect();
            format!("[{}]", elems.join(", "))
        };
        let mut chain = src;
        let stages = pick(rng, 4); // 0..3 transform stages
        for _ in 0..stages {
            if pick(rng, 2) == 0 {
                chain = format!("({}).map(it => {})", chain, map_body(rng, n_caps));
            } else {
                let cmp = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                chain = format!("({}).filter(it {} {})", chain, cmp, operand(rng, n_caps));
            }
        }
        let terminal = match pick(rng, 3) {
            0 => format!("({}).reduce({}, (acc, x) => acc + x)", chain, lit(rng)),
            1 => format!("({}).sum()", chain),
            _ => format!("({}).count()", chain),
        };
        format!("{preamble}{terminal}")
    }

    /// A random `f64`-map pipeline: a `Float`-array literal source followed by 1–3
    /// `.map(it OP operand)` stages with `OP ∈ {+,-,*}` and a float operand that is
    /// either a literal or a captured `f64` global. Every stage is `f64`-kernel
    /// eligible (uses the binder, no `/`, no comparison/`if`/call), so a `Float` source
    /// drives the native `f64` kernel — the path the oracle below must hold identical to
    /// the bytecode VM and the tree-walker, bit-for-bit (Cranelift `fadd/fsub/fmul` are
    /// the same SSE scalar ops the interpreter runs, in the same left-to-right order).
    fn gen_float_map_pipeline(rng: &mut u64) -> String {
        fn flit(rng: &mut u64) -> String {
            let whole = (next(rng) % 21) as i64 - 10;
            let frac = next(rng) % 1000;
            format!("{}.{:03}", whole, frac)
        }
        let n_caps = pick(rng, 3) as usize; // 0–2 captured f64 globals
        let mut preamble = String::new();
        for i in 0..n_caps {
            preamble.push_str(&format!("c{} = {}\n", i, flit(rng)));
        }
        fn operand(rng: &mut u64, n_caps: usize) -> String {
            if n_caps > 0 && pick(rng, 2) == 0 {
                format!("c{}", pick(rng, n_caps as u64))
            } else {
                flit(rng)
            }
        }
        // An f64 map body: a bare `{+,-,*}`, or a pure float builtin the kernel emits
        // inline (`sqrt`/`abs`/`min`/`max`). A `sqrt` of a negative value yields `NaN` —
        // both engines render it identically, so it's a valid differential case.
        fn map_body(rng: &mut u64, n_caps: usize) -> String {
            let op = ["+", "-", "*"][pick(rng, 3) as usize];
            match pick(rng, 6) {
                0 => format!("sqrt(it {} {})", op, operand(rng, n_caps)),
                1 => format!("abs(it {} {})", op, operand(rng, n_caps)),
                2 => format!("min(it, {})", operand(rng, n_caps)),
                3 => format!("max(it, {})", operand(rng, n_caps)),
                _ => format!("it {} {}", op, operand(rng, n_caps)),
            }
        }
        let n = 1 + pick(rng, 6);
        let elems: Vec<String> = (0..n).map(|_| flit(rng)).collect();
        let mut chain = format!("[{}]", elems.join(", "));
        for _ in 0..(1 + pick(rng, 3)) {
            chain = format!("({}).map(it => {})", chain, map_body(rng, n_caps));
        }
        format!("{preamble}{chain}")
    }

    /// A random **mixed** `Int`-source → `Float`-body map: an `Int` array literal (with
    /// some large values, to exercise `i64` wrap inside integer subexpressions before the
    /// float promotion) or a `range`, mapped by a `{+,-,*}` body over the binder and int /
    /// float literals whose root is `Float` (forced by a trailing `* <float>`). The body's
    /// integer subexpressions must wrap as `i64` and convert to `f64` only at the first
    /// float operand — exactly what the mixed kernel's node-by-node typing reproduces.
    fn gen_mixed_map_pipeline(rng: &mut u64) -> String {
        // An int literal that is *sometimes* large enough that a product wraps `i64`.
        fn ilit(rng: &mut u64) -> String {
            match pick(rng, 3) {
                0 => format!("{}", (next(rng) % 21) as i64 - 10),
                1 => format!("{}", 1_000_000_000_i64 + (next(rng) % 1000) as i64),
                _ => "3000000000".to_string(),
            }
        }
        fn flit(rng: &mut u64) -> String {
            format!("{}.{:03}", (next(rng) % 21) as i64 - 10, next(rng) % 1000)
        }
        let src = if pick(rng, 2) == 0 {
            let s = (next(rng) % 40) as i64 - 10;
            let e = s + (next(rng) % 60) as i64;
            format!("range({}, {})", s, e)
        } else {
            let n = 1 + pick(rng, 6);
            let elems: Vec<String> = (0..n).map(|_| ilit(rng)).collect();
            format!("[{}]", elems.join(", "))
        };
        // Build a `{+,-,*}` chain over `it` and int/float operands, then force a `Float`
        // root with a trailing `* <float>` so the map is mixed-eligible.
        let mut body = "it".to_string();
        for _ in 0..(1 + pick(rng, 3)) {
            let op = ["+", "-", "*"][pick(rng, 3) as usize];
            let opd = if pick(rng, 2) == 0 { ilit(rng) } else { flit(rng) };
            body = format!("({} {} {})", body, op, opd);
        }
        let core = format!("({} * {})", body, flit(rng)); // Float root
        let int_core = format!("(it {} {})", ["+", "-", "*"][pick(rng, 3) as usize], ilit(rng)); // Int
        // Optionally wrap in a pure builtin — `sqrt`/`abs`/`min`/`max` — exercising the
        // mixed kernel's Int→Float promotion (`sqrt`/`min` of an `Int` subexpression too).
        let wrapped = match pick(rng, 7) {
            0 => format!("sqrt(abs({}))", core),
            1 => format!("abs({})", core),
            2 => format!("min({}, {})", core, flit(rng)),
            3 => format!("max({}, {})", core, flit(rng)),
            4 => format!("sqrt({})", int_core), // sqrt(Int) → Float (tests fcvt in sqrt)
            5 => format!("sqrt(min({}, {}))", int_core, next(rng) % 30), // i64 min then sqrt
            _ => core,
        };
        format!("({}).map(it => {})", src, wrapped)
    }

    /// Triple oracle for the **mixed** `Int`→`Float` map kernel. The int-array-literal
    /// sources guarantee an `Int` receiver that actually drives the mixed kernel (not a
    /// fall-through), so a mis-placed `fcvt` or an integer op done in `f64` (losing the
    /// `i64` wrap) would diverge here.
    #[test]
    fn differential_mixed_map_kernel_oracle() {
        let mut rng = 0x3117_C0DE_BEEF_2026u64;
        for _ in 0..15_000 {
            let src = gen_mixed_map_pipeline(&mut rng);
            let jit = run_vm_jit(&src);
            let no_jit = run_vm_no_jit(&src);
            let tw = run_tw(&src);
            match (jit, no_jit, tw) {
                (Ok(a), Ok(b), Ok(c)) => {
                    assert_eq!(a, b, "mixed map: JIT ≠ bytecode VM on `{src}`");
                    assert_eq!(b, c, "mixed map: bytecode VM ≠ tree-walker on `{src}`");
                }
                (Err(()), Err(()), Err(())) => {}
                (j, n, t) => panic!("OUTCOME divergence on `{src}`: jit={j:?} nojit={n:?} tw={t:?}"),
            }
        }
    }

    /// Triple oracle for the `f64` map kernel: a random `Float`-array map pipeline must
    /// render the *same* array on the JIT-native path, the pure bytecode loop, and the
    /// tree-walker. Guards the monomorphized `f64` kernel + its `f64`-capture passing.
    #[test]
    fn differential_float_map_kernel_oracle() {
        let mut rng = 0xF10A_7C0D_E5EE_9001u64;
        for _ in 0..15_000 {
            let src = gen_float_map_pipeline(&mut rng);
            let jit = run_vm_jit(&src);
            let no_jit = run_vm_no_jit(&src);
            let tw = run_tw(&src);
            match (jit, no_jit, tw) {
                (Ok(a), Ok(b), Ok(c)) => {
                    assert_eq!(a, b, "f64 map: JIT ≠ bytecode VM on `{src}`");
                    assert_eq!(b, c, "f64 map: bytecode VM ≠ tree-walker on `{src}`");
                }
                (Err(()), Err(()), Err(())) => {}
                (j, n, t) => panic!("OUTCOME divergence on `{src}`: jit={j:?} nojit={n:?} tw={t:?}"),
            }
        }
    }

    /// A random `match`-bearing user function (`int` scrutinee: `Int` / `Or`-of-`Int` /
    /// guarded-binder / `_` arms, the last always a catch-all) applied per element through
    /// a map→reduce/sum. Exercises the JIT's `match` lowering ([`gen_match`]) against the
    /// VM and tree-walker — arm order, or-patterns, guards, and binder scoping must agree.
    fn gen_match_pipeline(rng: &mut u64) -> String {
        fn lit(rng: &mut u64) -> i64 {
            (next(rng) % 21) as i64 - 10
        }
        let k = 2 + pick(rng, 8) as i64; // scrutinee modulus → m in 0..k
        // A small int body that may reference the param `m` (always in scope).
        fn body(rng: &mut u64) -> String {
            if pick(rng, 2) == 0 {
                format!("{}", lit(rng))
            } else {
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                format!("(m {} {})", op, lit(rng))
            }
        }
        let mut arms = String::new();
        for _ in 0..(1 + pick(rng, 4)) {
            match pick(rng, 3) {
                0 => arms.push_str(&format!("  {} => {},\n", pick(rng, k as u64), body(rng))),
                1 => {
                    let n = 2 + pick(rng, 2);
                    let alts: Vec<String> = (0..n).map(|_| format!("{}", pick(rng, k as u64))).collect();
                    arms.push_str(&format!("  {} => {},\n", alts.join(" | "), body(rng)));
                }
                _ => {
                    let cmp = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                    arms.push_str(&format!("  x if x {} {} => {},\n", cmp, lit(rng), body(rng)));
                }
            }
        }
        arms.push_str(&format!("  _ => {},\n", body(rng)));
        let preamble = format!("fn fm(m) = match m {{\n{}}}\n", arms);
        let src = if pick(rng, 2) == 0 {
            let s = (next(rng) % 20) as i64;
            format!("range({}, {})", s, s + (next(rng) % 50) as i64)
        } else {
            let n = 1 + pick(rng, 6);
            let elems: Vec<String> = (0..n).map(|_| format!("{}", next(rng) % 30)).collect();
            format!("[{}]", elems.join(", "))
        };
        let mapped = format!("({}).map(it => fm(it % {}))", src, k);
        let terminal = if pick(rng, 2) == 0 {
            format!("({}).reduce(0, (a, x) => a + x)", mapped)
        } else {
            format!("({}).sum()", mapped)
        };
        format!("{preamble}{terminal}")
    }

    #[test]
    fn match_kernel_matches() {
        // The S4 benchmark function: literal, or-pattern, literal, guarded binder, wildcard.
        let weight = "fn weight(m) = match m {\n  0 => 7,\n  1 | 2 | 3 => 2,\n  11 => 5,\n  x if x > 7 => 3,\n  _ => 1,\n}\n";
        let src = format!("{weight}(0..40).map(k => weight(k % 12)).reduce(0, (a, x) => a + x)");
        let j = run_vm_jit(&src).expect("jit");
        assert_eq!(j, run_vm_no_jit(&src).expect("vm"));
        assert_eq!(j, run_tw(&src).expect("tw"));
    }

    #[test]
    fn differential_match_kernel_oracle() {
        let mut rng = 0x6A7C_4ECD_0FF1_CE42u64;
        for _ in 0..15_000 {
            let src = gen_match_pipeline(&mut rng);
            let jit = run_vm_jit(&src);
            let no_jit = run_vm_no_jit(&src);
            let tw = run_tw(&src);
            match (jit, no_jit, tw) {
                (Ok(a), Ok(b), Ok(c)) => {
                    assert_eq!(a, b, "match: JIT ≠ bytecode VM on `{src}`");
                    assert_eq!(b, c, "match: bytecode VM ≠ tree-walker on `{src}`");
                }
                (Err(()), Err(()), Err(())) => {}
                (j, n, t) => panic!("OUTCOME divergence on `{src}`: jit={j:?} nojit={n:?} tw={t:?}"),
            }
        }
    }

    /// Triple oracle for the JIT array/fused kernels: every random `Int`-array
    /// pipeline must produce the *same* result on the JIT-native path, the pure
    /// bytecode loop, and the tree-walker. This closes the one confirmed fuzzing gap
    /// — the map/filter/fused kernels were previously only exercised by hand. Any
    /// divergence (a mis-compiled kernel, an off-by-one, a wrap mismatch) fails here.
    #[test]
    fn captured_var_map_kernel_matches() {
        // A map body capturing outer `i64` variables now compiles to a native kernel
        // (passed the captures as loop-invariant args); it must agree with the bytecode
        // VM and the tree-walker. 0..6 of x*3+7 = 7,10,13,16,19,22 → sum 87.
        let src = "k = 7\nm = 3\nrange(0, 6).map(x => x * m + k).sum()";
        let jit = run_vm_jit(src).expect("jit");
        assert_eq!(jit, run_vm_no_jit(src).expect("vm"));
        assert_eq!(jit, run_tw(src).expect("tw"));
        assert_eq!(jit, "87");
        // a float capture is not i64-closed → falls through identically on all engines
        let f = "k = 1.5\n[1, 2, 3].map(x => x + k).sum()";
        assert_eq!(run_vm_jit(f).expect("jit"), run_tw(f).expect("tw"));
    }

    #[test]
    fn jit_scalar_builtins_match() {
        // abs/min/max in a map body compile to native code; must agree on all engines.
        for src in [
            "[-3, 5, -7, 2].map(x => abs(x)).sum()",
            "[1, 9, 4, 2].map(x => min(x, 3)).sum()",
            "[1, 9, 4, 2].map(x => max(x, 3)).sum()",
            "range(0, 6).reduce(-100, (a, x) => max(a, x))", // running max
            "[5, 1, 8, 2].reduce(999, (a, x) => min(a, x))", // running min
            // abs(i64::MIN) wraps to i64::MIN (wrapping_abs), matching the kernel's iabs.
            "[-9223372036854775808].map(x => abs(x)).sum()",
            // min/max past 2^53: the interpreter compares via f64 (lossy) and returns the
            // original operand — the kernel mirrors that, so both pick the same element.
            "[9007199254740993].map(x => min(x, 9007199254740992)).sum()",
        ] {
            let j = run_vm_jit(src).expect("jit");
            assert_eq!(j, run_vm_no_jit(src).expect("vm"), "JIT≠VM on `{src}`");
            assert_eq!(j, run_tw(src).expect("tw"), "JIT≠tw on `{src}`");
        }
        // A user function shadowing `max` must dispatch to the user's fn, NOT the builtin
        // op — all engines agree (and the JIT must not silently emit the builtin).
        let shadow = "fn max(a, b) = a + b\n[1, 2, 3].map(x => max(x, 10)).sum()";
        let j = run_vm_jit(shadow).expect("jit");
        assert_eq!(j, run_vm_no_jit(shadow).expect("vm"));
        assert_eq!(j, run_tw(shadow).expect("tw"));
        assert_eq!(j, "36"); // (1+10)+(2+10)+(3+10) = 36, the user's `+`, not max
    }

    #[test]
    fn f64_map_kernel_matches() {
        // A `Float`-array map with `{+,-,*}` and a captured `f64` drives the native f64
        // kernel; it must agree with the bytecode VM and tree-walker bit-for-bit.
        let src = "k = 0.5\n[1.0, 2.0, 3.0].map(x => x * 2.0 + k)";
        let jit = run_vm_jit(src).expect("jit");
        assert_eq!(jit, run_vm_no_jit(src).expect("vm"));
        assert_eq!(jit, run_tw(src).expect("tw"));
        // chained float maps each run as a standalone f64 kernel (floats don't fuse)
        let chained = "[1.5, 2.5].map(x => x + 1.0).map(x => x * 3.0)";
        assert_eq!(
            run_vm_jit(chained).expect("jit"),
            run_tw(chained).expect("tw")
        );
        // inline float builtins: sqrt (incl. NaN on negatives), abs, min, max
        for src in [
            "[1.0, 4.0, 9.0].map(x => sqrt(x))",
            "[-2.5, 3.5].map(x => abs(x))",
            "[1.5, 9.0, 4.0].map(x => min(x, 3.0))",
            "[1.5, 9.0, 4.0].map(x => max(x, 3.0))",
            "[-1.0, 2.0].map(x => sqrt(x))", // sqrt(-1) = NaN on both engines
            "[2.0, 5.0].map(x => sqrt(x * x + 1.0))",
        ] {
            assert_eq!(run_vm_jit(src).expect("jit"), run_tw(src).expect("tw"), "on `{src}`");
        }
        // a user function shadowing `sqrt` must NOT be replaced by the builtin op
        let shadow = "fn sqrt(x) = x + 1.0\n[1.0, 2.0].map(x => sqrt(x))";
        assert_eq!(run_vm_jit(shadow).expect("jit"), run_tw(shadow).expect("tw"));
    }

    #[test]
    fn mixed_map_kernel_matches() {
        // Int source, float body → the mixed (i64→f64) kernel; must match all engines.
        let src = "[1, 2, 3].map(it => it * 0.5)";
        let jit = run_vm_jit(src).expect("jit");
        assert_eq!(jit, run_vm_no_jit(src).expect("vm"));
        assert_eq!(jit, run_tw(src).expect("tw"));
        // An integer subexpression must wrap as i64 *before* the float promotion: with the
        // mixed kernel typing each node, `it * 3000000000` is an i64 product (wraps for the
        // 5e9 element) and only the trailing `* 1.0` lifts to f64 — doing the whole thing in
        // f64 would NOT wrap and would diverge. Held identical across all three engines.
        let wrap = "[5000000000].map(it => (it * 3000000000) * 1.0)";
        let j = run_vm_jit(wrap).expect("jit");
        assert_eq!(j, run_vm_no_jit(wrap).expect("vm"));
        assert_eq!(j, run_tw(wrap).expect("tw"));
    }

    #[test]
    fn differential_array_kernels_triple_oracle() {
        let mut rng = 0xA11C_E0FF_EE00_1234u64;
        for _ in 0..15_000 {
            let src = gen_int_pipeline(&mut rng);
            let jit = run_vm_jit(&src);
            let no_jit = run_vm_no_jit(&src);
            let tw = run_tw(&src);
            match (jit, no_jit, tw) {
                (Ok(a), Ok(b), Ok(c)) => {
                    assert_eq!(a, b, "JIT ≠ bytecode VM on `{src}`");
                    assert_eq!(b, c, "bytecode VM ≠ tree-walker on `{src}`");
                }
                (Err(()), Err(()), Err(())) => {}
                (j, n, t) => panic!("OUTCOME divergence on `{src}`: jit={j:?} nojit={n:?} tw={t:?}"),
            }
        }
    }

    /// Run the random `gen_expr` programs through the *full* typed pipeline
    /// (`parse → typecheck → type-directed compile → VM`) and diff against the
    /// tree-walker. This exercises the type checker and typed compiler end-to-end
    /// (the other fuzzers bypass them via `compile_with_types(ast, None)`). The type
    /// checker is conservative, so it may reject a program the dynamic tree-walker
    /// would run — that asymmetry is allowed; what must hold is that whenever the
    /// typed VM *does* run, its value matches, and it never panics.
    #[test]
    fn differential_typed_pipeline_vs_tree_walker() {
        let mut rng = 0x7401_DEAD_C0DE_5A5Au64;
        for _ in 0..40_000 {
            let src = gen_expr(&mut rng, 5, &[]);
            match (run_vm_typed(&src), run_tw(&src)) {
                // both run → values must agree
                (Ok(a), Ok(b)) => assert_eq!(a, b, "typed VM ≠ tree-walker on `{src}`"),
                // both reject → fine
                (Err(()), Err(())) => {}
                // checker stricter than the dynamic tree-walker → allowed
                (Err(()), Ok(_)) => {}
                // typed VM ran but tree-walker rejected: only legitimate via the B2
                // recursion-depth difference; anything else is a real divergence.
                (Ok(_), Err(())) if tw_hit_recursion_limit(&src) => {}
                (v, t) => panic!("OUTCOME divergence on `{src}`: typed={v:?} tw={t:?}"),
            }
        }
    }

    /// First-class function values (`MakeFunc`/`CallValue`): lambdas and
    /// function-name aliases stored in variables and called, higher-order calls,
    /// free-vars-as-globals, function rendering, and the error paths. The VM must
    /// match the tree-walker on every shape. (These run the raw engines without the
    /// type checker, which in production additionally gates higher-order/capture.)
    #[test]
    fn first_class_functions_match_tree_walker() {
        let cases: &[&str] = &[
            "add = (a, b) => a + b\nadd(2, 3)",            // lambda → global, called
            "k = 10\nf = p => p + k\nf(5)",                // free var resolves to global
            "fn dbl(x) = x * 2\nh = dbl\nh(7)",            // bare fn name aliased + called
            "twice = n => n * 2\ntwice(twice(3))",         // nested application
            "fn apply(f, x) = f(x)\napply(p => p * 2, 5)", // higher-order (lambda arg)
            "fn dbl(x) = x * 2\nfn apply(f, x) = f(x)\napply(dbl, 5)", // higher-order (named)
            "add = (a, b) => a + b\n\"f={add}\"",          // function rendered in a string
            "fn dbl(x) = x * 2\nh = dbl\nh(1, 2)",         // error: wrong arity (both reject)
            "x = 5\nx(3)",                                  // error: calling a non-function
        ];
        for src in cases {
            match (run_vm(src), run_tw(src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "VM ≠ tree-walker on `{src}`"),
                (Err(()), Err(())) => {}
                (v, t) => panic!("divergence on `{src}`: vm={v:?} tw={t:?}"),
            }
        }
    }

    /// Robustness: random source over Helix-meaningful characters must never
    /// *panic* the lexer/parser/checker — only ever produce a value or a clean
    /// error. (Catches missing depth guards, index panics, etc.)
    #[test]
    fn parser_never_panics_on_random_input() {
        const CHARS: &[u8] = b"0123456789+-*/%()[]{}.,:<>=! \"abcxy_\nif then else let in fn mut and or not";
        let mut rng = 0xCAFE_F00D_1234_5678u64;
        for _ in 0..20_000 {
            let len = (next(&mut rng) % 60) as usize;
            let s: String = (0..len)
                .map(|_| CHARS[(next(&mut rng) % CHARS.len() as u64) as usize] as char)
                .collect();
            // Must return Ok or Err — never unwind. A panic fails the test.
            if let Ok(toks) = lexer::lex(&s)
                && let Ok(ast) = parser::parse(toks) {
                    let _ = crate::types::check(&ast);
                }
        }
    }

    /// The VM must be observationally identical to the tree-walker.
    fn assert_parity(src: &str) {
        assert_eq!(
            format!("{}", vm_val(src)),
            format!("{}", tw_val(src)),
            "VM and tree-walker disagree on: {src}"
        );
    }

    #[test]
    fn parity_scalar_and_control_flow() {
        for src in [
            "1 + 2 * 3 - 4",
            "2 ** 10",
            "7 % 3",
            "10 / 4",
            "-5 + 3",
            "not true",
            "1 < 2",
            "3 >= 3",
            "2 == 2",
            "2 != 3",
            "if 3 > 2 then 10 else 20",
            "if false then 1 else if true then 2 else 3",
            "true and false",
            "true or false",
            "false and missing",  // short-circuit: determined false
            "true or missing",    // short-circuit: determined true
            "missing and true",   // three-valued
            "missing ?? 42",
            "5 ?? 42",
            "let x = 10, y = x + 5 in x * y",
            "let a = 1 in let b = 2 in a + b",
            "[1, 2, 3][1]",
            "[10, 20, 30, 40][-1]",
            "[1 + 1, 2 * 3, 4][2]",
            "let xs = [5, 6, 7] in xs[0] + xs[2]",
            "let n = 42 in \"answer is {n}\"",
            "let x = 3, y = 4 in \"{x} + {y} = {x + y}\"",
            "3.5 + 1.5",
            "sqrt(144.0)",
            "abs(-7)",
            "max(3, 9)",
            "min(3, 9)",
            "pi > 3.0",
        ] {
            assert_parity(src);
        }
    }

    #[test]
    fn parity_comprehensions() {
        for src in [
            "[1, 2, 3, 4].map(it * 2)",
            "[1, 2, 3, 4, 5].filter(it > 2)",
            "[1, 2, 3, 4, 5, 6].where(it % 2 == 0)",
            "[1, 2, 3, 4].reduce(0, (acc, x) => acc + x)",
            "[1, 2, 3, 4].reduce(1, (acc, x) => acc * x)",
            "range(10).map(it * it)",
            // `a..b` range literals desugar to `range(a, b)` — must match exactly
            "(0..5).map(it * 2)",
            "(1..4).reduce(0, (acc, x) => acc + x)",
            "(0..10).filter(it % 3 == 0).reduce(0, (acc, x) => acc + x)",
            "let n = 50 in (0..n).reduce(0, (acc, x) => acc + x)",
            "(0..3 + 1).count()",
            "(2..2).count()",
            "range(20).filter(it % 3 == 0).reduce(0, (acc, x) => acc + x)",
            // fused range reductions (no array materialized)
            "range(100).reduce(0, (acc, x) => acc + x)",
            "range(5, 15).reduce(1, (acc, x) => acc + x)",
            "let n = 50 in range(n).reduce(0, (acc, x) => acc + x)",
            "range(3, 3).reduce(99, (acc, x) => acc + x)",
            // named binder
            "[5, 10, 15].map(x => x + 1)",
            // nested comprehensions
            "[[1, 2], [3, 4]].map(row => row.reduce(0, (a, b) => a + b))",
            // body uses an outer variable
            "let k = 100 in [1, 2, 3].map(it + k)",
            // missing propagation
            "missing.map(it + 1)",
            // chained
            "range(8).map(it * 2).filter(it > 5).reduce(0, (a, x) => a + x)",
            // any / all, including short-circuit + missing three-valued logic
            "[1, 2, 3, 4].any(it > 3)",
            "[1, 2, 3, 4].all(it > 0)",
            "[1, 2, 3, 4].all(it > 2)",
            "[1, 2, 3].any(it > 10)",
            "range(100).any(it == 42)",
            "[1, missing, 3].any(it > 5)",
            "[1, missing, 3].all(it > 0)",
            "[1, missing, 3].any(it > 0)",
            "missing.all(it > 0)",
        ] {
            assert_parity(src);
        }
    }

    #[test]
    fn parity_value_methods_and_destructuring() {
        for src in [
            // array value-methods
            "[1, 2, 3, 4].sum()",
            "[10, 20, 30].mean()",
            "[3, 1, 2].sort()",
            "[1, 2, 3].count()",
            "[5, 1, 9, 2].max()",
            "[1, 2, 3, 4, 5].take(2)",
            "[1, 2, 3].reverse()",
            "[3, 1, 3, 2, 1, 3].unique()",
            "[3, 1, 3, 2, 1, 3].frequencies()",
            "[\"AT\", \"TG\", \"AT\"].frequencies()",
            // string + dna value-methods
            "\"helix\".upper()",
            "dna(\"ATGCGC\").gc_content()",
            "dna(\"ATGATG\").find(\"GAT\")",
            "dna(\"ATGC\").kmers(2)",
            // kmers (ACGT-only spectrum) vs windows (faithful) must agree across engines
            "dna(\"ATGNCC\").kmers(2)",
            "dna(\"ATGNCC\").windows(2)",
            "dna(\"AT\").kmers(5)",
            "dna(\"ATGCATGC\").kmer_counts(3)",
            // DNA ordering (`<`/sort) must agree across engines
            "dna(\"ATG\") < dna(\"CAT\")",
            "[dna(\"CAT\"), dna(\"ATG\")].sort().first()",
            // `missing` propagates through method calls (except is_missing) on both engines
            "missing.upper()",
            "missing.no_such_method()",
            "missing.phred().mean()",
            "missing.is_missing()",
            // destructuring + field/index, all on the VM
            "a, b = (3, 4)\na * b",
            "p, q, r = [1, 2, 3]\np + q + r",
            "{x: 7, y: 8}.x + {x: 7, y: 8}.y",
        ] {
            assert_parity(src);
        }
    }

    #[test]
    fn parity_functions_and_recursion() {
        assert_parity("fn sq(x) = x * x\nsq(9)");
        assert_parity("fn fib(n) = if n < 2 then n else fib(n - 1) + fib(n - 2)\nfib(15)");
        assert_parity("fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\nfact(10)");
        assert_parity("fn add(a, b) = a + b\nadd(40, 2)");
        assert_parity("mut acc = 0\nacc = acc + 100\nacc * 2");
        // Named arguments + defaults desugar at parse time, so both engines see the
        // same positional call — agreement by construction, pinned here.
        assert_parity("fn box(w, h, d = 1) = w * h * d\nbox(2, d: 5, h: 3)");
        assert_parity("fn cd(n, acc = 0) = if n <= 0 then acc else cd(n - 1, acc + n)\ncd(5)");
    }

    /// The tree-walker recurses on the native stack (the 20k `MAX_CALL_DEPTH` guard)
    /// while the VM keeps frames on the heap (1M-deep), so a function recursing in that
    /// gap succeeds on the VM and is rejected by the tree-walker. This is a documented,
    /// by-design engine difference — NOT a parity violation — and the differential
    /// oracle treats it as agreement (B2). Pinned here in both shapes.
    #[test]
    fn recursion_depth_is_a_by_design_engine_difference() {
        let deep = "fn deep(n) = if n <= 0 then 0 else deep(n - 1)\n";
        let plain = format!("{deep}deep(50000)");
        let caught = format!("{deep}r = try deep(50000)\nr.ok");

        // The VM keeps frames on the heap, so 50k deep is fine on this small test stack.
        // Uncaught it returns a value; caught by `try` it caught nothing (ok: true).
        assert!(run_vm(&plain).is_ok(), "VM should recurse 50k deep on the heap");
        assert_eq!(run_vm(&caught), Ok("true".to_string()));

        // The tree-walker recurses on the NATIVE stack, so reaching its 20k guard needs
        // the 2 GiB stack the real binary gives it (a test thread is only ~2 MiB).
        // Uncaught it rejects on recursion depth; caught by `try` the record is ok: false.
        std::thread::Builder::new()
            .stack_size(2 << 30)
            .spawn(move || {
                assert!(run_tw(&plain).is_err(), "tree-walker should hit its native-stack limit");
                assert!(tw_hit_recursion_limit(&plain), "rejected specifically on recursion depth");
                assert_eq!(run_tw(&caught), Ok("false".to_string()));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// The VM keeps its call stack on the heap, so recursion far deeper than the
    /// tree-walker's native-stack limit runs fine — on an ordinary test thread,
    /// with no 2 GiB stack needed. (`sum(1..100000) = 5000050000`.)
    #[test]
    fn deep_recursion_is_iterative() {
        let src = "fn sum(n, acc) = if n <= 0 then acc else sum(n - 1, acc + n)\nsum(100000, 0)";
        assert_eq!(format!("{}", vm_val(src)), "5000050000");
    }

    /// Automatic memoization turns overlapping recursion linear. `fib(40)` is
    /// ~165M calls naively — this only returns instantly (and correctly) if the
    /// pure, two-self-call `fib` is being memoized. Also confirms the result is
    /// unchanged (memoization is observably transparent).
    #[test]
    fn memoization_makes_overlapping_recursion_linear() {
        let v = vm_val("fn fib(n) = if n < 2 then n else fib(n - 1) + fib(n - 2)\nfib(40)");
        assert_eq!(format!("{}", v), "102334155");
    }

    /// Float recursion is memoized too (keyed by bit pattern). `fibf(40.0)` is
    /// ~165M calls naively — instant only if the float-keyed memo works.
    #[test]
    fn float_recursion_is_memoized() {
        let v = vm_val(
            "fn fibf(n) = if n < 2.0 then n else fibf(n - 1.0) + fibf(n - 2.0)\nfibf(40.0)",
        );
        assert_eq!(format!("{}", v), "102334155.0");
    }

    /// A function that reads a mutable global *through a callee* must NOT be
    /// memoized — its result isn't a function of its arguments alone. Here `f`
    /// reaches `mut g` via `leaf()`; with `g` changed between calls, the second
    /// `f(20)` must reflect the new `g` (= fib(21)·g = 10946·100), not a stale
    /// cached value.
    #[test]
    fn memoization_respects_transitive_mutable_reads() {
        let src = "mut g = 0\n\
                   fn leaf() = g\n\
                   fn f(n) = if n < 2 then leaf() else f(n - 1) + f(n - 2)\n\
                   g = 1\nx = f(20)\ng = 100\nf(20)";
        assert_eq!(format!("{}", vm_val(src)), "1094600");
    }

    /// Division must never be JIT-compiled: native `fdiv` returns inf on /0,
    /// but the interpreter errors — so a `/`-using function falls back and
    /// division by zero still raises (rather than silently producing inf).
    #[test]
    fn division_by_zero_is_not_jitted_to_inf() {
        let toks = lexer::lex("fn f(x) = 10.0 / x\nf(0.0)").unwrap();
        let ast = parser::parse(toks).unwrap();
        let prog = bytecode::compile_with_types(&ast, None).unwrap();
        let jit = crate::jit::build(
            &ast,
            &prog.reduce_loops,
            &prog.map_kernels,
            &prog.filter_kernels,
            &prog.fused_kernels,
        );
        let err = exec(&prog, jit.as_ref()).unwrap_err();
        assert!(err.message.contains("division by zero"), "got: {}", err.message);
    }

    /// Runtime errors must still surface (and match the tree-walker's wording).
    #[test]
    fn errors_propagate() {
        let toks = lexer::lex("fn boom(n) = boom(n + 1)\nboom(0)").unwrap();
        let ast = parser::parse(toks).unwrap();
        let prog = bytecode::compile_with_types(&ast, None).unwrap();
        let err = run(&prog, None).unwrap_err();
        assert!(err.message.contains("maximum recursion depth"));
    }

    /// The JIT must produce identical results to the bytecode VM for the integer
    /// functions it compiles. Run each program both ways and compare.
    fn jit_val(src: &str) -> Value {
        let toks = lexer::lex(src).unwrap();
        let ast = parser::parse(toks).unwrap();
        let mut prog = bytecode::compile_with_types(&ast, None).expect("expected this program to compile");
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        let jit = crate::jit::build(
            &ast,
            &prog.reduce_loops,
            &prog.map_kernels,
            &prog.filter_kernels,
            &prog.fused_kernels,
        );
        exec(&prog, jit.as_ref()).unwrap().pop().unwrap_or(Value::Unit)
    }

    #[test]
    fn jit_matches_vm() {
        for src in [
            "fn fib(n) = if n < 2 then n else fib(n - 1) + fib(n - 2)\nfib(20)",
            "fn fact(n) = if n <= 1 then 1 else n * fact(n - 1)\nfact(12)",
            "fn sum(n, acc) = if n <= 0 then acc else sum(n - 1, acc + n)\nsum(1000, 0)",
            "fn ack(m, n) = if m == 0 then n + 1 else if n == 0 then ack(m - 1, 1) else ack(m - 1, ack(m, n - 1))\nack(2, 3)",
            "fn sq(x) = let y = x * x in y + 1\nsq(7)",
            // Float specialization (f64 native code).
            "fn scale(x) = x * 2.5\nscale(4.0)",
            "fn sq(x) = x * x\nsq(3.0)", // same fn, picked as f64 for a Float arg
            "fn norm(a, b) = a / (a + b)\nnorm(1.0, 3.0)",
            "fn pow2(n, acc) = if n <= 0.0 then acc else pow2(n - 1.0, acc * 2.0)\npow2(10.0, 1.0)",
            // NB: forward-referenced mutual recursion (even/odd) is not covered —
            // the single-pass bytecode compiler can't resolve it yet (follow-up).
        ] {
            assert_eq!(format!("{}", jit_val(src)), format!("{}", vm_val(src)), "JIT≠VM on: {src}");
        }
    }
