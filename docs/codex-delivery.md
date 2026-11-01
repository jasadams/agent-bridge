# Codex peer input delivery

The adapter delivers only to an already loaded native thread. It verifies the
private socket and same-user peer, canonical `CODEX_HOME`, loaded-thread membership,
and fresh thread metadata. It never starts/resumes a thread, interrupts work,
answers a permission request or changes native permission rules.

Active delivery is selected by verified runtime capabilities and protocol facts,
not a version allowlist. An active thread with
`canAcceptDirectInput: true` receives one `turn/steer` using its latest exact
`inProgress` turn ID as `expectedTurnId`. The lookup is bounded to one descending
summary turn. Input retains the original bridge UUID and peer envelope; steer
returns only a turn ID. Validated acceptance is Delivered, not model read, task completion
or proof of model judgement. A running tool may finish before the next model
request sees the input; the adapter does not cancel it to reduce latency.

Idle threads and false/missing direct-input capabilities use the native queue.
A newer or unrecognized runtime that reports the same valid contract uses the
same active path. The `initialize.userAgent` version is diagnostic only: missing,
unusual or changed version text neither enables nor blocks a native write. The
private same-user peer, canonical home, loaded destination, status/capability,
latest active turn and exact receipt remain mandatory. A version string alone
never establishes compatibility; changed or unsupported protocol facts still
fail safely. Exercised 0.165 and 0.166 fixtures are regression examples,
not an allowlist.

Contradictory/malformed metadata, unavailable threads, unsupported required lookup
APIs and a stale active-turn selection fail before submission. If the active turn
changes after the lookup, the steer may be rejected; no queue fallback or second
write occurs. A lost/mismatched/ambiguous steer receipt remains Unknown. Lookup
timeouts before a write are Failed; timeouts after a write attempt remain Unknown.
The bridge never automatically retries an ambiguous write or sends the same UUID
through both native methods. The idle read and queue write are not atomic with a user starting a turn. That
one queued handoff can still wait for the new turn to finish; it is never resent
through steer. Tools and permission waits also retain their native safe boundaries.
Native error text is not persisted because it can echo
private message content.

## Backpressure and recovery

Each canonical Codex native session has at most **64 pending broker deliveries**
across all aliases. Delivered history does not use admission slots, whether or not
the model ever processes it. No recipient receipt tool call is required. Unknown
history is not pending queue work and is never automatically replayed. Typed
board/pairing notices use the same delivery rule. Retained history and native queue
capacity still have independent resource limits; delivery is not history deletion.

The existing global bound remains 1000 queued messages and dispatcher concurrency
remains bounded, with at most one active delivery worker per canonical Codex
session. Its runtime fence lasts until the delivery worker finishes, even if
another operation observes the durable outcome first.
Local sends, explicit retries, inbound remote messages and typed controls share
admission. Idempotent sends return the existing record before new admission checks.
Captured provider/native identity is immutable across alias replacement; a rebound
alias cannot redirect an old attempt to another provider, even if the UUID matches.

The exercised native queue holds 100 entries and can include messages sent
outside this broker. The exact validated capacity rejection produces a failed
message with a sanitized queue-full diagnostic, distinct from an Unknown write.
Only that exact code/message and an unambiguous error-only response qualify;
generic errors, malformed error envelopes and contradictory receipts remain Unknown.
Capacity classification uses the correlated error-only response and exact known
code/message semantics, independently of runtime version.
There is no automatic retry when native capacity becomes available. A result
describes only the latest attempt: an earlier explicitly retried Unknown or
Delivered attempt may still execute. A definite latest rejection does not prove
historical absence or make blind retries safe.

When admission is full, inspect exact message IDs using `agent-bridge history`
and `agent-bridge get ID`. Only unattempted queued work can be cancelled (public
Failed with a cancellation reason). Policy blocks remain Pending with an explicit
retry reason, not permanent failure. Failed definite
rejections can be explicitly retried once capacity is available. For Unknown or
Delivered work, inspect both native conversation/queue and broker state before any
explicit duplicate-authorized retry; an old native submission can still execute.
Do not use repeated inbox polling as delivery or recovery automation.

Historical pre-provenance Codex queue rows with unknown captured provider are
blocked with a diagnostic rather than being silently rebound/submitted. Inspect
and resolve historical native and broker state before deciding whether a new
message is appropriate; a new UUID can duplicate an old native queued attempt.
Schema 14 adds only a typed-control marker for the existing message queue. Older
binaries reject the newer schema; use a matched database backup/binary for rollback.

This code does not migrate, delete, cancel or convert the native historical queue,
and never replays old uncertain records on upgrade. Existing native backlog may
still drain when its original turn finishes. Deployment/restarting the owner broker
is a separate action; isolated validation does not change the running installation.

## Evidence and regression coverage

The [AGB-11 diagnosis](codex-delivery-diagnosis.md) records the original queue-only
boundary, historical uncertainty and disposable native feasibility evidence.
The [production native regression report](codex-active-delivery-regression.md)
records historical executable evidence; the [capability-driven follow-up](codex-capability-delivery-regression.md)
records the current 0.166 runtime and version-independent contract tests.
`tools/codex-native-delivery.py --production` exercises the built adapter through
actual broker sends and correlated MCP replies against a deterministic localhost
Responses fixture. It also verifies held-tool success, same-turn continuation,
observer disconnection and exact native capacity rejection. Synthetic model
fixtures prove runtime/API correlation, not external-model understanding or UI
qualification. Run `--help` for isolated prerequisites and artifact verification.
