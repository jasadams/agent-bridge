# Agent Bridge contributor instructions

Read `.claude/team.md`, `.claude/build-test.md`, and
`docs/CODING_STANDARDS.md` before implementation. Read the ticket and relevant
feature documentation in `docs/`; Trakkt is the ticket ledger (team `AGB`).

Fetch the default branch before new implementation work and create an isolated
worktree from that fetched commit. Preserve the invoking checkout and unrelated
changes; record the base commit and instruction paths in the handoff.

Delegate independent implementation and review when useful. Keep edits scoped to
the ticket and obtain an independent written review before committing. Reviews
require findings and a verdict, with fresh review after material changes; signing,
private keys, and signed approval artifacts are not required.

Run build, test, and Clippy commands in the foreground and consume their final
results in the same turn. Never background them or end a turn awaiting completion.
Do not fix unrelated failures or reformat unrelated code. No automatic elapsed-time
limit applies unless the user explicitly supplies one.

For backlog-fast, follow change-appropriate validation and current-head PR CI.
Leave a green PR open and the ticket In Review for merge-sweeper; do not merge or
mark Done. Preserve unfinished work and review logs during cleanup.
