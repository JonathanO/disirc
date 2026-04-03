# Contributing to disirc

Thank you for your interest in contributing to disirc!

## Developer Certificate of Origin (DCO)

This project uses the [Developer Certificate of Origin](https://developercertificate.org/) (DCO). By contributing to this project, you agree that your contributions are your own work (or you have the right to submit them) and that they can be distributed under the project's MIT license.

Please sign off your commits by adding a `Signed-off-by` line:

```
Signed-off-by: Your Name <your.email@example.com>
```

You can do this automatically with `git commit -s`.

## Getting started

See [DEVELOPING.md](DEVELOPING.md) for:

- Build and test instructions
- Local development setup with Docker + UnrealIRCd
- Discord bot setup for Layer 4 tests
- Code quality requirements

## Quality requirements

All contributions must pass before merging:

```sh
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo deny check
```

Or use the task runner: `just check`

## Code style

- See `CLAUDE.md` for detailed coding guidelines
- Unit tests go inline with `#[cfg(test)]` modules
- Use property-based tests (`proptest`) for edge-case-prone functions
- No `unsafe` code
- Error handling: `thiserror` for library errors, `anyhow` for application-layer errors

## Specs

Features are driven by specs in `specs/`. Before implementing a new feature, write or update the relevant spec and get it reviewed. See `SPECS.md` for the current status of each spec.

## Commit messages

Use conventional commit prefixes:

- `feat(<module>)` — new feature
- `fix(<module>)` — bug fix
- `refactor(<module>)` — code restructuring
- `test(<module>)` — test additions/changes
- `chore(<scope>)` — maintenance
- `spec:` — spec additions/changes
- `docs:` — documentation

## Reporting issues

Please open a GitHub issue for bugs, feature requests, or questions.
