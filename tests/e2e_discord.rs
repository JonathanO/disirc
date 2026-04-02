//! Layer 4 e2e tests: real `UnrealIRCd` + real Discord Gateway.
//!
//! Uses a **single** bridge instance (one UnrealIRCd container, one Discord
//! Gateway connection) shared across all assertion blocks.  This avoids
//! rapid gateway reconnections that cause Discord to throttle GUILD_CREATE.
//!
//! Run explicitly with:
//!
//! ```text
//! cargo test --test e2e_discord -- --include-ignored --nocapture
//! ```
//!
//! Required environment variables (all read at runtime, never committed):
//!
//! - `DISCORD_BRIDGE_BOT_TOKEN` — bridge bot's Gateway token
//! - `DISCORD_TEST_BOT_TOKEN` — test harness bot's REST token
//! - `DISCORD_TEST_GUILD_ID` — test guild ID
//! - `DISCORD_TEST_CHANNEL_ID` — webhook-enabled channel ID
//! - `DISCORD_TEST_IRC_CHANNEL` — IRC channel mapped to the above
//! - `DISCORD_TEST_WEBHOOK_URL` — webhook URL for the above channel
//! - `DISCORD_TEST_PLAIN_CHANNEL_ID` — plain (no webhook) channel ID
//! - `DISCORD_TEST_PLAIN_IRC_CHANNEL` — IRC channel mapped to the above

mod helpers;

use helpers::log_capture::init_capture_tracing;
use std::path::Path;
use tokio::sync::mpsc;
use tokio::time::Duration;

use disirc::bridge::run_bridge;
use disirc::config::{BridgeEntry, Config, DiscordConfig, IrcConfig, PseudoclientConfig};
use disirc::discord::connection::run_discord;
use disirc::discord::{DiscordCommand, DiscordEvent};
use disirc::irc::unreal::run_connection;
use disirc::irc::{S2SCommand, S2SEvent};
use disirc::signal::ControlEvent;

#[path = "helpers/discord_client.rs"]
mod discord_client;
use discord_client::DiscordTestClient;

// ---------------------------------------------------------------------------
// Environment-based configuration
// ---------------------------------------------------------------------------

/// All secrets read from the environment. None are ever hardcoded.
struct Secrets {
    bridge_token: String,
    test_token: String,
    #[allow(dead_code)] // Required for env validation; used by future guild-level tests.
    guild_id: u64,
    webhook_channel_id: u64,
    webhook_irc_channel: String,
    webhook_url: String,
    plain_channel_id: u64,
    plain_irc_channel: String,
}

/// Helper to read a required env var, panicking with a clear message if missing.
fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("required env var {name} is not set"))
}

/// Helper to read a required env var and parse it as `u64`.
fn required_env_u64(name: &str) -> u64 {
    required_env(name)
        .parse()
        .unwrap_or_else(|e| panic!("env var {name} is not a valid u64: {e}"))
}

/// Read all required environment variables. Panics if any are missing —
/// these tests are gated behind a GitHub Environment with required
/// reviewer approval, so missing vars indicates misconfiguration.
fn read_secrets() -> Secrets {
    Secrets {
        bridge_token: required_env("DISCORD_BRIDGE_BOT_TOKEN"),
        test_token: required_env("DISCORD_TEST_BOT_TOKEN"),
        guild_id: required_env_u64("DISCORD_TEST_GUILD_ID"),
        webhook_channel_id: required_env_u64("DISCORD_TEST_CHANNEL_ID"),
        webhook_irc_channel: required_env("DISCORD_TEST_IRC_CHANNEL"),
        webhook_url: required_env("DISCORD_TEST_WEBHOOK_URL"),
        plain_channel_id: required_env_u64("DISCORD_TEST_PLAIN_CHANNEL_ID"),
        plain_irc_channel: required_env("DISCORD_TEST_PLAIN_IRC_CHANNEL"),
    }
}

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

/// Build a config for the full bridge with real Discord + real IRC.
fn full_config(secrets: &Secrets, host: &str, s2s_port: u16) -> Config {
    Config {
        discord: DiscordConfig {
            token: secrets.bridge_token.clone(),
        },
        irc: IrcConfig {
            uplink: host.to_string(),
            port: s2s_port,
            tls: false,
            link_name: "bridge.test.net".into(),
            link_password: "testpassword".into(),
            sid: "002".into(),
            description: "E2E Test Bridge".into(),
        },
        pseudoclients: PseudoclientConfig {
            host_suffix: "discord.test.net".into(),
            ident: "discord".into(),
        },
        formatting: disirc::config::FormattingConfig {
            dm_bridging: true,
            ..disirc::config::FormattingConfig::default()
        },
        bridges: vec![
            BridgeEntry {
                discord_channel_id: secrets.webhook_channel_id.to_string(),
                irc_channel: secrets.webhook_irc_channel.clone(),
                webhook_url: Some(secrets.webhook_url.clone()),
            },
            BridgeEntry {
                discord_channel_id: secrets.plain_channel_id.to_string(),
                irc_channel: secrets.plain_irc_channel.clone(),
                webhook_url: None,
            },
        ],
    }
}

/// Handles for a fully-wired bridge (real Discord + real IRC).
struct FullBridge {
    bridge_handle: tokio::task::JoinHandle<()>,
    irc_handle: tokio::task::JoinHandle<()>,
    discord_handle: tokio::task::JoinHandle<()>,
    _control_tx: mpsc::Sender<ControlEvent>,
}

impl Drop for FullBridge {
    fn drop(&mut self) {
        self.bridge_handle.abort();
        self.irc_handle.abort();
        self.discord_handle.abort();
    }
}

/// Spawn the full bridge with real Discord Gateway and real IRC S2S connections.
fn spawn_full_bridge(config: &Config) -> FullBridge {
    let (irc_event_tx, irc_event_rx) = mpsc::channel::<S2SEvent>(256);
    let (irc_cmd_tx, irc_cmd_rx) = mpsc::channel::<S2SCommand>(256);
    let (discord_event_tx, discord_event_rx) = mpsc::channel::<DiscordEvent>(256);
    let (discord_cmd_tx, discord_cmd_rx) = mpsc::channel::<DiscordCommand>(256);
    let (control_tx, control_rx) = mpsc::channel::<ControlEvent>(4);

    let config_owned = config.clone();
    let bridge_handle = tokio::spawn(async move {
        run_bridge(
            &config_owned,
            Path::new("/dev/null"),
            irc_event_rx,
            irc_cmd_tx,
            discord_event_rx,
            discord_cmd_tx,
            control_rx,
        )
        .await;
    });

    let irc_config = config.irc.clone();
    let irc_handle = tokio::spawn(async move {
        run_connection(&irc_config, irc_cmd_rx, irc_event_tx).await;
    });

    let discord_config = config.discord.clone();
    let bridges = config.bridges.clone();
    let discord_handle = tokio::spawn(async move {
        run_discord(&discord_config, &bridges, discord_event_tx, discord_cmd_rx).await;
    });

    FullBridge {
        bridge_handle,
        irc_handle,
        discord_handle,
        _control_tx: control_tx,
    }
}

/// Wait for the S2S link to appear in LINKS, same as Layer 3.
async fn wait_for_bridge_in_links(
    client: &mut helpers::TestIrcClient,
    bridge_name: &str,
    timeout_secs: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        client.send("LINKS").await;
        let poll_end = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut found = false;
        let mut end_of_links = false;
        while !end_of_links {
            let remaining = poll_end
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                break;
            }
            let Some(line) = client.read_line_timeout(remaining).await else {
                break;
            };
            if let Some(token) = line.strip_prefix("PING :") {
                client.send(&format!("PONG :{token}")).await;
                continue;
            }
            if line.contains(bridge_name) {
                found = true;
            }
            if line.contains(" 365 ") {
                end_of_links = true;
            }
        }
        if found {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Timed out after {timeout_secs}s: {bridge_name:?} not found in LINKS"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---------------------------------------------------------------------------
// Test suite
// ---------------------------------------------------------------------------

/// Full Layer 4 e2e test suite.
///
/// Starts one UnrealIRCd container and one bridge (IRC + Discord + bridge
/// processor) and runs all assertion blocks against the shared infrastructure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + Discord credentials"]
async fn e2e_discord_suite() {
    let capture = init_capture_tracing();
    let secrets = read_secrets();
    let irc = helpers::start_unrealircd().await;
    let config = full_config(&secrets, &irc.host, irc.s2s_port);
    let _bridge = spawn_full_bridge(&config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join(&secrets.webhook_irc_channel).await;
    client.join(&secrets.plain_irc_channel).await;
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    // Wait for guild_create → MemberSnapshot → deferred burst → pseudoclient
    // JOIN.  This proves the Discord Gateway delivered GUILD_CREATE (which
    // requires the GUILDS intent) and the bridge created pseudoclients.
    client
        .expect_line_containing("JOIN", Duration::from_secs(30))
        .await;

    // --- Discord → IRC (webhook channel) ---
    {
        let discord = DiscordTestClient::new(&secrets.test_token, secrets.webhook_channel_id);
        discord.send_message("suite-webhook-d2i").await;
        client
            .expect_line_containing("suite-webhook-d2i", Duration::from_secs(10))
            .await;
    }

    // --- IRC → Discord (webhook channel) ---
    {
        let discord = DiscordTestClient::new(&secrets.test_token, secrets.webhook_channel_id);
        let anchor = discord.latest_message_id().await;

        client
            .send_privmsg(&secrets.webhook_irc_channel, "suite-webhook-i2d")
            .await;

        let msg = discord
            .poll_messages_containing(&anchor, "suite-webhook-i2d", Duration::from_secs(10))
            .await;
        assert!(
            msg.author.bot,
            "expected webhook message (bot=true), got bot=false"
        );
        assert_eq!(
            msg.author.username, "testbot",
            "webhook username should match IRC nick"
        );
        // Webhook path: message content is the raw text, no **[nick]** prefix.
        assert_eq!(
            msg.content, "suite-webhook-i2d",
            "webhook message content should be exactly the IRC text, no nick prefix"
        );
    }

    // --- Formatting (webhook channel) ---
    {
        let discord = DiscordTestClient::new(&secrets.test_token, secrets.webhook_channel_id);
        discord.send_message("**bold** *italic* `code`").await;

        let line = client
            .expect_line_containing("bold", Duration::from_secs(10))
            .await;
        assert!(
            line.contains('\x02'),
            "expected IRC bold control code (\\x02) in: {line:?}"
        );
    }

    // --- Discord → IRC (plain channel) ---
    {
        let discord = DiscordTestClient::new(&secrets.test_token, secrets.plain_channel_id);
        discord.send_message("suite-plain-d2i").await;
        client
            .expect_line_containing("suite-plain-d2i", Duration::from_secs(10))
            .await;
    }

    // --- IRC → Discord (plain channel) ---
    {
        let discord = DiscordTestClient::new(&secrets.test_token, secrets.plain_channel_id);
        let anchor = discord.latest_message_id().await;

        client
            .send_privmsg(&secrets.plain_irc_channel, "suite-plain-i2d")
            .await;

        let msg = discord
            .poll_messages_containing(&anchor, "suite-plain-i2d", Duration::from_secs(10))
            .await;
        // Plain path: message has exactly one **[nick]** prefix followed by content.
        // The nick is ping-fixed (ZWNJ after first char), so check structure not exact nick.
        assert!(
            msg.content.starts_with("**["),
            "plain message must start with **[nick]** prefix, got: {:?}",
            msg.content
        );
        assert_eq!(
            msg.content.matches("**[").count(),
            1,
            "plain message must have exactly one **[nick]** prefix (no duplication), got: {:?}",
            msg.content
        );
        assert!(
            msg.content.contains("suite-plain-i2d"),
            "plain message must contain the original text, got: {:?}",
            msg.content
        );
    }

    // --- Formatting (plain channel) ---
    {
        let discord = DiscordTestClient::new(&secrets.test_token, secrets.plain_channel_id);
        discord.send_message("**bold** *italic* `code`").await;

        let line = client
            .expect_line_containing("bold", Duration::from_secs(10))
            .await;
        assert!(
            line.contains('\x02'),
            "expected IRC bold control code (\\x02) in: {line:?}"
        );
    }

    // --- Discord → IRC mention resolution ---
    //
    // The test bot sends a message containing its own user mention `<@id>`.
    // The bridge should resolve it to the bot's display name on IRC.
    {
        let discord = DiscordTestClient::new(&secrets.test_token, secrets.webhook_channel_id);
        // Send a probe message to discover the test bot's user ID.
        let probe = discord.send_message("mention-probe").await;
        let test_bot_id = &probe.author.id;
        let test_bot_name = &probe.author.username;
        client
            .expect_line_containing("mention-probe", Duration::from_secs(10))
            .await;

        // Now send a message that mentions the test bot by ID.
        discord
            .send_message(&format!("hello <@{test_bot_id}>!"))
            .await;

        let line = client
            .expect_line_containing("hello", Duration::from_secs(10))
            .await;
        // The mention should be resolved to @name, not the raw <@id>.
        assert!(
            !line.contains(&format!("<@{test_bot_id}>")),
            "raw mention <@{test_bot_id}> should be resolved, not passed through; got: {line:?}"
        );
        assert!(
            line.contains(&format!("@{test_bot_name}")),
            "mention should resolve to @{test_bot_name}; got: {line:?}"
        );
    }

    // --- IRC → Discord mention resolution ---
    //
    // The IRC client sends `@PseudoclientNick` and the bridge should convert
    // it to a Discord `<@user_id>` mention.  We use the test bot's pseudoclient
    // since it has a known Discord user ID.
    {
        let discord = DiscordTestClient::new(&secrets.test_token, secrets.plain_channel_id);
        // Discover the test bot's pseudoclient nick by sending a probe and
        // checking what JOIN'd the channel.  The test bot's Discord user should
        // have a pseudoclient from the guild_create snapshot.
        let probe = discord.send_message("nick-probe").await;
        let test_bot_id = &probe.author.id;
        client
            .expect_line_containing("nick-probe", Duration::from_secs(10))
            .await;

        // Find the pseudoclient's nick.  It's based on the test bot's username
        // with a ping-fix ZWSP after the first character.  For mentioning from
        // IRC, we use the original username (without ZWSP) — the resolver does
        // case-insensitive matching.
        let test_bot_name = &probe.author.username;

        let anchor = discord.latest_message_id().await;
        client
            .send_privmsg(
                &secrets.plain_irc_channel,
                &format!("hey @{test_bot_name} are you there?"),
            )
            .await;

        let msg = discord
            .poll_messages_containing(&anchor, "are you there", Duration::from_secs(10))
            .await;
        assert!(
            msg.content.contains(&format!("<@{test_bot_id}>")),
            "IRC @{test_bot_name} should resolve to <@{test_bot_id}> in Discord; got: {:?}",
            msg.content
        );
    }

    // Note: L4 DM tests are not included because Discord does not allow
    // bot-to-bot DMs.  DM bridging is tested at L3 (mock Discord, real IRC)
    // where we can inject DmReceived events directly.  Manual testing with
    // a human Discord user is needed to verify the full L4 DM path.

    capture.assert_no_warnings_or_errors();
}
