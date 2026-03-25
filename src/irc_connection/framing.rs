// These items are called by the connection loop (implemented in the next task).
// Until that task is complete they appear unused to the compiler.
#![allow(dead_code)]

/// Line-oriented framing for the IRC wire protocol.
///
/// IRC messages are `\r\n`-terminated. The MTAGS capability extends the
/// maximum line length from 512 to 4096 bytes. Invalid UTF-8 is replaced
/// with U+FFFD rather than causing an error.
use std::io;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::irc_message::IrcMessage;

/// Maximum payload size (bytes, excluding `\r\n`) we will accept on inbound lines.
const MAX_LINE_BYTES: usize = 4096;

/// Wraps an `AsyncRead` and yields one decoded IRC line per call.
pub struct LineReader<R> {
    inner: BufReader<R>,
}

impl<R: tokio::io::AsyncRead + Unpin> LineReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
        }
    }

    /// Read the next `\r\n`-terminated line.
    ///
    /// Returns:
    /// - `Ok(Some(line))` — a line was read; `\r\n` (or bare `\n`) stripped.
    /// - `Ok(None)` — the connection was closed cleanly (EOF at line boundary).
    /// - `Err(e)` — an I/O error occurred.
    ///
    /// Lines longer than `MAX_LINE_BYTES` are **dropped** (logged at WARN level)
    /// and the method loops to return the next valid line.
    ///
    /// Invalid UTF-8 bytes are replaced with U+FFFD.
    pub async fn next_line(&mut self) -> io::Result<Option<String>> {
        loop {
            let mut raw: Vec<u8> = Vec::with_capacity(512);
            let n = self.inner.read_until(b'\n', &mut raw).await?;
            if n == 0 {
                return Ok(None); // clean EOF
            }

            // Strip trailing \r\n or bare \n.
            // The `> 0` guards are always true here (n > 0 guarantees raw.len() ≥ 1)
            // so `>= 0` is mutation-equivalent. The guards are kept for clarity.
            let end = raw.len();
            let end = if end > 0 && raw[end - 1] == b'\n' {
                end - 1
            } else {
                end
            };
            let end = if end > 0 && raw[end - 1] == b'\r' {
                end - 1
            } else {
                end
            };
            let payload = &raw[..end];

            if payload.len() > MAX_LINE_BYTES {
                tracing::warn!(
                    bytes = payload.len(),
                    "Dropping overlong IRC line (> {} bytes)",
                    MAX_LINE_BYTES
                );
                continue;
            }

            return Ok(Some(String::from_utf8_lossy(payload).into_owned()));
        }
    }
}

/// Wraps an `AsyncWrite` and serialises `IrcMessage` values as wire lines.
pub struct LineWriter<W> {
    inner: W,
}

impl<W: tokio::io::AsyncWrite + Unpin> LineWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { inner: writer }
    }

    /// Serialise `msg` as a `\r\n`-terminated wire line and flush.
    ///
    /// Returns `Err` if the message cannot be serialised (e.g. too long) or if
    /// the underlying write fails.
    pub async fn write_message(&mut self, msg: &IrcMessage) -> io::Result<()> {
        let wire = msg
            .to_wire()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        self.inner.write_all(wire.as_bytes()).await?;
        self.inner.flush().await
    }

    /// Write a raw pre-formatted line (must already include `\r\n`).
    ///
    /// Used for PING/PONG bypassing the rate limiter.
    pub async fn write_raw(&mut self, line: &str) -> io::Result<()> {
        self.inner.write_all(line.as_bytes()).await?;
        self.inner.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irc_message::{IrcCommand, IrcMessage};
    use tokio::io::duplex;

    // Helper: wrap both halves of a duplex channel in LineReader / LineWriter.
    fn make_pair(
        buf: usize,
    ) -> (
        LineReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        LineWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    ) {
        let (a, _b) = duplex(buf);
        let (r, w) = tokio::io::split(a);
        (LineReader::new(r), LineWriter::new(w))
    }

    // Helper: create a reader wrapping a raw byte slice (simulates incoming data).
    fn reader_from_bytes(data: &[u8]) -> LineReader<std::io::Cursor<Vec<u8>>> {
        LineReader::new(std::io::Cursor::new(data.to_vec()))
    }

    // ── LineReader tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn reads_crlf_line() {
        let mut r = reader_from_bytes(b"PING :server\r\n");
        let line = r.next_line().await.unwrap().unwrap();
        assert_eq!(line, "PING :server");
    }

    #[tokio::test]
    async fn reads_bare_lf_line() {
        let mut r = reader_from_bytes(b"PING :server\n");
        let line = r.next_line().await.unwrap().unwrap();
        assert_eq!(line, "PING :server");
    }

    #[tokio::test]
    async fn reads_multiple_lines() {
        let mut r = reader_from_bytes(b"PING :a\r\nPING :b\r\n");
        assert_eq!(r.next_line().await.unwrap().unwrap(), "PING :a");
        assert_eq!(r.next_line().await.unwrap().unwrap(), "PING :b");
    }

    #[tokio::test]
    async fn eof_at_start_returns_none() {
        let mut r = reader_from_bytes(b"");
        assert!(r.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn eof_after_last_line_returns_none() {
        let mut r = reader_from_bytes(b"PING :x\r\n");
        r.next_line().await.unwrap().unwrap();
        assert!(r.next_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn replaces_invalid_utf8() {
        // 0xFF is not valid UTF-8.
        let mut r = reader_from_bytes(b"PING :\xff\r\n");
        let line = r.next_line().await.unwrap().unwrap();
        assert!(
            line.contains('\u{FFFD}'),
            "expected replacement char in {line:?}"
        );
    }

    #[tokio::test]
    async fn drops_overlong_line_and_returns_next() {
        // Build a line of MAX_LINE_BYTES + 1 payload bytes, then a valid line.
        let long: Vec<u8> = std::iter::repeat(b'X')
            .take(MAX_LINE_BYTES + 1)
            .chain(b"\r\n".iter().copied())
            .collect();
        let mut data = long;
        data.extend_from_slice(b"PING :ok\r\n");

        let mut r = reader_from_bytes(&data);
        // The overlong line is dropped; the next line is returned.
        let line = r.next_line().await.unwrap().unwrap();
        assert_eq!(line, "PING :ok");
    }

    #[tokio::test]
    async fn accepts_line_exactly_at_limit() {
        // Payload of exactly MAX_LINE_BYTES bytes should be accepted.
        let mut data: Vec<u8> = std::iter::repeat(b'X').take(MAX_LINE_BYTES).collect();
        data.extend_from_slice(b"\r\n");

        let mut r = reader_from_bytes(&data);
        let line = r.next_line().await.unwrap().unwrap();
        assert_eq!(line.len(), MAX_LINE_BYTES);
    }

    // ── LineWriter tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_message_appends_crlf() {
        // Write to one end of a duplex, read raw bytes from the other end.
        let (a, mut b) = duplex(4096);
        let (_ar, aw) = tokio::io::split(a);
        let mut writer = LineWriter::new(aw);

        let msg = IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Eos,
        };
        writer.write_message(&msg).await.unwrap();

        let mut buf = vec![0u8; 64];
        use tokio::io::AsyncReadExt;
        let n = b.read(&mut buf).await.unwrap();
        let written = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(
            written.ends_with("\r\n"),
            "expected \\r\\n, got {written:?}"
        );
    }

    #[tokio::test]
    async fn write_raw_sends_bytes_as_is() {
        let (a, mut b) = duplex(4096);
        let (_ar, aw) = tokio::io::split(a);
        let mut writer = LineWriter::new(aw);

        writer.write_raw("PONG :token\r\n").await.unwrap();

        let mut buf = vec![0u8; 64];
        use tokio::io::AsyncReadExt;
        let n = b.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"PONG :token\r\n");
    }

    #[tokio::test]
    async fn partial_line_at_eof_returned_without_stripping() {
        // Input has no \r\n terminator; EOF is reached mid-line.
        // The partial line should be returned as-is, with no trailing byte stripped.
        let mut r = reader_from_bytes(b"PING :server");
        let line = r.next_line().await.unwrap().unwrap();
        assert_eq!(line, "PING :server");
    }

    #[tokio::test]
    async fn write_read_roundtrip() {
        // Write a message through one channel half; read it back through the
        // other half with LineReader to verify end-to-end framing.
        let (a, b) = duplex(4096);
        let (_ar, aw) = tokio::io::split(a);
        let (br, _bw) = tokio::io::split(b);

        let mut writer = LineWriter::new(aw);
        let mut reader = LineReader::new(br);

        let msg = IrcMessage {
            tags: vec![],
            prefix: None,
            command: IrcCommand::Eos,
        };
        writer.write_message(&msg).await.unwrap();
        let line = reader.next_line().await.unwrap().unwrap();
        assert_eq!(line, "EOS");
    }
}
