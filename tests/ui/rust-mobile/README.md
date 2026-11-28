# rust-mobile's replay corpus

Sessions recorded on Android by [areeb-h/rust-mobile](https://github.com/areeb-h/rust-mobile),
with the apps they were recorded from, copied here unchanged (ADR 0069 D6): rust-mobile's CI
replays them as a gate, so a Helix change that would fail them must fail Helix's own CI first.

| | |
|---|---|
| Copied from | `spike/replay-demo/` at rust-mobile `851e51453550c07067dc40a092e8f06bb7fbbdfc` (2026-11-27) |
| Recorded with | Helix `b3f898d` (the sessions' calls carry its text fingerprints, `args`) |
| Apps | `stopwatch.helix`, `inputs.helix` — rust-mobile's copies, not Helix's examples, so this tests what rust-mobile replays |

`a_mobile_shells_replay_corpus_still_replays` in `tests/cli.rs` replays each folder on all three
engines. **Do not re-record or `--accept` these here**: they are the other repository's
evidence. A Helix change that fails them is either a bug, or a deliberate change to the session
format that needs a compatibility path (as `args` → `argsum` has) and a note in rust-mobile.
When rust-mobile's corpus changes, copy it again and update the commit above.
