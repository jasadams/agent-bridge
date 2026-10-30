# Capability-driven Codex delivery regression

Active routing follows the [native delivery contract](codex-delivery.md). Runtime
version strings are diagnostic metadata, not an allowlist. Explicit true direct
input capability, an exact loaded thread in the verified home, and the latest
valid active-turn precondition are required before the adapter selects one steer
write. Absent/false capability uses the queue; malformed or unsupported facts
fail safely. Post-write ambiguity never authorizes another write or method.

The [official App Server contract](https://learn.chatgpt.com/docs/app-server#steer-an-active-turn)
describes steering within the current turn using `expectedTurnId` and a matching
`turnId` response. Generated schemas from installed 0.165.0 and 0.166.0 have the
same used request properties and response. Their steer schema difference is an
unused optional `AdditionalContextEntry.name` field. Schema comparison supports
the protocol analysis; it does not replace runtime qualification.

## Exercised source and result

Between 2026-10-29 23:59:50 and 2026-10-30 00:00:07 UTC, all six production
cases passed against committed source
`dd925b57a1a2cef2ff98594298e35be3af3c2782`, built on main
`b682ddcbd26fddc8990e1fedde9e2d089987a6c1`. The executable SHA-256 was
`9cce1468869f88131502bd27fe2c7d8777bb6eabd607b7eadd0076ab26658523`;
installed `codex-cli 0.166.0` was
`b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8`.
The broker's status independently matched the source commit. The build reports
the repository's development version `0.2.0`; it is not the running owner install.

The [evidence manifest](diagnostics/codex-capability-delivery.json) records exact
source-file hashes, binary hashes, message/thread IDs, timestamps and raw-artifact
hashes. Raw evidence remains at `/var/tmp/agb13-codex-1p8zp7cp`; the legacy directory
prefix does not change its measured source. No native cases failed in this run.
Offline replay accepted all six cases, and all eight independent corruptions were
rejected. Both owned native/broker leaders exited 0, with no cleanup errors or
surviving non-zombie group members.

| Case | Observed result |
| --- | --- |
| Idle | One native queue write; input consumed, acknowledged and replied to |
| Generating | One steer; input consumed before the original turn completed |
| Held tool | Steer accepted while command ran; command completed successfully, then input was consumed in the same turn |
| Long turn | One steer at a later safe boundary; original turn completed successfully |
| Disconnected observer | Production delivery and MCP correlation worked after observer reconnect |
| Saturation | 100 entries accepted; one further queue write definitely rejected, then the disposable queue drained |

The integrated local gates on base `44d0c4437a297944acea3f98e5059c04326dc237`
passed formatting, all-target check, strict Clippy, 479 Rust tests (three intentional
LAN ignores) and 12 Python safety tests. Main's subsequent code-copy integration
changed board UI files; the native proof used the rebased commit above. Current-head
platform CI is recorded with the PR, separately from these local measurements.
Later evidence/report-only commits do not relabel this native run: compiled inputs
and the harness must retain the manifest hashes, while report hashes describe the
measurement-time text.

## Reproduce the native proof

Use Linux, Python 3.11+ and already installed `websockets==15.0.1`. No tools,
credentials or owner configuration are installed or copied. Build a reviewed,
committed source tree with accurate metadata, then run the installed runtime:

```sh
AGENT_BRIDGE_BUILD_COMMIT=$(git rev-parse HEAD) cargo +1.98.0 build --locked
TMPDIR=/var/tmp python3 tools/codex-native-delivery.py --production \
  --broker "$PWD/target/debug/agent-bridge" --codex /absolute/path/to/codex
python3 tools/codex-native-delivery.py --verify-artifacts ARTIFACT_ROOT
python3 tools/codex-native-delivery.py --verify-negative-tests ARTIFACT_ROOT
```

The six cases cover idle delivery, active generation, held tool execution,
same-turn continuation, disconnected observation and actual queue saturation.
The capacity case removes only the adapter's direct-input capability metadata to
exercise the queue contract; this controlled projection is recorded separately.
The adapter's own reads and single write are reconstructed from actual proxy wire
frames, native history and model inputs. The fixture never interrupts, resumes,
cancels or starts work on behalf of the adapter. Only the disposable driver
creates its test threads and initial turns. MCP acknowledgement and correlated
reply are verified separately from native adapter acceptance.

The runner records the resolved runtime/broker paths, versions, SHA-256 hashes,
source commit/file hashes, exact IDs and timing. Production runs reject dirty
executable source and a broker built from another commit. Run state is private,
uses disk temporary storage, and is retained for independent offline checks.
Shared process cleanup preserves owned group IDs until final signalling, attempts
all teardown stages and treats any cleanup failure as nonpassing. It removes only
its own unique discovery socket, with identity checks.

## Initial regression and limits

Before the fix, the added 0.166 contract test exited 101: the actual selected
method was `thread/queue/add`, while active/capable delivery required `turn/steer`.
The retained `red-version-test.log` and the ticket's original submitted-message
observations preserve this failure. The initial version-allowlist plan was
superseded by the user's capability-driven requirement before production changes.
Regression examples now cover 0.165, 0.166, a synthetic future version, missing or
unusual version headers, and malformed protocol/error boundaries.

Historical [0.165 runtime evidence](codex-active-delivery-regression.md) remains
historical. New native measurements must retain their own source/runtime hashes;
an old result is never relabelled as a new-source pass. A scripted localhost
provider proves transport, safe-boundary consumption and tool correlation, not
external-model judgment. Native macOS, physical-network and interactive UI
qualification are outside this Linux fixture. Source qualification does not deploy
or replace the running owner broker, configure owner sessions, or publish a release.

## Initial CI portability repair

The initial PR head `936e968fe9345126575941926e69cf5d5373b449` completed
[run 40977086142](https://github.com/jasadams/agent-bridge/actions/runs/40977086142).
Both macOS jobs passed. Both Linux jobs failed only the new readiness-observation
unit test: their Python 3.10 has no `asyncio.timeout`. The preceding Rust, vendor,
Clippy and other Python checks passed. Both failed job logs are retained as
`/var/tmp/agb20-linux-arm-ci.log` and `/var/tmp/agb20-linux-x86-ci.log`.

The readiness helper now uses portable `asyncio.wait_for` around the unchanged
nonreaping observation loop. The production deadline remains 15 seconds; timeout
and immediate-child-exit tests both assert no `poll()` or `wait()` reaping. The
native fixture's other APIs still require Python 3.11+. No platform is skipped.
The earlier native measurement above retains its exact source and harness hashes;
refreshed measurements after this harness repair are attributed separately.
