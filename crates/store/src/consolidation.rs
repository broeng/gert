//! Moving a local change's review data onto its Gerrit change, and what is
//! left afterwards.
//!
//! When consolidation finds a local change (or a local patch set) on
//! Gerrit, [`Store::rewrite_local_change`] moves the drafts and pending
//! reviews the run computed a destination for onto the Gerrit change and
//! patch set. Moved drafts are *reserved* (`drafts.reserved_by`): they are
//! posted inline with the review (`ReviewInput.comments`), never uploaded
//! as drafts, so every ordinary draft path ignores them
//! ([`model::Draft::is_ordinary`]). After the post,
//! [`Store::finish_local_post`] removes what was posted. Drafts that could
//! not be moved are *held* (`drafts.held_reason`) on their local patch set,
//! for the user to publish or drop ([`Store::held_items`]).
//!
//! A post is claimed first ([`Store::claim_local_post`]): its drafts and
//! its review get the post's nonce, which is also in the review's tag, so
//! after a crash [`Store::unfinished_local_posts`] names the posts to check
//! against the change's messages before posting again.
//!
//! The rewrite moves exactly what it is told. Anything created after the
//! run's snapshot stays where it is and counts as unsent, so the change is
//! not consolidated and the next run handles it.

use std::collections::HashSet;

use model::{
    ChangeNumber, Draft, LocalChange, LocalChangeId, LocalChangeState, LocalDraftId, Location,
    PatchSetNumber, PatchSetRef, PendingReview, PendingReviewId, PendingReviewState, Side,
};
use rusqlite::params;

use crate::{
    Error, ServerId, Store,
    local::{
        has_unsent, load_change, load_patch_sets, move_to_gerrit_number, query_changes, set_state,
    },
    number, pending_reviews,
};

/// A new place for a moved draft whose lines moved (ported).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftAnchor {
    pub path: String,
    pub side: Side,
    pub parent: Option<u32>,
    pub location: Location,
}

/// Moves listed drafts and reviews from one local patch set onto a Gerrit
/// patch set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PatchSetMove {
    /// A local patch set of the change being rewritten.
    pub from: PatchSetNumber,
    pub to: PatchSetRef,
    /// `to` is the very same commit: the version is mapped to it for
    /// redirects. Otherwise the drafts are ported.
    pub exact: bool,
    /// The drafts on `from` to move, with a new place where their lines
    /// moved. On a move that is not exact, only drafts with an anchor
    /// move. Sealed message drafts are not listed: they move with their
    /// review.
    pub drafts: Vec<(LocalDraftId, Option<DraftAnchor>)>,
    /// The pending reviews on `from` to move (not `Sending` or `Sent`),
    /// each with its message draft.
    pub reviews: Vec<PendingReviewId>,
}

/// A pending review's new state, applied only if the review belongs to the
/// local change and is in one of the states `from`. See
/// [`Store::set_pending_review_state`] for `note`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewStateChange {
    pub review: PendingReviewId,
    pub from: Vec<PendingReviewState>,
    pub state: PendingReviewState,
    pub note: Option<String>,
}

/// One consolidation step for a local change, applied in one transaction
/// by [`Store::rewrite_local_change`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsolidationRewrite {
    pub local_change: LocalChangeId,
    /// The Gerrit change it is (or whose versions these are).
    pub gerrit_number: ChangeNumber,
    pub moves: Vec<PatchSetMove>,
    /// Drafts on the change's local patch sets that did not port, with the
    /// reason: they stay on their local patch set, held for the user.
    pub keep: Vec<(LocalDraftId, String)>,
    pub review_states: Vec<ReviewStateChange>,
    /// The change's new state. `Consolidated` only takes effect when
    /// nothing unsent is left afterwards; otherwise `else_state` does, if
    /// given.
    pub state: Option<LocalChangeState>,
    pub else_state: Option<LocalChangeState>,
}

/// What [`Store::rewrite_local_change`] changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteReport {
    /// Drafts moved and reserved, message drafts included.
    pub drafts_moved: usize,
    pub reviews_moved: usize,
    pub drafts_held: usize,
    pub patch_sets_mapped: usize,
    pub review_states: usize,
    /// Listed drafts and reviews that were not moved or held: not on the
    /// named patch set, already reserved or held, sealed, a non-exact
    /// move without an anchor, or a review being sent or sent.
    pub skipped: usize,
    /// The change's state afterwards.
    pub state: LocalChangeState,
}

/// What [`Store::finish_local_post`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinishReport {
    /// Posted drafts removed.
    pub drafts_removed: usize,
    /// Posted drafts kept because they were edited while the post was in
    /// flight (or are not this change's): they stay for the next post.
    pub drafts_kept: usize,
    pub review_states: usize,
}

/// What [`Store::claim_local_post`] claimed for one consolidation post.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalPost {
    /// The review, now `Sending` with the post's nonce, if one was claimed.
    pub review: Option<PendingReview>,
    /// The drafts the post sends inline: the change's reserved drafts,
    /// except the message drafts of its other reviews, each now carrying
    /// the post's nonce. In creation order.
    pub drafts: Vec<Draft>,
}

/// What the held card of a Gerrit change shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeldItems {
    /// Drafts of local patch sets that could not be moved onto the change.
    pub drafts: Vec<Draft>,
    /// Pending reviews on the change that are held or failed.
    pub reviews: Vec<PendingReview>,
}

impl Store {
    /// Moves a local change's listed drafts and pending reviews onto the
    /// Gerrit change, in one transaction: sets `gerrit_number`, maps exact
    /// versions to their Gerrit patch sets, moves and reserves the drafts
    /// (with their new anchors), moves the reviews with their message
    /// drafts, holds the drafts in `keep`, applies the review states and
    /// then the change's state (see [`ConsolidationRewrite`]). Nothing that
    /// is not listed moves to another patch set. When the change gets its
    /// Gerrit change here, what is left on its synthetic number moves to
    /// the Gerrit change's number on its own local patch sets, as a
    /// reconcile does: from now on those are local patch sets of the
    /// Gerrit change, and their data shows there.
    ///
    /// Returns `None`, writing nothing, if the change is gone, which is
    /// also what a removed server looks like: the caller ends its run.
    /// Fails, writing nothing, with [`Error::LocalTarget`] for a local
    /// `gerrit_number` or move target, and with
    /// [`Error::OtherGerritChange`] if the change already belongs to
    /// another Gerrit change.
    pub fn rewrite_local_change(
        &mut self,
        server: ServerId,
        rewrite: &ConsolidationRewrite,
    ) -> Result<Option<RewriteReport>, Error> {
        if rewrite.gerrit_number.is_local() || rewrite.moves.iter().any(|m| m.to.number.is_local())
        {
            return Err(Error::LocalTarget);
        }
        let tx = self.conn.transaction()?;
        let Some(change) = load_change(&tx, server, rewrite.local_change)? else {
            return Ok(None);
        };
        if let Some(other) = change.gerrit_number
            && other != rewrite.gerrit_number
        {
            return Err(Error::OtherGerritChange(other));
        }
        let gerrit = number(rewrite.gerrit_number);
        tx.execute(
            "UPDATE local_changes SET gerrit_number = ?3 WHERE server_id = ?1 AND id = ?2",
            params![server.0, change.id.0, gerrit],
        )?;
        if change.gerrit_number.is_none() {
            move_to_gerrit_number(&tx, server, change.id, rewrite.gerrit_number)?;
        }
        let versions: HashSet<PatchSetNumber> = load_patch_sets(&tx, server, change.id)?
            .into_iter()
            .map(|p| p.number)
            .collect();
        let mut report = RewriteReport {
            drafts_moved: 0,
            reviews_moved: 0,
            drafts_held: 0,
            patch_sets_mapped: 0,
            review_states: 0,
            skipped: 0,
            state: change.state,
        };
        for m in &rewrite.moves {
            if !versions.contains(&m.from) {
                report.skipped += m.drafts.len() + m.reviews.len();
                continue;
            }
            if m.exact
                && let Some(id) = m.from.local_id()
            {
                report.patch_sets_mapped += tx.execute(
                    "UPDATE local_patch_sets SET gerrit_patch_set = ?3
                     WHERE server_id = ?1 AND id = ?2 AND gerrit_patch_set IS NOT ?3",
                    params![server.0, id, m.to.number.0],
                )?;
            }
            for (id, anchor) in &m.drafts {
                let movable = self_movable(&tx, server, *id, m.from)?;
                if !movable || (!m.exact && anchor.is_none()) {
                    report.skipped += 1;
                    continue;
                }
                move_draft(&tx, server, *id, gerrit, &m.to, change.id)?;
                if let Some(a) = anchor {
                    let range = a.location.range();
                    tx.execute(
                        "UPDATE drafts SET path = ?3, side = ?4, parent = ?5, line = ?6,
                             start_line = ?7, start_character = ?8, end_line = ?9,
                             end_character = ?10
                         WHERE server_id = ?1 AND id = ?2",
                        params![
                            server.0,
                            id.0,
                            a.path,
                            a.side.as_wire(),
                            a.parent,
                            a.location.line(),
                            range.map(|r| r.start_line),
                            range.map(|r| r.start_character),
                            range.map(|r| r.end_line),
                            range.map(|r| r.end_character)
                        ],
                    )?;
                }
                report.drafts_moved += 1;
            }
            for id in &m.reviews {
                let n = tx.execute(
                    "UPDATE pending_reviews SET change_number = ?4, patch_set = ?5,
                         revision_sha = ?6
                     WHERE server_id = ?1 AND id = ?2 AND local_change_id = ?3
                       AND patch_set = ?7 AND state IN ('pending', 'held', 'failed')",
                    params![
                        server.0,
                        id.0,
                        change.id.0,
                        gerrit,
                        m.to.number.0,
                        m.to.sha.as_str(),
                        m.from.0
                    ],
                )?;
                if n == 0 {
                    report.skipped += 1;
                    continue;
                }
                report.reviews_moved += 1;
                report.drafts_moved += tx.execute(
                    "UPDATE drafts SET change_number = ?3, patch_set = ?4, revision_sha = ?5,
                         reserved_by = ?6
                     WHERE server_id = ?1 AND pending_review_id = ?2",
                    params![
                        server.0,
                        id.0,
                        gerrit,
                        m.to.number.0,
                        m.to.sha.as_str(),
                        change.id.0
                    ],
                )?;
            }
        }
        for (id, reason) in &rewrite.keep {
            // Held drafts keep their local patch set number, which with the
            // reason keeps them out of every ordinary path; the Gerrit
            // number makes them findable for the held card.
            let held = draft_on(&tx, server, *id)?
                .filter(|(ps, sealed, reserved)| versions.contains(ps) && !*sealed && !*reserved);
            if held.is_none() {
                report.skipped += 1;
                continue;
            }
            report.drafts_held += tx.execute(
                "UPDATE drafts SET change_number = ?3, held_reason = ?4
                 WHERE server_id = ?1 AND id = ?2",
                params![server.0, id.0, gerrit, reason],
            )?;
        }
        for s in &rewrite.review_states {
            if pending_reviews::transition(
                &tx,
                server,
                s.review,
                Some(change.id),
                &s.from,
                s.state,
                s.note.as_deref(),
            )? {
                report.review_states += 1;
            }
        }
        report.state = apply_state(&tx, server, &change, rewrite.state, rewrite.else_state)?;
        tx.commit()?;
        Ok(Some(report))
    }

    /// The drafts reserved for a local change's consolidation post, in
    /// creation order: the drafts the rewrite moved onto the Gerrit change
    /// and the message drafts of the reviews it moved. The messages of
    /// reviews that were not moved (held, or still on a local patch set)
    /// are not among them: they must not be posted.
    pub fn consolidation_drafts(
        &self,
        server: ServerId,
        change: LocalChangeId,
    ) -> Result<Vec<Draft>, Error> {
        crate::drafts::query(
            &self.conn,
            "server_id = ?1 AND reserved_by = ?2",
            &[&server.0, &change.0],
        )
    }

    /// Claims a consolidation post of local change `change` under `nonce`,
    /// in one transaction: marks the change's reserved drafts (except the
    /// message drafts of other reviews) with the nonce, and, if `review` is
    /// given and in one of the states `from`, claims it as
    /// [`Store::mark_pending_review_sending`] does, with the same nonce. A
    /// crash after the post leaves the nonce on both, for
    /// [`Store::unfinished_local_posts`]; [`Store::finish_local_post`] or
    /// [`Store::release_local_post`] clear it.
    ///
    /// Claiming again with the same nonce returns the same post (a retry
    /// after a crash). `None`, writing nothing, if the change is gone, if
    /// a draft or the review is claimed under another nonce, or if the
    /// review is not the change's or in none of the states `from`.
    pub fn claim_local_post(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        review: Option<(PendingReviewId, &[PendingReviewState])>,
        nonce: &str,
    ) -> Result<Option<LocalPost>, Error> {
        let tx = self.conn.transaction()?;
        if load_change(&tx, server, change)?.is_none() {
            return Ok(None);
        }
        // The drafts of this post: reserved for the change, except the
        // messages of its other reviews.
        let mine = "server_id = ?1 AND reserved_by = ?2
                    AND (pending_review_id IS NULL OR pending_review_id IS ?3)";
        let review_id = review.map(|(id, _)| id.0);
        let taken: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM drafts WHERE {mine}
                     AND posting_nonce IS NOT NULL AND posting_nonce <> ?4)"
            ),
            params![server.0, change.0, review_id, nonce],
            |r| r.get(0),
        )?;
        if taken {
            return Ok(None);
        }
        let claimed = match review {
            None => None,
            Some((id, from)) => {
                let Some(r) = pending_reviews::load(&tx, server, id)? else {
                    return Ok(None);
                };
                let again =
                    r.state == PendingReviewState::Sending && r.nonce.as_deref() == Some(nonce);
                let claimable = from.contains(&r.state) && r.state != PendingReviewState::Sending;
                if r.local_change != change || !(again || claimable) {
                    return Ok(None);
                }
                tx.execute(
                    "UPDATE pending_reviews SET state = 'sending', nonce = ?3, error = NULL,
                         held_reason = NULL
                     WHERE server_id = ?1 AND id = ?2",
                    params![server.0, id.0, nonce],
                )?;
                pending_reviews::load(&tx, server, id)?
            }
        };
        tx.execute(
            &format!("UPDATE drafts SET posting_nonce = ?4 WHERE {mine}"),
            params![server.0, change.0, review_id, nonce],
        )?;
        let drafts = crate::drafts::query(&tx, mine, &[&server.0, &change.0, &review_id])?;
        tx.commit()?;
        Ok(Some(LocalPost {
            review: claimed,
            drafts,
        }))
    }

    /// The post of local change `change` claimed under `nonce`, as it is
    /// now, without changing anything: the review being sent under it, if
    /// any, and the drafts carrying it, in creation order. After a crash
    /// this is exactly what the claim covered, so the caller can check
    /// whether it reached the server, and finish or post it again. `None`
    /// if nothing carries the nonce (or the change is gone).
    pub fn local_post(
        &self,
        server: ServerId,
        change: LocalChangeId,
        nonce: &str,
    ) -> Result<Option<LocalPost>, Error> {
        if load_change(&self.conn, server, change)?.is_none() {
            return Ok(None);
        }
        let review = pending_reviews::query(
            &self.conn,
            "r.server_id = ?1 AND r.local_change_id = ?2 AND r.state = 'sending'
             AND r.nonce = ?3",
            &[&server.0, &change.0, &nonce],
        )?
        .into_iter()
        .next();
        let drafts = crate::drafts::query(
            &self.conn,
            "server_id = ?1 AND reserved_by = ?2 AND posting_nonce = ?3",
            &[&server.0, &change.0, &nonce],
        )?;
        if review.is_none() && drafts.is_empty() {
            return Ok(None);
        }
        Ok(Some(LocalPost { review, drafts }))
    }

    /// Gives up a claimed post that was not sent (the server refused it, or
    /// a check after a crash found it was not posted): clears `nonce` from
    /// the change's drafts so a later post can claim them. The review's
    /// state is the caller's (`set_pending_review_state`). Returns how many
    /// drafts were released.
    pub fn release_local_post(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        nonce: &str,
    ) -> Result<usize, Error> {
        Ok(self.conn.execute(
            "UPDATE drafts SET posting_nonce = NULL
             WHERE server_id = ?1 AND reserved_by = ?2 AND posting_nonce = ?3",
            params![server.0, change.0, nonce],
        )?)
    }

    /// The consolidation posts that were claimed and not finished or
    /// released: `(local change, nonce)` from drafts carrying a posting
    /// nonce and from reviews being sent. After a crash, the caller checks
    /// the Gerrit change's messages for the post's tag before posting
    /// again. Sorted, without duplicates.
    pub fn unfinished_local_posts(
        &self,
        server: ServerId,
    ) -> Result<Vec<(LocalChangeId, String)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT reserved_by, posting_nonce FROM drafts
             WHERE server_id = ?1 AND reserved_by IS NOT NULL AND posting_nonce IS NOT NULL
             UNION
             SELECT local_change_id, nonce FROM pending_reviews
             WHERE server_id = ?1 AND state = 'sending' AND nonce IS NOT NULL
             ORDER BY 1, 2",
        )?;
        let rows = stmt.query_map([server.0], |r| {
            Ok((LocalChangeId(r.get(0)?), r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records a successful consolidation post, in one transaction: removes
    /// the posted drafts (`(id, version)` as they were sent) whose version
    /// is unchanged, and applies the review states. A draft edited while
    /// the post was in flight stays reserved, so the next post sends it;
    /// the post's claim on the change's drafts is cleared (see
    /// [`Store::claim_local_post`]).
    /// Only the change's own drafts (reserved for it, its reviews' message
    /// drafts, or held on its versions) are removed. `None`, writing
    /// nothing, if the change is gone.
    pub fn finish_local_post(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        posted: &[(LocalDraftId, u32)],
        review_states: &[ReviewStateChange],
    ) -> Result<Option<FinishReport>, Error> {
        let tx = self.conn.transaction()?;
        if load_change(&tx, server, change)?.is_none() {
            return Ok(None);
        }
        let numbers = crate::local::patch_set_numbers("?2");
        let mut report = FinishReport {
            drafts_removed: 0,
            drafts_kept: 0,
            review_states: 0,
        };
        for (id, version) in posted {
            let n = tx.execute(
                &format!(
                    "DELETE FROM drafts WHERE server_id = ?1 AND id = ?3 AND version = ?4
                       AND (reserved_by = ?2
                            OR pending_review_id IN (SELECT id FROM pending_reviews
                                WHERE server_id = ?1 AND local_change_id = ?2)
                            OR (held_reason IS NOT NULL AND patch_set IN {numbers}))"
                ),
                params![server.0, change.0, id.0, version],
            )?;
            if n > 0 {
                report.drafts_removed += 1;
            } else {
                report.drafts_kept += 1;
            }
        }
        // What stays (edited while the post was in flight) is free for the
        // next post.
        tx.execute(
            "UPDATE drafts SET posting_nonce = NULL
             WHERE server_id = ?1 AND reserved_by = ?2 AND posting_nonce IS NOT NULL",
            params![server.0, change.0],
        )?;
        for s in review_states {
            if pending_reviews::transition(
                &tx,
                server,
                s.review,
                Some(change),
                &s.from,
                s.state,
                s.note.as_deref(),
            )? {
                report.review_states += 1;
            }
        }
        tx.commit()?;
        Ok(Some(report))
    }

    /// What is held on Gerrit change `gerrit` from local reviews: drafts
    /// that could not be moved onto it, and pending reviews on it that are
    /// held or failed, each in creation order.
    pub fn held_items(&self, server: ServerId, gerrit: ChangeNumber) -> Result<HeldItems, Error> {
        Ok(HeldItems {
            drafts: crate::drafts::query(
                &self.conn,
                "server_id = ?1 AND change_number = ?2 AND held_reason IS NOT NULL",
                &[&server.0, &number(gerrit)],
            )?,
            reviews: pending_reviews::query(
                &self.conn,
                "r.server_id = ?1 AND r.change_number = ?2 AND r.state IN ('held', 'failed')",
                &[&server.0, &number(gerrit)],
            )?,
        })
    }

    /// The local changes a consolidation run looks up: those checking,
    /// local or vanished, and any other with a Gerrit change and unsent
    /// data (a closed one with held items, or one whose post is not done).
    /// The most recently updated first.
    pub fn local_changes_to_consolidate(
        &self,
        server: ServerId,
    ) -> Result<Vec<LocalChange>, Error> {
        let rows = query_changes(
            &self.conn,
            "server_id = ?1 AND (state IN ('checking', 'local', 'vanished')
                                 OR gerrit_number IS NOT NULL)",
            [server.0],
        )?;
        let mut out = Vec::new();
        for c in rows {
            let lookup = matches!(
                c.state,
                LocalChangeState::Checking | LocalChangeState::Local | LocalChangeState::Vanished
            );
            if lookup || has_unsent(&self.conn, server, c.id)? {
                out.push(c);
            }
        }
        Ok(out)
    }
}

/// `(patch set, sealed, reserved or held)` of a draft of `server`.
fn draft_on(
    conn: &rusqlite::Connection,
    server: ServerId,
    id: LocalDraftId,
) -> Result<Option<(PatchSetNumber, bool, bool)>, Error> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT patch_set, pending_review_id IS NOT NULL,
                    reserved_by IS NOT NULL OR held_reason IS NOT NULL
             FROM drafts WHERE server_id = ?1 AND id = ?2",
            params![server.0, id.0],
            |r| {
                Ok((
                    PatchSetNumber(u32::try_from(r.get::<_, i64>(0)?).unwrap_or(0)),
                    r.get(1)?,
                    r.get(2)?,
                ))
            },
        )
        .optional()?)
}

/// Whether draft `id` sits on `from` and may be moved on its own: not a
/// sealed message, not already reserved or held.
fn self_movable(
    conn: &rusqlite::Connection,
    server: ServerId,
    id: LocalDraftId,
    from: PatchSetNumber,
) -> Result<bool, Error> {
    Ok(draft_on(conn, server, id)?
        .is_some_and(|(ps, sealed, taken)| ps == from && !sealed && !taken))
}

fn move_draft(
    conn: &rusqlite::Connection,
    server: ServerId,
    id: LocalDraftId,
    gerrit: i64,
    to: &PatchSetRef,
    owner: LocalChangeId,
) -> Result<(), Error> {
    conn.execute(
        "UPDATE drafts SET change_number = ?3, patch_set = ?4, revision_sha = ?5,
             reserved_by = ?6
         WHERE server_id = ?1 AND id = ?2",
        params![
            server.0,
            id.0,
            gerrit,
            to.number.0,
            to.sha.as_str(),
            owner.0
        ],
    )?;
    Ok(())
}

/// Applies `state` (falling back to `else_state` when `Consolidated` is
/// refused); returns the state afterwards.
fn apply_state(
    conn: &rusqlite::Connection,
    server: ServerId,
    change: &LocalChange,
    state: Option<LocalChangeState>,
    else_state: Option<LocalChangeState>,
) -> Result<LocalChangeState, Error> {
    let Some(state) = state else {
        return Ok(change.state);
    };
    let now = set_state(conn, server, change.id, state)?.unwrap_or(change.state);
    if now != state
        && let Some(fallback) = else_state
    {
        return Ok(set_state(conn, server, change.id, fallback)?.unwrap_or(now));
    }
    Ok(now)
}

#[cfg(test)]
mod tests;
