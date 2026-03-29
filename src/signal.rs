use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Control events
// ---------------------------------------------------------------------------

/// Events sent from the OS signal handler to the main processing task.
#[derive(Debug, PartialEq)]
pub enum ControlEvent {
    /// Config file should be reloaded (`SIGHUP` received on Unix).
    Reload,
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

    // On non-Unix: sender is moved into the above spawn or dropped here;
    // either way the receiver silently never yields.
    #[cfg(not(unix))]
    drop(tx);

    rx
}

#[cfg(unix)]
async fn unix_signal_loop(tx: mpsc::Sender<ControlEvent>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sighup = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to install SIGHUP handler: {e}");
            return;
        }
    };

    loop {
        sighup.recv().await;
        tracing::info!("SIGHUP received — queuing config reload");
        if tx.send(ControlEvent::Reload).await.is_err() {
            // Receiver dropped — main task has exited; stop the loop.
            break;
        }
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

    /// On non-Unix platforms the sender is dropped immediately, closing the
    /// channel. recv() returns None at once — no events, no blocking.
    /// The main loop simply never receives a reload event.
    #[cfg(not(unix))]
    #[tokio::test]
    async fn non_unix_channel_closes_immediately() {
        let mut rx = spawn_signal_handler();
        let result = rx.recv().await;
        assert_eq!(
            result, None,
            "expected closed channel (None) on non-Unix platform"
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
