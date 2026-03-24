# Message Formatting

## Discord → IRC

Because the Discord user is represented as an IRC pseudoclient, no sender prefix is added. The message text is sent as-is after transformation.

### Transformations applied

| Input | Transformation |
|-------|---------------|
| Discord mention `<@123456>` | Replace with `@nick` if user is resolvable in local state, else `@<id>` |
| Discord mention `<@!123456>` (legacy member mention) | Same as `<@123456>` |
| Discord channel mention `<#123456>` | Replace with `#channel-name` if resolvable, else `#deleted-channel` |
| Discord role mention `<@&123456>` | Replace with `@role-name` if resolvable, else `@deleted-role` |
| Custom emoji `<:name:id>` | Reduce to `:name:` |
| Animated emoji `<a:name:id>` | Reduce to `:name:` |
| Bold `**text**` | Convert to IRC bold: `\x02text\x02` |
| Italic `*text*` or `_text_` | Convert to IRC italic: `\x1dtext\x1d` |
| Underline `__text__` | Convert to IRC underline: `\x1ftext\x1f` |
| Strikethrough `~~text~~` | Pass through unchanged, including markers (no IRC equivalent; preserving markers conveys intent) |
| Inline code `` `code` `` | Pass through unchanged |
| Code block ` ```lang\ncode\n``` ` | First line sent as-is; remaining lines sent as continuation `PRIVMSG` lines prefixed with `\x02>\x02 ` |
| Newlines (`\n`, `\r\n`, `\r`) | Normalise to `\n`, then split; each non-empty line is a separate `PRIVMSG` (max 5 lines; truncate remainder with `[+N more lines]`) |

### Length splitting

If a single formatted line exceeds 400 bytes, split at the last word boundary before the limit and send the remainder as a continuation `PRIVMSG`. No continuation prefix is added — the message reads naturally as multiple lines from the same pseudoclient.

### IRC nick ping-fix (Discord → IRC)

When a Discord user's display name or resolved mention text would appear in the message and matches an IRC user's nick in the channel, no modification is made to the text. The pseudoclient model means the Discord user *is* the IRC user, so highlights are expected and correct.

## IRC → Discord

Native IRC users connect to the IRC network independently and have no corresponding Discord account. Unlike Discord users (who appear on IRC as pseudoclients and are their own IRC sender), native IRC users cannot be represented as a real Discord sender. Their nick must therefore be shown explicitly.

**Webhook path:** nick is set as the webhook `username` field — the message body needs no prefix. This is the preferred path as it gives IRC users a distinct visual identity in Discord.

**Plain path:** Format as `**[nick]** message text`.

### IRC control character stripping

All raw IRC control characters must be stripped or converted before sending to Discord. Unhandled characters sent as-is would appear as garbage or invisible characters in Discord.

| Input | Transformation |
|-------|---------------|
| IRC bold `\x02text\x02` | Convert to `**text**` |
| IRC italic `\x1dtext\x1d` | Convert to `*text*` |
| IRC underline `\x1ftext\x1f` | Convert to `__text__` |
| IRC strikethrough `\x1etext\x1e` | Convert to `~~text~~` |
| IRC reverse `\x16` | Strip (treat as italic for best-effort rendering) |
| IRC color codes `\x03[N[,M]]` | Strip color codes and any trailing reset; keep text content |
| IRC reset `\x0f` | Strip |
| Any remaining Unicode Cc (control) characters | Strip |

Processing order: parse all formatting as a sequence of styled spans, then emit Discord markdown. This handles nested and overlapping styles correctly.

All IRC→Discord transformations assume the incoming text is valid UTF-8 (see [spec-02 §Character encoding](02-irc-connection.md#character-encoding) for how non-UTF-8 bytes are handled at the connection layer).

### Mention conversion (IRC → Discord)

`@nick` in IRC text is compared case-insensitively against Discord members in the bridged channel. If a match is found, replace with `<@discord_user_id>`. If no match, leave as plain `@nick`.

### Ping-fix (IRC nick in Discord)

When the IRC nick appears as the webhook username or in the `**[nick]**` prefix (plain path), insert a zero-width space (`U+200B`) after the first character. This prevents Discord from notifying any Discord user whose display name matches the IRC nick.

Example: nick `alice` → `a\u200Blice` as the webhook username.

This applies to the **nick field only**, not to message body text.

### Length limit

Discord's message limit is 2000 characters. If the formatted message body exceeds this, truncate at the last word boundary before 2000 characters and append `… [truncated]`.

## IRCv3: server-time

When the uplink has negotiated `server-time`, outgoing `PRIVMSG` messages on the IRC side (Discord → IRC) include the original Discord message timestamp as a message tag:

```
@time=2024-01-15T12:34:56.789Z :<uid> PRIVMSG #channel :<text>
```

Timestamps are formatted as ISO 8601 UTC (`YYYY-MM-DDTHH:MM:SS.mmmZ`).

## References

- [research/discord-irc-prior-art.md](../research/discord-irc-prior-art.md) — ping-fix zero-width space, IRC control character handling, webhook username constraints
- [IRCv3 Message Tags specification](https://ircv3.net/specs/extensions/message-tags) — accessed 2026-03-22
- [IRCv3 server-time specification](https://ircv3.net/specs/extensions/server-time) — accessed 2026-03-22
- [IRC formatting reference (modern.ircdocs.horse)](https://modern.ircdocs.horse/formatting) — accessed 2026-03-22
