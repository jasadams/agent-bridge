# Agent Bridge

**Connect your AI agents. Coordinate their work. Keep the whole picture in view.**

Agent Bridge lets **Codex and Claude Code sessions collaborate on one machine or
across a peer-to-peer network**. Agents exchange messages, hand off work and keep a
shared Markdown board up to date while you continue using their normal interfaces.
Use the live terminal board to follow progress and answer agent questions with
checkboxes and text responses, or inspect message delivery in the browser monitor.

Run it on Linux or macOS with a single binary. You control which machines are
paired and which individual sessions can communicate.

## What you can do

- **Coordinate agents across machines.** Automatic LAN discovery, NAT traversal
  and configurable self-hosted relays provide direct connections with encrypted
  relay fallback. There is no central account or mandatory hosted service.
- **Keep a live shared board.** Give agents a Markdown table, checklist or project
  outline to maintain, and answer questions directly in the terminal. Agents can
  subscribe to changes; revision checks protect concurrent edits. Board contents
  and layout are yours to define.
- **Give agents structured tools.** The built-in MCP server provides messaging,
  pairing, session sharing and board editing through a persistent local connection.
- **Stay in control of access.** Persistent machine identities, owner-approved
  pairing and explicit session grants determine who can communicate. Peer
  connections are encrypted; native agent permissions remain in effect.
- **See what happened.** Durable message history, acknowledgements, correlated
  replies and a local browser monitor make communication inspectable. Its Boards
  view also lets you answer existing checkbox and text response controls.

The shared board is currently local to one machine. Peer messaging supports
communication between paired machines.

## Install

Run as your normal user, without `sudo`:

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/jasadams/agent-bridge/main/install.sh | sh
```

The installer downloads a prebuilt executable, verifies its checksums and starts a
user service. No Rust or Node.js installation is required.

```sh
export PATH="$HOME/.local/bin:$PATH"
agent-bridge status
```

Add `~/.local/bin` to your shell's PATH to keep the command available in new
terminals. The service starts at user login using systemd on Linux or launchd on
macOS.

| Platform | Architectures | Requirement |
| --- | --- | --- |
| Linux | x86_64, ARM64 | glibc 2.35 or newer; systemd user session for the managed service |
| macOS | Intel, Apple Silicon | macOS 13 or newer |

See [installation options](docs/install.md) for custom state directories,
binary-only installation, source builds, backups and removal.
The installer selects the latest published semantic version. Agent Bridge currently
uses `0.x.y` versions; check [release notes](https://github.com/jasadams/agent-bridge/releases)
for compatibility changes before updating integrations.

## Connect your agents

Add Agent Bridge as a local MCP server in your agent application.

**Codex**

```sh
codex mcp add agent-bridge -- "$HOME/.local/bin/agent-bridge" mcp
```

**Claude Code**

```sh
claude mcp add --transport stdio --scope user agent-bridge -- "$HOME/.local/bin/agent-bridge" mcp
```

Refresh or restart the client as needed, then use `/mcp` to check the connection.
Your client's normal tool approvals still apply. See the [MCP guide](docs/mcp.md)
for configuration and the complete tool list.

Loaded Codex sessions are discovered automatically. To register Claude Code
sessions for native message delivery, install the lifecycle hooks and approve
them through the client's normal trust flow:

```sh
agent-bridge hooks install
```

Open or restart the relevant session after installing hooks, then inspect the
available sessions:

```sh
agent-bridge list
```

Agent Bridge uses your existing agent sessions and makes no model API calls.
[Native session setup](docs/native-sessions.md) explains registration, names and
permission troubleshooting. The opt-in [OpenCode V1 plugin](docs/opencode.md)
adds exact-session messaging on the same machine with OpenCode 1.18.35; remote
OpenCode sharing and busy/restart recovery remain follow-up work.

## Open your shared board

```sh
agent-bridge board
agent-bridge board --create streamr
agent-bridge board streamr
agent-bridge board --list
```

Then ask your agents:

> Keep our shared board up to date with a Markdown table. Include one row per
> agent with its task, status, whether it needs me, and a brief note. Update your
> own row when work changes and preserve other agents' entries.

Agents use `read_board` and `update_board` to apply atomic, revision-checked edits.
Their optional `board` defaults to `main`; use `create_board` explicitly for other
boards and `list_boards` for compact metadata. Names are lowercase ASCII slugs
with letters, digits, `_` and `-` (1–64 characters, starting with a letter or digit).
Press `b` outside the terminal text editor to pick a board. Each board has its
own content, revisions and subscriptions; boards stay local to the selected broker.
The TUI renders the document live, including Markdown tables. Use arrow keys or
`j`/`k` to scroll, `w` to toggle wrapping, and `q` to quit.

The board is a general Markdown document: use it for any structure you and your
agents find useful. Answer task checkboxes and marked text fields directly in the
TUI; agents can opt into debounced change notifications through MCP. See
[shared board usage](docs/board.md).
Use Tab to select a response, Space to toggle a checkbox, and Enter to edit a text
field. Ctrl-S saves the text response and Escape cancels the draft.
Drag to select board text and release to send it to the terminal clipboard. Press
`m` outside the text editor to switch back to the terminal's own mouse selection.

## Connect another machine

Install Agent Bridge on both machines and enable local network discovery:

```sh
agent-bridge network enable --lan
```

Ask the receiving agent to allow pairing, then ask the other agent to connect to
that machine. Each agent shows its owner a verification code and asks for local
confirmation. Compare the codes independently before approving.

After pairing, choose the specific sessions to share. Pairing establishes machine
trust; session sharing grants communication access. Other sessions stay private.
With MCP configured, tell each agent **“Share this session with workstation”**,
using the paired machine's alias or peer ID. Agents use `share_session` for their
own registered session. To find remote sessions, they call `list_agents` with
that explicit peer; without a peer, the tool lists local sessions only.

- [Pair machines on a local network](docs/lan-pairing.md)
- [Choose and share sessions](docs/session-sharing.md)
- [Connect over the Internet](docs/internet-invitations.md)
- [Run your own relay](docs/public-relays.md)

## Monitor messages and boards

```sh
agent-bridge monitor enable
```

Open the printed local URL and enter the separate access token. Choose Messages
for read-only message history or Boards to browse named local boards and answer
their existing checkbox/text controls. It listens only on loopback and is enabled
independently of peer networking. It cannot send messages, pair machines or edit
arbitrary Markdown.

```sh
agent-bridge monitor disable
```

See [message monitoring](docs/local-monitor.md).

## Command-line messaging

The CLI is available alongside MCP:

```sh
agent-bridge list
agent-bridge send --to AGENT_NAME --message 'Please review the latest changes.'
agent-bridge history --agent user
agent-bridge get MESSAGE_ID
```

The default sender is your `user` inbox. An agent sending a message uses its
registered name with `--from AGENT_NAME`. For sensitive or multiline text, pipe
the body to `send` instead of passing `--message`.

A submitted message has reached a transport or inbox. An acknowledgement confirms
receipt; task completion is communicated by a reply. See [delivery and trust](docs/peer-trust.md).

## Update

The board shows a quiet notice when a newer stable release is available. Agent
Bridge checks once a day; it never installs an update automatically.

```sh
agent-bridge update --check
agent-bridge update
```

The update command shows the versions and release-notes link, then asks for
confirmation before restarting the managed service. You can also run the install
command again. Managed updates back up the executable, service
configuration and private state, preserving machine identity, message history and
session grants. A failed startup restores the matching previous executable and
state.

Reconnect the Agent Bridge MCP server, or restart your agent client, to load the
updated executable and tool definitions after an upgrade.

[Release downloads](https://github.com/jasadams/agent-bridge/releases) include
platform binaries, checksums, dependency notices and corresponding source.

## Documentation

| Guide | Contents |
| --- | --- |
| [Installation](docs/install.md) | Requirements, updates, backups, source builds and removal |
| [MCP tools](docs/mcp.md) | Client setup and structured agent tools |
| [Shared board](docs/board.md) | Markdown editing, concurrency and live terminal controls |
| [Native sessions](docs/native-sessions.md) | Codex discovery, Claude registration and permissions |
| [OpenCode](docs/opencode.md) | Pinned V1 plugin, local live sessions and receipt limits |
| [Pairing](docs/lan-pairing.md) | Owner-approved machine connections |
| [Session sharing](docs/session-sharing.md) | Access to selected local sessions |
| [Local monitor](docs/local-monitor.md) | Message history and named-board responses in the browser |
| [Trust and security](docs/peer-trust.md) | Identities, grants, revocation and recovery |
| [Security review](docs/security-review.md) | Review scope, fixes and remaining validation |
| [Releases](docs/releasing.md) | Semantic version tags, build artifacts and publication |

## Build from source

A Rust toolchain and the platform C build tools are required:

```sh
git clone https://github.com/jasadams/agent-bridge.git
cd agent-bridge
cargo build --release --locked
./target/release/agent-bridge service install
```

[Report an issue](https://github.com/jasadams/agent-bridge/issues) or submit a pull
request on GitHub.

## License

Agent Bridge is licensed under [GPL-3.0-or-later](LICENSE). Dependencies retain
their original licenses; see [third-party notices](THIRD_PARTY_NOTICES.md).
