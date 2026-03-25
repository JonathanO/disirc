# TODO — spec/02-irc-connection

Status: **Pending**

- [ ] Define `S2SEvent` and `S2SCommand` protocol-agnostic types (inbound/outbound event enums)
- [ ] Implement UnrealIRCd translation layer: `IrcMessage` → `S2SEvent` and `S2SCommand` → `IrcMessage`
- [ ] TCP/TLS connection with `tokio-rustls` + line-oriented framing (`\r\n`, max 4096 bytes with MTAGS)
- [ ] Handshake sequence (PASS, PROTOCTL EAUTH, PROTOCTL caps, SID, SERVER; verify uplink credentials)
- [ ] Burst: translate outbound `S2SCommand::IntroduceUser` / `S2SCommand::BurstChannel` / `S2SCommand::EndOfBurst` to wire (UID + SJOIN + EOS)
- [ ] Burst: receive uplink burst, translate wire (UID/SJOIN/SID/EOS) to `S2SEvent` and emit to processing task
- [ ] Ongoing message handling: translate wire PING/PONG/PRIVMSG/NICK/QUIT/PART/KICK/SQUIT to `S2SEvent`
- [ ] Message tag handling (strip `s2s-md/*` and `@unrealircd.org/userhost`; pass `@time=` through)
- [ ] Token-bucket rate limiter (capacity 10, refill 1/500 ms; PING/PONG bypass)
- [ ] Ping keepalive (send every 90 s; timeout after 60 s with no PONG)
- [ ] Reconnection with exponential backoff (5 s → 5 min cap)
