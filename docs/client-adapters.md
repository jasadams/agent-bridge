# Built-in native client adapters

`src/clients/` is the integration boundary for Claude, Codex and OpenCode. Its explicit
`Adapter::for_provider` dispatch selects native session and endpoint validation,
message transport, lifecycle registration, and capabilities. Inbox has no native
adapter: its history and message availability remain broker-owned.

The Claude adapter owns private Unix socket validation and user-frame generation.
The OpenCode adapter uses its pinned V1 plugin private socket, verifies exact live
sessions and application insertion receipts, and shares native title metadata.
Its durable canonical queue, generation leases and admission remain broker-owned;
typed OpenCode controls enter that same queue with their raw control text.
Direct adapter delivery without the broker's durable context fails closed. See
[OpenCode recovery invariants](opencode-recovery.md).
OpenCode uses the same explicit paired-peer session grants as the other clients;
see [OpenCode setup and limits](opencode.md).
The Codex adapter owns private daemon initialization, loaded-session checks, and
fresh capability/turn lookup and a single native input write: qualified active
threads use steer; other compatible threads use the queue. Canonical bounded broker
admission also covers typed controls; see [Codex delivery](codex-delivery.md).
Discovery observers and hook/context generation live in this
module too. Discovery only observes existing sessions; it never starts or resumes
a model session. Public `delivery`, `discovery`, `claude_discovery`, and `hook`
entry points remain available for compatibility.

Local broker registration validates the selected client's endpoint and session.
Database registration independently checks session identity, including discovery
registrations. Remote directory/envelope validation reuses the same client session
rules, keeping its additional length and control-character restrictions; remote
records never carry a native socket endpoint. Existing UUID syntax acceptance,
provider strings, stable routing names, and message envelope formats remain
unchanged.

Claude ordinary messages retain its native peer wrapper, sanitized sender display
label, and compact stable references; Codex retains the shared peer envelope and
acknowledgement guidance. OpenCode ordinary messages use a sender-labelled body
with complete compact route/message references. Its V1 system context hook
supplies session-specific messaging guidance separately from peer message text.
Locally generated `ControlNotice::BoardChanged` and `ControlNotice::Pairing` travel
through a separate typed entry point and retain their existing text without a peer
envelope. Board subscriptions require the control-notice capability. Pairing
selectors additionally require `PairingPrompts`, supported by Claude and Codex;
OpenCode retains board controls but rejects native pairing prompt selection with
an explicit CLI fallback. Both require an enabled exact registration and their
existing local opt-in checks. Capabilities
do not grant peer permissions, owner approval, or acknowledgement authority.

Durable queues, reply correlation, grants, network routing, retry decisions, board
storage, and owner approval stay in shared broker/database/network modules. Native
adapters report the existing failed/submitted/uncertain outcomes without turning
submission into a claim of model execution or recipient acknowledgement.

To integrate another built-in client, extend the explicit provider/adapter dispatch,
its native validation and transport, lifecycle/discovery wiring, and capability
mapping. Notification providers must use capabilities rather than new provider
whitelists. Add adapter contracts alongside `tests/clients_contracts.rs` and the
private `clients` tests; retain real socket and mock-daemon integration coverage.

Native display metadata is stored in `session_metadata`, keyed by provider and
native session ID. It is optional and independent of lifecycle registration:
metadata alone never enables a destination. Exact registered aliases take
precedence; otherwise display names resolve only to enabled automatic
`provider-session` routes. Use `provider:title` to qualify a title; duplicate
titles within that provider remain ambiguous. Renames update metadata without
changing route identity, messages, acknowledgements, or session grants.

Schema version 11 transactionally copies Claude and Codex titles (including NULL
and empty titles) into this shared table and removes their old tables. Reopening
is idempotent. Older binaries reject this newer numbered schema; downgrading
requires restoring the matching pre-upgrade database backup and binary together.
There is no automatic down migration or compatibility with simultaneous older
writers. Remote provider support is negotiated through an opt-in directory extension;
legacy directory responses retain only Claude, Codex and Inbox so strict older
provider enums remain readable. Capabilities never replace session grants.
Adding an adapter needs no separate title table or title lookup UNION.
