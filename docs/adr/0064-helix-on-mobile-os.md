# ADR 0064 — Helix on mobile-os: apps that bring their sandbox, and a shared design

- **Status:** Accepted (first part done)
- **Date:** 2026-11-11
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0055](0055-apps.md) ("A target: mobile-os"), [ADR 0057](0057-gestures.md),
  [ADR 0063](0063-the-app-platform.md), mobile-os ADR 0005 (Flatpak and portals) and ADR 0008
  (any device).

## Context

[mobile-os](https://github.com/areeb-h/mobile-os) is a privacy-first Linux phone OS:
- its shell is Rust with GTK4 and libadwaita, on the phoc compositor;
- third-party apps are Flatpaks;
- a planned service, `privacyd`, adds what Flatpak lacks, such as per-app network on/off.

Its own review of Helix reached the conclusion ADR 0055 did. Helix does not belong inside the
shell, the compositor or the system services: those need native code every frame and no
interpreter. Where Helix helps mobile-os is in three places:

1. **The apps people install.** Flatpak's permissions are written by hand, they are coarse,
   and they drift from what the app does. A Helix app can bring a sandbox computed from its
   code.
2. **The shell's design.** The shell picks its colours by hand. Helix computes colours from
   one seed and proves every text/background pair meets WCAG contrast. Its springs and gesture
   rules are already settled (ADR 0057).
3. **Apps written in Helix**, which get replay tests, the black box (ADR 0063 D2) and hot
   reload, once they look native on the phone.

## Decisions

### D1 — A built app carries a Flatpak manifest generated from its code *(done)*

`helix build app.helix --app` (ADR 0063 D4) now also writes `app.helix.<id>.flatpak.json` into
the Linux AppDir.
- **Sandbox:** its `finish-args` start from what any windowed app needs to draw (Wayland, an X
  fallback, shared memory, the GPU). They add exactly what the inferred ceiling grants:
  - `--share=network` only if the code reaches the network;
  - `--filesystem=home:ro` for a file reader, `--filesystem=home` for a writer.
- **Packaging:** it runs on the GNOME runtime, which carries the WebKitGTK the window uses, and
  installs the app, its `.desktop` entry, its icon and its `permissions.json`.
- **Results on the examples:** Orbit and the gallery get a sandbox with no network and no
  files, because their code reaches neither. Files gets the home folder read-write.
- **The `.desktop` entry** marks the app `X-Purism-FormFactor=Workstation;Mobile;`, the
  convention phoc-based shells use to list an app in phone mode. mobile-os's shell lists apps
  through GIO, so an installed Helix app appears with no shell change.

Limits, stated:
- **Files by place, not kind.** Flatpak grants files by location, so "reads files" becomes the
  home folder. Narrower locations wait on ADR 0021's per-path grants.
- **Programs.** Running programs needs no Flatpak grant, because programs inside the sandbox
  run. The host is never opened (`flatpak-spawn --host` is an escape, not a permission).
- **Untested build.** `flatpak-builder` was not available where this was built. The manifest
  is checked by unit tests, not yet by building it.
- **The runtime's libraries.** The executable inside is the one `helix build` copied. A
  distributed Flatpak should be built inside the GNOME SDK, so its libraries match the
  runtime's.

### D2 — The design is exported, with its proof *(done)*

`ui.tokens(design, format, mode)` writes a design's tokens for another program:
- `"gtk"`: GTK CSS `@define-color hx_accent …;` lines, ready to `@import` into a shell's
  stylesheet;
- `"css"`: custom properties for every token and spring;
- `"json"`: colours, sizes, springs as stiffness and damping ratio, and the contrast report.

Each export says how many text/background pairs were checked and how many are below WCAG.
`examples/tools/design_tokens.helix --seed … --format gtk` is the same as a command, and it
fails when any pair is below WCAG. So mobile-os can generate its palette in CI and fail the
build if it regresses.
- **Checked by GTK.** The `gtk` output was loaded into GTK's own CSS parser, which accepted it;
  the same check refuses a malformed colour.
- **Same on every engine.** `tests/corpus/ui_tokens.helix` holds all three formats on all
  three engines.

### D3 — What mobile-os can take now, without depending on Helix

These are rules, not code, and the shell can adopt them in Rust:
- **The throw (ADR 0057):** momentum is the finger's speed over the last 100 ms, projected
  0.2 s ahead.
- **The rubber band:** a gesture resists the wrong way on UIKit's curve.
- **A keyboard path:** every gesture has a keyboard equivalent.
- **Springs:** the spatial and effects springs, as numbers, from `ui.tokens(d, "json")`.
- **The contrast check:** pairs and thresholds as in `ui.contrast_report`.

### D4 — Next: Helix apps that look native on the phone

ADR 0055 planned a GTK4 and libadwaita renderer of the view tree. It would live in a separate
binary, so the core stays free of GTK. A Helix app would then use the phone's own widgets,
accessibility and theme instead of a web view. The view tree already has roles, accessible
names and messages (ADR 0063 D5), which map onto GTK widgets and their accessible roles. That
renderer is the next large step for Helix on mobile-os.

### D5 — Next: privacyd reads the manifest

Every built app carries `permissions.json`: what it may do, why (the call path), and that it
was inferred and is enforced. `privacyd` can read it at install time:
- **Network off by default.** An app whose code never touches the network starts with its
  network switched off at the cgroup level. That is a second wall behind Flatpak's, and it
  doesn't depend on the app's own claims.
- **A prompt worth reading.** The permission prompt can show the reason ("reads files:
  `update -> open_folder -> read_dir`") instead of a bare permission name.

## Consequences

- **Helix stays out of the OS core.** The OS core stays Rust, as mobile-os's rules require.
  Helix contributes data (tokens, manifests) and apps.
- **Generated, then checked.** The shell's colours become a generated file with a CI check,
  not hand-picked values.
- **Two places now depend on the inference.** A built app's Flatpak sandbox and its in-process
  ceiling come from one analysis (ADR 0063 D3), so they agree by construction.
