# Developing disirc

## Prerequisites

- **Rust** (stable, edition 2024) — install via [rustup](https://rustup.rs/)
- **cargo-deny** — dependency audit tool
- **cargo-mutants** — mutation testing tool

Install the CLI tools:

```sh
cargo install cargo-deny cargo-mutants
```

Enable the pre-commit hook:

```sh
git config core.hooksPath hooks
```

## Building

```sh
cargo build
```

For a release build:

```sh
cargo build --release
```

## Running

1. Copy the example config and fill in your real values:

   ```sh
   cp config.example.toml config.toml
   ```

2. Edit `config.toml` with your Discord bot token, IRC uplink details, SID, and bridge channel mappings. See the comments in `config.example.toml` for guidance.

3. Run the daemon:

   ```sh
   cargo run
   ```

   Or with a custom config path:

   ```sh
   cargo run -- --config /path/to/config.toml
   ```

4. Control log verbosity with `RUST_LOG`:

   ```sh
   RUST_LOG=debug cargo run
   RUST_LOG=disirc=trace,serenity=warn cargo run
   ```

> **Never commit `config.toml`** — it contains secrets. The `.gitignore` already excludes it.

## Testing

### Unit and integration tests

```sh
cargo test
```

Some tests that require real network connections are marked `#[ignore]`. To include them:

```sh
cargo test -- --include-ignored
```

### Linting

Clippy is configured with pedantic and cargo lint groups in `Cargo.toml` under `[lints.clippy]`. Run with warnings as errors:

```sh
cargo clippy -- -D warnings
```

### Formatting

```sh
cargo fmt --check    # verify
cargo fmt            # auto-fix
```

### Dependency audit

Checks for CVEs, licence violations, and banned crates (configured in `deny.toml`):

```sh
cargo deny check
```

### Quality gate

All four checks must pass before any commit:

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo deny check
```

### Pre-commit hook

A pre-commit hook that enforces the first three checks (fmt, clippy, test) is included in `hooks/`. To activate it after cloning:

```sh
git config core.hooksPath hooks
```

This runs automatically on every `git commit`. `cargo deny` is excluded from the hook because it requires network access and is slow — run it manually or rely on CI.

> The hook is already activated if you followed this guide. It applies only to your local clone (`core.hooksPath` is not committed to the repo config).

## Mutation testing

[cargo-mutants](https://github.com/llogiq/mutagen) systematically modifies your code and checks that at least one test fails for each mutation. This catches gaps where tests pass but don't actually verify behaviour.

### Run mutation tests for a specific module

```sh
cargo mutants --file src/bridge.rs
cargo mutants --file src/formatting.rs
cargo mutants --file src/pseudoclients.rs
```

### Run mutation tests for the whole crate

```sh
cargo mutants
```

> **Note:** Mutation testing is slow (minutes to hours depending on crate size). Scoping to a single file is recommended during development.

### Interpreting results

- **caught** — a test detected the mutation (good)
- **unviable** — the mutation did not compile (neutral)
- **missed** — no test caught the mutation (potential test gap)

Missed mutants should be investigated. If they represent real test gaps, write tests to catch them. If they are equivalent mutants (e.g., noop shims, non-deterministic clock functions, integration-only async loops), document them in the relevant `specs/<name>/TODO.md`.

### Closing out a spec

Before marking a spec as Implemented in `SPECS.md`, mutation testing must pass with zero missed mutants that represent real test gaps:

```sh
cargo mutants --file src/<module>.rs
```

See `CLAUDE.md` section "Closing out a spec" for the full policy.

## Project structure

See `LAYOUT.md` for a detailed map of every source module. The high-level architecture:

```
main.rs
  |
  +-- spawns IRC connection task (src/irc/)
  |     communicates via mpsc channels: S2SEvent / S2SCommand
  |
  +-- spawns Discord connection task (src/discord/)
  |     communicates via mpsc channels: DiscordEvent / DiscordCommand
  |
  +-- runs bridge loop (src/bridge.rs)
        owns BridgeMap, IrcState, DiscordState, PseudoclientManager
        routes messages bidirectionally
        handles config hot-reload via ControlEvent
```
