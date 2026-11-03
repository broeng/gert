//! Consolidation: sending a local change's review data to Gerrit once the
//! change, or a version of it, is there.
//!
//! [`Syncer::consolidate`] runs on the poll cadence while signed in. It is
//! fully automatic (no confirmation, a user decision) and writes only to
//! the server this syncer talks to.
//!
//! # One run at a time
//!
//! A run holds the server's consolidation lease in the store
//! (`consolidation_leases`) and renews it before every post, so one run per
//! server goes on at a time, also across processes on one cache (a second
//! `gert`, a new session after a re-login). Every claim of a post records
//! the run's owner token and the time; a run recovers only the posts of
//! other runs that are older than [`CLAIM_STALE`], so a request still in
//! flight is never posted a second time. That assumes that a request not
//! answered within that time never reaches the server; the lease lasts as
//! long, so a run held up that long loses it and stops before posting
//! again ([`Error::LeaseLost`]).
//!
//! # A run
//!
//! 1. **Recovery.** Posts claimed by a run that is gone (a crash, a lost
//!    connection) are checked against the change's messages: a message by
//!    the signed-in account with the post's tag means it arrived, and it
//!    is finished without posting again (the votes it cast are read from
//!    that message); otherwise it is posted again under the same nonce.
//! 2. **Lookup.** Local changes without a Gerrit change are looked up in
//!    batches of up to 50 per project, by `project~branch~Change-Id` (each
//!    part percent-encoded: Gerrit decodes them) when the target branch is
//!    known, else by Change-Id. Not found: `Checking` becomes `Local`.
//!    Several hits with an unknown branch: `Ambiguous`, never
//!    consolidated. A Change-Id Gerrit cannot be asked about (not `I` and
//!    40 hex digits) is `Local` at once.
//! 3. **Known changes.** Local changes with a Gerrit change are asked about
//!    in batches of up to 50 by number, with their revisions: one request
//!    per batch, not per change. Those with nothing that could be sent
//!    (only held or failed items) are asked about at most every
//!    [`SLOW_RECHECK`]. The detail of a change is fetched only when there is
//!    something to move or send.
//! 4. **Found closed** (merged or abandoned): everything of the row is held
//!    (`Closed`, reviews `Held`, drafts with a held reason) and never sent
//!    automatically, also if the change is restored later (a user
//!    decision): the user publishes or drops it from the held card.
//! 5. **Found open:** each local version that is a Gerrit patch set (an
//!    *exact* match) has its drafts and pending reviews moved onto that
//!    patch set in one store transaction; moved drafts are reserved for
//!    the post. A version that is not a Gerrit patch set stays as it is,
//!    unsent and not held: porting it is LC5b's.
//! 6. **Send.** Per revision, the pending reviews in creation order, each
//!    in one `POST …/review`: the reserved comments on that revision inline
//!    (`comments`; the first post of a revision takes them all), the
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
//! they do not count (checked on the dev Gerrit), so the rule is ours: on
//! a non-current exact match the comments and message are posted and every
//! vote is held (the review becomes `Held`). An approval (the label's
//! highest value, when positive, in the target change's `label_values`)
//! held under the strict rule (a local patch set of a Gerrit change) or
//! under `exact` may only be dropped: its label is in the review's
//! `drop_only`, which the held card obeys. The current patch set is taken
//! from every answer (`change_info`), so once a new patch set shows up, the
//! later posts of the run hold their votes too. Votes that arrived after a
//! new patch set (the answer says so, or, from servers that do not send
//! the change along, the refreshed change does) were cast on the old one:
//! the review is `Held`, its approvals drop-only, and the reason says
//! where they landed. A newer review supersedes an older unsent review's
//! vote on a label, but only when the newer vote will be cast.
//!
//! Votes never sink comments: a post refused for its votes (a 400, 403 or
//! 409 whose text mentions a label or a vote, such as "Applying label
//! \"Code-Review\": 2 is restricted") is posted again without them under
//! the same claim and tag, and the votes are recorded as failed. Gerrit
//! answers 200 and silently drops an unknown label or a value the label
//! does not have; those votes, missing from the answer's `labels`, are
//! failed too. A review whose votes failed is `Failed` with the server's
//! text and keeps exactly the votes that did not apply, while its message
//! and comments are gone (they were posted); the held card shows it. An
//! answer that says the change is closed (a 409 naming "closed") sends
//! nothing more: the change is held as closed.
//!
//! # Failures
//!
//! A refusal of the whole post (anything but a vote problem) gives the
//! claim up in one transaction: the review becomes `Failed` with the
//! server's text and its comments are held with it as their reason, so
//! they show in the held card, block nothing and are not posted again
//! automatically. A post whose fate is unknown (no answer, a 5xx, an
//! unreadable answer) keeps its claim for recovery and ends the run, as
//! does a refused login (the claim is given up then: Gerrit did not take
//! it). A lost connection anywhere else ends the run too; what was done
//! stays done, and nothing is half applied.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    hash::{DefaultHasher, Hash, Hasher},
    sync::atomic::{AtomicU64, Ordering},
};

use gerrit_api::{ChangeOption, ChangeQuery, Error as ApiError};
use jiff::{SignedDuration, Timestamp};
use model::{
    AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, Draft, DraftHandling,
    DraftSync, LocalApproval, LocalChange, LocalChangeId, LocalChangeState, LocalDraftId,
    LocalPatchSet, PatchSetNumber, PendingReview, PendingReviewId, PendingReviewState, ReviewInput,
    ReviewResult, Revision,
};
use store::{Claimant, ConsolidationRewrite, LocalPost, PatchSetMove, ReviewStateChange};

use super::Syncer;
use crate::Error;

/// Votes as label and value, sorted by label.
type Votes = Vec<(String, i32)>;

/// How many changes one query names.
const BATCH: usize = 50;
/// The most results one lookup asks for (Gerrit's default query limit).
const LOOKUP_LIMIT: u32 = 500;
/// The clock skew allowed by the "purely local" test.
const SKEW: SignedDuration = SignedDuration::from_mins(5);
/// How long the consolidation lease lasts without renewal, and how old a
/// claim of another run must be before it is recovered (by default): far
/// beyond any request's life (the HTTP client gives up connecting after
/// 10 s, and Gerrit answers a review within seconds).
pub const CLAIM_STALE: SignedDuration = SignedDuration::from_mins(15);
/// How often a local change with nothing that could be sent (only held or
/// failed items, or a closed change) is asked about (by default).
pub const SLOW_RECHECK: SignedDuration = SignedDuration::from_mins(30);

/// The waits of consolidation (see [`Syncer::with_consolidation_timing`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsolidationTiming {
    /// How old a claim of another run must be before it is recovered.
    pub claim_stale: SignedDuration,
    /// How often a change with nothing that could be sent is asked about.
    pub slow_recheck: SignedDuration,
}

impl Default for ConsolidationTiming {
    fn default() -> Self {
        Self {
            claim_stale: CLAIM_STALE,
            slow_recheck: SLOW_RECHECK,
        }
    }
}

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
    /// Posted, and these votes are held (the review is `Held` with
    /// `reason`): not sent, or cast on a patch set that was no longer
    /// current. Those of `drop_only` may only be dropped.
    VotesHeld {
        votes: Votes,
        drop_only: Vec<String>,
        reason: String,
    },
    /// Posted, but these votes did not apply (the review is `Failed`
    /// with `error` and keeps them).
    VotesFailed { votes: Votes, error: String },
    /// Refused as a whole: nothing was posted. The review is `Failed` and
    /// the comments are held with the server's text.
    Refused { error: String },
}

/// One consolidation post (or a review that needed none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostReport {
    pub local_change: LocalChangeId,
    pub change: ChangeNumber,
    /// The patch set posted to, where the comments and the cast votes are.
    pub patch_set: PatchSetNumber,
    /// The review it sent; `None` for a comment-only post.
    pub review: Option<PendingReviewId>,
    /// Inline comments it carried, the review's message included.
    pub comments: usize,
    /// The votes that were cast, on `patch_set`.
    pub votes: Votes,
    /// Found on the server after an interrupted run, so not posted again.
    pub recovered: bool,
    pub outcome: PostOutcome,
}

/// What a [`Syncer::consolidate`] run did, for the pages to refresh and
/// redirect.
#[derive(Debug, Default)]
pub struct ConsolidateReport {
    /// False when another run on this server was going on (it holds the
    /// lease), so this one did nothing.
    pub ran: bool,
    /// Local changes found on Gerrit for the first time: their synthetic
    /// numbers now redirect to the Gerrit change.
    pub found: Vec<FoundChange>,
    /// Local changes whose Gerrit change is merged or abandoned: held.
    pub closed: Vec<(LocalChangeId, ChangeNumber)>,
    /// Closed local changes whose Gerrit change is open again. What was
    /// held stays held.
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
    /// What ended the run early (offline, the login refused, the store
    /// failing, the lease lost); what the report lists was done. The next
    /// run goes on from there.
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
    /// The Change-Id is not one Gerrit can be asked about (nor one it
    /// would have taken in a push).
    Malformed,
    /// Not decided: the answer may be incomplete. Nothing changes; LC5b
    /// must not read this as "not on Gerrit" when it deletes vanished rows.
    Unknown,
}

/// A run-ending failure, or a problem with one change.
enum Problem {
    Stop(Error),
    Skip(String),
    /// Gerrit said the change is closed: nothing more is sent to it.
    Closed,
}

impl From<Error> for Problem {
    fn from(e: Error) -> Self {
        let ends_run = match &e {
            Error::Api(a) => a.is_retryable() || a.is_unauthorized(),
            Error::Store(_) | Error::LeaseLost => true,
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

/// The run: who it is and what it did.
struct Run<'a> {
    /// The owner token of its lease and claims.
    owner: String,
    me: AccountId,
    options: &'a ConsolidateOptions,
    report: ConsolidateReport,
}

impl Run<'_> {
    fn claimant(&self) -> Claimant<'_> {
        Claimant {
            owner: &self.owner,
            at: Timestamp::now(),
        }
    }
}

/// The state a review gets after its post, with what goes with it.
struct Settled {
    state: PendingReviewState,
    note: Option<String>,
    /// The review's votes that stay unsent (or, held, that landed on a
    /// patch set that is not current); the others are removed.
    keep: Votes,
    drop_only: BTreeSet<String>,
    outcome: PostOutcome,
}

impl Settled {
    fn held(votes: Votes, drop_only: BTreeSet<String>, reason: String) -> Self {
        Self {
            state: PendingReviewState::Held,
            note: Some(reason.clone()),
            keep: votes.clone(),
            outcome: PostOutcome::VotesHeld {
                votes,
                drop_only: drop_only.iter().cloned().collect(),
                reason,
            },
            drop_only,
        }
    }

    fn failed(votes: Votes, error: String) -> Self {
        Self {
            state: PendingReviewState::Failed,
            note: Some(error.clone()),
            keep: votes.clone(),
            drop_only: BTreeSet::new(),
            outcome: PostOutcome::VotesFailed { votes, error },
        }
    }
}

/// The current patch set as the run knows it, per Gerrit change: from the
/// change's detail, moved on by every answer.
type Current = Option<PatchSetNumber>;

/// A post whose votes may have arrived after a new patch set, from a
/// server that did not send the change along: checked against the change
/// refreshed after the posts.
struct Unverified {
    review: PendingReviewId,
    patch_set: PatchSetNumber,
    tag: String,
    votes: Votes,
}

/// What a local change has that consolidation could act on.
struct RowData {
    versions: Vec<LocalPatchSet>,
    /// Its reviews, in creation order.
    reviews: Vec<PendingReview>,
    /// Its drafts on its local versions (not reserved).
    on_versions: Vec<Draft>,
    /// The drafts reserved for its post.
    reserved: Vec<Draft>,
}

impl RowData {
    /// Something could be sent: a pending review, or a draft that is not
    /// held (on a version, or reserved and not refused).
    fn sendable(&self) -> bool {
        self.reviews
            .iter()
            .any(|r| r.state == PendingReviewState::Pending)
            || self.on_versions.iter().any(movable)
            || self.reserved.iter().any(postable)
    }

    /// What was moved onto the Gerrit change and waits to be sent.
    fn moved_sendable(&self) -> bool {
        self.reviews
            .iter()
            .any(|r| r.state == PendingReviewState::Pending && !r.patch_set.is_local())
            || self.reserved.iter().any(postable)
    }

    /// Something sits on a version that is one of `shas`: an exact match
    /// with data to move.
    fn exact_work(&self, shas: &[&model::RevisionId]) -> bool {
        self.versions
            .iter()
            .filter(|v| shas.contains(&&v.sha))
            .any(|v| {
                self.reviews.iter().any(|r| {
                    r.patch_set == v.number
                        && matches!(
                            r.state,
                            PendingReviewState::Pending
                                | PendingReviewState::Held
                                | PendingReviewState::Failed
                        )
                }) || self
                    .on_versions
                    .iter()
                    .any(|d| d.patch_set == v.number && movable(d))
            })
    }
}

impl Syncer {
    /// The application's name, which starts the tag of every review that
    /// consolidation posts (`<name>-local-<id>-<nonce>`): the app passes
    /// its `APP_NAME`. Tests may leave the default.
    pub fn with_app_name(mut self, name: &str) -> Self {
        self.app_name = name.into();
        self
    }

    /// Other waits for consolidation than [`CLAIM_STALE`] and
    /// [`SLOW_RECHECK`]: for tests and tools that cannot wait (a claim
    /// recovered at once is only safe when no other run can be going on).
    pub fn with_consolidation_timing(mut self, timing: ConsolidationTiming) -> Self {
        self.consolidation_timing = timing;
        self
    }

    /// Sends the review data of local changes that are on Gerrit now (see
    /// the module documentation of `consolidate`). Fully automatic and
    /// idempotent: run it on the poll cadence while signed in and online.
    /// Returns at once, with `ran == false`, while another run on this
    /// server (in any process using this cache) holds the lease. Whatever
    /// ends the run early (a lost connection, a refused login, a failing
    /// store) is in [`ConsolidateReport::stopped`], next to what was done.
    ///
    /// Anonymous sessions have nothing to send and do nothing.
    pub async fn consolidate(&self, options: &ConsolidateOptions) -> ConsolidateReport {
        let mut report = ConsolidateReport::default();
        if !self.client.is_authenticated() {
            report.ran = true;
            return report;
        }
        let owner = format!("{}-{}", std::process::id(), new_nonce());
        match self.renew_lease(&owner) {
            Ok(()) => {}
            Err(Error::LeaseLost) => return report,
            Err(e) => {
                report.stopped = Some(e);
                return report;
            }
        }
        report.ran = true;
        let mut run = Run {
            owner,
            me: AccountId(0),
            options,
            report,
        };
        if let Err(e) = self.consolidate_run(&mut run).await {
            run.report.stopped = Some(e);
        }
        if let Err(e) = self
            .store()
            .end_consolidation_lease(self.server, &run.owner)
        {
            log::warn!("ending the consolidation lease: {e}");
        }
        run.report
    }

    /// Takes or renews the lease for `owner`; [`Error::LeaseLost`] if
    /// another run holds it.
    fn renew_lease(&self, owner: &str) -> Result<(), Error> {
        let now = Timestamp::now();
        let until = now.checked_add(CLAIM_STALE).unwrap_or(now);
        if self
            .store()
            .take_consolidation_lease(self.server, owner, now, until)?
        {
            Ok(())
        } else {
            Err(Error::LeaseLost)
        }
    }

    async fn consolidate_run(&self, run: &mut Run<'_>) -> Result<(), Error> {
        if self
            .store()
            .local_changes_to_consolidate(self.server)?
            .is_empty()
            && self.store().unfinished_local_posts(self.server)?.is_empty()
        {
            return Ok(());
        }
        run.me = self.me().await?;

        let now = Timestamp::now();
        let stale_before = now
            .checked_sub(self.consolidation_timing.claim_stale)
            .unwrap_or(now);
        let recoverable =
            self.store()
                .recoverable_local_posts(self.server, &run.owner, stale_before)?;
        for (change, nonce) in recoverable {
            match self.recover(change, &nonce, stale_before, run).await {
                Ok(()) => {}
                Err(Problem::Stop(e)) => return Err(e),
                Err(Problem::Skip(why)) => run.report.errors.push((change, why)),
                Err(Problem::Closed) => {}
            }
        }
        // Changes with a post still claimed (by a run that may be going on,
        // or one that could not be settled) wait: their drafts are taken.
        let busy: BTreeSet<LocalChangeId> = self
            .store()
            .unfinished_local_posts(self.server)?
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        let rows: Vec<LocalChange> = self
            .store()
            .local_changes_to_consolidate(self.server)?
            .into_iter()
            .filter(|r| !busy.contains(&r.id))
            .collect();
        let (known, to_look_up): (Vec<LocalChange>, Vec<LocalChange>) =
            rows.into_iter().partition(|r| r.gerrit_number.is_some());

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
        let mut found = Vec::new();
        for (row, lookup) in self.look_up(&to_look_up, run).await? {
            match lookup {
                Lookup::Found(n) => found.push((row, n)),
                Lookup::NotFound => {
                    run.report.not_found.push(row.id);
                    self.set_state_if(
                        &row,
                        &[LocalChangeState::Checking],
                        LocalChangeState::Local,
                    )?;
                }
                Lookup::Malformed => {
                    log::info!(
                        "local change {} has Change-Id {:?}, which Gerrit cannot be asked \
                         about; it stays local",
                        row.id.0,
                        row.change_id
                    );
                    self.set_state_if(
                        &row,
                        &[LocalChangeState::Checking],
                        LocalChangeState::Local,
                    )?;
                }
                Lookup::Ambiguous => {
                    run.report.ambiguous.push(row.id);
                    self.set_state_if(
                        &row,
                        &[LocalChangeState::Checking, LocalChangeState::Local],
                        LocalChangeState::Ambiguous,
                    )?;
                }
                Lookup::Unknown => {}
            }
        }
        for (row, number) in found {
            let outcome = match self.refresh_change(number).await {
                Ok(detail) => self.consolidate_found(&row, number, detail, run).await,
                Err(e) => Err(e.into()),
            };
            self.settle_problem(&row, number, outcome, run).await?;
        }
        self.consolidate_known(known, run).await
    }

    /// Records what a change's handling ended with: a skipped change is
    /// noted, a closed one held, a stop returned.
    async fn settle_problem(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        outcome: Result<(), Problem>,
        run: &mut Run<'_>,
    ) -> Result<(), Error> {
        match outcome {
            Ok(()) => Ok(()),
            Err(Problem::Stop(e)) => Err(e),
            Err(Problem::Skip(why)) => {
                run.report.errors.push((row.id, why));
                Ok(())
            }
            Err(Problem::Closed) => {
                // Learn whether it was merged or abandoned.
                let held = match self.refresh_change(number).await {
                    Ok(detail) if detail.summary.status != ChangeStatus::New => {
                        self.hold_closed(row, number, &detail.summary.status, run)
                    }
                    Ok(_) => Ok(()),
                    Err(e) => Err(e.into()),
                };
                match held {
                    Ok(()) | Err(Problem::Closed) => Ok(()),
                    Err(Problem::Stop(e)) => Err(e),
                    Err(Problem::Skip(why)) => {
                        run.report.errors.push((row.id, why));
                        Ok(())
                    }
                }
            }
        }
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
        run: &mut Run<'_>,
    ) -> Result<Vec<(LocalChange, Lookup)>, Error> {
        let mut by_project: BTreeMap<&str, Vec<&LocalChange>> = BTreeMap::new();
        let mut out = Vec::new();
        for row in rows {
            if !is_change_id(&row.change_id) {
                out.push((row.clone(), Lookup::Malformed));
            } else if row.project.contains('"') {
                out.push((row.clone(), Lookup::Unknown));
            } else {
                by_project.entry(&row.project).or_default().push(row);
            }
        }
        for (project, rows) in by_project {
            for batch in rows.chunks(BATCH) {
                let terms: Vec<String> = batch.iter().map(|r| lookup_term(r)).collect();
                let q = format!("({}) project:\"{project}\"", terms.join(" OR "));
                let page = match self
                    .client
                    .query_changes(&ChangeQuery::new(q).limit(LOOKUP_LIMIT))
                    .await
                {
                    Ok(page) => page,
                    Err(e) => {
                        let why = skip_or_stop(e.into())?;
                        run.report
                            .errors
                            .extend(batch.iter().map(|r| (r.id, why.clone())));
                        continue;
                    }
                };
                for row in batch {
                    let found = classify_hits(row, &page.changes, page.more_changes);
                    out.push(((*row).clone(), found));
                }
            }
        }
        Ok(out)
    }

    // ---- known changes ----

    /// What `row` has that consolidation could act on.
    fn row_data(&self, row: &LocalChange) -> Result<RowData, Error> {
        let store = self.store();
        let versions = store.local_patch_sets(self.server, row.id)?;
        let numbers: BTreeSet<PatchSetNumber> = versions.iter().map(|v| v.number).collect();
        let mut on_versions = store.load_drafts(self.server, row.number)?;
        if let Some(g) = row.gerrit_number {
            on_versions.extend(store.load_drafts(self.server, g)?);
        }
        on_versions.retain(|d| numbers.contains(&d.patch_set) && d.reserved_by.is_none());
        Ok(RowData {
            reviews: store.pending_reviews_of_local_change(self.server, row.id)?,
            reserved: store.consolidation_drafts(self.server, row.id)?,
            versions,
            on_versions,
        })
    }

    /// Handles the local changes that have a Gerrit change: asks about
    /// those due in batches, then moves and sends where there is work.
    async fn consolidate_known(
        &self,
        rows: Vec<LocalChange>,
        run: &mut Run<'_>,
    ) -> Result<(), Error> {
        let now = Timestamp::now();
        let slow_before = now
            .checked_sub(self.consolidation_timing.slow_recheck)
            .unwrap_or(now);
        let mut due: Vec<(LocalChange, ChangeNumber, RowData)> = Vec::new();
        for row in rows {
            let Some(number) = row.gerrit_number else {
                continue;
            };
            let data = self.row_data(&row)?;
            let urgent = data.sendable() && row.state != LocalChangeState::Closed;
            let checked = self.store().local_change_checked_at(self.server, row.id)?;
            if urgent || checked.is_none_or(|t| t < slow_before) {
                due.push((row, number, data));
            }
        }
        for batch in due.chunks(BATCH) {
            let numbers: BTreeSet<ChangeNumber> = batch.iter().map(|(_, n, _)| *n).collect();
            let q = numbers
                .iter()
                .map(|n| format!("change:{n}"))
                .collect::<Vec<_>>()
                .join(" OR ");
            let mut query = ChangeQuery::new(q).limit(u32::try_from(numbers.len()).unwrap_or(1));
            query.options = vec![ChangeOption::AllRevisions];
            let changes: HashMap<ChangeNumber, ChangeDetail> =
                match self.client.query_change_details(&query).await {
                    Ok((list, _)) => list.into_iter().map(|d| (d.summary.number, d)).collect(),
                    Err(e) => {
                        let why = skip_or_stop(e.into())?;
                        for (row, ..) in batch {
                            run.report.errors.push((row.id, why.clone()));
                        }
                        continue;
                    }
                };
            let asked: Vec<LocalChangeId> = batch.iter().map(|(r, ..)| r.id).collect();
            self.store()
                .mark_local_changes_checked(self.server, &asked, Timestamp::now())?;
            for (row, number, data) in batch {
                let Some(lite) = changes.get(number) else {
                    run.report.errors.push((
                        row.id,
                        format!("change {number} is not on Gerrit (any more)"),
                    ));
                    continue;
                };
                let outcome = self
                    .consolidate_known_one(row, *number, data, lite, run)
                    .await;
                self.settle_problem(row, *number, outcome, run).await?;
            }
        }
        Ok(())
    }

    /// One known change, given what the batch query said about it.
    async fn consolidate_known_one(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        data: &RowData,
        lite: &ChangeDetail,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        if lite.summary.status != ChangeStatus::New {
            if row.state != LocalChangeState::Closed {
                self.hold_closed(row, number, &lite.summary.status, run)?;
            }
            return Ok(());
        }
        if row.state == LocalChangeState::Closed {
            // Restored: the Gerrit state is open again, but what was held
            // stays held (a user decision).
            self.store()
                .set_local_change_state(self.server, row.id, LocalChangeState::Local)?;
            run.report.reopened.push((row.id, number));
        } else if row.state == LocalChangeState::Checking {
            // Asked about: Gerrit has the change.
            self.store()
                .set_local_change_state(self.server, row.id, LocalChangeState::Local)?;
        }
        let shas: Vec<&model::RevisionId> = lite.revisions.iter().map(|r| &r.sha).collect();
        // A version pushed since: the detail maps it, once.
        let newly_pushed = data
            .versions
            .iter()
            .any(|v| v.gerrit_patch_set.is_none() && shas.contains(&&v.sha));
        if newly_pushed || data.exact_work(&shas) || data.moved_sendable() {
            let detail = self.refresh_change(number).await?;
            if detail.summary.status != ChangeStatus::New {
                return self.hold_closed(row, number, &detail.summary.status, run);
            }
            self.consolidate_open(row, number, detail, run).await
        } else {
            self.finish_change(row, number, run)
        }
    }

    /// A change found by the lookup, with its fresh detail.
    async fn consolidate_found(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: ChangeDetail,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        run.report.found.push(found(row, number));
        if detail.summary.status != ChangeStatus::New {
            return self.hold_closed(row, number, &detail.summary.status, run);
        }
        self.consolidate_open(row, number, detail, run).await
    }

    /// Holds everything of a change found merged or abandoned.
    fn hold_closed(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        status: &ChangeStatus,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let reason = match status {
            ChangeStatus::Merged => "change merged".to_owned(),
            ChangeStatus::Abandoned => "change abandoned".to_owned(),
            other => format!("change {}", other.to_string().to_lowercase()),
        };
        match self
            .store()
            .hold_closed_local_change(self.server, row.id, number, &reason)
        {
            Ok(_) => {
                run.report.closed.push((row.id, number));
                Ok(())
            }
            Err(e @ (store::Error::LocalTarget | store::Error::OtherGerritChange(_))) => {
                Err(Problem::Skip(e.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    // ---- one open change ----

    /// Moves the exact versions of `row` onto `number` and sends.
    async fn consolidate_open(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: ChangeDetail,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
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
        let (moves, all_on_gerrit) = self.exact_moves(&row, &detail)?;
        // Looked up: local now. A vanished row keeps its state for LC5b.
        let settled = (row.state == LocalChangeState::Checking).then_some(LocalChangeState::Local);
        let (state, else_state) = if all_on_gerrit {
            (Some(LocalChangeState::Consolidated), settled)
        } else {
            (settled, None)
        };
        self.rewrite(&row, number, moves, state, else_state, Vec::new())?;
        let posted_before = run.report.posts.len();
        let mut unverified = Vec::new();
        let sent = self.send(&row, number, &detail, &mut unverified, run).await;
        let posted = run.report.posts[posted_before..]
            .iter()
            .any(|p| !matches!(p.outcome, PostOutcome::Refused { .. }));
        if posted && let Some(fresh) = self.refresh_after_posts(number).await {
            self.verify_votes(&row, number, &fresh, unverified, run)?;
        }
        sent?;
        self.finish_change(&row, number, run)
    }

    fn rewrite(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        moves: Vec<PatchSetMove>,
        state: Option<LocalChangeState>,
        else_state: Option<LocalChangeState>,
        review_states: Vec<ReviewStateChange>,
    ) -> Result<(), Problem> {
        let rewrite = ConsolidationRewrite {
            local_change: row.id,
            gerrit_number: number,
            moves,
            keep: Vec::new(),
            review_states,
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
        detail: &ChangeDetail,
    ) -> Result<(Vec<PatchSetMove>, bool), Problem> {
        let data = self.row_data(row)?;
        let mut moves = Vec::new();
        let mut all_on_gerrit = true;
        for v in &data.versions {
            let Some(target) = detail
                .revisions
                .iter()
                .find(|r| r.sha == v.sha && !r.number.is_local())
            else {
                all_on_gerrit = false;
                continue;
            };
            let drafts: Vec<(LocalDraftId, Option<store::DraftAnchor>)> = data
                .on_versions
                .iter()
                .filter(|d| d.patch_set == v.number && movable(d))
                .map(|d| (d.local_id, None))
                .collect();
            let reviews: Vec<PendingReviewId> = data
                .reviews
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
                drafts,
                reviews,
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
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let mut store = self.store();
        let Some(now) = store.local_change(self.server, row.id)? else {
            return Ok(());
        };
        if now.state == LocalChangeState::Consolidated {
            run.report.consolidated.push((row.id, number));
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
            run.report.consolidated.push((row.id, number));
        }
        Ok(())
    }

    /// Brings the cached change and comments up to date after posts, so
    /// the pages show what was posted; returns the change as it is now.
    /// Failures are only logged: the posts are done.
    async fn refresh_after_posts(&self, number: ChangeNumber) -> Option<ChangeDetail> {
        let detail = match self.refresh_change(number).await {
            Ok(d) => Some(d),
            Err(e) => {
                log::warn!("refreshing change {number} after consolidation: {e}");
                None
            }
        };
        if let Err(e) = self.refresh_comments(number).await {
            log::warn!("refreshing comments of change {number} after consolidation: {e}");
        }
        detail
    }

    /// For posts whose answer did not say the current patch set: if the
    /// refreshed change shows a newer patch set uploaded before our
    /// message, the votes landed on the old one. Those reviews are held
    /// then, approvals drop-only.
    fn verify_votes(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        fresh: &ChangeDetail,
        unverified: Vec<Unverified>,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let mut changes = Vec::new();
        for u in unverified {
            let Some(current) = fresh.current_patch_set else {
                continue;
            };
            if current == u.patch_set {
                continue;
            }
            let Some(ours) = fresh
                .messages
                .iter()
                .find(|m| m.tag.as_deref() == Some(u.tag.as_str()))
            else {
                continue;
            };
            let newer_first = fresh
                .revisions
                .iter()
                .filter(|r| r.number > u.patch_set)
                .any(|r| r.created.is_some_and(|c| c <= ours.date));
            if !newer_first {
                continue;
            }
            let reason = landed_reason(u.patch_set, current, &u.votes);
            let drop_only: BTreeSet<String> = u
                .votes
                .iter()
                .filter(|(l, v)| is_approval(fresh, l, *v))
                .map(|(l, _)| l.clone())
                .collect();
            let mut change = ReviewStateChange::new(
                u.review,
                &[PendingReviewState::Sent],
                PendingReviewState::Held,
                Some(reason.clone()),
            );
            change.labels = Some(u.votes.iter().cloned().collect());
            change.drop_only = drop_only.clone();
            changes.push(change);
            if let Some(p) = run
                .report
                .posts
                .iter_mut()
                .rev()
                .find(|p| p.review == Some(u.review))
            {
                p.outcome = PostOutcome::VotesHeld {
                    votes: u.votes.clone(),
                    drop_only: drop_only.into_iter().collect(),
                    reason,
                };
            }
        }
        if !changes.is_empty() {
            self.rewrite(row, number, Vec::new(), None, None, changes)?;
        }
        Ok(())
    }

    // ---- sending ----

    /// Sends the moved pending reviews of `row` and its reserved comments,
    /// per revision.
    async fn send(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        unverified: &mut Vec<Unverified>,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let mut current: Current = detail.current_patch_set;
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
        self.supersede(row, number, &reviews, current)?;
        let mut revisions: BTreeSet<PatchSetNumber> = reviews.iter().map(|r| r.patch_set).collect();
        revisions.extend(
            self.postable_comments(row.id, None)?
                .iter()
                .map(|d| d.patch_set),
        );
        for ps in revisions {
            let Some(rev) = detail.revision(ps) else {
                run.report
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
                    ps,
                    &nonce,
                    &run.claimant(),
                )?;
                if let Some(post) = claimed {
                    self.post_claimed(
                        row,
                        number,
                        detail,
                        rev,
                        post,
                        &nonce,
                        &mut current,
                        unverified,
                        run,
                    )
                    .await?;
                }
            }
            if !self.postable_comments(row.id, Some(ps))?.is_empty() {
                let nonce = new_nonce();
                let claimed = self.store().claim_local_post(
                    self.server,
                    row.id,
                    None,
                    ps,
                    &nonce,
                    &run.claimant(),
                )?;
                if let Some(post) = claimed {
                    self.post_claimed(
                        row,
                        number,
                        detail,
                        rev,
                        post,
                        &nonce,
                        &mut current,
                        unverified,
                        run,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    /// The reserved comments of `change` that a post would carry (on `ps`,
    /// if given): not review messages, not held.
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

    /// A newer review whose votes will be cast (its patch set is current)
    /// supersedes the vote of every older unsent review of the change on
    /// the same label: the older one loses it before anything is sent. A
    /// newer review whose votes will be held supersedes nothing, so an
    /// older vote meant for the current patch set is not lost to it.
    fn supersede(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        sending: &[PendingReview],
        current: Current,
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
        for newer in sending.iter().filter(|r| Some(r.patch_set) == current) {
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
    /// documentation for the outcomes). `current` is the change's current
    /// patch set as the run knows it; answers move it on.
    #[allow(clippy::too_many_arguments)]
    async fn post_claimed(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        rev: &Revision,
        post: LocalPost,
        nonce: &str,
        current: &mut Current,
        unverified: &mut Vec<Unverified>,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let review = post.review.as_ref();
        let parts = Parts::of(&post, rev.number);
        let votes: BTreeMap<String, i32> = review.map(|r| r.labels.clone()).unwrap_or_default();
        let castable = *current == Some(rev.number);
        let (send_votes, held) = if castable || votes.is_empty() {
            (votes, None)
        } else {
            let held: Votes = votes.into_iter().collect();
            let drop_only = drop_only_labels(row, detail, review, &held, run.options);
            let reason = held_reason(rev.number, *current, &held, &drop_only);
            (
                BTreeMap::new(),
                Some(Settled::held(held, drop_only, reason)),
            )
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
        if parts.comments.is_empty() && send_votes.is_empty() {
            // Nothing to post: a review whose votes were all superseded or
            // are held, without a message.
            if let Some(s) = &held {
                entry.outcome = s.outcome.clone();
            }
            self.settle(row, &post, nonce, &parts.posted, held)?;
            if review.is_some() {
                run.report.posts.push(entry);
            }
            return Ok(());
        }
        let tag = self.local_tag(row.id, nonce);
        let input = ReviewInput {
            labels: send_votes,
            comments: parts.comments.iter().map(|d| d.input()).collect(),
            drafts: DraftHandling::Keep,
            tag: Some(tag.clone()),
            notify: None,
            ignore_automatic_attention_set_rules: true,
            ..ReviewInput::default()
        };
        let checked = self
            .ensure_on_server(&rev.sha)
            .and_then(|()| self.renew_lease(&run.owner));
        if let Err(e) = checked {
            self.give_up(row, &post, nonce, PendingReviewState::Pending, None, None)?;
            return Err(e.into());
        }
        let sent_votes: Votes = input.labels.clone().into_iter().collect();
        let answer = match self.client.post_review(number, &rev.sha, &input).await {
            Err(e) if is_closed_answer(&e) => {
                self.give_up(row, &post, nonce, PendingReviewState::Pending, None, None)?;
                return Err(Problem::Closed);
            }
            Err(e) if !input.labels.is_empty() && is_vote_refusal(&e) => {
                // Votes never sink comments: the same post without them,
                // under the same claim and tag.
                let failed = Settled::failed(sent_votes.clone(), refusal_text(&e));
                let bare = ReviewInput {
                    labels: BTreeMap::new(),
                    ..input.clone()
                };
                if bare.comments.is_empty() {
                    Ok((Some(failed), None))
                } else {
                    match self.client.post_review(number, &rev.sha, &bare).await {
                        Ok(result) => Ok((Some(failed), result.current_patch_set)),
                        Err(e) => Err(e),
                    }
                }
            }
            Ok(result) => {
                let settled = votes_after(detail, &input, &result, rev.number);
                if settled.is_none()
                    && !input.labels.is_empty()
                    && result.current_patch_set.is_none()
                    && let Some(r) = review
                {
                    unverified.push(Unverified {
                        review: r.id,
                        patch_set: rev.number,
                        tag: tag.clone(),
                        votes: sent_votes.clone(),
                    });
                }
                Ok((settled.or(held), result.current_patch_set))
            }
            Err(e) => Err(e),
        };
        match answer {
            Ok((settled, now_current)) => {
                if now_current.is_some() {
                    *current = now_current;
                }
                entry.votes = match &settled {
                    Some(s) if s.state == PendingReviewState::Failed => sent_votes
                        .iter()
                        .filter(|v| !s.keep.contains(v))
                        .cloned()
                        .collect(),
                    _ => sent_votes,
                };
                if let Some(s) = &settled {
                    entry.outcome = s.outcome.clone();
                }
                self.settle(row, &post, nonce, &parts.posted, settled)?;
                run.report.posts.push(entry);
                Ok(())
            }
            Err(e) => {
                if is_closed_answer(&e) {
                    self.give_up(row, &post, nonce, PendingReviewState::Pending, None, None)?;
                    return Err(Problem::Closed);
                }
                let refused = self.refused(row, &post, nonce, e)?;
                entry.outcome = PostOutcome::Refused { error: refused };
                run.report.posts.push(entry);
                Ok(())
            }
        }
    }

    /// Gives up a claim that did not reach the server, in one store
    /// transaction: the review to `to` (with `note`), the post's comments
    /// held with `hold` if given, the claim cleared.
    fn give_up(
        &self,
        row: &LocalChange,
        post: &LocalPost,
        nonce: &str,
        to: PendingReviewState,
        note: Option<String>,
        hold: Option<&str>,
    ) -> Result<(), Problem> {
        let review = post
            .review
            .as_ref()
            .map(|r| ReviewStateChange::new(r.id, &[PendingReviewState::Sending], to, note));
        self.store()
            .abandon_local_post(self.server, row.id, nonce, review.as_ref(), hold)?;
        Ok(())
    }

    /// Records a post that reached the server, in one store transaction:
    /// removes what it posted and sets the review `Sent`, or as `settled`
    /// says (with the votes it keeps).
    fn settle(
        &self,
        row: &LocalChange,
        post: &LocalPost,
        nonce: &str,
        posted: &[(LocalDraftId, u32)],
        settled: Option<Settled>,
    ) -> Result<(), Problem> {
        let states: Vec<ReviewStateChange> = post
            .review
            .iter()
            .map(|r| {
                let mut change = ReviewStateChange::new(
                    r.id,
                    &[PendingReviewState::Sending],
                    settled
                        .as_ref()
                        .map_or(PendingReviewState::Sent, |s| s.state),
                    settled.as_ref().and_then(|s| s.note.clone()),
                );
                if let Some(s) = &settled {
                    change.labels = Some(s.keep.iter().cloned().collect());
                    change.drop_only = s.drop_only.clone();
                }
                change
            })
            .collect();
        self.store()
            .finish_local_post(self.server, row.id, nonce, posted, &states)?;
        Ok(())
    }

    /// Handles a post that failed with `e`. A refusal gives the claim up:
    /// the review becomes `Failed` and its comments are held with the
    /// server's text (a refused login puts the review back to `Pending` and
    /// ends the run); returns the server's text. Anything else may have
    /// reached the server, so the claim stays for recovery and the run
    /// ends.
    fn refused(
        &self,
        row: &LocalChange,
        post: &LocalPost,
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
        if e.is_unauthorized() {
            self.give_up(row, post, nonce, PendingReviewState::Pending, None, None)?;
            return Err(Problem::Stop(e.into()));
        }
        let text = refusal_text(&e);
        let hold = format!("Gerrit refused the post: {text}");
        self.give_up(
            row,
            post,
            nonce,
            PendingReviewState::Failed,
            Some(text.clone()),
            Some(&hold),
        )?;
        Ok(text)
    }

    fn local_tag(&self, change: LocalChangeId, nonce: &str) -> String {
        format!("{}-local-{}-{nonce}", self.app_name, change.0)
    }

    // ---- recovery ----

    /// Settles a post claimed under `nonce` by a run that is gone: takes
    /// it over, then finishes it if the change has a message by the
    /// signed-in account with its tag, else posts it again under the same
    /// nonce.
    async fn recover(
        &self,
        change: LocalChangeId,
        nonce: &str,
        stale_before: Timestamp,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let (row, post) = {
            let mut store = self.store();
            let Some(row) = store.local_change(self.server, change)? else {
                return Ok(());
            };
            let by = run.claimant();
            let Some(post) =
                store.adopt_local_post(self.server, change, nonce, &by, stale_before)?
            else {
                return Ok(());
            };
            (row, post)
        };
        let Some(number) = row.gerrit_number else {
            // Never posted: nothing is claimed without a Gerrit change.
            return self.give_up(&row, &post, nonce, PendingReviewState::Pending, None, None);
        };
        let detail = self.refresh_change(number).await?;
        let tag = self.local_tag(change, nonce);
        let review_ps = post.review.as_ref().map(|r| r.patch_set);
        let me = run.me;
        let message = detail.messages.iter().find(|m| {
            m.tag.as_deref() == Some(tag.as_str())
                && (m.author.as_ref().is_some_and(|a| a.id == me)
                    || m.real_author.as_ref().is_some_and(|a| a.id == me))
                && (review_ps.is_none() || m.revision.is_none() || m.revision == review_ps)
        });
        let ps = review_ps
            .or_else(|| message.and_then(|m| m.revision))
            .or_else(|| post.drafts.iter().map(|d| d.patch_set).min());
        let Some(rev) = ps.and_then(|ps| detail.revision(ps)) else {
            return Err(Problem::Skip(format!(
                "the interrupted post's patch set is not on change {number}"
            )));
        };
        let Some(message) = message else {
            if detail.summary.status != ChangeStatus::New {
                // Closed meanwhile: nothing is sent automatically any more.
                self.give_up(&row, &post, nonce, PendingReviewState::Pending, None, None)?;
                return Err(Problem::Closed);
            }
            let mut current = detail.current_patch_set;
            let mut unverified = Vec::new();
            self.post_claimed(
                &row,
                number,
                &detail,
                rev,
                post,
                nonce,
                &mut current,
                &mut unverified,
                run,
            )
            .await?;
            return self.finish_change(&row, number, run);
        };
        // It arrived. The votes it cast are in its text ("Patch Set 2:
        // Code-Review+1"): the change's votes cover only the current patch
        // set. Votes not there were not cast: held for a patch set that is
        // not current, or not applied.
        let parts = Parts::of(&post, rev.number);
        let cast = votes_in_message(&message.message);
        let votes: Votes = post
            .review
            .as_ref()
            .map(|r| r.labels.clone().into_iter().collect())
            .unwrap_or_default();
        let (applied, missing): (Votes, Votes) = votes
            .into_iter()
            .partition(|(label, value)| cast.get(label) == Some(value));
        let current = detail.current_patch_set;
        let settled = if !missing.is_empty() && current == Some(rev.number) {
            let error = format!(
                "the review was posted, but its message does not show {}",
                describe_votes(&missing)
            );
            Some(Settled::failed(missing, error))
        } else if !missing.is_empty() {
            let drop_only =
                drop_only_labels(&row, &detail, post.review.as_ref(), &missing, run.options);
            let reason = held_reason(rev.number, current, &missing, &drop_only);
            Some(Settled::held(missing, drop_only, reason))
        } else if !applied.is_empty() && current.is_some_and(|c| c != rev.number) {
            // Cast, on a patch set that is not current now; whether it
            // was when they arrived, the message's date tells.
            let newer_first = detail
                .revisions
                .iter()
                .filter(|r| r.number > rev.number)
                .any(|r| r.created.is_some_and(|c| c <= message.date));
            newer_first.then(|| {
                let drop_only: BTreeSet<String> = applied
                    .iter()
                    .filter(|(l, v)| is_approval(&detail, l, *v))
                    .map(|(l, _)| l.clone())
                    .collect();
                let reason = landed_reason(rev.number, current.unwrap_or(rev.number), &applied);
                Settled::held(applied.clone(), drop_only, reason)
            })
        } else {
            None
        };
        let outcome = settled
            .as_ref()
            .map_or(PostOutcome::Sent, |s| s.outcome.clone());
        self.settle(&row, &post, nonce, &parts.posted, settled)?;
        run.report.posts.push(PostReport {
            local_change: change,
            change: number,
            patch_set: rev.number,
            review: post.review.as_ref().map(|r| r.id),
            comments: parts.comments.len(),
            votes: applied,
            recovered: true,
            outcome,
        });
        self.finish_change(&row, number, run)
    }
}

/// What a claimed post sends to one revision.
struct Parts<'a> {
    /// The inline comments: the reserved comments on the revision that
    /// are not held, and the review's message.
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
/// message, not deleted, not held, not refused before (by an older build).
fn postable(d: &Draft) -> bool {
    d.pending_review.is_none()
        && d.held_reason.is_none()
        && d.is_visible()
        && !matches!(d.sync, DraftSync::Failed { .. })
}

/// Whether a draft on a local version moves with it: not a review's
/// message, not deleted, not held.
fn movable(d: &Draft) -> bool {
    d.is_visible() && !d.is_sealed() && d.reserved_by.is_none() && d.held_reason.is_none()
}

/// The text of a failure that skips what it is about, or the failure if
/// it ends the run.
fn skip_or_stop(e: Error) -> Result<String, Error> {
    match Problem::from(e) {
        Problem::Stop(e) => Err(e),
        Problem::Skip(why) => Ok(why),
        Problem::Closed => Ok("the change is closed".to_owned()),
    }
}

/// What happened to the votes of a post that the server took: `None` when
/// all applied. Votes that arrived after their patch set stopped being
/// current were cast there, where they do not count: held, approvals
/// drop-only. Votes missing from the answer were dropped (an unknown label,
/// a value the label does not have: Gerrit answers 200 for those): failed.
fn votes_after(
    detail: &ChangeDetail,
    input: &ReviewInput,
    result: &ReviewResult,
    ps: PatchSetNumber,
) -> Option<Settled> {
    if input.labels.is_empty() {
        return None;
    }
    if let Some(current) = result.current_patch_set
        && current != ps
    {
        let cast: Votes = input
            .labels
            .iter()
            .filter(|(l, v)| result.labels.get(*l) == Some(*v))
            .map(|(l, v)| (l.clone(), *v))
            .collect();
        let drop_only: BTreeSet<String> = cast
            .iter()
            .filter(|(l, v)| is_approval(detail, l, *v))
            .map(|(l, _)| l.clone())
            .collect();
        let reason = landed_reason(ps, current, &cast);
        return Some(Settled::held(cast, drop_only, reason));
    }
    let dropped = input.unapplied_labels(result);
    if dropped.is_empty() {
        return None;
    }
    let error = format!("Gerrit did not apply {}", describe_votes(&dropped));
    Some(Settled::failed(dropped, error))
}

/// Why votes cast on `ps` after `current` became current are held.
fn landed_reason(ps: PatchSetNumber, current: PatchSetNumber, votes: &[(String, i32)]) -> String {
    format!(
        "{} {} cast on PS {}, which was no longer current (PS {} is), so {} only there. \
         An approval cannot be moved; other votes can be published onto the current patch \
         set.",
        describe_votes(votes),
        if votes.len() == 1 { "was" } else { "were" },
        ps.0,
        current.0,
        if votes.len() == 1 {
            "it counts"
        } else {
            "they count"
        },
    )
}

/// Whether the server refused a post because of its votes: a 400, 403 or
/// 409 that names a label or a vote. Seen on the dev Gerrit (3.14):
/// 403 `Applying label "Code-Review": 2 is restricted`. Gerrit accepts votes
/// on a patch set that is not current, so no refusal says that (it is
/// caught from the answer's current patch set instead, see
/// [`votes_after`]); "not current" is still read as a vote problem, should
/// a server say it. A closed change's answer is not a vote problem (see
/// [`is_closed_answer`]).
fn is_vote_refusal(e: &ApiError) -> bool {
    let message = match e {
        ApiError::BadRequest { message }
        | ApiError::Forbidden { message }
        | ApiError::Conflict { message } => message.to_lowercase(),
        _ => return false,
    };
    message.contains("label") || message.contains("vote") || message.contains("not current")
}

/// Whether the server refused a post because the change is closed: a 409
/// naming "closed" (3.14: `change is closed` for votes on an abandoned
/// change, `Cannot reduce vote on labels for closed change: Code-Review`
/// on a merged one). Nothing more is sent then; the change is held.
fn is_closed_answer(e: &ApiError) -> bool {
    matches!(e, ApiError::Conflict { message } if message.to_lowercase().contains("closed"))
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

/// The labels of `votes` that may only be dropped when held: approvals
/// under the strict rule (a local patch set of a Gerrit change, or votes
/// given after the change was created) or under `exact` (the stricter of
/// the review's snapshot and the project's setting now).
fn drop_only_labels(
    row: &LocalChange,
    detail: &ChangeDetail,
    review: Option<&PendingReview>,
    votes: &[(String, i32)],
    options: &ConsolidateOptions,
) -> BTreeSet<String> {
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
    votes
        .iter()
        .filter(|(l, v)| drop_only && is_approval(detail, l, *v))
        .map(|(l, _)| l.clone())
        .collect()
}

/// Why the votes of a review on non-current patch set `ps` are held, and
/// what the user may do with them (the structured form is `drop_only`).
fn held_reason(
    ps: PatchSetNumber,
    current: Current,
    votes: &[(String, i32)],
    drop_only: &BTreeSet<String>,
) -> String {
    let (fixed, others): (Votes, Votes) = votes
        .iter()
        .cloned()
        .partition(|(l, _)| drop_only.contains(l));
    let current = current.map_or_else(|| "another".to_owned(), |c| format!("PS {}", c.0));
    let mut reason = format!(
        "PS {} is not the current patch set on Gerrit ({current} is), so these votes were not sent.",
        ps.0
    );
    if !fixed.is_empty() {
        reason.push_str(&format!(
            " {}: an approval goes only onto the patch set it was given on while that is \
             current, so it can only be dropped.",
            describe_votes(&fixed)
        ));
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

/// The votes a review's change message records on its first line, as
/// Gerrit writes it: "Patch Set 2: Code-Review+2 Verified-1" (a vote
/// removed reads "-Code-Review", that is 0).
fn votes_in_message(message: &str) -> BTreeMap<String, i32> {
    let first = message.lines().next().unwrap_or_default();
    let Some((head, votes)) = first.split_once(':') else {
        return BTreeMap::new();
    };
    if !head.trim_start().starts_with("Patch Set") {
        return BTreeMap::new();
    }
    let mut out = BTreeMap::new();
    for word in votes.split_whitespace() {
        if let Some(label) = word.strip_prefix('-')
            && !label.is_empty()
            && !label.starts_with(|c: char| c.is_ascii_digit())
        {
            out.insert(label.to_owned(), 0);
            continue;
        }
        let Some(at) = word.rfind(['+', '-']) else {
            continue;
        };
        let (label, value) = word.split_at(at);
        if label.is_empty() {
            continue;
        }
        if let Ok(v) = value.trim_start_matches('+').parse::<i32>() {
            out.insert(label.to_owned(), v);
        }
    }
    out
}

/// A Change-Id Gerrit can be asked about without matching anything else:
/// `I` and 40 hex digits (a shorter one would be a prefix search, and
/// anything else makes Gerrit refuse the whole query: "Invalid change
/// format").
fn is_change_id(id: &str) -> bool {
    id.len() == 41
        && id.starts_with('I')
        && id[1..]
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Percent-encodes a part of a `project~branch~Change-Id` triplet: Gerrit
/// URL-decodes each part, so a `%` or `+` must be encoded to survive (and
/// a `~` or `"` cannot break the term).
fn triplet_part(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The lookup term of a row: `change:"project~branch~Id"` with a known
/// branch, else `change:Id`.
fn lookup_term(row: &LocalChange) -> String {
    match &row.branch {
        Some(branch) => format!(
            "change:\"{}~{}~{}\"",
            triplet_part(&row.project),
            triplet_part(branch),
            row.change_id
        ),
        None => format!("change:{}", row.change_id),
    }
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

/// A fresh nonce: unique per process run and over runs (the time is part
/// of it), which is all a tag or an owner token needs.
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
