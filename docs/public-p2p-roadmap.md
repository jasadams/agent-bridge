# Public peer-to-peer roadmap

Agreed scope, 27 October 2026. Implement and review one milestone at a time.
The existing identities, QUIC networking, durable delivery, session grants,
LAN discovery and manual mutual pairing are the foundation. This roadmap
supersedes the remaining milestone sequence in the original P2P experiment plan;
it does not imply that public routing or the web monitor is already implemented.

Target experience: “Connect to this person”, both owners approve and verify the
intended identity, select sessions to share, and communicate without manual router
configuration. Agents organize the exchange; a remote request never authorizes
itself. Existing native agent permissions remain in force.

## 1. Agent-assisted LAN pairing

- A restricted pairing protocol on the existing encrypted QUIC listener allows
  unpaired machines to negotiate without access to messaging or administration.
- Owners explicitly open a bounded pairing window. Agents discover a target and
  carry the handshake automatically; no SSH or copying long invitation tokens.
- Use established short-authentication-string cryptographic building blocks,
  commitment sequencing and identity-bound transcripts. Both owners compare the
  code via a trusted channel and explicitly confirm locally before trust commits.
- Show pending requests and their states through local CLI/RPC. An untrusted
  request cannot contain arbitrary prompts, execute commands, or wake native
  agents repeatedly. Owners can ask their agents to inspect requests.
- Expiry, cancellation, request/concurrency limits, duplicate suppression and
  fail-closed restart behavior bound the pre-trust surface. No automatic grants.

Acceptance: real isolated brokers pair over the network with no SSH/token copying;
wrong-code, substitution, replay, one-sided confirmation, cancellation and expiry
cannot create unauthorized trust. Messaging/admin remain unavailable before
pairing. Physical two-machine/no-SSH user acceptance is recorded separately.

## 2. Session sharing and connection controls

- One explicit “share this session for two-way messaging” action applies the
  intended discovery/send/receive grants together, with clear scope.
- Show paired/pending/connected/offline states and shared sessions.
- Stop sharing, revoke and cancel independently. New sessions remain private;
  session replacement cannot inherit access. Reconnect preserves valid grants.

Acceptance: allowed message, native acceptance evidence and correlated reply work; private
sessions are excluded and revocation blocks further access immediately.

## 3. Public rendezvous and self-hosted relays

- An open-source, independently operable node provides rendezvous and libp2p
  Circuit Relay v2 reservations. Support multiple configurable operators.
- Carry bounded pairing traffic and end-to-end encrypted peer streams; identity
  verification, private keys and trust remain at the endpoints.
- Enforce quotas, reservation expiry, connection limits and abuse controls.
- No mandatory vendor account, central trust authority or globally unique hostname.

Acceptance: endpoints on separate networks pair and exchange traffic through a
chosen relay using outbound connections only; relay metadata exposure documented.

## 4. Automatic direct connections through NAT

- Add libp2p reachability detection, public-address discovery and DCUtR coordinated
  hole punching. Prefer direct connections and upgrade relayed paths when possible.
- Recover after network/IP/router changes. Show direct versus relayed transport.
- NAT traversal is best effort; relay fallback remains available.

Acceptance: a representative NAT matrix records direct success and relay fallback;
path changes preserve durable delivery, identity and destination-session pinning.

## 5. Internet invitations and restrictive networks

- Share connection links/QR codes with peer identity and rendezvous hints.
- Accept domain/IP contact locations without treating them as verified identity.
- Preserve owner verification when an address/name is the only initial hint.
- Add encrypted TCP relay transport for blocked UDP and fail over between configured
  relays. Retry direct connectivity without duplicating native delivery.

Acceptance: new owners connect without port forwarding across separate networks,
including blocked UDP, relay loss and public-address changes.

## 6. Security hardening and release acceptance

- Exercise identity substitution, hostile pairing, replay, exhaustion, relay
  interference, outages and privacy/metadata boundaries.
- Validate restarts, ambiguous delivery, deduplication and session pinning across
  transport changes; run supported OS/native agent acceptance.
- Obtain external security review and resolve material findings before strong
  public security claims. Publish self-hosting, privacy, upgrade and recovery docs.
- Use the project's release workflow when actually releasing software.

Acceptance: documented test matrix passes, outstanding platform limits are explicit,
and material security findings are resolved.

## 7. Local message monitor, embedded in the same binary

- Embed a lightweight HTTP server and all HTML/CSS/JavaScript assets in
  `agent-bridge`. No separate server process, Node.js runtime, CDN or runtime asset
  downloads. Read the existing broker message database.
- Show source/destination machine and session, delivery states, timestamps and
  correlated conversations. Filter by machine/agent/status/time; stream live updates
  without model calls.
- Start read-only. Bind loopback by default, authenticate browser access, reject
  cross-origin requests and DNS-rebinding attempts, and omit message bodies from
  HTTP logs. Never put browser access secrets in query strings or referrers.
- Enable/disable independently of peer networking. The loopback network boundary
  does not isolate against same-user processes or administrator access.

Acceptance: observe a local and remote exchange live and inspect history; other
machines and unrelated websites cannot read the monitor. A clean installation
serves the entire monitor from the single executable without internet access.

## Delivery discipline

Read the repository's current guidance and implementation status before changes.
Fetch the default branch and create an isolated task branch/worktree. Preserve
unrelated changes, independently review material changes and run the documented
format, test and Clippy checks. Record validation scope and remaining limits.
Source publication does not imply an official release or deployment.
