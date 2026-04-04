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

LinkUp is a no-op — the bridge starts `NotReady` and stays there until
`BurstComplete`. This avoids a redundant intermediate state.

| Phase | IRC events | Discord events | Our burst |
|-------|-----------|----------------|-----------|
| **NotReady** | Remote burst registers external nicks; LinkUp is a no-op | Buffered in `deferred_discord_events` | Not sent |
| **Ready** | Processed normally (messages routed, state updated) | Processed immediately | Sent on entry (existing pseudoclients + deferred replay + EOS) |

## First connect flow

```
1. IRC handshake completes
2. LinkUp (no-op — already NotReady)
3. Discord GUILD_CREATE → MemberSnapshot → buffered
4. Remote burst: UIDs, SJOINs → external nicks registered in PseudoclientManager
5. Remote EOS → BurstComplete → phase = Ready
6. Our burst sent:
   a. produce_burst_commands() — empty on first connect (no pseudoclients yet)
   b. Replay deferred MemberSnapshot → introduce_pseudoclient for each online member
      → UID + SJOIN commands for each pseudoclient
   c. Our EOS
7. Bridge is live — messages relay bidirectionally
```

## Reconnect flow (IRC link drops, Discord stays connected)

```
1. LinkDown → phase = NotReady
   - Deferred events cleared
   - External nicks cleared
   - Pseudoclients remain in PseudoclientManager (not quit)
2. New Discord events arrive → buffered (phase is NotReady)
3. IRC reconnects → handshake completes
4. LinkUp (no-op — already NotReady)
5. Remote burst arrives → external nicks re-registered
6. Remote EOS → BurstComplete → phase = Ready
7. Our burst sent:
   a. produce_burst_commands() — re-introduces all existing pseudoclients
      (UIDs + SJOINs for each, using stored state)
   b. Replay any deferred Discord events (may introduce new members)
   c. Our EOS
8. If a nick collision occurs (external user took a pseudoclient's nick),
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
