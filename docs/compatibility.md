# Native compatibility qualification

Qualification combines real client execution with a deterministic loopback model
fixture. It verifies transport and actual native MCP tool execution, including
confirmed native acceptance and correlated replies without receipt tool calls.
Claude socket writes remain Unknown; separate fixture observations can demonstrate
model input without changing the adapter's proof boundary. Scripted tool choices do not
establish an external model's judgment, comprehension, or task quality.

The [2026-10-29 run report](compatibility-2026-10-29.md) records the measured source,
results, failed attempts and outstanding checks.

## Reproduce

Current standalone OpenCode qualification uses **OpenCode 2.2.5**,
`@opencode/plugin` **2.2.5**, Bun **1.3.11** and the locally built broker. It uses
the V2 plugin and authenticated public API on an explicitly launched isolated
server. The existing three-provider driver is now ported in place to V2; its
[current measured matrix and exact provenance](opencode-v2-validation.md#retained-native-matrix)
record six local and two loopback-QUIC round trips. The older 1.18.35 report remains
historical evidence at its recorded commit. Standalone/setup and full matrix runs
are separate checks, not interchangeable claims.
Rust isolated fixtures continue covering all three adapters. Separate Codex
[capability/native evidence](codex-capability-delivery-regression.md) remains
bounded by its recorded runtime and source.

From a clean checkout of the revision being qualified:

```sh
AGENT_BRIDGE_BUILD_COMMIT=$(git rev-parse HEAD) cargo +1.98.0 build --locked
(cd plugins/opencode && bun install --frozen-lockfile && bun run typecheck && bun test)
tools/opencode-native.sh "$PWD/target/debug/agent-bridge"
tools/opencode-setup-native.sh "$PWD/target/debug/agent-bridge"
python3 tools/qualification_native.py "$PWD/target/debug/agent-bridge" --scratch-parent /tmp/opencode --evidence /tmp/opencode/native-matrix.json
```

Set `AGENT_BRIDGE_NATIVE_LOG_DIR` to retain successful disposable run evidence;
failures retain their private root automatically. Both runners use private
HOME/XDG/config/workspace/broker/native sessions under `/tmp/opencode`, a loopback
scripted provider and generated Basic authentication. They never discover the
owner's service or inherit owner credentials. Fixture diagnostics are not
recipient acknowledgement/status-poll steps.

Record exact command/exit, binary SHA-256, source HEAD plus dirty patch/hash,
plugin/module hashes and actual broker status. A null `build_commit` stays null;
it cannot prove a dirty binary's source. The installed setup compares extracted
module hashes with current source. It retains original/effective configuration
and explicit initial fixture normalization to native `mcp.servers` and direct
tools, preserving the exact generated command/package. This does not certify that
the installer itself emits native V2 MCP shape. Preserve failed attempts and do
not borrow a later binary hash for an earlier run.

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

The existing PR build matrix also runs vendor relay tests/lints and
plugin tests. Native model fixtures are opt-in and are not silently counted as
CI tests. Installer tests use isolated service-manager/download fixtures; they
do not prove a real systemd or launchd installation.

## Evidence boundaries

| Requirement | Reproducible check | Boundary |
| --- | --- | --- |
| Three-provider native matrix | In-place V2 driver; [current 8-pair evidence](opencode-v2-validation.md#retained-native-matrix); historical report separately | Scripted actual-runtime wiring, not external-model judgment; standalone V2 harnesses alone do not establish the matrix |
| Two brokers, independent session grants | Two current native loopback-QUIC pairs plus Rust isolated remote-adapter/permission fixtures | Same host, not physical cross-machine/router native acceptance |
| Discovery, resume, close, replacement, restart | V2 installed setup harness plus focused plugin/Rust fixtures | Disposable native sessions; no claim to follow every interactive selected TUI tab |
| Busy native tools, permission waits, pause/cancel, idle race | `opencode-native.sh` | Unanswered permission remains unanswered; no native abort/permission-response API calls; idle check is not an atomic reserve |
| Duplicate retry, ambiguous receipt, crash windows, migration | Native reconciliation harness, plugin tests, Rust `opencode*`, `native_delivery`, `discovery`, `claude_discovery`, store/migration tests | Lost-receipt/journal failure injection is protocol coverage; absence is never permission to repeat a prompt |
| Sender label, body and correlation | Actual provider requests and persisted OpenCode user messages | Model-visible content is distinct from interactive rendering; headless execution does not prove TUI appearance or rich cards |
| Installed context, board updates, notices and cleanup | `opencode-setup-native.sh`, loaded installed assets and actual MCP | Verifies one system block/request, compaction/reload, own-write suppression, coalescing, unsubscribe and delete cleanup; no interactive notification-dismissal claim |
| Existing Claude/Codex discovery and typed controls, retained grants/data | Actual Codex discovery and matrix; Rust adapter/discovery/native-control/store/migration suites | Claude live hooks/title discovery and typed pairing/board behavior have protocol coverage, with actual interactive owner flows still outstanding |
| Linux/macOS install/package | Installer/packaging/release tests and current-head PR build matrix | Native macOS clients, actual managed installation and optimized tagged package/signing are outstanding unless separately recorded |

The Codex matrix exercises **unheld-turn workflows**. It does not independently
qualify held active turns or queue saturation. AGB-13's merged adapter fix and
[production regression evidence](codex-active-delivery-regression.md) cover those
separate cases. [Current delivery semantics](codex-delivery.md) distinguish active
steering from idle/compatibility queue delivery; the
[original diagnosis](codex-delivery-diagnosis.md) records the earlier queue-only
behavior. No owner deployment is implied by source qualification.

Current outcomes are `pending`, `delivered`, `failed` and `unknown`; historical
reports below retain the older labels and tool calls as evidence, not current
requirements. Delivered means validated native acceptance, never model read or
task completion. Claude socket-only writes remain Unknown. Native final
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
