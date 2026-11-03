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
    DraftSync, FileEntry, LocalApproval, LocalChange, LocalChangeId, LocalChangeState,
    LocalDraftId, LocalPatchSet, PatchSetNumber, PendingReview, PendingReviewId,
    PendingReviewState, ReviewInput, ReviewResult, Revision, RevisionId, Side,
};
use store::{Claimant, ConsolidationRewrite, LocalPost, PatchSetMove, ReviewStateChange};

use super::Syncer;
use crate::{Error, HasCheckout, PortCommit, PortError, PortMaps, PortSource};

mod held;
mod port;

pub use held::{HeldItem, HeldPublished, held_place};

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
            // Every time: whatever was written since is held too.
            return self.hold_closed(row, number, lite, run);
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
        let port_work = run
            .options
            .port
            .as_ref()
            .is_some_and(|p| p.has_checkout(&row.project) != HasCheckout::NotNow)
            && !self.port_backing_off(row.id)
            && !data
                .to_port(
                    |sha| shas.contains(&sha),
                    row.state == LocalChangeState::Vanished,
                )
                .is_empty();
        if newly_pushed || data.exact_work(&shas) || data.moved_sendable() || port_work {
            let detail = self.refresh_change(number).await?;
            if detail.summary.status != ChangeStatus::New {
                return self.hold_closed(row, number, &detail, run);
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
            return self.hold_closed(row, number, &detail, run);
        }
        self.consolidate_open(row, number, detail, run).await
    }

    /// Holds everything of a change found merged or abandoned, its
    /// approvals that may not move as drop-only (judged from `detail`).
    fn hold_closed(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let reason = match &detail.summary.status {
            ChangeStatus::Merged => "change merged".to_owned(),
            ChangeStatus::Abandoned => "change abandoned".to_owned(),
            other => format!("change {}", other.to_string().to_lowercase()),
        };
        let held = self
            .store()
            .hold_closed_local_change(self.server, row.id, number, &reason);
        match held {
            Ok(Some((reviews, drafts))) => {
                if row.state != LocalChangeState::Closed || reviews + drafts > 0 {
                    run.report.closed.push((row.id, number));
                }
                self.mark_drop_only(row, number, detail, run.options)?;
                Ok(())
            }
            Ok(None) => Ok(()),
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
                .any(|r| r.created.is_none_or(|c| c <= ours.date));
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
            self.rewrite(row, number, Vec::new(), Vec::new(), None, None, changes)?;
        }
        Ok(())
    }

    // ---- porting ----

    /// Ports the versions of `row` that never reached Gerrit
    /// ([`RowData::to_port`]) onto one patch set of `detail`: the current
    /// one if its commit is in the checkout, else the newest one that is.
    /// Each draft goes where [`port::port_draft`] says or is held with the
    /// reason; the pending reviews go along (their votes are decided when
    /// they are posted, see [`casts`]). With no patch set in the checkout,
    /// or no checkout configured for the project, everything is held.
    /// Without a port source those versions stay as they are; with a
    /// checkout that cannot be used now, or `git` failing, they stay too,
    /// and the change is not tried again before the slow re-check.
    async fn port_plan(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        data: &RowData,
        run: &mut Run<'_>,
    ) -> Result<PortPlan, Problem> {
        let mut plan = PortPlan::default();
        let on_gerrit = |sha: &RevisionId| {
            detail
                .revisions
                .iter()
                .any(|r| &r.sha == sha && !r.number.is_local())
        };
        let versions = data.to_port(on_gerrit, row.state == LocalChangeState::Vanished);
        if versions.is_empty() {
            return Ok(plan);
        }
        let Some(source) = run.options.port.clone() else {
            return Ok(plan);
        };
        match source.has_checkout(&row.project) {
            HasCheckout::Yes => {}
            HasCheckout::NotNow => {
                self.port_failed(row.id);
                return Ok(plan);
            }
            HasCheckout::NotConfigured => {
                let why = format!(
                    "No checkout is configured for project {}, so this could not be carried \
                     over from the local version.",
                    row.project
                );
                for v in versions {
                    plan.hold_version(row, detail, data, v, &why, run.options);
                }
                return Ok(plan);
            }
        }
        let (rev, commit) = match port_target(&*source, &row.project, detail) {
            Ok(Some(target)) => target,
            Ok(None) => {
                let why = format!(
                    "No patch set of change {number} is in the checkout, so this could not be \
                     carried over from the local version."
                );
                for v in versions {
                    plan.hold_version(row, detail, data, v, &why, run.options);
                }
                return Ok(plan);
            }
            Err(e) => {
                self.port_failed(row.id);
                if e != PortError::NoCheckout {
                    run.report
                        .errors
                        .push((row.id, format!("porting the local versions: {e}")));
                }
                return Ok(plan);
            }
        };
        // Gerrit takes comments only on the files its patch set changes;
        // only comments on files need the list.
        let on_files = versions.iter().any(|v| {
            data.on_versions
                .iter()
                .any(|d| d.patch_set == v.number && movable(d) && !d.path.starts_with('/'))
        });
        let files = if on_files {
            match self.client.get_files(number, &rev.sha, None).await {
                Ok(files) => files,
                Err(e) => {
                    let why = skip_or_stop(e.into())?;
                    self.port_failed(row.id);
                    run.report.errors.push((row.id, why));
                    return Ok(plan);
                }
            }
        } else {
            Vec::new()
        };
        let mut failed = false;
        for v in versions {
            match port_version(&*source, row, v, rev, &commit, &files, data) {
                Ok((m, keep)) => {
                    plan.report.push(Ported {
                        local_change: row.id,
                        change: number,
                        from: v.number,
                        to: Some(rev.number),
                        drafts: m.drafts.len(),
                        reviews: m.reviews.len(),
                        held: keep.clone(),
                        held_reviews: 0,
                    });
                    plan.moves.push(m);
                    plan.keep.extend(keep);
                }
                Err(e) => {
                    failed = true;
                    if e != PortError::NoCheckout {
                        run.report
                            .errors
                            .push((row.id, format!("porting PS {}: {e}", v.number.0)));
                    }
                }
            }
        }
        if failed {
            self.port_failed(row.id);
        } else {
            self.port_worked(row.id);
        }
        Ok(plan)
    }

    /// Porting `change` failed in a way that may pass (a checkout that
    /// could not be opened, `git` failing): it is tried again after the
    /// slow re-check, not on every run.
    fn port_failed(&self, change: LocalChangeId) {
        self.port_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(change, Timestamp::now());
    }

    fn port_worked(&self, change: LocalChangeId) {
        self.port_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&change);
    }

    /// Whether porting `change` failed less than the slow re-check ago.
    fn port_backing_off(&self, change: LocalChangeId) -> bool {
        let now = Timestamp::now();
        let since = now
            .checked_sub(self.consolidation_timing.slow_recheck)
            .unwrap_or(now);
        self.port_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&change)
            .is_some_and(|t| *t > since)
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
        // Only the account the run is signed in as speaks for its reviews:
        // one written under another account is held for the user.
        let (reviews, foreign): (Vec<PendingReview>, Vec<PendingReview>) = reviews
            .into_iter()
            .partition(|r| r.author.is_none_or(|a| a == run.me));
        for r in &foreign {
            self.hold_foreign(row, number, detail, r, run)?;
        }
        // Likewise comments written under another account.
        self.store()
            .hold_foreign_drafts(self.server, row.id, run.me)?;
        self.supersede(row, number, detail, &reviews, current, run.options)?;
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
                let Some(post) = claimed else {
                    continue;
                };
                // Posted before, and the claim was lost (a power cut
                // before the store wrote it): settle, do not post again.
                let prefix = self.review_tag_prefix(row.id, r.id);
                if let Some(m) = mine_tagged(detail, run.me, |t| t.starts_with(&prefix)) {
                    let m = m.clone();
                    self.settle_found(row, number, detail, rev, &post, &nonce, &m, run)
                        .await?;
                    continue;
                }
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

    /// Records which votes of `row`'s held and failed reviews on `number`
    /// may only be dropped, as [`held_vote_drop_only`] judges them against
    /// `detail`'s current patch set. Skipped when `detail` does not say the
    /// labels' values (a batch query's answer): the held card and
    /// [`Syncer::publish_held`] judge again from the change as it is then.
    fn mark_drop_only(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        options: &ConsolidateOptions,
    ) -> Result<(), Problem> {
        if detail.label_values.is_empty() {
            return Ok(());
        }
        let mut store = self.store();
        for r in store.pending_reviews_of_local_change(self.server, row.id)? {
            if r.change != number
                || !matches!(
                    r.state,
                    PendingReviewState::Held | PendingReviewState::Failed
                )
            {
                continue;
            }
            let drop_only = drop_only_now(row, detail, &r, options);
            if drop_only != r.drop_only {
                store.set_held_drop_only(self.server, r.id, &drop_only)?;
            }
        }
        Ok(())
    }

    /// Holds pending review `r`, written while another account was signed
    /// in: the run's account does not send it on its own. The user may
    /// publish it from the held card (then as the account signed in now),
    /// or drop it.
    fn hold_foreign(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        r: &PendingReview,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let reason = foreign_reason(r);
        let mut change = ReviewStateChange::new(
            r.id,
            &[PendingReviewState::Pending],
            PendingReviewState::Held,
            Some(reason.clone()),
        );
        change.drop_only = drop_only_now(row, detail, r, run.options);
        let drop_only: Vec<String> = change.drop_only.iter().cloned().collect();
        let held = self.store().apply_review_state(self.server, &change)?;
        if held {
            run.report.posts.push(PostReport {
                local_change: row.id,
                change: number,
                patch_set: r.patch_set,
                review: Some(r.id),
                comments: 0,
                votes: Vec::new(),
                recovered: false,
                outcome: PostOutcome::VotesHeld {
                    votes: r.labels.clone().into_iter().collect(),
                    drop_only,
                    reason,
                },
            });
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

    /// A newer review whose votes will be cast (see [`casts`]) supersedes
    /// the vote of every older unsent review of the change on the same
    /// label: the older one loses it before anything is sent. A newer
    /// review whose votes will be held supersedes nothing, so an older vote
    /// meant for the current patch set is not lost to it.
    fn supersede(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        sending: &[PendingReview],
        current: Current,
        options: &ConsolidateOptions,
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
        let cast = |r: &&PendingReview| {
            detail
                .revision(r.patch_set)
                .is_some_and(|rev| casts(row, r, rev, current, options))
        };
        for newer in sending.iter().filter(cast) {
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
        let castable = review.is_none_or(|r| casts(row, r, rev, *current, run.options));
        let (send_votes, held) = if castable || votes.is_empty() {
            (votes, None)
        } else {
            let held: Votes = votes.into_iter().collect();
            let drop_only = drop_only_labels(row, detail, review, &held, run.options);
            let reason =
                votes_held_reason(row, review, rev, *current, run.options, &held, &drop_only);
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
        let tag = self.local_tag(row.id, review.map(|r| r.id), nonce);
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
            .and_then(|()| run.go_on())
            .and_then(|()| self.renew_lease(&run.owner));
        if let Err(e) = checked {
            self.give_up(row, &post, nonce, PendingReviewState::Pending, None, None)?;
            return Err(e.into());
        }
        let sent_votes: Votes = input.labels.clone().into_iter().collect();
        let answer = match self
            .post_review_by_deadline(number, &rev.sha, &input)
            .await?
        {
            Err(e) if is_closed_answer(&e) => {
                self.give_up(row, &post, nonce, PendingReviewState::Pending, None, None)?;
                return Err(Problem::Closed);
            }
            Err(e) if !input.labels.is_empty() && is_vote_refusal(&e) => {
                // Votes never sink comments: the same post without them,
                // under the same claim and tag.
                let drop_only = review
                    .map(|r| drop_only_now(row, detail, r, run.options))
                    .unwrap_or_default();
                let failed = Settled::failed_with(sent_votes.clone(), refusal_text(&e), drop_only);
                let bare = ReviewInput {
                    labels: BTreeMap::new(),
                    ..input.clone()
                };
                if bare.comments.is_empty() {
                    Ok((Some(failed), None))
                } else {
                    match self
                        .post_review_by_deadline(number, &rev.sha, &bare)
                        .await?
                    {
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

    /// Posts a review, giving up after the post deadline: the request is
    /// dropped (which aborts it) and the run ends with
    /// [`Error::PostTimedOut`], leaving the claim for the next run's tag
    /// check.
    async fn post_review_by_deadline(
        &self,
        number: ChangeNumber,
        revision: &model::RevisionId,
        input: &ReviewInput,
    ) -> Result<Result<ReviewResult, ApiError>, Problem> {
        let post = Box::pin(self.client.post_review(number, revision, input));
        let deadline = Box::pin(async_io::Timer::after(
            self.consolidation_timing.post_deadline,
        ));
        match futures::future::select(post, deadline).await {
            futures::future::Either::Left((answer, _)) => Ok(answer),
            futures::future::Either::Right(_) => Err(Problem::Stop(Error::PostTimedOut)),
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

    /// The tag of a post: `<app>-local-<change>-r<review>-<nonce>`, or
    /// `<app>-local-<change>-<nonce>` for a comment-only post. The review
    /// id lets a run find a review that was posted although its claim was
    /// lost ([`Syncer::review_tag_prefix`]).
    fn local_tag(
        &self,
        change: LocalChangeId,
        review: Option<PendingReviewId>,
        nonce: &str,
    ) -> String {
        match review {
            Some(r) => format!("{}{nonce}", self.review_tag_prefix(change, r)),
            None => format!("{}-local-{}-{nonce}", self.app_name, change.0),
        }
    }

    /// What every tag of review `review`'s posts starts with.
    fn review_tag_prefix(&self, change: LocalChangeId, review: PendingReviewId) -> String {
        format!("{}-local-{}-r{}-", self.app_name, change.0, review.0)
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
        // The account the post went out as: the first claim's, which an
        // earlier sign-in may have made.
        let poster = self
            .store()
            .local_post_account(self.server, nonce)?
            .unwrap_or(run.me);
        let detail = match self.refresh_change(number).await {
            Ok(d) => d,
            Err(Error::Api(ApiError::NotFound)) => {
                let why = format!("change {number} is not on Gerrit any more");
                return self.give_up_gone(&row, &post, nonce, &why);
            }
            Err(e) => return Err(e.into()),
        };
        let tag = self.local_tag(change, post.review.as_ref().map(|r| r.id), nonce);
        let review_ps = post.review.as_ref().map(|r| r.patch_set);
        // Arrived, whoever posted it: settled as sent below. The nonce makes
        // the tag unique; the author must still be the claim's account (or
        // this one), as nobody else posts our tags.
        let message = tagged_by(&detail, &[poster, run.me], |t| t == tag)
            .filter(|m| review_ps.is_none() || m.revision.is_none() || m.revision == review_ps);
        let foreign = poster != run.me
            || post
                .review
                .as_ref()
                .is_some_and(|r| r.author.is_some_and(|a| a != run.me));
        if message.is_none() && foreign {
            // Claimed for another account and not posted: not this
            // account's to post. The review is held with its comments.
            let reason = match &post.review {
                Some(r) => foreign_reason(r),
                None => format!(
                    "written while signed in as account {}; it is sent only by that account, \
                     or published from here",
                    poster.0
                ),
            };
            self.give_up(
                &row,
                &post,
                nonce,
                PendingReviewState::Held,
                Some(reason.clone()),
                Some(&reason),
            )?;
            return self.mark_drop_only(&row, number, &detail, run.options);
        }
        let ps = review_ps
            .or_else(|| message.and_then(|m| m.revision))
            .or_else(|| post.drafts.iter().map(|d| d.patch_set).min());
        let Some(rev) = ps.and_then(|ps| detail.revision(ps)) else {
            let why = match ps {
                Some(ps) => format!("PS {} is not on change {number} any more", ps.0),
                None => format!("the post's patch set is not on change {number}"),
            };
            return self.give_up_gone(&row, &post, nonce, &why);
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
        let message = message.clone();
        self.settle_found(&row, number, &detail, rev, &post, nonce, &message, run)
            .await?;
        self.finish_change(&row, number, run)
    }

    /// Gives up a post whose change or patch set is gone from Gerrit: the
    /// review fails and the comments are held with `why`, so the claim
    /// does not lock them for good.
    fn give_up_gone(
        &self,
        row: &LocalChange,
        post: &LocalPost,
        nonce: &str,
        why: &str,
    ) -> Result<(), Problem> {
        log::warn!(
            "consolidation gave a post of local change {} up: {why}",
            row.id.0
        );
        self.give_up(
            row,
            post,
            nonce,
            PendingReviewState::Failed,
            Some(why.to_owned()),
            Some(why),
        )
    }

    /// Settles a claimed post that is found on the server already (its
    /// change message `message`, by the signed-in account): removes the
    /// drafts that were published with its tag (the others stay for a
    /// later post) and gives the review the state its votes call for. The
    /// votes it cast are in the message's text ("Patch Set 2:
    /// Code-Review+1"); Gerrit leaves unchanged votes out of it, so on the
    /// current patch set the change's vote list counts too.
    #[allow(clippy::too_many_arguments)]
    async fn settle_found(
        &self,
        row: &LocalChange,
        number: ChangeNumber,
        detail: &ChangeDetail,
        rev: &Revision,
        post: &LocalPost,
        nonce: &str,
        message: &model::ChangeMessage,
        run: &mut Run<'_>,
    ) -> Result<(), Problem> {
        let parts = Parts::of(post, rev.number);
        let tag = message.tag.clone().unwrap_or_default();
        let posted: Vec<(LocalDraftId, u32)> = match self.client.get_comments(number).await {
            Ok(published) => parts
                .comments
                .iter()
                .filter(|d| {
                    published.iter().any(|c| {
                        c.tag.as_deref() == Some(tag.as_str())
                            && c.path == d.path
                            && c.patch_set == d.patch_set
                            && c.message == d.message
                    })
                })
                .map(|d| (d.local_id, d.version))
                .collect(),
            Err(e) => {
                skip_or_stop(e.into())?;
                parts.posted.clone()
            }
        };
        let cast = votes_in_message(&message.message);
        let current = detail.current_patch_set;
        // Who posted it: whoever the claim was made for.
        let me = message
            .real_author
            .as_ref()
            .or(message.author.as_ref())
            .map_or(run.me, |a| a.id);
        let votes: Votes = post
            .review
            .as_ref()
            .map(|r| r.labels.clone().into_iter().collect())
            .unwrap_or_default();
        let (applied, missing): (Votes, Votes) = votes.into_iter().partition(|(label, value)| {
            cast.get(label) == Some(value)
                || (current == Some(rev.number)
                    && detail.votes.get(label).is_some_and(|all| {
                        all.iter().any(|v| v.account.id == me && v.value == *value)
                    }))
        });
        // Whether the post meant to cast its votes, as `post_claimed` decides.
        let meant = post
            .review
            .as_ref()
            .is_none_or(|r| casts(row, r, rev, current, run.options));
        let settled = if !missing.is_empty() && meant {
            let error = format!(
                "the review was posted, but Gerrit does not show {}",
                describe_votes(&missing)
            );
            Some(Settled::failed(missing, error))
        } else if !missing.is_empty() {
            let review = post.review.as_ref();
            let drop_only = drop_only_labels(row, detail, review, &missing, run.options);
            let reason =
                votes_held_reason(row, review, rev, current, run.options, &missing, &drop_only);
            Some(Settled::held(missing, drop_only, reason))
        } else if !applied.is_empty() && current.is_some_and(|c| c != rev.number) {
            // Cast, on a patch set that is not current now; whether it
            // was when they arrived, the message's date tells (a patch set
            // without a time counts as maybe newer).
            let newer_first = detail
                .revisions
                .iter()
                .filter(|r| r.number > rev.number)
                .any(|r| r.created.is_none_or(|c| c <= message.date));
            newer_first.then(|| {
                let drop_only: BTreeSet<String> = applied
                    .iter()
                    .filter(|(l, v)| is_approval(detail, l, *v))
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
        self.settle(row, post, nonce, &posted, settled)?;
        run.report.posts.push(PostReport {
            local_change: row.id,
            change: number,
            patch_set: rev.number,
            review: post.review.as_ref().map(|r| r.id),
            comments: posted.len(),
            votes: applied,
            recovered: true,
            outcome,
        });
        Ok(())
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

/// What porting decided for one change, for its rewrite.
#[derive(Default)]
struct PortPlan {
    moves: Vec<PatchSetMove>,
    keep: Vec<(LocalDraftId, String)>,
    review_states: Vec<ReviewStateChange>,
    report: Vec<Ported>,
}

impl PortPlan {
    /// Holds everything of version `v` that would have been ported, with
    /// `why`: its drafts, and its pending reviews with their votes
    /// (approvals drop-only where the rules say so).
    fn hold_version(
        &mut self,
        row: &LocalChange,
        detail: &ChangeDetail,
        data: &RowData,
        v: &LocalPatchSet,
        why: &str,
        options: &ConsolidateOptions,
    ) {
        let held: Vec<(LocalDraftId, String)> = data
            .on_versions
            .iter()
            .filter(|d| d.patch_set == v.number && movable(d))
            .map(|d| (d.local_id, why.to_owned()))
            .collect();
        let mut reviews = 0;
        for r in data
            .reviews
            .iter()
            .filter(|r| r.patch_set == v.number && r.state == PendingReviewState::Pending)
        {
            let votes: Votes = r.labels.iter().map(|(l, v)| (l.clone(), *v)).collect();
            let drop_only = drop_only_labels(row, detail, Some(r), &votes, options);
            let mut change = ReviewStateChange::new(
                r.id,
                &[PendingReviewState::Pending],
                PendingReviewState::Held,
                Some(format!("{why}{}", vote_actions(&votes, &drop_only))),
            );
            change.drop_only = drop_only;
            self.review_states.push(change);
            reviews += 1;
        }
        self.report.push(Ported {
            local_change: row.id,
            change: detail.summary.number,
            from: v.number,
            to: None,
            drafts: 0,
            reviews: 0,
            held: held.clone(),
            held_reviews: reviews,
        });
        self.keep.extend(held);
    }
}

/// The patch set to port onto: the current one of `detail` if its commit
/// is in the checkout, else the newest one that is; with the commit as the
/// checkout has it. `None` when no patch set is in the checkout.
fn port_target<'a>(
    source: &dyn PortSource,
    project: &str,
    detail: &'a ChangeDetail,
) -> Result<Option<(&'a Revision, PortCommit)>, PortError> {
    let mut revisions: Vec<&Revision> = detail
        .revisions
        .iter()
        .filter(|r| !r.number.is_local())
        .collect();
    revisions.sort_by_key(|r| std::cmp::Reverse(r.number));
    if let Some(at) = revisions
        .iter()
        .position(|r| Some(r.number) == detail.current_patch_set)
    {
        let current = revisions.remove(at);
        revisions.insert(0, current);
    }
    for rev in revisions {
        if let Some(commit) = source.commit(project, &rev.sha)? {
            return Ok(Some((rev, commit)));
        }
    }
    Ok(None)
}

/// The move of version `v`'s drafts and pending reviews onto `rev` (whose
/// commit is `commit` and whose files Gerrit lists as `files`), and the
/// drafts held with the reason. One `line_maps` call for the revision side
/// and one for the parent side, as needed. `Err` leaves the version as it
/// is (no checkout, `git` failing); a commit missing from the checkout
/// holds what it covers.
fn port_version(
    source: &dyn PortSource,
    row: &LocalChange,
    v: &LocalPatchSet,
    rev: &Revision,
    commit: &PortCommit,
    files: &[FileEntry],
    data: &RowData,
) -> Result<(PatchSetMove, Vec<(LocalDraftId, String)>), PortError> {
    let drafts: Vec<&Draft> = data
        .on_versions
        .iter()
        .filter(|d| d.patch_set == v.number && movable(d))
        .collect();
    // The paths the checkout is asked about: not those `port_draft` holds
    // or carries over without a map.
    let paths = |side: Side| -> Vec<String> {
        let set: BTreeSet<&str> = drafts
            .iter()
            .filter(|d| d.side == side && d.in_reply_to.is_none())
            .filter(|d| d.path != model::PATCHSET_LEVEL_PATH)
            .filter(|d| {
                side == Side::Revision
                    || (d.path != model::COMMIT_MSG_PATH && d.parent.is_none_or(|p| p <= 1))
            })
            .map(|d| d.path.as_str())
            .collect();
        set.into_iter().map(str::to_owned).collect()
    };
    let (revision_paths, parent_paths) = (paths(Side::Revision), paths(Side::Parent));
    let revision = if revision_paths.is_empty() {
        Ok(PortMaps::new())
    } else {
        side_maps(source.line_maps(&row.project, &v.sha, &rev.sha, &revision_paths))?
    };
    let parent = if parent_paths.is_empty() {
        Ok(PortMaps::new())
    } else {
        match source.commit(&row.project, &v.sha)? {
            None => Err(format!(
                "The local version cannot be compared with the patch set: commit {} is not \
                 in the checkout.",
                v.sha.short(12)
            )),
            Some(from) if from.is_merge() || commit.is_merge() => Err(
                "Comments on the base side of a merge commit are not carried over: Gerrit \
                 compares a merge with an auto-merge."
                    .to_owned(),
            ),
            Some(from) => match (from.parent(), commit.parent()) {
                // The very same base: every file is the same there.
                (Some(a), Some(b)) if a == b => Ok(parent_paths
                    .iter()
                    .map(|p| {
                        let same = model::LineMap::identity(p.clone(), u32::MAX);
                        (p.clone(), crate::PathMap::Lines(same))
                    })
                    .collect()),
                (Some(a), Some(b)) => {
                    side_maps(source.line_maps(&row.project, a, b, &parent_paths))?
                }
                _ => Err("A root commit has no base side to carry a comment over to.".to_owned()),
            },
        }
    };
    let mut moved = Vec::new();
    let mut keep = Vec::new();
    for d in drafts {
        match port::port_draft(d, rev.number, files, &revision, &parent) {
            Ok(anchor) => moved.push((d.local_id, Some(anchor))),
            Err(why) => keep.push((d.local_id, why)),
        }
    }
    let reviews = data
        .reviews
        .iter()
        .filter(|r| r.patch_set == v.number && r.state == PendingReviewState::Pending)
        .map(|r| r.id)
        .collect();
    Ok((
        PatchSetMove {
            from: v.number,
            to: rev.as_ref(),
            exact: false,
            drafts: moved,
            reviews,
        },
        keep,
    ))
}

/// One side's answer of the checkout: the maps, or (a commit missing from
/// the checkout) why the drafts on that side are held. Other errors leave
/// the version as it is.
fn side_maps(answer: Result<PortMaps, PortError>) -> Result<port::SideMaps, PortError> {
    match answer {
        Ok(maps) => Ok(Ok(maps)),
        Err(e @ PortError::MissingCommit(_)) => Ok(Err(format!(
            "The local version cannot be compared with the patch set: {e}."
        ))),
        Err(e) => Err(e),
    }
}

/// The newest change message by `me` whose tag `matches`.
fn mine_tagged(
    detail: &ChangeDetail,
    me: AccountId,
    matches: impl Fn(&str) -> bool,
) -> Option<&model::ChangeMessage> {
    tagged_by(detail, &[me], matches)
}

/// The newest change message by one of `accounts` whose tag `matches`.
fn tagged_by<'a>(
    detail: &'a ChangeDetail,
    accounts: &[AccountId],
    matches: impl Fn(&str) -> bool,
) -> Option<&'a model::ChangeMessage> {
    let by = |a: &Option<model::Account>| a.as_ref().is_some_and(|a| accounts.contains(&a.id));
    detail
        .messages
        .iter()
        .rev()
        .find(|m| m.tag.as_deref().is_some_and(&matches) && (by(&m.author) || by(&m.real_author)))
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
        // Votes Gerrit did not take at all are held too, not lost.
        let dropped = input.unapplied_labels(result);
        let mut reason = if cast.is_empty() {
            format!("PS {} was no longer current (PS {} is).", ps.0, current.0)
        } else {
            landed_reason(ps, current, &cast)
        };
        if !dropped.is_empty() {
            reason.push_str(&format!(
                " Gerrit did not apply {}.",
                describe_votes(&dropped)
            ));
        }
        let mut held = cast;
        held.extend(dropped);
        held.sort();
        return Some(Settled::held(held, drop_only, reason));
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

/// Why a review written under another account is held.
fn foreign_reason(r: &PendingReview) -> String {
    let who = match (&r.author_name, r.author) {
        (Some(name), _) => name.clone(),
        (None, Some(id)) => format!("account {}", id.0),
        (None, None) => "another account".to_owned(),
    };
    format!(
        "written while signed in as {who}; it is sent only by that account, or published from \
         here"
    )
}

/// Whether held vote `label` = `value` of `review` may only be dropped
/// rather than published onto `target`, a patch set of `detail`: it is
/// already marked so ([`PendingReview::drop_only`]), or it is an approval
/// (the label's highest positive value on `detail`) given on another
/// commit than `target`, unless the review is purely local (its votes
/// were given before the change was on Gerrit) and the stricter of its
/// `local_approval` snapshot and `setting` (the project's now) lets
/// approvals move (anything but `exact`). The held card shows it and
/// [`Syncer::publish_held`] refuses by it, so the strict rule and `exact`
/// hold however the review came to be held.
pub fn held_vote_drop_only(
    detail: &ChangeDetail,
    review: &PendingReview,
    label: &str,
    value: i32,
    target: &RevisionId,
    setting: LocalApproval,
) -> bool {
    review.drop_only.contains(label)
        || (is_approval(detail, label, value)
            && review.given_on_sha != *target
            && !(purely_local(review) && review.strategy.max(setting) != LocalApproval::Exact))
}

/// The labels of `review`'s votes that may only be dropped as things are
/// now: [`held_vote_drop_only`] against `detail`'s current patch set, with
/// the project's setting from `options`.
fn drop_only_now(
    row: &LocalChange,
    detail: &ChangeDetail,
    review: &PendingReview,
    options: &ConsolidateOptions,
) -> BTreeSet<String> {
    let Some(target) = detail
        .current_patch_set
        .and_then(|ps| detail.revision(ps))
        .map(|r| r.sha.clone())
    else {
        return review.drop_only.clone();
    };
    let setting = options
        .local_approval
        .get(&row.project)
        .copied()
        .unwrap_or(LocalApproval::Exact);
    review
        .labels
        .iter()
        .filter(|(l, v)| held_vote_drop_only(detail, review, l, **v, &target, setting))
        .map(|(l, _)| l.clone())
        .collect()
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
    let drop_only = review
        .is_none_or(|r| !purely_local(r) || strategy(row, r, options) == LocalApproval::Exact);
    votes
        .iter()
        .filter(|(l, v)| drop_only && is_approval(detail, l, *v))
        .map(|(l, _)| l.clone())
        .collect()
}

/// "Purely local": when the review's votes were given, the change was not
/// on Gerrit, or Gerrit could not be reached
/// ([`PendingReview::purely_local`]: no vote saved on a local patch set
/// of a Gerrit change, and the votes saved offline or before a complete
/// lookup that did not find the change started). Otherwise the strict
/// rule applies to them, also when nothing proves the change was there:
/// Gerrit was reachable at the vote, but no lookup completed before the
/// change showed up (a push right after the vote). That is the safe
/// choice; a saved review starts a lookup at once, so the gap is small.
///
/// The one place this is decided: consolidation's vote decisions, the
/// held card ([`held_vote_drop_only`]) and recovery go through it.
pub fn purely_local(review: &PendingReview) -> bool {
    review.purely_local()
}

/// The `local_approval` strategy for a review: the stricter of its
/// snapshot and the project's setting now (`exact` without an entry).
fn strategy(
    row: &LocalChange,
    review: &PendingReview,
    options: &ConsolidateOptions,
) -> LocalApproval {
    review.strategy.max(
        options
            .local_approval
            .get(&row.project)
            .copied()
            .unwrap_or(LocalApproval::Exact),
    )
}

/// Whether the review's votes are cast when it is posted to `rev`: never
/// unless `rev` is current. On the very commit they were given on (an
/// exact match), they are. On another one (the review was ported from a
/// version that never reached Gerrit) only under `follow` for a purely
/// local change: the strict rule never moves a vote, and the `exact*`
/// strategies hold them.
fn casts(
    row: &LocalChange,
    review: &PendingReview,
    rev: &Revision,
    current: Current,
    options: &ConsolidateOptions,
) -> bool {
    if current != Some(rev.number) {
        return false;
    }
    is_exact(review, rev)
        || (purely_local(review) && strategy(row, review, options) == LocalApproval::Follow)
}

/// Whether `rev` is the commit the review's votes were given on.
fn is_exact(review: &PendingReview, rev: &Revision) -> bool {
    review.given_on_sha == rev.sha
}

/// Why the votes `votes` of a post to `rev` are held: for an exact match
/// (or a post without a review) because `rev` is not current, else because
/// the review was ported ([`ported_held_reason`]).
#[allow(clippy::too_many_arguments)]
fn votes_held_reason(
    row: &LocalChange,
    review: Option<&PendingReview>,
    rev: &Revision,
    current: Current,
    options: &ConsolidateOptions,
    votes: &[(String, i32)],
    drop_only: &BTreeSet<String>,
) -> String {
    match review {
        Some(r) if !is_exact(r, rev) => {
            ported_held_reason(row, r, rev, current, options, votes, drop_only)
        }
        _ => held_reason(rev.number, current, votes, drop_only),
    }
}

/// Why the votes of `review`, ported from a local version that never
/// reached Gerrit onto `rev`, are held, and what the user may do with them
/// (the structured form is `drop_only`).
#[allow(clippy::too_many_arguments)]
fn ported_held_reason(
    row: &LocalChange,
    review: &PendingReview,
    rev: &Revision,
    current: Current,
    options: &ConsolidateOptions,
    votes: &[(String, i32)],
    drop_only: &BTreeSet<String>,
) -> String {
    let mut reason = format!(
        "These votes were given on local commit {}, which never reached Gerrit; the review \
         was carried over to PS {}, so they were not sent.",
        review.given_on_sha.short(12),
        rev.number.0
    );
    if review.known_on_gerrit {
        reason.push_str(" The change was on Gerrit when they were given, so they are not moved.");
    } else if !purely_local(review) {
        reason.push_str(
            " Gerrit could not confirm the change was not there yet when you voted, so they \
             are not moved.",
        );
    } else {
        match strategy(row, review, options) {
            LocalApproval::Follow if current != Some(rev.number) => {
                let current =
                    current.map_or_else(|| "another".to_owned(), |c| format!("PS {}", c.0));
                reason.push_str(&format!(
                    " PS {} is not the current patch set on Gerrit ({current} is).",
                    rev.number.0
                ));
            }
            LocalApproval::Follow => {}
            LocalApproval::ExactOrManual | LocalApproval::Exact => {
                reason.push_str(" The project sends votes only onto the exact commit.");
            }
        }
    }
    reason.push_str(&vote_actions(votes, drop_only));
    reason
}

/// What the user may do with held votes that were given on another commit
/// than the one they would go to, as a sentence for each kind (with a
/// leading blank); empty without votes.
fn vote_actions(votes: &[(String, i32)], drop_only: &BTreeSet<String>) -> String {
    let (fixed, others): (Votes, Votes) = votes
        .iter()
        .cloned()
        .partition(|(l, _)| drop_only.contains(l));
    let mut out = String::new();
    if !fixed.is_empty() {
        out.push_str(&format!(
            " {}: an approval goes only onto the commit it was given on, so it can only be \
             dropped.",
            describe_votes(&fixed)
        ));
    }
    if !others.is_empty() {
        out.push_str(&format!(
            " {}: publish onto the current patch set, or drop.",
            describe_votes(&others)
        ));
    }
    out
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
/// label's highest value, when that is positive, or more (a vote given
/// with borrowed labels whose range is wider than the change's).
fn is_approval(detail: &ChangeDetail, label: &str, value: i32) -> bool {
    detail
        .label_values
        .get(label)
        .and_then(|values| values.iter().map(|v| v.value).max())
        .is_some_and(|max| max > 0 && value >= max)
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
