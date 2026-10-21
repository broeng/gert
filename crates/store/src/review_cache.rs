//! Caches for the change and diff views: file lists, diffs, related changes
//! and diff preferences.
//!
//! Keys use revision SHAs, never just patch set numbers (see
//! [`model::FilesKey`]). "No base" (compare against the parent) is stored
//! as the empty string, because a SHA is never empty and SQL primary keys
//! do not treat `NULL`s as equal.
//!
//! Complex values (the file list, a diff) are stored as JSON text, like the
//! change summaries: they are only ever read and written whole, never
//! queried by field, so columns would add schema churn for no benefit. The
//! JSON of a diff is what is kept (not the server response), so the model
//! can gain defaulted fields without invalidating the cache.
//!
//! Timestamps are Unix milliseconds in an `INTEGER` column, so age and
//! eviction ordering is numeric.

use std::time::Duration;

use model::{
    ChangeNumber, Diff, DiffKey, DiffPreferences, FileEntry, FilesKey, PatchSetRef, RelatedChange,
    RevisionId, Timestamp,
};
use rusqlite::{OptionalExtension, params};

use crate::{Error, ServerId, Store, number};

/// A cached file list.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedFileList {
    pub files: Vec<FileEntry>,
    /// Paths marked as reviewed by the signed-in user, when known (never
    /// fetched for anonymous sessions).
    pub reviewed: Option<Vec<String>>,
    pub fetched_at: Timestamp,
}

/// A cached diff.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedDiff {
    pub diff: Diff,
    pub fetched_at: Timestamp,
}

/// Cached related changes of one revision.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedRelated {
    pub changes: Vec<RelatedChange>,
    pub fetched_at: Timestamp,
}

/// Cached diff preferences of the signed-in user.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedDiffPreferences {
    pub preferences: DiffPreferences,
    pub fetched_at: Timestamp,
}

/// How much of the review caches to keep, applied by [`Store::prune`].
///
/// Diffs are by far the biggest rows, so they also have a size cap.
/// "Age" is the time since the row was last fetched from the server: the
/// UI refreshes what it shows, so rows that are looked at stay young and
/// the ones of changes nobody opened for a while age out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrunePolicy {
    /// Rows fetched longer ago than this are dropped.
    pub max_age: Duration,
    /// After the age pass, the oldest diffs are dropped until the stored
    /// diff JSON is at most this many bytes.
    pub max_diff_bytes: u64,
}

impl Default for PrunePolicy {
    /// Two weeks, and 256 MiB of diffs.
    fn default() -> Self {
        Self {
            max_age: Duration::from_secs(14 * 24 * 60 * 60),
            max_diff_bytes: 256 * 1024 * 1024,
        }
    }
}

/// What [`Store::prune`] removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub diffs: usize,
    pub file_lists: usize,
    pub related: usize,
}

/// `""` for "compare against the parent".
fn sha(r: &Option<PatchSetRef>) -> &str {
    r.as_ref().map_or("", |r| r.sha.as_str())
}

fn millis(t: Timestamp) -> i64 {
    t.as_millisecond()
}

fn from_millis(ms: i64) -> Result<Timestamp, Error> {
    Timestamp::from_millisecond(ms).map_err(|_| Error::BadTimestamp(ms.to_string()))
}

impl Store {
    /// Stores a file list. `reviewed` of `None` keeps whatever reviewed
    /// paths are already stored for this key.
    pub fn save_file_list(
        &mut self,
        server: ServerId,
        key: &FilesKey,
        files: &[FileEntry],
        reviewed: Option<&[String]>,
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        let reviewed = reviewed.map(serde_json::to_string).transpose()?;
        self.conn.execute(
            "INSERT INTO file_lists
                 (server_id, change_number, base_sha, target_sha, files_json, reviewed_json,
                  fetched_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (server_id, change_number, base_sha, target_sha)
             DO UPDATE SET files_json = excluded.files_json,
                           reviewed_json = COALESCE(excluded.reviewed_json, reviewed_json),
                           fetched_ms = excluded.fetched_ms",
            params![
                server.0,
                number(key.change),
                sha(&key.base),
                key.target.sha.as_str(),
                serde_json::to_string(files)?,
                reviewed,
                millis(fetched_at)
            ],
        )?;
        Ok(())
    }

    pub fn load_file_list(
        &self,
        server: ServerId,
        key: &FilesKey,
    ) -> Result<Option<CachedFileList>, Error> {
        let row: Option<(String, Option<String>, i64)> = self
            .conn
            .query_row(
                "SELECT files_json, reviewed_json, fetched_ms FROM file_lists
                 WHERE server_id = ?1 AND change_number = ?2 AND base_sha = ?3
                   AND target_sha = ?4",
                params![
                    server.0,
                    number(key.change),
                    sha(&key.base),
                    key.target.sha.as_str()
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(files, reviewed, ms)| {
            Ok(CachedFileList {
                files: serde_json::from_str(&files)?,
                reviewed: reviewed.map(|r| serde_json::from_str(&r)).transpose()?,
                fetched_at: from_millis(ms)?,
            })
        })
        .transpose()
    }

    pub fn save_diff(
        &mut self,
        server: ServerId,
        key: &DiffKey,
        diff: &Diff,
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        let json = serde_json::to_string(diff)?;
        self.conn.execute(
            "INSERT INTO diffs
                 (server_id, change_number, base_sha, target_sha, path, whitespace, context,
                  json, size, fetched_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT (server_id, change_number, base_sha, target_sha, path, whitespace,
                          context)
             DO UPDATE SET json = excluded.json, size = excluded.size,
                           fetched_ms = excluded.fetched_ms",
            params![
                server.0,
                number(key.change),
                sha(&key.base),
                key.target.sha.as_str(),
                key.path,
                key.whitespace.as_wire(),
                key.context.to_wire(),
                json,
                i64::try_from(json.len()).unwrap_or(i64::MAX),
                millis(fetched_at)
            ],
        )?;
        Ok(())
    }

    pub fn load_diff(&self, server: ServerId, key: &DiffKey) -> Result<Option<CachedDiff>, Error> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT json, fetched_ms FROM diffs
                 WHERE server_id = ?1 AND change_number = ?2 AND base_sha = ?3
                   AND target_sha = ?4 AND path = ?5 AND whitespace = ?6 AND context = ?7",
                params![
                    server.0,
                    number(key.change),
                    sha(&key.base),
                    key.target.sha.as_str(),
                    key.path,
                    key.whitespace.as_wire(),
                    key.context.to_wire()
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(json, ms)| {
            Ok(CachedDiff {
                diff: serde_json::from_str(&json)?,
                fetched_at: from_millis(ms)?,
            })
        })
        .transpose()
    }

    /// Whether a diff is cached, without reading or parsing it (used to
    /// decide about prefetching).
    pub fn has_diff(&self, server: ServerId, key: &DiffKey) -> Result<bool, Error> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM diffs
                 WHERE server_id = ?1 AND change_number = ?2 AND base_sha = ?3
                   AND target_sha = ?4 AND path = ?5 AND whitespace = ?6 AND context = ?7)",
            params![
                server.0,
                number(key.change),
                sha(&key.base),
                key.target.sha.as_str(),
                key.path,
                key.whitespace.as_wire(),
                key.context.to_wire()
            ],
            |r| r.get(0),
        )?)
    }

    pub fn save_related(
        &mut self,
        server: ServerId,
        change: ChangeNumber,
        revision: &RevisionId,
        related: &[RelatedChange],
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO related_changes
                 (server_id, change_number, revision_sha, json, fetched_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (server_id, change_number, revision_sha)
             DO UPDATE SET json = excluded.json, fetched_ms = excluded.fetched_ms",
            params![
                server.0,
                number(change),
                revision.as_str(),
                serde_json::to_string(related)?,
                millis(fetched_at)
            ],
        )?;
        Ok(())
    }

    pub fn load_related(
        &self,
        server: ServerId,
        change: ChangeNumber,
        revision: &RevisionId,
    ) -> Result<Option<CachedRelated>, Error> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT json, fetched_ms FROM related_changes
                 WHERE server_id = ?1 AND change_number = ?2 AND revision_sha = ?3",
                params![server.0, number(change), revision.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(json, ms)| {
            Ok(CachedRelated {
                changes: serde_json::from_str(&json)?,
                fetched_at: from_millis(ms)?,
            })
        })
        .transpose()
    }

    pub fn save_diff_preferences(
        &mut self,
        server: ServerId,
        preferences: &DiffPreferences,
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO diff_preferences (server_id, json, fetched_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (server_id)
             DO UPDATE SET json = excluded.json, fetched_ms = excluded.fetched_ms",
            params![
                server.0,
                serde_json::to_string(preferences)?,
                millis(fetched_at)
            ],
        )?;
        Ok(())
    }

    pub fn load_diff_preferences(
        &self,
        server: ServerId,
    ) -> Result<Option<CachedDiffPreferences>, Error> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT json, fetched_ms FROM diff_preferences WHERE server_id = ?1",
                [server.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(json, ms)| {
            Ok(CachedDiffPreferences {
                preferences: serde_json::from_str(&json)?,
                fetched_at: from_millis(ms)?,
            })
        })
        .transpose()
    }

    /// Drops stale file lists, diffs and related changes of all servers,
    /// then the oldest diffs while they exceed the size cap. See
    /// [`PrunePolicy`]. Cheap enough to call after every diff save.
    pub fn prune(&mut self, policy: &PrunePolicy, now: Timestamp) -> Result<PruneReport, Error> {
        let max_age_ms = i64::try_from(policy.max_age.as_millis()).unwrap_or(i64::MAX);
        let cutoff = millis(now).saturating_sub(max_age_ms);
        let tx = self.conn.transaction()?;
        let mut report = PruneReport {
            diffs: tx.execute("DELETE FROM diffs WHERE fetched_ms < ?1", [cutoff])?,
            file_lists: tx.execute("DELETE FROM file_lists WHERE fetched_ms < ?1", [cutoff])?,
            related: tx.execute(
                "DELETE FROM related_changes WHERE fetched_ms < ?1",
                [cutoff],
            )?,
        };

        let mut total: i64 =
            tx.query_row("SELECT COALESCE(SUM(size), 0) FROM diffs", [], |r| r.get(0))?;
        let cap = i64::try_from(policy.max_diff_bytes).unwrap_or(i64::MAX);
        if total > cap {
            let mut victims = Vec::new();
            {
                let mut oldest =
                    tx.prepare("SELECT rowid, size FROM diffs ORDER BY fetched_ms, rowid")?;
                let mut rows = oldest.query([])?;
                while total > cap
                    && let Some(row) = rows.next()?
                {
                    victims.push(row.get::<_, i64>(0)?);
                    total -= row.get::<_, i64>(1)?;
                }
            }
            let mut delete = tx.prepare("DELETE FROM diffs WHERE rowid = ?1")?;
            for id in &victims {
                report.diffs += delete.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
