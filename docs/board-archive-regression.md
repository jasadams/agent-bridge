# Board archive regression evidence — 30 October 2026

Archive/Restore share one serialized broker lifecycle across CLI, MCP, HTTP,
terminal and browser. Retention is a persisted UTC deadline of 30 × 24 hours;
startup and five-minute cleanup do not depend on a viewer. Exact Markdown and its
content timestamp survive restore. Lifecycle revisions, controls and named watches
are fenced across restore and same-name reuse.

## Exercised source

Fresh task base was `540914c48a9f7808ef711b48ff00bcc0fb0a4eda`; integration was
rebased onto fetched main `a6c6727c821a34f4461ce995f9ae830c6373364a` before the
final inventory. Guidance: `AGENTS.md`, `CLAUDE.md`, `.claude/team.md`,
`.claude/build-test.md`, `docs/CODING_STANDARDS.md` and the complete ticket ledger.

Actual viewer tests exercised an **uncommitted development snapshot**, not a Git
commit or release: `a6c6727c821a34f4461ce995f9ae830c6373364a+agb25-wip-f14702a71290a9ab`.
Binary SHA256: `46f9f06a314afcc3754472a593b7b3307b86a5f73ae919290dc53bff035af86a`.
The adjacent [source/evidence manifest](board-archive-regression.json) records all
78 compiled source hashes, their manifest digest and evidence receipts. Compiled
sources stayed unchanged through the build and viewer runs. Later test-expectation
and report edits are not relabelled as new viewer executions. PR CI independently
checks the final commit.

## Controlled storage and API checks

The Rust tests use private temporary state and explicit controlled timestamps.
They cover just-before/at/after expiry, restart without extension, rollback,
repeat archive, restore/rearchive, startup and periodic cleanup, serialized
write/archive and restore/purge races, reserved names and identical-byte name reuse.
They exercise actual stale update, checkbox, response, message and subscription
requests, including a fresh revision paired with an old control. Accepted message
receipt readback remains exactly-once; lifecycle actions create no agent messages.

Nine subprocess exits interrupt restore at durable intent, empty staging before
identity, recorded staging before bytes, partial/full writes, rename before fsync,
fsync before final transaction, failed transaction and committed active state.
Recovery reopens the database after the deadline and preserves exact bytes, time
and admitted generation. Purge transaction failure retains retryable metadata after
unlink. Symlink/hardlink/replaced-target failures preserve unrelated files.
Deterministic same-device/inode/content collisions with different birth/change
metadata fail closed at destructive boundaries. Unsupported birth-time metadata
has no inode-only fallback; actual staging writes/rename retain birth identity.

Authenticated HTTP and real stdio MCP/CLI checks cover strict requests, origin and
access boundaries, protected main, conditional metadata/SSE lifecycle changes,
read-only archives, exact restore and stale revision rejection.

## Actual viewers

- Linux Chromium: **18/18 groups passed**, command exit0 and broker exit0. This
  includes Archive/Restore and dates, exact CRLF/Unicode/time restoration, main,
  local Save/Discard/Cancel, external archive/identical restore during selection,
  pending save, late read and uncertain save, and accepted-message hold/drop receipt
  recovery. Real successful lifecycle responses are held, dropped or have only
  their decoded incarnation/generation corrupted; uncertainty remains pinned to
  the original operation, explicit Inspect recovers and no action is replayed.
  Copy/raw/footer/catalog/privacy and the 320px dialog are checked.
- Linux PTY: **4/4 scenarios passed**, command, four viewers and broker all exit0.
  Main protection, confirmation/cancel, exact Archive/Restore/content/time,
  archived picker/read-only state and retained external-archive drafts are tested.
  Strong waits require actual READ-ONLY and the restored revision before input.
- Existing ordinary browser regression: **61 groups passed**, command exit0 on the same
  immutable binary, including ten catalog, fifteen message and seven copy
  subcases. The private message fixture reports no surviving thread or cleanup
  errors.

The TUI uses a single FIFO RPC worker. The exact already-observed-lifecycle/late
successful-save branch is covered by deterministic View regression assertions,
including retained draft/base and rejected implicit rebase; it is not claimed as
native receipt overtaking. No new post-save RPC behavior was introduced.

Reproduce on an isolated build:

```sh
python3 tools/monitor_board_archive_qa.py --binary /absolute/agent-bridge --binary-sha256 SHA256 --source-checkpoint SOURCE --output /private/new-web-results
python3 tools/test_board_archive_tui.py --binary /absolute/agent-bridge --output /private/new-tui-results
python3 tools/test_monitor.py --binary /absolute/agent-bridge --browser --output /private/new-monitor-results
```

## Gates and preserved failures

Rust 1.98.0 was selected directly from the existing installed toolchain, with one
worktree-specific `target/debug0`, locked dependencies and debug info disabled.
The repaired build, formatting, all-target check and strict Clippy each exit0.
Final package result: exit0, **560 passed, zero failed, five ignored** across 52
test-harness summaries. Two ignored entries are child-process entry points invoked
by their passing crash/timezone parents; three require a physical multicast LAN
and were not run.

The full initial inventories are retained, including nonzero exits:

- Storage recovery compile failed once from the cleanup-worker receiver, then
  passed after repair; the crash parent invokes its ignored child explicitly.
- Initial backend inventory: 136 passed, two failed, two helper entry points
  ignored. A notice test expected the wrong existing revoke state; a TUI fixture
  used a raw rather than broker revision. Both fixtures were corrected.
- First full inventory: formatting1; check/Clippy/package101 blocked by one test
  JSON-value comparison type error. No blocked target was counted as passing.
- Repaired full inventory: 542 passed,15 failed,5 ignored. Fourteen failures were
  old-schema fixtures retaining the newly added table; one strict compact receipt
  expectation omitted the lifecycle field. Test reconstruction/expectations were
  fixed without changing unrelated production behavior.
- Next full inventory: 559 passed,one failed,5 ignored; one conditional-read
  expectation still omitted `state: active`. This exact assertion was corrected.
- The first existing normal-browser run failed one catalog fake-clock fixture: it
  advanced time before the injected abort listener was ready. A ready flag now
  synchronizes only that fixture; timeout/backoff/singleflight assertions remain.
  The targeted catalog rerun passed all ten cases.
- Earlier browser runs1–3 failed from harness document reset, unevaluated init
  function and optional-field indexing. They remain failed records. Run4's durable
  14-group pass predates receipt hardening; its original resumed shell exit was
  unavailable and is not separately claimed. Run5 above consumed actual exit0.
- Earlier PTY run used a weak archive-name wait. The corrected wait was rerun and
  preserved; both old runs predate the final source and are historical evidence.

Independent review requested changes to long-lived inode identification, late TUI
save draft retention and web receipt identity admission. All three were repaired
and freshly reviewed; no cryptographic signing was required.

## Scope and retained receipts

This is private loopback/browser/PTY and controlled-time testing. No owner broker,
configuration, credentials, boards, services or native sessions were changed.
There was no physical 30-day wait, macOS interactive viewer run, physical LAN run,
or new model/native-client qualification. The browser suite does not invent a
live 503 restoring/410 expiry/purge-recreate UI result; controlled backend/API tests
cover those lifecycle boundaries. External user backups are outside cleanup.

Local evidence remains under `/home/jason/.cache/agb25-gates`,
`/home/jason/.cache/agb25-checkpoints/final1-20261030T024519Z`,
`/home/jason/.cache/agb25-web-qa5`,
`/home/jason/.cache/agb25-tui-final1-20261030-0248`, and the normal browser receipt
listed in the manifest. Failed runs and written reviews remain available.
