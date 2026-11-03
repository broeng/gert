//! Pending reviews: votes and a message given on a local patch set, kept
//! until what they were given on is on Gerrit.
//!
//! ```text
//!   create ──▶ Pending ──mark_sending──▶ Sending ──▶ Sent
//!               │  edit, cancel             │
//!               │                           ├──▶ Failed ─┐
//!               └──────────▶ Held ◀─────────┘            │
//!                             │  mark_sending (publish)  │ (retry)
//!                             └──────────▶ Sending ◀─────┘
//! ```
//!
//! Only a `Pending` review can be edited or cancelled; a `Held` or `Failed`
//! one can be dropped. [`Store::mark_pending_review_sending`] is a
//! compare-and-set from the states the caller names, so two consolidation
//! runs cannot both send the same review, and it returns the review as it
//! was claimed. Every other transition goes through
//! [`Store::set_pending_review_state`], which names the states it may start
//! from.
//!
//! # The message
//!
//! The review's message is not a column: it is a patch-set-level draft on
//! the review's patch set with `pending_review_id` set (*sealed*). A sealed
//! draft is never uploaded as a draft: it is posted as the review's
//! message. So it is plain local text outside the draft state machine:
//! [`Store::update_pending_review`] changes or removes the row directly,
//! it never gets a server draft id, and `edit_draft` / `delete_draft`
//! refuse it. [`PendingReview::message`] is a read-only copy of its text.
//! The review's inline comments are ordinary drafts on the local patch
//! set, not linked to it.

use std::collections::BTreeMap;

use model::{
    ChangeNumber, LocalApproval, LocalChangeId, LocalChangeState, LocalDraftId,
    PATCHSET_LEVEL_PATH, PatchSetNumber, PendingReview, PendingReviewId, PendingReviewState,
    RevisionId, Side, Timestamp,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{
    Error, ServerId, Store,
    drafts::bad,
    number,
    review_cache::{from_millis, millis},
};

/// What [`Store::create_pending_review`] takes. The local change, the
/// change number and the commit come from the patch set's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPendingReview {
    /// A local patch set.
    pub patch_set: PatchSetNumber,
    pub labels: BTreeMap<String, i32>,
    pub strategy: LocalApproval,
    /// The review message; empty or blank means none.
    pub message: String,
    /// Whether the message asks for something to be done.
    pub unresolved: bool,
    /// Also the first `voted_at`.
    pub created: Timestamp,
    /// An ordinary patch-set-level draft on the same local patch set that
    /// held the message until now (the reply dialog's text, kept while the
    /// dialog was closed): removed in the same transaction, since the
    /// review's sealed message takes its place. A draft that is anything
    /// else (sealed, a reply, on another patch set) is left alone.
    pub replaces_draft: Option<LocalDraftId>,
}

/// What [`Store::update_pending_review`] replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReviewEdit {
    pub labels: BTreeMap<String, i32>,
    /// The project's mode now. Taken only when the labels change: editing
    /// the votes is voting again, editing the message is not.
    pub strategy: LocalApproval,
    /// Empty or blank removes the message.
    pub message: String,
    pub unresolved: bool,
}

const COLUMNS: &str = "r.id, r.local_change_id, r.change_number, r.patch_set, r.revision_sha, \
     r.given_on_sha, r.labels, r.strategy, r.state, r.error, r.held_reason, r.created_ms, \
     r.voted_at_ms, r.nonce, d.message, d.id, d.unresolved";

const FROM: &str = "pending_reviews r LEFT JOIN drafts d ON d.pending_review_id = r.id";

fn review_from_row(r: &Row<'_>) -> rusqlite::Result<PendingReview> {
    let ts = |col: usize| -> rusqlite::Result<Timestamp> {
        let ms: i64 = r.get(col)?;
        from_millis(ms).map_err(|_| bad(col, &ms.to_string()))
    };
    let labels: String = r.get(6)?;
    let strategy: String = r.get(7)?;
    let state: String = r.get(8)?;
    Ok(PendingReview {
        id: PendingReviewId(r.get(0)?),
        local_change: LocalChangeId(r.get(1)?),
        change: ChangeNumber(u64::try_from(r.get::<_, i64>(2)?).map_err(|_| bad(2, "number"))?),
        patch_set: PatchSetNumber(
            u32::try_from(r.get::<_, i64>(3)?).map_err(|_| bad(3, "patch set"))?,
        ),
        revision: RevisionId(r.get(4)?),
        given_on_sha: RevisionId(r.get(5)?),
        labels: serde_json::from_str(&labels).map_err(|_| bad(6, &labels))?,
        // A mode this build does not know is taken as the strictest.
        strategy: LocalApproval::parse(&strategy).unwrap_or(LocalApproval::Exact),
        state: PendingReviewState::parse(&state).ok_or_else(|| bad(8, &state))?,
        error: r.get(9)?,
        held_reason: r.get(10)?,
        created: ts(11)?,
        voted_at: ts(12)?,
        nonce: r.get(13)?,
        message: r.get(14)?,
        message_draft: r.get::<_, Option<i64>>(15)?.map(LocalDraftId),
        unresolved: r.get::<_, Option<bool>>(16)?.unwrap_or(false),
    })
}

pub(crate) fn load(
    conn: &Connection,
    server: ServerId,
    id: PendingReviewId,
) -> Result<Option<PendingReview>, Error> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM {FROM} WHERE r.server_id = ?1 AND r.id = ?2"
        ))?
        .query_row(params![server.0, id.0], review_from_row)
        .optional()?)
}

/// The reviews of `server` matching `cond` (over `r.`, with `?1` the
/// server), in creation order.
pub(crate) fn query(
    conn: &Connection,
    cond: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<PendingReview>, Error> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM {FROM} WHERE {cond} ORDER BY r.id"
    ))?;
    let rows = stmt.query_map(args, review_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Moves review `id` to `to` if it is in one of the states `from` (and,
/// with `owner`, belongs to that local change). `note` becomes the error of
/// a `Failed` review and the reason of a `Held` one; both are cleared
/// otherwise. Returns whether it moved.
pub(crate) fn transition(
    conn: &Connection,
    server: ServerId,
    id: PendingReviewId,
    owner: Option<LocalChangeId>,
    from: &[PendingReviewState],
    to: PendingReviewState,
    note: Option<&str>,
) -> Result<bool, Error> {
    let Some(review) = load(conn, server, id)? else {
        return Ok(false);
    };
    if !from.contains(&review.state) || owner.is_some_and(|o| o != review.local_change) {
        return Ok(false);
    }
    let error = (to == PendingReviewState::Failed).then_some(note).flatten();
    let held = (to == PendingReviewState::Held).then_some(note).flatten();
    conn.execute(
        "UPDATE pending_reviews SET state = ?4, error = ?5, held_reason = ?6
         WHERE server_id = ?1 AND id = ?2 AND state = ?3",
        params![
            server.0,
            id.0,
            review.state.as_str(),
            to.as_str(),
            error,
            held
        ],
    )?;
    Ok(true)
}

/// Creates, updates or removes the review's sealed message draft so that it
/// holds `message` (none when blank). Plain row writes: a sealed draft is
/// never uploaded, so it has no sync state to keep.
fn write_message(
    conn: &Connection,
    server: ServerId,
    review: &PendingReview,
    message: &str,
    unresolved: bool,
    now: Timestamp,
) -> Result<(), Error> {
    match (review.message_draft, message.trim().is_empty()) {
        (Some(d), true) => {
            conn.execute(
                "DELETE FROM drafts WHERE server_id = ?1 AND id = ?2",
                params![server.0, d.0],
            )?;
        }
        (Some(d), false) => {
            conn.execute(
                "UPDATE drafts SET message = ?3, unresolved = ?4, updated_ms = ?5,
                     version = version + 1
                 WHERE server_id = ?1 AND id = ?2",
                params![server.0, d.0, message, unresolved, millis(now)],
            )?;
        }
        (None, true) => {}
        (None, false) => {
            // A review that consolidation already moved has its message
            // reserved for the post like the rest of its drafts.
            let reserved = (!review.patch_set.is_local()).then_some(review.local_change.0);
            conn.execute(
                "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path,
                     side, message, unresolved, updated_ms, state, pending_review_id,
                     reserved_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending_create', ?10, ?11)",
                params![
                    server.0,
                    number(review.change),
                    review.patch_set.0,
                    review.revision.as_str(),
                    PATCHSET_LEVEL_PATH,
                    Side::Revision.as_wire(),
                    message,
                    unresolved,
                    millis(now),
                    review.id.0,
                    reserved
                ],
            )?;
        }
    }
    Ok(())
}

fn missing() -> Error {
    Error::Sqlite(rusqlite::Error::QueryReturnedNoRows)
}

impl Store {
    /// Saves a review given on a local patch set, with its message as a
    /// sealed draft. The review belongs to the patch set's local change and
    /// is posted on its Gerrit change if it has one, else on its synthetic
    /// number; the votes are given on the patch set's commit. Fails with
    /// [`Error::NotLocal`] for a patch set that is not a local patch set of
    /// this server, and with [`Error::NotReviewable`] when the patch set's
    /// change is consolidated or closed.
    pub fn create_pending_review(
        &mut self,
        server: ServerId,
        input: &NewPendingReview,
    ) -> Result<PendingReview, Error> {
        let tx = self.conn.transaction()?;
        let owner: Option<(i64, Option<i64>, String, String)> = match input.patch_set.local_id() {
            Some(ps) => tx
                .query_row(
                    "SELECT c.id, c.gerrit_number, p.sha, c.state
                     FROM local_patch_sets p JOIN local_changes c ON c.id = p.local_change_id
                     WHERE p.server_id = ?1 AND p.id = ?2",
                    params![server.0, ps],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?,
            None => None,
        };
        let Some((local_change, gerrit_number, sha, state)) = owner else {
            return Err(Error::NotLocal);
        };
        if let Some(state @ (LocalChangeState::Consolidated | LocalChangeState::Closed)) =
            LocalChangeState::parse(&state)
        {
            return Err(Error::NotReviewable(state));
        }
        let change = match gerrit_number {
            Some(n) => n,
            None => LocalChangeId(local_change)
                .number()
                .map_or(i64::MAX, number),
        };
        tx.execute(
            "INSERT INTO pending_reviews (server_id, local_change_id, change_number, patch_set,
                 revision_sha, given_on_sha, labels, strategy, state, created_ms, voted_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, 'pending', ?8, ?8)",
            params![
                server.0,
                local_change,
                change,
                input.patch_set.0,
                sha,
                serde_json::to_string(&input.labels)?,
                input.strategy.as_str(),
                millis(input.created)
            ],
        )?;
        let id = PendingReviewId(tx.last_insert_rowid());
        if let Some(old) = input.replaces_draft {
            tx.execute(
                "DELETE FROM drafts
                 WHERE server_id = ?1 AND id = ?2 AND patch_set = ?3 AND path = ?4
                     AND in_reply_to IS NULL AND pending_review_id IS NULL
                     AND reserved_by IS NULL AND held_reason IS NULL",
                params![server.0, old.0, input.patch_set.0, PATCHSET_LEVEL_PATH],
            )?;
        }
        let review = load(&tx, server, id)?.ok_or_else(missing)?;
        write_message(
            &tx,
            server,
            &review,
            &input.message,
            input.unresolved,
            input.created,
        )?;
        let review = load(&tx, server, id)?.ok_or_else(missing)?;
        tx.commit()?;
        Ok(review)
    }

    /// Replaces the votes and message of a review that is still `Pending`
    /// (the reply dialog reopened on it). The strategy and `voted_at` change
    /// only if the labels do. `None` if there is no such review or it is no
    /// longer pending; nothing is written then.
    pub fn update_pending_review(
        &mut self,
        server: ServerId,
        id: PendingReviewId,
        edit: &PendingReviewEdit,
        now: Timestamp,
    ) -> Result<Option<PendingReview>, Error> {
        let tx = self.conn.transaction()?;
        let Some(review) = load(&tx, server, id)? else {
            return Ok(None);
        };
        if review.state != PendingReviewState::Pending {
            return Ok(None);
        }
        if review.labels != edit.labels {
            tx.execute(
                "UPDATE pending_reviews SET labels = ?3, strategy = ?4, voted_at_ms = ?5
                 WHERE server_id = ?1 AND id = ?2",
                params![
                    server.0,
                    id.0,
                    serde_json::to_string(&edit.labels)?,
                    edit.strategy.as_str(),
                    millis(now)
                ],
            )?;
        }
        write_message(&tx, server, &review, &edit.message, edit.unresolved, now)?;
        let review = load(&tx, server, id)?;
        tx.commit()?;
        Ok(review)
    }

    /// Withdraws a `Pending` review (the user changed their mind before it
    /// could be sent). Its inline comments are drafts of their own and stay.
    /// Its message draft is *unsealed*: it stays as an ordinary
    /// patch-set-level draft on the same patch set, so cancelling never
    /// loses text the user wrote; they can delete it like any draft. False
    /// if there is no such review or it is not pending.
    pub fn cancel_pending_review(
        &mut self,
        server: ServerId,
        id: PendingReviewId,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        match load(&tx, server, id)? {
            Some(r) if r.state == PendingReviewState::Pending => {}
            _ => return Ok(false),
        }
        tx.execute(
            "UPDATE drafts SET pending_review_id = NULL, reserved_by = NULL
             WHERE server_id = ?1 AND pending_review_id = ?2",
            params![server.0, id.0],
        )?;
        tx.execute(
            "DELETE FROM pending_reviews WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Discards a `Held` or `Failed` review (the "drop" action of the held
    /// card) together with its message draft: the user decided it is not
    /// to be sent. Its inline drafts stay as they are. False if there is no
    /// such review or it is in another state.
    pub fn drop_pending_review(
        &mut self,
        server: ServerId,
        id: PendingReviewId,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        match load(&tx, server, id)? {
            Some(r)
                if matches!(
                    r.state,
                    PendingReviewState::Held | PendingReviewState::Failed
                ) => {}
            _ => return Ok(false),
        }
        tx.execute(
            "DELETE FROM drafts WHERE server_id = ?1 AND pending_review_id = ?2",
            params![server.0, id.0],
        )?;
        tx.execute(
            "DELETE FROM pending_reviews WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn load_pending_review(
        &self,
        server: ServerId,
        id: PendingReviewId,
    ) -> Result<Option<PendingReview>, Error> {
        load(&self.conn, server, id)
    }

    /// The reviews posted (or to be posted) on a change, in creation order,
    /// which is the order they are sent in. `change` is a local change's
    /// synthetic number or a Gerrit change number (reviews on its local
    /// patch sets, and those consolidation moved onto it).
    pub fn pending_reviews(
        &self,
        server: ServerId,
        change: ChangeNumber,
    ) -> Result<Vec<PendingReview>, Error> {
        query(
            &self.conn,
            "r.server_id = ?1 AND r.change_number = ?2",
            &[&server.0, &number(change)],
        )
    }

    /// The reviews of a local change, wherever they are now, in creation
    /// order.
    pub fn pending_reviews_of_local_change(
        &self,
        server: ServerId,
        change: LocalChangeId,
    ) -> Result<Vec<PendingReview>, Error> {
        query(
            &self.conn,
            "r.server_id = ?1 AND r.local_change_id = ?2",
            &[&server.0, &change.0],
        )
    }

    /// Claims a review for sending: if it is in one of the states `from`
    /// (`Pending` for the automatic send, `Held` for a manual publish,
    /// `Failed` for a retry), sets `Sending` and the send attempt's `nonce`
    /// and returns the review as claimed, read in the same transaction, so
    /// the caller posts exactly that. `None` if another run claimed it
    /// first, it is in another state, or it is gone.
    pub fn mark_pending_review_sending(
        &mut self,
        server: ServerId,
        id: PendingReviewId,
        from: &[PendingReviewState],
        nonce: &str,
    ) -> Result<Option<PendingReview>, Error> {
        let tx = self.conn.transaction()?;
        match load(&tx, server, id)? {
            Some(r) if from.contains(&r.state) && r.state != PendingReviewState::Sending => {}
            _ => return Ok(None),
        }
        tx.execute(
            "UPDATE pending_reviews SET state = 'sending', nonce = ?3, error = NULL,
                 held_reason = NULL
             WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0, nonce],
        )?;
        let review = load(&tx, server, id)?;
        tx.commit()?;
        Ok(review)
    }

    /// Moves a review to `to` if it is in one of the states `from`. `note`
    /// is the error of a `Failed` review or the reason of a `Held` one;
    /// both are cleared for the other states. False if the review is gone
    /// or in another state.
    pub fn set_pending_review_state(
        &mut self,
        server: ServerId,
        id: PendingReviewId,
        from: &[PendingReviewState],
        to: PendingReviewState,
        note: Option<&str>,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let moved = transition(&tx, server, id, None, from, to, note)?;
        tx.commit()?;
        Ok(moved)
    }

    /// Removes one label's vote from a review that is not being sent, when
    /// a newer review sets the same label and supersedes it. Not a new
    /// vote: `voted_at` and the strategy stay. `None` if the review is gone
    /// or `Sending`; a label it does not have changes nothing.
    pub fn remove_review_label(
        &mut self,
        server: ServerId,
        id: PendingReviewId,
        label: &str,
    ) -> Result<Option<PendingReview>, Error> {
        let tx = self.conn.transaction()?;
        let Some(mut review) = load(&tx, server, id)? else {
            return Ok(None);
        };
        if review.state == PendingReviewState::Sending {
            return Ok(None);
        }
        if review.labels.remove(label).is_some() {
            tx.execute(
                "UPDATE pending_reviews SET labels = ?3 WHERE server_id = ?1 AND id = ?2",
                params![server.0, id.0, serde_json::to_string(&review.labels)?],
            )?;
        }
        tx.commit()?;
        Ok(Some(review))
    }
}

#[cfg(test)]
mod tests;
