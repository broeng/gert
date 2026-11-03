//! Consolidation: sending a local change's review data to Gerrit once the
//! change, or a version of it, is there.
//!
//! [`Syncer::consolidate`] runs on the poll cadence while signed in. It is
//! fully automatic (no confirmation, a user decision) and writes only to
//! the server this syncer talks to. One run, under a per-server lock:
//!
//! 1. **Recovery.** Posts claimed by an earlier run that never finished
//!    (a crash, a lost connection) are checked against the change's
//!    messages: a message by the signed-in account with the post's tag
//!    means it arrived, and it is finished without posting again;
//!    otherwise it is posted again under the same nonce.
//! 2. **Lookup.** Local changes without a Gerrit change are looked up in
//!    batches of up to 50 per project, by `project~branch~Change-Id` when
//!    the target branch is known, else by Change-Id. Not found: `Checking`
//!    becomes `Local`. Several hits with an unknown branch: `Ambiguous`,
//!    never consolidated.
//! 3. **Found closed** (merged or abandoned): the row becomes `Closed`
//!    with its Gerrit number, and nothing is sent; everything is held for
//!    the user (LC5b's held card). Closed rows are checked again on every
//!    run: a restored change goes back to normal handling.
//! 4. **Found open:** the change's detail is refreshed, which maps the
//!    local versions that are Gerrit patch sets. Each such version (an
//!    *exact* match) has its drafts and pending reviews moved onto the
//!    Gerrit patch set in one store transaction; moved drafts are reserved
//!    for the post. A version that is not a Gerrit patch set stays as it
//!    is, unsent and not held: porting it is LC5b's.
//! 5. **Send.** Per revision, the pending reviews in creation order, each
//!    in one `POST …/review`: the reserved comments on that revision inline
//!    (`comments`, the first post of a revision takes them all), the
//!    review's message (its sealed patch-set-level draft, posted as a
//!    `/PATCHSET_LEVEL` comment with its `unresolved` flag, as an ordinary
//!    reply posts it), the votes the rules allow, the tag
//!    `<app>-local-<local change id>-<nonce>`,
//!    `ignore_automatic_attention_set_rules`, `notify` left to Gerrit
//!    (ALL), and `drafts: KEEP`, so that no other draft of the user (a
//!    web UI draft, say) is ever published. Reserved comments left on a
//!    revision without a review go in a comment-only post.
//!
//! # Votes
//!
//! A vote is cast only while its patch set is current on Gerrit. Gerrit
//! 3.14 accepts votes on an older patch set and keeps them there, where
//! they do not count (checked on the dev Gerrit), so the rule is ours:
//! on a non-current exact match the comments and message are posted and
//! every vote is held (the review becomes `Held`). An approval (the
//! label's highest value, when positive, in the target change's
//! `label_values`) held under the strict rule (a local patch set of a
//! Gerrit change) or under `exact` can only be dropped; other held votes
//! may be published onto the current patch set. The held reason says
//! which. A newer review that sets a label supersedes an older unsent
//! review's vote on it.
//!
//! Votes never sink comments: a post refused for its votes (a 400, 403 or
//! 409 whose text mentions a label or a vote, such as "Applying label
//! \"Code-Review\": 2 is restricted") is posted again without them under
//! the same claim and tag, and the votes are recorded as failed. Gerrit
//! answers 200 and silently drops an unknown label or a value the label
//! does not have; those votes, missing from the answer's `labels`, are
//! failed too. A review whose votes failed is `Failed` with the server's
//! text and keeps exactly the votes that did not apply, while its message
//! and comments are gone (they were posted); the held card shows it.
//!
//! # Failures
//!
//! A refusal of the whole post (anything but a vote problem) releases the
//! claim, makes the review `Failed` with the server's text and marks its
//! comments failed, so they are not posted again automatically. A post
//! whose fate is unknown (no answer, a 5xx, an unreadable answer) keeps its
//! claim for the recovery of the next run and ends this one, as does a
//! refused login (the claim is released then: Gerrit did not take it). A
//! lost connection anywhere else ends the run too; what was done stays
//! done, and nothing is half applied.

use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{DefaultHasher, Hash, Hasher},
    sync::atomic::{AtomicU64, Ordering},
};

use gerrit_api::{ChangeQuery, Error as ApiError};
use jiff::{SignedDuration, Timestamp};
use model::{
    AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, Draft, DraftHandling,
    DraftSync, LocalApproval, LocalChange, LocalChangeId, LocalChangeState, LocalDraftId,
    PatchSetNumber, PendingReview, PendingReviewId, PendingReviewState, ReviewInput, ReviewResult,
    Revision,
};
use store::{ConsolidationRewrite, DraftFailure, LocalPost, PatchSetMove, ReviewStateChange};

use super::Syncer;
use crate::Error;

/// Votes as label and value, sorted by label.
type Votes = Vec<(String, i32)>;

/// How many changes one lookup query names.
const LOOKUP_BATCH: usize = 50;
/// The most results one lookup asks for (Gerrit's default query limit).
const LOOKUP_LIMIT: u32 = 500;
/// The clock skew allowed by the "purely local" test.
const SKEW: SignedDuration = SignedDuration::from_mins(5);

/// What [`Syncer::consolidate`] takes besides the store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsolidateOptions {
    /// The `local_approval` setting of each project as it is now. A
    /// project that is not listed counts as `exact`, the strictest.
    pub local_approval: BTreeMap<String, LocalApproval>,
    // LC5b adds the `PortSource` here: what ports the comments of versions
    // that never reached Gerrit onto a Gerrit patch set (see
    // `Syncer::exact_moves`, where those versions are skipped now).
}

/// A local change found on Gerrit in a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundChange {
    pub local_change: LocalChangeId,
    /// The synthetic number its pages had until now.
    pub local_number: ChangeNumber,
    pub gerrit: ChangeNumber,
}

/// How one consolidation post ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostOutcome {
    /// Posted with every vote it carried.
    Sent,
    /// Posted without its votes, which are held (the review is `Held`
    /// with `reason`).
    VotesHeld {
        votes: Vec<(String, i32)>,
        reason: String,
    },
    /// Posted, but these votes did not apply (the review is `Failed`
    /// with `error` and keeps them).
    VotesFailed {
        votes: Vec<(String, i32)>,
        error: String,
    },
    /// Refused as a whole: nothing was posted. The review is `Failed` and
    /// the comments are marked failed.
    Refused { error: String },
}

/// One consolidation post (or a review that needed none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostReport {
    pub local_change: LocalChangeId,
    pub change: ChangeNumber,
    pub patch_set: PatchSetNumber,
    /// The review it sent; `None` for a comment-only post.
    pub review: Option<PendingReviewId>,
    /// Inline comments it carried, the review's message included.
    pub comments: usize,
    /// The votes that were cast.
    pub votes: Vec<(String, i32)>,
    /// Found on the server after an interrupted run, so not posted again.
    pub recovered: bool,
    pub outcome: PostOutcome,
}

/// What a [`Syncer::consolidate`] run did, for the pages to refresh and
/// redirect.
#[derive(Debug, Default)]
pub struct ConsolidateReport {
    /// False when another run on this server was going on, so this one did
    /// nothing.
    pub ran: bool,
    /// Local changes found on Gerrit for the first time: their synthetic
    /// numbers now redirect to the Gerrit change.
    pub found: Vec<FoundChange>,
    /// Local changes whose Gerrit change is merged or abandoned: held.
    pub closed: Vec<(LocalChangeId, ChangeNumber)>,
    /// Closed local changes whose Gerrit change is open again.
    pub reopened: Vec<(LocalChangeId, ChangeNumber)>,
    /// Looked up and not on Gerrit.
    pub not_found: Vec<LocalChangeId>,
    /// Several Gerrit changes match, and the target branch is unknown.
    pub ambiguous: Vec<LocalChangeId>,
    pub posts: Vec<PostReport>,
    /// Local changes with nothing unsent left, all of whose versions are
    /// Gerrit patch sets: tombstones now.
    pub consolidated: Vec<(LocalChangeId, ChangeNumber)>,
    /// Problems that skipped one change but not the run.
    pub errors: Vec<(LocalChangeId, String)>,
    /// What ended the run early (offline, the login refused): the next run
    /// goes on from there.
    pub stopped: Option<Error>,
}

impl ConsolidateReport {
    /// The Gerrit changes whose data changed: their pages should reload.
    pub fn changed_gerrit_changes(&self) -> BTreeSet<ChangeNumber> {
        let mut out: BTreeSet<ChangeNumber> = self.found.iter().map(|f| f.gerrit).collect();
        out.extend(self.closed.iter().map(|c| c.1));
        out.extend(self.reopened.iter().map(|c| c.1));
        out.extend(self.posts.iter().map(|p| p.change));
        out.extend(self.consolidated.iter().map(|c| c.1));
        out
    }
}

/// What a lookup found for one local change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lookup {
    Found(ChangeNumber),
    NotFound,
    Ambiguous,
    /// Not decided: the answer may be incomplete, or the change cannot be
    /// asked about (a malformed Change-Id). Nothing changes; LC5b must not
    /// read this as "not on Gerrit" when it deletes vanished rows.
    Unknown,
}

/// A run-ending failure, or a problem with one change.
enum Problem {
    Stop(Error),
    Skip(String),
}

impl From<Error> for Problem {
    fn from(e: Error) -> Self {
        let ends_run = match &e {
            Error::Api(a) => a.is_retryable() || a.is_unauthorized(),
            Error::Store(_) => true,
            Error::DraftsNotUploaded(_) | Error::Local(_) => false,
        };
        if ends_run {
            Self::Stop(e)
        } else {
            Self::Skip(e.to_string())
        }
    }
}

impl From<store::Error> for Problem {
    fn from(e: store::Error) -> Self {
        Self::Stop(e.into())
    }
}

/// The state a held or failed review gets after its post, with the note.
struct Settled {
    state: PendingReviewState,
    note: Option<String>,
    /// The review's votes that stay unsent; the others are removed.
    keep: Vec<(String, i32)>,
    outcome: PostOutcome,
}

impl Syncer {
    /// The application's name, which starts the tag of every review that
    /// consolidation posts (`<name>-local-<id>-<nonce>`): the app passes
    /// its `APP_NAME`. Tests may leave the default.
    pub fn with_app_name(mut self, name: &str) -> Self {
        self.app_name = name.into();
        self
    }

    /// Sends the review data of local changes that are on Gerrit now (see
    /// the module documentation of `consolidate`). Fully automatic and
    /// idempotent: run it on the poll cadence while signed in and online.
    /// Returns at once, with `ran == false`, while another run on this
    /// server is going on. A lost connection or a refused login ends the
    /// run early with the error in [`ConsolidateReport::stopped`]; a
    /// failing store (or a removed server) is the `Err`.
    ///
    /// Anonymous sessions have nothing to send and do nothing.
    pub async fn consolidate(
        &self,
        options: &ConsolidateOptions,
    ) -> Result<ConsolidateReport, Error> {
        let Some(_guard) = self.consolidate_lock.try_lock() else {
            return Ok(ConsolidateReport::default());
        };
        let mut report = ConsolidateReport {
            ran: true,
            ..ConsolidateReport::default()
        };
        if !self.client.is_authenticated() {
            return Ok(report);
        }
        match self.consolidate_run(options, &mut report).await {
            Ok(()) => Ok(report),
            Err(Error::Store(e)) => Err(Error::Store(e)),
            Err(e) => {
                report.stopped = Some(e);
                Ok(report)
            }
        }
    }

    async fn consolidate_run(
        &self,
        options: &ConsolidateOptions,
        report: &mut ConsolidateReport,
    ) -> Result<(), Error> {
        if self
            .store()
            .local_changes_to_consolidate(self.server)?
            .is_empty()
            && self.store().unfinished_local_posts(self.server)?.is_empty()
        {
            return Ok(());
        }
        let me = self.me().await?;
        // Changes whose interrupted post could not be settled wait for the
        // next run: their drafts are still claimed.
        let mut unsettled = BTreeSet::new();
        let unfinished = self.store().unfinished_local_posts(self.server)?;
        for (change, nonce) in unfinished {
            match self.recover(change, &nonce, me, options, report).await {
                Ok(()) => {}
                Err(Problem::Stop(e)) => return Err(e),
                Err(Problem::Skip(why)) => {
                    unsettled.insert(change);
                    report.errors.push((change, why));
                }
            }
        }

        let rows: Vec<LocalChange> = self
            .store()
            .local_changes_to_consolidate(self.server)?
            .into_iter()
            .filter(|r| !unsettled.contains(&r.id))
            .collect();
        let (known, to_look_up): (Vec<LocalChange>, Vec<LocalChange>) =
            rows.into_iter().partition(|r| r.gerrit_number.is_some());
        let mut work: Vec<(LocalChange, ChangeNumber)> = known
            .into_iter()
            .filter_map(|r| r.gerrit_number.map(|n| (r, n)))
            .collect();
        let to_look_up: Vec<LocalChange> = to_look_up
            .into_iter()
            .filter(|r| {
                matches!(
                    r.state,
                    LocalChangeState::Checking
                        | LocalChangeState::Local
                        | LocalChangeState::Vanished
                )
            })
            .collect();
        for (row, found) in self.look_up(&to_look_up, report).await? {
            match found {
                Lookup::Found(n) => work.push((row, n)),
                Lookup::NotFound => {
                    report.not_found.push(row.id);
                    self.set_state_if(
                        &row,
                        &[LocalChangeState::Checking],
                        LocalChangeState::Local,
                    )?;
                }
                Lookup::Ambiguous => {
                    report.ambiguous.push(row.id);
                    self.set_state_if(
                        &row,
                        &[LocalChangeState::Checking, LocalChangeState::Local],
                        LocalChangeState::Ambiguous,
                    )?;
                }
                Lookup::Unknown => {}
            }
        }
        for (row, number) in work {
            match self.consolidate_change(&row, number, options, report).await {
                Ok(()) => {}
                Err(Problem::Stop(e)) => return Err(e),
                Err(Problem::Skip(why)) => report.errors.push((row.id, why)),
            }
        }
        Ok(())
    }

    /// The signed-in account, as last fetched or fetched now.
    async fn me(&self) -> Result<AccountId, Error> {
        if let Some((id, _)) = self.store().account(self.server)? {
            return Ok(id);
        }
        Ok(self.account_self().await?.id)
    }

    /// Sets `to` if the row is still in one of the states `from` and has
    /// no Gerrit change. Read and written under one store lock.
    fn set_state_if(
        &self,
        row: &LocalChange,
        from: &[LocalChangeState],
        to: LocalChangeState,
    ) -> Result<(), Error> {
        let mut store = self.store();
        let Some(now) = store.local_change(self.server, row.id)? else {
            return Ok(());
        };
        if now.gerrit_number.is_none() && from.contains(&now.state) {
            store.set_local_change_state(self.server, row.id, to)?;
        }
        Ok(())
    }

    // ---- lookup ----

    /// Looks `rows` up on Gerrit, per project in batches. A refused query
    /// skips its batch (recorded); a lost connection ends the run.
    async fn look_up(
        &self,
        rows: &[LocalChange],
        report: &mut ConsolidateReport,
    ) -> Result<Vec<(LocalChange, Lookup)>, Error> {
        let mut by_project: BTreeMap<&str, Vec<&LocalChange>> = BTreeMap::new();
        let mut out = Vec::new();
        for row in rows {
            if lookup_term(row).is_some() && !row.project.contains('"') {
                by_project.entry(&row.project).or_default().push(row);
            } else {
                out.push((row.clone(), Lookup::Unknown));
            }
        }
        for (project, rows) in by_project {
            for batch in rows.chunks(LOOKUP_BATCH) {
                let terms: Vec<String> = batch.iter().filter_map(|r| lookup_term(r)).collect();
                let q = format!("({}) project:\"{project}\"", terms.join(" OR "));
                let page = match self
                    .client
                    .query_changes(&ChangeQuery::new(q).limit(LOOKUP_LIMIT))
                    .await
                {
                    Ok(page) => page,
                    Err(e) => match Problem::from(Error::from(e)) {
                        Problem::Stop(e) => return Err(e),
                        Problem::Skip(why) => {
                            report
                                .errors
                                .extend(batch.iter().map(|r| (r.id, why.clone())));
                            continue;
                        }
                    },
                };
                for row in batch {
                    let found = classify_hits(row, &page.changes, page.more_changes);
                    out.push(((*row).clone(), found));
                }
            }
        }
        Ok(out)
    }

    // ---- one change ----

    /// Handles a local change whose Gerrit change is `number`: closed →
    /// held; open → moves exact versions and sends.
    async fn consolidate_change(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        options: &ConsolidateOptions,
        report: &mut ConsolidateReport,
    ) -> Result<(), Problem> {
        let detail = self.refresh_change(number).await?;
        // The row as it is after the refresh, which mapped its versions.
        let Some(row) = self.store().local_change(self.server, row.id)? else {
            return Ok(());
        };
        if row.gerrit_number.is_some_and(|g| g != number) {
            return Err(Problem::Skip(format!(
                "belongs to change {} on Gerrit, not {number}",
                row.gerrit_number.map_or(0, |g| g.0)
            )));
        }
        let first_found = row.gerrit_number.is_none();
        if detail.summary.status != ChangeStatus::New {
            if row.state != LocalChangeState::Closed || first_found {
                self.rewrite(
                    &row,
                    number,
                    Vec::new(),
                    Some(LocalChangeState::Closed),
                    None,
                )?;
                report.closed.push((row.id, number));
            }
            if first_found {
                report.found.push(found(&row, number));
            }
            return Ok(());
        }
        let (moves, all_on_gerrit) = self.exact_moves(&row, number, &detail)?;
        let reopened = row.state == LocalChangeState::Closed;
        // Looked up (checking) or open again (closed): local now. A
        // vanished row keeps its state for LC5b.
        let settled = matches!(
            row.state,
            LocalChangeState::Checking | LocalChangeState::Closed
        )
        .then_some(LocalChangeState::Local);
        let (state, else_state) = if all_on_gerrit {
            (Some(LocalChangeState::Consolidated), settled)
        } else {
            (settled, None)
        };
        self.rewrite(&row, number, moves, state, else_state)?;
        if first_found {
            report.found.push(found(&row, number));
        }
        if reopened {
            report.reopened.push((row.id, number));
        }
        let posted_before = report.posts.len();
        let sent = self.send(&row, number, &detail, options, report).await;
        if report.posts[posted_before..]
            .iter()
            .any(|p| !matches!(p.outcome, PostOutcome::Refused { .. }) && !p.recovered)
        {
            self.refresh_after_posts(number).await;
        }
        sent?;
        self.finish_change(&row, number, report)?;
        Ok(())
    }

    fn rewrite(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        moves: Vec<PatchSetMove>,
        state: Option<LocalChangeState>,
        else_state: Option<LocalChangeState>,
    ) -> Result<(), Problem> {
        let rewrite = ConsolidationRewrite {
            local_change: row.id,
            gerrit_number: number,
            moves,
            keep: Vec::new(),
            review_states: Vec::new(),
            state,
            else_state,
        };
        match self.store().rewrite_local_change(self.server, &rewrite) {
            Ok(_) => Ok(()),
            Err(e @ (store::Error::LocalTarget | store::Error::OtherGerritChange(_))) => {
                Err(Problem::Skip(e.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// The moves of `row`'s versions that are patch sets of `detail` (exact
    /// matches): each version's drafts and pending reviews onto its patch
    /// set. Also whether every version is a Gerrit patch set.
    ///
    /// LC5b: the versions that are not (amended before the upload) are
    /// skipped here; porting them is where a `PortSource` comes in.
    fn exact_moves(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
    ) -> Result<(Vec<PatchSetMove>, bool), Problem> {
        let store = self.store();
        let versions = store.local_patch_sets(self.server, row.id)?;
        let mut drafts = store.load_drafts(self.server, row.number)?;
        drafts.extend(store.load_drafts(self.server, number)?);
        let reviews = store.pending_reviews_of_local_change(self.server, row.id)?;
        let mut moves = Vec::new();
        let mut all_on_gerrit = true;
        for v in &versions {
            let Some(target) = detail
                .revisions
                .iter()
                .find(|r| r.sha == v.sha && !r.number.is_local())
            else {
                all_on_gerrit = false;
                continue;
            };
            let move_drafts: Vec<(LocalDraftId, Option<store::DraftAnchor>)> = drafts
                .iter()
                .filter(|d| {
                    d.patch_set == v.number
                        && d.is_visible()
                        && !d.is_sealed()
                        && d.reserved_by.is_none()
                        && d.held_reason.is_none()
                })
                .map(|d| (d.local_id, None))
                .collect();
            let move_reviews: Vec<PendingReviewId> = reviews
                .iter()
                .filter(|r| {
                    r.patch_set == v.number
                        && matches!(
                            r.state,
                            PendingReviewState::Pending
                                | PendingReviewState::Held
                                | PendingReviewState::Failed
                        )
                })
                .map(|r| r.id)
                .collect();
            moves.push(PatchSetMove {
                from: v.number,
                to: target.as_ref(),
                exact: true,
                drafts: move_drafts,
                reviews: move_reviews,
            });
        }
        Ok((moves, all_on_gerrit))
    }

    /// Marks the row consolidated if all its versions are Gerrit patch
    /// sets; the store refuses while anything is unsent.
    fn finish_change(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        report: &mut ConsolidateReport,
    ) -> Result<(), Problem> {
        let mut store = self.store();
        let Some(now) = store.local_change(self.server, row.id)? else {
            return Ok(());
        };
        if now.state == LocalChangeState::Consolidated {
            report.consolidated.push((row.id, number));
            return Ok(());
        }
        if now.state == LocalChangeState::Closed {
            return Ok(());
        }
        let all_mapped = store
            .local_patch_sets(self.server, row.id)?
            .iter()
            .all(|v| v.gerrit_patch_set.is_some());
        if all_mapped
            && store.set_local_change_state(self.server, row.id, LocalChangeState::Consolidated)?
                == Some(LocalChangeState::Consolidated)
        {
            report.consolidated.push((row.id, number));
        }
        Ok(())
    }

    /// Brings the cached change and comments up to date after posts, so
    /// the pages show what was posted. Failures are only logged: the posts
    /// are done.
    async fn refresh_after_posts(&self, number: ChangeNumber) {
        if let Err(e) = self.refresh_change(number).await {
            log::warn!("refreshing change {number} after consolidation: {e}");
        }
        if let Err(e) = self.refresh_comments(number).await {
            log::warn!("refreshing comments of change {number} after consolidation: {e}");
        }
    }

    // ---- sending ----

    /// Sends the moved pending reviews of `row` and its reserved comments,
    /// per revision.
    async fn send(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        options: &ConsolidateOptions,
        report: &mut ConsolidateReport,
    ) -> Result<(), Problem> {
        let reviews: Vec<PendingReview> = self
            .store()
            .pending_reviews_of_local_change(self.server, row.id)?
            .into_iter()
            .filter(|r| {
                r.state == PendingReviewState::Pending
                    && r.change == number
                    && !r.patch_set.is_local()
            })
            .collect();
        self.supersede(row, number, &reviews)?;
        let mut revisions: BTreeSet<PatchSetNumber> = reviews.iter().map(|r| r.patch_set).collect();
        revisions.extend(
            self.postable_comments(row.id, None)?
                .iter()
                .map(|d| d.patch_set),
        );
        for ps in revisions {
            let Some(rev) = detail.revision(ps) else {
                report
                    .errors
                    .push((row.id, format!("patch set {ps} is not on change {number}")));
                continue;
            };
            for r in reviews.iter().filter(|r| r.patch_set == ps) {
                let nonce = new_nonce();
                let claimed = self.store().claim_local_post(
                    self.server,
                    row.id,
                    Some((r.id, &[PendingReviewState::Pending])),
                    &nonce,
                )?;
                if let Some(post) = claimed {
                    self.post_claimed(row, number, detail, rev, post, &nonce, options, report)
                        .await?;
                }
            }
            if !self.postable_comments(row.id, Some(ps))?.is_empty() {
                let nonce = new_nonce();
                let claimed = self
                    .store()
                    .claim_local_post(self.server, row.id, None, &nonce)?;
                if let Some(post) = claimed {
                    self.post_claimed(row, number, detail, rev, post, &nonce, options, report)
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// The reserved comments of `change` that a post would carry (on `ps`,
    /// if given): not review messages, not failed before.
    fn postable_comments(
        &self,
        change: LocalChangeId,
        ps: Option<PatchSetNumber>,
    ) -> Result<Vec<Draft>, Problem> {
        Ok(self
            .store()
            .consolidation_drafts(self.server, change)?
            .into_iter()
            .filter(|d| postable(d) && ps.is_none_or(|ps| d.patch_set == ps))
            .collect())
    }

    /// A newer review that sets a label supersedes the vote of every older
    /// unsent review of the change on that label: the older one loses it
    /// before anything is sent.
    fn supersede(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        sending: &[PendingReview],
    ) -> Result<(), Problem> {
        let mut store = self.store();
        let mut older = store.pending_reviews(self.server, number)?;
        for r in store.pending_reviews_of_local_change(self.server, row.id)? {
            if !older.iter().any(|o| o.id == r.id) {
                older.push(r);
            }
        }
        older.retain(|o| {
            matches!(
                o.state,
                PendingReviewState::Pending | PendingReviewState::Held | PendingReviewState::Failed
            )
        });
        for newer in sending {
            for label in newer.labels.keys() {
                for o in older
                    .iter()
                    .filter(|o| o.id < newer.id && o.labels.contains_key(label))
                {
                    store.remove_review_label(self.server, o.id, label)?;
                }
            }
        }
        Ok(())
    }

    /// Posts a claimed post to `rev` and settles the claim (see the module
    /// documentation for the outcomes).
    #[allow(clippy::too_many_arguments)]
    async fn post_claimed(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        rev: &Revision,
        post: LocalPost,
        nonce: &str,
        options: &ConsolidateOptions,
        report: &mut ConsolidateReport,
    ) -> Result<(), Problem> {
        let review = post.review.as_ref();
        let parts = Parts::of(&post, rev.number);
        let current = detail.current_patch_set == Some(rev.number);
        let votes: BTreeMap<String, i32> = review.map(|r| r.labels.clone()).unwrap_or_default();
        let (send_votes, held) = if current || votes.is_empty() {
            (votes, None)
        } else {
            let reason = held_reason(row, detail, rev.number, review, &votes, options);
            (BTreeMap::new(), Some((votes, reason)))
        };
        let mut entry = PostReport {
            local_change: row.id,
            change: number,
            patch_set: rev.number,
            review: review.map(|r| r.id),
            comments: parts.comments.len(),
            votes: Vec::new(),
            recovered: false,
            outcome: PostOutcome::Sent,
        };
        let held_settled = |votes: BTreeMap<String, i32>, reason: String| Settled {
            state: PendingReviewState::Held,
            note: Some(reason.clone()),
            keep: votes.clone().into_iter().collect(),
            outcome: PostOutcome::VotesHeld {
                votes: votes.into_iter().collect(),
                reason,
            },
        };
        if parts.comments.is_empty() && send_votes.is_empty() {
            // Nothing to post: a review whose votes were all superseded or
            // are held, without a message.
            let settled = held.map(|(v, reason)| held_settled(v, reason));
            if let Some(s) = &settled {
                entry.outcome = s.outcome.clone();
            }
            self.settle(row, &post, &parts.posted, settled)?;
            if review.is_some() {
                report.posts.push(entry);
            }
            return Ok(());
        }
        let input = ReviewInput {
            labels: send_votes,
            comments: parts.comments.iter().map(|d| d.input()).collect(),
            drafts: DraftHandling::Keep,
            tag: Some(self.local_tag(row.id, nonce)),
            notify: None,
            ignore_automatic_attention_set_rules: true,
            ..ReviewInput::default()
        };
        if let Err(e) = self.ensure_on_server(&rev.sha) {
            self.release(row, &post, nonce)?;
            return Err(e.into());
        }
        let answer = match self.client.post_review(number, &rev.sha, &input).await {
            Err(e) if !input.labels.is_empty() && is_vote_refusal(&e) => {
                // Votes never sink comments: the same post without them,
                // under the same claim and tag.
                let error = refusal_text(&e);
                let failed = Settled {
                    state: PendingReviewState::Failed,
                    note: Some(error.clone()),
                    keep: input.labels.clone().into_iter().collect(),
                    outcome: PostOutcome::VotesFailed {
                        votes: input.labels.clone().into_iter().collect(),
                        error,
                    },
                };
                let bare = ReviewInput {
                    labels: BTreeMap::new(),
                    ..input.clone()
                };
                if bare.comments.is_empty() {
                    Ok(Some(failed))
                } else {
                    match self.client.post_review(number, &rev.sha, &bare).await {
                        Ok(_) => Ok(Some(failed)),
                        Err(e) => Err(e),
                    }
                }
            }
            Ok(result) => Ok(match votes_after(&input, &result, rev.number) {
                Some(s) => Some(s),
                None => held.map(|(v, reason)| held_settled(v, reason)),
            }),
            Err(e) => Err(e),
        };
        match answer {
            Ok(settled) => {
                entry.votes = match &settled {
                    Some(s) if s.state == PendingReviewState::Failed => input
                        .labels
                        .iter()
                        .filter(|(l, v)| !s.keep.contains(&((*l).clone(), **v)))
                        .map(|(l, v)| (l.clone(), *v))
                        .collect(),
                    _ => input.labels.clone().into_iter().collect(),
                };
                if let Some(s) = &settled {
                    entry.outcome = s.outcome.clone();
                }
                self.settle(row, &post, &parts.posted, settled)?;
                report.posts.push(entry);
                Ok(())
            }
            Err(e) => {
                let refused = self.refused(row, &post, &parts, nonce, e)?;
                entry.outcome = PostOutcome::Refused { error: refused };
                report.posts.push(entry);
                Ok(())
            }
        }
    }

    /// Gives up a claim before anything was sent: the drafts are free
    /// again and the review is `Pending` again.
    fn release(&self, row: &LocalChange, post: &LocalPost, nonce: &str) -> Result<(), Problem> {
        let mut store = self.store();
        store.release_local_post(self.server, row.id, nonce)?;
        if let Some(r) = &post.review {
            store.set_pending_review_state(
                self.server,
                r.id,
                &[PendingReviewState::Sending],
                PendingReviewState::Pending,
                None,
            )?;
        }
        Ok(())
    }

    /// Records a post that reached the server: removes what it posted and
    /// sets the review `Sent`, or as `settled` says (removing the votes
    /// that did apply).
    fn settle(
        &self,
        row: &LocalChange,
        post: &LocalPost,
        posted: &[(LocalDraftId, u32)],
        settled: Option<Settled>,
    ) -> Result<(), Problem> {
        let mut store = self.store();
        let states: Vec<ReviewStateChange> = post
            .review
            .iter()
            .map(|r| ReviewStateChange {
                review: r.id,
                from: vec![PendingReviewState::Sending],
                state: settled
                    .as_ref()
                    .map_or(PendingReviewState::Sent, |s| s.state),
                note: settled.as_ref().and_then(|s| s.note.clone()),
            })
            .collect();
        store.finish_local_post(self.server, row.id, posted, &states)?;
        if let (Some(review), Some(s)) = (&post.review, &settled) {
            for label in review.labels.keys() {
                if !s.keep.iter().any(|(l, _)| l == label) {
                    store.remove_review_label(self.server, review.id, label)?;
                }
            }
        }
        Ok(())
    }

    /// Handles a post that failed with `e`. A refusal releases the claim:
    /// the review becomes `Failed` (`Pending` again for a refused login,
    /// which also ends the run) and its comments are marked failed;
    /// returns the server's text. Anything else may have reached the
    /// server, so the claim stays for the next run's recovery and the run
    /// ends.
    fn refused(
        &self,
        row: &LocalChange,
        post: &LocalPost,
        parts: &Parts,
        nonce: &str,
        e: ApiError,
    ) -> Result<String, Problem> {
        let refused = matches!(
            e,
            ApiError::BadRequest { .. }
                | ApiError::Forbidden { .. }
                | ApiError::Conflict { .. }
                | ApiError::NotFound
                | ApiError::Unauthorized
                | ApiError::LocalNumber(_)
                | ApiError::InvalidUrl(_)
        );
        if !refused {
            return Err(Problem::Stop(e.into()));
        }
        let text = refusal_text(&e);
        let mut store = self.store();
        store.release_local_post(self.server, row.id, nonce)?;
        let login = e.is_unauthorized();
        if let Some(review) = &post.review {
            let (to, note) = if login {
                (PendingReviewState::Pending, None)
            } else {
                (PendingReviewState::Failed, Some(text.as_str()))
            };
            store.set_pending_review_state(
                self.server,
                review.id,
                &[PendingReviewState::Sending],
                to,
                note,
            )?;
        }
        if login {
            return Err(Problem::Stop(e.into()));
        }
        let failure = DraftFailure::Rejected {
            error: text.clone(),
            retryable: false,
        };
        for d in &parts.comments {
            if d.pending_review.is_none() {
                store.record_draft_failure(
                    self.server,
                    d.local_id,
                    d.version,
                    &failure,
                    Timestamp::now(),
                )?;
            }
        }
        Ok(text)
    }

    fn local_tag(&self, change: LocalChangeId, nonce: &str) -> String {
        format!("{}-local-{}-{nonce}", self.app_name, change.0)
    }

    // ---- recovery ----

    /// Settles a post claimed under `nonce` by an earlier run: finished if
    /// the change has a message by `me` with its tag, else posted again
    /// under the same nonce.
    async fn recover(
        &self,
        change: LocalChangeId,
        nonce: &str,
        me: AccountId,
        options: &ConsolidateOptions,
        report: &mut ConsolidateReport,
    ) -> Result<(), Problem> {
        let (row, post) = {
            let store = self.store();
            let Some(row) = store.local_change(self.server, change)? else {
                return Ok(());
            };
            let Some(post) = store.local_post(self.server, change, nonce)? else {
                return Ok(());
            };
            (row, post)
        };
        let Some(number) = row.gerrit_number else {
            // Never posted: nothing is claimed without a Gerrit change.
            return self.release(&row, &post, nonce);
        };
        let detail = self.refresh_change(number).await?;
        let tag = self.local_tag(change, nonce);
        let message = detail.messages.iter().find(|m| {
            m.tag.as_deref() == Some(tag.as_str())
                && (m.author.as_ref().is_some_and(|a| a.id == me)
                    || m.real_author.as_ref().is_some_and(|a| a.id == me))
        });
        let ps = post
            .review
            .as_ref()
            .map(|r| r.patch_set)
            .or_else(|| message.and_then(|m| m.revision))
            .or_else(|| post.drafts.iter().map(|d| d.patch_set).min());
        let Some(rev) = ps.and_then(|ps| detail.revision(ps)) else {
            return Err(Problem::Skip(format!(
                "the interrupted post's patch set is not on change {number}"
            )));
        };
        if message.is_none() {
            if detail.summary.status != ChangeStatus::New {
                // Closed meanwhile: nothing is sent automatically any more.
                return self.release(&row, &post, nonce);
            }
            return self
                .post_claimed(&row, number, &detail, rev, post, nonce, options, report)
                .await;
        }
        // It arrived. Votes that are not on the change now did not apply,
        // or were held for a patch set that is not current.
        let parts = Parts::of(&post, rev.number);
        let votes: BTreeMap<String, i32> = post
            .review
            .as_ref()
            .map(|r| r.labels.clone())
            .unwrap_or_default();
        let (applied, missing): (Votes, Votes) =
            votes.clone().into_iter().partition(|(label, value)| {
                detail
                    .votes
                    .get(label)
                    .is_some_and(|all| all.iter().any(|v| v.account.id == me && v.value == *value))
            });
        let settled = if missing.is_empty() {
            None
        } else if detail.current_patch_set == Some(rev.number) {
            let error = format!(
                "the review was posted, but Gerrit does not show {}",
                describe_votes(&missing)
            );
            Some(Settled {
                state: PendingReviewState::Failed,
                note: Some(error.clone()),
                keep: missing.clone(),
                outcome: PostOutcome::VotesFailed {
                    votes: missing,
                    error,
                },
            })
        } else {
            let held: BTreeMap<String, i32> = missing.iter().cloned().collect();
            let reason = held_reason(
                &row,
                &detail,
                rev.number,
                post.review.as_ref(),
                &held,
                options,
            );
            Some(Settled {
                state: PendingReviewState::Held,
                note: Some(reason.clone()),
                keep: missing.clone(),
                outcome: PostOutcome::VotesHeld {
                    votes: missing,
                    reason,
                },
            })
        };
        let outcome = settled
            .as_ref()
            .map_or(PostOutcome::Sent, |s| s.outcome.clone());
        self.settle(&row, &post, &parts.posted, settled)?;
        report.posts.push(PostReport {
            local_change: change,
            change: number,
            patch_set: rev.number,
            review: post.review.as_ref().map(|r| r.id),
            comments: parts.comments.len(),
            votes: applied,
            recovered: true,
            outcome,
        });
        Ok(())
    }
}

/// What a claimed post sends to one revision.
struct Parts<'a> {
    /// The inline comments: the reserved comments on the revision that
    /// did not fail before, and the review's message.
    comments: Vec<&'a Draft>,
    /// `(id, version)` of each, for `finish_local_post`.
    posted: Vec<(LocalDraftId, u32)>,
}

impl<'a> Parts<'a> {
    fn of(post: &'a LocalPost, ps: PatchSetNumber) -> Self {
        let review = post.review.as_ref().map(|r| r.id);
        let comments: Vec<&Draft> = post
            .drafts
            .iter()
            .filter(|d| {
                d.patch_set == ps
                    && match d.pending_review {
                        // The review's message, posted as a patch-set-level
                        // comment, as an ordinary reply posts it: that keeps
                        // its `unresolved` flag, which a `message` cannot
                        // carry.
                        Some(r) => Some(r) == review && !d.message.trim().is_empty(),
                        None => postable(d),
                    }
            })
            .collect();
        let posted = comments.iter().map(|d| (d.local_id, d.version)).collect();
        Self { comments, posted }
    }
}

/// Whether a reserved draft is a comment a post may carry: not a review's
/// message, not deleted, not refused before.
fn postable(d: &Draft) -> bool {
    d.pending_review.is_none() && d.is_visible() && !matches!(d.sync, DraftSync::Failed { .. })
}

/// What happened to the votes of a post that the server took: `None` when
/// all applied. Votes missing from the answer were dropped (an unknown
/// label, a value the label does not have: Gerrit answers 200 for those);
/// votes that arrived after their patch set stopped being current count
/// only there, so they did not do what was meant either.
fn votes_after(input: &ReviewInput, result: &ReviewResult, ps: PatchSetNumber) -> Option<Settled> {
    if input.labels.is_empty() {
        return None;
    }
    if let Some(current) = result.current_patch_set
        && current != ps
    {
        let votes: Vec<(String, i32)> = input.labels.clone().into_iter().collect();
        let error = format!(
            "PS {} was no longer current when {} arrived (PS {} is), so {} only on PS {}",
            ps.0,
            describe_votes(&votes),
            current.0,
            if votes.len() == 1 {
                "it counts"
            } else {
                "they count"
            },
            ps.0
        );
        return Some(Settled {
            state: PendingReviewState::Failed,
            note: Some(error.clone()),
            keep: votes.clone(),
            outcome: PostOutcome::VotesFailed { votes, error },
        });
    }
    let dropped = input.unapplied_labels(result);
    if dropped.is_empty() {
        return None;
    }
    let error = format!("Gerrit did not apply {}", describe_votes(&dropped));
    Some(Settled {
        state: PendingReviewState::Failed,
        note: Some(error.clone()),
        keep: dropped.clone(),
        outcome: PostOutcome::VotesFailed {
            votes: dropped,
            error,
        },
    })
}

/// Whether the server refused a post because of its votes: a 400, 403 or
/// 409 that names a label or a vote. Seen on the dev Gerrit (3.14):
/// 403 `Applying label "Code-Review": 2 is restricted`, 409 `Cannot reduce
/// vote on labels for closed change: Code-Review`. Gerrit accepts votes on
/// a patch set that is not current, so no refusal says that (it is caught
/// from the answer's current patch set instead, see [`votes_after`]); "not
/// current" is still read as a vote problem, should a server say it.
fn is_vote_refusal(e: &ApiError) -> bool {
    let message = match e {
        ApiError::BadRequest { message }
        | ApiError::Forbidden { message }
        | ApiError::Conflict { message } => message.to_lowercase(),
        _ => return false,
    };
    message.contains("label") || message.contains("vote") || message.contains("not current")
}

/// The server's text of a refusal, else the error.
fn refusal_text(e: &ApiError) -> String {
    match e {
        ApiError::BadRequest { message }
        | ApiError::Forbidden { message }
        | ApiError::Conflict { message }
            if !message.trim().is_empty() =>
        {
            message.trim().to_owned()
        }
        _ => e.to_string(),
    }
}

/// Why the votes of a review on non-current patch set `ps` are held, and
/// what the user may do with them. An approval held under the strict rule
/// (a local patch set of a Gerrit change, or votes given after the change
/// was created) or under `exact` can only be dropped.
fn held_reason(
    row: &LocalChange,
    detail: &ChangeDetail,
    ps: PatchSetNumber,
    review: Option<&PendingReview>,
    votes: &BTreeMap<String, i32>,
    options: &ConsolidateOptions,
) -> String {
    // "Purely local": the votes were given before the change was on Gerrit.
    let created = detail.summary.created;
    let purely_local = review.is_some_and(|r| {
        created
            .checked_sub(SKEW)
            .is_ok_and(|limit| r.voted_at < limit)
    });
    let strategy = review.map_or(LocalApproval::Exact, |r| {
        r.strategy.max(
            options
                .local_approval
                .get(&row.project)
                .copied()
                .unwrap_or(LocalApproval::Exact),
        )
    });
    let drop_only = !purely_local || strategy == LocalApproval::Exact;
    let (approvals, others): (Votes, Votes) = votes
        .iter()
        .map(|(l, v)| (l.clone(), *v))
        .partition(|(l, v)| is_approval(detail, l, *v));
    let current = detail
        .current_patch_set
        .map_or_else(|| "another".to_owned(), |c| format!("PS {}", c.0));
    let mut reason = format!(
        "PS {} is not the current patch set on Gerrit ({current} is), so these votes were not sent.",
        ps.0
    );
    if !approvals.is_empty() {
        if drop_only {
            reason.push_str(&format!(
                " {}: an approval goes only onto the patch set it was given on while that is \
                 current, so it can only be dropped.",
                describe_votes(&approvals)
            ));
        } else {
            reason.push_str(&format!(
                " {}: publish onto the current patch set, or drop.",
                describe_votes(&approvals)
            ));
        }
    }
    if !others.is_empty() {
        reason.push_str(&format!(
            " {}: publish onto the current patch set, or drop.",
            describe_votes(&others)
        ));
    }
    reason
}

/// Whether `value` is an approval of `label` on the target change: the
/// label's highest value, when that is positive.
fn is_approval(detail: &ChangeDetail, label: &str, value: i32) -> bool {
    detail
        .label_values
        .get(label)
        .and_then(|values| values.iter().map(|v| v.value).max())
        .is_some_and(|max| max > 0 && value == max)
}

/// "Code-Review +2, Verified -1".
fn describe_votes(votes: &[(String, i32)]) -> String {
    votes
        .iter()
        .map(|(l, v)| {
            if *v > 0 {
                format!("{l} +{v}")
            } else {
                format!("{l} {v}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A Change-Id Gerrit can be asked about without matching anything else:
/// `I` and 40 hex digits (a shorter one would be a prefix search).
fn is_change_id(id: &str) -> bool {
    id.len() == 41
        && id.starts_with('I')
        && id[1..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The lookup term of a row: `change:"project~branch~Id"` with a known
/// branch, else `change:Id`. `None` for a Change-Id that cannot be asked
/// about.
fn lookup_term(row: &LocalChange) -> Option<String> {
    if !is_change_id(&row.change_id) {
        return None;
    }
    Some(match &row.branch {
        Some(branch) if !branch.contains('"') && !row.project.contains('"') => {
            format!("change:\"{}~{branch}~{}\"", row.project, row.change_id)
        }
        _ => format!("change:{}", row.change_id),
    })
}

/// What the lookup's answer says about `row`. A row with a known branch
/// takes only hits on that branch.
fn classify_hits(row: &LocalChange, hits: &[ChangeSummary], more: bool) -> Lookup {
    let matching: Vec<&ChangeSummary> = hits
        .iter()
        .filter(|c| {
            c.project == row.project
                && c.change_id.as_deref() == Some(row.change_id.as_str())
                && row.branch.as_ref().is_none_or(|b| *b == c.branch)
        })
        .collect();
    match (matching.as_slice(), more) {
        // The answer may be cut off: "none" or "one" might be wrong.
        (_, true) if matching.len() < 2 => Lookup::Unknown,
        ([], false) => Lookup::NotFound,
        ([one], false) => Lookup::Found(one.number),
        _ => Lookup::Ambiguous,
    }
}

fn found(row: &LocalChange, gerrit: ChangeNumber) -> FoundChange {
    FoundChange {
        local_change: row.id,
        local_number: row.number,
        gerrit,
    }
}

/// A fresh nonce for a post: unique per process run and over runs (the
/// time is part of it), which is all the tag needs.
fn new_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = DefaultHasher::new();
    (
        Timestamp::now().as_nanosecond(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        std::process::id(),
    )
        .hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests;
