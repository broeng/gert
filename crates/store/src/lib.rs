//! SQLite cache for server data. Synchronous and GPUI-free.
//!
//! Calls block on disk I/O, so callers run them off the UI thread (GPUI's
//! background executor). [`Store`] is `Send`; share it between threads
//! behind a mutex.
//!
//! Every row is keyed by [`ServerId`], so several servers can live in one
//! database. Schema changes are ordered steps tracked with
//! `PRAGMA user_version`; see `migrations.rs`. The change and diff view
//! caches (file lists, diffs, related changes, diff preferences) live in
//! `review_cache.rs`, including the pruning policy.

mod drafts;
mod error;
mod migrations;
mod review_cache;

use std::{path::Path, time::Duration};

pub use model::ServerId;
use model::{ChangeDetail, ChangeNumber, ChangeSummary, Timestamp};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

pub use drafts::{CachedComments, DeleteOutcome, DraftCounts, DraftFailure, MergeReport};
pub use error::Error;
pub use review_cache::{
    CachedDiff, CachedDiffPreferences, CachedFileList, CachedRelated, PrunePolicy, PruneReport,
};

/// How long a writer waits for a competing connection before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A cached query result: the changes in server order, plus paging state.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedQuery {
    pub changes: Vec<ChangeSummary>,
    /// Whether the server had more results after the last cached page.
    pub more_changes: bool,
    /// When the most recent page was fetched.
    pub fetched_at: Timestamp,
}

/// A cached change detail.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedChange {
    pub detail: ChangeDetail,
    pub fetched_at: Timestamp,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (and migrates) the database at `path`. The parent directory
    /// must exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::init(Connection::open(path)?, true)
    }

    pub fn open_in_memory() -> Result<Self, Error> {
        Self::init(Connection::open_in_memory()?, false)
    }

    fn init(mut conn: Connection, file_backed: bool) -> Result<Self, Error> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Refuse a newer database before changing anything in it, including
        // the journal mode.
        let supported = u32::try_from(migrations::MIGRATIONS.len()).unwrap_or(u32::MAX);
        let found = migrations::user_version(&conn)?;
        if found > supported {
            return Err(Error::SchemaTooNew { found, supported });
        }
        if file_backed {
            // Readers don't block the writer, which matters when the UI
            // reads while a refresh writes.
            conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
            conn.execute_batch("PRAGMA synchronous = NORMAL")?;
        }
        conn.execute_batch("PRAGMA foreign_keys = ON")?;
        migrations::migrate(&mut conn, migrations::MIGRATIONS)?;
        Ok(Self { conn })
    }

    /// The schema version of the open database.
    pub fn schema_version(&self) -> Result<u32, Error> {
        migrations::user_version(&self.conn)
    }

    /// Gets or creates the id for a server by URL, for callers that know
    /// no slug (a login, tests). A trailing slash on `base_url` is ignored.
    /// Several rows may share a URL; this prefers one no config entry has
    /// claimed yet (so a fresh login's rows are adopted by its entry) and
    /// then the oldest.
    pub fn server_id(&mut self, base_url: &str) -> Result<ServerId, Error> {
        let url = normalize_url(base_url);
        let tx = self.conn.transaction()?;
        let id = match find_by_url(&tx, url)? {
            Some(id) => id,
            None => {
                tx.execute("INSERT INTO servers (base_url) VALUES (?1)", [url])?;
                tx.last_insert_rowid()
            }
        };
        tx.commit()?;
        Ok(ServerId(id))
    }

    /// The row of the configured server `slug` at `base_url`, created if
    /// needed. This is how `app` maps `[[servers]]` entries to rows, so that
    /// two entries with the same URL get separate caches.
    ///
    /// 1. A row with this slug is the server's; if its URL differs the
    ///    config was edited, and the row follows it (the cache is kept).
    /// 2. Otherwise a row with this URL and no slug is adopted: caches from
    ///    before slugs existed (or from a login) become the first entry's.
    /// 3. Otherwise a new row.
    pub fn server_for_slug(&mut self, slug: &str, base_url: &str) -> Result<ServerId, Error> {
        let url = normalize_url(base_url);
        let tx = self.conn.transaction()?;
        let by_slug: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, base_url FROM servers WHERE slug = ?1",
                [slug],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let id = if let Some((id, current)) = by_slug {
            if current != url {
                // The config is the truth for a slugged row; unlike
                // `set_server_url` this allows sharing a URL.
                tx.execute(
                    "UPDATE servers SET base_url = ?1 WHERE id = ?2",
                    params![url, id],
                )?;
            }
            id
        } else if let Some(id) = tx
            .query_row(
                "SELECT id FROM servers WHERE base_url = ?1 AND slug IS NULL ORDER BY id LIMIT 1",
                [url],
                |r| r.get(0),
            )
            .optional()?
        {
            tx.execute(
                "UPDATE servers SET slug = ?1 WHERE id = ?2",
                params![slug, id],
            )?;
            id
        } else {
            tx.execute(
                "INSERT INTO servers (base_url, slug) VALUES (?1, ?2)",
                params![url, slug],
            )?;
            tx.last_insert_rowid()
        };
        tx.commit()?;
        Ok(ServerId(id))
    }

    /// Removes a server and everything cached for it. Every table's
    /// `server_id` references `servers(id)` with `ON DELETE CASCADE`, so the
    /// row deletion purges queries, details, file lists, diffs, comments,
    /// drafts and the rest. Other servers are untouched. Unknown ids are
    /// ignored.
    pub fn delete_server(&mut self, server: ServerId) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM servers WHERE id = ?1", [server.0])?;
        Ok(())
    }

    /// Removes the server configured as `slug`, as [`Store::delete_server`]
    /// does. False if no row has that slug (nothing was cached).
    pub fn delete_server_by_slug(&mut self, slug: &str) -> Result<bool, Error> {
        let removed = self
            .conn
            .execute("DELETE FROM servers WHERE slug = ?1", [slug])?;
        Ok(removed > 0)
    }

    /// Gives an existing server a new URL, keeping its id and so its cache.
    /// Fails with [`Error::ServerUrlTaken`] if another row has that URL, and
    /// with [`Error::NoSuchServer`] for an unknown id.
    pub fn set_server_url(&mut self, server: ServerId, base_url: &str) -> Result<(), Error> {
        let url = normalize_url(base_url);
        let tx = self.conn.transaction()?;
        let taken = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM servers WHERE base_url = ?1 AND id <> ?2)",
            params![url, server.0],
            |r| r.get::<_, bool>(0),
        )?;
        if taken {
            return Err(Error::ServerUrlTaken(url.to_owned()));
        }
        let changed = tx.execute(
            "UPDATE servers SET base_url = ?1 WHERE id = ?2",
            params![url, server.0],
        )?;
        if changed == 0 {
            return Err(Error::NoSuchServer);
        }
        tx.commit()?;
        Ok(())
    }

    /// A value from the application state table, e.g. the last active
    /// server.
    pub fn app_state(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row("SELECT value FROM app_state WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_app_state(&mut self, key: &str, value: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO app_state (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Replaces the cached result of `query` with this first page.
    pub fn save_query_page(
        &mut self,
        server: ServerId,
        query: &str,
        changes: &[ChangeSummary],
        more_changes: bool,
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM query_results WHERE server_id = ?1 AND query = ?2",
            params![server.0, query],
        )?;
        tx.execute(
            "INSERT INTO query_results (server_id, query, more_changes, fetched_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![server.0, query, more_changes, fetched_at.to_string()],
        )?;
        insert_items(&tx, server, query, changes)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds a further page ("load more") after the cached ones. Changes
    /// already in the list (results shift while paging) are not repeated.
    /// With no cached result yet this behaves like [`Self::save_query_page`].
    pub fn append_query_page(
        &mut self,
        server: ServerId,
        query: &str,
        changes: &[ChangeSummary],
        more_changes: bool,
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        let next: Option<i64> = tx
            .query_row(
                "SELECT (SELECT COALESCE(MAX(position) + 1, 0) FROM query_result_items
                     WHERE server_id = ?1 AND query = ?2)
             FROM query_results WHERE server_id = ?1 AND query = ?2",
                params![server.0, query],
                |r| r.get(0),
            )
            .optional()?;
        let Some(next) = next else {
            drop(tx);
            return self.save_query_page(server, query, changes, more_changes, fetched_at);
        };
        let fresh: Vec<&ChangeSummary> = {
            let mut seen = tx.prepare_cached(
                "SELECT 1 FROM query_result_items
                 WHERE server_id = ?1 AND query = ?2 AND number = ?3",
            )?;
            let mut fresh = Vec::new();
            for c in changes {
                if !seen.exists(params![server.0, query, number(c.number)])? {
                    fresh.push(c);
                }
            }
            fresh
        };
        // Upsert every change (they may be newer than cached), but only
        // list the new ones.
        for c in changes {
            upsert_change(&tx, server, c)?;
        }
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO query_result_items (server_id, query, position, number)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (i, c) in (next..).zip(fresh) {
                ins.execute(params![server.0, query, i, number(c.number)])?;
            }
        }
        tx.execute(
            "UPDATE query_results SET more_changes = ?3, fetched_at = ?4
             WHERE server_id = ?1 AND query = ?2",
            params![server.0, query, more_changes, fetched_at.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records a successful fetch that changed nothing: only `fetched_at`
    /// moves. Does nothing if the query is not cached.
    pub fn touch_query(
        &mut self,
        server: ServerId,
        query: &str,
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE query_results SET fetched_at = ?3 WHERE server_id = ?1 AND query = ?2",
            params![server.0, query, fetched_at.to_string()],
        )?;
        Ok(())
    }

    /// The cached result of `query`, or `None` if it was never fetched.
    pub fn load_query(&self, server: ServerId, query: &str) -> Result<Option<CachedQuery>, Error> {
        let head: Option<(bool, String)> = self
            .conn
            .query_row(
                "SELECT more_changes, fetched_at FROM query_results
                 WHERE server_id = ?1 AND query = ?2",
                params![server.0, query],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((more_changes, fetched_at)) = head else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.json FROM query_result_items i
             JOIN changes c ON c.server_id = i.server_id AND c.number = i.number
             WHERE i.server_id = ?1 AND i.query = ?2
             ORDER BY i.position",
        )?;
        let rows = stmt.query_map(params![server.0, query], |r| r.get::<_, String>(0))?;
        let mut changes = Vec::new();
        for json in rows {
            changes.push(serde_json::from_str(&json?)?);
        }
        Ok(Some(CachedQuery {
            changes,
            more_changes,
            fetched_at: parse_ts(&fetched_at)?,
        }))
    }

    /// A cached change summary, from any query that returned it.
    pub fn load_change_summary(
        &self,
        server: ServerId,
        num: ChangeNumber,
    ) -> Result<Option<ChangeSummary>, Error> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM changes WHERE server_id = ?1 AND number = ?2",
                params![server.0, number(num)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    /// Stores the detail, and refreshes the change's list-row summary too.
    pub fn save_change_detail(
        &mut self,
        server: ServerId,
        detail: &ChangeDetail,
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        upsert_change(&tx, server, &detail.summary)?;
        tx.execute(
            "INSERT INTO change_details (server_id, number, json, fetched_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (server_id, number)
             DO UPDATE SET json = excluded.json, fetched_at = excluded.fetched_at",
            params![
                server.0,
                number(detail.summary.number),
                serde_json::to_string(detail)?,
                fetched_at.to_string()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_change_detail(
        &self,
        server: ServerId,
        num: ChangeNumber,
    ) -> Result<Option<CachedChange>, Error> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT json, fetched_at FROM change_details
                 WHERE server_id = ?1 AND number = ?2",
                params![server.0, number(num)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(json, at)| {
            Ok(CachedChange {
                detail: serde_json::from_str(&json)?,
                fetched_at: parse_ts(&at)?,
            })
        })
        .transpose()
    }
}

/// A row with this URL, preferring one without a slug, then the oldest.
fn find_by_url(tx: &Transaction<'_>, url: &str) -> Result<Option<i64>, Error> {
    Ok(tx
        .query_row(
            "SELECT id FROM servers WHERE base_url = ?1
             ORDER BY slug IS NOT NULL, id LIMIT 1",
            [url],
            |r| r.get(0),
        )
        .optional()?)
}

fn normalize_url(base_url: &str) -> &str {
    base_url.trim().trim_end_matches('/')
}

/// SQLite integers are signed; Gerrit change numbers never get near 2^63.
fn number(n: ChangeNumber) -> i64 {
    i64::try_from(n.0).unwrap_or(i64::MAX)
}

fn parse_ts(s: &str) -> Result<Timestamp, Error> {
    s.parse().map_err(|_| Error::BadTimestamp(s.to_owned()))
}

fn upsert_change(tx: &Transaction<'_>, server: ServerId, c: &ChangeSummary) -> Result<(), Error> {
    tx.prepare_cached(
        "INSERT INTO changes (server_id, number, updated, json) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (server_id, number)
         DO UPDATE SET updated = excluded.updated, json = excluded.json",
    )?
    .execute(params![
        server.0,
        number(c.number),
        c.updated.to_string(),
        serde_json::to_string(c)?
    ])?;
    Ok(())
}

fn insert_items(
    tx: &Transaction<'_>,
    server: ServerId,
    query: &str,
    changes: &[ChangeSummary],
) -> Result<(), Error> {
    let mut ins = tx.prepare_cached(
        "INSERT INTO query_result_items (server_id, query, position, number)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (position, c) in (0_i64..).zip(changes) {
        upsert_change(tx, server, c)?;
        ins.execute(params![server.0, query, position, number(c.number)])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
