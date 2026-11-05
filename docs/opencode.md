# OpenCode native messaging

The built-in `opencode` provider targets an exact existing OpenCode session, not
its build/plan mode. This integration supports OpenCode **2.2.5** and the **V2**
`@opencode/plugin` package at **2.2.5**. Standalone plugin tests use Bun
**1.3.11**; native embedded runtime evidence is recorded separately. The plugin
package and lockfile are in `plugins/opencode/`.
Opt-in setup is available through `agent-bridge opencode install`; see
[installation and removal](install.md). Setup writes plugin/MCP configuration and
bundled assets only. It does not start a native session, run inference, replace a
service, enable sharing or change native permissions. V2 reloads configuration
after installation, updates or removal.

The plugin uses OpenCode's supported V2 plugin context directly. It does not
discover or connect to an owner HTTP server. No model API key belongs in the broker. Normal native
usage continues, and delivery never creates a replacement conversation or edits
transcript files.

## Session context and presentation

The V2 context hook supplies one stable messaging block on each primary model request
for a verified live session. It names the exact `opencode-SESSION_ID` route and
broker state directory, explains sending and correlated replies,
and gives brief shared-board guidance. It replaces its own block if a hook array
is reused. Missing, expired or replaced identities omit the block; context hooks
do not activate stored history. Native compaction receives additional summary
guidance without replacing its default prompt. The next model request refreshes
the messaging block, including after reload when lifecycle activity verifies the
session again.

Ordinary peer messages contain a bounded sender display label, the complete body
and minimal message/from/to/parent references. Setup instructions are not appended
to each peer message. The resulting user text and system context are separately
model-visible: the system block still costs tokens on each request. This is plain
native text, with no claim of Claude-style peer cards or a custom rich renderer.
Display labels never replace exact route references.

The existing MCP connection provides discovery, send, correlated
reply and shared-board read/update. Board notifications remain off by default;
subscribe with the exact session identity, attribute own writes, and unsubscribe
when no longer needed. Keep board task/status/blocker or user-action entries
brief and revise the current entry; leave detailed logs in the native terminal.
The board has no required table schema. The system context directs agents to use
broker board/lifecycle tools only, never shell/editor writes to backing files. On
conflict, review the latest returned snapshot and retry only intended edits while
preserving other writers. If tools are unavailable, report the blocker rather than
writing files directly. Backing paths are internal metadata; authored Markdown
paths remain unchanged. See [board behavior](board.md).

Native owner pairing prompts are unavailable for OpenCode: no supported owner
Yes/No input workflow has been established. CLI/MCP pairing selectors reject an
OpenCode target before creating an exchange; use the explicit owner CLI pairing
workflow instead. Board controls remain supported. Peer text, board content and
native permission requests cannot supply pairing consent or change permissions.

## Native state and delivery bindings

The connector reads global native `session.list` pages and exact `session.get`;
it does not scan at startup or mirror title changes. Saved idle sessions and cold
locations remain native candidates; archived sessions are omitted. IDs use `ses_`
plus 26 ASCII letters/digits. Titles are display-only, never lookup or send routes.
This provider boundary is staged: public broker listing/routing cutover belongs to AGB-46.

Private protocol-2 state requests are `{op:"state_list",cursor:null|string}` and
`{op:"state_get",session:"ses_..."}`. Replies bind the operation and socket instance;
list replies contain `sessions`/`next_cursor`, exact replies echo `session` and
contain `state` (null only for confirmed absence). Rows require `session`,
`display_name`, `status`, `directory`, `updated_ms`; unknown metadata is null and
status is null because these native APIs expose no live status. The authenticated
transport endpoint is attached locally, never accepted from row JSON.

State reads never prompt or write the receipt journal. The broker lists the fixed
socket directory `<state-dir>/oc/` and asks every socket in parallel, each within a
500 ms first-reply deadline; a socket that answers is live. A refused connection
means the plugin is gone, so the broker unlinks that socket file. A hung plugin
is skipped for that request and never unlinked. At most 64 sockets, 64 pages and
4096 distinct sessions per socket are allowed. Rows for one session from several
sockets are merged (the first socket in sorted order wins; a later socket only
supplies status). Native failures are unavailable/incomplete, not successful empty
results. The plugin reports `status` (`idle`, `busy` or `retry`) from its in-memory
view of `session.status` events; it is not stored and is null until the first event
after the plugin starts.
Completed lists filter only native `time.updated` with inclusive age boundaries;
unknown update times remain included and exact lookup ignores age. The provider
accepts any positive u32 age (including 7 and 30), not a maximum of 30 days.

Nothing registers, renews or closes a delivery binding: liveness is the plugin
answering on its socket. A failed delivery is the failure; no pre-check gates it
and nothing expires by time. Sending to or from a session no plugin answers for
fails with an error naming the sender or the recipient. Queued work that finds no
plugin fails immediately and is retried explicitly. Explicit retry of provably
unattempted work checks the original exact identity. Attempted work only reconciles
read-only through retained validated native receipt proof or independent exact
transcript evidence.

## Private transport and receipts

Each plugin instance has a random UUID-named Unix socket in the private owner-only
directory `<state-dir>/oc/`, which the plugin creates from its `stateDir` option.
The endpoint is bound to that UUID, and every successful response must
match both the socket instance and exact session. Rust validates socket type,
owner, mode, private directory, path length and connected peer UID. The plugin
checks the connecting UID with the native Unix credential API and rejects
unavailable credential checks. It validates one bounded newline JSON frame per
connection, payload fields, UUID message IDs and live session membership before
calling the client. Unknown, archived and mismatched sessions fail closed.

Socket protocol **2** delivers bounded structured frames that bind session, broker
message ID, namespace, dispatch attempt, original rendered content and native
message ID. Native admission is checked by the broker (`opencode_admit`) against
the dispatch attempt, pause and policy. At-most-once insertion rests on the broker
message ID, the deterministic native message ID, the receipt journal (shared by
every plugin instance of one state directory; a compare-and-swap lets exactly one
instance prompt) and the dispatch attempt ID. Both plugin and broker must be
upgraded together; an older plugin gets "OpenCode plugin is outdated; restart
OpenCode".

Ordering and bounded backpressure belong to the broker, keyed by provider and native
session across aliases. At most 64 pending messages may target one OpenCode
conversation; the shared 1000-message queued bound also applies to direct/remote
enqueue and retries. The plugin submits native input once through V2 `session.prompt`,
including while a tool/model operation or permission request is active. OpenCode
durably admits input into its native inbox and wakes the runner without cancelling the task or
answering permission. The next normal runner iteration can include that input;
an already issued model request cannot see it. The current native agent, model
and variant remain selected by V2; prompt admission cannot override them.
No tools or permission overrides are copied.
Pause is checked again at admission; queued work remains cancellable until then.
The plugin has no hidden future-submission queue.

The local broker bounds active socket handlers. At capacity it stops accepting
until a handler finishes; excess connections remain subject to the operating
system's bounded listen backlog and client timeouts. A client timeout after a
request is written does not prove whether it was processed: inspect the existing
message ID and do not automatically resubmit an Unknown outcome. Plugin RPCs
that cannot obtain a local scheduling slot report that they were not submitted;
they do not open a broker socket. Admission and other RPCs have separate bounded capacity. Four stalled admissions can still delay or
reject further admissions before submission; inspect the outcome rather than
assuming a native attempt occurred.
Capacity diagnostics contain counts, not message bodies.

Waiting is bounded by the queue count and message byte limits, rather than a
120-second age limit. Historical
failed messages stay failed until explicit operator retry. A queued *reconciliation* of an earlier attempt cannot be cancelled as if
it were unattempted work. Unresolved attempts do not block later eligible messages;
they remain Unknown until positive read-only reconciliation proves acceptance.

These queue bounds apply to broker queued/sending/unknown barrier work, not to input
already accepted by the native client. Delivered input is owned by
OpenCode; the journal's durable row/byte limits still apply. `delivered` proves
exact native acceptance, not transcript/model consumption or completion.
Native errors, permission denial, structured-output stops or other early exits
may leave inserted input unprocessed. The bridge never replays it to force another
run. The owner retains control of permission and native continuation.

Refresh extracted assets with `opencode install`; V2 reloads changed configuration.
Protocol-2 tokens are retained as adapter compatibility details, not a second
public lifecycle. V1 implementation/harness paths are superseded.

A native ID is `msg_` plus a SHA-256 digest of the durable broker namespace, exact
session and lowercase broker UUID, separated by NUL bytes. This avoids native ID
collisions between brokers and conversations. The broker stores original rendered
content before delivery. A private FULL-sync SQLite plugin journal retains
immutable mappings and prepared/attempting/inserted tombstones. Its atomic
prepared-to-attempting transition has one winner across plugin processes. Receipts
are persisted before success. Durable limits (65,536 rows and 64 MiB of text plus proof bytes)
reject new mappings rather than evicting duplicate protection. No automatic journal
pruning is provided.

V2 `session.prompt` returns after durable admission. Before success the plugin
validates the exact returned native ID, session, user type, complete text, steer
delivery, finite creation time and absence of unexpected attachments. It atomically
persists original guarded receipt facts in the existing journal. Delivered means
client acceptance, **not** transcript insertion or model read. A lost/malformed/
mismatched native return or failed proof commit is Unknown. Read-only retry can
use that validated journal proof; its fresh request nonce
is distinct from the original dispatch attempt. Old NULL-proof
history, or lost native response before proof commit needs independent exact
transcript evidence. No absence or `--allow-duplicate` permits another prompt.
See [exact proof, field consumers, migration and limits](delivery-outcomes.md#opencode-v2-proof-and-retained-journal-fields).
All migrated legacy IDs reconcile read-only. An old broker's explicit retry could
turn a legacy submitted/uncertain message back into queued without preserving attempt
provenance, so even migrated queued work projects to Unknown but does not block fresh submissions.
Legacy absence cannot authorize a new prompt or a replacement namespaced ID;
genuine unattempted legacy work is indistinguishable and requires conservative
operator resolution. Only modern durable pre-attempt records can be retried as new
submissions under the same message ID.

Session verification and submission do not lock owner selection. Input can be
durably accepted during existing native work; subsequent processing follows
native behavior. Admission linearizes
an in-flight submission: a later pause cannot recall it. No native cancellation or
permission response is sent. A useful reply uses the broker parent ID.
There is no exactly-once model-processing or completion
claim. See [recovery invariants](opencode-recovery.md) for crash transitions.

## Sharing with a paired machine

OpenCode sessions are private by default. After owner-approved machine pairing,
explicitly share the exact live registration on each machine:

```sh
agent-bridge peers share machine-b --agent opencode-SESSION_ID
agent-bridge list --remote
agent-bridge send --from opencode-SESSION_ID --to machine-b/REMOTE_AGENT_ID --message 'Hello'
```

The share operation grants discover, send and receive for the exact `opencode`
provider and native `ses_` ID. A title rename keeps that identity; a replacement
session cannot inherit it. The other owner independently shares their destination.
Unshare/revocation and disabled/replaced registration checks apply before queued
native submission. Replies pin both endpoints to the original provider/session,
even if the remote directory changes. The replying session may use its canonical
route after receiving through a manual alias of the same provider/session; the
reply still targets the original sender route. See [session sharing](session-sharing.md).

Upgraded brokers negotiate provider support before dispatching an OpenCode source
or destination. A peer that successfully answers only the legacy directory is
reported as unsupported for OpenCode; work is not relabeled or reported delivered.
Legacy directories permanently omit OpenCode so older strict provider readers
can still discover Claude, Codex and Inbox. The protocol remains version 1 on
`/agent-bridge/message/1`; the opt-in `directory_supported` exchange adds bounded
explicit provider capabilities without granting session access. An extension
rejection or codec error alone does not prove lack of support: the broker first
requires a successful legacy directory response. The strict legacy directory
fixture is pinned to pre-OpenCode commit `8fde538`.

Schema 12 extends the strict session grant provider constraint to OpenCode while
preserving existing permissions. Schema 13 adds durable delivery binding tables with
transactional enqueue/backpressure guards. Schema 21 drops the stored OpenCode
registration table and the delivery generation column: no agent status or
liveness is stored. It is one-way; back up the broker database (`messages.sqlite3`
and its `-wal`/`-shm` files) before installing. Older binaries reject this newer
database schema; a downgrade requires the matching pre-upgrade database backup
and binary.

No API `tools`/permissions fields, permission hooks, approval responses, automatic
remote sharing or native peer cards are introduced.

## Validation

The [compatibility guide](compatibility.md) separates current V2 standalone
qualification from the historical three-provider matrix and outstanding platform/UI checks.

See the plugin README for frozen dependency installation, type checks, supplied
in-process-client tests, and the disposable native compatibility harness. Tests
use temporary directories and local processes; they do not use owner sessions,
configuration or credentials. Rust local transport coverage is in `tests/opencode.rs`; store/trust and
isolated two-broker remote coverage are in `tests/opencode_store.rs` and
`tests/opencode_network.rs`. These source checks do not establish live model
reasoning, physical cross-machine delivery or macOS runtime acceptance.
`tools/opencode-setup-native.sh` separately runs the built installer and extracted
plugin/MCP assets, checking model-visible context, native compaction/reload,
compact peer presentation and board opt-in/notification behavior. These checks
use the same credential-free local provider boundary.
The deterministic native harness uses a local model fixture; it checks real
OpenCode/plugin/broker execution, native acceptance/reply, read-only reconciliation,
durable inbox acceptance during held tools and SSE model streams, next-boundary model
visibility, preserved owner agent/model/variant, alias FIFO, queued pause/cancel,
more than 125 seconds of paused unattempted work, unanswered permission input and
no replay after a controlled stream error. It makes no native cancel/permission-response calls. It does not
test an external model's reasoning or the owner's production environment. Physical/native macOS runtime
acceptance remains separate from source checks and CI tests.

Baseline API sources: [V2 plugin interface](https://opencode.ai/v2/docs/build/plugins/server),
[V2 API](https://opencode.ai/v2/docs/api), [migration contract](https://opencode.ai/v2/docs/migrate-v1),
and [version-specific durable acceptance implementation](delivery-outcomes.md#opencode-v2-proof-and-retained-journal-fields).
The installed-asset fixture records an explicit disposable normalization of the
installer's legacy MCP shape to native `mcp.servers` plus direct-tool mode,
preserving the exact generated command and extracted package. It does not claim
the installer emits native V2 MCP configuration or rerun the full native matrix.
