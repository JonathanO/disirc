# Research Index

Check this file before starting any research task. If the topic is already covered, read the existing file rather than re-investigating.

| File | Topic | Key conclusions |
|------|-------|-----------------|
| [unreal-ircd-s2s-protocol.md](unreal-ircd-s2s-protocol.md) | UnrealIRCd S2S protocol vs RFC 2813 | Uses SID/UID (not RFC 2813 tokens); PROTOCTL for capability negotiation; SJOIN replaces NJOIN; EAUTH must be first PROTOCTL; EOS marks end of burst |
| [discord-irc-prior-art.md](discord-irc-prior-art.md) | FauxFaux/discord-irc prior art | Webhook-per-channel for IRC user identity in Discord; loop prevention via webhook user ID filtering; @everyone suppression bug if allowed_mentions omitted; ping-fix zero-width space |
| [unrealircd-ircv3-s2s.md](unrealircd-ircv3-s2s.md) | UnrealIRCd IRCv3 capabilities and S2S interaction | PROTOCTL MTAGS is the master gate for all tag propagation S2S; @time, @msgid, @account, @bot all propagate when MTAGS is active; echo-message/chathistory/standard-replies are client-only; s2s-md/ tags carry early moddata in UID burst |
