# Qualification run: 2026-10-29

The real native fixture matrix passed on Linux x86_64, kernel
7.0.9-204.fc44, glibc 2.43. This qualifies the measured fixture paths, with the
outstanding checks below. It is not a blanket production-readiness verdict.
The [reproduction guide and coverage matrix](compatibility.md) explains the
commands and boundaries. The checked-in [native evidence](diagnostics/native-compatibility.json)
pins the executable, source, harness hashes, exact native artifacts, synthetic
message/session references, observations and cleanup.

Executable source: `a00d963ab1782192b53c290d3779ac7f0906a6f5`.
Binary SHA-256: `da7473d0fdd6f44374123fc4a20a14b118399f12b8c8b59eea1a1858959b9bf8`.
The source commit identifies the built executable; qualification files were
uncommitted when measured and are identified by their recorded hashes. The
production source was clean. Earlier `7cc4cfe`/`e7347b4` inventories were retained as intermediate evidence.
After the named-board merge, the executable was rebuilt at `a00d963` and both the
combined matrix and separate native/setup harnesses were run against that source.

| Check | Observed result |
| --- | --- |
| Claude 2.1.300↔OpenCode 1.18.35 | Both directions passed; actual native MCP ack/reply and reply ack; sender/body/reference visible to local scripted model |
| Standalone Codex 0.165.0↔OpenCode 1.18.35 | Both idle directions passed through actual private native queue and broker MCP; active turns are a separate known AGB-13 defect |
| OpenCode↔OpenCode on one broker | Both directions passed; exact native session and one matching user text part per insertion |
| OpenCode↔OpenCode on two brokers | Both directions passed over real same-host QUIC, with private directories before independent exact-session sharing and correlated remote/local parent IDs |
| Native lifecycle and durability | Rename, broker restart retaining identity/history/grants/pause, plugin close, inactive history lookup, explicit same-session resume, replacement generation, retained native user-message uniqueness and delete unregister passed |
| Busy/permission/recovery native harness | Exit 0; nine scripted provider requests; explicit ack/reply, read-only reconciliation, busy-tool deferral, pause/cancel and unanswered permission wait passed; no cancel/permission-response calls |
| Installed-asset native harness | Exit 0; 18 scripted provider requests; exact system context on 17 tool-bearing requests, actual MCP discovery/ack/reply/board read/update, compaction/reload, notice coalescing/own-write suppression/unsubscribe/delete cleanup passed |
| Rust gates | Formatting, all-target check and Clippy, full package `--no-fail-fast` each exited 0; 390 tests passed, three multicast-LAN scenarios remained intentionally ignored |
| Plugin gates | Frozen install, TypeScript and 52 Bun tests passed |
| Installer/package/release | Shell syntax, 21 installer tests, seven packaging tests and release tests exited 0 |
| Harness safety | Six CI-safe Python tests passed: credential isolation, route identity, startup filesystem failure, exited-leader descendant cleanup, teardown-failure nonpass and HTTP/process cleanup |
| Real published Linux package smoke | Current `install.sh --version v0.2.2 --no-service --bin-dir DISPOSABLE/bin` exited 0 and installed executable reported `agent-bridge 0.2.2`; published artifact source is `3a928340bb735e642c02ad53d4e2e9d0a7230ef0`, separately from current-source native qualification |

The native runner's provider responses were scripted on localhost. Actual
runtime requests established model-visible receipt, and persisted broker state
established explicit MCP acknowledgement/reply. Neither acknowledgement nor a
scripted final response establishes independent model task quality. OpenCode
native user history and model input were checked separately from interactive UI;
no rich-card or TUI-rendering claim is made.

Initial inventories and failed attempts were retained rather than overwritten.
A provenance preflight rejected a binary before its metadata-bearing rebuild
finished (exit 1). The first matrix hit the host `/tmp` user quota and correctly
reported uncertain native insertion (exit 1); subsequent disposable caches used
disk storage. A disk-workspace run was interrupted (exit 130) to keep OpenCode's
ancestor configuration search outside the owner home; candidate owner config
files were absent, and cleanup completed. Two harness assertion errors were
corrected: destination selection must not match a sender reference, and rename
uses `display_name`, not `title`. Another harness assertion compared transient
registration flags as if they were durable grants; it now checks exact
peer/provider/session/permission tuples. These failed runs were not counted as
passing qualification. A later cold native bootstrap exceeded the initial
20-second HTTP timeout before any pair row. Subsequent health probes consumed
their whole readiness budget despite a fresh independent health request returning
HTTP 200. The runner now matches the existing SDK harness: two-second retriable
read-only health probes and a 60-second request bound for native operations,
without retrying timed-out mutations. These failed runs remain recorded.
Review also repaired descendant-process and startup-failure
cleanup, with regression tests. The reported updater throttle test passed in the
initial unchanged full suite; no updater test or implementation was changed.

Faster health probes also exposed a lifecycle sequencing error in the harness:
broker restart disables a route before its old generation lease ends. A fresh
plugin cannot replace that still-live generation merely because discovery omits
the disabled route. The harness now waits for the documented old-lease fence
before its explicit replacement/resume. The unsuccessful early resume was
recorded, not treated as a successful native lifecycle check.
On the merged named-board source, an initial run passed all six local directions
before a harness lookup indexed `listening` during asynchronous network startup.
The runner now waits for the listener list to be published, as the existing Rust
two-broker tests do, rather than treating a transient missing field as readiness.

The first merged-source full Rust inventory exited 101: 383 passed, seven failed
across `relay_brokers`, `relay_config`, `session_sharing` and `sharing_brokers`,
with three ignored LAN cases. Several failures were SQLite I/O errors; a relay
timeout and a downstream not-submitted assertion were also retained without
claiming a proven root cause. Installer fixtures exited 1 with two failures,
including explicit `EDQUOT`; packaging exited 1 after an LLVM linker bus error.
The native run reached all eight rows but its `/tmp` evidence write then failed
with `EDQUOT`, so that run was not counted as passing. All four affected Rust
targets and installer/package checks passed unchanged with `TMPDIR=/var/tmp`.
The final matrix wrote its passing evidence to disk. A subsequent full package run also exited 0 with disk temporary storage. The
initial nonzero results and bounded reruns are included in the manifest, rather
than overwritten.

Native macOS/ARM clients, physical cross-machine/router paths, actual managed
systemd/launchd installation, optimized tagged packaging/signing, interactive
rendering/notification dismissal and external-model judgment remain **outstanding**.
Four-platform CI covers source/plugin/installer/package fixtures, not those native
or physical checks. Existing Claude/Codex title/hook/typed-control and registry
migration regressions passed their protocol/store suites; interactive owner
pairing behavior was not inferred from those tests. AGB-13 tracks Codex active-turn
delay and queue saturation separately; AGB-7 does not fix or qualify that path.

No release, publication, owner service replacement, private configuration,
credentials or owner native-session changes were made. Only disposable broker
identities/session grants were paired/shared. All owned native/broker processes,
HTTP thread and unique Codex socket were stopped/awaited or removed; intentional
SIGTERM/SIGKILL exits are reported as teardown exits rather than native test failures.
