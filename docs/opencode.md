# OpenCode native messaging

The built-in `opencode` provider targets an exact existing OpenCode session, not
its build/plan mode. This integration supports OpenCode **1.18.35**, its **V1**
`@opencode-ai/plugin` and `@opencode-ai/sdk` packages at **1.18.35**, and Bun
**1.3.11**. The plugin package and lockfile are in `plugins/opencode/`.
Installation into the owner's configuration is separate work; this ticket does
not install a plugin, replace a service, or change native permissions.

The plugin uses the SDK client supplied by OpenCode. That client can use an
in-process fetch transport: `serverUrl` is deliberately not used to discover or
contact an HTTP server. No model API key belongs in the broker. Normal native
usage continues, and delivery never creates a replacement conversation or edits
transcript files.

## Live session scope

The plugin belongs to one OpenCode server/plugin instance and may observe several
live sessions. It does not advertise stored history at startup. Session creation,
chat submission, observed activity/status, and the runtime `tui.session.select`
event activate an existing session only after verification through the supplied
client. Title updates refresh metadata for sessions already observed as live;
renaming archived history alone does not register it. A TUI's selected session is
one source of activity, not a global single-session routing switch. V1 has no
reliable active-TUI-selection feed: selection paths without a server event become
visible on the next native prompt/activity. Session IDs
use `ses_` plus 26 ASCII letters/digits; titles are display labels, never delivery
identities. Routes are `opencode-SESSION_ID`; use `opencode:TITLE` for title lookup.
Duplicate titles remain ambiguous.

Deleting/archiving a session or disposing the plugin unregisters its live routes.
An abrupt process exit can leave a stale enabled registration. Its socket cannot
accept delivery, and a second enabled plugin endpoint for the same session is
rejected. Explicitly unregister the stale exact route before replacing it.
Automatic restart reconciliation and busy-session ordering belong to AGB-4.

## Private transport and receipts

Each plugin instance has a random UUID-named Unix socket in a private owner-only
directory. The endpoint is bound to that UUID, and every successful response must
match both the socket instance and exact session. Rust validates socket type,
owner, mode, private directory, path length and connected peer UID. The plugin
checks the connecting UID with the native Unix credential API and rejects
unavailable credential checks. It validates one bounded newline JSON frame per
connection, payload fields, UUID message IDs and live session membership before
calling the client. Unknown, archived and mismatched sessions fail closed.

The local broker operation `opencode_observe` accepts `session`, `endpoint` and
optional `title`; it probes that exact endpoint/session before registering the
stable route and storing shared native title metadata. Ordinary explicit
registration performs the same probe. Metadata alone cannot enable a session.
The transport accepts `{op:"probe",session}` and
`{op:"deliver",session,id,content}`. Successful receipts include `ok`, `session`
and `instance`; delivery receipts additionally include the broker `id` and
`native_message_id`. Frames are bounded to 256 KiB. Content is the shared peer
envelope with explicit acknowledgement and correlated reply instructions.

A broker UUID maps deterministically to `msg_` plus the lowercase UUID without hyphens.
The plugin retains each in-flight submission and checks the native message ID so
ordinary retries cannot create a second prompt. It never treats an uncertain
submission as permission to blindly repeat a prompt. This is ordinary retry
idempotence within the supported process lifetime, not durable crash recovery.

`session.promptAsync` returning HTTP 204 is **not** evidence of durable insertion:
the native handler starts a background prompt and returns before insertion can
finish. The plugin verifies the exact native user message through
`session.message` before emitting a successful application receipt. The broker
records that as **submitted**, never acknowledged or complete. A missing,
malformed, mismatched or lost receipt after writing is uncertain. A model must
explicitly call the existing MCP/CLI acknowledgement and send a reply preserving
the broker message ID as its parent; API acceptance never supplies that authority.
AGB-4 must add durable receipts, restart reconciliation and ordered busy-session
recovery without changing this distinction.

## Sharing with a paired machine

OpenCode sessions are private by default. After owner-approved machine pairing,
explicitly share the exact live registration on each machine:

```sh
agent-bridge peers share machine-b --agent opencode-SESSION_ID
agent-bridge list --remote
agent-bridge send --from opencode-SESSION_ID --to machine-b/REMOTE_AGENT_ID --message 'Hello'
```

The share operation grants discover, send and receive for the exact `opencode`
provider and native `ses_` ID. A title rename keeps that identity; a replacement
session cannot inherit it. The other owner independently shares their destination.
Unshare/revocation and disabled/replaced registration checks apply before queued
native submission. Replies pin both endpoints to the original provider/session,
even if a cached directory changes. See [session sharing](session-sharing.md).

Upgraded brokers negotiate provider support before dispatching an OpenCode source
or destination. A peer that successfully answers only the legacy directory is
reported as unsupported for OpenCode; work is not relabeled or reported delivered.
Legacy directories permanently omit OpenCode so older strict provider readers
can still discover Claude, Codex and Inbox. The protocol remains version 1 on
`/agent-bridge/message/1`; the opt-in `directory_supported` exchange adds bounded
explicit provider capabilities without granting session access. An extension
rejection or codec error alone does not prove lack of support: the broker first
requires a successful legacy directory response. The strict legacy directory
fixture is pinned to pre-OpenCode commit `8fde538`.

Schema 12 extends the strict session grant provider constraint to OpenCode while
preserving existing permissions. Older binaries reject this newer database schema;
a downgrade requires the matching pre-upgrade database backup and binary.

No API `tools`/permissions fields, permission hooks, approval responses, automatic
remote sharing or native peer cards are introduced.

## Validation

See the plugin README for frozen dependency installation, type checks, supplied
in-process-client tests, and the disposable native compatibility harness. Tests
use temporary directories and local processes; they do not use owner sessions,
configuration or credentials. Rust local transport coverage is in `tests/opencode.rs`; store/trust and
isolated two-broker remote coverage are in `tests/opencode_store.rs` and
`tests/opencode_network.rs`. These source checks do not establish live model
reasoning, physical cross-machine delivery or macOS runtime acceptance.
The deterministic native harness uses a local model fixture; it checks real
OpenCode/plugin/broker execution and message correlation, not an external model's
reasoning or the owner's production environment. Physical/native macOS runtime
acceptance remains separate from source checks and CI tests.

Baseline API sources: [V1 plugin interface](https://github.com/anomalyco/opencode/blob/v1.18.35/packages/plugin/src/index.ts),
[native identifier generation](https://github.com/anomalyco/opencode/blob/v1.18.35/packages/opencode/src/id/id.ts),
[plugin documentation](https://opencode.ai/docs/plugins/), and
[server messages](https://opencode.ai/docs/server/#messages).
