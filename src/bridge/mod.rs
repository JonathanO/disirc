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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BridgeEntry, DiscordConfig, FormattingConfig, IrcConfig, PseudoclientConfig,
    };
    use std::collections::HashMap;
    use std::fs;

    fn config_with_state_file(state_file: Option<String>) -> Config {
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
                connect_timeout: 15,
            },
            pseudoclients: PseudoclientConfig {
                ident: "discord".into(),
                reintroduce_on_kill: false,
                dm_bridging: true,
                channel_idle_timeout_secs: 0,
                offline_timeout_secs: 0,
                state_file,
            },
            formatting: FormattingConfig::default(),
            bridges: vec![BridgeEntry {
                discord_channel_id: "111".into(),
                irc_channel: "#test".into(),
                webhook_url: None,
            }],
        }
    }

    // --- load_seed_state ---

    #[test]
    fn load_seed_state_none_path_returns_empty() {
        let config = config_with_state_file(None);
        let seed = load_seed_state(&config);
        assert!(seed.is_empty());
    }

    /// `NotFound` branch must return empty AND log at INFO level ("starting
    /// fresh" without an error).  The log-level distinction pins the match-guard
    /// mutants — both branches return the same value, only the log differs.
    #[test]
    #[tracing_test::traced_test]
    fn load_seed_state_missing_file_returns_empty_and_logs_info() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does_not_exist.json");
        let config = config_with_state_file(Some(path.to_string_lossy().into_owned()));
        let seed = load_seed_state(&config);
        assert!(seed.is_empty());
        assert!(
            logs_contain("No persisted state file"),
            "expected INFO 'No persisted state file' for missing file"
        );
        assert!(
            !logs_contain("Failed to load persisted state"),
            "must not log WARN 'Failed to load' for a plain missing file"
        );
    }

    /// Non-`NotFound` I/O errors must log at WARN, not INFO.  This catches the
    /// `guard-with-true` mutant on `e.kind() == NotFound` — with the mutation,
    /// any I/O error takes the `NotFound` arm and logs INFO instead.  We trigger
    /// a non-`NotFound` I/O error by pointing at a directory rather than a file
    /// (yields `IsADirectory` on Linux).
    #[test]
    #[tracing_test::traced_test]
    fn load_seed_state_io_error_other_than_not_found_logs_warn() {
        let tmp = tempfile::tempdir().unwrap();
        // Path IS the directory; read_to_string on it returns an I/O error
        // whose kind is *not* NotFound.
        let path = tmp.path().to_path_buf();
        let config = config_with_state_file(Some(path.to_string_lossy().into_owned()));
        let seed = load_seed_state(&config);
        assert!(seed.is_empty());
        assert!(
            logs_contain("Failed to load persisted state"),
            "expected WARN for non-NotFound I/O error"
        );
        assert!(
            !logs_contain("No persisted state file"),
            "must not log INFO 'No persisted state file' for non-NotFound I/O error"
        );
    }

    /// Non-NotFound errors (e.g. corrupt JSON) must return empty AND log at
    /// WARN level.  This paired with the missing-file test catches the
    /// match-guard mutants on `e.kind() == NotFound`.
    #[test]
    #[tracing_test::traced_test]
    fn load_seed_state_corrupt_file_returns_empty_and_logs_warn() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        fs::write(&path, "not valid json {{{").unwrap();
        let config = config_with_state_file(Some(path.to_string_lossy().into_owned()));
        let seed = load_seed_state(&config);
        assert!(seed.is_empty());
        assert!(
            logs_contain("Failed to load persisted state"),
            "expected WARN 'Failed to load persisted state' for corrupt file"
        );
        assert!(
            !logs_contain("No persisted state file"),
            "must not log INFO 'No persisted state file' for a corrupt file"
        );
    }

    #[test]
    fn load_seed_state_valid_file_returns_filtered_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        // #test is configured; #other is not — must be filtered out.
        let json = r##"{
            "version": 1,
            "pseudoclients": {
                "42": {
                    "channels": ["#test", "#other"],
                    "last_active": 100,
                    "channel_last_active": {"#test": 90, "#other": 80},
                    "went_offline_at": null
                }
            }
        }"##;
        fs::write(&path, json).unwrap();
        let config = config_with_state_file(Some(path.to_string_lossy().into_owned()));
        let seed = load_seed_state(&config);
        assert_eq!(seed.len(), 1);
        let pc = seed.get(&42).expect("user 42 present");
        assert_eq!(pc.channels, vec!["#test".to_string()]);
        assert!(pc.channel_last_active.contains_key("#test"));
        assert!(!pc.channel_last_active.contains_key("#other"));
    }

    // --- maybe_save_state ---

    #[test]
    fn maybe_save_state_noop_when_not_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let config = config_with_state_file(Some(path.to_string_lossy().into_owned()));
        let mut bridge = BridgeState::new(&config, HashMap::new());
        // state_dirty is false by default.
        maybe_save_state(&mut bridge);
        assert!(
            !path.exists(),
            "no file should be written when state_dirty is false"
        );
    }

    #[test]
    fn maybe_save_state_noop_when_state_file_is_none() {
        let config = config_with_state_file(None);
        let mut bridge = BridgeState::new(&config, HashMap::new());
        bridge.state_dirty = true;
        // Must not panic when path is None.
        maybe_save_state(&mut bridge);
        // state_dirty should remain true since there was nothing to save to.
        assert!(bridge.state_dirty);
    }

    #[test]
    fn maybe_save_state_writes_and_clears_dirty_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.json");
        let config = config_with_state_file(Some(path.to_string_lossy().into_owned()));
        let mut bridge = BridgeState::new(&config, HashMap::new());
        bridge.state_dirty = true;
        maybe_save_state(&mut bridge);
        assert!(path.exists(), "file should be written when dirty");
        assert!(
            !bridge.state_dirty,
            "dirty flag should be cleared on success"
        );
    }

    #[test]
    fn maybe_save_state_write_failure_keeps_dirty_flag() {
        // Point at a path whose parent directory does not exist — write fails.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("missing_dir").join("state.json");
        let config = config_with_state_file(Some(path.to_string_lossy().into_owned()));
        let mut bridge = BridgeState::new(&config, HashMap::new());
        bridge.state_dirty = true;
        maybe_save_state(&mut bridge);
        assert!(
            bridge.state_dirty,
            "dirty flag should remain set when save fails"
        );
    }
}
