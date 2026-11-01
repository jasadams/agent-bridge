# Agent Bridge peer to peer implementation plan

**Current continuation:** the agreed [public P2P roadmap](public-p2p-roadmap.md) supersedes this document's remaining milestone sequence. Earlier implementation history remains in the progress ledger.

Add authenticated communication between bridges on different machines. Once networking is enabled, bridges discover nearby machines automatically, owners pair them once, and permitted agents can exchange durable messages. Internet connections support explicit addresses, NAT traversal and optional self-hosted relays. Identity and access decisions remain under each owner's control.

The first release should deliver the full LAN flow before adding internet traversal. Preserve existing local commands, native delivery adapters and the distinction between transport submission, agent acknowledgement and task completion.

## Scope and architectural decisions

Use Rust libp2p with persistent Ed25519 identities, QUIC, mDNS discovery and a versioned request-response protocol. Add AutoNAT, circuit relay and DCUtR in the internet milestone. Confirm compatible crate versions and enabled features in a small transport experiment before integrating them.

Libp2p supplies [local multicast discovery](https://libp2p.io/docs/mdns/), [QUIC transport](https://docs.libp2p.io/concepts/transports/quic/) and [relay-assisted connection upgrades](https://libp2p.io/docs/dcutr/). Its [Rust implementation](https://github.com/libp2p/rust-libp2p) is MIT licensed. These are building blocks; Agent Bridge must implement authorization, persistence and delivery semantics itself.

```text
Native agents                                  Native agents
     |                                              |
Private Unix socket                            Private Unix socket
     |                                              |
Local broker and SQLite <-- encrypted P2P --> Local broker and SQLite
     |                                              |
Local trust policy                             Local trust policy
```

Each service instance is a peer. Agents belong to that peer's namespace. A bridge authenticates its remote counterpart and vouches for the agent names originating there. This does not establish a separate cryptographic identity for each model session. Processes running as the local OS user retain the existing ability to act through the local bridge.

Networking is opt-in, with an explicit LAN enable command. Once enabled, discovery and reconnect are automatic. Direct addressing works without a bootstrap server, public DHT, vendor account or internet discovery service. Relay operators provide connectivity and cannot grant trust.

Defer global discovery, transitive trust, broadcast chat, attachments, third-party offline mailboxes and independently signed agent identities. Keep native application permission checks in force. Include an explicit project license in the release; MIT is the proposed default, subject to the owner's licensing choice.

## Identity and trust

Create one private key in the existing private state directory, with mode 0600 and atomic creation. Derive the peer ID from its public key. Restarting preserves identity; copying the state directory copies the identity and should be documented as a deliberate migration operation. Reject invalid or foreign-owned key files and never silently replace a damaged key.

Store peer aliases separately from peer IDs. An address or mDNS name is only a connection hint. Every connection must prove possession of the expected identity through the transport handshake.

Pairing has two supported paths:

1. Discover a peer locally, compare the full fingerprint through a trusted channel, and explicitly approve the peer and grants on each side.
2. Exchange an invitation containing the issuer's peer ID and connection hints. The recipient pins that identity; the issuer must separately approve the recipient's authenticated identity. An invitation alone grants no access.

Use expiring, bounded pairing windows and requests. Never grant access solely because a device shares the LAN. Advertise only protocol presence and connection hints before pairing; agent names, working directories, transcripts and socket paths remain private. mDNS still exposes the presence of a bridge and its discovery metadata to the LAN.

Grants are directional and local. Model discovery and message permissions separately, scoped to stable local session IDs and optionally a peer's claimed source agent. Start with an explicit allowlist of exported agents; new sessions are private until allowed. A peer's source-agent claims are assertions by that peer, not independently authenticated session identities.

Revocation closes active connections, removes cached exports and blocks subsequent ingress and egress attempts. Recheck policy when dispatching already queued inbound messages. Record blocked deliveries without erasing history. Revocation cannot recall a message already submitted to a native agent or committed on another machine.

For the first release, key replacement requires re-pairing. Later rotation can support transitions endorsed by the old key, while still allowing local refusal. No implicit trust inheritance or automatic approval of replacement keys.

## Network protocol and routing

Keep `src/protocol.rs` as the private local administrative API. Add a separate network protocol under `src/network/`, with a protocol identifier such as `/agent-bridge/message/1`. Remote peers must never reach local registration, hook installation, pause, arbitrary history reads or administrative configuration handlers.

Define bounded operations for capability negotiation, permitted agent discovery, message acceptance and delivery status synchronization. Reject unsupported major versions before accepting work. Use length-delimited frames, explicit timeouts and validated identifiers. Preserve the existing 64 KiB body limit and 256 KiB serialized frame limit, including envelope overhead.

The immutable message envelope contains:

- Origin peer ID and origin message UUID.
- Source agent ID, destination peer ID and pinned destination session ID.
- Body, parent reference as an origin-peer/message-ID pair, and reply hop count.
- Protocol version and an optional expiry policy if introduced later.

Derive the effective origin from the authenticated connection and reject conflicting claims. Accept messages only for the receiving bridge. Do not support application-level forwarding through other bridges. Circuit relays carry encrypted connections between the actual endpoints.

Use a canonical payload representation or compare all immutable parsed fields for duplicate detection. The deduplication key is `(origin_peer_id, message_uuid)`; conflicting reuse fails. Authenticate receipts and status updates against the peer and destination session associated with the original message.

Expose friendly remote destinations such as `workstation/reviewer`, while accepting a full peer ID and stable agent ID for unambiguous routing. Resolve aliases when enqueueing and persist the resulting peer and session IDs. Renames, alias reuse and reconnection must never redirect queued messages. Before sending to an unknown session, require a permitted directory response; offline sends can use a previously known pinned session.

Return only exports permitted for the caller. Refresh directories on connection and local registration changes; invalidate cached entries on disconnect and show them as offline rather than live. Never include local adapter endpoints in exports. Render remote names and message content as untrusted data, with safe escaping for terminals and native envelopes.

## Durable delivery and acknowledgements

Separate inter-bridge transport progress from native delivery state. A remote database receipt is not native submission, and native submission is not agent acknowledgement.

| Record | Proposed states | Meaning |
| --- | --- | --- |
| Outbound transfer | pending, awaiting receipt, accepted, rejected, blocked | Whether the destination bridge has durably accepted the envelope. |
| Destination delivery | queued, sending, submitted, acknowledged, failed, uncertain, cancelled | Existing native delivery semantics at the receiving bridge. |

Commit outbound work before attempting network I/O. The receiver authenticates the origin and checks current peer authorization, then looks up the deduplication key and compares immutable fields. An already accepted identical transfer returns its durable receipt even if the queue is now full or the destination session has disappeared. Revoked peers receive no receipt or status access. For new transfers, validate grants, capacity and destination pinning, then commit the inbound record and native delivery queue entry in one transaction. Send acceptance only after that transaction commits. Retransmission never schedules a second native delivery.

Retry ambiguous network transfers automatically with the same immutable identity and bounded exponential backoff with jitter. Resume after restart or reconnect. Keep retries bounded in concurrency and rate; offline peers must not cause a busy loop. Permanent rejection requires an explicit user action to retry. Local and per-peer capacity limits apply before durable acceptance.

Retain the existing rule that an interrupted native submission becomes `uncertain` and is never automatically replayed. Network retries may repeat the transfer but must not repeat native execution. Exactly-once model execution is outside the guarantee.

Persist destination status events with monotonically increasing sequence numbers. The origin acknowledges its last persisted sequence and resumes synchronization after reconnect. Display the timestamp of the latest remote observation so stale status is visible. Agent acknowledgement propagates as a status event without waking the sender; an explicit reply is a new message and can wake its destination.

Keep the 100-hop reply limit and validate parent references against known conversation records where available. Hop counts alone cannot prevent fresh-message loops, so add per-peer and per-recipient rate limits. Replies remain subject to directional grants; pairing setup should show whether both directions are permitted. Peers running older versions may still reject replies beyond eight hops.

For the first release, cancellation stops transfers that have not started. After an attempted transfer, a lost receipt makes acceptance ambiguous; report that ambiguity and do not claim remote cancellation. Accepted remote deliveries continue at the destination. A later protocol can add explicit cancellation with race-aware results.

Keep deduplication records while retries are possible. Before adding automatic history deletion, define tombstones and a bounded replay window; deleting history must not silently allow old transfers to execute again.

## Storage and code changes

Introduce numbered SQLite migrations before changing the schema. Existing local databases must open without losing registrations, history or queued messages. Migration-aware binaries must refuse schemas newer than they support. Existing releases lack that check: stop the old service and replace its executable before migration, and document that rollback to those releases requires restoring the pre-migration backup. Back up a stopped database before migration and document restoration; copying only the main SQLite file during live WAL writes is insufficient.

| Location | Responsibility |
| --- | --- |
| `src/network/identity.rs` | Key lifecycle, peer IDs and fingerprints. |
| `src/network/trust.rs` | Pairing, grants, revocation and policy evaluation. |
| `src/network/wire.rs` | Versioned envelopes, validation and bounded framing. |
| `src/network/transport.rs` | libp2p swarm, discovery, dialing and connection lifecycle. |
| `src/network/worker.rs` | Durable transfer scheduling, receipts and status synchronization. |
| `src/db.rs` and migration module | Schema versions, peers, grants, remote directories, transfers, inbound deduplication and status events. |
| `src/broker.rs` | Shared enqueue validation and integration with local native dispatch. |
| `src/main.rs` and `src/protocol.rs` | Local networking and peer management commands. |
| `src/delivery.rs` | Peer-qualified provenance and remote reply instructions. |
| `src/service.rs` | Network configuration loading and service lifecycle. |

Reuse native submission and recipient pinning code through explicit internal methods. Do not route remote input through the unrestricted local `Request` enum. Keep network I/O outside SQLite locks and use bounded channels between the swarm and persistence workers.

Proposed tables cover peers and aliases, directional grants, connection hints, exported session policy, remote agent cache, outbound transfers, inbound deduplication and ordered status events. Use explicit foreign keys and transactional state transitions. Keep local message UUID interfaces compatible by mapping remote composite identities to local record IDs.

## Configuration and diagnostics

Proposed CLI shape, to be finalized with parser and help-text tests:

```sh
agent-bridge network enable --lan
agent-bridge identity show
agent-bridge peers discover
agent-bridge peers pair PEER_ID --fingerprint FINGERPRINT
agent-bridge peers allow workstation --agent LOCAL_AGENT_ID --receive
agent-bridge peers allow workstation --agent LOCAL_AGENT_ID --send
agent-bridge list --remote
agent-bridge send --to workstation/reviewer --message 'Please review this change'
agent-bridge peers add PEER_ID --address MULTIADDR --alias workstation
agent-bridge peers revoke PEER_ID
agent-bridge network disable
```

Define `--receive` as permission for that peer to deliver to the named local agent, and `--send` as permission for that local agent to originate messages to that peer. Destination-side permissions still apply. Directory disclosure receives its own grant or an explicit documented implication of receiving permission.

Store configuration under the private state directory. Include listening addresses and ports, discovery interfaces, manual addresses, optional relay addresses, connection limits and retry settings. Networking disable stops advertisements, listeners and transfers while preserving local messaging and durable records.

Status should distinguish discovered, unpaired, trusted, connected, offline, blocked, incompatible and revoked peers. Show direct versus relayed connections, listening addresses, last connection error and queued transfer counts. Logs omit message bodies and private keys by default.

## Internet connectivity

First support a configured public address or DNS name with a pinned peer ID, configurable UDP port and documented firewall or router forwarding. Address resolution never replaces identity verification. LAN discovery remains independent of internet access.

Then add reachability checks, relay reservations and DCUtR attempts. Hole punching is best effort; restrictive NAT, carrier-grade NAT and blocked UDP may require relay transport. Validate a TCP-based relay path for networks that block UDP before promising that fallback. Report the selected path and actionable connection errors.

Make relay use explicit and configurable. Ship instructions for a separately operated open-source relay, with reservation quotas, connection limits and abuse controls. Relays transport end-to-end encrypted traffic but can observe endpoint/network metadata and traffic timing. They do not persist offline messages; the sending bridge retains those until the recipient reconnects.

Avoid automatic router port mapping in the first release. Add UPnP or PCP later only as an explicit option with visible mapping lifecycle and cleanup.

## Implementation milestones

Each milestone should land as reviewable changes with an independent written review. Re-review material fixes and require the repository's relevant checks before integration.

| Order | Deliverable | Completion criteria |
| --- | --- | --- |
| 0 | Baseline and transport experiment | Establish a committed default branch and remote if still absent; fetch it and create the implementation worktree. Read current guidance and ledger, record base commit and instruction paths. Pin compatible dependencies. Prove two isolated peers authenticate and exchange bounded frames. |
| 1 | Migrations, identities and trust | Existing databases migrate; identities survive restart; fingerprint pairing, directional grants and immediate revocation pass tests. Network remains opt-in. |
| 2 | Explicit-address messaging | Two brokers exchange messages and replies using configured addresses, real SQLite persistence and simulated native adapters. Lost receipts and restart do not duplicate native submission. |
| 3 | Automatic LAN experience | mDNS discovery, pairing workflow, permitted directories, remote names and reconnect work on two physical machines without internet access. This is the first LAN release candidate. |
| 4 | Internet connectivity | Manual public addressing, configured relay and NAT traversal work across separate networks. Tests distinguish direct, relayed and unreachable paths, including blocked UDP. |
| 5 | Release readiness | Failure tests, resource bounds, independent review, user documentation, license and live native exchanges pass. Publish only through the project's release workflow. |

After milestone 1, protocol validation tests and CLI documentation can be delegated independently. Keep schema ownership with one implementer; transport and persistence work must use agreed interfaces. Review and integration remain the coordinating agent's responsibility.

## Validation and release criteria

Run `cargo fmt --check`, `cargo test --locked` and `cargo clippy --all-targets --locked -- -D warnings`. Extend existing socket and durability tests rather than requiring model calls in CI.

Add integration tests for:

- Untrusted discovery, identity mismatch, forged origins, invitation replay and revoked active connections.
- Directional grants, private sessions, peer name collisions and session replacement while offline.
- Lost requests, lost receipts, concurrent duplicate sends, conflicting UUID reuse and receiver restart after commit but before response.
- Sender restart, status synchronization after reconnect, fast agent acknowledgements and uncertain native submission without replay.
- Oversized or truncated frames, unsupported versions, slow connections, queue exhaustion and bounded resource use under malicious peers.
- Existing database migration and schema refusal, key-file corruption, local-only startup and network disable during transfer.

Use deterministic loopback tests for protocol and persistence. Keep multicast and router behaviour in a separate environment-sensitive suite. Test both origin and destination roles and assert that remote requests cannot invoke local administration.

Live acceptance requires two physical machines on a LAN and two separate internet networks. Demonstrate discovery and pairing, an allowed message and correlated reply, blocked access to a private agent, revocation, offline recovery and a real native acknowledgement. Record direct and relayed outcomes separately. Submitted status alone does not pass the native round-trip check.

The release must preserve local messaging when networking is disabled, require no central service for LAN or explicit-address connections, and document precisely which NAT scenarios require a relay or manual configuration.
