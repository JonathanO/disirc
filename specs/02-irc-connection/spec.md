# IRC Server Link (UnrealIRCd S2S)

## Overview

`disirc` connects to UnrealIRCd as a peer server using the UnrealIRCd S2S protocol. The link must be pre-configured on the UnrealIRCd side with a matching `link{}` block. All messages are line-oriented, `\r\n` terminated, max 512 bytes (or 4096 with MTAGS).

### Character encoding

IRC is historically byte-transparent with no mandated encoding. `disirc` assumes all text is **UTF-8**. Incoming bytes that are not valid UTF-8 are replaced with U+FFFD (REPLACEMENT CHARACTER) on read. This is a lossy conversion — the original bytes cannot be recovered. This is acceptable because Discord requires valid UTF-8, and modern IRC networks overwhelmingly use UTF-8.

## Handshake sequence

### 1. Outgoing authentication

After TCP/TLS connection is established, `disirc` sends:

```
PASS :<link_password>
PROTOCTL EAUTH=<link_name>
PROTOCTL NOQUIT NICKv2 SJOIN SJ3 CLK TKLEXT2 NICKIP ESVID MLOCK EXTSWHOIS MTAGS
PROTOCTL SID=<our_sid>
SERVER <link_name> 1 :<description>
```

Ordering constraints:
- `EAUTH=<link_name>` **must be sent as its own PROTOCTL line before any other PROTOCTL tokens** — UnrealIRCd uses it for early identification.
- `SID=<our_sid>` must be sent before `SERVER`.
- When using TLS client certificate authentication instead of a password, send `PASS :*`.

### 2. Receive uplink credentials

UnrealIRCd responds with its own `PASS`, `PROTOCTL`, and `SERVER` lines. `disirc` must:
- Verify the received `PASS` matches the configured `link_password`. Close the connection if it does not.
- Record the uplink's SID from its `PROTOCTL SID=` line.
- Record which PROTOCTL capabilities the uplink advertises (used to gate optional features such as `server-time`).

If the uplink sends `ERROR` at any point during the handshake, log at `ERROR` level and **exit** — this indicates a misconfiguration, not a transient failure.

### 3. Burst

After both sides have sent `SERVER`, `disirc` sends its burst:

**Introduce each active pseudoclient:**
```
:<our_sid> UID <nick> 1 <unix_timestamp> <ident> <host> <uid> 0 +i * * * :<realname>
```

UID field positions:
| # | Field | Value for pseudoclients |
|---|-------|------------------------|
| 1 | nick | Sanitized Discord username |
| 2 | hopcount | `1` |
| 3 | timestamp | Unix timestamp of introduction |
| 4 | ident | `pseudoclients.ident` from config |
| 5 | host | `<sanitized_nick>.<host_suffix>` |
| 6 | uid | `<our_sid>` + 6 alphanumeric chars |
| 7 | servicestamp | `0` |
| 8 | umodes | `+i` (invisible) |
| 9 | virthost | `*` |
| 10 | cloakedhost | `*` |
| 11 | ip | `*` (services convention) |
| 12 | realname | Discord display name |

**Join pseudoclients to bridged channels (one SJOIN per channel):**
```
:<our_sid> SJOIN <unix_timestamp> <#channel> + :<uid>
```

The timestamp should match the channel's creation time if known from the uplink's burst; otherwise use the current time.

**Signal end of burst:**
```
:<our_sid> EOS
```

### 4. Receive uplink burst

While sending our own burst, the uplink simultaneously sends its burst: `UID` commands for all existing users, `SJOIN` commands for all channels, then `EOS`. `disirc` must process this to build local state (see [State tracking](#state-tracking)).

Note: **AWAY state is not included in UID burst lines**. A user's away status must be tracked from `AWAY` commands received separately during and after sync.

## State tracking

`disirc` maintains in-memory state rebuilt from scratch on every connect:

- All known IRC users: `uid → { nick, ident, host, server_sid }`
- All channel members: `channel → Set<uid>`
- Our pseudoclients: `discord_user_id → { uid, nick }` (see `06-pseudoclients.md`)

This state is used for:
- Loop prevention (filtering our own UIDs from incoming PRIVMSG)
- Nick collision detection during burst
- Resolving IRC nicks to Discord user IDs for mention conversion

## Ongoing protocol

### Incoming messages to handle

| Message | Action |
|---------|--------|
| `PING :<token>` | Reply `:<our_sid> PONG <our_sid> :<token>` immediately (bypasses rate limiter) |
| `:<sid> UID <...>` | Add user to local state |
| `:<uid> NICK <newnick> :<ts>` | Update nick in local state |
| `:<uid> QUIT :<reason>` | Remove user from local state |
| `:<sid> SJOIN <ts> <#channel> <modes> :<members>` | Update channel membership in local state |
| `:<uid> PART <#channel>` | Update channel membership in local state |
| `:<uid> KICK <#channel> <target>` | Update channel membership in local state |
| `:<uid> PRIVMSG <#channel> :<text>` | Relay to Discord if channel is bridged and UID is not one of our pseudoclients |
| `:<uid> NOTICE <#channel> :<text>` | Relay to Discord as italic (see `04-message-bridging.md`) |
| `:<uid> AWAY :<reason>` | Update pseudoclient away state if targeted user is known |
| `:<uid> AWAY` | Clear pseudoclient away state if targeted user is known |
| `ERROR :<msg>` | Log at `ERROR`, close connection, begin reconnect backoff |
| `:<sid> SID <name> <hop> <newsid> :<info>` | Record new server in local state |
| `:<sid> SQUIT <target> :<reason>` | Remove server and all its users from local state |
| `SVSNICK <uid> <newnick> :<ts>` | Apply forced nick change to pseudoclient if targeted at one of ours |

All other messages are logged at `DEBUG` and ignored.

### Incoming message tag handling

When `MTAGS` is active, incoming messages may carry message tags. Rules:

- `@time=` — pass through to Discord as the message timestamp (see `05-formatting.md`).
- `@msgid=` — record for future edit/delete correlation; do not forward to Discord in v1.
- `@account=` — ignore; IRC account names are not mapped to Discord identities.
- `@bot=` — ignore.
- `s2s-md/*` — **always discard**. These carry oper credentials, TLS cipher info, and internal server state; they must never reach Discord.
- `@unrealircd.org/userhost` — **always discard**. This tag leaks a user's real hostname behind their cloak.
- All other unknown tags — discard silently.

### Outgoing messages

All outgoing messages except PING/PONG pass through the rate limiter (see [Rate limiting](#rate-limiting)).

| Purpose | Message |
|---------|---------|
| Pseudoclient speech | `:<uid> PRIVMSG <#channel> :<text>` |
| Pseudoclient speech with server-time | `@time=<iso8601> :<uid> PRIVMSG <#channel> :<text>` |
| Pseudoclient away (set) | `:<uid> AWAY :<reason>` |
| Pseudoclient away (unset) | `:<uid> AWAY` |
| Pseudoclient quit | `:<uid> QUIT :<reason>` |
| New pseudoclient introduction | `UID` then `SJOIN` (same as burst) |
| Keepalive | `PING :<our_sid>` |

`server-time` tags are only emitted when the uplink advertised `MTAGS` in its `PROTOCTL`.

## Rate limiting

All outgoing lines pass through a token-bucket rate limiter before writing to the socket:

- Bucket capacity: 10 lines
- Refill rate: 1 line per 500 ms
- If the bucket is empty, the line is queued in-memory (unbounded) and sent when a token becomes available.

`PING` and `PONG` bypass the limiter and are written immediately.

## Ping / keepalive

- `disirc` sends `PING :<our_sid>` to the uplink every 90 seconds.
- If no `PONG` is received within 60 seconds of a `PING`, the link is considered dead and reconnect begins.

## Reconnection

On any link failure (socket error, `ERROR` after a successful link, ping timeout):
1. Close the connection.
2. Discard all local IRC state (rebuilt on next connect).
3. Reconnect with exponential backoff: 5 s, 10 s, 20 s, 40 s … capped at 5 minutes.
4. On reconnect, repeat the full handshake and burst sequence.

While the link is down, incoming Discord messages are dropped (not buffered).

## References

- [research/unreal-ircd-s2s-protocol.md](../research/unreal-ircd-s2s-protocol.md) — handshake sequence, UID/SJOIN syntax, PROTOCTL capabilities, NOQUIT behaviour
- [UnrealIRCd Server Protocol — Introduction](https://www.unrealircd.org/docs/Server_protocol:Introduction) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — UID command](https://www.unrealircd.org/docs/Server_protocol:UID_command) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — SJOIN command](https://www.unrealircd.org/docs/Server_protocol:SJOIN_command) — accessed 2026-03-22
