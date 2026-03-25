# TODO — spec/02-irc-connection

Status: **Implemented**

- [x] Define `S2SEvent` and `S2SCommand` protocol-agnostic types (inbound/outbound event enums)
- [x] Implement UnrealIRCd translation layer: `IrcMessage` → `S2SEvent` and `S2SCommand` → `IrcMessage`
- [x] TCP/TLS connection with `tokio-rustls` + line-oriented framing (`\r\n`, max 4096 bytes with MTAGS)
- [x] Handshake sequence (PASS, PROTOCTL EAUTH, PROTOCTL caps, SID, SERVER; verify uplink credentials)
- [x] Burst: translate outbound `S2SCommand::IntroduceUser` / `S2SCommand::BurstComplete` to wire (UID + EOS)
- [x] Burst: receive uplink burst, translate wire (UID/SJOIN/SID/EOS) to `S2SEvent` and emit to processing task
- [x] Ongoing message handling: translate wire PING/PONG/PRIVMSG/NICK/QUIT/PART/KICK/SQUIT to `S2SEvent`
- [x] Message tag handling (strip `s2s-md/*` and `@unrealircd.org/userhost`; pass `@time=` through)
- [x] Token-bucket rate limiter (capacity 10, refill 1/500 ms; PING/PONG bypass)
- [x] Ping keepalive (send every 90 s; timeout after 60 s with no PONG)
- [x] Reconnection with exponential backoff + full jitter (5 s → 300 s cap)
- [x] Run mutation testing (`cargo mutants`) and address surviving mutants
  - 3 MISSED mutants documented as equivalent: two `> vs >=` in framing.rs (end always ≥1), one `!queue.is_empty()` near-equivalent in connection.rs (busy-loop but identical I/O)
