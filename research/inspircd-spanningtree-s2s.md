# InspIRCd SpanningTree S2S Protocol

## Summary

InspIRCd's server-to-server protocol ("SpanningTree") is a UID/SID-based protocol similar in spirit to TS6. It diverges from UnrealIRCd's S2S in nearly every message: capability exchange uses `CAPAB` instead of `PROTOCTL`/`PASS`; authentication is HMAC-SHA256 embedded in `SERVER`; channel burst uses `FJOIN` (not `SJOIN`) and includes per-membership IDs; end-of-burst is `ENDBURST` (not `EOS`); and InspIRCd adds protocol-level `METADATA`, `ENCAP`, `IJOIN`, `SAVE`, and `LMODE` messages with no UnrealIRCd equivalents. The protocol is versioned (`CAPAB START <ver>`) with versions 1202 (v2), 1205 (v3/v4 compat), and 1206 (v4 only).

---

## Findings

### 1. Server linking / handshake

The handshake has four phases. Both sides drive the exchange simultaneously once TCP/TLS is established.

#### Phase 1 — Protocol version negotiation

```
◀  CAPAB START 1205
▶  CAPAB START 1205
```

Both sides send `CAPAB START <protocol-version>` immediately on connect. The negotiated version is the minimum of both. A mismatch that leaves no common version triggers `ERROR` and disconnection.

Supported version numbers:
- `1202` — InspIRCd 2.x and 3.x
- `1205` — InspIRCd 3.x and 4.x
- `1206` — InspIRCd 4.x only

#### Phase 2 — Capability exchange

Both sides then send a series of `CAPAB` sub-commands followed by `CAPAB END`:

```
CAPAB MODULES   <space-separated module=data list>
CAPAB MODSUPPORT <optional modules>
CAPAB CHANMODES  list=<chars> param=<chars> param-set=<chars> prefix=<ranks:chars> simple=<chars>
CAPAB USERMODES  list=<chars> param=<chars> param-set=<chars> simple=<chars>
CAPAB CAPABILITIES NICKMAX=30 CHANMAX=64 MAXMODES=20 IDENTMAX=10 MAXQUIT=255 MAXTOPIC=307 MAXKICK=255 MAXREAL=128 MAXAWAY=200 MAXHOST=64 MAXLINE=512 CASEMAPPING=ascii GLOBOPS=0
CAPAB EXTBANS    <extended-ban-list>    (v4 / 1206+; replaces old inline token)
CAPAB END
```

The receiving server validates that required modules match, modes are compatible, and size limits are acceptable. In v4 (1206+) module names dropped the `m_` prefix and `.so` suffix, and module link data switched from simple names to URI query string key-value pairs.

Relevant `CAPAB CAPABILITIES` keys renamed in v4:
- `CHANMAX` → `MAXCHANNEL`
- `IDENTMAX` → `MAXUSER`
- `NICKMAX` → `MAXNICK`

#### Phase 3 — Server authentication

```
▶  SERVER irc2.example.com wobble 0 079 :Second Server
◀  SERVER irc1.example.com wibble 0 702 :First Server
```

Wire format (pre-1206):
```
SERVER <server-name> <password> <unused:0> <sid> :<description>
```

Wire format (1206+, `<unused>` field removed):
```
SERVER <server-name> <password> <sid> :<description>
```

The password field is an HMAC-SHA256 digest computed over the session data; it is not transmitted in plaintext. The receiving server verifies the hostname, password, SID uniqueness, and that this server is not already connected.

Remote servers linked behind the new peer are introduced (post-burst) with:
```
:<sid> SERVER <server-name> <password> <sid> :<description>
```

#### Phase 4 — State synchronisation (BURST / ENDBURST)

After SERVER is accepted, the new server sends its burst. Burst order is mandatory:

1. `BURST [<ts>]` — announces the start of burst; optional timestamp for clock sync
2. `SINFO rawversion :InspIRCd-3.2.1` — server software info (and other SINFO keys)
3. `SERVER` lines for any servers already linked behind this one
4. `UID` lines for all users on those servers
5. `OPERTYPE` if any are opers
6. `FJOIN` lines for all channels (with modes and membership)
7. `FTOPIC` for channel topics
8. `METADATA` for any channel/user metadata
9. `ENDBURST` — marks end of burst

Both sides independently send their full burst. Normal message routing resumes after both have sent `ENDBURST`.

---

### 2. User introduction (UID / UUID)

#### Identifier structure

- **SID** (Server Identifier): 3 characters, format `[0-9][A-Z0-9]{2}`. Configured statically; globally unique.
- **UUID**: SID (3 chars) + UID suffix (6 chars, format `[A-Z0-9]{6}`). Total 9 characters. Immutable for the life of the connection — survives nick changes and collisions.

Example: `079AAAAAB` — server SID `079`, user suffix `AAAAAB`.

#### UID wire format

Protocol version ≤ 1205:
```
[:<sid>] UID <uuid> <ts> <nick> <real-host> <displayed-host> <real-user> <ip> <signon> <modes> [<mode-params>]+ :<real-name>
```

Protocol version 1206+:
```
[:<sid>] UID <uuid> <ts> <nick> <real-host> <displayed-host> <real-user> <displayed-user> <ip> <signon> <modes> [<mode-params>]+ :<real-name>
```

The `<displayed-user>` (ident visible to other users) was added in 1206 to distinguish it from `<real-user>`. All other fields are present in both versions.

Field summary:
| Field | Notes |
|---|---|
| `<uuid>` | 9-char UUID (SID + UID) |
| `<ts>` | Nick/account creation timestamp |
| `<nick>` | Current nickname |
| `<real-host>` | Actual connecting hostname |
| `<displayed-host>` | vhost or cloaked host shown to others |
| `<real-user>` | Actual ident/username |
| `<displayed-user>` | Displayed ident (1206+ only) |
| `<ip>` | IPv4, IPv6, or UNIX socket path |
| `<signon>` | Connection timestamp |
| `<modes>` | User mode string (e.g. `+iw`) |
| `<mode-params>` | Mode parameters if any |
| `:<real-name>` | GECOS / real name field |

Comparison with UnrealIRCd `UID`: UnrealIRCd carries `<hopcount>`, `<servicestamp>`, `<virthost>`, `<cloakedhost>` as explicit separate fields and has a fixed 9-char UID. InspIRCd omits hopcount and servicestamp from UID itself (services account is conveyed via `METADATA`), and UUID is always exactly 9 characters with the same SID prefix convention.

---

### 3. Channel burst (FJOIN)

#### FJOIN wire format

```
[:<sid>] FJOIN <channel> <ts> <modes> [<mode-params>]+ :[<prefix-modes>,<uuid>[:<membid>]]+
```

Example:
```
:36D FJOIN #chan 1234567890 +nl 69 :o,36DAAAAAA:420 v,36DAAAAAB:69
```

Fields:
- `<channel>` — channel name
- `<ts>` — channel creation timestamp (used for TS-based merge/collision resolution)
- `<modes>` — channel mode string
- `<mode-params>` — mode parameters (e.g., limit value for `+l`)
- Member list (trailing parameter): space-separated `<prefix-modes>,<uuid>:<membid>` entries
  - `<prefix-modes>`: privilege mode letters (e.g., `o` for op, `v` for voice, `oh` for multiple)
  - `<uuid>`: the user's UUID
  - `:<membid>`: membership ID (unsigned 64-bit integer, unique per user-channel pair)

Membership IDs were added in the protocol to allow `KICK` and other messages to unambiguously refer to a specific join event even if the same user rejoins.

Comparison with UnrealIRCd `SJOIN`: UnrealIRCd encodes prefix modes as `@`, `%`, `+` symbols prepended to the nick/UID in the member list (e.g., `@00AAAAAAA`). InspIRCd separates prefix mode letters from the UUID with a comma. InspIRCd also carries per-membership IDs which UnrealIRCd has no equivalent of.

---

### 4. End of burst

```
[:<sid>] ENDBURST
```

Marks that the source server has finished sending its initial burst. Only usable in the "fully connected" phase (i.e., after SERVER handshake completes). Both sides send this independently.

UnrealIRCd equivalent: `EOS` (End Of Sync, sent as `:<sid> EOS`).

---

### 5. Ongoing events

All ongoing messages use UUID prefixes (`:<uuid>`) for user-originated commands, and SID prefixes (`:<sid>`) for server-originated commands. Nicknames and server names are never used in routed S2S messages — only UUIDs and SIDs.

#### Nick change

```
:<uuid> NICK <new-nick> <ts>
```

Example: `:36DAAAAAA NICK Sadie 1234567890`

The `<ts>` is the original connection timestamp of the user, not the current time.

#### Quit

```
:<uuid> QUIT :<reason>
```

Standard form; reason may be empty. QUIT is broadcast to all servers.

#### Channel join (post-burst)

Post-burst joins use `IJOIN` (Incremental JOIN), not FJOIN:

```
:<uuid> IJOIN <channel> <membid> [<ts> <prefix-modes>]
```

Examples:
- Basic join: `:36DAAAAAA IJOIN #chan 69`
- Join with op: `:36DAAAAAA IJOIN #chan 69 1234567890 o`

`<ts>` and `<prefix-modes>` are optional. If `<ts>` is included it allows TS validation; `<prefix-modes>` lists privilege mode characters.

IJOIN was introduced in v3 (protocol 1202→1205 transition era). It is more efficient than FJOIN for single-user joins because it omits the full channel mode state retransmission.

#### Channel part

```
:<uuid> PART <channel> :<reason>
```

Standard IRC PART form with UUID substituted for nick. Broadcast to servers with users in the channel.

#### Channel kick

```
:<uuid> KICK <channel> <target-uuid> <membid> :<reason>
```

The `<membid>` (membership ID) was added as a third parameter to unambiguously identify which join event is being kicked. This prevents races where a user parts and rejoins between a kick being issued and delivered.

Pre-membership-ID form (older protocol versions):
```
:<uuid> KICK <channel> <target-uuid> :<reason>
```

#### PRIVMSG / NOTICE

```
:<uuid> PRIVMSG <target> :<text>
:<uuid> NOTICE <target> :<text>
```

`<target>` may be a channel name or a UUID. Channel PRIVMSG/NOTICE are **not** broadcast to all servers — InspIRCd compiles a list of only those directly-connected servers that have at least one member of the channel, and sends the message only to those servers. This is the same optimization UnrealIRCd uses.

In v4, a `~context=<chan>` message tag can be attached to a user-targeted PRIVMSG to indicate the context channel from which it was sent.

#### AWAY / AWAY unset

```
:<uuid> AWAY <time> :<reason>     (set away)
:<uuid> AWAY                      (unset away)
```

`<time>` is a UNIX timestamp of when the user became away. UnrealIRCd uses `AWAY :<reason>` with no timestamp field.

#### Server split (SQUIT equivalent)

```
[:<sid>] SQUIT <server-name> :<reason>
```

Informs the network that `<server-name>` has split from the network. The receiving server removes that server and all servers/users behind it.

There is also `RSQUIT` (Remote SQUIT), which allows a server to request a remote server be delinked. RSQUIT is converted to a local SQUIT by the server responsible for the link.

#### New server link

Remote servers are introduced during burst or when a new server links via:

```
[:<sid>] SERVER <server-name> <password> <sid> :<description>
```

Sent by the hub server to inform the rest of the network. The `<password>` field in this context is vestigial/informational; the actual authentication already happened at the direct link.

---

### 6. Keepalive (PING / PONG)

```
:<sid> PING <target-sid>
:<sid> PONG <target-sid>
```

Both PING and PONG take a single `<target>` parameter which is the SID of the target server. Only usable in the "fully connected" phase.

This differs from UnrealIRCd's `PING :<server>` form (which uses the server name, not SID, and uses a trailing parameter). InspIRCd uses a positional parameter and SIDs throughout.

---

### 7. Additional message types

#### METADATA

Carries arbitrary key-value metadata for channels, users, memberships, or the whole network:

```
[:<sid>] METADATA <channel> <ts> <key> [:<value>]        (channel)
[:<sid>] METADATA <uuid> <key> [:<value>]                 (user)
[:<sid>] METADATA {@} <uuid> <channel> <ts> <membid> <key> [:<value>]  (membership, 1206+)
[:<sid>] METADATA {*} <key> [:<value>]                    (network-wide)
```

Common keys:
- `accountname` — services account name (replaces UnrealIRCd's `SVSLOGIN`/`SETIDENT`-via-moddata)
- `accountid` — numeric account ID
- `swhois` — custom WHOIS line
- `ctitle` — custom title
- `mlock` — channel mode lock string
- `topiclock` — topic lock state

METADATA is an InspIRCd-specific mechanism. UnrealIRCd uses `SVSLOGIN`, `SETHOST`, `CHGNAME`, moddata in the `UID` burst, and `s2s-md/` tags to convey analogous information.

#### ENCAP

```
:<uuid> ENCAP <target> <message> [<params>]+
```

`<target>` may be a SID or a glob pattern (e.g., `*.example.com`). If the target is not reachable or the encapsulated message type is unknown, the message is silently dropped (non-fatal). Allows optional module messages to be propagated without requiring all servers to support them.

UnrealIRCd has no direct equivalent. Optional/unknown message types in UnrealIRCd simply generate unknown command warnings or are silently ignored depending on the command.

#### SAVE

```
[:<sid>] SAVE <uuid> <ts>
```

Resolves a nick collision by forcing the specified user's nick to their UUID string. This avoids the kill-and-reconnect storm that older IRC protocols used for collision resolution.

#### FMODE (forced mode change)

```
:<uuid> FMODE <channel> <channel-ts> <modes> [<mode-params>]+
```

Used to apply channel mode changes across the network. Includes the channel creation timestamp for TS-based conflict resolution.

#### FTOPIC

```
:<uuid> FTOPIC <channel> <channel-ts> <topic-ts> [<setter>] :<topic>
```

Sets or syncs a channel topic. The dual-timestamp design (channel creation TS + topic set TS) allows proper resolution when two servers have different topics for the same channel.

#### FHOST / FIDENT / FNAME

Host, ident, and realname changes applied server-side:

```
[:<sid>] FHOST <uuid> <new-host>
[:<sid>] FIDENT <uuid> <new-ident>
[:<sid>] FNAME <uuid> :<new-realname>
```

In 1206+, FHOST and FIDENT accept a second parameter to indicate the new real (not just displayed) hostname/ident.

#### LMODE

```
[:<sid>] LMODE <channel> <ts> <list-mode-char> [<setter> <settime> <mask>]+
```

Synchronises list modes (ban, invite-exception, ban-exception) with setter and set-time information. Added in v3. UnrealIRCd carries this in the `SJOIN` burst and subsequent `MODE` commands.

#### SINFO

```
[:<sid>] SINFO <key> :<value>
```

Carries server software version and description. Keys: `rawversion`, `rawbranch` (1206+), `customversion` (1206+), `desc`.

#### OPERTYPE

```
:<uuid> OPERTYPE :<type>
```

Announces that a user has gained IRC operator status and their oper type. In v4 (1206+) can carry `~name`, `~chanmodes`, `~usermodes`, `~commands`, `~privileges`, `~snomasks` message tags.

#### ADDLINE / DELLINE

```
:<uuid> ADDLINE <type> <mask> <setter> <settime> <duration> :<reason>
:<uuid> DELLINE <type> <mask>
```

Network-wide X-line (ban/exception/etc.) management. `<duration>` of 0 means permanent. Replaces the per-type GLINE/ZLINE/ELINE etc. commands that older versions used.

#### NUM

```
:<uuid> NUM <sid> <target-uuid> <numeric> [<params>]+
```

Forwards an IRC numeric reply to a remote user. Allows WHOIS, WHO, and other query results to traverse the network.

#### ERROR

```
[:<sid>] ERROR :<reason>
```

Signals a protocol error; the connection is about to be closed. Valid in all connection phases.

---

### 8. Key differences from UnrealIRCd S2S

| Aspect | InspIRCd | UnrealIRCd |
|---|---|---|
| **Handshake opener** | `CAPAB START <ver>` | `PASS :<password>` then `PROTOCTL EAUTH=...` |
| **Capability negotiation** | `CAPAB` sub-commands (MODULES, CHANMODES, USERMODES, CAPABILITIES, END) | `PROTOCTL` tokens (NOQUIT, SJOIN, SJ3, CLK, etc.) |
| **Authentication** | HMAC-SHA256 in `SERVER` password field | Plaintext password in `PASS`, or TLS cert (`PASS :*`) |
| **User ID length** | 9 chars (3 SID + 6 UID suffix) | 9 chars (3 SID + 6 UID suffix) — same length |
| **User introduction** | `UID` with displayed-user field (1206+), no hopcount | `UID` with hopcount, servicestamp, virthost, cloakedhost |
| **Channel burst** | `FJOIN` with comma-separated `prefix-modes,uuid:membid` | `SJOIN` with `@%+` symbols prepended to UID |
| **Channel joins post-burst** | `IJOIN` (incremental, efficient) | Standard `JOIN :<uid>` routed |
| **Membership IDs** | Yes — per user-channel join event (unsigned 64-bit) | No equivalent |
| **End of burst** | `ENDBURST` | `EOS` |
| **Nick collision** | `SAVE <uuid> <ts>` (forces nick to UUID) | `SVSNICK` / kill the colliding user |
| **Metadata/extinfo** | `METADATA` command (channel, user, membership, network) | Moddata in UID burst via `s2s-md/` tags; `SVSLOGIN`, `SETHOST`, etc. |
| **Optional messages** | `ENCAP` wrapper for unknown/optional message types | No formal equivalent; unknown commands ignored |
| **AWAY timestamp** | `AWAY <time> :<reason>` (includes set-time) | `AWAY :<reason>` (no timestamp) |
| **PING/PONG** | `PING <target-sid>` (SID, positional) | `PING :<servername>` (name, trailing) |
| **Topic** | `FTOPIC <chan> <chan-ts> <topic-ts> [setter] :topic` (dual timestamp) | `TOPIC <chan> <setter> <topic-ts> :<topic>` |
| **Mode lists** | `LMODE` with setter+settime per entry | Inline in `SJOIN` burst or `MODE` commands |
| **Server query routing** | `SQUERY` message (v4) | `PRIVMSG <service>` |
| **Services account** | `METADATA <uuid> accountname :<name>` | `SVSLOGIN` command |
| **Ban management** | `ADDLINE` / `DELLINE` (unified) | Per-type GLINE, ZLINE, ELINE, etc. |

---

## References

- [InspIRCd Spanning Tree Protocol — Example Connection](https://docs.inspircd.org/server/examples/connection/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — Concepts](https://docs.inspircd.org/server/concepts/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — Messages index](https://docs.inspircd.org/server/messages/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — UID](https://docs.inspircd.org/server/messages/uid/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — FJOIN](https://docs.inspircd.org/server/messages/fjoin/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — CAPAB](https://docs.inspircd.org/server/messages/capab/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — SERVER](https://docs.inspircd.org/server/messages/server/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — BURST](https://docs.inspircd.org/server/messages/burst/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — ENDBURST](https://docs.inspircd.org/server/messages/endburst/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — NICK](https://docs.inspircd.org/server/messages/nick/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — AWAY](https://docs.inspircd.org/server/messages/away/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — IJOIN](https://docs.inspircd.org/server/messages/ijoin/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — FMODE](https://docs.inspircd.org/server/messages/fmode/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — FTOPIC](https://docs.inspircd.org/server/messages/ftopic/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — METADATA](https://docs.inspircd.org/server/messages/metadata/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — ENCAP](https://docs.inspircd.org/server/messages/encap/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — SAVE](https://docs.inspircd.org/server/messages/save/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — PING](https://docs.inspircd.org/server/messages/ping/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — PONG](https://docs.inspircd.org/server/messages/pong/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — SINFO](https://docs.inspircd.org/server/messages/sinfo/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — OPERTYPE](https://docs.inspircd.org/server/messages/opertype/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — ADDLINE](https://docs.inspircd.org/server/messages/addline/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — NUM](https://docs.inspircd.org/server/messages/num/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — ERROR](https://docs.inspircd.org/server/messages/error/) — accessed 2026-03-25
- [InspIRCd Spanning Tree Protocol — Change Log](https://docs.inspircd.org/server/change-log/) — accessed 2026-03-25
- [PyLink inspircd.py protocol implementation](https://github.com/PyLink/PyLink/blob/master/protocols/inspircd.py) — accessed 2026-03-25
- [PyLink ircs2s_common.py (QUIT/PART/KICK wire format)](https://github.com/PyLink/PyLink/blob/master/protocols/ircs2s_common.py) — accessed 2026-03-25
- [SASL authentication across IRC S2S protocols (grawity gist)](https://gist.github.com/grawity/8389307) — accessed 2026-03-25
- [InspIRCd m_spanningtree source tree (insp4 branch)](https://github.com/inspircd/inspircd/tree/insp4/src/modules/m_spanningtree) — accessed 2026-03-25
