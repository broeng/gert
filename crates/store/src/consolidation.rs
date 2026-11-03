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
//! against the change's messages before posting again. The claim also
//! names the run that made it and when (`local_post_claims`): a later run
//! recovers only the posts of runs that are gone and that are too old to
//! still be in flight ([`Store::recoverable_local_posts`],
//! [`Store::adopt_local_post`]). One run per server at a time holds the
//! consolidation lease ([`Store::take_consolidation_lease`]), also across
//! processes sharing the database.
//!
//! The rewrite moves exactly what it is told. Anything created after the
//! run's snapshot stays where it is and counts as unsent, so the change is
//! not consolidated and the next run handles it.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use model::{
    ChangeNumber, Draft, LocalChange, LocalChangeId, LocalChangeState, LocalDraftId, Location,
    PatchSetNumber, PatchSetRef, PendingReview, PendingReviewId, PendingReviewState, Side,
    Timestamp,
};
use rusqlite::params;

use crate::{
    Error, ServerId, Store,
    local::{
        has_unsent, load_change, load_patch_sets, move_to_gerrit_number, query_changes, set_state,
    },
    number, pending_reviews,
    review_cache::millis,
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
    /// The votes the review keeps (those that did not reach Gerrit);
    /// `None` keeps them as they are.
    pub labels: Option<BTreeMap<String, i32>>,
    /// For a `Held` review: the labels whose votes may only be dropped
    /// ([`PendingReview::drop_only`]). Ignored in other states.
    pub drop_only: BTreeSet<String>,
}

impl ReviewStateChange {
    /// `review` from one of `from` to `state` with `note`, votes as they
    /// are.
    pub fn new(
        review: PendingReviewId,
        from: &[PendingReviewState],
        state: PendingReviewState,
        note: Option<String>,
    ) -> Self {
        Self {
            review,
            from: from.to_vec(),
            state,
            note,
            labels: None,
            drop_only: BTreeSet::new(),
        }
    }
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

/// Who claims a consolidation post, and when: the run's owner token and
/// the time of the claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claimant<'a> {
    pub owner: &'a str,
    pub at: Timestamp,
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
            if pending_reviews::apply_change(&tx, server, Some(change.id), s)? {
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
    /// Only drafts on `patch_set`, the revision the post goes to, are
    /// claimed (Gerrit posts a review's comments on its revision); a
    /// review must be on it too. Claiming again with the same nonce returns
    /// the same post (a retry after a crash). `None`, writing nothing, if
    /// the change is gone, if a draft or the review is claimed under
    /// another nonce, or if the review is not the change's, not on
    /// `patch_set` or in none of the states `from`. The claim records `by`
    /// (see [`Store::recoverable_local_posts`]) and is on disk when this
    /// returns, also after a power cut.
    pub fn claim_local_post(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        review: Option<(PendingReviewId, &[PendingReviewState])>,
        patch_set: PatchSetNumber,
        nonce: &str,
        by: &Claimant<'_>,
    ) -> Result<Option<LocalPost>, Error> {
        self.durably(|s| s.claim_in(server, change, review, patch_set, nonce, by))
    }

    fn claim_in(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        review: Option<(PendingReviewId, &[PendingReviewState])>,
        patch_set: PatchSetNumber,
        nonce: &str,
        by: &Claimant<'_>,
    ) -> Result<Option<LocalPost>, Error> {
        let tx = self.conn.transaction()?;
        if load_change(&tx, server, change)?.is_none() {
            return Ok(None);
        }
        // The drafts of this post: reserved for the change on the revision
        // posted to, not held, refused or deleted, except the messages of
        // its other reviews.
        let mine = "server_id = ?1 AND reserved_by = ?2 AND held_reason IS NULL
                    AND patch_set = ?5 AND state NOT IN ('failed', 'pending_delete')
                    AND (pending_review_id IS NULL OR pending_review_id IS ?3)";
        let review_id = review.map(|(id, _)| id.0);
        let taken: bool = tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM drafts WHERE {mine}
                     AND posting_nonce IS NOT NULL AND posting_nonce <> ?4)"
            ),
            params![server.0, change.0, review_id, nonce, patch_set.0],
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
                if r.local_change != change || r.patch_set != patch_set || !(again || claimable) {
                    return Ok(None);
                }
                tx.execute(
                    "UPDATE pending_reviews SET state = 'sending', nonce = ?3, error = NULL,
                         held_reason = NULL, drop_only = NULL
                     WHERE server_id = ?1 AND id = ?2",
                    params![server.0, id.0, nonce],
                )?;
                pending_reviews::load(&tx, server, id)?
            }
        };
        tx.execute(
            &format!("UPDATE drafts SET posting_nonce = ?4 WHERE {mine}"),
            params![server.0, change.0, review_id, nonce, patch_set.0],
        )?;
        let drafts = crate::drafts::query(
            &tx,
            "server_id = ?1 AND reserved_by = ?2 AND posting_nonce = ?3",
            &[&server.0, &change.0, &nonce],
        )?;
        record_claim(&tx, server, change, nonce, by)?;
        tx.commit()?;
        Ok(Some(LocalPost {
            review: claimed,
            drafts,
        }))
    }

    /// Takes over the post claimed under `nonce` for recovery: only if no
    /// claim records it, or its claim is `by.owner`'s, or older than
    /// `stale_before` (its run is gone and its request cannot still be in
    /// flight). Records `by` as the claimant and returns the post as
    /// claimed ([`Store::local_post`]), in one transaction. `None`,
    /// writing nothing, otherwise or if nothing carries the nonce.
    pub fn adopt_local_post(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        nonce: &str,
        by: &Claimant<'_>,
        stale_before: Timestamp,
    ) -> Result<Option<LocalPost>, Error> {
        self.durably(|s| s.adopt_in(server, change, nonce, by, stale_before))
    }

    fn adopt_in(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        nonce: &str,
        by: &Claimant<'_>,
        stale_before: Timestamp,
    ) -> Result<Option<LocalPost>, Error> {
        let tx = self.conn.transaction()?;
        let live: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM local_post_claims
                 WHERE server_id = ?1 AND nonce = ?2 AND owner <> ?3 AND claimed_ms >= ?4)",
            params![server.0, nonce, by.owner, millis(stale_before)],
            |r| r.get(0),
        )?;
        if live {
            return Ok(None);
        }
        let Some(post) = read_post(&tx, server, change, nonce)? else {
            return Ok(None);
        };
        record_claim(&tx, server, change, nonce, by)?;
        tx.commit()?;
        Ok(Some(post))
    }

    /// Runs `f` with `synchronous = FULL`, so that its commit is on disk
    /// before the caller goes on: a claim must survive a power cut, or the
    /// post it was made for could be sent a second time. The database
    /// otherwise runs with `NORMAL`, which may lose the last commits.
    fn durably<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        let before: i64 = self
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        self.conn.execute_batch("PRAGMA synchronous = FULL")?;
        let out = f(self);
        let restored = self
            .conn
            .execute_batch(&format!("PRAGMA synchronous = {before}"));
        let out = out?;
        restored?;
        Ok(out)
    }

    /// The unfinished posts ([`Store::unfinished_local_posts`]) that a run
    /// of `owner` may recover: those no claim records (claimed before
    /// claims named their run) and those claimed by another run before
    /// `stale_before`. A younger claim of another run may still be in
    /// flight. Sorted, without duplicates.
    pub fn recoverable_local_posts(
        &self,
        server: ServerId,
        owner: &str,
        stale_before: Timestamp,
    ) -> Result<Vec<(LocalChangeId, String)>, Error> {
        let mut out = Vec::new();
        for (change, nonce) in self.unfinished_local_posts(server)? {
            let live: bool = self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM local_post_claims
                     WHERE server_id = ?1 AND nonce = ?2
                       AND (owner = ?3 OR claimed_ms >= ?4))",
                params![server.0, nonce, owner, millis(stale_before)],
                |r| r.get(0),
            )?;
            if !live {
                out.push((change, nonce));
            }
        }
        Ok(out)
    }

    /// Takes the server's consolidation lease for `owner` until `until`, if
    /// no other owner holds it past `now`; renews it if `owner` holds it.
    /// One statement, so two processes on one database cannot both get it.
    /// Returns whether `owner` holds it now.
    pub fn take_consolidation_lease(
        &mut self,
        server: ServerId,
        owner: &str,
        now: Timestamp,
        until: Timestamp,
    ) -> Result<bool, Error> {
        let n = self.conn.execute(
            "INSERT INTO consolidation_leases (server_id, owner, expires_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (server_id) DO UPDATE
                 SET owner = excluded.owner, expires_ms = excluded.expires_ms
                 WHERE consolidation_leases.owner = excluded.owner
                    OR consolidation_leases.expires_ms <= ?4",
            params![server.0, owner, millis(until), millis(now)],
        )?;
        Ok(n > 0)
    }

    /// Gives up `owner`'s consolidation lease; another owner's stays.
    pub fn end_consolidation_lease(&mut self, server: ServerId, owner: &str) -> Result<(), Error> {
        self.conn.execute(
            "DELETE FROM consolidation_leases WHERE server_id = ?1 AND owner = ?2",
            params![server.0, owner],
        )?;
        Ok(())
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
        read_post(&self.conn, server, change, nonce)
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
        let tx = self.conn.transaction()?;
        let n = release_drafts(&tx, server, change, nonce)?;
        tx.commit()?;
        Ok(n)
    }

    /// Holds everything unsent of a local change whose Gerrit change
    /// `gerrit` is merged or abandoned, in one transaction: sets the Gerrit
    /// change (moving what is on the synthetic number there, as
    /// [`Store::rewrite_local_change`] does) and the state `Closed`, makes
    /// its pending reviews `Held` and gives its drafts (on its local patch
    /// sets, or reserved for its post) `reason` as their held reason. A
    /// post in flight (claimed drafts, a review being sent) is left to its
    /// run. Nothing of it is sent automatically any more, also if the
    /// change is restored: the user publishes or drops it from the held
    /// card. `None` if the change is gone; fails with
    /// [`Error::OtherGerritChange`] or [`Error::LocalTarget`] as the
    /// rewrite does. Returns how many reviews and drafts it held.
    pub fn hold_closed_local_change(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        gerrit: ChangeNumber,
        reason: &str,
    ) -> Result<Option<(usize, usize)>, Error> {
        if gerrit.is_local() {
            return Err(Error::LocalTarget);
        }
        let tx = self.conn.transaction()?;
        let Some(row) = load_change(&tx, server, change)? else {
            return Ok(None);
        };
        if let Some(other) = row.gerrit_number
            && other != gerrit
        {
            return Err(Error::OtherGerritChange(other));
        }
        tx.execute(
            "UPDATE local_changes SET gerrit_number = ?3 WHERE server_id = ?1 AND id = ?2",
            params![server.0, change.0, number(gerrit)],
        )?;
        // Also what was written on the synthetic number since: it is held
        // on the Gerrit change, where the held card looks.
        move_to_gerrit_number(&tx, server, change, gerrit)?;
        set_state(&tx, server, change, LocalChangeState::Closed)?;
        let reviews = tx.execute(
            "UPDATE pending_reviews SET state = 'held', held_reason = ?3, error = NULL,
                 drop_only = NULL
             WHERE server_id = ?1 AND local_change_id = ?2 AND state = 'pending'",
            params![server.0, change.0, reason],
        )?;
        let numbers = crate::local::patch_set_numbers("?2");
        let drafts = tx.execute(
            &format!(
                "UPDATE drafts SET held_reason = ?3
                 WHERE server_id = ?1 AND held_reason IS NULL AND pending_review_id IS NULL
                   AND posting_nonce IS NULL AND state <> 'pending_delete'
                   AND (reserved_by = ?2 OR patch_set IN {numbers})"
            ),
            params![server.0, change.0, reason],
        )?;
        tx.commit()?;
        Ok(Some((reviews, drafts)))
    }

    /// Records that consolidation asked Gerrit about these local changes
    /// at `at`.
    pub fn mark_local_changes_checked(
        &mut self,
        server: ServerId,
        changes: &[LocalChangeId],
        at: Timestamp,
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        for c in changes {
            tx.execute(
                "UPDATE local_changes SET checked_ms = ?3 WHERE server_id = ?1 AND id = ?2",
                params![server.0, c.0, millis(at)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// When consolidation last asked Gerrit about a local change; `None`
    /// if never (or the change is gone).
    pub fn local_change_checked_at(
        &self,
        server: ServerId,
        change: LocalChangeId,
    ) -> Result<Option<Timestamp>, Error> {
        use rusqlite::OptionalExtension;
        let ms: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT checked_ms FROM local_changes WHERE server_id = ?1 AND id = ?2",
                params![server.0, change.0],
                |r| r.get(0),
            )
            .optional()?;
        ms.flatten()
            .map(crate::review_cache::from_millis)
            .transpose()
    }

    /// Gives up a claimed post that did not reach the server, in one
    /// transaction: applies `review` (the review back to `Pending`, or
    /// `Failed` with the server's refusal), holds the post's comments with
    /// `hold_comments` as their reason if given (the server refused them:
    /// they wait for the user and block nothing), and clears the claim.
    /// `None`, writing nothing, if the change is gone; else how many drafts
    /// were released.
    pub fn abandon_local_post(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        nonce: &str,
        review: Option<&ReviewStateChange>,
        hold_comments: Option<&str>,
    ) -> Result<Option<usize>, Error> {
        let tx = self.conn.transaction()?;
        if load_change(&tx, server, change)?.is_none() {
            return Ok(None);
        }
        if let Some(reason) = hold_comments {
            tx.execute(
                "UPDATE drafts SET held_reason = ?4
                 WHERE server_id = ?1 AND reserved_by = ?2 AND posting_nonce = ?3
                   AND pending_review_id IS NULL",
                params![server.0, change.0, nonce, reason],
            )?;
        }
        let released = release_drafts(&tx, server, change, nonce)?;
        if let Some(r) = review {
            pending_reviews::apply_change(&tx, server, Some(change), r)?;
        }
        tx.commit()?;
        Ok(Some(released))
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

    /// Records a successful consolidation post claimed under `nonce`, in
    /// one transaction: removes the posted drafts (`(id, version)` as they
    /// were sent) and applies the review states. A claimed draft cannot be
    /// edited or deleted ([`Error::DraftBeingSent`]), so the version check
    /// that keeps a draft whose version changed is only a safeguard. The post's claim is cleared (see
    /// [`Store::claim_local_post`]), so its drafts that were not posted (on
    /// another revision) are free for the next post.
    /// Only the change's own drafts (reserved for it, its reviews' message
    /// drafts, or held on its versions) are removed. `None`, writing
    /// nothing, if the change is gone.
    pub fn finish_local_post(
        &mut self,
        server: ServerId,
        change: LocalChangeId,
        nonce: &str,
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
        // What stays is free for the next post.
        release_drafts(&tx, server, change, nonce)?;
        for s in review_states {
            if pending_reviews::apply_change(&tx, server, Some(change), s)? {
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

/// The post claimed under `nonce`: the review being sent under it and the
/// drafts carrying it. `None` if the change is gone or nothing carries it.
fn read_post(
    conn: &rusqlite::Connection,
    server: ServerId,
    change: LocalChangeId,
    nonce: &str,
) -> Result<Option<LocalPost>, Error> {
    if load_change(conn, server, change)?.is_none() {
        return Ok(None);
    }
    let review = pending_reviews::query(
        conn,
        "r.server_id = ?1 AND r.local_change_id = ?2 AND r.state = 'sending'
         AND r.nonce = ?3",
        &[&server.0, &change.0, &nonce],
    )?
    .into_iter()
    .next();
    let drafts = crate::drafts::query(
        conn,
        "server_id = ?1 AND reserved_by = ?2 AND posting_nonce = ?3",
        &[&server.0, &change.0, &nonce],
    )?;
    if review.is_none() && drafts.is_empty() {
        return Ok(None);
    }
    Ok(Some(LocalPost { review, drafts }))
}

/// Records (or moves) the claim of the post under `nonce` to `by`.
fn record_claim(
    conn: &rusqlite::Connection,
    server: ServerId,
    change: LocalChangeId,
    nonce: &str,
    by: &Claimant<'_>,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO local_post_claims (server_id, nonce, local_change_id, owner, claimed_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (server_id, nonce) DO UPDATE
             SET owner = excluded.owner, claimed_ms = excluded.claimed_ms",
        params![server.0, nonce, change.0, by.owner, millis(by.at)],
    )?;
    Ok(())
}

/// Clears the claim under `nonce`: its record and the nonce on the
/// change's drafts. Returns how many drafts were released.
fn release_drafts(
    conn: &rusqlite::Connection,
    server: ServerId,
    change: LocalChangeId,
    nonce: &str,
) -> Result<usize, Error> {
    conn.execute(
        "DELETE FROM local_post_claims WHERE server_id = ?1 AND nonce = ?2",
        params![server.0, nonce],
    )?;
    Ok(conn.execute(
        "UPDATE drafts SET posting_nonce = NULL
         WHERE server_id = ?1 AND reserved_by = ?2 AND posting_nonce = ?3",
        params![server.0, change.0, nonce],
    )?)
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
