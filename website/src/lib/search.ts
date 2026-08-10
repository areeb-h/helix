import { listDocs, listExamples } from "./content";
import { extractDocBlocks } from "./helix";
import api from "@/data/api.json";

export interface SearchEntry {
  /** Where it lives. */
  href: string;
  /** What it is: "Docs", "Tour", "Reference", "Guide". */
  kind: string;
  title: string;
  /** A short excerpt shown under the title. */
  body: string;
}

function squash(s: string, max = 150): string {
  const t = s.replace(/\s+/g, " ").trim();
  return t.length > max ? t.slice(0, max - 1) + "…" : t;
}

/**
 * Build the search index at BUILD TIME from the same sources the pages render.
 *
 * Deliberately indexes headings and their first paragraph rather than full text: a
 * language reference is mostly code and tables, and full-text indexing them produces a
 * large payload whose hits are mostly noise. Every method and builtin name is indexed
 * individually, because "does Helix have X?" is the question this site exists to answer
 * fast — the `scan` incident being the proof.
 */
export function buildIndex(): SearchEntry[] {
  const out: SearchEntry[] = [];

  // --- Guides -------------------------------------------------------------
  out.push(
    {
      href: "/start",
      kind: "Guide",
      title: "Get started",
      body: "Install, hello world, a real program, the REPL, and building a standalone binary.",
    },
    {
      href: "/bench",
      kind: "Guide",
      title: "Benchmarks",
      body: "Ten kernels against C, Rust, Go, CPython and NumPy — wins and losses, with the methodology.",
    },
    {
      href: "/playground",
      kind: "Guide",
      title: "Playground",
      body: "Run Helix snippets and see the output.",
    }
  );

  // --- Docs: one entry per heading ----------------------------------------
  for (const doc of listDocs()) {
    out.push({
      href: `/docs/${doc.slug}`,
      kind: "Docs",
      title: doc.title,
      body: squash(doc.markdown.replace(/^#.*$/gm, "").slice(0, 400)),
    });

    const lines = doc.markdown.split("\n");
    for (let i = 0; i < lines.length; i++) {
      const m = /^(#{2,3})\s+(.+)$/.exec(lines[i]);
      if (!m) continue;
      // The first non-empty, non-heading line beneath the heading.
      let body = "";
      for (let j = i + 1; j < Math.min(i + 8, lines.length); j++) {
        const t = lines[j].trim();
        if (t && !t.startsWith("#") && !t.startsWith("|") && !t.startsWith("```")) {
          body = t;
          break;
        }
      }
      const title = m[2].replace(/[`*_]/g, "").trim();
      out.push({
        href: `/docs/${doc.slug}`,
        kind: doc.title,
        title,
        body: squash(body),
      });
    }
  }

  // --- Tour: one entry per example file, plus each documented example ------
  for (const ex of listExamples()) {
    out.push({
      href: `/tour#${ex.slug}`,
      kind: "Tour",
      title: ex.title,
      body: squash(ex.intro.join(" ")),
    });
    for (const b of extractDocBlocks(ex.source)) {
      if (b.examples.length === 0) continue;
      const first = b.examples[0].code[b.examples[0].code.length - 1] ?? "";
      out.push({
        href: `/tour#${ex.slug}`,
        kind: "Example",
        title: first,
        body: squash(b.prose.join(" ")),
      });
    }
  }

  // --- Reference: every method and builtin, individually ------------------
  const methods = api.methods as Record<string, { name: string; effect: string }[]>;
  for (const [type, list] of Object.entries(methods)) {
    for (const m of list) {
      out.push({
        href: `/reference#type-${type}`,
        kind: `${type} method`,
        title: `${m.name}()`,
        body: m.effect === "pure" ? `A pure method on ${type}.` : `On ${type} — effect: ${m.effect}.`,
      });
    }
  }
  for (const b of api.builtins as { name: string; category: string; effect: string }[]) {
    out.push({
      href: `/reference#cat-${b.category}`,
      kind: "Builtin",
      title: `${b.name}()`,
      body: b.effect === "pure" ? `Builtin (${b.category}).` : `Builtin (${b.category}) — effect: ${b.effect}.`,
    });
  }

  return out;
}
