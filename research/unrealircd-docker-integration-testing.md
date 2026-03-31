# UnrealIRCd Docker Integration Testing

## Summary

Research into running UnrealIRCd 6 in Docker for integration testing of an S2S-connecting bridge. Building from the official UnrealIRCd source tarball is the recommended approach — it pins a specific version and avoids depending on third-party images. A minimal config derived from the official test suite (`unrealircd-tests`) can enable both S2S and client connections. For the test IRC client, raw TCP with tokio is simplest and avoids adding another dependency. Startup detection should use TCP connect retry (UnrealIRCd logs `UNREALIRCD_START` but that is inside the container).

## 1. Docker Image

### Recommendation: Build our own Dockerfile

Building from the official UnrealIRCd source tarball (downloaded from `unrealircd.org`) is the right approach. It pins a specific version, controls the install layout, and avoids all third-party image dependency. The `-F` flag runs UnrealIRCd in the foreground (required for Docker).

UnrealIRCd refuses to build or run as root. The Dockerfile creates a dedicated `ircd` user; `make install` places files at `/home/ircd/unrealircd/`:
- Binary: `/home/ircd/unrealircd/bin/unrealircd`
- Config dir: `/home/ircd/unrealircd/conf/` (bind-mount target for `unrealircd.conf`)
- Default module lists (`modules.default.conf`, `operclass.default.conf`, etc.) are installed to the same conf dir, so relative `include` statements in `unrealircd.conf` resolve correctly.

```dockerfile
FROM debian:bookworm-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev libpcre2-dev \
    libcurl4-openssl-dev libc-ares-dev libsodium-dev \
    wget ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG UNREALIRCD_VERSION=6.1.10
RUN wget -q "https://www.unrealircd.org/downloads/unrealircd-${UNREALIRCD_VERSION}.tar.gz" \
    && tar xzf "unrealircd-${UNREALIRCD_VERSION}.tar.gz" \
    && cd "unrealircd-${UNREALIRCD_VERSION}" \
    && ./Config \
        --enable-ssl \
        --with-fd-setsize=1024 \
        --with-permissions=0600 \
        --nointro \
    && make -j"$(nproc)" \
    && make install \
    && cd .. \
    && rm -rf "unrealircd-${UNREALIRCD_VERSION}" "unrealircd-${UNREALIRCD_VERSION}.tar.gz"

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 libpcre2-8-0 libcurl4 libc-ares2 libsodium23 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /root/unrealircd /root/unrealircd

EXPOSE 6667 6900
WORKDIR /root/unrealircd
CMD ["/root/unrealircd/bin/unrealircd", "-F"]
```

Key design notes:
- **No `DESTDIR`** — installing directly means the compiled-in paths (`/root/unrealircd/...`) match the runtime paths. Using `DESTDIR=/build` would break the binary's self-referential paths.
- **Config bind-mount target**: `/home/ircd/unrealircd/conf/unrealircd.conf`.

### `ircd/unrealircd` on Docker Hub — not recommended

- **Repository**: https://hub.docker.com/r/ircd/unrealircd
- **Maintained by**: `@adamus1red` (GitHub: `adamus1red/docker-unrealircd`) — a community maintainer, **not** the UnrealIRCd project itself. The UnrealIRCd project (github.com/unrealircd, unrealircd.org) has no Docker-related repositories and does not reference this image in its documentation.
- **Tags**: `nightly`, `edge`, and PR-based tags (e.g. `pr-55`). No `latest` tag exists.
- **Structure**: The image has UnrealIRCd installed at `/app/unrealircd/` and uses `/ircd/` as the config directory. It ships with no `EXPOSE` directive and `CMD ['/bin/sh']`, making it a CI build image rather than a ready-to-run server image. A CMD override (`/app/unrealircd/bin/unrealircd -F`) is needed to use it as a server.
- **Verdict**: Usable as a quick start but not suitable for pinned, reproducible tests. Version is implicit in the `nightly` tag.

### Other community images — too old or untested

| Image | Notes |
|-------|-------|
| `agamemnon23/unrealircd` | Compiles from source, v1.0.0 from 2022. Stale. |
| `djlegolas/unrealircd` | Supports UnrealIRCd 6+, mounts at `/app/data/unrealircd.conf`. Less established. |
| `bbriggs/unrealircd` | Only UnrealIRCd 4.x. Too old. |
| `carterfields/unrealircd` | v6.1.1.1. No maintenance status information. |

## 2. Minimal Config for S2S Link

Derived from the official test suite at `unrealircd-tests/serverconfig/unrealircd/`. The test suite uses `irc1.conf` (per-server) + `common.conf` (shared blocks). Below is a single merged config.

### Required blocks

UnrealIRCd 6 requires all of these to start:
- `include "modules.default.conf"` (loads all core modules)
- `me {}` (server identity + SID)
- `admin {}` (admin contact)
- `class {}` (at least "clients" and "servers")
- `allow {}` (who can connect as clients)
- `listen {}` (ports)
- `set {}` (network-name, kline-address are mandatory)

### Complete minimal test config

```
/* Load required modules */
include "modules.default.conf";
include "operclass.default.conf";

loadmodule "cloak_sha256";

/* Server identity */
me {
    name irc.test.net;
    info "Integration test IRC server";
    sid 001;
};

admin {
    "Test server";
    "Not for production";
};

/* Connection classes */
class clients {
    pingfreq 90;
    maxclients 1000;
    sendq 100000;
    options { nofakelag; };
};

class servers {
    pingfreq 60;
    connfreq 15;
    maxclients 10;
    sendq 5M;
};

/* Allow all client connections (tests connect from localhost) */
allow {
    mask *@*;
    class clients;
    maxperip 100;
};

/* Plain TCP for IRC clients (test verification) */
listen {
    ip *;
    port 6667;
};

/* Plain TCP for S2S links */
listen {
    ip *;
    port 6900;
    options { serversonly; };
};

/* S2S link block for the bridge */
link bridge.test.net {
    incoming {
        mask *;
    };
    password "testpassword";
    class servers;
    hub *;
};

/* Oper account for test convenience */
oper test {
    class clients;
    mask *@*;
    password "test";
    operclass netadmin-with-override;
};

/* Network settings (set::network-name and set::kline-address are mandatory) */
set {
    network-name "TestNet";
    default-server "irc.test.net";
    help-channel "#help";
    hiddenhost-prefix "Clk";
    kline-address "test@example.org";
    cloak-keys {
        "Q71kCDAqmT1FqNl3hsQAskLksBd5HiHP3OG3ai077MpVEn2whUQeTThxD3T8lw1XjwWDjQ1W8U1t2uCP";
        "obr42bf4H6h68784hg2JI6qePVl40D3oMedr02aj3a64d251NKgrl1icdr0q4fQVbRJf11224GoMGySd";
        "Xudc2KqmsPMc43X7Ddx826Qn0l346Ax0d67h6G38js1pIhM0n5XT0u8m1T62wS66DTt461uBU4Q56P0N";
    };
    modes-on-connect "+ixw";
    modes-on-join "+";
    maxchannelsperuser 25;
    handshake-delay 0;
    ping-cookie no;
    max-unknown-connections-per-ip 99;

    /* Disable anti-flood for testing */
    anti-flood {
        everyone {
            connect-flood 250:1;
        }
        known-users {
            join-flood 250:10;
        }
        unknown-users {
            join-flood 250:10;
        }
        channel {
            boot-delay 0;
            split-delay 0;
        }
    };
};

/* Disable connthrottle (interferes with rapid test connections) */
blacklist-module "connthrottle";

log {
    source { all; }
    destination {
        file "ircd.log" { maxsize 10M; }
    }
};
```

### Key notes on this config

- **`options { serversonly; }`** on port 6900 restricts it to S2S connections only.
- **No TLS on either port** -- the test server uses plain TCP to avoid certificate setup complexity. The `link` block uses a plaintext password `"testpassword"`.
- **`mask *`** in the link block accepts incoming connections from any IP (necessary when Docker networking assigns unpredictable IPs).
- **`hub *`** allows the linked server to introduce other servers (required for S2S).
- **`handshake-delay 0`** and **`ping-cookie no`** eliminate startup delays that would slow tests.
- **Anti-flood settings are disabled** to prevent rate-limiting during rapid test message exchanges.
- **`blacklist-module "connthrottle"`** prevents the connthrottle module from throttling rapid reconnections.
- The **cloak keys** are from the official test suite and are obviously not secret.

### S2S handshake sequence (what disirc sends)

Based on `src/irc/unreal/connection.rs`, the bridge sends:
1. `PASS :testpassword`
2. `PROTOCTL EAUTH=bridge.test.net`
3. `PROTOCTL NOQUIT NICKv2 SJOIN SJ3 TKLEXT2 NEXTBANS MTAGS ...`
4. `PROTOCTL SID=002`
5. `SERVER bridge.test.net 1 :Discord-IRC Bridge`

The UnrealIRCd server responds with its own PASS/PROTOCTL/SERVER sequence. After both sides exchange credentials, the server sends its network burst and ends with `EOS`.

## 3. Test IRC Client

### Option A: Raw TCP with tokio (recommended)

For integration tests, a simple raw TCP client is the best approach:

```rust
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

struct TestIrcClient {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: tokio::io::WriteHalf<TcpStream>,
    nick: String,
}

impl TestIrcClient {
    async fn connect(addr: &str, nick: &str) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (read, write) = tokio::io::split(stream);
        let mut client = Self {
            reader: BufReader::new(read),
            writer: write,
            nick: nick.to_string(),
        };
        // Register
        client.send(&format!("NICK {nick}")).await?;
        client.send(&format!("USER {nick} 0 * :Test User")).await?;
        // Read until 001 (RPL_WELCOME) or error
        client.expect_numeric("001").await?;
        Ok(client)
    }

    async fn send(&mut self, line: &str) -> anyhow::Result<()> {
        self.writer.write_all(format!("{line}\r\n").as_bytes()).await?;
        Ok(())
    }

    async fn read_line(&mut self) -> anyhow::Result<String> {
        let mut line = String::new();
        timeout(Duration::from_secs(5), self.reader.read_line(&mut line)).await??;
        Ok(line.trim_end().to_string())
    }

    async fn expect_numeric(&mut self, numeric: &str) -> anyhow::Result<String> {
        loop {
            let line = self.read_line().await?;
            // Respond to PING during registration
            if line.starts_with("PING") {
                let token = line.splitn(2, ':').nth(1).unwrap_or("token");
                self.send(&format!("PONG :{token}")).await?;
                continue;
            }
            if line.contains(&format!(" {numeric} ")) {
                return Ok(line);
            }
        }
    }
}
```

**Why raw TCP over the `irc` crate**:
- The `irc` crate (v1.0.0) is a full IRC client framework with config files, automatic reconnection, etc. -- far more than needed for a test helper.
- Last published in 2023; maintenance status uncertain.
- A raw client gives us exact control over timing, which is critical for integration tests that verify message ordering.
- We already use tokio throughout the project, so no new dependencies are needed.
- The test client only needs: connect, register (NICK/USER), join channels, send PRIVMSG, and read incoming lines.

### Option B: The `irc` crate

If raw TCP proves too tedious, `irc = "1.0.0"` on crates.io provides a tokio-based async client. It supports programmatic configuration (no config file needed):

```rust
let config = irc::client::prelude::Config {
    nickname: Some("testbot".to_string()),
    server: Some("127.0.0.1".to_string()),
    port: Some(6667),
    ..Default::default()
};
let client = irc::client::Client::from_config(config).await?;
client.identify()?;
```

Not recommended as a first choice due to the extra dependency and abstraction layer.

## 4. Docker in CI (GitHub Actions)

### Standard pattern: `services` block

GitHub Actions has native Docker service container support:

```yaml
jobs:
  integration-test:
    runs-on: ubuntu-latest
    services:
      unrealircd:
        image: ircd/unrealircd:latest
        ports:
          - 6667:6667
          - 6900:6900
        volumes:
          - ${{ github.workspace }}/tests/fixtures/unrealircd.conf:/ircd/unrealircd.conf
        options: --health-cmd "nc -z localhost 6667" --health-interval 2s --health-timeout 5s --health-retries 15

    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --test integration -- --include-ignored
```

**Key points**:
- The `services` block starts the container before any steps run.
- The `options` with `--health-cmd` makes the job wait until the container passes health checks before running steps.
- `nc -z localhost 6667` is a TCP connect check -- it succeeds once UnrealIRCd is accepting connections.
- Ports are mapped from the container to the runner's localhost.

### Alternative: docker-compose in a step

For more control (or if the config file path is complex):

```yaml
    steps:
      - uses: actions/checkout@v4
      - name: Start UnrealIRCd
        run: docker compose -f tests/docker-compose.yml up -d --wait
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --test integration -- --include-ignored
      - name: Teardown
        if: always()
        run: docker compose -f tests/docker-compose.yml down
```

The `--wait` flag (docker compose v2.1+) blocks until health checks pass. Ubuntu runners ship docker compose v2.

### Running Docker from within a Rust test

For tests that manage their own container lifecycle:

```rust
use std::process::Command;
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout, Duration};

async fn start_unrealircd() -> String {
    let container_id = String::from_utf8(
        Command::new("docker")
            .args(["run", "-d", "--rm",
                   "-p", "6667:6667", "-p", "6900:6900",
                   "-v", "./tests/fixtures/unrealircd.conf:/ircd/unrealircd.conf:ro",
                   "ircd/unrealircd:latest"])
            .output()
            .expect("docker run failed")
            .stdout
    ).unwrap().trim().to_string();

    // Wait for TCP readiness
    wait_for_port("127.0.0.1:6667", Duration::from_secs(30)).await;
    container_id
}

async fn wait_for_port(addr: &str, max_wait: Duration) {
    let deadline = tokio::time::Instant::now() + max_wait;
    loop {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("Timed out waiting for {addr}");
        }
        sleep(Duration::from_millis(200)).await;
    }
}

fn stop_unrealircd(container_id: &str) {
    Command::new("docker")
        .args(["stop", container_id])
        .output()
        .expect("docker stop failed");
}
```

**Recommendation**: Use the GitHub Actions `services` block for CI, and in-test `docker run`/`docker stop` for local development. Both approaches are well-established.

## 5. Startup Timing

### How to know when UnrealIRCd is ready

**Log message**: UnrealIRCd emits `unreal_log(ULOG_INFO, "main", "UNREALIRCD_START", NULL, "UnrealIRCd started.")` in `src/ircd.c` line 857 after `config_run()` completes. The event ID is `UNREALIRCD_START`. This is logged to `ircd.log` inside the container.

**TCP readiness** (recommended for tests): Simply retry a TCP connection to port 6667 (or 6900) in a loop with a short sleep. Once `connect()` succeeds, the server is accepting connections. This is:
- Simpler than parsing container logs
- Works identically in CI and local development
- The same approach used by Docker health checks (`nc -z localhost 6667`)

**Expected startup time**: UnrealIRCd is a C program that starts very fast (under 1 second typically). In Docker, container overhead adds another 1-2 seconds. Total time to TCP-ready is usually 2-3 seconds.

**Recommended readiness check**:

```rust
/// Poll TCP until the server accepts connections, or panic after timeout.
async fn wait_for_port(addr: &str, max_wait: Duration) {
    let start = tokio::time::Instant::now();
    loop {
        match TcpStream::connect(addr).await {
            Ok(_) => return,
            Err(_) if start.elapsed() < max_wait => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("Server at {addr} not ready after {max_wait:?}: {e}"),
        }
    }
}
```

Use a 10-second timeout in CI (conservative for slow runners) and 5 seconds locally.

## References

- [ircd/unrealircd on Docker Hub](https://hub.docker.com/r/ircd/unrealircd)
- [UnrealIRCd Configuration docs](https://www.unrealircd.org/docs/Configuration) -- accessed 2026-03-30
- [UnrealIRCd Link block docs](https://www.unrealircd.org/docs/Link_block) -- accessed 2026-03-30
- [UnrealIRCd Listen block docs](https://www.unrealircd.org/docs/Listen_block) -- accessed 2026-03-30
- [UnrealIRCd Set block docs](https://www.unrealircd.org/docs/Set_block) -- accessed 2026-03-30
- [Tutorial: Linking servers](https://www.unrealircd.org/docs/Tutorial:_Linking_servers) -- accessed 2026-03-30
- [unrealircd-tests repo (unreal60 branch)](https://github.com/unrealircd/unrealircd-tests/tree/unreal60) -- accessed 2026-03-30
- [unrealircd-tests/serverconfig/unrealircd/irc1.conf](https://github.com/unrealircd/unrealircd-tests/blob/unreal60/serverconfig/unrealircd/irc1.conf) -- accessed 2026-03-30
- [unrealircd-tests/serverconfig/unrealircd/common.conf](https://github.com/unrealircd/unrealircd-tests/blob/unreal60/serverconfig/unrealircd/common.conf) -- accessed 2026-03-30
- [UnrealIRCd source: src/ircd.c (startup log)](https://github.com/unrealircd/unrealircd/blob/unreal60_dev/src/ircd.c) -- accessed 2026-03-30
- [UnrealIRCd example.conf](https://github.com/unrealircd/unrealircd/blob/unreal60_dev/doc/conf/examples/example.conf) -- accessed 2026-03-30
- [irc crate on crates.io](https://crates.io/crates/irc) -- accessed 2026-03-30
- [agamemnon23/unrealircd Docker image](https://github.com/agamemnon23/unrealircd) -- accessed 2026-03-30
- [GitHub Actions service containers](https://docs.github.com/en/actions/using-containerized-services/about-service-containers)
- [Docker Compose --wait flag](https://docs.docker.com/compose/how-tos/wait/)
