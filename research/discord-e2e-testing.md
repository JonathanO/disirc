# Discord E2E Testing Approaches

## Summary

Investigated how to end-to-end test a Discord bot (specifically the Discord side of an IRC bridge) without using Discord's Gateway for the test harness. The recommended approach is raw REST API calls via reqwest with a second bot token and REST polling for message verification.

## Findings

### Serenity provides no test utilities

Multiple GitHub issues confirm there is no mock client, test harness, or fake Gateway in serenity:
- Issue #758, #1247, #1895 all recommend extracting logic into pure functions.
- The poise framework (built on serenity) also has no mock context (issue #142).
- No community crate exists for mocking serenity.

### Test bot setup (one-time manual)

- Create a second Discord Application in the Developer Portal (cannot be automated).
- Enable the Bot user, copy the token.
- Enable Message Content privileged intent.
- Create or designate a test guild, invite both bots (bridge + test harness).
- Store tokens and IDs as CI secrets.

### REST API is sufficient for the test harness

A full Gateway connection is unnecessary. The test harness only needs:
- **Send:** `POST /api/v10/channels/{id}/messages` with `Authorization: Bot <token>`.
- **Verify:** `GET /api/v10/channels/{id}/messages?limit=10&after={snowflake}` polling every 500ms with 10-second timeout.
- **Webhook verification:** Messages sent via webhooks have `webhook_id` set on the Message object.

This requires only `reqwest` (already a transitive dependency) and `serde_json`.

### Rate limits are generous

- 5 messages per 5 seconds per channel.
- 50 requests per second globally.
- 30 requests per second per webhook.
- A test suite of 10-20 tests running sequentially will not hit limits.

### CI considerations

- Tests marked `#[ignore]` by default, separate CI job.
- Only run when secrets are available (GitHub Actions `if: secrets.DISCORD_TEST_BOT_TOKEN != ''`).
- Sequential execution in a single channel to avoid interference.
- Generous timeouts (5-10s) to absorb Discord API latency spikes.

### Prior art

No mature Discord e2e testing framework exists:
- Corde (JS, 2022, unmaintained): second bot sends commands, verifies responses.
- distest (Python, unmaintained): discord.py-based.
- Most projects rely on unit tests + manual testing + HTTP mocking.
- Discord has not provided official testing infrastructure (feature request #2528 closed 2021).

## References

- [Serenity Issue #758: Best way to test](https://github.com/serenity-rs/serenity/issues/758)
- [Serenity Issue #1247: Testing without Discord](https://github.com/serenity-rs/serenity/issues/1247)
- [Discord Message API](https://docs.discord.com/developers/resources/message)
- [Discord Rate Limits](https://discord.com/developers/docs/topics/rate-limits)
- [Corde E2E Testing Library](https://github.com/cordejs/corde)
- [Discord API Feature Request #2528](https://github.com/discord/discord-api-docs/issues/2528)
