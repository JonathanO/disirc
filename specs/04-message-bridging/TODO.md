# TODO — spec/04-message-bridging

Status: **In Progress**

- [ ] Task 1 — `BridgeMap`: channel mapping table (discord↔IRC, webhook URL lookup, update from config)
- [ ] Task 2 — Discord→IRC message relay: `DiscordEvent::MessageReceived` → `S2SCommand::SendMessage`; content edge cases (whitespace, attachments, sticker, multi-line); ACTION detection
- [ ] Task 3 — IRC→Discord message relay: `S2SEvent::MessageReceived` / `NoticeReceived` → `DiscordCommand::SendMessage`; NOTICE and ACTION formatting; ping-fix
- [ ] Task 4 — IRC lifecycle events: `LinkUp` (burst), `LinkDown` (reset), `BurstComplete`, `UserIntroduced`/`UserNickChanged`/`UserQuit` (external nick tracking), `NickForced` (SVSNICK), `ChannelBurst` (timestamp tracking), `UserParted`/`UserKicked`
- [ ] Task 5 — Discord lifecycle events: `MemberSnapshot` (burst introduce), `MemberAdded` (introduce), `MemberRemoved` (quit), `PresenceUpdated` (away/introduce per spec-06 Option B)
- [ ] Task 6 — `run_bridge` loop: `tokio::select!` on both channels; owns `PseudoclientManager`; wires into `main.rs`
