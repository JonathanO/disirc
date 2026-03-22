# TODO

Tracks current and upcoming work. Updated by Claude at the start and end of each session, and whenever task status changes.

## In progress

- Spec review with user (all specs under review — no implementation begun)

## Future specs (deferred from v1)

- **DM bridging** — IRC `PRIVMSG` to a pseudoclient UID forwarded as a Discord DM and vice versa. Architecture must not preclude this: route `PRIVMSG` to non-channel targets; do not discard Discord DM `MESSAGE_CREATE` events at the framework level.

## Pending — implementation (blocked on spec review completion)

- Implement `specs/01-configuration.md` (config parsing and validation)
- Implement `specs/05-formatting.md` (message formatting — pure logic, no deps, good first impl)
- Implement `specs/06-pseudoclients.md` (nick sanitization, UID assignment, state tracking)
- Implement `specs/02-irc-connection.md` (UnrealIRCd S2S link)
- Implement `specs/03-discord-connection.md` (Discord Gateway + webhook delivery)
- Implement `specs/04-message-bridging.md` (bidirectional relay, loop prevention)

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
