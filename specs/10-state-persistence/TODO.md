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

- `load_seed_state` and `maybe_save_state` in `bridge/mod.rs`: `#[mutants::skip]` — I/O + config plumbing wrappers
- `non_unix_signal_loop` and `unix_signal_loop` in `signal.rs`: `#[mutants::skip]` — platform-specific signal handling

## Notes

- 10 unit tests in `persist.rs`, 12 integration tests in `orchestrator.rs` (seed + dirty flag + cooldown boundary)
- 686 total lib tests pass
