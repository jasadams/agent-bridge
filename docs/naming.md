# Naming

An agent has an **ID** and a **name**. They are not interchangeable.

- **ID** is identity. It is the exact route (`claude-<session>`, a registered alias, or `<peer>/<route>` for a
  remote agent). IDs route messages, authorize access, key stored state and address replies. They never change.
- **Name** is a label for people. It is mutable, need not be unique, and is never used to route or authorize.

## One resolver

`Broker::resolve_agent(id, remote_label) -> AgentRef { id, name, provider, machine, local }`
(`src/broker.rs`, type in `src/naming.rs`) is the only place that turns an ID into a name.

- It reads the native client on demand, the same source `list_agents` uses. There is no cache, no stored name,
  no timer and no polling. Call it once per request or delivery, for the agents involved only.
- Never call it while holding the database lock: native reads block.
- A registered alias is already the agent's chosen name and is returned as is.
- With no client name, the fallback is provider plus the last four characters of the session, for example
  `Claude 9f3c`. `naming::fallback_label` defines it once; no other code may reimplement it.
- Every name is passed through `naming::sanitize_label` (control, format and separator characters removed,
  80 characters, empty means no name). Names come from other programs and machines.
- `remote_label` is the untrusted label a remote sender attached to one message (read with
  `Db::inbound_source_label`); callers resolving a route that is not tied to a message pass `None`.
- A Claude name lookup runs a bounded `claude agents --json` (5 s deadline) per delivery. The send request itself
  starts none for an exact route: a send's reply shows the stored or fallback label for Claude agents.
- `AgentRef::label()` is what recipients see: `name` for a local agent, `machine/name` for a remote one.

## Envelopes

Claude, OpenCode and Codex deliveries show the sender's `label()`; the ID stays in the routing fields
(`from`, `Reply to`, `[peer … from=…]`) so replies are exact.

## Remote senders

The sending broker resolves its own sender once and attaches the result as the optional `source_label` on the
wire `Envelope`. It is untrusted and informational: the receiver sanitizes it again, shows it as
`machine/name`, and never routes on it. It is stored only inside the already-persisted envelope. Senders
without it (and routes with no label) fall back as above.

Compatibility: an `Envelope` with `source_label` is rejected by peers that predate it (strict fields), so
both brokers of a pair need this version for messages to flow. Envelopes without the field are unchanged.

## Input by name

`send --from/--to` and MCP `send_message` accept an ID or a name. `Broker::resolve_input` turns what was typed
into an ID before the duplicate check or any storage, so a name and its ID are the same send. An exact ID (a
registered alias, `claude-<session>`, or `<peer>/<route>`) always wins over a name. Otherwise the name is matched
exactly (after `sanitize_label`) against the names of reachable local agents, read on demand from the native
clients with `list_agents`' source; the no-name fallback (`Claude 9f3c`) is accepted too. Zero matches, several
matches (the error lists their IDs) are errors: it never guesses. A provider that cannot be read contributes no
candidates, exactly as in `list_agents`, and is named in the unknown-name error.
Replies keep using the exact `from` / `Reply to` ID. Remote agents are addressed by ID (`peer/route`) only, and only sessions seen in the last 7 days are
candidates. Names are accepted by `send` alone; `history`, topics, boards and grants still take IDs. If a send
was already stored and its name no longer resolves on a retry, the error says so (the outcome stays
uncertain, never "not persisted"): retry with the exact ID.

## Exception

Topic posts keep their `author_display_name` snapshot: a topic is a historical record and shows the name the
author had when posting.

## Payloads

Every API/MCP reply that names an agent also carries its resolved `{id, name, provider, machine}`
(`naming::AgentRef`; `local` is internal and not serialized), so clients never have to render a bare route:

- Message records (`send_message`, `get_message`, `list_messages`, history, retry/cancel, and the MCP/CLI
  board-input receipt): `sender_agent` and `recipient_agent`.
- Web `/api/messages` and `/api/messages/{id}`: `source_agent` and `destination_agent` on each message.
- `list_agents` rows: `id` (the route; `name` remains the route for compatibility) and a non-empty
  `display_name` (the client name, else the fallback). Remote directory rows add `id` (`<peer>/<route>`) and
  `display_name`.
- Topics already carry the author's name (`author_display_name`, see the exception above).

The broker resolves these after releasing the database lock, once per distinct agent (and, for a remote sender, per message label) per request. Each provider's native names are read at most once per request (one `claude agents --json`, one Codex,
one OpenCode read) and every agent in the response resolves from that request-local snapshot. If a
provider's read fails or is incomplete, that provider's agents show the generated fallback name for that
request. The reply to a send does not read Claude names at all, so a send is not slowed by a busy machine.

## Web interface

The web page labels agents only with names the broker resolved; it never builds a name from an ID. Message rows
use `source_agent`/`destination_agent`; topic posts use `author_name` (the author's snapshot name, else the
resolved `author_agent`, decided in Rust by `attach_topic_agents`) and the mention list uses `agent`. The web
topic reads resolve after releasing the database lock, and the mention list is read when the picker is used
(at most every 30 seconds), not on the topic refresh timer. A message end with no reference (an unresolvable
remote end) shows "Unknown agent", never its ID. The IDs the page still shows are in the message details panel,
the hover title of a topic author and the `@<id>` mention token it inserts.

Board message controls in the web interface are labelled with the owner's resolved name: `GET /api/boards/{name}`
returns `message_labels` (control key to name) from the same resolver, after the database lock is released. The
live board events carry no labels (their identity is polled every 500 ms and must not read native clients), so
a rename shows on the next board load.

The old `/api/agents` name list, the page's name polling and its ID-pattern heuristics are gone.
`tests/web/labels.js` (run by `tests/web_labels.rs`, needs Node) exercises the label helpers with ID-bearing
payloads, and `tests/monitor_http.rs` fails if the removed heuristics return. The terminal board composer
(`board_tui`, `board inspect`) uses the same labels: the broker's `BoardInspect` reply carries `message_labels`
from the same resolver (`Broker::board_message_labels`), computed in `Broker::request` after the database lock
is released, once per inspect request. The terminal never derives a name from the owner's ID or the marker's
text; a control the broker did not label shows "Agent", as the web page does.

The terminal composer polls the board every second and on file changes, so it does not ask for names on each
poll: `BoardInspect` carries a `labels` flag (default off) and the broker resolves `message_labels` only when it
is set. The viewer sets it for the first view of a board and whenever a message control it is showing has no name
it holds (a control is keyed by its ID, label, owner and the board's identity, so a new or changed control or
owner qualifies); every other poll reuses the names it holds. Nothing is cached in the broker. So a name shows
at first view, when the controls change, and when the board is reopened; an agent rename shows up at the next of
those, not on the next poll. Against a broker that rejects `BoardInspect` (the main board's legacy read) the
viewer has no names and every control shows an un-numbered "Agent". CLI `board --metadata --archived` and MCP
`inspect_board` set the flag, so their replies carry `message_labels`.
