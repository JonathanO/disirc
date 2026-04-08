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

    // Abort connection tasks — they run reconnect-forever loops and won't
    // stop on their own.  The bridge loop has already saved state.
    irc_handle.abort();
    discord_handle.abort();

    tracing::info!("disirc shutting down");
}
