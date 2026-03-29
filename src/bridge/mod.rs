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

/// Null resolver: no IRC mention conversion.
struct NoopIrcResolver;
// mutants::skip — trivial test-only stub returning None
#[mutants::skip]
impl IrcMentionResolver for NoopIrcResolver {
    fn resolve_nick(&self, _: &str) -> Option<String> {
        None
    }
}

/// Null resolver: no Discord mention conversion.
struct NoopDiscordResolver;
// mutants::skip — trivial test-only stub returning None
#[mutants::skip]
impl DiscordResolver for NoopDiscordResolver {
    fn resolve_user(&self, _: &str) -> Option<String> {
        None
    }
    fn resolve_channel(&self, _: &str) -> Option<String> {
        None
    }
    fn resolve_role(&self, _: &str) -> Option<String> {
        None
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

    loop {
        tokio::select! {
            maybe_event = irc_event_rx.recv() => {
                let Some(event) = maybe_event else { break };

                match &event {
                    S2SEvent::LinkUp => {
                        let now = unix_now();
                        for cmd in produce_burst_commands(&pm, &irc_state, now) {
                            let _ = irc_cmd_tx.send(cmd).await;
                        }
                    }
                    S2SEvent::MessageReceived { from_uid, target, text, timestamp } => {
                        if let Some(cmd) = route_irc_to_discord(
                            &pm, &bridge_map, &irc_state,
                            from_uid, target, text, false, &NoopIrcResolver,
                        ) {
                            let _ = discord_cmd_tx.send(cmd).await;
                        }
                        // TODO: thread `timestamp` (IRC server-time) through to
                        // the Discord send path for accurate message timing.
                        let _ = timestamp;
                    }
                    S2SEvent::NoticeReceived { from_uid, target, text } => {
                        if let Some(cmd) = route_irc_to_discord(
                            &pm, &bridge_map, &irc_state,
                            from_uid, target, text, true, &NoopIrcResolver,
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
                    let cmds = route_discord_to_irc(
                        &mut pm, &bridge_map, &discord_state, &irc_state,
                        *channel_id, *author_id, author_name, content, attachments,
                        None, now, &NoopDiscordResolver,
                    );
                    for cmd in cmds {
                        let _ = irc_cmd_tx.send(cmd).await;
                    }
                }

                let now = unix_now();
                let cmds = apply_discord_event(&mut discord_state, &mut pm, &irc_state, &event, now);
                for cmd in cmds {
                    let _ = irc_cmd_tx.send(cmd).await;
                }
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
