# State Persistence TODO

## Tasks

- [x] Task 1: Add `serde_json` dependency and `state_file` config field
- [x] Task 2: Create `src/persist.rs` with types and load/save functions
- [x] Task 3: Integrate seed map into `BridgeState` and `introduce_pseudoclient`
- [x] Task 4: Wire save into `run_bridge` idle tick and shutdown path
- [x] Task 5: Add dirty flag to avoid unnecessary writes
- [x] Task 6: Unit tests for persist module (roundtrip, missing file, corrupt file, version mismatch, channel validation)
- [x] Task 7: Integration test for startup merge (seed + MemberSnapshot)
- [ ] Task 8: Mutation testing — deferred to CI (WSL character device prevents local copy-mode)

## Equivalent/excluded mutants

- `load_seed_state` and `maybe_save_state` in `bridge/mod.rs`: `#[mutants::skip]` — I/O + config plumbing wrappers
- `apply_discord_event` `#[allow(clippy::implicit_hasher)]`: pedantic lint suppression only

## Notes

- 10 unit tests in `persist.rs`, 4 integration tests in `orchestrator.rs`
- 674 total lib tests pass
