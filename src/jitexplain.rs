//! `helix jit-explain <script>` — which numeric kernels the JIT compiled, and where.
//!
//! **The footgun this answers.** `AGENTS.md` lists it: falling off a JIT kernel is
//! *silent*. The answer stays correct and the program gets much slower, so the only
//! symptom is a wall-clock number the reader has nothing to compare against. Until now
//! there was no way to ask whether a hot loop had been compiled at all — the language
//! offered a large speedup, declined to give it, and said nothing.
//!
//! **What this reports, and what it deliberately does not.** Compilation happens in two
//! stages, and only the second is observable here:
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

/// The human report. `file` is shown as the reader typed it.
pub fn render(file: &str, sites: &[Site], engine: Engine) -> String {
    let mut s = String::new();
    let built = sites.iter().filter(|x| x.compiled).count();
    s.push_str(&format!(
        "{file}: {} kernel site(s) offered to the JIT, {built} compiled\n",
        sites.len()
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
    if sites.is_empty() {
        s.push_str(
            "\nThe compiler offered no kernel sites at all. Numeric `map`/`filter`/`reduce`/\
             `scan` over packed arrays and tail-recursive numeric functions are the shapes \
             it offers; a comprehension over records, strings or a DataFrame is not one.\n",
        );
        return s;
    }
    s.push('\n');
    for x in sites {
        // "DECLINED" names a decision the JIT made about THIS shape. With the JIT
        // switched off it made no decision, and printing the word anyway would send a
        // reader to rewrite a loop that was never looked at.
        let verdict = match (engine, x.compiled) {
            (Engine::Off, _) => "not built (no JIT)",
            (Engine::NothingBuilt, _) => "not built (no codegen)",
            (Engine::Live, true) => "compiled",
            (Engine::Live, false) => "DECLINED",
        };
        s.push_str(&format!("  {:>4}:{:<4} {:<14} {verdict}\n", x.line, x.col, x.family));
    }
    s.push_str(
        "\nA comprehension whose line is not listed was never offered to the JIT: the \
         compiler ruled its shape out before codegen. This reports what the JIT was asked \
         and what it answered — not yet WHY a shape was refused.\n",
    );
    s
}

/// The same report as data.
pub fn to_json(file: &str, sites: &[Site], engine: Engine) -> serde_json::Value {
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
        "sites": sites.iter().map(|x| serde_json::json!({
            "family": x.family,
            "index": x.idx,
            "line": x.line,
            "col": x.col,
            "compiled": x.compiled,
        })).collect::<Vec<_>>(),
    })
}
