# Overview

## What is disirc?

`disirc` is a bridge daemon that links to an UnrealIRCd network as a **peer server** using the IRC server-to-server (S2S) protocol. Discord users in bridged channels are represented as real IRC pseudoclients — they appear to other IRC users as ordinary connected users, with nicks, hostnames, and the ability to speak, join, and quit naturally.

## Goals

- Link to UnrealIRCd as a server using the UnrealIRCd S2S protocol (derived from RFC 2813).
- Represent each active Discord user as an IRC pseudoclient with a real nick and hostmask.
- Relay messages bidirectionally: Discord messages appear to come from the user's pseudoclient; IRC messages are forwarded to the mapped Discord channel.
- Support IRCv3 features where UnrealIRCd's S2S layer allows them, to improve the experience.
- Be resilient: reconnect and re-burst on link failure.
- Be configurable via a single TOML config file.

## Non-goals for v1

- No support for non-UnrealIRCd daemons in the initial version (TS6, InspIRCd, etc.).
- No web UI or dashboard.
- No relaying of voice, attachments, or reactions in the initial version.
- No multi-network IRC support (one uplink per instance).
- No multi-guild Discord support (one bot token per instance).
- No DM bridging in the initial version (see below).

## Deferred (future versions)

**DM bridging**: IRC users can `/msg` a Discord user's pseudoclient; disirc forwards it as a Discord DM from the bot, and vice versa. The architecture deliberately does not preclude this:
- The message routing layer handles `PRIVMSG` to both channel and UID targets — non-channel targets are dropped in v1 but not discarded at the parsing level.
- Discord `MESSAGE_CREATE` events for DMs are not discarded at the framework level — they are filtered but the filter is intentionally a single, removable check.

## Architecture

### Network topology

```
┌─────────────────────────────────┐
│         UnrealIRCd network      │
│                                 │
│  ┌──────────┐                   │
│  │ IRC users│                   │
│  └────┬─────┘                   │
│       │  S2S link               │
│  ┌────┴──────────────────────┐  │
│  │  disirc (pseudo-server)   │  │
│  │  ┌───────────────────┐    │  │
│  │  │ pseudoclients      │    │  │
│  │  │  Alice (Discord)   │    │  │
│  │  │  Bob   (Discord)   │    │  │
│  │  └───────────────────┘    │  │
│  └────────────┬───────────────┘  │
└───────────────┼──────────────────┘
                │ Discord Gateway + REST
        ┌───────┴────────┐
        │    Discord     │
        └────────────────┘
```

- `disirc` presents itself as an IRC server to the UnrealIRCd network.
- Each Discord user active in a bridged channel is introduced as a pseudoclient (UID) under `disirc`'s SID.
- Messages from Discord are sent as `PRIVMSG` from the pseudoclient's UID.
- Messages from IRC are forwarded to the corresponding Discord channel via the REST API.
- A single async Tokio runtime manages both connections concurrently.

### Internal concurrency model

All mutable application state (pseudoclients, channel mappings, nick tables) is
owned by a single **processing task**. The two I/O tasks — IRC reader and
Discord gateway — communicate with it exclusively through
`tokio::sync::mpsc` channels.

```
┌────────────────────────┐     S2SEvent        ┌─────────────────────────────────┐
│  IRC reader task       │ ──────────────────► │                                 │
│  (parse + translate)   │                     │      processing task            │
└────────────────────────┘                     │                                 │
                                               │  owns: PseudoclientManager      │
┌──────────────────┐         DiscordEvent      │         channel map             │
│  Discord gateway │ ──────────────────────►   │         routing state           │
│  task            │                           └───────────────┬─────────────────┘
└──────────────────┘                                           │
                                         S2SCommand (outbound) │  REST calls
                                                               ▼
                                                 IRC writer task / reqwest
                                                 (translate + serialise)
```

This is the **actor model**: the processing task is the actor; the channels
are its mailbox. Because only one task ever touches the shared state, no
`Mutex` or `RwLock` is needed on the application types. Concurrency bugs
(races, deadlocks) are structurally prevented rather than guarded against.

**Rule**: I/O tasks (IRC reader, Discord gateway, IRC writer) must not hold
references to application state. They communicate only through channels.

The IRC reader task parses raw lines into `IrcMessage` and then translates
them into protocol-agnostic `S2SEvent` values before sending them to the
processing task. The IRC writer task performs the reverse: it receives
`S2SCommand` values and translates them into `IrcMessage` for serialisation.
The processing task never sees dialect-specific wire types.

## IRCv3 features

`PROTOCTL MTAGS` is the single gate that enables all tag propagation on the S2S link. Without it, every tag is silently dropped by UnrealIRCd.

### Active on the S2S link (requires `PROTOCTL MTAGS`)

| Feature | Tag | How disirc uses it |
|---------|-----|--------------------|
| `server-time` | `@time=` | Discord message timestamp preserved when relaying to IRC |
| `message-ids` | `@msgid=` | Auto-generated by UnrealIRCd if omitted; recorded for future edit/delete |

### Client-facing (handled by UnrealIRCd, no S2S action needed)

| Feature | How it works |
|---------|-------------|
| `away-notify` | UnrealIRCd delivers AWAY events to subscribed clients automatically; `disirc` only needs to send `AWAY` commands for its pseudoclients |
| `account-tag` | Delivered by UnrealIRCd to clients for logged-in users; not relevant to disirc pseudoclients |

### Client-only (cannot be influenced from S2S)

`echo-message`, `draft/chathistory`, `standard-replies`, `extended-join`, `multi-prefix`, `userhost-in-names`, `sts`, `sasl` — all have local-client-only code paths in UnrealIRCd.

## Error handling

`disirc` is a long-running daemon. Errors are categorised by their scope of
impact; the response must match that scope.

### Per-message failures — log and continue

A single bad message must never affect other messages or connections. The
daemon drops the message, logs at `WARN`, and continues.

| Situation | Response |
|-----------|----------|
| `SerializeError` building an outgoing IRC message | Drop message, log `WARN` |
| `ParseError` on an incoming IRC line | Skip line, log `WARN` |
| Discord API error on a single send (4xx, rate limit) | Drop message, log `WARN` |

### Per-link failures — reconnect, do not exit

When a connection is lost the daemon tears it down, discards all associated
in-memory state, and reconnects with exponential backoff. The other link
continues operating normally during reconnection.

| Situation | Response |
|-----------|----------|
| IRC socket error | Reconnect IRC link (see `specs/02-irc-connection/spec.md`) |
| IRC ping timeout | Reconnect IRC link |
| `ERROR` from UnrealIRCd after a successful handshake | Reconnect IRC link |
| Discord gateway disconnect | Reconnect Discord gateway |

### Fatal failures — exit the process

Some failures indicate misconfiguration or an unrecoverable environment
problem. Retrying will not help; the operator must intervene.

| Situation | Response |
|-----------|----------|
| `ERROR` from UnrealIRCd **during** the handshake | Log `ERROR`, exit |
| Config file unreadable or invalid at startup | Log `ERROR`, exit |

### Panics

Panics are reserved for programmer errors — violated invariants that
"cannot happen" given correct code. They must never be triggered by runtime
input (malformed messages, network data, user config). `.unwrap()` and
`.expect()` on `Result`/`Option` values derived from external input are
forbidden; use `?` or explicit match arms instead.

### Error types and crates

- **`thiserror`**: define typed error enums in each module (`ParseError`,
  `SerializeError`, etc.) so callers can match on specific variants.
- **`anyhow`**: use in the top-level application and connection layers where
  errors are logged and recovered from rather than matched on. Provides
  context chains (`context()`/`with_context()`) that make log output actionable.

## Protocol layering and future S2S dialects

`irc_message.rs` is explicitly an **UnrealIRCd S2S wire layer**. Commands such
as `UID`, `SJOIN`, `SID`, `PROTOCTL`, and `EOS` are UnrealIRCd-specific; they
are named and structured after the UnrealIRCd wire format and make no attempt
to be generic.

Supporting a second S2S dialect (InspIRCd, TS6/Charybdis, etc.) in a future
version would require a translation layer sitting between the wire and the
application logic. The intended shape of that layer is:

```
pseudoclients / application logic
         │  protocol-agnostic events
         │  (UserIntroduction, ChannelBurst, …)
         ▼
   S2S translation layer   ◄─── one impl per dialect
         │  IrcMessage (wire types)
         ▼
   TCP / TLS socket
```

**Constraint for spec-02 (IRC connection)**: the connection layer must not
allow UnrealIRCd wire types (`IrcCommand::Uid`, `IrcCommand::Sjoin`, etc.) to
be referenced outside of the translation layer itself. Application code —
pseudoclients, message bridging, routing — must speak the protocol-agnostic
event types and remain ignorant of the wire dialect.

In v1 both layers will be implemented for UnrealIRCd only; the split exists so
that a second dialect can be added later by providing a new translation layer
implementation without touching application logic.

### Cross-protocol event comparison

Research into three major S2S dialects (UnrealIRCd, TS6/Charybdis, InspIRCd
SpanningTree) confirms that the following events exist in all three and map
cleanly to protocol-agnostic types. These form the basis of `S2SEvent` and
`S2SCommand`.

| Event / Command | UnrealIRCd | TS6 | InspIRCd |
|---|---|---|---|
| Introduce user | `UID` (12 params) | `UID` (9 params, `EUID` ext) | `UID` (10–11 params) |
| Channel burst | `SJOIN` (bans inline) | `SJOIN` + `BMASK` | `FJOIN` (prefix-letter,uuid:membid) |
| Post-burst join | `JOIN :<uid>` | `JOIN <ts> <#chan> +` | `IJOIN` |
| End of burst | `EOS` | Implicit (PING/PONG) | `ENDBURST` |
| Nick change | `NICK` | `NICK` | `NICK` |
| Quit | `QUIT` | `QUIT` | `QUIT` |
| Part | `PART` | `PART` | `PART` |
| Kick | `KICK` | `KICK` | `KICK` (+ membership ID) |
| Message | `PRIVMSG` / `NOTICE` | `PRIVMSG` / `NOTICE` | `PRIVMSG` / `NOTICE` |
| Away | `AWAY` (no timestamp) | `AWAY` (no timestamp) | `AWAY <time>` |
| Server split | `SQUIT` | `SQUIT` | `SQUIT` |
| Introduce server | `SID` | `SERVER` | `SERVER` |
| Keepalive | `PING :<name>` (trailing) | `PING <name> <name>` (positional) | `PING <SID>` (positional) |

Notable dialect-specific concepts that have no cross-protocol equivalent and
will not appear in the v1 protocol-agnostic types:
- **InspIRCd**: `METADATA`, `ENCAP`, `SAVE`, `LMODE`, membership IDs
- **TS6**: `BMASK`, `TB` (topic burst), `TMODE`, `ENCAP`, `SAVE`
- **UnrealIRCd**: `SVSNICK`, `PROTOCTL`, embedded ban sigils in `SJOIN`

## References

- [research/unreal-ircd-s2s-protocol.md](../../research/unreal-ircd-s2s-protocol.md)
- [research/unrealircd-ircv3-s2s.md](../../research/unrealircd-ircv3-s2s.md)
- [research/ts6-s2s-protocol.md](../../research/ts6-s2s-protocol.md)
- [research/inspircd-spanningtree-s2s.md](../../research/inspircd-spanningtree-s2s.md)
