//! What the user does with the held items of a Gerrit change (the held
//! card): drop one, or publish a selection of them.
//!
//! A publish is a consolidation post like any other, so that it is
//! idempotent and recovered the same way after a crash.
//! [`Store::claim_held_publish`] turns the selection, in one transaction,
//! into a new pending review in state `Sending` on the change's current
//! patch set, whose votes are the selected votes (keeping where, when and
//! under which strategy they were given, so the approval rules judge them
//! as before should the post fail), whose message is the
//! selected review message, and whose inline comments are the selected
//! held comments, rewritten as patch-set-level comments (their text is the
//! caller's, quoting where they were). Everything carries the post's nonce
//! and the claim names its owner, exactly as [`Store::claim_local_post`]
//! leaves a post. The selected votes and message leave their held review,
//! which goes once nothing is left of it. After the post,
//! [`Store::finish_local_post`] removes what was posted; a run that finds
//! the claim later checks the change's messages for the post's tag before
//! posting again.

use std::collections::BTreeMap;

use model::{
    ChangeNumber, LocalChangeId, LocalDraftId, PATCHSET_LEVEL_PATH, PatchSetRef, PendingReviewId,
    PendingReviewState, Side, Timestamp,
};
use rusqlite::{Connection, OptionalExtension, params};

use super::{Claimant, LocalPost, read_post, record_claim};
use crate::{Error, ServerId, Store, local::load_change, number, pending_reviews};

/// A selection of held items to publish onto Gerrit change `gerrit`, all of
/// local change `local_change` (see [`Store::claim_held_publish`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldPublish {
    pub local_change: LocalChangeId,
    pub gerrit: ChangeNumber,
    /// The change's current patch set, which the post goes to.
    pub to: PatchSetRef,
    /// Votes of held or failed reviews: `(review, label)`.
    pub votes: Vec<(PendingReviewId, String)>,
    /// Held or failed reviews whose message is published. The first one
    /// becomes the new review's message; any other is posted as a
    /// patch-set-level comment of its own.
    pub messages: Vec<PendingReviewId>,
    /// Held comments with the text they are posted with, as a
    /// patch-set-level comment.
    pub comments: Vec<(LocalDraftId, String)>,
    /// The new review's `created` (and `voted_at` when it carries no
    /// votes).
    pub at: Timestamp,
    /// The account that publishes it (and whose name the post goes out
    /// under), with its username.
    pub author: model::AccountId,
    pub author_name: Option<String>,
}

/// Whether review `id` is a held or failed review of `change` on Gerrit
/// change `gerrit`; returns it then.
fn held_review(
    conn: &Connection,
    server: ServerId,
    id: PendingReviewId,
    change: Option<LocalChangeId>,
    gerrit: Option<ChangeNumber>,
) -> Result<Option<model::PendingReview>, Error> {
    Ok(pending_reviews::load(conn, server, id)?.filter(|r| {
        matches!(
            r.state,
            PendingReviewState::Held | PendingReviewState::Failed
        ) && change.is_none_or(|c| c == r.local_change)
            && gerrit.is_none_or(|g| g == r.change)
    }))
}

/// Deletes review `id` if nothing is left of it: no vote and no message.
fn drop_if_empty(conn: &Connection, server: ServerId, id: PendingReviewId) -> Result<(), Error> {
    if let Some(r) = pending_reviews::load(conn, server, id)?
        && r.labels.is_empty()
        && r.message_draft.is_none()
    {
        conn.execute(
            "DELETE FROM pending_reviews WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0],
        )?;
    }
    Ok(())
}

/// Whether draft `id` is a held comment of local change `change` on Gerrit
/// change `gerrit`, free to be dropped or published: held, not a review's
/// message, not claimed by a post, not deleted, and on one of the change's
/// versions or reserved for its post.
fn held_comment(
    conn: &Connection,
    server: ServerId,
    id: LocalDraftId,
    change: LocalChangeId,
    gerrit: ChangeNumber,
) -> Result<bool, Error> {
    let numbers = crate::local::patch_set_numbers("?3");
    Ok(conn
        .query_row(
            &format!(
                "SELECT 1 FROM drafts
                 WHERE server_id = ?1 AND id = ?2 AND change_number = ?4
                   AND held_reason IS NOT NULL AND pending_review_id IS NULL
                   AND posting_nonce IS NULL AND state <> 'pending_delete'
                   AND (reserved_by = ?3 OR patch_set IN {numbers})"
            ),
            params![server.0, id.0, change.0, number(gerrit)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The local change a held draft belongs to: the one it is reserved for,
/// or the one whose version it is on.
fn owner_of_draft(
    conn: &Connection,
    server: ServerId,
    id: LocalDraftId,
) -> Result<Option<LocalChangeId>, Error> {
    let found: Option<(Option<i64>, i64)> = conn
        .query_row(
            "SELECT reserved_by, patch_set FROM drafts WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((reserved, patch_set)) = found else {
        return Ok(None);
    };
    if let Some(c) = reserved {
        return Ok(Some(LocalChangeId(c)));
    }
    let Some(ps) = u32::try_from(patch_set)
        .ok()
        .and_then(|n| model::PatchSetNumber(n).local_id())
    else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT local_change_id FROM local_patch_sets WHERE server_id = ?1 AND id = ?2",
            params![server.0, ps],
            |r| r.get(0),
        )
        .optional()?
        .map(LocalChangeId))
}

impl Store {
    /// Drops one vote of a held or failed review (the held card's "drop"),
    /// whether or not it may be published; the review goes once it has
    /// neither votes nor a message left. False if the review is gone, in
    /// another state, or has no vote on `label`.
    pub fn drop_held_vote(
        &mut self,
        server: ServerId,
        review: PendingReviewId,
        label: &str,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let Some(mut r) = held_review(&tx, server, review, None, None)? else {
            return Ok(false);
        };
        if r.labels.remove(label).is_none() {
            return Ok(false);
        }
        r.drop_only.remove(label);
        write_votes(&tx, server, review, &r.labels, &r.drop_only)?;
        drop_if_empty(&tx, server, review)?;
        tx.commit()?;
        Ok(true)
    }

    /// Drops the message of a held or failed review; the review goes if it
    /// has no votes left. False if the review is gone, in another state, or
    /// has no message.
    pub fn drop_held_message(
        &mut self,
        server: ServerId,
        review: PendingReviewId,
    ) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let Some(draft) =
            held_review(&tx, server, review, None, None)?.and_then(|r| r.message_draft)
        else {
            return Ok(false);
        };
        tx.execute(
            "DELETE FROM drafts WHERE server_id = ?1 AND id = ?2",
            params![server.0, draft.0],
        )?;
        drop_if_empty(&tx, server, review)?;
        tx.commit()?;
        Ok(true)
    }

    /// Drops a held comment (it is deleted: it never reached Gerrit). False
    /// if it is gone, not held, a review's message, or claimed by a post.
    pub fn drop_held_draft(
        &mut self,
        server: ServerId,
        draft: LocalDraftId,
    ) -> Result<bool, Error> {
        let n = self.conn.execute(
            "DELETE FROM drafts WHERE server_id = ?1 AND id = ?2 AND held_reason IS NOT NULL
               AND pending_review_id IS NULL AND posting_nonce IS NULL",
            params![server.0, draft.0],
        )?;
        Ok(n > 0)
    }

    /// The local change a held item belongs to: the review's, or the
    /// draft's (see [`Store::claim_held_publish`]). `None` if it is gone.
    pub fn held_draft_owner(
        &self,
        server: ServerId,
        draft: LocalDraftId,
    ) -> Result<Option<LocalChangeId>, Error> {
        owner_of_draft(&self.conn, server, draft)
    }

    /// Claims a publish of held items under `nonce` (see the module
    /// documentation): one transaction, on disk when it returns. Returns
    /// the post as claimed (the new review, `Sending`, and the drafts it
    /// carries), ready for the review post.
    ///
    /// Fails with [`Error::DropOnly`], writing nothing, if a selected vote
    /// may only be dropped, and with [`Error::LocalTarget`] if `to` is a
    /// local patch set. `None`, writing nothing, if nothing is selected, if
    /// the change is gone or belongs to another Gerrit change, or if any
    /// selected item is no longer held there (gone, being sent, a vote the
    /// review no longer has): the caller looks again.
    pub fn claim_held_publish(
        &mut self,
        server: ServerId,
        publish: &HeldPublish,
        nonce: &str,
        by: &Claimant<'_>,
    ) -> Result<Option<LocalPost>, Error> {
        if publish.to.number.is_local() || publish.gerrit.is_local() {
            return Err(Error::LocalTarget);
        }
        self.durably(|s| s.claim_held_in(server, publish, nonce, by))
    }

    fn claim_held_in(
        &mut self,
        server: ServerId,
        p: &HeldPublish,
        nonce: &str,
        by: &Claimant<'_>,
    ) -> Result<Option<LocalPost>, Error> {
        if p.votes.is_empty() && p.messages.is_empty() && p.comments.is_empty() {
            return Ok(None);
        }
        let tx = self.conn.transaction()?;
        let Some(row) = load_change(&tx, server, p.local_change)? else {
            return Ok(None);
        };
        if row.gerrit_number != Some(p.gerrit) {
            return Ok(None);
        }
        let (change, gerrit) = (Some(p.local_change), Some(p.gerrit));
        // Check everything before writing anything.
        let mut votes: BTreeMap<String, i32> = BTreeMap::new();
        // Where the votes were given, when and under which strategy: the
        // new review keeps it, so the approval rules judge it as before
        // (should it fail and be held again).
        let mut origin: Option<(String, i64, String)> = None;
        for (id, label) in &p.votes {
            let Some(r) = held_review(&tx, server, *id, change, gerrit)? else {
                return Ok(None);
            };
            let this = (
                r.given_on_sha.as_str().to_owned(),
                crate::review_cache::millis(r.voted_at),
                r.strategy.as_str().to_owned(),
            );
            if origin.as_ref().is_some_and(|o| *o != this) {
                // Votes given on different commits or at different times
                // do not go in one post.
                return Ok(None);
            }
            origin = Some(this);
            if r.drop_only.contains(label) {
                return Err(Error::DropOnly(label.clone()));
            }
            let Some(value) = r.labels.get(label) else {
                return Ok(None);
            };
            if votes.insert(label.clone(), *value).is_some() {
                // Two votes on one label cannot go in one post.
                return Ok(None);
            }
        }
        let mut messages = Vec::new();
        for id in &p.messages {
            match held_review(&tx, server, *id, change, gerrit)?.and_then(|r| r.message_draft) {
                Some(d) => messages.push(d),
                None => return Ok(None),
            }
        }
        for (id, _) in &p.comments {
            if !held_comment(&tx, server, *id, p.local_change, p.gerrit)? {
                return Ok(None);
            }
        }

        let at = crate::review_cache::millis(p.at);
        let (given_on, voted_at, strategy) = origin.unwrap_or_else(|| {
            (
                p.to.sha.as_str().to_owned(),
                at,
                model::LocalApproval::Exact.as_str().to_owned(),
            )
        });
        tx.execute(
            "INSERT INTO pending_reviews (server_id, local_change_id, change_number, patch_set,
                 revision_sha, given_on_sha, labels, strategy, state, created_ms, voted_at_ms,
                 nonce, author_id, author_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?10, ?6, ?7, 'sending', ?8, ?11, ?9, ?12, ?13)",
            params![
                server.0,
                p.local_change.0,
                number(p.gerrit),
                p.to.number.0,
                p.to.sha.as_str(),
                serde_json::to_string(&votes)?,
                strategy,
                at,
                nonce,
                given_on,
                voted_at,
                i64::try_from(p.author.0).ok(),
                p.author_name
            ],
        )?;
        let review = PendingReviewId(tx.last_insert_rowid());
        for (id, label) in &p.votes {
            if let Some(mut r) = pending_reviews::load(&tx, server, *id)? {
                r.labels.remove(label);
                write_votes(&tx, server, *id, &r.labels, &r.drop_only)?;
            }
        }
        // The messages and comments: onto the current patch set, reserved
        // for the change's post and claimed by it.
        let onto = "change_number = ?3, patch_set = ?4, revision_sha = ?5, reserved_by = ?6,
                    posting_nonce = ?7, held_reason = NULL, state = 'pending_create',
                    failed_op = NULL, error = NULL, retryable = NULL, server_draft_id = NULL";
        for (i, d) in messages.iter().enumerate() {
            tx.execute(
                &format!(
                    "UPDATE drafts SET {onto}, pending_review_id = ?8
                     WHERE server_id = ?1 AND id = ?2"
                ),
                params![
                    server.0,
                    d.0,
                    number(p.gerrit),
                    p.to.number.0,
                    p.to.sha.as_str(),
                    p.local_change.0,
                    nonce,
                    (i == 0).then_some(review.0)
                ],
            )?;
        }
        for (id, text) in &p.comments {
            tx.execute(
                &format!(
                    "UPDATE drafts SET {onto}, path = ?8, side = ?9, parent = NULL, line = NULL,
                         start_line = NULL, start_character = NULL, end_line = NULL,
                         end_character = NULL, in_reply_to = NULL, message = ?10,
                         version = version + 1
                     WHERE server_id = ?1 AND id = ?2"
                ),
                params![
                    server.0,
                    id.0,
                    number(p.gerrit),
                    p.to.number.0,
                    p.to.sha.as_str(),
                    p.local_change.0,
                    nonce,
                    PATCHSET_LEVEL_PATH,
                    Side::Revision.as_wire(),
                    text
                ],
            )?;
        }
        let sources: std::collections::BTreeSet<PendingReviewId> = p
            .votes
            .iter()
            .map(|(id, _)| *id)
            .chain(p.messages.iter().copied())
            .collect();
        for id in sources {
            drop_if_empty(&tx, server, id)?;
        }
        record_claim(&tx, server, p.local_change, nonce, by)?;
        let post = read_post(&tx, server, p.local_change, nonce)?;
        tx.commit()?;
        Ok(post)
    }
}

/// Writes a held review's votes and which of them may only be dropped.
fn write_votes(
    conn: &Connection,
    server: ServerId,
    id: PendingReviewId,
    labels: &BTreeMap<String, i32>,
    drop_only: &std::collections::BTreeSet<String>,
) -> Result<(), Error> {
    let drop_only: std::collections::BTreeSet<&String> = drop_only
        .iter()
        .filter(|l| labels.contains_key(*l))
        .collect();
    conn.execute(
        "UPDATE pending_reviews SET labels = ?3, drop_only = ?4 WHERE server_id = ?1 AND id = ?2",
        params![
            server.0,
            id.0,
            serde_json::to_string(labels)?,
            if drop_only.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&drop_only)?)
            }
        ],
    )?;
    Ok(())
}
