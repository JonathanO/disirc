# Pseudoclients

## What is a pseudoclient?

A pseudoclient is a virtual IRC user introduced by `disirc` under its own SID. To the IRC network, it is indistinguishable from a real connected user. Each Discord user active in at least one bridged channel has a corresponding pseudoclient.

## Identity mapping

| IRC field | Value |
|-----------|-------|
| Nick | Discord username, sanitized (see below) |
| Ident | `pseudoclients.ident` from config (default: `discord`) |
| Host | `<discord_user_id>.discord.com` |
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

### State vs IRC commands

Discord events **always** update `PseudoclientManager` state, regardless of
whether the IRC link is connected.  IRC commands (UID, SJOIN, AWAY, QUIT) are
only emitted when the link is `Ready`.

This means pseudoclients exist in memory before they appear on IRC.  When the
IRC link becomes ready (BurstComplete), all existing pseudoclients are burst to
IRC in a single batch.

### Introduction

A pseudoclient is created in `PseudoclientManager` when:
- A `MemberSnapshot` (from `GUILD_CREATE`) includes a **non-offline** member, **or**
- A `PRESENCE_UPDATE` arrives for a user with no pseudoclient and non-offline status, **or**
- A Discord user sends a message in a bridged channel and has no pseudoclient.

If the IRC link is `Ready`, the introduction is immediately sent to IRC:
```
:<our_sid> UID <nick> 1 <timestamp> <ident> <host> <uid> * +i 0 <realname> *
:<our_sid> SJOIN <timestamp> <#channel> + :<uid>
```

Multiple bridged channels: one `SJOIN` per channel.

If the IRC link is **not** ready, the pseudoclient is created in memory only.
It will be sent to IRC when the link becomes ready (see Burst below).

### Burst on LinkUp

When the IRC S2S handshake completes (LinkUp), the bridge sends its burst
and goes live immediately:

1. Walk all pseudoclients in `PseudoclientManager`.
2. For each pseudoclient, emit `UID` + `SJOIN` commands.
3. Emit `AWAY` for pseudoclients with Idle, DnD, or Offline presence.
4. Send our `EOS`.

Both sides burst concurrently — we don't wait for the remote burst.  Nick
collisions with external nicks are handled by the KILL handler — if
UnrealIRCd kills a colliding pseudoclient, it is reintroduced with a
suffixed nick.

### Messages before IRC ready

Discord messages arriving before the IRC link is ready are **dropped**.  The
pseudoclient is created (or already exists) in memory, but the message text is
not relayed.  This is analogous to IRC netsplit behaviour where messages during
a split are not delivered to the other side.

### Unknown user events (large-guild chunking)

For large Discord guilds, `GUILD_CREATE` delivers only online and role-bearing members;
the remaining (offline) members arrive via `GUILD_MEMBERS_CHUNK` events that serenity
merges into its cache. `disirc` does not process `GUILD_MEMBERS_CHUNK` directly, so
these offline members have no pseudoclient at startup.

Events received for a user with no existing pseudoclient are handled as follows:

| Event | Behaviour |
|---|---|
| `DiscordEvent::MessageReceived` | Introduce pseudoclient on demand, then relay the message (if IRC link is Ready; otherwise message is dropped). |
| `DiscordEvent::PresenceUpdated` with non-offline status | Create pseudoclient and store presence. Emit IRC commands if link is Ready. |
| `DiscordEvent::PresenceUpdated` with offline status | Silently drop. The user was never introduced; there is nothing to update or quit. |
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

Pseudoclients use **lazy channel membership**: they are introduced to
the IRC network without joining any channels.  A pseudoclient joins a
bridged IRC channel only when the Discord user first sends a message in
the corresponding Discord channel.

This avoids populating IRC channel user lists with Discord users who
never participate in that channel.

The **first** pseudoclient introduced from a `MemberSnapshot` (typically
the bridge bot) joins all bridged channels eagerly.  This ensures the
bridge server has at least one user in each channel, which is required
for IRC S2S message routing — UnrealIRCd only forwards PRIVMSGs to
servers that have users in the channel.

### On-demand JOIN

When a Discord message arrives in a bridged channel and the
pseudoclient exists but is not in the corresponding IRC channel:

1. Emit `SJOIN` to join the pseudoclient to the channel.
2. Update `PseudoclientState.channels` to include the new channel.
3. Relay the message as normal.

### Burst on reconnect

On IRC reconnect, pseudoclients are re-burst with their current
`channels` list — only channels they previously joined.  Membership
persists in memory across reconnects but is lost on bridge restart.

### Runtime channel addition

When a new `[[bridge]]` entry is added via config reload:

1. Begin relaying messages for the new channel pair.
2. Pseudoclients join the new channel lazily when they speak in it.

### Runtime channel removal

When a `[[bridge]]` entry is removed via config reload:

1. For each pseudoclient that was in the removed channel:
   - If the pseudoclient has no remaining bridged channels: send `QUIT :Bridge channel removed`.
   - Otherwise: send `PART <#irc_channel> :Bridge channel removed`.
2. Stop relaying messages for the removed channel pair.

## State tracking

`disirc` maintains an in-memory map of:
- `discord_user_id → PseudoclientState { uid, nick, username, display_name, channels, presence }`
- `irc_nick → discord_user_id` (for reverse lookup)
- `irc_uid → discord_user_id`

Pseudoclient state persists across IRC reconnects.  On reconnect, all existing
pseudoclients are re-burst to the new link.  State is only cleared when a
pseudoclient is explicitly quit (member removal, offline with quit-on-offline).

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
