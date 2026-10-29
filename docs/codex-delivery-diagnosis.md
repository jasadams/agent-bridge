# Codex native delivery diagnosis (AGB-11)

This is historical queue-only evidence. The AGB-13 active delivery implementation
is described in [Codex delivery](codex-delivery.md). Replay the original default
queue-only harness on recorded commit `5cb93aa7b932c8bdeb84cc5c4b9c0257b327574c`;
on the fixed current source use the harness `--production` mode. The evidence
below remains unchanged and does not describe the fixed adapter's expected active
behavior.

## Finding

On Codex CLI **0.165.0**, `thread/queue/add` accepts a message into a native
per-thread queue; it does **not** steer the current turn. In the disposable
reproducer, queued input remained outside the conversation and model request
until the entire current turn completed. Completing a native shell tool and
continuing the same turn did not consume the queue. An idle thread consumed the
queue automatically, without an inbox read, then actually acknowledged and
replied through Agent Bridge MCP.

The native queue holds **100 submissions**. While an active turn was held, 100
unique additions succeeded; addition 101 returned JSON-RPC error `-32600`,
`queue cannot contain more than 100 submissions`. The rejected ID was absent
from the complete queue snapshot and every fixture model request. The current
adapter conservatively records this explicit rejection as `uncertain` because
it treats errors after the write as potentially ambiguous. Closing its short
queue connection did not prevent accepted messages from executing later.

These observations explain a concrete failure mode for a coordinator that stays
inside one long turn: handoffs accumulate rather than entering that turn, then
later submissions can be rejected when the native queue fills. They do not prove
the exact server response to historical messages for which no wire log exists.
No production transport behavior changes in this diagnostic.

## Historical evidence and limits

The running broker reported version 0.2.2, build
`3a928340bb735e642c02ad53d4e2e9d0a7230ef0`. Its old `codex_queue.rs` and the current
`src/clients/codex.rs` have the same handshake, queue request, response matching,
limits and post-write outcome handling; the intervening refactor changed module
paths and extracted UUID validation. Reported build metadata is not independent
proof of deployed binary provenance.

The coordinator filtered only task-relevant durable state and transcript records:

| Message ID | Created UTC, 2026-10-29 | Native peer user record | State before manual acknowledgement |
| --- | --- | --- | --- |
| `c5e5215e-1f53-4eed-afd3-0a3b6dad6d9f` | 03:55:03.902 | not observed by approximately 04:13 UTC | uncertain |
| `528b43e6-7e9b-43cd-a6b4-4ecc3f3d1a4a` | 04:03:02.536 | not observed by that cutoff | uncertain |
| `a55b855a-e5cf-4b4a-a0ec-7e03bcabd4e2` | 03:55:48.126 | not observed by that cutoff | uncertain |
| `ea916bc2-6742-4905-8144-920c96abde8f` | 01:55:25.250 | 03:55:23.441, after recipient's long turn ended | subsequently acknowledged |

The last example's insertion delay was approximately 119 minutes 58 seconds.
For the first three, mentions in history/tool output and later manual
acknowledgements are not native receipt evidence. A single read-only queue audit
found exactly 100 coordinator submissions with `nextCursor: null`, none matching
those three IDs; this snapshot supports the saturation hypothesis but cannot
recover their missing historical native responses. No repeat inbox polling was
used for this diagnosis or as a workaround.

## Reproduce safely

For the historical queue-only replay, use a checkout of base
`5cb93aa7b932c8bdeb84cc5c4b9c0257b327574c`. The evidence manifest records the exact binaries, hashes, OS, Python,
websockets version, synthetic session/message IDs and UTC boundaries:
[recorded native evidence](diagnostics/codex-native-delivery.json).

```sh
cargo +1.98.0 build --locked -p agent-bridge
python3 -m venv /tmp/agb11-venv
/tmp/agb11-venv/bin/python -m pip install websockets==15.0.1
/tmp/agb11-venv/bin/python tools/codex-native-delivery.py \
  --broker target/debug/agent-bridge --codex /path/to/codex
# Recheck the printed ARTIFACT_ROOT without starting native processes:
/tmp/agb11-venv/bin/python tools/codex-native-delivery.py \
  --verify-artifacts /tmp/agb11-codex-PRINTED_SUFFIX
```

The harness requires Linux `/proc`, Python 3.11+, and the existing private
`/tmp/codex-daemon-UID` directory. It creates a unique private temporary directory,
allowlists the child environment, uses separate `HOME`, `CODEX_HOME`, broker state
and cwd, and configures a loopback Responses fixture requiring no authentication.
The native daemon socket lives inside that temporary directory. A unique private
same-user websocket proxy in the native discovery directory records exact wire
frames so the unchanged adapter can find it. The broker runs `--no-discovery`.
An unrelated observer encountering the proxy must fail the existing canonical
`codexHome` comparison **before** `initialized`, loaded-thread enumeration or
registration. `TMPDIR` alone cannot isolate discovery: the adapter's directory
is a literal `/tmp/codex-daemon-UID`.

Only disposable threads are started. No owner session is started/resumed, no
native permissions are changed through the API, and no approval request is
answered. Initial disposable MCP configuration enables only acknowledgement and
reply tools. The native tool fixture runs only `sleep 4` or `sleep 1`, to completion.
No interrupt/cancel/queue-start/queue-delete calls are issued. Owned process groups,
including MCP descendants, are terminated during bounded cleanup; the saturated
queue is drained naturally before cleanup. Logs contain synthetic fixture content.
The model endpoint is local; this does not claim native startup makes no unrelated
background network attempts.

## Controlled cases

| Case | Observed boundary and assertion |
| --- | --- |
| Idle | Broker send → exact echoed queue UUID/input → native user insertion → fixture request → actual MCP acknowledgement and parent-correlated reply. |
| Generating | Hold the active model HTTP request. Exact message stays queued, absent from `thread/read` and model input; arrives only after original exact turn completes. |
| Held native tool | Queue during a real native `exec_command` sleep. Tool completes normally; queue waits for original turn completion, then actual MCP ack/reply. |
| Long turn | Native sleep finishes, then hold the continuing model request in the same turn. Queue still does not enter at the tool boundary; only full turn completion drains it. This is a bounded continuation fixture, not a two-hour soak. |
| Disconnected/reconnected | Close the creator's observer while its turn is active. Fresh initialized metadata-only connection reads the loaded thread/queue without resume; after release delivery still runs both actual MCP tools. A fresh metadata connection does not receive turn subscription events. |
| Saturation | 100 individually matched native successes; broker's 101st ID receives the exact capacity error, becomes uncertain, and is absent from the complete snapshot/model. All accepted entries then drain. |
| Active steer feasibility | Separate disposable scenario: fresh initialize → read direct-input capability → bounded latest-turn lookup → `turn/steer` with exact active ID → close connection. Input reaches the fixture in the same original turn, which completes normally. No queue write for that ID. |
| Held-tool steer feasibility | Same fresh connection path while native sleep is running. Original command item completes with exit code 0; next model input includes the matching `call_hold` function output and steered text. No work is cancelled. |

`events.json` records UTC and monotonic timestamps at broker RPC, proxy request and
response, native notification, queue snapshot and fixture request boundaries.
`results.json` records exact ack/reply evidence and child exits. `summary.json` is
reconstructed by the offline verifier, including original exact-turn completion
before queued model receipt. Queue diagnostics and exact-ID broker status reads
observe the experiment; they do not trigger execution. No inbox tools are enabled.

The fixture produces deterministic Responses tool calls. This proves native
runtime delivery and actual MCP correlation, not external-model comprehension,
judgement, human UI presentation, or a live paid provider. In the held-tool steer
case, receipt at the model waits until that tool returns; steer is not a promise
of zero-delay execution or interruption.

## Smallest supported proposal (not implemented in this diagnostic)

Keep idle delivery on the verified native queue path. Add an explicitly reviewed
active-handoff path for existing loaded sessions whose fresh metadata says
`canAcceptDirectInput: true`. On the same already-home-verified connection, read
the latest turn with bounded `thread/turns/list` (`limit: 1`, descending, summary)
and require `inProgress`; send **one** `turn/steer` with its `expectedTurnId`, the
original bridge UUID as `clientUserMessageId`, and the unchanged peer envelope.
This is supported by the installed generated protocol and the disposable tests,
without resume, a persistent UI connection, new turns, or permission changes.

The race between metadata lookup and write must fail closed. Do not queue and
steer the same message, automatically replay an uncertain write, or silently
fall back after a lost/rejected/mismatched steer receipt. A changed active turn
must produce an explicit safe outcome; any proposed retry/fallback requires a
separate no-insertion proof and review. Missing/false capability and unsupported
native versions retain queue-only behavior with an honest delayed-delivery
explanation. Native acceptance remains distinct from recipient acknowledgement.

Also distinguish the pinned native capacity rejection from a transport-ambiguous
write, with an explicit diagnostic/backpressure outcome and bounded broker-side
admission policy. Do not infer success from an error or blindly retry historical
uncertain messages. Do not classify every `-32600` response as the capacity error.
Status should explain that accepted queued input can wait for the whole active
turn and report definite queue rejection separately from receipt uncertainty.
These are proposals, not changes to current durable states or retry semantics.

The installed experimental schema was generated locally with:

```sh
CODEX_HOME=DISPOSABLE_HOME HOME=DISPOSABLE_HOME codex app-server \
  generate-ts --experimental --out DISPOSABLE_SCHEMA_DIR
```

Relevant generated types: `ThreadQueueAddParams`, `QueuedSubmission`,
`ThreadQueueListParams/Response`, `Thread.canAcceptDirectInput`,
`ThreadTurnsListParams/Response`, and `TurnSteerParams` (required `expectedTurnId`,
optional `clientUserMessageId`). General official descriptions distinguish Queue
and Steer, but do not substitute for this installed private-daemon evidence:
[app-server](https://learn.chatgpt.com/docs/app-server) and
[Codex Remote](https://developers.openai.com/blog/mastering-codex-remote-for-engineering).

## Regression plan for a follow-up transport change

- Preserve exact-home/peer-UID/loaded-session checks, no start/resume, existing
  permissions and unchanged envelopes. Exercise true, false, missing and stale
  direct-input capabilities; active-to-idle and active-ID replacement races.
- Prove exactly one write path per UUID: idle queue or active steer. Verify exact
  native response ID and turn ID; prove UUID/input correlation from the outgoing
  request and native user item/model input (steer returns only a turn ID). Lost replies and
  mismatches remain uncertain without retry or queue fallback.
- Test queue capacity and explicit rejection separately from timeout, close,
  frame overflow and notification-budget exhaustion. Verify broker capacity is
  bounded and aliases cannot bypass it; historical uncertain attempts never replay.
- Retain real private websocket fixtures for the first-eight socket selection
  bound, multiple homes, stale sockets, loaded-list pagination, 128 incoming-frame
  budget, 1 MiB frame bound and 20-second overall deadline. No saturation or
  high-notification experiment on owner sessions.
- Reject server requests/approval frames before interpreting a matching numeric
  RPC ID as a response; a colliding server request must not masquerade as a queue
  or steer receipt. This is a secondary robustness finding, not the demonstrated
  historical cause. Never answer permissions as part of transport.
- Reuse this native harness for idle, held sampling, real tool completion,
  same-turn continuation, observer disconnection and safe active steer. Add an
  independent UI check for any new supported native version.

## Validation and failure inventory

Rust production sources are unchanged. The selected Rust validation was the
current worktree `cargo +1.98.0 build --locked -p agent-bridge`: exit 0. Harness
Python compilation, help, offline evidence verification and the isolated native
runs are recorded with actual exits in the evidence manifest. No unrelated Rust
suite or updater changes were made.

Two initial harness runs failed and are retained in `/tmp`:

1. `/tmp/agb11-codex-rqi45py6`, exit 1: idle/generation passed, and disconnected
   delivery reached both MCP tools, but the harness incorrectly waited for a
   subscribed `turn/completed` event on a fresh metadata-only connection. It now
   waits for fixture continuation and verifies actual broker ack/reply instead.
2. `/tmp/agb11-codex-mybjllb1`, exit 1: all five delivery cases passed; the
   saturation assertion assumed the queue list's default page held 100 entries.
   The default is 25 with a cursor. Explicit `limit: 100` now verifies the whole
   snapshot. The exact native 101st-add rejection was already captured in this run.

Subsequent saturation/steer and full native runs passed without weakening receipt,
no-approval, exact-ID or original-work-completion assertions. Production active
steer/backpressure implementation and owner UI verification remain follow-up work,
subject to independent review. Historical capacity errors cannot be recovered
from the available snapshots, so that part of the incident explanation remains
an evidence-supported inference rather than a per-message proven response.
