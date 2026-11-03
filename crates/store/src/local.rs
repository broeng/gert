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
    ChangeDetail, ChangeNumber, ChangeSummary, LocalChange, LocalChangeId, LocalChangeState,
    LocalPatchSet, LocalPatchSetId, PatchSetNumber, PatchSetRef, RevisionId, Timestamp,
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
    /// See [`Store::has_unsent`].
    pub has_unsent: bool,
    /// When the change became a tombstone (`Consolidated`), by the store's
    /// clock; `None` in other states, and for a tombstone made before this
    /// was recorded.
    pub consolidated_at: Option<Timestamp>,
    /// Since when none of its versions has been on a named branch (by the
    /// complete scans, [`Store::mark_local_versions_on_branch`]); `None`
    /// while one is, or before a scan recorded it.
    pub off_branch_since: Option<Timestamp>,
}

/// A cached Gerrit change found by its Change-Id (see
/// [`Store::cached_changes_by_change_id`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedGerritChange {
    pub summary: ChangeSummary,
    /// Every patch set with its commit, when the change detail is cached
    /// (opening the change page caches it); `None` otherwise.
    pub patch_sets: Option<Vec<PatchSetRef>>,
    /// The current patch set, from the cached detail.
    pub current: Option<PatchSetNumber>,
}

/// What a cached Gerrit change contributes to the classification of local
/// commits, read without deserialising the change (see
/// [`Store::cached_change_keys`]). For comparing, not for display.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CachedChangeKey {
    pub number: ChangeNumber,
    pub change_id: String,
    pub branch: String,
    /// The status as the cache stores it (`"New"`, `"Merged"`, ...).
    pub status: String,
    /// `(number, sha)` of every patch set, when the detail is cached.
    pub patch_sets: Option<Vec<(u32, String)>>,
    /// The current patch set, from the cached detail.
    pub current: Option<u32>,
}

/// A row as the reconcile plan saw it: its id and its state then. An op
/// naming a row whose state has changed since is skipped as stale, so a
/// plan computed from an old snapshot cannot undo what consolidation did
/// in between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub id: LocalChangeId,
    pub state: LocalChangeState,
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
    /// The row to update, as the plan saw it.
    ///
    /// * `None`: the row is found by its key (the reconcile's project,
    ///   `branch` and `change_id`), or created.
    /// * A row that is gone (deleted meanwhile): as `None`, so the commits
    ///   are not dropped.
    /// * A row in another state than seen: skipped, counted as stale.
    /// * Otherwise the key fields are written too, which re-keys the row (a
    ///   commit whose target branch changed). If another row of the project
    ///   already has the new key, the op is skipped and the row reported in
    ///   [`ReconcileReport::conflicts`]; [`LocalOp::Merge`] resolves that.
    pub seen: Option<Seen>,
    pub branch: Option<String>,
    pub change_id: String,
    /// Set when `Some`; `None` keeps what the row has (NULL for a new one).
    /// A local number fails the reconcile with [`Error::LocalTarget`].
    /// When a row gets its first Gerrit change, the drafts and pending
    /// reviews on its synthetic number move onto that change (keeping
    /// their local patch set numbers).
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

/// One step of a reconcile plan (see [`Store::reconcile_local`]). Ops only
/// touch rows of the reconciled server and project; a row that is gone,
/// belongs to another project or is no longer in the state the plan saw
/// is skipped and counted as stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalOp {
    Upsert(LocalChangeUpsert),
    /// A version turned out to be a Gerrit patch set (the same commit). A
    /// local `gerrit_patch_set` fails the reconcile with
    /// [`Error::LocalTarget`].
    MapPatchSet {
        patch_set: LocalPatchSetId,
        gerrit_patch_set: PatchSetNumber,
    },
    /// `Consolidated` is refused (the state stays, and the row is reported
    /// as kept) while the change has unsent data.
    SetState {
        seen: Seen,
        state: LocalChangeState,
    },
    /// Deletes the change as [`Store::delete_local_change`] does, unless it
    /// has unsent data by the time the plan is applied: then it is kept,
    /// reported in [`ReconcileReport::kept`], and moved to `else_state`
    /// (`Vanished`, so it shows dimmed) if one is given.
    Delete {
        seen: Seen,
        else_state: Option<LocalChangeState>,
    },
    /// Folds `from` into `into` (two rows of one change, e.g. when a
    /// re-key collides): `from`'s versions move to `into` with their ids
    /// and numbers, numbered after `into`'s (a version `into` already has
    /// gives its drafts and reviews to `into`'s copy), the drafts on
    /// `from`'s synthetic number and its pending reviews follow, and `from`
    /// is deleted. `into` keeps its fields and state, and takes `from`'s
    /// `gerrit_number` if it has none (moving its own drafts and reviews on
    /// its synthetic number onto that change too). While either row has a
    /// consolidation post in flight (a review being sent, or drafts claimed
    /// by [`Store::claim_local_post`]), the merge is skipped as stale.
    Merge {
        from: Seen,
        into: Seen,
    },
}

/// What [`Store::reconcile_local`] changed.
///
/// The caller must not record the scan's refs fingerprint as done unless
/// [`ReconcileReport::is_settled`]: a stale, kept or conflicting op means
/// the store did not take the plan as computed, and the next scan has to
/// look again even if the refs did not move.
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
    /// Re-keys refused because another row has the new key.
    pub conflicts: Vec<LocalChangeId>,
    /// `(from, into)` of each merge done.
    pub merged: Vec<(LocalChangeId, LocalChangeId)>,
    pub patch_sets_added: usize,
    pub patch_sets_mapped: usize,
    /// Ops that named a row that is gone, belongs to another project, or
    /// is no longer in the state the plan saw.
    pub stale: usize,
}

impl ReconcileReport {
    /// Whether anything was written.
    pub fn changed(&self) -> bool {
        !(self.inserted.is_empty()
            && self.updated.is_empty()
            && self.deleted.is_empty()
            && self.merged.is_empty())
            || self.patch_sets_mapped > 0
    }

    /// Whether the plan was applied as computed: nothing stale, kept or
    /// conflicting.
    pub fn is_settled(&self) -> bool {
        self.stale == 0 && self.kept.is_empty() && self.conflicts.is_empty()
    }
}

const CHANGE_COLUMNS: &str = "id, project, branch, change_id, gerrit_number, subject, \
     author_name, author_email, updated_ms, state, first_seen_ms";

const PATCH_SET_COLUMNS: &str =
    "id, local_change_id, sha, parent_sha, seen_ms, ordinal, gerrit_patch_set, on_branch";

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
        on_branch: r.get(7)?,
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

/// When local change `id` became a tombstone, if it is one and that was
/// recorded.
fn consolidated_at(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
) -> Result<Option<Timestamp>, Error> {
    let ms: Option<Option<i64>> = conn
        .prepare_cached(
            "SELECT consolidated_ms FROM local_changes
             WHERE server_id = ?1 AND id = ?2 AND state = 'consolidated'",
        )?
        .query_row(params![server.0, id.0], |r| r.get(0))
        .optional()?;
    ms.flatten().map(from_millis).transpose()
}

/// Since when none of local change `id`'s versions is on a named branch,
/// if recorded.
fn off_branch_since(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
) -> Result<Option<Timestamp>, Error> {
    let ms: Option<Option<i64>> = conn
        .prepare_cached("SELECT off_branch_ms FROM local_changes WHERE server_id = ?1 AND id = ?2")?
        .query_row(params![server.0, id.0], |r| r.get(0))
        .optional()?;
    ms.flatten().map(from_millis).transpose()
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
        "DELETE FROM local_post_claims WHERE server_id = ?1 AND local_change_id = ?2",
        params![server.0, id.0],
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
    // A tombstone records when it was made (by the store's clock: the
    // callers that settle a change carry no time), so the scan can tell
    // when no page can still name it.
    let consolidated_ms =
        (state == LocalChangeState::Consolidated).then(|| millis(Timestamp::now()));
    conn.execute(
        "UPDATE local_changes SET state = ?3, consolidated_ms = ?4
         WHERE server_id = ?1 AND id = ?2",
        params![server.0, id.0, state.as_str(), consolidated_ms],
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

/// Moves the drafts and pending reviews on local change `id`'s synthetic
/// number onto Gerrit change `gerrit`, keeping their (local) patch set
/// numbers: once a row has a Gerrit change, its local versions are local
/// patch sets of that change, and their data belongs there. Inside the
/// caller's transaction.
pub(crate) fn move_to_gerrit_number(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
    gerrit: ChangeNumber,
) -> Result<(), Error> {
    conn.execute(
        "UPDATE drafts SET change_number = ?3 WHERE server_id = ?1 AND change_number = ?2",
        params![server.0, synthetic(id), number(gerrit)],
    )?;
    conn.execute(
        "UPDATE pending_reviews SET change_number = ?3
         WHERE server_id = ?1 AND change_number = ?2",
        params![server.0, synthetic(id), number(gerrit)],
    )?;
    Ok(())
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
    /// vanished; and rows with a Gerrit change that need the user: closed
    /// ones (found merged or abandoned, their review held) and any other
    /// with held items (held drafts on its versions or reserved for its
    /// post, held or failed reviews). Those open their Gerrit change, whose
    /// held card shows the items. Tombstones are never listed.
    pub fn dashboard_local_changes(&self, server: ServerId) -> Result<Vec<LocalChange>, Error> {
        let numbers = patch_set_numbers("local_changes.id");
        query_changes(
            &self.conn,
            &format!(
                "server_id = ?1 AND (
                     (gerrit_number IS NULL
                      AND state IN ('checking', 'local', 'ambiguous', 'vanished'))
                     OR (gerrit_number IS NOT NULL AND state <> 'consolidated' AND (
                         state = 'closed'
                         OR EXISTS (SELECT 1 FROM pending_reviews r
                             WHERE r.server_id = ?1 AND r.local_change_id = local_changes.id
                               AND r.state IN ('held', 'failed')
                               AND (r.labels <> '{{}}' OR EXISTS (SELECT 1 FROM drafts m
                                   WHERE m.pending_review_id = r.id)))
                         OR EXISTS (SELECT 1 FROM drafts d
                             WHERE d.server_id = ?1 AND d.held_reason IS NOT NULL
                               AND (d.reserved_by = local_changes.id
                                    OR d.patch_set IN {numbers})))))"
            ),
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
                    consolidated_at: consolidated_at(&self.conn, server, change.id)?,
                    off_branch_since: off_branch_since(&self.conn, server, change.id)?,
                    change,
                })
            })
            .collect()
    }

    /// Records which versions of `project`'s local changes are on a named
    /// branch: those whose commit is one of `on_branch`, the commits of a
    /// complete scan ([`model::LocalScan::is_complete`]; never call it
    /// with an incomplete one, whose missing commits may still be there).
    /// A change none of whose versions is on a branch any more records
    /// `now` as when they left (kept while they stay off; cleared when one
    /// is back): a tombstone's age is counted from then. Returns how many
    /// versions changed.
    pub fn mark_local_versions_on_branch(
        &mut self,
        server: ServerId,
        project: &str,
        on_branch: &[RevisionId],
        now: Timestamp,
    ) -> Result<usize, Error> {
        let shas: Vec<&str> = on_branch.iter().map(RevisionId::as_str).collect();
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE local_patch_sets
             SET on_branch = (sha IN (SELECT value FROM json_each(?3)))
             WHERE server_id = ?1
               AND local_change_id IN
                   (SELECT id FROM local_changes WHERE server_id = ?1 AND project = ?2)
               AND on_branch IS NOT (sha IN (SELECT value FROM json_each(?3)))",
            params![server.0, project, serde_json::to_string(&shas)?],
        )?;
        tx.execute(
            "UPDATE local_changes SET off_branch_ms = CASE
                 WHEN EXISTS (SELECT 1 FROM local_patch_sets p
                              WHERE p.local_change_id = local_changes.id AND p.on_branch)
                 THEN NULL
                 ELSE COALESCE(off_branch_ms, ?3) END
             WHERE server_id = ?1 AND project = ?2",
            params![server.0, project, millis(now)],
        )?;
        tx.commit()?;
        Ok(changed)
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
    /// `change_ids`, from any query or change page that fetched them, with
    /// their patch sets when the change detail is cached too. For
    /// classifying local commits: a commit whose SHA is one of the patch
    /// sets is on Gerrit.
    pub fn cached_changes_by_change_id(
        &self,
        server: ServerId,
        project: &str,
        change_ids: &[String],
    ) -> Result<Vec<CachedGerritChange>, Error> {
        if change_ids.is_empty() {
            return Ok(Vec::new());
        }
        // The Change-Id test uses the index `changes_by_change_id`.
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.json, d.json FROM changes c
             LEFT JOIN change_details d ON d.server_id = c.server_id AND d.number = c.number
             WHERE c.server_id = ?1
               AND json_extract(c.json, '$.change_id') IN (SELECT value FROM json_each(?3))
               AND json_extract(c.json, '$.project') = ?2
             ORDER BY c.number",
        )?;
        let rows = stmt.query_map(
            params![server.0, project, serde_json::to_string(change_ids)?],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (summary, detail) = row?;
            let detail: Option<ChangeDetail> =
                detail.map(|d| serde_json::from_str(&d)).transpose()?;
            out.push(CachedGerritChange {
                summary: serde_json::from_str(&summary)?,
                patch_sets: detail
                    .as_ref()
                    .map(|d| d.revisions.iter().map(|r| r.as_ref()).collect()),
                current: detail.and_then(|d| d.current_patch_set),
            });
        }
        Ok(out)
    }

    /// What [`Store::cached_changes_by_change_id`] finds, reduced to what
    /// classifying local commits uses (number, Change-Id, branch, status,
    /// patch sets and the current one), extracted in SQL: cheap enough to
    /// run on every scanner round to see whether the cache moved.
    pub fn cached_change_keys(
        &self,
        server: ServerId,
        project: &str,
        change_ids: &[String],
    ) -> Result<Vec<CachedChangeKey>, Error> {
        if change_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT c.number, json_extract(c.json, '$.change_id'),
                    json_extract(c.json, '$.branch'), json_extract(c.json, '$.status'),
                    d.number IS NOT NULL, json_extract(d.json, '$.current_patch_set'),
                    (SELECT json_group_array(json_array(json_extract(r.value, '$.number'),
                                                        json_extract(r.value, '$.sha')))
                     FROM json_each(d.json, '$.revisions') r)
             FROM changes c
             LEFT JOIN change_details d ON d.server_id = c.server_id AND d.number = c.number
             WHERE c.server_id = ?1
               AND json_extract(c.json, '$.change_id') IN (SELECT value FROM json_each(?3))
               AND json_extract(c.json, '$.project') = ?2
             ORDER BY c.number",
        )?;
        let rows = stmt.query_map(
            params![server.0, project, serde_json::to_string(change_ids)?],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (number, change_id, branch, status, has_detail, current, patch_sets) = row?;
            let patch_sets = match (has_detail, patch_sets) {
                (true, Some(json)) => Some(serde_json::from_str::<Vec<(u32, String)>>(&json)?),
                _ => None,
            };
            out.push(CachedChangeKey {
                number: ChangeNumber(
                    u64::try_from(number).map_err(|_| Error::from(bad(0, &number.to_string())))?,
                ),
                change_id,
                branch: branch.unwrap_or_default(),
                status: status.unwrap_or_default(),
                patch_sets,
                current: current.and_then(|c| u32::try_from(c).ok()),
            });
        }
        Ok(out)
    }

    // ---- writing ----

    /// Applies a reconcile plan for one project of a server in one
    /// transaction (see [`LocalOp`] for each step). `now` is the
    /// `first_seen` of new rows. Fails with [`Error::NoSuchServer`] if the
    /// server row is gone, and with [`Error::LocalTarget`] if the plan names
    /// a local number as a Gerrit change or patch set, writing nothing
    /// either way. Everything else that does not fit the store as it is
    /// now is skipped and reported, never an error, so one odd row cannot
    /// wedge a project's reconcile.
    pub fn reconcile_local(
        &mut self,
        server: ServerId,
        project: &str,
        ops: &[LocalOp],
        now: Timestamp,
    ) -> Result<ReconcileReport, Error> {
        let local_target = ops.iter().any(|op| match op {
            LocalOp::Upsert(u) => u.gerrit_number.is_some_and(ChangeNumber::is_local),
            LocalOp::MapPatchSet {
                gerrit_patch_set, ..
            } => gerrit_patch_set.is_local(),
            _ => false,
        });
        if local_target {
            return Err(Error::LocalTarget);
        }
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
        for op in ops {
            match op {
                LocalOp::Upsert(u) => upsert(&tx, server, project, u, now, &mut report)?,
                LocalOp::MapPatchSet {
                    patch_set,
                    gerrit_patch_set,
                } => {
                    let current: Option<Option<i64>> = tx
                        .query_row(
                            "SELECT p.gerrit_patch_set FROM local_patch_sets p
                             JOIN local_changes c ON c.id = p.local_change_id
                             WHERE p.server_id = ?1 AND p.id = ?2 AND c.project = ?3",
                            params![server.0, patch_set.0, project],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match current {
                        None => report.stale += 1,
                        Some(c) if c == Some(i64::from(gerrit_patch_set.0)) => {}
                        Some(_) => {
                            tx.execute(
                                "UPDATE local_patch_sets SET gerrit_patch_set = ?3
                                 WHERE server_id = ?1 AND id = ?2",
                                params![server.0, patch_set.0, gerrit_patch_set.0],
                            )?;
                            report.patch_sets_mapped += 1;
                        }
                    }
                }
                LocalOp::SetState { seen, state } => {
                    let Some(c) = seen_row(&tx, server, project, *seen)? else {
                        report.stale += 1;
                        continue;
                    };
                    if c.state == *state {
                        continue;
                    }
                    if set_state(&tx, server, c.id, *state)? == Some(*state) {
                        touch(&mut report, c.id);
                    } else {
                        report.kept.push(c.id);
                    }
                }
                LocalOp::Delete { seen, else_state } => {
                    let Some(c) = seen_row(&tx, server, project, *seen)? else {
                        report.stale += 1;
                        continue;
                    };
                    if has_unsent(&tx, server, c.id)? {
                        report.kept.push(c.id);
                        if let Some(s) = else_state
                            && *s != c.state
                            && set_state(&tx, server, c.id, *s)? == Some(*s)
                        {
                            touch(&mut report, c.id);
                        }
                    } else {
                        delete_change(&tx, server, c.id)?;
                        report.deleted.push(c.id);
                        report.updated.retain(|id| *id != c.id);
                    }
                }
                LocalOp::Merge { from, into } => {
                    let rows = (
                        seen_row(&tx, server, project, *from)?,
                        seen_row(&tx, server, project, *into)?,
                    );
                    match rows {
                        (Some(f), Some(i))
                            if f.id != i.id
                                && !posting(&tx, server, f.id)?
                                && !posting(&tx, server, i.id)? =>
                        {
                            merge(&tx, server, &f, &i)?;
                            report.merged.push((f.id, i.id));
                            report.updated.retain(|id| *id != f.id);
                            touch(&mut report, i.id);
                        }
                        _ => report.stale += 1,
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

    /// Deletes a local change that vanished from the checkout with unsent
    /// data once Gerrit was asked and does not have it (the user decision
    /// O1), with all its local data, as [`Store::delete_local_change`]
    /// does. Only if it is still `Vanished` without a Gerrit change, no
    /// post of it is in flight, and none of its data was written by
    /// another account than `me` (Gerrit hides another account's private
    /// change from `me`, so "not found" proves nothing then), checked in
    /// the same transaction: a change whose commits came back meanwhile is
    /// not `Vanished` any more. False otherwise.
    pub fn delete_vanished_local_change(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        me: model::AccountId,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let Some(row) = load_change(&tx, server, change)? else {
            return Ok(false);
        };
        if row.state != LocalChangeState::Vanished
            || row.gerrit_number.is_some()
            || posting(&tx, server, change)?
            || others_data(&tx, server, change, me)?
        {
            return Ok(false);
        }
        let removed = delete_change(&tx, server, change)?;
        tx.commit()?;
        Ok(removed)
    }
}

/// Whether local change `id` has data written by another account than
/// `me`: a pending review or a draft on its numbers with another author.
fn others_data(
    conn: &Connection,
    server: ServerId,
    id: LocalChangeId,
    me: model::AccountId,
) -> Result<bool, Error> {
    let numbers = patch_set_numbers("?3");
    let me = i64::try_from(me.0).unwrap_or(i64::MAX);
    Ok(conn
        .prepare_cached(&format!(
            "SELECT EXISTS (
                 SELECT 1 FROM pending_reviews WHERE server_id = ?1 AND local_change_id = ?3
                   AND author_id IS NOT NULL AND author_id <> ?4
             ) OR EXISTS (
                 SELECT 1 FROM drafts WHERE server_id = ?1
                   AND (change_number = ?2 OR patch_set IN {numbers} OR reserved_by = ?3)
                   AND author_id IS NOT NULL AND author_id <> ?4
             )"
        ))?
        .query_row(params![server.0, synthetic(id), id.0, me], |r| r.get(0))?)
}

/// Whether a consolidation post of local change `id` is in flight: a
/// review being sent, or drafts claimed by a post. Such a row is not
/// merged until the post is finished or released.
fn posting(conn: &Connection, server: ServerId, id: LocalChangeId) -> Result<bool, Error> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM pending_reviews
                     WHERE server_id = ?1 AND local_change_id = ?2 AND state = 'sending')
                 OR EXISTS (SELECT 1 FROM drafts
                     WHERE server_id = ?1 AND reserved_by = ?2 AND posting_nonce IS NOT NULL)",
        )?
        .query_row(params![server.0, id.0], |r| r.get(0))?)
}

/// Records that `id` changed, once.
fn touch(report: &mut ReconcileReport, id: LocalChangeId) {
    if !report.inserted.contains(&id) && !report.updated.contains(&id) {
        report.updated.push(id);
    }
}

/// The row `seen` names, if it is still in the project and in the state
/// the plan saw.
fn seen_row(
    conn: &Connection,
    server: ServerId,
    project: &str,
    seen: Seen,
) -> Result<Option<LocalChange>, Error> {
    Ok(load_change(conn, server, seen.id)?
        .filter(|c| c.project == project && c.state == seen.state))
}

fn upsert(
    conn: &Connection,
    server: ServerId,
    project: &str,
    u: &LocalChangeUpsert,
    now: Timestamp,
    report: &mut ReconcileReport,
) -> Result<(), Error> {
    let by_id = match u.seen {
        Some(seen) => match load_change(conn, server, seen.id)?.filter(|c| c.project == project) {
            Some(c) if c.state != seen.state => {
                report.stale += 1;
                return Ok(());
            }
            found => found,
        },
        None => None,
    };
    let existing = match by_id {
        Some(c) => {
            // Re-keying onto another row's key would break the unique key:
            // leave it to a merge.
            let other = find_by_key(conn, server, project, u.branch.as_deref(), &u.change_id)?;
            if other.is_some_and(|o| o.id != c.id) {
                report.conflicts.push(c.id);
                return Ok(());
            }
            Some(c)
        }
        None => find_by_key(conn, server, project, u.branch.as_deref(), &u.change_id)?,
    };
    let (id, mut changed) = match existing {
        Some(c) => {
            let gerrit_number = u.gerrit_number.or(c.gerrit_number);
            let same = c.branch == u.branch
                && c.change_id == u.change_id
                && c.gerrit_number == gerrit_number
                && c.subject == u.subject
                && c.author_name == u.author_name
                && c.author_email == u.author_email
                && c.updated == u.updated;
            if !same {
                conn.execute(
                    "UPDATE local_changes SET branch = ?3, change_id = ?4, gerrit_number = ?5,
                         subject = ?6, author_name = ?7, author_email = ?8, updated_ms = ?9
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
            if c.gerrit_number.is_none()
                && let Some(g) = gerrit_number
            {
                move_to_gerrit_number(conn, server, c.id, g)?;
            }
            let mut changed = !same;
            if let Some(state) = u.state
                && state != c.state
            {
                if set_state(conn, server, c.id, state)? == Some(state) {
                    changed = true;
                } else {
                    report.kept.push(c.id);
                }
            }
            (c.id, changed)
        }
        None => {
            conn.execute(
                "INSERT INTO local_changes (server_id, project, branch, change_id,
                     gerrit_number, subject, author_name, author_email, updated_ms, state,
                     first_seen_ms)
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
            let id = LocalChangeId(conn.last_insert_rowid());
            report.inserted.push(id);
            (id, false)
        }
    };
    let added = add_patch_sets(conn, server, id, &u.patch_sets)?;
    report.patch_sets_added += added;
    changed |= added > 0;
    if changed {
        touch(report, id);
    }
    Ok(())
}

/// Folds local change `from` into `into`, see [`LocalOp::Merge`].
fn merge(
    conn: &Connection,
    server: ServerId,
    from: &LocalChange,
    into: &LocalChange,
) -> Result<(), Error> {
    let gerrit_number = into.gerrit_number.or(from.gerrit_number);
    // Where drafts and reviews of `from` are posted from now on.
    let target = number(gerrit_number.unwrap_or(into.number));
    let into_versions = load_patch_sets(conn, server, into.id)?;
    let mut versions = Vec::new();
    for p in load_patch_sets(conn, server, from.id)? {
        match into_versions.iter().find(|q| q.sha == p.sha) {
            Some(q) => {
                // The same commit twice: `into`'s copy takes over.
                conn.execute(
                    "UPDATE drafts SET patch_set = ?3 WHERE server_id = ?1 AND patch_set = ?2",
                    params![server.0, p.number.0, q.number.0],
                )?;
                conn.execute(
                    "UPDATE pending_reviews SET patch_set = ?3
                     WHERE server_id = ?1 AND patch_set = ?2",
                    params![server.0, p.number.0, q.number.0],
                )?;
                conn.execute(
                    "DELETE FROM local_patch_sets WHERE server_id = ?1 AND id = ?2",
                    params![server.0, p.id.0],
                )?;
                versions.push(q.number.0);
            }
            None => {
                conn.execute(
                    "UPDATE local_patch_sets SET local_change_id = ?3,
                         ordinal = (SELECT COALESCE(MAX(ordinal), 0) + 1 FROM local_patch_sets
                                    WHERE local_change_id = ?3)
                     WHERE server_id = ?1 AND id = ?2",
                    params![server.0, p.id.0, into.id.0],
                )?;
                versions.push(p.number.0);
            }
        }
    }
    let versions = serde_json::to_string(&versions)?;
    let from_synthetic = number(from.number);
    let from_gerrit = from.gerrit_number.map(number);
    // Drafts: on `from`'s synthetic number, or on the versions it brought
    // (which may carry its Gerrit number), and those reserved for its post.
    conn.execute(
        "UPDATE drafts SET change_number = ?2
         WHERE server_id = ?1 AND (change_number = ?3
               OR (change_number IS ?4 AND patch_set IN (SELECT value FROM json_each(?5))))",
        params![server.0, target, from_synthetic, from_gerrit, versions],
    )?;
    conn.execute(
        "UPDATE drafts SET reserved_by = ?3 WHERE server_id = ?1 AND reserved_by = ?2",
        params![server.0, from.id.0, into.id.0],
    )?;
    conn.execute(
        "UPDATE pending_reviews SET local_change_id = ?3,
             change_number = CASE WHEN change_number = ?4 THEN ?5 ELSE change_number END
         WHERE server_id = ?1 AND local_change_id = ?2",
        params![server.0, from.id.0, into.id.0, from_synthetic, target],
    )?;
    if gerrit_number != into.gerrit_number {
        conn.execute(
            "UPDATE local_changes SET gerrit_number = ?3 WHERE server_id = ?1 AND id = ?2",
            params![server.0, into.id.0, gerrit_number.map(number)],
        )?;
        if let Some(g) = gerrit_number {
            move_to_gerrit_number(conn, server, into.id, g)?;
        }
    }
    delete_change(conn, server, from.id)?;
    Ok(())
}

#[cfg(test)]
mod tests;
