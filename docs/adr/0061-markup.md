# ADR 0061 — Markup: views written as elements, read by the parser

- **Status:** Accepted — implemented in `src/markup.rs` (reading) and `src/parser.rs` (meaning).
- **Date:** 2026-11-08
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0055](0055-apps.md) (views are data), [ADR 0060](0060-the-ui-platform.md)
  (the platform; markup is its second layer).

## Context

A Helix view is data: `{tag, props, children}` records, built with `ui.el(tag, props,
children)`. That is right for the runtime: a view diffs, replays and tests without a browser,
and the page script never runs app code. It is wrong for people. A card with a heading, a list
and a button is four nested calls whose props records and children arrays have to be counted.
Everyone who has written a web UI, and every code-writing AI, thinks of that card as markup.

## Decision

Helix source can contain markup, and the parser turns it into the view data `ui.el` builds.
Nothing after the parser changes: the checker, the three engines, `view_html` and `view_diff`
see only records, arrays and calls.

```helix
fn plan_row(p) = <li key={p.id} class={["row", if p.best then "best" else missing]} click="pick" value={p.id}>
  <strong>{p.name}</strong> costs {p.price} a month
  {if p.best then <Badge label="Best" /> else missing}
</li>
```

### Where markup starts

A `<` followed by a letter or `>` starts markup, unless a value has just ended. A value has
ended after a name, a literal, `)`, `]` or `}`. So these keep their meaning:
- `a <b` and `xs[0]<limit` still compare;
- `<<` still shifts;
- `< b` (with a space) still compares, including at the start of a continued line.

Markup can therefore go anywhere an expression can:
- after `=`, `=>`, `then`, `else` or `do`;
- in a list or a call;
- at the start of a line.

No program in the repository changed meaning.

### What it means

| Markup | View data |
|---|---|
| `<div …>kids</div>`, `<br />` | `{tag: "div", props: {…}, children: [kids]}` |
| `class="a"`, `click="save"`, `key={k}` and the other runtime props (`id`, `role`, `value`, `input`, `change`, `submit`, `enter`, `layout`, `drag`, `reorder`…) | the prop of that name; `drag-axis` is `drag_axis` |
| `aria-label="Close"` | `aria: {label: "Close"}` |
| any other attribute (`href`, `type`, `data-row`, `disabled`) | `attrs`, spelled as written |
| `value=` on `input`, `option`, `progress`, `meter`, `data` | HTML's `value` (elsewhere it is what a click sends) |
| a bare attribute | `true` |
| `"…"` | a Helix string, so `"{n} items"` interpolates |
| `{expr}` | the expression |
| `{...rec}` | the record's fields as props, a later attribute winning |
| text | text nodes, indentation folded as JSX folds it; `&amp;`, `&lt;`, `&#x2192;` decode |
| `<>…</>` | an array of the children |
| `<Card title="x">kids</Card>` | `card(title: "x", children: [kids])`, or `Card(…)` if that is the name defined |
| `<ui.Button caption="Save" />` | `ui.button(caption: "Save")` |

A component is any function: its attributes are its named arguments, and its children are the
argument `children`. So every existing `std.ui` component works as a tag with no change, and
defaults fill in as for any call.

A child hole can hold:
- a node;
- a string or a number, which becomes text;
- a list, which is flattened in place;
- `missing` or a Bool, which renders nothing.

So `{items.map(row)}`, `{n} left` and `{if busy then <Spinner /> else missing}` need no
wrapping. The renderer does this flattening (`view.rs`), so a view built with `ui.el` gets it
too.

`class` also takes a list: `class={["row", if on then "on" else missing]}` joins its strings
and skips `missing`, Bools and empty strings, as `clsx` does. `style` also takes a string:
`style="--fill: 40%"`.

### Errors

A hole is parsed from the line and column where it was written, so errors point into the
markup, including type and runtime errors inside a hole. Markup's own mistakes have their own
messages:
- an unclosed tag, and a closing tag that closes the wrong one;
- a `<` or `}` in text;
- `class=4` (a number goes in braces);
- `className`;
- `onClick` (a view sends messages; it cannot carry handlers);
- a component that is not defined.

### Safety

Markup adds no authority:
- What it builds is what `ui.el` builds, and the renderer's rules apply unchanged: refused
  tags, refused `on*` attributes and inert `javascript:` URLs.
- `onClick` and the other `on*` names are refused when the file is parsed.
- Text and attribute values are escaped when written out, as before.

## Consequences

- **`helix fmt`** treats an element as one token and keeps it as written: it re-indents the
  line it starts on but never reflows inside it, as with a multi-line string.
- **Tests:**
  - `tests/corpus/ui_markup.helix` runs on all three engines. It shows that markup equals the
    `ui.el` form, and covers every row of the table above, comparisons, a diff of two markup
    views, and the utility classes a markup view uses.
  - Unit tests in `src/markup.rs` cover whitespace, holes and errors.
- **Not yet:**
  - formatting inside markup;
  - `helix check` lints for accessibility (the platform's tools layer);
  - markup in `helix docs` examples.
