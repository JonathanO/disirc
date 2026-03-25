// This module will be called from main.rs in a future task.
#![allow(dead_code)]

//! Full IRC server-link connection loop for UnrealIRCd S2S.
//!
//! The public entry point is [`run_connection`]. It never returns — on link
//! failure it emits `S2SEvent::LinkDown`, waits with full-jitter exponential
//! backoff, and reconnects.

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use tokio::sync::mpsc;

use crate::config::IrcConfig;
use crate::irc_message::{IrcCommand, IrcMessage};

use super::connect::{IrcReader, IrcWriter, connect};
use super::rate_limiter::TokenBucket;
use super::translation::{translate_inbound, translate_outbound};
use super::types::{S2SCommand, S2SEvent};

// ── Timing constants (overridden in tests) ─────────────────────────────────

/// How often to send a keepalive PING to the uplink.
const PING_INTERVAL: Duration = Duration::from_secs(90);

/// How long to wait for a PONG before declaring the link dead.
const PONG_TIMEOUT: Duration = Duration::from_secs(60);

/// A "never-fire" duration used to arm timers that shouldn't trigger yet.
const FAR_FUTURE: Duration = Duration::from_secs(86_400);

// ── Internal types ─────────────────────────────────────────────────────────

/// Timing parameters for the session loop (parameterised so tests can use
/// short durations without real sleeps).
struct SessionTimings {
    ping_interval: Duration,
    pong_timeout: Duration,
}

impl SessionTimings {
    fn production() -> Self {
        Self {
            ping_interval: PING_INTERVAL,
            pong_timeout: PONG_TIMEOUT,
        }
    }
}

/// Capabilities negotiated during the UnrealIRCd handshake.
struct HandshakeResult {
    /// The uplink's SID, extracted from its `PROTOCTL SID=` line.
    uplink_sid: String,
    /// Whether the uplink advertised `MTAGS` — gates `@time=` tag emission.
    mtags_active: bool,
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Compute the reconnect delay for the given attempt number (0-indexed).
///
/// Uses full-jitter exponential backoff: delay = `rand(0, min(5 × 2^attempt, 300))` seconds.
/// Keeping this `pub(crate)` allows unit-testing without exposing it as library API.
pub(crate) fn backoff_delay(attempt: u32) -> Duration {
    const CAP_SECS: u64 = 300;
    const BASE_SECS: u64 = 5;
    let exp = BASE_SECS.saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX));
    let capped = exp.min(CAP_SECS);
    let secs = if capped == 0 {
        0
    } else {
        rand::random::<u64>() % capped
    };
    Duration::from_secs(secs)
}

/// Run the IRC connection forever, reconnecting on failure.
///
/// Connects to `config.uplink:config.port`, runs the UnrealIRCd S2S
/// handshake, emits `S2SEvent::LinkUp`, then processes events until the link
/// fails. On failure emits `S2SEvent::LinkDown` and reconnects after a
/// full-jitter exponential backoff delay.
pub async fn run_connection(
    config: &IrcConfig,
    cmd_rx: mpsc::Receiver<S2SCommand>,
    event_tx: mpsc::Sender<S2SEvent>,
) -> ! {
    let mut cmd_rx = cmd_rx;
    let mut attempt: u32 = 0;
    loop {
        tracing::info!(
            attempt,
            uplink = %config.uplink,
            port = config.port,
            "Connecting to uplink"
        );
        match run_once(config, &mut cmd_rx, &event_tx).await {
            Ok(()) => {
                tracing::info!("Link closed cleanly");
            }
            Err(e) => {
                tracing::error!("Link error: {e:#}");
            }
        }
        // Drop any S2SCommands queued while the link was down — the
        // processing task will re-introduce its pseudoclients on the next LinkUp.
        while cmd_rx.try_recv().is_ok() {}

        let delay = backoff_delay(attempt);
        tracing::info!(
            attempt,
            delay_ms = delay.as_millis(),
            "Waiting before reconnect"
        );
        tokio::time::sleep(delay).await;
        attempt = attempt.saturating_add(1);
    }
}

// ── Internal implementation ────────────────────────────────────────────────

/// One full connection attempt: connect → handshake → session → error.
async fn run_once(
    config: &IrcConfig,
    cmd_rx: &mut mpsc::Receiver<S2SCommand>,
    event_tx: &mpsc::Sender<S2SEvent>,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = connect(&config.uplink, config.port, config.tls)
        .await
        .context("TCP/TLS connect failed")?;

    let hs = do_handshake(&mut reader, &mut writer, config)
        .await
        .context("Handshake failed")?;

    let _ = event_tx.send(S2SEvent::LinkUp).await;

    let result = run_session(
        reader,
        writer,
        hs,
        cmd_rx,
        event_tx,
        &config.sid,
        SessionTimings::production(),
    )
    .await;

    if let Err(ref e) = result {
        let _ = event_tx
            .send(S2SEvent::LinkDown {
                reason: e.to_string(),
            })
            .await;
    }
    result
}

/// Send our five-line S2S credential sequence to the uplink.
async fn send_credentials(writer: &mut IrcWriter, config: &IrcConfig) -> std::io::Result<()> {
    // 1. PASS :<link_password>
    writer
        .write_message(&IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Pass {
                password: config.link_password.clone(),
            },
        })
        .await?;

    // 2. PROTOCTL EAUTH=<link_name>   (must be first, alone on its own line)
    writer
        .write_message(&IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Protoctl {
                tokens: vec![format!("EAUTH={}", config.link_name)],
            },
        })
        .await?;

    // 3. PROTOCTL <capability tokens>
    writer
        .write_message(&IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Protoctl {
                tokens: vec![
                    "NOQUIT".into(),
                    "NICKv2".into(),
                    "SJOIN".into(),
                    "SJ3".into(),
                    "CLK".into(),
                    "TKLEXT2".into(),
                    "NICKIP".into(),
                    "ESVID".into(),
                    "MLOCK".into(),
                    "EXTSWHOIS".into(),
                    "MTAGS".into(),
                ],
            },
        })
        .await?;

    // 4. PROTOCTL SID=<our_sid>
    writer
        .write_message(&IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Protoctl {
                tokens: vec![format!("SID={}", config.sid)],
            },
        })
        .await?;

    // 5. SERVER <link_name> 1 :<description>
    writer
        .write_message(&IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Server {
                name: config.link_name.clone(),
                hop_count: 1,
                description: config.description.clone(),
            },
        })
        .await?;

    Ok(())
}

/// Run the UnrealIRCd S2S handshake and return the negotiated capabilities.
///
/// Sends our credentials, then reads the uplink's credentials until `SERVER`
/// is received. Exits the process (fatal) if the uplink sends the wrong
/// password or an `ERROR`.
async fn do_handshake(
    reader: &mut IrcReader,
    writer: &mut IrcWriter,
    config: &IrcConfig,
) -> anyhow::Result<HandshakeResult> {
    send_credentials(writer, config)
        .await
        .context("Sending handshake credentials")?;

    let mut uplink_sid = String::new();
    let mut mtags_active = false;
    let mut pass_seen = false;

    loop {
        let line = reader
            .next_line()
            .await
            .context("Read during handshake")?
            .ok_or_else(|| anyhow::anyhow!("EOF during handshake"))?;

        let msg = match IrcMessage::parse(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Unparseable line during handshake ({e}): {line:?}");
                continue;
            }
        };

        match &msg.command {
            IrcCommand::Pass { password } => {
                if *password != config.link_password {
                    let _ = writer.write_raw("ERROR :Bad password\r\n").await;
                    tracing::error!(
                        "Uplink sent wrong link password — this is a misconfiguration, exiting"
                    );
                    std::process::exit(1);
                }
                pass_seen = true;
            }
            IrcCommand::Protoctl { tokens } => {
                for token in tokens {
                    if let Some(sid) = token.strip_prefix("SID=") {
                        uplink_sid = sid.to_owned();
                    }
                    if token == "MTAGS" {
                        mtags_active = true;
                    }
                }
            }
            // Either form of server introduction ends the handshake.
            IrcCommand::Server { .. } | IrcCommand::Sid { .. } => {
                if !pass_seen {
                    anyhow::bail!("SERVER/SID received before PASS during handshake");
                }
                break;
            }
            IrcCommand::Error { message } => {
                tracing::error!("ERROR from uplink during handshake: {message}");
                std::process::exit(1);
            }
            IrcCommand::Ping { token } => {
                // PING can arrive at any time; answer immediately.
                let pong = format!(":{} PONG {} :{token}\r\n", config.sid, config.sid);
                writer
                    .write_raw(&pong)
                    .await
                    .context("Writing PONG during handshake")?;
            }
            _ => {
                tracing::debug!("Ignoring during handshake: {line:?}");
            }
        }
    }

    Ok(HandshakeResult {
        uplink_sid,
        mtags_active,
    })
}

/// Run the main session loop for a live link.
///
/// Handles:
/// - Inbound lines → parse → translate to `S2SEvent` → send on `event_tx`.
/// - Outbound `S2SCommand` → translate → rate-limit → write to wire.
/// - Inbound `PING` → immediate `PONG` (bypasses rate limiter).
/// - Keepalive: send `PING` every `ping_interval`; bail if no `PONG` within
///   `pong_timeout`.
///
/// `ping_interval` and `pong_timeout` are parameterised so tests can use short
/// values without sleeping for real.
async fn run_session(
    mut reader: IrcReader,
    mut writer: IrcWriter,
    hs: HandshakeResult,
    cmd_rx: &mut mpsc::Receiver<S2SCommand>,
    event_tx: &mpsc::Sender<S2SEvent>,
    our_sid: &str,
    timings: SessionTimings,
) -> anyhow::Result<()> {
    let ping_interval = timings.ping_interval;
    let pong_timeout = timings.pong_timeout;
    let mut bucket = TokenBucket::default_irc();
    let mut queue: VecDeque<IrcMessage> = VecDeque::new();

    let mut ping_tick = tokio::time::interval(ping_interval);
    // Consume the immediate first tick so we don't send a PING at t=0.
    ping_tick.tick().await;

    let mut waiting_for_pong = false;

    // PONG deadline: armed when we send a PING, reset when we receive a PONG.
    // Starts at FAR_FUTURE so it never fires before the first PING.
    let pong_sleep = tokio::time::sleep(FAR_FUTURE);
    tokio::pin!(pong_sleep);

    // Write timer: reset when the queue becomes non-empty; fires when the
    // next token is available.
    let write_timer = tokio::time::sleep(FAR_FUTURE);
    tokio::pin!(write_timer);

    loop {
        // ── Drain queue ──────────────────────────────────────────────────
        while !queue.is_empty() && bucket.try_consume(Instant::now()) {
            let msg = queue.pop_front().unwrap();
            writer.write_message(&msg).await.context("Write error")?;
        }

        // Schedule the next drain if there are still items in the queue.
        if !queue.is_empty() {
            let delay = bucket.refill_delay(Instant::now());
            write_timer
                .as_mut()
                .reset(tokio::time::Instant::now() + delay);
        }

        // ── Select ────────────────────────────────────────────────────────
        tokio::select! {
            // Inbound line from the uplink.
            result = reader.next_line() => {
                let line = result
                    .context("Read error")?
                    .ok_or_else(|| anyhow::anyhow!("Connection closed by remote"))?;

                let msg = match IrcMessage::parse(&line) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(line = ?line, "Failed to parse inbound line: {e}");
                        continue;
                    }
                };

                match &msg.command {
                    IrcCommand::Ping { token } => {
                        // Respond immediately, bypassing the rate limiter.
                        let pong = format!(":{our_sid} PONG {our_sid} :{token}\r\n");
                        writer.write_raw(&pong).await.context("Writing PONG")?;
                    }
                    IrcCommand::Pong { token, .. } => {
                        if token.as_str() == our_sid {
                            waiting_for_pong = false;
                            pong_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + FAR_FUTURE);
                        }
                    }
                    IrcCommand::Error { message } => {
                        anyhow::bail!("ERROR from uplink: {message}");
                    }
                    _ => {
                        if let Some(event) = translate_inbound(&msg) {
                            let _ = event_tx.send(event).await;
                        } else {
                            tracing::debug!(line = ?line, "Unhandled inbound command");
                        }
                    }
                }
            }

            // Outbound command from the processing task.
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    None => return Ok(()), // channel closed = graceful shutdown
                    Some(cmd) => {
                        let ts = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let msgs = translate_outbound(&cmd, our_sid, hs.mtags_active, ts);
                        queue.extend(msgs);
                    }
                }
            }

            // Rate-limited queue drain: fires when the next token is available.
            _ = &mut write_timer, if !queue.is_empty() => {
                // Token should now be available; loop back to drain at the top.
            }

            // Outgoing keepalive PING.
            _ = ping_tick.tick() => {
                let ping = format!("PING :{our_sid}\r\n");
                writer.write_raw(&ping).await.context("Writing PING")?;
                waiting_for_pong = true;
                pong_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + pong_timeout);
            }

            // PONG timeout.
            _ = &mut pong_sleep, if waiting_for_pong => {
                anyhow::bail!("Ping timeout: no PONG received within {pong_timeout:?}");
            }
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irc_connection::framing::{LineReader, LineWriter};
    use proptest::prelude::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // ── Helpers ──────────────────────────────────────────────────────────

    fn test_config() -> IrcConfig {
        IrcConfig {
            uplink: "127.0.0.1".into(),
            port: 6900,
            tls: false,
            link_name: "discord.test.org".into(),
            link_password: "hunter2".into(),
            sid: "002".into(),
            description: "Test Bridge".into(),
        }
    }

    /// Create an in-memory pair of (IrcReader, IrcWriter) for the "client"
    /// (our side) plus raw halves for the "uplink" (test harness) side.
    fn make_pair(
        buf: usize,
    ) -> (
        IrcReader,
        IrcWriter,
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (uplink_stream, client_stream) = tokio::io::duplex(buf);
        let (uplink_r, uplink_w) = tokio::io::split(uplink_stream);
        let (client_r, client_w) = tokio::io::split(client_stream);

        use tokio::io::{AsyncRead, AsyncWrite};
        let reader = LineReader::new(Box::new(client_r) as Box<dyn AsyncRead + Unpin + Send>);
        let writer = LineWriter::new(Box::new(client_w) as Box<dyn AsyncWrite + Unpin + Send>);
        (reader, writer, uplink_r, uplink_w)
    }

    // ── backoff_delay ─────────────────────────────────────────────────────

    #[test]
    fn backoff_attempt_0_is_below_5s() {
        for _ in 0..20 {
            assert!(backoff_delay(0) < Duration::from_secs(5));
        }
    }

    #[test]
    fn backoff_attempt_10_is_below_300s() {
        for _ in 0..20 {
            assert!(backoff_delay(10) < Duration::from_secs(300));
        }
    }

    proptest! {
        #[test]
        fn backoff_always_below_300s(attempt: u32) {
            let d = backoff_delay(attempt);
            prop_assert!(d < Duration::from_secs(300));
        }

        #[test]
        fn backoff_always_nonnegative(attempt: u32) {
            let _d = backoff_delay(attempt); // just ensure it doesn't panic
        }
    }

    // ── do_handshake ─────────────────────────────────────────────────────

    /// Verify the five outbound lines and that we correctly parse the uplink's
    /// credentials.
    #[tokio::test]
    async fn handshake_correct_outbound_sequence_and_parses_uplink_state() {
        let (mut client_r, mut client_w, uplink_r, mut uplink_w) = make_pair(65_536);

        // Server task: collect the 5 credential lines; reply with uplink creds.
        let server_task = tokio::spawn(async move {
            let mut lines = Vec::new();
            let mut reader = LineReader::new(uplink_r);
            for _ in 0..5 {
                lines.push(reader.next_line().await.unwrap().unwrap());
            }
            uplink_w
                .write_all(
                    b"PASS :hunter2\r\n\
                      PROTOCTL SID=001 MTAGS\r\n\
                      SERVER irc.server.org 1 :IRC Server\r\n",
                )
                .await
                .unwrap();
            lines
        });

        let config = test_config();
        let result = do_handshake(&mut client_r, &mut client_w, &config)
            .await
            .unwrap();

        let sent = server_task.await.unwrap();
        assert_eq!(sent[0], "PASS :hunter2", "line 1: PASS");
        assert_eq!(
            sent[1], "PROTOCTL EAUTH=discord.test.org",
            "line 2: PROTOCTL EAUTH"
        );
        assert!(
            sent[2].starts_with("PROTOCTL NOQUIT"),
            "line 3: PROTOCTL caps; got {:?}",
            sent[2]
        );
        assert_eq!(sent[3], "PROTOCTL SID=002", "line 4: PROTOCTL SID");
        assert_eq!(
            sent[4], "SERVER discord.test.org 1 :Test Bridge",
            "line 5: SERVER"
        );

        assert_eq!(result.uplink_sid, "001");
        assert!(result.mtags_active);
    }

    /// Uplink sends PROTOCTL without MTAGS → mtags_active is false.
    #[tokio::test]
    async fn handshake_no_mtags_if_not_advertised() {
        let (mut client_r, mut client_w, _uplink_r, mut uplink_w) = make_pair(65_536);

        tokio::spawn(async move {
            // Read and discard the 5 outbound lines.
            let mut reader = LineReader::new(_uplink_r);
            for _ in 0..5 {
                reader.next_line().await.unwrap();
            }
            uplink_w
                .write_all(
                    b"PASS :hunter2\r\n\
                      PROTOCTL SID=001\r\n\
                      SERVER irc.server.org 1 :IRC Server\r\n",
                )
                .await
                .unwrap();
        });

        let config = test_config();
        let result = do_handshake(&mut client_r, &mut client_w, &config)
            .await
            .unwrap();

        assert_eq!(result.uplink_sid, "001");
        assert!(!result.mtags_active);
    }

    /// A PING during the handshake is answered with a PONG immediately.
    #[tokio::test]
    async fn handshake_responds_to_ping() {
        let (mut client_r, mut client_w, uplink_r, mut uplink_w) = make_pair(65_536);

        let server_task = tokio::spawn(async move {
            let mut reader = LineReader::new(uplink_r);
            // Read the 5 credential lines.
            for _ in 0..5 {
                reader.next_line().await.unwrap();
            }
            // Send a PING before SERVER.
            uplink_w.write_all(b"PING :testtoken\r\n").await.unwrap();
            // Read the PONG response.
            let pong_line = reader.next_line().await.unwrap().unwrap();
            // Then finish the handshake.
            uplink_w
                .write_all(b"PASS :hunter2\r\nSERVER irc.server.org 1 :S\r\n")
                .await
                .unwrap();
            pong_line
        });

        let config = test_config();
        let _result = do_handshake(&mut client_r, &mut client_w, &config)
            .await
            .unwrap();

        let pong_line = server_task.await.unwrap();
        assert_eq!(pong_line, ":002 PONG 002 :testtoken");
    }

    // ── run_session ───────────────────────────────────────────────────────

    fn default_hs() -> HandshakeResult {
        HandshakeResult {
            uplink_sid: "001".into(),
            mtags_active: false,
        }
    }

    /// Inbound PRIVMSG is translated and emitted as S2SEvent::MessageReceived.
    #[tokio::test]
    async fn session_inbound_privmsg_emits_event() {
        let (client_r, client_w, uplink_r, mut uplink_w) = make_pair(65_536);

        // Write a PRIVMSG then close BOTH halves of the uplink DuplexStream.
        // tokio::io::split shares the stream via Arc — we must drop both halves
        // to drop the DuplexStream, which signals EOF on client_r.
        tokio::spawn(async move {
            uplink_w
                .write_all(b":ABC001 PRIVMSG #test :hello\r\n")
                .await
                .unwrap();
            drop(uplink_w);
            drop(uplink_r); // completes the Arc → DuplexStream dropped → EOF on client_r
        });

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<S2SCommand>(4);
        let (event_tx, mut event_rx) = mpsc::channel::<S2SEvent>(16);
        let _keep_cmd_tx = cmd_tx; // keep alive so cmd_rx doesn't return None

        let _ = run_session(
            client_r,
            client_w,
            default_hs(),
            &mut cmd_rx,
            &event_tx,
            "002",
            SessionTimings::production(),
        )
        .await;

        let event = event_rx.try_recv().expect("expected an event");
        match event {
            S2SEvent::MessageReceived {
                from_uid,
                target,
                text,
                ..
            } => {
                assert_eq!(from_uid, "ABC001");
                assert_eq!(target, "#test");
                assert_eq!(text, "hello");
            }
            other => panic!("expected MessageReceived, got {other:?}"),
        }
    }

    /// An outbound S2SCommand is translated to an IRC wire line.
    #[tokio::test]
    async fn session_outbound_command_written_to_wire() {
        let (client_r, client_w, uplink_r, mut uplink_w) = make_pair(65_536);

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<S2SCommand>(4);
        let (event_tx, _event_rx) = mpsc::channel::<S2SEvent>(4);

        // Send command then close uplink to terminate the session.
        let cmd_task = tokio::spawn(async move {
            cmd_tx
                .send(S2SCommand::SendMessage {
                    from_uid: "002AAAAAA".into(),
                    target: "#test".into(),
                    text: "hi".into(),
                    timestamp: None,
                })
                .await
                .unwrap();
            drop(cmd_tx); // cmd_rx will return None → session exits cleanly
            // Brief pause so the message is written before we check.
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(uplink_w);
        });

        // Read what the session writes to the wire.
        let read_task = tokio::spawn(async move {
            let mut reader = LineReader::new(uplink_r);
            let mut found: Option<String> = None;
            while let Ok(Some(line)) = reader.next_line().await {
                if line.contains("PRIVMSG") {
                    found = Some(line);
                    break;
                }
            }
            found
        });

        let _ = run_session(
            client_r,
            client_w,
            default_hs(),
            &mut cmd_rx,
            &event_tx,
            "002",
            SessionTimings::production(),
        )
        .await;

        cmd_task.await.unwrap();
        let wire_line = read_task.await.unwrap();
        let line = wire_line.expect("expected PRIVMSG on the wire");
        assert!(
            line.contains(":002AAAAAA PRIVMSG #test :hi"),
            "unexpected wire line: {line:?}"
        );
    }

    /// Inbound PING from the uplink is answered with PONG immediately.
    #[tokio::test]
    async fn session_ping_gets_immediate_pong() {
        let (client_r, client_w, uplink_r, mut uplink_w) = make_pair(65_536);

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<S2SCommand>(4);
        let (event_tx, _event_rx) = mpsc::channel::<S2SEvent>(4);
        let _keep = cmd_tx;

        // Write PING then close.
        let write_task = tokio::spawn(async move {
            uplink_w.write_all(b"PING :pingtoken\r\n").await.unwrap();
            // Read PONG before closing.
            let mut reader = LineReader::new(uplink_r);
            let pong = reader.next_line().await.unwrap().unwrap();
            drop(uplink_w);
            pong
        });

        let _ = run_session(
            client_r,
            client_w,
            default_hs(),
            &mut cmd_rx,
            &event_tx,
            "002",
            SessionTimings::production(),
        )
        .await;

        let pong_line = write_task.await.unwrap();
        assert_eq!(pong_line, ":002 PONG 002 :pingtoken");
    }

    /// After `ping_interval`, the session sends a PING to the uplink.
    #[tokio::test]
    async fn session_sends_keepalive_ping() {
        let (client_r, client_w, uplink_r, mut uplink_w) = make_pair(65_536);

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<S2SCommand>(4);
        let (event_tx, _event_rx) = mpsc::channel::<S2SEvent>(4);
        let _keep = cmd_tx;

        // Read whatever the session sends.
        let read_task = tokio::spawn(async move {
            let mut reader = LineReader::new(uplink_r);
            let mut found = false;
            while let Ok(Some(line)) = reader.next_line().await {
                if line.starts_with("PING :") {
                    found = true;
                    break;
                }
            }
            found
        });

        // Close the uplink after 120ms (enough for a 50ms ping_interval).
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            drop(uplink_w);
        });

        let _ = run_session(
            client_r,
            client_w,
            default_hs(),
            &mut cmd_rx,
            &event_tx,
            "002",
            SessionTimings {
                ping_interval: Duration::from_millis(50),
                pong_timeout: Duration::from_secs(60),
            },
        )
        .await;

        let saw_ping = read_task.await.unwrap();
        assert!(
            saw_ping,
            "expected a PING to be sent after the ping interval"
        );
    }

    /// If no PONG is received within `pong_timeout`, the session returns Err.
    #[tokio::test]
    async fn session_ping_timeout_returns_error() {
        let (client_r, client_w, _uplink_r, _uplink_w) = make_pair(65_536);

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<S2SCommand>(4);
        let (event_tx, _event_rx) = mpsc::channel::<S2SEvent>(4);
        let _keep = cmd_tx;
        // Keep uplink_w alive (don't close connection; uplink just doesn't send PONG).
        let _uplink_w = _uplink_w;

        let result = run_session(
            client_r,
            client_w,
            default_hs(),
            &mut cmd_rx,
            &event_tx,
            "002",
            SessionTimings {
                ping_interval: Duration::from_millis(50),
                pong_timeout: Duration::from_millis(30),
            },
        )
        .await;

        assert!(result.is_err(), "expected ping timeout error");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("timeout") || msg.contains("Ping"),
            "unexpected error message: {msg}"
        );
    }
}
