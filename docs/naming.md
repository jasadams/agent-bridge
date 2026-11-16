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
- A Claude name lookup runs a bounded `claude agents --json` (5 s deadline) per delivery.
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
request.

## Still to come

Removing the web UI's name heuristics (AGB-60).
