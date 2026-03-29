# Source layout

This file describes every module in `src/` and what belongs in each one.
Update it whenever a module is added, removed, or significantly refactored.

---

## Top-level modules (`src/`)

| File / dir | What it contains |
|------------|-----------------|
| `src/lib.rs` | Crate root — declares all public modules; `#![deny(unsafe_code)]`. |
| `src/main.rs` | Binary entry point — loads config, spawns the IRC connection task and Discord connection task via `tokio::spawn`, then runs the bridge loop on the main thread. The three components communicate via `tokio::sync::mpsc` channels. Uses `tracing-subscriber` with `RUST_LOG` env-filter. |
| `src/config.rs` | Configuration file format (`Config`, `IrcConfig`, `BridgeEntry`, etc.) and validation. Read from `config.toml` at startup. Hot-reload support via `reload()` → `BridgeDiff`. |
| `src/formatting/` | Bidirectional text transforms: Discord markdown ↔ IRC formatting codes, mention/emoji expansion, message splitting, truncation. No I/O. See below. |
| `src/pseudoclients.rs` | Pseudoclient lifecycle and identity. Tracks Discord users as fake IRC clients; generates UID allocations, nick sanitisation, and builds the UnrealIRCd wire messages (UID/SJOIN/QUIT/PART) to introduce or remove them. |
| `src/bridge/` | **Bridge processing layer.** Channel routing, state management, message relay, and the main event loop. No direct I/O; takes and returns protocol-agnostic types (`S2SCommand`, `DiscordCommand`). See below. |
| `src/signal.rs` | OS signal handling (SIGTERM / Ctrl-C). `spawn_signal_handler()` returns an `mpsc::Receiver<ControlEvent>` that the bridge loop can `select!` on. |
| `src/irc/` | IRC abstraction layer — see below. |
| `src/discord/` | Discord abstraction layer — see below. |

---

## `src/formatting/` — Bidirectional text transforms

Discord markdown ↔ IRC formatting codes, mention/emoji expansion, message
splitting, and truncation. No I/O. Split by direction with shared constants
in `mod.rs`.

| File | What it contains |
|------|-----------------|
| `src/formatting/mod.rs` | Shared IRC control character constants (`BOLD`, `ITALIC`, etc.), `format_server_time`, public trait definitions (`DiscordResolver`, `IrcMentionResolver`), re-exports of both submodules' public API, and cross-direction roundtrip property tests. |
| `src/formatting/discord_to_irc.rs` | Discord→IRC transforms: `discord_to_irc` (top-level entry point), `resolve_mentions`, `markdown_to_irc`, `split_for_irc`. Private helpers for escape sentinels, code span protection, word boundary underscores, and paired marker conversion. All Discord→IRC tests and property tests. |
| `src/formatting/irc_to_discord.rs` | IRC→Discord transforms: `irc_to_discord_formatting`, `convert_irc_mentions`, `ping_fix_nick`, `truncate_for_discord`, `irc_to_discord_webhook`, `irc_to_discord_plain`. All IRC→Discord tests and property tests. |

---

## `src/bridge/` — Bridge processing layer

Channel routing, state management, message relay, burst generation, and the
main event loop. No direct I/O; operates on protocol-agnostic types
(`S2SCommand`, `DiscordCommand`, `S2SEvent`, `DiscordEvent`).

| File | What it contains |
|------|-----------------|
| `src/bridge/mod.rs` | Re-exports all public types and functions. Contains `NoopIrcResolver` / `NoopDiscordResolver` (used only by `run_bridge`), `unix_now`, and the `run_bridge` async event loop which owns `PseudoclientManager`, `IrcState`, and `DiscordState`. |
| `src/bridge/map.rs` | `BridgeInfo` (immutable snapshot of one bridge entry) and `BridgeMap` (bidirectional O(1) channel routing table built from config). |
| `src/bridge/relay.rs` | Message format conversion: `discord_to_irc_commands` (Discord message → Vec of `S2SCommand::SendMessage`), `irc_to_discord_command` (IRC PRIVMSG/NOTICE/ACTION → `DiscordCommand::SendMessage`). Private `extract_action` helper for CTCP ACTION parsing. |
| `src/bridge/state.rs` | `IrcState` (uid→nick map + channel→timestamp cache), `DiscordState` (display_name cache + guild→irc-channel map), `apply_irc_event` (updates IrcState + PseudoclientManager from S2S events), `apply_discord_event` (updates DiscordState + PseudoclientManager from Discord events, returns S2S commands to emit). Private `introduce_pseudoclient` helper. |
| `src/bridge/routing.rs` | `produce_burst_commands` (generates IntroduceUser+JoinChannel+BurstComplete for all pseudoclients), `route_irc_to_discord` (IRC→Discord message routing with loop prevention), `route_discord_to_irc` (Discord→IRC routing with on-demand pseudoclient introduction), `update_guild_irc_channels` (populates guild→irc-channel map from bridge config). |

---

## `src/irc/` — IRC abstraction layer

Holds protocol-agnostic types shared by the rest of the application, plus a
submodule for each concrete IRC server dialect.

| File | What it contains |
|------|-----------------|
| `src/irc/mod.rs` | Re-exports `S2SEvent`, `S2SCommand`, `MemberPrefix` from `types.rs`. Declares the `unreal` submodule. |
| `src/irc/types.rs` | **Protocol-agnostic boundary types.** `S2SEvent` — events emitted from the connection layer to the processing task (link up/down, users, channels, messages). `S2SCommand` — commands sent from the processing task to the connection layer. `MemberPrefix` — channel member privilege levels. Nothing in these types is UnrealIRCd-specific; they could be adapted for any S2S IRC dialect. |

---

## `src/irc/unreal/` — UnrealIRCd S2S implementation

All code in this submodule is specific to the UnrealIRCd server-to-server
protocol. The rest of the application communicates with it only through the
`S2SEvent` / `S2SCommand` boundary defined in `src/irc/types.rs`.

| File | What it contains |
|------|-----------------|
| `src/irc/unreal/mod.rs` | Re-exports `run_connection` (the public entry point) and the four public wire types (`IrcMessage`, `IrcCommand`, `UidParams`, `SjoinParams`) for use by `pseudoclients.rs`. Declares all private submodules. |
| `src/irc/unreal/irc_message.rs` | **Wire type definitions.** `IrcMessage` (tags + prefix + command), `IrcCommand` enum covering all commands used in the handshake and session (PASS, SERVER, SID, UID, SJOIN, PRIVMSG, PING, PONG, …), `UidParams`, `SjoinParams`. Parsing (`IrcMessage::parse`) and serialisation (`IrcMessage::to_wire`). |
| `src/irc/unreal/framing.rs` | `LineReader<R>` / `LineWriter<W>` — generic async line framing over any `AsyncRead`/`AsyncWrite`. Strips `\r\n`, enforces the 4096-byte line limit, replaces invalid UTF-8. Used by the connection layer to turn a raw byte stream into `IrcMessage` values. |
| `src/irc/unreal/connect.rs` | TCP/TLS connection factory: `connect(host, port, tls)` returns a `(IrcReader, IrcWriter)` pair. Uses `tokio-rustls` with a permissive `ServerCertVerifier` (`AcceptAnyCert`) because IRC uplinks commonly use self-signed certificates; the link password is the actual authentication mechanism. |
| `src/irc/unreal/connection.rs` | **Main connection loop.** `run_connection` — never returns; handles connect → handshake → session → reconnect with full-jitter exponential backoff. `do_handshake` — sends credentials, reads uplink introduction, records `uplink_sid` and MTAGS capability. `run_session` — `tokio::select!` loop: inbound lines → `S2SEvent`, outbound `S2SCommand` → rate-limited wire writes, keepalive PING/PONG. |
| `src/irc/unreal/rate_limiter.rs` | `TokenBucket` — continuous token-bucket rate limiter. Capacity 10, refill rate 1 token per 500 ms. Used by `run_session` to prevent flooding the uplink. |
| `src/irc/unreal/translation.rs` | `translate_inbound(IrcMessage) → Option<S2SEvent>` and `translate_outbound(S2SCommand, …) → Vec<IrcMessage>`. The only place where UnrealIRCd wire types are converted to/from the protocol-agnostic boundary. |

---

## `src/discord/` — Discord abstraction layer

Gateway event handling, webhook-based message sending, and the Discord-side
event/command type definitions. The rest of the application communicates with
this layer through `DiscordEvent` / `DiscordCommand` boundary types.

### Architecture

Serenity types are confined to this module — they never leak into the bridge
layer. All inbound Discord events are converted to `DiscordEvent` at the
boundary; all outbound commands flow through `DiscordCommand`.

```
┌───────────────────────────────────────────────────────────┐
│                  SERENITY BOUNDARY                        │
│  Gateway Events                HTTP Client + Webhook APIs │
│  (Ready, Message, Presence)    (execute, send_message)    │
└──────────┬─────────────────────────────┬─────────────────┘
           │                             │
      ┌────▼────────────┐      ┌─────────▼──────────┐
      │  EventHandler    │      │ process_commands   │
      │  (thin shim)     │      │  (async loop)      │
      │  [mutants::skip] │      │  [mutants::skip]   │
      └────┬────────────┘      └─────────┬──────────┘
           │                             │
      ┌────▼─────────────────────────┐   │
      │  Pure helper functions       │   │
      │  - map_online_status()       │   │
      │  - resolve_display_name()    │   │
      │  - should_relay_message()    │   │
      │  - build_member_snapshot()   │   │
      │  [unit tested]               │   │
      │                              │   │
      │  send_discord_message()      │◄──┘
      │  - sanitize_webhook_username │
      │  - suppress_mentions()       │
      │  [unit tested]               │
      └────┬─────────────────────────┘
           │
      ┌────▼─────────────────────────────┐
      │  mpsc channels                   │
      │  DiscordEvent / DiscordCommand   │
      │  (no serenity types cross here)  │
      └────┬─────────────────────────────┘
           │
      ┌────▼─────────────────────────────┐
      │  Bridge loop                     │
      │  (independent of serenity)       │
      └─────────────────────────────────┘
```

### Outbound message paths

Two send paths with automatic fallback:

- **Webhook path** (preferred): parses webhook URL via `webhook_id_from_url()`,
  calls `Webhook::from_url` → `webhook.execute()`. Username sanitised to 2–32
  chars. Mentions suppressed via `CreateAllowedMentions::new()`.
- **Plain path** (fallback when no webhook configured): uses
  `ChannelId::send_message()`. Nick prepended as `**[nick]**`. Both nick and
  text pass through `suppress_mentions()` to inject zero-width spaces after `@`.

### Testing seams

The architecture is designed for testability without a live Discord connection:

- **Pure functions** (`map_online_status`, `resolve_display_name`,
  `should_relay_message`, `build_member_snapshot_event`, `sanitize_webhook_username`,
  `suppress_mentions`, `apply_reload`) — tested directly, no serenity dependency.
- **Handler methods** — `DiscordHandler` can be constructed with a mock
  `mpsc::Sender<DiscordEvent>` in tests (see `make_handler()` test helper in
  `handler.rs`). Handler async methods take primitive parameters.
- **Webhook/REST layer** — serenity's `HttpBuilder::proxy()` can redirect REST
  calls to a `wiremock` mock server for integration tests.
- **Gateway** — not practically mockable; the `EventHandler` shim is thin enough
  that this is acceptable. The handler trait impl is `#[mutants::skip]`.

### Files

| File | What it contains |
|------|-----------------|
| `src/discord/mod.rs` | Re-exports `DiscordCommand`, `DiscordEvent`, `DiscordPresence`, `MemberInfo`, `webhook_id_from_url`. Declares `connection`, `handler`, `send`, and `types` submodules. |
| `src/discord/types.rs` | **Boundary types.** `DiscordEvent` — events emitted from the Discord handler to the bridge loop (messages, member snapshots, presence updates, member add/remove). `DiscordCommand` — commands sent from the bridge loop to the Discord send task (send message, reload bridges). `DiscordPresence` — online/idle/dnd/offline enum. `MemberInfo` — per-member snapshot data. `webhook_id_from_url` — extracts the webhook ID from a Discord webhook URL. |
| `src/discord/connection.rs` | `run_discord` — the public entry point. Creates the serenity `Client` with the gateway handler, spawns the webhook send task, and runs the gateway event loop. Never returns. Manages `self_filter` (webhook ID set for loop prevention) and `channel_ids` (bridged channel set). |
| `src/discord/handler.rs` | `DiscordHandler` — implements serenity's `EventHandler` trait. Converts gateway events (`message`, `guild_create`, `guild_member_addition`, `guild_member_removal`, `presence_update`) into `DiscordEvent` values sent to the bridge loop via `mpsc`. Builds `MemberSnapshot` events from guild data. Pure helper functions are extracted and unit tested separately. |
| `src/discord/send.rs` | `send_discord_message` — executes a single `DiscordCommand::SendMessage` via webhook or plain channel send. `process_discord_commands` — async loop receiving `DiscordCommand` values from the bridge loop. `apply_reload` — updates channel ID and webhook ID filter sets on bridge config reload. `snapshot_from_cache` — emits `MemberSnapshot` events for newly added bridges using serenity's cache. |
