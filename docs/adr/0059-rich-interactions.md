# ADR 0059 — Rich interactions: swipe actions, boards, carousels, symbols, readable charts

- **Status:** Accepted — implemented in `std/ui.helix` and `src/view.rs`.
- **Date:** 2026-11-08
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0057](0057-gestures.md) (the page drags, the app decides),
  [ADR 0058](0058-motion-v3.md) (the first half of this milestone).

## Context

ADR 0058 left the rest of the motion milestone undone: swipe actions on rows, pull-to-refresh,
carousels, dragging between containers, animated icons and interactive charts. Each is
expected by a phone or a desktop user, and each has to fit the model ADR 0057 set out. The
view says what can happen; the page plays it; the app decides the outcome; and there is
always a keyboard path.

## Decision

### Swipe actions (`ui.swipe_row`, `ui.swipe_list`, `ui.swiped`)

A row's content is a `drag` node on the x axis over its actions, as in Mail.

- **Opening and closing.** Let go, the app is sent the row's key with `drag: {dx, vx}`.
  `ui.swiped(open, e, n)` turns that into the row now open. A row opens when its momentum
  would carry it past half the actions' width (76 px each), counting from where it was. A tap
  on an open row closes it, and only one row is open at a time.
- **No CSS transition.** The open row is a class that moves the content by the actions'
  width. The content is a `layout` node, so a close by a tap animates with FLIP, and a close
  by a drag lands with the drag's own spring.
- **Keyboard.** Focus on an action opens the row (`:focus-within`), so the actions are
  reachable without a swipe.

### Between containers, and drop targets

These are two new props:

- **`reorder_group`** on containers lets them trade rows. While a row is dragged it moves in
  both directions, and the container under the pointer makes room where it would go. Let go,
  the source container's message is sent `{key, from, to, from_list, to_list}`, where the
  lists are the containers' `value`s. `ui.moved_between(columns, move)` applies it, and
  refuses a move that does not fit.
- **`drop`** on any node makes it a target. A row held over it lights it up
  (`hx-drop-over`), and a row let go over it sends `{msg, value, dropped: key}`.

The row lands as the new node in its new container, springing from where it was let go. The
old node is removed at once rather than playing an exit. On the keyboard, Alt with ← and →
moves a card to the neighbouring container of its group. `ui.board` builds Kanban columns
from these.

### Carousels (`ui.carousel`)

- **The platform's scroll.** The slides are a row that scrolls with `scroll-snap-type: x
  mandatory`, so a flick, a trackpad and a touch screen all carry their own momentum and
  snap. Nothing about the scrolling is the page script's.
- **Buttons and dots.** These move the track. The page marks the dot of the slide in view as
  current, turns off the button at either end, and sends `{msg, value: index}` when the slide
  changes. The first look sends nothing.
- **Accessibility.** The region and slides carry WAI-ARIA's carousel role descriptions.

### Symbols (`ui.symbol`)

These effects follow SF Symbols':

- **`bounce`, `wiggle` and `pulse`** are entrance presets on springs with less damping than
  the system's, so they are felt.
- **`swap`** scales the new symbol in as the old one fades, in one grid cell.

The symbol's key includes a `trigger`: a count, or the state it shows. Changing the trigger
makes a new node, so the effect plays exactly when the app says so. Under reduced motion it
simply changes.

### Readable charts

A line chart with up to 120 points gets a band per point of its first series. A bar chart
gets a band per bar.

- **Hover or keyboard focus** shows a crosshair, each series' dot and a box of values.
- **It is CSS.** The bands are always in the page and only become visible, so it costs
  nothing per frame.
- **Keyboard.** One band is in the Tab order, ← → Home End move between bands, and each band
  says its values aloud. A chart with bands is a `group`, not an `img`, because its parts can
  be focused.
- **Contrast.** The values are drawn in the text colour. The series colours mark only the
  dots, because those colours are proven to 3:1, not 4.5:1.

## Not done: pull-to-refresh

On the web, a pull is the page's own scroll. To take it over, a list would have to give up
native scrolling on touch screens (`touch-action`) or track every touch while it is scrolled
to the top. Both make scrolling worse for every user to add a gesture a refresh button does
as well. It waits for a design that keeps native scrolling.

## Consequences

- The gallery's "Rich interactions" card demonstrates each of these, and its charts are
  readable by pointing.
- The browser test drives each in Chromium, with motion and with reduced motion.
- The corpus pins the helpers' decisions, including moves that do not fit, and the markup.
- **No new authority:**
  - every event is a `{msg, value}` a click could send, with numbers or a key besides;
  - the carousel's buttons only scroll an element by its id;
  - nothing is a handler.
