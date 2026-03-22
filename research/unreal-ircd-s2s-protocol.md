# UnrealIRCd Server-to-Server Protocol

## Summary

UnrealIRCd's S2S protocol is derived from RFC 2813 but diverges substantially in every major area. The integer "server token" model from RFC 2813 is replaced by globally-unique 3-character SIDs and 9-character UIDs. Capability negotiation moves from PASS flags into a dedicated PROTOCTL command. Channel bursting is handled by SJOIN (with timestamp-based conflict resolution) rather than NJOIN. The result shares the line-oriented IRC framing of RFC 2813 but is not compatible with it.

## Findings

### Handshake sequence

After TCP/TLS connection, the connecting server sends (in order):

```
PASS :<password>
PROTOCTL EAUTH=<servername>
PROTOCTL <capabilities...>
PROTOCTL SID=<sid>
SERVER <servername> 1 :<description>
```

Key constraints:
- `EAUTH=<servername>` **must be the first PROTOCTL token sent** — UnrealIRCd uses it for early identification before SERVER.
- `PASS :*` is sent instead of a real password when using TLS client certificate authentication.
- The `SID=<sid>` token declares our Server ID before SERVER is sent.

Common PROTOCTL capabilities to advertise:
```
NOQUIT NICKv2 SJOIN SJ3 CLK TKLEXT2 NICKIP ESVID MLOCK EXTSWHOIS
```

After both sides have sent SERVER, each sends a burst of network state followed by `EOS`.

The receiving side also sends `NETINFO` after burst:
```
NETINFO <maxglobal> <timestamp> <protocolversion> <cloakhash> 0 0 0 :<networkname>
```

### Server IDs (SID)

- Exactly 3 alphanumeric characters; **first character must be a digit** (`[0-9][A-Z0-9]{2}`).
- Globally unique across the network — not per-connection like RFC 2813 tokens.
- Configured statically in the server config; announced via `PROTOCTL SID=` and the `SID` command.
- Introduced to the rest of the network via:
  ```
  :<prefix> SID <servername> <hopcount> <sid> :<description>
  ```

### User IDs (UID)

UIDs are 9 alphanumeric characters. The first 3 characters are the server's SID. This makes UIDs globally unique and self-routing.

The `UID` command introduces a user to the network (replaces NICK in S2S):

```
UID <nick> <hopcount> <timestamp> <ident> <host> <uid> <servicestamp> <umodes> <virthost> <cloakedhost> <ip> :<gecos>
```

Full example:
```
UID James 1 1469538255 bond sis.gov.uk 00AAAAAAA 0 +ixwo Clk-123A45B6.gov.uk * :Bond, James Bond
```

Fields:
| Position | Field | Notes |
|----------|-------|-------|
| 1 | nick | Display nick |
| 2 | hopcount | `1` for directly introduced |
| 3 | timestamp | Unix timestamp of nick registration |
| 4 | ident | Username / ident |
| 5 | host | Real hostname |
| 6 | uid | 9-char UID (`SID` + 6 chars) |
| 7 | servicestamp | Services account stamp; `0` or `*` if none |
| 8 | umodes | User modes string (e.g. `+i`) |
| 9 | virthost | Virtual host; `*` if none |
| 10 | cloakedhost | Cloaked hostname; `*` if none |
| 11 | ip | Base64-encoded binary IP; `*` for services |
| 12 | gecos | Real name (GECOS), colon-prefixed |

Nick changes after introduction:
```
:<uid> NICK <newnick> :<timestamp>
```

### SJOIN — channel burst and joins

SJOIN replaces both JOIN and NJOIN from RFC 2813. Used during burst and for ongoing joins.

```
SJOIN <timestamp> <#channel> [+<modes> [<params>]] :<memberlist>
```

Example:
```
SJOIN 1100000000 #test +nt :@00AAAAAAA +00BBBBBBB
```

Member list prefixes:
| Prefix | Status |
|--------|--------|
| `*` | Channel owner (~) |
| `~` | Channel admin (&) |
| `@` | Channel op (@) |
| `%` | Half-op (%) |
| `+` | Voice (+) |
| _(none)_ | Regular member |

List mode entries in the SJOIN buffer use special sigils:
| Sigil | Meaning |
|-------|---------|
| `&` | Ban |
| `"` | Ban exception |
| `'` | Invite exception |

Timestamp conflict resolution:
- Remote timestamp **older** → remote wins; local modes are wiped.
- Remote timestamp **newer** → local wins; remote is instructed to clean up.
- **Equal** → modes are merged.

### NOQUIT — netsplit handling

With `PROTOCTL NOQUIT` active (mandatory in UnrealIRCd 5+):
- Individual `QUIT` messages are **suppressed** during netsplits.
- A single `SQUIT` removes the splitting server and all its users implicitly.
- This dramatically reduces traffic during netsplits.

For ongoing individual user disconnects (not netsplits), `QUIT` is still used normally:
```
:<uid> QUIT :<reason>
```

### PRIVMSG routing

Identical to RFC 2813's spanning-tree model but uses UID prefixes instead of nicks:
```
:<uid> PRIVMSG #channel :<text>
```

With MTAGS (message tags) active, tags are prepended:
```
@time=2024-01-15T12:34:56.789Z :<uid> PRIVMSG #channel :<text>
```

### Message length

- Standard: 512 bytes (inherited from RFC 1459).
- With `PROTOCTL MTAGS`: up to 4096 bytes.
- With `PROTOCTL BIGLINES` (UnrealIRCd 6.1+): up to 16384 bytes.

### Authentication

Three modes (configured in UnrealIRCd's `link{}` block):
1. **Password only**: `PASS :<password>`
2. **TLS certificate only**: `PASS :*` — certificate fingerprint verified against config
3. **Both**: password + certificate fingerprint

### Authentication differences vs RFC 2813

RFC 2813 §5.3 explicitly states that DNS + plaintext password is the only authentication mechanism and acknowledges it as weak. UnrealIRCd adds TLS mutual authentication and pre-registration identity (EAUTH), making the link substantially more secure.

### Key commands with no RFC 2813 equivalent

| Command | Purpose |
|---------|---------|
| `UID` | User introduction (replaces NICK) |
| `SID` | Server introduction with SID |
| `SJOIN` | Channel burst and joins with TS conflict resolution |
| `NETINFO` | Network metadata exchange |
| `EOS` | End-of-sync / end-of-burst marker |
| `TKL` | Network-wide ban propagation (G/Z/Q-lines, shuns) |
| `MD` | Moddata — attach module-defined metadata to users/channels |
| `CHGIDENT`/`CHGHOST`/`CHGNAME` | Change ident/host/realname |
| `SVS*` | Services commands (`SVSNICK`, `SVSJOIN`, `SVSMODE`, etc.) |
| `SLOG` | Structured log event broadcast (JSON via MTAGS, v6+) |

### Where RFC 2813 and UnrealIRCd agree

- Line-oriented framing, `\r\n` terminated.
- Spanning-tree topology — one path between any two servers.
- `PASS` + `SERVER` as the basis of the handshake (though syntax differs).
- `PING`/`PONG` keepalive.
- `PRIVMSG`, `NOTICE`, `PART`, `KICK`, `MODE`, `TOPIC` command names (syntax may differ).
- `SQUIT` to announce server disconnection.
- `KILL` to force-disconnect a user.

## References

- [UnrealIRCd Server Protocol — Introduction](https://www.unrealircd.org/docs/Server_protocol:Introduction) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — PROTOCTL](https://www.unrealircd.org/docs/Server_protocol:PROTOCTL_command) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — UID command](https://www.unrealircd.org/docs/Server_protocol:UID_command) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — SJOIN command](https://www.unrealircd.org/docs/Server_protocol:SJOIN_command) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — Server ID](https://www.unrealircd.org/docs/Server_protocol:Server_ID) — accessed 2026-03-22
- [UnrealIRCd Server Protocol — User ID](https://www.unrealircd.org/docs/Server_protocol:User_ID) — accessed 2026-03-22
- [RFC 2813 — Internet Relay Chat: Server Protocol](https://datatracker.ietf.org/doc/html/rfc2813) — accessed 2026-03-22
