#!/usr/bin/env python3
"""agentbench.py — how well do agents write Helix? Measured, not guessed (ADR 0062, ADR 0068 Part 0).

    python3 scripts/agentbench.py --selftest                 # prove every acceptance check can say yes AND no
    python3 scripts/agentbench.py --list                     # the tasks
    python3 scripts/agentbench.py --agent claude --runs 3    # run an agent on every task, 3 times each
    python3 scripts/agentbench.py --agent claude --tasks sales-by-region,fix-the-bug --runs 1
    python3 scripts/agentbench.py --agent claude --runs 3 --record   # …and add it to RESULTS.md
    python3 scripts/agentbench.py --from 20261127-030209.json --record # record a run CI uploaded

WHAT IT MEASURES. Each task in bench/agents/tasks/<name>/ is a job stated the way a user would
state it (`task.md`), the files the user would hand over (`files/`), and an acceptance check
the agent never sees (`accept.py`). An agent works in a fresh directory holding the task's
files and AGENTS.md, with `helix` on its PATH. That `helix` is a recording wrapper: every call
the agent makes — what it ran, how it exited, what errors it was shown — goes to a log, so the
numbers below are the same for any agent, whatever it says about itself:

  solved         the hidden acceptance check passed
  first-clean    the FIRST call that compiled the agent's code (check, run, test or eval) had no error
  calls          helix invocations to the end; failed = those that exited non-zero
  errors         distinct error messages the agent was shown, and how many carried a `help:` line

Agents are noisy, so every task runs --runs times and the report gives the spread, not one
number. It runs on demand, not on every PR: it needs an agent and its credentials, and costs
money. A run's raw data stays beside its workspaces; `--record` puts it in bench/agents/results/
and appends its summary to bench/agents/RESULTS.md, which is how the trend is kept.

THE AGENT. `--agent` is a command template run in the task's directory, with `{prompt_file}`
replaced by the path of a file holding the prompt. `--agent claude` is shorthand for Claude Code
in print mode, allowed only to read and write files in the task directory and to run `helix`
(`--allowedTools "Bash(helix:*)" Read Write Edit Glob Grep`; no web, no subagents, no MCP). Any
other agent CLI works through a template; its own self-report is parsed only for Claude Code's
JSON (turns, cost, time, model). The agent's environment is an allowlist (PASS_ENV), so an
agent started from inside another agent's session does not inherit that session.

--selftest needs no agent, and CI runs it: for every task the acceptance check must PASS on
the reference solution and FAIL on the broken one and on the untouched files, so a check that
cannot say no is caught before it is believed (docs/testing.md: a gate has to be sabotaged once
to prove it can fail), and a language change that breaks a task is caught on its PR.
"""

import argparse
import concurrent.futures
import importlib.util
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True  # no __pycache__ beside the tasks

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TASKS = os.path.join(REPO, "bench", "agents", "tasks")
RESULTS = os.path.join(REPO, "bench", "agents", "results")

AGENTS = {
    # Files in the task directory and `helix`, nothing else: no other commands, no web, no
    # subagents, no MCP servers. Unlisted tools that need a permission are refused in print
    # mode anyway; the explicit list makes the condition visible in the results.
    "claude": (
        'claude -p "$(cat {prompt_file})" --output-format json --no-session-persistence '
        '--strict-mcp-config --max-turns 80 '
        '--allowedTools "Bash(helix:*)" Read Write Edit Glob Grep '
        '--disallowedTools WebFetch WebSearch Task Agent'
    ),
}

# What the agent's process inherits. An allowlist, not the caller's whole environment: an agent
# run from inside another agent's session (or a CI job) must not inherit that session's
# identity, and nothing the caller happens to have set may change what is measured.
PASS_ENV = ("HOME", "USER", "LANG", "LC_ALL", "TERM", "TMPDIR", "HTTPS_PROXY", "https_proxy",
            "HTTP_PROXY", "http_proxy", "NO_PROXY", "no_proxy", "NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE",
            "ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "ANTHROPIC_MODEL")

# The calls that compile the agent's code; `search`, `doc` and `describe` always succeed, so
# they say nothing about whether the first draft was right.
COMPILES = ("check", "run", "test", "eval")

PREAMBLE = """You are working in the current directory, which holds the files for this job.
The `helix` command is installed: Helix is the programming language to use. AGENTS.md in this
directory describes how to work with it. Do the job below, then stop. Leave the finished files
in this directory.

THE JOB:

"""

# The recording wrapper. It is inert to the agent's work: same arguments, same streams, same
# exit code; stdout passes straight through, and stderr is copied to the agent AND kept for the
# log. What it does change is AUTHORITY. The real binary gets none of the agent's credentials
# (the API key, this harness's own variables), and runs under the capability gate (ADR 0021)
# with files allowed and the network and processes denied: no task needs them, and an agent
# that can only run `helix` must not be able to send anything anywhere with it. Files are not
# scoped yet (ADR 0068 Part 1) — the workflow's artifact scan is the second line for that.
SHIM = r'''#!/usr/bin/env python3
import json, os, subprocess, sys, time
real = os.environ["AGENTBENCH_HELIX"]
log = os.environ["AGENTBENCH_LOG"]
env = {k: v for k, v in os.environ.items() if not k.startswith(("ANTHROPIC_", "AGENTBENCH_", "HELIX_"))}
env.update(HELIX_CAP="enforce", HELIX_ALLOW_FS="all")
start = time.monotonic()
p = subprocess.Popen([real] + sys.argv[1:], stderr=subprocess.PIPE, env=env)
err = []
for chunk in iter(lambda: p.stderr.read(4096), b""):
    sys.stderr.buffer.write(chunk)
    sys.stderr.buffer.flush()
    if sum(len(c) for c in err) < 65536:
        err.append(chunk)
rc = p.wait()
with open(log, "a") as fh:
    fh.write(json.dumps({"args": sys.argv[1:], "rc": rc, "secs": round(time.monotonic() - start, 3),
                         "stderr": b"".join(err).decode(errors="replace")}) + "\n")
sys.exit(rc)
'''


def load_task(name):
    d = os.path.join(TASKS, name)
    spec = importlib.util.spec_from_file_location("accept_" + name.replace("-", "_"), os.path.join(d, "accept.py"))
    mod = importlib.util.module_from_spec(spec)
    sys.path.insert(0, os.path.join(REPO, "bench", "agents"))
    spec.loader.exec_module(mod)
    return {"name": name, "dir": d, "task": open(os.path.join(d, "task.md")).read(), "check": mod.check}


def task_names(only):
    names = sorted(n for n in os.listdir(TASKS) if os.path.isfile(os.path.join(TASKS, n, "task.md")))
    if only:
        want = [w.strip() for w in only.split(",") if w.strip()]
        missing = [w for w in want if w not in names]
        if missing:
            sys.exit("agentbench: no such task: %s" % ", ".join(missing))
        names = want
    return names


def fresh_workspace(task, base, overlay=None):
    ws = tempfile.mkdtemp(prefix="agentbench-%s-" % task["name"], dir=base)
    files = os.path.join(task["dir"], "files")
    if os.path.isdir(files):
        shutil.copytree(files, ws, dirs_exist_ok=True)
    if overlay:
        shutil.copytree(os.path.join(task["dir"], overlay), ws, dirs_exist_ok=True)
    return ws


def summarize_log(path):
    calls = []
    if os.path.exists(path):
        with open(path) as fh:
            calls = [json.loads(line) for line in fh if line.strip()]
    def first_line(err):
        for line in err.splitlines():
            if line.startswith("error"):
                return line.strip()
        return None
    errors = {}
    for c in calls:
        msg = first_line(c["stderr"])
        if msg:
            errors.setdefault(msg, "help:" in c["stderr"])
    verbs = [c["args"][0] if c["args"] else "" for c in calls]
    checks = [c for c in calls if c["args"][:1] == ["check"]]
    compiles = [c for c in calls if c["args"][:1] and c["args"][0] in COMPILES]
    first = compiles[0] if compiles else None
    return {
        "calls": len(calls),
        "failed": sum(1 for c in calls if c["rc"] != 0),
        "checks": len(checks),
        "first_clean": None if first is None else (first["rc"] == 0 and first_line(first["stderr"]) is None),
        "errors": len(errors),
        "errors_with_help": sum(1 for h in errors.values() if h),
        "error_messages": sorted(errors)[:20],
        "verbs": {v: verbs.count(v) for v in sorted(set(verbs))},
    }


def agent_self_report(stdout):
    # Claude Code's `--output-format json`: one object with turns, cost and time.
    try:
        j = json.loads(stdout)
    except (ValueError, TypeError):
        return {}
    if not isinstance(j, dict):
        return {}
    rec = {k: j.get(k) for k in ("num_turns", "total_cost_usd", "duration_ms", "is_error", "subtype") if k in j}
    if isinstance(j.get("modelUsage"), dict):
        rec["models"] = sorted(j["modelUsage"])
    if isinstance(j.get("result"), str):
        rec["result"] = j["result"][-400:]
    return rec


def run_one(task, run, args, helix, base):
    ws = fresh_workspace(task, base)
    shutil.copy(os.path.join(REPO, "AGENTS.md"), os.path.join(ws, "AGENTS.md"))
    tools = tempfile.mkdtemp(prefix="agentbench-bin-", dir=base)
    # The real binary is copied next to the shim, so nothing the agent can see (its PATH, the
    # shim's environment) leads back to this repository and its acceptance checks.
    real = os.path.join(tools, ".helix-real")
    shutil.copy(helix, real)
    shim = os.path.join(tools, "helix")
    with open(shim, "w") as fh:
        fh.write(SHIM)
    os.chmod(shim, 0o755)
    log = os.path.join(tools, "calls.jsonl")
    prompt_file = os.path.join(tools, "prompt.md")
    with open(prompt_file, "w") as fh:
        fh.write(PREAMBLE + task["task"])
    env = {k: os.environ[k] for k in PASS_ENV if k in os.environ}
    env.update(PATH=tools + os.pathsep + os.environ.get("PATH", ""), AGENTBENCH_HELIX=real,
               AGENTBENCH_LOG=log)
    template = AGENTS.get(args.agent, args.agent)
    cmd = template.replace("{prompt_file}", prompt_file)
    start = time.monotonic()
    try:
        p = subprocess.run(cmd, shell=True, cwd=ws, env=env, capture_output=True, timeout=args.timeout,
                           stdin=subprocess.DEVNULL)
        rc, out = p.returncode, p.stdout.decode(errors="replace")
    except subprocess.TimeoutExpired:
        rc, out = "timeout", ""
    secs = time.monotonic() - start
    # The check runs on the files the agent left, with the REAL binary, outside the agent's run.
    try:
        failures = task["check"](ws, helix)
    except Exception as e:  # a broken check must not look like a failed agent
        failures = ["acceptance check crashed: %r" % e]
    rec = {"task": task["name"], "run": run, "agent_rc": rc, "secs": round(secs, 1),
           "solved": not failures, "failures": failures[:10]}
    rec.update(summarize_log(log))
    rec["agent"] = agent_self_report(out)
    os.remove(real)
    if args.keep:
        rec["workspace"] = ws
        rec["calls_log"] = log
    else:
        shutil.rmtree(ws, ignore_errors=True)
        shutil.rmtree(tools, ignore_errors=True)
    return rec


def selftest(names, helix, base):
    ok = True
    for name in names:
        task = load_task(name)
        # `untouched` is the task's own files with nothing done: a check that passes on it
        # would credit an agent for giving up.
        for overlay, want in (("reference", True), ("broken", False), (None, False)):
            label = overlay or "untouched"
            if overlay and not os.path.isdir(os.path.join(task["dir"], overlay)):
                print("  %-22s %-9s MISSING" % (name, label))
                ok = False
                continue
            ws = fresh_workspace(task, base, overlay)
            try:
                failures = task["check"](ws, helix)
            except Exception as e:
                failures = ["check crashed: %r" % e]
            good = (not failures) == want
            ok = ok and good
            if not good:
                detail = "  <- " + ("; ".join(failures)[:300] if failures else "passed, but must fail")
            elif failures:
                # Say WHY a wrong solution fails, so a "no" for the wrong reason (a typo in the
                # broken file, a crash) is visible rather than counted.
                detail = "  (fails: %s)" % failures[0].splitlines()[0][:110]
            else:
                detail = ""
            print("  %-22s %-9s %s%s" % (name, label, "ok" if good else "BROKEN", detail))
            shutil.rmtree(ws, ignore_errors=True)
    return ok


def report(records, runs):
    by = {}
    for r in records:
        by.setdefault(r["task"], []).append(r)
    lines = ["| task | solved | first compile clean | helix calls (median) | failed calls | errors shown (with help) |",
             "|---|---|---|---|---|---|"]
    for name in sorted(by):
        rs = by[name]
        fc = [r["first_clean"] for r in rs if r["first_clean"] is not None]
        lines.append("| %s | %d/%d | %s | %s | %s | %d (%d) |" % (
            name, sum(r["solved"] for r in rs), len(rs),
            "%d/%d" % (sum(fc), len(fc)) if fc else "—",
            statistics.median(r["calls"] for r in rs), statistics.median(r["failed"] for r in rs),
            sum(r["errors"] for r in rs), sum(r["errors_with_help"] for r in rs)))
    total = len(records)
    solved = sum(r["solved"] for r in records)
    lines.append("")
    lines.append("**Solved %d of %d runs (%.0f%%)** over %d task(s), %d run(s) each." % (
        solved, total, 100.0 * solved / max(total, 1), len(by), runs))
    cost = [r["agent"].get("total_cost_usd") for r in records if r["agent"].get("total_cost_usd") is not None]
    if cost:
        lines.append("Agent-reported cost: $%.2f total." % sum(cost))
    return "\n".join(lines)


def finish(run, record, base):
    """Print the summary and save the raw run: beside its workspaces, or, with `record`, in the
    repository with a section appended to RESULTS.md."""
    table = report(run["records"], run["runs"])
    print()
    print(table)
    if record:
        for r in run["records"]:  # machine-local paths mean nothing in the repository
            r.pop("workspace", None)
            r.pop("calls_log", None)
    out = os.path.join(RESULTS if record else base, "%s.json" % run["stamp"])
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as fh:
        json.dump(run, fh, indent=1)
    print("\nwritten: %s" % (os.path.relpath(out, REPO) if record else out))
    if record:
        with open(os.path.join(REPO, "bench", "agents", "RESULTS.md"), "a") as fh:
            fh.write("\n## %s-%s-%s — %s, agent `%s`%s\n\n%s\n\nRaw: `bench/agents/results/%s.json`\n" % (
                run["stamp"][:4], run["stamp"][4:6], run["stamp"][6:8], run["helix"], run["agent"],
                " (%s)" % ", ".join(run["models"]) if run["models"] else "", table, run["stamp"]))
        print("appended to bench/agents/RESULTS.md")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bin", default=os.path.join(REPO, "target", "gate", "helix"))
    ap.add_argument("--agent", help="`claude`, or a command template using {prompt_file}")
    ap.add_argument("--tasks", help="comma-separated task names (default: all)")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--jobs", type=int, default=2)
    ap.add_argument("--timeout", type=float, default=1200.0, help="seconds per agent run")
    ap.add_argument("--base", default=None, help="where workspaces go (default: a temp dir)")
    ap.add_argument("--keep", action="store_true", help="keep each workspace, and record its path")
    ap.add_argument("--record", action="store_true", help="append the summary to bench/agents/RESULTS.md")
    ap.add_argument("--from", dest="from_file", metavar="RESULTS_JSON",
                    help="summarize (and with --record, record) a finished run, e.g. one CI uploaded")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("--selftest", action="store_true")
    args = ap.parse_args()

    if args.from_file:
        with open(args.from_file) as fh:
            finish(json.load(fh), args.record, os.path.dirname(os.path.abspath(args.from_file)))
        return 0

    names = task_names(args.tasks)
    if args.list:
        for n in names:
            first = load_task(n)["task"].strip().splitlines()[0]
            print("%-22s %s" % (n, first[:100]))
        return 0
    helix = os.path.abspath(args.bin)
    if not os.access(helix, os.X_OK):
        sys.exit("agentbench: no helix binary at %s" % helix)
    base = args.base or tempfile.mkdtemp(prefix="agentbench-")
    os.makedirs(base, exist_ok=True)
    if args.selftest:
        print("agentbench --selftest: can every acceptance check say yes and no?")
        return 0 if selftest(names, helix, base) else 1
    if not args.agent:
        sys.exit("agentbench: say which agent to run (--agent claude, or a template)")

    jobs = [(load_task(n), r) for n in names for r in range(args.runs)]
    records = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for rec in pool.map(lambda j: run_one(j[0], j[1], args, helix, base), jobs):
            records.append(rec)
            print("%-22s run %d  %s  calls=%d failed=%d first_clean=%s  %.0fs" % (
                rec["task"], rec["run"], "SOLVED" if rec["solved"] else "failed", rec["calls"], rec["failed"],
                rec["first_clean"], rec["secs"]), flush=True)
    version = subprocess.run([helix, "--version"], capture_output=True, text=True).stdout.strip()
    run = {"helix": version, "agent": args.agent if args.agent in AGENTS else "custom", "template": args.agent,
           "models": sorted({m for r in records for m in r["agent"].get("models", [])}),
           "runs": args.runs, "stamp": time.strftime("%Y%m%d-%H%M%S"), "records": records}
    finish(run, args.record, base)
    return 0


if __name__ == "__main__":
    sys.exit(main())
