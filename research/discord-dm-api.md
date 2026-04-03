# Discord Bot DM API

## Summary

Discord bots can send and receive DMs. Sending requires creating a DM channel via `POST /users/@me/channels` with `recipient_id`, then sending a message to that channel. Receiving requires the `DIRECT_MESSAGES` gateway intent. DMs do not support webhooks or threads, but do support message replies (references). The `MESSAGE_CONTENT` privileged intent is NOT required for DMs — bots always receive full content in DMs.

## Findings

### Sending DMs

- `POST /users/@me/channels` with `{ recipient_id: "user_id" }` creates (or returns existing) DM channel
- Then `POST /channels/{dm_channel_id}/messages` sends the message
- No guild membership requirement — bot can DM any user whose ID it knows
- Rate limited: rapid DM opening causes blocking; mass DMing violates Discord ToS
- DMs should be user-initiated or response-driven per Discord's guidelines

### Receiving DMs

- `MESSAGE_CREATE` gateway events fire for DMs, same as guild messages
- Requires `DIRECT_MESSAGES` intent (not privileged, bit 12 = 4096)
- DM messages lack `guild_id` and `member` fields (no guild context)
- `MESSAGE_CONTENT` privileged intent is NOT required for DMs — bots always get full content

### DM vs guild message detection

- Check `guild_id` field: absent/null for DMs
- Channel type: `1` (DM) vs `0` (GUILD_TEXT)

### No webhooks in DMs

- Webhooks are guild-channel-only; cannot be used in DM channels
- IRC users cannot appear as distinct senders in DMs — always the bridge bot

### No threads in DMs

- DM channels (type 1) do not support thread creation
- Group DMs (type 3) also do not support threads
- Threads are restricted to guild channels (GUILD_TEXT, GUILD_ANNOUNCEMENT, GUILD_FORUM, GUILD_MEDIA)
- No planned support on Discord's public roadmap

### Message replies in DMs

- DMs DO support message replies (`message_reference` field)
- When a user replies to a message, the reply includes `message_reference.message_id`
- The bot can look up the referenced message to determine conversational context
- This enables a natural back-and-forth UX without explicit addressing

### Bot-to-bot DMs

Bots **cannot** DM other bots. The API returns error 50007 "Cannot send messages to this user". This is an undocumented Discord platform restriction — the API docs don't mention it, but it's confirmed by multiple community reports. This means automated e2e testing of DM bridging requires a human Discord user; two bot accounts cannot test the DM path.

### Privacy and blocking

- Users can block bot DMs via:
  - Global setting: Settings > Privacy & Safety > "Allow direct messages from server members"
  - Per-server toggle in server privacy settings
  - Per-user blocking (right-click > Block)
- However: bots can bypass the "Allow DMs from server members" toggle — this is a known Discord issue
- No explicit consent mechanism for bot DMs
- Bridge should make DM bridging opt-in to respect user expectations

### Rate limits

- 50 requests/second globally across all routes
- Opening many new DM channels rapidly causes rate limiting/blocking
- Parse rate limit response headers rather than hardcoding limits

## References

- [Discord Create DM endpoint](https://discord.com/developers/docs/resources/user#create-dm) — accessed 2026-04-02
- [Discord Channel Types](https://discord.com/developers/docs/resources/channel#channel-object-channel-types) — accessed 2026-04-02
- [Discord Gateway Intents](https://discord.com/developers/docs/events/gateway#gateway-intents) — accessed 2026-04-02
- [Discord Rate Limits](https://discord.com/developers/docs/topics/rate-limits) — accessed 2026-04-02
- [Message Content Privileged Intent FAQ](https://support-dev.discord.com/hc/en-us/articles/4404772028055-Message-Content-Privileged-Intent-FAQ) — accessed 2026-04-02
- [Discord Blocking & Privacy Settings](https://support.discord.com/hc/en-us/articles/217916488-Blocking-Privacy-Settings) — accessed 2026-04-02
- [Threads in DMs feature request](https://support.discord.com/hc/en-us/community/posts/11149356521111-Threads-in-DMS) — accessed 2026-04-02
- [Bot-to-bot DM limitation (corde)](https://github.com/cordejs/corde/discussions/1071) — accessed 2026-04-02
- [Bot-to-bot DM limitation (discord.js)](https://github.com/discordjs/discord.js/issues/4112) — accessed 2026-04-02
