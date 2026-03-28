# Pseudoclients

## What is a pseudoclient?

A pseudoclient is a virtual IRC user introduced by `disirc` under its own SID. To the IRC network, it is indistinguishable from a real connected user. Each Discord user active in at least one bridged channel has a corresponding pseudoclient.

## Identity mapping

| IRC field | Value |
|-----------|-------|
| Nick | Discord username, sanitized (see below) |
| Ident | `pseudoclients.ident` from config (default: `discord`) |
| Host | `<sanitized-username>.<pseudoclients.host_suffix>` |
| Realname (GECOS) | Discord display name (may differ from username) |
| UID | `<our_sid>` + 6 alphanumeric chars, unique and stable per Discord user ID for the session |
| Modes | `+i` (invisible) by default |

## Nick sanitization

Discord usernames may contain characters not valid in IRC nicks. Sanitization rules:

1. Replace any character not in `[A-Za-z0-9_\-\[\]\\^{}|`]` with `_`.
2. If the result starts with a digit, prefix with `d`.
3. Truncate to 30 characters.
4. If the sanitized nick collides with an existing IRC nick (known from the burst), apply the following fallback chain in order, stopping at the first non-colliding result:
   - Append `_` and retry, up to 3 times (e.g. `alice_`, `alice__`, `alice___`).
   - Truncate the sanitized nick as needed and append the last 8 hex digits of the Discord user ID (e.g. `alice_a1b2c3d4`). This resolves pseudoclient-vs-pseudoclient collisions in all but astronomically unlikely cases (1 in ~4 billion per pair).
   - **Final fallback**: use `d` + the 6-character unique portion of the pseudoclient's UID (i.e. the UID with the 3-character SID prefix stripped). Since UIDs are globally unique on the network by definition, this form is guaranteed collision-free. Example: UID `0D0ABCXYZ` → nick `dABCXYZ`.

The sanitized nick is stable for the session. If the Discord user changes their username, the pseudoclient is re-introduced with the new nick (QUIT + new UID).

## Lifecycle

### Introduction

A pseudoclient is introduced when:
- The IRC link burst begins and the Discord user is currently in a bridged channel, **or**
- A Discord user sends a message in a bridged channel and has no existing pseudoclient.

Introduction sequence:
```
:<our_sid> UID <nick> 1 <timestamp> <ident> <host> <uid> * +i 0 <realname> *
:<our_sid> SJOIN <timestamp> <#channel> + :<uid>
```

Multiple bridged channels: one `SJOIN` per channel.

### Unknown user events (large-guild chunking)

For large Discord guilds, `GUILD_CREATE` delivers only online and role-bearing members;
the remaining (offline) members arrive via `GUILD_MEMBERS_CHUNK` events that serenity
merges into its cache. `disirc` does not process `GUILD_MEMBERS_CHUNK` directly, so
these offline members have no pseudoclient at startup.

Events received for a user with no existing pseudoclient are handled as follows:

| Event | Behaviour |
|---|---|
| `DiscordEvent::MessageReceived` | Introduce pseudoclient on demand (see Introduction above), then relay the message. |
| `DiscordEvent::PresenceUpdated` | Silently drop. No pseudoclient exists to update; when the user next sends a message they will be introduced and subsequent presence updates will apply normally. |
| `DiscordEvent::MemberRemoved` | Silently drop. No pseudoclient exists to quit. |

This lazy introduction strategy avoids introducing pseudoclients for offline members who
may never become active in a bridged channel.

### Quit

A pseudoclient is quit when:
- The Discord user leaves the guild, **or**
- The Discord user goes offline (optional — see presence policy below).

Quit message:
```
:<uid> QUIT :Disconnected from Discord
```

### Presence policy

By default, pseudoclients persist while the user is a guild member, regardless of online status. Away status (see `03-discord-connection.md`) reflects presence without quitting. This avoids noisy join/quit spam for users who frequently go offline.

If a future config option `pseudoclients.quit_on_offline = true` is set, pseudoclients are quit when the user goes offline and re-introduced when they come back online.

## Channel membership

- A pseudoclient joins all bridged IRC channels it is a member of when introduced.
- A pseudoclient does not join non-bridged channels.

### Runtime channel addition

When a new `[[bridge]]` entry is added via config reload:

1. Fetch the current Discord member list for the new channel.
2. For each member, look up or create a pseudoclient (introduction sequence as normal if new).
3. Send `SJOIN` for every existing pseudoclient that is a member of the new Discord channel.
4. Begin relaying messages for the new channel pair.

### Runtime channel removal

When a `[[bridge]]` entry is removed via config reload:

1. For each pseudoclient that was in the removed channel:
   - If the pseudoclient has no remaining bridged channels: send `QUIT :Bridge channel removed`.
   - Otherwise: send `PART <#irc_channel> :Bridge channel removed`.
2. Stop relaying messages for the removed channel pair.

## State tracking

`disirc` maintains an in-memory map of:
- `discord_user_id → PseudoclientState { uid, nick, channels }`
- `irc_nick → discord_user_id` (for reverse lookup)
- `irc_uid → discord_user_id`

This state is rebuilt from scratch on every reconnect (IRC burst + Discord member fetch).

## SVSNICK handling

If the IRC network sends `SVSNICK` targeting one of our pseudoclients (e.g., a services bot forcing a nick change), `disirc` applies the new nick and updates its internal state. It does not attempt to restore the original nick.

## Concurrency and ownership

`PseudoclientManager` is **not thread-safe by design**. It uses plain
`HashMap` fields and takes `&mut self` on every mutating operation. No
`Arc`, `Mutex`, or `RwLock` is used or intended.

Thread safety is provided at the architecture level rather than inside the
type: all IRC and Discord events must be funnelled through
`tokio::sync::mpsc` channels to a **single processing task** that owns the
manager exclusively. No spawned subtask may hold a reference to the manager.

```
IRC reader task         Discord gateway task
      │                        │
      │ IrcMessage              │ DiscordEvent
      ▼                        ▼
 mpsc::Sender ────────► mpsc::Receiver
                                │
                      single processing task
                      (sole owner of PseudoclientManager)
```

This is the actor model: `PseudoclientManager` is private state inside an
actor; the channel is its mailbox. Events queue in the channel and are
processed one at a time, so the manager never needs locking.

**Constraint for spec-02 (IRC connection) and spec-03 (Discord connection)**:
the reader/gateway tasks must communicate exclusively via the mpsc channel.
They must not accept a shared reference to `PseudoclientManager`.
