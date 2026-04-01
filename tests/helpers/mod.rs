//! Shared helpers for e2e tests.

#[allow(dead_code)] // Only used by e2e_discord, not e2e_irc.
pub mod discord_client;
pub mod irc_client;

pub use irc_client::TestIrcClient;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage};

/// Tag used for the locally-built test image.
const TEST_IMAGE: &str = "disirc-unrealircd-test";
const TEST_IMAGE_TAG: &str = "latest";

/// Handle to a running `UnrealIRCd` Docker container.
///
/// Dropping this value stops and removes the container.
pub struct IrcContainer {
    /// Hostname or IP to use when connecting to the container from the host.
    /// On Docker Desktop (Windows/macOS) this is provided by
    /// [`ContainerAsync::get_host`] and must not be assumed to be `127.0.0.1`.
    pub host: String,
    /// Host port mapped to the container's S2S port 6900.
    pub s2s_port: u16,
    /// Host port mapped to the container's client port 6667.
    pub client_port: u16,
    // Kept alive so the container is not dropped until the test ends.
    _container: ContainerAsync<GenericImage>,
}

/// Start a fresh `UnrealIRCd` container and wait for it to be fully ready.
///
/// Builds the local test image on first call (Docker layer cache makes
/// subsequent calls fast). Requires Docker to be running.
/// The container is automatically cleaned up when the returned
/// [`IrcContainer`] is dropped.
///
/// The test config is baked into the image (see `tests/fixtures/Dockerfile`),
/// so no bind-mount is required. This avoids Windows host-path issues.
pub async fn start_unrealircd() -> IrcContainer {
    ensure_test_image_built();

    let container = GenericImage::new(TEST_IMAGE, TEST_IMAGE_TAG)
        .with_exposed_port(6667u16.tcp())
        .with_exposed_port(6900u16.tcp())
        // Wait until UnrealIRCd logs "UnrealIRCd started." to stderr — this
        // confirms all modules are loaded and the server is ready for connections.
        .with_wait_for(WaitFor::message_on_stderr("UnrealIRCd started."))
        .start()
        .await
        .expect("Failed to start UnrealIRCd container (is Docker running?)");

    // On Docker Desktop (Windows/macOS), containers may not be reachable on
    // 127.0.0.1 — use get_host() to get the correct address.
    let host = container.get_host().await.expect("failed to get host");
    let client_port = container
        .get_host_port_ipv4(6667)
        .await
        .expect("failed to get client port");
    let s2s_port = container
        .get_host_port_ipv4(6900)
        .await
        .expect("failed to get S2S port");

    IrcContainer {
        host: host.to_string(),
        s2s_port,
        client_port,
        _container: container,
    }
}

/// Build the test Docker image from `tests/fixtures/Dockerfile` if it has not
/// been built yet in this process. The Docker layer cache makes rebuilds fast
/// when nothing has changed.
///
/// Uses a [`std::sync::OnceLock`] so parallel test threads only build once.
fn ensure_test_image_built() {
    static BUILT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    BUILT.get_or_init(|| {
        let fixtures = std::fs::canonicalize("tests/fixtures")
            .expect("tests/fixtures not found — run from repo root");

        // Normalise for Docker on Windows (strip \\?\ prefix, forward slashes).
        let fixtures_str = fixtures.to_str().unwrap();
        let fixtures_docker = fixtures_str
            .strip_prefix(r"\\?\")
            .unwrap_or(fixtures_str)
            .replace('\\', "/");

        let status = std::process::Command::new("docker")
            .args([
                "build",
                "-t",
                &format!("{TEST_IMAGE}:{TEST_IMAGE_TAG}"),
                &fixtures_docker,
            ])
            .status()
            .expect("failed to run `docker build` — is Docker installed?");

        assert!(status.success(), "docker build for test image failed");
    });
}
