# Developing disirc

## Prerequisites

- **Rust** (stable, edition 2024) — install via [rustup](https://rustup.rs/)
- **cargo-deny** — dependency audit tool
- **cargo-mutants** — mutation testing tool

Install the CLI tools:

```sh
cargo install cargo-deny cargo-mutants
```

Enable the pre-commit hook:

```sh
git config core.hooksPath hooks
```

## Building

```sh
cargo build
```

For a release build:

```sh
cargo build --release
```

## Running

1. Copy the example config and fill in your real values:

   ```sh
   cp config.example.toml config.toml
   ```

2. Edit `config.toml` with your Discord bot token, IRC uplink details, SID, and bridge channel mappings. See the comments in `config.example.toml` for guidance.

3. Run the daemon:

   ```sh
   cargo run
   ```

   Or with a custom config path:

   ```sh
   cargo run -- --config /path/to/config.toml
   ```

4. Control log verbosity with `RUST_LOG`:

   ```sh
   RUST_LOG=debug cargo run
   RUST_LOG=disirc=trace,serenity=warn cargo run
   ```

> **Never commit `config.toml`** — it contains secrets. The `.gitignore` already excludes it.

## Testing

### Unit and integration tests

```sh
cargo test
```

Some tests that require real network connections are marked `#[ignore]`. To include them:

```sh
cargo test -- --include-ignored
```

### Linting

Clippy is configured with pedantic and cargo lint groups in `Cargo.toml` under `[lints.clippy]`. Run with warnings as errors:

```sh
cargo clippy -- -D warnings
```

### Formatting

```sh
cargo fmt --check    # verify
cargo fmt            # auto-fix
```

### Dependency audit

Checks for CVEs, licence violations, and banned crates (configured in `deny.toml`):

```sh
cargo deny check
```

### Quality gate

All four checks must pass before any commit:

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo deny check
```

### Pre-commit hook

A pre-commit hook that enforces the first three checks (fmt, clippy, test) is included in `hooks/`. To activate it after cloning:

```sh
git config core.hooksPath hooks
```

This runs automatically on every `git commit`. `cargo deny` is excluded from the hook because it requires network access and is slow — run it manually or rely on CI.

> The hook is already activated if you followed this guide. It applies only to your local clone (`core.hooksPath` is not committed to the repo config).

## Mutation testing

[cargo-mutants](https://github.com/llogiq/mutagen) systematically modifies your code and checks that at least one test fails for each mutation. This catches gaps where tests pass but don't actually verify behaviour.

### Run mutation tests for a specific module

```sh
cargo mutants --file src/bridge.rs
cargo mutants --file src/formatting.rs
cargo mutants --file src/pseudoclients.rs
```

### Run mutation tests for the whole crate

```sh
cargo mutants
```

> **Note:** Mutation testing is slow (minutes to hours depending on crate size). Scoping to a single file is recommended during development.

### Interpreting results

- **caught** — a test detected the mutation (good)
- **unviable** — the mutation did not compile (neutral)
- **missed** — no test caught the mutation (potential test gap)

Missed mutants should be investigated. If they represent real test gaps, write tests to catch them. If they are equivalent mutants (e.g., noop shims, non-deterministic clock functions, integration-only async loops), document them in the relevant `specs/<name>/TODO.md`.

### Closing out a spec

Before marking a spec as Implemented in `SPECS.md`, mutation testing must pass with zero missed mutants that represent real test gaps:

```sh
cargo mutants --file src/<module>.rs
```

See `CLAUDE.md` section "Closing out a spec" for the full policy.

## End-to-end tests

### Layer 3: Real UnrealIRCd, mocked Discord

Requires Docker. Pulls a pre-built UnrealIRCd image from `ghcr.io/jonathano/disirc-unrealircd-test` (cached locally after first pull).

```sh
cargo test --test e2e_irc -- --include-ignored --nocapture
```

### Layer 4: Real UnrealIRCd + real Discord

Requires Docker and two Discord bots. See the setup guide below, then:

```sh
cargo test --test e2e_discord -- --include-ignored --nocapture --test-threads=1
```

### Discord bot setup (Layer 4)

Layer 4 tests need two Discord bots and a test guild. This is a one-time manual setup.

#### Create the bots

In the [Discord Developer Portal](https://discord.com/developers/applications), create two Applications:

1. **Bridge bot** — connects via Gateway, relays messages.
2. **Test harness bot** — used by tests to send and poll messages via REST.

For each, go to Bot tab and create a bot user. Copy each bot's token.

#### Enable privileged intents (bridge bot only)

In Developer Portal > bridge bot Application > Bot > Privileged Gateway Intents, enable:

- **Server Members Intent** — for member snapshots (pseudoclients)
- **Message Content Intent** — to read message text
- **Presence Intent** — for online/offline status tracking

The test harness bot does not need privileged intents.

#### Invite both bots to the test guild

Open these OAuth2 URLs in a browser while logged into Discord.

**Bridge bot** (Gateway + webhooks):
```
https://discord.com/oauth2/authorize?client_id=BRIDGE_BOT_CLIENT_ID&scope=bot&permissions=536874048
```
Permissions: View Channels, Send Messages, Read Message History, Manage Webhooks.

**Test harness bot** (REST only):
```
https://discord.com/oauth2/authorize?client_id=TEST_BOT_CLIENT_ID&scope=bot&permissions=68608
```
Permissions: View Channels, Send Messages, Read Message History.

Replace the `client_id` values with Application IDs from the Developer Portal.

#### Create test channels

In the test guild, create two text channels:

- **Webhook channel** (e.g. `#e2e-webhook`) — create a webhook in Channel Settings > Integrations > Webhooks. Copy the webhook URL.
- **Plain channel** (e.g. `#e2e-plain`) — no webhook. Tests the plain bot message path.

Both bots must have Send Messages and Read Message History in both channels.

#### Set environment variables

| Variable | Type | Example |
|----------|------|---------|
| `DISCORD_BRIDGE_BOT_TOKEN` | Secret | `MTQ4ODgyMDU3M...` |
| `DISCORD_TEST_BOT_TOKEN` | Secret | `MTQ5MDEyMzQ1N...` |
| `DISCORD_TEST_WEBHOOK_URL` | Secret | `https://discord.com/api/webhooks/123/abc...` |
| `DISCORD_TEST_GUILD_ID` | ID | `1234567890123456` |
| `DISCORD_TEST_CHANNEL_ID` | ID | `1234567890123457` |
| `DISCORD_TEST_IRC_CHANNEL` | Name | `#e2e-webhook` |
| `DISCORD_TEST_PLAIN_CHANNEL_ID` | ID | `1234567890123458` |
| `DISCORD_TEST_PLAIN_IRC_CHANNEL` | Name | `#e2e-plain` |

IRC channel names **must** include the `#` prefix.

To get IDs: enable Developer Mode in Discord (User Settings > Advanced > Developer Mode), then right-click a channel or guild name and Copy ID.

#### Run locally

```sh
export DISCORD_BRIDGE_BOT_TOKEN="..."
export DISCORD_TEST_BOT_TOKEN="..."
export DISCORD_TEST_WEBHOOK_URL="..."
export DISCORD_TEST_GUILD_ID="..."
export DISCORD_TEST_CHANNEL_ID="..."
export DISCORD_TEST_IRC_CHANNEL="#e2e-webhook"
export DISCORD_TEST_PLAIN_CHANNEL_ID="..."
export DISCORD_TEST_PLAIN_IRC_CHANNEL="#e2e-plain"

cargo test --test e2e_discord -- --include-ignored --nocapture --test-threads=1
```

### CI configuration

Layer 4 tests run in a GitHub Environment called `discord-e2e` with required reviewer approval. This prevents accidental secret exposure and rate limit burn.

In repo Settings > Environments > `discord-e2e`:

**Secrets** (3): `DISCORD_BRIDGE_BOT_TOKEN`, `DISCORD_TEST_BOT_TOKEN`, `DISCORD_TEST_WEBHOOK_URL`

**Variables** (5): `DISCORD_TEST_GUILD_ID`, `DISCORD_TEST_CHANNEL_ID`, `DISCORD_TEST_IRC_CHANNEL`, `DISCORD_TEST_PLAIN_CHANNEL_ID`, `DISCORD_TEST_PLAIN_IRC_CHANNEL`

### UnrealIRCd test image

Published to `ghcr.io/jonathano/disirc-unrealircd-test` by the `docker-test-image.yml` workflow. Auto-rebuilds when `tests/fixtures/Dockerfile` changes on main. Manual rebuild: Actions tab > "Publish UnrealIRCd test image" > Run workflow.

## Project structure

See `LAYOUT.md` for a detailed map of every source module. The high-level architecture:

```
main.rs
  |
  +-- spawns IRC connection task (src/irc/)
  |     communicates via mpsc channels: S2SEvent / S2SCommand
  |
  +-- spawns Discord connection task (src/discord/)
  |     communicates via mpsc channels: DiscordEvent / DiscordCommand
  |
  +-- runs bridge loop (src/bridge.rs)
        owns BridgeMap, IrcState, DiscordState, PseudoclientManager
        routes messages bidirectionally
        handles config hot-reload via ControlEvent
```
