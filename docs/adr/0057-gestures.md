# ADR 0057 — Gestures: the page drags, the app decides

- **Status:** Accepted — implemented in `std/ui.helix` and `src/view.rs`.
- **Date:** 2026-11-07
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0055](0055-apps.md) (the app runtime), [ADR 0056](0056-design-engine.md)
  (springs with velocity, which this ADR hands a throw to).

## Context

ADR 0056 left "drag gestures with velocity hand-off" undone. A desktop or phone app needs at
least four drag gestures:

- a divider between panes;
- a list put in order by hand;
- a toast swiped away;
- a sheet pulled down to close.

The app model is that a view is data and every change is an event through `update`. A drag
does not fit it if every pointer move is a round trip: even on 127.0.0.1 the node would trail
the finger by a frame or more, and a thrown thing would lose the speed it was thrown at.

## Decision

**The page does the dragging; the app decides the outcome.** A view says what can be dragged,
which way, and which message hears the result — props, like every other behaviour (no handler
is ever sent to the page). While a pointer is down the page's own script moves the node, on the
compositor, with no round trip. When it is let go the app is sent one event. Its next frame
puts the node where the app decided: kept, moved or gone. The node springs there from where it
was let go, starting at the speed it was thrown — the same spring solver and velocity hand-off
as ADR 0056's layout animations.

### The props

- **`drag: msg`**, with **`drag_axis`** (`x`, `y`, `both`, or a direction: `down`, `up`, `left`,
  `right`) and **`drag_commit`** (px).
  - Let go, the app gets `{msg, value, drag: {dx, dy, vx, vy}}`. `value` is the node's own
    `value`, exactly what a click on it would send, so a toast swiped away sends what its close
    button sends, and an app needs no new case for it.
  - With `drag_commit`, a throw is sent only if its momentum would carry it that far. Momentum
    is the pointer's speed over its last 100 ms, projected 0.2 s ahead. A smaller throw springs
    back, and the app hears nothing.
  - A direction means the other way resists, with UIKit's rubber-band curve
    (`(1 − 1/(x·0.55/d + 1))·d`).
- **`reorder: msg`** on a container. Its keyed children can be dragged into a new order, by a
  child's **`handle`** if it has one. On a touch screen a row without a handle is lifted by a
  350 ms press, so a swipe still scrolls the list. The rows the dragged one passes slide out of
  its way while it moves. Let go, the app gets `{key, from, to}`; `ui.reordered(items, move)`
  applies it, and refuses a move that does not fit the list (stale, forged or out of range).
- **`resize: msg`** on a divider. As it moves, it sends the size the pane before it would
  have, with `done: true` on the last one.
- **`handle: true`** marks the part of a draggable node that grabs it, such as a sheet's
  grabber or a row's grip.

### Every gesture has a keyboard path

- A divider is a focusable `separator` (its value and orientation say where it is), and its
  arrow keys move it 16 px, or 64 px with Shift.
- Alt with ↑ or ↓ moves the focused row of a sortable list.
- A sheet closes with Escape and its close button, and a toast with its close button.

What the focused thing does with a key comes before the app's own shortcuts. The keys of a
composite widget (tree, menu, tabs, radio group, divider) are never taken by a shortcut.

### The page knows when the app has answered

The server answers a POST before it runs `update`, and sends no frame when nothing changed. So
the page cannot tell from the frames alone when a dropped node's fate has arrived. A gesture's
last event therefore carries a `seq` made up by the page: its tab and a count. The frame after
it lists that `seq` in `acks`, and the server sends this frame even when the view did not
change. The page settles the gesture on that frame: before it, from where the node was let go;
after it, to wherever the frame put it.

If no answer comes within 2 s (an update that raised is logged, and the frame then carries no
change), the node goes back where it was. A `seq` is echoed only if it is a short string.

### Components

The gestures are used by these components:

- **`split_view(first, second, size, msg)`**, and `ui.resized(v, min, max)` to keep a reported
  size in bounds.
- **`sortable(id, label, items, msg)`**, and `ui.reordered`.
- **Swipes on existing components:**
  - `toasts` can be swiped sideways (`drag_commit: 80`);
  - `sheet` can be pulled down by its grabber (`drag_commit: 96`).
- **`tree(id, label, nodes, open, selected, toggle, select)`**. It is WAI-ARIA's tree view: one
  row is in the Tab order. ↑ ↓ move, → opens a row or goes to its first child, ← closes a row
  or goes to its parent, Home and End go to the ends, and Enter selects. Only what is open is
  drawn.

Three more components don't use a gesture but finish the set:

- **`disclosure` and `accordion`.** The body is always in the page, so its height can spring
  open (grid rows `0fr` to `1fr`). A closed body is `inert`.
- **`popover`**, a dialog anchored to its button: focus moves in, and Escape or a press
  outside closes it.
- **`spinner`**, and `progress(label, missing)` for a bar with no known end.

The file manager uses `split_view` and `tree` for a resizable sidebar of folders. Its tree is
read with the same `list_folder` as the list, so a folder in the tree is one the app listed and
a link is never in it. The app opens only a key the tree holds.

## Consequences

- **The app's model stays the only state that matters.** What a drag looked like on the way is
  the page's; where things are is the app's, and it replays.
- **A drag costs no round trips while it moves.** A divider is the exception: it sends its
  size as it goes, coalesced to the last value waiting, because the layout on both sides is the
  app's.
- **No new authority.** The page gains no handler and no new kind of message: a gesture's
  event is the same `{msg, value}` a click sends, plus numbers the app may ignore. `seq` is
  checked and only echoed.
- **Not done:**
  - dragging between containers (a row onto a folder);
  - a pinch, or a pan with inertia over a canvas;
  - snapping a divider to points.
