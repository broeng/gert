//! The held card's actions: what the user does with the items that
//! consolidation held on a Gerrit change (a vote, a review message, a
//! comment that did not port).
//!
//! Dropping an item only touches the store. Publishing is **user
//! initiated**: [`Syncer::publish_held`] sends the selected items in one
//! review post to the change's current patch set, and nothing else: votes
//! as votes, a review message as the patch-set message, and a held comment
//! as a patch-set-level comment whose text says where it was ("On local 2,
//! src/a.rs line 40: …"), never at a guessed line. Like consolidation's
//! posts it carries `drafts: KEEP` (no other draft of the user is
//! published), `ignore_automatic_attention_set_rules` and a tag of the
//! form `<app>-local-<change>-r<review>-<nonce>`, and it is claimed in the
//! store before it goes out (`store::Store::claim_held_publish`), so a
//! crash after the post is recovered by the next consolidation run's tag
//! check instead of being posted twice. It holds the server's
//! consolidation lease while it runs, so it never overlaps a consolidation
//! run on that server (in this process or another): while one runs, it
//! refuses with [`Error::ConsolidationBusy`] and sends nothing.
//!
//! An approval that may not move ("approvals are only sent on the exact
//! commit they were given on") is never published: [`Error::DropOnly`],
//! before anything is sent, whatever the caller asks. Which votes those are
//! is judged by [`held_vote_drop_only`] from the change as it is right
//! before the post (an approval given on another commit, unless purely
//! local under a strategy that lets it move), whatever was recorded when
//! the vote was held; the held card shows the same.

use model::{COMMIT_MSG_PATH, Draft, Location, MERGE_LIST_PATH, PATCHSET_LEVEL_PATH, PatchSetRef};
use store::{HeldItems, HeldPublish};

use super::*;

/// One held item of a Gerrit change's held card.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HeldItem {
    /// The vote on `label` of a held or failed review.
    Vote {
        review: PendingReviewId,
        label: String,
    },
    /// The message of a held or failed review.
    Message(PendingReviewId),
    /// A held comment.
    Comment(LocalDraftId),
}

/// What [`Syncer::publish_held`] posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldPublished {
    /// The patch set posted to: the current one.
    pub patch_set: PatchSetNumber,
    /// The votes that were cast.
    pub votes: Votes,
    /// The comments posted, the message included.
    pub comments: usize,
    /// `Sent`; `VotesFailed` or `VotesHeld` when votes did not apply (the
    /// new review keeps them, in the held card); `Refused` when Gerrit
    /// refused the post as a whole (nothing was posted; the items are back
    /// in the held card with Gerrit's text).
    pub outcome: PostOutcome,
}

impl Syncer {
    /// What is held on Gerrit change `gerrit` from local reviews: drafts
    /// that did not port, and held or failed reviews, of an open or closed
    /// change. Blocking (SQLite).
    pub fn held_items(&self, gerrit: ChangeNumber) -> Result<HeldItems, Error> {
        Ok(self.store().held_items(self.server, gerrit)?)
    }

    /// Drops one held item, also a drop-only vote. Its local change becomes
    /// a tombstone once nothing of it is left (see
    /// `store::Store::settle_local_change`). False if the item is not held
    /// (any more). Blocking (SQLite).
    pub fn drop_held(&self, item: &HeldItem) -> Result<bool, Error> {
        let mut store = self.store();
        let (dropped, owner) = match item {
            HeldItem::Vote { review, label } => {
                let owner = store
                    .load_pending_review(self.server, *review)?
                    .map(|r| r.local_change);
                (store.drop_held_vote(self.server, *review, label)?, owner)
            }
            HeldItem::Message(review) => {
                let owner = store
                    .load_pending_review(self.server, *review)?
                    .map(|r| r.local_change);
                (store.drop_held_message(self.server, *review)?, owner)
            }
            HeldItem::Comment(draft) => {
                let owner = store.held_draft_owner(self.server, *draft)?;
                (store.drop_held_draft(self.server, *draft)?, owner)
            }
        };
        if dropped && let Some(owner) = owner {
            store.settle_local_change(self.server, owner)?;
        }
        Ok(dropped)
    }

    /// Publishes held items of Gerrit change `gerrit` onto its current
    /// patch set `onto`, at the user's request (see the module
    /// documentation). The items must belong to one local change.
    ///
    /// `setting` is the project's `local_approval` now (`exact` without an
    /// entry). Checked first, before anything is sent: a vote that may
    /// only be dropped ([`held_vote_drop_only`], as recorded and again
    /// against the fresh change) fails with [`Error::DropOnly`]; another consolidation run on the server with
    /// [`Error::ConsolidationBusy`]; `onto` no longer being Gerrit's
    /// current patch set (asked right before the post) with
    /// [`Error::NotCurrent`]; an item that is no longer held, or items of
    /// several local changes, with [`Error::HeldChanged`]. After the post,
    /// the published items leave the held list, and the change and its
    /// comments are fetched again. A post whose fate is unknown (no answer
    /// in time, a lost connection) fails and keeps its claim, which the
    /// next consolidation run settles by the tag.
    pub async fn publish_held(
        &self,
        gerrit: ChangeNumber,
        onto: PatchSetNumber,
        items: &[HeldItem],
        setting: LocalApproval,
    ) -> Result<HeldPublished, Error> {
        if gerrit.is_local() || onto.is_local() {
            return Err(ApiError::LocalNumber(format!("change {gerrit}, patch set {onto}")).into());
        }
        if !self.client.is_authenticated() {
            return Err(ApiError::Unauthorized.into());
        }
        // Never a drop-only vote, whatever the caller asks: refused before
        // any request (the store refuses it again when claiming).
        let held = self.held_items(gerrit)?;
        for item in items {
            if let HeldItem::Vote { review, label } = item
                && held
                    .reviews
                    .iter()
                    .any(|r| r.id == *review && r.drop_only.contains(label))
            {
                return Err(Error::DropOnly(label.clone()));
            }
        }
        let key = (Arc::as_ptr(&self.store) as usize, self.server.0);
        let Some(_running) = Running::start(key) else {
            return Err(Error::ConsolidationBusy);
        };
        let owner = format!("{}-{}", std::process::id(), new_nonce());
        match self.renew_lease(&owner) {
            Ok(()) => {}
            Err(Error::LeaseLost) => return Err(Error::ConsolidationBusy),
            Err(e) => return Err(e),
        }
        let result = self
            .publish_held_leased(gerrit, onto, items, &held, &owner, setting)
            .await;
        if let Err(e) = self.store().end_consolidation_lease(self.server, &owner) {
            log::warn!("ending the consolidation lease: {e}");
        }
        result
    }

    async fn publish_held_leased(
        &self,
        gerrit: ChangeNumber,
        onto: PatchSetNumber,
        items: &[HeldItem],
        held: &HeldItems,
        owner: &str,
        setting: LocalApproval,
    ) -> Result<HeldPublished, Error> {
        let local_change = self.owner_of(items, held)?;
        // Is `onto` current? Asked right before the post.
        let detail = self.refresh_change(gerrit).await?;
        let current = detail.current_patch_set.ok_or(Error::HeldChanged)?;
        if current != onto {
            return Err(Error::NotCurrent {
                asked: onto,
                current,
            });
        }
        let rev = detail.revision(current).ok_or(Error::HeldChanged)?.clone();
        self.ensure_on_server(&rev.sha)?;
        // An approval that may not move is refused, judged from the change
        // as it is now, also if nothing marked it when it was held.
        for item in items {
            if let HeldItem::Vote { review, label } = item
                && let Some(r) = held.reviews.iter().find(|r| r.id == *review)
                && let Some(value) = r.labels.get(label)
                && held_vote_drop_only(&detail, r, label, *value, &rev.sha, setting)
            {
                return Err(Error::DropOnly(label.clone()));
            }
        }
        let mut publish = HeldPublish {
            local_change,
            gerrit,
            to: PatchSetRef {
                number: rev.number,
                sha: rev.sha.clone(),
            },
            votes: Vec::new(),
            messages: Vec::new(),
            comments: Vec::new(),
            at: Timestamp::now(),
        };
        for item in items {
            match item {
                HeldItem::Vote { review, label } => publish.votes.push((*review, label.clone())),
                HeldItem::Message(review) => publish.messages.push(*review),
                HeldItem::Comment(id) => {
                    let d = held
                        .drafts
                        .iter()
                        .find(|d| d.local_id == *id)
                        .ok_or(Error::HeldChanged)?;
                    publish.comments.push((*id, self.quoted(d)?));
                }
            }
        }
        let nonce = new_nonce();
        let by = Claimant {
            owner,
            at: Timestamp::now(),
        };
        let claimed = match self
            .store()
            .claim_held_publish(self.server, &publish, &nonce, &by)
        {
            Ok(Some(post)) => post,
            Ok(None) => return Err(Error::HeldChanged),
            Err(store::Error::DropOnly(label)) => return Err(Error::DropOnly(label)),
            Err(e) => return Err(e.into()),
        };
        let row = self
            .store()
            .local_change(self.server, local_change)?
            .ok_or(Error::HeldChanged)?;
        let review = claimed.review.clone().ok_or(Error::HeldChanged)?;
        let parts = Parts::of(&claimed, rev.number);
        let input = ReviewInput {
            labels: review.labels.clone(),
            comments: parts.comments.iter().map(|d| d.input()).collect(),
            drafts: DraftHandling::Keep,
            tag: Some(self.local_tag(row.id, Some(review.id), &nonce)),
            notify: None,
            ignore_automatic_attention_set_rules: true,
            ..ReviewInput::default()
        };
        let sent_votes: Votes = input.labels.clone().into_iter().collect();
        let mut outcome = PostOutcome::Sent;
        let answer = self
            .post_review_by_deadline(gerrit, &rev.sha, &input)
            .await
            .map_err(stopped)?;
        let settled = match answer {
            Ok(result) => votes_after(&detail, &input, &result, rev.number),
            // Votes never sink comments: the same post without them. A
            // closed change refusing votes (a post-submit vote Gerrit does
            // not take) is the same: the comments still go.
            Err(e) if !input.labels.is_empty() && (is_vote_refusal(&e) || is_closed_answer(&e)) => {
                let failed = Settled::failed(sent_votes.clone(), refusal_text(&e));
                if !input.comments.is_empty() {
                    let bare = ReviewInput {
                        labels: BTreeMap::new(),
                        ..input.clone()
                    };
                    let again = self
                        .post_review_by_deadline(gerrit, &rev.sha, &bare)
                        .await
                        .map_err(stopped)?;
                    if let Err(e) = again {
                        return self.publish_refused(&row, &claimed, &nonce, rev.number, e);
                    }
                }
                Some(failed)
            }
            Err(e) => return self.publish_refused(&row, &claimed, &nonce, rev.number, e),
        };
        let cast: Votes = match &settled {
            Some(s) => {
                outcome = s.outcome.clone();
                match s.state {
                    PendingReviewState::Failed => sent_votes
                        .iter()
                        .filter(|v| !s.keep.contains(v))
                        .cloned()
                        .collect(),
                    _ => Vec::new(),
                }
            }
            None => sent_votes,
        };
        self.settle(&row, &claimed, &nonce, &parts.posted, settled)
            .map_err(stopped)?;
        self.refresh_after_posts(gerrit).await;
        self.store().settle_local_change(self.server, row.id)?;
        Ok(HeldPublished {
            patch_set: rev.number,
            votes: cast,
            comments: parts.comments.len(),
            outcome,
        })
    }

    /// The local change all of `items` belong to, as held now.
    fn owner_of(&self, items: &[HeldItem], held: &HeldItems) -> Result<LocalChangeId, Error> {
        let mut owner = None;
        for item in items {
            let this = match item {
                HeldItem::Vote { review, .. } | HeldItem::Message(review) => held
                    .reviews
                    .iter()
                    .find(|r| r.id == *review)
                    .map(|r| r.local_change),
                HeldItem::Comment(id) => {
                    if !held.drafts.iter().any(|d| d.local_id == *id) {
                        return Err(Error::HeldChanged);
                    }
                    self.store().held_draft_owner(self.server, *id)?
                }
            };
            let this = this.ok_or(Error::HeldChanged)?;
            if owner.is_some_and(|o| o != this) {
                return Err(Error::HeldChanged);
            }
            owner = Some(this);
        }
        owner.ok_or(Error::HeldChanged)
    }

    /// The text a held comment is posted with as a patch-set-level
    /// comment: where it was, then what it said.
    fn quoted(&self, d: &Draft) -> Result<String, Error> {
        let version = if d.patch_set.is_local() {
            match self.store().local_patch_set(self.server, d.patch_set)? {
                Some(v) => format!("local {}", v.ordinal),
                None => "a local version".to_owned(),
            }
        } else {
            format!("PS {}", d.patch_set.0)
        };
        Ok(quote(&version, d))
    }

    /// A publish Gerrit answered with `e`. A refusal gives the claim up:
    /// the new review fails with Gerrit's text and the comments are held
    /// with it, back in the held card (`Refused`); a refused login does the
    /// same and fails. Anything else may have reached the server, so the
    /// claim stays for the next consolidation run's tag check.
    fn publish_refused(
        &self,
        row: &LocalChange,
        post: &LocalPost,
        nonce: &str,
        patch_set: PatchSetNumber,
        e: ApiError,
    ) -> Result<HeldPublished, Error> {
        let refused = matches!(
            e,
            ApiError::BadRequest { .. }
                | ApiError::Forbidden { .. }
                | ApiError::Conflict { .. }
                | ApiError::NotFound
                | ApiError::Unauthorized
        );
        if !refused {
            return Err(e.into());
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
        )
        .map_err(stopped)?;
        if e.is_unauthorized() {
            return Err(e.into());
        }
        Ok(HeldPublished {
            patch_set,
            votes: Vec::new(),
            comments: 0,
            outcome: PostOutcome::Refused { error: text },
        })
    }
}

/// The error of a consolidation step outside a run: the store's, or what
/// ended the post (a step that only writes the store never skips).
fn stopped(p: Problem) -> Error {
    match p {
        Problem::Stop(e) => e,
        Problem::Skip(_) | Problem::Closed => Error::HeldChanged,
    }
}

/// "On local 2, src/a.rs line 40: text": where comment `d`, on `version`,
/// was, then its text.
pub(super) fn quote(version: &str, d: &Draft) -> String {
    format!("On {}: {}", held_place(version, d), d.message)
}

/// Where held comment `d` was, on `version` ("local 2", "PS 3"): "local 2,
/// src/a.rs line 40", "local 2, the commit message", or just "local 2"
/// for a patch-set-level comment. What a published held comment quotes,
/// and what the held card shows.
pub fn held_place(version: &str, d: &Draft) -> String {
    let file = match d.path.as_str() {
        PATCHSET_LEVEL_PATH => None,
        COMMIT_MSG_PATH => Some("the commit message".to_owned()),
        MERGE_LIST_PATH => Some("the merge list".to_owned()),
        path => Some(path.to_owned()),
    };
    let Some(mut place) = file else {
        return version.to_owned();
    };
    match (d.side, d.parent) {
        (Side::Parent, Some(n)) => place.push_str(&format!(" (parent {n})")),
        (Side::Parent, None) => place.push_str(" (base)"),
        (Side::Revision, _) => {}
    }
    match d.location {
        Location::File => {}
        Location::Line(l) => place.push_str(&format!(" line {l}")),
        Location::Range(r) if r.start_line == r.end_line => {
            place.push_str(&format!(" line {}", r.start_line));
        }
        Location::Range(r) => place.push_str(&format!(" lines {}-{}", r.start_line, r.end_line)),
    }
    format!("{version}, {place}")
}
