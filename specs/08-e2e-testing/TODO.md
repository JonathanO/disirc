# E2E Testing TODO

## Layer 3: Real-IRC e2e

- [ ] Create UnrealIRCd test config file
- [ ] Create Docker setup (Dockerfile or docker-compose.yml)
- [ ] Implement `TestIrcClient` helper (raw tokio TCP)
- [ ] Write test: bridge connects and completes S2S handshake
- [ ] Write test: Discord→IRC message relay (injected event → PRIVMSG)
- [ ] Write test: IRC→Discord message relay (PRIVMSG → wiremock POST)
- [ ] Write test: pseudoclient appears for Discord user
- [ ] Write test: bridge reconnects after link loss
- [ ] CI: GitHub Actions job with Docker services

## Layer 4: Full e2e

- [ ] Implement `DiscordTestClient` helper (reqwest REST)
- [ ] Write test: Discord→IRC via real Discord API
- [ ] Write test: IRC→Discord via real Discord API
- [ ] Write test: formatting preserved across bridge
- [ ] Write test: nick/username correct across bridge
- [ ] CI: GitHub Actions job with secrets
- [ ] Document manual setup steps for test guild/bot
