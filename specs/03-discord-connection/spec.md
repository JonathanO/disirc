# Discord Connection

## Bot setup

`disirc` connects to Discord using the bot token from `discord.token`. It uses the Gateway (WebSocket) API to receive real-time events. The bot must be a member of every guild containing a bridged channel.

## Required Gateway intents

| Intent | Reason |
|--------|--------|
| `GUILD_MEMBERS` | Receive presence/membership events to manage pseudoclient lifecycle |
| `GUILD_MESSAGES` | Receive messages in guild channels |
| `GUILD_PRESENCES` | Receive online/idle/offline status for away-notify support |
| `MESSAGE_CONTENT` | Read message text body (privileged — must be enabled in Discord Developer Portal) |

## Required bot permissions

| Permission | Reason |
|------------|--------|
| `SEND_MESSAGES` | Post IRC messages into bridged channels |
| `MANAGE_WEBHOOKS` | Create and reuse per-channel webhooks for IRC user identity |
| `READ_MESSAGE_HISTORY` | Resolve message references |

## Webhook delivery (IRC → Discord)

Each bridged Discord channel has an optional webhook URL in its `[[bridge]]` config entry. When present, IRC messages are sent via the webhook with the IRC user's nick set as the `username` and an optional avatar URL, making IRC users appear as distinct senders in Discord.

```toml
[[bridge]]
discord_channel_id = "123456789012345678"
irc_channel        = "#general"
webhook_url        = "https://discord.com/api/webhooks/..."
```

If no `webhook_url` is configured for a bridge entry, messages fall back to plain `channel.send()` with the nick formatted as `**[nick]** message`.

### Webhook username constraints

Discord enforces a 2–32 character limit on webhook usernames:
- Truncate nicks longer than 32 characters.
- Pad nicks shorter than 2 characters with `_` (e.g., `x` → `x_`).

### Webhook self-message filtering

Webhooks have their own Discord user ID distinct from the bot's user ID. `disirc` must record the user ID of every webhook it uses and filter incoming `MESSAGE_CREATE` events whose `author.id` matches either the bot's user ID **or** any webhook user ID. Without this, messages sent via webhook would loop back to IRC.

The webhook user ID equals the numeric `{id}` segment embedded in the webhook URL path (`https://discord.com/api/webhooks/{id}/{token}`). No REST call is required to discover it — parse the URL at startup and store the `id` in the self-message filter set.

### Mention safety (webhook path)

When sending via webhook, pass `allowed_mentions` with `parse: []` to suppress `@everyone` and `@here`. If the bot has the `MENTION_EVERYONE` permission and the IRC message explicitly contains `@everyone` or `@here`, this suppression may be relaxed as a future operator option — but the safe default is always to suppress.

### Mention safety (plain channel.send fallback)

On the fallback path, replace `@everyone` and `@here` (case-insensitively) with a visually similar string that does not trigger the ping (e.g., insert a zero-width space: `@\u200Beveryone`).

## Lifecycle

1. On startup, establish a Gateway connection using the bot token. Parse all configured webhook URLs and record the numeric webhook ID from each URL in the self-message filter set.
2. Wait for the `READY` event. Record the bot's own user ID in the self-message filter set.
3. The Gateway delivers a `GUILD_CREATE` event for each guild the bot is in. Each `GUILD_CREATE` (with both `GUILD_MEMBERS` and `GUILD_PRESENCES` intents active) includes `members` and `presences` maps that form the initial snapshot. For large guilds (member count above Discord's large-guild threshold), the `GUILD_CREATE` contains online and role-bearing members only; the remaining members arrive in subsequent `GUILD_MEMBERS_CHUNK` events that serenity merges into the cache automatically. `disirc` does not emit a `MemberSnapshot` for `GUILD_MEMBERS_CHUNK` events — those offline members are introduced lazily on first message (see `06-pseudoclients.md`).
4. This membership snapshot is used to populate the IRC burst (see `02-irc-connection.md`).
5. Enter the event loop dispatching events.

## Config reload (new channels)

When a config reload adds a new `[[bridge]]` entry (see `01-configuration.md`):

1. Read the current member list and presence data for the new Discord channel from the
   serenity cache. The cache is already fully populated by `GUILD_CREATE` and
   `GUILD_MEMBERS_CHUNK` events at startup, and kept current by `PRESENCE_UPDATE` events
   thereafter — no REST call is needed. The cache is searched by iterating guild entries
   and checking each guild's channel map for the target channel ID.
2. Register the webhook user ID for the new channel's webhook (if configured) with the self-message filter.
3. Hand the member and presence snapshot to the pseudoclient layer to introduce new pseudoclients and SJOIN them to the IRC channel (see `06-pseudoclients.md`).
4. Begin routing `MESSAGE_CREATE` events for the new channel.

When a config reload removes a `[[bridge]]` entry:

1. Stop routing `MESSAGE_CREATE` events for that channel immediately (before processing any in-flight events).
2. The pseudoclient layer handles the IRC-side PART/QUIT sequence (see `06-pseudoclients.md`).

## Reconnection

Serenity handles Gateway reconnection and session resumption automatically. `disirc` does not need custom reconnect logic for Discord.

## Events handled

| Event | Action |
|-------|--------|
| `MESSAGE_CREATE` | If in a bridged channel and not from the bot or a webhook it owns, relay to IRC via the sender's pseudoclient |
| `PRESENCE_UPDATE` | Update the corresponding pseudoclient's away status on IRC |
| `GUILD_MEMBER_ADD` | Introduce a new pseudoclient if the user is in a bridged channel |
| `GUILD_MEMBER_REMOVE` | Quit the corresponding pseudoclient from IRC |
| `MESSAGE_UPDATE` | Future: relay edits (out of scope for initial version) |
| `MESSAGE_DELETE` | Future: relay deletes (out of scope for initial version) |

All other events are ignored.

## Self-message filtering

`disirc` must not relay messages it sent itself. On `MESSAGE_CREATE`, skip the event if `author.id` matches:
- The bot's own user ID (from `READY`), **or**
- The user ID of any webhook owned by the bot in a bridged channel.

## Outgoing messages

- IRC → Discord messages are sent via webhook (preferred) or plain `channel.send()` (fallback).
- If the REST call fails, log at `WARN` and continue. Do not retry.

## Presence mapping

Discord presence is mapped to IRC away status for pseudoclients:

| Discord status | IRC away |
|---------------|----------|
| `online` | Not away (unset) |
| `idle` | `AWAY :idle` |
| `dnd` | `AWAY :do not disturb` |
| `offline` / `invisible` | `AWAY :offline` |
| _(unknown future variant)_ | `AWAY :offline` |

`OnlineStatus` is `#[non_exhaustive]` in serenity — any variant not listed above must be treated as offline.

## References

- [research/discord-irc-prior-art.md](../../research/discord-irc-prior-art.md) — webhook lifecycle, loop prevention, username constraints, @everyone suppression bug
- [research/discord-gateway-presences.md](../../research/discord-gateway-presences.md) — member list and presence snapshot timing; large-guild chunking behaviour
- [research/serenity-0.12-api.md](../../research/serenity-0.12-api.md) — serenity EventHandler API, OnlineStatus non_exhaustive, CreateAllowedMentions, webhook execution
- [Discord Webhooks documentation](https://discord.com/developers/docs/resources/webhook) — accessed 2026-03-22
- [Discord Gateway Intents](https://discord.com/developers/docs/topics/gateway#gateway-intents) — accessed 2026-03-22
