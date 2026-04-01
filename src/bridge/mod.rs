//! Bridge processing: routing, state management, and message relay.
//!
//! This module is split by concern:
//! - [`map`] — `BridgeMap` and `BridgeInfo` (bidirectional channel routing).
//! - [`relay`] — Message format conversion between Discord and IRC commands.
//! - [`state`] — IRC and Discord lifecycle state tracking.
//! - [`routing`] — Message routing, burst generation, and guild channel mapping.

mod map;
mod relay;
mod routing;
mod state;

use tokio::sync::mpsc;

use crate::config::Config;
use crate::discord::{DiscordCommand, DiscordEvent};
use crate::formatting::{DiscordResolver, IrcMentionResolver};
use crate::irc::{S2SCommand, S2SEvent};
use crate::pseudoclients::PseudoclientManager;
use crate::signal::ControlEvent;

// ---------------------------------------------------------------------------
// Re-exports — preserve the public API of `crate::bridge::*`
// ---------------------------------------------------------------------------

pub use map::{BridgeInfo, BridgeMap};
pub use relay::{discord_to_irc_commands, irc_to_discord_command};
pub use routing::{
    produce_burst_commands, route_discord_to_irc, route_irc_to_discord, update_guild_irc_channels,
};
pub use state::{DiscordState, IrcState, apply_discord_event, apply_irc_event};

// ---------------------------------------------------------------------------
// Bridge loop helpers
// ---------------------------------------------------------------------------

/// Resolves IRC `@nick` to Discord `<@user_id>` mentions using the
/// pseudoclient manager's `nick→discord_id` mapping.
struct BridgeIrcResolver<'a> {
    pm: &'a PseudoclientManager,
}

impl IrcMentionResolver for BridgeIrcResolver<'_> {
    fn resolve_nick(&self, nick: &str) -> Option<String> {
        let state = self.pm.get_by_nick(nick)?;
        Some(state.discord_user_id.to_string())
    }
}

/// Resolves Discord mentions (`<@id>`, `<#id>`, `<@&id>`) to display names
/// using the bridge's cached guild data.
struct BridgeDiscordResolver<'a> {
    discord_state: &'a DiscordState,
}

impl DiscordResolver for BridgeDiscordResolver<'_> {
    fn resolve_user(&self, id: &str) -> Option<String> {
        let uid: u64 = id.parse().ok()?;
        self.discord_state.display_names.get(&uid).cloned()
    }
    fn resolve_channel(&self, id: &str) -> Option<String> {
        let cid: u64 = id.parse().ok()?;
        self.discord_state.channel_names.get(&cid).cloned()
    }
    fn resolve_role(&self, id: &str) -> Option<String> {
        let rid: u64 = id.parse().ok()?;
        self.discord_state.role_names.get(&rid).cloned()
    }
}

// ---------------------------------------------------------------------------
// Bridge loop
// ---------------------------------------------------------------------------

/// Current Unix timestamp in seconds.
// mutants::skip — non-deterministic clock function; cannot be tested deterministically
#[mutants::skip]
fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Main bridge processing loop.
///
/// Owns `PseudoclientManager`, `IrcState`, and `DiscordState`.  Runs until
/// both event channels close (which happens when the connection tasks exit).
///
/// - `config_path` — path to the config file, used for hot-reload on
///   `ControlEvent::Reload`.
// mutants::skip — requires live IRC + Discord connections to exercise
#[mutants::skip]
pub async fn run_bridge(
    config: &Config,
    config_path: &std::path::Path,
    mut irc_event_rx: mpsc::Receiver<S2SEvent>,
    irc_cmd_tx: mpsc::Sender<S2SCommand>,
    mut discord_event_rx: mpsc::Receiver<DiscordEvent>,
    discord_cmd_tx: mpsc::Sender<DiscordCommand>,
    mut control_rx: mpsc::Receiver<ControlEvent>,
) {
    let mut current_config = config.clone();
    let mut bridge_map = BridgeMap::from_config(&config.bridges);
    let mut pm = PseudoclientManager::new(
        &config.irc.sid,
        &config.pseudoclients.ident,
        &config.pseudoclients.host_suffix,
    );
    let mut irc_state = IrcState::default();
    let mut discord_state = DiscordState::default();
    // Track whether we have sent the initial S2S burst for this link.
    // Reset on LinkDown so the next LinkUp can burst again.
    let mut burst_sent = false;

    loop {
        tokio::select! {
            maybe_event = irc_event_rx.recv() => {
                let Some(event) = maybe_event else { break };

                match &event {
                    S2SEvent::LinkUp => {
                        // Burst only if we already have Discord member data.
                        // If guild_create hasn't arrived yet, `pm` is empty and
                        // we defer the burst until the first MemberSnapshot.
                        if !pm.is_empty() {
                            let now = unix_now();
                            for cmd in produce_burst_commands(&pm, &irc_state, now) {
                                let _ = irc_cmd_tx.send(cmd).await;
                            }
                            burst_sent = true;
                        }
                    }
                    S2SEvent::LinkDown { .. } => {
                        burst_sent = false;
                    }
                    S2SEvent::MessageReceived { from_uid, target, text, timestamp } => {
                        let resolver = BridgeIrcResolver { pm: &pm };
                        if let Some(cmd) = route_irc_to_discord(
                            &pm, &bridge_map, &irc_state,
                            from_uid, target, text, false, &resolver,
                        ) {
                            let _ = discord_cmd_tx.send(cmd).await;
                        }
                        // TODO: thread `timestamp` (IRC server-time) through to
                        // the Discord send path for accurate message timing.
                        let _ = timestamp;
                    }
                    S2SEvent::NoticeReceived { from_uid, target, text } => {
                        let resolver = BridgeIrcResolver { pm: &pm };
                        if let Some(cmd) = route_irc_to_discord(
                            &pm, &bridge_map, &irc_state,
                            from_uid, target, text, true, &resolver,
                        ) {
                            let _ = discord_cmd_tx.send(cmd).await;
                        }
                    }
                    _ => {}
                }

                apply_irc_event(&mut irc_state, &mut pm, &event);
            }

            maybe_event = discord_event_rx.recv() => {
                let Some(event) = maybe_event else { break };

                // Populate guild→irc-channel map before apply_discord_event uses it.
                if let DiscordEvent::MemberSnapshot { guild_id, channel_ids, .. } = &event {
                    update_guild_irc_channels(&mut discord_state, &bridge_map, *guild_id, channel_ids);
                }

                // Route Discord messages to IRC before state update.
                if let DiscordEvent::MessageReceived {
                    channel_id,
                    author_id,
                    author_name,
                    content,
                    attachments,
                } = &event
                {
                    let now = unix_now();
                    let resolver = BridgeDiscordResolver { discord_state: &discord_state };
                    let cmds = route_discord_to_irc(
                        &mut pm, &bridge_map, &discord_state, &irc_state,
                        *channel_id, *author_id, author_name, content, attachments,
                        None, now, &resolver,
                    );
                    for cmd in cmds {
                        let _ = irc_cmd_tx.send(cmd).await;
                    }
                }

                let now = unix_now();
                let cmds = apply_discord_event(&mut discord_state, &mut pm, &irc_state, &event, now);

                if irc_state.is_link_up() {
                    // If this is the first MemberSnapshot and we deferred the
                    // burst (because the link came up before guild_create), send
                    // the burst now — it includes all the members we just learned
                    // about.  The `cmds` from apply_discord_event are individual
                    // introduces; the burst is a superset, so we skip `cmds` and
                    // burst instead to avoid duplicates.
                    if matches!(&event, DiscordEvent::MemberSnapshot { .. }) && !burst_sent {
                        let now = unix_now();
                        for cmd in produce_burst_commands(&pm, &irc_state, now) {
                            let _ = irc_cmd_tx.send(cmd).await;
                        }
                        burst_sent = true;
                    } else {
                        // Normal path: forward live commands.
                        for cmd in cmds {
                            let _ = irc_cmd_tx.send(cmd).await;
                        }
                    }
                }
                // If link is not up, commands are suppressed.  pm state was
                // already updated by apply_discord_event; produce_burst_commands
                // will include these members when the link comes up.
            }

            maybe_ctrl = control_rx.recv() => {
                match maybe_ctrl {
                    Some(ControlEvent::Reload) => {
                        match crate::config::reload(config_path, &current_config) {
                            Ok((new_config, diff)) => {
                                if !diff.is_empty() {
                                    let added_ids: Vec<u64> = diff
                                        .added
                                        .iter()
                                        .chain(diff.webhook_changed.iter())
                                        .filter_map(|e| e.discord_channel_id.parse().ok())
                                        .collect();
                                    let removed_ids: Vec<u64> = diff
                                        .removed
                                        .iter()
                                        .filter_map(|e| e.discord_channel_id.parse().ok())
                                        .collect();
                                    let added_webhook_ids: Vec<u64> = diff
                                        .added
                                        .iter()
                                        .chain(diff.webhook_changed.iter())
                                        .filter_map(|e| {
                                            e.webhook_url.as_deref()
                                                .and_then(crate::discord::webhook_id_from_url)
                                        })
                                        .collect();
                                    let removed_webhook_ids: Vec<u64> = diff
                                        .removed
                                        .iter()
                                        .chain(diff.webhook_changed.iter())
                                        .filter_map(|e| {
                                            e.webhook_url.as_deref()
                                                .and_then(crate::discord::webhook_id_from_url)
                                        })
                                        .collect();
                                    let _ = discord_cmd_tx
                                        .send(DiscordCommand::ReloadBridges {
                                            added_channel_ids: added_ids,
                                            removed_channel_ids: removed_ids,
                                            added_webhook_ids,
                                            removed_webhook_ids,
                                        })
                                        .await;
                                    bridge_map = BridgeMap::from_config(&new_config.bridges);
                                }
                                current_config = new_config;
                                tracing::info!("Config reloaded");
                            }
                            Err(e) => {
                                tracing::warn!("Config reload failed: {e}");
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tokio::sync::mpsc;
    use tokio::time::Duration;

    use crate::config::{BridgeEntry, Config, DiscordConfig, IrcConfig, PseudoclientConfig};
    use crate::discord::{DiscordEvent, DiscordPresence, MemberInfo};
    use crate::irc::{S2SCommand, S2SEvent};
    use crate::signal::ControlEvent;

    use super::run_bridge;

    fn test_config() -> Config {
        Config {
            discord: DiscordConfig { token: "x".into() },
            irc: IrcConfig {
                uplink: "localhost".into(),
                port: 6667,
                tls: false,
                link_name: "bridge.test".into(),
                link_password: "pw".into(),
                sid: "002".into(),
                description: "test".into(),
            },
            pseudoclients: PseudoclientConfig {
                host_suffix: "test.net".into(),
                ident: "discord".into(),
            },
            bridges: vec![BridgeEntry {
                discord_channel_id: "111".into(),
                irc_channel: "#test".into(),
                webhook_url: None,
            }],
        }
    }

    /// Helper: spin up `run_bridge` and return the channel ends, keeping the
    /// join handle alive for the duration of the test.
    fn spawn_bridge() -> (
        mpsc::Sender<S2SEvent>,
        mpsc::Receiver<S2SCommand>,
        mpsc::Sender<DiscordEvent>,
        mpsc::Sender<ControlEvent>,
        tokio::task::JoinHandle<()>,
    ) {
        let (irc_event_tx, irc_event_rx) = mpsc::channel::<S2SEvent>(64);
        let (irc_cmd_tx, irc_cmd_rx) = mpsc::channel::<S2SCommand>(64);
        let (discord_event_tx, discord_event_rx) = mpsc::channel::<DiscordEvent>(64);
        let (discord_cmd_tx, _discord_cmd_rx) = mpsc::channel(64);
        let (control_tx, control_rx) = mpsc::channel::<ControlEvent>(4);

        let config = test_config();
        let handle = tokio::spawn(async move {
            run_bridge(
                &config,
                Path::new("/dev/null"),
                irc_event_rx,
                irc_cmd_tx,
                discord_event_rx,
                discord_cmd_tx,
                control_rx,
            )
            .await;
        });

        (
            irc_event_tx,
            irc_cmd_rx,
            discord_event_tx,
            control_tx,
            handle,
        )
    }

    /// Discord `MemberSnapshot` arriving before `LinkUp` must NOT produce any
    /// IRC commands (no `IntroduceUser` / `JoinChannel`).  The burst on
    /// `LinkUp` is the canonical introduction path; sending commands before the
    /// session starts would create duplicate UID introductions that UnrealIRCd
    /// would reject.
    #[tokio::test]
    async fn discord_snapshot_before_link_up_produces_no_irc_commands() {
        let (irc_event_tx, mut irc_cmd_rx, discord_event_tx, _ctrl, _handle) = spawn_bridge();

        // Send a MemberSnapshot with an online member — link is still down.
        discord_event_tx
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

        // Give the bridge a moment to process the event.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // No IRC commands should have been emitted yet.
        assert!(
            irc_cmd_rx.try_recv().is_err(),
            "expected no IRC commands before LinkUp, but one was sent"
        );

        // Now bring the link up — the burst MUST include Alice.
        irc_event_tx.send(S2SEvent::LinkUp).await.unwrap();

        // Collect all commands until BurstComplete.
        let mut cmds = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            assert!(!remaining.is_zero(), "timed out waiting for BurstComplete");
            match tokio::time::timeout(remaining, irc_cmd_rx.recv()).await {
                Ok(Some(cmd)) => {
                    let done = matches!(cmd, S2SCommand::BurstComplete);
                    cmds.push(cmd);
                    if done {
                        break;
                    }
                }
                _ => panic!("channel closed before BurstComplete"),
            }
        }

        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "burst must include IntroduceUser for Alice; got: {cmds:?}"
        );
    }

    /// When `LinkUp` fires with no member data, the burst is deferred.
    /// A subsequent `MemberSnapshot` triggers the deferred burst.
    #[tokio::test]
    async fn link_up_with_empty_pm_defers_burst_until_snapshot() {
        let (irc_event_tx, mut irc_cmd_rx, discord_event_tx, _ctrl, _handle) = spawn_bridge();

        // Bring the link up with no member data — burst should be deferred.
        irc_event_tx.send(S2SEvent::LinkUp).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            irc_cmd_rx.try_recv().is_err(),
            "no burst should fire when pm is empty on LinkUp"
        );

        // Now send a snapshot with Bob — this triggers the deferred burst.
        discord_event_tx
            .send(DiscordEvent::MemberSnapshot {
                guild_id: 999,
                members: vec![MemberInfo {
                    user_id: 1002,
                    display_name: "Bob".into(),
                    presence: DiscordPresence::Online,
                }],
                channel_ids: vec![111],
                channel_names: std::collections::HashMap::new(),
                role_names: std::collections::HashMap::new(),
            })
            .await
            .unwrap();

        // The deferred burst must include Bob and end with BurstComplete.
        let mut cmds = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            assert!(!remaining.is_zero(), "timed out waiting for BurstComplete");
            match tokio::time::timeout(remaining, irc_cmd_rx.recv()).await {
                Ok(Some(cmd)) => {
                    let done = matches!(cmd, S2SCommand::BurstComplete);
                    cmds.push(cmd);
                    if done {
                        break;
                    }
                }
                _ => panic!("channel closed before BurstComplete"),
            }
        }
        assert!(
            cmds.iter()
                .any(|c| matches!(c, S2SCommand::IntroduceUser { .. })),
            "deferred burst must include IntroduceUser for Bob; got: {cmds:?}"
        );
    }
}
