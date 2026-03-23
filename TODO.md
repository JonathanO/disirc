# TODO

Tracks current and upcoming work. Updated by Claude at the start and end of each session, and whenever task status changes.

## In progress

- Spec review with user (all specs under review — no implementation begun)

## Future specs (deferred from v1)

- **DM bridging** — IRC `PRIVMSG` to a pseudoclient UID forwarded as a Discord DM and vice versa. Architecture must not preclude this: route `PRIVMSG` to non-channel targets; do not discard Discord DM `MESSAGE_CREATE` events at the framework level.

## Pending — implementation

Specs are approved. Implement in order (each builds on the previous).

### `specs/01-configuration.md`
- [x] Define config structs with serde (`DiscordConfig`, `IrcConfig`, `PseudoclientConfig`, `BridgeEntry`, root `Config`)
- [x] Config loading from file (read TOML, deserialize, CLI `--config` flag)
- [x] Validation logic (SID regex, channel names, webhook URL, duplicate detection, at-least-one-bridge)
- [x] Tests: unit tests + proptest for all validation rules
- [x] SIGHUP handler (tokio signal, send reload event into mpsc channel)
- [ ] Reload diff logic (compute added/removed entries, apply, log summary; validate before applying)

### `specs/05-formatting.md`
- [ ] Discord→IRC: mention and emoji resolution (`<@id>`, `<#id>`, `<@&id>`, `<:name:id>`, `<a:name:id>`)
- [ ] Discord→IRC: markdown to IRC control codes (bold, italic, underline, strikethrough)
- [ ] Discord→IRC: newline splitting, code block handling, length splitting at 400 bytes
- [ ] IRC→Discord: control character stripping/conversion (all `\x01`–`\x1f`)
- [ ] IRC→Discord: `@nick` mention conversion, ping-fix zero-width space, length truncation at 2000 chars
- [ ] `server-time` ISO 8601 timestamp formatting + proptest suite across all transforms

### `specs/06-pseudoclients.md`
- [ ] Nick sanitization (character replacement, digit prefix, truncation to 30 chars)
- [ ] Nick collision fallback chain (`_` ×3, 8 hex digits of Discord ID, UID-derived guaranteed fallback)
- [ ] UID generation (SID + 6 unique alphanumeric chars, stable per Discord user ID for session)
- [ ] `PseudoclientState` struct and in-memory state maps (`discord_id → state`, `nick → id`, `uid → id`)
- [ ] Introduction message generation (UID line + SJOIN line)
- [ ] Quit/Part message generation
- [ ] SVSNICK handling (apply forced nick change, update state)
- [ ] Runtime channel add/remove (SJOIN existing pseudoclients to new channel; PART/QUIT on removal)

### `specs/02-irc-connection.md`
- [ ] TCP/TLS connection with `tokio-rustls` + line-oriented framing (`\r\n`, max 4096 bytes with MTAGS)
- [ ] Handshake sequence (PASS, PROTOCTL EAUTH, PROTOCTL caps, SID, SERVER; verify uplink credentials)
- [ ] Burst: send UID + SJOIN + EOS for all active pseudoclients
- [ ] Burst: receive and process uplink UID/SJOIN/SID/EOS (build local state)
- [ ] Ongoing message handling (PING/PONG, PRIVMSG relay, NICK/QUIT/PART/KICK/SQUIT state updates)
- [ ] Message tag parsing (strip `s2s-md/*` and `@unrealircd.org/userhost`; pass `@time=` through)
- [ ] Token-bucket rate limiter (capacity 10, refill 1/500 ms; PING/PONG bypass)
- [ ] Ping keepalive (send every 90 s; timeout after 60 s with no PONG)
- [ ] Reconnection with exponential backoff (5 s → 5 min cap)

### `specs/03-discord-connection.md`
- [ ] Gateway connection and READY event (record bot user ID, fetch webhook user IDs)
- [ ] Member list and presence fetch on startup for all bridged channels
- [ ] `MESSAGE_CREATE` routing and self-message filtering (bot ID + webhook user IDs)
- [ ] `PRESENCE_UPDATE` → pseudoclient `AWAY` status
- [ ] `GUILD_MEMBER_ADD` / `GUILD_MEMBER_REMOVE` → pseudoclient introduce/quit
- [ ] Webhook management (create per channel if missing, cache and reuse, fallback to plain send)
- [ ] Config reload: fetch members + presence for new channel; register new webhook user ID

### `specs/04-message-bridging.md`
- [ ] Mutable channel map (consulted on every message, updated atomically on reload)
- [ ] Discord→IRC relay pipeline (filter → format → send PRIVMSG; attachment URLs; sticker handling)
- [ ] IRC→Discord relay pipeline (filter → format → webhook preferred → plain fallback)
- [ ] Loop prevention (bot/webhook ID filter on Discord side; SID prefix filter on IRC side)
- [ ] NOTICE and ACTION (`/me`) handling
- [ ] Error handling (inaccessible channels log at ERROR; failed sends log at WARN; link-down drops)

## Completed

- Rewrote all specs for UnrealIRCd S2S architecture (pseudoclient model, S2S handshake, webhooks)
- Added `specs/06-pseudoclients.md`
- Removed `irc` client crate; added `tokio-rustls`
- Wrote `research/unreal-ircd-s2s-protocol.md` from RFC 2813 vs UnrealIRCd research
- Wrote `research/discord-irc-prior-art.md` from FauxFaux/discord-irc analysis
- Created `research/INDEX.md`
- Updated `specs/02-irc-connection.md` with precise UID/SJOIN/PROTOCTL/PING syntax
- Updated `CLAUDE.md`: research workflow, cargo-deny, clippy pedantic, unsafe_code, research index rule, session continuity rule, TODO.md update rule
- Updated `specs/00-overview.md`: DM bridging deferred to future, not hard no
- Updated `specs/05-formatting.md`: clarified IRC→Discord sender attribution asymmetry
- Updated `specs/06-pseudoclients.md`: nick collision fallback chain (3×_, 8 hex digits, UID-derived); runtime channel add/remove
- Updated `specs/01-configuration.md`: runtime config reload via SIGHUP, reloadable vs non-reloadable fields
- Updated `specs/03-discord-connection.md`: webhook self-message filtering, reload procedure
- Updated `specs/04-message-bridging.md`: channel map mutable at runtime, consult current map on every message
