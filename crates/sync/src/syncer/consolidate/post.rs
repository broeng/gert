//! Sending a local review: one post per review, claimed first and tagged
//! so that a lost answer is recovered instead of resent, and what a
//! refusal or a timeout leaves.

use super::*;

impl Syncer {
    /// Sends the moved pending reviews of `row` and its reserved comments,
    /// per revision.
    pub(super) async fn send(
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
    pub(super) fn mark_drop_only(
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
    pub(super) async fn post_claimed(
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
    pub(super) async fn post_review_by_deadline(
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
    pub(super) fn give_up(
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
    pub(super) fn settle(
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
    pub(super) fn refused(
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
    pub(super) fn local_tag(
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
pub(super) fn tagged_by<'a>(
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
pub(super) struct Parts<'a> {
    /// The inline comments: the reserved comments on the revision that
    /// are not held, and the review's message.
    pub(super) comments: Vec<&'a Draft>,
    /// `(id, version)` of each, for `finish_local_post`.
    pub(super) posted: Vec<(LocalDraftId, u32)>,
}

impl<'a> Parts<'a> {
    pub(super) fn of(post: &'a LocalPost, ps: PatchSetNumber) -> Self {
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
pub(super) fn postable(d: &Draft) -> bool {
    d.pending_review.is_none()
        && d.held_reason.is_none()
        && d.is_visible()
        && !matches!(d.sync, DraftSync::Failed { .. })
}

/// Whether a draft on a local version moves with it: not a review's
/// message, not deleted, not held.
pub(super) fn movable(d: &Draft) -> bool {
    d.is_visible() && !d.is_sealed() && d.reserved_by.is_none() && d.held_reason.is_none()
}

/// The text of a failure that skips what it is about, or the failure if
/// it ends the run.
pub(super) fn skip_or_stop(e: Error) -> Result<String, Error> {
    match Problem::from(e) {
        Problem::Stop(e) => Err(e),
        Problem::Skip(why) => Ok(why),
        Problem::Closed => Ok("the change is closed".to_owned()),
    }
}

/// Whether the server refused a post because of its votes: a 400, 403 or
/// 409 that names a label or a vote. Seen on the dev Gerrit (3.14):
/// 403 `Applying label "Code-Review": 2 is restricted`. Gerrit accepts votes
/// on a patch set that is not current, so no refusal says that (it is
/// caught from the answer's current patch set instead, see
/// [`votes_after`]); "not current" is still read as a vote problem, should
/// a server say it. A closed change's answer is not a vote problem (see
/// [`is_closed_answer`]).
pub(super) fn is_vote_refusal(e: &ApiError) -> bool {
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
pub(super) fn is_closed_answer(e: &ApiError) -> bool {
    matches!(e, ApiError::Conflict { message } if message.to_lowercase().contains("closed"))
}

/// The server's text of a refusal, else the error.
pub(super) fn refusal_text(e: &ApiError) -> String {
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
pub(super) fn foreign_reason(r: &PendingReview) -> String {
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

/// A fresh nonce: unique per process run and over runs (the time is part
/// of it), which is all a tag or an owner token needs.
pub(super) fn new_nonce() -> String {
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
