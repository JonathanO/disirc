# Source layout

This file describes every module in `src/` and what belongs in each one.
Update it whenever a module is added, removed, or significantly refactored.

---

## Top-level modules (`src/`)

| File / dir | What it contains |
|------------|-----------------|
| `src/lib.rs` | Crate root — declares all public modules; `#![deny(unsafe_code)]`. |
| `src/main.rs` | Binary entry point (stub until application wiring is added). |
| `src/config.rs` | Configuration file format (`Config`, `IrcConfig`, `BridgeEntry`, etc.) and validation. Read from `config.toml` at startup. |
| `src/formatting.rs` | Bidirectional text transforms: Discord markdown ↔ IRC formatting codes, mention/emoji expansion, message splitting, truncation. No I/O. |
| `src/pseudoclients.rs` | Pseudoclient lifecycle and identity. Tracks Discord users as fake IRC clients; generates UID allocations, nick sanitisation, and builds the UnrealIRCd wire messages (UID/SJOIN/QUIT/PART) to introduce or remove them. |
| `src/bridge.rs` | **Bridge processing layer.** `BridgeMap` (discord↔IRC channel routing), `IrcState` (external nick map + channel-ts cache), `DiscordState` (display-name cache + guild→irc-channel map), and the five pure relay functions: `discord_to_irc_commands`, `irc_to_discord_command`, `apply_irc_event`, `apply_discord_event`. No I/O; takes and returns protocol-agnostic types (`S2SCommand`, `DiscordCommand`). |
| `src/signal.rs` | OS signal handling (SIGTERM / Ctrl-C). Wraps `tokio::signal` into a future the main task can `select!` on. |
| `src/irc/` | IRC abstraction layer — see below. |

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
