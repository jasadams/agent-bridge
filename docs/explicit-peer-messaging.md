# Explicit address peer messaging

Explicit-address mode connects paired bridges over encrypted QUIC using manually configured IP addresses. It includes durable transfers and remote delivery status while retaining the native Claude and Codex adapters. [LAN mode](lan-messaging.md) adds automatic IPv4 discovery; [Public rendezvous and self-hosted relays](public-relays.md) add outbound-only circuit connections; [Automatic NAT traversal](nat-traversal.md) can upgrade relayed connections to direct paths; DNS addresses remain a later milestone.

## Upgrade existing state

Stop the service and back up the entire state directory before replacing the binary. Startup applies current migrations, preserving existing messages, identities and grants. Schema 3 introduced messaging, schema 4 adds LAN configuration and invitations, and schema 5 adds machine names. Rollback requires restoring the pre-upgrade state backup. See [identity and trust instructions](peer-trust.md) for fingerprint verification and key backups.

Networking is disabled by default. Local messaging works without enabling it. Once enabled, the setting and listening address persist across service restarts.

## Configure both machines

Run a bridge on each machine. Exchange and verify the complete public fingerprints through a trusted channel, then pair independently on both machines:

```sh
agent-bridge identity show
agent-bridge peers pair OTHER_PEER_ID --fingerprint VERIFIED_FINGERPRINT --alias workstation
```

Choose a reachable local IP and an available UDP port on each machine. For a machine with address `192.168.1.20`:

```sh
agent-bridge network enable --listen /ip4/192.168.1.20/udp/44001/quic-v1
agent-bridge status
```

Enable records configuration; inspect `network.listening` and `network.last_error` in status to confirm startup. Port 0 chooses an available port shown in status, but a fixed port preserves manual configuration after restart. Listening on `0.0.0.0` binds all IPv4 interfaces; advertise a concrete reachable address to the other machine.

Record the other machine's address on each bridge:

```sh
agent-bridge peers add workstation --address /ip4/192.168.1.21/udp/44001/quic-v1
```

The address is a connection hint. The authenticated identity must match the paired peer ID. This milestone accepts only `/ip4/IP/udp/PORT/quic-v1` or `/ip6/IP/udp/PORT/quic-v1`. Configure both directions so either bridge can initiate sends and synchronize status after reconnecting. Firewalls must allow the chosen UDP ports. Already routable internet addresses with manual forwarding can work; restrictive NAT and blocked UDP require later connectivity features.

## Grant access and discover agents

On each bridge, grant permissions for its own local sessions:

```sh
agent-bridge list
agent-bridge peers allow workstation --agent reviewer --permission discover
agent-bridge peers allow workstation --agent reviewer --permission receive
agent-bridge peers allow workstation --agent reviewer --permission send
agent-bridge list --remote
```

Discovery lets the peer see this session. Receive lets it deliver here. Send lets this local session originate messages to that peer, whose destination also needs receiving permission. The default sender `user` needs its own explicit grants when used. Permissions are independent and pinned to provider and session IDs.

Remote directories refresh automatically while networking is enabled, normally within five seconds of a change. Only permitted sessions appear. Cached entries remain available offline and show `online: false`, allowing enqueueing to a previously known session. Each peer can export up to 100 agents; narrow discovery grants if that limit is reported.

## Send and reply

```sh
agent-bridge send --from reviewer --to workstation/reviewer --message 'Please review this change'
```

Remote names resolve to a peer ID, provider and destination session when queued. Later alias replacement cannot redirect that work. Retrying an identical send with the same `--id` and original arguments returns the original record even if aliases or directories changed. Conflicting reuse fails.

Send and continue working. The receiver assigns its own local message UUID and displays the source as `PEER_ID/source-agent`. The native envelope includes that UUID and exact reply references for useful responses:

```sh
agent-bridge send --from reviewer --to PEER_ID/source-agent \
  --in-reply-to RECEIVED_LOCAL_UUID --message 'Review complete'
```

A correlated reply pins the original source session from the received envelope and does not require rediscovery. Explicit replies can wake agents. Delivery-status synchronization only updates records. The conversation limit is 100 reply hops; peers running older versions may still enforce the previous eight-hop limit.

## Interpret receipts and native status

Outbound remote `get` results include a separate `transfer` object:

| Transfer state | Meaning |
| --- | --- |
| `pending` | Stored locally and not attempted. |
| `awaiting_receipt` | Attempted; durable acceptance remains unconfirmed. |
| `accepted` | The remote bridge durably committed the message or reported its persisted delivery status. |
| `rejected` | The remote bridge reported a permanent rejection. |
| `blocked` | Local trust or sending permission no longer permits transfer. |
| `cancelled` | An unattempted transfer was cancelled locally. |

The top-level message `state` is `pending`, `delivered`, `failed` or `unknown`.
After remote synchronization it reports the latest known destination outcome.
Transfer acceptance alone is not native acceptance. Policy-blocked work remains
Pending with an explicit retry reason; cancelled unattempted work is Failed.
Delivered never proves model read or completion. `transfer.observed_at` records
when the latest remote observation was stored locally; offline observations can
be stale. Transfer states above are diagnostics, not another public message lifecycle.
See [adapter proof and compatibility](delivery-outcomes.md).

The origin commits its outbox before network I/O. The receiver atomically commits the immutable envelope, deduplication identity and native queue record before returning acceptance. Lost receipts retry with the same origin-peer/message-ID pair. Identical duplicates return acceptance without scheduling native delivery again, even if the queue is now full or the destination session disappeared. Conflicting duplicates fail.

Ambiguous network transfers retry automatically with bounded exponential backoff and jitter. Queues and status cursors survive restarts. An interrupted destination native submission becomes `unknown` and is never replayed automatically. Network deduplication cannot guarantee exactly-once model execution.

Cancel is available only before the first transfer attempt. After an attempt, a lost receipt means remote acceptance may already have happened. Accepted native deliveries must be inspected and, if needed, explicitly retried at the destination using its local UUID and the existing duplicate-override rules. Rejected or locally blocked outbound transfers can be retried after fixing permissions. Replaying an accepted envelope never bypasses receiver deduplication or reruns native delivery.

## Disable and revoke access

```sh
agent-bridge network disable
agent-bridge peers deny workstation --agent reviewer --permission receive
agent-bridge peers revoke workstation
```

Disable closes the listener and connections and stops new attempts while retaining queues, caches and history. Re-enabling resumes pending and ambiguous transfers. Queued network requests recheck the persisted setting before acceptance. Already in-flight transfers may have committed elsewhere and cannot be recalled; previously accepted native queue entries continue under local delivery rules.

Revocation disconnects the peer, clears grants and cached exports, and blocks queued inbound and pending outbound work. Receiving permissions are checked again before native dispatch. Submitted work cannot be recalled. Restoring trust requires explicit pairing and fresh grants. Blocked messages require an explicit retry; they do not silently resume. A blocked inbound message can be retried safely at its destination after permissions are restored.

## Resource bounds and trust

Network frames are limited to 256 KiB and bodies to 64 KiB. JSON escaping counts toward the serialized limit. Status pages respect both event count and encoded size. Queues retain the 1000 queued-message limit and allow 100 pending transfers per peer. New inbound messages are capped at 60 per peer per minute and 30 per peer and destination session per minute. Duplicate receipts bypass those new-message limits. Retained inbound history also has per-peer limits of 64 MiB/8,192 messages and global limits of 256 MiB/32,768 messages, including both stored body and envelope bytes. Completed deliveries still count. New messages at capacity are refused without deleting history or deduplication records; identical accepted retries still succeed. See [quota and maintenance details](security-review.md#sr-03--unbounded-completed-inbound-history-medium).

The transport bounds connections, streams, queues and request duration. Only paired, nonrevoked identities reach the application protocol. Peers can request permitted directories, send messages and synchronize their own delivery statuses. They cannot invoke local administration or read arbitrary history. Same-OS-user processes retain administrative access through the private Unix socket.

History and deduplication records are retained indefinitely. Protocol synchronization makes no model API calls and submits no model notifications. Automated validation uses real loopback QUIC brokers and simulated native adapters; physical-machine and real native application acceptance remains a release step.
