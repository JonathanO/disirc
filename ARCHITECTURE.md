# Architecture

Runtime component interaction, event flows, and state lifecycle.

For module-level source layout see [LAYOUT.md](LAYOUT.md).
For high-level design goals and protocol layering see [specs/00-overview](specs/00-overview/spec.md).

## Components

```
                        ┌─────────────────────────────┐
                        │       main.rs                │
                        │  load config, spawn tasks    │
                        └──────┬──────────────────┬────┘
                               │                  │
                 ┌─────────────▼──────┐   ┌───────▼─────────────┐
                 │  IRC connection    │   │  Discord connection  │
                 │  (src/irc/)        │   │  (src/discord/)      │
                 │                    │   │                      │
                 │  S2S handshake     │   │  Gateway + REST      │
                 │  line framing      │   │  webhook send        │
                 │  translate in/out  │   │  event handler       │
                 └────┬──────────▲────┘   └────┬──────────▲──────┘
                      │          │              │          │
                S2SEvent    S2SCommand    DiscordEvent  DiscordCommand
                      │          │              │          │
                 ┌────▼──────────┴──────────────▼──────────┴──────┐
                 │              Bridge loop (src/bridge/mod.rs)    │
                 │  async select! dispatcher — no logic, just     │
                 │  forwards events to BridgeState and sends      │
                 │  resulting commands to the connection tasks     │
                 │                                                │
                 │  ┌──────────────────────────────────────────┐  │
                 │  │  BridgeState (src/bridge/orchestrator.rs) │  │
                 │  │                                          │  │
                 │  │  Owns:                                   │  │
                 │  │  - PseudoclientManager                   │  │
                 │  │  - IrcState (uid/nick maps, chan TS)     │  │
                 │  │  - DiscordState (display names, guilds)  │  │
                 │  │  - LinkPhase (Down/Bursting/Ready)       │  │
                 │  │  - Deferred Discord event buffer         │  │
                 │  │  - Kill cooldown map                     │  │
                 │  │                                          │  │
                 │  │  Synchronous handlers:                   │  │
                 │  │  - handle_irc_event() -> HandlerOutput   │  │
                 │  │  - handle_discord_event() -> HandlerOutput│  │
                 │  │  - reload_config()                       │  │
                 │  └──────────────────────────────────────────┘  │
                 └────────────────────────────────────────────────┘
```

All mutable state lives in `BridgeState`. The bridge loop and connection tasks
are stateless dispatchers. This makes the core logic synchronous and
deterministically testable.

## Link lifecycle (LinkPhase state machine)

```
                  BurstComplete
  NotReady ──────────────────────► Ready
      ▲                              │
      │           LinkDown           │
      └──────────────────────────────┘
```

On `LinkUp`, the bridge sends its burst and goes live immediately (all
pseudoclients + EOS, phase = `Ready`).  Both sides burst concurrently —
we don't wait for the remote burst.  `BurstComplete` (remote EOS) is a
no-op; the bridge is already live.

| Phase | IRC events | Discord events | Our burst |
|-------|-----------|----------------|-----------|
| **NotReady** | Remote burst registers external nicks | State always updated (PM, DiscordState); IRC commands **not** emitted; messages dropped | Sent on LinkUp |
| **Ready** | Processed normally (messages routed, state updated) | State updated **and** IRC commands emitted immediately | Already sent |

Discord events are never buffered. They always update `PseudoclientManager`
and `DiscordState` immediately. The difference between phases is only whether
IRC commands are emitted.

## First connect flow

```
1. Discord GUILD_CREATE → MemberSnapshot → pseudoclients created in PM
2. IRC handshake completes
3. LinkUp → our burst sent immediately:
   produce_burst_commands() — UIDs + SJOINs + AWAY + EOS
4. Remote burst: UIDs, SJOINs → external nicks registered
5. Remote EOS → BurstComplete → phase = Ready
6. Bridge is live — messages relay bidirectionally
```

Note: steps 1-2 may occur in either order. Discord events update PM state
regardless, and the burst on LinkUp sends whatever pseudoclients exist at
that point.

## Reconnect flow (IRC link drops, Discord stays connected)

```
1. LinkDown → phase = NotReady
   - External nicks cleared
   - Pseudoclients remain in PseudoclientManager
2. Discord events continue to update PM state normally
   (new members introduced, presence changes recorded, messages dropped)
3. IRC reconnects → handshake completes
4. LinkUp → our burst sent immediately:
   produce_burst_commands() — re-introduces all existing pseudoclients
   (UIDs + SJOINs + AWAY + EOS)
5. Remote burst arrives → external nicks re-registered
6. Remote EOS → BurstComplete → phase = Ready
7. If a nick collision occurs (external user took a pseudoclient's nick),
   UnrealIRCd sends KILL → KILL handler re-introduces with a fresh UID
```

## Discord event flow

```
Discord Gateway
    │
    ├─ MESSAGE_CREATE (bridged channel)
    │   → DiscordEvent::MessageReceived
    │   → route_discord_to_irc() → S2SCommand::SendMessage
    │     (on-demand pseudoclient introduction if needed)
    │
    ├─ GUILD_CREATE
    │   → DiscordEvent::MemberSnapshot (all members with presence)
    │   → Cache display_names for all members
    │   → introduce_pseudoclient() for each online member
    │
    ├─ PRESENCE_UPDATE
    │   → DiscordEvent::PresenceUpdated (with display_name from payload)
    │   → If already introduced: update AWAY status
    │   → If not introduced + non-offline: introduce pseudoclient
    │   → If not introduced + offline: skip (no pseudoclient needed)
    │
    ├─ GUILD_MEMBER_ADDITION
    │   → DiscordEvent::MemberAdded → cache display_name
    │
    ├─ GUILD_MEMBER_REMOVAL
    │   → DiscordEvent::MemberRemoved → QUIT pseudoclient
    │
    └─ MESSAGE_CREATE (DM, if dm_bridging enabled)
        → DiscordEvent::DmReceived → route_dm_to_irc()
```

## IRC event flow

```
IRC S2S link
    │
    ├─ PRIVMSG #channel
    │   → S2SEvent::MessageReceived
    │   → route_irc_to_discord() → DiscordCommand::SendMessage
    │     (webhook path if configured, plain path otherwise)
    │
    ├─ NOTICE #channel
    │   → S2SEvent::NoticeReceived
    │   → route_irc_to_discord() (is_notice=true, italicised on Discord)
    │
    ├─ PRIVMSG <pseudoclient-uid> (DM, if dm_bridging enabled)
    │   → route_irc_to_dm() → DiscordCommand::SendDm
    │
    ├─ UID (user introduced)
    │   → S2SEvent::UserIntroduced → register external nick
    │
    ├─ QUIT / NICK / KILL
    │   → S2SEvent::UserQuit / UserNickChanged / UserKilled
    │   → Update nick maps; KILL handler may reintroduce pseudoclient
    │
    ├─ SJOIN (channel burst)
    │   → S2SEvent::ChannelBurst → record channel timestamp
    │
    └─ EOS
        → S2SEvent::BurstComplete → trigger our burst + deferred replay
```

## Pseudoclient identity

Each Discord user gets one IRC pseudoclient with:

| Field | Value |
|-------|-------|
| Nick | Display name, sanitised for IRC (collision-suffixed if needed) |
| Ident | Static from config (default: `discord`) |
| Host | `{discord_user_id}.discord.com` |
| Realname (GECOS) | Discord display name |
| UID | `{our_sid}` + 6 alphanumeric chars, stable per Discord user for the session |

## Message paths (IRC → Discord)

```
                      ┌─────────────────┐
                      │ IRC PRIVMSG     │
                      │ from external   │
                      │ user            │
                      └────────┬────────┘
                               │
                      ┌────────▼────────┐
                      │ route_irc_to_   │
                      │ discord()       │
                      │ - loop filter   │
                      │ - nick→mention  │
                      └───┬─────────┬───┘
                          │         │
              has webhook │         │ no webhook
                          │         │
                 ┌────────▼───┐ ┌───▼────────────┐
                 │ Webhook    │ │ Plain send     │
                 │ - username │ │ - **[nick]**   │
                 │ = IRC nick │ │   prefix       │
                 │ - avatar   │ │ - suppress @   │
                 └────────────┘ └────────────────┘
```

## Message paths (Discord → IRC)

```
                      ┌─────────────────┐
                      │ Discord message │
                      │ in bridged chan │
                      └────────┬────────┘
                               │
                      ┌────────▼────────┐
                      │ route_discord_  │
                      │ to_irc()        │
                      │ - loop filter   │
                      │ - on-demand     │
                      │   pseudoclient  │
                      │   introduction  │
                      └────────┬────────┘
                               │
                      ┌────────▼────────┐
                      │ discord_to_irc_ │
                      │ commands()      │
                      │ - markdown→IRC  │
                      │ - mention resolve│
                      │ - line splitting │
                      │ - attachments   │
                      └────────┬────────┘
                               │
                      ┌────────▼────────┐
                      │ PRIVMSG from    │
                      │ pseudoclient UID│
                      │ to #channel     │
                      └─────────────────┘
```
