# State Persistence TODO

## Tasks

- [x] Task 1: Add `serde_json` dependency and `state_file` config field
- [x] Task 2: Create `src/persist.rs` with types and load/save functions
- [x] Task 3: Integrate seed map into `BridgeState`
- [x] Task 4: Wire save into `run_bridge` idle tick and shutdown path
- [x] Task 5: Add dirty flag to avoid unnecessary writes
- [x] Task 6: Unit tests for persist module
- [x] Task 7: Integration tests for seed restore (MemberSnapshot, PresenceUpdated, on-demand, offline, bot exclusion, channel filtering)
- [x] Task 8: Mutation testing — 0 missed (105 tested: 88 caught, 17 unviable)
- [x] Task 9: Graceful shutdown (SIGTERM/SIGINT handling, non-Unix Ctrl-C, task abort)
- [x] Task 10: Refactor — centralize seed logic in orchestrator, remove seed parameter from apply_discord_event

## Equivalent/excluded mutants

- `non_unix_signal_loop` in `signal.rs`: `#[mutants::skip]` — `#[cfg(not(unix))]`, so on
  the Linux CI target the mutated body is never compiled in, tests pass, and the mutant
  would be reported as a false MISSED. This skip is load-bearing, not merely defensive.

Previously skipped, now covered — no longer excluded:

- `unix_now` in `bridge/mod.rs` — the "cannot be tested deterministically" justification
  was wrong. Mutation testing needs a *distinguishing* assertion, not a deterministic
  value: the function is non-deterministic in its exact result but tightly bounded, so a
  bounds check on the epoch seconds kills both mutants (`-> 0`, `-> 1`) and is stable
  forever. Mutation run: 2 tested, 2 caught, 0 missed.

- `load_seed_state` and `maybe_save_state` in `bridge/mod.rs` — 8 tempdir-based tests.
  The `NotFound`-vs-other-error branch is pinned by `tracing-test` log-level assertions
  (INFO "No persisted state file" vs WARN "Failed to load persisted state"), since both
  branches return the same empty map and differ only in logging.
- `unix_signal_loop` in `signal.rs` — already covered by the existing
  `unix_signals_map_to_control_events` test; the skip was masking real coverage.
  Mutation run: 6 mutants, 2 caught, 4 unviable, 0 missed.
- `run_bridge` in `bridge/mod.rs` — the skip claimed live IRC and Discord
  connections were needed, but the loop's whole interface is mpsc channels plus
  `&Config` / `&Path`. 6 tests cover shutdown-exits-and-saves, each event channel
  closing, a closed control channel disabling its branch without exiting early,
  event dispatch through to the command channels, and a failing reload being
  logged without aborting the loop.
  Mutation run for `bridge/mod.rs`: 9 tested, 6 caught, 3 unviable, 0 missed.

## Notes

- 10 unit tests in `persist.rs`, 12 integration tests in `orchestrator.rs` (seed + dirty flag + cooldown boundary)
- 686 total lib tests pass
