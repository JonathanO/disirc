//! Layer 4 e2e tests: real `UnrealIRCd` + real Discord Gateway.
//!
//! All tests are `#[ignore = "requires Docker + Discord credentials"]` — they require Docker AND Discord bot credentials.
//! Run them explicitly with:
//!
//! ```text
//! cargo test --test e2e_discord -- --include-ignored --nocapture --test-threads=1
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
// Tests — Webhook channel
// ---------------------------------------------------------------------------

/// Test bot sends a message in the webhook channel; IRC client sees the PRIVMSG.
#[tokio::test]
#[ignore = "requires Docker + Discord credentials"]
async fn e2e_discord_to_irc_webhook() {
    let capture = init_capture_tracing();
    let secrets = read_secrets();
    let irc = helpers::start_unrealircd().await;
    let config = full_config(&secrets, &irc.host, irc.s2s_port);
    let _bridge = spawn_full_bridge(&config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join(&secrets.webhook_irc_channel).await;
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    // Allow time for Discord Gateway to connect and member snapshot to propagate.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let discord = DiscordTestClient::new(&secrets.test_token, secrets.webhook_channel_id);
    discord.send_message("layer4-webhook-d2i-test").await;

    client
        .expect_line_containing("layer4-webhook-d2i-test", Duration::from_secs(10))
        .await;

    capture.assert_no_warnings_or_errors();
}

/// IRC client sends a PRIVMSG; test bot polls and finds the webhook message.
#[tokio::test]
#[ignore = "requires Docker + Discord credentials"]
async fn e2e_irc_to_discord_webhook() {
    let capture = init_capture_tracing();
    let secrets = read_secrets();
    let irc = helpers::start_unrealircd().await;
    let config = full_config(&secrets, &irc.host, irc.s2s_port);
    let _bridge = spawn_full_bridge(&config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join(&secrets.webhook_irc_channel).await;
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    tokio::time::sleep(Duration::from_secs(5)).await;

    let discord = DiscordTestClient::new(&secrets.test_token, secrets.webhook_channel_id);
    let anchor = discord.latest_message_id().await;

    client
        .send_privmsg(&secrets.webhook_irc_channel, "layer4-webhook-i2d-test")
        .await;

    let msg = discord
        .poll_messages_containing(&anchor, "layer4-webhook-i2d-test", Duration::from_secs(10))
        .await;

    // Webhook messages should use the IRC nick as the webhook username.
    assert!(
        msg.author.bot,
        "expected webhook message (bot=true), got bot=false"
    );
    assert_eq!(
        msg.author.username, "testbot",
        "webhook username should match IRC nick"
    );

    capture.assert_no_warnings_or_errors();
}

/// Formatting: test bot sends bold/italic/code; IRC client verifies control codes.
#[tokio::test]
#[ignore = "requires Docker + Discord credentials"]
async fn e2e_formatting_webhook() {
    let capture = init_capture_tracing();
    let secrets = read_secrets();
    let irc = helpers::start_unrealircd().await;
    let config = full_config(&secrets, &irc.host, irc.s2s_port);
    let _bridge = spawn_full_bridge(&config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join(&secrets.webhook_irc_channel).await;
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    tokio::time::sleep(Duration::from_secs(5)).await;

    let discord = DiscordTestClient::new(&secrets.test_token, secrets.webhook_channel_id);
    discord.send_message("**bold** *italic* `code`").await;

    // IRC bold = \x02, italic = \x1D, monospace = \x11
    // Verify at least bold (\x02) arrives in the IRC message.
    let line = client
        .expect_line_containing("bold", Duration::from_secs(10))
        .await;
    assert!(
        line.contains('\x02'),
        "expected IRC bold control code (\\x02) in: {line:?}"
    );

    capture.assert_no_warnings_or_errors();
}

// ---------------------------------------------------------------------------
// Tests — Plain channel (no webhook)
// ---------------------------------------------------------------------------

/// Test bot sends a message in the plain channel; IRC client sees the PRIVMSG.
#[tokio::test]
#[ignore = "requires Docker + Discord credentials"]
async fn e2e_discord_to_irc_plain() {
    let capture = init_capture_tracing();
    let secrets = read_secrets();
    let irc = helpers::start_unrealircd().await;
    let config = full_config(&secrets, &irc.host, irc.s2s_port);
    let _bridge = spawn_full_bridge(&config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join(&secrets.plain_irc_channel).await;
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    tokio::time::sleep(Duration::from_secs(5)).await;

    let discord = DiscordTestClient::new(&secrets.test_token, secrets.plain_channel_id);
    discord.send_message("layer4-plain-d2i-test").await;

    client
        .expect_line_containing("layer4-plain-d2i-test", Duration::from_secs(10))
        .await;

    capture.assert_no_warnings_or_errors();
}

/// IRC client sends a PRIVMSG; test bot polls plain channel and finds
/// the `**[nick]** text` format (no webhook username).
#[tokio::test]
#[ignore = "requires Docker + Discord credentials"]
async fn e2e_irc_to_discord_plain() {
    let capture = init_capture_tracing();
    let secrets = read_secrets();
    let irc = helpers::start_unrealircd().await;
    let config = full_config(&secrets, &irc.host, irc.s2s_port);
    let _bridge = spawn_full_bridge(&config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join(&secrets.plain_irc_channel).await;
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    tokio::time::sleep(Duration::from_secs(5)).await;

    let discord = DiscordTestClient::new(&secrets.test_token, secrets.plain_channel_id);
    let anchor = discord.latest_message_id().await;

    client
        .send_privmsg(&secrets.plain_irc_channel, "layer4-plain-i2d-test")
        .await;

    let msg = discord
        .poll_messages_containing(&anchor, "layer4-plain-i2d-test", Duration::from_secs(10))
        .await;

    // Plain path uses **[nick]** format, sent by the bridge bot itself.
    assert!(
        msg.content.contains("**[testbot]**"),
        "expected **[testbot]** prefix in plain message, got: {:?}",
        msg.content
    );

    capture.assert_no_warnings_or_errors();
}

/// Formatting: test bot sends bold/italic/code in plain channel; IRC client
/// verifies control codes arrive.
#[tokio::test]
#[ignore = "requires Docker + Discord credentials"]
async fn e2e_formatting_plain() {
    let capture = init_capture_tracing();
    let secrets = read_secrets();
    let irc = helpers::start_unrealircd().await;
    let config = full_config(&secrets, &irc.host, irc.s2s_port);
    let _bridge = spawn_full_bridge(&config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join(&secrets.plain_irc_channel).await;
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    tokio::time::sleep(Duration::from_secs(5)).await;

    let discord = DiscordTestClient::new(&secrets.test_token, secrets.plain_channel_id);
    discord.send_message("**bold** *italic* `code`").await;

    let line = client
        .expect_line_containing("bold", Duration::from_secs(10))
        .await;
    assert!(
        line.contains('\x02'),
        "expected IRC bold control code (\\x02) in: {line:?}"
    );

    capture.assert_no_warnings_or_errors();
}
