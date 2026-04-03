//! Tracing subscriber setup for e2e tests.
//!
//! Installs a global subscriber filtered to `disirc=trace,serenity=debug`
//! (suppressing hyper/h2 noise).  The filter can be overridden via `RUST_LOG`.

use std::sync::OnceLock;

use tracing_subscriber::EnvFilter;

/// Install the tracing subscriber once per process.
///
/// Uses a global [`OnceLock`] so the subscriber is only installed once
/// (tracing only allows one global subscriber).  Subsequent calls are no-ops.
///
/// The filter can be overridden via `RUST_LOG` for local debugging.
pub fn init_tracing() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("disirc=trace,serenity=debug"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init()
            .ok();
    });
}
