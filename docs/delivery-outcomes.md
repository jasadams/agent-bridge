# Message delivery outcomes

Agent Bridge records one durable message lifecycle, exposed as four outcomes.
AGB-33 retains existing disk/wire names through a single public projection; it
introduces no migration or parallel lifecycle.

| Outcome (`state`) | Meaning |
| --- | --- |
| Pending (`pending`) | The bridge persisted the message; delivery is queued, in progress or policy-blocked. A policy block includes a reason and requires explicit retry after restoring access. |
| Delivered (`delivered`) | The recipient client confirmed acceptance through its supported native interface. This does not prove model read, understanding, action or task completion. |
| Failed (`failed`) | Definite rejection/non-delivery for the current attempt, including cancellation of unattempted work. Inspect the reason and retry restrictions. |
| Unknown (`unknown`) | Delivery may have happened, but confirmation is missing or insufficient. Never automatically replay an ambiguous native write. |

Normal messaging is send-and-continue. Status inspection is diagnostic, for a
failure or missing response, not a follow-up step after every send. Sending is
asynchronous; a successful send may return Pending. During diagnosis, inspect the
same UUID with `get_message` or `agent-bridge get ID`. A rejected request's
`request_id` is not a persisted message ID. Preserve the request UUID and exact
arguments for idempotent retry; do not mistake an error for a successful receipt.
Neither Delivered nor a correlated reply is an owner approval or a permission grant.

## What each adapter can prove

| Adapter | Delivered proof | Insufficient or missing proof |
| --- | --- | --- |
| Codex | A validated, request-correlated native queue receipt matching the submitted UUID and exact input; or a validated active `turn/steer` response matching the selected exact active turn. Private same-user socket, canonical home, loaded-thread and capability checks still apply. | Merely writing the socket, mismatched/lost response or malformed/contradictory receipt is Unknown. Active steer acceptance returns a turn ID, not proof of model consumption. Queue acceptance may wait behind existing native work. The exact recognized native queue-capacity rejection is Failed. |
| Claude Code | The current socket-only adapter has no native acceptance receipt and therefore cannot establish Delivered from a successful write. | A successful socket write is Unknown, not acceptance. A definite pre-write rejection/failure is Failed; ambiguous writes stay Unknown. A later fixture-observed model request or reply is independent round-trip evidence, not a fabricated native receipt. |
| OpenCode | Protocol-2 `inserted` receipt fenced to the exact broker namespace, session, generation, attempt and native message ID, after exact native readback verifies user role, session, ID and complete text. Read-only reconciliation can recover the same proof. | HTTP 204 from `promptAsync` alone is not insertion evidence. Lost/malformed/mismatched receipts, corrupt/missing attempt evidence or absent exact native readback remain Unknown. `deferred` is pre-attempt Pending, not hidden accepted work. |
| Local inbox | Durable acceptance into the exact local inbox. | Inbox acceptance does not mean an agent read it. |

See [Codex delivery](codex-delivery.md), [OpenCode recovery](opencode-recovery.md)
and [native sessions](native-sessions.md). Remote bridge durable transfer acceptance
is not native acceptance: the sender synchronizes the destination's latest known
outcome. Remote observations can be stale; inspect their observation timestamp.

## Capacity, recovery and cancellation

Successfully accepted native history no longer consumes Codex's 64 pending-work
slots, including history without any recipient tool calls. All aliases of the same
native session still share admission. Genuine pending work, dispatcher concurrency,
message bytes, inbound retained-history quotas, native queue capacity and OpenCode
journal lifetime limits remain bounded. Delivery is not permission to erase history
or deduplication tombstones.

Unknown is not a hidden receipt debt. It never triggers automatic native replay.
Codex Unknown history does not exhaust pending-work admission. OpenCode retains
its attempt-aware FIFO barrier: inspect exact native history and explicitly request
read-only reconciliation. Neither absence of proof nor `--allow-duplicate` authorizes
a second OpenCode prompt. Restarts preserve uncertainty and exact-session ownership.

Cancel remains available only for eligible unattempted work. It reports Failed with
a cancellation reason; it cannot recall native input or a possibly accepted remote
transfer. A policy block instead remains Pending, with the reason and need for an
explicit retry visible. Restoring permission does not silently resume blocked work.
A definite latest-attempt failure cannot prove an earlier duplicate-authorized
attempt never executed.

## Deletion and retention inventory (AGB-33)

### Removed agent-driven receipt work

* The operational acknowledgement command/tool and active recipient-driven
  acknowledgement mutation paths are deleted. Delivery no longer depends on a
  recipient performing record maintenance.
* Codex admission no longer counts successfully submitted/delivered history as
  unresolved receipt slots. Ordinary messages and typed controls use native
  delivery evidence rather than different receipt requirements.
* OpenCode session and compaction guidance no longer instructs recipients to
  acknowledge messages or preserve acknowledgement debt.
* `plugins/opencode/native.ts` and `setup-native.ts` remove the acknowledgement
  stage, its tool call and its pass flag. They check native acceptance/readback,
  actual correlated replies and the unchanged permission/continuation boundaries.
* `tools/qualification_native.py` removes both directions' acknowledgement calls
  and native allowlist entries. It observes model input independently in each
  direction, invokes one correlated reply, and expects Claude socket delivery to
  remain Unknown. `tools/codex-native-delivery.py` removes the acknowledgement
  stage and verifies native receipt/input plus the actual reply result instead.
* Normal README, MCP and injected context describe send-and-continue, not receipt
  exchanges, outcome polling or migration reminders. Diagnostic docs describe the
  four outcomes. Historical release notes and
  measured qualification artifacts are not rewritten as if a new run occurred.

### Compatibility retained, and why

| Retained surface | Reason and removal boundary |
| --- | --- |
| Historical stored `submitted`, `acknowledged`, `uncertain` labels | Read old history without destructive rewriting, replay or loss of provenance. Historical acknowledgement is receipt evidence only. Submitted rows need immutable captured provider evidence: Codex/OpenCode/inbox acceptance projects to Delivered; Claude or unknown provider projects to Unknown. Keep until an explicit, independently validated migration/rollback policy replaces old storage. |
| Peer protocol-v1 legacy labels | Mixed-version peers must continue parsing and synchronizing accepted history. Accept legacy `submitted`/`acknowledged` as acceptance evidence and `uncertain` as Unknown; retain legacy wire encoding where required by old readers. This is compatibility decoding/encoding, not a recipient acknowledgement API. Remove only with explicit peer protocol negotiation/version retirement. Old peers can still have their own receipt-based admission limit; this release cannot silently repair their installation. |
| Internal `queued`, `sending`, `blocked`, `cancelled` phases | Dispatcher claims, permission recovery and cancellation still need operational distinctions. Project queued/sending/blocked to Pending and cancelled to Failed, preserving reasons. These are not extra public outcomes or a parallel lifecycle. |
| OpenCode protocol-2 `inserted`, `deferred`, `uncertain` receipts and journal phases | Existing installed plugins and durable attempt fences require this adapter evidence. `uncertain` maps to Unknown, not automatic replay; `inserted` needs exact native proof. Change only with a separately negotiated adapter protocol and journal compatibility plan. |
| Legacy OpenCode IDs and reconcile-only mappings | Old retries could erase attempt provenance. Absence of native proof cannot safely authorize a new prompt or a new namespaced ID. Remove only when retained historical attempts can be resolved safely; never expire them into replay permission. |
| Transport receipt/status cursors and transfer diagnostics | Remote durable acceptance, deduplication and reconnect progress are still necessary. Cursor acknowledgements are transport bookkeeping, not model tool calls. They never establish model read or task completion. |
| Dated reports, release notes, `docs/diagnostics/native-compatibility.json` | These record what older binaries actually did, including legacy tool calls. Preserve evidence honestly; a modern runner's changed assertions are not a rerun of those artifacts. |

Exact implementation boundaries: `src/main.rs` removes `Commands::Ack`;
`src/mcp.rs` removes the acknowledgement tool and its argument schema;
`src/protocol.rs` removes `Request::Ack`; `src/broker.rs` removes its dispatch;
`src/db.rs` removes `Db::ack`. There is no remaining local acknowledgement writer.
The historical-label readers are `protocol::delivery_state`, the immutable-provider
projection in `Db::delivery_state`/`counts`, and the monitor's pre-pagination SQL.
`network::store::apply_events` alone can still import `acknowledged` from an old
authenticated peer. `sync_events` retains v1 labels and maps read-only OpenCode
reconciliation back to `uncertain` so another bridge cannot mistake proof lookup
for a new Pending submission. It preserves event sequence/cursors and stored history.
`migrations.rs` retains the original event ledger/trigger; no migration, new receipt
table or alternate status column is introduced. Old board admission receipts are
normalized on read in `broker.rs`, not rewritten or reenqueued.

Claude's socket-only Unknown rows do not consume the global queued-work count,
any Codex session slot, or network in-flight work after remote durable acceptance.
They are not claimed again on restart; explicit duplicate authorization remains
required for native retry. Retained inbound history still counts toward the real
8,192-message/64-MiB per-peer and 32,768-message/256-MiB global storage quotas,
regardless of outcome. Those are history/dedup bounds, not receipt debt.

## Focused verification

The core regression must admit the next handoff after 64 successful deliveries
without recipient tool calls, while separately rejecting a full genuine pending
queue. Cover pending-to-delivered, definite rejection, lost confirmation, same-ID
deduplication, restart, historical provider-aware projection, mixed-version peers,
blocked explicit retry and cancelled failure. Exercise all three adapters with
isolated fixtures; never use the owner's broker, credentials or native state.

Plugin validation includes context assertions excluding receipt/status-poll
requirements, an ordinary native send with no recipient tool call or reply,
typecheck, unit tests, exact native readback and both native harnesses.
Qualification fixture tests must expect one
correlated reply rather than an acknowledgement stage. Native model traffic proves
fixture visibility and API/tool wiring, not external-model judgment or task quality.
