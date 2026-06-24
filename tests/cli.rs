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
        if name == "dataframes.helix" {
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
