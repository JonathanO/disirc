# TODO — spec/03-discord-connection

Status: **Pending**

- [ ] Gateway connection and READY event (record bot user ID, fetch webhook user IDs)
- [ ] Member list and presence fetch on startup for all bridged channels
- [ ] `MESSAGE_CREATE` routing and self-message filtering (bot ID + webhook user IDs)
- [ ] `PRESENCE_UPDATE` → pseudoclient `AWAY` status
- [ ] `GUILD_MEMBER_ADD` / `GUILD_MEMBER_REMOVE` → pseudoclient introduce/quit
- [ ] Webhook management (create per channel if missing, cache and reuse, fallback to plain send)
- [ ] Config reload: fetch members + presence for new channel; register new webhook user ID
