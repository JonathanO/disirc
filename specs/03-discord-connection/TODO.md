# TODO — spec/03-discord-connection

Status: **Implementation complete — mutation testing pending**

- [x] Define `DiscordEvent` / `DiscordCommand` boundary types (protocol-agnostic channel boundary, analogous to `S2SEvent`/`S2SCommand`)
- [x] Startup: parse webhook IDs from configured URLs; establish Gateway connection; record bot user ID from `READY` event
- [x] Handle `guild_create()` — extract initial member and presence snapshot from delivered data
- [x] `MESSAGE_CREATE` routing and self-message filtering (bot ID + webhook user IDs)
- [x] `PRESENCE_UPDATE` → `DiscordEvent::PresenceUpdated` (map `OnlineStatus` including `_` catch-all)
- [x] `GUILD_MEMBER_ADD` / `GUILD_MEMBER_REMOVE` → `DiscordEvent::MemberAdded` / `MemberRemoved`
- [x] Webhook sending — enforce 2–32 char username constraints, suppress `@everyone`/`@here` via `allowed_mentions`, fallback to plain `channel.send()` with zero-width space suppression
- [x] Config reload: REST-fetch members + presence for newly added channel; parse webhook ID from new URL
- [ ] Mutation testing: zero surviving mutants required before marking Implemented
