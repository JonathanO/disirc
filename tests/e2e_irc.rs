//! Layer 3 e2e tests: real UnrealIRCd in Docker, Discord side mocked via mpsc.
//!
//! All tests are `#[ignore]` — they require Docker and are not run in the
//! normal `cargo test` suite. Run them explicitly with:
//!
//! ```text
//! cargo test --test e2e_irc -- --include-ignored --nocapture
//! ```

mod helpers;

use helpers::log_capture::init_capture_tracing;
use std::path::Path;
use tokio::sync::mpsc;
use tokio::time::Duration;

use disirc::bridge::run_bridge;
use disirc::config::{BridgeEntry, Config, DiscordConfig, IrcConfig, PseudoclientConfig};
use disirc::discord::{DiscordCommand, DiscordEvent, DiscordPresence, MemberInfo};
use disirc::irc::unreal::run_connection;
use disirc::irc::{S2SCommand, S2SEvent};
use disirc::signal::ControlEvent;

// ---------------------------------------------------------------------------
// Test infrastructure
// ---------------------------------------------------------------------------

/// Build a test config pointing at the given S2S port, with a single bridge
/// entry mapping Discord channel 111 ↔ IRC channel `#e2e-test`.
fn e2e_config(host: &str, s2s_port: u16) -> Config {
    Config {
        discord: DiscordConfig {
            token: "fake-token".into(),
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
        bridges: vec![BridgeEntry {
            discord_channel_id: "111".into(),
            irc_channel: "#e2e-test".into(),
            webhook_url: None,
        }],
    }
}

/// Wiring returned by [`spawn_bridge`].
struct BridgeTasks {
    /// Inject Discord events into the bridge.
    discord_event_tx: mpsc::Sender<DiscordEvent>,
    /// Capture Discord commands emitted by the bridge.
    discord_cmd_rx: mpsc::Receiver<DiscordCommand>,
    bridge_handle: tokio::task::JoinHandle<()>,
    conn_handle: tokio::task::JoinHandle<()>,
    /// Kept alive to prevent the bridge from treating a closed control channel
    /// as a shutdown signal. Dropped when `abort()` is called.
    _control_tx: mpsc::Sender<ControlEvent>,
}

impl BridgeTasks {
    /// Wait for a `DiscordCommand::SendMessage` whose `text` contains `needle`.
    /// Returns the full `text` field for further assertions.
    /// Ignores other commands. Panics on timeout.
    async fn expect_send_message(&mut self, needle: &str, timeout: Duration) -> String {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                panic!("timed out waiting for DiscordCommand::SendMessage containing {needle:?}");
            }
            match tokio::time::timeout(remaining, self.discord_cmd_rx.recv()).await {
                Ok(Some(DiscordCommand::SendMessage { text, .. })) if text.contains(needle) => {
                    return text;
                }
                Ok(Some(_)) => continue,
                _ => panic!(
                    "discord_cmd_rx closed before receiving SendMessage containing {needle:?}"
                ),
            }
        }
    }
}

impl Drop for BridgeTasks {
    fn drop(&mut self) {
        self.bridge_handle.abort();
        self.conn_handle.abort();
    }
}

/// Spawn `run_connection` and `run_bridge` wired together.
/// Returns handles plus the Discord-facing channel ends for test injection and
/// capture.
fn spawn_bridge(config: Config) -> BridgeTasks {
    let (irc_event_tx, irc_event_rx) = mpsc::channel::<S2SEvent>(256);
    let (irc_cmd_tx, irc_cmd_rx) = mpsc::channel::<S2SCommand>(256);
    let (discord_event_tx, discord_event_rx) = mpsc::channel::<DiscordEvent>(256);
    let (discord_cmd_tx, discord_cmd_rx) = mpsc::channel::<DiscordCommand>(256);
    // Keep the control sender alive for the duration of the test — dropping it
    // causes `run_bridge` to treat the closed channel as a shutdown signal.
    let (control_tx, control_rx) = mpsc::channel::<ControlEvent>(4);

    let config_for_bridge = config.clone();
    let bridge_handle = tokio::spawn(async move {
        run_bridge(
            &config_for_bridge,
            Path::new("/dev/null"),
            irc_event_rx,
            irc_cmd_tx,
            discord_event_rx,
            discord_cmd_tx,
            control_rx,
        )
        .await;
    });

    let config_for_conn = config;
    let conn_handle = tokio::spawn(async move {
        run_connection(&config_for_conn.irc, irc_cmd_rx, irc_event_tx).await;
    });

    BridgeTasks {
        discord_event_tx,
        discord_cmd_rx,
        bridge_handle,
        conn_handle,
        _control_tx: control_tx,
    }
}

/// Poll LINKS until `bridge_name` appears, retrying every 500ms.
/// Panics if `timeout_secs` elapses without success.
async fn wait_for_bridge_in_links(
    client: &mut helpers::TestIrcClient,
    bridge_name: &str,
    timeout_secs: u64,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        client.send("LINKS").await;
        // Read lines for up to 1s looking for the bridge name or end-of-links.
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
        if tokio::time::Instant::now() >= deadline {
            panic!("Timed out after {timeout_secs}s: {bridge_name:?} not found in LINKS");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify the bridge establishes an S2S link with UnrealIRCd.
/// The bridge's server name (`bridge.test.net`) should appear in LINKS.
#[tokio::test]
#[ignore]
async fn e2e_bridge_connects_to_unrealircd() {
    let capture = init_capture_tracing();
    let irc = helpers::start_unrealircd().await;
    let config = e2e_config(&irc.host, irc.s2s_port);
    let tasks = spawn_bridge(config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;

    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    capture.assert_no_warnings_or_errors();
    drop(tasks);
}

/// Inject a Discord message and verify the IRC client sees the corresponding
/// PRIVMSG in the bridged channel.
#[tokio::test]
#[ignore]
async fn e2e_discord_to_irc_message_relay() {
    let capture = init_capture_tracing();
    let irc = helpers::start_unrealircd().await;
    let config = e2e_config(&irc.host, irc.s2s_port);
    let tasks = spawn_bridge(config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join("#e2e-test").await;

    // Wait for the S2S link to be established.
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    // Introduce Alice as an online Discord user in the guild that owns channel 111.
    tasks
        .discord_event_tx
        .send(DiscordEvent::MemberSnapshot {
            guild_id: 999,
            members: vec![MemberInfo {
                user_id: 1001,
                display_name: "Alice".into(),
                presence: DiscordPresence::Online,
            }],
            channel_ids: vec![111],
            channel_names: std::collections::HashMap::new(),
            role_names: std::collections::HashMap::new(),
        })
        .await
        .unwrap();

    // Wait for any line mentioning Alice (NICK intro, JOIN, etc.) to confirm
    // her pseudoclient has been introduced to the network.
    client
        .expect_line_containing("Alice", Duration::from_secs(10))
        .await;

    // Now relay a Discord message from Alice.
    tasks
        .discord_event_tx
        .send(DiscordEvent::MessageReceived {
            channel_id: 111,
            author_id: 1001,
            author_name: "Alice".into(),
            content: "hello from discord".into(),
            attachments: vec![],
        })
        .await
        .unwrap();

    // The IRC client should see a PRIVMSG from Alice's pseudoclient.
    client
        .expect_privmsg("Alice", "hello from discord", Duration::from_secs(10))
        .await;

    capture.assert_no_warnings_or_errors();
    drop(tasks);
}

/// Send a PRIVMSG from a test IRC client and verify the bridge emits a
/// `DiscordCommand::SendMessage` for the bridged Discord channel.
#[tokio::test]
#[ignore]
async fn e2e_irc_to_discord_message_relay() {
    let capture = init_capture_tracing();
    let irc = helpers::start_unrealircd().await;
    let config = e2e_config(&irc.host, irc.s2s_port);
    let mut tasks = spawn_bridge(config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join("#e2e-test").await;

    // Introduce a pseudoclient so the bridge has a presence in #e2e-test and
    // therefore receives PRIVMSGs sent to that channel.
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    tasks
        .discord_event_tx
        .send(DiscordEvent::MemberSnapshot {
            guild_id: 999,
            members: vec![MemberInfo {
                user_id: 1002,
                display_name: "BridgeUser".into(),
                presence: DiscordPresence::Online,
            }],
            channel_ids: vec![111],
            channel_names: std::collections::HashMap::new(),
            role_names: std::collections::HashMap::new(),
        })
        .await
        .unwrap();

    // Wait for the pseudoclient JOIN to confirm the bridge is in the channel.
    client
        .expect_line_containing("BridgeUser", Duration::from_secs(10))
        .await;

    // Send an IRC message from the test client.
    client.send_privmsg("#e2e-test", "hello from irc").await;

    // The bridge should produce a DiscordCommand::SendMessage for channel 111.
    // Plain path (no webhook): text is "**[nick]** content" with exactly one prefix.
    let text = tasks
        .expect_send_message("hello from irc", Duration::from_secs(10))
        .await;
    assert!(
        text.starts_with("**["),
        "plain message text must start with **[nick]** prefix; got: {text:?}"
    );
    assert_eq!(
        text.matches("**[").count(),
        1,
        "plain message text must have exactly one **[nick]** prefix; got: {text:?}"
    );

    capture.assert_no_warnings_or_errors();
    drop(tasks);
}

/// Regression test: inject a `MemberSnapshot` **before** the S2S link is
/// established and verify the pseudoclient still appears on IRC via the burst.
///
/// Previously, pre-link commands were queued ahead of the burst, causing
/// UnrealIRCd to receive duplicate UID introductions and SQUIT the bridge.
/// After the fix the bridge suppresses live-introduce commands when the link
/// is down; `produce_burst_commands` on `LinkUp` is the sole introduction path.
#[tokio::test]
#[ignore]
async fn e2e_snapshot_before_link_up_still_appears_in_burst() {
    let capture = init_capture_tracing();
    let irc = helpers::start_unrealircd().await;
    let config = e2e_config(&irc.host, irc.s2s_port);
    let tasks = spawn_bridge(config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join("#e2e-test").await;

    // Inject MemberSnapshot immediately — before the S2S link is established.
    // The bridge has not yet seen LinkUp so it must not send UID/SJOIN now.
    tasks
        .discord_event_tx
        .send(DiscordEvent::MemberSnapshot {
            guild_id: 999,
            members: vec![MemberInfo {
                user_id: 3001,
                display_name: "EarlyUser".into(),
                presence: DiscordPresence::Online,
            }],
            channel_ids: vec![111],
            channel_names: std::collections::HashMap::new(),
            role_names: std::collections::HashMap::new(),
        })
        .await
        .unwrap();

    // Now wait for the link to come up (this triggers the burst).
    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    // EarlyUser must appear via the burst even though the snapshot arrived
    // before the link was established.
    client
        .expect_line_containing("EarlyUser", Duration::from_secs(10))
        .await;

    capture.assert_no_warnings_or_errors();
    drop(tasks);
}

/// Inject a `MemberSnapshot` and verify the pseudoclient's nick appears on IRC.
#[tokio::test]
#[ignore]
async fn e2e_pseudoclient_appears_on_irc() {
    let capture = init_capture_tracing();
    let irc = helpers::start_unrealircd().await;
    let config = e2e_config(&irc.host, irc.s2s_port);
    let tasks = spawn_bridge(config);

    let mut client =
        helpers::TestIrcClient::connect(&format!("{}:{}", irc.host, irc.client_port), "testbot")
            .await;
    client.join("#e2e-test").await;

    wait_for_bridge_in_links(&mut client, "bridge.test.net", 15).await;

    tasks
        .discord_event_tx
        .send(DiscordEvent::MemberSnapshot {
            guild_id: 999,
            members: vec![MemberInfo {
                user_id: 2001,
                display_name: "TestUser".into(),
                presence: DiscordPresence::Online,
            }],
            channel_ids: vec![111],
            channel_names: std::collections::HashMap::new(),
            role_names: std::collections::HashMap::new(),
        })
        .await
        .unwrap();

    // The test IRC client should see TestUser's pseudoclient JOIN #e2e-test.
    client
        .expect_line_containing("TestUser", Duration::from_secs(10))
        .await;

    capture.assert_no_warnings_or_errors();
    drop(tasks);
}
