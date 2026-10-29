# Capability-driven Codex delivery regression

Active routing follows the [native delivery contract](codex-delivery.md). Runtime
version strings are diagnostic metadata, not an allowlist. Explicit true direct
input capability, an exact loaded thread in the verified home, and the latest
valid active-turn precondition are required before the adapter selects one steer
write. Absent/false capability uses the queue; malformed or unsupported facts
fail safely. Post-write ambiguity never authorizes another write or method.

The [official App Server contract](https://learn.chatgpt.com/docs/app-server#steer-an-active-turn)
describes steering within the current turn using `expectedTurnId` and a matching
`turnId` response. Generated schemas from installed 0.165.0 and 0.166.0 have the
same used request properties and response. Their steer schema difference is an
unused optional `AdditionalContextEntry.name` field. Schema comparison supports
the protocol analysis; it does not replace runtime qualification.

## Reproduce the native proof

Use Linux, Python 3.11+ and already installed `websockets==15.0.1`. No tools,
credentials or owner configuration are installed or copied. Build a reviewed,
committed source tree with accurate metadata, then run the installed runtime:

```sh
AGENT_BRIDGE_BUILD_COMMIT=$(git rev-parse HEAD) cargo +1.98.0 build --locked
TMPDIR=/var/tmp python3 tools/codex-native-delivery.py --production \
  --broker "$PWD/target/debug/agent-bridge" --codex /absolute/path/to/codex
python3 tools/codex-native-delivery.py --verify-artifacts ARTIFACT_ROOT
python3 tools/codex-native-delivery.py --verify-negative-tests ARTIFACT_ROOT
```

The six cases cover idle delivery, active generation, held tool execution,
same-turn continuation, disconnected observation and actual queue saturation.
The capacity case removes only the adapter's direct-input capability metadata to
exercise the queue contract; this controlled projection is recorded separately.
The adapter's own reads and single write are reconstructed from actual proxy wire
frames, native history and model inputs. The fixture never interrupts, resumes,
cancels or starts work on behalf of the adapter. Only the disposable driver
creates its test threads and initial turns. MCP acknowledgement and correlated
reply are verified separately from native adapter acceptance.

The runner records the resolved runtime/broker paths, versions, SHA-256 hashes,
source commit/file hashes, exact IDs and timing. Production runs reject dirty
executable source and a broker built from another commit. Run state is private,
uses disk temporary storage, and is retained for independent offline checks.
Shared process cleanup preserves owned group IDs until final signalling, attempts
all teardown stages and treats any cleanup failure as nonpassing. It removes only
its own unique discovery socket, with identity checks.

## Initial regression and limits

Before the fix, the added 0.166 contract test exited 101: the actual selected
method was `thread/queue/add`, while active/capable delivery required `turn/steer`.
The retained `red-version-test.log` and the ticket's original submitted-message
observations preserve this failure. The initial version-allowlist plan was
superseded by the user's capability-driven requirement before production changes.
Regression examples now cover 0.165, 0.166, a synthetic future version, missing or
unusual version headers, and malformed protocol/error boundaries.

Historical [0.165 runtime evidence](codex-active-delivery-regression.md) remains
historical. New native measurements must retain their own source/runtime hashes;
an old result is never relabelled as a new-source pass. A scripted localhost
provider proves transport, safe-boundary consumption and tool correlation, not
external-model judgment. Native macOS, physical-network and interactive UI
qualification are outside this Linux fixture. Source qualification does not deploy
or replace the running owner broker, configure owner sessions, or publish a release.
