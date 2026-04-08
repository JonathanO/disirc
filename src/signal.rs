use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Control events
// ---------------------------------------------------------------------------

/// Events sent from the OS signal handler to the main processing task.
#[derive(Debug, PartialEq)]
pub enum ControlEvent {
    /// Config file should be reloaded (`SIGHUP` received on Unix).
    Reload,
    /// Graceful shutdown requested (`SIGTERM` or `SIGINT`).
    Shutdown,
}

// ---------------------------------------------------------------------------
// Signal handler
// ---------------------------------------------------------------------------

/// Spawn a background task that listens for OS signals and forwards them as
/// [`ControlEvent`]s. Returns the receiving end of the channel.
///
/// On Unix, `SIGHUP` sends [`ControlEvent::Reload`].
/// On non-Unix platforms, `SIGHUP` is not available; the returned receiver
/// will never yield an event (reload is not supported on those platforms in v1).
pub fn spawn_signal_handler() -> mpsc::Receiver<ControlEvent> {
    let (tx, rx) = mpsc::channel(1);

    #[cfg(unix)]
    tokio::spawn(unix_signal_loop(tx));

    #[cfg(not(unix))]
    tokio::spawn(non_unix_signal_loop(tx));

    rx
}

#[cfg(unix)]
// mutants::skip — platform-specific; tested via SIGHUP integration test on Unix only
#[mutants::skip]
async fn unix_signal_loop(tx: mpsc::Sender<ControlEvent>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to install SIGHUP handler: {e}");
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to install SIGTERM handler: {e}");
            return;
        }
    };
    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to install SIGINT handler: {e}");
            return;
        }
    };

    loop {
        let event = tokio::select! {
            _ = sighup.recv() => {
                tracing::info!("SIGHUP received — queuing config reload");
                ControlEvent::Reload
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received — initiating graceful shutdown");
                ControlEvent::Shutdown
            }
            _ = sigint.recv() => {
                tracing::info!("SIGINT received — initiating graceful shutdown");
                ControlEvent::Shutdown
            }
        };
        let is_shutdown = event == ControlEvent::Shutdown;
        if tx.send(event).await.is_err() {
            break;
        }
        if is_shutdown {
            break;
        }
    }
}

/// Non-Unix: only Ctrl-C (SIGINT equivalent) is available.
#[cfg(not(unix))]
// mutants::skip — platform-specific signal handling
#[mutants::skip]
async fn non_unix_signal_loop(tx: mpsc::Sender<ControlEvent>) {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("Ctrl-C received — initiating graceful shutdown");
        let _ = tx.send(ControlEvent::Shutdown).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::time::Duration;

    /// On non-Unix platforms, the signal handler waits for Ctrl-C.
    /// Verify the channel stays open (handler is running, not dropped).
    #[cfg(not(unix))]
    #[tokio::test]
    async fn non_unix_handler_keeps_channel_open() {
        let mut rx = spawn_signal_handler();
        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            result.is_err(),
            "channel should stay open (handler waiting for Ctrl-C)"
        );
    }

    /// On Unix, SIGHUP causes a Reload event.
    #[cfg(unix)]
    #[tokio::test]
    async fn sighup_sends_reload_event() {
        let mut rx = spawn_signal_handler();

        // Send SIGHUP to ourselves after a short delay.
        let pid = std::process::id();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            std::process::Command::new("kill")
                .args(["-s", "HUP", &pid.to_string()])
                .status()
                .expect("failed to send SIGHUP");
        });

        let result = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(
            matches!(result, Ok(Some(ControlEvent::Reload))),
            "expected Reload event after SIGHUP, got {result:?}"
        );
    }
}
