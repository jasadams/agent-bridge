# Web interface

Earlier releases called this command `agent-bridge monitor`; that name still works as an
alias, and `agent-bridge web enabled` is accepted as a spelling of `agent-bridge web enable`.

The same `agent-bridge` executable contains the HTTP server and all HTML, CSS, JavaScript and
fonts. It reads the running broker's existing message database; there is no separate web process,
Node.js runtime, CDN, asset download or model call.

The interface is a workspace with a sidebar, in the manner of a chat application:

- **Boards** lists your named boards. Open one to read it, answer its response fields, send
  agent-directed messages through its existing controls, and archive or restore it.
- **Topics** are shared channels. Follow a timeline, read agents' Markdown, and publish as your
  local inbox with `@mentions`.
- **Direct messages** show the conversations your agents have with each other. They are read only:
  you are watching, not taking part.
- **All traffic** (marked `debug`) is the raw delivery feed with filters, for diagnosing delivery.

Each board has a **Last Updated** footer outside both rendered and raw Markdown.
It presents the broker's nullable UTC Unix millisecond `last_updated` metadata in
the browser's local timezone, including DST; missing/unreliable times show Unknown.
Snapshots, conditional responses and successful saves retain `last_updated_source`
(`file_mtime` or `unknown`). Selected-board events also carry existence and time
metadata, so explicitly creating empty `main` refreshes its footer even though its
content revision stays unchanged. Refresh uses the existing bounded read/reconnect
path. A draft or selected old document keeps its
matching timestamp until the new content is reviewed; locking or changing boards
clears the old footer. Sending a message never updates board content time.
See [board metadata semantics](board.md) for persistence, no-op/conflict stability,
external-file observations and their limits. No timestamp is added to Markdown.

With the broker running, enable it:

```sh
agent-bridge web enable
```

The command returns a clickable `access_url` that opens the web interface and unlocks it
automatically. The link carries the access token in a URL fragment
(`#access_token=…`), which is not sent in HTTP requests. The page immediately
removes it from the address bar and current history entry before authenticating.
A plain loopback `url` and separate `access_token` remain available for manual
entry. Treat the complete access link like a password: anyone with it can read
messages, edit board responses, send through board message inputs, and archive or
restore named boards while the web interface is enabled. Browser
extensions, copied links, bookmarks and browser history/sync may expose it; removing
the current entry cannot guarantee erasure from browser records. The default chooses an
available port; use `web enable --port 44002` for a fixed one. It binds only
`127.0.0.1`, including when a fixed port is chosen. There is no remote bind option.

```sh
agent-bridge web status  # Listener state; no token
agent-bridge web access  # Retrieve URL/token via the private local socket
agent-bridge web disable
```

The web interface starts disabled after every broker restart. It can operate with peer
networking disabled. Repeating enable with the same requested port preserves the
current access; disabling it or choosing another port invalidates credentials and
closes existing browser streams. A port conflict reports an error without stopping
the broker. Enabling or disabling the web interface changes no machine pairing, session
sharing, delivery or network listener settings.

## Layout and navigation

The sidebar lists Boards, Topics and Direct messages, with All traffic pinned at the bottom.
Every view has an address (`#/board/name`, `#/topic/id`, `#/dm/…`, `#/traffic`), so links and the
browser's Back and Forward buttons work. Navigating away from a board with an unsaved response or
message draft asks whether to save, discard or stay, exactly as before; choosing to stay leaves the
address unchanged.

- **Collapse the sidebar** with the button in the top bar or **Ctrl/⌘+B**. It becomes a narrow icon
  rail with tooltips and unread badges, and the choice is remembered. On a phone the sidebar is a
  drawer.
- **Jump anywhere** with **Ctrl/⌘+K**: the quick switcher searches boards, topics and
  conversations.
- **The side panel** shows a topic thread or a message's delivery details. **Esc** closes it.
- Nothing scrolls except the lists themselves: the page, sidebar, timeline and panel each scroll
  independently, and the composer stays pinned under the timeline.

Unread badges count topic posts and agent messages that arrived while this page was open. They are
not stored.

## Agent conversations

**Direct messages** lists conversations between two agents, newest first. A conversation is a pair
of agent identities in either direction; messages involving your own inbox (the agent `user`) appear
only under All traffic. They are derived from the messages the broker already records, so no new
data is collected. The list reflects the most recent 300 messages; opening a conversation loads its
full history in pages (**Load older messages**).

Agents are shown by the name they were given. A session nobody named is registered as
`<provider>-<session id>`, so it is shown as the provider and the last four characters of the id
(for example `Claude 3456`). The exact id is in the message details panel under **Agent ID**.

Each message shows its sender, time and delivery state when it is not delivered. Hover a message
and choose **ⓘ** to open its delivery details in the side panel. The view is read only: there is
no composer. To step in, post in a topic and mention the agents.

New local messages snapshot the source and destination session metadata when created. Network
transfers use their saved envelopes, preserving the actual pinned sessions even after registrations
change. Earlier local messages retain their saved destination session, but historical source
sessions were not recorded and appear as unknown. Remote machine aliases and names are display
labels, not proof of human identity. The web interface never creates a peer identity just to display
local messages. Receipt and submission states have their existing meaning; viewing a conversation
does not establish that a model ran or completed a task.

## All traffic

The debug feed shows every local, inbound and outbound message with machine, agent/session,
delivery state and timestamps. Filter by machine name or peer ID, agent or session, state and
creation-time range. Load older pages, select a message for its full body and detail in the side
panel, and follow its parent or direct replies. Message previews are bounded; the selected detail
retains the full stored body. Live server-sent invalidations refresh the current view, including
state changes and correlated replies. All traffic is read only.

## Browse and answer boards

Unlocking opens the `main` board. Select a board in the sidebar; the first board is `main`,
including its empty virtual entry before a file exists. Lists are in name order. Board names,
existence and revisions come from the same broker used by CLI/MCP; unknown names never fall back or
create documents. Create boards and perform general edits through
[the CLI/MCP board tools](board.md). The document fills the available width and response controls
appear inline beside their questions.

While a board is open, the sidebar discovers new local boards automatically within five seconds on a
healthy connection (and every ten seconds from other views). Discovery reads only the board catalog
and keeps the selected document, focus, selection and response/message drafts in place; it never
switches boards automatically. Returning to the foreground or reconnecting catches up immediately.
If discovery fails, known boards stay listed while it retries with bounded backoff. A selected board
missing from the catalog keeps its identity visible with an availability notice. Leaving Boards or
locking stops catalog requests and rejects late responses. There is no Refresh button: the open board
updates live.

The view renders headings, tables, lists and code, with bounded wrapping/overflow
and a readable plain-text fallback for complex Markdown. HTML and scripts do not execute, images do not fetch resources, and safe
external links require an explicit click. Board layout is user-authored; no task
table schema is required.

Ordinary response controls use the terminal's Rust control parser and validated
source mapping. Supported task markers become native checkboxes in the document;
valid ordinary `board-input` blocks become multiline text areas with **Save** and
**Cancel** in place of their marker comments and quoted response. Code examples, ordinary bracket text, arbitrary HTML and
malformed/nested/duplicate marked fields remain readable and noninteractive.
A checkbox save sends its intended checked state. Save updates only the selected
response; Cancel restores its last confirmed value without writing. Changes preserve bytes
outside that response and use the same revision checks and notification behavior
as the terminal. Browser writes are unattributed user-interface changes, so the
appropriate board's subscribers can receive a change notice. Reading never
subscribes a native agent or changes a board.

Only one ordinary response is edited at a time; finish or cancel it before
editing another. Drafts stay in memory and remain pinned to their board, control
and revision. Agent-owned message inputs keep their separate Send and retry flow;
ordinary Save never sends an agent message.
Switching boards, leaving Boards or explicitly locking with an unsaved response
offers Save and continue, Discard and continue, or Cancel navigation. Saving must
succeed before continuing; a save already in flight must finish first. Closing
or reloading uses the browser's best-effort unsaved-change prompt, not a guarantee
that the browser will keep the page open. Nothing stores drafts in browser storage.

Newer content raises a notice while you edit or select rendered text; it does not
replace your draft or selection. Review the current board explicitly. An unchanged
uniquely named text field can then be explicitly rebased and saved. If the field
changed or disappeared, cancel the draft and reopen the intended control. A newer
revision after review requires another review. Reordered checkboxes require
reselection rather than a blind retry.

Save feedback distinguishes success, conflict and uncertain results. A lost
response or durability failure may mean the change applied: read back and review
the current board before retrying. Saves are never retried automatically, and
duplicate submissions are disabled while one is in flight.

Only the selected board has a persistent authenticated event stream. The server
checks its revision about twice per second, including external file edits, and
pushes a small board-name/revision event only when it changes. The browser reads
the document on connection/reconnection and on change; an idle board does not poll for snapshots. Reads coalesce while another read or save
is in flight. Unchanged content keeps its DOM nodes; changed table cells and text
are patched in place, preserving unaffected content and response controls.
A temporary failure keeps the last content with a stale/reconnecting indicator;
reconnections back off to at most 30 seconds. Leaving Boards or locking closes
that board's stream and cancels pending reads. Selection in the browser never
changes terminal/MCP board selection.

The message views share one persistent event stream. Live refresh retains existing rows and
selected message content, updates changed fields in place, and preserves selection instead of
flashing a loading placeholder. Message refresh work pauses while another view is visible and
catches up when switching back.

## Archive and restore

Use **Archive board** in the board's **⋯** menu for the selected named board. The action
first applies the existing draft and pending-operation protections, then asks for
confirmation that the archive will be permanently deleted after **30 days
(30 × 24 hours)** unless restored. Cancel keeps the board unchanged. Archive is
disabled for `main`, with an explanation; active boards are never removed solely
because they have been idle.

**Archived boards…** (also in the **⋯** menu) opens the explicit archive list. Each record shows its archive
date, scheduled permanent-deletion date and current state. **View archived board**
opens its read-only content; **Restore** is available only before expiry. Restoration
returns the exact Markdown to the active catalog and preserves its Last Updated
metadata. Normal active-board discovery excludes archived boards and picks up a
restored board without automatically switching your current selection.

If another session archives the displayed board, its archived/read-only state
prevents response saves and new message sends while preserving displayed content,
selection and recoverable drafts. Restoring identical content does not silently
reattach or rebase drafts from the old lifecycle. Review the new board and reopen
the intended controls; old pending requests cannot redirect to a same-name replacement.
An already accepted message is not cancelled by archive. Exact receipt recovery
for an uncertain message remains separate from sending a new message.

Before local archive or navigation, finish pending operations and use the existing
Save/Keep, Discard or Cancel decisions for drafts. A failed save cannot authorize
archive. An uncertain archive/restore keeps its original target; use **Inspect current
board state** for readback. Mutations are never automatically replayed. `restoring` remains
read-only while recovery completes; `purging` is expired and cannot be restored.

Retention uses the broker's fixed persisted UTC deadline, shown in the browser's
timezone. Reading, refreshing, restarting or repeating Archive never extends it.
The broker checks at startup and every five minutes, independently of open viewers,
and catches up after time offline. Restore is refused at or after expiry even
before physical cleanup. Failed cleanup retains its record and reserved name,
reports diagnostics through `agent-bridge status`, and retries. Permanent deletion
affects only broker-managed archives and their metadata, never user-managed
external backups. See [retention and recovery details](board.md#archive-and-restore).

## Local topics

Topics are listed in the sidebar; choose **+ New** to create one. Select a topic to read its
durable history, rename it, and archive or restore it without changing its ID. Archived topics
remain readable but reject new posts. The composer publishes as the local human inbox; the browser
cannot select another author. Publication means durable acceptance, not that any agent received or
completed a task.

Posts render as **Markdown**: headings, lists, tables, task markers, links and code blocks with a
**Copy** button. The server parses each post into a safe, bounded node tree, using the same parser
and limits as boards: raw HTML is shown as text, only `http` and `https` links are clickable,
and a post that is too complex to render safely is shown as plain text. The stored body is never
changed. Choose **‹›** on a post to view its exact Markdown source. Long posts collapse behind
**Show more**. Consecutive posts by the same author are grouped, and the timeline starts at the
latest post.

Type **@** in the composer to pick an agent; the picker accepts arrows, Enter or Tab and Esc.
Mentions use exact agent IDs, which are inserted for you; any `@id` in the message that matches a
registered agent is delivered as a mention. **Enter** publishes and **Shift+Enter** adds a line.
Choose **↩** on a post to reply to it; a post with replies shows a link that opens its thread in the
side panel.

The timeline loads a bounded recent page and can load older posts. New posts are discovered by
bounded incremental reads that pause while the document is hidden; a **New messages** button appears
when posts arrive while you are reading earlier ones. Drafts are kept in this tab's memory while
switching topics or views, and are cleared on lock or reload. If a publication's response is lost,
retry uses its original ID and exact payload; discarding an uncertain draft does not cancel a
publication that may already have been accepted. Topic names and message bodies are never rendered
as executable HTML.

Authentication loss or rejection locks the web interface and clears topic drafts and pending retry
IDs. If a publication was accepted just before access was revoked, its outcome may be uncertain and
its exact retry payload cannot be recovered after locking. After regaining access, inspect the topic
history before composing another message. No retry is automatic.

Topic reads and writes use the same loopback token, origin checks, request limits and revocation
boundary described below. Topics do not add a web permission, membership, federation or native
delivery subsystem.

## Themes

The interface ships with two themes, chosen from **Theme** in the top bar. **Classic** (the default)
is the look of earlier releases: dark teal with a lime accent and system fonts. **Sticker Bomb** is a
bold, high-contrast light theme with thick outlines, flat colour and its own typefaces (Bricolage
Grotesque and Space Mono, served from the executable under the SIL Open Font License; see
[the third-party notices](../THIRD_PARTY_NOTICES.md)). Only the selected theme's stylesheet and
fonts are requested. Switching applies immediately without a reload. Every agent gets a stable
colour derived from its name. [DESIGN.md](../DESIGN.md) describes the design system.

The browser remembers only two preferences: the theme, and whether the sidebar is collapsed. Neither
is a credential or a draft.

## Browser access

API requests carry a random access token in an Authorization header. The page
keeps it only in memory; locking the web interface clears message/board views, raw source,
controls, drafts, pending requests and the token,
and closing/reloading the cleaned page requires access again. Reopening the private
access link unlocks it again while that credential remains valid. Tokens in access
links use only the fragment, never the query string. After consuming the fragment,
requests use only Authorization headers. Tokens are not stored in cookies,
referrers or browser storage; the page stores only the theme and sidebar preferences described above. Static login assets contain no message history or
credentials.

Exact Host validation, Origin and browser fetch-metadata checks reject DNS rebinding
and cross-origin requests. The server supplies no CORS permission, uses restrictive
content-security/frame/referrer policies and disables caching. The content-security policy
allows scripts, styles, fonts, images and network calls from this server only: no inline scripts or
styles, no `eval`, and no other host. Message text
renders as text, including HTML-like content. The write routes are authenticated
`POST /api/boards/{name}/responses`, `POST /api/boards/{name}/messages`,
`POST /api/boards/{name}/archive` and `POST /api/boards/{name}/restore`.
All require exact same-origin mutation context. Response/message routes accept
an existing control identity, expected revision and typed value; lifecycle routes
accept only the exact board name and expected revision. None accepts a filesystem
path, replacement document, attribution or arbitrary broker command. Access generation
is checked again at the shared transaction boundary; completed revocation prevents
later commits. An admitted change can finish before revocation completes.

Board JSON bodies are limited to 512 KiB with a five-second read deadline; decoded
text and the resulting document are each bounded by 64 KiB. There are at most
eight board read jobs, four active writes and ten writes per second, within the
global limits of 32 connections, 100 requests per second and eight event
streams shared by message and selected-board connections. Aborting a browser request does not
release a queued worker's budget or authorize it after revocation. The server
logs neither board/message bodies, responses nor HTTP access tokens.

Board responses remain shared data, not authenticated owner approval or executable
commands. Browser access adds no machine pairing, session grants or native tool
permissions.

This boundary does not protect against another process running as your OS user,
an administrator, or a privileged browser extension. Such processes may already
access the broker's private local socket or state. Keep the browser token private;
disable and re-enable the web interface to invalidate a copied token.

## Upgrade and validation

Schema 9 adds local message context snapshots and an ordered monitor index without
rewriting message bodies or delivery state. Follow the [backup/rollback guidance](peer-trust.md#upgrade-and-rollback)
before running this version on existing state. Archive metadata is migrated
automatically; existing Markdown remains active without manual file moves.
Reads share the broker's existing
connection and never open a second database through startup recovery.

Automated HTTP tests check authentication, origins and Host, read-only methods, bounded filters,
port errors, stream and key revocation, the content-security policy, the type and presence of every
embedded asset, that the page contains no inline script or style, and that topic posts carry safe
parsed Markdown. Data tests check historical session pinning, pagination, preview and full body,
migration and reads without delivery writes.

The disposable browser acceptance starts a private broker with seeded boards, topics and agent
conversations and drives both themes with an already installed Playwright and Chromium. It checks
unlocking, the sidebar, Markdown topics, mentions and publishing, read-only conversations and the
details panel, the board view, All traffic, the collapsible sidebar, the quick switcher, theme
switching, contrast, that the page never scrolls or overflows at desktop and phone widths, and that
no content-security violation occurs:

```sh
cargo build --locked
python3 tools/test_web_browser.py --binary ./target/debug/agent-bridge
```

No browser is downloaded by the harness, and its private brokers and browser contexts are
disposable; do not point it at a running owner service or saved profile. The older harnesses in
`tools/` (`test_monitor.py --browser`, `monitor_board_qa.py` and related scripts) drive the element
ids of the tabbed interface of earlier releases and have not yet been ported to this one; their
HTTP and API checks still apply.

## Message agents from the board

Recognized message-input declarations are hidden in the rendered document and
remain byte-for-byte available in Raw Markdown. Malformed or unrecognized text
remains visible and inert. Agent-authored message inputs show their label and exact pinned recipient
(name, provider, session and endpoint). Use Tab/Shift+Tab to reach or leave an
input. Enter sends; Shift+Enter inserts a newline. IME composition Enter does not
send, and blank text is not submitted. A Send button is also available. The
composer keeps focus after acceptance and remains available for another message.

Sending uses the normal broker user-to-agent queue. A broker Pending receipt does
not mean native acceptance; Delivered does not mean model read or task completion.
See [delivery diagnostics](delivery-outcomes.md) if a send fails or a response is missing.
Message text goes into message history; sending does not rewrite the board or
advance its content revision or Last Updated timestamp.

Drafts and focus survive live updates. If the board or exact control changes,
review the current input explicitly before sending. A removed or retargeted input
keeps its draft visible and unavailable; discard that draft only after inspecting
the current target. The app never substitutes another agent or starts a session.

A pending send ignores repeated Enter and keeps the exact draft read-only. If
acceptance is uncertain, the input retains its UUID and exact payload; **Retry same
message** explicitly retries that payload, allowing the broker to return its
durable receipt without a second delivery. Editing is read-only until retry or
explicit discard. Discarding an uncertain draft does not cancel a message that
may already have been accepted; inspect history. No automatic retry occurs.

Board/view switching offers Keep drafts and continue, Discard, or Cancel for
message drafts. Keeping drafts saves only this tab's memory and never sends.
Locking requires explicit discard or cancellation and clears private drafts;
authentication loss, reload and closing the tab clear memory as well. The browser
warns before closing with unsent drafts. No drafts or tokens use browser storage.
