# OpenCode recovery invariants

This is the local AGB-4 state-transition contract. OpenCode 1.18.35 embeds Bun
1.3.14; standalone plugin tests use pinned Bun 1.3.11 and the V1 SDK 1.18.35.
Schema 13 follows the sharing grant migration 12 without changing its constraints.

* One broker queue owns FIFO, keyed by provider and exact native session across
  aliases. A deferred or unresolved attempted head blocks fresh submissions,
  including earlier row IDs requeued after a later attempt became uncertain.
  Pause and queued cancellation remain broker operations. The plugin never
  retains an accepted hidden queue of future prompts.
* Every message has immutable native identity and rendered content. Native IDs
  include a durable broker namespace and exact native session, avoiding global
  native-ID collisions between brokers/sessions. All legacy IDs reconcile read-only, including queued rows: old retries erased
  attempt provenance. They keep their original native IDs and cannot receive a new
  namespaced ID or another prompt. Native readback must match session, user role, ID and exact text.
* Before sending a new delivery, the broker persists its attempt fence. A lost
  transport response becomes uncertain, never permission for another new prompt.
  Explicit reconciliation is read-only, even with `--allow-duplicate`.
* The private plugin SQLite journal commits prepared, then atomically compares
  and changes prepared to attempting before invoking the prompt API. Only the
  winning transition may prompt. Verified insertion is committed before success
  is returned. Unknown/missing/corrupt journal state is never proof of no attempt;
  a reconciliation request cannot create or reset an attempt. Tombstones are
  retained without eviction. Capacity exhaustion fails closed.
* Deferred receipts occur only before a native attempt, with no asynchronous
  retained work. The first busy deadline is durable and never extended by
  heartbeats or restart. An explicit user retry of provably unattempted work may
  establish a fresh bounded window. Lease expiry and busy deadlines run without native events.
* Leases bind generation, instance and endpoint. Renewal, close, readiness and
  delivery outcomes are fenced against stale generations. Reopening after expiry
  requires verified live observation. Old queued work cannot silently cross a
  new generation; explicit retry is restricted to its original exact identity.
* Native admission checks broker pause, claim, generation, liveness and current
  access immediately before the journal attempt. Admission is the linearization
  point: subsequent pause/cancel does not recall already admitted in-flight work.
  The integration does not claim an atomic native idle-to-submit transaction.

The native source saves user content before joining the session runner. Its
Running runner joins existing work; it does not cancel it. Busy and permission
waits defer peer submissions. A native user may start work after the final idle
check: that race can insert a peer user message while a run is active, and its
processing timing follows native runner behavior. The plugin never cancels native
work, changes permission rules, answers an approval, starts/resumes a conversation,
or claims exactly-once model execution. Bounded status/message reads occur only
for submission and explicit reconciliation; heartbeat liveness uses plugin events,
not continuous model polling.

Crash outcomes: before the attempt fence, no prompt has been authorized; after the
fence but before insertion, the outcome may remain uncertain. After insertion but
before receipt, exact native readback recovers submitted. After a persisted receipt,
reconciliation verifies the native insertion without invoking another prompt.
An uncertain head stays a barrier until verified insertion or explicit operator
resolution; lease renewal alone does not resolve it.

Policy revocation, unsharing and registration replacement preserve attempted
uncertainty instead of converting it to a cancellable blocked/failed queue entry.
An explicit pending read-only reconciliation may still inspect historical native
proof without restoring a revoked Receive grant; it needs the current exact live
registration and never grants native submission authority. A failed proof remains
uncertain, and no heartbeat automatically retries it.
