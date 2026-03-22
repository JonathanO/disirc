# Message Bridging

## Channel mapping

Each `[[bridge]]` entry pairs one Discord channel with one IRC channel. Messages flow in both directions across that pair.

- One Discord channel maps to exactly one IRC channel, and vice versa.
- Messages in unmapped channels are silently ignored.
- The channel map is mutable at runtime via config reload (see `01-configuration.md`). The routing layer must consult the current map on every message, not a snapshot taken at startup.

## Discord → IRC

1. A `MESSAGE_CREATE` event arrives for a mapped Discord channel.
2. Filter: skip if the author is the bot itself or any webhook owned by the bot (see `03-discord-connection.md`).
3. Filter: skip if message text and attachments are both empty after trimming (e.g., sticker-only with no text).
4. Look up (or create) the pseudoclient for the Discord user (see `06-pseudoclients.md`).
5. Format the message text (see `05-formatting.md`).
6. Send as `PRIVMSG` from the pseudoclient's UID to the mapped IRC channel:
   ```
   :<uid> PRIVMSG #channel :<formatted-text>
   ```
7. If there are attachments, send each attachment URL as an additional `PRIVMSG` line.
8. If IRCv3 `server-time` is negotiated with the uplink, include the Discord message timestamp:
   ```
   @time=2024-01-15T12:34:56.789Z :<uid> PRIVMSG #channel :<text>
   ```

The message appears on IRC as if the Discord user themselves sent it — no bracketed prefix needed.

### Content edge cases (Discord → IRC)

| Case | Behaviour |
|------|-----------|
| Whitespace-only message | Skip (do not relay) |
| Attachment with no text | Send each attachment URL as a `PRIVMSG`; no text line |
| Text + attachments | Send text first, then each attachment URL as separate `PRIVMSG` lines |
| Sticker only | Send `[sticker: <name>]` as the message text |
| Custom emoji `<:name:id>` | Reduce to `:name:` (see `05-formatting.md`) |
| Multi-line message | Each non-empty line sent as a separate `PRIVMSG` (max 5 lines; remainder truncated with `[+N more lines]`) |

## IRC → Discord

1. A `PRIVMSG` line arrives for a mapped IRC channel from an IRC user.
2. Filter: skip if the sender UID belongs to one of our own pseudoclients (loop prevention).
3. Format the message (see `05-formatting.md`).

### Preferred path: webhook

If the channel has a `webhook_url` configured:
- Send via webhook with `username` set to the IRC nick (truncated/padded to 2–32 chars) and the configured `avatar_url` if present.
- Pass `allowed_mentions: { parse: [] }` to suppress `@everyone`/`@here`.
- On webhook send failure, fall back to the plain channel send path and log at `WARN`.

### Fallback path: plain channel send

If no webhook is configured, or the webhook send fails:
- Format as `**[nick]** message text` and send via `channel.send()`.
- Suppress `@everyone`/`@here` by inserting a zero-width space (`U+200B`) after the `@`.

### IRC event types

| IRC event | Discord output |
|-----------|---------------|
| `PRIVMSG` | Normal message (webhook or plain) |
| `NOTICE` | Message wrapped in `*...*` (italic) |
| `ACTION` (/me) | Message prefixed with `* nick ` and sent as plain text |

### Ping-fix

When sending an IRC user's nick to Discord (as the webhook username or in the formatted prefix), insert a zero-width space (`U+200B`) after the first character of the nick. This prevents Discord from highlighting any Discord user whose name matches the IRC nick.

## Ordering

- Messages are relayed in the order received on each side.
- No cross-side ordering guarantee is made.

## Error handling

- If sending a message to Discord fails, log at `WARN` and continue. Do not retry.
- If sending a `PRIVMSG` to IRC fails (link down), the message is dropped. The reconnect flow in `02-irc-connection.md` handles recovery.
- If a bridged Discord channel becomes inaccessible (bot removed, channel deleted), log at `ERROR` and continue processing other channels. Do not crash.

## Loop prevention

- **Discord → IRC**: filter messages whose `author.id` matches the bot's user ID or any webhook user ID owned by the bot.
- **IRC → Discord**: filter messages whose sender UID has our SID as its prefix.
- These two rules together prevent relay loops regardless of network topology.

## References

- [research/discord-irc-prior-art.md](../research/discord-irc-prior-art.md) — attachment handling, loop prevention, @everyone suppression, webhook fallback pattern
- [Discord Message Object](https://discord.com/developers/docs/resources/message) — accessed 2026-03-22
