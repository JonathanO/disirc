# Research Index

Check this file before starting any research task. If the topic is already covered, read the existing file rather than re-investigating.

| File | Topic | Key conclusions |
|------|-------|-----------------|
| [unreal-ircd-s2s-protocol.md](unreal-ircd-s2s-protocol.md) | UnrealIRCd S2S protocol vs RFC 2813 | Uses SID/UID (not RFC 2813 tokens); PROTOCTL for capability negotiation; SJOIN replaces NJOIN; EAUTH must be first PROTOCTL; EOS marks end of burst |
| [discord-irc-prior-art.md](discord-irc-prior-art.md) | FauxFaux/discord-irc prior art | Webhook-per-channel for IRC user identity in Discord; loop prevention via webhook user ID filtering; @everyone suppression bug if allowed_mentions omitted; ping-fix zero-width space |
| [unrealircd-ircv3-s2s.md](unrealircd-ircv3-s2s.md) | UnrealIRCd IRCv3 capabilities and S2S interaction | PROTOCTL MTAGS is the master gate for all tag propagation S2S; @time, @msgid, @account, @bot all propagate when MTAGS is active; echo-message/chathistory/standard-replies are client-only; s2s-md/ tags carry early moddata in UID burst |
| [discord-markdown-parsing.md](discord-markdown-parsing.md) | Discord markdown parser internals and edge cases | Discord uses a fork of Khan Academy's simple-markdown (regex rule-based, no spec); `__` is underline not bold; parsing priority: code > spoiler > underline > bold > italic > strikethrough; no client library ships a parser (only formatters); existing Rust crates have known bugs; hand-rolled regex approach is more reliable for bridge use |
| [inspircd-spanningtree-s2s.md](inspircd-spanningtree-s2s.md) | InspIRCd SpanningTree S2S protocol | Uses CAPAB (not PROTOCTL) for negotiation; HMAC-SHA256 auth in SERVER; FJOIN replaces SJOIN (uses prefix-mode,uuid:membid format); IJOIN for post-burst joins; ENDBURST (not EOS); METADATA/ENCAP/SAVE/LMODE/IJOIN have no UnrealIRCd equivalents; membership IDs in FJOIN and KICK are unique to InspIRCd |
| [ts6-s2s-protocol.md](ts6-s2s-protocol.md) | TS6 IRC S2S protocol (charybdis/solanum/ircd-ratbox) | PASS+CAPAB+SERVER+SVINFO handshake; no explicit EOB — uses remote PING after burst; UID has 9 params (no virthost/cloakedhost/svstamp); EUID adds realhost+account; SJOIN uses @/+ prefixes; BMASK separate from SJOIN; QS≈NOQUIT; ENCAP for optional message routing (no UnrealIRCd equivalent); SAVE for nick collision (vs UnrealIRCd kill) |
