//! Local changes: the user's unpushed commits shown as changes, and their
//! versions (local patch sets).
//!
//! # Rows and numbers
//!
//! A local change is keyed by (server, project, target branch, Change-Id),
//! the branch NULL when unknown. Its synthetic number is
//! [`ChangeNumber::LOCAL_BASE`] plus its row id, and a local patch set's is
//! [`PatchSetNumber::LOCAL_BASE`] plus its row id; ids are never reused.
//! Drafts on a local change or local patch set are rows of `drafts` with
//! those numbers. A local patch set of a *Gerrit* change has a local change
//! row too, with `gerrit_number` set, and its drafts carry the Gerrit change
//! number and the local patch set number.
//!
//! # Who writes what
//!
//! * The scan's reconcile ([`Store::reconcile_local`]) applies a list of
//!   [`LocalOp`]s that `sync` computed from the checkout and the cache, in
//!   one transaction.
//! * Consolidation moves drafts and pending reviews onto the Gerrit change
//!   and sets states (`consolidation.rs`).
//! * Deleting a local change ([`Store::delete_local_change`]) removes its
//!   versions, its pending reviews and its drafts with it.
//!
//! A change has *unsent data* ([`Store::has_unsent`]) while drafts sit on
//! its numbers (held ones included), drafts are reserved for its
//! consolidation post, or one of its pending reviews is not sent. Such a
//! change is never deleted by a reconcile and never marked consolidated.

use model::{
    ChangeNumber, ChangeSummary, LocalChange, LocalChangeId, LocalChangeState, LocalPatchSet,
    LocalPatchSetId, PatchSetNumber, RevisionId, Timestamp,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{
    Error, ServerId, Store,
    drafts::bad,
    number,
    review_cache::{from_millis, millis},
};

/// A local change with its versions, as the reconcile logic needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalChangeEntry {
    pub change: LocalChange,
    /// In the order they were first seen.
    pub patch_sets: Vec<LocalPatchSet>,
    /// Drafts on its numbers, or pending reviews not yet sent: see
    /// [`Store::has_unsent`].
    pub has_unsent: bool,
}

/// A version to add to a local change (see [`LocalChangeUpsert`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewLocalPatchSet {
    pub sha: RevisionId,
    /// The first parent; `None` for a root commit.
    pub parent_sha: Option<RevisionId>,
    pub seen_at: Timestamp,
}

/// Creates a local change or updates one, and adds versions to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalChangeUpsert {
    /// The row to update. `None` finds the row by its key (the reconcile's
    /// project, `branch` and `change_id`) or creates it. With an id, the
    /// key fields are written too, which re-keys the row (a commit whose
    /// target branch changed); that fails the reconcile if another row of
    /// the project already has the new key.
    pub id: Option<LocalChangeId>,
    pub branch: Option<String>,
    pub change_id: String,
    /// Set when `Some`; `None` keeps what the row has (NULL for a new one).
    pub gerrit_number: Option<ChangeNumber>,
    pub subject: String,
    pub author_name: String,
    pub author_email: String,
    pub updated: Timestamp,
    /// Set when `Some` (see [`LocalOp::SetState`] for `Consolidated`);
    /// `None` keeps the row's state, and a new row starts as `Checking`.
    pub state: Option<LocalChangeState>,
    /// Added unless the change already has a version with that SHA; new
    /// ones get the next ordinals in this order.
    pub patch_sets: Vec<NewLocalPatchSet>,
}

/// One step of a reconcile plan (see [`Store::reconcile_local`]). Ops that
/// name a row by id only touch rows of the reconciled server and project;
/// a row that is gone (deleted meanwhile) is skipped and counted as stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalOp {
    Upsert(LocalChangeUpsert),
    /// A version turned out to be a Gerrit patch set (the same commit).
    MapPatchSet {
        patch_set: LocalPatchSetId,
        gerrit_patch_set: PatchSetNumber,
    },
    /// `Consolidated` is refused (the state stays) while the change has
    /// unsent data.
    SetState {
        change: LocalChangeId,
        state: LocalChangeState,
    },
    /// Deletes the change as [`Store::delete_local_change`] does, unless it
    /// has unsent data by the time the plan is applied: then it is kept
    /// and reported in [`ReconcileReport::kept`].
    Delete(LocalChangeId),
}

/// What [`Store::reconcile_local`] changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// New rows, in the order of their upserts.
    pub inserted: Vec<LocalChangeId>,
    /// Rows whose fields, state or versions changed.
    pub updated: Vec<LocalChangeId>,
    pub deleted: Vec<LocalChangeId>,
    /// Deletions and moves to `Consolidated` refused because of unsent
    /// data.
    pub kept: Vec<LocalChangeId>,
    pub patch_sets_added: usize,
    pub patch_sets_mapped: usize,
    /// Ops that named a row that no longer exists in the project.
    pub stale: usize,
}

impl ReconcileReport {
    /// Whether anything was written.
    pub fn changed(&self) -> bool {
        !(self.inserted.is_empty() && self.updated.is_empty() && self.deleted.is_empty())
            || self.patch_sets_mapped > 0
    }
}

const CHANGE_COLUMNS: &str = "id, project, branch, change_id, gerrit_number, subject, \
     author_name, author_email, updated_ms, state, first_seen_ms";

const PATCH_SET_COLUMNS: &str =
    "id, local_change_id, sha, parent_sha, seen_ms, ordinal, gerrit_patch_set";

fn ts_at(r: &Row<'_>, col: usize) -> rusqlite::Result<Timestamp> {
    let ms: i64 = r.get(col)?;
    from_millis(ms).map_err(|_| bad(col, &ms.to_string()))
}

fn change_from_row(r: &Row<'_>) -> rusqlite::Result<LocalChange> {
    let id = LocalChangeId(r.get(0)?);
    let state: String = r.get(9)?;
    Ok(LocalChange {
        id,
        number: id.number().ok_or_else(|| bad(0, &id.0.to_string()))?,
        project: r.get(1)?,
        branch: r.get(2)?,
        change_id: r.get(3)?,
        gerrit_number: r
            .get::<_, Option<i64>>(4)?
            .map(|n| u64::try_from(n).map(ChangeNumber))
            .transpose()
            .map_err(|_| bad(4, "negative change number"))?,
        subject: r.get(5)?,
        author_name: r.get(6)?,
        author_email: r.get(7)?,
        updated: ts_at(r, 8)?,
        state: LocalChangeState::parse(&state).ok_or_else(|| bad(9, &state))?,
        first_seen: ts_at(r, 10)?,
    })
}

fn patch_set_from_row(r: &Row<'_>) -> rusqlite::Result<LocalPatchSet> {
    let id = LocalPatchSetId(r.get(0)?);
    Ok(LocalPatchSet {
        id,
        number: id.number().ok_or_else(|| bad(0, &id.0.to_string()))?,
        local_change: LocalChangeId(r.get(1)?),
        sha: RevisionId(r.get(2)?),
        parent_sha: r.get::<_, Option<String>>(3)?.map(RevisionId),
        seen_at: ts_at(r, 4)?,
        ordinal: u32::try_from(r.get::<_, i64>(5)?).map_err(|_| bad(5, "ordinal"))?,
        gerrit_patch_set: r
            .get::<_, Option<i64>>(6)?
            .map(|n| u32::try_from(n).map(PatchSetNumber))
            .transpose()
            .map_err(|_| bad(6, "patch set number"))?,
    })
}

/// The synthetic change number of `id` as stored in `drafts` and
/// `pending_reviews`.
fn synthetic(id: LocalChangeId) -> i64 {
    id.number().map_or(i64::MAX, number)
}

/// An SQL list of the synthetic patch set numbers of local change `?id`
/// (the parameter named in `id`).
pub(crate) fn patch_set_numbers(id: &str) -> String {
    format!(
        "(SELECT {} + id FROM local_patch_sets WHERE local_change_id = {id})",
        PatchSetNumber::LOCAL_BASE
    )
}

pub(crate) fn load_change(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
) -> Result<Option<LocalChange>, Error> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {CHANGE_COLUMNS} FROM local_changes WHERE server_id = ?1 AND id = ?2"
        ))?
        .query_row(params![server.0, id.0], change_from_row)
        .optional()?)
}

pub(crate) fn load_patch_sets(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
) -> Result<Vec<LocalPatchSet>, Error> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PATCH_SET_COLUMNS} FROM local_patch_sets
         WHERE server_id = ?1 AND local_change_id = ?2 ORDER BY ordinal"
    ))?;
    let rows = stmt.query_map(params![server.0, id.0], patch_set_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn query_changes(
    conn: &Connection,
    sql_where: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<LocalChange>, Error> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {CHANGE_COLUMNS} FROM local_changes WHERE {sql_where}
         ORDER BY updated_ms DESC, id DESC"
    ))?;
    let rows = stmt.query_map(args, change_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn has_unsent(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
) -> Result<bool, Error> {
    let numbers = patch_set_numbers("?3");
    Ok(conn
        .prepare_cached(&format!(
            "SELECT EXISTS (
                 SELECT 1 FROM drafts WHERE server_id = ?1
                   AND (change_number = ?2 OR patch_set IN {numbers} OR reserved_by = ?3)
             ) OR EXISTS (
                 SELECT 1 FROM pending_reviews WHERE server_id = ?1 AND state <> 'sent'
                   AND (local_change_id = ?3 OR change_number = ?2 OR patch_set IN {numbers})
             )"
        ))?
        .query_row(params![server.0, synthetic(id), id.0], |r| r.get(0))?)
}

/// Deletes a local change and everything on its numbers, inside the
/// caller's transaction. False if there is no such row.
fn delete_change(conn: &Connection, server: ServerId, id: LocalChangeId) -> Result<bool, Error> {
    let numbers = patch_set_numbers("?3");
    // Drafts first: on its synthetic number, on its local patch sets (also
    // on a Gerrit change's number), those reserved for its post, and the
    // message drafts of its pending reviews wherever consolidation moved
    // them.
    conn.execute(
        &format!(
            "DELETE FROM drafts WHERE server_id = ?1 AND (
                 change_number = ?2 OR patch_set IN {numbers} OR reserved_by = ?3
                 OR pending_review_id IN (
                     SELECT id FROM pending_reviews WHERE server_id = ?1
                       AND (local_change_id = ?3 OR change_number = ?2
                            OR patch_set IN {numbers})))"
        ),
        params![server.0, synthetic(id), id.0],
    )?;
    conn.execute(
        &format!(
            "DELETE FROM pending_reviews WHERE server_id = ?1
               AND (local_change_id = ?3 OR change_number = ?2 OR patch_set IN {numbers})"
        ),
        params![server.0, synthetic(id), id.0],
    )?;
    conn.execute(
        "DELETE FROM local_patch_sets WHERE server_id = ?1 AND local_change_id = ?2",
        params![server.0, id.0],
    )?;
    let n = conn.execute(
        "DELETE FROM local_changes WHERE server_id = ?1 AND id = ?2",
        params![server.0, id.0],
    )?;
    Ok(n > 0)
}

/// Sets a change's state, refusing `Consolidated` while it has unsent
/// data. Returns the state afterwards, `None` if the row is gone.
pub(crate) fn set_state(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
    state: LocalChangeState,
) -> Result<Option<LocalChangeState>, Error> {
    let Some(current) = load_change(conn, server, id)?.map(|c| c.state) else {
        return Ok(None);
    };
    if current == state {
        return Ok(Some(current));
    }
    if state == LocalChangeState::Consolidated && has_unsent(conn, server, id)? {
        return Ok(Some(current));
    }
    conn.execute(
        "UPDATE local_changes SET state = ?3 WHERE server_id = ?1 AND id = ?2",
        params![server.0, id.0, state.as_str()],
    )?;
    Ok(Some(state))
}

/// Adds the versions the change does not have yet. Returns how many.
fn add_patch_sets(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
    patch_sets: &[NewLocalPatchSet],
) -> Result<usize, Error> {
    let mut added = 0;
    for p in patch_sets {
        added += conn
            .prepare_cached(
                "INSERT INTO local_patch_sets
                     (local_change_id, server_id, sha, parent_sha, seen_ms, ordinal)
                 SELECT ?1, ?2, ?3, ?4, ?5,
                     (SELECT COALESCE(MAX(ordinal), 0) + 1 FROM local_patch_sets
                      WHERE local_change_id = ?1)
                 WHERE NOT EXISTS (
                     SELECT 1 FROM local_patch_sets WHERE local_change_id = ?1 AND sha = ?3)",
            )?
            .execute(params![
                id.0,
                server.0,
                p.sha.as_str(),
                p.parent_sha.as_ref().map(RevisionId::as_str),
                millis(p.seen_at)
            ])?;
    }
    Ok(added)
}

/// The row an upsert without an id is about.
fn find_by_key(
    conn: &Connection,
    server: ServerId,
    project: &str,
    branch: Option<&str>,
    change_id: &str,
) -> Result<Option<LocalChange>, Error> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {CHANGE_COLUMNS} FROM local_changes
             WHERE server_id = ?1 AND project = ?2 AND branch IS ?3 AND change_id = ?4"
        ))?
        .query_row(
            params![server.0, project, branch, change_id],
            change_from_row,
        )
        .optional()?)
}

impl Store {
    // ---- reading ----

    pub fn local_change(
        &self,
        server: ServerId,
        id: LocalChangeId,
    ) -> Result<Option<LocalChange>, Error> {
        load_change(&self.conn, server, id)
    }

    /// The local change with this synthetic number; `None` for a server's
    /// change number.
    pub fn local_change_by_number(
        &self,
        server: ServerId,
        change: ChangeNumber,
    ) -> Result<Option<LocalChange>, Error> {
        match change.local_id() {
            Some(id) => self.local_change(server, LocalChangeId(id)),
            None => Ok(None),
        }
    }

    /// Every local change row of the server in any state, tombstones
    /// included, the most recently updated first.
    pub fn local_changes(&self, server: ServerId) -> Result<Vec<LocalChange>, Error> {
        query_changes(&self.conn, "server_id = ?1", [server.0])
    }

    /// The local changes the dashboard lists, the most recently updated
    /// first: changes of their own (not versions of a Gerrit change, and
    /// not yet found on Gerrit) that are checking, local, ambiguous or
    /// vanished. Closed and consolidated rows belong to their Gerrit change
    /// and show there.
    pub fn dashboard_local_changes(&self, server: ServerId) -> Result<Vec<LocalChange>, Error> {
        query_changes(
            &self.conn,
            "server_id = ?1 AND gerrit_number IS NULL
               AND state IN ('checking', 'local', 'ambiguous', 'vanished')",
            [server.0],
        )
    }

    /// The local changes with this Change-Id, in any project or branch.
    pub fn local_changes_by_change_id(
        &self,
        server: ServerId,
        change_id: &str,
    ) -> Result<Vec<LocalChange>, Error> {
        query_changes(
            &self.conn,
            "server_id = ?1 AND change_id = ?2",
            params![server.0, change_id],
        )
    }

    /// The local change rows that hold versions of (or became) Gerrit
    /// change `gerrit`.
    pub fn local_changes_of_gerrit_change(
        &self,
        server: ServerId,
        gerrit: ChangeNumber,
    ) -> Result<Vec<LocalChange>, Error> {
        query_changes(
            &self.conn,
            "server_id = ?1 AND gerrit_number = ?2",
            params![server.0, number(gerrit)],
        )
    }

    /// Every local change of a project with its versions and whether it
    /// has unsent data: the store's side of a reconcile.
    pub fn local_changes_of_project(
        &self,
        server: ServerId,
        project: &str,
    ) -> Result<Vec<LocalChangeEntry>, Error> {
        let changes = query_changes(
            &self.conn,
            "server_id = ?1 AND project = ?2",
            params![server.0, project],
        )?;
        changes
            .into_iter()
            .map(|change| {
                Ok(LocalChangeEntry {
                    patch_sets: load_patch_sets(&self.conn, server, change.id)?,
                    has_unsent: has_unsent(&self.conn, server, change.id)?,
                    change,
                })
            })
            .collect()
    }

    /// The versions of a local change in the order they were first seen.
    pub fn local_patch_sets(
        &self,
        server: ServerId,
        change: LocalChangeId,
    ) -> Result<Vec<LocalPatchSet>, Error> {
        load_patch_sets(&self.conn, server, change)
    }

    /// The local patch set with this synthetic number; `None` for a
    /// server's patch set number.
    pub fn local_patch_set(
        &self,
        server: ServerId,
        patch_set: PatchSetNumber,
    ) -> Result<Option<LocalPatchSet>, Error> {
        let Some(id) = patch_set.local_id() else {
            return Ok(None);
        };
        Ok(self
            .conn
            .prepare_cached(&format!(
                "SELECT {PATCH_SET_COLUMNS} FROM local_patch_sets WHERE server_id = ?1 AND id = ?2"
            ))?
            .query_row(params![server.0, id], patch_set_from_row)
            .optional()?)
    }

    /// The local patch sets of Gerrit change `gerrit`, over all its local
    /// change rows, in the order they were first seen. Includes versions
    /// that are mapped to a Gerrit patch set (the caller filters on
    /// `gerrit_patch_set`).
    pub fn local_patch_sets_of_gerrit_change(
        &self,
        server: ServerId,
        gerrit: ChangeNumber,
    ) -> Result<Vec<LocalPatchSet>, Error> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {PATCH_SET_COLUMNS} FROM local_patch_sets
             WHERE server_id = ?1 AND local_change_id IN (
                 SELECT id FROM local_changes WHERE server_id = ?1 AND gerrit_number = ?2)
             ORDER BY seen_ms, id"
        ))?;
        let rows = stmt.query_map(params![server.0, number(gerrit)], patch_set_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The local patch sets with this commit. Usually one; the same commit
    /// can be a version in two projects (forks) or in two rows of a change
    /// whose target branch changed.
    pub fn local_patch_sets_by_sha(
        &self,
        server: ServerId,
        sha: &RevisionId,
    ) -> Result<Vec<LocalPatchSet>, Error> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {PATCH_SET_COLUMNS} FROM local_patch_sets
             WHERE server_id = ?1 AND sha = ?2 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![server.0, sha.as_str()], patch_set_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Whether `sha` is a local patch set that is not known to be a Gerrit
    /// patch set: a commit no revision request may name. A version mapped
    /// to a Gerrit patch set is on the server, so requests may use it.
    pub fn is_local_sha(&self, server: ServerId, sha: &RevisionId) -> Result<bool, Error> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM local_patch_sets
                     WHERE server_id = ?1 AND sha = ?2 AND gerrit_patch_set IS NULL)",
            )?
            .query_row(params![server.0, sha.as_str()], |r| r.get(0))?)
    }

    /// Whether a local change has data that is not on Gerrit yet: drafts on
    /// its synthetic number or its local patch sets (held ones included),
    /// drafts reserved for its consolidation post, or pending reviews that
    /// are not sent (also after consolidation moved them onto the Gerrit
    /// change). False for an unknown id.
    pub fn has_unsent(&self, server: ServerId, change: LocalChangeId) -> Result<bool, Error> {
        has_unsent(&self.conn, server, change)
    }

    /// The cached Gerrit changes of `project` whose Change-Id is one of
    /// `change_ids`, from any query or change page that fetched them. For
    /// classifying local commits.
    pub fn cached_changes_by_change_id(
        &self,
        server: ServerId,
        project: &str,
        change_ids: &[String],
    ) -> Result<Vec<ChangeSummary>, Error> {
        if change_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT json FROM changes WHERE server_id = ?1
               AND json_extract(json, '$.project') = ?2
               AND json_extract(json, '$.change_id') IN (SELECT value FROM json_each(?3))
             ORDER BY number",
        )?;
        let rows = stmt.query_map(
            params![server.0, project, serde_json::to_string(change_ids)?],
            |r| r.get::<_, String>(0),
        )?;
        let mut out = Vec::new();
        for json in rows {
            out.push(serde_json::from_str(&json?)?);
        }
        Ok(out)
    }

    // ---- writing ----

    /// Applies a reconcile plan for one project of a server in one
    /// transaction (see [`LocalOp`] for each step). `now` is the
    /// `first_seen` of new rows. Fails with [`Error::NoSuchServer`] if the
    /// server row is gone, writing nothing.
    pub fn reconcile_local(
        &mut self,
        server: ServerId,
        project: &str,
        ops: &[LocalOp],
        now: Timestamp,
    ) -> Result<ReconcileReport, Error> {
        let tx = self.conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM servers WHERE id = ?1)",
            [server.0],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::NoSuchServer);
        }
        let mut report = ReconcileReport::default();
        let in_project = |id: LocalChangeId| -> Result<Option<LocalChange>, Error> {
            Ok(load_change(&tx, server, id)?.filter(|c| c.project == project))
        };
        for op in ops {
            match op {
                LocalOp::Upsert(u) => {
                    let existing = match u.id {
                        Some(id) => match in_project(id)? {
                            Some(c) => Some(c),
                            None => {
                                report.stale += 1;
                                continue;
                            }
                        },
                        None => {
                            find_by_key(&tx, server, project, u.branch.as_deref(), &u.change_id)?
                        }
                    };
                    let (id, mut changed) = match existing {
                        Some(c) => {
                            let gerrit_number = u.gerrit_number.or(c.gerrit_number);
                            let state = u.state.unwrap_or(c.state);
                            let same = c.branch == u.branch
                                && c.change_id == u.change_id
                                && c.gerrit_number == gerrit_number
                                && c.subject == u.subject
                                && c.author_name == u.author_name
                                && c.author_email == u.author_email
                                && c.updated == u.updated;
                            if !same {
                                tx.execute(
                                    "UPDATE local_changes SET branch = ?3, change_id = ?4,
                                         gerrit_number = ?5, subject = ?6, author_name = ?7,
                                         author_email = ?8, updated_ms = ?9
                                     WHERE server_id = ?1 AND id = ?2",
                                    params![
                                        server.0,
                                        c.id.0,
                                        u.branch,
                                        u.change_id,
                                        gerrit_number.map(number),
                                        u.subject,
                                        u.author_name,
                                        u.author_email,
                                        millis(u.updated)
                                    ],
                                )?;
                            }
                            let mut changed = !same;
                            if state != c.state {
                                match set_state(&tx, server, c.id, state)? {
                                    Some(s) if s == state => changed = true,
                                    _ => report.kept.push(c.id),
                                }
                            }
                            (c.id, changed)
                        }
                        None => {
                            tx.execute(
                                "INSERT INTO local_changes (server_id, project, branch,
                                     change_id, gerrit_number, subject, author_name,
                                     author_email, updated_ms, state, first_seen_ms)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                                params![
                                    server.0,
                                    project,
                                    u.branch,
                                    u.change_id,
                                    u.gerrit_number.map(number),
                                    u.subject,
                                    u.author_name,
                                    u.author_email,
                                    millis(u.updated),
                                    u.state.unwrap_or(LocalChangeState::Checking).as_str(),
                                    millis(now)
                                ],
                            )?;
                            let id = LocalChangeId(tx.last_insert_rowid());
                            report.inserted.push(id);
                            (id, false)
                        }
                    };
                    let added = add_patch_sets(&tx, server, id, &u.patch_sets)?;
                    report.patch_sets_added += added;
                    changed |= added > 0;
                    if changed && !report.inserted.contains(&id) && !report.updated.contains(&id) {
                        report.updated.push(id);
                    }
                }
                LocalOp::MapPatchSet {
                    patch_set,
                    gerrit_patch_set,
                } => {
                    let n = tx.execute(
                        "UPDATE local_patch_sets SET gerrit_patch_set = ?3
                         WHERE server_id = ?1 AND id = ?2 AND gerrit_patch_set IS NOT ?3
                           AND local_change_id IN (
                               SELECT id FROM local_changes WHERE server_id = ?1 AND project = ?4)",
                        params![server.0, patch_set.0, gerrit_patch_set.0, project],
                    )?;
                    report.patch_sets_mapped += n;
                }
                LocalOp::SetState { change, state } => {
                    let Some(c) = in_project(*change)? else {
                        report.stale += 1;
                        continue;
                    };
                    if c.state == *state {
                        continue;
                    }
                    match set_state(&tx, server, c.id, *state)? {
                        Some(s) if s == *state => {
                            if !report.updated.contains(&c.id) {
                                report.updated.push(c.id);
                            }
                        }
                        _ => report.kept.push(c.id),
                    }
                }
                LocalOp::Delete(change) => {
                    if in_project(*change)?.is_none() {
                        report.stale += 1;
                    } else if has_unsent(&tx, server, *change)? {
                        report.kept.push(*change);
                    } else {
                        delete_change(&tx, server, *change)?;
                        report.deleted.push(*change);
                        report.updated.retain(|id| id != change);
                    }
                }
            }
        }
        tx.commit()?;
        Ok(report)
    }

    /// Sets a local change's state, outside a reconcile (a lookup found it,
    /// or did not). `Consolidated` is refused while the change has unsent
    /// data, checked in the same transaction. Returns the state afterwards,
    /// or `None` if the change (or its server) is gone.
    pub fn set_local_change_state(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        state: LocalChangeState,
    ) -> Result<Option<LocalChangeState>, Error> {
        let tx = self.conn.transaction()?;
        let now = set_state(&tx, server, change, state)?;
        tx.commit()?;
        Ok(now)
    }

    /// Deletes a local change with its versions, its pending reviews and
    /// its drafts (on its synthetic number, on its local patch sets, those
    /// reserved for its consolidation post, and its pending reviews'
    /// message drafts) in one transaction, whatever is unsent. False if
    /// there is no such change.
    pub fn delete_local_change(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let removed = delete_change(&tx, server, change)?;
        tx.commit()?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests;
