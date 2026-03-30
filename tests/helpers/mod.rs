//! Shared helpers for e2e tests.

pub mod irc_client;

pub use irc_client::TestIrcClient;

use testcontainers::core::{IntoContainerPort, Mount};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};

/// Handle to a running UnrealIRCd Docker container.
///
/// Dropping this value stops and removes the container.
pub struct IrcContainer {
    /// Host port mapped to the container's S2S port 6900.
    pub s2s_port: u16,
    /// Host port mapped to the container's client port 6667.
    pub client_port: u16,
    // Kept alive so the container is not dropped until the test ends.
    _container: ContainerAsync<GenericImage>,
}

/// Start a fresh UnrealIRCd container and wait for it to accept connections.
///
/// Requires Docker to be running. The container is automatically cleaned up
/// when the returned [`IrcContainer`] is dropped.
pub async fn start_unrealircd() -> IrcContainer {
    let conf_path = std::fs::canonicalize("tests/fixtures/unrealircd.conf")
        .expect("tests/fixtures/unrealircd.conf not found — run `cargo test` from repo root");

    let container = GenericImage::new("ircd/unrealircd", "latest")
        .with_exposed_port(6667u16.tcp())
        .with_exposed_port(6900u16.tcp())
        .with_mount(Mount::bind_mount(
            conf_path.to_str().unwrap(),
            "/ircd/unrealircd.conf",
        ))
        .start()
        .await
        .expect("Failed to start UnrealIRCd container (is Docker running?)");

    let client_port = container
        .get_host_port_ipv4(6667)
        .await
        .expect("failed to get client port");
    let s2s_port = container
        .get_host_port_ipv4(6900)
        .await
        .expect("failed to get S2S port");

    // Poll until UnrealIRCd accepts client connections on the mapped port.
    wait_for_tcp(
        &format!("127.0.0.1:{client_port}"),
        std::time::Duration::from_secs(30),
    )
    .await;

    IrcContainer {
        s2s_port,
        client_port,
        _container: container,
    }
}

/// Poll `addr` until TCP connect succeeds or `max_wait` elapses.
async fn wait_for_tcp(addr: &str, max_wait: std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        if start.elapsed() >= max_wait {
            panic!("Timed out after {max_wait:?} waiting for {addr}");
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}
