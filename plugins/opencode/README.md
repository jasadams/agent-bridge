# OpenCode V1 plugin

Supported baseline: OpenCode **1.18.35**, `@opencode-ai/plugin` and
`@opencode-ai/sdk` **1.18.35**, Bun **1.3.11**. This is a V1 server plugin;
it never constructs a client from `serverUrl`. The supplied SDK client may only
work inside OpenCode's process.

Install development dependencies and validate reproducibly from the repo root:

```sh
bun install --cwd plugins/opencode --frozen-lockfile
bun run --cwd plugins/opencode typecheck
bun run --cwd plugins/opencode test
```

The plugin is opt-in. A disposable project's `opencode.json` can load its absolute
file URL and select an existing private broker state directory:

```json
{
  "plugin": [["file:///absolute/agent-bridge/plugins/opencode/index.ts", {
    "stateDir": "/absolute/private/bridge-state"
  }]]
}
```

Alternatively set `AGENT_BRIDGE_STATE_DIR` in that OpenCode process. Loading does
not scan history, start sessions, change permission rules, or share sessions.
The installer and the owner's native configuration are outside this ticket.

Registrations cover sessions observed live within this server/plugin directory,
not only the currently displayed TUI conversation. Session creation, native
`tui.session.select`, `session.status` activity and `chat.message` verify the exact
session through the supplied client. A title update only updates an already-live
session. Selecting a TUI session through a UI path that emits no server event is
observed on its next native prompt/activity; the V1 server plugin has no reliable
active-TUI-selection feed. The route is `opencode-<native session ID>`; build/plan
mode never becomes a routing identity. Delete/archive/stale-session observation
removes the registration; normal disposal unregisters and removes the socket.
An abrupt native crash can leave a visible stale route: delivery fails against its
dead private endpoint, and restart replacement/reconciliation is deferred to AGB-4.

Each plugin instance atomically creates a 0700 directory under `/tmp` (independent
of a potentially long macOS `TMPDIR`) and a 0600 Unix socket named
`<instance UUID>.sock`. Same-user connected peers are checked using Linux
`SO_PEERCRED` or macOS `getpeereid`; a missing/changed Bun socket descriptor fails
closed. One bounded UTF-8 newline JSON frame is accepted per connection:

```json
{"op":"probe","session":"ses_0123456789abcdefghijklmnop"}
{"op":"deliver","session":"ses_0123456789abcdefghijklmnop","id":"12345678-1234-4234-8234-123456789abc","content":"peer envelope"}
```

Replies bind the exact instance and session. Delivery receipts also bind `id` and
`native_message_id`. Unknown fields, unknown/stale sessions, other directories,
wrong native identities, oversized frames and invalid payloads fail closed.
The socket authenticates the local user, not arbitrary remote peers; shared broker
permissions remain authoritative.

The native ID is `msg_` followed by the lowercase broker UUID without hyphens.
Concurrent/repeated ordinary retries share the same submission; an ID reused with
another session or payload is rejected. Receipt records are retained for this
plugin lifetime, with a bounded capacity that rejects new IDs rather than evicts
deduplication state. A pre-existing exact native message is verified rather than
rewritten. The plugin supplies only `messageID` and a text part to
`session.promptAsync`; no tools, permission fields, model or agent override.

In 1.18.35 the HTTP prompt-async handler forks prompt processing and returns 204.
That response is not durable insertion evidence. The plugin polls the exact native
message and validates its user role, session, ID and text before returning success.
A successful socket receipt means **submitted**, never model acknowledgement,
completion or permission approval. An ambiguous insertion retains its ID and never
replays the API call automatically. The memory receipt contract does not provide
crash durability, ordered busy-session handling or restart recovery; these are
AGB-4. Remote sharing/UI presentation remains AGB-6.

Run the isolated native check with a locally built binary:

```sh
tools/opencode-native.sh /absolute/path/to/agent-bridge /absolute/path/to/opencode
```

This creates disposable HOME/XDG/config/session/broker state, starts the actual
pinned native runtime and loads this plugin. A scripted localhost OpenAI-compatible
model fixture invokes the actual native MCP acknowledgement and correlated reply
tools, and the harness checks the exact native user-message ID and broker parent.
It requires no owner credentials or external model key and cleans up child
processes and state. It validates native execution and tool wiring with synthetic
model responses, **not** independent language-model judgement, human approval,
physical-network behavior or busy/restart recovery. Dependency installation may
need registry access; all generated native configuration is temporary.
