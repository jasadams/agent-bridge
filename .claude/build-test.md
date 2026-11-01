# Build, test, review, and cleanup

## Setup and scope

This is one Rust package, `agent-bridge`, with local patched libp2p dependencies
under `vendor/`. Use the checked-in `Cargo.lock` and `--locked`. The manifest
requires Rust 1.94 or newer; CI pins Rust 1.98.0 with rustfmt and Clippy. Install
that toolchain with `rustup toolchain install 1.98.0 --profile minimal --component
rustfmt --component clippy` and select it with `rustup override set 1.98.0` in the
worktree. Python 3 and a POSIX shell support installer/release checks. Linux needs
a C compiler and standard build tools for native dependencies (SQLite is bundled).
Rust-only work needs no Node.js toolchain. OpenCode plugin work additionally
requires Bun 1.3.11; `plugins/opencode/bun.lock` pins the V2 plugin/client packages
at 2.2.5 and TypeScript dependencies. Run `bun install --frozen-lockfile`,
`bun run typecheck`, and `bun test` from that directory. The release matrix runs
these checks on Linux and macOS. See `docs/opencode.md` and the plugin README for
the isolated native compatibility harness; it needs OpenCode 2.2.5 and never
uses owner configuration or credentials.

Read the relevant feature docs and the Trakkt ticket before changing behavior.
There is no repository-local ticket ledger; `docs/p2p-progress.md` and security
review reports provide progress context, while Trakkt is authoritative. Build duration has not been measured;
do not borrow timing claims or build queues from other repositories.

## Choose validation for the current change

Documentation and workflow bootstrap: inspect links, configuration, shell syntax,
and `git diff --check`; do not compile solely for documentation. A workflow change
also needs independent review of its triggers, permissions, and commands.

Rust executable changes: run the following from the current worktree, selecting
focused tests for the affected behavior and then the package suite as appropriate:

```sh
cargo fmt --check
cargo check --locked -p agent-bridge --all-targets
cargo clippy --locked -p agent-bridge --all-targets -- -D warnings
cargo test --locked -p agent-bridge --no-fail-fast
```

Inspect formatting failures before fixing them; format only touched code and
report unrelated baseline drift. Keep test flags, toolchain, and artifact paths
consistent across retries. Collect independent test failures before editing;
`--no-fail-fast` belongs before test-harness arguments. Record command exits,
failures and blocked tests honestly. Do not count unexecuted checks as passing.

Changes to patched vendor crates need their own manifest checks/tests. For the
relay crate, CI uses `--manifest-path vendor/libp2p-relay/Cargo.toml` and
`--target-dir target` for tests and Clippy, plus a separate formatting check.
Other vendor changes require equivalent targeted validation.

Installer/release changes additionally use `sh -n install.sh` and the relevant
`python3 tools/test_installer.py`, `python3 tools/test_packaging.py`, and
`python3 tools/test_release.py` checks. Network/broker scenarios in `tools/` may
start real processes; read their prerequisites and use isolated temporary state.
Never modify the user's running service or private state for validation.

## CI and backlog-fast

`.github/workflows/release.yml` already runs on pull requests and main pushes.
Its four-platform matrix validates formatting, package tests and Clippy, vendor
relay tests/formatting/Clippy, installer and release Python checks, and release
binary builds. Only semantic version tag pushes can publish. See
`docs/releasing.md` for the release process. Do not add a duplicate PR pipeline.

Backlog-fast runs selected local checks and independent review, then waits within
the same turn for all current-head PR checks. Empty, pending, cancelled, or failed
checks are not green. Leave a successful PR open for merge-sweeper and record any
runtime or manual QA not exercised. Signing is retired.

All builds, tests, and Clippy commands must run in the foreground; consume the
actual exit before continuing or ending the turn. Do not use `&`, `nohup`, detached
watchers, or Monitor. Unrelated failures are reported, not repaired in this ticket.

## Review records and safe cleanup

Append each review entry through `scripts/append-review-log.sh` on stdin. It
resolves the canonical worktree and serializes appends using a lock, so daily logs
survive removal of ticket worktrees. Review logs are local and ignored by Git.
If append fails, preserve the complete entry elsewhere and report the failure.

No project-specific in-flight, reclamation, or worktree-retirement helper is
provided. Use backlog-fast's repository/worktree protocol portable in-flight
fallback, including its atomic pickup lock and explicit self-exclusion before
review. Automated stranded-claim reclamation is unavailable: preserve evidence
and artifacts rather than guessing ownership or abandoning another worker's work.
Do not copy scripts from a different repository.

Before cleanup, inspect `git status`, `git worktree list`, and unpublished commits.
Preserve review logs left in a worktree by moving their entries through the append
helper before removing that tree. Remove only the clean, published worktree and
branch owned by this run with standard Git commands; never force removal or delete
the invoking checkout, another worker's tree, or unfinished work. Leave artifacts
in place and report them when safe ownership or publication cannot be established.
