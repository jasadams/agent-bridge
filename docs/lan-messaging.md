# Automatic LAN discovery and pairing

Enable LAN mode to find nearby Agent Bridge instances without entering their IP addresses. Discovery supplies unverified connection hints. Owners still verify identities, pair both bridges and grant access to individual agent sessions before messages can flow.

This milestone uses IPv4 mDNS multicast. Both machines need a multicast-capable local network and reachable UDP listeners. IPv6-only discovery, discovery across routers, NAT traversal and relays are not implemented. Explicit-address IPv4 and IPv6 connections remain available through [manual peer configuration](explicit-peer-messaging.md).

## Enable LAN mode

On both machines, with the broker running:

```sh
agent-bridge network enable --lan
agent-bridge status
agent-bridge peers discover
```

The default listener binds all IPv4 interfaces on an available UDP port. A specific local address or fixed port can be selected with `--listen /ip4/192.168.1.20/udp/44001/quic-v1`. The enable response records the configuration; status reports listener readiness, errors, connected identities and current discovery hints. [Machine names](machine-names.md) default to the short hostname and can be changed with `machine set-name`.

LAN mode and networking enablement persist across restarts. Schema 4 introduced opt-in discovery; schema 5 adds machine display names while preserving network settings. Stop the service and back up the whole state directory before upgrading, as described in [identity and trust](peer-trust.md).

To run explicit-address networking without multicast, use `network enable --listen ADDRESS` without `--lan`. To stop listeners, discovery advertisements and transfers, use `network disable`. Disabling preserves queued transfers and manual addresses. `peers discover` immediately returns an empty list once the persisted mode is disabled, while the worker shuts down its transport tasks.

## Verify and pair discovered peers

Discovery results include a claimed `peer_id`, its full `fingerprint`, address hints, and local `trusted` and `revoked` flags. `verified_advertisement: false` means the multicast announcement itself is unauthenticated. Even an already trusted identity can be claimed by a forged announcement; the QUIC handshake must prove possession of its private key before protocol requests succeed.

Compare the entire discovered fingerprint with `identity show` on the other machine through a trusted channel. Then pair on each bridge:

```sh
agent-bridge identity show
agent-bridge peers pair VERIFIED_PEER_ID --fingerprint VERIFIED_FINGERPRINT --alias workstation
```

No manual address command is needed when a paired identity has a live LAN discovery entry. Pairing grants no agent access. New or forged announcements cannot create trust records, replace aliases, reactivate revoked peers or inherit session permissions.

Advertisements contain public identity, connection hints and an optional unverified machine display name under the `_agent-bridge._udp.local` service name. They contain no agent names, model conversation content, working directories or local socket paths. LAN participants can observe bridge presence, identity, name and advertised addresses.

## Recommended agent-assisted pairing

Use [owner-approved LAN pairing](lan-pairing.md) for the agent-organized flow:
open a short window on the receiver, connect by discovered name, accept the
request and have both owners compare and confirm the verification code. No SSH
or long-token copying is needed. Pairing grants no agent access.

## Manual mutual pairing

Use the [single-use mutual pairing workflow](secure-pairing.md) for new machines:

```sh
# On machine A:
agent-bridge peers offer --seconds 120
# On B, pass A's offer on stdin and verify A's full fingerprint:
agent-bridge peers join --fingerprint VERIFIED_A_FINGERPRINT --alias workstation
# On A, pass B's response on stdin and verify B's full fingerprint:
agent-bridge peers finish --fingerprint VERIFIED_B_FINGERPRINT --alias laptop
```

Transfer tokens and compare fingerprints through a trusted channel. The issuer can
finish an offer for only one responder and can cancel it with `peers cancel-offer NONCE`.
No network listener or session grants are enabled by pairing.

## Legacy one-way invitations

Invitations provide a signed alternative for carrying identity and current address hints between owners:

```sh
agent-bridge peers invite --seconds 120
```

The result contains an `invitation` token and the issuer's complete fingerprint. Transfer the token, independently compare the fingerprint, and accept it on the other bridge:

```sh
agent-bridge peers accept INVITATION_TOKEN --fingerprint VERIFIED_FINGERPRINT --alias workstation
```

Repeat in the reverse direction for mutual pairing. Accepting an invitation creates no agent grants and imports at most its first signed address hint, only if no manual address already exists for that peer. LAN discovery can provide newer addresses later.

An invitation is valid for acceptance for 30–600 seconds, with a default of 120 seconds. Its signature binds the issuer's key, identity, timestamps, nonce and address hints. Both clocks must be reasonably accurate; issue times more than 30 seconds in the future are rejected. Expiry does not revoke a pairing that was already accepted.

Each token can be accepted once per receiving bridge database, including across restarts. Replaying a consumed token cannot restore a revoked pairing. There is no global one-use registry: separate owners can each accept the same signed token after their own fingerprint verification. Expired records are pruned during acceptance, and at most 1024 unexpired consumed-token records are retained.

Pairing stays an explicit local administrative action. There is no unauthenticated remote pairing-request endpoint or remotely opened approval window. The expiring invitation is the bounded pairing window; it proves key ownership but does not replace owner verification or native application permissions.

## Permit sessions and exchange messages

On each machine, grant permissions for its own local agents:

```sh
agent-bridge list
agent-bridge peers allow workstation --agent reviewer --permission discover
agent-bridge peers allow workstation --agent reviewer --permission receive
agent-bridge peers allow workstation --agent reviewer --permission send
agent-bridge list --remote
agent-bridge send --from reviewer --to workstation/reviewer --message 'Please review this change'
```

Discovery, receiving and sending permissions are independent. A default `--from user` needs its own grants. Private sessions remain absent from remote directories. Friendly peer aliases are local labels; queued work pins the peer identity and destination session rather than following later alias changes.

Remote directories normally refresh within five seconds. Native envelopes include reply references using the receiving bridge's local message UUID. Send and continue working; use replies for useful results or questions. See [delivery diagnostics](delivery-outcomes.md) if delivery fails or a response is missing.

## Reconnect and revoke

Live discovery hints expire after 30 seconds without refresh. The user-visible cache holds up to 256 identities and four eligible addresses per identity. Automatic routes use private or link-local IP QUIC hints; public, loopback, unspecified and multicast destinations are rejected as automatic LAN hints. Manual routes have their existing validation rules and survive discovery expiry.

Trusted peers reconnect automatically after address or port changes. A manual address remains the first candidate, with up to four discovered candidates as fallbacks. A new route resets stale connection backoff. Hints are ephemeral and are not saved as manual addresses merely because a device was discovered or paired.

```sh
agent-bridge peers revoke workstation
```

Revocation disconnects the identity, clears grants and permitted remote directories, and blocks pending work. Its advertisement may remain visible with `revoked: true`; visibility cannot undo revocation. Restoring access needs explicit pairing and fresh grants. Already submitted native work cannot be recalled.

## Troubleshoot and validate

If discovery is empty, confirm both bridges use LAN mode, have a non-loopback IPv4 listener, share a multicast-capable LAN, and allow IPv4 mDNS on UDP 5353 plus the configured QUIC UDP ports. Guest Wi-Fi isolation, VPNs and segmented networks can prevent discovery or direct connections. Use manual configuration where multicast is unavailable. Identity mismatch errors require checking fingerprints and addresses.

The mDNS dependency has a documented [local patch](../vendor/libp2p-mdns/PATCHES.md) for task cancellation, bounded metadata and expiry ordering. Source provenance and the upstream MIT license are retained. The patched dependency has its own tests in addition to the application's tests.

Environment-sensitive tests are separate from the default suite:

```sh
cargo test --locked --test lan_brokers -- --ignored
cargo test --locked --test lan_discovery -- --ignored
cargo test --offline --locked --manifest-path vendor/libp2p-mdns/Cargo.toml --features tokio --lib --target-dir target
```

Two brokers on one physical host can validate actual multicast, encrypted messaging, port-change recovery and advertisement expiry. The implementation plan additionally requires two physical machines without internet access before calling the LAN release candidate accepted. That separate check must include discovery, verified pairing, private-agent exclusion, an allowed message and correlated reply, restart recovery, revocation and disablement.
