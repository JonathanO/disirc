# TODO — spec/02-irc-connection

Status: **Pending**

- [ ] TCP/TLS connection with `tokio-rustls` + line-oriented framing (`\r\n`, max 4096 bytes with MTAGS)
- [ ] Handshake sequence (PASS, PROTOCTL EAUTH, PROTOCTL caps, SID, SERVER; verify uplink credentials)
- [ ] Burst: send UID + SJOIN + EOS for all active pseudoclients
- [ ] Burst: receive and process uplink UID/SJOIN/SID/EOS (build local state)
- [ ] Ongoing message handling (PING/PONG, PRIVMSG relay, NICK/QUIT/PART/KICK/SQUIT state updates)
- [ ] Message tag parsing (strip `s2s-md/*` and `@unrealircd.org/userhost`; pass `@time=` through)
- [ ] Token-bucket rate limiter (capacity 10, refill 1/500 ms; PING/PONG bypass)
- [ ] Ping keepalive (send every 90 s; timeout after 60 s with no PONG)
- [ ] Reconnection with exponential backoff (5 s → 5 min cap)
