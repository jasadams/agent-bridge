# Persistent identity and peer trust

Identity and trust commands operate through the private broker socket. Networking is disabled by default. Enable [explicit-address messaging](explicit-peer-messaging.md) or [LAN discovery and pairing](lan-messaging.md) to connect remote bridges.

## Upgrade and rollback

Before starting this version against existing state, stop the running service and back up the entire state directory, including any SQLite WAL and SHM files. Replace the old executable before restarting. On macOS, the default directory is `~/Library/Application Support/AgentBridge`; an explicit `--state-dir` selects another directory.

Startup transactionally adopts the old unversioned database as schema 1, installs peer tables as schema 2, and applies schema 3 for messaging, schema 4 for LAN mode and invitations, schema 5 for machine names, and schema 6 for single-use mutual pairing, schema 7 for configured relay operators, schema 8 for dual-carrier operator routes, schema 9 for historical message context and the local monitor index, and schema 10 for retained inbound usage accounting. Registrations, session titles, pause state and history are preserved. An interrupted local native submission becomes Unknown (legacy stored `uncertain`); unattempted queued messages remain Pending. Unsupported newer schemas are refused before migration or recovery writes. See [delivery compatibility](delivery-outcomes.md).

Earlier released binaries have no schema-version guard. To roll back to those binaries, stop the service and restore the complete pre-upgrade state backup first. Do not run an old executable against the migrated database. Backups containing `identity.key` hold the bridge's private identity and need the same protection as live state.

## Create and inspect an identity

With the broker running:

```sh
agent-bridge identity show
```

The first call creates an Ed25519 key in `identity.key` under the selected state directory. Subsequent calls and service restarts retain the same identity. Normal local messaging does not require creating or reading an identity.

The response contains a `peer_id` and `fingerprint`. Both contain the complete canonical libp2p PeerId, which binds the public key. Compare the entire value through a trusted channel when pairing. There is no short verification code or central identity service.

The private key is stored with mode 0600 inside a mode 0700 directory. Creation publishes a fully written key without overwriting an existing key. Corrupt keys, symlinks, extra hard links, foreign ownership and unsafe permissions are rejected. The bridge never silently regenerates an invalid identity. If interrupted creation leaves a temporary hard link, preserve the state and investigate the extra link before removing it; do not delete the published key to work around a load failure.

Copying the state directory to a different machine copies the identity and grants. Treat that as an intentional migration, and stop using the original copy. Key replacement currently requires re-pairing on every counterpart.

For new machines, use the [mutual pairing workflow](secure-pairing.md). It verifies both identities with one reciprocal exchange, enforces one responder per issued offer, and supports cancellation. The manual commands below remain available.

## Pair a known peer

Obtain the other bridge's identity through a trusted channel and compare it with the full fingerprint shown on that machine. Then record the pairing locally:

```sh
agent-bridge peers pair PEER_ID --fingerprint VERIFIED_FINGERPRINT --alias workstation
agent-bridge peers list
```

The fingerprint must exactly match the canonical peer ID. This checks the supplied values; it cannot establish whether the channel used to obtain them was trustworthy. Pairing records local trust and grants no agent access. The network worker connects separately when networking is enabled and an address is configured. Each machine independently pairs and grants access to the other. Self-pairing and assigning an occupied alias to another identity are refused.

Aliases are local labels. Administrative commands accept an alias or full peer ID. Policy decisions use authenticated canonical peer IDs, never aliases. Repeating a pairing preserves existing grants; re-pairing a revoked peer restores trust with no grants.

For normal two-way conversation, [share a selected session](session-sharing.md)
with one atomic command. Individual permissions remain available below.

## Grant access to one session

Choose an agent from `agent-bridge list`, then add only the required permissions:

```sh
agent-bridge peers allow workstation --agent reviewer --permission discover
agent-bridge peers allow workstation --agent reviewer --permission receive
agent-bridge peers allow workstation --agent reviewer --permission send
```

| Permission | Meaning |
| --- | --- |
| `discover` | The peer can see this local session in its permitted directory. |
| `receive` | The peer can send messages to this local session. |
| `send` | This local session can originate messages to the peer. Its destination also needs permission on the receiving bridge. |

Each grant is independent. Receiving permission does not imply directory access or permission to reply. A two-way conversation needs sending and receiving grants on both sides; directory access is also needed to discover destinations.

Grants pin the local provider and session ID at the time of the command. Renaming an agent does not change that binding, and replacing its routing name with another session does not transfer access. Different aliases for the same native session share its grants. Disabled sessions cannot use existing grants or receive new ones. New sessions start with no permissions.

The peer list shows every stored grant's provider, session ID and permission, including grants whose sessions are no longer registered. Those stale grants remain bound to the original session. Revoke the peer to clear all of its grants, or deny an individual grant while that session is still addressable.

## Deny or revoke access

```sh
agent-bridge peers deny workstation --agent reviewer --permission receive
agent-bridge peers revoke workstation
```

Deny removes that one session permission. Revoke marks the peer revoked and transactionally deletes all its grants. Policy queries read current persisted state, so revocation takes effect immediately without restarting. Re-pairing requires another fingerprint verification and fresh grants. Local messages and history are unaffected.

When networking is enabled, revocation also disconnects the peer, invalidates its remote directory and blocks queued delivery. Dispatch rechecks current receiving policy. See [explicit-address messaging](explicit-peer-messaging.md) for transfer and retry semantics.

## Trust boundary

Processes running as the same OS user can administer this bridge, as they can already register local agents and send local messages. Peer trust configuration is not a sandbox for those processes. A future authenticated remote bridge will vouch for source-agent names; individual model sessions do not yet have independent cryptographic identities. Native application permissions remain in force.
