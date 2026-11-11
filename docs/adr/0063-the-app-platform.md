# ADR 0063 — The app platform: compete by being different, not by catching up

- **Status:** Accepted (in progress)
- **Date:** 2026-11-11
- **Deciders:** Areeb + Claude
- **Related:** [ADR 0021](0021-capability-sandbox.md) (capabilities),
  [ADR 0055](0055-apps.md) (apps, the platform stages), [ADR 0060](0060-the-ui-platform.md),
  [ADR 0062](0062-made-for-agents.md) (made for agents).

## Context

People build apps with React Native, Swift and SwiftUI, Kotlin and Compose, and .NET (MAUI,
WinUI, Blazor). Each has a decade of platform APIs, libraries and editor tooling. Helix will
not out-feature them by adding features one at a time in the same shape. It can be the better
choice for some apps by doing things none of them can:

- **Every Helix program gives one answer on three engines.** No other platform has that
  check, so none can promise that a bug seen once reproduces exactly, everywhere.
- **Every Helix program says what it reaches.** `helix effects` already computes, from the
  code, which files, hosts and processes each function can touch. No other platform derives an
  app's permissions from its code; they are written by hand and drift from what the code does.
- **An app is a fold over events.** `update(model, event)` and `view(model)` are ordinary
  functions. The runtime owns every input, so it can record, replay, inspect and test all of
  them.
- **Agents write it correctly** (ADR 0062), and the fixes are measured.

This ADR picks the ideas that follow from those facts and the order to build them.

## Decisions

### D1 — An app in its own window opens no port *(done)*

Before this, a windowed app served HTTP on `127.0.0.1` and the window loaded it. That required
a session token, a cookie, and Host and Origin checks, because any process on the machine could
connect to the port. Electron apps that serve locally have the same problem.

Now `ui.app({window: true})` calls `app_window`, and nothing listens:

- **The page** loads through the private `helix:` URL scheme (`http://helix.app/` on WebView2).
- **Events and the subscription** go through the web view's own message channel
  (`window.ipc.postMessage`). Only the runtime's two message shapes are accepted, and only
  from the app's own origin.
- **Frames** reach the page as `evaluate_script` calls.
- **One loop, two listeners.** Each message becomes the same request record a socket would
  have produced, plus `transport: "window"`, a field only the window's link sets. So the loop
  in `std/ui.helix` is unchanged (`src/localnet.rs`).

Consequences:

- **Nothing to attack.** There is no port, so there is nothing for another process, a web page
  rebinding DNS, or a port scanner to reach.
- **No network permission.** The window's handles skip the `net` capability gate, so an app
  run with no network authority can still show its own window.
- **Tested end to end.** `tests/ui/window.sh` checks on a real WebKitGTK window that the
  process has no listening socket. It also checks that a click reaches `update` and that the
  frame reaches the page: the button moves after the first click, and a second click lands
  only where the new frame put it.

### D2 — Every session is a black box that replays exactly *(done)*

A bug report today is a description and maybe a stack trace. Helix can do better: the app
records its session, and the recording reproduces the bug on any machine and any engine.

- **The tape.** With recording on, the runtime writes each event that reached `update`, and
  the results of every impure builtin `update` and `view` called while handling it (`now()`,
  `random()`, a file read, an HTTP response). That second part is the difference from Redux
  DevTools or Elm's debugger: an app that reads the clock or a file in `update` still replays
  exactly. The recording replays those answers instead of asking the world again.
- **Proved, not hoped.** Each step also records a hash of the screen it produced. Replay
  compares them and either says "12 of 12 screens identical" or names the first event whose
  screen differs. It runs on any of the three engines.
- **One command.** `helix replay app.helix session.jsonl` reproduces the session, including
  the error `update` raised and the event that caused it.
- **Limits, stated.** A value that cannot be written down (a database connection, a
  DataFrame) is recorded as opaque, and a session that needs one says it cannot replay that
  step. Methods on handles (`db.query`) are not on the tape yet.

### D3 — Permissions are inferred from the code and enforced *(next)*

`helix build --app` will run the same effects analysis as `helix effects`. It writes the app's
manifest from it: the files, directories, hosts and programs the code can reach. The built
binary enforces exactly that set through the ADR 0021 capability gate.

- **The manifest can't be wrong.** It is computed from the code, so it can't drift from what
  the app does, and a reviewer reads facts, not promises.
- **Platform formats come from it:** macOS entitlements, a Flatpak `finish-args`, later an
  Android manifest. A permission the code doesn't need is never requested.
- **A widening is visible.** An app that starts reaching a new host fails its build check
  until the manifest diff is accepted.

### D4 — One command builds a native app *(next)*

`helix build --app app.helix` produces, per OS, the thing a user double-clicks:
- a `.app` bundle on macOS;
- an AppDir with a `.desktop` file on Linux;
- an `.exe` on Windows.

It needs no Xcode project, Gradle file or `.csproj`. Signing uses the platform's tools when an
identity is configured. Native menus, file dialogs, clipboard and notifications arrive as
builtins behind the capability gate.

### D5 — The UI is an API

Every screen is data: a view tree with roles, accessible names and messages. The same tree
serves three readers:
- the replay tests;
- the accessibility tree;
- an agent that drives the app ("press the button named Save").

Competitors build test IDs, accessibility and automation separately, and they drift apart.
Here they are one thing, so an app that an agent can drive is also accessible. Planned as
`helix drive`.

### D6 — The editor knows what the binary knows

`helix lsp` will serve errors as you type, hover documentation, completion and go-to-definition.
It answers from the sources `helix check --json`, `helix describe` and `helix search` already
use. No separate model of the language is written for the editor, so it can't disagree with
the compiler.

### D7 — Mobile and the browser, after the core is right

This is ADR 0055's order, kept:
1. A `wasm32` build whose corpus output must match the other three engines byte for byte (a
   fourth engine).
2. The runtime in a Web Worker, so an app runs offline in a browser tab.
3. iOS and Android shells around the same web view.
   - The VM runs on iOS, where a JIT is not allowed.
   - D1's no-port window is the same shape a phone needs, so it carries over unchanged.
4. On mobile-os, a GTK renderer of the same view tree, for native widgets and the OS's own
   accessibility.

## What we will not do

- **Bundle a browser.** Electron's size and memory cost is what a native web view avoids.
- **Draw every pixel ourselves** (Flutter's way). Text shaping, input methods, accessibility
  and platform feel would all have to be rebuilt, and that is years of work (ADR 0055).
- **Copy a competitor's API shape.** Helix wins where its model is different, so it keeps that
  model and builds on it.

## Consequences

- **More checks per change.** The window transport is checked by `tests/ui/window.sh` on a real
  WebKitGTK window in CI, plus unit tests for the message shapes and the private origin.
- **The black box adds work per event, only when recording.** The cost is one hash per screen
  and the tape entries, each written as it happens, so a crash loses nothing.
- **Permission inference makes the effects analysis load-bearing.** A gap in it becomes a
  permission the app is missing, not a silent over-grant. The failure is loud: the gate refuses
  the call, and its message names the permission.
