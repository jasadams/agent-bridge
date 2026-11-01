# OpenCode V2 plugin

Supported baseline: OpenCode **2.2.5** and `@opencode/plugin` **2.2.5**.
Development checks use Bun **1.3.11**; the native executable's embedded runtime is
recorded independently by the harness. Production uses the supported V2 plugin
session API, not an owner HTTP service or the removed V1 SDK.

From the repository root:

```sh
bun install --cwd plugins/opencode --frozen-lockfile
bun run --cwd plugins/opencode typecheck
bun run --cwd plugins/opencode test
```

For normal opt-in setup use the built executable:

```sh
agent-bridge opencode install
# After a binary update, refresh the extracted assets explicitly:
agent-bridge opencode install
agent-bridge opencode uninstall
```

OpenCode reloads configuration after changes. This configures the plugin
and existing MCP connection, preserving unrelated config and broker state; it
does not start sessions, inference or a service. See [installation](../../docs/install.md)
for config paths, conflicts, backups, retained immutable assets and removal.

For source development, a disposable project's `opencode.json` can instead opt
into an existing private broker:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "plugins": [{"package": "/absolute/agent-bridge/plugins/opencode", "options": {
    "stateDir": "/absolute/private/bridge-state"
  }}]
}
```

Alternatively set `AGENT_BRIDGE_STATE_DIR` in that OpenCode process. Loading
never creates sessions, grants remote access or modifies native
permissions. The explicit installer also configures the MCP connection; the
manual plugin-only example above does not.

The V2 context hook adds a stable exact-session messaging/board block to each
model request after live identity verification. Compaction augments the default
summary context, and later requests refresh the block without appending setup to
peer transcript messages. Ordinary peer text is a concise sender label, complete
body and minimal route/message references. System context still consumes request
tokens; this is plain text, not a rich native peer card. Board subscriptions are
opt-in and retain own-write suppression, debounce, pause and lifecycle cleanup.
Native owner pairing prompts remain unavailable; use the explicit owner CLI
workflow. Peer text and board edits never supply consent.

## Scope and leases

The plugin registers verified live sessions within its server directory, rather
than claiming to follow one selected TUI tab. Creation, the prompt hook, native
activity/status and `tui.session.select` activate an exact existing session.
A title update only updates an already-live session. UI selection paths without
a server event are observed on the next native prompt/activity. Build/plan mode
never becomes a routing identity. Stable routes are `opencode-<session ID>`.

Each plugin instance atomically creates a 0700 directory under `/tmp` and a 0600
socket named `<instance UUID>.sock`. The short path avoids macOS `TMPDIR` length
limits. Linux `SO_PEERCRED` or macOS `getpeereid` verifies connected same-user
peers; unavailable Bun descriptors fail closed. Frames are strict bounded UTF-8
newline JSON, one per connection; at most 64 socket/native handlers are active.

Registration returns a broker namespace and exact session generation. Heartbeats
every five seconds renew a 20-second lease and report event-cached readiness;
they never poll the native model/session API. Every delivery and asynchronous
continuation checks generation, membership and lease. Close/delete/archive and
normal disposal close the exact generation. Crashes expire; another instance may
take over after expiry. A persisted conversation alone never proves liveness.
Stale heartbeats, closes and late callbacks cannot authorize a replacement.

## Durable receipt protocol

Probe frames are `{op:"probe",session}`. Successful probe replies bind `protocol:2`,
instance and exact session. Delivery/reconciliation frames additionally carry
`id`, `content`, `namespace`, `generation`, `attempt` and `native_message_id`.
Namespaces, generations, attempts and broker IDs are UUIDs. The native ID is
`msg_` plus SHA-256 of `namespace + NUL + exactSession + NUL + lowercaseBrokerUUID`.
Reply identities echo these fences. Status is `inserted`, `deferred` or `uncertain`.

The private `stateDir/opencode-receipts.sqlite3` journal uses SQLite WAL with
FULL synchronous commits. Immutable namespace/ID/session/content/native-ID
mappings progress through `prepared`, `attempting`, and `inserted`. Only the winner
of a committed cross-process `prepared`→`attempting` compare-and-set can call
`session.prompt`. The broker also admits that exact in-flight claim immediately
before the fence, checking pause/cancel/current generation. No asynchronous queue
continues submission after a deferred result. A rejected or lost broker admission
response defers only while the durable mapping is still `prepared`; an existing
attempt fence always requires read-only reconciliation.

Mappings/tombstones are never evicted. A write transaction enforces 65,536 rows
and 64 MiB of UTF-8 content plus receipt-proof bytes across all namespaces; at capacity, new IDs defer
before any native attempt with an explicit capacity error, while existing
mappings remain available. Native insertion already observed without a journal
row remains uncertain at capacity and can be proved by read-only reconciliation. Capacity is not permission to
delete the journal or reuse an attempted ID. There is no automatic journal
compaction; preserve it with the matching broker state.

V2 `session.prompt` returns after durable native inbox admission. The plugin
validates the returned exact native ID/session/user type/text/steer delivery/time,
then atomically commits bounded original generation/attempt/receipt facts in the
existing journal before returning `inserted`. That retained token means verified
client acceptance, **not** transcript insertion, model read or task completion.
Exact scoped transcript readback remains independent acceptance evidence. See
[provider proof and every journal-field consumer](../../docs/delivery-outcomes.md#opencode-v2-proof-and-retained-journal-fields).

Reconciliation is strictly read-only: no admission, journal transition or prompt
call, including ordinary retries and `--allow-duplicate`. Same-generation durable
receipt proof or exact native transcript readback can prove acceptance, including after a missing journal
mapping or legacy migration. This never creates or mutates a journal row.
An existing mapping must still match the immutable identity/content. Corrupt
journals or mismatched text remain Unknown; none
proves no attempt. A crash after the attempt fence but before insertion may therefore
require manual inspection. Legacy UUID-based native IDs are
accepted only for read-only reconciliation; absent legacy mappings require exact native proof or remain
Unknown. Journal schema 2 adds nullable `acceptance` to the existing table; old
inserted rows have no fabricated receipt. A fresh reconciliation nonce is not the
original guarded dispatch attempt. Changed generation or lost native response
before proof commit needs exact transcript evidence. No same-ID replay repairs
an Unknown result.

## Busy and permission behavior

FIFO, bounded queued work, pause/cancel and uncertain ordering barriers are
broker-owned. Live unattempted work has no 120-second age expiry; lease loss or
generation replacement still fails it. The updated plugin advertises readiness
for native input admission during active model/tool work and pending permissions.
It verifies session ownership and lifecycle before a fresh attempt. V2 keeps its
own current agent/model/variant; the prompt API does not override that selection.
The plugin copies no tools or policy fields.

In 2.2.5 `session.prompt` durably admits input before waking the runner without
cancelling it. OpenCode owns that native input; a subsequent normal model request
can see it at the next runner iteration. An in-flight request cannot. Appending
input does not answer a pending permission. Native errors and early stops may
leave accepted input unprocessed: Delivered proves native acceptance, not processing
or completion, and never authorizes another prompt.

Refresh extracted assets with `opencode install`; V2 reloads changed plugin config.
Protocol 2 is unchanged; the V1 plugin implementation and native harness paths
are superseded, not a second supported runtime lane.
The 64-per-session/1000-global bounds cover broker queued/sending/Unknown barrier work,
not Delivered native records. Journal lifetime row/byte ceilings still apply.
See [the recovery contract](../../docs/opencode-recovery.md).

## Disposable native validation

```sh
tools/opencode-native.sh /absolute/path/to/agent-bridge /absolute/path/to/opencode
tools/opencode-setup-native.sh /absolute/path/to/agent-bridge /absolute/path/to/opencode
```

The harness uses isolated HOME/XDG/config/session/broker state and a scripted
localhost OpenAI-compatible model fixture. It exercises the actual pinned native
runtime, plugin, broker and correlated MCP reply path, native inbox acceptance during held
native tools/model streams, next-boundary model visibility, owner selection,
alias FIFO, pause/cancel, a 125-second live wait, pending permission input and
no replay after native stream error. Temporary
fixture policy is initial test configuration; no target session's permission
rules are changed and no native permission response/cancel API is called.
All children are stopped; failure evidence is retained, and setting
`AGENT_BRIDGE_NATIVE_LOG_DIR` also retains successful disposable evidence.
Registry access may be needed
for native dependency initialization. Synthetic model responses validate real
execution/tool wiring, not independent language-model judgement, production
owner settings or physical-network behavior.

The setup harness runs the real installer in temporary HOME/XDG directories,
loads its extracted assets and MCP connection, and checks model-visible context,
actual compaction/reload, compact peer text, correlated replies and
board tool/notification behavior. It also checks unavailable native owner pairing
selection before any exchange is created. Its scripted provider demonstrates
transport and tool execution, not independent model judgement. Linux execution
does not establish macOS native-runtime or interactive UI qualification.

The setup fixture records original installer output, then explicitly normalizes
only its disposable MCP config to native `mcp.servers` and `codemode:false` for the
scripted provider. The exact generated command and extracted plugin directory
are preserved and checked; extracted module hashes must match current source.
This is installed-plugin qualification, not a claim that the installer emits
native V2 MCP shape. It is not a rerun of the historical three-provider matrix.
