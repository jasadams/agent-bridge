//! Durable local topics, independent of native delivery and availability.
use crate::{
    db::{Db, now},
    protocol::{MAX_BODY, Provider},
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AgentRef {
    pub bridge_id: Option<String>,
    pub agent_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TopicRef {
    pub bridge_id: Option<String>,
    pub topic_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub reference: AgentRef,
    pub display_name: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Topic {
    pub reference: TopicRef,
    pub name: String,
    pub archived: bool,
    pub created: i64,
    pub updated: i64,
    pub high_water: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicPost {
    pub id: String,
    pub topic: TopicRef,
    pub sequence: i64,
    pub author: AgentRef,
    pub author_display_name: String,
    pub body: String,
    pub parent: Option<String>,
    pub mentions: Vec<AgentRef>,
    pub created: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResult {
    pub post: TopicPost,
    pub duplicate: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadPage {
    pub posts: Vec<TopicPost>,
    pub next_cursor: String,
    pub previous_cursor: String,
    pub has_more: bool,
    pub has_older: bool,
    pub high_water: i64,
}
#[derive(Debug, Clone)]
pub struct TopicPublication {
    pub topic: TopicRef,
    pub id: String,
    pub author: AgentRef,
    pub body: String,
    pub parent: Option<String>,
    pub mentions: Vec<AgentRef>,
}
#[derive(Debug, Clone)]
pub struct TopicLimits {
    pub max_topics: u64,
    pub max_posts_per_topic: u64,
    pub max_total_posts: u64,
    pub max_total_body_bytes: u64,
}
impl Default for TopicLimits {
    fn default() -> Self {
        Self {
            max_topics: 1000,
            max_posts_per_topic: 100_000,
            max_total_posts: 1_000_000,
            max_total_body_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    bridge_id: String,
    topic_id: String,
    after: i64,
}

fn validate_local(local: &str) -> Result<()> {
    let parsed: libp2p_identity::PeerId = local
        .parse()
        .context("topic_invalid: invalid local bridge ID")?;
    ensure!(
        parsed.to_string() == local,
        "topic_invalid: noncanonical local bridge ID"
    );
    Ok(())
}

fn check_bridge(local: &str, bridge: Option<&str>) -> Result<()> {
    ensure!(
        bridge.is_none_or(|id| id == local),
        "topic_not_local: reference is not on this bridge"
    );
    Ok(())
}

fn check_uuid(id: &str) -> Result<()> {
    let parsed = uuid::Uuid::parse_str(id).context("topic_invalid: expected canonical UUID")?;
    ensure!(
        parsed.to_string() == id,
        "topic_invalid: expected canonical lowercase hyphenated UUID"
    );
    Ok(())
}

fn normalize_topic(local: &str, topic: &TopicRef) -> Result<TopicRef> {
    validate_local(local)?;
    check_bridge(local, topic.bridge_id.as_deref())?;
    check_uuid(&topic.topic_id)?;
    Ok(TopicRef {
        bridge_id: Some(local.into()),
        topic_id: topic.topic_id.clone(),
    })
}

fn identity(id: &str) -> Result<(&str, &str)> {
    let (provider, session) = id
        .split_once('-')
        .context("topic_agent_unknown: expected canonical agent ID")?;
    let parsed = provider
        .parse::<Provider>()
        .context("topic_agent_unknown: unknown provider")?;
    crate::clients::validate_session(&parsed, session)
        .context("topic_agent_unknown: invalid session ID")?;
    // Existing registrations and routing_name preserve the exact native/manual
    // session spelling. Do not tighten it here or silently merge old identities.
    Ok((provider, session))
}

fn normalize_agent(local: &str, agent: &AgentRef) -> Result<AgentRef> {
    check_bridge(local, agent.bridge_id.as_deref())?;
    identity(&agent.agent_id)?;
    Ok(AgentRef {
        bridge_id: Some(local.into()),
        agent_id: agent.agent_id.clone(),
    })
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.chars().count() <= 100
            && name.len() <= 400
            && name.trim() == name
            && !name.chars().any(char::is_control),
        "topic_invalid: names require 1–100 characters without controls or surrounding whitespace"
    );
    Ok(())
}

fn get_topic(conn: &Connection, local: &str, id: &str) -> Result<Topic> {
    conn.query_row("SELECT name,archived,created,updated,high_water FROM topics WHERE bridge_id=? AND topic_id=?",params![local,id],|r| Ok(Topic {
        reference:TopicRef {bridge_id:Some(local.into()),topic_id:id.into()}, name:r.get(0)?, archived:r.get(1)?, created:r.get(2)?, updated:r.get(3)?,high_water:r.get(4)?
    })).optional()?.context("topic_not_found: unknown topic")
}

fn known_agent(conn: &Connection, agent: &AgentRef, enabled: bool) -> Result<()> {
    let (provider, session) = identity(&agent.agent_id)?;
    let known:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM agents WHERE provider=?1 AND session=?2 AND (?3=0 OR enabled=1) AND (?4=0 OR name='user'))",params![provider,session,enabled,agent.agent_id=="inbox-user"],|r|r.get(0))?;
    ensure!(
        known,
        "topic_agent_unknown: exact local registration required"
    );
    Ok(())
}

fn current_display(conn: &Connection, provider: &str, session: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT display_name FROM session_metadata WHERE provider=? AND session=?",
            params![provider, session],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .filter(|name| !name.trim().is_empty()))
}

fn display_name(conn: &Connection, local: &str, agent: &AgentRef) -> Result<String> {
    let (provider, session) = identity(&agent.agent_id)?;
    if let Some(name) = current_display(conn, provider, session)? {
        return Ok(name);
    }
    let snapshot:Option<String>=conn.query_row("SELECT author_display_name FROM topic_posts WHERE bridge_id=? AND author_id=? ORDER BY created DESC,id DESC LIMIT 1",params![local,agent.agent_id],|r|r.get(0)).optional()?;
    Ok(snapshot.unwrap_or_else(|| agent.agent_id.clone()))
}

fn load_post(conn: &Connection, local: &str, id: &str) -> Result<Option<TopicPost>> {
    let post=conn.query_row("SELECT topic_id,sequence,author_id,author_display_name,body,parent,created FROM topic_posts WHERE bridge_id=? AND id=?",params![local,id],|r|Ok(TopicPost {
        id:id.into(),topic:TopicRef {bridge_id:Some(local.into()),topic_id:r.get(0)?},sequence:r.get(1)?,author:AgentRef {bridge_id:Some(local.into()),agent_id:r.get(2)?},author_display_name:r.get(3)?,body:r.get(4)?,parent:r.get(5)?,mentions:vec![],created:r.get(6)?
    })).optional()?;
    let Some(mut post) = post else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT agent_id FROM topic_mentions WHERE bridge_id=? AND post_id=? ORDER BY agent_id",
    )?;
    post.mentions = stmt
        .query_map(params![local, id], |r| {
            Ok(AgentRef {
                bridge_id: Some(local.into()),
                agent_id: r.get(0)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(post))
}

fn encode_cursor(local: &str, topic: &TopicRef, after: i64) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor {
        version: 1,
        bridge_id: local.into(),
        topic_id: topic.topic_id.clone(),
        after,
    })?))
}

fn decode_cursor(local: &str, topic: &TopicRef, raw: &str, high_water: i64) -> Result<i64> {
    if raw.is_empty() {
        return Ok(0);
    }
    ensure!(raw.len() <= 4096, "topic_invalid: cursor too large");
    let data = URL_SAFE_NO_PAD
        .decode(raw)
        .context("topic_invalid: malformed cursor")?;
    let cursor: Cursor =
        serde_json::from_slice(&data).context("topic_invalid: malformed cursor")?;
    ensure!(
        cursor.version == 1,
        "topic_invalid: unsupported cursor version"
    );
    check_bridge(local, Some(&cursor.bridge_id))?;
    ensure!(
        cursor.topic_id == topic.topic_id,
        "topic_invalid: cursor belongs to another topic"
    );
    ensure!(
        (0..=high_water).contains(&cursor.after),
        "topic_invalid: future or negative cursor position"
    );
    Ok(cursor.after)
}

impl Db {
    pub fn topic_create(&mut self, local: &str, name: &str, limits: &TopicLimits) -> Result<Topic> {
        validate_local(local)?;
        validate_name(name)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM topics WHERE bridge_id=?",
            [local],
            |r| r.get(0),
        )?;
        ensure!(
            (count as u64) < limits.max_topics,
            "topic_capacity: topic count limit reached"
        );
        let id = uuid::Uuid::new_v4().to_string();
        let timestamp = now();
        tx.execute(
            "INSERT INTO topics(bridge_id,topic_id,name,created,updated) VALUES(?,?,?,?,?)",
            params![local, id, name, timestamp, timestamp],
        )?;
        let topic = get_topic(&tx, local, &id)?;
        tx.commit()?;
        Ok(topic)
    }

    /// Known registrations, including disabled ones, not a native-readiness list.
    pub fn topic_agents(&self, local: &str) -> Result<Vec<AgentInfo>> {
        validate_local(local)?;
        let tx = self.conn.unchecked_transaction()?;
        let identities = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT provider,session FROM agents ORDER BY provider,session LIMIT 1001",
            )?;
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        ensure!(
            identities.len() <= 1000,
            "topic_capacity: agent catalog exceeds 1000 identities"
        );
        let mut result = Vec::with_capacity(identities.len());
        for (provider, session) in identities {
            let reference = normalize_agent(
                local,
                &AgentRef {
                    bridge_id: None,
                    agent_id: format!("{provider}-{session}"),
                },
            )?;
            known_agent(&tx, &reference, false)?;
            result.push(AgentInfo {
                display_name: display_name(&tx, local, &reference)?,
                reference,
            });
        }
        result.sort_by(|a, b| a.reference.cmp(&b.reference));
        tx.commit()?;
        Ok(result)
    }

    pub fn topic_list(&self, local: &str, include_archived: bool) -> Result<Vec<Topic>> {
        validate_local(local)?;
        let tx = self.conn.unchecked_transaction()?;
        let ids = {
            let mut stmt=tx.prepare("SELECT topic_id FROM topics WHERE bridge_id=?1 AND (?2 OR archived=0) ORDER BY created,topic_id")?;
            stmt.query_map(params![local, include_archived], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let topics = ids
            .iter()
            .map(|id| get_topic(&tx, local, id))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(topics)
    }

    pub fn topic_inspect(&self, local: &str, topic: &TopicRef) -> Result<Topic> {
        let topic = normalize_topic(local, topic)?;
        get_topic(&self.conn, local, &topic.topic_id)
    }

    pub fn topic_rename(&mut self, local: &str, topic: &TopicRef, name: &str) -> Result<Topic> {
        let topic = normalize_topic(local, topic)?;
        validate_name(name)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        get_topic(&tx, local, &topic.topic_id)?;
        tx.execute(
            "UPDATE topics SET name=?,updated=? WHERE bridge_id=? AND topic_id=?",
            params![name, now(), local, topic.topic_id],
        )?;
        let topic = get_topic(&tx, local, &topic.topic_id)?;
        tx.commit()?;
        Ok(topic)
    }

    pub fn topic_archive(
        &mut self,
        local: &str,
        topic: &TopicRef,
        archived: bool,
    ) -> Result<Topic> {
        let topic = normalize_topic(local, topic)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        get_topic(&tx, local, &topic.topic_id)?;
        tx.execute(
            "UPDATE topics SET archived=?,updated=? WHERE bridge_id=? AND topic_id=?",
            params![archived, now(), local, topic.topic_id],
        )?;
        let topic = get_topic(&tx, local, &topic.topic_id)?;
        tx.commit()?;
        Ok(topic)
    }

    pub fn topic_publish(
        &mut self,
        local: &str,
        publication: &TopicPublication,
        limits: &TopicLimits,
    ) -> Result<PublishResult> {
        self.topic_publish_with(local, publication, limits, |_, _| Ok(()))
    }

    /// The callback inserts transactional work only; it must never perform native I/O.
    /// It runs once for a new post, never for an idempotent retry.
    pub fn topic_publish_with<F>(
        &mut self,
        local: &str,
        publication: &TopicPublication,
        limits: &TopicLimits,
        callback: F,
    ) -> Result<PublishResult>
    where
        F: FnOnce(&Transaction<'_>, &TopicPost) -> Result<()>,
    {
        let topic = normalize_topic(local, &publication.topic)?;
        check_uuid(&publication.id)?;
        let author = normalize_agent(local, &publication.author)?;
        ensure!(
            !publication.body.trim().is_empty() && publication.body.len() <= MAX_BODY,
            "topic_invalid: body must be nonblank and at most 64 KiB"
        );
        if let Some(parent) = &publication.parent {
            check_uuid(parent)?;
        }
        let mentions = publication
            .mentions
            .iter()
            .map(|a| normalize_agent(local, a))
            .collect::<Result<BTreeSet<_>>>()?
            .into_iter()
            .collect::<Vec<_>>();
        ensure!(
            mentions.len() <= 100,
            "topic_invalid: at most 100 unique mentions"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(post) = load_post(&tx, local, &publication.id)? {
            ensure!(
                post.topic == topic
                    && post.author == author
                    && post.body == publication.body
                    && post.parent == publication.parent
                    && post.mentions == mentions,
                "topic_conflict: post UUID reused with different content"
            );
            tx.commit()?;
            return Ok(PublishResult {
                post,
                duplicate: true,
            });
        }
        let metadata = get_topic(&tx, local, &topic.topic_id)?;
        ensure!(
            !metadata.archived,
            "topic_archived: restore before publishing"
        );
        known_agent(&tx, &author, true)?;
        for mention in &mentions {
            known_agent(&tx, mention, false)?;
        }
        if let Some(parent) = &publication.parent {
            let same_topic:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM topic_posts WHERE bridge_id=? AND topic_id=? AND id=?)",params![local,topic.topic_id,parent],|r|r.get(0))?;
            ensure!(
                same_topic,
                "topic_invalid: parent must already exist in the same topic"
            );
        }
        // Aggregate only the bounded topic rows, never scan retained post bodies.
        let (total_posts,total_bytes):(i64,i64)=tx.query_row("SELECT coalesce(sum(high_water),0),coalesce(sum(retained_body_bytes),0) FROM topics WHERE bridge_id=?",[local],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let (total_posts, total_bytes) = (total_posts as u64, total_bytes as u64);
        ensure!(
            (metadata.high_water as u64) < limits.max_posts_per_topic
                && total_posts < limits.max_total_posts
                && (publication.body.len() as u64)
                    <= limits.max_total_body_bytes.saturating_sub(total_bytes),
            "topic_capacity: retained post or body byte limit reached"
        );
        ensure!(
            total_bytes <= limits.max_total_body_bytes,
            "topic_capacity: retained body byte limit reached"
        );
        let sequence = metadata
            .high_water
            .checked_add(1)
            .context("topic_capacity: sequence exhausted")?;
        let post = TopicPost {
            id: publication.id.clone(),
            topic,
            sequence,
            author_display_name: display_name(&tx, local, &author)?,
            author,
            body: publication.body.clone(),
            parent: publication.parent.clone(),
            mentions,
            created: now(),
        };
        tx.execute("UPDATE topics SET high_water=?,retained_body_bytes=retained_body_bytes+?,updated=? WHERE bridge_id=? AND topic_id=?",params![sequence,post.body.len() as i64,post.created,local,post.topic.topic_id])?;
        tx.execute("INSERT INTO topic_posts(bridge_id,id,topic_id,sequence,author_id,author_display_name,body,parent,created) VALUES(?,?,?,?,?,?,?,?,?)",params![local,post.id,post.topic.topic_id,post.sequence,post.author.agent_id,post.author_display_name,post.body,post.parent,post.created])?;
        for mention in &post.mentions {
            tx.execute(
                "INSERT INTO topic_mentions(bridge_id,post_id,agent_id) VALUES(?,?,?)",
                params![local, post.id, mention.agent_id],
            )?;
        }
        callback(&tx, &post)?;
        tx.commit()?;
        Ok(PublishResult {
            post,
            duplicate: false,
        })
    }

    pub fn topic_read(
        &mut self,
        local: &str,
        topic: &TopicRef,
        cursor: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<ReadPage> {
        let topic = normalize_topic(local, topic)?;
        ensure!(
            (1..=100).contains(&limit),
            "topic_invalid: read limit must be 1–100"
        );
        ensure!(
            cursor.is_none() || before.is_none(),
            "topic_invalid: cursor and before are mutually exclusive"
        );
        ensure!(
            before != Some(""),
            "topic_invalid: before cursor cannot be empty"
        );
        let tx = self.conn.transaction()?;
        let high_water = get_topic(&tx, local, &topic.topic_id)?.high_water;
        let bound = cursor
            .or(before)
            .map(|raw| decode_cursor(local, &topic, raw, high_water))
            .transpose()?;
        let (sql, position, reverse) = if before.is_some() {
            (
                "SELECT id FROM topic_posts WHERE bridge_id=?1 AND topic_id=?2 AND sequence<?3 ORDER BY sequence DESC LIMIT ?4",
                bound.unwrap_or(0),
                true,
            )
        } else if cursor.is_some() {
            (
                "SELECT id FROM topic_posts WHERE bridge_id=?1 AND topic_id=?2 AND sequence>?3 ORDER BY sequence ASC LIMIT ?4",
                bound.unwrap_or(0),
                false,
            )
        } else {
            (
                "SELECT id FROM topic_posts WHERE bridge_id=?1 AND topic_id=?2 AND sequence<=?3 ORDER BY sequence DESC LIMIT ?4",
                high_water,
                true,
            )
        };
        let mut ids = {
            let mut stmt = tx.prepare(sql)?;
            stmt.query_map(
                params![local, topic.topic_id, position, limit as i64],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if reverse {
            ids.reverse();
        }
        let posts = ids
            .iter()
            .map(|id| load_post(&tx, local, id)?.context("topic_invalid: missing retained post"))
            .collect::<Result<Vec<_>>>()?;
        let (previous, next, has_older, has_more) = match (posts.first(), posts.last()) {
            (Some(first), Some(last)) => (
                first.sequence,
                last.sequence,
                first.sequence > 1,
                last.sequence < high_water,
            ),
            _ if before.is_some() => {
                let bound = bound.unwrap_or(0);
                (
                    bound,
                    (bound - 1).max(0),
                    false,
                    high_water > 0 && high_water >= bound,
                )
            }
            _ => {
                let anchor = bound.unwrap_or(0);
                (anchor, anchor, anchor > 1, false)
            }
        };
        let page = ReadPage {
            posts,
            next_cursor: encode_cursor(local, &topic, next)?,
            previous_cursor: encode_cursor(local, &topic, previous)?,
            has_more,
            has_older,
            high_water,
        };
        tx.commit()?;
        Ok(page)
    }
}
