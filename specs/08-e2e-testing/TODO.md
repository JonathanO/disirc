# E2E Testing TODO

## Layer 3: Real-IRC e2e

- [x] Create UnrealIRCd test config file (`tests/fixtures/unrealircd.conf`)
- [x] Add `testcontainers = "0.23"` dev-dependency (replaces docker-compose)
- [x] Implement `TestIrcClient` helper (`tests/helpers/irc_client.rs`)
- [x] Implement `start_unrealircd()` container helper (`tests/helpers/mod.rs`)
- [x] Write test: bridge connects and completes S2S handshake (`e2e_bridge_connects_to_unrealircd`)
- [x] Write test: Discord→IRC message relay (`e2e_discord_to_irc_message_relay`)
- [x] Write test: IRC→Discord message relay (`e2e_irc_to_discord_message_relay`)
- [x] Write test: pseudoclient appears for Discord user (`e2e_pseudoclient_appears_on_irc`)
- [x] CI: GitHub Actions job with Docker (testcontainers manages container; just needs Docker daemon)
- [ ] Run tests against live Docker and verify all 4 pass

## Layer 4: Full e2e

- [ ] Implement `DiscordTestClient` helper (reqwest REST)
- [ ] Write test: Discord→IRC via real Discord API
- [ ] Write test: IRC→Discord via real Discord API
- [ ] Write test: formatting preserved across bridge
- [ ] Write test: nick/username correct across bridge
- [ ] CI: GitHub Actions job with secrets
- [ ] Document manual setup steps for test guild/bot
