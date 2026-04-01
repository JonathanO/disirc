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

## Future specs (deferred from v1)

- **DM bridging** — IRC `PRIVMSG` to a pseudoclient UID forwarded as a Discord DM and vice versa. Architecture must not preclude this: route `PRIVMSG` to non-channel targets; do not discard Discord DM `MESSAGE_CREATE` events at the framework level.
- **Mention resolution** — Wire serenity cache into `run_bridge` to implement `DiscordResolver` (convert `<@id>`, `<#id>`, `<@&id>` to names) and `IrcMentionResolver` (convert IRC nicks to Discord mentions). Currently using `NoopDiscordResolver` / `NoopIrcResolver` so mentions pass through unconverted.

## Bugs fixed during integration

- Missing `GUILDS` gateway intent — Discord never sent `GUILD_CREATE`, so pseudoclients were never created via the normal burst path
- Double nick prefix on plain IRC→Discord path — `relay.rs` and `send.rs` both prepended `**[nick]**`
- Pre-link duplicate UID race — Discord events arriving before IRC handshake completed produced commands that raced with the burst
- Bots excluded from pseudoclients — Discord bots lack presence data and were treated as offline
- SJOIN optional modes — UnrealIRCd omits channel modes parameter when none are set

## Completed milestones

- Rewrote all specs for UnrealIRCd S2S architecture (pseudoclient model, S2S handshake, webhooks)
- Implemented `specs/01-configuration`: all 6 tasks, 0 surviving mutants
- Implemented `specs/05-formatting`: 111 tests, 0 surviving mutants, chrono for server-time
- Implemented `specs/06-pseudoclients`: 103 tests, 0 surviving mutants, nick sanitization + collision chain + UID gen + state management
- Implemented `specs/07-irc-message-types`: 82 tests, 0 surviving mutants, `IrcMessage`/`IrcCommand`/`UidParams`/`SjoinParams`
- Implemented `specs/02-irc-connection`: 400 tests, 3 equivalent mutants (documented), S2S handshake + rate limiter + reconnect + full session loop
- Implemented `specs/03-discord-connection`: 25 handler + 16 send + 11 types + 8 connection tests, 1 equivalent mutant + 6 shim integration-only + 5 HTTP integration-only (all documented), Gateway event handling + webhook send + config reload
