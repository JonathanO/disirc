//! Bridge processing: routing, state management, and message relay.
//!
//! This module is split by concern:
//! - [`map`] — `BridgeMap` and `BridgeInfo` (bidirectional channel routing).
//! - [`relay`] — Message format conversion between Discord and IRC commands.
//! - [`state`] — IRC and Discord lifecycle state tracking.
//! - [`routing`] — Message routing, burst generation, and guild channel mapping.
//! - [`orchestrator`] — Stateful event handler (`BridgeState`).

mod map;
pub mod orchestrator;
mod relay;
mod routing;
mod state;
#[cfg(test)]
mod test_util;

use tokio::sync::mpsc;

use crate::config::Config;
use crate::discord::{DiscordCommand, DiscordEvent};
use crate::irc::{S2SCommand, S2SEvent};
use crate::signal::ControlEvent;

// ---------------------------------------------------------------------------
// Re-exports — preserve the public API of `crate::bridge::*`
// ---------------------------------------------------------------------------

pub use map::{BridgeInfo, BridgeMap};
pub use orchestrator::BridgeState;
pub use relay::{discord_to_irc_commands, irc_to_discord_command};
pub use routing::{
    produce_burst_commands, route_discord_to_irc, route_dm_to_irc, route_irc_to_discord,
    route_irc_to_dm, update_guild_irc_channels,
};
pub use state::{DiscordState, IrcState, apply_discord_event, apply_irc_event};

// ---------------------------------------------------------------------------
// Bridge loop
// ---------------------------------------------------------------------------

/// Load persisted seed state from the configured state file, if any.
///
/// Returns an empty map if persistence is disabled, the file doesn't exist,
/// or the file is corrupt.  Errors are logged as warnings.
// mutants::skip — I/O + config plumbing; tested via integration tests
#[mutants::skip]
fn load_seed_state(
    config: &Config,
) -> std::collections::HashMap<u64, crate::persist::PersistedPseudoclient> {
    let Some(ref path_str) = config.pseudoclients.state_file else {
        return std::collections::HashMap::new();
    };
    let path = std::path::Path::new(path_str);
    match crate::persist::load_state(path) {
        Ok(state) => {
            let valid_channels: Vec<&str> = config
                .bridges
                .iter()
                .map(|b| b.irc_channel.as_str())
                .collect();
            let seed = crate::persist::into_seed_map(state, &valid_channels);
            tracing::info!(
                path = %path.display(),
                pseudoclients = seed.len(),
                "Loaded persisted state"
            );
            seed
        }
        Err(crate::persist::PersistError::Io(ref e))
            if e.kind() == std::io::ErrorKind::NotFound =>
        {
            tracing::info!(path = %path.display(), "No persisted state file — starting fresh");
            std::collections::HashMap::new()
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "Failed to load persisted state — starting fresh");
            std::collections::HashMap::new()
        }
    }
}

/// Save persisted state to disk if the dirty flag is set.
// mutants::skip — I/O wrapper; tested via integration tests
#[mutants::skip]
fn maybe_save_state(bridge: &mut BridgeState) {
    if !bridge.state_dirty {
        return;
    }
    let Some(ref path_str) = bridge.config.pseudoclients.state_file else {
        return;
    };
    let snapshot = crate::persist::snapshot_from_pm(&bridge.pm);
    let path = std::path::Path::new(path_str);
    if let Err(e) = crate::persist::save_state(path, &snapshot) {
        tracing::warn!(path = %path.display(), error = %e, "Failed to save state");
    } else {
        tracing::debug!(path = %path.display(), "State saved");
        bridge.state_dirty = false;
    }
}

/// Current Unix timestamp in seconds.
// mutants::skip — non-deterministic clock function; cannot be tested deterministically
#[mutants::skip]
fn unix_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Main bridge processing loop.
///
/// Thin async dispatcher that receives events from IRC and Discord, delegates
/// to [`BridgeState`] for processing, and forwards the resulting commands.
///
/// Runs until both event channels close (which happens when the connection
/// tasks exit).
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
    let seed_state = load_seed_state(config);
    let mut bridge = BridgeState::new(config, seed_state);
    let mut control_alive = true;
    let mut idle_tick = tokio::time::interval(std::time::Duration::from_mins(1));

    loop {
        tokio::select! {
            maybe_event = irc_event_rx.recv() => {
                let Some(event) = maybe_event else { break };
                let output = bridge.handle_irc_event(&event, unix_now());
                for cmd in output.irc_commands {
                    let _ = irc_cmd_tx.send(cmd).await;
                }
                for cmd in output.discord_commands {
                    let _ = discord_cmd_tx.send(cmd).await;
                }
            }

            maybe_event = discord_event_rx.recv() => {
                let Some(event) = maybe_event else { break };
                let output = bridge.handle_discord_event(&event, unix_now());
                for cmd in output.irc_commands {
                    let _ = irc_cmd_tx.send(cmd).await;
                }
                for cmd in output.discord_commands {
                    let _ = discord_cmd_tx.send(cmd).await;
                }
            }

            _ = idle_tick.tick() => {
                let output = bridge.check_idle_timeouts(unix_now());
                for cmd in output.irc_commands {
                    let _ = irc_cmd_tx.send(cmd).await;
                }
                maybe_save_state(&mut bridge);
            }

            maybe_ctrl = control_rx.recv(), if control_alive => {
                match maybe_ctrl {
                    Some(ControlEvent::Reload) => {
                        match crate::config::reload(config_path, &bridge.config) {
                            Ok((new_config, _diff)) => {
                                if let Some(cmd) = bridge.reload_config(new_config) {
                                    let _ = discord_cmd_tx.send(cmd).await;
                                }
                                tracing::info!("Config reloaded");
                            }
                            Err(e) => {
                                tracing::warn!("Config reload failed: {e}");
                            }
                        }
                    }
                    Some(ControlEvent::Shutdown) => { break; }
                    None => { control_alive = false; }
                }
            }
        }
    }

    // Final save on clean shutdown.
    bridge.state_dirty = true;
    maybe_save_state(&mut bridge);
}
