//! Recovery: posts whose answer was lost, found again by their tag, and
//! changes found on Gerrit settled.

use super::*;

impl Syncer {
    /// Settles a post claimed under `nonce` by a run that is gone: takes
    /// it over, then finishes it if the change has a message by the
    /// signed-in account with its tag, else posts it again under the same
    /// nonce.
    pub(super) async fn recover(
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
    pub(super) async fn settle_found(
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
