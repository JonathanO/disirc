# CLAUDE.md — disirc project instructions

## What this project is

`disirc` is a Discord ↔ IRC bridge daemon written in Rust. It relays messages bidirectionally between mapped Discord channels and IRC channels, running as a single async process. It peers with UnrealIRCd using the server-to-server (S2S) protocol, presenting Discord users as real IRC pseudoclients on the network.

## Session continuity

At the **start** of every session:
1. Re-read this file (`CLAUDE.md`) in full.
2. Read `TODO.md` to understand what was in progress and what is pending.
3. Read `LAYOUT.md` for a map of every source module and what belongs in it.
4. Sync the in-session `TodoWrite` task list with `TODO.md`.

At the **start of each new implementation task** (new spec, new module, new bug fix), re-read the relevant spec and the **Spec-driven development workflow** and **Done means** sections of this file before writing any code.

Update `TODO.md` and the relevant `specs/<spec>/TODO.md` **immediately** whenever:
- A task is completed
- A new task is identified
- A task's status changes (blocked, deferred, etc.)

Do not batch `TODO.md` updates to the end of a session — update them in place as work happens so the files always reflect reality.

Use subagents when tasks can run in parallel, require isolated context, or involve independent workstreams that don't need to share state. For simple tasks, sequential operations, single-file edits, or tasks where you need to maintain context across steps, work directly rather than delegating.
Please write a high-quality, general-purpose solution using the standard tools available. Do not create helper scripts or workarounds to accomplish the task more efficiently. Implement a solution that works correctly for all valid inputs, not just the test cases. Do not hard-code values or create solutions that only work for specific test inputs. Instead, implement the actual logic that solves the problem generally.
Focus on understanding the problem requirements and implementing the correct algorithm. Tests are there to verify correctness, not to define the solution. Provide a principled implementation that follows best practices and software design principles.
If the task is unreasonable or infeasible, or if any of the tests are incorrect, please inform me rather than working around them. The solution should be robust, maintainable, and extendable.
If you intend to call multiple tools and there are no dependencies between the tool calls, make all of the independent tool calls in parallel. Prioritize calling tools simultaneously whenever the actions can be done in parallel rather than sequentially. For example, when reading 3 files, run 3 tool calls in parallel to read all 3 files into context at the same time. Maximize use of parallel tool calls where possible to increase speed and efficiency. However, if some tool calls depend on previous calls to inform dependent values like the parameters, do NOT call these tools in parallel and instead call them sequentially. Never use placeholders or guess missing parameters in tool calls.
Tests should be written first for bug fixes, as the test case serves to prevent regressions in future.

## Spec-driven development workflow

1. **Specs live in `specs/<name>/spec.md`**. Before implementing any feature, read the relevant spec file(s). Each spec directory also contains a `TODO.md` tracking tasks for that spec.
2. **No spec = no implementation**. If a feature has no spec, write or extend the spec first, get it reviewed, then implement.
3. **Update `SPECS.md`** when a spec moves from Pending → In Progress → Implemented.

## Key dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `tokio-rustls` | TLS for the UnrealIRCd server link |
| `serenity` | Discord Gateway + REST client |
| `serde` + `toml` | Config deserialization |
| `tracing` + `tracing-subscriber` | Structured logging |
| `thiserror` | Derive macros for typed error enums in each module |
| `anyhow` | Error propagation with context chains in the application/connection layer |
| `proptest` | Property-based testing (dev) |
| `cargo-deny` | Dependency audit — CVEs, licences, duplicates (CI) |

## Project structure

```
src/                    — implementation (added as specs are approved)
specs/                  — behavioral specs (source of truth)
  <name>/spec.md        — the spec itself
  <name>/TODO.md        — per-spec task list
research/               — research notes, protocol analysis, prior art (source material for specs)
tests/                  — integration tests
config.toml             — example/local config (never commit real tokens)
SPECS.md                — spec implementation status tracker (links to per-spec dirs)
TODO.md                 — high-level status index (links to per-spec TODOs)
deny.toml               — cargo-deny configuration
```

## Code style

- Unit tests go inline with `#[cfg(test)]` modules.
- Integration tests that require real network connections or credentials must be annotated `#[ignore]` until a mock harness exists.
- Integration tests that don't require external connections go in `tests/` (e.g., `tests/config.rs`, `tests/formatting.rs`).
- Use **property-based tests** (`proptest`) wherever a function has edge-case-prone input domains — formatting transforms, string validation, message splitting, and routing logic are all good candidates. Prefer `proptest!` macros over hand-picked example inputs for these cases.
- **Async event serialization**: IRC and Discord events must be funnelled through `tokio::sync::mpsc` channels to a single processing task per direction. Do not `tokio::spawn` a new task per incoming event — concurrent handlers will race on shared state.
- `#![deny(unsafe_code)]` must appear at the crate root. There is no justified use of `unsafe` in this project.

### What to test

**Test behaviour, not construction.** Do not write tests that merely construct a struct or enum variant and assert that the fields have the values you set — there is no logic there and the compiler already guarantees it. The rule of thumb: if removing the test would not catch any real bug, the test should not exist.

Write tests for:
- Functions with non-trivial logic (parsing, translation, transformation, routing).
- Edge cases and error paths of those functions.
- Any invariant that the compiler cannot enforce.

Do **not** write tests for:
- Constructing plain data types (`struct Foo { x: 1, y: 2 }` round-trips).
- Enum variant existence or field names.
- Trivial getters or `Clone`/`Debug` derives.

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
- A `research/` file: `[research/topic.md](../../research/topic.md)` (two levels up from `specs/<name>/spec.md`)
- A primary source directly if no research file exists: `[Title](URL) — accessed YYYY-MM-DD`

Do not write spec behaviour from memory alone when a primary source exists and is fetchable.

## Security

- Never commit `config.toml` containing real tokens. Use `config.example.toml` for examples.
- The Discord bot token and IRC connection passwords are secrets — treat them accordingly.
- `@everyone` and `@here` must be suppressed on all IRC → Discord paths by default. This is a mandatory safety rule, not an operator option.
