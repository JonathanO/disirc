//! Bridge processing: routing, state management, and message relay.
//!
//! This module is split by concern:
//! - [`map`] — `BridgeMap` and `BridgeInfo` (bidirectional channel routing).
//! - [`relay`] — Message format conversion between Discord and IRC commands.
//! - [`state`] — IRC and Discord lifecycle state tracking.
//! - [`routing`] — Message routing, burst generation, and guild channel mapping.
//! - [`orchestrator`] — Stateful event handler (`BridgeState`).

mod map;
pub(crate) mod orchestrator;
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

pub(crate) use orchestrator::BridgeState;

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

    /// `unix_now` must return a real wall-clock epoch value rather than a
    /// constant.  The function is non-deterministic in its exact value, but it
    /// is tightly bounded, and a bounds assertion is all that is needed to pin
    /// the `map_or` success arm — an exact value is not required.
    ///
    /// This catches both a degenerate constant return and unit confusion
    /// (`as_millis`/`as_nanos` in place of `as_secs`).
    #[test]
    fn unix_now_returns_plausible_epoch_seconds() {
        // 2023-11-14T22:13:20Z — safely in the past, so this stays true forever.
        const PAST: u64 = 1_700_000_000;
        // 2286-11-20T17:46:40Z — far enough out never to bite in practice, but
        // low enough that a millisecond-scale value trips it.
        const FUTURE: u64 = 10_000_000_000;

        let now = unix_now();
        assert!(now > PAST, "unix_now() = {now}, expected > {PAST}");
        assert!(now < FUTURE, "unix_now() = {now}, expected < {FUTURE}");
    }

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

    // -----------------------------------------------------------------------
    // run_bridge
    //
    // The loop's whole interface is mpsc channels plus &Config / &Path — no
    // socket is involved — so it can be driven directly.  Every test bounds
    // the call in a timeout, because the failure mode for a dispatch loop is
    // hanging rather than asserting.
    // -----------------------------------------------------------------------

    use crate::discord::{DiscordPresence, MemberInfo};
    use crate::irc::S2SCommand;

    /// Sending ends for one `run_bridge` invocation.
    #[allow(clippy::struct_field_names)]
    struct Harness {
        irc_event_tx: mpsc::Sender<S2SEvent>,
        discord_event_tx: mpsc::Sender<DiscordEvent>,
        control_tx: mpsc::Sender<ControlEvent>,
    }

    /// Run `run_bridge` to completion against `setup`, which is handed the
    /// sending ends before the loop is awaited.  Panics if the loop does not
    /// exit within five seconds.
    ///
    /// Only for tests whose outcome does not depend on events being processed
    /// before exit — `select!` may take the exit branch first.  Tests that
    /// need an event's effect must drive the loop concurrently instead, as
    /// `events_are_dispatched_and_commands_forwarded` does.
    async fn run_bridge_until_exit(
        config: &Config,
        config_path: &std::path::Path,
        setup: impl FnOnce(&mut Harness),
    ) {
        let (irc_event_tx, irc_event_rx) = mpsc::channel(16);
        let (irc_cmd_tx, _irc_cmd_rx) = mpsc::channel(16);
        let (discord_event_tx, discord_event_rx) = mpsc::channel(16);
        let (discord_cmd_tx, _discord_cmd_rx) = mpsc::channel(16);
        let (control_tx, control_rx) = mpsc::channel(16);

        let mut harness = Harness {
            irc_event_tx,
            discord_event_tx,
            control_tx,
        };
        setup(&mut harness);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_bridge(
                config,
                config_path,
                irc_event_rx,
                irc_cmd_tx,
                discord_event_rx,
                discord_cmd_tx,
                control_rx,
            ),
        )
        .await;
        assert!(result.is_ok(), "run_bridge did not exit within 5s");
    }

    #[test]
    fn shutdown_control_event_exits_loop_and_saves_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state_path = tmp.path().join("state.json");
        let config = config_with_state_file(Some(state_path.to_string_lossy().into_owned()));
        let config_path = tmp.path().join("config.toml");

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            run_bridge_until_exit(&config, &config_path, |h| {
                h.control_tx.try_send(ControlEvent::Shutdown).unwrap();
            })
            .await;
        });

        assert!(
            state_path.exists(),
            "clean shutdown must force a final state save"
        );
    }

    #[test]
    fn closing_irc_event_channel_exits_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_state_file(None);
        let config_path = tmp.path().join("config.toml");

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            run_bridge_until_exit(&config, &config_path, |h| {
                // Replace the sender with a dropped one; recv() then yields None.
                let (dead, _) = mpsc::channel(1);
                h.irc_event_tx = dead;
            })
            .await;
        });
    }

    #[test]
    fn closing_discord_event_channel_exits_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_state_file(None);
        let config_path = tmp.path().join("config.toml");

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            run_bridge_until_exit(&config, &config_path, |h| {
                let (dead, _) = mpsc::channel(1);
                h.discord_event_tx = dead;
            })
            .await;
        });
    }

    /// A closed control channel must disable that select branch rather than
    /// exit the loop — otherwise `recv()` returning `None` would spin forever
    /// on a ready branch.  The loop still ends via the event channels.
    #[test]
    fn closed_control_channel_disables_branch_without_exiting_early() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_state_file(None);
        let config_path = tmp.path().join("config.toml");

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            run_bridge_until_exit(&config, &config_path, |h| {
                // Close control first, then the event channels.
                let (dead_ctrl, _) = mpsc::channel(1);
                h.control_tx = dead_ctrl;
                let (dead_irc, _) = mpsc::channel(1);
                h.irc_event_tx = dead_irc;
            })
            .await;
        });
    }

    /// Events from both directions must reach `BridgeState` and the resulting
    /// commands must be forwarded to the command channels.
    ///
    /// The loop is driven concurrently rather than pre-loaded and shut down:
    /// `select!` picks randomly among ready branches, so queueing Shutdown
    /// alongside the events lets the loop exit before processing them. The
    /// driver instead waits for the command to actually arrive, and only then
    /// requests shutdown.
    ///
    /// Whichever order `LinkUp` and `MemberSnapshot` are picked in, the member
    /// ends up introduced — via the burst if `LinkUp` lands second, via the
    /// live path if it lands first.
    #[test]
    fn events_are_dispatched_and_commands_forwarded() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_state_file(None);
        let config_path = tmp.path().join("config.toml");

        let found = tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (irc_event_tx, irc_event_rx) = mpsc::channel(16);
            let (irc_cmd_tx, mut irc_cmd_rx) = mpsc::channel(16);
            let (discord_event_tx, discord_event_rx) = mpsc::channel(16);
            let (discord_cmd_tx, _discord_cmd_rx) = mpsc::channel(16);
            let (control_tx, control_rx) = mpsc::channel(16);

            let driver = async {
                irc_event_tx.send(S2SEvent::LinkUp).await.unwrap();
                discord_event_tx
                    .send(DiscordEvent::MemberSnapshot {
                        guild_id: 999,
                        members: vec![MemberInfo {
                            user_id: 4001,
                            username: "Alice".into(),
                            display_name: "Alice".into(),
                            presence: DiscordPresence::Online,
                        }],
                        channel_ids: vec![111],
                        channel_names: HashMap::new(),
                        role_names: HashMap::new(),
                        bot_user_id: 0,
                    })
                    .await
                    .unwrap();

                // Wait for the effect before shutting the loop down.
                let found = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while let Some(cmd) = irc_cmd_rx.recv().await {
                        if matches!(cmd, S2SCommand::IntroduceUser { .. }) {
                            return true;
                        }
                    }
                    false
                })
                .await
                .unwrap_or(false);

                let _ = control_tx.send(ControlEvent::Shutdown).await;
                found
            };

            let ((), found) = tokio::join!(
                run_bridge(
                    &config,
                    &config_path,
                    irc_event_rx,
                    irc_cmd_tx,
                    discord_event_rx,
                    discord_cmd_tx,
                    control_rx,
                ),
                driver
            );
            found
        });

        assert!(
            found,
            "the online member should be introduced on IRC via the command channel"
        );
    }

    /// A failing reload must be logged and the loop must keep running, not
    /// abort. The config path here does not exist, so `config::reload` errors.
    #[test]
    #[tracing_test::traced_test]
    fn failed_reload_is_logged_and_loop_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_state_file(None);
        let missing = tmp.path().join("does_not_exist.toml");

        tokio::runtime::Runtime::new().unwrap().block_on(async {
            run_bridge_until_exit(&config, &missing, |h| {
                h.control_tx.try_send(ControlEvent::Reload).unwrap();
                // Shutdown afterwards proves the loop survived the failure.
                h.control_tx.try_send(ControlEvent::Shutdown).unwrap();
            })
            .await;
        });

        assert!(
            logs_contain("Config reload failed"),
            "a failing reload must be logged as a warning"
        );
    }
}
