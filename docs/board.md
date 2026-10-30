# Shared Markdown board

Boards are local Markdown documents shared by agents using this broker.
The default board is `main`; agents target other boards explicitly by name.
Each board's contents are entirely up to the user and agents: tables, notes, checklists,
headings or any other Markdown. Agent Bridge does not require task/status fields
or automatically change the content when an agent starts or stops work.

Use it as the user's at-a-glance view across many agents. Keep each entry brief:
the current task, status, and any blocker or action needed from the user. Leave
logs, reasoning and implementation detail in the agent's native terminal or
session, where the user can inspect them. Refresh an existing entry when its
status changes instead of appending a running history.

Open the live terminal view:

```sh
agent-bridge board
agent-bridge board --create streamr
agent-bridge board streamr
agent-bridge board --list
agent-bridge board streamr --raw
agent-bridge board streamr --metadata
```

The document's bottom border shows **Last Updated** outside its Markdown. It
stays visible while scrolling, shortens to fit narrow terminals, and uses the
viewer's process timezone (`TZ` or the system setting), including daylight saving.
The web board shows the same instant in its footer using the browser's timezone.
Different viewers can therefore see different local clock times for identical
UTC metadata. Unknown or unavailable times are shown as unknown, never as now.
Agents no longer need to maintain clock text in the document. Existing timestamp
prose is left untouched, and `--raw` still prints the exact Markdown only.

`--metadata` prints JSON without Markdown. Board snapshots, listings, MCP reads
(including conditional reads) and update receipts expose `last_updated`, a nullable
UTC Unix timestamp in **milliseconds**, and `last_updated_source` (`file_mtime` or
`unknown`). Older brokers without these fields are shown as unknown by the TUI.

The broker persists revision/existence-bound metadata independently for each board
in its existing settings store. A real content change takes the modification time
captured from the same validated file descriptor as that content. Reads, redraws,
notifications, restarts, conflicts, rejected edits, same-value updates and metadata-only
touches do not advance a recorded time. Existing documents use their defensible
file time rather than being stamped when first opened; absent virtual `main` and
unreliable/future modification times remain unknown. A timestamp cannot be supplied
through Markdown or edit parameters.

External content changes are detected by revision refresh and use the captured
file time, not their discovery time. Restoring previously observed content after
an observed intervening change takes the restore's file time. This is not a
forensic edit log: editors can preserve file times, and an unobserved byte-identical
rewrite or A→B→A sequence cannot be distinguished from a touch. Those do not invent
a newer timestamp. If a write fails after its atomic rename, content may already
have changed; read back the coherent current revision/time before retrying, as with
other ambiguous board writes. A frozen text selection keeps the displayed revision
and timestamp together until it is released.

The terminal always shows the selected board name. Press `b` outside the text
editor to open the board picker. Save an unsaved response successfully or
explicitly discard it before switching; cancelling keeps the draft and board.
Switching resets document selection, response selection and viewport; pending
reads and saves remain pinned to their originating board.

Use `j`/`k` or arrow keys to scroll, Page Up/Down and Home/End to navigate,
`h`/`l` to pan horizontally, `w` to toggle wrapping, and `r` to refresh.
Use Tab/Shift-Tab to select a checkbox or text field in the Responses pane.
Space toggles the selected checkbox; Enter opens the selected text field.
Quit with `q`, Escape or Ctrl-C. File changes refresh the view automatically;
a brief broker outage keeps the last content visible and shows a retry status.
Extremely complex tables fall back to readable plain Markdown to bound rendering
work.
Tables include horizontal dividers between rows to keep separate items readable,
including when their text wraps across several terminal lines.
Drag across the rendered board to select text; releasing the mouse sends the
selection to the terminal clipboard. Outside the text editor, `m` toggles this
mouse handling on or off.

When a newer release is available, a quiet notice below the board shows the
current and available versions and the `agent-bridge update` command. The broker
checks in the background; the board only reads its cached result. The notice is
separate from your Markdown, does not change its revision or notify agents, and
does not interrupt drafts or text selection. See [update options](install.md#updates-backups-and-removal)
for checking explicitly and disabling automatic checks.

The broker must be running. Agents must use `create_board`, `read_board`,
`update_board` and the lifecycle tools through the existing persistent MCP server.
Identify a board by name and revision; backing-file locations are internal storage
metadata and are not part of normal responses, including verbose or conflict
snapshots. Never modify board files directly through a shell or editor. If the
tools are unavailable, report the blocker and restore the broker/MCP connection;
do not bypass it with direct file writes. There is no extra server or binary.

The existing [local web monitor](local-monitor.md) also has a Boards view. It
lists named boards, renders their Markdown and lets you answer the same existing
checkbox and marked text controls inline, beside their questions. Valid text
fields offer multiline editing with Save and Cancel; Raw Markdown keeps every
marker and original response byte. Browser selection is local to that tab; it
does not change terminal or agent selection. Creation and general Markdown edits
remain CLI/MCP operations. Browser drafts stay in memory and use explicit
save/discard/cancel choices, revision checks and current-board review on conflicts.

For a custom broker state directory:

```sh
agent-bridge --state-dir "/absolute/private/state" board
```

Board names are 1–64 lowercase ASCII characters matching
`[a-z0-9][a-z0-9_-]{0,63}`; paths, dots and silent normalization are rejected.
Use `--create NAME` (or MCP `create_board`) explicitly before using a non-main
board. Creation works without a terminal and never overwrites an existing board.
Unknown names fail without creating a file or falling back to main.
`--list` and MCP `list_boards` return deterministic name-ordered metadata
(name, existence and revision), without Markdown. They show active boards and include
absent `main`. Archived names remain reserved and count toward the 256-board limit,
including lazy `main`; listing scans at most 1,024 directory entries and errors
explicitly on excess instead of truncating. Existing boards are migrated automatically
without changing their Markdown or requiring manual file moves. A read of absent
`main` returns empty Markdown and its revision;
the first successful update creates it. Backups of the broker's state include the
board. Print its exact Markdown once with `agent-bridge board --raw`.

## Archive and restore

Archive removes a named board from active lists and makes it read-only. Its exact
Markdown remains available for explicit inspection and restoration for **30 days
(30 × 24 hours)**. `main` is protected and cannot be archived. Active boards are
never archived or deleted merely because they are old or inactive.

In the terminal, outside an editor, press `a` to request Archive for the selected
named board. The confirmation explains the retention window and permanent deletion;
Enter confirms and Escape cancels. Press `b` to open the picker, then Tab/Shift-Tab
to switch between active and archived boards. Select with `j`/`k` or arrows and
Enter to inspect. Archived entries show their archive and scheduled deletion dates.
On a restorable archived board, `r` opens Restore confirmation; Enter confirms and
Escape cancels. On an active board, `r` retains its refresh behavior.

Save successfully or explicitly discard retained response/message drafts before
archiving or navigating. Pending and uncertain operations keep their original
board and block a new lifecycle action. External archive keeps recoverable local
drafts and the displayed content, but prevents writes. Restore does not make an
old draft safe to save: read the current board and reopen its controls explicitly.
Ctrl-R cannot rebase a draft across an archive/restore or same-name replacement.
If an archive/restore outcome is uncertain, inspect the originating board rather
than automatically repeating it. In the TUI, Ctrl-D explicitly releases uncertain
tracking; it does not cancel an operation that may already have committed.

The [web viewer](local-monitor.md#archive-and-restore) has the same Archive,
Archived boards and Restore operations. Agents and scripts use the same broker
through CLI or [MCP lifecycle tools](mcp.md#archive-and-restore):

```sh
agent-bridge board streamr --metadata
agent-bridge board --archive streamr --expected-revision "CURRENT_ACTIVE_REVISION"
agent-bridge board --list --archived
agent-bridge board streamr --archived --metadata
agent-bridge board streamr --archived --raw
agent-bridge board --restore streamr --expected-revision "CURRENT_ARCHIVED_REVISION"
```

Read the exact current revision before either mutation. `--list --archived`
prints JSON metadata, including lifecycle state, `archived_at` and `expires_at`;
`--archived --raw` prints the exact inspected Markdown. Normal agent reads and
updates accept active boards only. Explicit inspection can read an archived board
without restoring it. Concurrent updates and lifecycle changes use revision checks;
a stale request fails instead of overwriting, recreating or targeting another board.

The successful archive persists `archived_at` and `expires_at` as UTC Unix
milliseconds, with `expires_at = archived_at + 30 × 24 hours`. Viewers format the
dates in their local timezone. Reads, listing, restarts and repeated archive requests
do not extend that deadline. Restore before expiry preserves the exact archived
Markdown and its original Last Updated metadata, removes the pending deletion and
returns the board to active lists. Archiving it again starts a new retention window.
A restore durably admitted before expiry can finish through recovery after the
deadline; a new restore at or after the deadline is refused even if cleanup has
not yet physically removed the archive.

The broker recovers archive operations and cleans up expired archives at startup,
then checks every five minutes while running. Both viewers may be closed. Time
offline counts toward retention, and the next startup catches up. Cleanup never
deletes before the persisted deadline; once expiry has been recorded, rolling the
clock back does not make the archive restorable again. Permanent cleanup removes
only broker-managed archived content and its archive-specific metadata. User-managed
external backups are outside this retention job and are neither scanned nor deleted.

A failed cleanup retains a retryable record and name reservation. Inspect
`board_archives` in `agent-bridge status` for the board's state, expiry and
`last_error`; the broker retries on subsequent cleanup passes. `restoring` is
read-only while a committed restore is recovered; `purging` means expiry is final
and cleanup is pending, so Restore is unavailable. Resolve the reported storage
problem rather than manually moving managed files or assuming deletion succeeded.
Archive and restore staging require filesystem creation-time metadata, captured
from the validated file descriptor together with device and inode. Filesystems
that cannot expose it are refused explicitly; ordinary board reads still work.
Cleanup and restore also compare the original file's recorded change time, size
and exact archived bytes. A changed or replaced path is left untouched with a
retryable diagnostic. These checks protect managed paths across offline retention;
they are not an ownership boundary against the operating-system user who owns the
broker state. These are owner storage safeguards, not an agent editing interface;
all agent changes must go through broker board tools.
The name can be reused only after permanent cleanup completes, and receives a new
identity. Old revisions, controls, drafts and named-board subscription requests
cannot target that replacement, even when its Markdown is identical.

Archive invalidates that board's subscriptions and pending notices. Restore does
not resubscribe agents or replay obsolete notices; read the restored board and opt
in again explicitly. Already admitted native notice delivery may finish. Archive
and Restore send no agent messages, grant no approval and do not cancel messages
already accepted by the broker. Exact accepted-message receipt recovery remains
available without sending another message; new sends from archived controls fail.

See the [archive regression evidence](board-archive-regression.md) for controlled
time, recovery, API and actual browser/terminal validation and its limits.

## Copy code

Rendered inline code has a contrasting background and an adjacent overlapping-sheets copy icon.
Click that control once to copy the complete parsed code span; selecting or clicking
the code text keeps ordinary selection and link behavior. Each rendered code block
has the same copy icon at its top. It copies the entire block, including lines
outside the viewport, indentation and its logical trailing newline. Copies omit
Markdown delimiters, language labels, display wrapping and button labels. They never
execute commands or write to the board, its timestamp or subscriptions.

The TUI shows copy icons as `[⧉]`; browser icons have accessible labels and tooltips.
In the TUI, outside an editor or board picker, press `c` to focus the visible code
copy controls. Tab/Shift-Tab cycles them, Enter or Space copies, and Escape leaves
copy focus. Normal Tab navigation still belongs to response fields. Scroll or pan to
reach other controls; resizing, wrapping, board switching and content changes reset
copy focus so old screen coordinates cannot target another snippet. Keyboard copying
also works when `m` has returned mouse handling to your terminal. A drag selection
omits copy-control cells while retaining selected code and prose.

The TUI sends a bounded, write-only OSC 52 clipboard request and reports **Copy sent
to terminal**; it cannot confirm terminal clipboard acceptance. The browser uses its
clipboard write API and briefly changes the copy icon to a checkmark on success,
with an accessible announcement. Denial or unavailable support appears as visible feedback;
manual selection remains available. Neither interface reads the clipboard. Copy
requests over 64 KiB of UTF-8 are rejected rather than truncated. Empty blocks leave
the terminal clipboard unchanged and report that no copy was sent. Raw Markdown and
complex documents shown as a plain-source fallback retain manual copying rather
than presenting parsed-code controls.

## Copy text

Press the left mouse button in the rendered document, drag across the text, and
release to copy the highlighted selection. A click without a drag does not copy
or change your clipboard. Selection copies visible text, including displayed
line breaks and any selected table borders, rather than the raw Markdown source.
It does not edit the document or notify board subscribers.

The displayed document stays fixed during a drag so agents' updates cannot change
what you are selecting. After release, the copied text stays highlighted and
the displayed document stays fixed until you clear or replace the selection.
Click to clear it or drag to select another region. Escape clears a selection;
scrolling, navigation or resizing also clears it and resumes the live view. Mouse
selection applies to the rendered document, not the Responses pane or draft editor. Selections are limited to 64 KiB and are never silently
truncated.

Copy uses the terminal's [OSC 52 clipboard support](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html),
which may need to be allowed in your terminal or terminal multiplexer. The board
sends clipboard data without reading the clipboard, and displays “Copy sent to
terminal” because the protocol does not acknowledge whether it was accepted.

If your terminal blocks clipboard requests, press `m` outside the text editor to
return mouse selection to the terminal and use its normal copy command. Many
terminals also provide Shift-drag to bypass application mouse handling; that
shortcut depends on the terminal.

## Links

Write links as `[label](https://example.com)`. HTTP and HTTPS links display only
their styled label and use OSC 8 terminal hyperlinks. The link's source must fit
on one line, and its label cannot contain another link or image. Other links,
schemes, non-ASCII URLs and URLs longer than 2048 bytes retain the `label (url)`
display.
Terminals without OSC 8 support show just the label for hyperlinks. Set
`NO_TERM_HYPERLINKS=1` for one invocation to disable generated links; `TERM=dumb`
also disables them. Use `agent-bridge board NAME --raw` to inspect or copy exact
Markdown and destinations. Rendering never opens a browser or fetches resources.

Targets are deduplicated and limited to 1,024 distinct destinations. At most
16,384 source label graphemes and 16,384 visible link cells are attributed.
Exceeding a source budget restores the original `label (url)` rendering for the
whole document; exceeding the visible-cell or conservative 512 KiB encoded-output
budget keeps the frame plain and shows a status hint. A board containing raw
terminal controls is sanitized with generated hyperlinks disabled. Raw Markdown
and existing Links sections remain unchanged.
The label can differ from its destination, so check the URL your terminal shows
on hover before opening an unexpected link.

Cmd/Ctrl-click requires the terminal to handle the mouse. If board selection
captures it, press `m` outside the text editor to hand the mouse back to the
terminal. The click shortcut depends on your terminal.

## Answer questions in the terminal

Agents can put standard task checkboxes and marked text fields anywhere in the
document. For example:

```markdown
## Urgent

- [ ] Approve merging PR #25
- [ ] Leave PR #25 open

Any instructions before merging?
<!-- board-input: merge-notes -->
>
<!-- /board-input -->
```

Checkboxes are independent toggles, not a mutually exclusive selector. Give them
clear labels so the user can distinguish questions. Code examples and ordinary
`[ ]` text are not interactive. The Responses pane shows each control's source
line, label and current state; the rest of the board remains rendered Markdown.

Text fields use standalone opening/closing comments and blockquoted response
lines. Choose a unique ID of 1–80 ASCII letters, digits, hyphens, underscores or
periods. Duplicate, nested, unclosed or malformed fields are not editable. Text
inside a field is response content, including anything resembling a checkbox or
marker; it does not create more controls.

In the text editor, type or paste a response. Enter adds a newline; arrow keys,
Home/End, Backspace and Delete edit the text. Ctrl-S saves and Escape cancels the
draft. Page Up/Down scrolls the main board so you can review the surrounding
question. Ctrl-C exits. Saving writes each response line back as `> response`,
preserving the rest of the document. Closing the terminal does not save a draft.

Answers use the same revision-checked broker updates as agents. A board refresh
clears checkbox selection so reordered questions cannot silently receive a click
intended for an older version. If the board changes during text entry, the draft
is retained but cannot save over the newer revision. Review the current board,
then use Ctrl-R to explicitly rebase an unchanged uniquely named field before
saving. If that field changed or disappeared, cancel and reopen the intended
field. An uncertain save is not automatically retried; check the board first.

The agent reads the resulting Markdown and interprets the response under the
user's instructions. Agent Bridge does not execute a checked item or treat it as
authenticated approval: agents can also edit this shared document.

## Subscribe to changes

Notifications are off by default. An enabled local Codex, Claude or OpenCode session can
opt in with the generic `subscribe` MCP tool, selecting its exact registration
and an explicit board selector:

```json
{"agent":"codex-<this-session-UUID>","events":[{"event":"board.changed","board":"streamr"}],"board_revisions":{"streamr":"CURRENT_REVISION_FROM_READ_BOARD"}}
```

Read each named board first and include its current revision in `board_revisions`
for both `subscribe` and selector-specific `unsubscribe`. Missing or stale named
fences fail clearly. `main` may omit this map; unsubscribe-all may omit it too.
The CLI obtains current named-board revisions automatically before subscribing or
unsubscribing. Archive removes the watch, and Restore requires fresh opt-in.

Use the real exact route supplied in the session's native context. `subscribe`
adds selectors idempotently and returns the effective `events`. `unsubscribe`
with the same selectors removes only those watches and their pending notices,
preserving other watches and their work. Removing an absent selector changes
nothing. Omit `events` to remove all subscriptions and pending notices. An explicit empty list is invalid. Each
`board.changed` selector requires an existing local board name, including explicit
`main`; there is no implicit board, wildcard or active-board routing.
Subscriptions end when the session becomes unavailable or the broker restarts;
reopening requires explicit opt-in again. They do not enable networking or share
boards with other machines. Both terminal responses and web-board responses use
the same selected-board revision updates and event delivery. Neither view needs
a subscription control: the receiving agent opts in through CLI or MCP.

The same API supports local `agent.opened` and `agent.closed` selectors. See
[event subscriptions](mcp.md#event-subscriptions) for snapshots and recovery.

The broker permits at most 64 watches globally and 8 concurrent delivery workers.
It polls at most one watched board every 250 ms in fair round-robin order;
unwatched boards add no polling work. Coalescing, pending state and own-write
suppression are independent for each board.

A subscribed agent receives a native change notice naming the board and its latest
revision and can call `read_board` to inspect checkboxes and text responses.
Notifications coalesce after **3 seconds without another change**, with a
**10-second maximum coalescing delay** from the first pending change. Those are
scheduling thresholds; a paused broker, queued work or native transport can delay
receipt. The notice contains no Markdown and is not a normal message requiring
acknowledgement. Failed or uncertain native submissions are not automatically
retried; the board remains available through `read_board`.

OpenCode notices use its durable native admission and receipt path: busy or
permission-waiting work can delay insertion, pause holds delivery, and an
uncertain native attempt is not resubmitted. Unsubscribe prevents pending notices
for the removed selectors from starting native delivery; close, lease expiry or
registration replacement removes the subscription. Broker restart invalidates
pending notices too. Native I/O already admitted before cancellation may finish;
unsubscribe does not cancel that I/O or retained selectors' work. A notice is
local board guidance, never an owner pairing or permission response.

OpenCode subscribe/unsubscribe receipts can report attempted hidden controls in
`recovery`, even after a watch is removed. A current snapshot does not prove the
native delivery queue is unblocked. Inspect the bounded recovery IDs and, when
appropriate, explicitly run `agent-bridge --state-dir "/same/private/state" retry
CONTROL_ID` against the same broker. Attempted OpenCode controls reconcile
read-only; they never insert a second prompt. Missing proof stays uncertain and
may continue blocking later notices. See [event recovery diagnostics](mcp.md#event-subscriptions)
for fields and limits. Reading diagnostics does not start native work.

To avoid being notified of its own writes, an agent supplies its own registered
name in the optional `agent` field of `update_board`. Unattributed updates,
including TUI and web responses, notify all subscribers. A pending
notification from another writer survives subsequent own writes. No-op edits do
not generate notifications. Writer attribution is supplied by a local caller;
it is not a separate authentication boundary between processes of the same OS user.

## Ask the agents to maintain it

For example, the user can instruct their agents:

> Keep our shared board up to date with a Markdown table. Give each agent one row
> containing its current task, status, and any blocker or action needed from me.
> Keep the row short; leave detailed progress in your native session. Update your
> own row when work changes. Preserve other agents' rows and notes.
> Write links as `[label](url)`.
> Use broker board tools only; never edit backing files directly.
> Read the latest board first. On conflict, review the returned snapshot and retry
> only my intended edits while preserving other writers. If tools are unavailable,
> report the blocker rather than writing files through a shell or editor.

Those columns are a user convention, not an application schema. The same tools
can instead maintain a release checklist, meeting notes or a project outline.
The board is user/agent-authored data and never grants tool permission or owner
approval.

## Incremental updates

`read_board` returns `markdown`, `revision`, `name`, `exists` and supported metadata.
Service-generated backing-file paths are omitted; paths written by the user inside
Markdown remain exact document content.
`update_board` requires that exact revision and an ordered array of edits.
`read_board` and `update_board` accept optional `board`, default `main`;
`create_board` requires it. Event subscriptions require explicit board selectors. Named-board revisions bind the board identity
as well as content, including archive/restore generation and same-name replacement,
so an old revision cannot authorize a write to another lifecycle of that board.
Compact write receipts include `board`; conflict snapshots and unchanged reads
include the selected identity. Keep conditional-read revisions scoped by board.
For example, to edit `streamr`:


```json
{
  "board": "streamr",
  "expected_revision": "FULL_REVISION_RETURNED_BY_READ_BOARD",
  "agent": "codex-<this-session-UUID>",
  "edits": [
    {
      "op": "replace",
      "old": "| Agent A | Implementing | No |",
      "new": "| Agent A | Ready for review | Yes |"
    }
  ]
}
```

Available edits:

- `replace`: replace one unique, nonempty literal `old` with `new`.
- `append`: append `text` exactly as supplied.
- `prepend`: prepend `text` exactly as supplied.
- `set`: replace the document with `markdown`, useful for initial setup.

No automatic newline is added. Each batch contains 1–32 edits and applies in order
as one atomic file replacement. If any edit fails or the result exceeds 64 KiB of
UTF-8, nothing is changed. Ambiguous or missing replacement text is rejected;
select a complete unique row or section instead of a common word.

Concurrent agents cannot silently overwrite changes made through the broker.
A stale revision returns an MCP error with structured `conflict: true`,
`applied: false` and the current board snapshot. Review that snapshot and retry
only the intended edits with its revision. Do not replace the whole board using
an old copy. After an ambiguous transport failure, read the current document
before retrying an append or another edit.

## Local scope

Named boards remain local to this broker's selected state directory. They do not
synchronize files or grant board access to paired machines. CLI, terminal, MCP
and browser views share storage and revision-checked updates without a required
content schema.

Agents must not edit backing files directly: those writes bypass revision checks,
lifecycle state and coordinated notifications. Use the broker tools and review the
latest returned snapshot after conflicts. If tools are unavailable, report the
blocker instead of using filesystem writes. Owner backup and storage administration
remain separate operations, not an alternative agent mutation interface. The TUI
edits only the selected checkbox or marked text field through the broker. Terminal
control sequences are removed for display while document content remains exact.
Links, HTML and images do not execute commands or fetch network resources.

## Broker compatibility

Legacy clients keep using `main` against a new broker. Named-board operations use
distinct protocol requests: an older broker rejects them before changing any
board. Upgrade the broker and reconnect/restart the MCP client
if named-board tools are unavailable or rejected. Main-only read/update calls retain their
legacy request forms where supported. Event subscriptions use only the generic
`subscribe` and `unsubscribe` operations; old board-specific operations are removed. Boards remain local to the selected broker;
this adds no network access or cross-machine sharing.

For an isolated terminal smoke test, start a broker with a fresh temporary
`--state-dir`, create `alpha` and `beta`, and open `alpha`. Add a checkbox and text
field through MCP; open a draft and verify `b` types into it. Cancel the editor,
use `b` to select `beta`, then return to `alpha` and verify its controls and content.
Exercise save/conflict/rebase with concurrent MCP edits, confirm failed saves
preserve the draft, and check that switching never writes to the other board.
Also check clipboard selection, refresh/reconnect and the selected-name display.

## Reusable message composers

Use MCP `add_board_message_input(agent, board, expected_revision, id, label)` after
`read_board`. `agent` must be an exact enabled local registration, not a native
display name. The tool pins its name, provider, session and endpoint and appends a
composer using the same revision checks and own-write notice suppression as
`update_board`. IDs are 1–80 ASCII letters, digits, dots, underscores or hyphens
and must be unique across message composers and response fields on that board.
Labels cannot contain control characters or HTML comment delimiters.

The CLI equivalent is:

```sh
agent-bridge add-board-message-input coordinator --board main \
  --expected-revision REVISION --id coordinator-message --label 'Message coordinator'
```

The supported Markdown syntax is a standalone HTML comment with strict JSON:

```md
<!-- board-message: {"id":"coordinator-message","instance":"123e4567-e89b-42d3-a456-426614174001","label":"Message coordinator","owner":{"name":"coordinator","provider":"codex","session":"123e4567-e89b-12d3-a456-426614174000","endpoint":""}} -->
```

Valid declarations appear as message composers rather than JSON text in the
rendered board. Raw Markdown remains byte-exact. Malformed declarations and
literal code examples remain visible and never become active inputs.

Code examples, nested comments and response bodies do not declare composers.
Malformed or duplicated declarations fail closed. Use the authoring tool to
capture the current registration instead of copying identity fields manually.
Each authored composer gets a fresh required canonical UUID `instance`; reusing
the same user-facing ID after removal creates a new identity and requires review
of an old draft. Exact raw restoration, including the same instance, restores the
same composer identity.
Markdown and labels remain untrusted content: neither proves user approval,
authenticates an agent, grants permissions or automatically executes anything.

An explicit user send uses the broker's normal durable user-to-agent queue and
message history. It preserves the submitted text exactly and does not change
Markdown, its revision, board update time or board subscriptions. Acceptance means
queued, not read, acknowledged or completed. No conversation is started or resumed.
A closed, disabled or replaced target is unavailable; no alias or replacement
session is substituted. Concurrent board changes require review before a new send.

Both views submit the same board name, exact revision, control hash, UUID and text.
Retry an ambiguous submission with that exact UUID and payload: a durable accepted
receipt is returned even after the board or owner changes, without enqueueing
again. Reusing a UUID with another board, revision, control or text is rejected;
a UUID already used by an ordinary message cannot be adopted. A new deliberate
send uses a new UUID. Queue rejection and errors retain the draft.

Both interfaces keep compact message boxes stacked at the bottom, below the
independently scrollable board. Each box shows the pinned agent's friendly name,
an editable input and Send. Click an input or Tab/Shift-Tab into it to type directly;
there is no activation step. A bounded stack scrolls when many inputs are present,
and keyboard focus brings the selected input into view. Errors and pending or stale
targets appear only when relevant; ordinary inputs reserve no feedback rows.

In the terminal, Tab/Shift-Tab focuses a message box directly.
While composing, **Enter sends** and **Shift-Enter inserts a newline**. Legacy
terminals may encode Shift-Enter as ordinary Enter; use **Ctrl-J for newline**
there. Page Up/Down still scrolls the board. Mouse wheel over the board scrolls
the document; wheel over the input stack scrolls the boxes. Ctrl-O opens exact
provider, session and endpoint details in the board
area; arrows or Page Up/Down scroll them, and Escape/Ctrl-O returns to the
unchanged draft. The original marker is also available in
`board NAME --raw` for inspection.

Tab/Shift-Tab moves between inputs and retains independent message drafts. Escape
leaves the composer while retaining its draft; Enter reopens the selected input.
Ctrl-E outside an editor cycles through retained drafts, even if their inputs were removed,
so its original target and text remain inspectable. Ctrl-X explicitly discards the
active message draft. Send or discard retained
drafts before changing boards or quitting; outside an editor, Ctrl-D explicitly
discards all retained message drafts. Continuing to edit cancels navigation.
Refreshing never discards drafts. After a board change, Ctrl-R reviews an input
only when its ID, label and pinned owner are unchanged; Enter remains an explicit
send action. A changed or removed input cannot receive the stale draft.

Successful acceptance clears only that exact draft and keeps the composer focused
for another message. Pending submissions ignore repeated Enter and lock editing.
An uncertain result keeps both text and UUID locked: Enter retries the exact same
submission; Ctrl-X discards explicitly. Definite errors retain an editable draft.
Text responses keep their existing Enter-newline/Ctrl-S-save behavior. Normal
quit is keyboard driven; killing a terminal/process cannot guarantee draft recovery.
