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
        match pick(rng, 24) {
            21 => {
                // Short-circuit boolean ops with three-valued `missing` — the
                // and/or/not opcodes (AndCheck/OrCheck/Kleene tables) were never
                // fuzzed. Operands come from a Bool-or-missing sub-grammar, with
                // an occasional would-error operand so short-circuit-past-error
                // is exercised (engines must agree on whether the RHS ran).
                let boolish = |rng: &mut u64, vars: &[String]| -> String {
                    match pick(rng, 5) {
                        0 => "true".to_string(),
                        1 => "false".to_string(),
                        2 => "missing".to_string(),
                        3 => {
                            let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                            format!(
                                "(({}) {} ({}))",
                                gen_expr(rng, 0, vars),
                                cop,
                                gen_expr(rng, 0, vars)
                            )
                        }
                        _ => "((1 / 0) > 0)".to_string(),
                    }
                };
                let l = boolish(rng, vars);
                let r = boolish(rng, vars);
                match pick(rng, 3) {
                    0 => format!("(({l}) and ({r}))"),
                    1 => format!("(({l}) or ({r}))"),
                    _ => format!("(not ({l}))"),
                }
            }
            22 => {
                // Strings and dicts as first-class fuzz values (never generated
                // before): string ordering/equality/length/indexing, and dict
                // construction with BTreeMap-ordered terminals. Confined to
                // scalar-composing shapes whose errors are engine-identical.
                const POOL: [&str; 5] = ["\"a\"", "\"b\"", "\"ab\"", "\"\"", "\"z\""];
                let s1 = POOL[pick(rng, 5) as usize];
                let s2 = POOL[pick(rng, 5) as usize];
                match pick(rng, 5) {
                    0 => {
                        let cop = ["<", ">", "<=", ">=", "==", "!="][pick(rng, 6) as usize];
                        format!("(({s1}) {cop} ({s2}))")
                    }
                    1 => format!("((({s1}).length()) + ({}))", gen_expr(rng, 0, vars)),
                    // In- and out-of-bounds indexing both compose (OOB errors
                    // identically); the char compares back to a pool string.
                    2 => format!("((({s1})[({})]) == ({s2}))", gen_expr(rng, 0, vars)),
                    3 => {
                        let k1 = (next(rng) % 3) as i64;
                        let k2 = (next(rng) % 3) as i64;
                        let probe = (next(rng) % 4) as i64;
                        format!(
                            "(([({k1}, ({})), ({k2}, ({}))].to_dict()).get({probe}) ?? ({}))",
                            gen_expr(rng, 0, vars),
                            gen_expr(rng, 0, vars),
                            gen_expr(rng, 0, vars)
                        )
                    }
                    _ => format!(
                        "(([(true, ({})), (false, ({}))].to_dict()).values().sum())",
                        gen_expr(rng, 0, vars),
                        gen_expr(rng, 0, vars)
                    ),
                }
            }
            23 => {
                // Destructuring `match` patterns over tuple/record scrutinees —
                // previously only int-literal arms were fuzzed; the binder
                // install/restore path (DestructureBind vs eval_with_pattern)
                // gets continuous coverage. Wildcard stays last.
                if pick(rng, 2) == 0 {
                    let op = ["+", "-", "*"][pick(rng, 3) as usize];
                    format!(
                        "(match (({}), ({})) {{ (p, q) => (p {op} q), _ => ({}) }})",
                        gen_expr(rng, depth - 1, vars),
                        gen_expr(rng, depth - 1, vars),
                        gen_expr(rng, 0, vars)
                    )
                } else {
                    format!(
                        "(match {{a: ({}), b: ({})}} {{ {{a: p, b: q}} => (p - q), _ => ({}) }})",
                        gen_expr(rng, depth - 1, vars),
                        gen_expr(rng, depth - 1, vars),
                        gen_expr(rng, 0, vars)
                    )
                }
            }
            20 => {
                // Stepped-range terminals: lazy `range(a, b, s)` first/last/count/sum
                // are scalar-valued, so they compose as sub-expressions — continuous
                // coverage for the lazy-Range representation across empty, reversed,
                // and negative-step shapes (the arm deferred from the oracle audit).
                let a = (next(rng) % 200) as i64 - 100;
                let b = (next(rng) % 200) as i64 - 100;
                let mag = (next(rng) % 7) as i64 + 1;
                let s = if pick(rng, 2) == 0 { mag } else { -mag };
                let term = ["first()", "last()", "count()", "sum()"][pick(rng, 4) as usize];
                format!("((range({a}, {b}, {s})).{term})")
            }
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
                // Occasionally emit a ZERO-PARAMETER comprehension lambda over an
                // array that may be EMPTY or not. Both engines must reject it
                // identically and BEFORE iterating — the walker used to have no
                // check and so succeeded on `[]` while erroring on `[x]`, a
                // value-vs-error divergence this grammar could never generate
                // (it emits no `() =>` inside a comprehension). The empty/non-empty
                // pair is the whole point: the rejection must not depend on data.
                if pick(rng, 12) == 0 {
                    let m = ["map", "filter", "any", "all"][pick(rng, 4) as usize];
                    let src = if pick(rng, 2) == 0 {
                        "[]".to_string()
                    } else {
                        format!("[{}, {}]", gen_expr(rng, 0, vars), gen_expr(rng, 0, vars))
                    };
                    return format!("(try ({src}).{m}(() => ({}))).ok", gen_expr(rng, 0, vars));
                }
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
                // With probability 1/4, draw BOTH operands from an adversarial pool
                // of i64 edge values instead of the recursive grammar. As grammar
                // coincidences these pairings have ~1e-9 joint probability — 40k
                // fuzzed programs never produced `i64::MIN // -1`, the always-checked
                // overflow that aborted the host until 2026-07-10 (ADR 0024). Pairing
                // them deliberately makes MIN//-1, MIN%-1, MAX*MAX, and 2^53-boundary
                // comparisons routine fuzzer traffic. MIN is spelled as an expression
                // (`0 - MAX - 1`) so it can't trip literal-overflow handling.
                if pick(rng, 4) == 0 {
                    const EDGE: [&str; 8] = [
                        "(0 - 9223372036854775807 - 1)", // i64::MIN
                        "9223372036854775807",           // i64::MAX
                        "(-1)",
                        "0",
                        "1",
                        "2",
                        "9007199254740993",              // 2^53 + 1
                        "(-9007199254740993)",
                    ];
                    let a = EDGE[pick(rng, EDGE.len() as u64) as usize];
                    let b = EDGE[pick(rng, EDGE.len() as u64) as usize];
                    return format!("(({a}) {op} ({b}))");
                }
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

    // The three run_* harnesses return `Result<String, String>` — the error side is the
    // ERROR MESSAGE, so the differential fuzzers assert that engines agree not just on
    // "an error happened" but on WHICH error (two different errors used to compare equal
    // as `Err(())`, the trivially-passing arm the oracle audit flagged). Messages are
    // position-free (`HelixError::message`), so identical failures at identical sites
    // compare equal across engines.
    fn run_vm(src: &str) -> Result<String, String> {
        let toks = lexer::lex(src).map_err(|e| e.message.clone())?;
        let ast = parser::parse(toks).map_err(|e| e.message.clone())?;
        let mut prog = bytecode::compile_with_types(&ast, None).map_err(|_| String::from("unsupported by the bytecode compiler"))?;
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        match exec(&prog, None) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(e) => Err(e.message),
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
    fn run_vm_jit(src: &str) -> Result<String, String> {
        let toks = lexer::lex(src).map_err(|e| e.message.clone())?;
        let ast = parser::parse(toks).map_err(|e| e.message.clone())?;
        let mut prog = bytecode::compile_with_types(&ast, None).map_err(|_| String::from("unsupported by the bytecode compiler"))?;
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
            &prog.scan_loops,
        );
        match exec(&prog, jit.as_ref()) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(e) => Err(e.message),
        }
    }

    fn run_tw(src: &str) -> Result<String, String> {
        // The tree-walker recurses on the native stack (~tens of KB per Helix frame),
        // and several tests here drive it 100–300 deep — far past cargo's default
        // ~2 MiB test-thread stack, which would SIGABRT the whole test binary (and
        // every test scheduled after it). Run it on a large stack, matching
        // production's `run_on_big_stack`. The result is a `String` (Send), so it
        // crosses the scoped-thread boundary cleanly (a `Value` would not — it holds
        // `Rc`s, which is why the recursion-depth guard is what bounds these, not the
        // stack).
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(2 << 30)
                .spawn_scoped(scope, || {
                    let toks = lexer::lex(src).map_err(|e| e.message.clone())?;
                    let ast = parser::parse(toks).map_err(|e| e.message.clone())?;
                    let mut interp = Interp::new();
                    let mut last = Value::Unit;
                    for stmt in &ast {
                        match interp.exec(stmt) {
                            Ok(o) => last = o.value,
                            Err(e) => return Err(e.message),
                        }
                    }
                    Ok(format!("{}", last))
                })
                .unwrap()
                .join()
                .unwrap()
        })
    }

    /// True iff the tree-walker rejects `src` specifically by exhausting the
    /// shared `MAX_CALL_DEPTH` guard. Since #81 aligned the engines (one shared
    /// depth constant + walker TCO), the VM exhausts at the same depth with the
    /// identical message, so the fuzzer arms guarded by this are believed
    /// unreachable — kept as a defensive escape hatch that names the failure
    /// class precisely if a depth-related asymmetry ever reappears.
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
    fn run_vm_typed(src: &str) -> Result<String, String> {
        let toks = lexer::lex(src).map_err(|e| e.message.clone())?;
        let ast = parser::parse(toks).map_err(|e| e.message.clone())?;
        // Static-checker rejections are TAGGED: they belong to a different failure
        // taxonomy than dynamic errors (the checker reports the first STATIC error,
        // the engines report the first DYNAMICALLY-REACHED one), so the typed fuzzer
        // exempts them from message-parity while runtime errors stay strict.
        let types = crate::types::check(&ast).map_err(|e| format!("typecheck: {}", e.message))?;
        let mut prog =
            bytecode::compile_with_types(&ast, Some(types)).map_err(|_| String::from("unsupported by the bytecode compiler"))?;
        if matches!(prog.funcs[0].code.last(), Some(Op::Pop)) {
            prog.funcs[0].code.pop();
            prog.funcs[0].pos.pop();
        }
        match exec(&prog, None) {
            Ok(mut s) => Ok(format!("{}", s.pop().unwrap_or(Value::Unit))),
            Err(e) => Err(e.message),
        }
    }

    /// `isqrt` + short-circuit `.any()`/`.all()` over a bounded range — the sieve
    /// quick-win idiom (`is_prime(k) = range(2, isqrt(k)+1).all(d => k%d != 0)`) — must
    /// agree across tree-walker, VM, and JIT (value and error alike).
    #[test]
    fn isqrt_and_short_circuit_match_across_engines() {
        let cases = [
            "isqrt(0)",
            "isqrt(15)",
            "isqrt(16)",
            "isqrt(10000000)",
            "isqrt(9223372036854775807)",
            "isqrt(-4)",                                    // error on all engines
            "range(2, isqrt(91) + 1).any(d => 91 % d == 0)",  // composite 7*13 -> true
            "range(2, isqrt(97) + 1).all(d => 97 % d != 0)",  // prime -> true
            "range(2, isqrt(2) + 1).all(d => 2 % d != 0)",    // empty divisor range -> true
            // prime count below 100 via bounded trial division = 25
            "range(2, 100).filter(k => range(2, isqrt(k) + 1).all(d => k % d != 0)).count()",
            // the native sieve agrees with trial division and across engines
            "primes(1000).count()",
            "primes(50).sum()",
            "primes(2).count()",
        ];
        for src in cases {
            assert_eq!(run_tw(src), run_vm(src), "tw vs vm: {src}");
            assert_eq!(run_tw(src), run_vm_jit(src), "tw vs jit: {src}");
        }
    }

    /// Tail-self-recursive scalar i64 functions now JIT as native LOOPS — the recursion
    /// exclusion is lifted only for the tail shape (`tail_loopable_set`); each tail
    /// self-call rebinds the parameters and jumps, growing no stack, exactly the VM's
    /// `TailCallFn` frame reuse. All three engines must agree on every shape; non-tail
    /// recursion must be untouched (still VM/memoized); the native path must actually
    /// engage (a silent fallback would make the parity assertions vacuous).
    #[test]
    fn tail_recursive_fn_lowers_to_native_loop() {
        let cases = [
            // BARE final expressions, never print-wrapped: the harness formats the LAST
            // value and `print` returns Unit, which would make value parity vacuous
            // (the oracle-audit lesson). Depths stay modest in this tri-engine loop:
            // the TREE-WALKER recurses on the Rust stack and cargo-test threads get
            // ~2 MB — deep coverage is the VM-vs-JIT case below.
            //
            // plain accumulator countdown — the tail-call args read the SAME pre-call
            // `n` twice (`go(n - 1, acc + n)`), pinning the evaluate-then-rebind order
            "fn go(n, acc) = if n <= 0 then acc else go(n - 1, acc + n)\ngo(400, 0)",
            // nontrivial arg expressions
            "fn sq(n, acc) = if n <= 0 then acc else sq(n - 1, acc + n * n)\nsq(200, 0)",
            // tail call under a `let` in the tail path (binder shadow/restore)
            "fn lt(n, acc) = if n <= 0 then acc else (let m = n - 1 in lt(m, acc + n))\nlt(300, 0)",
            // nested if-else chain with euclidean % and // — collatz step count
            "fn c(n, k) = if n == 1 then k else if n % 2 == 0 then c(n // 2, k + 1) else c(3 * n + 1, k + 1)\nc(27, 0)",
            // wrapping i64 multiply through the loop (3^80 mod 2^64 — bit parity)
            "fn dbl(n, acc) = if n <= 0 then acc else dbl(n - 1, acc * 3)\ndbl(80, 1)",
            // zero iterations: base case immediately
            "fn z(n, acc) = if n <= 0 then acc else z(n - 1, acc + 1)\nz(-5, 42)",
            // NON-tail recursion untouched: fib still correct (VM/memoized, JIT-excluded)
            "fn fib(n) = if n < 2 then n else fib(n - 1) + fib(n - 2)\nfib(20)",
            // self-call in ARGUMENT position (not a tail shape) — stays on the VM
            "fn g(n) = if n <= 0 then 0 else g(g(n - 1) - 1)\ng(3)",
            // self-call inside a MATCH arm: `body_calls` now traverses match, so this
            // is correctly detected as recursion and runs on the VM (previously it
            // evaded `recursive_funcs` and was JIT'd as unguarded native recursion)
            "fn ma(n, acc) = match n { 0 => acc, k => ma(k - 1, acc + k) }\nma(300, 0)",
        ];
        for src in cases {
            assert_eq!(run_tw(src), run_vm(src), "tw vs vm: {src}");
            assert_eq!(run_tw(src), run_vm_jit(src), "tw vs jit: {src}");
        }
        // DEEP loop — far beyond the tree-walker's ~20k call-depth guard (a known,
        // by-design engine difference, so tw is not consulted here): the VM's
        // TailCallFn frame-reuse loop and the JIT native loop must both complete
        // and agree. sum 1..=3e6 = 4500001500000.
        let deep = "fn go(n, acc) = if n <= 0 then acc else go(n - 1, acc + n)\ngo(3000000, 0)";
        assert_eq!(run_vm(deep), run_vm_jit(deep), "deep: vm vs jit");
        assert_eq!(run_vm_jit(deep).unwrap(), "4500001500000");
        // ENGAGEMENT: the tail fn must actually run native (call_i64 bumps the counter).
        crate::jit::reset_native_call_count();
        let src = "fn go(n, acc) = if n <= 0 then acc else go(n - 1, acc + n)\ngo(10000, 0)";
        assert_eq!(run_vm_jit(src).unwrap(), "50005000");
        assert!(crate::jit::native_call_count() > 0, "tail fn did not engage the JIT");
    }

    /// MIXED (per-parameter `Int`/`Float`, annotation-typed) tail-recursive functions now
    /// JIT as native loops over the bits ABI (`MixedFn`): Float params cross the FFI as
    /// raw f64 bits, a Float result returns as bits — pure bit moves, so all three
    /// engines must agree bit-exactly. Dispatch fires only when every argument's RUNTIME
    /// type matches the annotations; anything else falls back to the VM (which ignores
    /// annotations), so mismatched calls stay engine-identical too.
    #[test]
    fn mixed_tail_recursive_fn_lowers_to_native_loop() {
        let cases = [
            // the mandelbrot escape-time shape: 4 Float params + an Int counter/result,
            // an or-condition mixing an i64 compare with an f64 compare
            "fn step(zr: Float, zi: Float, cr: Float, ci: Float, i: Int) = if i >= 60 or zr * zr + zi * zi > 4.0 then i else step(zr * zr - zi * zi + cr, 2.0 * zr * zi + ci, cr, ci, i + 1)\nstep(0.0, 0.0, 0.25, 0.35, 0)",
            "fn step(zr: Float, zi: Float, cr: Float, ci: Float, i: Int) = if i >= 60 or zr * zr + zi * zi > 4.0 then i else step(zr * zr - zi * zi + cr, 2.0 * zr * zi + ci, cr, ci, i + 1)\nstep(0.0, 0.0, -1.5, 0.02, 0)",
            // Float RESULT, pinned value: 1.0 * 0.5^3 = 0.125 (bit-exact halving)
            "fn geo(x: Float, n: Int) = if n <= 0 then x else geo(x * 0.5, n - 1)\ngeo(1.0, 3)",
            // deeper Float result — engines must agree on the exact f64 bits
            "fn geo(x: Float, n: Int) = if n <= 0 then x else geo(x * 0.5, n - 1)\ngeo(1.0, 30)",
            // Int→Float PROMOTION inside the loop body (x + n mixes kinds via fcvt)
            "fn p(x: Float, n: Int) = if n <= 0 then x else p(x + n, n - 1)\np(0.5, 100)",
            // let with a Float binding in the tail path (typed shadow/restore)
            "fn h(x: Float, n: Int) = if n <= 0 then x else (let y = x * 0.25 in h(y + x, n - 1))\nh(1.0, 40)",
            // sqrt in the loop body (always-Float builtin)
            "fn s(x: Float, n: Int) = if n <= 0 then x else s(sqrt(x + 2.0), n - 1)\ns(9.0, 25)",
            // MAX_ARITY boundary: 6 params
            "fn m6(a: Float, b: Float, c: Float, d: Float, e: Float, n: Int) = if n <= 0 then a + b + c + d + e else m6(b, c, d, e, a + 1.0, n - 1)\nm6(1.0, 2.0, 3.0, 4.0, 5.0, 50)",
            // DISPATCH DECLINE: Int passed where Float is annotated — the native pattern
            // does not match, all engines take the interpreter path (which ignores
            // annotations) and must agree on its dynamic result
            "fn f(x: Float, n: Int) = if n <= 0 then x else f(x * 0.5, n - 1)\nf(1, 3)",
            // all-Int annotations (mask 0): no mixed form, the plain i64 loop covers it
            "fn g(n: Int, acc: Int) = if n <= 0 then acc else g(n - 1, acc + n)\ng(300, 0)",
            // WRAPPER shape (the mandelbrot idiom): an unannotated interpreter fn whose
            // body TAIL-CALLS the native fn — `TailCallFn` now runs the same native
            // dispatch as `CallFn` and delivers the result as `Return` would (this was
            // the silent 40× hole: the wrapper's tail call previously always
            // frame-reused into bytecode)
            "fn step(zr: Float, zi: Float, cr: Float, ci: Float, i: Int) = if i >= 60 or zr * zr + zi * zi > 4.0 then i else step(zr * zr - zi * zi + cr, 2.0 * zr * zi + ci, cr, ci, i + 1)\nfn esc(a, b) = step(0.0, 0.0, a, b, 0)\nesc(0.25, 0.35)",
            // wrapper whose tail call's args DON'T match the pattern (Int where Float
            // is annotated) — dispatch declines, bytecode frame-reuse as before
            "fn geo(x: Float, n: Int) = if n <= 0 then x else geo(x * 0.5, n - 1)\nfn w(k) = geo(k, 3)\nw(1)",
            // NON-RECURSIVE annotated mixed fn (straight-line, same walker/codegen)
            "fn h(x: Float, n: Int) = x * n + 0.5\nh(2.5, 3)",
            // division by a NONZERO Float literal — bit-exact fdiv, no poison needed
            "fn d(x: Float, n: Int) = if n <= 0 then x else d(x / 2.0 + 1.0 / 4.0, n - 1)\nd(3.0, 5)",
            // Int / Float-literal promotes exactly like the interpreter
            "fn q(x: Float, n: Int) = if n <= 0 then x else q(x + n / 2.0, n - 1)\nq(0.0, 6)",
            // division by a ZERO literal is mixed-INELIGIBLE → all engines take the
            // interpreter path and raise its /0 error identically
            "fn dz(x: Float, n: Int) = if n <= 0 then x else dz(x / 0.0, n - 1)\ndz(1.0, 2)",
            // MIXED-CALLS-MIXED: a non-recursive mixed fn calling a mixed tail loop
            // natively (bits ABI + threaded poison pointer)
            "fn inner(x: Float, n: Int) = if n <= 0 then x else inner(x * 0.5, n - 1)\nfn outer(a: Float, k: Int) = inner(a, k) + inner(a, k)\nouter(1.0, 3)",
            // POISON THROUGH A CALLEE: the inner fn NaN-poisons; the caller's post-call
            // check must bail the whole chain to bytecode → the interpreter's exact
            // NaN-compare error on every engine (not a garbage-0 result)
            "fn nn(x: Float, n: Int) = if sqrt(x) > 0.0 then x else nn(x, n - 1)\nfn w2(a: Float, k: Int) = nn(a, k) + 1.0\nw2(-1.0, 2)",
            // the mandelbrot escape shape end-to-end at a small max_iter
            "fn step(zr: Float, zi: Float, cr: Float, ci: Float, i: Int) = if i >= 40 or zr * zr + zi * zi > 4.0 then i else step(zr * zr - zi * zi + cr, 2.0 * zr * zi + ci, cr, ci, i + 1)\nfn esc2(px: Int, py: Int) = step(0.0, 0.0, -2.5 + 3.5 * px / 60.0, -1.0 + 2.0 * py / 60.0, 0)\nrange(0, 60).map(py => range(0, 60).map(px => esc2(px, py)).sum()).sum()",
            // NaN POISON (the review-confirmed divergence): the interpreter RAISES on a
            // NaN comparison, so the native loop must bail (unordered fcmp → poison →
            // bytecode fallback → identical error), never silently order the NaN
            "fn bad(x: Float, n: Int) = if sqrt(x) > 0.0 then n else bad(x, n + 1)\nbad(-1.0, 0)",
            // NaN appearing mid-loop (x goes negative → sqrt(x) is NaN on a later
            // iteration): first iterations run native, the NaN one poisons + re-runs
            // on bytecode → same error as the interpreter
            "fn drift(x: Float, n: Int) = if sqrt(x) > 100.0 or n >= 5 then n else drift(x - 1.0, n + 1)\ndrift(2.0, 0)",
            // eager `and` with a NaN comparison the interpreter SHORT-CIRCUITS past:
            // native evaluates it eagerly → poison → fallback short-circuits → same
            // VALUE (not an error) on every engine
            "fn sc(x: Float, n: Int) = if n > 0 and sqrt(x) > 0.0 then n else sc(x, n + 1)\nsc(4.0, 0)",
            // inf stays ORDERED (no poison): x*x overflows to inf, inf > 1e10 is a
            // well-ordered comparison on every engine
            "fn ovf(x: Float, n: Int) = if n <= 0 or x * x > 10000000000.0 then x else ovf(x * x, n - 1)\novf(100000.0, 8)",
            // NaN in VALUE position (never compared): produced, bit-round-tripped
            // through the bits ABI, and printed identically
            "fn nv(x: Float, n: Int) = if n <= 0 then sqrt(x) else nv(x, n - 1)\nnv(-4.0, 2)",
            // Int→Float promotion with a HUGE i64 (beyond 2^53): fcvt_from_sint must
            // round exactly like the interpreter's `as f64`
            "fn hp(x: Float, n: Int) = if n <= 0 then x else hp(x + 4611686018427387905, n - 1)\nhp(0.5, 2)",
        ];
        for src in cases {
            assert_eq!(run_tw(src), run_vm(src), "tw vs vm: {src}");
            assert_eq!(run_tw(src), run_vm_jit(src), "tw vs jit: {src}");
        }
        // pinned exact value for the halving case
        assert_eq!(run_vm_jit("fn geo(x: Float, n: Int) = if n <= 0 then x else geo(x * 0.5, n - 1)\ngeo(1.0, 3)").unwrap(), "0.125");
        // ALL-Int-annotated params with FLOAT intermediates (mask = 0, admitted since the
        // body is not i64-closed): the faithful-xorshift Monte-Carlo shape — i64 RNG
        // state threaded through the loop (logical shifts = arithmetic shift + constant
        // mask), f64 point test inside, Int result. Tri-engine at a tw-safe depth (the
        // 8 nested lets make each tree-walker frame Rust-stack-heavy, so stay shallow)…
        let xs = "fn mc(state: Int, n: Int, hits: Int) = if n <= 0 then hits else (let a1 = state ^ (state << 13) in let a2 = a1 ^ ((a1 >> 7) & 144115188075855871) in let s = a2 ^ (a2 << 17) in let b1 = s ^ (s << 13) in let b2 = b1 ^ ((b1 >> 7) & 144115188075855871) in let t = b2 ^ (b2 << 17) in let x = ((s >> 11) & 9007199254740991) * 0.00000000000000011102230246251565404236316680908203125 in let y = ((t >> 11) & 9007199254740991) * 0.00000000000000011102230246251565404236316680908203125 in if x * x + y * y <= 1.0 then mc(t, n - 1, hits + 1) else mc(t, n - 1, hits))\nmc(88172645463325252, 100, 0)";
        assert_eq!(run_tw(xs), run_vm(xs), "xorshift: tw vs vm");
        assert_eq!(run_tw(xs), run_vm_jit(xs), "xorshift: tw vs jit");
        // …and the DEEP pin: 100k points of the shared uint64 xorshift64 stream count
        // exactly 78432 in the unit quarter-circle — verified byte-identical against
        // the C reference implementation (VM TailCallFn == JIT native loop; the
        // tree-walker's ~20k depth guard exempts it here, as documented).
        let xs100k = xs.replace("mc(88172645463325252, 100, 0)", "mc(88172645463325252, 100000, 0)");
        assert_eq!(run_vm(&xs100k), run_vm_jit(&xs100k), "xorshift 100k: vm vs jit");
        assert_eq!(run_vm_jit(&xs100k).unwrap(), "78432");
        // DEEP mixed loop — VM (TailCallFn) and JIT native loop, beyond the tw guard
        let deep = "fn p(x: Float, n: Int) = if n <= 0 then x else p(x + 1.0, n - 1)\np(0.0, 1000000)";
        assert_eq!(run_vm(deep), run_vm_jit(deep), "deep: vm vs jit");
        assert_eq!(run_vm_jit(deep).unwrap(), "1000000.0");
        // ENGAGEMENT: the mixed fn must actually run native (bits cross via call_i64).
        crate::jit::reset_native_call_count();
        let src = "fn geo(x: Float, n: Int) = if n <= 0 then x else geo(x * 0.5, n - 1)\ngeo(1.0, 30)";
        assert!(run_vm_jit(src).is_ok());
        assert!(crate::jit::native_call_count() > 0, "mixed tail fn did not engage the JIT");
    }

    /// Regression pins for the 2026-07 stability sweep — each of these was a CONFIRMED
    /// bug: (1) a slice step near i64::MAX wrapped the cursor into a 2^63 index and
    /// ABORTED the process; (2) the memo purity analysis missed a mutable-global read
    /// through a function VALUE (`let g = peek in (g)()` — the parser folds it into a
    /// plain call to the local name), so the VM served a stale cache where the
    /// tree-walker recomputed; (3) `range(N).take/drop` materialized the whole range
    /// (~1.6 GB at 1e8) despite the documented O(1).
    #[test]
    fn stability_sweep_regressions() {
        let cases = [
            // (1) extreme positive and negative slice steps: clean results, no abort
            "[10, 20, 30, 40, 50][1::9223372036854775807]",
            "[10, 20, 30, 40, 50][3::(0 - 9223372036854775807)]",
            "\"abcde\"[1::9223372036854775807]",
            // (2) the stale-memo shape, folded into one comparable value:
            // p (pre-mutation) * 10000 + the post-mutation recompute = 120705
            "mut k = 1\nfn peek() = k\nfn f(n) = if n < 2 then n else (let g = peek in (g)() + f(n - 1) + f(n - 2))\np = f(5)\nk = 100\np * 10000 + f(5)",
            // …and the direct-call variant of the same hazard
            "mut k = 1\nfn peek() = k\nfn f(n) = if n < 2 then n else peek() + f(n - 1) + f(n - 2)\np = f(5)\nk = 100\np * 10000 + f(5)",
            // (3) lazy take/drop must MATCH the dense equivalents exactly
            "range(0, 10).take(3).sum()",
            "range(0, 10).drop(7).sum()",
            "range(0, 10).take(-5).count()",
            "range(0, 10).take(99).sum()",
            "range(0, 10).drop(99).count()",
            "range(5, 50, 7).drop(2).first()",
            "range(0, 10).drop(3).take(4).sum()",
        ];
        for src in cases {
            assert_eq!(run_tw(src), run_vm(src), "tw vs vm: {src}");
            assert_eq!(run_tw(src), run_vm_jit(src), "tw vs jit: {src}");
        }
        // pinned values: the memo shape must show the RECOMPUTED result (705), and the
        // lazy take/drop re-slices must equal their dense counterparts
        assert_eq!(
            run_vm("mut k = 1\nfn peek() = k\nfn f(n) = if n < 2 then n else (let g = peek in (g)() + f(n - 1) + f(n - 2))\np = f(5)\nk = 100\np * 10000 + f(5)").unwrap(),
            "120705"
        );
        assert_eq!(run_vm("range(0, 10).take(3).sum()"), run_vm("[0, 1, 2].sum()"));
        assert_eq!(run_vm("range(5, 50, 7).drop(2).first()"), run_vm("[5, 12, 19, 26, 33, 40, 47].drop(2).first()"));
    }

    /// Lazy `enumerate()` (`ArrayData::Enumerate`) must (a) agree across tree-walker, VM,
    /// and JIT, and (b) be behaviourally IDENTICAL to the dense `(index, element)` tuple
    /// array — the lazy-vs-dense equivalence the cross-engine oracle alone cannot verify
    /// (all three engines share the one lazy representation), the exact class of bug the
    /// lazy-range work flagged.
    #[test]
    fn enumerate_lazy_matches_dense_and_across_engines() {
        let cases = [
            "[10, 20, 30].enumerate().map((i, x) => i + x).sum()",
            "[5, 6, 7, 8].enumerate().filter((i, x) => i % 2 == 0).map((i, x) => x).sum()",
            "[1.5, 2.5, 3.5].enumerate().map((i, x) => x * 1.0).sum()",
            "[].enumerate().count()",
            "[9].enumerate().first()",
            "[9, 8].enumerate().last()",
            "[3, 1, 2].enumerate().length()",
            "[100, 200, 300, 400].enumerate().map((i, x) => i * x).sum()",
        ];
        for src in cases {
            assert_eq!(run_tw(src), run_vm(src), "tw vs vm: {src}");
            assert_eq!(run_tw(src), run_vm_jit(src), "tw vs jit: {src}");
        }
        // Lazy enumerate vs the DENSE tuple array literal — must be indistinguishable.
        let pairs = [
            (
                "[10, 20, 30].enumerate().map((i, x) => i * 100 + x).sum()",
                "[(0, 10), (1, 20), (2, 30)].map((i, x) => i * 100 + x).sum()",
            ),
            (
                "[7, 8, 9, 10].enumerate().filter((i, x) => x > 8).map((i, x) => i).sum()",
                "[(0, 7), (1, 8), (2, 9), (3, 10)].filter((i, x) => x > 8).map((i, x) => i).sum()",
            ),
            ("[9, 8].enumerate().last()", "[(0, 9), (1, 8)].last()"),
        ];
        for (lazy, dense) in pairs {
            assert_eq!(run_vm(lazy), run_vm(dense), "lazy vs dense (vm): {lazy}");
            assert_eq!(run_tw(lazy), run_tw(dense), "lazy vs dense (tw): {lazy}");
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

    /// Oracle coverage for surfaces an audit flagged as untested by the differential oracle:
    /// Dict operations, record/dict enumeration order, record-update and value-call error
    /// paths, interpolation error paths, and signed integer/modulo edge cases. Each must be
    /// bit-identical across the VM and tree-walker (a value on both, or the same error on
    /// both). A divergence here would be a real correctness bug.
    #[test]
    fn audit_flagged_surfaces_match_tree_walker() {
        let cases = [
            // --- Dict operations (were untested by the oracle) ---
            "d = [(\"b\", 2), (\"a\", 1)].to_dict()\nprint(d.get(\"a\"))",
            "d = [(\"b\", 2), (\"a\", 1)].to_dict()\nprint(d.has(\"a\"))\nprint(d.contains(\"z\"))",
            "d = [(\"b\", 2), (\"a\", 1)].to_dict()\nprint(d.keys())\nprint(d.values())",
            "d = [(\"x\", 9)].to_dict()\nprint(d[\"x\"])",
            // --- record enumeration order + order-independent equality ---
            "r = {z: 1, a: 2, m: 3}\nprint(r.keys())\nprint(r.values())",
            "r1 = {z: 1, a: 2}\nr2 = {a: 2, z: 1}\nprint(r1 == r2)",
            // --- record-update + value-call error paths (error text must match) ---
            "print(try ({...5, x: 1}).ok)",             // non-record spread base
            "f = (x => x)\nprint((try (f)(1, 2)).ok)",  // arity error via value call
            // --- interpolation error path (format spec applied to a string) ---
            "x = \"hi\"\nprint((try \"{x:.2f}\").ok)",
            // --- signed integer / euclidean modulo edge cases ---
            "print(-7 % 3)\nprint(-7 // 3)\nprint(7 % -3)",
            "print(0 - 9223372036854775807 - 1)", // i64::MIN via wrap
        ];
        for src in cases {
            assert_eq!(run_vm(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
    }

    /// Tail-call optimization reuses the frame for a call in tail position instead of pushing
    /// one. It must change only stack behavior, never a result — so shallow tail recursion
    /// stays bit-identical to the tree-walker. Covers an if-tail, a let/do-body tail, a
    /// bare-body tail call, and a call inside `try` (which must NOT be optimized — the frame
    /// has to survive for the handler to unwind to).
    #[test]
    fn tail_calls_match_tree_walker_on_vm() {
        let cases = [
            "fn c(n, a) = if n <= 0 then a else c(n - 1, a + 1)\nprint(c(500, 0))",
            "fn s(n, a) = do { x = n\n if n <= 0 then a else s(n - 1, a + x) }\nprint(s(100, 0))",
            "fn f(n) = if n <= 0 then 0 else f(n - 1)\nprint(f(300))",
            "fn g(n, a) = if n <= 0 then a else (try g(n - 1, a + 1)).value\nprint(g(50, 0))",
        ];
        for src in cases {
            assert_eq!(run_vm(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
    }

    /// Record update `{ ...base, k: v }` runs identically on both engines: overriding an
    /// existing field, appending a new one, a bare copy, override-precedence when a key
    /// repeats, and that the base record is left unmutated (immutability).
    #[test]
    fn record_update_matches_tree_walker_on_vm() {
        let cases = [
            "b = {x: 1, y: 2}\nprint({...b, x: 9})",              // override
            "b = {x: 1}\nprint({...b, z: 3})",                    // append
            "b = {x: 1, y: 2}\nprint({...b})",                    // bare copy
            "b = {x: 1}\nr = {...b, x: 5}\nprint(b.x)\nprint(r.x)", // base unmutated
            "b = {x: 1}\nprint({...b, x: 2, x: 3}.x)",            // last write wins
            "b = {x: 1, y: 2}\nprint({...b, y: 20, z: 30}.y + {...b, y: 20, z: 30}.z)",
        ];
        for src in cases {
            assert_eq!(run_vm(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
    }

    /// Packed-array fast paths (perf, must stay bit-identical): `unique` gained an O(n)
    /// all-Int path (Str/DNA already had one; mixed/Float stay on `values_equal` so
    /// `1 == 1.0` still collapses), and `count`/`length`/`first`/`last` read the packed
    /// Int/Float buffer directly instead of materializing boxed Values.
    #[test]
    fn packed_array_fast_methods_match_tree_walker_on_vm() {
        let cases = [
            "print([3, 1, 2, 3, 1].unique())",                              // all-Int, first-seen order
            "print(range(0, 100).concat(range(0, 100)).unique().count())", // larger all-Int
            "print([1, 1.0].unique())",                                    // mixed: 1 == 1.0 collapses
            "print([1.0, 1, 2].unique())",                                 // mixed, Float first
            "print(range(0, 5).first())",                                  // packed Int first
            "print(range(0, 5).last())",                                   // packed Int last
            "print(range(0, 0).first())",                                  // empty → missing
            "print(range(0, 5).length())",                                 // packed Int length
            "print((range(0, 5) * 1.0).first())",                          // packed Float first
            "print((range(0, 5) * 1.0).last())",                           // packed Float last
        ];
        for src in cases {
            assert_eq!(run_vm(src), run_tw(src), "VM ≠ tree-walker on `{src}`");
        }
    }

    /// `range` operations across ALL THREE engines, compared BY VALUE — bare expressions, NOT
    /// `print`-wrapped (a `print(x)` returns `Unit`, so `run_*` would compare `unit == unit` and
    /// check only error-parity, never the value). Covers terminal/short-circuit methods (`first`,
    /// `last`, `count`, `length`, `take`, `drop`, `sum`), `map`/`filter`/`reduce`, the empty /
    /// reverse / negative / stepped shapes, LARGE ranges, and chains — precisely the surface a
    /// lazy-`range` refactor changes. It passes today (eager `range`) and must keep passing when
    /// `range` goes lazy: any divergence a lazy rewrite introduces (short-circuit, span/`count`
    /// off-by-one, reverse-step, large-range materialization) fails this on VM/JIT vs tree-walker.
    #[test]
    fn differential_range_operations_all_engines() {
        let cases = [
            // terminal / short-circuit (small)
            "range(0, 5).first()", "range(0, 5).last()", "range(0, 5).count()", "range(0, 5).length()",
            "range(0, 5).sum()", "range(0, 10).take(3).sum()", "range(0, 10).drop(3).sum()",
            "range(0, 10).map(it => it * it).sum()", "range(0, 10).filter(it => it % 2 == 0).count()",
            "range(0, 10).reduce(0, (a, x) => a + x)",
            // empty ranges (start >= end)
            "range(5, 5).count()", "range(5, 5).sum()", "range(5, 5).first()", "range(7, 3).count()",
            // negative bounds
            "range(-5, 5).sum()", "range(-5, 5).count()", "range(-5, 5).first()", "range(-5, 5).last()",
            // stepped (ascending + descending)
            "range(0, 10, 2).sum()", "range(0, 10, 3).count()", "range(10, 0, -1).sum()",
            "range(10, 0, -2).first()", "range(10, 0, -2).last()", "range(0, 10, 5).count()",
            // large ranges — the value is identical regardless of eager/lazy materialization
            "range(0, 1000000).sum()", "range(0, 1000000).count()",
            "range(0, 20000000).first()", "range(0, 20000000).last()", "range(0, 20000000).length()",
            // chains
            "range(0, 100).filter(it => it > 50).map(it => it * 2).sum()",
            "range(0, 100).take(10).drop(3).count()",
        ];
        for src in cases {
            let (vm, jit, tw) = (run_vm(src), run_vm_jit(src), run_tw(src));
            assert_eq!(vm, tw, "VM ≠ tree-walker on `{src}`");
            assert_eq!(jit, tw, "JIT ≠ tree-walker on `{src}`");
            // Every case is a valid program; if one starts ERRORING on all engines the test would
            // still pass the equality checks — so require a real value (guards the parse-error trap).
            assert!(vm.is_ok(), "range op unexpectedly errored on `{src}`: {vm:?}");
        }
    }

    /// A lazy `range(...)` must behave BYTE-IDENTICALLY to the equivalent dense `Int` array. The
    /// cross-engine oracle can't catch a `Range`-vs-`Ints` divergence — all three engines would
    /// AGREE on a wrong answer (e.g. all say `range+1` is `Float`) — so this compares `range(…).OP`
    /// directly against `[…ints…].OP`. It caught a real regression: `range(0,5)+1` promoted to
    /// `Float` while `[0,…]+1` stayed `Int`, because the typed arithmetic fast path matched
    /// `ArrayData::Ints` specifically and the unmatched `Range` fell to the f64 path.
    #[test]
    fn range_behaves_identically_to_ints_array() {
        let pairs = [
            ("range(0, 5)", "[0, 1, 2, 3, 4]"),
            ("range(-2, 3)", "[-2, -1, 0, 1, 2]"),
            ("range(10, 0, -2)", "[10, 8, 6, 4, 2]"),
        ];
        let ops = [
            " + 1", " - 2", " * 3", " * 2.0", " / 2", " // 2", " % 2", ".sum()", ".mean()",
            ".max()", ".min()", ".map(it => it + 10)", ".filter(it => it > 1)", ".reverse()",
            ".sort()", ".to_json()", ".count()", ".length()", ".first()", ".last()", "[1]",
        ];
        for (r, ints) in pairs {
            for op in ops {
                let rsrc = format!("{r}{op}");
                let isrc = format!("{ints}{op}");
                assert_eq!(
                    run_vm(&rsrc),
                    run_vm(&isrc),
                    "range `{rsrc}` != equivalent ints `{isrc}` — Range diverges from the Int array"
                );
                assert_eq!(run_vm(&rsrc), run_tw(&rsrc), "range VM != tree-walker on `{rsrc}`");
            }
            // broadcast math functions over a range must match the array too
            for f in ["sqrt", "abs"] {
                assert_eq!(
                    run_vm(&format!("{f}({r})")),
                    run_vm(&format!("{f}({ints})")),
                    "{f}(range) != {f}(ints array)"
                );
            }
        }
    }

    /// The JIT engagement counter — used by the differential fuzzers to prove native code actually
    /// RAN (not a silent bytecode fallback, the "engagement ≠ correctness" trap) — is wired: a
    /// JIT-eligible numeric kernel bumps it, and it is observable + resettable.
    #[test]
    fn jit_engagement_counter_is_wired() {
        crate::jit::reset_native_call_count();
        assert_eq!(crate::jit::native_call_count(), 0, "counter did not reset");
        // A large i64 map is JIT-eligible → the native kernel runs → the counter bumps.
        let _ = run_vm_jit("(range(0, 100000)).map(it => it * 2).sum()");
        assert!(
            crate::jit::native_call_count() > 0,
            "a JIT-eligible kernel did not bump the native-call counter — the engagement probe is broken"
        );
    }

    /// Calling a first-class function *value* produced by an expression —
    /// `(rec.handler)(x)`, `(fns[i])(x)` — runs natively on both engines. These pin
    /// the VM's `CallValue` opcode path (and its error text) to the tree-walker:
    /// a record-stored closure, a dispatch table indexed at runtime, a closure that
    /// captures a free variable, and the three failure modes (not callable, wrong
    /// arity, an inner runtime error propagating out of the called value).
    #[test]
    fn call_value_matches_tree_walker_on_vm() {
        let cases = [
            "r = {handler: (x => x + 1)}\nprint((r.handler)(10))", // record-stored closure
            "fns = [(x => x * 2), (x => x + 100)]\nprint((fns[0])(5))\nprint((fns[1])(5))", // dispatch table
            "k = 7\nr = {f: (x => x + k)}\nprint((r.f)(3))",       // captured free var
            "r = {handler: 3}\nprint((r.handler)(10))",           // not callable → same error
            "r = {f: (x => x)}\nprint((r.f)(1, 2))",              // arity mismatch → same error
            "r = {f: (x => 1 / x)}\nprint((r.f)(0))",             // inner error propagates
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
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                // Depths are aligned since #81 (shared constant + walker TCO), so
                // this arm should be unreachable — a named defensive guard; see
                // `recursion_depth_is_aligned_across_engines`.
                (Ok(_), Err(_)) if tw_hit_recursion_limit(&src) => {}
                // one accepts, the other rejects → a real divergence
                (v, t) => panic!("OUTCOME divergence on `{src}`: vm={v:?} tw={t:?}"),
            }
        }
    }

    /// The unified, maximum-coverage oracle: the SAME broad whole-program generator as
    /// `differential_vm_vs_tree_walker`, but run **with the JIT engaged**. The no-JIT
    /// fuzzer proves VM == tree-walker; this proves **JIT == tree-walker** across the
    /// entire program space rather than only on isolated kernels. Its value is *composition*:
    /// a `reduce`/`map` kernel nested inside `let`/`if`/`try`/`match`/a closure exercises the
    /// native dispatch's stack discipline (e.g. `TryJitReduce`'s capture `split_off`) against
    /// every surrounding op — the interaction bugs the per-kernel oracles can't reach. A
    /// distinct seed explores different programs than the no-JIT fuzzer, widening coverage.
    #[test]
    fn differential_vm_jit_vs_tree_walker() {
        let mut rng = 0x0123_4567_89AB_CDEFu64; // distinct seed from the no-JIT fuzzer
        crate::jit::reset_native_call_count();
        let mut ok = 0u32;
        for _ in 0..30_000 {
            let src = gen_expr(&mut rng, 5, &[]);
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b, "JIT ≠ tree-walker on `{src}`");
                    ok += 1;
                }
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                // Aligned since #81 — defensive guard, identical to the no-JIT fuzzer above.
                (Ok(_), Err(_)) if tw_hit_recursion_limit(&src) => {}
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
        // The oracle is only meaningful if programs actually SUCCEED (else the `(Err,Err)` arm
        // passes trivially — the `let a=[…]` parse-error trap) AND the JIT actually ENGAGES (else
        // `run_vm_jit` silently == the bytecode VM and this tests nothing about native code — the
        // "engagement ≠ correctness" trap). Both are asserted, so a regression that stops the
        // fuzzer generating runnable/JIT-eligible programs fails loudly instead of going green.
        assert!(ok > 3_000, "differential fuzzer had too few successful programs: {ok}/30000");
        assert!(
            crate::jit::native_call_count() > 0,
            "JIT never engaged across 30000 programs — the oracle was silently testing the bytecode VM, not native code"
        );
    }

    /// Map-side `a[i]` lowers to UNCHECKED native loads, so the VM must discharge a bounds
    /// obligation before every kernel run. This sweeps the boundary EXHAUSTIVELY rather than
    /// randomly: endpoint bugs live at `end == len`, `end == len+1`, `start == -1`, and empty
    /// ranges, and a fuzzer hits those only by luck.
    ///
    /// The map's binder is an ELEMENT VALUE, not a counter (the reduce's is), so the endpoint
    /// proof transfers only over a lazy range. Negative indices are the sharp edge: `a[-2]` is
    /// LEGAL Helix (the interpreter Python-wraps it), so a proof that only checked `< len`
    /// would let the kernel read off the front of the buffer and still look right.
    #[test]
    fn map_index_bounds_agree_across_engines_at_every_boundary() {
        crate::jit::reset_native_call_count();
        let mut checked = 0u32;
        for len in [0usize, 1, 3, 5] {
            let arr: Vec<String> = (0..len).map(|i| ((i + 1) * 10).to_string()).collect();
            let a = format!("[{}]", arr.join(", "));
            for start in -4i64..=5 {
                for end in -4i64..=6 {
                    // `a[i]` over a range: the Counter obligation, dischargeable only because
                    // the source is a range.
                    let src = format!("a = {a}\n({start}..{end}).map(i => a[i])");
                    let (tw, vm, jit) = (run_tw(&src), run_vm(&src), run_vm_jit(&src));
                    assert_eq!(tw, vm, "tw vs vm on `{src}`");
                    assert_eq!(vm, jit, "vm vs JIT on `{src}`");
                    checked += 1;

                    // `a[k]` for a loop-invariant scalar: a point check that holds over any
                    // source shape, so it must NOT be gated on range-ness.
                    let src = format!("a = {a}\nk = {start}\n(0..{end}).map(i => a[i] + a[k])");
                    let (tw, vm, jit) = (run_tw(&src), run_vm(&src), run_vm_jit(&src));
                    assert_eq!(tw, vm, "scalar-index tw vs vm on `{src}`");
                    assert_eq!(vm, jit, "scalar-index vm vs JIT on `{src}`");

                    // The gather shape: an element-value binder over a NON-range source. The
                    // indices are arbitrary data, so this must fall back — and stay correct,
                    // including the negative (wrapping) elements.
                    let src = format!("a = {a}\nidx = [{start}, {end}]\nidx.map(x => a[x])");
                    let (tw, vm, jit) = (run_tw(&src), run_vm(&src), run_vm_jit(&src));
                    assert_eq!(tw, vm, "gather tw vs vm on `{src}`");
                    assert_eq!(vm, jit, "gather vm vs JIT on `{src}`");
                }
            }
        }
        assert!(checked > 300, "boundary sweep too small: {checked}");
        // Agreement is worthless if the kernel never ran — three engines silently sharing the
        // bytecode loop agree trivially. This is the "engagement ≠ correctness" trap the
        // differential fuzzers guard the same way.
        assert!(
            crate::jit::native_call_count() > 0,
            "no native kernel ran across the sweep — the oracle compared the VM against itself"
        );
    }

    /// The indexed map kernel must actually ENGAGE on the shape it exists for — not merely
    /// agree by falling back. Pins the payoff (the ~40x gap this closed) as a behavioral fact:
    /// if a future change quietly stops admitting `a[i]`, the perf regression fails HERE, as a
    /// correctness test, instead of silently costing 40x until someone re-benchmarks.
    #[test]
    fn indexed_map_engages_the_native_kernel_only_over_a_range_source() {
        // In bounds over a range → the kernel runs.
        crate::jit::reset_native_call_count();
        let src = "a = (0..64).map(i => i * 3)\n(0..64).map(i => a[i] + 1).reduce(0, (s, x) => s + x)";
        assert_eq!(run_vm_jit(src), run_tw(src));
        let over_range = crate::jit::native_call_count();
        assert!(over_range > 0, "indexed map did not engage over a range source");

        // Out of bounds by ONE → the obligation fails, so the kernel must NOT run this map.
        // (`a`'s own construction still JITs, so the count can rise; what matters is the
        // answer, which only the checked loop can produce.)
        let src = "a = (0..64).map(i => i * 3)\n(0..65).map(i => a[i])";
        assert!(run_vm_jit(src).is_err(), "out-of-bounds indexed map should raise");
        assert_eq!(run_vm_jit(src), run_tw(src), "OOB error text must match the walker");

        // A gather (element-value binder, non-range source) is unprovable → must fall back.
        // Verified by agreement rather than by counting, since `idx`'s own map may JIT.
        let src = "a = [10, 20, 30]\nidx = [2, 0, 1]\nidx.map(x => a[x])";
        assert_eq!(run_vm_jit(src), run_tw(src));
        assert_eq!(run_vm_jit(src).unwrap(), "[30, 10, 20]");
    }


    /// The f64 (mixed-indexed) twin of the map-side bounds sweep: `(0..n).map(i => a[i] *
    /// 2.0)` over a `Floats` array runs the MIXED kernel's unchecked F64 loads, so the same
    /// endpoint discharge applies. What is NEW — and what this test chiefly exists for — is
    /// TYPE CONFUSION: one stored kernel now carries an i64 and a mixed specialization, and
    /// the VM's marshal routes by the runtime array representation. An `Ints` buffer
    /// reaching an F64 load would reinterpret the bits as (tiny, plausible-looking) floats
    /// and corrupt results SILENTLY — no crash, no error, just wrong science — so the
    /// routing probes below are asserted against literal expected VALUES, not merely
    /// cross-engine agreement.
    #[test]
    fn f64_map_index_agrees_and_routes_by_representation() {
        crate::jit::reset_native_call_count();
        for len in [0usize, 1, 3, 5] {
            let arr: Vec<String> = (0..len).map(|i| format!("{i}.5")).collect();
            let a = format!("[{}]", arr.join(", "));
            for start in -3i64..=4 {
                for end in -3i64..=5 {
                    let src = format!("a = {a}\n({start}..{end}).map(i => a[i] * 2.0)");
                    let (tw, vm, jit) = (run_tw(&src), run_vm(&src), run_vm_jit(&src));
                    assert_eq!(tw, vm, "tw vs vm on `{src}`");
                    assert_eq!(vm, jit, "vm vs JIT on `{src}`");
                }
            }
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "the mixed-indexed kernel never engaged — the sweep compared the VM against itself"
        );

        for (src, want) in [
            // Float-rooted body over an INTS array: the mixed spec's marshal requires
            // `Floats` → declines → checked loop. (The corruption signature this guards
            // against: tiny denormal-ish junk like 4.9e-323 in place of 20.0.)
            ("a = [10, 20, 30]\n(0..3).map(i => a[i] * 2.0)", "[20.0, 40.0, 60.0]"),
            // A body BOTH analyses admit (`a[i] + 1`), over Floats → the mixed spec.
            ("a = [1.5, 2.5, 3.5]\n(0..3).map(i => a[i] + 1)", "[2.5, 3.5, 4.5]"),
            // The same body over Ints → the i64 spec; floats here would be corruption.
            ("a = [10, 20, 30]\n(0..3).map(i => a[i] + 1)", "[11, 21, 31]"),
            // Mixed representations across two caps: neither spec matches → checked loop.
            (
                "a = [10, 20, 30]\nb = [0.5, 1.5, 2.5]\n(0..3).map(i => a[i] + b[i])",
                "[10.5, 21.5, 32.5]",
            ),
            // A Float SCALAR cap: the analysis types scalar caps `i64`, so the marshal
            // (which requires `Value::Int`) must decline rather than pass f64 bits.
            ("a = [1.5, 2.5, 3.5]\nk = 0.5\n(0..3).map(i => a[i] * k)", "[0.75, 1.25, 1.75]"),
            // sqrt/min compose over the loads (the same arms the unindexed mixed kernel
            // has always had, now fed from memory instead of the counter).
            ("a = [4.0, 9.0, 16.0]\n(0..3).map(i => sqrt(a[i]))", "[2.0, 3.0, 4.0]"),
            (
                "a = [1.5, 2.5, 3.5]\nb = [3.0, 1.0, 2.0]\n(0..3).map(i => min(a[i], b[i]))",
                "[1.5, 1.0, 2.0]",
            ),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "engines disagree on `{src}`");
            assert_eq!(jit, run_vm(src), "vm disagrees on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
    }


    /// Affine map indices — `a[2*i]`, `a[i + off]`, `a[i*n + k]` (the matmul row/column
    /// reads) — discharge by proving the range's two ENDPOINT indices in bounds, composed
    /// with the source's step (`idx = base + coef*(start + step*j)`, affine∘affine, monotone
    /// in `j`) in CHECKED i128 — with i64 captures and the 100M materialization cap the
    /// composed magnitude can exceed even i128, and overflow must decline exactly like
    /// out-of-range, never accept. The sweep runs stride × offset × range × length
    /// exhaustively; the endpoint mistakes (an off-by-one at either end of a NEGATIVE-coef
    /// descent, a base that wraps only the first element) live at boundaries a fuzzer
    /// reaches by luck.
    #[test]
    fn affine_map_index_agrees_at_every_boundary() {
        crate::jit::reset_native_call_count();
        for len in [0usize, 1, 4, 7] {
            let arr: Vec<String> = (0..len).map(|i| format!("{i}.5")).collect();
            let a = format!("[{}]", arr.join(", "));
            for coef in [-2i64, -1, 0, 1, 2, 3] {
                for off in [-2i64, -1, 0, 1, 2] {
                    for end in [0i64, 1, 3, 4] {
                        // Spell negatives as real arithmetic — `a[1 * i + -1]` is a PARSE
                        // error, and a sweep corner that parse-errors "agrees" on (Err,Err)
                        // while exercising nothing. That vacuous corner shipped in this
                        // test's first version and was caught only because sabotaging the
                        // LOWER endpoint failed to turn it red: every lo<0 case needs a
                        // negative constant, and every negative constant was a parse error.
                        let c = if coef < 0 { format!("(0 - {})", -coef) } else { coef.to_string() };
                        let o = if off < 0 { format!("- {}", -off) } else { format!("+ {off}") };
                        let src =
                            format!("a = {a}\n(0..{end}).map(i => a[{c} * i {o}] * 2.0)");
                        let (tw, vm, jit) = (run_tw(&src), run_vm(&src), run_vm_jit(&src));
                        assert_eq!(tw, vm, "tw vs vm on `{src}`");
                        assert_eq!(vm, jit, "vm vs JIT on `{src}`");
                    }
                }
            }
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "the affine map kernel never engaged — the sweep compared the VM against itself"
        );

        // The matmul row/column reads (captured scalars riding in base and coef — `i*n`
        // lands as a synthetic `$aff` cap the compile site evaluates once) and the composed
        // maptemp inner loop, pinned against literal values.
        for (src, want) in [
            (
                "n = 3\na = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]\ni = 1\n(0..3).map(k => a[i * n + k])",
                "[4.0, 5.0, 6.0]",
            ),
            (
                "n = 3\nb = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]\nj = 2\n(0..3).map(k => b[k * n + j])",
                "[3.0, 6.0, 9.0]",
            ),
            (
                "n = 3\na = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]\nb = [9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0]\ni = 1\nj = 0\n(0..3).map(k => a[i * n + k] * b[k * n + j]).reduce(0.0, (s, x) => s + x)",
                "84.0",
            ),
            // A STEPPED source range composing with the affine index.
            (
                "a = [0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5]\nrange(0, 6, 2).map(e => a[2 * e])",
                "[0.5, 4.5, 8.5]",
            ),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "engines disagree on `{src}`");
            assert_eq!(jit, run_vm(src), "vm disagrees on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }

        // A huge captured coefficient: the i128 endpoint lands far outside [0, len), so the
        // kernel must never run — the checked loop raises the exact interpreter error.
        let src = "a = [0.5, 1.5]\nm = 4000000000000000000\n(0..2).map(i => a[m * i] * 2.0)";
        assert!(run_vm_jit(src).is_err(), "overflown affine index should raise");
        assert_eq!(run_vm_jit(src), run_tw(src), "overflow error text must match the walker");

        // A COMPOUND affine term (`i + a + b`) folds `a + b` into a synthetic `$aff` slot, so
        // the bound names the slot, not `a`/`b`. `relabel_value_scalars` must still recognize
        // `a`/`b` as INDEX scalars (they are recomputed in the index codegen) and keep them
        // `i64` — mislabeling them `ScalarValue` typed the index in `f64` and emitted ill-typed
        // IR the Cranelift verifier rejected, silently declining the kernel (an audit finding).
        // So this must ENGAGE the native kernel, not merely agree by falling back.
        crate::jit::reset_native_call_count();
        for (src, want) in [
            ("x = [10.0, 20.0, 30.0, 40.0, 50.0, 60.0]\na = 1\nb = 2\n(0..3).map(i => x[i + a + b])", "[40.0, 50.0, 60.0]"),
            ("x = [10.0, 20.0, 30.0, 40.0, 50.0, 60.0]\na = 1\nb = 1\n(0..3).map(i => x[(a + b) * i])", "[10.0, 30.0, 50.0]"),
            // a genuine value scalar `c` alongside the compound-affine index: `c` becomes
            // ScalarValue (f64), `a`/`b` stay Scalar (i64).
            ("x = [10.0, 20.0, 30.0, 40.0, 50.0, 60.0]\na = 1\nb = 2\nc = 2.0\n(0..3).map(i => c * x[i + a + b])", "[80.0, 100.0, 120.0]"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "compound-affine disagreement on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "compound-affine gather never JITed — the index scalars were mislabeled f64 and the \
             verifier declined the kernel (the perf cliff this guards against)"
        );
    }


    /// SAXPY — `(0..n).map(i => a * x[i] + y[i])` with a runtime float coefficient — is the
    /// canonical BLAS-1 op, and a value scalar in the mixed kernel is the feature under test.
    /// The subtle part is bit-identity: the scalar rides as `f64` in the kernel but is
    /// possibly-`Int` at runtime, so it is admitted ONLY where a genuine float promotes it
    /// (`MixT`). This pins both the routing (one kernel, i64 vs f64 by representation) AND the
    /// decline of the shapes that would diverge — verified against LITERAL values, since three
    /// engines agreeing cannot tell "correct" from "all fell back".
    #[test]
    fn saxpy_float_scalar_caps_route_and_decline_correctly() {
        crate::jit::reset_native_call_count();
        // Engaging SAXPY shapes over Floats arrays → the mixed kernel, exact values.
        for (src, want) in [
            (
                "a = 2.5\nx = [1.5, 2.5, 3.5]\ny = [0.25, 0.5, 0.75]\n(0..3).map(i => a * x[i] + y[i])",
                "[4.0, 6.75, 9.5]",
            ),
            // an INT coefficient in a float body: promoted, so the mixed kernel handles it too.
            (
                "a = 3\nx = [1.5, 2.5, 3.5]\ny = [0.25, 0.5, 0.75]\n(0..3).map(i => a * x[i] + y[i])",
                "[4.75, 8.0, 11.25]",
            ),
            // two value scalars, each promoted by its own array.
            (
                "c = 2.0\nd = 3.0\nx = [1.0, 2.0, 3.0]\ny = [0.5, 0.5, 0.5]\n(0..3).map(i => c * x[i] + d * y[i])",
                "[3.5, 5.5, 7.5]",
            ),
            // additive offset, and left-assoc `x[i] + a + b` (safe: the array promotes first).
            ("a = 100.0\nx = [1.0, 2.0, 3.0]\n(0..3).map(i => a + x[i])", "[101.0, 102.0, 103.0]"),
            ("a = 1.5\nb = 2.5\nx = [10.0, 20.0]\n(0..2).map(i => x[i] + a + b)", "[14.0, 24.0]"),
            // sqrt promotes, so a value scalar under sqrt is safe.
            ("a = 16.0\nx = [1.0, 2.0]\n(0..2).map(i => sqrt(a) + x[i])", "[5.0, 6.0]"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "engines disagree on `{src}`");
            assert_eq!(jit, run_vm(src), "vm disagrees on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "the SAXPY mixed kernel never engaged — the oracle tested the VM against itself"
        );

        // The DECLINE cases: a value scalar combined with an integer (or under abs/min) would be
        // i64 in the interpreter and f64 in the kernel. The `2^53+1` inputs make that a REAL
        // divergence (proven by sabotage: forcing engagement yields ...976 vs the correct
        // ...980), so these MUST fall back and agree on the i64-exact value.
        for src in [
            "a = 9007199254740993\nx = [1.0, 1.0]\n(0..2).map(i => a * 3 + x[i])",
            "a = 9007199254740993\nb = 9007199254740993\nx = [1.0, 1.0]\n(0..2).map(i => a + b + x[i])",
            "a = 9007199254740993\nx = [1.0, 1.0]\n(0..2).map(i => a * i + x[i])",
            "a = -5.5\nx = [1.0, 2.0]\n(0..2).map(i => abs(a) + x[i])",
            "a = 2.5\nx = [1.0, 5.0]\n(0..2).map(i => min(a, x[i]))",
        ] {
            assert_eq!(run_vm_jit(src), run_tw(src), "declined shape must match the walker on `{src}`");
            assert_eq!(run_vm_jit(src), run_vm(src), "declined shape must match the VM on `{src}`");
        }

        // Representation routing: the SAME body over Ints (int coefficient) runs the i64 spec
        // and stays Int; a Float coefficient with Int arrays declines to the VM.
        for (src, want) in [
            ("a = 3\nx = [10, 20, 30]\n(0..3).map(i => a * x[i])", "[30, 60, 90]"),
            ("a = 2.5\nx = [10, 20, 30]\n(0..3).map(i => a * x[i])", "[25.0, 50.0, 75.0]"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "routing disagreement on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
    }


    /// Value scalars in the **f64 indexed REDUCE** — the allocation-free spelling, and the one
    /// that matters most (it is the faithful port that beats C on k1 dot). Before this, the f64
    /// indexed reduce admitted NO value-scalar captures at all (`infer_f64_indexed`'s Ident arm
    /// returned `None` — a documented v1b limitation), so `s + c * a[i]` with `c` a variable of
    /// EITHER type fell to the VM: a measured ~30× cliff, and `map(...).reduce(...)` was
    /// perversely faster than the direct reduce that allocates nothing.
    ///
    /// This kernel is monomorphically `f64` (a `Float` init picked it), so there is no
    /// representation routing here — but the bit-identity rule carries over exactly, via the
    /// same shared `mix_combine`: a value scalar rides as `f64` yet may be `Int` at runtime, so
    /// it is admitted only where a genuine float (the accumulator, an array load, a float
    /// literal) promotes it. The decline cases below use 2^53+1 so a wrong admission is a REAL
    /// divergence — proven by sabotage (forcing it yields …928.0 vs the correct …944.0).
    #[test]
    fn f64_reduce_value_scalars_promote_or_decline() {
        crate::jit::reset_native_call_count();
        let arrs = "a = [1.5, 2.5, 3.5]\nb = [0.25, 0.5, 0.75]\n";
        // Engaging: the value scalar is promoted by an array load or the f64 accumulator.
        for (body, want) in [
            ("c = 2.5\n(0..3).reduce(0.0, (s, i) => s + c * a[i] + b[i])", "20.25"),
            // an INT variable coefficient — also declined before this change
            ("m = 3\n(0..3).reduce(0.0, (s, i) => s + m * a[i])", "22.5"),
            ("c = 2.0\nd = 4.0\n(0..3).reduce(0.0, (s, i) => s + c * a[i] + d * b[i])", "21.0"),
            ("c = 10.0\n(0..3).reduce(0.0, (s, i) => s + a[i] + c)", "37.5"),
            // sqrt promotes, so a value scalar under it is safe
            ("c = 16.0\n(0..3).reduce(0.0, (s, i) => s + sqrt(c) + a[i])", "19.5"),
            // the accumulator itself is a genuine float and promotes the scalar
            ("c = 1.5\n(0..3).reduce(1.0, (s, i) => s * c + a[i])", "14.0"),
        ] {
            let src = format!("{arrs}{body}");
            let jit = run_vm_jit(&src);
            assert_eq!(jit, run_tw(&src), "engines disagree on `{src}`");
            assert_eq!(jit, run_vm(&src), "vm disagrees on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "the f64 reduce never engaged with a value scalar — the oracle tested the VM against \
             itself, and the ~30× cliff this closes would be silently back"
        );

        // MUST DECLINE — the interpreter evaluates these subterms in i64, the kernel would use
        // f64. Fall back and agree on the i64-exact value.
        for body in [
            "big = 9007199254740993\n(0..3).reduce(0.0, (s, i) => s + big * 3 + a[i])",
            "big = 9007199254740993\n(0..3).reduce(0.0, (s, i) => s + big * i + a[i])",
            "p = 9007199254740993\nq = 9007199254740993\n(0..3).reduce(0.0, (s, i) => s + p + q + a[i])",
            // abs/min do not promote, so an SFloat argument must be refused
            "c = -5.5\n(0..3).reduce(0.0, (s, i) => s + abs(c) + a[i])",
            "c = 2.5\n(0..3).reduce(0.0, (s, i) => s + min(c, a[i]))",
        ] {
            let src = format!("{arrs}{body}");
            assert_eq!(run_vm_jit(&src), run_tw(&src), "declined shape must match the walker on `{src}`");
            assert_eq!(run_vm_jit(&src), run_vm(&src), "declined shape must match the VM on `{src}`");
        }

        // INDEX scalars must stay `i64` and NOT be relabeled into f64 value scalars — including a
        // name used as both an index and a value (necessarily Int), and a COMPOUND affine term
        // whose parts are named only through a synthetic `$aff` slot.
        for (src, want) in [
            ("a = [1.5, 2.5, 3.5]\nb = [0.25, 0.5, 0.75]\nk = 2\n(0..3).reduce(0.0, (s, i) => s + a[k] + b[i])", "12.0"),
            ("n = 2\nm = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]\nj = 1\n(0..3).reduce(0.0, (s, k) => s + m[k * n + j])", "12.0"),
            ("n = 2\nm = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]\n(0..3).reduce(0.0, (s, k) => s + m[k * n] * n)", "18.0"),
            ("a = [1.5, 2.5, 3.5]\np = 1\nq = 1\nc = 2.0\n(0..2).reduce(0.0, (s, i) => s + c * a[i + p + q - 1])", "12.0"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "index-scalar disagreement on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
    }


    /// map→reduce fusion by substitution: `range(s,e).map(f).reduce(init,g)` compiles the fold
    /// over `g(acc, f(i))` directly, so no intermediate array is built. This is an AST rewrite,
    /// which makes the hazards CAPTURE, SHADOWING, and ERROR ORDER rather than arithmetic — each
    /// gets a case below, and each guard has been sabotage-proven (removing the capture check
    /// makes the first case return 0.0 instead of 15.0; naming the accumulator slot after the
    /// user's binder breaks the second ON THE VM PATH ONLY, since the JIT path takes the fused
    /// route and never runs the fall-through; removing the re-entry guard makes compilation hang).
    #[test]
    fn map_reduce_fusion_is_exact_and_declines_where_it_must() {
        // ENGAGEMENT, expressed as the property that actually matters: the map spelling must cost
        // the SAME number of native calls as the equivalent direct reduce. Fused, both are one
        // native fold; unfused, the map spelling additionally runs a map kernel and builds an
        // array. A relative assertion self-calibrates if kernel accounting ever changes.
        let fused = "(0..4).map(i => i * 1.5).reduce(0.0, (s, x) => s + x)";
        let direct = "(0..4).reduce(0.0, (s, i) => s + i * 1.5)";
        crate::jit::reset_native_call_count();
        assert_eq!(run_vm_jit(fused).as_deref(), Ok("9.0"), "fused value");
        let n_fused = crate::jit::native_call_count();
        crate::jit::reset_native_call_count();
        assert_eq!(run_vm_jit(direct).as_deref(), Ok("9.0"), "direct value");
        let n_direct = crate::jit::native_call_count();
        assert!(n_direct > 0, "the direct reduce did not engage — the oracle is not testing native code");
        assert_eq!(
            n_fused, n_direct,
            "map→reduce cost {n_fused} native calls vs {n_direct} for the equivalent direct \
             reduce — fusion is not engaging, so the intermediate array is back"
        );

        // Exactness across the shapes fusion takes.
        for (src, want) in [
            ("(0..4).map(i => i * 1.5).reduce(0.0, (s, x) => s + x)", "9.0"),
            // The `it` SUGAR must fuse identically. It is the IDIOMATIC spelling, and the
            // first version of this matched `Expr::Lambda` directly, silently excluding it and
            // leaving ordinary code 93x slower (0.93s vs 0.01s at n=20M). Now routed through
            // `comprehension_params`, the same desugaring the compile path uses, so the two
            // spellings cannot drift apart again.
            ("(0..4).map(it * 1.5).reduce(0.0, (s, x) => s + x)", "9.0"),
            ("a = [1.5, 2.5, 3.5]\n(0..3).map(a[it]).reduce(0.0, (s, x) => s + x)", "7.5"),
            ("a = [1.5, 2.5, 3.5]\n(0..3).map(i => a[i]).reduce(0.0, (s, x) => s + x)", "7.5"),
            (
                "a = [1.5, 2.5, 3.5]\nb = [0.25, 0.5, 0.75]\nc = 2.5\n(0..3).map(i => c * a[i] + b[i]).reduce(0.0, (s, x) => s + x)",
                "20.25",
            ),
            // the element binder used TWICE in `g` → the substituted body is duplicated, which is
            // safe only because every admitted node is pure and deterministic
            ("(0..4).map(i => i * 1.5).reduce(0.0, (s, x) => s + x * x)", "31.5"),
            // absent from `g`
            ("(0..3).map(i => i * 1.5).reduce(0.0, (s, x) => s + 1.0)", "3.0"),
            // an affine index and a promoting call inside the map body
            ("a = [0.5, 1.5, 2.5, 3.5, 4.5, 5.5]\n(0..3).map(i => a[2 * i]).reduce(0.0, (s, x) => s + x)", "7.5"),
            ("a = [4.0, 9.0, 16.0]\n(0..3).map(i => sqrt(a[i])).reduce(0.0, (s, x) => s + x)", "9.0"),
            // empty, reverse, and negative-start ranges
            ("(3..3).map(i => i * 1.5).reduce(0.0, (s, x) => s + x)", "0.0"),
            ("(3..0).map(i => i * 1.5).reduce(0.0, (s, x) => s + x)", "0.0"),
            ("((-2)..2).map(i => i * 1.5).reduce(0.0, (s, x) => s + x)", "-3.0"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "JIT vs walker on `{src}`");
            assert_eq!(jit, run_vm(src), "JIT vs VM on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }

        // CAPTURE: `f` references a variable named like the accumulator. Substituting would bind
        // it to the accumulator and yield 0.0; the fusion must decline. All three engines agree.
        let src = "s = 5.0\n(0..3).map(i => s * 1.0).reduce(0.0, (s, x) => s + x)";
        assert_eq!(run_vm_jit(src), run_tw(src), "capture case: JIT vs walker");
        assert_eq!(run_vm_jit(src), run_vm(src), "capture case: JIT vs VM");
        assert_eq!(run_vm_jit(src).as_deref(), Ok("15.0"), "capture case value");

        // SHADOWING: the range bound shares the accumulator's name. The accumulator SLOT is
        // synthetic, so the recompiled fall-through still resolves the bound outward. The VM path
        // is the one that exercises this (it runs the fall-through), so all three are compared.
        let src = "u = 3\n(0..u).map(i => i * 1.5).reduce(0.0, (u, x) => u + x)";
        assert_eq!(run_vm_jit(src), run_tw(src), "shadow case: JIT vs walker");
        assert_eq!(run_vm(src), run_tw(src), "shadow case: VM vs walker (runs the fall-through)");
        assert_eq!(run_vm_jit(src).as_deref(), Ok("4.5"), "shadow case value");

        // ERROR ORDER — the hazard fusion would introduce if the fused body could raise. Here `f`
        // raises out-of-bounds at i=2 while `g` divides by zero at i=0. Unfused, `map` runs to
        // completion first, so the OOB is what surfaces; a fused body would surface the division.
        // Because the guard declines whenever bounds are not proven, the original path runs and
        // the OOB is reported — on every engine.
        let src = "a = [1.5, 2.5]\n(0..3).map(i => a[i]).reduce(0.0, (s, x) => s + 1.0 / (x - 1.5))";
        let e = run_vm_jit(src);
        assert!(e.is_err(), "expected a raise");
        assert_eq!(e, run_tw(src), "error-order: JIT vs walker");
        assert_eq!(e, run_vm(src), "error-order: JIT vs VM");
        assert!(
            e.as_ref().unwrap_err().contains("out of bounds"),
            "must report f's out-of-bounds, not g's division: {e:?}"
        );

        // Shapes that must DECLINE and stay correct: an i64 init (already fused by FusedKernel and
        // not to be disturbed), a binding form the substitution refuses, non-numeric elements,
        // chained maps, a filter in the chain, and a non-idempotent bound (which would otherwise
        // be evaluated twice — once for the guard's operands, once by the fall-through).
        for (src, want) in [
            ("(0..5).map(i => i * 2).reduce(0, (s, x) => s + x)", "20"),
            ("(0..3).map(i => let q = i in q * 1.5).reduce(0.0, (s, x) => s + x)", "4.5"),
            ("(0..3).map(i => i * 1.5).map(y => y + 1.0).reduce(0.0, (s, x) => s + x)", "7.5"),
            ("(0..5).filter(i => i > 1).map(i => i * 1.5).reduce(0.0, (s, x) => s + x)", "13.5"),
            ("fn f() = 3\n(0..f()).map(i => i * 1.5).reduce(0.0, (s, x) => s + x)", "4.5"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "declined shape: JIT vs walker on `{src}`");
            assert_eq!(jit, run_vm(src), "declined shape: JIT vs VM on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
    }


    /// Numeric recursion reaches native code WITHOUT type annotations.
    ///
    /// The mixed per-parameter specialization is the sound successor to the removed blanket-`f64`
    /// function spec (a float-arg function can still return an `Int`, so blanket `f64` codegen
    /// diverged on result type). But it was reachable only through explicit `: Int` / `: Float`
    /// annotations, so the natural shape of a numeric loop — float state plus an integer counter —
    /// never compiled at all. Measured before this: `fn spin(zr, zi, i, n)` ran 0.53s where the
    /// identical annotated body ran 0.01s, with `JIT ≈ NOJIT` confirming it never reached native
    /// code; the all-`Float` shape was the same at 0.63s. Both are now 0.01s.
    ///
    /// `infer_param_kinds` only PROPOSES kinds. Two validators stand behind it, which is why a
    /// wrong proposal cannot produce a wrong answer: `mixed_tail_ret_kind` re-types the body under
    /// the proposal and declines if it does not check, and the VM re-tests every argument's runtime
    /// type against the compiled `float_mask` before dispatching. The engagement assertions below
    /// are what make this test meaningful — agreement alone would also hold if nothing compiled.
    #[test]
    fn unannotated_numeric_recursion_reaches_native_code() {
        // Each pair is (unannotated, annotated) with identical bodies: same answer, and the
        // unannotated form must ENGAGE rather than merely agree.
        let cases = [
            // mixed: float state + int counter (the shape that was 53× slower)
            (
                "fn spin(zr, zi, i, n) = if i >= n then zr + zi else spin(zr * 0.5 + 1.0, zi * 0.5 + 0.25, i + 1, n)\nspin(0.0, 0.0, 0, 40)",
                "fn spin(zr: Float, zi: Float, i: Int, n: Int) = if i >= n then zr + zi else spin(zr * 0.5 + 1.0, zi * 0.5 + 0.25, i + 1, n)\nspin(0.0, 0.0, 0, 40)",
            ),
            // all-float, where the LIMIT's kind is only knowable from the comparison `i >= lim`
            (
                "fn spin(zr, zi, i, lim) = if i >= lim then zr + zi else spin(zr * 0.5 + 1.0, zi * 0.5 + 0.25, i + 1.0, lim)\nspin(0.0, 0.0, 0.0, 40.0)",
                "fn spin(zr: Float, zi: Float, i: Float, lim: Float) = if i >= lim then zr + zi else spin(zr * 0.5 + 1.0, zi * 0.5 + 0.25, i + 1.0, lim)\nspin(0.0, 0.0, 0.0, 40.0)",
            ),
            // the mandelbrot inner loop, unannotated — an int result out of a float body
            (
                "fn step(zr, zi, cr, ci, i) = if i >= 50 then 50 else if zr * zr + zi * zi > 4.0 then i else step(zr * zr - zi * zi + cr, 2.0 * zr * zi + ci, cr, ci, i + 1)\nstep(0.0, 0.0, 0.3, 0.5, 0)",
                "fn step(zr: Float, zi: Float, cr: Float, ci: Float, i: Int) = if i >= 50 then 50 else if zr * zr + zi * zi > 4.0 then i else step(zr * zr - zi * zi + cr, 2.0 * zr * zi + ci, cr, ci, i + 1)\nstep(0.0, 0.0, 0.3, 0.5, 0)",
            ),
            // a partly-annotated signature: the annotations must be honoured exactly and the rest
            // inferred around them
            (
                "fn go(a, b: Int) = if b >= 20 then a else go(a * 0.5 + 1.0, b + 1)\ngo(0.0, 0)",
                "fn go(a: Float, b: Int) = if b >= 20 then a else go(a * 0.5 + 1.0, b + 1)\ngo(0.0, 0)",
            ),
        ];
        for (unann, ann) in cases {
            // Correctness first: all three engines, both spellings, one value.
            let want = run_tw(unann);
            assert!(want.is_ok(), "walker failed on `{unann}`: {want:?}");
            assert_eq!(run_vm(unann), want, "VM disagrees on `{unann}`");
            assert_eq!(run_vm_jit(unann), want, "JIT disagrees on `{unann}`");
            assert_eq!(run_vm_jit(ann), want, "the annotated twin gives a different answer");

            // Engagement: the unannotated form must actually call native code. Without this the
            // test would pass just as happily if the inference did nothing.
            crate::jit::reset_native_call_count();
            let _ = run_vm_jit(unann);
            assert!(
                crate::jit::native_call_count() > 0,
                "unannotated recursion did not reach native code — the annotation cliff is back:\n\
                 {unann}"
            );
        }

        // The DECLINE path: a parameter with contradictory evidence (used as an i64-closed operand
        // AND float-tainted) must be refused rather than guessed at, and the program must still
        // produce the interpreter's answer via the bytecode path.
        for src in [
            "fn odd(a, i, n) = if i >= n then a else odd(a % 3 + 0.5, i + 1, n)\nodd(1, 0, 5)",
            "fn shifty(a, i, n) = if i >= n then a else shifty((a << 1) + 0.25, i + 1, n)\nshifty(1, 0, 4)",
        ] {
            let want = run_tw(src);
            assert_eq!(run_vm(src), want, "VM disagrees on the declined shape `{src}`");
            assert_eq!(run_vm_jit(src), want, "JIT disagrees on the declined shape `{src}`");
        }
    }


    /// `to_float` — the explicit Int→Float conversion — compiles natively instead of forcing the
    /// whole enclosing loop onto the VM.
    ///
    /// It is what a careful user writes, and it was in none of the JIT's float-builtin gates, so
    /// any body containing it declined. Measured at n=20M before/after:
    /// `reduce(0.0, (a,i) => a + to_float(i) * 1.5)` 1.56s → 0.01s (156×),
    /// `map(to_float(it) * 1.5).reduce(…)` 2.27s → 0.01s (227×),
    /// `reduce(0.0, (a,i) => a + to_float(i))` 1.32s → 0.01s (132×).
    ///
    /// The risk is ROUNDING, not speed: the interpreter computes `*i as f64` while the kernel
    /// emits `fcvt_from_sint`. Both round to nearest-even, so they agree — but past 2^53 the
    /// conversion is lossy and a mismatch would be invisible on small inputs, which is why the
    /// cases below straddle 2^53 and both i64 extremes.
    #[test]
    fn to_float_compiles_natively_and_rounds_like_the_interpreter() {
        crate::jit::reset_native_call_count();
        for (src, want) in [
            // exact, in both the map and the reduce position
            ("(0..5).map(to_float(it)).reduce(0.0, (s, x) => s + x)", "10.0"),
            ("(0..5).reduce(0.0, (s, i) => s + to_float(i))", "10.0"),
            // LOSSY past 2^53: 9007199254740993 has no f64 representation and must round DOWN
            ("to_float(9007199254740993)", "9007199254740992.0"),
            (
                "(9007199254740990..9007199254740995).map(to_float(it)).reduce(0.0, (s, x) => s + x)",
                "45035996273704960.0",
            ),
            // the i64 extremes
            ("to_float(9223372036854775807)", "9223372036854775808.0"),
            ("to_float(0 - 9223372036854775807)", "-9223372036854775808.0"),
            // negatives, nesting, composition with sqrt, and to_float of a Float (identity)
            ("((-5)..5).map(to_float(it) * 1.5).reduce(0.0, (s, x) => s + x)", "-7.5"),
            ("(0..5).map(to_float(to_float(it))).reduce(0.0, (s, x) => s + x)", "10.0"),
            ("a = [1.5, 2.5, 3.5]\n(0..3).map(to_float(a[it])).reduce(0.0, (s, x) => s + x)", "7.5"),
            // inside a tail-recursive numeric function, inferred and annotated
            ("fn go(a, i, n) = if i >= n then a else go(a + to_float(i) * 0.5, i + 1, n)\ngo(0.0, 0, 20)", "95.0"),
            ("fn go(a: Float, i: Int, n: Int) = if i >= n then a else go(a + to_float(i) * 0.5, i + 1, n)\ngo(0.0, 0, 20)", "95.0"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "JIT vs walker on `{src}`");
            assert_eq!(jit, run_vm(src), "JIT vs VM on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "no native call — `to_float` is back to forcing the VM path"
        );

        // Still correct where it CANNOT compile: a non-numeric argument (`to_float` also parses
        // numeric strings) and a genuine division by zero must behave exactly as the interpreter.
        for src in [
            "to_float(\"3.5\") + 1.0",
            "(0..3).map(to_float(\"2.5\")).reduce(0.0, (s, x) => s + x)",
            "(0..4).reduce(0.0, (s, i) => s + 1.0 / to_float(i))",
        ] {
            assert_eq!(run_vm_jit(src), run_tw(src), "fallback shape `{src}`");
            assert_eq!(run_vm_jit(src), run_vm(src), "fallback shape (VM) `{src}`");
        }

        // `to_float` returns Float, so it must NOT leak into the i64-only kernel: an integer
        // context containing it stays correct rather than compiling as i64.
        let src = "(0..5).map(it * 2).reduce(0, (s, x) => s + x)";
        assert_eq!(run_vm_jit(src).as_deref(), Ok("20"), "the i64 path is unaffected");
    }


    /// A FLOAT `reduce` over an ARRAY folds leading `map` stages into its own body, so
    /// `xs.map(f).reduce(0.0, g)` costs what the hand-composed `xs.reduce(0.0, g∘f)` costs —
    /// measured 0.73s vs 0.04s at n=20M, an 18× penalty for writing the pipeline the natural way.
    ///
    /// This is the array-source twin of the range-source fusion, and it needed a different
    /// mechanism: `fusion_stage` builds only `i64` transforms, so a float map body is not an
    /// eligible stage at all — the chain walk stopped immediately and the receiver stayed a method
    /// call, which is not idempotent, so the whole plan declined. The maps are therefore peeled
    /// SYNTACTICALLY and folded before the sink is built, which also means the folded body faces
    /// exactly the same `reduce_jit_f64_body` validation a hand-written one would.
    ///
    /// Both guards are sabotage-proven: dropping them makes the capture case return 0.0 instead of
    /// 20.0 and the shadow case 4.0 instead of 200.0.
    #[test]
    fn float_array_reduce_folds_map_stages() {
        let xs = "xs = [1.5, 2.5, 3.5]\n";
        for (body, want) in [
            ("xs.map(it * 1.5).reduce(0.0, (a, x) => a + x)", "11.25"),
            ("xs.map(it * 1.5).map(it + 1.0).reduce(0.0, (a, x) => a + x)", "14.25"),
            ("xs.map(x => x * 2.0).reduce(0.0, (a, y) => a + y * y)", "83.0"),
            // the element binder used TWICE in the reduce body duplicates the folded expression,
            // which is safe only because every admitted node is pure
            ("xs.map(it * 2.0).reduce(0.0, (a, x) => a + x * x)", "83.0"),
            ("xs.map(abs(it - 2.5)).reduce(0.0, (a, x) => a + x)", "2.0"),
            ("c = 3.0\nxs.map(it * c).reduce(0.0, (a, x) => a + x)", "22.5"),
        ] {
            let src = format!("{xs}{body}");
            let jit = run_vm_jit(&src);
            assert_eq!(jit, run_tw(&src), "JIT vs walker on `{src}`");
            assert_eq!(jit, run_vm(&src), "JIT vs VM on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }

        // CAPTURE: the map body names the reduce's accumulator. Folding would bind it to the
        // accumulator and yield 0.0.
        let src = "a = 10.0\nxs = [1.5, 2.5]\nxs.map(a * 1.0).reduce(0.0, (a, x) => a + x)";
        assert_eq!(run_vm_jit(src), run_tw(src), "capture case vs walker");
        assert_eq!(run_vm_jit(src).as_deref(), Ok("20.0"), "capture case");

        // SHADOW: the map body names the reduce's element binder.
        let src = "x = 100.0\nxs = [1.5, 2.5]\nxs.map(x * 1.0).reduce(0.0, (a, x) => a + x)";
        assert_eq!(run_vm_jit(src), run_tw(src), "shadow case vs walker");
        assert_eq!(run_vm_jit(src).as_deref(), Ok("200.0"), "shadow case");

        // Shapes that must NOT fold, each still correct: a filter in the chain (the f64 reduce
        // analysis has no `if`), an i64 accumulator (a separate, untouched path), an `Ints` array
        // under a float reduce (representation mismatch → fallback), a non-idempotent source
        // (which folding must not cause to be evaluated twice), and an empty array.
        for (src, want) in [
            ("xs = [1.5, 2.5, 3.5]\nxs.filter(it > 2.0).map(it * 2.0).reduce(0.0, (a, x) => a + x)", "12.0"),
            ("xs = [1.5, 2.5, 3.5]\nxs.map(it * 2.0).filter(it > 4.0).reduce(0.0, (a, x) => a + x)", "12.0"),
            ("ys = [1, 2, 3]\nys.map(it * 3).reduce(0, (a, x) => a + x)", "18"),
            ("ys = [1, 2, 3]\nys.map(it * 2).reduce(0.0, (a, x) => a + to_float(x))", "12.0"),
            ("fn mk() = [1.5, 2.5]\nmk().map(it * 2.0).reduce(0.0, (a, x) => a + x)", "8.0"),
            ("emp = [1.0][0:0]\nemp.map(it * 2.0).reduce(0.0, (a, x) => a + x)", "0.0"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "non-folding shape vs walker on `{src}`");
            assert_eq!(jit, run_vm(src), "non-folding shape vs VM on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }

        // ERROR ORDER: unfused, every `f(x)` runs before any `g`. A raise from either side must
        // surface identically on all three engines — the folded kernel poisons and falls back to
        // the original chain rather than reporting a different error.
        for src in [
            "xs = [1.5, 2.5, 3.5]\nxs.map(1.0 / (it - 1.5)).reduce(0.0, (a, x) => a + x)",
            "xs = [1.5, 2.5, 3.5]\nxs.map(it * 2.0).reduce(0.0, (a, x) => a + 1.0 / (x - 3.0))",
        ] {
            let e = run_vm_jit(src);
            assert!(e.is_err(), "expected a raise on `{src}`");
            assert_eq!(e, run_tw(src), "error text vs walker on `{src}`");
            assert_eq!(e, run_vm(src), "error text vs VM on `{src}`");
        }
    }


    /// `to_int` and `sign` compile natively. Both are Int-returning and — the property that makes
    /// them safe with no new machinery — **neither can raise**:
    /// `to_int` SATURATES (NaN → 0, ±inf → the i64 extremes), which is exactly Rust's `as i64`
    /// and Cranelift's `fcvt_to_sint_sat`; `sign` is two comparisons whose NaN case falls through
    /// to 0, matching the interpreter (which compares rather than using `signum`, so it does not
    /// propagate NaN). Measured at n=30M: `map(sign(…))` 3.18s → 0.02s (159×), `map(to_int(it)*2)`
    /// 3.08s → 0.01s (308×), `map(to_float(to_int(…)))` 4.41s → 0.02s (220×).
    ///
    /// Contrast `floor`/`ceil`/`round`/`trunc`, which RAISE when the result leaves i64 range and
    /// so still need a poison path — and `clamp`, which raises when `lo > hi`. Those remain on the
    /// VM deliberately, not by oversight.
    #[test]
    fn to_int_and_sign_compile_and_match_the_interpreter_at_every_edge() {
        crate::jit::reset_native_call_count();
        for (src, want) in [
            // to_int: truncation toward zero, both signs
            ("(0..6).map(to_int(to_float(it) * 1.5)).reduce(0, (s, x) => s + x)", "21"),
            ("((-6)..0).map(to_int(to_float(it) * 1.5)).reduce(0, (s, x) => s + x)", "-30"),
            // to_int SATURATES rather than raising — the property the lowering depends on
            ("to_int(1.0e30)", "9223372036854775807"),
            ("to_int(0.0 - 1.0e30)", "-9223372036854775808"),
            ("to_int(inf)", "9223372036854775807"),
            ("to_int(0.0 - inf)", "-9223372036854775808"),
            // NaN → 0 for BOTH (sqrt of a negative is how a NaN is reached without raising)
            ("to_int(sqrt(-1.0))", "0"),
            ("sign(sqrt(-1.0))", "0"),
            // sign over floats and ints, including the infinities and both zeroes
            ("((-5)..5).map(sign(to_float(it))).reduce(0, (s, x) => s + x)", "-1"),
            ("((-5)..5).map(sign(it)).reduce(0, (s, x) => s + x)", "-1"),
            ("sign(inf) + sign(-inf)", "0"),
            ("sign(0.0) + sign(0) + sign(-0.0)", "0"),
            // to_int of an Int is the identity
            ("(0..5).map(to_int(it)).reduce(0, (s, x) => s + x)", "10"),
            // inside a reduce body and a tail-recursive function
            ("(0..20).reduce(0, (s, i) => s + sign(to_float(i) - 10.0))", "-1"),
            ("fn go(a, i, n) = if i >= n then a else go(a + to_int(to_float(i) * 1.5), i + 1, n)\ngo(0, 0, 12)", "96"),
            // float-ROOTED bodies with the Int builtin inside — the mixed kernel
            ("(0..6).map(to_float(to_int(to_float(it) * 1.5)) * 2.0).reduce(0.0, (s, x) => s + x)", "42.0"),
            ("(0..6).map(to_float(sign(to_float(it) - 3.0)) * 0.5).reduce(0.0, (s, x) => s + x)", "-0.5"),
        ] {
            let jit = run_vm_jit(src);
            assert_eq!(jit, run_tw(src), "JIT vs walker on `{src}`");
            assert_eq!(jit, run_vm(src), "JIT vs VM on `{src}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{src}`");
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "neither builtin reached native code — they are back to forcing the VM path"
        );

        // The RAISING builtins must still behave exactly as the interpreter, on every engine.
        // They are excluded on purpose: a native kernel cannot raise, and no poison path covers
        // the map/reduce kernels yet.
        for src in [
            "floor(1.0e30)",
            "ceil(1.0e30)",
            "round(1.0e30)",
            "trunc(1.0e30)",
            "clamp(1.5, 3.0, 0.0)",
            "(0..3).map(floor(1.0e30)).reduce(0, (s, x) => s + x)",
        ] {
            let e = run_vm_jit(src);
            assert!(e.is_err(), "expected a raise on `{src}`");
            assert_eq!(e, run_tw(src), "raise text vs walker on `{src}`");
            assert_eq!(e, run_vm(src), "raise text vs VM on `{src}`");
        }
        // …and where they do NOT raise they must still give the interpreter's answer.
        for (src, want) in [
            ("floor(2.7) + ceil(2.1) + trunc(2.9) + round(2.5)", "10"),
            ("clamp(5.0, 0.0, 3.0)", "3.0"),
        ] {
            assert_eq!(run_vm_jit(src), run_tw(src), "non-raising case `{src}`");
            assert_eq!(run_vm_jit(src).as_deref(), Ok(want), "`{src}`");
        }
    }


    /// A `map` over a lazy range runs on GENERATED counter values, not a materialized buffer.
    ///
    /// Materializing purely so the kernel had something to read left that buffer live alongside
    /// the output, so a single `(0..n).map(f)` peaked at TWICE its result — measured 328 MB for
    /// 160 MB of payload at n=20M, now 186 MB; the k1 dot product's documented ~400 MB overhead
    /// over C was exactly one such transient, and its peak fell 485 MB → 345 MB. It is also 2–3×
    /// FASTER, because a full buffer is no longer written and then read back.
    ///
    /// The sharp edge is CHUNK BOUNDARIES: values are generated 16K at a time, so the element
    /// index must be `base + k` rather than `k`. A bug there is invisible below 16384 elements,
    /// which is why the cases below straddle 16383/16384/16385 and 32767/32768 (the latter also
    /// crossing `PAR_MATH_THRESHOLD`, where generation moves into rayon workers).
    #[test]
    fn range_map_generates_values_without_materializing() {
        // Chunk-boundary elements, read individually so an off-by-chunk shows up as a wrong value
        // rather than being averaged away by a sum.
        let src = "n = 100000\na = (0..n).map(it * 2)\n\"{a[0]} {a[16383]} {a[16384]} {a[32767]} {a[32768]} {a[n - 1]}\"";
        assert_eq!(run_vm_jit(src), run_tw(src), "chunk boundaries vs walker");
        assert_eq!(run_vm_jit(src).as_deref(), Ok("0 32766 32768 65534 65536 199998"));

        for (s, want) in [
            // lengths exactly at, and one past, a chunk boundary
            ("a = (0..16384).map(it * 3)\n\"{a.length()} {a[16383]} {a.reduce(0, (s, x) => s + x)}\"", "16384 49149 402628608"),
            ("a = (0..16385).map(it * 3)\n\"{a.length()} {a[16384]} {a.reduce(0, (s, x) => s + x)}\"", "16385 49152 402677760"),
            // below vs above PAR_MATH_THRESHOLD — serial and parallel generation
            ("(0..1000).map(it * 7).reduce(0, (s, x) => s + x)", "3496500"),
            ("(0..40000).map(it * 7).reduce(0, (s, x) => s + x)", "5599860000"),
            // degenerate ranges
            ("(5..5).map(it * 2).length()", "0"),
            ("(5..0).map(it * 2).length()", "0"),
            ("(0..1).map(it * 2)", "[0]"),
            // negative start and negative step
            ("((-5)..5).map(it * 2)", "[-10, -8, -6, -4, -2, 0, 2, 4, 6, 8]"),
            ("range(10, 0, -1).map(it * 2)", "[20, 18, 16, 14, 12, 10, 8, 6, 4, 2]"),
            ("range(0, 20, 3).map(it + 1)", "[1, 4, 7, 10, 13, 16, 19]"),
            // the element formula is computed in i128, so a start near i64::MAX cannot overflow
            // before the truncation the interpreter also performs
            ("range(9223372036854775805, 9223372036854775807, 1).map(it * 1)",
             "[9223372036854775805, 9223372036854775806]"),
            // f64 output (the mixed kernel) over a generated range
            ("a = (0..40000).map(it * 1.5)\n\"{a[16384]} {a[39999]}\"", "24576.0 59998.5"),
            // an INDEXED map over a range still discharges its bounds against the range's
            // endpoints — the fast path passes the same `src_range` to the marshal
            ("n = 40000\nx = (0..n).map(it * 2)\n(0..n).map(x[it] + 1).reduce(0, (s, y) => s + y)", "1600000000"),
            // a captured scalar in the body, i64 and f64
            ("c = 3\n(0..40000).map(it * c).reduce(0, (s, x) => s + x)", "2399940000"),
        ] {
            let jit = run_vm_jit(s);
            assert_eq!(jit, run_tw(s), "JIT vs walker on `{s}`");
            assert_eq!(jit, run_vm(s), "JIT vs VM on `{s}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{s}`");
        }

        // FILTER over a range takes the same fast path, and compaction makes chunk offsets the
        // sharp edge: chunk i's survivors must land immediately after chunk i-1's. Peak memory
        // 250 MB -> 98 MB on a 20M range keeping half.
        for (s, want) in [
            ("a = (0..100000).filter(it % 2 == 0)\n\"{a.length()} {a[8191]} {a[8192]} {a[16383]} {a[16384]} {a[a.length() - 1]}\"",
             "50000 16382 16384 32766 32768 99998"),
            // a predicate keeping ~one element per chunk, so a wrong offset shows up at once
            // rather than being absorbed by neighbours
            ("a = (0..100000).filter(it % 16384 == 0)\n\"{a.length()} {a[1]} {a[2]}\"", "7 16384 32768"),
            ("a = (0..16384).filter(it > 16000)\n\"{a.length()} {a[0]}\"", "383 16001"),
            ("a = (0..16385).filter(it > 16000)\n\"{a.length()} {a[a.length() - 1]}\"", "384 16384"),
            ("(0..100000).filter(it < 0).length()", "0"),
            ("(0..100000).filter(it >= 0).length()", "100000"),
            ("(5..5).filter(it > 0).length()", "0"),
            ("range(20, 0, -1).filter(it % 3 == 0)", "[18, 15, 12, 9, 6, 3]"),
            ("((-10)..10).filter(it % 4 == 0)", "[-8, -4, 0, 4, 8]"),
            ("(0..40000).filter(it % 3 == 0).map(it * 2).reduce(0, (s, x) => s + x)", "533346666"),
        ] {
            let jit = run_vm_jit(s);
            assert_eq!(jit, run_tw(s), "filter-over-range vs walker on `{s}`");
            assert_eq!(jit, run_vm(s), "filter-over-range vs VM on `{s}`");
            assert_eq!(jit.as_deref(), Ok(want), "`{s}`");
        }

        // An out-of-bounds indexed map over a range must still raise the interpreter's exact
        // error — the fast path declines and the checked loop runs.
        let src = "x = [1, 2, 3]\n(0..40000).map(x[it]).length()";
        let e = run_vm_jit(src);
        assert!(e.is_err(), "expected an out-of-bounds raise");
        assert_eq!(e, run_tw(src), "raise text vs walker");
        assert_eq!(e, run_vm(src), "raise text vs VM");
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
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// `i64::MIN // -1` and `i64::MIN % -1` are an always-checked i64 overflow that
    /// `div_euclid`/`rem_euclid` panic on (even in release). Both the tree-walker and
    /// the VM must WRAP identically (`//` → `i64::MIN`, `%` → 0), never abort — a case
    /// the expression fuzzer never generates (it doesn't emit `i64::MIN` with a `-1`
    /// divisor). The JIT only compiles `//`/`%` by a positive constant, so it cannot
    /// reach this case.
    #[test]
    fn int_min_floordiv_mod_wrap_on_all_engines() {
        let d = "(0 - 9223372036854775807 - 1) // (0 - 1)";
        let m = "(0 - 9223372036854775807 - 1) % (0 - 1)";
        assert_eq!(run_tw(d).unwrap(), "-9223372036854775808");
        assert_eq!(run_vm(d).unwrap(), "-9223372036854775808");
        assert_eq!(run_tw(m).unwrap(), "0");
        assert_eq!(run_vm(m).unwrap(), "0");
    }

    /// On-demand SOAK fuzzer — ignored by default; run during stabilization passes:
    /// `HELIX_SOAK_SEED=<n> HELIX_SOAK_ITERS=<n> cargo test --profile gate
    /// soak_differential -- --ignored --nocapture`. Same tri-engine value AND
    /// error-message parity as the standing fuzzers, but the seed and iteration count
    /// come from the environment, so repeated runs explore FRESH program space instead
    /// of re-walking the fixed seeds. Mixes free expressions, two-parameter functions,
    /// and the tail-recursive family in one stream.
    #[test]
    #[ignore = "on-demand soak — seed via HELIX_SOAK_SEED, iterations via HELIX_SOAK_ITERS"]
    fn soak_differential() {
        let seed: u64 = std::env::var("HELIX_SOAK_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0xD1CE);
        let iters: u32 = std::env::var("HELIX_SOAK_ITERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100_000);
        let mut rng = seed ^ 0x9E37_79B9_7F4A_7C15;
        crate::jit::reset_native_call_count();
        let mut ok = 0u32;
        for i in 0..iters {
            let src = match i % 4 {
                0 | 1 => gen_expr(&mut rng, 5, &[]),
                2 => {
                    let params = vec!["a".to_string(), "b".to_string()];
                    let body = gen_expr(&mut rng, 4, &params);
                    format!(
                        "fn f(a, b) = {}\nf({}, {})",
                        body,
                        gen_lit(&mut rng),
                        gen_lit(&mut rng)
                    )
                }
                _ => {
                    let params = vec!["n".to_string(), "acc".to_string()];
                    let base = gen_expr(&mut rng, 2, &params);
                    let term = gen_expr(&mut rng, 2, &params);
                    let dec = pick(&mut rng, 3) + 1;
                    let start_n = (next(&mut rng) % 120) as i64;
                    let start_acc = gen_lit(&mut rng);
                    format!(
                        "fn tf(n, acc) = if n <= 0 then ({base}) else tf(n - {dec}, ({term}))\ntf({start_n}, {start_acc})"
                    )
                }
            };
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b, "soak: JIT ≠ tree-walker on `{src}`");
                    ok += 1;
                }
                (Err(ea), Err(eb)) => {
                    assert_eq!(ea, eb, "soak: error-message divergence on `{src}`")
                }
                (Ok(_), Err(_)) if tw_hit_recursion_limit(&src) => {}
                (v, t) => panic!("soak: OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
        assert!(ok > iters / 10, "soak: too few successful programs: {ok}/{iters}");
        assert!(crate::jit::native_call_count() > 0, "soak: the JIT never engaged");
        println!("soak seed={seed:#x} iters={iters}: {ok} successful, all engine-identical");
    }

    /// Continuous differential coverage for the tail-recursion native loops (B1 i64 +
    /// B2 mixed): random tail-self-recursive functions whose base case, loop term, and
    /// call arguments are full `gen_expr` trees over the parameters. The shape is
    /// TERMINATION-SAFE by construction (the first argument of every tail call is a
    /// literal decrement `n - 1..=3` and the condition is `n <= 0`), while everything
    /// else — including error paths like float bitwise, `??`, `try`, `match` inside the
    /// term — is free-form. A wrapper function is sometimes interposed so the
    /// `TailCallFn` native dispatch is fuzzed too. Value AND error-message parity.
    #[test]
    fn differential_tail_recursive_fns() {
        let mut rng = 0x7A11_CA11_F0F0_1234u64;
        crate::jit::reset_native_call_count();
        for i in 0..4_000 {
            let params = vec!["n".to_string(), "acc".to_string()];
            let base = gen_expr(&mut rng, 2, &params);
            let term = gen_expr(&mut rng, 2, &params);
            let dec = pick(&mut rng, 3) + 1;
            let start_n = (next(&mut rng) % 120) as i64; // tw-stack-safe depth
            let start_acc = gen_lit(&mut rng);
            // Alternate the three dispatch shapes: plain i64 params, an annotated
            // MIXED signature (Float acc), and a wrapper tail-calling the tail fn.
            let src = match i % 3 {
                0 => format!(
                    "fn tf(n, acc) = if n <= 0 then ({base}) else tf(n - {dec}, ({term}))\ntf({start_n}, {start_acc})"
                ),
                1 => format!(
                    "fn tf(acc: Float, n: Int) = if n <= 0 then ({base}) else tf(({term}) * 1.0, n - {dec})\ntf(1.5, {start_n})"
                ),
                _ => format!(
                    "fn tf(n, acc) = if n <= 0 then ({base}) else tf(n - {dec}, ({term}))\nfn w(a, b) = tf(a, b)\nw({start_n}, {start_acc})"
                ),
            };
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "tail-fn JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "no tail fn ever engaged the JIT across 4000 programs — the fuzzer is testing only the interpreter"
        );
    }

    /// The widened i64 JIT op set (bitwise, constant shifts, `//` by a positive constant,
    /// and `and`/`or` in a condition) must be bit-identical to the tree-walker. `gen_expr`
    /// already fuzzes `//`; bitwise/shift/and-or are not in its grammar, so pin them here
    /// (scalar functions, a fused reduce/filter kernel, and negative operands for the
    /// arithmetic-shift and euclidean-floor edges). Each is called with all-Int args so the
    /// native i64 specialization is compiled and dispatched.
    #[test]
    fn jit_widened_ops_match_tree_walker() {
        let cases = [
            "fn f(x, y) = x & y\nf(12, 10)",
            "fn f(x, y) = x | y\nf(12, 10)",
            "fn f(x, y) = x ^ y\nf(12, 10)",
            "fn f(x) = x & 7\nf(29)",
            "fn f(x) = x << 5\nf(3)",
            "fn f(x) = x >> 2\nf(-13)", // arithmetic (sign-extending) shift
            "fn f(x) = x << 0\nf(7)",
            "fn f(x) = x // 3\nf(-7)", // euclidean floor: -3, not truncating -2
            "fn f(x) = x // 4\nf(-1)",
            "fn f(x) = x // 3\nf(7)",
            "fn f(x) = if x > 0 and x < 10 then 1 else 0\nf(5)",
            "fn f(x) = if x > 0 and x < 10 then 1 else 0\nf(15)",
            "fn f(x) = if x == 1 or x == 2 then 9 else 0\nf(2)",
            "fn f(x) = if x > 0 and x < 100 or x == 500 then 1 else 0\nf(500)",
            "(range(0, 20)).reduce(0, (acc, x) => acc + (x & 1))", // bitmask in a fused kernel
            "(range(0, 20)).filter(x => x > 2 and x % 2 == 0).count()", // and-cond in a filter
            "(range(-5, 6)).reduce(0, (acc, x) => acc + (x // 2))", // floor-div in a kernel
        ];
        for src in cases {
            assert_eq!(run_vm_jit(src), run_tw(src), "JIT ≠ tree-walker on `{src}`");
        }
    }

    /// A map over `>= PAR_MATH_THRESHOLD` (1<<15) elements takes the rayon-chunked native
    /// map kernel; the result must stay byte-identical to the sequential tree-walker
    /// (order-preserving parallelism — each `dst[i]` is `body(src[i])`, no cross-element
    /// accumulation). Covers the i64, f64, mixed (Int→Float), and captured-variable kernels.
    #[test]
    fn parallel_map_kernel_matches_tree_walker() {
        let cases = [
            "print((range(0, 100000)).map(it * 2 + 1).sum())",       // i64 map kernel
            "print(((range(0, 100000)) * 1.0).map(it * 2.0).sum())", // f64 map kernel
            "print((range(0, 100000)).map(it * 1.5).sum())",         // mixed Int→Float kernel
            "k = 7\nprint((range(0, 100000)).map(it * k + k).sum())", // captured var (shared caps)
            "print((range(-50000, 50000)).map((it & 255) ^ 3).sum())", // bitwise body, negatives
        ];
        for src in cases {
            assert_eq!(run_vm_jit(src), run_tw(src), "parallel map JIT ≠ tree-walker on `{src}`");
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
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// Oracle-first safety net for the **nested-fold / captured-reduce JIT** (the O(N²)
    /// pairwise pattern: N-body, distance matrices, all-vs-all). The inner reduce's body
    /// captures the OUTER map variable `i` — today that makes the reduce JIT-ineligible
    /// (the kernel ABI has no capture slot), so `run_vm_jit` runs it on the VM and this
    /// asserts VM == tree-walker. When the capture-aware reduce kernel lands, `run_vm_jit`
    /// engages it with NO change here, and the same assertion becomes JIT == tree-walker
    /// bit-for-bit — the safety net is in place *before* the codegen, as the cardinal rule
    /// (never a silent miscompilation) demands.
    #[test]
    fn differential_captured_reduce_jit() {
        let mut rng = 0xCAFE_D00D_F00D_BABEu64;
        // `i` (the captured outer binder) joins the inner fold's `acc`/`x`.
        let atoms = vec!["acc".to_string(), "x".to_string(), "i".to_string()];
        for _ in 0..10_000 {
            let body = gen_i64_eligible(&mut rng, 3, &atoms);
            let n = (next(&mut rng) % 6) as i64; // small outer range, 0..6
            let start = (next(&mut rng) % 20) as i64 - 5;
            let end = (next(&mut rng) % 20) as i64 - 5;
            let init = (next(&mut rng) % 11) as i64 - 5;
            let src = format!(
                "(range(0, {n})).map(i => (range({start}, {end})).reduce({init}, (acc, x) => ({body}))).sum()"
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "captured reduce JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// The **parallel nested-reduce** (#31) at a size that CROSSES the parallel threshold
    /// (`outer * inner >= PAR_MATH_THRESHOLD`) must stay bit-identical to the tree-walker —
    /// the order-preserving rayon collect over the native inner captured-reduce kernel. The
    /// `differential_captured_reduce_jit` fuzzer uses tiny ranges that stay on the serial
    /// path, so this pins the PARALLEL path specifically (negative starts, captured `i` in
    /// `min`/`max`, wrapping arithmetic — each a distinct inner-kernel shape).
    #[test]
    fn parallel_nested_reduce_matches_tree_walker() {
        let cases = [
            "(range(0, 200)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + (i - j) * (i - j))).sum()",
            "(range(0, 300)).map(i => (range(0, 150)).reduce(0, (acc, j) => acc + i * j)).sum()",
            "(range(-50, 250)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + max(i, j))).sum()",
            "(range(0, 250)).map(i => (range(0, 200)).reduce(1, (acc, j) => acc + (i * i - j))).sum()",
        ];
        for src in cases {
            assert_eq!(run_vm_jit(src), run_tw(src), "parallel nested reduce JIT ≠ tree-walker on `{src}`");
        }
    }

    /// REGRESSION (adversarial-review find): a reverse/empty nested range with EXTREME bounds
    /// (`os = i64::MAX`, `oe = i64::MIN`) must not overflow the span computation. Before the fix
    /// `run_nested_reduce` did `oe - os` in i64 → a debug-profile overflow PANIC while the
    /// tree-walker returned a clean empty result (a JIT ≠ tree-walker divergence the fuzzers,
    /// which emit `i64::MAX`/`i64::MIN`, could hit). Spans are now computed in i128.
    #[test]
    fn nested_reduce_reverse_and_extreme_ranges() {
        let cases = [
            // reverse OUTER range → empty map → sum 0 (would panic on i64 `oe - os`)
            "hi = 9223372036854775807\nlo = 0 - 9223372036854775807 - 1\n(range(hi, lo)).map(i => (range(0, 4)).reduce(0, (acc, j) => acc + i + j)).sum()",
            // reverse INNER range → each inner reduce returns its init → sum of inits (10 * 7)
            "hi = 9223372036854775807\nlo = 0 - 9223372036854775807 - 1\n(range(0, 10)).map(i => (range(hi, lo)).reduce(7, (acc, j) => acc + i + j)).sum()",
            // plain empty outer / empty inner ranges
            "(range(5, 5)).map(i => (range(0, 3)).reduce(0, (acc, j) => acc + i * j)).sum()",
            "(range(0, 5)).map(i => (range(3, 3)).reduce(9, (acc, j) => acc + i + j)).sum()",
        ];
        for src in cases {
            assert_eq!(run_vm_jit(src), run_tw(src), "reverse/extreme nested range JIT ≠ tree-walker on `{src}`");
        }
    }

    /// The **parallel array-indexed nested reduce** (this landing): the all-pairs distance-matrix
    /// shape `range(n).map(i => range(m).reduce(0, (acc,j) => ...a[i]...a[j]...))` at a size that
    /// CROSSES the parallel threshold (`n*m >= 1<<15`), so the outer map parallelizes over the
    /// native inner captured-reduce kernel while it reads captured arrays by BOTH the scalar `i`
    /// and the counter `j`. The array bases are shared read-only across rayon workers; the bounds
    /// pre-check is hoisted ONCE over the whole outer/inner range. Must stay bit-identical to the
    /// tree-walker — the v1c fuzzer uses tiny ranges that stay serial, so this pins the PARALLEL
    /// array-cap path. Covers one array (`a[i]`,`a[j]`), two arrays (`a[i]*b[j]`, two bases in the
    /// template), and a scalar-only array (`a[i]` never counter-indexed → only the point check).
    #[test]
    fn parallel_indexed_nested_reduce_matches_tree_walker() {
        let a: Vec<i64> = (0..220).map(|k| (k * 13 + 7) % 100 - 50).collect();
        let b: Vec<i64> = (0..220).map(|k| (k * 31 + 5) % 50 - 25).collect();
        let (aa, bb) = (fmt_i64_arr(&a), fmt_i64_arr(&b));
        // Deterministic all-pairs shapes, each 200x200 = 40000 >= 1<<15 (parallel), all in-bounds.
        let fixed = [
            format!("a = {aa}\n(range(0, 200)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + abs(a[i] - a[j]))).sum()"),
            format!("a = {aa}\n(range(0, 200)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + (a[i] - a[j]) * (a[i] - a[j]))).sum()"),
            format!("a = {aa}\nb = {bb}\n(range(0, 200)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + a[i] * b[j])).sum()"),
            format!("a = {aa}\n(range(0, 200)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + max(a[i], a[j]))).sum()"),
            // scalar-only array: `a[i]` never counter-indexed — only the point check applies to it.
            format!("a = {aa}\n(range(0, 200)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + a[i] + j)).sum()"),
        ];
        for src in &fixed {
            assert_eq!(run_vm_jit(src), run_tw(src), "parallel indexed nested reduce JIT ≠ tree-walker on `{src}`");
        }
        // Fuzzed `{acc, a[i], a[j]}` bodies at a parallel size (185x185 = 34225 >= 1<<15).
        let mut rng = 0xA11_9A15_EED2_0263u64;
        let atoms = vec!["acc".to_string(), "a[i]".to_string(), "a[j]".to_string()];
        for _ in 0..25 {
            let body = gen_i64_eligible(&mut rng, 3, &atoms);
            let src = format!("a = {aa}\n(range(0, 185)).map(i => (range(0, 185)).reduce(0, (acc, j) => ({body}))).sum()");
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "parallel indexed nested reduce JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// Parallel array-indexed nested reduce — FALLBACK + edge coverage. (1) An outer range that
    /// runs `a[i]` off the end at a PARALLEL size: the hoisted Scalar-bound pre-check
    /// (`[os,oe) ⊆ [0,len)`) declines, falling back to the serial map-of-reduce, which raises the
    /// EXACT interpreter OOB error — identical string on both engines. (2) A counter index `a[j]`
    /// off the end (Counter bound `[is,ie) ⊆ [0,len)` declines) with `a[i]` still in-bounds. (3) A
    /// reverse OUTER range WITH array caps → empty map → `[]` (the i128 span guard, now shared by
    /// the array path, must neither overflow the base-pointer offset math nor deref an empty range).
    #[test]
    fn parallel_indexed_nested_reduce_fallback_and_edges() {
        let a: Vec<i64> = (0..100).map(|k| (k * 7 + 1) % 40).collect();
        let aa = fmt_i64_arr(&a);
        // (1) outer 200 > len 100 → `a[i]` OOB for i >= 100 → both engines raise the identical error.
        let oob_scalar = format!("a = {aa}\n(range(0, 200)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + a[i] + a[j])).sum()");
        let vm = vm_jit_err_msg(&oob_scalar).expect("VM must error (scalar index OOB, parallel size)");
        let tw = tw_err_msg(&oob_scalar).expect("tree-walker must error (scalar index OOB)");
        assert_eq!(vm, tw, "parallel scalar-OOB message must match on `{oob_scalar}`");
        assert!(vm.contains("out of bounds"), "unexpected: {vm}");

        // (2) inner counter 200 > len 100 → `a[j]` OOB; a short outer keeps `a[i]` in-bounds so
        // only the Counter bound trips. Identical error on both engines.
        let oob_counter = format!("a = {aa}\n(range(0, 50)).map(i => (range(0, 200)).reduce(0, (acc, j) => acc + a[i] + a[j])).sum()");
        let vmc = vm_jit_err_msg(&oob_counter).expect("VM must error (counter index OOB)");
        let twc = tw_err_msg(&oob_counter).expect("tree-walker must error (counter index OOB)");
        assert_eq!(vmc, twc, "parallel counter-OOB message must match on `{oob_counter}`");

        // (3) reverse OUTER range with array caps, inner in-bounds → the parallel path runs with an
        // empty `os..oe` (n = 0 via the i128 span guard) → `[]` → sum 0. The base pointer sits in
        // the caps template but is never dereferenced (zero iterations).
        let rev = format!("a = {aa}\nhi = 9223372036854775807\nlo = 0 - 9223372036854775807 - 1\n(range(hi, lo)).map(i => (range(0, 50)).reduce(0, (acc, j) => acc + a[i] + a[j])).sum()");
        assert_eq!(run_vm_jit(&rev), run_tw(&rev), "reverse outer with array caps JIT ≠ tree-walker on `{rev}`");
        assert_eq!(run_vm_jit(&rev), Ok("0".to_string()));
    }

    /// The **parallel TRIANGULAR nested reduce**: an inner range whose bounds are AFFINE in the
    /// outer binder (`range(i + 1, n)` — the all-pairs shape where each unordered pair is counted
    /// once). Each worker computes its OWN inner bounds from its OWN `i`; nothing is hoisted. Sizes
    /// cross the parallel threshold (the per-`i` span is ~n/2, so the trapezoid `n²/2 >= 1<<15`
    /// needs n >= 256 — 300 here), so this pins the PARALLEL affine path, not the serial one.
    /// Covers: upper-triangular start (`i + 1`, `i`), lower-triangular END (`range(0, i)`), a
    /// strided start (`2 * i`) whose per-`i` range goes EMPTY for large `i`, and a both-affine
    /// range (`range(i, 2 * i)`) — each against the tree-walker.
    #[test]
    fn parallel_triangular_nested_reduce_matches_tree_walker() {
        let a: Vec<i64> = (0..640).map(|k| (k * 13 + 7) % 100 - 50).collect();
        let b: Vec<i64> = (0..640).map(|k| (k * 31 + 5) % 50 - 25).collect();
        let (aa, bb) = (fmt_i64_arr(&a), fmt_i64_arr(&b));
        let fixed = [
            // the k4 all-pairs shape itself: `sc = 1` (base `1`), `ec = 0` (base `300`).
            format!("a = {aa}\n(range(0, 300)).map(i => (range(i + 1, 300)).reduce(0, (acc, j) => acc + abs(a[i] - a[j]))).sum()"),
            // start `i` exactly → `sc = 1` over the synthesized `0` base.
            format!("a = {aa}\n(range(0, 300)).map(i => (range(i, 300)).reduce(0, (acc, j) => acc + (a[i] - a[j]) * (a[i] - a[j]))).sum()"),
            // LOWER-triangular: the END is affine (`ec = 1`, base `0`), the start constant.
            format!("a = {aa}\n(range(0, 300)).map(i => (range(0, i)).reduce(0, (acc, j) => acc + abs(a[i] - a[j]))).sum()"),
            // `1 + i` — the binder on the RIGHT of the `+`.
            format!("a = {aa}\n(range(0, 300)).map(i => (range(1 + i, 300)).reduce(0, (acc, j) => acc + max(a[i], a[j]))).sum()"),
            // strided start `2 * i`: the per-`i` range EMPTIES once `2i >= 300`, while the union
            // pre-check bounds only `[min start, max end) = [0, 300)`. Pins that an empty per-`i`
            // range loads nothing even though its start sits outside the checked union.
            format!("a = {aa}\n(range(0, 300)).map(i => (range(2 * i, 300)).reduce(0, (acc, j) => acc + a[i] * b[j])).sum()"),
            // BOTH bounds affine: `range(i, 2 * i)` → `sc = 1`, `ec = 2`; union `[0, 598) ⊆ [0, 640)`.
            format!("a = {aa}\nb = {bb}\n(range(0, 300)).map(i => (range(i, 2 * i)).reduce(0, (acc, j) => acc + a[i] + b[j])).sum()"),
            // two arrays, triangular.
            format!("a = {aa}\nb = {bb}\n(range(0, 300)).map(i => (range(i + 1, 300)).reduce(0, (acc, j) => acc + a[i] * b[j])).sum()"),
        ];
        for src in &fixed {
            assert_eq!(run_vm_jit(src), run_tw(src), "parallel triangular nested reduce JIT ≠ tree-walker on `{src}`");
        }
        // Fuzzed `{acc, a[i], a[j]}` bodies under a triangular range at a parallel size.
        let mut rng = 0x7B1A_9C1A_B00F_1234u64;
        let atoms = vec!["acc".to_string(), "a[i]".to_string(), "a[j]".to_string()];
        for _ in 0..25 {
            let body = gen_i64_eligible(&mut rng, 3, &atoms);
            let src = format!("a = {aa}\n(range(0, 300)).map(i => (range(i + 1, 300)).reduce(0, (acc, j) => ({body}))).sum()");
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "triangular nested reduce JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// ENGAGEMENT probe (the "engagement ≠ correctness" trap). The differential tests above would
    /// pass just as happily if the triangular shape silently DECLINED and the serial map-of-reduce
    /// produced the same correct numbers — which is precisely the bug being fixed, so correctness
    /// alone cannot pin it. The counter discriminates: `run_nested_reduce_arrays` notes exactly ONE
    /// native call on the CALLING thread (its rayon workers bump their own thread-locals), whereas
    /// the declined fallback drives `call_reduce_caps` once per outer `i` — ~300 notes. The
    /// rectangular control proves the probe reads the same way on a shape that always engaged.
    #[test]
    fn triangular_nested_reduce_actually_engages_the_nested_path() {
        let a: Vec<i64> = (0..320).map(|k| (k * 13 + 7) % 100 - 50).collect();
        let aa = fmt_i64_arr(&a);
        let probe = |inner: &str| -> u64 {
            let src = format!(
                "a = {aa}\n(range(0, 300)).map(i => ({inner}).reduce(0, (acc, j) => acc + abs(a[i] - a[j]))).sum()"
            );
            crate::jit::reset_native_call_count();
            let got = run_vm_jit(&src);
            assert!(got.is_ok(), "kernel failed on `{src}`: {got:?}");
            crate::jit::native_call_count()
        };
        let rect = probe("(range(0, 300))");
        let tri = probe("(range(i + 1, 300))");
        assert_eq!(
            tri, rect,
            "the TRIANGULAR nested reduce does not engage the nested path the way the rectangular \
             one does: {tri} native calls vs {rect}. One dispatch = the parallel nested path; a \
             per-`i` count (~300) = the serial fallback — the gap this change fixes."
        );
        assert!(tri <= 2, "expected a single nested dispatch, got {tri} native calls");
    }

    #[test]
    fn zz_probe_discriminates_scratch() {
        let a: Vec<i64> = (0..320).map(|k| (k * 13 + 7) % 100 - 50).collect();
        let aa = fmt_i64_arr(&a);
        let probe = |inner: &str| -> u64 {
            let src = format!(
                "a = {aa}\n(range(0, 300)).map(i => ({inner}).reduce(0, (acc, j) => acc + abs(a[i] - a[j]))).sum()"
            );
            crate::jit::reset_native_call_count();
            let _ = run_vm_jit(&src);
            crate::jit::native_call_count()
        };
        eprintln!("PROBE RECT range(0,300)       -> {}", probe("(range(0, 300))"));
        eprintln!("PROBE TRI  range(i+1,300)     -> {}", probe("(range(i + 1, 300))"));
        eprintln!("PROBE NONAFFINE range(i*i,300)-> {}", probe("(range(i * i, 300))"));
        eprintln!("PROBE BINARY-NOI range(1+1,300)-> {}", probe("(range(1 + 1, 300))"));
    }

    /// TRIANGULAR nested reduce — the pre-check's decline paths. The per-`i` inner range varies, so
    /// the bounds obligation lands on the UNION `[min_i start(i), max_i end(i))`; when that union
    /// escapes the array the parallel path must DECLINE to the serial map-of-reduce, which raises
    /// the EXACT interpreter error — identical string on both engines. Also pins the affine
    /// OVERFLOW guard (a bound that leaves `i64` for some `i` must fall back rather than wrap in a
    /// worker) and a NON-affine bound (`i * i`), which declines as it always has.
    #[test]
    fn triangular_nested_reduce_bounds_and_edges() {
        let a: Vec<i64> = (0..100).map(|k| (k * 7 + 1) % 40).collect();
        let aa = fmt_i64_arr(&a);
        // Counter union `[1, 200)` escapes len 100 → decline → the serial path's exact OOB error.
        let oob = format!("a = {aa}\n(range(0, 50)).map(i => (range(i + 1, 200)).reduce(0, (acc, j) => acc + a[i] + a[j])).sum()");
        let vm = vm_jit_err_msg(&oob).expect("VM must error (triangular counter OOB)");
        let tw = tw_err_msg(&oob).expect("tree-walker must error (triangular counter OOB)");
        assert_eq!(vm, tw, "triangular counter-OOB message must match on `{oob}`");
        assert!(vm.contains("out of bounds"), "unexpected: {vm}");

        // Scalar bound: outer 200 > len 100 → `a[i]` OOB → decline → identical error.
        let oob_s = format!("a = {aa}\n(range(0, 200)).map(i => (range(i + 1, 200)).reduce(0, (acc, j) => acc + a[i] + a[j])).sum()");
        assert_eq!(
            vm_jit_err_msg(&oob_s).expect("VM must error"),
            tw_err_msg(&oob_s).expect("tree-walker must error"),
            "triangular scalar-OOB message must match on `{oob_s}`"
        );

        let edges = [
            // affine start that OVERFLOWS i64 for i >= 1 → the `fits` guard declines to the serial
            // path, which wraps `i + hi` exactly as the tree-walker does.
            "hi = 9223372036854775807\n(range(0, 10)).map(i => (range(i + hi, 4)).reduce(0, (acc, j) => acc + i + j)).sum()".to_string(),
            // affine END that overflows.
            "hi = 9223372036854775807\n(range(0, 10)).map(i => (range(0, i + hi)).reduce(0, (acc, j) => acc + i + j)).sum()".to_string(),
            // NON-affine inner bound (`i * i`) → declines, serial, still correct.
            format!("a = {aa}\n(range(0, 9)).map(i => (range(i * i, 81)).reduce(0, (acc, j) => acc + i + j)).sum()"),
            // empty outer range with a triangular inner → `[]` → 0.
            "(range(5, 5)).map(i => (range(i + 1, 30)).reduce(0, (acc, j) => acc + i * j)).sum()".to_string(),
            // triangular over a NEGATIVE outer start: start(i) = i+1 goes negative → the union's
            // `inner_lo < 0` declines (the serial path Python-wraps a negative index).
            format!("a = {aa}\n(range(-5, 40)).map(i => (range(i + 1, 40)).reduce(0, (acc, j) => acc + a[j])).sum()"),
            // whole triangle empty for every i (start always past end) → sum of inits.
            "(range(0, 20)).map(i => (range(i + 100, 30)).reduce(7, (acc, j) => acc + i + j)).sum()".to_string(),
            // tiny triangular, BELOW the parallel threshold → the serial affine route.
            "(range(0, 12)).map(i => (range(i + 1, 12)).reduce(0, (acc, j) => acc + i * j)).sum()".to_string(),
        ];
        for src in &edges {
            assert_eq!(run_vm_jit(src), run_tw(src), "triangular edge JIT ≠ tree-walker on `{src}`");
        }
    }

    /// The **multi-accumulator i64 reduce** (K=4 partials over a K-strided main loop + a remainder
    /// tail, combined at exit) must equal the single-accumulator fold BYTE-FOR-BYTE across the
    /// K-boundary edges — empty, `len < K` (tail only), `len = K·m` (no tail), `len = K·m + r`
    /// (main + tail), and reverse/extreme ranges. Integer add is associative + commutative, so the
    /// partitioned sum is identical; this pins the main/tail split (an off-by-one there is the one
    /// real risk of the transform) against BOTH the bytecode VM and the tree-walker, including the
    /// array-indexed (captured) shape that rides the caps/index machinery.
    #[test]
    fn multiacc_reduce_matches_across_k_boundary() {
        let mut cases: Vec<String> = Vec::new();
        for n in [0, 1, 2, 3, 4, 5, 7, 8, 9, 12, 13, 16, 17, 100, 401] {
            cases.push(format!("(range(0, {n})).reduce(0, (c, k) => c + k)"));
            cases.push(format!("(range(0, {n})).reduce(3, (c, k) => c + k * k)"));
            cases.push(format!("(range(0, {n})).reduce(0, (c, k) => c + abs(k - 7))"));
        }
        // array-indexed (captured) sums — multi-acc through the caps/index machinery, at K-edges.
        for n in [0, 1, 4, 5, 7, 8, 11] {
            cases.push(format!(
                "a = [3, 1, 4, 1, 5, 9, 2, 6, 5, 3, 5]\n(range(0, {n})).reduce(0, (c, j) => c + a[j]*a[j])"
            ));
        }
        // REGRESSION (adversarial-review find): a `term` that references the accumulator through a
        // `let` body — `acc + (let d = 0 in acc)` — must NOT be treated as multi-acc-eligible (the
        // term is not accumulator-free). Before the fix `expr_uses_ident` returned false for the
        // `let` (unhandled node → `_ => false`), so multi-acc engaged and panicked (the accumulator
        // is absent from the partials' vars). It must now fall back to the single-accumulator fold.
        cases.push("(range(0, 6)).reduce(1, (acc, i) => acc + (let d = 0 in acc + i))".to_string());
        cases.push("(range(0, 6)).reduce(2, (acc, i) => acc + (if i > 2 then i else 0))".to_string());
        // empty + reverse/extreme ranges → the empty fold returns `init` on every engine.
        cases.push("(range(5, 5)).reduce(7, (c, k) => c + k)".to_string());
        cases.push(
            "hi = 9223372036854775807\nlo = 0 - 9223372036854775807 - 1\n(range(hi, lo)).reduce(9, (c, k) => c + k)"
                .to_string(),
        );
        for src in &cases {
            assert_eq!(run_vm_jit(src), run_tw(src), "multi-acc reduce JIT ≠ tree-walker on `{src}`");
            assert_eq!(run_vm_jit(src), run_vm_no_jit(src), "multi-acc JIT ≠ bytecode VM on `{src}`");
        }
    }

    /// The **array-indexed reduce kernel** (`arr[counter]`, the dot-product / weighted-sum
    /// pattern) must equal the tree-walker. The inner fold reads two captured `Int` arrays by
    /// the loop counter `j`; when the range is in-bounds the JIT engages the native kernel with
    /// its base-pointer captures, when it runs off the end the VM's bounds pre-check falls back
    /// to the bytecode `Op::Index` — this diffs BOTH paths against the tree-walker. Bodies are
    /// random `i64`-eligible expressions over `{acc, a[j], b[j]}`, so `+ - * min max abs` and
    /// literals combine with the indexed reads exactly as a real kernel would.
    #[test]
    fn differential_dot_product_reduce_jit() {
        let mut rng = 0xD07_9403_1CE5_0FF5u64;
        let atoms = vec!["acc".to_string(), "a[j]".to_string(), "b[j]".to_string()];
        for _ in 0..5_000 {
            let len = 1 + (next(&mut rng) % 8) as i64; // 1..=8
            let a: Vec<i64> = (0..len).map(|_| (next(&mut rng) % 21) as i64 - 10).collect();
            let b: Vec<i64> = (0..len).map(|_| (next(&mut rng) % 21) as i64 - 10).collect();
            // `n` spans in-bounds (native engages) AND past-the-end (pre-check → fallback → OOB).
            let n = (next(&mut rng) % (len as u64 + 3)) as i64;
            let init = (next(&mut rng) % 11) as i64 - 5;
            let body = gen_i64_eligible(&mut rng, 3, &atoms);
            let src = format!(
                "a = {}\nb = {}\n(range(0, {n})).reduce({init}, (acc, j) => ({body}))",
                fmt_i64_arr(&a),
                fmt_i64_arr(&b),
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "dot-product JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// When `arr[j]` runs out of bounds the VM's pre-check must fall back to the bytecode
    /// loop, which raises the SAME out-of-bounds error the tree-walker does — identical
    /// message, not merely identical `Err` outcome (proving the native kernel never silently
    /// swallows an access the interpreter would reject). Covers `end > len` and a negative
    /// start whose Python-wrap is still out of range.
    #[test]
    fn differential_indexed_reduce_oob_fallback() {
        let over = "a = [10, 20, 30]\n(range(0, 5)).reduce(0, (acc, j) => acc + a[j])";
        let vm = vm_jit_err_msg(over).expect("VM must error (end > len)");
        let tw = tw_err_msg(over).expect("tree-walker must error (end > len)");
        assert_eq!(vm, tw, "OOB message must match across engines on `{over}`");
        assert!(vm.contains("out of bounds for length 3"), "unexpected OOB message: {vm}");

        // `start < 0` whose wrap `a[len + j]` is STILL out of range: the pre-check's `s < 0`
        // arm forces the fallback, where the interpreter wraps and then raises.
        let neg = "a = [10, 20, 30]\n(range(-100, 3)).reduce(0, (acc, j) => acc + a[j])";
        let vm2 = vm_jit_err_msg(neg).expect("VM must error (negative wrap OOB)");
        let tw2 = tw_err_msg(neg).expect("tree-walker must error (negative wrap OOB)");
        assert_eq!(vm2, tw2, "negative-index OOB message must match across engines on `{neg}`");
        assert!(vm2.contains("out of bounds"), "unexpected negative OOB message: {vm2}");
    }

    /// A negative start whose Python-wrap IS in range (`a[-1] == a[len-1]`): the kernel would
    /// do a raw (wrong) load, so the `s < 0` pre-check must force the fallback where the
    /// interpreter wraps. Both engines must agree on the wrapped result — proving the native
    /// path is never wrongly taken on a negative counter. Plus the empty-array boundary.
    #[test]
    fn differential_indexed_reduce_edge_cases() {
        // Negative counters that wrap into range: j = -2, -1 → a[2] + a[3] = 30 + 40 = 70.
        let wrap = "a = [10, 20, 30, 40]\n(range(-2, 0)).reduce(0, (acc, j) => acc + a[j])";
        assert_eq!(run_tw(wrap), Ok("70".to_string()), "tree-walker wrap result");
        assert_eq!(run_vm_jit(wrap), run_tw(wrap), "wrapped negative index JIT ≠ tree-walker");

        // Empty array + empty range → just the init (zero loads); both engines agree.
        let empty_ok = "a = []\n(range(0, 0)).reduce(7, (acc, j) => acc + a[j])";
        assert_eq!(run_vm_jit(empty_ok), Ok("7".to_string()), "empty range must yield init");
        assert_eq!(run_vm_jit(empty_ok), run_tw(empty_ok));

        // Empty array + a real range → `a[0]` is OOB on both (pre-check `e=1 > len=0`),
        // with the IDENTICAL error message.
        let empty_oob = "a = []\n(range(0, 1)).reduce(0, (acc, j) => acc + a[j])";
        let (j, t) = (run_vm_jit(empty_oob), run_tw(empty_oob));
        assert!(j.is_err(), "index into empty array must error");
        assert_eq!(j, t, "empty-array OOB must raise the identical message");
    }

    /// The O(N²) flagship (all-pairs / N-body inner sum): an outer `map`'s scalar `k` AND an
    /// indexed array `xs[j]` are BOTH captured by one inner reduce — a `Scalar` value and an
    /// `ArrayI64` base pointer in the SAME `caps` buffer, ordered by first appearance. Exercises
    /// the mixed-kind marshalling on both engines (and its out-of-bounds fallback).
    #[test]
    fn indexed_reduce_mixed_scalar_and_array_caps() {
        let cases = [
            "xs = [2, 3, 5, 7, 11]\n(range(0, 3)).map(k => (range(0, 5)).reduce(0, (acc, j) => acc + k * xs[j])).sum()",
            "xs = [1, -2, 3, -4]\n(range(1, 4)).map(k => (range(0, 4)).reduce(k, (acc, j) => acc + xs[j] - k)).sum()",
            // inner range runs off `xs` → every outer iteration falls back; both engines error.
            "xs = [1, 2, 3]\n(range(0, 2)).map(k => (range(0, 9)).reduce(0, (acc, j) => acc + k + xs[j])).sum()",
        ];
        for src in cases {
            assert_eq!(run_vm_jit(src), run_tw(src), "mixed-caps indexed reduce JIT ≠ tree-walker on `{src}`");
        }
    }

    /// The **f64 array-indexed reduce kernel** (v1b) — the float dot product, the scientific
    /// flagship. Two captured `Float` arrays are read by the loop counter and folded into an
    /// `f64` accumulator (`0.0` init). `.reduce` is naive left-to-right, so the kernel's
    /// `fmul`/`fadd` in source order must be BIT-identical to the tree-walker. Random
    /// `+ - * min max abs` bodies over `{acc, a[j], b[j]}`, in-bounds (native) and past-the-end
    /// (pre-check → fallback → OOB) alike.
    #[test]
    fn differential_f64_dot_product_reduce_jit() {
        let mut rng = 0xF10A_7D07_1DEA_5EEDu64;
        let atoms = vec!["acc".to_string(), "a[j]".to_string(), "b[j]".to_string()];
        for _ in 0..5_000 {
            let len = 1 + (next(&mut rng) % 8) as i64; // 1..=8
            let a: Vec<i64> = (0..len).map(|_| (next(&mut rng) % 21) as i64 - 10).collect();
            let b: Vec<i64> = (0..len).map(|_| (next(&mut rng) % 21) as i64 - 10).collect();
            let n = (next(&mut rng) % (len as u64 + 3)) as i64;
            let body = gen_i64_eligible(&mut rng, 3, &atoms);
            let src = format!(
                "a = {}\nb = {}\n(range(0, {n})).reduce(0.0, (acc, j) => ({body}))",
                fmt_f64_arr(&a),
                fmt_f64_arr(&b),
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "f64 dot-product JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// The `Floats`-array pre-check + fallback (v1b): an out-of-bounds `a[j]` on a `Float`
    /// array must fall back to the bytecode loop and raise the exact tree-walker error; an
    /// empty range yields the `f64` init on both engines.
    #[test]
    fn differential_f64_indexed_reduce_oob_fallback() {
        let oob = "a = [1.0, 2.0, 3.0]\n(range(0, 5)).reduce(0.0, (acc, j) => acc + a[j])";
        let vm = vm_jit_err_msg(oob).expect("VM must error (f64 OOB)");
        let tw = tw_err_msg(oob).expect("tree-walker must error (f64 OOB)");
        assert_eq!(vm, tw, "f64 OOB message must match across engines on `{oob}`");
        assert!(vm.contains("out of bounds for length 3"), "unexpected f64 OOB message: {vm}");

        // Empty range → just the f64 init (a is a non-`Floats` empty array → pre-check falls
        // back, but the empty range folds zero elements, so both engines return the init).
        let empty_ok = "a = []\n(range(0, 0)).reduce(2.5, (acc, j) => acc + a[j])";
        assert_eq!(run_vm_jit(empty_ok), run_tw(empty_ok));
        assert_eq!(run_vm_jit(empty_ok), Ok("2.5".to_string()), "empty range must yield the f64 init");
    }

    /// REGRESSION (adversarial-review find): a `let` inside the reduce body that REBINDS the
    /// counter binder `j` must never let the kernel do an unchecked load at the let-bound
    /// index. Before the fix, `let j = 100 in a[j]` slipped past the `idx == pb` gate (matched
    /// syntactically) and the native kernel read `a[100]` — far out of bounds — returning heap
    /// garbage while the VM/tree-walker correctly raised OOB. The fix rejects any counter-
    /// shadowing `let` from the JIT (falls back to the exact-erroring bytecode loop).
    #[test]
    fn indexed_reduce_counter_shadow_is_safe() {
        // Out-of-range shadow → both engines raise the identical OOB error (no wild native read).
        let oob = "a = [10, 20, 30]\n(range(0, 3)).reduce(0, (acc, j) => acc + (let j = 100 in a[j]))";
        let vm = vm_jit_err_msg(oob).expect("VM must error (shadowed OOB index)");
        let tw = tw_err_msg(oob).expect("tree-walker must error (shadowed OOB index)");
        assert_eq!(vm, tw, "shadowed-index OOB message must match across engines on `{oob}`");
        assert!(vm.contains("out of bounds for length 3"), "unexpected shadowed OOB message: {vm}");

        // In-range shadow → both engines read a[1] each step = 60 (fallback is correct, not just safe).
        let inrange = "a = [10, 20, 30]\n(range(0, 3)).reduce(0, (acc, j) => acc + (let j = 1 in a[j]))";
        assert_eq!(run_vm_jit(inrange), run_tw(inrange), "shadowed in-range JIT ≠ tree-walker");
        assert_eq!(run_vm_jit(inrange), Ok("60".to_string()), "shadowed index must read a[1] each step");

        // A NON-counter `let` (x doesn't shadow j) stays eligible AND correct: (10+5)+(20+5)+(30+5).
        let ok = "a = [10, 20, 30]\n(range(0, 3)).reduce(0, (acc, j) => acc + a[j] + (let x = 5 in x))";
        assert_eq!(run_vm_jit(ok), run_tw(ok), "non-counter let JIT ≠ tree-walker");
        assert_eq!(run_vm_jit(ok), Ok("75".to_string()));
    }

    /// The **captured-scalar array index** (v1c): `a[i]` where `i` is a captured scalar (the
    /// outer `map` binder), not the loop counter — the all-pairs shape (integer distance /
    /// Hamming matrices: `abs(codes[i] - codes[j])`, where `codes[j]` is the v1a counter index
    /// and `codes[i]` the new scalar index). The VM point-checks `0 <= i < len` before the
    /// native load; an out-of-range `i` (outer range past the array) falls back to the exact
    /// interpreter error. Random `{acc, a[i], a[j]}` bodies over in- and out-of-bounds outers.
    #[test]
    fn differential_scalar_indexed_reduce_jit() {
        let mut rng = 0x5CA1_1DEA_BCD1_2345u64;
        let atoms = vec!["acc".to_string(), "a[i]".to_string(), "a[j]".to_string()];
        for _ in 0..4_000 {
            let len = (1 + next(&mut rng) % 8) as i64; // 1..=8
            let a: Vec<i64> = (0..len).map(|_| (next(&mut rng) % 21) as i64 - 10).collect();
            let outer = (next(&mut rng) % (len as u64 + 2)) as i64; // sometimes i runs off `a`
            let n = (next(&mut rng) % (len as u64 + 2)) as i64;
            let body = gen_i64_eligible(&mut rng, 2, &atoms);
            let src = format!(
                "a = {}\n(range(0, {outer})).map(i => (range(0, {n})).reduce(0, (acc, j) => ({body}))).sum()",
                fmt_i64_arr(&a),
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "scalar-indexed reduce JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// Focused v1c edges: a scalar-indexed array with `len < inner range` (an array read ONLY
    /// by the scalar, never the counter — no range check applies, only the point check), and an
    /// out-of-bounds scalar index that must fall back to the identical OOB error on both engines.
    #[test]
    fn scalar_indexed_reduce_edge_cases() {
        // `a` (len 3) indexed only by the scalar `i`, inner range 100 (> len) — the point check
        // `i < 3` passes, the range check must NOT apply. Sum over 100 of a[i] = 100 * a[i].
        let only_scalar = "a = [10, 20, 30]\n(range(0, 3)).map(i => (range(0, 100)).reduce(0, (acc, j) => acc + a[i])).sum()";
        assert_eq!(run_vm_jit(only_scalar), run_tw(only_scalar), "scalar-only index JIT ≠ tree-walker");
        assert_eq!(run_vm_jit(only_scalar), Ok("6000".to_string())); // 100*(10+20+30)

        // Outer range past `a` → `a[i]` OOB for i >= 3 → both engines raise the identical error.
        let oob = "a = [10, 20, 30]\n(range(0, 5)).map(i => (range(0, 4)).reduce(0, (acc, j) => acc + a[i] + a[j])).sum()";
        let vm = vm_jit_err_msg(oob).expect("VM must error (scalar index OOB)");
        let tw = tw_err_msg(oob).expect("tree-walker must error (scalar index OOB)");
        assert_eq!(vm, tw, "scalar-index OOB message must match on `{oob}`");
        assert!(vm.contains("out of bounds"), "unexpected: {vm}");
    }

    /// Format a `[i64]` array literal (`[1, 2, 3]`) for a fuzzed source program.
    fn fmt_i64_arr(xs: &[i64]) -> String {
        let inner: Vec<String> = xs.iter().map(|v| v.to_string()).collect();
        format!("[{}]", inner.join(", "))
    }

    /// Format `[i64]` as an exact-valued `[f64]` literal (`[1.0, -2.0]`) — whole-number floats
    /// parse identically in both engines and keep the fold bit-exact.
    fn fmt_f64_arr(xs: &[i64]) -> String {
        let inner: Vec<String> = xs.iter().map(|v| format!("{}.0", v)).collect();
        format!("[{}]", inner.join(", "))
    }

    /// Run `src` on the JIT-enabled VM, returning the error *message* (not just `()`), so a
    /// fallback path's exact diagnostic can be diffed against the tree-walker's.
    fn vm_jit_err_msg(src: &str) -> Option<String> {
        let toks = lexer::lex(src).ok()?;
        let ast = parser::parse(toks).ok()?;
        let mut prog = bytecode::compile_with_types(&ast, None).ok()?;
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
            &prog.scan_loops,
        );
        match exec(&prog, jit.as_ref()) {
            Ok(_) => None,
            Err(e) => Some(e.message),
        }
    }

    /// Run `src` on the tree-walker, returning the first error *message* (companion to
    /// [`vm_jit_err_msg`]).
    fn tw_err_msg(src: &str) -> Option<String> {
        let toks = lexer::lex(src).ok()?;
        let ast = parser::parse(toks).ok()?;
        let mut interp = Interp::new();
        for stmt in &ast {
            if let Err(e) = interp.exec(stmt) {
                return Some(e.message);
            }
        }
        None
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
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// The fold-JIT over a **record** accumulator (`{s: …, n: …}`) — slot accesses are
    /// `a.field` (mapped to the field's init position), and the result is rebuilt as a
    /// record reusing the init's symbols. Array source, sometimes staged, so `run_vm_jit`
    /// engages the native multi-slot kernel. Asserts JIT == tree-walker across 10k folds.
    #[test]
    fn differential_record_reduce_jit() {
        let mut rng = 0xD15E_A5ED_F00D_1357u64;
        let atoms = vec!["a.s".to_string(), "a.n".to_string(), "x".to_string()];
        for _ in 0..10_000 {
            let nlen = 1 + pick(&mut rng, 8);
            let elems: Vec<String> =
                (0..nlen).map(|_| format!("{}", (next(&mut rng) % 41) as i64 - 20)).collect();
            let arr = format!("[{}]", elems.join(", "));
            let recv = match pick(&mut rng, 3) {
                0 => format!("({}).filter(x => x % 2 == 0)", arr),
                1 => format!("({}).map(x => x + 1)", arr),
                _ => arr,
            };
            let e0 = gen_i64_eligible(&mut rng, 3, &atoms);
            let e1 = gen_i64_eligible(&mut rng, 3, &atoms);
            let i0 = (next(&mut rng) % 41) as i64 - 20;
            let i1 = (next(&mut rng) % 41) as i64 - 20;
            let src = format!(
                "({}).reduce({{s: {}, n: {}}}, (a, x) => {{s: {}, n: {}}})",
                recv, i0, i1, e0, e1
            );
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "record reduce JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vmjit={v:?} tw={t:?}"),
            }
        }
    }

    /// The fold-JIT over an **array** source (the fused-kernel path, not the range loop):
    /// a 2-tuple `i64` accumulator folded over a small int-array literal, sometimes behind
    /// a `filter`/`map` stage — so `run_vm_jit` engages the native fused tuple-reduce
    /// kernel. Asserts JIT == tree-walker bit-for-bit across 10k folds.
    #[test]
    fn differential_tuple_reduce_fused_jit() {
        let mut rng = 0xFA57_F01D_0BAD_CAFEu64;
        let atoms = vec!["a[0]".to_string(), "a[1]".to_string(), "x".to_string()];
        for _ in 0..10_000 {
            let nlen = 1 + pick(&mut rng, 8);
            let elems: Vec<String> =
                (0..nlen).map(|_| format!("{}", (next(&mut rng) % 41) as i64 - 20)).collect();
            let arr = format!("[{}]", elems.join(", "));
            // optionally a stage, exercising the staged fused-reduce path.
            let recv = match pick(&mut rng, 3) {
                0 => format!("({}).filter(x => x % 2 == 0)", arr),
                1 => format!("({}).map(x => x + 1)", arr),
                _ => arr,
            };
            let e0 = gen_i64_eligible(&mut rng, 3, &atoms);
            let e1 = gen_i64_eligible(&mut rng, 3, &atoms);
            let i0 = (next(&mut rng) % 41) as i64 - 20;
            let i1 = (next(&mut rng) % 41) as i64 - 20;
            let src =
                format!("({}).reduce(({}, {}), (a, x) => ({}, {}))", recv, i0, i1, e0, e1);
            match (run_vm_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "fused tuple reduce JIT ≠ tree-walker on `{src}`"),
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
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
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
                (v, t) => panic!("OUTCOME divergence on `{src}`: vm={v:?} tw={t:?}"),
            }
        }
    }

    /// Run `src` on the VM with the JIT *disabled* (no native kernels) — the pure
    /// bytecode-loop oracle that `run_vm_jit` is diffed against for the array/fused
    /// kernels (which only engage when a `Jit` is supplied).
    fn run_vm_no_jit(src: &str) -> Result<String, String> {
        run_vm(src)
    }

    /// Drive `n` generated programs through the JIT / bytecode-VM / tree-walker triple and
    /// assert all three agree: `Ok` values equal, `Err` messages equal, outcomes never
    /// diverge. Crucially it ALSO asserts the JIT actually *engaged* (native trampolines
    /// fired at least once). Without that check a per-kernel oracle passes VACUOUSLY the
    /// moment its kernel stops being JIT-compiled: the native path silently falls back to
    /// the bytecode VM, so `jit == no_jit` holds trivially and the "oracle" proves nothing
    /// about native code — the "engagement ≠ correctness" trap the counter exists to close.
    /// `label` names the kernel in failure messages. The engagement counter is thread-local,
    /// so oracles running in parallel never clobber each other's count.
    fn triple_oracle(label: &str, seed: u64, n: usize, mut generate: impl FnMut(&mut u64) -> String) {
        let mut rng = seed;
        crate::jit::reset_native_call_count();
        for _ in 0..n {
            let src = generate(&mut rng);
            match (run_vm_jit(&src), run_vm_no_jit(&src), run_tw(&src)) {
                (Ok(a), Ok(b), Ok(c)) => {
                    assert_eq!(a, b, "{label}: JIT ≠ bytecode VM on `{src}`");
                    assert_eq!(b, c, "{label}: bytecode VM ≠ tree-walker on `{src}`");
                }
                (Err(ea), Err(eb), Err(ec)) => {
                    assert_eq!(ea, eb, "{label}: error-message divergence on `{src}`");
                    assert_eq!(eb, ec, "{label}: error-message divergence on `{src}`");
                }
                (j, nj, t) => {
                    panic!("{label}: OUTCOME divergence on `{src}`: jit={j:?} nojit={nj:?} tw={t:?}")
                }
            }
        }
        assert!(
            crate::jit::native_call_count() > 0,
            "{label}: the JIT never engaged across {n} programs — the oracle was silently \
             testing only the bytecode VM, not native code"
        );
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
        // An Int-typed subexpression over `it` using the FULL i64-closed op set the mixed
        // kernel now supports (`+ - * % // & | ^ << >>`), with valid right operands (a positive
        // const for `%`/`//`, an in-range shift amount). Its integer subexpressions wrap as
        // `i64`; only a trailing `* <float>` promotes to `f64` — so `(it % 97) * 1.0` and the
        // like now JIT instead of falling to the VM.
        fn int_body(rng: &mut u64) -> String {
            let mut e = "it".to_string();
            for _ in 0..(1 + pick(rng, 3)) {
                e = match pick(rng, 9) {
                    0 => format!("({} + {})", e, ilit(rng)),
                    1 => format!("({} - {})", e, ilit(rng)),
                    2 => format!("({} * {})", e, ilit(rng)),
                    3 => format!("({} % {})", e, 1 + next(rng) % 40),
                    4 => format!("({} // {})", e, 1 + next(rng) % 40),
                    5 => format!("({} & {})", e, ilit(rng)),
                    6 => format!("({} | {})", e, ilit(rng)),
                    7 => format!("({} ^ {})", e, ilit(rng)),
                    _ => format!("({} {} {})", e, if pick(rng, 2) == 0 { "<<" } else { ">>" }, next(rng) % 16),
                };
            }
            e
        }
        // Force a `Float` root: either an int subexpression promoted by `* <float>` (exercises
        // the new ops in a mixed body), or the original `{+,-,*}` mixed chain.
        let core = if pick(rng, 2) == 0 {
            format!("({} * {})", int_body(rng), flit(rng))
        } else {
            let mut body = "it".to_string();
            for _ in 0..(1 + pick(rng, 3)) {
                let op = ["+", "-", "*"][pick(rng, 3) as usize];
                let opd = if pick(rng, 2) == 0 { ilit(rng) } else { flit(rng) };
                body = format!("({} {} {})", body, op, opd);
            }
            format!("({} * {})", body, flit(rng))
        };
        let int_core = int_body(rng); // pure `Int` (for `sqrt(Int)` etc.)
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
        triple_oracle("mixed map", 0x3117_C0DE_BEEF_2026, 15_000, gen_mixed_map_pipeline);
    }

    /// Triple oracle for the `f64` map kernel: a random `Float`-array map pipeline must
    /// render the *same* array on the JIT-native path, the pure bytecode loop, and the
    /// tree-walker. Guards the monomorphized `f64` kernel + its `f64`-capture passing.
    #[test]
    fn differential_float_map_kernel_oracle() {
        triple_oracle("f64 map", 0xF10A_7C0D_E5EE_9001, 15_000, gen_float_map_pipeline);
    }

    /// A random pure-`f64` fold over a `Float` array: `[…].reduce(<float>, (acc, x) => body)`
    /// where `body` is `+ - *` / `sqrt(abs …)` / `min` / `max` over `{acc, x, float}`. Helix's
    /// `.reduce` is naive left-to-right, so the kernel's straight `fadd`/`fmul` (same order as
    /// the interpreter — NOT the Neumaier path that `.sum()` uses) is **bit-exact**; that is
    /// what makes the f64 reduce kernel safe, and this is its safety net.
    fn gen_float_reduce(rng: &mut u64) -> String {
        fn flit(rng: &mut u64) -> String {
            format!("{}.{:03}", (next(rng) % 21) as i64 - 10, next(rng) % 1000)
        }
        fn atom(rng: &mut u64) -> String {
            match pick(rng, 3) {
                0 => "acc".to_string(),
                1 => "x".to_string(),
                _ => flit(rng),
            }
        }
        fn expr(rng: &mut u64, depth: u32) -> String {
            if depth == 0 || pick(rng, 2) == 0 {
                return atom(rng);
            }
            match pick(rng, 6) {
                0 => format!("(({}) + ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                1 => format!("(({}) - ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                2 => format!("(({}) * ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                // `abs` keeps the radicand ≥ 0 → no NaN to complicate the diff.
                3 => format!("sqrt(abs({}))", expr(rng, depth - 1)),
                4 => format!("min(({}), ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                _ => format!("max(({}), ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
            }
        }
        // Small float array (bounded magnitudes → the fold can't drift to ±inf/NaN).
        let n = 1 + pick(rng, 6);
        let elems: Vec<String> = (0..n).map(|_| flit(rng)).collect();
        format!("([{}]).reduce({}, (acc, x) => ({}))", elems.join(", "), flit(rng), expr(rng, 3))
    }

    /// Triple oracle for the **f64 reduce** kernel. Until the kernel lands the fold runs on
    /// the bytecode VM, so this asserts VM == tree-walker; once it lands, `run_vm_jit` engages
    /// the native f64 fold with no change here and the same assertion becomes JIT == VM ==
    /// tree-walker bit-for-bit. (Engagement is confirmed by a benchmark, not a green oracle.)
    #[test]
    fn differential_float_reduce_oracle() {
        triple_oracle("f64 reduce", 0xF01D_F10A_7C0D_2026, 15_000, gen_float_reduce);
    }

    /// A **range-source** f64 reduce: `range(s, e).reduce(flit, (acc, x) => body)` where the
    /// accumulator `acc` is `f64` but the element `x` is the `i64` range counter — so the body
    /// is MIXED (`acc + x*x`: the integer subexpression `x*x` wraps as `i64`, then promotes to
    /// `f64` at the first float operand, exactly as the interpreter's `arith`). Locks the
    /// typed reduce codegen against the bytecode VM and the tree-walker. Ineligible bodies
    /// (Int root, or a mixed-kind `min`/`max`) fall back on every engine — still parity.
    fn gen_range_float_reduce(rng: &mut u64) -> String {
        fn flit(rng: &mut u64) -> String {
            format!("{}.{:03}", (next(rng) % 21) as i64 - 10, next(rng) % 1000)
        }
        fn atom(rng: &mut u64) -> String {
            match pick(rng, 4) {
                0 => "acc".to_string(),
                1 | 2 => "x".to_string(), // the i64 range counter (biased so bodies stay mixed)
                _ => flit(rng),
            }
        }
        fn expr(rng: &mut u64, depth: u32) -> String {
            if depth == 0 || pick(rng, 2) == 0 {
                return atom(rng);
            }
            match pick(rng, 6) {
                0 => format!("(({}) + ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                1 => format!("(({}) - ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                2 => format!("(({}) * ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                3 => format!("sqrt(abs({}))", expr(rng, depth - 1)),
                4 => format!("min(({}), ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                _ => format!("max(({}), ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
            }
        }
        let s = (next(rng) % 8) as i64;
        let e = s + (next(rng) % 14) as i64; // x in [s, e) — small so the fold stays finite
        format!("range({}, {}).reduce({}, (acc, x) => ({}))", s, e, flit(rng), expr(rng, 3))
    }

    /// Triple oracle for the **range-source f64 reduce** kernel: JIT == bytecode VM ==
    /// tree-walker over a fuzzed range-fold. The typed reduce loop has landed, so `triple_oracle`
    /// confirms the native path actually engages (≈600 native calls / 3000 programs measured);
    /// if it ever regressed to a VM fall-back this stops being a green-but-vacuous VM==tw check
    /// and fails loudly instead.
    #[test]
    fn differential_range_float_reduce_oracle() {
        triple_oracle("range f64 reduce", 0x2A3D_F00D_5EED_2026, 15_000, gen_range_float_reduce);
    }

    /// **Division** in a range-source f64 reduce (`c + 1.0/g(k)` — the series-sum shape). Native
    /// `fdiv` yields inf/nan on a zero divisor where the interpreter RAISES, so the dividing kernel
    /// carries a **poison** out-param the codegen sets on ANY zero divisor (every division, every
    /// iteration); a set flag makes the VM fall back to the exact-erroring bytecode loop, and an
    /// unset flag guarantees no `/0` occurred so the fold is bit-exact. Because the flag is set at
    /// the division site — not inferred from the final result — it is sound even when a later op or
    /// iteration would "rescue" the inf (`min(inf, 5)`, `finite/inf`, or a body that overwrites the
    /// accumulator), so NO min/max or nested-division restriction is needed. This fuzzes
    /// division-heavy bodies — including natural `/0`, min/max, and nested/acc-ignoring shapes —
    /// and asserts JIT == bytecode VM == tree-walker across finite / erroring / fallback outcomes.
    /// (Engagement is proven by benchmark — basel 20M at 0.02s vs ~3.6s interpreted; this oracle
    /// proves CORRECTNESS. A regression seed here — `x - sqrt(abs(x))/x`, whose body ignores `acc`
    /// so a `/0` at x=0 was overwritten — is exactly what retired the earlier is-finite approach.)
    #[test]
    fn differential_range_float_reduce_division_oracle() {
        fn flit(rng: &mut u64) -> String {
            format!("{}.{:03}", (next(rng) % 11) as i64 - 5, next(rng) % 1000)
        }
        fn atom(rng: &mut u64) -> String {
            match pick(rng, 4) {
                0 => "acc".to_string(),
                1 | 2 => "x".to_string(), // the i64 counter (0 in some ranges → /0 coverage)
                _ => flit(rng),
            }
        }
        fn expr(rng: &mut u64, depth: u32) -> String {
            if depth == 0 || pick(rng, 2) == 0 {
                return atom(rng);
            }
            match pick(rng, 8) {
                0 => format!("(({}) + ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                1 => format!("(({}) - ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                2 => format!("(({}) * ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                3 => format!("sqrt(abs({}))", expr(rng, depth - 1)),
                4 => format!("min(({}), ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                5 => format!("max(({}), ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
                // division (dividend + divisor) — biased 2/8 so many bodies actually divide.
                _ => format!("(({}) / ({}))", expr(rng, depth - 1), expr(rng, depth - 1)),
            }
        }
        triple_oracle("div range f64 reduce", 0xD1F5_00D5_EED2_0271, 15_000, |rng| {
            let s = (next(rng) % 4) as i64; // 0..3 → sometimes starts at 0 (x = 0 → /0)
            let e = s + (next(rng) % 12) as i64;
            format!("range({}, {}).reduce({}, (acc, x) => ({}))", s, e, flit(rng), expr(rng, 3))
        });
        // Focused: series sums JIT to the right value; a `/0` errors on all engines; the excluded
        // rescue shapes (nested division / min-of-a-division) fall back but stay correct.
        let basel = "(range(0, 2000)).reduce(0.0, (c, k) => c + 1.0/((k+1)*(k+1)))";
        assert_eq!(run_vm_jit(basel), run_tw(basel), "basel JIT ≠ tree-walker");
        assert_eq!(run_vm_jit(basel), run_vm_no_jit(basel), "basel JIT ≠ bytecode VM");
        for div0 in [
            "(range(0, 5)).reduce(0.0, (c, k) => c + 1.0/k)", // divisor counter hits 0
            "(range(0, 5)).reduce(0.0, (c, k) => c + 1.0/(1.0/k))", // nested-division rescue shape
            "(range(0, 5)).reduce(0.0, (c, k) => c + min(1.0/k, 0.5))", // min rescue shape
            "range(0, 4).reduce(4.925, (acc, x) => (x - sqrt(abs(x))/x))", // acc-ignoring overwrite
        ] {
            assert!(run_vm_jit(div0).is_err(), "expected /0 error on `{div0}`");
            assert!(run_tw(div0).is_err(), "tree-walker must also error on `{div0}`");
        }
    }

    /// An **f64 tuple/record** accumulator fold — multi-statistic one-pass reductions
    /// (`(sum, sum_sq)`, `(min, max)`): every slot is `f64`, over either a `Float`-array
    /// element (pure f64) or the `i64` range counter (mixed per slot). Covers tuple AND record
    /// shapes, 2–3 slots, range AND array sources. Locks the f64-tuple codegen (both the fused
    /// array kernel and the range reduce loop, with bit-pattern slot marshalling) against the
    /// bytecode VM and tree-walker. Ineligible components (Int root / mixed `min`/`max`) fall
    /// back on every engine — still parity.
    fn gen_f64_tuple_reduce(rng: &mut u64) -> String {
        fn flit(rng: &mut u64) -> String {
            format!("{}.{:03}", (next(rng) % 21) as i64 - 10, next(rng) % 1000)
        }
        fn atom(rng: &mut u64, nslots: usize, is_record: bool) -> String {
            let c = pick(rng, (nslots + 2) as u64) as usize;
            if c < nslots {
                if is_record { format!("a.f{c}") } else { format!("a[{c}]") }
            } else if c == nslots {
                "x".to_string() // the element (Float array) or i64 range counter
            } else {
                flit(rng)
            }
        }
        fn expr(rng: &mut u64, depth: u32, nslots: usize, is_record: bool) -> String {
            if depth == 0 || pick(rng, 2) == 0 {
                return atom(rng, nslots, is_record);
            }
            let e = |r: &mut u64| expr(r, depth - 1, nslots, is_record);
            match pick(rng, 6) {
                0 => format!("(({}) + ({}))", e(rng), e(rng)),
                1 => format!("(({}) - ({}))", e(rng), e(rng)),
                2 => format!("(({}) * ({}))", e(rng), e(rng)),
                3 => format!("sqrt(abs({}))", e(rng)),
                4 => format!("min(({}), ({}))", e(rng), e(rng)),
                _ => format!("max(({}), ({}))", e(rng), e(rng)),
            }
        }
        let nslots = 2 + pick(rng, 2) as usize; // 2 or 3
        let is_record = pick(rng, 2) == 0;
        let is_range = pick(rng, 2) == 0;
        let comps: Vec<String> = (0..nslots).map(|_| expr(rng, 2, nslots, is_record)).collect();
        let inits: Vec<String> = (0..nslots).map(|_| flit(rng)).collect();
        let (init_s, body_s) = if is_record {
            let i: Vec<String> = (0..nslots).map(|k| format!("f{}: {}", k, inits[k])).collect();
            let b: Vec<String> = (0..nslots).map(|k| format!("f{}: {}", k, comps[k])).collect();
            (format!("{{{}}}", i.join(", ")), format!("{{{}}}", b.join(", ")))
        } else {
            (format!("({})", inits.join(", ")), format!("({})", comps.join(", ")))
        };
        let src = if is_range {
            let s = (next(rng) % 6) as i64;
            format!("range({}, {})", s, s + (next(rng) % 10) as i64)
        } else {
            let n = 1 + pick(rng, 6);
            let elems: Vec<String> = (0..n).map(|_| flit(rng)).collect();
            format!("[{}]", elems.join(", "))
        };
        format!("{src}.reduce({init_s}, (a, x) => {body_s})")
    }

    /// Triple oracle for the **f64 tuple/record reduce** kernel (range + array): JIT == bytecode
    /// VM == tree-walker. The kernel has landed (array literals are idempotent → fused), so
    /// `triple_oracle` confirms the native path engages (≈140 native calls / 3000 programs
    /// measured) rather than trusting it — a regression to VM fall-back fails the test instead
    /// of passing as a vacuous VM==tw check.
    #[test]
    fn differential_f64_tuple_reduce_oracle() {
        triple_oracle("f64 tuple reduce", 0x7AB1_E5F0_0DED_2026, 15_000, gen_f64_tuple_reduce);
    }

    /// A JIT-eligible `f64` reduce (range/array scalar, tuple, or record — the kernels from
    /// `8f656d8`/`f35e4ce`/`225a8b3`) reduced to a scalar and wrapped in a random scalar
    /// CONTEXT: `let`, `if`, arithmetic, `try`, `match`. The per-kernel oracles prove these
    /// fire and are bit-exact in ISOLATION; this proves the native multi-slot dispatch (with
    /// its bit-pattern slot marshalling and stack choreography) stays bit-exact in
    /// COMPOSITION — a kernel result feeding surrounding ops is where a stack-discipline bug
    /// in `TryJitReduce`/`TryJitFused` would surface.
    fn gen_f64_reduce_composed(rng: &mut u64) -> String {
        fn flit(rng: &mut u64) -> String {
            format!("{}.{:03}", (next(rng) % 13) as i64 - 6, next(rng) % 1000)
        }
        // An eligible f64 body over the accumulator atom(s) and the element `x`: `+ - *` and
        // `sqrt(abs(.))` only — within the JIT-eligible subset so the kernel actually fires
        // (no `min`/`max`, which would be mixed-kind and fall back over a range counter).
        fn body(rng: &mut u64, accs: &[&str]) -> String {
            fn atom(rng: &mut u64, accs: &[&str]) -> String {
                let c = pick(rng, (accs.len() + 2) as u64) as usize;
                if c < accs.len() {
                    accs[c].to_string()
                } else if c == accs.len() {
                    "x".to_string()
                } else {
                    flit(rng)
                }
            }
            fn ex(rng: &mut u64, d: u32, accs: &[&str]) -> String {
                if d == 0 || pick(rng, 2) == 0 {
                    return atom(rng, accs);
                }
                match pick(rng, 4) {
                    0 => format!("(({}) + ({}))", ex(rng, d - 1, accs), ex(rng, d - 1, accs)),
                    1 => format!("(({}) - ({}))", ex(rng, d - 1, accs), ex(rng, d - 1, accs)),
                    2 => format!("(({}) * ({}))", ex(rng, d - 1, accs), ex(rng, d - 1, accs)),
                    _ => format!("sqrt(abs({}))", ex(rng, d - 1, accs)),
                }
            }
            ex(rng, 2, accs)
        }
        // A small `Float` array (x is a `f64` element) or a small range (x is the i64 counter).
        fn src(rng: &mut u64) -> String {
            if pick(rng, 2) == 0 {
                let n = 1 + pick(rng, 5);
                let e: Vec<String> = (0..n).map(|_| flit(rng)).collect();
                format!("[{}]", e.join(", "))
            } else {
                let s = (next(rng) % 6) as i64;
                format!("range({}, {})", s, s + (next(rng) % 9) as i64)
            }
        }
        let inner = match pick(rng, 4) {
            1 => {
                let k = pick(rng, 2);
                format!(
                    "({}.reduce(({}, {}), (a, x) => (({}), ({}))))[{}]",
                    src(rng), flit(rng), flit(rng), body(rng, &["a[0]", "a[1]"]), body(rng, &["a[0]", "a[1]"]), k
                )
            }
            2 => format!(
                "({}.reduce({{p: {}, q: {}}}, (a, x) => {{p: ({}), q: ({})}})).{}",
                src(rng), flit(rng), flit(rng), body(rng, &["a.p", "a.q"]), body(rng, &["a.p", "a.q"]),
                if pick(rng, 2) == 0 { "p" } else { "q" }
            ),
            _ => format!("{}.reduce({}, (acc, x) => ({}))", src(rng), flit(rng), body(rng, &["acc"])),
        };
        match pick(rng, 5) {
            0 => format!("(let w = ({inner}) in ((w) + ({})))", flit(rng)),
            1 => format!("(if (({inner}) > ({})) then ({inner}) else ({}))", flit(rng), flit(rng)),
            2 => format!("(({inner}) * ({}))", flit(rng)),
            3 => format!("((try ({inner})).value ?? ({}))", flit(rng)),
            _ => format!("(match 0 {{ 0 => ({inner}), _ => ({}) }})", flit(rng)),
        }
    }

    /// Triple oracle for the f64 reduce kernels **in composition** (see `gen_f64_reduce_composed`).
    #[test]
    fn differential_f64_reduce_composition_oracle() {
        triple_oracle(
            "f64 reduce composition",
            0x5EED_F64C_0FFE_2026,
            15_000,
            gen_f64_reduce_composed,
        );
    }

    /// A curated battery of pathological / edge-case programs — the inputs a fuzzer rarely
    /// lands on but a user will: empty collections, out-of-bounds and negative indices,
    /// divide/mod by zero, `i64::MIN / -1`, NaN/inf, domain errors, malformed crypto input,
    /// type confusion through an `Unknown`-typed parameter, and a format-spec edge. Each MUST
    /// resolve to a value or a CLEAN error on all three engines — running them in-process is
    /// the no-panic guard (a Rust panic here fails the test) — and the engines MUST agree on
    /// the outcome. Locks the robustness an ad-hoc audit verified into a permanent test.
    #[test]
    fn robustness_edge_cases_never_panic_and_agree() {
        let cases = [
            "print([].max())",
            "print([].mean())",
            "print([].first())",
            "print([].reduce(0, (a, x) => a + x))",
            "print([1, 2, 3][999])",
            "print([1, 2, 3][-99])",
            "print(\"abc\"[99])",
            "print(([1, 2, 3])[5:2])",
            "print((1, 2, 3)[9])",
            "print(1 / 0)",
            "print(1 % 0)",
            "print(1 // 0)",
            "print(5 % 0)",
            "print(-9223372036854775808 // -1)",
            "print(-9223372036854775808 % -1)",
            "print(sqrt(-1.0))",
            "print(log(0.0))",
            "print(log(-5.0))",
            "print(0.0 / 0.0)",
            "print((1.0 / 0.0) - (1.0 / 0.0))",
            "print(2 ** 1000)",
            "print(9223372036854775807 + 1)",
            "print(chr(-1))",
            "print(chr(99999999))",
            "print(ord(\"\"))",
            "print(hex_decode(\"xyz\"))",
            "print(hex_decode(\"a\"))",
            "print(base64_decode(\"!!!!\"))",
            "print(aes_decrypt(\"short\", \"blob\"))",
            "print(ed25519_verify(\"bad\", \"m\", \"sig\"))",
            "fn f(x) = x[0]\nprint(f(5))",
            "fn f(x) = x.foo()\nprint(f(5))",
            "fn f(x) = x + 1\nprint(f(\"s\"))",
            "fn f(x) = x[0]\nprint(f(missing))",
            "x = 1.5\nprint(\"{x:.999f}\")",
            "print([1, 2, 3].take(-5))",
            "print(range(0, 99999999999999))",
            "print(([0.0 / 0.0, 1.0]).sort())",
        ];
        for src in cases {
            let jit = run_vm_jit(src);
            let no_jit = run_vm_no_jit(src);
            let tw = run_tw(src);
            // Reaching here at all proves no engine panicked. Now require outcome parity.
            assert_eq!(
                jit.is_ok(),
                no_jit.is_ok(),
                "engine outcome disagreement (JIT vs VM) on `{src}`: {jit:?} vs {no_jit:?}"
            );
            assert_eq!(
                no_jit.is_ok(),
                tw.is_ok(),
                "engine outcome disagreement (VM vs tree-walker) on `{src}`: {no_jit:?} vs {tw:?}"
            );
            if let (Ok(a), Ok(b), Ok(c)) = (&jit, &no_jit, &tw) {
                assert_eq!(a, b, "value disagreement (JIT vs VM) on `{src}`");
                assert_eq!(b, c, "value disagreement (VM vs tree-walker) on `{src}`");
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
        triple_oracle("match", 0x6A7C_4ECD_0FF1_CE42, 15_000, gen_match_pipeline);
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
        triple_oracle("int array kernels", 0xA11C_E0FF_EE00_1234, 15_000, gen_int_pipeline);
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
                // Both reject: a STATIC-checker rejection (tagged `typecheck:`) belongs
                // to a different failure taxonomy — the checker reports the first
                // static error, the tree-walker the first dynamically-REACHED one, so
                // their messages legitimately differ (found by the message-parity
                // upgrade: `… | 186.0` rejected statically while the tw hit an OOB in
                // an earlier-evaluated condition). RUNTIME errors must match exactly.
                (Err(ea), Err(eb)) => {
                    if !ea.starts_with("typecheck: ") {
                        assert_eq!(ea, eb, "error-message divergence on `{src}`");
                    }
                }
                // checker stricter than the dynamic tree-walker → allowed
                (Err(_), Ok(_)) => {}
                // typed VM ran but tree-walker rejected: only legitimate via the B2
                // recursion-depth difference; anything else is a real divergence.
                (Ok(_), Err(_)) if tw_hit_recursion_limit(&src) => {}
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
                (Err(ea), Err(eb)) => assert_eq!(ea, eb, "error-message divergence on `{src}`"),
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

    /// Recursion behavior is ALIGNED across engines (#81, 2026-07): a tail call
    /// to a top-level `fn` reuses the frame on every engine (the walker's
    /// `call_function` trampoline, the VM's `TailCallFn`, the JIT's native
    /// loops), so deep TAIL recursion succeeds everywhere at constant depth;
    /// NON-tail recursion counts against the one shared `MAX_CALL_DEPTH` and
    /// errors with the identical message everywhere. The old (20k, 1M]
    /// walker/VM gap — one engine printing where the other errored — is gone.
    #[test]
    fn recursion_depth_is_aligned_across_engines() {
        // TAIL: 50k-deep succeeds on both engines (frame reuse, constant depth).
        let deep = "fn deep(n) = if n <= 0 then 0 else deep(n - 1)\n";
        let plain = format!("{deep}deep(50000)");
        assert_eq!(run_vm(&plain), Ok("0".to_string()));
        assert_eq!(run_tw(&plain), Ok("0".to_string()));

        // NON-tail: both exhaust the shared depth with the identical error,
        // and `try` observes it identically.
        let nontail = "fn s(n) = if n == 0 then 0 else n + s(n - 1)\n";
        let plain_nt = format!("{nontail}s(50000)");
        let tw = run_tw(&plain_nt).unwrap_err();
        let vm = run_vm(&plain_nt).unwrap_err();
        assert_eq!(tw, vm, "depth-exhaustion error text must match");
        assert!(tw.contains("maximum recursion depth (20000) exceeded"), "got: {tw}");
        let caught = format!("{nontail}r = try s(50000)\nr.ok");
        assert_eq!(run_vm(&caught), Ok("false".to_string()));
        assert_eq!(run_tw(&caught), Ok("false".to_string()));

        // In-budget non-tail recursion still returns identical values.
        let ok_nt = format!("{nontail}s(1000)");
        assert_eq!(run_vm(&ok_nt), Ok("500500".to_string()));
        assert_eq!(run_tw(&ok_nt), Ok("500500".to_string()));

        // The walker's trampoline also frame-reuses tails through `let` and
        // `match` bodies, and a tail call to a DIFFERENT top-level fn.
        for src in [
            "fn g2(n, a) = let m = n - 1 in if n == 0 then a else g2(m, a + 1)\ng2(100000, 0)",
            "fn g3(n) = match n { 0 => 7, _ => g3(n - 1) }\ng3(100000)",
            "fn base(n) = n + 1\nfn g4(n) = if n == 0 then base(0) else g4(n - 1)\ng4(100000)",
        ] {
            let vm = run_vm(src);
            assert_eq!(vm, run_tw(src), "engines disagree on `{src}`");
            assert!(vm.is_ok(), "`{src}` should succeed: {vm:?}");
        }
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
            &prog.scan_loops,
        );
        let err = exec(&prog, jit.as_ref()).unwrap_err();
        assert!(err.message.contains("division by zero"), "got: {}", err.message);
    }

    /// Runtime errors must still surface (and match the tree-walker's wording). The depth
    /// guard catches runaway *non-tail* recursion; the `+ 1` keeps the self-call non-tail
    /// (its result is consumed by the add), so it still pushes frames to the limit. A *tail*
    /// runaway like `boom(n) = boom(n + 1)` is now constant-space under TCO — an intentional
    /// infinite loop (`while true`), the shape a server's accept loop relies on.
    #[test]
    fn errors_propagate() {
        let toks = lexer::lex("fn boom(n) = 1 + boom(n + 1)\nboom(0)").unwrap();
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
            &prog.scan_loops,
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

    /// Sweep follow-ups (#80): the VM's error TEXT must match the walker's for
    /// where-vs-filter naming, match-guard wording, and `join` arity through an
    /// Unknown receiver (the DfJoin value fallback used to silently drop the
    /// extra argument and print a value the walker rejects).
    #[test]
    fn vm_error_text_matches_walker_on_sweep_followups() {
        for src in [
            // `where` quoted as written (was: "type Int has no method `filter`").
            "fn g(x) = x.where(it > 1)\ng(5)",
            // A non-bool `where` predicate names `where` (was: "`filter` expects…").
            "[1, 2].where(it * 2)",
            // Guard wording: `missing` and non-boolean guards get the shared
            // `match`-guard message, not the `if`-condition or generic one.
            "match 1 { x if missing => 1, _ => 2 }",
            "match 1 { x if 1 => 1, _ => 2 }",
            // `join` arity through an Unknown receiver.
            "fn j(x) = x.join(\"-\", \"z\")\nj([\"a\", \"b\"])",
        ] {
            let tw = run_tw(src).unwrap_err();
            let vm = run_vm(src).unwrap_err();
            assert_eq!(tw, vm, "engines disagree on `{src}`");
        }
        assert!(run_vm("fn j(x) = x.join(\"-\", \"z\")\nj([\"a\", \"b\"])")
            .unwrap_err()
            .contains("takes 1 argument, got 2"));
        // Guards still pass/fail correctly after the op change.
        let ok = "match 2 { x if x > 1 => 10, _ => 20 }";
        assert_eq!(run_vm(ok).unwrap(), "10");
        assert_eq!(run_tw(ok).unwrap(), "10");
        let fall = "match 0 { x if x > 1 => 10, _ => 20 }";
        assert_eq!(run_vm(fall).unwrap(), "20");
        assert_eq!(run_tw(fall).unwrap(), "20");
    }

    /// #81 round 2 (adversarial review): lexical scoping, boundary parity, and
    /// fn-name rebinding. The walker's call boundary now swaps its locals map
    /// wholesale, so a callee resolves free names against ITS OWN frame and
    /// then globals — never the caller's locals (the flat shared env used to
    /// give callees DYNAMIC scoping, a verified walker/VM divergence).
    #[test]
    fn walker_scoping_matches_vm_lexically() {
        // A callee reading a global is immune to the caller's shadowing
        // param/let (walker used to print 42/99 here where the VM prints 10).
        for src in [
            "x = 10\nfn callee() = x\nfn caller(x) = callee() + 0\ncaller(42)",
            "x = 10\nfn callee() = x\nfn caller(n) = let x = 99 in callee() + n\ncaller(0)",
        ] {
            assert_eq!(run_vm(src), Ok("10".to_string()), "vm on `{src}`");
            assert_eq!(run_tw(src), Ok("10".to_string()), "tw on `{src}`");
        }
        // A failing `let` initializer restores already-installed bindings — in
        // value position too (the leak survived `try` and clobbered a global).
        let leak = "marker = 1\nidx = 5\n\
fn valpos() = (let marker = 99, boom = [1, 2][idx] in 0) + 0\n\
b = try valpos()\nmarker";
        assert_eq!(run_vm(leak), Ok("1".to_string()));
        assert_eq!(run_tw(leak), Ok("1".to_string()));
        // The depth budget trips at the SAME activation on both engines (the
        // VM's frames vec includes `<main>`, which must not eat a user frame).
        let down = "fn down(n) = if n == 0 then 0 else 1 + down(n - 1)\n";
        for (n, ok) in [(19999, true), (20000, false)] {
            let src = format!("{down}down({n})");
            let vm = run_vm(&src);
            assert_eq!(vm, run_tw(&src), "boundary disagreement at {n}");
            assert_eq!(vm.is_ok(), ok, "unexpected outcome at {n}: {vm:?}");
        }
        // TCO fires even when the CALLER shadows the callee's name with a
        // param (two-map env: the callee's self-call resolves the global fn,
        // not the caller's local — the walker used to depth-error here).
        let dyn_shadow = "fn g(n) = if n == 0 then 0 else g(n - 1)\n\
fn caller(g2, n) = g2(n)\ncaller(g, 30000)";
        assert_eq!(run_vm(dyn_shadow), Ok("0".to_string()));
        assert_eq!(run_tw(dyn_shadow), Ok("0".to_string()));
        // An immutable-global ALIAS of a fn is frame-reused on NEITHER engine
        // (the VM's `resolve` prefers globals → CallValue, never peepholed;
        // the walker's gate keys on the DECLARED name). One frame each — this
        // sits exactly at the budget and must agree.
        let alias = "fn id(x) = x\nh = id\n\
fn probe(d) = if d == 0 then h(42) else 0 + probe(d - 1)\nprobe(19998)";
        let vm = run_vm(alias);
        assert_eq!(vm, run_tw(alias), "alias-call disagreement");
        assert_eq!(vm, Ok("42".to_string()));
        // Rebinding a `fn`-declared name is an error on both engines, `mut` or
        // plain: the VM binds CallFn targets at compile time, so a late
        // rebinding could never be honored there.
        for src in ["fn f(x) = x\nf = 5\n1", "fn f(x) = x\nmut f = 5\n1"] {
            let tw = run_tw(src).unwrap_err();
            assert_eq!(tw, run_vm(src).unwrap_err(), "rebind error mismatch on `{src}`");
            assert!(tw.contains("immutable and cannot be reassigned"), "got: {tw}");
        }
    }

    /// #82: ADR-0001 equality semantics. `==`/`!=` are THREE-VALUED at any
    /// depth (a compared `missing` makes the answer `missing`, unless a
    /// definite structural difference decides first — Kleene); set-like
    /// operations (`unique`, `frequencies`, `contains`, `index_of`) use the
    /// total IDENTITY equality where `missing` matches `missing`. Tuples
    /// order lexicographically. All shared code (`ops::eq3`/`values_equal`/
    /// `compare`), so tri-engine parity is by construction — pinned anyway.
    #[test]
    fn three_valued_equality_and_tuple_ordering() {
        for (src, want) in [
            // three-valued structural equality
            ("{a: missing} == {a: missing}", "missing"),
            ("{a: 1, b: missing} == {a: 2, b: missing}", "false"), // definite diff wins
            ("{a: 1, b: missing} != {a: 2, b: missing}", "true"),
            ("(try 5) == (try 5)", "missing"), // ok-records carry error: missing
            ("missing == missing", "missing"),
            ("[1, missing] == [1, 2]", "missing"),
            ("[1, missing] == [2, missing]", "false"),
            // identity equality for set-like operations
            ("[missing, missing].unique()", "[missing]"),
            ("[missing, 1, missing].frequencies()", "[(missing, 2), (1, 1)]"),
            ("[1, missing].contains(missing)", "true"),
            ("[1, missing].index_of(missing)", "1"),
            // unchanged: numeric coercion and IEEE NaN
            ("1 == 1.0", "true"),
            ("[1.0, sqrt(-1.0)] == [1.0, sqrt(-1.0)]", "false"),
            // lexicographic tuple ordering
            ("(1, 2) < (1, 3)", "true"),
            ("(1, 2) < (1, 2, 3)", "true"), // equal prefix -> length decides
            ("(1, 2) <= (1, 2)", "true"),
            ("(2, 0) <= (1, 9)", "false"),
            ("(5, 1) > (4, 99)", "true"),
            ("(1, missing) < (1, 2)", "missing"),
            // an unorderable PAIR errors like the scalars would
            ("(try ((1, \"a\") < (1, 2))).ok", "false"),
        ] {
            let vm = run_vm(src);
            assert_eq!(vm, run_tw(src), "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // Duplicate record fields reject at parse time — literals and patterns.
        for src in ["{a: 1, a: 2}", "match {a: 1} { {a: x, a: y} => x, _ => 0 }"] {
            let tw = run_tw(src).unwrap_err();
            assert_eq!(tw, run_vm(src).unwrap_err(), "on `{src}`");
            assert!(tw.contains("duplicate field"), "got: {tw}");
        }
    }

    /// #83: continuous protection for the {memoized fn × mutable-global read ×
    /// mutation-between-calls} class — the 2026-07 stale-memo bug hid exactly
    /// here, invisible to gen_expr (which emits no `mut` statements and no
    /// fn-reads-global shapes). Four read-indirections (bare, match-arm,
    /// `??`-coalesce, `try`), a fib-shaped memoization candidate, a rebind, and
    /// a re-call: the VM must recompute, never serve the stale cache. Walker
    /// (never memoizes across the rebind) is the reference.
    #[test]
    fn differential_memo_mut_globals() {
        let mut rng = 0x00C0_FFEE_D00D_5EEDu64;
        for i in 0..2000u32 {
            let g0 = (next(&mut rng) % 1000) as i64;
            let g1 = (next(&mut rng) % 1000) as i64 + 1000;
            let k = 3 + (next(&mut rng) % 10); // fib depth 3..=12 — walker-stack-safe
            let read = match pick(&mut rng, 4) {
                0 => "fn rd() = g",
                1 => "fn rd() = match 0 { 0 => g, _ => 0 }",
                2 => "fn rd() = (missing ?? g)",
                _ => "fn rd() = (try g).value",
            };
            let src = format!(
                "mut g = {g0}\n{read}\nfn f(n) = if n < 2 then rd() else f(n - 1) + f(n - 2)\n\
a = f({k})\ng = {g1}\n(a * 1000000) + f({k})"
            );
            let vm = run_vm(&src);
            let tw = run_tw(&src);
            assert_eq!(vm, tw, "memo x mut-global divergence (iter {i}) on:\n{src}");
        }
    }

    /// A comprehension's function must bind the element: `xs.map(() => 5)`
    /// ignores every element, so BOTH engines reject it BEFORE iterating, with
    /// the identical message. The walker used to have no such check — it only
    /// noticed when the destructure failed, so this SUCCEEDED on an empty `xs`
    /// (the lambda is never invoked → `[]`) and failed with a different message
    /// once `xs` had data. A bug that ships green and detonates on real input,
    /// and a value-vs-error divergence from the VM. The empty-vs-non-empty pair
    /// is the point of this test: the rejection must not depend on the data.
    #[test]
    fn zero_param_comprehension_lambda_rejects_on_both_engines() {
        for (src, want) in [
            ("[].map(() => 5)", "`map`'s function needs at least one parameter"),
            ("[1, 2].map(() => 5)", "`map`'s function needs at least one parameter"),
            ("[].filter(() => true)", "`filter`'s function needs at least one parameter"),
            ("[1, 2].filter(() => true)", "`filter`'s function needs at least one parameter"),
            ("[1, 2].where(() => true)", "`where`'s function needs at least one parameter"),
            ("[].any(() => true)", "`any`'s function needs at least one parameter"),
            ("[1, 2].all(() => true)", "`all`'s function needs at least one parameter"),
            // reduce already agreed — it has its own exact-two-params check.
            ("[1, 2].reduce(0, () => 9)", "`reduce`'s function needs exactly two parameters, but got 0"),
        ] {
            let tw = run_tw(src).unwrap_err();
            let vm = run_vm(src).unwrap_err();
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(tw, want, "`{src}`");
        }
        // The one-parameter forms still work, on both engines.
        for (src, want) in [
            ("[1, 2].map(it * 2)", "[2, 4]"),
            ("[1, 2, 3].filter(it > 1)", "[2, 3]"),
            ("[1, 2].any(it > 1)", "true"),
            ("[].map(it * 2)", "[]"),
        ] {
            assert_eq!(run_vm(src), run_tw(src), "engines disagree on `{src}`");
            assert_eq!(run_vm(src), Ok(want.to_string()), "`{src}`");
        }
    }

    /// The poison FFI wrappers run PARALLEL, with a poison cell per chunk reduced by `|`.
    /// Every raising kernel — the rounders, dividing bodies, and mixed callees — had been
    /// giving up the chunked-parallel path: a float-parameter callee measured 0.06s where its
    /// non-raising twin measured 0.02s. Now 0.03s.
    ///
    /// The risk the reduce introduces is a poison raised in ONE chunk being lost. Every case
    /// here is sized past `PAR_MATH_THRESHOLD` and raises in a chosen chunk — early, late, or
    /// at exactly one index.
    ///
    /// A LESSON FROM THE SABOTAGE, recorded so the cases are not "simplified" back: probes
    /// written as `floor(if i == K then 1e19 else …)` prove NOTHING here. `if` is not in the
    /// mixed analysis, so a conditional body declines to the bytecode loop and raises
    /// correctly however broken the reduce is — three such probes passed happily against a
    /// reduce hard-wired to return 0. Every case below is straight-line arithmetic that
    /// raises only on a chosen index range, and all of them fail under that sabotage.
    #[test]
    fn the_parallel_poison_reduce_never_loses_a_chunks_bail() {
        // `floor(x * 1e14)` leaves i64 range at x >= 92234, so the plain counter raises in
        // the LATE chunks and the reversed one in the EARLY chunks. A division raises at
        // exactly one index — the sharpest single-chunk case.
        for src in [
            "(0..200000).map(i => floor(to_float(i) * 100000000000000.0)).count()",
            "(0..200000).map(i => floor(to_float(200000 - i) * 100000000000000.0)).count()",
            "(0..200000).map(i => 1.0 / to_float(100000 - i)).count()",
            "(0..200000).map(i => 1.0 / to_float(100 - i)).count()",
            "(0..200000).map(i => 1.0 / to_float(199999 - i)).count()",
            "fn g(x: Float, d: Float) = x / d\n(0..200000).map(i => g(1.0, to_float(100000 - i))).count()",
            "fn g(x: Float) = if x > 1.0 then 1.0 else 0.0\n(0..200000).map(i => g(sqrt(to_float(100000 - i)))).count()",
            // A materialized (non-range) source takes the other wrapper.
            "src = (0..200000).map(i => i * 2)\nsrc.map(i => floor(to_float(i) * 100000000000000.0)).count()",
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert!(tw.is_err(), "`{src}` should raise");
            assert_eq!(tw, vm, "tree-walker and VM disagree on the error for `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on the error for `{src}`");
        }
        // Clean runs at the same size must NOT poison, and must be exact at the chunk
        // boundaries the parallel split introduces (16384, and 32768 = the threshold itself).
        for (src, want) in [
            ("a = (0..200000).map(i => floor(to_float(i) * 1.5))\n[a[16383], a[16384], a[32767], a[32768], a[199999]]",
             "[24574, 24576, 49150, 49152, 299998]"),
            ("d = 4.0\na = (0..200000).map(i => to_float(i) / d)\n[a[199999]]", "[49999.75]"),
            ("fn g(x: Float) = x * 2.0 + 1.0\na = (0..200000).map(i => g(to_float(i)))\n[a[0], a[199999]]",
             "[1.0, 399999.0]"),
            // Straddling the threshold in both directions.
            ("a = (0..32767).map(i => floor(to_float(i) * 1.5))\n[a[32766], a.count()]", "[49149, 32767]"),
            ("a = (0..32768).map(i => floor(to_float(i) * 1.5))\n[a[32767], a.count()]", "[49150, 32768]"),
            ("src = (0..200000).map(i => i * 2)\n[src.map(i => floor(to_float(i) * 0.5))[199999]]", "[199999]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
    }

    /// A map body may CALL a `Float`-parameter user function — the last shape where naming
    /// something cost two orders of magnitude (2.09s against 0.02s inline at 20M, annotated
    /// or inferred). The kernel marshals to the mixed bits ABI, hands the callee a stack
    /// cell as its poison out-param, and folds that cell into its own accumulator.
    ///
    /// THE CRUX IS THAT A CALLEE'S BAIL MUST POISON THE WHOLE MAP. It very nearly did not:
    /// `map_body_raises` scanned only for rounders and division *in the body*, so a body that
    /// merely CALLS a mixed function was built poison-free, the VM took the non-poison
    /// wrapper, and a raising callee was silently swallowed — `[0.0, 0.0, …]` where the other
    /// two engines raised "division by zero". Every raise case below failed that way before
    /// the fix, and they are the reason this test exists.
    #[test]
    fn a_map_body_may_call_a_float_parameter_function_and_its_bail_poisons_the_map() {
        for (src, want) in [
            ("fn g(x: Float) = x * 2.0 + 1.0\n(0..6).map(i => g(to_float(i)))", "[1.0, 3.0, 5.0, 7.0, 9.0, 11.0]"),
            // Unannotated: the kinds are inferred (Stage 3j), and must reach the same table.
            ("fn g(x) = x * 2.0 + 1.0\n(0..6).map(i => g(to_float(i)))", "[1.0, 3.0, 5.0, 7.0, 9.0, 11.0]"),
            ("fn g(a: Float, b: Int) = a * to_float(b) + 1.0\n(0..6).map(i => g(to_float(i), 3))", "[1.0, 4.0, 7.0, 10.0, 13.0, 16.0]"),
            // A Float-argument function returning `Int` — the exact reason there is no plain
            // f64 specialization, so the mixed one is the only thing callable.
            ("fn g(x: Float) = if x > 2.0 then 1 else 0\n(0..6).map(i => g(to_float(i)))", "[0, 0, 0, 1, 1, 1]"),
            ("fn inner(x: Float) = x * 2.0\nfn outer(x: Float) = inner(x) + 1.0\n(0..6).map(i => outer(to_float(i)))", "[1.0, 3.0, 5.0, 7.0, 9.0, 11.0]"),
            ("c = 2.5\nfn g(x: Float) = x * 2.0\n(0..6).map(i => g(to_float(i)) + c)", "[2.5, 4.5, 6.5, 8.5, 10.5, 12.5]"),
            ("fn g(x: Float) = x * 1.5\n(0..6).map(i => floor(g(to_float(i))))", "[0, 1, 3, 4, 6, 7]"),
            ("fn g(x: Float, d: Float) = x / d\n(0..6).map(i => g(to_float(i), 4.0))", "[0.0, 0.25, 0.5, 0.75, 1.0, 1.25]"),
            ("fn g(x: Float, n: Int) = if n <= 0 then x else g(x * 1.5, n - 1)\n(0..5).map(i => g(to_float(i), 2))", "[0.0, 2.25, 4.5, 6.75, 9.0]"),
            // Argument kinds must EQUAL the callee's parameter kinds — no promoting at the
            // boundary, so an `Int` argument to a `Float` parameter declines to the VM.
            ("fn g(x: Float) = x * 2.0\n(0..4).map(i => g(i) + 0.0)", "[0.0, 2.0, 4.0, 6.0]"),
            // Neighbours: an i64 callee, and a user function shadowing a builtin name.
            ("fn f(x) = x * 2 + 1\n(0..5).map(i => f(i) * 0.5)", "[0.5, 1.5, 2.5, 3.5, 4.5]"),
            ("fn sqrt(x: Float) = x + 100.0\n(0..4).map(i => sqrt(to_float(i)))", "[100.0, 101.0, 102.0, 103.0]"),
            ("fn g(x: Float) = x * 2.0\n(0..0).map(i => g(to_float(i)))", "[]"),
            ("fn g(x: Float) = x * 2.0\n[1, 2, 3].map(i => g(to_float(i)))", "[2.0, 4.0, 6.0]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // The callee raises: `/0` at every element and at one element, a NaN comparison, and
        // a rounder out of range. Each must surface the interpreter's exact error.
        for src in [
            "fn g(x: Float, d: Float) = x / d\n(0..6).map(i => g(to_float(i), 0.0))",
            "fn g(x: Float, d: Float) = x / d\n(0..6).map(i => g(1.0, to_float(3 - i)))",
            "fn g(x: Float) = if x > 1.0 then 1.0 else 0.0\n(0..4).map(i => g(sqrt(-to_float(i + 1))))",
            "fn g(x: Float) = to_float(floor(x))\n(0..4).map(i => g(to_float(i) * 1e19))",
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert!(tw.is_err(), "`{src}` should raise");
            assert_eq!(tw, vm, "tree-walker and VM disagree on the error for `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on the error for `{src}`");
        }
        crate::jit::reset_native_call_count();
        assert!(run_vm_jit("fn g(x: Float) = x * 2.0 + 1.0\n(0..64).map(i => g(to_float(i))).sum()").is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "the float-parameter callee never reached a native kernel"
        );
    }

    /// The pure mixed-signature table must name exactly the functions the JIT gives a mixed
    /// specialization, with the same kinds. It is the compile-time half of a two-sided gate:
    /// the bytecode compiler uses it to decide whether a call inside a map body can be typed,
    /// and `build` re-derives the same thing. A disagreement would mean emitting guards for
    /// kernels the JIT never compiles — or typing a call by a signature the callee lacks.
    #[test]
    fn the_pure_mixed_sig_table_matches_what_the_jit_specializes() {
        use crate::jit::NumKind;
        let src = "fn plain(x: Float) = x * 2.0 + 1.0\n\
                   fn twoarg(a: Float, b: Int) = a * to_float(b)\n\
                   fn inty(x) = x * 2 + 1\n\
                   fn spin(zr, zi, i, n) = if i >= n then i else spin(zr * zr - zi * zi, 2.0 * zr * zi, i + 1, n)\n\
                   fn wrapper(px: Int) = plain(to_float(px))\n\
                   fn usesdiv(x: Float, d: Float) = x / d\n\
                   fn nontail(x: Float) = if x <= 0.0 then 0.0 else x + nontail(x - 1.0)\n\
                   plain(1.0)\n";
        let toks = lexer::lex(src).expect("lex");
        let ast = parser::parse(toks).expect("parse");
        let table = crate::jit::mixed_fn_sigs(&ast);

        assert!(table.contains_key("plain"), "a Float-parameter function must get a mixed sig");
        assert!(table.contains_key("twoarg"));
        assert!(table.contains_key("wrapper"), "a mixed body calling an earlier one qualifies");
        assert!(table.contains_key("usesdiv"), "non-literal divisors are admitted since 3w");
        // Unannotated numeric recursion must still be inferred — Stage 3j's annotation cliff.
        assert!(table.contains_key("spin"), "unannotated numeric recursion must be inferred");
        // An all-`Int` function is the plain i64 specialization's job; `mixed_fn_sig` declines
        // a zero float-mask so the two never compete.
        assert!(!table.contains_key("inty"), "all-Int belongs to the i64 spec, not mixed");
        assert!(!table.contains_key("nontail"), "non-tail recursion is excluded");

        assert_eq!(table["plain"].0, vec![NumKind::Float]);
        assert_eq!(table["plain"].1, NumKind::Float);
        assert_eq!(table["twoarg"].0, vec![NumKind::Float, NumKind::Int]);
        assert_eq!(table["twoarg"].1, NumKind::Float);
    }

    /// The VALUE-SCALAR mixed map: captures ride as `f64` bits instead of Int-proven `i64`s,
    /// so a `Float` variable in a mixed body compiles instead of declining. `d = 4.0;
    /// map(i => to_float(i) / d)` went 3.48s → 0.05s at 20M, matching the `d = 4` and
    /// literal spellings exactly.
    ///
    /// THE GUARD IS `mix_combine`, and constructing a case that exercises it took two
    /// refinements — recorded here because the obvious probes prove nothing:
    ///   * `c = 2^53+1; map(i => to_float(c * i))` does NOT discriminate: `c` is an `Int` at
    ///     runtime, so the Int-proven marshal wins and this never reaches the value-scalar
    ///     path at all.
    ///   * Multiplier 2 does NOT discriminate either: `(2^53+1) * 2` rounds to the same f64
    ///     from both directions. Multiplier 3 is the smallest that separates them.
    /// The case below has BOTH: a `Float` capture to force the value-scalar path, and a large
    /// `Int` capture used in an integer product. Forcing `(SFloat, Int)` to combine yields
    /// `27021597764222980.0` on the JIT against `27021597764222984.0` on the other two.
    #[test]
    fn a_float_capture_compiles_as_a_value_scalar_and_unpromoted_scalars_decline() {
        for (src, want) in [
            ("d = 4.0\n(0..6).map(i => to_float(i) / d)", "[0.0, 0.25, 0.5, 0.75, 1.0, 1.25]"),
            ("c = 2.5\n(0..6).map(i => to_float(i) * c)", "[0.0, 2.5, 5.0, 7.5, 10.0, 12.5]"),
            // An INT variable through the same body — the Int-proven marshal still wins.
            ("d = 4\n(0..6).map(i => to_float(i) / d)", "[0.0, 0.25, 0.5, 0.75, 1.0, 1.25]"),
            ("a = 2.5\nb = 1.5\n(0..6).map(i => to_float(i) * a + b)", "[1.5, 4.0, 6.5, 9.0, 11.5, 14.0]"),
            ("d = 4.0\n(0..8).map(i => ceil(to_float(i) / d))", "[0, 1, 1, 1, 1, 2, 2, 2]"),
            ("fn dbl(x) = x * 2\nd = 4.0\n(0..6).map(i => to_float(dbl(i)) / d)", "[0.0, 0.5, 1.0, 1.5, 2.0, 2.5]"),
            // THE DISCRIMINATING CASE (see the doc comment). A Float capture forces the
            // value-scalar path; the Int capture must stay `i64` inside `c * i`.
            (
                "c = 9007199254740993\nd = 2.5\n(0..5).map(i => to_float(c * i) + d)",
                "[2.5, 9007199254740994.0, 18014398509481988.0, 27021597764222984.0, 36028797018963968.0]",
            ),
            // Degenerate and non-range sources.
            ("d = 4.0\n(0..0).map(i => to_float(i) / d)", "[]"),
            ("d = 4.0\n(4..0).map(i => to_float(i) / d)", "[]"),
            ("d = 4.0\n[1, 2, 3].map(i => to_float(i) / d)", "[0.25, 0.5, 0.75]"),
            // Neighbours this must not disturb.
            ("c = 7\n(0..5).map(j => ((c * j) % 100) * 0.5)", "[0.0, 3.5, 7.0, 10.5, 14.0]"),
            ("a = [1.5, 2.5, 3.5]\nc = 2.0\n(0..3).map(i => c * a[i])", "[3.0, 5.0, 7.0]"),
            ("(0..5).map(i => to_int(to_float(i) * 1.5))", "[0, 1, 3, 4, 6]"),
            ("(0..5).map(i => to_float(i) * 0.5)", "[0.0, 0.5, 1.0, 1.5, 2.0]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // A Float capture with a zero divisor still raises exactly (poison through the
        // value-scalar kernel).
        let src = "d = 0.0\n(0..4).map(i => to_float(i) / d)";
        let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
        assert!(tw.is_err(), "`{src}` should raise");
        assert_eq!(tw, vm);
        assert_eq!(vm, jit);

        crate::jit::reset_native_call_count();
        assert!(run_vm_jit("d = 4.0\n(0..64).map(i => to_float(i) / d).sum()").is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "the value-scalar mixed kernel never reached native code"
        );
    }

    /// Non-literal float divisors compile — in mixed FUNCTION bodies behind an IMMEDIATE
    /// poison bail, and in mixed MAP bodies behind the accumulated poison. This single
    /// literal-only restriction was k2's entire 5.3×: `row`'s `2.7 / to_float(g)` declined
    /// the whole function to the VM, costing ~250 ns of dispatch per pixel around a native
    /// `step`. k2 is now 0.08s — tied with C.
    ///
    /// The bail must be IMMEDIATE in a function body (not accumulate-and-store): a tail loop
    /// can be infinite, and a `/0` inside one must error like the interpreter, not spin
    /// natively — pinned below by a loop capped at a billion iterations. Sabotage-proven:
    /// removing the bail makes `f(1.5, 0.0)` return `inf` on the JIT where both other
    /// engines raise "division by zero".
    #[test]
    fn non_literal_float_divisors_compile_and_zero_divisors_raise_exactly() {
        for (src, want) in [
            // The k2 shape: a tail loop dividing by a parameter.
            (
                "fn f(x: Int, n: Int, acc: Float, d: Float) =\n  if x >= n then acc\n  else f(x + 1, n, acc + to_float(x) / d, d)\nf(0, 10, 0.0, 4.0)",
                "11.25",
            ),
            // `/` always yields Float, even Int / Int.
            ("fn f(a, b) = a / b\nf(10, 4)", "2.5"),
            // A callee that divides, called from a loop — poison must propagate.
            (
                "fn sq(x: Float) = x * x\nfn f(i: Int, n: Int, acc: Float, d: Float) =\n  if i >= n then acc\n  else f(i + 1, n, acc + sq(to_float(i)) / d, d)\nf(0, 5, 0.0, 2.0)",
                "15.0",
            ),
            // Division inside a CONDITION.
            ("fn f(x: Float, d: Float) = if x / d > 1.0 then 1 else 0\nf(3.0, 2.0)", "1"),
            // Map bodies: the `ceil(x / d)` spelling that previously forced `* 0.25`.
            ("d = 4.0\n(0..8).map(i => ceil(to_float(i) / d))", "[0, 1, 1, 1, 1, 2, 2, 2]"),
            ("d = 4.0\n(0..6).map(i => to_float(i) / d)", "[0.0, 0.25, 0.5, 0.75, 1.0, 1.25]"),
            ("(0..6).map(i => i / 2 + 0.0)", "[0.0, 0.5, 1.0, 1.5, 2.0, 2.5]"),
            ("(0..6).map(i => to_float(i) / 4.0)", "[0.0, 0.25, 0.5, 0.75, 1.0, 1.25]"),
            ("fn dbl(x) = x * 2\nd = 4.0\n(0..6).map(i => to_float(dbl(i)) / d)", "[0.0, 0.5, 1.0, 1.5, 2.0, 2.5]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // The raises: exact error text everywhere. The billion-iteration loop is the
        // immediate-bail case — accumulate-and-store would spin natively for minutes.
        for src in [
            "fn f(x: Float, d: Float) = x / d\nf(1.5, 0.0)",
            "fn f(x: Float, d: Float) = x / d\nf(1.5, -0.0)",
            "fn f(a, b) = a / b\nf(10, 0)",
            "fn f(a: Float, n: Int) =\n  if n >= 1000000000 then a\n  else f(a / to_float(0), n + 1)\nf(1.0, 0)",
            "fn f(x: Int, n: Int, acc: Float) =\n  if x >= n then acc\n  else f(x + 1, n, acc + 1.0 / to_float(3 - x))\nf(0, 6, 0.0)",
            "fn inner(x: Float, d: Float) = x / d\nfn outer(i: Int, n: Int, acc: Float, d: Float) =\n  if i >= n then acc\n  else outer(i + 1, n, acc + inner(to_float(i), d), d)\nouter(0, 5, 0.0, 0.0)",
            "fn f(x: Float, d: Float) = if x / d > 1.0 then 1 else 0\nf(3.0, 0.0)",
            "d = 0.0\n(0..6).map(i => to_float(i) / d)",
            "(0..6).map(i => 6.0 / to_float(3 - i))",
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert!(tw.is_err(), "`{src}` should raise");
            assert_eq!(tw, vm, "tree-walker and VM disagree on the error for `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on the error for `{src}`");
        }
        // Engagement: the dividing map kernel must actually run natively. The divisor is an
        // INT variable deliberately — the plain mixed analysis carries captures as
        // Int-proven scalars (Stage 3m's contract), so a FLOAT-variable divisor still falls
        // back to the VM (correct, measured 3.48s vs 0.24s at 20M; recorded in the roadmap
        // as the ScalarValue gap). This assertion is what caught that gap in the first
        // place: every value case above passes on VM fallback alone.
        crate::jit::reset_native_call_count();
        assert!(run_vm_jit("d = 4\n(0..64).map(i => to_float(i) / d).sum()").is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "the dividing map kernel never reached native code"
        );
    }

    /// The Int-rooted mixed map and the RAISING rounders. Two stages pinned together
    /// because the second builds on the first: (3u) an `i64`-out body through Float
    /// intermediates (`to_int(to_float(i) * 1.5)`) previously had no kernel shape at all;
    /// (3v) `floor`/`ceil`/`round`/`trunc` may now appear in mixed map bodies, backed by a
    /// poison out-param — on any out-of-i64-range result the VM discards the whole output
    /// and the bytecode loop re-runs to raise the exact interpreter error.
    ///
    /// The two sabotage-proven cruxes: `round` is HALF-AWAY-FROM-ZERO — lowering it to
    /// Cranelift's `nearest` (round-to-nearest-even) turns `[1, 2, 3, 4]` into
    /// `[0, 2, 2, 4]` on the tie battery — and the `0.49999999999999994` case defeats the
    /// textbook `trunc(x + copysign(0.5, x))` lowering, whose f64 add rounds up to 1.0.
    /// And a RAISING kernel must never take the in-place buffer reuse: forcing it makes the
    /// 4-param in-place runner call a 5-param kernel (ABI mismatch, crash) — and even with
    /// matched ABI, a poison after mutating the source would corrupt the fall-back's input.
    #[test]
    fn int_rooted_mixed_maps_and_raising_rounders_agree_and_poison_exactly() {
        for (src, want) in [
            // Stage 3u: Int-rooted mixed bodies (no rounder, never raise).
            ("(0..8).map(i => to_int(to_float(i) * 1.5))", "[0, 1, 3, 4, 6, 7, 9, 10]"),
            ("c = 10\n(0..6).map(i => to_int(to_float(i) * 1.5) + c)", "[10, 11, 13, 14, 16, 17]"),
            ("fn f(x) = x * 3\n(0..6).map(i => to_int(to_float(f(i)) * 0.5))", "[0, 1, 3, 4, 6, 7]"),
            ("(1..4).map(i => to_int(to_float(i) * 1e19))", "[9223372036854775807, 9223372036854775807, 9223372036854775807]"),
            ("(0..3).map(i => to_int(sqrt(-to_float(i + 1))))", "[0, 0, 0]"),
            // Stage 3v: every tie, both signs — half-away-from-zero.
            ("(0..4).map(i => round(to_float(i) + 0.5))", "[1, 2, 3, 4]"),
            ("(0..4).map(i => round(-to_float(i) - 0.5))", "[-1, -2, -3, -4]"),
            // The largest f64 below 0.5: `f64::round` gives 0; the add-0.5 shortcut gives 1.
            ("(0..2).map(i => round(to_float(i) * 0.49999999999999994))", "[0, 0]"),
            ("(0..4).map(i => floor(-to_float(i) - 0.5))", "[-1, -2, -3, -4]"),
            ("(0..4).map(i => ceil(-to_float(i) - 0.5))", "[0, -1, -2, -3]"),
            ("(0..4).map(i => trunc(-to_float(i) - 0.5))", "[0, -1, -2, -3]"),
            ("(0..4).map(i => floor(i) + 1)", "[1, 2, 3, 4]"),
            // Exactly representable MIN is accepted; the poison range check is half-open.
            (
                "(1..3).map(i => round(to_float(i) * (-4.611686018427388e18)))",
                "[-4611686018427387904, -9223372036854775808]",
            ),
            // A capture, a user call, the Float-rooted variant, and a shadowed `round`.
            ("c = 2\n(0..5).map(i => round(to_float(i * c) * 0.5))", "[0, 1, 2, 3, 4]"),
            ("fn f(x) = x * 3\n(0..5).map(i => trunc(to_float(f(i)) * 0.5))", "[0, 1, 3, 4, 6]"),
            ("(0..5).map(i => to_float(round(to_float(i) * 0.5)) * 2.0)", "[0.0, 2.0, 2.0, 4.0, 4.0]"),
            ("fn round(x) = 99\n(0..3).map(i => round(to_float(i) * 0.5) + 0)", "[99, 99, 99]"),
            // A chain whose SECOND map raises: the dead intermediate must survive the
            // poisoned kernel so the bytecode re-run raises over intact input.
            ("(0..5).map(i => i * 2).map(i => round(to_float(i) * 0.5))", "[0, 1, 2, 3, 4]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // The raises: identical ERROR TEXT on all three engines, from a range source, a
        // chained dead intermediate, NaN, and inf.
        for src in [
            "(0..5).map(i => round(to_float(i) * 4.0e18))",
            "(1..4).map(i => floor(to_float(i) * 1e19))",
            "(0..5).map(i => i * 2).map(i => round(to_float(i) * 4.0e18))",
            "(0..3).map(i => round(sqrt(-to_float(i + 1))))",
            "(0..3).map(i => floor(to_float(i) + inf))",
            "(1..3).map(i => round(to_float(i) * 4.611686018427388e18))",
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert!(tw.is_err(), "`{src}` should raise");
            assert_eq!(tw, vm, "tree-walker and VM disagree on the error for `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on the error for `{src}`");
        }
        // Engagement, separately per specialization: the Int-rooted kernel and the raising
        // kernel each must actually run — agreement alone is satisfied by declining.
        for src in [
            "(0..64).map(i => to_int(to_float(i) * 1.5)).sum()",
            "(0..64).map(i => round(to_float(i) * 0.5)).sum()",
        ] {
            crate::jit::reset_native_call_count();
            assert!(run_vm_jit(src).is_ok());
            assert!(crate::jit::native_call_count() > 0, "never reached native: `{src}`");
        }
    }

    /// `scan` — the prefix fold — gets a native kernel. It was the last comprehension with no
    /// native form at all: 0.54s against its `reduce` twin's 0.00s at 10M elements, now 0.05s.
    ///
    /// The kernel is SERIAL by definition (element *j* depends on element *j−1*), so there is
    /// no parallel form to keep byte-identical; order is the definition. Every value case here
    /// checks the FULL output array element-wise, because the kernel's one new obligation over
    /// a reduce is the store index — and an off-by-one there is the classic scan bug
    /// (inclusive vs exclusive). Sabotage-proven: storing the PRE-update accumulator turns
    /// `[0, 1, 3, 6, …]` into `[0, 0, 1, 3, …]` on the JIT alone, caught by the first case.
    ///
    /// The declines matter equally: the guard's operands are consumed whether or not the
    /// native path is taken (`TryJitFused`'s protocol), so a Float init/capture, an array
    /// source, an array capture, or a shadowed `range` exercises exactly the stack discipline
    /// a mistake would corrupt.
    #[test]
    fn scan_compiles_to_a_native_prefix_fold_and_declines_exactly() {
        for (src, want) in [
            ("(0..8).scan(0, (s, x) => s + x)", "[0, 1, 3, 6, 10, 15, 21, 28]"),
            ("(0..5).scan(100, (s, x) => s + x)", "[100, 101, 103, 106, 110]"),
            ("(3..8).scan(0, (s, x) => s + x)", "[3, 7, 12, 18, 25]"),
            // 20! wraps i64 twice over; the wrap must be the interpreter's.
            ("(1..21).scan(1, (s, x) => s * x).last()", "2432902008176640000"),
            ("(0..6).scan(0, (s, x) => if x * 7 % 5 > s then x * 7 % 5 else s)", "[0, 2, 4, 4, 4, 4]"),
            ("c = 3\n(0..6).scan(0, (s, x) => s + x * c)", "[0, 3, 9, 18, 30, 45]"),
            ("a = 2\nb = 5\n(0..6).scan(0, (s, x) => s + x * a + b)", "[5, 12, 21, 32, 45, 60]"),
            ("fn dbl(x) = x * 2\n(0..6).scan(0, (s, x) => s + dbl(x))", "[0, 2, 6, 12, 20, 30]"),
            ("fn dbl(x) = x * 2\nc = 1\n(0..6).scan(0, (s, x) => s + dbl(x) + c)", "[1, 4, 9, 16, 25, 36]"),
            ("big = 9223372036854775807\n(0..2).scan(big, (s, x) => s + 1)", "[-9223372036854775808, -9223372036854775807]"),
            ("(0..100).scan(0, (s, x) => s + x).reduce(0, (a, y) => a + y)", "166650"),
            ("(0..5).scan(0, (s, x) => s + x).map(it * 2)", "[0, 2, 6, 12, 20]"),
            // Declines — each re-evaluates its operands on the fall-through, so these are the
            // stack-discipline and idempotence cases as much as value cases.
            ("(0..5).scan(0.0, (s, x) => s + to_float(x))", "[0.0, 1.0, 3.0, 6.0, 10.0]"),
            ("c = 2.5\n(0..5).scan(0, (s, x) => s + x * c)", "[0.0, 2.5, 7.5, 15.0, 25.0]"),
            ("[5, 1, 4].scan(0, (s, x) => s + x)", "[5, 6, 10]"),
            ("a = [10, 20, 30, 40, 50]\n(0..5).scan(0, (s, x) => s + a[x])", "[10, 30, 60, 100, 150]"),
            // A user `range` must be CALLED, never fused over.
            ("fn range(a, b) = [7, 8]\nrange(0, 2).scan(0, (s, x) => s + x)", "[7, 15]"),
            // Degenerate ranges and scope hygiene.
            ("(0..0).scan(42, (s, x) => s + x)", "[]"),
            ("(5..0).scan(42, (s, x) => s + x)", "[]"),
            ("(7..8).scan(1, (s, x) => s + x)", "[8]"),
            ("s = 50\n(0..4).scan(s, (s, x) => s + x)", "[50, 51, 53, 56]"),
            ("x = 999\nr = (0..4).scan(0, (s, x) => s + x)\n[r, [x]]", "[[0, 1, 3, 6], [999]]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // Element-wise reads at a size where a wrong store index cannot hide in a sum.
        let src = "a = (0..100000).scan(0, (s, x) => s + x)\n[a[0], a[1], a[99998], a[99999]]";
        let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
        assert_eq!(tw, vm);
        assert_eq!(vm, jit);
        assert_eq!(vm, Ok("[0, 1, 4999850001, 4999950000]".to_string()));

        // And the kernel must actually run — every assertion above is satisfied by a guard
        // that always falls through.
        crate::jit::reset_native_call_count();
        assert!(run_vm_jit("(0..64).scan(0, (s, x) => s + x).last()").is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "the scan kernel never reached native code"
        );
    }

    /// A CAPTURED i64 `map(f).reduce(init, g)` chain fuses by substitution instead of
    /// materializing its intermediate array. Before this it had NO fused form at all — the
    /// capture-free chain goes through `FusedKernel` (which has no caps slice), so a captured
    /// one materialized: 0.34s and 110 MB against 0.00s and 20 MB at 10M elements.
    ///
    /// The load-bearing guard is capture SAFETY: `f`'s body is about to sit inside a lambda
    /// binding the accumulator, so a free variable of `f` named `pa` would be captured by the
    /// accumulator and silently change meaning. On this i64 arm that corruption can be MASKED —
    /// if the stolen variable was `f`'s only free var, the corrupted body has no captures left
    /// and the empty-caps check declines by luck. The `s`+`c` case below defeats the mask with a
    /// second, genuine capture: sabotaging the guard makes it return 618 on the JIT against 55
    /// on the other two engines, which is how it earned its place here.
    #[test]
    fn a_captured_i64_map_reduce_chain_fuses_and_shadowing_declines() {
        for (src, want) in [
            ("c = 3\n(0..6).map(i => i * c + 1).reduce(0, (s, x) => s + x)", "51"),
            ("k = 10\n(0..6).map(i => i + 1).reduce(0, (s, x) => s + x * k)", "210"),
            ("c = 3\nk = 10\n(0..6).map(i => i * c).reduce(0, (s, x) => s + x * k)", "450"),
            ("c = 3\n(0..6).map(i => i * c).reduce(0, (s, x) => s + x + c)", "63"),
            ("c = 3\n(0..6).map(it * c + 1).reduce(0, (s, x) => s + x)", "51"),
            ("a = [10, 20, 30, 40]\n(0..4).map(i => a[i] * 2).reduce(0, (s, x) => s + x)", "200"),
            ("a = [10, 20, 30, 40]\nc = 3\n(0..4).map(i => a[i] * c).reduce(0, (s, x) => s + x)", "300"),
            ("c = 9223372036854775807\n(0..4).map(i => i * c).reduce(0, (s, x) => s + x)", "-6"),
            ("c = -7\n(0..6).map(i => i * c).reduce(100, (s, x) => s + x)", "-5"),
            ("fn f(x) = x * 2\nc = 5\n(0..6).map(i => f(i) + c).reduce(0, (s, x) => s + x)", "60"),
            // THE MASK-DEFEATING CASE: `f` mentions the accumulator's name AND carries a second
            // capture. Fusion must decline (capture safety), and the answer must be the
            // unfused 55 — a corrupted substitution gives 618.
            ("s = 4\nc = 3\n(0..5).map(i => i * s + c).reduce(0, (s, x) => s + x)", "55"),
            // Its single-capture sibling, where the corruption would be masked — kept so both
            // routes through the guard are pinned.
            ("s = 4\n(0..5).map(i => i * s).reduce(0, (s, x) => s + x)", "40"),
            // `init` evaluates in the OUTER scope even when it names the accumulator binder.
            ("s = 100\nc = 2\n(0..5).map(i => i * c).reduce(s, (s, x) => s + x)", "120"),
            // OOB inside `f` must produce the fall-through's exact error, not a native load.
            // Negative start Python-wraps in the interpreter, so it must decline and agree.
            ("a = [10, 20, 30]\nc = 1\n(-1..3).map(i => a[i] * c).reduce(0, (s, x) => s + x)", "90"),
            // A Float capture declines to the ordinary path and still answers.
            ("c = 2.5\n(0..5).map(i => i * c).reduce(0, (s, x) => s + x)", "25.0"),
            // Degenerate ranges return `init` untouched.
            ("c = 3\n(0..0).map(i => i * c).reduce(42, (s, x) => s + x)", "42"),
            ("c = 3\n(5..0).map(i => i * c).reduce(42, (s, x) => s + x)", "42"),
            // The two NEIGHBOURS this must not disturb: the capture-free `FusedKernel` chain
            // and the Float-init substitution.
            ("(0..6).map(i => i * 2 + 1).reduce(0, (s, x) => s + x)", "36"),
            ("c = 0.5\n(0..6).map(i => to_float(i) * c).reduce(0.0, (s, x) => s + x)", "7.5"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // The OOB-at-end decline: all three engines must raise the SAME error text.
        let src = "a = [10, 20, 30]\nc = 1\n(0..4).map(i => a[i] * c).reduce(0, (s, x) => s + x)";
        let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
        assert!(tw.is_err(), "OOB should raise");
        assert_eq!(tw, vm, "tree-walker and VM disagree on the OOB error");
        assert_eq!(vm, jit, "VM and JIT disagree on the OOB error");

        // Engagement: the fused chain must actually reach a native reduce kernel.
        crate::jit::reset_native_call_count();
        assert!(run_vm_jit("c = 3\n(0..64).map(i => i * c + 1).reduce(0, (s, x) => s + x)").is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "the captured i64 fused chain never reached a native kernel"
        );
    }

    /// An `f64` reduce body may CAPTURE a scalar and may CALL an `i64` user function. Both
    /// spellings used to fall to the bytecode loop while their literal/inline twins ran
    /// natively — 0.78s vs 0.01s for a captured coefficient, 0.74s vs 0.01s for a call, over
    /// 10M elements.
    ///
    /// Two paths had to learn this, and they are gated separately: a body WITH captures goes
    /// through the indexed analysis, a capture-FREE one through `infer_reduce_f64_kind`. So a
    /// call-only body and a call-plus-capture body exercise different code and both are here.
    ///
    /// The promotion rule is the correctness crux: a value scalar rides as `f64` but may be
    /// `Int` at runtime, so `mix_combine` admits it only where a genuine float promotes it.
    /// The 2^53 cases pin that — where `i64` and `f64` genuinely differ.
    #[test]
    fn an_f64_reduce_may_capture_a_scalar_and_call_a_user_function() {
        for (src, want) in [
            // Scalar captures, float and int.
            ("c = 0.5\n(0..6).reduce(0.0, (s, i) => s + to_float(i) * c)", "7.5"),
            ("c = 3\n(0..6).reduce(0.0, (s, i) => s + to_float(i) * c)", "45.0"),
            ("a = 0.5\nb = 2.0\n(0..6).reduce(0.0, (s, i) => s + to_float(i) * a + b)", "19.5"),
            ("xs = [1.0, 2.0, 3.0]\nc = 2.0\n(0..3).reduce(0.0, (s, i) => s + c * xs[i])", "12.0"),
            // Past 2^53, where an `i64` product and an `f64` one diverge.
            (
                "c = 9007199254740993\n(0..3).reduce(0.0, (s, i) => s + to_float(c * i))",
                "27021597764222976.0",
            ),
            // User calls: capture-free path, then the captured path.
            ("fn f(x) = x * 2\n(0..6).reduce(0.0, (s, i) => s + to_float(f(i)))", "30.0"),
            ("fn f(x) = x * 2\nc = 0.5\n(0..6).reduce(0.0, (s, i) => s + to_float(f(i)) * c)", "15.0"),
            ("fn g(x) = x + 1\nfn f(x) = g(x) * 2\n(0..6).reduce(0.0, (s, i) => s + to_float(f(g(i))))", "54.0"),
            ("fn f(a, b) = a * b + 1\n(0..6).reduce(0.0, (s, i) => s + to_float(f(i, 3)))", "51.0"),
            // The callee wraps at i64::MAX before the body promotes.
            ("fn f(x) = x * 9223372036854775807\n(0..4).reduce(0.0, (s, i) => s + to_float(f(i) % 100))", "110.0"),
            ("fn tri(x, acc) = if x <= 0 then acc else tri(x - 1, acc + x)\n(0..5).reduce(0.0, (s, i) => s + to_float(tri(i, 0)))", "20.0"),
            // A user function shadowing a builtin must win over the inline lowering.
            ("fn abs(x) = x + 1000\n(0..4).reduce(0.0, (s, i) => s + to_float(abs(-i)))", "3994.0"),
            ("fn min(a, b) = a\n(0..4).reduce(0.0, (s, i) => s + to_float(min(i, 99)))", "6.0"),
            ("fn sign(x) = x - 1\n(0..4).reduce(0.0, (s, i) => s + to_float(sign(i)))", "2.0"),
            // Declines that must still produce the interpreter's answer.
            ("fn f(x) = x * 2\n(0..4).reduce(0.0, (s, i) => s + f(2.5))", "20.0"),
            ("fn f(x) = x / 2\n(0..4).reduce(0.0, (s, i) => s + to_float(f(i)))", "3.0"),
            ("fn f(x) = if x <= 0 then 0 else x + f(x - 1)\n(0..4).reduce(0.0, (s, i) => s + to_float(f(i)))", "10.0"),
            ("c = 0.5\n(0..0).reduce(0.0, (s, i) => s + to_float(i) * c)", "0.0"),
            // Shapes that must be UNCHANGED by this: the Stage 3h dot product, and the
            // dividing body whose kernel carries a poison out-param.
            ("a = [1.5, 2.5, 3.5]\nb = [0.25, 0.5, 0.75]\nc = 2.5\n(0..3).reduce(0.0, (s, i) => s + c * a[i] + b[i])", "20.25"),
            ("(0..5).reduce(0.0, (s, i) => s + to_float(i) / 2.0)", "5.0"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // Both new shapes must actually reach native code, checked separately because they
        // take different paths (capture-free vs captured).
        for src in [
            "fn f(x) = x * 2\n(0..64).reduce(0.0, (s, i) => s + to_float(f(i)))",
            "c = 0.5\n(0..64).reduce(0.0, (s, i) => s + to_float(i) * c)",
        ] {
            crate::jit::reset_native_call_count();
            assert!(run_vm_jit(src).is_ok());
            assert!(crate::jit::native_call_count() > 0, "never reached a native kernel: `{src}`");
        }
    }

    /// A `filter` predicate may CAPTURE free `i64` scalars. Before this, swapping a literal
    /// threshold for a variable moved the whole filter onto the bytecode loop — the same cliff
    /// the map body had, measured 0.55s against 0.01s over 10M elements.
    ///
    /// The filter kernel COMPACTS, so its output offset depends on how many earlier elements
    /// were kept; chunk boundaries are therefore the sharp edge and are read INDIVIDUALLY here
    /// (a sum can hide a swapped pair). The decline cases matter as much as the accepting ones:
    /// they exercise popping the captures off the stack and falling through to the bytecode
    /// loop, which is where a stack-discipline mistake would surface.
    #[test]
    fn a_filter_predicate_may_capture_and_declines_a_non_int_capture() {
        for (src, want) in [
            ("k = 3\n(0..8).filter(it > k)", "[4, 5, 6, 7]"),
            ("c = 2\n(0..8).filter(it * c > 6)", "[4, 5, 6, 7]"),
            ("lo = 2\nhi = 6\n(0..10).filter(it > lo and it < hi)", "[3, 4, 5]"),
            ("a = 1\nb = 8\n(0..10).filter(it < a or it > b)", "[0, 9]"),
            ("k = 3\n(0..10).filter(it > k and it < k * 3)", "[4, 5, 6, 7, 8]"),
            ("k = -3\n(-5..5).filter(it > k)", "[-2, -1, 0, 1, 2, 3, 4]"),
            // A literal modulus is still required (a variable divisor could be 0, which must
            // raise); combining one with a capture must work.
            ("k = 3\n(0..20).filter(it % 5 == 0 and it > k)", "[5, 10, 15]"),
            ("fn dbl(x) = x * 2\nk = 4\n(0..10).filter(dbl(it) > k)", "[3, 4, 5, 6, 7, 8, 9]"),
            ("k = 2\n[5, 1, 4, 2, 3].filter(it > k)", "[5, 4, 3]"),
            ("k = 2\n(0..6).where(it > k)", "[3, 4, 5]"),
            // A non-`Int` capture must DECLINE to the bytecode loop, never reinterpret bits.
            ("k = 2.5\n(0..8).filter(it > k)", "[3, 4, 5, 6, 7]"),
            ("k = 3.0\n(0..8).filter(it > k)", "[4, 5, 6, 7]"),
            // Degenerate sources, and the all-keep / all-drop extremes of a compacting loop.
            ("k = 3\n(0..0).filter(it > k)", "[]"),
            ("k = 3\n(8..0).filter(it > k)", "[]"),
            ("k = -1\n(0..5).filter(it > k)", "[0, 1, 2, 3, 4]"),
            ("k = 100\n(0..5).filter(it > k)", "[]"),
            // Roughly one survivor per chunk, where a wrong output offset shows up at once
            // instead of being absorbed by its neighbours.
            ("k = -1\nn = 40000\n(0..n).filter(it % 16384 == 0 and it > k)", "[0, 16384, 32768]"),
            ("k = 20000\nn = 40000\n(0..n).filter(it % 8192 == 0 and it < k)", "[0, 8192, 16384]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // Chunk-boundary elements read one at a time, straddling `PAR_MATH_THRESHOLD`.
        let src = "k = -1\nn = 40000\na = (0..n).filter(it > k)\n\
                   [a[0], a[16383], a[16384], a[32767], a[32768], a[39999]]";
        let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
        assert_eq!(tw, vm, "tree-walker and VM disagree at the chunk boundaries");
        assert_eq!(vm, jit, "VM and JIT disagree at the chunk boundaries");
        assert_eq!(vm, Ok("[0, 16383, 16384, 32767, 32768, 39999]".to_string()));

        crate::jit::reset_native_call_count();
        assert!(run_vm_jit("k = 3\n(0..64).filter(it > k).count()").is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "a capturing filter predicate never reached a native kernel"
        );
    }

    /// A mixed `Int`→`Float` map body may CALL an `i64`-eligible user function. Factoring a
    /// loop body into a named function used to drop the whole map to the bytecode loop —
    /// measured 1.50s against 0.02s inline over 20M elements, and 2.00s vs 0.02s with an
    /// integer op wrapped around the call. Both spellings now cost the same.
    ///
    /// The sharp edge is SHADOWING: the user-call arm is tried before the inline-builtin arm,
    /// so `fn abs(x) = x + 1000` must dispatch to the user's function and never to `iabs`.
    /// Each shadow case below returns something the real builtin never would, so a wrong
    /// dispatch cannot coincide with the right answer.
    #[test]
    fn a_mixed_map_body_may_call_an_i64_user_function() {
        for (src, want) in [
            // Shadowed builtins: each returns what the genuine builtin would not.
            ("fn abs(x) = x + 1000\n(0..4).map(i => abs(-i) * 0.5)", "[500.0, 499.5, 499.0, 498.5]"),
            ("fn min(a, b) = a\n(0..4).map(i => min(i, 99) * 0.5)", "[0.0, 0.5, 1.0, 1.5]"),
            ("fn max(a, b) = a * 10 + b\n(0..3).map(i => max(i, 2) * 0.5)", "[1.0, 6.0, 11.0]"),
            ("fn to_int(x) = x * 7\n(0..3).map(i => to_int(i) * 0.5)", "[0.0, 3.5, 7.0]"),
            ("fn sign(x) = x - 1\n(0..3).map(i => sign(i) * 0.5)", "[-0.5, 0.0, 0.5]"),
            // Ordinary shapes.
            ("fn f(x) = x * 2 + 1\n(0..5).map(i => f(i) * 0.5)", "[0.5, 1.5, 2.5, 3.5, 4.5]"),
            ("fn f(x) = x * 2 + 1\n(0..5).map(i => (f(i) % 4) * 0.25)", "[0.25, 0.75, 0.25, 0.75, 0.25]"),
            ("fn g(x) = x + 1\nfn f(x) = g(x) * 2\n(0..5).map(i => f(g(i)) * 0.5)", "[2.0, 3.0, 4.0, 5.0, 6.0]"),
            ("c = 3\nfn f(x) = x * 2\n(0..5).map(i => f(i + c) * 0.5)", "[3.0, 4.0, 5.0, 6.0, 7.0]"),
            ("fn f(a, b) = a * b + 1\n(0..5).map(i => f(i, 3) * 0.5)", "[0.5, 2.0, 3.5, 5.0, 6.5]"),
            // The callee wraps at i64::MAX; the kernel must wrap identically before promoting.
            (
                "fn f(x) = x * 9223372036854775807\n(0..4).map(i => (f(i) % 100) * 0.5)",
                "[0.0, 3.5, 49.0, 2.5]",
            ),
            ("fn f(x, acc) = if x <= 0 then acc else f(x - 1, acc + x)\n(0..5).map(i => f(i, 0) * 0.5)", "[0.0, 0.5, 1.5, 3.0, 5.0]"),
            // A FLOAT argument must decline — the callee has no f64 form. These must still
            // produce the interpreter's answer, via the bytecode loop.
            ("fn f(x) = x * 2\n(0..3).map(i => f(2.5) * 0.5)", "[2.5, 2.5, 2.5]"),
            ("fn f(x) = x * 2\n(0..3).map(i => f(to_float(i)) * 0.5)", "[0.0, 1.0, 2.0]"),
            // Callees the i64 specialization does not compile must decline, not miscompile.
            ("fn f(x) = x / 2\n(0..3).map(i => f(i) * 0.5)", "[0.0, 0.25, 0.5]"),
            ("fn f(x) = x * 1.5\n(0..3).map(i => f(i) * 0.5)", "[0.0, 0.75, 1.5]"),
            ("fn f(x) = if x <= 0 then 0 else x + f(x - 1)\n(0..4).map(i => f(i) * 0.5)", "[0.0, 0.5, 1.5, 3.0]"),
            // Degenerate and non-range sources.
            ("fn f(x) = x * 2\n(0..0).map(i => f(i) * 0.5)", "[]"),
            ("fn f(x) = x * 2\n[1, 2, 3].map(i => f(i) * 0.5)", "[1.0, 2.0, 3.0]"),
            // A call inside a NESTED build, where the inner map also captures the outer binder.
            (
                "fn f(x) = x % 7\nn = 3\n(0..n).map(i => (0..n).map(j => f(i * j) * 0.5))",
                "[[0.0, 0.0, 0.0], [0.0, 0.5, 1.0], [0.0, 1.0, 2.0]]",
            ),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // The call must actually reach native code — every assertion above is satisfied by a
        // JIT that declines, which is exactly the state this test exists to prevent.
        crate::jit::reset_native_call_count();
        let src = "fn f(x) = x * 2 + 1\n(0..64).map(i => f(i) * 0.5).sum()";
        assert!(run_vm_jit(src).is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "a user call in a mixed map body never reached a native kernel"
        );
    }

    /// A map over a uniquely-owned array REUSES its buffer instead of allocating a second
    /// one. `xs.map(f).map(g)` used to peak at both buffers even though the intermediate is
    /// dead the moment `g` consumes it — measured 340 MB at n=20M against 186 MB now.
    ///
    /// The failure mode is the opposite of a wrong number: a source that is still reachable
    /// being mutated under the program. So every case here keeps a SECOND way to observe the
    /// original and reads it AFTER the map. Sabotage-proven — replacing `Rc::get_mut` with an
    /// unconditional `&mut` makes `src` print `[10, 20, 30]` instead of `[1, 2, 3]` on the JIT
    /// while the other two engines still print the original.
    ///
    /// Sizes straddle `PAR_MATH_THRESHOLD`, because the parallel form aliases per chunk and a
    /// boundary mistake is invisible below it. Boundary elements are read INDIVIDUALLY rather
    /// than summed, since a sum can hide a swapped pair.
    #[test]
    fn map_reuses_a_dead_buffer_but_never_a_reachable_one() {
        for (src, want) in [
            // Still reachable under its own name, an alias, a field, and a closure — none of
            // these may be rewritten.
            ("src = [1, 2, 3]\nout = src.map(it * 10)\n[src, out]", "[[1, 2, 3], [10, 20, 30]]"),
            (
                "src = [1, 2, 3]\nalias = src\nout = src.map(it * 10)\n[alias, src, out]",
                "[[1, 2, 3], [1, 2, 3], [10, 20, 30]]",
            ),
            // Mapped twice: the second map must see the ORIGINAL, not the first's output.
            (
                "src = [1, 2, 3]\na = src.map(it * 10)\nb = src.map(it + 100)\n[src, a, b]",
                "[[1, 2, 3], [10, 20, 30], [101, 102, 103]]",
            ),
            ("r = {xs: [1, 2, 3]}\nout = r.xs.map(it * 10)\n[r.xs, out]", "[[1, 2, 3], [10, 20, 30]]"),
            ("src = [1.5, 2.5]\nout = src.map(it * 2.0)\n[src, out]", "[[1.5, 2.5], [3.0, 5.0]]"),
            // Dead intermediates: the values must still be right after reuse.
            ("(0..5).map(it * 2).map(it + 1)", "[1, 3, 5, 7, 9]"),
            ("(0..5).map(it * 2).map(it + 1).map(it * 3)", "[3, 9, 15, 21, 27]"),
            ("[1, 2, 3].map(it * 2).map(it + 1)", "[3, 5, 7]"),
            ("(0..5).map(it * 0.5).map(it + 1.0)", "[1.0, 1.5, 2.0, 2.5, 3.0]"),
            ("(0..0).map(it * 2).map(it + 1)", "[]"),
            ("c = 7\n(0..5).map(it * c).map(it + c)", "[7, 14, 21, 28, 35]"),
            // A chain where the middle stage is named, so IT is reachable while the first is not.
            (
                "src = (0..5).map(it * 2)\nmid = src.map(it + 1)\nout = mid.map(it * 3)\n[src, mid, out]",
                "[[0, 2, 4, 6, 8], [1, 3, 5, 7, 9], [3, 9, 15, 21, 27]]",
            ),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // Chunk boundaries, read one at a time. 16384 and 32768 are the chunk edges; 32768 is
        // also PAR_MATH_THRESHOLD itself, where the whole run moves onto rayon workers.
        let src = "n = 40000\na = (0..n).map(it * 2).map(it + 1)\n\
                   [a[0], a[16383], a[16384], a[32767], a[32768], a[39999]]";
        let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
        assert_eq!(tw, vm, "tree-walker and VM disagree at the chunk boundaries");
        assert_eq!(vm, jit, "VM and JIT disagree at the chunk boundaries");
        assert_eq!(vm, Ok("[1, 32767, 32769, 65535, 65537, 79999]".to_string()));

        // Just under the threshold, so the serial path is exercised at a near-boundary size.
        let src = "n = 32767\na = (0..n).map(it * 2).map(it + 1)\na[n - 1]";
        assert_eq!(run_vm_jit(src), run_tw(src));
        assert_eq!(run_vm_jit(src), Ok("65533".to_string()));

        // And the reuse must actually be reached natively — a JIT that declined every chain
        // would satisfy every assertion above.
        crate::jit::reset_native_call_count();
        assert!(run_vm_jit("(0..70000).map(it * 2).map(it + 1)[69999]").is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "the chained map never reached a native kernel"
        );
    }

    /// A mixed `Int`-source → `Float` map that CAPTURES a free scalar. The capture rides as a
    /// plain `i64` and is typed `Int` by the kernel, which matches the interpreter only if an
    /// integer subexpression containing it wraps identically and promotion happens at the same
    /// node — so the cases that matter are the ones that overflow or sit at 2^53, not the ones
    /// that merely work.
    ///
    /// The `Float`-capture rows are the guard: typing `c` as `i64` is sound ONLY because the VM
    /// proves it is a `Value::Int` before dispatch. Sabotaging that check (accepting a `Float`
    /// by truncation) makes `c = 2.5` return `[0.0, 1.0, 2.0, 3.0]` on the JIT against
    /// `[0.0, 1.25, 2.5, 3.75]` on the other two engines — which is why `2.5` is here and not
    /// just `2.0`, whose truncation is invisible.
    #[test]
    fn captured_mixed_int_to_float_map_agrees_and_declines_a_float_capture() {
        for (src, want) in [
            // The capture makes an integer subexpression wrap; the kernel must wrap the same.
            (
                "c = 9223372036854775807\n(0..4).map(j => ((c * j) % 100) * 0.5)",
                "[0.0, 3.5, 49.0, 2.5]",
            ),
            ("c = 9223372036854775807\n(0..3).map(j => (c + j) * 1.0)", "[9223372036854775808.0, -9223372036854775808.0, -9223372036854775808.0]"),
            ("c = -7\n(0..4).map(j => ((c * j) % 100) * 0.5)", "[0.0, 46.5, 43.0, 39.5]"),
            // Exact in `i64` past `f64`'s 2^53: promotion happens once, at the end.
            ("c = 9007199254740993\n(1..3).map(j => (c * j) * 1.0)", "[9007199254740992.0, 18014398509481984.0]"),
            ("c = 9007199254740992\n(1..4).map(j => (c + j) * 1.0)", "[9007199254740992.0, 9007199254740994.0, 9007199254740996.0]"),
            // A FLOAT capture must decline to the bytecode loop, not promote early.
            ("c = 2.5\n(0..4).map(j => (c * j) * 0.5)", "[0.0, 1.25, 2.5, 3.75]"),
            ("c = 2.0\n(0..4).map(j => (c * j) * 0.5)", "[0.0, 1.0, 2.0, 3.0]"),
            // Shape coverage: several captures, a repeat, an empty and a reversed range, a
            // data-array source, and the builtins the mixed analysis admits.
            ("a = 3\nb = 5\n(0..4).map(j => ((a * j + b) % 7) * 0.25)", "[1.25, 0.25, 1.0, 0.0]"),
            ("c = 6\n(0..4).map(j => (c * j + c) * 0.5)", "[3.0, 6.0, 9.0, 12.0]"),
            ("c = 3\n(0..0).map(j => (c * j) * 0.5)", "[]"),
            ("c = 3\n(4..0).map(j => (c * j) * 0.5)", "[]"),
            ("c = 3\n[1, 2, 3].map(j => (c * j) * 0.5)", "[1.5, 3.0, 4.5]"),
            ("c = 3\n(0..4).map(j => to_float(c * j) * 0.5)", "[0.0, 1.5, 3.0, 4.5]"),
            ("c = 3\n(0..4).map(j => max(c * j, 4) * 0.5)", "[2.0, 2.0, 3.0, 4.5]"),
            ("c = 3\n(0..4).map(j => ((c * j) << 2) * 0.5)", "[0.0, 6.0, 12.0, 18.0]"),
            // k8's build shape: the inner map captures the OUTER binder, which is the whole
            // reason a nested array build used to fall to the VM.
            (
                "n = 3\n(0..n).map(i => (0..n).map(j => ((i * j) % 100) * 0.5))",
                "[[0.0, 0.0, 0.0], [0.0, 0.5, 1.0], [0.0, 1.0, 2.0]]",
            ),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // Agreement alone would be satisfied by a JIT that declined every one of the above.
        crate::jit::reset_native_call_count();
        let src = "c = 7\n(0..64).map(j => ((c * j) % 100) * 0.5).sum()";
        assert!(run_vm_jit(src).is_ok());
        assert!(
            crate::jit::native_call_count() > 0,
            "the captured mixed map never reached a native kernel"
        );
    }

    /// `i64` arithmetic wraps, and division is total. Both are deliberate (see
    /// `docs/integer-semantics.md`), and both are easy to break silently: Cranelift's
    /// `sdiv`/`srem` raise a hardware trap (SIGFPE) on divide-by-zero AND on
    /// `i64::MIN / -1`, so a kernel that lowered `//` or `%` directly would kill the
    /// process where the tree-walker raises a catchable error. Nothing in the type
    /// system prevents that regression, so it is pinned here.
    ///
    /// The expected VALUES are asserted, not just cross-engine agreement — three engines
    /// that agree on a newly-wrong answer would satisfy agreement alone.
    #[test]
    fn integer_overflow_wraps_and_division_is_total_on_every_engine() {
        // MIN has no literal spelling (the lexer sees `9223372036854775808` first), so it
        // is built by subtraction — which is itself the wrapping behaviour under test.
        const MIN: &str = "(0 - 9223372036854775807 - 1)";
        for (src, want) in [
            ("9223372036854775807 + 1", "-9223372036854775808"),
            ("9223372036854775807 * 2", "-2"),
            ("(0 - 9223372036854775807) - 2", "9223372036854775807"),
            ("9223372036854775807 * 9223372036854775807", "1"),
            // `/` is true division: it always promotes to `f64`, so it never wraps — but
            // promotion is not exactness. `MAX / 1` rounds to 2^63 because `i64::MAX` has
            // no `f64` representation. Both failure modes are real and they are different.
            ("9223372036854775807 / 1", "9223372036854775808.0"),
            // `to_int` saturates rather than wrapping or trapping: total for finite input.
            ("to_int(9.3e18)", "9223372036854775807"),
            // `i64::MAX` is not representable as `f64`; the nearest is 2^63.
            ("to_float(9223372036854775807)", "9223372036854775808.0"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // The `sdiv`/`srem` overflow case, which traps in hardware but must wrap here.
        for (expr, want) in [
            (format!("{MIN} // (0 - 1)"), "-9223372036854775808"),
            (format!("{MIN} % (-1)"), "0"),
            (format!("{MIN} / (0 - 1)"), "9223372036854775808.0"),
        ] {
            let (tw, vm, jit) = (run_tw(&expr), run_vm(&expr), run_vm_jit(&expr));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{expr}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{expr}`");
            assert_eq!(vm, Ok(want.to_string()), "`{expr}`");
        }
        // Divide-by-zero raises rather than trapping — checked with the divisor arriving
        // as ARRAY DATA so constant folding cannot mask it, and in the map / reduce /
        // compiled-function positions where a native kernel is what actually runs.
        for src in [
            "1 / 0",
            "1 % 0",
            "d = (0..3).map(it - 1)\nd.map(100 // it).sum()",
            "d = (0..3).map(it - 1)\nd.map(100 % it).sum()",
            "d = (0..3).map(it - 1)\nd.reduce(0, (s, x) => s + (100 // x))",
            "fn f(a, b) = a // b\nf(100, 0)",
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert!(tw.is_err(), "`{src}` should raise, not wrap or trap");
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
        }
        // Everything above would be satisfied by a JIT that simply declines integer `//`,
        // which would make the kernel claims vacuous. Establish that a native kernel DOES
        // run integer division, using divisors that do not raise — so the zero and
        // `MIN / -1` cases above are genuinely guarded rather than merely never compiled.
        crate::jit::reset_native_call_count();
        let safe = "d = (0..3).map(it + 1)\nd.map(100 // it).sum()";
        assert_eq!(run_vm_jit(safe), Ok("183".to_string()), "`{safe}`");
        assert!(
            crate::jit::native_call_count() > 0,
            "integer `//` never reached a native kernel, so the trap-safety cases above \
             prove nothing about compiled code"
        );

        // `MIN // -1` through a native map kernel: the divisor is data, so the kernel —
        // not constant folding — performs the division that would trap.
        let src = format!("d = (0..2).map(-1)\nd.map({MIN} // it)[0]");
        let (tw, vm, jit) = (run_tw(&src), run_vm(&src), run_vm_jit(&src));
        assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
        assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
        assert_eq!(vm, Ok("-9223372036854775808".to_string()), "`{src}`");
    }

    /// The set-like operations (`unique`, `frequencies`, `top`) answer on `values_equal`
    /// identity (ADR 0001). Two of them take a hash path when the array's kinds admit a
    /// key that reproduces those classes EXACTLY; this pins that the key is exact, that
    /// the kinds it refuses are refused, and that `unique` and `frequencies` can never
    /// report different identities for the same array.
    ///
    /// The `Int` key exists because the fallback is O(n x distinct): a 5M histogram over
    /// 10k distinct integers ran 2.5e10 comparisons — 41.7s, against 0.06s for the SAME
    /// histogram spelled with string keys. The last case here is a scale pin; if the
    /// hash path stops being taken it still PASSES, but it stalls for seconds.
    #[test]
    fn set_like_operations_hash_exactly_the_identities_they_report() {
        for (src, want) in [
            // ---- Int: the new hash path. Count desc, then value ASC BY DECIMAL STRING,
            // which is the pre-existing sort — "10" sorts before "2".
            ("[3, 1, 3, 2, 1, 3].frequencies()", "[(3, 3), (1, 2), (2, 1)]"),
            ("[10, 2, 10, 2].frequencies()", "[(10, 2), (2, 2)]"),
            ("[3, 1, 3, 2, 1, 3].unique()", "[3, 1, 2]"),
            // negative and extreme i64 keys are exact, not bucketed through a float
            ("[-1, -1, 1].frequencies()", "[(-1, 2), (1, 1)]"),
            (
                "[9223372036854775807, 0 - 9223372036854775807].unique().length()",
                "2",
            ),
            // ---- `missing` joins the Int key: all missings are ONE identity, and a
            // missing is never an integer.
            ("[missing, 1, missing].frequencies()", "[(missing, 2), (1, 1)]"),
            ("[missing, 1, missing].unique()", "[missing, 1]"),
            ("[missing, missing].frequencies()", "[(missing, 2)]"),
            ("[0, missing, 0].frequencies()", "[(0, 2), (missing, 1)]"),
            // ---- A Float anywhere REFUSES the Int key and keeps the scan, because
            // `values_equal` collapses 1 == 1.0 across types...
            ("[1, 1.0, 2].frequencies()", "[(1, 2), (2, 1)]"),
            ("[1, 1.0, 2].unique()", "[1, 2]"),
            // ...and above 2^53 that collapse is not even TRANSITIVE, so no hash key
            // could reproduce it: both integers equal the float, but not each other.
            // (Order-dependent by nature — the scan is the definition here.)
            (
                "[9007199254740993, 9007199254740992.0, 9007199254740992].frequencies()",
                "[(9007199254740993, 2), (9007199254740992, 1)]",
            ),
            ("[9007199254740993, 9007199254740992].unique().length()", "2"),
            // ---- Text: `dna("AT")` and `"AT"` are DIFFERENT identities. `values_equal`
            // has same-kind arms only, so the cross pair is false — which is what
            // `contains`/`index_of` have always reported. Keying on the bytes alone
            // merged them, and `unique().length()` disagreed with `index_of`.
            ("[dna(\"AT\"), \"AT\"].index_of(\"AT\")", "1"),
            ("[dna(\"AT\"), \"AT\"].unique().length()", "2"),
            ("[dna(\"AT\"), \"AT\"].frequencies().length()", "2"),
            // (a `Str` inside a tuple prints quoted, a `Dna` bare — so the split shows)
            ("[dna(\"AT\"), dna(\"AT\"), \"AT\"].frequencies()", "[(AT, 2), (\"AT\", 1)]"),
            // homogeneous text is untouched
            ("[\"AT\", \"TG\", \"AT\"].frequencies()", "[(\"AT\", 2), (\"TG\", 1)]"),
            ("[\"AT\", \"TG\", \"AT\"].unique()", "[\"AT\", \"TG\"]"),
            // ATG TGC GCA CAT ATG TGC -> 4 distinct
            ("dna(\"ATGCATGC\").kmers(3).frequencies().length()", "4"),
            // ---- `top` shares the histogram, so it inherits the same identities.
            ("[3, 1, 3, 2, 1, 3].top(2)", "[(3, 3), (1, 2)]"),
            ("[missing, 1, missing].top(1)", "[(missing, 2)]"),
            // ---- empty and singleton
            ("[].frequencies()", "[]"),
            ("[7].frequencies()", "[(7, 1)]"),
            // ---- kinds with no key at all keep the scan
            ("[(1, 2), (1, 2), (3, 4)].frequencies()", "[((1, 2), 2), ((3, 4), 1)]"),
            ("[true, false, true].frequencies()", "[(true, 2), (false, 1)]"),
            ("[[1], [1], [2]].unique().length()", "2"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }

        // `unique` and `frequencies` must agree on HOW MANY identities an array has, for
        // every kind mix above — they pick their keys independently, so this is the
        // property that catches one path being fixed without the other.
        for src in [
            "[3, 1, 3, 2, 1, 3]",
            "[missing, 1, missing, -1]",
            "[1, 1.0, 2]",
            "[dna(\"AT\"), \"AT\", dna(\"AT\")]",
            "[9007199254740993, 9007199254740992.0, 9007199254740992]",
            "[true, false, true]",
            "[]",
        ] {
            let a = format!("{src}.unique().length()");
            let b = format!("{src}.frequencies().length()");
            assert_eq!(run_vm(&a), run_vm(&b), "unique/frequencies disagree on `{src}`");
            assert_eq!(run_tw(&a), run_vm(&a), "engines disagree on `{a}`");
        }

        // Scale pin: 50k elements over 5000 distinct integers. The hash path is a few
        // milliseconds; the O(n x distinct) scan is ~1.25e8 `values_equal` calls and
        // takes seconds. Same shape in text, which has always been hashed.
        let ints = "(0..50000).map(it % 5000).frequencies()";
        assert_eq!(run_vm(&format!("{ints}.length()")), Ok("5000".to_string()));
        assert_eq!(run_vm(&format!("{ints}[0]")), Ok("(0, 10)".to_string()));
        let text = "(0..50000).map(\"w{it % 5000}\").frequencies()";
        assert_eq!(run_vm(&format!("{text}.length()")), Ok("5000".to_string()));
        // Every count is 10 and every distinct key is present, on both spellings — a
        // key that dropped or merged buckets would move the length or the counts.
        assert_eq!(
            run_vm(&format!("{ints}.map((v, c) => c).unique()")),
            Ok("[10]".to_string())
        );
        assert_eq!(
            run_vm(&format!("{text}.map((v, c) => c).unique()")),
            Ok("[10]".to_string())
        );
        assert_eq!(run_vm("(0..50000).map(it % 5000).unique().length()"), Ok("5000".to_string()));
    }

    /// Interpolation renders a scalar by appending it DIRECTLY to the output buffer
    /// rather than routing it through `write!(buf, "{}", value)` — two nested
    /// `fmt::Arguments` dispatches per hole. That is only sound if the short road
    /// writes byte-for-byte what the formatter writes, so this asserts exactly that,
    /// for every kind that has a fast arm and every kind that does not, against
    /// `Display` itself as the reference.
    ///
    /// One case only fails under overflow checks: writing `(-i) as u64` instead of
    /// `unsigned_abs()` wraps `i64::MIN` back to itself, whose `as u64` happens to be
    /// the right magnitude — so a release build prints the correct digits by accident.
    /// `cargo test --bin helix` (dev profile) catches it; `--profile gate` cannot.
    #[test]
    fn a_directly_appended_scalar_is_byte_identical_to_its_formatter() {
        let mut cases: Vec<Value> = vec![
            Value::Int(0),
            Value::Int(1),
            Value::Int(-1),
            Value::Int(9),
            Value::Int(10),
            Value::Int(99),
            Value::Int(100),
            // the boundaries of the two-digit-at-a-time loop, and of i64 itself
            Value::Int(i64::MAX),
            Value::Int(i64::MIN), // negating this overflows — `unsigned_abs` is why it works
            Value::Int(i64::MIN + 1),
            Value::Int(-9223372036854775807),
            Value::Bool(true),
            Value::Bool(false),
            Value::Missing,
            Value::Unit,
            Value::Str(std::rc::Rc::new(String::new())),
            Value::Str(std::rc::Rc::new("plain".to_string())),
            // a string is written RAW here (quoting happens only inside a container)
            Value::Str(std::rc::Rc::new("has \"quotes\" and \\ and \n".to_string())),
            Value::Str(std::rc::Rc::new("ünïcödé — 中文 — 🧬".to_string())),
            Value::Dna(std::rc::Rc::new("ATGC".to_string())),
            // no fast arm: these must still go through the formatter unchanged
            Value::Float(0.0),
            Value::Float(-0.0),
            Value::Float(1.5),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NAN),
            Value::Tuple(std::rc::Rc::new(vec![Value::Int(1), Value::Int(2)])),
        ];
        // Every digit count from 1 to 19, both signs, plus the neighbours of each power
        // of ten — where the pair loop's odd/even tail branch changes.
        let mut p: i64 = 1;
        for _ in 0..19 {
            for d in [-1i64, 0, 1] {
                cases.push(Value::Int(p + d));
                cases.push(Value::Int(-(p + d)));
            }
            p = p.saturating_mul(10);
        }
        // A deterministic sweep so the interior of each length is covered too.
        let mut s: u64 = 0x2545F4914F6CDD1D;
        for _ in 0..4000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            cases.push(Value::Int(s as i64));
            cases.push(Value::Int((s % 1_000_000) as i64));
        }
        for v in &cases {
            let mut buf = String::new();
            assert!(
                crate::value::write_value(&mut buf, v, 1, 1).is_ok(),
                "write_value failed on {v:?}"
            );
            assert_eq!(buf, format!("{v}"), "direct append != Display for {v:?}");
        }
        // Appending must never clobber what is already in the buffer, and the same
        // value written twice must produce the same bytes twice.
        let mut buf = "prefix:".to_string();
        assert!(crate::value::write_value(&mut buf, &Value::Int(i64::MIN), 1, 1).is_ok());
        assert!(crate::value::write_value(&mut buf, &Value::Int(7), 1, 1).is_ok());
        assert_eq!(buf, format!("prefix:{}7", i64::MIN));
    }

    /// The VM builds an interpolated string by reading its hole values IN PLACE off the
    /// value stack and truncating once at the end — it no longer `split_off`s them into
    /// a throwaway `Vec` per string. That leaves the operands on the stack across the
    /// fallible middle of the op, so what this pins is the unwind: a hole that raises,
    /// caught by `try`, must leave the stack exactly as deep as it was, and execution
    /// must carry on correctly afterwards. A leak or an over-truncation here corrupts
    /// every later operand, so the cases keep computing after the catch.
    #[test]
    fn an_interpolation_that_raises_midway_unwinds_the_stack_exactly() {
        for (src, want) in [
            // baseline: the shapes themselves, on both engines
            ("z = 3\n\"a{z}b\"", "a3b"),
            ("z = 3\n\"{z}{z}{z}\"", "333"),
            ("\"{1 + 1}\"", "2"),
            ("z = -5\n\"n={z}\"", "n=-5"),
            // a hole raises: the FIRST one, with nothing yet written
            ("d = 0\nr = try \"{1 // d}\"\nr.ok", "false"),
            // ...and a LATER one, with earlier holes already on the stack
            ("d = 0\nr = try \"x{1}y{2}z{1 // d}\"\nr.ok", "false"),
            // the value stack must be intact afterwards — this is the actual point
            ("d = 0\nr = try \"x{1}y{1 // d}\"\n2 + 3", "5"),
            ("d = 0\nr = try \"x{1}y{1 // d}\"\n[1, 2, 3].sum()", "6"),
            ("d = 0\nr = try \"{1 // d}\"\n\"ok{4 + 4}\"", "ok8"),
            // nested: the raise happens inside a call inside a hole
            ("fn bad(x) = x // 0\nd = 1\nr = try \"a{bad(d)}b\"\nr.ok", "false"),
            ("fn bad(x) = x // 0\nd = 1\nr = try \"a{bad(d)}b\"\n7 * 6", "42"),
            // an interpolation nested inside another interpolation's hole
            ("z = 2\n\"<{\"[{z}]\"}>\"", "<[2]>"),
            ("d = 0\nz = 2\nr = try \"<{\"[{z // d}]\"}>\"\nr.ok", "false"),
            // An interpolation NESTED in an expression that already has operands on the
            // stack: `base` is non-zero here, so consuming one slot too many silently
            // eats the neighbour. Each case reads a value stacked BEFORE the hole.
            ("x = 5\n[11, \"{x}\", 22][0]", "11"),
            ("x = 5\n[11, \"{x}\", 22][2]", "22"),
            ("x = 5\n[11, \"{x}\", 22].length()", "3"),
            ("fn g(a, b, c) = a * 100 + c\nx = 5\ng(1, \"{x}\", 3)", "103"),
            ("x = 5\na, b, c = (11, \"a{x}b\", 22)\na * 100 + c", "1122"),
            ("x = 5\n7 + \"{x}{x}\".length()", "9"),
            ("x = 5\n[[1, 2], [\"{x}\"], [3]].length()", "3"),
            ("x = 5\n{a: 11, b: \"{x}\", c: 22}.a", "11"),
            ("x = 5\n{a: 11, b: \"{x}\", c: 22}.c", "22"),
            // two interpolations side by side inside one enclosing expression
            ("x = 5\ny = 6\n[9, \"{x}\", \"{y}\", 8][3]", "8"),
            // ...and the same shapes with a hole that RAISES, caught, then the
            // neighbours still read correctly
            ("d = 0\nr = try [11, \"{1 // d}\", 22]\nr.ok", "false"),
            ("d = 0\nr = try [11, \"{1 // d}\", 22]\n[11, 22][1]", "22"),
            ("fn g(a, b, c) = a * 100 + c\nd = 0\nr = try g(1, \"{1 // d}\", 3)\n1 + 2", "3"),
            // a format-spec failure raises from the OTHER arm of the same op
            ("s = \"text\"\nr = try \"{s:.2f}\"\nr.ok", "false"),
            ("s = \"text\"\nr = try \"a{1}b{s:.2f}\"\n9 - 4", "5"),
            // and a spec that works still works
            ("x = 1.005\n\"{x:.2f}\"", "1.00"),
            // 50 catches in a row must not drift the stack a slot at a time: every one
            // of them raises, and the map that wraps them still returns 50 elements.
            (
                "d = 0\n(0..50).map(x => if (try \"{x}{1 // d}\").ok then 1 else 0).sum()",
                "0",
            ),
            ("d = 0\n(0..50).map(x => (try \"{x}{1 // d}\").ok).length()", "50"),
            // a caught raise, then real work, repeated — the stack must not grow
            (
                "d = 0\n(0..50).map(x => do {\n  r = try \"{x}{1 // d}\"\n  x * 2\n}).sum()",
                "2450",
            ),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }


        // A consumed operand that is never truncated sits BELOW the result, where no
        // later op reads it — the program still answers correctly while the stack grows
        // without bound. Only the DEPTH shows it, so compare against the same program
        // with the holes removed: 50 interpolations must leave the stack exactly as
        // deep as 50 constants do.
        let depth = |src: &str| -> usize {
            let toks = lexer::lex(src).expect("lex");
            let ast = parser::parse(toks).expect("parse");
            let prog = bytecode::compile_with_types(&ast, None).expect("compile");
            match exec(&prog, None) {
                Ok(stack) => stack.len(),
                Err(e) => panic!("`{src}` failed: {e:?}"),
            }
        };
        for (holes, plain) in [
            ("(0..50).map(x => \"{x}\").length()", "(0..50).map(x => \"c\").length()"),
            (
                "(0..50).map(x => \"a{x}b{x}c\").length()",
                "(0..50).map(x => \"abc\").length()",
            ),
            (
                "d = 0\n(0..50).map(x => (try \"{x}{1 // d}\").ok).length()",
                "d = 0\n(0..50).map(x => (try \"c\").ok).length()",
            ),
            (
                "x = 1\n[9, \"{x}\", \"{x}\", 8].length()",
                "x = 1\n[9, \"c\", \"c\", 8].length()",
            ),
        ] {
            assert_eq!(
                depth(holes),
                depth(plain),
                "interpolation leaked or over-consumed stack slots in `{holes}`"
            );
        }
    }

    /// The other early exit out of the middle of `Op::Interp` is the length cap, and it
    /// leaves the same operands on the stack as a raising hole does. IGNORED because
    /// `MAX_STRING_LEN` is 1 GB: tripping it costs gigabytes of memcpy and peak RSS per
    /// engine — the naive `s = "{s}{s}"` doubling version of this ran 220 SECONDS, which
    /// does not belong in a gate that has to stay fast enough to run constantly. The
    /// stack discipline it would check is already pinned by the raising-hole cases
    /// above, which take the identical `return Err` out of the identical place.
    ///
    ///     cargo test --profile gate --bin helix -- --ignored the_interpolation_length_cap
    #[test]
    #[ignore = "allocates ~2 GB per engine to reach the 1 GB cap"]
    fn the_interpolation_length_cap_fires_identically_on_both_engines() {
        // One doubling straight past the cap, rather than thirty from one byte.
        let half = 1usize << 29;
        let boom = format!("s = \"x\".repeat({half})\n\"{{s}}{{s}}{{s}}\".length()");
        let (tw, vm) = (run_tw(&boom), run_vm(&boom));
        assert_eq!(tw, vm, "engines disagree on the interpolation length cap");
        let msg = vm.unwrap_err();
        assert!(msg.contains("interpolated string exceeds"), "got: {msg}");
        // ...and caught, the program keeps running on a stack of the right depth.
        let caught = format!("s = \"x\".repeat({half})\nr = try \"{{s}}{{s}}{{s}}\"\nr.ok");
        assert_eq!(run_tw(&caught), run_vm(&caught), "engines disagree on the caught cap");
        assert_eq!(run_vm(&caught), Ok("false".to_string()));
    }

    /// `position`, and the `take_while`/`drop_while` built on it, SHORT-CIRCUIT. They used
    /// to desugar to `map(p).index_of(Bool(want))`, which ran the predicate over every
    /// element and materialized one `Value` per element to find an index that is usually
    /// near the front. Measured before the change, on a 90M range:
    ///
    ///     (0..90_000_000).take_while(it < 5)   24.33s   2.12 GB
    ///     (0..90_000_000).position(it > 5)      5.03s   2.12 GB
    ///     (0..90_000_000).any(it > 5)           0.07s     14 MB   <- already lazy
    ///
    /// After: 0.02s / 15.6 MB and 0.00s / 15.5 MB. Even when NOTHING can be skipped the
    /// intermediate array is gone — a full-scan `take_while` over 10M went 287 MB -> 17.7 MB.
    ///
    /// What this test pins is that the answers did not move. The arms reproduce
    /// `index_of`'s comparison exactly rather than approximately: `values_equal` is false
    /// for every non-`Bool` against a `Bool`, so a `missing` result — or an outright
    /// non-boolean one — is SKIPPED, neither a match nor an error. That is deliberately
    /// unlike `any`/`all`, which do reject a non-boolean test.
    #[test]
    fn position_and_the_while_verbs_short_circuit_without_moving_any_answer() {
        for (src, want) in [
            // ---- position
            ("[5, 6, 7].position(it > 5)", "1"),
            ("[5, 6, 7].position(it > 99)", "missing"),
            ("[5, 6, 7].position(it > 0)", "0"),
            ("[].position(it > 0)", "missing"),
            ("missing.position(it > 0)", "missing"),
            // a non-boolean or `missing` result never matches and never raises
            ("[5, 6, 7].position(it)", "missing"),
            ("[5, 6, 7].position(it * 2)", "missing"),
            ("[5, missing, 7].position(it > 6)", "2"),
            ("[missing].position(it > 0)", "missing"),
            // multi-parameter binders, and a named predicate as a bare identifier
            ("[[1, 2], [3, 4]].position((a, b) => b == 4)", "1"),
            ("[[1, 2], [3, 4]].position((a, b) => a > 9)", "missing"),
            // A BARE named predicate does not bind here, and did not before either:
            // `wrap_bound_fn_arg` only reaches the general method branch, not the
            // desugared verbs, so `big` is the implicit-`it` body — the function VALUE,
            // which is not `Bool(true)`, so nothing ever matches. Recorded as it is
            // rather than quietly fixed: making it bind is a separate change from making
            // it fast, and `map`/`any`/`all` accepting it is the inconsistency to settle.
            ("fn big(x) = x > 5\n[1, 9, 2].position(big)", "missing"),
            ("fn big(x) = x > 5\n[1, 9, 2].position(x => big(x))", "1"),
            // ---- take_while / drop_while
            ("[1, 2, 3, 9, 4].take_while(it < 5)", "[1, 2, 3]"),
            ("[1, 2, 3, 9, 4].drop_while(it < 5)", "[9, 4]"),
            ("[1, 2, 3].take_while(it < 99)", "[1, 2, 3]"),
            ("[1, 2, 3].drop_while(it < 99)", "[]"),
            ("[1, 2, 3].take_while(it < 0)", "[]"),
            ("[1, 2, 3].drop_while(it < 0)", "[1, 2, 3]"),
            ("[].take_while(it < 5)", "[]"),
            ("missing.take_while(it < 5)", "missing"),
            // a `missing` element's test is `missing`, which is not `false`, so the run
            // CONTINUES through it — the prefix keeps the hole
            ("[1, missing, 3].take_while(it < 2)", "[1, missing]"),
            ("[1, missing, 3].drop_while(it < 2)", "[3]"),
            ("[1, 2, 3].take_while(it)", "[1, 2, 3]"),
            ("[[1, 2], [3, 4]].take_while((a, b) => a < 3)", "[[1, 2]]"),
            // ---- lazy and stepped ranges, and composition with other verbs
            ("(0..10).position(it > 6)", "7"),
            ("(0..10).take_while(it < 4)", "[0, 1, 2, 3]"),
            ("(0..10).drop_while(it < 4)", "[4, 5, 6, 7, 8, 9]"),
            ("range(0, 20, 3).position(it > 8)", "3"),
            ("range(0, 20, 3).take_while(it < 10)", "[0, 3, 6, 9]"),
            ("(0..20).filter(it % 2 == 0).take_while(it < 9)", "[0, 2, 4, 6, 8]"),
            ("(0..20).take_while(it < 9).sum()", "36"),
            ("(0..20).drop_while(it < 9).first()", "9"),
            ("fn f(xs) = xs.take_while(it < 3)\nf([1, 2, 5])", "[1, 2]"),
            ("[[1, 2, 9], [4, 5]].map(r => r.take_while(it < 5))", "[[1, 2], [4]]"),
            ("[[1, 2, 9], [4, 5]].map(r => r.position(it > 3))", "[2, 0]"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }

        // SHORT-CIRCUITING IS OBSERVABLE, and this is the one intended behaviour change.
        // A predicate that raises on an element PAST the stopping point used to abort the
        // whole program, because the desugar evaluated it everywhere. It is now never
        // evaluated there. `any`/`all` have always behaved this way, so the four early-exit
        // verbs finally agree with each other.
        for (src, want) in [
            // 100 // 4 = 25, not > 50 -> the run ends at index 0; the `0` is never divided
            ("[4, 1, 0].take_while(100 // it > 50)", "[]"),
            // 100 // 1 = 100 > 90 -> index 1 wins; the `0` is never divided
            ("[4, 1, 0].position(100 // it > 90)", "1"),
            ("[4, 1, 0].drop_while(100 // it > 50)", "[4, 1, 0]"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // ...and a predicate that raises AT or BEFORE the stopping point still raises,
        // identically on both engines — short-circuiting must not swallow a real error.
        for src in ["[0, 1].take_while(100 // it > 50)", "[0, 1].position(100 // it > 50)"] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.unwrap_err();
            assert!(msg.contains("division by zero"), "`{src}` got: {msg}");
        }

        // Errors keep their wording, and a non-array receiver now names the verb the user
        // actually wrote — the old desugar reported `map`, which appears nowhere in the
        // source they typed.
        for (src, needle) in [
            ("[1, 2].position()", "takes one predicate function"),
            ("[1, 2].position(it > 0, it < 5)", "takes one predicate function"),
            ("[1, 2].take_while()", "takes one predicate function"),
            ("5.position(it > 0)", "type Int has no method `position`"),
            ("\"abc\".position(it)", "no method `position`"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.unwrap_err();
            assert!(msg.contains(needle), "`{src}` got: {msg}");
        }

        // Scale: 50M elements answered from the first handful. Under the materializing
        // desugar this allocated ~800 MB and took seconds; a regression shows up as the
        // gate suddenly needing both. The `.length()` on the result stays O(1) because a
        // lazy range's `take` is O(1).
        for (src, want) in [
            ("(0..50000000).take_while(it < 3).length()", "3"),
            ("(0..50000000).position(it > 2)", "3"),
            ("(0..50000000).drop_while(it < 3).first()", "3"),
            ("(0..50000000).any(it > 2)", "true"),
        ] {
            assert_eq!(run_vm(src), Ok(want.to_string()), "`{src}`");
        }
    }

    /// A tail-self-recursive function may READ GLOBALS and still compile to a native loop.
    ///
    /// `value_eligible`'s `Ident` arm admits only parameters, so one global read anywhere
    /// in the function — condition or body, it made no difference — dropped the entire
    /// loop to the bytecode VM. This is the same capture defect the map/filter/reduce
    /// kernels were fixed for, on the most fundamental construct in the language: a tail
    /// loop IS Helix's `while`. Measured at 10M iterations:
    ///
    ///     fn go(i, acc) = if i >= 10000000 then acc else go(i + 1, acc + 1)   0.01s
    ///     n = 10000000                                                        0.80s
    ///     fn go(i, acc) = if i >= n        then acc else go(i + 1, acc + 1)
    ///
    /// — an 80x penalty for naming the bound instead of inlining it. Now both are 0.01s.
    ///
    /// The specialization takes the captured globals as trailing `i64` parameters, which
    /// the VM reads AT DISPATCH. That is what makes it sound: nothing else runs during a
    /// native call, so a capture is loop-invariant for the whole loop, and a `mut` global
    /// reassigned between two calls is seen at its current value by each.
    #[test]
    fn a_tail_loop_may_read_globals_and_still_compile_to_a_native_loop() {
        // Every one of these must agree on all three engines. The interesting half is
        // what a capture can get WRONG: staleness, shadowing, and a non-Int global.
        for (src, want) in [
            ("n = 10\nfn go(i, a) = if i >= n then a else go(i + 1, a + i)\ngo(0, 0)", "45"),
            ("k = 3\nfn go(i, a) = if i >= 10 then a else go(i + 1, a + k)\ngo(0, 0)", "30"),
            ("a = 2\nb = 100\nfn go(i, c) = if i >= b then c else go(i + a, c + 1)\ngo(0, 0)", "50"),
            ("n = 7\nfn go(i, a) = if i >= n then a + n else go(i + 1, a + 1)\ngo(0, 0)", "14"),
            // a PARAMETER of the same name shadows the global — the parameter must win
            ("n = 999\nfn go(n, a) = if n <= 0 then a else go(n - 1, a + 1)\ngo(4, 0)", "4"),
            // a `let` inside the body shadows it for the rest of the body
            ("k = 100\nfn go(i, a) = if i >= 5 then a else do {\n  k = 2\n  go(i + 1, a + k)\n}\ngo(0, 0)", "10"),
            ("k = 100\nfn go(i, a) = if i >= 5 then a else do {\n  j = 2\n  go(i + 1, a + k + j)\n}\ngo(0, 0)", "510"),
            // a match arm's binder shadows it inside that arm only
            ("m = 50\nfn go(i, a) = if i >= 4 then a else go(i + 1, a + match i { 0 => 7, m => m })\ngo(0, 0)", "13"),
            // A global that is not an `Int` AT DISPATCH declines to the VM, which handles
            // it correctly as always. These must actually READ the global — an earlier
            // draft of this test used `if i >= 3` with an unread `n = 4.5`, which has no
            // captures at all, compiles by the ordinary path, and proves nothing.
            ("n = 4.5\nfn go(i, a) = if i >= n then a else go(i + 1, a + 1)\ngo(0, 0)", "5"),
            ("f = 2.0\nfn go(i, a) = if i >= 3 then a else go(i + 1, a + f)\ngo(0, 0.0)", "6.0"),
            // a `let` binding a name that is NOT a global must not become a capture
            ("n = 5\nfn go(i, a) = if i >= n then a else do {\n  j = i * 2\n  go(i + 1, a + j)\n}\ngo(0, 0)", "20"),
            // ...nor a match arm's binder
            ("n = 4\nfn go(i, a) = if i >= n then a else go(i + 1, a + match i { 0 => 7, q => q })\ngo(0, 0)", "13"),
            // a global holding a FUNCTION is not an i64 and must not be captured as one
            ("fn h(x) = x + 1\ng = h\nfn go(i, a) = if i >= 3 then a else go(i + 1, a + 1)\ngo(0, 0)", "3"),
            // arithmetic keeps the interpreter's exact wrapping
            ("k = 9223372036854775807\nfn go(i, a) = if i >= 3 then a else go(i + 1, a + k)\ngo(0, 0)", "9223372036854775805"),
            ("k = -5\nfn go(i, a) = if i >= 4 then a else go(i + 1, a + k)\ngo(0, 0)", "-20"),
            ("n = 0\nfn go(i, a) = if i >= n then a else go(i + 1, a + 1)\ngo(0, 0)", "0"),
            ("k = 3\nfn sq(x) = x * x\nfn go(i, a) = if i >= 5 then a else go(i + 1, a + sq(k))\ngo(0, 0)", "45"),
            ("k = 2\nfn go(i, a) = if i >= 4 then a else go(i + 1, a + match k { 2 => 10, _ => 0 })\ngo(0, 0)", "40"),
            ("a = 1\nb = 2\nfn go(i, c) = if i >= 6 then c else go(i + 1, if i % 2 == 0 then c + a else c + b)\ngo(0, 0)", "9"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{src}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }

        // A `mut` global reassigned BETWEEN calls: each call marshals the value that is
        // current when it dispatches, so the second call must see the new bound. A
        // specialization that baked the global in at compile time would print [5, 5].
        let src = "mut n = 5\nfn go(i, a) = if i >= n then a else go(i + 1, a + 1)\n\
                   x = go(0, 0)\nn = 9\ny = go(0, 0)\n[x, y]";
        assert_eq!(run_tw(src), run_vm_jit(src), "engines disagree on a reassigned capture");
        assert_eq!(run_vm_jit(src), Ok("[5, 9]".to_string()));

        // A raise inside the loop still raises, with the interpreter's exact text.
        let src = "z = 0\nfn go(i, a) = if i >= 3 then a else go(i + 1, a + 100 // z)\ngo(0, 0)";
        let (tw, jit) = (run_tw(src), run_vm_jit(src));
        assert_eq!(tw, jit, "engines disagree on a raising capture loop");
        assert!(jit.unwrap_err().contains("division by zero"));

        // ENGAGEMENT. Agreement is worthless if the JIT simply declined everything, so
        // assert a native call actually happened for the capture shape — and that the
        // loop runs natively at a depth that would blow the VM's own recursion guard if
        // it were not a real loop (frame reuse, no stack growth).
        crate::jit::reset_native_call_count();
        let deep = "n = 3000000\nfn go(i, a) = if i >= n then a else go(i + 1, a + 1)\ngo(0, 0)";
        assert_eq!(run_vm_jit(deep), Ok("3000000".to_string()));
        assert!(
            crate::jit::native_call_count() > 0,
            "the capture-taking tail loop never reached native code, so every case above \
             proves only that the VM still works"
        );

        // A loop whose body binds names LOCALLY must still compile: if `free_idents`
        // forgot a `let` or a match-arm binder, that name would be recorded as a capture,
        // the VM would fail to resolve it to a global, and the whole loop would silently
        // fall back to the interpreter — right answer, 80x slower. Only engagement can
        // see that, which is why these two are asserted here and not above. `do { }`
        // desugars to `let`, so this covers most real loop bodies.
        for src in [
            "n = 5\nfn go(i, a) = if i >= n then a else do {\n  j = i * 2\n  go(i + 1, a + j)\n}\ngo(0, 0)",
            "n = 4\nfn go(i, a) = if i >= n then a else go(i + 1, a + match i { 0 => 7, q => q })\ngo(0, 0)",
        ] {
            crate::jit::reset_native_call_count();
            assert!(run_vm_jit(src).is_ok(), "`{src}`");
            assert!(
                crate::jit::native_call_count() > 0,
                "a locally-bound name was mistaken for a captured global, so this loop \
                 fell back to the VM: `{src}`"
            );
        }

        // ...and the DECLINING shapes must NOT reach native code, or the "declines to the
        // VM" claim is untested. A Float global READ BY THE LOOP is the boundary case.
        crate::jit::reset_native_call_count();
        let float_cap = "n = 4.5\nfn go(i, a) = if i >= n then a else go(i + 1, a + 1)\ngo(0, 0)";
        assert_eq!(run_vm_jit(float_cap), Ok("5".to_string()));
        assert_eq!(
            crate::jit::native_call_count(),
            0,
            "a Float capture must decline at dispatch, not be marshalled as an i64"
        );

        // A `missing` capture reaches the interpreter's exact error, not a marshalled 0.
        let miss = "n = missing\nfn go(i, a) = if i >= n then a else go(i + 1, a + 1)\ngo(0, 0)";
        let (tw, jit) = (run_tw(miss), run_vm_jit(miss));
        assert_eq!(tw, jit, "engines disagree on a `missing` capture");
        assert!(jit.unwrap_err().contains("`if` condition is `missing`"));
    }

    /// `concat` over PACKED numeric arrays skips the general path's boxing. That path
    /// costs three passes per call — `to_values()` boxes the receiver into a `Vec<Value>`,
    /// `to_vec()` clones it, and `array_sniff` unboxes the result back to packed — moving
    /// 16 bytes per element twice to append to a buffer of 8-byte elements.
    ///
    /// Measured with `fn build(i, acc) = if i >= n then acc else build(i+1, acc.concat([i*i]))`:
    ///
    ///     n = 20_000    1.83s -> 0.05s
    ///     n = 40_000   14.18s -> 0.13s
    ///     n = 80_000   83.94s -> 5.10s
    ///
    /// STILL O(n^2) — the receiver is copied on every call, and what remains is exactly
    /// memcpy bandwidth (the 40k -> 80k jump is the L2 cliff at 640 KB a copy). An O(1)
    /// append needs last-use liveness so the final read of a binding MOVES instead of
    /// cloning, leaving the `Rc` unique enough to extend in place. Recorded in
    /// docs/ROADMAP.md as the prerequisite for `while`-style syntax.
    ///
    /// What is pinned here is that the fast path answers exactly what the general path
    /// answered — including every mix that must FALL THROUGH to it.
    #[test]
    fn packed_concat_answers_exactly_what_the_boxing_path_answered() {
        for (src, want) in [
            // both packed: the fast path
            ("[1, 2].concat([3])", "[1, 2, 3]"),
            ("[1, 2].concat([3], [4, 5])", "[1, 2, 3, 4, 5]"),
            ("[1.0, 2.0].concat([3.0])", "[1.0, 2.0, 3.0]"),
            // a lazy range on either side materializes to the same integers
            ("(0..3).concat([9])", "[0, 1, 2, 9]"),
            ("[9].concat((0..3))", "[9, 0, 1, 2]"),
            ("range(0, 9, 3).concat([7])", "[0, 3, 6, 7]"),
            ("(0..3).concat((0..2))", "[0, 1, 2, 0, 1]"),
            // MIXED kinds must fall through — the general path keeps `1` an Int and
            // `2.0` a Float rather than promoting either
            ("[1].concat([2.0])", "[1, 2.0]"),
            ("[1.0].concat([2])", "[1.0, 2]"),
            // empty, missing, strings, nesting: all the general path
            ("[].concat([1])", "[1]"),
            ("[1].concat([])", "[1]"),
            ("[1].concat()", "[1]"),
            ("[1, missing].concat([2])", "[1, missing, 2]"),
            ("[\"a\"].concat([\"b\"])", "[\"a\", \"b\"]"),
            ("[[1], [2]].concat([[3]])", "[[1], [2], [3]]"),
            // the result stays packed, so numeric verbs still take their fast path
            ("[1].concat([2]).sum()", "3"),
            ("[1.5].concat([2.5]).mean()", "2.0"),
            ("(0..1000).concat((0..1000)).length()", "2000"),
            ("(0..1000).concat([5]).sum()", "499505"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // A non-array argument keeps the general path's exact wording — the fast path
        // must decline rather than reproduce the message.
        for src in ["[1].concat(5)", "[1].concat(\"x\")", "[1.0].concat(2.0)"] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.unwrap_err();
            assert!(msg.contains("`concat` expects arrays"), "`{src}` got: {msg}");
        }
    }

    /// `dot` preserves int-ness — the rule `sum`, `cumsum` and `product` already follow,
    /// and the one `dot` was breaking. It widened to `f64` unconditionally, which made it
    /// the only integer reduction in the language that could return a WRONG ANSWER:
    ///
    ///     xs = (0..1000000)
    ///     xs.dot(xs)                                333332833333127552.0   <- was
    ///     xs.map(it * it).sum()                     333332833333500000
    ///     xs.zip(xs).map((a, b) => a * b).sum()     333332833333500000
    ///     exact                                     333332833333500000
    ///
    /// Off by 372,448, silently, because an f64 cannot hold integers past 2^53. Found by
    /// comparing a program against its own equivalent spelling — the standing method.
    #[test]
    fn dot_stays_exact_on_integers_and_agrees_with_its_equivalent_spellings() {
        for (src, want) in [
            // an all-Int dot is an Int, and equals the spellings it is sugar for
            ("[1, 2, 3].dot([4, 5, 6])", "32"),
            ("[1, 2, 3].zip([4, 5, 6]).map((a, b) => a * b).sum()", "32"),
            ("[-2, 3].dot([4, -5])", "-23"),
            ("[].dot([])", "0"),
            ("[7].dot([0])", "0"),
            // exact past 2^53, where the old f64 path drifted
            ("(0..1000000).dot((0..1000000))", "333332833333500000"),
            ("(0..1000000).map(it * it).sum()", "333332833333500000"),
            ("[94906266, 94906266].dot([94906266, 94906266])", "18014398652125512"),
            // a Float on EITHER side keeps the float result, unchanged
            ("[1.0, 2.0].dot([3.0, 4.0])", "11.0"),
            ("[1, 2].dot([3.0, 4.0])", "11.0"),
            ("[1.0, 2.0].dot([3, 4])", "11.0"),
            // `missing` still propagates from either side
            ("[1, missing].dot([1, 2])", "missing"),
            ("[1, 2].dot([1, missing])", "missing"),
            ("missing.dot([1, 2])", "missing"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }

        // A product that cannot be summed inside an i128 falls back to the SAME f64
        // expression as before, so those inputs answer exactly what they used to — and
        // still match the spelling `dot` is sugar for.
        for src in [
            "a = [3037000499, 3037000499]\na.dot(a)",
            "a = [3037000499, 3037000499]\na.zip(a).map((x, y) => x * y).sum()",
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok("18446744061852497920.0".to_string()), "`{src}`");
        }

        // Errors keep their exact wording AND their precedence: a non-numeric element is
        // reported before a length mismatch, because the widening still runs first.
        for (src, needle) in [
            ("[1, 2].dot([1])", "equal-length"),
            ("[1, \"a\"].dot([1, 2])", "dot"),
            ("[1].dot(5)", "expects an array"),
            ("[1].dot()", "takes one array argument"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert!(vm.unwrap_err().contains(needle), "`{src}`");
        }
    }

    /// `xs.clamp(lo, hi)` with `lo > hi` used to ABORT THE PROCESS — SIGABRT, exit 134,
    /// core dumped — and `try` could not catch it:
    ///
    ///     print([1, 2, 3].clamp(5, 1))
    ///     error: internal error (src/interp/methods.rs:1440): min > max. min = 5, max = 1
    ///     Aborted (core dumped)
    ///
    /// `Ord::clamp` and `f64::clamp` both PANIC when `min > max`, and the array method
    /// called them without checking. ADR 0024 says user input must never take the host
    /// down, so this was the most severe class of defect the language can have: not a wrong
    /// answer but a dead process, from three characters typed in the wrong order.
    ///
    /// The SCALAR `clamp(x, lo, hi)` builtin always had the guard. So the identical mistake
    /// was a clean catchable error one way and fatal the other — which is also why this
    /// survived: nobody writes the array form by accident in a test.
    #[test]
    fn clamp_with_reversed_bounds_raises_instead_of_aborting_the_host() {
        // The crash case, on every engine, with the scalar's exact wording.
        for src in [
            "[1, 2, 3].clamp(5, 1)",
            "[1.0, 2.0].clamp(5.0, 1.0)",
            "[1, 2, 3].clamp(-1, -5)",
            "(0..10).clamp(9, 2)",
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.unwrap_err();
            assert!(msg.contains("`clamp` needs lo <= hi"), "`{src}` got: {msg}");
        }
        // ...and it is CATCHABLE, which is the whole point — an abort is not.
        assert_eq!(run_vm("(try [1, 2, 3].clamp(5, 1)).ok"), Ok("false".to_string()));
        assert_eq!(run_vm("r = try [1, 2, 3].clamp(5, 1)\n1 + 1"), Ok("2".to_string()));

        // Ordinary clamping is unchanged.
        for (src, want) in [
            ("[1, 2, 3].clamp(1, 2)", "[1, 2, 2]"),
            ("[-1, 5, 2, 9].clamp(0, 4)", "[0, 4, 2, 4]"),
            ("[1.0, 5.0].clamp(2.0, 3.0)", "[2.0, 3.0]"),
            ("[1, 2, 3].clamp(2, 2)", "[2, 2, 2]"),
            ("[].clamp(0, 1)", "[]"),
            ("[1, missing].clamp(0, 5)", "missing"),
            ("clamp(3, 1, 5)", "3"),
            // a NaN bound cannot be caught by `lo > hi` (every NaN comparison is false), so
            // the selection is written as comparisons rather than `.clamp()`, which panics
            // on a NaN bound too. Nothing matches, so elements pass through.
            ("[1, 2, 3].clamp(sqrt(-1.0), 5.0)", "[1, 2, 3]"),
            ("[1.0, 2.0].clamp(0.0, sqrt(-1.0))", "[1.0, 2.0]"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
    }

    /// `unique`/`frequencies`/`top` over FLOAT arrays were still the O(n × distinct) scan —
    /// the defect the integer path was rescued from, left on the element type scientific
    /// data actually uses. Stringifying every float and hashing the TEXT was 220x faster
    /// than asking the float array directly; measured end to end, 2.91s -> 0.02s (145x) for
    /// `(0..60000).map(to_float(it)).unique()`.
    ///
    /// Floats were excluded on the first pass for two real reasons, and this is how each is
    /// handled rather than dodged:
    ///
    /// * `-0.0 == 0.0` is TRUE but their bits differ, so zero is canonicalized before
    ///   hashing and the first of the pair seen stays the representative.
    /// * **NaN is equal to nothing, not even itself.** It therefore belongs to no
    ///   equivalence class and gets NO key at all: a fresh bucket every time, never entering
    ///   the table. That is exactly what the `values_equal` scan produced, since `NaN == NaN`
    ///   is false there too — so `[nan, nan].unique()` has TWO elements, before and after.
    #[test]
    fn float_histograms_hash_without_moving_a_single_answer() {
        for (src, want) in [
            ("[1.0, 1.0, 2.0].frequencies()", "[(1.0, 2), (2.0, 1)]"),
            ("[1.0, 2.0, 1.0].unique()", "[1.0, 2.0]"),
            ("[2.5].frequencies()", "[(2.5, 1)]"),
            ("[].frequencies()", "[]"),
            // -0.0 and 0.0 are ONE identity, and the first seen represents it
            // `-0.0` is POSITIVE zero in IEEE, so writing a negative zero that way
            // tests nothing. Unary negation is what produces one — an earlier draft used
            // the subtraction and was therefore not exercising -0.0 at all.
            ("nz = -0.0
[0.0, nz].unique().count()", "1"),
            ("nz = -0.0
[0.0, nz].frequencies()", "[(0.0, 2)]"),
            ("nz = -0.0
[nz, 0.0].frequencies()", "[(-0.0, 2)]"),
            // NaN is equal to nothing, so every NaN is its own bucket
            ("[sqrt(-1.0), sqrt(-1.0)].unique().count()", "2"),
            ("[sqrt(-1.0), sqrt(-1.0)].frequencies().count()", "2"),
            ("[1.0, sqrt(-1.0), 1.0].frequencies().count()", "2"),
            // `missing` is one identity and is never a float
            ("[1.0, missing, 1.0, missing].frequencies()", "[(1.0, 2), (missing, 2)]"),
            ("[missing, 1.0].unique()", "[missing, 1.0]"),
            // infinities are ordinary keys
            // float  by zero RAISES here (division is total), so an infinity is built
            // by overflow instead
            ("big = 1.0e308 * 10.0
[big, big].frequencies().count()", "1"),
            ("big = 1.0e308 * 10.0
[big, -big].unique().count()", "2"),
            // MIXED Int/Float still falls through to the scan: `values_equal` collapses
            // 1 == 1.0, and above 2^53 that collapse is not even transitive, so no hash
            // key can reproduce it
            ("[1, 1.0].unique().count()", "1"),
            ("[1, 1.0].frequencies()", "[(1, 2)]"),
            ("[1.0, 1].frequencies()", "[(1.0, 2)]"),
            // `top` shares the histogram
            ("[1.0, 1.0, 2.0].top(1)", "[(1.0, 2)]"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }

        // `unique` and `frequencies` choose their keys independently, so they must be shown
        // to report the SAME identity count — that is what catches one being fixed and not
        // the other, NaN included.
        for src in [
            "[1.0, 1.0, 2.0]",
            "[0.0, -0.0, 1.0]",
            "[sqrt(-1.0), sqrt(-1.0), 1.0]",
            "[1.0, missing, missing]",
            "[1, 1.0, 2]",
            "[]",
        ] {
            let a = format!("{src}.unique().count()");
            let b = format!("{src}.frequencies().count()");
            assert_eq!(run_vm(&a), run_vm(&b), "unique/frequencies disagree on `{src}`");
            assert_eq!(run_tw(&a), run_vm(&a), "engines disagree on `{a}`");
        }

        // Scale: 60k distinct floats. Under the scan this was ~1.8e9 `values_equal` calls
        // and took ~3 seconds; a regression turns this test from instant back into that.
        let f = "(0..60000).map(to_float(it))";
        assert_eq!(run_vm(&format!("{f}.unique().count()")), Ok("60000".to_string()));
        assert_eq!(run_vm(&format!("{f}.frequencies().count()")), Ok("60000".to_string()));
        assert_eq!(
            run_vm(&format!("{f}.map(it * 0.0).unique().count()")),
            Ok("1".to_string()),
            "60k floats that are all +0.0 collapse to one identity"
        );
    }

    /// `-9223372036854775808` is `i64::MIN`, an Int — not a Float that merely prints like
    /// one. It is the single integer that cannot be written positively (its magnitude is
    /// one larger than `i64::MAX`), so the lexer degraded the literal to `f64` and the
    /// negation then applied to a float:
    ///
    ///     print(-9223372036854775808)      -9223372036854775808.0   <- was a Float
    ///     print(-9223372036854775807 - 1)  -9223372036854775808     <- the workaround
    ///
    /// Silent, because it prints almost right — the same shape as the `dot` defect fixed
    /// earlier today. Unary minus now folds into a bare literal, which is what the PATTERN
    /// parser already did (`Pattern::Int(-v)`); expressions had simply never been taught.
    #[test]
    fn negating_a_literal_folds_and_i64_min_stays_an_integer() {
        for (src, want) in [
            // the value that could not previously be written at all
            ("-9223372036854775808", "-9223372036854775808"),
            ("-9223372036854775808 + 1", "-9223372036854775807"),
            ("-9223372036854775808 // 2", "-4611686018427387904"),
            // the old workaround still means the same thing
            ("-9223372036854775807 - 1", "-9223372036854775808"),
            ("-9223372036854775808 == -9223372036854775807 - 1", "true"),
            // an EXPLICIT float keeps its type — the fold reads the digits, not the value
            ("-9223372036854775808.0", "-9223372036854775808.0"),
            // ...and a magnitude PAST i64::MIN is still a Float, even though it rounds to
            // the same f64. Deciding from the `f64` would have accepted this one.
            ("-9223372036854775809", "-9223372036854775808.0"),
            // a positive over-large literal is unchanged
            ("9223372036854775808", "9223372036854775808.0"),
            // ordinary negatives
            ("-1", "-1"),
            ("-1.5", "-1.5"),
            ("-0.0", "-0.0"),
            ("3 - -1", "4"),
            ("x = 3\n-x", "-3"),
            ("[-1, -2].sum()", "-3"),
            // POSTFIX BINDS TIGHTER than unary minus, so the fold must not swallow it:
            // this is `-([1,2,3].sum())`, not `(-[1,2,3]).sum()`.
            ("-[1, 2, 3].sum()", "-6"),
            ("-[1, 2, 3][0]", "-1"),
            // the pattern form, which already folded, still agrees
            ("match -9223372036854775808 { -9223372036854775808 => 1, _ => 0 }", "1"),
            ("match -1 { -1 => 7, _ => 0 }", "7"),
            ("match -1.5 { -1.5 => 7, _ => 0 }", "7"),
            // i64::MIN's own arithmetic edges keep the semantics documented in
            // docs/integer-semantics.md
            ("-9223372036854775808 - 1", "9223372036854775807"),
            ("to_int(-1.0e30)", "-9223372036854775808"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // The type change is the point, so assert the TYPE and not just the rendering:
        // a Float would print with a trailing `.0`, and `is_missing`-style probes cannot
        // see it, but integer division can.
        // (`%` accepts floats — `-9223372036854775808.0 % 2` is `-0.0` — so it cannot tell
        // the two apart. Bitwise `&` requires integers and can.)
        assert_eq!(run_vm("-9223372036854775808 & 1"), Ok("0".to_string()));
        let float_form = run_vm("-9223372036854775808.0 & 1").unwrap_err();
        assert!(
            // the type checker says "bitwise operator `&` needs integers, but got a Float"
            // and the runtime says "`&` needs two integers"; this harness reaches the
            // second because it does not run the checker, so match what both contain
            float_form.contains("integers"),
            "the explicit-float form must still be a Float; got: {float_form}"
        );
    }

    /// A NON-LITERAL right operand for `%`, `//`, `<<`, `>>` now compiles in the mixed
    /// path. It used to decline the WHOLE enclosing kernel, so naming a modulus — the most
    /// ordinary refactor there is — cost 17-110x:
    ///
    ///     fn f(i, n, m, acc: Float) = ... acc + to_float(i % m)     2.83s  <- was
    ///     fn f(i, n, m, acc: Float) = ... acc + to_float(i % 7)     0.05s
    ///
    /// After: 0.05s, a 57x speedup, with the literal spelling unchanged.
    ///
    /// TWO THINGS MAKE THIS DELICATE, and both are pinned below.
    ///
    /// `%` and `//` are EUCLIDEAN in Helix, not truncating — `7 % -3` is 1 and `-7 // 3`
    /// is -3 — and the old lowering was correct ONLY because the gate guaranteed a
    /// positive constant: it added the divisor back (`rem_euclid` only for `d > 0`) and
    /// subtracted one from the quotient (floor only for `d > 0`).
    ///
    /// And native `srem`/`sdiv` TRAP rather than misbehave on two inputs: a zero divisor,
    /// which the interpreter RAISES on, and `(i64::MIN, -1)`, which the interpreter does
    /// NOT raise on — it wraps. Both bail to the poison block so the VM re-runs and
    /// produces the exact behaviour. A shift count outside `0..=63` bails likewise.
    #[test]
    fn a_non_literal_modulus_divisor_or_shift_compiles_and_keeps_every_edge() {
        // A `Float` parameter forces the MIXED path, which is the one this stage changed.
        // Each helper takes its right operand as a PARAMETER, so none of them is a literal.
        let h = "\
fn md(i: Int, d: Int, acc: Float) = if i >= 1 then acc else md(i + 1, d, acc + to_float(7 % d))\n\
fn mn(i: Int, d: Int, acc: Float) = if i >= 1 then acc else mn(i + 1, d, acc + to_float(0 - 7 % d))\n\
fn dv(i: Int, d: Int, acc: Float) = if i >= 1 then acc else dv(i + 1, d, acc + to_float(7 // d))\n\
fn dn(i: Int, d: Int, acc: Float) = if i >= 1 then acc else dn(i + 1, d, acc + to_float((0 - 7) // d))\n\
fn sl(i: Int, k: Int, acc: Float) = if i >= 1 then acc else sl(i + 1, k, acc + to_float(1 << k))\n\
fn sr(i: Int, k: Int, acc: Float) = if i >= 1 then acc else sr(i + 1, k, acc + to_float(256 >> k))\n\
fn sg(i: Int, k: Int, acc: Float) = if i >= 1 then acc else sg(i + 1, k, acc + to_float(-256 >> k))\n\
fn nn(i: Int, d: Int, acc: Float) = if i >= 1 then acc else nn(i + 1, d, acc + to_float(-7 % d))\n\
fn nd(i: Int, d: Int, acc: Float) = if i >= 1 then acc else nd(i + 1, d, acc + to_float(-7 // d))\n\
fn mm(i: Int, d: Int, acc: Float) = if i >= 1 then acc else mm(i + 1, d, acc + to_float(-9223372036854775808 % d))\n\
fn dd(i: Int, d: Int, acc: Float) = if i >= 1 then acc else dd(i + 1, d, acc + to_float(-9223372036854775808 // d))\n";

        for (expr, want) in [
            // EUCLIDEAN on both signs of the divisor and of the dividend
            ("md(0, 3, 0.0)", "1.0"),
            ("md(0, -3, 0.0)", "1.0"),
            ("dv(0, 3, 0.0)", "2.0"),
            ("dv(0, -3, 0.0)", "-2.0"),
            ("dn(0, 3, 0.0)", "-3.0"),
            ("dn(0, -3, 0.0)", "3.0"),
            // A NEGATIVE dividend is the only shape where the remainder comes out
            // negative and the divisor therefore has to be added back as its MAGNITUDE.
            // Without these the magnitude fix is untested — the sabotage battery caught
            // exactly that: replacing `abs(d)` with `d` still passed.
            ("nn(0, -3, 0.0)", "2.0"),
            ("nn(0, 3, 0.0)", "2.0"),
            ("nd(0, -3, 0.0)", "3.0"),
            ("nd(0, 3, 0.0)", "-3.0"),
            // `>>` is ARITHMETIC (sign-extending). A positive dividend cannot tell an
            // arithmetic shift from a logical one, so only a negative one pins it.
            ("sg(0, 2, 0.0)", "-64.0"),
            ("sg(0, 0, 0.0)", "-256.0"),
            ("sg(0, 63, 0.0)", "-1.0"),
            ("md(0, 1, 0.0)", "0.0"),
            ("dv(0, 1, 0.0)", "7.0"),
            // shifts in range
            ("sl(0, 3, 0.0)", "8.0"),
            ("sl(0, 0, 0.0)", "1.0"),
            ("sl(0, 63, 0.0)", "-9223372036854775808.0"),
            ("sr(0, 4, 0.0)", "16.0"),
            ("sr(0, 63, 0.0)", "0.0"),
            // `i64::MIN` with -1 does NOT raise — it WRAPS — even though the native
            // instruction would trap, so this is the bail proving it defers to the VM
            ("(try (mm(0, -1, 0.0))).ok", "true"),
            ("(try (dd(0, -1, 0.0))).ok", "true"),
            ("mm(0, -1, 0.0)", "0.0"),
            // a zero divisor RAISES, and a shift out of range on either side raises
            ("(try (md(0, 0, 0.0))).ok", "false"),
            ("(try (dv(0, 0, 0.0))).ok", "false"),
            ("(try (sl(0, 64, 0.0))).ok", "false"),
            ("(try (sl(0, -1, 0.0))).ok", "false"),
            ("(try (sr(0, 64, 0.0))).ok", "false"),
        ] {
            let src = format!("{h}{expr}");
            let (tw, vm, jit) = (run_tw(&src), run_vm(&src), run_vm_jit(&src));
            assert_eq!(tw, vm, "tree-walker and VM disagree on `{expr}`");
            assert_eq!(vm, jit, "VM and JIT disagree on `{expr}`");
            assert_eq!(vm, Ok(want.to_string()), "`{expr}`");
        }

        // The raising cases must carry the interpreter's exact wording, not a generic one.
        for (expr, needle) in [
            ("md(0, 0, 0.0)", "modulo by zero"),
            ("dv(0, 0, 0.0)", "division by zero"),
            ("sl(0, 64, 0.0)", "shift amount"),
        ] {
            let src = format!("{h}{expr}");
            let (tw, jit) = (run_tw(&src), run_vm_jit(&src));
            assert_eq!(tw, jit, "engines disagree on `{expr}`");
            let msg = jit.unwrap_err();
            assert!(msg.contains(needle), "`{expr}` got: {msg}");
        }

        // ENGAGEMENT. Every case above would also pass if the JIT had simply declined
        // them all, so assert a native call actually happened for a VARIABLE right
        // operand — that is the whole point of the stage.
        crate::jit::reset_native_call_count();
        let src = format!("{h}md(0, 3, 0.0)");
        assert_eq!(run_vm_jit(&src), Ok("1.0".to_string()));
        assert!(
            crate::jit::native_call_count() > 0,
            "a variable modulus never reached native code, so the cases above prove only \
             that the VM still works"
        );
    }

    /// Error text gets the indefinite article right, and a second `...spread` says what is
    /// actually wrong. Both came out of reading real output rather than tests:
    ///
    ///     print((r.x)(1))       `x` is a Int, not a function        <- was
    ///     print({...a, ...b})   expected a name as a record field name, found `...`
    ///
    /// The first reads as unfinished, and `Int` and `Array` are the two type names that hit
    /// it — which is most type errors in practice. The second describes the TOKEN rather
    /// than the problem: a record update has one base, so two spreads have no meaning, and
    /// the old message sent the reader looking for a missing field name.
    #[test]
    fn error_text_reads_as_english_and_names_the_real_problem() {
        // `with_article` is the whole of the first fix; check the boundary directly rather
        // than only through one message.
        for (t, want) in [
            ("Int", "an Int"),
            ("Array", "an Array"),
            ("Float", "a Float"),
            ("String", "a String"),
            ("Dict", "a Dict"),
            ("Unit", "a Unit"),
            ("", "a "),
        ] {
            assert_eq!(crate::value::with_article(t), want.to_string());
        }

        for (src, needle) in [
            // the case that exposed it
            ("r = {x: 5}\n(r.x)(1)", "is an Int, not a function"),
            ("f = 1.5\nf(1)", "is a Float, not a function"),
            // a second spread names the real problem
            ("a = {x: 1}\nb = {y: 2}\n{...a, ...b}", "takes one `...spread`, not two"),
            // ...and a MISPLACED spread keeps its own, different message: these are two
            // distinct mistakes and the reader should be told which one they made
            ("a = {x: 1}\n{q: 1, ...a}", "must be the first element"),
            // the spread base still has to be a record
            ("d = [(\"a\", 1)].to_dict()\n{...d, x: 1}", "needs a record"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.unwrap_err();
            assert!(msg.contains(needle), "`{src}` got: {msg}");
        }

        // No message should say "a Int" or "a Array" again. This is the regression guard:
        // the fix is mechanical across ~39 sites, so a new one is easy to add by hand.
        for src in [
            "r = {x: 5}\n(r.x)(1)",
            "[1, 2].filter(it)",
            "\"s\" * 2",
        ] {
            if let Err(msg) = run_vm(src) {
                assert!(!msg.contains("a Int"), "ungrammatical article in: {msg}");
                assert!(!msg.contains("a Array"), "ungrammatical article in: {msg}");
            }
        }
    }

    /// A duplicate field is rejected in a record UPDATE, not only in a plain literal.
    /// `{y: 2, y: 3}` was a parse error while `{...b, y: 2, y: 3}` was silently accepted
    /// with last-wins — the same mistake caught in one spelling and not the other, which is
    /// the shape of nearly every defect found in this codebase.
    ///
    /// ADR 0001 wants one entry per key because order-independent equality assumes it: two
    /// "equal" records could otherwise disagree on `.a`. The update branch simply never got
    /// the check the literal branch has.
    #[test]
    fn a_duplicate_field_is_rejected_in_a_record_update_too() {
        for (src, needle) in [
            ("{y: 2, y: 3}", "duplicate field `y` in record literal"),
            ("b = {x: 1}\n{...b, y: 2, y: 3}", "duplicate field `y` in record update"),
            ("b = {x: 1}\n{...b, y: 2, z: 3, y: 4}", "duplicate field `y` in record update"),
            ("b = {x: 1}\n{...b, a: 1, a: 2, a: 3}", "duplicate field `a` in record update"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.unwrap_err();
            assert!(msg.contains(needle), "`{src}` got: {msg}");
        }

        // OVERRIDING A BASE FIELD IS THE POINT OF AN UPDATE and must stay legal — only a
        // repeat within the update's own field list is a duplicate.
        for (src, want) in [
            ("b = {y: 1}\n{...b, y: 9}", "{y: 9}"),
            ("b = {x: 1, y: 1}\n{...b, y: 9}", "{x: 1, y: 9}"),
            ("b = {x: 1}\n{...b, y: 2, z: 3}", "{x: 1, y: 2, z: 3}"),
            ("b = {x: 1}\n{...b}", "{x: 1}"),
            ("b = {x: 1}\n{...b,}", "{x: 1}"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
    }

    /// `take`/`drop` re-slice a PACKED numeric array instead of boxing the whole source.
    ///
    /// A lazy `Range` already had this — added when `range(100000000).take(1)` was found
    /// materializing ~1.6 GB to keep one element. But a range that has been through a `map`
    /// is `Ints`, not `Range`, and that spelling kept boxing:
    ///
    ///     xs = (0..20000000).map(it * 2)
    ///     xs.take(3).sum()      503 MB  ->  190 MB
    ///     xs[0]                 190 MB              (the array alone, for scale)
    ///
    /// 320 MB of `Vec<Value>` to keep three numbers. One defect, fixed for one
    /// representation and not its neighbour — the same shape as `clamp` (array vs scalar),
    /// `dot` (vs `sum`/`cumsum`), and duplicate fields (literal vs update).
    #[test]
    fn take_and_drop_reslice_a_packed_array_without_boxing_it() {
        for (src, want) in [
            // Int arrays, including both clamps
            ("[1, 2, 3, 4, 5].take(0)", "[]"),
            ("[1, 2, 3, 4, 5].take(2)", "[1, 2]"),
            ("[1, 2, 3, 4, 5].take(5)", "[1, 2, 3, 4, 5]"),
            ("[1, 2, 3, 4, 5].take(99)", "[1, 2, 3, 4, 5]"),
            ("[1, 2, 3, 4, 5].take(-1)", "[]"),
            ("[1, 2, 3, 4, 5].drop(0)", "[1, 2, 3, 4, 5]"),
            ("[1, 2, 3, 4, 5].drop(2)", "[3, 4, 5]"),
            ("[1, 2, 3, 4, 5].drop(5)", "[]"),
            ("[1, 2, 3, 4, 5].drop(99)", "[]"),
            ("[1, 2, 3, 4, 5].drop(-1)", "[1, 2, 3, 4, 5]"),
            // Floats take the same path
            ("[1.5, 2.5, 3.5].take(2)", "[1.5, 2.5]"),
            ("[1.5, 2.5, 3.5].drop(2)", "[3.5]"),
            // heterogeneous arrays are NOT packed and must keep the general path
            ("[1, \"a\", true].take(2)", "[1, \"a\"]"),
            ("[1, \"a\", true].drop(2)", "[true]"),
            // empty, and the lazy-range arm that already existed
            ("[].take(3)", "[]"),
            ("[].drop(3)", "[]"),
            ("(0..5).take(2)", "[0, 1]"),
            ("(0..5).drop(2)", "[2, 3, 4]"),
            ("range(0, 20, 3).take(2)", "[0, 3]"),
            // the result must still be PACKED, or the numeric verbs lose their fast path
            ("[1, 2, 3, 4, 5].take(2).sum()", "3"),
            ("[1, 2, 3, 4, 5].drop(2).mean()", "4.0"),
            ("[1.5, 2.5, 3.5].take(2).sum()", "4.0"),
            // chained
            ("[1, 2, 3, 4, 5].drop(1).take(2)", "[2, 3]"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // A non-Int count defers to the general path so its errors are unchanged.
        for src in ["[1, 2].take(1.5)", "[1, 2].drop(1.5)", "[1, 2].take(\"x\")"] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert!(vm.is_err(), "`{src}` should error");
        }
        // `missing` propagates rather than erroring (ADR 0001), as before.
        assert_eq!(run_vm("[1, 2].take(missing)"), run_tw("[1, 2].take(missing)"));
    }

    /// `contains(v)`/`index_of(v)` scan a PACKED array in place instead of boxing it.
    ///
    /// Both answer a scalar, yet both materialized the whole source to do it:
    ///
    ///     xs = (0..20000000).map(it * 2)
    ///     xs.contains(4)        492 MB  ->  186 MB
    ///     xs.any(it == 4)       186 MB              (the streaming neighbour, for scale)
    ///
    /// 306 MB of `Vec<Value>` to settle a question decided by element 2, while the
    /// closure-taking spellings `any(p)`/`position(p)` already cost nothing extra. One
    /// operation, two spellings, only one of them fixed — as with `take`/`drop` (packed vs
    /// lazy `Range`), `clamp`, `dot`, and duplicate record fields.
    ///
    /// The scan reuses `values_equal` on the same `Value` the general path would have
    /// built, so the cases below are really asking whether that reuse held: cross-type
    /// equality, `missing` identity, and IEEE (NaN equal to nothing, ±0.0 equal to each
    /// other). Written against `inf`/`inf - inf` because `0.0 / 0.0` is a division-by-zero
    /// *error* in Helix — an earlier draft used it and tested only the error path.
    ///
    /// The JIT is not covered here on purpose: it has no `contains`/`index_of` of its own,
    /// so all three engines reach this one implementation.
    #[test]
    fn contains_and_index_of_scan_a_packed_array_without_boxing_it() {
        for (src, want) in [
            // packed Ints
            ("[1, 2, 3].contains(2)", "true"),
            ("[1, 2, 3].contains(9)", "false"),
            ("[1, 2, 3].index_of(2)", "1"),
            ("[1, 2, 3].index_of(9)", "missing"),
            ("[7, 7, 7].index_of(7)", "0"), // the FIRST match
            ("[-9223372036854775808].index_of(-9223372036854775808)", "0"),
            // cross-type: `1 == 1.0`, so a Float needle matches a packed Int array
            ("[1, 2, 3].contains(2.0)", "true"),
            ("[1, 2, 3].index_of(2.0)", "1"),
            ("[1, 2, 3].contains(2.5)", "false"),
            ("[1.0, 2.0].contains(2)", "true"),
            ("[1.0, 2.0].index_of(2)", "1"),
            // a needle of an unrelated type must miss, not error
            ("[1, 2, 3].contains(\"2\")", "false"),
            ("[1, 2, 3].contains(true)", "false"),
            ("[1, 2, 3].index_of([2])", "missing"),
            // packed arrays are missing-free, so `missing` is never found in one...
            ("[1, 2, 3].contains(missing)", "false"),
            ("[1, 2, 3].index_of(missing)", "missing"),
            // ...but IS found in a heterogeneous one (identity equality, ADR 0001)
            ("[1, missing, 3].contains(missing)", "true"),
            ("[1, missing, 3].index_of(missing)", "1"),
            // packed Floats, IEEE: NaN equals nothing, not even itself
            ("[1.5, inf - inf].contains(inf - inf)", "false"),
            ("[1.5, inf - inf].index_of(inf - inf)", "missing"),
            // ...and a NaN in the array must not poison the scan for other elements
            ("[1.5, inf - inf].contains(1.5)", "true"),
            ("[inf - inf, 2.5].index_of(2.5)", "1"),
            // infinities equal themselves; the two signs stay distinct
            ("[inf].contains(inf)", "true"),
            ("[inf].contains(-inf)", "false"),
            ("[1.0, inf, -inf].index_of(-inf)", "2"),
            ("[1, 2].contains(inf)", "false"),
            // signed zero: -0.0 == 0.0 in IEEE, in both directions and cross-type
            ("[-0.0].contains(0.0)", "true"),
            ("[0.0].index_of(-0.0)", "0"),
            ("[0, 1].index_of(-0.0)", "0"),
            // lazy Range
            ("(0..5).contains(3)", "true"),
            ("(0..5).index_of(3)", "3"),
            ("(0..5).index_of(9)", "missing"),
            ("range(0, 20, 3).index_of(9)", "3"),
            ("range(0, 20, 3).contains(10)", "false"),
            ("range(10, 0, -2).index_of(6)", "2"),
            // lazy Enumerate — the needle is a tuple, built one element at a time
            ("[5, 6, 7].enumerate().contains((1, 6))", "true"),
            ("[5, 6, 7].enumerate().index_of((1, 6))", "1"),
            ("[5, 6, 7].enumerate().contains(6)", "false"),
            // A lazy `Enumerate` over a heterogeneous inner array is the one packed shape
            // that CAN meet a `missing` — nested inside the tuple, where `values_equal`
            // reaches it by recursion rather than at the top level. Found by sabotaging
            // the `missing` rule and asking why the mutation survived.
            ("[1, missing, 3].enumerate().index_of((1, missing))", "1"),
            ("[1, missing, 3].enumerate().contains((1, missing))", "true"),
            ("[1, missing].enumerate().index_of((1, 2))", "missing"),
            // heterogeneous `Values` arrays defer to the general path, unchanged
            ("[1, \"a\", true].contains(\"a\")", "true"),
            ("[1, \"a\", true].index_of(true)", "2"),
            // empty
            ("[].contains(1)", "false"),
            ("[].index_of(1)", "missing"),
            ("(0..0).index_of(0)", "missing"),
            // the mapped-range shape the memory regression was found on
            ("(0..100).map(it * 2).index_of(4)", "2"),
            ("(0..100).map(it * 2).contains(5)", "false"),
            ("(0..100).map(it * 2.0).index_of(4.0)", "2"),
            // a miss composes with `??`, which is the documented idiom
            ("([1, 2, 3].index_of(9) ?? -1)", "-1"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // A wrong arity defers to the general path, so each method keeps its own (and
        // differently worded) error rather than acquiring the other's.
        for (src, want) in [
            ("[1, 2].contains()", "contains` takes one value to look for"),
            ("(0..5).contains(1, 2)", "contains` takes one value to look for"),
            ("[1, 2].index_of()", "index_of` takes 1 argument, got 0"),
            ("(0..5).index_of(1, 2)", "index_of` takes 1 argument, got 2"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.expect_err(&format!("`{src}` should error"));
            assert!(msg.contains(want), "`{src}` said: {msg}");
        }
    }

    /// `'…'` is an alternate string delimiter, and `'''…'''` the interpolating multi-line
    /// form — while `"""…"""` stays RAW.
    ///
    /// The point is not novelty, it is that the quote you are NOT delimited by needs no
    /// escaping. A conditional inside a hole was the worst-reading construct in real Helix:
    ///
    ///     print("-> {if ok then \"YES\" else \"NO\"}")      # before
    ///     print("-> {if ok then 'YES' else 'NO'}")         # after
    ///
    /// `'` was previously a lexer error ("unexpected character `'`"), so claiming it cannot
    /// change the meaning of any existing program — which is why this arrives as a lexer
    /// change with no parser or AST change at all: both forms produce the same `Tok::Str` /
    /// `Tok::InterpStr`.
    ///
    /// The two triples divide the work rather than competing: `"""` is raw (CSS, JSON,
    /// regexes, Windows paths go in verbatim) and `'''` interpolates. Nothing about `"`
    /// or `"""` changes, which the second half of this test is here to hold.
    #[test]
    fn single_quoted_and_triple_single_quoted_strings() {
        for (src, want) in [
            // an ordinary string, identical to the double-quoted one
            ("'hi'", "hi"),
            ("''", ""),
            ("'abc'.length()", "3"),
            ("'a' == \"a\"", "true"),
            // interpolation, format specs and brace escapes all behave the same
            ("'{1 + 1}'", "2"),
            ("'{3.14159:.2f}'", "3.14"),
            ("'{{literal}}'", "{literal}"),
            // THE MOTIVATING CASE: the other quote is literal inside, both ways round
            ("'he said \"hi\"'", "he said \"hi\""),
            ("\"it's fine\"", "it's fine"),
            ("\"-> {if true then 'YES' else 'NO'}\"", "-> YES"),
            ("'-> {if false then \"YES\" else \"NO\"}'", "-> NO"),
            // both delimiters escapable in either kind of string
            ("'a\\'b'", "a'b"),
            ("'a\\\"b'", "a\"b"),
            ("\"a\\'b\"", "a'b"),
            // a `}` inside a NESTED string must not close the interpolation hole — this is
            // what forced the hole scanner to track WHICH quote opened the nested string
            // rather than merely whether one was open.
            ("\"{'}'}\"", "}"),
            ("'{\"}\"}'", "}"),
            ("\"{'a}b'.length()}\"", "3"),
            // ''' — interpolating, multi-line, and a lone quote inside is literal
            ("'''a\nb'''", "a\nb"),
            ("'''{1 + 1}'''", "2"),
            ("''''''", ""),
            ("'''it's fine'''", "it's fine"),
            ("'''has \" and ' inside'''", "has \" and ' inside"),
            // --- and NOTHING about the double-quoted forms moved -------------------
            ("\"hi\"", "hi"),
            ("\"{1 + 1}\"", "2"),
            ("\"\"\"{1 + 1}\"\"\"", "{1 + 1}"), // """ is STILL RAW
            ("\"\"\"a\nb\"\"\"", "a\nb"),
            ("\"{{literal}}\"", "{literal}"),
            ("\"{3.14159:.2f}\"", "3.14"),
            // the old escaped spelling keeps working — this is the 467-site migration's
            // safety net, since both spellings must coexist during it
            ("\"{if true then \\\"Y\\\" else \\\"N\\\"}\"", "Y"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // The single-quoted forms reach the SAME diagnostics as the double-quoted ones.
        // (That each one's HINT names its own delimiter is asserted in tests/cli.rs, where
        // stderr is visible — these helpers return only the message.)
        for (src, want) in [
            ("'oops", "unterminated string literal"),
            ("\"oops", "unterminated string literal"),
            ("'''oops", "unterminated string literal"),
            ("'\\q'", "unknown string escape `\\q`"),
            ("'{}'", "empty `{}` interpolation"),
            ("'{1 +}'", "unexpected"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.expect_err(&format!("`{src}` should error"));
            assert!(msg.contains(want), "`{src}` said: {msg}");
        }
    }

    /// Unary `-` compiles into the i64 map/filter kernel, so the IDIOMATIC spelling stops
    /// losing to the clumsy one.
    ///
    ///     xs.map(-it)         0.45s  ->  0.04s      xs.map(0 - it)       0.04s
    ///     xs.filter(-it > -5) 0.47s  ->  0.03s      xs.filter(0 - it > -5) 0.02s
    ///
    /// at 8M elements, bit-identical results — an 11-16x gap between two spellings of one
    /// operation, where `-it` is the one anybody would actually write.
    ///
    /// `gen_value` already lowered `Neg` (`ineg`, which wraps exactly like the
    /// interpreter's `wrapping_neg`); only `value_eligible_cap` had never been taught the
    /// operator, so the body was rejected before codegen ever saw it. `filter` is fixed by
    /// the same one arm because `cond_eligible_cap` delegates its comparison operands here.
    ///
    /// FLOAT arrays deliberately still decline: that is the mixed kernel, whose codegen
    /// (`gen_value_typed`) has no `Neg` arm, and admitting a shape that codegen cannot emit
    /// is how this area got reverted three times before. `[1.5].map(-it)` is covered below
    /// to prove it stays CORRECT while staying interpreted.
    #[test]
    fn unary_minus_compiles_into_the_i64_map_and_filter_kernels() {
        // ENGAGEMENT FIRST — three engines agreeing proves nothing if the JIT declined and
        // the VM answered for it. Removing the `value_eligible_cap` arm must fail HERE.
        crate::jit::reset_native_call_count();
        assert_eq!(
            run_vm_jit("(0..100000).map(-it).sum()").unwrap(),
            "-4999950000"
        );
        assert!(
            crate::jit::native_call_count() > 0,
            "`map(-it)` did not engage the JIT — the kernel declined and the VM answered"
        );
        crate::jit::reset_native_call_count();
        assert_eq!(
            run_vm_jit("(0..100000).filter(-it > -10).length()").unwrap(),
            "10"
        );
        assert!(
            crate::jit::native_call_count() > 0,
            "`filter(-it > ..)` did not engage the JIT"
        );
        // The FLOAT paths, which needed two more gates and one more codegen arm. Two
        // distinct kernels: `Int`-source → `Float` (the "mixed" one, whose codegen
        // `gen_value_typed` had no `Neg` arm either) and `Floats`-source → `Floats` (whose
        // codegen `gen_value` already had one, so only its gate was missing). Which HALF of
        // the pair was absent differs per path, so each was traced and measured rather than
        // assumed — the first attempt here fixed only the mixed one and the measurement,
        // not the reading, is what revealed the other.
        crate::jit::reset_native_call_count();
        assert_eq!(
            run_vm_jit("(0..100000).map(-it * 1.0).sum()").unwrap(),
            "-4999950000.0"
        );
        assert!(
            crate::jit::native_call_count() > 0,
            "the Int-source -> Float `map(-it * 1.0)` did not engage the JIT"
        );
        // The Floats-source case needs a DELTA, not `> 0`: building the array is itself a
        // jittable `map`, so a bare `> 0` was satisfied by the construction alone and
        // survived deleting the gate under test. Count the same program with and without
        // the negation and require the negating one to run strictly more native calls.
        crate::jit::reset_native_call_count();
        let _ = run_vm_jit("ys = (0..100000).map(it * 1.0)\nys.sum()");
        let without = crate::jit::native_call_count();
        crate::jit::reset_native_call_count();
        assert_eq!(
            run_vm_jit("ys = (0..100000).map(it * 1.0)\nys.map(-it).sum()").unwrap(),
            "-4999950000.0"
        );
        let with_neg = crate::jit::native_call_count();
        assert!(
            with_neg > without,
            "the Floats-source `map(-it)` did not engage the JIT \
             ({with_neg} native calls with it, {without} without — the building `map` \
             accounts for the rest)"
        );
        // Values AT KERNEL SCALE. Every float case below this point in the table runs on a
        // handful of elements, which is BELOW the dispatch threshold — so it is checking
        // the interpreter, not the code just written. These three run at 100k, where the
        // kernel really executes, and each is chosen to fail if the lowering is wrong:
        // `fneg` gives element 0 a NEGATIVE zero, where `0.0 - x` would give a positive
        // one, and where negating the i64 with `fneg` would corrupt the bit pattern.
        for (src, want) in [
            (
                "ys = (0..100000).map(it * 1.0)\nys.map(-it).take(3)",
                "[-0.0, -1.0, -2.0]",
            ),
            (
                "ys = (0..100000).map(it * 1.0)\nys.map(0.0 - it).take(3)",
                "[0.0, -1.0, -2.0]", // NOT the same as `-it` at element 0
            ),
            ("(0..100000).map(-it * 1.5).take(3)", "[0.0, -1.5, -3.0]"),
            ("(0..100000).map(-to_float(it)).take(3)", "[-0.0, -1.0, -2.0]"),
            ("(0..100000).map(-it).take(3)", "[0, -1, -2]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tw vs vm disagree on `{src}`");
            assert_eq!(vm, jit, "vm vs jit disagree on `{src}` — KERNEL-SCALE divergence");
            assert_eq!(jit, Ok(want.to_string()), "`{src}`");
        }

        for (src, want) in [
            // the newly-compiling shapes
            ("[1, 2, 3].map(-it)", "[-1, -2, -3]"),
            ("[1, 2, 3].map(-(it + 1))", "[-2, -3, -4]"),
            ("[1, 2, 3].map(-it * 3)", "[-3, -6, -9]"),
            ("[1, 2, 3].map(- -it)", "[1, 2, 3]"),
            ("(0..5).map(-it)", "[0, -1, -2, -3, -4]"),
            ("[1, 2, 3].filter(-it > -3)", "[1, 2]"),
            ("(0..10).filter(-it >= -4)", "[0, 1, 2, 3, 4]"),
            // `ineg` WRAPS, exactly as the interpreter's `wrapping_neg` does. This is the
            // case that would expose any disagreement between the two, and it is the only
            // i64 for which negation is not an involution.
            (
                "[-9223372036854775808].map(-it)",
                "[-9223372036854775808]",
            ),
            (
                "[-9223372036854775808, 0, 9223372036854775807].map(-it)",
                "[-9223372036854775808, 0, -9223372036854775807]",
            ),
            ("[9223372036854775807].map(-it)", "[-9223372036854775807]"),
            // negation composed with the operators that carry their own guards; `%` and
            // `//` are EUCLIDEAN here, so the sign of the left operand is observable
            ("[7, 8].map(-it % 3)", "[2, 1]"),
            ("[7, 8].map(-(it % 3))", "[-1, -2]"),
            ("[7, 8].map(-it // 3)", "[-3, -3]"),
            ("[7, 8].map(-it >> 1)", "[-4, -4]"),
            ("[7, 8].map(-it & 3)", "[1, 0]"),
            ("[-7, -8].map(-it % 3)", "[1, 2]"),
            // captures ride as `caps[i]`, including an i64::MIN one
            ("k = 5\n[1, 2].map(-it + k)", "[4, 3]"),
            ("k = 5\n[1, 2].map(-k + it)", "[-4, -3]"),
            (
                "k = -9223372036854775808\n[1, 2].map(-k)",
                "[-9223372036854775808, -9223372036854775808]",
            ),
            ("k = 3\n[1, 2, 3, 4].filter(-it > -k)", "[1, 2]"),
            // nested positions
            ("[1, 2].map(if it > 1 then -it else it)", "[1, -2]"),
            ("[5, 6].map(-abs(it))", "[-5, -6]"),
            ("[5, 6].map(abs(-it))", "[5, 6]"),
            // FLOAT arrays now compile too, and `fneg` is the EXACT IEEE sign flip — which
            // is only observable on the zeros, so those are the cases that pin it.
            ("[1.5, 2.5].map(-it)", "[-1.5, -2.5]"),
            ("[0.0].map(-it)", "[-0.0]"),
            ("[-0.0].map(-it)", "[0.0]"),
            ("[0.0, -0.0].map(-it)", "[-0.0, 0.0]"),
            ("[inf, -inf].map(-it)", "[-inf, inf]"),
            ("[inf - inf].map(-it)", "[NaN]"),
            ("[1.5].map(-(it + 1.0))", "[-2.5]"),
            ("[1.5].map(-it * 2.0)", "[-3.0]"),
            ("[1.5].map(- -it)", "[1.5]"),
            ("[2.0].map(-sqrt(it))", "[-1.4142135623730951]"),
            ("[1.5].map(-abs(it))", "[-1.5]"),
            ("k = 2.0\n[1.5].map(-it * k)", "[-3.0]"),
            // Int source, Float result — the mixed kernel. Negating INSIDE `to_float`
            // versus outside it differ on the zero, and both must match the interpreter.
            ("(0..5).map(-it * 1.5)", "[0.0, -1.5, -3.0, -4.5, -6.0]"),
            ("(0..3).map(to_float(-it))", "[0.0, -1.0, -2.0]"),
            ("(0..3).map(-to_float(it))", "[-0.0, -1.0, -2.0]"),
            ("[1, 2].map(-it * 1.0)", "[-1.0, -2.0]"),
            ("[1.5, missing].map(-it)", "[-1.5, missing]"),
            // a `missing` or non-numeric element declines and behaves exactly as before
            ("[1, missing, 3].map(-it)", "[-1, missing, -3]"),
            ("[missing].map(-it)", "[missing]"),
            // reduce/scan bodies use a different analysis and are untouched
            ("[1, 2, 3].reduce(0, (a, b) => a + -b)", "-6"),
            ("[1, 2, 3].scan(0, (a, b) => a + -b)", "[-1, -3, -6]"),
        ] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tw vs vm disagree on `{src}`");
            assert_eq!(vm, jit, "vm vs jit disagree on `{src}`");
            assert_eq!(jit, Ok(want.to_string()), "`{src}`");
        }
        // Negating a non-numeric still raises, with the same text on every engine.
        for src in ["[1, \"a\"].map(-it)", "[true].map(-it)"] {
            let (tw, vm, jit) = (run_tw(src), run_vm(src), run_vm_jit(src));
            assert_eq!(tw, vm, "tw vs vm disagree on `{src}`");
            assert_eq!(vm, jit, "vm vs jit disagree on `{src}`");
            assert!(jit.is_err(), "`{src}` should error");
        }
    }

    /// A malformed comprehension is rejected even when the receiver is `missing`.
    ///
    /// This was a three-engine VALUE divergence, and the worst kind: it escaped through
    /// `try` into an ordinary boolean, where no error text could reveal it.
    ///
    ///     (try missing.map()).ok     tree-walker: true      VM and JIT: false
    ///
    /// `missing.map()` returned `missing` on the walker while `[1, 2].map()` was an error
    /// — the same malformed call, an error for one receiver and a success for another, so
    /// the walker was inconsistent with itself before it was inconsistent with anything
    /// else. Arity and the binder requirement are STRUCTURAL: the VM and JIT settle them
    /// when they compile the comprehension, so the receiver's runtime value cannot matter.
    /// The walker reached the same rules per-arm, after matching the receiver, and the
    /// `missing` arm returned first.
    ///
    /// The fix must not evaluate anything. All three engines agree that
    /// `missing.reduce(1 / 0, ...)` is `missing` while `[].reduce(1 / 0, ...)` divides by
    /// zero, so validating by running the comprehension against an empty array — the
    /// tempting way to avoid restating the rules — would have swapped this divergence for
    /// a new one. That is why `comp_shape_check` is purely structural.
    #[test]
    fn a_malformed_comprehension_is_rejected_on_a_missing_receiver_too() {
        // Every engine must reject these, with the identical message, whatever the
        // receiver is. `run_tw` vs `run_vm` is the oracle comparison; the walker is the
        // designated reference, which is precisely why it drifting mattered.
        for (src, want) in [
            ("missing.map()", "`map` takes exactly one expression"),
            ("missing.map(1, 2)", "`map` takes exactly one expression"),
            ("missing.map(() => 5)", "`map`'s function needs at least one parameter"),
            ("missing.filter()", "`filter` takes exactly one expression"),
            (
                "missing.filter(() => 5)",
                "`filter`'s function needs at least one parameter",
            ),
            ("missing.where()", "`where` takes exactly one expression"),
            ("missing.any()", "`any` takes exactly one expression"),
            ("missing.all()", "`all` takes exactly one expression"),
            (
                "missing.reduce()",
                "`reduce` takes a starting value and an accumulator function",
            ),
            (
                "missing.reduce(1)",
                "`reduce` takes a starting value and an accumulator function",
            ),
            (
                "missing.reduce(0, acc + it)",
                "`reduce` needs an explicit accumulator function",
            ),
            (
                "missing.reduce(0, (a) => a)",
                "`reduce`'s function needs exactly two parameters, but got 1",
            ),
            (
                "missing.scan(1)",
                "`scan` takes a starting value and an accumulator function",
            ),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.expect_err(&format!("`{src}` should error"));
            assert!(msg.contains(want), "`{src}` said: {msg}");
            // ...and the array spelling of the same malformed call reports it too, so the
            // rejection is a property of the CALL and not of the receiver.
            let arr = src.replace("missing.", "[1, 2].");
            assert!(run_vm(&arr).is_err(), "`{arr}` should error");
            assert_eq!(run_tw(&arr), run_vm(&arr), "engines disagree on `{arr}`");
        }
        // A WELL-FORMED call still propagates under ADR 0001 — the fix narrows nothing.
        // The `1 / 0` cases are the load-bearing ones: they prove no argument is
        // evaluated on this path, which is what all three engines already agreed on and
        // what an empty-array validation would have broken.
        for src in [
            "missing.map(it * 2)",
            "missing.filter(it > 0)",
            "missing.where(it > 0)",
            "missing.any(it > 0)",
            "missing.all(it > 0)",
            "missing.position(it > 0)",
            "missing.reduce(0, (a, b) => a + b)",
            "missing.scan(0, (a, b) => a + b)",
            "missing.map(1 / 0)",
            "missing.reduce(1 / 0, (a, b) => a)",
            "missing.scan(1 / 0, (a, b) => a)",
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok("missing".to_string()), "`{src}`");
        }
        // A receiver that is neither an array nor `missing` reports the TYPE error rather
        // than the arity one, and the fix must not reorder that — but it is NOT asserted
        // through these helpers, because they cannot see it. `run_vm`/`run_tw` call
        // `compile_with_types(.., None)`, so the type checker the CLI runs first is
        // skipped; without it the VM reaches its compile-time arity check and the walker
        // reaches its receiver-type check, and the two report different errors for
        // `5.map()`. That difference predates this fix, is on a path it does not touch,
        // and is masked from users by the checker. It is recorded in `docs/ROADMAP.md`
        // rather than pinned here, where the assertion would only be describing the
        // harness. End-to-end agreement was verified against the previous binary across
        // all three engines for `5`, `"ab"`, `[1, 2]` and `(0..3)` receivers.
        // The escape route that hid it: `try` turns the error into a value, so this is
        // the exact expression that read `true` on one engine and `false` on two.
        for src in [
            "(try missing.map()).ok",
            "(try missing.filter()).ok",
            "(try missing.reduce(1)).ok",
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok("false".to_string()), "`{src}`");
        }
        assert_eq!(
            run_tw("(try missing.map(it * 2)).ok"),
            Ok("true".to_string())
        );
    }

    /// `cumsum` reads a packed source, and `flatten` concatenates packed columns.
    ///
    ///     xs = (0..20000000).map(it * 2)
    ///     xs.cumsum().last()     645 MB  ->  346 MB
    ///     [xs].flatten()         797 MB  ->  346 MB
    ///
    /// Two different causes, one class. `cumsum` always RETURNED a packed column but
    /// never had a packed input, so the source was boxed before it was called; `flatten`
    /// boxed every inner element twice (`to_values()`, then the output vec) only for
    /// `array_sniff` to unpack the lot again.
    ///
    /// The accumulation is deliberately identical to the general path rather than better,
    /// which is what most of these cases check: `wrapping_add` for ints, and plain `+=`
    /// for floats — NOT the Neumaier summation `sum`/`mean` use. `[1e16, 1.0, -1e16]` is
    /// the case that tells them apart (Neumaier would end at `1.0`, plain `+=` at `0.0`),
    /// so it is the one that would catch an "improvement" that silently diverged.
    #[test]
    fn cumsum_and_flatten_work_on_packed_columns() {
        for (src, want) in [
            // cumsum, ints
            ("[1, 2, 3].cumsum()", "[1, 3, 6]"),
            ("[].cumsum()", "[]"),
            ("[5].cumsum()", "[5]"),
            ("[-1, -2, -3].cumsum()", "[-1, -3, -6]"),
            // i64 overflow WRAPS, exactly as the general path's `wrapping_add` did
            (
                "[9223372036854775807, 1].cumsum()",
                "[9223372036854775807, -9223372036854775808]",
            ),
            (
                "[9223372036854775807, 9223372036854775807].cumsum()",
                "[9223372036854775807, -2]",
            ),
            (
                "[-9223372036854775808, -1].cumsum()",
                "[-9223372036854775808, 9223372036854775807]",
            ),
            // cumsum, floats — plain `+=`, so the middle term is LOST here. Neumaier
            // would keep it and end at `1.0`; that difference is the point of this case.
            (
                "[1e16, 1.0, -1e16].cumsum()",
                "[10000000000000000.0, 10000000000000000.0, 0.0]",
            ),
            ("[1.5, 2.5, 3.5].cumsum()", "[1.5, 4.0, 7.5]"),
            (
                "[0.1, 0.2, 0.3].cumsum()",
                "[0.1, 0.30000000000000004, 0.6000000000000001]",
            ),
            // a NaN poisons the running total from that point on and is never turned
            // into `missing` — `cumsum` checks only for `missing`, unlike the reductions
            ("[1.0, inf - inf, 2.0].cumsum()", "[1.0, NaN, NaN]"),
            ("[1.0, inf, 2.0].cumsum()", "[1.0, inf, inf]"),
            ("[1.0, inf, -inf].cumsum()", "[1.0, inf, NaN]"),
            // the accumulator starts at a POSITIVE zero, so these stay positive
            ("[-0.0, -0.0].cumsum()", "[0.0, 0.0]"),
            // cumsum on the lazy representations
            ("(0..5).cumsum()", "[0, 1, 3, 6, 10]"),
            ("(0..0).cumsum()", "[]"),
            ("range(0, 20, 3).cumsum()", "[0, 3, 9, 18, 30, 45, 63]"),
            ("range(10, 0, -2).cumsum()", "[10, 18, 24, 28, 30]"),
            ("(0..5).reverse().cumsum()", "[4, 7, 9, 10, 10]"),
            // `Values` arrays keep the general path, including missing propagation
            ("[1, missing, 3].cumsum()", "missing"),
            ("[1, 2.5].cumsum()", "[1.0, 3.5]"),
            ("[2.5, 1].cumsum()", "[2.5, 3.5]"),
            ("(0..10).map(it * 2).cumsum().last()", "90"),
            ("[1, 2, 3].cumsum().cumsum()", "[1, 4, 10]"),
            ("[1, 2, 3].cumsum() == [1, 3, 6]", "true"),
            // --- flatten -----------------------------------------------------------
            ("[[1, 2], [3]].flatten()", "[1, 2, 3]"),
            ("[[1, 2], []].flatten()", "[1, 2]"),
            ("[[], []].flatten()", "[]"),
            ("[].flatten()", "[]"),
            ("[[1.5, 2.5], [3.5]].flatten()", "[1.5, 2.5, 3.5]"),
            ("[(0..3), (0..2)].flatten()", "[0, 1, 2, 0, 1]"),
            ("[range(0, 6, 2), [9]].flatten()", "[0, 2, 4, 9]"),
            // A MIXED nesting must stay boxed: packing it to floats would silently turn
            // that `1` into a `1.0`.
            ("[[1], [2.0]].flatten()", "[1, 2.0]"),
            ("[[2.0], [1]].flatten()", "[2.0, 1]"),
            ("[[1], [2.0]].flatten().first()", "1"),
            ("[[1, 2.0]].flatten()", "[1, 2.0]"),
            // non-array elements, and array/scalar mixtures, keep the general path
            ("[1, 2, 3].flatten()", "[1, 2, 3]"),
            ("[[1], 2].flatten()", "[1, 2]"),
            ("[2, [1]].flatten()", "[2, 1]"),
            ("[[\"a\"], [\"b\"]].flatten()", "[\"a\", \"b\"]"),
            ("[[missing], [1]].flatten()", "[missing, 1]"),
            // flatten is ONE level deep, not recursive
            ("[[[1]], [[2]]].flatten()", "[[1], [2]]"),
            // the flattened result must still be packed for the numeric verbs
            ("[[1, 2], [3]].flatten().sum()", "6"),
            ("[[1, 2], [3]].flatten().contains(3)", "true"),
            ("[[1, 2], [3]].flatten() == [1, 2, 3]", "true"),
            ("[[1.5], [2.5]].flatten().mean()", "2.0"),
            ("[[3, 1], [2]].flatten().sort()", "[1, 2, 3]"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        for (src, want) in [
            ("[1, \"a\"].cumsum()", "needs an array of numbers"),
            ("[3, 1].enumerate().cumsum()", "needs an array of numbers"),
            ("[1, 2].cumsum(3)", "`cumsum` takes no arguments"),
            ("(0..5).cumsum(3)", "`cumsum` takes no arguments"),
            ("[[1, 2], [3]].flatten(9)", "`flatten` takes no arguments"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.expect_err(&format!("`{src}` should error"));
            assert!(msg.contains(want), "`{src}` said: {msg}");
        }
    }

    /// `sort`/`reverse` keep a packed array packed, and reversing a range stays lazy.
    ///
    ///     xs = (0..20000000).map(it * 2)
    ///     xs.sort().first()             796 MB  ->  338 MB
    ///     xs.reverse().first()          796 MB  ->  338 MB
    ///     range(100000000).reverse().first()   3071 MB  ->  15 MB
    ///
    /// The cost used to be paid twice: a `Vec<Value>` of the whole source to do the work,
    /// and a result returned through `Value::array` (which, unlike `Value::array_sniff`,
    /// does not re-pack) that silently stripped the fast path from everything downstream.
    ///
    /// So the cases that matter most here are not the orderings — they are the ones that
    /// keep USING the result, because the change is really "this now comes back packed
    /// where it came back boxed", and any behaviour that observes representation would
    /// break there.
    #[test]
    fn sort_and_reverse_keep_a_packed_array_packed() {
        for (src, want) in [
            // ordering, Ints
            ("[3, 1, 2].sort()", "[1, 2, 3]"),
            ("[3, 1, 2].reverse()", "[2, 1, 3]"),
            ("[2, 2, 1].sort()", "[1, 2, 2]"),
            ("[-5, 3, -1, 0].sort()", "[-5, -1, 0, 3]"),
            ("[].sort()", "[]"),
            ("[].reverse()", "[]"),
            (
                "[-9223372036854775808, 9223372036854775807, 0].sort()",
                "[-9223372036854775808, 0, 9223372036854775807]",
            ),
            // `numeric_cmp` compares two Ints as i64 on purpose: widening to f64 would
            // collapse these three distinct values into one and lose the order.
            (
                "[9007199254740993, 9007199254740992, 9007199254740994].sort()",
                "[9007199254740992, 9007199254740993, 9007199254740994]",
            ),
            // ordering, Floats — `total_cmp`, so signed zeros keep a deterministic
            // position (`-0.0` before `0.0`) rather than comparing equal.
            ("[3.5, 1.5, 2.5].sort()", "[1.5, 2.5, 3.5]"),
            ("[0.0, -0.0].sort()", "[-0.0, 0.0]"),
            ("[-0.0, 0.0].sort()", "[-0.0, 0.0]"),
            ("[1.0, -0.0, 0.0, -1.0].sort()", "[-1.0, -0.0, 0.0, 1.0]"),
            ("[1.0, inf, -inf].sort()", "[-inf, 1.0, inf]"),
            // A NaN never aborts the sort (a non-total comparator would make Rust panic).
            // Note it lands FIRST here, not after `+inf`: `inf - inf` has its sign bit
            // set, and `total_cmp` orders by that bit. See `numeric_cmp`.
            ("[1.0, inf - inf, -inf, 2.0].sort()", "[NaN, -inf, 1.0, 2.0]"),
            ("[1.0, -(inf - inf), -inf, 2.0].sort()", "[-inf, 1.0, 2.0, NaN]"),
            ("[1.5, 2.5].reverse().reverse()", "[1.5, 2.5]"),
            // lazy Range — reversing one is another range, so this stays O(1)
            ("(0..5).sort()", "[0, 1, 2, 3, 4]"),
            ("(0..5).reverse()", "[4, 3, 2, 1, 0]"),
            ("(0..0).reverse()", "[]"),
            ("(0..0).sort()", "[]"),
            ("range(5, 0, -1).sort()", "[1, 2, 3, 4, 5]"),
            ("range(5, 0, -1).sort().first()", "1"),
            ("range(5, 0, -1).sort().sum()", "15"),
            ("(0..1).reverse()", "[0]"),
            ("range(0, 20, 3).reverse()", "[18, 15, 12, 9, 6, 3, 0]"),
            ("range(10, 0, -2).reverse()", "[2, 4, 6, 8, 10]"),
            ("range(10, 0, -2).sort()", "[2, 4, 6, 8, 10]"),
            ("range(0, 20, 3).reverse().reverse()", "[0, 3, 6, 9, 12, 15, 18]"),
            // ...and the reversed range still answers every verb a range answers
            ("range(0, 20, 3).reverse().first()", "18"),
            ("range(0, 20, 3).reverse().last()", "0"),
            ("range(0, 20, 3).reverse().length()", "7"),
            ("range(0, 20, 3).reverse().sum()", "63"),
            ("(0..5).reverse().take(2)", "[4, 3]"),
            ("(0..5).reverse().drop(3)", "[1, 0]"),
            ("(0..5).reverse().index_of(3)", "1"),
            ("(0..5).reverse().enumerate().first()", "(0, 4)"),
            // A step of `i64::MIN` cannot be negated, so that case materializes instead
            // of wrapping to a bogus step. This range really does have two elements, and
            // that matters: a one-element version passes even with a wrapping negation
            // because the step is never applied. Sabotaging `checked_neg` into
            // `wrapping_neg` survived the weaker case, which is how the gap was found.
            (
                "range(9223372036854775807, -9223372036854775808, -9223372036854775808)",
                "[9223372036854775807, -1]",
            ),
            (
                "range(9223372036854775807, -9223372036854775808, -9223372036854775808).reverse()",
                "[-1, 9223372036854775807]",
            ),
            (
                "range(9223372036854775807, -9223372036854775808, -9223372036854775808).reverse().sum()",
                "9223372036854775806",
            ),
            (
                "range(0, -9223372036854775808, -9223372036854775808).reverse()",
                "[0]",
            ),
            // `Values` and `Enumerate` defer to the general path, unchanged
            ("[3, 1].enumerate().reverse()", "[(1, 1), (0, 3)]"),
            ("[1, \"a\"].reverse()", "[\"a\", 1]"),
            ("[\"b\", \"a\"].sort()", "[\"a\", \"b\"]"),
            ("[1, 2.5].sort()", "[1, 2.5]"),
            // --- the result is now PACKED: nothing may observe that ------------------
            ("[3, 1, 2].sort() == [1, 2, 3]", "true"),
            ("[[3, 1].sort()] == [[1, 3]]", "true"),
            ("([3, 1].sort(), 1) == ([1, 3], 1)", "true"),
            ("[3, 1, 2].sort().sum()", "6"),
            ("[3, 1, 2].sort().mean()", "2.0"),
            ("[3, 1, 2].sort().contains(2)", "true"),
            ("[3, 1, 2].reverse().index_of(1)", "1"),
            ("[3, 1, 2].sort().take(2)", "[1, 2]"),
            ("[3, 1, 2].sort().map(it * 2)", "[2, 4, 6]"),
            ("[3, 1, 2].sort().filter(it > 1)", "[2, 3]"),
            ("[3, 1, 2].sort().concat([9])", "[1, 2, 3, 9]"),
            ("[3, 1, 2].sort().cumsum()", "[1, 3, 6]"),
            ("[3, 1, 2].sort().unique()", "[1, 2, 3]"),
            ("[3, 1, 2].sort().frequencies()", "[(1, 1), (2, 1), (3, 1)]"),
            ("[3, 1, 2].sort().argmin()", "0"),
            ("[3, 1, 2].sort().join(\"-\")", "1-2-3"),
            ("[3, 1, 2].sort().enumerate().first()", "(0, 1)"),
            ("[3, 1, 2].reverse().zip([1, 2, 3])", "[(2, 1), (1, 2), (3, 3)]"),
            ("[3.5, 1.5].sort().std()", "1.0"),
            ("[1.0, 3.0].reverse().median()", "2.0"),
            ("[[3, 1].sort(), [2].sort()].flatten()", "[1, 3, 2]"),
            // the mapped-range shapes the regression was measured on
            ("(0..10).map(it * 2).sort().first()", "0"),
            ("(0..10).map(it * 2).reverse().first()", "18"),
            ("(0..10).map(it * 1.0).reverse().first()", "9.0"),
            ("(0..10).filter(it % 3 == 0).reverse()", "[9, 6, 3, 0]"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            assert_eq!(vm, Ok(want.to_string()), "`{src}`");
        }
        // Every rejection `sort` used to make it still makes — the fast path must not
        // quietly start accepting an array the general path refuses.
        for (src, want) in [
            ("[1, missing].sort()", "cannot sort: the array has missing values"),
            ("[1, \"a\"].sort()", "all numbers, all strings, or all DNA"),
            ("[3, 1].enumerate().sort()", "all numbers, all strings, or all DNA"),
            ("[1, 2].sort(3)", "takes no arguments, got 1"),
            ("[1, 2].reverse(3)", "takes no arguments, got 1"),
            ("(0..5).sort(3)", "takes no arguments, got 1"),
            ("(0..5).reverse(3)", "takes no arguments, got 1"),
        ] {
            let (tw, vm) = (run_tw(src), run_vm(src));
            assert_eq!(tw, vm, "engines disagree on `{src}`");
            let msg = vm.expect_err(&format!("`{src}` should error"));
            assert!(msg.contains(want), "`{src}` said: {msg}");
        }
    }
