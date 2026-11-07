# ADR 0056 — The design engine: computed colour, concentric geometry, materials and spring motion

- **Status:** Accepted — implemented in `std/ui.helix`.
- **Date:** 2026-11-07
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0055](0055-apps.md) (the app runtime this styles).

## Context

ADR 0055's Phase 2 gave apps a component library styled by a hand-written stylesheet: one
light and one dark palette, fixed radii, three spring presets. A design lab drew the same app
three ways and the user wanted all three looks, an Apple-leaning feel with some of Google's
motion, and "buttery smooth" animation. It also showed what a hand-written stylesheet gets wrong:

- **Corners were inconsistent.** A row with a 10 px corner inside a list with a 14 px corner
  and 5 px of padding looks pinched. Apple's guidance (WWDC25, "concentricity") is that a
  nested shape's radius is its container's minus the padding between them. No fixed set of
  radii satisfies that for every container, padding and density.
- **Contrast was hoped for, not known.** Each palette was checked by axe on the pages the
  gallery happened to draw. A new accent colour meant a new, unchecked palette.
- **Motion lost its velocity.** A FLIP animation restarted from rest whenever its target
  changed. CSS easings carry no velocity at all.
- **Rendering was not adapted to the machine.** Blur costs a frame on a software rasteriser:
  there is no GPU in the sandbox the app was demonstrated in, and none on a CI runner.

## Decision

Styles are computed in Helix from a few choices. The client turns them into CSS custom
properties, and every component reads only those.

### 1. Colour: OKLCH roles from one seed, with contrast proven

`ui.design({seed | hue, material, shape, density, effects})` builds both palettes in OKLCH.
OKLCH is a perceptual space, so a palette built by lightness reads the same at any hue.

- **Gamut.** Each colour is fitted to sRGB by reducing chroma only. Lightness and hue are what
  the eye keeps.
- **Contrast.** Every text role is moved in lightness, by bisection, just far enough to reach
  its WCAG ratio (4.5:1 for text, 3:1 for control parts) on every surface it can sit on.
- **The proof.** `ui.contrast_report(design)` lists every pair the engine checked. The corpus
  test sweeps the hue circle in every material, plus extreme seeds such as pure red, black and
  white. All 1,440 pairs pass on all three engines.

Status hues are fixed, so red means danger in every design.

### 2. Geometry: concentric by construction

`ui.geometry(shape, density)` returns every radius and padding as numbers. Nested radii are
derived as `inner = max(4, outer − padding)`; a capsule stays a capsule at any size. The rule
covers these pairs:

- card → its contents;
- list → its rows;
- overlay → its items;
- sheet → its controls;
- window → a floating toolbar.

In the **soft** shape the outer radii are derived from the controls: a card's radius is a
control's plus the card's padding, so a button against a card's edge is exactly concentric with
it. **Round** makes controls capsules, as Apple's current controls are. **Sharp** keeps small
corners and lets the floor win. Density changes the paddings, and the radii follow them.

### 3. Materials are token sets

- **`tonal`:** solid surfaces layered by tone, as in Material.
- **`glass`:** translucent, blurred surfaces over a backdrop the design generates from the
  seed, with a specular sheen from the top edge.
- **`hybrid`** (the default): solid content, with glass only on the navigation layer —
  toolbars, rails, menus, sheets and toasts. This is Liquid Glass's model: the controls float
  over the content.

Components use only role classes (`mat-card`, `mat-overlay`, `mat-frame`), so a material is a
different set of token values, not different CSS.

Glass opacity is **solved**, not chosen. It is the least alpha at which text keeps 4.5:1 over
every part of the backdrop it can sit on, computed by compositing in 8-bit sRGB the way a
browser does.

### 4. Effects follow the machine

The client checks for GPU compositing; a software renderer sets `data-perf="light"`. In each of
these cases every glass surface is drawn as the solid surface of its role, the same colour role
without the blur:

- light performance;
- the platform's reduce-transparency setting;
- high contrast;
- `effects: "light"`.

`effects: "full"` overrides the check. Blur is only ever used where a surface floats over
content.

### 5. Motion: two families of springs, and velocity that carries

There are two families, after Material 3's physics system, each in fast, default and slow:

- **Spatial springs** (damping ratio 0.9) move things.
- **Effects springs** (critically damped) change how things look, because a fade that
  overshoots flickers.

Helix solves each one into a CSS `linear()` curve, so the GPU plays every frame.

The client solves the same physics with an **initial velocity**. A node whose layout changes
while it is still moving keeps its velocity and bends toward the new target. Each axis is its
own animation (`transform` and `translate`, composited), so both run on the compositor.

The **`share`** prop marks shared elements. A node with the same `share` name in two
consecutive frames morphs from the old box to the new one: a segmented control's thumb glides,
and a row can grow into its detail view.

Frames are applied once per display refresh, so a burst of frames is one layout. Under
reduced motion nothing moves.

### 6. Type, state layers and pressed shapes (from the platforms' own guidance)

- **Type scale.** One computed scale of text styles, after Apple's: large title, titles 1–3,
  headline, body, callout, footnote and caption, as `--type-*` and `.t-*`. Compact density
  scales it to a 13 px body, the macOS body size. Tracking is computed from the size by the
  dynamic-metrics curve Inter publishes for UI text (`ui.tracking(px)`). Small text opens up
  and large text draws in, which is what SF Pro's optical sizes do: 12 px is neutral and 13 px
  is −0.003 em. Apple's SF table gives −6/1000 em at 13 pt.
- **State layers.** Material's opacities: hover 8%, focus 10%, pressed 10%, dragged 16%, in
  the content's own colour. A disabled control's content is at 38%.
- **Pressed shapes.** As in Material 3's buttons, a pressed control's corners ease in, to 60%
  of the resting radius, on the fast spatial spring. A capsule's resting radius is written as
  half its height, not 999 px, so the morph interpolates over its whole length.
- **Springs.** The stiffnesses are Material 3's: 800 for fast, 380 for default and 200 for
  slow, from its Compose motion tokens. The damping (0.75, 0.85 and 0.9) sits between Material
  Expressive's playful 0.6 to 0.8 and Apple's critically damped "smooth". A small thing settles
  with a hint of give; nothing wobbles.

Apple's guidance this follows (WWDC25 "Meet Liquid Glass" and "Get to know the new design
system"):

- Glass is for the navigation layer that floats over content, never glass on glass.
- Only primary actions are tinted.
- Corners are fixed, capsule (half the height) or concentric (the parent's radius minus the
  padding).
- macOS small and medium controls stay rounded rectangles; large ones are capsules.
- A pinned header gets a "hard" scroll edge; a scroll view gets a soft one.
- Reduce Transparency frosts glass, Increase Contrast turns surfaces plain black or white with
  a border, and Reduce Motion removes elasticity.

### 7. Designs are live

`design:` may be a function of the model. A frame whose design changed carries the new
stylesheet, so an app can offer a theme picker. `ui.scoped(design, node)` gives a subtree its
own design in both modes. The design lab uses it to put three looks on one page.

## Consequences

- **Every component is restyled.** The new components are icons, segmented control, chips,
  radio group, stepper, ring, skeleton, sheet, toasts with an action, list and list item,
  avatars, stat tile and sparkline, breadcrumbs, search field, kbd, FAB, and the app frame
  (`shell`, `rail`, `toolbar`). Existing component APIs are unchanged; `menu` gained an icon
  trigger, item icons and a danger item.
- **The cost is small.** Building a design is about 7 ms, about 1 ms of which is the palettes,
  and it is identical on all three engines. A live design rebuilds its stylesheet only when the
  design value changes.
- **Not done here:**
  - continuous ("squircle") corners, which wait on CSS `corner-shape` support in WebKit;
  - drag gestures with velocity hand-off;
  - shape morphing beyond the press state.
