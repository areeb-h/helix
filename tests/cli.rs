//! End-to-end CLI integration tests: they run the *actual compiled `helix`
//! binary* as a subprocess, exercising the parts unit tests can't reach — argument
//! parsing, file reading, exit codes, stdout/stderr, the REPL, and the
//! `HELIX_NOVM` engine switch. Cargo provides the freshly-built binary path via
//! `CARGO_BIN_EXE_helix`, and runs with the package root as the working directory
//! so the examples' relative data paths (`examples/data/*.csv`) resolve.

use std::io::Write;
use std::process::{Command, Stdio};

/// Run the `helix` binary and capture (stdout, stderr, exit_code). `env` adds
/// environment variables; `stdin` is fed to the process (for the REPL).
fn run(args: &[&str], env: &[(&str, &str)], stdin: &str) -> (String, String, Option<i32>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_helix"));
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("failed to spawn helix");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("failed to wait on helix");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// Run a source string by writing it to a unique temp file (tests run in
/// parallel, so the name is tagged).
fn run_source(src: &str, env: &[(&str, &str)], tag: &str) -> (String, String, Option<i32>) {
    let path = std::env::temp_dir().join(format!("helix_it_{tag}.helix"));
    std::fs::write(&path, src).unwrap();
    let r = run(&[path.to_str().unwrap()], env, "");
    let _ = std::fs::remove_file(&path);
    r
}

fn example_files() -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut out: Vec<_> = std::fs::read_dir(&dir)
        .expect("examples/ dir")
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension().and_then(|s| s.to_str()) == Some("helix")).then_some(p)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn every_example_runs_clean() {
    let files = example_files();
    assert!(files.len() >= 10, "expected the example suite, saw {}", files.len());
    for path in files {
        let rel = format!("examples/{}", path.file_name().unwrap().to_str().unwrap());
        let (stdout, stderr, code) = run(&[&rel], &[], "");
        assert_eq!(code, Some(0), "`{rel}` exited {code:?}; stderr:\n{stderr}");
        assert!(!stdout.trim().is_empty(), "`{rel}` produced no output");
    }
}

/// The VM (default) and the tree-walker (`HELIX_NOVM=1`) must produce identical
/// output for every example — the same parity the differential fuzzers check at
/// the unit level, here through the real CLI. `dataframes.helix` is excluded: its
/// group-by emits rows in Polars' nondeterministic order.
#[test]
fn vm_matches_tree_walker_via_cli() {
    for path in example_files() {
        let name = path.file_name().unwrap().to_str().unwrap();
        // Excluded: group-by emits rows in Polars' nondeterministic order, so the two
        // engines can print the same rows in a different order.
        if name == "dataframes.helix" || name == "variants.helix" {
            continue;
        }
        let rel = format!("examples/{name}");
        let (vm, _, vc) = run(&[&rel], &[], "");
        let (tw, _, tc) = run(&[&rel], &[("HELIX_NOVM", "1")], "");
        assert_eq!(vc, Some(0), "VM run of `{rel}` failed");
        assert_eq!(tc, Some(0), "tree-walker run of `{rel}` failed");
        assert_eq!(vm, tw, "VM and tree-walker disagree on `{rel}`");
    }
}

#[test]
fn version_and_help_flags() {
    for flag in ["--version", "-V"] {
        let (stdout, _, code) = run(&[flag], &[], "");
        assert_eq!(code, Some(0));
        assert!(stdout.contains("helix"), "`{flag}` => {stdout:?}");
    }
    for flag in ["--help", "-h"] {
        let (stdout, _, code) = run(&[flag], &[], "");
        assert_eq!(code, Some(0));
        assert!(stdout.to_lowercase().contains("usage"), "`{flag}` => {stdout:?}");
    }
}

#[test]
fn missing_file_is_a_clean_error() {
    let (_, stderr, code) = run(&["does_not_exist.helix"], &[], "");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("cannot read"), "stderr: {stderr:?}");
}

#[test]
fn type_error_aborts_before_running() {
    // An undefined name is caught by the type checker; nothing should print.
    let (stdout, stderr, code) =
        run_source("print(\"start\")\nprint(undefined_name)\n", &[], "typeerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("not defined"), "stderr: {stderr:?}");
    // The type error fires before execution, so the earlier print never runs.
    assert!(!stdout.contains("start"), "side effects leaked: {stdout:?}");
}

#[test]
fn runtime_error_exits_nonzero() {
    let (_, stderr, code) = run_source("print(1 / 0)\n", &[], "divzero");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("division by zero"), "stderr: {stderr:?}");
}

#[test]
fn immutable_reassignment_errors_on_the_vm() {
    let (_, stderr, code) = run_source("x = 1\nx = 2\nprint(x)\n", &[], "immut");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("immutable"), "stderr: {stderr:?}");
}

#[test]
fn repl_evaluates_and_exits_on_eof() {
    // No file arg => REPL. Feed one expression, then EOF (closed stdin).
    let (stdout, _, code) = run(&[], &[], "21 + 21\n");
    assert_eq!(code, Some(0), "REPL should exit cleanly on EOF");
    assert!(stdout.contains("42"), "REPL did not echo the result: {stdout:?}");
}

/// Write `files` into a fresh temp directory and run `entry` (resolved there, so
/// the loader's sibling-import resolution works). Returns (stdout, stderr, code).
fn run_modules(
    files: &[(&str, &str)],
    entry: &str,
    env: &[(&str, &str)],
    tag: &str,
) -> (String, String, Option<i32>) {
    let dir = std::env::temp_dir().join(format!("helix_mod_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (name, src) in files {
        std::fs::write(dir.join(name), src).unwrap();
    }
    let entry_path = dir.join(entry);
    let r = run(&[entry_path.to_str().unwrap()], env, "");
    let _ = std::fs::remove_dir_all(&dir);
    r
}

#[test]
fn module_program_runs_and_matches_engines() {
    // The committed multi-file example: shapes.helix imports geometry.helix.
    let entry = "examples/modules/shapes.helix";
    let (vm, stderr, code) = run(&[entry], &[], "");
    assert_eq!(code, Some(0), "modules example failed; stderr:\n{stderr}");
    assert!(vm.contains("area 12"), "unexpected output: {vm:?}");
    let (tw, _, _) = run(&[entry], &[("HELIX_NOVM", "1")], "");
    assert_eq!(vm, tw, "VM and tree-walker disagree on the modules example");
}

#[test]
fn cross_module_calls_and_local_shadowing() {
    let lib = "fn double(x) = x * 2\nfn quad(x) = double(double(x))\nN = 7\n";
    // `double` is redefined locally in main — it must shadow the module's `double`.
    let main = "import lib\nprint(lib.quad(3))\nprint(lib.N)\nfn double(x) = x + 100\nprint(double(1))\n";
    let (out, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "shadow");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "12\n7\n101", "got: {out:?}"); // quad(3)=12, N=7, local double(1)=101
}

#[test]
fn import_cycle_is_rejected() {
    let a = "import b\nprint(1)\n";
    let b = "import a\nprint(2)\n";
    let (_, stderr, code) =
        run_modules(&[("a.helix", a), ("b.helix", b)], "a.helix", &[], "cycle");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("cycle"), "stderr: {stderr:?}");
}

#[test]
fn missing_module_is_a_clean_error() {
    let (_, stderr, code) =
        run_modules(&[("m.helix", "import nope\nprint(1)\n")], "m.helix", &[], "missing");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("cannot find module"), "stderr: {stderr:?}");
}

#[test]
fn import_alias_renames_the_namespace() {
    let lib = "fn mean2(a, b) = (a + b) / 2\nPI = 3\n";
    // `as st` makes the module reachable as `st`, not `stats`.
    let main = "import stats as st\nprint(st.mean2(2, 4))\nprint(st.PI)\n";
    let (out, stderr, code) =
        run_modules(&[("stats.helix", lib), ("main.helix", main)], "main.helix", &[], "alias");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3.0\n3", "got: {out:?}"); // division always yields Float
    // The bare module name is NOT in scope when aliased.
    let main_bad = "import stats as st\nprint(stats.PI)\n";
    let (_, stderr2, code2) = run_modules(
        &[("stats.helix", lib), ("main.helix", main_bad)],
        "main.helix",
        &[],
        "alias_bare",
    );
    assert_ne!(code2, Some(0), "bare name should not resolve when aliased");
    assert!(!stderr2.is_empty());
}

#[test]
fn subdirectory_import_resolves_nested_path() {
    // `import lib.stats` resolves to the nested file `lib/stats.helix`.
    let dir = std::env::temp_dir().join("helix_mod_subdir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(dir.join("lib").join("stats.helix"), "fn mean2(a, b) = (a + b) / 2\n").unwrap();
    std::fs::write(dir.join("main.helix"), "import lib.stats\nprint(stats.mean2(10, 20))\n").unwrap();
    let entry = dir.join("main.helix");
    let (vm, stderr, code) = run(&[entry.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(vm.trim(), "15.0", "got: {vm:?}"); // division always yields Float
    // Both engines agree.
    let (tw, _, _) = run(&[entry.to_str().unwrap()], &[("HELIX_NOVM", "1")], "");
    assert_eq!(vm, tw, "VM and tree-walker disagree on a subdirectory import");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cli_subcommands_work() {
    // `helix eval "<code>"`
    let (out, _, code) = run(&["eval", "print(6 * 7)"], &[], "");
    assert_eq!(code, Some(0), "eval failed");
    assert_eq!(out.trim(), "42");

    // `helix version`
    let (vout, _, vcode) = run(&["version"], &[], "");
    assert_eq!(vcode, Some(0));
    assert!(vout.contains("helix"), "version: {vout:?}");

    // `helix run <file>` matches the bare-path shorthand.
    let path = std::env::temp_dir().join("helix_cli_run.helix");
    std::fs::write(&path, "print(\"hi\")\n").unwrap();
    let (rout, _, rcode) = run(&["run", path.to_str().unwrap()], &[], "");
    let _ = std::fs::remove_file(&path);
    assert_eq!(rcode, Some(0));
    assert_eq!(rout.trim(), "hi");
}

#[cfg(not(feature = "managed"))]
#[test]
fn python_subcommand_without_managed_feature_errors() {
    // A default build still parses `helix python …` but explains how to enable it.
    let (_, stderr, code) = run(&["python", "install"], &[], "");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("managed-runtime support"), "stderr: {stderr:?}");
}

#[cfg(feature = "managed")]
#[test]
fn python_dir_prints_the_managed_runtime_path() {
    // Offline command — no download. (`install` needs network, so it isn't tested here.)
    let (out, stderr, code) = run(&["python", "dir"], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert!(out.contains("helix"), "expected a .../helix/python path, got: {out:?}");
}

#[test]
fn json_round_trips_through_the_cli() {
    // Build a record (no string braces → no interpolation snag), serialize, re-parse,
    // and access fields — exercises to_json + parse_json + record access end to end.
    let src = "r = {a: 1, b: [2, 3]}\ns = to_json(r)\nprint(s)\nd = parse_json(s)\nprint(d.a)\nprint(d.b.sum())\n";
    let (out, stderr, code) = run_source(src, &[], "json");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "{\"a\":1,\"b\":[2,3]}");
    assert_eq!(lines[1], "1"); // d.a
    assert_eq!(lines[2], "5"); // d.b.sum()
}

#[test]
fn try_catches_runtime_errors() {
    // `try EXPR` yields {ok, value, error}; a runtime error is caught (not aborting),
    // and recovery composes with `??`.
    let src = concat!(
        "ok = try (10 * 2)\n",
        "print(ok.ok)\n",                                   // true
        "print(ok.value)\n",                                // 20
        "bad = try [1, 2, 3][99]\n",
        "print(bad.ok)\n",                                  // false (out-of-bounds caught)
        "v = (try parse_json(\"[1,\")).value ?? \"fallback\"\n",
        "print(v)\n",                                       // fallback
        "print(\"continues\")\n",                           // program did not abort
    );
    let (out, stderr, code) = run_source(src, &[], "try");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "true\n20\nfalse\nfallback\ncontinues");
}

#[test]
fn record_string_indexing() {
    // `r["key"]` — dynamic field access; an absent key is `missing` (the optional
    // accessor). Useful for JSON whose keys aren't valid identifiers.
    let src = "r = {a: 1, b: 2}\nprint(r[\"a\"])\nprint(r[\"b\"])\nprint(r[\"z\"].is_missing())\n";
    let (out, stderr, code) = run_source(src, &[], "recidx");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "1\n2\ntrue");
}

#[test]
fn read_fastq_parses_reads_with_quality() {
    // FASTQ -> records {id, seq, qual, length}; sequence methods apply to `seq`.
    let src = "r = read_fastq(\"examples/data/reads.fastq\")\nprint(r.count())\nprint(r.first().length)\nprint(r.first().seq.gc_content())\n";
    let (out, stderr, code) = run_source(src, &[], "fastq");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n12\n0.5"); // 3 reads; first is 12 bp; GC = 0.5
}

#[test]
fn read_vcf_makes_variants_queryable() {
    // The bio flagship: a VCF becomes a DataFrame the normal verbs work on. INFO
    // fields (gene) are columns alongside the fixed ones (qual). No group-by here, so
    // counts are deterministic.
    let src = "v = read_vcf(\"examples/data/variants.vcf\")\nprint(v.count())\nprint(v.where(gene == \"BRCA1\").count())\nprint(v.where(qual > 50).count())\n";
    let (out, stderr, code) = run_source(src, &[], "vcf");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n3\n3"); // 6 variants; 3 in BRCA1; 3 with qual > 50
}

#[test]
fn with_derives_columns_from_expressions() {
    // `df.with({name: expr, ...})` adds columns computed over existing ones. The
    // value expressions reference bare column names, like the other column verbs.
    let src = "v = read_vcf(\"examples/data/variants.vcf\")\nd = v.with({strong: qual > 50})\nprint(d.where(strong).count())\n";
    let (out, stderr, code) = run_source(src, &[], "with");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3"); // 3 of 6 variants have qual > 50
}

#[test]
fn join_combines_frames_on_a_key() {
    // `a.join(b, key)` defaults to an inner join; a trailing string picks the type.
    // samples has S1..S4; sample_meta has S1..S3, S5 — so inner keeps 3, left keeps 4.
    let src = "s = read_csv(\"examples/data/samples.csv\")\nm = read_csv(\"examples/data/sample_meta.csv\")\nprint(s.join(m, sample_id).count())\nprint(s.join(m, sample_id, \"left\").count())\n";
    let (out, stderr, code) = run_source(src, &[], "join");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n4");
}

#[test]
fn descriptive_statistics_and_correlation() {
    // Population statistics (so var == std^2) plus Pearson correlation, with the
    // missing-propagation rule: a `missing` in either series yields `missing`.
    let src = "xs = [2, 4, 4, 4, 5, 5, 7, 9]\nprint(xs.median())\nprint(xs.var())\nprint(xs.std())\nprint(correlation([1, 2, 3, 4], [2, 4, 6, 8]))\nprint(correlation([1, 2, 3], [1, missing, 3]))\n";
    let (out, stderr, code) = run_source(src, &[], "stats");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4.5\n4.0\n2.0\n1.0\nmissing");
}

#[test]
fn inferential_statistics_t_test_and_normal() {
    // The normal CDF (broadcasting math) and Welch's two-sample t-test. The t-test
    // returns a {statistic, df, p_value} record whose fields are reachable.
    let src = "print(normal_cdf(0.0))\ncontrol = [5.1, 4.9, 5.0, 5.2, 4.8, 5.0]\ntreated = [5.6, 5.8, 5.5, 5.9, 5.7, 5.4]\nr = t_test(control, treated)\nprint(r.p_value < 0.01)\nprint(r.statistic < 0.0)\n";
    let (out, stderr, code) = run_source(src, &[], "ttest");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "0.5\ntrue\ntrue"); // strong, significant difference
}

#[test]
fn t_test_on_constant_samples_is_a_clean_error() {
    let src = "print(t_test([2, 2, 2], [2, 2, 2]))\n";
    let (_out, stderr, code) = run_source(src, &[], "ttesterr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("t-test is undefined"), "stderr:\n{stderr}");
}

#[test]
fn linear_regression_fits_and_predicts() {
    // OLS fit of a textbook dataset (R: intercept 2.2, slope 0.6, R^2 0.6), with
    // predictions recovered by broadcasting `slope * x + intercept`.
    let src = "x = [1.0, 2.0, 3.0, 4.0, 5.0]\ny = [2.0, 4.0, 5.0, 4.0, 5.0]\nf = linear_regression(x, y)\nprint(f.slope)\nprint(f.intercept)\nprint(f.r_squared)\nprint(f.slope * 6.0 + f.intercept)\n";
    let (out, stderr, code) = run_source(src, &[], "lm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "0.6\n2.2\n0.6\n5.8"); // predicted y at x = 6
}

#[test]
fn linear_regression_without_variance_is_a_clean_error() {
    let src = "print(linear_regression([1, 1, 1], [1, 2, 3]))\n";
    let (_out, stderr, code) = run_source(src, &[], "lmerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("linear regression is undefined"), "stderr:\n{stderr}");
}

#[test]
fn multiple_regression_recovers_coefficients() {
    // y = 1 + 2*x1 + 3*x2 exactly → coefficients [1, 2, 3], R^2 = 1. The result's
    // coefficients/p_values are parameter-indexed arrays (index 0 is the intercept).
    let src = "x1 = [1.0, 2.0, 3.0, 4.0, 5.0]\nx2 = [2.0, 1.0, 4.0, 3.0, 5.0]\ny = [9.0, 8.0, 19.0, 18.0, 26.0]\nf = multiple_regression([x1, x2], y)\nc = f.coefficients\nprint(c.count())\nprint(f.r_squared)\nprint(round(c[0]) == 1 and round(c[1]) == 2 and round(c[2]) == 3)\n";
    let (out, stderr, code) = run_source(src, &[], "mlr");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n1.0\ntrue"); // 3 coefficients; perfect fit; b = [1, 2, 3]
}

#[test]
fn multiple_regression_on_collinear_predictors_is_a_clean_error() {
    let src = "print(multiple_regression([[1, 2, 3, 4], [2, 4, 6, 8]], [1, 3, 2, 5]))\n";
    let (_out, stderr, code) = run_source(src, &[], "mlrerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("multiple regression is undefined"), "stderr:\n{stderr}");
}

#[test]
fn column_extracts_values_for_statistics() {
    // `df.column(name)` materializes a column as an array, so the array statistics
    // apply directly to loaded data. Polars nulls become `missing`, so `drop_missing`
    // composes before an aggregation.
    let src = "p = read_csv(\"examples/data/patients.csv\")\nprint(p.column(\"age\").median())\nv = read_vcf(\"examples/data/variants.vcf\")\nprint(v.column(\"qual\").drop_missing().count())\n";
    let (out, stderr, code) = run_source(src, &[], "dfcolumn");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "43.0\n6"); // median of 8 ages; 6 non-null quals
}

#[test]
fn column_with_unknown_name_is_a_clean_error() {
    let src = "print(read_csv(\"examples/data/patients.csv\").column(\"nope\"))\n";
    let (_out, stderr, code) = run_source(src, &[], "dfcolerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("no column `nope`"), "stderr:\n{stderr}");
}

#[test]
fn correlation_on_mismatched_lengths_is_a_clean_error() {
    let src = "print(correlation([1, 2], [1, 2, 3]))\n";
    let (_out, stderr, code) = run_source(src, &[], "corrlen");
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("equal-length arrays"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn join_without_an_operand_is_a_clean_error() {
    // A no-argument `join` type-checks (DataFrame args are the unchecked runtime
    // boundary), so the compiler must stay total and emit the friendly diagnostic
    // rather than the "internal error ... please report" totality breach.
    let src = "s = read_csv(\"examples/data/samples.csv\")\nprint(s.join())\n";
    let (out, stderr, code) = run_source(src, &[], "joinerr");
    assert_ne!(code, Some(0), "stdout:\n{out}");
    assert!(
        stderr.contains("`join` needs a DataFrame to join with"),
        "stderr:\n{stderr}"
    );
    assert!(!stderr.contains("internal error"), "stderr:\n{stderr}");
}

#[test]
fn join_on_an_unknown_key_is_a_clean_error() {
    // Keys are validated against both schemas up front, so a typo reads as a Helix
    // error naming the frame and listing valid columns — not Polars' lazy-plan dump.
    let src = "s = read_csv(\"examples/data/samples.csv\")\nm = read_csv(\"examples/data/sample_meta.csv\")\nprint(s.join(m, no_such_key).count())\n";
    let (out, stderr, code) = run_source(src, &[], "joinkey");
    assert_ne!(code, Some(0), "stdout:\n{out}");
    assert!(
        stderr.contains("no column `no_such_key` in the left frame"),
        "stderr:\n{stderr}"
    );
}

// Real network fetch — ignored by default so the suite stays offline-friendly.
// Run with: `cargo test -- --ignored`.
#[cfg(feature = "http")]
#[test]
#[ignore]
fn http_get_returns_a_status() {
    let src = "r = http_get(\"https://example.com\")\nprint(r.status)\n";
    let (out, stderr, code) = run_source(src, &[], "http");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "200");
}

// --- Python interop (Phase 6) ------------------------------------------------
// These run against whichever feature set the suite was built with: the
// feature-gated tests need `cargo test --features python` (and a Python
// interpreter on the box); the default build instead asserts the friendly
// "rebuild with --features python" error.

fn run_script(src: &str, env: &[(&str, &str)], tag: &str) -> (String, String, Option<i32>) {
    let dir = std::env::temp_dir().join(format!("helix_py_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.helix");
    std::fs::write(&path, src).unwrap();
    let r = run(&[path.to_str().unwrap()], env, "");
    let _ = std::fs::remove_dir_all(&dir);
    r
}

#[cfg(feature = "python")]
#[test]
fn python_import_math_on_both_engines() {
    // Both surface syntaxes: the statement form (sugar) and the expression form.
    let src = "import python.math as m\nprint(m.sqrt(16.0))\nmod = python.import(\"math\")\nprint(mod.gcd(12, 18))\n";
    let (vm, stderr, code) = run_script(src, &[], "math_vm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(vm.trim(), "4.0\n6", "got: {vm:?}");
    let (tw, _, _) = run_script(src, &[("HELIX_NOVM", "1")], "math_tw");
    assert_eq!(vm, tw, "VM and tree-walker disagree on Python interop");
}

#[cfg(feature = "python")]
#[test]
fn python_object_is_opaque_until_to_array() {
    // A Python list stays an opaque PyObject (NOT silently an Array); `to_array`
    // is the explicit, on-demand materialization into a native Helix Array.
    let src = "import python.builtins as b\nxs = b.list(b.range(0, 4))\nprint(xs)\nprint(to_array(xs).sum())\n";
    let (out, stderr, code) = run_script(src, &[], "opaque");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "<python list>\n6", "got: {out:?}");
}

#[cfg(feature = "python")]
#[test]
fn python_exception_becomes_a_helix_error() {
    let src = "m = python.import(\"no_such_module_xyz\")\nprint(m)\n";
    let (_, stderr, code) = run_script(src, &[], "pymiss");
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("python error: ModuleNotFoundError"),
        "stderr: {stderr:?}"
    );
}

#[cfg(feature = "python")]
#[test]
fn python_dataframe_round_trips_zero_copy() {
    // A Helix DataFrame flows out to Python's polars (len() = rows) and back via
    // `to_dataframe`, becoming a first-class Helix DataFrame again. Needs the Python
    // `polars` package; skip cleanly if it isn't installed so the suite stays portable.
    // The relative CSV path resolves because `run` sets cwd to the manifest dir.
    let src = concat!(
        "df = read_csv(\"examples/data/patients.csv\")\n",
        "print(python.import(\"builtins\").len(df))\n",
        "back = to_dataframe(python.import(\"polars\").concat([df]))\n",
        "print(back.count())\n",
    );
    let (out, stderr, code) = run_script(src, &[], "dfroundtrip");
    if stderr.contains("No module named 'polars'") {
        eprintln!("skipping python_dataframe_round_trips_zero_copy: Python polars not installed");
        return;
    }
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "8\n8", "got: {out:?}"); // 8 patients, preserved across the round-trip
}

#[cfg(feature = "python")]
#[test]
fn python_tensor_round_trips_via_numpy() {
    // A Helix Tensor crosses to NumPy and back via `to_tensor`, becoming a
    // first-class Helix Tensor again. Needs Python `numpy`; skip if it's absent.
    let src = concat!(
        "t = tensor([[1.0, 2.0], [3.0, 4.0]])\n",
        "np = python.import(\"numpy\")\n",
        "print(np.sum(t))\n",                  // to_py: Tensor -> NumPy -> scalar
        "print(to_tensor(np.transpose(t)).shape())\n", // round-trip -> native verb
    );
    let (out, stderr, code) = run_script(src, &[], "tensorroundtrip");
    if stderr.contains("No module named 'numpy'") {
        eprintln!("skipping python_tensor_round_trips_via_numpy: Python numpy not installed");
        return;
    }
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "10.0\n[2, 2]", "got: {out:?}");
}

#[cfg(not(feature = "python"))]
#[test]
fn python_without_feature_errors_with_rebuild_hint() {
    // The default build has the `python` global and parses `python.import`, but
    // calling it fails loudly with a build hint — never a cryptic runtime crash.
    let src = "m = python.import(\"math\")\nprint(m)\n";
    let (_, stderr, code) = run_script(src, &[], "nopy");
    assert_ne!(code, Some(0));
    assert!(
        stderr.contains("without Python support"),
        "stderr: {stderr:?}"
    );
}
