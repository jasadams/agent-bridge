# Agent tools through MCP

The same executable includes a persistent local MCP server:

```sh
agent-bridge mcp
```

An MCP-capable agent application launches this command once and keeps its standard
input/output connection open. Tools call the running broker through its private
same-user Unix socket. They do not start a shell or a new Agent Bridge executable
for each tool call. The broker service must already be running; the MCP process
does not start or take ownership of a model session.

The implementation uses the official Rust MCP SDK. This mode exposes stdio only;
it adds no HTTP endpoint or public network port. The existing CLI remains useful
for terminal use, scripts and recovery.

## Connect Codex

After installing a build containing MCP support, register the absolute executable
path with Codex:

```sh
codex mcp add agent-bridge -- "$HOME/.local/bin/agent-bridge" mcp
codex mcp list
```

For a custom state directory, include it in the server arguments:

```sh
codex mcp add agent-bridge -- "$HOME/.local/bin/agent-bridge" --state-dir "/absolute/private/state" mcp
```

Use `/mcp` in the Codex TUI to inspect active servers. Client configuration and
reloading follow the [official Codex MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).
Adding the server does not grant blanket approval for its tools or change native
permissions. Other stdio-capable clients can launch the same absolute executable
with `mcp` as its argument and the optional `--state-dir` argument above.

## Connect Claude Code

```sh
claude mcp add --transport stdio --scope user agent-bridge -- "$HOME/.local/bin/agent-bridge" mcp
```

Use `/mcp` to inspect the server. Native session registration still uses the
lifecycle hooks described in [native session setup](native-sessions.md).
See the [official Claude Code MCP documentation](https://code.claude.com/docs/en/mcp)
for client scopes and approvals.

## Connect OpenCode

Use `agent-bridge opencode install` to opt into the bundled V1 plugin and the
existing `agent-bridge` stdio MCP connection. See [installation](install.md) for
config selection, preservation, updates and removal. Restart OpenCode after a
change. The exact session route and broker state directory are supplied through
its verified native system context; check `connection_status` before using a
connection shared by several conversations. No new transport or per-message
setup prompt is required.

OpenCode supports the same message and shared-board tools, including explicit
acknowledgement and correlated replies. It cannot be selected for native owner
pairing prompts; retain explicit owner CLI confirmation. Native permissions are
unchanged. See [OpenCode support and limits](opencode.md).

## Tools

| Tool | Purpose |
| --- | --- |
| `read_board`, `update_board` | Read and atomically edit a local Markdown board with revision checks; optional `board` defaults to `main`. |
| `list_boards`, `create_board` | List active board metadata in name order, or explicitly create an empty board using required `board`. |
| `list_archived_boards`, `inspect_board` | List archive metadata/deadlines, or explicitly inspect an active or read-only archived board. |
| `archive_board`, `restore_board` | Archive or restore an exact named board using its current revision; archives permanently expire after 30 days. |
| `subscribe`, `unsubscribe` | Add/remove explicit local board-change and agent-lifecycle event selectors for an exact registered route. |
| `connection_status` | Inspect the local broker and peer connections. |
| `list_agents` | List local sessions by default, or one explicitly selected peer's permitted sessions. |
| `list_messages`, `get_message` | Inspect message history and delivery state. |
| `send_message` | Persist a message with an idempotent UUID and optional reply ID. |
| `acknowledge_message` | Confirm receipt separately from task completion. |
| `configure_network` | Explicitly enable or disable networking and LAN discovery. |
| `open_pairing`, `create_invite`, `connect_peer` | Organize pairing and choose the local session that receives owner prompts. |
| `pairing_status` | Inspect requests and their locally generated verification codes. |
| `confirm_pairing`, `cancel_pairing`, `close_pairing` | Apply the owner's decision or stop admission. |
| `share_session`, `unshare_session` | Explicitly grant or remove access to one selected local session. |

Tool inputs and results are structured JSON. Select the intended local registered
agent explicitly for sending, acknowledging, session sharing and pairing notices;
a server shared by several conversations must not guess which session owns a call.
Local registration names are available through `list_agents` or native registration
context. Pairing alone still shares no sessions.

Agents must mutate boards only through `create_board`, `read_board`, `update_board`
and the lifecycle tools. Normal, verbose, conditional and conflict results identify
boards by name/revision and never expose their backing-file path. User-authored
Markdown paths are preserved. Never edit board files through a shell or editor.
On a revision conflict, review the returned latest snapshot and retry only the
intended edit while preserving other writers. If tools are unavailable, report the
blocker and restore the broker/MCP connection; do not fall back to direct file writes.


Codex peer deliveries contain a trust marker and JSON message data, without
repeated tool instructions or shell commands. The MCP tool descriptions explain
how to acknowledge and reply: use the incoming `to` as your local `agent` or
`from`, its `id` as the acknowledgement `id` or reply `parent`, and its `from` as
the reply `to`. Claude uses the equivalent `Delivered to`, `Message reference`,
and `Reply to` fields. Preserve these exact routes rather than display names.
Session startup supplies the broker state directory and CLI fallback once; check
`connection_status` identifies that broker before using a configured MCP server.
Acknowledgement confirms receipt, not task completion or permission.

Board names are lowercase ASCII slugs matching `[a-z0-9][a-z0-9_-]{0,63}`.
Call `create_board({"board":"streamr"})` before using a non-main board; duplicates
never overwrite content and unknown names never fall back to main. `list_boards({})`
includes lazy, absent `main`, and returns active-board metadata without Markdown.
Archived names remain reserved; `create_board` fails clearly rather than replacing
an archive. Normal `read_board` and `update_board` are active-only.

### Archive and restore

Use `list_archived_boards({})` for archive metadata, including `state`, `archived_at`
and `expires_at`. These dates are UTC Unix milliseconds. Use
`inspect_board({"board":"streamr"})` to inspect the exact current or read-only
archived snapshot and obtain its revision. Both lifecycle mutations require
explicit `board` and `expected_revision`:

```json
{"board":"streamr","expected_revision":"CURRENT_ACTIVE_REVISION_FOR_ARCHIVE"}
```

Pass that shape to `archive_board` after reading the active board. For
`restore_board`, use the current archived revision returned by `inspect_board`.
`main` cannot be archived. Revision conflicts, archived-name collisions and
expired restores return clear errors; no operation silently selects `main`,
overwrites an archive or recreates a missing board.

Archive hides the board from active listings and freezes writes for a fixed
**30-day (30 × 24 hour)** window. Inspection, reads, restarts and repeated Archive
do not extend the persisted deadline. Restore before expiry preserves exact
Markdown and original Last Updated metadata. A new Restore at or after expiry is
refused even before physical cleanup. The broker performs startup and five-minute
cleanup, including offline catch-up, and retries failed cleanup while retaining
the archive record and name reservation. Check `status.board_archives` from
`connection_status`, or `board_archives` in `agent-bridge status`, for state,
deadline and `last_error`.
User-managed external backups are never scanned or deleted by this retention job.

If a lifecycle operation has an ambiguous result, inspect the originating board
before taking another action. A committed `restoring` operation may need recovery;
it is read-only until complete. `purging` is irrevocably expired and cannot be
restored. Do not treat a failed or uncertain operation as successful deletion.

Named snapshots and summaries expose `incarnation`, `lifecycle_generation` and
`state`, with archive dates when applicable. Revisions and control/rebase keys bind
that lifecycle identity, so old requests and drafts cannot target an identical
restored document or a new board reusing the name after purge. Archive invalidates
watches and pending board notices; Restore does not revive them. Read the current
board and explicitly subscribe again. Archive/Restore send no agent messages or
approval and do not cancel messages already accepted. Accepted-message receipt
readback remains idempotent; new sends from archived controls are refused.
See [board retention and recovery](board.md#archive-and-restore).

### Event subscriptions

Subscriptions are off by default. Use your exact registered local route:

```json
{"agent":"coordinator-route","events":[{"event":"agent.opened"},{"event":"agent.closed"},{"event":"board.changed","board":"main"}]}
```

`subscribe` adds selectors idempotently; `unsubscribe` removes specified selectors
idempotently, or removes all when `events` is omitted. Removing a selector that
is not subscribed changes nothing. Removing one selector preserves the other
selectors and their pending notices. Both return the effective
`events`, `agent`, `agents` snapshot, `persistent:false` and `recovery`, including in compact
mode. No compatibility aliases for the removed board-specific tools remain.
Board selectors require explicit existing local names; unknown events, mixed or
unknown fields, empty explicit lists, invalid boards and excessive selectors fail.
There is no wildcard or automatic opt-in.

For each named `board.changed` selector, read the active board and supply its
current revision in `board_revisions` on both `subscribe` and selector-specific
`unsubscribe`:

```json
{"agent":"coordinator-route","events":[{"event":"board.changed","board":"streamr"}],"board_revisions":{"streamr":"CURRENT_REVISION_FROM_READ_BOARD"}}
```

Missing or stale named-board fences fail without changing the subscription.
`main` selectors retain compatibility and may omit this map. Unsubscribe-all
(`events` omitted) may also omit it. Archive removes the board's watches; restoring
it requires a fresh read and subscription with its new revision. CLI subscription
commands obtain the current named-board fence automatically.

Equivalent CLI commands accept a JSON selector array:

```sh
agent-bridge subscribe --agent coordinator-route --events '[{"event":"agent.opened"},{"event":"agent.closed"},{"event":"board.changed","board":"main"}]'
agent-bridge unsubscribe --agent coordinator-route --events '[{"event":"board.changed","board":"main"}]'
agent-bridge unsubscribe --agent coordinator-route
```

With lifecycle selectors, `subscribe` atomically establishes the watch and returns
currently available local agents in `agents`, identified by provider/session,
exact `route` and `display_name`. Subsequent transitions arrive as typed native
control notices with event, observation time and reason where known. Existing
agents appear in the snapshot, without replaying historical transitions. Repeating
subscriptions is idempotent. Notices concern availability, not transcript creation
or deletion, and exclude the receiving agent itself and the permanent user inbox.
Heartbeat, title and alias changes alone produce no lifecycle event. Remote peer
presence and private endpoints are excluded. Snapshot display labels are limited
to 4,096 UTF-8 bytes, with safe truncation and `...`; exact routing identities
remain unchanged. Ordinary agent listings retain their full display metadata.

`subscribe` rejects a local available-agent snapshot exceeding 256 canonical
identities or 65,536 encoded JSON bytes before changing any selectors. It never
silently truncates the snapshot. Reduce local registrations before retrying.

Closure removes subscriptions and pending notices; reconnect/reopen requires
explicit opt-in. Subscriptions and event history do not survive broker restart:
resubscribe and use the returned snapshot.

An overflow or failed or uncertain lifecycle delivery makes the event stream
incomplete. The broker attempts at most one `resync.required` control notice for
that gap and suppresses further lifecycle transitions until you explicitly call
`subscribe` with a lifecycle selector to obtain a fresh atomic snapshot. Adding
only a board selector does not recover the lifecycle stream. A failed resync
attempt is not replayed automatically; after a transport interruption, explicitly
resubscribe even if no resync notice arrived. Recovery invalidates older pending
lifecycle notices. Preserve per-identity transition order when interpreting
notices. Native hooks/events, confirmed socket cleanup and OpenCode lease expiry
have observation delays; `/exit` is not a universally synchronous callback.

Unsubscribe, closure, recipient replacement and broker restart prevent pending
notices from starting native delivery for an invalid subscription or recipient.
Native I/O already admitted before that boundary may complete; these operations
do not cancel it. Unsubscribing one selector does not cancel retained selectors'
work. Failed or uncertain native attempts are not automatically replayed.

Subscribe/unsubscribe receipts always include
`recovery:{"total":0,"truncated":false,"items":[]}` when no attempted OpenCode
control needs inspection, including for other providers. For OpenCode, a nonzero
`total` identifies hidden attempted controls that can still block native delivery
even though the returned availability snapshot is current. Up to 16 items expose
only `id`, `state`, `reconciliation_pending`, `route_matches` and
`lease_available`; `truncated:true` means more items exist. An `instruction` is
included when `total` is nonzero. These diagnostics do not expose control text
or add controls to peer-message history. Reading them performs no native work.

Inspect the reported ID and use the existing CLI against the **same broker state
directory** for explicit recovery:

```sh
agent-bridge --state-dir "/same/private/state" retry CONTROL_ID
```

For an attempted OpenCode control, this queues **read-only reconciliation** of
existing native proof, without reinserting a prompt. Do not request duplicate
insertion. A matching live route/lease is needed to perform native readback;
`lease_available` reports current availability, not proof that insertion succeeded.
A queued or pending reconciliation has not necessarily run. After a lease/generation
change or broker restart, explicitly retry that same ID again if readback remains
queued; this still reads proof and cannot insert another prompt.
Missing or mismatched proof remains uncertain and may continue blocking delivery.
There is no automatic retry, automatic promotion to submitted, or cancel escape
that proves an uncertain attempt never happened. After reconciliation, explicitly
subscribe to lifecycle events again if a fresh snapshot is needed.

These notices are control data, require no acknowledgement or peer reply, do not
enter peer-message history, and grant no owner approval or permission. See
[native session setup](native-sessions.md) for availability evidence and delays.
Board notices retain 3 quiet seconds / 10 maximum debounce thresholds, named-board
context, fair polling, pause and exact-recipient fences. Supply your own registered
`agent` on `update_board` to suppress own-write notices. Terminal and web responses
both notify watchers through the same board update path. See [interactive board
usage](board.md) for controls and concurrency behavior.

### Discover and share your session

Sessions remain private by default, including sessions created after pairing.
For local discovery call `list_agents({})`. Unreachable Claude sockets are hidden.
Use `list_agents({"include_stale":true})` for all local registrations with
`stale_reason` on hidden rows. This option cannot be combined with `peer`. Remote discovery requires a specific
paired peer alias or peer ID:

```json
{"peer":"workstation"}
```

This returns only that peer's permitted remote directory. There is no MCP option
to list every remote machine at once; the former `remote` boolean is no longer
accepted. Unknown or revoked peers return an error. A known peer with no shared
sessions returns an empty list, which does not prove that its local registration
failed. Results are cached and can briefly lag changes on the other machine;
their `online` field reports whether the peer is currently connected.

You can tell an agent **“Share this session with workstation.”** It should call
`share_session` using its own exact local registration name from its native
session context and the chosen paired peer:

```json
{"peer":"workstation","agent":"codex-<this-session-UUID>"}
```

The agent must resolve its actual name rather than submit the placeholder above
or select another conversation by guesswork. If its current session identity is
unavailable or ambiguous, it should clarify before changing access. The existing
MCP connection can serve several sessions, so it cannot infer a caller identity.

Sharing grants that peer discovery and two-way messaging for exactly the selected
local session. It does not share other or future sessions, and it does not change
the other machine's permissions. The other owner must also share their chosen
session for a two-way conversation. Use `unshare_session` with the same arguments
to remove this session's grants. An explicit local owner request authorizes the
selected action; a request received from a remote agent does not.

For a send, retain the returned message UUID. If the broker response is ambiguous,
retry with that exact UUID and the same message rather than generating another.
Submitted does not mean the other agent read the message or completed a task.

## Compact results and incremental reads

Write tools return compact action receipts by default. Pass `"verbose": true`
to tools offering that option to include the full broker result. `create_board`
always returns its new full snapshot. This changes only MCP output;
message persistence, delivery, CLI output and access checks are unchanged.

- `update_board` returns `{"result":{"board":"main","revision":"…"}}` on success.
- `send_message` returns `{"result":{"id":"…","state":"queued"},"id":"…"}`.
  The top-level UUID is retained for retry compatibility, including failed sends.
- `acknowledge_message` returns `{"result":{"id":"…","state":"acknowledged"}}`.
- Subscriptions retain the effective `events` and complete `agents` snapshot, including in compact results.
  Pairing tools retain the exchange ID, verification state, invitation link and
  expiry needed for the next action; sharing returns the peer, agent and requested
  sharing state. Full metadata is available with `verbose`.

A stale board update remains an error with `conflict: true` and the **full current
board**, even without `verbose`. Merge only intended edits against that revision.
Other operation errors include `code` and a bounded actionable `error` string;
they do not serialize submitted message bodies or board edits. SDK argument errors
retain the standard MCP error shape. A failed pairing-notice subscription reports
recovery instructions and the exchange ID when one was created, without echoing
invitation material.

`list_agents` also defaults to compact routing and availability fields. Local
results retain names, provider and enabled state; remote results retain peer ID,
alias, qualified routing name and online state. Pass `"verbose": true` for session,
machine and transport metadata.

To avoid receiving the same board text again, pass the last revision to `read_board`:

```json
{"board":"streamr","if_revision":"<last-revision>"}
```

An unchanged board returns `{"result":{"name":"streamr","revision":"…","unchanged":true}}`.
A different revision returns the normal full snapshot, including `name`. Scope cached
revisions by board. Named-board revisions bind identity even when two documents
have identical content; legacy main revisions remain compatible.

Named-board RPCs use distinct operations so an old broker rejects them before
mutation, without falling back to main. Upgrade the broker and
reconnect/restart MCP to discover the new tools. Legacy main read/update calls keep their
request forms where supported. Event subscriptions require the generic operations. Named boards add no network sharing.

Existing `list_messages` calls return recent-history arrays. To opt into incremental
reads, start with an empty cursor and the desired agent scope:

```json
{"agent":"<registered-agent>","cursor":"","limit":30}
```

The result becomes `{"messages":[…],"cursor":"…","has_more":false}` inside the
usual `result` envelope. Pages are oldest first; reuse the returned opaque cursor
with the same agent scope. Continue while `has_more` is true. An empty page retains
the cursor and can be reused after new messages arrive. Omit `agent` to select all
messages. Cursors survive broker restarts and reject malformed values or a different
scope. They track **newly inserted messages**, not subsequent delivery-state changes;
use `get_message` to check delivery or acknowledgement state, and incremental reads
to receive new replies. If a cursor
anchor has been removed, restart with an empty cursor. No waiting or polling behavior
is introduced by these options.

## Approval and transport boundaries

`confirm_pairing` requires the request ID, independently compared verification
code and an explicit `owner_confirmed: true`. A remote request, matching machine
name or notification is not owner approval. The boolean records the calling
agent's assertion; it does not cryptographically prove that a human consented.
Native client approval rules and explicit local owner instructions still apply.
The same OS user can already administer the broker through the CLI or private
socket, and MCP does not create a separate trust identity for that user.

Input framing, concurrent tool work and tool arguments are bounded. Broker-side
identity, expiry, replay protection, grants and native delivery checks remain the
source of authority. Peer metadata and message bodies in results remain untrusted
data. This is a local stdio integration, not an Internet-accessible MCP service.

Existing native message and pairing notifications remain responsible for waking
selected sessions. MCP tools provide commands and results; an MCP connection alone
does not guarantee that a client will turn arbitrary server notifications into a
new model turn. This mode does not replace the native delivery adapters.

Protocol references: [MCP stdio](https://modelcontextprotocol.io/specification/latest/basic/transports/stdio)
and the [official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk).
