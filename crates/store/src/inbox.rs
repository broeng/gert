//! The in-app notification inbox: every event that matched a notification
//! rule, whether or not the desktop notification was shown.
//!
//! An item is unique per server and event key, so recording the same event
//! twice (a replay after a crash between the write and the acknowledgement)
//! adds nothing. Rows carry the title of the rule that fired as it was at
//! match time, never an index into the rule list, which the user can
//! reorder. The inbox is pruned whenever items are added: at most
//! [`INBOX_KEEP`] per server, none older than [`INBOX_RETENTION`].
//!
//! `created_ms` is when the event happened (`Event::at`), so the order and
//! the day groups are those of the activity, also for catch-up events.

use std::{collections::HashMap, time::Duration};

use model::{Event, Timestamp};
use rusqlite::{Transaction, params, params_from_iter};

use crate::{Error, ServerId, Store, review_cache::from_millis, review_cache::millis};

/// How many items of a server the inbox keeps (the newest).
pub const INBOX_KEEP: usize = 500;

/// How long the inbox keeps an item.
pub const INBOX_RETENTION: Duration = Duration::from_secs(60 * 24 * 3600);

/// The id of an inbox item, unique across servers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InboxId(pub i64);

/// An event to put in the inbox.
#[derive(Clone, Debug, PartialEq)]
pub struct NewInboxItem {
    pub event: Event,
    /// `Rule::title()` of the rule that fired, at match time.
    pub rule_title: String,
    /// Whether the rule was a mention rule (it words the title).
    pub mention: bool,
    /// Whether the desktop notification was shown.
    pub shown_on_os: bool,
}

/// An inbox item as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct InboxItem {
    pub id: InboxId,
    pub server: ServerId,
    pub event: Event,
    pub rule_title: String,
    pub mention: bool,
    pub shown_on_os: bool,
    /// When the event happened.
    pub created_at: Timestamp,
    /// `None` while unread.
    pub read_at: Option<Timestamp>,
}

impl Store {
    /// Adds the items to the inbox of `server` and prunes it as of `now`.
    /// Returns how many were new; an event already in the inbox (same
    /// key) is ignored and keeps its read state.
    pub fn add_inbox(
        &mut self,
        server: ServerId,
        items: &[NewInboxItem],
        now: Timestamp,
    ) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let added = add(&tx, server, items, now)?;
        tx.commit()?;
        Ok(added)
    }

    /// Adds `items` to the inbox of `server` and acknowledges the pending
    /// events with the keys `seen`, in one transaction: if the inbox write
    /// fails nothing is acknowledged, so the events come back with the
    /// next pass, and a crash can neither lose an event nor leave it
    /// pending after it was kept. Returns how many items were new.
    pub fn deliver_events(
        &mut self,
        server: ServerId,
        items: &[NewInboxItem],
        seen: &[String],
        now: Timestamp,
    ) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let added = add(&tx, server, items, now)?;
        {
            let mut delete =
                tx.prepare("DELETE FROM pending_events WHERE server_id = ?1 AND key = ?2")?;
            for key in seen {
                delete.execute(params![server.0, key])?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// The newest `limit` items, newest first, of `servers` (every server
    /// if `None`).
    pub fn inbox_items(
        &self,
        servers: Option<&[ServerId]>,
        limit: usize,
    ) -> Result<Vec<InboxItem>, Error> {
        let mut sql = "SELECT id, server_id, event, rule_title, mention, shown_on_os,
                              created_ms, read_ms FROM inbox"
            .to_owned();
        let mut args: Vec<i64> = Vec::new();
        if let Some(servers) = servers {
            let marks = vec!["?"; servers.len()].join(", ");
            sql.push_str(&format!(" WHERE server_id IN ({marks})"));
            args.extend(servers.iter().map(|s| s.0));
        }
        sql.push_str(" ORDER BY created_ms DESC, id DESC LIMIT ?");
        args.push(i64::try_from(limit).unwrap_or(i64::MAX));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(args), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(
                |(id, server, json, rule_title, mention, shown_on_os, created, read)| {
                    Ok(InboxItem {
                        id: InboxId(id),
                        server: ServerId(server),
                        event: serde_json::from_str(&json)?,
                        rule_title,
                        mention,
                        shown_on_os,
                        created_at: from_millis(created)?,
                        read_at: read.map(from_millis).transpose()?,
                    })
                },
            )
            .collect()
    }

    /// How many unread items each server has; servers with none are left
    /// out.
    pub fn inbox_unread_counts(&self) -> Result<HashMap<ServerId, usize>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT server_id, COUNT(*) FROM inbox WHERE read_ms IS NULL GROUP BY server_id",
        )?;
        let counts = stmt
            .query_map([], |r| Ok((ServerId(r.get(0)?), r.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(counts
            .into_iter()
            .map(|(id, n)| (id, usize::try_from(n).unwrap_or(0)))
            .collect())
    }

    /// Marks the items read as of `now`; an item read already keeps its
    /// time.
    pub fn mark_inbox_read(&mut self, ids: &[InboxId], now: Timestamp) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("UPDATE inbox SET read_ms = ?2 WHERE id = ?1 AND read_ms IS NULL")?;
            for id in ids {
                stmt.execute(params![id.0, millis(now)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn mark_inbox_unread(&mut self, ids: &[InboxId]) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE inbox SET read_ms = NULL WHERE id = ?1")?;
            for id in ids {
                stmt.execute([id.0])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Marks every unread item of `server` (of all servers if `None`)
    /// read. Returns how many changed.
    pub fn mark_all_inbox_read(
        &mut self,
        server: Option<ServerId>,
        now: Timestamp,
    ) -> Result<usize, Error> {
        Ok(self.conn.execute(
            "UPDATE inbox SET read_ms = ?1
             WHERE read_ms IS NULL AND (?2 IS NULL OR server_id = ?2)",
            params![millis(now), server.map(|s| s.0)],
        )?)
    }

    pub fn delete_inbox(&mut self, ids: &[InboxId]) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM inbox WHERE id = ?1")?;
            for id in ids {
                stmt.execute([id.0])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes the read items of `server` (of all servers if `None`).
    /// Returns how many went.
    pub fn delete_read_inbox(&mut self, server: Option<ServerId>) -> Result<usize, Error> {
        Ok(self.conn.execute(
            "DELETE FROM inbox
             WHERE read_ms IS NOT NULL AND (?1 IS NULL OR server_id = ?1)",
            params![server.map(|s| s.0)],
        )?)
    }

    /// Applies the keep and retention limits to every server's inbox as of
    /// `now`. Returns how many items went.
    pub fn prune_inbox(&mut self, now: Timestamp) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let gone = prune(&tx, None, INBOX_KEEP, now)?;
        tx.commit()?;
        Ok(gone)
    }
}

fn add(
    tx: &Transaction<'_>,
    server: ServerId,
    items: &[NewInboxItem],
    now: Timestamp,
) -> Result<usize, Error> {
    let mut added = 0;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO inbox
                (server_id, event_key, event, rule_title, mention, shown_on_os, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for item in items {
            let json = serde_json::to_string(&item.event)?;
            added += insert.execute(params![
                server.0,
                item.event.key(),
                json,
                item.rule_title,
                item.mention,
                item.shown_on_os,
                millis(item.event.at),
            ])?;
        }
    }
    prune(tx, Some(server), INBOX_KEEP, now)?;
    Ok(added)
}

/// Drops items older than the retention and all but the newest `keep` per
/// server, for `server` or every server.
fn prune(
    tx: &Transaction<'_>,
    server: Option<ServerId>,
    keep: usize,
    now: Timestamp,
) -> Result<usize, Error> {
    let server = server.map(|s| s.0);
    let oldest = millis(now) - i64::try_from(INBOX_RETENTION.as_millis()).unwrap_or(i64::MAX);
    let mut gone = tx.execute(
        "DELETE FROM inbox WHERE created_ms < ?1 AND (?2 IS NULL OR server_id = ?2)",
        params![oldest, server],
    )?;
    gone += tx.execute(
        "DELETE FROM inbox
         WHERE (?1 IS NULL OR server_id = ?1)
           AND id NOT IN (
               SELECT id FROM (
                   SELECT id, ROW_NUMBER() OVER (
                       PARTITION BY server_id ORDER BY created_ms DESC, id DESC) AS n
                   FROM inbox) WHERE n <= ?2)",
        params![server, i64::try_from(keep).unwrap_or(i64::MAX)],
    )?;
    Ok(gone)
}

#[cfg(test)]
mod tests;
