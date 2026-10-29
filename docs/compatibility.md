# Native compatibility qualification

Qualification combines real client execution with a deterministic loopback model
fixture. It verifies transport and actual native MCP tool execution, including
explicit acknowledgements and correlated replies. Scripted tool choices do not
establish an external model's judgment, comprehension, or task quality.

The [2026-10-29 run report](compatibility-2026-10-29.md) records the measured source,
results, failed attempts and outstanding checks.

## Reproduce

Use Rust 1.98.0, Bun 1.3.11, Python 3.11+, and `websockets==15.0.1` in your own
environment. The runner does not install tools. Native pins are OpenCode 1.18.35
(V1 SDK/plugin 1.18.35), Claude Code 2.1.300, and the standalone Codex 0.165.0
artifact with its experimental private Unix WebSocket queue API. Similarly named
Codex builds without that API are not qualified by this run. These are measured
fixture pins, not production routing gates. Active delivery uses runtime
[capabilities and protocol validation](codex-delivery.md), with
[newer-runtime evidence](codex-capability-delivery-regression.md) recorded separately.

From a clean checkout of the revision being qualified:

```sh
AGENT_BRIDGE_BUILD_COMMIT=$(git rev-parse HEAD) cargo +1.98.0 build --locked
(cd plugins/opencode && bun install --frozen-lockfile && bun run typecheck && bun test)
python3 tools/test_qualification.py
python3 tools/qualification_native.py target/debug/agent-bridge --scratch-parent /var/tmp --evidence /var/tmp/native-matrix.json
tools/opencode-native.sh "$PWD/target/debug/agent-bridge"
tools/opencode-setup-native.sh "$PWD/target/debug/agent-bridge"
```

Choose a new evidence path for each run. If `/tmp` has a small quota, add
`--scratch-parent /path/to/available/disk` to the Python runner. Its working
directories remain disposable `/tmp` directories so OpenCode cannot search an
owner home directory's ancestors for project configuration. Model endpoints bind
only to loopback; credentials are dummy fixture values. Inherited environment
variables are allowlisted, with private HOME, XDG, Claude and Codex paths. The
runner creates disposable native sessions explicitly; the bridge observer does
not create or resume conversations. Claude's `--bare` mode intentionally skips
owner hooks, so the runner explicitly registers its inbox. Codex is discovered
through a uniquely named socket under the validated private native socket folder;
it does not start, stop or update the owner's managed daemon.

The binary's status `build_commit` must match checkout HEAD. The JSON evidence
records the binary SHA-256, source commit, OS, exact native versions/artifact paths,
checkout status, source/harness hashes, message/session correlations, explicit
ack/reply results and cleanup exits. Modified production source is rejected. Keep
the checkout clean when building; a commit label alone cannot prove an arbitrary
modified executable was built from that source. Preserve the evidence alongside
the command and exit code. Failure logs and disposable databases remain in the
reported private scratch directory for diagnosis; they contain only fixture data.
Remove that exact directory when no longer needed. The runner never removes the
owner's configuration, sessions, credentials, services or tools.

Run all selected source gates independently and retain failures before repairing:

```sh
cargo +1.98.0 fmt --check
cargo +1.98.0 check --locked -p agent-bridge --all-targets
cargo +1.98.0 clippy --locked -p agent-bridge --all-targets -- -D warnings
TMPDIR=/var/tmp cargo +1.98.0 test --locked -p agent-bridge --no-fail-fast
sh -n install.sh
TMPDIR=/var/tmp python3 tools/test_installer.py
TMPDIR=/var/tmp python3 tools/test_packaging.py
python3 tools/test_release.py
```

The existing four-platform PR pipeline also runs vendor relay tests/lints and
plugin tests. Native model fixtures are opt-in and are not silently counted as
CI tests. Installer tests use isolated service-manager/download fixtures; they
do not prove a real systemd or launchd installation.

## Evidence boundaries

| Requirement | Reproducible check | Boundary |
| --- | --- | --- |
| Claude↔OpenCode, Codex↔OpenCode, OpenCode↔OpenCode | `qualification_native.py`, both directions, actual native adapters and real broker MCP | Driver initiates the send from a registered native identity; recipient executes ack/reply and source executes reply ack through native MCP |
| Two brokers, independent session grants | Same runner, two installed OpenCode integrations, real loopback QUIC, private directories before sharing | Same physical host; physical cross-machine/router acceptance remains outstanding |
| Discovery, rename, resume, close, replacement, restart | Same runner plus installed setup harness | Disposable native sessions; native TUI selection paths without events retain the documented V1 limit |
| Busy native tools, permission waits, pause/cancel, idle race | `opencode-native.sh` | Unanswered permission remains unanswered; no native abort/permission-response API calls; idle check is not an atomic reserve |
| Duplicate retry, ambiguous receipt, crash windows, migration | Native reconciliation harness, plugin tests, Rust `opencode*`, `native_delivery`, `discovery`, `claude_discovery`, store/migration tests | Lost-receipt/journal failure injection is protocol coverage; absence is never permission to repeat a prompt |
| Sender label, body and correlation | Actual provider requests and persisted OpenCode user messages | Model-visible content is distinct from interactive rendering; headless execution does not prove TUI appearance or rich cards |
| Installed context, board updates, notices and cleanup | `opencode-setup-native.sh`, loaded installed assets and actual MCP | Verifies one system block/request, compaction/reload, own-write suppression, coalescing, unsubscribe and delete cleanup; no interactive notification-dismissal claim |
| Existing Claude/Codex discovery and typed controls, retained grants/data | Actual Codex discovery and matrix; Rust adapter/discovery/native-control/store/migration suites | Claude live hooks/title discovery and typed pairing/board behavior have protocol coverage, with actual interactive owner flows still outstanding |
| Linux/macOS install/package | Installer/packaging/release tests and current-head four-platform CI | Native macOS clients, actual managed installation and optimized tagged package/signing are outstanding unless separately recorded |

The Codex matrix exercises **unheld-turn workflows**. It does not independently
qualify held active turns or queue saturation. AGB-13's merged adapter fix and
[production regression evidence](codex-active-delivery-regression.md) cover those
separate cases. [Current delivery semantics](codex-delivery.md) distinguish active
steering from idle/compatibility queue delivery; the
[original diagnosis](codex-delivery-diagnosis.md) records the earlier queue-only
behavior. No owner deployment is implied by source qualification.

`submitted` means adapter acceptance/insertion, `acknowledged` requires an explicit
recipient tool call, and neither state alone proves task completion. Native final
fixture output is scripted completion only. Confirmed production failures must be
fixed or linked to a narrow bug before declaring a row qualified. Platform and
interactive checks that were unavailable remain outstanding, not inferred passes.

Setup/restart/removal commands, immutable routing names, bounded FIFO/busy windows,
read-only reconciliation and troubleshooting are in [OpenCode integration](opencode.md),
[recovery](opencode-recovery.md), [installation](install.md) and
[the plugin README](../plugins/opencode/README.md). Do not retry uncertain native
work as a fresh prompt. Inspect exact native history and use broker read-only
reconciliation. Do not run a service installer/uninstaller against the owner's
manager to reproduce a fixture. No release or service replacement is part of
qualification.
