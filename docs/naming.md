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

## Exception

Topic posts keep their `author_display_name` snapshot: a topic is a historical record and shows the name the
author had when posting.

## Still to come

Payloads carrying agents as `{id, name, provider, machine}` (AGB-59), removing the web UI's name heuristics
(AGB-60) and accepting names as input (AGB-61).
