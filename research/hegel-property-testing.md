# Hegel property-based testing for Rust

## Summary

This note compares Hegel (crate `hegeltest`) with proptest for the disirc test
suite. Hegel ports the Hypothesis engine to Rust. It gives better shrinking, a
failure database, and settings profiles. It is in beta and has frequent
breaking releases. disirc uses it with the `static-engine` feature, so the
engine is in `Cargo.lock`.

## Findings

### Architecture

- At launch, Hegel ran Hypothesis in a Python server. The Rust crate now runs
  a native Rust engine in-process and has no Python dependency (changelog:
  "Hegel now runs entirely in-process").
- By default, the build script compiles the engine (`hegeltest-c`) as a
  shared library with a nested `cargo build`, and the tests load it at
  runtime. That nested build resolves its own dependencies, so `Cargo.lock`
  and `cargo deny` do not control them. If no local engine checkout exists,
  the build script fetches the pinned `hegeltest-c` source from crates.io.
- The `static-engine` feature links the engine as a normal dependency. Its
  dependency tree is then in `Cargo.lock`, and `cargo deny` checks it. The
  crate docs warn that this exposes the engine's dependencies to feature
  unification, which can change type inference in unrelated code. disirc
  compiled with no changes.

### API notes

- `#[hegel::test] fn name(tc: TestCase)` replaces a `proptest!` test.
  `tc.draw(generator)` replaces each `name in strategy` argument.
- `tc.draw` needs a `PrintableGenerator`. Helper functions return
  `impl PrintableGenerator<T>`. `use hegel::prelude::*` imports `gs`,
  `TestCase`, `Generator`, and `PrintableGenerator`.
- `gs::from_regex` matches the full string by default, as proptest does. It
  uses Python `re` syntax, which has no Unicode classes such as `\PC`. Use
  `gs::text().exclude_categories(&["C"])` for that class.
- `hegel::one_of!` has no weights. `gs::sampled_from` accepts a `Vec` or a
  slice.
- `#[hegel::explicit_test_case(name = value)]` runs a fixed input first. The
  name is the binding of the `let` that receives the draw.
- The default case count is 100. `hegel.toml` at the package root sets
  counts per profile. A section for a shipped profile merges over it.
- The `ci` profile is selected automatically on CI (for example, when
  `GITHUB_ACTIONS` is set). It is derandomized and does not use the failure
  database, so CI runs are repeatable.
- Locally, the failure database is in `.hegel/examples`.

### Stability

- The README says that the library is in beta and "may make breaking changes".
- Version 0.49.2 was published on 2026-10-07. The changelog has 235 release
  entries since the repository was created on 2026-01-14.
- In the 0.49.2 changelog, every release marked as breaking is an x.y.0
  release (for example 0.40.0 and 0.33.0). Renovate merges patch releases
  automatically, but only after CI passes. If a release breaks the build,
  CI fails and the update does not merge.

### Measured cost in disirc

- The library tests took 0.59 s at 100 cases, 1.31 s at 1000 cases, and
  5.30 s at 5000 cases for each property.

## References

- [Antithesis blog: Introducing Hegel](https://antithesis.com/blog/2026/hegel/) — accessed 2026-10-08
- [hegeltest on docs.rs](https://docs.rs/crate/hegeltest/latest) — accessed 2026-10-08
- [hegeldev/hegel-rust](https://github.com/hegeldev/hegel-rust) — README, CHANGELOG.md, Cargo.toml, build.rs; accessed 2026-10-08
- [hegeldev/hegel-skill](https://github.com/hegeldev/hegel-skill) — agent skill for writing Hegel tests; accessed 2026-10-08
