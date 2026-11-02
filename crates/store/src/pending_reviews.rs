//! Pending reviews: votes and a message given on a local patch set, kept
//! until what they were given on is on Gerrit.
//!
//! ```text
//!   create ──▶ Pending ──mark_sending──▶ Sending ──▶ Sent
//!               │  ▲ edit, cancel           │
//!               │  └──── (requeue) ─────────┤──▶ Failed
//!               └──────────▶ Held ◀─────────┘
//! ```
//!
//! Only a `Pending` review can be edited. [`Store::mark_pending_review_sending`]
//! is a compare-and-set from `Pending`, so two consolidation runs cannot
//! both send the same review. Every other transition goes through
//! [`Store::set_pending_review_state`], which names the states it may start
//! from.
//!
//! The review's message is not a column: it is a patch-set-level draft on
//! the same patch set with `pending_review_id` set (sealed), created,
//! edited and removed with the review. Its inline comments are ordinary
//! drafts on the local patch set, not linked to it.

use std::collections::BTreeMap;

use model::{
    ChangeNumber, LocalApproval, LocalChangeId, LocalDraftId, PATCHSET_LEVEL_PATH, PatchSetNumber,
    PatchSetRef, PendingReview, PendingReviewId, PendingReviewState, RevisionId, Side, Timestamp,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{
    Error, ServerId, Store,
    drafts::bad,
    number,
    review_cache::{from_millis, millis},
};

/// What [`Store::create_pending_review`] takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPendingReview {
    pub local_change: Option<LocalChangeId>,
    /// The local change's synthetic number, or the Gerrit change of a
    /// local patch set.
    pub change: ChangeNumber,
    /// A local patch set; its SHA is what the votes are given on.
    pub patch_set: PatchSetRef,
    pub labels: BTreeMap<String, i32>,
    pub strategy: LocalApproval,
    /// The review message; empty or blank means none.
    pub message: String,
    pub created: Timestamp,
}

/// What [`Store::update_pending_review`] replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingReviewEdit {
    pub labels: BTreeMap<String, i32>,
    /// The project's mode at the time of the edit: editing the votes is
    /// voting again.
    pub strategy: LocalApproval,
    /// Empty or blank removes the message.
    pub message: String,
}

const COLUMNS: &str = "r.id, r.local_change_id, r.change_number, r.patch_set, r.given_on_sha, \
     r.labels, r.strategy, r.state, r.error, r.held_reason, r.created_ms, r.nonce, \
     d.message, d.id";

const FROM: &str = "pending_reviews r LEFT JOIN drafts d ON d.pending_review_id = r.id";

fn review_from_row(r: &Row<'_>) -> rusqlite::Result<PendingReview> {
    let labels: String = r.get(5)?;
    let strategy: String = r.get(6)?;
    let state: String = r.get(7)?;
    let created_ms: i64 = r.get(10)?;
    Ok(PendingReview {
        id: PendingReviewId(r.get(0)?),
        local_change: r.get::<_, Option<i64>>(1)?.map(LocalChangeId),
        change: ChangeNumber(u64::try_from(r.get::<_, i64>(2)?).map_err(|_| bad(2, "number"))?),
        patch_set: PatchSetNumber(
            u32::try_from(r.get::<_, i64>(3)?).map_err(|_| bad(3, "patch set"))?,
        ),
        given_on_sha: RevisionId(r.get(4)?),
        labels: serde_json::from_str(&labels).map_err(|_| bad(5, &labels))?,
        // A mode this build does not know is taken as the strictest.
        strategy: LocalApproval::parse(&strategy).unwrap_or(LocalApproval::Exact),
        state: PendingReviewState::parse(&state).ok_or_else(|| bad(7, &state))?,
        error: r.get(8)?,
        held_reason: r.get(9)?,
        created: from_millis(created_ms).map_err(|_| bad(10, &created_ms.to_string()))?,
        nonce: r.get(11)?,
        message: r.get(12)?,
        message_draft: r.get::<_, Option<i64>>(13)?.map(LocalDraftId),
    })
}

fn load(
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

fn state_of(
    conn: &Connection,
    server: ServerId,
    id: PendingReviewId,
) -> Result<Option<PendingReviewState>, Error> {
    let s: Option<String> = conn
        .query_row(
            "SELECT state FROM pending_reviews WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0],
            |r| r.get(0),
        )
        .optional()?;
    s.map(|s| PendingReviewState::parse(&s).ok_or_else(|| Error::Sqlite(bad(0, &s))))
        .transpose()
}

/// Writes a review's state; `note` becomes the error of a `Failed` review
/// and the reason of a `Held` one, and both are cleared otherwise. Returns
/// the rows changed (0 or 1).
pub(crate) fn write_state(
    conn: &Connection,
    server: ServerId,
    id: PendingReviewId,
    state: PendingReviewState,
    note: Option<&str>,
) -> Result<usize, Error> {
    let error = (state == PendingReviewState::Failed)
        .then_some(note)
        .flatten();
    let held = (state == PendingReviewState::Held)
        .then_some(note)
        .flatten();
    Ok(conn.execute(
        "UPDATE pending_reviews SET state = ?3, error = ?4, held_reason = ?5
         WHERE server_id = ?1 AND id = ?2",
        params![server.0, id.0, state.as_str(), error, held],
    )?)
}

/// Creates, updates or removes the review's message draft so that it holds
/// `message` (none when blank).
fn write_message(
    conn: &Connection,
    server: ServerId,
    review: &PendingReview,
    message: &str,
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
                "UPDATE drafts SET message = ?3, updated_ms = ?4, version = version + 1
                 WHERE server_id = ?1 AND id = ?2",
                params![server.0, d.0, message, millis(now)],
            )?;
        }
        (None, true) => {}
        (None, false) => {
            conn.execute(
                "INSERT INTO drafts (server_id, change_number, patch_set, revision_sha, path,
                     side, message, unresolved, updated_ms, state, pending_review_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, 'pending_create', ?9)",
                params![
                    server.0,
                    number(review.change),
                    review.patch_set.0,
                    review.given_on_sha.as_str(),
                    PATCHSET_LEVEL_PATH,
                    Side::Revision.as_wire(),
                    message,
                    millis(now),
                    review.id.0
                ],
            )?;
        }
    }
    Ok(())
}

impl Store {
    /// Saves a review given on a local patch set, with its message as a
    /// sealed draft. Fails with [`Error::NotLocal`] for a patch set that is
    /// not local: a review on a Gerrit patch set is sent right away.
    pub fn create_pending_review(
        &mut self,
        server: ServerId,
        input: &NewPendingReview,
    ) -> Result<PendingReview, Error> {
        if !input.patch_set.number.is_local() {
            return Err(Error::NotLocal);
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO pending_reviews (server_id, local_change_id, change_number, patch_set,
                 given_on_sha, labels, strategy, state, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8)",
            params![
                server.0,
                input.local_change.map(|c| c.0),
                number(input.change),
                input.patch_set.number.0,
                input.patch_set.sha.as_str(),
                serde_json::to_string(&input.labels)?,
                input.strategy.as_str(),
                millis(input.created)
            ],
        )?;
        let id = PendingReviewId(tx.last_insert_rowid());
        let review =
            load(&tx, server, id)?.ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
        write_message(&tx, server, &review, &input.message, input.created)?;
        let review =
            load(&tx, server, id)?.ok_or(Error::Sqlite(rusqlite::Error::QueryReturnedNoRows))?;
        tx.commit()?;
        Ok(review)
    }

    /// Replaces the votes, mode and message of a review that is still
    /// `Pending` (the reply dialog reopened on it). `None` if there is no
    /// such review or it is no longer pending; nothing is written then.
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
        tx.execute(
            "UPDATE pending_reviews SET labels = ?3, strategy = ?4
             WHERE server_id = ?1 AND id = ?2",
            params![
                server.0,
                id.0,
                serde_json::to_string(&edit.labels)?,
                edit.strategy.as_str()
            ],
        )?;
        write_message(&tx, server, &review, &edit.message, now)?;
        let review = load(&tx, server, id)?;
        tx.commit()?;
        Ok(review)
    }

    /// Removes a review that is not being sent. Its inline comments are
    /// drafts of their own and stay. Its message draft is *unsealed*: it
    /// stays as an ordinary patch-set-level draft on the same patch set, so
    /// cancelling never loses text the user wrote; they can delete it like
    /// any draft. False if there is no such review or it is `Sending`.
    pub fn cancel_pending_review(
        &mut self,
        server: ServerId,
        id: PendingReviewId,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        match state_of(&tx, server, id)? {
            None | Some(PendingReviewState::Sending) => return Ok(false),
            Some(_) => {}
        }
        tx.execute(
            "UPDATE drafts SET pending_review_id = NULL
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
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM {FROM} WHERE r.server_id = ?1 AND r.change_number = ?2
             ORDER BY r.id"
        ))?;
        let rows = stmt.query_map(params![server.0, number(change)], review_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The reviews of a local change, wherever they are now, in creation
    /// order.
    pub fn pending_reviews_of_local_change(
        &self,
        server: ServerId,
        change: LocalChangeId,
    ) -> Result<Vec<PendingReview>, Error> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM {FROM} WHERE r.server_id = ?1 AND r.local_change_id = ?2
             ORDER BY r.id"
        ))?;
        let rows = stmt.query_map(params![server.0, change.0], review_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Claims a `Pending` review for sending: sets `Sending` and the send
    /// attempt's `nonce` in one statement, only if the review is still
    /// pending. False if another run claimed it first, it is not pending,
    /// or it is gone.
    pub fn mark_pending_review_sending(
        &mut self,
        server: ServerId,
        id: PendingReviewId,
        nonce: &str,
    ) -> Result<bool, Error> {
        let n = self.conn.execute(
            "UPDATE pending_reviews SET state = 'sending', nonce = ?3, error = NULL,
                 held_reason = NULL
             WHERE server_id = ?1 AND id = ?2 AND state = 'pending'",
            params![server.0, id.0, nonce],
        )?;
        Ok(n > 0)
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
        match state_of(&tx, server, id)? {
            Some(s) if from.contains(&s) => {}
            _ => return Ok(false),
        }
        write_state(&tx, server, id, to, note)?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
