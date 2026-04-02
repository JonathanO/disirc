# DM Bridging

Bridge private messages between IRC `/msg` and Discord DMs.

## Overview

When an IRC user sends a private message to a Discord user's pseudoclient, the bridge forwards it as a Discord DM from the bridge bot. When a Discord user DMs the bridge bot, the bridge forwards it as an IRC PRIVMSG from the Discord user's pseudoclient to the addressed IRC user.

This feature is **opt-in** via the `dm_bridging` config option (default: `false`). When disabled, private messages to pseudoclients are silently dropped (current v1 behaviour).

## Configuration

```toml
[formatting]
dm_bridging = false   # default: false (opt-in)
```

When `dm_bridging = true`, the bridge:
- Requires the `DIRECT_MESSAGES` gateway intent (bit 12, not privileged)
- Relays IRC private messages to Discord DMs
- Relays Discord DMs to IRC private messages

## IRC → Discord

When an IRC user sends `PRIVMSG <pseudoclient_uid> :text`:

1. The bridge receives `S2SEvent::MessageReceived` with the pseudoclient UID as `target`.
2. Look up the pseudoclient by UID → get the Discord user ID.
3. Open (or reuse) a DM channel with that Discord user via `POST /users/@me/channels`.
4. Send the message to the DM channel via `POST /channels/{dm_channel_id}/messages`.
5. Format: `**[irc_nick]** text` — same as the plain channel path. Webhooks are not available in DMs.
6. Apply the same formatting pipeline as channel messages: IRC control codes → Discord markdown, mention resolution, `@everyone`/`@here` suppression.

### Sender identity

Discord DMs cannot use webhooks, so all messages appear as sent by the bridge bot. The IRC user's nick is embedded in the message text as `**[nick]** message`, identical to the plain (non-webhook) channel path.

### DM channel caching

The bridge should cache the DM channel ID per Discord user to avoid repeated `POST /users/@me/channels` calls. The Discord API returns the existing channel if one already exists, so redundant calls are safe but wasteful.

### Error handling

- If the Discord user has blocked the bot or disabled DMs, the API returns 403 Forbidden. Log a warning and drop the message.
- Rate limit responses (429) should be handled by the existing HTTP retry logic.

## Discord → IRC

When a Discord user sends a DM to the bridge bot:

1. The bridge receives `MESSAGE_CREATE` with no `guild_id` (DM indicator).
2. Determine the target IRC user using **reply context** or **nick-colon addressing**.
3. Send a `PRIVMSG` from the Discord user's pseudoclient to the target IRC user.

### Determining the target IRC user

Discord DMs have no inherent concept of "who on IRC am I talking to." The bridge uses two mechanisms, checked in order:

#### 1. Reply context

If the DM is a reply to a previous message (has `message_reference`), look up the referenced message. If it was a relayed IRC→Discord DM (formatted as `**[nick]** text`), extract the IRC nick from the `**[nick]**` prefix. This is the target.

This enables natural conversational flow:
- IRC user Bob sends `/msg AlicePseudoclient hello`
- Bridge DMs Alice: `**[bob]** hello`
- Alice replies to that message: `hey, what's up?`
- Bridge extracts "bob" from the referenced message → sends PRIVMSG from Alice's pseudoclient to Bob

#### 2. Nick-colon addressing

If the DM is not a reply (or the referenced message can't be resolved), check for a leading `nick: ` pattern at the start of the message (same syntax as `irc_nick_colon_mention`). Look up the nick as an IRC user (external nick in `IrcState`, or pseudoclient nick in `PseudoclientManager`). If found, that's the target; strip the `nick: ` prefix from the relayed text.

This enables initiating new DM conversations:
- Alice DMs the bridge bot: `bob: hey, are you around?`
- Bridge sends PRIVMSG from Alice's pseudoclient to Bob: `hey, are you around?`

#### 3. No target found

If neither mechanism identifies a target, the bridge bot replies to the DM with a short help message:

> To message an IRC user, reply to one of their messages or start your message with `nick: text`.

This message is sent once per unresolvable DM (not repeated for every message).

### Sender identity

The PRIVMSG is sent from the Discord user's **pseudoclient** on IRC. From the IRC user's perspective, it looks like the Discord user is directly `/msg`ing them — which is the correct mental model.

### Pseudoclient availability

If the Discord user doesn't have an active pseudoclient (e.g., they're offline and no pseudoclient was created), the bridge cannot send the PRIVMSG. In this case, reply with an error:

> Unable to relay message — you don't have an active IRC presence. Send a message in a bridged channel first.

### Message reference tracking

To support reply-based addressing, the bridge must track which outbound DM messages correspond to which IRC user. Two approaches:

**Option A: Parse the prefix** — when looking up a referenced message, fetch it from Discord's API and parse the `**[nick]**` prefix. Simple, no state needed, but requires an API call per reply.

**Option B: In-memory map** — maintain a `HashMap<MessageId, String>` mapping outbound DM message IDs to IRC nicks. Avoids API calls but grows unbounded. Should be bounded (e.g., LRU cache with 1000 entries per DM channel).

Recommendation: **Option A** for simplicity. The API call is cheap (single GET) and replies are infrequent compared to channel traffic. Option B can be added later if the API call becomes a bottleneck.

## Formatting

### IRC → Discord DMs

Same pipeline as IRC → Discord channel (plain path):
1. IRC control codes → Discord markdown
2. Mention resolution (`@nick` → `<@id>`)
3. Nick-colon mention conversion (if enabled)
4. `@everyone`/`@here` suppression
5. Prefix with `**[nick]** `
6. Truncate to Discord's 2000 character limit

### Discord → IRC DMs

Same pipeline as Discord → IRC channel messages:
1. Mention resolution (`<@id>` → `@nick`, `<#id>` → `#channel`, `<@&id>` → `@role`)
2. Custom emoji reduction (`<:name:id>` → `:name:`)
3. Discord markdown → IRC control codes
4. Line splitting (max 5 lines)

No `**[nick]**` prefix — the PRIVMSG source is the pseudoclient itself.

## Gateway intent

The `DIRECT_MESSAGES` intent (bit 12 = 4096) must be added to the gateway identify payload when `dm_bridging` is enabled. This intent is **not privileged** and does not require Discord developer portal approval.

## Self-message filtering

The bridge bot's own DMs must not be relayed back. The existing `self_filter` (which contains the bot's user ID) handles this — DM `MESSAGE_CREATE` events from the bot itself are already filtered.

## Privacy and opt-in

- DM bridging is **disabled by default** (`dm_bridging = false`).
- When enabled, Discord users receive DMs from the bridge bot without explicit consent. This is inherent to Discord's bot DM model. The bridge operator should inform users that DM bridging is active.
- Discord users can block the bridge bot to stop receiving DMs. The bridge handles 403 responses gracefully.

## Scope limitations

- **No group DMs**: Only 1:1 DMs between the bridge bot and a Discord user. Group DMs are not supported.
- **No DM history**: The bridge does not replay past IRC messages when a DM channel is opened. Only live messages are relayed.
- **No offline delivery**: If the IRC user is not connected when a Discord DM arrives, the message is dropped (IRC has no offline message store). A warning could be sent back to the Discord user.
- **Single bridge bot**: All DMs go through one bot account. The IRC user's identity is embedded in the message text, not the sender.

## References

- [research/discord-dm-api.md](../../research/discord-dm-api.md)
- [Discord Create DM endpoint](https://discord.com/developers/docs/resources/user#create-dm) — accessed 2026-04-02
- [Discord Gateway Intents](https://discord.com/developers/docs/events/gateway#gateway-intents) — accessed 2026-04-02
