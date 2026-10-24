//! The notification ledger: which events were already reported.
//!
//! Event keys are opaque strings (see `model::Event::key`). The ledger
//! only remembers them for a while ([`EVENT_RETENTION`]); an event old
//! enough to be forgotten is not detected again, because detection only
//! compares the previous and the new state of a change.
//!
//! The ledger says "already detected"; `pending_events` holds the detected
//! events that were not yet acknowledged as delivered, and survives
//! restarts. An event enters both in one transaction ([`Store::queue_events`])
//! and leaves the pending table with [`Store::ack_events`].
//!
//! Whether a server was ever watched is a column of the server row, so
//! removing the server removes it, like the ledger rows.

use std::time::Duration;

use model::{Event, Timestamp};
use rusqlite::{OptionalExtension, params};

use crate::{Error, ServerId, Store, review_cache::from_millis, review_cache::millis};

/// How long the ledger keeps a key.
pub const EVENT_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);

impl Store {
    /// Records event keys as reported at `now` and says, per key and in
    /// order, whether it was new. A key seen before (even earlier in the
    /// same slice) is `false` and keeps its first time.
    pub fn record_events(
        &mut self,
        server: ServerId,
        keys: &[String],
        now: Timestamp,
    ) -> Result<Vec<bool>, Error> {
        let tx = self.conn.transaction()?;
        let mut fresh = Vec::with_capacity(keys.len());
        {
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO notified_events (server_id, key, seen_ms)
                 VALUES (?1, ?2, ?3)",
            )?;
            for key in keys {
                fresh.push(insert.execute(params![server.0, key, millis(now)])? > 0);
            }
        }
        tx.commit()?;
        Ok(fresh)
    }

    /// Records the events in the ledger and queues the ones that were new
    /// as pending, in one transaction. Returns how many were queued; an
    /// event seen before (in the ledger, or earlier in the slice) is
    /// ignored.
    pub fn queue_events(
        &mut self,
        server: ServerId,
        events: &[Event],
        now: Timestamp,
    ) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let mut queued = 0;
        {
            let mut ledger = tx.prepare(
                "INSERT OR IGNORE INTO notified_events (server_id, key, seen_ms)
                 VALUES (?1, ?2, ?3)",
            )?;
            let mut pending = tx.prepare(
                "INSERT OR IGNORE INTO pending_events (server_id, key, event, created_ms)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for event in events {
                let key = event.key();
                if ledger.execute(params![server.0, key, millis(now)])? > 0 {
                    let json = serde_json::to_string(event)?;
                    queued += pending.execute(params![server.0, key, json, millis(now)])?;
                }
            }
        }
        tx.commit()?;
        Ok(queued)
    }

    /// The events detected but not yet acknowledged, oldest first.
    pub fn pending_events(&self, server: ServerId) -> Result<Vec<Event>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT event FROM pending_events WHERE server_id = ?1
             ORDER BY created_ms, rowid",
        )?;
        let mut events = stmt
            .query_map([server.0], |r| r.get::<_, String>(0))?
            .map(|json| Ok(serde_json::from_str::<Event>(&json?)?))
            .collect::<Result<Vec<_>, Error>>()?;
        events.sort_by_key(|e| e.at);
        Ok(events)
    }

    /// Removes delivered events (by [`Event::key`]) from the pending table.
    pub fn ack_events(&mut self, server: ServerId, keys: &[String]) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        {
            let mut delete =
                tx.prepare("DELETE FROM pending_events WHERE server_id = ?1 AND key = ?2")?;
            for key in keys {
                delete.execute(params![server.0, key])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Forgets keys recorded before `older_than`, of every server. Returns
    /// how many ledger rows went. Pending events older than that go too.
    pub fn prune_events(&mut self, older_than: Timestamp) -> Result<usize, Error> {
        self.conn.execute(
            "DELETE FROM pending_events WHERE created_ms < ?1",
            [millis(older_than)],
        )?;
        Ok(self.conn.execute(
            "DELETE FROM notified_events WHERE seen_ms < ?1",
            [millis(older_than)],
        )?)
    }

    /// When the first watch pass of the server recorded its baseline, or
    /// `None` if the server was never watched.
    pub fn watched_since(&self, server: ServerId) -> Result<Option<Timestamp>, Error> {
        let ms: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT watched_since FROM servers WHERE id = ?1",
                [server.0],
                |r| r.get(0),
            )
            .optional()?;
        ms.flatten().map(from_millis).transpose()
    }

    /// Marks the server as watched since `now`, unless it already is.
    /// Unknown servers are ignored.
    pub fn mark_watched(&mut self, server: ServerId, now: Timestamp) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE servers SET watched_since = ?2
             WHERE id = ?1 AND watched_since IS NULL",
            params![server.0, millis(now)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
