# DM Bridging TODO

## Tasks

- [x] Add `dm_bridging` config option to `FormattingConfig` (default: false)
- [x] Add `DIRECT_MESSAGES` intent (always included, harmless when unused)
- [x] IRC→Discord: detect PRIVMSG to pseudoclient UID, relay as DM
- [x] IRC→Discord: DM channel creation/caching (via serenity `create_dm_channel`)
- [x] IRC→Discord: formatting (same as plain channel path)
- [x] Discord→IRC: detect DM MESSAGE_CREATE (no guild_id), relay as PRIVMSG
- [x] Discord→IRC: reply-based target resolution (parse message_reference)
- [x] Discord→IRC: nick-colon target resolution (fallback)
- [x] Discord→IRC: help message for unresolvable DMs
- [x] Discord→IRC: handle missing pseudoclient (user offline)
- [x] Error handling: 403 Forbidden (blocked), rate limits
- [x] Unit tests for DM routing logic (13 tests)
- [x] Handler unit tests (3 tests)
- [x] L3 e2e tests (3 tests, mock Discord, real IRC)
- [x] Mutation testing: 0 surviving on routing.rs, handler.rs, state.rs, mod.rs
- [ ] L4 e2e tests (real Discord + real IRC) — deferred, requires manual setup
- [ ] Update SPECS.md status
