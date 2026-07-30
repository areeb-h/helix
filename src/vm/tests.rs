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
                        "(0 - 1)",
                        "0",
                        "1",
                        "2",
                        "9007199254740993",              // 2^53 + 1
                        "(0 - 9007199254740993)",
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
            "isqrt(0 - 4)",                                    // error on all engines
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
            "fn z(n, acc) = if n <= 0 then acc else z(n - 1, acc + 1)\nz(0 - 5, 42)",
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
            "fn step(zr: Float, zi: Float, cr: Float, ci: Float, i: Int) = if i >= 60 or zr * zr + zi * zi > 4.0 then i else step(zr * zr - zi * zi + cr, 2.0 * zr * zi + ci, cr, ci, i + 1)\nstep(0.0, 0.0, 0.0 - 1.5, 0.02, 0)",
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
            "fn nn(x: Float, n: Int) = if sqrt(x) > 0.0 then x else nn(x, n - 1)\nfn w2(a: Float, k: Int) = nn(a, k) + 1.0\nw2(0.0 - 1.0, 2)",
            // the mandelbrot escape shape end-to-end at a small max_iter
            "fn step(zr: Float, zi: Float, cr: Float, ci: Float, i: Int) = if i >= 40 or zr * zr + zi * zi > 4.0 then i else step(zr * zr - zi * zi + cr, 2.0 * zr * zi + ci, cr, ci, i + 1)\nfn esc2(px: Int, py: Int) = step(0.0, 0.0, 0.0 - 2.5 + 3.5 * px / 60.0, 0.0 - 1.0 + 2.0 * py / 60.0, 0)\nrange(0, 60).map(py => range(0, 60).map(px => esc2(px, py)).sum()).sum()",
            // NaN POISON (the review-confirmed divergence): the interpreter RAISES on a
            // NaN comparison, so the native loop must bail (unordered fcmp → poison →
            // bytecode fallback → identical error), never silently order the NaN
            "fn bad(x: Float, n: Int) = if sqrt(x) > 0.0 then n else bad(x, n + 1)\nbad(0.0 - 1.0, 0)",
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
            "fn nv(x: Float, n: Int) = if n <= 0 then sqrt(x) else nv(x, n - 1)\nnv(0.0 - 4.0, 2)",
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
            "range(0, 10).take(0 - 5).count()",
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
            "a = 0.0 - 5.5\nx = [1.0, 2.0]\n(0..2).map(i => abs(a) + x[i])",
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
            "c = 0.0 - 5.5\n(0..3).reduce(0.0, (s, i) => s + abs(c) + a[i])",
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
            ("((0 - 2)..2).map(i => i * 1.5).reduce(0.0, (s, x) => s + x)", "-3.0"),
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
            format!("a = {aa}\n(range(0 - 5, 40)).map(i => (range(i + 1, 40)).reduce(0, (acc, j) => acc + a[j])).sum()"),
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
            ("[1.0, sqrt(0.0 - 1.0)] == [1.0, sqrt(0.0 - 1.0)]", "false"),
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
