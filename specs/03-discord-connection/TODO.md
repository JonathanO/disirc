# TODO — spec/03-discord-connection

Status: **Implemented**

- [x] Define `DiscordEvent` / `DiscordCommand` boundary types (protocol-agnostic channel boundary, analogous to `S2SEvent`/`S2SCommand`)
- [x] Startup: parse webhook IDs from configured URLs; establish Gateway connection; record bot user ID from `READY` event
- [x] Handle `guild_create()` — extract initial member and presence snapshot from delivered data
- [x] `MESSAGE_CREATE` routing and self-message filtering (bot ID + webhook user IDs)
- [x] `PRESENCE_UPDATE` → `DiscordEvent::PresenceUpdated` (map `OnlineStatus` including `_` catch-all)
- [x] `GUILD_MEMBER_ADD` / `GUILD_MEMBER_REMOVE` → `DiscordEvent::MemberAdded` / `MemberRemoved`
- [x] Webhook sending — enforce 2–32 char username constraints, suppress `@everyone`/`@here` via `allowed_mentions`, fallback to plain `channel.send()` with zero-width space suppression
- [x] Config reload: REST-fetch members + presence for newly added channel; parse webhook ID from new URL
- [x] Mutation testing: zero surviving mutants required before marking Implemented

## Mutation testing — accepted exceptions

All surviving mutants after the final run (`cargo mutants --timeout 60`) are either
equivalent mutants or integration-only. None represent real test gaps.

### Equivalent mutant

| Location | Mutation | Reason |
|---|---|---|
| `handler.rs:58` | Delete `OnlineStatus::Offline \| OnlineStatus::Invisible` match arm | The `_` catch-all arm produces identical output; no test can distinguish the two. |

### Integration-only — EventHandler shims (`handler.rs`)

The six serenity `EventHandler` trait methods (`ready`, `guild_create`, `message`,
`presence_update`, `guild_member_addition`, `guild_member_removal`) are thin shims that
delegate immediately to the inner functions (`handle_ready`, `handle_message_event`,
`build_member_snapshot_event`, `presence_event`, `member_addition_event`,
`member_removal_event`). The inner functions are fully unit-tested. The shims themselves
require a live Discord gateway `Context` that cannot be constructed in unit tests.

| Location | Mutation |
|---|---|
| `handler.rs:198` | `ready` → `()` |
| `handler.rs:203` | `guild_create` → `()` |
| `handler.rs:225` | `message` → `()` |
| `handler.rs:236` | `presence_update` → `()` |
| `handler.rs:246` | `guild_member_addition` → `()` |
| `handler.rs:263` | `guild_member_removal` → `()` |

### Integration-only — network-dependent functions (`send.rs`)

These functions perform live Discord HTTP calls (`Webhook::from_url`,
`ChannelId::to_channel`, `GuildId::members`, `webhook.execute`). They cannot be
unit-tested without a mock HTTP layer. All pure logic inside them (username
sanitisation, mention suppression, routing table mutation) is unit-tested in separate
functions with full coverage.

| Location | Mutation |
|---|---|
| `send.rs:61` | `send_discord_message` → `()` |
| `send.rs:119` | `guild_id_for_channel` → `None` / `Some(Default::default())` |
| `send.rs:146` | `fetch_member_snapshot` → `()` |
| `send.rs:194` | `process_discord_commands` → `()` |

### TIMEOUT = caught

Two mutations in `sanitize_webhook_username` and `suppress_mentions` caused infinite
loops (e.g. `<` → `>` in the length check, `+` → arithmetic in the search loop). The
mutation framework's timeout mechanism correctly detected these as failures — they are
not surviving mutants.
