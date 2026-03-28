# Discord Gateway: Members and Presences at Startup

## Summary

Presence data is gateway-only — there is no REST endpoint to fetch current presences.
Both the `GUILD_MEMBERS` and `GUILD_PRESENCES` privileged intents must be enabled to
receive a full member list and presence snapshot in the initial `GUILD_CREATE` event.
The serenity cache exposes this as `Guild::members` and `Guild::presences`.

## Findings

### Presence availability

- **No REST endpoint** exists to fetch presence data. Presences are gateway-only.
- Without `GUILD_PRESENCES`, all users other than the bot appear as `offline` with no activities.
- With `GUILD_PRESENCES`, presence data is included in `GUILD_CREATE` and updated by
  `PRESENCE_UPDATE` events thereafter.

### `GUILD_CREATE` content

| Condition | Members array | Presences array |
|-----------|--------------|-----------------|
| Neither intent | Bot only | Empty |
| `GUILD_MEMBERS` only | Bot only (small guilds: all, but no presences) | Empty |
| `GUILD_PRESENCES` only | Online members only | Present |
| Both intents | Online + offline members | Present |

For **large guilds** (member\_count > `LARGE_THRESHOLD`, typically 250):
- With both intents: online members, members with roles/nicks, voice members in `GUILD_CREATE`;
  remaining members arrive via `GUILD_MEMBERS_CHUNK` events.
- Without `GUILD_PRESENCES`: Discord forces full chunking — only bot in `GUILD_CREATE`,
  everything else via `GUILD_MEMBERS_CHUNK`.

For **small guilds**: all members are included in `GUILD_CREATE` when both intents are active.

### serenity cache fields

- `Guild::members: HashMap<UserId, Member>` — member profiles (nick, roles, joined\_at, user)
- `Guild::presences: HashMap<UserId, Presence>` — online status + activities (empty without
  `GUILD_PRESENCES` intent)

### Startup sequence for disirc

1. `ready()` fires — cache is still empty at this point; `Ready::guilds` lists unavailable guilds.
2. `guild_create()` fires once per guild the bot is in, populating the cache.
3. With both privileged intents, `guild_create` delivers a `Guild` with `members` and `presences`
   already populated for online/all members depending on guild size.
4. `GUILD_MEMBERS_CHUNK` events may follow for large guilds; serenity merges them into the cache.
5. Subscribe to `PRESENCE_UPDATE` for real-time changes.

### REST member fetching (fallback)

`GuildId::members(ctx, limit, after)` fetches a page (up to 1000 members).
`GuildId::members_iter(ctx)` handles pagination automatically.
Rate-limited at ~2 requests/second per guild; use gateway chunking instead for startup.

### Privileged intent approval

Both `GUILD_MEMBERS` and `GUILD_PRESENCES` are privileged. Bots in fewer than 100 guilds
can enable them in the Developer Portal without review; larger bots require Discord approval.

## References

- [Discord Gateway — Guild Create](https://discord.com/developers/docs/events/gateway-events#guild-create) — accessed 2026-03-26
- [Discord Gateway Intents](https://discord.com/developers/docs/topics/gateway#gateway-intents) — accessed 2026-03-26
- [GUILD_CREATE doesn't include members without GUILD_PRESENCES intent (discord-api-docs #3968)](https://github.com/discord/discord-api-docs/issues/3968) — accessed 2026-03-26
- [serenity Guild struct](https://docs.rs/serenity/0.12.4/serenity/model/guild/struct.Guild.html) — accessed 2026-03-26
