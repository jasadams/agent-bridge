# Native session setup

Agent Bridge connects to existing Codex, Claude Code and OpenCode sessions. The broker,
client MCP connection and native delivery adapter have separate responsibilities:
MCP provides agent tools, while native adapters deliver incoming messages to the
selected running session.

## Shared-board changes

For every native client, board mutations must use broker tools: `create_board`,
`read_board`, `update_board` and lifecycle operations. Use logical board names and
current revisions, never shell/editor writes to backing files. On conflict, review
the returned latest snapshot and retry only the intended change while preserving
other writers. If the tools are unavailable, report the blocker rather than bypassing
the broker with filesystem edits. Normal responses omit internal backing-file paths;
user-authored Markdown and unrelated connection configuration remain unchanged.

## Codex

The service discovers loaded sessions from the existing native Codex daemon and
follows lifecycle and name-change events. No model call or manual registration is
needed for discovery. Check `codex_discovery` in:

```sh
agent-bridge status
agent-bridge list
```

Optional registration hooks provide messaging instructions when a session starts:

```sh
agent-bridge hooks install
```

Review and trust those hooks through Codex's normal `/hooks` flow. Discovery and
receiving messages do not depend on trusting the optional hooks. See the
[official Codex documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)
for MCP client configuration.

On Codex 0.165.0, verified active threads that support direct input receive peer
handoffs through their current turn's native steer path. Idle or unsupported-capability
threads retain native queuing, which may wait for the whole active turn to finish.
Native acceptance is not model read or task completion. See [Codex delivery and backpressure](codex-delivery.md)
for version limits, bounded admission and explicit recovery; the [original diagnosis](codex-delivery-diagnosis.md)
retains queue-only evidence and historical uncertainty.

## Claude Code

Discovery uses the installed native CLI, independently of hooks:

```sh
claude agents --json
agent-bridge list
```

Claude Code **2.1.300** is the verified version. Bridge reads full native UUIDs,
names, working directories and native status on demand; it does not scan private
registries or transcripts, install a mod, or maintain an active-session catalog.
An idle ordinary terminal is discoverable without any Bridge registration. Rename
and resume retain the UUID; a different session with the same name remains a
different identity. Names that match several UUIDs are ambiguous.

The local list result is `{agents, providers}`. `providers.claude.state` is
`complete`, `unavailable`, or `incomplete`; a failed query is not an empty directory.
Other providers' results remain visible. Old array-only `list` RPC callers receive
an error when a queried provider is unavailable/incomplete. Native background work
can be listed without a live process; missing runtime status stays unknown.
`native_status` preserves Claude's process status and background state separately.
`startedAt` is start time, **not last activity**. Claude has no inactivity-age
filter; `--max-age-days` is passed only to providers with native update evidence.

Discovery is not delivery. Claude's current delivery adapter still needs the
native inbox socket supplied through the existing lifecycle hooks. To configure
that delivery binding (not native discovery):

```sh
agent-bridge hooks install
```

The startup hook binds the session's native inbox socket; the end hook retires that
binding. Hooks run through Claude Code's normal workspace trust flow. Native rows
without a binding remain discoverable with `deliverable:false` and
`delivery_unavailable_reason:delivery_binding_missing`. A refused socket also makes
delivery unavailable; it does not prove that a native conversation has vanished.
Exact registered routes retain their existing delivery checks, even if directory
queries fail. Bridge never starts, resumes or prompts an agent for discovery.

Existing aliases, grants and disabled registrations are preserved. Old `enabled`
flags mixed lifecycle availability with explicit disable, so a legacy false value
is conservatively retained rather than automatically re-enabled from discovery.
Schema 20 separates that durable owner intent in `agent_owner_policy` from binding
availability. Explicit register/unregister changes owner policy; native hooks,
rebinds, end events and failed socket probes do not. Thus a failed delivery binding
cannot hide a still-present native UUID, and native rebind cannot undo an explicit
owner disable. Alias policy is pinned to name/provider/UUID, not inherited by a
same-name replacement. Delivery binding retirement and lifecycle control cleanup
remain active independently of discovery.
After verifying the exact UUID and intended binding, explicitly register that
route again to restore it. No permission is inferred from a native name, and no
same-name replacement inherits grants. Native-only discovery creates no DB row,
delivery binding or peer grant. Message IDs, outcomes, deduplication and Unknown's
no-automatic-replay behavior are unchanged.

When incoming messages are held, review the client's cross-session message
settings. Agent Bridge leaves those choices and tool approvals under the owner's
control. See [Claude Code's inbound message controls](https://code.claude.com/docs/en/cross-session-messaging#control-inbound-messages)
and [MCP setup](https://code.claude.com/docs/en/mcp).

Ordinary incoming messages use Claude's native cross-session presentation, labelled
with the sender's native session name or explicit registration alias. Remote names
include their machine qualifier. The transport name is not part of the agent's name.
The message contains its text and a short reference (message ID, reply address and
recipient); repeated JSON envelopes and command instructions are omitted. Startup
context explains how to send and reply through the messaging MCP tools or CLI.
Use the stable addresses in that reference for routing, not a shortened display name
or Claude's native `SendMessage` tool. Machine metadata remains available through
`get_message` / `agent-bridge get MESSAGE_ID`.

The cross-session wrapper is an internal Claude format checked against 2.1.300;
client updates may require compatibility changes. Native control notifications
(pairing and board changes) keep their separate format. A live UI check is needed
when qualifying a new client version.

## OpenCode

For supported OpenCode **2.2.5** V2, install the bundled native plugin and MCP
connection explicitly:

```sh
agent-bridge opencode install
```

V2 reloads changed configuration. Send a prompt in the
existing conversation you want to use, then check its live registration:

```sh
agent-bridge list
```

Its exact route is `opencode-SESSION_ID`, where the native ID starts with `ses_`.
Routes are verified against the current native server. The setup command
does not start a native session or model, change permissions, or enable peer
sharing. Repeat it after upgrading Agent Bridge to update the extracted plugin.

See [OpenCode installation options](install.md#optional-opencode-integration) for
custom state/configuration paths, backups and removal, and the
[OpenCode guide](opencode.md) for lifecycle, receipts, recovery and explicitly
shared sessions.

## Session names

`agent-bridge list` shows native directory observations, retained bindings, native display names and stable
routing names. Automatically registered names use `codex-<thread-UUID>`,
`claude-<session-UUID>` or `opencode-<ses_ID>`. Qualified display names such as `codex:reviewer` are also
accepted for messaging. If a display name is ambiguous, use the stable name.

```sh
agent-bridge list --all
agent-bridge send --to 'codex:reviewer' --message 'Ready for review.'
```

Use `list --all` or MCP `list_agents({"include_stale":true})` to inspect hidden local
bindings. For Claude, absent native rows carry `native_absent`; failed native reads
carry `directory_unavailable`. These diagnostic binding rows are not export
candidates. Socket checks describe delivery readiness separately from membership.
They send no message. An unbound native row does not become deliverable merely
because discovery succeeded.

Claude's native process kind/status and UUID, not the inbox socket or a Bridge
lease, describe directory membership. No last-active timestamp is invented from
process start or file modification time. Display names remain labels; use stable
routing names when names collide.

An available session indicates transport availability. It does not establish that
the model is currently working or that a message has been read. Manual Claude
aliases share the same socket checks. A verified native session end disables all
aliases of that exact session; Codex close/unload or daemon disconnect does the
same. Removing one registration with ordinary `unregister` affects only that
alias. Alias removal alone is not a lifecycle transition while the native
identity remains available through another registration.

## Permissions and troubleshooting

The broker's private Unix socket belongs to the current OS user. A sandboxed
client or shell may also need permission to connect to that socket. Grant only
the specific access required through your client's normal approval controls;
installation does not enable unrestricted shell access or bypass trust settings.

For an end-to-end check, ask one registered agent to send a short message to a
second session and request a correlated reply. Inspect the message with
`agent-bridge get MESSAGE_ID`. `delivered` confirms native client acceptance, not
model read or task completion. Claude's
socket-only interface remains `unknown` even when a fixture later observes input.
See [delivery outcomes](delivery-outcomes.md).

If a send times out, retain its UUID and retry with the same `--id` and exact
message. For a message in `unknown` state, inspect the native conversation before
explicitly retrying: a partial submission may already have delivered it.

```sh
agent-bridge history --agent user
agent-bridge retry MESSAGE_ID
```

Native delivery depends on the capabilities exposed by the installed client and
its running session. [Trust and recovery](peer-trust.md) covers grants, revocation
and safe upgrade/rollback; [MCP setup](mcp.md) covers the structured command path.

## Optional local lifecycle events

A coordinator can opt into `agent.opened` and `agent.closed` through generic
[`subscribe` selectors](mcp.md#event-subscriptions), alongside explicit
`board.changed` selectors. Use the exact receiving route in native context.
The atomic subscribe-time snapshot lists currently available local identities;
subsequent control notices describe genuine availability changes, not new or
deleted transcripts. Repeated discovery, heartbeats and renames do not open agents.

Claude SessionEnd and confirmed socket failure, Codex native discovery/close/
archive/unload and daemon disconnect, and OpenCode disposal/deletion/archive or
lease expiry supply availability evidence. Fallback observation may be delayed;
`/exit` is not guaranteed to synchronously notify all clients. Notices are local
control data, require no acknowledgement, grant no permission and do not enter
peer-message history. Closing the recipient prunes its subscriptions; reopen or
broker restart requires explicit resubscription and a fresh returned snapshot.
Snapshots reject more than 256 canonical identities or 65,536 encoded JSON bytes
before subscription changes; they never silently truncate identities. Display
labels are bounded to 4,096 UTF-8 bytes with safe truncation and `...`, without
changing exact routes or ordinary listing metadata.

Overflow or a Failed or Unknown lifecycle delivery marks the stream incomplete.
The broker attempts one `resync.required` notice and suppresses further lifecycle
transitions until explicit lifecycle subscription obtains a fresh snapshot.
Resync attempts are not automatically replayed. Unsubscribe, closure, replacement
and restart block pending delivery for invalid recipients or selectors; native
I/O admitted earlier may still finish. See the [event subscription recovery
contract](mcp.md#event-subscriptions).

For OpenCode, subscribe/unsubscribe receipts include bounded `recovery`
diagnostics for hidden attempted controls: total count, truncation, and at most
16 items containing ID, state, pending reconciliation and current route/lease
availability. Other providers return the same empty recovery shape. A fresh
snapshot can coexist with blocked native delivery. Inspect the ID, then use
`agent-bridge --state-dir "/same/private/state" retry CONTROL_ID` for explicit
read-only native reconciliation through that broker. This never reinserts an
attempted prompt. Missing proof remains Unknown; neither unsubscribe nor cancel
proves no insertion occurred. Diagnostics trigger no native work or automatic
retry. See [event recovery](mcp.md#event-subscriptions) for the complete contract.
