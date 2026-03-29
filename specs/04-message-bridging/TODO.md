# TODO — spec/04-message-bridging

Status: **Complete** — all tasks done; mutation testing done (11 equivalent/shim survivors documented)

- [x] Task 1 — `BridgeMap`: channel mapping table (discord↔IRC, webhook URL lookup, update from config)
- [x] Task 2 — Discord→IRC message relay: `DiscordEvent::MessageReceived` → `S2SCommand::SendMessage`; content edge cases (whitespace, attachments, sticker, multi-line); ACTION detection
- [x] Task 3 — IRC→Discord message relay: `S2SEvent::MessageReceived` / `NoticeReceived` → `DiscordCommand::SendMessage`; NOTICE and ACTION formatting; ping-fix
- [x] Task 4 — IRC lifecycle events: `LinkUp` (burst), `LinkDown` (reset), `BurstComplete`, `UserIntroduced`/`UserNickChanged`/`UserQuit` (external nick tracking), `NickForced` (SVSNICK), `ChannelBurst` (timestamp tracking), `UserParted`/`UserKicked`
- [x] Task 5 — Discord lifecycle events: `MemberSnapshot` (burst introduce), `MemberAdded` (introduce), `MemberRemoved` (quit), `PresenceUpdated` (away/introduce per spec-06 Option B)
- [x] Task 6 — `run_bridge` loop: `tokio::select!` on both channels; owns `PseudoclientManager`; wires into `main.rs`

## Mutation testing survivors (11 — all acceptable)

`cargo mutants --file src/bridge.rs` found 11 survivors in three categories:

### Shim resolvers — 8 survivors (lines 519–534)

`NoopIrcResolver::resolve_nick` and `NoopDiscordResolver::{resolve_user,resolve_channel,resolve_role}` always return `None`.
Mutations to `Some(String::new())` / `Some("xyzzy".into())` survive because these shims are wired into `run_bridge`
(integration-only path). Mention-conversion correctness is tested exhaustively in `specs/05-formatting`; the bridge
routing tests use `NullResolver`/`NullIrcResolver` defined in `#[cfg(test)]` and do not go through these shims.

### `unix_now()` — 2 survivors (line 687)

Mutations to return `0` or `1` survive because `unix_now()` is called only inside `run_bridge` which has no unit
tests. The helper is a non-deterministic clock shim; testing the exact value it returns would be brittle. Its
integration is covered indirectly: timestamp seeding propagates through `ts_for_channel` fallback logic, which is
tested via `burst_falls_back_to_now_ts_when_channel_unknown`.

### `run_bridge` — 1 survivor (line 710)

The entire async event loop is integration-only (requires live `tokio::spawn`-ed IRC/Discord tasks and live
channels). Replacing the body with `()` survives by definition; no unit-level test harness for it exists.
