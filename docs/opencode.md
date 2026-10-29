# OpenCode native messaging

The built-in `opencode` provider targets an exact existing OpenCode session, not
its build/plan mode. This integration supports OpenCode **1.18.35**, its **V1**
`@opencode-ai/plugin` and `@opencode-ai/sdk` packages at **1.18.35**. Native
OpenCode embeds Bun **1.3.14**; standalone plugin tests use Bun **1.3.11**. The plugin package and lockfile are in `plugins/opencode/`.
Opt-in setup is available through `agent-bridge opencode install`; see
[installation and removal](install.md). Setup writes plugin/MCP configuration and
bundled assets only. It does not start a native session, run inference, replace a
service, enable sharing or change native permissions. Restart affected OpenCode
instances after installation, updates or removal.

The plugin uses the SDK client supplied by OpenCode. That client can use an
in-process fetch transport: `serverUrl` is deliberately not used to discover or
contact an HTTP server. No model API key belongs in the broker. Normal native
usage continues, and delivery never creates a replacement conversation or edits
transcript files.

## Session context and presentation

The V1 system transform supplies one stable messaging block on each model request
for a verified live session. It names the exact `opencode-SESSION_ID` route and
broker state directory, explains explicit acknowledgement and correlated replies,
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

The existing MCP connection provides discovery, send, acknowledge, correlated
reply and shared-board read/update. Board notifications remain off by default;
subscribe with the exact session identity, attribute own writes, and unsubscribe
when no longer needed. Keep board task/status/blocker or user-action entries
brief and revise the current entry; leave detailed logs in the native terminal.
The board has no required table schema. See [board behavior](board.md).

Native owner pairing prompts are unavailable for OpenCode: no supported V1 owner
Yes/No input workflow has been established. CLI/MCP pairing selectors reject an
OpenCode target before creating an exchange; use the explicit owner CLI pairing
workflow instead. Board controls remain supported. Peer text, board content and
native permission requests cannot supply pairing consent or change permissions.

## Live session scope

The plugin belongs to one OpenCode server/plugin instance and may observe several
live sessions. It does not advertise stored history at startup. Session creation,
chat submission, observed activity/status, and the runtime `tui.session.select`
event activate an existing session only after verification through the supplied
client. Title updates refresh metadata for sessions already observed as live;
renaming archived history alone does not register it. A TUI's selected session is
one source of activity, not a global single-session routing switch. V1 has no
reliable active-TUI-selection feed: selection paths without a server event become
visible on the next native prompt/activity. Session IDs
use `ses_` plus 26 ASCII letters/digits; titles are display labels, never delivery
identities. Routes are `opencode-SESSION_ID`; use `opencode:TITLE` for title lookup.
Duplicate titles remain ambiguous.

Deleting/archiving a session or disposing the plugin unregisters its live routes.
A crashed/closed instance expires through a 20-second lease, renewed every five
seconds by the live plugin. Broker expiry also runs while paused and without
native events. Stale generations cannot renew, close or update a replacement.
A new verified live instance can reopen the same conversation after expiry;
old queued work cannot silently follow its new generation. Explicit retry of
provably unattempted work checks the original exact identity before rebinding.
Attempted work only reconciles through native readback.

## Private transport and receipts

Each plugin instance has a random UUID-named Unix socket in a private owner-only
directory. The endpoint is bound to that UUID, and every successful response must
match both the socket instance and exact session. Rust validates socket type,
owner, mode, private directory, path length and connected peer UID. The plugin
checks the connecting UID with the native Unix credential API and rejects
unavailable credential checks. It validates one bounded newline JSON frame per
connection, payload fields, UUID message IDs and live session membership before
calling the client. Unknown, archived and mismatched sessions fail closed.

The local `opencode_observe` operation registers a verified live generation and
returns its durable broker namespace. Heartbeats, readiness, close and native
admission bind that exact generation and endpoint instance. Metadata alone cannot
enable a session. The lease readiness bit means eligible for native input admission,
including during active work and unanswered permissions. Socket protocol **2** delivers bounded structured frames that
bind session, broker message ID, namespace, generation, dispatch attempt, original
rendered content and native message ID. Both plugin and broker must be upgraded
together; protocol 1 cannot provide this recovery contract.

FIFO and bounded backpressure belong to the broker, keyed by provider and native
session across aliases. At most 64 pending messages may target one OpenCode
conversation; the shared 1000-message queued bound also applies to direct/remote
enqueue and retries. The plugin submits native input once through `promptAsync`,
including while a tool/model operation or permission request is active. OpenCode
persists the input and joins its existing runner without cancelling the task or
answering permission. The next normal runner iteration can include that input;
an already issued model request cannot see it. The current native agent, model
and variant are preserved from validated session metadata; unavailable active
selection defers before an attempt. No tools or permission overrides are copied.
Pause is checked again at admission; queued work remains cancellable until then.
The plugin has no hidden future-submission queue. Unattempted broker work waits
while its exact owning session/generation remains live, and is dispatched when
admission becomes available.
Waiting is bounded by the queue count and message byte limits, rather than a
120-second age limit. Lease loss, close or generation replacement still fails
queued work without native events; restart does not extend a lease. Historical
failed messages stay failed until explicit operator retry. A queued *reconciliation* of an earlier attempt cannot be cancelled as if
it were unattempted work. Unresolved attempts remain barriers to fresh native
submissions; read-only reconciliation can inspect multiple uncertain messages.

These queue bounds apply to broker queued/sending/uncertain messages, not to input
already inserted into the native conversation. Submitted input is owned by
OpenCode; the journal's durable row/byte limits still apply. `submitted` proves
exact native insertion, not model consumption, acknowledgement or completion.
Native errors, permission denial, structured-output stops or other early exits
may leave inserted input unprocessed. The bridge never replays it to force another
run. The owner retains control of permission and native continuation.

Active admission requires reinstalling the plugin with `opencode install` and a
full OpenCode restart. Protocol 2 frames and receipt semantics are unchanged; its
version alone does not advertise active support. A loaded older plugin remains
idle-only with the updated broker. An older broker with the updated plugin may
admit active input but retains its old 120-second broker expiry policy.

A native ID is `msg_` plus a SHA-256 digest of the durable broker namespace, exact
session and lowercase broker UUID, separated by NUL bytes. This avoids native ID
collisions between brokers and conversations. The broker stores original rendered
content before delivery. A private FULL-sync SQLite plugin journal retains
immutable mappings and prepared/attempting/inserted tombstones. Its atomic
prepared-to-attempting transition has one winner across plugin processes. Receipts
are persisted before success. Durable limits (65,536 rows and 64 MiB of text)
reject new mappings rather than evicting duplicate protection. No automatic journal
pruning is provided.

`session.promptAsync` returning HTTP 204 is **not** evidence of durable insertion.
The plugin verifies exact native user-message ID, session, role and text before a
successful receipt. The broker records **submitted**, never acknowledged or
complete. A lost, malformed, mismatched or missing response after an attempted
write is uncertain. `retry MESSAGE_ID` for attempted OpenCode work is **read-only
reconciliation**, even with `--allow-duplicate`: exact existing native content can
prove insertion, including when a journal mapping is missing; absence cannot prove
no attempt, and never authorizes another prompt. Missing/corrupt journals do not
create permission to repeat an attempt. All migrated legacy IDs reconcile read-only. An old broker's explicit retry could
turn a submitted/uncertain message back into queued without preserving attempt
provenance, so even migrated queued work is uncertain and blocks fresh submissions.
Legacy absence cannot authorize a new prompt or a replacement namespaced ID;
genuine unattempted legacy work is indistinguishable and requires conservative
operator resolution. Only modern durable pre-attempt records can be retried as new
submissions under the same message ID.

Native status/selection checks and submission do not reserve the runner or lock
owner selection. Input can be inserted during existing native work; the supported
runner joins that work without cancelling it. Subsequent processing follows
native behavior. Admission linearizes
an in-flight submission: a later pause cannot recall it. No native cancellation or
permission response is sent. A model must explicitly acknowledge and send a reply
with the broker parent ID. There is no exactly-once model-processing or completion
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
even if a cached directory changes. The replying session may use its canonical
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
preserving existing permissions. Schema 13 adds local lease/generation and durable
delivery binding tables with transactional enqueue/backpressure guards. Older
binaries reject this newer database schema;
a downgrade requires the matching pre-upgrade database backup and binary.

No API `tools`/permissions fields, permission hooks, approval responses, automatic
remote sharing or native peer cards are introduced.

## Validation

The [compatibility matrix](compatibility.md) describes reproducible real-client
qualification, precise evidence boundaries and outstanding platform/UI checks.

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
OpenCode/plugin/broker execution, acknowledgement/reply, read-only reconciliation,
native insertion during held tools and SSE model streams, next-boundary model
visibility, preserved owner agent/model/variant, alias FIFO, queued pause/cancel,
more than 125 seconds of paused unattempted work, unanswered permission input and
no replay after a controlled stream error. It makes no native cancel/permission-response calls. It does not
test an external model's reasoning or the owner's production environment. Physical/native macOS runtime
acceptance remains separate from source checks and CI tests.

Baseline API sources: [V1 plugin interface](https://github.com/anomalyco/opencode/blob/v1.18.35/packages/plugin/src/index.ts),
[native identifier generation](https://github.com/anomalyco/opencode/blob/v1.18.35/packages/opencode/src/id/id.ts),
[plugin documentation](https://opencode.ai/docs/plugins/), and
[server messages](https://opencode.ai/docs/server/#messages).
