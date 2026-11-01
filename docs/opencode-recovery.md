# OpenCode recovery invariants

This is the local AGB-4 state-transition contract. OpenCode 1.18.35 embeds Bun
1.3.14; standalone plugin tests use pinned Bun 1.3.11 and the V1 SDK 1.18.35.
Schema 13 follows the sharing grant migration 12 without changing its constraints.
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
  namespaced ID or another prompt. Native readback must match session, user role, ID and exact text.
* Before sending a new delivery, the broker persists its attempt fence. A lost
  transport response becomes Unknown, never permission for another new prompt.
  Explicit reconciliation is read-only, even with `--allow-duplicate`.
* The private plugin SQLite journal commits prepared, then atomically compares
  and changes prepared to attempting before invoking the prompt API. Only the
  winning transition may prompt. Verified insertion is committed before success
  is returned. Unknown/missing/corrupt journal state is never proof of no attempt;
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

The native source saves user content before joining the session runner. The
Running runner joins existing work without cancelling it. Admission readiness
means the exact live session may accept native input, including during native
model/tool work and unanswered permissions. Status and native selection reads
remain guarded against lifecycle/revision races. The plugin preserves validated
native agent/model/variant; it never supplies tools, permissions or peer-provided
system instructions. Missing active selection defers before the journal attempt.
Session updates fence selection changes through broker admission.

OpenCode owns inserted input. Its next normal runner iteration reads the new
user record; an in-flight model request cannot see input added after it started.
Exact verified insertion is `delivered`, not proof of model processing.
Native errors, permission denial, structured-output or compaction stops may end a
run without consuming inserted input. These outcomes never authorize replay or a
bridge-controlled second inference loop. The integration never cancels work,
answers permissions, changes owner policy or claims exactly-once model execution.

The 64-per-session and 1000-global broker limits cover queued/sending/unknown barrier
work, not Delivered native records awaiting model consumption. Durable journal
row/byte ceilings still bound lifetime insertion. Protocol 2 retains its existing
frames and receipts; older loaded plugins remain idle-only. Reinstall and fully
restart OpenCode to activate the new plugin behavior. Heartbeats use events,
not continuous model polling.

Crash outcomes: before the attempt fence, no prompt has been authorized; after the
fence but before insertion, the outcome may remain Unknown. After insertion but
before receipt, exact native readback recovers Delivered. After a persisted receipt,
reconciliation verifies the native insertion without invoking another prompt.
An Unknown head stays a barrier until verified insertion or explicit operator
resolution; lease renewal alone does not resolve it.

Policy revocation, unsharing and registration replacement preserve attempted
uncertainty instead of converting it to a cancellable blocked/failed queue entry.
An explicit pending read-only reconciliation may still inspect historical native
proof without restoring a revoked Receive grant; it needs the current exact live
registration and never grants native submission authority. A failed proof remains
Unknown, and no heartbeat automatically retries it.
