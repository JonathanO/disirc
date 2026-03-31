# Test-only UnrealIRCd image built from the official source tarball.
# The test config (unrealircd.conf from the same directory) is baked in.
#
# Client port: 6667  TLS: 6697  S2S port: 6900
#
# Build: docker build -t disirc-unrealircd-test tests/fixtures/
# (Cargo test helpers build this automatically on first run.)

FROM debian:bookworm-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev libpcre2-dev \
    libcurl4-openssl-dev libc-ares-dev libsodium-dev \
    wget ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# UnrealIRCd refuses to build or run as root. Create a dedicated user whose
# home directory becomes the install prefix (/home/ircd/unrealircd).
RUN useradd -m -s /bin/sh ircd

USER ircd
WORKDIR /home/ircd

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

# Generate a self-signed TLS certificate. UnrealIRCd 6.x requires at least one
# TLS listener; the cert lives at the compiled-in default path.
RUN openssl req -x509 -newkey rsa:2048 -days 3650 -nodes \
    -keyout /home/ircd/unrealircd/conf/tls/server.key.pem \
    -out  /home/ircd/unrealircd/conf/tls/server.cert.pem \
    -subj "/CN=irc.test.net"

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 libpcre2-8-0 libcurl4 libc-ares2 libsodium23 \
    && rm -rf /var/lib/apt/lists/*

# Recreate the same user so file ownership and the compiled-in home path
# (/home/ircd/unrealircd/...) match at runtime.
RUN useradd -m -s /bin/sh ircd

# Copy the compiled install. Paths are baked into the binary as
# /home/ircd/unrealircd/..., so the layout must be preserved exactly.
COPY --from=builder /home/ircd/unrealircd /home/ircd/unrealircd

# Bake in the test config, overriding the default conf installed by make install.
# This avoids bind-mount path issues on Windows hosts.
COPY --chown=ircd:ircd unrealircd.conf /home/ircd/unrealircd/conf/unrealircd.conf

EXPOSE 6667 6697 6900
USER ircd
WORKDIR /home/ircd/unrealircd
# -F runs in the foreground (no fork), required for Docker.
CMD ["/home/ircd/unrealircd/bin/unrealircd", "-F"]
