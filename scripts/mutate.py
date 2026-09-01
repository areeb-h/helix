#!/usr/bin/env python3
"""mutate.py — break a semantic chokepoint on purpose and demand that a test notices.

    python3 scripts/mutate.py --only NAME [--jobs 1]

WHY THIS EXISTS. `vmparity` and `dfdiff` compare implementations against each other, and
both are blind to a bug in code the implementations SHARE. That is not hypothetical: the
`select`/`sort`/`group` shadowing bug lived in `arg_as_column_name`, which the walker and
the VM share deliberately so they cannot diverge — and `column_name_args` computes the
column names BEFORE `lf.select(&names)` reaches a backend, so polars and native-df received
identical wrong names and agreed too. Three engines and two backends, all quiet. A corpus
golden written by hand is what caught it.

The rule that follows is:

    A COMPONENT CANNOT BE EVIDENCE FOR SEMANTICS THAT IT DEFINES.

So this asks the complementary question to "do the implementations agree?": **if this
helper were wrong, would anything fail?** A mutant that survives is an evidence hole, named
precisely — a place where the suite would not notice the bug.

MUTATIONS ARE CURATED, NOT RANDOM. Random edits to Rust overwhelmingly produce code that
does not compile, and a compile failure is not evidence of anything; the interesting
mutations are semantic ones a reviewer could plausibly write. Each entry below is a change
someone might make while "simplifying" — which is exactly the change that needs a test
standing in front of it.

A mutant that fails to COMPILE is reported separately and counts as neither killed nor
survived: the harness could not ask the question.

WHAT COUNTS AS KILLED. Not "the suite exited nonzero" -- that conflates a mutant being
caught with the suite being broken for some unrelated reason, and quietly turns every
verdict into KILLED the moment one test is red for its own reasons. The harness runs the
suite on the CLEAN tree first and a mutant is killed only by a failure that baseline does
not already have. The verdict then names the test, which is the part worth reading: "a
test noticed" is a claim, `chart_axis_labels_use_three_significant_figures` is evidence.

SAFETY, AND WHY IT IS SHAPED THIS WAY. The first version kept a `.orig` copy and restored it
in a `finally`. That is not enough: the process was killed mid-run and left a mutated
`src/chart.rs` on disk, because **a `finally` does not survive SIGKILL**. So:

  1. **git is the backup**, not a sidecar file. Restoring is `git checkout -- <path>`, which
     cannot be half-written and leaves nothing to clean up.
  2. **A dirty tree is refused.** A mutation is applied by EDITING a tracked file; with
     uncommitted work in `src/` a crash cannot be told apart from a mutation, and the
     restore would destroy real edits.
  3. **Recovery runs FIRST**, before anything is touched, and names the file and the command.
  4. **One mutant per invocation by default.** The batch form takes ~25 minutes and was
     killed by a duration limit before producing a single verdict. A long run that yields
     nothing is worse than a short one that yields one fact.

COST, AND WHY IT IS ONE TARGET. An earlier version ran `cargo build --tests`, which
builds EVERY test binary -- the lib unit tests and both integration targets. A mutation
only needs the suite that would notice it, so this builds `--test cli` alone.

AND WHY THE GATE PROFILE, NOT `dev`. `scripts/gate.sh` runs the suite under
`--profile gate` (opt-3, no LTO) with mold when it is installed, and this harness runs it
the same way -- not as a speed trick, but because the profile CHANGES THE ANSWER. Under
`dev` the interpreter's frames are large enough that the anti-drift corpus test overflows
the stack and fails before any mutation is applied; a red test can kill no mutant, so
every mutant it alone would have caught would report SURVIVED. A harness that asks a
different question from the project's own bar is measuring a different project.

Not in the gate: each mutant costs a rebuild. Run it after touching a chokepoint, and when
adding one to the catalog.
"""

import argparse
import hashlib
import io
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Written before a file is edited, removed after it is restored. A `finally` does not
# survive SIGKILL, so the guarantee cannot be "the run always cleans up" -- it is "an
# interrupted run leaves a note, and the next run acts on it before anything else". That
# is not theoretical: a killed run left a mutated `src/chart.rs` in the tree.
SENTINEL = os.path.join(ROOT, "target", "mutate.active")

# The clean-tree failure set, cached. The suite takes minutes -- several tests link a
# standalone binary -- and charging that to every invocation would price the harness out
# of the one-mutant-at-a-time habit that makes it usable at all.
BASELINE_CACHE = os.path.join(ROOT, "target", "mutate.baseline.json")

# (name, file, old, new, what a reviewer would think they were doing)
MUTANTS = [
    (
        "column-name-ignores-binding",
        "src/interp/dataframe_ops.rs",
        "Expr::Ident { name, .. } => match resolve_var(name) {\n            None => Ok(name.clone()),",
        "Expr::Ident { name, .. } => match None::<Value> {\n            None => Ok(name.clone()),",
        "take a bare name literally again (ADR 0028 reverted for column positions)",
    ),
    (
        "column-sigil-resolves-binding",
        "src/interp/dataframe_ops.rs",
        "Expr::Column { name, .. } => Ok(name.clone()),",
        "Expr::Column { name, .. } => Ok(resolve_var(name)\n            .and_then(|v| match v { Value::Str(s) => Some((*s).clone()), _ => None })\n            .unwrap_or_else(|| name.clone())),",
        "make `@name` resolve a binding too, for consistency",
    ),
    (
        "axis-label-uses-value-formatter",
        "src/chart.rs",
        "    let (lo_lbl, hi_lbl) = (fmt_axis(ymin), fmt_axis(ymax));",
        "    let (lo_lbl, hi_lbl) = (fmt_num(ymin), fmt_num(ymax));",
        "one number formatter is simpler than two",
    ),
    (
        "rich-separator-in-plain-output",
        "src/report.rs",
        'if !opts.rich || opts.ascii_only() { ", " } else { " \\u{b7} " }',
        'if opts.ascii_only() { ", " } else { " \\u{b7} " }',
        "the separator only depends on the box style",
    ),
    (
        "ceiling-does-not-narrow",
        "src/capability.rs",
        "    let narrow = |ceiling: bool, from_env: bool| ceiling && (!env_says || from_env);",
        "    let narrow = |ceiling: bool, from_env: bool| ceiling || from_env;",
        "let the environment grant what the manifest did not",
    ),
    (
        "bar-value-right-aligned",
        "src/chart.rs",
        "        out.push(' ');\n        out.push_str(&paint_num(opts, &valstrs[i]));",
        "        let used = ((v / maxv) * barw as f64).ceil() as usize;\n        out.push_str(&\" \".repeat(barw.saturating_sub(used)));\n        out.push(' ');\n        out.push_str(&paint_num(opts, &valstrs[i]));",
        "line the values up in a column",
    ),
    (
        "effects-skip-transitive-calls",
        "src/effects.rs",
        "                if !is_method && self.bodies.contains_key(callee.as_str()) {\n                    self.visit(&callee, path);\n                }",
        "                let _ = &callee;",
        "only report effects a function names directly",
    ),
    (
        "archive-path-ignores-parent",
        "src/module.rs",
        "            std::path::Component::ParentDir => {\n                out.pop();\n            }",
        "            std::path::Component::ParentDir => {}",
        "a bundle archive has no `..` paths anyway",
    ),
]


FAILED_LINE = re.compile(r"^test (\S+) \.\.\. FAILED")


def suite(env):
    """Run the CLI suite. Returns (failing test names, did-it-report-at-all, raw output).

    `reported` is separate from the failure set on purpose. A stack overflow aborts the
    test binary mid-run: the process exits nonzero with no summary and possibly no FAILED
    lines at all. That is not a mutant being caught, it is the question going unasked, and
    calling it KILLED would be the harness lying in the direction that flatters it.
    """
    # `< /dev/null` is not optional: a test that reads stdin blocks forever without it,
    # which reads as "the suite is slow" right up until the run is killed for taking too
    # long. `scripts/gate.sh` carries the same requirement.
    res = run(env + "cargo test --profile gate --test cli < /dev/null 2>&1")
    out = res.stdout
    fails = {m.group(1) for m in (FAILED_LINE.match(l) for l in out.splitlines()) if m}
    reported = any(l.startswith("test result:") for l in out.splitlines())
    return fails, reported, out


def cargo_env(jobs):
    """The gate's own build settings, not a private set of this tool's invention.

    Deviating here is how a harness ends up disagreeing with the bar it is supposed to
    reinforce. `CARGO_BUILD_JOBS` governs the BUILD only -- the suite itself runs one
    thread per core, which is where the real memory goes, since several CLI tests link a
    standalone executable.
    """
    env = "CARGO_BUILD_JOBS=%s " % jobs
    if shutil.which("mold"):
        env += 'RUSTFLAGS="${RUSTFLAGS:-} -Clink-arg=-fuse-ld=mold" '
    return env


def tree_id():
    """A fingerprint of exactly what is checked out: commit plus every uncommitted byte.

    Keyed on the porcelain status as well as HEAD, so ANY difference in the tree -- a
    changed `Cargo.lock`, a new untracked file -- invalidates the cache. A baseline that
    describes a tree which is no longer there is worse than no baseline, because it turns
    a stale pass into a confident SURVIVED.
    """
    head = git(["rev-parse", "HEAD"]).stdout.strip()
    status = git(["status", "--porcelain"]).stdout
    diff = git(["diff", "HEAD"]).stdout
    return head + ":" + hashlib.sha256((status + diff).encode("utf-8")).hexdigest()[:16]


def git(args):
    return subprocess.run(["git"] + args, cwd=ROOT, capture_output=True, text=True)


def run(cmd, **kw):
    return subprocess.run(cmd, cwd=ROOT, shell=True, capture_output=True, text=True, **kw)


def dirty_sources():
    """Tracked files under src/ carrying uncommitted changes."""
    out = git(["status", "--porcelain", "--", "src/"]).stdout.strip()
    # Split on whitespace rather than slicing a fixed prefix: porcelain's status field is
    # one or two characters depending on staging, and an off-by-one prints a path that does
    # not exist, which is worse than no path at all.
    return [l.split(None, 1)[1] for l in out.splitlines() if l.strip() and not l.startswith("??")]


def arm(path):
    """Record the file about to be mutated, so a killed run is recoverable."""
    os.makedirs(os.path.dirname(SENTINEL), exist_ok=True)
    io.open(SENTINEL, "w", encoding="utf-8", newline="\n").write(path)


def disarm():
    try:
        os.remove(SENTINEL)
    except OSError:
        pass


def recover():
    """Undo a mutation left by a killed run. Returns the restored path, or None.

    Only the file named in the sentinel is touched. The harness knows it put that edit
    there, which is exactly the knowledge a blanket `git checkout -- src/` would lack --
    and why this one can run automatically where that one could not.
    """
    if not os.path.exists(SENTINEL):
        return None
    path = io.open(SENTINEL, encoding="utf-8").read().strip()
    disarm()
    if not path or path not in dirty_sources():
        return None
    git(["checkout", "--", path])
    return path


def preflight():
    """Refuse to start on a dirty tree, and say plainly how to recover from a killed run."""
    healed = recover()
    if healed:
        print("recovered from an interrupted run: restored %s\n" % healed)
    stray = run("find src scripts -name '*.orig' 2>/dev/null").stdout.split()
    dirty = dirty_sources()
    if not dirty and not stray:
        return True
    print("REFUSING TO RUN -- the working tree is not clean.\n")
    if dirty:
        print("  uncommitted changes under src/:")
        for d in dirty:
            print("     ", d)
        print()
        print("  A mutation is applied by EDITING these files and undone with `git checkout`.")
        print("  With real work in the tree the two are indistinguishable, and a crash mid-run")
        print("  would destroy it. Commit or stash first.")
    if stray:
        print()
        print("  leftover backups from a run that was killed:")
        for s2 in stray:
            print("     ", s2)
        print("  Restore with: git checkout -- <file> && rm <file>.orig")
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--only")
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--jobs", default="1")
    ap.add_argument("--refresh-baseline", action="store_true")
    a = ap.parse_args()

    if a.list:
        for name, path, _, _, why in MUTANTS:
            print("%-32s %-32s %s" % (name, path, why))
        return 0

    if not a.only and not a.all:
        print("pick one mutant with --only NAME, or --all for the long run (~25 min).")
        print("`--list` shows the catalog. One at a time is the default because a batch that")
        print("is killed halfway yields no verdicts at all.")
        return 2

    if not a.jobs.isdigit() or not 1 <= int(a.jobs) <= 2:
        print("--jobs must be 1 or 2. Six cores and 11 GB; a wider mutation build has taken")
        print("this machine down outright, and a run that kills its host proves nothing.")
        return 2

    if not preflight():
        return 2

    # SIGKILL cannot be caught, but a duration limit sends SIGTERM first. Restoring on the
    # way out costs nothing and turns most kills into clean exits.
    def bail(_signum, _frame):
        recover()
        sys.exit(130)

    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, bail)

    chosen = [m for m in MUTANTS if a.all or m[0] == a.only]
    if not chosen:
        print("no mutant named %r" % a.only)
        return 2

    env = cargo_env(a.jobs)

    # The clean tree, first. Without this the harness cannot tell "the mutant was caught"
    # from "this test was already red", and would report the flattering answer to both.
    fid = tree_id()
    cached = None
    if not a.refresh_baseline:
        try:
            c = json.loads(io.open(BASELINE_CACHE, encoding="utf-8").read())
            if c.get("tree") == fid:
                cached = set(c["fails"])
        except Exception:
            cached = None

    if cached is not None:
        base_fails, base_reported, base_out = cached, True, ""
        print("baseline: cached for this exact tree (--refresh-baseline to re-run)")
    else:
        print("baseline: running the suite on the clean tree (minutes; cached afterwards)")
        sys.stdout.flush()
        base_fails, base_reported, base_out = suite(env)
        if base_reported:
            io.open(BASELINE_CACHE, "w", encoding="utf-8", newline="\n").write(
                json.dumps({"tree": fid, "fails": sorted(base_fails)}, indent=2)
            )
    if not base_reported:
        print("REFUSING TO CONTINUE -- the suite did not report on a CLEAN tree, so no")
        print("verdict from it would mean anything. Last lines:")
        for line in base_out.strip().splitlines()[-8:]:
            print("   ", line)
        return 2
    if base_fails:
        print("      %d pre-existing failure(s); only NEW ones count as a kill:" % len(base_fails))
        for f in sorted(base_fails):
            print("        ", f)
    else:
        print("      clean: nothing fails before a mutation is applied")
    sys.stdout.flush()

    killed, survived, uncompilable = [], [], []

    for name, path, old, new, why in chosen:
        full = os.path.join(ROOT, path)
        src = io.open(full, encoding="utf-8").read()
        n = src.count(old)
        if n != 1:
            print("SKIP  %-32s pattern matched %d times in %s — the catalog is stale" % (name, n, path))
            uncompilable.append(name)
            continue

        print("---   %-32s %s" % (name, why))
        sys.stdout.flush()
        arm(path)
        try:
            io.open(full, "w", encoding="utf-8", newline="\n").write(src.replace(old, new, 1))
            t0 = time.time()
            # One target, not `--tests`. And the EXIT CODE decides, not a substring: a
            # grep for "error" calls a build broken because a warning mentioned the word.
            build = run(env + "cargo test --profile gate --test cli --no-run < /dev/null 2>&1")
            if build.returncode != 0:
                print("      does not compile -- the question could not be asked")
                for line in build.stdout.strip().splitlines()[-3:]:
                    print("        " + line)
                uncompilable.append(name)
                continue
            fails, reported, out = suite(env)
            secs = time.time() - t0
            new = sorted(fails - base_fails)
            if not reported:
                print("      the suite did not report (%.0fs) — question not asked" % secs)
                for line in out.strip().splitlines()[-3:]:
                    print("        " + line)
                uncompilable.append(name)
            elif new:
                # Name the test. "KILLED" is a claim; the test that noticed is the evidence.
                print("      KILLED   (%.0fs)  %s" % (secs, new[0]))
                for extra in new[1:4]:
                    print("                        %s" % extra)
                if len(new) > 4:
                    print("                        (+%d more)" % (len(new) - 4))
                killed.append(name)
            else:
                print("      SURVIVED (%.0fs)  *** no test failed that was not already failing ***" % secs)
                survived.append(name)
            sys.stdout.flush()
        finally:
            # git, not a sidecar copy: it cannot be half-written, and it is the same
            # restore a human would run by hand after a kill.
            git(["checkout", "--", path])
            disarm()

    print()
    print("killed %d | survived %d | uncompilable %d" % (len(killed), len(survived), len(uncompilable)))
    if survived:
        print()
        print("SURVIVORS -- each is a semantic change nothing in the suite notices:")
        for s2 in survived:
            print("   ", s2)
    if survived and base_fails:
        # A red test cannot kill anything: its failure is already in the baseline, so a
        # mutant it alone would have caught reads as SURVIVED. Say this next to the
        # survivors, where the wrong conclusion would otherwise be drawn -- not only in
        # the header, which by now has scrolled away.
        print()
        print("   ...but %d test(s) were ALREADY failing and so could kill nothing." % len(base_fails))
        print("   A SURVIVED verdict above is blind to whatever these would have caught:")
        for f in sorted(base_fails):
            print("       ", f)
    left = dirty_sources()
    if left:
        print()
        print("WARNING: src/ is dirty on exit -- restore with: git checkout -- src/")
    return 1 if survived else 0


if __name__ == "__main__":
    sys.exit(main())
