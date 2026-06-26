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
fn cross_module_runtime_error_points_at_the_dependency() {
    // A runtime error inside an imported module must render against that module's own
    // file and local line — not the entry file. `boom` is on line 2 of lib.helix.
    let lib = "# lib\nfn boom(n) = [10, 20, 30][n]\n";
    let main = "import lib\nprint(\"start\")\nprint(lib.boom(99))\n";
    let (_out, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "caret");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("lib.helix:2:"), "should point at lib.helix line 2:\n{stderr}");
    assert!(stderr.contains("[10, 20, 30][n]"), "should show lib's source line:\n{stderr}");
    assert!(!stderr.contains("main.helix"), "must not point at the entry file:\n{stderr}");
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
    let src = "r = {a: 1, b: [2, 3]}\ns = json.stringify(r)\nprint(s)\nd = json.parse(s)\nprint(d.a)\nprint(d.b.sum())\n";
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
        "v = (try json.parse(\"[1,\")).value ?? \"fallback\"\n",
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
    let src = "r = bio.read_fastq(\"examples/data/reads.fastq\")\nprint(r.count())\nprint(r.first().length)\nprint(r.first().seq.gc_content())\n";
    let (out, stderr, code) = run_source(src, &[], "fastq");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n12\n0.5"); // 3 reads; first is 12 bp; GC = 0.5
}

#[test]
fn read_vcf_accepts_gzipped_files() {
    // Real-world VCFs are bgzipped `.vcf.gz`; the reader sniffs the gzip magic bytes
    // and decompresses transparently, so a `.vcf.gz` queries identically to its plain
    // form (the fixture is the gzip of examples/data/variants.vcf).
    let src = "v = bio.read_vcf(\"examples/data/variants.vcf.gz\")\nprint(v.count())\nprint(v.where(gene == \"BRCA1\").count())\n";
    let (out, stderr, code) = run_source(src, &[], "vcfgz");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n3"); // identical to the plain-VCF result
}

#[test]
fn read_vcf_makes_variants_queryable() {
    // The bio flagship: a VCF becomes a DataFrame the normal verbs work on. INFO
    // fields (gene) are columns alongside the fixed ones (qual). No group-by here, so
    // counts are deterministic.
    // `af` is a header-typed Float INFO column, so `af > 0.001` is a NUMERIC
    // comparison (3 rows) — a plain string column would mis-compare and give 5.
    let src = "v = bio.read_vcf(\"examples/data/variants.vcf\")\nprint(v.count())\nprint(v.where(gene == \"BRCA1\").count())\nprint(v.where(qual > 50).count())\nprint(v.where(af > 0.001).count())\n";
    let (out, stderr, code) = run_source(src, &[], "vcf");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n3\n3\n3"); // 6 variants; 3 BRCA1; 3 qual>50; 3 af>0.001
}

#[test]
fn read_bcf_queries_identically_to_read_vcf() {
    // BCF is the binary, BGZF-framed form of VCF. read_bcf shares read_vcf's record
    // model and column-building, so the same queries over the binary fixture must
    // give the SAME answers as the text VCF (including the header-typed Float `af`
    // column, so `af > 0.001` stays a numeric comparison). The fixture is generated
    // from variants.vcf by the ignored `generate_bcf_fixture` test in src/vcf.rs.
    let src = "b = bio.read_bcf(\"examples/data/variants.bcf\")\nprint(b.count())\nprint(b.where(gene == \"BRCA1\").count())\nprint(b.where(qual > 50).count())\nprint(b.where(af > 0.001).count())\n";
    let (out, stderr, code) = run_source(src, &[], "bcf");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "6\n3\n3\n3"); // identical to the plain-VCF result above
}

#[test]
fn read_vcf_region_query_uses_the_index() {
    // The local-first capability: `read_vcf(path, region)` seeks via the `.tbi` index
    // and returns only the variants intersecting the region, identical to a full read
    // filtered to that window (INFO columns preserved). The bgzipped+indexed fixture is
    // generated from variants.vcf by the ignored `generate_vcf_index_fixture` test.
    let src = "p = \"examples/data/variants.vcf.gz\"\nprint(bio.read_vcf(p, \"chr17:43044000-43046000\").count())\nprint(bio.read_vcf(p, \"chr13\").count())\nprint(bio.read_vcf(p, \"chr17:43090000-43100000\").select(pos, gene).column(\"pos\").first())\nprint(bio.read_vcf(p).count())\n";
    let (out, stderr, code) = run_source(src, &[], "vcfregion");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    // 2 in the chr17 window; 2 on chr13; the tail window's single variant is at 43091983;
    // a plain read of the same (now BGZF) file still scans all 6.
    assert_eq!(out.trim(), "2\n2\n43091983\n6");
}

#[test]
fn read_vcf_region_without_index_is_a_clean_error() {
    // A region query against a file with no `.tbi` (here the plain, unindexed .vcf)
    // fails with a clear message rather than a panic.
    let src = "print(bio.read_vcf(\"examples/data/variants.vcf\", \"chr17:1-9999999\").count())\n";
    let (_out, stderr, code) = run_source(src, &[], "vcfnoidx");
    assert_eq!(code, Some(1));
    assert!(stderr.contains("indexed") || stderr.contains(".tbi"), "stderr:\n{stderr}");
}

#[test]
fn read_sam_makes_alignments_queryable() {
    // The alignment flagship: a SAM file becomes a DataFrame with the eleven mandatory
    // fields as columns. `ref` is resolved from the header (null for an unmapped read),
    // `mapq` is a numeric column, and the CIGAR is rendered to its SAM string.
    let src = "a = bio.read_sam(\"examples/data/alignments.sam\")\nprint(a.count())\nprint(a.where(ref == \"chr1\").count())\nprint(a.where(mapq > 50).count())\nprint(a.where(name == \"read2\").column(\"cigar\").first())\n";
    let (out, stderr, code) = run_source(src, &[], "sam");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4\n2\n2\n5M2I1M"); // 4 reads; 2 on chr1; 2 mapq>50; read2 CIGAR
}

#[test]
fn read_bam_queries_identically_to_read_sam() {
    // BAM is the binary, BGZF-framed form of SAM. read_bam shares read_sam's record
    // model and column-building, so the same queries over the binary fixture give the
    // SAME answers. The fixture is generated from alignments.sam by the ignored
    // `generate_bam_fixture` test in src/sam.rs.
    let src = "b = bio.read_bam(\"examples/data/alignments.bam\")\nprint(b.count())\nprint(b.where(ref == \"chr1\").count())\nprint(b.where(mapq > 50).count())\nprint(b.where(name == \"read2\").column(\"cigar\").first())\n";
    let (out, stderr, code) = run_source(src, &[], "bam");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4\n2\n2\n5M2I1M"); // identical to the plain-SAM result above
}

#[test]
fn read_bam_region_query_uses_the_index() {
    // The local-first capability for alignments: `read_bam(path, region)` seeks via the
    // `.bai` index and returns only the reads intersecting the region (by CIGAR-spanned
    // reference coordinates), identical to a full read filtered to the window. The
    // indexed BAM+`.bai` fixture is generated by the ignored `generate_bam_fixture` test.
    let src = "p = \"examples/data/alignments.bam\"\nprint(bio.read_bam(p, \"chr1\").count())\nprint(bio.read_bam(p, \"chr2\").count())\nprint(bio.read_bam(p, \"chr1:140-160\").count())\nprint(bio.read_bam(p).count())\n";
    let (out, stderr, code) = run_source(src, &[], "bamregion");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    // 2 reads on chr1; 1 on chr2; 1 read (read2 @150) spans chr1:140-160; scan reads all 4.
    assert_eq!(out.trim(), "2\n1\n1\n4");
}

#[test]
fn read_gff_makes_features_queryable() {
    // A GFF3 file becomes a DataFrame: the standard feature columns plus one string
    // column per attribute tag (so `Name` is queryable alongside `type`/`strand`).
    let src = "g = bio.read_gff(\"examples/data/genes.gff3\")\nprint(g.where(type == \"gene\").count())\nprint(g.where(Name == \"BRCA1\").count())\n";
    let (out, stderr, code) = run_source(src, &[], "gff");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n1"); // 3 gene features; 1 named BRCA1
}

#[test]
fn read_bed_makes_intervals_queryable() {
    // A BED file becomes a DataFrame; the optional name/score/strand columns appear
    // because the file carries them, and `score` is numeric (`score > 400`).
    let src = "b = bio.read_bed(\"examples/data/peaks.bed\")\nprint(b.count())\nprint(b.where(score > 400).count())\n";
    let (out, stderr, code) = run_source(src, &[], "bed");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4\n3"); // 4 intervals; 3 with score > 400
}

#[test]
fn higher_order_functions_work_on_both_engines() {
    // A function-valued parameter is callable (`f(x)`): the gradual checker permits
    // an Unknown-typed name as a call target. Runtime already supported it — this
    // pins the checker fix and VM/tree-walker agreement through the real CLI.
    let src = "fn inc(n) = n + 1\nfn apply(f, x) = f(x)\nfn twice(f, x) = f(f(x))\nprint(apply(inc, 5))\nprint(apply((n => n * 2), 5))\nprint(twice(inc, 5))\n";
    let (vm, e1, c1) = run_source(src, &[], "hof_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "6\n10\n7"); // apply(inc,5); apply(double,5); twice(inc,5)
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "hof_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on higher-order functions");
}

#[test]
fn closures_capture_on_both_engines() {
    // Standalone closures that capture an enclosing local — returned, stored, called
    // later — work identically on the VM (upvalues) and the tree-walker (env capture),
    // including capturing function-valued params and two-level nesting.
    let src = concat!(
        "fn make(k) = (p => p + k)\n",
        "g = make(10)\n",
        "print(g(5))\n",
        "fn inc(n) = n + 1\n",
        "fn dbl(n) = n * 2\n",
        "fn compose(f, h) = (x => f(h(x)))\n",
        "comp = compose(inc, dbl)\n",
        "print(comp(10))\n",
        "fn outer(a) = (b => (cc => a + b + cc))\n",
        "p1 = outer(1)\n",
        "p2 = p1(2)\n",
        "print(p2(3))\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "clo_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "15\n21\n6"); // make+10 then +5; inc(dbl(10)); 1+2+3
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "clo_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on closures");
}

#[test]
fn match_works_on_both_engines() {
    // `match` with literal arms + wildcard, a binding pattern (and recursion through
    // arms), and the `missing` pattern — identical on the VM (compiled to test/jump
    // ops sharing the tree-walker's matcher) and the tree-walker.
    let src = concat!(
        "print(match 2 { 1 => \"one\", 2 => \"two\", _ => \"other\" })\n",
        "fn fib(n) = match n { 0 => 0, 1 => 1, _ => fib(n - 1) + fib(n - 2) }\n",
        "print(fib(10))\n",
        "print(match missing { missing => \"absent\", _ => \"present\" })\n",
        "print(match 42 { x => x + 1 })\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "match_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "two\n55\nabsent\n43");
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "match_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on `match`");
}

#[test]
fn match_nested_patterns_on_both_engines() {
    // Tuple + record patterns (with a partial match), and the killer case:
    // destructuring a `try` result. Identical on both engines.
    let src = concat!(
        "print(match (1, 2) { (a, b) => a + b })\n",
        "print(match {a: 1, b: 2} { {b: x} => x, _ => 0 })\n",
        "fn unwrap(r) = match r { {ok: true, value: v} => v, _ => -1 }\n",
        "print(unwrap(try (20 / 4)))\n",
        "print(unwrap(try (1 / 0)))\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "matchn_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "3\n2\n5.0\n-1"); // tuple sum; record field; try ok; try err
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "matchn_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on nested patterns");
}

#[test]
fn match_or_patterns_on_both_engines() {
    // `a | b | c` matches if any alternative does; composes inside a tuple pattern
    // (with a sibling binding). Identical on both engines.
    let src = concat!(
        "print(match 2 { 1 | 2 | 3 => \"low\", _ => \"high\" })\n",
        "print(match 9 { 1 | 2 | 3 => \"low\", _ => \"high\" })\n",
        "print(match (1, 5) { (1 | 2, x) => x, _ => 0 })\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "matchor_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "low\nhigh\n5"); // in-set; not-in-set; or inside a tuple
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "matchor_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on or-patterns");
}

#[test]
fn match_guards_on_both_engines() {
    // `pat if cond => ...` — an arm is taken only if the guard (with the pattern's
    // bindings in scope) holds, else the next arm is tried. Identical on both engines.
    let src = concat!(
        "print(match 5 { n if n > 3 => \"big\", _ => \"small\" })\n",
        "print(match 2 { n if n > 3 => \"big\", _ => \"small\" })\n",
        "print(match (1, 2) { (a, b) if a < b => \"asc\", _ => \"other\" })\n",
        "print(match try (10 / 2) { {ok: true, value: v} if v > 3 => \"big\", _ => \"other\" })\n",
    );
    let (vm, e1, c1) = run_source(src, &[], "matchg_vm");
    assert_eq!(c1, Some(0), "stderr:\n{e1}");
    assert_eq!(vm.trim(), "big\nsmall\nasc\nbig"); // guard true; false; tuple-bind guard; try+guard
    let (tw, _, c2) = run_source(src, &[("HELIX_NOVM", "1")], "matchg_tw");
    assert_eq!(c2, Some(0));
    assert_eq!(vm, tw, "VM and tree-walker disagree on match guards");
}

#[test]
fn with_derives_columns_from_expressions() {
    // `df.with({name: expr, ...})` adds columns computed over existing ones. The
    // value expressions reference bare column names, like the other column verbs.
    let src = "v = bio.read_vcf(\"examples/data/variants.vcf\")\nd = v.with({strong: qual > 50})\nprint(d.where(strong).count())\n";
    let (out, stderr, code) = run_source(src, &[], "with");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3"); // 3 of 6 variants have qual > 50
}

#[test]
fn join_combines_frames_on_a_key() {
    // `a.join(b, key)` defaults to an inner join; a trailing string picks the type.
    // samples has S1..S4; sample_meta has S1..S3, S5 — so inner keeps 3, left keeps 4.
    let src = "s = io.read_csv(\"examples/data/samples.csv\")\nm = io.read_csv(\"examples/data/sample_meta.csv\")\nprint(s.join(m, sample_id).count())\nprint(s.join(m, sample_id, \"left\").count())\n";
    let (out, stderr, code) = run_source(src, &[], "join");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n4");
}

#[test]
fn import_resolves_on_the_search_path() {
    // A module that is not beside the script resolves via `HELIX_PATH` — the mechanism
    // shared user libraries (and a future stdlib) rely on.
    let lib = std::env::temp_dir().join("helix_sp_lib");
    let _ = std::fs::remove_dir_all(&lib);
    std::fs::create_dir_all(lib.join("tools")).unwrap();
    std::fs::write(lib.join("tools").join("util.helix"), "fn triple(x) = x * 3\n").unwrap();
    let src = "import tools.util as u\nprint(u.triple(7))\n";
    let (out, stderr, code) =
        run_source(src, &[("HELIX_PATH", lib.to_str().unwrap())], "searchpath");
    let _ = std::fs::remove_dir_all(&lib);
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "21");
}

#[test]
fn bio_sequence_helpers_over_fastq() {
    // The native `bio.*` sequence helpers over the reads of a FASTQ file.
    let src = "r = bio.read_fastq(\"examples/data/reads.fastq\")\nseqs = r.map(x => x.seq)\nprint(bio.total_length(seqs))\nprint(bio.mean_gc(seqs) > 0.4)\n";
    let (out, stderr, code) = run_source(src, &[], "bioseq");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "36\ntrue"); // 3 reads x 12 bp; mean GC ~0.44
}

#[test]
fn selective_import_binds_names_unqualified() {
    // `import m.{a, b}` brings the chosen names into scope without the namespace.
    let lib = "fn triple(x) = x * 3\nfn quad(x) = x * 4\n";
    let main = "import lib.{triple, quad}\nprint(triple(5))\nprint(quad(2))\n";
    let (out, stderr, code) =
        run_modules(&[("lib.helix", lib), ("main.helix", main)], "main.helix", &[], "selimp");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "15\n8");
}

#[test]
fn imports_resolve_from_the_project_root() {
    // A file in a subdirectory can import a module elsewhere in the project by its
    // root-relative path — not just files sitting beside it. The root is the helix.toml
    // directory (and, with no manifest, the entry file's own directory).
    let dir = std::env::temp_dir().join("helix_rootimp");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("utils.helix"), "fn double(x) = x * 2\n").unwrap();
    // `sub/needs.helix` imports the ROOT module `utils`, which is not beside it.
    std::fs::write(
        dir.join("sub/needs.helix"),
        "import utils\nfn sext(x) = utils.double(x) * 3\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.helix"), "import sub.needs\nprint(needs.sext(2))\n").unwrap();
    let entry = dir.join("main.helix");
    let entry = entry.to_str().unwrap();

    // With a manifest (root = the helix.toml directory).
    std::fs::write(dir.join("helix.toml"), "[package]\nname = \"app\"\n").unwrap();
    let (out, stderr, code) = run(&[entry], &[], "");
    assert_eq!(code, Some(0), "with manifest; stderr:\n{stderr}");
    assert_eq!(out.trim(), "12");

    // Without a manifest (root = the entry file's directory). Still resolves.
    std::fs::remove_file(dir.join("helix.toml")).unwrap();
    let (out, stderr, code) = run(&[entry], &[], "");
    assert_eq!(code, Some(0), "without manifest; stderr:\n{stderr}");
    assert_eq!(out.trim(), "12");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn native_map_filter_kernels_agree_across_all_paths() {
    // `map`/`filter` over an Int array compile to a native JIT kernel on the VM. The
    // result must be byte-identical across the VM native kernel (default), the
    // tree-walker oracle (HELIX_NOVM), and the bytecode loop (HELIX_NOJIT).
    let src = "xs = range(0, 1000)\n\
               m = xs.map(x => x * x - 3 * x + 1)\n\
               f = xs.filter(x => x % 7 == 0)\n\
               g = xs.map(x => if x > 500 then x * 2 else 0 - x)\n\
               print(m.sum())\nprint(f.count())\nprint(f.sum())\nprint(g.sum())\n";
    let (vm, stderr, code) = run_source(src, &[], "kern_vm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let (tw, _, _) = run_source(src, &[("HELIX_NOVM", "1")], "kern_tw");
    let (nojit, _, _) = run_source(src, &[("HELIX_NOJIT", "1")], "kern_nojit");
    assert_eq!(vm, tw, "native kernel vs tree-walker oracle");
    assert_eq!(vm, nojit, "native kernel vs bytecode loop");
    assert!(!vm.trim().is_empty());
}

#[test]
fn fused_pipelines_match_the_oracle() {
    // A chain of map/filter (± a reduce sink) over an Int source compiles to ONE native
    // loop with no intermediate arrays. The fused result must be byte-identical to the
    // tree-walker (which materializes every stage) and the bytecode loop.
    let cases = [
        // filter→map (array, Collect)
        "print([1,2,3,4,5,6,7,8,9,10].filter(x => x % 2 == 0).map(x => x * x))",
        // map→filter→map (3 stages, Collect)
        "print([1,2,3,4,5,6,7,8].map(x => x + 1).filter(x => x > 4).map(x => x * 10))",
        // range→map→filter→reduce (the zero-allocation scalar pipeline)
        "print(range(0, 200).map(x => x * x).filter(x => x % 3 == 0).reduce(0, (a, x) => a + x))",
        // array→map→reduce (1 stage + reduce)
        "print([1,2,3,4,5].map(x => x * 2).reduce(0, (a, x) => a + x))",
        // range→filter→map→reduce
        "print(range(1, 500).filter(x => x % 7 == 0).map(x => x - 1).reduce(0, (a, x) => a + x))",
        // filter→count (the zero-allocation counting sink)
        "print([1,2,3,4,5,6,7,8,9,10].filter(x => x % 2 == 0).count())",
        // range→map→filter→count
        "print(range(0, 100).map(x => x * x).filter(x => x % 3 == 0).count())",
    ];
    for (i, src) in cases.iter().enumerate() {
        let (vm, stderr, code) = run_source(src, &[], &format!("fuse_vm{i}"));
        assert_eq!(code, Some(0), "case {i} stderr:\n{stderr}");
        let (tw, _, _) = run_source(src, &[("HELIX_NOVM", "1")], &format!("fuse_tw{i}"));
        let (nojit, _, _) = run_source(src, &[("HELIX_NOJIT", "1")], &format!("fuse_nj{i}"));
        assert_eq!(vm, tw, "case {i}: fused vs tree-walker:\n{src}");
        assert_eq!(vm, nojit, "case {i}: fused vs bytecode:\n{src}");
    }
}

#[test]
fn kernel_bodies_can_call_helper_functions() {
    // A kernel/fused body may call JIT-eligible user functions — the function is compiled
    // natively and called from inside the loop. Must match the oracle on every path.
    let cases = [
        "fn sq(x) = x * x\nprint([1,2,3,4,5].map(x => sq(x)))",
        "fn g(x) = x * 3\nfn f(x) = g(x) + 1\nprint([1,2,3,4].map(x => f(x)).filter(x => x % 2 == 0))",
        "fn sq(x) = x * x\nprint([1,2,3,4,5,6].filter(x => sq(x) > 9))",
        "fn dbl(x) = x * 2\nprint(range(0, 50).map(x => dbl(x)).filter(x => x > 30).reduce(0, (a, x) => a + x))",
    ];
    for (i, src) in cases.iter().enumerate() {
        let (vm, stderr, code) = run_source(src, &[], &format!("fnk_vm{i}"));
        assert_eq!(code, Some(0), "case {i} stderr:\n{stderr}");
        let (tw, _, _) = run_source(src, &[("HELIX_NOVM", "1")], &format!("fnk_tw{i}"));
        let (nojit, _, _) = run_source(src, &[("HELIX_NOJIT", "1")], &format!("fnk_nj{i}"));
        assert_eq!(vm, tw, "case {i}: native (fn-call) vs tree-walker:\n{src}");
        assert_eq!(vm, nojit, "case {i}: native (fn-call) vs bytecode:\n{src}");
    }
}

#[test]
fn ineligible_map_bodies_fall_through_correctly() {
    // A float array (no Int kernel) and a non-arithmetic body both bypass the kernel
    // and run the bytecode loop — still correct, and identical to the tree-walker.
    let src = "print([1.0, 4.0, 9.0].map(x => x * 2.0).sum())\n\
               print([1,2,3].map(x => sqrt(x * 1.0)).count())\n";
    let (vm, stderr, code) = run_source(src, &[], "fall_vm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let (tw, _, _) = run_source(src, &[("HELIX_NOVM", "1")], "fall_tw");
    assert_eq!(vm, tw);
    assert_eq!(vm.trim(), "28.0\n3");
}

#[test]
fn assertions_raise_with_a_message_and_are_catchable() {
    // A passing assert is silent; a failing one raises a clean, catchable error.
    let (out, stderr, code) = run_source(
        "assert(1 < 2)\nr = try assert(false, \"nope\")\nprint(r.ok)\nprint(r.error)\n",
        &[],
        "assertok",
    );
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "false\nassertion failed: nope");

    // An uncaught failure exits non-zero with the message.
    let (_o, stderr, code) = run_source("assert_eq(1, 2)\n", &[], "assertfail");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("assertion failed: 1 != 2"), "stderr:\n{stderr}");
}

#[test]
fn helix_test_runs_test_files_and_reports() {
    // `helix test` discovers `*_test.helix` files, runs each in isolation, and exits
    // non-zero iff any failed. A test passes by running to completion without raising;
    // `assert*` raise on failure.
    let dir = std::env::temp_dir().join("helix_testrun");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("math.helix"), "fn double(x) = x * 2\n").unwrap();
    // Passing: imports a project module (root-anchored), asserts.
    std::fs::write(
        dir.join("math_test.helix"),
        "import math\nfn test_double() = assert_eq(math.double(3), 6)\ntest_double()\n",
    )
    .unwrap();
    // Passing nested test (float closeness).
    std::fs::write(
        dir.join("sub/calc_test.helix"),
        "fn test_close() = assert_close(0.1 + 0.2, 0.3)\ntest_close()\n",
    )
    .unwrap();
    // A non-test file must be ignored.
    std::fs::write(dir.join("helper.helix"), "print(\"should not run\")\n").unwrap();

    // All pass → exit 0, summary present.
    let (out, stderr, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(0), "stderr:\n{stderr}\nout:\n{out}");
    assert!(out.contains("2 passed"), "out:\n{out}");
    assert!(!out.contains("should not run"), "ran a non-test file:\n{out}");

    // Add a failing test → exit 1, the failure and its assertion message are reported.
    std::fs::write(
        dir.join("broken_test.helix"),
        "fn test_bad() = assert_eq(2 + 2, 5)\ntest_bad()\n",
    )
    .unwrap();
    let (out, _stderr, code) = run(&["test", dir.to_str().unwrap()], &[], "");
    assert_eq!(code, Some(1), "a failing test must exit non-zero:\n{out}");
    assert!(out.contains("FAIL"), "out:\n{out}");
    assert!(out.contains("4 != 5"), "out:\n{out}");
    assert!(out.contains("2 passed, 1 failed"), "out:\n{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_module_on_search_path_is_a_clean_error() {
    // An import found neither locally nor on the search path fails with a clear message.
    let src = "import nowhere.lib as x\nprint(1)\n";
    let (_out, stderr, code) = run_source(src, &[], "missmod");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("cannot find module `nowhere.lib`"), "stderr:\n{stderr}");
}

#[test]
fn descriptive_statistics_and_correlation() {
    // Population statistics (so var == std^2) plus Pearson correlation, with the
    // missing-propagation rule: a `missing` in either series yields `missing`.
    let src = "xs = [2, 4, 4, 4, 5, 5, 7, 9]\nprint(xs.median())\nprint(xs.var())\nprint(xs.std())\nprint(stats.correlation([1, 2, 3, 4], [2, 4, 6, 8]))\nprint(stats.correlation([1, 2, 3], [1, missing, 3]))\n";
    let (out, stderr, code) = run_source(src, &[], "stats");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "4.5\n4.0\n2.0\n1.0\nmissing");
}

#[test]
fn inferential_statistics_t_test_and_normal() {
    // The normal CDF (broadcasting math) and Welch's two-sample t-test. The t-test
    // returns a {statistic, df, p_value} record whose fields are reachable.
    let src = "print(stats.normal_cdf(0.0))\ncontrol = [5.1, 4.9, 5.0, 5.2, 4.8, 5.0]\ntreated = [5.6, 5.8, 5.5, 5.9, 5.7, 5.4]\nr = stats.t_test(control, treated)\nprint(r.p_value < 0.01)\nprint(r.statistic < 0.0)\n";
    let (out, stderr, code) = run_source(src, &[], "ttest");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "0.5\ntrue\ntrue"); // strong, significant difference
}

#[test]
fn t_test_on_constant_samples_is_a_clean_error() {
    let src = "print(stats.t_test([2, 2, 2], [2, 2, 2]))\n";
    let (_out, stderr, code) = run_source(src, &[], "ttesterr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("t-test is undefined"), "stderr:\n{stderr}");
}

#[test]
fn linear_regression_fits_and_predicts() {
    // OLS fit of a textbook dataset (R: intercept 2.2, slope 0.6, R^2 0.6), with
    // predictions recovered by broadcasting `slope * x + intercept`.
    let src = "x = [1.0, 2.0, 3.0, 4.0, 5.0]\ny = [2.0, 4.0, 5.0, 4.0, 5.0]\nf = stats.linear_regression(x, y)\nprint(f.slope)\nprint(f.intercept)\nprint(f.r_squared)\nprint(f.slope * 6.0 + f.intercept)\n";
    let (out, stderr, code) = run_source(src, &[], "lm");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "0.6\n2.2\n0.6\n5.8"); // predicted y at x = 6
}

#[test]
fn linear_regression_without_variance_is_a_clean_error() {
    let src = "print(stats.linear_regression([1, 1, 1], [1, 2, 3]))\n";
    let (_out, stderr, code) = run_source(src, &[], "lmerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("linear regression is undefined"), "stderr:\n{stderr}");
}

#[test]
fn multiple_regression_recovers_coefficients() {
    // y = 1 + 2*x1 + 3*x2 exactly → coefficients [1, 2, 3], R^2 = 1. The result's
    // coefficients/p_values are parameter-indexed arrays (index 0 is the intercept).
    let src = "x1 = [1.0, 2.0, 3.0, 4.0, 5.0]\nx2 = [2.0, 1.0, 4.0, 3.0, 5.0]\ny = [9.0, 8.0, 19.0, 18.0, 26.0]\nf = stats.multiple_regression([x1, x2], y)\nc = f.coefficients\nprint(c.count())\nprint(f.r_squared)\nprint(round(c[0]) == 1 and round(c[1]) == 2 and round(c[2]) == 3)\n";
    let (out, stderr, code) = run_source(src, &[], "mlr");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "3\n1.0\ntrue"); // 3 coefficients; perfect fit; b = [1, 2, 3]
}

#[test]
fn multiple_regression_on_collinear_predictors_is_a_clean_error() {
    let src = "print(stats.multiple_regression([[1, 2, 3, 4], [2, 4, 6, 8]], [1, 3, 2, 5]))\n";
    let (_out, stderr, code) = run_source(src, &[], "mlrerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("multiple regression is undefined"), "stderr:\n{stderr}");
}

#[test]
fn column_extracts_values_for_statistics() {
    // `df.column(name)` materializes a column as an array, so the array statistics
    // apply directly to loaded data. Polars nulls become `missing`, so `drop_missing`
    // composes before an aggregation.
    let src = "p = io.read_csv(\"examples/data/patients.csv\")\nprint(p.column(\"age\").median())\nv = bio.read_vcf(\"examples/data/variants.vcf\")\nprint(v.column(\"qual\").drop_missing().count())\n";
    let (out, stderr, code) = run_source(src, &[], "dfcolumn");
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    assert_eq!(out.trim(), "43.0\n6"); // median of 8 ages; 6 non-null quals
}

#[test]
fn column_with_unknown_name_is_a_clean_error() {
    let src = "print(io.read_csv(\"examples/data/patients.csv\").column(\"nope\"))\n";
    let (_out, stderr, code) = run_source(src, &[], "dfcolerr");
    assert_ne!(code, Some(0));
    assert!(stderr.contains("no column `nope`"), "stderr:\n{stderr}");
}

#[test]
fn correlation_on_mismatched_lengths_is_a_clean_error() {
    let src = "print(stats.correlation([1, 2], [1, 2, 3]))\n";
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
    let src = "s = io.read_csv(\"examples/data/samples.csv\")\nprint(s.join())\n";
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
    let src = "s = io.read_csv(\"examples/data/samples.csv\")\nm = io.read_csv(\"examples/data/sample_meta.csv\")\nprint(s.join(m, no_such_key).count())\n";
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
    let src = "r = http.get(\"https://example.com\")\nprint(r.status)\n";
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
        "df = io.read_csv(\"examples/data/patients.csv\")\n",
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
