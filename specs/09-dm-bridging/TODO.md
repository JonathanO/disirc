# DM Bridging TODO

## Tasks

- [ ] Add `dm_bridging` config option to `FormattingConfig` (default: false)
- [ ] Add `DIRECT_MESSAGES` intent when `dm_bridging` is enabled
- [ ] IRC→Discord: detect PRIVMSG to pseudoclient UID, relay as DM
- [ ] IRC→Discord: DM channel creation/caching
- [ ] IRC→Discord: formatting (same as plain channel path)
- [ ] Discord→IRC: detect DM MESSAGE_CREATE (no guild_id), relay as PRIVMSG
- [ ] Discord→IRC: reply-based target resolution (parse message_reference)
- [ ] Discord→IRC: nick-colon target resolution (fallback)
- [ ] Discord→IRC: help message for unresolvable DMs
- [ ] Discord→IRC: handle missing pseudoclient (user offline)
- [ ] Error handling: 403 Forbidden (blocked), rate limits
- [ ] Unit tests for DM routing logic
- [ ] L3 e2e tests (mock Discord, real IRC)
- [ ] L4 e2e tests (real Discord + real IRC)
- [ ] Update SPECS.md status
