# OpenCode V1 plugin

Supported baseline: OpenCode **1.18.35**, `@opencode-ai/plugin` and
`@opencode-ai/sdk` **1.18.35**. Development checks use Bun **1.3.11**;
the installed OpenCode executable embeds Bun **1.3.14**. These are separate
runtimes: the native harness records its embedded version. This is a V1 server
plugin using only its supplied SDK client. `serverUrl` may have no listener.

From the repository root:

```sh
bun install --cwd plugins/opencode --frozen-lockfile
bun run --cwd plugins/opencode typecheck
bun run --cwd plugins/opencode test
```

A disposable project's `opencode.json` can opt into an existing private broker:

```json
{
  "plugin": [["file:///absolute/agent-bridge/plugins/opencode/index.ts", {
    "stateDir": "/absolute/private/bridge-state"
  }]]
}
```

Alternatively set `AGENT_BRIDGE_STATE_DIR` in that OpenCode process. Loading
never scans history, starts sessions, grants remote access or modifies native
permissions. Owner configuration and service installation remain separate work.

## Scope and leases

The plugin registers verified live sessions within its server directory, rather
than claiming to follow one selected TUI tab. Creation, `chat.message`, native
activity/status and `tui.session.select` activate an exact existing session.
A title update only updates an already-live session. V1 UI selection paths without
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
`promptAsync`. The broker also admits that exact in-flight claim immediately
before the fence, checking pause/cancel/current generation. No asynchronous queue
continues submission after a deferred result. A rejected or lost broker admission
response defers only while the durable mapping is still `prepared`; an existing
attempt fence always requires read-only reconciliation.

Mappings/tombstones are never evicted. A write transaction enforces 65,536 rows
and 64 MiB of UTF-8 content across all namespaces; at capacity, new IDs defer
before any native attempt with an explicit capacity error, while existing
mappings remain available. Native insertion already observed without a journal
row remains uncertain at capacity and can be proved by read-only reconciliation. Capacity is not permission to
delete the journal or reuse an attempted ID. There is no automatic journal
compaction; preserve it with the matching broker state.

A 204 response only accepts background native work. The plugin verifies the exact
native user message's ID, role, session and complete text before committing
`inserted` and returning success. This means **submitted**, never model
acknowledgement, completion or permission approval. The agent explicitly uses
existing broker MCP tools to acknowledge and reply with the broker ID as parent.

Reconciliation is strictly read-only: no admission, journal transition or prompt
call, including ordinary retries and `--allow-duplicate`. Exact native readback can prove insertion, including after a missing journal
mapping or legacy migration. This never creates or mutates a journal row.
An existing mapping must still match the immutable identity/content. Corrupt
journals, missing native messages or mismatched text remain uncertain; none
proves no attempt. A crash after the attempt fence but before insertion may therefore
require manual inspection/acknowledgement. Legacy UUID-based native IDs are
accepted only for read-only reconciliation; absent legacy mappings require exact native proof or remain
uncertain. No same-ID replay repairs an unknown result.

## Busy and permission behavior

FIFO, bounded queuing/deadlines, pause/cancel and uncertain ordering barriers are
broker-owned. The plugin refreshes native status once per fresh delivery and
uses cached busy/permission events. Known busy work or pending permission defers
before the attempt fence. An idle/readiness event lets the broker reconsider its
existing queue; the plugin does not retain hidden submission work.

Status checking cannot atomically reserve OpenCode's runner. In 1.18.35 the
native prompt creates a user message before `ensureRunning` joins existing work.
A user can start work after the idle snapshot: insertion may occur during that
work and be processed by the existing run. The integration never cancels work,
answers a permission prompt, supplies API `tools`/permissions fields, or replays
an uncertain prompt. A receipt proves insertion, not processing order inside a
native run. See [the recovery contract](../../docs/opencode-recovery.md).

## Disposable native validation

```sh
tools/opencode-native.sh /absolute/path/to/agent-bridge /absolute/path/to/opencode
```

The harness uses isolated HOME/XDG/config/session/broker state and a scripted
localhost OpenAI-compatible model fixture. It exercises the actual pinned native
runtime, plugin, broker and MCP acknowledgement/reply path, blocking native tool
work, pending native permission, and the residual idle-to-busy race. Temporary
fixture policy is initial test configuration; no target session's permission
rules are changed and no native permission response/cancel API is called.
All children and temporary state are cleaned up. Registry access may be needed
for native dependency initialization. Synthetic model responses validate real
execution/tool wiring, not independent language-model judgement, production
owner settings or physical-network behavior.
