//! Published comments, the user's drafts, and reviewed marks.
//!
//! # Drafts and their sync state
//!
//! A draft is written here first and uploaded afterwards, so editing works
//! offline. Each row has a [`DraftSync`] state:
//!
//! ```text
//!             create_draft                       upload ok
//!   (none) ───────────────▶ PendingCreate ───────────────────┐
//!                               │  ▲ retry_failed_draft       ▼
//!                               │  └───────────────┐       Synced ◀──────────┐
//!                  rejected     ▼                  │         │  ▲             │
//!                          Failed{Create}          │  edit   │  │ upload ok   │
//!                                                  │         ▼  │             │
//!                          Failed{Update} ◀── PendingUpdate ────┘             │
//!                               │ retry_failed_draft ▲                        │
//!                               └────────────────────┘                        │
//!   delete_draft: never uploaded → row removed; else PendingDelete ──ok──▶ (row removed)
//!                                   (rejected → Failed{Delete}, retry → PendingDelete)
//! ```
//!
//! * A *transient* failure (no network, a 502/503/504) leaves the state as
//!   it is and records `last_error` and the attempt count; the retry loop
//!   tries again later.
//! * A *rejection* by the server (403, 409, 400, other statuses) moves the
//!   row to `Failed`; automatic retries skip it. The user edits or deletes
//!   the draft, or asks for a retry.
//! * Editing a draft always makes it pending again and bumps `version`.
//!   An upload reports the version it sent: if the draft changed while the
//!   request was in flight, the result is recorded (the server id is kept)
//!   but the draft stays pending, so the newer text is uploaded next.
//!
//! # Merging the server's list
//!
//! [`Store::merge_server_drafts`] folds the result of `GET /drafts` into the
//! table. Local pending work wins:
//!
//! | local row | server has it | server lacks it |
//! |---|---|---|
//! | none | added as `Synced` | |
//! | `Synced` | overwritten with the server's copy | removed (published or deleted elsewhere), unless it became synced after the fetch started |
//! | `PendingUpdate`, `Failed` | local copy kept | `PendingUpdate`: becomes `PendingCreate` (the text is not lost); `Failed`: kept |
//! | `PendingDelete` | kept (still to delete) | removed (deleted elsewhere: confirmed) |
//! | `PendingCreate` | (has no server id) untouched | untouched |

use model::{
    ChangeNumber, Comment, Draft, DraftInput, DraftOp, DraftSync, LocalDraftId, Location,
    PatchSetNumber, PatchSetRef, RevisionId, Side, Timestamp,
};
use rusqlite::{OptionalExtension, Row, params};

use crate::{
    Error, ServerId, Store, number,
    review_cache::{from_millis, millis},
};

/// The cached published comments of a change.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedComments {
    pub comments: Vec<Comment>,
    pub fetched_at: Timestamp,
}

/// What [`Store::delete_draft`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// There is no such draft.
    NotFound,
    /// The server never had it: the row is gone.
    Removed,
    /// The row stays as `PendingDelete` until the server confirms.
    Pending,
}

/// How a failed upload is recorded, see [`Store::record_draft_failure`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftFailure {
    /// Worth retrying automatically (network, overload).
    Transient(String),
    /// The server refused; `retryable` says whether a manual retry may help.
    Rejected { error: String, retryable: bool },
}

/// What [`Store::merge_server_drafts`] changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    /// Edited drafts whose server copy vanished; they will be created again.
    pub recreated: usize,
    /// Server drafts skipped because their patch set's commit is unknown.
    pub skipped: usize,
}

impl MergeReport {
    pub fn changed(&self) -> bool {
        self.added + self.updated + self.removed + self.recreated > 0
    }
}

const COLUMNS: &str = "id, change_number, patch_set, revision_sha, server_draft_id, path, side, \
     parent, line, start_line, start_character, end_line, end_character, in_reply_to, message, \
     unresolved, updated_ms, state, failed_op, error, retryable, attempts, last_error, version";

#[derive(Debug)]
struct BadColumn(String);

impl std::fmt::Display for BadColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unexpected value {:?}", self.0)
    }
}

impl std::error::Error for BadColumn {}

fn bad(col: usize, v: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        col,
        rusqlite::types::Type::Text,
        Box::new(BadColumn(v.to_owned())),
    )
}

fn op_name(op: DraftOp) -> &'static str {
    match op {
        DraftOp::Create => "create",
        DraftOp::Update => "update",
        DraftOp::Delete => "delete",
    }
}

fn parse_op(col: usize, s: Option<String>) -> rusqlite::Result<DraftOp> {
    match s.as_deref() {
        Some("create") => Ok(DraftOp::Create),
        Some("update") => Ok(DraftOp::Update),
        Some("delete") => Ok(DraftOp::Delete),
        other => Err(bad(col, other.unwrap_or("NULL"))),
    }
}

fn draft_from_row(r: &Row<'_>) -> rusqlite::Result<Draft> {
    let u32_at = |i: usize| -> rusqlite::Result<Option<u32>> {
        Ok(r.get::<_, Option<i64>>(i)?
            .and_then(|v| u32::try_from(v).ok()))
    };
    let line = u32_at(8)?;
    let range = match (u32_at(9)?, u32_at(10)?, u32_at(11)?, u32_at(12)?) {
        (Some(start_line), Some(start_character), Some(end_line), Some(end_character)) => {
            Some(model::CommentRange {
                start_line,
                start_character,
                end_line,
                end_character,
            })
        }
        _ => None,
    };
    let state: String = r.get(17)?;
    let sync = match state.as_str() {
        "synced" => DraftSync::Synced,
        "pending_create" => DraftSync::PendingCreate,
        "pending_update" => DraftSync::PendingUpdate,
        "pending_delete" => DraftSync::PendingDelete,
        "failed" => DraftSync::Failed {
            op: parse_op(18, r.get(18)?)?,
            error: r.get::<_, Option<String>>(19)?.unwrap_or_default(),
            retryable: r.get::<_, Option<bool>>(20)?.unwrap_or(false),
        },
        other => return Err(bad(17, other)),
    };
    let side: String = r.get(6)?;
    let updated_ms: i64 = r.get(16)?;
    Ok(Draft {
        local_id: LocalDraftId(r.get(0)?),
        server_id: r.get(4)?,
        change: ChangeNumber(u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0)),
        patch_set: PatchSetNumber(u32::try_from(r.get::<_, i64>(2)?).unwrap_or(0)),
        revision: RevisionId(r.get(3)?),
        path: r.get(5)?,
        side: match side.as_str() {
            "PARENT" => Side::Parent,
            "REVISION" => Side::Revision,
            other => return Err(bad(6, other)),
        },
        parent: u32_at(7)?,
        location: Location::from_parts(line, range),
        in_reply_to: r.get(13)?,
        message: r.get(14)?,
        unresolved: r.get(15)?,
        updated: from_millis(updated_ms).map_err(|_| bad(16, &updated_ms.to_string()))?,
        sync,
        attempts: u32::try_from(r.get::<_, i64>(21)?).unwrap_or(u32::MAX),
        last_error: r.get(22)?,
        version: u32::try_from(r.get::<_, i64>(23)?).unwrap_or(u32::MAX),
    })
}

/// `(start_line, start_character, end_line, end_character)` of a range.
type RangeColumns = (Option<u32>, Option<u32>, Option<u32>, Option<u32>);

fn range_columns(l: &Location) -> RangeColumns {
    match l.range() {
        Some(r) => (
            Some(r.start_line),
            Some(r.start_character),
            Some(r.end_line),
            Some(r.end_character),
        ),
        None => (None, None, None, None),
    }
}

impl Store {
    // ---- published comments ----

    /// Replaces the cached published comments of a change.
    pub fn save_comments(
        &mut self,
        server: ServerId,
        change: ChangeNumber,
        comments: &[Comment],
        fetched_at: Timestamp,
    ) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO comments (server_id, change_number, json, fetched_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (server_id, change_number)
             DO UPDATE SET json = excluded.json, fetched_ms = excluded.fetched_ms",
            params![
                server.0,
                number(change),
                serde_json::to_string(comments)?,
                millis(fetched_at)
            ],
        )?;
        Ok(())
    }

    pub fn load_comments(
        &self,
        server: ServerId,
        change: ChangeNumber,
    ) -> Result<Option<CachedComments>, Error> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT json, fetched_ms FROM comments WHERE server_id = ?1 AND change_number = ?2",
                params![server.0, number(change)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(json, ms)| {
            Ok(CachedComments {
                comments: serde_json::from_str(&json)?,
                fetched_at: from_millis(ms)?,
            })
        })
        .transpose()
    }

    // ---- drafts: local edits ----

    /// Writes a new draft as `PendingCreate`.
    pub fn create_draft(
        &mut self,
        server: ServerId,
        change: ChangeNumber,
        patch_set: &PatchSetRef,
        input: &DraftInput,
        now: Timestamp,
    ) -> Result<Draft, Error> {
        let (sl, sc, el, ec) = range_columns(&input.location);
        self.conn.execute(
            "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path, side,
                 parent, line, start_line, start_character, end_line, end_character,
                 in_reply_to, message, unresolved, updated_ms, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 'pending_create')",
            params![
                server.0,
                number(change),
                patch_set.number.0,
                patch_set.sha.as_str(),
                input.path,
                input.side.as_wire(),
                input.parent,
                input.location.line(),
                sl,
                sc,
                el,
                ec,
                input.in_reply_to,
                input.message,
                input.unresolved,
                millis(now)
            ],
        )?;
        let id = LocalDraftId(self.conn.last_insert_rowid());
        self.load_draft(server, id)?
            .ok_or_else(|| Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }

    /// Changes the text and resolution of a draft; its place in the file
    /// does not change. Makes the draft pending again (`Synced` becomes
    /// `PendingUpdate`, `Failed` goes back to the operation that failed).
    /// `None` if there is no such draft or it is being deleted.
    pub fn edit_draft(
        &mut self,
        server: ServerId,
        id: LocalDraftId,
        message: &str,
        unresolved: bool,
        now: Timestamp,
    ) -> Result<Option<Draft>, Error> {
        let Some(d) = self.load_draft(server, id)? else {
            return Ok(None);
        };
        let state = match &d.sync {
            DraftSync::PendingDelete => return Ok(None),
            DraftSync::PendingCreate
            | DraftSync::Failed {
                op: DraftOp::Create,
                ..
            } => "pending_create",
            // A failed delete becomes an update: the user wants the draft.
            DraftSync::Synced
            | DraftSync::PendingUpdate
            | DraftSync::Failed {
                op: DraftOp::Update | DraftOp::Delete,
                ..
            } => "pending_update",
        };
        self.conn.execute(
            "UPDATE drafts SET message = ?3, unresolved = ?4, updated_ms = ?5, state = ?6,
                 failed_op = NULL, error = NULL, retryable = NULL, attempts = 0,
                 last_error = NULL, version = version + 1
             WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0, message, unresolved, millis(now), state],
        )?;
        self.load_draft(server, id)
    }

    /// Deletes a draft locally. One the server never saw is removed at
    /// once; one it has stays as `PendingDelete` (hidden from threads)
    /// until the deletion was confirmed.
    pub fn delete_draft(
        &mut self,
        server: ServerId,
        id: LocalDraftId,
    ) -> Result<DeleteOutcome, Error> {
        let Some(d) = self.load_draft(server, id)? else {
            return Ok(DeleteOutcome::NotFound);
        };
        if d.server_id.is_none() {
            self.remove_draft(server, id)?;
            return Ok(DeleteOutcome::Removed);
        }
        self.conn.execute(
            "UPDATE drafts SET state = 'pending_delete', failed_op = NULL, error = NULL,
                 retryable = NULL, attempts = 0, last_error = NULL, version = version + 1
             WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0],
        )?;
        Ok(DeleteOutcome::Pending)
    }

    /// Makes a `Failed` draft pending again, for the operation that
    /// failed. Does nothing for other states.
    pub fn retry_failed_draft(
        &mut self,
        server: ServerId,
        id: LocalDraftId,
    ) -> Result<Option<Draft>, Error> {
        let Some(d) = self.load_draft(server, id)? else {
            return Ok(None);
        };
        if let DraftSync::Failed { op, .. } = d.sync {
            let state = match op {
                DraftOp::Create => "pending_create",
                DraftOp::Update => "pending_update",
                DraftOp::Delete => "pending_delete",
            };
            self.conn.execute(
                "UPDATE drafts SET state = ?3, failed_op = NULL, error = NULL, retryable = NULL,
                     attempts = 0, last_error = NULL
                 WHERE server_id = ?1 AND id = ?2",
                params![server.0, id.0, state],
            )?;
        }
        self.load_draft(server, id)
    }

    // ---- drafts: reading ----

    pub fn load_draft(&self, server: ServerId, id: LocalDraftId) -> Result<Option<Draft>, Error> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM drafts WHERE server_id = ?1 AND id = ?2"),
                params![server.0, id.0],
                draft_from_row,
            )
            .optional()?)
    }

    /// All drafts of a change in creation order, including ones being
    /// deleted (see [`Draft::is_visible`]).
    pub fn load_drafts(&self, server: ServerId, change: ChangeNumber) -> Result<Vec<Draft>, Error> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM drafts WHERE server_id = ?1 AND change_number = ?2 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![server.0, number(change)], draft_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Drafts the automatic upload works on (pending create, update or
    /// delete), oldest first. `Failed` ones are not included.
    pub fn pending_drafts(&self, server: ServerId) -> Result<Vec<Draft>, Error> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM drafts WHERE server_id = ?1
               AND state IN ('pending_create', 'pending_update', 'pending_delete')
             ORDER BY id"
        ))?;
        let rows = stmt.query_map([server.0], draft_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The drafts of a change that the server does not have in their
    /// current form: everything but `Synced` and the ones being deleted.
    pub fn unsynced_drafts(
        &self,
        server: ServerId,
        change: ChangeNumber,
    ) -> Result<Vec<Draft>, Error> {
        Ok(self
            .load_drafts(server, change)?
            .into_iter()
            .filter(|d| d.is_visible() && d.sync.is_pending())
            .collect())
    }

    // ---- drafts: results of uploads ----

    fn remove_draft(&mut self, server: ServerId, id: LocalDraftId) -> Result<(), Error> {
        self.conn.execute(
            "DELETE FROM drafts WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0],
        )?;
        Ok(())
    }

    /// Records that the server has the draft (after a create or update).
    /// `version` is that of the draft as it was sent. If the draft was
    /// edited meanwhile, the server id is kept but the draft stays pending
    /// (as an update). Returns the stored draft, or `None` if it was
    /// deleted locally in the meantime: the caller then deletes the copy it
    /// just created on the server.
    pub fn record_draft_uploaded(
        &mut self,
        server: ServerId,
        id: LocalDraftId,
        version: u32,
        server_draft_id: &str,
        now: Timestamp,
    ) -> Result<Option<Draft>, Error> {
        let Some(d) = self.load_draft(server, id)? else {
            return Ok(None);
        };
        if d.sync == DraftSync::PendingDelete {
            // Deleted while the upload was in flight; the draft exists on
            // the server now, so the pending delete has something to do.
            self.conn.execute(
                "UPDATE drafts SET server_draft_id = ?3 WHERE server_id = ?1 AND id = ?2",
                params![server.0, id.0, server_draft_id],
            )?;
            return self.load_draft(server, id);
        }
        if d.version == version {
            self.conn.execute(
                "UPDATE drafts SET server_draft_id = ?3, state = 'synced', failed_op = NULL,
                     error = NULL, retryable = NULL, attempts = 0, last_error = NULL,
                     synced_ms = ?4
                 WHERE server_id = ?1 AND id = ?2",
                params![server.0, id.0, server_draft_id, millis(now)],
            )?;
        } else {
            self.conn.execute(
                "UPDATE drafts SET server_draft_id = ?3, state = 'pending_update'
                 WHERE server_id = ?1 AND id = ?2",
                params![server.0, id.0, server_draft_id],
            )?;
        }
        self.load_draft(server, id)
    }

    /// Records a failed attempt of the draft's current operation. Ignored
    /// if the draft was edited since the attempt started (`version`
    /// differs): the failure is about text that no longer exists.
    pub fn record_draft_failure(
        &mut self,
        server: ServerId,
        id: LocalDraftId,
        version: u32,
        failure: &DraftFailure,
        now: Timestamp,
    ) -> Result<Option<Draft>, Error> {
        let Some(d) = self.load_draft(server, id)? else {
            return Ok(None);
        };
        if d.version != version {
            return Ok(Some(d));
        }
        match failure {
            DraftFailure::Transient(error) => {
                self.conn.execute(
                    "UPDATE drafts SET attempts = attempts + 1, last_error = ?3,
                         last_attempt_ms = ?4
                     WHERE server_id = ?1 AND id = ?2",
                    params![server.0, id.0, error, millis(now)],
                )?;
            }
            DraftFailure::Rejected { error, retryable } => {
                let Some(op) = d.sync.op() else {
                    return Ok(Some(d));
                };
                self.conn.execute(
                    "UPDATE drafts SET state = 'failed', failed_op = ?3, error = ?4,
                         retryable = ?5, attempts = attempts + 1, last_error = ?4,
                         last_attempt_ms = ?6
                     WHERE server_id = ?1 AND id = ?2",
                    params![server.0, id.0, op_name(op), error, retryable, millis(now)],
                )?;
            }
        }
        self.load_draft(server, id)
    }

    /// The server confirmed that the draft is gone (deleted, or it did not
    /// exist): removes the row of a `PendingDelete` draft. Returns whether
    /// it was removed. A draft that was edited back to life meanwhile
    /// stays.
    pub fn record_draft_deleted(
        &mut self,
        server: ServerId,
        id: LocalDraftId,
    ) -> Result<bool, Error> {
        let n = self.conn.execute(
            "DELETE FROM drafts WHERE server_id = ?1 AND id = ?2 AND state = 'pending_delete'",
            params![server.0, id.0],
        )?;
        Ok(n > 0)
    }

    /// An update found no draft on the server (it was published or
    /// deleted elsewhere): the draft is created again from the local text.
    pub fn restart_draft_as_create(
        &mut self,
        server: ServerId,
        id: LocalDraftId,
    ) -> Result<Option<Draft>, Error> {
        self.conn.execute(
            "UPDATE drafts SET server_draft_id = NULL, state = 'pending_create',
                 failed_op = NULL, error = NULL, retryable = NULL, attempts = 0, last_error = NULL
             WHERE server_id = ?1 AND id = ?2 AND state IN ('pending_update', 'failed')",
            params![server.0, id.0],
        )?;
        self.load_draft(server, id)
    }

    // ---- drafts: merging the server's list ----

    /// Folds `GET /drafts` of one change into the table, see the module
    /// documentation for the rules. `patch_sets` maps patch set numbers to
    /// commits for drafts the server sent without `commit_id`;
    /// `fetch_started` is when the request began, so that a draft uploaded
    /// while it was in flight is not mistaken for one that vanished.
    pub fn merge_server_drafts(
        &mut self,
        server: ServerId,
        change: ChangeNumber,
        server_drafts: &[Comment],
        patch_sets: &[PatchSetRef],
        fetch_started: Timestamp,
        now: Timestamp,
    ) -> Result<MergeReport, Error> {
        let local = self.load_drafts(server, change)?;
        let synced_at: std::collections::HashMap<i64, i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, synced_ms FROM drafts
                 WHERE server_id = ?1 AND change_number = ?2 AND synced_ms IS NOT NULL",
            )?;
            let rows = stmt.query_map(params![server.0, number(change)], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let tx = self.conn.transaction()?;
        let mut report = MergeReport::default();

        for s in server_drafts {
            let mine = local
                .iter()
                .find(|d| d.server_id.as_deref() == Some(s.id.as_str()));
            let (sl, sc, el, ec) = range_columns(&s.location);
            match mine {
                Some(d) if d.sync == DraftSync::Synced => {
                    let same = d.message == s.message
                        && d.unresolved == s.unresolved
                        && d.location == s.location
                        && d.in_reply_to == s.in_reply_to
                        && d.path == s.path
                        && d.patch_set == s.patch_set
                        && d.side == s.side
                        && d.parent == s.parent;
                    if !same {
                        tx.execute(
                            "UPDATE drafts SET patch_set = ?3, path = ?4, side = ?5, parent = ?6,
                                 line = ?7, start_line = ?8, start_character = ?9,
                                 end_line = ?10, end_character = ?11, in_reply_to = ?12,
                                 message = ?13, unresolved = ?14, updated_ms = ?15,
                                 synced_ms = ?16
                             WHERE server_id = ?1 AND id = ?2",
                            params![
                                server.0,
                                d.local_id.0,
                                s.patch_set.0,
                                s.path,
                                s.side.as_wire(),
                                s.parent,
                                s.location.line(),
                                sl,
                                sc,
                                el,
                                ec,
                                s.in_reply_to,
                                s.message,
                                s.unresolved,
                                millis(s.updated),
                                millis(now)
                            ],
                        )?;
                        report.updated += 1;
                    }
                }
                // Local pending work wins.
                Some(_) => {}
                None => {
                    let revision = patch_sets
                        .iter()
                        .find(|p| p.number == s.patch_set)
                        .map(|p| p.sha.clone())
                        .or_else(|| s.commit_id.clone());
                    let Some(revision) = revision else {
                        report.skipped += 1;
                        continue;
                    };
                    tx.execute(
                        "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha,
                             server_draft_id, path, side, parent, line, start_line,
                             start_character, end_line, end_character, in_reply_to, message,
                             unresolved, updated_ms, state, synced_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                             ?15, ?16, ?17, 'synced', ?18)",
                        params![
                            server.0,
                            number(change),
                            s.patch_set.0,
                            revision.as_str(),
                            s.id,
                            s.path,
                            s.side.as_wire(),
                            s.parent,
                            s.location.line(),
                            sl,
                            sc,
                            el,
                            ec,
                            s.in_reply_to,
                            s.message,
                            s.unresolved,
                            millis(s.updated),
                            millis(now)
                        ],
                    )?;
                    report.added += 1;
                }
            }
        }

        for d in &local {
            let Some(sid) = d.server_id.as_deref() else {
                continue;
            };
            if server_drafts.iter().any(|s| s.id == sid) {
                continue;
            }
            // The server no longer has this draft.
            match &d.sync {
                DraftSync::Synced => {
                    let fresh = synced_at
                        .get(&d.local_id.0)
                        .is_some_and(|ms| *ms >= millis(fetch_started));
                    if !fresh {
                        tx.execute("DELETE FROM drafts WHERE id = ?1", [d.local_id.0])?;
                        report.removed += 1;
                    }
                }
                DraftSync::PendingDelete
                | DraftSync::Failed {
                    op: DraftOp::Delete,
                    ..
                } => {
                    tx.execute("DELETE FROM drafts WHERE id = ?1", [d.local_id.0])?;
                    report.removed += 1;
                }
                DraftSync::PendingUpdate => {
                    tx.execute(
                        "UPDATE drafts SET server_draft_id = NULL, state = 'pending_create',
                             attempts = 0, last_error = NULL WHERE id = ?1",
                        [d.local_id.0],
                    )?;
                    report.recreated += 1;
                }
                DraftSync::PendingCreate | DraftSync::Failed { .. } => {}
            }
        }
        tx.commit()?;
        Ok(report)
    }

    // ---- reviewed marks ----

    /// Sets or clears a reviewed mark in every cached file list of the
    /// patch set `target` (the marks belong to the patch set, whatever
    /// base the list compares against). A list without marks yet is
    /// treated as empty. Returns the number of lists changed.
    pub fn set_reviewed_path(
        &mut self,
        server: ServerId,
        change: ChangeNumber,
        target: &RevisionId,
        path: &str,
        reviewed: bool,
    ) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let rows: Vec<(String, Option<String>)> = {
            let mut stmt = tx.prepare(
                "SELECT base_sha, reviewed_json FROM file_lists
                 WHERE server_id = ?1 AND change_number = ?2 AND target_sha = ?3",
            )?;
            let rows = stmt.query_map(params![server.0, number(change), target.as_str()], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let mut changed = 0;
        for (base, json) in rows {
            let mut paths: Vec<String> = match json {
                Some(j) => serde_json::from_str(&j)?,
                None => Vec::new(),
            };
            let present = paths.iter().any(|p| p == path);
            if present == reviewed {
                continue;
            }
            if reviewed {
                paths.push(path.to_owned());
            } else {
                paths.retain(|p| p != path);
            }
            tx.execute(
                "UPDATE file_lists SET reviewed_json = ?5
                 WHERE server_id = ?1 AND change_number = ?2 AND base_sha = ?3
                   AND target_sha = ?4",
                params![
                    server.0,
                    number(change),
                    base,
                    target.as_str(),
                    serde_json::to_string(&paths)?
                ],
            )?;
            changed += 1;
        }
        tx.commit()?;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests;
