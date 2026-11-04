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

The startup hook records the session's private inbox socket as a delivery binding.
The end hook removes only the matching normalized endpoint, so a late end event
cannot close a replacement. No native membership or enabling flag is stored; sends
verify native state through the connector boundary when they need it. Hooks run
through Claude Code's normal workspace trust flow. Check the client session's status
if it does not expose an inbox socket.

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

`agent-bridge list` reads Claude, Codex and OpenCode native state for the current
request and returns `{"agents":[...],"providers":[...]}`. Native rows use exact
`codex-<thread-UUID>`, `claude-<session-UUID>` or `opencode-<ses_ID>` routes. An
explicit registration alias may replace a route name; display names and titles are
labels only and never resolve a destination. Canonical provider session IDs are
reserved and cannot be registered as aliases.

```sh
agent-bridge list
agent-bridge list --max-age-days 30
agent-bridge send --to 'codex-<thread-UUID>' --message 'Ready for review.'
```

The age filter applies only to OpenCode, whose native API supplies an activity
timestamp; Claude and Codex ignore it. Default is 7 days, explicit 30 is useful for
cold sessions. `list --all` and MCP `include_stale: true` include disabled manual
Inbox configuration only; they do not retain saved native rows.

Native availability means the provider answered this request. It does not establish
that the model is working, that a message has been read, or that a session is
permitted. Use exact provider/session identities for routing. Removing an explicit
alias affects only that alias; queued messages keep their immutable identity.

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

## Board notifications

Explicit `board.changed` subscriptions remain available; the removed
`agent.opened`/`agent.closed` selectors have no replacement. Native lookup is not
permission and does not create a durable availability catalog. See the
[event subscription contract](mcp.md#event-subscriptions).

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
