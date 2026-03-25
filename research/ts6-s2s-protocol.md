# TS6 IRC S2S Protocol

## Summary

TS6 is the server-to-server protocol used by ircd-ratbox, charybdis, and solanum. It is built around globally-unique SIDs (server identifiers) and UIDs (user identifiers) for collision-free routing, and uses timestamp-based conflict resolution for both nicks and channels. The core handshake is PASS → CAPAB → SERVER → SVINFO → burst, with no explicit end-of-burst command — burst completion is instead signalled by the initiator sending a remote PING after its burst, which the receiver answers with a PONG.

---

## Findings

### 1. Server linking / handshake

TS6 uses a sequential handshake. All messages use plain-text IRC framing (`\r\n` terminated), identical to RFC 1459.

**Initiating server sends first:**

```
PASS <password> TS 6 :<our-SID>
CAPAB <capability-list>
SERVER <our-servername> 1 :<description>
```

**Listener receives SERVER, validates, then sends:**

```
PASS <password> TS 6 :<our-SID>
CAPAB <capability-list>
SERVER <our-servername> 1 :<description>
SVINFO 6 6 0 <current-unix-ts>
<burst data>
```

**Initiator then sends:**

```
SVINFO 6 6 0 <current-unix-ts>
<burst data>
```

#### PASS format

```
PASS <password> TS <ts-version> :<SID>
```

Example: `PASS secretpassword TS 6 :0AC`

The `TS 6` literal identifies this as a TS6 link. Earlier versions used `TS 5` or `TS`.

#### CAPAB format

```
CAPAB :QS ENCAP EX IE CHW KNOCK TB SAVE EUID EOPMOD BAN MLOCK SERVICES RSFNC CLUSTER
```

Capabilities are a space-separated list in a trailing parameter.

**Required capabilities (all charybdis TS6 links must send):**
- `QS` — Quit storm: a single SQUIT removes the splitting server and all users behind it without individual QUIT messages (equivalent to UnrealIRCd's NOQUIT)
- `ENCAP` — ENCAP command support

**Strongly recommended:**
- `EX` — ban exception mode (+e)
- `IE` — invite exception mode (+I)
- `CHW` — channel wall (@#channel messages)
- `KNOCK` — KNOCK command
- `SAVE` — nick collision resolution via UID-as-nick (avoids kills)
- `EUID` — extended UID (includes realhost + account in one message)
- `TB` — topic burst command
- `SERVICES` — services integration
- `RSFNC` — remote services forced nick change

**Optional/feature-gated:**
- `EOPMOD` — extended +z and topic moderation
- `BAN` — propagated network bans (K/R/X-lines)
- `MLOCK` — channel mode lock from services
- `CLUSTER` — server clustering
- `ZIP` — compression (ziplinks)
- `KLN`, `UNKLN` — remote K-line management
- `GLN` — G-lines

#### SERVER format (initial handshake)

```
SERVER <servername> <hopcount> :<description>
```

`hopcount` is always `1` in the initial handshake (direct link).

#### SVINFO format

```
SVINFO <current-ts-version> <min-ts-version> 0 <current-unix-timestamp>
```

Example: `SVINFO 6 6 0 1711234567`

SVINFO verifies protocol compatibility and clock synchronisation. If the minimum version is not supported, or if the clock offset is too large, the link is dropped.

---

### 2. Burst sequence

After the handshake, each side sends its full burst in this order:

1. `SID` and `SERVER` messages for all known remote servers
2. `BAN` messages for all propagated network bans (if CAP_BAN)
3. `UID` or `EUID` messages for all known users
   - Possibly followed by `ENCAP REALHOST`, `ENCAP LOGIN`, and/or `AWAY` per user
4. `SJOIN` messages for all known channels
   - Possibly followed by `BMASK` (for ban/exception/quiet lists) and/or `TB` (topic)
5. A remote `PING` to signal end of burst (see §4 below)

---

### 3. User introduction (burst)

#### UID format

```
:<SID> UID <nick> <hopcount> <nickTS> <umodes> <username> <visible-hostname> <ip> <uid> :<gecos>
```

Fields (positional, no labels on wire):

| Position | Field | Notes |
|----------|-------|-------|
| 1 | nick | Display nickname |
| 2 | hopcount | Always `1` for directly connected users |
| 3 | nickTS | Unix timestamp of nick registration (or last nick change) |
| 4 | umodes | User mode string, e.g. `+iw` |
| 5 | username | Ident / username |
| 6 | visible-hostname | Displayed hostname (may be a cloak/vhost) |
| 7 | ip | IP address in text form; `0` if hidden; IPv6 uses `::` shortening; a colon-leading IPv6 address is prepended with `0` (e.g. `0::1`) |
| 8 | uid | 9-char UID: 3-char SID + 6 alphanumerics |
| 9 | gecos | Real name (GECOS), colon-prefixed trailing parameter |

Example:
```
:0AC UID Alice 1 1711234567 +iw alice host.example.com 192.0.2.1 0ACAAAAAB :Alice Smith
```

**Key differences from UnrealIRCd UID:**
- TS6 UID has no `servicestamp` field (services account is conveyed separately via `ENCAP LOGIN`)
- TS6 UID has no separate `virthost` / `cloakedhost` fields; only one hostname is sent (the visible one)
- TS6 UID has no Base64-encoded binary IP; it uses plain text
- TS6 UID carries `hopcount` explicitly; UnrealIRCd also carries it
- TS6 has no equivalent of UnrealIRCd's `MD`/`s2s-md/` tags in the UID line

#### EUID format (charybdis TS6 extension; requires EUID capability)

```
:<SID> EUID <nick> <hopcount> <nickTS> <umodes> <username> <visible-hostname> <ip> <uid> <real-hostname> <account-name> :<gecos>
```

Adds two fields after `<uid>`:

| Field | Notes |
|-------|-------|
| real-hostname | Actual hostname (pre-cloak); `*` if same as visible-hostname |
| account-name | Services account name; `*` if not logged in |

EUID consolidates what UID + `ENCAP REALHOST` + `ENCAP LOGIN` conveyed separately, avoiding a race window during burst.

---

### 4. Channel burst

#### SJOIN format

```
:<SID> SJOIN <channelTS> <#channel> <simple-modes> [<mode-params>...] :<member-list>
```

Example:
```
:0AC SJOIN 1700000000 #general +nt :@+0ACAAAAAB +0ACAAAAAC 0ACAAAAAD
```

`<simple-modes>` is the full channel mode string minus list modes (e.g. `+nts`, `+ntk`, `+ntl 50`). Mode parameters (key, limit) immediately follow as separate positional parameters before the trailing member list.

**Member list encoding:**

Each entry is `<status-prefix><uid>`, separated by spaces, in a single trailing parameter.

Status prefix characters:

| Prefix | Status |
|--------|--------|
| `@` | Op (+o) |
| `+` | Voice (+v) |
| `@+` | Op + voice |
| _(none)_ | Regular member |

Note: charybdis/solanum also support `~` (owner, +q) and `%` (halfop, +h) prefix characters in some configurations, though these are extension modes.

**TS conflict resolution rules (SJOIN):**

- Incoming `channelTS` is **older** than current → incoming side wins; local modes are wiped, incoming modes applied.
- Incoming `channelTS` is **newer** than current → local wins; incoming modes and member statuses are ignored, users are joined without status.
- Incoming `channelTS` **equals** current → modes are merged (union).

**All ban-like list modes must be burst using BMASK, not inline in SJOIN.**

#### BMASK format (ban/exception/quiet burst)

```
:<SID> BMASK <channelTS> <#channel> <type> :<space-separated-masks>
```

`<type>` is the mode letter: `b` (ban), `e` (ban exception), `I` (invite exception), `q` (quiet, charybdis extension).

Example:
```
:0AC BMASK 1700000000 #general b :*!*@evil.example.com *!*@spam.example.org
```

TS rule: if the incoming `channelTS` is newer than the local channel TS, drop and do not propagate.

#### TB format (topic burst)

```
:<SID> TB <#channel> <topicTS> [<topic-setter>] :<topic>
```

Requires `TB` capability. The topic-setter field is optional; if absent, a server name is used.

Acceptance rule: accept if the channel has no topic yet, or if `topicTS` is older than the current topic's TS and the topics differ. Otherwise discard.

---

### 5. End of burst

TS6 has **no dedicated EOB command**. End of burst is signalled by the initiating server sending a remote PING after its burst completes:

```
:<our-SID> PING <our-servername> <their-servername>
```

The charybdis source describes this as "quick, dirty EOB." When the receiving server responds with a PONG, the initiator knows the remote server has processed the full burst:

```
:<their-SID> PONG <their-servername> <our-servername>
```

Both sides independently complete their burst and send this PING. The link is considered fully synchronised when both PINGs have been answered.

**This contrasts with:**
- UnrealIRCd: explicit `EOS` command (`:<SID> EOS`)
- InspIRCd: explicit `ENDBURST` command (`[:<SID>] ENDBURST`)

---

### 6. Ongoing events

In all ongoing messages, the source prefix is the `<UID>` of the originating user (for user-originated commands) or the `<SID>` of the originating server (for server-originated commands). Nicknames are never used as S2S prefixes once TS6 is established.

#### Nick change

```
:<UID> NICK <new-nick> <new-nickTS>
```

Example: `:0ACAAAAAB NICK Bob 1711234999`

The new `nickTS` is the current time of the nick change (not the original connection timestamp).

#### Quit

```
:<UID> QUIT :<reason>
```

Propagation: broadcast. No QUIT is sent for a user removed by a KILL — the KILL itself is the removal notice.

With `QS` capability active (always enabled in charybdis links): during a netsplit, individual QUITs for users behind the splitting server are suppressed. The SQUIT removes them all implicitly.

#### Channel join (post-burst)

```
:<UID> JOIN <channelTS> <#channel> +
```

The trailing `+` is a literal plus sign indicating no modes are being set. The user is joined with no status. If the channel does not exist, it is created with the given TS and no modes.

#### Channel part

```
:<UID> PART <#channel> :<reason>
```

The reason is a trailing parameter and may be empty.

#### Channel kick

```
:<any-source> KICK <#channel> <target-UID> :<reason>
```

Source may be a UID (user-issued kick) or a SID (server-issued kick). Propagation: broadcast.

#### PRIVMSG / NOTICE

```
:<UID> PRIVMSG <target> :<message>
:<any-source> NOTICE <target> :<message>
```

`<target>` may be `#channel`, a UID, a server mask (`$*.example.com`), or a status-targeted channel message (e.g., `@#channel` if `CHW` capability is active).

NOTICE allows server sources (services can send NOTICEs from a SID).

#### AWAY / AWAY unset

```
:<UID> AWAY :<reason>       (set away)
:<UID> AWAY                 (unset away; no trailing parameter)
```

Propagation: broadcast.

**Contrast with InspIRCd** which adds a timestamp: `:<UUID> AWAY <time> :<reason>`. TS6 carries no timestamp in AWAY.

#### Server split (SQUIT)

```
SQUIT <target-servername> :<reason>
```

Or with prefix when propagating:

```
:<SID> SQUIT <target-servername> :<reason>
```

Removes `<target-servername>` and all servers and users behind it from the network.

#### New server link (post-burst)

New servers are introduced via SID during burst or when a new peer links:

```
:<hub-SID> SID <servername> <hopcount> <new-SID> :<description>
```

Example:
```
:0AC SID irc2.example.com 2 1BC :Second server
```

This is the TS6 equivalent of UnrealIRCd's `:<SID> SID <name> <hops> <sid> :<desc>` — the wire format is identical. Note this differs from the initial `SERVER` command used in the handshake itself.

---

### 7. Keepalive (PING / PONG)

```
:<source> PING <origin-servername> [<destination-servername>]
:<source> PONG <origin-servername> <destination-servername>
```

Parameters are positional (not trailing). The origin field in PING is sent as the server **name** (not SID), and is not used for routing — it is purely informational. The destination parameter in PING is used for remote PINGs (routing the PONG back).

Example keepalive exchange between directly linked servers:

```
:0AC PING irc1.example.com irc2.example.com
:1BC PONG irc2.example.com irc1.example.com
```

Remote PING used for EOB detection (after burst):

```
:0AC PING irc1.example.com irc2.example.com
```

**Contrast with UnrealIRCd and InspIRCd:** Both use server **names** in PING/PONG, but UnrealIRCd uses a trailing parameter form (`PING :<servername>`). InspIRCd uses SIDs as positional parameters throughout. TS6 uses names as positional parameters.

---

### 8. ENCAP — optional message propagation

```
:<source> ENCAP <target-server-mask> <subcommand> [<parameters>...]
```

ENCAP routes a subcommand to all servers matching `<target-server-mask>`. Propagation is independent of whether the receiving server understands the subcommand — unknown ENCAP subcommands are silently discarded. This allows optional features to propagate without requiring universal support.

Common ENCAP subcommands:

| Subcommand | Purpose |
|------------|---------|
| `REALHOST <UID> <real-hostname>` | Convey real hostname for cloaked user (pre-EUID) |
| `LOGIN <UID> <account-name>` | Convey services login (pre-EUID) |
| `CHGHOST <UID> <new-vhost>` | Change visible hostname |
| `DLINE <duration> <ip/cidr> :<reason>` | D-line propagation |
| `KLINE <duration> <user> <host> :<reason>` | K-line propagation |
| `UNKLINE <user> <host>` | Remove K-line |
| `RESV <duration> <nick/channel> :<reason>` | Reserve a nick or channel |
| `RSFNC <UID> <new-nick> <new-nickTS> <old-nickTS>` | Services-forced nick change |
| `SASL ...` | SASL authentication exchange |
| `SU <UID> [<account>]` | Services login/logout |

ENCAP has no direct equivalent in UnrealIRCd. UnrealIRCd uses dedicated first-class commands for each of these purposes (SVS\* commands, TKL, etc.). The ENCAP pattern allows TS6 servers to add new optional message types without a version bump or capability flag.

---

### 9. Mode change commands

#### TMODE — timestamped channel mode change

```
:<source> TMODE <channelTS> <#channel> <modestring> [<mode-params>...]
```

If the incoming `channelTS` is newer than the local TS for the channel, the message is dropped. This prevents stale mode changes from a desynced server from applying to a channel that was recreated with a lower TS.

This is analogous to UnrealIRCd's `MODE` command, but with an explicit TS guard. UnrealIRCd uses `MODE` for both channel and user modes post-burst.

#### MODE — user mode change

```
:<UID> MODE <UID> <modestring>
```

User modes use a simple MODE with the target being the user's own UID.

---

### 10. Nick collision handling

#### SAVE (charybdis TS6; requires SAVE capability)

```
:<SID> SAVE <UID> <nickTS>
```

When two users on different sides of a split have the same nickname with the same timestamp (exact collision), SAVE forces the specified user's displayed nick to their full UID string (e.g., `0ACAAAAAB`). This avoids the kill-both-users behaviour of older protocols.

Without SAVE capability, collisions result in kills via KILL.

---

### 11. MLOCK — channel mode lock

```
:<SID> MLOCK <channelTS> <#channel> :<modestring>
```

Propagated by services servers. Sets a mode lock that prevents the locked modes from being changed. Requires `MLOCK` capability. Analogous to UnrealIRCd's `MLOCK` command (identical purpose, similar format).

---

### 12. BAN — network ban propagation

```
:<SID> BAN <type> <user-mask> <host-mask> <creation-TS> <duration> <lifetime> <oper-name> :<reason>
```

`<type>` is `K` (K-line), `R` (RESV), or `X` (X-line). `<duration>` is seconds (0 = permanent). `<lifetime>` specifies how long the ban is remembered even if expired, to prevent revival after splits.

Acceptance rule: ignore and do not propagate if the incoming `creation-TS` is older than an existing ban of the same type+mask (incoming is already superseded).

---

### 13. Unique TS6 concepts vs UnrealIRCd

The following concepts exist in TS6 but have no direct UnrealIRCd equivalent, or differ substantially:

#### ENCAP wrapper pattern

TS6's ENCAP subcommand mechanism allows extending the protocol with optional message types that propagate transparently to servers that don't understand them. UnrealIRCd has no equivalent — all S2S commands are first-class. New features in UnrealIRCd require a new PROTOCTL token and are only sent to servers that negotiate that token.

#### BMASK (ban list burst)

TS6 separates list-mode bursting into a dedicated `BMASK` command, separate from the channel state burst in SJOIN. UnrealIRCd includes ban list entries directly inline in `SJOIN` using sigil-prefixed entries (`&`, `"`, `'`).

#### TB (topic burst command)

TS6 has a discrete `TB` command for bursting channel topics, gated behind the `TB` capability. UnrealIRCd sends topics during burst using the ordinary `TOPIC` command (or `SVSTOPIC` for services).

#### SAVE (collision resolution)

TS6's `SAVE` command resolves exact nick collisions by renaming a user to their UID string rather than killing both users. UnrealIRCd handles collisions via `SVSNICK` (services) or kill, not via a SAVE primitive.

#### EUID (consolidated user introduction)

TS6's `EUID` combines UID + ENCAP REALHOST + ENCAP LOGIN into a single atomic message. UnrealIRCd achieves this differently: its `UID` command carries all host/vhost/cloak fields directly in the command, and services account is a separate `SVSLOGIN` or conveyed via `s2s-md/` tags.

#### QS (quit storm suppression)

TS6's `QS` capability suppresses individual QUIT messages during netsplits. UnrealIRCd's `NOQUIT` token is the direct equivalent.

#### No explicit EOB command

TS6 uses a remote PING after burst to signal end of burst. UnrealIRCd uses explicit `EOS`. InspIRCd uses explicit `ENDBURST`. The TS6 approach is implicit and requires the receiver to correlate the PONG with the end of burst.

#### SID identifier format

TS6 SIDs: one digit followed by two alphanumerics, e.g. `0AC`, `1BC`. Identical format to UnrealIRCd SIDs and InspIRCd SIDs — this is consistent across all three protocol families.

---

### 14. TS6 vs UnrealIRCd — comparison table

| Aspect | TS6 (charybdis/solanum) | UnrealIRCd |
|--------|------------------------|------------|
| **Handshake opener** | `PASS <pw> TS 6 :<SID>` | `PASS :<password>` |
| **Capability negotiation** | `CAPAB :<token-list>` (single message) | `PROTOCTL <tokens>` (may be multiple messages; `EAUTH` must be first) |
| **SID declaration** | In PASS trailing parameter | `PROTOCTL SID=<sid>` before SERVER |
| **Server introduction** | `SERVER <name> 1 :<desc>` (no SID in command) | `SERVER <name> 1 :<desc>` (SID sent via PROTOCTL SID=) |
| **Protocol validation** | `SVINFO 6 6 0 <ts>` | PROTOCTL `VL` token |
| **Clock sync check** | Yes, via SVINFO timestamp | Not explicitly |
| **User introduction** | `UID <nick> <hops> <ts> <umodes> <user> <vhost> <ip> <uid> :<gecos>` (9 params) | `UID <nick> <hops> <ts> <ident> <host> <uid> <svstamp> <umodes> <virthost> <cloakedhost> <ip> :<gecos>` (12 params) |
| **Extended user intro** | `EUID` (adds realhost + account inline) | No EUID; UnrealIRCd UID already contains all host fields |
| **Services account** | `ENCAP LOGIN` or EUID account field | `SVSLOGIN` or `s2s-md/account` tag in UID burst |
| **Channel burst** | `SJOIN <ts> <chan> <modes> :<@+UID ...>` | `SJOIN <ts> <chan> [+<modes> [params]] :<@%+*~UID ...>` |
| **Prefix symbols in SJOIN** | `@` (op), `+` (voice), `@+` (both) | `*` (~), `~` (&), `@` (@), `%` (%), `+` (+) |
| **Ban list burst** | Separate `BMASK` command | Inline in SJOIN using `&`, `"`, `'` sigils |
| **Topic burst** | Separate `TB` command | `TOPIC` or `SVSTOPIC` during burst |
| **End of burst** | Remote PING after burst (implicit) | Explicit `EOS` command |
| **Nick collision** | `SAVE` (renames to UID string) | Kill via KILL; services use SVSNICK |
| **Netsplit QUIT suppression** | `QS` capability | `PROTOCTL NOQUIT` |
| **Channel mode change** | `TMODE` (with channelTS guard) | `MODE` (no explicit TS guard in command) |
| **PING/PONG format** | `PING <name> <name>` (positional names) | `PING :<servername>` (trailing name) |
| **PING/PONG identifiers** | Server names in PING/PONG | Server names in PING/PONG |
| **Optional message routing** | `ENCAP <mask> <subcmd>` (transparent forwarding) | No equivalent; all commands are first-class |
| **Away timestamp** | Not carried in AWAY | Not carried in AWAY (both same) |
| **Mode locks** | `MLOCK` (with MLOCK capability) | `MLOCK` (similar purpose, same name) |
| **Network ban propagation** | `BAN <type> ...` (unified K/R/X) | `TKL` (unified G/Z/Q-lines, shuns) |
| **WHOIS / numerics routing** | Standard WHOIS routing by server name | Standard WHOIS routing by server name |
| **Services commands** | ENCAP RSFNC, ENCAP SU, MLOCK | First-class SVS\* commands (SVSJOIN, SVSMODE, SVSNICK, etc.) |

---

## References

- [charybdis/doc/technical/ts6-protocol.txt (master)](https://github.com/charybdis-ircd/charybdis/blob/master/doc/technical/ts6-protocol.txt) — accessed 2026-03-25
- [solanum/doc/technical/ts6-protocol.txt (main)](https://github.com/solanum-ircd/solanum/blob/main/doc/technical/ts6-protocol.txt) — accessed 2026-03-25
- [grawity/irc-docs — ts6.txt](https://github.com/grawity/irc-docs/blob/master/server/ts6.txt) — accessed 2026-03-25
- [grawity/irc-docs — ts6v7.txt](https://github.com/grawity/irc-docs/blob/master/server/ts6v7.txt) — accessed 2026-03-25
- [grawity/irc-docs — ts6-euid.txt](https://github.com/grawity/irc-docs/blob/master/server/ts6-euid.txt) — accessed 2026-03-25
- [ircd-seven/doc/technical/ts6-protocol.txt (freenode)](https://github.com/freenode/ircd-seven/blob/master/doc/technical/ts6-protocol.txt) — accessed 2026-03-25
- [charybdis/include/s_serv.h — CAP flags](https://github.com/charybdis-ircd/charybdis/blob/master/include/s_serv.h) — accessed 2026-03-25
- [charybdis/modules/core/m_server.c — EOB via PING](https://github.com/charybdis-ircd/charybdis/blob/master/modules/core/m_server.c) — accessed 2026-03-25
- [charybdis/doc/technical/ts6-protocol.txt (Debian 3.5.3-1)](https://sources.debian.org/src/charybdis/3.5.3-1/doc/technical/ts6-protocol.txt/) — accessed 2026-03-25
- [SASL authentication from the perspective of IRC S2S protocols (grawity gist)](https://gist.github.com/grawity/8389307) — accessed 2026-03-25
- [ratbox.org TS6 documentation](https://www.ratbox.org/documentation/ircd_ts6.php) — accessed 2026-03-25
