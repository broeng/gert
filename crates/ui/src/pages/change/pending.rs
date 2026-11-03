//! The change page's list of pending reviews: the reviews given on the
//! change's local versions (from the reply dialog and quick votes) that
//! are not on Gerrit yet, for a local change and for a Gerrit change with
//! local patch sets.
//!
//! One compact card under the header, only while there is a review that
//! is not sent. Each row names the version, the votes, the start of the
//! message and the state (pending, sending, held or failed with the
//! reason), with the actions the state allows: edit (reopens the reply
//! dialog on it) and cancel for a pending review, drop for a held or
//! failed one. Dropping discards the message too, so it asks once: the
//! first press arms it, the second drops.
//!
//! Keys: `l n` / `l p` move the selection (shown when there are several),
//! `l e` edits, `l c` cancels and `l d` drops the selected review. The
//! list is the shared [`crate::comments::ChangeComments`]' reviews, read
//! from the store with the threads and reloaded whenever the outbox says
//! the change moved, so it is there at once and follows every write.

use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use model::{PendingReview, PendingReviewId, PendingReviewState};

use super::{
    ChangePage,
    view::{pill, section_title},
};
use crate::{actions::*, change_detail::patch_set_label, reply};

/// How long a first "drop" waits for the second.
const DROP_ASK: std::time::Duration = std::time::Duration::from_secs(5);

/// `Code-Review +2, Verified +1`, or "no votes".
pub fn votes_text(r: &PendingReview) -> String {
    if r.labels.is_empty() {
        return "no votes".to_owned();
    }
    r.labels
        .iter()
        .map(|(l, v)| format!("{l} {}", reply::plan::vote_text(*v)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The state as the row says it.
pub fn state_text(r: &PendingReview) -> String {
    let with = |what: &str, why: &Option<String>| match why.as_deref().map(str::trim) {
        Some(w) if !w.is_empty() => format!("{what}: {w}"),
        _ => what.to_owned(),
    };
    match r.state {
        PendingReviewState::Pending => "pending".to_owned(),
        PendingReviewState::Sending => "sending\u{2026}".to_owned(),
        PendingReviewState::Held => with("held", &r.held_reason),
        PendingReviewState::Failed => with("failed", &r.error),
        PendingReviewState::Sent => "sent".to_owned(),
    }
}

/// The first non-blank line of the message, if it has one.
fn message_start(r: &PendingReview) -> Option<String> {
    r.message
        .as_deref()?
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_owned())
}

impl ChangePage {
    /// The reviews the card lists: those not sent yet, oldest first.
    pub(super) fn listed_reviews(&self) -> Vec<&PendingReview> {
        self.reviews.iter().filter(|r| r.is_unsent()).collect()
    }

    /// The selected review: the one under the selection, else the newest.
    fn selected_review(&self) -> Option<PendingReview> {
        let listed = self.listed_reviews();
        self.review_cursor
            .and_then(|id| listed.iter().find(|r| r.id == id))
            .or(listed.last())
            .map(|r| (*r).clone())
    }

    fn step_review(&mut self, forward: bool, cx: &mut Context<Self>) {
        let ids: Vec<PendingReviewId> = self.listed_reviews().iter().map(|r| r.id).collect();
        if ids.is_empty() {
            return self.no_reviews(cx);
        }
        let at = self
            .selected_review()
            .and_then(|r| ids.iter().position(|id| *id == r.id))
            .unwrap_or(ids.len() - 1);
        let next = if forward {
            (at + 1) % ids.len()
        } else {
            (at + ids.len() - 1) % ids.len()
        };
        self.review_cursor = Some(ids[next]);
        self.disarm_drop(cx);
        self.remeasure(super::state::Block::PendingReviews);
        cx.notify();
    }

    fn no_reviews(&mut self, cx: &mut Context<Self>) {
        self.hint
            .say("No local reviews waiting here", false, cx, |p| &mut p.hint);
    }

    pub(super) fn next_review(
        &mut self,
        _: &NextPendingReview,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_review(true, cx);
    }

    pub(super) fn prev_review(
        &mut self,
        _: &PrevPendingReview,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_review(false, cx);
    }

    pub(super) fn edit_review_action(
        &mut self,
        _: &EditPendingReview,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_review() {
            Some(r) => self.edit_review(&r, cx),
            None => self.no_reviews(cx),
        }
    }

    pub(super) fn cancel_review_action(
        &mut self,
        _: &CancelPendingReview,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_review() {
            Some(r) => self.cancel_review(&r, cx),
            None => self.no_reviews(cx),
        }
    }

    pub(super) fn drop_review_action(
        &mut self,
        _: &DropPendingReview,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.selected_review() {
            Some(r) => self.drop_review(&r, cx),
            None => self.no_reviews(cx),
        }
    }

    /// Reopens the reply dialog on `r`, if it can still change.
    fn edit_review(&mut self, r: &PendingReview, cx: &mut Context<Self>) {
        if r.state != PendingReviewState::Pending {
            let why = "Only a pending review can be edited; drop it and write a new one";
            return self.hint.say(why, false, cx, |p| &mut p.hint);
        }
        self.open_reply(Some(r.id), cx);
    }

    /// Withdraws pending review `r`; its message stays as a draft.
    fn cancel_review(&mut self, r: &PendingReview, cx: &mut Context<Self>) {
        if r.state != PendingReviewState::Pending {
            let why = match r.state {
                PendingReviewState::Held | PendingReviewState::Failed => {
                    "This review is held; drop it instead (l d)"
                }
                _ => "This review is being sent and can no longer be cancelled",
            };
            return self.hint.say(why, false, cx, |p| &mut p.hint);
        }
        let (syncer, id) = (self.session.syncer().clone(), r.id);
        let had_message = r.message.is_some();
        self.review_write(
            async move { syncer.cancel_pending_review(id) },
            if had_message {
                "Review cancelled; its message is kept as a draft"
            } else {
                "Review cancelled"
            },
            cx,
        );
    }

    /// Drops held or failed review `r` with its message, on the second ask.
    fn drop_review(&mut self, r: &PendingReview, cx: &mut Context<Self>) {
        if !matches!(
            r.state,
            PendingReviewState::Held | PendingReviewState::Failed
        ) {
            let why = match r.state {
                PendingReviewState::Pending => {
                    "This review is still pending; cancel it instead (l c)"
                }
                _ => "This review is being sent and can no longer be dropped",
            };
            return self.hint.say(why, false, cx, |p| &mut p.hint);
        }
        if self.confirm_drop != Some(r.id) {
            self.arm_drop(r.id, cx);
            let ask = "Drop this review and its message? Press l d (or Drop) again";
            return self.hint.say(ask, false, cx, |p| &mut p.hint);
        }
        self.disarm_drop(cx);
        let (syncer, id) = (self.session.syncer().clone(), r.id);
        self.review_write(
            async move { syncer.drop_pending_review(id) },
            "Review dropped",
            cx,
        );
    }

    /// Asks about dropping review `id`; the question expires after
    /// [`DROP_ASK`], so a later `l d` asks again.
    fn arm_drop(&mut self, id: PendingReviewId, cx: &mut Context<Self>) {
        self.confirm_drop = Some(id);
        self.remeasure(super::state::Block::PendingReviews);
        self.drop_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DROP_ASK).await;
            this.update(cx, |this, cx| {
                if this.confirm_drop == Some(id) {
                    this.disarm_drop(cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn disarm_drop(&mut self, cx: &mut Context<Self>) {
        self.confirm_drop = None;
        self.drop_timer = None;
        self.remeasure(super::state::Block::PendingReviews);
        cx.notify();
    }

    /// Runs a pending review write on the background executor, then says
    /// `done` (or why nothing changed) and has the shared comments read the
    /// list again. A write asked for while one runs is ignored, like a
    /// second quick vote: replacing the task would cancel the first.
    fn review_write(
        &mut self,
        write: impl std::future::Future<Output = Result<bool, sync::Error>> + Send + 'static,
        done: &'static str,
        cx: &mut Context<Self>,
    ) {
        if self.reviews_task.is_some() {
            return;
        }
        self.reviews_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(write).await;
            this.update(cx, |this, cx| {
                this.reviews_task = None;
                let (text, error) = match result {
                    Ok(true) => (done.to_owned(), false),
                    Ok(false) => (
                        "The review changed meanwhile; nothing was done".to_owned(),
                        false,
                    ),
                    Err(e) => (format!("Could not change the review: {e}"), true),
                };
                let change = this.number;
                this.outbox.update(cx, |o, cx| o.published(change, cx));
                this.hint.say(text, error, cx, |p| &mut p.hint);
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- rendering ----

    pub(super) fn pending_reviews_card(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let listed: Vec<PendingReview> = self.listed_reviews().into_iter().cloned().collect();
        let selected = (listed.len() > 1)
            .then(|| self.selected_review().map(|r| r.id))
            .flatten();
        let revisions = self.detail.as_ref().map_or(&[][..], |d| &d.revisions[..]);
        let rows: Vec<AnyElement> = listed
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (state_color, can_edit, can_drop) = match r.state {
                    PendingReviewState::Pending => (theme.primary, true, false),
                    PendingReviewState::Held => (theme.warning, false, true),
                    PendingReviewState::Failed => (theme.danger, false, true),
                    _ => (muted, false, false),
                };
                let state = state_text(r);
                let id = r.id;
                let arm = self.confirm_drop == Some(id);
                h_flex()
                    .id(("pending-review", i))
                    .debug_selector(move || format!("pending-review-{i}: {state}"))
                    .gap_2()
                    .px_1()
                    .py_0p5()
                    .items_center()
                    .text_sm()
                    .border_l_2()
                    .border_color(if selected == Some(id) {
                        theme.list_active_border
                    } else {
                        theme.transparent
                    })
                    .when(selected == Some(id), |d| d.bg(theme.list_active))
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .truncate()
                            .text_xs()
                            .text_color(muted)
                            .child(patch_set_label(revisions, r.patch_set)),
                    )
                    .child(div().flex_none().child(votes_text(r)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(muted)
                            .children(message_start(r)),
                    )
                    .child(pill(state_text(r), state_color))
                    .when(can_edit, |d| {
                        d.child(
                            Button::new(("pending-edit", i))
                                .label("Edit")
                                .xsmall()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(r) = this.review_by_id(id) {
                                        this.edit_review(&r, cx);
                                    }
                                })),
                        )
                        .child(
                            Button::new(("pending-cancel", i))
                                .label("Cancel")
                                .xsmall()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(r) = this.review_by_id(id) {
                                        this.cancel_review(&r, cx);
                                    }
                                })),
                        )
                    })
                    .when(can_drop, |d| {
                        d.child(
                            Button::new(("pending-drop", i))
                                .label(if arm { "Drop?" } else { "Drop" })
                                .xsmall()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(r) = this.review_by_id(id) {
                                        this.drop_review(&r, cx);
                                    }
                                })),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let keys = if listed.len() > 1 {
            "l n / l p select \u{b7} l e edit \u{b7} l c cancel \u{b7} l d drop"
        } else {
            "l e edit \u{b7} l c cancel \u{b7} l d drop"
        };
        v_flex()
            .child(
                super::view::card_title_bar(cx)
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(section_title("Local reviews", cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child("sent automatically when their version is on Gerrit"),
                    )
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(muted).child(keys)),
            )
            .child(v_flex().px_2().py_1().gap_0p5().children(rows))
            .into_any_element()
    }

    fn review_by_id(&self, id: PendingReviewId) -> Option<PendingReview> {
        self.reviews.iter().find(|r| r.id == id).cloned()
    }

    #[cfg(test)]
    pub(crate) fn listed_reviews_for_test(&self) -> Vec<PendingReview> {
        self.listed_reviews().into_iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use model::{
        LocalApproval, LocalChangeId, PatchSetNumber, PendingReview, PendingReviewId,
        PendingReviewState, RevisionId, Timestamp,
    };

    use super::{message_start, state_text, votes_text};

    fn review(state: PendingReviewState) -> PendingReview {
        PendingReview {
            id: PendingReviewId(1),
            local_change: LocalChangeId(1),
            change: model::ChangeNumber(7),
            patch_set: PatchSetNumber(PatchSetNumber::LOCAL_BASE + 1),
            revision: RevisionId("l1".into()),
            given_on_sha: RevisionId("l1".into()),
            labels: BTreeMap::new(),
            strategy: LocalApproval::Exact,
            state,
            error: None,
            held_reason: None,
            created: Timestamp::UNIX_EPOCH,
            voted_at: Timestamp::UNIX_EPOCH,
            nonce: None,
            message: None,
            unresolved: false,
            message_draft: None,
        }
    }

    #[test]
    fn rows_say_votes_message_and_state() {
        let mut r = review(PendingReviewState::Pending);
        assert_eq!(votes_text(&r), "no votes");
        r.labels = [("Code-Review".to_owned(), 2), ("Verified".to_owned(), -1)].into();
        assert_eq!(votes_text(&r), "Code-Review +2, Verified -1");
        assert_eq!(message_start(&r), None);
        r.message = Some("\n  First line  \nsecond".into());
        assert_eq!(message_start(&r).as_deref(), Some("First line"));
        assert_eq!(state_text(&r), "pending");
        r.state = PendingReviewState::Held;
        r.held_reason = Some("amended before upload".into());
        assert_eq!(state_text(&r), "held: amended before upload");
        r.state = PendingReviewState::Failed;
        assert_eq!(state_text(&r), "failed");
        r.error = Some("403 not allowed".into());
        assert_eq!(state_text(&r), "failed: 403 not allowed");
        r.state = PendingReviewState::Sending;
        assert_eq!(state_text(&r), "sending\u{2026}");
    }
}
