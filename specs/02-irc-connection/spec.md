# IRC Server Link (UnrealIRCd S2S)

## Overview

`disirc` connects to UnrealIRCd as a peer server using the UnrealIRCd S2S
protocol. The connection module is responsible for:

1. Maintaining the TCP/TLS socket and line framing.
2. Running the UnrealIRCd-specific handshake state machine.
3. **Translating** between `IrcMessage` wire types and protocol-agnostic
   `S2SEvent` / `S2SCommand` types.
4. Rate-limiting outbound traffic.
5. Sending keepalive pings and detecting timeouts.
6. Reconnecting with exponential backoff on link failure.

The processing task never sees `IrcMessage` or any other UnrealIRCd wire type.
All communication between the connection module and the rest of the application
passes through the `S2SEvent` / `S2SCommand` boundary.

### Character encoding

IRC is historically byte-transparent with no mandated encoding. `disirc`
assumes all text is **UTF-8**. Incoming bytes that are not valid UTF-8 are
replaced with U+FFFD (REPLACEMENT CHARACTER) on read. This is lossy — the
original bytes cannot be recovered — and is acceptable because Discord requires
valid UTF-8 and modern IRC networks overwhelmingly use UTF-8.

---

## Protocol-agnostic types

These types live in `src/s2s_event.rs` (or `src/irc_connection/types.rs` —
exact path TBD at implementation time). They must not reference any
UnrealIRCd-specific types.

### `S2SEvent` — inbound (connection module → processing task)

```
S2SEvent::LinkUp
    // Handshake complete; link is ready for burst.

S2SEvent::LinkDown { reason: String }
    // Link lost or closed; processing task should discard all IRC state.

S2SEvent::UserIntroduced {
    uid: String,       // 9-char UID
    nick: String,
    ident: String,
    host: String,      // displayed hostname (cloak or real)
    server_sid: String,
    realname: String,
}

S2SEvent::UserNickChanged { uid: String, new_nick: String }

S2SEvent::UserQuit { uid: String, reason: String }

S2SEvent::ServerIntroduced { sid: String, name: String }

S2SEvent::ServerQuit { sid: String, reason: String }
    // Processing task must remove the server and all users homed to it.

S2SEvent::ChannelBurst {
    channel: String,
    ts: u64,
    members: Vec<(String, MemberPrefix)>,  // (uid, prefix)
}

S2SEvent::UserJoined { uid: String, channel: String }

S2SEvent::UserParted { uid: String, channel: String, reason: Option<String> }

S2SEvent::UserKicked {
    uid: String,
    channel: String,
    by_uid: String,
    reason: String,
}

S2SEvent::MessageReceived {
    from_uid: String,
    target: String,    // channel or UID
    text: String,
    timestamp: Option<DateTime<Utc>>,  // from @time= tag; None if absent
}

S2SEvent::NoticeReceived {
    from_uid: String,
    target: String,
    text: String,
}

S2SEvent::AwaySet { uid: String, reason: String }

S2SEvent::AwayCleared { uid: String }

S2SEvent::NickForced { uid: String, new_nick: String }
    // Services-forced nick change (SVSNICK on the wire).
    // Processing task must update PseudoclientManager if this UID is ours.

S2SEvent::BurstComplete
    // Uplink has sent EOS; its burst is fully received.
    // Processing task may now emit S2SCommand::BurstComplete.
```

`MemberPrefix` is an enum: `Owner`, `Admin`, `Op`, `HalfOp`, `Voice`, `None`.

### `S2SCommand` — outbound (processing task → connection module)

```
S2SCommand::IntroduceUser {
    uid: String,
    nick: String,
    ident: String,
    host: String,
    realname: String,
    // Connection module supplies: timestamp, hopcount, umodes, servicestamp,
    // virthost, cloakedhost, ip — these are fixed for pseudoclients.
}

S2SCommand::JoinChannel {
    uid: String,
    channel: String,
    ts: u64,
}

S2SCommand::QuitUser { uid: String, reason: String }

S2SCommand::PartChannel { uid: String, channel: String, reason: Option<String> }

S2SCommand::SendMessage {
    from_uid: String,
    target: String,
    text: String,
    timestamp: Option<DateTime<Utc>>,  // emitted as @time= tag if MTAGS active
}

S2SCommand::SendNotice {
    from_uid: String,
    target: String,
    text: String,
}

S2SCommand::SetAway { uid: String, reason: String }

S2SCommand::ClearAway { uid: String }

S2SCommand::BurstComplete
    // Processing task signals it has finished sending its burst.
    // Connection module translates this to EOS on the wire.
```

Keepalive (`PING`/`PONG`) is managed entirely within the connection module and
does not surface as `S2SCommand` — the processing task never needs to trigger
or respond to keepalive explicitly.

---

## Transport layer

### TCP / TLS

The connection uses `tokio-rustls` for TLS. Whether to use TLS is determined
by the config (`irc.tls = true/false`). The config may optionally supply a
client certificate for mutual TLS authentication (`link_cert_path`,
`link_key_path`); if present, `PASS :*` is sent instead of the real password.

### Line framing

- Lines are `\r\n` terminated.
- Maximum incoming line length: 4096 bytes (MTAGS extended limit).
- Lines longer than 4096 bytes are dropped with a `WARN` log; the connection
  remains open.
- The reader produces `String` values (UTF-8, invalid bytes replaced).
- The writer serialises `IrcMessage` via its `Display` impl (which appends
  `\r\n`) and writes the result to the socket.

---

## Handshake

The handshake is an UnrealIRCd-specific state machine that runs inside the
connection module. No `S2SEvent` is emitted until `LinkUp`.

### 1. Outgoing authentication

Immediately after connection, `disirc` sends:

```
PASS :<link_password>
PROTOCTL EAUTH=<link_name>
PROTOCTL NOQUIT NICKv2 SJOIN SJ3 CLK TKLEXT2 NICKIP ESVID MLOCK EXTSWHOIS MTAGS
PROTOCTL SID=<our_sid>
SERVER <link_name> 1 :<description>
```

Ordering constraints:
- `EAUTH=<link_name>` **must be its own PROTOCTL line sent before any other
  PROTOCTL tokens** — UnrealIRCd uses it for early identification.
- `SID=<our_sid>` must be sent before `SERVER`.
- When using TLS client certificate authentication, send `PASS :*`.

### 2. Receive uplink credentials

UnrealIRCd responds with its own `PASS`, `PROTOCTL`, and `SERVER` lines.
`disirc` must:

- Verify the received `PASS` matches the configured `link_password`. If it
  does not, send `ERROR :Bad password`, close the connection, and **exit** —
  this is a misconfiguration, not a transient failure.
- Record the uplink's SID from its `PROTOCTL SID=` line.
- Record whether the uplink advertised `MTAGS` — used to gate `@time=` tag
  emission on outbound messages.

If the uplink sends `ERROR` at any point during the handshake, log at `ERROR`
and **exit** (fatal — see spec-00 error handling policy).

### 3. Burst

After both sides have sent `SERVER`, burst begins. Both sides burst
simultaneously; ordering within the burst is not guaranteed.

**Outbound burst** (translating `S2SCommand` to wire):

- Receive `S2SCommand::IntroduceUser` for each active pseudoclient → emit
  `:<our_sid> UID ...` then `:<our_sid> SJOIN ...` for each bridged channel.
- Receive `S2SCommand::BurstComplete` → emit `:<our_sid> EOS`.

The UID wire parameters for pseudoclients are:

| Field | Value |
|-------|-------|
| hopcount | `1` |
| timestamp | Unix timestamp at introduction time |
| ident | `pseudoclients.ident` from config |
| host | `<sanitized_nick>.<host_suffix>` from config |
| servicestamp | `0` |
| umodes | `+i` |
| virthost | `*` |
| cloakedhost | `*` |
| ip | `*` (services convention) |

**Inbound burst** (translating wire to `S2SEvent`):

- `UID` → `S2SEvent::UserIntroduced`
- `SJOIN` → `S2SEvent::ChannelBurst`
- `SID` → `S2SEvent::ServerIntroduced`
- `EOS` → `S2SEvent::BurstComplete`

Other commands received during burst (e.g. `PING`, `PRIVMSG`) are handled the
same as in the ongoing phase.

### 4. Link up

`S2SEvent::LinkUp` is emitted after the connection module has sent `SERVER`
and is ready to accept `S2SCommand` values for burst. The processing task may
then query `PseudoclientManager` and begin sending `IntroduceUser` commands.

---

## Translation layer — ongoing messages

### Inbound (wire → `S2SEvent`)

| Wire message | `S2SEvent` emitted |
|---|---|
| `PING :<token>` | _(handled internally — PONG sent immediately, no event)_ |
| `:<uid> PRIVMSG <target> :<text>` | `MessageReceived` |
| `:<uid> NOTICE <target> :<text>` | `NoticeReceived` |
| `:<uid> NICK <newnick> :<ts>` | `UserNickChanged` |
| `:<uid> QUIT :<reason>` | `UserQuit` |
| `:<uid> AWAY :<reason>` | `AwaySet` |
| `:<uid> AWAY` | `AwayCleared` |
| `:<uid> PART <#channel> [:<reason>]` | `UserParted` |
| `:<uid> KICK <#channel> <target> [:<reason>]` | `UserKicked` |
| `:<sid> UID <...>` | `UserIntroduced` |
| `:<sid> SJOIN <...>` | `UserJoined` (post-burst single-member) or `ChannelBurst` |
| `:<sid> SID <...>` | `ServerIntroduced` |
| `:<sid> SQUIT <...>` | `ServerQuit` |
| `SVSNICK <uid> <newnick> :<ts>` | `NickForced` |
| `ERROR :<msg>` | `LinkDown` (then close + begin reconnect backoff) |

All other commands are logged at `DEBUG` and produce no event.

### Outbound (`S2SCommand` → wire)

| `S2SCommand` | Wire |
|---|---|
| `IntroduceUser { ... }` | `:<our_sid> UID <...>` |
| `JoinChannel { uid, channel, ts }` | `:<our_sid> SJOIN <ts> <channel> + :<uid>` |
| `QuitUser { uid, reason }` | `:<uid> QUIT :<reason>` |
| `PartChannel { uid, channel, reason }` | `:<uid> PART <channel> [:<reason>]` |
| `SendMessage { from_uid, target, text, timestamp }` | `[@time=<ts>] :<from_uid> PRIVMSG <target> :<text>` |
| `SendNotice { from_uid, target, text }` | `:<from_uid> NOTICE <target> :<text>` |
| `SetAway { uid, reason }` | `:<uid> AWAY :<reason>` |
| `ClearAway { uid }` | `:<uid> AWAY` |
| `BurstComplete` | `:<our_sid> EOS` |

`@time=` tags are only emitted on `SendMessage` when the uplink advertised
`MTAGS` during handshake.

### Message tag handling

When `MTAGS` is active, incoming messages may carry message tags.

| Tag | Action |
|-----|--------|
| `@time=` | Parsed as `DateTime<Utc>`; placed in `S2SEvent::MessageReceived::timestamp` |
| `@msgid=` | Recorded for future edit/delete correlation; not forwarded to Discord in v1 |
| `@account=` | Ignored; IRC account names are not mapped to Discord identities in v1 |
| `@bot=` | Ignored |
| `s2s-md/*` | **Always discarded** — carries oper credentials, TLS info, internal state |
| `@unrealircd.org/userhost` | **Always discarded** — leaks real hostname behind cloak |
| All other unknown tags | Discarded silently |

---

## State tracking

`disirc` maintains in-memory state rebuilt from scratch on every connect. This
state is owned exclusively by the **processing task** (not the connection
module). The processing task updates its state in response to `S2SEvent`
values.

State to track:

- All known IRC users: `uid → { nick, ident, host, server_sid }`
- All channel members: `channel → Set<uid>`
- Uplink MTAGS capability: `bool` (set at handshake, used to gate `@time=` emission)

This state is used for:

- Loop prevention (filtering our own UIDs from incoming `MessageReceived`)
- Nick collision detection
- Resolving IRC nicks to Discord user IDs for mention conversion
- Routing `NickForced` events to `PseudoclientManager`

**Concurrency**: `PseudoclientManager` and all other state types are not
thread-safe and do not need to be. They are owned by the single processing task
and never accessed concurrently. See the actor model in spec-00.

---

## Rate limiting

All outbound `IrcMessage` writes except `PING` and `PONG` pass through a
token-bucket rate limiter:

- Bucket capacity: 10 tokens
- Refill rate: 1 token per 500 ms
- If the bucket is empty, the line is queued in memory (unbounded) and written
  when a token is available.

`PING` and `PONG` bypass the limiter and are written immediately.

---

## Ping / keepalive

- `disirc` sends `PING :<our_sid>` to the uplink every 90 seconds.
- If no `PONG` is received within 60 seconds of a `PING`, the link is
  considered dead and reconnection begins.
- `PING` from the uplink is answered immediately with
  `:<our_sid> PONG <our_sid> :<token>`, bypassing the rate limiter.

---

## Reconnection

On any link failure (`LinkDown`, socket error, ping timeout):

1. Emit `S2SEvent::LinkDown { reason }` to the processing task.
2. Close the socket.
3. Wait with exponential backoff with full jitter: base delay doubles each
   attempt (5 s, 10 s, 20 s, 40 s … capped at 300 s), then a uniformly random
   value in `[0, capped_delay)` is used as the actual sleep duration. This
   prevents thunderstorm reconnects if multiple bridge instances restart
   simultaneously.
4. Reconnect and repeat the full handshake and burst sequence.

While the link is down, `S2SCommand` values sent by the processing task are
dropped. The processing task is responsible for discarding in-memory IRC state
on receipt of `LinkDown` and re-requesting burst on the next `LinkUp`.

---

## References

- [research/unreal-ircd-s2s-protocol.md](../../research/unreal-ircd-s2s-protocol.md)
- [research/unrealircd-ircv3-s2s.md](../../research/unrealircd-ircv3-s2s.md)
- [research/ts6-s2s-protocol.md](../../research/ts6-s2s-protocol.md)
- [research/inspircd-spanningtree-s2s.md](../../research/inspircd-spanningtree-s2s.md)
- [UnrealIRCd Server Protocol — Introduction](https://www.unrealircd.org/docs/Server_protocol:Introduction) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — UID command](https://www.unrealircd.org/docs/Server_protocol:UID_command) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — SJOIN command](https://www.unrealircd.org/docs/Server_protocol:SJOIN_command) — accessed 2026-03-22
