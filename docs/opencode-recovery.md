# OpenCode recovery invariants

This is the local OpenCode **2.2.5** V2 recovery contract. Standalone plugin tests
use Bun **1.3.11** and `@opencode/plugin` **2.2.5**. Broker schema 13's original
attempt fences are retained. Separately, plugin journal schema 2 adds nullable
acceptance provenance to the existing receipt table; no broker migration is added.
Public outcomes are [Pending, Delivered, Failed and Unknown](delivery-outcomes.md);
legacy phase names and protocol-2 receipt tokens are internal compatibility data.

* One broker queue owns FIFO, keyed by provider and exact native session across
  aliases. A deferred or unresolved attempted head blocks fresh submissions,
  including earlier row IDs requeued after a later attempt became Unknown.
  Pause and queued cancellation remain broker operations. The plugin never
  retains an accepted hidden queue of future prompts.
* Every message has immutable native identity and rendered content. Native IDs
  include a durable broker namespace and exact native session, avoiding global
  native-ID collisions between brokers/sessions. All legacy IDs reconcile read-only, including queued rows: old retries erased
  attempt provenance. They keep their original native IDs and cannot receive a new
  namespaced ID or another prompt. Independent transcript readback must match the
  exact scoped session, user type, native ID and complete text.
* Before sending a new delivery, the broker persists its attempt fence. A lost
  transport response becomes Unknown, never permission for another new prompt.
  Explicit reconciliation is read-only, even with `--allow-duplicate`.
* The private plugin SQLite journal commits prepared, then atomically compares
  and changes prepared to attempting before invoking the prompt API. Only the
  winning transition may prompt. A supported V2 durable user-admission receipt
  is validated and committed atomically with existing `inserted` state before
  success is returned. This token means native acceptance, not transcript/model
  consumption. Unknown/missing/corrupt journal state is never proof of no attempt;
  a reconciliation request cannot create or reset an attempt. Tombstones are
  retained without eviction. Capacity exhaustion fails closed.
* Deferred receipts occur only before a native attempt, with no asynchronous
  retained work. Phase `queued` proves no native attempt, including a fenced
  deferred receipt. Its wait has no native-work age limit: continuing heartbeats
  from the exact live generation preserve queued work through tools and unanswered
  permissions, and admission readiness enables automatic FIFO dispatch. Existing
  per-session/global count and message byte limits bound retained work. Schema
  13's deadline column remains compatibility metadata, not an expiry condition.
  Lease expiry still runs without native events; restart never extends a lease.
  Historical failed messages are never automatically resurrected.
* Leases bind generation, instance and endpoint. Renewal, close, readiness and
  delivery outcomes are fenced against stale generations. Reopening after expiry
  requires verified live observation. Old queued work cannot silently cross a
  new generation; explicit retry is restricted to its original exact identity.
* Native admission checks broker pause, claim, generation, liveness and current
  access immediately before the journal attempt. Admission is the linearization
  point: subsequent pause/cancel does not recall already admitted in-flight work.
  Native status/selection reads do not reserve the runner or lock owner selection.

The native source commits inbox admission before returning from `session.prompt`.
The existing runner wakes without cancelling work. Admission readiness
means the exact live session may accept native input, including during native
model/tool work and unanswered permissions. Session ownership and lifecycle remain
guarded. V2 retains its current native agent/model/variant; the plugin's prompt
cannot override that selection. It never supplies tools, permissions or
peer-provided system instructions.

OpenCode owns admitted input. At the next safe step boundary it promotes inbox
input into transcript; an in-flight model request cannot see input added after it
started. Exact durable acceptance is `delivered`, not proof of transcript/model processing.
Native errors, permission denial, structured-output or compaction stops may end a
run without consuming inserted input. These outcomes never authorize replay or a
bridge-controlled second inference loop. The integration never cancels work,
answers permissions, changes owner policy or claims exactly-once model execution.

The 64-per-session and 1000-global broker limits cover queued/sending/unknown barrier
work, not Delivered native records awaiting model consumption. Durable journal
row/byte ceilings still bound lifetime acceptance, including stored proof bytes.
Protocol 2 retains its existing tokens; V1 implementation/harness paths are
superseded. Refresh assets with `opencode install`; V2 reloads config. Heartbeats use events,
not continuous model polling.

Crash outcomes: before the attempt fence, no prompt has been authorized; after the
fence but before a validated receipt is durably journaled, the outcome may remain
Unknown. Exact transcript readback can independently recover Delivered. A lost
broker response after journal commit can recover from the original receipt in
the same generation. The current reconciliation nonce is fresh, echoed for
response fencing and never substituted for the original guarded proof attempt.
Different-generation and historical NULL-proof rows require transcript evidence.
A lost native return or proof-write failure cannot use pending-inbox readback:
that HTTP method is not exposed by the supported plugin context. No recovery
invokes another prompt. An Unknown head stays a barrier until verified acceptance or explicit operator
resolution; lease renewal alone does not resolve it.

Policy revocation, unsharing and registration replacement preserve attempted
uncertainty instead of converting it to a cancellable blocked/failed queue entry.
An explicit pending read-only reconciliation may still inspect historical native
proof without restoring a revoked Receive grant; it needs the current exact live
registration and never grants native submission authority. A failed proof remains
Unknown, and no heartbeat automatically retries it.

The additive journal upgrade is transactional and preserves all old rows without
inventing provenance. Its single `acceptance` column stores original generation,
attempt and validated native type/delivery/creation time; identity and content stay
in the existing immutable row. Preparation and proof commit count content plus
proof bytes against 64 MiB, retaining the 65,536-row cap and every tombstone.
See [canonical proof sources, field consumers and rollback boundary](delivery-outcomes.md#opencode-v2-proof-and-retained-journal-fields).
