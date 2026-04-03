# disirc task runner — install just: cargo install just

set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# Run all quality checks (same as pre-commit hook + cargo deny)
check: fmt clippy test deny

# Format check
fmt:
    cargo fmt --check

# Lint
clippy:
    cargo clippy -- -D warnings

# Run unit and integration tests
test:
    cargo test

# Dependency audit
deny:
    cargo deny check

# Run Layer 3 e2e tests (requires Docker)
e2e:
    cargo test --test e2e_irc -- --include-ignored --nocapture

# Run Layer 4 e2e tests (requires Docker + Discord credentials)
e2e-discord:
    cargo test --test e2e_discord -- --include-ignored --nocapture --test-threads=1

# Run the bridge with debug logging
[unix]
run:
    RUST_LOG=disirc=debug cargo run

# Run the bridge with debug logging
[windows]
run:
    $env:RUST_LOG="disirc=debug"; cargo run

# Run mutation testing on a specific file
mutants file:
    cargo mutants --timeout 20 --file {{file}} -- --lib

# Run mutation testing on the whole codebase
mutants-all:
    cargo mutants --timeout 20 -j 4 -- --lib

# Start a local UnrealIRCd in the background
ircd-start:
    docker run -d --name unrealircd -p 6667:6667 -p 6900:6900 ghcr.io/jonathano/disirc-unrealircd-test:latest

# Start a local UnrealIRCd in the foreground (Ctrl+C to stop)
ircd:
    docker run --rm --name unrealircd -p 6667:6667 -p 6900:6900 ghcr.io/jonathano/disirc-unrealircd-test:latest

# Stop the local UnrealIRCd
ircd-stop:
    docker stop unrealircd
    docker rm unrealircd
