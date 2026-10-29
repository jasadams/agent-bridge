# Native session setup

Agent Bridge connects to existing Codex and Claude Code sessions. The broker,
client MCP connection and native delivery adapter have separate responsibilities:
MCP provides agent tools, while native adapters deliver incoming messages to the
selected running session.

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

On Codex 0.165.0, native queue acceptance can wait for the entire active turn to
finish, including tool continuations; the native queue is limited to 100 entries.
Acceptance is not model receipt. See the [delivery diagnosis and proposed active
handoff path](codex-delivery-diagnosis.md) for reproducible evidence and limits.

## Claude Code

Install lifecycle hooks, then start a new session or restart the relevant session:

```sh
agent-bridge hooks install
```

The startup hook registers the session's native inbox socket and the end hook
disables its automatic registration. Hooks run through Claude Code's normal
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
context explains how to acknowledge and reply through the messaging MCP tools or CLI.
Use the stable addresses in that reference for routing, not a shortened display name
or Claude's native `SendMessage` tool. Machine metadata remains available through
`get_message` / `agent-bridge get MESSAGE_ID`.

The cross-session wrapper is an internal Claude format checked against 2.1.300;
client updates may require compatibility changes. Native control notifications
(pairing and board changes) keep their separate format. A live UI check is needed
when qualifying a new client version.

## Session names

`agent-bridge list` shows enabled registrations, native display names and stable
routing names. Automatically registered names use `codex-<thread-UUID>` or
`claude-<session-UUID>`. Qualified display names such as `codex:reviewer` are also
accepted for messaging. If a display name is ambiguous, use the stable name.

```sh
agent-bridge list --all
agent-bridge send --to 'codex:reviewer' --message 'Ready for review.'
```

An enabled session indicates transport availability. It does not establish that
the model is currently working or that a message has been read. Manual aliases
must be unregistered explicitly when no longer needed.

## Permissions and troubleshooting

The broker's private Unix socket belongs to the current OS user. A sandboxed
client or shell may also need permission to connect to that socket. Grant only
the specific access required through your client's normal approval controls;
installation does not enable unrestricted shell access or bypass trust settings.

For an end-to-end check, ask one registered agent to send a short message to a
second session and request an acknowledgement and reply. Inspect the message with
`agent-bridge get MESSAGE_ID`. A `submitted` state alone is not proof of receipt
or task completion.

If a send times out, retain its UUID and retry with the same `--id` and exact
message. For a message in `uncertain` state, inspect the native conversation before
explicitly retrying: a partial submission may already have delivered it.

```sh
agent-bridge history --agent user
agent-bridge retry MESSAGE_ID
```

Native delivery depends on the capabilities exposed by the installed client and
its running session. [Trust and recovery](peer-trust.md) covers grants, revocation
and safe upgrade/rollback; [MCP setup](mcp.md) covers the structured command path.
