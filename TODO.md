# TODO

High-level status tracker. Each spec has its own `TODO.md` in its directory.

Updated by Claude at the start and end of each session, and whenever task status changes.

## In progress

(none)

## Spec status

| Spec | Status | Detail |
|------|--------|--------|
| [specs/00-overview](specs/00-overview/TODO.md) | n/a — architecture doc | — |
| [specs/01-configuration](specs/01-configuration/TODO.md) | ✅ Implemented | — |
| [specs/02-irc-connection](specs/02-irc-connection/TODO.md) | ⏳ Pending | 9 tasks |
| [specs/03-discord-connection](specs/03-discord-connection/TODO.md) | ⏳ Pending | 7 tasks |
| [specs/04-message-bridging](specs/04-message-bridging/TODO.md) | ⏳ Pending | 6 tasks |
| [specs/05-formatting](specs/05-formatting/TODO.md) | ✅ Implemented | — |
| [specs/06-pseudoclients](specs/06-pseudoclients/TODO.md) | ⏳ Pending | 8 tasks |
| [specs/07-irc-message-types](specs/07-irc-message-types/TODO.md) | ⏳ Pending | 5 tasks |

## Future specs (deferred from v1)

- **DM bridging** — IRC `PRIVMSG` to a pseudoclient UID forwarded as a Discord DM and vice versa. Architecture must not preclude this: route `PRIVMSG` to non-channel targets; do not discard Discord DM `MESSAGE_CREATE` events at the framework level.

## Completed milestones

- Rewrote all specs for UnrealIRCd S2S architecture (pseudoclient model, S2S handshake, webhooks)
- Implemented `specs/01-configuration`: all 6 tasks, 0 surviving mutants
- Implemented `specs/05-formatting`: 111 tests, 0 surviving mutants, chrono for server-time
