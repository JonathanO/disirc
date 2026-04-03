use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use disirc::bridge::run_bridge;
use disirc::config;
use disirc::discord::connection::run_discord;
use disirc::irc::unreal::run_connection;
use disirc::signal::spawn_signal_handler;

// mutants::skip — entry point requiring full runtime environment
#[mutants::skip]
#[tokio::main]
async fn main() {
    // --- Logging ---
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // --- Config ---
    let config_path = config::config_path_from_args();
    let cfg = match config::load_and_validate(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to load config: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        "disirc starting"
    );

    // --- Channels ---
    // Buffer of 256 commands/events each direction; the bridge loop drains them promptly.
    let (irc_event_tx, irc_event_rx) = mpsc::channel(256);
    let (irc_cmd_tx, irc_cmd_rx) = mpsc::channel(256);
    let (discord_event_tx, discord_event_rx) = mpsc::channel(256);
    let (discord_cmd_tx, discord_cmd_rx) = mpsc::channel(256);

    // --- Signal handler ---
    let control_rx = spawn_signal_handler();

    // --- Spawn IRC connection task ---
    let irc_handle = {
        let irc_config = cfg.irc.clone();
        tokio::spawn(async move {
            run_connection(&irc_config, irc_cmd_rx, irc_event_tx).await;
        })
    };

    // --- Spawn Discord connection task ---
    let discord_handle = {
        let discord_config = cfg.discord.clone();
        let bridges = cfg.bridges.clone();
        tokio::spawn(async move {
            run_discord(&discord_config, &bridges, discord_event_tx, discord_cmd_rx).await;
        })
    };

    // --- Bridge loop (runs until both channels close or a signal fires) ---
    run_bridge(
        &cfg,
        &config_path,
        irc_event_rx,
        irc_cmd_tx,
        discord_event_rx,
        discord_cmd_tx,
        control_rx,
    )
    .await;

    // Check spawned tasks for panics — await them with a short timeout
    // so we catch panics that are still unwinding when run_bridge exits.
    for (name, handle) in [("IRC", irc_handle), ("Discord", discord_handle)] {
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        match result {
            Ok(Err(e)) if e.is_panic() => {
                // Extract the actual panic message from the payload.
                let panic = e.into_panic();
                let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic".to_string()
                };
                tracing::error!("{name} task panicked: {msg}");
            }
            Ok(Err(e)) => tracing::error!("{name} task failed: {e}"),
            // Ok(Ok(())) — task exited cleanly.
            // Err(_) — task still running after timeout (normal for IRC reconnect loop).
            _ => {}
        }
    }

    tracing::info!("disirc shutting down");
}
