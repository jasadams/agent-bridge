# Install Agent Bridge

The installer downloads a prebuilt executable and sets up a per-user service. It
does not need sudo, Rust, Node.js or a model API key.

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/jasadams/agent-bridge/main/install.sh | sh
```

The installer resolves the latest published semantic version once, then downloads
all files from that immutable release tag. Supported platforms are Linux
x86_64/arm64 with glibc 2.35 or newer, and macOS Intel/Apple
Silicon with macOS 13 or newer. Alpine/musl and older systems can build from source.
macOS binaries are ad-hoc signed, not Apple notarized.

The installer automatically removes group/other write permission from existing
user-owned directories along its installation paths inside your home, printing
each change. It does not change permissions recursively or modify directories
outside your home; unsafe external or foreign-owned paths are refused with their
path shown.

The executable goes to `~/.local/bin/agent-bridge`. Add `~/.local/bin` to your PATH
if it is not already present. Linux uses a systemd user service named
`agent-bridge.service`; macOS uses the `local.agent-bridge` LaunchAgent. They start
at user login. A Linux systemd user session must be available; headless/container
setups can choose binary-only mode and run `serve` under their own supervisor.

```sh
agent-bridge status
agent-bridge monitor enable
```

Open the monitor's loopback URL and paste its separate access token. Tokens are
not embedded in URLs. Peer networking and monitor access remain independently
controlled. A fresh installation enables neither peer networking nor the monitor.
Open an existing Codex or Claude session to use native delivery. Claude registration
hooks can be installed separately with `agent-bridge hooks install`; hook trust and
native permissions remain your choices.

## Agent tools

The installed executable also provides `agent-bridge mcp`, a persistent local
stdio server for structured agent tools. See [MCP setup](mcp.md) to register it with
your agent application. Installation does not automatically edit client MCP
configuration or change tool approval settings.

## Optional OpenCode integration

OpenCode **1.18.35** V1 can use the bundled native plugin and
existing Agent Bridge MCP tools. Installation is explicit and does not start a
service, OpenCode, a model or a package manager:

```sh
agent-bridge --state-dir /absolute/private/state opencode install
# Choose a particular supported JSON/JSONC configuration file.
agent-bridge --state-dir /absolute/private/state opencode install --config /absolute/opencode.jsonc
agent-bridge opencode uninstall --config /absolute/opencode.jsonc
```

The installer registers the current executable's absolute path for MCP and the
same broker state directory for both MCP and the plugin. Keep that executable at
its installed path. Repeat `opencode install` after upgrading the binary to update
the extracted plugin; repeat installation without changes is a no-op. Fully restart
the OpenCode process after installing, updating or removing the integration.
Existing running instances keep their loaded code. No client permissions, tool
approvals, peer grants or board subscriptions are enabled by installation.

On Linux and macOS the default global directory is `$XDG_CONFIG_HOME/opencode`,
otherwise `~/.config/opencode`. With multiple global files, the active `plugin`
array's file is selected to preserve inherited plugins. Otherwise the existing
`opencode.jsonc`, `opencode.json`, then legacy `config.json` is preferred; a new
installation uses `opencode.jsonc`. `OPENCODE_CONFIG` selects a custom file;
additional directory/inline configuration is checked for conflicting integration
entries. Project configuration is edited only through explicit `--config`.
An extensionless global `config` file contains legacy TOML that V1 can migrate
and merge after these JSON files. Setup refuses while that file exists: migrate
it with OpenCode, review the resulting JSON configuration, then retry. The installer
never edits or migrates that legacy file. Its appearance during an interrupted
setup also prevents automatic recovery from overwriting configuration.
An explicit file that would hide an existing global plugin array is refused with
the correct target path. The pinned V1 format uses singular `plugin` and a flat
`mcp` map; V2 configuration is outside this integration's supported scope.

JSONC comments, unrelated plugins/MCP settings and permissions are preserved.
Every changed existing file has an exact-byte backup beside it. Private ownership
and recovery sidecars also live beside the selected config. Existing unowned
integration entries and owner changes to managed entries are preserved and reported
as conflicts. If removal would discard an adjacent owner comment, move that comment
outside the managed entry before retrying. A concurrent edit or interrupted setup
is checked before recovery; retry never restores a whole backup over newer edits.

Plugin source and license assets are embedded in normal binaries and source
packages, then extracted into immutable hash directories beneath
`$XDG_DATA_HOME/agent-bridge/opencode` or `~/.local/share/agent-bridge/opencode`.
They live outside auto-discovered OpenCode plugin directories. Removal deletes
only the recorded config entries and ownership manifest; it retains immutable
assets for already-running instances, backups, broker state, history and grants.
Remove retained asset directories manually only after every session using them
has stopped. See [OpenCode transport and validation](opencode.md).

## Options and pinned builds

Pass installer arguments after `sh -s --`:

```sh
# Install the executable only, without touching services.
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/jasadams/agent-bridge/main/install.sh | sh -s -- --no-service

# Use a custom binary directory (binary-only mode).
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/jasadams/agent-bridge/main/install.sh | sh -s -- --no-service --bin-dir "$HOME/bin"
```

`--state-dir ABSOLUTE_PATH` selects service state. The existing default is
`~/Library/Application Support/AgentBridge` on both platforms, preserving deployed
identities and history. For an existing service using a custom state directory,
pass that same directory; the installer refuses ambiguous or mismatched service
state instead of risking a rollback against the wrong database.

`--version v0.2.0` pins a specific published semantic version. Choose the exact tag
from [release downloads](https://github.com/jasadams/agent-bridge/releases). The
latest release advances only after all platform builds and checks succeed.
The installer verifies that SOURCE_VERSION and the executable's reported version
match that tag. `agent-bridge --version` and `status.version` identify the running
version; `status.build_commit` identifies its source revision.

## Updates, backups and removal

The broker checks GitHub for a newer stable release once a day, beginning about
one minute after startup. Results and failed attempts are cached across restarts.
Offline checks do not interrupt messaging or board use. The board TUI shows a
quiet update notice outside the shared document; it never changes Markdown,
revisions or agent board-change notifications. `agent-bridge status` includes
cached availability under `updates` without making a network request.

```sh
# Check now, including when automatic checks are disabled; installs nothing.
agent-bridge update --check

# Show versions and a release-notes link, then confirm installation interactively.
agent-bridge update

# Persist the automatic-check preference for this state directory.
agent-bridge update --disable-checks
agent-bridge update --enable-checks
```

Use the global `--state-dir` option for a custom service state directory. Automatic
checks contact GitHub's public release API and can be disabled with the command
above; existing cached availability remains visible. Explicit checks still work
when automatic checks are disabled. Detection alone never installs software or
restarts the service, and notices are not sent into agent conversations.

`agent-bridge update` supports managed Linux and macOS installations at
`~/.local/bin/agent-bridge`. It requires a terminal and an affirmative response
before installation; noninteractive calls cannot approve an update. It runs the
installer bundled with the executable, pinned to the displayed release, reusing
download verification and service backup/rollback. For source builds, custom
binary paths or binary-only installs, rerun the original install command with the
same options instead. You can also rerun that command to update a managed install.

A managed upgrade stops the previous service,
creates a private backup of its executable, service definition and complete state
(including SQLite WAL/SHM and identity), replaces the executable atomically and
checks readiness. If startup fails, it restores the old binary and matching state
before restarting the prior service. Backup paths are printed and retained under
`~/.local/state/agent-bridge-backups/`. Treat backups as private identity material.
Existing service environment/configuration is preserved on compatible upgrades.
No peer grants, trust, history, hook settings or native permissions are reset.
Reconnect the Agent Bridge MCP server or restart its client after upgrading;
existing stdio connections keep the previous executable and tool definitions.

Binary-only installs retain the previous executable beside the destination.
They refuse to overwrite a configured managed installation, even when it is
stopped; choose a separate custom binary directory for a standalone copy. A binary-only update does not migrate state or restart a service.

```sh
agent-bridge service uninstall
```

Removal stops/unregisters the managed service and retains the executable, state,
history and backups. Hooks have a separate `hooks uninstall` command. Older binaries
must never open a newer schema; follow the [backup/rollback guidance](peer-trust.md#upgrade-and-rollback).

## Download integrity and licensing

The script uses HTTPS and verifies exact SHA-256 manifest entries for the binary,
GPL license, complete dependency notice bundle, source commit and source version
before executing anything. Checksums detect corrupt or inconsistent downloads;
they are not an independent publisher signature. As with any `curl | sh` command,
you are trusting this repository and its distribution account. You can download
and inspect the script before running it instead.

Versioned license notices are retained under
`~/.local/share/agent-bridge/licenses/COMMIT-VERSION/` (or inside a custom binary
directory's `.agent-bridge-licenses/COMMIT-VERSION/`). Each published build provides its full
corresponding source, locked dependency sources and offline Cargo configuration.
The original project is GPL-3.0-or-later; dependencies retain their own licenses.

## Build from source

```sh
git clone https://github.com/jasadams/agent-bridge.git
cd agent-bridge
cargo build --release --locked
./target/release/agent-bridge service install
```

Release builds use Rust 1.98.0. Source builds need the platform C toolchain. The service
can run without a Codex executable installed; private native delivery requires a
compatible running native daemon and an already loaded destination session.
For an exact tagged version, check out the tag and set
`AGENT_BRIDGE_BUILD_VERSION=0.2.0` when building. Downloaded corresponding-source
archives contain SOURCE_VERSION and preserve their release version automatically.
See [versioned releases](releasing.md).
