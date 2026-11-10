# ADR 0062 — Made for agents to get right: measured, not guessed

- **Status:** Accepted
- **Date:** 2026-11-10
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0060](0060-the-ui-platform.md), [ADR 0061](0061-markup.md),
  `AGENTS.md`.

## Context

Much Helix code will be written by AI agents. An agent makes the same mistakes a newcomer does,
at speed. It acts on whatever an error message says, and it believes the documentation it can
reach. So for Helix "easy to use" means three things:

- the first thing an agent tries is either right, or refused with the fix;
- nothing fails silently;
- every question can be answered by the binary.

Guessing which mistakes agents make is unreliable, so this was measured.

## The study

In round one, five fresh agents each built one thing:
- a todo app;
- a sales dashboard over a CSV;
- a three-step signup wizard;
- a memory game with timers and keys;
- a log-analysis CLI with tests.

Each had only the `helix` binary, `AGENTS.md`, the README and the example apps; reading the
implementation was not allowed. Each kept a log of every mistake: what it wrote, the error,
whether the error named the fix, and how it found the fix.

**What they found:**
- **All five finished,** and four had a first full draft that type-checked. The language core
  was not the problem. The errors that existed were mostly good ones.
- **Their friction was finding things:**
  - the app library had no `doc`/`describe` entries;
  - multi-word search dropped words without saying so;
  - `--lint` buried the user's notes under notes about the library.
- **The worst failures made no noise**, or gave the wrong advice:
  - `onDoubleClick` was answered with "use `doubleclick`", an attribute that is never sent;
  - a mistyped `clik="save"` button did nothing;
  - a click's `value={7}` was `7` in a replay but `"7"` in a browser;
  - `{n} {n}` rendered two spaces;
  - uncalled `fn test_*` were skipped;
  - `x == missing` passed `check` and failed at run time.

## Decision

Treat an agent's mistake as a defect of the language or its tools, and fix it at the most
reliable of three levels:

1. **Make it work.** If the agent's guess is a reasonable reading, make it mean that:
   - `[head, ...rest]`;
   - `df.sort(-@price)` and `"-price"`;
   - a record field inside a query;
   - `helix eval` with imports;
   - typed click values;
   - `fn test_*` functions run as tests;
   - `ui.after` for one-shot timers;
   - the 3D utilities.
2. **Refuse it where it is written, naming the fix.** If the guess can never be right, refuse it
   at that spot and say what to write:
   - mistyped props;
   - React spellings (`onClick`, `htmlFor`, `tabIndex`);
   - events Helix lacks;
   - `x == missing`;
   - `&&` and `||`.

   Each error names the Helix spelling, or says what to use instead.
3. **Note it when it might be intended.** When the code might be deliberate, `helix check --lint`
   says what it sees:
   - a class that looks like a utility but produces no CSS;
   - HTML that cannot nest (a list inside a button).

A rule that can only be documented is the last resort. Even then, the binary answers for it:
- `helix doc ui` and `helix doc ui.<name>` are read from the library's own source;
- `helix describe events` lists every event and key chord;
- `helix search` matches every word and shows the closest matches.

`AGENTS.md` gained a "Building an app" section, with a complete small app that is type-checked
and replayed on all three engines.

## Consequences

- **Measured again.** A second round of fresh agents, on new tasks, measures what changed; the
  method is the measurement. When that round finds new kinds of mistakes, they are fixed the
  same way.
- **Tests.** Every fix has a test that would fail without it:
  - `tests/corpus/agent_affordances.helix` (three engines);
  - `markup::tests::the_mistakes_agents_make_name_their_fix`;
  - the CLI tests for `helix test`, search and the busy port;
  - the browser test that holds animation frames to force the typing race.
- **Stricter.** Some programs that ran before are now refused: `x == missing` against the
  literal, and `&&`. Each was a bug every time; the refusal says what to write instead.
