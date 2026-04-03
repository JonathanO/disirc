# TODO

High-level status tracker. Each spec has its own `TODO.md` in its directory.

Updated by Claude at the start and end of each session, and whenever task status changes.

## In progress

None.

## Spec status

| Spec | Status | Detail |
|------|--------|--------|
| [specs/00-overview](specs/00-overview/TODO.md) | n/a — architecture doc | — |
| [specs/01-configuration](specs/01-configuration/TODO.md) | ✅ Implemented | — |
| [specs/02-irc-connection](specs/02-irc-connection/TODO.md) | ✅ Implemented | — |
| [specs/03-discord-connection](specs/03-discord-connection/TODO.md) | ✅ Implemented | — |
| [specs/04-message-bridging](specs/04-message-bridging/TODO.md) | ✅ Implemented | 16 Task-6 tests; 11 equivalent/shim mutants documented |
| [specs/05-formatting](specs/05-formatting/TODO.md) | ✅ Implemented | — |
| [specs/06-pseudoclients](specs/06-pseudoclients/TODO.md) | ✅ Implemented | — |
| [specs/07-irc-message-types](specs/07-irc-message-types/TODO.md) | ✅ Implemented | — |
| [specs/08-e2e-testing](specs/08-e2e-testing/TODO.md) | ✅ Implemented | L3 + L4 tests, CI workflows, DEVELOPING.md docs |
| [specs/09-dm-bridging](specs/09-dm-bridging/TODO.md) | ✅ Implemented | PR #19 merged |

## Pending

None.

## Future features

- **Topic bridging** — Sync channel topics bidirectionally between Discord and IRC. Discord channel topic changes should update the IRC channel topic and vice versa.
- **Metrics / observability** — Structured metrics for operators: message counts (per direction), pseudoclient count, link uptime, reconnect count. Could expose via a simple HTTP endpoint or structured log events.
- **Dynamic configuration** — Allow operators to change bridge settings at runtime from within IRC (e.g. via PRIVMSG to the bridge server) or Discord (e.g. bot commands). Currently config changes require editing `config.toml` and sending SIGHUP.
- **Configurable quit-on-offline** — Currently pseudoclients persist when a Discord user goes offline (set AWAY instead of QUIT). A config option `pseudoclients.quit_on_offline` could quit them instead, optionally with an idle delay before quitting. See spec 06 "Presence policy" section.
- **Reintroduce-on-kill delay** — Currently `reintroduce_on_kill` re-introduces immediately after the KILL (with a 30s cooldown to prevent loops). A configurable delay before reintroduction would be less aggressive and give IRC operators time to address the issue that prompted the KILL.

## Completed features (post-v1)

- ~~**Mention resolution**~~ — Implemented in PR #14. Real resolvers use bridge state (display_names, channel_names, role_names from guild_create, plus PseudoclientManager nick lookup).
- ~~**Nick-colon mention**~~ — Implemented in PR #18. Leading `nick: ` in IRC messages converted to Discord mentions.
- ~~**KILL handling**~~ — Implemented in PR #21. Pseudoclient cleanup on KILL + optional reintroduction with fresh UID and cooldown.
- ~~**Orchestrator refactor**~~ — PR #23 merged. Extracted `BridgeState` from `run_bridge` for deterministic testing. Fixed nick collision race via deferred introduction.
- ~~**LinkPhase state machine**~~ — PR #24 merged. Replaced boolean flags with `LinkPhase` enum. Simplified kill cooldown.
- ~~**Pseudoclient hostnames**~~ — PR #23. Changed from `sanitize_nick.host_suffix` to `{user_id}.discord.com`.
- ~~**Presence fixes**~~ — PR #25. Cache display names for offline members, extract display name from presence_update payload, AWAY :Offline for offline users, proper burst on BurstComplete (not LinkUp), re-burst on reconnect, buffer Discord events when link is Down.

## Bugs fixed during integration

- Missing `GUILDS` gateway intent — Discord never sent `GUILD_CREATE`, so pseudoclients were never created via the normal burst path
- Double nick prefix on plain IRC→Discord path — `relay.rs` and `send.rs` both prepended `**[nick]**`
- Pre-link duplicate UID race — Discord events arriving before IRC handshake completed produced commands that raced with the burst (fixed properly via deferred introduction in PR #23)
- Nick collision during burst — Pseudoclients introduced before EOS collided with real IRC users; fixed by buffering Discord events until burst completes
- PONG token mismatch — UnrealIRCd echoes link_name not SID in PONG; fixed to accept either
- Bots excluded from pseudoclients — Discord bots lack presence data and were treated as offline
- SJOIN optional modes — UnrealIRCd omits channel modes parameter when none are set
- Windows shutdown race — control_rx closing immediately on Windows caused bridge to exit
- Offline members not introduced when coming online — display names not cached for offline members in MemberSnapshot; fixed by including all members
- Discord events before LinkUp silently dropped — MemberSnapshot arriving before IRC handshake updated PM state but IRC commands were dropped; fixed by buffering in Down phase
- EOS sent on LinkUp instead of BurstComplete — our burst was empty; pseudoclients sent post-burst. Fixed to send UIDs+SJOINs+EOS on BurstComplete
- Pseudoclients lost on IRC reconnect — pm retained state but UIDs never re-sent to new link; fixed by re-bursting on BurstComplete
- Spurious ClearAway on first introduction — new pseudoclients sent AWAY clear despite never being away
- Offline presence silently dropped — users going offline produced no IRC command; now sends AWAY :Offline

## Completed milestones

- Rewrote all specs for UnrealIRCd S2S architecture (pseudoclient model, S2S handshake, webhooks)
- Implemented `specs/01-configuration`: all 6 tasks, 0 surviving mutants
- Implemented `specs/05-formatting`: 111 tests, 0 surviving mutants, chrono for server-time
- Implemented `specs/06-pseudoclients`: 103 tests, 0 surviving mutants, nick sanitization + collision chain + UID gen + state management
- Implemented `specs/07-irc-message-types`: 82 tests, 0 surviving mutants, `IrcMessage`/`IrcCommand`/`UidParams`/`SjoinParams`
- Implemented `specs/02-irc-connection`: 400 tests, 3 equivalent mutants (documented), S2S handshake + rate limiter + reconnect + full session loop
- Implemented `specs/03-discord-connection`: 25 handler + 16 send + 11 types + 8 connection tests, 1 equivalent mutant + 6 shim integration-only + 5 HTTP integration-only (all documented), Gateway event handling + webhook send + config reload
