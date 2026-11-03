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
//! `gert`, a new session after a re-login); within one process a run also
//! marks its server as running, so a second run there returns at once.
//! Every claim of a post records the run's owner token and the time, and
//! is on disk before the post goes out. Every post has a deadline
//! ([`POST_DEADLINE`], 2 minutes): when it passes, the request is dropped
//! (which aborts it), the claim stays and the run ends; the next run's
//! recovery checks the tag. A run recovers only the posts of other runs
//! that are older than [`CLAIM_STALE`] (15 minutes, well above the
//! deadline and Gerrit's processing time), so a request still in flight
//! is never posted a second time; the lease lasts as long, so a run held
//! up that long loses it and stops before posting again
//! ([`Error::LeaseLost`]).
//!
//! The deadline is a monotonic timer, which stops while the machine
//! sleeps, while the lease and claims use the wall clock. So a run must
//! never start on a server while that server's previous run is still
//! alive in the same process (it may be asleep in a post whose deadline
//! has not passed in its own time): [`Syncer::consolidate`] enforces this
//! per store and server. Another process waking from the same sleep can
//! still adopt a claim whose request is being completed; that residual
//! risk is accepted (and the tag check catches a post that did arrive).
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
//!    known, else by Change-Id. Not found: `Checking` becomes `Local`,
//!    and a `Vanished` row (gone from the checkout, kept for its unsent
//!    data) is deleted with its local data and logged (the user decision
//!    O1), but only on a definite answer: the query succeeded, the
//!    Change-Id was well-formed and the answer was not cut off. A failed
//!    query or an undecided answer changes nothing.
//!    Several hits with an unknown branch: `Ambiguous`, never
//!    consolidated. A Change-Id Gerrit cannot be asked about (not `I` and
//!    40 hex digits) is `Local` at once; a vanished change with one is
//!    deleted (O1: Gerrit refuses such a Change-Id in a push, so it never
//!    gets there). A vanished change holding data written under another
//!    account is never deleted: that account's private change is hidden
//!    from this one.
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
//!    the post. In the same transaction, a version that is not a Gerrit
//!    patch set and never will be (a version seen later is one, the
//!    version is on no named branch in the last complete scan, or the
//!    change vanished from the checkout) is *ported*: through a
//!    [`PortSource`] (the checkout), each of its drafts moves onto the
//!    current patch set where its lines are unchanged (onto the newest
//!    patch set in the checkout when the current one is not there), or is
//!    held with the reason (see the `port` module); its pending reviews go
//!    along. With no patch set in the checkout, or no checkout configured
//!    for the project, everything is held. A newer version that is not on
//!    Gerrit yet waits for its own patch set. Without a port source nothing
//!    is ported or held; a checkout that cannot be used now (or `git`
//!    failing) leaves the versions for a later try, after the slow
//!    re-check.
//! 6. **Send.** Per revision, the pending reviews in creation order, each
//!    in one `POST …/review`: the reserved comments on that revision inline
//!    (`comments`; the first post of a revision takes them all), the
//!    review's message (its sealed patch-set-level draft, posted as a
//!    `/PATCHSET_LEVEL` comment with its `unresolved` flag, as an ordinary
//!    reply posts it), the votes the rules allow, the tag
//!    `<app>-local-<local change id>-r<review id>-<nonce>` (a comment-only
//!    post leaves out `-r<review id>`),
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
//! A review ported from a version that never reached Gerrit (its votes
//! were given on another commit than the one it is posted to) follows the
//! rules by case. Unless its votes are purely local ([`purely_local`]:
//! when they were given, the change was not on Gerrit, or Gerrit could
//! not be reached), the strict rule holds: nothing is cast, approvals are
//! held drop-only and other votes publishable. A purely local change takes the
//! stricter of the review's `local_approval` snapshot and the project's
//! setting now (`exact` without an entry): under `follow` every vote is
//! cast while the target patch set is current (it is in the checkout, or
//! nothing was ported), also when some comments were held; otherwise all
//! are held, publishable. Under `exact-or-manual` all are held,
//! publishable; under `exact` approvals are held drop-only and other votes
//! publishable. Votes are cast all together or not at all.
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
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gerrit_api::{ChangeOption, ChangeQuery, Error as ApiError};
use jiff::{SignedDuration, Timestamp};
use model::{
    AccountId, ChangeDetail, ChangeNumber, ChangeStatus, ChangeSummary, Draft, DraftHandling,
    DraftSync, LocalApproval, LocalChange, LocalChangeId, LocalChangeState, LocalDraftId,
    LocalPatchSet, PatchSetNumber, PendingReview, PendingReviewId, PendingReviewState, ReviewInput,
    ReviewResult, Revision, RevisionId, Side,
};
use store::{Claimant, ConsolidationRewrite, LocalPost, PatchSetMove, ReviewStateChange};

use super::Syncer;
use crate::{Error, HasCheckout, PortCommit, PortError, PortSource};

mod held;
mod lookup;
mod port;
mod post;
mod recover;
mod votes;

pub use held::{HeldItem, HeldPublished, held_place};
use post::{
    Parts, foreign_reason, is_closed_answer, is_vote_refusal, movable, new_nonce, postable,
    refusal_text, skip_or_stop, tagged_by,
};
use votes::{
    casts, describe_votes, drop_only_labels, drop_only_now, is_approval, landed_reason,
    vote_actions, votes_after, votes_held_reason, votes_in_message,
};
pub use votes::{held_vote_drop_only, purely_local};

/// Votes as label and value, sorted by label.
type Votes = Vec<(String, i32)>;

/// How many changes one query names.
const BATCH: usize = 50;
/// The most results one lookup asks for (Gerrit's default query limit).
const LOOKUP_LIMIT: u32 = 500;
/// How long the consolidation lease lasts without renewal, and how old a
/// claim of another run must be before it is recovered (by default): far
/// beyond a post's life, which [`POST_DEADLINE`] bounds (in awake time),
/// plus Gerrit's time to process it (seconds).
pub const CLAIM_STALE: SignedDuration = SignedDuration::from_mins(15);
/// How long a consolidation post may take before it is given up (and the
/// request aborted), by default.
pub const POST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
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
    /// How long a post may take.
    pub post_deadline: std::time::Duration,
}

impl Default for ConsolidationTiming {
    fn default() -> Self {
        Self {
            claim_stale: CLAIM_STALE,
            slow_recheck: SLOW_RECHECK,
            post_deadline: POST_DEADLINE,
        }
    }
}

/// The servers (per store) with a consolidation run going on in this
/// process.
static RUNNING: std::sync::Mutex<Vec<(usize, i64)>> = std::sync::Mutex::new(Vec::new());

/// Marks a store's server as running a consolidation until dropped.
struct Running((usize, i64));

impl Running {
    /// `None` if a run on this store and server is going on already.
    fn start(key: (usize, i64)) -> Option<Self> {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        if running.contains(&key) {
            return None;
        }
        running.push(key);
        Some(Self(key))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        running.retain(|k| *k != self.0);
    }
}

/// Asks a consolidation run to stop: set from another thread (the user
/// signed out or another account signed in), checked before every post.
/// A post already sent is completed; the run ends before the next one with
/// [`Error::Stopped`]. Clones share the flag; two flags are equal when they
/// are the same flag.
#[derive(Clone, Debug, Default)]
pub struct StopFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl StopFlag {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl PartialEq for StopFlag {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for StopFlag {}

/// What [`Syncer::consolidate`] takes besides the store.
#[derive(Clone, Default)]
pub struct ConsolidateOptions {
    /// The `local_approval` setting of each project as it is now. A
    /// project that is not listed counts as `exact`, the strictest.
    pub local_approval: BTreeMap<String, LocalApproval>,
    /// Stops the run before its next post (see [`StopFlag`]).
    pub stop: StopFlag,
    /// The checkouts, for porting the comments of versions that never
    /// reached Gerrit onto a Gerrit patch set. Without it nothing is
    /// ported: those versions stay as they are, and nothing is held for
    /// them.
    pub port: Option<Arc<dyn PortSource>>,
}

impl std::fmt::Debug for ConsolidateOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsolidateOptions")
            .field("local_approval", &self.local_approval)
            .field("stop", &self.stop)
            .field("port", &self.port.is_some())
            .finish()
    }
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

/// What a run did with a local version that never reached Gerrit: its
/// comments and pending reviews ported onto a Gerrit patch set, or held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ported {
    pub local_change: LocalChangeId,
    pub change: ChangeNumber,
    /// The local version.
    pub from: PatchSetNumber,
    /// Where its comments and reviews went; `None` when no patch set of
    /// the change is in the checkout, and everything was held.
    pub to: Option<PatchSetNumber>,
    /// Drafts moved onto `to`, to be posted with the change's next post.
    pub drafts: usize,
    /// Pending reviews moved onto `to`.
    pub reviews: usize,
    /// Drafts held on the local version, with the reason.
    pub held: Vec<(LocalDraftId, String)>,
    /// Pending reviews held on the local version.
    pub held_reviews: usize,
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
    /// Rows of `not_found` and `ambiguous` (or with a Change-Id Gerrit
    /// cannot be asked about) whose state this run changed: `Checking`
    /// became `Local`, a row became `Ambiguous`, or a `Vanished` row that
    /// Gerrit does not have was deleted with its local data (O1). The
    /// others were looked up as before and nothing about them moved.
    pub state_changed: Vec<LocalChangeId>,
    pub posts: Vec<PostReport>,
    /// Local versions that never reached Gerrit, ported or held (as
    /// planned: a draft edited meanwhile stays where it is).
    pub ported: Vec<Ported>,
    /// Local changes with nothing unsent left, each of whose versions is a
    /// Gerrit patch set or never will be one (ported, superseded): tombstones
    /// now.
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
        out.extend(self.ported.iter().map(|p| p.change));
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
    /// Not decided: the answer may be incomplete. Nothing changes; in
    /// particular a vanished row is not deleted.
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
            Error::Store(_) | Error::LeaseLost | Error::PostTimedOut | Error::Stopped => true,
            Error::DraftsNotUploaded(_)
            | Error::Local(_)
            | Error::DropOnly(_)
            | Error::ConsolidationBusy
            | Error::NotCurrent { .. }
            | Error::HeldChanged
            | Error::MaybePosted(_) => false,
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
    /// [`Error::Stopped`] once the run was asked to stop.
    fn go_on(&self) -> Result<(), Error> {
        if self.options.stop.is_stopped() {
            Err(Error::Stopped)
        } else {
            Ok(())
        }
    }

    fn claimant(&self) -> Claimant<'_> {
        Claimant {
            owner: &self.owner,
            at: Timestamp::now(),
            account: Some(self.me),
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
        Self::failed_with(votes, error, BTreeSet::new())
    }

    /// Failed, with the labels of `votes` that may only be dropped.
    fn failed_with(votes: Votes, error: String, drop_only: BTreeSet<String>) -> Self {
        Self {
            state: PendingReviewState::Failed,
            note: Some(error.clone()),
            keep: votes.clone(),
            drop_only,
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

    /// The versions whose data is ported now: not a patch set of the change
    /// (`on_gerrit` tells), with a draft that moves or a pending review on
    /// them, and superseded: a version seen later is a patch set of the
    /// change, the version was on no named branch in the last complete scan
    /// (amended away, or the branch reset), or the change vanished from the
    /// checkout. Only such a
    /// version "never reaches Gerrit"; a newer version that is not on
    /// Gerrit yet waits for its own patch set (its drafts say "sent when
    /// this version is on Gerrit"). In order of the versions.
    fn to_port(
        &self,
        on_gerrit: impl Fn(&RevisionId) -> bool,
        vanished: bool,
    ) -> Vec<&LocalPatchSet> {
        self.versions
            .iter()
            .filter(|v| !on_gerrit(&v.sha))
            .filter(|v| {
                self.on_versions
                    .iter()
                    .any(|d| d.patch_set == v.number && movable(d))
                    || self
                        .reviews
                        .iter()
                        .any(|r| r.patch_set == v.number && r.state == PendingReviewState::Pending)
            })
            .filter(|v| {
                vanished
                    || !v.on_branch
                    || self
                        .versions
                        .iter()
                        .any(|w| w.ordinal > v.ordinal && on_gerrit(&w.sha))
            })
            .collect()
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
    /// consolidation posts (`<name>-local-<id>-r<review>-<nonce>`): the
    /// app passes its `APP_NAME`. Tests may leave the default.
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
        let key = (std::sync::Arc::as_ptr(&self.store) as usize, self.server.0);
        let Some(_running) = Running::start(key) else {
            return report;
        };
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
        // Before any query goes out: a "not found" proves no more than
        // that the change was not on Gerrit when its lookup started.
        let asked = Timestamp::now();
        for (row, lookup) in self.look_up(&to_look_up, run).await? {
            match lookup {
                Lookup::Found(n) => found.push((row, n)),
                Lookup::NotFound | Lookup::Malformed if row.state == LocalChangeState::Vanished => {
                    // Gone from the checkout with unsent data, and never to
                    // be on Gerrit (O1): it goes with its local data. Only
                    // on a definite answer: the query succeeded, the
                    // Change-Id was one Gerrit can be asked about and the
                    // answer was not cut off (else the lookup is not
                    // `NotFound`); or the Change-Id is one Gerrit refuses
                    // in a push. Not while it holds another account's
                    // data: Gerrit hides that account's private changes.
                    if lookup == Lookup::NotFound {
                        run.report.not_found.push(row.id);
                    }
                    if self
                        .store()
                        .delete_vanished_local_change(self.server, row.id, run.me)?
                    {
                        let why = if lookup == Lookup::NotFound {
                            "is not on Gerrit"
                        } else {
                            "has a Change-Id Gerrit does not take"
                        };
                        log::info!(
                            "local change {} ({} in {}) vanished from the checkout and {why}: \
                             deleted with its unsent local review data",
                            row.id.0,
                            row.change_id,
                            row.project
                        );
                        run.report.state_changed.push(row.id);
                    }
                }
                Lookup::NotFound => {
                    run.report.not_found.push(row.id);
                    // A definite answer, as for O1 above: what makes the
                    // votes saved before `asked` purely local.
                    self.store()
                        .mark_local_change_not_found(self.server, row.id, asked)?;
                    if self.set_state_if(
                        &row,
                        &[LocalChangeState::Checking],
                        LocalChangeState::Local,
                    )? {
                        run.report.state_changed.push(row.id);
                    }
                }
                Lookup::Malformed => {
                    if self.set_state_if(
                        &row,
                        &[LocalChangeState::Checking],
                        LocalChangeState::Local,
                    )? {
                        // Said once, when it becomes local.
                        log::info!(
                            "local change {} has Change-Id {:?}, which Gerrit cannot be asked \
                             about; it stays local",
                            row.id.0,
                            row.change_id
                        );
                        run.report.state_changed.push(row.id);
                    }
                }
                Lookup::Ambiguous => {
                    run.report.ambiguous.push(row.id);
                    if self.set_state_if(
                        &row,
                        &[LocalChangeState::Checking, LocalChangeState::Local],
                        LocalChangeState::Ambiguous,
                    )? {
                        log::info!(
                            "local change {} ({} in {}) matches several Gerrit changes and its \
                             target branch is unknown; it is not consolidated",
                            row.id.0,
                            row.change_id,
                            row.project
                        );
                        run.report.state_changed.push(row.id);
                    }
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
                        self.hold_closed(row, number, &detail, run)
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
    /// no Gerrit change. Read and written under one store lock. True when
    /// the state changed.
    fn set_state_if(
        &self,
        row: &LocalChange,
        from: &[LocalChangeState],
        to: LocalChangeState,
    ) -> Result<bool, Error> {
        let mut store = self.store();
        let Some(now) = store.local_change(self.server, row.id)? else {
            return Ok(false);
        };
        if now.gerrit_number.is_none() && from.contains(&now.state) && now.state != to {
            store.set_local_change_state(self.server, row.id, to)?;
            return Ok(true);
        }
        Ok(false)
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
        let data = self.row_data(&row)?;
        let (mut moves, all_on_gerrit) = exact_moves(&data, &detail);
        let plan = self.port_plan(&row, number, &detail, &data, run).await?;
        moves.extend(plan.moves);
        // Looked up: local now. A vanished row keeps its state for LC5b.
        let settled = (row.state == LocalChangeState::Checking).then_some(LocalChangeState::Local);
        let (state, else_state) = if all_on_gerrit {
            (Some(LocalChangeState::Consolidated), settled)
        } else {
            (settled, None)
        };
        self.rewrite(
            &row,
            number,
            moves,
            plan.keep,
            state,
            else_state,
            plan.review_states,
        )?;
        run.report.ported.extend(plan.report);
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

    #[allow(clippy::too_many_arguments)]
    fn rewrite(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        moves: Vec<PatchSetMove>,
        keep: Vec<(LocalDraftId, String)>,
        state: Option<LocalChangeState>,
        else_state: Option<LocalChangeState>,
        review_states: Vec<ReviewStateChange>,
    ) -> Result<(), Problem> {
        let rewrite = ConsolidationRewrite {
            local_change: row.id,
            gerrit_number: number,
            moves,
            keep,
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

    /// Marks the row consolidated once nothing of it is left to do: nothing
    /// unsent, and every version a Gerrit patch set or never to become one
    /// (`store::Store::settle_local_change`).
    fn finish_change(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        if self.store().settle_local_change(self.server, row.id)?
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
}

/// The moves of a row's versions that are patch sets of `detail` (exact
/// matches): each version's drafts and pending reviews onto its patch set.
/// Also whether every version is a Gerrit patch set. The other versions
/// are ported, or wait ([`RowData::to_port`]).
fn exact_moves(data: &RowData, detail: &ChangeDetail) -> (Vec<PatchSetMove>, bool) {
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
    (moves, all_on_gerrit)
}

#[cfg(test)]
mod tests;
