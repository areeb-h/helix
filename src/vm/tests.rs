    use super::*;
    use crate::{bytecode, lexer, parser};

    /// Run a source string on the VM and return the value of its final
    /// expression (the trailing `Pop` is stripped so the value survives).
    fn vm_val(src: &str) -> Value {
        let toks = lexer::lex(src).unwrap();
        let mut ast = parser::parse(toks).unwrap();
        crate::namespace::resolve(&mut ast);
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
        let mut ast = parser::parse(toks).unwrap();
        crate::namespace::resolve(&mut ast);
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
        match pick(rng, 15) {
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
                // includes % and / (zero divisors error identically on both engines)
                let op = ["+", "-", "*", "%", "/"][pick(rng, 5) as usize];
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
        let mut ast = parser::parse(toks).map_err(|_| ())?;
        crate::namespace::resolve(&mut ast);
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

    /// Full pipeline (type-check → *type-directed* compile → VM), so
    /// receiver-polymorphic methods (DataFrame/Tensor column-verbs) route by the
    /// receiver's inferred type rather than falling back to the tree-walker.
    fn run_vm_typed(src: &str) -> Result<String, ()> {
        let toks = lexer::lex(src).map_err(|_| ())?;
        let mut ast = parser::parse(toks).map_err(|_| ())?;
        crate::namespace::resolve(&mut ast);
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
        let csv = "io.read_csv(\"examples/data/patients.csv\")";
        let cases = [
            format!("{csv}.where(age > 40).count()"),
            format!("{csv}.where(age > 40 and resting_hr < 75).count()"),
            format!("{csv}.where(age > 40).select(name, age).sort(age).count()"),
            // predicate referencing a global variable → the resolve_var path
            format!("t = 40\n{csv}.where(age > t).count()"),
            // grouped aggregation over an unevaluated column
            "io.read_csv(\"examples/data/genes.csv\").group(species).mean(expression).count()".to_string(),
            // the same queries with the `@column` sigil — must behave identically
            format!("{csv}.where(@age > 40).count()"),
            format!("{csv}.where(@age > 40 and @resting_hr < 75).count()"),
            format!("{csv}.where(@age > 40).select(@name, @age).sort(@age).count()"),
            format!("{csv}.with({{adult: @age >= 18}}).count()"),
            "io.read_csv(\"examples/data/genes.csv\").group(@species).mean(@expression).count()".to_string(),
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
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("examples/ dir") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("helix") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let toks = lexer::lex(&src).unwrap_or_else(|_| panic!("lex failed: {path:?}"));
            let mut ast = parser::parse(toks).unwrap_or_else(|_| panic!("parse failed: {path:?}"));
            crate::namespace::resolve(&mut ast);
            let types =
                crate::types::check(&ast).unwrap_or_else(|_| panic!("type-check failed: {path:?}"));
            bytecode::compile_with_types(&ast, Some(types)).unwrap_or_else(|_| {
                panic!("`{path:?}` falls back to the tree-walker — it should compile on the VM")
            });
            checked += 1;
        }
        assert!(checked >= 10, "expected the full example suite, only saw {checked}");
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
            // string + dna value-methods
            "\"helix\".upper()",
            "dna(\"ATGCGC\").gc_content()",
            "dna(\"ATGATG\").find(\"GAT\")",
            "dna(\"ATGC\").kmers(2)",
            // kmers (ACGT-only spectrum) vs windows (faithful) must agree across engines
            "dna(\"ATGNCC\").kmers(2)",
            "dna(\"ATGNCC\").windows(2)",
            "dna(\"AT\").kmers(5)",
            // DNA ordering (`<`/sort) must agree across engines
            "dna(\"ATG\") < dna(\"CAT\")",
            "[dna(\"CAT\"), dna(\"ATG\")].sort().first()",
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
