# TODO — spec/04-message-bridging

Status: **Pending**

- [ ] Mutable channel map (consulted on every message, updated atomically on reload)
- [ ] Discord→IRC relay pipeline (filter → format → emit `S2SCommand::Privmsg`; attachment URLs; sticker handling)
- [ ] IRC→Discord relay pipeline (`S2SEvent::Message` → filter → format → webhook preferred → plain fallback)
- [ ] Loop prevention (bot/webhook ID filter on Discord side; SID prefix filter on IRC side)
- [ ] NOTICE and ACTION (`/me`) handling
- [ ] Error handling (inaccessible channels log at ERROR; failed sends log at WARN; link-down drops)
