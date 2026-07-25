# AGENTS.md file for nochange

`nochange` is a Rust command-line application that provides OfflineIMAP- and
msmtp-style functionality for Microsoft 365 accounts through Microsoft Graph.
It synchronizes mail to Maildir, stores incremental state in SQLite, authenticates
with Microsoft Entra ID, and exposes a sendmail-compatible interface.

## Development

- Use stable Rust with the repository's Rust 2024 edition.
- Use Cargo for dependency management, builds, tests, formatting, and linting.
- Write tests that express the plan and requirements before implementing code
  that makes them pass.
- Prefer well-maintained, community-accepted crates from crates.io over custom
  implementations, while avoiding unnecessary dependencies.
- Add Rustdoc (`///` or `//!`) to all non-trivial public items and to private
  functions or objects whose invariants are not obvious.
- Follow standard Rust naming conventions. Use action-oriented names for
  operations such as `get_`, `build_`, `parse_`, `collect_`, and `apply_`;
  reserve noun-only names for accessors where that reads naturally.
- Represent expected failures with `Result` and meaningful error variants.
  Prefer existing error types when they communicate the failure clearly, and
  use `thiserror` for domain errors that need dedicated context.
- Do not use `assert!`, `panic!`, `unwrap`, or `expect` in production code.
  These are acceptable in tests when they make the expected invariant clear.
- Keep async work non-blocking. Use Tokio-aware filesystem, network, timing, and
  synchronization APIs inside async paths unless a bounded synchronous operation
  is intentionally isolated.
- Preserve opaque Microsoft Graph IDs, delta links, tokens, and MIME bytes
  without logging or normalizing them unnecessarily.
- Run formatting, tests, and linting before completing every code task:

  ```console
  cargo fmt --check
  cargo test --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  ```

## Testing

- Target exactly 100% coverage of behavior changed by the task: every added or
  changed behavior must have a test that would fail without that change. Do not
  add unrelated coverage for pre-existing behavior.
- Use Rust's built-in test framework with `#[test]` and `#[tokio::test]`.
- Put private implementation tests in the source module's `#[cfg(test)]` module.
  Put public API and end-to-end component tests in the corresponding integration
  test, for example `src/graph.rs` → `tests/graph.rs`.
- Prefer small trait-backed fakes for application boundaries. Use `wiremock` for
  HTTP behavior, `tempfile` for isolated filesystems, and `assert_cmd` with
  `predicates` for CLI behavior.
- Test this project's behavior rather than standard-library or third-party
  implementation details, except where an intentional integration contract
  crosses that boundary.
- Consolidate repeated cases into table-driven loops when only inputs and
  expected outputs differ.
- Test time-dependent code through injected clocks or sleepers; do not rely on
  wall-clock timing when a deterministic boundary is practical.
- Prefer assertions on structured events, return values, filesystem state, and
  database state over brittle matching of complete log lines. Tests that protect
  redaction or stable user-facing diagnostics may assert the relevant fragments.
- Verify failure and interruption paths for synchronization changes, including
  staging cleanup, checkpoint replay, idempotency, and conflict preservation.

## Repository Structure

- `Cargo.toml`: Package metadata and dependency declarations.
- `Cargo.lock`: Cargo-generated dependency lockfile for this application; update
  it through Cargo rather than editing it by hand.
- `README.md`: Human-readable installation, configuration, and usage contract.
- `PLAN.md`: Architecture, synchronization semantics, phases, and acceptance
  criteria.
- `nochange.conf`: Example configuration file.
- `src/`: Library and executable source code.
- `tests/`: Integration and CLI tests.

## Boundaries

- **Ask first**
  - Large cross-module refactors.
  - New dependencies with broad impact.
  - Destructive data, Maildir, state-schema, or migration changes.
- **Never**
  - Commit secrets, credentials, authorization codes, tokens, message content,
    or account-specific state.
  - Edit generated files by hand when a generation workflow exists.
  - Use destructive Git operations unless explicitly requested.
