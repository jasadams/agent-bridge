# Public rendezvous and self-hosted relays

Milestone 3 adds an independently operable rendezvous and libp2p Circuit Relay v2
node to the same `agent-bridge` executable. Endpoints make outbound connections to
configured nodes, discover circuit routes and exchange pairing/messages through
them. No endpoint port forwarding or LAN multicast is needed for that path.

This is configurable, federated discovery/relaying, not a global DHT or identity
service. There is no mandatory operator, vendor account or centrally granted trust.
Anyone with a reachable server can operate a node; endpoints choose up to four.
[Automatic NAT traversal](nat-traversal.md) now attempts direct upgrades while retaining
relay fallback. [Internet invitations](internet-invitations.md) add DNS addresses,
signed contact links/QR codes and encrypted TCP fallback for blocked UDP.

## Operate a node

On a server with a reachable UDP port, run the installed executable:

```sh
agent-bridge --state-dir /path/to/private/relay-state relay serve \
  --listen /ip4/0.0.0.0/udp/4001/quic-v1 \
  --advertise /ip4/PUBLIC_SERVER_IP/udp/4001/quic-v1
```

Replace `PUBLIC_SERVER_IP` with a concrete IPv4 address. Explicit IPv6 QUIC
addresses are supported too. An operator must permit the listener in the server's
firewall and make the advertised address reachable. A relay hosted behind NAT
would itself need a reachable mapping; ordinary endpoint clients do not.

Wildcard listeners require an explicit advertised address. Advertisements accept
direct IP or canonical DNS addresses over QUIC or TCP, with the same carrier as their
listener; circuits and other peers' claims about the server's public
address are rejected. A concrete listener can advertise its actual address
automatically for local tests. See the invitation guide for dual-listener setup.

The node prints newline-delimited JSON status containing its `peer_id`, listeners,
limits, reservation/circuit/registration counts, denial counts and last error. It
prints no forwarded message bodies or private keys and throttles status output.
Share this public contact address with users:

```text
/ip4/PUBLIC_SERVER_IP/udp/4001/quic-v1/p2p/RELAY_PEER_ID
```

The pinned relay ID authenticates which infrastructure operator clients reach. It
is not the identity of the person they want to pair with.

The relay has its own persistent identity in its private state directory. Without
`--state-dir`, it defaults to `~/Library/Application Support/AgentBridgeRelay`,
separate from the normal broker. Milestone 4 nodes also provide bounded AutoNAT v2 reachability
checks; the NAT guide explains their diagnostic and destination limits. Relay serving refuses broker databases and takes
both role locks to prevent simultaneously using that state as an agent endpoint.
Keep keys/backups private, and reuse the same state directory after restart.
Ctrl-C stops the foreground server; dropping the library node aborts its tasks.
A process supervisor may run this command for a long-lived installation.

### Capacity and abuse controls

Defaults are 128 reservations, 128 circuits, 8 MiB per circuit and 300-second
circuit/reservation lifetimes; rendezvous permits 128 registrations with maximum
300-second registration lifetime, and the node permits 256 established connections.
Use `relay serve --help` for the corresponding `--max-*`, `--reservation-seconds`
and `--registration-seconds` options. Validation limits prevent zero/unbounded
settings. Per-peer reservations/circuits, pending connections and rendezvous
request/stream rates are also bounded. Quota exhaustion is a refusal, never an
implicit trust grant.

The application namespace is `agent-bridge`. Rendezvous records, timers, discovery
pages, cookies, response frames and handler concurrency are bounded. Large pages
are paginated under the encoded frame budget. Public infrastructure still needs
ordinary operational capacity planning: resource limits constrain costs but cannot
promise availability against every denial-of-service or Sybil attack.

## Configure endpoint clients

On each machine, with its broker running:

```sh
agent-bridge network relays add /ip4/SERVER_IP/udp/4001/quic-v1/p2p/RELAY_PEER_ID
agent-bridge network enable --listen /ip4/0.0.0.0/udp/0/quic-v1
agent-bridge network relays list
agent-bridge status
agent-bridge peers discover
```

No LAN mode is needed. `network enable --lan` can combine LAN discovery with
configured relays if desired. Relay configuration alone neither enables networking
nor creates machine trust, session grants or a private endpoint identity. Enabling
networking registers public routing metadata with configured operators.

The status `relays` array distinguishes connecting, reserved and error states,
registration readiness and errors. The `rendezvous` array contains ephemeral
public circuit hints. A reservation is a routing facility, not a paired endpoint.
Infrastructure connections are excluded from the application's trusted `connected`
list unless the operator identity was independently and explicitly paired as an
endpoint. Configured relay identities get no message or directory privileges.

Endpoints reserve circuits, renew short-lived registrations, follow bounded
paginated discovery and retry failed reservations with backoff. The current client
requests a 30-second registration and renews it before expiry. Multiple nodes can
provide alternative routes; an unavailable configured node does not prevent using
another. There is no automatic, unapproved selection of a new operator.

```sh
agent-bridge network relays remove RELAY_PEER_ID
agent-bridge network disable
```

Configuration persists in schema 7. Changing configured operators recreates the
transport and cancels incomplete pairing approvals. Disabling networking closes
transport tasks, reservations and ephemeral discovery while preserving configured
operators, committed trust, grants and durable messages. No background relay work
survives transport drop. Public discovery records at other nodes may remain visible
until their TTL expires; disabling is not immediate metadata erasure.

## Pair and share over a relay

Each owner needs the intended counterpart's public peer ID; public names are not
globally unique. Obtain contact details from the other owner. On the receiver:

```sh
agent-bridge pairing open --seconds 120
```

On the sender, once rendezvous reports the intended peer:

```sh
agent-bridge pairing connect OTHER_MACHINE_PEER_ID
```

The agents carry the handshake through a circuit. The receiver's owner accepts the
request; both owners independently compare the matching verification code through
a trusted channel and confirm locally, exactly as in [LAN pairing](lan-pairing.md).
No SSH, long-token copying, native wakeup from an unknown peer, or remote approval
is involved. A relay or discovered name cannot authorize the exchange.

Pairing creates no grants. Each owner then [shares their selected local session](session-sharing.md):

```sh
agent-bridge peers share OTHER_MACHINE_PEER_ID --agent LOCAL_SESSION_ID
agent-bridge list --remote
```

Direct manually configured IP routes and LAN routes remain available. A manual
circuit route can be configured with `peers add` using the complete pinned relay
and final endpoint path. Its operator must already be explicitly configured; a
manual hint does not configure or trust an operator. Removing that operator also
disables retained manual circuits through it. Automatic public routes accept only canonical circuits
through the currently configured operators with a final peer ID matching the
signed registration. Rendezvous cannot inject arbitrary direct dial destinations.

Message durability, deduplication, session pinning, delivery status and correlated
replies are unchanged. A relay only forwards live traffic and does not store
messages for offline recipients. The sender retains pending messages. After
remote revocation, an origin may only see a disconnected/offline peer and retain
its unacknowledged intent; a closed connection cannot communicate the remote
owner's policy decision. Revocation at the destination blocks further access and
clears that identity's grants. Already submitted native work cannot be recalled.

## Encryption and metadata

Direct endpoint traffic uses authenticated encrypted QUIC. A relayed circuit uses
an endpoint-to-endpoint Noise handshake and encrypted stream, carried over the
clients' outer QUIC connections to the relay. The actual endpoint key is
independently authenticated; decrypting outer QUIC as the operator does not reveal
the inner pairing or message stream. See [libp2p circuit relay](https://libp2p.io/docs/circuit-relay/).

Operators can observe connecting IPs, peer identities, destinations, traffic sizes
and timing. Rendezvous registrations expose public identities and routing hints
to discovery queries. The client does not register agent names, adapter sockets,
working directories or conversation content. This is not an anonymity network.

Signed peer records prove key ownership, not owner intent or record freshness.
An operator can omit or replay hints; cache TTLs run from receipt rather than
providing cryptographic freshness. Stale hints can fail or direct attempts to a
formerly reachable path, but cannot replace endpoint key authentication or SAS
owner approval. A malicious relay can deny service; it cannot grant itself trust.
The same-OS-user and administrator endpoint-access boundaries remain unchanged.

## Validation

The normal format/test/Clippy checks include relay configuration, migration,
operator role isolation, actual reservations/discovery, quotas, untrusted-protocol
separation, relay-only SAS pairing, shared/private sessions, durable ACK/reply,
revocation and independent-operator fallback.

An optional Linux acceptance harness creates user/network namespaces, SNAT and a
firewall that blocks direct client-to-client traffic. All networking changes stay
inside the new namespaces; host interfaces/firewall and live bridge state are
untouched. It requires `unshare`, `nsenter`, `ip`, `nft` and unprivileged namespaces:

```sh
cargo build --locked
python3 tools/test_relay_nat.py --binary "$PWD/target/debug/agent-bridge"
```

It simulates both owners' comparison/approval only for disposable test brokers,
records logs/firewall/results under `/tmp/ab-nat-qa-*`, and cleans up its processes.
This proves relayed operation across isolated networks behind NAT, not a physical
WAN or supported-platform acceptance matrix. Live cross-internet/macOS checks and external security audit remain pending.
Milestone 5 adds separate simulated blocked-UDP TCP acceptance.

The vendored rendezvous dependency includes documented bounded-state/frame fixes.
Run its standalone checks from the README when changing that patch; application
checks do not cover dependency unit tests. Source/version/license are preserved in
`vendor/libp2p-rendezvous` and its `PATCHES.md`.

Before upgrading existing state to schema 8, stop the service and back up the whole
state directory, including WAL/SHM files. Rollback requires the pre-upgrade state.
See [upgrade guidance](peer-trust.md#upgrade-and-rollback).
