//! Minimal tracing capture layer for e2e test log assertions.
//!
//! Provides a [`CaptureLayer`] that records formatted log lines to a shared
//! buffer, and an [`init_capture_tracing`] function that sets up a subscriber
//! filtered to `disirc=trace` (suppressing hyper/h2/serenity noise).

use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// Shared buffer that collects log output for later assertions.
#[derive(Clone)]
pub struct LogCapture {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl LogCapture {
    fn new() -> Self {
        Self {
            buf: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Return all captured log lines.
    pub fn lines(&self) -> Vec<String> {
        let buf = self.buf.lock().unwrap();
        String::from_utf8_lossy(&buf)
            .lines()
            .map(String::from)
            .collect()
    }

    /// Assert that no captured lines contain WARN or ERROR level markers.
    /// Panics with the offending lines if any are found.
    pub fn assert_no_warnings_or_errors(&self) {
        let lines = self.lines();
        let problems: Vec<_> = lines
            .iter()
            .filter(|line| line.contains(" WARN ") || line.contains(" ERROR "))
            .collect();
        assert!(
            problems.is_empty(),
            "expected no WARN/ERROR logs, found {}:\n{}",
            problems.len(),
            problems
                .iter()
                .map(|l| format!("  {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

/// Writer that appends to the shared buffer.
pub struct CaptureWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl std::io::Write for CaptureWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogCapture {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter {
            buf: Arc::clone(&self.buf),
        }
    }
}

/// Initialise a tracing subscriber filtered to `disirc=trace` that captures
/// output for assertions. Returns a [`LogCapture`] handle.
///
/// Uses a global [`OnceLock`] so the subscriber is only installed once per
/// process (tracing only allows one global subscriber). Subsequent calls
/// return a new [`LogCapture`] that shares the same buffer.
///
/// The filter can be overridden via `RUST_LOG` for local debugging.
pub fn init_capture_tracing() -> LogCapture {
    static CAPTURE: OnceLock<LogCapture> = OnceLock::new();
    let capture = CAPTURE
        .get_or_init(|| {
            let capture = LogCapture::new();
            let filter = EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("disirc=trace"));
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(capture.clone())
                .with_test_writer()
                .try_init()
                .ok();
            capture
        })
        .clone();
    // Clear the buffer for each test so assertions only check this test's logs.
    capture.buf.lock().unwrap().clear();
    capture
}
