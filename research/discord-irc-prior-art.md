# Prior Art: FauxFaux/discord-irc

## Summary

`FauxFaux/discord-irc` is a Node.js/TypeScript Discord↔IRC bridge. It connects to Discord as a bot (Gateway client) and to IRC as a regular client. It uses per-channel webhooks (operator-configured) to give IRC users distinct identities in Discord. Several lessons — including one notable safety bug — were extracted and incorporated into the disirc specs.

## Findings

### Discord integration model

- Uses discord.js v13 as a Gateway client with `GUILDS` and `GUILD_MESSAGES` intents.
- Authenticates with a bot token via `discord.login()`.
- Receives messages via the `message` Gateway event; sends to Discord via `channel.send()` (plain path) or `WebhookClient.send()` (webhook path).
- Reconnection delegates entirely to discord.js's built-in retry (`retryLimit: 3`).

### Webhook lifecycle

- **No dynamic webhook creation.** Webhook URLs are operator-managed: created in the Discord UI and pasted into config. The bot parses the URL at startup to extract `id` and `token` from the last two path segments.
- `WebhookClient` objects are created once and reused for the lifetime of the process.
- No invalidation or re-creation on failure — a deleted webhook causes permanent silent message loss.

### Loop prevention

Two checks in `sendToIRC` prevent relay loops:

1. Drop events where `author.id === this.discord.user.id` (bot's own account).
2. Drop events where `author.id` matches any webhook ID in `this.webhooks` (messages the bot sent via webhook).

Both checks are necessary; webhook messages fire as gateway events with a different author ID than the bot account itself.

### @everyone / @here suppression — notable bug

The plain `channel.send()` path computes a `withFilteredMentions` variable that replaces `@everyone`/`@here` with a visually similar non-pinging string, but the **default format template uses `{$withMentions}` not `{$withFilteredMentions}`**. The filtered version is only used if the operator explicitly configures a custom format string referencing that variable. This means **@everyone pings are not suppressed by default on the non-webhook path** — a real safety bug.

On the webhook path, suppression is permission-aware: if the bot lacks `MENTION_EVERYONE`, it passes `disableMentions: 'everyone'` to the webhook send. If the bot *has* that permission, it passes `'none'`, allowing the ping through.

**disirc decision:** suppress @everyone/@here unconditionally on all paths. This is a mandatory safety rule in CLAUDE.md.

### Webhook username constraints

Discord enforces 2–32 characters on webhook usernames. The bot truncates to 32 and pads short nicks with `_`. No further sanitization — characters invalid in webhook usernames (e.g. `@`, `#`, `:`) are passed through.

**disirc decision:** same 2–32 constraint enforced; nick sanitization goes further (see `specs/03-discord-connection.md`).

### Ping-fix (zero-width space)

An optional `parallelPingFix` inserts `U+200B` (zero-width space) after the first character of the IRC sender's nick when relaying to IRC, preventing IRC highlights for users present on both sides.

Applied as: `displayUsername.slice(0, 1) + '\u200B' + displayUsername.slice(1)`

**disirc decision:** apply ping-fix to the nick field only (webhook username / `**[nick]**` prefix), not message body text. Specified in `specs/05-formatting.md`.

### Attachment handling

Sent as separate IRC `say()` calls with the CDN URL. Text and attachment URLs are separate messages. Empty text with an attachment sends only the URL.

**disirc decision:** same pattern — text first, then each attachment URL as a separate `PRIVMSG` (see `specs/04-message-bridging.md`).

### Flood protection

IRC flood protection (`floodProtection: true`, `floodProtectionDelay: 500ms`) is delegated entirely to the `irc-upd` client library. No Discord-side rate limiting.

**disirc decision:** implement an explicit token-bucket rate limiter in the S2S output path (capacity 10, refill 1/500ms). Specified in `specs/02-irc-connection.md`. Discord REST rate limiting is handled by serenity.

### Async safety

Every IRC event handler is an `async` function, but the `irc-upd` event emitter does not `await` them. Concurrent events can interleave and corrupt shared state. The codebase acknowledges this with `// TODO: almost certainly not async safe` comments.

**disirc decision:** all IRC and Discord events are funnelled through `tokio::sync::mpsc` channels to a single processing task per direction. Mandated in CLAUDE.md code style.

### Other bugs noted

- `WebhookClient` constructor receives positional string arguments with an `as any` cast — acknowledged as wrong in source comments but works at runtime due to JavaScript's dynamic dispatch.
- `this.discord.users.find('id', dId)` in embed mention resolution is a discord.js v11 API that does not exist in v13 — this code path is broken at runtime.
- Channel mapping supports both IDs and names in config, with ID taking precedence. Ambiguous and underdocumented.
- No handling of webhook 429 rate limits or 50035 invalid form body errors — webhook errors are fire-and-forget (`catch(logger.error)`).

## References

- [FauxFaux/discord-irc](https://github.com/FauxFaux/discord-irc) — analysed 2026-03-22
