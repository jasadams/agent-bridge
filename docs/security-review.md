# Three-angle security review

Three separate agents reviewed the complete feature implementation from different
perspectives. They initially worked independently; material findings were shared
for remediation and fresh review. The coordinator integrated fixes and verification.
These are internal agent-assisted reviews, not three external audits or a guarantee
that no vulnerability remains.

| Review | Focus | Result |
| --- | --- | --- |
| [Protocol and trust](security-review-protocol.md) | Remote identity, pairing/SAS, replay/revocation, encrypted transport, discovery/operators and NAT | No confirmed exploitable finding in scope; 70 independent regressions passed. |
| [Browser and local OS](security-review-browser.md) | HTTP/auth/XSS/origins, browser lifecycle, private IPC and local plaintext disclosure | Two findings; fixes freshly approved. Eleven negative HTTP cases and nine Chromium/P2P acceptance checks passed. |
| [Persistence and delivery](security-review-state.md) | Grants/session pinning, durable history, quotas, transactional recovery, native delivery | Retained-history finding plus independent confirmation of local delivery boundaries; all three fixes approved. Forty-four independent targeted tests passed. |

## Findings and remediation

### SR-01 — Native Codex process-argument disclosure: medium

The adapter previously passed the complete message envelope to `codex queue
--message`. An OS with cross-user process-list visibility could expose plaintext
to another local user, even though network transport and state were private. The
reviewer reproduced the actual adapter path using a disposable fake receiver and
confirmed the canary in process arguments. No production session was used.

**Fixed:** delivery now uses the already running daemon's authenticated same-UID
private Unix WebSocket. It verifies the expected Codex home and exact loaded
session UUID before sending text. It queues with the message UUID as
`clientUserMessageId`, requires confirmation of that UUID and exact input, bounds
frames/pages/time, and does not start or resume a thread. There is no child-process launch,
argument/environment payload or insecure CLI fallback. Loss/rejection after a
possible write remains `uncertain` and is never automatically resent to another
daemon. Native errors that might echo text are not stored or logged.

This adapter uses the installed Codex protocol's `thread/queue/add` method. Its
schema was generated locally with `codex app-server generate-json-schema
--experimental`; the transport/initialization boundary was checked against the
[official app-server documentation](https://learn.chatgpt.com/docs/app-server).
The [documented CLI queue command](https://learn.chatgpt.com/docs/developer-commands#codex-queue)
accepts message text as an argument; no stdin behavior is assumed. Queue-RPC
compatibility is version-specific and unsupported versions fail closed. Actual
native Codex/macOS acceptance of this replacement remains required before release.
The legacy `--codex-bin` option is retained for existing service configuration but
is no longer executed to deliver message bodies.

An operator who chooses `agent-bridge send --message TEXT` can still expose their
own text in that command's arguments. For sensitive content, pipe stdin into
`agent-bridge send --to NAME` instead. This fix removes the unavoidable downstream
adapter exposure, not the OS visibility of arguments chosen by the user.

### SR-02 — Claude pathname/connected-peer race: low

Path validation checked socket ownership and its immediate private directory,
but not the connected endpoint's UID. A different local user able to rename a
writable ancestor of a custom socket location could substitute a listener between
validation and connection. Default private-home/sticky-`/tmp` placement mitigates
this precondition. This is a source-confirmed missing check; no cross-user exploit
was executed.

**Fixed:** the connected Unix socket's peer credentials must match the bridge's
UID before any bytes are written. A unit regression checks valid and mismatched
UIDs and confirms no data was written. The same connected-peer check also protects
the new native Codex queue transport.

### SR-03 — Unbounded completed inbound history: medium

Receive permission previously bounded rates and pending work, but completed
messages retained both body and serialized envelope indefinitely. A compromised
paired machine with a Receive grant could continue consuming disk after freeing
queue slots, within the allowed rate. This is a source-confirmed availability
issue; the fix is covered by bounded unit/database tests rather than a disk-fill
experiment.

**Fixed:** retained inbound history is limited to **64 MiB and 8,192 messages per
peer**, and **256 MiB and 32,768 messages globally**. Bytes include both the UTF-8
body and serialized envelope. Row limits additionally bound per-message metadata;
these are admission budgets, not exact SQLite file-size quotas. Locally authored
history remains under the local owner's control and is not charged here.

Schema 10 transactionally backfills usage and maintains counters with insertion/
deletion triggers. Admission reads the counters inside the same transaction as
message persistence. Completed/submitted messages still consume their quota.
Exact retries of an already accepted message remain idempotent at the limit;
over-budget new messages write nothing. No history, receipts or deduplication IDs
are automatically deleted, and no grants are changed. Both byte and row boundaries,
UTF-8/embedded-NUL accounting, migration and retry behavior have regression coverage.
The final two quota regressions also passed separately.

At capacity, new inbound transfers are refused with an explicit quota error.
Owners should disable unnecessary receiving grants and preserve/back up state
while arranging supported retention/archival maintenance. This change adds no
history-compaction command. Do not delete deduplication records or reset a peer's
identity merely to recover space: doing so can remove replay protection. State
already above a quota upgrades without deletion and refuses further new inbound
messages. Back up the full state directory before the schema upgrade; follow
[rollback guidance](peer-trust.md#upgrade-and-rollback).

## Dependency advisory check

`cargo audit --json` 0.22.1 fetched the RustSec database and checked all **430 locked
dependencies**: **zero known vulnerability advisories**. Database commit:
`99670cfc0bc574433034b75876a4b791ccaa2efb`; reported update time
`2026-10-27T11:02:42+01:00`.

One informational maintenance warning remains:
[RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html), archived
`paste` 1.0.15. It is a transitive build-time procedural macro through
`netlink-packet-core` → `if-watch` → libp2p on Linux. The advisory does not identify
an exploitable vulnerability. It was recorded, not silently ignored or replaced
with an unreviewed fork. Advisory checks do not prove dependencies vulnerability-free
or audit the project's vendored source changes; those were included in source review.

## Final regression checks

The complete application run passed **184 tests**, zero failures, with three
multicast-sensitive cases ignored. All-target strict Clippy, formatting and diff
checks passed. New regressions cover private queue IDs/loaded sessions/home/lost
confirmation, connected UID checks, retained byte/row boundaries, UTF-8 and
embedded-NUL accounting, migration and idempotent retries. The final two quota
regressions passed separately after adding the embedded-NUL case.

The remediated executable also passed all nine Chromium/live-monitor/P2P
acceptance checks. Vendored source was unchanged during security remediation;
its patches were included in source review. Machine-specific artifacts and logs
are not published.

## Local browser board-response boundary

The local monitor now includes a narrow authenticated write route for existing
board checkbox/text response controls. Message endpoints remain read-only; there
are no browser messaging, pairing, grant, general Markdown-edit or filesystem-path
operations. This extends the historical GET-only monitor surface described by the
earlier acceptance work above.

Response writes use the shared Rust control parser, exact board/revision identity,
private filesystem operations and broker transaction/notification logic. Monitor
access generation is fenced at the commit boundary; input body/time and worker
budgets remain bounded even when an HTTP request is aborted. Post-replacement
failures are treated as uncertain and are not automatically retried. Browser
credentials/drafts remain in memory, lock clears private views, and untrusted
Markdown renders through structural DOM data without executable HTML or automatic
resource fetching. See [the monitor boundary and validation](local-monitor.md).
These controls do not change the same-UID/administrator trust boundary or turn a
board response into owner consent. This addition is not an external security audit.

## Remaining acceptance limits

The three scoped reviewers approved the remediations with no remaining confirmed
material findings in their scopes. Formal cryptographic verification, exhaustive
fuzzing, an external human audit, a physical Internet/router matrix, supported
platform/native-model acceptance, and binary redistribution licensing checks remain
outstanding. This review completes the requested three perspectives, not all of
Milestone 6's release acceptance. Same-UID code and administrators remain inside
the endpoint trust boundary; at-rest database encryption is not provided. Agents
must continue treating peer message text as untrusted content rather than user
approval; cryptographic machine authentication cannot prevent model prompt injection.

The review used disposable fixtures. No installed service, production pairing/grant,
native model session, saved browser profile or host networking was changed.
