//! The approval rules: which votes of a review are cast, held to publish
//! by hand or held drop-only, and the reasons the held card shows.

use super::*;

impl Syncer {
    /// For posts whose answer did not say the current patch set: if the
    /// refreshed change shows a newer patch set uploaded before our
    /// message, the votes landed on the old one. Those reviews are held
    /// then, approvals drop-only.
    pub(super) fn verify_votes(
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
}

/// What happened to the votes of a post that the server took: `None` when
/// all applied. Votes that arrived after their patch set stopped being
/// current were cast there, where they do not count: held, approvals
/// drop-only. Votes missing from the answer were dropped (an unknown label,
/// a value the label does not have: Gerrit answers 200 for those): failed.
pub(super) fn votes_after(
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
pub(super) fn landed_reason(
    ps: PatchSetNumber,
    current: PatchSetNumber,
    votes: &[(String, i32)],
) -> String {
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
pub(super) fn drop_only_now(
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
pub(super) fn drop_only_labels(
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
pub(super) fn strategy(
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
pub(super) fn casts(
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
pub(super) fn votes_held_reason(
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
pub(super) fn vote_actions(votes: &[(String, i32)], drop_only: &BTreeSet<String>) -> String {
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
pub(super) fn held_reason(
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
pub(super) fn is_approval(detail: &ChangeDetail, label: &str, value: i32) -> bool {
    detail
        .label_values
        .get(label)
        .and_then(|values| values.iter().map(|v| v.value).max())
        .is_some_and(|max| max > 0 && value >= max)
}

/// "Code-Review +2, Verified -1".
pub(super) fn describe_votes(votes: &[(String, i32)]) -> String {
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
pub(super) fn votes_in_message(message: &str) -> BTreeMap<String, i32> {
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
