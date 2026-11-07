# ADR 0058 — Motion v3: container transforms, navigation, cascades and scroll-driven titles

- **Status:** Accepted — implemented in `std/ui.helix`.
- **Date:** 2026-11-07
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0056](0056-design-engine.md) (springs and shared elements),
  [ADR 0057](0057-gestures.md) (the page drags, the app decides).

## Context

ADR 0056 made motion physical: springs with velocity, layout animations, and shared elements.
Three things the best toolkits do were still missing.

**A shared element could only stretch.** A tile morphing into a larger card was a `scale()` of
the new node, so its text and corners stretched on the way. Material calls the right motion a
*container transform*, and the App Store's cards are the familiar example.

**There was no navigation.** A phone app is a stack of screens: one is pushed on and slides in,
it is popped and slides out, and it is swiped back from the edge. The swipe can stop anywhere,
and the screen beneath follows the finger the whole way.

**Lists arrived all at once.** Every item in a list that filled appeared in the same frame. A
short cascade reads as one motion instead of a flash.

**Titles did not follow the scroll.** Apple's large title shrinks into the toolbar as the page
scrolls under it. Driven from script, that costs a frame per scroll event.

## Decision

### Container transforms

A new node with content takes the `share` name of an old one at another size. It is not
stretched: it is revealed.

- **It starts in the old node's box.** It is moved there, and a `clip-path: inset(… round r)`
  opens from the old size to its own, on one spatial spring. Its own corner radius is used, so
  the corners stay true.
- **A node smaller than the one it replaces is scaled down to it instead,** because there is
  nothing to reveal.
- **Its content fades in** on an effects spring, a little after it starts to move.
- **The old node becomes a ghost.** If it has left the page, it is put back for the moment as
  a ghost of itself in a fixed layer: inert, `aria-hidden`, and without ids. It moves and
  stretches toward the new box while it fades out in the first third of the move, so the eye
  follows one thing changing shape.
- **An empty node keeps the old morph.** A segmented control's thumb has nothing in it to
  stretch.

### A navigation stack

`ui.stack(id, screens, back)` draws the top two of `screens`.

- **The top screen** arrives with the `push` preset: it slides in from the side on a spatial
  spring, solid (it does not fade). A popped screen leaves the same way.
- **The screen beneath** steps back a third of the way and dims, and it is `inert`. While the
  top screen is dragged, the page writes the drag's progress to `--drag-p` on the stack, so the
  screen beneath follows the finger in CSS, with no script per frame.
- **The top screen is dragged from a 22 px edge** (a `handle`), toward the right only, and is
  committed at 120 px of momentum. That is ADR 0057's `drag` exactly. Its back button names
  the screen it returns to.

Two smaller changes to how things leave:

- A solid preset now leaves on a spatial spring, not the effects spring: a whole screen
  sliding away is a movement, not a fade.
- When a gesture lands, the node's offset is cleared with its CSS transition held off, so a
  spring and a transition never both move it.

### Cascades

Nodes that arrive in one frame into one parent arrive one after another: 28 ms apart, at most
twelve steps (336 ms), with nothing to set. One node arriving alone is not delayed.

### Scroll-driven large titles

`ui.large_title(text)` sits in a shell's content and `ui.toolbar_title(text)` in its toolbar.

- **The shell's scroller names a scroll timeline** (`--hx-page`), and its frame widens the
  timeline's scope, so the toolbar can follow a scroller it is not inside.
- **As the page scrolls,** the large title fades and settles over the first 64 px, and the
  small one fades in from 36 to 72 px.
- **Both run on the compositor,** as CSS scroll-driven animations.
- **Where the browser has no scroll timelines,** the large title stays and the small one is
  never shown.
- **Reduced motion keeps them,** because they follow the person's own scrolling; they are
  not motion played at them.

## Consequences

- The gallery's "Navigation and motion" card shows a mail stack that can be swiped back, and an
  album grid whose tiles open into cards and cascade back. The file manager's folder title
  collapses into its toolbar.
- The browser test drives each in Chromium, with motion and with reduced motion:
  - a push, and a swipe-back that is short or committed;
  - the back button;
  - the ghost and clip of a container transform, and the ghost's removal;
  - the cascade's rising delays.
- No new authority: every behaviour here is data in the view, played by the page's fixed
  script.
- **Not done here:**
  - swipe actions on list rows;
  - pull-to-refresh;
  - carousels;
  - dragging between containers;
  - animated icons;
  - interactive charts.

  These are the rest of the motion milestone.
