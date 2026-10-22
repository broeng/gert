//! Quick votes: the header bar's buttons that cast the highest permitted
//! value of a label with one key, and moving on to the next change of the
//! relation chain after a vote (also after a vote in the reply dialog).

use gpui_kit::*;
use model::{ChangeNumber, ReviewInput};
use sync::PublishOutcome;

use super::{ChangeEvent, ChangePage};
use crate::{
    actions::QuickVote,
    reply::{
        self,
        plan::{self, QuickButton},
    },
    session::is_unauthorized,
};

impl ChangePage {
    /// The buttons of the header bar.
    pub(super) fn quick_buttons(&self) -> Vec<QuickButton> {
        self.detail.as_ref().map_or_else(Vec::new, |d| {
            plan::quick_votes(d, self.session.username.as_deref())
        })
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
        let buttons = self.quick_buttons();
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

    fn vote_sent(
        &mut self,
        review: &ReviewInput,
        result: Result<PublishOutcome, sync::Error>,
        cx: &mut Context<Self>,
    ) {
        self.voting = None;
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
            Some(n) => cx.emit(ChangeEvent::Advance(
                n,
                format!("{note}, moved to change {n}"),
            )),
            None => self.hint.say(note, false, cx, |p| &mut p.hint),
        }
    }

    /// Where a vote that cast `voted` leads: nowhere if it changed no vote,
    /// the setting is off, or there is no open child.
    pub(super) fn advance_target(&self, voted: &[(String, i32)], cx: &App) -> Option<ChangeNumber> {
        if voted.is_empty() || !self.layout.read(cx).advance_after_vote() {
            return None;
        }
        let chain = &self.related.as_ref()?.changes;
        plan::advance_target(chain, self.number)
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
