//! Quick votes: the header bar's buttons that cast the highest permitted
//! value of a label with one key (Code-Review's lowest while the user's
//! drafts object), publishing the drafts with it, and moving on to the next change of the
//! relation chain after a vote (also after a vote in the reply dialog).
//!
//! On a local version (a local change, or a local patch set of a Gerrit
//! change shown as the target) a quick vote is never sent: it goes into
//! the newest `Pending` review of that version, replacing that label's
//! vote and keeping the rest, or into a new pending review with only that
//! vote when there is none. So pressing `Shift-V` twice does not make two
//! reviews, and a vote added after a reply joins it, as it would on
//! Gerrit. The buttons show the votes of that review as the user's, with
//! those carried from an older local version that follow the amended
//! commit to Gerrit (by the configured `local_approval`; a carried vote
//! that would be held is not given yet), and a saved vote moves on along
//! the relation chain as a cast one does.

use gpui_kit::*;
use model::{
    ChangeNumber, PatchSetNumber, PatchSetRef, PendingReview, PendingReviewState, ReviewInput,
    Timestamp,
};
use store::NewPendingReview;
use sync::PublishOutcome;

use super::{ChangeEvent, ChangePage};
use crate::{
    actions::QuickVote,
    change_detail::patch_set_label,
    reply::{
        self,
        plan::{self, QuickButton},
    },
    session::is_unauthorized,
};

impl ChangePage {
    /// The buttons of the header bar.
    pub(super) fn quick_buttons(&self, cx: &App) -> Vec<QuickButton> {
        let objections = plan::drafts_object(&self.comments.read(cx).threads());
        let Some(d) = &self.detail else {
            return Vec::new();
        };
        let me = self.session.username.as_deref();
        match self.local_target() {
            Some(target) => {
                let mut own = self
                    .newest_pending(&target, cx)
                    .map(|r| r.labels)
                    .unwrap_or_default();
                // A vote from an older version that follows the amended
                // commit is given already; one that would be held is not.
                for (label, value) in self.following_carried_votes(cx) {
                    own.entry(label).or_insert(value);
                }
                plan::local_quick_votes(d, me, objections, &own)
            }
            None => plan::quick_votes(d, me, objections),
        }
    }

    /// The local version a reply or vote would be on, if it is one (see
    /// [`ChangePage::on_local_target`]).
    pub(super) fn local_target(&self) -> Option<PatchSetRef> {
        if !self.on_local_target() {
            return None;
        }
        let d = self.detail.as_ref()?;
        self.sel.target_ref(d)
    }

    /// The newest pending review of version `target` that can still change.
    fn newest_pending(&self, target: &PatchSetRef, cx: &App) -> Option<PendingReview> {
        self.comments
            .read(cx)
            .reviews()
            .iter()
            .rev()
            .find(|r| r.patch_set == target.number && r.state == PendingReviewState::Pending)
            .cloned()
    }

    /// Whether a quick vote is on its way to the server.
    pub(super) fn voting(&self) -> bool {
        self.voting.is_some()
    }

    pub(super) fn quick_vote_action(
        &mut self,
        action: &QuickVote,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.quick_vote(action.position, cx);
    }

    /// Casts the vote of the button at `position`.
    pub(super) fn quick_vote(&mut self, position: usize, cx: &mut Context<Self>) {
        if self.voting() {
            return;
        }
        if let Some(why) = reply::unavailable(&self.session) {
            self.hint
                .say(why.replace("reply", "vote"), false, cx, |p| &mut p.hint);
            return;
        }
        let buttons = self.quick_buttons(cx);
        let Some(button) = buttons.get(position) else {
            let why = if buttons.is_empty() {
                "No votes to cast on this change"
            } else {
                "No such vote button"
            };
            self.hint.say(why, false, cx, |p| &mut p.hint);
            return;
        };
        if button.applied {
            let text = format!("You already voted {}", button.text());
            self.hint.say(text, false, cx, |p| &mut p.hint);
            return;
        }
        if let Some(target) = self.local_target() {
            return self.quick_vote_local(button.clone(), target, cx);
        }
        let Some(revision) = self
            .detail
            .as_ref()
            .and_then(|d| d.current())
            .map(|r| r.sha.clone())
        else {
            self.hint
                .say("The change is not loaded yet", false, cx, |p| &mut p.hint);
            return;
        };
        let review = plan::quick_vote_input(button);
        let syncer = self.session.syncer().clone();
        let change = self.number;
        self.hint.say(
            format!("Voting {}\u{2026}", button.text()),
            false,
            cx,
            |p| &mut p.hint,
        );
        self.voting = Some(cx.spawn(async move |this, cx| {
            let sent = review.clone();
            let result = cx
                .background_spawn(
                    async move { syncer.publish_review(change, &revision, &sent).await },
                )
                .await;
            this.update(cx, |this, cx| this.vote_sent(&review, result, cx))
                .ok();
        }));
        cx.notify();
    }

    /// Saves the vote of `button` on local version `target` (see the module
    /// documentation). Store writes only, on the background executor.
    fn quick_vote_local(
        &mut self,
        button: QuickButton,
        target: PatchSetRef,
        cx: &mut Context<Self>,
    ) {
        let newest = self.newest_pending(&target, cx);
        let strategy = self.local_approval(cx);
        let syncer = self.session.syncer().clone();
        // Whether Gerrit could be asked about the change now: an offline
        // vote counts as given before the change was there.
        let offline = self.session.offline();
        let label = self.detail.as_ref().map_or_else(String::new, |d| {
            patch_set_label(&d.revisions, target.number)
        });
        let vote = (button.label.clone(), button.value);
        let saved = vote.clone();
        self.voting = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let vote = saved;
                    // Only the label: the message may have changed since the
                    // list was read. Gone or claimed for sending meanwhile:
                    // a new review.
                    if let Some(r) = newest
                        && let Some(r) = syncer
                            .set_pending_review_label(r.id, &vote.0, vote.1, strategy, offline)?
                    {
                        return Ok(r);
                    }
                    syncer.create_pending_review(&NewPendingReview {
                        patch_set: target.number,
                        labels: [vote].into(),
                        strategy,
                        message: String::new(),
                        unresolved: false,
                        created: Timestamp::now(),
                        offline,
                        replaces_draft: None,
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.voting = None;
                match result {
                    Ok(_) => {
                        let change = this.number;
                        this.outbox.update(cx, |o, cx| o.published(change, cx));
                        cx.emit(ChangeEvent::LocalReviewSaved);
                        let text = format!(
                            "Saved {} locally; sent when {label} is on Gerrit",
                            button.text()
                        );
                        this.after_vote(&[vote], text, cx);
                    }
                    Err(e) => {
                        let text = reply::describe_local(&e);
                        this.hint.say(text, true, cx, |p| &mut p.hint);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn vote_sent(
        &mut self,
        review: &ReviewInput,
        result: Result<PublishOutcome, sync::Error>,
        cx: &mut Context<Self>,
    ) {
        self.voting = None;
        // The drafts were published, or some uploaded on the way: show them
        // as they are now.
        let change = self.number;
        self.outbox.update(cx, |o, cx| o.published(change, cx));
        match result {
            Ok(outcome) => {
                match outcome.comments {
                    Some(view) => self.comments.update(cx, |c, cx| c.apply_view(view, cx)),
                    None => self.comments.update(cx, |c, cx| c.refresh(cx)),
                }
                self.take_detail(outcome.detail, cx);
                let voted = plan::applied_votes(&review.labels, &outcome.unapplied_labels);
                if voted.is_empty() {
                    let dropped = plan::voted_text(&outcome.unapplied_labels).replacen(
                        "Voted",
                        "The server did not apply",
                        1,
                    );
                    self.hint.say(dropped, true, cx, |p| &mut p.hint);
                } else if let Some(w) = outcome.result.error {
                    let text = format!("{}, but: {w}", plan::voted_text(&voted));
                    self.hint.say(text, true, cx, |p| &mut p.hint);
                } else {
                    self.after_vote(&voted, plan::voted_text(&voted), cx);
                }
            }
            Err(e) if is_unauthorized(&e) => cx.emit(ChangeEvent::Unauthorized),
            Err(e) => {
                self.hint.say(describe_error(&e), true, cx, |p| &mut p.hint);
            }
        }
        cx.notify();
    }

    /// A vote (`voted` is not empty) took effect: go on to the child in the
    /// relation chain if the setting says so, else say `note` here.
    pub(super) fn after_vote(
        &mut self,
        voted: &[(String, i32)],
        note: String,
        cx: &mut Context<Self>,
    ) {
        let target = self.advance_target(voted, cx);
        match target {
            Some((n, at)) => {
                let name = self.chain_name(n, at);
                cx.emit(ChangeEvent::Advance(
                    n,
                    at,
                    format!("{note}, moved to {name}"),
                ))
            }
            None => self.hint.say(note, false, cx, |p| &mut p.hint),
        }
    }

    /// Where a vote that cast `voted` leads: nowhere if it changed no vote,
    /// the setting is off, or there is no open child (see
    /// [`super::advance_row`]). The chain may be Gerrit's or the
    /// checkout's, so a vote on a local version moves on as well.
    pub(super) fn advance_target(
        &self,
        voted: &[(String, i32)],
        cx: &App,
    ) -> Option<(ChangeNumber, Option<PatchSetNumber>)> {
        if voted.is_empty() || !self.layout.read(cx).advance_after_vote() {
            return None;
        }
        super::advance_row(&self.related.as_ref()?.rows)
    }

    /// How the note names the change a vote moved on to: `change 123`, or
    /// as its chain row does (`local`, `#7 local 2`), since a local
    /// change's number is synthetic.
    fn chain_name(&self, n: ChangeNumber, at: Option<PatchSetNumber>) -> String {
        if !n.is_local() && at.is_none_or(|ps| !ps.is_local()) {
            return format!("change {n}");
        }
        self.related
            .as_ref()
            .and_then(|r| {
                r.rows.iter().find(|row| match row.open {
                    Some(super::ChainOpen::Change(c)) => c == n && at.is_none(),
                    Some(super::ChainOpen::At(c, ps)) => c == n && at == Some(ps),
                    _ => false,
                })
            })
            .map_or_else(
                || "the next change".to_owned(),
                |row| match n.is_local() {
                    true => format!("local change \u{201c}{}\u{201d}", row.subject),
                    false => row.label.clone(),
                },
            )
    }
}

fn describe_error(e: &sync::Error) -> String {
    use gerrit_api::Error as Api;
    let refused = |what: &str, message: &str| {
        if message.trim().is_empty() {
            format!("Vote not sent: the server says it is {what}.")
        } else {
            format!("Vote not sent: {what}: {}", message.trim())
        }
    };
    match e {
        sync::Error::Api(api) => match api {
            Api::Forbidden { message } => refused("not allowed (403)", message),
            Api::Conflict { message } => refused("in conflict (409)", message),
            Api::BadRequest { message } => refused("rejected (400)", message),
            api if api.is_retryable() => {
                format!("Vote not sent: the server cannot be reached ({api}).")
            }
            api => format!("Vote not sent: {api}"),
        },
        other => format!("Vote not sent: {other}"),
    }
}
