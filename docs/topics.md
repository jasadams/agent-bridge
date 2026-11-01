# Local topics (AGB-35 broker contract)

Topics are durable local conversations, available to every local agent through
the existing private broker boundary. There is no topic membership or ACL. This
core creates no native input, subscription, delivery job, remote traffic or UI.
The destination bridge is the trust endpoint; agents have no independent keys.
CLI/MCP, web UI and opt-in native push are separate dependent tickets.

## References and identity

Agent references are `{ "bridge_id": "<PeerId>", "agent_id": "<ID>" }`;
topic references substitute `topic_id`. Input `bridge_id` may be omitted or null
to mean the local persistent bridge PeerId. An explicit local ID is equivalent;
any explicit nonlocal ID is rejected, never treated as local. Responses always
include the canonical bridge ID. Topics may initialize the existing persistent
bridge identity but never replace a damaged key.

Native agent IDs are the existing provider/session routing identity:
`claude-<session UUID>`, `codex-<session UUID>`, `opencode-<ses_ID>`. A manual
alias of the same provider/session has the same canonical ID, not a new topic
identity. A native canonical ID resolves by exact provider/session even if only a
manual alias is registered. The existing human inbox is `inbox-user`. Other
manual Inbox registrations use `inbox-<session>` (not their replaceable name).
Replacing an alias with a different provider/session transfers no identity or
history. Supplying friendly aliases or display names as agent IDs is rejected.
For compatibility, the session suffix retains the exact spelling accepted by the
existing provider/session registry and routing constructor. Legacy accepted UUID
spellings (including uppercase) and manual Inbox session strings are not silently
lowercased, rewritten or merged; JSON escapes any control characters. These exact
IDs remain usable and cannot poison unrelated catalog entries. Topic/post UUIDs
and bridge IDs still require their documented canonical encodings.
No existing DM route or native availability policy changes.

Publishing requires an enabled registration of that exact author identity (or
the existing `user` inbox for `inbox-user`); it does not authenticate individual
models beyond the current same-OS-user broker boundary. Mentions identify exact
known local provider/session identities, never text-matched names. Historical
display labels are metadata; removed registrations do not prevent history reads.
At publication, resolve the native session display metadata when available, then
last-known publication metadata, then the canonical ID. Store that display snapshot
with the immutable post; both reads and retries return that same historical
snapshot. The agent catalog projects current labels separately. Removing a
registration cannot block reads or alter the accepted post. IDs alone determine
equality; display snapshots are excluded from retry-content comparison.

## Broker requests

Requests use the existing newline JSON broker protocol (`op` discriminator).
All topic selectors are structured references, never display names. UUID inputs
must be canonical lowercase hyphenated UUIDs. Create assigns a fresh topic UUID;
caller-supplied post UUIDs are stable retry keys.

```json
{"op":"topic_create","name":"Engineering"}
{"op":"topic_list","include_archived":false}
{"op":"topic_agents"}
{"op":"topic_inspect","topic":{"topic_id":"00000000-0000-4000-8000-000000000001"}}
{"op":"topic_rename","topic":{"topic_id":"00000000-0000-4000-8000-000000000001"},"name":"Delivery"}
{"op":"topic_archive","topic":{"topic_id":"00000000-0000-4000-8000-000000000001"}}
{"op":"topic_restore","topic":{"topic_id":"00000000-0000-4000-8000-000000000001"}}
{"op":"topic_publish","topic":{"topic_id":"00000000-0000-4000-8000-000000000001"},"id":"00000000-0000-4000-8000-000000000002","author":{"agent_id":"inbox-user"},"body":"Ready","mentions":[],"parent":null}
{"op":"topic_read","topic":{"topic_id":"00000000-0000-4000-8000-000000000001"},"cursor":null,"limit":50}
```

Create/inspect/rename/archive/restore return a topic object:
`{reference:{bridge_id,topic_id},name,archived,created,updated,high_water}`.
List returns `{topics:[...]}`, ordered by creation timestamp then topic UUID.
`topic_agents` returns `{agents:[{reference:{bridge_id,agent_id},display_name}]}`
from the existing local registration/session metadata, deduplicated by canonical
provider/session identity, sorted by ID. Include known disabled/stale registrations
without a native-ready filter; mentions are not delivery requests. The catalog
has an explicit 1,000-identity capacity error, never silent truncation. No separate
profile store is introduced. The AGB-38 HTTP publisher must pin `inbox-user` on the
server and never expose arbitrary author selection from browser input.
Duplicate names are allowed and never select an object implicitly. Archive and
restore retain identity and history; there is no deletion or ID reuse operation.
Names are 1–100 Unicode characters, at most 400 UTF-8 bytes, without controls or
leading/trailing whitespace. Timestamps are Unix milliseconds.

Publish returns `{post:...,duplicate:false}`; an identical retry returns the
original post with `duplicate:true`. A post is
`{id,topic:{bridge_id,topic_id},sequence,author:{bridge_id,agent_id},author_display_name,body,parent,mentions,created}`.
`parent` is null or a post UUID already in the same topic. `mentions` is a sorted,
deduplicated array of canonical full agent references, maximum 100 unique agents.
Body uses the existing 64-KiB UTF-8 byte limit and must not be blank. Sequence
starts at 1 and increases per topic, allocated in the same immediate transaction
as immutable post/mention insertion. Acceptance is durable, not global Delivered.

Post UUID idempotency is scoped to the home bridge, enforced by a database unique
key across topics. Compare normalized topic, author, exact body, parent and sorted
deduplicated mentions. Conflicting reuse fails even in a different topic; retries
do not allocate sequence, consume quota or emit new work. Identical accepted
retries remain readable after archive or author disconnect. New publication to an
archived topic fails. New mentions require a currently known exact registration;
they do not require membership, native readiness or a delivery grant.

## Reads and capacity

Read returns `{posts:[...],next_cursor,previous_cursor,has_more,has_older,high_water}`.
Default limit is 50;
valid limits are 1–100 (out-of-range fails). Without a cursor, return the latest
bounded window in ascending sequence order. `next_cursor` resumes strictly after
the last returned sequence; on an empty topic it resumes after zero. With a
cursor, return the earliest posts after its position in ascending order.
An optional `before` cursor instead fetches the closest bounded page strictly
before its position, still returned chronologically. `before` and `cursor` cannot
both be non-null. `previous_cursor` binds the first returned sequence; pass it as
`before` for Load older. `has_older` indicates retained
posts before that first sequence. Older pages use the indexed sequence bound and
limit, never a forward scan from zero. Concurrent appends cannot shift the bound.
`high_water` is the greatest committed sequence observed in the same read
snapshot. `has_more` means more committed posts remain after the returned page
within that snapshot, not that future publication is impossible. The version-1
opaque base64url cursor binds bridge ID, topic UUID and sequence. Malformed,
unknown-version, nonlocal, wrong-topic or future-position cursors fail explicitly.
The empty string cursor explicitly starts a full scan after sequence zero.
On an empty forward page both cursors retain the requested after-position (zero
for a new full scan), never reset an existing polling position to zero.
An empty preceding page keeps its requested before-position as `previous_cursor`
and uses `max(before-position - 1, 0)` as `next_cursor`; `has_older` is false and
`has_more` means a post exists at or above that before-position. Thus switching
back to forward reads includes the excluded boundary without replaying earlier
pages. An empty new topic returns position-zero cursors and both flags false.
Restart preserves positions; later appends are visible on the next read and never
shift earlier sequences. Initial latest-window reads intentionally omit older
history; use the empty string cursor to scan the full retained history.

Optional fields default as shown: `include_archived:false`, `parent:null`,
`mentions:[]`, `cursor:null`, `before:null`, `limit:50`. Only nullable references,
parent and cursors accept explicit JSON null; booleans/limits/mention arrays do
not. Reference objects reject unknown fields, as do requests. Explicit empty
cursor is the full-scan sentinel; an empty `before` is invalid.

No post eviction, automatic retention worker or permanent deletion. Defaults:
1,000 topics, 100,000 posts per topic, 1,000,000 total posts, 256 MiB total retained
post body bytes. Archived data counts. Limits are configurable/testable through
the core `TopicLimits` parameter, not a new owner settings UI. Exceeding a bound
returns an explicit capacity error and commits nothing. Retrying an already
accepted identical UUID works at capacity. Mentions and names have separate
bounded lengths; topic listing is bounded by the topic capacity.

## Transaction seam and compatibility

Publication provides a core transaction callback after the new post/mentions are
inserted and before commit. AGB-36 can insert recipient work in that same existing
SQLite transaction; callback failure rolls back sequence, post and recipient work.
The callback runs only for a new post. No native I/O may run inside it. AGB-35
passes a no-op and creates no second delivery lifecycle.

Schema 18 is additive: topic/post/mention tables only, plus immutable-history
guards. Existing messages, boards, identities, peer grants and native fences are
unchanged. Existing error transport remains in use; explicit categories in error
messages are `topic_not_local`, `topic_not_found`, `topic_invalid`,
`topic_conflict`, `topic_archived`, `topic_capacity` and `topic_agent_unknown`.
