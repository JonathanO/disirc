# CLAUDE.md — disirc project instructions

## What this project is

`disirc` is a Discord ↔ IRC bridge daemon written in Rust. It relays messages bidirectionally between mapped Discord channels and IRC channels, running as a single async process. It peers with UnrealIRCd using the server-to-server (S2S) protocol, presenting Discord users as real IRC pseudoclients on the network.

## Session continuity

At the **start** of every session, read `TODO.md` to understand what was in progress and what is pending. Sync the in-session `TodoWrite` task list with `TODO.md` at session start.

Update `TODO.md` **immediately** whenever:
- A task is completed
- A new task is identified
- A task's status changes (blocked, deferred, etc.)

Do not batch `TODO.md` updates to the end of a session — update it in place as work happens so the file always reflects reality.

## Spec-driven development workflow

1. **Specs live in `specs/`**. Before implementing any feature, read the relevant spec file(s).
2. **No spec = no implementation**. If a feature has no spec, write or extend the spec first, get it reviewed, then implement.
3. **Tests before code**. For each spec being implemented, write failing tests first, then write the minimum code to make them pass.
4. **Update `SPECS.md`** when a spec moves from Pending → In Progress → Implemented.

## Key dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `tokio-rustls` | TLS for the UnrealIRCd server link |
| `serenity` | Discord Gateway + REST client |
| `serde` + `toml` | Config deserialization |
| `tracing` + `tracing-subscriber` | Structured logging |
| `proptest` | Property-based testing (dev) |
| `cargo-deny` | Dependency audit — CVEs, licences, duplicates (CI) |

## Project structure

```
src/          — implementation (added as specs are approved)
specs/        — behavioral specs (source of truth)
research/     — research notes, protocol analysis, prior art (source material for specs)
tests/        — integration tests
config.toml   — example/local config (never commit real tokens)
SPECS.md      — spec implementation status tracker
deny.toml     — cargo-deny configuration
```

## Code style

- Unit tests go inline with `#[cfg(test)]` modules.
- Integration tests that require real network connections or credentials must be annotated `#[ignore]` until a mock harness exists.
- Integration tests that don't require external connections go in `tests/` (e.g., `tests/config.rs`, `tests/formatting.rs`).
- Use **property-based tests** (`proptest`) wherever a function has edge-case-prone input domains — formatting transforms, string validation, message splitting, and routing logic are all good candidates. Prefer `proptest!` macros over hand-picked example inputs for these cases.
- **Async event serialization**: IRC and Discord events must be funnelled through `tokio::sync::mpsc` channels to a single processing task per direction. Do not `tokio::spawn` a new task per incoming event — concurrent handlers will race on shared state.
- `#![deny(unsafe_code)]` must appear at the crate root. There is no justified use of `unsafe` in this project.

## Done means

A task is not done until all of the following pass with no errors or warnings:

```
cargo test                          # all non-ignored tests green
cargo clippy -- -D warnings         # zero warnings, including pedantic and cargo groups
cargo fmt --check
cargo deny check                    # no CVEs, licence violations, or banned crates
```

Clippy lint groups are configured in `Cargo.toml` under `[lints.clippy]`:
- `pedantic = "warn"` — stricter correctness and style
- `cargo = "warn"` — Cargo.toml hygiene

## Closing out a spec (Implemented)

Before marking a spec as Implemented in `SPECS.md`, run mutation testing scoped to the relevant module and address any surviving mutants:

```
cargo mutants -p disirc -- <module-path>
```

Zero surviving mutants is required. If a mutant survives, either fix the test gap or update the spec to explicitly exclude that case.

## When to commit

Commit at these natural boundaries — not before:

1. **Spec approved** — after a spec has been written and reviewed, before any implementation begins. Message: `spec: add/update <spec-name>`.
2. **Task complete** — after each individual implementation task passes `cargo test`, `cargo clippy -- -D warnings`, and `cargo fmt --check`. Message: `feat(<module>): <task description>`.
3. **Spec implemented** — after all tasks for a spec are done and mutation testing is clean, mark the spec Implemented in `SPECS.md`. Message: `chore: mark <spec-name> as Implemented`.

Each task should be a single focused commit. Do not batch multiple tasks into one commit.

Do **not** commit:
- Failing tests or clippy warnings.
- Work in progress mid-implementation.
- Spec drafts that haven't been reviewed.

## Research workflow

### When to do research

Before writing or updating a spec that touches an external protocol, API, or prior art, research the topic and record findings in `research/`. Specs cite research files; research files cite primary sources (URLs, RFCs, commit hashes).

**Always check `research/INDEX.md` first.** If the topic is already covered, read the existing file rather than re-investigating. After completing new research, add a row to `research/INDEX.md` before proceeding.

### Parallelise research with subagents

When a task involves multiple independent research questions, launch them as parallel subagents in a single message rather than sequentially. Examples of parallelisable work:
- Analysing multiple prior-art repositories simultaneously
- Fetching protocol documentation alongside API documentation
- Checking multiple external sources for the same fact

Do not serialise research steps that have no dependency on each other.

### Research files (`research/`)

Each research file covers one topic and follows this structure:

```markdown
# <Topic>

## Summary
Two or three sentences: what was investigated and the key conclusion.

## Findings
Detailed notes, relevant quotes, code snippets, gotchas.

## References
- [Title](URL) — accessed YYYY-MM-DD
- [RFC NNNN §N.N](https://tools.ietf.org/html/rfcNNNN#section-N.N) — accessed YYYY-MM-DD
```

### References in specs

Every spec that draws on external sources must include a `## References` section at the bottom. Cite either:
- A `research/` file: `[research/topic.md](../research/topic.md)`
- A primary source directly if no research file exists: `[Title](URL) — accessed YYYY-MM-DD`

Do not write spec behaviour from memory alone when a primary source exists and is fetchable.

## Security

- Never commit `config.toml` containing real tokens. Use `config.example.toml` for examples.
- The Discord bot token and IRC connection passwords are secrets — treat them accordingly.
- `@everyone` and `@here` must be suppressed on all IRC → Discord paths by default. This is a mandatory safety rule, not an operator option.
