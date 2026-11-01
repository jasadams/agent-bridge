# AGB-33 OpenCode V2 validation

Executed on Linux with OpenCode **2.2.5**, `@opencode/plugin` **2.2.5**,
standalone Bun **1.3.11**, and native embedded Bun **1.4.4**. All native runs used
private disposable HOME/XDG/config/workspace/broker/session state and a scripted
localhost OpenAI-compatible provider. No owner credentials/service/state were
used. Native/broker children were stopped before returning each foreground result.
The standalone/setup checks do not establish external-model judgment, macOS
acceptance or the separate three-provider matrix recorded below.

## Results and exact provenance

| Check | Exit/result | Evidence |
| --- | --- | --- |
| `bun run typecheck` in `plugins/opencode` | 0 | Supported V2 plugin API; no plugin inbox cast or HTTP integration |
| `bun test index.test.ts --test-name-pattern 'V2 durable inbox proof'`, before repair | 1: 3 failed, 1 passed | Durable native inbox unavailable in transcript; ordinary transcript-only implementation |
| Focused receipt/legacy-upgrade/quota/concurrency regression selection | 0: 9 passed | Exact admission return, fresh reconcile nonce, changed generation, lost response, failed proof commit, old NULL proof and byte capacity |
| Final `bun test` | 0: 74 passed, 390 assertions | Foreground full plugin suite; no omitted failure counted as pass |
| Native attempt 7, transcript-only proof | 1 | `/tmp/opencode/aboc-native-0waQS3`, logs `/tmp/opencode/agb33-v2-native-attempt7`; held-tool message `2e211dc4-4232-495f-8793-a775da805329` stayed Unknown after durable native admission |
| Native attempt 8, supported receipt proof | 0: passed | `/tmp/opencode/aboc-native-BNIARb`, logs `/tmp/opencode/agb33-v2-native-attempt8`; 10 native model requests; ordinary input with zero recipient tools/replies; 127,753-ms paused live wait; held tool, permission, alias FIFO, stream and no-replay checks |
| Installed setup attempt 7 | 0: passed | `/tmp/opencode/aboc-setup-DbeszB`; 17 model/context requests; actual MCP discovery/reply/board update, compaction/reload, notification coalescing, unsubscribe, deletion cleanup and pairing preflight |

Commands for the two passing native runs, from the repository root:

```sh
AGENT_BRIDGE_NATIVE_LOG_DIR=/tmp/opencode/agb33-v2-native-attempt8 tools/opencode-native.sh /home/jason/repos/agent-bridge-agb-33/target/debug/agent-bridge
AGENT_BRIDGE_NATIVE_LOG_DIR=/tmp/opencode/agb33-v2-setup-attempt7 tools/opencode-setup-native.sh /home/jason/repos/agent-bridge-agb-33/target/debug/agent-bridge
```

Both runs started at source HEAD
`b11a50c9a651b5acbf803047b3520c05a64fb814` with dirty changes. This is **not** a
claim that HEAD alone identifies either executable. The native checkout-plugin
run used binary SHA-256
`4f6aa7965f24d375ef538678ab323dd9746fe44105839f6f8e227e01ad92a97d`.
The final installed-asset run used the rebuilt binary SHA-256
`64f899c90a1c1c7ad4ff3ae86b53ea6f4b51c8a5149b935c11c91e00a3d71049`.
Do not substitute the later hash for the earlier run or infer unrecorded binary
source equivalence. Actual broker status reported `build_commit: null`; the final
setup retains the complete raw `broker-status.json` rather than inventing a commit.

The final setup's `source.patch` SHA-256 is
`f68e1831d8eb4f44ea23c760f142ab0bb79dd7d922a9d9ebef922a3d291b6f43`.
Its extracted module hashes match the source modules used by the native run:

| Module | SHA-256 |
| --- | --- |
| `index.ts` | `06ae93044ff7371c2037eefa1701c0fc23c7c99e482c9412ab5d01fbbfff7d38` |
| `journal.ts` | `816da1581a56715e1b53865d34113fbf1c2adeeb967a06f0f1255dfdbd5b7cf5` |
| `rpc.ts` | `885215120dfd9b74f9dea7ef30ba5b073bb82de93f61f817552a45500ced23dd` |
| `peer.ts` | `7b6cb9c4aea67c8f7e0ee953d1238c7d9c3d2c58315c658b7cf8fec2d630fdbb` |

After these runs only documentation, shell comments and harness stdout summary
terminology/size changed; production proof and native assertions were unchanged.
Each run's recorded harness hash remains the actual executed version, not a
retroactively substituted current-file hash.

## Evidence boundaries and preserved failures

Native evidence includes `provenance.json`, installed server `openapi.json`,
`plugins.json`, exact real V2 `native-prompt-receipts.json`, held inbox snapshots,
model requests/events and `checks.json`. The ordinary-send case independently
checks the exact native user ID once, actual model-visible input, zero native
recipient tool calls and zero broker receipt replies. Later useful reply checks
are a separate fixture scenario, not a receipt requirement for ordinary messages.
Held native inbox acceptance and later transcript/model visibility are separate
assertions; Delivered is not a model-read/task-completion claim.

Installed setup retains original `installer-config.json`, `effective-config.json`
and `fixture-normalization.json`. The exact installer-generated isolated MCP
command/arguments and absolute extracted package are checked **before** explicit
initial fixture normalization to V2 `mcp.servers` and `codemode:false` direct tools.
This qualifies installed assets and that normalized disposable configuration,
not a claim that the installer itself emits native V2 MCP shape. Extracted
module hashes are compared with current source before execution.

Earlier failures remain retained: API probe authentication/schema inspection;
native attempts 1–6 exposed lazy project bootstrap, the 200-message page limit,
MCP startup/direct-tool mode and host plugin-module loading assumptions. Attempt
7 is the actual product-proof regression. Setup attempts 1–2 exposed installer
shape assumptions and an intermediate unsupported API implementation; attempts
3–6 exposed V2 compaction system-part/template and post-reload MCP-readiness
fixture assumptions. None is counted as a passing run. The unsupported plugin
inbox API was removed, not cast around or hidden behind a fallback.

The [provider proof inventory](delivery-outcomes.md#opencode-v2-proof-and-retained-journal-fields)
links versioned canonical durable-admission/transaction implementations and
documents original journal provenance versus fresh request nonce, additive
journal schema 2, quota accounting, historical rows and the lost-native-response
before-proof limit. Historical measured reports are not relabeled as fresh V2
evidence.

## Retained native matrix

The existing `tools/qualification_native.py` was ported **in place**, preserving
`Processes`, its Codex probe import consumer, all 13 original isolation/cleanup
tests and the existing runtime matrix. Four focused checks were added:
`python3 tools/test_qualification.py` exited **0, 17 passed**. No new qualification
framework, CI command removal, trigger or platform-matrix narrowing was introduced.

Final matrix attempt 05 exited **0** on Linux: six local round trips across Claude
Code **2.1.300**, Codex **0.166.0** and OpenCode **2.2.5**, plus two remote OpenCode
round trips over two real loopback-QUIC brokers. Each direction reached actual
native model input with exact references and one useful correlated MCP reply,
without recipient acknowledgement tools. Claude outcomes remained Unknown.
Default-private directories, independent exact-session grants, rename, broker
restart identity/pause/grants/history, plugin close/lease expiry, fresh generation,
exactly-once history and archive/delete fences passed. All owned children exited,
the HTTP thread stopped, the owned Codex socket was absent and cleanup errors were
empty.

```sh
python3 tools/qualification_native.py /home/jason/repos/agent-bridge-agb-33/target/debug/agent-bridge \
  --scratch-parent /tmp/opencode \
  --evidence /tmp/opencode/agb33-v2-matrix-20261101-05.json \
  --allow-dirty-source \
  --expected-binary-sha256 406c4764bb3293d6f616a681c0e6bea34c78135733650e09cbe0c1133b450232
```

The selected binary was rebuilt once with supported
`AGENT_BRIDGE_BUILD_COMMIT=b11a50c9a651b5acbf803047b3520c05a64fb814`, serially with
`CARGO_BUILD_JOBS=1`. This is HEAD **plus dirty source**, not a clean-commit claim.
Evidence and the source patch are
`/tmp/opencode/agb33-v2-matrix-20261101-05.json` and
`/tmp/opencode/agb33-v2-matrix-20261101-05.source.patch`; the patch digest is
`d42c12ae5a3690cfa2a085f5d26d3c878438c15d54ecd05ddc2a9ae87ddf151a`.
The executable digest is the command's `406c…` value, **not** either earlier native
run's digest. Captured manifests have identical hashes for all 68 compared Rust,
manifest/build-script and production-plugin source files between the earlier
selected-binary matrix attempt and this run; that comparison is source evidence,
not an assertion that HEAD alone identifies a dirty executable.

Attempt 01 exited 1 with **zero pairs** because the provenance gate rejected a
null embedded commit. It is not qualification. Attempt 04 reproduced an inherited
V1-only expectation that an unarchived session must remain inactive until explicit
UI resume. The final check instead uses the existing supported V2 contract:
canonical live API session plus a fresh, ready, unexpired owning plugin lease
establishes availability without a TUI. It captured unchanged native user inputs,
then one subsequent explicit input exactly once. Archived history ended its lease
and route without adding input. No production lifecycle policy changed.

These matrix checks use Python **3.14.4** and websockets **15.0.1**. Codex workflows
are unheld-turn scenarios; separate active-turn evidence is not implied. Native
macOS, physical cross-machine networking, interactive rendering and external-model
judgment remain outstanding. The captured 2026-10-29 report remains evidence of
its original runtime/commit, not this V2 run.
