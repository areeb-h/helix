//! `helix jit-explain <script>` — which numeric kernels the JIT compiled, and where.
//!
//! **The footgun this answers.** `AGENTS.md` lists it: falling off a JIT kernel is
//! *silent*. The answer stays correct and the program gets much slower, so the only
//! symptom is a wall-clock number the reader has nothing to compare against. Until now
//! there was no way to ask whether a hot loop had been compiled at all — the language
//! offered a large speedup, declined to give it, and said nothing.
//!
//! **Two families, not one.** The JIT compiles *kernel sites* — a `map`/`filter`/
//! `reduce`/`scan` body, reached through a `TryJit*` op — and it also compiles whole
//! *functions*, entered by name (`Jit::lookup` / `capture_loop`), which is how a
//! tail-recursive numeric function becomes a native loop. Reporting only the first was
//! this module's own first bug: `bench/fib.helix`, the project's canonical JIT
//! benchmark, answered "0 kernel sites offered" while its whole point is that `fib` is
//! compiled — and the message went on to list tail-recursive functions among the shapes
//! it covers. A report that contradicts itself in its own second paragraph is worse than
//! no report.
//!
//! **What this reports, and what it deliberately does not.** For a kernel site,
//! compilation happens in two stages, and only the second is observable here:
//!
//! 1. the **compiler** decides a comprehension's shape is eligible and emits a
//!    `TryJit*` op for it (`Program::{reduce_loops, map_kernels, …}`);
//! 2. the **JIT** either generates native code for that site or declines, leaving the
//!    slot `None` so the indices stay aligned with the compiler's.
//!
//! Every site that reached stage 1 is listed with its source position and its stage-2
//! outcome. A comprehension whose line is **absent** from the listing never reached
//! stage 1 — which is itself the answer a reader needs, and the output says so rather
//! than leaving it to be inferred.
//!
//! It does **not** say *why* a shape was refused. The 23 eligibility predicates in
//! `jit::analysis` answer `bool`, not a reason; inventing a plausible-sounding cause
//! from the outside would be worse than admitting the gap, because a wrong explanation
//! sends the reader to rewrite the wrong thing. That is a real follow-up, not a
//! pretended feature.

use crate::bytecode::{Op, Program};
use crate::jit::Jit;

/// One site the compiler offered to the JIT, and what the JIT did with it.
pub struct Site {
    /// Which kernel family — the vocabulary a reader can act on.
    pub family: &'static str,
    /// The site's index within its family (its `kernel_idx` / `loop_idx`).
    pub idx: u32,
    pub line: u32,
    pub col: u32,
    /// Did native code get generated for this site?
    pub compiled: bool,
}

/// Whether a `map` site compiled, across every specialization.
///
/// A map kernel has five: plain `i64`, `f64`, and three mixed forms picked by the
/// receiver's element type and its captures at run time. The question a reader is
/// asking is "did this site get native code", so ANY specialization counts — reporting
/// "declined" because the `f64` variant is absent would be false for an Int array.
fn map_compiled(jit: &Jit, i: usize) -> bool {
    jit.map_kernel(i).is_some()
        || jit.map_kernel_f64(i).is_some()
        || jit.map_kernel_mixed(i).is_some()
        || jit.map_kernel_mixed_int(i).is_some()
        || jit.map_kernel_mixed_value(i).is_some()
}

/// Every site the compiler offered, in source order per chunk.
///
/// `jit` is `None` for three different reasons — no `jit` feature, `HELIX_NOJIT=1`, or
/// simply nothing to build — so the caller passes availability separately rather than
/// inferring it from this. Conflating them made a program with no numeric kernels report
/// "no JIT in this run", which was false.
pub fn sites(prog: &Program, jit: Option<&Jit>) -> Vec<Site> {
    let mut out = Vec::new();
    for chunk in &prog.funcs {
        for (pc, op) in chunk.code.iter().enumerate() {
            // The position side-table is parallel to `code`; a missing entry would mean
            // a compiler bug, so fall back to 0 rather than panicking in a diagnostic.
            let (line, col) = chunk.pos.get(pc).copied().unwrap_or((0, 0));
            let (family, idx, compiled) = match op {
                Op::TryJitReduce { loop_idx, .. } => (
                    "reduce",
                    *loop_idx,
                    jit.is_some_and(|j| j.reduce_loop(*loop_idx as usize).is_some()),
                ),
                // A nested reduce indexes the SAME reduce table by its inner loop.
                Op::TryJitNestedReduce { inner_loop_idx, .. } => (
                    "nested-reduce",
                    *inner_loop_idx,
                    jit.is_some_and(|j| j.reduce_loop(*inner_loop_idx as usize).is_some()),
                ),
                Op::TryJitMap { kernel_idx, .. } => (
                    "map",
                    *kernel_idx,
                    jit.is_some_and(|j| map_compiled(j, *kernel_idx as usize)),
                ),
                Op::TryJitFilter { kernel_idx, .. } => (
                    "filter",
                    *kernel_idx,
                    jit.is_some_and(|j| {
                        j.filter_kernel(*kernel_idx as usize).is_some()
                            || j.filter_kernel_f64(*kernel_idx as usize).is_some()
                    }),
                ),
                Op::TryJitFused { kernel_idx, .. } => (
                    "fused",
                    *kernel_idx,
                    jit.is_some_and(|j| j.fused_kernel(*kernel_idx as usize).is_some()),
                ),
                Op::TryJitScan { loop_idx, .. } => (
                    "scan",
                    *loop_idx,
                    jit.is_some_and(|j| j.scan_loop(*loop_idx as usize).is_some()),
                ),
                _ => continue,
            };
            out.push(Site { family, idx, line, col, compiled });
        }
    }
    out.sort_by_key(|s| (s.line, s.col, s.family));
    out
}

/// A user function the JIT compiled whole — the family a `TryJit*` walk cannot see.
pub struct CompiledFn {
    pub name: String,
    /// Compiled in the form that takes its captured globals as trailing parameters.
    /// The VM declines this entry point when a capture is not an `Int` at call time, so
    /// it is worth distinguishing: it is compiled, and still conditional.
    pub captures: bool,
}

/// Every user function the JIT holds native code for.
///
/// Asked by name against `Program::func_names`, because the JIT's tables are keyed by
/// name and hold no list of their own. Names carry the multi-module rewrite's `m<N>$`
/// prefix, which is stripped for display exactly as every other diagnostic does.
pub fn functions(prog: &Program, jit: Option<&Jit>) -> Vec<CompiledFn> {
    let Some(j) = jit else { return Vec::new() };
    prog.func_names
        .iter()
        .filter_map(|n| {
            let captures = j.capture_loop(n).is_some();
            (j.lookup(n).is_some() || captures)
                .then(|| CompiledFn { name: crate::strip_mangling(n), captures })
        })
        .collect()
}

/// Why a site has no native code, when the reason is not about the site at all.
///
/// Three states have to stay distinct or the report blames the wrong thing:
///
/// - the JIT is **switched off** (`HELIX_NOJIT=1`, or a build without the feature);
/// - the JIT is on but produced **nothing at all** — `codegen::build` declines wholesale
///   on any target that is not x86-64 Linux (`src/jit/codegen.rs:43`), which is **two of
///   the six released platforms** (both aarch64 builds) plus macOS and Windows. Saying
///   "DECLINED" there would tell every Apple-Silicon reader that their loops are shaped
///   wrong, when the truth is that this build has no native codegen at all;
/// - the JIT is on, built something, and refused **this** site — the only case where the
///   shape is the answer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// `HELIX_NOJIT=1`, or no `jit` feature.
    Off,
    /// On, but it compiled nothing here (unsupported target, or no eligible site).
    NothingBuilt,
    /// On, and it produced native code for at least one site.
    Live,
}

/// Where a site actually is, in terms a reader can act on.
///
/// **The line a site carries is a position in the MERGED module space** — every imported
/// file concatenated — which for a multi-module program is a position in no file anyone
/// has open. A field report caught it exactly: `app.helix` is 298 lines and this tool
/// reported compiled sites at 1539, 2179 and 2345. Those are real positions in the
/// 2,443-line merged program of `app` + `ui/` + `web/`, and useless to someone trying to
/// find the loop. For a tool whose stated job is "which kernels compiled, AND WHERE",
/// that was the job half done.
///
/// A single-file program keeps the bare `line:col`: the file is the argument the reader
/// just typed, so repeating it on every row is noise. That is the same rule error
/// rendering already applies — `multi_module` is exactly the flag `render_err` uses to
/// decide whether module context is worth showing.
fn site_where(spans: &[crate::module::Span], multi: bool, line: u32, col: u32) -> String {
    if !multi {
        return format!("{line}:{col}");
    }
    let (_, filename, local) = crate::module::locate(spans, line as usize);
    format!("{}:{local}:{col}", display_path(filename))
}

/// A loaded module's path as short as it can be without becoming ambiguous: relative to
/// the working directory when it is under it (`ui/render.helix`), absolute otherwise. The
/// loader canonicalizes every path, so the raw value is an absolute one that would wrap
/// the terminal on every row.
fn display_path(abs: &str) -> String {
    let p = std::path::Path::new(abs);
    std::env::current_dir()
        .ok()
        .and_then(|cwd| p.strip_prefix(&cwd).ok().map(|r| r.display().to_string()))
        .unwrap_or_else(|| abs.to_string())
}

/// The human report. `file` is shown as the reader typed it.
pub fn render(
    file: &str,
    sites: &[Site],
    fns: &[CompiledFn],
    engine: Engine,
    spans: &[crate::module::Span],
    multi: bool,
) -> String {
    let mut s = String::new();
    let built = sites.iter().filter(|x| x.compiled).count();
    s.push_str(&format!(
        "{file}: {} kernel site(s) offered to the JIT, {built} compiled; \
         {} function(s) compiled whole\n",
        sites.len(),
        fns.len()
    ));
    match engine {
        Engine::Off => s.push_str(
            "\nNo JIT in this run — the binary was built without the `jit` feature, or \
             `HELIX_NOJIT=1` is set. Nothing below could compile: that is the switch, not \
             the shapes, so no site is marked declined.\n",
        ),
        // Only worth saying when there WAS something to compile.
        Engine::NothingBuilt if !sites.is_empty() => s.push_str(
            "\nThe JIT is enabled but produced no native code here. Native codegen is \
             x86-64 Linux only today, so on any other target every site below is unbuilt \
             for that reason and not because of its shape.\n",
        ),
        _ => {}
    }
    if !fns.is_empty() {
        s.push_str("\nfunctions compiled whole (entered by name, not through a kernel site):\n");
        for f in fns {
            s.push_str(&format!(
                "  {}{}\n",
                f.name,
                if f.captures { "   (takes captured globals; the VM declines a non-Int capture)" } else { "" }
            ));
        }
    }
    if sites.is_empty() {
        // Only say "nothing here" when nothing here is TRUE. `bench/fib.helix` compiles
        // one function and offers no kernel site; claiming it offered nothing at all,
        // then listing tail-recursive functions among the covered shapes, was this
        // module's first bug.
        if fns.is_empty() {
            s.push_str(
                "\nNothing in this program was compiled. Numeric `map`/`filter`/`reduce`/\
                 `scan` over packed arrays and tail-recursive numeric functions are the \
                 shapes the JIT takes; a comprehension over records, strings or a \
                 DataFrame is not one.\n",
            );
        } else {
            s.push_str(
                "\nNo kernel sites — this program's native code is in the function(s) \
                 above.\n",
            );
        }
        return s;
    }
    s.push('\n');
    // One pass to size the location column, so a multi-module listing lines up instead of
    // ragged-edging on every differently-named file.
    let wheres: Vec<String> =
        sites.iter().map(|x| site_where(spans, multi, x.line, x.col)).collect();
    let w = wheres.iter().map(|t| t.len()).max().unwrap_or(0);
    for (x, at) in sites.iter().zip(&wheres) {
        // "DECLINED" names a decision the JIT made about THIS shape. With the JIT
        // switched off it made no decision, and printing the word anyway would send a
        // reader to rewrite a loop that was never looked at.
        let verdict = match (engine, x.compiled) {
            (Engine::Off, _) => "not built (no JIT)",
            (Engine::NothingBuilt, _) => "not built (no codegen)",
            (Engine::Live, true) => "compiled",
            (Engine::Live, false) => "DECLINED",
        };
        s.push_str(&format!("  {at:<w$}  {:<14} {verdict}\n", x.family));
    }
    s.push_str(
        "\nA comprehension whose line is not listed was never offered to the JIT: the \
         compiler ruled its shape out before codegen. This reports what the JIT was asked \
         and what it answered — not yet WHY a shape was refused.\n",
    );
    s
}

/// The same report as data.
pub fn to_json(
    file: &str,
    sites: &[Site],
    fns: &[CompiledFn],
    engine: Engine,
    spans: &[crate::module::Span],
    multi: bool,
) -> serde_json::Value {
    let locate = |line: u32| -> (String, usize) {
        if multi {
            let (_, filename, local) = crate::module::locate(spans, line as usize);
            (display_path(filename), local)
        } else {
            (file.to_string(), line as usize)
        }
    };
    serde_json::json!({
        "file": file,
        "engine": match engine {
            Engine::Off => "off",
            Engine::NothingBuilt => "nothing-built",
            Engine::Live => "live",
        },
        "jit_available": engine != Engine::Off,
        "offered": sites.len(),
        "compiled": sites.iter().filter(|x| x.compiled).count(),
        "functions": fns.iter().map(|f| serde_json::json!({
            "name": f.name,
            "captures_globals": f.captures,
        })).collect::<Vec<_>>(),
        "sites": sites.iter().map(|x| {
            let (f, local) = locate(x.line);
            serde_json::json!({
                "family": x.family,
                "index": x.idx,
                // The file and the line WITHIN it — what a reader or an editor can open.
                "file": f,
                "line": local,
                // The merged-module position, kept because it is what the compiler and
                // the bytecode actually carry: dropping it would make this report
                // impossible to correlate with anything downstream.
                "merged_line": x.line,
                "col": x.col,
                "compiled": x.compiled,
            })
        }).collect::<Vec<_>>(),
    })
}
