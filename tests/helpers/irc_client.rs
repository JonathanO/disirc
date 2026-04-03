//! Raw tokio TCP IRC client for e2e test verification.

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};

/// Minimal IRC client for use in e2e tests. Connects as a regular user (not
/// S2S) to the test IRC server on port 6667, allowing tests to observe the
/// IRC-visible effects of bridge operations.
pub struct TestIrcClient {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: tokio::io::WriteHalf<TcpStream>,
}

impl TestIrcClient {
    /// Connect to `addr`, register as `nick`, and wait for `RPL_WELCOME` (001).
    /// Handles PING challenges during registration automatically.
    pub async fn connect(addr: &str, nick: &str) -> Self {
        let stream = TcpStream::connect(addr)
            .await
            .unwrap_or_else(|e| panic!("IRC connect to {addr} failed: {e}"));
        let (read, write) = tokio::io::split(stream);
        let mut client = Self {
            reader: BufReader::new(read),
            writer: write,
        };
        client.send(&format!("NICK {nick}")).await;
        client
            .send(&format!("USER {nick} 0 * :E2E Test Client"))
            .await;
        client.expect_code("001", Duration::from_secs(15)).await;
        client
    }

    /// Send a raw IRC line. CRLF is appended automatically.
    pub async fn send(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("IRC write failed");
    }

    /// Read one raw line. Returns `None` on timeout or EOF.
    pub async fn read_line_timeout(&mut self, dur: Duration) -> Option<String> {
        let mut line = String::new();
        match timeout(dur, self.reader.read_line(&mut line)).await {
            Ok(Ok(0) | Err(_)) | Err(_) => None,
            Ok(Ok(_)) => Some(line.trim_end_matches(['\r', '\n']).to_string()),
        }
    }

    /// JOIN `channel` and wait for `RPL_ENDOFNAMES` (366).
    /// Panics immediately if the server responds with an error numeric (4xx/5xx).
    pub async fn join(&mut self, channel: &str) {
        self.send(&format!("JOIN {channel}")).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            let line = self
                .read_line_timeout(remaining)
                .await
                .unwrap_or_else(|| panic!("timed out waiting for JOIN {channel} response (366)"));
            if let Some(token) = line.strip_prefix("PING :") {
                self.send(&format!("PONG :{token}")).await;
                continue;
            }
            // IRC format: :server NNN nick ...
            let parts: Vec<&str> = line.splitn(4, ' ').collect();
            if let Some(numeric) = parts.get(1) {
                if *numeric == "366" {
                    return;
                }
                // JOIN-specific error numerics (RFC 2812 + UnrealIRCd extensions).
                // 400-level numerics like 422 (no MOTD) are unrelated to JOIN.
                if let Ok(n) = numeric.parse::<u16>() {
                    if matches!(n, 403 | 405 | 437 | 448 | 471..=480 | 520) {
                        panic!("JOIN {channel} failed with error {n}: {line}");
                    }
                }
            }
        }
    }

    /// Send `NAMES #channel` and poll until the reply contains a nick other
    /// than our own (i.e. a pseudoclient has appeared).  Retries every 2s
    /// until `timeout_dur` elapses.
    #[allow(dead_code)] // used from e2e_discord but not e2e_irc
    pub async fn expect_names_contain(&mut self, channel: &str, timeout_dur: Duration) {
        let deadline = tokio::time::Instant::now() + timeout_dur;
        loop {
            self.send(&format!("NAMES {channel}")).await;
            // Collect 353 lines until 366 (end of names).
            let mut names = String::new();
            loop {
                let remaining = deadline
                    .checked_duration_since(tokio::time::Instant::now())
                    .unwrap_or(Duration::ZERO);
                assert!(
                    !remaining.is_zero(),
                    "timed out waiting for pseudoclient in NAMES {channel}"
                );
                let line = self
                    .read_line_timeout(remaining)
                    .await
                    .unwrap_or_else(|| panic!("timed out waiting for NAMES reply for {channel}"));
                if let Some(token) = line.strip_prefix("PING :") {
                    self.send(&format!("PONG :{token}")).await;
                    continue;
                }
                let parts: Vec<&str> = line.splitn(4, ' ').collect();
                if parts.get(1) == Some(&"353") {
                    // :server 353 nick = #channel :@testbot user1 user2
                    if let Some(trailing) = line.rsplit_once(':') {
                        names.push(' ');
                        names.push_str(trailing.1);
                    }
                } else if parts.get(1) == Some(&"366") {
                    break;
                }
            }
            // Check if any nick besides our own is present.
            let nicks: Vec<&str> = names
                .split_whitespace()
                .map(|n| n.trim_start_matches('@').trim_start_matches('+'))
                .filter(|n| !n.eq_ignore_ascii_case("testbot"))
                .collect();
            if !nicks.is_empty() {
                return;
            }
            // No pseudoclients yet — wait and retry.
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Send `PRIVMSG target :text`.
    pub async fn send_privmsg(&mut self, target: &str, text: &str) {
        self.send(&format!("PRIVMSG {target} :{text}")).await;
    }

    /// Read lines until one contains `needle`, responding to PING automatically.
    /// Panics if `timeout_dur` elapses first.
    pub async fn expect_line_containing(&mut self, needle: &str, timeout_dur: Duration) -> String {
        let deadline = tokio::time::Instant::now() + timeout_dur;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            assert!(
                !remaining.is_zero(),
                "timed out waiting for line containing {needle:?}"
            );
            let line = self
                .read_line_timeout(remaining)
                .await
                .unwrap_or_else(|| panic!("timed out waiting for line containing {needle:?}"));
            if let Some(token) = line.strip_prefix("PING :") {
                self.send(&format!("PONG :{token}")).await;
                continue;
            }
            if line.contains(needle) {
                return line;
            }
        }
    }

    /// Read lines until a PRIVMSG is found where the prefix contains
    /// `nick_fragment` and the message text contains `text_fragment`.
    /// Panics if `timeout_dur` elapses first.
    #[allow(dead_code)] // Used by e2e_irc but not e2e_discord.
    pub async fn expect_privmsg(
        &mut self,
        nick_fragment: &str,
        text_fragment: &str,
        timeout_dur: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + timeout_dur;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            assert!(
                !remaining.is_zero(),
                "timed out waiting for PRIVMSG from nick~={nick_fragment:?} \
                 with text~={text_fragment:?}"
            );
            let line = self
                .read_line_timeout(remaining)
                .await
                .unwrap_or_else(|| panic!("timed out waiting for PRIVMSG"));
            if let Some(token) = line.strip_prefix("PING :") {
                self.send(&format!("PONG :{token}")).await;
                continue;
            }
            // :nick!user@host PRIVMSG target :text
            if line.contains("PRIVMSG")
                && line.contains(nick_fragment)
                && line.contains(text_fragment)
            {
                return;
            }
        }
    }

    /// Wait for the server to send a line containing numeric `code`.
    /// Responds to PING automatically.
    async fn expect_code(&mut self, code: &str, dur: Duration) {
        let deadline = tokio::time::Instant::now() + dur;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            let line = self
                .read_line_timeout(remaining)
                .await
                .unwrap_or_else(|| panic!("timed out waiting for numeric {code}"));
            if let Some(token) = line.strip_prefix("PING :") {
                self.send(&format!("PONG :{token}")).await;
                continue;
            }
            // IRC format: :server NNN nick ...
            let mut parts = line.splitn(4, ' ');
            let _ = parts.next(); // prefix
            if parts.next() == Some(code) {
                return;
            }
        }
    }
}
