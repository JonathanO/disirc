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
| Backslash escape `\*`, `\_`, etc. | Remove the backslash; pass the escaped character through literally (do not treat it as a formatting marker) |
| Inline code `` `code` `` | Pass through unchanged; no formatting is applied inside inline code spans |
| Code block ` ```lang\ncode\n``` ` | First line sent as-is; remaining lines sent as continuation `PRIVMSG` lines prefixed with `\x02>\x02 `. No formatting is applied inside code blocks |
| Bold `**text**` | Convert to IRC bold: `\x02text\x02` |
| Italic `*text*` | Convert to IRC italic: `\x1dtext\x1d` |
| Italic `_text_` | Convert to IRC italic **only** when the underscores are at a word boundary (i.e. preceded/followed by whitespace or start/end of string). Intraword underscores (e.g. `some_variable_name`) are left unchanged. This matches Discord's actual rendering behaviour |
| Underline `__text__` | Convert to IRC underline: `\x1ftext\x1f` |
| Strikethrough `~~text~~` | Pass through unchanged, including markers (no IRC equivalent; preserving markers conveys intent) |
| Newlines (`\n`, `\r\n`, `\r`) | Normalise to `\n`, then split; each non-empty line is a separate `PRIVMSG` (max 5 lines; truncate remainder with `[+N more lines]`) |

### Processing order (Discord → IRC)

Transformations must be applied in a specific order to match Discord's own parsing priority:

1. **Backslash escapes** — strip `\` before markdown characters to prevent them from triggering formatting.
2. **Code blocks** (` ``` `) and **inline code** (`` ` ``) — extract and protect; no formatting applies inside code spans.
3. **Mentions and emoji** — resolve `<@id>`, `<#id>`, `<@&id>`, `<:name:id>`, `<a:name:id>`.
4. **Underline** `__text__` — before single `_` to avoid consuming double underscores as two italic markers.
5. **Bold** `**text**` — before single `*` for the same reason.
6. **Italic** `*text*` and word-boundary `_text_`.
7. **Strikethrough** `~~text~~` — passed through unchanged.

This order is derived from Discord's own parser (a fork of [simple-markdown](https://github.com/discord/simple-markdown)) which uses rule priority: code > underline > bold > italic > strikethrough.

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
- [research/discord-markdown-parsing.md](../research/discord-markdown-parsing.md) — Discord's simple-markdown fork, parsing priority, intraword underscore behaviour, backslash escapes
- [discord/simple-markdown](https://github.com/discord/simple-markdown) — Discord's markdown parser (fork of Khan Academy simple-markdown) — accessed 2026-03-24
- [IRCv3 Message Tags specification](https://ircv3.net/specs/extensions/message-tags) — accessed 2026-03-22
- [IRCv3 server-time specification](https://ircv3.net/specs/extensions/server-time) — accessed 2026-03-22
- [IRC formatting reference (modern.ircdocs.horse)](https://modern.ircdocs.horse/formatting) — accessed 2026-03-22
