# UnrealIRCd IRCv3 Support and S2S Interaction

## Summary

UnrealIRCd advertises a broad set of IRCv3 capabilities to clients via CAP LS. Most IRCv3 features are implemented at the client-facing layer; however, the core message-tag infrastructure (`@time=`, `@msgid=`, `@account=`, `@bot`) is fully propagated in server-to-server (S2S) traffic when both sides advertise `PROTOCTL MTAGS`. A linking pseudo-server that wants to participate in IRCv3 metadata (e.g., preserving message timestamps or account names) must advertise `MTAGS` (and optionally `BIGLINES`) in its own `PROTOCTL` line; without it, UnrealIRCd will strip all tags before forwarding to that peer.

---

## Findings

### 1. IRCv3 capabilities advertised to clients (CAP LS)

Source: ircv3.net/software/servers table for UnrealIRCd (accessed 2026-03-22):

**Full support (all current versions):**
- `cap-notify`
- `account-notify`
- `account-tag`
- `away-notify`
- `batch`
- `bot` (Bot Mode)
- `chghost`
- `echo-message`
- `extended-join`
- `labeled-response`
- `message-tags`
- `msgid`
- `multi-prefix`
- `sasl` (v3.1 and v3.2)
- `server-time`
- `sts`
- `userhost-in-names`
- `WebIRC` / `WebSockets`
- `WHOX`
- `draft/account-extban`
- `draft/chathistory`

**Version-gated:**
- `extended-monitor` — 6.0+
- `invite-notify` — 6.0+
- `Monitor` — 6.0+
- `setname` — 6.0+
- `standard-replies` — 6.1+
- `UTF8ONLY` — 6.2+
- `draft/extended-isupport` — 6.2+
- `draft/network-icon` — 6.2.2+
- `draft/no-implicit-names` — 6.1.5+
- `draft/account-registration` — 6.0+ (add-on module)
- `draft/channel-rename` — 6.1+ (add-on module)
- `draft/message-redaction` — 6.0+ (add-on module)
- `Metadata` — 6.1+ (add-on module)

**Not supported:**
- `draft/multiline`
- `draft/pre-away`
- `draft/read-marker`

---

### 2. S2S-relevant IRCv3 features

#### 2a. PROTOCTL MTAGS — the master gate for all message-tag S2S traffic

`PROTO_MTAGS` (bitmask `0x000040`) is set on a peer connection when it sends `PROTOCTL MTAGS`. The check macro is `SupportMTAGS(client)`. UnrealIRCd only forwards message tags to a peer whose direction link has this flag set — enforced in `_mtags_to_string()` which short-circuits with no output if `!SupportMTAGS(client->direction)`.

**Client tag size limit:** 4,094 bytes
**Server tag size limit (with MTAGS):** 8,191 bytes total tag section

Source: `src/modules/message-tags.c`; `include/struct.h` PROTO_MTAGS definition.

#### 2b. `PROTOCTL BIGLINES` (UnrealIRCd 6.1.1+)

Sets `PROTO_BIGLINES` (`0x000100`). When enabled, full S2S lines can be up to 16,384 bytes (sender + tags + content + terminators) and the server allows 30 parameters (`MAXPARA*2`) per line. Without BIGLINES, the non-tag portion of S2S lines remains at 512 bytes and 15 parameters.

Advertising `BIGLINES` is independent of `MTAGS` but is typically sent together.

Source: `Server_protocol:Changes` docs; `include/struct.h`.

#### 2c. `server-time` / `@time=` tag — propagated S2S

- **Propagation:** Yes. The `mtag_add_or_inherit_time()` function checks for an existing `time` tag in received `recv_mtags`; if found it duplicates it (preserving the original timestamp), otherwise it stamps with the current ISO 8601 time.
- **Validation:** `server_time_mtag_is_ok()` accepts the tag only when `IsServer(client)` — clients cannot originate or spoof it.
- **Format:** ISO 8601 extended, e.g. `@time=2019-12-23T09:55:55.260Z`
- **Requirement for a linking server:** Advertise `PROTOCTL MTAGS`. No separate token is required for server-time specifically.

Source: `src/modules/server-time.c` (accessed via raw GitHub 2026-03-22).

#### 2d. `msgid` / `@msgid=` tag — propagated S2S

- **Propagation:** Yes. `mtag_add_or_inherit_msgid()` inherits an existing msgid from `recv_mtags` or generates a fresh one using 128-bit randomness.
- **Validation:** `msgid_mtag_is_ok()` — only from servers (`IsServer(client)` and non-null value).
- **Generation for multi-event commands (e.g. SJOIN):** Stacks msgids via SHA256 to produce per-sub-event unique IDs.
- **Requirement:** Advertise `PROTOCTL MTAGS`.

Source: `src/modules/message-ids.c` (accessed via raw GitHub 2026-03-22).

#### 2e. `account-tag` / `@account=` tag — propagated S2S

- **Propagation:** Yes. `mtag_add_account()` hooks `HOOKTYPE_NEW_MESSAGE`; if `IsLoggedIn(client)` it sets the tag value to `client->user->account`. For incoming S2S messages `account_tag_mtag_is_ok()` allows the tag if `IsServer(client)`.
- **Value:** The account name string (service username), same as what is shown in `WHOIS` as the logged-in account.
- **Validation:** Clients cannot originate this tag; only servers may inject it.
- **Requirement:** Advertise `PROTOCTL MTAGS`.

Source: `src/modules/account-tag.c` (accessed via raw GitHub 2026-03-22).

#### 2f. `batch` — propagated S2S

- **Propagation:** Yes, with nuance. The `cmd_batch` command handler relays the `BATCH` command to servers unconditionally, but only sends to clients that have the `batch` capability enabled. The `batch_mtag_is_ok()` function accepts the `@batch=` tag only from servers.
- **Use case in S2S:** Used internally for `labeled-response` (intra-server BATCH syntax differs: `:servername BATCH target +xxxxx labeled-response`). Not generally emitted by a bridge for its own messages.
- **Requirement:** Advertise `PROTOCTL MTAGS`.

Source: `src/modules/batch.c` (accessed via raw GitHub 2026-03-22).

#### 2g. `away-notify` — AWAY propagates S2S independently

- **How AWAY propagates:** UnrealIRCd broadcasts `AWAY` to all linked servers via `sendto_server()` regardless of capability negotiation — this is plain S2S flood, not tag-gated.
- **away-notify for clients:** Handled locally. When a user joins a channel, the server sends AWAY state to local clients that have `CAP away-notify` enabled; there is no S2S-specific away-notify mechanism.
- **In UID burst:** AWAY status is not embedded in the UID command; it arrives as a separate `AWAY` command during or after sync.
- **Requirement for a linking server:** None beyond supporting the `AWAY` command in S2S. No PROTOCTL token is needed.

Source: `src/modules/away.c` (accessed via raw GitHub 2026-03-22).

#### 2h. `bot-tag` / `@bot` tag — propagated S2S

- **Propagation:** Yes. `bottag_mtag_is_ok()` allows the tag when `IsServer(client) && (value == NULL)`.
- **Value:** Always null (no value); presence of the tag is the signal.
- **Trigger:** Automatically attached to messages from users carrying user mode `+B` (bot mode) via `mtag_add_bottag()`.
- **Requirement:** Advertise `PROTOCTL MTAGS`.

Source: `src/modules/bot-tag.c` (accessed via raw GitHub 2026-03-22).

#### 2i. `userhost-tag` / `@unrealircd.org/userhost` — propagated S2S

- **Propagation:** Yes. Sent to servers and to IRCops, but not to regular clients.
- **Validation:** Only accepted from servers.
- **Note:** This is a vendor-prefixed tag containing the sender's real `user@host`. It leaks the real hostname of cloaked users; a bridge must not forward this to Discord.

Source: `src/modules/userhost-tag.c` (accessed via raw GitHub 2026-03-22).

#### 2j. `s2s-md/` tags in UID — early moddata

When `PROTOCTL MTAGS` is active, the `UID` command is prefixed with server-only tags conveying user metadata that cannot fit in the fixed UID parameters:

```
@s2s-md/creationtime=1679075545;s2s-md/operlogin=JamesBond;s2s-md/operclass=netadmin-with-override
  UID nickname hopcount timestamp username hostname uid servicestamp umodes virthost cloakedhost ip :gecos
```

Known `s2s-md/` keys (from doc and source):
- `s2s-md/creationtime` — user account creation time
- `s2s-md/operlogin` — IRCOp login name
- `s2s-md/operclass` — IRCOp class
- `s2s-md/certfp` — TLS certificate fingerprint (synced by certfp module)
- `s2s-md/tls_cipher` — TLS cipher string
- `s2s-md/geoip` — GeoIP country code
- `s2s-md/sasl` — SASL mechanism used
- `s2s-md/webirc` — WebIRC origin data

These tags only appear in S2S traffic and are never forwarded to clients. They are used to synchronise module state across the network during burst.

Source: `Server_protocol:UID_command` docs; `Server_protocol:Changes` docs; `src/modules/message-tags.c`.

#### 2k. `labeled-response` — hybrid (S2S relay required)

- `labeled-response` has S2S relay behavior for routing replies back to the originating server when a command is forwarded.
- `labeled_response_mtag_is_ok()` returns 1 unconditionally for servers.
- Format differences: local connections use `:sender BATCH +xxxxx labeled-response`; remote connections use `:servername BATCH target +xxxxx labeled-response`.
- A bridge is unlikely to need to handle this, but must not strip `@label=` tags it receives over S2S if it intends to relay command responses.

Source: `src/modules/labeled-response.c` (accessed via raw GitHub 2026-03-22).

---

### 3. What a linking server must do to use each S2S feature

| Feature | Required PROTOCTL token | Notes |
|---------|------------------------|-------|
| Any message tags at all | `MTAGS` | Master gate; without it UnrealIRCd strips all tags to that peer |
| `@time=` preservation | `MTAGS` | Inherit tag from recv_mtags to preserve original timestamp |
| `@msgid=` preservation | `MTAGS` | Inherit tag; or let UnrealIRCd generate if absent |
| `@account=` on messages | `MTAGS` | Tag is added automatically by UnrealIRCd when user is logged in |
| `@bot` tag | `MTAGS` | Added automatically for `+B` users |
| `@batch=` relay | `MTAGS` | Needed to relay labeled-response batches |
| Early moddata in UID | `MTAGS` | Enables `s2s-md/` prefixed tags on UID lines |
| 16 KB S2S lines | `BIGLINES` | Optional; widens line buffer from 4 KB to 16 KB |
| AWAY propagation | *(none)* | Sent unconditionally via plain `AWAY` command |
| `away-notify` for clients | *(none, client-only)* | Handled locally by each server for its own clients |

The full PROTOCTL sequence UnrealIRCd sends to a new peer (from `src/serv.c send_proto()`):

```
PROTOCTL NOQUIT NICKv2 SJOIN SJOIN2 UMODE2 VL SJ3 TKLEXT TKLEXT2 NICKIP ESVID NEXTBANS [SJSBY] [MTAGS]
PROTOCTL CHANMODES=... USERMODES=... BOOTED=... PREFIX=... SID=xxx MLOCK TS EXTSWHOIS
PROTOCTL NICKCHARS=... CHANNELCHARS=... BIGLINES
```

`MTAGS` is conditionally included in line 1 when the `message-tags` module is loaded.

---

### 4. Client-side only features (no S2S influence from a linking server)

These CAPs are handled entirely at the local client-facing layer. A pseudo-server link cannot control or inject them:

| CAP | Why client-only |
|-----|----------------|
| `echo-message` | `MyUser(client)` guard in both handlers; never touches S2S path |
| `draft/chathistory` | Explicitly `if (!MyUser(client)) return;` — local client command only |
| `standard-replies` | Client CAP only, no S2S hooks |
| `extended-join` | Client-side modification of JOIN output; servers always receive plain JOIN |
| `chghost` | Server sends CHGHOST to capable local clients; S2S uses SETHOST/CHGHOST commands directly |
| `account-notify` | Sent to local clients watching account changes; S2S uses UID servicestamp and SVSLOGIN |
| `multi-prefix` | Modifies NAMES/WHO output for local clients; irrelevant to S2S |
| `userhost-in-names` | Same — local NAMES output enrichment |
| `sts` | STS policy only meaningful to directly connecting TLS clients |
| `sasl` | Authentication happens between client and services; irrelevant to a server link |
| `cap-notify` | CAP negotiation meta-capability; server links don't do CAP LS |

---

### 5. Gotchas and limitations for a bridge pseudo-server

1. **MTAGS is opt-in per direction.** UnrealIRCd checks `SupportMTAGS(client->direction)` before serialising tags. If the bridge's PROTOCTL does not include `MTAGS`, all `@time=`, `@msgid=`, `@account=` tags will be silently dropped on every message from UnrealIRCd to the bridge.

2. **Tag-only messages (`TAGMSG`) require MTAGS.** If a client sends a reaction or typing indicator via TAGMSG, it arrives at UnrealIRCd and must be forwarded S2S. Without MTAGS the bridge will never see TAGMSG at all.

3. **4 KB vs 8 KB tag budget.** Client tag sections are capped at 4,094 bytes; server (MTAGS peer) tag sections at 8,191 bytes. With BIGLINES the total line can be 16,384 bytes. A bridge implementing MTAGS but not BIGLINES must still handle graceful truncation if lines arrive longer than 4 KB (they won't in practice without BIGLINES on the other end, but worth noting).

4. **`s2s-md/` tags must not be forwarded to Discord.** They carry internal server state (oper credentials, TLS ciphers, GeoIP) and are not meaningful outside the IRC network.

5. **`@unrealircd.org/userhost` leaks real hostnames.** This vendor tag contains `user@realhost` and bypasses cloaking. A bridge must explicitly discard it rather than relaying it to Discord.

6. **`@account=` is empty string when not logged in.** The `IsLoggedIn()` guard means the tag is simply absent when no account is set. The bridge should treat absent `@account=` as anonymous, not as a protocol error.

7. **`@msgid=` is generated by UnrealIRCd if absent.** A bridge need not generate its own msgids for messages it injects; UnrealIRCd will assign one. However if the bridge wants to use msgids for Discord message correlation it must read the `@msgid=` from the echoed or forwarded message.

8. **AWAY state is not in the UID burst tag section.** A bridge that needs to know a user's away status at link time must track the `AWAY` command during burst; there is no tag or UID field for it.

9. **`batch` in S2S is for labeled-response routing, not bulk message delivery.** A bridge should not attempt to use BATCH for sending batched Discord history to IRC clients — that is handled by the `draft/chathistory` module which is client-only.

10. **`PROTOCTL NEXTBANS` changes extended ban syntax.** Without it, bans use single-letter format (`~a:account`); with it, named format (`~account:name`). A bridge that inspects MODE/SJOIN lines should advertise NEXTBANS to receive the unambiguous named syntax.

11. **Unknown PROTOCTL tokens are silently ignored.** UnrealIRCd will not error on tokens it does not understand (explicit comment in `protoctl.c`). This means a bridge can safely advertise tokens UnrealIRCd does not know about without breaking the link.

---

## References

- [IRCv3 Software Support — Servers (ircv3.net)](https://ircv3.net/software/servers) — accessed 2026-03-22
- [UnrealIRCd Server Protocol documentation index](https://www.unrealircd.org/docs/Server_protocol) — accessed 2026-03-22
- [Server_protocol:PROTOCTL_command](https://www.unrealircd.org/docs/Server_protocol:PROTOCTL_command) — accessed 2026-03-22
- [Server_protocol:Changes](https://www.unrealircd.org/docs/Server_protocol:Changes) — accessed 2026-03-22
- [Server_protocol:UID_command](https://www.unrealircd.org/docs/Server_protocol:UID_command) — accessed 2026-03-22
- [Server_protocol:MD_command](https://www.unrealircd.org/docs/Server_protocol:MD_command) — accessed 2026-03-22
- [Dev:Message_tags](https://www.unrealircd.org/docs/Dev:Message_tags) — accessed 2026-03-22
- [src/modules/message-tags.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/message-tags.c) — accessed 2026-03-22
- [src/modules/server-time.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/server-time.c) — accessed 2026-03-22
- [src/modules/message-ids.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/message-ids.c) — accessed 2026-03-22
- [src/modules/account-tag.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/account-tag.c) — accessed 2026-03-22
- [src/modules/batch.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/batch.c) — accessed 2026-03-22
- [src/modules/away.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/away.c) — accessed 2026-03-22
- [src/modules/bot-tag.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/bot-tag.c) — accessed 2026-03-22
- [src/modules/echo-message.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/echo-message.c) — accessed 2026-03-22
- [src/modules/labeled-response.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/labeled-response.c) — accessed 2026-03-22
- [src/modules/userhost-tag.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/userhost-tag.c) — accessed 2026-03-22
- [src/modules/protoctl.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/modules/protoctl.c) — accessed 2026-03-22
- [src/serv.c (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/src/serv.c) — accessed 2026-03-22
- [include/struct.h (unreal60_dev)](https://raw.githubusercontent.com/unrealircd/unrealircd/unreal60_dev/include/struct.h) — accessed 2026-03-22
- [IRCv3 message-tags specification](https://ircv3.net/specs/extensions/message-tags) — accessed 2026-03-22
