# Spec Status

Tracks implementation status for each spec file.

| Spec | Topic | Status |
|------|-------|--------|
| [`specs/00-overview/spec.md`](specs/00-overview/spec.md) | Architecture & goals | N/A — architecture overview |
| [`specs/01-configuration/spec.md`](specs/01-configuration/spec.md) | Config file format & validation | Implemented |
| [`specs/02-irc-connection/spec.md`](specs/02-irc-connection/spec.md) | UnrealIRCd S2S link lifecycle | Implemented |
| [`specs/03-discord-connection/spec.md`](specs/03-discord-connection/spec.md) | Discord bot & gateway | Implemented |
| [`specs/04-message-bridging/spec.md`](specs/04-message-bridging/spec.md) | Relay rules & routing | Implemented |
| [`specs/05-formatting/spec.md`](specs/05-formatting/spec.md) | Message formatting & transforms | Implemented |
| [`specs/06-pseudoclients/spec.md`](specs/06-pseudoclients/spec.md) | Pseudoclient lifecycle & identity | Implemented |
| [`specs/07-irc-message-types/spec.md`](specs/07-irc-message-types/spec.md) | Typed IRC message representation | Implemented |
| [`specs/08-e2e-testing/spec.md`](specs/08-e2e-testing/spec.md) | End-to-end testing strategy | In Progress |

## Statuses

- **Pending** — spec written, not yet implemented
- **In Progress** — implementation underway
- **Implemented** — code written, tests passing, mutants clean
