# Implementation status

The public repository starts with the complete current source snapshot. Private
local development history and machine-specific QA artifacts are not published.

| Milestone | Status |
| --- | --- |
| Persistent identity and owner-approved pairing | Implemented |
| Explicit session sharing and connection controls | Implemented |
| Configurable self-hosted rendezvous and encrypted relays | Implemented |
| NAT traversal and direct/relayed fallback | Implemented |
| Signed contact links/QR, DNS and encrypted TCP fallback | Implemented |
| Security hardening and release acceptance | Three internal agent review perspectives completed; broader acceptance remains |
| Same-binary local message monitor | Implemented |

The three scoped reviews identified and fixed native argument disclosure,
connected-socket user validation and retained inbound history growth. See
[the security report](security-review.md). This is not an external audit or a
vulnerability-free guarantee.

Remaining acceptance includes physical Internet/router testing, supported native
Codex/Claude and macOS checks, an external human security audit and a supported
history retention/compaction workflow preserving replay protection. Publication
does not imply production-readiness or completion of the remaining acceptance work.

The original code is GPL-3.0-or-later; third-party notices are retained.

## Portable installation and releases

A POSIX installer selects prebuilt Linux x86_64/arm64 and macOS arm64 binaries, verifies
checksums and retains source/license notices before delegating transactional
per-user service setup. Linux systemd and macOS launchd upgrades stop before
private backup and restore matching binary/state on failure. Existing state,
identity, grants and service configuration are preserved. Binary-only installs
are available without service managers.

CI tests/builds all three platforms before publishing an immutable semantic-version
release. Version tags supply the executable's version, and the installer selects
the latest published release or an explicitly pinned tag. Corresponding source
includes locked dependency sources and offline build configuration. The source
revision is exposed in CI-built status metadata. See [installation](install.md).
The project currently uses pre-1.0 semantic versions; publication does not imply
completion of the remaining security and physical-network acceptance work.

Native CI additionally exercises repeated threaded and separate-process identity
initialization with both process-local and filesystem serialization. Identity lock
creation uses exclusive creation followed by plain open for an existing lock,
avoiding the concurrent Darwin create-or-open path implicated in native CI failures.
A minimal
vendored relay fix prevents non-reservation connection closure from incorrectly
reporting a live reservation as closed; retained upstream tests and a focused
regression cover the event semantics. No relay wire protocol or trust policy is
changed.

## Native pairing prompts

A locally selected native session can receive bounded pairing control notices,
ask its owner Yes/No after independently comparing the displayed code, and receive
completion updates automatically. The notification path carries no arbitrary
remote message text and grants neither trust nor session access by itself. Manual
pairing remains available. See [owner-approved pairing](lan-pairing.md).

## MCP agent tools

The executable includes a persistent stdio MCP server using the official Rust
SDK. Typed messaging, connection, pairing and session-sharing tools forward to the
existing private broker without per-call CLI processes. Owner confirmation, trust
and grant checks remain in place. Native adapters still deliver incoming events;
MCP provides the structured command interface. See [MCP setup](mcp.md).

MCP agent discovery defaults to local registrations. Remote discovery requires an
explicit peer alias or ID and returns only that peer's permitted directory.
Agents can share their own exact registered session through the existing
session-sharing tools when instructed by their local owner; other and future
sessions remain private by default.

## Local shared Markdown board

One content-agnostic Markdown document is shared through local broker RPC and
revision-checked MCP edits. A live terminal interface supports Markdown task
checkboxes and marked free-form text responses. Local native sessions can opt in
to change notifications with a 3-second debounce and 10-second coalescing cap;
subscriptions expire with session lifecycle or broker restart. Storage,
editing and rendering are separated for later board/source selection; this
version does not synchronize boards between machines. See [shared board](board.md).

## OpenCode local sessions

A pinned OpenCode 2.2.5 V2 plugin and built-in local adapter register verified
live sessions, retain shared native titles and submit peer messages to exact
existing conversations with ordinary retry deduplication. Native insertion
receipts remain distinct from model read, task completion and correlated replies.
Canonical FIFO, bounded busy/permission deferral, generation leases, durable
receipt fences and read-only restart reconciliation are implemented. Real native
fixtures cover busy tools, unanswered permissions, pause/cancel and the residual
idle-to-busy race without cancellation or approval responses. OpenCode also uses
explicit paired-peer provider/session grants and negotiated remote capabilities. See [OpenCode integration](opencode.md) for scope and validation limits.
