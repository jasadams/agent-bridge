# Codex production delivery regression (AGB-13)

The production regression mode of `tools/codex-native-delivery.py` sends through
the built broker's normal public send API. It preserves the historical AGB-11
artifact verifier and direct-API feasibility scenarios. Run with Python 3.11+,
websockets 15.0.1, an installed Codex runtime and the current task binary. The
historical measurements below used Codex 0.165.0; runtime versions/hashes are
recorded rather than used as routing gates. See the
[capability-driven follow-up](codex-capability-delivery-regression.md) for current evidence:

```sh
python3 tools/codex-native-delivery.py --production --broker target/debug/agent-bridge
python3 tools/codex-native-delivery.py --verify-artifacts /tmp/agb13-codex-PRINTED_SUFFIX
python3 tools/codex-native-delivery.py --verify-negative-tests /tmp/agb13-codex-PRINTED_SUFFIX
```

The six production cases are idle, active generation, held native tool, same-turn
tool-to-model continuation, disconnected observer and capacity rejection. Active
cases leave native metadata untouched. The actual adapter must perform its own
fresh loaded-thread, direct-input capability and latest-turn queries before its
single `turn/steer` write. The offline verifier reconstructs that sequence from
proxy wire frames, checks exact message/native client ID and user input, and
requires the marker in model input before original-turn completion. A disconnected
observer uses final native history with exactly the original completed turn.
Actual MCP function outputs and broker state prove acknowledgement and a reply
correlated to the original message. A successful held command's function output
must survive into the model request containing the delivered marker. Production
commands sleep normally and then print a unique tool sentinel; the native completed
item's exact output must equal that sentinel, and the matching `call_hold` function
output must contain it. The delivery marker is separate from the tool sentinel.

The capacity case is explicitly a **missing-capability compatibility fixture**:
only this disposable thread's proxy removes `canAcceptDirectInput` from native
thread metadata, recording both original and forwarded frames. This exercises
production queue fallback without claiming that stock 0.165.0 lacks direct-input
capability. The native queue itself is unmodified: 100 direct disposable additions
must succeed, the broker's 101st exact ID must receive the real pinned capacity
error and become failed, and the complete native queue/model evidence must exclude
that rejected ID. All accepted entries drain naturally before cleanup.

Eight offline corruption tests remove acceptance/tool completion, interrupt the
original turn, move receipt outside it, change the native client ID, remove direct
capability, duplicate the write or broaden the compatibility transformation. Each
must fail reconstruction.

Each run records source commit, working-diff digest and per-file source hashes
including nonignored untracked files, binary paths/SHA256/version,
Python/library/platform versions, exact synthetic IDs and UTC/monotonic event times.
Only acknowledgement/reply MCP tools are enabled. No inbox read triggers execution.
The isolated allowlisted environment, private homes, loopback Responses fixture,
exact-home discovery fence and owned process-group cleanup follow the historical
[diagnosis](codex-delivery-diagnosis.md). No owner sessions or permissions change.
The fixture proves runtime execution and correlation; it does not establish
external-model judgement or human UI presentation.

The final post-integration production run passed all six cases, exit 0, at
`/home/jason/agb13-native-tmp/agb13-codex-mpgtoo7s` on 2026-10-29 UTC, using base
`a00d963ab1782192b53c290d3779ac7f0906a6f5` with the named-board changes integrated.
Separate offline replay accepted six cases and the corruption suite rejected eight,
both exit 0. Independent review also accepted all six, reran the eight harness
negative cases and rejected eight separately constructed corruptions, exit 0.
The [evidence manifest](diagnostics/codex-active-delivery.json) records
exact IDs/times, versions, source and artifact hashes, and cleanup. Native and
broker exited 0; bounded `/proc` audits found no surviving non-zombie members of
either owned process group, including MCP descendants.

The exact final native-exercised normal-build binary is the immutable copy
`/home/jason/agb13-native-tmp/agb13-merged-build-n0w4mf4t/agent-bridge`, SHA256
`ec2114e380a7a0954d19fce4aa106a3ac5d39d72956be031c364a5b719176cdc`.
All 101 authored inputs in its adjacent compiled-source manifest match the native
run's source file hashes, including the final test fixtures and the unchanged
interpreted harness. The harness SHA256 remains
`b12c9c3cf3482c3151c4e6156f83c3dbc36f084972f4531b32b6731fe7824ae3`.
The full raw wire/model artifacts remain available for offline reconstruction.

The earlier pre-integration run at
`/home/jason/agb13-native-tmp/agb13-codex-iva291bv` also passed six cases with binary
`0737ce98d051de5ad4c6880fa6b98373080111946f57273eee91b8cda172fd22` on base
`e7347b4f258a3a44fc9421157eedf3ab30a329e1`. Its separate replay and corruption
suite passed, and independent review accepted six and rejected eight separately
tampered copies. That prior binary is preserved at
`/home/jason/agb13-native-tmp/agent-bridge-native-exercised-0737ce98`; it is not the
final merged artifact. Prior evidence records the interpreted-harness correction,
subsequent test-only timeout-fixture delta, and distinct Cargo-test feature-graph
binary `78dd5088…`. Those historical qualifications remain in the manifest's
`prior_preintegration_proof`; the new merged run covers the complete final source
inputs with a fresh normal-build binary.

Two preceding attempts are preserved. `/tmp/agb13-codex-6jbclai_` executed all six
cases and cleaned up successfully, then exited 1 because the strict verifier
caught the saturation proxy also transforming the disposable creator's
`thread/start` metadata. The proxy now tracks request IDs and the actual adapter's
initialize identity and transforms only its exact saturation `thread/read`
response. This was a fixture boundary failure.

`/tmp/agb13-codex-r8el2xw1` exited 1 when `/tmp` user quota rejected native history
writes with OS error 122; quota also prevented saving `events.json`/results.
Existing logs, rollout and versions remain, and an independent `/proc` audit
found no owned native/broker/MCP survivors or remaining proxy socket. Remaining
cases were blocked by that environment failure. The successful final run used a
new owner-verified real directory, mode 0700, under `/home/jason` through a
process-scoped `TMPDIR`; the fixed `/tmp` discovery proxy and home fence remained.
No existing evidence was deleted. Neither failed attempt establishes a production
adapter defect.
