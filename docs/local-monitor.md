# Local monitor

The same `agent-bridge` executable contains the HTTP server and all HTML, CSS and
JavaScript. It reads the running broker's existing message database; there is no
separate web process, Node.js runtime, CDN, asset download or model call. Messages
shows delivery history; Boards reads local named boards and accepts only changes
to their existing checkbox and marked text response controls.

With the broker running, enable it:

```sh
agent-bridge monitor enable
```

The command returns a loopback `url` and a separate `access_token`. Open that URL
in a browser and paste the token into the access field. The default chooses an
available port; use `monitor enable --port 44002` for a fixed one. It binds only
`127.0.0.1`, including when a fixed port is chosen. There is no remote bind option.

```sh
agent-bridge monitor status  # Listener state; no token
agent-bridge monitor access  # Retrieve URL/token via the private local socket
agent-bridge monitor disable
```

The monitor starts disabled after every broker restart. It can operate with peer
networking disabled. Repeating enable with the same requested port preserves the
current access; disabling it or choosing another port invalidates credentials and
closes existing browser streams. A port conflict reports an error without stopping
the broker. Enabling/disabling the monitor changes no machine pairing, session
sharing, delivery or network listener settings.

## Inspect messages

The feed shows local, inbound and outbound messages with machine, agent/session,
delivery state and timestamps. Filter by machine name/peer ID, agent/session,
state and creation-time range. Load older pages, select a message for its full
body/detail, and follow its parent or direct replies. Message previews are bounded;
the selected detail retains the full stored body. Live server-sent invalidations
refresh the current view, including state changes and correlated replies.

New local messages snapshot the source and destination session metadata when
created. Network transfers use their saved envelopes, preserving the actual pinned
sessions even after registrations change. Earlier local messages retain their saved
destination session, but historical source sessions were not recorded and appear
as unknown. Remote machine aliases/names are display labels, not proof of human
identity. The monitor never creates a peer identity just to display local messages.

Message views remain read-only. There are no browser send, acknowledge, retry,
cancel, pairing or grant controls. Receipt/submission states have their existing meaning;
monitoring does not establish that a model ran or completed a task.

## Browse and answer boards

Choose Boards, then select a board from the visible selector. The first selection
is `main`, including its empty virtual entry before a file exists. Lists are in
name order. Board names, existence and revisions come from the same broker used
by CLI/MCP; unknown names never fall back or create documents. Create boards and
perform general edits through [the CLI/MCP board tools](board.md).

The view renders headings, tables, lists and code, with bounded wrapping/overflow
and a readable source fallback for complex Markdown. Raw Markdown shows the exact
source. HTML and scripts do not execute, images do not fetch resources, and safe
external links require an explicit click. Board layout is user-authored; no task
table schema is required.

The Responses pane uses the terminal's Rust control parser and source mapping.
Code examples, ordinary bracket text, malformed/nested/duplicate marked fields
and response contents never create controls. A checkbox save sends its intended
checked state. Text responses have Save and Cancel. Changes preserve bytes
outside that response and use the same revision checks and notification behavior
as the terminal. Browser writes are unattributed user-interface changes, so the
appropriate board's subscribers can receive a change notice. Reading never
subscribes a native agent or changes a board.

Drafts stay in memory and remain pinned to their board, control and revision.
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

Only the selected board is refreshed, independently of message activity, using
conditional reads with at most one read in flight. Normal refresh is about once
per second; failures back off to at most 30 seconds between attempts. External
file edits are visible through the same revision check. A temporary failure keeps
the last content with a stale/reconnecting indicator. Leaving Boards or locking
stops its refresh work. Selection in the browser never changes terminal/MCP board
selection.

## Browser access

API requests carry a random access token in an Authorization header. The page
keeps it only in memory; locking the monitor clears message/board views, raw source,
controls, drafts, pending requests and the token,
and closing/reloading the page requires access again. Tokens do not appear in URLs,
query strings, cookies, referrers or browser storage. Static login assets contain
no message history or credentials.

Exact Host validation, Origin and browser fetch-metadata checks reject DNS rebinding
and cross-origin requests. The server supplies no CORS permission, uses restrictive
content-security/frame/referrer policies and disables caching. Message text
renders as text, including HTML-like content. The only write route is authenticated
`POST /api/boards/{name}/responses`; other non-GET routes remain rejected. That
route requires exact same-origin mutation context and accepts only an existing
control identity, expected revision and typed desired value, never a path,
replacement document, attribution or arbitrary broker command. Access generation
is checked again at the shared transaction boundary; completed revocation prevents
later commits. An admitted change can finish before revocation completes.

Board JSON bodies are limited to 512 KiB with a five-second read deadline; decoded
text and the resulting document are each bounded by 64 KiB. There are at most
eight board read jobs, four active writes and ten writes per second, within the
global limits of 32 connections, 100 requests per second and eight message event
streams. Aborting a browser request does not
release a queued worker's budget or authorize it after revocation. The server
logs neither board/message bodies, responses nor HTTP access tokens.

Board responses remain shared data, not authenticated owner approval or executable
commands. Browser access adds no machine pairing, session grants or native tool
permissions.

This boundary does not protect against another process running as your OS user,
an administrator, or a privileged browser extension. Such processes may already
access the broker's private local socket or state. Keep the browser token private;
disable/re-enable the monitor to invalidate a copied token.

## Upgrade and validation

Schema 9 adds local message context snapshots and an ordered monitor index without
rewriting message bodies or delivery state. Follow the [backup/rollback guidance](peer-trust.md#upgrade-and-rollback)
before running this version on existing state. Reads share the broker's existing
connection and never open a second database through startup recovery.

Automated HTTP tests check authentication, origins/Host, read-only methods, bounded
filters, port errors and stream/key revocation. Data tests check historical session
pinning, pagination, preview/full body, migration and reads without delivery writes.
The disposable acceptance harness can exercise local and real peer messages and
an already installed Playwright browser without starting native model sessions:

```sh
cargo build --locked
python3 tools/test_monitor.py --binary ./target/debug/agent-bridge --browser
```

Omit `--browser` when Playwright and its matching browser are unavailable; that run
has no browser acceptance claim. No browser is downloaded by the harness.
The extended harness also exercises named-board navigation, response writes,
conflicts/drafts, live refresh, untrusted Markdown and late-callback/lock behavior
against the built executable. Its private brokers/browser contexts are disposable;
do not point it at a running owner service or saved profile.
