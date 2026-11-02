//! Durable owner intent, separate from native delivery-binding availability.
use crate::{db::Db, protocol::Provider};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) fn owner_policy_enabled_on(
    conn: &Connection,
    provider: &Provider,
    session: &str,
    name: &str,
) -> Result<bool> {
    let exact: Option<bool> = conn
        .query_row(
            "SELECT enabled FROM agent_owner_policy WHERE name=? AND provider=? AND session=?",
            params![name, provider.label(), session],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(enabled) = exact {
        return Ok(enabled);
    }
    if name == crate::clients::lifecycle::routing_name(provider, session) {
        let sibling: Option<bool> = conn.query_row(
            "SELECT max(enabled) FROM agent_owner_policy WHERE provider=? AND session=?",
            params![provider.label(), session],
            |row| row.get(0),
        )?;
        if let Some(enabled) = sibling {
            return Ok(enabled);
        }
    }
    // This is only the local owner-policy default, never a peer grant.
    Ok(true)
}

/// Owner edits must immediately deny queued work through every affected route.
/// Never revive retired availability when policy becomes permissive again.
pub(crate) fn clamp_bindings_on(
    conn: &Connection,
    provider: &Provider,
    session: &str,
) -> Result<()> {
    let names = {
        let mut query =
            conn.prepare("SELECT name FROM agents WHERE provider=? AND session=? AND enabled=1")?;
        query
            .query_map(params![provider.label(), session], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for name in names {
        if !owner_policy_enabled_on(conn, provider, session, &name)? {
            conn.execute(
                "UPDATE agents SET enabled=0 WHERE name=? AND provider=? AND session=?",
                params![name, provider.label(), session],
            )?;
        }
    }
    Ok(())
}

impl Db {
    /// Query current owner intent for this exact route and native identity.
    /// Canonical routes without exact policy use any-enabled sibling policy;
    /// a route with no applicable policy retains the existing local default.
    pub fn owner_policy_enabled(
        &self,
        provider: &Provider,
        session: &str,
        name: &str,
    ) -> Result<bool> {
        owner_policy_enabled_on(&self.conn, provider, session, name)
    }
}
