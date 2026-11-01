# Message delivery outcomes

Agent Bridge records one durable message lifecycle, exposed as four outcomes.
AGB-33 retains existing broker disk/wire names through a single public projection;
it introduces no broker migration or parallel lifecycle. The existing OpenCode
plugin receipt journal has a separate additive schema-2 upgrade described below.

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
| OpenCode V2 | Exact supported `session.prompt` durable user-admission receipt, validated and committed to the existing receipt journal before success; alternatively exact scoped native transcript evidence. Protocol-2 `inserted` remains the compatibility token for verified client acceptance, not a transcript/model-processing claim. | Lost/malformed/mismatched native returns or a failed proof commit remain Unknown. Same-generation read-only reconciliation can use the original guarded durable journal proof. Missing proof, old/null-proof rows or a changed generation require independent exact transcript readback; absence never authorizes another prompt. `deferred` remains pre-attempt Pending. |
| Local inbox | Durable acceptance into the exact local inbox. | Inbox acceptance does not mean an agent read it. |

See [Codex delivery](codex-delivery.md), [OpenCode recovery](opencode-recovery.md)
and [native sessions](native-sessions.md). Remote bridge durable transfer acceptance
is not native acceptance: the sender synchronizes the destination's latest known
outcome. Remote observations can be stale; inspect their observation timestamp.

### OpenCode V2 proof and retained journal fields

OpenCode **2.2.5**'s installed OpenAPI describes `POST /api/session/{sessionID}/prompt`
as durable admission, returning `Session.Inbox.User`. This is not inferred from a
type name: the versioned canonical [Session.prompt implementation](https://github.com/anomalyco/opencode/blob/v2.2.5/packages/core/src/session/session.ts)
awaits `SessionInbox.admit` and then returns the admitted input. The
[inbox implementation](https://github.com/anomalyco/opencode/blob/v2.2.5/packages/core/src/session/inbox.ts)
publishes `InboxEnqueued`; the [Bus implementation](https://github.com/anomalyco/opencode/blob/v2.2.5/packages/core/src/bus.ts)
commits the event and its projection in one immediate database transaction before
publication returns. First admission wins by ID/session/type, even if a caller
supplies changed text, so comparing the **returned** complete text is essential.
Steered input is promoted from the durable inbox into transcript at a later safe
step boundary; a held tool, stream or permission can delay that promotion.

The plugin validates returned native ID, exact session, `type: user`, complete
text, `delivery: steer`, finite creation time and absence of unexpected attachments.
The existing namespace/generation/attempt guards and winning attempt CAS remain
mandatory. The supported plugin API returns this receipt but does **not** expose
the HTTP client's inbox-list method. Production uses no cast, guessed plugin API,
configured HTTP integration or native-state scraping.

Journal schema 2 transactionally adds one nullable `acceptance` column to the
existing `receipts` table. A receipt commit atomically writes existing `inserted`
state plus bounded JSON facts: original `generation`, original `attempt`, native
`type`, `delivery`, and `created`. Namespace, message ID, session, native ID and
complete text remain bound by the existing immutable row; text is not duplicated.
No new table or lifecycle is added. Historical `inserted` rows retain NULL proof:
they fence replay but do not fabricate a V2 admission receipt. Older journal
readers reject schema 2; preserve the matching journal/plugin/broker backup for
rollback. No tombstone deletion or down-migration is provided.

`Journal.accepted()` is the only receipt-proof writer, after the original guarded
native call. `hasAcceptance()` is consumed by duplicate delivery and read-only
reconciliation; it checks immutable row identity, original proof shape and current
generation. An ordinary delivery also checks the original attempt. Reconciliation
instead has a **fresh request nonce**, echoed unchanged for the broker's current
response fence: it does not overwrite or pretend to equal the original proof
attempt. Original provenance is established by the guarded journal commit, not an
invented independent broker field. `Journal.inserted()` retains the independent
transcript-proof path and never manufactures receipt JSON. Exact transcript
readback remains usable across a generation change or a missing receipt mapping.

Both row preparation and proof commit count UTF-8 `content` **plus** `acceptance`
bytes against the same 64-MiB ceiling; the 65,536-row ceiling is unchanged. A failed
capacity/write commit after native admission remains Unknown with the original
attempt fence. A lost native response **before** journal proof cannot be recovered
from a pending inbox by this plugin; it remains Unknown until exact transcript
evidence exists. A lost broker response **after** proof commit can reconcile in
the same generation. Replaced generation, malformed proof, or legacy NULL proof
requires exact transcript evidence. No recovery path calls `prompt` again.

## Capacity, recovery and cancellation

Successfully accepted native history no longer consumes Codex's 64 pending-work
slots, including history without any recipient tool calls. All aliases of the same
native session still share admission. Genuine pending work, dispatcher concurrency,
message bytes, inbound retained-history quotas, native queue capacity and OpenCode
journal lifetime limits remain bounded. Delivery is not permission to erase history
or deduplication tombstones.

Unknown is not a hidden receipt debt. It never triggers automatic native replay.
Codex Unknown history does not exhaust pending-work admission. OpenCode allows
later eligible messages past an Unknown, while retaining its per-message attempt
fence: inspect exact native history and request read-only reconciliation. Neither absence of proof nor `--allow-duplicate` authorizes
a second OpenCode prompt. Restarts preserve uncertainty and exact-session ownership.

For OpenCode, `get_message` exposes `opencode_attempt` separately from the
current `detail`: a recorded initiating Unknown includes its message, session,
generation, attempt, timestamp in Unix milliseconds, and a privacy-safe error
category. `timestamp_kind: detected` means the broker's detection time, not a
claimed native failure time. Subsequent read-only reconciliation updates the current detail without
replacing that original evidence. Historical Unknown records whose original
evidence was not retained report `status: unavailable`; missing fields in older
recorded evidence are not reconstructed from later reconciliation.

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
| OpenCode protocol-2 `inserted`, `deferred`, `uncertain` receipts and journal phases | Retain adapter tokens and attempt fences, not a parallel public lifecycle. `inserted` means validated native client acceptance (guarded durable V2 receipt or exact transcript proof), never model consumption. Plugin journal schema 2 adds nullable bounded acceptance provenance; old NULL-proof rows remain transcript-only. Retire these tokens/schema compatibility only with an explicit adapter/journal compatibility plan. |
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

A completed Claude frame retains internal `submitted`, distinct from an actual
partial/lost native write (`uncertain`); immutable provider projection makes both
publicly Unknown without claiming acceptance. Completed submissions preserve normal
opt-in lifecycle transport behavior. Genuine ambiguous lifecycle attempts mark the
existing stream desynchronized, fence unattempted old controls and require an
explicit fresh snapshot, but never enqueue automatic native resync/replay. Definite
failure/overflow keeps its existing bounded resync behavior. Board revision hints
and pairing phase notices report ambiguity locally without false acceptance or
automatic resend.

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
