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

Install lifecycle hooks, then start a new session or restart the relevant session:

```sh
agent-bridge hooks install
```

The startup hook registers the session's native inbox socket and the end hook
disables its automatic registration. Default local listings also check that the
private native socket still accepts connections, hiding missing or refused sockets
immediately. The broker checks enabled Claude registrations every minute and disables
them after two consecutive failures; a successful check or registration clears the
first failure. This repairs missed end hooks after crashes or disconnects. Hooks run through Claude Code's normal
workspace trust flow. Check the client session's status if it does not expose an
inbox socket.

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

`agent-bridge list` shows available registrations, native display names and stable
routing names. Automatically registered names use `codex-<thread-UUID>`,
`claude-<session-UUID>` or `opencode-<ses_ID>`. Qualified display names such as `codex:reviewer` are also
accepted for messaging. If a display name is ambiguous, use the stable name.

```sh
agent-bridge list --all
agent-bridge send --to 'codex:reviewer' --message 'Ready for review.'
```

Use `list --all` or MCP `list_agents({"include_stale":true})` to inspect hidden local
registrations. Stale rows include `stale_reason`: `disabled`, `socket_missing`,
`socket_refused`, or `socket_invalid`. Socket checks send no message; inconclusive
errors such as a full accept backlog do not retire a session. A disabled session
needs native registration again to become enabled.

Socket availability cannot distinguish a live background spare from an interactive
session, or prove that a session behind a shared process socket is still open.
Process kinds, stable restart aliases, activity timestamps and result pagination
are not currently recorded by this check. Display names remain labels; use stable
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
