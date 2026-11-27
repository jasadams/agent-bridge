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
content revision stays unchanged. The page has no Refresh button: it reads the board again when
that event arrives and, after its live connection drops, through the existing bounded reconnect
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

Agents are shown by name. The broker resolves each agent's name when it answers a request (Claude's session
name, a Codex thread name or an OpenCode title, else a generated name such as `Claude 9f3c`) and sends it with
every message as `source_agent` / `destination_agent`, and with topic posts and the mention list as
`author_agent` / `agent` ({id, name, provider, machine}). The page only displays these references; it never
builds a name from an ID and no longer polls for names. The exact routing ID is in the message details panel
under **Agent ID** and in a hover title on topic authors. A renamed session appears under its new name the next
time its messages are loaded; a topic post already on screen keeps the name it was loaded with. Two sessions may share a name; the details panel tells them apart by ID. Topic
posts keep the name the author had when posting (see `docs/naming.md`). Names are labels only and never
choose a destination.

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

The interface ships with five theme choices, chosen from **Theme** in the top bar. **Classic** (the default)
is the look of earlier releases: dark teal with a lime accent and system fonts. **Sticker Bomb** is a
bold, high-contrast light theme with thick outlines, flat colour and its own typefaces (Bricolage
Grotesque and Space Mono, served from the executable under the SIL Open Font License; see
[the third-party notices](../THIRD_PARTY_NOTICES.md)). The three **Orca** choices are for a browser tab
beside the agent chats in the Orca application: **Orca Dark** and **Orca Light** match Orca's own dark and
light appearance (its neutral greys and borders, corner radii, 36px header and the Geist typeface, also
served from the executable under the SIL Open Font License), and **Orca (system)** uses one or the other
according to the browser's appearance (`prefers-color-scheme`), switching live, without a reload, when
that appearance changes. Only the selected theme's stylesheet and fonts are requested (the system choice
requests only the Orca sheet it currently needs), and exactly one theme stylesheet is in the page. Switching
applies immediately without a reload. If a sheet fails to load, the page goes back to the last choice that is applied (so a failed swap while following the appearance stays on Orca (system)). Every agent gets a stable colour derived from its name.
[DESIGN.md](../DESIGN.md) describes the design system and lists the Orca values it took from Orca.

The browser remembers only two preferences: the theme, and whether the sidebar is collapsed. Neither
is a credential or a draft. For Orca (system) the remembered theme is `orca-system` itself, not the
appearance it resolved to, so the page keeps following the appearance on every visit.

### Orca (system) and the browser inside Orca

The page cannot read Orca's setting; it only sees what the browser reports. What was found:

- **Verified:** with Orca's own Appearance setting on System and macOS in its Light appearance, a page in an Orca browser tab (Orca's Electron 43.8.10) reports `prefers-color-scheme: light`.
- **Not verified:** whether the tab mirrors Orca's own setting or only macOS. Orca's main process sets Electron's `nativeTheme.themeSource` from its Theme setting (System, Light or Dark; read in the application's `out/main/index.js`), so a tab is expected to follow Orca's own setting, but this was not tested with Orca set explicitly to Dark or Light, and with System and a Light macOS both explanations predict light. Orca can also override the colour scheme of a single site in a browser tab, which would apply to this page like any other. Orca (system) follows whatever the browser reports, so it behaves correctly either way; both appearances and a live change are covered by emulation in the browser test.

### Choosing the theme from the link

Put `theme=<name>` in the address fragment (`classic`, `sticker`, `orca`, `orca-light` or `orca-system`) and the page opens in that
theme from its first paint, with no click and no flash of another theme. The blocking `theme.js`
applies it before the page is drawn and requests only that theme's stylesheet and fonts. For the
plain address:

```text
http://127.0.0.1:<port>/#theme=orca
```

For the private access link from `agent-bridge web enable` (`<url>#access_token=<token>`), add the
theme to the same fragment, joined with `&`:

```text
http://127.0.0.1:<port>/#theme=orca&access_token=<token>
```

The page still unlocks from the link. The theme travels in the fragment like the token, so it is never
sent in a request, and the token is consumed and removed exactly as without a theme.

- A theme in the link takes the same effect as choosing it from **Theme**: it is remembered as the
  saved theme (the page still stores only the theme name and the sidebar state).
- Only one known, lowercase name counts (after percent-decoding). An unknown, empty, differently cased or repeated
  `theme` is ignored: the saved theme (or Classic) applies, nothing is stored, and the address is left
  as it was (unless the fragment also names `access_token`, which is always cleared; see
  [Browser access](#browser-access)).
- After applying it the page removes `theme` from the address, so a later reload keeps whatever you
  chose from the menu in the meantime. Opening a link that carries a theme (a new tab, a bookmark, a
  pasted link in a fresh load) selects that theme again; changing only the fragment of a page that is
  already open does not reload it and does not apply a theme.
- Supported forms without the token: `#theme=<name>`, and a route with a theme (`#/board/main&theme=orca`
  or `#theme=orca&/board/main`). The page removes only the `theme=…` part of the fragment, as text; every
  other character, the route included, stays exactly as typed, so the route opens as it would without a
  theme. With only a theme, the address is empty after it is removed, so once unlocked the page opens the
  default board, exactly as a reload of the plain address does. The access link also carries a route; see
  below.

### Opening a board from the access link

A coordinator that opens the board for the owner in a browser tab (one per project, for example a `kyomi`
board and a `krausen` board) puts the existing board route, the theme and the access token in the one
fragment. Nothing is new: the route is `#/board/<name>` exactly as in a link without a token, and the
`&`-separated parts may come in any order. To build it, take the link `agent-bridge web enable` printed
(`<url>#access_token=<token>`), and add the route and, if wanted, the theme as further `&`-separated parts
of the fragment. Percent-encode the board name as `encodeURIComponent` does (a `&` or `/` in a name would
otherwise end or split the part):

```text
http://127.0.0.1:<port>/#/board/kyomi&theme=orca&access_token=<token>
http://127.0.0.1:<port>/#theme=orca&access_token=<token>&/board/kyomi
http://127.0.0.1:<port>/#/board/kyomi&access_token=<token>
```

All orders of route, theme and token work, with or without the theme. After the page unlocks it opens
the named board in the chosen theme, with no click.

- **Nothing of the link is written back.** The whole fragment is cleared from the address and the history
  entry exactly as for any access link (the token, the theme and the board part all go). The board's name
  is held in memory only. After unlocking, the page opens it only if its own board list holds exactly that
  name, and then the address shows the page's own text for it, `#/board/kyomi`. A reload shows the locked
  screen with that address (no token, no theme), so entering the token opens the same board.
- Only a board that exists is opened. A board the page does not list, a name the page cannot read, more
  than one board part (even the same one twice) or none opens the default board with a clean address. This
  is deliberate and for security: only names the page itself lists are ever shown back, so a crafted link
  cannot put text of its own, or a piece of the token, into the address, the title or the page. It differs
  from a link without a token, where a board that does not exist shows the board error.
- Only a board route is used (not a topic or a conversation). A board whose name contains the text
  `access_token` cannot be opened by a link, as described under [Browser access](#browser-access).
- The board list is read once after the token is accepted, before the workspace is shown. If it cannot
  be read, the page stays locked with its usual "could not be reached" message and keeps nothing of the link.
- A malformed or repeated theme or token part is handled as described above and under
  [Browser access](#browser-access). A link that stays locked (an unreadable shape, a malformed token, or a
  well-formed token the broker refuses) is cleared completely and keeps no board.

## Browser access

API requests carry a random access token in an Authorization header. The page
keeps it only in memory; locking the web interface clears message/board views, raw source,
controls, drafts, pending requests and the token,
and closing/reloading the cleaned page requires access again. Reopening the private
access link unlocks it again while that credential remains valid. Tokens in access
links use only the fragment, never the query string. After consuming the fragment,
requests use only Authorization headers.

The page removes the token from the address bar and the current history entry for every link
shape, including one it cannot read. It unlocks only when the first `access_token` parameter of the
fragment, after splitting on `&` (a leading `?` and percent-encoding of the key or value are
accepted), holds a valid token; this covers `#access_token=<token>` and, with a theme,
`#theme=<name>&access_token=<token>` in either order, each with a board beside them. A fragment that contains the text
`access_token` in any other shape, for example joined with `;`, `,` or a space, after an `&?`,
inside a route (`#/x?access_token=<token>`), as another parameter's value, in upper case or encoded
more than once, or as a repeated key whose first value is not the token, does not unlock: the page
shows the normal locked screen, replaces the whole fragment with an empty one (a route in it is
lost too) and stores nothing. It sends nothing either, except in one case: a repeated `access_token`
key whose first value is a well-formed token is tried as if typed and refused, and the address,
storage and every URL still hold no token. Open the link as `agent-bridge web enable` printed it,
or enter the token by hand. The same applies when such a link is pasted into a page that is already
open: the open session and its route are kept and the fragment is cleared. A fragment encoded eight or more
times is cleared too. A board or topic whose own name contains the text `access_token` cannot be
opened by a link, a reload or the browser's Back and Forward buttons: its fragment is cleared and the
page opens, or stays on, the current or default board (choosing it in the sidebar still works).
A route that does not contain the text is left alone. Only a fragment that contains the text
`access_token` is recognised as a link: a token with no such text beside it cannot be told from an
ordinary route and is not touched.

A link that does unlock leaves nothing of itself in the address. A board it names is opened only if the
page's own list holds it
([Opening a board from the access link](#opening-a-board-from-the-access-link)).

Tokens are not stored in cookies,
referrers or browser storage; the page stores only the theme and sidebar preferences described above. Static login assets contain no message history or
credentials. A theme can be added to the access link's fragment
([Choosing the theme from the link](#choosing-the-theme-from-the-link)); the token is still
removed from the address and never stored. The board route can join them in the same fragment.

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
conversations and drives every theme choice with an already installed Playwright and Chromium. It checks
choosing each theme from the plain address and from the access link, and opening a named board from a link
that carries the route, the theme and the token in every order (first paint, only that theme's
sheet requested, the token and theme removed from the address and absent from storage, cookies and
requests, a menu choice surviving a reload, and ignored unknown, empty and repeated values),
unlocking, the sidebar, Markdown topics, mentions and publishing, read-only conversations and the
details panel, the board view, All traffic, the collapsible sidebar, the quick switcher, theme
switching (including quick overlapping picks with held sheet responses), contrast, that the page never scrolls or overflows at desktop and phone widths, and that
no content-security violation occurs. For Orca Dark, Orca Light and Orca (system) it also checks contrast,
that no Classic or Sticker Bomb colour is used, and that nothing scrolls or overflows on every screen,
menu, dialog, toast and empty state listed in [DESIGN.md](../DESIGN.md) (Orca theme), with the browser in
each appearance. For Orca (system) it emulates both appearances and changes the appearance while the page is open
(including quick changes with held sheet responses), and checks that the page follows it in both directions without a
reload, requests only the sheet it needs, keeps one theme sheet, and still saves only `orca-system`.
It also opens every access-link shape listed under [Browser access](#browser-access), readable or not,
at load and pasted into an open or a locked page, and checks that the address and history entry never
keep the token, that only the readable shapes unlock, and that nothing reaches storage, cookies, a
request, the console or another origin:

```sh
cargo build --locked
python3 tools/test_web_browser.py --binary ./target/debug/agent-bridge
```

No browser is downloaded by the harness, and its private brokers and browser contexts are
disposable; do not point it at a running owner service or saved profile. A separate script,
`python3 tools/check_orca_tokens.py`, compares the Orca colours, typeface and sizes in the two Orca sheets
with the stylesheet of an installed Orca application (read only; it exits 2 and checks nothing where Orca
is not installed); it is not run in CI. Run the browser test with a Python that has
Playwright and a matching Chromium already installed (for example a throwaway virtual environment). It
launches Chromium with `--use-mock-keychain --password-store=basic`, so macOS shows no keychain
prompt. The broker's agents are Claude sessions bound to private sockets, and a stand-in `claude`
executable on the broker's PATH names two of the three sessions (the third shows the broker's generated name), so the names the page shows differ from the
agent IDs; it checks the page shows the names and the IDs only in the details panel. An `--output`
directory may be new, empty, or one an earlier run made (once the directory is checked, the run puts a
`.agb-web-browser-test` file in it, so a run that crashed still counts): the run removes what the earlier run
left directly in it (`state`, `home`, `bin`, `s` and `broker.log`), removes any symbolic link directly in it
(never what it points to), and overwrites the copied binary and the screenshots. A directory that has other contents and no such file, a symbolic link, a file, or a `--binary`
inside the directory (other than the copy an earlier run made) is refused with a message and exit status 2,
and nothing is touched. If the broker fails to start, the run stops it before it exits. The path must be short
enough for the broker's Unix socket path. It is not run in CI.

Four further manual harnesses in `tools/` cover behaviour the theme test does not: the board drafts,
rebase, archive and restore flows, catalog discovery, code copy, message send and retry, and topic
publishing. Each starts its own disposable broker with a throwaway state directory, drives Chromium
through the same sidebar and `#/board/<name>` address routes a person uses (they pass
`--use-mock-keychain --password-store=basic`, and read the page through `page.evaluate` because the
content-security policy blocks `wait_for_function`), and needs Python 3.11 or newer with Playwright and
a matching Chromium. Run them from the repository root against a binary built from the same tree, each
with a new, short output directory (the archive harness refuses an existing one), never against a running
owner service. None runs in CI.

```sh
cargo build --locked
python3 tools/test_monitor.py --binary ./target/debug/agent-bridge --browser --output /tmp/agb-monitor
python3 tools/monitor_catalog_qa.py --binary ./target/debug/agent-bridge --output /tmp/agb-catalog
python3 tools/monitor_board_archive_qa.py --binary "$PWD/target/debug/agent-bridge" \
  --binary-sha256 "$(shasum -a 256 target/debug/agent-bridge | cut -d' ' -f1)" \
  --source-checkpoint "$(git rev-parse HEAD)" --output "$HOME/agb-archive"
python3 tools/test_topics_browser.py ./target/debug/agent-bridge /tmp/agb-topics
```

- `test_monitor.py` checks the message API (authentication, origins, filters, historical session pinning,
  the live stream, credential rotation) with a real loopback peer exchange, and with `--browser` the
  private-link unlock, All traffic with the details panel and live delivery-state changes, and then
  runs the board, code-copy and message checks (`monitor_board_qa.py`, `monitor_code_copy_qa.py` and
  `monitor_message_qa.py` have no command line of their own; they run from here).
- `monitor_catalog_qa.py` (also run by `test_monitor.py --browser`) checks automatic board discovery, its cadence, back-off, timeout and
  single flight, and that a late old response never replaces the current list.
- `monitor_board_archive_qa.py` checks Archive, Restore and the Archived boards dialog, including held
  and dropped real responses. It refuses a binary whose hash differs from `--binary-sha256`.
- `test_topics_browser.py` checks topic create, rename, archive and restore, publish, reply, mention,
  retry after a lost response, loading older posts, locking with a publish in flight, and a broker restart.

The page has no Refresh button, Raw Markdown view or board dropdown; these harnesses read the exact
Markdown the page keeps (in its hidden `#board-raw`), provoke a re-read by dropping the live connection or
editing the board, and use the sidebar to open boards and topics.

## Message agents from the board

Recognized message-input declarations are hidden in the rendered document and
remain byte-for-byte in the board's Markdown source (the page has no Raw Markdown view; read it with
`agent-bridge board <name> --raw`). Malformed or unrecognized text remains visible and inert. Each message
input is labelled with the broker's resolved name of its exact pinned recipient, never an ID (a session
without a name shows "Claude" and the last four characters of its ID). Use Tab/Shift+Tab to reach or leave an
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
