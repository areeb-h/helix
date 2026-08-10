import { NextResponse } from "next/server";
import { execFile, type ExecFileOptions, type ExecFileException } from "node:child_process";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { REPO } from "@/lib/content";
import api from "@/data/api.json";

export const runtime = "nodejs";
export const dynamic = "force-dynamic";

// Executing submitted code is the one genuinely dangerous thing a docs site can do, so
// the guard is layered and each layer is stated:
//
//   1. EFFECTS. Helix tracks an effect for every builtin (pure / io / net / rand / time)
//      and `helix describe` publishes it. Any program mentioning a builtin whose effect
//      is not `pure` is REFUSED before execution. This is not a keyword blocklist I
//      invented — it is the language's own effect data, so it cannot drift from the
//      binary the way a hand-written list would.
//   2. NO FILESYSTEM REACH. The program runs in a fresh temp directory that is deleted
//      afterwards, with cwd set there.
//   3. LIMITS. Hard timeout, output cap, and a source-length cap.
//
// Even so this endpoint only runs where the Helix binary is present, and it is disabled
// unless HELIX_PLAYGROUND=1 — a deployment must opt in deliberately rather than inherit
// remote code execution by accident.

const TIMEOUT_MS = 4000;
const MAX_OUTPUT = 16_000;
const MAX_SOURCE = 4_000;

// Builtins carry TWO fields and they deliberately differ: `effect` is the CAPABILITY
// (pure / fs-read / fs-write / net) and `pure` is referential transparency. `sleep` is
// `effect: "pure"` but `pure: false` — no capability, yet not a pure function. Filtering
// on `effect` alone let `sleep` through, which an adversarial test caught before this
// shipped. `pure === false` is the conservative field, so that is the one used.
const IMPURE: string[] = (api.builtins as { name: string; pure: boolean }[])
  .filter((b) => b.pure === false)
  .map((b) => b.name);

// Methods only publish `effect`, so that is what is available — it catches the
// filesystem writers and the network ones. Methods marked pure that are merely
// NONDETERMINISTIC (`shuffle`, `sample`, `choice`) are allowed through deliberately:
// they reach nothing outside the process, so they are a variable answer, not a risk.
const IMPURE_METHODS: string[] = Object.values(
  api.methods as Record<string, { name: string; effect: string }[]>
)
  .flat()
  .filter((m) => m.effect !== "pure")
  .map((m) => m.name);

const BANNED = [...new Set([...IMPURE, ...IMPURE_METHODS])].filter(
  // `print` is how a playground shows anything. It is classified impure because it
  // writes to stdout, which is exactly the effect we are capturing on purpose.
  (n) => n !== "print"
);

function findImpure(src: string): string | null {
  // Strip comments and string literals first, so prose and text data cannot trip the
  // guard — and, more importantly, so a banned name hidden in a string cannot smuggle
  // past it either (it is removed, not matched).
  const stripped = src
    .replace(/#[^\n]*/g, " ")
    .replace(/"""[\s\S]*?"""/g, '""')
    .replace(/'''[\s\S]*?'''/g, "''")
    .replace(/"(?:[^"\\\n]|\\.)*"/g, '""')
    .replace(/'(?:[^'\\\n]|\\.)*'/g, "''");

  for (const name of BANNED) {
    if (new RegExp(`\\b${name}\\s*\\(`).test(stripped)) return name;
  }
  return null;
}

async function findBinary(): Promise<string | null> {
  for (const p of [
    path.join(REPO, "target/release/helix"),
    path.join(REPO, "target/gate/helix"),
    path.join(REPO, "target/debug/helix"),
  ]) {
    try {
      await fs.access(p);
      return p;
    } catch {
      /* try the next one */
    }
  }
  return null;
}

export async function POST(req: Request) {
  if (process.env.HELIX_PLAYGROUND !== "1") {
    return NextResponse.json(
      { error: "The playground is disabled on this deployment.", disabled: true },
      { status: 503 }
    );
  }

  const { source } = (await req.json()) as { source?: string };
  if (typeof source !== "string" || source.trim() === "") {
    return NextResponse.json({ error: "No source given." }, { status: 400 });
  }
  if (source.length > MAX_SOURCE) {
    return NextResponse.json(
      { error: `Source is longer than ${MAX_SOURCE} characters.` },
      { status: 413 }
    );
  }

  const banned = findImpure(source);
  if (banned) {
    return NextResponse.json({
      refused: true,
      error: `\`${banned}\` has a side effect, so it cannot run here.`,
      detail:
        "The playground runs computation only. Helix records, for every builtin and " +
        "method, whether it is pure and which capability it needs — filesystem, network, " +
        "the clock — and this endpoint refuses anything that is not pure, using the " +
        "binary's own registry rather than a hand-written blocklist. Install Helix to " +
        "run the whole language.",
    });
  }

  const bin = await findBinary();
  if (!bin) {
    return NextResponse.json(
      { error: "No helix binary found — build one with `cargo build --release`." },
      { status: 503 }
    );
  }

  const dir = await fs.mkdtemp(path.join(os.tmpdir(), "helix-play-"));
  const file = path.join(dir, "main.helix");
  try {
    await fs.writeFile(file, source, "utf8");
    const result = await new Promise<{ stdout: string; stderr: string; timedOut: boolean }>(
      (resolve) => {
        const opts: ExecFileOptions = {
          cwd: dir,
          timeout: TIMEOUT_MS,
          maxBuffer: MAX_OUTPUT,
          killSignal: "SIGKILL",
          // A DELIBERATELY minimal environment: the child gets a PATH and a HOME
          // pointing at its own temp dir, and inherits none of the server's secrets.
          // Cast because Node's ProcessEnv type insists on NODE_ENV, which is exactly
          // one of the things not being passed through.
          env: { PATH: process.env.PATH ?? "/usr/bin:/bin", HOME: dir } as unknown as NodeJS.ProcessEnv,
        };
        const child = execFile(
          bin,
          ["run", file],
          opts,
          (err: ExecFileException | null, stdout: string | Buffer, stderr: string | Buffer) => {
            // execFile reports a timeout by KILLING the child: `killed` is set and
            // `signal` is the kill signal. `code === "ETIMEDOUT"` is not how it arrives
            // with an explicit killSignal, so checking only that reported a timed-out
            // run as empty output — confusing, and caught by the adversarial test.
            const e = err as (NodeJS.ErrnoException & { killed?: boolean; signal?: string }) | null;
            const timedOut = Boolean(
              e && (e.killed || e.signal === "SIGKILL" || e.code === "ETIMEDOUT")
            );
            resolve({
              stdout: String(stdout).slice(0, MAX_OUTPUT),
              stderr: String(stderr).slice(0, MAX_OUTPUT),
              timedOut,
            });
          }
        );
        child.on("error", () => resolve({ stdout: "", stderr: "failed to start", timedOut: false }));
      }
    );

    if (result.timedOut) {
      return NextResponse.json({
        stdout: result.stdout,
        stderr: `timed out after ${TIMEOUT_MS} ms`,
        timedOut: true,
      });
    }
    return NextResponse.json({ stdout: result.stdout, stderr: result.stderr });
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
}
