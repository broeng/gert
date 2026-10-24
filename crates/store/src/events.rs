//! The notification ledger: which events were already reported.
//!
//! Event keys are opaque strings (see `model::Event::key`). The ledger
//! only remembers them for a while ([`EVENT_RETENTION`]); an event old
//! enough to be forgotten is not detected again, because detection only
//! compares the previous and the new state of a change.
//!
//! Whether a server was ever watched is a column of the server row, so
//! removing the server removes it, like the ledger rows.

use std::time::Duration;

use model::Timestamp;
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

    /// Forgets keys recorded before `older_than`, of every server. Returns
    /// how many rows went.
    pub fn prune_events(&mut self, older_than: Timestamp) -> Result<usize, Error> {
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
