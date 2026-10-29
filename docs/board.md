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
```

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

The broker must be running. Agents use the existing persistent MCP server and its
`read_board` and `update_board` tools. There is no extra server or binary.

The existing [local web monitor](local-monitor.md) also has a Boards view. It
lists named boards, renders their Markdown and lets you answer the same existing
checkbox and marked text controls. Browser selection is local to that tab; it
does not change terminal or agent selection. Creation and general Markdown edits
remain CLI/MCP operations. Browser drafts stay in memory and use explicit
save/discard/cancel choices, revision checks and current-board review on conflicts.

For a custom broker state directory:

```sh
agent-bridge --state-dir "/absolute/private/state" board
```

Documents are stored as `boards/<name>.md` inside that private state directory.
Names are 1–64 lowercase ASCII characters matching
`[a-z0-9][a-z0-9_-]{0,63}`; paths, dots and silent normalization are rejected.
Use `--create NAME` (or MCP `create_board`) explicitly before using a non-main
board. Creation works without a terminal and never overwrites an existing board.
Unknown names fail without creating a file or falling back to main.
`--list` and MCP `list_boards` return deterministic name-ordered metadata
(name, existence and revision), without Markdown. They include absent `main`.
There are at most 256 boards, including lazy `main`; listing scans at most
1,024 directory entries and errors explicitly on excess instead of truncating.
Existing `boards/main.md` stays in place; no migration is needed. A read of absent
`main` returns empty Markdown and its revision;
the first successful update creates it. Backups of the broker's state include the
board. Print its exact Markdown once with `agent-bridge board --raw`.

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
{"agent":"codex-<this-session-UUID>","events":[{"event":"board.changed","board":"streamr"}]}
```

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
including TUI responses and direct file edits, notify all subscribers. A pending
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
> Read the latest board first, and retry conflicts against the returned revision.

Those columns are a user convention, not an application schema. The same tools
can instead maintain a release checklist, meeting notes or a project outline.
The board is user/agent-authored data and never grants tool permission or owner
approval.

## Incremental updates

`read_board` returns `markdown`, `revision`, `name`, `path` and `exists`.
`update_board` requires that exact revision and an ordered array of edits.
`read_board` and `update_board` accept optional `board`, default `main`;
`create_board` requires it. Event subscriptions require explicit board selectors. Named-board revisions bind the board identity
as well as content, so a revision from another board cannot authorize a write.
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

Manual file edits are possible, but external editors do not participate in the
broker's revision transaction. Coordinate manual changes with active agents to
avoid racing a file save against an MCP update. The TUI edits only the selected
checkbox or marked text field. Terminal control sequences are removed for display,
while the Markdown file
retains its original text. Links, HTML and images do not execute commands or fetch
network resources.

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
