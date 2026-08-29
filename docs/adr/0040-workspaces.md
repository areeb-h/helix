# ADR 0040 — a manifest is a package; a workspace is the module root

**Status:** Accepted & implemented
**Date:** 2026-08-29

## The question

`helix.toml` means two things that a repository of several packages needs to keep apart:

1. **"This directory is a distributable package."** That is what `helix add <name> --path
   <dir>` consumes, and what a registry will publish.
2. **"In-project imports are anchored here."** `project_context` walks up from the entry
   file and stops at the nearest `helix.toml`; `import a.b` then means
   `<that dir>/a/b.helix`.

One file, two meanings. A repo with three packages had to choose which one it got.

## How it was found

A field report on `helix-ui` filed an import failure, then corrected its own filing twice
before the mechanism was right — including a fix from this side that did not work. The
measured table is the whole argument:

| root `helix.toml` | `ui/helix.toml` | `import ui.parse` from `ui/render.helix` |
|---|---|---|
| absent | present | **cannot find module `ui.parse`** |
| **added** | present | **cannot find module `ui.parse`** |
| present | **removed** | ok |

Adding a manifest at the root does not help, because the walk stops at the *nearest* one.
The nested manifests are not a mistake — `ui`, `web` and `nn` are meant to be
distributable — so the repo was forced to pick:

- **manifests per package** → each package anchors at itself, so a file inside it cannot
  be checked directly: `import ui.parse` written in `ui/render.helix` resolves to
  `ui/ui/parse.helix`.
- **one manifest at the root** → anchoring is right, and nothing is a package any more.

There was no third option, because nothing spanned several packages.

**What it cost.** Thirteen of twenty files could not be checked directly. `--lint` read
only the file it was handed, so nothing reached them that way either, and an O(n²)
accumulation in an imported training loop survived a whole release cycle.

## Decision

A manifest may carry a `[workspace]` table naming member directories:

```toml
[package]
name = "helix-ui"
version = "0.1.0"

[workspace]
members = ["ui", "web", "nn"]
```

`project_context` keeps its "nearest manifest" rule, then asks one further question: does
an ancestor manifest claim this directory as a member? If one does, **that ancestor is the
module root**, and the member's `helix.toml` goes on meaning only "this is a package".

- A member keeps its manifest, so `helix add ui --path ui/` is unchanged.
- `import ui.parse` means `<workspace root>/ui/parse.helix` from every file, which is the
  property `project_context`'s own doc comment says anchoring exists to provide.
- A directory whose manifest **no** ancestor claims anchors at itself, exactly as before.
  Standalone packages are untouched, and so is every project that does not opt in.
- Nesting is one level: a workspace root never looks further up. Two answers to "where is
  the root" is the defect being fixed.

**A listed member that does not exist is refused**, checked for every member rather than
only the one being loaded. Left silent, a typo would self-anchor that package — precisely
the failure this table ends — and surface far away as a confusing import error inside it.

**A member's own `[dependencies]` is refused**, not ignored: it would resolve against a
manifest that is no longer the project root, so it would declare something and do nothing.
Per-member resolution is a real feature and a separate decision.

## Why not the alternatives

**Prefer the outermost manifest instead of the nearest.** No new syntax — and it silently
changes what every existing multi-manifest project resolves to, with no way to opt out. It
also makes a package's meaning depend on what happens to sit above it, so cloning a package
into a parent that has a manifest changes its imports.

**A separate marker file (`helix-workspace.toml`).** Sidesteps `deny_unknown_fields`
entirely. Rejected because it puts project identity in two files that can disagree, and
"which file does this go in" is the question this ADR exists to stop asking.

**Infer the root from the import that failed.** Guessing. A root that depends on whether an
import happened to resolve is not a root.

## The compatibility event

`Manifest` is `#[serde(deny_unknown_fields)]`, and that is deliberate: a silently discarded
`[capabilities]` block once looked like it restricted authority and did nothing, which is
the worst shape a security control can have. So a `[workspace]` table is **refused** by
0.7.0 and earlier.

That is the correct direction — loud, not silent — but the wrong words: a reader is told
their manifest is malformed when it is merely newer. The unknown-key refusal now names the
running build's version and points at the project's `helix` requirement, so the two cases
can be told apart. A project using `[workspace]` declares `helix = ">=0.8.0"`.

This is the same transitional wart `docs/RELEASING.md` records for the `-dev` marker: the
parser ships one release ahead of the first file that uses the feature.

## Consequences

- A multi-package repo gets both meanings at once, which it could not before.
- `helix check ui/render.helix` works on a member file — the case that was impossible.
- `--lint` walking the import graph independently fixes *coverage* from an entry point.
  The two are complementary: traversal reaches files through an entry, `[workspace]` makes
  each file checkable on its own.
- The failed-import diagnostic names the anchoring directory and the `helix.toml` that
  chose it, so when a member is *not* listed the reader is told where the root actually is
  rather than deducing it from three experiments.

## Open

- **Glob members** (`members = ["packages/*"]`). Deferred: exact paths first, because a
  glob matching nothing is a silent anchor change and needs its own refusal rule.
- **Per-member dependencies.** An error today, a feature later.
