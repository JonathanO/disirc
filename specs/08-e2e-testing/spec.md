# Spec 08: End-to-End Testing

## Goal

Verify that the full bridge works against real (or close-to-real) services, catching protocol bugs that unit tests and HTTP-level mocks cannot reach.

## Testing layers

The project uses four testing layers, each catching a different class of bug:

| Layer | What it tests | Infra needed | Credentials |
|-------|--------------|--------------|-------------|
| **Unit** | Pure logic: formatting, routing, state management | None | None |
| **HTTP integration** | Wire protocol (duplex), HTTP mechanics (wiremock) | None | None |
| **Real-IRC e2e** | Full S2S handshake + message relay against real UnrealIRCd; Discord side mocked via wiremock | Docker | None |
| **Full e2e** | Both sides live: real IRC + real Discord | Docker + Discord test guild | Bot token, guild/channel IDs |

## Layer 3: Real-IRC e2e (Option C)

### Architecture

```
                    Docker
                 ┌───────────┐
                 │ UnrealIRCd │
                 │  SID=001   │
                 │ :6667 :6900│
                 └──┬─────┬──┘
       S2S (6900)   │     │  IRC client (6667)
                 ┌──┴─────┴──┐
                 │   Bridge   │
                 │  SID=002   │
                 └──────┬─────┘
                        │ mpsc / wiremock
                 ┌──────┴─────┐
                 │  Test code  │
                 │ (wiremock + │
                 │  IRC client)│
                 └────────────┘
```

### Components

**UnrealIRCd in Docker:**
- Image: `ircd/unrealircd` (or custom Dockerfile).
- Minimal config: SID 001, S2S link block for `bridge.test.net` (SID 002, password `testpassword`, plain TCP port 6900), client listener on port 6667.
- Startup detection: TCP connect retry with 100ms poll, 10-second timeout.

**Test IRC client:**
- Raw tokio TCP (~60 lines). Connect to port 6667, send `NICK`/`USER`, `JOIN` the bridged channel, read/send `PRIVMSG`.
- No new dependency — tokio is already available.

**Discord mock (wiremock):**
- Reuses the wiremock proxy pattern from the HTTP integration tests.
- Mocks webhook GET/POST endpoints for IRC→Discord verification.
- For Discord→IRC: injects `DiscordEvent::MessageReceived` directly into the bridge's mpsc channel (no Discord API involvement).

### Test cases

1. **Bridge connects and completes S2S handshake** — verify the bridge's pseudoclients appear on the IRC server (test IRC client sees JOINs or NAMES).
2. **Discord→IRC message relay** — inject `DiscordEvent::MessageReceived`, test IRC client reads the corresponding PRIVMSG.
3. **IRC→Discord message relay** — test IRC client sends PRIVMSG, wiremock verifies the bridge POSTs to the webhook endpoint.
4. **Pseudoclient appears for Discord user** — inject a `MemberSnapshot` event with an online user, verify the nick appears on IRC (WHO or JOIN observed).
5. **Bridge reconnects after IRC link loss** — kill and restart the Docker container (or drop the S2S link), verify the bridge re-establishes and re-bursts pseudoclients.

### CI

- Tests marked `#[ignore]` by default.
- Separate GitHub Actions job with `services` block for Docker.
- No secrets required.

## Layer 4: Full e2e (Option A)

### Architecture

```
                    Docker                     Discord API
                 ┌───────────┐              ┌─────────────┐
                 │ UnrealIRCd │              │  Test Guild  │
                 │  SID=001   │              │  #bridged    │
                 └──┬─────┬──┘              └──┬────────┬─┘
       S2S (6900)   │     │  IRC client         │webhook │ REST
                 ┌──┴─────┴──┐              ┌──┴────────┴─┐
                 │   Bridge   │──── Gateway ─│   Discord    │
                 │  SID=002   │              │   (real)     │
                 └────────────┘              └──────────────┘
                       ▲                           ▲
                       │ TCP                       │ REST
                 ┌─────┴────────────────────┬──────┴───┐
                 │        Test code                     │
                 │  TestIrcClient + DiscordTestClient   │
                 └─────────────────────────────────────┘
```

### Setup (one-time manual)

1. Create a Discord Application + Bot in the Developer Portal.
2. Enable Message Content privileged intent.
3. Create a test guild, invite both the bridge bot and the test harness bot.
4. Create a bridged channel with a webhook URL.
5. Store as GitHub Actions secrets:
   - `DISCORD_TEST_BOT_TOKEN`
   - `DISCORD_TEST_CHANNEL_ID`
   - `DISCORD_TEST_GUILD_ID`
   - `DISCORD_TEST_WEBHOOK_URL`

### Test harness — Discord side

Raw `reqwest` REST API calls (no serenity, no Gateway):

```rust
struct DiscordTestClient {
    http: reqwest::Client,
    token: String,
    channel_id: u64,
}
```

- **Send:** `POST /api/v10/channels/{id}/messages`
- **Verify:** `GET /api/v10/channels/{id}/messages?limit=10&after={snowflake}`, poll every 500ms, 10-second timeout.
- Rate limits are generous (5 msg/5s/channel).

### Test cases

1. **Discord→IRC** — Test bot sends message in bridged channel, test IRC client verifies PRIVMSG arrives on IRC.
2. **IRC→Discord** — Test IRC client sends PRIVMSG, test bot polls channel messages and verifies the webhook-delivered message appears.
3. **Formatting preserved** — Bold/italic/code in Discord message appears as IRC control codes on IRC side (and vice versa).
4. **Nick appears correctly** — Discord user's display name is the pseudoclient nick on IRC; IRC user's nick appears as the webhook username on Discord.

### CI

- Tests `#[ignore]` by default.
- Separate GitHub Actions job, only runs when secrets are present.
- Sequential execution in a single channel.
- Generous timeouts (5-10s per assertion) to absorb Discord API latency.

## Implementation order

1. Real-IRC e2e (Layer 3) first — highest value, no credentials, catches S2S protocol bugs.
2. Full e2e (Layer 4) second — adds Discord path confidence, requires manual setup.

## Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| IRC server for tests | Docker UnrealIRCd | Real protocol, fast startup, no mock maintenance |
| Discord mock for Layer 3 | wiremock (existing) | Already proven in HTTP integration tests |
| Discord harness for Layer 4 | Raw reqwest REST API | Lighter than serenity; no Gateway needed for send+poll |
| Test IRC client | Raw tokio TCP | No new deps; full control over timing |
| Message verification (Discord) | REST polling (GET messages) | Simpler than Gateway listener; sufficient for tests |
| CI strategy | `#[ignore]` + separate jobs | Layer 3 always runs (Docker only); Layer 4 when secrets available |

## References

- [research/unrealircd-docker-integration-testing.md](../../research/unrealircd-docker-integration-testing.md)
- [research/discord-e2e-testing.md](../../research/discord-e2e-testing.md)
