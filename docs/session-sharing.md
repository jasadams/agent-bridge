# Share one agent session

Machine pairing and session sharing are separate decisions. After
[owner-approved pairing](lan-pairing.md), each owner selects the local sessions that
may communicate. New sessions remain private; no trust or permission propagates
through a familiar machine name or a reused session alias.

## Share and stop sharing

Ask your agent to share the current session with the intended paired machine. It
can use its registered ID from `agent-bridge list`:

```sh
agent-bridge peers share machine-b --agent LOCAL_AGENT_ID
```

This single local operation atomically grants `discover`, `send` and `receive` for
that exact provider/session. It allows machine-b to see and send to this session, and
this session to send to machine-b. Machine B's owner independently shares their local
session with machine-a. Neither side can grant permissions on the other's behalf.

The response shows the resulting local peer policy. Sharing is explicit,
idempotent and persists through reconnects/restarts. Existing individual grants
remain available with `peers allow/deny` for narrower policies. Sharing does not
enable networking, create trust, start a native agent or change its permissions.

```sh
agent-bridge peers shared
agent-bridge peers unshare machine-b --agent LOCAL_AGENT_ID
```

Unshare removes all three permissions for this exact session, including permissions
previously added individually; it does not alter other sessions. It also works for
a disabled registration. If the routing name was replaced, it refers to the new
session and cannot remove the old session's grants by accident. Use the exact
provider/session from `peers shared` to remove stale grants:

```sh
agent-bridge peers unshare machine-b --provider codex --session OLD_SESSION_UUID
```

OpenCode uses the same commands with `--provider opencode --session ses_...`;
its ID must be `ses_` followed by exactly 26 ASCII letters/digits. Claude/Codex
retain UUID session validation. Titles are display metadata, never grants.

This form needs no current registration and never broadens access. `shared` lists
partial grants too, along with provider/session, current routing names, enabled
state and a `stale` flag when no registration remains. `shared: true` means all
three grant records exist, not that the session is currently enabled or online.

Renaming retains the provider/session binding; replacing a name or creating another
native session does not inherit permissions. Aliases for the same provider/session
share the same policy. A correlated reply may use another local alias for the
same provider/session, including a native plugin's canonical route. Its destination
remains the original sender route and provider/session; an alias cannot redirect
the reply to another destination. Revoking a peer clears every grant for that identity.

## Inspect connections and pending requests

```sh
agent-bridge peers connections
agent-bridge pairing list
```

`connections` combines paired identities, discovered unpaired machines and local
pairing attempts. Each row includes a full peer ID, display name, trust/revocation,
network connectivity, pairing progress and granted sessions. States are:

| State | Meaning |
| --- | --- |
| `unpaired` | Discovered machine with no active trust or pending exchange. |
| `pairing_pending` | Untrusted identity with an incomplete local pairing attempt. |
| `connected` | Trusted identity with an established transport connection. |
| `offline` | Trusted identity without a current transport connection. |
| `network_disabled` | Stored trusted identity while local networking is disabled. |
| `revoked` | Trust withdrawn; grants cleared and communication blocked. |

[NAT traversal](nat-traversal.md) adds live direct/relayed `path` and `transport`
fields for connected trusted identities. Other rows have null path information.
A connection does not prove that an individual session is enabled, permitted or
currently thinking. These are local snapshots; remote directories may take up to
five seconds to refresh. Discovery labels are unverified; the full peer ID and
trust state distinguish them. `pairing list` displays verification codes and name
context for owners; `connections` omits codes.

Use `pairing cancel ID` for a pending attempt, `pairing close` to clear the local
window and pending attempts, or `peers revoke PEER` to remove committed trust.
Network disable stops transfers without deleting stored grants.

## Messages after sharing

```sh
agent-bridge list --remote
agent-bridge send --from LOCAL_AGENT_ID --to machine-b/REMOTE_AGENT_ID --message 'Hello'
```

OpenCode can be either endpoint when both brokers support it. Provider support
is negotiated separately from permission: it never shares an ungranted session.
The legacy directory permanently filters out OpenCode while retaining permitted
Claude/Codex/Inbox entries. This is covered by a strict older-reader fixture from
pre-OpenCode commit `8fde538`. An older peer that successfully answers the legacy
directory is explicitly unsupported for an OpenCode transfer; an extension failure
without that successful fallback is retried rather than treated as proof.

The receiver must explicitly acknowledge and send a correlated reply. Submitted
status is not proof of receipt or task completion. Stop-sharing and revocation
check current policy at acceptance and before queued native submission; cached
remote directories cannot override a withdrawn grant. Already attempted network transfers may still arrive if the destination still
permits them; already submitted native work cannot be recalled.

This milestone adds no wildcard sharing, session inheritance or browser UI. The
single-binary message monitor remains milestone 7 in the
[public P2P roadmap](public-p2p-roadmap.md).
