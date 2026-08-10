// Build-time readers for the repository's OWN content. Nothing here is a copy: the site
// renders docs/, examples/ and bench/ straight from the files the gate checks, so the
// site rots exactly as much as the repo does — which is to say, not at all.
import fs from "node:fs";
import path from "node:path";

/** The repo root, one level above website/. */
export const REPO = path.resolve(process.cwd(), "..");

export function readRepoFile(rel: string): string {
  return fs.readFileSync(path.join(REPO, rel), "utf8");
}

export interface ExampleFile {
  slug: string;
  /** Path relative to the repo root, for citation. */
  rel: string;
  title: string;
  /** The leading `#` header comment, as prose. */
  intro: string[];
  source: string;
}

/**
 * The runnable language examples, in a deliberate teaching order rather than
 * alphabetical — a tour should build on itself.
 */
const TOUR_ORDER = [
  "tour",
  "bindings",
  "operators",
  "strings",
  "interpolation",
  "collections",
  "missing-data",
  "control-flow",
  "match",
  "functions",
  "closures",
  "recursion",
  "records",
  "tuples",
  "slicing",
  "scoping",
  "named-arguments",
  "error-handling",
  "errors",
  "typed",
];

function titleFor(slug: string): string {
  const explicit: Record<string, string> = {
    tour: "A first tour",
    "missing-data": "Missing data",
    "control-flow": "Control flow",
    "named-arguments": "Named arguments",
    "error-handling": "Errors you can catch",
    typed: "Type annotations",
  };
  if (explicit[slug]) return explicit[slug];
  return slug.charAt(0).toUpperCase() + slug.slice(1).replace(/-/g, " ");
}

export function listExamples(): ExampleFile[] {
  const dir = path.join(REPO, "examples/language");
  const files = fs
    .readdirSync(dir)
    .filter((f) => f.endsWith(".helix"))
    .map((f) => f.replace(/\.helix$/, ""));

  const ordered = [
    ...TOUR_ORDER.filter((s) => files.includes(s)),
    ...files.filter((s) => !TOUR_ORDER.includes(s)).sort(),
  ];

  return ordered.map((slug) => {
    const rel = `examples/language/${slug}.helix`;
    const source = readRepoFile(rel);
    // The leading `#` block is the file's own introduction.
    const intro: string[] = [];
    for (const line of source.split("\n")) {
      const t = line.trimStart();
      if (t.startsWith("##")) break;
      if (t.startsWith("#")) {
        const body = t.slice(1);
        intro.push(body.startsWith(" ") ? body.slice(1) : body);
      } else if (t === "" && intro.length > 0) {
        break;
      } else if (t !== "") {
        break;
      }
    }
    return { slug, rel, title: titleFor(slug), intro, source };
  });
}

export interface DocPage {
  slug: string;
  rel: string;
  title: string;
  markdown: string;
}

/**
 * Curated docs, in reading order. The repo has more markdown than belongs on a website
 * (ROADMAP is an engineering log, not documentation), so this list is explicit rather
 * than a directory scan — and anything added to docs/ is invisible here until someone
 * decides where it belongs, which is the right default for a reference.
 */
const DOC_ORDER: { slug: string; file: string; title: string }[] = [
  { slug: "execution-engine", file: "execution-engine.md", title: "The three engines" },
  { slug: "comments-and-docs", file: "comments-and-docs.md", title: "Comments & doc-tests" },
  { slug: "integer-semantics", file: "integer-semantics.md", title: "Integer semantics" },
  { slug: "syntax-and-dx", file: "syntax-and-dx.md", title: "Syntax & DX" },
  { slug: "memory-safety", file: "memory-safety.md", title: "Memory & safety" },
  { slug: "vectorized-kernels", file: "vectorized-kernels.md", title: "Vectorized kernels" },
  { slug: "caching-and-memory", file: "caching-and-memory.md", title: "Caching & memory" },
  { slug: "python-interop", file: "python-interop.md", title: "Python interop" },
  { slug: "testing", file: "testing.md", title: "Testing" },
  { slug: "deployment", file: "deployment.md", title: "Deployment" },
];

export function listDocs(): DocPage[] {
  return DOC_ORDER.filter((d) => fs.existsSync(path.join(REPO, "docs", d.file))).map(
    (d) => ({
      slug: d.slug,
      rel: `docs/${d.file}`,
      title: d.title,
      markdown: readRepoFile(`docs/${d.file}`),
    })
  );
}

export function getDoc(slug: string): DocPage | undefined {
  return listDocs().find((d) => d.slug === slug);
}
